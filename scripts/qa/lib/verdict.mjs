export const VERDICTS = Object.freeze(["PASS", "FAIL", "BLOCKED", "NOT_RUN"]);

export function isVerdict(value) {
  return VERDICTS.includes(value);
}

export function assertVerdict(value, context) {
  if (!isVerdict(value)) {
    throw new Error(`invalid verdict ${JSON.stringify(value)} (${context})`);
  }
  return value;
}

/** Necessary cases must all be PASS for a full-acceptance claim. */
export function fullAcceptance(cases) {
  const necessary = cases.filter((item) => item.necessary !== false);
  if (!necessary.length) return false;
  return necessary.every((item) => item.verdict === "PASS");
}

export function summarize(cases) {
  const counts = { PASS: 0, FAIL: 0, BLOCKED: 0, NOT_RUN: 0 };
  for (const item of cases) {
    if (counts[item.verdict] !== undefined) counts[item.verdict] += 1;
  }
  return counts;
}

export function classifyResult(item) {
  if (item.verdict === "PASS") return "ok";
  if (item.verdict === "BLOCKED") return "environment";
  if (item.verdict === "NOT_RUN") {
    if (item.reason === "budget-stop" || item.reason === "policy") return "policy";
    return "policy";
  }
  if (item.reason === "control-not-driven" || item.reason === "case-driver-unavailable" || item.reason === "missing-evidence") {
    return "harness";
  }
  return "product";
}

export function laneExitCode(cases, { required = true } = {}) {
  if (!required) return 0;
  if (cases.some((item) => item.verdict === "FAIL")) return 1;
  if (cases.some((item) => item.necessary !== false && item.verdict === "BLOCKED")) return 1;
  if (
    cases.some(
      (item) =>
        item.necessary !== false &&
        item.verdict === "NOT_RUN" &&
        item.reason !== "dry-run",
    )
  ) {
    return 1;
  }
  return 0;
}

/** Full acceptance is against the whole necessary matrix, not just cases present in a partial report. */
export function fullAcceptanceAgainstMatrix(resultCases, matrixCases) {
  const byId = new Map(resultCases.map((item) => [item.id, item]));
  const necessary = matrixCases.filter((item) => item.necessary !== false);
  if (!necessary.length) return false;
  for (const item of necessary) {
    const got = byId.get(item.id);
    if (!got || got.verdict !== "PASS") return false;
  }
  return true;
}
