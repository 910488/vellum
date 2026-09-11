# Vellum Harness Evaluator

`vellum-eval` runs the real Codex CLI and Vellum adapters against deterministic
public coding tasks. It is a developer tool and is not exposed in the desktop
UI.

## Codex Local Compact 0.150 switchover A/B

`compaction-engine-ab` is the suite for the switchover gate. It has exactly two
arms, selected with `--compaction-engine`:

- `codex-local-native-oracle` — Codex core 0.150's own native local compact,
  the oracle the port is measured against.
- `codex-local-v0-150` — Vellum's embedded production engine (the default).

The retired `legacy`, `canonical`, `canonical-v1`, and `canonical-v2` values are
refused rather than silently accepted, so a stale command line cannot measure an
engine that no longer exists. `--grok-compaction` accepts only `desktop`: Grok
native compaction is retired, and an explicit Grok compactor would change the
engine under test.

The merge gate is 3/3 task passes per cell across four tasks (single-resume,
goal-revision, tool-continuity, double-compaction) and three providers
(Grok 4.6, DeepSeek V4 Flash, Qwen 806), with the embedded arm passing no fewer
tasks than the native oracle. Requested and observed engine must match on every
run; the `LocalCompactionAttempt` diagnostic reports the engine that actually
ran, written from the resolved engine rather than from the request.

Hidden text graders decode UTF-8 signatures emitted by Windows PowerShell
while continuing to compare the exact semantic content.

## Ten-minute compaction gate

GLM-5.2, and the GPT-5.4 Mini control. It defaults to a 600 second global
budget, round-robin scheduling, and protocol fail-fast:

```powershell
cargo run -p vellum-eval --bin vellum-eval -- matrix `
  --compaction-engine codex-local-v0-150
```

Use `--compaction-engine codex-local-native-oracle` for the eval-only baseline. Reports keep
acceptance, transport, tools, compaction trigger, local materialization,
resume, continuity, and resource budgets as independent gates.

### Failure Origin & Qualified Pass Rate

The harness evaluates terminal failure origins to distinguish engine capability from
transient environment or model errors:
- `vellum_compaction`: Runtime compaction or canonical quality gate failures.
- `provider`: Upstream provider timeouts, HTTP 429/503 errors, or quota exhaustion. Full-case retry is permitted only for retriable provider errors.
- `evaluator_infrastructure`: Zero-request timeouts, harness harness VM faults, or verification harness timeouts.
- `model`: Tool protocol / transport passed, but acceptance tests failed.
- `unknown`: Indeterminate or missing structured evidence.

Reports calculate both `rawPassRate` and `engineQualifiedPassRate` (which excludes
`provider` and `evaluator_infrastructure` failures from the denominator).
Each phase also records a `PhaseContextObservation` using the exact context window
and auto-compact threshold published in that run's Codex catalog. Its trigger
decision is one of `below_threshold`, `compaction_observed`,
`above_threshold_without_request`, or `unknown_missing_tokens`; the evaluator
does not reconstruct or guess omitted catalog values. Case diagnostics include
the source hash, candidate generation, model-visible/durable token breakdown,
quality outcome, fallback reason, and structured failure origin/class.

## Safety model

- Provider credentials remain in the host Vellum data directory.
- Every task gets a new workspace, `CODEX_HOME`, and one-time bearer token.
- On Windows, the authoritative executor starts a fresh Windows Sandbox VM.
  It maps the task workspace, per-case `CODEX_HOME`, and per-invocation control
  directory as separate writable folders, plus the pinned Codex runtime
  read-only. It never maps their parent case directory, so `hidden/` and the
  post-agent verification workspace are absent from the model VM. Provider
  credentials never enter the VM.
- Windows Sandbox ownership is isolated per evaluator process/run. Cross-process
  launches are serialized, and stale-session cleanup skips marker directories
  whose owner PID is still alive; one eval must never stop another eval's VM.
- The Windows Sandbox firewall permits only the ephemeral Vellum gateway. The
  Docker executor remains available as a synthetic cross-platform lane and
  for the network-disabled hidden grader.
- Docker agent containers join an internal network with no general egress.
- A single-purpose `socat` sidecar can only reach the ephemeral Vellum gateway.
- The gateway permits only the registered token, model IDs, and Responses API
  paths.
- Hidden graders are mounted only into a second network-disabled container
  after the agent exits. A Windows Sandbox case must never map `case_root`
  wholesale; adding a model-visible mount requires an isolation regression
  test proving that neither `hidden/` nor verification artifacts are reachable.
- Traces are redacted before being persisted.
- OAuth fields, cookies, authorization headers, API-key fields, and common
  provider-key prefixes are removed from both JSONL and plain-text traces.

The agent uses `--dangerously-bypass-approvals-and-sandbox` only inside the
outer Windows Sandbox or Docker boundary. Never copy that flag to a host-side
Codex invocation.

## Datasets

The checked-in suite manifests select records from:

- OpenAI HumanEval (`HumanEval.jsonl.gz`)
- Google Research sanitized MBPP (`sanitized-mbpp.json`)

Dataset files are not duplicated under `vellum/`. By default the CLI discovers
them in `../public_datasets`; use `--dataset-root` to select another directory.
`prepare` downloads missing HumanEval/MBPP files and rejects any content that
does not match the evaluator's pinned SHA-256. `prepare.json` and each run
record include those source hashes.
The manifest also supports fixed Git repository commits and local reproducible
fixtures; network access for those sources is confined to `prepare`.

`smoke` contains six tasks. `core-30` contains:

- 10 short HumanEval repairs
- 8 multi-file MBPP adaptations
- 4 deterministic transport/retry scenarios
- 4 forced-context compaction scenarios
- 4 model-switch resume scenarios

Additional reliability suites:

- `investigation-replay` is the quota-free Investigation Ledger V3 harness.
  Run `cargo run -p vellum-eval --bin vellum-eval -- investigation-replay [--file PATH] [--mode shadow|recover]`
  to replay a captured Codex rollout JSONL trace, portable `items` trace, or
  `{userPrompt, events}` fixture. Raw rollout replay preserves each non-zero
  `last_token_usage` record, including cached input, and skips zero-usage local
  compaction/control turns so request-boundary token attribution remains exact.
  The command prints operation, link, frontier-transition, and recovery diagnostics.
  Reports also include Task Stall Guard V4 counters (`toolResultsSinceProgress`,
  `maxToolResultsSinceProgress`, `firstStallCandidateSourceIndex`,
  `stallRecoveryCount`, `stallCandidate`, `watchdogCandidate`) and proven recovery
  injections with request index, intervention type, and message hash. A Level 2
  candidate receives one tool-disabled finalization turn. Only if the model
  attempts another provider turn does the gateway emit `task_stall_terminal`,
  refuse that turn, and record
  `failureOrigin=model` with
  `failureClass=model_no_progress_after_recovery`.
  The focused unit corpus remains available through
  `cargo test -p vellum-proxy-runtime --lib investigation_replay` and replays
  portable tool events through the shared reducer (cross-tool aggregation,
  envelope-empty output, self-transcript, channel coverage, false-merge guards).
  It does not call a model and does not depend on `rollout.jsonl` literals.
- `enhanced-mvp` is the Enhanced Codex ablation corpus. In-process fixtures
  (`vellum-eval enhanced-mvp-fixtures` and `cargo test -p vellum-enhanced-codex`)
  cover tool-call dedup, context prune/overflow retry, and bounded
  continuation without provider quota. Live manifests under
  `evals/manifests/enhanced-mvp-e*.json` must be run with an explicit
  `--ablation-profile E0..E5` and `--compaction-engine codex-local`. Model
  names never select a profile. Enhanced MVP runs must keep
  `legacy_harness_mutation_count == 0`.
- `enhanced-quick-e1`, `enhanced-quick-e2`, and `enhanced-quick-e3` are the
  mechanism-attributed tuning rounds. Each manifest has one target scenario,
  one short control, and one multi-file control. Run the paired campaign driver
  from the repository root; one round is exactly 12 real executions (three
  tasks, two models, E0 plus one candidate profile) and rounds alternate arm
  order:

  ```powershell
  python evals/tools/enhanced_quick_campaign.py --module E1 --round 1 `
    --binary C:\path\to\verified\enhanced-codex.exe
  ```

  The authoritative agent runs inside a fresh Windows Sandbox. The hidden
  grader remains a second `--network none` Docker container. The Enhanced
  process writes the same redacted identity and mechanism notifications used by
  App Server to an evaluator-only JSONL path; a requested profile is never
  accepted as observed evidence. Every case must match the pinned commit,
  runtime digest, and profile. The target additionally must emit its expected
  event (`duplicate_suppressed`, `overflow_retry`, or `continuation.allowed`).
  Missing attribution, missing mechanism evidence, incomplete termination,
  protocol or resource disqualification, or a fully-qualified control
  regression fails the round. Token (+10%) and duration (+20%) control limits
  are calculated separately for each model and only from pairs where both
  arms succeeded. Use at most three rounds per module. The script fixes the seed from module and round, records the
  manifest and binary hashes, uses round-robin scheduling, caps each arm at
  500k tokens, and writes a paired JSON report under the candidate run.
  After freezing the implementation, add `--phase holdout`; this selects three
  unseen tasks from the six-task holdout pool, runs three repeats, and produces
  the 36-execution promotion sample.
  E2 uses the optional fault field
  `whileTotalEstimatedTokensAbove: 10000`. Starting at `atRequest`, the
  gateway repeatedly returns `context_length_exceeded` while its total input
  estimate (instructions + tool schema + messages) is above the threshold.
  Ordinary one-shot fault injection remains unchanged when this field is
  absent. Gateway traces and task results preserve every HTTP error and label
  expected, recovered, unrecovered, and unexpected observations separately.
- `evals/harness-lab` is a non-product Node sidecar for testing pinned upstream
  harness modules without rebuilding Rust. It replays recorded Codex rollouts
  through Qwen's original loop detector and can proxy live Responses traffic in
  observe, stop, or one-shot recovery-prompt modes. Its results are POC evidence
  only; see that directory's README for the qualification boundary.
- `protocol-replay` is an offline adapter/replay corpus. It validates Responses,
  Chat, reasoning-realm sanitization, and error envelopes without consuming
  Provider quota.
- `desktop-protocol-replay` replays sanitized Codex Desktop request shapes,
  including zstd bodies, `additional_tools`, duplicate tool definitions,
  provider-realm reasoning, and official item-ID normalization.
- `harness-stability-12` gives GLM-5.2p, GLM-5.2, and Grok 4.5 the same
  twelve tasks. Use one repeat as the merge gate and three for manual/nightly
  stability measurements.
- `switch-matrix-6` currently schedules the five enabled profile models:
  five same-model continuations, 20 directed transitions, and 20 directed
  round trips. Qwen3-Coder-Next is temporarily excluded from CLI evaluation.
- `continuity-12` produces exactly 12 cases for a three-model matrix: three
  same-model continuations, six directed A→B transitions, and three cyclic
  A→B→A round trips.
- `natural-compact` uses the configured model window and requires a real
  compaction plus a successful resumed turn.
- `long-task-stability-2` isolates two regressions observed in real Desktop
  sessions: stateless snapshots must reuse the durable canonical checkpoint
  instead of causing a compaction storm, and Chat/Ollama streams must continue
  through tools or fail explicitly when the provider reports an output limit.
  compaction, durable resume, repeated compaction, tool continuity, portable
  provider handoff, round trips, and fail-closed malformed summaries.
  contract gate for exact official output replay, ciphertext restoration,
  retained-item de-duplication, server-side suffixes, and portable handoff.
  Engine V2 spec section 74. It covers 50 tool exchanges before recall,
  rejected hypotheses, changed-file and latest-test fidelity, loop guards,
  50k tool output, three sequential compactions, provider switching, and
  malformed semantic extraction. `prepare` validates its fixtures without
  provider traffic; a matrix run is still required for behavioral promotion.
- The manifest schema supports pinned SWE-bench bundles. Each bundle must
  contain `workspace/` and `hidden/`, is verified by SHA-256, and **must**
  use exactly one grader source:
  - OCI: immutable `graderImage` (`registry/name@sha256:<manifest>`).
    `prepare` pulls a missing image and verifies both the manifest digest
    and the Docker config digest.
  - GitHub Release archive: complete `graderImageArchive` plus a local
    `graderImage` tag named `:cfg-<12 hex of image_digest>`. `prepare`
    downloads the gzip archive (private-release 404 fallback),
    decompresses with an uncompressed-size cap, and `docker load --input`.
  `image_digest` is always the Docker config digest. `:latest` and
  combining both modes fail suite load. Gold patches and hidden tests
  remain outside the agent workspace.

The 0.2.3 release SWE gate is the single pinned instance
`psf__requests-1142`. Its image archive, bundle, artifact manifest, grader
lock, and content manifest live on the immutable GitHub Release tag
`swebench-grader-psf-requests-1142-a3ba3a231e3c`. Do not overwrite that
tag. Active suites `swe-terminal-gate` and `deepseek-luna-swe-roundtrip`
use `graderImageArchive` plus the local tag
`sweb.eval.x86_64.psf__requests-1142:cfg-64ac3483a165`. They do not
reference GHCR.

The single-task gate forces a 24k context window (16k auto-compaction
threshold) and requires at least one observed compaction. This keeps a
multi-tool SWE run inside its 400k cumulative input budget instead of letting
a 500k provider context postpone compaction until after the evaluator budget
has already been exceeded. The gateway also applies the same 30-turn hard cap
to every SWE provider and across a transient full-case retry; the 31st
non-compaction request is rejected before it can reach the upstream. A
`compaction_trigger` sent through `/v1/responses` remains auxiliary and does
not consume a model turn. Before dispatch the gateway atomically reserves the
projected input surface, so parallel requests cannot share the same remaining
budget. Terminal SSE usage replaces that reservation; JSON/disconnected
responses without parseable usage are conservatively charged at the
projection, so a missing harness metric cannot turn the 400k gate into a false
zero-token PASS. The
model-visible harness states the same budget and asks the agent to prefer a
focused patch and targeted tests instead of restarting broad exploration
after compaction. Luna retains its additional atomic campaign ledger.

Pinned 1142 identity:

- Image archive SHA-256: `a3ba3a231e3c7a48f8410b5d7942911763bfa2ad2295b7cb6d79032a099f7055`
- Config digest: `sha256:64ac3483a165db2706f67b59e064095d0646e4db397d1573443b307c45763d1b`
- Bundle SHA-256: `073c9c6b4957da5aaad21f24d1e5666da00a1e707eeb6e848536a0d9f635d929`

The remaining 11 Verified-12 instances are post-release qualification.
Do not generate or check in `evals/suites/swe-verified-12.json` until
those archives exist. The Verified-12 builder remains a development
tool. `evals/fixtures/invalid-manifest/swe-terminal-gate-expanded.json`
is kept only as a fail-closed fixture for mutable local tags.

Networked seed stages resolve apt/conda/pip once; every publishable
stage is then rebuilt twice from a normalized rootfs and must have an
identical image ID:

```powershell
python evals/tools/build_swe_grader.py `
  --selection evals/swebench/verified-12-selection.json `
  --dataset ../public_datasets/swe-bench-verified.parquet `
  --instance psf__requests-1142 `
  --artifacts ../public_datasets/swebench/_grader `
  --create-lock `
  --tag-instance
```

`--replay-lock <grader-lock.json>` replays the canonical rootfs archives
without network access and verifies their hashes and image IDs. Resume and
single-build options are developer diagnostics; they must not be used to
publish or pin an artifact.

Verified-12 is built one instance at a time (no `docker prune`). After each
image is double-built and exported, `verify_swe_grader.py` checks the official
`FAIL_TO_PASS` tests: unpatched baseline must fail and the gold patch must
pass, with `--network none` and the hidden eval mounted outside the workspace.

```powershell
python evals/tools/publish_verified_12.py `
  --dataset ../public_datasets/swe-bench-verified.parquet `
  --selection evals/swebench/verified-12-selection.json `
  --artifacts ../public_datasets/swebench/_grader `
  --bundle-root ../public_datasets/swebench `
  --suite-output evals/suites/swe-verified-12.json `
  --bundle-base-url https://github.com/910488/vellum/releases/download/swebench-verified12-pending
```

`--publish` creates the immutable GitHub Release tag
`swebench-verified12-a45b1fe4-<builder-lock-hash>` and rewrites the suite
URLs only after all 12 instances have verified.

Generate the fixed suite and deterministic task bundles from the verified
local images:

```powershell
python -m pip install swebench pandas pyarrow docker datasets
python evals/tools/prepare_swebench.py `
  --selection evals/swebench/verified-12-selection.json `
  --dataset ../public_datasets/swe-bench-verified.parquet `
  --bundle-root ../public_datasets/swebench `
  --suite-output evals/suites/swe-verified-12.json `
  --bundle-base-url https://your-artifact-host.example/swe-verified-12 `
  --reuse-existing-images
```

The generated suite pins the parquet hash, repository commit, bundle hash,
Docker config digest, and either a complete `graderImageArchive` or a
digest-pinned OCI `graderImage`. Each bundle is produced in two independent
temporary directories with sorted entries, normalized metadata and a content
manifest; mismatched hashes stop the build. `prepare` refuses a missing or
mismatched artifact rather than silently changing the benchmark. This
builder path is for post-release qualification of the remaining 11
Verified-12 instances; it is not a 0.2.3 release gate.

A focused qualification task may provide
`evals/graders/<task-id>.sh`. `prepare` installs that script over the bundle's
`hidden/eval.sh` and includes its SHA-256 in the prepared dataset metadata.
This is used by the Ollama SWE terminal gate to run the pinned regression with
fail-fast exit semantics instead of treating a later cleanup command as a
successful grader result.

## Commands

From `src-tauri`:

```powershell
cargo run -p vellum-eval --bin vellum-eval -- doctor
cargo run -p vellum-eval --bin vellum-eval -- doctor --profile desktop-six
cargo run -p vellum-eval --bin vellum-eval -- sandbox-preflight
cargo run -p vellum-eval --bin vellum-eval -- parity --profile issue-7-required --executor windows-sandbox
cargo run -p vellum-eval --bin vellum-eval -- app-server-parity --profile issue-7-required
cargo run -p vellum-eval --bin vellum-eval -- qualify-issue-7 --grok-run <run-id> --glm-run <run-id> --parity <parity.json> --app-parity <app-server-report.json>
cargo run -p vellum-eval --bin vellum-eval -- models
cargo run -p vellum-eval --bin vellum-eval -- protocol-replay
cargo run -p vellum-eval --bin vellum-eval -- app-replay --suite desktop-protocol-replay
cargo run -p vellum-eval --bin vellum-eval -- live --route all --phase candidate
```

`live` is a candidate-source acceptance driver. It copies production
credentials into an isolated data directory and starts an in-process
router from the current tree. It accepts only `--phase candidate`.
`--phase installed` fails closed: that flag never exercised the
installed Desktop, and Installed Gate D must run against the real
installer instead. Do not label candidate live artifacts as installed.

```powershell
cargo run -p vellum-eval --bin vellum-eval -- prepare --suite smoke
cargo run -p vellum-eval --bin vellum-eval -- run --suite smoke --model <catalog-id> --repeat 1
cargo run -p vellum-eval --bin vellum-eval -- run --suite compact-smoke --model <catalog-id> --repeat 1
cargo run -p vellum-eval --bin vellum-eval -- run --suite long-task-stability-2 --model <catalog-id> --task stateless-canonical-resume --repeat 1
cargo run -p vellum-eval --bin vellum-eval -- run --suite long-task-stability-2 --model <catalog-id> --task ollama-long-stream-tool-continuation --repeat 1
cargo run -p vellum-eval --bin vellum-eval -- matrix --suite core-30 --models <id,id,id> --repeat 3
cargo run -p vellum-eval --bin vellum-eval -- matrix --suite continuity-12 --models <grok>,<glm>,<nvidia> --repeat 3
cargo run -p vellum-eval --bin vellum-eval -- matrix --suite natural-compact --models <grok>,<glm>,<nvidia> --repeat 1 --natural-context
cargo run -p vellum-eval --bin vellum-eval -- matrix --suite harness-stability-12 --profile desktop-six --repeat 1
cargo run -p vellum-eval --bin vellum-eval -- matrix --suite switch-matrix-6 --profile desktop-six --transition directed --repeat 1
cargo run -p vellum-eval --bin vellum-eval -- matrix --suite switch-matrix-6 --profile desktop-six --transition all-round-trips --repeat 3
cargo run -p vellum-eval --bin vellum-eval -- report <run-id> --format html
cargo run -p vellum-eval --bin vellum-eval -- report <run-id> --compare <baseline-run-id>
cargo run -p vellum-eval --bin vellum-eval -- triage <run-id>
```

Useful limits:

```text
--max-tasks N
--max-wall-seconds N
--max-total-tokens N
--resume RUN_ID
--codex-version VERSION
--natural-context
--task ID
--category NAME
--provider NAME
--tag TAG
--seed N
--control-model gpt-5.4-mini
--baseline RUN_ID
--compaction-engine codex-local-v0-150|codex-local-native-oracle
--profile desktop-six
--lane grok|official|luna|nemotron|laguna
--transition directed|cyclic|all-round-trips
--executor windows-sandbox|windows|docker
```

`--resume RUN_ID` is a run-level continuation, not another provider retry
layer. Cases with valid terminal conclusions (pass, model/Vellum/protocol
failure, deterministic injected fault, or a permanent evaluation budget cap)
remain complete. A latest attempt whose structured origin is a retryable
Provider failure or non-deterministic evaluator-infrastructure failure is
scheduled again from the prepared fixture. Its prior result remains in
`results.jsonl`, its case/trace/patch/test artifacts move under
`attempts/<case-hash>/attempt-N`, and reports use only the latest result for a
case. Run token accounting continues to include every recorded attempt.

`windows-sandbox` is the default and authoritative Windows lane. Run
`sandbox-preflight` after preparing a suite to verify the writable mapped
workspace and pinned Codex runtime without contacting any model Provider.
`windows` is retained only as a diagnostic lane because Codex can narrow its
host sandbox policy. `docker` is the portable synthetic lane.

Issue #7 promotion uses two complementary Windows checks. `app-server-parity`
drives the pinned Desktop Codex core through its real `app-server --stdio`
entrypoint and the production Vellum backend, including a native shell call and
continuation. The HumanEval qualification runs `codex exec` inside Windows
Sandbox. Instructions, model-visible tool names, harness profile, terminal
events and call/output history must match. Codex may vary policy-specific JSON
schema details between Desktop `workspace-write` and the externally isolated
`--dangerously-bypass-approvals-and-sandbox` lane; that hash difference is
reported explicitly and is not silently treated as exact equality.

Long-context cases use their forced test window by default. Add
`--natural-context` for the slower, manual/nightly mode that keeps each
provider model's configured real context window. The focused canonical gate
uses a 15k window with a 10k auto-compaction threshold so the live check
actually reaches `/responses/compact` within its bounded budget; broader
suites may use larger fixture-specific windows:
large enough to contain Codex's canonical instructions and tool definitions,
but small enough for the checked-in fixture to trigger a real compaction.

the original ciphertext-recovery prompt and a clarified control as separate
tasks at a 24k forced context window and explicitly publishes a 10k
`forcedAutoCompactTokenLimit`. Keeping these values separate gives the model
the qualified 24k working window while ensuring an efficient successful run
still crosses a real Canonical V2 compaction boundary. The original remains the
promotion signal, while the clarified task helps distinguish investigation
ambiguity from engine state loss. `forcedAutoCompactTokenLimit` is eval-only,
must be positive and smaller than `forcedContextWindow`, and is ignored with
`--natural-context`. Run `--natural-context` separately as a slower manual
control and never combine its result with the forced-window denominator.

Research-sprawl recovery and the hardened parallel-inspection prompt are
independent A/B controls. Product defaults remain `shadow` and the new
hardened parallel guidance off. General `run`/`matrix` evals select both Task
Stall and Task Efficiency behavior explicitly with `--recovery-mode`; the
choice is recorded in `run.json` and is resume-stable:

```powershell
$env:VELLUM_EVAL_PARALLEL_GUIDANCE = 'on'
cargo run -p vellum-eval --bin vellum-eval -- matrix `
  --suite compaction-engine-ab `
  --models <catalog-id,...> `
  --task engine-ab-tool-continuity `
  --recovery-mode recover
```

Omit the flag (or pass `--recovery-mode shadow`) for the observation-only
control. The evaluator applies this as an isolated runtime policy override;
it does not mutate production defaults or process-wide recovery environment.
In `recover` mode, Task Stall state is durable across requests. An ignored
Level 1 warning can therefore escalate after the configured patience to one
tool-disabled finalization turn; a further no-progress request fails closed.
This bounded finalization remains eval-only.

Action Conversion V2 focused variants keep the persistent overlay enabled
whenever research-sprawl mode is `recover`. Isolate L2 escalation and the
stable action-bias prompt with these inherited eval-only switches:

```powershell
# B: persistent L1 overlay only
$env:VELLUM_EVAL_ACTION_RECOVERY_ESCALATION = 'off'
$env:VELLUM_EVAL_ACTION_BIAS_GUIDANCE = 'off'

# C: persistent overlay + L2
$env:VELLUM_EVAL_ACTION_RECOVERY_ESCALATION = 'on'
$env:VELLUM_EVAL_ACTION_BIAS_GUIDANCE = 'off'

# D: full Action Conversion V2
$env:VELLUM_EVAL_ACTION_RECOVERY_ESCALATION = 'on'
$env:VELLUM_EVAL_ACTION_BIAS_GUIDANCE = 'on'
```

Unset either variable after the focused run. Production defaults keep L2 and
action-bias guidance enabled; these switches exist to separate eval attribution.

Use `shadow` and `off` for the baseline. An `off` parallel variant preserves
the route's pre-existing parallel capability contract and removes only the
new hardened batching guidance, so the comparison does not change two prompt
variables at once. Do not default-enable recovery or the added parallel
guidance from one focused result; apply the promotion rules in the
research-sprawl implementation plan.

Case artifacts include request instruction/input/tool token estimates,
compaction source and checkpoint lineage, structured semantic-validation
diagnostics, phase execution/context observations, separate correctness / protocol /
performance qualification, and Task Efficiency usage coverage. A recovery conclusion is valid
only when these fields can show whether the request crossed the scheduler
threshold, whether prior negative memory was carried, and whether the terminal
failure came from Vellum, the provider, evaluator infrastructure, or the
model. Missing structured evidence is reported as unknown rather than inferred
from error-message keywords. Assistant reasoning-leak checks inspect only
assistant message output; tool output, prompts, diagnostics, and compaction
payloads are excluded.

Live runs write only beneath `target/vellum-evals/<run-id>`.

`legacy` is accepted only by the isolated evaluator. The Desktop proxy always
uses the versioned canonical journal. Run the quota-free canonical replay
before either Docker matrix; do not start paid live A/B traffic until the
offline contract and Rust tests pass.

`doctor` sends a minimal, one-token provider probe to each enabled third-party
route. This is intentional and may consume a negligible amount of provider
quota.

Fault manifests can deterministically inject 429, 500, 524, malformed JSON,
empty or non-JSON compaction results, truncated SSE, missing completion events,
duplicate deltas, and fragmented SSE/tool arguments.

Gateway traces contain only structured diagnostics: hashed session identifiers,
request ordinal, Provider/route/model, input item counts, tool schema hash,
reasoning item counts, encrypted byte length and ciphertext hash, compaction markers, SSE
termination, retry/HTTP state, and Provider-specific continuity metadata. Raw
reasoning and encrypted reasoning payloads are never persisted.

OpenAI Official retains provider-owned encrypted reasoning unchanged. Every
third-party route uses readable replay: ciphertext is removed before it can
enter a later request while readable summaries and tool history are retained.

Failure classes distinguish model capability, Provider service/quota,
Vellum adapter/protocol, Codex Harness compatibility, evaluator
infrastructure, and indeterminate failures. `triage` groups failed cases by
these classes.

## Result authority

A task passes only when every hidden acceptance command exits successfully.
The model's final answer is never used as the correctness signal. Public
materializers print `VELLUM_EVAL_PASS` as an additional audit marker, but
generic fixed-repository graders do not depend on that string. Reports
separately preserve agent completion, protocol errors, tool
metrics, acceptance-command counts, token use, patches, gateway timelines,
Codex JSONL, and grader output. Repeated runs are aggregated by task to expose
stability rather than reporting only an average pass rate.

Baseline comparison fails the pass-rate gate below -5 percentage points and
the stable-regression gate when more than two previously stable task/model
pairs regress. Token or P95 duration growth above 30% is reported as a
performance warning rather than a model-correctness failure.

Official-native result attribution records `observedCanonicalEngine` as
`official-native`. Its `canonicalStrategy` may still describe the separately
journaled portable handoff representation; that field does not identify the
engine that executed the Official compaction.

## Compaction engine A/B artifacts

The production arm is `codex-local-v0-150`. The eval-only
`codex-local-native-oracle` arm suppresses Vellum's compact endpoint and sets
`features.remote_compaction_v2=false`, allowing the same Codex Core binary to
compact locally. Primary metric is hidden-grader task pass. Reports record:

- `requestedCompactionEngine` / `observedCompactionEngine` / `compactionEngineMatched`
- `codexCompactionEvents`, `remoteCompactRequests`, `vellumCanonicalJournalCount`
- `preCompactionModelVisibleTokens` / `postCompactionModelVisibleTokens` / `modelVisibleCompressionRatio`

`observedCompactionEngine` is one of `codex_local_v0_150`,
`codex_local_native_oracle`, `remote_v2`, `mixed`, `none`, or `unknown`. `mixed` or a
requested/observed mismatch is a VOID infrastructure failure (`wrong_engine`
/ `mixed_engine`), not a model failure. Stage 0 is a 2×3×1×1 wiring canary
on `engine-ab-single-resume`; stop and fix injection if the native oracle
observes `remote_v2`.

```powershell
cargo run -p vellum-eval --bin vellum-eval -- matrix --suite compaction-engine-ab --task engine-ab-single-resume --compaction-engine codex-local-v0-150 --repeat 1
cargo run -p vellum-eval --bin vellum-eval -- matrix --suite compaction-engine-ab --task engine-ab-single-resume --compaction-engine codex-local-native-oracle --repeat 1
cargo run -p vellum-eval --bin vellum-eval -- report <run-id> --format json
```

The HTML report and `compaction-engine-comparison.json` summarize task pass,
duplicate work, wall time, tokens, and compression ratio by model × engine.

CI should run Rust contract/replay tests only. `prepare`, `run`, and `matrix`
are explicit live commands because they require Docker, Provider credentials,
network access during image preparation, and paid model traffic.
