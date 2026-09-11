import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { applyEvidenceGate, jsonSemanticProblem, staleEvidenceProblem } from "./evidence.mjs";
import { runCleanup } from "./cleanup.mjs";
import { withTimeout } from "./timeout.mjs";
import { redactText } from "./sanitize.mjs";
import { CASE_TIMEOUT_MS } from "./budget.mjs";
import { classifyResult } from "./verdict.mjs";

export function caseEvidenceDir(outDir, id) {
  return path.join(outDir, "evidence", id.replace(/[^\w.-]+/g, "_"));
}

export async function executePreparedCase(ctx, spec) {
  const evidenceDir = spec.evidenceDir ?? caseEvidenceDir(ctx.outDir, spec.id);
  mkdirSync(evidenceDir, { recursive: true });
  const started = Date.now();
  let verdict = "PASS";
  let reason = null;
  let detail = "";
  let command = spec.commandLog ?? null;
  const requiredEvidence = spec.evidence ?? [];

  const fail = (nextVerdict, nextReason, nextDetail) => {
    verdict = nextVerdict;
    reason = nextReason;
    detail = nextDetail;
  };

  let reservation = null;
  try {
    if (spec.skipVerdict) {
      fail(spec.skipVerdict, spec.skipReason ?? null, spec.skipDetail ?? "");
    } else if (spec.requiresCredentials?.length) {
      const missing = spec.requiresCredentials.filter((key) => !ctx.env[key]);
      if (missing.length) {
        fail("BLOCKED", "missing-credentials", `missing ${missing.join(", ")}`);
      }
    }

    if (verdict === "PASS" && spec.model) {
      const estimate = spec.estimate ?? 8_192;
      if (typeof ctx.budget.reserve === "function") {
        if (!ctx.budget.reserve(spec.model, estimate)) {
          fail(
            "NOT_RUN",
            "budget-stop",
            `model ${spec.model} stopped before start; remaining=${ctx.budget.remaining(spec.model)} estimate=${JSON.stringify(estimate)}`,
          );
        } else {
          reservation = { model: spec.model, estimate };
        }
      } else if (!ctx.budget.canStart(spec.model, estimate)) {
        fail(
          "NOT_RUN",
          "budget-stop",
          `model ${spec.model} stopped before start; remaining=${ctx.budget.remaining(spec.model)} estimate=${JSON.stringify(estimate)}`,
        );
      }
    }

    if (verdict === "PASS" && spec.run) {
      const timeoutMs = spec.timeoutMs ?? ctx.caseTimeoutMs ?? CASE_TIMEOUT_MS;
      const outcome = await withTimeout(
        timeoutMs,
        () => spec.run({ evidenceDir, env: ctx.env, ctx }),
        { label: spec.id },
      );
      if (outcome?.command) command = outcome.command;
      if (outcome?.detail) detail = String(outcome.detail);
      if (reservation) {
        ctx.budget.settle(reservation.model, outcome?.usage ?? { missing: true }, reservation.estimate);
        reservation = null;
      } else if (outcome?.usage && spec.model) {
        ctx.budget.record(spec.model, outcome.usage);
      }
      if (outcome?.verdict && outcome.verdict !== "PASS") {
        fail(outcome.verdict, outcome.reason ?? "error", outcome.detail ?? detail);
      }
    }
  } catch (cause) {
    if (cause?.code === "TIMEOUT") {
      fail("FAIL", "timeout", cause.message);
    } else {
      fail("FAIL", "error", cause?.message ?? String(cause));
    }
  } finally {
    if (reservation && typeof ctx.budget.release === "function") {
      ctx.budget.release(reservation.model, reservation.estimate);
    }
  }

  try {
    await runCleanup(spec.cleanupHooks ?? []);
  } catch (cause) {
    fail("FAIL", "cleanup-failure", cause.message);
  }

  if (verdict === "PASS") {
    const gate = applyEvidenceGate(verdict, requiredEvidence, evidenceDir);
    if (gate.verdict !== "PASS") {
      fail(gate.verdict, gate.reason, `missing evidence: ${gate.missingEvidence.join(", ")}`);
    }
  }
  if (verdict === "PASS" && spec.jsonKeys?.length) {
    const relative = spec.jsonFile ?? requiredEvidence.find((item) => item.endsWith(".json"));
    if (relative) {
      const problem = jsonSemanticProblem(path.join(evidenceDir, relative), spec.jsonKeys);
      if (problem) fail("FAIL", "missing-evidence", `json semantic: ${problem}`);
    }
  }
  if (verdict === "PASS" && spec.runStartedAt) {
    const stale = staleEvidenceProblem(new Date().toISOString(), spec.runStartedAt);
    if (stale === "stale-before-run") fail("FAIL", "stale-evidence", stale);
  }

  const record = {
    id: spec.id,
    domain: spec.domain ?? "runner",
    surface: spec.surface ?? "runner",
    lane: spec.lane ?? ctx.lane,
    necessary: spec.necessary !== false,
    model: spec.model ?? null,
    runId: ctx.runId ?? null,
    observedAt: new Date().toISOString(),
    classification: classifyResult({ verdict, reason, domain: spec.domain ?? "runner" }),
    verdict,
    reason,
    detail: redactText(detail),
    durationMs: Date.now() - started,
    command,
    evidence: requiredEvidence,
    evidenceDir,
    evidencePaths: requiredEvidence.map((item) => path.join(evidenceDir, item)),
  };
  writeFileSync(path.join(evidenceDir, "result.json"), `${JSON.stringify(record, null, 2)}\n`);
  return record;
}

export function writeLog(evidenceDir, relative, text) {
  const full = path.join(evidenceDir, relative);
  mkdirSync(path.dirname(full), { recursive: true });
  writeFileSync(full, text ?? "", "utf8");
  return full;
}
