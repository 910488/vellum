#!/usr/bin/env node
/**
 * Unified Vellum QA entry: pnpm qa -- --lane offline|desktop|remote|live|all
 */
import { mkdirSync } from "node:fs";
import path from "node:path";
import { parseQaArgs, usage, validateInject, validateLane } from "./lib/args.mjs";
import { REPO_ROOT } from "./lib/commands.mjs";
import { runQa } from "./lib/orchestrator.mjs";

const args = parseQaArgs(process.argv.slice(2));

if (args.help) {
  process.stdout.write(`${usage()}\n`);
  process.exit(0);
}

try {
  if (args.inject) validateInject(args.inject);
  else validateLane(args.lane);
} catch (error) {
  process.stderr.write(`${error.message}\n${usage()}\n`);
  process.exit(error.exitCode ?? 2);
}

const stamp = new Date().toISOString().replace(/[:.]/g, "-");
const defaultDir = path.join(
  REPO_ROOT,
  "qa",
  "reports",
  "generated",
  args.inject ? `inject-${args.inject}-${stamp}` : `${args.lane}-${stamp}`,
);
const outDir = args.outDir ? path.resolve(args.outDir) : defaultDir;
mkdirSync(outDir, { recursive: true });

if (args.lane === "sandbox" && !args.inject && !args.buildInfo && !args.dryRun) {
  process.stderr.write(`sandbox lane requires --build-info <path>\n${usage()}\n`);
  process.exit(2);
}

const result = await runQa({
  lane: args.lane,
  inject: args.inject,
  outDir,
  dryRun: args.dryRun,
  buildInfo: args.buildInfo,
  env: process.env,
});

process.stdout.write(
  JSON.stringify(
    {
      exitCode: result.exitCode,
      json: result.jsonPath,
      html: result.htmlPath,
      fullAcceptance: result.report.fullAcceptance,
      counts: result.report.counts,
      git: result.report.git?.commit,
    },
    null,
    2,
  ) + "\n",
);

process.exit(result.exitCode);
