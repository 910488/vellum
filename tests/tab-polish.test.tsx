import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import i18n from "i18next";
import "@/i18n";
import { humanDuration } from "@/lib/humanDuration";
import { operationalErrorKey } from "@/lib/operationalError";
import { paginatePreviewLog } from "@/dev/paginatePreviewLog";
import { Notice } from "@/components/ui";
import { VellumUpdate, type VellumUpdateActions } from "@/components/VellumUpdate";
import type { RequestLog } from "@/types";

afterEach(cleanup);
describe("tab usability", () => {
  it("uses readable durations and actionable error summaries in every locale", () => {
    for (const locale of ["zh-TW", "zh-CN", "en", "ja"]) {
      const t = i18n.getFixedT(locale);
      expect(humanDuration(4_920_000, t)).toBe(t("tabPolish.time.hoursMinutes", { hours: 1, minutes: 22 }));
      expect(t(operationalErrorKey("Grok Build unavailable (HTTP 429)"))).not.toContain("Build unavailable");
      expect(operationalErrorKey("refresh token rejected: refresh_token_reused")).toBe("tabPolish.loginExpired");
    }
  });
  it("keeps raw diagnostics closed without losing the original error", () => {
    const { container } = render(<Notice raw="refresh_token_reused">Please sign in again</Notice>);
    expect(container.querySelector("details")?.open).toBe(false);
    expect(container.querySelector("pre")?.textContent).toBe("refresh_token_reused");
  });
  it("paginates the 420-row fixture and applies filters before slicing", () => {
    const log: RequestLog = { providers: [], entries: Array.from({ length: 420 }, (_, id) => ({
      id, routeId: id % 2 ? "b" : "a", provider: "Fixture", model: "model", inputTokens: 100,
      outputTokens: 10, status: id % 3 ? 200 : 503, error: null, durationMs: 100, firstByteMs: null, createdAt: id,
    })) };
    expect(paginatePreviewLog(log).entries).toHaveLength(20);
    expect(paginatePreviewLog(log, { offset: 400 }).entries[0]?.id).toBe(400);
    const filtered = paginatePreviewLog(log, { routeId: "a", failedOnly: true, limit: 10, offset: 10 });
    expect(filtered.entryTotal).toBe(70);
    expect(filtered.entries[0]?.id).toBe(60);
    expect(paginatePreviewLog(log, { focusEntryId: 41 }).entryOffset).toBe(40);
  });
  it("shows active and pending core versions, and offers repair for a missing core", async () => {
    await i18n.changeLanguage("en");
    const repair = vi.fn();
    const actions = Object.fromEntries(["onCheck", "onUpdate", "onCancel", "onRestart", "onUnschedule", "onRetry", "onOpenDownloads", "onAutoUpdate", "onEarlyAccess", "onOpenHosts"].map((key) => [key, vi.fn()])) as unknown as VellumUpdateActions;
    const view = { current: "0.3.1", state: { kind: "current" as const, checkedAt: null }, autoUpdate: false, earlyAccess: false, hosts: { total: 0, pending: 0, failed: 0 }, notes: null };
    const { rerender } = render(<VellumUpdate view={{ ...view, core: { activeVersion: "0.3.1", pendingVersion: "0.3.2", missing: false } }} actions={actions} />);
    expect(screen.getByText(/0.3.1.*0.3.2/)).toBeTruthy();
    rerender(<VellumUpdate view={{ ...view, core: { activeVersion: null, pendingVersion: null, missing: true } }} actions={{ ...actions, onRepairCore: repair }} />);
    fireEvent.click(screen.getByRole("button", { name: i18n.t("tabPolish.core.repair") }));
    expect(repair).toHaveBeenCalledOnce();
  });
});
