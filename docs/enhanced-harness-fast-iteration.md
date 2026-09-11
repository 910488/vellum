# Enhanced Harness fast iteration gate

This gate answers one narrow question per module: when the real provider edge
encounters the condition the port was designed for, does the port improve the
outcome without regressing ordinary work?

## Environment and evidence

- **Agent:** pinned Enhanced Codex binary in a fresh Windows Sandbox for every
  invocation. Only the ephemeral Vellum gateway is reachable.
- **Provider:** the same Qwen and Omen routes in both arms. E0 and one candidate
  profile receive the same three tasks and deterministic round seed.
- **Grader:** the existing immutable evaluator Docker lane with networking
  disabled. Hidden files are never mounted into the agent VM.
- **Attribution:** each case records the runtime-reported commit, digest,
  feature profile, port flags, and `enhanced.*` event counts. Requested config
  and artifact files do not count as observed evidence.

The candidate binary must be rebuilt after changes in the Enhanced Codex fork,
then its digest must replace the Windows entry in `enhanced-runtime.lock.json`.
The evaluator refuses a binary whose digest does not match the lock.

## Quick round

A round contains three roles across two models and two arms, for 12 real
executions:

| Role | Purpose |
| --- | --- |
| Target | deterministically stimulates exactly one module |
| Short control | catches simple coding and tool-use regressions |
| Multi-file control | catches integration and exploration regressions |

E1 replays a completed function call with the same call id, name, and
arguments. The grader requires one side effect and the candidate must emit
`enhanced.tool.duplicate_suppressed`.

E2 applies an eval-only persistent `context_length_exceeded` rule starting at
request 2. Every request whose gateway total estimate (instructions, tool
schema, and input) remains above 10,000 tokens is refused. The candidate must
prune and retry once, retain the projection for at least three later provider
requests, complete the implementation, and emit both
`enhanced.context.overflow_retry` and
`enhanced.context.projection_applied`. The trace records the rule, threshold,
actual estimate, and request index for every refusal.

E3 returns one valid empty normal stop after the agent creates a native pending
plan. The candidate must run one bounded continuation, finish the plan, and
emit `enhanced.continuation.allowed`.

Run at most three rounds per module. Odd rounds run E0 first; even rounds run
the candidate first. Do not alter tasks, prompts, thresholds, or graders during
a round. Any such change starts a new campaign version.

## Decision rules

A quick round uses scoring schema `enhanced-paired-v2` and passes only when:

- every case reports the expected runtime identity and profile;
- both target cases pass completion, hidden correctness, protocol, resource,
  runtime-attribution, and mechanism gates;
- a control that fully qualified in E0 also fully qualifies in the candidate
  arm;
- each model's successful control pairs have median candidate token cost no
  more than 10% above E0 and median duration no more than 20% above E0;
- there is at least one E0-fail to candidate-pass conversion, or successful
  target pairs improve median token cost by at least 15%.

Every HTTP error stays in `httpFaults`. A declared injection is marked
recovered only when its kind and request position (plus threshold for a
persistent rule) match the manifest and a later request reaches a successful
terminal. Reports are versioned and never replace the original run evidence.

Each case is capped at 180 seconds, 200k input tokens, and 10k output tokens.
Each arm is capped at 500k total tokens. Infrastructure failures may be retried
once with the same seed and pair identity; they never count as module wins.

## Holdout and promotion

After the implementation is frozen, select three tasks from a six-task holdout
pool that was not inspected during tuning. Run three repeats with the same two
models and two arms, for 36 executions. Promotion requires target correctness
and mechanism evidence on both models, no correctness or protocol regression,
and the same efficiency limits. For E2, either median target tokens improve by
at least 15% or the candidate converts an E0 overflow failure into a correct
bounded completion.

Passing this gate qualifies the module for an installed Desktop canary. It does
not enable the module by default; the installed bridge and Desktop attestation
gate remain required.

The checked-in `enhanced-holdout-pool.json` contains the three unseen target
variants, two shared controls, and one reserve. The driver selects the target
for the requested module plus the two controls and raises the repeat to three:

```powershell
python evals/tools/enhanced_quick_campaign.py --module E2 --round 1 `
  --phase holdout --binary C:\path\to\verified\enhanced-codex.exe
```
