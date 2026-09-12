import { useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "@/lib/api";
import { Btn, Cap, Card, Row, Rows, Segment, Toggle } from "@/components/ui";
import type {
  LayerStatus,
  UpdateChannel,
  UpdateComponent,
  UpdateStatusSnapshot,
} from "@/types";

const CHANNELS: UpdateChannel[] = ["stable", "preview"];

export function UpdateCards({
  snapshot,
  onChanged,
}: {
  snapshot: UpdateStatusSnapshot | null;
  onChanged: (next: UpdateStatusSnapshot) => void;
}) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [expandedRemote, setExpandedRemote] = useState(false);

  if (!snapshot) return null;

  async function run(label: string, work: () => Promise<void>) {
    setBusy(label);
    setError(null);
    try {
      await work();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(null);
    }
  }

  return (
    <Card data-testid="update-cards">
      <Cap>{t("settings.page.updates.title")}</Cap>
      <p className="note" style={{ marginTop: 10 }}>
        {t("settings.page.updates.description")}
      </p>
      {!snapshot.liveAutoUpdate ? (
        <p className="note" data-testid="update-live-disabled">
          {t("settings.page.updates.liveDisabled")}
        </p>
      ) : null}
      {error ? (
        <p className="note" role="alert" data-testid="update-action-error">
          {t("settings.page.updates.actionFailed", { error })}
        </p>
      ) : null}
      <Rows>
        <Row label={t("settings.page.updates.channel")}>
          <Segment
            value={snapshot.preferences.channel}
            options={CHANNELS.map((channel) => ({
              value: channel,
              label: t(`settings.page.updates.${channel}`),
            }))}
            onChange={(channel) =>
              void run("channel", async () => {
                await api.setUpdatePreferences({ channel });
                onChanged(await api.getUpdateStatus());
              })
            }
          />
        </Row>
        <Row label={t("settings.page.updates.autoCheck")}>
          <Toggle
            checked={snapshot.preferences.autoCheck}
            label={t("settings.page.updates.autoCheck")}
            onChange={(autoCheck) =>
              void run("auto-check", async () => {
                await api.setUpdatePreferences({ autoCheck });
                onChanged(await api.getUpdateStatus());
              })
            }
          />
        </Row>
        <Row label={t("settings.page.updates.autoDownload")}>
          <Toggle
            checked={snapshot.preferences.autoDownload}
            label={t("settings.page.updates.autoDownload")}
            onChange={(autoDownload) =>
              void run("auto-download", async () => {
                await api.setUpdatePreferences({ autoDownload });
                onChanged(await api.getUpdateStatus());
              })
            }
          />
        </Row>
        <Row label={t("settings.page.updates.idleHandoff")}>
          <Toggle
            checked={snapshot.preferences.coreIdleHandoff}
            label={t("settings.page.updates.idleHandoff")}
            onChange={(coreIdleHandoff) =>
              void run("handoff", async () => {
                await api.setUpdatePreferences({ coreIdleHandoff });
                onChanged(await api.getUpdateStatus());
              })
            }
          />
        </Row>
      </Rows>
      <LayerCard
        layer={snapshot.desktop}
        busy={busy}
        onCheck={() =>
          void run("check-desktop", async () => {
            onChanged(await api.checkUpdates("desktop"));
          })
        }
        onDownload={() =>
          void run("dl-desktop", async () => {
            await api.downloadUpdate("desktop");
            onChanged(await api.getUpdateStatus());
          })
        }
        onApply={() =>
          void run("apply-desktop", async () => {
            await api.applyUpdate("desktop");
            onChanged(await api.getUpdateStatus());
          })
        }
        onCancel={() =>
          void run("cancel-desktop", async () => {
            await api.cancelUpdateDownload("desktop");
            onChanged(await api.getUpdateStatus());
          })
        }
        onRollback={() =>
          void run("rb-desktop", async () => {
            await api.rollbackUpdate("desktop");
            onChanged(await api.getUpdateStatus());
          })
        }
      />
      <LayerCard
        layer={snapshot.remote}
        busy={busy}
        expandable
        expanded={expandedRemote}
        onToggleExpand={() => setExpandedRemote((open) => !open)}
        onCheck={() =>
          void run("check-remote", async () => {
            onChanged(await api.checkUpdates("remote"));
          })
        }
        onDownload={() =>
          void run("dl-remote", async () => {
            await api.downloadUpdate("remote");
            onChanged(await api.getUpdateStatus());
          })
        }
        onApply={() =>
          void run("apply-remote", async () => {
            await api.applyUpdate("remote");
            onChanged(await api.getUpdateStatus());
          })
        }
        onCancel={() =>
          void run("cancel-remote", async () => {
            await api.cancelUpdateDownload("remote");
            onChanged(await api.getUpdateStatus());
          })
        }
        onRollback={() =>
          void run("rb-remote", async () => {
            await api.rollbackUpdate("remote");
            onChanged(await api.getUpdateStatus());
          })
        }
        onHostPolicy={(hostId, idleAutoUpdate) =>
          void run(`policy-${hostId}`, async () => {
            await api.setRemoteUpdatePolicy(hostId, idleAutoUpdate);
            onChanged(await api.getUpdateStatus());
          })
        }
      />
      <LayerCard
        layer={snapshot.core}
        busy={busy}
        onCheck={() =>
          void run("check-core", async () => {
            onChanged(await api.checkUpdates("core"));
          })
        }
        onDownload={() =>
          void run("dl-core", async () => {
            await api.downloadUpdate("core");
            onChanged(await api.getUpdateStatus());
          })
        }
        onApply={() =>
          void run("apply-core", async () => {
            await api.applyUpdate("core");
            onChanged(await api.getUpdateStatus());
          })
        }
        onCancel={() =>
          void run("cancel-core", async () => {
            await api.cancelUpdateDownload("core");
            onChanged(await api.getUpdateStatus());
          })
        }
        onRollback={() =>
          void run("rb-core", async () => {
            await api.rollbackUpdate("core");
            onChanged(await api.getUpdateStatus());
          })
        }
      />
    </Card>
  );
}

function LayerCard({
  layer,
  busy,
  expandable,
  expanded,
  onToggleExpand,
  onCheck,
  onDownload,
  onApply,
  onCancel,
  onRollback,
  onHostPolicy,
}: {
  layer: LayerStatus;
  busy: string | null;
  expandable?: boolean;
  expanded?: boolean;
  onToggleExpand?: () => void;
  onCheck: () => void;
  onDownload: () => void;
  onApply: () => void;
  onCancel: () => void;
  onRollback: () => void;
  onHostPolicy?: (hostId: string, idle: boolean) => void;
}) {
  const { t } = useTranslation();
  const progress =
    layer.downloadTotal > 0
      ? Math.min(100, Math.round((layer.downloadBytes / layer.downloadTotal) * 100))
      : layer.phase === "downloading"
        ? 0
        : null;
  return (
    <div data-testid={`update-layer-${layer.component}`} style={{ marginTop: 18 }}>
      <Cap>{t(`settings.page.updates.layer.${layer.component}`)}</Cap>
      {!layer.liveAutoUpdate ? (
        <p className="note">{t("settings.page.updates.layerDisabled")}</p>
      ) : null}
      <Rows>
        <Row label={t("settings.page.updates.current")}>{layer.currentVersion}</Row>
        <Row label={t("settings.page.updates.available")}>
          {layer.availableVersion ?? t("settings.page.updates.none")}
        </Row>
        <Row label={t("settings.page.updates.progress")}>
          {progress === null ? t("settings.page.updates.phase." + layer.phase) : `${progress}%`}
        </Row>
        <Row label={t("settings.page.updates.applyWhen")}>
          {t(`settings.page.updates.condition.${layer.applyCondition}`, {
            defaultValue: layer.applyCondition,
          })}
        </Row>
        {layer.releaseNotes ? (
          <Row label={t("settings.page.updates.notes")}>{layer.releaseNotes}</Row>
        ) : null}
        {layer.failureReason ? (
          <Row label={t("settings.page.updates.failure")}>{layer.failureReason}</Row>
        ) : null}
      </Rows>
      <div className="rowline" style={{ marginTop: 12 }}>
        <Btn soft disabled={busy !== null || !layer.liveAutoUpdate} onClick={onCheck}>
          {t("settings.page.updates.check")}
        </Btn>
        <Btn soft disabled={busy !== null || !layer.liveAutoUpdate || layer.phase !== "available"} onClick={onDownload}>
          {t("settings.page.updates.download")}
        </Btn>
        <Btn
          soft
          disabled={
            busy !== null ||
            !layer.liveAutoUpdate ||
            (layer.phase !== "waitingForIdle" &&
              layer.phase !== "waitingForRestart" &&
              layer.phase !== "staged")
          }
          onClick={onApply}
        >
          {t("settings.page.updates.apply")}
        </Btn>
        <Btn soft disabled={busy !== null || layer.phase !== "downloading"} onClick={onCancel}>
          {t("settings.page.updates.cancel")}
        </Btn>
        <Btn
          soft
          disabled={busy !== null || (layer.phase !== "failed" && layer.phase !== "rolledBack" && layer.phase !== "applied")}
          onClick={onRollback}
        >
          {t("settings.page.updates.rollback")}
        </Btn>
      </div>
      {expandable ? (
        <div style={{ marginTop: 8 }}>
          <Btn soft onClick={onToggleExpand}>
            {t("settings.page.updates.hosts")}
          </Btn>
          {expanded
            ? layer.hosts.map((host) => (
                <Rows key={host.hostId}>
                  <Row label={host.hostId}>
                    <span className="runtime-control">
                      <span>
                        {t(`settings.page.updates.phase.${host.phase}`)}
                        {host.stagedVersion ? ` · ${host.stagedVersion}` : ""}
                      </span>
                      <Toggle
                        checked={host.idleAutoUpdate}
                        onChange={(idle) => onHostPolicy?.(host.hostId, idle)}
                        label={t("settings.page.updates.idleAuto")}
                      />
                    </span>
                  </Row>
                </Rows>
              ))
            : null}
        </div>
      ) : null}
    </div>
  );
}

export function layerTitle(_component: UpdateComponent): string {
  return _component;
}
