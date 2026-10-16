import { RemoteModels } from "@/components/RemoteModels";
import { RemoteHandoff } from "@/components/RemoteHandoff";
import { RemoteHostAdd, RemoteHostLoginGuide } from "@/components/RemoteHostAdd";
import { Btn, Card, Empty, Notice } from "@/components/ui";
import { useRemoteController } from './remote/useRemoteController';
import type { RemoteProps } from './remote/useRemoteState';
import { RemoteHostList } from './remote/RemoteHostList';
import { RemoteHostWork } from './remote/RemoteHostWork';
import { RemoteAccountCatalog } from './remote/RemoteAccountCatalog';
import { RemoteAdvancedAccounts } from './remote/RemoteAdvancedAccounts';
import { RemoteHostDetails } from './remote/RemoteHostDetails';
import { RemoteThreadDetails } from './remote/RemoteThreadDetails';
import { RemoteConfirmations } from './remote/RemoteConfirmations';
export function Remote(props: RemoteProps) {
  const model = useRemoteController(props);
  const {
    t,
    hosts,
    setHosts,
    addedHost,
    setAddedHost,
    selectedHostId,
    setSelectedHostId,
    setModelObservation,
    busy,
    discoveryError,
    featureEnabled,
    discovering,
    probeVersion,
    setPending,
    legacyBrokerHosts,
    trackOperation,
    load,
    host,
    status,
    operationRunning,
    strayOperation,
    active,
    sshTrust,
    setFailure,
    operation,
  } = model;
  function rescan() {
    // A deliberate refresh dismisses the old failure, not the persisted journal.
    // A later deployment still runs the full backend preflight.
    setFailure(null);
    if (operation?.state === "failed" || operation?.state === "cancelled") trackOperation(null);
    void load();
  }
  return (<>
    <div className="canvas__head">
      <div>
        <h2 className="canvas__title">{t("navigation.remote.label")}</h2>
        <p className="note">{t("remote.blurb")}</p>
      </div>
      <Btn soft onClick={rescan} disabled={busy !== null || discovering}>
        {discovering ? t("remote.rescanning") : t("remote.rescan")}
      </Btn>
    </div>

    {legacyBrokerHosts.length ? (<Notice tone="warn" raw={legacyBrokerHosts.join(t("common.itemSeparator"))}>
      {t("remote.legacyBrokerUnsupported")}
    </Notice>) : null}
    {discoveryError ? <Notice tone="error" raw={discoveryError}>{t("remote.error.discovery")}</Notice> : null}
    {addedHost && selectedHostId === addedHost.candidate.vellumHostId ? <RemoteHostLoginGuide host={addedHost} /> : null}
    {discovering ? <Notice>{t("remote.discovering")}</Notice> : null}
    {strayOperation ? (<Notice acts={<Btn mini soft onClick={() => setSelectedHostId(strayOperation.hostId)}>{t("remote.goToHost")}</Btn>}>
      {t("remote.otherHostBusy", {
        host: hosts.find((item) => item.vellumHostId === strayOperation.hostId)?.displayName ?? strayOperation.hostId,
      })}
    </Notice>) : null}

    {featureEnabled === false ? (<Card><Empty>{t("remote.featureDisabled")}</Empty></Card>) : !hosts.length ? (discovering ? null : <RemoteHostAdd onAdded={(added) => {
      setAddedHost(added);
      setHosts((rows) => [...rows.filter((r) => r.vellumHostId !== added.candidate.vellumHostId), added.candidate]);
      setSelectedHostId(added.candidate.vellumHostId);
    }} />) : (<div className="remote-layout">
      <RemoteHostList model={model} />


      <div className="remote-detail">
        {/* ---------- 工作面 ---------- */}
        <RemoteHostWork model={model} />
        {/* ---------- ChatGPT account catalog ---------- */}
        {selectedHostId && host ? <RemoteHandoff key={`handoff-${selectedHostId}`} hostId={selectedHostId} hostName={host.displayName} active={active} /> : null}

        {selectedHostId && status?.agent && status.managerState !== "unmanaged" ? <RemoteModels key={`models-${selectedHostId}`} hostId={selectedHostId} disabled={busy !== null || operationRunning || sshTrust?.hostId !== selectedHostId || !sshTrust?.confirmed} revision={probeVersion} onOperation={trackOperation} onCatalog={setModelObservation} /> : null}
        <RemoteAccountCatalog model={model} />
        <details className="remote-advanced">
          <summary>{t("remoteWork.advanced")}</summary>
          <RemoteAdvancedAccounts model={model} />
          <RemoteHostDetails model={model} />
          <RemoteThreadDetails model={model} />


        </details>

        {/* ---------- 危險區 ----------
                「一鍵還原」在 Windows 語境聽起來是救援、是安全的，但它其實是
                拆除。名字要講出後果，而且不能跟「匯出診斷包」擺在同一排 ——
                肌肉記憶會按錯。 */}
        <div className="remote-danger">
          <b>{t("tabPolish.restoreHost")}</b>
          <p>{t("remote.danger.body")}</p>
          <Btn danger onClick={() => setPending("restore")} disabled={!selectedHostId || busy !== null || operationRunning}>
            {t("remote.act.restore")}
          </Btn>
        </div>
      </div>
    </div>)}

    <RemoteConfirmations model={model} />

  </>);
}
