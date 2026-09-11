/**
 * 模型頁帳號額度的預覽台（開發用，不進打包產物）。
 *
 * 為什麼需要它：ChatGPT 同時有 5 小時與每週兩條上限，而先擋住你的幾乎
 * 都是 5 小時那條。要看到「週還剩八成、5 小時已經見底」這一格，得先真的
 * 把 5 小時視窗用完 —— 用完之後也就沒得看了。
 *
 * 假的只有帳號、額度與 reset credits 三個回傳值；元件、i18n 與其餘的
 * api 瀏覽器 mock 都是真的。
 *
 * 開發用：
 *   pnpm dev:renderer   然後開 /preview/models.html
 */
import { useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
import { getLocalePreference, setLocalePreference } from "@/i18n";
import { APP_LOCALES, type LocalePreference } from "@/i18n/locale";
import { api } from "@/lib/api";
import { Models } from "@/screens/Models";
import type { QuotaSnapshot } from "@/types";

import "@/styles/tokens.css";
import "@/styles/base.css";
import "@/styles/components.css";

const ACCOUNT = "acct_01HZX9K3QW";

const SCENES = {
  fiveHourTight: "5 小時見底、週還很多",
  weeklyTight: "週見底、5 小時剛重置",
  bothHealthy: "兩條都還很鬆",
  weeklyOnly: "只回一條（舊帳號）",
  unavailable: "查不到額度",
} as const;
type Scene = keyof typeof SCENES;

function hoursFromNow(hours: number): string {
  return new Date(Date.now() + hours * 3_600_000).toISOString();
}

function window_(
  unit: QuotaSnapshot["period"]["unit"],
  amount: number | null,
  usedPercent: number,
  resetInHours: number,
): QuotaSnapshot {
  return {
    routeId: "openai-official",
    usedPercent,
    period: { unit, amount },
    resetAt: hoursFromNow(resetInHours),
    tier: null,
    stale: false,
  };
}

/** 後端回傳的順序刻意不照長度排 —— 排序是前端的事，這裡要驗的就是它。 */
function windowsFor(scene: Scene): QuotaSnapshot[] {
  switch (scene) {
    case "fiveHourTight":
      return [window_("week", null, 18, 96), window_("hour", 5, 93, 1.4)];
    case "weeklyTight":
      return [window_("week", null, 96, 52), window_("hour", 5, 4, 4.8)];
    case "bothHealthy":
      return [window_("week", null, 22, 88), window_("hour", 5, 31, 3.1)];
    case "weeklyOnly":
      return [window_("week", null, 44, 70)];
    case "unavailable":
      return [];
  }
}

function installFixture(scene: () => Scene) {
  api.getCodexOAuthStatus = () =>
    Promise.resolve({
      authenticated: true,
      defaultAccountId: ACCOUNT,
      selectionRevision: 3,
      selectionVerified: true,
      selectedAt: Date.now(),
      accounts: [
        {
          accountId: ACCOUNT,
          email: "developer@example.test",
          authenticatedAt: Date.now() - 86_400_000,
          isDefault: true,
        },
      ],
    });
  api.getCodexOAuthAccountQuota = () => {
    const windows = windowsFor(scene());
    return windows.length
      ? Promise.resolve(windows)
      : Promise.reject(new Error("rate_limit contains no usable windows"));
  };
  api.getCodexOAuthResetCredits = () =>
    Promise.resolve({
      availableCount: 1,
      credits: [
        {
          id: "credit-1",
          resetType: "primary",
          status: "available",
          expiresAt: hoursFromNow(240),
          title: "Reset credit",
          description: null,
        },
      ],
    });
}

const THEMES = ["light", "dark"] as const;

function Harness() {
  const [scene, setScene] = useState<Scene>("fiveHourTight");
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
          <select
            value={theme}
            onChange={(event) => setTheme(event.target.value as (typeof THEMES)[number])}
          >
            {THEMES.map((value) => <option key={value} value={value}>{value}</option>)}
          </select>
        </label>
      </div>
      <div
        className="shell"
        style={{ gridTemplateColumns: "minmax(0, 1fr)", gridTemplateRows: "minmax(0, 1fr)" }}
      >
        <div className="canvas">
          <Models
            key={`${scene}-${nonce}`}
            onChanged={() => {}}
            refreshVersion={0}
            onRefreshComplete={() => {}}
          />
        </div>
      </div>
    </>
  );
}

installFixture(() => "fiveHourTight");
ReactDOM.createRoot(document.getElementById("root")!).render(<Harness />);
