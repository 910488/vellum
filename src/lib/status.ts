/**
 * System status decoding. Headline labels are translation keys.
 */
import type { EnhancedDesktopRuntimeStatus, Overview, ProxyStatus, RuntimeNotice, RuntimeStatus, UpdateAttention, UpdateStatusSnapshot } from "@/types";
import type { ScreenId } from "@/screens/registry";

export interface SystemStatus {
  proxy: ProxyStatus | null;
  runtime: RuntimeStatus | null;
  enhancedRuntime?: EnhancedDesktopRuntimeStatus | null;
  overview: Overview | null;
  updates?: UpdateStatusSnapshot | null;
}

export const EMPTY_STATUS: SystemStatus = { proxy: null, runtime: null, enhancedRuntime: null, overview: null, updates: null };

export interface Headline {
  tone: "ok" | "warn" | "quiet";
  labelKey: string;
  model: string | null;
  provider: string | null;
  routeSource: "telemetry" | "default" | null;
  modelIsLive: boolean;
  endpoint: string | null;
  quotaRemaining: number | null;
  restartRequired: boolean;
  /** 真的壞掉時的原文，直接顯示。 */
  error: string | null;
  /** 需要處理、但不是故障的狀態。前端自己組句子。 */
  notice: RuntimeNotice | null;
  updateAttention: UpdateAttention;
}

export function headline({ proxy, runtime, enhancedRuntime, overview, updates }: SystemStatus): Headline {
  const defaultRoute = overview?.route ?? null;
  const telemetry = overview?.lastSuccessfulRoute ?? null;
  const routeId = telemetry?.routeId ?? defaultRoute?.id ?? null;
  const base = {
    model: telemetry?.model ?? defaultRoute?.model ?? null,
    provider: telemetry?.provider ?? defaultRoute?.name ?? null,
    routeSource: telemetry ? ("telemetry" as const) : defaultRoute ? ("default" as const) : null,
    endpoint: proxy?.running ? proxy.baseUrl : null,
    quotaRemaining:
      overview?.quota && routeId === overview.quota.routeId
        ? 100 - overview.quota.usedPercent
        : null,
    restartRequired: (runtime?.restartRequired ?? false) || (enhancedRuntime?.restartRequired ?? false),
    error: proxy?.lastError ?? null,
    notice: proxy?.notice ?? null,
    updateAttention: updates?.attention ?? "none",
  };

  if (!proxy) return { ...base, modelIsLive: false, tone: "quiet", labelKey: "status.reading" };
  if (!proxy.running) {
    return { ...base, modelIsLive: false, tone: "quiet", labelKey: "status.proxyStopped" };
  }
  if (!proxy.codexManaged) {
    return { ...base, modelIsLive: false, tone: "warn", labelKey: "status.codexNotManaged" };
  }
  // A pending restart is a separate, actionable fact rendered by StatusBar's
  // right-hand pill. It must not replace the left-hand transport headline:
  // doing so repeats the same warning twice and hides whether the Proxy is
  // actually live. The currently loaded route remains live until the restart
  // applies the next catalog/runtime state.
  return { ...base, modelIsLive: true, tone: "ok", labelKey: "status.live" };
}

export function attention({ runtime, overview }: SystemStatus): Partial<Record<ScreenId, number>> {
  const result: Partial<Record<ScreenId, number>> = {};
  const findings = overview?.findings.length ?? 0;
  if (findings > 0) result.today = findings;
  const modelReasons = (runtime?.restartReasons ?? []).filter((reason) =>
    reason.code !== "desktopUpdateReady" && reason.code !== "remoteUpdateReady",
  );
  if (runtime?.restartRequired && modelReasons.length) {
    result.models = Math.max(1, modelReasons.length);
  } else if (runtime?.restartRequired && (runtime.restartReasons.length === 0)) {
    result.models = 1;
  }
  return result;
}
