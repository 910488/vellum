import { useEffect } from "react";
import { api } from "@/lib/api";
import { startVisiblePoll } from "@/lib/visiblePoll";
import { useRemoteAccounts } from './useRemoteAccounts';
export function useRemotePolling(context: ReturnType<typeof useRemoteAccounts>) {
  const {
    load,
    selectedHostId,
    inspectHost,
    probeVersion,
    setPairings,
    loadPairings,
    statuses,
    setDesktopCodex,
    operation,
    trackOperation,
    setDiscoveryError,
    active,
    setSessions,
    setExecutionAccounts,
    loadExecutionAccounts,
    executionLogin,
    setExecutionLogin,
    chatgptLogin,
    setChatgptLogin,
    advancePairQueue,
    resumeBootstrapAfterPairing,
    setResumeBootstrapAfterPairing,
    pairQueue,
    pairQueueRestoreAccount,
    setPairQueueLength,
    setTakeoverPrompt,
    waitControls,
  } = context;
  useEffect(() => { void load(); }, [load]);
  useEffect(() => {
    if (!selectedHostId) return;
    void inspectHost(selectedHostId);
  }, [selectedHostId, probeVersion, inspectHost]);
  /* Desktop 換 ChatGPT 帳號時只推這一台。記在後端而不是這裡的 state，因為切帳號
     是在設定頁按的，那時這個畫面通常已經被卸載了。 */
  useEffect(() => { void api.setActiveRemoteHost(selectedHostId); }, [selectedHostId]);
  useEffect(() => {
    if (!selectedHostId) {
      setPairings([]);
      return;
    }
    void loadPairings(selectedHostId);
  }, [selectedHostId, probeVersion, loadPairings]);
  useEffect(() => {
    if (!selectedHostId || !statuses[selectedHostId]?.inventory) {
      setDesktopCodex(null);
      return;
    }
    let cancelled = false;
    api.getDesktopCodexCompatibility(selectedHostId)
      .then((next) => { if (!cancelled) setDesktopCodex(next); })
      .catch((cause) => {
        if (!cancelled) {
          setDesktopCodex({
            state: "unavailable",
            desktopVersion: null,
            desktopSchemaSha256: null,
            remoteVersion: null,
            requiredRemoteVersion: null,
            remoteArch: null,
            agentProtocol: null,
            canUpdate: false,
            detail: String(cause),
          });
        }
      });
    return () => { cancelled = true; };
  }, [selectedHostId, statuses, probeVersion]);
  useEffect(() => {
    if (!operation || operation.state !== "running") return;
    return startVisiblePoll({
      active,
      intervalMs: 1000,
      load: async () => {
        await api.getRemoteOperation(operation.operationId).then((next) => {
          trackOperation(next);
          if (next.state !== "running") void load();
        }).catch((cause) => setDiscoveryError(String(cause)));
      },
    });
  }, [operation, load, trackOperation, active]);
  useEffect(() => {
    const current = selectedHostId ? statuses[selectedHostId]?.agent?.grok : null;
    if (!selectedHostId || !current?.loginPending) return;
    return startVisiblePoll({
      active,
      intervalMs: 2000,
      load: async () => {
        await api.pollRemoteGrokLogin(selectedHostId).then(() => load()).catch((cause) => setDiscoveryError(String(cause)));
      },
    });
  }, [selectedHostId, statuses, load, active]);
  useEffect(() => {
    if (!selectedHostId) {
      setSessions(null);
      return;
    }
    let cancelled = false;
    api.getRemoteSessionSummary(selectedHostId)
      .then((next) => { if (!cancelled) setSessions(next); })
      .catch(() => { if (!cancelled) setSessions(null); });
    return () => { cancelled = true; };
  }, [selectedHostId, probeVersion]);
  useEffect(() => {
    if (!selectedHostId) {
      setExecutionAccounts([]);
      return;
    }
    void loadExecutionAccounts(selectedHostId);
  }, [selectedHostId, probeVersion, loadExecutionAccounts]);
  useEffect(() => {
    if (!selectedHostId || !executionLogin) return;
    return startVisiblePoll({
      active,
      intervalMs: Math.max(2000, executionLogin.interval * 1000),
      load: async () => {
        await api
          .pollRemoteOfficialExecutionAccountLogin(selectedHostId, executionLogin.loginId)
          .then(async (next) => {
            if (next.state === "authenticated") {
              setExecutionLogin(null);
              await loadExecutionAccounts(selectedHostId);
            }
          })
          .catch((cause) => setDiscoveryError(String(cause)));
      },
    });
  }, [selectedHostId, executionLogin, loadExecutionAccounts, active]);
  useEffect(() => {
    if (!selectedHostId || !chatgptLogin) return;
    return startVisiblePoll({
      active,
      intervalMs: 2000,
      load: async () => {
        await api.pollRemoteCodexAccountLogin(selectedHostId).then(async (next) => {
          if (next.state !== "synchronized") return;
          setChatgptLogin(null);
          if (await advancePairQueue(selectedHostId)) return;
          await loadPairings(selectedHostId);
          await inspectHost(selectedHostId);
          if (resumeBootstrapAfterPairing) {
            setResumeBootstrapAfterPairing(false);
            trackOperation(await api.bootstrapRemoteHost(selectedHostId));
          }
        }).catch((cause) => {
          setChatgptLogin(null);
          pairQueue.current = [];
          pairQueueRestoreAccount.current = null;
          setPairQueueLength(0);
          setDiscoveryError(String(cause));
        });
      },
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedHostId, chatgptLogin, inspectHost, loadPairings, resumeBootstrapAfterPairing, trackOperation, active]);
  useEffect(() => {
    setTakeoverPrompt((current) => current && (current.hostId !== selectedHostId || current.token !== waitControls.token) ? null : current);
  }, [selectedHostId, waitControls.token]);
}
