import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { REPO_ROOT, runProcess } from "./commands.mjs";

function pwsh() {
  return process.platform === "win32" ? "powershell" : "pwsh";
}

export async function probeDesktop(outDir) {
  const logPath = path.join(outDir, "launcher-desktop.log");
  mkdirSync(outDir, { recursive: true });
  const script = path.join(REPO_ROOT, "scripts", "qa", "desktop", "probe.ps1");
  const result = await runProcess(
    [pwsh(), "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", script],
    { timeoutMs: 30_000 },
  );
  const combined = `${result.stdout}\n${result.stderr}`;
  writeFileSync(logPath, combined, "utf8");
  let parsed = null;
  try {
    parsed = JSON.parse(result.stdout.trim().split("\n").at(-1));
  } catch {
    parsed = null;
  }
  const ok = Boolean(parsed?.ok);
  return {
    ok,
    reason: parsed?.reason ?? (result.code === 0 ? "not-found" : "probe-failed"),
    parsed,
    logPath,
    stdout: result.stdout,
    stderr: result.stderr,
  };
}

export async function probeDocker(outDir) {
  const logPath = path.join(outDir, "launcher-remote.log");
  mkdirSync(outDir, { recursive: true });
  const result = await runProcess(["docker", "info"], { timeoutMs: 30_000 });
  writeFileSync(logPath, `${result.stdout}\n${result.stderr}`, "utf8");
  return {
    ok: result.code === 0,
    reason: result.code === 0 ? "docker-ok" : "docker-unavailable",
    logPath,
    stdout: result.stdout,
    stderr: result.stderr,
  };
}

export function probeLiveCredentials(env = process.env) {
  const enabled = env.VELLUM_QA_LIVE === "1" || env.VELLUM_QA_LIVE === "true";
  const keys = [
    "VELLUM_QA_OPENCODE_KEY",
    "VELLUM_QA_GROK_KEY",
    "VELLUM_QA_QWEN_KEY",
    "OPENCODE_API_KEY",
    "XAI_API_KEY",
  ];
  const present = keys.filter((key) => Boolean(env[key]));
  if (!enabled) {
    return {
      ok: false,
      reason: "live-not-enabled",
      detail: "set VELLUM_QA_LIVE=1 to run live sessions",
      present,
    };
  }
  // Candidate live stages the production Desktop credential store into an
  // isolated data root. Environment variables are diagnostic only; requiring
  // one here would block valid stored Qwen/OpenCode/Grok credentials before
  // the real launcher gets a chance to validate them.
  return { ok: true, reason: "enabled", present };
}

export function probeReleaseBuild() {
  const candidates = [
    path.join(REPO_ROOT, "target", "local-release", "build-info.json"),
    path.join(REPO_ROOT, "src-tauri", "target", "release", "vellum.exe"),
  ];
  const found = candidates.filter((item) => existsSync(item));
  return { ok: found.length > 0, found };
}
