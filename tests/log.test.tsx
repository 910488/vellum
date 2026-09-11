import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import i18n from "i18next";
import "@/i18n";
import { Log } from "@/screens/Log";

const apiMocks = vi.hoisted(() => ({
  getRequestLog: vi.fn(),
  getUsageActivity: vi.fn(),
  getBootTelemetry: vi.fn(),
  getInvokeRing: vi.fn((): Array<{ cmd: string; ok: boolean; error?: string; at: number }> => []),
}));

vi.mock("@/lib/api", () => ({
  api: apiMocks,
  getInvokeRing: () => apiMocks.getInvokeRing(),
}));

const emptyActivity = {
  days: [],
  totalTokens: 0,
  peakTokens: 0,
  longestTaskDurationMs: 0,
  currentStreakDays: 0,
  longestStreakDays: 0,
  providers: [],
  officialSource: "proxy_fallback" as const,
  warning: null,
};

function mockReadyLog(log: Record<string, unknown>) {
  apiMocks.getRequestLog.mockResolvedValue(log);
  apiMocks.getUsageActivity.mockResolvedValue(emptyActivity);
  apiMocks.getBootTelemetry.mockResolvedValue({
    bootCount: 1,
    startedAt: 1,
    previousStartedAt: null,
    pid: 42,
  });
}

describe("Log runtime notices", () => {
  beforeAll(async () => {
    await i18n.changeLanguage("zh-TW");
    // jsdom 沒有 scrollIntoView / requestAnimationFrame；定位導航需要它們。
    Element.prototype.scrollIntoView = vi.fn();
    globalThis.requestAnimationFrame = ((callback: FrameRequestCallback) => {
      callback(0);
      return 0;
    }) as typeof requestAnimationFrame;
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
    apiMocks.getInvokeRing.mockReturnValue([]);
  });

  /* 這張卡回答的是「誰吃掉我的量」。以下三件事只要壞一件，它就答錯：
     順序、分母、以及尾巴收不收。 */
  it("ranks providers by share and makes the shares add up", async () => {
    apiMocks.getRequestLog.mockResolvedValue({ entries: [], providers: [] });
    apiMocks.getUsageActivity.mockResolvedValue({
      ...emptyActivity,
      totalTokens: 1_000,
      // 故意不按大小給，卡片必須自己排序。
      providers: [
        { routeId: "b", provider: "Beta", tokens: 250, accountCount: 0, source: "proxy" },
        { routeId: "a", provider: "Alpha", tokens: 700, accountCount: 0, source: "proxy" },
        { routeId: "c", provider: "Gamma", tokens: 50, accountCount: 0, source: "proxy" },
      ],
    });
    apiMocks.getBootTelemetry.mockResolvedValue({
      bootCount: 1,
      startedAt: 1,
      previousStartedAt: null,
      pid: 42,
    });
    render(<Log refreshVersion={0} onRefreshComplete={() => {}} />);

    const rows = await screen.findAllByRole("generic", { hidden: true }).then(() =>
      Array.from(document.querySelectorAll(".usageshare__row")),
    );
    expect(rows.map((row) => row.querySelector(".usageshare__name")?.textContent)).toEqual([
      "Alpha",
      "Beta",
      "Gamma",
    ]);
    expect(rows.map((row) => row.querySelector(".usageshare__share")?.textContent)).toEqual([
      "70%",
      "25%",
      "5%",
    ]);
    // 長條的段長直接就是 token 數，不是預先算好的百分比 —— 兩者若分開算，
    // 圖與數字會各說各話。
    const segs = Array.from(document.querySelectorAll(".usageshare__bar .stackbar__seg"));
    expect(segs.map((seg) => (seg as HTMLElement).style.flexGrow)).toEqual(["700", "250", "50"]);
  });

  /* 小到 0% 的那一列仍然有用量。印成 0% 會讓人以為它沒被用到。 */
  it("says <1% instead of rounding a real share down to nothing", async () => {
    apiMocks.getRequestLog.mockResolvedValue({ entries: [], providers: [] });
    apiMocks.getUsageActivity.mockResolvedValue({
      ...emptyActivity,
      providers: [
        { routeId: "a", provider: "Alpha", tokens: 100_000, accountCount: 0, source: "proxy" },
        { routeId: "b", provider: "Beta", tokens: 100, accountCount: 0, source: "proxy" },
      ],
    });
    apiMocks.getBootTelemetry.mockResolvedValue({
      bootCount: 1,
      startedAt: 1,
      previousStartedAt: null,
      pid: 42,
    });
    render(<Log refreshVersion={0} onRefreshComplete={() => {}} />);
    await screen.findByText("<1%");
  });

  /* 沒有任何用量時整張卡不該出現 —— 一條空長條說不出任何事。 */
  it("hides the card entirely when nothing has been used", async () => {
    mockReadyLog({ entries: [], providers: [] });
    render(<Log refreshVersion={0} onRefreshComplete={() => {}} />);
    await screen.findByText(i18n.t("log.requests.title"));
    expect(document.querySelector(".usageshare__bar")).toBeNull();
  });

  it("renders a structured usage warning instead of crashing React", async () => {
    apiMocks.getRequestLog.mockResolvedValue({ entries: [], providers: [] });
    apiMocks.getUsageActivity.mockResolvedValue({
      ...emptyActivity,
      warning: {
        code: "usageProfileUnavailable",
        params: { count: "2" },
      },
    });
    apiMocks.getBootTelemetry.mockResolvedValue({
      bootCount: 1,
      startedAt: 1,
      previousStartedAt: null,
      pid: 42,
    });

    render(<Log refreshVersion={0} onRefreshComplete={() => {}} />);

    await waitFor(() => {
      expect(
        screen.getByText("2 個 OpenAI OAuth 帳號的 Codex Token 統計暫時無法讀取"),
      ).toBeTruthy();
    });
  });

  it("shows a cached-token ratio and a short connection id", async () => {
    const connectionId = "01JABCDEFGHIJKLMNOPQRSTUV";
    mockReadyLog({
      entries: [
        {
          id: 1,
          routeId: "grok-cli",
          provider: "Grok Build",
          model: "grok-4.5",
          inputTokens: 1_000,
          outputTokens: 200,
          cachedInputTokens: 400,
          status: 200,
          error: null,
          durationMs: 800,
          firstByteMs: 120,
          createdAt: 1_700_000_000,
          connectionId,
        },
      ],
      providers: [],
    });

    render(<Log refreshVersion={0} onRefreshComplete={() => {}} />);

    await waitFor(() => {
      expect(screen.getByText("快取 40%")).toBeTruthy();
    });
    expect(screen.getByText("conn 01JABCDE")).toBeTruthy();
    expect(screen.queryByText(connectionId)).toBeNull();
  });

  it("shows only shortened hashes for control A and execution B", async () => {
    const control = `sha256:${"a".repeat(64)}`;
    const execution = `sha256:${"b".repeat(64)}`;
    mockReadyLog({
      entries: [{
        id: 2,
        routeId: "official",
        provider: "Official",
        model: "gpt-5.6-luna",
        inputTokens: 10,
        outputTokens: 5,
        status: 200,
        error: null,
        durationMs: 300,
        firstByteMs: 100,
        createdAt: 1_700_000_000,
        controlAccountHash: control,
        executionAccountHash: execution,
      }],
      providers: [],
    });

    render(<Log refreshVersion={0} onRefreshComplete={() => {}} />);

    expect(await screen.findByText("A aaaaaaaa · B bbbbbbbb")).toBeTruthy();
    expect(screen.queryByText(control)).toBeNull();
    expect(screen.queryByText(execution)).toBeNull();
  });

  it("keeps compaction and subagent runs on separate facets of the same card", async () => {
    mockReadyLog({
      entries: [],
      providers: [],
      compactionEvents: [
        {
          id: 11,
          createdAt: 1_700_000_000,
          engine: "canonical",
          outcome: "compacted",
          tokensBefore: 8_000,
          tokensAfter: 2_000,
        },
      ],
      subagentRuns: [
        {
          callId: "call-xyz-123456",
          state: "requested",
          childRequestHash: null,
          routeId: null,
          model: "child-model",
          effort: null,
          linkConfidence: "unlinked",
          requestedAt: 1_700_000_010,
        },
      ],
    });

    render(<Log refreshVersion={0} onRefreshComplete={() => {}} />);

    // 壓縮／子代理／桌面呼叫合成一張卡、用分頁切換（見 Log.tsx 的
    // logFacet）——預設分頁是「壓縮」，這裡先確認它顯示壓縮內容、
    // 不顯示子代理內容，再切到「子代理」分頁確認反過來也成立。
    await waitFor(() => {
      expect(screen.getByText("壓縮")).toBeTruthy();
    });
    const card = screen.getByText("壓縮").closest(".card") as HTMLElement;
    expect(card.textContent).toContain("canonical");
    expect(card.textContent).not.toContain("child-model");

    fireEvent.click(screen.getByRole("button", { name: "子代理" }));
    expect(card.textContent).toContain("child-model");
    expect(card.textContent).not.toContain("canonical");
  });

  it("never renders an incomplete, ambiguous, or failed run with the success colour", async () => {
    mockReadyLog({
      entries: [],
      providers: [],
      subagentRuns: [
        {
          callId: "call-requested",
          state: "requested",
          childRequestHash: null,
          routeId: null,
          model: "child-model",
          effort: null,
          linkConfidence: "unlinked",
          requestedAt: 1_700_000_000,
        },
        {
          callId: "call-running",
          state: "running",
          childRequestHash: "child-running",
          routeId: "route-a",
          model: "child-model",
          effort: "medium",
          linkConfidence: "exact",
          requestedAt: 1_700_000_010,
        },
        {
          callId: "call-completed",
          state: "completed",
          childRequestHash: "child-completed",
          routeId: "route-a",
          model: "child-model",
          effort: "medium",
          linkConfidence: "exact",
          requestedAt: 1_700_000_020,
          completedAt: 1_700_000_025,
          durationMs: 5_000,
          outcome: "success",
        },
        {
          callId: "call-failed",
          state: "failed",
          childRequestHash: "child-failed",
          routeId: "route-a",
          model: "child-model",
          effort: "medium",
          linkConfidence: "exact",
          requestedAt: 1_700_000_030,
          completedAt: 1_700_000_035,
          outcome: "provider_failure",
          errorCategory: "provider_protocol",
        },
        {
          callId: null,
          state: "unlinked",
          childRequestHash: "child-orphan",
          routeId: "route-a",
          model: "child-model",
          effort: null,
          linkConfidence: "unlinked",
          requestedAt: 1_700_000_040,
        },
      ],
    });

    render(<Log refreshVersion={0} onRefreshComplete={() => {}} />);

    // 子代理是三個分頁之一，預設分頁是「壓縮」——切過去才看得到列表。
    await waitFor(() => {
      expect(screen.getByRole("button", { name: "子代理" })).toBeTruthy();
    });
    fireEvent.click(screen.getByRole("button", { name: "子代理" }));

    const subagent = screen.getByRole("button", { name: "子代理" }).closest(".card") as HTMLElement;
    expect(subagent.textContent).toContain("共 5 個");
    expect(subagent.textContent).toContain("完成 1");
    expect(subagent.textContent).toContain("執行中 2");
    expect(subagent.textContent).toContain("需注意 2");
    const rows = subagent.querySelectorAll(".subrun");
    expect(rows.length).toBe(5);

    for (const row of Array.from(rows)) {
      const badge = row.querySelector(".req__status") as HTMLElement;
      // The badge itself carries the run's *state* label ("已完成"); the
      // timeline inside a failed run's expandable body can also render a
      // "已完成" step (a completion signal arrived) without the run being a
      // success, so this must read the badge text specifically, not the
      // whole row's text.
      const isCompletedRow = badge.textContent === "已完成";
      if (isCompletedRow) {
        expect(badge.className).toContain("req__status--good");
      } else {
        expect(badge.className).not.toContain("req__status--good");
      }
    }
  });

  it("surfaces the capped invoke ring for both ok and error", async () => {
    apiMocks.getInvokeRing.mockReturnValue([
      { cmd: "get_request_log", ok: true, at: 1_700_000_000_000 },
      { cmd: "get_usage_activity", ok: false, error: "usage down", at: 1_700_000_001_000 },
    ]);
    mockReadyLog({ entries: [], providers: [] });

    render(<Log refreshVersion={0} onRefreshComplete={() => {}} />);

    // 桌面呼叫是三個分頁之一，預設分頁是「壓縮」——切過去才看得到列表。
    await waitFor(() => {
      expect(screen.getByRole("button", { name: "桌面呼叫" })).toBeTruthy();
    });
    fireEvent.click(screen.getByRole("button", { name: "桌面呼叫" }));
    expect(screen.getByText("get_request_log")).toBeTruthy();
    expect(screen.getByText("get_usage_activity")).toBeTruthy();
    expect(screen.getByText("ok")).toBeTruthy();
    expect(screen.getByText("error")).toBeTruthy();
    expect(screen.getByText("usage down")).toBeTruthy();
  });

  it("shows a clickable child-count badge that expands the child run", async () => {
    mockReadyLog({
      entries: [
        {
          id: 10,
          routeId: "grok-cli",
          provider: "Grok",
          model: "grok-4.5",
          inputTokens: 10,
          outputTokens: 5,
          status: 200,
          error: null,
          durationMs: 100,
          firstByteMs: null,
          createdAt: 1_700_000_000,
          requestIdHash: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
      ],
      providers: [],
      subagentRuns: [
        {
          callId: "call-1",
          state: "completed",
          parentRequestHash: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          childRequestHash: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
          childUsageEntryId: 20,
          routeId: "route-a",
          model: "child-model",
          effort: null,
          linkConfidence: "exact",
          requestedAt: 1_700_000_010,
        },
      ],
    });

    render(<Log refreshVersion={0} onRefreshComplete={() => {}} />);

    await waitFor(() => {
      expect(screen.getByText("1 個子代理")).toBeTruthy();
    });

    fireEvent.click(screen.getByText("1 個子代理"));

    const details = document.getElementById("subrun-call-1") as HTMLDetailsElement;
    expect(details).toBeTruthy();
    expect(details.open).toBe(true);
  });

  it("counts multiple children under one parent", async () => {
    mockReadyLog({
      entries: [
        {
          id: 1,
          routeId: "grok-cli",
          provider: "Grok",
          model: "grok-4.5",
          inputTokens: 10,
          outputTokens: 5,
          status: 200,
          error: null,
          durationMs: 100,
          firstByteMs: null,
          createdAt: 1_700_000_000,
          requestIdHash: "sha256:cccccccccccccccccccccccccccccccc",
        },
      ],
      providers: [],
      subagentRuns: [
        {
          callId: "call-a",
          state: "running",
          parentRequestHash: "sha256:cccccccccccccccccccccccccccccccc",
          childRequestHash: "sha256:dddddddddddddddddddddddddddddddd",
          linkConfidence: "exact",
          requestedAt: 1_700_000_010,
        },
        {
          callId: "call-b",
          state: "running",
          parentRequestHash: "sha256:cccccccccccccccccccccccccccccccc",
          childRequestHash: "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
          linkConfidence: "exact",
          requestedAt: 1_700_000_020,
        },
      ],
    });

    render(<Log refreshVersion={0} onRefreshComplete={() => {}} />);

    await waitFor(() => {
      expect(screen.getByText("2 個子代理")).toBeTruthy();
    });
  });

  it("renders no child badge when no sub-agent run links to the entry", async () => {
    mockReadyLog({
      entries: [
        {
          id: 1,
          routeId: "grok-cli",
          provider: "Grok",
          model: "grok-4.5",
          inputTokens: 10,
          outputTokens: 5,
          status: 200,
          error: null,
          durationMs: 100,
          firstByteMs: null,
          createdAt: 1_700_000_000,
          requestIdHash: "sha256:ffffffffffffffffffffffffffffffff",
        },
      ],
      providers: [],
      subagentRuns: [
        {
          callId: "call-x",
          state: "unlinked",
          parentRequestHash: "sha256:00000000000000000000000000000000",
          childRequestHash: "sha256:11111111111111111111111111111111",
          linkConfidence: "unlinked",
          requestedAt: 1_700_000_010,
        },
      ],
    });

    render(<Log refreshVersion={0} onRefreshComplete={() => {}} />);

    await waitFor(() => {
      expect(screen.getByText("Grok")).toBeTruthy();
    });
    expect(screen.queryByText("1 個子代理")).toBeNull();
  });

  it("locates the parent from the child without crashing on a missing target", async () => {
    mockReadyLog({
      entries: [],
      providers: [],
      subagentRuns: [
        {
          callId: "call-p",
          state: "completed",
          parentRequestHash: "sha256:22222222222222222222222222222222",
          childRequestHash: "sha256:33333333333333333333333333333333",
          parentUsageEntryId: 999,
          childUsageEntryId: 42,
          linkConfidence: "exact",
          requestedAt: 1_700_000_010,
        },
      ],
    });

    render(<Log refreshVersion={0} onRefreshComplete={() => {}} />);

    // 子代理是三個分頁之一，預設分頁是「壓縮」——切過去才看得到列表。
    await waitFor(() => {
      expect(screen.getByRole("button", { name: "子代理" })).toBeTruthy();
    });
    fireEvent.click(screen.getByRole("button", { name: "子代理" }));

    await waitFor(() => {
      expect(screen.getByText("父層 22222222")).toBeTruthy();
    });

    // The parent row (id 999) is absent from `entries`, so navigating is a
    // no-op that must not throw.
    const button = screen.getByRole("button", { name: "父層 22222222" });
    expect(() => fireEvent.click(button)).not.toThrow();
    const childButton = screen.getByRole("button", { name: "child 33333333" });
    expect(childButton.title).toBe("定位子層請求（33333333）");
    expect(() => fireEvent.click(childButton)).not.toThrow();
  });

  /* 系統事件本來不分頁：三種面向一次全畫出來，安靜的一天沒事，跑了一整天
     之後那張卡就是幾百列。換頁沿用請求明細那一顆 Pager —— 同一頁上同一個
     動作不該長成兩個樣子。 */
  it("pages the system events card the way the request list is paged", async () => {
    mockReadyLog({
      entries: [],
      providers: [],
      compactionEvents: Array.from({ length: 25 }, (_, index) => ({
        id: 500 + index,
        createdAt: 1_700_000_000 + index,
        engine: "local_trigger",
        outcome: "compacted",
        reason: null,
        tokensBefore: 1_000 + index,
        tokensAfter: 100,
      })),
    });

    render(<Log refreshVersion={0} onRefreshComplete={() => {}} />);

    // 第一頁 20 列，第 21 列要翻頁才看得到。
    await waitFor(() => {
      expect(document.querySelectorAll(".req--event").length).toBe(20);
    });
    expect(screen.getByText("共 25 筆")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "下一頁" }));

    await waitFor(() => {
      expect(document.querySelectorAll(".req--event").length).toBe(5);
    });
  });

  /* 換面向是換主題，不是換頁。停在第 2 頁再切過去，會落在一個沒有第 2 頁的
     列表上——不歸零就是一片空白。 */
  it("returns to the first page when the facet changes", async () => {
    mockReadyLog({
      entries: [],
      providers: [],
      compactionEvents: Array.from({ length: 25 }, (_, index) => ({
        id: 500 + index,
        createdAt: 1_700_000_000 + index,
        engine: "local_trigger",
        outcome: "compacted",
        reason: null,
        tokensBefore: 1_000,
        tokensAfter: 100,
      })),
      subagentRuns: [
        {
          callId: "call-solo",
          state: "completed",
          parentRequestHash: null,
          childRequestHash: null,
          childUsageEntryId: null,
          routeId: "route-a",
          model: "child-model",
          effort: null,
          linkConfidence: "exact",
          requestedAt: 1_700_000_010,
        },
      ],
    });

    render(<Log refreshVersion={0} onRefreshComplete={() => {}} />);

    await waitFor(() => {
      expect(screen.getByRole("button", { name: "下一頁" })).toBeTruthy();
    });
    fireEvent.click(screen.getByRole("button", { name: "下一頁" }));
    await waitFor(() => {
      expect(document.querySelectorAll(".req--event").length).toBe(5);
    });

    fireEvent.click(screen.getByRole("button", { name: "子代理" }));

    // 只有一個 run，所以它必須看得到——留在第 2 頁的話這裡是空的。
    await waitFor(() => {
      expect(document.getElementById("subrun-call-solo")).toBeTruthy();
    });
  });

  /* 分頁之後 jumpToChildren 不能再假設「所有 run 都在 DOM 裡」：它得先切到
     那個 run 所在的那一頁，否則點了徽章什麼都不會發生。 */
  it("jumps to the page holding the child run, not just the facet", async () => {
    const filler = Array.from({ length: 24 }, (_, index) => ({
      callId: `call-filler-${index}`,
      state: "completed" as const,
      parentRequestHash: null,
      childRequestHash: null,
      childUsageEntryId: null,
      routeId: "route-a",
      model: "child-model",
      effort: null,
      linkConfidence: "exact" as const,
      requestedAt: 1_700_000_000 + index,
    }));
    mockReadyLog({
      entries: [
        {
          id: 10,
          routeId: "grok-cli",
          provider: "Grok",
          model: "grok-4.5",
          inputTokens: 10,
          outputTokens: 5,
          status: 200,
          error: null,
          durationMs: 100,
          firstByteMs: null,
          createdAt: 1_700_000_000,
          requestIdHash: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        },
      ],
      providers: [],
      // 目標排在第 25 個，也就是第二頁的第一列。
      subagentRuns: [
        ...filler,
        {
          callId: "call-target",
          state: "completed",
          parentRequestHash: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          childRequestHash: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
          childUsageEntryId: 20,
          routeId: "route-a",
          model: "child-model",
          effort: null,
          linkConfidence: "exact",
          requestedAt: 1_700_000_100,
        },
      ],
    });

    render(<Log refreshVersion={0} onRefreshComplete={() => {}} />);

    await waitFor(() => {
      expect(screen.getByText("1 個子代理")).toBeTruthy();
    });

    fireEvent.click(screen.getByText("1 個子代理"));

    await waitFor(() => {
      const details = document.getElementById("subrun-call-target") as HTMLDetailsElement | null;
      expect(details).toBeTruthy();
      expect(details?.open).toBe(true);
    });
    // 是「翻到那一頁」，不是「碰巧 25 個都畫出來了」：第二頁只有 5 列。
    expect(screen.getByText("第 2 / 2 頁")).toBeTruthy();
    expect(document.querySelectorAll(".subrun").length).toBe(5);
  });

  it("asks the backend for one page instead of the KEEP window", async () => {
    mockReadyLog({
      entries: [],
      providers: [],
      entryTotal: 80,
    });
    render(<Log refreshVersion={0} onRefreshComplete={() => {}} />);
    await screen.findByText(i18n.t("log.requests.title"));
    expect(apiMocks.getRequestLog).toHaveBeenCalledWith(
      expect.objectContaining({ limit: 20, offset: 0 }),
    );
    const first = apiMocks.getRequestLog.mock.calls[0]?.[0] as { limit: number };
    expect(first.limit).toBeLessThan(500);
  });
});
