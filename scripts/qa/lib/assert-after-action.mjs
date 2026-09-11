import { readFileSync } from "node:fs";

const pass = (detail = "post-action state verified") => ({
  ok: true,
  verdict: "PASS",
  reason: null,
  detail,
});

function hasOwn(object, field) {
  return object != null && Object.prototype.hasOwnProperty.call(object, field);
}

/** Shared post-action assertion. Guest and host both import this module. */
export function assertionAfterAction({ before, after, verify }) {
  if (!verify || !verify.kind) {
    return {
      ok: false,
      verdict: "FAIL",
      reason: "outcome-not-verified",
      detail: "driver has no verify spec; a successful click is not a pass",
    };
  }
  if (verify.kind === "process-running") {
    if (!Array.isArray(before?.processes) || !Array.isArray(after?.processes)) {
      return { ok: false, verdict: "FAIL", reason: "outcome-not-verified", detail: "process snapshot is missing" };
    }
    const beforeNames = new Set(before.processes.map((p) => String(p).toLowerCase()));
    const names = new Set(after.processes.map((p) => String(p).toLowerCase()));
    const want = String(verify.name).toLowerCase();
    if (beforeNames.has(want)) {
      return {
        ok: false,
        verdict: "FAIL",
        reason: "state-unchanged",
        detail: `process ${verify.name} was already running before the action`,
      };
    }
    if (!names.has(want)) {
      return {
        ok: false,
        verdict: "FAIL",
        reason: "state-unchanged",
        detail: `expected process ${verify.name} after action; observed ${[...names].join(",") || "(none)"}`,
      };
    }
    return pass(`process ${verify.name} started`);
  }
  if (verify.kind === "process-stopped") {
    if (!Array.isArray(before?.processes) || !Array.isArray(after?.processes)) {
      return { ok: false, verdict: "FAIL", reason: "outcome-not-verified", detail: "process snapshot is missing" };
    }
    const beforeNames = new Set(before.processes.map((p) => String(p).toLowerCase()));
    const names = new Set(after.processes.map((p) => String(p).toLowerCase()));
    const want = String(verify.name).toLowerCase();
    if (!beforeNames.has(want)) {
      return {
        ok: false,
        verdict: "FAIL",
        reason: "outcome-not-verified",
        detail: `process ${verify.name} was not observed before the stop action`,
      };
    }
    if (names.has(want)) {
      return {
        ok: false,
        verdict: "FAIL",
        reason: "state-unchanged",
        detail: `process ${verify.name} still running after stop`,
      };
    }
    return pass(`process ${verify.name} stopped`);
  }
  if (verify.kind === "field-changed") {
    if (
      !hasOwn(before, verify.field) ||
      !hasOwn(after, verify.field) ||
      before[verify.field] == null ||
      after[verify.field] == null
    ) {
      return {
        ok: false,
        verdict: "FAIL",
        reason: "outcome-not-verified",
        detail: `cannot compare ${verify.field}; before/after observation is missing`,
      };
    }
    if (before[verify.field] === after[verify.field]) {
      return {
        ok: false,
        verdict: "FAIL",
        reason: "state-unchanged",
        detail: `button succeeded but ${verify.field} did not change`,
      };
    }
    return pass(`${verify.field} changed`);
  }
  if (verify.kind === "attestation-pin") {
    const digest = after?.attestation?.digest || after?.digest;
    const pin = verify.digest;
    const normalize = (value) => String(value ?? "").trim().toLowerCase().replace(/^sha256:/, "");
    if (!digest || !pin || normalize(digest) !== normalize(pin)) {
      return {
        ok: false,
        verdict: "FAIL",
        reason: "attestation-mismatch",
        detail: `runtime digest ${digest ?? "(missing)"} does not match pin`,
      };
    }
    return pass("runtime attestation matches the full pinned digest");
  }
  if (verify.kind === "usage-source") {
    if (after?.usageSource !== verify.source) {
      return {
        ok: false,
        verdict: "FAIL",
        reason: "usage-source-mismatch",
        detail: `expected usage source ${verify.source}, got ${after?.usageSource}`,
      };
    }
    if (!after?.unit) {
      return {
        ok: false,
        verdict: "FAIL",
        reason: "usage-unit-missing",
        detail: "usage value is missing its unit",
      };
    }
    const usage = after?.usage;
    const total =
      typeof usage === "number"
        ? usage
        : Number(usage?.total ?? 0) || Number(usage?.input ?? 0) + Number(usage?.output ?? 0);
    if (!Number.isFinite(total) || total <= 0) {
      return {
        ok: false,
        verdict: "FAIL",
        reason: "outcome-not-verified",
        detail: "provider usage is missing or non-positive",
      };
    }
    return pass(`usage came from ${verify.source} with unit ${after.unit}`);
  }
  if (verify.kind === "file-diff") {
    if (!after?.fileChanged) {
      return {
        ok: false,
        verdict: "FAIL",
        reason: "no-file-change",
        detail: "model claimed success but workspace file did not change",
      };
    }
    return pass("workspace file changed");
  }
  if (verify.kind === "ui-matches-backend") {
    const sources = after?.backendSources ?? verify.sources ?? [];
    if (after?.uiValue == null || after?.backendValue == null || after.uiValue !== after.backendValue) {
      return {
        ok: false,
        verdict: "FAIL",
        reason: "outcome-not-verified",
        detail: `UI ${after?.uiValue ?? "(missing)"} does not match backend ${after?.backendValue ?? "(missing)"}; sources=${(sources ?? []).join(",")}`,
      };
    }
    if (!Array.isArray(sources) || sources.length === 0) {
      return { ok: false, verdict: "FAIL", reason: "outcome-not-verified", detail: "backend source is missing" };
    }
    if (after.unit == null && after.usage != null) {
      return { ok: false, verdict: "FAIL", reason: "usage-unit-missing", detail: "usage value is missing its unit" };
    }
    return pass(`UI matches backend source ${sources.join(",")}`);
  }
  if (verify.kind === "host-state") {
    if (!after?.clicked) {
      return {
        ok: false,
        verdict: after?.vellumRunning ? "FAIL" : "BLOCKED",
        reason: after?.vellumRunning ? "control-not-driven" : "launcher-unavailable",
        detail: after?.detail || "Remote Manager control was not driven",
      };
    }
    if (!hasOwn(before, "hostState") || !hasOwn(after, "hostState") || !before.hostState || !after.hostState) {
      return {
        ok: false,
        verdict: "FAIL",
        reason: "outcome-not-verified",
        detail: "remote hostState before/after observation is missing",
      };
    }
    if (after.hostState === before.hostState) {
      return {
        ok: false,
        verdict: "FAIL",
        reason: "state-unchanged",
        detail: `button succeeded but hostState did not change; readyz=${after?.readyz ?? "missing"} daemonOwner=${after?.daemonOwner ?? "missing"}`,
      };
    }
    if (verify.requireReadyz && after?.readyz !== true) {
      return {
        ok: false,
        verdict: "FAIL",
        reason: "outcome-not-verified",
        detail: "authenticated readyz is not true after the UI action",
      };
    }
    if (verify.requireDaemonOwner && after?.daemonOwner !== verify.requireDaemonOwner) {
      return {
        ok: false,
        verdict: "FAIL",
        reason: "outcome-not-verified",
        detail: `native daemon owner ${after?.daemonOwner ?? "(missing)"} != ${verify.requireDaemonOwner}`,
      };
    }
    return pass(`remote host state changed from ${before.hostState} to ${after.hostState}`);
  }
  if (verify.kind === "manual-checkpoint") {
    return {
      ok: false,
      verdict: "BLOCKED",
      reason: "manual-checkpoint",
      detail: "interactive login/OS checkpoint; inject is not a PASS",
    };
  }
  return {
    ok: false,
    verdict: "FAIL",
    reason: "outcome-not-verified",
    detail: `unknown verify.kind ${verify.kind}`,
  };
}

if (String(process.argv[1] ?? "").replace(/\\/g, "/").endsWith("assert-after-action.mjs")) {
  const before = JSON.parse(readFileSync(process.argv[2], "utf8"));
  const after = JSON.parse(readFileSync(process.argv[3], "utf8"));
  const verify = JSON.parse(readFileSync(process.argv[4], "utf8"));
  process.stdout.write(`${JSON.stringify(assertionAfterAction({ before, after, verify }))}\n`);
}
