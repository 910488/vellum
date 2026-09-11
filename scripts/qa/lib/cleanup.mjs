export function cleanupError(failures) {
  const error = new Error(
    `cleanup failed: ${failures.map((item) => item.message || String(item)).join("; ")}`,
  );
  error.code = "CLEANUP";
  error.failures = failures;
  return error;
}

export async function runCleanup(hooks = []) {
  const failures = [];
  for (const hook of hooks) {
    if (!hook) continue;
    try {
      await hook();
    } catch (cause) {
      failures.push(cause instanceof Error ? cause : new Error(String(cause)));
    }
  }
  if (failures.length) throw cleanupError(failures);
}
