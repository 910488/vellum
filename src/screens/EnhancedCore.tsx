/**
 * Enhanced Core —— 這一頁只回答一個問題：
 * **Codex Desktop 現在跑的是不是 Vellum 的核心，如果不是，卡在哪一步。**
 *
 * 其他所有東西都是那個答案的證據。上一版把它們攤成五張同重量的卡（狀態、
 * 相容性、Remote Control、工作階段、診斷），答案本身反而沒有位置：頁首寫
 * 「正在服務／尚未服務」，而「為什麼不」被收在第二張卡底下一個 <details>
 * 裡叫「差異與阻擋原因」。要把因果接起來，得自己讀完五張卡。
 *
 * 這一版的概念是**交接**。Enhanced 會跑，是因為四次交接依序完成了：
 *
 *   成品   釘住的核心與 bridge 在磁碟上驗得過
 *   武裝   CODEX_CLI_PATH 持有這次啟動的租約
 *   認養   Codex Desktop 真的去啟動了那支 bridge
 *   就位   兩個子行程在這次啟動的 digest 上初始化完成
 *
 * 沒在服務的時候，一定是其中**某一節**開著，而這一頁的工作就是指出那一節。
 * 所以版面就是那條交接線本身（`Seam`），證據與阻擋原因掛在它們所屬的那一
 * 節下面。相容性不再是一張卡 —— 它是「成品」那一節的證據。
 *
 * `serving` 與 `active` 的分別是這一頁存在的理由之一，所以它在頁首有自己的
 * 句子：舊啟動留下來的 bridge 仍然在服務每一個 turn，那不是「沒在跑」，
 * 但也不是「跑在你現在設定的東西上」。
 */
import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "@/lib/api";
import { activeEnhancedSessions, type RuntimeObservations } from "@/lib/enhanced";
import type { EnhancedDesktopRuntimeStatus } from "@/types";
import {
  Btn,
  Cap,
  Card,
  Empty,
  Metric,
  Notice,
  Pill,
  Row,
  Rows,
  Seam,
  SeamLink,
  Segment,
  State,
  Tray,
  type SeamState,
} from "@/components/ui";

/** 交接的四節，依序。`done` 由 status 判定，不是由後端的字串狀態轉譯 ——
 *  每一節都對應一個後端本來就有的事實，中間不多一層命名。 */
type LinkKey = "artifact" | "armed" | "adopted" | "inPlace";

function chainOf(status: EnhancedDesktopRuntimeStatus | null): Record<LinkKey, boolean> {
  return {
    artifact: Boolean(status?.artifactReady),
    armed: status?.environmentState === "leased",
    adopted: Boolean(status?.bridgeObserved),
    inPlace: Boolean(status?.active),
  };
}

const LINK_ORDER: LinkKey[] = ["artifact", "armed", "adopted", "inPlace"];

/** 第一個沒完成的那一節。全部完成時是 null。 */
function openLink(chain: Record<LinkKey, boolean>): LinkKey | null {
  return LINK_ORDER.find((key) => !chain[key]) ?? null;
}

function linkState(key: LinkKey, chain: Record<LinkKey, boolean>, open: LinkKey | null): SeamState {
  if (chain[key]) return "done";
  return key === open ? "open" : "pending";
}

/** 只讀短碼。完整 digest 在證據列裡，這裡是給掃視用的。 */
function shortDigest(digest: string | null): string | null {
  if (!digest) return null;
  const bare = digest.replace(/^sha256:/, "");
  return bare.length > 12 ? `${bare.slice(0, 12)}…` : bare;
}

/** 路徑只留最後兩段。完整路徑進 title —— 卡片寬度容不下 Windows 的絕對路徑，
 *  而會辨識的人認的是尾巴那個 hash 目錄。 */
function tailOf(path: string | null): string | null {
  if (!path) return null;
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts.slice(-2).join("\\") || path;
}

const REMOTE_FAILURE_STAGES = new Set([
  "initialize",
  "thread/list",
  "thread/read",
  "thread/resume",
  "turn/start",
  "turn/steer",
  "turn/interrupt",
  "protocol",
  "coreConnection",
  "relayProcess",
  "backpressure",
]);

/** Older bridges persisted optional capability-probe errors as connection
 * failures. Keep the renderer compatible with those observations while a new
 * bridge is waiting for Codex Desktop to restart. */
function actionableRemoteFailure(stage: string | null | undefined): string | null {
  return stage && REMOTE_FAILURE_STAGES.has(stage) ? stage : null;
}

export function EnhancedCore({ status, refreshVersion, onRefreshComplete }: {
  status: EnhancedDesktopRuntimeStatus | null;
  refreshVersion: number;
  onRefreshComplete: (version: number) => void;
}) {
  const { t } = useTranslation();
  const [report, setReport] = useState<RuntimeObservations | null>(null);
  const [runtime, setRuntime] = useState(status);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [exportPath, setExportPath] = useState<string | null>(null);
  const [filter, setFilter] = useState<"all" | "enhanced-codex" | "official-codex">("all");
  useEffect(() => setRuntime(status), [status]);
  useEffect(() => {
    let live = true;
    let timer: ReturnType<typeof setTimeout>;
    const load = async () => {
      try {
        const next = await api.getEnhancedRuntimeObservations();
        if (live) { setReport(next); setError(null); }
      } catch (e) {
        if (live) setError(String(e));
      } finally {
        if (live) { onRefreshComplete(refreshVersion); timer = setTimeout(load, 2000); }
      }
    };
    void load();
    return () => { live = false; clearTimeout(timer); };
  }, [refreshVersion, onRefreshComplete]);

  const chain = chainOf(runtime);
  const open = openLink(chain);
  const liveRows = report && !error ? activeEnhancedSessions(report) : [];
  const running = liveRows.filter((row) => row.state === "running").length;
  const remote = report?.remote;
  const remoteFailure = actionableRemoteFailure(remote?.lastFailureStage);

  const rows = useMemo(
    () =>
      Object.values(report?.sessions ?? {})
        .filter((row) => filter === "all" || row.plane === filter)
        .sort((a, b) => b.lastActivity - a.lastActivity),
    [report, filter],
  );

  const action = async (work: () => Promise<void>) => {
    setBusy(true);
    try { await work(); } catch (e) { setError(String(e)); } finally { setBusy(false); }
  };

  /* 頁首那一句。順序就是嚴重程度：使用者關掉的、我們正在修的、跑在舊啟動上
     的、正常的、卡住的 —— 每一種都有自己的話，沒有一種共用「尚未服務」。 */
  const verdict = !runtime?.enabled
    ? t("enhanced.verdict.disabled")
    : runtime.launchCoreDrift
      ? t("enhanced.launchCoreDrift")
      : runtime.serving && !runtime.active
        ? t("enhanced.verdict.servingOtherLaunch")
        : runtime.ready
          ? t("enhanced.verdict.serving")
          : t("enhanced.verdict.stoppedAt", { link: t(`enhanced.chain.${open ?? "inPlace"}`) });

  const evidence = [
    ["handshake", remote?.handshakeObserved],
    ["list", remote?.listObserved],
    ["history", remote?.historyObserved],
    ["stream", remote?.streamObserved],
    ["control", remote?.controlObserved],
  ] as const;
  const remoteOpen = evidence.find(([, observed]) => !observed)?.[0] ?? null;

  return <section className="enhanced-core">
    <div className="canvas__head">
      <div>
        <p className="eyebrow">{t("navigation.enhanced.label")}</p>
        <h2 className="canvas__title">{t("enhanced.heading")}</h2>
        <p className="note">{t("enhanced.lead")}</p>
      </div>
      <div className="enhanced-core__tools">
        <Btn soft mini disabled={busy} onClick={() => void action(async () => setRuntime(await api.recheckEnhancedRuntimeCompatibility()))}>{t("enhanced.recheck")}</Btn>
        <Btn soft mini disabled={busy} onClick={() => void action(async () => setExportPath(await api.exportEnhancedRuntimeDiagnostics()))}>{t("enhanced.export")}</Btn>
      </div>
    </div>

    <Card hero>
      <Cap>{t("enhanced.now")}</Cap>
      <div className="enhanced-core__verdict">
        {/* 主角數字是「執行中回合」，不是工作階段總數 —— 前者是這一刻真的
            有東西在跑，後者只是這次啟動以來看過幾條 thread。
            交接沒接上的時候這裡不放數字：那時唯一重要的事是斷在哪一節，
            而一個大大的 0 只會跟那句話搶視線，還讓人以為 0 是答案。 */}
        {runtime?.serving ? <Metric value={running} glow="var(--sage)" /> : null}
        <div>
          <p className="enhanced-core__says">{verdict}</p>
          <p className="enhanced-core__aside">
            {t("enhanced.runningTurns", { count: running })}
            {" · "}
            {t("enhanced.sessionCount", { count: liveRows.length })}
            {" · "}
            {t(`enhanced.states.${error ? "stale" : report?.freshness ?? "unavailable"}`)}
          </p>
        </div>
      </div>
      {exportPath ? <Notice>{t("enhanced.exported")}: <code>{exportPath}</code></Notice> : null}
      {error ? <Notice tone="warn" raw={error}>{t("enhanced.staleWarning")}</Notice> : null}
      {runtime?.unverified ? <Notice tone="warn">{t("enhanced.unverifiedNote")}</Notice> : null}
      {runtime?.missingHelpers.length ? (
        <Notice tone="warn">
          {t("enhanced.missingHelpers", { list: runtime.missingHelpers.join(t("common.itemSeparator")) })}
        </Notice>
      ) : null}
    </Card>

    <Card>
      <Cap>{t("enhanced.chain.title")}</Cap>
      <Seam>
        <SeamLink
          state={linkState("artifact", chain, open)}
          label={t("enhanced.chain.artifact")}
          mark={<State
            tone={chain.artifact ? (runtime?.unverified ? "warn" : "ok") : "quiet"}
            label={chain.artifact
              ? t(`enhanced.protocol.${runtime?.protocol?.verdict ?? "unknown"}`, { defaultValue: t("common.unknown") })
              : t("enhanced.mark.blocked")}
          />}
        >
          <Rows>
            <Row label={t("enhanced.candidateRuntime")}>
              <code title={runtime?.enhancedRuntimeDigest ?? undefined}>{shortDigest(runtime?.enhancedRuntimeDigest ?? null) ?? t("common.emDash")}</code>
            </Row>
            <Row label={t("enhanced.structure")}>
              {runtime?.protocol
                ? t(`enhanced.protocol.${runtime.protocol.verdict}`, { defaultValue: runtime.protocol.verdict })
                : t("enhanced.protocolUnrun")}
            </Row>
            <Row label={t("enhanced.qualification")}>
              {runtime?.lastQualification
                ? <Pill tone={runtime.lastQualification.passed ? "ok" : "crit"}>
                    {runtime.lastQualification.mode} · {runtime.lastQualification.passed ? "PASS" : "FAIL"}
                  </Pill>
                : <span className="rows__hint">{t("enhanced.notObserved")}</span>}
            </Row>
          </Rows>
          {runtime?.protocol?.deltas.length ? (
            <Tray label={t("enhanced.differences")} count={runtime.protocol.deltas.length}>
              <Rows>
                {runtime.protocol.deltas.map((delta, index) => (
                  <Row
                    key={`${delta.subject}-${index}`}
                    label={<><code>{delta.subject}</code>{delta.routed ? <> <Pill tone="crit">{t("enhanced.routed")}</Pill></> : null}</>}
                  >
                    <span className="rows__hint">
                      {t(`enhanced.delta.${delta.kind}`, { defaultValue: delta.kind })}
                      {delta.fields.length ? ` · ${delta.fields.join(t("common.itemSeparator"))}` : ""}
                    </span>
                  </Row>
                ))}
              </Rows>
              <p className="card__sub">{t("enhanced.evidenceNote")}</p>
            </Tray>
          ) : null}
        </SeamLink>

        <SeamLink
          state={linkState("armed", chain, open)}
          label={t("enhanced.chain.armed")}
          mark={<State
            tone={chain.armed ? "ok" : "quiet"}
            label={t(`enhanced.environment.${runtime?.environmentState ?? "unreadable"}`, { defaultValue: runtime?.environmentState ?? t("common.unknown") })}
          />}
        >
          <Rows>
            <Row label="CODEX_CLI_PATH">
              <code title={runtime?.environmentValue ?? undefined}>{tailOf(runtime?.environmentValue ?? null) ?? t("common.none")}</code>
            </Row>
          </Rows>
        </SeamLink>

        <SeamLink
          state={linkState("adopted", chain, open)}
          label={t("enhanced.chain.adopted")}
          mark={<State
            tone={chain.adopted ? "ok" : "quiet"}
            label={chain.adopted ? t("enhanced.observed") : t("enhanced.notObserved")}
          />}
        >
          <Rows>
            <Row label={t("enhanced.bridgeProcess")}>
              {runtime?.bridgePid
                ? <><code title={runtime.observedBridgeExecutable ?? undefined}>{tailOf(runtime.observedBridgeExecutable ?? null) ?? t("common.unknown")}</code> · PID {runtime.bridgePid}</>
                : <span className="rows__hint">{t("enhanced.notObserved")}</span>}
            </Row>
            <Row label={t("enhanced.bridgeState")}>
              {runtime?.bridgeState
                ? t(`enhanced.states.${runtime.bridgeState}`, { defaultValue: runtime.bridgeState })
                : t("common.emDash")}
            </Row>
          </Rows>
        </SeamLink>

        <SeamLink
          state={linkState("inPlace", chain, open)}
          label={t("enhanced.chain.inPlace")}
          mark={<State
            tone={chain.inPlace ? "ok" : "quiet"}
            label={chain.inPlace ? t("enhanced.mark.inPlace") : t("enhanced.mark.waiting")}
          />}
        >
          <Rows>
            <Row label={t("enhanced.children")}>
              {runtime?.officialChildPid || runtime?.enhancedChildPid
                ? <>Official {runtime.officialChildPid ?? t("common.emDash")} · Enhanced {runtime.enhancedChildPid ?? t("common.emDash")}</>
                : <span className="rows__hint">{t("enhanced.notObserved")}</span>}
            </Row>
            <Row label={t("enhanced.activeRuntime")}>
              <code title={runtime?.activeRuntimeDigest ?? undefined}>{shortDigest(runtime?.activeRuntimeDigest ?? null) ?? t("common.emDash")}</code>
            </Row>
            {runtime?.launchId ? (
              <Row label={t("enhanced.launch")}><code>{runtime.launchId}</code></Row>
            ) : null}
          </Rows>
        </SeamLink>
      </Seam>

      {/* 阻擋原因掛在停住的那一節下面，而不是收在頁尾的 <details> 裡。
          鏈是完整的卻仍有阻擋原因，代表那是「跑得起來但有話要說」（未驗證的
          Desktop、被換掉的核心）—— 那種掛在整條線的末端才對。 */}
      {runtime?.blockers.length ? (
        <div className={`enhanced-core__why${open ? ` enhanced-core__why--at-${open}` : ""}`}>
          <p className="enhanced-core__why-cap">
            {open ? t("enhanced.whyStopped", { link: t(`enhanced.chain.${open}`) }) : t("enhanced.whyNoted")}
          </p>
          <ul>{runtime.blockers.map((blocker, index) => <li key={index}>{blocker}</li>)}</ul>
        </div>
      ) : null}
    </Card>

    <Card>
      <Cap>Remote Control</Cap>
      <p className="card__sub" style={{ margin: "0 0 14px" }}>{t("enhanced.relayNote")}</p>
      <Rows>
        <Row label={t("enhanced.relay")}>
          <State
            tone={remote?.state === "serving" ? "ok" : "quiet"}
            label={t(`enhanced.states.${remote?.state ?? "unavailable"}`, { defaultValue: remote?.state ?? t("common.unknown") })}
          />
        </Row>
        <Row label={t("enhanced.clients")}>
          {Object.keys(remote?.clients ?? {}).length
            ? Object.entries(remote!.clients).map(([name, state]) => <Pill key={name} tone={state === "connected" ? "ok" : "quiet"}>{name}</Pill>)
            : <span className="rows__hint">{t("common.none")}</span>}
        </Row>
      </Rows>
      {/* 同一條線，五節。這五件事本來就有順序 —— 沒握手就不會有列表，
          沒串流就談不上控制 —— 所以它跟上面那條是同一個結構，不是另一種圖。 */}
      <Seam tight>
        {evidence.map(([key, observed]) => (
          <SeamLink
            key={key}
            state={observed ? "done" : key === remoteOpen ? "open" : "pending"}
            label={t(`enhanced.${key}`)}
            mark={<State tone={observed ? "ok" : "quiet"} label={t(observed ? "enhanced.observed" : "enhanced.notObserved")} />}
          />
        ))}
      </Seam>
      {remoteFailure ? (
        <Notice tone="warn">
          {t("enhanced.failureStage")}: {remoteFailure}
          {remote?.lastErrorCode != null ? ` (${remote.lastErrorCode})` : ""}
        </Notice>
      ) : null}
    </Card>

    <Card>
      <div className="rowline">
        <Cap>{t("enhanced.sessions")}</Cap>
        <Segment
          options={[
            { value: "all", label: t("enhanced.all") },
            { value: "enhanced-codex", label: "Enhanced" },
            { value: "official-codex", label: "Official" },
          ]}
          value={filter}
          onChange={setFilter}
        />
      </div>
      {/* 不分頁。這是觀測列表不是帳本，翻頁只會讓「剛剛那條 thread 在哪」
          變成一件要翻找的事；改成一塊有上限高度、自己捲動的區域，
          一屏看得完整批，看不完的往下捲一段就到，卡片外的版面不動。 */}
      {!rows.length ? <Empty>{t("enhanced.empty")}</Empty> : (
        <div className="sessions">
          {rows.map((row) => (
            <div className={`sessions__row sessions__row--${row.plane}`} key={row.threadId}>
              <span className="sessions__plane">{row.plane === "enhanced-codex" ? "Enhanced" : "Official"}</span>
              <code className="sessions__id" title={row.threadId}>{row.threadId}</code>
              <span className="sessions__model">{row.model ?? t("common.emDash")}</span>
              <span className={`sessions__state sessions__state--${row.state}`}>
                {t(`enhanced.states.${row.state}`, { defaultValue: row.state })}
              </span>
              <span className="sessions__when">{new Date(row.lastActivity * 1000).toLocaleString()}</span>
              {row.parentThreadId ? (
                <span className="sessions__parent" title={row.parentThreadId}>
                  {t("enhanced.parent")} <code>{row.parentThreadId.slice(0, 12)}…</code>
                </span>
              ) : null}
            </div>
          ))}
        </div>
      )}
    </Card>
  </section>;
}
