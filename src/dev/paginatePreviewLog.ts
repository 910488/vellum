import type { RequestLog } from "@/types";
export function paginatePreviewLog(log: RequestLog, args: { limit?: number; offset?: number; failedOnly?: boolean; routeId?: string; focusEntryId?: number } = {}): RequestLog {
  const entries = log.entries.filter((entry) => (!args.failedOnly || entry.status >= 400) && (!args.routeId || entry.routeId === args.routeId));
  const limit = Math.max(1, args.limit ?? 20);
  const focus = args.focusEntryId == null ? -1 : entries.findIndex((entry) => entry.id === args.focusEntryId);
  const offset = focus >= 0 ? Math.floor(focus / limit) * limit : Math.max(0, args.offset ?? 0);
  return { ...log, entries: entries.slice(offset, offset + limit), entryTotal: entries.length, entryOffset: offset,
    entryRoutes: [...new Map(log.entries.map((entry) => [entry.routeId, { routeId: entry.routeId, provider: entry.provider }])).values()] };
}
