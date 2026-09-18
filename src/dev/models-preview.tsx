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
import type { QuotaPoolSettings, QuotaPoolStatus, QuotaSnapshot } from "@/types";

import "@/styles/tokens.css";
import "@/styles/base.css";
import "@/styles/components.css";

const ACCOUNT = "acct_01HZX9K3QW";

/* 額度池要看的是「好幾個帳號擺在一起」的樣子，所以池的場景另外給一組
   帳號：一個週額度很鬆、一個 5 小時見底但週還有、一個已經到閘門。
   單帳號的場景保留原樣 —— 它驗的是另一件事（視窗排序與讀數）。 */
const POOL_ACCOUNTS = [
  { accountId: "acct_pool_a", email: "joe84@gmail.com", weekly: 41, fiveHour: 62, floor: 30 },
  { accountId: "acct_pool_b", email: "joe.work@lumenworks.io", weekly: 78, fiveHour: 0, floor: 20 },
  { accountId: "acct_pool_c", email: "joe.demo@lumenworks.io", weekly: 24, fiveHour: 88, floor: 24 },
] as const;

const SCENES = {
  pool: "額度池（三個帳號）",
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
    case "pool":
    case "unavailable":
      return [];
  }
}

let poolState: QuotaPoolStatus = {
  enabled: true,
  strategy: "rank",
  members: POOL_ACCOUNTS.map((entry) => ({
    accountId: entry.accountId,
    inPool: true,
    paused: false,
    weeklyFloor: entry.floor,
    maintainFiveHourWindow: false,
  })),
  activeAccountId: POOL_ACCOUNTS[0]!.accountId,
};

function installPoolFixture() {
  api.getCodexOAuthStatus = () =>
    Promise.resolve({
      authenticated: true,
      defaultAccountId: POOL_ACCOUNTS[0]!.accountId,
      selectionRevision: 3,
      selectionVerified: true,
      selectedAt: Date.now(),
      accounts: POOL_ACCOUNTS.map((entry, index) => ({
        accountId: entry.accountId,
        email: entry.email,
        authenticatedAt: Date.now() - 86_400_000,
        isDefault: index === 0,
        planType: index === 0 ? "Plus" : "Business",
      })),
    });
  api.getCodexOAuthAccountQuota = (accountId: string) => {
    const entry = POOL_ACCOUNTS.find((candidate) => candidate.accountId === accountId);
    if (!entry) return Promise.reject(new Error("unknown account"));
    return Promise.resolve([
      window_("hour", 5, 100 - entry.fiveHour, 1.6),
      window_("week", null, 100 - entry.weekly, 56),
    ]);
  };
  api.getCodexOAuthResetCredits = (accountId: string) =>
    Promise.resolve({
      availableCount: accountId === POOL_ACCOUNTS[1]!.accountId ? 0 : 2,
      credits:
        accountId === POOL_ACCOUNTS[1]!.accountId
          ? []
          : [
              {
                id: "credit-1",
                resetType: "primary",
                status: "available" as const,
                expiresAt: hoursFromNow(30),
                title: "Reset credit",
                description: null,
              },
              {
                id: "credit-2",
                resetType: "primary",
                status: "available" as const,
                expiresAt: hoursFromNow(240),
                title: "Reset credit",
                description: null,
              },
            ],
    });
  api.getCodexQuotaPool = () => Promise.resolve(poolState);
  api.setCodexQuotaPool = (settings: QuotaPoolSettings) => {
    poolState = { ...settings, activeAccountId: poolState.activeAccountId };
    return Promise.resolve(poolState);
  };
}

function installFixture(scene: () => Scene) {
  api.getCodexQuotaPool = () =>
    Promise.resolve({ enabled: false, strategy: "rank", members: [], activeAccountId: null });
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
  const [scene, setScene] = useState<Scene>("pool");
  const [pref, setPref] = useState<LocalePreference>(getLocalePreference());
  const [theme, setTheme] = useState<(typeof THEMES)[number]>("light");
  const [nonce, setNonce] = useState(0);

  useEffect(() => {
    if (scene === "pool") installPoolFixture();
    else installFixture(() => scene);
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
