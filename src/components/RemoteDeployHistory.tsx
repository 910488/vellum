import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "@/lib/api";
import { describeBlocker } from "@/lib/remoteVocabulary";
import { operationalErrorKey } from "@/lib/operationalError";
import type { RemoteDeploySummary } from "@/types";

/** Local evidence: remains available when SSH is down. No independent polling. */
export function RemoteDeployHistory({ hostId, revision }: { hostId: string; revision: number }) {
  const { t } = useTranslation();
  const [summary, setSummary] = useState<RemoteDeploySummary | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  useEffect(() => {
    let active = true;
    setSummary(null); setUnavailable(false);
    api.getRemoteDeploySummary(hostId).then((next) => { if (active) setSummary(next); })
      .catch(() => { if (active) setUnavailable(true); });
    return () => { active = false; };
  }, [hostId, revision]);
  if (unavailable) return <p className="remote-deploy-history note">{t("remoteHistory.unavailable")}</p>;
  if (!summary) return null;
  const code = summary.lastFailureCode;
  const reason = code === "DeploymentInterrupted" ? t("remoteHistory.interruptedReason")
    : code ? describeBlocker(code).messageKey ?? operationalErrorKey(code) : null;
  return <p className="remote-deploy-history note" aria-label={t("remoteHistory.label")}>
    {t("remoteHistory.counts", { count: summary.total, succeeded: summary.succeeded, failed: summary.failed, interrupted: summary.interrupted })}
    {" · "}{t("remoteHistory.blocked", { count: summary.blocked })}
    {code && reason ? <><br />{t("remoteHistory.lastFailure", { reason: code === "DeploymentInterrupted" ? reason : t(reason), code })}</> : null}
  </p>;
}
