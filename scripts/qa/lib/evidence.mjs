import { existsSync, readFileSync, statSync } from "node:fs";
import path from "node:path";

const PNG_MAGIC = Buffer.from([0x89, 0x50, 0x4e, 0x47]);

export function evidencePath(dir, relative) {
  return path.join(dir, relative);
}

export function describeEvidenceProblem(full, relative) {
  if (!existsSync(full)) return "missing";
  let stat;
  try {
    stat = statSync(full);
  } catch {
    return "unreadable";
  }
  if (stat.size <= 0) return "empty";
  const lower = relative.replace(/\\/g, "/").toLowerCase();
  if (lower.endsWith(".png")) {
    const buf = readFileSync(full);
    const head = buf.subarray(0, 4);
    if (!head.equals(PNG_MAGIC)) return "not a PNG";
    if (buf.length < 32) return "png-header-only";
  }
  if (lower.endsWith(".json")) {
    try {
      JSON.parse(readFileSync(full, "utf8"));
    } catch {
      return "not JSON";
    }
  }
  return null;
}

export function jsonSemanticProblem(full, requiredKeys = []) {
  try {
    const value = JSON.parse(readFileSync(full, "utf8"));
    if (typeof value !== "object" || value == null) return "json-not-object";
    for (const key of requiredKeys) {
      if (value[key] === undefined) return `json-missing:${key}`;
    }
    return null;
  } catch {
    return "not JSON";
  }
}

export function staleEvidenceProblem(observedAt, runStartedAt, maxAgeMs = 6 * 60 * 60 * 1000) {
  if (!observedAt) return "missing-observedAt";
  const observed = new Date(observedAt).getTime();
  const started = new Date(runStartedAt).getTime();
  if (!Number.isFinite(observed) || !Number.isFinite(started)) return "invalid-timestamp";
  if (observed < started - 1000) return "stale-before-run";
  if (Date.now() - observed > maxAgeMs) return "stale-max-age";
  return null;
}

export function missingEvidence(required, dir) {
  const missing = [];
  for (const item of required ?? []) {
    const full = evidencePath(dir, item);
    const problem = describeEvidenceProblem(full, item);
    if (problem) missing.push(problem === "missing" || problem === "empty" ? item : `${item} (${problem})`);
  }
  return missing;
}

/** A PASS with missing or wrong-typed required evidence is a harness/app defect. */
export function applyEvidenceGate(verdict, required, dir) {
  const missing = missingEvidence(required, dir);
  if (verdict === "PASS" && missing.length) {
    return {
      verdict: "FAIL",
      reason: "missing-evidence",
      missingEvidence: missing,
    };
  }
  return { verdict, reason: null, missingEvidence: missing };
}
