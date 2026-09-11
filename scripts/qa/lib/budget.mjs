/** Live token budgets: input + output, whole round, including subagents/review/compaction/retries. */
export const LIVE_TOKEN_BUDGETS = Object.freeze({
  "opencode-mimo-2.5": 200_000,
  "grok-4.6": 100_000,
  qwen: Number.POSITIVE_INFINITY,
});

export const CASE_TIMEOUT_MS = 10 * 60 * 1000;
export const LIVE_STAGE_TIMEOUT_MS = 90 * 60 * 1000;

export function conservativeTokens(usage) {
  if (usage == null) return 0;
  if (typeof usage === "number" && Number.isFinite(usage)) return Math.ceil(usage);
  if (usage.total != null && Number.isFinite(usage.total)) return Math.ceil(usage.total);
  const input = Number(usage.input) || 0;
  const output = Number(usage.output) || 0;
  if (input || output) return Math.ceil(input + output);
  if (usage.estimate != null && Number.isFinite(usage.estimate)) {
    return Math.ceil(usage.estimate);
  }
  // Missing usage: conservative placeholder so a silent provider cannot starve the ledger.
  if (usage.missing) return Math.ceil(Number(usage.missingEstimate) || 8_192);
  return 0;
}

export function createBudgetTracker(limits = LIVE_TOKEN_BUDGETS) {
  const used = Object.create(null);
  const stopped = Object.create(null);
  const reserved = Object.create(null);

  function remaining(model) {
    const cap = limits[model];
    if (cap == null) return 0;
    if (!Number.isFinite(cap)) return Number.POSITIVE_INFINITY;
    return cap - (used[model] ?? 0);
  }

  function markCap(model) {
    const cap = limits[model];
    if (Number.isFinite(cap) && (used[model] ?? 0) >= cap) stopped[model] = true;
  }

  return {
    limits,
    used() {
      return { ...used };
    },
    remaining,
    canStart(model, estimate = 0) {
      if (stopped[model]) return false;
      const need = conservativeTokens(estimate);
      const left = remaining(model);
      return left >= need;
    },
    reserve(model, estimate = 0) {
      if (!this.canStart(model, estimate)) return false;
      const need = conservativeTokens(estimate);
      used[model] = (used[model] ?? 0) + need;
      reserved[model] = (reserved[model] ?? 0) + need;
      markCap(model);
      return true;
    },
    release(model, estimate = 0) {
      const need = conservativeTokens(estimate);
      used[model] = Math.max(0, (used[model] ?? 0) - need);
      reserved[model] = Math.max(0, (reserved[model] ?? 0) - need);
      return used[model];
    },
    settle(model, usage, estimate = 0) {
      const reservedAmt = conservativeTokens(estimate);
      const actual =
        usage == null || usage.missing
          ? conservativeTokens({ missing: true, missingEstimate: 8_192 })
          : conservativeTokens(usage);
      used[model] = Math.max(0, (used[model] ?? 0) - reservedAmt + actual);
      reserved[model] = Math.max(0, (reserved[model] ?? 0) - reservedAmt);
      markCap(model);
      return used[model];
    },
    record(model, usage) {
      const add = conservativeTokens(usage);
      used[model] = (used[model] ?? 0) + add;
      markCap(model);
      return used[model];
    },
    stop(model, reason = "budget-stop") {
      stopped[model] = reason;
    },
    isStopped(model) {
      return Boolean(stopped[model]);
    },
    snapshot() {
      const models = {};
      for (const model of Object.keys(limits)) {
        const cap = limits[model];
        const left = remaining(model);
        models[model] = {
          limit: Number.isFinite(cap) ? cap : "unlimited",
          used: used[model] ?? 0,
          remaining: Number.isFinite(left) ? left : "unlimited",
          stopped: stopped[model] || false,
        };
      }
      return models;
    },
  };
}
