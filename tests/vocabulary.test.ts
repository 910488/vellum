import { describe, expect, it } from "vitest";
import { REMEDY, providerState, quotaUnavailableReason, supportsQuota } from "@/lib/vocabulary";

describe("providerState", () => {
  it("Proxy 沒跑、已啟用 → 待生效，並指向啟動 Proxy", () => {
    const state = providerState({ enabled: true, proxyRunning: false, applied: false });
    expect(state.labelKey).toBe("status.pending");
    expect(state.remedyKey).toBe(REMEDY.startProxy);
  });

  it("Proxy 沒跑、已停用 → 已停用，不該叫使用者去做什麼", () => {
    const state = providerState({ enabled: false, proxyRunning: false, applied: false });
    expect(state.labelKey).toBe("status.off");
    expect(state.remedyKey).toBeNull();
  });

  it("已啟用且真的載入了 → 生效中", () => {
    const state = providerState({ enabled: true, proxyRunning: true, applied: true });
    expect(state.labelKey).toBe("status.live");
    expect(state.tone).toBe("ok");
  });

  it("已停用且真的沒載入 → 已停用", () => {
    expect(
      providerState({ enabled: false, proxyRunning: true, applied: false }).labelKey,
    ).toBe("status.off");
  });

  it("存檔與實際不一致（兩個方向都算）→ 待生效，並指向重啟 Proxy", () => {
    // 剛啟用，Proxy 還沒重啟
    const justEnabled = providerState({ enabled: true, proxyRunning: true, applied: false });
    // 剛停用，Proxy 還在服務它
    const justDisabled = providerState({ enabled: false, proxyRunning: true, applied: true });
    for (const state of [justEnabled, justDisabled]) {
      expect(state.labelKey).toBe("status.pending");
      expect(state.remedyKey).toBe(REMEDY.restartProxy);
    }
  });

  it("只回三種狀態 —— 不再增生第四種說法", () => {
    const labels = new Set<string>();
    for (const enabled of [true, false]) {
      for (const proxyRunning of [true, false]) {
        for (const applied of [true, false]) {
          labels.add(providerState({ enabled, proxyRunning, applied }).labelKey);
        }
      }
    }
    expect(labels).toEqual(new Set(["status.live", "status.pending", "status.off"]));
  });

  it("待生效一定帶著解法，生效中與已停用一定不帶", () => {
    for (const enabled of [true, false]) {
      for (const proxyRunning of [true, false]) {
        for (const applied of [true, false]) {
          const state = providerState({ enabled, proxyRunning, applied });
          expect(state.remedyKey === null).toBe(state.state !== "pending");
        }
      }
    }
  });
});

describe("supportsQuota", () => {
  it("只有官方與 Grok 查得到額度", () => {
    expect(supportsQuota("official")).toBe(true);
    expect(supportsQuota("grokCli")).toBe(true);
  });

  it("一般 OpenAI 相容端點沒有額度端點 —— 給按鈕是騙人的", () => {
    expect(supportsQuota("openAiCompatible")).toBe(false);
  });

  it("查不到的時候要說明原因，不是留白", () => {
    expect(quotaUnavailableReason("openAiCompatible")).toBe("vocabulary.quotaUnavailable");
    expect(quotaUnavailableReason("grokCli")).toBe("");
  });
});
