import { createHash } from "node:crypto";
import { readFileSync, renameSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

function artifact(root, relative) {
  const bytes = readFileSync(join(root, relative));
  return {
    url: `bundle://${relative}`,
    sha256: createHash("sha256").update(bytes).digest("hex"),
  };
}

export function finalizeRemoteDarwinManifest(root) {
  const resolvedRoot = resolve(root);
  const manifestPath = join(resolvedRoot, "manifest.json");
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));

  for (const section of ["codex", "agent", "proxy"]) {
    if (manifest[section]?.artifacts == null) {
      throw new Error(`remote manifest is missing ${section}.artifacts`);
    }
  }

  manifest.codex.artifacts["darwin-arm64"] = artifact(
    resolvedRoot,
    "darwin-arm64/codex",
  );
  manifest.agent.artifacts["darwin-arm64"] = artifact(
    resolvedRoot,
    "darwin-arm64/vellum-remote-agent",
  );
  manifest.proxy.artifacts["darwin-arm64"] = artifact(
    resolvedRoot,
    "darwin-arm64/vellum-proxy-daemon",
  );
  manifest.schemaVersion = 4;
  manifest.protocolVersion = Math.max(Number(manifest.protocolVersion) || 0, 4);

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
  const manifest = finalizeRemoteDarwinManifest(root);
  console.log(
    `Finalized remote schema ${manifest.schemaVersion} protocol ${manifest.protocolVersion} with Darwin ARM64 artifacts.`,
  );
}
