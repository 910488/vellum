import type { ProviderOverview, QuotaPeriod, QuotaSnapshot } from "@/types";

export interface TightestWeeklyQuota {
  name: string;
  remaining: number;
  resetAt: string | null;
}

export type Translate = (key: string, values?: Record<string, unknown>) => string;

export function remainingQuotaPresentation(remaining: number): {
  value: number;
  meterPercent: number;
} {
  const value = Math.max(0, Math.min(100, remaining));
  return { value, meterPercent: value };
}

/**
 * 視窗長度是結構化資料（單位＋數量），不是後端組好的一句話 ——
 * 判斷週視窗只要看單位，不必再猜「7 天」「7 日」「weekly」是不是同一件事。
 */
export function isWeeklyQuota(window: Pick<QuotaSnapshot, "period">): boolean {
  return window.period.unit === "week";
}

/** Codex may expose a separate rolling five-hour allowance. Detect the
 * capability from the returned window instead of guessing from a plan name. */
export function isFiveHourQuota(window: Pick<QuotaSnapshot, "period">): boolean {
  return window.period.unit === "hour" && window.period.amount === 5;
}

/** 把視窗長度組成當前語言的一句話。 */
export function quotaPeriodLabel(period: QuotaPeriod, t: Translate): string {
  switch (period.unit) {
    case "week":
      return t("quota.period.week");
    case "month":
      return t("quota.period.month");
    case "hour":
      return t("quota.period.hours", { count: period.amount ?? 0 });
    case "day":
      return t("quota.period.days", { count: period.amount ?? 0 });
    default:
      return t("quota.period.unspecified");
  }
}

export function findTightestWeeklyQuota(
  providers: ProviderOverview[],
): TightestWeeklyQuota | null {
  const candidates = providers.flatMap((provider) => {
    if (!provider.route.enabled) return [];
    return provider.quotaWindows
      .filter(isWeeklyQuota)
      .map((window) => ({
        name: provider.route.name,
        remaining: 100 - Math.max(0, Math.min(100, window.usedPercent)),
        resetAt: window.resetAt,
      }));
  });

  return candidates.sort((a, b) => a.remaining - b.remaining)[0] ?? null;
}

export interface AccountQuotaPresentation {
  remaining: number;
  period: QuotaPeriod;
  resetAt: string | null;
}

/**
 * 視窗長度換算成秒。只用來排序，不用來顯示 —— 顯示交給 quotaPeriodLabel，
 * 因為「5 小時」在四種語言裡不是同一種寫法。
 */
function periodSeconds(period: QuotaPeriod): number {
  switch (period.unit) {
    case "hour":
      return (period.amount ?? 1) * 3_600;
    case "day":
      return (period.amount ?? 1) * 86_400;
    case "week":
      return 604_800;
    case "month":
      return 2_592_000;
    default:
      return Number.MAX_SAFE_INTEGER;
  }
}

/**
 * 帳號的每一個額度視窗，依視窗長度排，短的在前。
 *
 * ChatGPT 同時有 5 小時與每週兩條上限。之前這裡只挑週視窗顯示，於是畫面
 * 上的數字跟「為什麼現在送不出去」對不起來 —— 週還剩八成，5 小時已經見底。
 * 兩條都要出現。
 *
 * 排序用視窗長度而不是剩餘量，是為了讓它固定：同一個位置永遠是同一條視窗。
 * 依剩餘量排的話，兩條交叉的時候整列會自己對調，每次看都要重讀一次哪條是
 * 哪條。哪一條真的擋著你，交給 bindingQuotaWindow 標出來。
 */
export function accountQuotaWindows(
  windows: QuotaSnapshot[],
): AccountQuotaPresentation[] {
  return windows
    .map((window) => ({
      remaining: Math.round(100 - Math.max(0, Math.min(100, window.usedPercent))),
      period: window.period,
      resetAt: window.resetAt,
    }))
    .sort((left, right) => periodSeconds(left.period) - periodSeconds(right.period));
}

/**
 * 現在真正擋著你的那一條：剩最少的。
 *
 * 不是最短的那一條 —— 週視窗見底而 5 小時剛重置的時候，擋住你的是週視窗，
 * 而它還要好幾天才會回來。
 */
export function bindingQuotaWindow(
  windows: QuotaSnapshot[],
): AccountQuotaPresentation | null {
  const ordered = accountQuotaWindows(windows);
  // reduce 保留先到者，所以剩餘量相同時留下的是視窗較短的那一條。
  return ordered.reduce<AccountQuotaPresentation | null>(
    (tightest, window) =>
      tightest && tightest.remaining <= window.remaining ? tightest : window,
    null,
  );
}

/** 單一數字的場合（一句話的標籤）用擋著你的那一條。 */
export function accountQuotaPresentation(
  windows: QuotaSnapshot[],
): AccountQuotaPresentation | null {
  return bindingQuotaWindow(windows);
}

export function accountQuotaLabel(
  windows: QuotaSnapshot[],
  t: Translate,
): string | null {
  const presentation = accountQuotaPresentation(windows);
  if (!presentation) return null;
  return t("quota.remainingLabel", {
    period: quotaPeriodLabel(presentation.period, t),
    remaining: presentation.remaining,
  });
}
