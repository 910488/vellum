/**
 * 簡化後「軟體更新」面板的預覽台（開發用，不進打包產物）。
 *
 * 新面板只有一個版本號在往前走；每個狀態都要真的發布一版、真的卡一段
 * 對話才看得到，所以這裡用場景直接擺出來。按鈕會推動場景（檢查 → 最新、
 * 重新啟動遇到對話 → 已排定、取消排定 → 已下載），點過一輪就能看完流程。
 *
 * 真的面板（UpdateCards）由 lib/vellumUpdate 把後端快照翻成這裡的場景，
 * 那段對照有 tests/updates-ui.test.tsx 守著；這裡只看畫面。
 *
 * 開發用：
 *   pnpm dev:renderer   然後開 /preview/updates.html
 */
import { useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
import { useTranslation } from "react-i18next";
import { getLocalePreference, setLocalePreference } from "@/i18n";
import { APP_LOCALES, type LocalePreference } from "@/i18n/locale";
import { Btn, Card } from "@/components/ui";
import {
  VellumUpdate,
  type VellumUpdateHosts,
  type VellumUpdateState,
} from "@/components/VellumUpdate";

import "@/styles/tokens.css";
import "@/styles/base.css";
import "@/styles/components.css";

const CURRENT = "0.2.9";
const NEXT = "0.2.10";
const NIGHTLY = "0.2.11-nightly.20261009.1";

const SCENES = {
  current: "最新",
  neverChecked: "最新（還沒檢查過）",
  checking: "檢查中",
  available: "有新版（自動更新關著）",
  downloading: "下載中",
  ready: "已下載，等重啟",
  scheduled: "已排定（對話還在跑）",
  failed: "失敗",
  unsigned: "這個組建不能自動更新（現況）",
} as const;
type Scene = keyof typeof SCENES;

const HOSTS = {
  none: { label: "沒有遠端主機", value: { total: 0, pending: 0, failed: 0 } },
  current: { label: "2 台都最新", value: { total: 2, pending: 0, failed: 0 } },
  pending: { label: "2 台、1 台待更新", value: { total: 2, pending: 1, failed: 0 } },
  failed: { label: "3 台、1 台失敗", value: { total: 3, pending: 1, failed: 1 } },
} satisfies Record<string, { label: string; value: VellumUpdateHosts }>;
type Hosts = keyof typeof HOSTS;

const THEMES = ["light", "dark"] as const;
const NOTES = [
  "・智慧分配改依帳號容量加權",
  "・ChatGPT 更新 CLI 後，bridge 會自動改用現行 core",
  "・「需重新啟動 Codex」只在重啟真的有用時才亮",
].join("\n");

function stateFor(scene: Scene, early: boolean): VellumUpdateState {
  const version = early ? NIGHTLY : NEXT;
  switch (scene) {
    case "current":
      return { kind: "current", checkedAt: new Date(Date.now() - 42 * 60_000).toISOString() };
    case "neverChecked":
      return { kind: "current", checkedAt: null };
    case "checking":
      return { kind: "checking" };
    case "available":
      return { kind: "available", version };
    case "downloading":
      return { kind: "downloading", version, percent: 62 };
    case "ready":
      return { kind: "ready", version };
    case "scheduled":
      return { kind: "scheduled", version, turns: 2, requests: 1 };
    case "failed":
      return { kind: "failed", version, reason: "desktopInstallDidNotComplete" };
    case "unsigned":
      return { kind: "unsigned" };
  }
}

function Harness() {
  const { t } = useTranslation();
  const [scene, setScene] = useState<Scene>("ready");
  const [hosts, setHosts] = useState<Hosts>("pending");
  const [pref, setPref] = useState<LocalePreference>(getLocalePreference());
  const [theme, setTheme] = useState<(typeof THEMES)[number]>("light");
  const [autoUpdate, setAutoUpdate] = useState(true);
  const [earlyAccess, setEarlyAccess] = useState(false);
  const [log, setLog] = useState<string | null>(null);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
  }, [theme]);

  // 按鈕推動場景，讓流程可以一路點過去，而不是只看靜態畫面。
  function go(next: Scene, note: string, after?: { scene: Scene; ms: number }) {
    setScene(next);
    setLog(note);
    if (after) window.setTimeout(() => setScene(after.scene), after.ms);
  }

  return (
    <>
      <div className="devbar">
        <label>
          <span>狀態</span>
          <select value={scene} onChange={(event) => setScene(event.target.value as Scene)}>
            {Object.entries(SCENES).map(([value, label]) => (
              <option key={value} value={value}>{label}</option>
            ))}
          </select>
        </label>
        <label>
          <span>主機</span>
          <select value={hosts} onChange={(event) => setHosts(event.target.value as Hosts)}>
            {Object.entries(HOSTS).map(([value, item]) => (
              <option key={value} value={value}>{item.label}</option>
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

      <div className="canvas" style={{ display: "grid", placeItems: "start center", padding: "48px 16px 96px" }}>
        <div style={{ width: "min(760px, 100%)", display: "grid", gap: 18 }}>
          <div className="update-panel__head">
            <div>
              <h2 className="dialog__title">{t("settings.page.updates.title")}</h2>
              <p className="note">{t("settings.page.updates.panelHint")}</p>
            </div>
            <Btn soft onClick={() => setLog("關閉面板")}>{t("common.close")}</Btn>
          </div>
          <Card>
              <VellumUpdate
                view={{
                  current: CURRENT,
                  state: stateFor(scene, earlyAccess),
                  autoUpdate,
                  earlyAccess,
                  hosts: HOSTS[hosts].value,
                  notes: NOTES,
                }}
                actions={{
                  onCheck: () =>
                    go("checking", "檢查更新", { scene: autoUpdate ? "downloading" : "available", ms: 1200 }),
                  onUpdate: () => go("downloading", "下載並更新：update_vellum"),
                  onCancel: () => go("current", "取消下載：留在目前版本"),
                  onRestart: () => go("scheduled", "重新啟動：有 2 段對話還在跑，改成排定"),
                  onUnschedule: () => go("ready", "取消排定：回到已下載"),
                  onRetry: () => go("downloading", "重試：已保留的安裝檔直接重用"),
                  onOpenDownloads: () => setLog("開啟 GitHub Releases 下載頁"),
                  onAutoUpdate: (next) => {
                    setAutoUpdate(next);
                    setLog(next ? "自動更新：開" : "自動更新：關（只在按「檢查更新」時才查）");
                  },
                  onEarlyAccess: (next) => {
                    setEarlyAccess(next);
                    setLog(next ? "搶先體驗版：開，目標改成 nightly" : "搶先體驗版：關");
                  },
                  onOpenHosts: () => setLog("跳到遠端管理頁"),
                }}
              />
          </Card>
          {log ? (
            <p className="note" style={{ fontFamily: "var(--font-mono)", opacity: 0.7 }}>
              ↳ {log}
            </p>
          ) : null}
        </div>
      </div>
    </>
  );
}

ReactDOM.createRoot(document.getElementById("root")!).render(<Harness />);
