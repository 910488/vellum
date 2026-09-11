import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { randomUUID } from "node:crypto";

export function newRunId() {
  const stamp = new Date().toISOString().replace(/[:.]/g, "-");
  return `qa-${stamp}-${randomUUID().slice(0, 8)}`;
}

export function acquireRunLock(lockPath, runId, pid = process.pid) {
  mkdirSync(path.dirname(lockPath), { recursive: true });
  if (existsSync(lockPath)) {
    const existing = JSON.parse(readFileSync(lockPath, "utf8"));
    const error = new Error(
      `another sandbox run holds the lock: ${existing.runId} pid=${existing.pid}; not closing it`,
    );
    error.code = "SANDBOX_LOCK";
    error.existing = existing;
    throw error;
  }
  const lock = { runId, pid, acquiredAt: new Date().toISOString() };
  writeFileSync(lockPath, `${JSON.stringify(lock, null, 2)}\n`);
  return lock;
}

export function releaseRunLock(lockPath, runId) {
  if (!existsSync(lockPath)) return false;
  try {
    const existing = JSON.parse(readFileSync(lockPath, "utf8"));
    if (existing.runId !== runId) return false;
  } catch {
    return false;
  }
  rmSync(lockPath, { force: true });
  return true;
}

/**
 * Generate a .wsb that maps only the kit, the single installer dir, this run's
 * output, and an optional encrypted secrets inbox. Never maps the repo,
 * host Codex home, Vellum data, or the Docker socket.
 */
export function renderWsb({
  kitHost,
  installerHost,
  outputHost,
  secretsHost = null,
  onceHost = null,
  memoryMb = 8192,
  logonCommand,
}) {
  const folders = [
    mapFolder(kitHost, "C:\\SandboxKit", true),
    mapFolder(installerHost, "C:\\Installers", true),
    mapFolder(outputHost, "C:\\QaOutput", false),
  ];
  if (secretsHost) folders.push(mapFolder(secretsHost, "C:\\QaSecrets", true));
  if (onceHost) folders.push(mapFolder(onceHost, "C:\\QaOnce", false));
  return `<Configuration>
  <VGpu>Disable</VGpu>
  <Networking>Enable</Networking>
  <AudioInput>Disable</AudioInput>
  <VideoInput>Disable</VideoInput>
  <PrinterRedirection>Disable</PrinterRedirection>
  <ClipboardRedirection>Disable</ClipboardRedirection>
  <MemoryInMB>${Number(memoryMb)}</MemoryInMB>
  <MappedFolders>
${folders.join("\n")}
  </MappedFolders>
  <LogonCommand>
    <Command>${escapeXml(logonCommand)}</Command>
  </LogonCommand>
</Configuration>
`;
}

function mapFolder(host, sandbox, readOnly) {
  if (!host) throw new Error("mapped host folder is required");
  return `    <MappedFolder>
      <HostFolder>${escapeXml(host)}</HostFolder>
      <SandboxFolder>${escapeXml(sandbox)}</SandboxFolder>
      <ReadOnly>${readOnly ? "true" : "false"}</ReadOnly>
    </MappedFolder>`;
}

function escapeXml(value) {
  return String(value)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

export function forbiddenMapping(xml) {
  const text = String(xml).toLowerCase();
  if (text.includes(".codex")) return "host-codex-home";
  if (text.includes("com.vellum.desktop")) return "host-vellum-data";
  if (text.includes("docker.sock") || text.includes("pipe\\docker")) return "docker-socket";
  return null;
}
