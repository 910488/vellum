import type { RuntimeNotice } from "@/types";

export type Translate = (key: string, values?: Record<string, unknown>) => string;

/**
 * 把後端的執行期通報組成當前語言的一句話。
 *
 * 後端只給代號與參數 —— 它不知道使用者把介面切成哪一種語言，也不該知道。
 * 代號沒有對應資源時 i18next 會回傳鍵本身，那是缺翻譯的訊號，不是空白。
 */
export function noticeText(notice: RuntimeNotice, t: Translate): string {
  return t(`runtime.notice.${notice.code}`, resolveNoticeParams(notice.params));
}

/**
 * `RuntimeNotice.params` 是後端序列化過來的 `Record<string, string>` ——
 * `count` 永遠是字串。i18next 的複數判斷只認數字型的 `count`：字串會讓它
 * 找不到 `_one`／`_other`，整句話原樣退回鍵名。這裡只把看起來是整數的
 * `count` 轉成數字，其餘參數維持字串，不影響非複數的插值。
 */
function resolveNoticeParams(params: Record<string, string>): Record<string, unknown> {
  const { count, ...rest } = params;
  if (count !== undefined && /^-?\d+$/.test(count)) {
    return { ...rest, count: Number(count) };
  }
  return params;
}
