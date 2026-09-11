import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import {
  applyDocumentLang,
  I18N_FALLBACK_LOCALE,
  I18N_LANGUAGE_SELECTOR_ENABLED,
  readLocalePreference,
  resolveRuntimeLocale,
  type AppLocale,
  type LocalePreference,
  writeLocalePreference,
} from "./locale";
import { assertResourceParity, resources } from "./resources";

assertResourceParity();

const initialPreference = readLocalePreference();
const initialLocale = resolveRuntimeLocale(initialPreference);

void i18n.use(initReactI18next).init({
  resources,
  lng: initialLocale,
  fallbackLng: I18N_FALLBACK_LOCALE,
  // Never silently fall back to Traditional Chinese for missing keys once multi-locale is live.
  partialBundledLanguages: true,
  interpolation: { escapeValue: false },
  returnNull: false,
  saveMissing: false,
  parseMissingKeyHandler: (key) => {
    if (import.meta.env.MODE !== "production") {
      console.error(`[i18n] missing key: ${key}`);
    }
    return key;
  },
});

applyDocumentLang(initialLocale);

export function getLocalePreference(): LocalePreference {
  return readLocalePreference();
}

export function getResolvedLocale(): AppLocale {
  // Keep callers on the gated runtime locale until full UI localization lands.
  if (!I18N_LANGUAGE_SELECTOR_ENABLED) return I18N_FALLBACK_LOCALE;
  const current = i18n.resolvedLanguage ?? i18n.language;
  if (current === "zh-TW" || current === "zh-CN" || current === "en" || current === "ja") {
    return current;
  }
  return resolveRuntimeLocale(readLocalePreference());
}

export async function setLocalePreference(preference: LocalePreference): Promise<AppLocale> {
  // Always persist the preference so it can resume after the flag is enabled.
  writeLocalePreference(preference);
  const locale = resolveRuntimeLocale(preference);
  await i18n.changeLanguage(locale);
  applyDocumentLang(locale);
  return locale;
}

export { i18n };
export default i18n;
