/**
 * Shared state vocabulary. Helpers return semantic keys; components translate.
 */

import type { ProviderKind } from "@/types";

export type LiveState = "live" | "pending" | "off";

export interface StateBadge {
  state: LiveState;
  labelKey: string;
  tone: "ok" | "warn" | "quiet";
  remedyKey: string | null;
}

export const REMEDY = {
  startProxy: "status.remedy.startProxy",
  restartProxy: "status.remedy.restartProxy",
  restartCodex: "status.remedy.restartCodex",
} as const;

const LIVE: StateBadge = {
  state: "live",
  labelKey: "status.live",
  tone: "ok",
  remedyKey: null,
};
const OFF: StateBadge = {
  state: "off",
  labelKey: "status.off",
  tone: "quiet",
  remedyKey: null,
};

function pending(remedyKey: string): StateBadge {
  return {
    state: "pending",
    labelKey: "status.pending",
    tone: "warn",
    remedyKey,
  };
}

export function providerState(facts: {
  enabled: boolean;
  proxyRunning: boolean;
  applied: boolean;
}): StateBadge {
  if (!facts.proxyRunning) return facts.enabled ? pending(REMEDY.startProxy) : OFF;
  if (facts.enabled && facts.applied) return LIVE;
  if (!facts.enabled && !facts.applied) return OFF;
  return pending(REMEDY.restartProxy);
}

export const NONE_KEY = "common.none";
/** @deprecated use NONE_KEY + t(); kept for transitional call sites */
export const NONE = "—";

export function supportsQuota(kind: ProviderKind): boolean {
  return kind === "official" || kind === "grokCli";
}

export function quotaUnavailableReasonKey(kind: ProviderKind): string {
  return supportsQuota(kind) ? "" : "vocabulary.quotaUnavailable";
}

export function quotaUnavailableReason(kind: ProviderKind, t?: (key: string) => string): string {
  const key = quotaUnavailableReasonKey(kind);
  if (!key) return "";
  return t ? t(key) : key;
}
