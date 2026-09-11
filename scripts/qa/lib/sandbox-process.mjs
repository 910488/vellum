/** Process names that mean a Windows Sandbox VM is actually alive. */
export const SANDBOX_PROCESS_NAMES = Object.freeze([
  "WindowsSandboxServer",
  "WindowsSandboxRemoteSession",
  "vmmemWindowsSandbox",
  "WindowsSandboxClient",
  "WindowsSandbox",
]);

export const SINGLE_INSTANCE_MESSAGE = "僅允許一個正在執行的 Windows 沙箱執行個體";

export function parseProcessList(stdout) {
  return String(stdout ?? "")
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line && !/^name$/i.test(line) && !/^-+$/.test(line));
}

export function sandboxProcessesIndicateAlive(names) {
  const set = new Set((names ?? []).map((name) => String(name).toLowerCase()));
  return SANDBOX_PROCESS_NAMES.some((name) => set.has(name.toLowerCase()));
}

export function refuseSecondSandboxInstance(names) {
  if (sandboxProcessesIndicateAlive(names)) {
    return { allowed: false, detail: SINGLE_INSTANCE_MESSAGE };
  }
  return { allowed: true };
}

/**
 * Wait until a guest-mapped file appears. Used for HEARTBEAT / DONE.json.
 * Quiet unless the file changes; callers log at most every quietLogMs.
 */
export function parseGuestJson(text) {
  const raw = String(text ?? "").replace(/^\uFEFF/, "").trim();
  if (!raw) {
    const error = new Error("empty guest json");
    error.code = "GUEST_JSON_EMPTY";
    throw error;
  }
  return JSON.parse(raw);
}

export async function waitForGuestFile(filePath, {
  timeoutMs,
  pollMs = 5000,
  existsSync,
  readFileSync,
  sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
  now = () => Date.now(),
  parseJson = false,
} = {}) {
  if (typeof existsSync !== "function") throw new Error("existsSync required");
  const deadline = now() + timeoutMs;
  let last = null;
  const started = now();
  while (now() < deadline) {
    if (existsSync(filePath)) {
      const text = readFileSync ? readFileSync(filePath, "utf8") : "";
      last = text;
      if (!parseJson) {
        return { found: true, text, elapsedMs: now() - started, changed: true };
      }
      try {
        const value = parseGuestJson(text);
        return { found: true, text, value, elapsedMs: now() - started, changed: true };
      } catch {
        // PowerShell Set-Content writes a BOM or a partial file; keep waiting.
      }
    }
    await sleep(pollMs);
  }
  return { found: false, text: last, elapsedMs: now() - started, changed: false };
}
