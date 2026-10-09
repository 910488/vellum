import { useCallback, useEffect, useId, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "@/lib/api";
import { Btn, Card } from "@/components/ui";
import { VellumUpdate } from "@/components/VellumUpdate";
import { autoUpdateOn, vellumUpdateHosts, vellumUpdateState } from "@/lib/vellumUpdate";
import type { UpdateStatusSnapshot } from "@/types";

const RELEASES_URL = "https://github.com/910488/vellum/releases/latest";

/** Backend reasons arrive as codes such as `proxyBusy`; anything else is shown as is. */
function describeReason(t: (key: string, options?: Record<string, unknown>) => string, raw: string): string {
  return /^[A-Za-z]+$/.test(raw)
    ? t(`settings.page.updates.reason.${raw}`, { defaultValue: raw })
    : raw;
}

export function UpdatePanel({
  open,
  snapshot,
  onChanged,
  onClose,
  onOpenHosts,
}: {
  open: boolean;
  snapshot: UpdateStatusSnapshot | null;
  onChanged: (next: UpdateStatusSnapshot) => void;
  onClose: () => void;
  onOpenHosts: () => void;
}) {
  const { t } = useTranslation();
  const ref = useRef<HTMLDialogElement>(null);
  const titleId = useId();

  useEffect(() => {
    const dialog = ref.current;
    if (!dialog) return;
    if (open && !dialog.open) dialog.showModal();
    if (!open && dialog.open) dialog.close();
  }, [open]);

  return (
    <dialog ref={ref} className="dialog update-panel" aria-labelledby={titleId} onCancel={onClose}>
      <div className="update-panel__body">
        <div className="update-panel__head">
          <div>
            <h2 className="dialog__title" id={titleId}>{t("settings.page.updates.title")}</h2>
            <p className="note">{t("settings.page.updates.panelHint")}</p>
          </div>
          <Btn soft onClick={onClose}>{t("common.close")}</Btn>
        </div>
        {snapshot ? (
          <UpdateCards snapshot={snapshot} onChanged={onChanged} onOpenHosts={onOpenHosts} />
        ) : (
          <p className="note">{t("common.loading")}</p>
        )}
      </div>
    </dialog>
  );
}

/**
 * 更新面板的容器：把後端三層快照翻成一張卡（見 lib/vellumUpdate），
 * 動作也只剩「更新 Vellum」這一件事 —— 後端的 update_vellum 會一起
 * 處理 Desktop、core 與開了閒置更新的遠端主機。
 */
export function UpdateCards({
  snapshot,
  onChanged,
  onOpenHosts,
}: {
  snapshot: UpdateStatusSnapshot;
  onChanged: (next: UpdateStatusSnapshot) => void;
  onOpenHosts: () => void;
}) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState<"restarting" | boolean>(false);
  const busyRef = useRef(false);
  const [checking, setChecking] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    onChanged(await api.getUpdateStatus());
  }, [onChanged]);

  async function run(work: () => Promise<void>, kind: "restarting" | true = true) {
    if (busyRef.current) return;
    busyRef.current = true;
    setBusy(kind);
    setError(null);
    try {
      await work();
    } catch (cause) {
      setError(
        t("settings.page.updates.actionFailed", {
          error: describeReason(t, cause instanceof Error ? cause.message : String(cause)),
        }),
      );
      await refresh().catch(() => undefined);
    } finally {
      busyRef.current = false;
      setBusy(false);
      setChecking(false);
    }
  }

  const state = vellumUpdateState(snapshot, { checking });
  // 下載與排定都在後端進行；畫面每秒讀一次，進度與剩餘工作才會動。
  const live = busy !== false || state.kind === "downloading" || state.kind === "scheduled";
  useEffect(() => {
    if (!live) return;
    let alive = true;
    const timer = window.setInterval(() => {
      void api.getUpdateStatus().then((next) => {
        if (alive) onChanged(next);
      }).catch(() => undefined);
    }, 1000);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, [live, onChanged]);

  const auto = autoUpdateOn(snapshot);
  const update = () => run(async () => onChanged(await api.updateVellum()));

  return (
    <Card data-testid="update-cards">
      <VellumUpdate
        view={{
          current: snapshot.desktop.currentVersion,
          state,
          autoUpdate: auto,
          earlyAccess: snapshot.preferences.channel === "preview",
          hosts: vellumUpdateHosts(snapshot.remote),
          notes: snapshot.desktop.releaseNotes,
          busy: busy === "restarting" ? "restarting" : busy !== false && state.kind !== "checking",
          error,
        }}
        actions={{
          onCheck: () => {
            setChecking(true);
            // 自動更新開著時，檢查到新版就直接下載，和背景排程做的事一樣。
            void run(async () => onChanged(auto ? await api.updateVellum() : await api.checkUpdates()));
          },
          onUpdate: () => void update(),
          onRetry: () => void update(),
          onCancel: () =>
            void run(async () => {
              for (const component of ["desktop", "core"] as const) {
                const phase = snapshot[component].phase;
                if (phase === "downloading" || phase === "verifying") {
                  await api.cancelUpdateDownload(component);
                }
              }
              await refresh();
            }),
          onRestart: () =>
            void run(async () => {
              // 成功時後端會結束這個程序；排定時則回到面板顯示在等什麼。
              const outcome = await api.restartToUpdate();
              if (outcome === "scheduled") await refresh();
            }, "restarting"),
          onUnschedule: () =>
            void run(async () => {
              await api.cancelScheduledRestart();
              await refresh();
            }),
          onOpenDownloads: () => {
            window.open(RELEASES_URL, "_blank", "noopener,noreferrer");
          },
          onAutoUpdate: (next) =>
            void run(async () => {
              await api.setUpdatePreferences({ autoCheck: next, autoDownload: next });
              await refresh();
            }),
          onEarlyAccess: (next) =>
            void run(async () => {
              await api.setUpdatePreferences({ channel: next ? "preview" : "stable" });
              // 換了頻道，「最新版」的定義就跟著換，要重查一次。
              onChanged(await api.checkUpdates());
            }),
          onOpenHosts,
        }}
      />
    </Card>
  );
}
