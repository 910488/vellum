import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "@/lib/api";
import { refreshHandoffs, useHandoffRecords } from "@/lib/handoffStore";
import { handoffReviewTarget } from "@/lib/handoffNavigation";
import { handoffDraft, handoffState, handoffSummary, handoffTerminal, type RemoteHandoffPreflight, type RemoteHandoffRecord, type RemoteHandoffReview, type RemoteHandoffThread } from "@/lib/remoteHandoff";
import { HandoffThreadActions } from "./HandoffThreadActions";
import { Btn, Cap, Card, Confirm, Notice, State } from "./ui";
import "@/styles/handoff.css";

export function RemoteHandoff({ hostId, hostName, active }: { hostId: string; hostName: string; active: boolean }) {
  const { t, i18n } = useTranslation();
  const records = useHandoffRecords().filter((record) => record.hostId === hostId);
  const [threads, setThreads] = useState<RemoteHandoffThread[]>([]);
  const [selected, setSelected] = useState("");
  const [report, setReport] = useState<RemoteHandoffPreflight | null>(null);
  const [brief, setBrief] = useState("");
  const [requiredOs, setRequiredOs] = useState<"" | "windows" | "linux" | "darwin">("");
  const [recordId, setRecordId] = useState<string | null>(() => handoffReviewTarget()?.host === hostId ? handoffReviewTarget()?.id ?? null : null);
  const [compose, setCompose] = useState(false);
  const [review, setReview] = useState<RemoteHandoffReview | null>(null);
  const [showDiff, setShowDiff] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [discardId, setDiscardId] = useState<string | null>(null);
  const generation = useRef(0);
  const inFlight = useRef(false);
  const briefEdited = useRef(false);
  const autoCaptured = useRef(new Set<string>());
  const record = compose ? undefined : records.find((r) => r.id === recordId) ?? records.find((r) => !r.result.outcome);
  const terminal = record ? handoffTerminal(record) : false;
  const captured = record?.result.capture;
  const [threadRefresh, setThreadRefresh] = useState(0);

  useEffect(() => {
    if (!active) return;
    let live = true;
    void Promise.resolve().then(() => api.listRemoteHandoffThreads()).then((rows) => {
      if (live) { setThreads(rows); setSelected((current) => rows.some((row) => row.threadId === current && !row.handedOff) ? current : rows.find((row) => !row.handedOff)?.threadId || ""); }
    }).catch((e: unknown) => { if (live) setFailure(String(e)); });
    return () => { live = false; };
  }, [active, hostId, compose, threadRefresh]);
  useEffect(() => {
    const open = (event: Event) => {
      const detail = (event as CustomEvent<{ host: string; id: string }>).detail;
      if (detail.host === hostId) { setCompose(false); setRecordId(detail.id); }
    };
    window.addEventListener("vellum:handoff-review", open);
    return () => window.removeEventListener("vellum:handoff-review", open);
  }, [hostId]);
  useEffect(() => {
    const current = ++generation.current;
    setReport(null); setFailure(null);
    if (!selected || !active) return;
    void Promise.resolve().then(() => api.preflightRemoteHandoff(hostId, selected, requiredOs ? { requiredOs } : {})).then((value) => {
      if (generation.current === current) setReport(value);
    }).catch((e: unknown) => { if (generation.current === current) setFailure(String(e)); });
    return () => { generation.current++; };
  }, [selected, hostId, active, requiredOs]);
  useEffect(() => {
    if (report && !briefEdited.current) setBrief(handoffDraft(report.thread.draft, (key) => t(key)));
  }, [report, t]);
  useEffect(() => {
    let live = true;
    setReview(null); setShowDiff(false);
    if (!record?.result.fetched || record.result.outcome || !active) return;
    void Promise.resolve().then(() => api.reviewRemoteHandoff(record.id)).then((value) => { if (live) setReview(value); })
      .catch((e: unknown) => { if (live) setFailure(String(e)); });
    return () => { live = false; };
  }, [record?.id, record?.result.capture?.commit, record?.result.fetched, record?.result.outcome, active]);
  useEffect(() => {
    if (!active || !record || !terminal || record.result.outcome || busy || inFlight.current) return;
    if (record.result.fetched && !record.result.capture?.partial) return;
    const key = `${record.id}:${record.remote?.state === "completed" ? "completed" : "partial"}`;
    if (autoCaptured.current.has(key)) return;
    autoCaptured.current.add(key);
    void act(() => api.captureRemoteHandoff(record.id));
  }, [active, record?.id, terminal, record?.remote?.state, record?.result.capture?.partial, record?.result.fetched, record?.result.outcome, busy]);

  async function act(work: () => Promise<RemoteHandoffRecord>) {
    if (inFlight.current) return;
    inFlight.current = true; setBusy(true); setFailure(null);
    try {
      const next = await work();
      setRecordId(next.id); setCompose(false);
      await refreshHandoffs();
      if (next.result.outcome) setThreads(await api.listRemoteHandoffThreads());
    } catch (e) { setFailure(String(e)); await refreshHandoffs().catch(() => undefined); }
    finally { inFlight.current = false; setBusy(false); }
  }
  function message(detail: string) {
    const code = detail.split(":")[0]?.trim() ?? "";
    if (["HandoffDaemonUnavailable", "HandoffHostNotReady", "codexRuntimeIncomplete", "RemoteCodexAgentUpdateRequired"].includes(code)) return t("handoff.hostRepair");
    if (["HandoffGitUnavailable", "HandoffGitVersionUnsupported"].includes(code)) return t("handoff.gitRepair");
    if (code === "officialAccountPairingRequired") return t("handoff.pairRepair");
    const key = `handoff.errors.${code}`;
    return i18n.exists(key) ? t(key) : t("handoff.checkFailed");
  }
  const date = (timestamp: number) => new Date(timestamp < 1e12 ? timestamp * 1000 : timestamp).toLocaleString(i18n.language);
  return <>
    <Card className="ho">
      <div className="ho-head"><div><h3>{record ? record.source.name : t("handoff.title", { host: hostName })}</h3>
        <p className="note">{record ? `${record.source.repo} · ${record.source.branch}` : t("handoff.lede")}</p></div>
        {record ? <State tone={terminal ? "ok" : "warn"} label={t(`handoff.states.${handoffState(record)}`)} /> : null}
      </div>
      {failure ? <Notice tone="warn" raw={failure}>{message(failure)}</Notice> : null}
      {!record ? <>
        <fieldset className="ho-threads" disabled={busy}><legend>{t("handoff.chooseThread")} <button type="button" className="btn btn--soft btn--mini" disabled={busy} onClick={() => setThreadRefresh((n) => n + 1)} aria-label={t("handoff.refreshThreads")}>↻</button></legend>
          {threads.length ? threads.map((thread) => <label key={thread.threadId} className={`ho-thread${selected === thread.threadId ? " ho-thread--on" : ""}`}>
            <input type="radio" name={`handoff-${hostId}`} value={thread.threadId} checked={selected === thread.threadId} disabled={thread.handedOff} onChange={() => { briefEdited.current = false; setSelected(thread.threadId); setBrief(""); }} />
            <span><strong>{thread.name}</strong><small>{thread.repo} · {thread.branch}</small></span>
            <small>{thread.handedOff ? t("handoff.handedOff") : date(thread.updatedAt)}</small>
          </label>) : <p className="note">{t("handoff.noThreads")}</p>}
        </fieldset>
        <label className="ho-brief">{t("handoff.requiredOs")}<select className="input" disabled={busy} value={requiredOs} onChange={(event) => setRequiredOs(event.target.value as typeof requiredOs)}>
          <option value="">{t("handoff.detectOs")}</option><option value="windows">Windows</option><option value="linux">Linux</option><option value="darwin">macOS</option>
        </select></label>
        {report ? <>
          <div className="ho-cargo">
            {[[report.changes.filter((c) => !c.untracked).length, "changed"], [report.changes.filter((c) => c.staged).length, "staged"], [report.changes.filter((c) => c.untracked).length, "untracked"]].map(([count, key]) => <div key={key}><b>{count}</b><span>{t(`handoff.${key}`)}</span></div>)}
            <ul>{report.changes.map((change) => <li key={change.path}><code>{change.untracked ? "?" : change.staged ? "S" : "M"}</code> {change.path}</li>)}</ul>
          </div>
          <p className="note">{t("handoff.transferNote")}</p>
          <label className="ho-brief">{t("handoff.brief")}<textarea className="input" rows={7} value={brief} onChange={(event) => { briefEdited.current = true; setBrief(event.target.value); }} disabled={busy} placeholder={t("handoff.briefPlaceholder")} /></label>
          <p className="note">{t("handoff.briefHint")}</p>
          <p className="note">{report.thread.model} · {report.thread.effort} · {report.remoteOs}</p>
          {!report.autoWriteback ? <p className="note">{t("handoff.noAutoWriteback")}</p> : null}
          {report.blockers.map((blocker) => <Notice key={blocker} tone="warn" raw={blocker}>{message(blocker)}</Notice>)}
          {report.warnings.map((warning) => <Notice key={warning} raw={warning}>{t("handoff.toolWarning", { tool: warning.match(/^Remote tool (\S+)/)?.[1] ?? "" })}</Notice>)}
          {!report.blockers.length ? <p className="ho-ok">✓ {t("handoff.preflightReady")}</p> : null}
          <p className="note">{t("handoff.refNotice")}</p>
          <div className="ho-acts"><Btn disabled={busy || !!report.blockers.length || !brief.trim()} onClick={() => void act(() => api.startRemoteHandoff(hostId, selected, brief, requiredOs ? { requiredOs } : {}))}>{t("handoff.start")}</Btn>
            <Btn soft disabled={busy} onClick={() => { briefEdited.current = false; setBrief(""); setReport(null); setSelected(""); }}>{t("handoff.reset")}</Btn></div>
        </> : selected && !failure ? <p role="status">{t("handoff.checking")}</p> : null}
      </> : <>
        {record.remote?.providerError ? <Notice tone="warn"><strong>{t("handoff.providerFailed", { provider: record.remote.providerError.provider, status: record.remote.providerError.status ?? "—" })}</strong>{record.remote.providerError.route ? <p><code>{record.remote.providerError.route}</code></p> : null}<p>{record.remote.providerError.message}</p></Notice> : null}
        {record.writeback?.state === "delivered" ? <p className="note">{t("handoff.writebackDelivered")}</p> : record.writeback?.state === "skipped" && record.writeback.reason === "officialThread" ? <p className="note">{t("handoff.writebackUnsupported")}</p> : null}
        <div className="ho-relay"><span>{t("handoff.local")}<strong>{t("handoff.handedOff")}</strong></span><i aria-hidden="true" /><span>{hostName}<strong>{t(`handoff.states.${handoffState(record)}`)}</strong></span></div>
        {record.remote?.threadId ? <div><p className="note">{t("handoff.remoteThread")} <code>{record.remote.threadId}</code></p>
          <HandoffThreadActions key={record.id} record={record} />
        </div> : null}
        {record.error ? <Notice tone="warn" raw={record.error}>{message(record.error)}</Notice> : null}
        {record.remote?.summary ? <p>{record.remote.summary}</p> : null}
        <Btn mini soft onClick={() => void (async () => { try { await navigator.clipboard.writeText(handoffSummary(record, (key) => t(key))); } catch (e) { setFailure(String(e)); } })()}>{t("handoff.copySummary")}</Btn>
        {record.remote?.plan.length ? <ol className="ho-plan">{record.remote.plan.map((line, index) => <li key={index}>{line}</li>)}</ol> : null}
        {record.remote?.recentOutput ? <pre className="ho-log">{record.remote.recentOutput}</pre> : null}
        {captured && record.result.fetched ? <>
          {captured.partial ? <Notice tone="warn">{t("handoff.partialCapture")}</Notice> : null}
          {captured.excludedCacheFiles ? <p className="note">{t("handoff.excludedCaches", { count: captured.excludedCacheFiles })}</p> : null}
          <div className="ho-diff">{captured.diffstat.length ? captured.diffstat.map((file) => <div key={file.path}><code>{file.path}</code><span className="ho-add">{file.added == null ? t("handoff.binary") : `+${file.added}`}</span><span className="ho-del">{file.deleted == null ? "" : `−${file.deleted}`}</span></div>) : <p>{t("handoff.noChanges")}</p>}</div>
          {review && !review.canApply && !record.result.outcome ? <Notice tone="warn">{t("handoff.drift")}</Notice> : null}
          {showDiff && review ? <pre className="ho-log" tabIndex={0}>{review.patch || t("handoff.noChanges")}</pre> : null}
        </> : null}
        {record.result.outcome ? <Notice>{t(`handoff.states.${record.result.outcome}`)}{record.result.branch ? <p><code>{record.result.branch}</code></p> : null}
          {!record.result.cleaned ? <Btn soft disabled={busy} onClick={() => void act(() => record.result.outcome === "discarded" ? api.discardRemoteHandoff(record.id) : api.settleRemoteHandoff(record.id, true))}>{t("handoff.retryCleanup")}</Btn> : null}
        </Notice> : <div className="ho-acts">
          {captured ? <Btn soft disabled={busy || record.stage !== "running"} onClick={() => void act(() => api.captureRemoteHandoff(record.id))}>{t("handoff.recapture")}</Btn> : null}
          {record.result.fetched && terminal ? <>
            <Btn disabled={busy} onClick={() => void act(() => api.settleRemoteHandoff(record.id, !review?.canApply))}>{t(review?.canApply ? "handoff.apply" : "handoff.branch")}</Btn>
            {review?.canApply ? <Btn soft disabled={busy} onClick={() => void act(() => api.settleRemoteHandoff(record.id, true))}>{t("handoff.branch")}</Btn> : null}
          </> : <Btn disabled={busy || record.stage !== "running"} onClick={() => void act(() => api.captureRemoteHandoff(record.id))}>{t(terminal ? "handoff.fetch" : "handoff.fetchPartial")}</Btn>}
          {record.stage !== "running" ? <Btn soft disabled={busy} onClick={() => void act(() => api.resumeRemoteHandoff(record.id))}>{t("handoff.resume")}</Btn> : null}
          {review ? <Btn soft disabled={busy} onClick={() => setShowDiff(!showDiff)}>{t("handoff.fullDiff")}</Btn> : null}
          {terminal || ["localSnapshot", "remotePrepared", "snapshotPushed"].includes(record.stage) ? <Btn soft disabled={busy} onClick={() => setDiscardId(record.id)}>{t("handoff.discard")}</Btn> : <span className="note">{t("handoff.detached")}</span>}
        </div>}
      </>}
    </Card>
    <Card quiet><div className="ho-head"><Cap>{t("handoff.ledger")}</Cap><Btn mini soft disabled={busy} onClick={() => { briefEdited.current = false; setBrief(""); setCompose(true); setRecordId(null); setSelected(""); setReport(null); }}>{t("handoff.new")}</Btn></div>
      {records.length ? records.map((row) => <button className="ho-ledger" key={row.id} onClick={() => { setCompose(false); setRecordId(row.id); setFailure(null); }}><span><strong>{row.source.name}</strong><small>{row.source.branch} · {row.id}</small></span><State tone={handoffTerminal(row) ? "ok" : "quiet"} label={t(`handoff.states.${handoffState(row)}`)} /></button>) : <p className="note">{t("handoff.noRecords")}</p>}
    </Card>
    {discardId ? <Confirm open title={t("handoff.discard")} confirmLabel={t("handoff.discard")} facts={[{ key: t("handoff.discard"), value: t("handoff.discardConfirm") }]} onCancel={() => setDiscardId(null)} onConfirm={() => { const id = discardId; setDiscardId(null); void act(() => api.discardRemoteHandoff(id)); }} /> : null}
  </>;
}
