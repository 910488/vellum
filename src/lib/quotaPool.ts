import { isFiveHourQuota, isWeeklyQuota } from "@/lib/quota";
import type {
  CodexOAuthAccount,
  QuotaPoolMember,
  QuotaPoolSettings,
  QuotaSnapshot,
} from "@/types";

export interface QuotaPoolAccount {
  account: CodexOAuthAccount;
  member: QuotaPoolMember;
  weekly: QuotaSnapshot | null;
  fiveHour: QuotaSnapshot | null;
  weeklyRemaining: number | null;
  fiveHourRemaining: number | null;
  burnable: number;
  usable: boolean;
  reason: "paused" | "weeklyGate" | "fiveHour" | "missingQuota" | null;
}

export function normalizeQuotaPool(
  settings: QuotaPoolSettings,
  accounts: CodexOAuthAccount[],
): QuotaPoolSettings {
  const byId = new Map(settings.members.map((member) => [member.accountId, member]));
  const ordered = settings.members
    .filter((member) => accounts.some((account) => account.accountId === member.accountId))
    .map((member) => ({
      ...member,
      weeklyFloor: Math.max(0, Math.min(100, Math.round(member.weeklyFloor))),
      paused: member.inPool ? member.paused : false,
    }));
  for (const account of accounts) {
    if (!byId.has(account.accountId)) {
      ordered.push({ accountId: account.accountId, inPool: false, paused: false, weeklyFloor: 0 });
    }
  }
  return { ...settings, members: ordered };
}

export function quotaPoolAccounts(
  settings: QuotaPoolSettings,
  accounts: CodexOAuthAccount[],
  quotas: Record<string, QuotaSnapshot[]>,
): QuotaPoolAccount[] {
  const normalized = normalizeQuotaPool(settings, accounts);
  const accountById = new Map(accounts.map((account) => [account.accountId, account]));
  return normalized.members.flatMap((member) => {
    const account = accountById.get(member.accountId);
    if (!account) return [];
    const windows = quotas[member.accountId] ?? [];
    const weekly = windows.find(isWeeklyQuota) ?? null;
    const fiveHour = windows.find(isFiveHourQuota) ?? null;
    const weeklyRemaining = weekly ? Math.max(0, Math.min(100, 100 - weekly.usedPercent)) : null;
    const fiveHourRemaining = fiveHour ? Math.max(0, Math.min(100, 100 - fiveHour.usedPercent)) : null;
    const burnable = Math.max(0, (weeklyRemaining ?? 0) - member.weeklyFloor);
    const reason = !member.inPool
      ? null
      : member.paused
        ? "paused"
        : weeklyRemaining === null || fiveHourRemaining === null
          ? "missingQuota"
          : burnable <= 0
            ? "weeklyGate"
            : fiveHourRemaining <= 0
              ? "fiveHour"
              : null;
    return [{
      account,
      member,
      weekly,
      fiveHour,
      weeklyRemaining,
      fiveHourRemaining,
      burnable,
      usable: member.inPool && reason === null,
      reason,
    }];
  });
}

export function quotaPoolRotation(
  settings: QuotaPoolSettings,
  accounts: QuotaPoolAccount[],
): QuotaPoolAccount[] {
  const live = accounts.filter((entry) => entry.usable);
  if (settings.strategy === "most") {
    return [...live].sort((left, right) => right.burnable - left.burnable);
  }
  if (settings.strategy === "soonest") {
    const reset = (entry: QuotaPoolAccount) => {
      const value = entry.weekly?.resetAt ? Date.parse(entry.weekly.resetAt) : Number.POSITIVE_INFINITY;
      return Number.isFinite(value) ? value : Number.POSITIVE_INFINITY;
    };
    return [...live].sort((left, right) => reset(left) - reset(right));
  }
  return live;
}
