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
}
