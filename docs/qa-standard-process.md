# Vellum 標準測試流程

Operational document. It does not override `protocol-source-of-truth.md`.

## Loop

功能清單 → 自動測試 → 真實操作 → 證據報告。

1. Update the versioned matrix in `qa/matrix/v1.mjs` (see [qa-acceptance-matrix.md](qa-acceptance-matrix.md)).
2. Run `pnpm qa -- --lane offline` (free; this is what CI runs).
3. Build a Windows release per `BUILDING.md` and record source / artifact hashes.
4. On an isolated Windows test account / login session, with a test workspace and a Codex home shared by native and Enhanced: `pnpm qa -- --lane desktop`.
5. Against independently named Docker resources: `pnpm qa -- --lane remote`.
6. Real sessions: `pnpm qa -- --lane live` (explicit; never scheduled as paid CI).
7. Publish JSON + HTML under `qa/reports/`. Necessary cases not all `PASS` ⇒ no 完整驗收通過.

`pnpm qa -- --lane all` is the same order in one process.

Windows Sandbox (clean PC): `pnpm qa -- --lane sandbox --build-info <build-info.json>`. See [qa-sandbox.md](qa-sandbox.md). The installer is the unique file named in that JSON. Necessary `BLOCKED`/`NOT_RUN` still yield a non-zero exit.

## Verdicts

Only `PASS` / `FAIL` / `BLOCKED` / `NOT_RUN`.

| Verdict | Meaning |
|---|---|
| PASS | Expected result observed, case-specific postcondition checked, and required evidence is present, correctly typed, and semantically successful |
| FAIL | Wrong result, timeout, missing evidence on a claimed pass, or cleanup failure |
| BLOCKED | Environment / credentials / launcher could not drive the case |
| NOT_RUN | Not attempted this round (policy, budget stop, unobserved live trait, dry-run) |

Reruns keep original FAIL rows in `priorFailures`. Product bugs get a repro and stay open.

## Lanes

| Lane | What it composes |
|---|---|
| `offline` | `pnpm typecheck`, `pnpm test`, `cargo test -p vellum-proxy-runtime --lib`, src-tauri lib + contract tests, `vellum-eval protocol-replay` / desktop + compaction app-replay, `scripts/enhanced-gate.mjs status` |
| `desktop` | Windows UI Automation against the real Vellum window; invoking a control alone does not pass its outcome assertion; tray / OAuth are manual checkpoints |
| `remote` | `scripts/remote-e2e.ps1 -Lane full` on a clean Docker dev host, including both manifest architectures, pinned Codex, authenticated `/readyz`, native Agent / App Server state, and cleanup |
| `live` | Real third-party sessions, subagents, Guardian, remote resume. Official live is `NOT_RUN` this round |

Renderer jsdom tests are regression only. They never count as live Enhanced / Remote pass.

## Live budgets

- OpenCode MiMo 2.5: 200K input+output for the whole round
- Grok Build Grok 4.6: 100K input+output
- Qwen: unlimited via existing Qwen routing
- 10 minutes per case; 90 minutes live-stage wall clock
- Usage includes 子代理、review、compaction、retries
- Missing usage is estimated conservatively; over-budget stops that model as `NOT_RUN` / `budget-stop`

Set `VELLUM_QA_LIVE=1` plus provider credentials to attempt live. The existing `vellum-eval live` command proves candidate provider routing only; it cannot pass Enhanced agent-loop, Remote session, subagent, or Guardian cases. Those cases require their own driver artifacts and otherwise remain `NOT_RUN` / `case-driver-unavailable`.

## Evidence

Each matrix case lists required files (screenshot, sanitized events, session id, usage, attestation). A `PASS` with empty, missing, wrong-type, or semantically failed evidence is rewritten to `FAIL`. Logs are redacted (tokens, bearer, long hex). A screenshot proves rendered state, not that the action's backend postcondition succeeded.

## Tab sync

Adding a `ScreenId` in `src/screens/registry.ts` requires new matrix cases in the same change. `tests/qa-matrix.test.ts` fails otherwise.

## Runner self-checks

```text
pnpm qa -- --lane banana          # exit 2
pnpm qa -- --inject timeout
pnpm qa -- --inject budget-stop
pnpm qa -- --inject missing-credentials
pnpm qa -- --inject missing-evidence
pnpm qa -- --inject cleanup-failure
```

CI runs only the free `offline` lane. No new cron or paid desktop/model job.

## macOS

Out of this round's pass claim. Do not treat a Windows report as macOS acceptance.
