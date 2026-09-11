import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";

import os from "node:os";
import path from "node:path";
import { REPO_ROOT, runProcess } from "./commands.mjs";
import { resolveInstaller } from "./installer.mjs";
import { resolveHostSecrets, sealCredentials, destroyCredentialChannel } from "./credential-channel.mjs";
import { acquireRunLock, newRunId, releaseRunLock, renderWsb, forbiddenMapping } from "./wsb.mjs";
import { executePreparedCase, writeLog } from "./execute.mjs";
import { CASES } from "../../../qa/matrix/v1.mjs";
import { expandModels } from "./models.mjs";
import { DRIVERS } from "../../../qa/matrix/drivers.mjs";
import {
  parseProcessList,
  parseGuestJson,
  refuseSecondSandboxInstance,
  sandboxProcessesIndicateAlive,
  waitForGuestFile,
  SINGLE_INSTANCE_MESSAGE,
} from "./sandbox-process.mjs";

const SANDBOX_EXE = path.join(process.env.WINDIR || "C:\\Windows", "System32", "WindowsSandbox.exe");
const HEARTBEAT_WAIT_MS = 5 * 60 * 1000;
const DONE_WAIT_MS = 40 * 60 * 1000;

export function sandboxExeAvailable() {
  return existsSync(SANDBOX_EXE);
}

export async function launchSandboxDetached(wsbPath, helperPath) {
  const helper = helperPath || `${wsbPath}.launch.ps1`;
  writeFileSync(
    helper,
    `Start-Process -FilePath ${JSON.stringify(SANDBOX_EXE)} -ArgumentList ${JSON.stringify(wsbPath)}\n`,
  );
  const result = await runProcess(
    ["powershell", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", helper],
    { timeoutMs: 20_000, shell: false, inherit: false },
  );
  return { pid: null, stdout: result.stdout, stderr: result.stderr, code: result.code, helper };
}

export async function listSandboxProcessNames() {
  const result = await runProcess(
    [
      "powershell",
      "-NoProfile",
      "-Command",
      "Get-Process WindowsSandboxClient,WindowsSandbox,WindowsSandboxServer,WindowsSandboxRemoteSession,vmmemWindowsSandbox -ErrorAction SilentlyContinue | Select-Object -ExpandProperty ProcessName",
    ],
    { timeoutMs: 15_000, shell: false, inherit: false },
  );
  return parseProcessList(result.stdout);
}

export async function closeOwnedSandboxUi() {
  await runProcess(
    [
      "powershell",
      "-NoProfile",
      "-Command",
      "Get-Process WindowsSandboxRemoteSession,WindowsSandboxServer,WindowsSandboxClient,WindowsSandbox -ErrorAction SilentlyContinue | Stop-Process -Force",
    ],
    { timeoutMs: 20_000, shell: false, inherit: false },
  );
}

function sandboxLaneCases() {
  return expandModels(
    CASES.filter((item) => ["sandbox", "desktop", "remote", "live"].includes(item.lane)),
  );
}

function readJson(filePath) {
  if (!existsSync(filePath)) return null;
  try {
    return parseGuestJson(readFileSync(filePath, "utf8"));
  } catch {
    return { ok: false, error: `${path.basename(filePath)} not JSON` };
  }
}

function copyNamed(srcDir, destDir, names) {
  for (const name of names) {
    const src = path.join(srcDir, name);
    if (existsSync(src)) copyFileSync(src, path.join(destDir, name));
  }
}

function stageKit(kitHost) {
  const kitSources = [
    ["sandbox/bootstrap.ps1", "bootstrap.ps1"],
    ["scripts/qa/sandbox/guest-bootstrap.ps1", "guest-bootstrap.ps1"],
    ["scripts/qa/sandbox/guest-start.cmd", "guest-start.cmd"],
    ["scripts/qa/sandbox/guest-drive.ps1", "guest-drive.ps1"],
    ["scripts/qa/sandbox/guest-remote.ps1", "guest-remote.ps1"],
    ["scripts/qa/sandbox/guest-live.ps1", "guest-live.ps1"],
    ["scripts/qa/sandbox/open-credentials.mjs", "open-credentials.mjs"],
    ["scripts/qa/lib/assert-after-action.mjs", "assert-after-action.mjs"],
    ["scripts/qa/lib/build-after-from-dump.mjs", "build-after-from-dump.mjs"],
    ["scripts/qa/lib/live-runners.mjs", "live-runners.mjs"],
    ["scripts/qa/desktop/dump.ps1", "desktop/dump.ps1"],
    ["scripts/qa/lib/credential-channel.mjs", "credential-channel.mjs"],
    ["scripts/qa/desktop/act.ps1", "desktop/act.ps1"],
    ["scripts/qa/desktop/probe.ps1", "desktop/probe.ps1"],
    ["scripts/qa/desktop/screenshot.ps1", "desktop/screenshot.ps1"],
  ];
  for (const [rel, dest] of kitSources) {
    const src = path.join(REPO_ROOT, rel);
    if (!existsSync(src)) continue;
    const target = path.join(kitHost, dest);
    mkdirSync(path.dirname(target), { recursive: true });
    copyFileSync(src, target);
  }
  writeFileSync(path.join(kitHost, "drivers.json"), `${JSON.stringify(DRIVERS, null, 2)}\n`);
  writeFileSync(
    path.join(kitHost, "remote-buttons.json"),
    `${JSON.stringify(
      Object.entries(DRIVERS)
        .filter(([, spec]) => spec.action?.kind === "remote-ui")
        .map(([id, spec]) => ({ id, control: spec.action.control, process: spec.action.process })),
      null,
      2,
    )}\n`,
  );
  try {
    copyFileSync(process.execPath, path.join(kitHost, "node.exe"));
  } catch {
    // Guest credential unwrap degrades to a human checkpoint if node cannot be staged.
  }
}

async function waitHeartbeatThenDone(outputHost) {
  const heartbeat = path.join(outputHost, "HEARTBEAT");
  const donePath = path.join(outputHost, "DONE.json");
  const hb = await waitForGuestFile(heartbeat, {
    timeoutMs: HEARTBEAT_WAIT_MS,
    pollMs: 5000,
    existsSync,
    readFileSync,
  });
  if (!hb.found) {
    return { heartbeat: false, done: null, names: await listSandboxProcessNames() };
  }
  const doneWait = await waitForGuestFile(donePath, {
    timeoutMs: DONE_WAIT_MS,
    pollMs: 5000,
    existsSync,
    readFileSync,
    parseJson: true,
  });
  return {
    heartbeat: true,
    heartbeatText: hb.text,
    done: doneWait.found ? doneWait.value ?? readJson(donePath) : null,
    names: await listSandboxProcessNames(),
  };
}

async function runOneVm({
  scenario,
  uninstall,
  runRoot,
  kitHost,
  installerHost,
  secretsHost,
  identity,
  runId,
  ctx,
}) {
  const outputHost = path.join(runRoot, "output", scenario);
  mkdirSync(outputHost, { recursive: true });
  const manifest = {
    runId,
    scenario,
    uninstall: Boolean(uninstall),
    installerName: path.basename(identity.installer),
    installerSha256: identity.installerSha256,
    commit: identity.commit,
    development: identity.development,
    developmentLabel: identity.developmentLabel,
    createdAt: new Date().toISOString(),
  };
  writeFileSync(path.join(outputHost, "run-manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
  if (ctx.onceKey && ctx.onceHost) {
    mkdirSync(ctx.onceHost, { recursive: true });
    writeFileSync(path.join(ctx.onceHost, "once.key"), ctx.onceKey, { encoding: "utf8", flag: "w" });
  }

  const alive = await listSandboxProcessNames();
  const gate = refuseSecondSandboxInstance(alive);
  if (!gate.allowed) {
    return {
      scenario,
      launched: false,
      alreadyRunning: true,
      names: alive,
      detail: gate.detail,
      outputHost,
    };
  }

  const logon = "cmd.exe /c C:\\SandboxKit\\guest-start.cmd";
  const wsb = renderWsb({
    kitHost,
    installerHost,
    outputHost,
    secretsHost,
    onceHost: ctx.onceHost || null,
    logonCommand: logon,
  });
  const forbidden = forbiddenMapping(wsb);
  const wsbPath = path.join(runRoot, `${scenario}.wsb`);
  writeFileSync(wsbPath, wsb);
  if (forbidden) {
    return { scenario, launched: false, forbidden, outputHost, wsbPath };
  }

  const launched = await launchSandboxDetached(wsbPath, path.join(runRoot, `${scenario}.launch.ps1`));
  const guest = await waitHeartbeatThenDone(outputHost);
  return {
    scenario,
    launched: true,
    launchedCode: launched.code,
    wsbPath,
    outputHost,
    ...guest,
    sawOurHeartbeat: guest.heartbeat,
  };
}

async function archiveAndClose(vm) {
  if (!vm?.sawOurHeartbeat) {
    return { closed: false, reason: "not-ours-or-no-heartbeat", names: vm?.names ?? [] };
  }
  await closeOwnedSandboxUi();
  const deadline = Date.now() + 90_000;
  let names = [];
  while (Date.now() < deadline) {
    names = await listSandboxProcessNames();
    if (!sandboxProcessesIndicateAlive(names)) return { closed: true, names };
    await new Promise((resolve) => setTimeout(resolve, 3000));
  }
  return { closed: false, names };
}

async function dedicatedHostLifecycle(runId, runRoot, outputHost) {
  const script = path.join(REPO_ROOT, "scripts", "qa", "remote", "dedicated-host.ps1");
  const stateRoot = path.join(runRoot, "dev-host");
  mkdirSync(stateRoot, { recursive: true });
  const up = await runProcess(
    [
      "powershell",
      "-NoProfile",
      "-ExecutionPolicy",
      "Bypass",
      "-File",
      script,
      "-Command",
      "up",
      "-RunId",
      runId,
      "-StateRoot",
      stateRoot,
    ],
    { timeoutMs: 15 * 60 * 1000, shell: false, inherit: false },
  );
  const hostJson = path.join(stateRoot, "host.json");
  if (existsSync(hostJson) && outputHost) {
    copyFileSync(hostJson, path.join(outputHost, "dedicated-host.json"));
  }
  return {
    ok: up.code === 0 && existsSync(hostJson),
    code: up.code,
    stdout: up.stdout,
    stderr: up.stderr,
    stateRoot,
    host: readJson(hostJson),
  };
}

async function dedicatedHostDown(runId, stateRoot) {
  const script = path.join(REPO_ROOT, "scripts", "qa", "remote", "dedicated-host.ps1");
  return runProcess(
    [
      "powershell",
      "-NoProfile",
      "-ExecutionPolicy",
      "Bypass",
      "-File",
      script,
      "-Command",
      "down",
      "-RunId",
      runId,
      "-StateRoot",
      stateRoot,
    ],
    { timeoutMs: 120_000, shell: false, inherit: false },
  );
}

export function destroyOnceChannel(onceHost, ...outputHosts) {
  if (onceHost && existsSync(onceHost)) {
    try {
      rmSync(onceHost, { recursive: true, force: true, maxRetries: 8, retryDelay: 250 });
    } catch {
      try {
        const keyPath = path.join(onceHost, "once.key");
        if (existsSync(keyPath)) writeFileSync(keyPath, Buffer.alloc(0));
      } catch {
        // leave empty
      }
    }
  }
  for (const dir of outputHosts) {
    if (!dir) continue;
    const leaked = path.join(dir, "once.key");
    if (existsSync(leaked)) {
      try {
        rmSync(leaked, { force: true });
      } catch {
        writeFileSync(leaked, Buffer.alloc(0));
      }
    }
  }
}

export function remoteCaseVerdict(matrixCase, remoteUi, host) {
  const rows = remoteUi?.buttons ?? [];
  const row = rows.find((item) => item.id === matrixCase.id || item.id === matrixCase.parentId);
  if (!row) {
    return {
      verdict: "BLOCKED",
      reason: host?.ok ? "unobserved" : "launcher-unavailable",
      detail: host?.ok
        ? "dedicated Docker host is up, but this Remote Manager control was not driven; backend success is not a substitute"
        : `dedicated host not started: ${host?.error || host?.stderr || host?.stdout || "docker unavailable"}`,
    };
  }
  return {
    verdict: row.verdict,
    reason: row.reason,
    detail: row.detail,
  };
}

function guestCaseMap(outputHost) {
  const parsed = readJson(path.join(outputHost, "guest-cases.json"));
  const live = readJson(path.join(outputHost, "guest-live.json"));
  const rows = [...(parsed?.cases ?? []), ...(live?.cases ?? [])];
  const map = Object.create(null);
  for (const row of rows) map[row.id] = row;
  return map;
}

function batchOutcome(ctx, cases, verdict, reason, detail, extra) {
  return Promise.all(
    cases.map((matrixCase) =>
      executePreparedCase(ctx, {
        ...matrixCase,
        model: undefined,
        run: async ({ evidenceDir }) => {
          writeLog(evidenceDir, "events.json", JSON.stringify({ reason, detail, ...(extra ?? {}) }, null, 2));
          if (matrixCase.evidence?.includes("install-log.json")) {
            writeLog(evidenceDir, "install-log.json", JSON.stringify({ blocked: true, reason, detail }));
          }
          if (matrixCase.evidence?.includes("uninstall-log.json")) {
            writeLog(evidenceDir, "uninstall-log.json", JSON.stringify({ ran: false, reason, detail }));
          }
          if (matrixCase.evidence?.[0] && !existsSync(path.join(evidenceDir, matrixCase.evidence[0]))) {
            writeLog(evidenceDir, matrixCase.evidence[0], `${reason}: ${detail}\n`);
          }
          return { verdict, reason, detail };
        },
      }),
    ),
  );
}

export async function runSandbox(ctx) {
  const cases = sandboxLaneCases();
  const buildInfo = ctx.buildInfo;
  if (!buildInfo) {
    return batchOutcome(ctx, cases, "FAIL", "wrong-installer", "sandbox lane requires --build-info");
  }

  let identity;
  try {
    identity = resolveInstaller(buildInfo);
  } catch (error) {
    return batchOutcome(ctx, cases, "FAIL", "wrong-installer", error.message, { code: error.code });
  }

  const runId = ctx.runId || newRunId();
  const runRoot = path.join(os.tmpdir(), "vellum-qa-runs", runId);
  const kitHost = path.join(runRoot, "kit");
  const installerHost = path.join(runRoot, "installer");
  const secretsHost = path.join(runRoot, "secrets");
  const onceHost = path.join(runRoot, "once");
  const lockPath = path.join(REPO_ROOT, "qa", "runs", "sandbox.lock");
  mkdirSync(path.join(REPO_ROOT, "qa", "runs"), { recursive: true });
  writeFileSync(path.join(REPO_ROOT, "qa", "runs", `${runId}.path.txt`), `${runRoot}\n`);
  mkdirSync(kitHost, { recursive: true });
  mkdirSync(installerHost, { recursive: true });
  mkdirSync(secretsHost, { recursive: true });
  mkdirSync(onceHost, { recursive: true });

  let lock;
  try {
    lock = acquireRunLock(lockPath, runId);
  } catch (error) {
    return batchOutcome(ctx, cases, "BLOCKED", "launcher-unavailable", error.message);
  }

  stageKit(kitHost);
  copyFileSync(identity.installer, path.join(installerHost, path.basename(identity.installer)));

  ctx.installerIdentity = identity;
  const secrets = resolveHostSecrets(ctx.env);
  const sealed = sealCredentials(secretsHost, {
    fields: Object.keys(secrets),
    opencode: secrets.opencode ?? null,
    grok: secrets.grok ?? null,
    qwen: secrets.qwen ?? null,
  });
  ctx.onceKey = sealed.key;
  ctx.onceHost = onceHost;

  const launchLog = path.join(ctx.outDir, "launcher-sandbox.log");
  const logState = (extra) => {
    writeFileSync(
      launchLog,
      JSON.stringify(
        {
          runId,
          identity: {
            installerSha256: identity.installerSha256,
            commit: identity.commit,
            developmentLabel: identity.developmentLabel,
            version: identity.version,
          },
          sandboxExe: sandboxExeAvailable(),
          ...extra,
        },
        null,
        2,
      ),
    );
  };

  const previewWsb = renderWsb({
    kitHost,
    installerHost,
    outputHost: path.join(runRoot, "output", "vellum-first"),
    secretsHost,
    onceHost,
    logonCommand: "cmd.exe /c C:\\SandboxKit\\guest-start.cmd",
  });
  const forbidden = forbiddenMapping(previewWsb);
  logState({ forbidden, wsbPreview: previewWsb.includes("guest-start.cmd") });

  if (forbidden) {
    releaseRunLock(lockPath, runId);
    destroyCredentialChannel(secretsHost);
    return batchOutcome(ctx, cases, "FAIL", "error", `wsb mapped forbidden path: ${forbidden}`);
  }

  if (ctx.dryRun) {
    writeFileSync(path.join(runRoot, "dry-run.wsb"), previewWsb);
    releaseRunLock(lockPath, runId);
    destroyCredentialChannel(secretsHost);
    return batchOutcome(ctx, cases, "NOT_RUN", "dry-run", "sandbox dry-run generated wsb only", {
      dryRun: true,
      runId,
    });
  }

  if (!sandboxExeAvailable()) {
    releaseRunLock(lockPath, runId);
    destroyCredentialChannel(secretsHost);
    return batchOutcome(ctx, cases, "BLOCKED", "launcher-unavailable", "Windows Sandbox is not enabled on this host");
  }

  let host = { ok: false, skipped: true };
  try {
    const firstAlive = await listSandboxProcessNames();
    logState({ processesBefore: firstAlive });
    const firstGate = refuseSecondSandboxInstance(firstAlive);
    if (!firstGate.allowed) {
      releaseRunLock(lockPath, runId);
      destroyCredentialChannel(secretsHost);
      return batchOutcome(
        ctx,
        cases,
        "BLOCKED",
        "launcher-unavailable",
        `${SINGLE_INSTANCE_MESSAGE}; existing processes: ${firstAlive.join(", ") || "(none)"}. Not closing another instance.`,
        { processes: firstAlive },
      );
    }

    const vellumOut = path.join(runRoot, "output", "vellum-first");
    mkdirSync(vellumOut, { recursive: true });
    host = await dedicatedHostLifecycle(runId, runRoot, vellumOut).catch((error) => ({
      ok: false,
      error: error.message,
    }));
    if (host.ok && host.host?.keyPath && existsSync(host.host.keyPath)) {
      copyFileSync(host.host.keyPath, path.join(secretsHost, "remote-id_ed25519"));
      if (existsSync(`${host.host.keyPath}.pub`)) {
        copyFileSync(`${host.host.keyPath}.pub`, path.join(secretsHost, "remote-id_ed25519.pub"));
      }
    }
    logState({ dedicatedHost: { ok: host.ok, error: host.error, code: host.code, container: host.host?.container } });

    const vellumFirst = await runOneVm({
      scenario: "vellum-first",
      uninstall: false,
      runRoot,
      kitHost,
      installerHost,
      secretsHost,
      identity,
      runId,
      ctx,
    });
    logState({ vellumFirst: { heartbeat: vellumFirst.heartbeat, done: vellumFirst.done, names: vellumFirst.names } });

    const closed = await archiveAndClose(vellumFirst);
    let codexFirst = {
      scenario: "codex-first",
      launched: false,
      skipped: true,
      detail: "codex-first not started",
    };
    if (vellumFirst.sawOurHeartbeat && closed.closed) {
      codexFirst = await runOneVm({
        scenario: "codex-first",
        uninstall: true,
        runRoot,
        kitHost,
        installerHost,
        secretsHost,
        identity,
        runId,
        ctx,
      });
      await archiveAndClose(codexFirst);
    } else if (vellumFirst.alreadyRunning) {
      codexFirst = { ...codexFirst, alreadyRunning: true, detail: vellumFirst.detail };
    } else if (!closed.closed) {
      codexFirst = {
        ...codexFirst,
        detail: `cannot start second VM; leftover sandbox processes: ${(closed.names ?? []).join(", ")}`,
      };
    }

    if (host.ok) {
      await dedicatedHostDown(runId, host.stateRoot).catch(() => {});
    }

    await archiveAndClose(codexFirst.sawOurHeartbeat ? codexFirst : vellumFirst);
    destroyCredentialChannel(secretsHost);
    destroyOnceChannel(onceHost, vellumFirst.outputHost, codexFirst.outputHost);
    releaseRunLock(lockPath, runId);

    const guestById = {
      ...guestCaseMap(vellumFirst.outputHost || ""),
    };
    const remoteUi = readJson(path.join(vellumFirst.outputHost || "", "remote-ui.json"));
    const uninstallLog = readJson(path.join(codexFirst.outputHost || "", "uninstall-log.json"));

    return Promise.all(
      cases.map((matrixCase) => {
        const liveUnobserved =
          matrixCase.lane === "live" &&
          !matrixCase.officialLive &&
          !guestById[matrixCase.id] &&
          !guestById[matrixCase.parentId];
        return executePreparedCase(ctx, {
          ...matrixCase,
          ...(liveUnobserved
            ? {
                skipVerdict: "NOT_RUN",
                skipReason: "unobserved",
                skipDetail:
                  "live task was not observed inside the sandbox guest; env flags do not count as PASS",
              }
            : {}),
          run: async ({ evidenceDir }) =>
            verdictForChainedCase(matrixCase, {
              evidenceDir,
              vellumFirst,
              codexFirst,
              guestById,
              remoteUi,
              uninstallLog,
              host,
              identity,
            }),
        });
      }),
    );
  } catch (error) {
    if (host?.ok) await dedicatedHostDown(runId, host.stateRoot).catch(() => {});
    try {
      destroyCredentialChannel(secretsHost);
    } catch {
      // Mapped secrets can stay locked while the VM is alive; never skip the report.
    }
    destroyOnceChannel(onceHost);
    try {
      releaseRunLock(lockPath, runId);
    } catch {
      // ignore
    }
    return batchOutcome(ctx, cases, "FAIL", "error", error.message);
  }
}

function verdictForChainedCase(matrixCase, ctx) {
  const { evidenceDir, vellumFirst, codexFirst, guestById, remoteUi, uninstallLog, host, identity } = ctx;
  writeLog(
    evidenceDir,
    "events.json",
    JSON.stringify(
      {
        id: matrixCase.id,
        vellumFirst: { heartbeat: vellumFirst.heartbeat, done: vellumFirst.done, alreadyRunning: vellumFirst.alreadyRunning },
        codexFirst: { heartbeat: codexFirst.heartbeat, done: codexFirst.done, skipped: codexFirst.skipped },
        dedicatedHost: { ok: host.ok, container: host.host?.container },
        identity: { sha: identity.installerSha256, commit: identity.commit },
      },
      null,
      2,
    ),
  );

  if (matrixCase.officialLive || matrixCase.id === "offline.official-live-not-run") {
    writeLog(evidenceDir, matrixCase.evidence?.[0] ?? "policy.txt", "Official live is out of scope this round\n");
    return { verdict: "NOT_RUN", reason: "policy", detail: "Official live excluded this round" };
  }

  if (matrixCase.id === "sandbox.install.vellum-first") {
    copyNamed(vellumFirst.outputHost || "", evidenceDir, ["install-vellum.json", "install-codex.json", "codex-before.json", "webview2.json"]);
    const install = readJson(path.join(vellumFirst.outputHost || "", "install-vellum.json"));
    if (install) writeLog(evidenceDir, "install-log.json", JSON.stringify(install, null, 2));
    else writeLog(evidenceDir, "install-log.json", JSON.stringify({ missing: true, done: vellumFirst.done }));
    if (vellumFirst.alreadyRunning) {
      return { verdict: "BLOCKED", reason: "launcher-unavailable", detail: vellumFirst.detail };
    }
    if (!vellumFirst.heartbeat) {
      return {
        verdict: "BLOCKED",
        reason: "launcher-unavailable",
        detail: "sandbox guest did not write HEARTBEAT",
      };
    }
    if (!vellumFirst.done?.ok) {
      return { verdict: "FAIL", reason: "error", detail: String(vellumFirst.done?.error || "guest bootstrap failed") };
    }
    const before = readJson(path.join(vellumFirst.outputHost, "codex-before.json"));
    if (before?.present) {
      return { verdict: "FAIL", reason: "error", detail: "vellum-first VM already had Codex" };
    }
    return { verdict: "PASS", detail: `vellum-first installer ${vellumFirst.done.installerSha256}` };
  }

  if (matrixCase.id === "sandbox.install.codex-first") {
    copyNamed(codexFirst.outputHost || "", evidenceDir, ["install-vellum.json", "install-codex.json", "codex-before.json"]);
    const install = readJson(path.join(codexFirst.outputHost || "", "install-vellum.json"));
    if (install) writeLog(evidenceDir, "install-log.json", JSON.stringify(install, null, 2));
    else writeLog(evidenceDir, "install-log.json", JSON.stringify({ missing: true, done: codexFirst.done, skipped: codexFirst.skipped }));
    if (codexFirst.skipped || !codexFirst.launched) {
      return {
        verdict: "BLOCKED",
        reason: "launcher-unavailable",
        detail: codexFirst.detail || "second clean VM not reached",
      };
    }
    if (!codexFirst.done?.ok) {
      return { verdict: "FAIL", reason: "error", detail: String(codexFirst.done?.error || "codex-first bootstrap failed") };
    }
    return { verdict: "PASS", detail: "codex-first guest completed" };
  }

  if (matrixCase.id === "sandbox.os.uninstall") {
    if (uninstallLog) writeLog(evidenceDir, "uninstall-log.json", JSON.stringify(uninstallLog, null, 2));
    else writeLog(evidenceDir, "uninstall-log.json", JSON.stringify({ ran: false, note: "uninstall not reached" }));
    if (!uninstallLog?.ran) {
      return { verdict: "BLOCKED", reason: "launcher-unavailable", detail: "uninstall not executed" };
    }
    if (uninstallLog.vellumExeRemaining) {
      return { verdict: "FAIL", reason: "state-unchanged", detail: "uninstaller ran but Vellum.exe remains" };
    }
    return { verdict: "PASS", detail: `uninstall exit ${uninstallLog.exitCode}` };
  }

  if (matrixCase.lane === "desktop") {
    const row = guestById[matrixCase.id] || guestById[matrixCase.parentId];
    if (!row) {
      return {
        verdict: "BLOCKED",
        reason: "launcher-unavailable",
        detail: "sandbox guest did not drive this desktop case",
      };
    }
    if (row.verdict === "PASS" && !row.assertionOk) {
      return { verdict: "FAIL", reason: "outcome-not-verified", detail: row.detail };
    }
    return { verdict: row.verdict, reason: row.reason, detail: row.detail };
  }

  if (matrixCase.lane === "live") {
    const row = guestById[matrixCase.id] || guestById[matrixCase.parentId];
    if (!row) {
      return {
        verdict: "NOT_RUN",
        reason: "unobserved",
        detail: "live task was not observed inside the sandbox guest; env flags do not count as PASS",
      };
    }
    return { verdict: row.verdict, reason: row.reason, detail: row.detail };
  }

  if (matrixCase.lane === "remote") {
    const mapped = remoteCaseVerdict(matrixCase, remoteUi, host);
    writeLog(
      evidenceDir,
      "deploy-log.txt",
      `dedicated-host ok=${host.ok} container=${host.host?.container ?? ""} verdict=${mapped.verdict} ${mapped.detail}\n`,
    );
    const row = (remoteUi?.buttons ?? []).find((item) => item.id === matrixCase.id);
    if (row?.readyz != null) {
      writeLog(
        evidenceDir,
        "readyz.json",
        JSON.stringify({ ready: row.readyz, daemonOwner: row.daemonOwner, hostState: row.hostStateAfter }, null, 2),
      );
    }
    return mapped;
  }

  return { verdict: "NOT_RUN", reason: "unobserved", detail: `sandbox lane did not handle ${matrixCase.id}` };
}
