/**
 * Whether anyone is at this window.
 *
 * A visible document is not enough. A maximized window on a Mac nobody sits
 * at, or one buried under other windows on Windows, stays "visible" for
 * weeks, so every poll and animation keeps running. WKWebView also keeps a
 * little memory from every IPC response: four weeks of status refreshes on
 * an unattended Mac mini grew one Vellum WebContent process to 9 GB.
 *
 * Polls and animations therefore run only while the window is visible and
 * has seen input recently. The first input after a quiet spell, or the
 * window becoming visible, tells listeners at once so they can refresh.
 */
export const IDLE_AFTER_MS = 10 * 60_000;

type Listener = (present: boolean) => void;

const INPUT_EVENTS = ["pointermove", "pointerdown", "keydown", "wheel", "focus"] as const;

let lastInput = Date.now();
let announced = true;
let idleTimer: ReturnType<typeof setTimeout> | undefined;
const listeners = new Set<Listener>();

function visible(): boolean {
  return document.visibilityState === "visible";
}

export function isUserPresent(): boolean {
  return visible() && Date.now() - lastInput < IDLE_AFTER_MS;
}

/** Calls `listener` when presence starts or ends. Returns the unsubscribe. */
export function onPresenceChange(listener: Listener): () => void {
  listeners.add(listener);
  if (listeners.size === 1) {
    announced = isUserPresent();
    armIdleTimer();
  }
  return () => {
    listeners.delete(listener);
    if (!listeners.size) clearIdleTimer();
  };
}

function announce(present: boolean, always = false) {
  if (!always && present === announced) return;
  announced = present;
  listeners.forEach((listener) => listener(present));
}

function clearIdleTimer() {
  clearTimeout(idleTimer);
  idleTimer = undefined;
}

function armIdleTimer() {
  clearIdleTimer();
  if (!listeners.size || !isUserPresent()) return;
  idleTimer = setTimeout(() => {
    idleTimer = undefined;
    // Input since arming moved the deadline; wait for the new one.
    if (isUserPresent()) armIdleTimer();
    else announce(false);
  }, lastInput + IDLE_AFTER_MS - Date.now());
}

function onInput() {
  lastInput = Date.now();
  if (!announced && visible()) {
    announce(true);
    armIdleTimer();
  }
}

function onVisibility() {
  if (visible()) {
    lastInput = Date.now();
    announce(true, true);
    armIdleTimer();
  } else {
    clearIdleTimer();
    announce(false, true);
  }
}

// Listen from startup: input before the first poll still counts.
if (typeof window !== "undefined") {
  for (const type of INPUT_EVENTS) {
    window.addEventListener(type, onInput, { capture: true, passive: true });
  }
  document.addEventListener("visibilitychange", onVisibility);
}
