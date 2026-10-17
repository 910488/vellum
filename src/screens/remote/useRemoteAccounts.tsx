import { api } from "@/lib/api";
import { resumeConvergeAfterAccount } from "@/lib/remoteVocabulary";
import { useRemoteActions } from './useRemoteActions';
export function useRemoteAccounts(context: ReturnType<typeof useRemoteActions>) {
  const {
    selectedHostId,
    pairQueue,
    setPairQueueLength,
    pairQueueRestoreAccount,
    activeControlAccount,
    run,
    setResumeBootstrapAfterPairing,
    operation,
    setChatgptLogin,
    pairings,
    loadPairings,
    inspectHost,
    trackOperation,
    executionDisplayName,
    setExecutionLogin,
    setExecutionDisplayName,
    loadExecutionAccounts,
    setDevicePairing,
  } = context;
  function pairChatGpt(accountId?: string, makeActive = true) {
    if (!selectedHostId) return;
    if (!accountId) {
      pairQueue.current = [];
      setPairQueueLength(0);
    }
    pairQueueRestoreAccount.current = makeActive
      ? accountId ?? null
      : activeControlAccount?.accountId ?? null;
    void run("remote.act.chatgptPair", async () => {
      const login = await api.startRemoteCodexAccountLogin(selectedHostId, accountId);
      setResumeBootstrapAfterPairing(resumeConvergeAfterAccount(operation, selectedHostId));
      setChatgptLogin(login);
      window.open(login.verificationUrl, "_blank", "noopener,noreferrer");
    });
  }
  /**
   * 開始下一個排隊中的帳號，回傳「還有沒有下一個」。
   *
   * 隊列空掉時把 daemon 切回 Desktop 目前的預設帳號：批次配對的最後一步會把
   * auth.json 停在最後配對的那一個，而那通常不是使用者選的那一個。
   */
  async function advancePairQueue(hostId: string): Promise<boolean> {
    const next = pairQueue.current.shift();
    setPairQueueLength(pairQueue.current.length);
    if (!next) {
      const restoreAccount = pairQueueRestoreAccount.current;
      pairQueueRestoreAccount.current = null;
      if (restoreAccount) {
        try { await api.activateRemoteCodexAccount(hostId, restoreAccount); } catch { /* restoration is best effort */ }
      }
      return false;
    }
    const login = await api.startRemoteCodexAccountLogin(hostId, next);
    setChatgptLogin(login);
    window.open(login.verificationUrl, "_blank", "noopener,noreferrer");
    return true;
  }
  /** 把 Desktop 上尚未在這台主機配對的帳號排成一列，一次帶完。 */
  function pairAllChatGptAccounts() {
    if (!selectedHostId) return;
    const pending = pairings
      .filter((row) => ((!row.paired && !row.active) || row.credentialState === "expired") && !row.detail)
      .map((row) => row.accountId);
    if (!pending.length) return;
    pairQueueRestoreAccount.current = activeControlAccount?.accountId ?? null;
    pairQueue.current = pending;
    setPairQueueLength(pending.length);
    setResumeBootstrapAfterPairing(resumeConvergeAfterAccount(operation, selectedHostId));
    void run("remote.act.chatgptPairAll", async () => {
      await advancePairQueue(selectedHostId);
    });
  }
  /** 跳過這一個帳號，直接走下一個。之後隨時可以從這張卡片單獨補配對。 */
  function skipPairing() {
    if (!selectedHostId) return;
    setChatgptLogin(null);
    void run("remote.act.chatgptPairSkip", async () => {
      if (!await advancePairQueue(selectedHostId)) await loadPairings(selectedHostId);
    });
  }
  function activateChatGpt(accountId?: string) {
    if (!selectedHostId) return;
    void run("remote.act.chatgptActivate", async () => {
      await api.activateRemoteCodexAccount(selectedHostId, accountId);
      await loadPairings(selectedHostId);
      await inspectHost(selectedHostId);
      if (!accountId && resumeConvergeAfterAccount(operation, selectedHostId)) {
        trackOperation(await api.bootstrapRemoteHost(selectedHostId));
      }
    });
  }
  function selectRemoteControlAccount(accountId: string) {
    const account = pairings.find((row) => row.accountId === accountId);
    if (!account || account.detail) return;
    if ((!account.paired && !account.active) || account.credentialState === "expired") {
      pairChatGpt(account.accountId, true);
      return;
    }
    if (!account.active) activateChatGpt(account.accountId);
  }
  function addOfficialExecutionAccount() {
    if (!selectedHostId) return;
    const name = executionDisplayName.trim();
    if (!name) return;
    void run("remote.act.executionLogin", async () => {
      const login = await api.startRemoteOfficialExecutionAccountLogin(selectedHostId, name);
      setExecutionLogin(login);
      setExecutionDisplayName("");
      window.open(login.verificationUrl, "_blank", "noopener,noreferrer");
    });
  }
  function selectOfficialExecutionAccount(accountIdHash: string) {
    if (!selectedHostId) return;
    void run("remote.act.executionSelect", async () => {
      await api.selectRemoteOfficialExecutionAccount(selectedHostId, accountIdHash);
      await loadExecutionAccounts(selectedHostId);
    });
  }
  function selectOfficialExecutionRoute(value: string) {
    if (!selectedHostId) return;
    if (value === "control") {
      void run("remote.act.executionFollowControl", async () => {
        await api.clearRemoteOfficialExecutionAccountSelection(selectedHostId);
        await loadExecutionAccounts(selectedHostId);
      });
      return;
    }
    selectOfficialExecutionAccount(value);
  }
  function removeOfficialExecutionAccount(accountIdHash: string) {
    if (!selectedHostId) return;
    void run("remote.act.executionRemove", async () => {
      await api.removeRemoteOfficialExecutionAccount(selectedHostId, accountIdHash);
      await loadExecutionAccounts(selectedHostId);
    });
  }
  function pairMobileDevice() {
    if (!selectedHostId) return;
    void run("remote.act.devicePair", async () => {
      setDevicePairing(await api.startRemoteControlPairing(selectedHostId));
    });
  }
  return {
    ...context,
    pairChatGpt,
    advancePairQueue,
    pairAllChatGptAccounts,
    skipPairing,
    activateChatGpt,
    selectRemoteControlAccount,
    addOfficialExecutionAccount,
    selectOfficialExecutionAccount,
    selectOfficialExecutionRoute,
    removeOfficialExecutionAccount,
    pairMobileDevice,
  };
}
