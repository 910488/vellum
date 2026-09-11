import { useLocaleFormat } from "@/i18n/useLocaleFormat";
import { useTranslation } from "react-i18next";
/**
 * 模型 —— 換 Codex 用的模型，以及加供應商。
 *
 * 這頁是階段 1 的重點：「換模型」是這個 app 存在的理由，以前卻沒有家 ——
 * 啟用／停用在「供應商」頁、可熱切換的固定槽位在「執行狀態」頁、
 * 要不要重啟又在第三個地方。現在三件事在同一頁，由上而下就是決策順序。
 *
 * 狀態一律走 lib/vocabulary.ts 的三態。這頁曾經有十一種字串在講同一組
 * 布林（下次啟動載入／排除、型錄已加入・待重啟套用、設定已保存・待重新啟動…），
 * 而且「最近請求」跟「已啟用」用同一種 pill，讓歷史看起來像狀態。
 */
import { useEffect, useRef, useState } from "react";
import { api } from "@/lib/api";
import { providerNameFromEndpoint } from "@/lib/providerName";
import {
  accountQuotaWindows,
  quotaPeriodLabel,
  type AccountQuotaPresentation,
  type Translate,
} from "@/lib/quota";
import { codexCompatibleProbeModels } from "@/lib/onboarding";
import {
  isOpenCodeZenEndpoint,
  OPENCODE_ZEN_BASE_URL,
  OPENCODE_ZEN_NAME,
  providerPresetDraft,
} from "@/lib/providerPresets";
import {
  isSpendableReset,
  orderResetCredits,
  resetExpiryTone,
  soonestResetExpiry,
} from "@/lib/resetCredits";
import { expiresAtRef, expiresInRef } from "@/lib/format";
import { providerState } from "@/lib/vocabulary";
import { Btn, Cap, Card, Confirm, Empty, KV, Meter, Pill, Row, Rows, State, Toggle, Tray } from "@/components/ui";
import type {
  CodexOAuthDeviceLogin,
  CodexOAuthStatus,
  CodexResetCredit,
  CodexResetCredits,
  GrokAccountStatus,
  GrokLoginStatus,
  CatalogStatus,
  InsecureHttpPolicy,
  ModelCapability,
  ModelRoute,
  ProbeResult,
  QuotaSnapshot,
  Route,
  RouteReprobeReport,
} from "@/types";

const FIELD_LABEL: Record<string, string> = {
  contextWindow: "models.ui.probe.context",
  model: "common.model",
  wire: "models.ui.probe.wire",
};

function wireLabel(wire: ProbeResult["wire"], t: (key: string) => string): string {
  if (wire === "responses") return "Responses API";
  if (wire === "chat") return "Chat Completions";
  return t("models.ui.wireUnknown");
}

function probeIssueLabel(
  issue: string | null | undefined,
  t: (key: string, options?: Record<string, unknown>) => string,
  capability?: ModelCapability | null,
): string | null {
  if (!issue) return null;
  const latestAttempt = [...(capability?.probeAttempts ?? [])]
    .reverse()
    .find((attempt) => attempt.outcome !== "success");
  const withDetail = (label: string) =>
    latestAttempt?.message ? `${label}: ${latestAttempt.message}` : label;
  if (issue === "provider_opt_in_required" || issue === "provider_access_restricted") {
    return withDetail(t("models.ui.probe.verificationRequired"));
  }
  if (issue === "timeout") {
    return withDetail(t("models.ui.probe.timeout"));
  }
  if (issue === "provider_quota") {
    const lastAttempt = [...(capability?.probeAttempts ?? [])].reverse().find(
      (a) => a.outcome === "provider_quota" && a.retryAfter,
    );
    if (lastAttempt?.retryAfter) {
      return withDetail(t("models.ui.probe.quotaWithRetry", { seconds: lastAttempt.retryAfter }));
    }
    return withDetail("HTTP 429 · provider_quota");
  }
  if (issue === "provider_unauthorized") {
    return withDetail(t("models.ui.probe.unauthorized"));
  }
  if (issue === "provider_protocol") {
    return withDetail(t("models.ui.probe.protocolError"));
  }
  if (issue === "provider_unavailable") {
    return withDetail("provider_unavailable");
  }
  if (issue === "tool_call_missing") {
    return withDetail(t("models.ui.probe.toolCallMissing"));
  }
  return null;
}

type PendingConfirm =
  | { kind: "removeAccount"; accountId: string; label: string }
  | { kind: "logoutAll" }
  | { kind: "removeGrokAccount"; accountId: string; label: string }
  | { kind: "removeRoute"; route: Route }
  | { kind: "consumeReset"; accountId: string; label: string; creditId: string };

function reprobeErrorDetail(
  error: RouteReprobeReport["errors"][number],
  t: (key: string) => string,
): string {
  const facts = [
    error.stage,
    error.status ? `HTTP ${error.status}` : null,
    error.timeout ? t("models.ui.probe.timeout") : null,
    error.retryAfter ? `Retry-After ${error.retryAfter}s` : null,
  ].filter(Boolean);
  return `${error.model}${facts.length ? ` · ${facts.join(" · ")}` : ""}: ${error.message}`;
}

function effortProbeDiagnostic(
  capability: ModelCapability | null | undefined,
  t: (key: string, options?: Record<string, unknown>) => string,
): string | null {
  if (capability?.effortProbeStatus !== "indeterminate") return null;
  const latestEffortAttempt = [...(capability.probeAttempts ?? [])]
    .reverse()
    .find((attempt) => attempt.stage === "effort" && attempt.outcome !== "success");
  const providerDetail = latestEffortAttempt?.message?.trim();
  const issue = capability.effortProbeIssue;
  if (issue === "provider_ignores_unknown_effort") {
    return t("models.ui.modelCatalog.effortReasonIgnored");
  }
  if (issue === "provider_quota_or_access") {
    return t("models.ui.modelCatalog.effortReasonQuota", {
      detail: providerDetail ? ` · ${providerDetail}` : "",
    });
  }
  if (issue === "provider_error") {
    return t("models.ui.modelCatalog.effortReasonProvider", {
      detail: providerDetail ? ` · ${providerDetail}` : "",
    });
  }
  return t("models.ui.modelCatalog.effortReasonUnknown", {
    detail: providerDetail ? ` · ${providerDetail}` : "",
  });
}

function effortProbeCanRetry(capability: ModelCapability | null | undefined): boolean {
  if (capability?.effortProbeStatus === "not_probed") return true;
  return capability?.effortProbeStatus === "indeterminate" &&
    capability.effortProbeIssue !== "provider_ignores_unknown_effort";
}
/** 一個帳號的所有額度視窗。
 *
 * ChatGPT 同時有 5 小時與每週兩條上限，Grok 有週與月。只顯示其中一條的
 * 結果，是畫面說「還剩 88%」而請求照樣被擋下來 —— 見底的是另一條。
 *
 * 位置固定（視窗短的在上），份量跟著實情走：剩最少的那一條用大的數字，
 * 因為它才是現在擋著你的那一條。反過來讓位置跟著剩餘量跑的話，兩條交叉
 * 時整列會自己對調，每次看都要重讀一次。剩不到一成五再換成 coral，但話
 * 一直都寫在旁邊，不靠顏色單獨表意。
 */
function AccountQuota({
  windows,
  t,
  resetLabel,
}: {
  windows: AccountQuotaPresentation[];
  t: Translate;
  resetLabel: (iso: string | null) => string;
}) {
  const binding = windows.reduce<AccountQuotaPresentation | null>(
    (tightest, window) =>
      tightest && tightest.remaining <= window.remaining ? tightest : window,
    null,
  );
  return (
    <span className="acct__quota">
      {windows.map((window) => (
        <span
          className={`acct__quota-window${window === binding ? " acct__quota-window--lead" : ""}`}
          key={`${window.period.unit}-${window.period.amount ?? 0}`}
        >
          <span className="acct__quota-line">
            <strong>{window.remaining}%</strong>
            <Meter percent={window.remaining} tone={window.remaining <= 15 ? "coral" : "honey"} />
          </span>
          <small>
            {quotaPeriodLabel(window.period, t)}
            {window.resetAt ? ` · ${resetLabel(window.resetAt)}` : ""}
          </small>
        </span>
      ))}
    </span>
  );
}

/** 一個帳號手上每一張 Reset 券。
 *
 * 一張券一列，到期時間寫在名稱底下，右邊是它自己的「使用重置」——
 * 照 ChatGPT 設定頁的樣子。以前是一顆帳號層級的按鈕配一份清單，於是
 * 「按下去會用掉哪一張」得由畫面另外解釋（曾經真的標了一個「下一張會用
 * 掉」的標籤）。把按鈕放回它作用的那一列，這件事就不需要解釋了。
 *
 * 排序仍是最快到期的在最上面：券張張等價，先用快過期的沒有代價，而作廢
 * 是靜悄悄的，沒有任何一步會通知你。
 *
 * 已經用掉或過期的留在最下面而不是被濾掉，否則「3 張可用」跟看得到的行
 * 數兜不起來。它們沒有按鈕，改成一個狀態字。
 */
function ResetLedger({
  id,
  credits,
  t,
  labelledBy,
  busy,
  onUse,
}: {
  id: string;
  credits: CodexResetCredit[];
  t: Translate;
  labelledBy: string;
  busy: boolean;
  onUse: (creditId: string) => void;
}) {
  const now = Date.now();
  const ordered = orderResetCredits(credits);
  return (
    <span className="resets" id={id} role="group" aria-labelledby={labelledBy}>
      {/* 抬頭不再重印張數。上面那一列已經有「Reset 3」，同一個數字在同一個
          畫面上寫兩次，第二次不會被讀成確認，會被讀成另一個數字。 */}
      <span className="resets__head">
        <Cap>{t("models.ui.reset.ledgerTitle")}</Cap>
      </span>
      {ordered.length === 0 ? (
        <span className="resets__none">{t("models.ui.reset.noneUsable")}</span>
      ) : (
        ordered.map((credit) => {
          const tone = resetExpiryTone(credit, now);
          const spendable = isSpendableReset(credit);
          const expiresAt = expiresAtRef(credit.expiresAt);
          const expiresIn = expiresInRef(credit.expiresAt, now);
          const name = credit.title ?? credit.resetType ?? t("models.ui.reset.untitled");
          return (
            <span
              className="resets__row"
              key={credit.id}
              data-tone={tone}
              data-spendable={spendable}
              title={credit.description ?? undefined}
            >
              <span className="resets__what">
                <span className="resets__name">{name}</span>
                {/* 絕對時間回答「哪一天」，相對時間回答「來不來得及」。兩個都
                    要：只給「剩 4 天」排不進行事曆，只給日期得自己心算。

                    已經用掉或過期的只留日期。「還剩多久」對一張用不了的券沒有
                    意義，而它過期時算出來的字，剛好跟右邊的狀態字一模一樣。 */}
                <span className="resets__when">
                  {credit.expiresAt ? (
                    <>
                      <span className="resets__date">{t(expiresAt.key, expiresAt.values)}</span>
                      {spendable && expiresIn ? (
                        <span className="resets__left">{t(expiresIn.key, expiresIn.values)}</span>
                      ) : null}
                    </>
                  ) : (
                    <span className="resets__left">{t("models.ui.reset.noExpiry")}</span>
                  )}
                </span>
              </span>
              {spendable ? (
                <Btn soft mini disabled={busy} onClick={() => onUse(credit.id)}>
                  {t("models.ui.reset.use")}
                </Btn>
              ) : (
                <span className="resets__state">
                  {t(tone === "expired" ? "models.ui.reset.lapsed" : "models.ui.reset.spent")}
                </span>
              )}
            </span>
          );
        })
      )}
    </span>
  );
}

export function Models({
  onChanged,
  refreshVersion,
  onRefreshComplete,
}: {
  onChanged: () => void;
  refreshVersion: number;
  onRefreshComplete: (version: number) => void;
}) {
  const { t } = useTranslation();
  const { exact, resetLabel } = useLocaleFormat();
  const [routes, setRoutes] = useState<Route[]>([]);
  const [modelRoutes, setModelRoutes] = useState<ModelRoute[]>([]);
  const [catalogStatus, setCatalogStatus] = useState<CatalogStatus>({
    proxyRunning: false,
    injectedModelIds: [],
  });
  const [endpoint, setEndpoint] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [modelProbeBusy, setModelProbeBusy] = useState<string | null>(null);
  const [renamingRoute, setRenamingRoute] = useState<string | null>(null);
  const [routeNameDraft, setRouteNameDraft] = useState("");
  /** `${routeId} ${upstreamModel}` — model ids are not unique across routes. */
  const [renamingModel, setRenamingModel] = useState<string | null>(null);
  const [modelNameDraft, setModelNameDraft] = useState("");
  /* 正在編輯上下文長度的那一列（`${route.id}:${model}`），以及草稿字串。
     草稿留成字串而不是數字：空字串是有意義的一種輸入（＝自動），
     轉成數字就沒辦法跟 0 區分。 */
  const [editingWindowKey, setEditingWindowKey] = useState<string | null>(null);
  const [windowDraft, setWindowDraft] = useState("");
  /** Provider name for the add flow, seeded from the endpoint and editable. */
  const [draftProviderName, setDraftProviderName] = useState("");
  /** Per-model aliases chosen before the provider exists, keyed by upstream id. */
  const [draftModelNames, setDraftModelNames] = useState<Record<string, string>>({});
  const [probing, setProbing] = useState(false);
  const [result, setResult] = useState<ProbeResult | null>(null);
  const [selectedModels, setSelectedModels] = useState<string[]>([]);
  const [missing, setMissing] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);
  /* 破壞性動作（移除帳號、登出全部、移除供應商、消耗 Reset）一律先走這裡，
     不直接執行 —— 跟「改名」「停用」共用一顆 soft 按鈕會讓人手滑按錯。 */
  const [pendingConfirm, setPendingConfirm] = useState<PendingConfirm | null>(null);
  const [oauth, setOauth] = useState<CodexOAuthStatus | null>(null);
  const [deviceLogin, setDeviceLogin] = useState<CodexOAuthDeviceLogin | null>(null);
  const [oauthBusy, setOauthBusy] = useState(false);
  const [copyFeedback, setCopyFeedback] = useState<string | null>(null);
  const [resetCredits, setResetCredits] = useState<Record<string, CodexResetCredits>>({});
  const [resetErrors, setResetErrors] = useState<Record<string, string>>({});
  const [resetBusy, setResetBusy] = useState<string | null>(null);
  /** 拉開了哪一個帳號的 Reset 托盤。一次只開一個：兩個帳號各攤四張券，卡片
      會長到要捲動，這頁真正的主線（換模型、加供應商）就被推到看不見的地方。 */
  const [openResets, setOpenResets] = useState<string | null>(null);
  const [accountQuotas, setAccountQuotas] = useState<Record<string, QuotaSnapshot[]>>({});
  const [accountQuotaErrors, setAccountQuotaErrors] = useState<Record<string, string>>({});
  const [grokAccounts, setGrokAccounts] = useState<GrokAccountStatus | null>(null);
  const [grokLogin, setGrokLogin] = useState<GrokLoginStatus | null>(null);
  const [grokBusy, setGrokBusy] = useState<string | null>(null);
  const [grokQuotas, setGrokQuotas] = useState<Record<string, QuotaSnapshot[]>>({});
  const [grokQuotaErrors, setGrokQuotaErrors] = useState<Record<string, string>>({});
  const [opencodeApiKey, setOpencodeApiKey] = useState("");
  const [opencodeBusy, setOpencodeBusy] = useState(false);
  const [opencodePreset, setOpencodePreset] = useState<"opencodeZen" | "opencodeGo">("opencodeZen");
  const [resetFeedback, setResetFeedback] = useState<{
    tone: "ok" | "warn";
    text: string;
  } | null>(null);
  const [routeBusy, setRouteBusy] = useState<string | null>(null);
  /** Last Provider-level re-probe outcome per route, for the summary shown
   * next to the button once it finishes. Cleared when a new re-probe for
   * that route starts. */
  const [reprobeReports, setReprobeReports] = useState<Record<string, RouteReprobeReport>>({});
  const pollGeneration = useRef(0);
  const grokPollGeneration = useRef(0);
  const refreshParts = useRef<{ version: number; done: Set<string> }>({
    version: 0,
    done: new Set(),
  });
  const markRefreshPart = (part: string) => {
    if (refreshVersion <= 0 || refreshParts.current.version !== refreshVersion) return;
    refreshParts.current.done.add(part);
    if (["main", "grokQuota", "openaiQuota", "resetCredits"].every((key) => refreshParts.current.done.has(key))) {
      onRefreshComplete(refreshVersion);
    }
  };

  useEffect(() => {
    let alive = true;
    refreshParts.current = { version: refreshVersion, done: new Set() };
    void Promise.allSettled([
      api.listRoutes(),
      api.listModelRoutes(),
      api.getCatalogStatus(),
      api.getCodexOAuthStatus(),
      api.getGrokAccountStatus(),
    ]).then((results) => {
      if (!alive) return;
      const [nextRoutes, models, status, oauthStatus, grokStatus] = results;
      if (nextRoutes.status === "fulfilled") setRoutes(nextRoutes.value);
      if (models.status === "fulfilled") setModelRoutes(models.value);
      if (status.status === "fulfilled") setCatalogStatus(status.value);
      if (oauthStatus.status === "fulfilled") setOauth(oauthStatus.value);
      if (grokStatus.status === "fulfilled") setGrokAccounts(grokStatus.value);
      const failures = results
        .slice(0, 4)
        .filter((result): result is PromiseRejectedResult => result.status === "rejected");
      setError(
        failures.length
          ? t("models.ui.errors.partialRefresh", { detail: failures.map((failure) => String(failure.reason)).join(t("common.listSeparator")) })
          : null,
      );
      // Grok is optional. A missing CLI does not turn the entire model page
      // refresh into an application failure.
      markRefreshPart("main");
    });
    return () => {
      alive = false;
      pollGeneration.current += 1;
      grokPollGeneration.current += 1;
    };
  }, [refreshVersion]);

  const grokRoute = routes.find((route) => route.providerKind === "grokCli");
  const opencodePresetBaseUrl = providerPresetDraft(opencodePreset).baseUrl;
  const opencodeRoute = routes.find(
    (route) => route.baseUrl.toLowerCase() === opencodePresetBaseUrl.toLowerCase(),
  );

  useEffect(() => {
    let alive = true;
    const forceQuotaRefresh =
      refreshVersion > 0 &&
      refreshParts.current.version === refreshVersion &&
      !refreshParts.current.done.has("grokQuota");
    const accounts = grokAccounts?.accounts ?? [];
    if (!grokRoute || !accounts.length) {
      setGrokQuotas({});
      setGrokQuotaErrors({});
      markRefreshPart("grokQuota");
      return () => {
        alive = false;
      };
    }
    void Promise.all(
      accounts.map(async (account) => {
        try {
          return {
            accountId: account.accountId,
            windows: await api.getGrokAccountQuota(
              account.accountId,
              grokRoute.id,
              forceQuotaRefresh,
            ),
            error: null,
          };
        } catch (cause) {
          return {
            accountId: account.accountId,
            windows: null,
            error: String(cause),
          };
        }
      }),
    ).then((results) => {
      if (!alive) return;
      const quotas: Record<string, QuotaSnapshot[]> = {};
      const errors: Record<string, string> = {};
      for (const result of results) {
        if (result.windows) quotas[result.accountId] = result.windows;
        if (result.error) errors[result.accountId] = result.error;
      }
      setGrokQuotas(quotas);
      setGrokQuotaErrors(errors);
      markRefreshPart("grokQuota");
    });
    return () => {
      alive = false;
    };
  }, [grokAccounts, grokRoute?.id, refreshVersion]);

  useEffect(() => {
    let alive = true;
    const accounts = oauth?.accounts ?? [];
    if (!accounts.length) {
      setResetCredits({});
      setResetErrors({});
      markRefreshPart("resetCredits");
      return () => {
        alive = false;
      };
    }
    void Promise.all(
      accounts.map(async (account) => {
        try {
          return {
            accountId: account.accountId,
            credits: await api.getCodexOAuthResetCredits(account.accountId),
            error: null,
          };
        } catch (cause) {
          return {
            accountId: account.accountId,
            credits: null,
            error: String(cause),
          };
        }
      }),
    ).then((results) => {
      if (!alive) return;
      const nextCredits: Record<string, CodexResetCredits> = {};
      const nextErrors: Record<string, string> = {};
      for (const result of results) {
        if (result.credits) nextCredits[result.accountId] = result.credits;
        if (result.error) nextErrors[result.accountId] = result.error;
      }
      setResetCredits(nextCredits);
      setResetErrors(nextErrors);
      markRefreshPart("resetCredits");
    });
    return () => {
      alive = false;
    };
  }, [oauth, refreshVersion]);

  useEffect(() => {
    let alive = true;
    const forceQuotaRefresh =
      refreshVersion > 0 &&
      refreshParts.current.version === refreshVersion &&
      !refreshParts.current.done.has("openaiQuota");
    const accounts = oauth?.accounts ?? [];
    if (!accounts.length) {
      setAccountQuotas({});
      setAccountQuotaErrors({});
      markRefreshPart("openaiQuota");
      return () => {
        alive = false;
      };
    }
    void Promise.all(
      accounts.map(async (account) => {
        try {
          return {
            accountId: account.accountId,
            windows: await api.getCodexOAuthAccountQuota(
              account.accountId,
              forceQuotaRefresh,
            ),
            error: null,
          };
        } catch (cause) {
          return {
            accountId: account.accountId,
            windows: null,
            error: String(cause),
          };
        }
      }),
    ).then((results) => {
      if (!alive) return;
      const nextQuotas: Record<string, QuotaSnapshot[]> = {};
      const nextErrors: Record<string, string> = {};
      for (const result of results) {
        if (result.windows) nextQuotas[result.accountId] = result.windows;
        if (result.error) nextErrors[result.accountId] = result.error;
      }
      setAccountQuotas(nextQuotas);
      setAccountQuotaErrors(nextErrors);
      if (accounts.some((account) => !account.email)) {
        void api.getCodexOAuthStatus().then((refreshed) => {
          if (!alive) return;
          const gainedEmail = refreshed.accounts.some(
            (next) =>
              Boolean(next.email) &&
              accounts.some((previous) => previous.accountId === next.accountId && !previous.email),
          );
          if (gainedEmail) setOauth(refreshed);
        });
      }
      markRefreshPart("openaiQuota");
    });
    return () => {
      alive = false;
    };
  }, [oauth, refreshVersion]);

  async function refreshResetCredits(accountId: string) {
    try {
      const credits = await api.getCodexOAuthResetCredits(accountId);
      setResetCredits((current) => ({ ...current, [accountId]: credits }));
      setResetErrors((current) => {
        const next = { ...current };
        delete next[accountId];
        return next;
      });
      return credits;
    } catch (cause) {
      setResetErrors((current) => ({ ...current, [accountId]: String(cause) }));
      throw cause;
    }
  }

  /** 用掉使用者按的那一張。清單上每一列有自己的按鈕，所以這裡不必替他挑；
      挑選規則一旦回到程式手上，畫面就得另外解釋按下去會用掉哪一張。 */
  function confirmConsumeReset(accountId: string, accountLabel: string, creditId: string) {
    const credits = resetCredits[accountId];
    const credit = credits?.credits.find(
      (candidate) => candidate.id === creditId && isSpendableReset(candidate),
    );
    if (!credit) {
      setResetFeedback({ tone: "warn", text: t("models.ui.errors.noReset") });
      return;
    }
    setPendingConfirm({ kind: "consumeReset", accountId, label: accountLabel, creditId: credit.id });
  }

  async function consumeReset(accountId: string, creditId: string) {
    setResetBusy(accountId);
    setResetFeedback(null);
    try {
      const result = await api.consumeCodexOAuthReset(accountId, creditId);
      await refreshResetCredits(accountId);
      setResetFeedback({
        tone: "ok",
        text:
          result.code === "reset"
            ? t("models.ui.errors.resetSuccess")
            : t("models.ui.errors.resetCompleted", { code: result.code }),
      });
      onChanged();
    } catch (cause) {
      setResetFeedback({ tone: "warn", text: t("models.ui.errors.resetFailed", { detail: String(cause) }) });
    } finally {
      setResetBusy(null);
    }
  }

  async function retryResetQuery(accountId: string) {
    setResetBusy(accountId);
    setResetFeedback(null);
    try {
      await refreshResetCredits(accountId);
    } catch (cause) {
      setResetFeedback({ tone: "warn", text: t("models.ui.errors.resetLookupFailed", { detail: String(cause) }) });
    } finally {
      setResetBusy(null);
    }
  }

  async function startCodexLogin() {
    setOauthBusy(true);
    setError(null);
    const generation = ++pollGeneration.current;
    try {
      const login = await api.startCodexOAuthLogin();
      setDeviceLogin(login);
      const deadline = Date.now() + login.expiresIn * 1000;
      while (generation === pollGeneration.current && Date.now() < deadline) {
        await new Promise((resolve) => window.setTimeout(resolve, login.interval * 1000));
        if (generation !== pollGeneration.current) return;
        const account = await api.pollCodexOAuthLogin(login.deviceCode);
        if (account) {
          setOauth(await api.getCodexOAuthStatus());
          setDeviceLogin(null);
          onChanged();
          return;
        }
      }
      if (generation === pollGeneration.current) {
        setError(t("models.ui.errors.oauthExpired"));
        setDeviceLogin(null);
      }
    } catch (cause) {
      if (generation === pollGeneration.current) {
        setError(t("models.ui.errors.oauthLoginFailed", { detail: String(cause) }));
        setDeviceLogin(null);
      }
    } finally {
      if (generation === pollGeneration.current) setOauthBusy(false);
    }
  }

  async function setDefaultAccount(accountId: string) {
    const previousRevision = oauth?.selectionRevision ?? 0;
    setOauthBusy(true);
    setError(null);
    try {
      const next = await api.setDefaultCodexOAuthAccount(accountId);
      if (
        next.defaultAccountId !== accountId ||
        (oauth?.defaultAccountId !== accountId && (next.selectionRevision ?? 0) <= previousRevision) ||
        next.selectionVerified !== true
      ) {
        throw new Error("Official account selection was not verified by the backend");
      }
      setOauth(next);
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.oauthSwitchFailed", { detail: String(cause) }));
    } finally {
      setOauthBusy(false);
    }
  }

  async function removeAccount(accountId: string) {
    setError(null);
    try {
      setOauth(await api.removeCodexOAuthAccount(accountId));
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.oauthRemoveFailed", { detail: String(cause) }));
    }
  }

  async function refreshAccount() {
    setOauthBusy(true);
    setError(null);
    try {
      setOauth(await api.refreshCodexOAuth());
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.oauthRefreshFailed", { detail: String(cause) }));
    } finally {
      setOauthBusy(false);
    }
  }

  async function logoutAccounts() {
    pollGeneration.current += 1;
    setOauthBusy(true);
    setError(null);
    try {
      setOauth(await api.logoutCodexOAuth());
      setDeviceLogin(null);
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.oauthLogoutFailed", { detail: String(cause) }));
    } finally {
      setOauthBusy(false);
    }
  }

  async function probe() {
    const value = endpoint.trim();
    if (!value) return;
    setProbing(true);
    setResult(null);
    setSelectedModels([]);
    setDraftModelNames({});
    setError(null);
    try {
      const detected = await api.discoverEndpointModels(value, apiKey.trim() || undefined);
      setResult(detected);
      // Seed the guess from the hostname, but let it be corrected before the
      // provider is created. Service prefixes (api/www/gateway) are skipped
      // so api.provider.example prefills as "provider", not "example".
      setDraftProviderName(providerNameFromEndpoint(value, t("models.ui.customProvider")));
      // Discovery only lists the catalog. Capability inference starts after
      // the user chooses a model, avoiding paid requests and cold starts for
      // every model the Provider happens to advertise.
      setSelectedModels([]);
    } catch (cause) {
      setError(t("models.ui.errors.probeFailed", { detail: String(cause) }));
    } finally {
      setProbing(false);
    }
  }

  async function verifyPendingModel(model: string) {
    if (!result) return;
    setModelProbeBusy(model);
    setError(null);
    try {
      const capability = await api.probeEndpointModel(
        endpoint.trim(),
        model,
        apiKey.trim() || undefined,
      );
      setResult((current) => current ? {
        ...current,
        wire: current.wire ?? capability.wire,
        streaming: capability.streaming ?? current.streaming,
        reasoning: capability.reasoning ?? current.reasoning,
        contextWindow: capability.contextWindow ?? current.contextWindow,
        modelCapabilities: current.modelCapabilities.map((item) =>
          item.model.toLowerCase() === model.toLowerCase() ? capability : item
        ),
      } : current);
      if (capability.toolCalling === true && capability.wire !== null) {
        setSelectedModels((current) => current.includes(model) ? current : [...current, model]);
      } else {
        setError(
          probeIssueLabel(capability.probeIssue, t)
            ?? t("models.ui.errors.modelToolProbeFailed", { model }),
        );
      }
    } catch (cause) {
      setError(t("models.ui.errors.modelProbeFailed", { model, detail: String(cause) }));
    } finally {
      setModelProbeBusy(null);
    }
  }

  async function toggleEnabled(route: Route) {
    setError(null);
    setRouteBusy(route.id);
    try {
      setRoutes(await api.setRouteEnabled(route.id, !route.enabled));
      setModelRoutes(await api.listModelRoutes());
      setCatalogStatus(await api.getCatalogStatus());
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.routeRefreshFailed", { detail: String(cause) }));
    } finally {
      setRouteBusy(null);
    }
  }

  /** Toggles between `deny` (default) and `allowPrivateNetwork` — the
   * `allowPublicWithoutCredentials` policy is deliberately not exposed here,
   * since it applies to a rarer, more consequential case (crediential-free
   * public HTTP) that deserves its own explicit UI, not a side effect of
   * this checkbox. */
  async function toggleAllowPrivateNetworkHttp(route: Route) {
    setError(null);
    setRouteBusy(route.id);
    const next: InsecureHttpPolicy =
      route.insecureHttpPolicy === "allowPrivateNetwork" ? "deny" : "allowPrivateNetwork";
    try {
      setRoutes(await api.setRouteInsecureHttpPolicy(route.id, next));
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.routeRefreshFailed", { detail: String(cause) }));
    } finally {
      setRouteBusy(null);
    }
  }

  async function reprobeRoute(route: Route) {
    setError(null);
    setRouteBusy(route.id);
    setReprobeReports((current) => {
      const { [route.id]: _dropped, ...rest } = current;
      return rest;
    });
    try {
      const routeReport = await api.reprobeRouteCapabilities(route.id);
      setReprobeReports((current) => ({ ...current, [route.id]: routeReport }));
      const [nextRoutes, nextModels, nextCatalog] = await Promise.all([
        api.listRoutes(),
        api.listModelRoutes(),
        api.getCatalogStatus(),
      ]);
      setRoutes(nextRoutes);
      setModelRoutes(nextModels);
      setCatalogStatus(nextCatalog);
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.reprobeFailed", { detail: String(cause) }));
    } finally {
      setRouteBusy(null);
    }
  }

  async function reprobeRouteModel(route: Route, model: string) {
    setError(null);
    setModelProbeBusy(`${route.id}:${model}`);
    try {
      await api.reprobeRouteModelCapability(route.id, model);
      const [nextRoutes, nextModels, nextCatalog] = await Promise.all([
        api.listRoutes(),
        api.listModelRoutes(),
        api.getCatalogStatus(),
      ]);
      setRoutes(nextRoutes);
      setModelRoutes(nextModels);
      setCatalogStatus(nextCatalog);
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.modelProbeFailed", { model, detail: String(cause) }));
    } finally {
      setModelProbeBusy(null);
    }
  }

  async function refreshGrokModels(route: Route) {
    setError(null);
    setGrokBusy("models");
    try {
      await api.refreshGrokModelCatalog(route.id);
      const [nextRoutes, nextModels, nextCatalog] = await Promise.all([
        api.listRoutes(),
        api.listModelRoutes(),
        api.getCatalogStatus(),
      ]);
      setRoutes(nextRoutes);
      setModelRoutes(nextModels);
      setCatalogStatus(nextCatalog);
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.grokRefreshFailed", { detail: String(cause) }));
    } finally {
      setGrokBusy(null);
    }
  }

  async function removeRoute(route: Route) {
    setError(null);
    setRouteBusy(route.id);
    try {
      setRoutes(await api.deleteRoute(route.id));
      setModelRoutes(await api.listModelRoutes());
      setCatalogStatus(await api.getCatalogStatus());
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.routeRemoveFailed", { detail: String(cause) }));
    } finally {
      setRouteBusy(null);
    }
  }

  async function toggleRouteModel(route: Route, model: string) {
    const current = route.selectedModels ?? route.models;
    const selected = current.includes(model)
      ? current.filter((candidate) => candidate !== model)
      : [...current, model];
    if (!selected.length) {
      setError(t("models.ui.errors.providerModelRequired"));
      return;
    }
    setRouteBusy(route.id);
    setError(null);
    try {
      setRoutes(await api.setRouteModels(route.id, selected));
      setModelRoutes(await api.listModelRoutes());
      setCatalogStatus(await api.getCatalogStatus());
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.catalogRefreshFailed", { detail: String(cause) }));
    } finally {
      setRouteBusy(null);
    }
  }

  async function saveRouteName(route: Route) {
    const name = routeNameDraft.trim();
    if (!name || name === route.name) {
      setRenamingRoute(null);
      return;
    }
    setRouteBusy(route.id);
    setError(null);
    try {
      setRoutes(await api.setRouteName(route.id, name));
      setModelRoutes(await api.listModelRoutes());
      setCatalogStatus(await api.getCatalogStatus());
      setRenamingRoute(null);
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.catalogRefreshFailed", { detail: String(cause) }));
    } finally {
      setRouteBusy(null);
    }
  }

  async function saveModelName(route: Route, model: string) {
    setRouteBusy(route.id);
    setError(null);
    try {
      setRoutes(await api.setModelDisplayName(route.id, model, modelNameDraft.trim()));
      setModelRoutes(await api.listModelRoutes());
      setCatalogStatus(await api.getCatalogStatus());
      setRenamingModel(null);
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.catalogRefreshFailed", { detail: String(cause) }));
    } finally {
      setRouteBusy(null);
    }
  }

  /**
   * 存下這個模型的最大上下文長度。
   *
   * 空字串＝交回自動判定，不是 0 —— 所以 null 與 0 一定要分開處理。
   * 這個值是模型的屬性（視窗多大），不是某個對話的狀態，所以它住在這一頁；
   * 「上下文」頁只負責讀，不再有設定區。
   */
  async function saveModelWindow(route: Route, catalogId: string) {
    const raw = windowDraft.trim();
    const parsed = raw === "" ? null : Number(raw);
    if (parsed !== null && (!Number.isFinite(parsed) || parsed <= 0)) {
      setError(t("models.ui.modelCatalog.windowInvalid"));
      return;
    }
    setRouteBusy(route.id);
    setError(null);
    try {
      await api.setBudgetOverride(catalogId, parsed);
      setModelRoutes(await api.listModelRoutes());
      setEditingWindowKey(null);
      onChanged();
    } catch (cause) {
      setError(t("models.ui.modelCatalog.windowSaveFailed", { detail: String(cause) }));
    } finally {
      setRouteBusy(null);
    }
  }

  async function toggleModelVision(route: Route, model: string, vision: boolean) {
    setRouteBusy(route.id);
    setError(null);
    try {
      setRoutes(await api.setModelVision(route.id, model, vision));
      setModelRoutes(await api.listModelRoutes());
      setCatalogStatus(await api.getCatalogStatus());
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.catalogRefreshFailed", { detail: String(cause) }));
    } finally {
      setRouteBusy(null);
    }
  }

  async function createRoute() {
    if (!result) return;
    const preferredModel =
      (missing.model && selectedModels.includes(missing.model) ? missing.model : "") ||
      selectedModels[0] ||
      "";
    const preferredCapability = result.modelCapabilities.find(
      (capability) => capability.model.toLowerCase() === preferredModel.toLowerCase(),
    );
    const wire =
      preferredCapability?.wire ??
      result.wire ??
      (missing.wire === "responses" || missing.wire === "chat" ? missing.wire : null);
    if (!wire) {
      setError(t("models.ui.errors.wireRequired"));
      return;
    }
    const model = preferredModel;
    if (!model) {
      setError(t("models.ui.errors.modelRequired"));
      return;
    }

    setError(null);
    try {
      const contextWindow = Number(missing.contextWindow);
      const detectedContextWindow =
        result.contextWindow ??
        (Number.isFinite(contextWindow) && contextWindow > 0 ? contextWindow : null);
      // Aliases chosen before the provider existed ride along on the capability
      // entries, so the very first catalog Codex sees already reads correctly.
      const capabilities = result.modelCapabilities.map((capability) => {
        const alias = draftModelNames[capability.model]?.trim();
        return alias ? { ...capability, displayName: alias } : capability;
      });
      const next = await api.createRoute(
        draftProviderName.trim() ||
          providerNameFromEndpoint(endpoint.trim(), t("models.ui.customProvider")),
        endpoint.trim(),
        model,
        wire,
        result.streaming,
        result.reasoning,
        result.serverSideResume,
        apiKey.trim() || undefined,
        endpoint.toLowerCase().includes("grok") ? "grokCli" : "openAiCompatible",
        result.models,
        selectedModels,
        detectedContextWindow,
        capabilities,
      );
      setRoutes(next);
      setModelRoutes(await api.listModelRoutes());
      setCatalogStatus(await api.getCatalogStatus());
      setEndpoint("");
      setApiKey("");
      setResult(null);
      setSelectedModels([]);
      setMissing({});
      setDraftProviderName("");
      setDraftModelNames({});
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.routeAddFailed", { detail: String(cause) }));
    }
  }

  async function startGrokLogin() {
    setGrokBusy("login");
    setError(null);
    const generation = ++grokPollGeneration.current;
    try {
      let status = await api.startGrokAccountLogin();
      setGrokLogin(status);
      while (generation === grokPollGeneration.current && status.state === "waiting") {
        await new Promise((resolve) => window.setTimeout(resolve, 1500));
        if (generation !== grokPollGeneration.current) return;
        status = await api.pollGrokAccountLogin(status.loginId);
        setGrokLogin(status);
      }
      if (status.state === "complete") {
        setGrokAccounts(await api.getGrokAccountStatus());
        setGrokLogin(null);
        onChanged();
      } else if (status.state === "failed") {
        setError(t("models.ui.errors.grokLoginFailed", { detail: status.error ?? t("models.ui.grok.loginIncomplete") }));
      }
    } catch (cause) {
      setError(t("models.ui.errors.grokStartFailed", { detail: String(cause) }));
    } finally {
      if (generation === grokPollGeneration.current) setGrokBusy(null);
    }
  }

  async function cancelGrokLogin() {
    const loginId = grokLogin?.loginId;
    if (!loginId) return;
    grokPollGeneration.current += 1;
    setGrokBusy("login");
    try {
      await api.cancelGrokAccountLogin(loginId);
      setGrokLogin(null);
    } catch (cause) {
      setError(t("models.ui.errors.grokCancelFailed", { detail: String(cause) }));
    } finally {
      setGrokBusy(null);
    }
  }

  async function setDefaultGrokAccount(accountId: string) {
    setGrokBusy(accountId);
    setError(null);
    try {
      setGrokAccounts(await api.setDefaultGrokAccount(accountId));
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.grokSwitchFailed", { detail: String(cause) }));
    } finally {
      setGrokBusy(null);
    }
  }

  async function refreshGrokAccount(accountId: string) {
    setGrokBusy(accountId);
    setError(null);
    try {
      setGrokAccounts(await api.refreshGrokAccount(accountId));
      const windows = await api.getGrokAccountQuota(accountId, grokRoute?.id, true);
      setGrokQuotas((current) => ({ ...current, [accountId]: windows }));
      setGrokQuotaErrors((current) => {
        const next = { ...current };
        delete next[accountId];
        return next;
      });
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.grokRefreshFailed", { detail: String(cause) }));
    } finally {
      setGrokBusy(null);
    }
  }

  async function removeGrokAccount(accountId: string) {
    setGrokBusy(accountId);
    setError(null);
    try {
      setGrokAccounts(await api.removeGrokAccount(accountId));
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.grokRemoveFailed", { detail: String(cause) }));
    } finally {
      setGrokBusy(null);
    }
  }

  async function connectOpenCodeCatalog(
    name: string,
    baseUrl: string,
    key: string,
    filter?: (capability: ModelCapability) => boolean,
    catalogScope: import("@/types").CatalogScope = "all",
  ) {
    const probeResult = await api.probeEndpoint(baseUrl, key || undefined);
    const openCode = isOpenCodeZenEndpoint(baseUrl);
    const compatible = (openCode
      ? probeResult.modelCapabilities.filter(
          (capability) => capability.wire !== null && capability.toolCalling !== false,
        )
      : codexCompatibleProbeModels(probeResult)
    ).filter((capability) => !filter || filter(capability));
    const [primary] = compatible;
    // `primary` must exist and carry a wire: falling back to
    // `probeResult.models[0]` would pick whatever is first in the raw
    // catalog regardless of whether it passed verification, silently
    // creating a route pointed at an unverified (possibly
    // protocol-unsupported) model.
    if (!primary || !primary.wire) {
      throw new Error(t("onboarding.connect.codexToolProtocolUnavailable"));
    }
    const selected = compatible.map((capability) => capability.model);
    // When a filter narrows this connection to a subset (the free tier),
    // that subset must also be the route's entire model catalog, not just
    // what's pre-selected — otherwise every paid model still shows up
    // unchecked in the model list, which is exactly the clutter the filter
    // exists to avoid.
    const capabilities = filter
      ? probeResult.modelCapabilities.filter((capability) => filter(capability))
      : probeResult.modelCapabilities;
    const models = filter
      ? probeResult.models.filter((model) =>
          capabilities.some((capability) => capability.model === model),
        )
      : probeResult.models;
    setRoutes(
      await api.createRoute(
        name,
        baseUrl,
        primary.model,
        primary.wire,
        probeResult.streaming,
        probeResult.reasoning,
        probeResult.serverSideResume,
        key,
        "openAiCompatible",
        models,
        selected,
        probeResult.contextWindow,
        capabilities,
        catalogScope,
      ),
    );
  }

  // "OpenCode Zen" here always means the free tier: matches what OpenCode's
  // own app shows by default (no special config needed, and it never shows
  // paid Zen models at all). Paid Zen access requires purchased credits this
  // account may not have, so the quick-connect card only ever offers what is
  // guaranteed to work regardless of plan. A user who genuinely holds paid
  // Zen credits and wants those models can still add them by hand through
  // the generic "Add Provider" flow below, which shows the full catalog.
  async function connectOpenCodeFreeModels(key: string, name: string = OPENCODE_ZEN_NAME) {
    await connectOpenCodeCatalog(
      name,
      OPENCODE_ZEN_BASE_URL,
      key,
      (capability) => capability.free === true && capability.deprecated !== true,
      "freeOnly",
    );
  }

  async function connectOpenCodeZen() {
    const key = opencodeApiKey.trim();
    if (opencodePreset === "opencodeGo" && !key) {
      setError(t("models.ui.errors.opencodeApiKeyRequired"));
      return;
    }
    setOpencodeBusy(true);
    setError(null);
    try {
      if (opencodePreset === "opencodeGo") {
        const { name, baseUrl } = providerPresetDraft(opencodePreset);
        await connectOpenCodeCatalog(name, baseUrl, key);
        // OpenCode Go's own endpoint genuinely rejects Zen's free models —
        // verified live: a free model posted to zen/go/v1 comes back
        // "Model ... is not supported" (401), the same way a Go-exclusive
        // model posted to zen/v1 does. They are not the same catalog
        // filtered by tier; a Go connection cannot reach the free models by
        // picking different model ids, only by also talking to the Zen
        // endpoint. So attach them as their own route instead of leaving a
        // Go subscriber without any free models at all.
        try {
          await connectOpenCodeFreeModels(
            key,
            t("models.ui.opencode.freeRouteName", { name: OPENCODE_ZEN_NAME }),
          );
        } catch (cause) {
          // Non-fatal: the Go connection above already succeeded, and free
          // models are a bonus on top of it, not a requirement for it.
          setError(t("models.ui.errors.opencodeFreeAttachFailed", { detail: String(cause) }));
        }
      } else {
        await connectOpenCodeFreeModels(key);
      }
      setModelRoutes(await api.listModelRoutes());
      setCatalogStatus(await api.getCatalogStatus());
      setOpencodeApiKey("");
      onChanged();
    } catch (cause) {
      setError(t("models.ui.errors.opencodeConnectFailed", { detail: String(cause) }));
    } finally {
      setOpencodeBusy(false);
    }
  }

  const step = result ? 3 : probing ? 2 : 1;

  return (
    <>
      <div className="canvas__head">
        <div>
          <p className="eyebrow">{t("models.title")}</p>
          <h2 className="canvas__title">{t("models.ui.catalogTitle")}</h2>
          <p className="prose" style={{ marginTop: 6 }}>
            {t("models.ui.catalogHint")}
          </p>
        </div>
      </div>

      {error ? <p className="note">{error}</p> : null}

      <Card>
        <Cap>{t("models.ui.chatgpt.title")}</Cap>
        <p className="prose" style={{ marginTop: 12 }}>
          {t("models.ui.chatgpt.description")}
        </p>

        {deviceLogin ? (
          <div className="detect" style={{ marginTop: 16 }}>
            <DetectRow status="attention" name={t("models.ui.oauth.code")} value={deviceLogin.userCode} />
            <DetectRow status="fact" name={t("models.ui.oauth.loginPage")} value={deviceLogin.verificationUri} />
            <p className="prose">{t("models.ui.oauth.browserHint")}</p>
            <div className="rowline">
              <Btn
                soft
                onClick={() => {
                  void navigator.clipboard
                    .writeText(deviceLogin.userCode)
                    .then(() => {
                      setCopyFeedback(t("models.ui.oauth.copied"));
                      window.setTimeout(() => setCopyFeedback(null), 2400);
                    })
                    .catch(() => setCopyFeedback(t("models.ui.oauth.copyFailed")));
                }}
              >
                {t("models.ui.oauth.copy")}
              </Btn>
              {copyFeedback ? (
                <Pill tone={copyFeedback === t("models.ui.oauth.copyFailed") ? "warn" : "ok"}>
                  {copyFeedback}
                </Pill>
              ) : null}
              <Btn
                soft
                onClick={() => {
                  pollGeneration.current += 1;
                  setDeviceLogin(null);
                  setOauthBusy(false);
                }}
              >
                {t("common.cancel")}
              </Btn>
            </div>
          </div>
        ) : null}

        {oauth?.accounts.length ? (
          <Rows>
            {oauth.accounts.map((account) => {
              const accountLabel =
                account.email ?? `ChatGPT ${account.accountId.slice(0, 8)}`;
              const credits = resetCredits[account.accountId];
              const available =
                credits?.credits.filter((credit) => credit.status === "available").length ??
                credits?.availableCount ??
                0;
              const resetError = resetErrors[account.accountId];
              const quotaError = accountQuotaErrors[account.accountId];
              const quotaWindows = accountQuotaWindows(
                accountQuotas[account.accountId] ?? [],
              );
              const isVerifiedCurrent = account.isDefault && oauth.selectionVerified === true;
              const resetsOpen = openResets === account.accountId;
              const soonest = credits ? soonestResetExpiry(credits.credits) : null;
              return (
                <Row key={account.accountId} label={accountLabel}>
                  <span className="acct">
                    <State
                      tone={isVerifiedCurrent ? "ok" : "quiet"}
                      label={isVerifiedCurrent ? t("models.ui.account.active") : t("models.ui.account.authenticated")}
                    />
                    <span className="acct__actions">
                      {quotaWindows.length ? (
                        <AccountQuota windows={quotaWindows} t={t} resetLabel={resetLabel} />
                      ) : (
                        <Pill tone={quotaError ? "warn" : "quiet"}>
                          {quotaError ? t("models.ui.quota.failed") : t("models.ui.quota.loading")}
                        </Pill>
                      )}
                      {/* 這顆藥丸是托盤的把手：張數是讀數，動作在托盤裡每一張
                          券自己那一列。查詢失敗或還在查的時候維持不能按的藥
                          丸 —— 那時候沒有東西可以拉出來，做成按鈕只會給出一
                          個按不動的把手。重試接在同一格，不另外占一欄：那顆
                          按鈕只在出錯時存在，常設欄位會讓整列平常空一格。 */}
                      <span className="acct__resets">
                        {resetError || !credits ? (
                          <Pill tone={resetError ? "warn" : "quiet"}>
                            {resetError
                              ? t("models.ui.reset.failed")
                              : t("models.ui.reset.loading")}
                          </Pill>
                        ) : (
                          <button
                            type="button"
                            id={`resets-${account.accountId}`}
                            className={`pill pill--tap${
                              soonest && resetExpiryTone(soonest) === "soon"
                                ? " pill--warn"
                                : available > 0
                                  ? " pill--ok"
                                  : " pill--quiet"
                            }`}
                            aria-expanded={resetsOpen}
                            aria-controls={`resets-tray-${account.accountId}`}
                            aria-label={t(
                              resetsOpen ? "models.ui.reset.hide" : "models.ui.reset.show",
                            )}
                            onClick={() =>
                              setOpenResets((current) =>
                                current === account.accountId ? null : account.accountId,
                              )
                            }
                          >
                            {`Reset ${available}`}
                            <i className="pill__caret" data-open={resetsOpen} aria-hidden="true" />
                          </button>
                        )}
                        {resetError ? (
                          <Btn
                            soft
                            mini
                            disabled={resetBusy !== null}
                            title={resetError}
                            onClick={() => void retryResetQuery(account.accountId)}
                          >
                            {resetBusy === account.accountId
                              ? t("common.processing")
                              : t("models.ui.quota.retry")}
                          </Btn>
                        ) : null}
                      </span>
                      <Btn
                        soft
                        mini
                        disabled={isVerifiedCurrent || oauthBusy}
                        onClick={() => void setDefaultAccount(account.accountId)}
                      >
                        {isVerifiedCurrent ? t("models.ui.account.current") : t("models.ui.account.useThis")}
                      </Btn>
                      <Btn
                        soft
                        mini
                        disabled={oauthBusy}
                        onClick={() => setPendingConfirm({ kind: "removeAccount", accountId: account.accountId, label: accountLabel })}
                      >
                        {t("common.remove")}
                      </Btn>
                    </span>
                    {resetsOpen && credits && credits.credits.length ? (
                      <ResetLedger
                        id={`resets-tray-${account.accountId}`}
                        credits={credits.credits}
                        t={t}
                        labelledBy={`resets-${account.accountId}`}
                        busy={resetBusy !== null}
                        onUse={(creditId) =>
                          confirmConsumeReset(account.accountId, accountLabel, creditId)
                        }
                      />
                    ) : null}
                  </span>
                </Row>
              );
            })}
          </Rows>
        ) : null}

        {resetFeedback ? (
          <div className="rowline" style={{ marginTop: 12 }}>
            <Pill tone={resetFeedback.tone}>{resetFeedback.text}</Pill>
          </div>
        ) : null}

        <div className="rowline" style={{ marginTop: 16 }}>
          <Btn disabled={oauthBusy} onClick={() => void startCodexLogin()}>
            {oauthBusy && deviceLogin ? t("models.ui.oauth.waiting") : t("models.ui.oauth.login")}
          </Btn>
          {oauth?.authenticated ? (
            <>
              <Btn soft disabled={oauthBusy} onClick={() => void refreshAccount()}>
                {t("models.ui.oauth.refreshToken")}
              </Btn>
              <Btn soft disabled={oauthBusy} onClick={() => setPendingConfirm({ kind: "logoutAll" })}>
                {t("models.ui.oauth.logoutAll")}
              </Btn>
            </>
          ) : null}
        </div>
      </Card>

      {grokRoute ? (
        <Card>
          <Cap>{t("models.ui.grok.title")}</Cap>
          <p className="prose" style={{ marginTop: 12 }}>
            {t("models.ui.grok.description")}
          </p>

          {grokLogin?.state === "waiting" ? (
            <div className="detect" style={{ marginTop: 16 }}>
              <DetectRow status="attention" name={t("models.ui.grok.loginStatus")} value={t("models.ui.oauth.waitingBrowser")} />
              <p className="prose">{t("models.ui.grok.browserHint")}</p>
              <div className="rowline">
                <Btn soft disabled={grokBusy !== null} onClick={() => void cancelGrokLogin()}>
                  {t("common.cancel")}
                </Btn>
              </div>
            </div>
          ) : null}

          {grokAccounts?.accounts.length ? (
            <Rows>
              {grokAccounts.accounts.map((account) => {
                const label =
                  account.email ??
                  (account.source === "external"
                    ? t("models.ui.grok.defaultAccount")
                    : `Grok ${account.accountId.slice(-8)}`);
                const quotaError = grokQuotaErrors[account.accountId];
                const quotaWindows = accountQuotaWindows(grokQuotas[account.accountId] ?? []);
                return (
                  <Row key={account.accountId} label={label}>
                    <span className="acct">
                      <State
                        tone={account.isDefault ? "ok" : "quiet"}
                        label={account.isDefault ? t("models.ui.account.active") : t("models.ui.account.authenticated")}
                      />
                      <span className="acct__actions">
                        {quotaWindows.length ? (
                          <AccountQuota windows={quotaWindows} t={t} resetLabel={resetLabel} />
                        ) : (
                          <Pill tone={quotaError ? "warn" : "quiet"}>
                            {quotaError ? t("models.ui.quota.failed") : t("models.ui.quota.loading")}
                          </Pill>
                        )}
                        <Pill tone="quiet">
                          {account.source === "external" ? t("models.ui.grok.externalCli") : t("models.ui.grok.managed")}
                        </Pill>
                        <Btn
                          soft
                          mini
                          disabled={grokBusy !== null}
                          onClick={() => void refreshGrokAccount(account.accountId)}
                        >
                          {grokBusy === account.accountId ? t("common.processing") : t("models.ui.grok.reauthenticate")}
                        </Btn>
                        <Btn
                          soft
                          mini
                          disabled={account.isDefault || grokBusy !== null}
                          onClick={() => void setDefaultGrokAccount(account.accountId)}
                        >
                          {account.isDefault ? t("models.ui.account.current") : t("models.ui.account.useThis")}
                        </Btn>
                        <Btn
                          soft
                          mini
                          disabled={grokBusy !== null}
                          onClick={() => setPendingConfirm({ kind: "removeGrokAccount", accountId: account.accountId, label })}
                        >
                          {account.source === "external" ? t("models.ui.grok.unlink") : t("common.remove")}
                        </Btn>
                      </span>
                    </span>
                  </Row>
                );
              })}
            </Rows>
          ) : (
            <Empty>{t("models.ui.grok.empty")}</Empty>
          )}

          <div className="rowline" style={{ marginTop: 16 }}>
            <Btn disabled={grokBusy !== null} onClick={() => void startGrokLogin()}>
              {grokBusy === "login" ? t("models.ui.oauth.waiting") : t("models.ui.grok.add")}
            </Btn>
            <Btn
              soft
              disabled={grokBusy !== null || !grokAccounts?.authenticated}
              onClick={() => void refreshGrokModels(grokRoute)}
            >
              {grokBusy === "models" ? t("common.processing") : t("models.ui.grok.refreshModels")}
            </Btn>
          </div>
        </Card>
      ) : null}

      <Card>
        <Cap>{t("models.ui.opencode.title")}</Cap>
        <p className="prose" style={{ marginTop: 12 }}>
          {opencodePreset === "opencodeGo"
            ? t("models.ui.opencode.descriptionGo")
            : t("models.ui.opencode.description")}
        </p>

        <div className="field" style={{ marginTop: 12 }}>
          <label className="field__label" htmlFor="opencode-catalog">
            {t("models.ui.opencode.catalog")}
          </label>
          <select
            id="opencode-catalog"
            className="input"
            value={opencodePreset}
            onChange={(event) => {
              setOpencodePreset(event.target.value as "opencodeZen" | "opencodeGo");
              setError(null);
            }}
          >
            <option value="opencodeZen">{t("models.ui.opencode.catalogZen")}</option>
            <option value="opencodeGo">{t("models.ui.opencode.catalogGo")}</option>
          </select>
          <p className="field__hint">{t("models.ui.opencode.catalogHint")}</p>
        </div>

        {opencodeRoute ? (
          <div className="rowline" style={{ marginTop: 16 }}>
            <State
              tone={opencodeRoute.enabled ? "ok" : "quiet"}
              label={t("models.ui.opencode.connected")}
            />
            <Pill tone="quiet">
              {t("models.ui.modelCatalog.countSelected", {
                selected: (opencodeRoute.selectedModels ?? opencodeRoute.models).length,
                total: opencodeRoute.models.length,
              })}
            </Pill>
          </div>
        ) : null}

        <form
          style={{ marginTop: 16 }}
          onSubmit={(event) => {
            event.preventDefault();
            void connectOpenCodeZen();
          }}
        >
          <div className="field">
            <label className="field__label" htmlFor="opencode-api-key">
              {t("models.ui.opencode.apiKey")}
            </label>
            <input
              id="opencode-api-key"
              className="input"
              type="password"
              autoComplete="off"
              value={opencodeApiKey}
              onChange={(event) => setOpencodeApiKey(event.target.value)}
              placeholder={t("models.ui.opencode.apiKeyPlaceholder")}
            />
          </div>
          <div className="rowline" style={{ marginTop: 14 }}>
            <Btn
              type="submit"
              disabled={opencodeBusy || (opencodePreset === "opencodeGo" && !opencodeApiKey.trim())}
            >
              {opencodeBusy ? t("models.ui.opencode.connecting") : t("models.ui.opencode.connect")}
            </Btn>
          </div>
        </form>
      </Card>

      <div className="grid2">
        <Card>
          <Cap>{t("models.ui.addProvider.title")}</Cap>
          <div className="steps" style={{ marginTop: 18 }}>
            {[
              [t("models.ui.addProvider.steps.endpoint"), t("models.ui.addProvider.steps.endpointHint")],
              [t("models.ui.addProvider.steps.probe"), t("models.ui.addProvider.steps.probeHint")],
              [t("models.ui.addProvider.steps.add"), t("models.ui.addProvider.steps.addHint")],
            ].map(([title, body], index) => (
              <div className={`step${step > index + 1 ? " step--done" : ""}`} key={title}>
                <div className="step__bead">{step > index + 1 ? "✓" : index + 1}</div>
                <div>
                  <h3 className="step__title">{title}</h3>
                  <p className="step__body">{body}</p>
                </div>
              </div>
            ))}
          </div>

          <form
            style={{ marginTop: 22 }}
            onSubmit={(event) => {
              event.preventDefault();
              void probe();
            }}
          >
            {step === 1 ? (
              <>
                <div className="field">
                  <label className="field__label" htmlFor="endpoint">
                    {t("models.ui.addProvider.endpoint")}
                  </label>
                  <input
                    id="endpoint"
                    className="input"
                    placeholder="https://api.example.com/v1"
                    value={endpoint}
                    onChange={(event) => setEndpoint(event.target.value)}
                  />
                </div>
                <div className="field" style={{ marginTop: 12 }}>
                  <label className="field__label" htmlFor="api-key">
                    {t("models.ui.addProvider.apiKey")}
                  </label>
                  <input
                    id="api-key"
                    className="input"
                    type="password"
                    autoComplete="off"
                    value={apiKey}
                    onChange={(event) => setApiKey(event.target.value)}
                    placeholder={t("models.ui.addProvider.apiKeyPlaceholder")}
                  />
                </div>
                <div className="rowline" style={{ marginTop: 18 }}>
                  <Btn type="submit" disabled={probing || !endpoint.trim()}>
                    {t("models.ui.addProvider.startProbe")}
                  </Btn>
                </div>
              </>
            ) : step === 2 ? (
              <p className="prose">{t("models.ui.addProvider.probing")}</p>
            ) : result ? (
              <>
                <div className="detect">
                  <DetectRow status={result.reachable ? "ok" : "attention"} name={t("models.ui.probe.reachable")} value={result.reachable ? t("common.yes") : t("common.no")} />
                  <DetectRow
                    status={result.wire ? "ok" : "attention"}
                    name={t("models.ui.probe.wire")}
                    value={wireLabel(result.wire, t)}
                  />
                  <DetectRow
                    status={result.models.length ? "ok" : "attention"}
                    name={t("common.model")}
                    value={
                      result.models.length
                        ? t("models.ui.probe.modelCount", { count: result.models.length })
                        : t("models.ui.probe.modelsMissing")
                    }
                  />
                  <DetectRow
                    status={result.contextWindow === null ? "attention" : "ok"}
                    name={t("models.ui.probe.context")}
                    value={result.contextWindow === null ? t("models.ui.probe.required") : exact(result.contextWindow)}
                  />
                  <DetectRow
                    status={
                      result.modelCapabilities.some(
                        (capability) => capability.probeIssue === "timeout" && capability.streaming == null,
                      )
                        ? "attention"
                        : result.streaming
                          ? "ok"
                          : "fact"
                    }
                    name={t("models.ui.probe.streaming")}
                    value={
                      result.modelCapabilities.some(
                        (capability) => capability.probeIssue === "timeout" && capability.streaming == null,
                      )
                        ? t("models.ui.probe.unknown")
                        : result.streaming
                          ? t("models.ui.probe.supported")
                          : t("models.ui.probe.unsupported")
                    }
                  />
                  <DetectRow
                    status={result.modelCapabilities.some((capability) => capability.toolCalling) ? "ok" : "attention"}
                    name={t("models.ui.probe.toolCalling")}
                    value={
                      result.modelCapabilities.some((capability) => capability.toolCalling)
                        ? t("models.ui.probe.typedToolCalls")
                        : t("models.ui.probe.toolCallingUnavailable")
                    }
                  />
                  <DetectRow status={result.reasoning ? "ok" : "fact"} name={t("models.ui.probe.reasoning")} value={result.reasoning ? t("models.ui.probe.detected") : t("models.ui.probe.notDetected")} />
                  <DetectRow
                    status={result.serverSideResume ? "ok" : "fact"}
                    name={t("models.ui.probe.serverResume")}
                    value={result.serverSideResume ? t("models.ui.probe.remembers") : t("models.ui.probe.localHistory")}
                  />
                </div>

                {result.models.length > 0 ? (
                  <div className="detected-models" aria-label={t("models.ui.probe.detectedModels")}>
                    {result.models.map((model) => {
                      const capability = result.modelCapabilities.find(
                        (candidate) => candidate.model.toLowerCase() === model.toLowerCase(),
                      );
                      const openCodeModel = isOpenCodeZenEndpoint(endpoint);
                      // OpenCode Zen/Go capabilities come from Vellum's bundled
                      // model registry, so `wire === null` there means the registry
                      // itself identifies a provider-native protocol Vellum does not
                      // translate — not a slow or failed inference probe.
                      //
                      // `probeVersion == null` is still required: once a model has
                      // been probed, a null wire can also mean a Chat/Responses model
                      // that failed its live tool-call check, and that is a broken
                      // model rather than an unsupported protocol.
                      const unsupportedProtocol = openCodeModel && capability?.probeVersion == null && capability?.wire === null;
                      const catalogReady = Boolean(
                        openCodeModel && capability?.wire && capability.toolCalling !== false,
                      );
                      const verified = capability?.toolCalling === true && capability?.wire !== null;
                      const canVerify = !verified && !unsupportedProtocol && !catalogReady;
                      const incompatible = unsupportedProtocol || capability?.toolCalling === false;
                      const issueLabel = probeIssueLabel(capability?.probeIssue, t, capability);
                      const busy = modelProbeBusy === model;
                      return (
                      <label key={model} className="rowline" data-disabled={incompatible || undefined}>
                        <input
                          type="checkbox"
                          disabled={incompatible || busy}
                          checked={selectedModels.includes(model)}
                          onChange={() => {
                            if (canVerify) {
                              void verifyPendingModel(model);
                              return;
                            }
                            setSelectedModels((current) =>
                              current.includes(model)
                                ? current.filter((candidate) => candidate !== model)
                                : [...current, model],
                            );
                          }}
                        />
                        <code>{model}</code>
                        {capability?.free && capability.deprecated !== true ? (
                          <Pill tone="ok">{t("models.ui.modelCatalog.free")}</Pill>
                        ) : null}
                        {capability?.deprecated ? (
                          <Pill tone="warn">{t("models.ui.modelCatalog.deprecated")}</Pill>
                        ) : null}
                        {busy ? (
                          <span className="field__hint">{t("models.ui.probe.verifyingModel")}</span>
                        ) : unsupportedProtocol ? (
                          <span className="field__hint">{t("models.ui.probe.unsupportedOpenCodeProtocol")}</span>
                        ) : issueLabel ? (
                          <span className="field__hint">{issueLabel}</span>
                        ) : canVerify ? (
                          <span className="field__hint">{t("models.ui.probe.verifyToImport")}</span>
                        ) : incompatible ? (
                          <span className="field__hint">{t("models.ui.probe.chatOnly")}</span>
                        ) : null}
                      </label>
                      );
                    })}
                    <p className="field__hint">
                      {t("models.ui.probe.selectionHint")}
                    </p>
                  </div>
                ) : null}

                {selectedModels.length > 1 ? (
                  <div className="field" style={{ marginTop: 12 }}>
                    <label className="field__label" htmlFor="default-model">
                      {t("models.ui.probe.defaultModel")}
                    </label>
                    <select
                      id="default-model"
                      className="input"
                      value={
                        selectedModels.includes(missing.model ?? "")
                          ? missing.model
                          : selectedModels[0]
                      }
                      onChange={(event) =>
                        setMissing((previous) => ({ ...previous, model: event.target.value }))
                      }
                    >
                      {selectedModels.map((model) => (
                        <option key={model} value={model}>
                          {model}
                        </option>
                      ))}
                    </select>
                    <p className="field__hint">
                      {t("models.ui.probe.defaultModelHint")}
                    </p>
                  </div>
                ) : null}

                <div className="field" style={{ marginTop: 12 }}>
                  <label className="field__label" htmlFor="provider-name">
                    {t("models.ui.modelCatalog.renameProvider")}
                  </label>
                  <input
                    id="provider-name"
                    className="input"
                    value={draftProviderName}
                    onChange={(event) => setDraftProviderName(event.target.value)}
                  />
                  <p className="field__hint">{t("models.ui.modelCatalog.renameHint")}</p>
                </div>

                {selectedModels.length ? (
                  <div className="field" style={{ marginTop: 12 }}>
                    <span className="field__label">
                      {t("models.ui.modelCatalog.renameModel")}
                    </span>
                    {selectedModels.map((model) => (
                      <div className="rename__row" key={model}>
                        <code className="rename__current" title={model}>
                          {model}
                        </code>
                        <input
                          className="input rename__input"
                          placeholder={t("models.ui.modelCatalog.renameModelPlaceholder")}
                          value={draftModelNames[model] ?? ""}
                          onChange={(event) =>
                            setDraftModelNames((current) => ({
                              ...current,
                              [model]: event.target.value,
                            }))
                          }
                        />
                      </div>
                    ))}
                  </div>
                ) : null}

                {result.needsInput.map((field) => (
                  <div className="field" key={field} style={{ marginTop: 12 }}>
                    <label className="field__label" htmlFor={`need-${field}`}>
                      {t(FIELD_LABEL[field] ?? field)}
                    </label>
                    {field === "wire" ? (
                      <>
                        <select
                          id={`need-${field}`}
                          className="input"
                          value={missing.wire ?? ""}
                          onChange={(event) =>
                            setMissing((previous) => ({
                              ...previous,
                              wire: event.target.value,
                            }))
                          }
                        >
                          <option value="">{t("models.ui.probe.select")}</option>
                          <option value="chat">Chat Completions · /chat/completions</option>
                          <option value="responses">Responses API · /responses</option>
                        </select>
                        <p className="field__hint">
                          {t("models.ui.probe.wireHint")}
                        </p>
                      </>
                    ) : (
                      <input
                        id={`need-${field}`}
                        className="input"
                        value={missing[field] ?? ""}
                        onChange={(event) =>
                          setMissing((previous) => ({
                            ...previous,
                            [field]: event.target.value,
                          }))
                        }
                        placeholder={t("models.ui.probe.manualPlaceholder")}
                      />
                    )}
                  </div>
                ))}

                <div className="rowline" style={{ marginTop: 18 }}>
                  <Btn
                    disabled={!selectedModels.length}
                    onClick={() => void createRoute()}
                  >
                    {t("models.ui.addProvider.add")}
                  </Btn>
                  <Btn soft onClick={() => setResult(null)}>{t("models.ui.addProvider.reprobe")}</Btn>
                </div>
              </>
            ) : null}
          </form>
        </Card>

        <Card quiet>
          <Cap>{t("models.ui.probe.settingsTitle")}</Cap>
          <Rows>
            <Row label={t("models.ui.probe.requestSize")}>max_tokens=1</Row>
            <Row label={t("models.ui.probe.preferredWire")}>{t("models.ui.probe.preferredWireValue")}</Row>
            <Row label={t("models.ui.probe.contextSource")}>{t("models.ui.probe.contextSourceValue")}</Row>
            <Row label={t("models.ui.probe.history")}>{t("models.ui.probe.historyValue")}</Row>
          </Rows>
        </Card>
      </div>

      {/* 模型選單。
          這一區回答的是「Codex 的選單裡要出現哪些模型」，所以主角是**勾選**，
          不是供應商的元資料。原本把狀態、格式、最近使用、兩顆按鈕、再加一排
          裸 checkbox 全塞進同一列，每列的欄位還會隨條件左右浮動 —— 難怪很亂。

          改成：一家供應商一個托盤。收合時一行講完「誰、什麼狀態、幾個模型在
          選單裡」；展開才是逐個模型的勾選表，欄位全部釘死對齊。 */}
      <Card>
        <div>
          <Cap>{t("models.ui.modelCatalog.title")}</Cap>
        </div>
        <p className="rows__hint" style={{ marginTop: 8 }}>
          {t("models.ui.modelCatalog.hint")}
        </p>

        {routes.length === 0 ? (
          <Empty>{t("models.ui.modelCatalog.empty")}</Empty>
        ) : (
          <div className="ledger" style={{ marginTop: 6 }}>
            {routes.map((route) => {
              const mine = modelRoutes.filter((model) => model.routeId === route.id);
              const inCatalog = mine.some((model) =>
                catalogStatus.injectedModelIds.includes(model.catalogId),
              );
              const state = providerState({
                enabled: route.enabled,
                proxyRunning: catalogStatus.proxyRunning,
                applied: inCatalog,
              });
              const picked = route.selectedModels ?? route.models;
              /* 官方線路的模型清單由 OpenAI 決定，不能挑 */
              const pickable = route.providerKind !== "official";
              const busy = routeBusy === route.id;

              return (
                <Tray
                  key={route.id}
                  label={
                    <span className="prov">
                      <span className="prov__name">{route.name}</span>
                      <State tone={state.tone} label={t(state.labelKey)} />
                      <span className="prov__host">
                        {route.baseUrl.replace(/^https?:\/\//, "")}
                      </span>
                    </span>
                  }
                  count={
                    pickable
                      ? t("models.ui.modelCatalog.countSelected", { selected: picked.length, total: route.models.length })
                      : t("models.ui.modelCatalog.count", { count: route.models.length })
                  }
                >
                  {(state.remedyKey ? t(state.remedyKey) : null) ? (
                    <p className="note" style={{ margin: "2px 0 14px" }}>{(state.remedyKey ? t(state.remedyKey) : null)}</p>
                  ) : null}

                  {pickable ? (
                    <div className="rename">
                      <span className="rename__label">
                        {t("models.ui.modelCatalog.renameProvider")}
                      </span>
                      {renamingRoute === route.id ? (
                        <>
                          <input
                            className="input rename__input"
                            autoFocus
                            value={routeNameDraft}
                            disabled={busy}
                            onChange={(event) => setRouteNameDraft(event.target.value)}
                            onKeyDown={(event) => {
                              if (event.key === "Enter") void saveRouteName(route);
                              if (event.key === "Escape") setRenamingRoute(null);
                            }}
                          />
                          <Btn mini disabled={busy} onClick={() => void saveRouteName(route)}>
                            {t("models.ui.modelCatalog.renameSave")}
                          </Btn>
                          <Btn soft mini disabled={busy} onClick={() => setRenamingRoute(null)}>
                            {t("models.ui.modelCatalog.renameCancel")}
                          </Btn>
                        </>
                      ) : (
                        <>
                          <code className="rename__current">{route.name}</code>
                          <Btn
                            soft
                            mini
                            disabled={busy}
                            onClick={() => {
                              setRouteNameDraft(route.name);
                              setRenamingRoute(route.id);
                            }}
                          >
                            {t("models.ui.modelCatalog.rename")}
                          </Btn>
                        </>
                      )}
                      <p className="field__hint rename__hint">
                        {t("models.ui.modelCatalog.renameHint")}
                      </p>
                    </div>
                  ) : null}

                  {route.providerKind === "openAiCompatible" ? (
                    <div className="rename" style={{ marginTop: 10 }}>
                      <span className="rename__label">
                        {t("models.ui.modelCatalog.allowPrivateNetworkHttp")}
                      </span>
                      <Toggle
                        checked={route.insecureHttpPolicy === "allowPrivateNetwork"}
                        onChange={() => void toggleAllowPrivateNetworkHttp(route)}
                        label={t("models.ui.modelCatalog.allowPrivateNetworkHttp")}
                      />
                      <p className="field__hint rename__hint">
                        {t("models.ui.modelCatalog.allowPrivateNetworkHttpHint")}
                      </p>
                    </div>
                  ) : null}

                  {pickable && route.models.length ? (
                    <div className="menu">
                      {route.models.map((model) => {
                        const on = picked.includes(model);
                        const catalog = mine.find((candidate) => candidate.upstreamModel === model);
                        const capability = route.modelCapabilities.find(
                          (item) => item.model.toLowerCase() === model.toLowerCase(),
                        );
                        const effortLevels = capability?.reasoningEfforts?.length
                          ? capability.reasoningEfforts
                          : (catalog?.reasoningEfforts ?? []);
                        // `reasoningEfforts.length === 0` alone conflates three
                        // different states: never probed, probed and confirmed
                        // the provider has no discrete levels, and probed but
                        // inconclusive (rate limit/5xx/timeout/negative-control
                        // failure). Only the middle one may render as
                        // "automatic" — the others need their own label so a
                        // never-probed model does not look identical to one
                        // that genuinely has nothing to pick.
                        const effortStatus = capability?.effortProbeStatus ?? "not_probed";
                        const effortDisplay =
                          effortStatus === "supported" && effortLevels.length
                            ? effortLevels.join(" / ")
                            : effortStatus === "indeterminate"
                              ? t("models.ui.modelCatalog.effortUnverified")
                              : effortStatus === "not_probed"
                                ? t("models.ui.modelCatalog.effortNotProbed")
                                : t("models.ui.modelCatalog.auto");
                        const vision = capability?.vision === true;
                        const alias = capability?.displayName?.trim();
                        const renameKey = `${route.id} ${model}`;
                        const editingName = renamingModel === renameKey;
                        /* 只有進了目錄的模型才有 catalogId，也才設得了上下文
                           長度。沒有的時候鉛筆停用並說明原因，不要給一個按了
                           沒反應的按鈕。 */
                        const modelRoute = modelRoutes.find(
                          (candidate) =>
                            candidate.routeId === route.id &&
                            candidate.upstreamModel.toLowerCase() === model.toLowerCase(),
                        );
                        const windowKey = `${route.id} ${model}`;
                        const editingWindow = editingWindowKey === windowKey;
                        const probedWindow = capability?.contextWindow ?? catalog?.contextWindow;
                        const overrideWindow = modelRoute?.contextWindowOverride ?? null;
                        const shownWindow = overrideWindow ?? probedWindow;
                        const openCodeModel = isOpenCodeZenEndpoint(route.baseUrl);
                        // See the matching comment in the detected-models step above:
                        // `wire === null` alone does not mean "unsupported protocol"
                        // once every candidate gets probed up front — only a model
                        // that was never attempted (`probeVersion` unset) still is.
                        const verified = capability?.toolCalling === true && capability?.wire !== null;
                        const unsupportedProtocol = openCodeModel && capability?.probeVersion == null && capability?.wire === null;
                        const catalogReady = Boolean(
                          openCodeModel && capability?.wire && capability.toolCalling !== false,
                        );
                        const effortDiagnostic = effortProbeDiagnostic(capability, t);
                        // A harness-verified model whose Effort probe never
                        // ran, or ran but was inconclusive, still needs the
                        // per-model verify button -- `catalogReady` alone
                        // would hide it the moment tool-calling passes, even
                        // though Effort itself is still an open question.
                        const effortNeedsVerification =
                          (capability?.reasoning ?? catalog?.reasoning) === true &&
                          effortProbeCanRetry(capability);
                        const requiresVerification =
                          openCodeModel &&
                          !unsupportedProtocol &&
                          ((!verified && !catalogReady) || effortNeedsVerification);
                        const issueLabel = probeIssueLabel(capability?.probeIssue, t, capability);
                        const verifying = modelProbeBusy === `${route.id}:${model}`;
                        return (
                          // A div, not a label: the row holds several independent
                          // controls, and a wrapping label would make each of them
                          // also flip the selection checkbox.
                          <div className="menu__row" key={model}>
                            <label className="menu__pick">
                              <input
                                type="checkbox"
                                className="menu__box"
                                checked={on}
                                disabled={
                                  busy ||
                                  verifying ||
                                  capability?.toolCalling === false ||
                                  unsupportedProtocol
                                }
                                onChange={() => void toggleRouteModel(route, model)}
                              />
                              {editingName ? null : (
                                // Wrapped so the id and its optional Free badge
                                // together still occupy exactly one grid cell —
                                // `.menu__pick` is `display: contents`, so a bare
                                // sibling here would land in the next column
                                // instead of riding along with the model id.
                                <span className="menu__id-wrap">
                                  {/* The upstream id stays in the title: the alias
                                      is what you read, the id is what identifies it. */}
                                  <code className="menu__id" title={model}>
                                    {alias || model}
                                  </code>
                                  {capability?.deprecated ? (
                                    <Pill tone="warn">{t("models.ui.modelCatalog.deprecated")}</Pill>
                                  ) : null}
                                  {capability?.free && capability.deprecated !== true ? (
                                    <Pill tone="ok">{t("models.ui.modelCatalog.free")}</Pill>
                                  ) : null}
                                </span>
                              )}
                            </label>
                            {editingName ? (
                              <input
                                className="input menu__rename"
                                autoFocus
                                placeholder={t("models.ui.modelCatalog.renameModelPlaceholder")}
                                value={modelNameDraft}
                                disabled={busy}
                                onChange={(event) => setModelNameDraft(event.target.value)}
                                onKeyDown={(event) => {
                                  if (event.key === "Enter") void saveModelName(route, model);
                                  if (event.key === "Escape") setRenamingModel(null);
                                }}
                                onBlur={() => void saveModelName(route, model)}
                              />
                            ) : null}
                            {/* 上下文長度就地編輯。鉛筆貼著它要改的那個數字，
                                不放進右邊那排動作按鈕 —— 那排是「對這個模型做
                                什麼」，這裡是「把這個值改掉」。 */}
                            <span className="menu__win">
                              {editingWindow ? (
                                <input
                                  className="input menu__winput"
                                  autoFocus
                                  inputMode="numeric"
                                  placeholder={t("models.ui.modelCatalog.windowAuto")}
                                  aria-label={t("models.ui.modelCatalog.windowEdit")}
                                  value={windowDraft}
                                  disabled={busy}
                                  onChange={(event) => setWindowDraft(event.target.value)}
                                  onKeyDown={(event) => {
                                    if (event.key === "Enter" && modelRoute) {
                                      void saveModelWindow(route, modelRoute.catalogId);
                                    }
                                    if (event.key === "Escape") setEditingWindowKey(null);
                                  }}
                                  onBlur={() => {
                                    if (modelRoute) void saveModelWindow(route, modelRoute.catalogId);
                                  }}
                                />
                              ) : (
                                <>
                                  <span
                                    className="menu__winval"
                                    data-manual={overrideWindow !== null}
                                    title={
                                      overrideWindow !== null
                                        ? t("models.ui.modelCatalog.windowManual")
                                        : undefined
                                    }
                                  >
                                    {shownWindow
                                      ? `${exact(shownWindow)} ${t("models.ui.modelCatalog.tokenUnit")}`
                                      : t("models.ui.modelCatalog.unknownWindow")}
                                  </span>
                                  <button
                                    type="button"
                                    className="menu__pencil"
                                    disabled={busy || !modelRoute}
                                    title={
                                      modelRoute
                                        ? t("models.ui.modelCatalog.windowHint")
                                        : t("models.ui.modelCatalog.windowUnavailable")
                                    }
                                    aria-label={t("models.ui.modelCatalog.windowEdit")}
                                    onClick={() => {
                                      setWindowDraft(
                                        overrideWindow !== null ? String(overrideWindow) : "",
                                      );
                                      setEditingWindowKey(editingWindow ? null : windowKey);
                                    }}
                                  >
                                    {/* 手繪的鉛筆，不用 emoji：emoji 在不同平台
                                        長得不一樣，而且不會跟著主題換色。 */}
                                    <svg
                                      viewBox="0 0 16 16"
                                      width="12"
                                      height="12"
                                      aria-hidden="true"
                                      fill="none"
                                      stroke="currentColor"
                                      strokeWidth="1.4"
                                      strokeLinecap="round"
                                      strokeLinejoin="round"
                                    >
                                      <path d="M11.2 2.4a1.4 1.4 0 0 1 2 2L5.6 12H3.5v-2.1z" />
                                      <path d="M10 3.6 12.4 6" />
                                    </svg>
                                  </button>
                                </>
                              )}
                            </span>
                            <span className="menu__caps">
                              {(capability?.reasoning ?? catalog?.reasoning) ? t("models.ui.modelCatalog.reasoning") : t("models.ui.modelCatalog.noReasoning")}
                              {(capability?.reasoning ?? catalog?.reasoning) && capability?.toolCalling !== false && !issueLabel && !capability?.lastProbeFailed ? (
                                <> · {t("models.ui.modelCatalog.effort")} {effortDisplay}</>
                              ) : null}
                              {issueLabel ? (
                                <>{` · ${issueLabel}`}</>
                              ) : capability?.toolCalling === false ? (
                                <> · {t(
                                  unsupportedProtocol
                                    ? "models.ui.probe.unsupportedOpenCodeProtocol"
                                    : openCodeModel
                                      ? "models.ui.probe.toolCallingUnavailable"
                                      : "models.ui.probe.chatOnly",
                                )}</>
                              ) : null}
                            </span>
                            {effortDiagnostic ? (
                              <span className="menu__diagnostic">{effortDiagnostic}</span>
                            ) : null}
                            <span className="menu__actions">
                              {requiresVerification ? (
                                <button
                                  type="button"
                                  className="menu__vision"
                                  disabled={busy || verifying}
                                  onClick={() => void reprobeRouteModel(route, model)}
                                >
                                  {verifying
                                    ? t("models.ui.probe.verifyingModel")
                                    : effortStatus === "indeterminate"
                                      ? t("models.ui.modelCatalog.retryEffortProbe")
                                      : t("models.ui.probe.verifyModel")}
                                </button>
                              ) : null}
                              <button
                                type="button"
                                className="menu__vision"
                                disabled={busy}
                                title={t("models.ui.modelCatalog.renameHint")}
                                aria-label={t("models.ui.modelCatalog.renameModel")}
                                onClick={() => {
                                  setModelNameDraft(alias ?? "");
                                  setRenamingModel(editingName ? null : renameKey);
                                }}
                              >
                                {t("models.ui.modelCatalog.rename")}
                              </button>
                              <button
                                type="button"
                                className="menu__vision"
                                aria-pressed={vision}
                                disabled={busy}
                                title={t("models.ui.modelCatalog.visionHint")}
                                aria-label={
                                  vision
                                    ? t("models.ui.modelCatalog.visionOn")
                                    : t("models.ui.modelCatalog.visionOff")
                                }
                                onClick={() => void toggleModelVision(route, model, !vision)}
                              >
                                {t("models.ui.modelCatalog.vision")}
                              </button>
                            </span>
                          </div>
                        );
                      })}
                    </div>
                  ) : mine.length ? (
                    <div className="menu">
                      {mine.map((model) => (
                        <div className="menu__row" key={model.catalogId}>
                          <span className="menu__box" aria-hidden="true" />
                          <code className="menu__id">{model.upstreamModel}</code>
                          <span className="menu__win">
                            {model.contextWindow ? `${exact(model.contextWindow)} ${t("models.ui.modelCatalog.tokenUnit")}` : t("models.ui.modelCatalog.unknownWindow")}
                          </span>
                          <span className="menu__caps">
                            {model.reasoning ? t("models.ui.modelCatalog.reasoning") : t("models.ui.modelCatalog.noReasoning")}
                            {model.reasoning ? (
                              <> · {t("models.ui.modelCatalog.effort")} {model.reasoningEfforts.length ? model.reasoningEfforts.join(" / ") : t("models.ui.modelCatalog.auto")}</>
                            ) : null}
                          </span>
                        </div>
                      ))}
                    </div>
                  ) : (
                    <p className="note" style={{ margin: "2px 0 12px" }}>
                      {t("models.ui.modelCatalog.capabilityMissing")}
                    </p>
                  )}

                  {/* 動作固定在最下面靠右，不跟狀態擠同一行 —— 那正是原本
                      每列位置都不一樣的原因。 */}
                  <div className="menu__foot">
                    <span className="rows__hint">
                      {route.wire === "responses" ? t("models.ui.modelCatalog.responses") : t("models.ui.modelCatalog.chat")}
                      {route.isCurrent ? ` · ${t("models.ui.modelCatalog.recent")}` : ""}
                      {routeBusy === route.id
                        ? ` · ${t("models.ui.modelCatalog.reprobeInProgress", {
                            count: (route.selectedModels ?? route.models).length,
                          })}`
                        : reprobeReports[route.id]
                          ? ` · ${
                              reprobeReports[route.id]!.failed
                                ? t("models.ui.modelCatalog.reprobeSummaryWithFailures", {
                                    succeeded: reprobeReports[route.id]!.succeeded,
                                    targeted: reprobeReports[route.id]!.targeted,
                                    failed: reprobeReports[route.id]!.failed,
                                  })
                                : t("models.ui.modelCatalog.reprobeSummary", {
                                    succeeded: reprobeReports[route.id]!.succeeded,
                                    targeted: reprobeReports[route.id]!.targeted,
                                  })
                            }`
                          : ""}
                    </span>
                    <span className="acct__actions">
                      {route.providerKind === "openAiCompatible" ? (
                        <Btn soft mini disabled={busy} onClick={() => void reprobeRoute(route)}>
                          {t("models.ui.modelCatalog.reprobe")}
                        </Btn>
                      ) : null}
                      <Btn soft mini disabled={busy} onClick={() => void toggleEnabled(route)}>
                        {route.enabled ? t("common.disabled") : t("common.enabled")}
                      </Btn>
                      {pickable ? (
                        <Btn
                          soft
                          mini
                          disabled={busy}
                          onClick={() => setPendingConfirm({ kind: "removeRoute", route })}
                        >
                          {t("common.remove")}
                        </Btn>
                      ) : null}
                    </span>
                  </div>
                  {reprobeReports[route.id]?.errors.length ? (
                    <div aria-label={t("models.ui.modelCatalog.reprobe")}>
                      {reprobeReports[route.id]!.errors.map((probeError) => (
                        <p className="note" key={`${probeError.model}:${probeError.stage ?? "probe"}`}>
                          {reprobeErrorDetail(probeError, t)}
                        </p>
                      ))}
                    </div>
                  ) : null}
                </Tray>
              );
            })}
          </div>
        )}
      </Card>
      <p className="note">
        {t("models.ui.modelCatalog.footerNote")}
      </p>

      {pendingConfirm ? (
        <Confirm
          open
          title={t(`models.ui.confirm.${pendingConfirm.kind}.title`)}
          confirmLabel={t(`models.ui.confirm.${pendingConfirm.kind}.confirmLabel`)}
          danger
          facts={
            pendingConfirm.kind === "removeRoute"
              ? [
                  { key: t("models.ui.confirm.removeRoute.factProvider"), value: pendingConfirm.route.name },
                  { key: t("models.ui.confirm.removeRoute.factEffect"), value: t("models.ui.confirm.removeRoute.effectValue") },
                ]
              : pendingConfirm.kind === "logoutAll"
                ? [
                    { key: t("models.ui.confirm.logoutAll.factAccount"), value: t("models.ui.confirm.logoutAll.accountsValue") },
                    { key: t("models.ui.confirm.logoutAll.factEffect"), value: t("models.ui.confirm.logoutAll.effectValue") },
                  ]
                : [
                    { key: t(`models.ui.confirm.${pendingConfirm.kind}.factAccount`), value: pendingConfirm.label },
                    { key: t(`models.ui.confirm.${pendingConfirm.kind}.factEffect`), value: t(`models.ui.confirm.${pendingConfirm.kind}.effectValue`) },
                  ]
          }
          onCancel={() => setPendingConfirm(null)}
          onConfirm={() => {
            const confirmed = pendingConfirm;
            setPendingConfirm(null);
            if (confirmed.kind === "removeAccount") void removeAccount(confirmed.accountId);
            else if (confirmed.kind === "logoutAll") void logoutAccounts();
            else if (confirmed.kind === "removeGrokAccount") void removeGrokAccount(confirmed.accountId);
            else if (confirmed.kind === "removeRoute") void removeRoute(confirmed.route);
            else void consumeReset(confirmed.accountId, confirmed.creditId);
          }}
        />
      ) : null}
    </>
  );
}

/**
 * 探測結果的一列。
 *
 * 原本是「Pill + 名稱 + 值」用 flex 排，而 pill 的寬度會隨字數變
 * （已探測／需補充），於是每一列的名稱欄都在左右浮動，一堆列疊起來
 * 完全對不齊 —— 而且那顆 pill 本身也比它承載的資訊量大太多。
 *
 * 改用 KV：記號欄寬度釘死，記號換成筆畫式的 State（跟全 app 一致），
 * 名稱與值就永遠對齊。
 */
function DetectRow({
  status,
  name,
  value,
}: {
  status: "ok" | "fact" | "attention";
  name: string;
  value: string;
}) {
  const { t } = useTranslation();
  const tone = status === "attention" ? "warn" : status === "ok" ? "ok" : "quiet";
  return (
    <KV
      mark={<State tone={tone} label={t(status === "attention" ? "models.ui.errors.detectAttention" : "models.ui.errors.detectFact")} />}
      label={name}
      value={value}
    />
  );
}
