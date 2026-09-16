import { useLocaleFormat } from "@/i18n/useLocaleFormat";
import { useTranslation } from "react-i18next";
/**
 * 上下文 —— Enhanced Codex core 的每個對話，離被壓縮還有多遠。
 *
 * 這一頁只講一件事：**哪一個對話快滿了，還有它上次被拿走了什麼**。
 *
 * 先前這裡的主角是「一次最新壓縮」，一個沒有主人的事件：使用者同時開著好
 * 幾個 Codex 視窗，每個視窗是獨立的上下文，看到一組數字卻不知道是誰的，
 * 就沒辦法據此做任何決定。所以改成先選對話、再看它身上發生的事。
 *
 * 「設定最大上下文長度」不在這裡了。那是模型的屬性，不是對話的狀態，
 * 現在寫在「模型」頁每個模型旁邊的鉛筆。這一頁只讀不寫。
 *
 * 逐項的壓縮內容明細不在這裡了。它回答的是「這一次壓縮動了哪幾則訊息」——
 * 沒有人是為了這件事打開這一頁的，而它把整頁往下推了很長一段。
 *
 * 「壓縮引擎」「Reasoning 延續」「跨工作階段延續」那一排也不在了。那三個
 * 欄位講的是已經廢掉的做法，而畫面上一個廢案看起來跟一個壞掉的功能一模一樣
 * ——「尚未建立 portable window」讀起來像是有東西沒設好，其實是根本不會有。
 *
 * 現在最下面是這次壓縮的原文：送出去的指示，以及模型寫回來的結果。壓縮真正
 * 讓人不安的是「它到底拿走了什麼、留下了什麼」，而那個答案只有在原文裡。
 */
import { useEffect, useState } from "react";
import { api } from "@/lib/api";
import { startVisiblePoll } from "@/lib/visiblePoll";
import { Cap, Card, Empty, Metric } from "@/components/ui";
import { tokens } from "@/lib/format";
import {
  isEnhancedCoreSession,
  isLive,
  matchesSessionQuery,
  orderByRecency,
  sessionLabel,
  sessionPercent,
} from "@/lib/sessions";
import type {
  CompactionPreview,
  CompactionTranscript,
  SessionStatus,
} from "@/types";

/**
 * 清單一次放幾個。
 *
 * 五個。上面那幾個是你這半天真的在做的事；再往下就是你已經忘了的對話，
 * 而它們只會讓你更難在這一列裡找到要找的那一個。翻更舊的用搜尋 ——
 * 那時候你心裡已經有一個名字了，清單幫不上忙。
 */
const RECENT_SESSIONS = 5;

export function Context({
  refreshVersion,
  onRefreshComplete,
  active = true,
}: {
  refreshVersion: number;
  onRefreshComplete: (version: number) => void;
  active?: boolean;
}) {
  const { t } = useTranslation();
  const { exact } = useLocaleFormat();
  const [sessions, setSessions] = useState<SessionStatus[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  /* 清單的搜尋字串。只影響清單，不影響下面已經選中的那個對話的明細。 */
  const [query, setQuery] = useState("");
  const [preview, setPreview] = useState<CompactionPreview | null>(null);
  /* 這次壓縮的原文：送出去的指示，與模型寫回來的結果。 */
  const [transcript, setTranscript] = useState<CompactionTranscript | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [compactError, setCompactError] = useState<string | null>(null);
  /* 滑過長條哪一段。null = 沒指到任何一段，讀數列顯示總計。 */
  const [hoveredSeg, setHoveredSeg] = useState<number | null>(null);
  /* 「還活著嗎」要靠現在時間才判斷得出來，所以它得跟著時間走，
     不能只在載入時算一次 —— 否則放著不動的畫面會一直說「活躍中」。 */
  const [nowSeconds, setNowSeconds] = useState(() => Math.floor(Date.now() / 1000));

  useEffect(() => {
    let alive = true;
    void (async () => {
      try {
        const next = await api.listSessions();
        if (!alive) return;
        setSessions(next);
        setError(null);
      } catch (cause) {
        if (alive) setError(t("context.errors.partialRefresh", { detail: String(cause) }));
      } finally {
        if (alive) {
          setLoading(false);
          if (refreshVersion > 0) onRefreshComplete(refreshVersion);
        }
      }
    })();
    return () => {
      alive = false;
    };
  }, [refreshVersion]);

  /* `ordered` 是這顆 core 上所有的對話，`shown` 是清單上真的畫出來的那些。
     兩個都要留著：空狀態要分得出「什麼都沒在跑」和「搜尋沒命中」，
     那是兩件完全不同的事，混成一句話會讓人以為工作階段不見了。

     搜尋結果也維持五筆上限，讓這一區永遠只是一個短清單。 */
  const ordered = orderByRecency(sessions.filter(isEnhancedCoreSession));
  const matched = ordered.filter((session) => matchesSessionQuery(session, query));
  const shown = matched.slice(0, RECENT_SESSIONS);
  /* 選擇是「意圖」，不是「索引」：清單會隨著壓力重新排序，記住 id 才不會
     在背景刷新後把使用者換到另一個對話上。選中的對話消失時退回最急的那個。

     刻意在 `ordered` 裡找而不是 `shown`：搜尋是清單的事，不是「你正在看哪個
     對話」的事。對 `shown` 解析的話，打字打到一半那些還沒命中的中間狀態
     會把下面整段明細卸載再重抓一次 —— 每一個按鍵都閃一次。 */
  const selected =
    ordered.find((session) => session.id === selectedId) ?? ordered[0] ?? null;

  useEffect(() => {
    if (!selected) {
      setPreview(null);
      setTranscript(null);
      return;
    }
    let alive = true;
    void (async () => {
      try {
        const [next, text] = await Promise.all([
          api.getCompactionPreview(selected.id),
          api.getCompactionTranscript(selected.id),
        ]);
        if (!alive) return;
        setPreview(next);
        setTranscript(text);
        setCompactError(null);
      } catch (cause) {
        if (alive) setCompactError(String(cause));
      }
      /* 換對話時把游標歸零：舊的段落索引套到新的段落上，讀數列會講出
         跟長條不一樣的話。 */
      setHoveredSeg(null);
    })();
    return () => {
      alive = false;
    };
  }, [selected?.id]);

  useEffect(() => {
    return startVisiblePoll({
      active,
      intervalMs: 60_000,
      load: async () => {
        setNowSeconds(Math.floor(Date.now() / 1000));
        const sessionsLoad = api
          .listSessions()
          .then(setSessions)
          .catch((cause) => setError(String(cause)));
        let detailLoad: Promise<unknown> = Promise.resolve();
        if (selected) {
          detailLoad = Promise.all([
            api.getCompactionPreview(selected.id),
            api.getCompactionTranscript(selected.id),
          ])
            .then(([next, text]) => {
              setPreview(next);
              setTranscript(text);
              setCompactError(null);
            })
            .catch((cause) => setCompactError(String(cause)));
        }
        await Promise.all([sessionsLoad, detailLoad]);
      },
    });
  }, [selected?.id, refreshVersion, active]);

  const head = (
    <div className="canvas__head">
      <div>
        <p className="eyebrow">{t("navigation.context.label")}</p>
        <h2 className="canvas__title">{t("context.sessions.title")}</h2>
        <p className="note" style={{ marginTop: 8 }}>
          {t("context.sessions.hint")}
        </p>
      </div>
    </div>
  );

  if (loading) {
    return (
      <>
        {head}
        <Card>
          <Empty>{t("context.sessions.loading")}</Empty>
        </Card>
      </>
    );
  }

  if (!ordered.length) {
    return (
      <>
        {head}
        <Card>
          <Empty>{error ?? t("context.sessions.empty")}</Empty>
        </Card>
      </>
    );
  }

  return (
    <>
      {head}

      {/* 清單本身就是主軸，所以它是一張完整的卡，不是側欄。
          排序照「離門檻多近」，最急的在最上面 —— 這一頁的問題就是
          「誰快掉東西」，排序必須直接回答它，而不是要人自己比對。 */}
      <Card>
        <div className="rowline">
          <Cap>{t("context.sessions.listTitle")}</Cap>
          {/* 畫出來的比實際有的少時就說出來 —— 不論是被搜尋濾掉的，還是被
              「最近幾個」擋在外面的。只印總數的話，數字會跟眼前的列數對不上，
              而使用者第一個念頭會是「不見的那兩個是不是掛了」。 */}
          <span className="rows__hint">
            {shown.length < ordered.length
              ? t("context.sessions.countFiltered", {
                  shown: shown.length,
                  total: ordered.length,
                })
              : t("context.sessions.count", { count: ordered.length })}
          </span>

          {/* 搜尋永遠在。只在超過幾個工作階段時才長出來的話，它就變成一個
              有時在、有時不在的東西 —— 沒辦法養成習慣，要用的時候還得先找。 */}
          <span className="sessionsearch">
            <input
              className="input sessionsearch__field"
              type="search"
              value={query}
              placeholder={t("context.sessions.search")}
              aria-label={t("context.sessions.search")}
              onChange={(event) => setQuery(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Escape") setQuery("");
              }}
            />
            {query ? (
              <button
                type="button"
                className="sessionsearch__clear"
                aria-label={t("context.sessions.searchClear")}
                onClick={() => setQuery("")}
              >
                ×
              </button>
            ) : null}
          </span>
        </div>

        <div className="watch watch--picklist">
          {shown.length === 0 ? (
            <Empty>{t("context.sessions.noMatch", { query: query.trim() })}</Empty>
          ) : null}
          {shown.map((session) => {
            const pct = sessionPercent(session);
            const gap = session.compactThresholdPercent - pct;
            const picked = selected?.id === session.id;
            return (
              <button
                type="button"
                className="watch__row watch__row--pick"
                key={session.id}
                data-serving={isLive(session, nowSeconds)}
                data-picked={picked}
                aria-pressed={picked}
                aria-label={t("context.sessions.pick", {
                  label: sessionLabel(session, t("context.sessions.untitled")),
                })}
                onClick={() => setSelectedId(session.id)}
              >
                <div className="watch__who">
                  <span className="watch__name">
                    {sessionLabel(session, t("context.sessions.untitled"))}
                  </span>
                  <span className="rows__hint">{session.provider}</span>
                </div>

                <div className="watch__models">
                  <code className="watch__model">{session.model}</code>
                </div>

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

                {/* 距離門檻還有幾個百分點 —— 這是這一頁唯一需要「比較」的
                    數字，所以把它算好放出來，不要留給使用者心算。
                    已經越過門檻的說「隨時」，不要印負數。 */}
                <div className="watch__act">
                  <span className="rows__hint">
                    {gap > 0
                      ? t("context.sessions.headroom", { percent: gap })
                      : t("context.sessions.imminent")}
                  </span>
                </div>
              </button>
            );
          })}
        </div>
      </Card>

      <div className="canvas__head" style={{ marginTop: 8 }}>
        <div>
          <p className="eyebrow">{t("context.compactionEyebrow")}</p>
          {/* 這裡用完整標題，不用 `sessionLabel` 的截短版：清單那一欄很窄所以
              要截，標題這一行整排都是它的，沒有理由再截一次。 */}
          <h2 className="canvas__title">
            {selected
              ? selected.label?.trim() || t("context.sessions.untitled")
              : t("context.compactionTitle")}
          </h2>
          <p className="note" style={{ marginTop: 8 }}>
            {t("context.compactionHint")}
          </p>
        </div>
      </div>

      {preview && preview.engine !== "pending" ? (
        <Card>
          {/* 壓縮最讓人不安的是「我不知道它拿走了什麼」，
              所以主角是前後對照，不是設定選項。 */}
          <div className="beforeafter">
            <div className="beforeafter__col">
              <p className="eyebrow" style={{ marginBottom: 12 }}>{t("context.before")}</p>
              {/* 長度一律由 token 數決定，不用百分比欄位 —— 百分比在前後兩組
                  是不同尺度的，拿來畫長度會讓圖說出跟數字不一樣的話。 */}
              <div className="stackbar" data-hovered={hoveredSeg !== null}>
                {preview.segments.map((s, i) => (
                  <button
                    key={s.label}
                    type="button"
                    className="stackbar__seg"
                    data-active={hoveredSeg === i}
                    style={{
                      flexGrow: segBefore(s),
                      background: s.tone,
                    }}
                    aria-label={t("context.segmentAria.before", { label: segmentLabel(s, t), tokens: tokens(segBefore(s)) })}
                    onMouseEnter={() => setHoveredSeg(i)}
                    onMouseLeave={() => setHoveredSeg(null)}
                    onFocus={() => setHoveredSeg(i)}
                    onBlur={() => setHoveredSeg(null)}
                  />
                ))}
              </div>
              <div style={{ marginTop: 18 }}>
                <Metric value={tokens(preview.beforeTokens)} glow="var(--coral)" small />
              </div>
            </div>

            <div className="beforeafter__arrow" aria-hidden="true">→</div>

            <div className="beforeafter__col">
              <p className="eyebrow" style={{ marginBottom: 12 }}>{t("context.after")}</p>
              {/* 前後使用相同欄寬，讓每個狀態都能辨識；總量差異由下方 token
                  數字呈現，避免高壓縮率時整條縮成幾個像素而看不到內容。 */}
              {/* 壓縮後那條必須「真的比較短」。
                  兩欄同寬，所以要把這條的 width 按 afterTokens/beforeTokens 縮；
                  不縮的話兩條一樣長，圖等於在說「什麼都沒少」，跟底下
                  412k → 147k 的數字直接矛盾。 */}
              <div
                className="stackbar"
                data-hovered={hoveredSeg !== null}
                style={{
                  width: preview.beforeTokens && preview.afterTokensExact
                    ? `${(preview.afterTokens / preview.beforeTokens) * 100}%`
                    : "100%",
                }}
              >
                {preview.segments.map((s, i) => (
                  <button
                    key={s.label}
                    type="button"
                    className="stackbar__seg"
                    data-active={hoveredSeg === i}
                    style={{
                      flexGrow: segAfter(s),
                      background: s.tone,
                    }}
                    aria-label={t("context.segmentAria.after", { label: segmentLabel(s, t), tokens: tokens(segAfter(s)) })}
                    onMouseEnter={() => setHoveredSeg(i)}
                    onMouseLeave={() => setHoveredSeg(null)}
                    onFocus={() => setHoveredSeg(i)}
                    onBlur={() => setHoveredSeg(null)}
                  />
                ))}
              </div>
              <div style={{ marginTop: 18 }}>
                <Metric
                  value={preview.afterTokensExact ? tokens(preview.afterTokens) : t("context.opaque")}
                  glow="var(--sage)"
                  small
                />
              </div>
            </div>
          </div>

          <div className="stackbar-legend" aria-label={t("context.legendAria")}>
            {preview.segments.map((segment, index) => (
              <button
                key={segment.label}
                type="button"
                className="stackbar-legend__item"
                data-active={hoveredSeg === index}
                onMouseEnter={() => setHoveredSeg(index)}
                onMouseLeave={() => setHoveredSeg(null)}
                onFocus={() => setHoveredSeg(index)}
                onBlur={() => setHoveredSeg(null)}
              >
                <span
                  className="stackbar-legend__swatch"
                  style={{ background: segment.tone }}
                />
                <span className="stackbar-legend__label">{segmentLabel(segment, t)}</span>
                <span className="stackbar-legend__value">
                  {tokens(segBefore(segment))} → {tokens(segAfter(segment))}
                </span>
              </button>
            ))}
          </div>

          {/* 讀數列。滑過或 Tab 到任一段就顯示那一段的明細；沒指到任何一段時
              顯示總計，所以這一列永遠有內容、高度固定，不會讓版面跳動。
              做成讀數列而不是浮動 tooltip：不會被卡片邊緣裁掉、鍵盤也拿得到。 */}
          <SegmentReadout preview={preview} index={hoveredSeg} />
        </Card>
      ) : (
        <Card>
          <Empty>{preview ? t("context.awaitingCompaction") : t("context.previewLoading")}</Empty>
        </Card>
      )}

      <TranscriptCard transcript={transcript} />

      <p className="note">
        {compactError ? `${compactError} · ` : ""}
        {error ? `${error} · ` : ""}
        {t("context.footerNote")}
      </p>
    </>
  );
}

/**
 * 段名照 `kind` 查，不用後端給的 `label`。
 *
 * 後端那個欄位自己就寫著「presentation only」，但它是在後端組的字串 ——
 * 舊紀錄裡存的是繁體中文，英文介面照樣把「工具續作狀態」印出來。
 * 認不得的 kind（更舊的 journal）才退回 label，總比空白好。
 */
function segmentLabel(
  segment: { kind?: string; label: string },
  t: (key: string, options?: Record<string, unknown>) => string,
): string {
  switch (segment.kind) {
    case "canonical_checkpoint":
      return t("context.segmentLabel.canonical");
    case "readable_reasoning":
      return t("context.segmentLabel.reasoning");
    case "tool_state":
      return t("context.segmentLabel.tool");
    case "retained_context":
      return t("context.segmentLabel.retained");
    case "observed_total":
      return t("context.segmentLabel.observedTotal");
    default:
      return segment.label;
  }
}

function segmentNote(kind: string, keepTurns: number, t: (key: string, options?: Record<string, unknown>) => string): string {
  if (kind === "canonical_checkpoint") {
    return t("context.segmentNote.canonical");
  }
  if (kind === "readable_reasoning") {
    return t("context.segmentNote.reasoning");
  }
  if (kind === "tool_state") {
    return t("context.segmentNote.tool");
  }
  if (kind === "retained_context") {
    return t("context.segmentNote.retained", { turns: keepTurns });
  }
  if (kind === "observed_total") {
    return t("context.segmentNote.observedTotal");
  }
  return t("context.segmentNote.default");
}

/**
 * 某一段實際佔多少 token —— 直接讀後端給的數字。
 *
 * 之前這裡是用長條的相對長度按比例回推總數，結果「完整保留」的那一段被算成
 * 少了 67k：讀數列一邊寫「完整保留，不動」一邊寫「省下 67k」。
 * 畫多長跟是多少是兩件事，不能互相推導。
 */
function segTokens(
  preview: CompactionPreview,
  index: number,
  side: "before" | "after",
): number {
  const seg = preview.segments[index];
  if (!seg) return 0;
  return side === "before" ? segBefore(seg) : segAfter(seg);
}

/* Rust 端只送 before/after（已經是同一把尺上的絕對 token 量），
   beforeTokens/afterTokens 只有 mock 有。兩邊都要能吃，而且**絕對不能**
   直接讀那兩個選用欄位去算長度 —— 實機會拿到 undefined，flexGrow 變 0，
   整條色條就空掉。 */
type Seg = CompactionPreview["segments"][number];
export const segBefore = (s: Seg): number => s.beforeTokens ?? s.before;
export const segAfter = (s: Seg): number => s.afterTokens ?? s.after;

/** 長條下方的讀數列。沒指到任何一段時顯示總計 —— 這一列永遠有內容。 */
function SegmentReadout({
  preview,
  index,
}: {
  preview: CompactionPreview;
  index: number | null;
}) {
  const { t } = useTranslation();
  const seg = index === null ? null : preview.segments[index];

  if (!seg || index === null) {
    if (!preview.afterTokensExact) {
      return (
        <div className="readout readout--idle">
          <span className="readout__swatch" style={{ background: "var(--ink-ghost)" }} />
          <span className="readout__label">{t("context.readout.total")}</span>
          <span className="readout__stat">
            {t("context.readout.officialStat", { before: tokens(preview.beforeTokens) })}
          </span>
          <span className="readout__note">
            {t("context.readout.officialNote")}
          </span>
        </div>
      );
    }
    const saved = preview.beforeTokens - preview.afterTokens;
    const pct = preview.beforeTokens
      ? Math.round((saved / preview.beforeTokens) * 100)
      : 0;
    return (
      <div className="readout readout--idle">
        <span className="readout__swatch" style={{ background: "var(--ink-ghost)" }} />
        <span className="readout__label">{t("context.readout.total")}</span>
        <span className="readout__stat">
          {tokens(preview.beforeTokens)} → {tokens(preview.afterTokens)}
        </span>
        <span className="readout__note">
          {t("context.readout.saved", { saved: tokens(saved), percent: pct })} · {t("context.readout.hover")}
        </span>
      </div>
    );
  }

  const before = segTokens(preview, index, "before");
  const after = segTokens(preview, index, "after");
  const share = preview.beforeTokens
    ? Math.round((before / preview.beforeTokens) * 100)
    : 0;
  const delta = before - after;

  return (
    <div className="readout">
      <span className="readout__swatch" style={{ background: seg.tone }} />
      <span className="readout__label">{segmentLabel(seg, t)}</span>
      <span className="readout__stat">
         {t("context.readout.tokenTransition", { before: tokens(before), after: tokens(after) })}
      </span>
      <span className="readout__note">
        {t("context.readout.share", { percent: share })}
        {/* delta 為 0 時不再多寫一句「完整保留」—— 下面的 segmentNote 已經
            在講同一件事，兩句連在一起會變成「完整保留…·完整保留…」。 */}
        {delta > 0 ? ` · ${t("context.readout.savedDelta", { tokens: tokens(delta) })}` : ""}
        {" · "}
        {segmentNote(seg.kind ?? "retained_context", preview.keepRecentTurns, t)}
      </span>
    </div>
  );
}

/* ---------- 這次壓縮的原文 ---------- */

/**
 * 送出去的指示，與模型寫回來的結果。
 *
 * 上面那張前後對照卡回答「少了多少」，這一張回答「留下的到底是什麼」。
 * 兩件事都只有原文答得了：摘要的品質不是 token 數看得出來的，而使用者對
 * 壓縮的不安，從來就不是「數字對不對」。
 *
 * 上下流，不是左右並排 —— 指示在前、結果在後，本來就有先後；並排時兩邊
 * 高度差很大，中間會空掉一大片。
 */
function TranscriptCard({ transcript }: { transcript: CompactionTranscript | null }) {
  const { t } = useTranslation();
  return (
    <>
      <div className="canvas__head" style={{ marginTop: 8 }}>
        <div>
          <p className="eyebrow">{t("context.transcript.eyebrow")}</p>
          <h2 className="canvas__title">{t("context.transcript.title")}</h2>
          <p className="note" style={{ marginTop: 8 }}>
            {t("context.transcript.hint")}
          </p>
        </div>
      </div>
      <Card>
        {transcript?.prompt || transcript?.result ? (
          <div className="transcript">
            <TranscriptBlock
              label={t("context.transcript.prompt")}
              note={t("context.transcript.promptNote")}
              body={transcript.prompt}
              missing={t("context.transcript.noPrompt")}
            />
            <TranscriptBlock
              label={t("context.transcript.result")}
              note={t("context.transcript.resultNote")}
              body={transcript.result}
              missing={t("context.transcript.noResult")}
            />
          </div>
        ) : (
          /* 讀不到就說為什麼。空的 <pre> 看起來跟壞掉一模一樣。 */
          <Empty>
            {transcript?.unavailableReason
              ? t(`context.transcript.unavailable.${transcript.unavailableReason}`, {
                  defaultValue: transcript.unavailableReason,
                })
              : t("context.transcript.empty")}
          </Empty>
        )}
      </Card>
    </>
  );
}

function TranscriptBlock({
  label,
  note,
  body,
  missing,
}: {
  label: string;
  note: string;
  body: string | null;
  missing: string;
}) {
  const { t } = useTranslation();
  return (
    <section className="transcript__block">
      <div className="rowline">
        <Cap>{label}</Cap>
        {/* 有字數才印字數。沒有內容時印「0 字」只是在強調空白。
            單位要寫出來：這一頁到處都是 token 數，一個裸數字會被讀成 token。
            `value` 不叫 `count`，否則 i18next 會去找 `_one`/`_other`。 */}
        {body ? (
          <span className="rows__hint">
            {t("context.transcript.chars", {
              value: Array.from(body).length.toLocaleString(),
            })}
          </span>
        ) : null}
      </div>
      <p className="note" style={{ marginTop: 6 }}>
        {note}
      </p>
      {body ? (
        /* 原文照原樣印。這裡的換行與縮排是 prompt 的一部分，
           重新排版等於改了它。太長時這一塊自己捲，不是整頁跟著長。 */
        <pre className="transcript__text">{body}</pre>
      ) : (
        <Empty>{missing}</Empty>
      )}
    </section>
  );
}
