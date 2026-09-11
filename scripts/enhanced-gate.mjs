#!/usr/bin/env node
// One entry point for the Enhanced Runtime checks, so the loop after a rebuild
// is a command rather than a procedure.
//
//   node scripts/enhanced-gate.mjs status      is it usable right now?  (reads only)
//   node scripts/enhanced-gate.mjs bridge      does the bridge behave?  (~1 min, no Desktop)
//   node scripts/enhanced-gate.mjs installed   does Codex Desktop go through it?
//   node scripts/enhanced-gate.mjs live --provider-url URL --provider-model ID
//                                              does it hold up against a real model?
//
// Extra arguments are forwarded to `vellum-eval`.
//
// Everything this script builds goes into one isolated target directory. It
// never writes `target/debug` and never stages into `src-tauri/binaries/dev/`.
//
// Not `target/debug`, because a bridge Codex Desktop is still running holds
// that file and the linker is refused with an access-denied error that names
// nothing. Keeping the whole gate out of that directory means a live bridge
// cannot block any part of a run, not just the bridge's own link step.
//
// Not staged either, because staging rewrites the dev sidecar pointer the app
// was built against: a command whose job is to tell you whether Enhanced works
// must not be able to invalidate the configuration it is reporting on.
// `build-sidecar.mjs` stages on purpose and is the right script for
// `pnpm dev`; it is the wrong one here.
//
// One directory is also one compile. Split across two, cargo has two caches
// and rebuilds the `vellum` library twice for every run.

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const suffix = process.platform === "win32" ? ".exe" : "";

/** Where everything the gate runs is compiled. Shared with `build-sidecar.mjs`. */
const GATE_TARGET = path.join("target", "sidecar-build");
const [command, ...forwarded] = process.argv.slice(2);

/** The bridge, the reader, and the two children it launches. */
const BRIDGE_RUN_BINARIES = [
  "vellum-eval",
  "vellum-codex-gate-child",
  "vellum-codex-app-server",
];

/** What each command needs compiled, and the arguments it runs with. */
const COMMANDS = {
  status: {
    binaries: ["vellum-eval"],
    args: () => ["enhanced-runtime-status"],
  },
  bridge: {
    binaries: BRIDGE_RUN_BINARIES,
    args: () => ["enhanced-integration-gate", "--mode", "bridge", "--bridge", gateBridge()],
  },
  installed: {
    binaries: ["vellum-eval"],
    args: () => ["enhanced-integration-gate", "--mode", "installed"],
  },
  // Same runtime as `bridge`, real model on the far end. The endpoint and the
  // upstream model id are the caller's: catalog ids are Vellum routing keys and
  // mean nothing to a provider.
  live: {
    binaries: BRIDGE_RUN_BINARIES,
    args: () => ["enhanced-integration-gate", "--mode", "live", "--bridge", gateBridge()],
  },
};

if (!command || !COMMANDS[command]) {
  process.stderr.write(
    `usage: node scripts/enhanced-gate.mjs <${Object.keys(COMMANDS).join("|")}> [extra vellum-eval args]\n`,
  );
  process.exit(2);
}

cargoBuild(COMMANDS[command].binaries);

const run = spawnSync(
  gateBinary("vellum-eval"),
  [...COMMANDS[command].args(), ...forwarded],
  { cwd: root, stdio: "inherit" },
);
process.exit(run.status ?? 1);

function cargoBuild(binaries) {
  const build = spawnSync(
    "cargo",
    [
      "build",
      ...binaries.flatMap((name) => ["-p", name, "--bin", name]),
    ],
    {
      cwd: root,
      stdio: ["inherit", "inherit", "pipe"],
      encoding: "utf8",
      env: { ...process.env, CARGO_TARGET_DIR: path.join(root, GATE_TARGET) },
    },
  );
  process.stderr.write(build.stderr ?? "");
  if (build.status === 0) {
    return;
  }
  reportLockedOutputs(build.stderr ?? "");
  process.exit(build.status ?? 1);
}

function gateBinary(name) {
  const binary = path.join(root, GATE_TARGET, "debug", `${name}${suffix}`);
  if (!existsSync(binary)) {
    throw new Error(`${binary} is missing; the build did not produce it`);
  }
  return binary;
}

function gateBridge() {
  return gateBinary("vellum-codex-app-server");
}

/**
 * Cargo says `failed to remove file <path>` with an OS access-denied error and
 * nothing else. On this project that is nearly always a process still running
 * the previous build. Naming it is the whole difference between a five-second
 * fix and a guessing game. Nothing is stopped here: whether a live bridge may
 * be killed is the operator's call, since it takes Codex Desktop's CLI with it.
 */
function reportLockedOutputs(stderr) {
  const locked = [...stderr.matchAll(/failed to remove file `([^`]+)`/g)].map(
    (match) => match[1],
  );
  if (locked.length === 0) {
    return;
  }
  process.stderr.write("\nThe linker could not replace these binaries:\n");
  for (const file of locked) {
    process.stderr.write(`  ${file}\n`);
    for (const holder of holdersOf(file)) {
      process.stderr.write(`    held by pid ${holder.Id} (${holder.ProcessName})\n`);
    }
  }
  process.stderr.write(
    "\nQuit that process and build again. If it is a bridge, Codex Desktop\n" +
      "started it and keeps it alive until Codex Desktop restarts;\n" +
      "`node scripts/enhanced-gate.mjs status` shows which bridge is live.\n",
  );
}

function holdersOf(file) {
  if (process.platform !== "win32") {
    return [];
  }
  const query = spawnSync(
    "powershell",
    [
      "-NoProfile",
      "-NonInteractive",
      "-Command",
      `Get-Process | Where-Object { $_.Path -eq '${file.replace(/'/g, "''")}' } | ` +
        "Select-Object Id,ProcessName | ConvertTo-Json -Compress",
    ],
    { encoding: "utf8" },
  );
  if (query.status !== 0 || !query.stdout.trim()) {
    return [];
  }
  try {
    const parsed = JSON.parse(query.stdout);
    return Array.isArray(parsed) ? parsed : [parsed];
  } catch {
    return [];
  }
}
