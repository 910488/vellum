/**
 * Writes update-manifest.json covering real release assets.
 * Signing happens separately so the raw bytes are what Ed25519 covers.
 * An empty assets array is a hard error: that signature would not bind
 * any installer / remote package / core archive.
 */
import { createHash } from "node:crypto";
import {
  existsSync,
  lstatSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { execFileSync } from "node:child_process";
import { basename, join, relative } from "node:path";
import { tmpdir } from "node:os";
import {
  assignTargetProtocolDigest,
  canProbePlatform,
  findCoreBinary,
  hashSchemaDir,
  isCoreArchiveName,
  readSidecarDigest,
} from "./lib/core-manifest-protocol.mjs";

const component = process.env.COMPONENT;
const tag = process.env.TAG;
const prerelease = process.env.PRERELEASE === "true";
const assetDir = process.env.ASSET_DIR ?? "release-assets";
const keyId = process.env.VELLUM_UPDATE_KEY_ID;
if (!component || !tag || !keyId) {
  console.error("COMPONENT, TAG, and VELLUM_UPDATE_KEY_ID are required");
  process.exit(1);
}

function walkFiles(dir, acc = []) {
  if (!existsSync(dir)) {
    return acc;
  }
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      walkFiles(path, acc);
    } else if (entry.isFile()) {
      acc.push(path);
    }
  }
  return acc;
}

function inferPlatformArch(name) {
  const lower = name.toLowerCase();
  let platform = "linux";
  if (
    lower.includes("windows") ||
    lower.endsWith(".exe") ||
    lower.endsWith(".msi") ||
    lower.includes("nsis") ||
    lower.includes("pc-windows")
  ) {
    platform = "windows";
  } else if (
    lower.includes("macos") ||
    lower.includes("darwin") ||
    lower.includes("apple") ||
    lower.endsWith(".dmg") ||
    lower.includes(".app.")
  ) {
    platform = "macos";
  }
  let arch = "x64";
  if (lower.includes("arm64") || lower.includes("aarch64")) {
    arch = "arm64";
  }
  return { platform, arch };
}

function requiredCoreHelpers(platform) {
  const suffix = platform === "windows" ? ".exe" : "";
  const helpers = [`codex-code-mode-host${suffix}`];
  if (platform === "windows") {
    helpers.push("codex-command-runner.exe", "codex-windows-sandbox-setup.exe");
  }
  return helpers;
}

const files = walkFiles(assetDir).filter((path) => {
  const name = basename(path);
  return (
    !name.startsWith(".") &&
    name !== "update-manifest.json" &&
    name !== "update-manifest.json.sig" &&
    (component !== "core" || isCoreArchiveName(name))
  );
});
if (files.length === 0) {
  console.error(
    `ASSET_DIR ${assetDir} has no files; refusing to sign a manifest with assets: []`,
  );
  process.exit(1);
}

const assets = files
  .map((path) => {
    const name = basename(path);
    const bytes = readFileSync(path);
    const { platform, arch } = inferPlatformArch(name);
    return {
      platform,
      arch,
      name,
      size: statSync(path).size,
      sha256: createHash("sha256").update(bytes).digest("hex"),
    };
  })
  .sort((a, b) => a.name.localeCompare(b.name));

const version = tag.replace(/^(desktop|remote|core)-v/, "");
const manifest = {
  // Existing desktop installations understand schema 1. Core is the only
  // component that needs the schema-2 signed runnable tree.
  schemaVersion: component === "core" ? 2 : 1,
  component,
  version,
  sourceCommit: process.env.GITHUB_SHA ?? "unknown",
  releaseTag: tag,
  sequence: Math.floor(Date.now() / 1000),
  keyId,
  prerelease,
  releaseNotes: process.env.RELEASE_NOTES ?? null,
  minDesktopVersion: "0.2.0",
  bridgeApiCompat: ">=1.0.0 <2.0.0",
  remoteProtocolCompat: ">=3 <5",
  dataFormat: {
    compatible: true,
    irreversibleMigration: false,
    rollbackCompatible: true,
  },
  assets,
};

if (component === "core") {
  const lockPath = join(process.cwd(), "enhanced-runtime.lock.json");
  if (!existsSync(lockPath)) {
    throw new Error("enhanced-runtime.lock.json is required for a core release");
  }
  const lock = JSON.parse(readFileSync(lockPath, "utf8"));
  const protocolVersion = lock.protocolVersion;
  const lockProtocolSchemaSha256 = lock.protocolSchemaSha256?.replace(/^sha256:/, "");
  if (!protocolVersion || !lockProtocolSchemaSha256 || !lock.enhancedCodexCommit) {
    throw new Error("core release identity or pinned protocol fingerprint is incomplete");
  }
  const versionFile = readFileSync(
    join(
      process.cwd(),
      "third_party",
      "codex-app-server-schema",
      protocolVersion,
      "VERSION",
    ),
    "utf8",
  );
  if (
    !versionFile.includes(`codex-cli ${protocolVersion}`) ||
    !versionFile.includes(`sha256: ${lockProtocolSchemaSha256}`)
  ) {
    throw new Error("enhanced-runtime.lock.json does not match the pinned protocol fixture");
  }

  const targets = [];
  for (const asset of assets) {
    const archive = files.find((path) => basename(path) === asset.name);
    const extractRoot = mkdtempSync(join(tmpdir(), "vellum-core-manifest-"));
    try {
      execFileSync(
        process.platform === "win32" ? "python" : "python3",
        [join(process.cwd(), "scripts", "extract-core-tree.py"), archive, extractRoot],
        { stdio: "inherit" },
      );
      const extracted = walkFiles(extractRoot).sort();
      if (extracted.length === 0) {
        throw new Error(`${asset.name} extracted an empty runnable tree`);
      }
      const requiredHelpers = requiredCoreHelpers(asset.platform);
      const records = extracted.map((path) => {
        const info = lstatSync(path);
        if (info.isSymbolicLink() || !info.isFile()) {
          throw new Error(`${asset.name} contains a non-regular file ${path}`);
        }
        const name = relative(extractRoot, path).replaceAll("\\", "/");
        if (info.size <= 0) {
          throw new Error(`${asset.name} contains an empty runnable-tree file ${name}`);
        }
        const base = basename(name);
        const isContractExecutable =
          base === (asset.platform === "windows" ? "codex.exe" : "codex") ||
          requiredHelpers.includes(base);
        const executable = isContractExecutable || (info.mode & 0o111) !== 0;
        return {
          path: name,
          size: info.size,
          sha256: createHash("sha256").update(readFileSync(path)).digest("hex"),
          executable,
        };
      });
      const coreName = asset.platform === "windows" ? "codex.exe" : "codex";
      const executable = records.find((file) => basename(file.path) === coreName)?.path;
      if (!executable) {
        throw new Error(`${asset.name} does not contain ${coreName}`);
      }
      const helpers = requiredHelpers.map((helper) => {
        const found = records.find((file) => basename(file.path) === helper);
        if (!found) {
          throw new Error(`${asset.name} is missing required helper ${helper}`);
        }
        return found.path;
      });
      const sidecarDigest = readSidecarDigest(assetDir, asset.name);
      let probedDigest = sidecarDigest;
      if (!probedDigest) {
        if (!canProbePlatform(asset.platform)) {
          throw new Error(
            `${asset.name}: no protocol sidecar and cannot probe a ${asset.platform} binary on ${process.platform}; refusing to copy the lock fixture hash`,
          );
        }
        probedDigest = probeExtractedCore(extractRoot, asset.platform);
      }
      const protocolSchemaSha256 = assignTargetProtocolDigest({
        lockDigest: lockProtocolSchemaSha256,
        probedDigest,
        sidecarDigest,
      });
      targets.push({
        platform: asset.platform,
        arch: asset.arch,
        asset: asset.name,
        executable,
        helpers,
        uncompressedSize: records.reduce((total, file) => total + file.size, 0),
        protocolSchemaSha256,
        files: records,
      });
    } finally {
      rmSync(extractRoot, { recursive: true, force: true });
    }
  }
  const probed = [...new Set(targets.map((target) => target.protocolSchemaSha256))];
  manifest.core = {
    upstreamCommit: lock.codexUpstreamCommit,
    enhancedCommit: lock.enhancedCodexCommit,
    featureProfile: lock.buildProfile,
    protocolSchemaSha256: probed[0],
    protocolCompat: `>=${protocolVersion} <1.0.0`,
    targets,
  };
}

function probeExtractedCore(extractRoot, platform) {
  const binary = findCoreBinary(extractRoot, platform);
  const schemaRoot = mkdtempSync(join(tmpdir(), "vellum-core-schema-"));
  try {
    execFileSync(binary, ["app-server", "generate-json-schema", "--out", schemaRoot], {
      stdio: "inherit",
      timeout: 30_000,
    });
    return hashSchemaDir(schemaRoot);
  } finally {
    rmSync(schemaRoot, { recursive: true, force: true });
  }
}

writeFileSync("update-manifest.json", `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`wrote update-manifest.json for ${tag} with ${assets.length} assets`);
for (const asset of assets) {
  console.log(`  ${asset.platform}/${asset.arch} ${asset.name} ${asset.sha256}`);
}
