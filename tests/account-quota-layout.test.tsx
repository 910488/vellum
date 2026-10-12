import { cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { AccountQuota } from "@/screens/Models";
import { accountQuotaWindows } from "@/lib/quota";
import type { QuotaSnapshot } from "@/types";

afterEach(cleanup);
const snapshot = (unit: "week" | "hour", usedPercent: number): QuotaSnapshot => ({
  routeId: "openai-official", usedPercent, period: { unit, amount: unit === "hour" ? 5 : null },
  resetAt: null, tier: null, stale: false,
});
const t = (key: string) => key;

describe("Official account quota slots", () => {
  it("renders only the real weekly window when five-hour quota is absent", () => {
    const { container } = render(<AccountQuota officialLayout windows={accountQuotaWindows([snapshot("week", 22)])} t={t} resetLabel={() => "later"} />);
    const slots = container.querySelectorAll(".acct__quota-window");
    expect(slots).toHaveLength(1);
    expect(slots[0]!.textContent).toContain("78%");
    expect(slots[0]!.classList.contains("acct__quota-window--lead")).toBe(true);
  });

  it("keeps two real windows and selects the binding one without adding a placeholder", () => {
    const { container } = render(<AccountQuota officialLayout windows={accountQuotaWindows([snapshot("week", 22), snapshot("hour", 90)])} t={t} resetLabel={() => "later"} />);
    expect(container.querySelectorAll(".acct__quota-window")).toHaveLength(2);
    expect(container.querySelector(".acct__quota-window--placeholder")).toBeNull();
    expect(container.querySelectorAll(".acct__quota-window")[0]!.textContent).toContain("78%");
    expect(container.querySelector(".acct__quota-window--lead")?.textContent).toContain("10%");
  });

  it("does not invent a five-hour window for other providers", () => {
    const { container } = render(<AccountQuota windows={accountQuotaWindows([snapshot("week", 22)])} t={t} resetLabel={() => "later"} />);
    expect(container.querySelectorAll(".acct__quota-window")).toHaveLength(1);
  });
});
