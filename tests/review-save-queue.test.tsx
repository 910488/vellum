import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import i18n from "i18next";
import { Settings } from "@/screens/Settings";
import type { ModelRoute, ReviewSettings, ReviewSettingsUpdate, Route } from "@/types";

// Covers the auto-review save queue (M9 protocol): switching policy must not
// clear the last explicit route/model/fallback, and rapid policy changes
// must persist in the order they were made instead of racing.

const apiMocks = vi.hoisted(() => ({
  getReviewSettings: vi.fn(),
  setReviewSettings: vi.fn(),
  listRoutes: vi.fn(),
  listReviewModelRoutes: vi.fn(),
  getRuntimeStatus: vi.fn(),
  listCatalogVersions: vi.fn(),
  getReviewStats: vi.fn(),
  getWebSearchSettings: vi.fn(),
  getSubagentSettings: vi.fn(),
  getSubagentCapability: vi.fn(),
  listModelRoutes: vi.fn(),
  getCodexOAuthStatus: vi.fn(),
  getEnhancedDesktopRuntimeStatus: vi.fn(),
  getProxyStatus: vi.fn(),
  getUpdateStatus: vi.fn(),
}));

vi.mock("@/lib/api", () => ({ api: apiMocks }));

const routes: Route[] = [
  {
    id: "route-a",
    name: "Alpha",
    baseUrl: "https://alpha.example",
    model: "alpha-one",
    wire: "responses",
    isCurrent: true,
    serverSideResume: true,
    streaming: true,
    reasoning: true,
    providerKind: "openAiCompatible",
    authKind: "bearer",
    enabled: true,
    models: ["alpha-one"],
    selectedModels: ["alpha-one"],
    contextWindow: 128_000,
    modelCapabilities: [],
  },
  {
    id: "route-b",
    name: "Beta",
    baseUrl: "https://beta.example",
    model: "beta-one",
    wire: "responses",
    isCurrent: false,
    serverSideResume: true,
    streaming: true,
    reasoning: true,
    providerKind: "openAiCompatible",
    authKind: "bearer",
    enabled: true,
    models: ["beta-one"],
    selectedModels: ["beta-one"],
    contextWindow: 128_000,
    modelCapabilities: [],
  },
];

const officialRoute: Route = {
  id: "openai-official",
  name: "OpenAI Official",
  baseUrl: "https://chatgpt.com/backend-api/codex",
  model: "codex-auto-review",
  wire: "responses",
  isCurrent: false,
  serverSideResume: true,
  streaming: true,
  reasoning: true,
  providerKind: "official",
  authKind: "chatGpt",
  enabled: true,
  models: ["codex-auto-review"],
  selectedModels: ["codex-auto-review"],
  contextWindow: 272_000,
  modelCapabilities: [],
};

const officialModelRoute: ModelRoute = {
  catalogId: "codex-auto-review",
  displayName: "Codex Auto Review",
  routeId: "openai-official",
  upstreamModel: "codex-auto-review",
  contextWindow: 272_000,
  wire: "responses",
  reasoning: true,
  streaming: true,
  reasoningEfforts: [],
  defaultReasoningEffort: null,
  reasoningEffortTransport: "none",
};

const modelRoutes: ModelRoute[] = [
  {
    catalogId: "m-a1",
    displayName: "Alpha One",
    routeId: "route-a",
    upstreamModel: "alpha-one",
    contextWindow: 128_000,
    wire: "responses",
    reasoning: true,
    streaming: true,
    reasoningEfforts: [],
    defaultReasoningEffort: null,
    reasoningEffortTransport: "none",
  },
  {
    catalogId: "m-b1",
    displayName: "Beta One",
    routeId: "route-b",
    upstreamModel: "beta-one",
    contextWindow: 128_000,
    wire: "responses",
    reasoning: true,
    streaming: true,
    reasoningEfforts: [],
    defaultReasoningEffort: null,
    reasoningEffortTransport: "none",
  },
];

const explicitReview: ReviewSettings = {
  onEdit: false,
  beforeSend: true,
  beforeCompact: false,
  routeId: "route-a",
  model: "alpha-one",
  policy: "always",
  fallbackCatalogId: null,
};

/** A fresh install: no policy chosen yet, no route/model ever picked. */
const freshReview: ReviewSettings = {
  onEdit: false,
  beforeSend: true,
  beforeCompact: false,
  routeId: "",
  model: "",
  fallbackCatalogId: null,
};

function t(key: string): string {
  return i18n.t(key);
}

function renderSettings() {
  return render(
    <Settings
      onChanged={() => {}}
      onOpenOnboarding={() => {}}
      refreshVersion={0}
      onRefreshComplete={() => {}}
    />,
  );
}

function policyButton(labelKey: string): HTMLElement {
  return screen.getByRole("button", { name: t(labelKey) });
}

function updateResult(settings: ReviewSettings): ReviewSettingsUpdate {
  return { settings, localApplied: true, remoteHostsPendingReapply: 0 };
}

function deferredReviewSave() {
  const pending: {
    resolve: (settings: ReviewSettings) => void;
    reject: (error: unknown) => void;
  }[] = [];
  apiMocks.setReviewSettings.mockImplementation(
    (_settings: ReviewSettings) =>
      new Promise<ReviewSettingsUpdate>((resolve, reject) =>
        pending.push({
          resolve: (settings) => resolve(updateResult(settings)),
          reject,
        }),
      ),
  );
  return pending;
}

describe("auto review settings save behavior", () => {
  beforeAll(async () => {
    await i18n.changeLanguage("en");
  });

  beforeEach(() => {
    apiMocks.getReviewSettings.mockResolvedValue(explicitReview);
    apiMocks.setReviewSettings.mockImplementation(
      async (settings: ReviewSettings) => updateResult(settings),
    );
    apiMocks.listRoutes.mockResolvedValue(routes);
    apiMocks.listReviewModelRoutes.mockResolvedValue(modelRoutes);
    apiMocks.getRuntimeStatus.mockResolvedValue({
      proxyRunning: false,
      codexManaged: false,
      activeRequests: 0,
      draining: false,
      restartRequired: false,
      restartReasons: [],
      liveApplied: [],
      activeCatalogVersion: null,
    });
    apiMocks.getUpdateStatus.mockResolvedValue({
      desktop: { component: "desktop", currentVersion: "0.2.9", availableVersion: null, stagedVersion: null, channel: "stable", phase: "idle", applyCondition: "idle", releaseNotes: null, failureReason: null, operationId: null, targetVersion: null, downloadBytes: 0, downloadTotal: 0, liveAutoUpdate: false, hosts: [] },
      remote: { component: "remote", currentVersion: "0.2.9", availableVersion: null, stagedVersion: null, channel: "stable", phase: "idle", applyCondition: "idle", releaseNotes: null, failureReason: null, operationId: null, targetVersion: null, downloadBytes: 0, downloadTotal: 0, liveAutoUpdate: false, hosts: [] },
      core: { component: "core", currentVersion: "bundled", availableVersion: null, stagedVersion: null, channel: "stable", phase: "idle", applyCondition: "idle", releaseNotes: null, failureReason: null, operationId: null, targetVersion: null, downloadBytes: 0, downloadTotal: 0, liveAutoUpdate: false, hosts: [] },
      preferences: { channel: "stable", autoCheck: true, autoDownload: true, coreIdleHandoff: false },
      liveAutoUpdate: false,
      attention: "none",
    });
    apiMocks.listCatalogVersions.mockResolvedValue([]);
    apiMocks.getProxyStatus.mockResolvedValue({
      running: true,
      baseUrl: "http://127.0.0.1:8788",
      catalogPath: null,
      codexManaged: true,
      lastError: null,
      notice: null,
    });
    apiMocks.getReviewStats.mockResolvedValue({
      totalRuns: 0,
      fallbackRuns: 0,
      providers: [],
      activeRouteId: null,
      activeModel: null,
      activeIsFallback: false,
      activeReason: null,
    });
    apiMocks.getWebSearchSettings.mockResolvedValue({
      settings: {
        enabled: false,
        mode: "disabled",
        domainPolicy: { allow: [], block: [] },
        searchContextSize: "medium",
      },
      hasBraveApiKey: false,
    });
    apiMocks.getSubagentSettings.mockResolvedValue({
      mode: "inherit",
      routeId: null,
      catalogId: null,
      reasoningEffort: null,
    });
    apiMocks.getSubagentCapability.mockResolvedValue({
      supported: true,
      desktopVersion: "26.818.61809",
      runtimeVersion: "0.149.0",
      detail: "supported",
    });
    apiMocks.listModelRoutes.mockResolvedValue(modelRoutes);
    apiMocks.getCodexOAuthStatus.mockResolvedValue({
      authenticated: false,
      defaultAccountId: null,
      accounts: [],
    });
    apiMocks.getEnhancedDesktopRuntimeStatus.mockResolvedValue({
      configured: false,
      enabled: false,
      ready: false,
      enhancedRuntimeDigest: null,
      officialCodexExecutable: null,
      enhancedCodexExecutable: null,
      bridgeExecutable: null,
      blockers: [],
    });
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it("explains which ChatGPT account is charged while auto review is off", async () => {
    apiMocks.getReviewSettings.mockResolvedValue({
      ...explicitReview,
      beforeSend: false,
    });
    renderSettings();

    const help = await screen.findByRole("button", {
      name: t("settings.page.review.billingHelp"),
    });
    fireEvent.click(help);

    expect(
      screen.getByRole("heading", {
        name: t("settings.page.review.billingHelpTitle"),
      }),
    ).toBeTruthy();
    expect(
      screen.getByText(t("settings.page.review.billingHelpFacts.disabled.body")),
    ).toBeTruthy();
    expect(
      screen.getByText(t("settings.page.review.billingHelpFacts.pool.body")),
    ).toBeTruthy();
    expect(apiMocks.setReviewSettings).not.toHaveBeenCalled();
  });

  /**
   * 計費帳號只有 Official 才問得出來，而且它是本機身分，不是策略。這三件
   * 事一起測：選單只在 Official 出現、選了會存下去、換回第三方會清掉。
   * 最後一項最容易漏——留著它不會有任何效果，卻會在使用者哪天換回
   * Official 時無聲地生效。
   */
  it("only asks who pays on Official, and forgets it when the reviewer leaves", async () => {
    apiMocks.listRoutes.mockResolvedValue([...routes, officialRoute]);
    apiMocks.listReviewModelRoutes.mockResolvedValue([...modelRoutes, officialModelRoute]);
    apiMocks.getReviewSettings.mockResolvedValue({
      ...explicitReview,
      routeId: "openai-official",
      model: "codex-auto-review",
      officialAccountId: null,
    });
    apiMocks.getCodexOAuthStatus.mockResolvedValue({
      authenticated: true,
      defaultAccountId: "acct-1",
      accounts: [
        { accountId: "acct-1", email: "daily@example.com", authenticatedAt: 0, isDefault: true },
        { accountId: "acct-2", email: "reviews@example.com", authenticatedAt: 0, isDefault: false },
      ],
    });
    renderSettings();
    await screen.findByText(t("settings.page.review.title"));

    const billing = await screen.findByDisplayValue(
      t("settings.page.review.billingFollowsDefault"),
    );
    fireEvent.change(billing, { target: { value: "acct-2" } });
    await waitFor(() => expect(apiMocks.setReviewSettings).toHaveBeenCalledTimes(1));
    expect(apiMocks.setReviewSettings).toHaveBeenLastCalledWith(
      expect.objectContaining({ officialAccountId: "acct-2" }),
    );

    // 換到第三方 Provider：選單消失，而且存下去的值是 null，不是留著。
    const provider = screen.getByDisplayValue("OpenAI Official");
    fireEvent.change(provider, { target: { value: "route-a" } });
    await waitFor(() => expect(apiMocks.setReviewSettings).toHaveBeenCalledTimes(2));
    expect(apiMocks.setReviewSettings).toHaveBeenLastCalledWith(
      expect.objectContaining({ routeId: "route-a", officialAccountId: null }),
    );
    await waitFor(() =>
      expect(
        screen.queryByText(t("settings.page.review.billingAccount")),
      ).toBeNull(),
    );
  });

  it("keeps the last explicit route/model when switching between always and failover", async () => {
    renderSettings();
    await screen.findByText(t("settings.page.review.title"));

    fireEvent.click(policyButton("review.policy.failover.label"));

    await waitFor(() => expect(apiMocks.setReviewSettings).toHaveBeenCalledTimes(1));
    // The already-selected route/model must be reused, not cleared, when
    // only the policy changes.
    expect(apiMocks.setReviewSettings).toHaveBeenLastCalledWith(
      expect.objectContaining({
        policy: "failover",
        routeId: "route-a",
        model: "alpha-one",
      }),
    );
  });

  it("serializes rapid policy changes and keeps the latest choice as the last save", async () => {
    const pending = deferredReviewSave();
    renderSettings();
    await screen.findByText(t("settings.page.review.title"));

    // Two policy changes in the same tick: the second must not be sent
    // until the first save settles.
    act(() => {
      fireEvent.click(policyButton("review.policy.failover.label"));
      fireEvent.click(policyButton("review.policy.always.label"));
    });

    await waitFor(() => expect(apiMocks.setReviewSettings).toHaveBeenCalledTimes(1));
    expect(apiMocks.setReviewSettings).toHaveBeenCalledWith(
      expect.objectContaining({ policy: "failover" }),
    );
    expect(pending).toHaveLength(1);

    act(() => pending[0]!.resolve({ ...explicitReview, policy: "failover" }));
    await waitFor(() => expect(apiMocks.setReviewSettings).toHaveBeenCalledTimes(2));
    expect(apiMocks.setReviewSettings).toHaveBeenLastCalledWith(
      expect.objectContaining({ policy: "always" }),
    );

    act(() => pending[1]!.resolve({ ...explicitReview, policy: "always" }));
    await waitFor(() =>
      expect(screen.queryByText(t("settings.page.saved"))).toBeTruthy(),
    );
  });

  it("rolls back to the last persisted settings when a save fails", async () => {
    renderSettings();
    await screen.findByText(t("settings.page.review.title"));

    apiMocks.setReviewSettings.mockRejectedValueOnce(new Error("config locked"));
    fireEvent.click(policyButton("review.policy.failover.label"));

    await waitFor(() =>
      expect(apiMocks.setReviewSettings).toHaveBeenCalledTimes(1),
    );
    await waitFor(() =>
      expect(policyButton("review.policy.always.label").getAttribute("aria-pressed")).toBe(
        "true",
      ),
    );
    expect(policyButton("review.policy.failover.label").getAttribute("aria-pressed")).toBe(
      "false",
    );
  });

  it("auto-selects the first enabled review model when switching to always on a fresh install", async () => {
    apiMocks.getReviewSettings.mockResolvedValue(freshReview);
    renderSettings();
    await screen.findByText(t("settings.page.review.title"));

    fireEvent.click(policyButton("review.policy.always.label"));

    await waitFor(() => expect(apiMocks.setReviewSettings).toHaveBeenCalledTimes(1));
    expect(apiMocks.setReviewSettings).toHaveBeenLastCalledWith(
      expect.objectContaining({ policy: "always", routeId: "route-a", model: "alpha-one" }),
    );
  });

  it("auto-selects primary and a different-provider fallback when switching to failover on a fresh install", async () => {
    apiMocks.getReviewSettings.mockResolvedValue(freshReview);
    renderSettings();
    await screen.findByText(t("settings.page.review.title"));

    fireEvent.click(policyButton("review.policy.failover.label"));

    await waitFor(() => expect(apiMocks.setReviewSettings).toHaveBeenCalledTimes(1));
    expect(apiMocks.setReviewSettings).toHaveBeenLastCalledWith(
      expect.objectContaining({
        policy: "failover",
        routeId: "route-a",
        model: "alpha-one",
        fallbackCatalogId: "m-b1",
      }),
    );
  });

  it("shows a clear error and does not switch when no review model is available", async () => {
    apiMocks.getReviewSettings.mockResolvedValue(freshReview);
    apiMocks.listReviewModelRoutes.mockResolvedValue([]);
    apiMocks.listModelRoutes.mockResolvedValue([]);
    renderSettings();
    await screen.findByText(t("settings.page.review.title"));

    fireEvent.click(policyButton("review.policy.failover.label"));

    await screen.findByText(t("settings.page.errors.reviewNoModelAvailable"));
    expect(apiMocks.setReviewSettings).not.toHaveBeenCalled();
    // The default (no explicit policy saved yet) resolves to "always" and
    // must stay pressed since the failover switch never actually applied.
    expect(policyButton("review.policy.always.label").getAttribute("aria-pressed")).toBe("true");
    expect(policyButton("review.policy.failover.label").getAttribute("aria-pressed")).toBe(
      "false",
    );
  });

  it("queue rollback uses the latest confirmed value, not a stale enqueue-time snapshot", async () => {
    // Baseline is failover. A (always) succeeds, then B (failover) succeeds,
    // then C (always) fails. The rollback for C must land on B -- the value
    // actually persisted most recently -- never on whatever was persisted
    // when C was merely enqueued (which predates B ever landing).
    apiMocks.getReviewSettings.mockResolvedValue({
      ...explicitReview,
      policy: "failover",
      fallbackCatalogId: "m-b1",
    });
    const pending = deferredReviewSave();
    renderSettings();
    await screen.findByText(t("settings.page.review.title"));

    act(() => {
      fireEvent.click(policyButton("review.policy.always.label"));
      fireEvent.click(policyButton("review.policy.failover.label"));
      fireEvent.click(policyButton("review.policy.always.label"));
    });

    await waitFor(() => expect(pending).toHaveLength(1));
    const firstCall = apiMocks.setReviewSettings.mock.calls[0]![0] as ReviewSettings;
    expect(firstCall).toMatchObject({ policy: "always", routeId: "route-a", model: "alpha-one" });
    act(() => pending[0]!.resolve(firstCall));

    await waitFor(() => expect(pending).toHaveLength(2));
    const secondCall = apiMocks.setReviewSettings.mock.calls[1]![0] as ReviewSettings;
    expect(secondCall).toMatchObject({
      policy: "failover",
      routeId: "route-a",
      model: "alpha-one",
      fallbackCatalogId: "m-b1",
    });
    act(() => pending[1]!.resolve(secondCall));

    await waitFor(() => expect(pending).toHaveLength(3));
    act(() => pending[2]!.reject(new Error("config locked")));

    await screen.findByText(
      i18n.t("settings.page.errors.reviewSave", { detail: "Error: config locked" }),
    );
    // Rolled back to B (failover, the latest confirmed value), not to the
    // rejected C (always).
    await waitFor(() =>
      expect(policyButton("review.policy.failover.label").getAttribute("aria-pressed")).toBe(
        "true",
      ),
    );
    expect(policyButton("review.policy.always.label").getAttribute("aria-pressed")).toBe("false");
  });
});
