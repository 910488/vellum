import { invoke } from "@tauri-apps/api/core";

export interface ClaudeCodeStatus {
  running: boolean;
  /** Leased project folders; empty for the user scope. */
  projects: string[];
  /** Whose settings the lease wrote: `~/.claude/settings.json` or one project's. */
  scope: ClaudeCodeScope | null;
  port: number | null;
  settingsMatch: boolean;
  recoveryRequired: boolean;
  notice: string | null;
  trust: { supported: boolean; sha256: string | null; trusted: boolean; managed: boolean; recoveryRequired: boolean };
  routing: {
    connectRequests?: number;
    tlsFailures?: number;
    apiRequests?: number;
    lastError?: string | null;
    activeRequests: number;
    desktopRequests: number;
    records: { id: string; sessionHash: string; accountId: string | null; operation?: "messages" | "count_tokens"; state: string; upstreamStatus: number | null }[];
  };
  accounts: {
    accounts: { id: string; label: string; expiresAt: number; reauthorizeRequired: boolean; gate?: ClaudeCodeGate; gated?: boolean; usage?: ClaudeCodeUsage | null;
      /** Start the next 5-hour window right after each reset. */
      maintainFiveHour?: boolean; fiveHourError?: string | null }[];
    selected: string | null;
    switchedFrom?: string | null;
    loginPending: boolean;
    loginError: string | null;
  };
}

export type ClaudeCodeScope = "user" | "project";

export interface ClaudeCodeWindow { utilization: number; resetsAt: string | null }
export interface ClaudeCodeUsage {
  observedAt: number;
  fiveHour: ClaudeCodeWindow | null;
  sevenDay: ClaudeCodeWindow | null;
  /** Weekly Fable share included in the plan; absent when the account has none. */
  fable?: ClaudeCodeWindow | null;
  /** Usage credits (extra usage) on or off, as the usage endpoint last said. */
  creditsEnabled?: boolean | null;
}

/** Percent of each window to keep; 0 leaves it ungated. */
export interface ClaudeCodeGate { fiveHour: number; sevenDay: number }

export interface ClaudeCodeRecentProject { path: string; sessions: number; lastActivityAt: number | null }

export const claudeCodeApi = {
  recentProjects: () => invoke<ClaudeCodeRecentProject[]>("claude_code_recent_projects"),
  status: () => invoke<ClaudeCodeStatus>("claude_code_status"),
  prepareCertificate: () => invoke<ClaudeCodeStatus>("claude_code_prepare_certificate"),
  trustCertificate: (expectedSha256: string) => invoke<ClaudeCodeStatus>("claude_code_trust_certificate", { expectedSha256 }),
  restoreTrust: () => invoke<ClaudeCodeStatus>("claude_code_restore_trust"),
  start: (projects: string[]) => invoke<ClaudeCodeStatus>("claude_code_start", { projects }),
  startUser: () => invoke<ClaudeCodeStatus>("claude_code_start_user"),
  stop: (force = false) => invoke<ClaudeCodeStatus>("claude_code_stop", { force }),
  select: (accountId: string | null) => invoke<ClaudeCodeStatus>("claude_code_select_account", { accountId }),
  login: () => invoke<void>("claude_code_begin_login"),
  cancelLogin: () => invoke<void>("claude_code_cancel_login"),
  remove: (accountId: string) => invoke<ClaudeCodeStatus>("claude_code_remove_account", { accountId }),
  setGate: (accountId: string, gate: ClaudeCodeGate) => invoke<ClaudeCodeStatus>("claude_code_set_gate", { accountId, gate }),
  usage: (accountId: string) => invoke<ClaudeCodeStatus>("claude_code_refresh_usage", { accountId }),
  setFiveHour: (accountId: string, enabled: boolean) => invoke<ClaudeCodeStatus>("claude_code_set_five_hour", { accountId, enabled }),
};
