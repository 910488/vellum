/**
 * 網頁搜尋設定的解讀。
 *
 * Rust 端的 `WebSearchSettings` 有一個地方會讓畫面說謊，所以讀寫一律經過這裡：
 * **有兩個「關」**：`enabled: false` 與 `mode: "disabled"` 都會讓 Proxy 把
 * Codex 的 web.run 工具從第三方請求裡拿掉。兩個開關講同一件事，畫面只給一顆
 * —— `isOn()` 把它們收成一個布林，`turnOn()` 一次寫對兩個欄位。
 *
 * Brave Search 是唯一的後端，沒有第二家可選，也不會像舊版那樣在憑證不足時
 * 靜默退回別家 —— 沒有金鑰就是「還沒設定完」，`needsBraveKey()` 講的是這件事。
 */

import type { WebSearchSettings } from "@/types";

/** 介面承認的可及範圍。cached 在這個引擎裡等於壞掉，不曝。 */
export type SearchReach = "indexed" | "live";

export const SEARCH_REACHES: { value: SearchReach; labelKey: string }[] = [
  { value: "indexed", labelKey: "settings.page.webSearch.reachOption.indexed" },
  { value: "live", labelKey: "settings.page.webSearch.reachOption.live" },
];

export function reachLabelKey(reach: SearchReach): string {
  return `settings.page.webSearch.reachOption.${reach}`;
}

/** 搜尋真的會發生嗎。兩個關只要有一個關著就是關著。 */
export function isOn(settings: WebSearchSettings): boolean {
  return settings.enabled && (settings.mode === "indexed" || settings.mode === "live");
}

export function reachOf(settings: WebSearchSettings): SearchReach {
  return settings.mode === "indexed" ? "indexed" : "live";
}

/**
 * 開。`mode` 已經是可用的兩階之一就留著 —— 使用者上次選的「只讀搜尋結果」
 * 不該因為關了再開就被改回「可開啟網頁」（那是擅自放寬權限）。
 */
export function turnOn(settings: WebSearchSettings): WebSearchSettings {
  const mode = settings.mode === "indexed" || settings.mode === "live" ? settings.mode : "live";
  return { ...settings, enabled: true, mode };
}

/** 關。只動 `enabled`，`mode` 原樣留著，下次開才回得到同一個範圍。 */
export function turnOff(settings: WebSearchSettings): WebSearchSettings {
  return { ...settings, enabled: false };
}

export function setReach(settings: WebSearchSettings, reach: SearchReach): WebSearchSettings {
  return { ...settings, mode: reach };
}

/** 開著卻沒有金鑰 —— 唯一的後端還沒設定完，查詢一律失敗，不是「換一家回答」。 */
export function needsBraveKey(settings: WebSearchSettings, hasBraveApiKey: boolean): boolean {
  return isOn(settings) && !hasBraveApiKey;
}

/** 多行輸入 ↔ 網域清單。空行與前後空白都不算一個網域。 */
export function parseDomainList(text: string): string[] {
  return text
    .split(/[\n,]/)
    .map((line) => line.trim())
    .filter(Boolean);
}

export function formatDomainList(domains: string[]): string {
  return domains.join("\n");
}

export function domainRuleCount(settings: WebSearchSettings): number {
  return settings.domainPolicy.allow.length + settings.domainPolicy.block.length;
}
