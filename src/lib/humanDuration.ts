export function humanDuration(ms: number, t: (key: string, options?: Record<string, unknown>) => string): string {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  if (seconds >= 3600) return t("tabPolish.time.hoursMinutes", { hours: Math.floor(seconds / 3600), minutes: Math.floor(seconds % 3600 / 60) });
  if (seconds >= 60) return t("tabPolish.time.minutesSeconds", { minutes: Math.floor(seconds / 60), seconds: seconds % 60 });
  return t("tabPolish.time.seconds", { seconds: seconds < 10 ? (ms / 1000).toFixed(1) : seconds });
}
