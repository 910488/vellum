import type { CodexOAuthAccount } from "../types";

function clean(value: string | null | undefined): string | null {
  const result = value?.trim();
  return result ? result : null;
}

export function codexWorkspaceLabel(account: CodexOAuthAccount): string {
  const name = clean(account.workspaceName);
  const plan = clean(account.planType)?.toLocaleLowerCase();
  const kind =
    account.workspaceKind ??
    (["free", "go", "plus", "pro"].includes(plan ?? "")
      ? "personal"
      : ["team", "business", "enterprise", "edu"].includes(plan ?? "")
        ? "business"
        : "unknown");
  if (kind === "personal") return "Personal";
  if (kind === "business") {
    if (name && !["personal", "business"].includes(name.toLocaleLowerCase())) {
      return `Business · ${name}`;
    }
    return "Business";
  }
  return (
    name ??
    clean(account.planType) ??
    `Workspace ${(account.workspaceId ?? account.accountId).slice(0, 8)}`
  );
}

export function codexAccountLabel(account: CodexOAuthAccount): string {
  return `${account.email ?? `ChatGPT ${account.accountId.slice(0, 8)}`} · ${codexWorkspaceLabel(account)}`;
}

/** Compact pool bars identify the billed workspace because one email can own
 * both Personal and Business credentials. */
export function codexPoolSegmentLabel(account: CodexOAuthAccount): string {
  return codexWorkspaceLabel(account).toLocaleUpperCase();
}
