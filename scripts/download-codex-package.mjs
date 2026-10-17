// Download the exact official runtime package, including helpers and resources.
import { createHash, randomUUID } from "node:crypto";
import { mkdir, rename, rm, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export async function downloadCodexPackage(version, target, destination, fetcher = fetch) {
  if (!/^[0-9][0-9A-Za-z.+-]{0,79}$/.test(version)
    || !/^(x86_64|aarch64)-(unknown-linux-musl|apple-darwin)$/.test(target)) {
    throw new Error("invalid pinned Codex version/target");
  }
  const tag = `rust-v${version}`;
  const name = `codex-package-${target}.tar.gz`;
  const response = await fetcher(`https://api.github.com/repos/openai/codex/releases/tags/${tag}`, {
    headers: { "User-Agent": "vellum-remote-package", Accept: "application/vnd.github+json" },
    signal: AbortSignal.timeout(300_000),
  });
  if (!response.ok) throw new Error(`Codex release lookup failed: ${response.status}`);
  const release = await response.json();
  const asset = release.assets?.find(value => value.name === name);
  const expectedUrl = `https://github.com/openai/codex/releases/download/${tag}/${name}`;
  if (release.tag_name !== tag || asset?.browser_download_url !== expectedUrl
    || !/^sha256:[0-9a-f]{64}$/.test(asset?.digest ?? "")) {
    throw new Error("Codex package release tag, URL or publisher digest is missing/invalid");
  }
  const download = await fetcher(expectedUrl, { signal: AbortSignal.timeout(300_000) });
  if (!download.ok) throw new Error(`Codex package download failed: ${download.status}`);
  if (Number(download.headers.get("content-length")) > 256 * 1024 * 1024) throw new Error("Codex package too large");
  const bytes = Buffer.from(await download.arrayBuffer());
  if (bytes.length > 256 * 1024 * 1024 || bytes.length === 0) throw new Error("Codex package size invalid");
  const sha256 = createHash("sha256").update(bytes).digest("hex");
  if (`sha256:${sha256}` !== asset.digest) throw new Error("Codex package publisher digest mismatch");
  await mkdir(dirname(destination), { recursive: true });
  const temporary = `${destination}.${randomUUID()}.tmp`;
  try {
    await writeFile(temporary, bytes, { flag: "wx" });
    await rename(temporary, destination);
  } finally {
    await rm(temporary, { force: true });
  }
  return sha256;
}

if (process.argv[1] && resolve(process.argv[1]) === resolve(fileURLToPath(import.meta.url))) {
  const [version, target, destination] = process.argv.slice(2);
  if (!version || !target || !destination) throw new Error("usage: download-codex-package.mjs <version> <target> <destination>");
  await downloadCodexPackage(version, target, destination);
}
