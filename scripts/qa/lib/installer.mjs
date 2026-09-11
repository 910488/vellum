import { createHash } from "node:crypto";
import { existsSync, readFileSync, statSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");

export function sha256File(filePath) {
  const hash = createHash("sha256");
  hash.update(readFileSync(filePath));
  return hash.digest("hex");
}

/**
 * Resolve the unique installer named by build-info.json.
 * Never picks "newest mtime" from a directory of setup.exe files.
 */
export function resolveInstaller(buildInfoPath) {
  if (!buildInfoPath) {
    const error = new Error("missing --build-info");
    error.code = "MISSING_BUILD_INFO";
    error.exitCode = 2;
    throw error;
  }
  if (!existsSync(buildInfoPath)) {
    const error = new Error(`build-info not found: ${buildInfoPath}`);
    error.code = "BUILD_INFO_NOT_FOUND";
    error.exitCode = 2;
    throw error;
  }
  let info;
  try {
    const raw = readFileSync(buildInfoPath, "utf8").replace(/^\uFEFF/, "");
    info = JSON.parse(raw);
  } catch (cause) {
    const error = new Error(`build-info is not JSON: ${cause.message}`);
    error.code = "BUILD_INFO_INVALID";
    error.exitCode = 2;
    throw error;
  }
  const installer = info.installer;
  if (!installer || !existsSync(installer)) {
    const error = new Error(`installer path in build-info does not exist: ${installer}`);
    error.code = "INSTALLER_MISSING";
    error.exitCode = 2;
    throw error;
  }
  const actualSha = sha256File(installer);
  const expectedSha = normalizeSha(info.installerSha256);
  if (!expectedSha || actualSha !== expectedSha) {
    const error = new Error(
      `installer SHA mismatch: expected ${expectedSha || "(missing)"} got ${actualSha}`,
    );
    error.code = "INSTALLER_SHA_MISMATCH";
    error.exitCode = 1;
    throw error;
  }
  if (info.commit && info.expectedCommit && info.commit !== info.expectedCommit) {
    const error = new Error(
      `build-info commit ${info.commit} does not match expectedCommit ${info.expectedCommit}`,
    );
    error.code = "INSTALLER_COMMIT_MISMATCH";
    error.exitCode = 1;
    throw error;
  }

  const executable = info.executable ? path.resolve(info.executable) : null;
  let executableSha256 = info.executableSha256 ? normalizeSha(info.executableSha256) : null;
  if (executable) {
    if (!existsSync(executable)) {
      const error = new Error(`sidecar executable missing: ${executable}`);
      error.code = "INSTALLER_SIDECAR_MISSING";
      error.exitCode = 1;
      throw error;
    }
    const actualExe = sha256File(executable);
    if (executableSha256 && actualExe !== executableSha256) {
      const error = new Error(`sidecar SHA mismatch: expected ${executableSha256} got ${actualExe}`);
      error.code = "INSTALLER_SIDECAR_SHA_MISMATCH";
      error.exitCode = 1;
      throw error;
    }
    executableSha256 = actualExe;
  }

  const enhancedCore = verifyEnhancedCore(info, executable);
  const remote = verifyRemotePayload(info, installer);

  const releaseKind = info.releaseKind ?? (info.mainRelease ? "main" : "development");
  const development = releaseKind !== "main" || info.mainRelease === false || info.clean === false;
  return {
    buildInfoPath: path.resolve(buildInfoPath),
    installer: path.resolve(installer),
    installerSha256: actualSha,
    executableSha256,
    commit: info.commit ?? null,
    expectedCommit: info.expectedCommit ?? null,
    version: info.version ?? null,
    releaseKind,
    development,
    developmentLabel: development ? "development candidate" : "main release",
    remoteManifestSha256: remote.manifestSha256,
    remoteArtifacts: info.remoteArtifacts ?? null,
    sidecar: executableSha256,
    enhancedCore: enhancedCore.sha256,
    enhancedCoreVerified: enhancedCore.verified,
    remotePayloadVerified: remote.verified,
    builtAt: info.builtAt ?? null,
    size: statSync(installer).size,
  };
}

function normalizeSha(value) {
  return String(value ?? "")
    .replace(/^sha256:/i, "")
    .toLowerCase();
}

function verifyFileSha(filePath, expected, code) {
  const want = normalizeSha(expected);
  if (!want) return sha256File(filePath);
  if (!existsSync(filePath)) {
    const error = new Error(`${code}: missing ${filePath}`);
    error.code = code;
    error.exitCode = 1;
    throw error;
  }
  const actual = sha256File(filePath);
  if (actual !== want) {
    const error = new Error(`${code}: expected ${want} got ${actual} for ${filePath}`);
    error.code = code;
    error.exitCode = 1;
    throw error;
  }
  return actual;
}

function verifyEnhancedCore(info, executable) {
  const lockPath = path.join(REPO_ROOT, "enhanced-runtime.lock.json");
  let pin = normalizeSha(info.enhancedCoreSha256);
  if (!pin && existsSync(lockPath)) {
    try {
      const lock = JSON.parse(readFileSync(lockPath, "utf8").replace(/^\uFEFF/, ""));
      pin = normalizeSha(lock?.artifacts?.["x86_64-pc-windows-msvc"]?.artifactSha256);
    } catch {
      pin = "";
    }
  }
  const candidates = [];
  if (executable) candidates.push(path.join(path.dirname(executable), "binaries", "vellum-enhanced-codex.exe"));
  if (info.artifactDir) {
    candidates.push(
      path.join(info.artifactDir, "target", "release", "binaries", "vellum-enhanced-codex.exe"),
    );
  }
  const found = candidates.find((item) => existsSync(item));
  if (!found) {
    return { sha256: pin || null, verified: false };
  }
  if (pin) verifyFileSha(found, pin, "INSTALLER_ENHANCED_CORE_SHA_MISMATCH");
  return { sha256: sha256File(found), verified: true, path: found };
}

function verifyRemotePayload(info, installerPath) {
  const want = normalizeSha(info.remoteManifestSha256);
  const candidates = [];
  if (info.artifactDir) {
    candidates.push(path.join(info.artifactDir, "target", "release", "resources", "remote", "manifest.json"));
  }
  candidates.push(path.join(path.dirname(installerPath), "..", "..", "resources", "remote", "manifest.json"));
  const manifest = candidates.find((item) => existsSync(item));
  if (want && !manifest) {
    const error = new Error("remote payload manifest missing");
    error.code = "INSTALLER_REMOTE_PAYLOAD_MISSING";
    error.exitCode = 1;
    throw error;
  }
  if (!manifest) return { manifestSha256: want || null, verified: false };
  const manifestSha = want ? verifyFileSha(manifest, want, "INSTALLER_REMOTE_MANIFEST_SHA_MISMATCH") : sha256File(manifest);
  const remoteRoot = path.dirname(manifest);
  const artifacts = info.remoteArtifacts ?? {};
  for (const [arch, files] of Object.entries(artifacts)) {
    const mapping = {
      agent: "vellum-remote-agent",
      broker: "vellum-remote-broker",
      codex: "codex",
      proxyArchive: "proxy-image.tar",
    };
    for (const [key, fileName] of Object.entries(mapping)) {
      const expected = files?.[key];
      if (!expected) continue;
      verifyFileSha(path.join(remoteRoot, arch, fileName), expected, "INSTALLER_REMOTE_ARTIFACT_SHA_MISMATCH");
    }
  }
  return { manifestSha256: manifestSha, verified: true, manifest };
}

/** Explicitly refuse mtime-based installer selection. */
export function refuseNewestMtimePicker() {
  return {
    allowed: false,
    reason: "installers are selected only via build-info.json identity, never newest LastWriteTime",
  };
}
