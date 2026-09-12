/**
 * Writes update-manifest.json covering real release assets.
 * Signing happens separately so the raw bytes are what Ed25519 covers.
 * An empty assets array is a hard error: that signature would not bind
 * any installer / remote package / core archive.
 */
import { createHash } from "node:crypto";
import { readdirSync, readFileSync, statSync, writeFileSync, existsSync } from "node:fs";
import { basename, join } from "node:path";

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

const files = walkFiles(assetDir).filter((path) => {
  const name = basename(path);
  return (
    !name.startsWith(".") &&
    name !== "update-manifest.json" &&
    name !== "update-manifest.json.sig"
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
  schemaVersion: 1,
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
  if (existsSync(lockPath)) {
    const lock = JSON.parse(readFileSync(lockPath, "utf8"));
    manifest.core = {
      upstreamCommit: lock.codexUpstreamCommit ?? "",
      featureProfile: lock.buildProfile ?? "enhanced-mvp-v1",
      helpers: [],
      protocolSchemaSha256: "",
      protocolCompat: "*",
    };
  }
}

writeFileSync("update-manifest.json", `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`wrote update-manifest.json for ${tag} with ${assets.length} assets`);
for (const asset of assets) {
  console.log(`  ${asset.platform}/${asset.arch} ${asset.name} ${asset.sha256}`);
}
