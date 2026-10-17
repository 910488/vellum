import type { RemoteOperationProgress } from "@/types";
import { operationalErrorKey } from "./operationalError";
import {
  describeBlocker, hostVerdict, operationKindKey, phaseKey,
  proxyMigrationFailure, deploymentVerificationFailure, deploymentPreflightFailure, psStartTimeFailure,
  type RemoteRemedy,
} from "./remoteVocabulary";

export type WorkRemedy = RemoteRemedy | "retry" | "inspect" | "syncDesktopCodex";
export type WorkState = "pending" | "waiting" | "running" | "failed" | "ready";

export interface WorkSurfaceInput {
  managerState: string;
  agentReachable: boolean;
  persistenceScope?: string | null;
  operation: RemoteOperationProgress | null;
  error?: string | null;
  blockers: string[];
  desktopCodexState?: string | null;
}

export interface WorkSurfaceModel {
  state: WorkState;
  statusKey: string;
  primaryKey: string;
  primaryDisabled: boolean;
  progress: { percent: number; phaseKey: string | null; phase: string; kindKey: string | null; kind: string } | null;
  remedy: { action: WorkRemedy; messageKey: string; repair: string | null } | null;
}

/** Presentation only: never clears a blocker or authorizes a backend action. */
export function remoteWorkSurface(input: WorkSurfaceInput): WorkSurfaceModel {
  const verdict = hostVerdict(input);
  const base: WorkSurfaceModel = {
    state: "pending", statusKey: "remoteWork.pending",
    primaryKey: verdict.primary === "bootstrap" ? "remote.act.bootstrap" : "remoteWork.redeploy",
    primaryDisabled: false, progress: null, remedy: null,
  };
  const operation = input.operation;
  if (operation?.state === "running") {
    const waiting = operation.phase === "waitingForIdle" || operation.phase === "awaitingTakeover";
    return {
      ...base, state: waiting ? "waiting" : "running", primaryDisabled: true,
      statusKey: waiting ? `remote.phase.${operation.phase}` : operation.phase === "observing" ? "remote.phase.observing" : "remoteWork.running",
      progress: { percent: Math.min(100, Math.max(0, operation.percent)), phaseKey: phaseKey(operation.phase), phase: operation.phase, kindKey: operationKindKey(operation.kind), kind: operation.kind },
    };
  }
  const failure = input.error || (operation?.state === "failed" ? operation.message || "unknown" : null) || input.blockers[0];
  if (failure) {
    // Match complete codes, not substrings (e.g. a future code containing a known one).
    const translatedCode = proxyMigrationFailure(failure) ?? deploymentVerificationFailure(failure);
    const candidates = translatedCode
      ? [describeBlocker(translatedCode.replace("remote.blocker.", ""))]
      : (failure.match(/[A-Za-z][A-Za-z0-9_]*/g) ?? []).map(describeBlocker);
    const blocker = candidates.find((item) => item.messageKey);
    const ps = psStartTimeFailure(failure);
    const desktopProtocol = failure.includes("CodexDesktopProtocolMismatch");
    const protocolRemedy = input.desktopCodexState === "agentUpdateRequired" ? "updateAgent"
      : !input.desktopCodexState || ["desktopUnavailable", "unavailable"].includes(input.desktopCodexState) ? "inspect" : "syncDesktopCodex";
    return {
      ...base, state: "failed", statusKey: "remoteWork.failed", primaryDisabled: true,
      remedy: {
        action: desktopProtocol ? protocolRemedy : blocker
          ? ["repair", "installCodex", "updateAgent"].includes(blocker.remedy ?? "") ? "bootstrap"
            : blocker.remedy === "plan" ? "inspect" : blocker.remedy ?? "inspect"
          : input.error || operation?.state === "failed" ? "retry" : "inspect",
        messageKey: operationalErrorKey(failure) === "tabPolish.loginExpired" ? "tabPolish.loginExpired"
          : desktopProtocol ? "remote.error.desktopCodexMismatch"
          : failure.includes("RemoteBoundaryKeyProvisionFailed") ? "remote.error.boundaryKeyProvisionFailed"
          : blocker?.messageKey ?? deploymentPreflightFailure(failure)
            ?? (input.error || operation?.state === "failed" ? "remoteWork.retryHint" : "remote.blockerUnknown"),
        repair: ps?.repair ?? null,
      },
    };
  }
  if (input.agentReachable && (input.managerState === "detachedReady" || input.managerState === "nativeActive")) {
    return { ...base, state: "ready", statusKey: input.persistenceScope === "onDemand" ? "remoteWork.readyOnDemand" : verdict.verdictKey };
  }
  return { ...base, statusKey: input.agentReachable ? "remoteWork.pending" : verdict.verdictKey };
}
