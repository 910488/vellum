import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import i18n from "i18next";
import "@/i18n";
import { assertResourceParity } from "@/i18n/resources";
import { RemoteWorkSurface } from "@/components/RemoteWorkSurface";
import { remoteWorkSurface, type WorkSurfaceInput } from "@/lib/remoteWorkSurface";
import { compareVersions, desktopRuntimeAhead, resumeConvergeAfterAccount } from "@/lib/remoteVocabulary";
import type { RemoteOperationProgress } from "@/types";

const pending: WorkSurfaceInput = { managerState: "unmanaged", agentReachable: true, blockers: [], operation: null };
const operation: RemoteOperationProgress = {
  operationId: "deploy", hostId: "host", kind: "oneClickBootstrap", phase: "deploymentApply",
  percent: 62, state: "running", message: null, result: null, updatedAt: "2026-10-16T00:00:00Z",
};
const states = {
  pending,
  waiting: { ...pending, operation: { ...operation, phase: "waitingForIdle" } },
  running: { ...pending, operation },
  failed: { ...pending, error: "OneClickBootstrapBlocked: nativeDaemonAppOwned" },
  ready: { ...pending, managerState: "detachedReady", persistenceScope: "onDemand" },
} satisfies Record<string, WorkSurfaceInput>;

afterEach(() => { cleanup(); });

describe("remote deployment presentation", () => {
  it.each(Object.entries(states))("derives the %s work surface without changing input", (state, input) => {
    const before = JSON.stringify(input);
    expect(remoteWorkSurface(input).state).toBe(state);
    expect(JSON.stringify(input)).toBe(before);
  });

  it("uses deploy for a fresh host, and redeploy for a managed host", () => {
    expect(remoteWorkSurface(pending).primaryKey).toBe("remote.act.bootstrap");
    expect(remoteWorkSurface(states.ready)).toMatchObject({ primaryKey: "remoteWork.redeploy", primaryDisabled: false, statusKey: "remoteWork.readyOnDemand", remedy: null });
    expect(remoteWorkSurface({ ...states.ready, persistenceScope: "linger" }).statusKey).toBe("remote.verdict.detachedReady");
    expect(remoteWorkSurface({ ...pending, agentReachable: false }).statusKey).toBe("remote.verdict.unreachable");
    expect(remoteWorkSurface({ ...states.ready, agentReachable: false })).toMatchObject({ state: "pending", statusKey: "remote.verdict.unreachable" });
  });

  it.each(["waitingForIdle", "awaitingTakeover", "observing", "deploymentApply"])("disables deployment while %s without treating observation as active work", (phase) => {
    const model = remoteWorkSurface({ ...pending, operation: { ...operation, phase } });
    expect(model.primaryDisabled).toBe(true);
    expect(model.remedy).toBeNull();
    expect(model.state).toBe(["waitingForIdle", "awaitingTakeover"].includes(phase) ? "waiting" : "running");
    if (phase === "observing") expect(model.statusKey).toBe("remote.phase.observing");
  });

  it.each([
    ["nativeDaemonAppOwned", "stopAppOwned"],
    ["officialAccountPairingRequired", "pairChatGpt"],
    ["officialAccountActivationRequired", "activateChatGpt"],
    ["officialAccountReauthenticationRequired", "pairChatGpt"],
    ["managedRuntimeRecoveryRequired", "bootstrap"],
    ["nativeCodexVersionMismatch", "bootstrap"],
    ["DeploymentAgentUpgradeRequired", "bootstrap"],
    ["codexRuntimeIncomplete", "bootstrap"],
    ["noModelsSelected", "inspect"],
    ["proxyPortConflict", "inspect"],
    ["unsupportedRemotePlatform", "inspect"],
    ["windowsOpenSshUnavailable", "inspect"],
    ["CodexDesktopProtocolMismatch", "inspect"],
    ["ProxyMigrationRecoveryFailed", "inspect"],
    ["ProxyMigrationRolledBack", "bootstrap"],
    ["OneClickBootstrapVerificationFailed", "bootstrap"],
    ["SSH connection timed out", "retry"],
    ["unknown_nativeDaemonAppOwned_suffix", "retry"],
  ])("selects one remedy for %s", (code, action) => {
    const model = remoteWorkSurface({ ...pending, error: `OneClickBootstrapBlocked: ${code}` });
    expect(model).toMatchObject({ state: "failed", remedy: { action } });
    expect(model.primaryDisabled).toBe(true);
    expect(model.progress).toBeNull();
  });

  it("uses the newest action failure, then operation failure, then the first blocker", () => {
    const input: WorkSurfaceInput = { ...pending, operation: { ...operation, state: "failed", message: "nativeDaemonAppOwned" }, blockers: ["noModelsSelected", "proxyPortConflict"] };
    expect(remoteWorkSurface(input).remedy?.action).toBe("stopAppOwned");
    expect(remoteWorkSurface({ ...input, error: "officialAccountPairingRequired" }).remedy?.action).toBe("pairChatGpt");
    expect(remoteWorkSurface({ ...input, operation: null }).remedy?.action).toBe("inspect");
    expect(remoteWorkSurface({ ...pending, blockers: ["futureBlocker"] }).remedy?.action).toBe("inspect");
  });

  it("keeps ps repair commands scoped to their blocker", () => {
    const error = 'OneClickBootstrapBlocked: psStartTimeUnavailable | unsupportedRemotePlatform; repairs: {"psStartTimeUnavailable":"apk add procps-ng","unsupportedRemotePlatform":"Use a supported host"}';
    expect(remoteWorkSurface({ ...pending, error }).remedy).toMatchObject({ action: "inspect", repair: "apk add procps-ng" });
  });

  it("offers only an available Desktop protocol repair", () => {
    const input = { ...pending, error: "CodexDesktopProtocolMismatch" };
    expect(remoteWorkSurface({ ...input, desktopCodexState: "updateAvailable" }).remedy?.action).toBe("syncDesktopCodex");
    expect(remoteWorkSurface({ ...input, desktopCodexState: "agentUpdateRequired" }).remedy?.action).toBe("updateAgent");
    expect(remoteWorkSurface({ ...input, desktopCodexState: "desktopUnavailable" }).remedy?.action).toBe("inspect");
  });

  it("does not present a cancelled deployment as a failure and keeps unknown phases readable", () => {
    expect(remoteWorkSurface({ ...pending, operation: { ...operation, state: "cancelled" } }).state).toBe("pending");
    expect(remoteWorkSurface({ ...pending, operation: { ...operation, phase: "futurePhase", percent: 120 } }).progress).toMatchObject({ phaseKey: null, phase: "futurePhase", percent: 100 });
  });
});

describe("work surface in all four languages", () => {
  it.each(["zh-TW", "zh-CN", "ja", "en"])("keeps failure details inside the margin after the remedy in %s", async (locale) => {
    await i18n.changeLanguage(locale);
    render(<RemoteWorkSurface model={remoteWorkSurface(states.failed)} disabled={false} remedyDisabled={false}
      elapsed={undefined} onDeploy={() => {}} onRemedy={() => {}}
      details={<details className="remote-work-details"><summary>{i18n.t("remoteWork.details")}</summary><pre>{"long/path/".repeat(100)}</pre></details>} />);
    const region = screen.getByRole("region", { name: i18n.t("remoteWork.label") });
    const details = screen.getByText(i18n.t("remoteWork.details")).closest("details");
    expect(details?.parentElement).toBe(region);
    expect(details?.previousElementSibling?.classList.contains("remote-work__remedy")).toBe(true);
    expect(details?.open).toBe(false);
  });

  it("keeps resource keys in parity", () => { expect(() => assertResourceParity()).not.toThrow(); });

  it("keeps manual repair as a single instruction and respects existing action gates", () => {
    const deploy = vi.fn();
    const remedy = vi.fn();
    const model = remoteWorkSurface({ ...pending, error: "psStartTimeUnavailable; repair: apk add procps-ng" });
    const view = render(<RemoteWorkSurface model={model} disabled={true} remedyDisabled={true} elapsed={undefined} onDeploy={deploy} onRemedy={remedy} />);
    expect(screen.getAllByRole("button")).toHaveLength(1);
    expect(screen.getByText("apk add procps-ng")).toBeTruthy();
    fireEvent.click(screen.getByRole("button"));
    expect(deploy).not.toHaveBeenCalled();
    view.rerender(<RemoteWorkSurface model={remoteWorkSurface(states.failed)} disabled={true} remedyDisabled={true} elapsed={undefined} onDeploy={deploy} onRemedy={remedy} />);
    for (const button of screen.getAllByRole<HTMLButtonElement>("button")) {
      expect(button.disabled).toBe(true);
      fireEvent.click(button);
    }
    expect(remedy).not.toHaveBeenCalled();
  });

  it.each(["zh-TW", "zh-CN", "ja", "en"])("renders five states with translated actions in %s", async (locale) => {
    await i18n.changeLanguage(locale);
    for (const input of Object.values(states)) {
      const model = remoteWorkSurface(input);
      const deploy = vi.fn();
      const remedy = vi.fn();
      render(<RemoteWorkSurface model={model} disabled={false} remedyDisabled={false} elapsed={undefined} onDeploy={deploy} onRemedy={remedy} />);
      const surface = within(screen.getByRole("region", { name: i18n.t("remoteWork.label") }));
      expect(surface.getByRole("status").textContent).toBe(i18n.t(model.statusKey));
      const primary = surface.getByRole<HTMLButtonElement>("button", { name: i18n.t(model.primaryKey) });
      expect(primary.disabled).toBe(model.primaryDisabled);
      if (!model.primaryDisabled) { fireEvent.click(primary); expect(deploy).toHaveBeenCalledOnce(); }
      expect(surface.queryAllByRole("progressbar").length).toBe(model.progress ? 1 : 0);
      if (model.state === "failed") {
        const next = surface.getByRole("button", { name: i18n.t("remoteWork.nextAction", { action: i18n.t("remote.act.stopAppOwned") }) });
        fireEvent.click(next);
        expect(remedy).toHaveBeenCalledOnce();
        expect(remedy).toHaveBeenCalledWith("stopAppOwned");
        expect(surface.getAllByRole("button")).toHaveLength(2);
      }
      cleanup();
    }
  });
});

describe("keeping a host in step without a second button", () => {
  const ahead = { state: "updateAvailable", canUpdate: true, desktopVersion: "0.150.0", remoteVersion: "codex-cli 0.147.0-alpha.6.6" };

  it("orders Codex versions the way the backend deploy does", () => {
    expect(compareVersions("0.150.0", "0.147.0-alpha.6.6")).toBeGreaterThan(0);
    expect(compareVersions("0.147.0-alpha.6.7", "0.147.0-alpha.6.6")).toBeGreaterThan(0);
    expect(compareVersions("0.147.0-alpha.6.10", "0.147.0-alpha.6.9")).toBeGreaterThan(0);
    expect(compareVersions("0.147.0", "0.147.0-alpha.6.6")).toBeGreaterThan(0);
    expect(compareVersions("0.147.0-alpha.6.5", "0.147.0-alpha.6.6")).toBeLessThan(0);
    expect(compareVersions("0.147.0", "codex-cli 0.147.0")).toBe(0);
    expect(compareVersions("unknown", "0.147.0")).toBeNull();
  });

  it("says a redeploy catches up only when Desktop is strictly newer", () => {
    expect(desktopRuntimeAhead(ahead)).toBe(true);
    expect(desktopRuntimeAhead({ ...ahead, desktopVersion: "0.147.0-alpha.6.5" })).toBe(false);
    expect(desktopRuntimeAhead({ ...ahead, canUpdate: false })).toBe(false);
    expect(desktopRuntimeAhead({ ...ahead, state: "current" })).toBe(false);
    expect(desktopRuntimeAhead(null)).toBe(false);
    const ready = { ...pending, managerState: "nativeActive" };
    expect(remoteWorkSurface({ ...ready, desktopRuntimeAhead: true })).toMatchObject({ state: "ready", statusKey: "remoteWork.desktopAhead", primaryKey: "remoteWork.redeploy", remedy: null });
    expect(remoteWorkSurface(ready).statusKey).toBe("remote.verdict.nativeActive");
  });

  it("finishes the deferred models once the account is paired", () => {
    const done = { hostId: "host", kind: "oneClickBootstrap", state: "completed", message: null };
    expect(resumeConvergeAfterAccount({ ...done, result: { chatgptPairingSuggested: true } }, "host")).toBe(true);
    expect(resumeConvergeAfterAccount({ ...done, result: { chatgptPairingSuggested: false } }, "host")).toBe(false);
    expect(resumeConvergeAfterAccount({ ...done, result: { chatgptPairingSuggested: true } }, "other")).toBe(false);
    expect(resumeConvergeAfterAccount({ ...done, state: "failed", message: "officialAccountPairingRequired" }, "host")).toBe(true);
    expect(resumeConvergeAfterAccount({ ...done, kind: "oneClickRestore", result: { chatgptPairingSuggested: true } }, "host")).toBe(false);
  });
});
