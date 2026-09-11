# Codex agent-loop seams

Search the pinned Codex tree (`633ab199cfd724aa78013c006b27a2b3d049fc3b`)
for these insertion points. The portable crate exposes `EnhancedTurnHooks`.
Load `$CODEX_HOME/enhanced-runtime.json` at session start; if the file is
absent, every hook must `DeferToUpstream`.

Published at `910488/enhanced-codex-core`; Vellum pins the exact source commit
and per-platform release artifacts:

| Seam | File |
|---|---|
| Session load | `session/session.rs` (`enhanced` field) |
| Tool dispatch | `tools/router.rs` `dispatch_tool_call_with_code_mode_result_inner` |
| Pre-sampling compact | `session/turn.rs` `run_pre_sampling_compact` |
| Overflow retry | `session/turn.rs` `run_sampling_request` `ContextWindowExceeded` |
| Turn-stop continuation | `session/turn.rs` after native `run_turn_stop_hooks` |
| Lifecycle reset | `run_turn` `on_new_user_input` / `on_assistant_success` / `Drop` idle |

Portable modules use `super::` so they compile both as this crate and as
`codex_core::enhanced`.

## Session lifecycle

```text
new user input admitted
        → hooks.on_new_user_input()
          (resets continuation budget and overflow retry sequence)

successful assistant / message response
        → hooks.on_assistant_success()
          (ends the current overflow recovery sequence)

turn idle
        → hooks.on_turn_idle()
          (clears overflow retry and any uncommitted continuation reservation)
```

Do not treat `on_new_user_input` as a stop decision. Reset is not AllowStop.

## 1. Tool dispatch

After a provider function call is parsed and before side-effecting execution:

```rust
match hooks.admit_tool_call(identity, &mut telemetry) {
    HookDecision::DeferToUpstream => { /* existing Codex dispatch */ }
    HookDecision::Handled(AdmitDecision::Execute) => { /* existing Codex dispatch */ }
    HookDecision::Handled(AdmitDecision::SuppressDuplicate { message }) => {
        return synthetic duplicate/failed output; do not execute
    }
    HookDecision::Handled(AdmitDecision::FailClosed { message }) => {
        return protocol collision error
    }
}
```

First admit is `InFlight` until the original attempt completes. On late
`function_call_output` for a handled id, call `ingest_late_tool_result`.

## 2. Pre-sampling compact

Before Codex local compact:

```rust
match hooks.plan_pressure(&surface, window, compact_threshold, &mut telemetry) {
    HookDecision::DeferToUpstream => { /* existing compact check */ }
    HookDecision::Handled(plan) => {
        apply plan.prune.surface to the model-visible input only
        if plan.compact == CompactDecision::Skip { skip native compact }
        else { existing Codex local compact }
    }
}
```

Do not add a second summarizer. E0 must take the DeferToUpstream arm.

## 3. Provider context overflow

Only on a confirmed context-window-exceeded provider error:

```rust
match hooks.plan_overflow(&before, &after, cancelled, &mut telemetry) {
    HookDecision::DeferToUpstream => return original error,
    HookDecision::Handled(OverflowDecision::Retry { .. }) => retry once,
    HookDecision::Handled(OverflowDecision::PreserveOriginalError) => original error,
    HookDecision::Handled(OverflowDecision::Cancelled) => cancel,
}
```

After a successful retry that produces an assistant response, call
`on_assistant_success()` so a later overflow can start a new sequence.

## 4. Turn-stop / bounded continuation

On the native turn-stop hook, **plan only**. Do not consume budget until the
extra primary-model stream is created:

```text
natural model stop
        → plan = hooks.on_turn_stop(context)
        → Continue { index } + reservation

response stream successfully created
        → hooks.commit_continuation(reservation)

stream creation failed
OR pre-stream compact failed
OR token rejection before the stream
        → hooks.release_continuation()
```

```rust
match hooks.on_turn_stop(&context, &mut telemetry) {
    HookDecision::DeferToUpstream => { /* existing stop */ }
    HookDecision::Handled(plan) => match plan.decision {
        ContinuationDecision::Continue { .. } => {
            let reserved = plan.reservation.expect("continue reserves an attempt");
            match start_primary_stream(CONTINUE_NUDGE) {
                Ok(_) => hooks.commit_continuation(reserved)?,
                Err(_) => hooks.release_continuation(),
            }
        }
        _ => { /* stop */ }
    }
}
```

`NativeSubagentWorkRemaining` alone must not consume the primary continuation
budget. Cancellation and user steer always win. User steer resets the stage
budget; it does not continue the previous stage.

## 4. Reporting which runtime ran the turn

The fork emits exactly two App Server notifications, and nothing else with a
`vellum/` prefix. They are addressed to the Vellum bridge, which absorbs them;
Codex Desktop never sees them.

The Desktop-aligned fork wires this at `codex-rs/core/src/enhanced/reporting.rs`,
`codex-rs/app-server/src/request_processors/initialize_processor.rs`, and
`codex-rs/app-server/src/message_processor.rs`. The fork-local payload builder
mirrors the portable contract because the external Codex repository cannot
depend on this workspace crate. The bridge allowlists event names and field
names, so any drift or extra field is dropped and recorded as `Rejected` in the
qualification journal.

`vellum/enhancedRuntimeIdentity` is the only place the running binary states
its own commit, digest, and three port flags. Vellum never infers them from the
model name or from the file on disk, so a fork that does not send it is treated
as an Enhanced runtime that never reported, not as a healthy one.

## 5. After the fork builds

`enhanced-runtime.lock.json` carries `enhancedCodexCommit` and an `artifacts`
entry for every supported Rust target. The Enhanced Core repository's artifact
workflow builds those archives; copy the published archive and executable
SHA-256 values into the lock before promotion, then re-run:

```
cargo test -p vellum-enhanced-codex
cargo test -p vellum --lib enhanced_runtime
vellum-eval enhanced-integration-gate --mode bridge
vellum-eval enhanced-integration-gate --mode installed --no-active-turn
```

## 6. Divergences found on 2026-09-03, and how they were closed

The fork vendors its own copy of these modules at `codex-rs/core/src/enhanced/`;
it does not depend on the crate. Two copies of the same file drift, and they
had. Diffing them (ignoring line endings, `use` splitting and rustfmt wrapping)
turned up four real differences plus one calibration error present in both.

As of 2026-09-03 all eleven shared modules are byte-identical between
`crates/vellum-enhanced-codex/src/` and `codex-rs/core/src/enhanced/`, and
`cargo check -p codex-core` passes on the fork. What follows is what was wrong
and what the fix commits the fork to.

### The calibration error: token estimation was not Codex's

`ModelVisibleSurface::estimated_tokens` counted `chars().count() / 4` for
payload text while using `.len()` for identifiers. Codex counts UTF-8 bytes over
`APPROX_BYTES_PER_TOKEN = 4` (`codex-rs/utils/string/src/truncate.rs`), applied
to the serialized JSON. On ASCII the two agree; on CJK a character is three
bytes, so the port reported a third of Codex's number and the pruner believed it
had room it did not have.

Both copies now count bytes throughout: `byte_count()` replaces `char_count()`,
`ToolResultPrunePolicy` fields are `min_text_bytes` / `keep_head_bytes` /
`keep_tail_bytes`, and `prune_blocks` walks a byte cursor while still emitting
whole characters so pruned text stays valid UTF-8. `seams.rs` follows at its one
call site (`before.byte_count()` / `after.byte_count()`).

Counting bytes is still not the same as counting what Codex counts.
`response_items_to_surface` flattens items to plain text, so the JSON wrapper
and the per-modality adjustments Codex applies to image, audio and encrypted
payloads are gone before the port sees anything. A base64 image measured 6,400
bytes of decoded text where Codex charges a flat per-image estimate, and no
amount of arithmetic inside the port recovers that: the information is not
there.

**So the seam carries the number instead.** `ModelVisibleSurface` gained
`item_token_estimates: Vec<Option<u64>>`, positionally aligned with `items`, and
`estimated_tokens()` prefers the entry at index N over its own byte guess.
`response_items_to_surface` fills it from
`crate::context_manager::estimate_item_token_count` — the same function
`compact_remote*` uses — so Codex stays the single authority on how big an item
is, and the port only has to receive the answer. The vector defaults to empty
and is skipped when serializing, so a host that supplies nothing gets the byte
estimate and the older on-disk shapes still deserialize.

The one thing the port must do itself is invalidate: `apply_pressure_prune`
clears the estimate for every item it rewrote, and only those, because a
host-supplied number describes the item that was measured, not the shorter one
the pruner left behind. Three tests pin this — the estimate is preferred, a
rewritten item loses it, and its neighbours keep theirs.

### Present in the fork, now taken from the crate

**`telemetry::field_name_is_forbidden` was weaker in the fork.** It normalized
with `name.trim().to_ascii_lowercase().replace('-', "_")`, which only
lowercases. The forbidden list is snake_case and the wire format is camelCase,
so `toolOutput` became `tooloutput` and did not contain `tool_output`. Four
entries were dead in the fork: `tool_output`, `raw_tool_output`, `user_text`,
`encrypted_content` — exactly the fields that carry model and user content.
(`rawPrompt`, `apiKey` and `authorization` still matched by accident, through
the single-word entries `prompt`, `apikey`, `auth`.) The fork now has the
crate's camelCase-to-snake_case normalizer.

**`lockfile` constants disagreed.** `CODEX_UPSTREAM_COMMIT` differed;
`QWEN_CODE_SOURCE_COMMIT` and `DEEPSEEK_HARNESS_SOURCE_COMMIT` already matched.
The crate is authoritative and the fork now carries its values. (There was also
an `APP_SERVER_PROTOCOL_HASH` in both. It has since been retired: nothing ever
compared it against a binary, and `protocol_compat` measures both cores'
protocols at runtime instead.)

**Smaller drift.** The fork derived `#[derive(Debug, Serialize)]` on the
notification payload where the crate also derives `Clone, PartialEq, Eq,
Deserialize`, so the fork could not round-trip its own notifications in a test.
`AutoContinuationBudget::reserved_index` existed only in the crate.

### Keep them from drifting again

Nothing checks these two trees against each other. Until something does, treat
a change to `crates/vellum-enhanced-codex/src/` as incomplete until the same
change lands in `codex-rs/core/src/enhanced/`. The check is a diff over the
eleven shared modules: `bounded_continuation`, `config`, `context_pruner`,
`context_recovery`, `digest`, `gateway`, `hooks`, `lockfile`, `notifications`,
`telemetry`, `tool_reliability`. `mod.rs`, `runtime.rs`, `reporting.rs` and
`seams.rs` are fork-only wiring and have no crate counterpart.
