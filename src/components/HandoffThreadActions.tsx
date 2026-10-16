import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "@/lib/api";
import type { RemoteHandoffRecord } from "@/lib/remoteHandoff";
import { Btn, Notice } from "./ui";

/** The backend owns link construction and opening; clipboard remains secondary. */
export function HandoffThreadActions({ record }: { record: RemoteHandoffRecord }) {
  const { t } = useTranslation();
  const threadId = record.remote?.threadId;
  const [target, setTarget] = useState<{ id: string; thread: string; available: boolean } | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    let live = true;
    setTarget(null); setFailure(null);
    if (threadId) void Promise.resolve().then(() => api.getRemoteHandoffThreadLink(record.id)).then((link) => {
      if (live) setTarget({ id: record.id, thread: threadId, available: !!link });
    }).catch(() => { if (live) setFailure("handoff.threadLinkFailed"); });
    return () => { live = false; };
  }, [record, threadId]);
  const current = target?.id === record.id && target.thread === threadId ? target : null;
  if (!threadId) return null;
  async function open() {
    setBusy(true); setFailure(null);
    try { await api.openRemoteHandoffThread(record.id); }
    catch { setFailure("handoff.openThreadFailed"); }
    finally { setBusy(false); }
  }
  async function copy() {
    try { await navigator.clipboard.writeText(threadId ?? ""); }
    catch { setFailure("handoff.copyFailed"); }
  }
  return <div>
    <div style={{ display: "flex", flexWrap: "wrap", gap: 8 }}>
      {current?.available ? <Btn mini disabled={busy} onClick={() => void open()}>{t("handoff.openCodexApp")}</Btn> : null}
      <Btn mini soft onClick={() => void copy()}>{t("handoff.copyThread")}</Btn>
    </div>
    {current && !current.available ? <p className="note">{t("handoff.addHostToCodex")}</p> : null}
    {failure ? <Notice tone="warn">{t(failure)}</Notice> : null}
  </div>;
}
