import { Cap, Card, Empty, State } from "@/components/ui";
import type { RemoteController } from './useRemoteController';
export function RemoteThreadDetails({ model }: {
  model: RemoteController;
}) {
  const { t, sessions, unknown } = model;
  return <><Card>
    <div className="remote-title-row">
      <div><Cap>{t("remote.sessions.cap")}</Cap><h3 className="remote-title">{t("remote.sessions.title")}</h3></div>
      <State tone={sessions?.observability === "nativeAppServer" ? "ok" : sessions?.observability === "daemonDown" ? "quiet" : "warn"} label={sessions ? t(`remote.sessions.observability.${sessions.observability}`) : t("common.loading")} />
    </div>
    {!sessions ? null : !sessions.threads.length ? (<Empty>{sessions.observability === "nativeAppServer" ? t("remote.sessions.empty") : t("remote.sessions.unreadable")}</Empty>) : (<div className="remote-session-list">
      {sessions.threads.map((thread) => (<div key={thread.threadId} className="remote-session">
        <div><code>{thread.threadId}</code><State tone={thread.activeTurn ? "ok" : "quiet"} label={thread.status} /></div>
        <small>{t("remote.sessions.turns", { count: thread.turnCount, last: thread.lastTurnStatus ?? unknown })}{thread.activeTurnId ? ` · ${thread.activeTurnId}` : ""}</small>
      </div>))}
    </div>)}
  </Card></>;
}
