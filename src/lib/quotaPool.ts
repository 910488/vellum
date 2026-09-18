import { isFiveHourQuota, isWeeklyQuota } from "@/lib/quota";
import type {
  CodexOAuthAccount,
  QuotaPoolMember,
  QuotaPoolSettings,
  QuotaSnapshot,
} from "@/types";

// Match the backend admission reserve: a request selected at the last few
// percent can itself cross the upstream five-hour limit.
export const FIVE_HOUR_ROUTING_RESERVE_PERCENT = 5;

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
      maintainFiveHourWindow: member.maintainFiveHourWindow ?? false,
    }));
  for (const account of accounts) {
    if (!byId.has(account.accountId)) {
      ordered.push({
        accountId: account.accountId,
        inPool: false,
        paused: false,
        weeklyFloor: 0,
        maintainFiveHourWindow: false,
      });
    }
  }
  // 順序只有一種：使用者排的那一種。後端存的設定可能還帶著舊的 most／soonest，
  // 這裡一律收成 rank —— 畫面上的號碼才會跟實際順序一致，下一次存檔也會把它寫回去。
  return { ...settings, strategy: "rank", members: ordered };
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
            : fiveHourRemaining <= FIVE_HOUR_ROUTING_RESERVE_PERCENT
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

/** 輪替順序就是成員順序，跳過現在用不了的帳號。跳過不會改變其餘的先後。 */
export function quotaPoolRotation(accounts: QuotaPoolAccount[]): QuotaPoolAccount[] {
  return accounts.filter((entry) => entry.usable);
}
