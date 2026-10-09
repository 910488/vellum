import { useId, useState } from "react";
import { useTranslation } from "react-i18next";
import { Btn, Meter, Row, Rows, Toggle } from "@/components/ui";

/**
 * 使用者心裡只有一個版本號：「我的 Vellum 是不是最新的」。Desktop、
 * Enhanced core、遠端套件是同一個 release 的三個檔案，由後端一起選、
 * 一起下載；這裡不讓人逐層挑。整張卡講的就是那個版本號怎麼往前走，
 * 所以版本號本身是主角，有新版時它會寫成「舊 → 新」。
 *
 * 每個狀態只有一個主要動作。等待中不要求再按一次：排定的重啟會在
 * 對話結束後自己發生，唯一能做的是取消排定。
 */
export type VellumUpdateState =
  | { kind: "current"; checkedAt: string | null }
  | { kind: "checking" }
  /** 自動更新關著時，檢查到新版只會停在這裡，等使用者按。 */
  | { kind: "available"; version: string }
  | { kind: "downloading"; version: string; percent: number }
  | { kind: "ready"; version: string }
  | { kind: "scheduled"; version: string; turns: number; requests: number }
  | { kind: "failed"; version: string | null; reason: string }
  /** 這個組建沒有更新公鑰：只能手動裝一次帶公鑰的版本。 */
  | { kind: "unsigned" };

export interface VellumUpdateHosts {
  total: number;
  pending: number;
  failed: number;
}

export interface VellumUpdateView {
  current: string;
  state: VellumUpdateState;
  autoUpdate: boolean;
  earlyAccess: boolean;
  hosts: VellumUpdateHosts;
  /** 目標版本的版本說明；沒有就不出連結。 */
  notes: string | null;
  /** 有動作在跑：主要按鈕與開關先停用。 */
  busy?: "restarting" | boolean;
  /** 上一個動作失敗的說明，已翻成人話。 */
  error?: string | null;
}

export interface VellumUpdateActions {
  onCheck: () => void;
  onUpdate: () => void;
  onCancel: () => void;
  onRestart: () => void;
  onUnschedule: () => void;
  onRetry: () => void;
  onOpenDownloads: () => void;
  onAutoUpdate: (next: boolean) => void;
  onEarlyAccess: (next: boolean) => void;
  onOpenHosts: () => void;
}

const K = "settings.page.updates.simple";

function targetOf(state: VellumUpdateState): string | null {
  switch (state.kind) {
    case "available":
    case "downloading":
    case "ready":
    case "scheduled":
    case "failed":
      return state.version || null;
    default:
      return null;
  }
}

function tone(state: VellumUpdateState): "ok" | "busy" | "ready" | "warn" | "crit" {
  switch (state.kind) {
    case "current":
      return "ok";
    case "checking":
    case "downloading":
      return "busy";
    case "available":
    case "ready":
      return "ready";
    case "scheduled":
      return "warn";
    case "failed":
    case "unsigned":
      return "crit";
  }
}

export function VellumUpdate({
  view,
  actions,
}: {
  view: VellumUpdateView;
  actions: VellumUpdateActions;
}) {
  const { t, i18n } = useTranslation();
  const { state } = view;
  const target = targetOf(state);
  const notesId = useId();
  const [notesOpen, setNotesOpen] = useState(false);
  const busy = Boolean(view.busy);

  const sentence = (() => {
    switch (state.kind) {
      case "current":
        return state.checkedAt
          ? t(`${K}.state.currentChecked`, {
              time: new Intl.DateTimeFormat(i18n.language, {
                dateStyle: "medium",
                timeStyle: "short",
              }).format(new Date(state.checkedAt)),
            })
          : t(`${K}.state.current`);
      case "checking":
        return t(`${K}.state.checking`);
      case "available":
        return t(`${K}.state.available`);
      case "downloading":
        return t(`${K}.state.downloading`, { percent: Math.round(state.percent) });
      case "ready":
        return t(`${K}.state.ready`);
      case "scheduled": {
        if (!state.turns && !state.requests) return t(`${K}.state.scheduledSoon`);
        const parts = [
          state.turns ? t(`${K}.busy.turns`, { count: state.turns }) : null,
          state.requests ? t(`${K}.busy.requests`, { count: state.requests }) : null,
        ].filter(Boolean);
        return t(`${K}.state.scheduled`, { work: parts.join(t(`${K}.busy.and`)) });
      }
      case "failed":
        return t(`${K}.state.failed`, {
          reason: t(`settings.page.updates.reason.${state.reason}`, { defaultValue: state.reason }),
        });
      case "unsigned":
        return t(`${K}.state.unsigned`);
    }
  })();

  const primary = (() => {
    switch (state.kind) {
      case "current":
        return <Btn soft disabled={busy} onClick={actions.onCheck}>{t(`${K}.action.check`)}</Btn>;
      case "checking":
        return <Btn soft disabled>{t(`${K}.action.checking`)}</Btn>;
      case "available":
        return <Btn disabled={busy} onClick={actions.onUpdate}>{t(`${K}.action.update`)}</Btn>;
      case "downloading":
        return <Btn soft onClick={actions.onCancel}>{t(`${K}.action.cancel`)}</Btn>;
      case "ready":
        return (
          <Btn disabled={busy} onClick={actions.onRestart}>
            {view.busy === "restarting" ? t(`${K}.action.restarting`) : t(`${K}.action.restart`)}
          </Btn>
        );
      case "scheduled":
        return <Btn soft disabled={busy} onClick={actions.onUnschedule}>{t(`${K}.action.unschedule`)}</Btn>;
      case "failed":
        return <Btn disabled={busy} onClick={actions.onRetry}>{t(`${K}.action.retry`)}</Btn>;
      case "unsigned":
        return <Btn onClick={actions.onOpenDownloads}>{t(`${K}.action.downloads`)}</Btn>;
    }
  })();

  const hostsLine = (() => {
    const { total, pending, failed } = view.hosts;
    if (total === 0) return t(`${K}.hosts.none`);
    if (failed > 0) return t(`${K}.hosts.failed`, { count: total, failed });
    if (pending > 0) return t(`${K}.hosts.pending`, { count: total, pending });
    return t(`${K}.hosts.current`, { count: total });
  })();

  return (
    <section className={`vupdate vupdate--${tone(state)}`} data-testid="vellum-update" data-state={state.kind}>
      <div className="vupdate__head">
        <div className="vupdate__version" aria-label={t(`${K}.versionLabel`, { version: view.current })}>
          <span className="vupdate__name">Vellum</span>
          <span className="vupdate__numbers">
            <span className={target ? "vupdate__from" : undefined}>{view.current}</span>
            {target ? (
              <>
                <span className="vupdate__arrow" aria-hidden="true">→</span>
                <span className="vupdate__to">{target}</span>
              </>
            ) : null}
          </span>
        </div>
        <div className="vupdate__act">{primary}</div>
      </div>

      <div className="vupdate__state" role="status">
        <span className="vupdate__dot" aria-hidden="true" />
        <span>
          {sentence}
          {view.notes && target ? (
            <button
              type="button"
              className="vupdate__link"
              aria-expanded={notesOpen}
              aria-controls={notesId}
              onClick={() => setNotesOpen((open) => !open)}
            >
              {t(`${K}.action.notes`)}
            </button>
          ) : null}
        </span>
      </div>
      {state.kind === "downloading" ? <Meter percent={state.percent} tone="honey" /> : null}
      {view.notes && target && notesOpen ? (
        <div className="vupdate__notes" id={notesId} aria-label={t(`${K}.notesTitle`)}>
          {view.notes}
        </div>
      ) : null}
      {view.error ? (
        <p className="vupdate__error" role="alert">{view.error}</p>
      ) : null}

      <Rows>
        <Row
          label={
            <span className="runtime-control__copy">
              <span>{t(`${K}.auto`)}</span>
              <span className="rows__hint">{t(`${K}.autoHint`)}</span>
            </span>
          }
        >
          <Toggle
            checked={view.autoUpdate}
            label={t(`${K}.auto`)}
            disabled={state.kind === "unsigned" || busy}
            onChange={actions.onAutoUpdate}
          />
        </Row>
        <Row
          label={
            <span className="runtime-control__copy">
              <span>{t(`${K}.early`)}</span>
              <span className="rows__hint">{t(`${K}.earlyHint`)}</span>
            </span>
          }
        >
          <Toggle
            checked={view.earlyAccess}
            label={t(`${K}.early`)}
            disabled={state.kind === "unsigned" || busy}
            onChange={actions.onEarlyAccess}
          />
        </Row>
        <Row label={t(`${K}.hosts.title`)}>
          <span className="vupdate__hosts">
            <span className={view.hosts.failed ? "rows__hint rows__hint--warn" : "rows__hint"}>{hostsLine}</span>
            {view.hosts.total > 0 ? (
              <Btn soft mini onClick={actions.onOpenHosts}>{t(`${K}.hosts.open`)}</Btn>
            ) : null}
          </span>
        </Row>
      </Rows>
    </section>
  );
}
