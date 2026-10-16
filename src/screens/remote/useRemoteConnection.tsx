import { useCallback } from "react";
import { api } from "@/lib/api";
import type { RemoteOperationProgress } from "@/types";
import { useRemoteState } from './useRemoteState';
export function useRemoteConnection(context: ReturnType<typeof useRemoteState>) {
  const {
    operationStartedAt,
    setOperation,
    sshTrustRequests,
    setSshTrust,
    probeGenerations,
    setProbingHostId,
    setStatuses,
    setLastUpdated,
    setProbeErrors,
    setDiscoveryError,
    setDiscovering,
    setFeatureEnabled,
    setLegacyBrokerHosts,
    setRelease,
    setHosts,
    setSelectedHostId,
    setProbeVersion,
    refreshVersion,
    onRefreshComplete,
    setPairings,
    setExecutionAccounts,
  } = context;
  const trackOperation = useCallback((next: RemoteOperationProgress | null) => {
    if (next && !operationStartedAt.current.has(next.operationId)) {
      operationStartedAt.current.set(next.operationId, Date.now());
    }
    setOperation(next);
  }, []);
  const ensureSshTrust = useCallback((hostId: string): Promise<boolean> => {
    const existing = sshTrustRequests.current.get(hostId);
    if (existing)
      return existing;
    const request = (async () => {
      setSshTrust({ hostId, confirmed: false, checking: true, fingerprint: null, fetchError: null });
      try {
        const status = await api.getRemoteSshTrustStatus(hostId);
        if (status.confirmed) {
          setSshTrust({ hostId, confirmed: true, checking: false, fingerprint: status.fingerprint ?? null, fetchError: null, source: status.source });
          return true;
        }
        try {
          const fingerprint = status.fingerprint ?? await api.fetchRemoteSshFingerprint(hostId);
          setSshTrust({ hostId, confirmed: false, checking: false, fingerprint, fetchError: null });
        }
        catch (cause) {
          setSshTrust({
            hostId,
            confirmed: false,
            checking: false,
            fingerprint: null,
            fetchError: String(cause),
          });
        }
        return false;
      }
      catch (cause) {
        setSshTrust({
          hostId,
          confirmed: false,
          checking: false,
          fingerprint: null,
          fetchError: String(cause),
        });
        return false;
      }
    })();
    sshTrustRequests.current.set(hostId, request);
    void request.finally(() => {
      if (sshTrustRequests.current.get(hostId) === request) {
        sshTrustRequests.current.delete(hostId);
      }
    });
    return request;
  }, []);
  const inspectHost = useCallback(async (hostId: string) => {
    const trusted = await ensureSshTrust(hostId);
    if (!trusted)
      return;
    const myGeneration = (probeGenerations.current[hostId] ?? 0) + 1;
    probeGenerations.current[hostId] = myGeneration;
    // Never clear the previously-good status here: an already-probed host
    // being re-probed should keep showing what it last knew while the
    // refresh is in flight, not blank out and look frozen.
    setProbingHostId(hostId);
    try {
      const next = await api.inspectRemoteHost(hostId);
      if (probeGenerations.current[hostId] !== myGeneration)
        return;
      setStatuses((current) => ({ ...current, [hostId]: next }));
      setLastUpdated((current) => ({ ...current, [hostId]: Date.now() }));
      setProbeErrors((current) => {
        if (!(hostId in current))
          return current;
        const {
          ...rest,
        } = current;
        return rest;
      });
    }
    catch (cause) {
      if (probeGenerations.current[hostId] !== myGeneration)
        return;
      // 探測失敗是那一台主機的屬性，不是整頁的警報。標在它自己那一列上。
      // A host-key change is a hard fail: surface the backend error and never
      // offer an auto-accept button.
      setProbeErrors((current) => ({ ...current, [hostId]: String(cause) }));
    }
    finally {
      if (probeGenerations.current[hostId] === myGeneration) {
        setProbingHostId((current) => current === hostId ? null : current);
      }
    }
  }, [ensureSshTrust]);
  const load = useCallback(async () => {
    setDiscoveryError(null);
    setDiscovering(true);
    try {
      const [flags, releaseStatus] = await Promise.all([
        api.getRemoteManagerFeatureFlags(),
        api.getRemoteReleaseStatus(),
      ]);
      setFeatureEnabled(flags.nativeCodexRemoteManager);
      setLegacyBrokerHosts(flags.legacyBrokerHostsDropped ?? []);
      setRelease(releaseStatus);
      if (!flags.nativeCodexRemoteManager) {
        setHosts([]);
        setStatuses({});
        return;
      }
      const discovered = await api.discoverRemoteConnections();
      setHosts(discovered);
      setSelectedHostId((current) => current ?? discovered[0]?.vellumHostId ?? null);
      const discoveredIds = new Set(discovered.map((host) => host.vellumHostId));
      setStatuses((current) => Object.fromEntries(Object.entries(current).filter(([hostId]) => discoveredIds.has(hostId))));
      // Live SSH inspection is deferred until after discovery paints. Probe
      // only the selected host instead of blocking this tab on every alias.
      setProbeVersion((current) => current + 1);
    }
    catch (cause) {
      setDiscoveryError(String(cause));
    }
    finally {
      setDiscovering(false);
      if (refreshVersion > 0)
        onRefreshComplete(refreshVersion);
    }
  }, [refreshVersion, onRefreshComplete]);
  const loadPairings = useCallback(async (hostId: string) => {
    try {
      setPairings(await api.listRemoteCodexAccountPairings(hostId));
    }
    catch (cause) {
      setPairings([]);
      setDiscoveryError(String(cause));
    }
  }, []);
  const loadExecutionAccounts = useCallback(async (hostId: string) => {
    try {
      setExecutionAccounts(await api.listRemoteOfficialExecutionAccounts(hostId));
    }
    catch {
      // Older agents have no managed Official execution-account API.
      setExecutionAccounts([]);
    }
  }, []);
  return {
    ...context,
    trackOperation,
    ensureSshTrust,
    inspectHost,
    load,
    loadPairings,
    loadExecutionAccounts,
  };
}
