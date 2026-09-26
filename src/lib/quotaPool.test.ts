import { describe, expect, it } from "vitest";
import type { CodexOAuthAccount, QuotaPoolSettings, QuotaSnapshot } from "../types";
import { normalizeQuotaPool, quotaPoolAccounts, quotaPoolRotation } from "./quotaPool";

const accounts: CodexOAuthAccount[] = [
  { accountId: "alice-personal", workspaceId: "personal", workspaceKind: "personal", email: "alice@example.test", authenticatedAt: 1, isDefault: true },
  { accountId: "alice-business", workspaceId: "shared", workspaceKind: "business", email: "alice@example.test", authenticatedAt: 2, isDefault: false },
  { accountId: "bob-business", workspaceId: "shared", workspaceKind: "business", email: "bob@example.test", authenticatedAt: 3, isDefault: false },
];

function windows(weeklyUsed: number): QuotaSnapshot[] {
  return [
    { routeId: "official", usedPercent: 10, period: { unit: "hour", amount: 5 }, resetAt: null, tier: null, stale: false },
    { routeId: "official", usedPercent: weeklyUsed, period: { unit: "week", amount: null }, resetAt: null, tier: null, stale: false },
  ];
}

describe("ChatGPT quota pool eligibility", () => {
  it("offers every managed credential and rotates across users and workspaces", () => {
    const settings: QuotaPoolSettings = { enabled: true, strategy: "rank", members: [] };
    const available = normalizeQuotaPool(settings, accounts);
    expect(available.members.map((member) => member.accountId)).toEqual(
      accounts.map((account) => account.accountId),
    );

    const enrolled: QuotaPoolSettings = {
      ...available,
      members: available.members.map((member) => ({ ...member, inPool: true })),
    };
    const entries = quotaPoolAccounts(enrolled, accounts, {
      "alice-personal": windows(100),
      "alice-business": windows(30),
      "bob-business": windows(20),
    });
    expect(entries.map((entry) => entry.member.inPool)).toEqual([true, true, true]);
    expect(quotaPoolRotation(entries).map((entry) => entry.account.accountId)).toEqual([
      "alice-business",
      "bob-business",
    ]);
  });
});
