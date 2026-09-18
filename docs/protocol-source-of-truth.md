# Vellum protocol source of truth

This document defines the protocol behavior that current Vellum implementations
must preserve. It is intentionally limited to production data flow, ownership
boundaries, failure semantics, and compatibility rules.

It is not an incident log, release manifest, test transcript, or provider setup
guide. Put dated evidence and acceptance runs in the relevant documents under
`docs/` or `evals/`.

## Implementation authority

When prose and code disagree, stop and reconcile them in the same change.
These locations own the current behavior:

| Concern | Authority |
| --- | --- |
| Shared proxy routing, adapters, streaming, errors, continuation, and compaction | `crates/vellum-proxy-runtime` |
| Compaction engine (Codex Local Compact 0.150 port) and its upstream provenance | `crates/vellum-proxy-runtime/src/codex_local_v0_150.rs`, `third_party/codex-0.150/PROVENANCE.md` |
| Desktop proxy lifecycle and Enhanced Runtime orchestration | `src-tauri/src/commands/proxy.rs`, `src-tauri/src/enhanced_runtime/` |
| Desktop HTTP and WebSocket bridge | `src-tauri/src/proxy.rs`, `src-tauri/src/proxy_runtime_bridge.rs` |
| Remote deployment and daemon wiring | `src-tauri/src/remote/`, `crates/vellum-proxy-runtime/src/runtime.rs` |
| Provider configuration and UI | `src/`, `src-tauri/src/commands/` |
| Cross-provider evaluation gates | `evals/` and `evals/README.md` |

`proxy_legacy.rs` is not the authority for shared behavior. A fix needed by
Desktop, the headless daemon, or a bundled remote image belongs in
`vellum-proxy-runtime`.

## Execution planes

Vellum has two Desktop execution planes:

1. **Native Codex** — the unmodified Codex process and its Official OpenAI
   connection.
2. **Enhanced Runtime** — Vellum's verified App Server bridge, used when a task
   is adopted by the proxy.

A task is bound to one execution plane. Once adopted, it must not silently
switch planes mid-conversation. A failure before adoption may leave Codex
native; a failure after adoption must be surfaced as the real failure.

### Proxy and Enhanced Runtime lifecycle

- Starting Proxy automatically attempts to arm Enhanced Runtime.
- Stopping Proxy releases Vellum's Enhanced Runtime injection and restores the
  native launch state.
- There is no independent user-facing enable/disable switch for Enhanced
  Runtime. The UI reports whether it is loaded and exposes its status.
- If arming fails, Proxy may remain running. Vellum releases any launch
  injection it owns, leaves unadopted Codex sessions native, and shows a
  non-blocking warning.
- An adopted task does not fall back to another execution plane when its bridge,
  route, provider, continuation, or protocol contract fails.

Prepare is reusable and is not part of the start transaction:

- Prepare validates the live executable identity, schema compatibility, Codex
  capability discovery, and a settings fingerprint. Reuse is allowed only while
  file identity / change monitoring still prove the artifacts are unchanged —
  not from path, version text, or a stale hash alone. Desktop prepare reuse is
  bound to the packaged `enhanced-runtime.lock.json` digest (the same
  `include_bytes` artifact Enhanced launch uses). An installed Desktop has no
  source-tree lockfile path; a missing path must not count as a match.
- Concurrent prepare requests for the same runtime share one validation.
- Prepare does not write the live catalog. Catalog history and current search
  settings are committed under the lifecycle lock, so a cancelled prepare
  cannot rewrite files after Stop or label an old catalog as current.
- The start transaction is lease check, necessary catalog commit, listener
  bind, authenticated readiness, then diversion settings. Diversion is
  committed only after readiness succeeds; later failure rolls that
  transaction back. Authenticated `/readyz` must not wait on hashing the
  process image; executable SHA-256 is a diagnostic (`/version`, live
  attribution), not a readiness gate.
- Stop immediately refuses new requests, cancels the current generation's
  HTTP / WebSocket / stream / background work, closes connections, and
  restores only Vellum-owned settings and the launch lease. Interrupted work
  is recorded as `user_stopped_proxy` / `proxy_stopped`, never as success or a
  generic provider error. The user's native Codex daemon is not killed.
- VACUUM and WAL checkpoint are bounded background maintenance after Stop,
  not part of the Stop completion path. Start / Stop / Repair share a
  generation so stale tasks cannot write newer status.
- Maintenance is admitted only with no active request guards and retains the
  lifecycle lock through completion. A Start overlapping admitted maintenance
  waits for it; that latency must be measured, not counted as a prepared fast
  start. Enhanced verification runs on a blocking worker; its shared file and
  environment writes, error cleanup, and notices must recheck generation under
  the lifecycle lock. A late old-generation result cannot re-arm after Stop or
  release a newer generation's lease.
- Proxy status keeps the boolean `running` field and adds
  `preparing` / `starting` / `running` / `stopping` / `stopped` / `failed`.
  Events and status carry operation ID, stage, elapsed time, and a concrete
  error. The Desktop host emits generation-keyed `proxy://lifecycle` events as
  status mutates so preparing/starting/stopping stages reach the UI without
  waiting for the status poll. Only the current generation is applied.

### Launch-path lease

Vellum treats `CODEX_CLI_PATH` as a three-way lease:

- **Absent before Vellum**: Vellum may inject its facade and must remove that
  injection when releasing the lease.
- **Already Vellum-owned**: Vellum may refresh it and later remove it.
- **Externally owned**: Vellum must preserve it and must not overwrite or remove
  it.

Cleanup must be idempotent and scoped to the value Vellum owns.

### Shared Codex home

Desktop, Enhanced Runtime, and remote-control discovery must resolve the same
effective Codex home. Do not create a second implicit home for the injected
runtime. Authentication, task metadata, and discovery must remain visible
across the native and enhanced paths.

Sharing the Codex home does not transfer an active thread writer between App
Server processes. A build without the multi-client relay sidecar must leave
Remote Control on Official Codex so a phone can read and resume Desktop's
native conversations. A build with the sidecar must route each remote request
through the thread's immutable execution-plane binding.

The relay must also preserve the native App Server initialization contract:
after Desktop initializes, it receives the relay's current
`remoteControl/status/changed` state. A relay process reaching its stdio-ready
state is not proof that Remote Control is online; the bridge must bootstrap
the current status so Desktop can restore an enabled preference, and relay
startup/auth failures must remain visible in Desktop diagnostics.
The relay uses the same `Codex Desktop` persistence identity as the native
stdio App Server so an Enhanced bridge restart reconnects the existing mobile
environment rather than creating a second empty-key enrollment.

### Tool continuations

When Vellum executes a provider-requested tool locally, the continuation must
retain every provider-required assistant field from the requesting hop. In
particular, Chat thinking providers that bind readable `reasoning_content` to
tool calls must receive that reasoning on the same assistant message as the
original call before its tool result. Reconstructing only the function call and
result is not a valid continuation, even when the client-visible Responses
stream is otherwise complete.

### Artifact and protocol verification

Enhanced Runtime activation verifies the actual artifacts being launched:

- the bundled runtime artifact digest selected by the build target's pinned
  entry in `enhanced-runtime.lock.json`;
- the bridge/facade digest; and
- the App Server protocol exposed by the resolved runtime binary.

Protocol compatibility is determined from the actual binary's schema:

- byte-identical schemas are **verified**;
- schema differences outside methods Vellum routes may be **unverified** but
  usable;
- a difference affecting a routed method or shape is **incompatible** and must
  block adoption.

Do not rely on a stale stored protocol hash as a substitute for checking the
resolved binary. An unverified runtime may arm, but adoption requires the
runtime attestation and routed-method compatibility checks to pass.

## Inbound proxy contract

The Desktop bridge and headless daemon expose the same logical protocol:

- OpenAI-compatible HTTP endpoints, including Responses and Chat Completions;
- Official OpenAI Responses passthrough;
- the Official Responses WebSocket path where supported; and
- bounded streaming, request, and response handling.

Provider credentials are accepted only through Vellum's configured secret
resolution. Never log authorization headers, API keys, OAuth tokens, full
credential-bearing URLs, or raw secret stores.

Default safety bounds are implementation configuration, not provider promises:

- maximum request body: 300 MiB;
- concurrent HTTP requests: 32;
- concurrent Official WebSocket sessions: 64;
- concurrent streams: 16;
- aggregate buffered stream data: 512 MiB;
- third-party non-stream response body: 64 MiB;
- individual stream event: 8 MiB;
- pending stream buffer: 16 MiB;
- Official stream idle timeout: 2 minutes; and
- third-party stream idle timeout: 15 minutes.

If these defaults change, update the implementation, tests, and this section
together.

OpenCode Go enforces a separate 4.5 MiB upstream request limit. For translated
Chat requests, Vellum preserves ordinary image data URLs unchanged and only
downscales/re-encodes them when the complete request exceeds that provider
limit, targeting 4 MiB to retain bounded JSON/tool headroom.
Unsupported images remain unchanged so the provider's real rejection category
and bounded diagnostic are preserved.

## Route selection

Route selection is explicit and deterministic:

1. Identify whether the request is Official passthrough or a Vellum provider
   route.
2. Resolve the requested model through the configured provider/model mapping.
3. Bind the task to the selected route.
4. Preserve that binding for continuation and child-agent traffic.

Do not silently redirect an Official request to a third-party provider. Do not
use substring guesses or a default provider to hide an unknown model mapping.
Return a bounded, specific routing error.

Provider base URLs use HTTPS by default. Plain HTTP is allowed only for an
explicitly configured local or trusted development endpoint; it must never
become an accidental fallback for a remote host.

### WebSocket per-turn routing

Execution-plane binding does not permit a stale WebSocket route. Every
`response.create` resolves its model and route from a fresh catalog snapshot:

- compatible Official turns may reuse the open Official segment;
- changing provider or Official connection identity closes that segment and
  performs a portable handoff;
- opaque reasoning, continuation, ciphertext, and authentication state never
  cross that handoff; and
- an unknown model, changed authentication posture, or unsafe mid-flight route
  change fails closed without silently reordering the turn.

An untagged first WebSocket frame is treated as an implicit
`response.create`.

## Official OpenAI

Official OpenAI Responses is a native passthrough contract:

- preserve the request and response protocol instead of translating through a
  third-party Chat or Responses adapter;
- do not redirect an Official `compaction_trigger` request;
- preserve Official WebSocket session semantics when that transport is used;
- preserve the selected ChatGPT account and its OAuth behavior; and
- report authentication, quota, protocol, and transport failures accurately.

Codex configuration and catalog metadata use `default_reasoning_summary:
"none"` to mean that reasoning summaries are disabled. Preserve that value in
the projected Official catalog so Codex and its UI retain the user's intended
setting. The ChatGPT Responses wire contract does not accept `"none"` as a
`reasoning.summary` value, so the shared Official HTTP/WebSocket outbound
boundary omits that optional field. It preserves `reasoning.effort`, supported
summary values, absent fields, third-party catalog defaults, and the read-only
source Codex catalog cache.

Every Official dispatch records a bounded structured diagnostic containing the
request/route/model identities, HTTP versus WebSocket transport, managed versus
incoming authentication mode, selection revision and verification state,
hashed control/execution workspace identities, the allow-listed requested and
effective reasoning-summary values, and whether a disabled summary was omitted
at the wire boundary. It never records tokens, authorization
headers, raw account identifiers, or request bodies. Together with the
credential-identity hash in `official_account_selected`, the revision joins an
execution back to the exact explicit account choice even when two users share
one ChatGPT workspace.

Display the human account identity returned by the authenticated account data
when available. An internal account identifier is a fallback, not the preferred
label, and no email address is hard-coded into the protocol.

A managed ChatGPT credential is identified by both its human user principal and
its selected ChatGPT workspace. `chatgpt_account_id` is the workspace/billing
identifier sent upstream; it is not a unique login identity because two users
can hold seats in the same workspace. Credential storage, selection, quota
caches, and Remote Control slots use a stable hash of user plus workspace, while
the upstream header continues to carry only the workspace id. Account surfaces
show email together with workspace name (or plan/id fallback), so one user's
Personal and workspace memberships remain visibly distinct.

Opaque Official reasoning and compaction state belongs to the Official plane.
It must not be forwarded to third-party providers.

### Managed ChatGPT quota pool

Desktop may opt managed ChatGPT credentials into an automatic quota pool. The
pool is disabled by default; while disabled, the verified manual account
selection remains authoritative. Pool membership, pause state, member order,
and each account's weekly remaining-floor are persisted locally.

For each new ordinary Official request, the Desktop authorization boundary
reads current 5-hour and weekly quota windows and selects only an explicitly
pooled, unpaused account whose 5-hour window is non-zero and whose weekly
remaining percentage is above its configured floor. Rotation follows the
explicit member order and skips members that are currently unavailable without
reordering the others. Legacy `most` and `soonest` settings deserialize as
`rank` and serialize back as `rank`. Missing quota data fails that member
closed. An enabled pool with no members keeps the manual selection until the
user adds one; once at least one member is configured, having no usable member
must not cross a weekly floor, consume a Reset credit, or silently use a
pool-external account. Auto Review's explicitly selected billing account
bypasses this ordinary-request policy.

Quota readings are cached for at most 30 seconds per token and workspace;
force-refresh invalidates the previous reading. Gates control admission of new
requests, not the final cost of an already admitted turn. Usage by other
clients and in-flight turns can therefore cross a floor before the next
reading. No in-flight request is replayed automatically after a quota error.
Each managed credential is a distinct pool member. Multiple user seats in the
same workspace may be pooled because upstream quota windows can be scoped to
the authenticated user; their access tokens, quota caches, gates, and rotation
state remain separate. A configuration change during selection rejects that
selection so the caller can retry with the new settings.

Opt-in local validation: `cargo test -p vellum --lib live_quota_pool_routing
-- --ignored --nocapture`. It refreshes existing managed grants, reads real
upstream quota, and requires two usable accounts. Pool settings and runtime
history are isolated in a temporary directory. It checks member-order routing,
gates, pause, exhaustion, persistence, then sends two minimal Official Luna
requests through the Desktop proxy, one per selected account. It never redeems
Reset credits or changes the production pool.

## Third-party provider portability

Third-party providers receive only portable conversation state. Vellum must
strip or materialize provider-specific state before crossing provider
boundaries, including:

- opaque Official reasoning items;
- encrypted or provider-owned continuation handles;
- unmaterialized foreign compaction items; and
- transport-only metadata that the destination provider does not understand.

When a verified third-party capability probe advertises reasoning efforts but
does not name a default, the Desktop catalog selects `medium` when available,
otherwise the lightest non-`none` effort. It must not silently default a
reasoning-capable route to `none` merely because that is the first slider item.

The portable state consists of system/developer instructions, user and
assistant text, supported tool declarations, tool calls and results, and
Vellum-owned checkpoint summaries.

### Responses-compatible providers

For providers with a compatible Responses endpoint:

- send the provider's supported Responses shape;
- preserve portable item order and tool-call pairing;
- use provider-supported continuation fields only when their ownership and
  lifetime are known; and
- normalize the response and stream into Vellum's internal event model without
  inventing missing reasoning. Preserve non-zero provider usage; when usage is
  missing or all-zero, project a Vellum-owned conservative estimate from the
  actual model-visible request and response so Codex can maintain its context
  meter and compaction threshold. Do not treat that estimate as
  provider-reported usage.

### Chat Completions providers

For Chat-only providers:

- convert portable Responses items to ordered Chat messages;
- preserve the current user turn exactly once;
- keep assistant tool calls paired with their tool results;
- on vision routes, preserve image-bearing tool results by closing every
  textual tool result first, then carrying the images in a multimodal user
  continuation after the complete tool-result batch;
- translate supported tool declarations and tool choice explicitly;
- treat unsupported Responses-only fields as compatibility decisions, not
  passthrough data; and
- map the Chat response or SSE stream back to the requested Vellum/OpenAI
  response shape.

Do not append a duplicate copy of the current query during translation.

### Grok-compatible routes

Grok routes use their configured provider adapter. They follow the same
portable-state and error rules as other third-party routes. Provider-specific
normalization must stay isolated in the adapter and covered by focused tests;
it must not leak into Official passthrough.
Grok Build models that do not advertise `none` receive an omitted optional
Effort field when an older Codex thread retains that stale selection; all
provider-supported named Effort values are preserved unchanged.

### OpenCode routes

OpenCode is a provider/runtime integration, not an Official OpenAI substitute.
Its model and endpoint resolution must be explicit. OpenCode responses,
streaming events, tool calls, usage, and failures are normalized through the
shared runtime just like other third-party routes.

Every OpenCode request carries a stable `x-opencode-session`, including model
catalog/capability probes, Auto Review, compaction, search, retry, and
continuation requests whose conversation identity is held in proxy metadata
rather than the translated JSON body. Vellum hashes the resolved conversation
key before sending it and never exposes the raw Codex session or thread
identifier.

## Tools and search

Tool definitions cross a provider boundary only when the destination supports
them. Adapters must preserve stable tool names, argument JSON, call identifiers,
result pairing, and ordering.

Search is a tool capability, not permission to invent citations. A provider may
use native search only when the configured route supports it. Otherwise Vellum
uses its declared search tool path or reports that search is unavailable.
Returned citations must originate from actual provider or tool output.

Codex's catalog field `supports_search_tool` also enables client-side deferred
MCP/App tool discovery. Vellum advertises that transport for third-party models
independently of whether the optional Brave web-search loop is enabled. Turning
off web search must not force every deferred tool schema into the first model
request or change its context footprint.

Unknown or unsupported tools must produce a bounded compatibility error. They
must not be silently dropped when doing so would change the requested behavior.

## Continuation and compaction

Continuation state has an owner:

- Official continuation and compaction remain owned by Official OpenAI/Codex.
- Third-party opaque continuation remains owned by that provider and may be
  reused only on a compatible route.
- Vellum-owned canonical checkpoints contain portable state and may cross
  providers.

Desktop Enhanced Runtime leaves routine conversation compaction to Codex. The
proxy must not intercept or redirect Official `compaction_trigger` traffic.

Remote and evaluation flows that do not have the same Codex-owned lifecycle may
use the canonical checkpoint pipeline in `vellum-proxy-runtime`. Vellum owns
checkpoint assembly: a summarizer may return prose, and provider output is not
required to be perfect JSON. The runtime validates, bounds, and assembles the
portable checkpoint.

Canonical compaction is not a selectable Desktop replacement for native Codex
compaction.

### The compaction engine

Engine resolution is a pure function of the route's provider kind, not a
setting. Official resolves to native passthrough and is never intercepted;
every third-party and Review route resolves to `codex_local_v0_150`. There is
no compactor route/model selector, no global compactor, no Grok-native path,
and no production "disabled" strategy -- a route that could silently stop
compacting, or compact on some other model, is the drift this removes.

The engine preserves the semantics and provenance documented beside its
implementation. It runs on the session's own route, model, authentication, and
verified effort after provider-private state is removed. It does not require a
JSON summary contract or retarget another model after failure.

A journal row identifies its engine. Materialization validates the engine,
schema version, and integrity hashes, and refuses state from an incompatible
engine instead of guessing how to resume it.

An Official compaction is journalled too, but it is not a before/after: the
result is ciphertext Vellum never reads, so the row stores the window that went
*in*, which is what a third-party route needs when it later meets the same
opaque marker. Such a row is flagged `provider_owned`, because the two shapes
are otherwise indistinguishable in the stored columns and a reader that guesses
reports the compaction backwards.

### Who asks for one

Codex schedules its own compaction from the catalog, and what the catalog
advertises is what decides who compacts:

- Desktop publishes `context_window` and no `auto_compact_token_limit`, so
  Codex derives its own threshold and compacts the thread locally. This is the
  Enhanced Runtime path and the reason the local proxy is a gateway here.
- A remote-managed Codex is published an explicit `auto_compact_token_limit`
  derived from the deployed threshold, and its `compaction_trigger` reaches the
  runtime, which compacts with the engine above.
- A model with no policy in an opted-in projection omits `context_window` as
  well, or Codex would schedule against its own fallback threshold.

`features.auto_compaction` is not a Codex feature flag -- current Codex answers
that name with "Unknown feature flag" -- so it is never written. The catalog is
the only scheduling signal Vellum gives Codex.

On any continuation path:

- validate provider, model, task, and checkpoint ownership before reuse;
- never send a foreign opaque handle to another provider;
- preserve unresolved tool-call/result relationships;
- reject incompatible state with the real continuation or compaction error; and
- avoid replaying the current user turn twice.

When a portable checkpoint is projected onto the Chat wire, it is background
context rather than a new user command. If that checkpoint would otherwise be
the final semantic message, append exactly one fixed synthetic user
continuation that tells the provider to resume the task. Never dispatch a Chat
request with the checkpoint as its assistant tail, and never treat the
synthetic continuation as the current user query.

## Auto Review and Guardian

Auto Review consumes the real task diff/context and calls the configured review
provider through the production route. Tests must exercise the same adapter and
streaming/data-flow path; a permissive mock is not evidence of provider
compatibility.

The review result must retain:

- provider/model identity;
- completion or failure state;
- bounded provider diagnostics;
- structured findings when available; and
- usage attributable to the review call.

Provider rows with no review activity in the trailing seven days are omitted
from the review-usage display. Historical records remain stored.

Guardian is orchestration around actual reviews. It must not turn provider,
authentication, quota, protocol, or parsing failures into a successful empty
review. Retry and fallback are explicit policy decisions and remain bounded.
Each reviewer leg is projected against that reviewer's own context window.
When inherited task history would overflow a smaller fallback route, preserve
the Guardian policy and planned-action boundaries, replace inherited history
with one explicit assessment turn, and leave tokenizer/output headroom. Chat
reviewers must receive a semantic user turn and an explicit single-outcome JSON
instruction; a malformed or missing outcome remains a protocol failure.

Regardless of an advertised route window, one Guardian projection is capped at
64K estimated input tokens. A 500K parent conversation is reduced while keeping
the authorization policy and planned-action boundaries; it is never forwarded
verbatim merely because the parent model accepts it. OpenAI-compatible routes
whose upstream model identifies as Qwen receive a 50-second attempt
deadline. When such a route is the fallback, its 50 seconds are reserved by
limiting the non-Qwen primary to 20 seconds; the whole review remains bounded
to 70 seconds so Codex still receives a terminal result before its own client
deadline.

## Streaming and error semantics

Trajectory loop detection is diagnostic only. Repeated tool calls, identical
tool results, or a long sequence of tool exchanges must not inject loop-guard
instructions, remove the available tools, force a final answer, or reject a
later continuation. This applies to Desktop and remote proxy execution alike.
Independent task-recovery policies and transport/resource limits keep their
own contracts; trajectory observations do not escalate into execution control.

Streaming adapters must:

- parse events incrementally;
- preserve text, tool-call, lifecycle, completion, and usage ordering;
- enforce event, pending-buffer, total-buffer, concurrency, and idle limits;
- terminate cleanly on provider completion; and
- surface malformed or truncated streams as protocol/transport failures.

For Chat-compatible SSE, either the conventional `[DONE]` marker or a parsed
choice carrying a non-null `finish_reason` is explicit provider completion.
Connection close without either signal remains a truncated-stream failure.

After a tool result, an HTTP 200 response containing no body bytes is retried
once with an explicit continuation instruction. The retry is dispatch-only and
uses the same route and model. A second empty stream remains a
`provider_protocol` failure and is recorded as a failure rather than success.

When a tool-capable Chat route has just received a tool result, Vellum may
retry once if the model's entire completion is only a short announcement of a
future action and contains no tool call. The announcement is not exposed as a
final answer. The retry receives the announcement as commentary plus an
explicit instruction to issue the promised tool call or report a concrete
result or blocker. A second such completion is terminal, so this recovery can
never form an unbounded provider loop.

Streaming qualification is based on real application deltas. Text routes use
text deltas and tool routes use function-argument deltas; reasoning events do
not substitute for either. A stream that completes without its expected delta
is a protocol failure. Only genuinely incremental delivery qualifies a route
as streaming; an end flush or non-streaming response does not.

The outward error must retain its category:

- configuration or routing;
- authentication or authorization;
- quota or rate limit;
- provider availability;
- request or response protocol;
- continuation or compaction;
- stream or transport; or
- internal invariant failure.

Include only a bounded, sanitized provider diagnostic. Never replace distinct
failures with a generic high-demand message, and never expose secrets or an
unbounded upstream body.

## Identity, native subagents, and accounting

Enhanced Runtime forwards native App Server subagent lifecycle traffic rather
than synthesizing a UI-only substitute. Parent and child identities must remain
stable across spawn, message, wait, resume, close, and completion.

Required behavior:

- a child inherits the parent's bound execution plane and route;
- child requests travel through the real App Server and proxy data path;
- child lifecycle events are forwarded in their typed form;
- child output is delivered to the parent through the native peer-message
  contract;
- an `agent_message` entering the parent conversation materializes as peer
  input, represented as `user` / `input_text`, not as an assistant answer;
- cancellation or failure remains attributable to the child that produced it;
  and
- the native chatbox/activity UI is driven by real lifecycle events.

Cancelling a parent fans out through the shared cancellation registry to every
confidently bound child. A child linked to an already-cancelled parent must
abort before sending upstream bytes. Each request records one terminal usage
row, and a late completion must not overwrite a recorded cancellation.

Usage accounting is graph-based:

- root task usage is counted once;
- each child task is counted once under its parent;
- repeated lifecycle observations do not create new agents or duplicate usage;
- system-event counts are derived from typed events, not text matching; and
- review, compaction, and subagent usage retain their own categories before
  aggregation.

UI summaries may abbreviate token values with `K` and `B`, but stored
accounting retains exact values.

## Remote boundary

Remote deployment packages the same shared proxy runtime; it is not a separate
protocol implementation. A remote image may add transport, supervision, and
deployment concerns, but provider routing, portability, streaming, error,
continuation, and canonical-checkpoint behavior remain owned by
`vellum-proxy-runtime`.

Generated payloads under `src-tauri/resources/remote` are build artifacts.
Never hand-edit them. Build scripts must derive them from the current source,
and release validation must use a source-fingerprinted fresh image.

Remote control and Desktop discovery must refer to the actual running task and
effective Codex home. Do not infer availability from a second runtime database
or a stale generated payload.

Remote identity keeps two roles distinct:

- the control account owns the Codex daemon, tasks, approvals, pairing, and
  session authority; and
- an optional Vellum-managed execution account changes only provider
  authorization.

Selecting an execution account must not rewrite the control account's
`CODEX_HOME/auth.json`, restart its daemon, or change task ownership. Remote
surfaces expose only bounded display identity and hashes, never tokens, raw
credential data, or upstream bodies.

## Change checklist

For changes covered by this contract:

1. Modify the owning shared implementation, not only a bridge or legacy copy.
2. Add focused tests that use the production adapter and data shape.
3. Update this document in the same change if a protocol invariant changes.
4. Run `cargo test -p vellum-proxy-runtime --lib`.
5. Run `pnpm typecheck` and focused UI tests when UI changed.
6. Run the relevant cross-provider continuation/compaction eval gate when that
   behavior changed.
7. Run `git diff --check`.
8. For release or remote-image work, follow `BUILDING.md`; do not treat a
   development server as release validation.

Detailed design, operations, and acceptance evidence belong in:

- `docs/enhanced-runtime-mvp.md`
- `docs/remote-proxy-manager.md`
- `docs/remote-acceptance.md`
- `evals/README.md`
- `BUILDING.md`
