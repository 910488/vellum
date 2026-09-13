/**
 * 遠端總管的機器碼 → 人話。
 *
 * 後端送過來的是代號：`nativeDaemonAppOwned`、`hostPreflight`、`drifted`。
 * 那些是給 issue tracker 與日誌用的，不是給使用者的 —— 原本的畫面直接把
 * 它們當文字塞進 <code> 裡，看到的人只能去 grep 原始碼。
 *
 * 跟 lib/vocabulary.ts 同一個規矩：這裡只回傳語意鍵，翻譯交給元件。
 * 後端不知道使用者把介面切成哪一種語言，也不該知道。
 *
 * 未知代號一律不吞掉：回 null，呼叫端顯示原碼。少一句人話比假裝認得
 * 一個其實沒處理過的狀態好。
 */

/** 主要動作。工作面上永遠只有一顆，這個型別就是「那一顆是什麼」。 */
export type RemotePrimaryAction = "bootstrap" | "reconverge" | null;

export interface RemoteVerdict {
  tone: "ok" | "warn" | "quiet";
  /** 短語，給 <State> 那一道筆畫。 */
  stateKey: string;
  /** 一句話的結論。整頁最重要的一行。 */
  verdictKey: string;
  primary: RemotePrimaryAction;
}

/**
 * 一台主機現在到底能不能用。
 *
 * managerState 的五個值（unmanaged／readyToPlan／drifted／nativeActive／
 * detachedReady）本身就是答案，但它們是駝峰英文代號，而且沒有告訴人
 * 「所以我現在該按什麼」。這裡把兩件事一起回答。
 */
export function hostVerdict(facts: {
  managerState: string;
  agentReachable: boolean;
}): RemoteVerdict {
  // Agent 連不上有兩種可能：真的沒裝（新機），或是連線壞了。兩種的下一步
  // 都是 Bootstrap —— 它本來就負責把 agent 裝上去，而且可以重複執行。
  if (!facts.agentReachable) {
    return {
      tone: "quiet",
      stateKey: "remote.state.unreachable",
      verdictKey: "remote.verdict.unreachable",
      primary: "bootstrap",
    };
  }
  switch (facts.managerState) {
    case "detachedReady":
      return {
        tone: "ok",
        stateKey: "remote.state.detachedReady",
        verdictKey: "remote.verdict.detachedReady",
        primary: "reconverge",
      };
    case "nativeActive":
      return {
        tone: "ok",
        stateKey: "remote.state.nativeActive",
        verdictKey: "remote.verdict.nativeActive",
        primary: "reconverge",
      };
    case "readyToPlan":
      return {
        tone: "warn",
        stateKey: "remote.state.readyToPlan",
        verdictKey: "remote.verdict.readyToPlan",
        primary: "reconverge",
      };
    case "drifted":
      return {
        tone: "warn",
        stateKey: "remote.state.drifted",
        verdictKey: "remote.verdict.drifted",
        primary: "reconverge",
      };
    case "unmanaged":
    default:
      return {
        tone: "quiet",
        stateKey: "remote.state.unmanaged",
        verdictKey: "remote.verdict.unmanaged",
        primary: "bootstrap",
      };
  }
}

/** 卡住的原因能不能就地解決，以及用哪一顆按鈕。 */
export type RemoteRemedy =
  | "stopAppOwned"
  | "pairChatGpt"
  | "activateChatGpt"
  | "repair"
  | "installCodex"
  | "updateAgent"
  | "bootstrap"
  | "plan";

interface BlockerEntry {
  remedy: RemoteRemedy | null;
  /** 尚未部署的主機本來就會有這些「缺項」。它們是待辦，不是故障。 */
  expectedBeforeBootstrap: boolean;
}

const BLOCKERS: Record<string, BlockerEntry> = {
  agentUnavailable: { remedy: "bootstrap", expectedBeforeBootstrap: true },
  codexRestartRequired: { remedy: "repair", expectedBeforeBootstrap: false },
  credentialsMissing: { remedy: "bootstrap", expectedBeforeBootstrap: true },
  desktopOfficialAccountMissing: { remedy: null, expectedBeforeBootstrap: false },
  dockerUnavailable: { remedy: null, expectedBeforeBootstrap: false },
  intelMacUnsupported: { remedy: null, expectedBeforeBootstrap: false },
  guiSessionUnavailable: { remedy: null, expectedBeforeBootstrap: false },
  proxyPortConflict: { remedy: null, expectedBeforeBootstrap: false },
  insufficientDiskSpace: { remedy: null, expectedBeforeBootstrap: false },
  incompleteObservation: { remedy: null, expectedBeforeBootstrap: false },
  hostNotConfigured: { remedy: "bootstrap", expectedBeforeBootstrap: true },
  injectionRequiresReadyProxy: { remedy: "bootstrap", expectedBeforeBootstrap: true },
  invalidCompactionThreshold: { remedy: null, expectedBeforeBootstrap: false },
  managedRuntimeRecoveryRequired: { remedy: "repair", expectedBeforeBootstrap: false },
  nativeCodexVersionMismatch: { remedy: "installCodex", expectedBeforeBootstrap: false },
  nativeDaemonAppOwned: { remedy: "stopAppOwned", expectedBeforeBootstrap: false },
  noModelsSelected: { remedy: "plan", expectedBeforeBootstrap: false },
  officialAccountActivationRequired: { remedy: "activateChatGpt", expectedBeforeBootstrap: false },
  officialAccountPairingRequired: { remedy: "pairChatGpt", expectedBeforeBootstrap: false },
  proxyConfigurationMissing: { remedy: "bootstrap", expectedBeforeBootstrap: true },
  // A newer schema than this build understands, or a config file that
  // couldn't be read at all -- both must block deployment outright rather
  // than reading as "just redeploy it" (see plan()'s
  // configuration_blocked_reasons). Never expected on a fresh host, so
  // never filtered out by `visibleBlockers`'s `fresh` pass.
  proxyConfigurationSchemaTooNew: { remedy: null, expectedBeforeBootstrap: false },
  proxyConfigurationUnreadable: { remedy: null, expectedBeforeBootstrap: false },
  systemdUserUnavailable: { remedy: null, expectedBeforeBootstrap: false },
  versionMismatch: { remedy: "updateAgent", expectedBeforeBootstrap: false },
};

/**
 * `host.status`'s `configuration.state` -- observed independently of
 * whether a deployment plan currently blocks on anything. `upgradeRequired`
 * and `repairRequired` never block (a normal deploy safely converges them),
 * so they never appear via `describeBlocker`/`visibleBlockers`; this is the
 * only place the Remote Manager surfaces them at all.
 *
 * `current` and `missing` return null on purpose: `current` is the healthy,
 * silent case, and `missing` is already covered by the existing
 * not-yet-deployed messaging elsewhere on this screen.
 */
export function configurationStateKey(state: string | undefined | null): string | null {
  switch (state) {
    case "upgradeRequired":
      return "remote.configuration.upgradeRequired";
    case "repairRequired":
      return "remote.configuration.repairRequired";
    case "incompatible":
      return "remote.configuration.incompatible";
    case "unreadable":
      return "remote.configuration.unreadable";
    default:
      return null;
  }
}

export interface RemoteBlocker {
  code: string;
  /** 認得的代號才有人話；認不得的回 null，呼叫端顯示原碼。 */
  messageKey: string | null;
  remedy: RemoteRemedy | null;
}

/**
 * 後端的代號帶著參數時是 `credentialMissing:cc` 這種形狀。冒號前才是代號。
 */
function blockerCode(reason: string): string {
  const separator = reason.indexOf(":");
  return separator === -1 ? reason : reason.slice(0, separator);
}

export function describeBlocker(reason: string): RemoteBlocker {
  const code = blockerCode(reason);
  const entry = BLOCKERS[code];
  return {
    code: reason,
    messageKey: entry ? `remote.blocker.${code}` : null,
    remedy: entry?.remedy ?? null,
  };
}

/**
 * 把「還沒部署」的待辦從故障清單裡拿掉。
 *
 * 一台乾淨的新機一定會同時報 proxyConfigurationMissing、credentialsMissing
 * 與 injectionRequiresReadyProxy —— 那三個就是「你還沒按部署」的三種說法。
 * 把它們當紅字列出來，等於第一次打開這頁就看到三個錯誤。
 *
 * `fresh` 由呼叫端決定：主機清單看 managerState，部署計畫則永遠是 false
 * —— 計畫特地把缺項算出來給人看，這裡不該再幫它過濾掉。
 */
export function visibleBlockers(reasons: string[], fresh: boolean): RemoteBlocker[] {
  const seen = new Set<string>();
  const blockers: RemoteBlocker[] = [];
  for (const reason of reasons) {
    const code = blockerCode(reason);
    if (fresh && BLOCKERS[code]?.expectedBeforeBootstrap) continue;
    if (seen.has(reason)) continue;
    seen.add(reason);
    blockers.push(describeBlocker(reason));
  }
  return blockers;
}

/**
 * 背景操作的階段。
 *
 * 刻意不做「第 N 步／共 M 步」：bootstrap 的 installCodex 與 nativeDaemon
 * 是有條件才跑的，restore 的 restartNative 也是，所以總步數不是定值。
 * 編一個分母出來會在跳號的時候被抓到，而那比沒有分母更糟。
 *
 * 真正缺的是「現在這一步在做什麼」跟「跑多久了」—— 一條停在 35% 的進度條
 * 沒辦法讓人分辨還在跑跟卡住了。
 */
const PHASES = new Set([
  "queued",
  "cleanHostPreflight",
  "hostPreflight",
  "resolvingDesktopCodex",
  "installCodex",
  "nativeDaemon",
  "deploymentPlan",
  "deploymentApply",
  "applying",
  "verification",
  "verified",
  "restorePreflight",
  "restoreLease",
  "restartNative",
  "stopProxy",
  "restoreVerification",
  "restored",
  "failed",
]);

/** 認得的階段才翻譯；認不得的回 null，呼叫端原樣顯示代號。 */
export function phaseKey(phase: string): string | null {
  return PHASES.has(phase) ? `remote.phase.${phase}` : null;
}

/** 操作種類 → 標題。「正在部署」比 `oneClickBootstrap` 好懂。 */
export function operationKindKey(kind: string): string | null {
  switch (kind) {
    case "oneClickBootstrap":
      return "remote.operation.bootstrap";
    case "applyDeployment":
      return "remote.operation.apply";
    case "oneClickRestore":
      return "remote.operation.restore";
    case "desktopCodexSync":
      return "remote.operation.desktopCodexSync";
    default:
      return null;
  }
}

/**
 * 已經跑了多久，mm:ss。
 *
 * 起算點是前端第一次看到這個 operation 的時間，不是後端的 updatedAt ——
 * updatedAt 每次輪詢都會變，拿它算等於一直顯示 0 秒。
 */
export function elapsedLabel(startedAt: number, now: number): string {
  const seconds = Math.max(0, Math.floor((now - startedAt) / 1000));
  const minutes = Math.floor(seconds / 60);
  return `${minutes}:${String(seconds % 60).padStart(2, "0")}`;
}
