import { describe, expect, it } from "vitest";
import type { CodexOAuthAccount } from "../types";
import { codexAccountLabel, codexPoolSegmentLabel, codexWorkspaceLabel } from "./codexAccount";

function account(overrides: Partial<CodexOAuthAccount>): CodexOAuthAccount {
  return {
    accountId: "chatgpt-credential",
    workspaceId: "workspace-12345678",
    workspaceName: null,
    planType: null,
    workspaceKind: "unknown",
    email: "person@example.test",
    authenticatedAt: 1,
    isDefault: false,
    ...overrides,
  };
}

describe("Codex account labels", () => {
  it("labels a personal workspace independently of reset availability", () => {
    expect(
      codexAccountLabel(account({ workspaceKind: "personal", planType: "plus" })),
    ).toBe("person@example.test · Personal");
  });

  it("ignores a stale Personal name on a Business workspace", () => {
    expect(
      codexAccountLabel(
        account({ workspaceKind: "business", workspaceName: "Personal", planType: "team" }),
      ),
    ).toBe("person@example.test · Business");
  });

  it("infers Business from a legacy response that has no workspace kind", () => {
    expect(
      codexAccountLabel(
        account({ workspaceKind: undefined, workspaceName: "Personal", planType: "team" }),
      ),
    ).toBe("person@example.test · Business");
  });

  it("keeps a real Business workspace name", () => {
    expect(
      codexWorkspaceLabel(
        account({ workspaceKind: "business", workspaceName: "Crypto", planType: "team" }),
      ),
    ).toBe("Business · Crypto");
  });

  it("distinguishes Personal and Business segments owned by the same email", () => {
    const personal = account({
      accountId: "personal",
      workspaceKind: "personal",
      planType: "plus",
    });
    const business = account({
      accountId: "business",
      workspaceKind: "business",
      planType: "team",
    });
    expect(codexPoolSegmentLabel(personal)).toBe("PERSONAL");
    expect(codexPoolSegmentLabel(business)).toBe("BUSINESS");
    expect(codexPoolSegmentLabel(personal)).not.toBe(codexPoolSegmentLabel(business));
  });
});
