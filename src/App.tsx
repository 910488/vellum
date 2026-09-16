import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, recordInvokeResult } from "@/lib/api";
import {
  buildOnboardingConnections,
  codexCompatibleProbeModels,
  markOnboardingCompleted,
  shouldShowOnboarding,
} from "@/lib/onboarding";
import { EMPTY_STATUS, attention, type SystemStatus } from "@/lib/status";
import { Btn, Notice } from "@/components/ui";
import { Rail } from "@/components/Rail";
import { StatusBar } from "@/components/StatusBar";
import { Context } from "@/screens/Context";
import { Log } from "@/screens/Log";
import { Models } from "@/screens/Models";
import { Remote } from "@/screens/Remote";
import {
  Onboarding,
  type ConnectKind,
  type CustomEndpointDraft,
  type OnboardingConnection,
} from "@/screens/Onboarding";
import { Settings } from "@/screens/Settings";
import { Today } from "@/screens/Today";
import { EnhancedCore } from "@/screens/EnhancedCore";
import { SCREENS, type ScreenId } from "@/screens/registry";
import { noticeText } from "@/lib/notice";
import { shouldApplyProxyLifecycle } from "@/lib/proxyLifecycle";
import type { ProxyStatus } from "@/types";

function emptyOnboardingConnections(): Record<ConnectKind, OnboardingConnection> {
  return {
    chatgpt: { accounts: [], pending: null },
    grok: { accounts: [], pending: null },
    custom: { accounts: [], pending: null },
  };
}

const MAX_WARM_SCREENS = 3;

/**
 * 系統狀態集中在這裡抓一次，往下傳。
 *
 * 以前現況頁自己抓 proxy 狀態、執行狀態頁自己抓一份、導覽軌收到寫死的
 * 埠號與寫死的紅點數字 —— 同一個事實有四份來源，就會有四種說法。
 */
export function App() {
  const { t } = useTranslation();
  const [screen, setScreen] = useState<ScreenId>("today");
  const [mountedScreens, setMountedScreens] = useState<Set<ScreenId>>(
    () => new Set<ScreenId>(["today"]),
  );
  const [dismissedAlerts, setDismissedAlerts] = useState<string[]>([]);
  const [status, setStatus] = useState<SystemStatus>(EMPTY_STATUS);
  const [refreshing, setRefreshing] = useState(false);
  const [refreshError, setRefreshError] = useState<string | null>(null);
  const [refreshVersions, setRefreshVersions] = useState<Partial<Record<ScreenId, number>>>({});
  const refreshSequence = useRef(0);
  const statusLoadGeneration = useRef(0);
  const refreshInFlight = useRef<Promise<void> | null>(null);
  const pageRefreshWaiter = useRef<{ version: number; resolve: () => void } | null>(null);
  const [onboardingComplete, setOnboardingComplete] = useState(
    () => !shouldShowOnboarding(window.localStorage),
  );
  const [onboardingStep, setOnboardingStep] = useState(0);
  const [onboardingConnections, setOnboardingConnections] = useState(emptyOnboardingConnections);
  const [onboardingBusy, setOnboardingBusy] = useState<ConnectKind | null>(null);
  const [onboardingError, setOnboardingError] = useState<string | null>(null);
  const [onboardingProxyBusy, setOnboardingProxyBusy] = useState(false);
  const oauthPollGeneration = useRef(0);
  const grokPollGeneration = useRef(0);
  const grokLoginId = useRef<string | null>(null);

  const navigate = useCallback((next: ScreenId) => {
    if (next === screen) return;
    const startedAt = performance.now();
    setMountedScreens((current) => {
      const updated = new Set(current);
      // Set insertion order doubles as a tiny LRU. Keeping the current page
      // and two recent pages makes ordinary back-and-forth instant without
      // retaining every large screen DOM for the lifetime of the app.
      updated.delete(next);
      updated.add(next);
      while (updated.size > MAX_WARM_SCREENS) {
        const oldest = updated.values().next().value as ScreenId | undefined;
        if (!oldest) break;
        updated.delete(oldest);
      }
      return updated;
    });
    setScreen(next);
    window.requestAnimationFrame(() => {
      recordInvokeResult(
        `ui:navigate:${next}`,
        true,
        undefined,
        Date.now(),
        performance.now() - startedAt,
      );
    });
  }, [screen]);

  const reloadOnboardingConnections = useCallback(async () => {
    const statusGeneration = ++statusLoadGeneration.current;
    const [routesResult, oauthResult, grokResult, proxyResult] = await Promise.allSettled([
      api.listRoutes(),
      api.getCodexOAuthStatus(),
      api.getGrokAccountStatus(),
      api.getProxyStatus(),
    ]);
    const routes = routesResult.status === "fulfilled" ? routesResult.value : [];
    const oauth = oauthResult.status === "fulfilled" ? oauthResult.value : null;
    const grok = grokResult.status === "fulfilled" ? grokResult.value : null;
    const next = buildOnboardingConnections(
      routes,
      oauth,
      grok,
      t("onboarding.connect.grokUnavailable"),
    );
    setOnboardingConnections((current) => ({
      chatgpt: { ...next.chatgpt, pending: current.chatgpt.pending },
      grok: { ...next.grok, pending: current.grok.pending },
      custom: next.custom,
    }));
    if (
      proxyResult.status === "fulfilled" &&
      statusGeneration === statusLoadGeneration.current
    ) {
      setStatus((current) => ({ ...current, proxy: proxyResult.value }));
    }
  }, [t]);

  useEffect(() => {
    if (onboardingComplete) return;
    void reloadOnboardingConnections();
    return () => {
      oauthPollGeneration.current += 1;
      grokPollGeneration.current += 1;
    };
  }, [onboardingComplete, reloadOnboardingConnections]);

  const connectOnboardingProvider = useCallback(async (kind: ConnectKind) => {
    if (kind === "custom") return;
    setOnboardingBusy(kind);
    setOnboardingError(null);
    if (kind === "chatgpt") {
      const generation = ++oauthPollGeneration.current;
      try {
        const login = await api.startCodexOAuthLogin();
        setOnboardingConnections((current) => ({
          ...current,
          chatgpt: {
            ...current.chatgpt,
            pending: { code: login.userCode, uri: login.verificationUri },
          },
        }));
        const deadline = Date.now() + login.expiresIn * 1000;
        while (generation === oauthPollGeneration.current && Date.now() < deadline) {
          await new Promise((resolve) => window.setTimeout(resolve, login.interval * 1000));
          if (generation !== oauthPollGeneration.current) return;
          if (await api.pollCodexOAuthLogin(login.deviceCode)) {
            setOnboardingConnections((current) => ({
              ...current,
              chatgpt: { ...current.chatgpt, pending: null },
            }));
            await reloadOnboardingConnections();
            return;
          }
        }
        throw new Error(t("models.ui.errors.oauthExpired"));
      } catch (cause) {
        if (generation === oauthPollGeneration.current) setOnboardingError(String(cause));
      } finally {
        if (generation === oauthPollGeneration.current) setOnboardingBusy(null);
      }
      return;
    }

    const generation = ++grokPollGeneration.current;
    try {
      let login = await api.startGrokAccountLogin();
      grokLoginId.current = login.loginId;
      while (generation === grokPollGeneration.current && login.state === "waiting") {
        await new Promise((resolve) => window.setTimeout(resolve, 1500));
        if (generation !== grokPollGeneration.current) return;
        login = await api.pollGrokAccountLogin(login.loginId);
      }
      if (login.state !== "complete") {
        throw new Error(login.error ?? t("models.ui.grok.loginIncomplete"));
      }
      await reloadOnboardingConnections();
    } catch (cause) {
      if (generation === grokPollGeneration.current) setOnboardingError(String(cause));
    } finally {
      grokLoginId.current = null;
      if (generation === grokPollGeneration.current) setOnboardingBusy(null);
    }
  }, [reloadOnboardingConnections, t]);

  const cancelOnboardingProvider = useCallback(async (kind: ConnectKind) => {
    setOnboardingError(null);
    if (kind === "chatgpt") {
      oauthPollGeneration.current += 1;
      setOnboardingConnections((current) => ({
        ...current,
        chatgpt: { ...current.chatgpt, pending: null },
      }));
      setOnboardingBusy(null);
      return;
    }
    if (kind === "grok") {
      grokPollGeneration.current += 1;
      const loginId = grokLoginId.current;
      grokLoginId.current = null;
      if (loginId) await api.cancelGrokAccountLogin(loginId).catch(() => undefined);
      setOnboardingBusy(null);
    }
  }, []);

  const addOnboardingCustomProvider = useCallback(async (draft: CustomEndpointDraft) => {
    setOnboardingBusy("custom");
    setOnboardingError(null);
    try {
      const result = await api.probeEndpoint(draft.baseUrl, draft.apiKey.trim() || undefined);
      if (!result.reachable) throw new Error(t("models.ui.errors.probeFailed", { detail: draft.baseUrl }));
      const compatible = codexCompatibleProbeModels(result);
      const selected = compatible.map((capability) => capability.model);
      const model = selected[0] ?? result.models[0];
      const wire = compatible[0]?.wire ?? result.wire;
      if (!model) throw new Error(t("models.ui.errors.modelRequired"));
      if (!wire || !selected.length) {
        throw new Error(t("onboarding.connect.codexToolProtocolUnavailable"));
      }
      await api.createRoute(
        draft.name,
        draft.baseUrl,
        model,
        wire,
        result.streaming,
        result.reasoning,
        result.serverSideResume,
        draft.apiKey.trim() || undefined,
        "openAiCompatible",
        result.models,
        selected,
        result.contextWindow,
        result.modelCapabilities,
      );
      await reloadOnboardingConnections();
    } catch (cause) {
      setOnboardingError(String(cause));
    } finally {
      setOnboardingBusy(null);
    }
  }, [reloadOnboardingConnections, t]);

  const startOnboardingProxy = useCallback(async () => {
    setOnboardingProxyBusy(true);
    setOnboardingError(null);
    try {
      const proxy = await api.startProxy();
      // Invalidate an initial/background status read that may have started
      // before the proxy transition. Otherwise its stale `running: false`
      // result can repaint the final onboarding step after a successful start.
      statusLoadGeneration.current += 1;
      setStatus((current) => ({ ...current, proxy }));
    } catch (cause) {
      setOnboardingError(String(cause));
    } finally {
      setOnboardingProxyBusy(false);
    }
  }, []);

  const finishOnboarding = useCallback(() => {
    markOnboardingCompleted(window.localStorage);
    setOnboardingComplete(true);
  }, []);

  const refresh = useCallback(async (force = false, manageSpinner = true) => {
    if (!force && refreshInFlight.current) {
      return refreshInFlight.current;
    }
    const statusGeneration = ++statusLoadGeneration.current;
    if (manageSpinner) setRefreshing(true);
    const work = (async () => {
      try {
        const [proxy, runtime, enhancedRuntime, overview, updates] = await Promise.allSettled([
          api.getProxyStatus(),
          api.getRuntimeStatus(),
          api.getEnhancedDesktopRuntimeStatus(),
          api.getOverview(force),
          api.getUpdateStatus(),
        ]);
        if (statusGeneration === statusLoadGeneration.current) {
          setStatus((current) => ({
            proxy: proxy.status === "fulfilled" ? proxy.value : current.proxy,
            runtime: runtime.status === "fulfilled" ? runtime.value : current.runtime,
            enhancedRuntime:
              enhancedRuntime.status === "fulfilled"
                ? enhancedRuntime.value
                : current.enhancedRuntime,
            overview: overview.status === "fulfilled" ? overview.value : current.overview,
            updates: updates.status === "fulfilled" ? updates.value : current.updates,
          }));
        }
        const failure = [proxy, runtime, enhancedRuntime, overview, updates].find(
          (result): result is PromiseRejectedResult => result.status === "rejected",
        );
        setRefreshError(failure ? t("common.statusUpdateFailed", { detail: String(failure.reason) }) : null);
      } finally {
        if (manageSpinner) setRefreshing(false);
        refreshInFlight.current = null;
      }
    })();
    refreshInFlight.current = work;
    return work;
  }, [t]);

  const manualRefresh = useCallback(async () => {
    const version = ++refreshSequence.current;
    setRefreshing(true);
    const pageRefresh = new Promise<void>((resolve) => {
      pageRefreshWaiter.current = { version, resolve };
    });
    setRefreshVersions((current) => ({ ...current, [screen]: version }));
    try {
      await Promise.all([
        refresh(true, false),
        Promise.race([
          pageRefresh,
          new Promise<void>((resolve) => window.setTimeout(resolve, 15_000)),
        ]),
      ]);
    } finally {
      pageRefreshWaiter.current = null;
      setRefreshing(false);
    }
  }, [refresh, screen]);

  const pageRefreshComplete = useCallback((version: number) => {
    if (pageRefreshWaiter.current?.version === version) {
      pageRefreshWaiter.current.resolve();
      pageRefreshWaiter.current = null;
    }
  }, []);

  const applyProxyStatus = useCallback((proxy: ProxyStatus) => {
    // Invalidate a status request that started before the lifecycle command.
    // The command result is the authoritative postcondition; waiting for a
    // second background refresh made the Today button appear unchanged and
    // invited users to click Start/Stop twice.
    setStatus((current) => {
      if (!shouldApplyProxyLifecycle(current.proxy, proxy)) {
        return current;
      }
      statusLoadGeneration.current += 1;
      return { ...current, proxy };
    });
  }, []);

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void api.subscribeProxyLifecycle((proxy) => {
      applyProxyStatus(proxy);
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [applyProxyStatus]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    const timer = window.setInterval(() => {
      if (document.visibilityState === "visible") void refresh();
    }, 60_000);
    const onVisibility = () => {
      if (document.visibilityState === "visible") void refresh();
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [refresh]);

  useEffect(() => {
    const updatePerformanceMode = () => {
      document.documentElement.dataset.performance =
        document.visibilityState === "visible" ? "normal" : "low";
    };
    updatePerformanceMode();
    document.addEventListener("visibilitychange", updatePerformanceMode);
    return () =>
      document.removeEventListener("visibilitychange", updatePerformanceMode);
  }, []);

  /* registry.ts 一直宣稱鍵盤捷徑從它讀，但其實從來沒接上。 */
  useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      if (!event.ctrlKey && !event.metaKey) return;
      const index = Number(event.key) - 1;
      if (!Number.isInteger(index)) return;
      const target = SCREENS[index];
      if (!target) return;
      event.preventDefault();
      navigate(target.id);
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [navigate]);

  if (!onboardingComplete) {
    return (
      <Onboarding
        step={onboardingStep}
        onStep={(next) => setOnboardingStep(Math.max(0, Math.min(4, next)))}
        connections={onboardingConnections}
        busy={onboardingBusy}
        error={onboardingError}
        proxyRunning={status.proxy?.running ?? false}
        proxyBusy={onboardingProxyBusy}
        onConnect={(kind) => void connectOnboardingProvider(kind)}
        onCancelConnect={(kind) => void cancelOnboardingProvider(kind)}
        onAddCustom={(draft) => void addOnboardingCustomProvider(draft)}
        onStartProxy={() => void startOnboardingProxy()}
        onFinish={finishOnboarding}
      />
    );
  }

  /**
   * 接管失敗要當著使用者的面講一次。
   *
   * 這些通知本來只長在設定頁裡。但接管是在**按下 Proxy 的那一刻**發生的，
   * 而按完之後沒有人會特地走去設定頁看它成功了沒 —— 於是失敗的那一次就
   * 安安靜靜地過去了，Codex 照常回話，只是回話的是原生 Codex。整件事最
   * 糟的地方不是失敗，是失敗跟成功長得一模一樣。
   *
   * 不阻塞：它不是 modal，不搶焦點，不擋任何操作。Proxy 已經在跑了，
   * 第三方對話也還是通的 —— 只是走原生 Codex。把它做成必須先關掉才能繼續
   * 的東西，等於為一件「降級但可用」的事付出「完全不能用」的代價。
   *
   * key 帶上 detail：同一種失敗換了原因就是新的一件事，該再講一次。關掉的
   * 是「這一次這個原因」，不是「這類訊息」。
   */
  const armingAlerts = (status.runtime?.liveApplied ?? []).filter((notice) =>
    notice.code === "enhancedDesktopRuntimeNotArmed" ||
    notice.code === "enhancedDesktopBridgeNotObserved",
  );
  const alertKey = (notice: (typeof armingAlerts)[number]) =>
    `${notice.code}:${JSON.stringify(notice.params ?? {})}`;
  const liveAlerts = armingAlerts.filter(
    (notice) => !dismissedAlerts.includes(alertKey(notice)),
  );

  return (
    <>
      {/* 大氣層。順序就是堆疊順序：天光 → 光斑 → 鏡頭光 → 紙紋。
          全部在 .shell 底下，所以紙紋會從半透明的卡片裡透出來，
          但不會糊到文字。 */}
      <div className="sky" aria-hidden="true">
        <div className="bokeh bokeh--a" />
        <div className="bokeh bokeh--b" />
        <div className="bokeh bokeh--c" />
      </div>
      <div className="leak" aria-hidden="true" />
      <div className="grain" aria-hidden="true" />

      <div className="shell">
        <StatusBar
          status={status}
          refreshing={refreshing}
          refreshError={refreshError}
          onRefresh={() => void manualRefresh()}
        />

        <Rail active={screen} onNavigate={navigate} attention={attention(status)} />

        <main className="canvas">
          {mountedScreens.has("enhanced") ? <div className="screen-slot" hidden={screen !== "enhanced"}><EnhancedCore status={status.enhancedRuntime ?? null} refreshVersion={refreshVersions.enhanced ?? 0} onRefreshComplete={pageRefreshComplete} /></div> : null}
          {mountedScreens.has("today") ? (
            <div className="screen-slot" hidden={screen !== "today"}>
            <Today
              proxy={status.proxy}
              overview={status.overview}
              onNavigate={navigate}
              onChanged={() => void refresh()}
              onProxyChanged={applyProxyStatus}
              refreshVersion={refreshVersions.today ?? 0}
              onRefreshComplete={pageRefreshComplete}
              active={screen === "today"}
            />
            </div>
          ) : null}
          {mountedScreens.has("models") ? (
            <div className="screen-slot" hidden={screen !== "models"}>
            <Models
              onChanged={() => void refresh()}
              refreshVersion={refreshVersions.models ?? 0}
              onRefreshComplete={pageRefreshComplete}
            />
            </div>
          ) : null}
          {mountedScreens.has("context") ? (
            <div className="screen-slot" hidden={screen !== "context"}>
            <Context
              refreshVersion={refreshVersions.context ?? 0}
              onRefreshComplete={pageRefreshComplete}
              active={screen === "context"}
            />
            </div>
          ) : null}
          {mountedScreens.has("remote") ? (
            <div className="screen-slot" hidden={screen !== "remote"}>
            <Remote
              refreshVersion={refreshVersions.remote ?? 0}
              onRefreshComplete={pageRefreshComplete}
              active={screen === "remote"}
            />
            </div>
          ) : null}
          {mountedScreens.has("log") ? (
            <div className="screen-slot" hidden={screen !== "log"}>
            <Log
              refreshVersion={refreshVersions.log ?? 0}
              onRefreshComplete={pageRefreshComplete}
              active={screen === "log"}
            />
            </div>
          ) : null}
          {mountedScreens.has("settings") ? (
            <div className="screen-slot" hidden={screen !== "settings"}>
            <Settings
              onChanged={() => void refresh()}
              onOpenOnboarding={() => {
                setOnboardingStep(0);
                setOnboardingError(null);
                setOnboardingComplete(false);
              }}
              refreshVersion={refreshVersions.settings ?? 0}
              onRefreshComplete={pageRefreshComplete}
              active={screen === "settings"}
            />
            </div>
          ) : null}
        </main>

        {liveAlerts.length ? (
          <div className="alerts" role="region" aria-label={t("runtime.alerts.label")}>
            {liveAlerts.map((notice) => (
              <Notice
                key={alertKey(notice)}
                tone="warn"
                acts={
                  <>
                    <Btn
                      soft
                      onClick={() =>
                        setDismissedAlerts((current) => [...current, alertKey(notice)])
                      }
                    >
                      {t("common.dismiss")}
                    </Btn>
                  </>
                }
              >
                {noticeText(notice, t)}
              </Notice>
            ))}
          </div>
        ) : null}
      </div>
    </>
  );
}
