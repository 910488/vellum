/**
 * 視覺原語。刻意保持小而笨 —— 它們只負責把 token 變成畫面，
 * 不碰資料、不碰狀態。任何一個開始需要 useEffect，就代表它放錯地方了。
 */
import { useEffect, useId, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { tokens } from "@/lib/format";
import type { HeatColumn, HeatMode } from "@/lib/heatmap";
import type { Severity } from "@/types";

/* ---------- 卡片 ---------- */

/**
 * 四層表面。層級是刻意的：一頁上八張同重量的卡片，眼睛沒有入口。
 *   hero   最強暈染 + 最大留白 —— 每頁最多一張，放這頁的主角
 *   預設   暈染卡片
 *   tile   小塊暈染 —— 指標磚
 *   quiet  無暈染   —— 參考資訊，只靠間距分組
 */
export function Card({
  children,
  hero = false,
  tile = false,
  quiet = false,
  className = "",
  style,
}: {
  children: ReactNode;
  hero?: boolean;
  tile?: boolean;
  quiet?: boolean;
  className?: string;
  style?: CSSProperties;
}) {
  const level = hero ? " card--hero" : tile ? " card--tile" : quiet ? " card--quiet" : "";
  return (
    <div className={`card${level}${className ? ` ${className}` : ""}`} style={style}>
      {children}
    </div>
  );
}

export function Cap({ children }: { children: ReactNode }) {
  return <p className="card__cap">{children}</p>;
}

/* ---------- 光暈數字 ---------- */

export function Metric({
  value,
  unit,
  glow = "var(--lavender)",
  small = false,
}: {
  value: string | number;
  unit?: string;
  glow?: string;
  small?: boolean;
}) {
  return (
    <div
      className={`metric${small ? " metric--sm" : ""}`}
      style={{ ["--glow" as string]: glow }}
    >
      <span className="metric__value">{value}</span>
      {unit ? <span className="metric__unit">{unit}</span> : null}
    </div>
  );
}

/* ---------- 裝飾式分隔 ---------- */

export function Ruler({ style }: { style?: CSSProperties }) {
  return (
    <div className="ruler" style={style}>
      <i className="ruler__dot" />
    </div>
  );
}

/* ---------- 進度條 ---------- */

export function Meter({
  percent,
  tone = "default",
  markAt,
  markLabel,
}: {
  percent: number;
  tone?: "default" | "honey" | "coral";
  /** 在量條上畫一道刻度（0–100）。用來標「門檻在哪」，看得出還差多遠。 */
  markAt?: number;
  markLabel?: string;
}) {
  const clamped = Math.min(100, Math.max(0, percent));
  return (
    <div
      className={`meter${tone !== "default" ? ` meter--${tone}` : ""}`}
      role="progressbar"
      aria-valuenow={clamped}
      aria-valuemin={0}
      aria-valuemax={100}
    >
      <span className="meter__fill" style={{ width: `${clamped}%` }} />
      {markAt != null ? (
        <i
          className="meter__mark"
          style={{ left: `${Math.min(100, Math.max(0, markAt))}%` }}
          title={markLabel}
          aria-hidden="true"
        />
      ) : null}
    </div>
  );
}

/* ---------- 狀態 ----------
 * 三態（生效中／待生效／已停用）不用 pill，也不用任何記號。
 *
 * 第一版是膠囊 —— 每個 AI 生成的 dashboard 都長那樣，而且它把狀態做成了
 * 一個「元件」，但狀態其實是這一列的註記，不是一顆按鈕。
 * 第二版換成方形墨記 —— 還是一個圖示，而且方塊在水墨語彙裡太硬。
 *
 * 這一版不加任何形狀：狀態就是那兩三個字，底下拖一道筆畫，三態靠**筆觸**
 * 區分（濕的實筆／斷開的乾筆／快沒有的細線）。全部在 CSS 的 ::after 裡，
 * 所以這裡連一個多餘的 DOM 節點都不需要。
 *
 * 顏色只是輔助 —— 狀態永遠有文字，不靠顏色單獨表意。
 */

export function State({
  tone,
  label,
}: {
  tone: "ok" | "warn" | "quiet";
  label: string;
}) {
  return <span className={`state state--${tone}`}>{label}</span>;
}

/* ---------- pill ---------- */

type PillTone = "default" | "ok" | "warn" | "crit" | "quiet";

export function Pill({
  children,
  tone = "default",
  dot = false,
}: {
  children: ReactNode;
  tone?: PillTone;
  dot?: boolean;
}) {
  return (
    <span className={`pill${tone !== "default" ? ` pill--${tone}` : ""}`}>
      {dot ? <i className="pill__dot" /> : null}
      {children}
    </span>
  );
}

/* ---------- 狀態列 ---------- */

export function Rows({ children }: { children: ReactNode }) {
  return <div className="rows">{children}</div>;
}

export function Row({ label, children }: { label: ReactNode; children: ReactNode }) {
  return (
    <div className="rows__item">
      <span className="rows__key">{label}</span>
      <span className="rows__val">{children}</span>
    </div>
  );
}

/* ---------- 按鈕 ---------- */

export function Btn({
  children,
  onClick,
  soft = false,
  mini = false,
  danger = false,
  disabled = false,
  type = "button",
  title,
  autoFocus = false,
}: {
  children: ReactNode;
  onClick?: () => void;
  soft?: boolean;
  /** 表格列裡的動作。一般按鈕的高度會把列撐開，破壞掃視的節奏。 */
  mini?: boolean;
  /**
   * 不可逆的動作。實心珊瑚色是這套色票裡唯一不出現在別處的填色，
   * 所以它在一頁上永遠只代表一件事。
   */
  danger?: boolean;
  disabled?: boolean;
  type?: "button" | "submit";
  title?: string;
  autoFocus?: boolean;
}) {
  return (
    <button
      type={type}
      className={`btn${soft ? " btn--soft" : ""}${danger ? " btn--danger" : ""}${mini ? " btn--mini" : ""}`}
      onClick={onClick}
      disabled={disabled}
      title={title}
      autoFocus={autoFocus}
    >
      {children}
    </button>
  );
}

/* ---------- 通報 ----------
 * 三級：說明／注意／失敗。
 *
 * 這個語彙原本不存在 —— `.notice` 在三個樣式表裡一條規則都沒有，所以
 * 「正在讀取 SSH 設定」跟一段 Rust panic 字串長得一模一樣，兩者都是裸段落。
 * 要分辨哪個是壞消息，得先把整句話讀完。
 *
 * 三件事寫進結構裡，而不是交給呼叫端自己拼：
 *   tag   永遠有那兩個字。顏色是輔助，不單獨表意。
 *   acts  補救動作跟訊息同一塊 —— 「發生什麼事」跟「怎麼辦」分兩個地方放，
 *         等於要人自己接起來。
 *   raw   機器原字串收在最後。人話在上面，這一行是給要回報問題的人的。
 */

export function Notice({
  tone = "info",
  children,
  acts,
  raw,
}: {
  tone?: "info" | "warn" | "error";
  children: ReactNode;
  acts?: ReactNode;
  raw?: string | null;
}) {
  const { t } = useTranslation();
  return (
    <div
      className={`notice${tone !== "info" ? ` notice--${tone}` : ""}`}
      role={tone === "error" ? "alert" : "status"}
    >
      <span className="notice__tag">{t(`common.notice.${tone}`)}</span>
      <div className="notice__body">{children}</div>
      {acts ? <div className="notice__acts">{acts}</div> : null}
      {raw ? <p className="notice__raw">{raw}</p> : null}
    </div>
  );
}

/* ---------- 確認 ----------
 * 用原生 <dialog>，理由跟 Tray 用 <details> 一樣：焦點鎖、Esc、aria-modal
 * 與背景惰性化全部由瀏覽器做。自己接一定會漏掉其中一項。
 *
 * 換掉 window.confirm 不只是為了長相：它會鎖住 JS thread，背景操作的
 * 輪詢會跟著停在原地；而且它畫出來的是一個 Windows 系統視窗，跟這個 app
 * 沒有任何關係。
 *
 * 內容是固定三格的事實，不是一段話 —— 一段話的確認視窗沒人讀完，讀完
 * 也記不住哪一句是「不會動到」。
 *
 * danger 的差別不只有顏色：預設焦點給取消，而且要打出主機名稱才解鎖。
 * 這一顆的後果比整頁其他動作加起來還大，多一道摩擦順便逼你確認選對了哪一台。
 */

export function Confirm({
  open,
  title,
  facts,
  confirmLabel,
  danger = false,
  gate = null,
  onConfirm,
  onCancel,
}: {
  open: boolean;
  title: string;
  facts: { key: string; value: ReactNode }[];
  confirmLabel: string;
  danger?: boolean;
  gate?: { label: string; phrase: string } | null;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const { t } = useTranslation();
  const ref = useRef<HTMLDialogElement>(null);
  const titleId = useId();
  const [typed, setTyped] = useState("");

  useEffect(() => {
    const node = ref.current;
    if (!node) return;
    if (open && !node.open) node.showModal();
    if (!open && node.open) node.close();
  }, [open]);

  // 每次開啟都從空白開始。留著上一次打的字等於白做一道摩擦。
  useEffect(() => { if (open) setTyped(""); }, [open]);

  const unlocked = !gate || typed.trim() === gate.phrase;

  return (
    <dialog
      ref={ref}
      className="dialog"
      aria-labelledby={titleId}
      // Esc。<dialog> 自己會關，這裡只負責讓父層的狀態跟著一致。
      onCancel={onCancel}
    >
      <form
        className="dialog__form"
        method="dialog"
        // 只服務 gate 輸入框裡按 Enter。確認鈕刻意是 type="button" 直接接
        // onClick —— 讓它送出表單的話，click 與 submit 會各觸發一次 onConfirm。
        onSubmit={(event) => { event.preventDefault(); if (unlocked) onConfirm(); }}
      >
        <h2 className="dialog__title" id={titleId}>{title}</h2>
        <div className="dialog__facts">
          {facts.map((fact) => (
            <div key={fact.key} style={{ display: "contents" }}>
              <span className="dialog__fact-key">{fact.key}</span>
              <span className="dialog__fact-val">{fact.value}</span>
            </div>
          ))}
        </div>
        {gate ? (
          <label className="dialog__gate">
            <span>{gate.label}</span>
            <input
              value={typed}
              onChange={(event) => setTyped(event.target.value)}
              autoComplete="off"
              spellCheck={false}
            />
          </label>
        ) : null}
        <div className="dialog__acts">
          <Btn soft onClick={onCancel} autoFocus={danger}>{t("common.cancel")}</Btn>
          <Btn danger={danger} disabled={!unlocked} onClick={onConfirm}>{confirmLabel}</Btn>
        </div>
      </form>
    </dialog>
  );
}

/* ---------- 開關 ---------- */

export function Toggle({
  checked,
  onChange,
  label,
  disabled = false,
}: {
  checked: boolean;
  onChange: (next: boolean) => void;
  label: string;
  /** 扳動它會做一件還在進行中、或前置條件還沒到的事。 */
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      className="toggle"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={() => onChange(!checked)}
    />
  );
}

/* ---------- 分段選擇 ---------- */

export function Segment<T extends string>({
  options,
  value,
  onChange,
}: {
  options: { value: T; label: string }[];
  value: T;
  onChange: (next: T) => void;
}) {
  return (
    <div className="segment">
      {options.map((opt) => (
        <button
          key={opt.value}
          type="button"
          className="segment__opt"
          aria-pressed={opt.value === value}
          onClick={() => onChange(opt.value)}
        >
          {opt.label}
        </button>
      ))}
    </div>
  );
}

/* ---------- 審查發現 ---------- */

const SEVERITY_CLASS: Record<Severity, string> = {
  critical: "finding--crit",
  warning: "",
  info: "finding--low",
};

export function FindingCard({
  severity,
  title,
  meta,
}: {
  severity: Severity;
  title: string;
  meta: string;
}) {
  return (
    <div className={`finding ${SEVERITY_CLASS[severity]}`.trim()}>
      <span className="finding__stripe" />
      <div className="finding__body">
        <div className="finding__title">{title}</div>
        <div className="finding__meta">{meta}</div>
      </div>
    </div>
  );
}

/* ---------- 托盤 ----------
 * 用原生 <details>：展開／收合的可及性、鍵盤操作與 aria-expanded
 * 都由瀏覽器處理，不需要自己接 state，也不會做錯。
 */

export function Tray({
  label,
  count,
  defaultOpen = false,
  tone,
  children,
}: {
  label: ReactNode;
  /** 收起來時仍然要看得到數量 —— 不然使用者不知道裡面有沒有東西 */
  count?: ReactNode;
  defaultOpen?: boolean;
  /** 裡面的東西正在動。收起來的時候，標題列是唯一還說得出這件事的地方。 */
  tone?: "running";
  children: ReactNode;
}) {
  return (
    <details className={`tray${tone ? ` tray--${tone}` : ""}`} open={defaultOpen}>
      <summary className="tray__head">
        <i className="tray__chev" aria-hidden="true" />
        <span className="tray__label">{label}</span>
        {count !== undefined ? <span className="tray__count">{count}</span> : null}
      </summary>
      <div className="tray__body">{children}</div>
    </details>
  );
}

/* ---------- 固定欄位列 ----------
 * 「狀態記號 + 名稱 + 值」這種列，記號的寬度會隨字數變（已探測／需補充），
 * 用 flex 排就會讓後面每一欄跟著左右浮動，一堆列疊起來完全對不齊。
 *
 * 用 grid 把三欄寬度釘死，記號再怎麼變也不會推到名稱。探測結果、帳號清單
 * 都走這個 —— 同一個毛病在多個地方出現過，就該有一個共用解法。
 */

export function KV({
  mark,
  label,
  value,
}: {
  mark?: ReactNode;
  label: ReactNode;
  value: ReactNode;
}) {
  return (
    <div className="kv">
      <span className="kv__mark">{mark}</span>
      <span className="kv__label">{label}</span>
      <span className="kv__value">{value}</span>
    </div>
  );
}

/* ---------- 交接鏈 ----------
 * 有些東西只有在前一件事成立時才可能成立：Enhanced 的成品先驗證，才輪得到
 * 環境變數持有租約；租約先在，Codex Desktop 才可能認養那支 bridge；認養了，
 * 兩個子行程才會在正確的 digest 上就位。這種結構原本被畫成一份平的清單，
 * 於是「哪一步沒過」要自己從五個 boolean 反推。
 *
 * 這裡把它畫成一筆連續的墨線，而不是 stepper —— 沒有編號、沒有圓圈、沒有
 * 箭頭方塊。狀態由**筆觸**帶：完成的是濕的實筆，停住的那一節斷成乾筆，
 * 還沒輪到的收成快沒有的細線。這跟 `State` 的三態是同一套語彙，只是轉了
 * 九十度 —— 它們講的本來就是同一件事。
 *
 * 語意上仍然是 <ol>：順序是這個結構的全部意義，讀屏軟體該聽得到「第 3 項，
 * 共 4 項」。視覺上的編號才是範本家具，語意上的不是。
 */

export type SeamState = "done" | "open" | "pending";

export function Seam({ children, tight = false }: { children: ReactNode; tight?: boolean }) {
  /** 每一節都只有標題與結論、沒有證據時收緊行高 —— 五個布林值撐開五格
   *  一般節距，看起來像五件大事，實際上是一條線上的五個刻度。 */
  return <ol className={`seam${tight ? " seam--tight" : ""}`}>{children}</ol>;
}

export function SeamLink({
  state,
  label,
  mark,
  children,
}: {
  state: SeamState;
  label: ReactNode;
  /** 這一節的結論。跟 `Row` 一樣靠右 —— 靠右對齊不吃內容寬度，
   *  所以四個語言下都不會逐列各歪各的。 */
  mark?: ReactNode;
  /** 證據與阻擋原因。掛在它們所屬的那一節下面，不是全部倒在頁尾。 */
  children?: ReactNode;
}) {
  return (
    <li className={`seam__link seam__link--${state}`}>
      <span className="seam__head">
        <span className="seam__label">{label}</span>
        {mark ? <span className="seam__mark">{mark}</span> : null}
      </span>
      {children ? <span className="seam__body">{children}</span> : null}
    </li>
  );
}

/* ---------- 分頁 ----------
 * 紀錄是除錯用的，不是帳本 —— 一次塞幾百列只會讓人捲到手痠。
 */

export function Pager({
  page,
  pageCount,
  total,
  onPage,
}: {
  page: number;
  pageCount: number;
  total: number;
  onPage: (next: number) => void;
}) {
  const { t } = useTranslation();
  if (pageCount <= 1) return null;
  return (
    <div className="pager">
      <Btn soft disabled={page <= 1} onClick={() => onPage(page - 1)}>
        {t("common.previousPage")}
      </Btn>
      <span className="pager__at">
        {t("common.pageOf", { page, total: pageCount })}
      </span>
      <span className="pager__total">{t("common.totalItems", { count: total })}</span>
      <Btn soft disabled={page >= pageCount} onClick={() => onPage(page + 1)}>
        {t("common.nextPage")}
      </Btn>
    </div>
  );
}

/* ---------- 用量熱區圖 ----------
 * 一欄一週、一列一個星期幾。三種模式共用同一個格陣：
 *   每日  一格一天，深淺 = 那天的量
 *   每週  由下往上填的格數 = 那週的量
 *   累計  填到當週為止的累積量
 *
 * 共用格陣是刻意的：切換時格子位置不動、只有填法變，眼睛不用重新找基準。
 * 換模式時整張圖跑一次由左至右的波浪 —— 那不只是裝飾，它讓人看見
 * 「同一批格子被重新解讀了一次」，而不是以為換了一張圖。
 *
 * 資料整形全在 lib/heatmap.ts（純函式、有測試），這裡只負責畫。
 */

export function Heatmap({
  columns,
  mode,
  hovered,
  hoveredDate,
  onHover,
}: {
  columns: HeatColumn[];
  mode: HeatMode;
  hovered: HeatColumn | null;
  hoveredDate: string | null;
  onHover: (column: HeatColumn | null, dayDate: string | null) => void;
}) {
  const { t } = useTranslation();
  if (!columns.length) return null;

  const hoverIndex = hovered ? columns.findIndex((c) => c.key === hovered.key) : -1;
  const hoveredDay =
    hoveredDate && hovered
      ? hovered.cells.find((cell) => cell.day?.date === hoveredDate)?.day
      : null;
  const hoveredTokens =
    mode === "daily" && hoveredDay
      ? hoveredDay.inputTokens + hoveredDay.outputTokens
      : (hovered?.total ?? 0);

  return (
    <div
      className="heat"
      aria-label={t("heatmap.aria")}
      onMouseLeave={() => onHover(null, null)}
      style={{ ["--heat-columns" as string]: columns.length }}
    >
      <div className="heat__body">
        {hovered && hoverIndex >= 0 ? (
          <div
            className="heat__tooltip"
            style={{
              left: `${Math.min(
                94,
                Math.max(6, ((hoverIndex + 0.5) / columns.length) * 100),
              )}%`,
            }}
          >
            <span>
              {mode === "daily" && hoveredDate
                ? hoveredDate
                : hovered.from === hovered.to
                  ? hovered.from
                  : `${hovered.from} – ${hovered.to}`}
            </span>
            <strong>{t("heatmap.tokens", { value: tokens(hoveredTokens) })}</strong>
          </div>
        ) : null}

        {/* key 綁 mode：換模式時整個格陣重新掛載，波浪才會重跑 */}
        <div
          className="heat__grid"
          key={mode}
          role="img"
          aria-label={t("heatmap.gridAria", { mode: t(`heatmap.mode.${mode}`) })}
        >
          {columns.map((col, ci) =>
            col.cells.map((cell, ri) => (
              <span
                key={`${col.key}-${ri}`}
                className={`heat__cell${cell.day ? "" : " heat__cell--void"}`}
                data-level={cell.level}
                data-on={
                  mode === "daily"
                    ? Boolean(
                        hoveredDate &&
                          cell.day?.date === hoveredDate &&
                          ci === hoverIndex,
                      )
                    : ci === hoverIndex
                }
                style={{ ["--col" as string]: ci }}
                onMouseEnter={() =>
                  mode === "daily" && !cell.day
                    ? onHover(null, null)
                    : onHover(col, cell.day?.date ?? null)
                }
              />
            )),
          )}
        </div>

        {/* 月份軸。只在該月的第一欄標，逐欄標會比格子還吵。 */}
        <div className="heat__months" aria-hidden="true">
          {columns.map((col, ci) => (
            <span key={col.key} style={{ ["--col" as string]: ci }}>
              {col.monthLabel ? t("heatmap.monthLabel", { month: col.monthLabel }) : null}
            </span>
          ))}
        </div>
      </div>
    </div>
  );
}

/* ---------- 空狀態 ---------- */

export function Empty({ glyph = "◌", children }: { glyph?: string; children: ReactNode }) {
  return (
    <div className="empty">
      {/* 符號不帶資訊 —— 意思在下面那句話裡，所以對讀屏軟體隱藏 */}
      <span className="empty__glyph" aria-hidden="true">
        {glyph}
      </span>
      {children}
    </div>
  );
}
