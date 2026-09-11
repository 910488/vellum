import { describe, expect, it } from "vitest";
import {
  isSpendableReset,
  orderResetCredits,
  resetExpiryTone,
  soonestResetExpiry,
} from "@/lib/resetCredits";
import { expiresAtRef, expiresInRef } from "@/lib/format";
import type { CodexResetCredit } from "@/types";

const NOW = Date.UTC(2026, 8, 10, 12, 0, 0);

function credit(over: Partial<CodexResetCredit> & { id: string }): CodexResetCredit {
  return {
    resetType: null,
    status: "available",
    expiresAt: null,
    title: null,
    description: null,
    ...over,
  };
}

function inHours(hours: number): string {
  return new Date(NOW + hours * 3_600_000).toISOString();
}

describe("orderResetCredits", () => {
  it("puts the credit that dies first at the top", () => {
    const ordered = orderResetCredits([
      credit({ id: "later", expiresAt: inHours(200) }),
      credit({ id: "soon", expiresAt: inHours(6) }),
      credit({ id: "middle", expiresAt: inHours(50) }),
    ]);
    expect(ordered.map((item) => item.id)).toEqual(["soon", "middle", "later"]);
  });

  it("keeps spent and expired credits in the list, at the bottom", () => {
    const ordered = orderResetCredits([
      credit({ id: "spent", status: "consumed", expiresAt: inHours(-4) }),
      credit({ id: "usable", expiresAt: inHours(300) }),
    ]);
    // 濾掉的話，「1 張可用」跟看得到的行數會兜不起來。
    expect(ordered.map((item) => item.id)).toEqual(["usable", "spent"]);
  });

  it("sorts a credit with no expiry after the ones that have a date", () => {
    const ordered = orderResetCredits([
      credit({ id: "unknown" }),
      credit({ id: "dated", expiresAt: inHours(900) }),
    ]);
    expect(ordered.map((item) => item.id)).toEqual(["dated", "unknown"]);
  });

  it("does not mutate the array it was handed", () => {
    const input = [
      credit({ id: "b", expiresAt: inHours(80) }),
      credit({ id: "a", expiresAt: inHours(2) }),
    ];
    orderResetCredits(input);
    expect(input.map((item) => item.id)).toEqual(["b", "a"]);
  });
});

describe("soonestResetExpiry", () => {
  /* 清單照這個順序排，所以最上面那一張就是最快作廢的那一張。使用者按哪
     一列就用掉哪一張 —— 挑選規則不在程式手上，畫面也就不必解釋它。 */
  it("names the credit that dies first, not the one upstream happened to list first", () => {
    const credits = [
      credit({ id: "listed-first", expiresAt: inHours(400) }),
      credit({ id: "expires-sooner", expiresAt: inHours(3) }),
    ];
    expect(soonestResetExpiry(credits)?.id).toBe("expires-sooner");
    expect(orderResetCredits(credits)[0]?.id).toBe("expires-sooner");
  });

  it("ignores credits that cannot be spent", () => {
    const credits = [
      credit({ id: "spent-but-sooner", status: "consumed", expiresAt: inHours(1) }),
      credit({ id: "usable", expiresAt: inHours(90) }),
    ];
    expect(soonestResetExpiry(credits)?.id).toBe("usable");
    expect(isSpendableReset(credits[0]!)).toBe(false);
  });

  it("is null when no spendable credit carries a date", () => {
    expect(soonestResetExpiry([credit({ id: "x" })])).toBeNull();
    expect(soonestResetExpiry([credit({ id: "y", status: "expired", expiresAt: inHours(-2) })])).toBeNull();
  });
});

describe("resetExpiryTone", () => {
  it("separates a missing date from a distant one", () => {
    // 「還很久」跟「上游沒說」在畫面上要長得不一樣。
    expect(resetExpiryTone(credit({ id: "none" }), NOW)).toBe("unknown");
    expect(resetExpiryTone(credit({ id: "far", expiresAt: inHours(400) }), NOW)).toBe("later");
  });

  it("warns inside two days and reports a lapsed credit as expired", () => {
    expect(resetExpiryTone(credit({ id: "soon", expiresAt: inHours(40) }), NOW)).toBe("soon");
    expect(resetExpiryTone(credit({ id: "gone", expiresAt: inHours(-1) }), NOW)).toBe("expired");
  });

  it("treats an unparseable date as unknown rather than expired", () => {
    expect(resetExpiryTone(credit({ id: "junk", expiresAt: "not a date" }), NOW)).toBe("unknown");
  });
});

describe("到期時間的兩種寫法", () => {
  it("says expires, never resets", () => {
    // 額度視窗會重置，Reset 券是到期作廢；同一組數字配錯動詞，意思相反。
    expect(expiresAtRef(inHours(30)).key).toBe("common.expiresAt");
  });

  it("counts down in the unit that is still readable", () => {
    expect(expiresInRef(inHours(20), NOW)?.key).toBe("common.expiresInHours");
    // 剛好跨過一天就換單位：「剩 30 小時」要人自己心算是不是明天。
    expect(expiresInRef(inHours(30), NOW)?.key).toBe("common.expiresInDays");
    expect(expiresInRef(inHours(100), NOW)?.key).toBe("common.expiresInDays");
    expect(expiresInRef(new Date(NOW + 90_000).toISOString(), NOW)?.key).toBe(
      "common.expiresInMinutes",
    );
    expect(expiresInRef(inHours(-2), NOW)?.key).toBe("common.expiredAlready");
  });

  it("never passes i18next a `count`, which would send it hunting for plural keys", () => {
    const ref = expiresInRef(inHours(100), NOW);
    expect(ref?.values).toEqual({ value: 4 });
  });

  it("has nothing to say without a date", () => {
    expect(expiresInRef(null, NOW)).toBeNull();
    expect(expiresAtRef(null).key).toBe("common.emDash");
  });
});
