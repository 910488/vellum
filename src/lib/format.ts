/** Display formatting helpers. Locale-aware values take an explicit locale. */

import type { AppLocale } from "@/i18n/locale";
import { formatDateTime, formatNumber, formatRelative } from "@/i18n/format";

const TOKEN_TIERS = [
  { limit: 1_000_000_000, suffix: "B", decimals: 1 },
  { limit: 1_000_000, suffix: "M", decimals: 1 },
  { limit: 1_000, suffix: "K", decimals: 0 },
] as const;

export function tokens(n: number): string {
  for (let i = 0; i < TOKEN_TIERS.length; i += 1) {
    const tier = TOKEN_TIERS[i];
    if (!tier || n < tier.limit) continue;
    const scaled = n / tier.limit;
    const text = scaled.toFixed(Number.isInteger(scaled) ? 0 : tier.decimals);
    if (Number(text) >= 1000) {
      const bigger = TOKEN_TIERS[i - 1];
      if (bigger) return `1${bigger.suffix}`;
    }
    return `${text}${tier.suffix}`;
  }
  return String(n);
}

export function exact(n: number, locale: AppLocale): string {
  return formatNumber(n, locale);
}

export function percent(used: number, total: number): number {
  if (total <= 0) return 0;
  return Math.min(100, Math.max(0, Math.round((used / total) * 100)));
}

export function duration(minutes: number): { value: string; unit: string } {
  const h = Math.floor(minutes / 60);
  const m = minutes % 60;
  if (h === 0) return { value: String(m), unit: "m" };
  return { value: String(h), unit: `h ${m}m` };
}

export type MessageRef = {
  key: string;
  values?: Record<string, string | number>;
};

/** Returns a translation key + values for a reset timestamp. */
export function resetLabelRef(iso: string | null): MessageRef {
  if (!iso) return { key: "common.emDash" };
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return { key: "common.emDash" };
  const hh = String(d.getHours()).padStart(2, "0");
  const mi = String(d.getMinutes()).padStart(2, "0");
  return {
    key: "common.resetAt",
    values: {
      month: d.getMonth() + 1,
      day: d.getDate(),
      time: `${hh}:${mi}`,
    },
  };
}

/** Locale-aware reset label for tests and non-React call sites. */
export function resetLabel(iso: string | null, locale: AppLocale, t?: (key: string, values?: Record<string, unknown>) => string): string {
  const ref = resetLabelRef(iso);
  if (t) return t(ref.key, ref.values);
  if (ref.key === "common.emDash") return "—";
  const d = new Date(iso as string);
  const hh = String(d.getHours()).padStart(2, "0");
  const mi = String(d.getMinutes()).padStart(2, "0");
  if (locale === "en") {
    return `Resets ${d.getMonth() + 1}/${d.getDate()} ${hh}:${mi}`;
  }
  return `${d.getMonth() + 1} 月 ${d.getDate()} 日 ${hh}:${mi} 重置`;
}

/** 本機時區，寫成 GMT+8 這種讀得懂的樣子。半小時時區（印度、尼泊爾）也要
    寫得出來，所以不是整除。 */
function localZoneLabel(at: Date): string {
  const minutes = -at.getTimezoneOffset();
  const sign = minutes < 0 ? "-" : "+";
  const abs = Math.abs(minutes);
  const hours = Math.floor(abs / 60);
  const rest = abs % 60;
  return `GMT${sign}${hours}${rest ? `:${String(rest).padStart(2, "0")}` : ""}`;
}

/**
 * 到期時刻的絕對寫法：幾月幾日幾點幾分，加上時區。
 *
 * 跟 `resetLabelRef` 分開，因為那個念的是「重置」—— 額度視窗會重置，
 * Reset 券不會，它是到期作廢。同一組數字配錯動詞，意思剛好相反。
 *
 * 時區要寫出來：到期時間是上游給的 UTC 字串，這裡換算成本機時間顯示，而
 * 一個沒有標時區的時刻沒辦法自己證明它換算過了。
 */
export function expiresAtRef(iso: string | null): MessageRef {
  if (!iso) return { key: "common.emDash" };
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return { key: "common.emDash" };
  const hh = String(d.getHours()).padStart(2, "0");
  const mi = String(d.getMinutes()).padStart(2, "0");
  return {
    key: "common.expiresAt",
    values: {
      month: d.getMonth() + 1,
      day: d.getDate(),
      time: `${hh}:${mi}`,
      zone: localZoneLabel(d),
    },
  };
}

/**
 * 還剩多久。絕對時間回答「哪一天」，這個回答「來不來得及」。
 *
 * 變數叫 `value` 不叫 `count`：i18next 看到 `count` 會改去找 `_one` /
 * `_other`，四語資源檔裡沒有那些鍵，整句話就會變成鍵名。
 */
export function expiresInRef(iso: string | null, now: number = Date.now()): MessageRef | null {
  if (!iso) return null;
  const at = new Date(iso).getTime();
  if (Number.isNaN(at)) return null;
  const left = at - now;
  if (left <= 0) return { key: "common.expiredAlready" };
  if (left < 3_600_000) {
    return { key: "common.expiresInMinutes", values: { value: Math.max(1, Math.floor(left / 60_000)) } };
  }
  if (left < 86_400_000) {
    return { key: "common.expiresInHours", values: { value: Math.floor(left / 3_600_000) } };
  }
  return { key: "common.expiresInDays", values: { value: Math.floor(left / 86_400_000) } };
}

export function sourceLabelKey(source: string): string {
  switch (source) {
    case "override":
      return "vocabulary.source.override";
    case "modelCache":
      return "vocabulary.source.modelCache";
    case "catalog":
      return "vocabulary.source.catalog";
    case "fallback":
      return "vocabulary.source.fallback";
    default:
      return source;
  }
}

/** Prefer sourceLabelKey + t() in UI; this remains for tests with an optional translator. */
export function sourceLabel(source: string, t?: (key: string) => string): string {
  const key = sourceLabelKey(source);
  if (key === source) return source;
  return t ? t(key) : key;
}

export function sinceLabelRef(epochSeconds: number | null | undefined): MessageRef | null {
  if (!epochSeconds) return null;
  const secs = Math.floor(Date.now() / 1000) - epochSeconds;
  if (secs < 60) return { key: "common.justNow" };
  if (secs < 3_600) return { key: "common.minutesAgo", values: { count: Math.floor(secs / 60) } };
  if (secs < 86_400) return { key: "common.hoursAgo", values: { count: Math.floor(secs / 3_600) } };
  return { key: "common.daysAgo", values: { count: Math.floor(secs / 86_400) } };
}

export function sinceLabel(
  epochSeconds: number | null | undefined,
  empty: string,
  locale: AppLocale,
  t?: (key: string, values?: Record<string, unknown>) => string,
): string {
  const ref = sinceLabelRef(epochSeconds);
  if (!ref) return empty;
  if (t) return t(ref.key, ref.values);
  if (ref.key === "common.justNow") {
    return locale === "en" ? "Just now" : locale === "ja" ? "たった今" : locale === "zh-CN" ? "刚刚" : "剛剛";
  }
  const count = Number(ref.values?.count ?? 0);
  if (ref.key === "common.minutesAgo") {
    return formatRelative(-count, "minute", locale);
  }
  if (ref.key === "common.hoursAgo") {
    return formatRelative(-count, "hour", locale);
  }
  return formatRelative(-count, "day", locale);
}

// keep date helper available for future UI use
export { formatDateTime };
