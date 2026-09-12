import { describe, expect, it } from "vitest";
import { EMPTY_STATUS, attention, headline, type SystemStatus } from "@/lib/status";
import type { Overview, ProxyStatus, RuntimeStatus } from "@/types";
import statusBarSource from "../src/components/StatusBar.tsx?raw";

const proxy = (patch: Partial<ProxyStatus> = {}): ProxyStatus => ({
  running: true,
  baseUrl: "http://127.0.0.1:8118",
  catalogPath: null,
  codexManaged: true,
  lastError: null,
  notice: null,
  ...patch,
});

const runtime = (patch: Partial<RuntimeStatus> = {}): RuntimeStatus => ({
  proxyRunning: true,
  codexManaged: true,
  activeRequests: 0,
  draining: false,
  restartRequired: false,
  restartReasons: [],
  liveApplied: [],
  activeCatalogVersion: null,
  ...patch,
});

const overview = (patch: Partial<Overview> = {}): Overview => ({
  route: {
    id: "r1",
    name: "Grok Build",
    baseUrl: "https://cli-chat-proxy.grok.com",
    model: "grok-4.5",
    wire: "chat",
    isCurrent: true,
    serverSideResume: false,
    streaming: true,
    reasoning: true,
    providerKind: "grokCli",
    authKind: "grokSession",
    enabled: true,
    models: ["grok-4.5"],
    selectedModels: ["grok-4.5"],
    contextWindow: 500_000,
    modelCapabilities: [],
  },
  lastSuccessfulRoute: null,
  quota: {
    routeId: "r1",
    usedPercent: 43,
    period: { unit: "week", amount: null },
    resetAt: null,
    tier: null,
    stale: false,
  },
  usage: { usedTokens: 0, windowTokens: 1, turns: 0, providerTotalTokens: 0, trend: [], compacted: false },
  health: { pooled: true, connections: 1, firstByteMs: null, reasoningVisible: true, historyRetentionDays: 30 },
  findings: [],
  ...patch,
});

const status = (patch: Partial<SystemStatus> = {}): SystemStatus => ({
  proxy: proxy(),
  runtime: runtime(),
  overview: overview(),
  ...patch,
});

describe("headline", () => {
  it("還沒讀到任何東西時說「讀取中」，不假裝已停止", () => {
    const h = headline(EMPTY_STATUS);
    expect(h.labelKey).toBe("status.reading");
    expect(h.tone).toBe("quiet");
  });

  it("Proxy 沒跑 → 未啟動，且不顯示端點", () => {
    const h = headline(status({ proxy: proxy({ running: false }) }));
    expect(h.labelKey).toBe("status.proxyStopped");
    expect(h.endpoint).toBeNull();
  });

  it("Proxy 跑著但 Codex 沒導向 → 不算生效中，要示警", () => {
    const h = headline(status({ proxy: proxy({ codexManaged: false }) }));
    expect(h.labelKey).toBe("status.codexNotManaged");
    expect(h.tone).toBe("warn");
  });

  it("兩者都成立才是生效中", () => {
    const h = headline(status());
    expect(h.labelKey).toBe("status.live");
    expect(h.tone).toBe("ok");
    expect(h.model).toBe("grok-4.5");
    expect(h.provider).toBe("Grok Build");
    expect(h.routeSource).toBe("default");
  });

  it("待重啟不覆蓋左側的 Proxy 狀態", () => {
    const h = headline(status({
      runtime: runtime({ restartRequired: true, restartReasons: [{ code: "codexRunning", params: {} }] }),
    }));
    expect(h.labelKey).toBe("status.live");
    expect(h.tone).toBe("ok");
    expect(h.modelIsLive).toBe(true);
    expect(h.restartRequired).toBe(true);
  });

  it("Enhanced 待重啟保留右側提醒，但不取代左側狀態", () => {
    const h = headline(status({
      enhancedRuntime: { restartRequired: true } as SystemStatus["enhancedRuntime"],
    }));
    expect(h.labelKey).toBe("status.live");
    expect(h.modelIsLive).toBe(true);
    expect(h.restartRequired).toBe(true);
  });

  it("右側待重啟提示使用琥珀色警示，而不是紅色故障", () => {
    expect(statusBarSource).toMatch(
      /h\.restartRequired \? <Pill tone="warn" dot>\{t\("status\.restartCodexRequired"\)\}<\/Pill>/,
    );
  });

  it("有成功流量時，以最近一次成功請求取代預設線路", () => {
    const h = headline(status({
      overview: overview({
        lastSuccessfulRoute: {
          routeId: "nvidia",
          provider: "nvidia",
          model: "z-ai/glm-5.2",
          createdAt: 1_722_222_222,
        },
      }),
    }));
    expect(h.model).toBe("z-ai/glm-5.2");
    expect(h.provider).toBe("nvidia");
    expect(h.routeSource).toBe("telemetry");
    // 顯示的線路與額度來源不同時，不可把預設線路的額度接到最近使用的 Provider。
    expect(h.quotaRemaining).toBeNull();
  });

  it("額度換算成「剩多少」，因為使用者關心的是剩下的", () => {
    expect(headline(status()).quotaRemaining).toBe(57);
  });

  it("沒有線路時模型是 null，不編一個名字出來", () => {
    const h = headline(status({ overview: overview({ route: null, quota: null }) }));
    expect(h.model).toBeNull();
    expect(h.quotaRemaining).toBeNull();
  });

  it("只有真的在服務時才把模型算成 live —— 歷史不是狀態", () => {
    expect(headline(status()).modelIsLive).toBe(true);
    expect(headline(status({ proxy: proxy({ running: false }) })).modelIsLive).toBe(false);
    expect(headline(status({ proxy: proxy({ codexManaged: false }) })).modelIsLive).toBe(false);
    expect(headline(status({ runtime: runtime({ restartRequired: true }) })).modelIsLive).toBe(true);
    expect(headline(EMPTY_STATUS).modelIsLive).toBe(false);
  });

  it("Proxy 回報的錯誤要帶上來", () => {
    expect(headline(status({ proxy: proxy({ lastError: "埠已被占用" }) })).error).toBe("埠已被占用");
  });

  /* 「上次沒還原 Codex 設定」要處理，但沒有東西壞掉。混進 error 就等於
     用紅色講一件不是故障的事，真的壞掉時反而沒有更強的訊號可用。 */
  it("狀態通報跟故障分開兩個欄位", () => {
    const held = headline(
      status({ proxy: proxy({ notice: { code: "codexConfigStillPointedAtVellum", params: {} } }) }),
    );
    expect(held.notice?.code).toBe("codexConfigStillPointedAtVellum");
    expect(held.error).toBeNull();

    const broken = headline(status({ proxy: proxy({ lastError: "埠已被占用" }) }));
    expect(broken.notice).toBeNull();
  });
});

describe("attention", () => {
  it("沒事就不要有紅點（以前是寫死的 2）", () => {
    expect(attention(status())).toEqual({});
  });

  it("待處理事項數量掛在現況", () => {
    const withFindings = status({
      overview: overview({
        findings: [
          { id: "a", severity: "warning", title: "t", location: "l" },
          { id: "b", severity: "info", title: "t", location: "l" },
        ],
      }),
    });
    expect(attention(withFindings).today).toBe(2);
  });

  it("待重啟掛在模型 —— 那裡才有解決它的按鈕", () => {
    const needsRestart = status({
      runtime: runtime({ restartRequired: true, restartReasons: [{ code: "routesAndCatalogUpdated", params: {} }] }),
    });
    expect(attention(needsRestart).models).toBe(1);
  });

  it("本體或 Remote 更新不增加模型頁 badge", () => {
    const updateReady = status({
      runtime: runtime({
        restartRequired: true,
        restartReasons: [{ code: "desktopUpdateReady", params: {} }],
      }),
      updates: { attention: "available" } as SystemStatus["updates"],
    });
    expect(attention(updateReady).models).toBeUndefined();
    expect(attention(updateReady).settings).toBe(1);
  });

  it("待重啟但沒給原因時仍然要顯示一個紅點", () => {
    const needsRestart = status({ runtime: runtime({ restartRequired: true, restartReasons: [] }) });
    expect(attention(needsRestart).models).toBe(1);
  });

  it("什麼都沒讀到時不亂標", () => {
    expect(attention(EMPTY_STATUS)).toEqual({});
  });
});
