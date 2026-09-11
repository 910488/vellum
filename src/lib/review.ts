/**
 * Auto-review policy metadata and stats helpers.
 * Labels are translation keys; components call t().
 */
import type { ReviewPolicy, ReviewSettings, ReviewStats } from "@/types";

export const REVIEW_POLICIES: {
  value: ReviewPolicy;
  labelKey: string;
  blurbKey: string;
}[] = [
  {
    value: "always",
    labelKey: "review.policy.always.label",
    blurbKey: "review.policy.always.blurb",
  },
  {
    value: "failover",
    labelKey: "review.policy.failover.label",
    blurbKey: "review.policy.failover.blurb",
  },
];

export function policyOf(settings: ReviewSettings | null): ReviewPolicy {
  if (!settings) return "always";
  return settings.policy ?? "always";
}

export function policyMeta(policy: ReviewPolicy) {
  return REVIEW_POLICIES.find((p) => p.value === policy) ?? REVIEW_POLICIES[0]!;
}

export function needsFallback(policy: ReviewPolicy): boolean {
  return policy === "failover";
}

export function fallbackShare(stats: ReviewStats | null): number {
  if (!stats || stats.totalRuns <= 0) return 0;
  return Math.round((stats.fallbackRuns / stats.totalRuns) * 100);
}

export function statsToShow(
  stats: ReviewStats | null,
  keepRouteIds: (string | null | undefined)[],
  nowEpochSeconds = Math.floor(Date.now() / 1000),
) {
  if (!stats) return [];
  const keep = new Set(keepRouteIds.filter((id): id is string => Boolean(id)));
  const sevenDaysAgo = nowEpochSeconds - 7 * 86_400;
  return stats.providers
    .filter((p) => {
      const runs = p.primaryRuns + p.fallbackRuns + p.failedRuns;
      if (runs === 0) return keep.has(p.routeId);
      return p.lastUsedAt != null && p.lastUsedAt >= sevenDaysAgo;
    })
    .sort(
      (a, b) =>
        b.primaryRuns + b.fallbackRuns - (a.primaryRuns + a.fallbackRuns) ||
        a.provider.localeCompare(b.provider),
    );
}
