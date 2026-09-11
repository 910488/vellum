# Enhanced harness improvement and qualification plan

This document defines the implementation, test environments, evidence, and
promotion gates for the three Enhanced Codex ports used by third-party models.
It is an operational acceptance document. The protocol rules in
`protocol-source-of-truth.md` remain authoritative.

## Decision from Desktop rollout evidence

The content-free analyzer in `evals/tools/analyze_enhanced_rollouts.py` was run
against local Codex rollout metadata from 2026-09-01 onward. It emits counts,
hashes, byte sizes, and source line numbers only; prompts, tool arguments, tool
outputs, and final answers are never copied to the report.
The captured report is kept with the run artifacts, outside the repository.

The current sample contains:

| Model | Threads | Turns | Tool calls | Text outputs >8 KiB | Text outputs >32 KiB | Compactions | Same call-id replay | Pending-plan stop |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Qwen `vlm-beb60d2887-qwen` | 25 | 69 | 707 | 11 | 4 | 8 | 0 | 0 |
| Omen `vlm-c1f84e8c6d-omen-alpha` | 6 | 24 | 332 | 35 | 9 | 2 | 0 | 0 |

This sample supports enabling the DeepSeek-derived context pressure and bounded
overflow recovery port for controlled A/B testing. It does not show an active
duplicate call-id or incomplete native-plan problem. The Qwen-derived
tool-call ledger and bounded continuation therefore remain defensive candidates
whose value must be established by fault injection and live A/B results.

The single identical-call streak in the Qwen sample was repeated
`write_stdin` polling with distinct call ids. It is not evidence of a provider
tool replay and must not be counted as a reliability improvement opportunity.

## Selected modules and wiring

### Qwen tool reliability

The port sits at the native tool-dispatch seam. It fingerprints normalized tool
arguments, restores handled call ids from durable history on resume, suppresses
only a replay with the same provider call id and the same fingerprint, fails
closed when the same id carries different arguments, and suppresses a late real
result after a synthetic duplicate result. Unique parallel calls are passed
through unchanged.

The feature is controlled by `qwenToolReliability`. E0 must defer to upstream
without mutating its ledger or telemetry. Promotion evidence comes from
deterministic duplicate, collision, resume, parallel-call, and late-result
fixtures; normal Desktop logs alone do not exercise these faults.

### DeepSeek context recovery

The port runs before Codex native compaction and on a provider context-overflow
error. It prunes only the model-visible tool-result surface, preserves
call/output pairing, keeps UTF-8 boundaries, uses host token estimates when
available, and permits at most one overflow retry after measurable surface
progress. Otherwise the original provider error is preserved.

The feature is controlled by `deepseekContextRecovery`. This is the only port
with a directly observed opportunity in the current Qwen/Omen sample: 59 text
tool results exceeded 8 KiB and 13 exceeded 32 KiB. Images are classified
separately so image payloads cannot inflate this signal.

### Qwen bounded continuation

The port runs at the native turn-stop seam and may continue at most twice. A
continuation is armed only when the history contains a successful native
`update_plan` tool output paired to the matching call id and at least one plan
step remains incomplete. Model-supplied plan arguments without successful
PlanHandler output are untrusted and cannot trigger another turn. User input,
cancel, idle, stream failure, and completed work release or reset the budget.

The feature is controlled by `qwenBoundedContinuation`. Because the current
sample contains no pending-plan natural stops, this port is not promoted on the
strength of the observational log analysis.

## Configuration and runtime evidence

The Desktop bridge owns the profile selection. Before spawning either child it
removes `VELLUM_ENHANCED_FEATURE_PROFILE` and
`VELLUM_ENHANCED_ABLATION_PROFILE` from the inherited environment. It restores
both variables only for the verified Enhanced child. Official Codex therefore
cannot inherit Enhanced behavior.

The Enhanced session loader resolves one effective feature set in this order:

1. `VELLUM_ENHANCED_ABLATION_PROFILE` (`E0` through `E5`);
2. the JSON object in `VELLUM_ENHANCED_FEATURE_PROFILE`;
3. `$CODEX_HOME/enhanced-runtime.json`;
4. all features off when Enhanced Codex is launched without a Vellum runtime
   manifest.

The Vellum Desktop release manifest enables E1, E2, and E3 for third-party
routes. E0 through E5 remain explicit eval overrides, and OpenAI Official stays
on the native Codex execution plane where none of these hooks are present.

Identity reporting uses the same resolver as session hooks. Each created
session emits `enhanced.session.features_applied` with the effective profile.
The bridge absorbs that event, records it in the qualification journal, and
increments `sessionFeaturesApplied` in its attestation. A qualification report
with a correct process identity but a zero session counter proves launch
configuration only; it does not prove that a task consumed the hooks.

## Test environments

No single executor is sufficient. Each environment has one explicit role.

| Environment | Role | Network | Authority |
| --- | --- | --- | --- |
| Host Rust/Python replay | Unit, seam, parser, telemetry, and content-free log analysis | No provider traffic | Code-contract gate |
| Windows Sandbox through `vellum-eval` | Real Enhanced Codex agent on isolated Windows with mapped workspace and per-case `CODEX_HOME` | Only the ephemeral Vellum gateway | Authoritative model A/B lane |
| Installed Codex Desktop plus Vellum bridge | Real `app-server --stdio`, Desktop adoption, child routing, attestation, resume, and runtime event evidence | Production Vellum route | Authoritative integration lane |
| Docker hidden grader | Run acceptance commands after the agent exits; synthetic protocol/replay checks | Disabled for grader; internal gateway network for synthetic agent cases | Correctness oracle, not Enhanced runtime authority |

Docker must not run an E0-E5 Enhanced comparison. The evaluator rejects that
combination because its image-bundled Codex is not the pinned Windows Enhanced
artifact. Windows Sandbox uses `codex exec`, while the installed gate exercises
the production Desktop bridge and `app-server` path. Promotion requires both
lanes; a Sandbox result alone cannot prove Desktop routing, and an installed
smoke turn alone cannot measure task correctness.

## Evaluation design

The primary metric is hidden-grader task pass rate. Secondary metrics are
qualified pass rate, stable task/model regressions, duplicate executions,
provider turns, cumulative input and output tokens, compaction count, overflow
retry count, automatic continuation count, wall time, and P95 duration.

Every live case must record the exact catalog model id, runtime digest,
Enhanced commit, requested ablation profile, effective feature flags, executor,
and `legacy_harness_mutation_count`. A case is void when requested and observed
profiles differ, the runtime digest is missing, the session-applied event is
absent, a Vellum legacy mutation occurs, or the failure origin is evaluator
infrastructure. Provider quota and transient service failures are excluded only
from the qualified denominator and remain visible in raw results.

Promotion gates:

- no correctness regression larger than 5 percentage points;
- no more than two previously stable task/model pairs regress;
- at least one target model improves hidden-grader pass rate or removes a
  deterministic reliability failure under the relevant fault fixture;
- zero duplicate side effects and zero mismatched-call-id execution;
- no unbounded overflow retry or automatic continuation;
- token or P95 wall-time growth above 30 percent is a promotion warning that
  requires an explicit review;
- `legacy_harness_mutation_count == 0` for every Enhanced case;
- installed Desktop evidence has `sessionFeaturesApplied > 0` after an
  Enhanced-bound task.

## Phased campaign and budget

Use only Qwen `vlm-beb60d2887-qwen` and Omen
`vlm-c1f84e8c6d-omen-alpha` for the initial paid campaign.

Stage 0 is quota-free. Run the portable crate, fork seam tests, bridge tests,
fixture evaluator, protocol replay, analyzer tests, and Sandbox preflight.

Stage 1 is a 48-case screening cap: six `enhanced-mvp` tasks, two models, E0
and E5, and two repeats. The hard cap is 3 million total tokens. Compare paired
task/model/seed results. Stop if the observed profile is wrong or any safety
gate fails.

Stage 2 isolates the winning feature with E1, E2, and E3 on only the task
categories that can exercise it. Do not spend this phase on a module whose
corresponding telemetry never fires.

Stage 3 runs `harness-stability-12` with three repeats for E0 and the selected
candidate. The entire campaign, including Stage 1, is capped at 192 cases and
12 million tokens. Paid expansion occurs only after Stage 1 passes its safety
and correctness gates.

## Commands

Run from the repository root unless noted otherwise.

```powershell
# Content-free observational evidence
python evals/tools/analyze_enhanced_rollouts.py `
  --codex-home $env:CODEX_HOME `
  --since 2026-09-01T00:00:00+08:00 `
  --model-prefix vlm-beb60d2887-qwen `
  --model-prefix vlm-c1f84e8c6d-omen-alpha `
  --output target/enhanced-log-analysis/report.json
python -m pytest -q evals/tools/test_analyze_enhanced_rollouts.py

# Offline and bridge contracts
cargo test -p vellum-enhanced-codex --lib
cargo test -p vellum --lib enhanced_runtime
cargo run -p vellum-eval --bin vellum-eval -- enhanced-mvp-fixtures
node scripts/enhanced-gate.mjs bridge

# Windows Sandbox readiness
cargo run -p vellum-eval --bin vellum-eval -- sandbox-preflight

# Stage 1, one command per profile. Supply the two exact model ids to --models.
cargo run -p vellum-eval --bin vellum-eval -- matrix `
  --suite evals/manifests/enhanced-mvp-e0.json `
  --models vlm-beb60d2887-qwen,vlm-c1f84e8c6d-omen-alpha `
  --ablation-profile E0 --compaction-engine codex-local `
  --executor windows-sandbox --repeat 2 `
  --max-tasks 24 --max-total-tokens 1500000
cargo run -p vellum-eval --bin vellum-eval -- matrix `
  --suite evals/manifests/enhanced-mvp-e5.json `
  --models vlm-beb60d2887-qwen,vlm-c1f84e8c6d-omen-alpha `
  --ablation-profile E5 --compaction-engine codex-local `
  --executor windows-sandbox --repeat 2 `
  --max-tasks 24 --max-total-tokens 1500000

# Installed Desktop integration; run after staging the candidate sidecars.
node scripts/enhanced-gate.mjs installed
node scripts/enhanced-gate.mjs status
```

The commands above define the executable gate. The shipped Vellum manifest now
enables the qualified E1, E2, and E3 set for third-party Desktop traffic. Every
release must still pass the Windows Sandbox A/B and installed Desktop gates;
the eval profiles retain E0 so later builds can detect regressions against the
unmodified upstream behavior.
