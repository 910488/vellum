import { createHash } from "node:crypto";
import { readFileSync, renameSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { assertRemotePayloadSize, codexArtifact, removeStagedCodex } from "./remote-codex-artifact.mjs";

function artifact(root, relative) {
  const bytes = readFileSync(join(root, relative));
  return {
    url: `bundle://${relative}`,
    sha256: createHash("sha256").update(bytes).digest("hex"),
  };
}

export function finalizeRemoteDarwinManifest(root, packagePath = join(root, "darwin-arm64/codex-package.tar.gz")) {
  const resolvedRoot = resolve(root);
  const manifestPath = join(resolvedRoot, "manifest.json");
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));

  for (const section of ["codex", "agent", "proxy"]) {
    if (manifest[section]?.artifacts == null) {
      throw new Error(`remote manifest is missing ${section}.artifacts`);
    }
  }

  manifest.codex.artifacts["darwin-arm64"] = codexArtifact(packagePath, manifest.codex.pinnedVersion, "aarch64-apple-darwin");
  manifest.agent.artifacts["darwin-arm64"] = artifact(
    resolvedRoot,
    "darwin-arm64/vellum-remote-agent",
  );
  manifest.proxy.artifacts["darwin-arm64"] = artifact(
    resolvedRoot,
    "darwin-arm64/vellum-proxy-daemon",
  );
  // Adding Darwin must never turn native Linux artifacts back into OCI archives.
  if (![3, 4, 5].includes(manifest.schemaVersion)) {
    throw new Error(`unsupported remote schema: ${manifest.schemaVersion}`);
  }
  manifest.schemaVersion = manifest.schemaVersion === 5 ? 5 : 4;
  manifest.protocolVersion = Math.max(Number(manifest.protocolVersion) || 0, manifest.schemaVersion);
  removeStagedCodex(resolvedRoot, [join(resolvedRoot, "darwin-arm64/codex-package.tar.gz")]);
  assertRemotePayloadSize(resolvedRoot);

  const temporaryPath = `${manifestPath}.tmp`;
  writeFileSync(temporaryPath, `${JSON.stringify(manifest, null, 2)}\n`);
  renameSync(temporaryPath, manifestPath);
  return manifest;
}

if (
  process.argv[1] &&
  resolve(process.argv[1]) === resolve(fileURLToPath(import.meta.url))
) {
  const root = process.argv[2];
  if (!root) {
    throw new Error("usage: node scripts/finalize-remote-darwin-manifest.mjs <resource-root>");
  }
  const manifest = finalizeRemoteDarwinManifest(root, process.argv[3]);
  console.log(
    `Finalized remote schema ${manifest.schemaVersion} protocol ${manifest.protocolVersion} with Darwin ARM64 artifacts.`,
  );
}
