/**
 * ChatGPT 帳號手上的 Reset 額度。
 *
 * 一張 Reset 是一張有死期的券：可以把用滿的視窗清掉一次，過了到期日就沒
 * 了。畫面本來只印張數，於是最要緊的那件事看不到 —— 不是「我有幾張」，
 * 是「有沒有一張快要作廢了」。這個模組只負責排序與判定，畫面負責呈現。
 */

import type { CodexResetCredit } from "@/types";

/** 還沒被用掉、也還沒過期的那些券。上游用字串表達狀態。 */
export const RESET_STATUS_AVAILABLE = "available";

/**
 * 剩不到這段時間就算「快到期」。
 *
 * 48 小時而不是 24：ChatGPT 的 5 小時視窗一天可能重來好幾次，24 小時的
 * 警告會在「今天還用得到」的時候才亮，等於沒有提早。
 */
export const RESET_EXPIRING_SOON_MS = 48 * 60 * 60 * 1000;

export type ResetExpiryTone = "expired" | "soon" | "later" | "unknown";

/** 券的到期時刻（epoch ms）。上游沒給或給了壞字串時是 null。 */
export function resetCreditExpiry(credit: CodexResetCredit): number | null {
  if (!credit.expiresAt) return null;
  const at = new Date(credit.expiresAt).getTime();
  return Number.isNaN(at) ? null : at;
}

export function isSpendableReset(credit: CodexResetCredit): boolean {
  return credit.status === RESET_STATUS_AVAILABLE;
}

/**
 * 到期時刻對「現在」的意思。
 *
 * 沒有到期時間的券回 `unknown`，不是 `later` —— 兩者在畫面上要長得不一
 * 樣：一個是「還很久」，一個是「上游沒說」。
 */
export function resetExpiryTone(
  credit: CodexResetCredit,
  now: number = Date.now(),
): ResetExpiryTone {
  const at = resetCreditExpiry(credit);
  if (at === null) return "unknown";
  if (at <= now) return "expired";
  return at - now <= RESET_EXPIRING_SOON_MS ? "soon" : "later";
}

/**
 * 能用的排前面，其中最快到期的排最前面。
 *
 * 沒有到期時間的排在有到期時間的後面：它不構成「快沒了」的理由，卻會因
 * 為排序時被當成 0 或 Infinity 而搶到頭尾兩端的位置。
 *
 * 已經用掉或過期的留在清單裡，只是沉到最下面。刪掉它們會讓「3 張」與看
 * 得到的行數兜不起來，使用者會以為畫面漏了東西。
 */
export function orderResetCredits(credits: CodexResetCredit[]): CodexResetCredit[] {
  return [...credits].sort((a, b) => {
    const spendable = Number(isSpendableReset(b)) - Number(isSpendableReset(a));
    if (spendable !== 0) return spendable;
    const left = resetCreditExpiry(a);
    const right = resetCreditExpiry(b);
    if (left === null && right === null) return 0;
    if (left === null) return 1;
    if (right === null) return -1;
    return isSpendableReset(a) ? left - right : right - left;
  });
}

/** 可用券裡最快到期的那一張，沒有就是 null。用來決定觸發鈕要不要示警。 */
export function soonestResetExpiry(credits: CodexResetCredit[]): CodexResetCredit | null {
  return (
    orderResetCredits(credits).find(
      (credit) => isSpendableReset(credit) && resetCreditExpiry(credit) !== null,
    ) ?? null
  );
}
