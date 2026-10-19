import { operationalErrorKey } from "@/lib/operationalError";
import { Btn, Cap, Card, Notice, State } from "@/components/ui";
import type { RemoteController } from './useRemoteController';
export function RemoteAccountCatalog({ model }: {
  model: RemoteController;
}) {
  const {
    t,
    selectedHostId,
    modelObservation,
    busy,
    chatgptLogin,
    pairings,
    pairChatGpt,
    pairAllChatGptAccounts,
  } = model;
  return <>{modelObservation && modelObservation.hostId === selectedHostId && modelObservation.models.some((model) => model.mandatory && model.selected) ? <Card>
    <div className="rowline">
      <Cap>{t("remote.account.title")}</Cap>
      <span className="rows__hint">{t("remote.account.catalogHint")}</span>
    </div>
    {pairings.length ? (<div className="accounts">
      {pairings.map((row, index) => (<div className="accounts__row" key={row.accountId}>
        <span className="accounts__who">
          <b title={row.accountId}>{row.email ?? t("tabPolish.account", { index: index + 1 })}</b>
          {row.workspaceName ? (<small className="rows__hint">{row.workspaceName}</small>) : null}
          {row.isDesktopDefault ? (<small className="rows__hint">{t("remote.account.desktopCurrent")}</small>) : null}
          {row.detail ? <Notice tone="warn" raw={row.detail}>{t(operationalErrorKey(row.detail))}</Notice> : null}
        </span>
        <State tone={row.detail ? "quiet" : row.credentialState === "expired" ? "warn" : row.paired || row.active ? "ok" : "quiet"} label={row.detail
          ? t("remote.account.pairingUnknown")
          : row.credentialState === "expired"
            ? t("remote.account.credentialExpired")
            : row.paired || row.active
              ? t("remote.account.credentialReady")
              : t("remote.account.credentialMissing")} />
        <span className="accounts__act">
          {row.detail || ((row.paired || row.active) && row.credentialState !== "expired") ? null : (<Btn mini soft onClick={() => pairChatGpt(row.accountId, false)} disabled={busy !== null || Boolean(chatgptLogin)}>
            {row.credentialState === "expired"
              ? t("remote.act.chatgptReauthenticate")
              : t("remote.act.chatgptAuthorize")}
          </Btn>)}
        </span>
      </div>))}
    </div>) : null}
    <div className="remote-actions">
      {pairings.some((row) => ((!row.paired && !row.active) || row.credentialState === "expired") && !row.detail) ? (<Btn soft onClick={pairAllChatGptAccounts} disabled={busy !== null || Boolean(chatgptLogin)}>
        {t("remote.act.chatgptPairAll")}
      </Btn>) : null}
    </div>
  </Card> : null}

  </>;
}
