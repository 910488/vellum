import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import i18n from "i18next";
import "@/i18n";
import { App } from "@/App";
import { shouldApplyProxyLifecycle } from "@/lib/proxyLifecycle";
import { Today } from "@/screens/Today";
import type { EnhancedDesktopRuntimeStatus, Overview, ProxyStatus, RuntimeStatus } from "@/types";

const apiMocks = vi.hoisted(() => ({
  startProxy: vi.fn(),
  stopProxyAndRestore: vi.fn(),
  getProviderOverviews: vi.fn(),
  listRoutes: vi.fn(),
  listSessions: vi.fn(),
  listModelRoutes: vi.fn(),
  getContextBudget: vi.fn(),
  refreshQuota: vi.fn(),
  getProxyStatus: vi.fn(),
  getRuntimeStatus: vi.fn(),
  getEnhancedDesktopRuntimeStatus: vi.fn(),
  getOverview: vi.fn(),
  getUpdateStatus: vi.fn(),
  subscribeProxyLifecycle: vi.fn(),
}));

vi.mock("@/lib/api", () => ({ api: apiMocks }));

const stopped: ProxyStatus = {
  running: false,
  baseUrl: "http://127.0.0.1:15721/v1",
  catalogPath: null,
  codexManaged: false,
  lastError: null,
  notice: null,
  phase: "stopped",
  operationId: null,
  generation: 0,
  stage: null,
  stageElapsedMs: null,
};

const running: ProxyStatus = {
  ...stopped,
  running: true,
  catalogPath: "catalog.json",
  codexManaged: true,
  phase: "running",
  generation: 1,
  operationId: "op_1",
};

const overview: Overview = {
  route: null,
  lastSuccessfulRoute: null,
  quota: null,
  usage: {
    usedTokens: 0,
    windowTokens: 128000,
    turns: 0,
    providerTotalTokens: 0,
    trend: [],
    compacted: false,
  },
  health: {
    pooled: true,
    connections: 0,
    firstByteMs: null,
    reasoningVisible: true,
    historyRetentionDays: 30,
  },
  findings: [],
};

describe("proxy lifecycle button state", () => {
  beforeAll(async () => {
    await i18n.changeLanguage("en");
  });

  beforeEach(() => {
    apiMocks.getProviderOverviews.mockResolvedValue([]);
    apiMocks.listRoutes.mockResolvedValue([]);
    apiMocks.listSessions.mockResolvedValue([]);
    apiMocks.listModelRoutes.mockResolvedValue([]);
    apiMocks.startProxy.mockResolvedValue(running);
    apiMocks.stopProxyAndRestore.mockResolvedValue(stopped);
    apiMocks.subscribeProxyLifecycle.mockResolvedValue(() => undefined);
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it("applies a lifecycle event on Today without polling getProxyStatus", async () => {
    let emitLifecycle: ((status: ProxyStatus) => void) | null = null;
    apiMocks.subscribeProxyLifecycle.mockImplementation(async (onStatus: (status: ProxyStatus) => void) => {
      emitLifecycle = onStatus;
      return () => {
        emitLifecycle = null;
      };
    });
    const onProxyChanged = vi.fn();
    render(
      <Today
        proxy={stopped}
        overview={overview}
        onNavigate={() => {}}
        onChanged={() => {}}
        onProxyChanged={onProxyChanged}
        refreshVersion={0}
        onRefreshComplete={() => {}}
        active
      />,
    );
    await waitFor(() => {
      if (!emitLifecycle) throw new Error("Today did not subscribe to lifecycle events");
    });
    emitLifecycle!({
      ...stopped,
      phase: "preparing",
      generation: 1,
      operationId: "op_1",
      stage: "discovery",
      stageElapsedMs: 9,
    });
    await waitFor(() => {
      const next = onProxyChanged.mock.calls[0]?.[0] as ProxyStatus | undefined;
      expect(next?.phase).toBe("preparing");
      expect(next?.stage).toBe("discovery");
      expect(next?.generation).toBe(1);
    });
    expect(apiMocks.getProxyStatus).not.toHaveBeenCalled();
  });

  it("commits the start command result on one click", async () => {
    const onProxyChanged = vi.fn();
    render(
      <Today
        proxy={stopped}
        overview={overview}
        onNavigate={() => {}}
        onChanged={() => {}}
        onProxyChanged={onProxyChanged}
        refreshVersion={0}
        onRefreshComplete={() => {}}
        active
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: i18n.t("today.proxy.start") }));
    await waitFor(() => expect(onProxyChanged).toHaveBeenCalledWith(running));
    expect(apiMocks.startProxy).toHaveBeenCalledTimes(1);
  });

  it("does not report a successful start as failed when dashboard refresh fails", async () => {
    apiMocks.getProviderOverviews.mockRejectedValue(new Error("overview down"));
    const onProxyChanged = vi.fn();
    render(
      <Today
        proxy={stopped}
        overview={overview}
        onNavigate={() => {}}
        onChanged={() => {}}
        onProxyChanged={onProxyChanged}
        refreshVersion={0}
        onRefreshComplete={() => {}}
        active
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: i18n.t("today.proxy.start") }));
    await waitFor(() => expect(onProxyChanged).toHaveBeenCalledWith(running));
    expect(await screen.findByText(/failed to refresh/i)).toBeTruthy();
  });

  it("does not present the selected default model as one the user has used", async () => {
    render(
      <Today
        proxy={running}
        overview={{
          ...overview,
          route: {
            id: "opencode-zen",
            name: "OpenCode Zen",
            baseUrl: "https://opencode.ai/zen/v1",
            model: "muse-spark-1.3-contributor-free",
            wire: "responses",
            isCurrent: true,
            serverSideResume: false,
            streaming: true,
            reasoning: false,
            providerKind: "openAiCompatible",
            authKind: "bearer",
            enabled: true,
            models: ["muse-spark-1.3-contributor-free"],
            selectedModels: ["muse-spark-1.3-contributor-free"],
            contextWindow: null,
            modelCapabilities: [],
          },
        }}
        onNavigate={() => {}}
        onChanged={() => {}}
        onProxyChanged={() => {}}
        refreshVersion={0}
        onRefreshComplete={() => {}}
        active
      />,
    );

    expect(screen.getByRole("heading", { name: "No successful requests yet" })).toBeTruthy();
    expect(screen.queryByRole("heading", { name: /muse-spark/i })).toBeNull();
  });

  it("presents the provider and model from the latest successful request", async () => {
    render(
      <Today
        proxy={running}
        overview={{
          ...overview,
          lastSuccessfulRoute: {
            routeId: "used-route",
            provider: "Actually used",
            model: "used-model",
            createdAt: 1_722_222_222,
          },
        }}
        onNavigate={() => {}}
        onChanged={() => {}}
        onProxyChanged={() => {}}
        refreshVersion={0}
        onRefreshComplete={() => {}}
        active
      />,
    );

    expect(screen.getByRole("heading", { name: "used-model · Actually used" })).toBeTruthy();
  });

  it("keeps the running boolean next to the phase enum", () => {
    expect(running.running).toBe(true);
    expect(running.phase).toBe("running");
    expect(stopped.running).toBe(false);
    expect(stopped.phase).toBe("stopped");
  });
});

describe("proxy lifecycle generation filter", () => {
  it("applies the same or newer generation and ignores stale events", () => {
    expect(shouldApplyProxyLifecycle(null, stopped)).toBe(true);
    expect(shouldApplyProxyLifecycle(stopped, { ...stopped, generation: 1, phase: "preparing" })).toBe(
      true,
    );
    expect(
      shouldApplyProxyLifecycle(
        { ...stopped, generation: 2, phase: "stopping" },
        { ...stopped, generation: 1, phase: "preparing", stage: "discovery" },
      ),
    ).toBe(false);
  });
});

const runtimeStopped: RuntimeStatus = {
  proxyRunning: false,
  codexManaged: false,
  activeRequests: 0,
  draining: false,
  restartRequired: false,
  restartReasons: [],
  liveApplied: [],
  activeCatalogVersion: null,
};

const enhancedIdle: EnhancedDesktopRuntimeStatus = {
  configured: false,
  enabled: false,
  artifactReady: false,
  active: false,
  bridgeObserved: false,
  ready: false,
  activationState: "disabled",
  environmentState: "released",
  environmentValue: null,
  observedBridgeExecutable: null,
  restartRequired: false,
  launchId: null,
  bridgeState: null,
  bridgePid: null,
  officialChildPid: null,
  enhancedChildPid: null,
  enhancedRuntimeDigest: null,
  activeRuntimeDigest: null,
  activeFeatureProfile: null,
  officialCodexExecutable: null,
  enhancedCodexExecutable: null,
  coreAvailable: false,
  missingHelpers: [],
  launchCoreDrift: false,
  bridgeExecutable: null,
  protocol: null,
  serving: false,
  unverified: false,
  lastQualification: null,
  blockers: [],
};

describe("proxy lifecycle events reach the UI without a poll", () => {
  let emitLifecycle: ((status: ProxyStatus) => void) | null = null;

  beforeAll(async () => {
    await i18n.changeLanguage("en");
  });

  beforeEach(() => {
    emitLifecycle = null;
    apiMocks.getProviderOverviews.mockResolvedValue([]);
    apiMocks.listRoutes.mockResolvedValue([]);
    apiMocks.listSessions.mockResolvedValue([]);
    apiMocks.listModelRoutes.mockResolvedValue([]);
    apiMocks.getContextBudget.mockResolvedValue(null);
    apiMocks.getProxyStatus.mockResolvedValue(stopped);
    apiMocks.getRuntimeStatus.mockResolvedValue(runtimeStopped);
    apiMocks.getEnhancedDesktopRuntimeStatus.mockResolvedValue(enhancedIdle);
    apiMocks.getOverview.mockResolvedValue(overview);
    apiMocks.getUpdateStatus.mockResolvedValue({
      desktop: { component: "desktop", currentVersion: "0.2.9", availableVersion: null, stagedVersion: null, channel: "stable", phase: "idle", applyCondition: "idle", releaseNotes: null, failureReason: null, operationId: null, targetVersion: null, downloadBytes: 0, downloadTotal: 0, liveAutoUpdate: false, hosts: [] },
      remote: { component: "remote", currentVersion: "0.2.9", availableVersion: null, stagedVersion: null, channel: "stable", phase: "idle", applyCondition: "idle", releaseNotes: null, failureReason: null, operationId: null, targetVersion: null, downloadBytes: 0, downloadTotal: 0, liveAutoUpdate: false, hosts: [] },
      core: { component: "core", currentVersion: "bundled", availableVersion: null, stagedVersion: null, channel: "stable", phase: "idle", applyCondition: "idle", releaseNotes: null, failureReason: null, operationId: null, targetVersion: null, downloadBytes: 0, downloadTotal: 0, liveAutoUpdate: false, hosts: [] },
      preferences: { channel: "stable", autoCheck: true, autoDownload: true, coreIdleHandoff: false },
      liveAutoUpdate: false,
      attention: "none",
    });
    apiMocks.startProxy.mockResolvedValue(running);
    apiMocks.stopProxyAndRestore.mockResolvedValue(stopped);
    apiMocks.subscribeProxyLifecycle.mockImplementation(async (onStatus: (status: ProxyStatus) => void) => {
      emitLifecycle = onStatus;
      return () => {
        emitLifecycle = null;
      };
    });
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it("shows preparing stages from events while the start command is still in flight", async () => {
    let resolveStart: (status: ProxyStatus) => void = () => undefined;
    apiMocks.startProxy.mockImplementation(
      () =>
        new Promise<ProxyStatus>((resolve) => {
          resolveStart = resolve;
        }),
    );
    render(<App />);
    const start = await screen.findByRole("button", { name: i18n.t("today.proxy.start") });
    await waitFor(() => expect(emitLifecycle).not.toBeNull());
    const polls = apiMocks.getProxyStatus.mock.calls.length;
    fireEvent.click(start);
    await waitFor(() => expect(apiMocks.startProxy).toHaveBeenCalledTimes(1));

    emitLifecycle?.({
      ...stopped,
      phase: "preparing",
      generation: 1,
      operationId: "op_1",
      stage: "discovery",
      stageElapsedMs: 8,
    });
    expect(await screen.findByText(/discovery/)).toBeTruthy();
    expect(screen.getByText(/8ms/)).toBeTruthy();
    expect(apiMocks.getProxyStatus.mock.calls.length).toBe(polls);
    expect(screen.queryByRole("button", { name: i18n.t("today.proxy.stopRestore") })).toBeNull();

    emitLifecycle?.({
      ...stopped,
      running: false,
      phase: "starting",
      generation: 1,
      operationId: "op_1",
      stage: "readiness",
      stageElapsedMs: 15,
    });
    expect(await screen.findByText(/readiness/)).toBeTruthy();
    resolveStart(running);
    await waitFor(() =>
      expect(screen.getByRole("button", { name: i18n.t("today.proxy.stopRestore") })).toBeTruthy(),
    );
  });

  it("updates preparing stage from an event without calling getProxyStatus again", async () => {
    render(<App />);
    await screen.findByRole("button", { name: i18n.t("today.proxy.start") });
    await waitFor(() => expect(emitLifecycle).not.toBeNull());
    const polls = apiMocks.getProxyStatus.mock.calls.length;
    expect(polls).toBeGreaterThan(0);

    emitLifecycle?.({
      ...stopped,
      phase: "preparing",
      generation: 1,
      operationId: "op_1",
      stage: "discovery",
      stageElapsedMs: 12,
    });

    expect(await screen.findByText(/discovery/)).toBeTruthy();
    expect(screen.getByText(/12ms/)).toBeTruthy();
    expect(apiMocks.getProxyStatus.mock.calls.length).toBe(polls);
  });

  it("ignores a superseded generation event", async () => {
    render(<App />);
    await screen.findByRole("button", { name: i18n.t("today.proxy.start") });
    await waitFor(() => expect(emitLifecycle).not.toBeNull());

    emitLifecycle?.({
      ...stopped,
      phase: "starting",
      generation: 2,
      operationId: "op_2",
      stage: "bind",
      stageElapsedMs: 4,
    });
    expect(await screen.findByText(/bind/)).toBeTruthy();

    emitLifecycle?.({
      ...stopped,
      phase: "preparing",
      generation: 1,
      operationId: "op_1",
      stage: "discovery",
      stageElapsedMs: 99,
    });

    expect(screen.getByText(/bind/)).toBeTruthy();
    expect(screen.queryByText(/discovery/)).toBeNull();
  });
});
