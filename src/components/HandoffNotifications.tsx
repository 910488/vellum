import { useTranslation } from "react-i18next";
import { api } from "@/lib/api";
import { refreshHandoffs, useHandoffRecords } from "@/lib/handoffStore";
import { handoffTerminal } from "@/lib/remoteHandoff";
import { Btn, Notice } from "./ui";
import { HandoffThreadActions } from "./HandoffThreadActions";

/** Durable completion inbox. Reopening Vellum discovers missed completions. */
export function HandoffNotifications({ onReview }: { onReview: (host: string, id: string) => void }) {
  const { t } = useTranslation();
  const records = useHandoffRecords().filter((record) => !record.result.acknowledged && !record.result.outcome && handoffTerminal(record));
  if (!records.length) return null;
  return <div aria-live="polite">{records.map((record) => <Notice key={record.id} acts={<>
    <Btn mini soft onClick={() => onReview(record.hostId, record.id)}>{t("handoff.review")}</Btn>
    <Btn mini soft onClick={() => void api.acknowledgeRemoteHandoff(record.id).then(refreshHandoffs).catch(() => undefined)}>{t("handoff.dismiss")}</Btn>
  </>}>
    {t(record.remote?.state === "completed" ? "handoff.completedNotice" : "handoff.partialNotice", { name: record.source.name })}
    {record.remote?.summary ? <p>{record.remote.summary}</p> : null}
    <HandoffThreadActions record={record} />
  </Notice>)}</div>;
}
