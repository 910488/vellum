declare module "../qa/matrix/v1.mjs" {
  export const MATRIX_VERSION: string;
  export const TAB_CONTROL: Record<string, string>;
  export const VERDICTS: readonly string[];
  export const REQUIRED_TABS: readonly string[];
  export const REQUIRED_SURFACES: readonly string[];
  export const REQUIRED_DOMAINS: readonly string[];
  export const PASS_CLAIM: {
    platforms: string[];
    excludePlatforms: string[];
    officialLive: string;
    tabSyncRule: string;
  };
  export type MatrixCase = {
    id: string;
    domain: string;
    surface: string;
    lane: string;
    necessary: boolean;
    preconditions: string;
    steps: string;
    expected: string;
    verificationSources: string[];
    automation: string;
    evidence: string[];
    control?: string;
    officialLive?: boolean;
    defaultVerdict?: string;
    unobservedMeans?: string;
    macosInPassClaim?: boolean;
  };
  export const CASES: MatrixCase[];
  export const MATRIX: {
    version: string;
    passClaim: typeof PASS_CLAIM;
    requiredTabs: readonly string[];
    requiredSurfaces: readonly string[];
    requiredDomains: readonly string[];
    verdicts: readonly string[];
    cases: MatrixCase[];
  };
}

declare module "../scripts/qa/lib/commands.mjs" {
  export const OFFLINE_COMMANDS: { id: string; argv: string[]; log: string }[];
}
