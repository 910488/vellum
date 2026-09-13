import { createHash } from "node:crypto";
import { createReadStream, createWriteStream, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { pipeline } from "node:stream/promises";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import { execFileSync } from "node:child_process";
import { Readable } from "node:stream";
import { fileURLToPath } from "node:url";

const root = join(fileURLToPath(new URL("..", import.meta.url)));
const lockPath = join(root, "enhanced-runtime.lock.json");
const args = new Map();
for (let index = 2; index < process.argv.length; index += 2) {
  const key = process.argv[index];
  const value = process.argv[index + 1];
  if (!key?.startsWith("--") || !value) {
    throw new Error("usage: sync-enhanced-runtime-lock.mjs --release <latest|tag>");
  }
  args.set(key.slice(2), value);
}

const requestedRelease = args.get("release") ?? "latest";
const lock = JSON.parse(readFileSync(lockPath, "utf8"));
const repository = new URL(lock.sourceRepository).pathname.replace(/^\//, "").replace(/\/$/, "");
if (!/^[\w.-]+\/[\w.-]+$/.test(repository)) {
  throw new Error(`unsupported Enhanced Core repository ${lock.sourceRepository}`);
}

const headers = {
  Accept: "application/vnd.github+json",
  "User-Agent": "vellum-release-lock-sync",
  "X-GitHub-Api-Version": "2022-11-28",
};
if (process.env.GITHUB_TOKEN) headers.Authorization = `Bearer ${process.env.GITHUB_TOKEN}`;

async function githubJson(path) {
  const response = await fetch(`https://api.github.com${path}`, { headers });
  if (!response.ok) throw new Error(`GitHub API ${path} failed: HTTP ${response.status}`);
  return response.json();
}

async function download(url, destination) {
  const response = await fetch(url, { headers: { "User-Agent": headers["User-Agent"] }, redirect: "follow" });
  if (!response.ok || !response.body) {
    throw new Error(`download ${basename(destination)} failed: HTTP ${response.status}`);
  }
  await pipeline(Readable.fromWeb(response.body), createWriteStream(destination));
}

async function sha256File(path) {
  const hash = createHash("sha256");
  await pipeline(createReadStream(path), hash);
  return hash.digest("hex");
}

const release = requestedRelease === "latest"
  ? await githubJson(`/repos/${repository}/releases/latest`)
  : await githubJson(`/repos/${repository}/releases/tags/${encodeURIComponent(requestedRelease)}`);

if (release.draft || release.prerelease) {
  throw new Error(`Enhanced Core release ${release.tag_name} must be a published stable release`);
}
const match = /^vellum-core-([0-9a-f]{40})$/.exec(release.tag_name);
if (!match) throw new Error(`Enhanced Core release tag ${release.tag_name} is not commit-addressed`);

const assets = new Map(release.assets.map((asset) => [asset.name, asset]));
const sumsAsset = assets.get("SHA256SUMS");
if (!sumsAsset) throw new Error(`Enhanced Core release ${release.tag_name} has no SHA256SUMS`);

const temporary = mkdtempSync(join(tmpdir(), "vellum-enhanced-lock-"));
try {
  const sumsPath = join(temporary, "SHA256SUMS");
  await download(sumsAsset.browser_download_url, sumsPath);
  const sums = new Map();
  for (const line of readFileSync(sumsPath, "utf8").split(/\r?\n/)) {
    const parsed = /^([0-9a-fA-F]{64})\s+\*?(.+)$/.exec(line.trim());
    if (parsed) sums.set(parsed[2], parsed[1].toLowerCase());
  }

  for (const [triple, artifact] of Object.entries(lock.artifacts ?? {})) {
    const asset = assets.get(artifact.archiveName);
    const expectedArchive = sums.get(artifact.archiveName);
    if (!asset || !expectedArchive) {
      throw new Error(`${release.tag_name} is missing ${artifact.archiveName} or its SHA256SUMS entry`);
    }
    const archive = join(temporary, artifact.archiveName);
    await download(asset.browser_download_url, archive);
    const archiveHash = await sha256File(archive);
    if (archiveHash !== expectedArchive) {
      throw new Error(`${artifact.archiveName} hash ${archiveHash} != SHA256SUMS ${expectedArchive}`);
    }
    if (asset.digest && asset.digest !== `sha256:${archiveHash}`) {
      throw new Error(`${artifact.archiveName} hash does not match the GitHub asset digest`);
    }

    const extracted = join(temporary, triple);
    execFileSync(
      process.platform === "win32" ? "python" : "python3",
      [join(root, "scripts", "extract-core-tree.py"), archive, extracted],
      { stdio: "inherit" },
    );
    const executable = join(extracted, triple.includes("windows") ? "codex.exe" : "codex");
    if (!existsSync(executable)) throw new Error(`${artifact.archiveName} does not contain ${basename(executable)}`);
    artifact.archiveSha256 = `sha256:${archiveHash}`;
    artifact.artifactSha256 = `sha256:${await sha256File(executable)}`;
  }

  lock.enhancedCodexCommit = match[1];
  writeFileSync(lockPath, `${JSON.stringify(lock, null, 2)}\n`, "utf8");
  console.log(`resolved Enhanced Core ${release.tag_name}`);
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
