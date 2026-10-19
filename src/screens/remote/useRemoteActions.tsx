import { api } from "@/lib/api";
import { PendingKind } from './remoteHelpers';
import { useRemotePresentation } from './useRemotePresentation';
export function useRemoteActions(context: ReturnType<typeof useRemotePresentation>) {
  const {
    busyRef,
    selectedHostId,
    sshTrust,
    ensureSshTrust,
    setBusy,
    setFailure,
    retry,
    load,
    setSshTrust,
    inspectHost,
    setPending,
    primaryActionKey,
    trackOperation,
    ownOperation,
    workSurface,
  } = context;
  async function run(actionKey: string, work: () => Promise<void>) {
    if (busyRef.current !== null) return;
    if (selectedHostId && actionKey !== "remote.trust.confirm" && actionKey !== "remote.act.bundle") {
      const alreadyConfirmed = sshTrust?.hostId === selectedHostId && sshTrust.confirmed;
      const trusted = alreadyConfirmed || await ensureSshTrust(selectedHostId);
      if (!trusted) return;
    }
    busyRef.current = actionKey;
    setBusy(actionKey);
    setFailure(null);
    try {
      await work();
    } catch (cause) {
      setFailure({ actionKey, raw: String(cause) });
      retry.current = () => void run(actionKey, work);
      // 失敗後把狀態重讀一次。使用者需要知道主機**現在**是什麼樣子，
      // 而不是動手之前的那張快照 —— 這也讓「上面的狀態是最新的」這句話成立。
      void load();
    } finally {
      busyRef.current = null;
      setBusy(null);
    }
  }
  function confirmSshTrust() {
    if (!selectedHostId || !sshTrust?.fingerprint) return;
    const hostId = selectedHostId;
    const fingerprint = sshTrust.fingerprint.fingerprint;
    void run("remote.trust.confirm", async () => {
      try {
        await api.confirmRemoteSshFingerprint(hostId, fingerprint);
      } catch (cause) {
        setSshTrust((current) => (
          current && current.hostId === hostId
            ? { ...current, fetchError: String(cause) }
            : current
        ));
        throw cause;
      }
      setSshTrust({
        hostId,
        confirmed: true,
        checking: false,
        fingerprint: null,
        fetchError: null,
      });
      await inspectHost(hostId);
    });
  }
  function runPending(kind: PendingKind) {
    setPending(null);
    if (!selectedHostId) return;
    switch (kind) {
      case "bootstrap":
        return void run(primaryActionKey, async () => {
          trackOperation(await api.bootstrapRemoteHost(selectedHostId));
        });
      case "restore":
        return void run("remote.act.restore", async () => {
          trackOperation(await api.restoreRemoteHost(selectedHostId));
        });
      case "restartNative":
        return void run("remote.act.restartNative", async () => {
          await api.restartRemoteNativeCodex(selectedHostId, `restart-${Date.now()}`);
          await load();
        });
      case "installCodex":
        return void run("remote.act.installCodex", async () => {
          await api.installRemotePinnedCodex(selectedHostId, `install-codex-${Date.now()}`);
          await load();
        });
      case "updateAgent":
        return void run("remote.act.updateAgent", async () => {
          await api.updateRemoteComponents(selectedHostId, `update-agent-${Date.now()}`);
          await load();
        });
      case "syncDesktopCodex":
        return void run("remote.act.syncDesktopCodex", async () => {
          trackOperation(await api.updateRemoteCodexForDesktop(selectedHostId));
        });
      case "stopAppOwned":
        return void run("remote.act.stopAppOwned", async () => {
          await api.stopRemoteAppOwnedCodex(selectedHostId, `stop-app-owned-${Date.now()}`);
          trackOperation(await api.bootstrapRemoteHost(selectedHostId));
        });
    }
  }
  function exportBundle() {
    if (!selectedHostId) return;
    void run("remote.act.bundle", async () => { await api.createRemoteSupportBundle(selectedHostId); });
  }
  /**
   * 重新部署是收斂：可重複執行、會等主機閒置、隨時可以取消，所以不再每次跳確認。
   * 只有第一次部署會先列出「會改／不動」，讓人知道這台主機上會多出什麼。
   */
  function deploy() {
    if (workSurface.primaryKey === "remoteWork.redeploy") return runPending("bootstrap");
    return setPending("bootstrap");
  }
  function retryOperation() {
    if (ownOperation?.kind === "oneClickRestore") return setPending("restore");
    return deploy();
  }
  return {
    ...context,
    run,
    confirmSshTrust,
    runPending,
    exportBundle,
    deploy,
    retryOperation,
  };
}
