import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Btn, Cap, Card, Confirm, Empty, Meter, Notice, Row, Rows, State } from "@/components/ui";
import { claudeCodeApi, type ClaudeCodeStatus } from "@/lib/claudeCode";
import { startVisiblePoll } from "@/lib/visiblePoll";

export function ClaudeCodeCard({ active }: { active: boolean }) {
  const { t } = useTranslation();
  const [status, setStatus] = useState<ClaudeCodeStatus | null>(null);
  const [project, setProject] = useState("");
  const [busy, setBusy] = useState(false);
  const [trustAccepted, setTrustAccepted] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [removeId, setRemoveId] = useState<string | null>(null);
  const revision = useRef(0);
  const mounted = useRef(false);
  const load = useCallback(async () => {
    const generation = revision.current;
    try {
      const next = await claudeCodeApi.status();
      if (!mounted.current || generation !== revision.current) return;
      setStatus(next);
      setProject((current) => current || next.project || "");
    } catch (cause) {
      if (mounted.current && generation === revision.current) setError(String(cause));
    }
  }, []);
  useEffect(() => {
    mounted.current = true;
    if (active) void load();
    const stop = startVisiblePoll({ active, intervalMs: 3000, load });
    return () => { mounted.current = false; revision.current++; stop(); };
  }, [active, load]);
  useEffect(() => setTrustAccepted(false), [status?.trust.sha256]);

  async function act(action: () => Promise<unknown>) {
    revision.current++;
    setBusy(true); setError(null);
    try { await action(); await load(); }
    catch (cause) { if (mounted.current) setError(String(cause)); }
    finally { if (mounted.current) setBusy(false); }
  }

  const state = !status ? "loading" : status.recoveryRequired ? "repair" : !status.running ? "stopped"
    : status.routing.lastError?.startsWith("client_tls") || status.routing.lastError === "client_ca_untrusted" ? "resume"
    : status.routing.records.some((record) => record.operation === "messages"
      && record.state === "completed" && record.upstreamStatus !== null
      && record.upstreamStatus >= 200 && record.upstreamStatus < 300) ? "connected" : "waiting";
  const label = (id: string | null) => status?.accounts.accounts.find((account) => account.id === id)?.label ?? (id || t("claudeCode.original"));
  return <Card>
    <Cap>Claude Desktop Code · {t("claudeCode.experimental")}</Cap>
    <p className="prose">{t("claudeCode.accountsHint")}</p>
    {status?.accounts.accounts.length ? <Rows className="official-account-list">
      {status.accounts.accounts.map((account) => {
        const selected = status.accounts.selected === account.id;
        return <Row key={account.id} label={account.label}>
          <span className="acct">
            <State tone={account.reauthorizeRequired ? "warn" : selected ? "ok" : "quiet"}
              label={account.reauthorizeRequired ? t("claudeCode.reauthorize") : selected ? t("claudeCode.selected") : t("models.ui.account.authenticated")} />
            <span className="acct__quota">
              {account.usage ? (["fiveHour", "sevenDay"] as const).map((key) => {
                const window = account.usage![key];
                if (!window) return null;
                const remaining = Math.max(0, Math.min(100, 100 - window.utilization));
                return <span className="acct__quota-window" key={key}>
                  <span className="acct__quota-line"><strong>{Math.round(remaining)}%</strong><Meter percent={remaining} tone={remaining <= 10 ? "coral" : "default"} /></span>
                  <small>{t(`claudeCode.${key}`)}{window.resetsAt ? ` · ${new Date(window.resetsAt).toLocaleString()}` : ""}</small>
                </span>;
              }) : <span className="field__hint">{t("claudeCode.usageNotLoaded")}</span>}
            </span>
            <span className="acct__resets">
              <Btn mini soft disabled={busy} onClick={() => void act(() => claudeCodeApi.usage(account.id))}>{t("claudeCode.refreshUsage")}</Btn>
              <Btn mini soft disabled={busy || selected || account.reauthorizeRequired} onClick={() => void act(() => claudeCodeApi.select(account.id))}>
                {t(selected ? "models.ui.account.current" : "models.ui.account.useThis")}
              </Btn>
            </span>
            <Btn mini soft disabled={busy || selected} onClick={() => setRemoveId(account.id)}>{t("common.remove")}</Btn>
          </span>
        </Row>;
      })}
    </Rows> : <Empty>{t(status ? "claudeCode.noAccounts" : "claudeCode.loading")}</Empty>}
    <div className="rowline" style={{ marginTop: 16 }}>
      <Btn disabled={busy || !status || status.accounts.loginPending} onClick={() => void act(() => claudeCodeApi.login())}>
        {t(status?.accounts.loginPending ? "models.ui.oauth.waitingBrowser" : "claudeCode.login")}
      </Btn>
      {status?.accounts.loginPending && <Btn soft disabled={busy} onClick={() => void act(() => claudeCodeApi.cancelLogin())}>{t("common.cancel")}</Btn>}
    </div>
    {status?.accounts.loginPending && <p role="status" className="prose">{t("claudeCode.loginPending")}</p>}
    <p className="field__hint">{t("claudeCode.switchHint")}</p>
    <details open={Boolean(status?.recoveryRequired || status?.trust.recoveryRequired)}>
    <summary>{t("claudeCode.connectionSettings")}</summary>
    <p className="prose">{t("claudeCode.scope")}</p>
    <p className="field__hint">{t("claudeCode.qualification")}</p>
    {status?.trust.supported && <div>
      <p className="field__hint">{t("claudeCode.trustHint")}</p>
      {status.trust.sha256 && <p>SHA-256: <code style={{ overflowWrap: "anywhere" }}>{status.trust.sha256}</code></p>}
      {status.trust.trusted && <p role="status">{t(status.trust.managed ? "claudeCode.trustManaged" : "claudeCode.trustExternal")}</p>}
      {status.trust.recoveryRequired && <Notice tone="warn">{t("claudeCode.trustRecovery")}</Notice>}
      {!status.trust.trusted && status.trust.sha256 && <label>
        <input type="checkbox" checked={trustAccepted} disabled={busy || status.running}
          onChange={(event) => setTrustAccepted(event.target.checked)} /> {t("claudeCode.trustConsent")}
      </label>}
      <div className="rowline">
        {!status.trust.sha256 && <Btn soft disabled={busy || status.running} onClick={() => void act(() => claudeCodeApi.prepareCertificate())}>{t("claudeCode.prepareTrust")}</Btn>}
        {!status.trust.trusted && status.trust.sha256 && <Btn soft disabled={busy || status.running || !trustAccepted}
          onClick={() => { setTrustAccepted(false); void act(() => claudeCodeApi.trustCertificate(status.trust.sha256!)); }}>{t("claudeCode.enableTrust")}</Btn>}
        {status.trust.managed && <Btn soft disabled={busy || status.running}
          onClick={() => void act(() => claudeCodeApi.restoreTrust())}>{t("claudeCode.restoreTrust")}</Btn>}
      </div>
    </div>}
    <Rows>
      <Row label={t("claudeCode.status")}><span role="status">{t(`claudeCode.${state}`)}</span></Row>
      <Row label={t("claudeCode.project")}>
        <input className="input" aria-label={t("claudeCode.project")} value={project}
          disabled={busy || status?.running || Boolean(status?.project)} onChange={(event) => setProject(event.target.value)} />
      </Row>
      <Row label={t("claudeCode.account")}>
        <select className="input" aria-label={t("claudeCode.account")} value={status?.accounts.selected ?? ""} disabled={busy || !status}
          onChange={(event) => void act(() => claudeCodeApi.select(event.target.value || null))}>
          <option value="">{t("claudeCode.original")}</option>
          {status?.accounts.accounts.map((account) => <option key={account.id} value={account.id}>{account.label}{account.reauthorizeRequired ? ` · ${t("claudeCode.reauthorize")}` : ""}</option>)}
        </select>
      </Row>
    </Rows>
    {state === "resume" && <Notice tone="warn">{t("claudeCode.resumeHint")}</Notice>}
    <div className="rowline">
      <Btn disabled={busy || !status || !project.trim() || status.running || (status.trust.supported && !status.trust.trusted)} onClick={() => void act(() => claudeCodeApi.start(project.trim()))}>{t(status?.recoveryRequired ? "claudeCode.reconnect" : "claudeCode.enable")}</Btn>
      <Btn soft disabled={busy || !status?.project || Boolean(status.routing.activeRequests)} onClick={() => void act(() => claudeCodeApi.stop())}>{t("claudeCode.restore")}</Btn>
      {Boolean(status?.routing.activeRequests) && <Btn danger disabled={busy} onClick={() => void act(() => claudeCodeApi.stop(true))}>{t("claudeCode.forceStop")}</Btn>}
    </div>
    {status?.routing.records.length ? <details><summary>{t("claudeCode.recent")}</summary>
      <Rows>{status.routing.records.slice(-8).reverse().map((record) => <Row key={record.id} label={record.sessionHash}>
        {label(record.accountId)}{record.operation ? ` · ${t(`claudeCode.${record.operation}`)}` : ""} · {t(`claudeCode.${record.state}`)}{record.upstreamStatus ? ` · HTTP ${record.upstreamStatus}` : ""}
      </Row>)}</Rows>
    </details> : null}
    {status?.notice && <details><summary>{t("claudeCode.details")}</summary><p>{status.notice}</p></details>}
    </details>
    <Confirm open={removeId !== null} title={t("claudeCode.removeAccountTitle")}
      facts={[{ key: t("claudeCode.account"), value: label(removeId) }]}
      confirmLabel={t("common.remove")} danger onCancel={() => setRemoveId(null)}
      onConfirm={() => { const id = removeId; setRemoveId(null); if (id) void act(() => claudeCodeApi.remove(id)); }} />
    {(error || status?.accounts.loginError) && <Notice tone="error">{error || status?.accounts.loginError}</Notice>}
  </Card>;
}
