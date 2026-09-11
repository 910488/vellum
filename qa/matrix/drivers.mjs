/**
 * Per-case driver specs. A case without a driver cannot PASS on the desktop,
 * live, remote, or sandbox lanes. Navigate cases may target the tab control;
 * every other case must name a distinct control or be an observe/live-session.
 */
import { CASES, TAB_CONTROL } from "./v1.mjs";

const EXPLICIT = {
  "tab.today.proxy-start": {
    action: { kind: "invoke", control: "啟動 Proxy", controlType: "Button", process: "Vellum" },
    wait: { process: "vellum-proxy-desktop", timeoutMs: 20000 },
    verify: { kind: "process-running", name: "vellum-proxy-desktop" },
  },
  "tab.today.proxy-stop-restore": {
    action: {
      kind: "invoke",
      control: "停止 Proxy 並還原 Codex",
      controlType: "Button",
      process: "Vellum",
    },
    wait: { processGone: "vellum-proxy-desktop", timeoutMs: 20000 },
    verify: { kind: "process-stopped", name: "vellum-proxy-desktop" },
  },
  "tab.today.add-provider": {
    action: { kind: "invoke", control: "加入供應商", controlType: "Button", process: "Vellum" },
    verify: { kind: "field-changed", field: "activeTab" },
  },
  "tab.today.statusbar-refresh": {
    action: { kind: "invoke", control: "重新整理", controlType: "Button", process: "Vellum" },
    verify: { kind: "field-changed", field: "refreshedAt" },
  },
  "tab.enhanced.recheck": {
    action: { kind: "invoke", control: "重新檢查相容性", controlType: "Button", process: "Vellum" },
    verify: { kind: "field-changed", field: "compatibilityCheckedAt" },
  },
  "tab.enhanced.export": {
    action: { kind: "invoke", control: "匯出診斷", controlType: "Button", process: "Vellum" },
    verify: { kind: "file-diff" },
  },
};

const CONTROL_BY_ID = {
  "tab.today.findings-adjust-context": "調整上下文視窗",
  "tab.models.add-provider-probe": "探測",
  "tab.models.add-provider-save": "儲存",
  "tab.models.add-provider-cancel": "取消",
  "tab.models.oauth-start": "登入 ChatGPT",
  "tab.models.oauth-cancel": "取消",
  "tab.models.grok-start": "登入 Grok",
  "tab.models.grok-cancel": "取消",
  "tab.models.logout-confirm": "登出全部",
  "tab.remote.discover": "重新整理",
  "tab.remote.plan": "變更模型…",
  "tab.remote.apply": "確認並同步",
  "tab.remote.bootstrap-confirm": "部署此主機",
  "tab.remote.bootstrap-cancel": "取消",
  "tab.remote.restore-confirm": "解除 Vellum 管理…",
  "tab.remote.restore-cancel": "取消",
  "tab.remote.stop-app-owned": "安全停止並重試",
  "tab.remote.retry": "重試",
  "tab.remote.bundle": "匯出診斷包",
  "tab.remote.update-agent": "更新遠端 Agent",
  "tab.remote.repair": "修復 codex launcher",
  "tab.remote.restart-native": "重啟遠端 Codex daemon",
  "tab.remote.install-codex": "安裝 pinned Codex CLI",
  "tab.remote.ssh-trust-confirm": "信任並繼續",
  "tab.remote.ssh-trust-cancel": "取消",
  "tab.remote.reapply-noop": "確認並同步",
  "tab.settings.restore-codex": "還原 Codex 原始設定",
  "tab.settings.restart-codex": "重新啟動 Codex",
  "tab.settings.restart-anyway": "仍要重啟",
  "tab.settings.exit": "結束 Vellum",
  "tab.settings.websearch-toggle": "第三方網頁搜尋",
  "onboarding.step-next": "下一步",
  "onboarding.connect-chatgpt": "連線 ChatGPT",
  "onboarding.connect-chatgpt-cancel": "取消",
  "onboarding.connect-grok": "連線 Grok",
  "onboarding.connect-grok-cancel": "取消",
  "onboarding.start-proxy": "啟動 Proxy",
  "onboarding.finish": "完成",
  "dialog.confirm-ok": "確認",
  "dialog.confirm-cancel": "取消",
  "tray.show": "顯示 Vellum",
  "tray.toggle-proxy": "啟動 Proxy",
  "tray.exit": "結束 Vellum",
  "remote.deploy.clean-host": "部署此主機",
  "remote.deploy.health": "重新整理",
  "remote.deploy.config-consistency": "確認並同步",
  "remote.deploy.reapply-noop": "確認並同步",
  "remote.deploy.reconnect": "重新整理",
  "remote.deploy.stop-cleanup": "解除 Vellum 管理…",
  "sandbox.os.uninstall": "結束 Vellum",
};

function actionKind(item) {
  if (item.automation === "live-session") return "live-session";
  if (item.automation === "docker-dev-host") return "remote-ui";
  if (item.automation === "desktop-ui-manual-checkpoint") return "manual-checkpoint";
  if (/\.navigate$/.test(item.id) || item.id === "onboarding.start") return "navigate";
  const tab = TAB_CONTROL[item.surface];
  const named = CONTROL_BY_ID[item.id] || (item.control && item.control !== tab ? item.control : null);
  if (!named) return "observe";
  return "invoke";
}

function verifyFor(item, kind) {
  if (item.id.includes("proxy-start") || item.id === "onboarding.start-proxy") {
    return { kind: "process-running", name: "vellum-proxy-desktop" };
  }
  if (item.id.includes("proxy-stop") || item.id === "tab.settings.exit" || item.id === "tray.exit") {
    return { kind: "process-stopped", name: "Vellum" };
  }
  if (item.id === "enhanced.core.attestation-pin") return { kind: "attestation-pin" };
  if (item.id.startsWith("enhanced.core.")) return { kind: "file-diff" };
  if (item.domain === "subagents" || item.domain === "guardian-review") {
    return { kind: "usage-source", source: "provider" };
  }
  if (kind === "navigate") return { kind: "field-changed", field: "activeTab" };
  if (kind === "observe") {
    return { kind: "ui-matches-backend", sources: item.verificationSources };
  }
  if (kind === "remote-ui") {
    return {
      kind: "host-state",
      requireReadyz: /health|clean-host|bootstrap-confirm|apply|repair|restart-native/.test(item.id),
      requireDaemonOwner: /restart-native|health|clean-host/.test(item.id) ? "codexCliDaemon" : null,
    };
  }
  if (kind === "live-session") return { kind: "file-diff" };
  if (kind === "manual-checkpoint") return { kind: "manual-checkpoint" };
  return { kind: "field-changed", field: "uiState" };
}

function specFromCase(item) {
  const explicit = EXPLICIT[item.id] ?? {};
  const kind = explicit.action?.kind || actionKind(item);
  const tab = TAB_CONTROL[item.surface];
  const control =
    explicit.action?.control ||
    CONTROL_BY_ID[item.id] ||
    (kind === "navigate" ? item.control || tab : item.control !== tab ? item.control : null) ||
    (kind === "observe" ? item.id : tab);
  return {
    prepare: item.preconditions,
    input: item.steps,
    action: {
      kind,
      control,
      controlType: explicit.action?.controlType || "Button",
      process: explicit.action?.process || (kind === "live-session" ? "ChatGPT" : "Vellum"),
    },
    wait: explicit.wait || { uiReady: true, timeoutMs: kind === "live-session" ? 600000 : 15000 },
    verify: explicit.verify || verifyFor(item, kind),
    restore: /uninstall|exit|stop-cleanup/.test(item.id) ? "destructive-last" : "none",
  };
}

const LANES_NEEDING_DRIVERS = new Set(["desktop", "live", "remote", "sandbox"]);

export const DRIVERS = Object.freeze(
  Object.fromEntries(
    CASES.filter((item) => LANES_NEEDING_DRIVERS.has(item.lane)).map((item) => [item.id, specFromCase(item)]),
  ),
);

export function driverFor(caseId) {
  const base = caseId.includes("::") ? caseId.split("::")[0] : caseId;
  return DRIVERS[caseId] ?? DRIVERS[base] ?? null;
}

export function remoteUiButtons() {
  return Object.entries(DRIVERS)
    .filter(([, spec]) => spec.action?.kind === "remote-ui")
    .map(([id, spec]) => ({ id, control: spec.action.control, process: spec.action.process }));
}
