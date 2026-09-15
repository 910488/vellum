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
  renameSync,
  rmSync,
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
// The Tauri CLI exports its merged bundle configuration while running
// `beforeBuildCommand`. This Cargo invocation compiles the bridge, which
// depends on the Vellum library and therefore runs Vellum's build script too.
// Letting that nested build inherit TAURI_CONFIG makes tauri-build validate
// the Desktop-only resource glob before this script has staged the bridge it
// points at, creating a clean-checkout bootstrap cycle.
const cargoEnvironment = { ...process.env, CARGO_TARGET_DIR: isolatedTarget };
delete cargoEnvironment.TAURI_CONFIG;

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
    env: cargoEnvironment,
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

await stageEnhancedRuntime();

// Stages the Enhanced Codex core and the helper executables Codex looks for
// beside it.
//
// The core is not a user-chosen path — `verify_settings` matches it byte for
// byte against `artifactSha256` in `enhanced-runtime.lock.json`, so exactly one
// file can pass and a release has to be the thing that puts it there.
//
// The helpers are not optional extras. Codex resolves them as siblings of its
// own binary (`<dir>/<name>.exe`, then `<dir>/resources/<name>.exe`); when that
// misses it falls back to the bare name, Windows searches PATH, and the user
// gets a "file not found" dialog in the middle of a turn. Shipping the core
// without them produces a build that verifies, injects, chats — and then fails
// at the first sandboxed shell command.
//
// A debug build stages nothing: the core is a ~300 MB build output and copying
// it on every `pnpm dev` is not worth it. `binaries/vellum-enhanced-codex.dev-path`
// names the fork's output instead, and `build.rs` reads that pointer.
async function stageEnhancedRuntime() {
  const pointer = join(root, "src-tauri", "binaries", "vellum-enhanced-codex.dev-path");
  const pointerValue = existsSync(pointer) ? readFileSync(pointer, "utf8").trim() : "";
  const fromPointer = pointerValue ? dirname(pointerValue) : null;
  const localOverride = process.env.VELLUM_ENHANCED_CODEX_DIR;
  let source = localOverride;

  if (profile === "debug") {
    source ??= fromPointer;
    const core = source ? join(source, `codex${suffix}`) : null;
    stageRelay(source, true);
    console.log(
      core && existsSync(core)
        ? `enhanced core (dev, not copied): ${core}`
        : "enhanced core: not configured; set binaries/vellum-enhanced-codex.dev-path to enable Enhanced locally",
    );
    return;
  }

  const lock = JSON.parse(
    readFileSync(join(root, "enhanced-runtime.lock.json"), "utf8"),
  );
  const platform = lock.artifacts?.[targetTriple];
  const expected = platform?.artifactSha256
    ?? (lock.targetTriple === targetTriple ? lock.artifactSha256 : null);
  if (!expected) {
    throw new Error(
      `enhanced-runtime.lock.json has no Enhanced core artifact pin for ${targetTriple}`,
    );
  }
  if (!source) {
    source = await downloadPinnedRuntime(lock, platform, expected);
  }

  stageRelay(source, false);
  if (process.env.VELLUM_DESKTOP_HOT_UPDATE === "1") {
    console.log("enhanced core: excluded from the Desktop-only hot update");
    return;
  }

  const core = join(source, `codex${suffix}`);
  if (!existsSync(core)) {
    throw new Error(`Enhanced core not found at ${core}`);
  }
  const digest = `sha256:${createHash("sha256").update(readFileSync(core)).digest("hex")}`;
  if (digest !== expected) {
    // Failing here is the whole point: the same mismatch found at run time is
    // a user staring at a settings screen they cannot fix.
    throw new Error(
      `Enhanced core does not match the pinned artifact
  expected ${expected}
  got      ${digest}
  from     ${core}`,
    );
  }
  stageUnchangedSkip(core, join(root, "src-tauri", "binaries", `vellum-enhanced-codex${suffix}`));
  console.log(`enhanced core staged: ${core}`);
  console.log(`enhanced core sha256: ${digest}`);

  const missing = [];
  const helpers = targetTriple.includes("windows")
    ? ["codex-windows-sandbox-setup", "codex-command-runner", "codex-code-mode-host"]
    : ["codex-code-mode-host"];
  for (const helper of helpers) {
    const from = join(source, `${helper}${suffix}`);
    if (!existsSync(from)) {
      missing.push(helper);
      continue;
    }
    stageUnchangedSkip(from, join(root, "src-tauri", "binaries", `${helper}${suffix}`));
    console.log(`enhanced helper staged: ${helper}${suffix}`);
  }
  if (missing.length) {
    throw new Error(`Enhanced helpers missing from ${source}: ${missing.join(", ")}`);
  }
}

function stageRelay(source, debug) {
  const relay = source ? join(source, `vellum-codex-relay${suffix}`) : null;
  if (!relay || !existsSync(relay)) {
    if (debug) {
      console.log("Remote Control relay: not configured for this development build");
      return;
    }
    throw new Error(`Remote Control relay missing from ${source}`);
  }
  const digest = createHash("sha256").update(readFileSync(relay)).digest("hex");
  const relative = debug
    ? `binaries/dev/vellum-codex-relay-${digest}${suffix}`
    : `binaries/vellum-codex-relay${suffix}`;
  const staged = join(root, "src-tauri", relative);
  mkdirSync(dirname(staged), { recursive: true });
  stageUnchangedSkip(relay, staged);
  if (debug) {
    writeFileSync(
      join(root, "src-tauri", "binaries", "vellum-codex-relay.dev-path"),
      `${relative.replaceAll("\\", "/")}\n`,
      "utf8",
    );
  }
  console.log(`Remote Control relay staged: ${staged}`);
  console.log(`Remote Control relay sha256: ${digest}`);
}

// The Windows archive is a zip, which only bsdtar reads, and a Git for Windows
// shell puts GNU tar on PATH ahead of the copy Windows ships. GNU tar also
// reads the leading `C:` of an absolute path as a remote host. Name the system
// binary rather than trusting whatever PATH order the build happens to run
// under.
function bsdtar() {
  if (process.platform !== "win32") {
    return "tar";
  }
  const system = join(process.env.SystemRoot ?? "C:\Windows", "System32", "tar.exe");
  return existsSync(system) ? system : "tar";
}

async function downloadPinnedRuntime(lock, platform, expectedCore) {
  const repository = lock.sourceRepository;
  const commit = lock.enhancedCodexCommit;
  const archiveName = platform?.archiveName;
  const expectedArchive = platform?.archiveSha256;
  if (!repository || !commit || !archiveName || !expectedArchive) {
    throw new Error(
      `release build has no local Enhanced core and the ${targetTriple} download pin is incomplete`,
    );
  }

  const cache = join(root, "target", "enhanced-runtime", commit, targetTriple);
  const cachedCore = join(cache, `codex${suffix}`);
  if (existsSync(cachedCore)) {
    const cachedDigest = `sha256:${createHash("sha256").update(readFileSync(cachedCore)).digest("hex")}`;
    if (cachedDigest === expectedCore) {
      console.log(`enhanced core cache hit: ${cache}`);
      return cache;
    }
  }

  const temporary = `${cache}.tmp-${process.pid}`;
  rmSync(temporary, { recursive: true, force: true });
  mkdirSync(temporary, { recursive: true });
  const archive = join(temporary, archiveName);
  const tag = `vellum-core-${commit}`;
  const url = `${repository}/releases/download/${tag}/${archiveName}`;
  console.log(`downloading Enhanced core: ${url}`);
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok) {
    throw new Error(`download Enhanced core failed: HTTP ${response.status} ${response.statusText}`);
  }
  const bytes = Buffer.from(await response.arrayBuffer());
  const archiveDigest = `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
  if (archiveDigest !== expectedArchive) {
    throw new Error(
      `Enhanced core archive does not match the pinned artifact\n  expected ${expectedArchive}\n  got      ${archiveDigest}\n  from     ${url}`,
    );
  }
  writeFileSync(archive, bytes);
  // Both paths are passed as the working directory and a bare file name: a
  // repository checked out under a path the active ANSI code page cannot
  // encode reaches a non-Unicode `tar.exe` as mojibake if it arrives in argv.
  execFileSync(bsdtar(), ["-xf", archiveName], { cwd: temporary, stdio: "inherit" });
  rmSync(archive, { force: true });

  const core = join(temporary, `codex${suffix}`);
  if (!existsSync(core)) {
    throw new Error(`Enhanced core archive ${archiveName} did not contain codex${suffix}`);
  }
  const coreDigest = `sha256:${createHash("sha256").update(readFileSync(core)).digest("hex")}`;
  if (coreDigest !== expectedCore) {
    throw new Error(
      `downloaded Enhanced core does not match the pinned artifact\n  expected ${expectedCore}\n  got      ${coreDigest}`,
    );
  }

  rmSync(cache, { recursive: true, force: true });
  mkdirSync(dirname(cache), { recursive: true });
  renameSync(temporary, cache);
  return cache;
}
