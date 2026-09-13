/**
 * 遠端總管。
 *
 * 版面的重點是「工作面」：一塊釘在頂端不捲走的區塊，永遠是
 * 「這台現在怎樣、我該按什麼」的答案。它有四個狀態 —— 待部署、就緒、
 * 進行中、失敗 —— 而且進行中與失敗都是**原地**取代按鈕。
 *
 * 上一版是資料在上、動作在中、結果在下：按下部署的當下，進度卡在
 * 十九行狀態與兩張卡以下，眼睛要跑三個地方才能把因果接起來。
 * 剩下的東西（主機細節、維護、執行緒、危險區）全部退到工作面下方，
 * 都是可以捲走的參考資料。
 */
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { api } from "@/lib/api";
import { startVisiblePoll } from "@/lib/visiblePoll";
import { Btn, Cap, Card, Confirm, Empty, Meter, Notice, Row, Rows, State, Tray } from "@/components/ui";
import {
  configurationStateKey,
  elapsedLabel,
  hostVerdict,
  operationKindKey,
  phaseKey,
  visibleBlockers,
  type RemoteRemedy,
} from "@/lib/remoteVocabulary";
import type {
  DesktopCodexCompatibilityStatus,
  PendingHostFingerprint,
  RemoteDeploymentPlan,
  RemoteChatGptAccountPairing,
  RemoteChatGptAccountLogin,
  RemoteControlPairing,
  RemoteOfficialExecutionAccount,
  RemoteOfficialExecutionAccountLogin,
  RemoteHostCandidate,
  RemoteHostStatus,
  RemoteOperationProgress,
  RemoteReleaseStatus,
  RemoteSessionSummary,
} from "@/types";

function formatBytes(value: number | null | undefined, unknown: string): string {
  if (value == null) return unknown;
  const units = ["B", "KB", "MB", "GB", "TB"];
  let index = 0;
  let current = value;
  while (current >= 1024 && index < units.length - 1) {
    current /= 1024;
    index += 1;
  }
  return `${current.toFixed(current >= 100 ? 0 : 1)} ${units[index]}`;
}

/** 需要確認才動手的動作。每一個都對應一個固定三格事實的確認視窗。 */
type PendingKind =
  | "bootstrap"
  | "restore"
  | "restartNative"
  | "installCodex"
  | "updateAgent"
  | "syncDesktopCodex"
  | "stopAppOwned";

export function Remote({
  refreshVersion,
  onRefreshComplete,
  active = true,
}: {
  refreshVersion: number;
  onRefreshComplete: (version: number) => void;
  active?: boolean;
}) {
  const { t } = useTranslation();
  const [hosts, setHosts] = useState<RemoteHostCandidate[]>([]);
  const [statuses, setStatuses] = useState<Record<string, RemoteHostStatus>>({});
  const [selectedHostId, setSelectedHostId] = useState<string | null>(null);
  const [plan, setPlan] = useState<RemoteDeploymentPlan | null>(null);
  const [selectedModels, setSelectedModels] = useState<string[]>([]);
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
  const [pairQueueLength, setPairQueueLength] = useState(0);
  const [resumeBootstrapAfterPairing, setResumeBootstrapAfterPairing] = useState(false);
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
  } | null>(null);
  const retry = useRef<(() => void) | null>(null);
  // State updates do not become visible until React renders again. Keep a
  // synchronous lock as well so two clicks in the same frame cannot launch
  // duplicate SSH/CLI operations.
  const busyRef = useRef<string | null>(null);
  // 起算點取前端第一次看到這個 operation 的時間。後端的 updatedAt 每次
  // 輪詢都會變，拿它算等於永遠顯示 0 秒。
  const operationStartedAt = useRef(new Map<string, number>());

  const trackOperation = useCallback((next: RemoteOperationProgress | null) => {
    if (next && !operationStartedAt.current.has(next.operationId)) {
      operationStartedAt.current.set(next.operationId, Date.now());
    }
    setOperation(next);
  }, []);

  const ensureSshTrust = useCallback(async (hostId: string): Promise<boolean> => {
    setSshTrust({ hostId, confirmed: false, checking: true, fingerprint: null, fetchError: null });
    try {
      const status = await api.getRemoteSshTrustStatus(hostId);
      if (status.confirmed) {
        setSshTrust({ hostId, confirmed: true, checking: false, fingerprint: null, fetchError: null });
        return true;
      }
      try {
        const fingerprint = await api.fetchRemoteSshFingerprint(hostId);
        setSshTrust({ hostId, confirmed: false, checking: false, fingerprint, fetchError: null });
      } catch (cause) {
        setSshTrust({
          hostId,
          confirmed: false,
          checking: false,
          fingerprint: null,
          fetchError: String(cause),
        });
      }
      return false;
    } catch (cause) {
      setSshTrust({
        hostId,
        confirmed: false,
        checking: false,
        fingerprint: null,
        fetchError: String(cause),
      });
      return false;
    }
  }, []);

  const inspectHost = useCallback(async (hostId: string) => {
    const trusted = await ensureSshTrust(hostId);
    if (!trusted) return;
    const myGeneration = (probeGenerations.current[hostId] ?? 0) + 1;
    probeGenerations.current[hostId] = myGeneration;
    // Never clear the previously-good status here: an already-probed host
    // being re-probed should keep showing what it last knew while the
    // refresh is in flight, not blank out and look frozen.
    setProbingHostId(hostId);
    try {
      const next = await api.inspectRemoteHost(hostId);
      if (probeGenerations.current[hostId] !== myGeneration) return;
      setStatuses((current) => ({ ...current, [hostId]: next }));
      setLastUpdated((current) => ({ ...current, [hostId]: Date.now() }));
      setProbeErrors((current) => {
        if (!(hostId in current)) return current;
        const { [hostId]: _dropped, ...rest } = current;
        return rest;
      });
    } catch (cause) {
      if (probeGenerations.current[hostId] !== myGeneration) return;
      // 探測失敗是那一台主機的屬性，不是整頁的警報。標在它自己那一列上。
      // A host-key change is a hard fail: surface the backend error and never
      // offer an auto-accept button.
      setProbeErrors((current) => ({ ...current, [hostId]: String(cause) }));
    } finally {
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
      setStatuses((current) => Object.fromEntries(
        Object.entries(current).filter(([hostId]) => discoveredIds.has(hostId)),
      ));
      // Live SSH inspection is deferred until after discovery paints. Probe
      // only the selected host instead of blocking this tab on every alias.
      setProbeVersion((current) => current + 1);
    } catch (cause) {
      setDiscoveryError(String(cause));
    } finally {
      setDiscovering(false);
      if (refreshVersion > 0) onRefreshComplete(refreshVersion);
    }
  }, [refreshVersion, onRefreshComplete]);

  useEffect(() => { void load(); }, [load]);

  useEffect(() => {
    if (!selectedHostId) return;
    void inspectHost(selectedHostId);
  }, [selectedHostId, probeVersion, inspectHost]);

  /* Desktop 換 ChatGPT 帳號時只推這一台。記在後端而不是這裡的 state，因為切帳號
     是在設定頁按的，那時這個畫面通常已經被卸載了。 */
  useEffect(() => { void api.setActiveRemoteHost(selectedHostId); }, [selectedHostId]);

  const loadPairings = useCallback(async (hostId: string) => {
    try {
      setPairings(await api.listRemoteCodexAccountPairings(hostId));
    } catch (cause) {
      setPairings([]);
      setDiscoveryError(String(cause));
    }
  }, []);

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
      load: () => {
        void api.getRemoteOperation(operation.operationId).then((next) => {
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
      load: () => {
        void api.pollRemoteGrokLogin(selectedHostId).then(() => load()).catch((cause) => setDiscoveryError(String(cause)));
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

  const loadExecutionAccounts = useCallback(async (hostId: string) => {
    try {
      setExecutionAccounts(await api.listRemoteOfficialExecutionAccounts(hostId));
    } catch {
      // Older agents have no managed Official execution-account API.
      setExecutionAccounts([]);
    }
  }, []);

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
      load: () => {
        void api
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
      load: () => {
        void api.pollRemoteCodexAccountLogin(selectedHostId).then(async (next) => {
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
          setPairQueueLength(0);
          setDiscoveryError(String(cause));
        });
      },
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedHostId, chatgptLogin, inspectHost, loadPairings, resumeBootstrapAfterPairing, trackOperation, active]);

  const host = hosts.find((item) => item.vellumHostId === selectedHostId) ?? null;
  const status = selectedHostId ? statuses[selectedHostId] ?? null : null;
  const native = status?.agent?.nativeCodex ?? null;
  const proxy = status?.agent?.proxy ?? null;
  const configuration = status?.agent?.configuration ?? null;
  const configurationKey = configurationStateKey(configuration?.state);
  const grok = status?.agent?.grok ?? null;
  const chatgpt = status?.chatgpt ?? null;
  const inventory = status?.inventory ?? null;
  const unknown = t("common.unknown");
  const operationRunning = operation?.state === "running";
  const proxyImageUpdateAvailable = Boolean(
    release?.ready
      && release.proxyImage
      && proxy?.image
      && release.proxyImage !== proxy.image,
  );
  const canApply = Boolean(plan && !plan.blockedReasons.length && selectedHostId && !busy && !operationRunning);
  const selectionChanged = useMemo(
    () => Boolean(plan && [...selectedModels].sort().join("\0") !== [...plan.selectedCatalogIds].sort().join("\0")),
    [plan, selectedModels],
  );

  const verdict = hostVerdict({
    managerState: status?.managerState ?? "unmanaged",
    agentReachable: Boolean(status?.agent),
  });
  const blockers = visibleBlockers(
    status?.blockedReasons ?? [],
    (status?.managerState ?? "unmanaged") === "unmanaged",
  );
  // 工作面只顯示屬於這一台的作業。跑在別台上的用一句話提示，不假裝是這裡的事。
  const ownOperation = operation && operation.hostId === selectedHostId ? operation : null;
  const strayOperation = operation && operation.hostId !== selectedHostId && operationRunning
    ? operation
    : null;
  const primaryActionKey = verdict.primary === "bootstrap"
    ? "remote.act.bootstrap"
    : "remote.act.reconverge";
  // 只翻譯認得的狀態。後端多出一個新的值時顯示原碼，不假裝看得懂。
  const CHATGPT_STATES = ["synchronized", "pairingRequired", "pairingPending", "activationRequired", "desktopAccountUnavailable"];
  const chatgptStateLabel = !chatgpt
    ? t("remote.account.chatgptUnavailable")
    : CHATGPT_STATES.includes(chatgpt.state)
      ? t(`remote.account.chatgpt.${chatgpt.state}`)
      : chatgpt.state;

  async function run(actionKey: string, work: () => Promise<void>) {
    if (busyRef.current !== null) return;
    if (selectedHostId && actionKey !== "remote.trust.confirm") {
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

  async function createPlan(ids = selectedModels) {
    if (!selectedHostId) return;
    const next = await api.planRemoteDeployment(selectedHostId, { catalogIds: ids, policy: {} });
    setPlan(next);
    setSelectedModels(next.selectedCatalogIds);
  }

  /**
   * Rebuild the plan from this host's own saved model selection and policy
   * overrides — the path a local-only settings change (Auto Review policy,
   * with the model selection itself untouched) must use. `createPlan`
   * always sends `policy: {}`, so using it here would silently reset this
   * host's own `autoReviewEnabled` override back to enabled on every
   * "re-plan" click.
   */
  async function reapplyPlan() {
    if (!selectedHostId) return;
    const next = await api.reapplyRemoteDeployment(selectedHostId);
    setPlan(next);
    setSelectedModels(next.selectedCatalogIds);
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
   * 卡住的原因配一顆補救按鈕。「發生什麼事」跟「怎麼辦」分兩個地方放，
   * 等於要人自己把兩件事接起來。
   */
  function remedyButton(remedy: RemoteRemedy | null): ReactNode {
    if (!remedy || !selectedHostId) return null;
    switch (remedy) {
      case "stopAppOwned":
        return <Btn mini soft onClick={() => setPending("stopAppOwned")} disabled={busy !== null}>{t("remote.act.stopAppOwned")}</Btn>;
      case "pairChatGpt":
        return <Btn mini soft onClick={() => pairChatGpt()} disabled={busy !== null || Boolean(chatgptLogin)}>{t("remote.act.chatgptPair")}</Btn>;
      case "activateChatGpt":
        return <Btn mini soft onClick={() => activateChatGpt()} disabled={busy !== null}>{t("remote.act.chatgptActivate")}</Btn>;
      case "repair":
        return <Btn mini soft onClick={repair} disabled={busy !== null}>{t("remote.act.repair")}</Btn>;
      case "installCodex":
        return <Btn mini soft onClick={() => setPending("installCodex")} disabled={busy !== null || !release?.ready}>{t("remote.act.installCodex")}</Btn>;
      case "updateAgent":
        return <Btn mini soft onClick={() => setPending("updateAgent")} disabled={busy !== null || !release?.ready}>{t("remote.act.updateAgent")}</Btn>;
      case "plan":
        return <Btn mini soft onClick={() => void run("remote.act.plan", () => createPlan([]))} disabled={busy !== null}>{t("remote.act.plan")}</Btn>;
      case "bootstrap":
        return <Btn mini soft onClick={() => setPending("bootstrap")} disabled={busy !== null || operationRunning}>{t(primaryActionKey)}</Btn>;
    }
  }

  function pairChatGpt(accountId?: string) {
    if (!selectedHostId) return;
    void run("remote.act.chatgptPair", async () => {
      const login = await api.startRemoteCodexAccountLogin(selectedHostId, accountId);
      setResumeBootstrapAfterPairing(Boolean(operation?.message?.includes("officialAccount")));
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
      const preferred = pairings.find((row) => row.isDesktopDefault);
      if (preferred) await api.activateRemoteCodexAccount(hostId, preferred.accountId).catch(() => {});
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
    const pending = pairings.filter((row) => !row.paired && !row.detail).map((row) => row.accountId);
    if (!pending.length) return;
    pairQueue.current = pending;
    setPairQueueLength(pending.length);
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
    });
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

  function repair() {
    if (!selectedHostId) return;
    void run("remote.act.repair", async () => {
      await api.repairRemoteManager(selectedHostId, `repair-${Date.now()}`);
      await load();
    });
  }

  const restartKey = native?.restartSafe ? "remote.act.restartNative" : "remote.act.takeoverNative";
  const restartDisabled = !native?.compatible
    || (!native.restartSafe && !native.standaloneInstalled)
    || busy !== null;

  /** 確認視窗的內容。固定三格事實，不是一段沒人讀完的話。 */
  const confirmConfig = pending ? {
    bootstrap: { titleKey: primaryActionKey, factsKey: "bootstrap", danger: false, gated: false },
    restore: { titleKey: "remote.act.restore", factsKey: "restore", danger: true, gated: true },
    restartNative: { titleKey: restartKey, factsKey: native?.restartSafe ? "restartNative" : "takeoverNative", danger: !native?.restartSafe, gated: false },
    installCodex: { titleKey: "remote.act.installCodex", factsKey: "installCodex", danger: false, gated: false },
    updateAgent: { titleKey: "remote.act.updateAgent", factsKey: "updateAgent", danger: false, gated: false },
    syncDesktopCodex: { titleKey: "remote.act.syncDesktopCodex", factsKey: "syncDesktopCodex", danger: false, gated: false },
    stopAppOwned: { titleKey: "remote.act.stopAppOwned", factsKey: "stopAppOwned", danger: false, gated: false },
  }[pending] : null;

  const runningPhaseKey = ownOperation ? phaseKey(ownOperation.phase) : null;
  const runningKindKey = ownOperation ? operationKindKey(ownOperation.kind) : null;
  const appOwnedFailure = Boolean(ownOperation?.state === "failed" && ownOperation.message?.includes("nativeDaemonAppOwned"));
  // The same reason can arrive on the action the user is standing in front of,
  // not just on a background operation — boundary-key provisioning restarts
  // native Codex, and an app-server Codex's own daemon disowns cannot be
  // restarted at all. Retry alone can never clear that, so offer the stop the
  // background branch already offers rather than leaving the dialog with two
  // buttons that both lead nowhere.
  const immediateAppOwnedFailure = Boolean(failure?.raw.includes("nativeDaemonAppOwned"));
  const immediateDesktopProtocolFailure = Boolean(failure?.raw.includes("CodexDesktopProtocolMismatch"));
  const operationDesktopProtocolFailure = Boolean(
    ownOperation?.state === "failed" && ownOperation.message?.includes("CodexDesktopProtocolMismatch"),
  );
  // Independent from the host-probe "Agent unreachable" signal elsewhere on
  // this screen: this is the *operation's own* failure reason, not the
  // background snapshot poll. A retry re-runs the same operation, which
  // re-provisions the boundary key from scratch — no special remedy button
  // needed, only the dedicated message so the user knows what actually
  // failed instead of reading a raw agent error string.
  const immediateBoundaryKeyFailure = Boolean(
    failure?.raw.includes("RemoteBoundaryKeyProvisionFailed"),
  );
  const operationBoundaryKeyFailure = Boolean(
    ownOperation?.state === "failed"
      && ownOperation.message?.includes("RemoteBoundaryKeyProvisionFailed"),
  );

  function desktopProtocolRemedy(): ReactNode {
    const updateAgentFirst = desktopCodex?.state === "agentUpdateRequired";
    const desktopRuntimeUnavailable = !desktopCodex
      || desktopCodex.state === "desktopUnavailable"
      || desktopCodex.state === "unavailable";
    return (
      <Btn
        mini
        soft
        onClick={() => setPending(updateAgentFirst ? "updateAgent" : "syncDesktopCodex")}
        disabled={busy !== null || (updateAgentFirst ? !release?.ready : desktopRuntimeUnavailable)}
      >
        {t(updateAgentFirst ? "remote.act.updateAgent" : "remote.act.syncDesktopCodex")}
      </Btn>
    );
  }

  /**
   * 重試失敗的背景作業時要重跑**它自己**，不是一律重跑 Bootstrap。
   * applyDeployment 沒得直接重試 —— 計畫有時效，過期的 planId 套下去會被拒絕，
   * 所以那一種的下一步是重新規劃。
   */
  function retryOperation() {
    if (ownOperation?.kind === "oneClickRestore") return setPending("restore");
    if (ownOperation?.kind === "applyDeployment") {
      return void run("remote.act.plan", () => createPlan(selectedModels));
    }
    return setPending("bootstrap");
  }

  return (
    <>
      <div className="canvas__head">
        <div>
          <h2 className="canvas__title">{t("remote.title")}</h2>
          <p className="note">{t("remote.blurb")}</p>
        </div>
        <Btn soft onClick={() => void load()} disabled={busy !== null || discovering}>
          {discovering ? t("remote.rescanning") : t("remote.rescan")}
        </Btn>
      </div>

      {legacyBrokerHosts.length ? (
        <Notice tone="warn" raw={legacyBrokerHosts.join(t("common.itemSeparator"))}>
          {t("remote.legacyBrokerUnsupported")}
        </Notice>
      ) : null}
      {discoveryError ? <Notice tone="error" raw={discoveryError}>{t("remote.error.discovery")}</Notice> : null}
      {discovering ? <Notice>{t("remote.discovering")}</Notice> : null}
      {strayOperation ? (
        <Notice
          acts={<Btn mini soft onClick={() => setSelectedHostId(strayOperation.hostId)}>{t("remote.goToHost")}</Btn>}
        >
          {t("remote.otherHostBusy", {
            host: hosts.find((item) => item.vellumHostId === strayOperation.hostId)?.displayName ?? strayOperation.hostId,
          })}
        </Notice>
      ) : null}

      {featureEnabled === false ? (
        <Card><Empty>{t("remote.featureDisabled")}</Empty></Card>
      ) : !hosts.length ? (
        <Card><Empty>{t("remote.noHosts")}</Empty></Card>
      ) : (
        <div className="remote-layout">
          <aside className="remote-hosts" aria-label={t("remote.hostsAria")}>
            {hosts.map((item) => {
              const itemStatus = statuses[item.vellumHostId];
              const unreachable = Boolean(probeErrors[item.vellumHostId]);
              const refreshing = probingHostId === item.vellumHostId && Boolean(itemStatus);
              const label = unreachable
                ? t("remote.probeFailed")
                : itemStatus
                  ? t(hostVerdict({
                      managerState: itemStatus.managerState,
                      agentReachable: Boolean(itemStatus.agent),
                    }).stateKey)
                  : probingHostId === item.vellumHostId
                    ? t("remote.probing")
                    : item.validated
                      ? t("remote.notProbed")
                      : t("remote.sshInvalid");
              return (
                <button
                  type="button"
                  key={item.vellumHostId}
                  className={`remote-host${selectedHostId === item.vellumHostId ? " remote-host--active" : ""}${unreachable ? " remote-host--unreachable" : ""}`}
                  onClick={() => { setSelectedHostId(item.vellumHostId); setPlan(null); setSelectedModels([]); setFailure(null); }}
                >
                  <b>{item.displayName}</b>
                  <span>{item.user ? `${item.user}@` : ""}{item.hostname ?? item.sshAlias}</span>
                  <small>
                    {label}
                    {refreshing ? <em className="remote-host__refreshing"> · {t("remote.updating")}</em> : null}
                  </small>
                </button>
              );
            })}
          </aside>

          <div className="remote-detail">
            {/* ---------- 工作面 ---------- */}
            <div className="worktop">
              <div className="worktop__head">
                <div>
                  <h3 className="worktop__name">{host?.displayName}</h3>
                  <p className="worktop__where">{host?.sshAlias}</p>
                </div>
                <div className="worktop__head-status">
                  <State tone={verdict.tone} label={t(verdict.stateKey)} />
                  {selectedHostId && probingHostId === selectedHostId ? (
                    <p className="worktop__updated worktop__updated--live">{t("remote.updating")}</p>
                  ) : selectedHostId && lastUpdated[selectedHostId] ? (
                    <p className="worktop__updated">
                      {t("remote.lastUpdated", {
                        when: new Date(lastUpdated[selectedHostId]).toLocaleTimeString(),
                      })}
                    </p>
                  ) : null}
                </div>
              </div>

              <p className="worktop__verdict">
                {t(verdict.verdictKey)}
                {sessions?.threads.length
                  ? ` · ${t("remote.summary.threads", { count: sessions.threads.length })}`
                  : ""}
              </p>

              {blockers.length ? (
                <div className="remote-blockers">
                  {blockers.map((blocker) => (
                    <Notice
                      key={blocker.code}
                      tone="warn"
                      raw={blocker.messageKey ? null : blocker.code}
                      acts={remedyButton(blocker.remedy)}
                    >
                      {blocker.messageKey ? t(blocker.messageKey) : t("remote.blockerUnknown")}
                    </Notice>
                  ))}
                </div>
              ) : null}

              {/* 進行中／失敗都原地取代按鈕。因與果同一格。 */}
              {proxyImageUpdateAvailable ? (
                <Notice
                  tone="warn"
                  acts={
                    <Btn mini soft onClick={() => setPending("bootstrap")} disabled={busy !== null || operationRunning}>
                      {t("remote.act.reconverge")}
                    </Btn>
                  }
                >
                  {t("remote.proxyImageUpdate")}
                </Notice>
              ) : null}

              {sshTrust?.hostId === selectedHostId && sshTrust.checking ? (
                <Notice>{t("remote.trust.checking")}</Notice>
              ) : sshTrust?.hostId === selectedHostId && !sshTrust.confirmed ? (
                <div className="worktop__trust">
                  <p className="worktop__verdict">{t("remote.trust.title")}</p>
                  <Notice tone="warn" raw={sshTrust.fetchError}>
                    {sshTrust.fetchError ? t("remote.trust.fetchFailed") : t("remote.trust.explain")}
                  </Notice>
                  {sshTrust.fingerprint ? (
                    <>
                      <Rows>
                        <Row label={t("remote.trust.factHost")}>{`${sshTrust.fingerprint.host}:${sshTrust.fingerprint.port}`}</Row>
                        <Row label={t("remote.trust.factFingerprint")}><code>{sshTrust.fingerprint.fingerprint}</code></Row>
                        <Row label={t("remote.trust.factCrossCheck")}>{t("remote.trust.crossCheckHint")}</Row>
                      </Rows>
                      <div className="worktop__acts">
                        <Btn onClick={confirmSshTrust} disabled={busy !== null}>{t("remote.trust.confirm")}</Btn>
                      </div>
                    </>
                  ) : null}
                </div>
              ) : ownOperation && ownOperation.state === "running" ? (
                <div className="worktop__run">
                  <div className="worktop__phase">
                    <b>{runningKindKey ? t(runningKindKey) : ownOperation.kind}</b>
                    <span className="worktop__tick">
                      {ownOperation.percent}% · {t("remote.operation.elapsed", {
                        clock: elapsedLabel(
                          operationStartedAt.current.get(ownOperation.operationId) ?? Date.now(),
                          Date.now(),
                        ),
                      })}
                    </span>
                  </div>
                  <Meter percent={ownOperation.percent} />
                  <p className="worktop__where">
                    {runningPhaseKey ? t(runningPhaseKey) : ownOperation.phase}
                  </p>
                </div>
              ) : failure ? (
                /* 剛按下去的動作失敗，排在背景作業失敗前面 —— 它比較新，
                   而且它是使用者正在等的那一件事。 */
                <Notice
                  tone="error"
                  raw={failure.raw}
                  acts={<>
                    {immediateAppOwnedFailure ? (
                      <Btn mini soft onClick={() => setPending("stopAppOwned")} disabled={busy !== null}>
                        {t("remote.act.stopAppOwned")}
                      </Btn>
                    ) : null}
                    {immediateDesktopProtocolFailure
                      ? desktopProtocolRemedy()
                      : <Btn mini soft onClick={() => retry.current?.()} disabled={busy !== null}>{t("remote.act.retry")}</Btn>}
                    <Btn mini soft onClick={exportBundle} disabled={busy !== null}>{t("remote.act.bundle")}</Btn>
                  </>}
                >
                  {immediateDesktopProtocolFailure
                    ? t("remote.error.desktopCodexMismatch")
                    : immediateAppOwnedFailure
                      ? t("remote.blocker.nativeDaemonAppOwned")
                    : immediateBoundaryKeyFailure
                      ? t("remote.error.boundaryKeyProvisionFailed")
                      : t("remote.error.actionFailed", { action: t(failure.actionKey) })}
                </Notice>
              ) : ownOperation && ownOperation.state === "failed" ? (
                <Notice
                  tone="error"
                  raw={ownOperation.message}
                  acts={<>
                    {appOwnedFailure ? (
                      <Btn mini soft onClick={() => setPending("stopAppOwned")} disabled={busy !== null}>
                        {t("remote.act.stopAppOwned")}
                      </Btn>
                    ) : null}
                    {operationDesktopProtocolFailure
                      ? desktopProtocolRemedy()
                      : <Btn mini soft onClick={retryOperation} disabled={busy !== null}>{t("remote.act.retry")}</Btn>}
                    <Btn mini soft onClick={exportBundle} disabled={busy !== null}>{t("remote.act.bundle")}</Btn>
                  </>}
                >
                  {operationDesktopProtocolFailure
                    ? t("remote.error.desktopCodexMismatch")
                    : appOwnedFailure
                      ? t("remote.blocker.nativeDaemonAppOwned")
                    : operationBoundaryKeyFailure
                      ? t("remote.error.boundaryKeyProvisionFailed")
                      : t("remote.error.operationFailed", {
                          action: runningKindKey ? t(runningKindKey) : ownOperation.kind,
                          phase: runningPhaseKey ? t(runningPhaseKey) : ownOperation.phase,
                        })}
                </Notice>
              ) : (
                <div className="worktop__acts">
                  <Btn
                    onClick={() => setPending("bootstrap")}
                    disabled={!selectedHostId || busy !== null || operationRunning}
                  >
                    {t(primaryActionKey)}
                  </Btn>
                  {/* 規劃要讀主機上的 catalog，連不上就一定失敗。
                      讓它可按只是把一個必然的錯誤留給使用者去踩。 */}
                  <Btn
                    soft
                    onClick={() => void run("remote.act.plan", () => createPlan([]))}
                    disabled={!selectedHostId || !status?.agent || busy !== null}
                  >
                    {t("remote.act.plan")}
                  </Btn>
                </div>
              )}
            </div>

            {/* ---------- 帳號 ----------
                ChatGPT 與 Grok 是同一類事（誰來付這次 turn 的帳），
                所以擺在一起。上一版把「部署 Grok 登入」跟「Restart native」
                排在同一排 —— 一個是帳號設定，一個是重啟服務程序。 */}
            <Card>
              <div className="rowline">
                <Cap>{t("remote.account.title")}</Cap>
                {/* 一句整體結論。逐一列出每個帳號之後，「ChatGPT: 已同步」
                    這一列就不再是資訊而是重複，所以它退成標題旁的註記。 */}
                <span className="rows__hint">{chatgptStateLabel}</span>
              </div>
              {chatgptLogin ? (
                <Notice
                  acts={pairQueueLength || chatgptLogin ? (
                    <Btn mini soft onClick={skipPairing} disabled={busy !== null}>{t("remote.act.chatgptPairSkip")}</Btn>
                  ) : null}
                >
                  {t("remote.account.pairingHint")}
                  {" "}
                  <a href={chatgptLogin.verificationUrl} target="_blank" rel="noreferrer">{chatgptLogin.verificationUrl}</a>
                  {" · "}
                  <strong>{chatgptLogin.userCode}</strong>
                  {pairQueueLength ? <> · {t("remote.account.pairingRemaining", { remaining: pairQueueLength })}</> : null}
                </Notice>
              ) : null}
              {/* 每個 Desktop 帳號一列。`paired` 講的是「這台主機自己有沒有那個
                  帳號的 grant」——不是 Desktop 有沒有。兩邊各持一份是刻意的，
                  一份 grant 沒辦法給兩個客戶端共用。 */}
              {pairings.length ? (
                <div className="accounts">
                  {pairings.map((row) => (
                    <div className={`accounts__row${row.active ? " accounts__row--active" : ""}`} key={row.accountId}>
                      <span className="accounts__who">
                        <b>{row.email ?? row.accountId}</b>
                        {/* 桌面端預設刻意不做成 pill —— 它不是狀態，是「Vellum
                            UI 切帳號時會推的是這一個」。跟狀態長得一樣就會被
                            當成第二種狀態讀。 */}
                        {row.isDesktopDefault ? (
                          <small className="rows__hint">{t("remote.account.desktopDefault")}</small>
                        ) : null}
                        {row.detail ? <small className="accounts__why">{row.detail}</small> : null}
                      </span>
                      <State
                        tone={row.detail ? "quiet" : row.active ? "ok" : row.paired ? "warn" : "quiet"}
                        label={row.detail
                          ? t("remote.account.pairingUnknown")
                          : row.active
                            ? t("remote.account.pairingActive")
                            : row.paired
                              ? t("remote.account.pairingPaired")
                              : t("remote.account.pairingMissing")}
                      />
                      {/* 動作欄永遠在，沒有動作時是空的 —— 不然有按鈕的那幾列
                          會把狀態往左推，整批就對不齊了。 */}
                      <span className="accounts__act">
                        {row.detail || row.active ? null : row.paired ? (
                          <Btn mini soft onClick={() => activateChatGpt(row.accountId)} disabled={busy !== null}>
                            {t("remote.act.chatgptActivateRow")}
                          </Btn>
                        ) : (
                          <Btn mini soft onClick={() => pairChatGpt(row.accountId)} disabled={busy !== null || Boolean(chatgptLogin)}>
                            {t("remote.act.chatgptPairRow")}
                          </Btn>
                        )}
                      </span>
                    </div>
                  ))}
                </div>
              ) : null}
              {/* Grok 是另一家的帳號，跟上面那批 ChatGPT 不同類，所以它自己
                  一列，不混進登記簿裡。 */}
              <Rows>
                <Row label="Grok">
                  {grok?.detachedQualified
                    ? t("remote.account.grokReady", { account: grok.account ?? "" })
                    : grok?.configured
                      ? t("remote.account.grokPartial")
                      : t("remote.account.grokAbsent")}
                </Row>
              </Rows>
              {grok?.loginPending ? (
                <Notice>
                  {t("remote.account.grokDeviceLogin")}
                  {" "}
                  {grok.verificationUri
                    ? <a href={grok.verificationUri} target="_blank" rel="noreferrer">{grok.verificationUri}</a>
                    : t("remote.account.grokWaitingUrl")}
                  {grok.userCode ? <> · <strong>{grok.userCode}</strong></> : null}
                </Notice>
              ) : null}
              <div className="remote-actions">
                {pairings.some((row) => !row.paired && !row.detail) ? (
                  <Btn soft onClick={pairAllChatGptAccounts} disabled={busy !== null || Boolean(chatgptLogin)}>
                    {t("remote.act.chatgptPairAll")}
                  </Btn>
                ) : null}
                {chatgpt?.state === "pairingRequired" || chatgpt?.state === "pairingPending" ? (
                  <Btn soft onClick={() => pairChatGpt()} disabled={busy !== null || Boolean(chatgptLogin)}>{t("remote.act.chatgptPair")}</Btn>
                ) : null}
                {chatgpt?.state === "activationRequired" ? (
                  <Btn soft onClick={() => activateChatGpt()} disabled={busy !== null}>{t("remote.act.chatgptActivate")}</Btn>
                ) : null}
                {grok?.loginPending ? (
                  <Btn soft onClick={() => selectedHostId && void run("remote.act.grokCancel", async () => { await api.cancelRemoteGrokLogin(selectedHostId); await load(); })} disabled={busy !== null}>{t("remote.act.grokCancel")}</Btn>
                ) : (
                  <Btn soft onClick={() => selectedHostId && void run("remote.act.grokLogin", async () => { await api.startRemoteGrokLogin(selectedHostId); await load(); })} disabled={!selectedHostId || busy !== null}>
                    {busy === "remote.act.grokLogin" ? (
                      <><span className="remote-login-spinner" aria-hidden="true" />{t("remote.account.grokStarting")}</>
                    ) : t("remote.act.grokLogin")}
                  </Btn>
                )}
                {grok?.configured ? (
                  <Btn soft onClick={() => selectedHostId && void run("remote.act.grokRefresh", async () => { await api.refreshRemoteGrokLogin(selectedHostId); await load(); })} disabled={busy !== null}>{t("remote.act.grokRefresh")}</Btn>
                ) : null}
              </div>
            </Card>

            {/* ---------- Remote control identity A ---------- */}
            <Card>
              <Cap>{t("remote.control.title")}</Cap>
              <p className="remote-mobile-empty">{t("remote.control.sameAccountHint")}</p>
              <Rows>
                <Row label={t("remote.control.identity")}>{chatgptStateLabel}</Row>
              </Rows>
              {devicePairing ? (
                <Notice>
                  {t("remote.control.deviceHint")}
                  {devicePairing.verificationUrl || devicePairing.uri ? (
                    <>
                      {" "}
                      <a href={devicePairing.verificationUrl ?? devicePairing.uri} target="_blank" rel="noreferrer">
                        {devicePairing.verificationUrl ?? devicePairing.uri}
                      </a>
                    </>
                  ) : null}
                  {devicePairing.pairingCode || devicePairing.code || devicePairing.userCode ? (
                    <>{" · "}<strong>{devicePairing.pairingCode ?? devicePairing.code ?? devicePairing.userCode}</strong></>
                  ) : null}
                  {devicePairing.expiresAt ? <span className="remote-mobile-expiry"> · {t("remote.control.expiresAt", { when: devicePairing.expiresAt })}</span> : null}
                </Notice>
              ) : null}
              <div className="remote-actions">
                <Btn soft onClick={pairMobileDevice} disabled={busy !== null || chatgpt?.state !== "synchronized"}>{t("remote.act.devicePair")}</Btn>
              </div>
            </Card>

            {/* ---------- Proxy execution identity B ---------- */}
            <Card>
              <Cap>{t("remote.execution.title")}</Cap>
              <p className="remote-mobile-empty">{t("remote.execution.independentHint")}</p>
              {executionAccounts.length ? (
                <Rows>
                  {executionAccounts.map((account) => (
                    <Row key={account.accountIdHash} label={account.displayName}>
                      <code>{account.accountIdHash.slice(0, 12)}</code>
                      {account.selected ? (
                        <> · {t("remote.execution.selected")}</>
                      ) : (
                        <> · <Btn mini soft onClick={() => selectOfficialExecutionAccount(account.accountIdHash)} disabled={busy !== null}>{t("remote.execution.select")}</Btn></>
                      )}
                      <> · <Btn mini soft onClick={() => removeOfficialExecutionAccount(account.accountIdHash)} disabled={busy !== null}>{t("remote.execution.remove")}</Btn></>
                    </Row>
                  ))}
                </Rows>
              ) : (
                <p className="remote-mobile-empty">{t("remote.execution.empty")}</p>
              )}
              {executionLogin ? (
                <Notice>
                  {t("remote.execution.loginHint")}
                  {" "}
                  <a href={executionLogin.verificationUrl} target="_blank" rel="noreferrer">{executionLogin.verificationUrl}</a>
                  {" · "}
                  <strong>{executionLogin.userCode}</strong>
                </Notice>
              ) : null}
              <div className="remote-actions">
                <input
                  className="input remote-mobile-name-input"
                  type="text"
                  placeholder={t("remote.execution.namePlaceholder")}
                  value={executionDisplayName}
                  onChange={(event) => setExecutionDisplayName(event.target.value)}
                  disabled={busy !== null || Boolean(executionLogin)}
                />
                <Btn soft onClick={addOfficialExecutionAccount} disabled={busy !== null || Boolean(executionLogin) || !executionDisplayName.trim()}>
                  {t("remote.act.executionLogin")}
                </Btn>
              </div>
            </Card>

            {/* ---------- 主機細節 ----------
                原本是十九行同權重的狀態，常駐在主卡上，其中 Docker 還出現兩次
                （一次來自 agent capabilities，一次來自 inventory）。
                這是「很少問但問起來要問得完整」的資料，所以收起來、分四組。 */}
            <Card>
              <Cap>{t("remote.detail.title")}</Cap>
              <Tray label={t("remote.detail.host")}>
                <Rows>
                  <Row label="SSH">{host?.validated ? t("remote.detail.sshResolved") : host?.validationError ?? t("remote.detail.sshUnresolved")}</Row>
                  <Row label="Agent">{status?.agent ? `${status.agent.agentVersion} · protocol ${status.agent.agentProtocol}` : probeErrors[selectedHostId ?? ""] ?? status?.agentError ?? t("remote.detail.agentAbsent")}</Row>
                  <Row label="OS">{status?.agent ? `${status.agent.capabilities.os} · ${status.agent.capabilities.arch}` : unknown}</Row>
                  <Row label={t("remote.detail.platform")}>{inventory?.platform ?? status?.agent?.capabilities.platform ?? unknown}</Row>
                  <Row label={t("remote.detail.proxyBackend")}>{inventory?.proxyBackend ?? status?.agent?.capabilities.proxyBackend ?? unknown}</Row>
                  <Row label={t("remote.detail.persistence")}>{
                    (inventory?.persistenceScope ?? status?.agent?.capabilities.persistenceScope) === "login"
                      ? t("remote.detail.loginResident")
                      : (inventory?.persistenceScope ?? status?.agent?.capabilities.persistenceScope) === "linger"
                        ? t("remote.detail.lingerResident")
                        : unknown
                  }</Row>
                  <Row label={t("remote.detail.managedHome")}>{inventory?.managedCodexHome ?? status?.agent?.capabilities.managedCodexHome ?? native?.codexHome ?? unknown}</Row>
                  {(inventory?.platform === "darwin-arm64" || status?.agent?.capabilities.platform === "darwin-arm64") && (
                    <Row label={t("remote.detail.isolationLabel")}>{t("remote.detail.isolation")}</Row>
                  )}
                  {(status?.agent?.capabilities.os === "darwin" || status?.agent?.capabilities.os === "macos") && status?.agent?.capabilities.arch === "x86_64" && (
                    <Row label={t("remote.detail.platform")}>{t("remote.blocker.intelMacUnsupported")}</Row>
                  )}
                  <Row label="CPU">{inventory?.system.cpuCores != null ? t("remote.detail.cores", { count: inventory.system.cpuCores }) : unknown}</Row>
                  <Row label="Memory">{formatBytes(inventory?.system.memoryBytes, unknown)}</Row>
                  <Row label="Disk">{inventory?.system.diskTotalBytes != null ? t("remote.detail.diskFree", { free: formatBytes(inventory.system.diskFreeBytes, unknown), total: formatBytes(inventory.system.diskTotalBytes, unknown) }) : unknown}</Row>
                  {/* Docker 原本出現兩次：一次來自 agent capabilities，一次來自
                      inventory，兩個來源在同一張卡上並列。留 inventory 那一份，
                      agent 只當備援。 */}
                  <Row label="Docker">{inventory?.docker.available
                    ? `${inventory.docker.mode}${inventory.docker.serverVersion ? ` · v${inventory.docker.serverVersion}` : ""}${inventory.docker.context ? ` · ${inventory.docker.context}` : ""}`
                    : status?.agent?.capabilities.dockerAvailable
                      ? status.agent.capabilities.dockerMode
                      : t("remote.detail.dockerAbsent")}</Row>
                </Rows>
              </Tray>
              <Tray label={t("remote.detail.runtime")}>
                <Rows>
                  <Row label="Proxy">{proxy?.ready ? t("remote.detail.proxyReady", { image: proxy.image ?? "managed" }) : proxy?.running ? t("remote.detail.proxyNotReady") : t("remote.detail.proxyStopped")}</Row>
                  {configurationKey && (
                    <Row label={t("remote.detail.config")}>{t(configurationKey)}</Row>
                  )}
                  <Row label="Codex CLI">{native?.codexVersion ?? status?.agent?.capabilities.codexVersion ?? unknown}</Row>
                  <Row label="Codex App CLI">{native?.cliLauncher?.ready ? native.cliLauncher.path ?? t("remote.detail.launcherLoginShell") : t("remote.detail.launcherBroken")}</Row>
                  <Row label="Daemon">{native?.daemonRunning ? t("remote.detail.daemonRunning", { pid: native.daemonPid ?? "?" }) : t("remote.detail.daemonStopped")}</Row>
                  <Row label={t("remote.detail.daemonOwner")}>{native ? `${native.daemonOwner} · ${native.durable ? t("remote.detail.durable") : t("remote.detail.notDurable")}` : unknown}</Row>
                  {/* 沒值就是沒值。原本這裡填「由 Agent 探測，不接受 renderer 路徑」
                      —— 那是給工程師的實作註記，不是給使用者的訊息。 */}
                  <Row label="CODEX_HOME">{native?.codexHome ?? unknown}</Row>
                  <Row label={t("remote.detail.codexSource")}>{inventory ? `${inventory.codex.source}${inventory.codex.version ? ` · ${inventory.codex.version}` : ""}` : unknown}</Row>
                </Rows>
              </Tray>
              <Tray label={t("remote.detail.version")}>
                <Rows>
                  <Row label={t("remote.desktopCodex.desktop")}>{desktopCodex?.desktopVersion ?? unknown}</Row>
                  <Row label={t("remote.desktopCodex.remote")}>{desktopCodex?.remoteVersion ?? unknown}</Row>
                  <Row label={t("remote.desktopCodex.status")}>{desktopCodex ? t(`remote.desktopCodex.states.${desktopCodex.state}`) : t("common.loading")}</Row>
                  <Row label={t("remote.detail.releaseTrust")}>{release?.ready ? `${release.releaseVersion ?? t("remote.detail.verified")} · ${release.trust}` : t("remote.detail.releaseUnverified", { trust: release?.trust ?? unknown })}</Row>
                  <Row label={t("remote.detail.pinned")}>{release?.ready ? `Codex ${release.codexVersion} · Agent ${release.agentVersion} · Broker ${release.brokerVersion}` : unknown}</Row>
                </Rows>
              </Tray>
              {inventory?.blockers.length ? (
                <div className="remote-maintenance-list" aria-label={t("remote.detail.inventoryBlockers")}>
                  {inventory.blockers.map((blocker) => (
                    <div key={blocker.code}>
                      <State tone={blocker.repairable ? "warn" : "quiet"} label={blocker.code} />
                      <span>{blocker.message}</span>
                    </div>
                  ))}
                </div>
              ) : null}
            </Card>

            {/* ---------- 維護 ----------
                一列一行：動作名／一句後果／按鈕。不是一排並列的按鈕 ——
                這些事一年按不到一次，光看動詞沒人知道後果，
                而把說明藏進 tooltip 只服務「已經知道那是什麼」的人。 */}
            <Card>
              <Tray label={t("remote.maintenance.title")}>
                {desktopCodex && desktopCodex.state !== "current" ? (
                  <div className="remote-chore">
                    <b>{t("remote.act.syncDesktopCodex")}</b>
                    <small>{t("remote.chore.syncDesktopCodex", {
                      desktop: desktopCodex.desktopVersion ?? unknown,
                      remote: desktopCodex.remoteVersion ?? unknown,
                    })}</small>
                    <Btn
                      soft
                      onClick={() => desktopCodex.state === "agentUpdateRequired"
                        ? setPending("updateAgent")
                        : setPending("syncDesktopCodex")}
                      disabled={busy !== null || (desktopCodex.state === "agentUpdateRequired"
                        ? !release?.ready
                        : !desktopCodex.canUpdate)}
                    >
                      {desktopCodex.state === "agentUpdateRequired"
                        ? t("remote.act.updateAgent")
                        : t("remote.act.syncDesktopCodex")}
                    </Btn>
                  </div>
                ) : null}
                {!release?.ready ? (
                  <Notice tone="warn" raw={release?.detail}>{t("remote.releaseBlocked")}</Notice>
                ) : null}
                <div className="remote-chore">
                  <b>{t(restartKey)}</b>
                  <small>{t(native?.restartSafe ? "remote.chore.restartNative" : "remote.chore.takeoverNative")}</small>
                  <Btn soft onClick={() => setPending("restartNative")} disabled={restartDisabled}>{t(restartKey)}</Btn>
                </div>
                {status?.availableActions.includes("installCodex") ? (
                  <div className="remote-chore">
                    <b>{t("remote.act.installCodex")}</b>
                    <small>{t("remote.chore.installCodex")}</small>
                    <Btn soft onClick={() => setPending("installCodex")} disabled={busy !== null || !release?.ready}>{t("remote.act.installCodex")}</Btn>
                  </div>
                ) : null}
                {status?.availableActions.includes("updateComponents") ? (
                  <div className="remote-chore">
                    <b>{t("remote.act.updateAgent")}</b>
                    <small>{t("remote.chore.updateAgent")}</small>
                    <Btn soft onClick={() => setPending("updateAgent")} disabled={busy !== null || !release?.ready}>{t("remote.act.updateAgent")}</Btn>
                  </div>
                ) : null}
                {status?.availableActions.includes("repair") ? (
                  <div className="remote-chore">
                    <b>{t("remote.act.repair")}</b>
                    <small>{t("remote.chore.repair")}</small>
                    <Btn soft onClick={repair} disabled={busy !== null}>{t("remote.act.repair")}</Btn>
                  </div>
                ) : null}
                {status?.availableActions.includes("exportSupportBundle") ? (
                  <div className="remote-chore">
                    <b>{t("remote.act.bundle")}</b>
                    <small>{t("remote.chore.bundle")}</small>
                    <Btn soft onClick={exportBundle} disabled={busy !== null}>{t("remote.act.bundle")}</Btn>
                  </div>
                ) : null}
              </Tray>
            </Card>

            <Card>
              <div className="remote-title-row">
                <div><Cap>{t("remote.sessions.cap")}</Cap><h3 className="remote-title">{t("remote.sessions.title")}</h3></div>
                <State
                  tone={sessions?.observability === "nativeAppServer" ? "ok" : sessions?.observability === "daemonDown" ? "quiet" : "warn"}
                  label={sessions ? t(`remote.sessions.observability.${sessions.observability}`) : t("common.loading")}
                />
              </div>
              {!sessions ? <Empty>{t("remote.sessions.unknown")}</Empty> : !sessions.threads.length ? (
                <Empty>{sessions.observability === "nativeAppServer" ? t("remote.sessions.empty") : t("remote.sessions.unreadable")}</Empty>
              ) : (
                <div className="remote-session-list">
                  {sessions.threads.map((thread) => (
                    <div key={thread.threadId} className="remote-session">
                      <div><code>{thread.threadId}</code><State tone={thread.activeTurn ? "ok" : "quiet"} label={thread.status} /></div>
                      <small>{t("remote.sessions.turns", { count: thread.turnCount, last: thread.lastTurnStatus ?? unknown })}{thread.activeTurnId ? ` · ${thread.activeTurnId}` : ""}</small>
                    </div>
                  ))}
                </div>
              )}
            </Card>

            {plan ? (
              <Card>
                <div className="remote-title-row">
                  <div><Cap>{t("remote.plan.cap")}</Cap><h3 className="remote-title">{t("remote.plan.title")}</h3></div>
                  <State tone={plan.blockedReasons.length ? "warn" : "ok"} label={plan.blockedReasons.length ? t("remote.plan.blocked") : t("remote.plan.ready")} />
                </div>
                <div className="remote-models">
                  {plan.selectedModels.map((model) => (
                    <label key={model.catalogId} className="remote-model">
                      <input type="checkbox" disabled={model.mandatory} checked={selectedModels.includes(model.catalogId)} onChange={(event) => setSelectedModels((current) => event.target.checked ? [...current, model.catalogId] : current.filter((id) => id !== model.catalogId))} />
                      <span><b>{model.displayName}</b><small>{model.routeId} → {model.upstreamModel}</small></span>
                    </label>
                  ))}
                </div>
                <Rows>
                  <Row label={t("remote.plan.configHash")}><code>{plan.drift.desiredConfigHash.slice(0, 16)}</code>{plan.drift.configChanged ? ` · ${t("remote.plan.changed")}` : ` · ${t("remote.plan.same")}`}</Row>
                  <Row label={t("remote.plan.catalogHash")}><code>{plan.drift.desiredCatalogHash.slice(0, 16)}</code></Row>
                  {plan.drift.reviewPolicyChanged ? (
                    <Row label={t("remote.plan.reviewPolicy")}>
                      <State tone="warn" label={t("remote.plan.reviewPolicyPending")} />
                    </Row>
                  ) : null}
                  <Row label={t("remote.plan.credentials")}>{plan.credentialRequirements.length ? plan.credentialRequirements.map((item) => `${item.credentialId}${item.detachedQualified ? " ✓" : " ⚠"}`).join(t("common.itemSeparator")) : t("remote.plan.noCredentials")}</Row>
                  <Row label={t("remote.plan.revision")}>{t("remote.plan.revisionValue", { desired: plan.desiredRevision, observed: plan.observedRevision })}</Row>
                  <Row label={t("remote.plan.planHash")}><code>{plan.planHash.slice(0, 16)}</code></Row>
                  <Row label={t("remote.plan.rollback")}>{plan.rollbackSummary}</Row>
                  <Row label={t("remote.plan.managed")}>{plan.managedChanges.join(t("common.itemSeparator"))}</Row>
                </Rows>
                {plan.blockedReasons.length ? (
                  <div className="remote-blockers">
                    {visibleBlockers(plan.blockedReasons, false).map((blocker) => (
                      <Notice key={blocker.code} tone="warn" raw={blocker.messageKey ? null : blocker.code} acts={remedyButton(blocker.remedy)}>
                        {blocker.messageKey ? t(blocker.messageKey) : t("remote.blockerUnknown")}
                      </Notice>
                    ))}
                  </div>
                ) : null}
                <div className="remote-actions">
                  {/* 模型選擇本身沒變、只是本機 Auto Review 設定改了時，
                      走 reapplyPlan（沿用這台主機自己存的 overrides），
                      不是 createPlan（固定送 policy: {}，會把
                      autoReviewEnabled 這類 per-host 選項重設掉）。 */}
                  <Btn
                    soft
                    onClick={() =>
                      void run("remote.act.replan", () =>
                        selectionChanged ? createPlan(selectedModels) : reapplyPlan(),
                      )
                    }
                    disabled={(!selectionChanged && !plan.drift.reviewPolicyChanged) || busy !== null}
                  >
                    {t("remote.act.replan")}
                  </Btn>
                  <Btn onClick={() => selectedHostId && plan && void run("remote.act.apply", async () => trackOperation(await api.applyRemoteDeployment(selectedHostId, plan.planId)))} disabled={!canApply}>{t("remote.act.apply")}</Btn>
                </div>
              </Card>
            ) : null}

            {/* ---------- 危險區 ----------
                「一鍵還原」在 Windows 語境聽起來是救援、是安全的，但它其實是
                拆除。名字要講出後果，而且不能跟「匯出診斷包」擺在同一排 ——
                肌肉記憶會按錯。 */}
            <div className="remote-danger">
              <b>{t("remote.act.restore")}</b>
              <p>{t("remote.danger.body")}</p>
              <Btn danger onClick={() => setPending("restore")} disabled={!selectedHostId || busy !== null || operationRunning}>
                {t("remote.act.restore")}
              </Btn>
            </div>
          </div>
        </div>
      )}

      {pending && confirmConfig ? (
        <Confirm
          open
          title={t(confirmConfig.titleKey)}
          confirmLabel={t(confirmConfig.titleKey)}
          danger={confirmConfig.danger}
          gate={confirmConfig.gated && host
            ? { label: t("remote.confirm.gate", { host: host.displayName }), phrase: host.displayName }
            : null}
          facts={[
            { key: t("remote.fact.changes"), value: t(`remote.confirm.${confirmConfig.factsKey}.changes`) },
            { key: t("remote.fact.untouched"), value: t(`remote.confirm.${confirmConfig.factsKey}.untouched`) },
            { key: t("remote.fact.turns"), value: t(`remote.confirm.${confirmConfig.factsKey}.turns`) },
          ]}
          onConfirm={() => runPending(pending)}
          onCancel={() => setPending(null)}
        />
      ) : null}
    </>
  );
}
