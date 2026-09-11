/**
 * 遠端總管的預覽台（開發用，不進打包產物）。
 *
 * 為什麼需要它：這一頁的每一個狀態都要有一台真的 Jetson 才看得到 ——
 * 未部署、進行中、失敗、被 Codex App 佔住，四種畫面各要把主機弄成
 * 那個樣子一次。結果就是「進度條看不到」「錯誤跟狀態長一樣」這種
 * 排版問題，只能等到出事的當下才發現。
 *
 * 這裡掛的是**真的 Remote 元件**、真的 api 層與真的 i18n 資源。假的只有
 * 最底下那一層：window.__TAURI_INTERNALS__.invoke 被換成 fixture，所以
 * api.ts 一行都不用改，hasTauri() 也是真的走 Tauri 那一條路。
 *
 * 開發用：
 *   pnpm dev:renderer   然後開 /preview/remote.html
 */
import { useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
import { getLocalePreference, setLocalePreference } from "@/i18n";
import { APP_LOCALES, type LocalePreference } from "@/i18n/locale";
import { Remote } from "@/screens/Remote";
import type {
  RemoteHostCandidate,
  RemoteHostStatus,
  RemoteOperationProgress,
} from "@/types";

import "@/styles/tokens.css";
import "@/styles/base.css";
import "@/styles/components.css";

/** 工作面的四個狀態，加上兩個最會出事的變體。 */
const SCENES = {
  ready: "就緒",
  fresh: "未部署",
  running: "進行中",
  failed: "失敗",
  appOwned: "被 Codex App 佔住",
  unreachable: "連不上",
} as const;
type Scene = keyof typeof SCENES;

const HOSTS: RemoteHostCandidate[] = [
  {
    codexHostId: "jetson", vellumHostId: "jetson", displayName: "Jetson Orin",
    sshAlias: "example-host", hostname: "192.0.2.10", user: "operator", port: 22,
    source: "codexApp", validated: true, validationError: null,
  },
  {
    codexHostId: null, vellumHostId: "pi5", displayName: "Raspberry Pi 5",
    sshAlias: "pi5", hostname: "192.168.1.42", user: "pi", port: 22,
    source: "openSsh", validated: true, validationError: null,
  },
];

function baseStatus(): RemoteHostStatus {
  return {
    hostId: "jetson",
    managerState: "detachedReady",
    cachedHost: { id: "jetson", name: "Jetson Orin", sshAlias: "gpu-dev" },
    agent: {
      agentVersion: "0.1.2",
      agentProtocol: 1,
      capabilities: {
        os: "linux", arch: "aarch64", dockerAvailable: true, dockerMode: "rootless",
        rootlessDocker: true, userSystemdAvailable: true, lingerEnabled: true,
        codexBinary: "/usr/local/bin/codex", codexVersion: "0.147.0",
      },
      proxy: {
        present: true, running: true, ready: true,
        image: "ghcr.io/910488/vellum/vellum-proxy:v0.1.2",
        imageDigest: `sha256:${"a".repeat(64)}`, configHash: "cfg", lastError: null,
      },
      configuration: {
        present: true, state: "current", schemaVersion: 2, requiresReconfigure: false, issue: null,
        configHash: "cfg", credentialRefs: [], credentialsReady: true,
      },
      nativeCodex: {
        codexHome: "/home/vellum-test/.codex", codexBinary: "/usr/local/bin/codex",
        codexVersion: "0.147.0", compatible: true, compatibilityReason: null,
        daemonRunning: true, daemonPid: 4242, daemonVersion: "0.147.0",
        daemonOwner: "codexCliDaemon", restartSafe: true, durable: true,
        standaloneInstalled: true,
        cliLauncher: {
          path: "/home/vellum-test/.local/bin/codex",
          target: "/home/vellum-test/.codex/packages/standalone/current/codex",
          ready: true,
        },
        remoteControlEnabled: true, activeTurn: false,
        sessionAuthority: "codexNativeDaemon", brokerEnabled: false,
      },
      // 「已安裝但還不合格」是最容易被漏掉的中間態，所以預設就掛這一種。
      grok: {
        configured: true, credentialId: "grok-cli", account: "operator@example.test",
        expiresAt: null, lastRefreshAt: null, detachedQualified: false,
        refreshTimerActive: false, loginPending: false, verificationUri: null,
        userCode: null, loginStartedAt: null, error: null,
      },
    },
    agentError: null,
    availableActions: ["installCodex", "updateComponents", "repair", "exportSupportBundle"],
    blockedReasons: [],
    inventory: {
      hostId: "jetson", agentVersion: "0.1.2", agentProtocol: 1,
      system: {
        os: "linux", arch: "aarch64", hostname: "jetson", cpuCores: 8,
        memoryBytes: 8_000_000_000, diskTotalBytes: 64_000_000_000, diskFreeBytes: 20_000_000_000,
      },
      docker: {
        available: true, mode: "rootless", rootless: true, daemon: "running",
        serverVersion: "28.2.2", clientVersion: "28.2.2", context: "default", userInDockerGroup: true,
      },
      codex: {
        binary: "/usr/local/bin/codex", version: "0.147.0", source: "path",
        codexHome: "/home/vellum-test/.codex", standaloneInstalled: true, appCliDiscoverable: true,
        appCliPath: "/home/vellum-test/.local/bin/codex", appServerSupported: true,
        compatible: true, compatibilityReason: null,
      },
      blockers: [],
      availableActions: ["installCodex", "updateComponents"],
    },
    chatgpt: {
      accountId: "acct-windows", expectedAccountId: "acct-windows",
      state: "synchronized", paired: true, active: true, loginPending: false, loginId: null,
    },
  };
}

function statusFor(scene: Scene): RemoteHostStatus {
  const status = baseStatus();
  switch (scene) {
    case "fresh":
      return {
        ...status,
        managerState: "unmanaged",
        // 乾淨新機一定同時報這三個。工作面應該一個都不顯示 ——
        // 它們是「你還沒按部署」的三種說法，不是三個錯誤。
        blockedReasons: ["proxyConfigurationMissing", "credentialsMissing", "injectionRequiresReadyProxy"],
        agent: { ...status.agent!, proxy: { ...status.agent!.proxy, running: false, ready: false } },
      };
    case "unreachable":
      return {
        ...status, managerState: "unmanaged", agent: null,
        agentError: "ssh: connect to host 192.0.2.10 port 22: Connection timed out",
        availableActions: [], blockedReasons: ["agentUnavailable"], inventory: null, chatgpt: null,
      };
    case "appOwned":
      return {
        ...status, managerState: "drifted",
        blockedReasons: ["nativeDaemonAppOwned", "codexRestartRequired"],
        agent: {
          ...status.agent!,
          nativeCodex: { ...status.agent!.nativeCodex!, daemonOwner: "codexAppDirect", restartSafe: false },
        },
      };
    default:
      return status;
  }
}

const RUNNING: RemoteOperationProgress = {
  operationId: "one-click-bootstrap-preview", hostId: "jetson", kind: "oneClickBootstrap",
  phase: "deploymentApply", percent: 62, state: "running",
  message: "Installing proxy, credentials and native catalog", result: null,
  updatedAt: new Date().toISOString(),
};

const FAILED: RemoteOperationProgress = {
  ...RUNNING, phase: "nativeDaemon", percent: 35, state: "failed",
  message: "OneClickBootstrapBlocked: nativeDaemonAppOwned; repair: stop the Codex App session first",
};

/**
 * fixture 換掉的是 Tauri 那一層，不是 api.ts —— 這樣預覽台驗證到的是
 * 真的那條路徑（hasTauri() 為真、call() 真的走 invoke）。
 */
function installFixture(getScene: () => Scene) {
  const invoke = async (cmd: string): Promise<unknown> => {
    const scene = getScene();
    switch (cmd) {
      case "get_remote_manager_feature_flags":
        return { nativeCodexRemoteManager: true, legacyBrokerHostsDropped: [] };
      case "remote_ssh_trust_status":
        return { host: "192.0.2.10", port: 22, confirmed: true };
      case "remote_ssh_fetch_fingerprint":
        return {
          host: "192.0.2.10",
          port: 22,
          keyType: "ssh-ed25519",
          fingerprint: "SHA256:fWGOBFJCuLczHBhYs/jCgMIJ/NUUnYJmt5/owwnZAc4",
        };
      case "remote_ssh_confirm_fingerprint":
        return null;
      case "get_remote_release_status":
        return {
          ready: true, trust: "bundled", releaseVersion: "v0.1.2", codexVersion: "0.147.0",
          agentVersion: "0.1.2", brokerVersion: "0.1.2",
          proxyImage: "ghcr.io/910488/vellum/vellum-proxy:v0.1.2",
          proxyDigest: `sha256:${"a".repeat(64)}`, detail: "verified",
        };
      case "discover_remote_connections":
        return HOSTS;
      case "inspect_remote_host":
        return statusFor(scene);
      case "get_remote_session_summary":
        return {
          hostId: "jetson", managerState: "detachedReady", detachedReady: true,
          observability: scene === "unreachable" ? "daemonDown" : "nativeAppServer",
          threads: scene === "unreachable" ? [] : [
            { threadId: "01JZ8QF3K2M4N6P8R0T2V4X6Z8", status: "active", activeTurn: true, activeTurnId: "turn-4f2a", turnCount: 12, lastTurnStatus: "inProgress" },
            { threadId: "01JZ8QG5L3N5P7R9T1V3X5Z7B9", status: "idle", activeTurn: false, activeTurnId: null, turnCount: 3, lastTurnStatus: "completed" },
          ],
        };
      case "bootstrap_remote_host":
      case "restore_remote_host":
      case "apply_remote_deployment":
      case "get_remote_operation":
        return scene === "failed" || scene === "appOwned" ? FAILED : RUNNING;
      case "plan_remote_deployment":
        return {
          planId: "plan-preview", hostId: "jetson", state: "readyToApply",
          desiredRevision: 4, observedRevision: 3,
          selectedCatalogIds: ["gpt-5.6-luna", "deepseek-v4-flash"],
          selectedModels: [
            { catalogId: "gpt-5.6-luna", displayName: "GPT-5.6 Luna", routeId: "official", upstreamModel: "gpt-5.6-luna", selected: true, mandatory: true },
            { catalogId: "deepseek-v4-flash", displayName: "DeepSeek V4 Flash", routeId: "cc", upstreamModel: "deepseek-v4-flash", selected: true, mandatory: false },
          ],
          credentialRequirements: [{ credentialId: "cc", kind: "apiKey", availableOnDesktop: true, detachedQualified: true }],
          drift: {
            desiredConfigHash: "b".repeat(64), remoteConfigHash: "c".repeat(64), configChanged: true,
            desiredCatalogHash: "d".repeat(64), remoteCatalogHash: "d".repeat(64), catalogChanged: false,
          },
          publicDiff: [], qualifiedCapabilities: [],
          rollbackSummary: "還原上一版的 config 與 catalog", planHash: "e".repeat(64),
          managedChanges: ["model_providers", "model_catalog"], restartRequired: true,
          blockedReasons: [], expiresAt: new Date(Date.now() + 3.6e6).toISOString(),
        };
      case "set_active_remote_host":
        return null;
      case "list_remote_official_execution_accounts":
        return scene === "fresh"
          ? []
          : [
              { accountIdHash: `sha256:${"1f".repeat(32)}`, displayName: "tester@example.com", authenticatedAt: "2026-08-30T09:12:00Z", selected: true, selectionRevision: 3, selectionVerified: true },
            ];
      // 帳號配對：五種狀態各一列，因為這張卡的排版問題只有在
      // 「已啟用／已配對／未配對／讀不到」同時出現時才看得出來。
      case "list_remote_codex_account_pairings":
        return scene === "fresh"
          ? []
          : [
              { accountId: "acct-0f1e2d", email: "tester@example.com", isDesktopDefault: true, paired: true, active: true, detail: null },
              { accountId: "acct-77aa31", email: "developer.two@example.org", isDesktopDefault: false, paired: true, active: false, detail: null },
              { accountId: "acct-9b04c8", email: "team-shared@a-rather-long-domain-name.example", isDesktopDefault: false, paired: false, active: false, detail: null },
              { accountId: "acct-320f1b", email: null, isDesktopDefault: false, paired: false, active: false, detail: "refresh token rejected: refresh_token_reused" },
            ];
      default:
        return null;
    }
  };
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = { invoke };
}

const THEMES = ["dark", "light"] as const;

function Harness() {
  const [scene, setScene] = useState<Scene>("ready");
  const [pref, setPref] = useState<LocalePreference>(getLocalePreference());
  const [theme, setTheme] = useState<(typeof THEMES)[number]>("light");
  // 換場景時整個畫面重掛，狀態才不會殘留上一個場景的 operation。
  const [nonce, setNonce] = useState(0);

  useEffect(() => {
    installFixture(() => scene);
    setNonce((current) => current + 1);
  }, [scene]);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
  }, [theme]);

  return (
    <>
      <div className="devbar">
        <label>
          <span>場景</span>
          <select value={scene} onChange={(event) => setScene(event.target.value as Scene)}>
            {Object.entries(SCENES).map(([value, label]) => (
              <option key={value} value={value}>{label}</option>
            ))}
          </select>
        </label>
        <label>
          <span>語言</span>
          <select
            value={pref}
            onChange={(event) => {
              const next = event.target.value as LocalePreference;
              setPref(next);
              void setLocalePreference(next);
            }}
          >
            {APP_LOCALES.map((locale) => <option key={locale} value={locale}>{locale}</option>)}
          </select>
        </label>
        <label>
          <span>主題</span>
          <select value={theme} onChange={(event) => setTheme(event.target.value as (typeof THEMES)[number])}>
            {THEMES.map((value) => <option key={value} value={value}>{value}</option>)}
          </select>
        </label>
      </div>
      {/* .canvas 必須待在一個高度被釘住的格線列裡，否則它不會變成捲動容器 ——
          而工作面的 sticky 是綁在那個容器上的。少了這一層，預覽台看到的
          會是一個永遠不捲的版面，也就驗不到這一版最主要的那個決定。 */}
      <div className="shell" style={{ gridTemplateColumns: "minmax(0, 1fr)", gridTemplateRows: "minmax(0, 1fr)" }}>
        <div className="canvas">
          <Remote key={`${scene}-${nonce}`} refreshVersion={0} onRefreshComplete={() => {}} />
        </div>
      </div>
    </>
  );
}

// 第一次繪製之前就要接上，否則 Remote 掛載當下的那一輪呼叫會走到
// 沒有 Tauri 的 mock 分支（回傳「功能未開啟」）。
installFixture(() => "ready");
ReactDOM.createRoot(document.getElementById("root")!).render(<Harness />);
