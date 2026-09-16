import { useTranslation } from "react-i18next";
/**
 * 紀錄 —— Token 使用量與請求執行紀錄。
 *
 * 兩層時間尺度，刻意分開：
 *   熱區圖   長期趨勢（每天／每週／累計）
 *   請求列表 單筆明細 —— 回答「剛剛那一筆為什麼失敗」
 * 混在一起的話兩個問題都答不好。
 *
 * 紀錄是除錯用的，不是帳本：只讀最近 N 筆、列表分頁 —— 一次塞幾百列
 * 只會讓人捲到手痠。
 *
 * OpenAI 熱區圖直接使用 Codex 個人檔案的官方統計；第三方模型再合併
 * Vellum Proxy 的逐請求用量，避免把同一筆官方請求重複計算。 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, getInvokeRing, type InvokeRingEntry } from "@/lib/api";
import { startVisiblePoll } from "@/lib/visiblePoll";
import { tokens } from "@/lib/format";
import { noticeText } from "@/lib/notice";
import {
  HEAT_MODES,
  buildActivityHeatmap,
  toColumns,
  type HeatColumn,
  type HeatMode,
} from "@/lib/heatmap";
import { Cap, Card, Empty, Heatmap, Pager, Segment } from "@/components/ui";
import type {
  BootTelemetry,
  RequestLog as RequestLogData,
  SubagentRun,
  UsageActivity,
  UsageProviderTotal,
} from "@/types";

/** Loose shape of `t` from `useTranslation()` — avoids pulling in i18next's
 * fully-typed overload set just for a couple of helper functions. */
type Translate = (key: string, options?: Record<string, unknown>) => string;

/** 一頁幾筆。20 筆大約是一屏，不用捲就能掃完。請求明細與系統事件共用，
 *  因為兩張卡的列高相近，兩種不同的頁長只會讓人以為它們是兩種東西。 */
const PAGE_SIZE = 20;
/** Retention hint only — the store still prunes past this; the renderer
 *  never fetches the whole window. */
const RETAIN = 500;

/* 段色不帶意義 —— 它只負責把長條上的一段和讀數列上的一列綁在一起，
   所以只要彼此分得開就夠，不需要圖例。取自色票裡既有的七個色相。 */
const SHARE_TONES = [
  "var(--lavender)",
  "var(--apricot)",
  "var(--skyglow)",
  "var(--sage)",
  "var(--honey)",
  "var(--haze)",
  "var(--coral)",
] as const;

/* 長條上最多切幾段。再多下去每段都變成看不出長短的細絲，
   而「誰吃掉我的量」問的就是長短，所以尾巴收成一段「其他」。 */
const SHARE_SEGMENTS = 6;

/** 佔比字串。小於 1% 顯示為 `<1%` —— 印成 `0%` 會讓一列看起來沒有用量，
    但它有，只是很小。 */
function sharePercent(value: number, total: number): string {
  if (total <= 0) return "0%";
  const percent = (value / total) * 100;
  if (percent > 0 && percent < 1) return "<1%";
  return `${Math.round(percent)}%`;
}

type ShareSlice = {
  key: string;
  label: string;
  tokens: number;
  tone: string;
  /** 這一列的數字來自 Codex 個人檔案而非 proxy 的逐請求紀錄。 */
  fromProfile: boolean;
  accountCount: number;
};

/** 由大到小，尾巴收成「其他」。回傳的順序就是長條與讀數列共用的順序。 */
function shareSlices(providers: UsageProviderTotal[], othersLabel: string): ShareSlice[] {
  const ranked = [...providers]
    .filter((provider) => provider.tokens > 0)
    .sort((a, b) => b.tokens - a.tokens);
  const head = ranked.slice(0, SHARE_SEGMENTS).map((provider, index) => ({
    key: provider.routeId,
    label: provider.provider,
    tokens: provider.tokens,
    tone: SHARE_TONES[index % SHARE_TONES.length]!,
    fromProfile: provider.source === "codex_profile",
    accountCount: provider.accountCount,
  }));
  const tail = ranked.slice(SHARE_SEGMENTS);
  if (!tail.length) return head;
  return [
    ...head,
    {
      key: "__others",
      label: othersLabel,
      tokens: tail.reduce((sum, provider) => sum + provider.tokens, 0),
      tone: "var(--ink-ghost)",
      fromProfile: false,
      accountCount: 0,
    },
  ];
}


/** 系統事件那張卡的三種面向。 */
type LogFacet = "compaction" | "subagent" | "invokes";

export function Log({
  refreshVersion,
  onRefreshComplete,
  active = true,
}: {
  refreshVersion: number;
  onRefreshComplete: (version: number) => void;
  active?: boolean;
}) {
  const { t } = useTranslation();
  const [data, setData] = useState<RequestLogData | null>(null);
  const [activity, setActivity] = useState<UsageActivity | null>(null);
  const [boot, setBoot] = useState<BootTelemetry | null>(null);
  const [mode, setMode] = useState<HeatMode>("daily");
  const [hover, setHover] = useState<{ col: HeatColumn; date: string | null } | null>(null);
  const [page, setPage] = useState(1);
  /* 紀錄是除錯用的：找「剛剛那筆為什麼失敗」原本得手動翻頁——分頁本身
     不是過濾，500 筆裡的一個 429 藏在第 20 頁翻不到。 */
  const [statusFilter, setStatusFilter] = useState<"all" | "failed">("all");
  const [providerFilter, setProviderFilter] = useState<string>("all");
  const [hoveredShare, setHoveredShare] = useState<number | null>(null);
  /* 壓縮／子代理／桌面呼叫共用一張卡，用這個切頁 —— 見下面渲染區塊
     開頭的註解。 */
  const [logFacet, setLogFacet] = useState<LogFacet>("compaction");
  /* 系統事件跟請求明細一樣分頁：三種面向都可能長到幾百列，而一次塞幾百列
     的問題不會因為它不是請求就消失。一個頁碼給三種面向共用 —— 換面向是換
     主題，不是換頁，所以那時回到第一頁；只有 jumpToChildren 例外，它要的
     就是某一個特定 run 所在的那一頁。 */
  const [eventPage, setEventPage] = useState(1);
  /* jumpToChildren 切分頁後，子代理那批 <details> 要等 React 真的把
     「子代理」分頁的內容掛上 DOM 才展開得到 —— 用 ref 記下待展開的
     run，讓下面那個 effect 在分頁真的生效後才動手，不用跟 React 的
     commit 時機賭 rAF 次數。 */
  const pendingSubagentJump = useRef<SubagentRun[] | null>(null);
  const focusEntryId = useRef<number | null>(null);
  const [focusTick, setFocusTick] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [invokes, setInvokes] = useState<InvokeRingEntry[]>([]);

  const loadLog = useCallback(async () => {
      const focus = focusEntryId.current;
      focusEntryId.current = null;
      const log = await api.getRequestLog({
          limit: PAGE_SIZE,
          offset: (page - 1) * PAGE_SIZE,
          failedOnly: statusFilter === "failed",
          routeId: providerFilter === "all" ? undefined : providerFilter,
          focusEntryId: focus ?? undefined,
        });
      setData(log);
      if (focus != null && typeof log.entryOffset === "number") {
          const nextPage = Math.floor(log.entryOffset / PAGE_SIZE) + 1;
          if (nextPage !== page) setPage(nextPage);
      }
      setInvokes(getInvokeRing());
      if (refreshVersion > 0) onRefreshComplete(refreshVersion);
  }, [onRefreshComplete, page, providerFilter, refreshVersion, statusFilter]);

  const loadSummary = useCallback(async () => {
    const [usage, boot] = await Promise.allSettled([
      api.getUsageActivity(),
      api.getBootTelemetry(),
    ]);
    if (usage.status === "fulfilled") setActivity(usage.value);
    if (boot.status === "fulfilled") setBoot(boot.value);
    const failures = [usage, boot].filter(
      (result): result is PromiseRejectedResult => result.status === "rejected",
    );
    setError(failures.length
      ? t("log.errors.partialRefresh", { detail: failures.map((failure) => String(failure.reason)).join(t("common.listSeparator")) })
      : null);
  }, [t]);

  useEffect(() => {
    void loadLog().catch((cause) => setError(String(cause)));
  }, [loadLog, focusTick]);

  useEffect(() => {
    void loadSummary();
  }, [loadSummary, refreshVersion]);

  useEffect(() => startVisiblePoll({
      active,
      intervalMs: 60_000,
      load: async () => {
        await Promise.all([loadLog(), loadSummary()]);
      },
    }), [active, loadLog, loadSummary]);

  const entries = data?.entries ?? [];
  const entryTotal = data?.entryTotal ?? entries.length;
  const pageCount = Math.max(1, Math.ceil(entryTotal / PAGE_SIZE));
  /* 資料變少時要把頁碼拉回範圍內，否則會停在一頁空白上 */
  const current = Math.min(page, pageCount);
  const shown = entries;

  /* 主請求 → 子請求關聯優先使用 DB entry id。Native V2 的 authority 是
     thread graph，hash 的是 thread id；compatibility path 才是 request id，
     所以只用 parentRequestHash 會讓 native child 永遠對不上請求列。 */
  const subagentByParent = useMemo(() => {
    const byEntry = new Map<number, SubagentRun[]>();
    const byHash = new Map<string, SubagentRun[]>();
    for (const run of data?.subagentRuns ?? []) {
      if (run.parentUsageEntryId != null) {
        const list = byEntry.get(run.parentUsageEntryId) ?? [];
        list.push(run);
        byEntry.set(run.parentUsageEntryId, list);
      }
      if (run.parentRequestHash) {
        const list = byHash.get(run.parentRequestHash) ?? [];
        list.push(run);
        byHash.set(run.parentRequestHash, list);
      }
    }
    return { byEntry, byHash };
  }, [data]);

  const subagentSummary = useMemo(() => {
    const runs = data?.subagentRuns ?? [];
    return runs.reduce(
      (summary, run) => {
        if (run.state === "completed") summary.completed += 1;
        else if (run.state === "requested" || run.state === "running") summary.active += 1;
        else summary.attention += 1;
        return summary;
      },
      { total: runs.length, completed: 0, active: 0, attention: 0 },
    );
  }, [data]);

  /* 三種面向的完整列表。分頁只切要畫的那一段，統計（上面的 subagentSummary、
     下面 Pager 的總數）仍然看全部 —— 「這一頁有幾個失敗」不是任何人想問的
     問題。 */
  const compactionEvents = data?.compactionEvents ?? [];
  const subagentRuns = data?.subagentRuns ?? [];
  /* 桌面呼叫的環形緩衝是舊到新，畫面要新到舊。 */
  const invokeRows = useMemo(() => invokes.slice().reverse(), [invokes]);
  const eventTotal =
    logFacet === "compaction"
      ? compactionEvents.length
      : logFacet === "subagent"
        ? subagentRuns.length
        : invokeRows.length;
  const eventPageCount = Math.max(1, Math.ceil(eventTotal / PAGE_SIZE));
  /* 跟請求明細同一條規則：資料變少時把頁碼拉回範圍內，否則會停在一頁空白上。 */
  const eventCurrent = Math.min(eventPage, eventPageCount);
  const eventFrom = (eventCurrent - 1) * PAGE_SIZE;
  const eventTo = eventCurrent * PAGE_SIZE;

  const providerByRoute = useMemo(() => {
    const map = new Map<string, string>();
    for (const route of data?.entryRoutes ?? []) map.set(route.routeId, route.provider);
    for (const entry of entries) map.set(entry.routeId, entry.provider);
    return map;
  }, [data, entries]);
  /* 篩選選單走後端回的 distinct routes，不接 activity.providers ——
     後者是官方統計跟 Proxy 紀錄合併過的另一份清單，routeId 集合不保證
     跟這裡的請求明細完全對得上。 */
  const providerFilterOptions = useMemo(
    () => [...providerByRoute.entries()].sort((a, b) => a[1].localeCompare(b[1])),
    [providerByRoute],
  );

  /* 捲動到 request 清單的某一筆；跨頁時先切到該筆所在頁。找不到就 no-op
     （對應「缺頁面目標」：不崩、不假裝成功）。 */
  const jumpToEntry = (entryId: number) => {
    const onPage = entries.some((entry) => entry.id === entryId);
    if (!onPage) {
      focusEntryId.current = entryId;
      setStatusFilter("all");
      setProviderFilter("all");
      setPage(1);
      setFocusTick((tick) => tick + 1);
    }
    requestAnimationFrame(() => {
      document
        .getElementById(`req-${entryId}`)
        ?.scrollIntoView({ behavior: "smooth", block: "center" });
    });
  };

  /* 展開並捲動到 parent 的所有 child：先切到子代理面向，再切到第一個 child
     所在的那一頁。同一個 parent 的 child 通常相鄰，所以其餘幾個多半也在同
     一頁；萬一不是，下面那個 effect 對找不到的元素就跳過，不會硬展開一個
     不在 DOM 裡的 run。 */
  const jumpToChildren = (runs: SubagentRun[]) => {
    const first = runs[0];
    if (!first) return;
    pendingSubagentJump.current = runs;
    const index = subagentRuns.indexOf(first);
    setEventPage(index < 0 ? 1 : Math.floor(index / PAGE_SIZE) + 1);
    setLogFacet("subagent");
  };

  /* 子代理分頁一旦真的掛上 DOM（`logFacet` 變成 "subagent" 之後的下一次
     render），就把 jumpToChildren 存的那批 run 展開並捲過去。用 effect
     而不是 setLogFacet 後面接 rAF，是因為 rAF 只能保證「下一次繪圖之前」，
     不保證 React 已經把這次 state 更新提交到 DOM——在測試環境
     （rAF 被同步 mock）兩者的順序甚至會反過來。 */
  useEffect(() => {
    const runs = pendingSubagentJump.current;
    if (logFacet !== "subagent" || !runs) return;
    pendingSubagentJump.current = null;
    const first = runs[0];
    if (!first) return;
    for (const run of runs) {
      const element = document.getElementById(`subrun-${subagentRunId(run, 0)}`);
      if (element) (element as HTMLDetailsElement).open = true;
    }
    const firstId = `subrun-${subagentRunId(first, 0)}`;
    requestAnimationFrame(() => {
      document
        .getElementById(firstId)
        ?.scrollIntoView({ behavior: "smooth", block: "center" });
    });
    /* `eventPage` 也在依賴裡：已經在子代理面向、只是換頁的那一次，
       `logFacet` 沒變，少了它就不會重跑。 */
  }, [logFacet, eventPage, data]);

  /* OpenAI 直接使用 Codex 個人檔案的官方 buckets；第三方才使用
     Vellum 每個上游 response 寫入一次的 Proxy 紀錄。 */
  const heat = useMemo(
    () =>
      buildActivityHeatmap(
        activity?.days ?? [],
        activity?.totalTokens ?? 0,
        365,
        true,
      ),
    [activity],
  );
  const columns = useMemo(() => toColumns(heat, mode), [heat, mode]);
  const shares = useMemo(
    () => shareSlices(activity?.providers ?? [], t("log.providers.others")),
    [activity, t],
  );
  /* 用切完之後的和當分母，而不是 activity.totalTokens：那個總數還含著
     用量為 0 的 Provider 以外的東西，兩邊不同源會讓佔比加起來不是 100%。 */
  const shareTotal = useMemo(
    () => shares.reduce((sum, slice) => sum + slice.tokens, 0),
    [shares],
  );

  if (!data || !activity) {
    return (
      <>
        <div className="canvas__head">
          <h2 className="canvas__title">{t("log.title")}</h2>
        </div>
        <Card>
          <Empty>{error ?? t("log.loading")}</Empty>
        </Card>
      </>
    );
  }

  return (
    <>
      <div className="canvas__head">
        <div>
          <p className="eyebrow">{t("log.title")}</p>
          <h2 className="canvas__title">{t("log.heading")}</h2>
        </div>
      </div>

      {boot ? (
        <p className="note">
          {boot.previousStartedAt
            ? t("log.boot", {
                count: boot.bootCount,
                pid: boot.pid,
                time: bootTime(boot.previousStartedAt),
              })
            : t("log.bootFirst", { pid: boot.pid })}
        </p>
      ) : null}

      {error ? <p className="note">{error}</p> : null}
      {activity.warning ? <p className="note">{noticeText(activity.warning, t)}</p> : null}

      <Card className="usage-card">
        {/* 統計摘要固定置於 HeatMap 上方，與時間分布形成同一個資訊區塊。 */}
        <div className="stats">
          <div className="stats__cell">
            <div className="stats__num">
              {tokens(activity.totalTokens)}
            </div>
            <div className="stats__label">{t("log.stats.total")}</div>
          </div>
          <div className="stats__cell">
            <div className="stats__num">
              {activity.peakTokens ? tokens(activity.peakTokens) : "—"}
            </div>
            <div className="stats__label">{t("log.stats.peak")}</div>
          </div>
          <div className="stats__cell">
            <div className="stats__num">
              {activity.longestTaskDurationMs
                ? `${(activity.longestTaskDurationMs / 1000).toFixed(1)}s`
                : "—"}
            </div>
            <div className="stats__label">{t("log.stats.longestRequest")}</div>
          </div>
          <div className="stats__cell">
            <div className="stats__num">{t("log.days", { count: activity.currentStreakDays })}</div>
            <div className="stats__label">{t("log.stats.currentStreak")}</div>
          </div>
          <div className="stats__cell">
            <div className="stats__num">{t("log.days", { count: activity.longestStreakDays })}</div>
            <div className="stats__label">{t("log.stats.longestStreak")}</div>
          </div>
        </div>

        <div className="heat__head heat__head--after-stats">
          <h3 className="heat__title">{t("log.activityTitle")}</h3>
          {/* 三種讀法共用同一個格陣：切換時位置不動、只有填法變。 */}
          <div className="heat__modes">
            {HEAT_MODES.map((m) => (
              <button
                key={m.value}
                type="button"
                className="heat__mode"
                aria-pressed={mode === m.value}
                onClick={() => {
                  setHover(null);
                  setMode(m.value);
                }}
              >
                {t(m.labelKey)}
              </button>
            ))}
          </div>
        </div>

        <div className="heat__plot">
          <Heatmap
            columns={columns}
            mode={mode}
            hovered={hover?.col ?? null}
            hoveredDate={hover?.date ?? null}
            onHover={(col, date) => setHover(col ? { col, date } : null)}
          />
        </div>
      </Card>

      {shares.length ? (
        <Card>
          <div className="rowline">
            <Cap>{t("log.providers.title")}</Cap>
            <span className="rows__hint">{t("log.providers.note")}</span>
          </div>
          {/* 長條在上、排名在下，共用同一個順序與同一組顏色。指到任一邊，
              另一邊跟著亮 —— 顏色只是綁定用的，所以不另外做圖例。 */}
          <div
            className="stackbar usageshare__bar"
            data-hovered={hoveredShare !== null}
            style={{ marginTop: 14 }}
          >
            {shares.map((slice, index) => (
              <button
                key={slice.key}
                type="button"
                className="stackbar__seg"
                data-active={hoveredShare === index}
                style={{ flexGrow: slice.tokens, background: slice.tone }}
                aria-label={t("log.providers.segmentAria", {
                  provider: slice.label,
                  percent: sharePercent(slice.tokens, shareTotal),
                  value: tokens(slice.tokens),
                })}
                onMouseEnter={() => setHoveredShare(index)}
                onMouseLeave={() => setHoveredShare(null)}
                onFocus={() => setHoveredShare(index)}
                onBlur={() => setHoveredShare(null)}
              />
            ))}
          </div>
          <div className="usageshare__rows" data-hovered={hoveredShare !== null}>
            {shares.map((slice, index) => (
              <div
                key={slice.key}
                className="usageshare__row"
                data-active={hoveredShare === index}
                onMouseEnter={() => setHoveredShare(index)}
                onMouseLeave={() => setHoveredShare(null)}
              >
                <span className="usageshare__swatch" style={{ background: slice.tone }} />
                <span className="usageshare__name">
                  {slice.label}
                  {slice.accountCount > 0 ? (
                    <span className="usageshare__source">
                      {t("log.providers.accountCount", { count: slice.accountCount })}
                    </span>
                  ) : null}
                  {slice.fromProfile ? (
                    <span className="usageshare__source">{t("log.providers.fromProfile")}</span>
                  ) : null}
                </span>
                {/* 佔比在前：這張卡問的是「誰吃掉我的量」，那是比例問題，
                    絕對數字要心算才答得出來，所以退成佐證。 */}
                <span className="usageshare__share">
                  {sharePercent(slice.tokens, shareTotal)}
                </span>
                <span className="usageshare__tokens">
                  {t("log.tokens", { value: tokens(slice.tokens) })}
                </span>
              </div>
            ))}
          </div>
        </Card>
      ) : null}

      <Card>
        <div className="rowline">
          <Cap>{t("log.requests.title")}</Cap>
          <span className="rows__hint">
            {t("log.requests.note", { count: RETAIN })}
          </span>
        </div>

        {providerFilterOptions.length > 0 || statusFilter !== "all" || providerFilter !== "all" ? (
          <div className="rowline" style={{ marginTop: 10 }}>
            <Segment
              options={[
                { value: "all", label: t("log.requests.filter.statusAll") },
                { value: "failed", label: t("log.requests.filter.statusFailed") },
              ]}
              value={statusFilter}
              onChange={(next) => {
                setStatusFilter(next);
                setPage(1);
              }}
            />
            {providerFilterOptions.length > 1 ? (
              <select
                className="input"
                style={{ width: "auto" }}
                value={providerFilter}
                onChange={(event) => {
                  setProviderFilter(event.target.value);
                  setPage(1);
                }}
              >
                <option value="all">{t("log.requests.filter.providerAll")}</option>
                {providerFilterOptions.map(([routeId, provider]) => (
                  <option key={routeId} value={routeId}>{provider}</option>
                ))}
              </select>
            ) : null}
          </div>
        ) : null}

        {entryTotal === 0 && (statusFilter !== "all" || providerFilter !== "all") ? (
          <Empty>{t("log.requests.filterEmpty")}</Empty>
        ) : entryTotal === 0 ? (
          <Empty>{t("log.requests.empty")}</Empty>
        ) : (
          <>
            <div style={{ marginTop: 8 }}>
              {shown.map((entry) => {
                const bad = entry.status >= 400;
                const children =
                  subagentByParent.byEntry.get(entry.id) ??
                  (entry.requestIdHash
                    ? (subagentByParent.byHash.get(entry.requestIdHash) ?? [])
                    : []);
                const childCount = children.length;
                return (
                  <div className="req" key={entry.id} id={`req-${entry.id}`}>
                    <span className="req__time">{clock(entry.createdAt)}</span>
                    <span className="req__who">
                      {entry.provider}
                      <span className="req__model">{entry.model}</span>
                    </span>
                    <span className="req__tok">
                      {tokens(entry.inputTokens)} → {tokens(entry.outputTokens)}
                      {cachedRatio(entry.cachedInputTokens, entry.inputTokens) != null ? (
                        <span className="req__cached">
                          {t("log.requests.cached", {
                            percent: cachedRatio(entry.cachedInputTokens, entry.inputTokens),
                          })}
                        </span>
                      ) : null}
                    </span>
                    <span className="req__dur">
                      {(entry.durationMs / 1000).toFixed(1)}s
                    </span>
                    <span
                      className={`req__status req__status--${bad ? "bad" : "ok"}`}
                    >
                      {entry.status}
                    </span>
                    {entry.streamQuality ||
                    entry.connectionId ||
                    entry.controlAccountHash ||
                    entry.executionAccountHash ||
                    childCount > 0 ? (
                      /* 品質徽章、connection、帳號 A·B、子代理數 —— 都是掃視時
                         偶爾才需要的短資訊，擠在同一個 flex 群組裡跟主要欄位共用
                         這一行，擠不下才換行；不再每一項各自另起一整行。 */
                      <span className="req__extra">
                        {entry.streamQuality ? (
                          <span
                            className={`req__quality req__quality--${entry.streamQuality}`}
                            title={t("log.requests.streamQualityTitle", {
                              quality: t(
                                `log.requests.streamQuality.${entry.streamQuality}`,
                              ),
                            })}
                          >
                            {t(`log.requests.streamQuality.${entry.streamQuality}`)}
                          </span>
                        ) : null}
                        {entry.connectionId ? (
                          <span className="req__meta" title={entry.connectionId}>
                            {t("log.requests.connection", { id: shortId(entry.connectionId) })}
                          </span>
                        ) : null}
                        {entry.controlAccountHash || entry.executionAccountHash ? (
                          <span
                            className="req__meta"
                            title={t("log.requests.accountIdentityTitle")}
                          >
                            {entry.controlAccountHash
                              ? `A ${shortId(entry.controlAccountHash.replace("sha256:", ""))}`
                              : "A —"}
                            {" · "}
                            {entry.executionAccountHash
                              ? `B ${shortId(entry.executionAccountHash.replace("sha256:", ""))}`
                              : "B —"}
                          </span>
                        ) : null}
                        {childCount > 0 ? (
                          <button
                            type="button"
                            className="req__meta req__meta--subagents"
                            onClick={() => jumpToChildren(children)}
                            title={t("log.requests.subagentChildrenTitle", { count: childCount })}
                          >
                            {t("log.requests.subagentChildren", { count: childCount })}
                          </button>
                        ) : null}
                      </span>
                    ) : null}
                    {/* 錯誤自己一行。上游的錯誤字串可以很長，塞回同一行就會撐爆。 */}
                    {entry.error ? (
                      <span className="req__err" title={entry.error}>
                        {entry.error}
                      </span>
                    ) : null}
                  </div>
                );
              })}
            </div>

            <Pager
              page={current}
              pageCount={pageCount}
              total={entryTotal}
              onPage={setPage}
            />

          </>
        )}
      </Card>

      {/* 壓縮／子代理／桌面呼叫原本各佔一張卡，新安裝或安靜的一天常常
          三張都是空狀態（各 231px），卻永遠佔在頁面上。這三個是同一件事
          的三種面向 —— Codex 執行期間發生的系統事件，不是三個獨立主題 ——
          合成一張卡用分頁切換，一次只顯示一種，空的那個只會是一小塊
          `<Empty>`，不會再吃掉一整張卡的高度。 */}
      <Card>
        <div className="rowline">
          <Cap>{t("log.systemEvents.title")}</Cap>
          <Segment
            options={[
              { value: "compaction", label: t("log.compaction.title") },
              { value: "subagent", label: t("log.subagent.title") },
              { value: "invokes", label: t("log.invokes.title") },
            ]}
            value={logFacet}
            onChange={(next) => {
              setLogFacet(next);
              setEventPage(1);
            }}
          />
        </div>
        <p className="rows__hint" style={{ marginTop: 4 }}>
          {t(`log.${logFacet}.note`, logFacet === "invokes" ? { count: invokes.length } : undefined)}
        </p>

        {logFacet === "compaction" ? (
        compactionEvents.length === 0 ? (
          <Empty>{t("log.compaction.empty")}</Empty>
        ) : (
          <div style={{ marginTop: 8 }}>
            {compactionEvents.slice(eventFrom, eventTo).map((event) => {
              const detailParts: string[] = [];
              if (event.itemsBefore != null && event.itemsAfter != null) {
                detailParts.push(
                  t("log.compaction.items", {
                    before: event.itemsBefore,
                    after: event.itemsAfter,
                  }),
                );
              }
              if (event.thresholdPercent != null) {
                detailParts.push(t("log.compaction.threshold", { percent: event.thresholdPercent }));
              }
              if (event.window != null && event.activeTokens != null) {
                detailParts.push(
                  t("log.compaction.window", {
                    active: tokens(event.activeTokens),
                    window: tokens(event.window),
                  }),
                );
              }
              // "codex_client" rows are parsed straight out of Codex's own
              // rollout text (compactions Codex itself performed, that
              // Vellum's proxy never saw as a compaction_trigger) — there is
              // no Vellum journal to report a checkpoint id for, so leaving
              // this line off here is accurate, not a gap.
              if (event.sourceModelVisibleTokens != null && event.replacementModelVisibleTokens != null) {
                detailParts.push(
                  `Model-visible: ${tokens(event.sourceModelVisibleTokens)} → ${tokens(event.replacementModelVisibleTokens)}${
                    event.replacementDurableTokens != null ? ` (durable: ${tokens(event.replacementDurableTokens)})` : ""
                  }`,
                );
              }
              if (event.fallbackReason) {
                detailParts.push(`Fallback: ${event.fallbackReason}`);
              }
              if (event.engine !== "codex_client") {
                detailParts.push(
                  event.checkpointId
                    ? t("log.compaction.checkpoint", { id: event.checkpointId, generation: event.generation ?? 1 })
                    : event.candidateGeneration != null
                      ? `Candidate Gen: ${event.candidateGeneration}`
                      : t("log.compaction.noCheckpoint"),
                );
              }
              return (
                /* 事件列的欄位跟請求列不一樣（沒有耗時，多一段明細），所以
                   用 `req--event` 換一套格線 —— 沿用請求列那六欄的話 outcome
                   會落進 4.2em 的耗時格、後兩欄整片留白，而明細沒有指定位置，
                   會被自動排位丟進下一列的第一欄（4.6em），一段話擠成一條直的。 */
                <div className="req req--event" key={`compaction-${event.id}`}>
                  <span className="req__time">{clock(event.createdAt)}</span>
                  <span className="req__who">{event.engine}</span>
                  <span className="req__tok">
                    {event.tokensBefore != null && event.tokensAfter != null
                      ? t("log.compaction.tokens", {
                          before: tokens(event.tokensBefore),
                          after: tokens(event.tokensAfter),
                        })
                      : "—"}
                  </span>
                  <span className="req__status req__status--ok">{event.outcome}</span>
                  {event.reason ? (
                    <span className="req__meta" title={event.reason}>
                      {event.reason}
                    </span>
                  ) : null}
                  {detailParts.length ? (
                    <span className="req__detail rows__hint">{detailParts.join(" · ")}</span>
                  ) : null}
                </div>
              );
            })}
          </div>
        )
        ) : null}

        {logFacet === "subagent" ? (
        <>
        {subagentSummary.total > 0 ? (
          <div className="subruns__stats" aria-label={t("log.subagent.summary.label")}>
            <span>{t("log.subagent.summary.total", { count: subagentSummary.total })}</span>
            <span className="subruns__stat--good">
              {t("log.subagent.summary.completed", { count: subagentSummary.completed })}
            </span>
            <span className="subruns__stat--warn">
              {t("log.subagent.summary.active", { count: subagentSummary.active })}
            </span>
            <span className="subruns__stat--bad">
              {t("log.subagent.summary.attention", { count: subagentSummary.attention })}
            </span>
          </div>
        ) : null}
        {subagentRuns.length === 0 ? (
          <Empty>{t("log.subagent.empty")}</Empty>
        ) : (
          <div className="subruns" style={{ marginTop: 8 }}>
            {subagentRuns.slice(eventFrom, eventTo).map((run, index) => {
              const steps = subagentTimeline(run, t);
              const runId = subagentRunId(run, index);
              const provider = run.routeId ? providerByRoute.get(run.routeId) : null;
              return (
                <details className="subrun" key={runId} id={`subrun-${runId}`}>
                  <summary className="subrun__summary">
                    <i className="subrun__chev" aria-hidden="true" />
                    <span className="req__time">{clock(run.requestedAt)}</span>
                    <span className="req__who">
                      {run.model ?? t("log.subagent.unknownModel")}
                      {run.effort ? <span className="req__model">{run.effort}</span> : null}
                    </span>
                    <span className="subrun__provider" title={run.routeId ?? undefined}>
                      {provider ?? run.routeId ?? "—"}
                    </span>
                    <span className="req__tok">
                      {run.durationMs != null ? `${(run.durationMs / 1000).toFixed(1)}s` : "—"}
                    </span>
                    <span className={`req__status ${runStatusClass(run.state)}`}>
                      {t(`log.subagent.state.${run.state}`)}
                    </span>
                  </summary>
                  <div className="subrun__body">
                    <div className="subrun__timeline">
                      {steps.map((step, stepIndex) => (
                        <span key={step.key} className="subrun__timeline-item">
                          {stepIndex > 0 ? (
                            <span className="subrun__arrow" aria-hidden="true">
                              {"→"}
                            </span>
                          ) : null}
                          <span className={`subrun__step subrun__step--${step.tone}`}>
                            {step.label}
                          </span>
                        </span>
                      ))}
                    </div>
                    <div className="subrun__meta">
                      {run.callId ? (
                        <span>{t("log.subagent.call", { id: shortId(run.callId) })}</span>
                      ) : null}
                      {run.childRequestHash ? (
                        run.childUsageEntryId != null ? (
                          <button
                            type="button"
                            className="subrun__locate"
                            onClick={() => jumpToEntry(run.childUsageEntryId as number)}
                            title={t("log.subagent.locateChild", {
                              id: shortHash(run.childRequestHash),
                            })}
                          >
                            {t("log.subagent.child", { id: shortHash(run.childRequestHash) })}
                          </button>
                        ) : (
                          <span>{t("log.subagent.child", { id: shortHash(run.childRequestHash) })}</span>
                        )
                      ) : null}
                      {run.parentRequestHash ? (
                        run.parentUsageEntryId != null ? (
                          <button
                            type="button"
                            className="subrun__locate"
                            onClick={() => jumpToEntry(run.parentUsageEntryId as number)}
                            title={t("log.subagent.locateParent", {
                              id: shortHash(run.parentRequestHash),
                            })}
                          >
                            {t("log.subagent.parent", { id: shortHash(run.parentRequestHash) })}
                          </button>
                        ) : (
                          <span>
                            {t("log.subagent.parent", { id: shortHash(run.parentRequestHash) })}
                          </span>
                        )
                      ) : null}
                      <span>
                        {t("log.subagent.requestedAt", { time: clockWithSeconds(run.requestedAt) })}
                      </span>
                      {run.completedAt != null ? (
                        <span>
                          {t("log.subagent.completedAt", { time: clockWithSeconds(run.completedAt) })}
                        </span>
                      ) : null}
                      <span>{t("log.subagent.linkLabel", { value: t(`log.subagent.link.${run.linkConfidence}`) })}</span>
                      {run.outcome ? (
                        <span>{t("log.subagent.outcome", { value: run.outcome })}</span>
                      ) : null}
                      {run.errorCategory ? (
                        <span className="req__err">
                          {t("log.subagent.error", { value: run.errorCategory })}
                        </span>
                      ) : null}
                    </div>
                  </div>
                </details>
              );
            })}
          </div>
        )}
        </>
        ) : null}

        {logFacet === "invokes" ? (
        invokeRows.length === 0 ? (
          <Empty>{t("log.invokes.empty")}</Empty>
        ) : (
          <div style={{ marginTop: 8 }}>
            {invokeRows
              .slice(eventFrom, eventTo)
              .map((entry, index) => (
                <div className="req" key={`${entry.at}-${entry.cmd}-${index}`}>
                  <span className="req__time">{clock(Math.floor(entry.at / 1000))}</span>
                  <span className="req__who">{entry.cmd}</span>
                  {entry.durationMs != null ? (
                    <span className="req__dur">{Math.round(entry.durationMs)}ms</span>
                  ) : null}
                  <span
                    className={`req__status req__status--${entry.ok ? "ok" : "bad"}`}
                  >
                    {entry.ok ? t("log.invokes.ok") : t("log.invokes.error")}
                  </span>
                  {!entry.ok && entry.error ? (
                    <span className="req__err" title={entry.error}>
                      {entry.error}
                    </span>
                  ) : null}
                </div>
              ))}
          </div>
        )
        ) : null}

        {/* 一個 Pager 給三種面向共用，位置與寫法都跟請求明細那張卡一致 ——
            同一個動作在同一頁上不該有兩種樣子。只有一頁時 Pager 自己不畫。 */}
        <Pager
          page={eventCurrent}
          pageCount={eventPageCount}
          total={eventTotal}
          onPage={setEventPage}
        />
      </Card>
    </>
  );
}

function shortId(value: string): string {
  return value.length > 8 ? value.slice(0, 8) : value;
}

/** 把 `sha256:<hex>` 短識別縮成前 8 個 hex 字元供 UI 顯示。 */
function shortHash(value: string): string {
  const hex = value.startsWith("sha256:") ? value.slice("sha256:".length) : value;
  return hex.slice(0, 8);
}

/** 一個 run 的穩定身份：有 callId 用它，否則用 child 的 usage 錨點，
    再退到 child hash，最後才退回 index（unlinked run）。 */
function subagentRunId(run: SubagentRun, index: number): string {
  if (run.callId) return run.callId;
  if (run.childUsageEntryId != null) return `entry-${run.childUsageEntryId}`;
  if (run.childRequestHash) return run.childRequestHash;
  return `unlinked-${index}`;
}

/**
 * Status colour for one run's badge. Only a genuinely `completed` run (the
 * joined child's own outcome said success) gets the affirmative colour —
 * every other state, including `failed`/`cancelled`, must read as
 * "not a plain success" so the log never lies about an in-flight, ambiguous,
 * unlinked, or failed spawn.
 */
function runStatusClass(state: SubagentRun["state"]): string {
  if (state === "completed") return "req__status--good";
  if (state === "failed" || state === "cancelled") return "req__status--bad";
  return "req__status--warn";
}

type SubagentTimelineStep = { key: string; label: string; tone: "done" | "pending" | "bad" | "neutral" };

/** The `requested → child → completed` timeline shown when a run row is
 * expanded — only the steps that actually happened (or are still pending)
 * are rendered, each tinted by whether it landed well or badly. */
function subagentTimeline(run: SubagentRun, t: Translate): SubagentTimelineStep[] {
  const steps: SubagentTimelineStep[] = [];
  if (run.callId) {
    steps.push({ key: "requested", label: t("log.subagent.timeline.requested"), tone: "neutral" });
  }
  if (run.childRequestHash) {
    const tone: SubagentTimelineStep["tone"] =
      run.state === "completed" ? "done" : run.state === "failed" || run.state === "cancelled" ? "bad" : "pending";
    steps.push({ key: "child", label: t("log.subagent.timeline.child"), tone });
  } else if (run.state === "ambiguous" || run.state === "unlinked") {
    steps.push({ key: "child", label: t("log.subagent.timeline.noChildLink"), tone: "bad" });
  }
  if (run.completedAt != null) {
    steps.push({
      key: "completed",
      label: t("log.subagent.timeline.completed"),
      tone: run.state === "completed" ? "done" : "bad",
    });
  } else if (run.state === "requested" || run.state === "running") {
    steps.push({ key: "completed", label: t("log.subagent.timeline.pending"), tone: "pending" });
  }
  return steps;
}

function cachedRatio(cached: number | undefined, input: number): number | null {
  if (cached == null || cached <= 0 || input <= 0) return null;
  return Math.round((cached / input) * 100);
}

/** 紀錄看的是「剛剛」，所以只給時分，不給日期。 */
function clock(epochSeconds: number): string {
  const d = new Date(epochSeconds * 1000);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

function clockWithSeconds(epochSeconds: number): string {
  const d = new Date(epochSeconds * 1000);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}:${String(d.getSeconds()).padStart(2, "0")}`;
}

/** Full local date/time for "last process start" (which may be days ago). */
function bootTime(epochSeconds: number): string {
  const d = new Date(epochSeconds * 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}/${pad(d.getMonth() + 1)}/${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}
