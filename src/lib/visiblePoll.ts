/**
 * Detail polling that extends the existing hidden-window pause:
 * hidden document or inactive page does not tick; becoming visible
 * refetches once.
 */
export function startVisiblePoll(options: {
  active: boolean;
  intervalMs: number;
  load: () => void;
}): () => void {
  if (!options.active) return () => undefined;
  const tick = () => {
    if (document.visibilityState === "visible") options.load();
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
