/** Pinned live models for this round. Silent substitution is a FAIL. */
export const PINNED_MODELS = Object.freeze({
  "opencode-mimo-2.5": Object.freeze({
    id: "opencode-mimo-2.5",
    provider: "opencode",
    catalogHints: ["mimo", "mimo-2.5"],
    budgetKey: "opencode-mimo-2.5",
  }),
  "grok-4.6": Object.freeze({
    id: "grok-4.6",
    provider: "grok",
    catalogHints: ["grok-4.6", "grok 4.6"],
    budgetKey: "grok-4.6",
  }),
  qwen: Object.freeze({
    id: "qwen",
    provider: "qwen",
    catalogHints: ["qwen"],
    budgetKey: "qwen",
  }),
});

export const DEFAULT_CASE_ESTIMATE = 8_192;

export function pinnedModel(id) {
  return PINNED_MODELS[id] ?? null;
}

export function assertPinnedModel(observed, expectedId) {
  const pin = pinnedModel(expectedId);
  if (!pin) {
    return { ok: false, reason: "wrong-model", detail: `unknown pinned model ${expectedId}` };
  }
  if (!observed) {
    return { ok: false, reason: "wrong-model", detail: `missing observed model; expected ${expectedId}` };
  }
  const text = String(observed).toLowerCase();
  const hit = pin.catalogHints.some((hint) => text.includes(hint.toLowerCase())) || text === pin.id;
  if (!hit) {
    return {
      ok: false,
      reason: "wrong-model",
      detail: `observed ${observed} does not match pinned ${expectedId}; silent substitution is forbidden`,
    };
  }
  return { ok: true, pin };
}

/** Expand matrix cases that list `models` into one case per pinned model. */
export function expandModels(cases) {
  const out = [];
  for (const item of cases) {
    const listed = Array.isArray(item.models) && item.models.length ? item.models : null;
    if (!listed) {
      out.push(item);
      continue;
    }
    for (const model of listed) {
      const pin = pinnedModel(model);
      if (!pin) {
        out.push({
          ...item,
          id: `${item.id}::${model}`,
          model,
          estimate: item.estimate ?? DEFAULT_CASE_ESTIMATE,
          pinned: null,
          expansionError: `unpinned model ${model}`,
        });
        continue;
      }
      out.push({
        ...item,
        id: listed.length > 1 ? `${item.id}::${model}` : item.id,
        model: pin.id,
        estimate: item.estimate ?? DEFAULT_CASE_ESTIMATE,
        pinned: pin,
        parentId: item.id,
      });
    }
  }
  return out;
}
