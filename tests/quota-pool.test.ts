import { describe, expect, it } from "vitest";
import { normalizeQuotaPool, quotaPoolAccounts, quotaPoolRotation } from "@/lib/quotaPool";
import type { CodexOAuthAccount, QuotaPoolSettings, QuotaSnapshot } from "@/types";

const accounts: CodexOAuthAccount[] = ["a", "b"].map((accountId) => ({
  accountId,
  email: `${accountId}@example.test`,
  authenticatedAt: 1,
  isDefault: accountId === "a",
}));

const settings: QuotaPoolSettings = {
  enabled: true,
  strategy: "rank",
  members: [
    { accountId: "a", inPool: true, paused: false, weeklyFloor: 30 },
    { accountId: "b", inPool: true, paused: false, weeklyFloor: 0 },
  ],
};

function quota(unit: "week" | "hour", usedPercent: number): QuotaSnapshot {
  return {
    routeId: "official",
    usedPercent,
    period: { unit, amount: unit === "hour" ? 5 : null },
    resetAt: unit === "week" ? "2026-09-20T00:00:00Z" : null,
    tier: null,
    stale: false,
  };
}

describe("quota pool", () => {
  it("keeps fractional remaining quota consistent with backend gate decisions", () => {
    const entries = quotaPoolAccounts(settings, accounts, {
      a: [quota("hour", 99.8), quota("week", 69.8)],
    });
    expect(entries[0]!.usable).toBe(true);
    expect(entries[0]!.burnable).toBeCloseTo(0.2);
  });
  it("normalizes new and removed accounts without opting new accounts in", () => {
    expect(normalizeQuotaPool({ ...settings, members: [settings.members[0]!] }, accounts).members)
      .toEqual([
        settings.members[0],
        { accountId: "b", inPool: false, paused: false, weeklyFloor: 0 },
      ]);
  });

  it("uses weekly remaining above the gate and treats five-hour exhaustion as waiting", () => {
    const entries = quotaPoolAccounts(settings, accounts, {
      a: [quota("hour", 62), quota("week", 41)],
      b: [quota("hour", 100), quota("week", 8)],
    });
    expect(entries[0]).toMatchObject({ burnable: 29, usable: true, reason: null });
    expect(entries[1]).toMatchObject({ burnable: 92, usable: false, reason: "fiveHour" });
  });

  it("applies rank, most-remaining, and soonest-reset ordering only to usable members", () => {
    const entries = quotaPoolAccounts(settings, accounts, {
      a: [quota("hour", 10), quota("week", 41)],
      b: [quota("hour", 10), quota("week", 8)],
    });
    expect(quotaPoolRotation(settings, entries).map((entry) => entry.account.accountId)).toEqual(["a", "b"]);
    expect(quotaPoolRotation({ ...settings, strategy: "most" }, entries).map((entry) => entry.account.accountId)).toEqual(["b", "a"]);
  });
});
