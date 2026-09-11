import { describe, expect, it } from "vitest";
import {
  accountQuotaPresentation,
  accountQuotaWindows,
  accountQuotaLabel,
  findTightestWeeklyQuota,
  isFiveHourQuota,
  isWeeklyQuota,
  quotaPeriodLabel,
  remainingQuotaPresentation,
} from "@/lib/quota";
import type { ProviderOverview, QuotaPeriod, QuotaSnapshot, Route } from "@/types";

function quota(period: QuotaPeriod, usedPercent: number): QuotaSnapshot {
  return {
    routeId: "route",
    period,
    usedPercent,
    resetAt: null,
    tier: null,
    stale: false,
  };
}

const WEEK: QuotaPeriod = { unit: "week", amount: null };
const MONTH: QuotaPeriod = { unit: "month", amount: null };
const FIVE_HOURS: QuotaPeriod = { unit: "hour", amount: 5 };
const THIRTY_DAYS: QuotaPeriod = { unit: "day", amount: 30 };

/** i18next 的替身：回傳鍵與參數，斷言看得出組出來的是哪一句。 */
const t = (key: string, values?: Record<string, unknown>) =>
  values ? `${key}(${JSON.stringify(values)})` : key;

function provider(
  name: string,
  windows: QuotaSnapshot[],
  enabled = true,
): ProviderOverview {
  return {
    route: {
      id: name,
      name,
      enabled,
    } as Route,
    appliedToRunningProxy: false,
    quota: windows[0] ?? null,
    quotaWindows: windows,
    quotaError: null,
    models: [],
    latestInputTokens: 0,
    turns: 0,
    firstByteMs: null,
  };
}

describe("weekly quota classification", () => {
  it("recognizes the week unit regardless of how long the window is named", () => {
    expect(isWeeklyQuota(quota(WEEK, 0))).toBe(true);
  });

  it.each([FIVE_HOURS, THIRTY_DAYS, MONTH])(
    "does not classify %o as weekly",
    (period) => {
      expect(isWeeklyQuota(quota(period, 0))).toBe(false);
    },
  );
});

describe("five-hour quota classification", () => {
  it("recognizes only the explicit rolling five-hour window", () => {
    expect(isFiveHourQuota(quota(FIVE_HOURS, 0))).toBe(true);
    expect(isFiveHourQuota(quota({ unit: "hour", amount: 6 }, 0))).toBe(false);
    expect(isFiveHourQuota(quota(WEEK, 0))).toBe(false);
  });
});

describe("quota period label", () => {
  it("uses named keys for week and month windows", () => {
    expect(quotaPeriodLabel(WEEK, t)).toBe("quota.period.week");
    expect(quotaPeriodLabel(MONTH, t)).toBe("quota.period.month");
  });

  it("passes the amount through for counted windows", () => {
    expect(quotaPeriodLabel(FIVE_HOURS, t)).toBe('quota.period.hours({"count":5})');
    expect(quotaPeriodLabel(THIRTY_DAYS, t)).toBe('quota.period.days({"count":30})');
  });

  it("names an unrecognized window instead of leaving it blank", () => {
    expect(quotaPeriodLabel({ unit: "unspecified", amount: null }, t)).toBe(
      "quota.period.unspecified",
    );
  });
});

describe("tightest weekly quota", () => {
  it("compares weekly windows across providers", () => {
    const result = findTightestWeeklyQuota([
      provider("OpenAI", [quota(WEEK, 99)]),
      provider("Grok Build", [quota(WEEK, 0)]),
    ]);

    expect(result).toMatchObject({
      name: "OpenAI",
      remaining: 1,
    });
  });

  it("ignores disabled providers and clamps malformed percentages", () => {
    const result = findTightestWeeklyQuota([
      provider("disabled", [quota(WEEK, 100)], false),
      provider("enabled", [quota(WEEK, 150)]),
    ]);

    expect(result).toMatchObject({
      name: "enabled",
      remaining: 0,
    });
  });

  it("returns null when no enabled provider has a weekly window", () => {
    expect(
      findTightestWeeklyQuota([provider("OpenAI", [quota(FIVE_HOURS, 99)])]),
    ).toBeNull();
  });
});

describe("remaining quota presentation", () => {
  it("uses the remaining percentage for both the number and meter", () => {
    expect(remainingQuotaPresentation(82)).toEqual({
      value: 82,
      meterPercent: 82,
    });
  });

  it("clamps malformed percentages", () => {
    expect(remainingQuotaPresentation(120).meterPercent).toBe(100);
    expect(remainingQuotaPresentation(-20).meterPercent).toBe(0);
  });
});

describe("account quota label", () => {
  /* ChatGPT 的 5 小時上限與每週上限同時生效，而先擋住你的幾乎都是 5 小時
     那條。這裡以前挑週視窗顯示，於是畫面上「還剩 82%」跟「現在送不出去」
     同時成立 —— 見底的是另一條，只是沒有被畫出來。 */
  it("leads with the window that is actually binding, not the weekly one", () => {
    const presentation = accountQuotaPresentation([
      quota(WEEK, 18),
      quota(FIVE_HOURS, 90),
    ]);
    expect(presentation?.period).toEqual(FIVE_HOURS);
    expect(presentation?.remaining).toBe(10);
  });

  /* 週見底而 5 小時剛重置時，擋住你的是週視窗 —— 而且它要好幾天才回來。
     「最短的那一條」在這一格會指錯人，所以看的是剩餘量。 */
  it("names the weekly window when it is the one that ran out", () => {
    const presentation = accountQuotaPresentation([
      quota(FIVE_HOURS, 4),
      quota(WEEK, 96),
    ]);
    expect(presentation?.period).toEqual(WEEK);
    expect(presentation?.remaining).toBe(4);
  });

  it("keeps the shorter window when both have the same amount left", () => {
    const presentation = accountQuotaPresentation([quota(WEEK, 50), quota(FIVE_HOURS, 50)]);
    expect(presentation?.period).toEqual(FIVE_HOURS);
  });

  /* 顯示順序照視窗長度固定：兩條交叉時整列不該自己對調。 */
  it("keeps every window, shortest first, so neither limit is hidden", () => {
    const windows = accountQuotaWindows([quota(WEEK, 18), quota(FIVE_HOURS, 90)]);
    expect(windows.map((window) => window.period)).toEqual([FIVE_HOURS, WEEK]);
    expect(windows.map((window) => window.remaining)).toEqual([10, 82]);
  });

  it("orders unrelated window lengths by duration too", () => {
    const windows = accountQuotaWindows([
      quota(MONTH, 10),
      quota(WEEK, 10),
      quota(FIVE_HOURS, 10),
    ]);
    expect(windows.map((window) => window.period.unit)).toEqual(["hour", "week", "month"]);
  });

  it("returns nothing when there are no windows at all", () => {
    expect(accountQuotaWindows([])).toEqual([]);
    expect(accountQuotaPresentation([])).toBeNull();
  });

  it("exposes the same remaining value and reset metadata used by the gold meter", () => {
    const weekly = quota(WEEK, 18);
    weekly.resetAt = "2026-08-04T21:23:00+08:00";
    expect(accountQuotaPresentation([weekly])).toEqual({
      remaining: 82,
      period: WEEK,
      resetAt: "2026-08-04T21:23:00+08:00",
    });
  });

  it("falls back to the available window and handles no data", () => {
    expect(accountQuotaPresentation([quota(FIVE_HOURS, 25)])).toMatchObject({
      remaining: 75,
      period: FIVE_HOURS,
    });
    expect(accountQuotaLabel([], t)).toBeNull();
  });

  it("shows both five-hour and weekly usage when the account reports both", () => {
    expect(
      accountQuotaWindows([
        quota(FIVE_HOURS, 10),
        quota(WEEK, 18),
      ]),
    ).toEqual([
      { remaining: 90, period: FIVE_HOURS, resetAt: null },
      { remaining: 82, period: WEEK, resetAt: null },
    ]);
  });

  it("does not invent a five-hour limit when the endpoint omits it", () => {
    expect(accountQuotaWindows([quota(WEEK, 18)])).toEqual([
      { remaining: 82, period: WEEK, resetAt: null },
    ]);
    expect(accountQuotaWindows([])).toEqual([]);
  });
});
