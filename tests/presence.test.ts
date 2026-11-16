import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { IDLE_AFTER_MS, isUserPresent, onPresenceChange } from "@/lib/presence";
import { startVisiblePoll } from "@/lib/visiblePoll";

function setVisibility(state: DocumentVisibilityState, notify = true) {
  Object.defineProperty(document, "visibilityState", { configurable: true, get: () => state });
  if (notify) document.dispatchEvent(new Event("visibilitychange"));
}

describe("user presence", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    setVisibility("visible", false);
    window.dispatchEvent(new Event("pointerdown"));
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("ends after a quiet spell on a visible window and returns with the next input", () => {
    const seen: boolean[] = [];
    const stop = onPresenceChange((present) => seen.push(present));
    expect(isUserPresent()).toBe(true);
    vi.advanceTimersByTime(IDLE_AFTER_MS - 1);
    expect(seen).toEqual([]);
    vi.advanceTimersByTime(1);
    expect(isUserPresent()).toBe(false);
    expect(seen).toEqual([false]);
    window.dispatchEvent(new Event("pointermove"));
    expect(isUserPresent()).toBe(true);
    expect(seen).toEqual([false, true]);
    stop();
  });

  it("moves the deadline with every input", () => {
    const seen: boolean[] = [];
    const stop = onPresenceChange((present) => seen.push(present));
    vi.advanceTimersByTime(IDLE_AFTER_MS / 2);
    window.dispatchEvent(new KeyboardEvent("keydown"));
    vi.advanceTimersByTime(IDLE_AFTER_MS / 2 + 1);
    expect(seen).toEqual([]);
    vi.advanceTimersByTime(IDLE_AFTER_MS / 2);
    expect(seen).toEqual([false]);
    stop();
  });

  it("is never present while hidden, and showing the window counts as a return", () => {
    const seen: boolean[] = [];
    const stop = onPresenceChange((present) => seen.push(present));
    setVisibility("hidden");
    expect(isUserPresent()).toBe(false);
    window.dispatchEvent(new Event("pointermove"));
    expect(seen).toEqual([false]);
    setVisibility("visible");
    expect(seen).toEqual([false, true]);
    expect(isUserPresent()).toBe(true);
    stop();
  });

  it("stops polling a window nobody is at, then catches up once on return", async () => {
    let loads = 0;
    const stop = startVisiblePoll({
      active: true,
      intervalMs: 60_000,
      load: () => {
        loads += 1;
      },
    });
    await vi.advanceTimersByTimeAsync(IDLE_AFTER_MS - 1);
    expect(loads).toBe(9);
    // Weeks on an unattended Mac read nothing at all.
    await vi.advanceTimersByTimeAsync(24 * 60 * 60_000);
    expect(loads).toBe(9);
    window.dispatchEvent(new Event("focus"));
    expect(loads).toBe(10);
    await vi.advanceTimersByTimeAsync(60_000);
    expect(loads).toBe(11);
    stop();
  });
});
