export type ProviderPreset = "custom" | "opencodeZen" | "opencodeGo";

export const OPENCODE_ZEN_BASE_URL = "https://opencode.ai/zen/v1";
export const OPENCODE_ZEN_NAME = "OpenCode Zen";
/**
 * OpenCode Go is a distinct, smaller catalog behind its own endpoint — not a
 * filtered view of Zen. The official console serves `/zen/v1/models` from a
 * "full" model list and `/zen/go/v1/models` from a separate "lite" one, so a
 * Go-subscription key can list the full catalog without error while most of
 * it (Claude, Gemini, GPT, …) is not actually part of what that subscription
 * grants. Picking this preset instead of OpenCode Zen keeps those models out
 * of the picker entirely rather than showing entries that will never work.
 */
export const OPENCODE_GO_BASE_URL = "https://opencode.ai/zen/go/v1";
export const OPENCODE_GO_NAME = "OpenCode Go";

function isOpenCodeZenPath(path: string): boolean {
  return path === "/zen/v1" || path === "/zen/v1/models";
}

function isOpenCodeGoPath(path: string): boolean {
  return path === "/zen/go/v1" || path === "/zen/go/v1/models";
}

export function isOpenCodeZenEndpoint(endpoint: string): boolean {
  try {
    const url = new URL(endpoint);
    const path = url.pathname.replace(/\/+$/, "").toLowerCase();
    return (
      url.protocol === "https:" &&
      url.hostname.toLowerCase() === "opencode.ai" &&
      url.port === "" &&
      url.username === "" &&
      url.password === "" &&
      (isOpenCodeZenPath(path) || isOpenCodeGoPath(path))
    );
  } catch {
    return false;
  }
}

export function providerPresetDraft(preset: ProviderPreset): {
  name: string;
  baseUrl: string;
  apiKeyRequired: boolean;
} {
  if (preset === "opencodeZen") {
    return {
      name: OPENCODE_ZEN_NAME,
      baseUrl: OPENCODE_ZEN_BASE_URL,
      apiKeyRequired: false,
    };
  }
  if (preset === "opencodeGo") {
    return {
      name: OPENCODE_GO_NAME,
      baseUrl: OPENCODE_GO_BASE_URL,
      apiKeyRequired: true,
    };
  }
  return { name: "", baseUrl: "", apiKeyRequired: false };
}
