import { describe, expect, it, vi } from "vitest";
import {
  cachedQuery,
  coalesceQuery,
  invalidateCachedQueries,
  resetQueryCacheForTests,
} from "@/lib/queryCache";
import { startVisiblePoll } from "@/lib/visiblePoll";

describe("query cache", () => {
  it("does not start a second identical query while the first is in flight", async () => {
    resetQueryCacheForTests();
    let starts = 0;
    let release!: (value: string) => void;
    const run = () =>
      coalesceQuery("k", () => {
        starts += 1;
        return new Promise<string>((resolve) => {
          release = resolve;
        });
      });
    const first = run();
    const second = run();
    expect(starts).toBe(1);
    release("ok");
    expect(await first).toBe("ok");
    expect(await second).toBe("ok");
  });

  it("reuses a cached catalog/overview result until settings change", async () => {
    resetQueryCacheForTests();
    let starts = 0;
    const read = () =>
      cachedQuery("catalog-status", async () => {
        starts += 1;
        return { proxyRunning: starts === 1 };
      });
    expect(await read()).toEqual({ proxyRunning: true });
    expect(await read()).toEqual({ proxyRunning: true });
    expect(starts).toBe(1);
    invalidateCachedQueries();
    expect(await read()).toEqual({ proxyRunning: false });
    expect(starts).toBe(2);
  });
});

describe("visible poll", () => {
  it("does not poll when the page is inactive", () => {
    let loads = 0;
    const stop = startVisiblePoll({
      active: false,
      intervalMs: 10,
      load: () => {
        loads += 1;
      },
    });
    stop();
    expect(loads).toBe(0);
  });

  it("refetches once when the window becomes visible", () => {
    let loads = 0;
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => "hidden",
    });
    const stop = startVisiblePoll({
      active: true,
      intervalMs: 60_000,
      load: () => {
        loads += 1;
      },
    });
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => "visible",
    });
    document.dispatchEvent(new Event("visibilitychange"));
    stop();
    expect(loads).toBe(1);
  });

  it("does not overlap a slow visible poll", async () => {
    vi.useFakeTimers();
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => "visible",
    });
    let loads = 0;
    let release!: () => void;
    const pending = new Promise<void>((resolve) => {
      release = resolve;
    });
    const stop = startVisiblePoll({
      active: true,
      intervalMs: 10,
      load: async () => {
        loads += 1;
        await pending;
      },
    });
    await vi.advanceTimersByTimeAsync(35);
    expect(loads).toBe(1);
    release();
    await Promise.resolve();
    await vi.advanceTimersByTimeAsync(10);
    expect(loads).toBe(2);
    stop();
    vi.useRealTimers();
  });
});
