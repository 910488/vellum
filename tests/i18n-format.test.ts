import { describe, expect, it } from "vitest";
import { formatDateTime, formatNumber, formatRelative, intlLocale } from "@/i18n/format";
import { htmlLangForLocale } from "@/i18n/locale";

describe("locale-aware formatters", () => {
  it("uses the resolved locale for number formatting", () => {
    expect(formatNumber(190240, "en")).toBe("190,240");
    expect(intlLocale("zh-TW")).toBe(htmlLangForLocale("zh-TW"));
  });

  it("formats dates with the resolved locale", () => {
    const date = new Date("2026-07-28T13:23:39Z");
    const label = formatDateTime(date, "en", {
      month: "numeric",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
      hour12: false,
    });
    expect(label.length).toBeGreaterThan(0);
  });

  it("formats relative time with the resolved locale", () => {
    expect(formatRelative(-10, "minute", "en")).toMatch(/10/);
  });
});
