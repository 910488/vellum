import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import i18n from "i18next";
import "@/i18n";
import { EnhancedCore } from "@/screens/EnhancedCore";
import { api } from "@/lib/api";
import { EMPTY_OBSERVATIONS } from "@/lib/enhanced";

afterEach(() => { cleanup(); vi.restoreAllMocks(); });

describe("Enhanced Core observed state", () => {
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
