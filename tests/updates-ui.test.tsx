import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import i18n from "i18next";
import "@/i18n";
import { UpdateCards } from "@/components/UpdateCards";
import { Rail } from "@/components/Rail";
import { api } from "@/lib/api";
import { attention, headline, type SystemStatus } from "@/lib/status";
import { vellumUpdateHosts, vellumUpdateState } from "@/lib/vellumUpdate";
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
    restartToApplyDesktopUpdate: vi.fn(),
    updateVellum: vi.fn(),
    restartToUpdate: vi.fn(),
    cancelScheduledRestart: vi.fn(),
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
  checkedAt: null,
  restartSchedule: null,
  ...patch,
});

/** A signed build with nothing going on. */
const live = (patch: Partial<UpdateStatusSnapshot> = {}): UpdateStatusSnapshot =>
  snapshot({
    liveAutoUpdate: true,
    desktop: layer("desktop", { liveAutoUpdate: true, phase: "idle", availableVersion: null, targetVersion: null }),
    core: layer("core", { liveAutoUpdate: true, phase: "idle", availableVersion: null, targetVersion: null }),
    remote: layer("remote", { liveAutoUpdate: true, phase: "idle", hosts: [] }),
    ...patch,
  });

const panel = (current: UpdateStatusSnapshot, onChanged: (next: UpdateStatusSnapshot) => void = () => undefined) =>
  render(<UpdateCards snapshot={current} onChanged={onChanged} onOpenHosts={() => undefined} />);

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

describe("one Vellum version over three layers", () => {
  it("cannot update itself without a signing key, whatever the layers say", () => {
    expect(vellumUpdateState(snapshot(), { checking: false })).toEqual({ kind: "unsigned" });
  });

  it("follows the desktop layer from available to ready", () => {
    const available = live({ desktop: layer("desktop", { liveAutoUpdate: true }) });
    expect(vellumUpdateState(available, { checking: false })).toEqual({ kind: "available", version: "0.3.0" });
    const ready = live({
      desktop: layer("desktop", { liveAutoUpdate: true, phase: "waitingForRestart", stagedVersion: "0.3.0" }),
    });
    expect(vellumUpdateState(ready, { checking: false })).toEqual({ kind: "ready", version: "0.3.0" });
  });

  it("adds core bytes to the download progress without showing a core version", () => {
    const state = vellumUpdateState(
      live({
        desktop: layer("desktop", { liveAutoUpdate: true, phase: "downloading", downloadBytes: 30, downloadTotal: 100 }),
        core: layer("core", {
          liveAutoUpdate: true,
          phase: "verifying",
          downloadBytes: 50,
          downloadTotal: 100,
          targetVersion: "0.150.0",
        }),
      }),
      { checking: false },
    );
    expect(state).toEqual({ kind: "downloading", version: "0.3.0", percent: 40 });
  });

  it("reports a core failure without pretending it is a Vellum version", () => {
    const state = vellumUpdateState(
      live({
        core: layer("core", { liveAutoUpdate: true, phase: "failed", failureReason: "pendingCoreRejectedBeforeLaunch" }),
      }),
      { checking: false },
    );
    expect(state).toEqual({ kind: "failed", version: null, reason: "pendingCoreRejectedBeforeLaunch" });
  });

  it("puts a scheduled restart ahead of everything but the signing key", () => {
    const state = vellumUpdateState(
      live({
        desktop: layer("desktop", { liveAutoUpdate: true, phase: "waitingForRestart", stagedVersion: "0.3.0" }),
        restartSchedule: { version: "0.3.0", openTurns: 2, activeRequests: 1, since: 1 },
      }),
      { checking: true },
    );
    expect(state).toEqual({ kind: "scheduled", version: "0.3.0", turns: 2, requests: 1 });
  });

  it("says when it last looked", () => {
    expect(vellumUpdateState(live({ checkedAt: 1_760_000_000 }), { checking: false })).toEqual({
      kind: "current",
      checkedAt: new Date(1_760_000_000_000).toISOString(),
    });
  });

  it("counts hosts that are behind or broken", () => {
    const host = (hostId: string, phase: LayerStatus["phase"]) => ({
      hostId,
      phase,
      currentVersion: "0.2.9",
      stagedVersion: null,
      idleAutoUpdate: true,
      failureReason: null,
    });
    expect(
      vellumUpdateHosts(
        layer("remote", { hosts: [host("a", "idle"), host("b", "waitingForIdle"), host("c", "failed")] }),
      ),
    ).toEqual({ total: 3, pending: 1, failed: 1 });
  });
});

describe("update panel", () => {
  it("points an unsigned build at the downloads page and locks its switches", async () => {
    await i18n.changeLanguage("en");
    panel(snapshot());
    expect(screen.getByRole("button", { name: "Open downloads" })).toBeTruthy();
    const switches = screen.getAllByRole("switch");
    expect(switches).toHaveLength(2);
    for (const toggle of switches) {
      expect((toggle as HTMLButtonElement).disabled).toBe(true);
    }
    expect(screen.queryByText(/Experimental/)).toBeNull();
  });

  it("is mounted globally instead of becoming a Settings card or real screen", () => {
    expect(settingsSource).not.toMatch(/UpdateCards/);
    expect(appSource).toMatch(/<UpdatePanel/);
    expect(appSource).toMatch(/onOpenUpdates/);
  });

  it("downloads during the check when automatic updates are on", async () => {
    await i18n.changeLanguage("en");
    vi.mocked(api.updateVellum).mockResolvedValue(live());
    panel(live());
    fireEvent.click(screen.getByRole("button", { name: "Check for updates" }));
    await waitFor(() => expect(api.updateVellum).toHaveBeenCalledTimes(1));
    expect(api.checkUpdates).not.toHaveBeenCalled();
  });

  it("only checks when automatic updates are off, and downloads on request", async () => {
    await i18n.changeLanguage("en");
    const manual = live({
      preferences: { channel: "stable", autoCheck: false, autoDownload: false, coreIdleHandoff: false },
    });
    vi.mocked(api.checkUpdates).mockResolvedValue(manual);
    vi.mocked(api.updateVellum).mockResolvedValue(manual);
    const view = panel(manual);
    fireEvent.click(screen.getByRole("button", { name: "Check for updates" }));
    await waitFor(() => expect(api.checkUpdates).toHaveBeenCalledTimes(1));
    expect(api.updateVellum).not.toHaveBeenCalled();

    view.rerender(
      <UpdateCards
        snapshot={{ ...manual, desktop: layer("desktop", { liveAutoUpdate: true }) }}
        onChanged={() => undefined}
        onOpenHosts={() => undefined}
      />,
    );
    fireEvent.click(await screen.findByRole("button", { name: "Download and update" }));
    await waitFor(() => expect(api.updateVellum).toHaveBeenCalledTimes(1));
  });

  it("turns both backend switches with the one automatic-updates switch", async () => {
    await i18n.changeLanguage("en");
    vi.mocked(api.setUpdatePreferences).mockResolvedValue({
      channel: "stable",
      autoCheck: false,
      autoDownload: false,
      coreIdleHandoff: false,
    });
    vi.mocked(api.getUpdateStatus).mockResolvedValue(live());
    panel(live());
    fireEvent.click(screen.getByRole("switch", { name: "Automatic updates" }));
    await waitFor(() =>
      expect(api.setUpdatePreferences).toHaveBeenCalledWith({ autoCheck: false, autoDownload: false }),
    );
  });

  it("schedules the restart while work is running and can take it back", async () => {
    await i18n.changeLanguage("en");
    const ready = live({
      desktop: layer("desktop", { liveAutoUpdate: true, phase: "waitingForRestart", stagedVersion: "0.3.0" }),
    });
    const scheduled = { ...ready, restartSchedule: { version: "0.3.0", openTurns: 1, activeRequests: 0, since: 1 } };
    vi.mocked(api.restartToUpdate).mockResolvedValue("scheduled");
    vi.mocked(api.getUpdateStatus).mockResolvedValue(scheduled);
    const changed = vi.fn();
    const view = panel(ready, changed);

    fireEvent.click(screen.getByRole("button", { name: "Restart and update now" }));
    await waitFor(() => expect(changed).toHaveBeenCalledWith(scheduled));
    // The old command refuses while a turn runs; the panel must not use it.
    expect(api.restartToApplyDesktopUpdate).not.toHaveBeenCalled();

    view.rerender(<UpdateCards snapshot={scheduled} onChanged={changed} onOpenHosts={() => undefined} />);
    expect(screen.getByTestId("vellum-update").textContent).toContain("1 Codex conversation");
    vi.mocked(api.cancelScheduledRestart).mockResolvedValue(true);
    fireEvent.click(await screen.findByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(api.cancelScheduledRestart).toHaveBeenCalledTimes(1));
  });

  it("explains a refused action instead of showing the reason code", async () => {
    await i18n.changeLanguage("en");
    vi.mocked(api.restartToUpdate).mockRejectedValueOnce(new Error("nothingStaged"));
    vi.mocked(api.getUpdateStatus).mockResolvedValue(live());
    panel(live({ desktop: layer("desktop", { liveAutoUpdate: true, phase: "staged", stagedVersion: "0.3.0" }) }));
    fireEvent.click(screen.getByRole("button", { name: "Restart and update now" }));
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("There is no downloaded update to install.");
    expect(alert.textContent).not.toContain("nothingStaged");
  });

  it("opens the release notes in place", async () => {
    await i18n.changeLanguage("en");
    panel(live({ desktop: layer("desktop", { liveAutoUpdate: true, releaseNotes: "Faster routing" }) }));
    expect(screen.queryByText("Faster routing")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Release notes" }));
    expect(screen.getByText("Faster routing")).toBeTruthy();
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
