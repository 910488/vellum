import { describe, expect, it } from "vitest";
import { describeBlocker, onDemandPsFailure, visibleBlockers } from "@/lib/remoteVocabulary";

describe("onDemand prerequisites", () => {
  it("keeps a missing ps prerequisite visible on a fresh host", () => {
    const reason = "onDemandPsUnavailable";
    const blocker = describeBlocker(reason);
    expect(blocker.messageKey).toBe("remote.blocker.onDemandPsUnavailable");
    expect(blocker.remedy).toBeNull();
    expect(visibleBlockers([reason, reason], true)).toEqual([blocker]);
  });

  it("recognizes the prerequisite code when the backend includes repair details", () => {
    expect(describeBlocker("onDemandPsUnavailable:apk add procps-ng").messageKey)
      .toBe("remote.blocker.onDemandPsUnavailable");
  });

  it("shows one line of repair advice only for the ps prerequisite", () => {
    expect(onDemandPsFailure("OneClickBootstrapBlocked: onDemandPsUnavailable; repair: apk add procps-ng\nextra details"))
      .toEqual({ repair: "apk add procps-ng" });
    expect(onDemandPsFailure("DeploymentPreflightBlocked: onDemandPsUnavailable; repair: apt-get install procps"))
      .toEqual({ repair: "apt-get install procps" });
    expect(onDemandPsFailure("onDemandPsUnavailable")).toEqual({ repair: null });
    expect(onDemandPsFailure("SSH failed; repair: reconnect")).toBeNull();
    expect(onDemandPsFailure(null)).toBeNull();
  });
});
