#!/usr/bin/env node
import { readFileSync, mkdirSync } from "node:fs";
import path from "node:path";
import { buildReport, loadPriorFailures, writeReports } from "./lib/report.mjs";
import { gitIdentity, artifactIdentity } from "./lib/git.mjs";
import { MATRIX_VERSION } from "../../qa/matrix/v1.mjs";
import { REPO_ROOT } from "./lib/commands.mjs";

const outDir = process.argv[2] || path.join(REPO_ROOT, "qa", "reports", "first-round");
const inputs = process.argv.slice(3);
if (!inputs.length) {
  process.stderr.write("usage: node scripts/qa/merge-reports.mjs OUT_DIR report.json...\n");
  process.exit(2);
}

const cases = [];
const commands = [];
const defects = [];
const unverifiedLive = [];
for (const file of inputs) {
  const report = JSON.parse(readFileSync(file, "utf8"));
  cases.push(...(report.cases ?? []));
  commands.push(...(report.commands ?? []));
  defects.push(...(report.defects ?? []));
  unverifiedLive.push(...(report.unverifiedLive ?? []));
}

const historyDir = path.join(REPO_ROOT, "qa", "reports", "first-round");
mkdirSync(outDir, { recursive: true });
const priorFailures = [
  ...loadPriorFailures(path.join(historyDir, "prior-offline-fail.json")),
  ...loadPriorFailures(path.join(historyDir, "prior-remote-fail.json")),
  ...loadPriorFailures(path.join(historyDir, "prior-remote-false-pass.json")),
];
const report = buildReport({
  lane: "all",
  matrixVersion: MATRIX_VERSION,
  git: gitIdentity(),
  artifacts: artifactIdentity(),
  cases,
  defects,
  unverifiedLive,
  priorFailures,
  commands,
});
const paths = writeReports(outDir, report);
process.stdout.write(`${JSON.stringify({ ...paths, fullAcceptance: report.fullAcceptance, counts: report.counts }, null, 2)}\n`);
process.exit(report.fullAcceptance ? 0 : 1);
