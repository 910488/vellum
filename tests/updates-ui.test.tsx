import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import i18n from "i18next";
import "@/i18n";
import { UpdateCards } from "@/components/UpdateCards";
import { Rail } from "@/components/Rail";
import { api } from "@/lib/api";
import { attention, headline, type SystemStatus } from "@/lib/status";
import type { LayerStatus, Overview, ProxyStatus, RuntimeStatus, UpdateStatusSnapshot } from "@/types";
import statusBarSource from "../src/components/StatusBar.tsx?raw";
import settingsSource from "../src/screens/Settings.tsx?raw";
import appSource from "../src/App.tsx?raw";

vi.mock("@/lib/api", () => ({
  api: {
    setUpdatePreferences: vi.fn(),
    getUpdateStatus: vi.fn(),
    checkUpdates: vi.fn(),
    downloadUpdate: vi.fn(),
    applyUpdate: vi.fn(),
    cancelUpdateDownload: vi.fn(),
    rollbackUpdate: vi.fn(),
    setRemoteUpdatePolicy: vi.fn(),
  },
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const layer = (component: LayerStatus["component"], patch: Partial<LayerStatus> = {}): LayerStatus => ({
  component,
  currentVersion: "0.2.9",
  availableVersion: "0.3.0",
  stagedVersion: null,
  channel: "stable",
  phase: "available",
  applyCondition: "download",
  releaseNotes: "notes",
  failureReason: null,
  operationId: "op",
  targetVersion: "0.3.0",
  downloadBytes: 0,
  downloadTotal: 1,
  liveAutoUpdate: false,
  hosts: component === "remote"
    ? [{ hostId: "host-1", phase: "waitingForIdle", currentVersion: "0.2.9", stagedVersion: "0.4.0", idleAutoUpdate: false, failureReason: null }]
    : [],
  ...patch,
});

const snapshot = (patch: Partial<UpdateStatusSnapshot> = {}): UpdateStatusSnapshot => ({
  desktop: layer("desktop"),
  remote: layer("remote"),
  core: layer("core", { applyCondition: "nextCoreStart", phase: "waitingForRestart", stagedVersion: "0.2.0" }),
  preferences: { channel: "stable", autoCheck: true, autoDownload: true, coreIdleHandoff: false },
  liveAutoUpdate: false,
  attention: "available",
  ...patch,
});

const proxy = (): ProxyStatus => ({
  running: true,
  baseUrl: "http://127.0.0.1:8118",
  catalogPath: null,
  codexManaged: true,
  lastError: null,
  notice: null,
});

const runtime = (patch: Partial<RuntimeStatus> = {}): RuntimeStatus => ({
  proxyRunning: true,
  codexManaged: true,
  activeRequests: 0,
  draining: false,
  restartRequired: false,
  restartReasons: [],
  liveApplied: [],
  activeCatalogVersion: null,
  ...patch,
});

const overview = (): Overview => ({
  route: null,
  lastSuccessfulRoute: null,
  quota: null,
  usage: { usedTokens: 0, windowTokens: 1, turns: 0, providerTotalTokens: 0, trend: [], compacted: false },
  health: { pooled: true, connections: 1, firstByteMs: null, reasoningVisible: true, historyRetentionDays: 30 },
  findings: [],
});

const status = (patch: Partial<SystemStatus> = {}): SystemStatus => ({
  proxy: proxy(),
  runtime: runtime(),
  overview: overview(),
  updates: snapshot(),
  ...patch,
});

describe("update settings cards", () => {
  it("renders one primary update action and keeps component controls experimental", async () => {
    await i18n.changeLanguage("en");
    render(<UpdateCards snapshot={snapshot()} onChanged={() => undefined} />);
    expect(screen.getByTestId("update-layer-desktop")).toBeTruthy();
    expect(screen.getByTestId("update-layer-remote")).toBeTruthy();
    expect(screen.getByTestId("update-layer-core")).toBeTruthy();
    expect(screen.getByTestId("update-live-disabled").textContent).toMatch(/signing/i);
    expect(screen.getByRole("progressbar")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Update all" })).toBeTruthy();
    expect(screen.getByText("Experimental: component updates")).toBeTruthy();
    expect((screen.getByText("Experimental: component updates").closest("details") as HTMLDetailsElement).open).toBe(false);
    expect(screen.getByTestId("update-layer-desktop").textContent).toContain("0.2.9");
    expect(screen.getByTestId("update-layer-desktop").textContent).toContain("0.3.0");
  });

  it("is mounted globally instead of becoming a Settings card or real screen", () => {
    expect(settingsSource).not.toMatch(/UpdateCards/);
    expect(appSource).toMatch(/<UpdatePanel/);
    expect(appSource).toMatch(/onOpenUpdates/);
  });

  it("checks, downloads, and schedules every live component from Update all", async () => {
    await i18n.changeLanguage("en");
    let current = snapshot({
      liveAutoUpdate: true,
      desktop: layer("desktop", { liveAutoUpdate: true }),
      remote: layer("remote", { liveAutoUpdate: true }),
      core: layer("core", { liveAutoUpdate: true, phase: "available" }),
    });
    vi.mocked(api.checkUpdates).mockImplementation(async () => current);
    vi.mocked(api.getUpdateStatus).mockImplementation(async () => current);
    vi.mocked(api.downloadUpdate).mockImplementation(async (component) => {
      current = { ...current, [component]: { ...current[component], phase: "staged" } };
      return { operationId: `download-${component}`, component, phase: "staged", targetVersion: "0.3.0" };
    });
    vi.mocked(api.applyUpdate).mockImplementation(async (component) => {
      current = { ...current, [component]: { ...current[component], phase: "applied" } };
      return { operationId: `apply-${component}`, component, phase: "applied", targetVersion: "0.3.0" };
    });

    render(<UpdateCards snapshot={current} onChanged={(next) => { current = next; }} />);
    fireEvent.click(screen.getByRole("button", { name: "Update all" }));

    await waitFor(() => expect(api.downloadUpdate).toHaveBeenCalledTimes(3));
    expect(api.applyUpdate).toHaveBeenCalledWith("desktop");
    expect(api.applyUpdate).toHaveBeenCalledWith("remote", "host-1");
    expect(api.applyUpdate).toHaveBeenCalledWith("core");
  });

  it("continues the remaining components when one update fails", async () => {
    await i18n.changeLanguage("en");
    let current = snapshot({
      liveAutoUpdate: true,
      desktop: layer("desktop", { liveAutoUpdate: true }),
      remote: layer("remote", { liveAutoUpdate: true }),
      core: layer("core", { liveAutoUpdate: true, phase: "available" }),
    });
    vi.mocked(api.checkUpdates).mockImplementation(async () => current);
    vi.mocked(api.getUpdateStatus).mockImplementation(async () => current);
    vi.mocked(api.downloadUpdate).mockImplementation(async (component) => {
      if (component === "desktop") throw new Error("desktop mirror unavailable");
      current = { ...current, [component]: { ...current[component], phase: "staged" } };
      return { operationId: `download-${component}`, component, phase: "staged", targetVersion: "0.3.0" };
    });
    vi.mocked(api.applyUpdate).mockImplementation(async (component) => ({
      operationId: `apply-${component}`, component, phase: "applied", targetVersion: "0.3.0",
    }));

    render(<UpdateCards snapshot={current} onChanged={(next) => { current = next; }} />);
    fireEvent.click(screen.getByRole("button", { name: "Update all" }));

    await waitFor(() => expect(api.downloadUpdate).toHaveBeenCalledTimes(3));
    expect(api.applyUpdate).toHaveBeenCalledWith("remote", "host-1");
    expect(api.applyUpdate).toHaveBeenCalledWith("core");
    expect((await screen.findByRole("alert")).textContent).toContain("desktop mirror unavailable");
  });

  it("opens updates from a tab-like rail action without navigating", async () => {
    await i18n.changeLanguage("en");
    const navigate = vi.fn();
    const openUpdates = vi.fn();
    render(
      <Rail
        active="today"
        onNavigate={navigate}
        onOpenUpdates={openUpdates}
        attention={{}}
        updateAttention
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: /Updates/ }));
    expect(openUpdates).toHaveBeenCalledTimes(1);
    expect(navigate).not.toHaveBeenCalled();
  });

  it("shows command failures instead of leaving an unhandled rejection", async () => {
    vi.mocked(api.checkUpdates).mockRejectedValueOnce(new Error("network unavailable"));
    const view = render(<UpdateCards snapshot={snapshot({ liveAutoUpdate: true, desktop: layer("desktop", { liveAutoUpdate: true }) })} onChanged={() => undefined} />);
    fireEvent.click(within(view.getByTestId("update-layer-desktop")).getByText("Check for updates"));
    await waitFor(() => expect(view.getByRole("alert").textContent).toContain("network unavailable"));
  });
});

describe("update attention vs models badge", () => {
  it("header states distinguish available, idle, restart, and failed", () => {
    expect(headline(status({ updates: snapshot({ attention: "available" }) })).updateAttention).toBe("available");
    expect(headline(status({ updates: snapshot({ attention: "waitingIdle" }) })).updateAttention).toBe("waitingIdle");
    expect(headline(status({ updates: snapshot({ attention: "waitingRestart" }) })).updateAttention).toBe("waitingRestart");
    expect(headline(status({ updates: snapshot({ attention: "failed" }) })).updateAttention).toBe("failed");
    expect(statusBarSource).toMatch(/status\.updateAvailable/);
    expect(statusBarSource).toMatch(/status\.updateWaitingIdle/);
    expect(statusBarSource).toMatch(/status\.updateWaitingRestart/);
    expect(statusBarSource).toMatch(/status\.updateFailed/);
  });

  it("does not increment models for desktop or remote update-ready reasons", () => {
    const withUpdates = status({
      runtime: runtime({
        restartRequired: true,
        restartReasons: [{ code: "desktopUpdateReady", params: {} }, { code: "remoteUpdateReady", params: {} }],
      }),
      updates: snapshot({ attention: "waitingRestart" }),
    });
    expect(attention(withUpdates).models).toBeUndefined();
    expect(attention(withUpdates).settings).toBeUndefined();
  });

  it("still badges models for catalog/runtime apply reasons", () => {
    const needsRestart = status({
      runtime: runtime({ restartRequired: true, restartReasons: [{ code: "catalogRestored", params: {} }] }),
      updates: snapshot({ attention: "none" }),
    });
    expect(attention(needsRestart).models).toBe(1);
    expect(attention(needsRestart).settings).toBeUndefined();
  });
});
