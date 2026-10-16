import { describe, expect, it } from "vitest";
import { convergeCredentialFailure, convergePairingSuggested, describeBlocker, deploymentPreflightFailure, deploymentVerificationFailure, phaseKey, psStartTimeFailure, proxyMigrationFailure, resumeConvergeAfterAccount, visibleBlockers } from "@/lib/remoteVocabulary";

describe("Linux process start-time prerequisites", () => {
  it("explains deployment preflight refusals and the auto-fix phase", () => {
    for (const code of ["bundledReleaseUnavailable", "activeWorkInProgress", "unsupportedRemotePlatform", "insufficientDiskSpace", "proxyPortConflict", "proxyConfigurationSchemaTooNew"]) {
      expect(deploymentPreflightFailure(`OneClickBootstrapBlocked: ${code}`)).toBe(`remote.blocker.${code}`);
      expect(describeBlocker(code).messageKey).toBe(`remote.blocker.${code}`);
    }
    expect(deploymentPreflightFailure("SSH reply lost")).toBeNull();
    expect(deploymentPreflightFailure(null)).toBeNull();
    expect(phaseKey("autoFix")).toBe("remote.phase.autoFix");
    expect(describeBlocker("bundledReleaseUnavailable").remedy).toBeNull();
  });
  it("resumes only the failed converge for this host after its account remedy", () => {
    const operation = { hostId: "host", kind: "oneClickBootstrap", state: "failed", message: "officialAccountPairingRequired" };
    expect(resumeConvergeAfterAccount(operation, "host")).toBe(true);
    expect(resumeConvergeAfterAccount(operation, "another")).toBe(false);
    expect(resumeConvergeAfterAccount({ ...operation, state: "completed" }, "host")).toBe(false);
    expect(resumeConvergeAfterAccount({ ...operation, kind: "restore" }, "host")).toBe(false);
    expect(resumeConvergeAfterAccount({ ...operation, message: "SSH failed" }, "host")).toBe(false);
    expect(resumeConvergeAfterAccount(null, "host")).toBe(false);
  });
  it("offers one-account pairing only for a credential blocker and keeps optional pairing separate", () => {
    expect(convergeCredentialFailure("officialAccountPairingRequired"))
      .toEqual({ messageKey: "remote.blocker.officialAccountPairingRequired", remedy: "pairChatGpt" });
    expect(convergeCredentialFailure("officialAccountActivationRequired")?.remedy).toBe("activateChatGpt");
    expect(convergeCredentialFailure("officialAccountReauthenticationRequired")?.remedy).toBe("pairChatGpt");
    expect(convergeCredentialFailure("noDeployableModels")?.remedy).toBeNull();
    expect(convergeCredentialFailure("officialAccountStatusUnavailable")?.remedy).toBe("bootstrap");
    expect(convergeCredentialFailure(null)).toBeNull();
    expect(convergePairingSuggested({ chatgptPairingSuggested: true })).toBe(true);
    for (const result of [null, undefined, "ready", {}, { chatgptPairingSuggested: "true" }, { chatgptPairingSuggested: false }]) {
      expect(convergePairingSuggested(result)).toBe(false);
    }
  });
  it("explains verification retry exhaustion and offers reconvergence", () => {
    expect(deploymentVerificationFailure("OneClickBootstrapVerificationFailed: retryExhausted"))
      .toBe("remote.blocker.deploymentVerificationFailed");
    expect(deploymentVerificationFailure("OneClickBootstrapVerificationFailed: proxyPortMatches=false"))
      .toBe("remote.blocker.deploymentVerificationFailed");
    expect(deploymentVerificationFailure("SSH failed")).toBeNull();
    expect(deploymentVerificationFailure(null)).toBeNull();
    expect(describeBlocker("deploymentVerificationFailed").remedy).toBe("bootstrap");
    expect(phaseKey("verificationRetry")).toBe("remote.phase.verificationRetry");
  });
  it("distinguishes recovered migrations, failed recovery, and missing agent support", () => {
    expect(proxyMigrationFailure("ProxyMigrationRolledBack: start failed")).toBe("remote.blocker.proxyMigrationRolledBack");
    expect(proxyMigrationFailure("ProxyMigrationRecoveryFailed: cause: ProxyMigrationRolledBack")).toBe("remote.blocker.proxyMigrationRecoveryFailed");
    expect(proxyMigrationFailure("ProxyMigrationAgentUpgradeRequired")).toBe("remote.blocker.proxyMigrationAgentUpgradeRequired");
    expect(proxyMigrationFailure(null)).toBeNull();
    expect(describeBlocker("proxyMigrationRolledBack").remedy).toBe("bootstrap");
    expect(describeBlocker("proxyMigrationRecoveryFailed").remedy).toBeNull();
    expect(describeBlocker("proxyMigrationAgentUpgradeRequired").remedy).toBe("updateAgent");
  });
  it("offers converge for an occupied saved port while preserving the exhausted-range blocker", () => {
    const occupied = describeBlocker("proxyPortOccupied");
    expect(occupied.messageKey).toBe("remote.blocker.proxyPortOccupied");
    expect(occupied.remedy).toBe("bootstrap");
    expect(visibleBlockers(["proxyPortOccupied"], true)).toEqual([occupied]);
    expect(describeBlocker("proxyPortConflict").remedy).toBeNull();
  });
  it("keeps an exhausted proxy port range visible even before first deployment", () => {
    const blocker = describeBlocker("proxyPortConflict");
    expect(blocker.messageKey).toBe("remote.blocker.proxyPortConflict");
    expect(blocker.remedy).toBeNull();
    expect(visibleBlockers(["proxyPortConflict"], true)).toEqual([blocker]);
  });
  it("lets bootstrap choose persistence when a legacy host reports no systemd", () => {
    expect(describeBlocker("systemdUserUnavailable").remedy).toBe("bootstrap");
    expect(visibleBlockers(["systemdUserUnavailable"], true)).toEqual([]);
  });
  it("keeps a missing ps prerequisite visible on a fresh host", () => {
    const reason = "psStartTimeUnavailable";
    const blocker = describeBlocker(reason);
    expect(blocker.messageKey).toBe("remote.blocker.psStartTimeUnavailable");
    expect(blocker.remedy).toBeNull();
    expect(visibleBlockers([reason, reason], true)).toEqual([blocker]);
  });

  it("recognizes the prerequisite code when the backend includes repair details", () => {
    expect(describeBlocker("psStartTimeUnavailable:apk add procps-ng").messageKey)
      .toBe("remote.blocker.psStartTimeUnavailable");
  });

  it("shows one line of repair advice only for the ps prerequisite", () => {
    expect(psStartTimeFailure("OneClickBootstrapBlocked: psStartTimeUnavailable; repair: apk add procps-ng\nextra details"))
      .toEqual({ repair: "apk add procps-ng" });
    expect(psStartTimeFailure("DeploymentPreflightBlocked: psStartTimeUnavailable; repair: apt-get install procps"))
      .toEqual({ repair: "apt-get install procps" });
    expect(psStartTimeFailure("psStartTimeUnavailable")).toEqual({ repair: null });
    expect(psStartTimeFailure("psStartTimeUnavailable; repair:")).toEqual({ repair: null });
    expect(psStartTimeFailure("SSH failed; repair: reconnect")).toBeNull();
    expect(psStartTimeFailure(null)).toBeNull();
  });
});
