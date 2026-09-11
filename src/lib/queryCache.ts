/** Bounded in-flight coalesce + TTL cache for identical IPC reads. */

const DEFAULT_CAP = 16;
const DEFAULT_TTL_MS = 5_000;

type Entry<T> = { value: T; at: number; epoch: number };

const inflight = new Map<string, Promise<unknown>>();
const store = new Map<string, Entry<unknown>>();
let epoch = 0;

export function settingsEpoch(): number {
  return epoch;
}

/** Bump when routes, catalog, or proxy lifecycle actually change. */
export function invalidateCachedQueries(prefix?: string): void {
  if (!prefix) {
    epoch += 1;
    store.clear();
    return;
  }
  for (const key of [...store.keys()]) {
    if (key === prefix || key.startsWith(`${prefix}:`) || key.startsWith(`${prefix}?`)) {
      store.delete(key);
    }
  }
}

export function coalesceQuery<T>(key: string, run: () => Promise<T>): Promise<T> {
  const existing = inflight.get(key);
  if (existing) return existing as Promise<T>;
  const pending = run().finally(() => {
    if (inflight.get(key) === pending) inflight.delete(key);
  });
  inflight.set(key, pending);
  return pending;
}

export function cachedQuery<T>(
  key: string,
  run: () => Promise<T>,
  options?: { ttlMs?: number; cap?: number; bypass?: boolean },
): Promise<T> {
  const ttlMs = options?.ttlMs ?? DEFAULT_TTL_MS;
  const cap = options?.cap ?? DEFAULT_CAP;
  const versioned = `${key}#${epoch}`;
  if (!options?.bypass) {
    const hit = store.get(versioned);
    if (hit && Date.now() - hit.at <= ttlMs && hit.epoch === epoch) {
      return Promise.resolve(hit.value as T);
    }
  }
  return coalesceQuery(versioned, async () => {
    const value = await run();
    while (store.size >= cap) {
      let oldestKey: string | undefined;
      let oldestAt = Number.POSITIVE_INFINITY;
      for (const [entryKey, entry] of store) {
        if (entry.at < oldestAt) {
          oldestAt = entry.at;
          oldestKey = entryKey;
        }
      }
      if (!oldestKey) break;
      store.delete(oldestKey);
    }
    store.set(versioned, { value, at: Date.now(), epoch });
    return value;
  });
}

export function resetQueryCacheForTests(): void {
  inflight.clear();
  store.clear();
  epoch = 0;
}
