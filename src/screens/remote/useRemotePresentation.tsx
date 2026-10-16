import { remoteWorkSurface } from "@/lib/remoteWorkSurface";
import { configurationStateKey, hostVerdict, deploymentWaitControls, visibleBlockers } from "@/lib/remoteVocabulary";
import { useRemoteConnection } from './useRemoteConnection';
export function useRemotePresentation(context: ReturnType<typeof useRemoteConnection>) {
  const {
    hosts,
    selectedHostId,
    statuses,
    t,
    operation,
    failure,
    desktopCodex,
    busy,
    sshTrust,
    chatgptLogin,
    release,
    pairings,
    executionAccounts,
    pending,
  } = context;
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
  const workSurface = remoteWorkSurface({
    managerState: status?.managerState ?? "unmanaged",
    agentReachable: Boolean(status?.agent),
    persistenceScope: inventory?.persistenceScope ?? status?.agent?.capabilities.persistenceScope,
    operation: ownOperation,
    error: failure?.raw,
    blockers: blockers.map((blocker) => blocker.code),
    desktopCodexState: desktopCodex?.state,
  });
  const primaryActionKey = workSurface.primaryKey;
  const workRemedy = workSurface.remedy?.action;
  const workRemedyDisabled = busy !== null || operationRunning || sshTrust?.hostId !== selectedHostId || !sshTrust?.confirmed
    || (workRemedy === "pairChatGpt" && Boolean(chatgptLogin))
    || ((workRemedy === "installCodex" || workRemedy === "updateAgent") && !release?.ready);
  // 只翻譯認得的狀態。後端多出一個新的值時顯示原碼，不假裝看得懂。
  const CHATGPT_STATES = ["ready", "synchronized", "pairingRequired", "pairingPending", "activationRequired", "reauthenticationRequired", "accountUnavailable", "desktopAccountUnavailable"];
  const chatgptStateLabel = !chatgpt
    ? t("remote.account.chatgptUnavailable")
    : CHATGPT_STATES.includes(chatgpt.state)
      ? t(`remote.account.chatgpt.${chatgpt.state}`)
      : chatgpt.state;
  const activeControlAccount = pairings.find((row) => row.active) ?? null;
  const selectedExecutionAccount = executionAccounts.find((account) => account.selected) ?? null;
  const restartKey = native?.restartSafe ? "remote.act.restartNative" : "remote.act.takeoverNative";
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
  const waitControls = deploymentWaitControls(ownOperation, selectedHostId);
  return {
    ...context,
    host,
    status,
    native,
    proxy,
    configuration,
    configurationKey,
    grok,
    chatgpt,
    inventory,
    unknown,
    operationRunning,
    verdict,
    blockers,
    ownOperation,
    strayOperation,
    workSurface,
    primaryActionKey,
    workRemedy,
    workRemedyDisabled,
    CHATGPT_STATES,
    chatgptStateLabel,
    activeControlAccount,
    selectedExecutionAccount,
    restartKey,
    confirmConfig,
    waitControls,
  };
}
