import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import i18n from "i18next";
import "@/i18n";
import { RemoteDeployHistory } from "@/components/RemoteDeployHistory";
const api = vi.hoisted(() => ({ getRemoteDeploySummary: vi.fn() }));
vi.mock("@/lib/api", () => ({ api }));
beforeEach(() => { vi.resetAllMocks(); });
afterEach(cleanup);
describe("local deployment evidence", () => {
  it.each(["zh-TW", "zh-CN", "ja", "en"])("shows counts and a translated failure without a chart or card in %s", async (locale) => {
    await i18n.changeLanguage(locale);
    api.getRemoteDeploySummary.mockResolvedValue({ total: 12, succeeded: 9, failed: 2, interrupted: 1, blocked: 3, lastFailureCode: "proxyPortConflict" });
    const view = render(<RemoteDeployHistory hostId="offline" revision={1} />);
    const line = await screen.findByLabelText(i18n.t("remoteHistory.label"));
    expect(line.textContent).toContain(i18n.t("remoteHistory.counts", { count: 12, succeeded: 9, failed: 2, interrupted: 1 }));
    expect(line.textContent).toContain(i18n.t("remoteHistory.blocked", { count: 3 }));
    expect(line.textContent).toContain(i18n.t("remote.blocker.proxyPortConflict"));
    expect(line.textContent).toContain("proxyPortConflict");
    expect(line.tagName).toBe("P");
    expect(view.container.querySelector(".card,svg,canvas")).toBeNull();
    view.rerender(<RemoteDeployHistory hostId="offline" revision={2} />);
    await waitFor(() => expect(api.getRemoteDeploySummary).toHaveBeenCalledTimes(2));
  });
  it("ignores a delayed previous-host reply and limits local read errors to its own line", async () => {
    await i18n.changeLanguage("en");
    let resolve!: (value: unknown) => void;
    api.getRemoteDeploySummary.mockReturnValueOnce(new Promise((done) => { resolve = done; })).mockRejectedValueOnce(new Error("local file unreadable"));
    const view = render(<RemoteDeployHistory hostId="old" revision={1} />);
    view.rerender(<RemoteDeployHistory hostId="new" revision={1} />);
    expect(await screen.findByText(i18n.t("remoteHistory.unavailable"))).toBeTruthy();
    resolve({ total: 999, succeeded: 999 });
    await waitFor(() => expect(screen.queryByText(/999/)).toBeNull());
    expect(screen.queryByRole("alert")).toBeNull();
  });
});
