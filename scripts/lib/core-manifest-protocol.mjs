/**
 * Per-target protocol schema digest for signed Enhanced Core manifests.
 * The digest is the same construction desktop_manager probes at startup:
 * SHA-256 over sorted relative paths, a 0 byte, then file bytes.
 */
import { createHash } from "node:crypto";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { basename, join, relative } from "node:path";

export function walkFiles(dir, acc = []) {
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

export function hashSchemaDir(root) {
  const files = walkFiles(root)
    .map((path) => ({
      relative: relative(root, path).replaceAll("\\", "/"),
      bytes: readFileSync(path),
    }))
    .sort((a, b) => (a.relative < b.relative ? -1 : a.relative > b.relative ? 1 : 0));
  if (files.length === 0) {
    throw new Error("schema output was empty");
  }
  const hasher = createHash("sha256");
  for (const file of files) {
    hasher.update(file.relative);
    hasher.update(Buffer.from([0]));
    hasher.update(file.bytes);
  }
  return hasher.digest("hex");
}

export function sidecarDigestPath(assetDir, assetName) {
  return join(assetDir, `${assetName}.protocol-schema-sha256`);
}

export function readSidecarDigest(assetDir, assetName) {
  const path = sidecarDigestPath(assetDir, assetName);
  if (!existsSync(path)) {
    return null;
  }
  const value = readFileSync(path, "utf8").trim().replace(/^sha256:/, "");
  if (!/^[0-9a-f]{64}$/i.test(value)) {
    throw new Error(`invalid protocol sidecar digest in ${path}`);
  }
  return value.toLowerCase();
}

export function canProbePlatform(platform, host = process.platform) {
  if (platform === "windows") {
    return host === "win32";
  }
  if (platform === "macos") {
    return host === "darwin";
  }
  if (platform === "linux") {
    return host === "linux";
  }
  return false;
}

export function coreBinaryName(platform) {
  return platform === "windows" ? "codex.exe" : "codex";
}

export function isCoreArchiveName(name) {
  return /\.(zip|tar\.gz|tgz)$/i.test(name);
}

export function findCoreBinary(extractRoot, platform) {
  const name = coreBinaryName(platform);
  const found = walkFiles(extractRoot).find((path) => basename(path) === name);
  if (!found) {
    throw new Error(`extracted tree does not contain ${name}`);
  }
  return found;
}

/**
 * Choose the signed target protocol digest. A lock-fixture hash that disagrees
 * with a probed binary is never written as the artifact identity.
 */
export function assignTargetProtocolDigest({ lockDigest, probedDigest, sidecarDigest }) {
  const probed = (sidecarDigest ?? probedDigest ?? "").replace(/^sha256:/, "").toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(probed)) {
    throw new Error(
      "refusing to copy the lock fixture protocol hash; probe or sidecar digest is required",
    );
  }
  const lock = (lockDigest ?? "").replace(/^sha256:/, "").toLowerCase();
  if (lock && lock !== probed) {
    return probed;
  }
  return probed;
}
