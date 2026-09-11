import { describe, expect, it } from "vitest";
import { APP_LOCALES } from "@/i18n/locale";
import { assertResourceParity, resourceKeySet, resources } from "@/i18n/resources";

function flatten(tree: Record<string, unknown>, prefix = ""): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [key, value] of Object.entries(tree)) {
    const path = prefix ? `${prefix}.${key}` : key;
    if (typeof value === "string") out[path] = value;
    else if (value && typeof value === "object") Object.assign(out, flatten(value as Record<string, unknown>, path));
  }
  return out;
}

function placeholders(text: string): string[] {
  return [...text.matchAll(/\{\{(\w+)\}\}/g)].map((match) => match[1]!).sort();
}

describe("i18n resources", () => {
  it("keeps identical key sets across all locales", () => {
    expect(() => assertResourceParity()).not.toThrow();
    const baseline = resourceKeySet("zh-TW");
    expect(baseline.length).toBeGreaterThan(50);
    for (const locale of APP_LOCALES) {
      expect(resourceKeySet(locale)).toEqual(baseline);
      expect(resources[locale].translation).toBeTruthy();
    }
  });

  it("includes interpolation and plural keys used by the shell", () => {
    const keys = resourceKeySet("zh-TW");
    expect(keys).toContain("common.minutesAgo");
    expect(keys).toContain("common.resetAt");
    expect(keys).toContain("common.totalItems_one");
    expect(keys).toContain("common.totalItems_other");
    expect(keys).toContain("quota.remainingLabel");
    expect(keys).toContain("settings.language.option.system");
  });

  /* 之前有一批日文與簡中的字串被存成 "??????" —— 編碼在某次編輯裡掉了，
     而缺字跟壞字不一樣：缺字會被 parity 檢查抓到，壞字不會，它一路長到畫面上。
     全形問號（？）是正常標點，只擋半形的。 */
  it("has no characters lost to encoding damage", () => {
    for (const locale of APP_LOCALES) {
      const flat = flatten(resources[locale].translation as unknown as Record<string, unknown>);
      for (const [key, text] of Object.entries(flat)) {
        expect({ locale, key, text }).toMatchObject({
          text: expect.not.stringMatching(/\?\?/),
        });
      }
    }
  });

  it("keeps the same placeholders across locales", () => {
    const baseline = flatten(resources["zh-TW"].translation as unknown as Record<string, unknown>);
    for (const locale of APP_LOCALES) {
      if (locale === "zh-TW") continue;
      const other = flatten(resources[locale].translation as unknown as Record<string, unknown>);
      for (const [key, text] of Object.entries(baseline)) {
        expect(placeholders(other[key] ?? "")).toEqual(placeholders(text));
      }
    }
  });
});
