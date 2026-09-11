import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import i18n from "i18next";
import { Models } from "@/screens/Models";
import type { ModelCapability, ModelRoute, Route, RouteReprobeReport } from "@/types";

// Covers the OpenCode Zen Effort re-probe fix: an unprobed/inconclusive
// Effort result must never render as "Automatic" (indistinguishable from a
// provider that genuinely has no discrete levels), and a Provider-level
// re-probe must report a real succeeded/failed summary instead of a bare
// pass/fail.

const apiMocks = vi.hoisted(() => ({
  listRoutes: vi.fn(),
  listModelRoutes: vi.fn(),
  getCatalogStatus: vi.fn(),
  getCodexOAuthStatus: vi.fn(),
  getGrokAccountStatus: vi.fn(),
  reprobeRouteCapabilities: vi.fn(),
  reprobeRouteModelCapability: vi.fn(),
}));

vi.mock("@/lib/api", () => ({ api: apiMocks }));

const zenRoute: Route = {
  id: "zen-route",
  name: "OpenCode Zen",
  baseUrl: "https://opencode.ai/zen/v1",
  model: "not-probed-model",
  wire: "chat",
  isCurrent: false,
  serverSideResume: false,
  streaming: true,
  reasoning: true,
  providerKind: "openAiCompatible",
  authKind: "bearer",
  enabled: true,
  models: ["not-probed-model", "indeterminate-model", "auto-model", "supported-model"],
  selectedModels: [
    "not-probed-model",
    "indeterminate-model",
    "auto-model",
    "supported-model",
  ],
  contextWindow: 128_000,
  modelCapabilities: [
    {
      model: "not-probed-model",
      contextWindow: 128_000,
      wire: "chat",
      streaming: true,
      reasoning: true,
      toolCalling: true,
      probeVersion: 4,
      reasoningEfforts: [],
      effortProbeStatus: "not_probed",
    } as ModelCapability,
    {
      model: "indeterminate-model",
      contextWindow: 128_000,
      wire: "chat",
      streaming: true,
      reasoning: true,
      toolCalling: true,
      probeVersion: 4,
      reasoningEfforts: [],
      effortProbeStatus: "indeterminate",
      effortProbeIssue: "provider_ignores_unknown_effort",
      probeAttempts: [{
        stage: "effort",
        wire: "chat",
        outcome: "provider_ignores_unknown_effort",
        durationMs: 1_500,
        timeout: false,
        message: "provider accepted the invalid negative control",
      }],
    } as ModelCapability,
    {
      model: "auto-model",
      contextWindow: 128_000,
      wire: "chat",
      streaming: true,
      reasoning: true,
      toolCalling: true,
      probeVersion: 4,
      reasoningEfforts: [],
      effortProbeStatus: "not_applicable",
      effortProbeVersion: 1,
    } as ModelCapability,
    {
      model: "supported-model",
      contextWindow: 128_000,
      wire: "chat",
      streaming: true,
      reasoning: true,
      toolCalling: true,
      probeVersion: 4,
      reasoningEfforts: ["low", "high"],
      effortProbeStatus: "supported",
      effortProbeVersion: 1,
    } as ModelCapability,
  ],
};

const zenModelRoutes: ModelRoute[] = zenRoute.models.map((model) => ({
  catalogId: `${zenRoute.id}:${model}`,
  displayName: `[OpenCode Zen] ${model}`,
  routeId: zenRoute.id,
  upstreamModel: model,
  contextWindow: 128_000,
  wire: "chat",
  reasoning: true,
  streaming: true,
  reasoningEfforts: [],
  defaultReasoningEffort: null,
  reasoningEffortTransport: "none",
}));

function t(key: string, options?: Record<string, unknown>): string {
  return i18n.t(key, options);
}

function renderModels() {
  return render(
    <Models onChanged={() => {}} refreshVersion={0} onRefreshComplete={() => {}} />,
  );
}

async function openZenTray() {
  // "OpenCode Zen" also appears as the static Add-Provider card's own
  // heading; only the one inside `.prov__name` is this route's Tray label.
  const label = await waitFor(() => {
    const match = document.querySelector(".prov__name");
    if (!match || match.textContent !== "OpenCode Zen") {
      throw new Error("Tray label not rendered yet");
    }
    return match;
  });
  const summary = label.closest("summary")!;
  fireEvent.click(summary);
}

describe("Models screen Effort probe status", () => {
  beforeAll(async () => {
    await i18n.changeLanguage("en");
  });

  beforeEach(() => {
    apiMocks.listRoutes.mockResolvedValue([zenRoute]);
    apiMocks.listModelRoutes.mockResolvedValue(zenModelRoutes);
    apiMocks.getCatalogStatus.mockResolvedValue({ proxyRunning: false, injectedModelIds: [] });
    apiMocks.getCodexOAuthStatus.mockResolvedValue({
      authenticated: false,
      defaultAccountId: null,
      accounts: [],
    });
    apiMocks.getGrokAccountStatus.mockResolvedValue({
      authenticated: false,
      defaultAccountId: null,
      accounts: [],
    });
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  // The Effort label lives alongside sibling text nodes inside one
  // `.menu__caps` span (not its own wrapped element), so `getByText`'s exact
  // whole-element matching cannot find it directly — check the row's
  // rendered text content instead.
  async function effortCapsText(model: string): Promise<string> {
    const row = (await screen.findByText(model)).closest(".menu__row")!;
    return row.querySelector(".menu__caps")?.textContent ?? "";
  }

  it("never renders a never-probed model's Effort as Automatic", async () => {
    renderModels();
    await openZenTray();

    const caps = await effortCapsText("not-probed-model");
    expect(caps).toContain(t("models.ui.modelCatalog.effortNotProbed"));
    expect(caps).not.toContain(t("models.ui.modelCatalog.auto"));
  });

  it("renders an inconclusive probe distinctly from both Automatic and never-probed", async () => {
    renderModels();
    await openZenTray();

    const caps = await effortCapsText("indeterminate-model");
    expect(caps).toContain(t("models.ui.modelCatalog.effortUnverified"));
    expect(caps).not.toContain(t("models.ui.modelCatalog.auto"));
    expect(caps).not.toContain(t("models.ui.modelCatalog.effortNotProbed"));
    const row = (await screen.findByText("indeterminate-model")).closest(".menu__row")!;
    expect(row.textContent).toContain(t("models.ui.modelCatalog.effortReasonIgnored"));
  });

  it("only renders Automatic once the probe genuinely confirmed no discrete levels", async () => {
    renderModels();
    await openZenTray();

    const caps = await effortCapsText("auto-model");
    expect(caps).toContain(t("models.ui.modelCatalog.auto"));
  });

  it("shows the verified levels for a model with a determined Supported outcome", async () => {
    renderModels();
    await openZenTray();

    const caps = await effortCapsText("supported-model");
    expect(caps).toContain("low / high");
  });

  it("offers retry only when another probe can produce new evidence", async () => {
    renderModels();
    await openZenTray();

    const neverProbed = (await screen.findByText("not-probed-model")).closest(
      ".menu__row",
    ) as HTMLElement;
    expect(
      within(neverProbed).getByRole("button", { name: t("models.ui.probe.verifyModel") }),
    ).toBeTruthy();
    // The Provider accepted the invalid negative-control value. Repeating the
    // same probe cannot establish support, so the UI explains the result and
    // does not offer a misleading retry loop.
    const indeterminate = (await screen.findByText("indeterminate-model")).closest(
      ".menu__row",
    ) as HTMLElement;
    expect(
      within(indeterminate).queryByRole("button", {
        name: t("models.ui.modelCatalog.retryEffortProbe"),
      }),
    ).toBeNull();
    // "auto-model" and "supported-model" have a determined Effort outcome
    // and a verified harness, so no retry affordance is needed.
    for (const model of ["auto-model", "supported-model"]) {
      const row = (await screen.findByText(model)).closest(".menu__row") as HTMLElement;
      expect(
        within(row).queryByRole("button", { name: t("models.ui.probe.verifyModel") }),
      ).toBeNull();
    }
  });

  it("shows the targeted-selection count while a Provider re-probe is running, then a real summary", async () => {
    let resolveReprobe: (report: RouteReprobeReport) => void = () => {};
    apiMocks.reprobeRouteCapabilities.mockImplementation(
      () =>
        new Promise<RouteReprobeReport>((resolve) => {
          resolveReprobe = resolve;
        }),
    );
    renderModels();
    await openZenTray();

    const reprobeButton = screen.getByRole("button", { name: t("models.ui.modelCatalog.reprobe") });
    const hint = reprobeButton.closest(".menu__foot")!.querySelector(".rows__hint")!;
    fireEvent.click(reprobeButton);

    await waitFor(() =>
      expect(hint.textContent).toContain(
        t("models.ui.modelCatalog.reprobeInProgress", { count: zenRoute.selectedModels!.length }),
      ),
    );

    act(() =>
      resolveReprobe({
        discovered: 4,
        targeted: 4,
        succeeded: 3,
        failed: 1,
        skipped: 0,
        errors: [{
          model: "indeterminate-model",
          message: "quota exhausted",
          stage: "typedTool",
          outcome: "provider_quota",
          status: 429,
          timeout: false,
          retryAfter: 45,
        }],
      }),
    );

    await waitFor(() =>
      expect(hint.textContent).toContain(
        t("models.ui.modelCatalog.reprobeSummaryWithFailures", {
          succeeded: 3,
          targeted: 4,
          failed: 1,
        }),
      ),
    );
    expect(
      screen.getByText(
        "indeterminate-model · typedTool · HTTP 429 · Retry-After 45s: quota exhausted",
      ),
    ).toBeTruthy();
  });
});
