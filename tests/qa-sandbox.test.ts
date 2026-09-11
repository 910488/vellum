import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createHash } from "node:crypto";
import { describe, expect, it } from "vitest";
import { resolveInstaller, refuseNewestMtimePicker } from "../scripts/qa/lib/installer.mjs";
import { expandModels, assertPinnedModel } from "../scripts/qa/lib/models.mjs";
import {
  sealCredentials,
  openCredentials,
  destroyCredentialChannel,
  resolveHostSecrets,
} from "../scripts/qa/lib/credential-channel.mjs";
import { acquireRunLock, releaseRunLock, renderWsb, forbiddenMapping } from "../scripts/qa/lib/wsb.mjs";
import { destroyOnceChannel, remoteCaseVerdict } from "../scripts/qa/lib/sandbox-lane.mjs";
import { assertionAfterAction } from "../scripts/qa/lib/assert-after-action.mjs";
import { buildAfterFromDump } from "../scripts/qa/lib/build-after-from-dump.mjs";
import { liveRunnerFor, scoreLiveCase } from "../scripts/qa/lib/live-runners.mjs";
import {
  refuseSecondSandboxInstance,
  sandboxProcessesIndicateAlive,
  waitForGuestFile,
  parseGuestJson,
  SINGLE_INSTANCE_MESSAGE,
} from "../scripts/qa/lib/sandbox-process.mjs";
import { createBudgetTracker } from "../scripts/qa/lib/budget.mjs";
import { resolveActionControl, assertionAfterAction } from "../scripts/qa/lib/drivers.mjs";
import { driverFor, DRIVERS } from "../qa/matrix/drivers.mjs";
import { CASES, TAB_CONTROL } from "../qa/matrix/v1.mjs";
import { classifyResult } from "../scripts/qa/lib/verdict.mjs";
import { jsonSemanticProblem, describeEvidenceProblem } from "../scripts/qa/lib/evidence.mjs";
import { laneExitCode, fullAcceptanceAgainstMatrix } from "../scripts/qa/lib/verdict.mjs";
import { rejectMergeProblems } from "../scripts/qa/lib/report.mjs";

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const runner = path.join(repo, "scripts", "qa", "run.mjs");

function tmp() {
  return mkdtempSync(path.join(tmpdir(), "vellum-qa-sb-"));
}

function runQa(args: string[], env: NodeJS.ProcessEnv = process.env) {
  return spawnSync(process.execPath, [runner, ...args], { cwd: repo, encoding: "utf8", env });
}

function fakeInstaller() {
  const dir = tmp();
  const installer = path.join(dir, "Vellum_0.0.0_x64-setup.exe");
  const bytes = Buffer.from("fake-nsis-bytes");
  writeFileSync(installer, bytes);
  const sha = createHash("sha256").update(bytes).digest("hex");
  const buildInfo = path.join(dir, "build-info.json");
  writeFileSync(
    buildInfo,
    JSON.stringify({
      commit: "abc",
      expectedCommit: "abc",
      releaseKind: "development",
      mainRelease: false,
      clean: false,
      installer,
      installerSha256: sha,
      version: "0.0.0",
    }),
  );
  return { dir, installer, sha, buildInfo };
}

describe("installer identity", () => {
  it("resolves the unique installer from build-info SHA, not mtime", () => {
    const fake = fakeInstaller();
    const resolved = resolveInstaller(fake.buildInfo);
    expect(resolved.installerSha256).toBe(fake.sha);
    expect(resolved.development).toBe(true);
    expect(resolved.developmentLabel).toMatch(/development/);
    expect(refuseNewestMtimePicker().allowed).toBe(false);
  });

  it("rejects commit vs expectedCommit mismatch", () => {
    const fake = fakeInstaller();
    const info = JSON.parse(readFileSync(fake.buildInfo, "utf8"));
    info.expectedCommit = "dddddddddddddddddddddddddddddddddddddddd";
    writeFileSync(fake.buildInfo, JSON.stringify(info));
    expect(() => resolveInstaller(fake.buildInfo)).toThrow(/expectedCommit/);
  });

  it("rejects a SHA mismatch", () => {
    const fake = fakeInstaller();
    const info = JSON.parse(readFileSync(fake.buildInfo, "utf8"));
    info.installerSha256 = "0".repeat(64);
    writeFileSync(fake.buildInfo, JSON.stringify(info));
    expect(() => resolveInstaller(fake.buildInfo)).toThrow(/SHA mismatch/);
  });
});

describe("sandbox single-instance gate", () => {
  it("refuses to launch when vmmemWindowsSandbox or RemoteSession is already alive", () => {
    expect(sandboxProcessesIndicateAlive(["vmmemWindowsSandbox"])).toBe(true);
    expect(sandboxProcessesIndicateAlive(["WindowsSandboxRemoteSession", "WindowsSandboxServer"])).toBe(true);
    expect(sandboxProcessesIndicateAlive(["notepad"])).toBe(false);
    const gate = refuseSecondSandboxInstance(["vmmemWindowsSandbox"]);
    expect(gate.allowed).toBe(false);
    expect(gate.detail).toBe(SINGLE_INSTANCE_MESSAGE);
  });

  it("parses PowerShell UTF-8 BOM guest JSON", () => {
    expect(parseGuestJson("\uFEFF{\"ok\":false,\"error\":\"blocked\"}")).toEqual({ ok: false, error: "blocked" });
  });

  it("waitForGuestFile times out without synthesizing HEARTBEAT", async () => {
    const result = await waitForGuestFile("C:\\missing-heartbeat", {
      timeoutMs: 20,
      pollMs: 5,
      existsSync: () => false,
      readFileSync: () => {
        throw new Error("should not read");
      },
    });
    expect(result.found).toBe(false);
  });
});

describe("budget reserve then settle", () => {
  it("reserves before dispatch and settles missing usage conservatively", () => {
    const budget = createBudgetTracker();
    expect(budget.reserve("opencode-mimo-2.5", 8_192)).toBe(true);
    expect(budget.used()["opencode-mimo-2.5"]).toBe(8_192);
    budget.settle("opencode-mimo-2.5", { missing: true }, 8_192);
    expect(budget.used()["opencode-mimo-2.5"]).toBe(8_192);
    expect(budget.reserve("opencode-mimo-2.5", 200_000)).toBe(false);
  });
});

describe("wsb generation", () => {
  it("maps kit/installer/output only and never host .codex or docker", () => {
    const xml = renderWsb({
      kitHost: "D:\\qa\\kit",
      installerHost: "D:\\qa\\installer",
      outputHost: "D:\\qa\\output",
      secretsHost: "D:\\qa\\secrets",
      logonCommand: "powershell.exe -File C:\\SandboxKit\\guest-bootstrap.ps1",
    });
    expect(xml).toContain("D:\\qa\\kit");
    expect(xml).not.toContain("Users\\alice");
    expect(forbiddenMapping(xml)).toBeNull();
    expect(forbiddenMapping(`${xml}<HostFolder>C:\\Users\\x\\.codex</HostFolder>`)).toBe("host-codex-home");
    const withOnce = renderWsb({
      kitHost: "D:\\qa\\kit",
      installerHost: "D:\\qa\\installer",
      outputHost: "D:\\qa\\output",
      secretsHost: "D:\\qa\\secrets",
      onceHost: "D:\\qa\\once",
      logonCommand: "cmd.exe /c C:\\SandboxKit\\guest-start.cmd",
    });
    expect(withOnce).toContain("D:\\qa\\once");
    expect(withOnce).toContain("C:\\QaOnce");
    expect(withOnce).not.toContain("once.key");
    expect(withOnce).not.toContain("D:\\qa\\output\\once");
  });

  it("destroys the wrap key outside ordinary output", () => {
    const dir = tmp();
    const onceHost = path.join(dir, "once");
    const outputHost = path.join(dir, "output");
    mkdirSync(onceHost, { recursive: true });
    mkdirSync(outputHost, { recursive: true });
    writeFileSync(path.join(onceHost, "once.key"), "secret-wrap-key");
    writeFileSync(path.join(outputHost, "once.key"), "leaked");
    destroyOnceChannel(onceHost, outputHost);
    expect(existsSync(path.join(onceHost, "once.key"))).toBe(false);
    expect(existsSync(path.join(outputHost, "once.key"))).toBe(false);
  });

  it("maps remote UI rows through shipped host-state assertions", () => {
    const blocked = remoteCaseVerdict(
      { id: "remote.deploy.clean-host" },
      { buttons: [] },
      { ok: true },
    );
    expect(blocked.verdict).toBe("BLOCKED");
    const row = remoteCaseVerdict(
      { id: "tab.remote.bootstrap-confirm" },
      {
        buttons: [
          {
            id: "tab.remote.bootstrap-confirm",
            verdict: "FAIL",
            reason: "state-unchanged",
            detail: "hostState did not change",
          },
        ],
      },
      { ok: true },
    );
    expect(row.verdict).toBe("FAIL");
    expect(row.reason).toBe("state-unchanged");
  });

  it("refuses to steal another run's lock", () => {
    const dir = tmp();
    const lock = path.join(dir, "sandbox.lock");
    acquireRunLock(lock, "run-a", 1);
    expect(() => acquireRunLock(lock, "run-b", 2)).toThrow(/another sandbox run/);
    expect(releaseRunLock(lock, "run-b")).toBe(false);
    expect(releaseRunLock(lock, "run-a")).toBe(true);
  });
});

describe("credential channel", () => {
  it("round-trips secrets and destroys the envelope", () => {
    const dir = tmp();
    const { key } = sealCredentials(dir, { opencode: "sk-test" });
    const opened = openCredentials(dir, key);
    expect(opened.opencode).toBe("sk-test");
    destroyCredentialChannel(dir);
    expect(() => openCredentials(dir, key)).toThrow();
    expect(existsSync(path.join(dir, "key.b64"))).toBe(false);
  });

  it("does not put secrets in resolver field names leaked to reports", () => {
    const secrets = resolveHostSecrets({ VELLUM_QA_OPENCODE_KEY: "sk-live" });
    expect(Object.keys(secrets)).toEqual(["opencode"]);
  });
});

describe("models + budget expansion", () => {
  it("expands models arrays onto pinned ids", () => {
    const expanded = expandModels([
      { id: "enhanced.core.read-file", models: ["opencode-mimo-2.5", "qwen"] },
    ]);
    expect(expanded.map((item) => item.model)).toEqual(["opencode-mimo-2.5", "qwen"]);
    expect(assertPinnedModel("some-mimo-2.5-chat", "opencode-mimo-2.5").ok).toBe(true);
    expect(assertPinnedModel("gpt-4o-mini", "opencode-mimo-2.5").ok).toBe(false);
  });
});

describe("drivers", () => {
  it("covers desktop/live/remote/sandbox cases with prepare/input/action/wait/verify/restore", () => {
    const needed = CASES.filter((item) => ["desktop", "live", "remote", "sandbox"].includes(item.lane));
    expect(needed.length).toBeGreaterThan(80);
    for (const item of needed) {
      const driver = driverFor(item.id);
      expect(driver, item.id).toBeTruthy();
      expect(driver?.prepare, item.id).toBeTruthy();
      expect(driver?.input, item.id).toBeTruthy();
      expect(driver?.action?.kind, item.id).toBeTruthy();
      expect(driver?.wait, item.id).toBeTruthy();
      expect(driver?.verify?.kind, item.id).toBeTruthy();
      expect(driver?.restore, item.id).toBeTruthy();
      const tab = TAB_CONTROL[item.surface];
      const isNavigate = /\.navigate$/.test(item.id) || driver?.action?.kind === "navigate";
      const surfaceIsTheControl = ["dialog", "tray", "onboarding"].includes(item.surface);
      if (!isNavigate && !surfaceIsTheControl && driver?.action?.kind === "invoke") {
        expect(driver?.action?.control, item.id).not.toBe(tab);
      }
    }
    expect(driverFor("enhanced.core.read-file")?.action.kind).toBe("live-session");
    expect(driverFor("subagent.spawn-two")?.action.kind).toBe("live-session");
    expect(driverFor("guardian.known-defect")?.action.kind).toBe("live-session");
    expect(driverFor("remote.deploy.clean-host")?.action.kind).toBe("remote-ui");
    expect(driverFor("tray.show")?.action.kind).toBe("manual-checkpoint");
    expect(Object.keys(DRIVERS).length).toBeGreaterThanOrEqual(needed.length);
  });

  it("rejects clicking the tab when the case is not a navigate", () => {
    expect(
      resolveActionControl({
        id: "tab.today.proxy-start",
        surface: "today",
        control: "現況",
      }),
    ).toBeNull();
    expect(
      resolveActionControl({
        id: "tab.today.proxy-start",
        surface: "today",
        driver: { action: { control: "啟動 Proxy" } },
      }),
    ).toBe("啟動 Proxy");
  });

  it("fails ui-matches-backend when UI and stored values differ", () => {
    const result = assertionAfterAction({
      before: {},
      after: { uiValue: "12%", backendValue: "40%", backendSources: ["quota-api"] },
      verify: { kind: "ui-matches-backend", sources: ["quota-api"] },
    });
    expect(result.ok).toBe(false);
    expect(result.reason).toBe("outcome-not-verified");
  });

  it("blocks remote host-state when Vellum did not click", () => {
    const result = assertionAfterAction({
      before: { hostState: "unreachable" },
      after: { clicked: false, vellumRunning: false, hostState: "unreachable" },
      verify: { kind: "host-state", requireReadyz: true, requireDaemonOwner: "codexCliDaemon" },
    });
    expect(result.ok).toBe(false);
    expect(result.verdict).toBe("BLOCKED");
    expect(result.reason).toBe("launcher-unavailable");
  });

  it("field-changed stays FAIL when dump left activeTab null", () => {
    const after = buildAfterFromDump({ clickedControl: "現況", dump: { processes: ["Vellum"] } });
    const result = assertionAfterAction({
      before: { activeTab: null },
      after,
      verify: { kind: "field-changed", field: "activeTab" },
    });
    expect(after.activeTab).toBeNull();
    expect(result.ok).toBe(false);
    expect(result.reason).toBe("outcome-not-verified");
  });

  it("drives shipped assert-after-action.mjs CLI", () => {
    const dir = tmp();
    writeFileSync(path.join(dir, "before.json"), JSON.stringify({ processes: [] }));
    writeFileSync(path.join(dir, "after.json"), JSON.stringify({ processes: ["vellum-proxy-desktop"] }));
    writeFileSync(path.join(dir, "verify.json"), JSON.stringify({ kind: "process-running", name: "vellum-proxy-desktop" }));
    const result = spawnSync(
      process.execPath,
      [
        path.join(repo, "scripts", "qa", "lib", "assert-after-action.mjs"),
        path.join(dir, "before.json"),
        path.join(dir, "after.json"),
        path.join(dir, "verify.json"),
      ],
      { encoding: "utf8" },
    );
    expect(result.status).toBe(0);
    expect(JSON.parse(result.stdout)).toMatchObject({ ok: true, verdict: "PASS" });
  });

  it("does not copy the click target into dump UI fields", () => {
    const dump = { processes: ["Vellum"], proxyRunning: false, activeTab: null, uiState: null, refreshedAt: null };
    const after = buildAfterFromDump({ clickedControl: "啟動 Proxy", dump });
    expect(after.activeTab).toBeNull();
    expect(after.uiState).toBeNull();
    expect(after.refreshedAt).toBeNull();
    expect(after.activeTab).not.toBe("啟動 Proxy");
  });

  it("keeps dump-selected tab even when it happens to equal the click name", () => {
    const dump = { activeTab: "現況", uiState: "現況", processes: ["Vellum"] };
    const after = buildAfterFromDump({ clickedControl: "現況", dump });
    expect(after.activeTab).toBe("現況");
  });

  it("guest-drive.ps1 does not assign the click control into UI fields", () => {
    const src = readFileSync(path.join(repo, "scripts", "qa", "sandbox", "guest-drive.ps1"), "utf8");
    expect(src).not.toMatch(/activeTab\s*=\s*\$control/);
    expect(src).not.toMatch(/uiState\s*=\s*\$control/);
    expect(src).not.toMatch(/refreshedAt\s*=\s*\(Get-Date/);
    expect(src).toMatch(/dump\.ps1/);
    expect(src).toMatch(/build-after-from-dump/);
  });

  it("dispatches distinct live runners and refuses a shared fileChanged blob", () => {
    expect(liveRunnerFor("subagent.spawn-two")).not.toBe(liveRunnerFor("enhanced.core.edit-function"));
    expect(liveRunnerFor("guardian.known-defect")).not.toBe(liveRunnerFor("enhanced.core.edit-function"));
    expect(liveRunnerFor("enhanced.core.continue")).not.toBe(liveRunnerFor("enhanced.core.cancel"));
    const shared = {
      caseId: "enhanced.core.edit-function",
      runner: "enhanced-edit",
      observationSource: "vellum-enhanced-session-events",
      completed: true,
      execCode: 0,
      fileChanged: true,
      sessionId: "thread-1",
    };
    const edit = scoreLiveCase("enhanced.core.edit-function", shared, { kind: "file-diff" });
    const spawn = scoreLiveCase("subagent.spawn-two", shared, { kind: "file-diff" });
    expect(edit.ok).toBe(true);
    expect(spawn.ok).toBe(false);
    expect(spawn.detail).toMatch(/refusing to score/);
    const spawnOwn = scoreLiveCase(
      "subagent.spawn-two",
      {
        caseId: "subagent.spawn-two",
        runner: "subagent-spawn",
        observationSource: "vellum-enhanced-session-events",
        completed: true,
        execCode: 0,
        fileChanged: true,
        sessionId: "t2",
        children: ["a"],
      },
      { kind: "file-diff" },
    );
    expect(spawnOwn.ok).toBe(false);
    expect(spawnOwn.detail).toMatch(/two distinct child/);
  });

  it("guest-live.ps1 does not spend tokens before the model-bound event adapter exists", () => {
    const src = readFileSync(path.join(repo, "scripts", "qa", "sandbox", "guest-live.ps1"), "utf8");
    expect(src).toMatch(/case-driver-unavailable/);
    expect(src).toMatch(/pinned model/);
    expect(src).toMatch(/round budget/);
    expect(src).not.toMatch(/function\s+Invoke-Codex|&\s+\$cli|Start-EnhancedTask/);
  });

  it("rejects live output that lacks native Enhanced event provenance", () => {
    const result = scoreLiveCase(
      "enhanced.core.edit-function",
      { caseId: "enhanced.core.edit-function", runner: "enhanced-edit", fileChanged: true, sessionId: "stale" },
      { kind: "file-diff" },
    );
    expect(result).toMatchObject({ ok: false, verdict: "FAIL", reason: "outcome-not-verified" });
  });

  it("requires two distinct native child identities", () => {
    const result = scoreLiveCase(
      "subagent.spawn-two",
      {
        caseId: "subagent.spawn-two",
        runner: "subagent-spawn",
        observationSource: "vellum-enhanced-session-events",
        completed: true,
        execCode: 0,
        fileChanged: true,
        sessionId: "parent-1",
        children: ["child-1", "child-1"],
      },
      { kind: "file-diff" },
    );
    expect(result.ok).toBe(false);
    expect(result.detail).toMatch(/two distinct child/);
  });

  it("fails when a button click leaves state unchanged", () => {
    const result = assertionAfterAction({
      before: { proxyRunning: false },
      after: { proxyRunning: false },
      verify: { kind: "field-changed", field: "proxyRunning" },
    });
    expect(result.ok).toBe(false);
    expect(result.reason).toBe("state-unchanged");
  });

  it("does not pass a stop action when process observations are missing", () => {
    const result = assertionAfterAction({
      before: {},
      after: {},
      verify: { kind: "process-stopped", name: "Vellum" },
    });
    expect(result).toMatchObject({ ok: false, reason: "outcome-not-verified" });
  });

  it("does not pass usage-source from labels without a measured value", () => {
    const result = assertionAfterAction({
      before: {},
      after: { usageSource: "provider", unit: "tokens" },
      verify: { kind: "usage-source", source: "provider" },
    });
    expect(result).toMatchObject({ ok: false, reason: "outcome-not-verified" });
  });

  it("requires the full attestation digest", () => {
    const result = assertionAfterAction({
      before: {},
      after: { digest: `sha256:${"a".repeat(12)}${"b".repeat(52)}` },
      verify: { kind: "attestation-pin", digest: `sha256:${"a".repeat(12)}${"c".repeat(52)}` },
    });
    expect(result).toMatchObject({ ok: false, reason: "attestation-mismatch" });
  });

  it("does not accept a claimed remote change when host state is identical", () => {
    const result = assertionAfterAction({
      before: { hostState: "ready" },
      after: { clicked: true, vellumRunning: true, hostState: "ready", hostStateChanged: true, readyz: true },
      verify: { kind: "host-state", requireReadyz: true },
    });
    expect(result).toMatchObject({ ok: false, reason: "state-unchanged" });
  });
});

describe("evidence anti-examples", () => {
  it("rejects a PNG that is only a file header", () => {
    const dir = tmp();
    const file = path.join(dir, "shot.png");
    writeFileSync(file, Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]));
    expect(describeEvidenceProblem(file, "shot.png")).toBe("png-header-only");
  });

  it("rejects JSON that parses but misses required keys", () => {
    const dir = tmp();
    const file = path.join(dir, "status.json");
    writeFileSync(file, JSON.stringify({ ok: true }));
    expect(jsonSemanticProblem(file, ["proxyRunning"])).toBe("json-missing:proxyRunning");
  });

  it("rejects duplicate or stale merge rows", () => {
    const problems = rejectMergeProblems(
      [
        { id: "a", verdict: "PASS", observedAt: "2020-01-01T00:00:00Z" },
        { id: "a", verdict: "PASS", observedAt: "2020-01-01T00:00:00Z" },
      ],
      { runStartedAt: "2026-09-09T00:00:00Z", runId: "r1" },
    );
    expect(problems.some((item) => item.kind === "duplicate")).toBe(true);
    expect(problems.some((item) => item.kind === "stale-evidence")).toBe(true);
  });
});

describe("verdict", () => {
  it("makes necessary BLOCKED/NOT_RUN a non-zero lane", () => {
    expect(laneExitCode([{ verdict: "NOT_RUN", necessary: true, reason: "budget-stop" }])).toBe(1);
    expect(laneExitCode([{ verdict: "BLOCKED", necessary: true }])).toBe(1);
    expect(laneExitCode([{ verdict: "NOT_RUN", necessary: false, reason: "policy" }])).toBe(0);
    expect(
      fullAcceptanceAgainstMatrix([{ id: "a", verdict: "PASS" }], [
        { id: "a", necessary: true },
        { id: "b", necessary: true },
      ]),
    ).toBe(false);
  });
});

describe("shipped sandbox CLI", () => {
  it("requires --build-info for the sandbox lane", () => {
    const result = runQa(["--lane", "sandbox"]);
    expect(result.status).toBe(2);
    expect(result.stderr).toMatch(/build-info/);
  });

  it("sandbox dry-run with a real build-info writes a wsb without launching", () => {
    rmSync(path.join(repo, "qa", "runs", "sandbox.lock"), { force: true });
    const fake = fakeInstaller();
    const outDir = tmp();
    const result = runQa([
      "--lane",
      "sandbox",
      "--dry-run",
      "--build-info",
      fake.buildInfo,
      "--out-dir",
      outDir,
    ]);
    expect(result.status).not.toBe(2);
    const report = JSON.parse(readFileSync(path.join(outDir, "report.json"), "utf8"));
    expect(report.lane).toBe("sandbox");
    expect(report.cases.length).toBeGreaterThan(0);
    expect(report.cases.every((item: { verdict: string }) => item.verdict === "NOT_RUN")).toBe(true);
  });
});

describe("new inject anti-examples", () => {
  it.each([
    ["png-header-only", "FAIL", "missing-evidence"],
    ["json-semantic-fail", "FAIL", "missing-evidence"],
    ["stale-evidence", "FAIL", "stale-evidence"],
    ["wrong-model", "FAIL", "wrong-model"],
    ["button-state-unchanged", "FAIL", "state-unchanged"],
    ["candidate-as-live", "NOT_RUN", "case-driver-unavailable"],
    ["wrong-installer", "FAIL", "wrong-installer"],
  ] as const)("%s", (inject, verdict, reason) => {
    const outDir = tmp();
    const result = runQa(["--inject", inject, "--out-dir", outDir]);
    expect(result.status).not.toBe(0);
    const report = JSON.parse(readFileSync(path.join(outDir, "report.json"), "utf8"));
    expect(report.cases[0].verdict).toBe(verdict);
    expect(report.cases[0].reason).toBe(reason);
  });
});
