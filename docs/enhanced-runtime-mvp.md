# Enhanced Codex Runtime MVP

Vellum Desktop can bind a thread to one of two Codex execution planes:

- **Official Codex** — the unmodified OpenAI Codex binary, used for Official / GPT traffic.
- **Enhanced Codex** — a pinned OpenAI Codex upstream plus three independently gated ports.

Enhanced Codex is the agent-loop authority. Vellum does not add Canonical V2,
Rolling Local, Action Conversion, loop guard, or any other historical Vellum
harness onto this path. A thin provider gateway may only inject auth, map
endpoints, translate Responses/Chat, normalize SSE, map provider errors, and
account usage.

## Source pins

See `enhanced-runtime.lock.json`. Revisions must not track `main` / `master` /
`latest`. The portable port source lives in `crates/vellum-enhanced-codex`.
It is **not** an Enhanced Codex runtime until it is compiled into

```text
910488/enhanced-codex-core @ the commit pinned by `enhancedCodexCommit`
```

at `codex-rs/core/src/enhanced/` and called from the four agent-loop seams in
`patches/openai-codex-enhanced-mvp/SEAMS.md`. Attribution is in
`crates/vellum-enhanced-codex/THIRD_PARTY_NOTICES.md`.

`enhancedCodexCommit` and the current build target's entry in `artifacts` must
be real values before Desktop may bind an Enhanced thread. Each platform entry
pins both the downloadable archive and the executable inside it. An empty or
missing target entry is an incomplete identity, not a
digest.

Pinned commits:

| Project | Commit |
|---|---|
| OpenAI Codex | `6bc50f104dcc0192e696cdeae721dfc19b507391` (`rust-v0.153.0`) |
| Qwen Code | `2b8f73c1e9cf8b355ec46c4623398c27b458b076` |
| DeepSeek Harness | `dd6322d604e00eec1ba5e0c8541159906a21094a` |

Protocol compatibility is measured, not pinned. Both cores are asked to emit
their own app-server schema and the two bundles are compared method by method
and shape by shape. Byte-identical bundles short-circuit to `verified`; a
difference that touches none of the methods the bridge demultiplexes on is
`unverified` and still runs, listed for the reader; a difference that does touch
one is `incompatible` and withholds arming. There is no stored hash to keep
current, because a stored hash could only ever say "not identical", which is
true of almost every Codex Desktop update and disqualifying in almost none.
`third_party/codex-app-server-schema/0.142.5/` remains the compatibility
baseline for `vellum-codex-facade`, which is migrated independently and does
not use the runtime comparison above. It is the only schema bundle left in the
tree: the Enhanced pin's own 301-file bundle was that pin's evidence, and both
are gone.

## Ports

| Port | Gate | Behavior |
|---|---|---|
| A | `qwen_tool_reliability` | Provider call-id ledger, fingerprint, duplicate suppression, fail-closed collisions, no synthetic success |
| B | `deepseek_context_recovery` | Deterministic tool-result prune, re-measure, Codex local compact only if still over threshold, one overflow retry that requires surface progress |
| C | `qwen_bounded_continuation` | At most two auto-continuations from deterministic unfinished signals; cancel and user steer win |

Ablation profiles E0–E5 are selected explicitly by eval. Model names never pick a profile.

## Desktop routing

`src-tauri/src/enhanced_runtime` owns:

- trusted `provider_id` routing (never model name)
- immutable `ThreadRuntimeBinding` (plane + runtime digest)
- one canonical Desktop `CODEX_HOME` task authority shared by both children
- fail-closed launch (no Enhanced → Official fallback)

Changing execution plane requires a new thread.

### Desktop App Server wiring

Starting the local Proxy automatically verifies the installed Official core,
the pinned Enhanced core, and the bridge executable. There is no independent
Enhanced toggle: Proxy start arms the bridge, and Proxy stop releases it. In
packaged builds the bridge is the
`vellum-codex-app-server` sidecar that ships beside the Vellum executable, not
a mode flag on the Vellum binary: Codex Desktop starts `CODEX_CLI_PATH`
directly, so it has to name a real standalone program.
The bridge is build-owned and not user-selectable. Proxy startup always selects
the sidecar whose digest was embedded into that Vellum host; a stale saved
development path is released using the old path and then replaced atomically.
Vellum selects the pinned Enhanced core automatically; users cannot substitute
arbitrary bridge bytes.

Vellum points `CODEX_CLI_PATH` at that sidecar through a per-user environment
lease with three-way restore — the pre-lease value is recorded before ours is
written, disabling restores exactly what was there, and a value someone else
has since changed is left alone. Re-enabling also refuses to overwrite a
current value that no longer belongs to the recorded lease. The lease is
broadcast (`WM_SETTINGCHANGE` on
Windows, `launchctl setenv` on macOS) before the packaged app is started.
Older builds could leave the exact configured Vellum bridge behind after the
lease file disappeared, or record that bridge as its own predecessor. Current
enablement treats that exact unleased value as recoverable Vellum state;
Disable removes it. A different value is always foreign and is never changed.

The lease is scoped to the local Proxy lifecycle. Proxy start verifies and
arms the launch after the listener is ready. Proxy Stop, Repair, the Vellum
window close button, tray Stop, explicit Vellum Exit, and a managed Codex
restart all use the same arm/disarm rule: the launch lease is taken only while
this process's Proxy is serving. A stopped Proxy never leaves a future Codex
launch pointed at the bridge. A leftover installed Vellum host still holding 15721 is a
fail-closed conflict, not a second injector. When Codex is already running,
teardown remains restart-required until Desktop has actually reloaded the
restored provider and process environment.

The bridge configures itself from a credential-free launch manifest that Vellum
writes atomically at `enhanced-runtime/launch-manifest.json`, carrying both
binaries and their hashes, the runtime digests, the model map and its hash,
the binding database, both `CODEX_HOME` paths, and a launch id.

The manager rejects enablement unless all of these checks pass:

- the configured Official core is the core discovered from the installed
  Codex Desktop;
- the Official core's generated App Server schema matches the protocol lock;
- the Enhanced binary hash matches `artifactSha256`;
- the bridge binary hash matches the sidecar this release shipped;
- the runtime lock and manifest identity are complete;
- the bridge and both child executables are absolute existing files.

### Ready means observed, not verified

`artifactReady` is a fact about files on disk. `enabled` records that the
running Proxy successfully prepared Enhanced. `active` is a fact about Codex
Desktop, and only the bridge can report it: it writes a `BridgeAttestationV1`
naming its own launch id, pid, parent, both child pids and binary hashes, the
protocol and model-map hashes, and a `starting | ready | degraded | failed |
stopped` state.
`ready = enabled && artifactReady && active && leaseOwned`, and `active`
additionally requires that the live bridge parent is the recorded installed
Codex Desktop executable and that the Enhanced child reported its identity
after initialize. The reported digest, fork commit, and all three feature flags
must match the launch manifest; a missing identity is not inferred from disk.

The global top-left runtime indicator renders artifact verification,
environment ownership, observed Desktop state, and blockers from attestation.
It must not collapse "waiting for restart", "failed", "environment drift",
and "active" into one Ready/Blocked label.

New threads require a mapped catalog model or trusted `modelProvider`; an
unknown new thread fails closed. For old pre-bridge chats only, an unbound
resume with neither hint may discover the thread through Official Codex. The
binding is committed only after Official returns the same thread id, so a lost
Enhanced binding cannot silently become Official.

`restart_codex_safely` therefore no longer treats a new Codex pid as success on
the Enhanced path; it waits for a ready attestation carrying the same launch id
and reports `EnhancedDesktopBridgeNotObserved` if none arrives.

### Qualification gates

```
vellum-eval enhanced-integration-gate --mode bridge
vellum-eval enhanced-integration-gate --mode installed --no-active-turn
```

Bridge mode is non-interactive and CI-runnable: by default it starts the real
bridge binary against deterministic App Server surrogates backed by the real
portable Enhanced modules and a scripted local Responses provider. That is a
component pass, never promotion GO. Explicit Official and Enhanced binaries
can be supplied for binary-level qualification. Installed mode uses private
per-run attestation and journal paths, restarts the real packaged Codex Desktop, and refuses
to report success without a matching ready attestation. Both write
`%LOCALAPPDATA%\Vellum\enhanced-runtime\qualifications\<run-id>\report.json`,
which carries binary identities, hashed thread ids, bindings, event counts,
provider request counts and every hard gate — and no prompts, tool output,
authorization, or API keys.
The status surface treats that result as current only while its bridge,
Official and Enhanced hashes still match the binaries now selected. Older
reports remain on disk as history but are not displayed as evidence for a
newer artifact.

Both child processes use the canonical Desktop `CODEX_HOME`. This is required
because Codex Remote Control owns one relay and one task manager: isolating the
Enhanced rollout and SQLite stores made Enhanced tasks invisible on mobile.
`remoteControl/*` lifecycle calls are owned by the Enhanced child, while normal
Desktop thread calls remain provider-bound by the bridge. Enhanced behavior is
selected by the attested process environment and runtime digest, not by a
second task store. A trusted model-provider map is regenerated from Vellum
catalog metadata on every managed restart. Model-name prefixes are never
routing authority.

## Eval

In-process fixtures (no provider quota):

```text
cargo test -p vellum-enhanced-codex --lib
cargo test -p vellum --lib enhanced_runtime
cargo run -p vellum --bin vellum-eval -- enhanced-mvp-fixtures
```

Live matrix (requires the Enhanced Codex **binary** whose digest is in the lock
file, plus provider credentials). Each suite encodes its profile; CLI must
match:

```text
cargo run -p vellum --bin vellum-eval -- matrix --suite evals/manifests/enhanced-mvp-e0.json --ablation-profile E0 --compaction-engine codex-local
```

The runner writes `$CODEX_HOME/enhanced-runtime.json` and records
`ablationProfile`, `runtimeDigest`, `enhancedCodexCommit`, and `featureFlags`
on `run.json`. Promotion stays **NO-GO** until those fields are real and the
hard safety gates pass on live Enhanced Codex telemetry, not in-process
self-tests. At least one target model (exact model id) must improve on B0/E0
for a reliability metric, not merely token or latency drop.

For real-machine observation, Desktop also writes an Enhanced-only `debugLog`
block to the isolated runtime home. The runtime writes structured JSONL to
`log/enhanced-events.jsonl` through a bounded non-blocking queue. Defaults are
an 8 MiB active file, four files total, a 2,048-event queue, and a 250 ms batch
flush. Queue pressure drops diagnostics rather than blocking the agent loop;
the next successful batch records `droppedEvents`. The records contain event
names, measurements, outcomes, and hashed identifiers only. They never contain
prompts, tool arguments, tool results, credentials, or reasoning.

The observation log includes the non-intervention denominator as well as
interventions: normal E1 admission/completion, every E2 pressure decision, and
every E3 stop evaluation. This allows a real-machine report to distinguish
"feature enabled but not exercised" from "feature intervened and changed the
run." The evaluator's explicit evidence log remains synchronous and separate.

Settings can export one Vellum diagnostics ZIP without pausing the proxy. The
export reads the retained Desktop log files, Enhanced JSONL rotations, trace
JSONL, and a T3-stripped projection of the structured runtime diagnostics. It
never copies credentials, configuration, history, or SQLite files. Export is
performed on the blocking worker pool, limits each source to 32 MiB and the
whole text payload to 128 MiB, and records truncation in `manifest.json`.
Secrets and user paths are redacted again, while request, session, thread, and
call identifiers become stable short hashes so related events remain useful.

## The retired Vellum compaction surface

Canonical is no longer something a Vellum install can select. Removed with the
cutover: the Settings "context compaction strategy" card and its frontend API,
types, helper and strings; the ten Tauri policy commands
(`get`/`set` session, route and global policy, the global compactor,
`resolve_compaction_policy`, `resolve_auto_compact_token_limit`); the persisted
session / route / global policy layers on `AppState`; the Vellum-projected
`auto_compact_token_limit` on every local catalog write; and `proxy_legacy.rs`,
the retired Desktop provider pipeline that was the last consumer of all of it.
The dormant `run_compaction` and `restore_latest_compaction` commands were also
removed: Desktop may observe runtime-owned compaction records, but it cannot
start, replace, or roll back the executing Codex runtime's context state.

Desktop still publishes each third-party model's `context_window`; Codex needs
that value both to render context usage and to derive its native local
compaction threshold. `[model_providers.vellum].name` is deliberately
`"Vellum"`, not `"OpenAI"`: bundled Codex uses the display name as its remote
compaction capability gate. This keeps third-party Enhanced threads on native
local compaction instead of sending an unsupported remote `/compact` request
to Vellum's gateway. Official threads get their own table,
`[model_providers.vellum-official]`, whose `name` *is* `"OpenAI"` — the model
on the other side of the hop is OpenAI's — so they keep remote compaction,
image generation, native web search, and history notes. Both tables point at
the same proxy listener: an Official turn that skipped the proxy would also
skip the ChatGPT account the user selected, and would be invisible to usage,
quota, and the log.

`policy.rs` now resolves Official to `ProviderNative` and every third-party
Provider to `Disabled` / `ThirdPartyGateway`, and
`state.runtime_canonical_engine()` returns `None` in production.

Two exceptions, both deliberate:

- **Eval.** `set_eval_route_compaction_policy` and `set_eval_canonical_engine`
  are dropped outside eval mode. Baseline runs still have to reproduce
  Canonical V1/V2 to compare an Enhanced profile against.
- **Remote.** A remote host serves a plain Codex over the network and has no
  Enhanced runtime to hand compaction to, so the remote runtime still compacts.
  `remote/deployment.rs` builds that policy itself rather than resolving a
  Vellum-wide one.

## Out of scope for this MVP

Vellum Canonical V1/V2, Rolling Local, Semantic Frontier, Action Conversion,
ResearchSprawl recovery, Task Stall / Loop Guard, Qwen todo/plan/full loop,
DeepSeek Cordis/event bus/session store, LLM-as-judge recovery, and
provider-specific threshold tuning.
