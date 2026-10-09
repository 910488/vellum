import type { VellumUpdateHosts, VellumUpdateState } from "@/components/VellumUpdate";
import type { LayerStatus, UpdatePhase, UpdateStatusSnapshot } from "@/types";

/**
 * 後端仍是 desktop／core／remote 三層，畫面只講一個 Vellum 版本號。
 * 版本號跟著 Desktop 走；core 是同一次更新順手帶下來的，它只在下載中
 * 與失敗時露面（沒有自己的版本號），remote 只出現在主機摘要那一列。
 *
 * 判斷順序就是使用者最需要知道的順序：不能自動更新 > 已排定重啟 >
 * 正在下載 > 正在檢查 > 等重啟 > 失敗 > 有新版 > 最新。
 */
const DOWNLOADING: UpdatePhase[] = ["downloading", "verifying"];
const READY: UpdatePhase[] = ["staged", "waitingForRestart"];
const BROKEN: UpdatePhase[] = ["failed", "blocked"];
const HOST_PENDING: UpdatePhase[] = [
  "available",
  "downloading",
  "verifying",
  "staged",
  "waitingForIdle",
  "applying",
  "validating",
];
const HOST_FAILED: UpdatePhase[] = ["failed", "rolledBack", "blocked"];

export function vellumUpdateState(
  snapshot: UpdateStatusSnapshot,
  local: { checking: boolean },
): VellumUpdateState {
  const { desktop, core } = snapshot;
  if (!snapshot.liveAutoUpdate) return { kind: "unsigned" };

  const schedule = snapshot.restartSchedule;
  if (schedule) {
    return {
      kind: "scheduled",
      version: schedule.version ?? desktop.stagedVersion ?? desktop.targetVersion ?? desktop.currentVersion,
      turns: schedule.openTurns,
      requests: schedule.activeRequests,
    };
  }

  const downloading = [desktop, core].filter((layer) => DOWNLOADING.includes(layer.phase));
  if (downloading.length) {
    const total = downloading.reduce((sum, layer) => sum + layer.downloadTotal, 0);
    const bytes = downloading.reduce((sum, layer) => sum + layer.downloadBytes, 0);
    return {
      kind: "downloading",
      version: DOWNLOADING.includes(desktop.phase) ? desktop.targetVersion ?? "" : "",
      percent: total > 0 ? Math.min(100, (bytes / total) * 100) : 0,
    };
  }

  if (local.checking || desktop.phase === "checking") return { kind: "checking" };

  if (READY.includes(desktop.phase)) {
    return { kind: "ready", version: desktop.stagedVersion ?? desktop.targetVersion ?? "" };
  }

  const broken = [desktop, core].find((layer) => BROKEN.includes(layer.phase));
  if (broken) {
    return {
      kind: "failed",
      // Core versions are numbered on their own; showing one beside the
      // Vellum version would read as a different Vellum.
      version: broken.component === "desktop" ? broken.targetVersion : null,
      reason: broken.failureReason ?? "unknown",
    };
  }

  if (desktop.phase === "available" && desktop.availableVersion) {
    return { kind: "available", version: desktop.availableVersion };
  }

  return {
    kind: "current",
    checkedAt: snapshot.checkedAt ? new Date(snapshot.checkedAt * 1000).toISOString() : null,
  };
}

export function vellumUpdateHosts(remote: LayerStatus): VellumUpdateHosts {
  return {
    total: remote.hosts.length,
    pending: remote.hosts.filter((host) => HOST_PENDING.includes(host.phase)).length,
    failed: remote.hosts.filter((host) => HOST_FAILED.includes(host.phase)).length,
  };
}

/** Automatic updates is one switch over the backend's check + download pair. */
export function autoUpdateOn(snapshot: UpdateStatusSnapshot): boolean {
  return snapshot.preferences.autoCheck && snapshot.preferences.autoDownload;
}
