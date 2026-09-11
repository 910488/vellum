/** Locale resolution and preference persistence for Vellum UI. */

export type AppLocale = "zh-TW" | "zh-CN" | "en" | "ja";
export type LocalePreference = "system" | AppLocale;

export const APP_LOCALES: AppLocale[] = ["zh-TW", "zh-CN", "en", "ja"];
export const LOCALE_STORAGE_KEY = "vellum.locale";

/** The four supported locales are complete for the primary application screens. */
export const I18N_LANGUAGE_SELECTOR_ENABLED = true;

/** Safety fallback for invalid or unavailable locale preferences. */
export const I18N_FALLBACK_LOCALE: AppLocale = "zh-TW";

export function isAppLocale(value: string | null | undefined): value is AppLocale {
  return value === "zh-TW" || value === "zh-CN" || value === "en" || value === "ja";
}

export function isLocalePreference(value: string | null | undefined): value is LocalePreference {
  return value === "system" || isAppLocale(value);
}

/** Normalize BCP 47 tags: underscores and unicode/private-use extensions. */
export function normalizeLocaleTag(tag: string | null | undefined): string {
  if (!tag) return "";
  const primary = tag.trim().replace(/_/g, "-").split(",")[0]?.trim() ?? "";
  if (!primary) return "";
  // Drop unicode extension / transformed extension / private use (`-u-`, `-t-`, `-x-`).
  const cut = primary.search(/-(?:u|t|x)-/i);
  return (cut >= 0 ? primary.slice(0, cut) : primary).toLowerCase();
}

/**
 * Map a browser/OS locale tag onto a supported Vellum locale.
 * Returns null when the tag is empty or unsupported so callers can keep scanning.
 */
export function tryMapSystemLocale(tag: string | null | undefined): AppLocale | null {
  const lower = normalizeLocaleTag(tag);
  if (!lower) return null;
  const parts = lower.split("-").filter(Boolean);
  const language = parts[0] ?? "";

  if (language === "zh") {
    // Script tags take precedence over region when present.
    if (parts.includes("hant")) return "zh-TW";
    if (parts.includes("hans")) return "zh-CN";
    if (parts.includes("tw") || parts.includes("hk") || parts.includes("mo")) return "zh-TW";
    if (parts.includes("cn") || parts.includes("sg") || parts.length === 1) return "zh-CN";
    return "zh-CN";
  }
  if (language === "ja") return "ja";
  if (language === "en") return "en";
  return null;
}

/** Map a browser/OS locale tag onto one of the four supported Vellum locales. */
export function mapSystemLocale(tag: string | null | undefined): AppLocale {
  return tryMapSystemLocale(tag) ?? "en";
}

export function resolveLocaleFromLanguages(
  languages: readonly string[] | null | undefined,
  fallbackLanguage?: string | null,
): AppLocale {
  const candidates = [...(languages ?? [])];
  if (fallbackLanguage) candidates.push(fallbackLanguage);
  for (const tag of candidates) {
    const mapped = tryMapSystemLocale(tag);
    if (mapped) return mapped;
  }
  return "en";
}

export function detectSystemLocale(
  languages: readonly string[] | null | undefined = typeof navigator !== "undefined" ? navigator.languages : null,
  language: string | null | undefined = typeof navigator !== "undefined" ? navigator.language : null,
): AppLocale {
  return resolveLocaleFromLanguages(languages, language);
}

export function readLocalePreference(
  storage: Pick<Storage, "getItem"> | null | undefined = typeof localStorage !== "undefined" ? localStorage : null,
): LocalePreference {
  try {
    const raw = storage?.getItem(LOCALE_STORAGE_KEY);
    if (isLocalePreference(raw)) return raw;
  } catch {
    // ignore storage failures; treat as system
  }
  return "system";
}

export function writeLocalePreference(
  preference: LocalePreference,
  storage: Pick<Storage, "setItem"> | null | undefined = typeof localStorage !== "undefined" ? localStorage : null,
): void {
  try {
    storage?.setItem(LOCALE_STORAGE_KEY, preference);
  } catch {
    // ignore quota / private-mode failures
  }
}

export function resolveAppLocale(
  preference: LocalePreference,
  languages?: readonly string[] | null,
  language?: string | null,
): AppLocale {
  if (preference !== "system") return preference;
  return detectSystemLocale(languages, language);
}

/** Active UI locale resolved from the saved preference or system language. */
export function resolveRuntimeLocale(
  preference: LocalePreference = readLocalePreference(),
  languages?: readonly string[] | null,
  language?: string | null,
): AppLocale {
  if (!I18N_LANGUAGE_SELECTOR_ENABLED) return I18N_FALLBACK_LOCALE;
  return resolveAppLocale(preference, languages, language);
}

export function htmlLangForLocale(locale: AppLocale): string {
  switch (locale) {
    case "zh-TW":
      return "zh-Hant-TW";
    case "zh-CN":
      return "zh-Hans-CN";
    case "ja":
      return "ja";
    case "en":
    default:
      return "en";
  }
}

export function applyDocumentLang(
  locale: AppLocale,
  doc: Document | null | undefined = typeof document !== "undefined" ? document : null,
): void {
  if (!doc?.documentElement) return;
  doc.documentElement.lang = htmlLangForLocale(locale);
}
