/**
 * Detail polling that extends the existing hidden-window pause:
 * hidden document or inactive page does not tick; becoming visible
 * refetches once.
 */
export function startVisiblePoll(options: {
  active: boolean;
  intervalMs: number;
  load: () => void | Promise<void>;
}): () => void {
  if (!options.active) return () => undefined;
  let inFlight = false;
  const tick = () => {
    if (document.visibilityState !== "visible" || inFlight) return;
    inFlight = true;
    try {
      Promise.resolve(options.load())
        .catch(() => undefined)
        .finally(() => {
          inFlight = false;
        });
    } catch {
      inFlight = false;
    }
  };
  const timer = window.setInterval(tick, options.intervalMs);
  const onVisibility = () => {
    if (document.visibilityState === "visible") options.load();
  };
  document.addEventListener("visibilitychange", onVisibility);
  return () => {
    window.clearInterval(timer);
    document.removeEventListener("visibilitychange", onVisibility);
  };
}
