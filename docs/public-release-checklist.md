# Public release checklist

Run this checklist against the exact commit that will become public.

## Current tree

1. Confirm `git status --short` contains only intended release changes.
2. Run Gitleaks with the repository configuration:

   ```text
   gitleaks dir . --redact
   ```

3. Search tracked files for personal paths, private network addresses, account
   identifiers, internal hostnames, and credential-bearing URLs.
4. Run the repository validation commands from `AGENTS.md` and `BUILDING.md`.

## Git history

A clean working tree does not make existing history safe. Before changing
repository visibility, scan every ref intended for publication:

```text
gitleaks git . --redact --log-opts="--all"
```

Review scanner allowlist hits manually. If a real secret ever entered Git,
revoke or rotate it first, then rewrite all published refs and coordinate the
required force push. If history contains private hostnames, paths, emails, or
operational records that must not be public, rewrite or publish a new sanitized
repository; deleting them only from the tip is insufficient.

## Final checks

- Verify the public remote URL and default branch.
- Verify branch protection, required reviews, and secret scanning are enabled.
- Do not publish local worktrees, build output, logs, support bundles, eval run
  artifacts, credential stores, or generated remote payloads.
- Make a fresh clone from the candidate public remote and repeat the tree scan.
