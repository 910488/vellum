/**
 * 初次設定的預覽台（開發用，不進打包產物）。
 *
 * 為什麼需要它：Onboarding 還沒接進 App.tsx，所以它在真實 app 裡
 * 一次都沒被畫出來過 —— 四個語言的字串全部翻好了，卻沒有人看得到。
 * 排版問題（欄寬、字距、換行、字型字形）只有畫出來才看得到，
 * 讀原始碼是找不到的。
 *
 * 這裡掛的是**真的元件**與**真的 i18n 資源**，不是仿造的樣板 ——
 * 仿造的樣板只會驗證仿造品。狀態用假資料，因為 Onboarding 本來就是
 * props 進、callback 出，不碰 api。
 *
 * 序幕自己已經有語言選單了（那是產品功能，不是預覽台的）。這裡這一組
 * 是為了在第一到第四幕也能切語言 —— 那幾幕上產品不該掛常駐的語言控制項。
 *
 * 開發用：
 *   pnpm dev:renderer   然後開 /onboarding-preview.html
 */
import { useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
import { getLocalePreference, i18n, setLocalePreference } from "@/i18n";
import { APP_LOCALES, type LocalePreference } from "@/i18n/locale";
import {
  Onboarding,
  type ConnectKind,
  type OnboardingConnection,
} from "@/screens/Onboarding";

import "@/styles/tokens.css";
import "@/styles/base.css";
import "@/styles/components.css";

/* 假資料刻意挑「最會撐版面」的那一組：已接上的帳號用長 email、
   有一家正在等授權（會長出代碼牌）、有一家不可用。
   挑好看的假資料等於沒有驗證。 */
const CONNECTIONS: Record<ConnectKind, OnboardingConnection> = {
  chatgpt: { accounts: ["developer@example.test"] },
  grok: {
    accounts: [],
    pending: { code: "HXKD-9F2Q", uri: "https://accounts.x.ai/device" },
  },
  custom: { accounts: [] },
};

const THEMES = ["dark", "light"] as const;

function Harness() {
  const [step, setStep] = useState(0);
  const [pref, setPref] = useState<LocalePreference>(getLocalePreference());
  const [theme, setTheme] = useState<(typeof THEMES)[number]>("dark");

  /* 序幕上的語言選單是產品自己的，改了之後這裡要跟著顯示正確的值，
     否則兩個控制項會各說各話。 */
  useEffect(() => {
    const sync = () => setPref(getLocalePreference());
    i18n.on("languageChanged", sync);
    return () => {
      i18n.off("languageChanged", sync);
    };
  }, []);

  return (
    <>
      <div className="sky" aria-hidden="true">
        <div className="bokeh bokeh--a" />
        <div className="bokeh bokeh--b" />
        <div className="bokeh bokeh--c" />
      </div>
      <div className="leak" aria-hidden="true" />
      <div className="grain" aria-hidden="true" />

      {/* 預覽台自己的控制列。position: fixed 疊在上面，不進入 .sheet
          的版面，才不會影響被測的東西。刻意做得像開發工具而不像產品 UI ——
          它不該被誤認成畫面的一部分。 */}
      <div className="devbar">
        <label>
          <span>locale</span>
          <select
            value={pref}
            onChange={(event) => {
              const next = event.target.value as LocalePreference;
              setPref(next);
              void setLocalePreference(next);
            }}
          >
            <option value="system">system</option>
            {APP_LOCALES.map((code) => (
              <option key={code} value={code}>
                {code}
              </option>
            ))}
          </select>
        </label>
        <label>
          <span>theme</span>
          <select
            value={theme}
            onChange={(event) => {
              const next = event.target.value as (typeof THEMES)[number];
              setTheme(next);
              document.documentElement.dataset.theme = next;
            }}
          >
            {THEMES.map((mode) => (
              <option key={mode} value={mode}>
                {mode}
              </option>
            ))}
          </select>
        </label>
        <label>
          <span>act</span>
          <select value={step} onChange={(event) => setStep(Number(event.target.value))}>
            {[0, 1, 2, 3, 4].map((n) => (
              <option key={n} value={n}>
                {n === 0 ? "overture" : n}
              </option>
            ))}
          </select>
        </label>
      </div>

      <Onboarding
        step={step}
        onStep={setStep}
        connections={CONNECTIONS}
        busy={null}
        error={null}
        proxyRunning={false}
        proxyBusy={false}
        onConnect={() => {}}
        onCancelConnect={() => {}}
        onAddCustom={() => {}}
        onStartProxy={() => {}}
        onFinish={() => setStep(0)}
      />
    </>
  );
}

const root = document.getElementById("root");
if (!root) throw new Error("#root not found");
document.documentElement.dataset.theme = "dark";
ReactDOM.createRoot(root).render(<Harness />);
