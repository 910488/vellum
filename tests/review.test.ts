import { describe, expect, it } from "vitest";
import {
  REVIEW_POLICIES,
  fallbackShare,
  needsFallback,
  policyOf,
  statsToShow,
} from "@/lib/review";
import type { ReviewSettings, ReviewStats } from "@/types";

function settings(patch: Partial<ReviewSettings> = {}): ReviewSettings {
  return {
    onEdit: false,
    beforeSend: true,
    beforeCompact: true,
    routeId: "",
    model: "",
    ...patch,
  };
}

function stats(patch: Partial<ReviewStats> = {}): ReviewStats {
  return {
    totalRuns: 0,
    fallbackRuns: 0,
    providers: [],
    activeRouteId: null,
    activeModel: null,
    activeIsFallback: false,
    activeReason: null,
    ...patch,
  };
}

describe("policyOf", () => {
  it("後端還沒送 policy 時預設為 always", () => {
    expect(policyOf(settings())).toBe("always");
    expect(policyOf(settings({ routeId: "grok-cli" }))).toBe("always");
  });

  it("後端送了就以它為準", () => {
    expect(policyOf(settings({ routeId: "grok-cli", policy: "failover" }))).toBe("failover");
    expect(policyOf(settings({ routeId: "grok-cli", policy: "always" }))).toBe("always");
  });

  it("沒有 settings 時也是 always", () => {
    expect(policyOf(null)).toBe("always");
  });

  it("兩個策略都有文案，而且寫的是後果不是功能名", () => {
    expect(REVIEW_POLICIES).toHaveLength(2);
    for (const p of REVIEW_POLICIES) expect(p.blurbKey.length).toBeGreaterThan(10);
  });
});

describe("needsFallback", () => {
  it("只有 failover 需要備援欄位", () => {
    expect(needsFallback("failover")).toBe(true);
    expect(needsFallback("always")).toBe(false);
  });
});

describe("fallbackShare", () => {
  it("沒跑過就是 0，不能是 NaN", () => {
    expect(fallbackShare(stats())).toBe(0);
    expect(fallbackShare(null)).toBe(0);
  });

  it("算的是備援佔總次數的比例", () => {
    expect(fallbackShare(stats({ totalRuns: 128, fallbackRuns: 23 }))).toBe(18);
    expect(fallbackShare(stats({ totalRuns: 10, fallbackRuns: 10 }))).toBe(100);
  });
});

describe("statsToShow", () => {
  const now = 2_000_000_000;
  const grok = {
    routeId: "grok-cli",
    provider: "Grok",
    model: "grok-4.5",
    primaryRuns: 100,
    fallbackRuns: 0,
    failedRuns: 0,
    lastUsedAt: now - 60,
  };
  const idle = {
    routeId: "weikuwu",
    provider: "weikuwu",
    model: "GLM",
    primaryRuns: 0,
    fallbackRuns: 0,
    failedRuns: 0,
    lastUsedAt: null,
  };

  it("完全沒關係的供應商不該灌長表格", () => {
    const rows = statsToShow(stats({ providers: [grok, idle] }), [], now);
    expect(rows.map((r) => r.routeId)).toEqual(["grok-cli"]);
  });

  it("目前被指定為主要／備援的就算 0 次也要列 —— 「設了卻沒出手」本身是資訊", () => {
    const rows = statsToShow(stats({ providers: [grok, idle] }), ["weikuwu"], now);
    expect(rows.map((r) => r.routeId)).toEqual(["grok-cli", "weikuwu"]);
  });

  it("出手多的排前面", () => {
    const busy = {
      ...idle,
      routeId: "x",
      provider: "X",
      fallbackRuns: 500,
      lastUsedAt: now - 30,
    };
    const rows = statsToShow(stats({ providers: [grok, busy] }), [], now);
    expect(rows[0]?.routeId).toBe("x");
  });

  it("超過七天沒有使用的 Provider 從列表移除，即使仍是目前設定", () => {
    const stale = { ...grok, lastUsedAt: now - 7 * 86_400 - 1 };
    const boundary = { ...grok, routeId: "boundary", lastUsedAt: now - 7 * 86_400 };
    const rows = statsToShow(
      stats({ providers: [stale, boundary] }),
      ["grok-cli"],
      now,
    );
    expect(rows.map((row) => row.routeId)).toEqual(["boundary"]);
  });
});
