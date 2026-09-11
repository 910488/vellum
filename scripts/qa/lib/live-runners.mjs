import { readFileSync } from "node:fs";
import { assertionAfterAction } from "./assert-after-action.mjs";

/** One runner id per live-session family. A blob from runner A cannot score case B. */
export const LIVE_RUNNERS = Object.freeze({
  "enhanced.core.read-file": "enhanced-read",
  "enhanced.core.edit-function": "enhanced-edit",
  "enhanced.core.run-tests": "enhanced-tests",
  "enhanced.core.continue": "enhanced-continue",
  "enhanced.core.cancel": "enhanced-cancel",
  "enhanced.core.attestation-pin": "enhanced-attest",
  "enhanced.trait.tool-repeat.live": "trait-tool-repeat",
  "enhanced.trait.context-recovery.live": "trait-context-recovery",
  "enhanced.trait.continuation.live": "trait-continuation",
  "enhanced.trait.cancel-priority.live": "trait-cancel-priority",
  "subagent.spawn-two": "subagent-spawn",
  "subagent.message": "subagent-message",
  "subagent.wait": "subagent-wait",
  "subagent.resume": "subagent-resume",
  "subagent.close": "subagent-close",
  "subagent.complete": "subagent-complete",
  "subagent.parent-cancel": "subagent-cancel",
  "subagent.usage-accounting": "subagent-usage",
  "guardian.known-defect": "guardian-defect",
  "guardian.clean-diff": "guardian-clean",
  "guardian.findings-identity": "guardian-identity",
  "remote.session.tool-task": "remote-tool-task",
  "remote.session.resume": "remote-resume",
});

export function liveRunnerFor(caseId) {
  const base = String(caseId ?? "").includes("::") ? String(caseId).split("::")[0] : String(caseId ?? "");
  return LIVE_RUNNERS[caseId] ?? LIVE_RUNNERS[base] ?? null;
}

export function scoreLiveCase(caseId, evidence, verify) {
  const runner = liveRunnerFor(caseId);
  if (!runner) {
    return {
      ok: false,
      verdict: "FAIL",
      reason: "outcome-not-verified",
      detail: `no live runner dispatched for ${caseId}`,
    };
  }
  if (!evidence || evidence.runner !== runner) {
    return {
      ok: false,
      verdict: "FAIL",
      reason: "outcome-not-verified",
      detail: `refusing to score ${caseId} (${runner}) from ${evidence?.runner ?? "missing"} evidence`,
    };
  }
  if (
    evidence.caseId !== caseId ||
    evidence.observationSource !== "vellum-enhanced-session-events" ||
    evidence.completed !== true ||
    evidence.execCode !== 0 ||
    !evidence.sessionId
  ) {
    return {
      ok: false,
      verdict: "FAIL",
      reason: "outcome-not-verified",
      detail: "live evidence must identify this exact case and a completed Enhanced session event stream",
    };
  }
  const after = {
    fileChanged: Boolean(evidence.fileChanged),
    sessionId: evidence.sessionId ?? null,
    usageSource: evidence.usageSource ?? null,
    unit: evidence.unit ?? null,
    usage: evidence.usage ?? null,
    digest: evidence.digest ?? null,
    attestation: evidence.digest ? { digest: evidence.digest } : null,
    children: evidence.children ?? null,
    findings: evidence.findings ?? null,
  };
  const spec = verify ?? { kind: "file-diff" };
  const result = assertionAfterAction({ before: { fileChanged: false }, after, verify: spec });
  if (runner === "subagent-spawn") {
    const kids = evidence.children;
    if (!Array.isArray(kids) || kids.length < 2 || new Set(kids.map(String)).size < 2) {
      return {
        ok: false,
        verdict: "FAIL",
        reason: "outcome-not-verified",
        detail: "subagent.spawn-two requires two distinct child identities",
      };
    }
  }
  if (
    runner === "guardian-defect" &&
    (!evidence.formalReview ||
      !Array.isArray(evidence.findings) ||
      !evidence.findings.some((finding) => finding && typeof finding === "object" && finding.file && finding.message))
  ) {
    return {
      ok: false,
      verdict: "FAIL",
      reason: "outcome-not-verified",
      detail: "guardian.known-defect requires findings from a formal review run",
    };
  }
  return result;
}

if (String(process.argv[1] ?? "").replace(/\\/g, "/").endsWith("live-runners.mjs")) {
  const cmd = process.argv[2];
  if (cmd === "for") {
    process.stdout.write(`${liveRunnerFor(process.argv[3]) ?? ""}\n`);
  } else if (cmd === "score") {
    const caseId = process.argv[3];
    const evidence = JSON.parse(readFileSync(process.argv[4], "utf8"));
    const verify = process.argv[5] ? JSON.parse(readFileSync(process.argv[5], "utf8")) : { kind: "file-diff" };
    process.stdout.write(`${JSON.stringify(scoreLiveCase(caseId, evidence, verify))}\n`);
  } else {
    process.stderr.write("usage: live-runners.mjs for <caseId> | score <caseId> <evidence.json> [verify.json]\n");
    process.exit(2);
  }
}
