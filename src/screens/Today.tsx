import { useLocaleFormat } from "@/i18n/useLocaleFormat";
import { useTranslation } from "react-i18next";
import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "@/lib/api";
import { shouldApplyProxyLifecycle } from "@/lib/proxyLifecycle";
import { startVisiblePoll } from "@/lib/visiblePoll";
import { percent, tokens } from "@/lib/format";
import { findTightestWeeklyQuota, quotaPeriodLabel, remainingQuotaPresentation } from "@/lib/quota";
import {
  isLive,
  orderSessions,
  sessionLabel,
  sessionPercent,
  tightestSession,
} from "@/lib/sessions";
import {
  NONE,
  providerState,
  quotaUnavailableReason,
  supportsQuota,
} from "@/lib/vocabulary";
import { Sparkline } from "@/components/Sparkline";
import { Btn, Cap, Card, Empty, FindingCard, Meter, Metric, Row, Rows, State, Tray } from "@/components/ui";
import type {
  ContextBudget,
  Overview,
  ProviderOverview,
  ProxyStatus,
  SessionStatus,
} from "@/types";
import type { ScreenId } from "./registry";

/* 一次攤開幾個工作階段。`orderSessions` 已經把活著的排在前面、其餘依上下文
   用量遞減，所以前十個就是「現在會出事的那些」。開很多視窗的人這張卡原本
   會長到整頁都是它，而第十一個之後的資訊價值只剩「那個視窗我是不是忘了
   關」—— 那個問題可以等被問到再答，收進托盤裡。 */
const SESSIONS_SHOWN = 10;

export function Today({
  proxy,
  overview,
  onNavigate,
  onChanged,
  onProxyChanged,
  refreshVersion,
  onRefreshComplete,
  active = true,
}: {
  proxy: ProxyStatus | null;
  overview: Overview | null;
  onNavigate: (id: ScreenId) => void;
  onChanged: () => void;
  onProxyChanged: (status: ProxyStatus) => void;
  refreshVersion: number;
  onRefreshComplete: (version: number) => void;
  active?: boolean;
}) {
  const { t } = useTranslation();
  const { exact, resetLabel, sinceLabel } = useLocaleFormat();
  const [proxyBusy, setProxyBusy] = useState(false);
  const [providers, setProviders] = useState<ProviderOverview[]>([]);
  const [dashboardRouteIds, setDashboardRouteIds] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [budget, setBudget] = useState<ContextBudget | null>(null);
  const [quotaBusy, setQuotaBusy] = useState<string | null>(null);
  const [askedAt, setAskedAt] = useState<Record<string, number>>({});
  const [sessions, setSessions] = useState<SessionStatus[]>([]);
  const loadGeneration = useRef(0);

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void api.subscribeProxyLifecycle((next) => {
      if (shouldApplyProxyLifecycle(proxy, next)) onProxyChanged(next);
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [onProxyChanged, proxy]);

  const load = useCallback(async (forceRefresh: boolean) => {
      const generation = ++loadGeneration.current;
      const [nextProviders, nextRoutes, nextSessions] = await Promise.allSettled([
        api.getProviderOverviews(forceRefresh),
        api.listRoutes(),
        api.listSessions(),
      ]);
      if (generation !== loadGeneration.current) return;
      if (nextProviders.status === "fulfilled") setProviders(nextProviders.value);
      if (nextSessions.status === "fulfilled") setSessions(nextSessions.value);
      if (nextRoutes.status === "fulfilled") {
        const saved = window.localStorage.getItem("vellum.dashboard.providers");
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
      const failures = [nextProviders, nextRoutes, nextSessions].filter(
        (result): result is PromiseRejectedResult => result.status === "rejected",
      );
      setError(
        failures.length
          ? t("today.errors.partialRefresh", { detail: failures.map((failure) => String(failure.reason)).join(t("common.listSeparator")) })
          : null,
      );
      if (forceRefresh && refreshVersion > 0) onRefreshComplete(refreshVersion);
  }, [onRefreshComplete, refreshVersion, t]);

  useEffect(() => {
    void load(refreshVersion > 0);
  }, [load, refreshVersion]);

  useEffect(() => startVisiblePoll({
    active,
    intervalMs: 300_000,
    load: () => load(false),
  }), [active, load]);

  const routeId = overview?.route?.id ?? null;
  const routeModel = overview?.route?.model ?? null;
  useEffect(() => {
    if (!routeId) {
      setBudget(null);
      return;
    }
    let alive = true;
    void api
      .listModelRoutes()
      .then((models) => {
        const match =
          models.find((m) => m.routeId === routeId && m.upstreamModel === routeModel) ??
          models.find((m) => m.routeId === routeId);
        return match ? api.getContextBudget(match.catalogId) : null;
      })
      .then((next) => {
        if (alive) setBudget(next);
      })
      .catch(() => {
      });
    return () => {
      alive = false;
    };
  }, [routeId, routeModel, refreshVersion]);

  if (!overview) {
    return (
      <>
        <div className="canvas__head">
          <h2 className="canvas__title">{t("today.title")}</h2>
        </div>
        <Card>
          <Empty>{t("today.loading")}</Empty>
        </Card>
      </>
    );
  }

  const { route, lastSuccessfulRoute, quota, usage, health, findings } = overview;
  const ctxPercent = percent(usage.usedTokens, usage.windowTokens);

  async function refreshProviderQuota(routeId: string) {
    setQuotaBusy(routeId);
    setError(null);
    try {
      await api.refreshQuota(routeId);
      setProviders(await api.getProviderOverviews());
      setAskedAt((prev) => ({ ...prev, [routeId]: Math.floor(Date.now() / 1000) }));
      onChanged();
    } catch (cause) {
      setError(t("today.errors.quotaRefresh", { detail: String(cause) }));
    } finally {
      setQuotaBusy(null);
    }
  }

  async function toggleProxy() {
    setProxyBusy(true);
    setError(null);
    try {
      const next = proxy?.running
        ? await api.stopProxyAndRestore()
        : await api.startProxy();
      // Apply the lifecycle command's observed result immediately. A slower
      // status refresh that began before this click is invalidated by App.
      onProxyChanged(next);
      onChanged();
      try {
        setProviders(await api.getProviderOverviews());
      } catch (cause) {
        // The proxy transition already succeeded. A secondary dashboard
        // refresh failure must not relabel it as a failed Start/Stop action.
        setError(t("today.errors.partialRefresh", { detail: String(cause) }));
      }
    } catch (cause) {
      setError(t("today.errors.proxyOperation", { detail: String(cause) }));
    } finally {
      setProxyBusy(false);
    }
  }

  const shown = providers.filter((provider) =>
    dashboardRouteIds.includes(provider.route.id),
  );

  const tightest = findTightestWeeklyQuota(providers);
  const tightestPresentation = tightest
    ? remainingQuotaPresentation(tightest.remaining)
    : null;

  const nowSeconds = Math.floor(Date.now() / 1000);
  const orderedSessions = orderSessions(sessions, nowSeconds);
  const liveCount = sessions.filter((session) => isLive(session, nowSeconds)).length;
  /* 前十個跟托盤裡的其餘用同一個列 —— 收起來的那些不是次要資料，只是排在
     後面，兩種畫法會讓人以為它們是別的東西。 */
  const sessionRow = (session: SessionStatus) => {
    const pct = sessionPercent(session);
    return (
      <div className="watch__row" key={session.id} data-serving={isLive(session, nowSeconds)}>
        <div className="watch__who">
          <span className="watch__name">
            {sessionLabel(session, t("context.sessions.untitled"))}
          </span>
          <span className="rows__hint">{session.provider}</span>
        </div>

        <div className="watch__models">
          <code className="watch__model">{session.model}</code>
        </div>

        {/* 用量條沿用額度那一欄的結構：數字 + 條 + 說明。
            門檻畫在條上，因為「還差多遠」是空間關係。 */}
        <div className="watch__quota">
          <span className="watch__pct">{pct}%</span>
          <span className="watch__bar">
            <i style={{ width: `${pct}%` }} />
            <b style={{ left: `${session.compactThresholdPercent}%` }} />
          </span>
          <span className="watch__reset">
            {exact(session.usedTokens)} / {exact(session.windowTokens)}
          </span>
        </div>

        <div className="watch__act">
          <span className="rows__hint">{sinceLabel(session.lastActivityAt)}</span>
        </div>
      </div>
    );
  };
  const tightestContextSession = tightestSession(sessions);
  const contextPercent = tightestContextSession
    ? sessionPercent(tightestContextSession)
    : ctxPercent;
  const contextThreshold =
    tightestContextSession?.compactThresholdPercent ?? budget?.compactThresholdPercent;
  const contextUsedTokens = tightestContextSession?.usedTokens ?? usage.usedTokens;
  const contextWindowTokens = tightestContextSession?.windowTokens ?? usage.windowTokens;

  const ordered = [...shown].sort((a, b) => {
    const aLive = (proxy?.running ?? false) && a.appliedToRunningProxy ? 0 : 1;
    const bLive = (proxy?.running ?? false) && b.appliedToRunningProxy ? 0 : 1;
    return aLive - bLive;
  });

  return (
    <>
      <div className="canvas__head">
        <div>
          <p className="eyebrow">{t("today.title")}</p>
          <h2 className="canvas__title">
            {lastSuccessfulRoute
              ? `${lastSuccessfulRoute.model} · ${lastSuccessfulRoute.provider}`
              : t("today.noSuccessfulRequest")}
          </h2>
        </div>
        <div className="rowline">
          {/* 只留真正屬於「現況」的那一個開關。停止會同時還原 Codex，
              所以文案要把後果講完，不能只說「停止」。 */}
          <Btn onClick={() => void toggleProxy()} disabled={proxyBusy}>
            {proxyBusy ? t("common.processing") : proxy?.running ? t("today.proxy.stopRestore") : t("today.proxy.start")}
          </Btn>
          {route ? null : <Btn onClick={() => onNavigate("models")}>{t("today.addProvider")}</Btn>}
        </div>
      </div>

      {proxy?.stage && (proxy.phase === "preparing" || proxy.phase === "starting" || proxy.phase === "stopping") ? (
        <p className="note">
          {proxy.stage}
          {proxy.stageElapsedMs != null ? ` · ${proxy.stageElapsedMs}ms` : ""}
        </p>
      ) : null}

      {error ? <p className="note">{error}</p> : null}

      {/* 階段 2：三個數字移到第一屏。
          它們是這頁存在的理由 —— 以前被 Provider 卡推到第二屏，等於要捲動
          才看得到「現在怎樣」，那就不叫現況了。

          主位顯示週額度剩餘最少的 Provider：可以同時開好幾個
          session 打不同供應商，這時候「當前」根本不是單數。也刻意不做平均 ——
          平均會把快爆掉的那一家藏起來，而那正是你需要先知道的。

          grid--even：這一排等高。三張都撐得起那個高度才套 —— 中間那張
          補上了門檻與預估，不是靠拉高留白湊出來的。 */}
      <div className="grid3 grid--even">
        <Card hero>
          <Cap>
            {tightest
              ? t("today.quota.tightest")
              : quota
                ? `${t("today.quota.remaining")} · ${quotaPeriodLabel(quota.period, t)}`
                : t("today.quota.remaining")}
          </Cap>
          {tightest ? (
            <>
              <Metric
                value={tightestPresentation!.value}
                unit="%"
                glow="var(--apricot-deep)"
              />
              <Meter percent={tightestPresentation!.meterPercent} tone="honey" />
              <p className="card__sub">
                {tightest.name}
                {tightest.resetAt ? ` · ${resetLabel(tightest.resetAt)}` : ""}
              </p>
            </>
          ) : (
            <Empty>{t("today.quota.noData")}</Empty>
          )}
        </Card>
        {/* 「用了幾 %」本身不可怕，可怕的是「什麼時候會被壓縮」。
            所以這張卡除了現值，還要回答門檻在哪、以這個速度還能撐多久。
            門檻直接畫成量條上的一道刻度 —— 距離是空間關係，寫成數字
            等於要人自己心算。 */}
        <Card tile>
          <Cap>
            {tightestContextSession ? t("today.context.nearestThreshold") : t("today.context.usage")}
          </Cap>
          <Metric value={contextPercent} unit="%" glow="var(--lavender)" small />
          <Meter
            percent={contextPercent}
            markAt={contextThreshold}
            markLabel={
              contextThreshold ? t("today.context.threshold", { percent: contextThreshold }) : undefined
            }
          />
          <p className="card__sub">
            {exact(contextUsedTokens)} / {exact(contextWindowTokens)} token
          </p>

          <Rows>
            {tightestContextSession ? (
              <>
                <Row label={t("common.provider")}>{tightestContextSession.provider}</Row>
                <Row label={t("common.model")}>
                  <code>{tightestContextSession.model}</code>
                </Row>
                <Row label={t("today.lastActivity")}>
                  {sinceLabel(tightestContextSession.lastActivityAt)}
                </Row>
              </>
            ) : (
              <>
                <Row label={t("today.context.compactThreshold")}>
                  {budget ? (
                    <>
                      {budget.compactThresholdPercent}%
                      <span className="rows__hint" style={{ marginLeft: 8 }}>
                        {exact(compactAt(budget))}
                      </span>
                    </>
                  ) : (
                    NONE
                  )}
                </Row>
                <Row label={t("today.context.averageTurn")}>
                  {usage.turns > 0
                    ? `${exact(Math.round(usage.usedTokens / usage.turns))} token`
                    : NONE}
                </Row>
                <Row label={t("today.context.atRate")}>{headroomLabel(usage, budget, t)}</Row>
              </>
            )}
          </Rows>
        </Card>
        <Card tile>
          <Cap>{t("today.providerTokens")}</Cap>
          <Metric value={tokens(usage.providerTotalTokens)} unit="token" glow="var(--lavender)" small />
          <Sparkline points={usage.trend} label={t("today.context.trend")} />
          <p className="card__sub">{t("today.requests", { count: usage.turns })}</p>
        </Card>
      </div>

      {/* 有事才出現。沒事的時候一張寫著「目前沒有需要處理的事」的卡，
          只是佔掉第一屏的位置。 */}
      {findings.length ? (
        <Card>
          <div className="rowline">
            <Cap>{t("today.findings.title")}</Cap>
            <span className="rows__hint">{t("today.findings.count", { count: findings.length })}</span>
          </div>
          {findings.map((f) => (
            <FindingCard key={f.id} severity={f.severity} title={f.title} meta={f.location} />
          ))}
          <div className="rowline" style={{ marginTop: 16 }}>
            <Btn onClick={() => onNavigate("context")}>{t("today.findings.adjustContext")}</Btn>
          </div>
        </Card>
      ) : null}

      {/* 工作階段。
          同時開好幾個 Codex 視窗時，每個視窗是**獨立的對話、獨立的上下文視窗**。
          所以「上下文用到哪」根本不是一個數字 —— 上面那張卡顯示的是最先撞到
          壓縮門檻的那一個，這裡把其餘的平鋪出來。

          欄位跟 Provider 那排一致（誰／模型／量／時間），沿用同一種列，
          不為了這個功能長出第四種版面。閒置的不隱藏，只是退到後面變淡 ——
          「那個視窗我是不是忘了關」本身就是有用的資訊。 */}
      {sessions.length ? (
        <Card>
          <div className="rowline">
            <Cap>{t("today.sessions.title")}</Cap>
            <span className="rows__hint">
              {t("today.sessions.count", { live: liveCount, total: sessions.length })}
            </span>
          </div>

          <div className="watch">{orderedSessions.slice(0, SESSIONS_SHOWN).map(sessionRow)}</div>

          {/* 剩下的收進托盤。數量寫在標題列上 —— 收起來的時候那是唯一還說得出
              「後面還有幾個」的地方。 */}
          {orderedSessions.length > SESSIONS_SHOWN ? (
            <Tray
              label={t("today.sessions.rest")}
              count={orderedSessions.length - SESSIONS_SHOWN}
            >
              <div className="watch">{orderedSessions.slice(SESSIONS_SHOWN).map(sessionRow)}</div>
            </Tray>
          ) : null}
        </Card>
      ) : null}

      {/* 監看板。
          可以同時開好幾個 session 打不同供應商、不同模型，所以這裡不是
          「一家一張卡」也不是「收起來的清單」—— 是一排可以同時掃過去的
          長條。每一條自己講完：誰、什麼狀態、跑什麼模型、額度剩多少、
          最近一次多大、首字多久。

          在跑的排前面、暗的排後面：沒在服務的供應商不該跟正在服務的搶注意力。 */}
      <Card>
        <div className="rowline">
          <Cap>{t("today.providers.title")}</Cap>
        </div>

        {shown.length ? (
          <div className="watch">
            {ordered.map((provider) => {
              const state = providerState({
                enabled: provider.route.enabled,
                proxyRunning: proxy?.running ?? false,
                applied: provider.appliedToRunningProxy,
              });
              const serving = state.state === "live";
              const window = provider.quotaWindows[0] ?? null;
              const left = window ? 100 - window.usedPercent : null;
              const kind = provider.route.providerKind;
              const canQuota = supportsQuota(kind);
              const asked = sinceLabel(askedAt[provider.route.id]);
              return (
                <div
                  className="watch__row"
                  key={provider.route.id}
                  data-serving={serving}
                >
                  <div className="watch__who">
                    <span className="watch__name">{provider.route.name}</span>
                    <State tone={state.tone} label={t(state.labelKey)} />
                  </div>

                  {/* 跑哪些模型。多 session 時這一欄才是關鍵 ——
                      同一家可能同時被兩個 session 用不同模型打。 */}
                  <div className="watch__models">
                    {provider.models.length ? (
                      provider.models.map((m) => (
                        <code className="watch__model" key={m.catalogId}>
                          {m.upstreamModel}
                        </code>
                      ))
                    ) : (
                      <span className="rows__hint">{t("today.providers.noModels")}</span>
                    )}
                  </div>

                  <div className="watch__quota">
                    {provider.quotaError ? (
                      <span className="watch__err" title={provider.quotaError}>
                        {t("today.providers.quotaFailed")}
                      </span>
                    ) : left === null ? (
                      <span className="rows__hint">
                        {canQuota ? t("today.providers.notQueried") : quotaUnavailableReason(kind, t)}
                      </span>
                    ) : (
                      <>
                        <span className="watch__pct">{left}%</span>
                        <span className="watch__bar">
                          <i style={{ width: `${Math.max(0, Math.min(100, left))}%` }} />
                        </span>
                        <span className="watch__reset">
                          {window ? quotaPeriodLabel(window.period, t) : null}
                          {window?.resetAt ? ` · ${resetLabel(window.resetAt)}` : ""}
                          {asked ? ` · ${t("today.providers.lastChecked", { since: asked })}` : ""}
                        </span>
                      </>
                    )}
                  </div>

                  {/* 額度查詢是逐家的，所以按鈕也逐家。
                      只有真的查得到的供應商才給按鈕 —— 對一般 OpenAI 相容端點
                      顯示「重新查詢」是騙人的，按了只會拿到錯誤。 */}
                  <div className="watch__act">
                    {canQuota ? (
                      <Btn
                        soft
                        mini
                        disabled={quotaBusy !== null}
                        title={t("today.providers.refreshTitle", { provider: provider.route.name })}
                        onClick={() => void refreshProviderQuota(provider.route.id)}
                      >
                        {quotaBusy === provider.route.id ? t("today.providers.querying") : t("today.providers.refresh")}
                      </Btn>
                    ) : null}
                  </div>

                  {(state.remedyKey ? t(state.remedyKey) : null) ? (
                    <p className="watch__remedy">{(state.remedyKey ? t(state.remedyKey) : null)}</p>
                  ) : null}
                </div>
              );
            })}
          </div>
        ) : (
          <Empty>{t("today.providers.empty")}</Empty>
        )}
      </Card>

      <Card quiet>
        <Cap>{t("today.health.title")}</Cap>
        <Rows>
          <Row label={t("today.health.connectionReuse")}>
            {health.pooled ? t("today.health.reusing", { count: health.connections }) : t("today.health.notReusing")}
          </Row>
          <Row label={t("today.health.firstByte")}>
            {health.firstByteMs === null ? NONE : `${health.firstByteMs} ms`}
          </Row>
          <Row label={t("today.health.reasoning")}>{health.reasoningVisible ? t("common.visible") : t("common.hidden")}</Row>
          <Row label={t("today.health.history")}>{t("today.health.historyValue", { days: health.historyRetentionDays, count: health.historyRetentionDays })}</Row>
        </Rows>
      </Card>
    </>
  );
}
function compactAt(budget: ContextBudget): number {
  return Math.round((budget.effectiveWindow * budget.compactThresholdPercent) / 100);
}

function headroomLabel(
  usage: { usedTokens: number; turns: number },
  budget: ContextBudget | null,
  t: (key: string, options?: Record<string, unknown>) => string,
): string {
  if (!budget) return NONE;
  const limit = compactAt(budget);
  if (usage.usedTokens >= limit) return t("today.headroom.exceeded");
  if (usage.turns <= 0) return t("today.headroom.noEstimate");
  const perTurn = usage.usedTokens / usage.turns;
  if (perTurn <= 0) return t("today.headroom.noEstimate");
  return t("today.headroom.estimate", { count: Math.floor((limit - usage.usedTokens) / perTurn) });
}
