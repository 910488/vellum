import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync, existsSync } from "node:fs";
import path from "node:path";
import { REPO_ROOT } from "./commands.mjs";

function git(args) {
  try {
    return execFileSync("git", args, {
      cwd: REPO_ROOT,
      encoding: "utf8",
    }).trim();
  } catch {
    return "";
  }
}

export function gitIdentity() {
  return {
    commit: git(["rev-parse", "HEAD"]),
    short: git(["rev-parse", "--short", "HEAD"]),
    branch: git(["rev-parse", "--abbrev-ref", "HEAD"]),
    dirty: git(["status", "--porcelain"]) !== "",
    originMain: git(["rev-parse", "origin/main"]),
  };
}

export function sha256File(filePath) {
  if (!existsSync(filePath)) return null;
  const hash = createHash("sha256");
  hash.update(readFileSync(filePath));
  return `sha256:${hash.digest("hex")}`;
}

export function artifactIdentity() {
  const lock = path.join(REPO_ROOT, "enhanced-runtime.lock.json");
  const provenanceCandidates = [
    path.join(REPO_ROOT, "target", "local-release", "build-info.json"),
    path.join(REPO_ROOT, "artifacts", "main", "build-info.json"),
  ];
  let buildInfo = null;
  let buildInfoPath = null;
  for (const candidate of provenanceCandidates) {
    if (existsSync(candidate)) {
      buildInfoPath = candidate;
      try {
        buildInfo = JSON.parse(readFileSync(candidate, "utf8"));
      } catch {
        buildInfo = { parseError: true };
      }
      break;
    }
  }
  return {
    enhancedRuntimeLock: sha256File(lock),
    buildInfoPath,
    buildInfo,
    packageJson: sha256File(path.join(REPO_ROOT, "package.json")),
  };
}
