import { createHash } from "node:crypto";
import { lstatSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { join, resolve, relative } from "node:path";
import { gunzipSync } from "node:zlib";
import { fileURLToPath } from "node:url";

export function codexArtifact(path, version, target, bundledRelative) {
  if (!/^[0-9][0-9A-Za-z.+-]{0,79}$/.test(version)
    || !/^(x86_64|aarch64)-(unknown-linux-musl|apple-darwin)$/.test(target)) {
    throw new Error("invalid pinned Codex version/target");
  }
  const bytes = readFileSync(path);
  // Tar framing makes this a conservative upper bound on installed file bytes.
  const uncompressedSize = gunzipSync(bytes, { maxOutputLength: 1024 * 1024 * 1024 }).length;
  if (!bytes.length || !uncompressedSize) throw new Error("empty Codex package");
  return {
    url: bundledRelative ? `bundle://${bundledRelative}`
      : `https://github.com/openai/codex/releases/download/rust-v${version}/codex-package-${target}.tar.gz`,
    sha256: createHash("sha256").update(bytes).digest("hex"),
    size: bytes.length,
    uncompressedSize,
  };
}

export function removeStagedCodex(root, paths) {
  for (const path of paths) {
    const within = relative(resolve(root), resolve(path));
    if (within && !within.startsWith("..") && !within.includes(":")) rmSync(path, { force: true });
  }
}

export function assertRemotePayloadSize(root, maxBytes = 80 * 1024 * 1024) {
  let bytes = 0;
  function visit(dir) {
    for (const name of readdirSync(dir)) {
      const path = join(dir, name);
      const stat = lstatSync(path);
      if (stat.isSymbolicLink()) throw new Error(`remote payload contains a link: ${path}`);
      if (stat.isDirectory()) visit(path);
      else bytes += stat.size;
    }
  }
  visit(root);
  if (bytes > maxBytes) throw new Error(`remote payload exceeds 80 MiB: ${bytes} bytes`);
  return bytes;
}

if (process.argv[1] && resolve(process.argv[1]) === resolve(fileURLToPath(import.meta.url))) {
  console.log(`Remote payload: ${assertRemotePayloadSize(process.argv[2])} bytes`);
}
