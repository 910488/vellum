import { describe, expect, it } from "vitest";
import {
  candidateLiveOutcome,
  desktopDrivenOutcome,
  remoteProofPassed,
} from "../scripts/qa/lib/orchestrator.mjs";
import { probeLiveCredentials } from "../scripts/qa/lib/probes.mjs";
import { buildReport, renderHtml } from "../scripts/qa/lib/report.mjs";
import { redactDeep, redactText } from "../scripts/qa/lib/sanitize.mjs";

describe("QA verdict honesty", () => {
  it("does not treat a successful UI Automation invoke as an outcome PASS", () => {
    expect(desktopDrivenOutcome("啟動 Proxy")).toMatchObject({
      verdict: "BLOCKED",
      reason: "outcome-not-verified",
    });
  });

  it("does not treat the candidate provider gate as proof of unrelated live cases", () => {
    expect(candidateLiveOutcome("subagent.spawn-two", "qwen")).toMatchObject({
      verdict: "NOT_RUN",
      reason: "case-driver-unavailable",
    });
  });

  it("requires the named remote test to be explicitly reported as passed", () => {
    const log = [
      "test bootstrap_converges_a_pristine_dev_host ... diagnostic output",
      "ok",
      "test result: ok. 1 passed; 0 failed",
    ].join("\n");
    expect(remoteProofPassed(log, "bootstrap_converges_a_pristine_dev_host")).toBe(true);
    expect(remoteProofPassed(log, "remote_deployment_converges_and_a_second_apply_changes_nothing")).toBe(false);
  });

  it("allows the isolated live launcher to validate credentials stored by Desktop", () => {
    expect(probeLiveCredentials({ VELLUM_QA_LIVE: "1" })).toMatchObject({
      ok: true,
      reason: "enabled",
      present: [],
    });
  });

  it("removes operator home directories from text and nested evidence paths", () => {
    expect(redactText("C:\\Users\\alice\\repo\\report.json")).toBe(
      "<USER_HOME>\\repo\\report.json",
    );
    expect(redactText("/home/alice/repo/report.json")).toBe(
      "<USER_HOME>/repo/report.json",
    );
    expect(
      redactDeep({ evidencePaths: ["C:/Users/alice/repo/report.json"] }),
    ).toEqual({ evidencePaths: ["<USER_HOME>/repo/report.json"] });
  });

  it("de-identifies every report section and direct HTML rendering", () => {
    const report = buildReport({
      lane: "offline",
      matrixVersion: "test",
      git: { commit: "abc", branch: "codex/test" },
      artifacts: { root: "C:\\Users\\alice\\artifacts" },
      cases: [],
      commands: ["cat /home/alice/private/run.log"],
      environment: { workspace: "C:/Users/alice/repo" },
    });
    const serialized = JSON.stringify(report);
    expect(serialized).not.toContain("alice");
    expect(serialized).toContain("<USER_HOME>");

    const html = renderHtml({
      ...report,
      defects: [{ id: "D1", detail: "C:\\Users\\alice\\secret.txt" }],
    });
    expect(html).not.toContain("alice");
    expect(html).toContain("&lt;USER_HOME&gt;");
  });
});
