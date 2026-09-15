import type {
  CodexOAuthStatus,
  GrokAccountStatus,
  ModelCapability,
  ProbeResult,
  Route,
} from "@/types";
import type { ConnectKind, OnboardingConnection } from "@/screens/Onboarding";

export const ONBOARDING_COMPLETED_KEY = "vellum.onboarding.completed.v1";

/**
 * 首次設定畫面出不出場。
 *
 * 元件、接線、i18n 資源都留著沒動，開關就只是這一個常數，不必動 App.tsx。
 * 開發預覽台（`preview/onboarding.html`）走的是元件直連，不受這裡影響，
 * 關掉期間照樣看得到。
 */
export const ONBOARDING_ENABLED = false;

export function onboardingWasCompleted(storage: Pick<Storage, "getItem">): boolean {
  return storage.getItem(ONBOARDING_COMPLETED_KEY) === "1";
}

/**
 * app 開起來要不要先走首次設定。
 *
 * 注意這個關法不會去寫「已完成」旗標，所以測試版使用者的 localStorage 仍是
 * 空的——等開關打開，他們下次啟動就會看到 onboarding。對沒設定過的人來說
 * 這是對的行為。
 */
export function shouldShowOnboarding(storage: Pick<Storage, "getItem">): boolean {
  return ONBOARDING_ENABLED && !onboardingWasCompleted(storage);
}

export function markOnboardingCompleted(storage: Pick<Storage, "setItem">): void {
  storage.setItem(ONBOARDING_COMPLETED_KEY, "1");
}

/**
 * Models exposed to Codex must produce a native, typed function call. Merely
 * accepting a `tools` field is insufficient: without a configured tool parser
 * some vLLM/llama.cpp deployments emit `<tool_call>` as assistant text.
 */
export function codexCompatibleProbeModels(result: ProbeResult): ModelCapability[] {
  return result.modelCapabilities.filter(
    (capability) => capability.toolCalling === true && capability.wire !== null,
  );
}

export function buildOnboardingConnections(
  routes: Route[],
  oauth: CodexOAuthStatus | null,
  grok: GrokAccountStatus | null,
  grokUnavailableLabel: string,
): Record<ConnectKind, OnboardingConnection> {
  const grokRouteAvailable = routes.some((route) => route.providerKind === "grokCli");
  return {
    chatgpt: {
      accounts:
        oauth?.accounts.map(
          (account) =>
            `${account.email ?? account.accountId} · ${
              account.workspaceName ?? account.planType ??
              `Workspace ${(account.workspaceId ?? account.accountId).slice(0, 8)}`
            }`,
        ) ?? [],
      pending: null,
    },
    grok: {
      accounts:
        grok?.accounts.map(
          (account) => account.email ?? `Grok ${account.accountId.slice(-8)}`,
        ) ?? [],
      pending: null,
      unavailable:
        !grokRouteAvailable && !(grok?.accounts.length) ? grokUnavailableLabel : null,
    },
    custom: {
      accounts: routes
        .filter((route) => route.providerKind === "openAiCompatible")
        .map((route) => route.name),
      pending: null,
    },
  };
}
