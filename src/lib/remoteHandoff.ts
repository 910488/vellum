/** Backend handoff contracts. SSH targets and model settings stay in Rust. */
export interface RemoteHandoffRequirements {
  requiredOs?: "windows" | "linux" | "darwin";
  tools?: ("git" | "python" | "python3" | "node" | "pnpm" | "cargo" | "dotnet")[];
}

export interface RemoteHandoffThread {
  threadId: string;
  name: string;
  repo: string;
  branch: string;
  updatedAt: number;
  model: string | null;
  modelProvider: string | null;
  effort: string | null;
  active: boolean | null;
  handedOff: boolean;
  draft?: { task: string; originalRequest: string; lastProgress: string } | null;
}

/** No model call: selected rollout excerpts only. The task is always first. */
export function handoffDraft(draft: RemoteHandoffThread["draft"], t: (key: string) => string): string {
  if (!draft?.task.trim()) return "";
  const background = [
    draft.originalRequest ? `${t("handoff.draftOriginal")}\n${draft.originalRequest}` : "",
    draft.lastProgress ? `${t("handoff.draftProgress")}\n${draft.lastProgress}` : "",
  ].filter(Boolean).join("\n\n");
  return background ? `${draft.task}\n\n${t("handoff.draftBackground")}\n${t("handoff.draftCaution")}\n\n${background}` : draft.task;
}

export interface RemoteHandoffChange {
  path: string;
  staged: boolean;
  untracked: boolean;
}

export interface RemoteHandoffPreflight {
  thread: RemoteHandoffThread;
  changes: RemoteHandoffChange[];
  blockers: string[];
  warnings: string[];
  remoteOs: string | null;
  localRefNotice: string;
}

export interface RemoteHandoffRecord {
  id: string;
  hostId: string;
  source: RemoteHandoffThread;
  snapshot: { base: string; commit: string; localRef: string; changes: RemoteHandoffChange[] };
  request: Record<string, unknown>;
  stage: "localSnapshot" | "remotePrepared" | "snapshotPushed" | "startUncertain" | "adopted" | "running";
  remote: {
    handoffId: string;
    phase: string;
    state: string;
    threadId: string | null;
    turnId: string | null;
    turnStatus: string | null;
    plan: string[];
    recentOutput: string;
    summary: string;
    capture: unknown;
  } | null;
  error: string | null;
  result: {
    captureOperation: string | null;
    capture: {
      resultRef: string; commit: string; snapshotCommit: string; partial: boolean;
      diffstat: { path: string; added: number | null; deleted: number | null }[];
    } | null;
    fetched: boolean;
    outcome: "applied" | "branched" | "discarded" | null;
    branch: string | null;
    applyIntent: boolean;
    cleaned: boolean;
    acknowledged: boolean;
  };
}

export interface RemoteHandoffReview { canApply: boolean; patch: string }

export function handoffTerminal(record: RemoteHandoffRecord): boolean {
  return ["completed", "failed", "interrupted"].includes(record.remote?.state ?? "");
}
export function handoffState(record: RemoteHandoffRecord): string {
  if (record.result.outcome) return record.result.outcome;
  if (record.remote?.state === "completed") return "back";
  if (record.remote?.state === "failed" || record.remote?.state === "interrupted") return "partial";
  if (record.stage === "running") return "running";
  return "uncertain";
}
