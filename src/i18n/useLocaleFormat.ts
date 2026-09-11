import { useCallback, useMemo } from "react";
import { useTranslation } from "react-i18next";
import { getResolvedLocale } from "@/i18n";
import type { AppLocale } from "@/i18n/locale";
import {
  exact as exactWithLocale,
  resetLabel as resetLabelWithLocale,
  resetLabelRef,
  sinceLabel as sinceLabelWithLocale,
  sinceLabelRef,
} from "@/lib/format";

/**
 * Locale-bound display helpers for React components.
 * Always use this instead of calling exact/resetLabel/sinceLabel with implicit defaults.
 */
export function useLocaleFormat() {
  const { t, i18n } = useTranslation();
  const locale: AppLocale = useMemo(() => {
    const current = i18n.resolvedLanguage ?? i18n.language;
    if (current === "zh-TW" || current === "zh-CN" || current === "en" || current === "ja") {
      return current;
    }
    return getResolvedLocale();
  }, [i18n.language, i18n.resolvedLanguage]);

  const translate = useCallback(
    (key: string, values?: Record<string, unknown>) => String(t(key, values)),
    [t],
  );

  const exact = useCallback((n: number) => exactWithLocale(n, locale), [locale]);
  const resetLabel = useCallback(
    (iso: string | null) => resetLabelWithLocale(iso, locale, translate),
    [locale, translate],
  );
  const sinceLabel = useCallback(
    (epochSeconds: number | null | undefined, empty = "") =>
      sinceLabelWithLocale(epochSeconds, empty, locale, translate),
    [locale, translate],
  );
  const translateRef = useCallback(
    (ref: { key: string; values?: Record<string, string | number> } | null, empty = "") => {
      if (!ref) return empty;
      return translate(ref.key, ref.values);
    },
    [translate],
  );

  return {
    locale,
    t,
    exact,
    resetLabel,
    sinceLabel,
    resetLabelRef,
    sinceLabelRef,
    translateRef,
  };
}
