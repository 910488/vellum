import { handoffTerminal } from "@/lib/remoteHandoff";
import { hostVerdict } from "@/lib/remoteVocabulary";
import type { RemoteController } from './useRemoteController';
export function RemoteHostList({ model }: {
  model: RemoteController;
}) {
  const {
    t,
    handoffs,
    hosts,
    statuses,
    selectedHostId,
    setSelectedHostId,
    setFailure,
    probeErrors,
    probingHostId,
  } = model;
  return <><aside className="remote-hosts" aria-label={t("remote.hostsAria")}>
    {hosts.map((item) => {
      const itemStatus = statuses[item.vellumHostId];
      const unreachable = Boolean(probeErrors[item.vellumHostId]);
      const refreshing = probingHostId === item.vellumHostId && Boolean(itemStatus);
      const label = unreachable
        ? t("remote.probeFailed")
        : itemStatus
          ? t(hostVerdict({
            managerState: itemStatus.managerState,
            agentReachable: Boolean(itemStatus.agent),
          }).stateKey)
          : probingHostId === item.vellumHostId
            ? t("remote.probing")
            : item.validated
              ? t("remote.notProbed")
              : t("remote.sshInvalid");
      return (<button type="button" key={item.vellumHostId} className={`remote-host${selectedHostId === item.vellumHostId ? " remote-host--active" : ""}${unreachable ? " remote-host--unreachable" : ""}`} onClick={() => { setSelectedHostId(item.vellumHostId); setFailure(null); }}>
        <b>{item.displayName}</b>
        <span>{item.user ? `${item.user}@` : ""}{item.hostname ?? item.sshAlias}</span>
        <small>
          {label}
          {refreshing ? <em className="remote-host__refreshing"> · {t("remote.updating")}</em> : null}
        </small>
        {handoffs.some((r) => r.hostId === item.vellumHostId && !r.result.outcome) ? <small>{t("handoff.hostCounts", {
          running: handoffs.filter((r) => r.hostId === item.vellumHostId && !r.result.outcome && !handoffTerminal(r)).length,
          ready: handoffs.filter((r) => r.hostId === item.vellumHostId && !r.result.outcome && handoffTerminal(r)).length,
        })}</small> : null}
      </button>);
    })}
  </aside></>;
}
