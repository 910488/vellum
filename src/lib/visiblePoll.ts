import { isUserPresent, onPresenceChange } from "./presence";

/**
 * Detail polling that pauses while nobody is at the window (hidden, or no
 * input for a while; see presence.ts). The user's return refetches once.
 */
export function startVisiblePoll(options: {
  active: boolean;
  intervalMs: number;
  load: () => void | Promise<void>;
}): () => void {
  if (!options.active) return () => undefined;
  let inFlight = false;
  const tick = () => {
    if (!isUserPresent() || inFlight) return;
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
  const stopPresence = onPresenceChange((present) => {
    if (present) tick();
  });
  return () => {
    window.clearInterval(timer);
    stopPresence();
  };
}
