/**
 * i18n-resources 那一支只保證四個語言彼此的鍵相同 —— 四個一起漏掉同一個鍵，
 * 它是綠的。畫面上看到的會是原樣印出來的 `remote.act.bootstrap`。
 *
 * 這一支補另一半：Remote 這一頁真的會查的鍵，四個語言都要有值。
 * 靜態鍵從原始碼抽，動態鍵（blocker 代號、phase、確認視窗的三格）從
 * remoteVocabulary 的真實清單展開 —— 那份清單本身對齊後端送得出來的值。
 */
import { describe, expect, it } from "vitest";
import remoteSource from "../src/screens/Remote.tsx?raw";
import uiSource from "../src/components/ui.tsx?raw";
import zhTW from "@/i18n/locales/zh-TW";
import zhCN from "@/i18n/locales/zh-CN";
import en from "@/i18n/locales/en";
import ja from "@/i18n/locales/ja";
import type { ResourceTree } from "@/i18n/resources";

const LOCALES: Record<string, ResourceTree> = { "zh-TW": zhTW, "zh-CN": zhCN, en, ja };

/** 後端會送出來的 blocker 代號。跟 remoteVocabulary 的 BLOCKERS 一致。 */
const BLOCKER_CODES = [
  "agentUnavailable", "codexRestartRequired", "credentialsMissing",
  "desktopOfficialAccountMissing", "dockerUnavailable", "hostNotConfigured",
  "injectionRequiresReadyProxy", "invalidCompactionThreshold",
  "managedRuntimeRecoveryRequired", "nativeCodexVersionMismatch",
  "nativeDaemonAppOwned", "noModelsSelected", "officialAccountActivationRequired",
  "officialAccountPairingRequired", "proxyConfigurationMissing",
  "systemdUserUnavailable", "versionMismatch",
];

/** 三個 operation kind 走得到的 phase。 */
const PHASES = [
  "queued", "cleanHostPreflight", "hostPreflight", "installCodex", "nativeDaemon",
  "deploymentPlan", "deploymentApply", "applying", "verification", "verified",
  "restorePreflight", "restoreLease", "restartNative", "stopProxy",
  "restoreVerification", "restored", "failed",
];

const CONFIRM_KINDS = [
  "bootstrap", "restore", "restartNative", "takeoverNative",
  "installCodex", "updateAgent", "stopAppOwned",
];

/** 帶 count 的鍵在資源樹裡是 _one / _other 兩個條目。 */
const PLURAL = new Set([
  "remote.summary.threads",
  "remote.detail.cores",
  "remote.sessions.turns",
  // ui.tsx 的 Pager 也走複數，抽取時會一起被撈進來。
  "common.totalItems",
]);

function collectKeys(): string[] {
  const keys = new Set<string>();
  for (const source of [remoteSource, uiSource]) {
    // t("…") 直接查的鍵
    for (const match of source.matchAll(/t\(\s*"([a-zA-Z0-9_.]+)"/g)) {
      if (match[1]) keys.add(match[1]);
    }
    // 先存成變數再查的鍵（primaryActionKey、restartKey、chore 的三元式…）
    for (const match of source.matchAll(/"(remote\.[a-zA-Z0-9_.]+)"/g)) {
      if (match[1]) keys.add(match[1]);
    }
  }
  for (const code of BLOCKER_CODES) keys.add(`remote.blocker.${code}`);
  for (const phase of PHASES) keys.add(`remote.phase.${phase}`);
  for (const kind of CONFIRM_KINDS) {
    for (const fact of ["changes", "untouched", "turns"]) {
      keys.add(`remote.confirm.${kind}.${fact}`);
    }
  }
  for (const state of ["synchronized", "pairingRequired", "pairingPending", "activationRequired", "desktopAccountUnavailable"]) {
    keys.add(`remote.account.chatgpt.${state}`);
  }
  for (const mode of ["nativeAppServer", "unsupported", "daemonDown"]) {
    keys.add(`remote.sessions.observability.${mode}`);
  }
  for (const tone of ["info", "warn", "error"]) keys.add(`common.notice.${tone}`);
  for (const kind of ["bootstrap", "apply", "restore"]) keys.add(`remote.operation.${kind}`);
  for (const state of ["unreachable", "unmanaged", "readyToPlan", "drifted", "nativeActive", "detachedReady"]) {
    keys.add(`remote.state.${state}`);
    keys.add(`remote.verdict.${state}`);
  }
  return [...keys].sort();
}

function lookup(tree: ResourceTree, key: string): string | undefined {
  let node: unknown = tree;
  for (const part of key.split(".")) {
    if (typeof node !== "object" || node === null) return undefined;
    node = (node as ResourceTree)[part];
  }
  return typeof node === "string" ? node : undefined;
}

describe("Remote screen translation keys", () => {
  const keys = collectKeys();

  it("extracts a meaningful key set from the screen source", () => {
    // 抽取壞掉時（例如改了呼叫寫法）這一支會安靜地通過，所以先釘一個下限。
    expect(keys.length).toBeGreaterThan(120);
    expect(keys).toContain("remote.act.bootstrap");
    expect(keys).toContain("remote.act.reconverge");
    expect(keys).toContain("remote.chore.takeoverNative");
  });

  for (const [locale, tree] of Object.entries(LOCALES)) {
    it(`resolves every key in ${locale}`, () => {
      const missing = keys.flatMap((key) => (
        PLURAL.has(key) ? [`${key}_one`, `${key}_other`] : [key]
      )).filter((key) => lookup(tree, key) === undefined);
      expect(missing).toEqual([]);
    });
  }
});
