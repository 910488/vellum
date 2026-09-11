/**
 * 紀錄的預覽台（開發用，不進打包產物）。
 *
 * 為什麼需要它：這一頁的問題是**版面經濟** —— 一屏看得到幾列、要捲多久才
 * 摸得到明細。那是只有畫出來、而且要有幾百列真資料才量得到的東西；空資料
 * 的畫面永遠看起來很寬裕。
 *
 * 掛的是真元件、真 api 層、真 i18n。假的只有最底層的
 * window.__TAURI_INTERNALS__.invoke。
 *
 * 開發用：
 *   pnpm dev:renderer   然後開 /preview/log.html
 */
import { useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
import { getLocalePreference, setLocalePreference } from "@/i18n";
import { APP_LOCALES, type LocalePreference } from "@/i18n/locale";
import { Log } from "@/screens/Log";
import type { RequestLog, UsageActivity } from "@/types";

import "@/styles/tokens.css";
import "@/styles/base.css";
import "@/styles/components.css";

const SCENES = { busy: "有幾百列", quiet: "安靜的一天", empty: "全新安裝" } as const;
type Scene = keyof typeof SCENES;

const ROUTES = [
  ["official", "OpenAI Official", "gpt-5.6-luna"],
  ["cc", "DeepSeek", "deepseek-v4-flash"],
  ["qwen", "Qwen", "qwen3-coder-plus"],
  ["grok-cli", "Grok", "grok-5-fast"],
] as const satisfies readonly (readonly [string, string, string])[];

function requestLog(scene: Scene): RequestLog {
  if (scene === "empty") return { entries: [], providers: [], compactionEvents: [], subagentRuns: [] };
  const count = scene === "busy" ? 420 : 7;
  const now = Math.floor(Date.now() / 1000);
  const entries = Array.from({ length: count }, (_, index) => {
    const [routeId, provider, model] = ROUTES[index % ROUTES.length]!;
    const bad = index % 17 === 3;
    return {
      id: count - index,
      routeId,
      provider,
      model,
      inputTokens: 4_200 + ((index * 977) % 61_000),
      outputTokens: 180 + ((index * 131) % 3_400),
      cachedInputTokens: index % 3 === 0 ? 3_100 + ((index * 53) % 40_000) : 0,
      status: bad ? (index % 34 === 3 ? 429 : 502) : 200,
      error: bad ? "upstream returned 502 after 2 retries (connection reset by peer)" : null,
      durationMs: 900 + ((index * 613) % 46_000),
      firstByteMs: 210 + ((index * 37) % 1_800),
      createdAt: now - index * 137,
      connectionId: index % 4 === 0 ? `conn-${(index * 7919).toString(16)}` : null,
      // 真的會出現的四個值（見 log.requests.streamQuality）；自己編一個會讓預覽台
      // 看起來正常、實機卻少一段翻譯。
      streamQuality: index % 5 === 0 ? "incremental" : index % 11 === 0 ? "buffered" : null,
      requestIdHash: `sha256:${((index + 1) * 2654435761).toString(16).padStart(16, "0")}`,
      controlAccountHash: index % 6 === 0 ? `sha256:${"1f".repeat(32)}` : null,
      executionAccountHash: index % 6 === 0 ? `sha256:${"9c".repeat(32)}` : null,
    };
  });
  return {
    entries,
    providers: ROUTES.map(([routeId, provider], index) => ({
      routeId,
      provider,
      requests: 40 + index * 17,
      inputTokens: 900_000 - index * 130_000,
      outputTokens: 60_000 - index * 8_000,
      totalTokens: 960_000 - index * 138_000,
    })) as unknown as RequestLog["providers"],
    compactionEvents: Array.from({ length: scene === "busy" ? 46 : 2 }, (_, index) => ({
      id: index + 1,
      createdAt: now - index * 3_600,
      engine: index % 4 === 0 ? "codex_client" : "official_canonical",
      itemsBefore: 320 - index,
      itemsAfter: 41,
      thresholdPercent: 78,
      window: 272_000,
      activeTokens: 214_000 - index * 900,
      checkpointId: index % 4 === 0 ? null : `ckpt-${(index + 1).toString(36)}`,
      generation: 1 + (index % 3),
      fallbackReason: index % 9 === 4 ? "reasoning replay unavailable" : null,
    })) as unknown as RequestLog["compactionEvents"],
    subagentRuns: Array.from({ length: scene === "busy" ? 31 : 1 }, (_, index) => ({
      parentEntryId: count - index * 3,
      parentRequestHash: `sha256:${((index + 9) * 2654435761).toString(16)}`,
      routeId: ROUTES[index % ROUTES.length]![0],
      model: ROUTES[index % ROUTES.length]![2],
      state: index % 7 === 2 ? "failed" : index % 5 === 1 ? "cancelled" : "completed",
      requestedAt: now - index * 811,
      completedAt: now - index * 811 + 42,
      inputTokens: 12_000 + index * 311,
      outputTokens: 900 + index * 17,
      detail: index % 7 === 2 ? "child provider returned 502" : null,
    })) as unknown as RequestLog["subagentRuns"],
  };
}

function usageActivity(scene: Scene): UsageActivity {
  const days = Array.from({ length: 365 }, (_, index) => {
    const date = new Date(Date.now() - (364 - index) * 86_400_000);
    const iso = date.toISOString().slice(0, 10);
    const quiet = scene === "quiet" || index % 9 === 0;
    return {
      date: iso,
      tokens: scene === "empty" ? 0 : quiet ? (index * 31) % 9_000 : 40_000 + ((index * 7919) % 380_000),
      requests: scene === "empty" ? 0 : quiet ? 2 : 20 + (index % 60),
    };
  });
  const totalTokens = days.reduce((sum, day) => sum + day.tokens, 0);
  return {
    days,
    totalTokens,
    peakTokens: Math.max(...days.map((day) => day.tokens)),
    longestTaskDurationMs: 4_920_000,
    currentStreakDays: scene === "empty" ? 0 : 23,
    longestStreakDays: scene === "empty" ? 0 : 61,
    providers: scene === "empty" ? [] : ROUTES.map(([routeId, provider], index) => ({
      routeId,
      provider,
      tokens: Math.round(totalTokens / (index + 2)),
      accountCount: index === 0 ? 2 : 0,
      source: index === 0 ? "codex_profile" : "proxy",
    })) as UsageActivity["providers"],
    officialSource: "codex_profile",
    warning: null,
  };
}

function installFixture(sceneOf: () => Scene) {
  const invoke = async (cmd: string): Promise<unknown> => {
    const scene = sceneOf();
    switch (cmd) {
      case "get_request_log":
        return requestLog(scene);
      case "get_usage_activity":
        return usageActivity(scene);
      case "get_boot_telemetry":
        return { pid: 22228, bootCount: 41, previousStartedAt: Math.floor(Date.now() / 1000) - 7_200 };
      default:
        return null;
    }
  };
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = { invoke };
}

const THEMES = ["dark", "light"] as const;

function Harness() {
  const [scene, setScene] = useState<Scene>("busy");
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
      {/* 高度釘死的 .shell，讓 .canvas 真的是捲動容器 —— 這一頁要量的就是
          「一屏放得下多少」，少了這一層量到的是無限高的假畫面。 */}
      <div className="shell" style={{ gridTemplateColumns: "minmax(0, 1fr)", gridTemplateRows: "minmax(0, 1fr)" }}>
        <div className="canvas">
          <Log key={`${scene}-${nonce}`} refreshVersion={0} onRefreshComplete={() => {}} />
        </div>
      </div>
    </>
  );
}

installFixture(() => "busy");
ReactDOM.createRoot(document.getElementById("root")!).render(<Harness />);
