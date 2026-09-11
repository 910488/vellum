import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import i18n from "i18next";
// Remote 這一頁本身不 import i18n（App 進入點負責初始化），所以測試要自己接。
import "@/i18n";
import { Remote } from "@/screens/Remote";
import type { RemoteHostCandidate, RemoteHostStatus, RemoteReleaseStatus } from "@/types";
import apiSource from "../src/lib/api.ts?raw";
import rustSource from "../src-tauri/src/lib.rs?raw";

const apiMocks = vi.hoisted(() => ({
  getRemoteManagerFeatureFlags: vi.fn(),
  getRemoteReleaseStatus: vi.fn(),
  discoverRemoteConnections: vi.fn(),
  inspectRemoteHost: vi.fn(),
  getDesktopCodexCompatibility: vi.fn(),
  updateRemoteCodexForDesktop: vi.fn(),
  getRemoteSessionSummary: vi.fn(),
  bootstrapRemoteHost: vi.fn(),
  restoreRemoteHost: vi.fn(),
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
  blockedReasons: ["credentialMissing:cc"],
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

/** 維護動作收在 <details> 裡。測試要先把它打開，就跟使用者一樣。 */
function openTray(label: string | RegExp) {
  fireEvent.click(screen.getByText(label));
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

  it("registers the bundled release readiness command used by the renderer", () => {
    expect(apiSource).toMatch(/getRemoteReleaseStatus[\s\S]*"get_remote_release_status"/);
    expect(rustSource).toMatch(/remote::commands::get_remote_release_status/);
    expect(apiSource).toMatch(/getDesktopCodexCompatibility[\s\S]*"get_desktop_codex_compatibility"/);
    expect(apiSource).toMatch(/updateRemoteCodexForDesktop[\s\S]*"update_remote_codex_for_desktop"/);
    expect(rustSource).toMatch(/remote::commands::get_desktop_codex_compatibility/);
    expect(rustSource).toMatch(/remote::commands::update_remote_codex_for_desktop/);
  });

  it("paints discovered hosts while the selected SSH probe is still pending", async () => {
    let resolveInspection!: (value: RemoteHostStatus) => void;
    apiMocks.inspectRemoteHost.mockReturnValue(new Promise((resolve) => {
      resolveInspection = resolve;
    }));

    renderRemote();

    expect(screen.getByText(/正在讀取 Codex\/OpenSSH 連線設定/)).toBeTruthy();
    expect((await screen.findAllByText("Jetson")).length).toBeGreaterThan(0);
    // 探測狀態標在那一台主機的那一列上，不再是一則全頁通報。
    expect(await screen.findByText("探測中…")).toBeTruthy();

    resolveInspection(status);
    await waitFor(() => expect(screen.queryByText("探測中…")).toBeNull());
  });

  it("keeps the last-good status visible with a live indicator while re-probing an already-probed host", async () => {
    const view = renderRemote();

    expect(await screen.findByText(/Codex App 目前使用此主機的 native daemon/)).toBeTruthy();
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
    expect(screen.getByText(/Codex App 目前使用此主機的 native daemon/)).toBeTruthy();
    expect(screen.getByText("credential cc is unavailable")).toBeTruthy();
    expect(screen.queryByText("探測中…")).toBeNull();
    expect(await screen.findByText(/· 更新中/)).toBeTruthy();

    resolveSecondProbe(status);
    await waitFor(() => expect(screen.queryByText(/· 更新中/)).toBeNull());
    expect(screen.getByText(/Codex App 目前使用此主機的 native daemon/)).toBeTruthy();
  });

  it("keeps a failed probe on the host row instead of raising a page-level alert", async () => {
    apiMocks.inspectRemoteHost.mockRejectedValue(new Error("ssh: connect timeout"));
    renderRemote();

    expect(await screen.findByText("探測失敗")).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("answers readiness in one line and hides the nineteen detail rows behind a tray", async () => {
    renderRemote();

    expect(await screen.findByText(/Codex App 目前使用此主機的 native daemon/)).toBeTruthy();
    // 細節預設收起來。jsdom 不套 <details> 的預設樣式，所以節點還是查得到 ——
    // 要斷言的是「它在一個關著的托盤裡」，不是「它不在 DOM 裡」。
    const runtime = screen.getByText("CODEX_HOME").closest("details");
    expect(runtime?.open).toBe(false);
    openTray("執行環境");
    expect(runtime?.open).toBe(true);
    expect(screen.getByText("/home/vellum-test/.codex")).toBeTruthy();
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

    expect(await screen.findByText(/Codex App 目前使用此主機的 native daemon/)).toBeTruthy();
    openTray("執行環境");
    expect(screen.getByText("設定")).toBeTruthy();
    expect(screen.getByText("遠端 Proxy 設定需要安全升級，重新部署即可自動修復。")).toBeTruthy();
  });

  it("shows nothing extra for a current configuration -- the healthy case stays silent", async () => {
    renderRemote();

    expect(await screen.findByText(/Codex App 目前使用此主機的 native daemon/)).toBeTruthy();
    openTray("執行環境");
    expect(screen.queryByText("設定")).toBeNull();
  });

  it("renders native thread detail and fail-closes release operations", async () => {
    renderRemote();

    expect(await screen.findByText("thread-native-123")).toBeTruthy();
    expect(screen.getByText(/7 個 turn · 最後一個 inProgress/)).toBeTruthy();
    expect(screen.getByText("credential cc is unavailable")).toBeTruthy();

    openTray("維護");
    expect(await screen.findByText(/內附的部署包尚未通過簽章驗證/)).toBeTruthy();
    expect((screen.getByRole("button", { name: "安裝 pinned Codex CLI" }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole("button", { name: "更新遠端 Agent" }) as HTMLButtonElement).disabled).toBe(true);
  });

  it("turns an unrecognised blocker code into a plain sentence with a remedy", async () => {
    apiMocks.inspectRemoteHost.mockResolvedValue({
      ...status,
      blockedReasons: ["nativeDaemonAppOwned"],
    });
    renderRemote();

    expect(await screen.findByText(/Codex App 目前直接持有此主機的 app-server/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "安全停止並重試" })).toBeTruthy();
  });

  it("pairs the desktop-selected ChatGPT identity without exposing a token", async () => {
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
    apiMocks.startRemoteCodexAccountLogin.mockResolvedValue({
      loginId: "login-1",
      verificationUrl: "https://auth.openai.com/device",
      userCode: "ABCD-EFGH",
      expectedAccountId: "acct-windows",
    });
    const open = vi.spyOn(window, "open").mockImplementation(() => null);
    renderRemote();

    fireEvent.click(await screen.findByRole("button", { name: "配對桌面端選定的 ChatGPT 帳號" }));
    // 單一帳號那條路不帶 account id：後端會落回 Desktop 目前的預設帳號，
    // 這正是「保留原本一次配對一個」的意思。
    await waitFor(() => expect(apiMocks.startRemoteCodexAccountLogin)
      .toHaveBeenCalledWith("jetson", undefined));
    expect(open).toHaveBeenCalledWith(
      "https://auth.openai.com/device",
      "_blank",
      "noopener,noreferrer",
    );
    expect(await screen.findByText("ABCD-EFGH")).toBeTruthy();
    expect(screen.queryByText(/refresh_token|access_token/)).toBeNull();
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
      { accountId: "acct-a", email: "a@example.com", isDesktopDefault: true, paired: true, active: true, detail: null },
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

  it("locks Grok login immediately and shows CLI progress until the URL is returned", async () => {
    let resolveLogin!: (value: unknown) => void;
    apiMocks.startRemoteGrokLogin.mockReturnValue(new Promise((resolve) => {
      resolveLogin = resolve;
    }));
    renderRemote();

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
    fireEvent.click(await screen.findByRole("button", { name: "變更模型…" }));
    expect(await screen.findByText("5.6 Luna")).toBeTruthy();
    expect(screen.getByText("DeepSeek V4 Flash")).toBeTruthy();
    const luna = screen.getByText("5.6 Luna").closest("label")?.querySelector("input");
    expect(luna?.checked).toBe(true);
    expect(luna?.disabled).toBe(true);
  });

  it("confirms deployment with three scannable facts instead of a native dialog", async () => {
    const confirm = vi.spyOn(window, "confirm");
    renderRemote();

    fireEvent.click(await screen.findByRole("button", { name: "重新同步" }));
    const modal = dialog();
    expect(modal.getByText("會改")).toBeTruthy();
    expect(modal.getByText("不動")).toBeTruthy();
    expect(modal.getByText("進行中的 turn")).toBeTruthy();
    expect(modal.getByText(/會被拒絕，不會強制中斷/)).toBeTruthy();
    expect(confirm).not.toHaveBeenCalled();
    expect(apiMocks.bootstrapRemoteHost).not.toHaveBeenCalled();
  });

  it("starts one-click bootstrap as an in-place background operation", async () => {
    renderRemote();

    const button = await screen.findByRole("button", { name: "重新同步" });
    fireEvent.click(button);
    fireEvent.click(dialog().getByRole("button", { name: "重新同步" }));

    await waitFor(() => expect(apiMocks.bootstrapRemoteHost).toHaveBeenCalledWith("jetson"));
    // 進度原地取代按鈕：階段、百分比與已經過的時間都在工作面上。
    expect(await screen.findByText("正在部署")).toBeTruthy();
    expect(screen.getByText("排入佇列")).toBeTruthy();
    expect(screen.getByText(/0% · 已經過 0:0\d/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "重新同步" })).toBeNull();
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

    fireEvent.click(await screen.findByRole("button", { name: "重新同步" }));
    fireEvent.click(dialog().getByRole("button", { name: "重新同步" }));

    // 失敗原地顯示，說出停在哪一步，並附上補救動作。使用者沒按任何東西之前，
    // 不會有任何視窗自己跳出來。
    expect(await screen.findByText(
      "Codex App 目前直接持有此主機的 app-server，Vellum 無法接管。",
      {},
      { timeout: 2500 },
    )).toBeTruthy();
    expect(document.querySelector("dialog[open]")).toBeNull();
    expect(apiMocks.stopRemoteAppOwnedCodex).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "安全停止並重試" }));
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

    fireEvent.click(await screen.findByRole("button", { name: "重新同步" }));
    fireEvent.click(dialog().getByRole("button", { name: "重新同步" }));

    expect(await screen.findByText(
      "OneClickBootstrapBlocked: nativeDaemonAppOwned",
      {},
      { timeout: 2500 },
    )).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "安全停止並重試" }));
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

    fireEvent.click(await screen.findByRole("button", { name: "重新同步" }));
    fireEvent.click(dialog().getByRole("button", { name: "重新同步" }));

    const mismatch = await screen.findByText(
      "遠端 Codex runtime 與目前 Desktop 協議不相容。請先同步 Desktop Codex runtime，再重新操作。",
      {},
      { timeout: 2500 },
    );
    expect(mismatch).toBeTruthy();
    expect(screen.queryByRole("button", { name: "重試" })).toBeNull();
    fireEvent.click(within(mismatch.closest('[role="alert"]') as HTMLElement)
      .getByRole("button", { name: "同步 Desktop Codex runtime" }));
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
    apiMocks.getRemoteOperation.mockResolvedValue({
      operationId: "one-click-bootstrap-1", hostId: "jetson", kind: "oneClickBootstrap",
      phase: "hostPreflight", percent: 5, state: "failed",
      message: "RemoteBoundaryKeyProvisionFailed: agent failed: connection refused",
      result: null, updatedAt: "2026-08-14T00:00:01Z",
    });
    renderRemote();

    fireEvent.click(await screen.findByRole("button", { name: "重新同步" }));
    fireEvent.click(dialog().getByRole("button", { name: "重新同步" }));

    const notice = await screen.findByText(
      "無法修復遠端 Proxy 驗證金鑰。請重試；若持續失敗，請匯出診斷包協助排查。",
      {},
      { timeout: 2500 },
    );
    expect(notice).toBeTruthy();
    // The raw agent reason must not be swallowed by the friendly message —
    // it stays available via the alert's `raw` prop for diagnosis, distinct
    // from a completely separate channel: the host-probe "Agent unreachable"
    // signal, which this test never touches (no host-probe mock changed).
    const alert = notice.closest('[role="alert"]') as HTMLElement;
    expect(within(alert).getByRole("button", { name: "重試" })).toBeTruthy();
    expect(within(alert).getByRole("button", { name: "匯出診斷包" })).toBeTruthy();
    expect(within(alert).queryByRole("button", { name: "同步 Desktop Codex runtime" })).toBeNull();
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
    apiMocks.bootstrapRemoteHost.mockRejectedValue(new Error(
      "nativeDaemonAppOwned: the running app-server is not held by Codex's daemon, so stop it explicitly before taking over",
    ));
    renderRemote();

    fireEvent.click(await screen.findByRole("button", { name: "重新同步" }));
    fireEvent.click(dialog().getByRole("button", { name: "重新同步" }));

    const notice = await screen.findByText(
      "Codex App 目前直接持有此主機的 app-server，Vellum 無法接管。",
      {},
      { timeout: 2500 },
    );
    const alert = notice.closest('[role="alert"]') as HTMLElement;
    expect(within(alert).getByRole("button", { name: "重試" })).toBeTruthy();

    fireEvent.click(within(alert).getByRole("button", { name: "安全停止並重試" }));
    fireEvent.click(dialog().getByRole("button", { name: "安全停止並重試" }));

    await waitFor(() => expect(apiMocks.stopRemoteAppOwnedCodex).toHaveBeenCalledWith(
      "jetson",
      expect.stringMatching(/^stop-app-owned-/),
    ));
  });

  it("enables install and agent update only with a complete deployment bundle", async () => {
    apiMocks.getRemoteReleaseStatus.mockResolvedValue({
      ...blockedRelease,
      ready: true,
      releaseVersion: "v0.1.2",
      codexVersion: "0.147.0",
      agentVersion: "0.1.2",
      brokerVersion: "0.1.2",
      proxyImage: "ghcr.io/910488/vellum/vellum-proxy:v0.1.2",
      proxyDigest: `sha256:${"a".repeat(64)}`,
      detail: "verified",
    });
    renderRemote();

    await screen.findByText(/Codex App 目前使用此主機的 native daemon/);
    openTray("維護");
    await waitFor(() => expect((screen.getByRole("button", { name: "安裝 pinned Codex CLI" }) as HTMLButtonElement).disabled).toBe(false));
    expect((screen.getByRole("button", { name: "更新遠端 Agent" }) as HTMLButtonElement).disabled).toBe(false);
    expect(screen.queryByText(/內附的部署包尚未通過簽章驗證/)).toBeNull();
  });

  it("prompts the user to resync when the bundled proxy image changed", async () => {
    apiMocks.getRemoteReleaseStatus.mockResolvedValue({
      ...blockedRelease,
      ready: true,
      releaseVersion: "0.2.2",
      proxyImage: "vellum-proxy:0.2.2-new-source",
      proxyDigest: `sha256:${"b".repeat(64)}`,
      detail: "verified",
    });

    renderRemote();

    expect(await screen.findByText(
      "此版本內含較新的 Proxy image，請按「重新同步」套用到這台主機。",
    )).toBeTruthy();
    expect(screen.getAllByRole("button", { name: "重新同步" }).length).toBeGreaterThan(1);
  });

  it("offers an exact Desktop runtime sync when remote protocol qualification is stale", async () => {
    apiMocks.getDesktopCodexCompatibility.mockResolvedValue({
      state: "updateAvailable",
      desktopVersion: "0.147.0-alpha.6.5",
      desktopSchemaSha256: "new-desktop-schema",
      remoteVersion: "0.147.0",
      requiredRemoteVersion: "0.147.0-alpha.6.5",
      remoteArch: "aarch64",
      agentProtocol: 2,
      canUpdate: true,
      detail: "official artifact available",
    });
    renderRemote();

    await waitFor(() => expect(apiMocks.getDesktopCodexCompatibility).toHaveBeenCalledWith("jetson"));
    openTray("維護");
    fireEvent.click(screen.getByRole("button", { name: "同步 Desktop Codex runtime" }));
    fireEvent.click(dialog().getByRole("button", { name: "同步 Desktop Codex runtime" }));

    await waitFor(() => expect(apiMocks.updateRemoteCodexForDesktop).toHaveBeenCalledWith("jetson"));
    expect(await screen.findByText("正在同步 Desktop Codex runtime")).toBeTruthy();
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
    expect(screen.queryByRole("button", { name: "重新同步" })).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "信任並繼續" }));
    await waitFor(() => expect(apiMocks.confirmRemoteSshFingerprint).toHaveBeenCalledWith(
      "jetson",
      "SHA256:fWGOBFJCuLczHBhYs/jCgMIJ/NUUnYJmt5/owwnZAc4",
    ));
    await waitFor(() => expect(apiMocks.inspectRemoteHost).toHaveBeenCalledWith("jetson"));
  });

  it("hard-fails a changed host key without an auto-accept button", async () => {
    apiMocks.inspectRemoteHost.mockRejectedValue(
      new Error("SshHostKeyFingerprintMismatch: 192.0.2.10:22 is no longer offering the confirmed fingerprint; refusing to trust it"),
    );
    renderRemote();

    expect(await screen.findByText("探測失敗")).toBeTruthy();
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

  it("separates fixed Remote control A from selectable Official execution B", async () => {
    apiMocks.listRemoteOfficialExecutionAccounts.mockResolvedValue([
      { accountIdHash: "a".repeat(64), displayName: "Official B1", authenticatedAt: "2026-08-11T00:00:00Z", selected: true },
      { accountIdHash: "b".repeat(64), displayName: "Official B2", authenticatedAt: "2026-08-10T00:00:00Z", selected: false },
    ]);
    renderRemote();

    expect(await screen.findByText("Remote 控制")).toBeTruthy();
    expect(screen.getByText(/Desktop 與手機必須使用相同/)).toBeTruthy();
    expect(screen.getByText("Official B1")).toBeTruthy();
    expect(screen.getByText("Official B2")).toBeTruthy();
    expect(screen.queryByText("配對新帳號")).toBeNull();

    fireEvent.click(screen.getByText("選用"));
    await waitFor(() => expect(apiMocks.selectRemoteOfficialExecutionAccount).toHaveBeenCalledWith("jetson", "b".repeat(64)));
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

      const nameInput = (await screen.findByPlaceholderText("這個 Official 帳號的顯示名稱")) as HTMLInputElement;
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
    await screen.findAllByText("Jetson");

    fireEvent.click(await screen.findByText("配對這支手機"));

    expect(await screen.findByText("WXYZ-9876")).toBeTruthy();
    expect(screen.getByText("https://chatgpt.com/pair")).toBeTruthy();
  });

  it("reapplies policy-only Auto Review drift without resending an empty policy override", async () => {
    // Local Auto Review settings changed but the model selection did not:
    // the "更新預覽" button must stay enabled off `reviewPolicyChanged`
    // alone, and must call `reapplyRemoteDeployment` (which reuses this
    // host's saved overrides) rather than `planRemoteDeployment` with
    // `policy: {}` (which would reset e.g. a saved `autoReviewEnabled:
    // false` override back to enabled).
    apiMocks.planRemoteDeployment.mockResolvedValue({
      planId: "plan-1", hostId: "jetson", state: "readyToApply",
      desiredRevision: 1, observedRevision: 3,
      selectedCatalogIds: ["gpt-5.6-luna", "vlm-third-party"],
      selectedModels: [
        { catalogId: "gpt-5.6-luna", displayName: "5.6 Luna", routeId: "official", upstreamModel: "gpt-5.6-luna", selected: true, mandatory: true },
        { catalogId: "vlm-third-party", displayName: "DeepSeek V4 Flash", routeId: "cc", upstreamModel: "deepseek-v4-flash", selected: true, mandatory: false },
      ],
      credentialRequirements: [],
      drift: {
        desiredConfigHash: "a".repeat(64), remoteConfigHash: "a".repeat(64), configChanged: false,
        desiredCatalogHash: "b".repeat(64), remoteCatalogHash: "b".repeat(64), catalogChanged: false,
        reviewPolicyFingerprint: "fp-2", reviewPolicyChanged: true,
      },
      publicDiff: [], qualifiedCapabilities: [], rollbackSummary: "restore", planHash: "c".repeat(64),
      managedChanges: [], restartRequired: false, blockedReasons: [], expiresAt: "2026-08-11T01:00:00Z",
    });
    apiMocks.reapplyRemoteDeployment.mockResolvedValue({
      planId: "plan-2", hostId: "jetson", state: "readyToApply",
      desiredRevision: 2, observedRevision: 3,
      selectedCatalogIds: ["gpt-5.6-luna", "vlm-third-party"],
      selectedModels: [
        { catalogId: "gpt-5.6-luna", displayName: "5.6 Luna", routeId: "official", upstreamModel: "gpt-5.6-luna", selected: true, mandatory: true },
        { catalogId: "vlm-third-party", displayName: "DeepSeek V4 Flash", routeId: "cc", upstreamModel: "deepseek-v4-flash", selected: true, mandatory: false },
      ],
      credentialRequirements: [],
      drift: {
        desiredConfigHash: "a".repeat(64), remoteConfigHash: "a".repeat(64), configChanged: false,
        desiredCatalogHash: "b".repeat(64), remoteCatalogHash: "b".repeat(64), catalogChanged: false,
        reviewPolicyFingerprint: "fp-2", reviewPolicyChanged: false,
      },
      publicDiff: [], qualifiedCapabilities: [], rollbackSummary: "restore", planHash: "d".repeat(64),
      managedChanges: [], restartRequired: false, blockedReasons: [], expiresAt: "2026-08-11T01:00:00Z",
    });
    expect(apiSource).toMatch(/reapplyRemoteDeployment[\s\S]*"reapply_remote_deployment"/);
    expect(rustSource).toMatch(/remote::commands::reapply_remote_deployment/);

    renderRemote();
    await screen.findAllByText("Jetson");

    fireEvent.click(await screen.findByText("變更模型…"));
    await waitFor(() => expect(apiMocks.planRemoteDeployment).toHaveBeenCalledTimes(1));
    await screen.findByText("設定已變更 — 待重新套用");

    const replanButton = (await screen.findByText("更新預覽")).closest("button");
    expect(replanButton).not.toBeNull();
    expect((replanButton as HTMLButtonElement).disabled).toBe(false);

    fireEvent.click(replanButton!);
    await waitFor(() => expect(apiMocks.reapplyRemoteDeployment).toHaveBeenCalledWith("jetson"));
    expect(apiMocks.planRemoteDeployment).toHaveBeenCalledTimes(1);

    await waitFor(() => expect(screen.queryByText("設定已變更 — 待重新套用")).toBeNull());
  });
});
