import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { useLocaleFormat } from "@/i18n/useLocaleFormat";
import { useHandoffRecords } from "@/lib/handoffStore";
import type { AddedRemoteHost } from "@/types";
import { handoffReviewTarget } from "@/lib/handoffNavigation";
import type { DesktopCodexCompatibilityStatus, PendingHostFingerprint, RemoteChatGptAccountPairing, RemoteChatGptAccountLogin, RemoteControlPairing, RemoteOfficialExecutionAccount, RemoteOfficialExecutionAccountLogin, RemoteHostCandidate, RemoteHostStatus, RemoteOperationProgress, RemoteReleaseStatus, RemoteSessionSummary } from "@/types";
import { PendingKind } from './remoteHelpers';
export type RemoteProps = {
  refreshVersion: number;
  onRefreshComplete: (version: number) => void;
  active?: boolean;
};
export function useRemoteState({ refreshVersion, onRefreshComplete, active = true }: RemoteProps) {
  const { t } = useTranslation();
  const { locale } = useLocaleFormat();
  const handoffs = useHandoffRecords();
  const [hosts, setHosts] = useState<RemoteHostCandidate[]>([]);
  const [addedHost, setAddedHost] = useState<AddedRemoteHost | null>(null);
  const [statuses, setStatuses] = useState<Record<string, RemoteHostStatus>>({});
  const [selectedHostId, setSelectedHostId] = useState<string | null>(() => handoffReviewTarget()?.host ?? null);
  useEffect(() => {
    const review = (event: Event) => {
      setSelectedHostId((event as CustomEvent<{ host: string }>).detail.host);
      setFailure(null);
    };
    window.addEventListener("vellum:handoff-review", review);
    return () => window.removeEventListener("vellum:handoff-review", review);
  }, []);
  const [modelObservation, setModelObservation] = useState<{
    hostId: string;
    models: import("@/types").RemoteModelOption[];
  } | null>(null);
  const [operation, setOperation] = useState<RemoteOperationProgress | null>(null);
  const [sessions, setSessions] = useState<RemoteSessionSummary | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  // 三條錯誤通道，刻意不共用一個 state。上一版全部塞進同一個 `error`，
  // 結果背景探測失敗會把使用者剛按出來的動作失敗蓋掉。
  const [discoveryError, setDiscoveryError] = useState<string | null>(null);
  const [failure, setFailure] = useState<{ actionKey: string; raw: string } | null>(null);
  const [probeErrors, setProbeErrors] = useState<Record<string, string>>({});
  const [featureEnabled, setFeatureEnabled] = useState<boolean | null>(null);
  const [release, setRelease] = useState<RemoteReleaseStatus | null>(null);
  const [desktopCodex, setDesktopCodex] = useState<DesktopCodexCompatibilityStatus | null>(null);
  const [discovering, setDiscovering] = useState(true);
  const [probingHostId, setProbingHostId] = useState<string | null>(null);
  const [lastUpdated, setLastUpdated] = useState<Record<string, number>>({});
  // Per-host probe generation. `inspectHost` bumps this before it awaits the
  // RPC and only applies the result if nothing bumped it again in the
  // meantime — otherwise a slow response to an old probe (e.g. after
  // rapidly switching away and back to a host) could land after, and
  // clobber, a newer one that already resolved.
  const probeGenerations = useRef<Record<string, number>>({});
  const [chatgptLogin, setChatgptLogin] = useState<RemoteChatGptAccountLogin | null>(null);
  const [pairings, setPairings] = useState<RemoteChatGptAccountPairing[]>([]);
  /* 批次配對還沒走到的帳號。每個帳號都要在這台主機上核可一次 —— 不是儀式，是
     OAuth 的 refresh token 會輪替且伺服器端偵測重用，一份 grant 沒辦法給兩個
     客戶端共用。所以隊列的作用是把 N 趟合成一趟，不是把核可省掉。
     權威值放 ref：推進隊列的是輪詢 effect 裡的 callback，讀 state 會讀到建立
     那一輪的舊值。state 只負責畫「還剩幾個」。 */
  const pairQueue = useRef<string[]>([]);
  const pairQueueRestoreAccount = useRef<string | null>(null);
  const [pairQueueLength, setPairQueueLength] = useState(0);
  const [resumeBootstrapAfterPairing, setResumeBootstrapAfterPairing] = useState(false);
  const [takeoverPrompt, setTakeoverPrompt] = useState<{ hostId: string; operationId: string; token: string } | null>(null);
  // Remote control identity A stays synchronized with Desktop. Official
  // execution identities B belong only to Vellum proxy authorization.
  const [executionAccounts, setExecutionAccounts] = useState<RemoteOfficialExecutionAccount[]>([]);
  const [executionLogin, setExecutionLogin] = useState<RemoteOfficialExecutionAccountLogin | null>(null);
  const [executionDisplayName, setExecutionDisplayName] = useState("");
  const [devicePairing, setDevicePairing] = useState<RemoteControlPairing | null>(null);
  const [probeVersion, setProbeVersion] = useState(0);
  const [pending, setPending] = useState<PendingKind | null>(null);
  const [legacyBrokerHosts, setLegacyBrokerHosts] = useState<string[]>([]);
  const [sshTrust, setSshTrust] = useState<{
    hostId: string;
    confirmed: boolean;
    checking: boolean;
    fingerprint: PendingHostFingerprint | null;
    fetchError: string | null;
    source?: string | null;
  } | null>(null);
  // Discovery and a refresh can converge on the same host in one render.
  // Share their trust check so an unconfirmed host gets one keyscan rather
  // than a small burst of identical pre-auth connections.
  const sshTrustRequests = useRef(new Map<string, Promise<boolean>>());
  const retry = useRef<(() => void) | null>(null);
  // State updates do not become visible until React renders again. Keep a
  // synchronous lock as well so two clicks in the same frame cannot launch
  // duplicate SSH/CLI operations.
  const busyRef = useRef<string | null>(null);
  // 起算點取前端第一次看到這個 operation 的時間。後端的 updatedAt 每次
  // 輪詢都會變，拿它算等於永遠顯示 0 秒。
  const operationStartedAt = useRef(new Map<string, number>());
  return {
    t,
    locale,
    handoffs,
    hosts,
    setHosts,
    addedHost,
    setAddedHost,
    statuses,
    setStatuses,
    selectedHostId,
    setSelectedHostId,
    modelObservation,
    setModelObservation,
    operation,
    setOperation,
    sessions,
    setSessions,
    busy,
    setBusy,
    discoveryError,
    setDiscoveryError,
    failure,
    setFailure,
    probeErrors,
    setProbeErrors,
    featureEnabled,
    setFeatureEnabled,
    release,
    setRelease,
    desktopCodex,
    setDesktopCodex,
    discovering,
    setDiscovering,
    probingHostId,
    setProbingHostId,
    lastUpdated,
    setLastUpdated,
    probeGenerations,
    chatgptLogin,
    setChatgptLogin,
    pairings,
    setPairings,
    pairQueue,
    pairQueueRestoreAccount,
    pairQueueLength,
    setPairQueueLength,
    resumeBootstrapAfterPairing,
    setResumeBootstrapAfterPairing,
    takeoverPrompt,
    setTakeoverPrompt,
    executionAccounts,
    setExecutionAccounts,
    executionLogin,
    setExecutionLogin,
    executionDisplayName,
    setExecutionDisplayName,
    devicePairing,
    setDevicePairing,
    probeVersion,
    setProbeVersion,
    pending,
    setPending,
    legacyBrokerHosts,
    setLegacyBrokerHosts,
    sshTrust,
    setSshTrust,
    sshTrustRequests,
    retry,
    busyRef,
    operationStartedAt,
    refreshVersion,
    onRefreshComplete,
    active,
  };
}
