/**
 * 工作階段的解讀。純函式，畫面不自己算。
 *
 * 第一性原理：使用者同時開好幾個 Codex 視窗，每個視窗是**獨立的對話、
 * 獨立的上下文視窗**。所以「上下文用到哪」根本不是一個數字 ——
 * 跟額度一樣，重要的是**最先被壓縮的那一個**，不是平均。
 *
 * 每個工作階段只需要回答三件事，其餘都是除錯資訊：
 *   1. 快被壓縮了嗎  → 用量百分比與門檻
 *   2. 在燒誰的額度  → Provider 與模型
 *   3. 還活著嗎      → 最後活動時間
 */
import type { SessionStatus } from "@/types";

/** 超過這段時間沒有動靜就當成閒置。Codex 視窗開著不關很常見。 */
export const IDLE_AFTER_SECONDS = 15 * 60;

export function sessionPercent(session: SessionStatus): number {
  if (session.windowTokens <= 0) return 0;
  return Math.min(
    100,
    Math.max(0, Math.round((session.usedTokens / session.windowTokens) * 100)),
  );
}

/** 還在活動中嗎。不隱藏閒置的 —— 只是讓它安靜（見 .watch 的 data-serving）。 */
export function isLive(session: SessionStatus, nowSeconds: number): boolean {
  return nowSeconds - session.lastActivityAt <= IDLE_AFTER_SECONDS;
}

/**
 * 最先撞到壓縮門檻的那一個。
 *
 * 取「距離門檻最近」而不是「用量最高」：不同模型的視窗大小差很多，
 * 用量 90% 的小視窗可能比 60% 的大視窗更快被壓縮。
 * 沒有工作階段時回 null，畫面就說沒有，不編一個數字。
 */
export function tightestSession(sessions: SessionStatus[]): SessionStatus | null {
  let best: SessionStatus | null = null;
  let bestGap = Number.POSITIVE_INFINITY;
  for (const session of sessions) {
    const gap = session.compactThresholdPercent - sessionPercent(session);
    if (gap < bestGap) {
      bestGap = gap;
      best = session;
    }
  }
  return best;
}

/** 活躍的排前面，其次照用量高低。閒置的不隱藏，只是排後面。 */
export function orderSessions(
  sessions: SessionStatus[],
  nowSeconds: number,
): SessionStatus[] {
  return [...sessions].sort((a, b) => {
    const live = Number(isLive(b, nowSeconds)) - Number(isLive(a, nowSeconds));
    if (live !== 0) return live;
    return sessionPercent(b) - sessionPercent(a);
  });
}

/**
 * 這個工作階段算不算 Enhanced core 的。
 *
 * 判定只寫在這裡一個地方：後端補上 `core` 欄位之前，每一筆都是 null，
 * 這時候寧可全部顯示也不要顯示成空的 —— 空清單會被讀成「沒有在跑」，
 * 那是一句假話。等後端開始標記，官方 core 的就會自己退場。
 * 要收緊成「只認 enhanced」，改這一行就好。
 */
export function isEnhancedCoreSession(session: SessionStatus): boolean {
  return session.core !== "official";
}


/**
 * 照「最近動過」排序，剛剛才動的在最前面。
 *
 * 清單只放前幾個，所以排序決定了誰進得去。用「最近動過」而不是「離門檻多近」：
 * 你回到這一頁，是為了看剛才在做的那個對話，不是為了看某個放著三天、剛好快滿
 * 的對話。壓力沒有被丟掉 —— 它畫在每一列上，只是不再決定順序。
 */
export function orderByRecency(sessions: SessionStatus[]): SessionStatus[] {
  return [...sessions].sort((a, b) => b.lastActivityAt - a.lastActivityAt);
}

/**
 * 清單的搜尋。比對的是這個對話的**標題**。
 *
 * 標題才是使用者認得一個對話的方式。把 Provider 與模型也丟進去比對聽起來
 * 更寬鬆，實際上更難用 —— 打「grok」會把每一個跑在 Grok 上的對話都撈出來，
 * 而你要找的是你命名過的那一個。
 *
 * 比對完整的 `session.label`，不是 {@link sessionLabel} 的結果：畫面上截短成
 * 16 個字的名字，搜尋時不該跟著被截短。空白切開後每一段都要命中（AND）。
 */
export function matchesSessionQuery(session: SessionStatus, query: string): boolean {
  const terms = query
    .trim()
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean);
  if (!terms.length) return true;
  const haystack = (session.label ?? "").toLowerCase();
  return terms.every((term) => haystack.includes(term));
}

/** 顯示名稱。沒有標題時使用呼叫端提供的本地化文字，不顯示內部 session id。 */
export function sessionLabel(session: SessionStatus, untitled: string): string {
  const label = session.label?.trim();
  if (!label) return untitled;
  const characters = Array.from(label);
  return characters.length > 16
    ? `${characters.slice(0, 16).join("")}…`
    : label;
}
