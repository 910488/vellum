import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Btn, Cap, Card, Confirm, Meter, Notice, Row, Rows, Segment, State } from "@/components/ui";
import { useLocaleFormat } from "@/i18n/useLocaleFormat";
import { claudeCodeApi, type ClaudeCodeRecentProject, type ClaudeCodeScope, type ClaudeCodeStatus, type ClaudeCodeWindow } from "@/lib/claudeCode";
import { startVisiblePoll } from "@/lib/visiblePoll";

type Phase = "loading" | "repair" | "stopped" | "resume" | "connected" | "waiting";

function phaseOf(status: ClaudeCodeStatus | null): Phase {
  if (!status) return "loading";
  if (status.recoveryRequired) return "repair";
  if (!status.running) return "stopped";
  const error = status.routing.lastError;
  if (error?.startsWith("client_tls") || error === "client_ca_untrusted") return "resume";
  return lastInference(status) ? "connected" : "waiting";
}

/** Only a completed 2xx `messages` request proves inference went through. */
function lastInference(status: ClaudeCodeStatus) {
  return [...status.routing.records].reverse().find((record) => record.operation === "messages"
    && record.state === "completed" && record.upstreamStatus !== null
    && record.upstreamStatus >= 200 && record.upstreamStatus < 300);
}

type GateKey = "fiveHour" | "sevenDay";

/** Fable's included weekly share: shown, never gated. Whether credits are on
 *  decides what happens past it, so that sits right under the bar. */
function FableWindow({ window, credits }: { window: ClaudeCodeWindow; credits: boolean | null }) {
  const { t } = useTranslation();
  const { resetLabel } = useLocaleFormat();
  const remaining = Math.round(windowRemaining(window));
  return <span className="acct__quota-window acct__quota-window--fable">
    <span className="acct__quota-line">
      <strong>{remaining}%</strong>
      <Meter percent={remaining} tone={remaining <= 10 ? "coral" : "default"} />
    </span>
    <small>{t("claudeCode.fable")}{window.resetsAt ? ` · ${resetLabel(window.resetsAt)}` : ""}</small>
    <small className={credits ? "acct__credits--on" : undefined}>
      {credits === null ? t("claudeCode.fableShown") : t(credits ? "claudeCode.creditsOn" : "claudeCode.creditsOff")}
    </small>
  </span>;
}

/** Matches the backend: a window past its reset is whole again. */
function windowRemaining(window: { utilization: number; resetsAt: string | null }) {
  if (window.resetsAt && Date.parse(window.resetsAt) <= Date.now()) return 100;
  return Math.max(0, Math.min(100, 100 - window.utilization));
}

function folderName(path: string) {
  return path.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || path;
}

/**
 * `refreshVersion` is the page's refresh button: subscription usage is a real
 * account request, so it is read there and only there, the same as ChatGPT.
 */
export function ClaudeCodeCard({ active, refreshVersion = 0, onRefreshed }: {
  active: boolean;
  refreshVersion?: number;
  onRefreshed?: () => void;
}) {
  const { t } = useTranslation();
  const { resetLabel } = useLocaleFormat();
  const [status, setStatus] = useState<ClaudeCodeStatus | null>(null);
  const [recent, setRecent] = useState<ClaudeCodeRecentProject[]>([]);
  const [scope, setScope] = useState<ClaudeCodeScope>("user");
  /** Folders checked for the next connection; typed ones join the list. */
  const [chosen, setChosen] = useState<string[]>([]);
  const [added, setAdded] = useState<string[]>([]);
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [trustAccepted, setTrustAccepted] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [removeId, setRemoveId] = useState<string | null>(null);
  /** A gate being dragged: shown at once, saved on release. */
  const [draft, setDraft] = useState<{ id: string; key: GateKey; floor: number } | null>(null);
  const revision = useRef(0);
  const mounted = useRef(false);
  const load = useCallback(async () => {
    const generation = revision.current;
    try {
      const next = await claudeCodeApi.status();
      if (!mounted.current || generation !== revision.current) return;
      setStatus(next);
      if (next.scope) setScope(next.scope);
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
  useEffect(() => {
    if (!active) return;
    let live = true;
    claudeCodeApi.recentProjects().then((list) => {
      if (!live) return;
      setRecent(list);
    }).catch(() => { /* Typed folders still work. */ });
    return () => { live = false; };
  }, [active]);
  useEffect(() => setTrustAccepted(false), [status?.trust.sha256]);
  const refreshed = useRef(onRefreshed);
  refreshed.current = onRefreshed;
  useEffect(() => {
    if (refreshVersion <= 0) return;
    let live = true;
    void (async () => {
      try {
        const current = await claudeCodeApi.status();
        const usable = current.accounts.accounts.filter((account) => !account.reauthorizeRequired);
        const results = await Promise.allSettled(usable.map((account) => claudeCodeApi.usage(account.id)));
        const failed = results.find((result): result is PromiseRejectedResult => result.status === "rejected");
        if (live && mounted.current) setError(failed ? String(failed.reason) : null);
        if (live) await load();
      } catch (cause) {
        if (live && mounted.current) setError(String(cause));
      } finally {
        refreshed.current?.();
      }
    })();
    return () => { live = false; };
  }, [refreshVersion, load]);

  async function act(action: () => Promise<unknown>) {
    revision.current++;
    setBusy(true); setError(null);
    try { await action(); await load(); }
    catch (cause) { if (mounted.current) setError(String(cause)); }
    finally { if (mounted.current) setBusy(false); }
  }

  const phase = phaseOf(status);
  const label = (id: string | null) => id === null ? t("claudeCode.original")
    : status?.accounts.accounts.find((account) => account.id === id)?.label ?? id;
  const selected = status?.accounts.selected ?? null;
  const inference = status ? lastInference(status) : undefined;
  const trust = status?.trust;
  const needsTrust = Boolean(trust?.supported && !trust.trusted);
  /** A lease pins the scope until it is restored. */
  const leasedProjects = status?.projects ?? [];
  const leasedScope = status?.scope ?? (leasedProjects.length ? "project" : null);
  const leased = leasedScope !== null;
  const userScope = (leasedScope ?? scope) === "user";
  const headline = phase === "connected" ? t("claudeCode.routingVia", { account: label(inference?.accountId ?? null) }) : t(`claudeCode.${phase}`);
  const headTone = phase === "connected" ? "ok" : phase === "repair" || phase === "resume" ? "warn" : "quiet";
  /** A held lease reconnects exactly its own projects. */
  const candidates = leased ? leasedProjects : [...new Set([...added, ...recent.map((entry) => entry.path)])];
  const picked = leased ? leasedProjects : chosen.filter((path) => candidates.includes(path));
  const toggle = (path: string, on: boolean) =>
    setChosen((current) => on ? [...current.filter((item) => item !== path), path] : current.filter((item) => item !== path));
  const addTyped = () => {
    const path = typed.trim();
    if (!path) return;
    setAdded((current) => current.includes(path) ? current : [path, ...current]);
    toggle(path, true);
    setTyped("");
  };

  return <Card>
    <div className="claude-code__head">
      <Cap>Claude Desktop Code · {t("claudeCode.experimental")}</Cap>
      <span role="status"><State tone={headTone} label={headline} /></span>
    </div>
    <p className="prose">{t("claudeCode.accountsHint")}</p>
    <p className="field__hint">{t("claudeCode.gateHint")}</p>

    <Rows className="official-account-list">
      <Row label={t("claudeCode.original")}>
        <span className="acct">
          <State tone={selected === null ? "ok" : "quiet"} label={selected === null ? t("claudeCode.selected") : t("claudeCode.standby")} />
          <span className="acct__quota field__hint">{t("claudeCode.passThrough")}</span>
          <span className="acct__resets">
            <Btn mini soft disabled={busy || !status || selected === null} onClick={() => void act(() => claudeCodeApi.select(null))}>
              {t(selected === null ? "models.ui.account.current" : "models.ui.account.useThis")}
            </Btn>
          </span>
          <span />
        </span>
      </Row>
      {status?.accounts.accounts.map((account) => {
        const current = selected === account.id;
        return <Row key={account.id} label={account.label}>
          <span className="acct">
            <State tone={account.reauthorizeRequired || account.gated ? "warn" : current ? "ok" : "quiet"}
              label={account.reauthorizeRequired ? t("claudeCode.reauthorize") : account.gated ? t("claudeCode.atGate")
                : current ? t("claudeCode.selected") : t("claudeCode.standby")} />
            <span className="acct__quota">
              {account.usage ? <>{(["fiveHour", "sevenDay"] as const).map((key) => {
                const window = account.usage![key];
                if (!window) return null;
                const remaining = Math.round(windowRemaining(window));
                const saved = account.gate?.[key] ?? 0;
                const floor = draft?.id === account.id && draft.key === key ? draft.floor : saved;
                const burnable = Math.max(0, remaining - floor);
                const valueText = t("claudeCode.gateValue", { floor, left: remaining });
                const commit = (next: number) => {
                  setDraft(null);
                  if (next === saved) return;
                  const gate = { fiveHour: account.gate?.fiveHour ?? 0, sevenDay: account.gate?.sevenDay ?? 0, [key]: next };
                  void act(() => claudeCodeApi.setGate(account.id, gate));
                };
                return <span className="acct__quota-window" key={key}>
                  <span className="acct__quota-line">
                    <strong>{remaining}%</strong>
                    <span className={`acct__gate${floor > 0 && floor >= remaining ? " acct__gate--over" : ""}`}>
                      <Meter percent={remaining} tone={remaining <= 10 ? "coral" : "default"} markAt={floor || undefined} markLabel={floor ? valueText : undefined} />
                      <span className="acct__gate-keep" style={{ width: `${floor}%` }} />
                      <input className="acct__gate-input" type="range" min="0" max="100" step="1" value={floor}
                        disabled={busy || account.reauthorizeRequired}
                        aria-label={t("claudeCode.gateLabel", { account: account.label, window: t(`claudeCode.${key}Name`) })}
                        aria-valuetext={valueText}
                        onChange={(event) => setDraft({ id: account.id, key, floor: Number(event.currentTarget.value) })}
                        onPointerUp={(event) => commit(Number(event.currentTarget.value))}
                        onKeyUp={(event) => commit(Number(event.currentTarget.value))}
                        onBlur={(event) => commit(Number(event.currentTarget.value))} />
                      <span className="acct__gate-focus" />
                    </span>
                  </span>
                  <small>{t(`claudeCode.${key}`)}{window.resetsAt ? ` · ${resetLabel(window.resetsAt)}` : ""}</small>
                  <small>{floor > 0
                    ? `${t("claudeCode.gateRead", { floor })} · ${burnable > 0 ? t("claudeCode.burnable", { value: burnable }) : t("claudeCode.atGate")}`
                    : t("claudeCode.noGate")}</small>
                </span>;
              })}
              {account.usage.fable && <FableWindow window={account.usage.fable} credits={account.usage.creditsEnabled ?? null} />}
              </> : <span className="field__hint">{t("claudeCode.usageNotLoaded")}</span>}
            </span>
            <span className="acct__resets">
              <Btn mini soft disabled={busy || current || account.reauthorizeRequired} onClick={() => void act(() => claudeCodeApi.select(account.id))}>
                {t(current ? "models.ui.account.current" : "models.ui.account.useThis")}
              </Btn>
              <Btn mini soft disabled={busy || current} onClick={() => setRemoveId(account.id)}>{t("common.remove")}</Btn>
            </span>
            <span />
          </span>
        </Row>;
      })}
    </Rows>
    {status?.accounts.switchedFrom && selected && <p className="field__hint" role="status">
      {t("claudeCode.switchedFrom", { from: label(status.accounts.switchedFrom), to: label(selected) })}
    </p>}
    <div className="rowline claude-code__login">
      <Btn soft disabled={busy || !status || status.accounts.loginPending} onClick={() => void act(() => claudeCodeApi.login())}>
        {t(status?.accounts.loginPending ? "models.ui.oauth.waitingBrowser" : "claudeCode.login")}
      </Btn>
      {status?.accounts.loginPending && <Btn soft disabled={busy} onClick={() => void act(() => claudeCodeApi.cancelLogin())}>{t("common.cancel")}</Btn>}
      <span className="field__hint">{t(status?.accounts.loginPending ? "claudeCode.loginPending" : "claudeCode.switchHint")}</span>
    </div>

    <section className="claude-code__connect" aria-label={t("claudeCode.connectionSettings")}>
      <Cap>{t("claudeCode.connectionSettings")}</Cap>
      {status?.running ? <>
        <Rows>
          {userScope
            ? <Row label={t("claudeCode.stepScope")}>{t("claudeCode.scopeUser")} · <code className="claude-code__path">~/.claude/settings.json</code></Row>
            : <Row label={t("claudeCode.project")}>
              <span className="claude-code__leased">{leasedProjects.map((path) => <code key={path} className="claude-code__path" title={path}>{path}</code>)}</span>
            </Row>}
          <Row label={t("claudeCode.lastInference")}>
            {inference ? `${label(inference.accountId)} · HTTP ${inference.upstreamStatus}` : t("claudeCode.noInferenceYet")}
          </Row>
        </Rows>
        {phase === "waiting" && <p className="field__hint">{t(userScope ? "claudeCode.waitingHintUser" : "claudeCode.waitingHint")}</p>}
        {phase === "resume" && <Notice tone="warn">{t("claudeCode.resumeHint")}</Notice>}
        <div className="rowline">
          <Btn soft disabled={busy || Boolean(status.routing.activeRequests)} onClick={() => void act(() => claudeCodeApi.stop())}>{t("claudeCode.restore")}</Btn>
          {Boolean(status.routing.activeRequests) && <Btn danger disabled={busy} onClick={() => void act(() => claudeCodeApi.stop(true))}>{t("claudeCode.forceStop")}</Btn>}
        </div>
      </> : <>
        {phase === "repair" && <Notice tone="warn">{t(userScope ? "claudeCode.repairHintUser" : "claudeCode.repairHint")}</Notice>}
        <ol className="claude-code__steps">
          <li className="claude-code__step">
            <span className="claude-code__step-title">{t("claudeCode.stepScope")}</span>
            {leased ? <span>{t(userScope ? "claudeCode.scopeUser" : "claudeCode.scopeProject")}</span>
              : <Segment value={scope} onChange={setScope} options={[
                { value: "user", label: t("claudeCode.scopeUser") },
                { value: "project", label: t("claudeCode.scopeProject") },
              ]} />}
            {userScope && <small className="field__hint">{t("claudeCode.scopeUserHint")}</small>}
          </li>
          {!userScope && <li className="claude-code__step">
            <span className="claude-code__step-title">{t("claudeCode.stepProject")}</span>
            {candidates.length > 0 && <ul className="claude-code__projects" aria-label={t("claudeCode.recentProjects")}>
              {candidates.map((path) => {
                const entry = recent.find((item) => item.path === path);
                return <li key={path}>
                  <label className="claude-code__project">
                    <input type="checkbox" checked={picked.includes(path)} disabled={busy || leased}
                      onChange={(event) => toggle(path, event.target.checked)} />
                    <span className="claude-code__project-name">{folderName(path)}</span>
                    {entry && <small className="field__hint">{t("claudeCode.sessionCount", { count: entry.sessions })}</small>}
                    <code className="claude-code__path" title={path}>{path}</code>
                  </label>
                </li>;
              })}
            </ul>}
            {!leased && <div className="rowline claude-code__add">
              <input className="input" aria-label={t("claudeCode.project")} value={typed} placeholder={"E:\\code\\project"} disabled={busy}
                onChange={(event) => setTyped(event.target.value)}
                onKeyDown={(event) => { if (event.key === "Enter") { event.preventDefault(); addTyped(); } }} />
              <Btn soft disabled={busy || !typed.trim()} onClick={addTyped}>{t("claudeCode.addFolder")}</Btn>
            </div>}
            <small className="field__hint">{t("claudeCode.projectHint")}</small>
          </li>}
          {trust?.supported && <li className="claude-code__step">
            <span className="claude-code__step-title">{t("claudeCode.stepTrust")}</span>
            {trust.trusted ? <span className="field__hint">{t(trust.managed ? "claudeCode.trustManaged" : "claudeCode.trustExternal")}</span> : <>
              <small className="field__hint">{t("claudeCode.trustHint")}</small>
              {trust.sha256 ? <>
                <span className="claude-code__path">SHA-256 <code>{trust.sha256}</code></span>
                <label className="claude-code__consent">
                  <input type="checkbox" checked={trustAccepted} disabled={busy}
                    onChange={(event) => setTrustAccepted(event.target.checked)} /> {t("claudeCode.trustConsent")}
                </label>
                <div className="rowline">
                  <Btn soft disabled={busy || !trustAccepted}
                    onClick={() => { setTrustAccepted(false); void act(() => claudeCodeApi.trustCertificate(trust.sha256!)); }}>{t("claudeCode.enableTrust")}</Btn>
                </div>
              </> : <div className="rowline">
                <Btn soft disabled={busy} onClick={() => void act(() => claudeCodeApi.prepareCertificate())}>{t("claudeCode.prepareTrust")}</Btn>
              </div>}
            </>}
            {trust.recoveryRequired && <Notice tone="warn">{t("claudeCode.trustRecovery")}</Notice>}
          </li>}
          <li className="claude-code__step">
            <span className="claude-code__step-title">{t("claudeCode.stepEnable")}</span>
            <div className="rowline">
              <Btn disabled={busy || !status || (!userScope && picked.length === 0) || needsTrust}
                onClick={() => void act(() => userScope ? claudeCodeApi.startUser() : claudeCodeApi.start(picked))}>
                {status?.recoveryRequired ? t("claudeCode.reconnect") : userScope ? t("claudeCode.enableUser") : t("claudeCode.enable", { count: picked.length })}
              </Btn>
              {leased && <Btn soft disabled={busy} onClick={() => void act(() => claudeCodeApi.stop())}>{t("claudeCode.restore")}</Btn>}
            </div>
            <small className="field__hint">{t("claudeCode.enableHint")}</small>
          </li>
        </ol>
      </>}
    </section>

    <details className="claude-code__more">
      <summary>{t("claudeCode.advanced")}</summary>
      <p className="prose">{t("claudeCode.scope")}</p>
      <p className="field__hint">{t("claudeCode.qualification")}</p>
      {trust?.managed && !status?.running && <div className="rowline">
        <Btn soft disabled={busy} onClick={() => void act(() => claudeCodeApi.restoreTrust())}>{t("claudeCode.restoreTrust")}</Btn>
      </div>}
      {status?.routing.records.length ? <>
        <Cap>{t("claudeCode.recent")}</Cap>
        <Rows>{status.routing.records.slice(-8).reverse().map((record) => <Row key={record.id} label={record.sessionHash}>
          {label(record.accountId)}{record.operation ? ` · ${t(`claudeCode.${record.operation}`)}` : ""} · {t(`claudeCode.${record.state}`)}{record.upstreamStatus ? ` · HTTP ${record.upstreamStatus}` : ""}
        </Row>)}</Rows>
      </> : null}
      {status?.notice && <p className="field__hint">{status.notice}</p>}
    </details>

    <Confirm open={removeId !== null} title={t("claudeCode.removeAccountTitle")}
      facts={[{ key: t("claudeCode.account"), value: label(removeId) }]}
      confirmLabel={t("common.remove")} danger onCancel={() => setRemoveId(null)}
      onConfirm={() => { const id = removeId; setRemoveId(null); if (id) void act(() => claudeCodeApi.remove(id)); }} />
    {(error || status?.accounts.loginError) && <Notice tone="error">{error || status?.accounts.loginError}</Notice>}
  </Card>;
}
