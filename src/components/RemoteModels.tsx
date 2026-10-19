import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "@/lib/api";
import type { RemoteModelOption, RemoteOperationProgress } from "@/types";
import { Btn, Cap, Card, Notice } from "./ui";

/**
 * A read-only catalogue with a local draft. Ticking boxes only edits the
 * draft; one "apply" submits one backend-controlled converge, so choosing
 * three models costs one deployment instead of three in a row.
 */
export function RemoteModels({ hostId, disabled, revision, onOperation, onCatalog }: {
  hostId: string; disabled: boolean; revision: number;
  onOperation: (operation: RemoteOperationProgress) => void;
  onCatalog: (value: { hostId: string; models: RemoteModelOption[] }) => void;
}) {
  const { t } = useTranslation();
  const [models, setModels] = useState<RemoteModelOption[]>([]);
  const [draft, setDraft] = useState<Set<string> | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [loading, setLoading] = useState(true);
  const inFlight = useRef(false);
  useEffect(() => {
    let active = true;
    setLoading(true);
    setError(null);
    setDraft(null);
    api.listRemoteHostModels(hostId).then((next) => { if (active) { setModels(next); onCatalog({ hostId, models: next }); } })
      .catch((cause) => { if (active) setError(String(cause)); })
      .finally(() => { if (active) setLoading(false); });
    return () => { active = false; };
  }, [hostId, revision, onCatalog]);
  const confirmed = new Set(models.filter((model) => model.selected).map((model) => model.catalogId));
  const chosen = draft ?? confirmed;
  const dirty = draft !== null && (draft.size !== confirmed.size || [...draft].some((id) => !confirmed.has(id)));
  function toggle(id: string, checked: boolean) {
    if (disabled || saving) return;
    const next = new Set(chosen);
    if (checked) next.add(id); else next.delete(id);
    setDraft(next);
  }
  async function apply() {
    if (inFlight.current || disabled || !dirty) return;
    const ids = models.filter((model) => chosen.has(model.catalogId)).map((model) => model.catalogId);
    inFlight.current = true;
    setSaving(true); setError(null);
    try {
      const operation = await api.updateRemoteHostModels(hostId, ids);
      setModels((current) => current.map((model) => ({ ...model, selected: ids.includes(model.catalogId) })));
      setDraft(null);
      onOperation(operation);
    } catch (cause) { setError(String(cause)); setDraft(null); }
    finally { inFlight.current = false; setSaving(false); }
  }
  return <Card>
    <Cap>{t("remoteModels.title")}</Cap>
    <p className="note">{t("remoteModels.hint")}</p>
    {error ? <Notice tone="error" raw={error}>{t("remoteModels.failed")}</Notice> : null}
    {loading ? <p>{t("common.loading")}</p> : !models.length ? <p>{t("remoteModels.empty")}</p> : <div className="remote-models">
      {models.map((model) => <label key={model.catalogId} className="remote-model">
        <input type="checkbox" checked={chosen.has(model.catalogId)} disabled={disabled || saving || loading || model.mandatory}
          onChange={(event) => toggle(model.catalogId, event.target.checked)} />
        <span><b>{model.displayName}</b><small>{model.routeId}{model.mandatory ? ` · ${t("remoteModels.mandatory")}` : ""}</small></span>
      </label>)}
    </div>}
    {dirty ? <div className="remote-actions">
      <Btn onClick={() => void apply()} disabled={disabled || saving}>{t(saving ? "remoteModels.applying" : "remoteModels.apply")}</Btn>
      <Btn soft onClick={() => setDraft(null)} disabled={saving}>{t("remoteModels.discard")}</Btn>
    </div> : null}
  </Card>;
}
