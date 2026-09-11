# Vellum repository instructions

These instructions apply to the entire repository. Codex loads this file from
the repository root. Keep detailed rationale in the linked documents so this
file remains a short, enforceable index.

## Required reading

- Use `docs/README.md` as the documentation index and authority map.
- Before changing proxy routing, provider adapters, authentication,
  continuation, compaction, streaming, search, or provider error handling,
  read `docs/protocol-source-of-truth.md`.
- Before creating, switching, merging, deleting, or synchronizing branches or
  worktrees, read `docs/git-worktree-branch-policy.md`.
- Before producing installers or embedded remote images, read `BUILDING.md`.

## Product and protocol invariants

- `main` is the only product source of truth for Windows and macOS.
  `MAC_OS` is a build mirror and must not contain unique product changes.
- Shared proxy behavior belongs in `crates/vellum-proxy-runtime`. Do not fix
  only the Desktop bridge or `proxy_legacy.rs` when the headless daemon and
  bundled remote image require the same behavior.
- OpenAI Official Responses is a native passthrough contract. Do not translate
  it through a third-party Chat or Responses adapter, and do not redirect an
  Official `compaction_trigger` request to another endpoint.
- Third-party providers must receive only portable state. Never forward opaque
  Official reasoning or an unmaterialized foreign compaction item.
- Preserve the real failure category and bounded provider diagnostic. Never
  replace protocol, authentication, quota, continuation, or compaction errors
  with a generic high-demand message.
- Vellum owns canonical checkpoint assembly. A summarizer may return prose;
  provider output is not required to be perfect JSON.

## Change discipline

- Start feature work from current `origin/main` on a dedicated `codex/*`
  branch/worktree. Do not develop directly on `MAC_OS`.
- Treat protocol behavior and its tests as one change. If an invariant in
  `docs/protocol-source-of-truth.md` changes, update that document in the same
  commit.
- Do not hand-edit generated remote payloads under
  `src-tauri/resources/remote`. Build scripts own those artifacts.
- After proxy-source changes, do a fresh release build. `Resume` is permitted
  only when the source-fingerprinted image reference matches, and is not the
  default for validation or release handoff.
- Before declaring protocol work complete, run at minimum:
  `cargo test -p vellum-proxy-runtime --lib`, `pnpm typecheck`, the focused UI
  tests when UI changed, and `git diff --check`.
- Cross-provider continuation or compaction changes also require the relevant
  eval gate described in `evals/README.md`.

## Git safety

- Preserve user changes in every worktree. Inspect `git status` in each target
  worktree before merge, reset, removal, or branch deletion.
- Never force-update `main`. A force update of a mirror or recovery branch
  requires an archive tag, an explicit unique-commit audit, and
  `--force-with-lease`.
- After a change lands on `main`, synchronize `MAC_OS` according to
  `docs/git-worktree-branch-policy.md` and verify both remote refs resolve to
  the same commit.

## Why this file exists

OpenAI's Codex documentation says repository-root `AGENTS.md` guidance is
loaded before work and can be refined by files closer to the working
directory. It also documents that Git worktrees share repository metadata and
that one branch cannot be checked out in multiple worktrees at the same time.
See:

- https://learn.chatgpt.com/docs/agent-configuration/agents-md
- https://learn.chatgpt.com/docs/environments/git-worktrees
