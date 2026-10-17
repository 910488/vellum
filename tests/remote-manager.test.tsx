import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import i18n from "i18next";
// Remote 這一頁本身不 import i18n（App 進入點負責初始化），所以測試要自己接。
import "@/i18n";
import { Remote } from "@/screens/Remote";
import type { RemoteHostCandidate, RemoteHostStatus, RemoteReleaseStatus } from "@/types";
import apiSource from "../src/lib/api.ts?raw";
import rustSource from "../src-tauri/src/lib.rs?raw";

const apiMocks = vi.hoisted(() => ({
  listRemoteHandoffThreads: vi.fn(),
  listRemoteHandoffs: vi.fn(),
  getRemoteHandoffThreadLink: vi.fn(),
  openRemoteHandoffThread: vi.fn(),
  pollRemoteHandoff: vi.fn(),
  getRemoteManagerFeatureFlags: vi.fn(),
  getRemoteReleaseStatus: vi.fn(),
  discoverRemoteConnections: vi.fn(),
  addRemoteSshHost: vi.fn(),
  inspectRemoteHost: vi.fn(),
  getDesktopCodexCompatibility: vi.fn(),
  updateRemoteCodexForDesktop: vi.fn(),
  getRemoteSessionSummary: vi.fn(),
  bootstrapRemoteHost: vi.fn(),
  listRemoteHostModels: vi.fn(),
  updateRemoteHostModels: vi.fn(),
  restoreRemoteHost: vi.fn(),
  getRemoteDeploySummary: vi.fn(),
  createRemoteSupportBundle: vi.fn(),
  getRemoteOperation: vi.fn(),
  stopRemoteAppOwnedCodex: vi.fn(),
  setActiveRemoteHost: vi.fn(),
  listRemoteCodexAccountPairings: vi.fn(),
  startRemoteCodexAccountLogin: vi.fn(),
  pollRemoteCodexAccountLogin: vi.fn(),
  activateRemoteCodexAccount: vi.fn(),
  listRemoteOfficialExecutionAccounts: vi.fn(),
  startRemoteOfficialExecutionAccountLogin: vi.fn(),
  pollRemoteOfficialExecutionAccountLogin: vi.fn(),
  selectRemoteOfficialExecutionAccount: vi.fn(),
  clearRemoteOfficialExecutionAccountSelection: vi.fn(),
  removeRemoteOfficialExecutionAccount: vi.fn(),
  startRemoteControlPairing: vi.fn(),
  planRemoteDeployment: vi.fn(),
  reapplyRemoteDeployment: vi.fn(),
  startRemoteGrokLogin: vi.fn(),
  pollRemoteGrokLogin: vi.fn(),
  cancelRemoteGrokLogin: vi.fn(),
  refreshRemoteGrokLogin: vi.fn(),
  getRemoteSshTrustStatus: vi.fn(),
  fetchRemoteSshFingerprint: vi.fn(),
  confirmRemoteSshFingerprint: vi.fn(),
}));

vi.mock("@/lib/api", () => ({ api: apiMocks }));

const host = {
  vellumHostId: "jetson",
  displayName: "Jetson",
  sshAlias: "Jetson",
  hostname: "192.0.2.10",
  user: "operator",
  port: 22,
  source: "codexApp",
  validated: true,
  validationError: null,
} as RemoteHostCandidate;

const status = {
  hostId: "jetson",
  managerState: "nativeActive",
  cachedHost: { id: "jetson", name: "Jetson", sshAlias: "Jetson" },
  agent: {
    agentVersion: "0.1.2",
    agentProtocol: 1,
    capabilities: {
      os: "linux", arch: "aarch64", dockerAvailable: true, dockerMode: "rootless",
      rootlessDocker: true, userSystemdAvailable: true, lingerEnabled: true,
      codexBinary: "/usr/local/bin/codex", codexVersion: "0.147.0",
    },
    proxy: { present: true, running: true, ready: true, image: "proxy", imageDigest: "sha256:abc", configHash: "cfg", lastError: null },
    configuration: {
      present: true, state: "current", schemaVersion: 2, requiresReconfigure: false, issue: null,
      configHash: "cfg", credentialRefs: [], credentialsReady: true,
    },
    nativeCodex: {
      codexHome: "/home/vellum-test/.codex", codexBinary: "/usr/local/bin/codex", codexVersion: "0.147.0",
      compatible: true, compatibilityReason: null, daemonRunning: true, daemonPid: 4242,
      daemonVersion: "0.147.0", daemonOwner: "codexCliDaemon", restartSafe: true, durable: true,
      standaloneInstalled: true,
      cliLauncher: { path: "/home/vellum-test/.local/bin/codex", target: "/home/vellum-test/.codex/packages/standalone/current/codex", ready: true },
      remoteControlEnabled: true, activeTurn: true,
      sessionAuthority: "codexNativeDaemon", brokerEnabled: false,
    },
    grok: null,
  },
  agentError: null,
  availableActions: ["installCodex", "updateComponents", "repair", "exportSupportBundle"],
  blockedReasons: [],
  inventory: {
    hostId: "jetson", agentVersion: "0.1.2", agentProtocol: 1,
    system: { os: "linux", arch: "aarch64", hostname: "jetson", cpuCores: 8, memoryBytes: 8_000_000_000, diskTotalBytes: 64_000_000_000, diskFreeBytes: 20_000_000_000 },
    docker: { available: true, mode: "rootless", rootless: true, daemon: "running", serverVersion: "28.2.2", clientVersion: "28.2.2", context: "default", userInDockerGroup: true },
    codex: { binary: "/usr/local/bin/codex", version: "0.147.0", source: "path", codexHome: "/home/vellum-test/.codex", standaloneInstalled: true, appCliDiscoverable: true, appCliPath: "/home/vellum-test/.local/bin/codex", appServerSupported: true, compatible: true, compatibilityReason: null },
    blockers: [{ code: "credentialMissing", message: "credential cc is unavailable", repairable: true }],
    availableActions: ["installCodex", "updateComponents"],
  },
  chatgpt: {
    accountId: "acct-windows",
    expectedAccountId: "acct-windows",
    state: "synchronized",
    paired: true,
    active: true,
    loginPending: false,
    loginId: null,
  },
} as RemoteHostStatus;

const blockedRelease: RemoteReleaseStatus = {
  ready: false,
  trust: "bundled",
  releaseVersion: null,
  codexVersion: null,
  agentVersion: null,
  brokerVersion: null,
  proxyImage: null,
  proxyDigest: null,
  detail: "ReleaseManifestSignatureMissing",
};

function renderRemote() {
  return render(<Remote refreshVersion={0} onRefreshComplete={() => {}} />);
}

/** A loaded host has a redeploy action; unrelated tests need no verdict copy. */
async function inspectedWorkSurface() {
  const region = await screen.findByRole("region", { name: i18n.t("remoteWork.label") });
  await within(region).findByRole("button", { name: i18n.t("remoteWork.redeploy") });
  return within(region);
}

/** 維護動作收在 <details> 裡。測試要先把它打開，就跟使用者一樣。 */
function openTray(label: string | RegExp) {
  const summary = screen.getByText(label);
  const advanced = summary.closest<HTMLDetailsElement>(".remote-advanced");
  if (advanced && !advanced.open && summary !== advanced.querySelector("summary")) fireEvent.click(advanced.querySelector("summary")!);
  fireEvent.click(summary);
}

/** 目前開著的確認視窗。 */
function dialog() {
  const node = document.querySelector("dialog[open]");
  if (!node) throw new Error("no dialog is open");
  return within(node as HTMLElement);
}

describe("Remote Manager operations UI", () => {
  beforeAll(async () => {
    await i18n.changeLanguage("zh-TW");
  });

  beforeEach(() => {
    apiMocks.getRemoteDeploySummary.mockResolvedValue({ total: 0, succeeded: 0, failed: 0, interrupted: 0, blocked: 0, lastFailureCode: null });
    apiMocks.listRemoteHostModels.mockResolvedValue([
      { catalogId: "gpt-5.6-luna", displayName: "5.6 Luna", routeId: "official", upstreamModel: "gpt-5.6-luna", selected: true, mandatory: true },
      { catalogId: "vlm-third-party", displayName: "DeepSeek V4 Flash", routeId: "cc", upstreamModel: "deepseek-v4-flash", selected: true, mandatory: false },
    ]);
    apiMocks.updateRemoteHostModels.mockImplementation(async (hostId: string) => ({
      operationId: "models-update", hostId, kind: "oneClickBootstrap", phase: "waitingForIdle", percent: 15,
      state: "running", message: null, result: null, updatedAt: "2026-10-16T00:00:00Z",
    }));
    apiMocks.listRemoteHandoffThreads.mockResolvedValue([]);
    apiMocks.listRemoteHandoffs.mockResolvedValue([]);
    apiMocks.getRemoteHandoffThreadLink.mockResolvedValue(null);
    apiMocks.openRemoteHandoffThread.mockResolvedValue(undefined);
    apiMocks.setActiveRemoteHost.mockResolvedValue(undefined);
    apiMocks.listRemoteCodexAccountPairings.mockResolvedValue([]);
    apiMocks.getRemoteManagerFeatureFlags.mockResolvedValue({
      nativeCodexRemoteManager: true,
      legacyBrokerHostsDropped: [],
    });
    apiMocks.getRemoteSshTrustStatus.mockResolvedValue({
      host: "192.0.2.10",
      port: 22,
      confirmed: true,
    });
    apiMocks.fetchRemoteSshFingerprint.mockResolvedValue({
      host: "192.0.2.10",
      port: 22,
      keyType: "ssh-ed25519",
      fingerprint: "SHA256:fWGOBFJCuLczHBhYs/jCgMIJ/NUUnYJmt5/owwnZAc4",
    });
    apiMocks.confirmRemoteSshFingerprint.mockResolvedValue(undefined);
    apiMocks.listRemoteOfficialExecutionAccounts.mockResolvedValue([]);
    apiMocks.getRemoteReleaseStatus.mockResolvedValue(blockedRelease);
    apiMocks.discoverRemoteConnections.mockResolvedValue([host]);
    apiMocks.addRemoteSshHost.mockResolvedValue({ candidate: host, loginCommand: "ssh -F private-config Jetson", keyInstallCommand: "ssh-copy-id -F private-config Jetson" });
    apiMocks.inspectRemoteHost.mockResolvedValue(status);
    apiMocks.getDesktopCodexCompatibility.mockResolvedValue({
      state: "current",
      desktopVersion: "0.147.0",
      desktopSchemaSha256: "desktop-schema",
      remoteVersion: "0.147.0",
      requiredRemoteVersion: "0.147.0",
      remoteArch: "aarch64",
      agentProtocol: 2,
      canUpdate: false,
      detail: "qualified",
    });
    apiMocks.updateRemoteCodexForDesktop.mockResolvedValue({
      operationId: "desktop-sync-1",
      hostId: "jetson",
      kind: "desktopCodexSync",
      phase: "resolvingDesktopCodex",
      state: "running",
      percent: 10,
      detail: null,
      updatedAt: new Date().toISOString(),
    });
    apiMocks.getRemoteSessionSummary.mockResolvedValue({
      hostId: "jetson",
      managerState: "nativeActive",
      detachedReady: true,
      observability: "nativeAppServer",
      threads: [{
        threadId: "thread-native-123",
        status: "active",
        activeTurn: true,
        activeTurnId: "turn-456",
        turnCount: 7,
        lastTurnStatus: "inProgress",
      }],
    });
    apiMocks.bootstrapRemoteHost.mockResolvedValue({
      operationId: "one-click-bootstrap-1",
      hostId: "jetson",
      kind: "oneClickBootstrap",
      phase: "queued",
      percent: 0,
      state: "running",
      message: null,
      result: null,
      updatedAt: "2026-08-11T00:00:00Z",
    });
    apiMocks.restoreRemoteHost.mockResolvedValue({
      operationId: "one-click-restore-1", hostId: "jetson", kind: "oneClickRestore",
      phase: "queued", percent: 0, state: "running", message: null, result: null,
      updatedAt: "2026-08-11T00:00:00Z",
    });
    apiMocks.getRemoteOperation.mockResolvedValue({
      operationId: "one-click-bootstrap-1",
      hostId: "jetson",
      kind: "oneClickBootstrap",
      phase: "verified",
      percent: 100,
      state: "completed",
      message: "Remote host is ready for native Codex projects",
      result: null,
      updatedAt: "2026-08-11T00:00:01Z",
    });
    apiMocks.stopRemoteAppOwnedCodex.mockResolvedValue({ daemonRunning: false });
    apiMocks.pollRemoteCodexAccountLogin.mockResolvedValue(status.chatgpt);
    apiMocks.activateRemoteCodexAccount.mockResolvedValue(status.chatgpt);
    apiMocks.pollRemoteGrokLogin.mockResolvedValue({});
    apiMocks.cancelRemoteGrokLogin.mockResolvedValue({});
    apiMocks.refreshRemoteGrokLogin.mockResolvedValue({});
    apiMocks.planRemoteDeployment.mockResolvedValue({
      planId: "plan-1", hostId: "jetson", state: "readyToApply",
      desiredRevision: 1, observedRevision: 0,
      selectedCatalogIds: ["gpt-5.6-luna", "vlm-third-party"],
      selectedModels: [
        { catalogId: "gpt-5.6-luna", displayName: "5.6 Luna", routeId: "official", upstreamModel: "gpt-5.6-luna", selected: true, mandatory: true },
        { catalogId: "vlm-third-party", displayName: "DeepSeek V4 Flash", routeId: "cc", upstreamModel: "deepseek-v4-flash", selected: true, mandatory: false },
      ],
      credentialRequirements: [],
      drift: { desiredConfigHash: "a".repeat(64), remoteConfigHash: null, configChanged: true, desiredCatalogHash: "b".repeat(64), remoteCatalogHash: null, catalogChanged: true },
      publicDiff: [], qualifiedCapabilities: [], rollbackSummary: "restore", planHash: "c".repeat(64),
      managedChanges: [], restartRequired: true, blockedReasons: [], expiresAt: "2026-08-11T01:00:00Z",
    });
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it("adds a host from the empty list and follows the normal fingerprint gate before probing", async () => {
    apiMocks.discoverRemoteConnections.mockResolvedValue([]);
    const fingerprint = { host: "192.0.2.10", port: 22, keyType: "ssh-ed25519", fingerprint: "SHA256:pending" };
    apiMocks.getRemoteSshTrustStatus.mockResolvedValue({ host: "192.0.2.10", port: 22, confirmed: false, fingerprint });
    renderRemote();
    const destination = await screen.findByLabelText(i18n.t("remoteAdd.destination"));
    fireEvent.change(destination, { target: { value: "operator@192.0.2.10" } });
    fireEvent.click(screen.getByRole("button", { name: i18n.t("remoteAdd.add") }));
    await waitFor(() => expect(apiMocks.addRemoteSshHost).toHaveBeenCalledWith("operator@192.0.2.10"));
    expect(await screen.findByText("SHA256:pending")).toBeTruthy();
    expect(screen.getByText(i18n.t("remoteAdd.loginHint"))).toBeTruthy();
    expect(apiMocks.inspectRemoteHost).not.toHaveBeenCalled();
    expect(apiMocks.fetchRemoteSshFingerprint).not.toHaveBeenCalled();
    apiMocks.getRemoteSshTrustStatus.mockResolvedValue({ host: "192.0.2.10", port: 22, confirmed: true });
    fireEvent.click(screen.getByRole("button", { name: i18n.t("remote.trust.confirm") }));
    await waitFor(() => expect(apiMocks.inspectRemoteHost).toHaveBeenCalledWith("jetson"));
  });

  it("shows inherited trust and probes without asking for confirmation or rescanning", async () => {
    apiMocks.getRemoteSshTrustStatus.mockResolvedValue({ host: "192.0.2.10", port: 22, confirmed: true, source: "userKnownHosts", fingerprint: { host: "192.0.2.10", port: 22, keyType: "ssh-ed25519", fingerprint: "SHA256:inherited" } });
    renderRemote();
    expect(await screen.findByText(i18n.t("remoteAdd.trustReused"))).toBeTruthy();
    await waitFor(() => expect(apiMocks.inspectRemoteHost).toHaveBeenCalledWith("jetson"));
    expect(screen.queryByRole("button", { name: i18n.t("remote.trust.confirm") })).toBeNull();
    expect(apiMocks.fetchRemoteSshFingerprint).not.toHaveBeenCalled();
    expect(apiMocks.confirmRemoteSshFingerprint).not.toHaveBeenCalled();
  });

  it("registers the bundled release readiness command used by the renderer", () => {
    expect(apiSource).toMatch(/getRemoteReleaseStatus[\s\S]*"get_remote_release_status"/);
    expect(rustSource).toMatch(/remote::commands::get_remote_release_status/);
    expect(apiSource).toMatch(/addRemoteSshHost[\s\S]*"add_remote_ssh_host"/);
    expect(rustSource).toMatch(/remote::commands::add_remote_ssh_host/);
    expect(apiSource).toMatch(/getDesktopCodexCompatibility[\s\S]*"get_desktop_codex_compatibility"/);
    expect(apiSource).toMatch(/updateRemoteCodexForDesktop[\s\S]*"update_remote_codex_for_desktop"/);
    expect(rustSource).toMatch(/remote::commands::get_desktop_codex_compatibility/);
    expect(rustSource).toMatch(/remote::commands::update_remote_codex_for_desktop/);
  });

  it.each(["reject", "throw"])("keeps handoff API %s failures inside the handoff section", async (mode) => {
    const failure = new Error("handoff thread discovery failed");
    if (mode === "throw") apiMocks.listRemoteHandoffThreads.mockImplementationOnce(() => { throw failure; });
    else apiMocks.listRemoteHandoffThreads.mockRejectedValueOnce(failure);
    renderRemote();
    expect(await screen.findByText(i18n.t("handoff.checkFailed"))).toBeTruthy();
    expect(screen.getByText(i18n.t("handoff.title", { host: "Jetson" }))).toBeTruthy();
    expect(screen.getByRole("button", { name: i18n.t("remote.rescan") })).toBeTruthy();
    expect(apiMocks.inspectRemoteHost).toHaveBeenCalled();
  });

  it("paints discovered hosts while the selected SSH probe is still pending", async () => {
    let resolveInspection!: (value: RemoteHostStatus) => void;
    apiMocks.inspectRemoteHost.mockReturnValue(new Promise((resolve) => {
      resolveInspection = resolve;
    }));

    renderRemote();

    expect(screen.getByText(/正在讀取 Codex\/OpenSSH 連線設定/)).toBeTruthy();
    expect((await screen.findAllByText("Jetson")).length).toBeGreaterThan(0);
    // 偵測狀態標在那一台主機的那一列上，不再是一則全頁通報。
    expect(await screen.findByText("偵測中…")).toBeTruthy();

    resolveInspection(status);
    await waitFor(() => expect(screen.queryByText("偵測中…")).toBeNull());
  });

  it("keeps the last-good status visible with a live indicator while re-probing an already-probed host", async () => {
    const view = renderRemote();

    const surface = await inspectedWorkSurface();
    // The fixture has a credential blocker: retain that state during refresh.
    expect(surface.getByRole("status").textContent).toBe(i18n.t("remote.verdict.nativeActive"));
    await waitFor(() => expect(apiMocks.inspectRemoteHost).toHaveBeenCalledTimes(1));
    expect(screen.queryByText(/更新中/)).toBeNull();

    let resolveSecondProbe!: (value: RemoteHostStatus) => void;
    apiMocks.inspectRemoteHost.mockReturnValue(new Promise((resolve) => {
      resolveSecondProbe = resolve;
    }));

    // Simulates the app-level "refresh" action: it re-runs discovery and
    // bumps the internal probe generation, which re-probes the currently
    // selected (already-probed) host.
    view.rerender(<Remote refreshVersion={1} onRefreshComplete={() => {}} />);
    await waitFor(() => expect(apiMocks.inspectRemoteHost).toHaveBeenCalledTimes(2));

    // The refresh must never blank the page: the last-good verdict, and the
    // rest of the previously-rendered detail, stay on screen while the new
    // probe is in flight — only a small "updating" marker is added.
    expect(surface.getByRole("status").textContent).toBe(i18n.t("remote.verdict.nativeActive"));
    expect(screen.getByText("credential cc is unavailable")).toBeTruthy();
    expect(screen.queryByText("偵測中…")).toBeNull();
    expect(await screen.findByText(/· 更新中/)).toBeTruthy();

    resolveSecondProbe(status);
    await waitFor(() => expect(screen.queryByText(/· 更新中/)).toBeNull());
    expect(surface.getByRole("status").textContent).toBe(i18n.t("remote.verdict.nativeActive"));
  });

  it("keeps a failed probe on the host row instead of raising a page-level alert", async () => {
    apiMocks.inspectRemoteHost.mockRejectedValue(new Error("ssh: connect timeout"));
    renderRemote();

    expect(await screen.findByText("偵測失敗")).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("clears a host probe error after a successful reinspection", async () => {
    apiMocks.inspectRemoteHost.mockRejectedValueOnce(new Error("ssh: connect timeout"));
    const view = renderRemote();
    const failed = await screen.findByText(i18n.t("remote.probeFailed"));
    expect(failed.closest("button")?.classList.contains("remote-host--unreachable")).toBe(true);

    view.rerender(<Remote refreshVersion={1} onRefreshComplete={() => {}} />);
    await inspectedWorkSurface();
    await waitFor(() => expect(screen.queryByText(i18n.t("remote.probeFailed"))).toBeNull());
    expect(document.querySelector(".remote-host--unreachable")).toBeNull();
    expect(apiMocks.inspectRemoteHost).toHaveBeenCalledTimes(2);
  });

  it("formats the last inspection time using the selected UI language and updates it on language changes", async () => {
    const clock = vi.spyOn(Date, "now").mockReturnValue(new Date(2026, 9, 16, 20, 43, 53).getTime());
    try {
      renderRemote();
      await inspectedWorkSurface();
      const updated = () => document.querySelector(".worktop__updated")?.textContent ?? "";
      await waitFor(() => expect(updated()).toContain("下午"));
      await act(async () => { await i18n.changeLanguage("en"); });
      await waitFor(() => expect(updated()).toMatch(/Updated.*8:43:53\s*PM/));
      expect(updated()).not.toMatch(/上午|下午/);
      expect(apiMocks.inspectRemoteHost).toHaveBeenCalledTimes(1);
    } finally {
      clock.mockRestore();
      await act(async () => { await i18n.changeLanguage("zh-TW"); });
    }
  });

  it("answers readiness in one line and hides the nineteen detail rows behind a tray", async () => {
    apiMocks.inspectRemoteHost.mockResolvedValue({ ...status, managerState: "detachedReady", blockedReasons: [], inventory: status.inventory && { ...status.inventory, blockers: [] } });
    renderRemote();

    const surface = await inspectedWorkSurface();
    expect(surface.getAllByRole("status")).toHaveLength(1);
    expect(surface.getByRole("status").textContent).toBe(i18n.t("remote.verdict.detachedReady"));
    // 細節預設收起來。jsdom 不套 <details> 的預設樣式，所以節點還是查得到 ——
    // 要斷言的是「它在一個關著的托盤裡」，不是「它不在 DOM 裡」。
    const runtime = screen.getByText("CODEX_HOME").closest("details");
    expect(runtime?.open).toBe(false);
    openTray("執行環境");
    expect(runtime?.open).toBe(true);
    expect(screen.getAllByText("/home/vellum-test/.codex").length).toBeGreaterThan(0);
  });

  it("shows Apple Silicon login persistence, isolation, and Intel as 尚未支援", async () => {
    apiMocks.inspectRemoteHost.mockResolvedValue({
      ...status,
      agent: status.agent && {
        ...status.agent,
        agentProtocol: 4,
        capabilities: {
          ...status.agent.capabilities,
          os: "macos",
          arch: "aarch64",
          dockerAvailable: false,
          platform: "darwin-arm64",
          proxyBackend: "native",
          serviceManager: "launchd",
          persistenceScope: "login",
          managedCodexHome: "/Users/joshhuang/.vellum-remote/codex",
        },
        nativeCodex: status.agent.nativeCodex && {
          ...status.agent.nativeCodex,
          codexHome: "/Users/joshhuang/.vellum-remote/codex",
        },
      },
      inventory: status.inventory && {
        ...status.inventory,
        agentProtocol: 4,
        platform: "darwin-arm64",
        proxyBackend: "native",
        serviceManager: "launchd",
        persistenceScope: "login",
        managedCodexHome: "/Users/joshhuang/.vellum-remote/codex",
        system: { ...status.inventory.system, os: "macos", arch: "aarch64" },
        docker: { ...status.inventory.docker, available: false, mode: "unavailable" },
      },
    });
    renderRemote();
    await inspectedWorkSurface();
    openTray("主機");
    expect(screen.getByText("登入後常駐")).toBeTruthy();
    expect(screen.getAllByText("/Users/joshhuang/.vellum-remote/codex").length).toBeGreaterThan(0);
    expect(screen.getByText(/本機 Vellum／Enhanced／~\/.codex 隔離/)).toBeTruthy();

    apiMocks.inspectRemoteHost.mockResolvedValue({
      ...status,
      agent: status.agent && {
        ...status.agent,
        capabilities: {
          ...status.agent.capabilities,
          os: "macos",
          arch: "x86_64",
        },
      },
    });
    cleanup();
    renderRemote();
    expect(await screen.findByText(/尚未支援/)).toBeTruthy();
  });

  it("shows a plain-language upgrade notice for a schema-1 configuration instead of a raw code", async () => {
    apiMocks.inspectRemoteHost.mockResolvedValue({
      ...status,
      agent: status.agent && {
        ...status.agent,
        configuration: {
          present: true, state: "upgradeRequired", schemaVersion: null,
          requiresReconfigure: true, issue: null,
          configHash: null, credentialRefs: [], credentialsReady: false,
        },
      },
    });
    renderRemote();

    await inspectedWorkSurface();
    openTray("執行環境");
    expect(screen.getByText("設定")).toBeTruthy();
    expect(screen.getByText("遠端 Proxy 設定需要安全升級，重新部署即可自動修復。")).toBeTruthy();
  });

  it("shows nothing extra for a current configuration -- the healthy case stays silent", async () => {
    renderRemote();

    await inspectedWorkSurface();
    openTray("執行環境");
    expect(screen.queryByText("設定")).toBeNull();
  });

  it("renders native thread detail and fail-closes release operations", async () => {
    apiMocks.bootstrapRemoteHost.mockRejectedValue(new Error("bundledReleaseUnavailable"));
    renderRemote();
    expect(await screen.findByText("thread-native-123")).toBeTruthy();
    expect(screen.getByText(/7 個 turn · 最近一次 inProgress/)).toBeTruthy();
    expect(screen.getByText("credential cc is unavailable")).toBeTruthy();
    const surface = await inspectedWorkSurface();
    fireEvent.click(surface.getByRole("button", { name: "重新部署" }));
    await waitFor(() => expect(surface.getByRole("status").textContent).toBe(i18n.t("remoteWork.failed")));
    expect(surface.getByRole<HTMLButtonElement>("button", { name: "重新部署" }).disabled).toBe(true);
    expect(surface.queryByRole("button", { name: /安裝 pinned|更新遠端|重試/ })).toBeNull();
    expect(apiMocks.planRemoteDeployment).not.toHaveBeenCalled();
  });

  it("turns an unrecognised blocker code into a plain sentence with a remedy", async () => {
    apiMocks.inspectRemoteHost.mockResolvedValue({
      ...status,
      blockedReasons: ["nativeDaemonAppOwned"],
    });
    renderRemote();

    expect(await screen.findByText(/Codex App 目前直接佔用此主機的 app-server/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "下一步：安全停止並重試" })).toBeTruthy();
  });

  it("authorizes a selected Desktop account without exposing a token", async () => {
    const pairingStatus: RemoteHostStatus = {
      ...status,
      chatgpt: {
        accountId: null,
        expectedAccountId: "acct-windows",
        state: "pairingRequired",
        paired: false,
        active: false,
        loginPending: false,
        loginId: null,
      },
    };
    apiMocks.inspectRemoteHost.mockResolvedValue(pairingStatus);
    apiMocks.listRemoteCodexAccountPairings.mockResolvedValue([
      {
        accountId: "acct-windows", email: "owner@example.com", workspaceName: "Personal",
        isDesktopDefault: true, paired: false, active: false, credentialState: "missing",
        expiresAt: null, detail: null,
      },
    ]);
    apiMocks.startRemoteCodexAccountLogin.mockResolvedValue({
      loginId: "login-1",
      verificationUrl: "https://auth.openai.com/device",
      userCode: "ABCD-EFGH",
      expectedAccountId: "acct-windows",
    });
    const open = vi.spyOn(window, "open").mockImplementation(() => null);
    renderRemote();

    fireEvent.click(await screen.findByRole("button", { name: "授權到遠端" }));
    await waitFor(() => expect(apiMocks.startRemoteCodexAccountLogin)
      .toHaveBeenCalledWith("jetson", "acct-windows"));
    expect(open).toHaveBeenCalledWith(
      "https://auth.openai.com/device",
      "_blank",
      "noopener,noreferrer",
    );
    expect(await screen.findByText("ABCD-EFGH")).toBeTruthy();
    expect(screen.queryByText(/refresh_token|access_token/)).toBeNull();
  });

  it("marks an expired Remote Control credential and starts reauthentication for that account", async () => {
    apiMocks.inspectRemoteHost.mockResolvedValue({
      ...status,
      chatgpt: {
        ...status.chatgpt,
        state: "reauthenticationRequired",
        credentialState: "expired",
        expiresAt: 1_786_656_000,
      },
    });
    apiMocks.listRemoteCodexAccountPairings.mockResolvedValue([
      {
        accountId: "acct-windows", email: "owner@example.com", workspaceName: "Personal",
        isDesktopDefault: true, paired: true, active: true, credentialState: "expired",
        expiresAt: 1_786_656_000, detail: null,
      },
    ]);
    apiMocks.startRemoteCodexAccountLogin.mockResolvedValue({
      loginId: "login-refresh",
      verificationUrl: "https://auth.openai.com/device",
      userCode: "REAUTH-1",
      expectedAccountId: "acct-windows",
    });
    vi.spyOn(window, "open").mockImplementation(() => null);
    renderRemote();

    expect(await screen.findByText(/這個帳號的遠端憑證已過期/)).toBeTruthy();
    fireEvent.click(screen.getAllByRole("button", { name: "重新登入" })[0]!);
    await waitFor(() => expect(apiMocks.startRemoteCodexAccountLogin)
      .toHaveBeenCalledWith("jetson", "acct-windows"));
    expect(await screen.findByText("REAUTH-1")).toBeTruthy();
  });

  it("treats the active auth file as authorized even before a managed slot exists", async () => {
    apiMocks.listRemoteCodexAccountPairings.mockResolvedValue([
      {
        accountId: "acct-windows", email: "owner@example.com", workspaceName: "Personal",
        isDesktopDefault: true, paired: false, active: true, credentialState: "valid",
        expiresAt: 1_800_000_000, detail: null,
      },
    ]);
    renderRemote();

    expect(await screen.findByText("遠端憑證可用")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "授權到遠端" })).toBeNull();
    expect(screen.queryByRole("button", { name: "配對桌面端全部 ChatGPT 帳號" })).toBeNull();
  });

  /**
   * 初次設定要一次帶完 Desktop 上全部帳號的核可。每個帳號在每台主機上都要各自
   * 核可一次 —— OAuth 的 refresh token 會輪替且伺服器端偵測重用，一份 grant
   * 不能給兩個客戶端共用，所以省不掉。能省的是「回列表再點一次」：一個配好就
   * 直接接下一個，最後把 daemon 切回 Desktop 目前的預設帳號（批次的最後一步會
   * 把 auth.json 停在最後配對的那一個，那通常不是使用者選的那一個）。
   */
  it("walks every unpaired desktop account and lands on the desktop default", async () => {
    apiMocks.listRemoteCodexAccountPairings.mockResolvedValue([
      { accountId: "acct-a", email: "a@example.com", workspaceName: "Personal", isDesktopDefault: true, paired: true, active: true, detail: null },
      { accountId: "acct-b", email: "b@example.com", isDesktopDefault: false, paired: false, active: false, detail: null },
      { accountId: "acct-c", email: "c@example.com", isDesktopDefault: false, paired: false, active: false, detail: null },
    ]);
    apiMocks.startRemoteCodexAccountLogin.mockImplementation((_host: string, accountId?: string) =>
      Promise.resolve({
        loginId: `login-${accountId}`,
        verificationUrl: `https://auth.openai.com/device#${accountId}`,
        userCode: `CODE-${accountId}`,
        expectedAccountId: accountId ?? "acct-a",
      }));
    apiMocks.pollRemoteCodexAccountLogin.mockResolvedValue({
      accountId: "acct-b", expectedAccountId: "acct-b", state: "synchronized",
      paired: true, active: true, loginPending: false, loginId: null,
    });
    apiMocks.activateRemoteCodexAccount.mockResolvedValue(status.chatgpt);
    vi.spyOn(window, "open").mockImplementation(() => null);
    renderRemote();

    fireEvent.click(await screen.findByRole("button", { name: "配對桌面端全部 ChatGPT 帳號" }));

    // 已配對的 acct-a 不會被排進去，只有 b 和 c。
    await waitFor(() => expect(apiMocks.startRemoteCodexAccountLogin)
      .toHaveBeenCalledWith("jetson", "acct-b"));
    expect(await screen.findByText("CODE-acct-b")).toBeTruthy();
    expect(apiMocks.startRemoteCodexAccountLogin).not.toHaveBeenCalledWith("jetson", "acct-a");

    await waitFor(() => expect(apiMocks.startRemoteCodexAccountLogin)
      .toHaveBeenCalledWith("jetson", "acct-c"), { timeout: 4000 });
    // 隊列走完之後回到 Desktop 的預設帳號，而不是停在最後配對的 acct-c。
    await waitFor(() => expect(apiMocks.activateRemoteCodexAccount)
      .toHaveBeenCalledWith("jetson", "acct-a"), { timeout: 4000 });
  });

  it("finishes the models a deploy deferred as soon as the Desktop account is paired", async () => {
    apiMocks.getRemoteOperation.mockResolvedValue({
      operationId: "one-click-bootstrap-1", hostId: "jetson", kind: "oneClickBootstrap",
      phase: "verified", percent: 100, state: "completed", message: null,
      result: { chatgptPairingSuggested: true, omittedCatalogIds: ["gpt-5.6-luna"] },
      updatedAt: "2026-10-17T00:00:01Z",
    });
    apiMocks.listRemoteCodexAccountPairings.mockResolvedValue([
      { accountId: "acct-a", email: "a@example.com", isDesktopDefault: true, paired: false, active: false, detail: null },
    ]);
    apiMocks.startRemoteCodexAccountLogin.mockResolvedValue({
      loginId: "login-a", verificationUrl: "https://auth.openai.com/device#a", userCode: "CODE-A", expectedAccountId: "acct-a",
    });
    apiMocks.pollRemoteCodexAccountLogin.mockResolvedValue({ ...status.chatgpt, state: "synchronized" });
    vi.spyOn(window, "open").mockImplementation(() => null);
    renderRemote();
    fireEvent.click(await screen.findByRole("button", { name: "重新部署" }));
    const pair = await screen.findByRole("button", { name: i18n.t("remote.act.chatgptPair") }, { timeout: 2500 });
    await waitFor(() => expect((pair as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(pair);
    await waitFor(() => expect(apiMocks.startRemoteCodexAccountLogin).toHaveBeenCalledWith("jetson", undefined));
    // No second click: the deferred models deploy on their own.
    await waitFor(() => expect(apiMocks.bootstrapRemoteHost).toHaveBeenCalledTimes(2), { timeout: 4000 });
  });

  it("locks Grok login immediately and shows CLI progress until the URL is returned", async () => {
    let resolveLogin!: (value: unknown) => void;
    apiMocks.startRemoteGrokLogin.mockReturnValue(new Promise((resolve) => {
      resolveLogin = resolve;
    }));
    renderRemote();
    await inspectedWorkSurface();
    openTray("進階");

    const button = await screen.findByRole("button", { name: "登入 Grok 帳號" });
    fireEvent.click(button);
    fireEvent.click(button);

    expect(apiMocks.startRemoteGrokLogin).toHaveBeenCalledTimes(1);
    const waiting = await screen.findByRole("button", { name: "正在等待 Grok CLI…" });
    expect((waiting as HTMLButtonElement).disabled).toBe(true);
    expect(waiting.querySelector(".remote-login-spinner")).toBeTruthy();

    resolveLogin({});
    await waitFor(() => expect(apiMocks.startRemoteGrokLogin).toHaveBeenCalledTimes(1));
  });

  it("shows official models as mandatory beside third-party models", async () => {
    renderRemote();
    expect(await screen.findByText("5.6 Luna")).toBeTruthy();
    expect(screen.getByText("DeepSeek V4 Flash")).toBeTruthy();
    const luna = screen.getByText("5.6 Luna").closest("label")?.querySelector("input");
    expect(luna?.checked).toBe(true);
    expect(luna?.disabled).toBe(true);
  });

  it("confirms the first deployment with three scannable facts instead of a native dialog", async () => {
    const confirm = vi.spyOn(window, "confirm");
    apiMocks.inspectRemoteHost.mockResolvedValue({ ...status, managerState: "unmanaged" });
    renderRemote();

    const surface = within(await screen.findByRole("region", { name: i18n.t("remoteWork.label") }));
    fireEvent.click(await surface.findByRole("button", { name: i18n.t("remote.act.bootstrap") }));
    const modal = dialog();
    expect(modal.getByText("會改")).toBeTruthy();
    expect(modal.getByText("不動")).toBeTruthy();
    expect(modal.getByText("進行中的 turn")).toBeTruthy();
    expect(modal.getByText(/會被拒絕，不會強制中斷/)).toBeTruthy();
    expect(confirm).not.toHaveBeenCalled();
    expect(apiMocks.bootstrapRemoteHost).not.toHaveBeenCalled();
  });

  it("redeploys a managed host in place without asking again", async () => {
    renderRemote();

    const button = await screen.findByRole("button", { name: "重新部署" });
    fireEvent.click(button);
    expect(document.querySelector("dialog[open]")).toBeNull();

    await waitFor(() => expect(apiMocks.bootstrapRemoteHost).toHaveBeenCalledWith("jetson"));
    // 進度留在工作面，主要按鈕停用，避免重複部署。
    expect(await screen.findByText("正在部署")).toBeTruthy();
    expect(screen.getByText("排入佇列")).toBeTruthy();
    expect(screen.getByText(/0% · 已經過 0:0\d/)).toBeTruthy();
    const surface = within(screen.getByRole("region", { name: i18n.t("remoteWork.label") }));
    expect(surface.getByRole<HTMLButtonElement>("button", { name: "重新部署" }).disabled).toBe(true);
  });

  it("explains that on-demand services need a reconnect after reboot", async () => {
    apiMocks.inspectRemoteHost.mockResolvedValue({
      ...status,
      inventory: { ...status.inventory, persistenceScope: "onDemand" },
    });
    renderRemote();
    expect(await screen.findByText("Agent supervisor 常駐；主機重開機後需重新連線一次")).toBeTruthy();
  });

  it.each(["immediate", "background"])("shows the ps prerequisite and repair command on %s failure", async (mode) => {
    const message = "OneClickBootstrapBlocked: psStartTimeUnavailable; repair: apk add procps-ng";
    if (mode === "immediate") {
      apiMocks.bootstrapRemoteHost.mockRejectedValueOnce(new Error(message));
    } else {
      apiMocks.getRemoteOperation.mockResolvedValue({
        operationId: "one-click-bootstrap-1", hostId: "jetson", kind: "oneClickBootstrap",
        phase: "hostPreflight", percent: 5, state: "failed", message, result: null,
        updatedAt: "2026-10-16T00:00:00Z",
      });
    }
    renderRemote();
    fireEvent.click(await screen.findByRole("button", { name: "重新部署" }));
    expect(await screen.findByText(
      "主機的 ps 無法查詢程序啟動時間，無法啟動 Codex daemon。請依修復指令安裝相容的 ps，再重新部署。",
      {}, { timeout: 2500 },
    )).toBeTruthy();
    expect(within(screen.getByRole("region", { name: i18n.t("remoteWork.label") })).getByText("apk add procps-ng")).toBeTruthy();
    expect(apiMocks.stopRemoteAppOwnedCodex).not.toHaveBeenCalled();
  });

  it("offers a safe app-owned stop on the failure instead of popping a dialog unprompted", async () => {
    apiMocks.getRemoteOperation.mockResolvedValue({
      operationId: "one-click-bootstrap-1",
      hostId: "jetson",
      kind: "oneClickBootstrap",
      phase: "nativeDaemon",
      percent: 35,
      state: "failed",
      message: "OneClickBootstrapBlocked: nativeDaemonAppOwned; repair: disconnect manually",
      result: null,
      updatedAt: "2026-08-11T00:00:01Z",
    });
    apiMocks.bootstrapRemoteHost
      .mockResolvedValueOnce({
        operationId: "one-click-bootstrap-1", hostId: "jetson", kind: "oneClickBootstrap",
        phase: "queued", percent: 0, state: "running", message: null, result: null,
        updatedAt: "2026-08-11T00:00:00Z",
      })
      .mockResolvedValueOnce({
        operationId: "one-click-bootstrap-2", hostId: "jetson", kind: "oneClickBootstrap",
        phase: "queued", percent: 0, state: "running", message: null, result: null,
        updatedAt: "2026-08-11T00:00:02Z",
      });
    renderRemote();

    fireEvent.click(await screen.findByRole("button", { name: "重新部署" }));

    // 失敗原地顯示，說出停在哪一步，並附上補救動作。使用者沒按任何東西之前，
    // 不會有任何視窗自己跳出來。
    expect(await screen.findByText(
      "Codex App 目前直接佔用此主機的 app-server，Vellum 無法接管。",
      {},
      { timeout: 2500 },
    )).toBeTruthy();
    expect(document.querySelector("dialog[open]")).toBeNull();
    expect(apiMocks.stopRemoteAppOwnedCodex).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "下一步：安全停止並重試" }));
    fireEvent.click(dialog().getByRole("button", { name: "安全停止並重試" }));

    await waitFor(() => expect(apiMocks.stopRemoteAppOwnedCodex).toHaveBeenCalledWith(
      "jetson",
      expect.stringMatching(/^stop-app-owned-/),
    ));
    await waitFor(() => expect(apiMocks.bootstrapRemoteHost).toHaveBeenCalledTimes(2));
  });

  it("keeps the failed operation visible when the app-owned stop is dismissed", async () => {
    apiMocks.getRemoteOperation.mockResolvedValue({
      operationId: "one-click-bootstrap-1", hostId: "jetson", kind: "oneClickBootstrap",
      phase: "nativeDaemon", percent: 35, state: "failed",
      message: "OneClickBootstrapBlocked: nativeDaemonAppOwned", result: null,
      updatedAt: "2026-08-11T00:00:01Z",
    });
    renderRemote();

    fireEvent.click(await screen.findByRole("button", { name: "重新部署" }));

    expect(await screen.findByText(
      "OneClickBootstrapBlocked: nativeDaemonAppOwned",
      {},
      { timeout: 2500 },
    )).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "下一步：安全停止並重試" }));
    fireEvent.click(dialog().getByRole("button", { name: "取消" }));

    expect(apiMocks.stopRemoteAppOwnedCodex).not.toHaveBeenCalled();
    expect(apiMocks.bootstrapRemoteHost).toHaveBeenCalledTimes(1);
    expect(screen.getByText("OneClickBootstrapBlocked: nativeDaemonAppOwned")).toBeTruthy();
  });

  it("turns a Desktop protocol mismatch into a runtime sync remedy", async () => {
    apiMocks.getDesktopCodexCompatibility.mockResolvedValue({
      state: "updateAvailable",
      desktopVersion: "0.147.0-alpha.6.6",
      desktopSchemaSha256: "desktop-schema-new",
      remoteVersion: "0.147.0-alpha.6.5",
      requiredRemoteVersion: "0.147.0-alpha.6.6",
      remoteArch: "aarch64",
      agentProtocol: 3,
      canUpdate: true,
      detail: "official artifact available",
    });
    apiMocks.getRemoteOperation.mockResolvedValue({
      operationId: "one-click-bootstrap-1", hostId: "jetson", kind: "oneClickBootstrap",
      phase: "installCodex", percent: 25, state: "failed",
      message: "CodexDesktopProtocolMismatch: missing field `shell_type`",
      result: null, updatedAt: "2026-08-14T00:00:01Z",
    });
    renderRemote();

    fireEvent.click(await screen.findByRole("button", { name: "重新部署" }));

    const mismatch = await screen.findByText(
      "遠端 Codex runtime 與目前 Desktop 協定不相容。請先同步 Desktop Codex runtime，再重試一次。",
      {},
      { timeout: 2500 },
    );
    expect(mismatch).toBeTruthy();
    expect(screen.queryByRole("button", { name: "重試" })).toBeNull();
    fireEvent.click(within(screen.getByRole("region", { name: i18n.t("remoteWork.label") }))
      .getByRole("button", { name: "下一步：同步 Desktop Codex runtime" }));
    expect(dialog().getByText(/下載與 Desktop core 完全一致/)).toBeTruthy();
  });

  /**
   * A legacy host with an active native lease but a missing/stale boundary
   * key hits `RemoteBoundaryKeyProvisionFailed` on every daemon-lifecycle
   * operation until Desktop re-provisions it. Unlike the Desktop-protocol
   * mismatch case above, there is no special remedy step — retrying the
   * same operation re-provisions the key from scratch — so this must keep
   * the ordinary retry button (not swap it for a dedicated action) while
   * still showing the dedicated message instead of the generic
   * "operation failed at phase" text.
   */
  it("shows a dedicated message and keeps ordinary retry for a boundary-key provisioning failure", async () => {
    const raw = "RemoteBoundaryKeyProvisionFailed: agent failed: connection refused";
    apiMocks.getRemoteOperation.mockResolvedValue({
      operationId: "one-click-bootstrap-1", hostId: "jetson", kind: "oneClickBootstrap",
      phase: "hostPreflight", percent: 5, state: "failed", message: raw, result: null, updatedAt: "2026-08-14T00:00:01Z",
    });
    renderRemote();
    const surface = await inspectedWorkSurface();
    fireEvent.click(surface.getByRole("button", { name: "重新部署" }));
    expect(await surface.findByText(i18n.t("remote.error.boundaryKeyProvisionFailed"), {}, { timeout: 2500 })).toBeTruthy();
    expect(surface.getAllByRole("button")).toHaveLength(2);
    expect(surface.getByRole<HTMLButtonElement>("button", { name: "重新部署" }).disabled).toBe(true);
    expect(surface.getByRole("button", { name: "下一步：重試" })).toBeTruthy();
    expect(surface.queryByRole("button", { name: "匯出診斷包" })).toBeNull();
    const exportButton = screen.getByRole("button", { name: "匯出診斷包" });
    expect(exportButton.closest("details")?.open).toBe(false);
    const details = screen.getByText(raw).closest("details");
    expect(details?.open).toBe(false);
    openTray("詳細資料"); expect(details?.open).toBe(true);
    openTray("進階");
    expect(screen.getByRole("button", { name: "匯出診斷包" })).toBeTruthy();
    fireEvent.click(surface.getByRole("button", { name: "下一步：重試" }));
    await waitFor(() => expect(apiMocks.bootstrapRemoteHost).toHaveBeenCalledTimes(2));
  });

  /**
   * The GPU dev host deployment failure. Provisioning the boundary key has to
   * restart native Codex, and Codex refuses every lifecycle command against
   * an app-server its own daemon does not claim — so this arrives as a
   * daemon-ownership failure on the action the user is standing in front of,
   * not as a boundary-key failure. Retry can never clear it; the same stop the
   * background branch offers has to be reachable from here too.
   */
  it("offers the app-owned stop when the action just pressed hit a daemon Codex disowns", async () => {
    apiMocks.bootstrapRemoteHost.mockRejectedValue(new Error("nativeDaemonAppOwned: app-server is not owned by Codex daemon"));
    renderRemote();
    const surface = await inspectedWorkSurface();
    fireEvent.click(surface.getByRole("button", { name: "重新部署" }));
    expect(await surface.findByText(i18n.t("remote.blocker.nativeDaemonAppOwned"))).toBeTruthy();
    expect(surface.getByRole<HTMLButtonElement>("button", { name: "重新部署" }).disabled).toBe(true);
    expect(surface.queryByRole("button", { name: "重試" })).toBeNull();
    fireEvent.click(surface.getByRole("button", { name: "下一步：安全停止並重試" }));
    fireEvent.click(dialog().getByRole("button", { name: "安全停止並重試" }));
    await waitFor(() => expect(apiMocks.stopRemoteAppOwnedCodex).toHaveBeenCalledWith("jetson", expect.stringMatching(/^stop-app-owned-/)));
  });

  it("converges bundled component updates without exposing maintenance controls", async () => {
    apiMocks.getRemoteReleaseStatus.mockResolvedValue({ ...blockedRelease, ready: true, detail: "verified" });
    renderRemote();
    const surface = await inspectedWorkSurface();
    const advanced = screen.getByText("進階").closest("details");
    expect(advanced?.open).toBe(false);
    openTray("進階");
    expect(advanced?.open).toBe(true);
    expect(screen.queryByRole("button", { name: /安裝 pinned|更新遠端 Agent|變更模型|修復|同步 Desktop Codex|重新啟動遠端/ })).toBeNull();
    fireEvent.click(surface.getByRole("button", { name: "重新部署" }));
    await waitFor(() => expect(apiMocks.bootstrapRemoteHost).toHaveBeenCalledWith("jetson"));
    expect(apiMocks.planRemoteDeployment).not.toHaveBeenCalled();
    expect(await surface.findByText("正在部署")).toBeTruthy();
  });

  it("allows an explicit rescan after manual repair without silently deploying again", async () => {
    apiMocks.bootstrapRemoteHost.mockRejectedValueOnce(new Error("psStartTimeUnavailable; repair: apk add procps-ng"));
    renderRemote();
    const surface = await inspectedWorkSurface();
    fireEvent.click(surface.getByRole("button", { name: "重新部署" }));
    expect(await surface.findByText("apk add procps-ng")).toBeTruthy();
    await waitFor(() => expect(surface.getByRole<HTMLButtonElement>("button", { name: "重新部署" }).disabled).toBe(true));
    await waitFor(() => expect(screen.getByRole<HTMLButtonElement>("button", { name: "重新掃描" }).disabled).toBe(false));
    fireEvent.click(screen.getByRole("button", { name: "重新掃描" }));
    await waitFor(() => expect(apiMocks.inspectRemoteHost.mock.calls.length).toBeGreaterThan(1));
    expect(apiMocks.bootstrapRemoteHost).toHaveBeenCalledTimes(1);
    expect(surface.getByRole<HTMLButtonElement>("button", { name: "重新部署" }).disabled).toBe(false);
    fireEvent.click(surface.getByRole("button", { name: "重新部署" }));
    await waitFor(() => expect(apiMocks.bootstrapRemoteHost).toHaveBeenCalledTimes(2));
  });

  it("uses the single converge action when the bundled proxy image changed", async () => {
    apiMocks.getRemoteReleaseStatus.mockResolvedValue({ ...blockedRelease, ready: true, releaseVersion: "0.2.2", proxyImage: "vellum-proxy:0.2.2-new-source", detail: "verified" });
    renderRemote();
    const surface = await inspectedWorkSurface();
    expect(surface.getAllByRole("button")).toHaveLength(1);
    fireEvent.click(surface.getByRole("button", { name: "重新部署" }));
    await waitFor(() => expect(apiMocks.bootstrapRemoteHost).toHaveBeenCalledWith("jetson"));
    expect(screen.queryByRole("button", { name: "重新同步" })).toBeNull();
  });

  it("offers an exact Desktop runtime sync when remote protocol qualification is stale", async () => {
    apiMocks.getDesktopCodexCompatibility.mockResolvedValue({ state: "updateAvailable", canUpdate: true });
    apiMocks.getRemoteOperation.mockResolvedValue({ operationId: "one-click-bootstrap-1", hostId: "jetson", kind: "oneClickBootstrap", state: "failed", phase: "nativeDaemon", percent: 35, message: "CodexDesktopProtocolMismatch", result: null, updatedAt: "2026-10-16T00:00:00Z" });
    renderRemote();
    const surface = await inspectedWorkSurface();
    fireEvent.click(surface.getByRole("button", { name: "重新部署" }));
    fireEvent.click(await surface.findByRole("button", { name: "下一步：同步 Desktop Codex runtime" }, { timeout: 2500 }));
    fireEvent.click(dialog().getByRole("button", { name: "同步 Desktop Codex runtime" }));
    await waitFor(() => expect(apiMocks.updateRemoteCodexForDesktop).toHaveBeenCalledWith("jetson"));
  });

  it("gates the teardown behind the host name and never calls it on cancel", async () => {
    renderRemote();

    fireEvent.click(await screen.findByRole("button", { name: "解除 Vellum 管理…" }));
    const modal = dialog();
    const confirm = modal.getByRole("button", { name: "解除 Vellum 管理…" }) as HTMLButtonElement;
    expect(confirm.disabled).toBe(true);

    fireEvent.change(modal.getByRole("textbox"), { target: { value: "Jetso" } });
    expect(confirm.disabled).toBe(true);

    fireEvent.change(modal.getByRole("textbox"), { target: { value: "Jetson" } });
    expect(confirm.disabled).toBe(false);
    fireEvent.click(confirm);

    await waitFor(() => expect(apiMocks.restoreRemoteHost).toHaveBeenCalledWith("jetson"));
    expect(await screen.findByText("正在解除管理")).toBeTruthy();
  });

  it("retries a failed restore through the host-name gate without starting deployment", async () => {
    apiMocks.getRemoteOperation.mockResolvedValue({
      operationId: "one-click-restore-1", hostId: "jetson", kind: "oneClickRestore",
      phase: "restore", percent: 35, state: "failed", message: "ssh: connect timeout",
      result: null, updatedAt: "2026-10-16T00:00:00Z",
    });
    renderRemote();
    await inspectedWorkSurface();
    fireEvent.click(screen.getByRole("button", { name: "解除 Vellum 管理…" }));
    fireEvent.change(dialog().getByRole("textbox"), { target: { value: "Jetson" } });
    fireEvent.click(dialog().getByRole("button", { name: "解除 Vellum 管理…" }));

    const surface = within(screen.getByRole("region", { name: i18n.t("remoteWork.label") }));
    fireEvent.click(await surface.findByRole("button", { name: "下一步：重試" }, { timeout: 2500 }));
    const confirm = dialog().getByRole<HTMLButtonElement>("button", { name: "解除 Vellum 管理…" });
    expect(confirm.disabled).toBe(true);
    fireEvent.change(dialog().getByRole("textbox"), { target: { value: "Jetson" } });
    fireEvent.click(confirm);
    await waitFor(() => expect(apiMocks.restoreRemoteHost).toHaveBeenCalledTimes(2));
    expect(apiMocks.bootstrapRemoteHost).not.toHaveBeenCalled();
  });

  it("exports local diagnostics without requiring an unreachable host to pass the trust gate", async () => {
    apiMocks.getRemoteSshTrustStatus.mockResolvedValue({ confirmed: false });
    apiMocks.fetchRemoteSshFingerprint.mockRejectedValue(new Error("ssh: connect timeout"));
    apiMocks.createRemoteSupportBundle.mockResolvedValue({ remoteUnavailable: true });
    renderRemote();
    await screen.findByText(i18n.t("remote.trust.fetchFailed"));
    openTray("進階");
    fireEvent.click(screen.getByRole("button", { name: "匯出診斷包" }));
    await waitFor(() => expect(apiMocks.createRemoteSupportBundle).toHaveBeenCalledWith("jetson"));
    expect(apiMocks.confirmRemoteSshFingerprint).not.toHaveBeenCalled();
    expect(apiMocks.inspectRemoteHost).not.toHaveBeenCalled();
  });

  it("asks the user to confirm the SSH fingerprint before inspect or bootstrap", async () => {
    apiMocks.getRemoteSshTrustStatus.mockResolvedValue({
      host: "192.0.2.10",
      port: 22,
      confirmed: false,
    });
    apiMocks.fetchRemoteSshFingerprint.mockResolvedValue({
      host: "192.0.2.10",
      port: 22,
      keyType: "ssh-ed25519",
      fingerprint: "SHA256:fWGOBFJCuLczHBhYs/jCgMIJ/NUUnYJmt5/owwnZAc4",
    });
    apiMocks.confirmRemoteSshFingerprint.mockImplementation(async () => {
      apiMocks.getRemoteSshTrustStatus.mockResolvedValue({
        host: "192.0.2.10",
        port: 22,
        confirmed: true,
      });
    });

    renderRemote();

    expect(await screen.findByText("確認這台主機的 SSH 金鑰")).toBeTruthy();
    expect(screen.getByText("SHA256:fWGOBFJCuLczHBhYs/jCgMIJ/NUUnYJmt5/owwnZAc4")).toBeTruthy();
    expect(screen.getByText(/一個你已經信任的管道/)).toBeTruthy();
    expect(apiMocks.inspectRemoteHost).not.toHaveBeenCalled();
    expect(screen.getByRole<HTMLButtonElement>("button", { name: "部署此主機" }).disabled).toBe(true);

    fireEvent.click(screen.getByRole("button", { name: "信任並繼續" }));
    await waitFor(() => expect(apiMocks.confirmRemoteSshFingerprint).toHaveBeenCalledWith(
      "jetson",
      "SHA256:fWGOBFJCuLczHBhYs/jCgMIJ/NUUnYJmt5/owwnZAc4",
    ));
    await waitFor(() => expect(apiMocks.inspectRemoteHost).toHaveBeenCalledWith("jetson"));
  });

  it("coalesces overlapping trust checks into one host-key scan", async () => {
    let resolveTrustStatus!: (value: { host: string; port: number; confirmed: boolean }) => void;
    apiMocks.getRemoteSshTrustStatus.mockImplementation(() => new Promise((resolve) => {
      resolveTrustStatus = resolve;
    }));

    const view = renderRemote();
    await waitFor(() => expect(apiMocks.getRemoteSshTrustStatus).toHaveBeenCalledTimes(1));

    view.rerender(<Remote refreshVersion={1} onRefreshComplete={() => {}} />);
    await waitFor(() => expect(apiMocks.discoverRemoteConnections).toHaveBeenCalledTimes(2));
    expect(apiMocks.getRemoteSshTrustStatus).toHaveBeenCalledTimes(1);

    resolveTrustStatus({ host: "192.0.2.10", port: 22, confirmed: false });
    await waitFor(() => expect(apiMocks.fetchRemoteSshFingerprint).toHaveBeenCalledTimes(1));
    expect(apiMocks.inspectRemoteHost).not.toHaveBeenCalled();
  });

  it("hard-fails a changed host key without an auto-accept button", async () => {
    apiMocks.inspectRemoteHost.mockRejectedValue(
      new Error("SshHostKeyFingerprintMismatch: 192.0.2.10:22 is no longer offering the confirmed fingerprint; refusing to trust it"),
    );
    renderRemote();

    expect(await screen.findByText("偵測失敗")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "信任並繼續" })).toBeNull();
    expect(apiMocks.confirmRemoteSshFingerprint).not.toHaveBeenCalled();
  });

  it("shows an unsupported-migration notice when a leftover broker profile was dropped", async () => {
    apiMocks.getRemoteManagerFeatureFlags.mockResolvedValue({
      nativeCodexRemoteManager: true,
      legacyBrokerHostsDropped: ["Old Broker Box"],
    });
    renderRemote();

    expect(await screen.findByText(/發現不再支援的舊 Broker 配對設定/)).toBeTruthy();
    expect(screen.getByText("Old Broker Box")).toBeTruthy();
  });

  it("exposes independent Remote Control and Official execution account selectors", async () => {
    apiMocks.listRemoteCodexAccountPairings.mockResolvedValue([
      {
        accountId: "acct-a", email: "control-a@example.com", workspaceName: "Personal",
        isDesktopDefault: true, paired: true, active: true, credentialState: "valid",
        expiresAt: 1_800_000_000, detail: null,
      },
      {
        accountId: "acct-b", email: "control-b@example.com", workspaceName: "Team",
        isDesktopDefault: false, paired: true, active: false, credentialState: "valid",
        expiresAt: 1_800_000_000, detail: null,
      },
    ]);
    apiMocks.listRemoteOfficialExecutionAccounts.mockResolvedValue([
      { accountIdHash: "a".repeat(64), displayName: "Official B1", authenticatedAt: "2026-08-11T00:00:00Z", selected: true },
      { accountIdHash: "b".repeat(64), displayName: "Official B2", authenticatedAt: "2026-08-10T00:00:00Z", selected: false },
    ]);
    renderRemote();

    expect(await screen.findByText("遠端控制")).toBeTruthy();
    openTray("進階");
    expect(screen.getByText(/Desktop 與手機必須使用相同/)).toBeTruthy();
    expect(screen.getByText("Official B1")).toBeTruthy();
    expect(screen.getByText("Official B2")).toBeTruthy();
    const controlSelector = await screen.findByDisplayValue("control-a@example.com · Personal");
    fireEvent.change(controlSelector, { target: { value: "acct-b" } });
    await waitFor(() => expect(apiMocks.activateRemoteCodexAccount).toHaveBeenCalledWith("jetson", "acct-b"));

    const executionSelector = screen.getByDisplayValue("Official B1");
    fireEvent.change(executionSelector, { target: { value: "b".repeat(64) } });
    await waitFor(() => expect(apiMocks.selectRemoteOfficialExecutionAccount).toHaveBeenCalledWith("jetson", "b".repeat(64)));

    fireEvent.change(executionSelector, { target: { value: "control" } });
    await waitFor(() => expect(apiMocks.clearRemoteOfficialExecutionAccountSelection).toHaveBeenCalledWith("jetson"));
  });

  it("starts an isolated Official execution login and polls by opaque login id", async () => {
    apiMocks.startRemoteOfficialExecutionAccountLogin.mockResolvedValue({
      loginId: "login-1",
      verificationUrl: "https://chatgpt.com/device",
      userCode: "ABCD-1234",
      expiresIn: 900,
      interval: 2,
    });
    apiMocks.pollRemoteOfficialExecutionAccountLogin.mockResolvedValue({ state: "pending", accountIdHash: null });
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      renderRemote();
      await screen.findAllByText("Jetson");
      openTray("進階");

      const nameInput = (await screen.findByPlaceholderText("帳號顯示名稱")) as HTMLInputElement;
      fireEvent.change(nameInput, { target: { value: "Official B" } });
      fireEvent.click(screen.getByText("新增 Official 執行帳號"));

      await vi.waitFor(() => expect(apiMocks.startRemoteOfficialExecutionAccountLogin).toHaveBeenCalledWith("jetson", "Official B"));
      expect(await screen.findByText("ABCD-1234")).toBeTruthy();

      apiMocks.pollRemoteOfficialExecutionAccountLogin.mockResolvedValue({ state: "authenticated", accountIdHash: "c".repeat(64) });
      await vi.advanceTimersByTimeAsync(2000);
      await vi.waitFor(() => expect(apiMocks.listRemoteOfficialExecutionAccounts).toHaveBeenCalledTimes(2));
      expect(screen.queryByText("ABCD-1234")).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("shows the device pairing code returned by remote-control pair --json", async () => {
    apiMocks.startRemoteControlPairing.mockResolvedValue({
      pairingCode: "WXYZ-9876",
      verificationUrl: "https://chatgpt.com/pair",
      expiresAt: "2026-08-11T01:00:00Z",
    });
    renderRemote();
    await inspectedWorkSurface();
    openTray("進階");

    fireEvent.click(await screen.findByText("配對這支手機"));

    expect(await screen.findByText("WXYZ-9876")).toBeTruthy();
    expect(screen.getByText("https://chatgpt.com/pair")).toBeTruthy();
  });

  it("changes models through converge without sending any renderer policy override", async () => {
    expect(apiSource).toMatch(/updateRemoteHostModels[\s\S]*"update_remote_host_models"/);
    expect(rustSource).toContain("remote::commands::update_remote_host_models");
    renderRemote();
    const label = (await screen.findByText("DeepSeek V4 Flash")).closest("label")!;
    fireEvent.click(within(label).getByRole("checkbox"));
    expect(apiMocks.updateRemoteHostModels).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: i18n.t("remoteModels.apply") }));
    await waitFor(() => expect(apiMocks.updateRemoteHostModels).toHaveBeenCalledWith("jetson", ["gpt-5.6-luna"]));
    expect(apiMocks.planRemoteDeployment).not.toHaveBeenCalled();
    expect(apiMocks.reapplyRemoteDeployment).not.toHaveBeenCalled();
    expect(await screen.findByText(i18n.t("remote.phase.waitingForIdle"))).toBeTruthy();
    // Host policy is owned by the backend; renderer sends only host and ids.
    expect(apiMocks.updateRemoteHostModels.mock.calls[0]).toHaveLength(2);
  });
});
