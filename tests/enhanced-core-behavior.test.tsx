import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import i18n from "i18next";
import "@/i18n";
import { EnhancedCore } from "@/screens/EnhancedCore";
import { api } from "@/lib/api";
import { EMPTY_OBSERVATIONS } from "@/lib/enhanced";

afterEach(() => { cleanup(); vi.restoreAllMocks(); });

describe("Enhanced Core observed state", () => {
  it("distinguishes a protected Official bridge from a failed degradation", async () => {
    await i18n.changeLanguage("en");
    const status = await api.getEnhancedDesktopRuntimeStatus();
    vi.spyOn(api, "getEnhancedRuntimeObservations").mockResolvedValue(EMPTY_OBSERVATIONS);
    const props = { refreshVersion: 0, onRefreshComplete: () => undefined };
    const missing = { ...status, coreAvailable: false, enhancedCore: "missing" as const, bridgePrepared: true, bridgeProtectionActive: true, ready: false, serving: false };
    const view = render(<EnhancedCore {...props} status={missing} />);
    expect(screen.getByText(i18n.t("bridgeFallback.active"))).toBeTruthy();
    expect(screen.queryByText(i18n.t("enhanced.verdict.serving"))).toBeNull();
    view.rerender(<EnhancedCore {...props} status={{ ...missing, bridgePrepared: false, bridgeProtectionActive: false }} />);
    expect(screen.getByText(i18n.t("bridgeFallback.failed"))).toBeTruthy();
  });
  it("explains a missing core and opens the update repair panel", async () => {
    await i18n.changeLanguage("en");
    const status = await api.getEnhancedDesktopRuntimeStatus();
    vi.spyOn(api, "getEnhancedRuntimeObservations").mockResolvedValue(EMPTY_OBSERVATIONS);
    const open = vi.fn();
    render(<EnhancedCore status={{ ...status, coreAvailable: false }} refreshVersion={0} onRefreshComplete={() => undefined} onOpenUpdates={open} />);
    expect(screen.getByText(i18n.t("settings.page.enhancedRuntime.inject.blocked.noCore"))).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: i18n.t("settings.page.updates.title") }));
    expect(open).toHaveBeenCalledOnce();
  });
  it("does not call a verified, armed artifact serving before adoption", async () => {
    const status = await api.getEnhancedDesktopRuntimeStatus();
    vi.spyOn(api, "getEnhancedRuntimeObservations").mockResolvedValue(EMPTY_OBSERVATIONS);
    const props = { refreshVersion: 0, onRefreshComplete: () => undefined };
    const armed = { ...status, enabled: true, artifactReady: true, environmentState: "leased" as const,
      bridgeObserved: false, active: false, ready: false, serving: false, launchCoreDrift: false };
    const view = render(<EnhancedCore {...props} status={armed} />);
    expect(screen.queryByText(i18n.t("enhanced.verdict.serving"))).toBeNull();
    expect(screen.getByText(i18n.t("enhanced.verdict.stoppedAt", { link: i18n.t("enhanced.chain.adopted") }))).toBeTruthy();
    expect(screen.queryByRole("switch")).toBeNull();
    expect(screen.queryByRole("textbox")).toBeNull();
    view.rerender(<EnhancedCore {...props} status={{ ...armed, bridgeObserved: true, active: true, ready: true, serving: true }} />);
    expect(screen.getByText(i18n.t("enhanced.verdict.serving"))).toBeTruthy();
    view.rerender(<EnhancedCore {...props} status={{ ...armed, bridgeObserved: true, serving: true }} />);
    expect(screen.getByText(i18n.t("enhanced.verdict.servingOtherLaunch"))).toBeTruthy();
  });
});
