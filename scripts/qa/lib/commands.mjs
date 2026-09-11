import { spawn } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
export const REPO_ROOT = path.resolve(here, "..", "..", "..");

/** Commands the offline lane actually shells out to — tests assert this list. */
export const OFFLINE_COMMANDS = Object.freeze([
  {
    id: "offline.typecheck",
    argv: ["pnpm", "typecheck"],
    log: "logs/typecheck.txt",
  },
  {
    id: "offline.vitest",
    argv: ["pnpm", "test"],
    log: "logs/vitest.txt",
  },
  {
    id: "offline.cargo-proxy-runtime",
    argv: ["cargo", "test", "-p", "vellum-proxy-runtime", "--lib"],
    log: "logs/cargo-proxy-runtime.txt",
  },
  {
    id: "offline.cargo-tauri-lib",
    argv: ["cargo", "test", "--manifest-path", "src-tauri/Cargo.toml", "--lib"],
    log: "logs/cargo-tauri-lib.txt",
  },
  {
    id: "offline.cargo-contract",
    argv: ["cargo", "test", "--manifest-path", "src-tauri/Cargo.toml", "--test", "contract"],
    log: "logs/cargo-contract.txt",
  },
  {
    id: "offline.protocol-replay",
    argv: ["cargo", "run", "-p", "vellum-eval", "--bin", "vellum-eval", "--", "protocol-replay"],
    log: "logs/protocol-replay.txt",
  },
  {
    id: "offline.app-replay-desktop",
    argv: [
      "cargo",
      "run",
      "-p",
      "vellum-eval",
      "--bin",
      "vellum-eval",
      "--",
      "app-replay",
      "--suite",
      "desktop-protocol-replay",
    ],
    log: "logs/app-replay-desktop.txt",
  },
  {
    id: "offline.app-replay-compaction",
    argv: [
      "cargo",
      "run",
      "-p",
      "vellum-eval",
      "--bin",
      "vellum-eval",
      "--",
      "app-replay",
      "--suite",
      "canonical-compaction-12",
    ],
    log: "logs/app-replay-compaction.txt",
  },
  {
    id: "offline.enhanced-gate-status",
    argv: ["node", "scripts/enhanced-gate.mjs", "status"],
    log: "logs/enhanced-gate-status.txt",
  },
]);

/** Offline contract cases that are covered by the command group above rather than extra shells. */
export const OFFLINE_ALIASED = Object.freeze({
  "offline.official-passthrough": [
    "offline.protocol-replay",
    "offline.cargo-proxy-runtime",
    "offline.cargo-contract",
  ],
  "offline.cross-provider-isolation": ["offline.cargo-proxy-runtime", "offline.cargo-tauri-lib"],
  "offline.streaming": ["offline.protocol-replay", "offline.cargo-proxy-runtime"],
  "offline.continuation-compaction": [
    "offline.app-replay-compaction",
    "offline.cargo-proxy-runtime",
  ],
  "offline.error-classification": ["offline.cargo-proxy-runtime", "offline.cargo-contract"],
});

export function runProcess(
  argv,
  { cwd = REPO_ROOT, env = process.env, timeoutMs, onSpawn, shell, inherit = true } = {},
) {
  return new Promise((resolve) => {
    const [rawCommand, ...args] = argv;
    const win = process.platform === "win32";
    const useShell =
      shell ?? (win && ["pnpm", "npm", "npx"].includes(rawCommand));
    const command = win && rawCommand === "pnpm" && !useShell ? "pnpm.cmd" : rawCommand;
    const child = spawn(command, args, {
      cwd,
      env,
      windowsHide: true,
      shell: useShell,
    });
    onSpawn?.(child);
    let stdout = "";
    let stderr = "";
    child.stdout?.on("data", (chunk) => {
      stdout += chunk.toString();
      if (inherit) process.stdout.write(chunk);
    });
    child.stderr?.on("data", (chunk) => {
      stderr += chunk.toString();
      if (inherit) process.stderr.write(chunk);
    });
    let timedOut = false;
    let timer;
    if (timeoutMs && timeoutMs > 0) {
      timer = setTimeout(() => {
        timedOut = true;
        killProcess(child);
      }, timeoutMs);
    }
    child.on("error", (error) => {
      clearTimeout(timer);
      resolve({
        code: 1,
        stdout,
        stderr: `${stderr}${error.message}`,
        timedOut,
        error,
      });
    });
    child.on("close", (code, signal) => {
      clearTimeout(timer);
      resolve({
        code: timedOut ? 124 : code ?? (signal ? 1 : 0),
        stdout,
        stderr,
        timedOut,
        signal,
      });
    });
  });
}

export function killProcess(child) {
  if (!child || child.killed) return;
  try {
    if (process.platform === "win32" && child.pid) {
      spawn("taskkill", ["/pid", String(child.pid), "/t", "/f"], {
        windowsHide: true,
        stdio: "ignore",
        shell: true,
      });
    } else {
      child.kill("SIGTERM");
    }
  } catch {
    try {
      child.kill();
    } catch {
      // ignore
    }
  }
}
