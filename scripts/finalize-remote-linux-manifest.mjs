import { createHash } from "node:crypto";
import { readFileSync, renameSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { assertRemotePayloadSize, codexArtifact, removeStagedCodex } from "./remote-codex-artifact.mjs";

export function finalizeRemoteLinuxManifest(root, releaseVersion, codexVersion, agentVersion, sourceFingerprint, packageRoot = root, bundleCodex = false) {
  bundleCodex = bundleCodex === true || bundleCodex === "bundle";
  if (![releaseVersion, codexVersion, agentVersion].every(value => typeof value === "string" && value.length > 0) || !/^[0-9a-f]{12}$/.test(sourceFingerprint)) {
    throw new Error("release, Codex, Agent versions and source fingerprint are required");
  }
  const artifact = relative => {
    const bytes = readFileSync(join(root, relative));
    if (bytes.length === 0) throw new Error(`empty remote artifact: ${relative}`);
    return { url: `bundle://${relative}`, sha256: createHash("sha256").update(bytes).digest("hex") };
  };
  const artifacts = name => ({
    "linux-x64": artifact(`linux-amd64/${name}`),
    "linux-arm64": artifact(`linux-arm64/${name}`),
  });
  const manifest = {
    schemaVersion: 5,
    releaseVersion,
    sourceFingerprint,
    codex: { pinnedVersion: codexVersion, compatibleRange: codexVersion, artifacts: Object.fromEntries([
      ["linux-x64", "linux-amd64", "x86_64-unknown-linux-musl"],
      ["linux-arm64", "linux-arm64", "aarch64-unknown-linux-musl"],
    ].map(([arch, dir, target]) => [arch, codexArtifact(join(packageRoot, dir, "codex-package.tar.gz"), codexVersion, target, bundleCodex ? `${dir}/codex-package.tar.gz` : null)])) },
    agent: { version: agentVersion, artifacts: artifacts("vellum-remote-agent") },
    proxy: { artifacts: artifacts("vellum-proxy-daemon") },
    protocolVersion: 5,
  };
  if (!bundleCodex) {
    removeStagedCodex(root, ["linux-amd64", "linux-arm64"].map(dir => join(root, dir, "codex-package.tar.gz")));
    assertRemotePayloadSize(root);
  }
  const path = join(root, "manifest.json");
  writeFileSync(`${path}.tmp`, `${JSON.stringify(manifest, null, 2)}\n`, "utf8");
  renameSync(`${path}.tmp`, path);
  return manifest;
}

if (process.argv[1] && resolve(process.argv[1]) === resolve(fileURLToPath(import.meta.url))) {
  finalizeRemoteLinuxManifest(...process.argv.slice(2));
}
