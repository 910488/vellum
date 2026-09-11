import type { AppLocale } from "./locale";
import { htmlLangForLocale } from "./locale";

export function intlLocale(locale: AppLocale): string {
  return htmlLangForLocale(locale);
}

export function formatNumber(
  value: number,
  locale: AppLocale,
  options?: Intl.NumberFormatOptions,
): string {
  return new Intl.NumberFormat(intlLocale(locale), options).format(value);
}

export function formatDateTime(
  date: Date,
  locale: AppLocale,
  options?: Intl.DateTimeFormatOptions,
): string {
  return new Intl.DateTimeFormat(intlLocale(locale), options).format(date);
}

export function formatRelative(
  value: number,
  unit: Intl.RelativeTimeFormatUnit,
  locale: AppLocale,
  options?: Intl.RelativeTimeFormatOptions,
): string {
  return new Intl.RelativeTimeFormat(intlLocale(locale), {
    numeric: "auto",
    ...options,
  }).format(value, unit);
}
