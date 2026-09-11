import { mkdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import path from "node:path";
import { redactDeep, redactText } from "./sanitize.mjs";
import { fullAcceptance, summarize } from "./verdict.mjs";

function escapeHtml(value) {
  return String(value ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

export function loadPriorFailures(historyPath) {
  if (!historyPath || !existsSync(historyPath)) return [];
  try {
    const prior = JSON.parse(readFileSync(historyPath, "utf8"));
    const rows = [];
    for (const item of prior.cases ?? []) {
      if (item.verdict === "FAIL") {
        rows.push({
          id: item.id,
          verdict: item.verdict,
          reason: item.reason,
          detail: item.detail,
          recordedAt: prior.generatedAt,
        });
      }
    }
    if (Array.isArray(prior.priorFailures)) rows.push(...prior.priorFailures);
    return rows;
  } catch {
    return [];
  }
}

export function rejectMergeProblems(incomingCases, { runId, runStartedAt } = {}) {
  const seen = new Set();
  const problems = [];
  for (const item of incomingCases ?? []) {
    if (!item?.id) {
      problems.push({ kind: "missing-id" });
      continue;
    }
    if (seen.has(item.id)) problems.push({ kind: "duplicate", id: item.id });
    seen.add(item.id);
    if (item.verdict === "PASS") {
      if (!item.observedAt && !item.durationMs) problems.push({ kind: "stale-or-unobserved", id: item.id });
      if (runStartedAt && item.observedAt && new Date(item.observedAt) < new Date(runStartedAt) - 1000) {
        problems.push({ kind: "stale-evidence", id: item.id });
      }
      if (runId && item.runId && item.runId !== runId) {
        problems.push({ kind: "wrong-run", id: item.id, runId: item.runId });
      }
    }
  }
  return problems;
}

export function buildReport({
  lane,
  matrixVersion,
  git,
  artifacts,
  cases,
  usage,
  defects,
  unverifiedLive,
  priorFailures,
  commands,
  runId = null,
  environment = null,
  sources = null,
  matrixCoverage = null,
}) {
  const counts = summarize(cases);
  const acceptance = fullAcceptance(cases) && matrixCoverage !== false;
  const report = {
    schemaVersion: 2,
    matrixVersion,
    generatedAt: new Date().toISOString(),
    runId,
    lane,
    git,
    artifacts,
    environment,
    sources,
    macosInPassClaim: false,
    officialLive: "NOT_RUN",
    fullAcceptance: acceptance,
    verdict: acceptance ? "PASS" : counts.FAIL ? "FAIL" : counts.BLOCKED ? "BLOCKED" : "NOT_RUN",
    counts,
    usage: usage ?? {},
    cases,
    defects: defects ?? [],
    unverifiedLive: unverifiedLive ?? [],
    priorFailures: priorFailures ?? [],
    commands: commands ?? [],
  };
  return redactDeep(report);
}

export function writeReports(outDir, report) {
  mkdirSync(outDir, { recursive: true });
  const jsonPath = path.join(outDir, "report.json");
  const htmlPath = path.join(outDir, "report.html");
  const safeReport = redactDeep(report);
  writeFileSync(jsonPath, `${JSON.stringify(safeReport, null, 2)}\n`, "utf8");
  writeFileSync(htmlPath, renderHtml(safeReport), "utf8");
  return { jsonPath, htmlPath };
}

export function renderHtml(report) {
  report = redactDeep(report);
  const rows = (report.cases ?? [])
    .map((item) => {
      const evidence = (item.evidencePaths ?? item.evidence ?? [])
        .map((entry) => escapeHtml(typeof entry === "string" ? entry : entry.path))
        .join("<br>");
      return `<tr class="${escapeHtml(item.verdict)}">
        <td>${escapeHtml(item.id)}</td>
        <td>${escapeHtml(item.domain)}</td>
        <td>${escapeHtml(item.lane)}</td>
        <td>${escapeHtml(item.verdict)}</td>
        <td>${escapeHtml(item.reason ?? "")}</td>
        <td>${escapeHtml(redactText(item.detail ?? ""))}</td>
        <td>${evidence}</td>
      </tr>`;
    })
    .join("\n");
  const defects = (report.defects ?? [])
    .map(
      (item) =>
        `<li><strong>${escapeHtml(item.id)}</strong> — ${escapeHtml(item.repro ?? item.detail ?? "")}</li>`,
    )
    .join("");
  const unverified = (report.unverifiedLive ?? [])
    .map((item) => `<li>${escapeHtml(typeof item === "string" ? item : item.id)}</li>`)
    .join("");
  const prior = (report.priorFailures ?? [])
    .map(
      (item) =>
        `<li>${escapeHtml(item.id)} (${escapeHtml(item.recordedAt ?? "")}): ${escapeHtml(item.reason ?? "")}</li>`,
    )
    .join("");
  return `<!DOCTYPE html>
<html lang="zh-Hant">
<head>
  <meta charset="utf-8"/>
  <title>Vellum QA ${escapeHtml(report.lane)} ${escapeHtml(report.matrixVersion)}</title>
  <style>
    body { font-family: ui-sans-serif, system-ui, sans-serif; margin: 24px; color: #1b1b1b; }
    table { border-collapse: collapse; width: 100%; font-size: 13px; }
    th, td { border: 1px solid #ddd; padding: 6px 8px; vertical-align: top; }
    th { background: #f4f1ea; text-align: left; }
    .PASS { background: #e7f6e7; }
    .FAIL { background: #fde8e8; }
    .BLOCKED { background: #fff4d6; }
    .NOT_RUN { background: #f0f0f0; }
    .banner { padding: 12px 16px; border-radius: 8px; margin-bottom: 16px; }
    .banner.no { background: #fde8e8; }
    .banner.ok { background: #e7f6e7; }
    code { font-size: 12px; }
  </style>
</head>
<body>
  <h1>Vellum QA report</h1>
  <div class="banner ${report.fullAcceptance ? "ok" : "no"}">
    ${report.fullAcceptance ? "完整驗收通過" : "未宣稱完整驗收通過"}
    — lane ${escapeHtml(report.lane)} — ${escapeHtml(report.verdict)}
  </div>
  <p>commit <code>${escapeHtml(report.git?.commit)}</code> · branch <code>${escapeHtml(report.git?.branch)}</code></p>
  <p>matrix ${escapeHtml(report.matrixVersion)} · generated ${escapeHtml(report.generatedAt)}</p>
  <p>macOS 不列入本輪通過聲明 · Official live = ${escapeHtml(report.officialLive)}</p>
  <p>PASS ${report.counts?.PASS ?? 0} · FAIL ${report.counts?.FAIL ?? 0} · BLOCKED ${report.counts?.BLOCKED ?? 0} · NOT_RUN ${report.counts?.NOT_RUN ?? 0}</p>
  <h2>Cases</h2>
  <table>
    <thead><tr><th>id</th><th>domain</th><th>lane</th><th>verdict</th><th>reason</th><th>detail</th><th>evidence</th></tr></thead>
    <tbody>${rows}</tbody>
  </table>
  <h2>Product defects (repro only; not marked fixed)</h2>
  <ul>${defects || "<li>none recorded</li>"}</ul>
  <h2>Live traits 未驗證</h2>
  <ul>${unverified || "<li>none listed</li>"}</ul>
  <h2>Original FAIL records retained</h2>
  <ul>${prior || "<li>none</li>"}</ul>
</body>
</html>
`;
}
