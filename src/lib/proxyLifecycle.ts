import type { ProxyStatus } from "@/types";

/** Must match `model::PROXY_LIFECYCLE_EVENT` in the Desktop host. */
export const PROXY_LIFECYCLE_EVENT = "proxy://lifecycle";

/** Apply an event or command result only when it is not from a superseded generation. */
export function shouldApplyProxyLifecycle(
  current: ProxyStatus | null | undefined,
  incoming: ProxyStatus,
): boolean {
  return (incoming.generation ?? 0) >= (current?.generation ?? 0);
}
