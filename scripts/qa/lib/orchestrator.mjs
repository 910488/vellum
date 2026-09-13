import { mkdirSync, writeFileSync, copyFileSync, existsSync } from "node:fs";
import path from "node:path";
import { CASES, MATRIX, MATRIX_VERSION, TAB_CONTROL } from "../../../qa/matrix/v1.mjs";
import { LANES, validateInject, validateLane } from "./args.mjs";
import {
  createBudgetTracker,
  CASE_TIMEOUT_MS,
  LIVE_STAGE_TIMEOUT_MS,
} from "./budget.mjs";
import { OFFLINE_ALIASED, OFFLINE_COMMANDS, REPO_ROOT, runProcess } from "./commands.mjs";
import { artifactIdentity, gitIdentity } from "./git.mjs";
import { executePreparedCase, writeLog } from "./execute.mjs";
import { injectBudgetSeed, injectSpecs } from "./inject.mjs";
import { probeDesktop, probeDocker, probeLiveCredentials, probeReleaseBuild } from "./probes.mjs";
import { buildReport, loadPriorFailures, writeReports } from "./report.mjs";
import { laneExitCode } from "./verdict.mjs";
import { expandModels } from "./models.mjs";
import { resolveActionControl } from "./drivers.mjs";
import { driverFor } from "../../../qa/matrix/drivers.mjs";
import { runSandbox } from "./sandbox-lane.mjs";
import { newRunId } from "./wsb.mjs";

const LANE_ORDER = ["offline", "desktop", "remote", "live"];

export function remoteProofPassed(fullLog, testName) {
  const log = String(fullLog);
  return log.includes(`test ${testName} ...`) && /test result: ok\./.test(log);
}

export function desktopDrivenOutcome(control) {
  return {
    verdict: "BLOCKED",
    reason: "outcome-not-verified",
    detail: `drove control ${control} via UI Automation, but no case-specific backend/reload assertion exists yet`,
  };
}

export function candidateLiveOutcome(caseId, route) {
  return {
    verdict: "NOT_RUN",
    reason: "case-driver-unavailable",
    detail: `candidate provider gate passed for ${route}, but it does not drive ${caseId}; no case-specific Enhanced/Desktop/Remote/Subagent/Guardian evidence was produced`,
  };
}

export function casesForLane(lane) {
  if (lane === "all") return CASES.filter((item) => LANE_ORDER.includes(item.lane));
  return CASES.filter((item) => item.lane === lane);
}

/** Coverage alias for offline inject cases. Must be JSON: evidence files end in .json. */
export function offlineInjectCoverageEvidence(runtimeVerdict, replayVerdict, caseId) {
  return `${JSON.stringify(
    {
      cargoProxyRuntime: runtimeVerdict ?? null,
      protocolReplay: replayVerdict ?? null,
      case: caseId,
    },
    null,
    2,
  )}\n`;
}

export function casesForLaneExpanded(lane) {
  return expandModels(casesForLane(lane));
}

function dryRunPlan(lane) {
  const selected = casesForLane(lane);
  return {
    lane,
    matrixVersion: MATRIX_VERSION,
    commands:
      lane === "offline" || lane === "all"
        ? OFFLINE_COMMANDS.map((item) => ({ id: item.id, argv: item.argv }))
        : [],
    caseIds: selected.map((item) => item.id),
    offlineCommandIds: OFFLINE_COMMANDS.map((item) => item.id),
  };
}

function copyEvidence(src, destRel, evidenceDir) {
  if (!src || !existsSync(src)) return;
  const dest = path.join(evidenceDir, destRel);
  mkdirSync(path.dirname(dest), { recursive: true });
  copyFileSync(src, dest);
}

async function runShellCase(ctx, matrixCase, argv, logRel) {
  const commandTimeoutMs = 45 * 60 * 1000;
  return executePreparedCase(ctx, {
    ...matrixCase,
    evidence: matrixCase.evidence,
    commandLog: argv.join(" "),
    timeoutMs: commandTimeoutMs,
    async run({ evidenceDir }) {
      const result = await runProcess(argv, {
        cwd: REPO_ROOT,
        timeoutMs: commandTimeoutMs,
      });
      const log = `${result.stdout}\n${result.stderr}`;
      writeLog(evidenceDir, logRel, log);
      if (result.timedOut) {
        const error = new Error(`${matrixCase.id} timed out`);
        error.code = "TIMEOUT";
        throw error;
      }
      if (result.code !== 0) {
        const statusRead =
          matrixCase.id === "offline.enhanced-gate-status" &&
          /Enhanced Runtime is (not ready|ready)/i.test(log);
        if (statusRead) {
          return {
            verdict: "PASS",
            detail: `status command ran (exit ${result.code}); ${log.trim().split(/\r?\n/).at(-1)}`,
            command: argv.join(" "),
          };
        }
        return {
          verdict: "FAIL",
          reason: "error",
          detail: `exit ${result.code}`,
          command: argv.join(" "),
        };
      }
      return {
        verdict: "PASS",
        detail: `exit 0; log ${logRel} bytes=${log.length}`,
        command: argv.join(" "),
      };
    },
  });
}

async function runOffline(ctx) {
  const records = [];
  const byId = Object.create(null);
  const commandsRan = [];

  for (const command of OFFLINE_COMMANDS) {
    const matrixCase = CASES.find((item) => item.id === command.id);
    const record = await runShellCase(ctx, matrixCase, command.argv, command.log);
    records.push(record);
    byId[command.id] = record;
    commandsRan.push({
      id: command.id,
      argv: command.argv,
      exit: record.verdict === "PASS" ? 0 : 1,
      evidenceDir: record.evidenceDir,
    });
  }

  for (const [aliasId, sources] of Object.entries(OFFLINE_ALIASED)) {
    const matrixCase = CASES.find((item) => item.id === aliasId);
    const sourceRecords = sources.map((id) => byId[id]).filter(Boolean);
    const failed = sourceRecords.filter((item) => item.verdict !== "PASS");
    const record = await executePreparedCase(ctx, {
      ...matrixCase,
      evidence: matrixCase.evidence,
      commandLog: sources.join(" + "),
      async run({ evidenceDir }) {
        const summary = sourceRecords
          .map((item) => `${item.id}=${item.verdict}`)
          .join("\n");
        writeLog(evidenceDir, matrixCase.evidence[0], `${summary}\n`);
        if (failed.length) {
          return {
            verdict: "FAIL",
            reason: "error",
            detail: `aliased commands failed: ${failed.map((item) => item.id).join(", ")}`,
          };
        }
        return { verdict: "PASS", detail: `covered by ${sources.join(", ")}` };
      },
    });
    records.push(record);
  }

  const traitOffline = CASES.filter(
    (item) =>
      item.lane === "offline" &&
      (item.domain === "enhanced-traits" ||
        (item.domain === "guardian-review" && item.automation === "offline-inject")),
  );
  const runtime = byId["offline.cargo-proxy-runtime"];
  const replay = byId["offline.protocol-replay"];
  for (const matrixCase of traitOffline) {
    const record = await executePreparedCase(ctx, {
      ...matrixCase,
      evidence: matrixCase.evidence,
      commandLog: "offline.cargo-proxy-runtime + offline.protocol-replay",
      async run({ evidenceDir }) {
        writeLog(
          evidenceDir,
          matrixCase.evidence[0],
          offlineInjectCoverageEvidence(runtime?.verdict, replay?.verdict, matrixCase.id),
        );
        if (runtime?.verdict !== "PASS") {
          return {
            verdict: "FAIL",
            reason: "error",
            detail: "vellum-proxy-runtime lib tests failed; offline inject coverage missing",
          };
        }
        return {
          verdict: "PASS",
          detail: "offline inject covered by vellum-proxy-runtime --lib (and protocol-replay when present)",
        };
      },
    });
    records.push(record);
  }

  return { records, commandsRan };
}

function blockedBatch(ctx, cases, reason, detail, logCopy) {
  return Promise.all(
    cases.map((matrixCase) =>
      executePreparedCase(ctx, {
        ...matrixCase,
        async run({ evidenceDir }) {
          if (logCopy?.src && logCopy?.dest) {
            copyEvidence(logCopy.src, logCopy.dest, evidenceDir);
            if (matrixCase.evidence?.[0] && matrixCase.evidence[0] !== logCopy.dest) {
              writeLog(
                evidenceDir,
                matrixCase.evidence[0],
                `${reason}: ${detail}\nsee ${logCopy.dest}\n`,
              );
            }
          } else if (matrixCase.evidence?.[0]) {
            writeLog(evidenceDir, matrixCase.evidence[0], `${reason}: ${detail}\n`);
          }
          return { verdict: "BLOCKED", reason, detail };
        },
      }),
    ),
  );
}

function notRunBatch(ctx, cases, reason, detail) {
  return Promise.all(
    cases.map((matrixCase) =>
      executePreparedCase(ctx, {
        ...matrixCase,
        run({ evidenceDir }) {
          if (matrixCase.evidence?.[0]) {
            writeLog(evidenceDir, matrixCase.evidence[0], `${reason}: ${detail}\n`);
          }
          return { verdict: "NOT_RUN", reason, detail };
        },
      }),
    ),
  );
}

async function runDesktop(ctx) {
  const cases = casesForLane("desktop");
  const probe = await probeDesktop(ctx.outDir);
  const release = probeReleaseBuild();
  const isolated = ctx.env.VELLUM_QA_DESKTOP === "1" || ctx.env.VELLUM_QA_DESKTOP === "true";
  if (!isolated) {
    return blockedBatch(
      ctx,
      cases,
      "launcher-unavailable",
      `Refusing to drive a non-isolated Vellum session. Set VELLUM_QA_DESKTOP=1 on a dedicated Windows test account. probe=${probe.reason}; releaseBuild=${JSON.stringify(release.found)}`,
      { src: probe.logPath, dest: "launcher-desktop.log" },
    );
  }
  if (!probe.ok) {
    return blockedBatch(
      ctx,
      cases,
      "launcher-unavailable",
      `desktop probe: ${probe.reason}; releaseBuild=${JSON.stringify(release.found)}; see launcher-desktop.log. Isolated Windows test account/session was not driveable here.`,
      { src: probe.logPath, dest: "launcher-desktop.log" },
    );
  }

  const records = [];
  const scriptDir = path.join(REPO_ROOT, "scripts", "qa", "desktop");
  const act = path.join(scriptDir, "act.ps1");

  async function drive(name, screenshot) {
    const argv = [
      "powershell",
      "-NoProfile",
      "-ExecutionPolicy",
      "Bypass",
      "-File",
      act,
      "-Name",
      name,
    ];
    if (screenshot) argv.push("-Screenshot", screenshot);
    return runProcess(argv, { timeoutMs: 20_000, shell: false });
  }

  for (const matrixCase of cases) {
    const record = await executePreparedCase(ctx, {
      ...matrixCase,
      async run({ evidenceDir }) {
        const png = (matrixCase.evidence ?? []).find((item) => item.toLowerCase().endsWith(".png"));
        const shot = png ? path.join(evidenceDir, png) : null;
        if (shot) mkdirSync(path.dirname(shot), { recursive: true });

        if (matrixCase.automation === "desktop-ui-manual-checkpoint") {
          writeLog(
            evidenceDir,
            "events.json",
            JSON.stringify({ caseId: matrixCase.id, driven: false, reason: "manual-checkpoint" }, null, 2),
          );
          return {
            verdict: "BLOCKED",
            reason: "manual-checkpoint",
            detail: "OS/tray/oauth checkpoint requires operator evidence; control was not driven",
          };
        }

        const tabName = TAB_CONTROL[matrixCase.surface];
        const driver = driverFor(matrixCase.id) || matrixCase.driver;
        const control = resolveActionControl({ ...matrixCase, driver });
        if (!control) {
          writeLog(
            evidenceDir,
            "events.json",
            JSON.stringify({ caseId: matrixCase.id, driven: false, reason: "no-control" }, null, 2),
          );
          return {
            verdict: "FAIL",
            reason: "control-not-driven",
            detail: `${matrixCase.id} has no control name; refusing to PASS on window presence`,
          };
        }

        const nav = await drive(tabName, control === tabName ? shot : null);
        if (nav.code !== 0) {
          writeLog(
            evidenceDir,
            "events.json",
            JSON.stringify({ caseId: matrixCase.id, driven: false, step: "navigate", tabName, stderr: nav.stderr, stdout: nav.stdout }, null, 2),
          );
          return {
            verdict: "FAIL",
            reason: "control-not-driven",
            detail: `failed to open tab ${tabName}: ${(nav.stderr || nav.stdout || "").trim()}`,
          };
        }

        if (control !== tabName) {
          const click = await drive(control, shot);
          writeLog(
            evidenceDir,
            "events.json",
            JSON.stringify({ caseId: matrixCase.id, driven: click.code === 0, tabName, control, stdout: click.stdout, stderr: click.stderr }, null, 2),
          );
          if (click.code !== 0) {
            return {
              verdict: "FAIL",
              reason: "control-not-driven",
              detail: `failed to invoke ${control}: ${(click.stderr || click.stdout || "").trim()}`,
            };
          }
        } else {
          writeLog(
            evidenceDir,
            "events.json",
            JSON.stringify({ caseId: matrixCase.id, driven: true, tabName, control, stdout: nav.stdout }, null, 2),
          );
        }

        const verify = driver?.verify;
        if (!verify) {
          return desktopDrivenOutcome(control);
        }
        return {
          verdict: "FAIL",
          reason: "outcome-not-verified",
          detail: `drove ${control} but host desktop has no isolated backend snapshot to assert ${verify.kind}; sandbox lane is the isolated path`,
        };
      },
    });
    records.push(record);
  }
  return records;
}

async function runRemote(ctx) {
  const cases = casesForLane("remote");
  const docker = await probeDocker(ctx.outDir);
  if (!docker.ok) {
    return blockedBatch(
      ctx,
      cases,
      "launcher-unavailable",
      `docker unavailable: ${docker.reason}. Independently named Docker resources were not driveable here.`,
      { src: docker.logPath, dest: "launcher-remote.log" },
    );
  }

  const e2e = path.join(REPO_ROOT, "scripts", "remote-e2e.ps1");
  const collect = path.join(REPO_ROOT, "scripts", "qa", "remote", "collect-host.ps1");
  const hostDir = path.join(ctx.outDir, "host-collect");
  mkdirSync(hostDir, { recursive: true });

  const result = await runProcess(
    [
      "pwsh",
      "-NoProfile",
      "-ExecutionPolicy",
      "Bypass",
      "-File",
      e2e,
      "-Lane",
      "full",
      "-KeepHost",
    ],
    { timeoutMs: 60 * 60 * 1000, shell: false },
  );
  writeFileSync(path.join(ctx.outDir, "remote-e2e.log"), `${result.stdout}\n${result.stderr}`);

  let collected = false;
  if (result.code === 0 && !result.timedOut) {
    const harvest = await runProcess(
      [
        "pwsh",
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        collect,
        "-OutDir",
        hostDir,
      ],
      { timeoutMs: 60_000, shell: false },
    );
    writeFileSync(path.join(ctx.outDir, "host-collect.log"), `${harvest.stdout}\n${harvest.stderr}`);
    collected = harvest.code === 0;
  }

  const down = await runProcess(
    [
      "pwsh",
      "-NoProfile",
      "-ExecutionPolicy",
      "Bypass",
      "-File",
      path.join(REPO_ROOT, "scripts", "dev-host.ps1"),
      "down",
    ],
    { timeoutMs: 120_000, shell: false },
  );
  writeFileSync(path.join(ctx.outDir, "cleanup.log"), `${down.stdout}\n${down.stderr}`);

  const fullLog = `${result.stdout}\n${result.stderr}`;
  const passedTest = (name) => remoteProofPassed(fullLog, name);
  const proofByCase = new Map([
    ["remote.deploy.clean-host", "bootstrap_converges_a_pristine_dev_host"],
    ["tab.remote.bootstrap-confirm", "bootstrap_converges_a_pristine_dev_host"],
    ["tab.remote.install-codex", "pinned_codex_install_lands_the_manifest_version"],
    ["remote.deploy.config-consistency", "remote_deployment_converges_and_a_second_apply_changes_nothing"],
    ["remote.deploy.reapply-noop", "remote_deployment_converges_and_a_second_apply_changes_nothing"],
    ["tab.remote.apply", "remote_deployment_converges_and_a_second_apply_changes_nothing"],
    ["tab.remote.reapply-noop", "remote_deployment_converges_and_a_second_apply_changes_nothing"],
  ]);
  const coveredByFull = new Set([...proofByCase.keys(), "remote.deploy.health", "remote.deploy.stop-cleanup"]);

  function copyIfPresent(srcName, destRel, evidenceDir) {
    const src = path.join(hostDir, srcName);
    if (!existsSync(src)) return false;
    copyEvidence(src, destRel, evidenceDir);
    return true;
  }

  return Promise.all(
    cases.map((matrixCase) =>
      executePreparedCase(ctx, {
        ...matrixCase,
        cleanupHooks:
          matrixCase.id === "remote.deploy.stop-cleanup" && down.code !== 0
            ? [
                async () => {
                  throw new Error(`dev-host down exit ${down.code}`);
                },
              ]
            : [],
        async run({ evidenceDir }) {
          writeLog(evidenceDir, "deploy-log.txt", `${result.stdout}\n${result.stderr}`);
          writeLog(
            evidenceDir,
            "events.json",
            JSON.stringify(
              {
                caseId: matrixCase.id,
                lane: "full",
                e2eExit: result.code,
                timedOut: result.timedOut,
                collected,
                cleanupExit: down.code,
              },
              null,
              2,
            ),
          );
          copyIfPresent("readyz.json", "readyz.json", evidenceDir);
          copyIfPresent("agent-status.json", "agent-status.json", evidenceDir);
          copyIfPresent("health.json", "health.json", evidenceDir);
          copyIfPresent("config-hash.json", "config-hash.json", evidenceDir);
          copyEvidence(path.join(ctx.outDir, "cleanup.log"), "cleanup.log", evidenceDir);
          copyEvidence(path.join(ctx.outDir, "remote-e2e.log"), "remote-e2e.log", evidenceDir);

          const proofTest = proofByCase.get(matrixCase.id);
          if (matrixCase.evidence?.includes("reapply.json")) {
            const idempotent = Boolean(proofTest && passedTest(proofTest));
            writeLog(
              evidenceDir,
              "reapply.json",
              JSON.stringify(
                {
                  test: proofTest,
                  passed: idempotent,
                  source: "remote-e2e -Lane full",
                },
                null,
                2,
              ),
            );
          }

          if (matrixCase.evidence?.includes("config-hash.json")) {
            writeLog(
              evidenceDir,
              "config-hash.json",
              JSON.stringify(
                {
                  sourceTest: proofTest,
                  passed: Boolean(proofTest && passedTest(proofTest)),
                  assertion: "remote config hash equals Desktop desired config hash after re-plan",
                },
                null,
                2,
              ),
            );
          }

          if (result.timedOut) {
            const error = new Error("remote e2e full timed out");
            error.code = "TIMEOUT";
            throw error;
          }
          if (result.code !== 0) {
            return {
              verdict: "FAIL",
              reason: "error",
              detail: `remote-e2e -Lane full exit ${result.code}`,
            };
          }
          if (!coveredByFull.has(matrixCase.id)) {
            return {
              verdict: "NOT_RUN",
              reason: "unobserved",
              detail: "remote-e2e full did not exercise this control; not counted as PASS",
            };
          }
          if ((matrixCase.id === "remote.deploy.clean-host" || matrixCase.id === "remote.deploy.health") && !collected) {
            return {
              verdict: "FAIL",
              reason: "semantic-evidence-failed",
              detail: "host collection did not satisfy ready=true and native Codex daemon ownership invariants",
            };
          }
          if (proofTest && !passedTest(proofTest)) {
            return {
              verdict: "FAIL",
              reason: "semantic-evidence-failed",
              detail: `remote-e2e exited 0 but did not report ${proofTest} as passed`,
            };
          }
          if (matrixCase.id === "remote.deploy.stop-cleanup" && down.code !== 0) {
            return { verdict: "FAIL", reason: "cleanup-failure", detail: `dev-host down exit ${down.code}` };
          }
          return {
            verdict: "PASS",
            detail: "remote-e2e -Lane full (staged payload + pinned Codex) plus host collect",
          };
        },
      }),
    ),
  );
}

async function runLive(ctx) {
  const cases = casesForLane("live");
  const official = cases.filter((item) => item.officialLive || item.id === "offline.official-live-not-run");
  const rest = expandModels(cases.filter((item) => !official.includes(item)));
  const officialRecords = await notRunBatch(
    ctx,
    official,
    "policy",
    "Official live is out of scope this round; offline contract and routing isolation only.",
  );

  const creds = probeLiveCredentials(ctx.env);
  writeFileSync(
    path.join(ctx.outDir, "launcher-live.log"),
    JSON.stringify({ ...creds, present: creds.present }, null, 2),
  );
  if (!creds.ok) {
    const reason = creds.reason === "missing-credentials" ? "missing-credentials" : "launcher-unavailable";
    const blocked = await blockedBatch(
      ctx,
      rest,
      reason,
      creds.detail ?? creds.reason,
      { src: path.join(ctx.outDir, "launcher-live.log"), dest: "launcher-live.log" },
    );
    return [...officialRecords, ...blocked];
  }

  const artifactDir = path.join(ctx.outDir, "live-eval");
  mkdirSync(artifactDir, { recursive: true });
  const route = ctx.env.VELLUM_QA_LIVE_ROUTE || "qwen";
  const liveRun = await runProcess(
    [
      "cargo",
      "run",
      "-p",
      "vellum-eval",
      "--bin",
      "vellum-eval",
      "--",
      "live",
      "--route",
      route,
      "--artifact-dir",
      artifactDir,
    ],
    { timeoutMs: LIVE_STAGE_TIMEOUT_MS },
  );
  writeFileSync(path.join(ctx.outDir, "live-eval.log"), `${liveRun.stdout}\n${liveRun.stderr}`);

  const started = Date.now();
  const records = [...officialRecords];
  for (const matrixCase of rest) {
    if (Date.now() - started > LIVE_STAGE_TIMEOUT_MS) {
      records.push(
        ...(await notRunBatch(ctx, [matrixCase], "live-stage-timeout", "90 minute live cap reached")),
      );
      continue;
    }
    records.push(
      await executePreparedCase(ctx, {
        ...matrixCase,
        model: matrixCase.model,
        estimate: matrixCase.estimate,
        async run({ evidenceDir }) {
          writeLog(
            evidenceDir,
            "events.json",
            JSON.stringify(
              {
                caseId: matrixCase.id,
                command: "vellum-eval live",
                route,
                exit: liveRun.code,
                timedOut: liveRun.timedOut,
                artifactDir,
              },
              null,
              2,
            ),
          );
          copyEvidence(path.join(ctx.outDir, "live-eval.log"), "live-eval.log", evidenceDir);
          if (liveRun.timedOut) {
            const error = new Error("live eval timed out");
            error.code = "TIMEOUT";
            throw error;
          }
          if (liveRun.code !== 0) {
            return {
              verdict: "BLOCKED",
              reason: "launcher-unavailable",
              detail: `vellum-eval live --route ${route} exit ${liveRun.code}: ${(liveRun.stderr || liveRun.stdout).slice(-500)}`,
            };
          }
          return candidateLiveOutcome(matrixCase.id, route);
        },
      }),
    );
  }
  return records;
}

function collectUnverified(cases) {
  return cases
    .filter(
      (item) =>
        (item.lane === "live" || item.unobservedMeans === "NOT_RUN") &&
        item.verdict !== "PASS" &&
        item.necessary,
    )
    .map((item) => ({
      id: item.id,
      verdict: item.verdict,
      reason: item.reason,
      note: "未驗證 this round",
    }));
}

function collectDefects(cases) {
  return cases
    .filter((item) => item.verdict === "FAIL" && item.reason !== "missing-evidence" && item.domain !== "runner")
    .map((item) => ({
      id: item.id,
      detail: item.detail,
      repro: item.command || item.detail,
      status: "open",
    }));
}

export async function runQa({
  lane,
  inject,
  outDir,
  dryRun = false,
  buildInfo = null,
  env = process.env,
} = {}) {
  if (inject) validateInject(inject);
  else validateLane(lane);

  mkdirSync(outDir, { recursive: true });

  if (dryRun && lane !== "sandbox") {
    const plan = dryRunPlan(lane);
    writeFileSync(path.join(outDir, "dry-run.json"), `${JSON.stringify(plan, null, 2)}\n`);
    const report = buildReport({
      lane,
      matrixVersion: MATRIX_VERSION,
      git: gitIdentity(),
      artifacts: artifactIdentity(),
      cases: plan.caseIds.map((id) => ({
        id,
        domain: "plan",
        lane,
        necessary: true,
        verdict: "NOT_RUN",
        reason: "dry-run",
        detail: "dry-run does not execute",
      })),
      commands: plan.commands,
    });
    const paths = writeReports(outDir, report);
    return { exitCode: 0, report, ...paths, plan };
  }

  const budget = createBudgetTracker();
  if (inject) injectBudgetSeed(inject, budget);

  const ctx = {
    lane: inject ? "inject" : lane,
    outDir,
    env,
    budget,
    caseTimeoutMs: CASE_TIMEOUT_MS,
    buildInfo,
    runId: newRunId(),
    dryRun,
  };

  let records = [];
  let commandsRan = [];

  if (inject) {
    const specs = injectSpecs(inject, outDir);
    for (const spec of specs) {
      records.push(await executePreparedCase(ctx, spec));
    }
  } else if (lane === "offline") {
    const offline = await runOffline(ctx);
    records = offline.records;
    commandsRan = offline.commandsRan;
  } else if (lane === "desktop") {
    records = await runDesktop(ctx);
  } else if (lane === "remote") {
    records = await runRemote(ctx);
  } else if (lane === "live") {
    records = await runLive(ctx);
  } else if (lane === "sandbox") {
    records = await runSandbox(ctx);
  } else if (lane === "all") {
    const offline = await runOffline(ctx);
    records.push(...offline.records);
    commandsRan = offline.commandsRan;
    records.push(...(await runDesktop(ctx)));
    records.push(...(await runRemote(ctx)));
    records.push(...(await runLive(ctx)));
  }

  const historyPath = path.join(REPO_ROOT, "qa", "reports", "first-round", "report.json");
  const report = buildReport({
    lane: ctx.lane,
    matrixVersion: MATRIX_VERSION,
    git: gitIdentity(),
    artifacts: {
      ...artifactIdentity(),
      ...(ctx.installerIdentity
        ? {
            buildInfoPath: ctx.buildInfo,
            buildInfo: {
              installer: ctx.installerIdentity.installer,
              installerSha256: ctx.installerIdentity.installerSha256,
              commit: ctx.installerIdentity.commit,
              expectedCommit: ctx.installerIdentity.expectedCommit,
              version: ctx.installerIdentity.version,
              sidecar: ctx.installerIdentity.sidecar,
              enhancedCore: ctx.installerIdentity.enhancedCore,
              remoteManifestSha256: ctx.installerIdentity.remoteManifestSha256,
              developmentLabel: ctx.installerIdentity.developmentLabel,
            },
          }
        : {}),
    },
    runId: ctx.runId,
    environment: {
      lane,
      sandbox: lane === "sandbox",
      installerSha256: ctx.installerIdentity?.installerSha256 ?? null,
      installerVersion: ctx.installerIdentity?.version ?? null,
      installerCommit: ctx.installerIdentity?.commit ?? null,
    },
    cases: records,
    usage: budget.snapshot(),
    defects: collectDefects(records),
    unverifiedLive: collectUnverified(records),
    priorFailures: loadPriorFailures(historyPath),
    commands: commandsRan,
  });
  const paths = writeReports(outDir, report);
  const required = Boolean(inject) || LANES.includes(lane);
  const exitCode = inject ? laneExitCode(records, { required: true }) : laneExitCode(records, { required });
  return { exitCode, report, ...paths };
}

export { MATRIX, LANES, OFFLINE_COMMANDS };
