import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "@/lib/api";
import type { RemoteModelOption, RemoteOperationProgress } from "@/types";
import { Cap, Card, Notice } from "./ui";

/** A read-only catalogue; a checkbox submits one backend-controlled converge. */
export function RemoteModels({ hostId, disabled, revision, onOperation, onCatalog }: {
  hostId: string; disabled: boolean; revision: number;
  onOperation: (operation: RemoteOperationProgress) => void;
  onCatalog: (value: { hostId: string; models: RemoteModelOption[] }) => void;
}) {
  const { t } = useTranslation();
  const [models, setModels] = useState<RemoteModelOption[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [loading, setLoading] = useState(true);
  const inFlight = useRef(false);
  useEffect(() => {
    let active = true;
    setLoading(true);
    setError(null);
    api.listRemoteHostModels(hostId).then((next) => { if (active) { setModels(next); onCatalog({ hostId, models: next }); } })
      .catch((cause) => { if (active) setError(String(cause)); })
      .finally(() => { if (active) setLoading(false); });
    return () => { active = false; };
  }, [hostId, revision, onCatalog]);
  async function change(id: string, checked: boolean) {
    if (inFlight.current || disabled) return;
    const ids = models.filter((model) => model.catalogId === id ? checked : model.selected).map((model) => model.catalogId);
    inFlight.current = true;
    setSaving(true); setError(null);
    try {
      const operation = await api.updateRemoteHostModels(hostId, ids);
      setModels((current) => current.map((model) => ({ ...model, selected: ids.includes(model.catalogId) })));
      onOperation(operation);
    } catch (cause) { setError(String(cause)); }
    finally { inFlight.current = false; setSaving(false); }
  }
  return <Card>
    <Cap>{t("remoteModels.title")}</Cap>
    <p className="note">{t("remoteModels.hint")}</p>
    {error ? <Notice tone="error" raw={error}>{t("remoteModels.failed")}</Notice> : null}
    {loading ? <p>{t("common.loading")}</p> : !models.length ? <p>{t("remoteModels.empty")}</p> : <div className="remote-models">
      {models.map((model) => <label key={model.catalogId} className="remote-model">
        <input type="checkbox" checked={model.selected} disabled={disabled || saving || loading || Boolean(error) || model.mandatory}
          onChange={(event) => void change(model.catalogId, event.target.checked)} />
        <span><b>{model.displayName}</b><small>{model.routeId}{model.mandatory ? ` · ${t("remoteModels.mandatory")}` : ""}</small></span>
      </label>)}
    </div>}
  </Card>;
}
