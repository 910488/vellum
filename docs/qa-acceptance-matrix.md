# Vellum 驗收矩陣（v1）

Machine-readable source: [`qa/matrix/v1.mjs`](../qa/matrix/v1.mjs).

Every case records 前置條件、操作、預期結果、查證來源、自動化方式、證據要求. Verdicts are only `PASS` / `FAIL` / `BLOCKED` / `NOT_RUN`.

**macOS is not part of this round's pass claim.** Official live traffic is `NOT_RUN` (offline contract + routing isolation only).

## Tab sync rule

Any new `ScreenId` in `src/screens/registry.ts` must add cases covering every new button, setting, and important display field in the same change. `tests/qa-matrix.test.ts` fails if a screen has no cases.

## Domains

| Domain | Surfaces | Lane | Pass bar |
|---|---|---|---|
| 全部 Tab | Today、Models、Context、Enhanced、Remote、Log、Settings, plus onboarding、對話框、托盤 | desktop (some Remote actions remote) | Real Windows UI; cancel / error / save / reload covered per control |
| 資訊正確性 | model, connection, Runtime, session, usage, log | desktop | UI vs backend / process / real events — not screen vs the same mock; provider-reported vs estimated usage stay distinct |
| Enhanced Core | read file, edit small function, run tests, continue, cancel | live | Formal desktop + real third-party session; attestation of pinned core |
| Enhanced 特性 | tool-repeat, context recovery, limited continuation, cancel-priority | offline inject **and** live, reported separately | Unobserved live traits are `NOT_RUN`, never live-PASS |
| Remote Manager | clean Docker deploy, health, config, reapply no-op, reconnect, stop/cleanup, session+resume | remote + live | Native Agent / App Server; not legacy Broker tests |
| 子代理 | spawn two children, message, wait, resume, close, complete, parent cancel, usage | live | Identity / routing / return / usage neither dropped nor double-counted |
| 自動審查／Guardian | known defect, clean diff, identity, 429 / timeout / malformed / verify-fail | live + offline inject | Faults must not become a successful empty review |
| 離線契約 | Official passthrough, cross-provider isolation, streaming, continuation/compaction, error classification | offline | Existing cargo / protocol-replay / app-replay |

Tasks are simple and objectively decidable. Hard coding benchmarks are not the function-verification bar.

## Automation keys

| `automation` | Driver |
|---|---|
| `shell` | Existing pnpm / cargo / vellum-eval / enhanced-gate |
| `desktop-ui` | Windows UI Automation (`scripts/qa/desktop/*.ps1`) |
| `desktop-ui-manual-checkpoint` | OS / OAuth / tray; screenshot + operator log required |
| `docker-dev-host` | `scripts/dev-host.ps1` + `scripts/remote-e2e.ps1` |
| `live-session` | Real model session under the token / time caps |
| `offline-inject` | Fault or trait coverage inside `vellum-proxy-runtime` tests |
| `none` | Policy skip (Official live) |

## First-round reports

JSON and HTML are generated under `qa/reports/generated/` and stay local.
Commit only reviewed Markdown summaries with operator paths and identities removed.
