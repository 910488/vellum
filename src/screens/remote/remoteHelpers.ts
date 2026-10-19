export function formatBytes(value: number | null | undefined, unknown: string): string {
  if (value == null) return unknown;
  const units = ["B", "KB", "MB", "GB", "TB"];
  let index = 0;
  let current = value;
  while (current >= 1024 && index < units.length - 1) {
    current /= 1024;
    index += 1;
  }
  return `${current.toFixed(current >= 100 ? 0 : 1)} ${units[index]}`;
}
export function matchesRemoteControlReady(state: string | null | undefined): boolean {
  return state === "ready" || state === "synchronized";
}
export type PendingKind = "bootstrap" | "restore" | "restartNative" | "installCodex" | "updateAgent" | "syncDesktopCodex" | "stopAppOwned";
