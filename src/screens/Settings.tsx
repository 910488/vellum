import { useLocaleFormat } from "@/i18n/useLocaleFormat";
/**
 * 設定 —— 調一次就不再碰的東西。
 *
 * 收進來的原本各自佔了一級導覽或擠在現況頁的按鈕列：
 *   自動審查        只有三個控制項，撐不起一個一級導覽
 *   還原 Codex      出事才用一次的保險，不該跟主動作並排
 *   要顯示的供應商  純顯示偏好，本來混在現況頁的按鈕列裡
 *   進階            排空、模型選單版本回復 —— 救援工具，不是每天要看的東西
 *   結束 Vellum     這是視窗層級的事，放在頁面按鈕列裡本來就不對，
 *                   而且它跟「啟動 Proxy」並排非常危險
 */
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { getLocalePreference, setLocalePreference } from "@/i18n";
import { I18N_LANGUAGE_SELECTOR_ENABLED, type LocalePreference } from "@/i18n/locale";
import { api } from "@/lib/api";
import { startVisiblePoll } from "@/lib/visiblePoll";
import { noticeText } from "@/lib/notice";
import { Btn, Cap, Card, Empty, Row, Rows, Segment, State, Toggle, Tray } from "@/components/ui";
import {
  REVIEW_POLICIES,
  fallbackShare,
  needsFallback,
  policyMeta,
  policyOf,
  statsToShow,
} from "@/lib/review";
import {
  SEARCH_REACHES,
  domainRuleCount,
  formatDomainList,
  isOn as webSearchIsOn,
  needsBraveKey,
  parseDomainList,
  reachOf,
  setReach,
  turnOff,
  turnOn,
} from "@/lib/webSearch";
import type {
  CatalogVersion,
  CodexOAuthStatus,
  ModelRoute,
  RestartResult,
  RestoreResult,
  ReviewPolicy,
  ReviewSettings,
  ReviewStats,
  Route,
  RuntimeStatus,
  SubagentMode,
  SubagentCapability,
  SubagentSettings,
  WebSearchProbeResult,
  WebSearchSettings,
  WebSearchSettingsView,
} from "@/types";
const DASHBOARD_KEY = "vellum.dashboard.providers";

export function Settings({
  onChanged,
  onOpenOnboarding,
  refreshVersion,
  onRefreshComplete,
  active = true,
}: {
  onChanged: () => void;
  onOpenOnboarding: () => void;
  refreshVersion: number;
  onRefreshComplete: (version: number) => void;
  active?: boolean;
}) {
  const { t } = useTranslation();
  const { sinceLabel } = useLocaleFormat();
  const [localePreference, setLocalePreferenceState] = useState<LocalePreference>(getLocalePreference());
  const [settings, setSettings] = useState<ReviewSettings | null>(null);
  const [routes, setRoutes] = useState<Route[]>([]);
  const [models, setModels] = useState<ModelRoute[]>([]);
  const [runtime, setRuntime] = useState<RuntimeStatus | null>(null);
  const [codexRestarting, setCodexRestarting] = useState(false);
  const [versions, setVersions] = useState<CatalogVersion[]>([]);
  const [dashboardRouteIds, setDashboardRouteIds] = useState<string[]>([]);
  const [restoreResult, setRestoreResult] = useState<RestoreResult | null>(null);
  const [restart, setRestart] = useState<RestartResult | null>(null);
  const [busy, setBusy] = useState(false);
  const [logExporting, setLogExporting] = useState(false);
  const [logExportPath, setLogExportPath] = useState<string | null>(null);
  const [reviewSaving, setReviewSaving] = useState(false);
  const [reviewNotice, setReviewNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [stats, setStats] = useState<ReviewStats | null>(null);
  const [webSearch, setWebSearch] = useState<WebSearchSettingsView | null>(null);
  const [braveKeyDraft, setBraveKeyDraft] = useState("");
  const [allowDraft, setAllowDraft] = useState("");
  const [blockDraft, setBlockDraft] = useState("");
  const [webSearchSaving, setWebSearchSaving] = useState(false);
  const [webSearchNotice, setWebSearchNotice] = useState<string | null>(null);
  const [searchProbe, setSearchProbe] = useState<WebSearchProbeResult | null>(null);
  const [searchProbeError, setSearchProbeError] = useState<string | null>(null);
  /** 我們什麼時候問的。上游那筆結果是何時算的，我們不知道，就不假裝知道。 */
  const [searchProbedAt, setSearchProbedAt] = useState<number | null>(null);
  const [searchProbing, setSearchProbing] = useState(false);
  const [subagent, setSubagent] = useState<SubagentSettings | null>(null);
  const [subagentCapability, setSubagentCapability] = useState<SubagentCapability | null>(null);
  const [subagentModels, setSubagentModels] = useState<ModelRoute[]>([]);
  const [oauth, setOauth] = useState<CodexOAuthStatus | null>(null);
  const [subagentSaving, setSubagentSaving] = useState(false);
  const [subagentNotice, setSubagentNotice] = useState<string | null>(null);
  const reviewGeneration = useRef(0);
  const reviewRef = useRef<ReviewSettings | null>(null);
  const reviewLastSavedRef = useRef<ReviewSettings | null>(null);
  const reviewSaveQueue = useRef<Promise<void>>(Promise.resolve());
  const webSearchGeneration = useRef(0);
  const subagentGeneration = useRef(0);
  const subagentRef = useRef<SubagentSettings | null>(null);
  const subagentLastSavedRef = useRef<SubagentSettings | null>(null);
  const subagentSaveQueue = useRef<Promise<void>>(Promise.resolve());

  useEffect(() => {
    let alive = true;
    const load = async () => {
      const [
        nextSettings,
        nextRoutes,
        nextModels,
        nextRuntime,
        nextVersions,
        nextStats,
        nextWebSearch,
        nextSubagent,
        nextSubagentCapability,
        nextSubagentModels,
        nextOauth,
      ] = await Promise.allSettled([
          api.getReviewSettings(),
          api.listRoutes(),
          api.listReviewModelRoutes(),
          api.getRuntimeStatus(),
          api.listCatalogVersions(),
          api.getReviewStats(),
          api.getWebSearchSettings(),
          api.getSubagentSettings(),
          api.getSubagentCapability(),
          api.listModelRoutes(),
          api.getCodexOAuthStatus(),
        ]);
      if (!alive) return;
      if (nextSettings.status === "fulfilled") {
        reviewRef.current = nextSettings.value;
        reviewLastSavedRef.current = nextSettings.value;
        setSettings(nextSettings.value);
      }
      if (nextSubagent.status === "fulfilled") {
        subagentRef.current = nextSubagent.value;
        subagentLastSavedRef.current = nextSubagent.value;
        setSubagent(nextSubagent.value);
      }
      if (nextSubagentCapability.status === "fulfilled") {
        setSubagentCapability(nextSubagentCapability.value);
      }
      if (nextSubagentModels.status === "fulfilled") setSubagentModels(nextSubagentModels.value);
      // 帳號清單拿不到不算失敗：沒有 Official 帳號的安裝根本不會看到這個
      // 選單，所以它不進下面的 failures，也不該把整頁標成載入失敗。
      if (nextOauth.status === "fulfilled") setOauth(nextOauth.value);
      if (nextWebSearch.status === "fulfilled") {
        setWebSearch(nextWebSearch.value);
        setAllowDraft(formatDomainList(nextWebSearch.value.settings.domainPolicy.allow));
        setBlockDraft(formatDomainList(nextWebSearch.value.settings.domainPolicy.block));
      }
      if (nextModels.status === "fulfilled") setModels(nextModels.value);
      if (nextRuntime.status === "fulfilled") setRuntime(nextRuntime.value);
      if (nextVersions.status === "fulfilled") setVersions(nextVersions.value);
      if (nextStats.status === "fulfilled") setStats(nextStats.value);
      if (nextRoutes.status === "fulfilled") {
        setRoutes(nextRoutes.value);
        const saved = window.localStorage.getItem(DASHBOARD_KEY);
        let selected: string[] = [];
        if (saved) {
          try {
            selected = JSON.parse(saved) as string[];
          } catch {
            selected = [];
          }
        }
        const available = new Set(nextRoutes.value.map((route) => route.id));
        selected = selected.filter((id) => available.has(id));
        if (!selected.length) {
          selected = nextRoutes.value.filter((route) => route.enabled).map((route) => route.id);
        }
        setDashboardRouteIds(selected);
      }
      const failures = [
        nextSettings,
        nextRoutes,
        nextModels,
        nextRuntime,
        nextVersions,
        nextStats,
        nextWebSearch,
        nextSubagent,
        nextSubagentModels,
      ].filter((result): result is PromiseRejectedResult => result.status === "rejected");
      setError(
        failures.length
          ? t("settings.page.errors.partialRefresh", { detail: failures.map((failure) => String(failure.reason)).join(t("common.listSeparator")) })
          : null,
      );
      if (refreshVersion > 0) onRefreshComplete(refreshVersion);
    };
    void load();
    const stopPoll = startVisiblePoll({
      active,
      intervalMs: 60_000,
      load: () => {
        void api.getReviewStats().then((next) => alive && setStats(next)).catch(() => {});
      },
    });
    return () => {
      alive = false;
      stopPoll();
    };
  }, [refreshVersion, active]);

  /**
   * 存自動審查設定。
   *
   * 跟 updateSubagent 同一套 serialized save queue：連按兩下切換策略時，
   * 兩個後端請求還是會照發出順序落地，先發的不會在後發的存檔後才回來
   * 把新值蓋掉。generation 守衛另外處理 UI 顯示（哪次回應還算數），
   * 兩者是分開的問題 —— queue 保正確落盤順序，generation 保畫面不被
   * 過期回應覆蓋。失敗時回滾到「已知確實存進去的那份」—— 這個值必須在
   * catch 當下才讀 `reviewLastSavedRef.current`，不能在入列當下就先存
   * 一份快照：假設 A、B 依序入列，A 存檔成功、B 隨後失敗，B 的 catch
   * 若用的是入列當下（A 可能都還沒開始存）的舊快照，回滾會蓋掉 A 已經
   * 確實落盤的值，畫面跟後端狀態就不一致了。
   */
  async function updateReview(patch: Partial<ReviewSettings>) {
    const current = reviewRef.current;
    if (!current) return;
    const next = { ...current, ...patch };
    reviewRef.current = next;
    const generation = ++reviewGeneration.current;
    setSettings(next);
    setReviewSaving(true);
    setReviewNotice(null);
    setError(null);
    reviewSaveQueue.current = reviewSaveQueue.current
      .catch(() => undefined)
      .then(async () => {
        try {
          const result = await api.setReviewSettings(next);
          const saved = result.settings;
          reviewLastSavedRef.current = saved;
          if (generation === reviewGeneration.current) {
            reviewRef.current = saved;
            setSettings(saved);
            setReviewNotice(
              result.remoteHostsPendingReapply > 0
                ? t("settings.page.review.savedRemotePending", {
                    count: result.remoteHostsPendingReapply,
                  })
                : t("settings.page.saved"),
            );
          }
        } catch (cause) {
          const lastSaved = reviewLastSavedRef.current ?? current;
          if (generation === reviewGeneration.current) {
            reviewRef.current = lastSaved;
            setSettings(lastSaved);
            setError(t("settings.page.errors.reviewSave", { detail: String(cause) }));
          }
        } finally {
          if (generation === reviewGeneration.current) setReviewSaving(false);
        }
      });
  }

  async function updateSubagent(patch: Partial<SubagentSettings>) {
    const previous = subagentLastSavedRef.current;
    const current = subagentRef.current;
    if (!previous || !current) return;
    const next = { ...current, ...patch };
    subagentRef.current = next;
    const generation = ++subagentGeneration.current;
    setSubagent(next);
    setSubagentSaving(true);
    setSubagentNotice(null);
    setError(null);
    // Saves are serialized so rapid edits cannot persist out of order; the
    // generation guard then stops a superseded save from touching the UI. On
    // failure we roll back to the last value known to be persisted, never to
    // an optimistic edit that a newer queued save may already depend on.
    subagentSaveQueue.current = subagentSaveQueue.current
      .catch(() => undefined)
      .then(async () => {
        try {
          const saved = await api.setSubagentSettings(next);
          if (generation === subagentGeneration.current) {
            subagentLastSavedRef.current = saved;
            subagentRef.current = saved;
            setSubagent(saved);
            setSubagentNotice(t("settings.page.saved"));
          }
        } catch (cause) {
          if (generation === subagentGeneration.current) {
            subagentLastSavedRef.current = previous;
            subagentRef.current = previous;
            setSubagent(previous);
            setError(t("settings.page.errors.subagentSave", { detail: String(cause) }));
          }
        } finally {
          if (generation === subagentGeneration.current) setSubagentSaving(false);
        }
      });
  }

  /**
   * Switching to "custom" keeps the current Provider/model when it is still
   * selectable; otherwise it seeds the selects from the current enabled
   * Provider and its first catalog model. Switching back to "inherit" clears
   * everything so Vellum stops managing Codex's sub-agent defaults.
   */
  function selectSubagentMode(mode: SubagentMode) {
    if (mode === "custom" && subagentCapability && !subagentCapability.supported) {
      setError(
        t("settings.page.subagent.unsupported", {
          detail: subagentCapability.detail,
        }),
      );
      return;
    }
    if (mode === "inherit") {
      void updateSubagent({ mode, routeId: null, catalogId: null, reasoningEffort: null });
      return;
    }
    const currentSubagent = subagentRef.current;
    const keep = Boolean(
      currentSubagent?.routeId &&
        currentSubagent.catalogId &&
        routes.some((route) => route.id === currentSubagent.routeId && route.enabled) &&
        subagentModels.some(
          (model) =>
            model.routeId === currentSubagent.routeId &&
            model.catalogId === currentSubagent.catalogId,
        ),
    );
    if (keep) {
      void updateSubagent({ mode });
      return;
    }
    const preferred =
      routes.find((route) => route.enabled && route.isCurrent) ??
      routes.find((route) => route.enabled);
    const first = subagentModels.find((model) => model.routeId === preferred?.id);
    void updateSubagent({
      mode,
      routeId: preferred?.id ?? null,
      catalogId: first?.catalogId ?? null,
      reasoningEffort: null,
    });
  }

  /**
   * Provider change atomically selects that Provider's first catalog model and
   * resets the effort so a stale effort can never be paired with a new model.
   */
  function selectSubagentProvider(routeId: string) {
    const first = subagentModels.find((model) => model.routeId === routeId);
    void updateSubagent({
      routeId: routeId || null,
      catalogId: first?.catalogId ?? null,
      reasoningEffort: null,
    });
  }

  /**
   * Model change keeps the current effort only when the new model verifiably
   * supports it; otherwise the same save resets it to automatic.
   */
  function selectSubagentModel(catalogId: string) {
    const currentSubagent = subagentRef.current;
    if (!currentSubagent?.routeId) return;
    const model = subagentModels.find(
      (candidate) =>
        candidate.routeId === currentSubagent.routeId && candidate.catalogId === catalogId,
    );
    const effortUnsupported = Boolean(
      currentSubagent.reasoningEffort &&
        model &&
        !model.reasoningEfforts.includes(currentSubagent.reasoningEffort),
    );
    void updateSubagent({
      catalogId: catalogId || null,
      reasoningEffort: effortUnsupported ? null : currentSubagent.reasoningEffort,
    });
  }

  /**
   * 存網頁搜尋設定。
   *
   * 樂觀更新 + generation 守衛，跟 updateReview 同一套：連按兩下時，
   * 後到的回應不能覆蓋先按的那次。金鑰走 options，不混進 settings ——
   * 後端也不會把它序列化進設定檔。
   */
  async function saveWebSearch(
    next: WebSearchSettings,
    options: { braveApiKey?: string; clearBraveApiKey?: boolean } = {},
  ) {
    if (!webSearch) return;
    const previous = webSearch;
    const generation = ++webSearchGeneration.current;
    setWebSearch({ ...webSearch, settings: next });
    setWebSearchSaving(true);
    setWebSearchNotice(null);
    setError(null);
    try {
      const saved = await api.setWebSearchSettings(next, options);
      if (generation !== webSearchGeneration.current) return;
      setWebSearch(saved);
      setAllowDraft(formatDomainList(saved.settings.domainPolicy.allow));
      setBlockDraft(formatDomainList(saved.settings.domainPolicy.block));
      setBraveKeyDraft("");
      setWebSearchNotice(t("settings.page.saved"));
    } catch (cause) {
      if (generation !== webSearchGeneration.current) return;
      setWebSearch(previous);
      setError(t("settings.page.errors.webSearchSave", { detail: String(cause) }));
    } finally {
      if (generation === webSearchGeneration.current) setWebSearchSaving(false);
    }
  }

  /**
   * 連線測試。
   *
   * 一定要有「測過了」的回饋：結果數字可能剛好跟上次一樣，沒有時間戳的話
   * 使用者不知道到底測了沒。失敗也記時間 —— 記的是我們什麼時候問的。
   */
  async function runSearchProbe() {
    setSearchProbing(true);
    setSearchProbeError(null);
    try {
      const result = await api.probeWebSearch();
      setSearchProbe(result);
    } catch (cause) {
      setSearchProbe(null);
      setSearchProbeError(String(cause));
    } finally {
      setSearchProbedAt(Math.floor(Date.now() / 1000));
      setSearchProbing(false);
    }
  }

  /**
   * settings 目前存的 route/model 是否仍指向一個已啟用的審查模型 —— 不
   * 只看有沒有值，值可能是已刪除或已停用 Provider 留下的殘骸。
   */
  function hasValidReviewSelection(routeId: string, model: string): boolean {
    return models.some(
      (candidate) =>
        candidate.routeId === routeId &&
        enabledRouteIds.has(candidate.routeId) &&
        (candidate.upstreamModel === model || candidate.catalogId === model),
    );
  }

  /** 排除 `exclude` 那個 Provider 後，第一個已啟用的審查模型。 */
  function firstEnabledReviewModel(exclude?: string): ModelRoute | undefined {
    return models.find(
      (model) => enabledRouteIds.has(model.routeId) && model.routeId !== exclude,
    );
  }

  /**
   * 換策略。
   *
   * 切策略是一次完整的原子 transition，不能只送 `{ policy }`：後端存檔
   * 會驗證 route/model（`failover` 還要驗證 fallback），沒有有效歷史選擇
   * 時單送 policy 必然被拒，畫面又彈回原策略，使用者永遠點不進去那幾個
   * 選單。這裡先在本機決定一組合法的 primary（有效歷史選擇就直接沿用，
   * 沒有就挑第一個已啟用的審查模型）——`failover` 再多決定一個不同
   * Provider 的 fallback，兩者都找不到才顯示明確錯誤、留在原策略，不送出
   * 半套設定。
   *
   * 讀的是 `reviewRef.current`，不是 `settings` state：兩次點擊在同一個
   *事件批次內連續發生時，`setSettings` 還沒反映到這次 render，但
   * `updateReview` 已經同步把 `reviewRef.current` 更新過——跟
   * `selectSubagentMode` 讀 `subagentRef.current` 同一個理由，見上面。
   */
  async function changePolicy(next: ReviewPolicy) {
    const current = reviewRef.current;
    if (!current) return;
    let routeId = current.routeId;
    let model = current.model;
    if (!hasValidReviewSelection(routeId, model)) {
      const first = firstEnabledReviewModel();
      if (!first) {
        setError(t("settings.page.errors.reviewNoModelAvailable"));
        return;
      }
      routeId = first.routeId;
      model = first.upstreamModel;
    }
    if (next === "always") {
      await updateReview({ policy: next, routeId, model });
      return;
    }
    // failover: also needs a fallback on a different Provider than `routeId`
    // (primary may have just been re-picked above, so re-check against the
    // final routeId, not the stale `current.routeId`).
    const fallbackStillValid = Boolean(
      current.fallbackCatalogId &&
        models.some(
          (candidate) =>
            candidate.catalogId === current.fallbackCatalogId &&
            candidate.routeId !== routeId &&
            enabledRouteIds.has(candidate.routeId),
        ),
    );
    let fallbackCatalogId = current.fallbackCatalogId ?? null;
    if (!fallbackStillValid) {
      const fallback = firstEnabledReviewModel(routeId);
      if (!fallback) {
        setError(t("settings.page.errors.reviewNoFallbackAvailable"));
        return;
      }
      fallbackCatalogId = fallback.catalogId;
    }
    await updateReview({ policy: next, routeId, model, fallbackCatalogId });
  }

  /**
   * 換指定 Provider：跟 `changePolicy` 同一套原子規則 —— `failover` 下換
   * Provider 若讓現有 fallback 變成同一家，必須立刻挑一個新的、不同
   * Provider 的 fallback 頂上，不能只把 fallback 清空後送出半套設定
   * （後端會直接拒絕沒有 fallback 的 failover 存檔）。讀
   * `reviewRef.current`，理由同 `changePolicy`。
   */
  /**
   * 這條線路是不是走 OpenAI Official 平面。只有它才有「用哪個 ChatGPT
   * 帳號計費」這件事——第三方線路的計費身分是那條線路自己的憑證。
   */
  function isOfficialRoute(routeId: string) {
    return routes.some((route) => route.id === routeId && route.providerKind === "official");
  }

  function selectReviewProvider(routeId: string) {
    const current = reviewRef.current;
    if (!current) return;
    const first = models.find((model) => model.routeId === routeId);
    if (!first) return;
    const patch: Partial<ReviewSettings> = {
      routeId,
      model: first.upstreamModel,
      // 換到非 Official 就把計費帳號清掉。留著它不會有任何效果，卻會在
      // 使用者哪天換回 Official 時無聲地生效——那是他上一次選的，不是
      // 這一次選的。
      ...(isOfficialRoute(routeId) ? {} : { officialAccountId: null }),
    };
    if (needsFallback(policyOf(current))) {
      const fallbackStillValid = Boolean(
        current.fallbackCatalogId &&
          models.some(
            (candidate) =>
              candidate.catalogId === current.fallbackCatalogId &&
              candidate.routeId !== routeId &&
              enabledRouteIds.has(candidate.routeId),
          ),
      );
      if (!fallbackStillValid) {
        const fallback = firstEnabledReviewModel(routeId);
        if (!fallback) {
          setError(t("settings.page.errors.reviewNoFallbackAvailable"));
          return;
        }
        patch.fallbackCatalogId = fallback.catalogId;
      }
    }
    void updateReview(patch);
  }

  function toggleDashboardProvider(routeId: string) {
    setDashboardRouteIds((current) => {
      const next = current.includes(routeId)
        ? current.filter((id) => id !== routeId)
        : [...current, routeId];
      window.localStorage.setItem(DASHBOARD_KEY, JSON.stringify(next));
      return next;
    });
  }

  async function repairCodex() {
    setBusy(true);
    setError(null);
    try {
      const result = await api.repairCodexConfig();
      setRestoreResult(result);
      onChanged();
    } catch (cause) {
      setError(t("settings.page.errors.restore", { detail: String(cause) }));
    } finally {
      setBusy(false);
    }
  }

  async function toggleDrain() {
    setError(null);
    try {
      setRuntime(
        await (runtime?.draining ? api.cancelGracefulDrain() : api.beginGracefulDrain()),
      );
      onChanged();
    } catch (cause) {
      setError(t("settings.page.errors.drain", { detail: String(cause) }));
    }
  }

  /// Codex Desktop 有對話在跑時，重新啟動會被擋下來。擋下來的唯一出路是使用者
  /// 自己說「我知道，還是要重啟」——所以那顆按鈕只在被擋時出現。
  const restartBlockedByTurn = restart?.notice.code === "restartBlockedByCodexTurn";

  /// 這顆按鈕就是交接本身：寫 launch manifest、取得 CODEX_CLI_PATH 的 lease、
  /// 關掉並重開 Codex Desktop，然後最多等 90 秒看 bridge 有沒有回報 attestation。
  /// 中間畫面必須看得出來它在做事，否則使用者只會再按一次。
  async function restartCodex(force = false) {
    setCodexRestarting(true);
    setError(null);
    try {
      setRestart(await api.restartCodexSafely(force));
      setRuntime(await api.getRuntimeStatus());
      onChanged();
    } catch (cause) {
      setError(t("settings.page.errors.restart", { detail: String(cause) }));
    } finally {
      setCodexRestarting(false);
    }
  }

  async function rollbackCatalog(versionId: string) {
    setError(null);
    try {
      setRuntime(await api.rollbackCatalogVersion(versionId));
      setVersions(await api.listCatalogVersions());
      onChanged();
    } catch (cause) {
      setError(t("settings.page.errors.rollback", { detail: String(cause) }));
    }
  }

  async function exportVellumLogs() {
    setLogExporting(true);
    setLogExportPath(null);
    setError(null);
    try {
      setLogExportPath(await api.exportVellumLogs());
    } catch (cause) {
      setError(t("settings.page.errors.logExport", { detail: String(cause) }));
    } finally {
      setLogExporting(false);
    }
  }

  if (!settings) {
  return (
      <>
        <div className="canvas__head">
          <h2 className="canvas__title">{t("settings.title")}</h2>
        </div>
        <Card>
          <Empty>{error ?? t("settings.page.loading")}</Empty>
        </Card>
      </>
    );
  }

  // Proxy 起來了但 Enhanced 沒武裝。這條通知帶著真正的原因（例如 bridge
  // 雜湊對不上），比任何我們自己組的句子都準確，所以它優先。
  const liveApplied = (runtime?.liveApplied ?? []).filter(
    (item) => item.code !== "enhancedDesktopRuntimeNotArmed",
  );
  const policy = policyOf(settings);
  const share = fallbackShare(stats);
  const enabledRouteIds = new Set(routes.filter((route) => route.enabled).map((route) => route.id));
  const rows = statsToShow(stats, [settings.routeId, fallbackRouteId(models, settings)]);
  /* 只有 failover 才可能有備援接手；其他策略下統計裡的舊值不代表現在。 */
  const showsFallback = needsFallback(policy) && (stats?.activeIsFallback ?? false);
  /* 實際在服務的那家。 */
  const activeProvider =
    routes.find((route) => route.id === stats?.activeRouteId)?.name ??
    routes.find((route) => route.id === settings.routeId)?.name ??
    null;

  /* 網頁搜尋。「兩個關」在 lib/webSearch.ts 收斂，畫面只讀結論 —— 那件事有
     測試把 Rust 端的規則鎖住。Brave 是唯一的後端，開著沒金鑰就是還沒設定完。 */
  const searchSettings = webSearch?.settings ?? null;
  const hasBraveKey = webSearch?.hasBraveApiKey ?? false;
  const searchOn = searchSettings ? webSearchIsOn(searchSettings) : false;
  const searchNeedsBraveKey = searchSettings ? needsBraveKey(searchSettings, hasBraveKey) : false;
  const searchReach = searchSettings ? reachOf(searchSettings) : "live";
  const domainsDirty = searchSettings
    ? parseDomainList(allowDraft).join("\n") !== searchSettings.domainPolicy.allow.join("\n") ||
      parseDomainList(blockDraft).join("\n") !== searchSettings.domainPolicy.block.join("\n")
    : false;
  const subagentSelectedModel = subagent
    ? subagentModels.find(
        (model) =>
          model.routeId === subagent.routeId && model.catalogId === subagent.catalogId,
      )
    : undefined;
  /* 探測狀態要跟 subagentSelectedModel（ModelRoute）分開查 —— ModelRoute 本身
     不帶 effortProbeStatus，那個欄位只在 Route.modelCapabilities 上。少了這
     一步，「還沒探測過」跟「探測完、確認這個供應商本來就沒有分級」會被畫成
     同一句「尚未驗證」，跟模型頁的四態顯示（見 Models.tsx）互相矛盾。 */
  const subagentCapabilityEntry = subagentSelectedModel
    ? routes
        .find((route) => route.id === subagentSelectedModel.routeId)
        ?.modelCapabilities.find(
          (capability) => capability.model.toLowerCase() === subagentSelectedModel.upstreamModel.toLowerCase(),
        )
    : undefined;
  const subagentEffortStatus = subagentCapabilityEntry?.effortProbeStatus ?? "not_probed";
  const subagentEffortOptions = subagentSelectedModel?.reasoningEfforts ?? [];
  const subagentEffortDisplay =
    subagentEffortStatus === "supported" && subagentEffortOptions.length
      ? null
      : subagentEffortStatus === "indeterminate"
        ? t("models.ui.modelCatalog.effortUnverified")
        : subagentEffortStatus === "not_probed"
          ? t("models.ui.modelCatalog.effortNotProbed")
          : t("models.ui.modelCatalog.auto");
  const subagentUnavailable = Boolean(
    subagent?.mode === "custom" &&
      subagent.routeId &&
      (!routes.some((route) => route.id === subagent.routeId && route.enabled) ||
        (subagent.catalogId && !subagentSelectedModel)),
  );

  return (
    <>
      <div className="canvas__head">
        <div>
          <p className="eyebrow">{t("settings.title")}</p>
          <h2 className="canvas__title">{t("settings.page.heading")}</h2>
        </div>
      </div>

      {error ? <p className="note">{error}</p> : null}

      {/* 語言是頁面級偏好，只有一個下拉。放進 grid2 會讓它吃掉 1.35fr 的主欄，
          把真正的主角（自動審查）擠進窄欄；份量要對得起內容。收成一條橫幅。 */}
      {I18N_LANGUAGE_SELECTOR_ENABLED ? (
        <Card quiet className="prefstrip">
          <div className="prefstrip__copy">
            <Cap>{t("settings.language.title")}</Cap>
            <p className="note">{t("settings.language.blurb")}</p>
          </div>
          <select
            className="input"
            aria-label={t("settings.language.title")}
            value={localePreference}
            onChange={(event) => {
              const next = event.target.value as LocalePreference;
              setLocalePreferenceState(next);
              void setLocalePreference(next);
            }}
          >
            <option value="system">{t("settings.language.option.system")}</option>
            <option value="zh-TW">{t("settings.language.option.zh-TW")}</option>
            <option value="zh-CN">{t("settings.language.option.zh-CN")}</option>
            <option value="en">{t("settings.language.option.en")}</option>
            <option value="ja">{t("settings.language.option.ja")}</option>
          </select>
        </Card>
      ) : null}

      <div className="grid2">
        <Card>
          <div className="rowline">
            <Cap>{t("settings.page.review.title")}</Cap>
            <Toggle
              label={t("settings.page.review.toggle")}
              checked={settings.beforeSend}
              onChange={(value) => void updateReview({ beforeSend: value })}
            />
          </div>
          <p className="prose" style={{ marginTop: 4 }}>
            {t("settings.page.review.description")}
          </p>

          {settings.beforeSend ? (
            <>
              <div style={{ marginTop: 18 }}>
                <Cap>{t("settings.page.review.strategyTitle")}</Cap>
                <Segment
                  options={REVIEW_POLICIES.map((p) => ({ value: p.value, label: t(p.labelKey) }))}
                  value={policy}
                  onChange={(next) => void changePolicy(next)}
                />
                {/* 一行講完後果。選項只有幾個字，塞不下條件與結果。 */}
                <p className="note" style={{ marginTop: 12 }}>
                  {t(policyMeta(policy).blurbKey)}
                </p>
              </div>

              <Rows>
                {/* 實際生效的那個模型可能不是指定的那個。這是設定頁上唯一
                    「現在怎樣」的事實，所以排在所有選項前面。 */}
                <Row label={t("settings.page.review.currentLabel")}>
                  <span className="rowline">
                    {/* 備援接手時，只寫模型名會少掉「是誰接手的」——
                        下面那行講的是失敗的那家，不是正在服務的那家。 */}
                    {activeProvider ? (
                      <span>{activeProvider}</span>
                    ) : null}
                    <span className="literal">
                      {(stats?.activeModel ?? settings.model) || t("settings.page.unspecified")}
                    </span>
                    {showsFallback ? <State tone="warn" label={t("settings.page.review.fallbackActive")} /> : null}
                  </span>
                  {showsFallback ? (
                    <span className="rows__hint" style={{ display: "block" }}>
                      {stats?.activeReason ?? t("settings.page.review.selectedUnavailable")}
                    </span>
                  ) : null}
                </Row>

                <Row label={t("settings.page.review.provider")}>
                  <select
                    className="input"
                    value={settings.routeId}
                    onChange={(event) => selectReviewProvider(event.target.value)}
                  >
                    <option value="">{t("settings.page.unspecified")}</option>
                    {routes
                      .filter((route) => route.enabled)
                      .map((route) => (
                        <option key={route.id} value={route.id}>{route.name}</option>
                      ))}
                  </select>
                </Row>
                <Row label={t("settings.page.review.model")}>
                  <select
                    className="input"
                    value={settings.model}
                    onChange={(event) => void updateReview({ model: event.target.value })}
                    disabled={!settings.routeId}
                  >
                    {!settings.routeId ? <option value="">{t("settings.page.unspecified")}</option> : null}
                    {models
                      .filter((model) => model.routeId === settings.routeId)
                      .map((model) => (
                        <option key={model.catalogId} value={model.upstreamModel}>
                          {model.upstreamModel}
                        </option>
                      ))}
                  </select>
                </Row>
                {/* 只有 Official 才問這件事。第三方線路的帳單跟著那條線路
                    自己的憑證走，這裡給選單只會讓人以為它有作用。 */}
                {isOfficialRoute(settings.routeId) ? (
                  <Row label={t("settings.page.review.billingAccount")}>
                    <select
                      className="input"
                      value={settings.officialAccountId ?? ""}
                      onChange={(event) =>
                        void updateReview({ officialAccountId: event.target.value || null })
                      }
                    >
                      <option value="">{t("settings.page.review.billingFollowsDefault")}</option>
                      {(oauth?.accounts ?? []).map((account) => (
                        <option key={account.accountId} value={account.accountId}>
                          {account.email ?? account.accountId}
                        </option>
                      ))}
                    </select>
                    {/* 指定的帳號不在清單裡（登出、移除、或設定是從別台同步
                        來的）。無聲地掉回預設帳號等於拿錯的帳單，所以這裡
                        講出來——後端也會擋，不會真的用預設帳號跑。 */}
                    {settings.officialAccountId &&
                    !(oauth?.accounts ?? []).some(
                      (account) => account.accountId === settings.officialAccountId,
                    ) ? (
                      <span className="rows__hint" style={{ display: "block" }}>
                        {t("settings.page.review.billingAccountMissing", {
                          account: settings.officialAccountId,
                        })}
                      </span>
                    ) : null}
                  </Row>
                ) : null}
                {needsFallback(policy) ? (
                  <Row label={t("settings.page.review.fallbackModel")}>
                    <select
                      className="input"
                      value={settings.fallbackCatalogId ?? ""}
                      onChange={(event) =>
                        void updateReview({ fallbackCatalogId: event.target.value || null })
                      }
                    >
                      <option value="">{t("settings.page.unspecified")}</option>
                      {/* 備援不能與指定的是同一家：該 Provider 額度用盡時頂不了自己。 */}
                      {models
                        .filter(
                          (model) =>
                            model.routeId !== settings.routeId &&
                            enabledRouteIds.has(model.routeId),
                        )
                        .map((model) => (
                          <option key={model.catalogId} value={model.catalogId}>
                            {model.displayName}
                          </option>
                        ))}
                    </select>
                  </Row>
                ) : null}
              </Rows>
            </>
          ) : null}

          <p className="note" style={{ marginTop: 12 }}>
            {reviewSaving ? t("common.processing") : reviewNotice ?? ""}
          </p>
        </Card>

        <Card quiet>
          <Cap>{t("settings.page.review.rulesTitle")}</Cap>
          <Rows>
            <Row label={t("settings.page.review.willReview")}>{t("settings.page.review.willReviewValue")}</Row>
            <Row label={t("settings.page.review.willNotReview")}>{t("settings.page.review.willNotReviewValue")}</Row>
            <Row label={t("settings.page.review.whenRejected")}>{t("settings.page.review.whenRejectedValue")}</Row>
          </Rows>
        </Card>
      </div>

      {/* 子代理跟審查兩者的資料形狀差太多，硬塞進同一個 grid2 只會讓
          第三張卡落到寬欄那一格、窄欄整格留白（grid2 是特意設計給兩張
          卡片配對的 1.35fr／1fr，不是拿來排三張的；見 base.css 裡的
          說明）。獨立成一張全寬卡，跟頁面上其它設定卡片一致。 */}
      {subagent ? (
          <Card>
            <div className="rowline">
              <Cap>{t("settings.page.subagent.title")}</Cap>
              <Segment
                options={[
                  { value: "inherit", label: t("settings.page.subagent.mode.inherit") },
                  { value: "custom", label: t("settings.page.subagent.mode.custom") },
                ]}
                value={subagent.mode}
                onChange={(next) => void selectSubagentMode(next)}
              />
            </div>
            <p className="prose" style={{ marginTop: 4 }}>
              {t("settings.page.subagent.description")}
            </p>
            {subagentCapability ? (
              <div
                className={`subagent-capability subagent-capability--${
                  subagentCapability.supported ? "ready" : "blocked"
                }`}
              >
                <span className="subagent-capability__state">
                  {subagentCapability.supported
                    ? t("settings.page.subagent.desktopReady")
                    : t("settings.page.subagent.desktopUnavailable")}
                </span>
                <span className="subagent-capability__detail">
                  {subagentCapability.supported
                    ? t("settings.page.subagent.desktopVersion", {
                        version:
                          subagentCapability.desktopVersion ?? t("common.unknown"),
                      })
                    : t("settings.page.subagent.unsupported", {
                        detail: subagentCapability.detail,
                      })}
                </span>
              </div>
            ) : null}

            {subagent.mode === "custom" ? (
              <>
                <div style={{ marginTop: 18 }}>
                  <Cap>{t("settings.page.subagent.defaultsTitle")}</Cap>
                  <Rows>
                    <Row label={t("common.provider")}>
                      <select
                        className="input"
                        value={subagent.routeId ?? ""}
                        disabled={subagentSaving || subagentCapability?.supported === false}
                        onChange={(event) => selectSubagentProvider(event.target.value)}
                      >
                        {/* 只列出至少有一個模型可當子代理預設的 Provider —— 選了一個空的
                            供應商只會落到下面的模型選單顯示「這個供應商沒有可用模型」，
                            不如一開始就不給選。目前選定的那個例外留著，不然設定值會
                            在畫面上憑空消失。 */}
                        {routes
                          .filter(
                            (route) =>
                              route.enabled &&
                              (route.id === subagent.routeId ||
                                subagentModels.some((model) => model.routeId === route.id)),
                          )
                          .map((route) => (
                            <option key={route.id} value={route.id}>{route.name}</option>
                          ))}
                      </select>
                    </Row>
                    <Row label={t("common.model")}>
                      <select
                        className="input"
                        value={subagent.catalogId ?? ""}
                        disabled={
                          subagentSaving ||
                          subagentCapability?.supported === false ||
                          !subagent.routeId
                        }
                        onChange={(event) => selectSubagentModel(event.target.value)}
                      >
                        {!subagent.routeId ? (
                          <option value="">{t("settings.page.unspecified")}</option>
                        ) : null}
                        {subagentModels
                          .filter((model) => model.routeId === subagent.routeId)
                          .map((model) => (
                            <option key={model.catalogId} value={model.catalogId}>
                              {model.displayName || model.upstreamModel}
                            </option>
                          ))}
                      </select>
                      {subagent.routeId &&
                      !subagentModels.some((model) => model.routeId === subagent.routeId) ? (
                        <span className="rows__hint">
                          {t("settings.page.subagent.modelEmpty")}
                        </span>
                      ) : null}
                    </Row>
                    <Row label={t("settings.page.subagent.effort")}>
                      <select
                        className="input"
                        value={subagent.reasoningEffort ?? ""}
                        disabled={
                          subagentSaving ||
                          subagentCapability?.supported === false ||
                          !subagent.catalogId
                        }
                        onChange={(event) =>
                          void updateSubagent({ reasoningEffort: event.target.value || null })
                        }
                      >
                        <option value="">{t("settings.page.subagent.autoEffort")}</option>
                        {subagentEffortOptions.map((effort) => (
                          <option key={effort} value={effort}>{effort}</option>
                        ))}
                      </select>
                      {subagent.catalogId && subagentEffortDisplay ? (
                        <span className="rows__hint">
                          {subagentEffortDisplay}
                        </span>
                      ) : null}
                    </Row>
                  </Rows>
                  <p className="note" style={{ marginTop: 12 }}>
                    {t("settings.page.subagent.hint")}
                  </p>
                  {subagentUnavailable ? (
                    <p className="note" style={{ marginTop: 8 }}>
                      {t("settings.page.subagent.unavailable")}
                    </p>
                  ) : null}
                </div>
              </>
            ) : null}
            <p className="note" style={{ marginTop: 12 }}>
              {subagentSaving ? t("common.processing") : subagentNotice ?? ""}
            </p>
          </Card>
        ) : null}

      {/* 比例放標題列當事實陳述，不做成大數字加一段解讀 ——
          解讀該由使用者自己下，介面給數字就好。

          只要自動審查是開的就顯示這張卡，即使一次都還沒跑過。原本用
          `totalRuns > 0` 當顯示條件，結果剛接線、還沒累積資料時整張卡消失，
          看起來像功能不見了 —— 而「設定生效但還沒出手」正是這時候最該確認的事。 */}
      {settings.beforeSend ? (
        <Card>
          <div className="rowline">
            <Cap>{t("settings.page.stats.title")}</Cap>
            {stats && stats.totalRuns > 0 ? (
              <span className="rows__hint">
                {t("settings.page.stats.summary", { fallback: stats.fallbackRuns, total: stats.totalRuns, percent: share })}
              </span>
            ) : null}
          </div>

          {rows.length ? (
            /* 表頭一次講完欄位，不在每一格重複標籤。 */
            <table className="revstat" style={{ marginTop: 8 }}>
              <colgroup>
                <col />
                <col className="revstat__col-count" />
                <col className="revstat__col-count" />
                <col className="revstat__col-count" />
                <col className="revstat__col-time" />
              </colgroup>
              <thead>
                <tr>
                  <th scope="col">{t("common.provider")}</th>
                  <th scope="col">{t("settings.page.stats.primary")}</th>
                  <th scope="col">{t("settings.page.stats.fallback")}</th>
                  <th scope="col">{t("settings.page.stats.failed")}</th>
                  <th scope="col">{t("settings.page.stats.lastUsed")}</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((p) => (
                  <tr key={`${p.routeId}:${p.model}`}>
                    <td>
                      <span className="revstat__who">
                        <span className="revstat__name">{p.provider}</span>
                        <code className="revstat__model">{p.model}</code>
                      </span>
                    </td>
                    <td className="revstat__n">{p.primaryRuns}</td>
                    <td className="revstat__n">{p.fallbackRuns}</td>
                    <td
                      className={`revstat__n${p.failedRuns > 0 ? " revstat__n--bad" : ""}`}
                    >
                      {p.failedRuns}
                    </td>
                    <td className="revstat__when">
                      {sinceLabel(p.lastUsedAt, t("settings.page.stats.neverUsed"))}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          ) : (
            <Empty>
              {t("settings.page.stats.empty")}
            </Empty>
          )}
        </Card>
      ) : null}

      {/* 網頁搜尋。開關立即生效 —— 工具是逐次請求從 Codex 的 tools 裡拿掉／留下的，
          引擎在設定變了之後自己重建，所以這裡不需要任何「重啟才生效」的提示。

          排在壓縮策略之後、顯示偏好之前：這三張講的都是「送出去的請求長什麼樣」。 */}
      {searchSettings ? (
        <Card>
          <div className="rowline">
            <Cap>{t("settings.page.webSearch.title")}</Cap>
            <Toggle
              label={t("settings.page.webSearch.toggle")}
              checked={searchOn}
              onChange={(value) =>
                void saveWebSearch(value ? turnOn(searchSettings) : turnOff(searchSettings))
              }
            />
          </div>
          <p className="prose" style={{ marginTop: 4 }}>
            {t("settings.page.webSearch.description")}
          </p>

          {webSearch?.migrationNotice ? (
            <p className="note" style={{ marginTop: 8 }}>
              <State tone="warn" label={t("common.notice.warn")} />{" "}
              {noticeText(webSearch.migrationNotice, t)}
            </p>
          ) : null}

          {searchOn ? (
            <>
              <Rows>
                {/* Brave 是唯一的後端；開著沒有金鑰就是還沒設定完，查詢一律失敗。
                    狀態燈跟輸入框放在同一列，不裝飾成另一個不相干的事實。 */}
                <Row label={t("settings.page.webSearch.braveKey")}>
                  <span className="keyfield">
                    <input
                      className="input"
                      type="password"
                      autoComplete="off"
                      value={braveKeyDraft}
                      placeholder={
                        hasBraveKey
                          ? t("settings.page.webSearch.braveKeySaved")
                          : t("settings.page.webSearch.braveKeyPlaceholder")
                      }
                      onChange={(event) => setBraveKeyDraft(event.target.value)}
                    />
                    <Btn
                      mini
                      soft
                      disabled={!braveKeyDraft.trim() || webSearchSaving}
                      onClick={() =>
                        void saveWebSearch(searchSettings, {
                          braveApiKey: braveKeyDraft.trim(),
                        })
                      }
                    >
                      {t("common.save")}
                    </Btn>
                    {hasBraveKey ? (
                      <Btn
                        mini
                        soft
                        disabled={webSearchSaving}
                        onClick={() =>
                          void saveWebSearch(searchSettings, { clearBraveApiKey: true })
                        }
                      >
                        {t("common.remove")}
                      </Btn>
                    ) : null}
                    {searchNeedsBraveKey ? (
                      <State tone="warn" label={t("status.pending")} />
                    ) : (
                      <State tone="ok" label={t("status.live")} />
                    )}
                  </span>
                  <span className="rows__hint" style={{ display: "block" }}>
                    {searchNeedsBraveKey
                      ? t("settings.page.webSearch.needsBraveKey")
                      : t("settings.page.webSearch.braveKeyHint")}
                  </span>
                </Row>

                <Row label={t("settings.page.webSearch.reach")}>
                  <Segment
                    options={SEARCH_REACHES.map((reach) => ({
                      value: reach.value,
                      label: t(reach.labelKey),
                    }))}
                    value={searchReach}
                    onChange={(next) => void saveWebSearch(setReach(searchSettings, next))}
                  />
                  <span className="rows__hint" style={{ display: "block" }}>
                    {t(`settings.page.webSearch.reachHint.${searchReach}`)}
                  </span>
                </Row>

                <Row label={t("settings.page.webSearch.probe")}>
                  <span className="runtime-control">
                    <span className="runtime-control__copy rows__hint">
                      {searchProbing
                        ? t("settings.page.webSearch.probing")
                        : searchProbeError
                          ? t("settings.page.webSearch.probeFailed", { detail: searchProbeError })
                          : searchProbe
                            ? t(
                                searchProbe.resultCount > 0
                                  ? "settings.page.webSearch.probeOk"
                                  : "settings.page.webSearch.probeEmpty",
                                { count: searchProbe.resultCount },
                              )
                            : t("settings.page.webSearch.probeHint")}
                      {searchProbedAt && !searchProbing ? (
                        <span className="literal" style={{ marginInlineStart: 8 }}>
                          {t("settings.page.webSearch.probedAt", {
                            when: sinceLabel(searchProbedAt, t("common.justNow")),
                          })}
                        </span>
                      ) : null}
                    </span>
                    <Btn soft disabled={searchProbing || webSearchSaving} onClick={() => void runSearchProbe()}>
                      {searchProbing
                        ? t("common.processing")
                        : t("settings.page.webSearch.probeAction")}
                    </Btn>
                  </span>
                </Row>
              </Rows>

              {/* 網域限制是裝一次的東西，數量不固定 —— 收進托盤，收起來時顯示條數。 */}
              <div style={{ marginTop: 16 }}>
                <Tray
                  label={t("settings.page.webSearch.domainTray")}
                  count={domainRuleCount(searchSettings)}
                >
                  <div className="field" style={{ marginTop: 12 }}>
                    <label className="field__label" htmlFor="websearch-allow">
                      {t("settings.page.webSearch.domainAllow")}
                    </label>
                    <textarea
                      id="websearch-allow"
                      className="input"
                      rows={3}
                      value={allowDraft}
                      onChange={(event) => setAllowDraft(event.target.value)}
                    />
                  </div>
                  <div className="field" style={{ marginTop: 12 }}>
                    <label className="field__label" htmlFor="websearch-block">
                      {t("settings.page.webSearch.domainBlock")}
                    </label>
                    <textarea
                      id="websearch-block"
                      className="input"
                      rows={3}
                      value={blockDraft}
                      onChange={(event) => setBlockDraft(event.target.value)}
                    />
                  </div>
                  <p className="field__hint" style={{ marginTop: 10 }}>
                    {t("settings.page.webSearch.domainHint")}
                  </p>
                  <div className="rowline" style={{ marginTop: 12 }}>
                    <Btn
                      soft
                      disabled={!domainsDirty || webSearchSaving}
                      onClick={() =>
                        void saveWebSearch({
                          ...searchSettings,
                          domainPolicy: {
                            allow: parseDomainList(allowDraft),
                            block: parseDomainList(blockDraft),
                          },
                        })
                      }
                    >
                      {t("common.save")}
                    </Btn>
                  </div>
                </Tray>
              </div>
            </>
          ) : null}

          <p className="note" style={{ marginTop: 12 }}>
            {webSearchSaving ? t("common.processing") : webSearchNotice ?? ""}
          </p>
        </Card>
      ) : null}

      <Card>
        <Cap>{t("settings.page.dashboard.title")}</Cap>
        <p className="note" style={{ marginTop: 10 }}>
          {t("settings.page.dashboard.note", { count: dashboardRouteIds.length })}
        </p>
        <Rows>
          {routes.map((route) => (
            <Row key={route.id} label={route.name}>
              <label className="rowline">
                <input
                  type="checkbox"
                  checked={dashboardRouteIds.includes(route.id)}
                  onChange={() => toggleDashboardProvider(route.id)}
                />
                <span>{route.enabled ? t("common.visible") : t("settings.page.dashboard.disabledVisible")}</span>
              </label>
            </Row>
          ))}
        </Rows>
      </Card>

      <Card>
        <Cap>{t("settings.page.restore.title")}</Cap>
        <p className="prose" style={{ marginTop: 10 }}>
          {t("settings.page.restore.description")}
        </p>
        <div className="rowline" style={{ marginTop: 16 }}>
          <Btn soft onClick={() => void repairCodex()} disabled={busy}>
            {busy ? t("common.processing") : t("settings.page.restore.action")}
          </Btn>
        </div>
        {restoreResult ? (
          <>
            <Rows>
              {restoreResult.cleared.length ? (
                restoreResult.cleared.map((item) => (
                  <Row key={item} label={t("settings.page.restore.cleared")}>{item}</Row>
                ))
              ) : (
                <Row label={t("settings.page.restore.result")}>{t("settings.page.restore.nothingToClear")}</Row>
              )}
            </Rows>
            <p className="note" style={{ marginTop: 12 }}>
              {t("settings.page.restore.preserved", { items: restoreResult.preserved.join(t("common.itemSeparator")) })}
            </p>
          </>
        ) : null}
      </Card>


      <Card quiet>
        <Cap>{t("settings.page.advanced.title")}</Cap>
        <p className="note" style={{ marginTop: 10 }}>
          {t("settings.page.advanced.description")}
        </p>
        <Rows>
          <Row label={t("settings.page.advanced.activeRequests")}>{runtime?.activeRequests ?? "—"}</Row>
          <Row label={t("settings.page.advanced.drainTitle")}>
            <span className="runtime-control">
              <span className="runtime-control__copy">
                <span>
                {runtime?.draining
                  ? t("settings.page.advanced.draining")
                  : t("settings.page.advanced.accepting")}
                </span>
                <span className="rows__hint">
                  {t("settings.page.advanced.drainHint")}
                </span>
              </span>
              <Btn
                soft
                onClick={() => void toggleDrain()}
              >
                {runtime?.draining ? t("settings.page.advanced.resume") : t("settings.page.advanced.stop")}
              </Btn>
            </span>
          </Row>
          <Row label={t("settings.page.advanced.catalogVersion")}>{runtime?.activeCatalogVersion ?? t("settings.page.advanced.notCreated")}</Row>
          <Row label={t("settings.page.advanced.restartTitle")}>
            <span className="runtime-control">
              <span className="runtime-control__copy rows__hint">
                {restart ? noticeText(restart.notice, t) : t("settings.page.advanced.restartHint")}
              </span>
              <Btn
                soft
                onClick={() => void restartCodex()}
              >
                {t("settings.page.advanced.restartAction")}
              </Btn>
              {restartBlockedByTurn ? (
                <Btn soft disabled={codexRestarting} onClick={() => void restartCodex(true)}>
                  {t("settings.page.advanced.restartAnyway")}
                </Btn>
              ) : null}
            </span>
          </Row>
          <Row label={t("settings.page.advanced.guideTitle")}>
            <span className="runtime-control">
              <span className="runtime-control__copy rows__hint">
                {t("settings.page.advanced.guideHint")}
              </span>
              <Btn soft onClick={onOpenOnboarding}>
                {t("settings.page.advanced.guideAction")}
              </Btn>
            </span>
          </Row>
        </Rows>

        <div style={{ marginTop: 18 }}>
          <Cap>{t("settings.page.advanced.catalogHistory")}</Cap>
          {versions.length ? (
            <Rows>
              {versions.map((version) => (
                <Row key={version.id} label={version.id}>
                  <span className="rowline">
                    <span className="rows__hint">
                      {new Date(version.createdAt * 1000).toLocaleString()}
                    </span>
                    <Btn
                      soft
                      onClick={() => void rollbackCatalog(version.id)}
                    >
                      {t("settings.page.advanced.rollback")}
                    </Btn>
                  </span>
                </Row>
              ))}
            </Rows>
          ) : (
            <Empty>{t("settings.page.advanced.noVersions")}</Empty>
          )}
        </div>

        {runtime?.restartRequired && runtime.restartReasons.length ? (
          <Rows>
            {runtime.restartReasons.map((reason) => (
              <Row key={reason.code} label={t("settings.page.advanced.restartRequired")}>
                {noticeText(reason, t)}
              </Row>
            ))}
          </Rows>
        ) : null}
        {/* Enhanced 沒武裝那一條不列在這裡：這一組的標籤是「目前已生效」，
            而它講的正好是沒有生效。它在 Enhanced Runtime 那張卡上、就在
            開關旁邊，那裡才是它在解釋的東西。 */}
        {liveApplied.length ? (
          <Rows>
            {liveApplied.map((item) => (
              <Row key={item.code} label={t("settings.page.advanced.applied")}>
                {noticeText(item, t)}
              </Row>
            ))}
          </Rows>
        ) : null}
      </Card>

      <Card quiet>
        <Cap>{t("settings.page.logs.title")}</Cap>
        <p className="note" style={{ marginTop: 10 }}>
          {t("settings.page.logs.description")}
        </p>
        <div className="rowline" style={{ marginTop: 16 }}>
          <Btn soft disabled={logExporting} onClick={() => void exportVellumLogs()}>
            {logExporting ? t("settings.page.logs.exporting") : t("settings.page.logs.export")}
          </Btn>
          <span className="rows__hint">
            {logExportPath
              ? t("settings.page.logs.exported", { path: logExportPath })
              : t("settings.page.logs.hint")}
          </span>
        </div>
      </Card>

      <Card quiet>
        <Cap>{t("settings.page.app.title")}</Cap>
        <div className="rowline" style={{ marginTop: 6 }}>
          <Btn soft onClick={() => void api.exitVellum()}>{t("settings.page.app.exit")}</Btn>
          <span className="rows__hint">
            {t("settings.page.app.exitHint")}
          </span>
        </div>
      </Card>
    </>
  );
}




/** 備援模型屬於哪一家 —— 統計表要把它列出來，就算它一次都沒出手過。 */
function fallbackRouteId(models: ModelRoute[], settings: ReviewSettings): string | null {
  if (!settings.fallbackCatalogId) return null;
  return (
    models.find((model) => model.catalogId === settings.fallbackCatalogId)?.routeId ?? null
  );
}
