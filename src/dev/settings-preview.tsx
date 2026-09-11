/**
 * Enhanced Codex Runtime 注入段的預覽台（開發用，不進打包產物）。
 *
 * 為什麼需要它：這張卡的每一個狀態都要真的把機器弄成那個樣子才看得到 ——
 * 殘留舊 bridge、產物雜湊對不上、Desktop 沒接上、bridge 起不來，每一種
 * 都得先把 CODEX_CLI_PATH 弄壞一次。結果就是「兩行互相矛盾」「按鈕在
 * 另一張卡」這種問題，只能等到卡住的當下才發現。
 *
 * 這裡掛的是**真的 Settings 元件**、真的 i18n 資源與真的 api 瀏覽器 mock。
 * 假的只有一個：api.getEnhancedDesktopRuntimeStatus 換成場景 fixture。
 * 沒有裝 __TAURI_INTERNALS__，所以其餘每一張卡都走 api.ts 既有的 mock，
 * 不用在這裡重寫一份會跟本體走鐘的假資料。
 *
 * 開發用：
 *   pnpm dev:renderer   然後開 /preview/settings.html
 */
import { useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
import { getLocalePreference, setLocalePreference } from "@/i18n";
import { APP_LOCALES, type LocalePreference } from "@/i18n/locale";
import { api } from "@/lib/api";
import { Settings } from "@/screens/Settings";
import { StatusBar } from "@/components/StatusBar";
import { EMPTY_STATUS } from "@/lib/status";
import type { EnhancedDesktopRuntimeStatus } from "@/types";

import "@/styles/tokens.css";
import "@/styles/base.css";
import "@/styles/components.css";

const OFFICIAL = "C:\\Users\\developer\\AppData\\Local\\OpenAI\\Codex\\bin\\example\\codex.exe";
const ENHANCED = "C:\\Users\\developer\\codex-enhanced\\target\\release\\codex.exe";
const BRIDGE =
  "C:\\Users\\developer\\src\\vellum\\src-tauri\\binaries\\dev\\vellum-codex-app-server-example.exe";
const STALE_BRIDGE =
  "C:\\Users\\developer\\src\\vellum\\target\\debug\\vellum-codex-app-server.exe";
const DIGEST = "sha256:13e0e92d5b21b905848fd5c4a9198d7cde0366e7d1e2c6dbe6b31f675f3f42c0";
const PROTOCOL = "sha256:0d00aee4fbb8c9de8634c05234acf535f9998ec7fa0b0ba07f37b748e31ee0b5";

const SCENES = {
  unconfigured: "還沒設定過",
  coreMissing: "這個組建沒帶 Enhanced core",
  artifactBlocked: "帶了，但雜湊對不上",
  verified: "驗過了，還沒注入",
  leased: "接管了路徑，Desktop 還沒接上",
  active: "已注入",
  armed: "接管好了，Desktop 是更早之前開的",
  proxyStopped: "Proxy 沒有啟動",
  missingHelpers: "少了官方安裝有的輔助程式",
  staleBridge: "殘留舊 bridge（卡住的那一種）",
  bridgeFailed: "Bridge 起不來",
  unverified: "在跑，但這個 Codex 版本沒驗過",
  incompatible: "Codex 改掉了要路由的方法",
} as const;
type Scene = keyof typeof SCENES;

function base(): EnhancedDesktopRuntimeStatus {
  return {
    configured: true,
    enabled: true,
    artifactReady: true,
    active: false,
    bridgeObserved: false,
    ready: false,
    activationState: "awaitingDesktopRestart",
    environmentState: "released",
    environmentValue: null,
    observedBridgeExecutable: null,
    restartRequired: true,
    launchId: null,
    bridgeState: null,
    bridgePid: null,
    officialChildPid: null,
    enhancedChildPid: null,
    enhancedRuntimeDigest: DIGEST,
    activeRuntimeDigest: null,
    activeFeatureProfile: null,
    officialCodexExecutable: OFFICIAL,
    enhancedCodexExecutable: ENHANCED,
    coreAvailable: true,
    bridgeExecutable: BRIDGE,
    lastQualification: null,
    missingHelpers: [],
    launchCoreDrift: false,
    protocol: {
      verdict: "verified",
      pinnedSchemaSha256: PROTOCOL.replace("sha256:", ""),
      desktopSchemaSha256: PROTOCOL.replace("sha256:", ""),
      deltas: [],
    },
    serving: false,
    unverified: false,
    blockers: [],
  };
}

function statusFor(scene: Scene): EnhancedDesktopRuntimeStatus {
  const status = base();
  switch (scene) {
    case "unconfigured":
      return {
        ...status,
        configured: false,
        enabled: false,
        artifactReady: false,
        activationState: "disabled",
        restartRequired: false,
        enhancedRuntimeDigest: null,
      };
    // core 由組建自己帶，所以「沒有」是安裝壞了，不是還沒設定完。這一格
    // 存在，是因為那兩句話在畫面上必須長得不一樣。
    case "coreMissing":
      return {
        ...status,
        configured: false,
        enabled: false,
        artifactReady: false,
        activationState: "disabled",
        restartRequired: false,
        enhancedCodexExecutable: null,
        coreAvailable: false,
        enhancedRuntimeDigest: null,
        blockers: ["this Vellum build does not contain the pinned Enhanced Codex core"],
      };
    case "artifactBlocked":
      return {
        ...status,
        artifactReady: false,
        activationState: "artifactBlocked",
        enhancedRuntimeDigest: null,
        blockers: [
          "Enhanced artifact mismatch: expected sha256:e03b7325acb3002cf4f5fdd0306369da1b22fa15dfe715dbcc02f5bab4d24886, got sha256:71c0a4c0f2f1b0b4d3f1b2b8f0a9e2c1d4e5f60718293a4b5c6d7e8f90a1b2c3",
        ],
      };
    case "verified":
      return status;
    /* 這一格是這次改動的重點：Codex Desktop 自己更新過，協定不再逐位元組
       相同，但差的每一項都不在 bridge 要分派的方法上。舊的做法會在這裡
       整組解除武裝；現在它跑，而畫面必須說清楚它沒被驗證過。 */
    case "unverified":
      return {
        ...status,
        active: true,
        bridgeObserved: true,
        ready: true,
        restartRequired: false,
        activationState: "active",
        environmentState: "leased",
        environmentValue: BRIDGE,
        activeRuntimeDigest: DIGEST,
        activeFeatureProfile: "E5",
        serving: true,
        bridgePid: 65056,
        officialChildPid: 60484,
        enhancedChildPid: 47176,
        unverified: true,
        protocol: {
          verdict: "unverified",
          pinnedSchemaSha256: PROTOCOL.replace("sha256:", ""),
          desktopSchemaSha256:
            "2bcdf66801b3d139fcda1a381e59aba6de0d223ec40c186e938ad3f487b4dbec",
          deltas: [
            {
              kind: "fieldRemoved",
              subject: "McpServerElicitationRequestParams/oneOf[2]",
              fields: ["elicitationId", "url"],
              routed: false,
            },
            {
              kind: "methodUnservable",
              subject: "plugin/reconcile",
              fields: [],
              routed: false,
            },
          ],
        },
      };
    /* 差異落在要路由的方法上就沒有降級可言，所以這一格是回退：Codex 照常
       跑，Enhanced 的功能一個都不在。這句話跟上面那句不能長得一樣。 */
    case "incompatible":
      return {
        ...status,
        artifactReady: false,
        activationState: "artifactBlocked",
        protocol: {
          verdict: "incompatible",
          pinnedSchemaSha256: PROTOCOL.replace("sha256:", ""),
          desktopSchemaSha256:
            "9f21c0ab44de5518e3a7cd7c0f9b1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f607182",
          deltas: [
            {
              kind: "methodUnknownToDesktop",
              subject: "thread/resume",
              fields: [],
              routed: true,
            },
            {
              kind: "methodUnservable",
              subject: "thread/handoff",
              fields: [],
              routed: false,
            },
          ],
        },
        blockers: [
          "Codex Desktop broke a method the bridge routes: the Enhanced core may send thread/resume, which Desktop no longer declares",
        ],
      };
    case "leased":
      return { ...status, environmentState: "leased", environmentValue: BRIDGE };
    // 接管完成，但眼前這個 Codex Desktop 是在那之前開的 —— 它下次啟動就會
    // 走 Enhanced。這不是失敗，畫面不能把它講成失敗。
    case "armed":
      return {
        ...status,
        environmentState: "leased",
        environmentValue: BRIDGE,
        activationState: "awaitingDesktopRestart",
        /* 真的撞到過的組合:接管好了、上一輪的 bridge 還在、協定未驗證、
           而且留了一串 attestation 對不上的 blocker。這一格會抓回歸，因為
           把回退寫成「enabled 但還沒 ready」的話，這裡就會同時說啟動路徑
           已接管、和已回到原生 Codex。 */
        serving: true,
        bridgePid: 65056,
        bridgeState: "ready",
        officialChildPid: 60484,
        enhancedChildPid: 47176,
        unverified: true,
        protocol: {
          verdict: "unverified",
          pinnedSchemaSha256: PROTOCOL.replace("sha256:", ""),
          desktopSchemaSha256:
            "2bcdf66801b3d139fcda1a381e59aba6de0d223ec40c186e938ad3f487b4dbec",
          deltas: [
            {
              kind: "fieldRemoved",
              subject: "McpServerElicitationRequestParams/oneOf[2]",
              fields: ["elicitationId", "url"],
              routed: false,
            },
            {
              kind: "methodUnservable",
              subject: "plugin/reconcile",
              fields: [],
              routed: false,
            },
          ],
        },
        blockers: [
          "bridge attestation is for launch 01M1N9Q01FJBTD5N9NY8KFSEEJ, expected 01M1NDEN8GYTK55ZE9XZHSXPNT",
          "bridge process 65056 is no longer running",
          "the running bridge was not started by Codex Desktop (EnhancedDesktopBridgeNotObserved)",
          "bridge attestation digests do not match the launch manifest",
        ],
      };
    // Proxy 停著的時候接管會被刻意釋放，所以注入在這個狀態下不可能成功。
    case "proxyStopped":
      return { ...status, environmentState: "released", environmentValue: null };
    // fork 只 build 了 codex.exe。對話跟路由都好，壞的是沙箱指令 —— 而那個
    // 要等到第一個 shell 指令才會現形，所以這裡就要說。
    case "missingHelpers":
      return {
        ...status,
        active: true,
        bridgeObserved: true,
        ready: true,
        activationState: "active",
        environmentState: "leased",
        environmentValue: BRIDGE,
        missingHelpers: [
          "codex-code-mode-host.exe",
          "codex-command-runner.exe",
          "codex-windows-sandbox-setup.exe",
        ],
      };
    case "active":
      return {
        ...status,
        active: true,
        bridgeObserved: true,
        ready: true,
        activationState: "active",
        environmentState: "leased",
        environmentValue: BRIDGE,
        restartRequired: false,
        launchId: "01M1GVDCSXM674VNYZ4BWPKECB",
        bridgeState: "ready",
        bridgePid: 48356,
        officialChildPid: 58288,
        enhancedChildPid: 46724,
        observedBridgeExecutable: BRIDGE,
        activeRuntimeDigest: DIGEST,
        activeFeatureProfile: "E5",
        lastQualification: {
          runId: "01M1H1TSJ2Q0J3W2N9V8XK4C7B",
          mode: "installed",
          startedAt: 1_756_800_000,
          finishedAt: 1_756_800_420,
          passed: true,
          promotionReady: true,
          reportPath: "C:\\Users\\developer\\AppData\\Local\\Vellum\\enhanced-runtime\\qualifications\\example\\report.json",
          failures: [],
        },
      };
    // 使用者實際卡住的那一格：產物驗過了，Desktop 手上也真的有一個活著的
    // Vellum bridge，但那是舊組建留下來的，而 CODEX_CLI_PATH 裡沒有 lease。
    case "staleBridge":
      return {
        ...status,
        activationState: "environmentDrift",
        environmentState: "orphanedBridge",
        environmentValue: STALE_BRIDGE,
        observedBridgeExecutable: STALE_BRIDGE,
        launchId: "01M1GVDCSXM674VNYZ4BWPKECB",
        bridgeState: "ready",
        bridgePid: 48356,
        officialChildPid: 58288,
        enhancedChildPid: 46724,
        blockers: [
          `a Vellum bridge remains in CODEX_CLI_PATH without an ownership lease: ${STALE_BRIDGE}`,
          `the live bridge runs from ${STALE_BRIDGE}, not the configured App Server bridge`,
        ],
      };
    case "bridgeFailed":
      return {
        ...status,
        activationState: "failed",
        environmentState: "leased",
        environmentValue: BRIDGE,
        launchId: "01M1H1TSJ2Q0J3W2N9V8XK4C7B",
        bridgeState: "failed",
        bridgePid: 51204,
        observedBridgeExecutable: BRIDGE,
        blockers: [
          "the Enhanced child exited before initialize: error: unknown flag `--experimental-json`",
        ],
      };
  }
}

/** 只換這兩個回傳值；其餘每一張卡都留給 api.ts 既有的瀏覽器 mock。
 *
 * Proxy 也要換，因為「注入」能不能成立取決於它 —— Proxy 停著的時候
 * `sync_desktop_launch` 會刻意釋放接管而不是拿下它。 */
function installFixture(scene: () => Scene) {
  api.getEnhancedDesktopRuntimeStatus = () =>
    new Promise((resolve) => setTimeout(() => resolve(statusFor(scene())), 120));
  const proxy = api.getProxyStatus.bind(api);
  api.getProxyStatus = async () => ({
    ...(await proxy()),
    running: scene() !== "proxyStopped",
  });
}

const THEMES = ["light", "dark"] as const;

function Harness() {
  const [scene, setScene] = useState<Scene>("staleBridge");
  const [pref, setPref] = useState<LocalePreference>(getLocalePreference());
  const [theme, setTheme] = useState<(typeof THEMES)[number]>("light");
  // 換場景時整張畫面重掛，路徑欄位才不會殘留上一個場景填好的值。
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
          <select
            value={theme}
            onChange={(event) => setTheme(event.target.value as (typeof THEMES)[number])}
          >
            {THEMES.map((value) => <option key={value} value={value}>{value}</option>)}
          </select>
        </label>
      </div>
      {/* 左上角那顆按鈕跟這張卡講的是同一件事，所以要能一起看：協定檢測的
          判定與差異只出現在它的浮層裡，光看 Settings 是驗不到的。 */}
      <StatusBar
        status={{ ...EMPTY_STATUS, enhancedRuntime: statusFor(scene), proxy: { running: scene !== "proxyStopped" } as never }}
        refreshing={false}
        refreshError={null}
        onRefresh={() => {}}
      />
      <div
        className="shell"
        style={{ gridTemplateColumns: "minmax(0, 1fr)", gridTemplateRows: "minmax(0, 1fr)" }}
      >
        <div className="canvas">
          <Settings
            key={`${scene}-${nonce}`}
            onChanged={() => {}}
            onOpenOnboarding={() => {}}
            refreshVersion={0}
            onRefreshComplete={() => {}}
          />
        </div>
      </div>
    </>
  );
}

// 第一次繪製之前就要接上，否則 Settings 掛載當下那一輪呼叫會拿到
// api.ts 的固定 disabled 預覽值。
installFixture(() => "staleBridge");
ReactDOM.createRoot(document.getElementById("root")!).render(<Harness />);
