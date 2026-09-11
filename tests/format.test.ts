import { describe, expect, it } from "vitest";
import { duration, exact, percent, resetLabelRef, sinceLabelRef, sourceLabel, sourceLabelKey, tokens } from "@/lib/format";
import i18n from "@/i18n";

describe("tokens", () => {
  it("shortens at K and M boundaries", () => {
    expect(tokens(950)).toBe("950");
    expect(tokens(1_000)).toBe("1K");
    expect(tokens(190_240)).toBe("190K");
    expect(tokens(1_000_000)).toBe("1M");
    expect(tokens(1_500_000)).toBe("1.5M");
  });

  it("does not print a pointless decimal on whole millions", () => {
    expect(tokens(2_000_000)).toBe("2M");
  });
});

describe("percent", () => {
  it("clamps instead of overflowing the meter", () => {
    expect(percent(150, 100)).toBe(100);
    expect(percent(-10, 100)).toBe(0);
  });

  it("treats a zero window as zero rather than dividing by zero", () => {
    expect(percent(100, 0)).toBe(0);
    expect(Number.isNaN(percent(100, 0))).toBe(false);
  });

  it("rounds to whole percent", () => {
    expect(percent(190_240, 500_000)).toBe(38);
  });
});

describe("duration", () => {
  it("drops the hour part under an hour", () => {
    expect(duration(42)).toEqual({ value: "42", unit: "m" });
  });

  it("splits hours and minutes", () => {
    expect(duration(252)).toEqual({ value: "4", unit: "h 12m" });
  });
});

describe("resetLabel", () => {
  it("returns a translation ref for local reset times", () => {
    const ref = resetLabelRef("2026-07-28T13:23:39Z");
    expect(ref.key).toBe("common.resetAt");
    expect(ref.values?.month).toBeTypeOf("number");
  });

  it("degrades to an em dash key rather than showing Invalid Date", () => {
    expect(resetLabelRef(null).key).toBe("common.emDash");
    expect(resetLabelRef("not a date").key).toBe("common.emDash");
  });
});

describe("exact", () => {
  it("groups thousands so long token counts stay scannable", () => {
    expect(exact(190_240, "en")).toBe("190,240");
  });
});

describe("sourceLabel", () => {
  it("maps every BudgetSource to a translation key", () => {
    expect(sourceLabelKey("override")).toBe("vocabulary.source.override");
    expect(sourceLabelKey("modelCache")).toBe("vocabulary.source.modelCache");
    expect(sourceLabelKey("catalog")).toBe("vocabulary.source.catalog");
    expect(sourceLabelKey("fallback")).toBe("vocabulary.source.fallback");
  });

  it("never says 型錄 in translated labels", () => {
    const t = i18n.getFixedT("zh-TW");
    for (const source of ["override", "modelCache", "catalog", "fallback"]) {
      expect(sourceLabel(source, t)).not.toContain("型錄");
    }
  });

  it("falls back to the raw value instead of rendering undefined", () => {
    expect(sourceLabel("something-new")).toBe("something-new");
  });
});

describe("tokens 的 B 階", () => {
  it("破十億要用 B，不能印出 4090M", () => {
    expect(tokens(4_090_000_000)).toBe("4.1B");
    expect(tokens(1_000_000_000)).toBe("1B");
  });

  it("四捨五入後滿 1000 要升一階，不能印出 1000.0M", () => {
    expect(tokens(999_999_999)).toBe("1B");
    expect(tokens(999_500)).toBe("1M");
  });

  it("M 與 K 的分界不受影響", () => {
    expect(tokens(4_700_000)).toBe("4.7M");
    expect(tokens(15_876)).toBe("16K");
  });
});

describe("sinceLabel", () => {
  const now = () => Math.floor(Date.now() / 1000);

  it("單位是 epoch 秒", () => {
    expect(sinceLabelRef(now() - 30)?.key).toBe("common.justNow");
    expect(sinceLabelRef(now() - 600)).toEqual({
      key: "common.minutesAgo",
      values: { count: 10 },
    });
  });
});
