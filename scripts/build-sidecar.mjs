// Builds the stdio App Server bridge and stages it for bundling.
//
// `CODEX_CLI_PATH` has to point at a real, standalone executable that ships
// with Vellum — Codex Desktop starts it directly, so it cannot be a mode flag
// on the main app. It is staged into `src-tauri/binaries/` and bundled as a
// resource, which is where `packaged_bridge_executable()` looks at runtime.
//
// Bundling as a resource rather than a Tauri `externalBin` is deliberate:
// `externalBin` is validated by `tauri-build`, so a missing sidecar would break
// every plain `cargo build` — including the one this script uses to produce it.
//
// Run from both `beforeDevCommand` and `beforeBuildCommand`, so the file exists
// before `build.rs` records its SHA-256 into the host and, for release, before
// the bundler picks it up.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { stageUnchangedSkip } from "./stage-unchanged.mjs";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const profileArgument = process.argv
  .slice(2)
  .find((argument) => argument.startsWith("--profile="))
  ?.slice("--profile=".length);
const profile = profileArgument ?? process.env.VELLUM_SIDECAR_PROFILE ?? "release";
if (profile !== "debug" && profile !== "release") {
  throw new Error(`unsupported sidecar profile: ${profile}`);
}
const binaryName = "vellum-codex-app-server";

function hostTriple() {
  const output = execFileSync("rustc", ["-vV"], { encoding: "utf8" });
  const line = output.split("\n").find((value) => value.startsWith("host:"));
  if (!line) {
    throw new Error("cannot determine the host target triple from `rustc -vV`");
  }
  return line.replace("host:", "").trim();
}

const targetTriple = hostTriple();
const suffix = targetTriple.includes("windows") ? ".exe" : "";
const isolatedTarget = join(root, "target", "sidecar-build");

execFileSync(
  "cargo",
  [
    "build",
    ...(profile === "release" ? ["--release"] : []),
    "-p",
    "vellum-codex-app-server",
    "--bin",
    binaryName,
  ],
  {
    stdio: "inherit",
    cwd: root,
    env: { ...process.env, CARGO_TARGET_DIR: isolatedTarget },
  },
);

const built = join(isolatedTarget, profile, `${binaryName}${suffix}`);
const digest = createHash("sha256").update(readFileSync(built)).digest("hex");
const relativeStaged =
  profile === "debug"
    ? `binaries/dev/${binaryName}-${digest}${suffix}`
    : `binaries/${binaryName}${suffix}`;
const staged = join(root, "src-tauri", relativeStaged);
mkdirSync(dirname(staged), { recursive: true });
stageUnchangedSkip(built, staged);
if (profile === "debug") {
  writeFileSync(
    join(root, "src-tauri", "binaries", "vellum-codex-app-server.dev-path"),
    `${relativeStaged.replaceAll("\\", "/")}\n`,
    "utf8",
  );
}
console.log(`sidecar staged: ${staged}`);
console.log(`sidecar profile: ${profile}`);
console.log(`sidecar sha256: ${digest}`);

// Enhanced Codex is a separately signed `core-v*` update. Desktop packages
// deliberately ship no core or helpers; the updater creates the first managed
// core slot after download. A debug pointer may still reference a local fork,
// but it is never copied into a Desktop artifact.
const enhancedPointer = join(
  root,
  "src-tauri",
  "binaries",
  "vellum-enhanced-codex.dev-path",
);
if (profile === "debug" && existsSync(enhancedPointer)) {
  console.log(`enhanced core (dev pointer only): ${readFileSync(enhancedPointer, "utf8").trim()}`);
} else {
  console.log("enhanced core: not bundled; install through the signed core update channel");
}
