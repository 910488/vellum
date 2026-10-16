import { formatDateTime } from "@/i18n/format";
import { api } from "@/lib/api";
import { RemoteWorkSurface } from "@/components/RemoteWorkSurface";
import { Btn, Notice, Row, Rows, State } from "@/components/ui";
import { elapsedLabel, convergePairingSuggested } from "@/lib/remoteVocabulary";
import type { RemoteController } from './useRemoteController';
export function RemoteHostWork({ model }: {
  model: RemoteController;
}) {
  const {
    t,
    locale,
    selectedHostId,
    sessions,
    busy,
    failure,
    probingHostId,
    lastUpdated,
    chatgptLogin,
    pairings,
    setTakeoverPrompt,
    setPending,
    sshTrust,
    operationStartedAt,
    host,
    operationRunning,
    verdict,
    blockers,
    ownOperation,
    workSurface,
    workRemedyDisabled,
    runWorkRemedy,
    run,
    confirmSshTrust,
    pairChatGpt,
    waitControls,
  } = model;
  return <><div className="worktop">
    <div className="worktop__head">
      <div>
        <h3 className="worktop__name">{host?.displayName}</h3>
        <p className="worktop__where">{host?.sshAlias}</p>
        {sessions?.threads.length ? <p className="worktop__where">{t("remote.summary.threads", { count: sessions.threads.length })}</p> : null}
      </div>
      <div className="worktop__head-status">
        <State tone={verdict.tone} label={t(verdict.stateKey)} />
        {selectedHostId && probingHostId === selectedHostId ? (<p className="worktop__updated worktop__updated--live">{t("remote.updating")}</p>) : selectedHostId && lastUpdated[selectedHostId] ? (<p className="worktop__updated">
          {t("remote.lastUpdated", {
            when: formatDateTime(new Date(lastUpdated[selectedHostId]), locale, { hour: "numeric", minute: "2-digit", second: "2-digit" }),
          })}
        </p>) : null}
      </div>
    </div>

    <RemoteWorkSurface model={workSurface} disabled={!selectedHostId || busy !== null || operationRunning || sshTrust?.hostId !== selectedHostId || !sshTrust?.confirmed} remedyDisabled={workRemedyDisabled} onDeploy={() => setPending("bootstrap")} onRemedy={runWorkRemedy} elapsed={ownOperation?.state === "running" ? t("remote.operation.elapsed", {
      clock: elapsedLabel(operationStartedAt.current.get(ownOperation.operationId) ?? Date.now(), Date.now()),
    }) : undefined} />

    {/* 進行中／失敗都原地取代按鈕。因與果同一格。 */}
    {ownOperation?.state === "completed" && convergePairingSuggested(ownOperation.result) ? (<Notice tone="info" acts={<Btn mini soft onClick={() => pairChatGpt()} disabled={busy !== null || Boolean(chatgptLogin) || !pairings.some((row) => row.isDesktopDefault)}>
      {t("remote.act.chatgptPair")}
    </Btn>}>
      {t("remote.account.optionalAfterDeploy")}
    </Notice>) : null}
    {sshTrust?.hostId === selectedHostId && sshTrust.checking ? (<Notice>{t("remote.trust.checking")}</Notice>) : sshTrust?.hostId === selectedHostId && sshTrust.confirmed && sshTrust.source === "userKnownHosts" ? (<Notice><strong>{t("remoteAdd.trustReused")}</strong><p>{t("remoteAdd.trustReusedHint")}</p>{sshTrust.fingerprint ? <code>{sshTrust.fingerprint.fingerprint}</code> : null}</Notice>) : sshTrust?.hostId === selectedHostId && !sshTrust.confirmed ? (<div className="worktop__trust">
      <p className="worktop__verdict">{t("remote.trust.title")}</p>
      <Notice tone="warn" raw={sshTrust.fetchError}>
        {sshTrust.fetchError ? t("remote.trust.fetchFailed") : t("remote.trust.explain")}
      </Notice>
      {sshTrust.fingerprint ? (<>
        <Rows>
          <Row label={t("remote.trust.factHost")}>{`${sshTrust.fingerprint.host}:${sshTrust.fingerprint.port}`}</Row>
          <Row label={t("remote.trust.factFingerprint")}><code>{sshTrust.fingerprint.fingerprint}</code></Row>
          <Row label={t("remote.trust.factCrossCheck")}>{t("remote.trust.crossCheckHint")}</Row>
        </Rows>
        <div className="worktop__acts">
          <Btn onClick={confirmSshTrust} disabled={busy !== null}>{t("remote.trust.confirm")}</Btn>
        </div>
      </>) : null}
    </div>) : ownOperation && ownOperation.state === "running" ? (<div className="worktop__run">
      {waitControls.cancel ? <div className="worktop__acts">
        {waitControls.token ? <Btn disabled={busy !== null} onClick={() => {
          if (selectedHostId && waitControls.token)
            setTakeoverPrompt({ hostId: selectedHostId, operationId: ownOperation.operationId, token: waitControls.token });
        }}>{t("remote.act.takeoverNative")}</Btn> : null}
        <Btn soft disabled={busy !== null} onClick={() => {
          if (selectedHostId)
            void run("remote.act.cancelDeployment", () => api.cancelRemoteDeployment(selectedHostId, ownOperation.operationId));
        }}>{t("remote.act.cancelDeployment")}</Btn>
      </div> : null}
    </div>) : ownOperation?.state === "cancelled" ? <Notice>{t("remote.operation.cancelled")}</Notice> : null}

  </div>

    {failure || ownOperation?.state === "failed" || blockers.length ? <details className="remote-work-details">
      <summary>{t("remoteWork.details")}</summary>
      <pre>{failure?.raw ?? (ownOperation?.state === "failed" ? ownOperation.message : null) ?? blockers.map((blocker) => blocker.code).join("\n")}</pre>
    </details> : null}
  </>;
}
