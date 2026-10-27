import { useSyncExternalStore } from "react";
import { api } from "./api";
import { handoffTerminal, type RemoteHandoffRecord } from "./remoteHandoff";

let records: RemoteHandoffRecord[] = [];
const listeners = new Set<() => void>();
let timer: ReturnType<typeof setInterval> | undefined;
let loading: Promise<void> | null = null;
export function refreshHandoffs(): Promise<void> {
  if (loading) return loading;
  loading = (async () => {
    const next = await api.listRemoteHandoffs();
    for (let index = 0; index < next.length; index++) {
      const record = next[index];
      if (!record || record.result.outcome || handoffTerminal(record)) continue;
      if (!["running", "startUncertain", "adopted"].includes(record.stage)) continue;
      try { next[index] = await api.pollRemoteHandoff(record.id); } catch { /* retry on next tick */ }
    }
    records = next;
    listeners.forEach((listener) => listener());
  })().finally(() => { loading = null; });
  return loading;
}
function tick() {
  if (document.visibilityState !== "hidden") void refreshHandoffs().catch(() => undefined);
}
function subscribe(listener: () => void) {
  listeners.add(listener);
  if (listeners.size === 1) {
    tick(); timer = setInterval(tick, 10000);
    document.addEventListener("visibilitychange", tick);
  }
  return () => {
    listeners.delete(listener);
    if (!listeners.size) {
      clearInterval(timer); timer = undefined;
      document.removeEventListener("visibilitychange", tick);
    }
  };
}
export function useHandoffRecords() {
  return useSyncExternalStore(subscribe, () => records);
}
