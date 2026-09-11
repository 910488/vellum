/**
 * Enhanced Core 的預覽台（開發用，不進打包產物）。
 *
 * 為什麼需要它：這一頁描述的是一條交接鏈 —— 我們的成品 → 我們的 bridge →
 * Codex Desktop 認養它 → 兩個子行程在正確的 digest 上初始化。鏈斷在哪一節，
 * 畫面就該指哪一節。但要在真機上重現「斷在第三節」得先把 Codex Desktop 弄成
 * 那個樣子，所以那些狀態的排版在出事以前沒有人看過。
 *
 * 掛的是真元件、真 api 層、真 i18n。假的只有最底層的
 * window.__TAURI_INTERNALS__.invoke（觀測檔）與當作 prop 餵進去的 status ——
 * 後者在真 app 裡本來就是 App.tsx 傳下來的。
 *
 * 開發用：
 *   pnpm dev:renderer   然後開 /preview/enhanced.html
 */
import { useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
import { getLocalePreference, setLocalePreference } from "@/i18n";
import { APP_LOCALES, type LocalePreference } from "@/i18n/locale";
import { EnhancedCore } from "@/screens/EnhancedCore";
import type { RuntimeObservations } from "@/lib/enhanced";
import type { EnhancedDesktopRuntimeStatus } from "@/types";

import "@/styles/tokens.css";
import "@/styles/base.css";
import "@/styles/components.css";

/** 交接鏈斷在不同節點時的樣子，加上兩個最會被誤讀的變體。 */
const SCENES = {
  live: "服務中",
  unverified: "未驗證的 Desktop",
  awaitingRestart: "等待 Desktop 重啟",
  drift: "核心被換掉（自動修復中）",
  blocked: "成品不可用",
  stale: "bridge 已死",
  disabled: "已停用",
} as const;
type Scene = keyof typeof SCENES;

const ENHANCED_EXE = "C:\\Users\\developer\\AppData\\Local\\Vellum\\enhanced-runtime\\vellum-enhanced-codex.exe";
const OFFICIAL_EXE = "C:\\Users\\developer\\AppData\\Local\\OpenAI\\Codex\\bin\\example-current\\codex.exe";
const SUPERSEDED_EXE = "C:\\Users\\developer\\AppData\\Local\\OpenAI\\Codex\\bin\\example-old\\codex.exe";
const BRIDGE_EXE = "C:\\Program Files\\Vellum\\vellum-app-server-bridge-8f3c21.exe";

function baseStatus(): EnhancedDesktopRuntimeStatus {
  return {
    configured: true,
    enabled: true,
    artifactReady: true,
    active: true,
    bridgeObserved: true,
    ready: true,
    activationState: "active",
    environmentState: "leased",
    environmentValue: BRIDGE_EXE,
    observedBridgeExecutable: BRIDGE_EXE,
    restartRequired: false,
    launchId: "01K4S9NQ7P3M2VYB8W6X",
    bridgeState: "ready",
    bridgePid: 22228,
    officialChildPid: 22412,
    enhancedChildPid: 22417,
    enhancedRuntimeDigest: "sha256:4c1f9a7e2b6d0538",
    activeRuntimeDigest: "sha256:4c1f9a7e2b6d0538",
    activeFeatureProfile: "qwen_tool_reliability,deepseek_context_recovery",
    officialCodexExecutable: OFFICIAL_EXE,
    enhancedCodexExecutable: ENHANCED_EXE,
    bridgeExecutable: BRIDGE_EXE,
    protocol: {
      verdict: "verified",
      pinnedSchemaSha256: "a".repeat(64),
      desktopSchemaSha256: "a".repeat(64),
      deltas: [],
    },
    serving: true,
    unverified: false,
    lastQualification: {
      runId: "qual-01K4S8",
      mode: "installed",
      startedAt: Math.floor(Date.now() / 1000) - 5400,
      finishedAt: Math.floor(Date.now() / 1000) - 5180,
      passed: true,
      promotionReady: true,
      reportPath: "C:\\Users\\developer\\AppData\\Local\\Vellum\\enhanced-runtime\\qualification\\example.json",
      failures: [],
    },
    coreAvailable: true,
    missingHelpers: [],
    launchCoreDrift: false,
    blockers: [],
  };
}

function statusFor(scene: Scene): EnhancedDesktopRuntimeStatus {
  const status = baseStatus();
  switch (scene) {
    case "live":
      return status;
    case "unverified":
      return {
        ...status,
        unverified: true,
        protocol: {
          verdict: "unverified",
          pinnedSchemaSha256: "a".repeat(64),
          desktopSchemaSha256: "b".repeat(64),
          deltas: [
            { kind: "fieldNewlyRequired", subject: "TurnStartedParams", fields: ["effort"], routed: false },
            { kind: "methodUnknownToDesktop", subject: "thread/setModel", fields: [], routed: false },
            { kind: "fieldRemoved", subject: "ThreadItem", fields: ["legacyTokenUsage", "legacyCacheHit"], routed: false },
          ],
        },
        lastQualification: null,
      };
    case "awaitingRestart":
      return {
        ...status,
        active: false,
        bridgeObserved: false,
        ready: false,
        serving: false,
        activationState: "awaitingDesktopRestart",
        restartRequired: true,
        bridgeState: null,
        bridgePid: null,
        officialChildPid: null,
        enhancedChildPid: null,
        activeRuntimeDigest: null,
        activeFeatureProfile: null,
        blockers: ["no bridge attestation for a Vellum-prepared launch"],
      };
    case "drift":
      return {
        ...status,
        restartRequired: true,
        launchCoreDrift: true,
        blockers: [
          `the live launch runs the Official core from ${SUPERSEDED_EXE}, not the configured ${OFFICIAL_EXE}`,
        ],
      };
    case "blocked":
      return {
        ...status,
        artifactReady: false,
        active: false,
        ready: false,
        serving: false,
        bridgeObserved: false,
        activationState: "artifactBlocked",
        environmentState: "released",
        environmentValue: null,
        observedBridgeExecutable: null,
        bridgeState: null,
        bridgePid: null,
        officialChildPid: null,
        enhancedChildPid: null,
        activeRuntimeDigest: null,
        activeFeatureProfile: null,
        protocol: {
          verdict: "incompatible",
          pinnedSchemaSha256: "a".repeat(64),
          desktopSchemaSha256: "c".repeat(64),
          deltas: [
            { kind: "methodUnservable", subject: "thread/resume", fields: ["cursor"], routed: true },
          ],
        },
        missingHelpers: ["codex-code-mode-host.exe", "codex-windows-sandbox-setup.exe"],
        blockers: [
          "the Enhanced core's protocol differs on a routed method (thread/resume)",
          "pinned artifact sha256 does not match the file on disk",
        ],
      };
    case "stale":
      return {
        ...status,
        active: false,
        ready: false,
        serving: false,
        activationState: "failed",
        bridgeState: "failed",
        environmentState: "orphanedBridge",
        blockers: ["the running bridge was not started by Codex Desktop (EnhancedDesktopBridgeNotObserved)"],
      };
    case "disabled":
      return {
        ...baseStatus(),
        enabled: false,
        active: false,
        ready: false,
        serving: false,
        bridgeObserved: false,
        activationState: "disabled",
        environmentState: "released",
        environmentValue: null,
        observedBridgeExecutable: null,
        bridgeState: null,
        bridgePid: null,
        officialChildPid: null,
        enhancedChildPid: null,
        activeRuntimeDigest: null,
        activeFeatureProfile: null,
      };
  }
}

function observationsFor(scene: Scene): RuntimeObservations {
  const now = Math.floor(Date.now() / 1000);
  const empty: RuntimeObservations = {
    schemaVersion: 1,
    launchId: "01K4S9NQ7P3M2VYB8W6X",
    bridgePid: 22228,
    updatedAt: now,
    freshness: "current",
    sessions: {},
    remote: {
      state: "unavailable",
      ownerPid: null,
      clients: {},
      handshakeObserved: false,
      listObserved: false,
      historyObserved: false,
      streamObserved: false,
      controlObserved: false,
      lastFailureStage: null,
      lastErrorCode: null,
    },
  };
  if (scene === "disabled" || scene === "awaitingRestart" || scene === "blocked") return { ...empty, freshness: "unavailable" };
  if (scene === "stale") return { ...empty, freshness: "stale", updatedAt: now - 900 };

  const rows: [string, string, string | null, string, number][] = [
    ["01K4SB2R7NQ0X3MJ8ZC5VYWD41", "enhanced-codex", "qwen3-coder-plus", "running", now - 4],
    ["01K4SB1P9MT8W2FH6RQ3JXNV07", "enhanced-codex", "deepseek-v4-flash", "approval", now - 61],
    ["01K4SAZ4KD6C1B9GT5NPXWQM32", "official-codex", "gpt-5.6-luna", "running", now - 118],
    ["01K4SAY8HV3J7L2QW9RTZBNK65", "enhanced-codex", "qwen3-coder-plus", "idle", now - 940],
    ["01K4SAX1FC5D8N4MP2VRTYJH90", "official-codex", "gpt-5.6-luna", "idle", now - 3120],
    ["01K4SAW6EB2A0K7SJ4XQNMLT18", "enhanced-codex", null, "attached", now - 21600],
    ["01K4SAV3DZ9Y6H1RN8WCFPGB74", "enhanced-codex", "deepseek-v4-flash", "unloaded", now - 86400],
  ];
  return {
    ...empty,
    sessions: Object.fromEntries(
      rows.map(([threadId, plane, model, state, lastActivity]) => [
        threadId,
        {
          threadId,
          parentThreadId: state === "approval" ? "01K4SB2R7NQ0X3MJ8ZC5VYWD41" : null,
          plane,
          model,
          state,
          lastActivity,
        },
      ]),
    ),
    remote:
      scene === "live" || scene === "unverified" || scene === "drift"
        ? {
            state: "transportReady",
            ownerPid: 22228,
            clients: { "iPhone 17 Pro": "connected", iPad: "idle" },
            handshakeObserved: true,
            listObserved: true,
            historyObserved: true,
            streamObserved: true,
            controlObserved: scene !== "drift",
            lastFailureStage: scene === "drift" ? "control" : null,
            lastErrorCode: scene === "drift" ? 4401 : null,
          }
        : empty.remote,
  };
}

function installFixture(sceneOf: () => Scene) {
  const invoke = async (cmd: string): Promise<unknown> => {
    const scene = sceneOf();
    switch (cmd) {
      case "list_enhanced_runtime_sessions":
        return observationsFor(scene);
      case "recheck_enhanced_runtime_compatibility":
      case "get_enhanced_desktop_runtime_status":
      case "get_enhanced_runtime_overview":
        return statusFor(scene);
      case "export_enhanced_runtime_diagnostics":
        return "C:\\Users\\developer\\Downloads\\vellum-enhanced-diagnostics-example.zip";
      default:
        return null;
    }
  };
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = { invoke };
}

const THEMES = ["dark", "light"] as const;

function Harness() {
  const [scene, setScene] = useState<Scene>("live");
  const [pref, setPref] = useState<LocalePreference>(getLocalePreference());
  const [theme, setTheme] = useState<(typeof THEMES)[number]>("light");
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
      {/* 高度釘死的 .shell 讓 .canvas 真的變成捲動容器 —— 見 remote-preview 的
          同一段註解。這一頁的重點之一就是「一屏能看到多少」，沒有這層量不到。 */}
      <div className="shell" style={{ gridTemplateColumns: "minmax(0, 1fr)", gridTemplateRows: "minmax(0, 1fr)" }}>
        <div className="canvas">
          <EnhancedCore
            key={`${scene}-${nonce}`}
            status={statusFor(scene)}
            refreshVersion={0}
            onRefreshComplete={() => {}}
          />
        </div>
      </div>
    </>
  );
}

installFixture(() => "live");
ReactDOM.createRoot(document.getElementById("root")!).render(<Harness />);
