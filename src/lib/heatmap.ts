/**
 * 從請求紀錄堆出每日用量。
 *
 * 為什麼在前端算而不是加一個 Rust 指令：接線是使用者那邊的範圍，前端不該
 * 偷加沒註冊的 command（`tests/api-contract.test.ts` 會擋，而且那個測試是對的）。
 * 紀錄本來就帶 createdAt 與 token 數，堆一下就有了。
 *
 * 代價要講清楚：**能畫多長取決於紀錄保留多久**。紀錄只留最近 N 筆，所以
 * 熱區圖看得到的歷史就到那裡為止。真的要看一整年，後端得另外存每日彙總 ——
 * 那是 aggregate，不是 log。
 */
import type {
  RequestLogEntry,
  UsageActivityDay,
  UsageDay,
  UsageHeatmap,
} from "@/types";

/** 本地時區的 YYYY-MM-DD。用 UTC 會讓跨日的請求算到前一天。 */
function localDate(epochSeconds: number): string {
  const d = new Date(epochSeconds * 1000);
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(
    d.getDate(),
  ).padStart(2, "0")}`;
}

function addDays(base: Date, delta: number): Date {
  const d = new Date(base);
  d.setDate(d.getDate() + delta);
  return d;
}

/**
 * @param entries 請求紀錄（順序不拘）
 * @param maxDays 最多回溯幾天。
 * @param fillRange 是否固定補滿 maxDays；年度 HeatMap 需要固定時間軸，
 *   避免資料較少時整張圖縮到右側。
 */
export function buildHeatmap(
  entries: RequestLogEntry[],
  maxDays = 182,
  fillRange = false,
): UsageHeatmap {
  const byDate = new Map<string, UsageDay>();
  let totalInputTokens = 0;
  let totalOutputTokens = 0;

  for (const entry of entries) {
    const date = localDate(entry.createdAt);
    const day = byDate.get(date) ?? {
      date,
      inputTokens: 0,
      outputTokens: 0,
      requests: 0,
      failedRequests: 0,
    };
    day.inputTokens += entry.inputTokens;
    day.outputTokens += entry.outputTokens;
    day.requests += 1;
    if (entry.status >= 400) day.failedRequests += 1;
    byDate.set(date, day);
    totalInputTokens += entry.inputTokens;
    totalOutputTokens += entry.outputTokens;
  }

  if (!byDate.size) {
    return {
      days: [],
      totalInputTokens: 0,
      totalOutputTokens: 0,
      totalRequests: 0,
      maxDayTokens: 0,
    };
  }

  /* 日曆必須連續：沒有用量的日子也要補成 0 的一格，否則畫面看不出
     「中間斷了三天」—— 那正是熱區圖唯一比一條折線強的地方。 */
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const earliest = [...byDate.keys()].sort()[0] ?? localDate(Date.now() / 1000);
  const span = Math.round(
    (today.getTime() - new Date(`${earliest}T00:00:00`).getTime()) / 86_400_000,
  );
  const length = fillRange
    ? Math.max(1, maxDays)
    : Math.min(maxDays, Math.max(1, span + 1));

  const days: UsageDay[] = [];
  let maxDayTokens = 0;
  for (let i = length - 1; i >= 0; i -= 1) {
    const d = addDays(today, -i);
    const date = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(
      d.getDate(),
    ).padStart(2, "0")}`;
    const found = byDate.get(date) ?? {
      date,
      inputTokens: 0,
      outputTokens: 0,
      requests: 0,
      failedRequests: 0,
    };
    maxDayTokens = Math.max(maxDayTokens, found.inputTokens + found.outputTokens);
    days.push(found);
  }

  return {
    days,
    totalInputTokens,
    totalOutputTokens,
    totalRequests: entries.length,
    maxDayTokens,
  };
}

/** Build the profile heatmap from Codex's authoritative daily token buckets. */
export function buildActivityHeatmap(
  activityDays: UsageActivityDay[],
  totalTokens: number,
  maxDays = 365,
  fillRange = true,
): UsageHeatmap {
  const byDate = new Map<string, UsageDay>();
  for (const item of activityDays) {
    const day = byDate.get(item.date) ?? {
      date: item.date,
      inputTokens: 0,
      outputTokens: 0,
      requests: 0,
      failedRequests: 0,
    };
    // Codex exposes one canonical "text tokens" number, not an input/output
    // decomposition. Keep it in one field so existing chart math remains
    // unchanged without inventing a split.
    day.inputTokens += item.tokens;
    day.requests += item.requests;
    byDate.set(item.date, day);
  }

  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const length = Math.max(1, maxDays);
  const days: UsageDay[] = [];
  let maxDayTokens = 0;
  for (let i = length - 1; i >= 0; i -= 1) {
    const d = addDays(today, -i);
    const date = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(
      d.getDate(),
    ).padStart(2, "0")}`;
    const found = byDate.get(date) ?? {
      date,
      inputTokens: 0,
      outputTokens: 0,
      requests: 0,
      failedRequests: 0,
    };
    maxDayTokens = Math.max(maxDayTokens, found.inputTokens);
    days.push(found);
  }
  if (!fillRange) {
    const first = days.findIndex(
      (day) => day.inputTokens > 0 || day.outputTokens > 0,
    );
    if (first > 0) days.splice(0, first);
  }
  return {
    days,
    totalInputTokens: totalTokens,
    totalOutputTokens: 0,
    totalRequests: activityDays.reduce((sum, day) => sum + day.requests, 0),
    maxDayTokens,
  };
}

/** 最近 n 天的 token 合計。 */
export function sumRecentDays(heat: UsageHeatmap, n: number): number {
  return heat.days
    .slice(-n)
    .reduce((sum, d) => sum + d.inputTokens + d.outputTokens, 0);
}

/* ============================================================
   三種讀法，同一個格陣
   ------------------------------------------------------------
   每日  一格一天，深淺 = 那天的量（密度圖）
   每週  一欄一週，由下往上填的格數 = 那週的量（柱狀圖）
   累計  一欄一週，填到當週為止的累積量（單調遞增的階梯）

   共用同一個 7×N 的格陣是刻意的：切換時位置不動、只有填法變，
   眼睛不用重新找基準。
   ============================================================ */

export type HeatMode = "daily" | "weekly" | "cumulative";

export const HEAT_MODES: { value: HeatMode; labelKey: string }[] = [
  { value: "daily", labelKey: "heatmap.mode.daily" },
  { value: "weekly", labelKey: "heatmap.mode.weekly" },
  { value: "cumulative", labelKey: "heatmap.mode.cumulative" },
];

const ROWS = 7;

export interface HeatCell {
  day: UsageDay | null;
  /** 0 = 空，1..4 = 深淺（每日）；每週／累計只有 0 或 4 */
  level: number;
}

export interface HeatColumn {
  key: string;
  cells: HeatCell[];
  /** 這一欄涵蓋的日期（給讀數列用） */
  from: string;
  to: string;
  total: number;
  requests: number;
  /** 這一欄是某個月的第一欄時，放月份標籤 */
  monthLabel: string | null;
}

const LEVEL_STOPS = [0.08, 0.28, 0.6];

function levelOf(value: number, max: number): number {
  if (value <= 0) return 0;
  const ratio = max ? value / max : 0;
  if (ratio >= (LEVEL_STOPS[2] ?? 1)) return 4;
  if (ratio >= (LEVEL_STOPS[1] ?? 1)) return 3;
  if (ratio >= (LEVEL_STOPS[0] ?? 1)) return 2;
  return 1;
}

const dayTotal = (d: UsageDay) => d.inputTokens + d.outputTokens;

/**
 * 把連續日曆切成「一欄一週」的格陣。
 *
 * 第一欄要補前導空格，讓每一列固定對應星期幾 —— 不補的話整張圖的星期
 * 會錯位，那比沒有這張圖更糟。
 */
export function toColumns(heat: UsageHeatmap, mode: HeatMode): HeatColumn[] {
  if (!heat.days.length) return [];

  const first = heat.days[0];
  if (!first) return [];
  const lead = new Date(`${first.date}T00:00:00`).getDay();

  /* 切成每欄 7 天。前面補 lead 個 null 讓星期對齊；**後面也要補滿**，
     否則最後那個未完成的週只有 2、3 格，柱狀模式就沒辦法從底部往上填
     （量到累計模式最後一欄掉回 2 格，破壞單調遞增）。 */
  const slots: (UsageDay | null)[] = [...Array<null>(lead).fill(null), ...heat.days];
  while (slots.length % ROWS !== 0) slots.push(null);
  const weeks: (UsageDay | null)[][] = [];
  for (let i = 0; i < slots.length; i += ROWS) {
    weeks.push(slots.slice(i, i + ROWS));
  }

  const weekTotals = weeks.map((w) =>
    w.reduce((sum, d) => sum + (d ? dayTotal(d) : 0), 0),
  );
  const maxWeek = Math.max(1, ...weekTotals);
  const grand = weekTotals.reduce((a, b) => a + b, 0) || 1;

  let running = 0;

  return weeks.map((week, wi) => {
    const present = week.filter((d): d is UsageDay => d !== null);
    const total = weekTotals[wi] ?? 0;
    running += total;

    /* 由下往上要填幾格。至少一格 —— 有用量卻完全看不到，比不精確更糟。 */
    const barFor = (value: number, max: number) =>
      value <= 0 ? 0 : Math.max(1, Math.round((value / max) * ROWS));

    const filled =
      mode === "weekly"
        ? barFor(total, maxWeek)
        : mode === "cumulative"
          ? barFor(running, grand)
          : 0;

    const cells: HeatCell[] = week.map((day, ri) => {
      if (mode === "daily") {
        return {
          day,
          level: day ? levelOf(dayTotal(day), heat.maxDayTokens) : 0,
        };
      }
      /* 由下往上：最後一列是第 1 格 */
      const fromBottom = ROWS - ri;
      return { day, level: fromBottom <= filled ? 4 : 0 };
    });

    const from = present[0]?.date ?? "";
    const to = present[present.length - 1]?.date ?? "";
    /* 月份標籤放在包含每月 1 日的欄。年度時間軸開頭若是上個月的
       零星日期就不重複標示，維持參考設計的一年十二個月份節點。 */
    const monthStart = present.find((day) => day.date.endsWith("-01"));
    const labelDate = monthStart?.date ?? "";
    const monthLabel = labelDate ? String(Number(labelDate.slice(5, 7))) : null;

    return {
      key: from || `w${wi}`,
      cells,
      from,
      to,
      total: mode === "cumulative" ? running : total,
      requests: present.reduce((sum, d) => sum + d.requests, 0),
      monthLabel,
    };
  });
}

/**
 * 連續使用天數。
 * current 從最後一天往回數（今天沒用就是 0），longest 是史上最長。
 */
export function streaks(heat: UsageHeatmap): { current: number; longest: number } {
  let current = 0;
  let longest = 0;
  let run = 0;
  for (const day of heat.days) {
    if (day.requests > 0) {
      run += 1;
      longest = Math.max(longest, run);
    } else {
      run = 0;
    }
  }
  for (let i = heat.days.length - 1; i >= 0; i -= 1) {
    if ((heat.days[i]?.requests ?? 0) > 0) current += 1;
    else break;
  }
  return { current, longest };
}

/** 單日最高用量。 */
export function peakDay(heat: UsageHeatmap): UsageDay | null {
  return heat.days.reduce<UsageDay | null>(
    (best, d) => (!best || dayTotal(d) > dayTotal(best) ? d : best),
    null,
  );
}
