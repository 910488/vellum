import { describe, expect, it } from "vitest";
import { segAfter, segBefore } from "../src/screens/Context";
import contextSource from "../src/screens/Context.tsx?raw";
import { compactionTranscript, sessions } from "../src/lib/mockData";
import { isEnhancedCoreSession, orderByRecency } from "../src/lib/sessions";
import zhTwSource from "../src/i18n/locales/zh-TW.ts?raw";

describe("context compaction segments", () => {
  it("uses the Rust before/after fields when mock-only token fields are absent", () => {
    const segment = {
      label: "Tool",
      before: 184_000,
      after: 12_000,
      tone: "var(--lavender)",
    };

    expect(segBefore(segment)).toBe(184_000);
    expect(segAfter(segment)).toBe(12_000);
  });

  it("keeps compatibility with mock segments carrying explicit token fields", () => {
    const segment = {
      label: "User message",
      before: 1,
      after: 1,
      beforeTokens: 94_000,
      afterTokens: 55_000,
      tone: "var(--honey)",
    };

    expect(segBefore(segment)).toBe(94_000);
    expect(segAfter(segment)).toBe(55_000);
  });

  /* 兩張卡曾經是「預覽」與「逐項明細」，而它們可能落在不同的壓縮事件上 ——
     所以當時綁成一次 snapshot 讀。逐項明細已經從這一頁拿掉了，剩下的兩張卡
     都跟著同一個 `selected.id` 走，沒有可以錯開的第二個事件。
     這裡守的是「不再有人去讀逐項內容」。 */
  it("no longer reads the item-level compaction detail", () => {
    expect(contextSource).not.toContain("api.getCompactionDetail(");
    expect(contextSource).not.toContain("api.getCompactionSnapshot(");
    expect(contextSource).toContain("api.getCompactionTranscript(selected.id)");
    expect(contextSource).not.toContain("CompactionDetailView");
    expect(contextSource).not.toContain("LedgerRow");
  });

  /* 「壓縮引擎」「Reasoning 延續」「跨工作階段延續」是廢案。畫面上一個廢案
     跟一個壞掉的功能長得一模一樣 ——「尚未建立 portable window」讀起來像是
     有東西沒設好，其實是根本不會有。它們不能再回來。 */
  it("no longer shows the abandoned continuity fields", () => {
    expect(contextSource).not.toContain("context-continuity");
    expect(contextSource).not.toContain("context.continuity.");
    expect(contextSource).not.toContain("crossSessionAvailable");
    expect(contextSource).not.toContain("readableReplayTokens");
  });

  /* 這張卡唯一的版面風險是「原文會不會把整頁撐爆」。mock 的 prompt 一旦
     縮成一句話，那個風險在預覽台上就永遠試不到。 */
  it("keeps the mock transcript long enough to exercise the layout", () => {
    const transcript = compactionTranscript();
    expect(transcript.prompt?.split("\n").length ?? 0).toBeGreaterThan(8);
    expect(transcript.result?.split("\n").length ?? 0).toBeGreaterThan(8);
    /* 有內容時就不該再給一個「為什麼看不到」的理由 —— 兩個同時出現，
       畫面會一邊給你東西看一邊說看不到。 */
    expect(transcript.unavailableReason).toBeNull();
  });

  it("localizes backend compaction availability codes", () => {
    expect(contextSource).toContain("context.transcript.unavailable.${transcript.unavailableReason}");
    expect(zhTwSource).toContain('"codexDesktopOpaque": "Codex Desktop 將這次壓縮保存為不透明狀態');
    expect(zhTwSource).not.toContain(
      "This compaction was performed inside Codex Desktop. Its instruction and replacement text did not pass through Vellum.",
    );
  });

  /* 清單最多五個，所以 mock 的對話必須超過五個，否則上限根本沒被走到。 */
  it("mocks more conversations than the list shows at once", () => {
    const enhanced = orderByRecency(sessions().filter(isEnhancedCoreSession));
    expect(enhanced.length).toBeGreaterThan(5);
    const newest = enhanced.at(0)!;
    const oldest = enhanced.at(-1)!;
    expect(newest.lastActivityAt).toBeGreaterThan(oldest.lastActivityAt);
    expect(contextSource).toContain("matched.slice(0, RECENT_SESSIONS)");
    expect(contextSource).toContain("const RECENT_SESSIONS = 5");
  });
});
