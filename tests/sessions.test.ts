import { describe, expect, it } from "vitest";
import {
  IDLE_AFTER_SECONDS,
  isLive,
  matchesSessionQuery,
  orderByRecency,
  orderSessions,
  sessionLabel,
  sessionPercent,
  tightestSession,
} from "@/lib/sessions";
import type { SessionStatus } from "@/types";

const NOW = 1_800_000_000;

function s(patch: Partial<SessionStatus> = {}): SessionStatus {
  return {
    id: "abcdef0123456789",
    label: null,
    routeId: "grok-cli",
    provider: "Grok Build",
    model: "grok-4.5",
    usedTokens: 100_000,
    windowTokens: 500_000,
    compactThresholdPercent: 80,
    lastActivityAt: NOW - 60,
    ...patch,
  };
}

describe("sessionPercent", () => {
  it("視窗為 0 時回 0，不是 NaN 或 Infinity", () => {
    expect(sessionPercent(s({ windowTokens: 0 }))).toBe(0);
  });

  it("超過視窗時夾在 100", () => {
    expect(sessionPercent(s({ usedTokens: 900_000 }))).toBe(100);
  });
});

describe("isLive", () => {
  it("剛剛有動靜就是活躍", () => {
    expect(isLive(s({ lastActivityAt: NOW - 30 }), NOW)).toBe(true);
  });

  it("超過閒置門檻就不算活躍 —— 但不代表要隱藏它", () => {
    expect(isLive(s({ lastActivityAt: NOW - IDLE_AFTER_SECONDS - 1 }), NOW)).toBe(false);
  });
});

describe("tightestSession", () => {
  it("取距離門檻最近的，不是用量最高的", () => {
    /* 小視窗 70%（距門檻 10）比大視窗 75%（距門檻 5）用量高，
       但後者會先被壓縮 —— 這正是不能只看用量的原因 */
    const small = s({ id: "small", usedTokens: 70, windowTokens: 100 });
    const big = s({ id: "big", usedTokens: 750, windowTokens: 1_000 });
    expect(tightestSession([small, big])?.id).toBe("big");
  });

  it("沒有工作階段時回 null，不要編一個出來", () => {
    expect(tightestSession([])).toBeNull();
  });
});

describe("orderSessions", () => {
  it("活躍的排前面，閒置的排後面（不隱藏）", () => {
    const idle = s({ id: "idle", lastActivityAt: NOW - 9_999, usedTokens: 490_000 });
    const live = s({ id: "live", lastActivityAt: NOW - 10, usedTokens: 10_000 });
    const order = orderSessions([idle, live], NOW).map((x) => x.id);
    expect(order).toEqual(["live", "idle"]);
    expect(order).toHaveLength(2);
  });

  it("同樣活躍時，用量高的排前面", () => {
    const a = s({ id: "a", usedTokens: 10_000 });
    const b = s({ id: "b", usedTokens: 400_000 });
    expect(orderSessions([a, b], NOW).map((x) => x.id)).toEqual(["b", "a"]);
  });
});

describe("sessionLabel", () => {
  it("沒有 label 時用呼叫端提供的未命名文字，不顯示內部 id", () => {
    expect(sessionLabel(s(), "未命名工作階段")).toBe("未命名工作階段");
  });

  it("有 label 就用 label", () => {
    expect(sessionLabel(s({ label: "vellum" }), "未命名工作階段")).toBe("vellum");
  });

  it("空白的 label 當成沒有", () => {
    expect(sessionLabel(s({ label: "   " }), "未命名工作階段")).toBe("未命名工作階段");
  });

  it("長聊天室名稱截短並保留省略號，讓各列對齊", () => {
    expect(
      sessionLabel(
        s({ label: "幫我仔細調查 Vellum 工作階段監看為甚麼失效" }),
        "未命名工作階段",
      ),
    ).toBe("幫我仔細調查 Vellum 工作…");
  });
});

describe("matchesSessionQuery", () => {
  it("空字串放行所有工作階段", () => {
    expect(matchesSessionQuery(s(), "")).toBe(true);
    expect(matchesSessionQuery(s(), "   ")).toBe(true);
  });

  it("比對標題，不分大小寫", () => {
    const named = s({ label: "Vellum Proxy 重構" });
    expect(matchesSessionQuery(named, "vellum")).toBe(true);
    expect(matchesSessionQuery(named, "重構")).toBe(true);
    expect(matchesSessionQuery(named, "remote")).toBe(false);
  });

  it("不比對 Provider 與模型 —— 打 grok 應該找的是你命名過的那個對話", () => {
    const named = s({ label: "夜間跑批" });
    expect(named.provider).toBe("Grok Build");
    expect(named.model).toBe("grok-4.5");
    expect(matchesSessionQuery(named, "grok")).toBe(false);
  });

  it("沒有標題的對話不會洩漏 id 到顯示名稱或搜尋", () => {
    expect(sessionLabel(s(), "未命名工作階段")).toBe("未命名工作階段");
    expect(matchesSessionQuery(s(), "abcdef")).toBe(false);
  });

  it("空白切開後每一段都要命中", () => {
    const named = s({ label: "Vellum Proxy 重構" });
    expect(matchesSessionQuery(named, "vellum 重構")).toBe(true);
    expect(matchesSessionQuery(named, "vellum remote")).toBe(false);
  });

  it("比對完整的標題，不是畫面上截短過的那個", () => {
    const long = s({ label: "幫我仔細調查 Vellum 工作階段監看為甚麼失效" });
    expect(sessionLabel(long, "未命名工作階段")).not.toContain("失效");
    expect(matchesSessionQuery(long, "失效")).toBe(true);
  });
});

/* 實機的 id 長這樣。它是內部識別，不應成為使用者可見名稱或搜尋內容。 */
const THREAD = "01a079b3-33f3-7bb3-a4c9-e60261a5267d";
const LIVE_ID = `codex:${THREAD}:${THREAD}`;

describe("orderByRecency", () => {
  it("剛動過的排最前面", () => {
    const ordered = orderByRecency([
      s({ id: "old", lastActivityAt: NOW - 9_000 }),
      s({ id: "new", lastActivityAt: NOW - 30 }),
      s({ id: "mid", lastActivityAt: NOW - 600 }),
    ]);
    expect(ordered.map((session) => session.id)).toEqual(["new", "mid", "old"]);
  });

  it("不改動傳進來的陣列", () => {
    const input = [s({ id: "a", lastActivityAt: 1 }), s({ id: "b", lastActivityAt: 2 })];
    orderByRecency(input);
    expect(input.map((session) => session.id)).toEqual(["a", "b"]);
  });
});

describe("matchesSessionQuery 與實機的 id", () => {
  it("打 codex 不會命中每一個對話", () => {
    /* 每個對話的 id 都以 `codex:` 開頭。整串丟進去比對的話，這一個查詢會
       把清單原封不動地還給你 —— 那不是搜尋。 */
    expect(matchesSessionQuery(s({ id: LIVE_ID }), "codex")).toBe(false);
  });

  it("沒有標題時不顯示或搜尋內部 id", () => {
    const untitled = s({ id: LIVE_ID });
    expect(sessionLabel(untitled, "Untitled session")).toBe("Untitled session");
    expect(matchesSessionQuery(untitled, "codex")).toBe(false);
    expect(matchesSessionQuery(untitled, "01a079")).toBe(false);
  });
});
