import { api } from "@/lib/api";
import { Btn, Cap, Card, Notice, Row, Rows } from "@/components/ui";
import { matchesRemoteControlReady } from './remoteHelpers';
import type { RemoteController } from './useRemoteController';
export function RemoteAdvancedAccounts({ model }: {
  model: RemoteController;
}) {
  const {
    t,
    selectedHostId,
    busy,
    chatgptLogin,
    pairings,
    executionAccounts,
    executionLogin,
    executionDisplayName,
    setExecutionDisplayName,
    devicePairing,
    load,
    grok,
    chatgpt,
    chatgptStateLabel,
    activeControlAccount,
    selectedExecutionAccount,
    run,
    pairChatGpt,
    selectRemoteControlAccount,
    addOfficialExecutionAccount,
    selectOfficialExecutionRoute,
    removeOfficialExecutionAccount,
    pairMobileDevice,
  } = model;
  return <>{/* ---------- Remote control role binding ---------- */}
    <Card>
      <Cap>{t("remote.control.title")}</Cap>
      <p className="remote-mobile-empty">{t("remote.control.sameAccountHint")}</p>
      <Rows>
        <Row label={t("remote.control.identity")}>
          <select className="input" value={activeControlAccount?.accountId ?? ""} onChange={(event) => selectRemoteControlAccount(event.target.value)} disabled={busy !== null || Boolean(chatgptLogin) || !pairings.length}>
            <option value="" disabled>{t("remote.control.choose")}</option>
            {pairings.map((account) => (<option key={account.accountId} value={account.accountId}>
              {account.email ?? account.accountId}
              {account.workspaceName ? ` · ${account.workspaceName}` : ""}
              {account.credentialState === "expired" ? ` · ${t("remote.account.credentialExpired")}` : ""}
              {!account.paired && !account.active ? ` · ${t("remote.account.credentialMissing")}` : ""}
            </option>))}
          </select>
        </Row>
        <Row label={t("remote.control.status")}>{chatgptStateLabel}</Row>
      </Rows>
      {chatgpt?.state === "reauthenticationRequired" ? (<Notice tone="warn" acts={activeControlAccount ? (<Btn mini soft onClick={() => pairChatGpt(activeControlAccount.accountId)} disabled={busy !== null || Boolean(chatgptLogin)}>
        {t("remote.act.chatgptReauthenticate")}
      </Btn>) : null}>
        {t("remote.control.credentialExpired")}
      </Notice>) : null}
      {devicePairing ? (<Notice>
        {t("remote.control.deviceHint")}
        {devicePairing.verificationUrl || devicePairing.uri ? (<>
          {" "}
          <a href={devicePairing.verificationUrl ?? devicePairing.uri} target="_blank" rel="noreferrer">
            {devicePairing.verificationUrl ?? devicePairing.uri}
          </a>
        </>) : null}
        {devicePairing.pairingCode || devicePairing.code || devicePairing.userCode ? (<>{" · "}<strong>{devicePairing.pairingCode ?? devicePairing.code ?? devicePairing.userCode}</strong></>) : null}
        {devicePairing.expiresAt ? <span className="remote-mobile-expiry"> · {t("remote.control.expiresAt", { when: devicePairing.expiresAt })}</span> : null}
      </Notice>) : null}
      <div className="remote-actions">
        <Btn soft onClick={pairMobileDevice} disabled={busy !== null || !matchesRemoteControlReady(chatgpt?.state)}>{t("remote.act.devicePair")}</Btn>
      </div>
    </Card>

    {/* ---------- Official model execution role binding ---------- */}
    <Card>
      <Cap>{t("remote.execution.title")}</Cap>
      <p className="remote-mobile-empty">{t("remote.execution.independentHint")}</p>
      <Rows>
        <Row label={t("remote.execution.identity")}>
          <select className="input" value={selectedExecutionAccount?.accountIdHash ?? "control"} onChange={(event) => selectOfficialExecutionRoute(event.target.value)} disabled={busy !== null}>
            <option value="control">{t("remote.execution.followControl")}</option>
            {executionAccounts.map((account) => (<option key={account.accountIdHash} value={account.accountIdHash}>{account.displayName}</option>))}
          </select>
        </Row>
      </Rows>
      {!selectedExecutionAccount && chatgpt?.credentialState === "expired" ? (<Notice tone="warn">{t("remote.execution.inheritedCredentialExpired")}</Notice>) : null}
      {executionLogin ? (<Notice>
        {t("remote.execution.loginHint")}
        {" "}
        <a href={executionLogin.verificationUrl} target="_blank" rel="noreferrer">{executionLogin.verificationUrl}</a>
        {" · "}
        <strong>{executionLogin.userCode}</strong>
      </Notice>) : null}
      <div className="remote-actions">
        <input className="input remote-mobile-name-input" type="text" placeholder={t("remote.execution.namePlaceholder")} value={executionDisplayName} onChange={(event) => setExecutionDisplayName(event.target.value)} disabled={busy !== null || Boolean(executionLogin)} />
        <Btn soft onClick={addOfficialExecutionAccount} disabled={busy !== null || Boolean(executionLogin) || !executionDisplayName.trim()}>
          {t("remote.act.executionLogin")}
        </Btn>
        {selectedExecutionAccount ? (<Btn soft onClick={() => removeOfficialExecutionAccount(selectedExecutionAccount.accountIdHash)} disabled={busy !== null}>
          {t("remote.execution.removeSelected")}
        </Btn>) : null}
      </div>
    </Card>

    <Card>
      <Cap>Grok</Cap>
      <Rows>
        <Row label={t("remote.execution.identity")}>
          {grok?.detachedQualified
            ? t("remote.account.grokReady", { account: grok.account ?? "" })
            : grok?.configured
              ? t("remote.account.grokPartial")
              : t("remote.account.grokAbsent")}
        </Row>
      </Rows>
      {grok?.loginPending ? (<Notice>
        {t("remote.account.grokDeviceLogin")} {grok.verificationUri
          ? <a href={grok.verificationUri} target="_blank" rel="noreferrer">{grok.verificationUri}</a>
          : t("remote.account.grokWaitingUrl")}
        {grok.userCode ? <> · <strong>{grok.userCode}</strong></> : null}
      </Notice>) : null}
      <div className="remote-actions">
        {grok?.loginPending ? (<Btn soft onClick={() => selectedHostId && void run("remote.act.grokCancel", async () => { await api.cancelRemoteGrokLogin(selectedHostId); await load(); })} disabled={busy !== null}>{t("remote.act.grokCancel")}</Btn>) : (<Btn soft onClick={() => selectedHostId && void run("remote.act.grokLogin", async () => { await api.startRemoteGrokLogin(selectedHostId); await load(); })} disabled={!selectedHostId || busy !== null}>
          {busy === "remote.act.grokLogin" ? <><span className="remote-login-spinner" aria-hidden="true" />{t("remote.account.grokStarting")}</> : t("remote.act.grokLogin")}
        </Btn>)}
        {grok?.configured ? (<Btn soft onClick={() => selectedHostId && void run("remote.act.grokRefresh", async () => { await api.refreshRemoteGrokLogin(selectedHostId); await load(); })} disabled={busy !== null}>{t("remote.act.grokRefresh")}</Btn>) : null}
      </div>
    </Card>

  </>;
}
