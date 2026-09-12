import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import i18n from "i18next";
import { Settings } from "@/screens/Settings";
import type { ModelRoute, Route, SubagentSettings } from "@/types";
import apiSource from "../src/lib/api.ts?raw";
import rustSource from "../src-tauri/src/lib.rs?raw";
import mockSource from "../src/lib/mockData.ts?raw";

const apiMocks = vi.hoisted(() => ({
  getReviewSettings: vi.fn(),
  listRoutes: vi.fn(),
  listReviewModelRoutes: vi.fn(),
  getRuntimeStatus: vi.fn(),
  getEnhancedDesktopRuntimeStatus: vi.fn(),
  getProxyStatus: vi.fn(),
  listCatalogVersions: vi.fn(),
  getReviewStats: vi.fn(),
  getWebSearchSettings: vi.fn(),
  getSubagentSettings: vi.fn(),
  getSubagentCapability: vi.fn(),
  listModelRoutes: vi.fn(),
  getCodexOAuthStatus: vi.fn(),
  setSubagentSettings: vi.fn(),
  resolveAutoCompactTokenLimit: vi.fn(),
  exportVellumLogs: vi.fn(),
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

const modelRoutes: ModelRoute[] = [
  {
    catalogId: "m-a1", displayName: "Alpha One", routeId: "route-a", upstreamModel: "alpha-one",
    contextWindow: 128_000, wire: "responses", reasoning: true, streaming: true,
    reasoningEfforts: ["low", "medium", "high"], defaultReasoningEffort: "medium",
    reasoningEffortTransport: "responses_object",
  },
  {
    catalogId: "m-a2", displayName: "Alpha Two", routeId: "route-a", upstreamModel: "alpha-two",
    contextWindow: 128_000, wire: "responses", reasoning: true, streaming: true,
    reasoningEfforts: ["low"], defaultReasoningEffort: "low",
    reasoningEffortTransport: "responses_object",
  },
  {
    catalogId: "m-a3", displayName: "Alpha Three", routeId: "route-a", upstreamModel: "alpha-three",
    contextWindow: 128_000, wire: "responses", reasoning: true, streaming: true,
    reasoningEfforts: ["low", "medium"], defaultReasoningEffort: "low",
    reasoningEffortTransport: "responses_object",
  },
  {
    catalogId: "m-b1", displayName: "Beta One", routeId: "route-b", upstreamModel: "beta-one",
    contextWindow: 128_000, wire: "responses", reasoning: true, streaming: true,
    reasoningEfforts: ["low"], defaultReasoningEffort: "low",
    reasoningEffortTransport: "responses_object",
  },
];

const inheritSettings: SubagentSettings = {
  mode: "inherit",
  routeId: null,
  catalogId: null,
  reasoningEffort: null,
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

function subagentCard(): HTMLElement {
  const title = screen.getByText(t("settings.page.subagent.title"));
  const card = title.closest(".card");
  if (!card) throw new Error("sub-agent card not found");
  return card as HTMLElement;
}

function cardSelect(label: string): HTMLSelectElement {
  const card = subagentCard();
  const key = Array.from(card.querySelectorAll(".rows__key")).find(
    (node) => node.textContent === label,
  );
  if (!key) throw new Error(`row label not found: ${label}`);
  const item = key.closest(".rows__item");
  const select = item?.querySelector("select");
  if (!select) throw new Error(`select not found in row: ${label}`);
  return select as HTMLSelectElement;
}

function deferredSubagentSave() {
  const pending: { resolve: (settings: SubagentSettings) => void; reject: (error: unknown) => void }[] = [];
  apiMocks.setSubagentSettings.mockImplementation(
    (_settings: SubagentSettings) =>
      new Promise<SubagentSettings>((resolve, reject) => pending.push({ resolve, reject })),
  );
  return pending;
}

describe("sub-agent default settings", () => {
  beforeAll(async () => {
    await i18n.changeLanguage("en");
  });

  beforeEach(() => {
    apiMocks.getReviewSettings.mockResolvedValue({
      onEdit: false,
      beforeSend: false,
      beforeCompact: false,
      routeId: "",
      model: "",
    });
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
    apiMocks.getSubagentSettings.mockResolvedValue(inheritSettings);
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
    apiMocks.setSubagentSettings.mockImplementation(
      async (settings: SubagentSettings) => settings,
    );
    apiMocks.resolveAutoCompactTokenLimit.mockResolvedValue(null);
    apiMocks.exportVellumLogs.mockResolvedValue("C:\\Users\\person\\Downloads\\vellum-logs.zip");
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it("registers the sub-agent settings commands the renderer invokes", () => {
    expect(apiSource).toMatch(/getSubagentSettings[\s\S]*"get_subagent_settings"/);
    expect(apiSource).toMatch(/getSubagentCapability[\s\S]*"get_subagent_capability"/);
    expect(apiSource).toMatch(/setSubagentSettings[\s\S]*"set_subagent_settings"/);
    expect(rustSource).toMatch(/commands::get_subagent_settings/);
    expect(rustSource).toMatch(/commands::get_subagent_capability/);
    expect(rustSource).toMatch(/commands::set_subagent_settings/);
  });

  it("exports the redacted Vellum log bundle from Settings", async () => {
    renderSettings();
    const button = await screen.findByRole("button", {
      name: t("settings.page.logs.export"),
    });
    fireEvent.click(button);

    await waitFor(() => expect(apiMocks.exportVellumLogs).toHaveBeenCalledTimes(1));
    expect(
      await screen.findByText(
        i18n.t("settings.page.logs.exported", {
          path: "C:\\Users\\person\\Downloads\\vellum-logs.zip",
        }),
      ),
    ).toBeTruthy();
    expect(apiSource).toMatch(/exportVellumLogs[\s\S]*"export_vellum_logs"/);
    expect(rustSource).toMatch(/commands::export_vellum_logs/);
  });

  it("provides a mock default in inherit mode", () => {
    expect(mockSource).toMatch(/mode: "inherit"/);
    expect(mockSource).toMatch(/reasoningEffort: null/);
  });

  it("shows Desktop capability and fails closed only when the Desktop runtime is unavailable", async () => {
    apiMocks.getSubagentCapability.mockResolvedValue({
      supported: false,
      desktopVersion: "26.818.61809",
      runtimeVersion: null,
      detail: "Desktop runtime was not found",
    });
    renderSettings();
    await screen.findByText(t("settings.page.subagent.title"));
    await screen.findByText(
      i18n.t("settings.page.subagent.unsupported", {
        detail: "Desktop runtime was not found",
      }),
    );

    fireEvent.click(screen.getByRole("button", { name: t("settings.page.subagent.mode.custom") }));

    expect(apiMocks.setSubagentSettings).not.toHaveBeenCalled();
  });

  it("identifies the Desktop app build instead of showing a standalone CLI version", async () => {
    renderSettings();
    await screen.findByText(t("settings.page.subagent.title"));

    expect(
      screen.getByText(
        i18n.t("settings.page.subagent.desktopVersion", { version: "26.818.61809" }),
      ),
    ).toBeTruthy();
    expect(screen.queryByText(/0\.147\.0/)).toBeNull();
  });

  it("seeds custom mode and atomically selects a Provider's first model", async () => {
    renderSettings();
    await screen.findByText(t("settings.page.subagent.title"));

    fireEvent.click(screen.getByRole("button", { name: t("settings.page.subagent.mode.custom") }));
    await waitFor(() => expect(apiMocks.setSubagentSettings).toHaveBeenCalledTimes(1));
    expect(apiMocks.setSubagentSettings).toHaveBeenLastCalledWith({
      mode: "custom",
      routeId: "route-a",
      catalogId: "m-a1",
      reasoningEffort: null,
    });

    fireEvent.change(cardSelect(t("common.provider")), { target: { value: "route-b" } });
    await waitFor(() => expect(apiMocks.setSubagentSettings).toHaveBeenCalledTimes(2));
    expect(apiMocks.setSubagentSettings).toHaveBeenLastCalledWith({
      mode: "custom",
      routeId: "route-b",
      catalogId: "m-b1",
      reasoningEffort: null,
    });
  });

  it("resets an effort the new model does not verify", async () => {
    apiMocks.getSubagentSettings.mockResolvedValue({
      mode: "custom",
      routeId: "route-a",
      catalogId: "m-a1",
      reasoningEffort: "medium",
    });
    renderSettings();
    await screen.findByText(t("settings.page.subagent.title"));

    fireEvent.change(cardSelect(t("common.model")), { target: { value: "m-a2" } });
    await waitFor(() => expect(apiMocks.setSubagentSettings).toHaveBeenCalledTimes(1));
    expect(apiMocks.setSubagentSettings).toHaveBeenLastCalledWith(
      expect.objectContaining({ catalogId: "m-a2", reasoningEffort: null }),
    );
  });

  it("keeps an effort the new model verifies", async () => {
    apiMocks.getSubagentSettings.mockResolvedValue({
      mode: "custom",
      routeId: "route-a",
      catalogId: "m-a1",
      reasoningEffort: "medium",
    });
    renderSettings();
    await screen.findByText(t("settings.page.subagent.title"));

    fireEvent.change(cardSelect(t("common.model")), { target: { value: "m-a3" } });
    await waitFor(() => expect(apiMocks.setSubagentSettings).toHaveBeenCalledTimes(1));
    expect(apiMocks.setSubagentSettings).toHaveBeenLastCalledWith(
      expect.objectContaining({ catalogId: "m-a3", reasoningEffort: "medium" }),
    );
  });

  it("serializes rapid edits and keeps the latest selection as the last save", async () => {
    const pending = deferredSubagentSave();
    apiMocks.getSubagentSettings.mockResolvedValue({
      mode: "custom",
      routeId: "route-a",
      catalogId: "m-a1",
      reasoningEffort: null,
    });
    renderSettings();
    await screen.findByText(t("settings.page.subagent.title"));

    // Two edits in the same tick: an effort pick followed by a Provider pick.
    // The Provider pick must win, and the second save must not start until the
    // first one settles.
    act(() => {
      fireEvent.change(cardSelect(t("settings.page.subagent.effort")), {
        target: { value: "medium" },
      });
      fireEvent.change(cardSelect(t("common.provider")), { target: { value: "route-b" } });
    });

    await waitFor(() => expect(apiMocks.setSubagentSettings).toHaveBeenCalledTimes(1));
    expect(apiMocks.setSubagentSettings).toHaveBeenCalledWith({
      mode: "custom",
      routeId: "route-a",
      catalogId: "m-a1",
      reasoningEffort: "medium",
    });
    expect(pending).toHaveLength(1);

    act(() =>
      pending[0]!.resolve({
        mode: "custom",
        routeId: "route-a",
        catalogId: "m-a1",
        reasoningEffort: "medium",
      }),
    );
    await waitFor(() => expect(apiMocks.setSubagentSettings).toHaveBeenCalledTimes(2));
    expect(apiMocks.setSubagentSettings).toHaveBeenLastCalledWith({
      mode: "custom",
      routeId: "route-b",
      catalogId: "m-b1",
      reasoningEffort: null,
    });

    act(() =>
      pending[1]!.resolve({
        mode: "custom",
        routeId: "route-b",
        catalogId: "m-b1",
        reasoningEffort: null,
      }),
    );
    await waitFor(() => expect(cardSelect(t("common.provider")).value).toBe("route-b"));
    expect(cardSelect(t("settings.page.subagent.effort")).value).toBe("");
  });

  it("rolls back to the last persisted selection when a save fails", async () => {
    apiMocks.getSubagentSettings.mockResolvedValue({
      mode: "custom",
      routeId: "route-a",
      catalogId: "m-a1",
      reasoningEffort: null,
    });
    renderSettings();
    await screen.findByText(t("settings.page.subagent.title"));

    apiMocks.setSubagentSettings.mockRejectedValueOnce(new Error("config locked"));
    fireEvent.change(cardSelect(t("common.model")), { target: { value: "m-a2" } });

    await screen.findByText(/Could not save sub-agent settings/);
    await waitFor(() => expect(cardSelect(t("common.model")).value).toBe("m-a1"));
  });

  it("shows an unavailable warning and never remaps a stale selection", async () => {
    apiMocks.getSubagentSettings.mockResolvedValue({
      mode: "custom",
      routeId: "route-gone",
      catalogId: "gone-model",
      reasoningEffort: "medium",
    });
    renderSettings();
    await screen.findByText(t("settings.page.subagent.title"));

    expect(screen.getByText(t("settings.page.subagent.unavailable"))).toBeTruthy();
    expect(apiMocks.setSubagentSettings).not.toHaveBeenCalled();
  });
});
