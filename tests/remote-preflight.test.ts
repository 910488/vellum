import { describe, expect, it } from "vitest";
import { describeBlocker, psStartTimeFailure, proxyMigrationFailure, visibleBlockers } from "@/lib/remoteVocabulary";

describe("Linux process start-time prerequisites", () => {
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
    expect(psStartTimeFailure("SSH failed; repair: reconnect")).toBeNull();
    expect(psStartTimeFailure(null)).toBeNull();
  });
});
