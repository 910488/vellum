import { describe, expect, it } from "vitest";
import {
  buildActivityHeatmap,
  buildHeatmap,
  streaks,
  sumRecentDays,
  toColumns,
} from "@/lib/heatmap";
import type { RequestLogEntry } from "@/types";

const DAY = 86_400;

function at(daysAgo: number, patch: Partial<RequestLogEntry> = {}): RequestLogEntry {
  /* 固定在當天中午，避免測試在接近午夜時因為跨日而飄 */
  const noon = new Date();
  noon.setHours(12, 0, 0, 0);
  return {
    id: Math.random(),
    routeId: "grok-cli",
    provider: "Grok Build",
    model: "grok-4.5",
    inputTokens: 1_000,
    outputTokens: 200,
    status: 200,
    error: null,
    durationMs: 800,
    firstByteMs: 200,
    createdAt: Math.floor(noon.getTime() / 1000) - daysAgo * DAY,
    ...patch,
  };
}

describe("buildHeatmap", () => {
  it("Codex 官方 lifetime total 不會被可見 buckets 截斷", () => {
    const heat = buildActivityHeatmap(
      [
        { date: "2026-07-19", tokens: 324_455_603, requests: 0 },
        { date: "2026-07-20", tokens: 23_619_995, requests: 2 },
      ],
      4_085_209_723,
      365,
      true,
    );
    expect(heat.totalInputTokens).toBe(4_085_209_723);
    expect(heat.maxDayTokens).toBe(324_455_603);
    expect(
      heat.days.find((day) => day.date === "2026-07-20")?.inputTokens,
    ).toBe(23_619_995);
  });

  it("沒有紀錄時回空，不是一堆 0 的格子", () => {
    const heat = buildHeatmap([]);
    expect(heat.days).toEqual([]);
    expect(heat.maxDayTokens).toBe(0);
  });

  it("同一天的多筆會合併", () => {
    const heat = buildHeatmap([at(0), at(0), at(0)]);
    const today = heat.days[heat.days.length - 1];
    expect(today?.requests).toBe(3);
    expect(today?.inputTokens).toBe(3_000);
    expect(today?.outputTokens).toBe(600);
  });

  it("中間沒有用量的日子要補成 0 的格子 —— 那是熱區圖唯一比折線強的地方", () => {
    const heat = buildHeatmap([at(4), at(0)]);
    /* 第 4 天前到今天 = 5 格，中間三天必須存在且為 0 */
    expect(heat.days).toHaveLength(5);
    expect(heat.days.map((d) => d.requests)).toEqual([1, 0, 0, 0, 1]);
  });

  it("maxDayTokens 取單日最大值，色階才有正確的分母", () => {
    const heat = buildHeatmap([
      at(1),
      at(0, { inputTokens: 50_000, outputTokens: 0 }),
      at(0, { inputTokens: 50_000, outputTokens: 0 }),
    ]);
    expect(heat.maxDayTokens).toBe(100_000);
  });

  it("失敗的請求要分開計數", () => {
    const heat = buildHeatmap([at(0, { status: 500 }), at(0)]);
    const today = heat.days[heat.days.length - 1];
    expect(today?.requests).toBe(2);
    expect(today?.failedRequests).toBe(1);
  });

  it("超過上限就截斷，不會為了一筆很舊的紀錄畫出幾千格", () => {
    const heat = buildHeatmap([at(900), at(0)], 30);
    expect(heat.days).toHaveLength(30);
  });

  it("年度 HeatMap 可固定補滿 365 天，維持十二個月的時間軸", () => {
    const heat = buildHeatmap([at(0)], 365, true);
    expect(heat.days).toHaveLength(365);
    expect(toColumns(heat, "daily")).toHaveLength(53);
  });

  it("累計是所有紀錄的總和，不受日曆區間截斷影響", () => {
    const heat = buildHeatmap([at(900), at(0)], 30);
    expect(heat.totalRequests).toBe(2);
    expect(heat.totalInputTokens).toBe(2_000);
  });
});

describe("sumRecentDays", () => {
  it("只加最近 n 天", () => {
    const heat = buildHeatmap([at(0), at(1), at(9)]);
    /* 每筆 1200 token；最近 2 天有兩筆 */
    expect(sumRecentDays(heat, 2)).toBe(2_400);
  });

  it("n 大於現有天數時就是全部", () => {
    const heat = buildHeatmap([at(0), at(1)]);
    expect(sumRecentDays(heat, 999)).toBe(2_400);
  });
});

describe("toColumns", () => {
  const heat = buildHeatmap([at(20), at(10), at(3), at(0)]);

  it("每一欄都補滿 7 格 —— 否則最後那個未完成的週沒辦法由下往上填", () => {
    for (const mode of ["daily", "weekly", "cumulative"] as const) {
      for (const col of toColumns(heat, mode)) {
        expect(col.cells).toHaveLength(7);
      }
    }
  });

  it("累計模式必須單調遞增 —— 累積量不可能變少", () => {
    const filled = toColumns(heat, "cumulative").map(
      (c) => c.cells.filter((cell) => cell.level > 0).length,
    );
    for (let i = 1; i < filled.length; i += 1) {
      expect(filled[i]).toBeGreaterThanOrEqual(filled[i - 1] ?? 0);
    }
  });

  it("柱狀模式由下往上填，不是由上往下", () => {
    const col = toColumns(heat, "weekly").find((c) => c.total > 0);
    const levels = col?.cells.map((c) => c.level) ?? [];
    /* 有色的必須都在尾端 */
    const firstOn = levels.findIndex((l) => l > 0);
    expect(firstOn).toBeGreaterThanOrEqual(0);
    expect(levels.slice(firstOn).every((l) => l > 0)).toBe(true);
  });

  it("有用量就至少填一格 —— 有量卻完全看不到比不精確更糟", () => {
    const tiny = buildHeatmap([
      at(20, { inputTokens: 1_000_000, outputTokens: 0 }),
      at(0, { inputTokens: 1, outputTokens: 0 }),
    ]);
    const last = toColumns(tiny, "weekly").at(-1);
    expect(last?.cells.filter((c) => c.level > 0).length).toBeGreaterThanOrEqual(1);
  });

  it("月份標籤只在該月第一欄出現", () => {
    const labels = toColumns(heat, "daily").map((c) => c.monthLabel).filter(Boolean);
    expect(new Set(labels).size).toBe(labels.length);
  });
});

describe("streaks", () => {
  it("中間斷掉就重算，current 從最後一天往回數", () => {
    const heat = buildHeatmap([at(5), at(4), at(2), at(1), at(0)]);
    expect(streaks(heat).longest).toBe(3);
    expect(streaks(heat).current).toBe(3);
  });

  it("今天沒用就是 0", () => {
    const heat = buildHeatmap([at(3), at(2)]);
    expect(streaks(heat).current).toBe(0);
  });
});
