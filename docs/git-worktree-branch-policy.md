# Git worktree and branch policy

## Purpose

This policy prevents platform drift, accidental branch deletion, and one
worktree overwriting another. It applies to humans, Codex tasks, release work,
and automation.

OpenAI's Codex documentation describes worktrees as independent checkouts that
share one repository's Git metadata. Git allows a branch to be checked out in
only one worktree at a time. Codex worktree chats may begin detached; create a
branch before retaining or sharing changes.

Official references:

- https://learn.chatgpt.com/docs/environments/git-worktrees
- https://learn.chatgpt.com/docs/agent-configuration/agents-md

## Branch roles

| Branch pattern | Role | Allowed unique product changes |
| --- | --- | --- |
| `main` | Sole Windows/macOS product source and release authority | Yes, after review/verification |
| `MAC_OS` | Compatibility/build mirror of `main` | No |
| `codex/<topic>` | Task branch created from current `origin/main` | Yes, until merged |
| `archive/*` tag | Read-only preservation before history repair | Historical only |

Do not implement a fix only on `MAC_OS`. macOS-specific code may exist in the
tree behind platform conditions, but it must land through `main`, then reach
`MAC_OS` by synchronization.

## Starting work

Before creating a worktree:

```text
git fetch --prune origin
git status --short --branch
git worktree list --porcelain
```

Confirm the intended base is current and confirm the proposed branch is not
already checked out elsewhere. New work uses a descriptive `codex/*` branch
based on `origin/main`.

PowerShell example from the primary checkout's parent directory:

```powershell
git -C .\vellum fetch --prune origin
git -C .\vellum worktree add -b codex/proxy-compaction .\.worktrees\vellum-proxy-compaction origin/main
```

macOS/Linux example:

```bash
git -C ./vellum fetch --prune origin
git -C ./vellum worktree add -b codex/proxy-compaction ./.worktrees/vellum-proxy-compaction origin/main
```

Use one task branch per worktree. Never attach the same branch to two
worktrees. Do not reuse `main` or `MAC_OS` as a scratch branch.

## During work

- Commit only files belonging to the task; unrelated worktree changes belong
  to their owner.
- Fetching is safe, but do not merge/rebase a dirty target worktree.
- Protocol changes must follow `docs/protocol-source-of-truth.md`.
- Do not copy generated build payloads between worktrees as source changes.
- Before handoff, record the branch, commit, validation commands and remaining
  local changes.

## Landing on main

The preferred sequence is:

1. Verify the task branch and inspect its diff against `origin/main`.
2. Update the task branch with current `origin/main` using the team's chosen
   merge/rebase strategy.
3. Run the required tests in the task worktree.
4. Merge through review, or fast-forward/cherry-pick into the clean `main`
   worktree when explicitly authorized.
5. Push `main` without force and verify `origin/main` resolves to the expected
   commit.
6. Synchronize `MAC_OS` immediately using the next section.

Never use a merge from an old, divergent `MAC_OS` branch as a shortcut for
landing macOS fixes. Audit and transplant the still-relevant commits onto a
branch based on current `main`.

## Synchronizing MAC_OS

Normal synchronization is a fast-forward because `MAC_OS` must contain no
unique commits.

First locate the worktree holding `MAC_OS`:

```text
git worktree list --porcelain
```

Then, from that clean worktree:

```text
git fetch --prune origin
git status --short --branch
git merge --ff-only origin/main
git push origin MAC_OS
git rev-parse origin/main
git rev-parse origin/MAC_OS
```

The two final hashes must be identical. If `--ff-only` fails, stop. Do not
blindly merge and do not force-push. Audit first:

```text
git log --oneline origin/main..MAC_OS
git log --oneline MAC_OS..origin/main
git status --short --branch
```

For a genuinely divergent mirror:

1. Preserve its tip with an annotated `archive/MAC_OS-pre-sync-YYYYMMDD` tag
   and push that tag.
2. Review every unique commit and transplant any still-required behavior onto
   a `codex/*` branch based on current `origin/main`.
3. Land and validate that branch through `main`.
4. Only after equivalence is proven, move `MAC_OS` to `origin/main` with an
   exact expected old SHA and `--force-with-lease`.
5. Verify local and remote `main`/`MAC_OS` hashes again.

## Dirty or occupied worktrees

A worktree with modified or untracked files is not disposable. Do not reset,
remove, clean, or switch its branch merely to free a branch name. Either finish
and commit its work, hand it off, or ask its owner to resolve it.

Before removing a worktree, verify all of the following:

```text
git -C <worktree> status --short --branch
git branch --contains <worktree-commit>
git worktree list --porcelain
```

Use ordinary `git worktree remove <path>` only after the target is clean and
its commits are reachable from a retained branch/tag. Run `git worktree prune`
only to remove stale metadata after the filesystem state is understood.

## Branch deletion

- Delete a task branch only after its commits are reachable from `main` or an
  intentional archive tag.
- Use `git branch -d`, not `-D`, for normal local cleanup.
- Delete a remote task branch only after verifying it is merged.
- Never delete `main` or `MAC_OS` as part of routine cleanup.
- A branch checked out in any worktree cannot be deleted; resolve that
  worktree first.

## Release handoff

Builds for both platforms are sourced from `main`; `MAC_OS` exists only for
compatibility with machines or workflows that still select that branch.

Before giving build instructions:

```text
git fetch origin
git rev-parse origin/main
git rev-parse origin/MAC_OS
git status --short --branch
```

If the hashes differ, synchronize first. Then follow `BUILDING.md`. Proxy
source changes require a non-resumed build so the embedded source-fingerprinted
image is rebuilt; after installation the user must run Remote Manager
“重新同步” to deploy that image.
