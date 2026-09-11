export const LANES = Object.freeze(["offline", "desktop", "remote", "live", "sandbox", "all"]);
export const INJECTS = Object.freeze([
  "timeout",
  "budget-stop",
  "missing-credentials",
  "missing-evidence",
  "wrong-evidence-type",
  "png-header-only",
  "json-semantic-fail",
  "stale-evidence",
  "wrong-model",
  "button-state-unchanged",
  "candidate-as-live",
  "wrong-installer",
  "cleanup-failure",
]);

export function parseQaArgs(argv = process.argv.slice(2)) {
  const out = {
    lane: null,
    inject: null,
    outDir: null,
    buildInfo: null,
    dryRun: false,
    help: false,
    extra: [],
  };
  for (let i = 0; i < argv.length; i += 1) {
    const token = argv[i];
    if (token === "--help" || token === "-h") {
      out.help = true;
      continue;
    }
    if (token === "--dry-run") {
      out.dryRun = true;
      continue;
    }
    if (token === "--lane") {
      const next = argv[i + 1];
      out.lane = next && !next.startsWith("--") ? next : "";
      if (next && !next.startsWith("--")) i += 1;
      continue;
    }
    if (token.startsWith("--lane=")) {
      out.lane = token.slice("--lane=".length);
      continue;
    }
    if (token === "--inject") {
      const next = argv[i + 1];
      out.inject = next && !next.startsWith("--") ? next : "";
      if (next && !next.startsWith("--")) i += 1;
      continue;
    }
    if (token.startsWith("--inject=")) {
      out.inject = token.slice("--inject=".length);
      continue;
    }
    if (token === "--out-dir") {
      const next = argv[i + 1];
      out.outDir = next && !next.startsWith("--") ? next : "";
      if (next && !next.startsWith("--")) i += 1;
      continue;
    }
    if (token.startsWith("--out-dir=")) {
      out.outDir = token.slice("--out-dir=".length);
      continue;
    }
    if (token === "--build-info") {
      const next = argv[i + 1];
      out.buildInfo = next && !next.startsWith("--") ? next : "";
      if (next && !next.startsWith("--")) i += 1;
      continue;
    }
    if (token.startsWith("--build-info=")) {
      out.buildInfo = token.slice("--build-info=".length);
      continue;
    }
    out.extra.push(token);
  }
  return out;
}

export function usage() {
  return [
    "usage: pnpm qa -- --lane offline|desktop|remote|live|sandbox|all [--out-dir DIR] [--build-info PATH] [--dry-run]",
    "       pnpm qa -- --inject timeout|budget-stop|missing-credentials|missing-evidence|wrong-evidence-type|png-header-only|json-semantic-fail|stale-evidence|wrong-model|button-state-unchanged|candidate-as-live|wrong-installer|cleanup-failure",
  ].join("\n");
}

export function validateLane(lane) {
  if (lane == null || lane === "") {
    const error = new Error("missing --lane");
    error.code = "MISSING_LANE";
    error.exitCode = 2;
    throw error;
  }
  if (!LANES.includes(lane)) {
    const error = new Error(`unknown lane: ${lane}`);
    error.code = "UNKNOWN_LANE";
    error.exitCode = 2;
    throw error;
  }
  return lane;
}

export function validateInject(inject) {
  if (inject == null || inject === "") return null;
  if (!INJECTS.includes(inject)) {
    const error = new Error(`unknown inject: ${inject}`);
    error.code = "UNKNOWN_INJECT";
    error.exitCode = 2;
    throw error;
  }
  return inject;
}
