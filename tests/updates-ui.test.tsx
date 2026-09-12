import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import i18n from "i18next";
import "@/i18n";
import { UpdateCards } from "@/components/UpdateCards";
import { api } from "@/lib/api";
import { attention, headline, type SystemStatus } from "@/lib/status";
import type { LayerStatus, Overview, ProxyStatus, RuntimeStatus, UpdateStatusSnapshot } from "@/types";
import statusBarSource from "../src/components/StatusBar.tsx?raw";
import settingsSource from "../src/screens/Settings.tsx?raw";

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

afterEach(cleanup);

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
  it("renders three layer cards with current, available, and apply condition", async () => {
    await i18n.changeLanguage("en");
    render(<UpdateCards snapshot={snapshot()} onChanged={() => undefined} />);
    expect(screen.getByTestId("update-layer-desktop")).toBeTruthy();
    expect(screen.getByTestId("update-layer-remote")).toBeTruthy();
    expect(screen.getByTestId("update-layer-core")).toBeTruthy();
    expect(screen.getByTestId("update-live-disabled").textContent).toMatch(/signing/i);
    expect(screen.getByTestId("update-layer-desktop").textContent).toContain("0.2.9");
    expect(screen.getByTestId("update-layer-desktop").textContent).toContain("0.3.0");
  });

  it("is mounted from Settings", () => {
    expect(settingsSource).toMatch(/UpdateCards/);
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
    expect(attention(withUpdates).settings).toBe(1);
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
