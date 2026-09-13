/**
 * Tauri 指令的唯一入口。
 *
 * 在瀏覽器（`pnpm dev:renderer`）跑時沒有 Tauri runtime，這裡會退回 mock，
 * 讓 UI 可以完全獨立於 Rust 端開發與測試。接線時只要把 mock 拿掉即可，
 * 呼叫端一行都不用改。
 */
import { EMPTY_OBSERVATIONS, type RuntimeObservations } from "./enhanced";
import type {
  CompactionDetail,
  CompactionTranscript,
  CompactionPreview,
  CompactionSnapshot,
  CodexOAuthAccount,
  CodexOAuthDeviceLogin,
  CodexOAuthStatus,
  CodexResetCredits,
  CodexResetResult,
  ContextBudget,
  Finding,
  GrokAccountStatus,
  GrokModelCatalog,
  GrokLoginStatus,
  Overview,
  ProviderOverview,
  ProbeResult,
  ProxyStatus,
  QuotaSnapshot,
  RequestLog,
  BootTelemetry,
  UsageActivity,
  ReviewSettings,
  ReviewSettingsUpdate,
  ReviewStats,
  SubagentCapability,
  SubagentSettings,
  SessionStatus,
  ModelRoute,
  CatalogStatus,
  InsecureHttpPolicy,
  RestoreResult,
  RuntimeStatus,
  EnhancedDesktopRuntimeStatus,
  EnhancedQualificationResult,
  CatalogVersion,
  RestartResult,
  Route,
  WebSearchProbeResult,
  WebSearchSettings,
  WebSearchSettingsView,
  RemoteDeploymentPlan,
  RemoteHostDesiredState,
  RemoteHostCandidate,
  RemoteHostStatus,
  RemoteSessionSummary,
  RemoteModelSelection,
  RemoteOperationProgress,
  RemoteGrokStatus,
  RemoteChatGptAccountPairing,
  RemoteChatGptAccountLogin,
  RemoteChatGptAccountStatus,
  RemoteOfficialExecutionAccount,
  RemoteOfficialExecutionAccountLogin,
  RemoteOfficialExecutionAccountPoll,
  RemoteControlPairing,
  RemoteReleaseStatus,
  DesktopCodexCompatibilityStatus,
  SshTrustStatus,
  PendingHostFingerprint,
  UpdateStatusSnapshot,
  UpdateOperation,
  UpdatePreferences,
  UpdateComponent,
} from "@/types";
import * as mock from "./mockData";
import { cachedQuery, invalidateCachedQueries } from "./queryCache";
import { PROXY_LIFECYCLE_EVENT } from "./proxyLifecycle";

export { PROXY_LIFECYCLE_EVENT, shouldApplyProxyLifecycle } from "./proxyLifecycle";

/** In-memory cap for the renderer invoke ring. Oldest entries drop first. */
export const INVOKE_RING_CAP = 200;

export type InvokeRingEntry = {
  cmd: string;
  ok: boolean;
  error?: string;
  at: number;
};

const invokeRing: InvokeRingEntry[] = [];

export function recordInvokeResult(
  cmd: string,
  ok: boolean,
  error?: string,
  at = Date.now(),
): void {
  invokeRing.push({ cmd, ok, ...(error ? { error } : {}), at });
  if (invokeRing.length > INVOKE_RING_CAP) {
    invokeRing.splice(0, invokeRing.length - INVOKE_RING_CAP);
  }
}

/** Newest-last snapshot of every `call()` success and failure. */
export function getInvokeRing(): InvokeRingEntry[] {
  return invokeRing.slice();
}

export function resetInvokeRing(): void {
  invokeRing.length = 0;
}

function hasTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

function afterSettingsChange<T>(promise: Promise<T>): Promise<T> {
  return promise.then((value) => {
    invalidateCachedQueries();
    return value;
  });
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const at = Date.now();
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    const result = await invoke<T>(cmd, args);
    recordInvokeResult(cmd, true, undefined, at);
    return result;
  } catch (cause) {
    const error = cause instanceof Error ? cause.message : String(cause);
    recordInvokeResult(cmd, false, error, at);
    throw cause;
  }
}

/** mock 走一小段延遲，才看得出 loading 狀態設計得對不對 */
function delay<T>(value: T, ms = 220): Promise<T> {
  return new Promise((resolve) => setTimeout(() => resolve(value), ms));
}

/**
 * 瀏覽器預覽沒有 Codex Desktop，也就沒有 bridge attestation。
 * `active` 只能是 false —— 預覽台先假裝 ready，正是要防的那種假陽性。
 */
function previewLayer(component: UpdateComponent, applyCondition: string): import("@/types").LayerStatus {
  return {
    component,
    currentVersion: "0.2.9",
    availableVersion: null,
    stagedVersion: null,
    channel: "stable",
    phase: "idle",
    applyCondition,
    releaseNotes: null,
    failureReason: null,
    operationId: null,
    targetVersion: null,
    downloadBytes: 0,
    downloadTotal: 0,
    liveAutoUpdate: false,
    hosts: [],
  };
}

function previewUpdateStatus(): UpdateStatusSnapshot {
  return {
    desktop: previewLayer("desktop", "restartVellum"),
    remote: previewLayer("remote", "hostIdle"),
    core: previewLayer("core", "nextCoreStart"),
    preferences: { channel: "stable", autoCheck: true, autoDownload: true, coreIdleHandoff: false },
    liveAutoUpdate: false,
    attention: "none",
  };
}

function previewEnhancedRuntimeStatus(): EnhancedDesktopRuntimeStatus {
  return {
    configured: false,
    enabled: false,
    artifactReady: false,
    active: false,
    bridgeObserved: false,
    ready: false,
    activationState: "disabled",
    environmentState: "released",
    environmentValue: null,
    observedBridgeExecutable: null,
    restartRequired: false,
    launchId: null,
    bridgeState: null,
    bridgePid: null,
    officialChildPid: null,
    enhancedChildPid: null,
    enhancedRuntimeDigest: null,
    activeRuntimeDigest: null,
    activeFeatureProfile: null,
    officialCodexExecutable: null,
    enhancedCodexExecutable: null,
    coreAvailable: false,
    missingHelpers: [],
    launchCoreDrift: false,
    bridgeExecutable: null,
    // 瀏覽器預覽沒有任何一支 Codex core 可以問，所以比對「跑不了」，
    // 而不是「跑了，結果一致」——後者會是預覽台最不該假裝的那種綠燈。
    protocol: null,
    serving: false,
    unverified: false,
    lastQualification: null,
    blockers: [],
  };
}

export const api = {
  getRemoteManagerFeatureFlags(): Promise<{
    nativeCodexRemoteManager: boolean;
    legacyBrokerHostsDropped?: string[];
  }> {
    if (!hasTauri()) return delay({ nativeCodexRemoteManager: false, legacyBrokerHostsDropped: [] });
    return call("get_remote_manager_feature_flags");
  },

  discoverRemoteConnections(): Promise<RemoteHostCandidate[]> {
    if (!hasTauri()) return delay([]);
    return call<RemoteHostCandidate[]>("discover_remote_connections");
  },

  inspectRemoteHost(hostId: string): Promise<RemoteHostStatus> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteHostStatus>("inspect_remote_host", { hostId });
  },

  getDesktopCodexCompatibility(hostId: string): Promise<DesktopCodexCompatibilityStatus> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<DesktopCodexCompatibilityStatus>("get_desktop_codex_compatibility", { hostId });
  },

  updateRemoteCodexForDesktop(hostId: string): Promise<RemoteOperationProgress> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteOperationProgress>("update_remote_codex_for_desktop", { hostId });
  },

  bootstrapRemoteHost(hostId: string): Promise<RemoteOperationProgress> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteOperationProgress>("bootstrap_remote_host", { hostId });
  },

  /** Resolves the host's SSH alias to an actual (host, port) and reports
   * whether that resolved target's key is already trusted in Vellum's own
   * known_hosts file. Never connects; safe to call speculatively. */
  getRemoteSshTrustStatus(hostId: string): Promise<SshTrustStatus> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<SshTrustStatus>("remote_ssh_trust_status", { hostId });
  },

  /** Fetches (without trusting) the key the host is currently offering, via
   * `ssh-keyscan`. Shows the user something to cross-check before they
   * confirm — nothing is written to disk by this call. */
  fetchRemoteSshFingerprint(hostId: string): Promise<PendingHostFingerprint> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<PendingHostFingerprint>("remote_ssh_fetch_fingerprint", { hostId });
  },

  /** Records the user's explicit confirmation of `fingerprint` and appends
   * the matching key to Vellum's known_hosts file. Only call this after a
   * real user action (a button click) — never automatically. */
  confirmRemoteSshFingerprint(hostId: string, fingerprint: string): Promise<void> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<void>("remote_ssh_confirm_fingerprint", { hostId, fingerprint });
  },

  planRemoteDeployment(
    hostId: string,
    selection: RemoteModelSelection,
  ): Promise<RemoteDeploymentPlan> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteDeploymentPlan>("plan_remote_deployment", { hostId, selection });
  },

  /**
   * Rebuild a plan from this host's own last-applied model selection and
   * policy overrides. Unlike `planRemoteDeployment`, this never sends an
   * empty `RemotePolicyOverrides` — the backend reuses the host's saved
   * overrides (`autoReviewEnabled` etc.), so it is the only reapply path
   * that cannot silently reset them.
   */
  reapplyRemoteDeployment(hostId: string): Promise<RemoteDeploymentPlan> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteDeploymentPlan>("reapply_remote_deployment", { hostId });
  },

  applyRemoteDeployment(hostId: string, planId: string): Promise<RemoteOperationProgress> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteOperationProgress>("apply_remote_deployment", { hostId, planId });
  },

  getRemoteOperation(operationId: string): Promise<RemoteOperationProgress> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteOperationProgress>("get_remote_operation", { operationId });
  },

  restartRemoteNativeCodex(hostId: string, operationId: string): Promise<unknown> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call("restart_remote_native_codex", { hostId, operationId });
  },

  restoreRemoteHost(hostId: string): Promise<RemoteOperationProgress> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteOperationProgress>("restore_remote_host", { hostId });
  },

  stopRemoteAppOwnedCodex(hostId: string, operationId: string): Promise<unknown> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call("stop_remote_app_owned_codex", { hostId, operationId });
  },

  startRemoteGrokLogin(hostId: string): Promise<RemoteGrokStatus> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteGrokStatus>("start_remote_grok_login", { hostId });
  },

  /**
   * 記住 Remote Manager 目前顯示哪一台。Desktop 換 ChatGPT 帳號時，只有這一台
   * 會跟著換，其餘主機在 Remote Manager 裡以「與 Desktop 不一致」呈現。
   */
  setActiveRemoteHost(hostId: string | null): Promise<void> {
    if (!hasTauri()) return Promise.resolve();
    return call<void>("set_active_remote_host", { hostId });
  },

  listRemoteCodexAccountPairings(hostId: string): Promise<RemoteChatGptAccountPairing[]> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteChatGptAccountPairing[]>("list_remote_codex_account_pairings", { hostId });
  },

  startRemoteCodexAccountLogin(hostId: string, accountId?: string): Promise<RemoteChatGptAccountLogin> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteChatGptAccountLogin>("start_remote_codex_account_login", { hostId, accountId });
  },

  pollRemoteCodexAccountLogin(hostId: string): Promise<RemoteChatGptAccountStatus> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteChatGptAccountStatus>("poll_remote_codex_account_login", { hostId });
  },

  activateRemoteCodexAccount(hostId: string, accountId?: string): Promise<RemoteChatGptAccountStatus> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteChatGptAccountStatus>("activate_remote_codex_account", { hostId, accountId });
  },

  listRemoteOfficialExecutionAccounts(hostId: string): Promise<RemoteOfficialExecutionAccount[]> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteOfficialExecutionAccount[]>("list_remote_official_execution_accounts", { hostId });
  },

  startRemoteOfficialExecutionAccountLogin(hostId: string, displayName: string): Promise<RemoteOfficialExecutionAccountLogin> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteOfficialExecutionAccountLogin>("start_remote_official_execution_account_login", { hostId, displayName });
  },

  pollRemoteOfficialExecutionAccountLogin(hostId: string, loginId: string): Promise<RemoteOfficialExecutionAccountPoll> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteOfficialExecutionAccountPoll>("poll_remote_official_execution_account_login", { hostId, loginId });
  },

  selectRemoteOfficialExecutionAccount(hostId: string, accountIdHash: string): Promise<unknown> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call("select_remote_official_execution_account", { hostId, accountIdHash });
  },

  removeRemoteOfficialExecutionAccount(hostId: string, accountIdHash: string): Promise<unknown> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call("remove_remote_official_execution_account", { hostId, accountIdHash });
  },

  startRemoteControlPairing(hostId: string): Promise<RemoteControlPairing> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteControlPairing>("start_remote_control_pairing", { hostId });
  },

  pollRemoteGrokLogin(hostId: string): Promise<RemoteGrokStatus> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteGrokStatus>("poll_remote_grok_login", { hostId });
  },

  refreshRemoteGrokLogin(hostId: string): Promise<RemoteGrokStatus> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteGrokStatus>("refresh_remote_grok_login", { hostId });
  },

  cancelRemoteGrokLogin(hostId: string): Promise<RemoteGrokStatus> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteGrokStatus>("cancel_remote_grok_login", { hostId });
  },

  getCodexOAuthStatus(): Promise<CodexOAuthStatus> {
    if (!hasTauri()) {
      return delay(mock.codexOAuthStatus());
    }
    return call<CodexOAuthStatus>("get_codex_oauth_status");
  },

  startCodexOAuthLogin(): Promise<CodexOAuthDeviceLogin> {
    if (!hasTauri()) {
      return delay({
        deviceCode: "mock-device",
        userCode: "ABCD-EFGH",
        verificationUri: "https://auth.openai.com/codex/device",
        expiresIn: 900,
        interval: 5,
      });
    }
    return call<CodexOAuthDeviceLogin>("start_codex_oauth_login");
  },

  pollCodexOAuthLogin(deviceCode: string): Promise<CodexOAuthAccount | null> {
    if (!hasTauri()) return delay(null);
    return call<CodexOAuthAccount | null>("poll_codex_oauth_login", { deviceCode });
  },

  setDefaultCodexOAuthAccount(accountId: string): Promise<CodexOAuthStatus> {
    if (!hasTauri()) return this.getCodexOAuthStatus();
    return call<CodexOAuthStatus>("set_default_codex_oauth_account", { accountId });
  },

  removeCodexOAuthAccount(accountId: string): Promise<CodexOAuthStatus> {
    if (!hasTauri()) return this.getCodexOAuthStatus();
    return call<CodexOAuthStatus>("remove_codex_oauth_account", { accountId });
  },

  logoutCodexOAuth(): Promise<CodexOAuthStatus> {
    if (!hasTauri()) return this.getCodexOAuthStatus();
    return call<CodexOAuthStatus>("logout_codex_oauth");
  },

  refreshCodexOAuth(): Promise<CodexOAuthStatus> {
    if (!hasTauri()) return this.getCodexOAuthStatus();
    return call<CodexOAuthStatus>("refresh_codex_oauth");
  },

  getCodexOAuthResetCredits(accountId: string): Promise<CodexResetCredits> {
    if (!hasTauri()) return delay(mock.resetCredits(accountId), 320);
    return call<CodexResetCredits>("get_codex_oauth_reset_credits", { accountId });
  },

  consumeCodexOAuthReset(accountId: string, creditId: string): Promise<CodexResetResult> {
    if (!hasTauri()) return delay({ code: "reset", windowsReset: 1 });
    return call<CodexResetResult>("consume_codex_oauth_reset", { accountId, creditId });
  },

  getCodexOAuthAccountQuota(accountId: string, forceRefresh = false): Promise<QuotaSnapshot[]> {
    if (!hasTauri()) return delay([]);
    return call<QuotaSnapshot[]>("get_codex_oauth_account_quota", { accountId, forceRefresh });
  },

  getGrokAccountStatus(): Promise<GrokAccountStatus> {
    if (!hasTauri()) {
      return delay({ authenticated: false, defaultAccountId: null, accounts: [] });
    }
    return call<GrokAccountStatus>("get_grok_account_status");
  },

  startGrokAccountLogin(): Promise<GrokLoginStatus> {
    if (!hasTauri()) {
      return delay({
        loginId: "mock-grok-login",
        state: "waiting",
        account: null,
        error: null,
      });
    }
    return call<GrokLoginStatus>("start_grok_account_login");
  },

  pollGrokAccountLogin(loginId: string): Promise<GrokLoginStatus> {
    if (!hasTauri()) {
      return delay({ loginId, state: "waiting", account: null, error: null });
    }
    return call<GrokLoginStatus>("poll_grok_account_login", { loginId });
  },

  cancelGrokAccountLogin(loginId: string): Promise<GrokLoginStatus> {
    if (!hasTauri()) {
      return delay({ loginId, state: "cancelled", account: null, error: null });
    }
    return call<GrokLoginStatus>("cancel_grok_account_login", { loginId });
  },

  setDefaultGrokAccount(accountId: string): Promise<GrokAccountStatus> {
    if (!hasTauri()) return this.getGrokAccountStatus();
    return call<GrokAccountStatus>("set_default_grok_account", { accountId });
  },

  refreshGrokAccount(accountId: string): Promise<GrokAccountStatus> {
    if (!hasTauri()) return this.getGrokAccountStatus();
    return call<GrokAccountStatus>("refresh_grok_account", { accountId });
  },

  removeGrokAccount(accountId: string): Promise<GrokAccountStatus> {
    if (!hasTauri()) return this.getGrokAccountStatus();
    return call<GrokAccountStatus>("remove_grok_account", { accountId });
  },

  getGrokAccountQuota(
    accountId: string,
    routeId?: string,
    forceRefresh = false,
  ): Promise<QuotaSnapshot[]> {
    if (!hasTauri()) return delay([]);
    return call<QuotaSnapshot[]>("get_grok_account_quota", {
      accountId,
      routeId,
      forceRefresh,
    });
  },

  refreshGrokModelCatalog(routeId: string): Promise<GrokModelCatalog> {
    if (!hasTauri()) {
      return delay({ models: ["grok-4.6", "grok-4.5"], defaultModel: "grok-4.6" });
    }
    return call<GrokModelCatalog>("refresh_grok_model_catalog", { routeId });
  },

  getOverview(forceRefresh = false): Promise<Overview> {
    if (!hasTauri()) return delay(mock.overview());
    return cachedQuery("overview", () => call<Overview>("get_overview", { forceRefresh }), {
      bypass: forceRefresh,
    });
  },

  listRoutes(): Promise<Route[]> {
    if (!hasTauri()) return delay(mock.routes());
    return call<Route[]>("list_routes");
  },

  refreshOpenCodeModelCatalogs(): Promise<Route[]> {
    if (!hasTauri()) return delay(mock.routes());
    return call<Route[]>("refresh_opencode_model_catalogs");
  },

  selectRoute(routeId: string): Promise<void> {
    if (!hasTauri()) return delay(undefined);
    return afterSettingsChange(call<void>("select_route", { routeId }));
  },

  probeEndpoint(endpoint: string, apiKey?: string): Promise<ProbeResult> {
    if (!hasTauri()) return delay(mock.probe(endpoint), 900);
    return call<ProbeResult>("probe_endpoint", { endpoint, apiKey });
  },

  discoverEndpointModels(endpoint: string, apiKey?: string): Promise<ProbeResult> {
    if (!hasTauri()) {
      const result = mock.probe(endpoint);
      return delay({
        ...result,
        wire: null,
        streaming: false,
        reasoning: false,
        serverSideResume: false,
        modelCapabilities: result.modelCapabilities.map((capability) => ({
          ...capability,
          wire: null,
          streaming: null,
          toolCalling: null,
          probeVersion: null,
        })),
        needsInput: ["model"],
      }, 200);
    }
    return call<ProbeResult>("discover_endpoint_models", { endpoint, apiKey });
  },

  probeEndpointModel(endpoint: string, model: string, apiKey?: string): Promise<import("../types").ModelCapability> {
    if (!hasTauri()) {
      const result = mock.probe(endpoint);
      const capability = result.modelCapabilities.find((item) => item.model === model);
      if (!capability) return Promise.reject(new Error(`Model ${model} was not discovered`));
      return delay({ ...capability, toolCalling: true, probeVersion: 4 }, 500);
    }
    return call<import("../types").ModelCapability>("probe_endpoint_model", {
      endpoint,
      model,
      apiKey,
    });
  },

  createRoute(
    name: string,
    baseUrl: string,
    model: string,
    wire: "responses" | "chat",
    streaming: boolean,
    reasoning: boolean,
    serverSideResume: boolean,
    apiKey?: string,
    providerKind: "official" | "openAiCompatible" | "grokCli" = "openAiCompatible",
    models?: string[],
    selectedModels?: string[],
    contextWindow?: number | null,
    modelCapabilities?: import("../types").ModelCapability[],
    catalogScope?: import("../types").CatalogScope,
  ): Promise<Route[]> {
    if (!hasTauri()) return delay(mock.routes());
    return afterSettingsChange(call<Route[]>("create_route", {
      input: {
        name,
        baseUrl,
        model,
        wire,
        streaming,
        reasoning,
        serverSideResume,
        apiKey,
        providerKind,
        models,
        selectedModels,
        contextWindow,
        modelCapabilities,
        catalogScope,
      },
    }));
  },

  reprobeRouteModelCapability(routeId: string, model: string): Promise<import("../types").ModelCapability> {
    if (!hasTauri()) {
      const route = mock.routes().find((candidate) => candidate.id === routeId);
      const capability = route?.modelCapabilities.find((item) => item.model === model);
      if (!capability) return Promise.reject(new Error(`Model ${model} was not discovered`));
      return delay({ ...capability, toolCalling: true, probeVersion: 4 }, 500);
    }
    return call<import("../types").ModelCapability>("reprobe_route_model_capability", {
      routeId,
      model,
    });
  },

  listModelRoutes(): Promise<ModelRoute[]> {
    if (!hasTauri()) {
      return delay(
        mock.routes().flatMap((route) =>
          (route.providerKind === "official"
            ? route.models
            : (route.selectedModels ?? route.models)
          ).map((model) => ({
            ...(() => {
              const capability = route.modelCapabilities.find(
                (item) => item.model.toLowerCase() === model.toLowerCase(),
              );
              return {
                contextWindow: capability?.contextWindow ?? route.contextWindow,
                wire: capability?.wire ?? route.wire,
                reasoning: capability?.reasoning ?? route.reasoning,
                streaming: capability?.streaming ?? route.streaming,
                reasoningEfforts: capability?.reasoningEfforts ?? [],
                defaultReasoningEffort: capability?.defaultReasoningEffort ?? null,
                reasoningEffortTransport: capability?.reasoningEffortTransport ?? "none",
              };
            })(),
            catalogId: route.providerKind === "official" ? model : `${route.id}:${model}`,
            displayName: route.providerKind === "official" ? model : `[${route.name}] ${model}`,
            routeId: route.id,
            upstreamModel: model,
          })),
        ),
      );
    }
    return call<ModelRoute[]>("list_model_routes");
  },

  listReviewModelRoutes(): Promise<ModelRoute[]> {
    if (!hasTauri()) return this.listModelRoutes();
    return call<ModelRoute[]>("list_review_model_routes");
  },

  reprobeRouteCapabilities(routeId: string): Promise<import("../types").RouteReprobeReport> {
    if (!hasTauri()) {
      const route = mock.routes().find((candidate) => candidate.id === routeId);
      const targeted = route?.selectedModels ?? route?.models ?? [];
      return delay(
        {
          discovered: route?.models.length ?? 0,
          targeted: targeted.length,
          succeeded: targeted.length,
          failed: 0,
          skipped: 0,
          errors: [],
        },
        900,
      );
    }
    return call<import("../types").RouteReprobeReport>("reprobe_route_capabilities", { routeId });
  },

  getProviderOverviews(forceRefresh = false): Promise<ProviderOverview[]> {
    if (!hasTauri()) {
      return delay(
        mock.routes().map((route) => ({
          route,
          appliedToRunningProxy: route.enabled,
          quota: null,
          /* 刻意給不同形狀：一家有週額度、一家完全沒有額度可查、一家快用完 ——
             現況頁的「最先擋住你的」要能被這三種情況考驗到。 */
          quotaWindows: mock.quotaWindowsFor(route.id),
          quotaError: route.id === "weikuwu" ? null : null,
          models: (route.providerKind === "official"
            ? route.models
            : (route.selectedModels ?? route.models)
          ).map((model) => ({
            catalogId: route.providerKind === "official" ? model : `${route.id}:${model}`,
            displayName: model,
            upstreamModel: model,
            contextWindow: route.contextWindow,
            effectiveWindow: Math.floor((route.contextWindow ?? 121_600) * 0.95),
            reasoning: route.reasoning,
            streaming: route.streaming,
          })),
          latestInputTokens: mock.activityFor(route.id).latestInputTokens,
          turns: mock.activityFor(route.id).turns,
          firstByteMs: mock.activityFor(route.id).firstByteMs,
        })),
      );
    }
    return cachedQuery(
      "provider-overviews",
      () => call<ProviderOverview[]>("get_provider_overviews", { forceRefresh }),
      { bypass: forceRefresh },
    );
  },

  getContextBudget(catalogId: string): Promise<ContextBudget> {
    if (!hasTauri()) return delay(mock.budget(catalogId));
    return call<ContextBudget>("get_context_budget", { catalogId });
  },

  setBudgetOverride(catalogId: string, tokens: number | null): Promise<ContextBudget> {
    if (!hasTauri()) return delay(mock.budget(catalogId, tokens));
    return call<ContextBudget>("set_budget_override", { catalogId, tokens });
  },

  getReviewSettings(): Promise<ReviewSettings> {
    if (!hasTauri()) return delay(mock.reviewSettings());
    return call<ReviewSettings>("get_review_settings");
  },

  setReviewSettings(settings: ReviewSettings): Promise<ReviewSettingsUpdate> {
    if (!hasTauri()) {
      return delay({ settings, localApplied: true, remoteHostsPendingReapply: 0 });
    }
    return call<ReviewSettingsUpdate>("set_review_settings", { settings });
  },

  getSubagentSettings(): Promise<SubagentSettings> {
    if (!hasTauri()) return delay(mock.subagentSettings());
    return call<SubagentSettings>("get_subagent_settings");
  },

  getRemoteReleaseStatus(): Promise<RemoteReleaseStatus> {
    if (!hasTauri()) return delay({
      ready: false,
      trust: "bundled",
      releaseVersion: null,
      codexVersion: null,
      agentVersion: null,
      brokerVersion: null,
      proxyImage: null,
      proxyDigest: null,
      detail: "Remote deployment resources are only available in the packaged app.",
    });
    return call<RemoteReleaseStatus>("get_remote_release_status");
  },

  getSubagentCapability(): Promise<SubagentCapability> {
    if (!hasTauri()) {
      return delay({
        supported: true,
        desktopVersion: "26.818.61809",
        runtimeVersion: "0.149.0",
        detail: "Renderer mock supports native sub-agent defaults",
      });
    }
    return call<SubagentCapability>("get_subagent_capability");
  },

  setSubagentSettings(settings: SubagentSettings): Promise<SubagentSettings> {
    if (!hasTauri()) return delay(settings);
    return call<SubagentSettings>("set_subagent_settings", { settings });
  },

  /**
   * 自動審查實際跑了誰。
   *
   * 需要新的 Rust 指令 `get_review_stats`（tests/api-contract.test.ts 會盯著，
   * 註冊之前那個測試是紅的 —— 那正是它的用途：把待接的線列出來）。
   * 後端要記的是每次審查最後由哪個 routeId／model 回答，以及那次是不是備援。
   */
  /**
   * 目前的工作階段。
   *
   * 需要新的 Rust 指令 `get_sessions`。資料其實已經在 `response_history` 裡：
   * `conversation_key` 是身分、`created_at` 是最後活動，逐筆 token 也有 ——
   * 要做的是按 conversation_key 聚合，補上該線路的模型與視窗大小。
   * 若能一併帶出 Codex 的工作目錄，就填進 `label`（那才是人看得懂的識別）。
   */
  listSessions(): Promise<SessionStatus[]> {
    if (!hasTauri()) return delay(mock.sessions(), 240);
    return cachedQuery("sessions", () => call<SessionStatus[]>("get_sessions"), { ttlMs: 2_000 });
  },

  getReviewStats(): Promise<ReviewStats> {
    if (!hasTauri()) return delay(mock.reviewStats(), 260);
    return call<ReviewStats>("get_review_stats");
  },

  getWebSearchSettings(): Promise<WebSearchSettingsView> {
    if (!hasTauri()) return delay(mock.webSearchSettings());
    return call<WebSearchSettingsView>("get_web_search_settings");
  },

  /**
   * 金鑰不走設定往返：`braveApiKey` 只有在要換一把新的時候才送，
   * `clearBraveApiKey` 才是移除。兩個都不給就是「不要動已保存的那把」——
   * 把空字串當成「清掉」會讓每次改別的欄位都順手刪掉金鑰。
   */
  setWebSearchSettings(
    settings: WebSearchSettings,
    options: { braveApiKey?: string; clearBraveApiKey?: boolean } = {},
  ): Promise<WebSearchSettingsView> {
    if (!hasTauri()) {
      return delay({
        settings,
        hasBraveApiKey: options.clearBraveApiKey
          ? false
          : Boolean(options.braveApiKey?.trim()) || mock.webSearchSettings().hasBraveApiKey,
      });
    }
    return call<WebSearchSettingsView>("set_web_search_settings", {
      request: {
        settings,
        braveApiKey: options.braveApiKey,
        clearBraveApiKey: options.clearBraveApiKey ?? false,
      },
    });
  },

  probeWebSearch(query?: string): Promise<WebSearchProbeResult> {
    if (!hasTauri()) {
      return delay({ output: "ok: 8 results", resultCount: 8 }, 700);
    }
    return call<WebSearchProbeResult>("probe_web_search", { query });
  },

  /**
   * `sessionId` 指定要看哪一個工作階段；省略時維持舊行為，回傳最新一次壓縮。
   * 後端尚未接受這個參數之前，多送的欄位會被忽略，畫面照樣拿得到資料。
   */
  getCompactionPreview(sessionId?: string): Promise<CompactionPreview> {
    if (!hasTauri()) return delay(mock.compaction());
    return call<CompactionPreview>("get_compaction_preview", { sessionId });
  },

  /**
   * Runtime 壓縮明細的唯讀觀測。Codex 沒有公開來源／替換內容時，
   * originAvailable=false；舊版 Vellum journal 只作歷史顯示。
   */
  getCompactionDetail(sessionId?: string): Promise<CompactionDetail> {
    if (!hasTauri()) return delay(mock.compactionDetail(), 320);
    return call<CompactionDetail>("get_compaction_detail", { sessionId });
  },

  /** Both Context cards from one backend selection/read. */
  getCompactionSnapshot(sessionId?: string): Promise<CompactionSnapshot> {
    if (!hasTauri()) return delay(mock.compactionSnapshot(), 320);
    return call<CompactionSnapshot>("get_compaction_snapshot", { sessionId });
  },

  /**
   * 這次壓縮的原文：送出去的指示，與模型寫回來的結果。
   *
   * 後端指令尚未實作（`get_compaction_transcript`）。在瀏覽器預覽台上走 mock，
   * 在桌面版上會拿到「找不到指令」的錯誤 —— 那是對的：畫面此刻該說「讀不到」，
   * 不是自己編一段內容出來充數。
   */
  getCompactionTranscript(sessionId: string): Promise<CompactionTranscript> {
    if (!hasTauri()) return delay(mock.compactionTranscript(), 280);
    return call<CompactionTranscript>("get_compaction_transcript", { sessionId });
  },

  refreshQuota(routeId: string): Promise<Overview> {
    if (!hasTauri()) return delay(mock.overview(), 600);
    return call<Overview>("refresh_quota", { routeId });
  },

  runReview(
    content: string,
    model?: string,
    endpoint?: string,
    apiKey?: string,
  ): Promise<Finding[]> {
    if (!hasTauri()) return delay([]);
    return call<Finding[]>("run_review", { content, model, endpoint, apiKey });
  },

  /**
   * One page of request rows. Event lists stay independently capped on the
   * backend; do not pass the retention window as `limit`.
   */
  getRequestLog(options?: {
    limit?: number;
    offset?: number;
    failedOnly?: boolean;
    routeId?: string;
    focusEntryId?: number;
  }): Promise<RequestLog> {
    const limit = options?.limit ?? 20;
    const offset = options?.offset ?? 0;
    if (!hasTauri()) {
      const full = mock.requestLog();
      const failedOnly = options?.failedOnly === true;
      const routeId = options?.routeId;
      const filtered = full.entries.filter(
        (entry) =>
          (!failedOnly || entry.status >= 400) &&
          (!routeId || entry.routeId === routeId),
      );
      const entries = filtered.slice(offset, offset + limit);
      return delay({
        ...full,
        entries,
        entryTotal: filtered.length,
        entryOffset: offset,
        entryRoutes: [...new Map(full.entries.map((entry) => [entry.routeId, { routeId: entry.routeId, provider: entry.provider }])).values()],
      });
    }
    return call<RequestLog>("get_request_log", {
      limit,
      offset,
      failedOnly: options?.failedOnly ?? false,
      routeId: options?.routeId,
      focusEntryId: options?.focusEntryId,
    });
  },

  getBootTelemetry(): Promise<BootTelemetry | null> {
    if (!hasTauri()) return delay(mock.bootTelemetry());
    return call<BootTelemetry | null>("get_boot_telemetry", {});
  },

  getUsageActivity(): Promise<UsageActivity> {
    if (!hasTauri()) return delay(mock.usageActivity());
    return call<UsageActivity>("get_usage_activity");
  },


  setRouteEnabled(routeId: string, enabled: boolean): Promise<Route[]> {
    if (!hasTauri()) {
      return delay(
        mock.routes().map((route) =>
          route.id === routeId ? { ...route, enabled } : route,
        ),
      );
    }
    return afterSettingsChange(call<Route[]>("set_route_enabled", { routeId, enabled }));
  },

  setRouteInsecureHttpPolicy(
    routeId: string,
    policy: InsecureHttpPolicy,
  ): Promise<Route[]> {
    if (!hasTauri()) {
      return delay(
        mock.routes().map((route) =>
          route.id === routeId ? { ...route, insecureHttpPolicy: policy } : route,
        ),
      );
    }
    return call<Route[]>("set_route_insecure_http_policy", { routeId, policy });
  },

  setRouteModels(routeId: string, models: string[]): Promise<Route[]> {
    if (!hasTauri()) {
      return delay(
        mock.routes().map((route) =>
          route.id === routeId ? { ...route, selectedModels: models } : route,
        ),
      );
    }
    return afterSettingsChange(call<Route[]>("set_route_models", { routeId, models }));
  },

  setRouteName(routeId: string, name: string): Promise<Route[]> {
    if (!hasTauri()) {
      return delay(
        mock.routes().map((route) => (route.id === routeId ? { ...route, name } : route)),
      );
    }
    return afterSettingsChange(call<Route[]>("set_route_name", { routeId, name }));
  },

  setModelDisplayName(routeId: string, model: string, name: string): Promise<Route[]> {
    if (!hasTauri()) {
      return delay(
        mock.routes().map((route) => {
          if (route.id !== routeId) return route;
          const displayName = name.trim() || null;
          const existing = route.modelCapabilities.find(
            (capability) => capability.model.toLowerCase() === model.toLowerCase(),
          );
          return {
            ...route,
            modelCapabilities: existing
              ? route.modelCapabilities.map((capability) =>
                  capability === existing ? { ...capability, displayName } : capability,
                )
              : [
                  ...route.modelCapabilities,
                  { model, contextWindow: null, wire: null, streaming: null, reasoning: null, probeVersion: null, displayName },
                ],
          };
        }),
      );
    }
    return call<Route[]>("set_model_display_name", { routeId, model, name });
  },

  setModelVision(routeId: string, model: string, vision: boolean): Promise<Route[]> {
    if (!hasTauri()) {
      return delay(
        mock.routes().map((route) => {
          if (route.id !== routeId) return route;
          const existing = route.modelCapabilities.find(
            (capability) => capability.model.toLowerCase() === model.toLowerCase(),
          );
          return {
            ...route,
            modelCapabilities: existing
              ? route.modelCapabilities.map((capability) =>
                  capability === existing ? { ...capability, vision } : capability,
                )
              : [
                  ...route.modelCapabilities,
                  { model, contextWindow: null, wire: null, streaming: null, reasoning: null, probeVersion: null, vision },
                ],
          };
        }),
      );
    }
    return afterSettingsChange(call<Route[]>("set_model_vision", { routeId, model, vision }));
  },

  deleteRoute(routeId: string): Promise<Route[]> {
    if (!hasTauri()) return delay(mock.routes().filter((route) => route.id !== routeId));
    return afterSettingsChange(call<Route[]>("delete_route", { routeId }));
  },

  getProxyStatus(): Promise<ProxyStatus> {
    if (!hasTauri()) {
      return delay({
        running: false,
        baseUrl: "http://127.0.0.1:15721/v1",
        catalogPath: null,
        codexManaged: false,
        lastError: null,
        notice: null,
        phase: "stopped",
        operationId: null,
        generation: 0,
        stage: null,
        stageElapsedMs: null,
      });
    }
    return call<ProxyStatus>("get_proxy_status");
  },

  /**
   * Generation-keyed Start/Stop stages. The UI applies only the current
   * generation and must not wait for the 60s status poll.
   */
  async subscribeProxyLifecycle(
    onStatus: (status: ProxyStatus) => void,
  ): Promise<() => void> {
    if (!hasTauri()) {
      return () => undefined;
    }
    const { listen } = await import("@tauri-apps/api/event");
    const unlisten = await listen<ProxyStatus>(PROXY_LIFECYCLE_EVENT, (event) => {
      onStatus(event.payload);
    });
    return unlisten;
  },

  getCatalogStatus(): Promise<CatalogStatus> {
    if (!hasTauri()) {
      return delay({ proxyRunning: false, injectedModelIds: [] });
    }
    return cachedQuery("catalog-status", () => call<CatalogStatus>("get_catalog_status"));
  },

  startProxy(): Promise<ProxyStatus> {
    if (!hasTauri()) {
      return delay({
        running: true,
        baseUrl: "http://127.0.0.1:15721/v1",
        catalogPath: "mock/vellum-model-catalog.json",
        codexManaged: true,
        lastError: null,
        notice: null,
        phase: "running",
        operationId: "op_mock",
        generation: 1,
        stage: null,
        stageElapsedMs: null,
      });
    }
    return afterSettingsChange(call<ProxyStatus>("start_proxy"));
  },

  stopProxyAndRestore(): Promise<ProxyStatus> {
    if (!hasTauri()) {
      return delay({
        running: false,
        baseUrl: "http://127.0.0.1:15721/v1",
        catalogPath: null,
        codexManaged: false,
        lastError: null,
        notice: null,
        phase: "stopped",
        operationId: null,
        generation: 0,
        stage: null,
        stageElapsedMs: null,
      });
    }
    return afterSettingsChange(call<ProxyStatus>("stop_proxy_and_restore"));
  },

  async repairCodexConfig(): Promise<RestoreResult> {
    if (!hasTauri()) {
      return {
        status: await this.stopProxyAndRestore(),
        changed: true,
        cleared: [
          "已停止 Vellum 本機 Proxy",
          "已還原 Codex 的供應商設定與代理網址",
          "已移除 Vellum 加進模型選單的項目",
        ],
        preserved: ["登入憑證", "聊天紀錄與工作階段", "專案分組與工作區資料"],
      };
    }
    return afterSettingsChange(call<RestoreResult>("repair_codex_config"));
  },

  exitVellum(): Promise<void> {
    if (!hasTauri()) return delay(undefined);
    return call<void>("exit_vellum");
  },

  getRuntimeStatus(): Promise<RuntimeStatus> {
    if (!hasTauri()) {
      return delay({
        proxyRunning: false,
        codexManaged: false,
        activeRequests: 0,
        draining: false,
        restartRequired: false,
        restartReasons: [],
        liveApplied: [],
        activeCatalogVersion: null,
      });
    }
    return call<RuntimeStatus>("get_runtime_status");
  },

  beginGracefulDrain(): Promise<RuntimeStatus> {
    if (!hasTauri()) return this.getRuntimeStatus().then((status) => ({ ...status, draining: true }));
    return call<RuntimeStatus>("begin_graceful_drain");
  },

  cancelGracefulDrain(): Promise<RuntimeStatus> {
    if (!hasTauri()) return this.getRuntimeStatus();
    return call<RuntimeStatus>("cancel_graceful_drain");
  },

  listCatalogVersions(): Promise<CatalogVersion[]> {
    if (!hasTauri()) return delay([]);
    return call<CatalogVersion[]>("list_catalog_versions");
  },

  rollbackCatalogVersion(versionId: string): Promise<RuntimeStatus> {
    if (!hasTauri()) return this.getRuntimeStatus();
    return call<RuntimeStatus>("rollback_catalog_version", { versionId });
  },

  /// `force` skips the check for a Codex Desktop turn still in flight. It is
  /// the user's answer to being told one is running, never a default.
  getUpdateStatus(): Promise<UpdateStatusSnapshot> {
    if (!hasTauri()) return delay(previewUpdateStatus());
    return call<UpdateStatusSnapshot>("get_update_status");
  },
  checkUpdates(component?: UpdateComponent): Promise<UpdateStatusSnapshot> {
    if (!hasTauri()) return delay(previewUpdateStatus());
    return call<UpdateStatusSnapshot>("check_updates", { component });
  },
  setUpdatePreferences(patch: Partial<UpdatePreferences>): Promise<UpdatePreferences> {
    if (!hasTauri()) return delay({ channel: "stable", autoCheck: true, autoDownload: true, coreIdleHandoff: false, ...patch });
    return afterSettingsChange(call<UpdatePreferences>("set_update_preferences", patch));
  },
  downloadUpdate(component: UpdateComponent, hostId?: string): Promise<UpdateOperation> {
    if (!hasTauri()) return delay({ operationId: "preview", component, phase: "downloading", targetVersion: null });
    return call<UpdateOperation>("download_update", { component, hostId });
  },
  applyUpdate(component: UpdateComponent, hostId?: string): Promise<UpdateOperation> {
    if (!hasTauri()) return delay({ operationId: "preview", component, phase: "waitingForRestart", targetVersion: null });
    return call<UpdateOperation>("apply_update", { component, hostId });
  },
  cancelUpdateDownload(component: UpdateComponent): Promise<UpdateOperation> {
    if (!hasTauri()) return delay({ operationId: "preview", component, phase: "available", targetVersion: null });
    return call<UpdateOperation>("cancel_update_download", { component });
  },
  rollbackUpdate(component: UpdateComponent): Promise<UpdateOperation> {
    if (!hasTauri()) return delay({ operationId: "preview", component, phase: "rolledBack", targetVersion: null });
    return call<UpdateOperation>("rollback_update", { component });
  },
  setRemoteUpdatePolicy(hostId: string, idleAutoUpdate: boolean): Promise<{ hostId: string; idleAutoUpdate: boolean }> {
    if (!hasTauri()) return delay({ hostId, idleAutoUpdate });
    return afterSettingsChange(call("set_remote_update_policy", { hostId, idleAutoUpdate }));
  },

  restartCodexSafely(force = false): Promise<RestartResult> {
    if (!hasTauri())
      return delay({ restarted: false, notice: { code: "restartUnavailableInPreview", params: {} } });
    return call<RestartResult>("restart_codex_safely", { force });
  },

  getEnhancedDesktopRuntimeStatus(): Promise<EnhancedDesktopRuntimeStatus> {
    if (!hasTauri()) {
      return delay(previewEnhancedRuntimeStatus());
    }
    return call<EnhancedDesktopRuntimeStatus>("get_enhanced_desktop_runtime_status");
  },

  getEnhancedRuntimeObservations(): Promise<RuntimeObservations> {
    if (!hasTauri()) return delay(EMPTY_OBSERVATIONS);
    return call<RuntimeObservations>("list_enhanced_runtime_sessions");
  },
  recheckEnhancedRuntimeCompatibility(): Promise<EnhancedDesktopRuntimeStatus> {
    if (!hasTauri()) return delay(previewEnhancedRuntimeStatus());
    return call<EnhancedDesktopRuntimeStatus>("recheck_enhanced_runtime_compatibility");
  },
  exportEnhancedRuntimeDiagnostics(): Promise<string> {
    if (!hasTauri()) return Promise.reject(new Error("Vellum Desktop required"));
    return call<string>("export_enhanced_runtime_diagnostics");
  },
  exportVellumLogs(): Promise<string> {
    if (!hasTauri()) return Promise.reject(new Error("Vellum Desktop required"));
    return call<string>("export_vellum_logs");
  },

  configureEnhancedDesktopRuntime(input: {
    officialCodexExecutable: string;
    enabled: boolean;
  }): Promise<EnhancedDesktopRuntimeStatus> {
    if (!hasTauri()) {
      return delay({
        ...previewEnhancedRuntimeStatus(),
        configured: true,
        enabled: input.enabled,
        officialCodexExecutable: input.officialCodexExecutable,
        blockers: ["Enhanced Runtime requires Vellum Desktop"],
      });
    }
    return call<EnhancedDesktopRuntimeStatus>("configure_enhanced_desktop_runtime", input);
  },

  disableEnhancedDesktopRuntime(): Promise<EnhancedDesktopRuntimeStatus> {
    if (!hasTauri()) return delay(previewEnhancedRuntimeStatus());
    return call<EnhancedDesktopRuntimeStatus>("disable_enhanced_desktop_runtime");
  },

  runEnhancedInstalledGate(): Promise<EnhancedQualificationResult> {
    if (!hasTauri()) {
      return Promise.reject(new Error("The installed gate requires Vellum Desktop"));
    }
    return call<EnhancedQualificationResult>("run_enhanced_installed_gate");
  },

  repairRemoteManager(hostId: string, operationId: string): Promise<unknown> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<unknown>("remote_manager_repair", { hostId, operationId });
  },

  createRemoteSupportBundle(hostId: string): Promise<unknown> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<unknown>("remote_manager_support_bundle", { hostId });
  },

  installRemotePinnedCodex(hostId: string, operationId: string): Promise<unknown> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<unknown>("install_remote_pinned_codex", { hostId, operationId });
  },

  updateRemoteComponents(hostId: string, operationId: string): Promise<unknown> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<unknown>("update_remote_components", { hostId, operationId });
  },

  getRemoteDesiredState(hostId: string): Promise<RemoteHostDesiredState> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteHostDesiredState>("get_remote_desired_state", { hostId });
  },

  getRemoteSessionSummary(hostId: string): Promise<RemoteSessionSummary> {
    if (!hasTauri()) return Promise.reject(new Error("Remote Manager requires Tauri"));
    return call<RemoteSessionSummary>("remote_session_summary", { hostId });
  },
};
