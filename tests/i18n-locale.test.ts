import { describe, expect, it } from "vitest";
import {
  detectSystemLocale,
  I18N_FALLBACK_LOCALE,
  I18N_LANGUAGE_SELECTOR_ENABLED,
  mapSystemLocale,
  normalizeLocaleTag,
  readLocalePreference,
  resolveAppLocale,
  resolveRuntimeLocale,
  tryMapSystemLocale,
  writeLocalePreference,
} from "@/i18n/locale";

describe("mapSystemLocale", () => {
  it.each([
    ["zh-Hant-TW", "zh-TW"],
    ["zh-HK", "zh-TW"],
    ["zh-MO", "zh-TW"],
    ["zh-TW", "zh-TW"],
    ["zh-TW-u-nu-hanidec", "zh-TW"],
    ["zh-Hans-CN", "zh-CN"],
    ["zh-SG", "zh-CN"],
    ["zh-CN", "zh-CN"],
    ["zh-Hans-TW", "zh-CN"],
    ["zh-Hant-CN", "zh-TW"],
    ["ja-JP", "ja"],
    ["en-US", "en"],
    ["fr-FR", "en"],
  ] as const)("maps %s to %s", (input, expected) => {
    expect(mapSystemLocale(input)).toBe(expected);
  });

  it("returns null for unsupported tags from tryMapSystemLocale", () => {
    expect(tryMapSystemLocale("fr-FR")).toBeNull();
    expect(tryMapSystemLocale("de-DE")).toBeNull();
  });

  it("prefers script over region for Chinese tags", () => {
    expect(tryMapSystemLocale("zh-Hans-TW")).toBe("zh-CN");
    expect(tryMapSystemLocale("zh-Hant-CN")).toBe("zh-TW");
  });

  it("strips unicode extensions before mapping", () => {
    expect(normalizeLocaleTag("zh-TW-u-nu-hanidec")).toBe("zh-tw");
  });
});

describe("detectSystemLocale", () => {
  it("prefers navigator.languages order", () => {
    expect(detectSystemLocale(["ja-JP", "en-US"], "en-US")).toBe("ja");
  });

  it("skips unsupported first preferences", () => {
    expect(detectSystemLocale(["fr-FR", "ja-JP"], "en-US")).toBe("ja");
    expect(detectSystemLocale(["de-DE", "zh-HK"], "en-US")).toBe("zh-TW");
  });

  it("falls back to navigator.language", () => {
    expect(detectSystemLocale([], "zh-HK")).toBe("zh-TW");
  });
});

describe("locale preference", () => {
  it("lets a saved manual locale override the system locale", () => {
    const store = new Map<string, string>();
    const storage = {
      getItem: (key: string) => store.get(key) ?? null,
      setItem: (key: string, value: string) => {
        store.set(key, value);
      },
    };
    writeLocalePreference("ja", storage);
    expect(readLocalePreference(storage)).toBe("ja");
    expect(resolveAppLocale(readLocalePreference(storage), ["en-US"], "en-US")).toBe("ja");
  });

  it("keeps system preference dynamic", () => {
    const store = new Map<string, string>([["vellum.locale", "system"]]);
    const storage = {
      getItem: (key: string) => store.get(key) ?? null,
      setItem: (key: string, value: string) => {
        store.set(key, value);
      },
    };
    expect(resolveAppLocale(readLocalePreference(storage), ["zh-CN"], "zh-CN")).toBe("zh-CN");
    expect(resolveAppLocale(readLocalePreference(storage), ["ja-JP"], "ja-JP")).toBe("ja");
  });
});

describe("runtime locale selection", () => {
  it("activates the saved locale preference", () => {
    expect(I18N_LANGUAGE_SELECTOR_ENABLED).toBe(true);
    expect(resolveRuntimeLocale("en", ["ja-JP"], "ja-JP")).toBe("en");
    expect(resolveRuntimeLocale("ja", ["en-US"], "en-US")).toBe("ja");
  });

  it("follows the system locale when the preference is system", () => {
    expect(resolveRuntimeLocale("system", ["zh-TW"], "en-US")).toBe("zh-TW");
    expect(resolveRuntimeLocale("system", ["ja-JP"], "en-US")).toBe("ja");
  });

  it("keeps the fallback for unsupported system languages", () => {
    expect(resolveRuntimeLocale("system", ["fr-FR"], "de-DE")).toBe("en");
    expect(I18N_FALLBACK_LOCALE).toBe("zh-TW");
  });
});
