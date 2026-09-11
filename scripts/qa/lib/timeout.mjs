export function timeoutError(ms, label) {
  const error = new Error(`${label ?? "operation"} timed out after ${ms}ms`);
  error.code = "TIMEOUT";
  error.timeoutMs = ms;
  return error;
}

export function withTimeout(ms, fn, { label, onTimeout } = {}) {
  if (!Number.isFinite(ms) || ms <= 0) return Promise.resolve().then(fn);
  let timer;
  return new Promise((resolve, reject) => {
    timer = setTimeout(() => {
      const error = timeoutError(ms, label);
      try {
        onTimeout?.(error);
      } catch {
        // ignore observer failures
      }
      reject(error);
    }, ms);
    Promise.resolve()
      .then(fn)
      .then(resolve, reject);
  }).finally(() => {
    clearTimeout(timer);
  });
}
