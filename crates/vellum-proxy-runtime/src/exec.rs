//! The production request execution engine (plan §25.3).
//!
//! [`crate::state::ProxyRuntimeState`] remains the backing surface ??
//! configuration, diagnostics, service adapters. [`ProxyRuntime`] is the thing
//! that actually executes a request against a captured
//! [`RuntimeSnapshot`]. M3C wires the upstream dispatch step end-to-end for
//! non-streaming requests; M5 adds real streaming (SSE passthrough with
//! third-party normalization, Chat?�Responses translation via
//! [`ChatSseAdapter`], and Official completion tracking). Nothing here
//! silently degrades a stream into a buffered call.
//!
//! Execution order (plan §23.5): live configuration ??request admission ??
//! capture one `RuntimeSnapshot` ??resolve route ??resolve harness profile ??
//! verify catalog/profile contract ??resolve auth **once** ??project the
//! route the adapter reads ??translate the request ??transport ??decode ??
//! normalize ??`RuntimeResponse::Json`. The snapshot is captured once, so a
//! route or policy change mid-request can never leak into a request that
//! already started; the auth is resolved once, so a credential rotation can
//! never half-change an in-flight call.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use futures_util::stream::{BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::adapter::{contains_vellum_synthetic_item, sanitize_for_official, ChatSseAdapter};
use crate::auth::ResolvedAuth;
use crate::codex_local_v0_150;
use crate::compaction::{
    estimate_tokens, has_compaction_trigger, rebuild_canonical_checkpoint_from_source,
    response_output_text, sanitize_portable_history,
};
use crate::continuation::{
    ensure_encrypted_reasoning_include, request_disables_server_store, resolve_continuation,
    ContinuationMode,
};
use crate::credentials::CredentialProvider;
use crate::diagnostics::{
    bounded_text, hash_text, input_item_manifest, redact_sensitive_json, redact_sensitive_text,
    spawn_prompt, ChildTurn, CodexMetadataConflictDiagnostic, CompactionDecision, CompactionEngine,
    CompactionOutcome, DetailLevel, DiagnosticEvent, DiagnosticsSink, InputItemManifest,
    LinkConfidence, NoopSink, SpawnCompleted, SpawnRequested, SubagentGraphLinked,
    SubagentLinkComparison, SubagentLinkMethod, TrajectoryDecisionDiagnostic, UsageWriteFailure,
};
use crate::error::RuntimeError;
use crate::grok_session::{conversation_key_from_request, GrokSessionRegistry};
use crate::harness::{resolve_with_options, HarnessOptions, HarnessProfile};
use crate::history::{
    flatten_chain_items, resolve_conversation_key, ConversationKeyResolution, HistoryEntry,
    HistoryStore, LocalCompactionRecordV1, MemoryHistoryStore,
};
use crate::investigation_reducer::InvestigationPolicy;
use crate::investigation_runtime::{
    build_investigation_runtime_input_seeded_with_usage_and_revision,
    reduce_investigation_input_traced_with_stall_and_efficiency,
};
use crate::lifecycle::{RequestGuard, RequestLifecycle};
use crate::official_auth::{OfficialAuthProvider, OfficialAuthorization};
use crate::profile_adapter::{
    prepare_upstream_request_details, prepare_upstream_request_with_environment,
    ProfileAdapterRoute,
};
use crate::replay::{
    assert_input_identity_len, checkpoint_item_identity, is_client_local_compaction_marker,
    is_verified_summary_replacement, journal_item_identity, request_item_identity,
    seed_request_identities, HydrationOutcome, ReplayContext,
};
use crate::request::{RequestMetadata, RuntimeRequest};
use crate::resource::{ResourceGuard, ResourcePolicy};
use crate::response::normalize_non_streaming_response;
use crate::review::{
    default_review_settings_source, is_guardian_request, prepare_guardian_request,
    resolve_guardian_route_plan, ReviewModelRoute, ReviewSettings, ReviewSettingsSource,
    StaticReviewSettingsSource,
};
use crate::route::{RouteCatalog, RuntimeModelRoute, RuntimeProviderKind, RuntimeWireFormat};
use crate::search::{DisabledSearchEngine, SearchEngine};
use crate::search_loop::{
    append_search_outputs, completed_search_events, execute_pending_searches, SearchLoopSession,
    SseAction,
};
use crate::snapshot::RuntimeSnapshot;
use crate::sse::{append_utf8_safe, strip_sse_field, take_limited_sse_block};
use crate::streaming::{
    assign_response_message_phases, completed_response_from_sse, failed_sse_event,
    failed_sse_event_with_category, normalize_third_party_readable_reasoning, parse_sse_value,
    sse_event_is_response_completed, terminal_error_from_sse, SseCompletionTracker,
    ThirdPartySseNormalizer,
};
use crate::task_efficiency::TaskEfficiencyPolicy;
use crate::task_stall::TaskStallPolicy;
use crate::trajectory::{analyze_trajectory, LoopGuardPolicy};
use crate::transport::{
    TransportError, UpstreamRequest, UpstreamResponse, UpstreamStream, UpstreamTransport,
};
use crate::usage::{usage_details_from_response, MemoryUsageStore, UsageRecord, UsageStore};

/// Third-party non-streaming calls are single buffered exchanges; bound the
/// wait so a wedged provider cannot pin a worker forever. Official native
/// HTTP has no overall duration cap.
const NON_STREAMING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
/// Maximum time a native spawn window can associate an ordinary child request
/// with its parent. The value is intentionally shorter than the proxy request
/// timeout so stale windows cannot manufacture low-confidence links.
const SUBAGENT_LINK_WINDOW: std::time::Duration = std::time::Duration::from_secs(900);

fn sha256_hex(payload: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(payload.as_bytes()))
}

/// Official per-chunk idle deadline (Desktop `UPSTREAM_STREAM_IDLE_TIMEOUT`).
/// This is idle = no bytes, not a total task time.
const OFFICIAL_STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Third-party stream idle deadline: 15 minutes with no bytes.
const THIRD_PARTY_STREAM_IDLE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(15 * 60);

/// Bound on the *header* phase of a streaming upstream call: from send to the
/// upstream opening response headers. This is distinct from every idle
/// timeout above (which start only once headers have already arrived) and
/// from `PRODUCTION_CONNECT_TIMEOUT` (TCP/TLS handshake only) — a peer that
/// accepts the connection and then never answers would otherwise stall this
/// phase indefinitely, since streaming dispatch deliberately passes no
/// overall `UpstreamRequest::timeout`.
///
/// Some third-party routes (Qwen 3.8 on `api.provider.example`) legitimately take
/// ~12-98s to open headers on a cold start; 30s would abort those turns and
/// turn a slow provider into a Vellum blocking failure. Match the Official
/// idle deadline so a slow-but-valid header open is never cut short.
const STREAM_OPEN_HEADER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Used when draining a small upstream error envelope.
const STREAM_IDLE_TIMEOUT: std::time::Duration = OFFICIAL_STREAM_IDLE_TIMEOUT;

/// Bounds how long one Guardian attempt (primary or fallback) may run before
/// this dispatch gives up on it. Deliberately much shorter than
/// `NON_STREAMING_TIMEOUT`/`STREAM_OPEN_HEADER_TIMEOUT`: those bound a
/// user-facing chat turn, where a slow-but-legitimate cold start must not be
/// cut short. Guardian is a background approval gate riding in front of that
/// same turn — a stalled primary here must free the fallback with enough
/// runway to still beat Codex's own client-side timeout for the whole
/// review request (~75s, observed live). The deadline covers the *entire*
/// attempt, not just the header phase: headers opening, a keepalive, or a
/// partial delta are not "done" for Guardian's purposes, only a complete,
/// valid assessment is (see `collect_guardian_attempt`).
const GUARDIAN_ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Total wall-clock budget for one Failover review dispatch (the primary
/// attempt plus, if needed, one fallback attempt). Leaves headroom under
/// Codex's own ~75s client timeout so a fallback that starts right at the
/// primary deadline still has time to answer before Codex gives up and the
/// review is lost regardless of what Vellum decides.
const GUARDIAN_REVIEW_TOTAL_BUDGET: std::time::Duration = std::time::Duration::from_secs(65);

/// Third-party non-stream buffered response cap.
const THIRD_PARTY_NON_STREAM_MAX_BYTES: usize = 64 * 1024 * 1024;

/// A completed execution output.
pub enum RuntimeResponse {
    Json(Value),
    /// An SSE stream of client-visible events. Stream-level failures are
    /// surfaced as `response.failed` events *inside* the stream (Desktop
    /// disconnect semantics), not as a `RuntimeError`.
    Sse(BoxStream<'static, Result<Bytes, RuntimeError>>),
    /// A byte-for-byte upstream response whose HTTP metadata is observable.
    Raw {
        status: u16,
        content_type: Option<String>,
        body: Vec<u8>,
    },
}

/// Envelope prefix for locally materialized compaction items.
///
/// Codex Local Compact 0.150 writes its own prefix so materialization can
/// parse exactly one engine's markers. The retired Canonical prefix is
/// recognized only to reject a legacy task by name.
const LOCAL_COMPACTION_PREFIX: &str = crate::replay::CODEX_LOCAL_V0_150_MARKER_PREFIX;

/// Random engine-neutral local compaction id, `cmp_local_<32 hex>`.
fn new_local_compaction_id() -> Result<String, RuntimeError> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|error| RuntimeError::Internal(format!("cannot create compaction id: {error}")))?;
    Ok(format!(
        "cmp_local_{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn local_compaction_item(id: &str) -> Value {
    json!({
        "id": id,
        "type": "compaction",
        "encrypted_content": format!("{LOCAL_COMPACTION_PREFIX}{id}")
    })
}

fn seed_replay_identities(
    input_len: usize,
    replay: &mut ReplayContext,
) -> Result<(), RuntimeError> {
    if replay.item_identities.is_empty() && input_len > 0 {
        replay.item_identities = seed_request_identities(input_len);
        if replay.suffix_end == usize::MAX {
            replay.suffix_end = input_len;
        }
    }
    assert_replay_aligned(input_len, replay)
}

fn assert_replay_aligned(input_len: usize, replay: &ReplayContext) -> Result<(), RuntimeError> {
    assert_input_identity_len(input_len, &replay.item_identities).map_err(RuntimeError::Internal)
}

fn apply_range_delta(bound: &mut usize, item_index: usize, delta: isize) {
    if item_index >= *bound {
        return;
    }
    if delta >= 0 {
        *bound = bound.saturating_add(delta as usize);
    } else {
        *bound = bound.saturating_sub((-delta) as usize);
    }
}

fn strip_client_local_compaction_pairs(
    pairs: &[(Value, String)],
) -> (Vec<(Value, String)>, bool, Option<String>) {
    let mut out = Vec::with_capacity(pairs.len());
    let mut repaired = false;
    let mut discarded_summary: Option<String> = None;
    let mut skip_verified_replacement = false;
    for (item, identity) in pairs {
        if skip_verified_replacement {
            skip_verified_replacement = false;
            if is_verified_summary_replacement(item) {
                if discarded_summary.is_none() {
                    discarded_summary = Some(
                        item.get("content")
                            .map(crate::adapter::content_to_plain_string)
                            .unwrap_or_default(),
                    );
                }
                repaired = true;
                continue;
            }
            out.push((item.clone(), identity.clone()));
            continue;
        }
        if is_client_local_compaction_marker(item) {
            repaired = true;
            skip_verified_replacement = true;
            continue;
        }
        out.push((item.clone(), identity.clone()));
    }
    (out, repaired, discarded_summary)
}

fn realign_identities_after_item_filter(
    replay: &mut ReplayContext,
    before: &[Value],
    after: &[Value],
) -> Result<(), RuntimeError> {
    if before.len() == after.len() {
        return assert_replay_aligned(after.len(), replay);
    }
    seed_replay_identities(before.len(), replay)?;
    let mut kept = Vec::with_capacity(after.len());
    let mut old_index = 0;
    let mut prefix_len = 0;
    let mut suffix_start = 0;
    for item in after {
        while old_index < before.len() && &before[old_index] != item {
            old_index += 1;
        }
        if old_index < before.len() {
            kept.push(
                replay
                    .item_identities
                    .get(old_index)
                    .cloned()
                    .unwrap_or_else(|| request_item_identity(kept.len())),
            );
            if old_index < replay.prefix_len {
                prefix_len += 1;
            }
            if old_index < replay.suffix_start {
                suffix_start += 1;
            }
            old_index += 1;
        } else {
            kept.push(request_item_identity(kept.len()));
        }
    }
    replay.item_identities = kept;
    replay.prefix_len = prefix_len;
    replay.suffix_start = suffix_start;
    replay.suffix_end = after.len();
    assert_replay_aligned(after.len(), replay)
}

impl std::fmt::Debug for RuntimeResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeResponse::Json(value) => f.debug_tuple("Json").field(value).finish(),
            RuntimeResponse::Sse(_) => f.write_str("Sse(<stream>)"),
            RuntimeResponse::Raw {
                status,
                content_type,
                body,
            } => f
                .debug_struct("Raw")
                .field("status", status)
                .field("content_type", content_type)
                .field("body_len", &body.len())
                .finish(),
        }
    }
}

/// The one production request execution engine, shared by Desktop and the
/// headless daemon.
pub struct ProxyRuntime {
    catalog: Arc<dyn RouteCatalog>,
    credentials: Arc<dyn CredentialProvider>,
    official_auth: Arc<dyn OfficialAuthProvider>,
    lifecycle: Arc<dyn RequestLifecycle>,
    transport: Arc<dyn UpstreamTransport>,
    history: Arc<dyn HistoryStore>,
    /// Shared Grok session/turn-index registry (moved out of Desktop's
    /// per-caller bookkeeping ??see `crate::grok_session`). One instance per
    /// running `ProxyRuntime`, so Desktop and the headless daemon each get
    /// their own in-process registry through this same implementation.
    grok_sessions: Arc<GrokSessionRegistry>,
    opencode_quota: Arc<crate::opencode::OpenCodeQuotaCooldown>,
    usage: Arc<dyn UsageStore>,
    search_engine: Arc<dyn SearchEngine>,
    /// Whether the local `web_search` compatibility wrapper should be offered
    /// to third-party routes (M8 real fix). Populated into every request's
    /// [`RuntimeSnapshot`] at capture time; see [`Self::with_web_search_wrapper_enabled`].
    web_search_wrapper_enabled: bool,
    /// Per-turn cap on server-side `web_search` hops (see
    /// [`crate::search_loop::SearchLoopSession`]). Defaults to
    /// [`crate::search_loop::DEFAULT_SEARCH_TOOL_LOOP_LIMIT`]; overridable via
    /// [`Self::with_search_tool_loop_limit`].
    search_tool_loop_limit: u8,
    /// Where *this request's* Auto Review policy comes from. Never read
    /// outside of building a fresh [`RuntimeSnapshot`] — a source must be
    /// asked again for every new request, never memoized on `self`, or a
    /// policy change in the UI would have no effect on an already-running
    /// proxy (see [`ReviewSettingsSource`]).
    review_source: Arc<dyn ReviewSettingsSource>,
    harness_options: HarnessOptions,
    delegation_runtime_wired: bool,
    task_stall_policy: TaskStallPolicy,
    task_efficiency_policy: TaskEfficiencyPolicy,
    task_stall_bounded_finalization: bool,
    diagnostic_config_hash: String,
    resource: ResourcePolicy,
    diagnostics: Arc<dyn DiagnosticsSink>,
    subagent_spawns: Arc<Mutex<Vec<PendingSubagentSpawn>>>,
    subagent_graph: Arc<Mutex<crate::subagent_graph::SubagentGraphRegistry>>,
    shadow_subagent_graph: Arc<Mutex<crate::subagent_graph::SubagentGraphRegistry>>,
    subagent_identity_mode: SubagentIdentityMode,
    parent_cancel: Arc<Mutex<ParentCancelRegistry>>,
    generation_stop: watch::Sender<bool>,
    efficiency_recoveries: Arc<Mutex<HashMap<String, LiveEfficiencyRecoveryEntry>>>,
}

#[derive(Debug, Clone)]
struct LiveEfficiencyRecoveryEntry {
    state: crate::task_efficiency::LiveEfficiencyRecoveryState,
    touched: tokio::time::Instant,
}

const LIVE_EFFICIENCY_RECOVERY_TTL: std::time::Duration =
    std::time::Duration::from_secs(6 * 60 * 60);

/// Rollout mode for Codex subagent turn and graph identity resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentIdentityMode {
    Legacy,
    #[default]
    Shadow,
    OfficialPreferred,
    OfficialOnly,
}

/// Auto Review provenance for one exchange (M9), threaded from
/// [`RequestMetadata`] into every dispatch path — buffered and streaming
/// alike — so the [`UsageRecord`] a request produces always agrees with what
/// [`ProxyRuntime::execute_against_snapshot`] actually decided, instead of
/// the streaming generators silently dropping it back to `None`.
#[derive(Debug, Clone, Default)]
struct GuardianProvenance {
    run_id: Option<String>,
    role: Option<String>,
    reason: Option<String>,
}

#[derive(Debug, Clone)]
struct PendingSubagentSpawn {
    parent_request_id: String,
    /// Internal registry key for the parent turn (see
    /// [`ParentCancelRegistry`]). Kept separate from `parent_request_id`,
    /// which stays the external correlation id shown in diagnostics: a
    /// WebSocket connection can reuse the same `x-request-id` across turns,
    /// so only a freshly generated per-turn id is safe as a cancellation key.
    parent_execution_id: String,
    call_id: String,
    prompt_hash: String,
    model: Option<String>,
    started: std::time::Instant,
    linked_request_ids: Vec<String>,
    saw_parallel_spawn: bool,
    time_window_candidates: Vec<ChildTurn>,
}

/// Every registry entry is timestamped so bounded cleanup never depends on a
/// caller remembering to release it. `sockets`/`terminal` used to be pruned
/// only by explicit unregister calls, which normal (non-cancelled,
/// non-disconnected) stream completions never issued -- both maps grew
/// without bound for the lifetime of the process. They now carry the same
/// `created` timestamp every other entry does, and `prune` retains all four
/// maps by both a TTL and a hard capacity cap.
struct TimestampedSender {
    sender: watch::Sender<bool>,
    created: tokio::time::Instant,
}

struct TimestampedOutcome {
    created: tokio::time::Instant,
}

/// How long a terminal-outcome dedupe entry survives. This only needs to
/// outlive the race window between a stream collector's own completion and
/// an external cancel/disconnect observer (milliseconds in practice); it is
/// deliberately much shorter than [`SUBAGENT_LINK_WINDOW`] so `terminal`
/// cannot become the registry's largest map.
const TERMINAL_DEDUPE_TTL: std::time::Duration = std::time::Duration::from_secs(120);

/// Hard cap on each map's size, independent of TTL correctness. A pathologic
/// client that opens far more concurrent/rapid turns than any real workload
/// would generate must evict the oldest entries rather than grow unbounded;
/// a bound this size is already generous for any real deployment.
const REGISTRY_MAX_ENTRIES: usize = 20_000;

/// Mint a fresh internal execution id. Every call site that dispatches a
/// turn -- HTTP Responses/Chat, WebSocket portable-replay, WebSocket
/// Official-native -- calls this once immediately before dispatch and
/// threads the result through [`ProxyRuntime::register_request_cancel`] and
/// every subsequent cancel/terminal call for that turn. Never derived from
/// the external `request_id`: a WebSocket client can legitimately reuse the
/// same `x-request-id` across several turns on one connection, which would
/// otherwise let one turn's cancel socket or terminal-dedupe entry collide
/// with another's.
pub fn new_execution_id() -> String {
    format!("exec_{}", ulid::Ulid::new())
}

/// Parent→child cancellation fan-out for subagent turns.
///
/// Every request registers a one-shot cancel channel under a freshly
/// generated *execution id* -- never the external `request_id`, which a
/// WebSocket client may reuse across several turns on the same connection.
/// A confident parent link (Point B) binds the child's channel to the
/// parent; [`ProxyRuntime::cancel_request_tree`] then fires every bound child
/// channel, dropping the child's in-flight upstream stream without the client
/// having to cancel each socket separately. A child linking to a parent that
/// was already cancelled aborts before it ever reaches upstream.
///
/// The terminal guard makes each execution's outcome idempotent: the first
/// terminal write wins, so a child collector can never have a late
/// "completed" overwrite the cancel that already landed (INV-CANCEL-1). Every
/// terminal write additionally releases the execution's cancel socket in the
/// same locked step, so a normal (uncancelled) completion no longer leaks a
/// `sockets` entry the way it used to (P0: registry entries never expired).
#[derive(Default)]
struct ParentCancelRegistry {
    sockets: HashMap<String, TimestampedSender>,
    parents: HashMap<String, ParentCancelEntry>,
    cancelled: HashMap<String, tokio::time::Instant>,
    terminal: HashMap<String, TimestampedOutcome>,
    /// Test-only: the most recently registered execution id, so black-box
    /// tests that dispatch through the real HTTP/WebSocket surface (and so
    /// never see the internally minted id directly) can still target it with
    /// [`ProxyRuntime::cancel_request`]/[`ProxyRuntime::cancel_request_tree`]
    /// without reaching into production code paths to expose it.
    #[cfg(test)]
    last_registered_execution_id: Option<String>,
}

struct ParentCancelEntry {
    cancelled: bool,
    created: tokio::time::Instant,
    children: Vec<String>,
}

/// Test-only: current sizes of each registry map, so cleanup tests assert
/// directly on the bounded maps instead of guessing from indirect behavior.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct RegistryLenSnapshot {
    pub(crate) sockets: usize,
    pub(crate) parents: usize,
    pub(crate) cancelled: usize,
    pub(crate) terminal: usize,
}

/// Evict the oldest entries of `map` (by `created_at`) until it is at or
/// under `REGISTRY_MAX_ENTRIES`. A last-resort bound independent of TTL
/// correctness -- see [`REGISTRY_MAX_ENTRIES`].
fn evict_oldest<V>(map: &mut HashMap<String, V>, created_at: impl Fn(&V) -> tokio::time::Instant) {
    if map.len() <= REGISTRY_MAX_ENTRIES {
        return;
    }
    let mut entries: Vec<(String, tokio::time::Instant)> = map
        .iter()
        .map(|(key, value)| (key.clone(), created_at(value)))
        .collect();
    entries.sort_by_key(|(_, created)| *created);
    let overflow = map.len() - REGISTRY_MAX_ENTRIES;
    for (key, _) in entries.into_iter().take(overflow) {
        map.remove(&key);
    }
}

impl ParentCancelRegistry {
    fn prune(&mut self) {
        let cutoff = SUBAGENT_LINK_WINDOW;
        self.parents
            .retain(|_, entry| entry.created.elapsed() < cutoff);
        self.cancelled
            .retain(|_, marked_at| marked_at.elapsed() < cutoff);
        self.sockets
            .retain(|_, entry| entry.created.elapsed() < cutoff);
        self.terminal
            .retain(|_, entry| entry.created.elapsed() < TERMINAL_DEDUPE_TTL);
        evict_oldest(&mut self.sockets, |entry| entry.created);
        evict_oldest(&mut self.terminal, |entry| entry.created);
        evict_oldest(&mut self.parents, |entry| entry.created);
        evict_oldest(&mut self.cancelled, |marked_at| *marked_at);
    }
}

/// First-writer-wins terminal claim, as a free function over just the
/// registry `Arc`. Stream collectors run inside `async_stream::stream!`
/// closures that are `'static` and therefore only hold cloned `Arc` fields,
/// never `&ProxyRuntime` itself -- this is what lets them share the exact
/// same atomic guard as every `&self` call site instead of falling back to a
/// local, per-collector `bool` that a racing external cancel/disconnect
/// could never see.
fn claim_terminal_once_raw(
    registry: &Arc<Mutex<ParentCancelRegistry>>,
    execution_id: &str,
    // Not persisted: the first-writer-wins claim only needs to know *that* a
    // terminal outcome landed, not which one. Kept as a parameter so every
    // call site still states its outcome at the claim point, matching
    // `finish_terminal_once`'s public signature and leaving room to log it
    // later without touching every caller again.
    _outcome: &str,
) -> bool {
    let mut registry = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let won = registry
        .terminal
        .insert(
            execution_id.to_string(),
            TimestampedOutcome {
                created: tokio::time::Instant::now(),
            },
        )
        .is_none();
    if won {
        registry.sockets.remove(execution_id);
    }
    won
}

/// Free-function counterpart to
/// [`ProxyRuntime::finish_terminal_once_with_record`], for stream-collector
/// closures (see [`claim_terminal_once_raw`]).
fn finish_terminal_once_with_record_raw(
    registry: &Arc<Mutex<ParentCancelRegistry>>,
    diagnostics: &Arc<dyn DiagnosticsSink>,
    usage: &Arc<dyn UsageStore>,
    execution_id: &str,
    outcome: &str,
    record: UsageRecord,
) -> bool {
    if !claim_terminal_once_raw(registry, execution_id, outcome) {
        return false;
    }
    if let Err(message) = usage.record(&record) {
        // The terminal claim is deliberately *not* rolled back: a lost write
        // must not let a late duplicate resurrect the turn (INV-CANCEL-1).
        // Surface the rejection so usage loss is investigable instead of
        // silent.
        record_usage_write_failure_diagnostic(
            diagnostics,
            execution_id,
            outcome,
            record.request_id.clone(),
            &record.route_id,
            &message,
        );
    }
    true
}

/// Emit the bounded `UsageWriteFailure` diagnostic without ever surfacing a
/// sink failure to the request path. Shared by the method call sites and the
/// stream-collector raw path so both record exactly one structured event.
fn record_usage_write_failure_diagnostic(
    diagnostics: &Arc<dyn DiagnosticsSink>,
    execution_id: &str,
    outcome: &str,
    request_id: Option<String>,
    route_id: &str,
    message: &str,
) {
    log::warn!(
        "terminal usage row failed to persist (execution_id={execution_id}, outcome={outcome}, route_id={route_id}); keeping the terminal claim: {message}"
    );
    let event = DiagnosticEvent::UsageWriteFailure(UsageWriteFailure {
        execution_id: execution_id.to_string(),
        outcome: outcome.to_string(),
        request_id,
        route_id: route_id.to_string(),
        error: bounded_text(message, 512),
    });
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        diagnostics.record(event);
    }));
}

/// RAII handle for one execution's cancel-socket registration. Dropping it
/// (on any exit path: return, `?`, panic unwind, or the wrapped stream simply
/// being dropped) releases the socket even if no code path explicitly called
/// [`ProxyRuntime::finish_terminal_once`] -- a structural backstop on top of
/// the explicit terminal write, not a replacement for it (the terminal
/// dedupe entry in `terminal` is deliberately *not* touched here: it must
/// outlive this guard so a late duplicate write is still rejected).
pub(crate) struct CancelRegistration {
    registry: Arc<Mutex<ParentCancelRegistry>>,
    execution_id: String,
    released: bool,
}

impl CancelRegistration {
    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.sockets.remove(&self.execution_id);
    }
}

impl Drop for CancelRegistration {
    fn drop(&mut self) {
        self.release();
    }
}

/// Connection-level Official auth posture. Tokens stay on the plan; the key
/// only compares the kind of credential that opened the socket, so a token
/// refresh never spuriously forces a socket reopen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OfficialWebSocketAuthPosture {
    None,
    PreserveIncoming,
    Managed { account_id: Option<String> },
    Bearer { credential_id: Option<String> },
}

/// Reuse key for one Official WebSocket segment: upstream URL plus auth
/// posture. The upstream model and route id are per-turn and are
/// deliberately excluded, so Official model A?�B on an account-compatible
/// route reuses the same socket (plan §"WebSocket per-turn routing").
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfficialWebSocketConnectionKey {
    pub upstream_url: String,
    pub auth_posture: OfficialWebSocketAuthPosture,
}

/// One WebSocket turn resolved against a fresh catalog snapshot. Every
/// `response.create` gets its own immutable plan; there is no
/// connection-level fallback to whatever the previous turn resolved to.
///
/// Exactly one `WebSocketTurnDispatch` exists at a time (the plan for the
/// turn currently being dispatched); it is never stored in a `Vec` or other
/// collection, so `OfficialWebSocketPlan`'s size does not multiply the way
/// clippy's `large_enum_variant` lint is guarding against.
#[allow(clippy::large_enum_variant)]
pub(crate) enum WebSocketTurnDispatch {
    OfficialNative(OfficialWebSocketPlan),
    PortableReplay { force_official_handoff: bool },
}

pub(crate) struct WebSocketTurnPlan {
    pub route_id: String,
    pub dispatch: WebSocketTurnDispatch,
}

/// Immutable routing/authentication state for one native Official Responses
/// WebSocket turn. The lifecycle guard deliberately lives with the plan so
/// proxy draining cannot tear down a tunnel that is still carrying Codex
/// frames. `connection_key` is what a caller compares to decide whether this
/// turn may reuse an already-open segment; `upstream_model`/`route_id` are
/// per-turn and never gate reuse on their own.
pub(crate) struct OfficialWebSocketPlan {
    pub connection_key: OfficialWebSocketConnectionKey,
    pub upstream_url: String,
    pub upstream_model: String,
    pub route_id: String,
    pub provider_name: String,
    pub auth_headers: Vec<(String, String)>,
    pub preserve_incoming_auth: bool,
    pub control_account_hash: Option<String>,
    pub execution_account_hash: Option<String>,
    pub selection_revision: Option<u64>,
    pub continuation_realm: Option<String>,
    managed_auth: Option<OfficialAuthorization>,
    _guard: RequestGuard,
}

impl ProxyRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        catalog: Arc<dyn RouteCatalog>,
        credentials: Arc<dyn CredentialProvider>,
        official_auth: Arc<dyn OfficialAuthProvider>,
        lifecycle: Arc<dyn RequestLifecycle>,
        transport: Arc<dyn UpstreamTransport>,
    ) -> Self {
        Self {
            catalog,
            credentials,
            official_auth,
            lifecycle,
            transport,
            history: Arc::new(MemoryHistoryStore::new()),
            grok_sessions: Arc::new(GrokSessionRegistry::new()),
            opencode_quota: Arc::new(crate::opencode::OpenCodeQuotaCooldown::new()),
            usage: Arc::new(MemoryUsageStore::new()),
            search_engine: Arc::new(DisabledSearchEngine),
            web_search_wrapper_enabled: false,
            search_tool_loop_limit: crate::search_loop::DEFAULT_SEARCH_TOOL_LOOP_LIMIT,
            review_source: default_review_settings_source(),
            harness_options: HarnessOptions::default(),
            delegation_runtime_wired: false,
            task_stall_policy: TaskStallPolicy::default(),
            task_efficiency_policy: TaskEfficiencyPolicy::default(),
            task_stall_bounded_finalization: false,
            diagnostic_config_hash: "unavailable".into(),
            resource: ResourcePolicy::production(),
            diagnostics: Arc::new(NoopSink),
            subagent_spawns: Arc::new(Mutex::new(Vec::new())),
            subagent_graph: Arc::new(Mutex::new(
                crate::subagent_graph::SubagentGraphRegistry::new(),
            )),
            shadow_subagent_graph: Arc::new(Mutex::new(
                crate::subagent_graph::SubagentGraphRegistry::new(),
            )),
            subagent_identity_mode: SubagentIdentityMode::default(),
            parent_cancel: Arc::new(Mutex::new(ParentCancelRegistry::default())),
            generation_stop: watch::channel(false).0,
            efficiency_recoveries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Set the subagent identity and graph resolution rollout mode.
    pub fn with_subagent_identity_mode(mut self, mode: SubagentIdentityMode) -> Self {
        self.subagent_identity_mode = mode;
        self
    }

    /// Access the subagent graph registry.
    pub fn subagent_graph(&self) -> Arc<Mutex<crate::subagent_graph::SubagentGraphRegistry>> {
        Arc::clone(&self.subagent_graph)
    }

    /// Access the shadow subagent graph registry (telemetry / shadow mode only).
    pub fn shadow_subagent_graph(
        &self,
    ) -> Arc<Mutex<crate::subagent_graph::SubagentGraphRegistry>> {
        Arc::clone(&self.shadow_subagent_graph)
    }

    /// Replace the production resource policy. Config may only *lower*
    /// the defaults; [`ResourcePolicy::with_limits`] clamps anything higher.
    pub fn with_resource_policy(mut self, resource: ResourcePolicy) -> Self {
        self.resource = resource;
        self
    }

    pub fn resource_policy(&self) -> ResourcePolicy {
        self.resource.clone()
    }

    /// Replace the durable continuation history backing (tests, daemon
    /// wiring). Defaults to an in-memory store; the parity fixtures and the
    /// restart-continuation test inject their own.
    pub fn with_history_store(mut self, history: Arc<dyn HistoryStore>) -> Self {
        self.history = history;
        self
    }

    /// Replace the usage accounting backing (daemon wiring). Defaults to an
    /// in-memory store.
    pub fn with_usage_store(mut self, usage: Arc<dyn UsageStore>) -> Self {
        self.usage = usage;
        self
    }

    /// Attach the public configuration identity used by structured request
    /// diagnostics. This is a hash/reference only and never contains secret
    /// configuration values.
    pub fn with_diagnostic_config_hash(mut self, config_hash: impl Into<String>) -> Self {
        self.diagnostic_config_hash = config_hash.into();
        self
    }

    pub fn active_request_count(&self) -> usize {
        self.lifecycle.active_count()
    }

    pub fn diagnostic_config_hash(&self) -> &str {
        &self.diagnostic_config_hash
    }

    /// The single atomic terminal-outcome writer. Every Responses, Chat,
    /// Official (HTTP and native WebSocket) and portable-WS path must reach
    /// its terminal usage row through here -- never through
    /// `self.usage.record()` directly -- so "exactly one terminal row per
    /// execution" (INV-CANCEL-1) holds globally rather than only among the
    /// call sites that happened to remember a local guard.
    ///
    /// `execution_id` is the internal, freshly generated key from
    /// [`Self::register_request_cancel`] (never the external `request_id`,
    /// which a WebSocket client may reuse across turns on one connection).
    /// The first caller to claim `execution_id` wins: a success that lands
    /// after a cancel/disconnect already claimed it is silently dropped, and
    /// vice versa. On a win, the cancel socket for `execution_id` is released
    /// in the same locked step, so a normal completion no longer leaks a
    /// `sockets` entry. Returns whether this call was the one that won.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_terminal_once(
        &self,
        execution_id: &str,
        route_id: &str,
        provider: &str,
        model: &str,
        request_id: &str,
        connection_id: Option<&str>,
        duration_ms: u64,
        first_byte_ms: Option<u64>,
        first_event_ms: Option<u64>,
        first_downstream_frame_ms: Option<u64>,
        outcome: &str,
        error_category: Option<&str>,
        error: Option<&str>,
        stage_times_ms: Option<Value>,
    ) -> bool {
        if !self.claim_terminal_once(execution_id, outcome) {
            return false;
        }
        let status = match (outcome, error_category) {
            ("success", _) => 200,
            ("client_cancel" | "client_disconnect" | "user_stopped_proxy", _) => 499,
            ("protocol_failure", Some("invalid_request")) => 400,
            _ => 502,
        };
        let record = UsageRecord {
            route_id: route_id.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            status,
            error: error.map(str::to_string),
            duration_ms,
            first_byte_ms,
            first_event_ms,
            first_downstream_frame_ms,
            request_id: Some(request_id.to_string()),
            connection_id: connection_id.map(str::to_string),
            outcome: Some(outcome.to_string()),
            error_category: error_category.map(str::to_string),
            stage_times_ms,
            agent_attribution: self.agent_attribution_for_execution(execution_id),
            ..Default::default()
        };
        let result = self.usage.record(&record);
        if let Err(message) = result {
            // The terminal claim is deliberately kept (INV-CANCEL-1): a lost
            // write must not let a late duplicate resurrect this turn. The
            // rejection is surfaced as a bounded structured diagnostic so
            // usage loss is never silent, without rolling the claim back.
            self.record_usage_write_failure(
                execution_id,
                outcome,
                Some(request_id.to_string()),
                route_id,
                &message,
            );
        }
        true
    }

    fn agent_attribution_for_execution(
        &self,
        execution_id: &str,
    ) -> Option<crate::usage::AgentUsageAttribution> {
        let target_graph = if self.subagent_identity_mode == SubagentIdentityMode::Shadow {
            &self.shadow_subagent_graph
        } else {
            &self.subagent_graph
        };
        let graph = target_graph.lock().unwrap_or_else(|p| p.into_inner());
        let binding = graph.execution_binding(execution_id)?;
        let thread_key = &binding.thread;
        let node = graph.get_node(thread_key);
        let depth = graph.node_depth(thread_key, 32);

        let parent_thread_id = node
            .as_ref()
            .and_then(|n| n.parent.as_ref())
            .map(|p| p.thread_id.as_str().to_string());
        let parent_turn_id = node
            .as_ref()
            .and_then(|n| n.parent_turn_id.as_ref())
            .map(|t| t.as_str().to_string());
        let root_turn_id = node
            .as_ref()
            .and_then(|n| n.root_turn_id.as_ref())
            .map(|t| t.as_str().to_string());
        let context_window_id = binding
            .context_window_id
            .as_ref()
            .map(|cw| cw.as_str().to_string())
            .or_else(|| {
                node.as_ref()
                    .and_then(|n| n.latest_context_window_id.as_ref())
                    .map(|cw| cw.as_str().to_string())
            });
        let agent_name = node.as_ref().and_then(|n| n.agent_name.clone());
        let subagent_kind = node.as_ref().and_then(|n| n.subagent_kind.clone());

        Some(crate::usage::AgentUsageAttribution {
            session_id: Some(thread_key.session_id.as_str().to_string()),
            thread_id: Some(thread_key.thread_id.as_str().to_string()),
            parent_thread_id,
            parent_turn_id,
            root_turn_id,
            context_window_id,
            agent_name,
            subagent_kind,
            depth,
            link_method: if node.as_ref().and_then(|n| n.parent.as_ref()).is_some() {
                Some(crate::diagnostics::SubagentLinkMethod::OfficialThreadMetadata)
            } else {
                None
            },
            link_confidence: if node.as_ref().and_then(|n| n.parent.as_ref()).is_some() {
                Some(crate::diagnostics::LinkConfidence::High)
            } else {
                None
            },
        })
    }

    /// Same atomic guarantee as [`Self::finish_terminal_once`], for call
    /// sites that need to write a richer [`UsageRecord`] than that helper's
    /// fixed field set covers (token counts, review attribution, conversation
    /// identity, stream-quality telemetry). The caller builds the full
    /// record, including `status`/`outcome`; this only decides whether that
    /// record is the one that gets written.
    pub(crate) fn finish_terminal_once_with_record(
        &self,
        execution_id: &str,
        outcome: &str,
        mut record: UsageRecord,
    ) -> bool {
        if !self.claim_terminal_once(execution_id, outcome) {
            return false;
        }
        if record.agent_attribution.is_none() {
            record.agent_attribution = self.agent_attribution_for_execution(execution_id);
        }
        if let Err(message) = self.usage.record(&record) {
            // Same policy as [`Self::finish_terminal_once`]: keep the
            // first-writer terminal claim, never roll it back, but surface
            // the lost write as a bounded structured diagnostic.
            self.record_usage_write_failure(
                execution_id,
                outcome,
                record.request_id.clone(),
                &record.route_id,
                &message,
            );
        }
        true
    }

    /// Attach the fire-and-forget diagnostic sink. Defaults to [`NoopSink`];
    /// the Desktop daemon writes structured events to `usage.db` and the
    /// headless daemon may choose a JSONL sink instead. The sink must never
    /// panic or affect request outcomes.
    pub fn with_diagnostics_sink(mut self, diagnostics: Arc<dyn DiagnosticsSink>) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    /// Replace the search engine backing (daemon wiring). Defaults to the
    /// fail-closed Disabled engine.
    pub fn with_search_engine(mut self, engine: Arc<dyn SearchEngine>) -> Self {
        self.search_engine = engine;
        self
    }

    /// Whether the local `web_search` compatibility wrapper should be offered
    /// to third-party routes (M8 real fix, replacing the old
    /// always-`false`/always-stripped placeholder). The caller resolves this
    /// the same way it decided what to build [`Self::with_search_engine`]
    /// from ??"search enabled AND a usable backend key configured" ??so the
    /// tool surface and the engine that actually answers a call can never
    /// disagree. Defaults to `false` (fail closed).
    pub fn with_web_search_wrapper_enabled(mut self, enabled: bool) -> Self {
        self.web_search_wrapper_enabled = enabled;
        self
    }

    pub fn web_search_wrapper_enabled(&self) -> bool {
        self.web_search_wrapper_enabled
    }

    /// Override the per-turn cap on server-side `web_search` hops (see
    /// [`crate::search_loop::SearchLoopSession`]). `0` is treated as `1` (a
    /// turn can always attempt one search) so this can never silently
    /// disable the loop entirely.
    pub fn with_search_tool_loop_limit(mut self, limit: u8) -> Self {
        self.search_tool_loop_limit = limit.max(1);
        self
    }

    pub fn search_tool_loop_limit(&self) -> u8 {
        self.search_tool_loop_limit
    }

    /// Configure a fixed, never-changing Auto Review / Guardian policy
    /// (headless daemon wiring: config is loaded once at process start and
    /// there is no live settings store to poll). For a host that has one —
    /// Desktop's `AppState` — use [`Self::with_review_settings_source`]
    /// instead so a policy change takes effect on the very next request
    /// instead of requiring a proxy restart.
    pub fn with_review_config(mut self, review: ReviewSettings) -> Self {
        self.review_source = Arc::new(StaticReviewSettingsSource(review));
        self
    }

    /// Configure a live Auto Review / Guardian policy source. Every new
    /// request calls [`ReviewSettingsSource::current`] once, at snapshot
    /// capture time, so this proxy always dispatches Guardian requests
    /// against whatever policy is authoritative right now — never a value
    /// frozen at construction.
    pub fn with_review_settings_source(mut self, source: Arc<dyn ReviewSettingsSource>) -> Self {
        self.review_source = source;
        self
    }

    /// Aggregated usage for one route (M7 parity accessor).
    pub fn usage_summary(&self, route_id: &str) -> Result<crate::usage::UsageSummary, String> {
        self.usage.summary(route_id)
    }

    /// Exact model-visible catalog metadata captured by the route authority.
    /// The `/v1/models` boundary uses this to serve modern Codex's native
    /// `models` catalog alongside the OpenAI-compatible `data` list.
    pub fn model_catalog_entry(&self, catalog_id: &str) -> Option<Value> {
        self.catalog.catalog_entry(catalog_id)
    }

    pub(crate) fn mark_first_downstream_frame(&self, request_id: &str, elapsed_ms: u64) {
        let _ = self
            .usage
            .mark_first_downstream_frame(request_id, elapsed_ms);
    }

    pub(crate) fn resolved_usage_identity(&self, catalog_id: &str) -> (String, String, String) {
        match self.catalog.resolve_model(catalog_id) {
            Some(resolved) => (
                resolved.route.route_id,
                resolved.route.provider_kind.as_str().to_string(),
                resolved.route.upstream_model,
            ),
            None => (
                catalog_id.to_string(),
                "unknown".into(),
                catalog_id.to_string(),
            ),
        }
    }

    pub fn usage_records(&self) -> Result<Vec<UsageRecord>, String> {
        self.usage.records()
    }

    /// Resolve one durable owner from the normalized request metadata. The
    /// previous response lookup is performed before the generic resolver so
    /// a restart/resume preserves an already-recorded owner rather than
    /// manufacturing a new chain key.
    pub(crate) fn resolve_request_conversation_key(
        &self,
        request: &RuntimeRequest,
    ) -> ConversationKeyResolution {
        let existing = conversation_key_from_request(&request.body).or_else(|| {
            request
                .body
                .get("previous_response_id")
                .and_then(Value::as_str)
                .and_then(|id| self.history.response_conversation_key(id).ok().flatten())
        });
        resolve_conversation_key(request, existing.as_deref())
    }

    /// Return chronological provider usage for one durable conversation.
    /// Auxiliary compaction/summarizer calls intentionally have no
    /// conversation identity and therefore cannot inflate task-efficiency
    /// counters. Failed attempts that consumed tokens are retained, but are
    /// placed after model-visible request boundaries because they did not
    /// create a response item in the portable transcript.
    fn completed_request_usages_for_conversation(
        &self,
        conversation_key: &str,
    ) -> Vec<crate::task_efficiency::CompletedRequestUsage> {
        self.usage
            .records()
            .unwrap_or_default()
            .into_iter()
            .filter(|record| {
                record.conversation_identity.as_deref() == Some(conversation_key)
                    && (record.input_tokens > 0 || record.output_tokens > 0)
            })
            .enumerate()
            .map(|(index, record)| {
                let request_index = record.request_id.as_deref().and_then(eval_request_index);
                let successful_boundary = record.status < 400
                    && record.error.is_none()
                    && record.outcome.as_deref().unwrap_or("success") == "success";
                crate::task_efficiency::CompletedRequestUsage {
                    usage_seq: (index + 1) as u64,
                    // A WebSocket client may legally reuse its request id
                    // across turns. Each terminal ledger row is already
                    // exactly-once; the monotonic usage sequence is the
                    // durable dedupe authority here.
                    request_id: None,
                    response_id: None,
                    input_tokens: record.input_tokens,
                    cached_input_tokens: record.cached_input_tokens,
                    output_tokens: record.output_tokens,
                    request_index,
                    source_index: (!successful_boundary).then_some(usize::MAX),
                }
            })
            .collect()
    }

    /// Record a diagnostic event without ever surfacing a sink failure to the
    /// request path (INV-D-001). A misbehaving sink is downgraded to a
    /// `log::warn!` and otherwise ignored.
    pub(crate) fn record_diagnostic(&self, event: DiagnosticEvent) {
        if let Err(message) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.diagnostics.record(event);
        })) {
            let payload = if let Some(text) = message.downcast_ref::<&str>() {
                (*text).to_string()
            } else if let Some(text) = message.downcast_ref::<String>() {
                text.clone()
            } else {
                "unknown panic".to_string()
            };
            log::warn!("diagnostic sink panicked: {payload}");
        }
    }

    /// Surface a terminal usage-write rejection as one bounded structured
    /// diagnostic (see [`record_usage_write_failure_diagnostic`]).
    fn record_usage_write_failure(
        &self,
        execution_id: &str,
        outcome: &str,
        request_id: Option<String>,
        route_id: &str,
        message: &str,
    ) {
        record_usage_write_failure_diagnostic(
            &self.diagnostics,
            execution_id,
            outcome,
            request_id,
            route_id,
            message,
        );
    }

    /// The currently effective content detail level, consulted *before*
    /// expensive capture work is performed.
    pub(crate) fn detail_level(&self) -> DetailLevel {
        self.diagnostics.detail_level()
    }

    /// Native Official WebSocket variant of Point B. It uses the route facts
    /// frozen into the connection plan rather than re-resolving the live
    /// catalog while frames are in flight. `execution_id` is this child
    /// turn's own cancel-registry key (see [`Self::register_request_cancel`]);
    /// returns `true` when the child linked to a parent that was already
    /// cancelled, meaning the caller must abort before dispatching upstream.
    pub(crate) fn record_official_websocket_child_turn(
        &self,
        plan: &OfficialWebSocketPlan,
        request: &RuntimeRequest,
        execution_id: &str,
    ) -> bool {
        self.record_subagent_child_turn(
            request,
            &plan.route_id,
            &plan.upstream_model,
            &plan.upstream_model,
            &request.body,
            execution_id,
        )
    }

    /// Test-only: resolve the internal execution id a pending spawn's parent
    /// turn was assigned, by that parent's external `request_id`. Black-box
    /// integration tests (real HTTP/WS round trips) have no other way to
    /// name the cancel-registry key for a turn they only know by its
    /// `x-request-id` -- by design, since that external id is never trusted
    /// as the registry key in production (a WebSocket client can repeat it
    /// across turns). Only meaningful while the spawn is still pending (Point
    /// C, a `function_call_output` for the same call id, removes it).
    #[cfg(test)]
    pub(crate) fn debug_pending_spawn_parent_execution_id(
        &self,
        parent_request_id: &str,
    ) -> Option<String> {
        let spawns = self
            .subagent_spawns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        spawns
            .iter()
            .find(|spawn| spawn.parent_request_id == parent_request_id)
            .map(|spawn| spawn.parent_execution_id.clone())
    }

    /// Point A: capture a parent response's native spawn tool calls. The
    /// prompt identity is retained immediately so a later child request can be
    /// linked even when Codex sends it as an ordinary `/responses` turn.
    /// `parent_request_id` is the external id shown in diagnostics;
    /// `parent_execution_id` is the internal cancel-registry key a later
    /// child link fans a cancel through -- kept separate because a WebSocket
    /// client can reuse the same `x-request-id` across turns.
    pub(crate) fn record_subagent_spawn_requests(
        &self,
        parent_request_id: &str,
        parent_execution_id: &str,
        response: &Value,
    ) {
        record_subagent_spawn_requests_in_sink(
            Arc::clone(&self.diagnostics),
            Arc::clone(&self.subagent_spawns),
            parent_request_id,
            parent_execution_id,
            response,
        );
    }

    /// Point C: a `function_call_output` with the same call id closes the
    /// spawn. This is the only parent/child edge Vellum can claim with high
    /// confidence without consulting Codex's own rollout tree.
    pub(crate) fn record_subagent_completions(&self, request: &Value) {
        let outputs = request
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut completed = Vec::new();
        {
            let mut spawns = self
                .subagent_spawns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            spawns.retain(|spawn| spawn.started.elapsed() < SUBAGENT_LINK_WINDOW);
            for item in &outputs {
                if item.get("type").and_then(Value::as_str) != Some("function_call_output") {
                    continue;
                }
                let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(index) = spawns.iter().position(|spawn| spawn.call_id == call_id) {
                    completed.push(spawns.remove(index));
                }
            }
        }
        for mut spawn in completed {
            let output = outputs
                .iter()
                .find(|item| {
                    item.get("type").and_then(Value::as_str) == Some("function_call_output")
                        && item.get("call_id").and_then(Value::as_str)
                            == Some(spawn.call_id.as_str())
                })
                .and_then(|item| item.get("output"))
                .map(|output| {
                    output
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| output.to_string())
                })
                .unwrap_or_default();
            let output_hash = hash_text(&output);
            if !spawn.saw_parallel_spawn && spawn.time_window_candidates.len() == 1 {
                let mut candidate = spawn.time_window_candidates.remove(0);
                candidate.call_id = Some(spawn.call_id.clone());
                candidate.parent_request_id = Some(spawn.parent_request_id.clone());
                candidate.link_method = SubagentLinkMethod::TimeWindow;
                candidate.link_confidence = LinkConfidence::Low;
                self.record_diagnostic(DiagnosticEvent::ChildTurn(candidate));
            }
            // Point B only ever pushes to `linked_request_ids` on the first
            // confident (high/medium-confidence) match for a spawn, so at
            // most one entry exists here ??the ambiguous/low-confidence path
            // never populates it (INV-D-005). This is deliberately the same
            // "confident, singular" bar as the ChildTurn link itself: a
            // downstream aggregator may treat this as a hard edge, never a
            // guess.
            let child_request_id = spawn.linked_request_ids.first().cloned();
            self.record_diagnostic(DiagnosticEvent::SpawnCompleted(SpawnCompleted {
                call_id: spawn.call_id,
                output: redact_sensitive_text(&bounded_text(&output, 8_192)),
                output_len: output.len() as u64,
                output_hash: Some(output_hash),
                duration_ms: spawn.started.elapsed().as_millis() as u64,
                child_request_id,
            }));
        }
    }

    /// Register a one-shot cancel channel under `execution_id` -- an id the
    /// caller minted with [`new_execution_id`] immediately before dispatch,
    /// never the external `request_id` (a WebSocket client can reuse the same
    /// `x-request-id` across several turns on one connection, so it must
    /// never be trusted as a unique registry key). Callers mint their own id
    /// up front, rather than receiving one back from this call, specifically
    /// so a caller that races the dispatch future against an inbound
    /// `response.cancel` (the WebSocket portable-turn and Official-native
    /// loops) already has the key in scope to cancel *while dispatch is still
    /// in flight*.
    ///
    /// The returned [`CancelRegistration`] releases the socket on drop -- on
    /// any exit path, including ones that forget to call
    /// [`Self::finish_terminal_once`] -- and the receiver resolves when this
    /// execution's tree is cancelled: the execution itself, or (for a child)
    /// its confidently linked parent.
    pub(crate) fn register_request_cancel(
        &self,
        execution_id: &str,
    ) -> (CancelRegistration, watch::Receiver<bool>) {
        // A cancel that landed *before* this registration (e.g. a parent tree
        // cancel that raced the turn's own bind/send ordering) must be visible
        // to the freshly created receiver immediately. A `watch::channel(false)`
        // would start from `false` and hide the already-cancelled state, so the
        // child would keep executing with no proactive wakeup (P0: Official
        // registration raced parent cancellation).
        let mut registry = self
            .parent_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.prune();
        let already_cancelled = registry.cancelled.contains_key(execution_id);
        let (sender, receiver) = watch::channel(already_cancelled);
        registry.sockets.insert(
            execution_id.to_string(),
            TimestampedSender {
                sender,
                created: tokio::time::Instant::now(),
            },
        );
        #[cfg(test)]
        {
            registry.last_registered_execution_id = Some(execution_id.to_string());
        }
        drop(registry);
        (
            CancelRegistration {
                registry: Arc::clone(&self.parent_cancel),
                execution_id: execution_id.to_string(),
                released: false,
            },
            receiver,
        )
    }

    /// Release an execution's cancel socket once it reaches a terminal state.
    /// Prefer letting the [`CancelRegistration`] guard drop; this remains for
    /// call sites that only hold the execution id (e.g. after the guard has
    /// already been moved into a spawned task under a different name).
    pub(crate) fn unregister_request_cancel(&self, execution_id: &str) {
        let mut registry = self
            .parent_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.sockets.remove(execution_id);
    }

    /// Whether `execution_id` is inside an already-cancelled tree. Used to
    /// give a child that was dropped by parent fan-out its own bounded 499
    /// row.
    pub fn is_request_cancelled(&self, execution_id: &str) -> bool {
        let registry = self
            .parent_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.cancelled.contains_key(execution_id)
    }

    /// Test-only snapshot of every registry map's current length. The prune
    /// tests assert on these directly rather than inferring from indirect
    /// behavior which map (if any) was evicted.
    #[cfg(test)]
    pub(crate) fn registry_len(&self) -> RegistryLenSnapshot {
        let registry = self
            .parent_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        RegistryLenSnapshot {
            sockets: registry.sockets.len(),
            parents: registry.parents.len(),
            cancelled: registry.cancelled.len(),
            terminal: registry.terminal.len(),
        }
    }

    /// Test-only: the most recently registered execution id (see
    /// [`ParentCancelRegistry::last_registered_execution_id`]). Lets
    /// black-box tests that dispatch through the real HTTP/WebSocket surface
    /// target a specific in-flight turn with `cancel_request`/
    /// `cancel_request_tree` without the production code ever exposing the
    /// internal id over the wire.
    #[cfg(test)]
    pub(crate) fn debug_last_registered_execution_id(&self) -> Option<String> {
        let registry = self
            .parent_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.last_registered_execution_id.clone()
    }

    /// Bind a child execution to its confidently linked parent execution.
    /// Returns `true` when the parent is already cancelled, which means the
    /// child must abort before it ever reaches upstream.
    pub(crate) fn bind_child_to_parent(
        &self,
        parent_execution_id: &str,
        child_execution_id: &str,
    ) -> bool {
        let mut registry = self
            .parent_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.prune();
        let parent_cancelled = registry.cancelled.contains_key(parent_execution_id);
        if let Some(parent) = registry.parents.get_mut(parent_execution_id) {
            parent.children.push(child_execution_id.to_string());
            parent.cancelled |= parent_cancelled;
        } else {
            registry.parents.insert(
                parent_execution_id.to_string(),
                ParentCancelEntry {
                    cancelled: parent_cancelled,
                    created: tokio::time::Instant::now(),
                    children: vec![child_execution_id.to_string()],
                },
            );
        }
        parent_cancelled
    }

    /// Directly cancel one execution: mark it and fire its own channel. Used
    /// for a child that linked to an already-cancelled parent so its empty
    /// response is attributed the same way a real cancel would be.
    pub fn cancel_request(&self, execution_id: &str) {
        let mut registry = self
            .parent_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.prune();
        registry
            .cancelled
            .insert(execution_id.to_string(), tokio::time::Instant::now());
        if let Some(entry) = registry.sockets.get(execution_id) {
            let _ = entry.sender.send(true);
        }
    }

    /// Cancel a parent execution and fan out to every confidently bound
    /// child: each child is marked cancelled and its in-flight upstream
    /// stream is dropped via its registered channel. Returns the bound child
    /// execution ids for diagnostics. Idempotent: a second cancel of the same
    /// parent no-ops the fan-out (children of already-cancelled trees are
    /// only marked once).
    pub fn cancel_request_tree(&self, parent_execution_id: &str) -> Vec<String> {
        let mut registry = self
            .parent_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.prune();

        if registry.cancelled.contains_key(parent_execution_id) {
            // Already cancelled: children were fanned out the first time.
            return registry
                .parents
                .get(parent_execution_id)
                .map(|parent| parent.children.clone())
                .unwrap_or_default();
        }
        registry
            .cancelled
            .insert(parent_execution_id.to_string(), tokio::time::Instant::now());
        if let Some(entry) = registry.sockets.get(parent_execution_id) {
            let _ = entry.sender.send(true);
        }

        // Execution graph traversal (BFS) using ParentCancelRegistry
        let mut cancelled_children = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let mut visited = std::collections::HashSet::new();
        visited.insert(parent_execution_id.to_string());

        if let Some(entry) = registry.parents.get_mut(parent_execution_id) {
            entry.cancelled = true;
            for child in &entry.children {
                if visited.insert(child.clone()) {
                    queue.push_back(child.clone());
                    cancelled_children.push(child.clone());
                }
            }
        }

        while let Some(current_exec) = queue.pop_front() {
            if let Some(entry) = registry.parents.get_mut(&current_exec) {
                entry.cancelled = true;
                for child in &entry.children {
                    if visited.insert(child.clone()) {
                        queue.push_back(child.clone());
                        cancelled_children.push(child.clone());
                    }
                }
            }
        }

        for child in &cancelled_children {
            registry
                .cancelled
                .insert(child.clone(), tokio::time::Instant::now());
            if let Some(entry) = registry.sockets.get(child) {
                let _ = entry.sender.send(true);
                tracing::info!(
                    parent_execution_id,
                    child_execution_id = %child,
                    "subagent child cancelled by execution graph cancellation"
                );
            }
        }
        cancelled_children
    }

    /// Cancel every in-flight HTTP / WebSocket / stream execution for this
    /// proxy generation. Records a first-writer-wins terminal of
    /// `user_stopped_proxy` so the interruption cannot later be claimed as
    /// success or as a generic provider error.
    pub async fn stopped(&self) {
        let mut rx = self.generation_stop.subscribe();
        if *rx.borrow() {
            return;
        }
        let _ = rx.changed().await;
    }

    pub fn cancel_all_in_flight_for_stop(&self) -> Vec<String> {
        self.generation_stop.send_replace(true);
        let now = tokio::time::Instant::now();
        let mut registry = self
            .parent_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.prune();
        let ids: Vec<String> = registry.sockets.keys().cloned().collect();
        for id in &ids {
            registry.cancelled.insert(id.clone(), now);
            if let Some(entry) = registry.sockets.get(id) {
                let _ = entry.sender.send(true);
            }
        }
        drop(registry);
        for id in &ids {
            let _ = self.finish_terminal_once(
                id,
                "",
                "",
                "",
                id,
                None,
                0,
                None,
                None,
                None,
                "user_stopped_proxy",
                Some("proxy_stopped"),
                Some("user stopped proxy"),
                None,
            );
        }
        ids
    }

    /// First-writer-wins terminal outcome guard for one execution id. Returns
    /// `false` when the execution already reached a terminal outcome, so a
    /// late duplicate terminal (e.g. a child "completed" racing a cancel) can
    /// never overwrite the outcome that already landed (INV-CANCEL-1). On a
    /// win, also releases the execution's cancel socket in the same locked
    /// step -- the reason a normal (uncancelled) completion no longer leaks a
    /// `sockets` entry.
    fn claim_terminal_once(&self, execution_id: &str, outcome: &str) -> bool {
        let won = claim_terminal_once_raw(&self.parent_cancel, execution_id, outcome);
        if won {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            match self.subagent_identity_mode {
                SubagentIdentityMode::Shadow => {
                    if let Ok(mut shadow) = self.shadow_subagent_graph.lock() {
                        shadow.mark_execution_terminal(execution_id, now);
                        shadow.prune(
                            now,
                            crate::subagent_graph::DEFAULT_GRAPH_TTL_MS,
                            crate::subagent_graph::DEFAULT_MAX_GRAPH_NODES,
                        );
                    }
                }
                _ => {
                    if let Ok(mut graph) = self.subagent_graph.lock() {
                        graph.mark_execution_terminal(execution_id, now);
                        graph.prune(
                            now,
                            crate::subagent_graph::DEFAULT_GRAPH_TTL_MS,
                            crate::subagent_graph::DEFAULT_MAX_GRAPH_NODES,
                        );
                    }
                }
            }
        }
        won
    }

    /// Observe incoming Codex turn metadata and record into the subagent graph.
    pub(crate) fn observe_codex_turn(
        &self,
        request: &RuntimeRequest,
        execution_id: &str,
        route_id: &str,
        upstream_model: &str,
    ) -> crate::subagent_graph::GraphObservation {
        let observation = crate::subagent_graph::TurnObservation {
            execution_id: execution_id.to_string(),
            request_id: request.metadata.request_id.clone(),
            connection_id: request.metadata.connection_id.clone(),
            route_id: route_id.to_string(),
            model: upstream_model.to_string(),
            observed_at_ms: request.metadata.received_at_ms,
        };

        if let Some(identity) = &request.metadata.codex_identity {
            if identity.trust == crate::codex_metadata::CodexIdentityTrust::Conflict {
                self.record_diagnostic(DiagnosticEvent::CodexMetadataConflict(
                    CodexMetadataConflictDiagnostic {
                        request_id: request.metadata.request_id.clone(),
                        execution_id: execution_id.to_string(),
                        fields: identity.conflict_fields.clone(),
                        structured_source_present: identity.source
                            == crate::codex_metadata::CodexIdentitySource::TurnMetadataHeader
                            || identity.source
                                == crate::codex_metadata::CodexIdentitySource::CanonicalClientMetadata,
                        compatibility_source_present: identity.subagent_header.is_some()
                            || identity.parent_thread_id.is_some(),
                    },
                ));
            }

            let target_graph = if self.subagent_identity_mode == SubagentIdentityMode::Shadow {
                &self.shadow_subagent_graph
            } else {
                &self.subagent_graph
            };

            let mut graph = target_graph.lock().unwrap_or_else(|p| p.into_inner());
            match graph.observe_turn(identity, observation) {
                Ok(obs) => {
                    if obs.edge_created {
                        if let (Some(p), Some(c)) = (&obs.parent, &obs.thread) {
                            self.record_diagnostic(DiagnosticEvent::SubagentGraphLinked(
                                SubagentGraphLinked {
                                    parent_thread_id: p.thread_id.as_str().to_string(),
                                    child_thread_id: c.thread_id.as_str().to_string(),
                                    parent_turn_id: identity
                                        .parent_turn_id
                                        .as_deref()
                                        .map(str::to_string),
                                    root_turn_id: identity
                                        .root_turn_id
                                        .as_deref()
                                        .map(str::to_string),
                                    method: obs.link_method,
                                    confidence: obs.confidence,
                                },
                            ));
                        }
                    }
                    obs
                }
                Err(e) => {
                    tracing::warn!("Failed to observe turn in subagent graph: {e}");
                    crate::subagent_graph::GraphObservation {
                        thread: None,
                        parent: None,
                        edge_created: false,
                        execution_bound: false,
                        link_method: crate::diagnostics::SubagentLinkMethod::None,
                        confidence: crate::diagnostics::LinkConfidence::None,
                    }
                }
            }
        } else {
            crate::subagent_graph::GraphObservation {
                thread: None,
                parent: None,
                edge_created: false,
                execution_bound: false,
                link_method: crate::diagnostics::SubagentLinkMethod::None,
                confidence: crate::diagnostics::LinkConfidence::None,
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_child_turn_diagnostic(
        &self,
        request: &RuntimeRequest,
        route_id: &str,
        catalog_id: &str,
        execution_body: &Value,
        identity: &crate::codex_metadata::CodexTurnIdentity,
        link_method: crate::diagnostics::SubagentLinkMethod,
        link_confidence: crate::diagnostics::LinkConfidence,
    ) {
        let manifest = execution_body
            .get("input")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(input_item_manifest)
                    .map(|(m, _)| m)
                    .collect::<Vec<InputItemManifest>>()
            })
            .unwrap_or_default();
        let instructions = execution_body.get("instructions").and_then(Value::as_str);
        let estimated_input_tokens = manifest
            .iter()
            .map(|item| item.estimated_tokens)
            .sum::<u64>()
            .saturating_add(instructions.map_or(0, |text| text.chars().count().div_ceil(4) as u64));
        let detail = self.detail_level();
        let full_context =
            (detail == DetailLevel::T3FullContext).then(|| redact_sensitive_json(execution_body));
        let (instructions_hash, instructions_len, instructions_prefix) =
            match (instructions, detail) {
                (Some(text), DetailLevel::T1Summary) => (
                    Some(hash_text(text)),
                    Some(text.chars().count() as u64),
                    Some(bounded_text(text, 512)),
                ),
                (Some(text), _) => (
                    Some(hash_text(text)),
                    Some(text.chars().count() as u64),
                    Some(text.to_string()),
                ),
                (None, _) => (None, None, None),
            };

        let event = ChildTurn {
            request_id: request.metadata.request_id.clone(),
            route_id: route_id.to_string(),
            call_id: None,
            parent_request_id: identity.parent_thread_id.as_deref().map(str::to_string),
            model: catalog_id.to_string(),
            effort: execution_body
                .pointer("/reasoning/effort")
                .and_then(Value::as_str)
                .map(str::to_string),
            link_method,
            link_confidence,
            instructions_hash,
            instructions_len,
            instructions_prefix: instructions_prefix.map(|text| redact_sensitive_text(&text)),
            input_item_count: execution_body
                .get("input")
                .and_then(Value::as_array)
                .map_or(0, |a| a.len() as u64),
            input_manifest: manifest,
            estimated_input_tokens,
            full_context,
            session_id: identity.session_id.as_ref().map(|id| id.to_string()),
            thread_id: identity.thread_id.as_ref().map(|id| id.to_string()),
            parent_thread_id: identity.parent_thread_id.as_ref().map(|id| id.to_string()),
            parent_turn_id: identity.parent_turn_id.as_ref().map(|id| id.to_string()),
            root_turn_id: identity.root_turn_id.as_ref().map(|id| id.to_string()),
            context_window_id: identity.context_window_id.as_ref().map(|id| id.to_string()),
            agent_name: identity.agent_name.clone(),
            subagent_kind: identity.subagent_kind.clone(),
            identity_source: Some(identity.source),
            identity_trust: Some(identity.trust),
            schema_fingerprint: identity.schema_fingerprint.clone(),
        };
        self.record_diagnostic(DiagnosticEvent::ChildTurn(event));
    }

    /// Point B: capture an in-flight child request.
    /// Prefers official structured metadata graph linkage, degrading to legacy heuristic matching
    /// according to the configured `SubagentIdentityMode`.
    pub(crate) fn record_subagent_child_turn(
        &self,
        request: &RuntimeRequest,
        route_id: &str,
        catalog_id: &str,
        upstream_model: &str,
        execution_body: &Value,
        execution_id: &str,
    ) -> bool {
        match self.subagent_identity_mode {
            SubagentIdentityMode::Legacy => {
                let (legacy_aborted, _, _) = self.record_subagent_child_turn_legacy(
                    request,
                    route_id,
                    catalog_id,
                    upstream_model,
                    execution_body,
                    execution_id,
                    true,
                );
                legacy_aborted
            }
            SubagentIdentityMode::Shadow => {
                // Shadow mode observes into the isolated shadow_subagent_graph
                let graph_obs =
                    self.observe_codex_turn(request, execution_id, route_id, upstream_model);

                // In shadow mode, legacy is 100% authoritative for execution behavior
                let (legacy_aborted, legacy_parent, legacy_conf) = self
                    .record_subagent_child_turn_legacy(
                        request,
                        route_id,
                        catalog_id,
                        upstream_model,
                        execution_body,
                        execution_id,
                        true,
                    );

                // Read-only comparison against shadow graph for telemetry
                let official_parent = request
                    .metadata
                    .codex_identity
                    .as_ref()
                    .and_then(|id| id.parent_thread_id.as_deref().map(str::to_string));

                let agree = match (&request.metadata.codex_identity, &legacy_parent) {
                    (Some(identity), Some(leg_req_id)) if identity.parent_thread_id.is_some() => {
                        let p = identity.parent_thread_id.as_ref().unwrap();
                        let shadow = self
                            .shadow_subagent_graph
                            .lock()
                            .unwrap_or_else(|p| p.into_inner());
                        if let Some(s) = &identity.session_id {
                            let parent_key =
                                crate::subagent_graph::CodexThreadKey::new(s.clone(), p.clone());
                            shadow.thread_has_request_id(&parent_key, leg_req_id)
                        } else {
                            false
                        }
                    }
                    (Some(identity), None) => identity.parent_thread_id.is_none(),
                    (None, None) => true,
                    _ => false,
                };

                self.record_diagnostic(DiagnosticEvent::SubagentLinkComparison(
                    SubagentLinkComparison {
                        official_parent,
                        legacy_parent_request_id: legacy_parent,
                        agree,
                        official_confidence: graph_obs.confidence,
                        legacy_confidence: legacy_conf,
                    },
                ));

                legacy_aborted
            }
            SubagentIdentityMode::OfficialPreferred => {
                let graph_obs =
                    self.observe_codex_turn(request, execution_id, route_id, upstream_model);
                let mut child_aborted_by_parent = false;
                let mut officially_linked = false;

                if graph_obs.confidence == LinkConfidence::High
                    || graph_obs.confidence == LinkConfidence::Medium
                {
                    if let Some(identity) = &request.metadata.codex_identity {
                        if let (Some(s_id), Some(p_id)) =
                            (&identity.session_id, &identity.parent_thread_id)
                        {
                            let parent_execs = {
                                let graph = self
                                    .subagent_graph
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner());
                                let (execs, _) = graph.resolve_parent_executions(
                                    s_id,
                                    p_id,
                                    identity.parent_turn_id.as_ref(),
                                );
                                execs
                            };
                            if !parent_execs.is_empty() {
                                officially_linked = true;
                                for p_exec in &parent_execs {
                                    if self.bind_child_to_parent(p_exec, execution_id) {
                                        self.cancel_request(execution_id);
                                        child_aborted_by_parent = true;
                                    }
                                }
                            }
                        }

                        if officially_linked {
                            self.emit_child_turn_diagnostic(
                                request,
                                route_id,
                                catalog_id,
                                execution_body,
                                identity,
                                graph_obs.link_method,
                                graph_obs.confidence,
                            );

                            return child_aborted_by_parent;
                        }
                    }
                }

                // Fallback to legacy heuristic path if official linkage was missing/unresolvable
                let (legacy_aborted, _, _) = self.record_subagent_child_turn_legacy(
                    request,
                    route_id,
                    catalog_id,
                    upstream_model,
                    execution_body,
                    execution_id,
                    true,
                );
                legacy_aborted
            }
            SubagentIdentityMode::OfficialOnly => {
                let graph_obs =
                    self.observe_codex_turn(request, execution_id, route_id, upstream_model);
                let mut child_aborted_by_parent = false;

                if graph_obs.confidence == LinkConfidence::High
                    || graph_obs.confidence == LinkConfidence::Medium
                {
                    if let Some(identity) = &request.metadata.codex_identity {
                        if let (Some(s_id), Some(p_id)) =
                            (&identity.session_id, &identity.parent_thread_id)
                        {
                            let parent_execs = {
                                let graph = self
                                    .subagent_graph
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner());
                                let (execs, _) = graph.resolve_parent_executions(
                                    s_id,
                                    p_id,
                                    identity.parent_turn_id.as_ref(),
                                );
                                execs
                            };
                            for p_exec in &parent_execs {
                                if self.bind_child_to_parent(p_exec, execution_id) {
                                    self.cancel_request(execution_id);
                                    child_aborted_by_parent = true;
                                }
                            }
                        }

                        self.emit_child_turn_diagnostic(
                            request,
                            route_id,
                            catalog_id,
                            execution_body,
                            identity,
                            graph_obs.link_method,
                            graph_obs.confidence,
                        );

                        return child_aborted_by_parent;
                    }
                }

                // OfficialOnly never falls back to legacy heuristics
                if let Some(identity) = &request.metadata.codex_identity {
                    self.emit_child_turn_diagnostic(
                        request,
                        route_id,
                        catalog_id,
                        execution_body,
                        identity,
                        crate::diagnostics::SubagentLinkMethod::None,
                        LinkConfidence::None,
                    );
                }
                false
            }
        }
    }

    /// Legacy heuristic implementation of child turn capture.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_subagent_child_turn_legacy(
        &self,
        request: &RuntimeRequest,
        route_id: &str,
        catalog_id: &str,
        upstream_model: &str,
        execution_body: &Value,
        execution_id: &str,
        emit_diagnostic: bool,
    ) -> (bool, Option<String>, LinkConfidence) {
        let mut spawns = self
            .subagent_spawns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        spawns.retain(|spawn| spawn.started.elapsed() < SUBAGENT_LINK_WINDOW);
        if spawns.is_empty() {
            return (false, None, LinkConfidence::None);
        }
        let instructions = execution_body
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::to_string);
        let input = execution_body
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let first_item_text = input.first().and_then(crate::diagnostics::input_item_text);
        let instruction_hash = instructions.as_deref().map(hash_text);
        let first_item_hash = first_item_text.as_deref().map(hash_text);

        let mut matches: Vec<usize> = spawns
            .iter()
            .enumerate()
            .filter(|(_, spawn)| {
                instruction_hash.as_deref() == Some(spawn.prompt_hash.as_str())
                    || first_item_hash.as_deref() == Some(spawn.prompt_hash.as_str())
            })
            .map(|(index, _)| index)
            .collect();
        let mut link_method = SubagentLinkMethod::ContentHash;
        let mut confidence = LinkConfidence::High;
        if matches.len() != 1 {
            matches = spawns
                .iter()
                .enumerate()
                .filter(|(_, spawn)| {
                    spawn
                        .model
                        .as_deref()
                        .is_some_and(|model| model == catalog_id || model == upstream_model)
                })
                .map(|(index, _)| index)
                .collect();
            link_method = SubagentLinkMethod::ConfiguredModel;
            confidence = LinkConfidence::Medium;
        }
        let mut deferred_time_window = false;
        let linked = if matches.len() == 1 {
            let spawn = &mut spawns[matches[0]];
            let first_link = spawn.linked_request_ids.is_empty();
            if first_link {
                spawn
                    .linked_request_ids
                    .push(request.metadata.request_id.clone());
                Some((
                    spawn.parent_request_id.clone(),
                    spawn.parent_execution_id.clone(),
                    spawn.call_id.clone(),
                ))
            } else {
                if matches!(confidence, LinkConfidence::High | LinkConfidence::Medium) {
                    Some((
                        spawn.parent_request_id.clone(),
                        spawn.parent_execution_id.clone(),
                        spawn.call_id.clone(),
                    ))
                } else {
                    None
                }
            }
        } else {
            link_method = SubagentLinkMethod::None;
            confidence = LinkConfidence::None;
            deferred_time_window = spawns.len() == 1
                && spawns[0].linked_request_ids.is_empty()
                && spawns[0].time_window_candidates.is_empty();
            None
        };

        let mut child_aborted_by_parent = false;
        let mut linked_parent_request_id = None;
        if let Some((parent_request_id, parent_execution_id, _call_id)) = linked.as_ref() {
            linked_parent_request_id = Some(parent_request_id.clone());
            if self.bind_child_to_parent(parent_execution_id, execution_id) {
                self.cancel_request(execution_id);
                child_aborted_by_parent = true;
            }
        }

        let manifest = input
            .iter()
            .map(input_item_manifest)
            .map(|(manifest, _)| manifest)
            .collect::<Vec<InputItemManifest>>();
        let estimated_input_tokens = manifest
            .iter()
            .map(|item| item.estimated_tokens)
            .sum::<u64>()
            .saturating_add(
                instructions
                    .as_deref()
                    .map_or(0, |text| text.chars().count().div_ceil(4) as u64),
            );
        let detail = self.detail_level();
        let full_context =
            (detail == DetailLevel::T3FullContext).then(|| redact_sensitive_json(execution_body));
        let (instructions_hash, instructions_len, instructions_prefix) =
            match (&instructions, detail) {
                (Some(text), DetailLevel::T1Summary) => (
                    Some(hash_text(text)),
                    Some(text.chars().count() as u64),
                    Some(bounded_text(text, 512)),
                ),
                (Some(text), _) => (
                    Some(hash_text(text)),
                    Some(text.chars().count() as u64),
                    Some(text.clone()),
                ),
                (None, _) => (None, None, None),
            };
        let event = ChildTurn {
            request_id: request.metadata.request_id.clone(),
            route_id: route_id.to_string(),
            call_id: linked.as_ref().map(|(_, _, call_id)| call_id.clone()),
            parent_request_id: linked.as_ref().map(|(parent, _, _)| parent.clone()),
            model: catalog_id.to_string(),
            effort: execution_body
                .pointer("/reasoning/effort")
                .and_then(Value::as_str)
                .map(str::to_string),
            link_method,
            link_confidence: confidence,
            instructions_hash,
            instructions_len,
            instructions_prefix: instructions_prefix.map(|text| redact_sensitive_text(&text)),
            input_item_count: input.len() as u64,
            input_manifest: manifest,
            estimated_input_tokens,
            full_context,
            session_id: None,
            thread_id: None,
            parent_thread_id: None,
            parent_turn_id: None,
            root_turn_id: None,
            context_window_id: None,
            agent_name: None,
            subagent_kind: None,
            identity_source: None,
            identity_trust: None,
            schema_fingerprint: None,
        };
        if deferred_time_window {
            spawns[0].time_window_candidates.push(event);
            return (false, None, LinkConfidence::None);
        }
        if emit_diagnostic {
            self.record_diagnostic(DiagnosticEvent::ChildTurn(event));
        }
        (
            child_aborted_by_parent,
            linked_parent_request_id,
            confidence,
        )
    }

    /// Configure the harness environment facts this host resolved (plan
    /// §25.1): host-proven options and whether a real delegation runtime is
    /// wired. Desktop's thin environment adapter supplies these; a remote
    /// daemon reports its own.
    pub fn with_harness_environment(
        mut self,
        options: HarnessOptions,
        delegation_runtime_wired: bool,
    ) -> Self {
        self.harness_options = options;
        self.delegation_runtime_wired = delegation_runtime_wired;
        self
    }

    /// Override behavioral recovery policies for an isolated evaluator.
    /// Production callers leave these at their environment-resolved defaults;
    /// evals use explicit values so a run cannot silently fall back to shadow.
    pub fn with_task_recovery_policies(
        mut self,
        stall: TaskStallPolicy,
        efficiency: TaskEfficiencyPolicy,
    ) -> Self {
        self.task_stall_policy = stall;
        self.task_efficiency_policy = efficiency;
        // This builder is used by the isolated evaluator. Production keeps
        // the environment-resolved advisory behavior unless it deliberately
        // opts into an equivalent host policy in a separate change.
        self.task_stall_bounded_finalization = true;
        self
    }

    /// Resolve one WebSocket `response.create` against a fresh catalog
    /// snapshot. Every turn is routed independently: the selected model is
    /// this turn's only routing authority, and there is no connection-level
    /// fallback to whatever a previous turn on the same socket resolved to.
    /// The caller compares `OfficialWebSocketPlan::connection_key` against
    /// any segment it already has open to decide reuse vs. reopen.
    pub(crate) async fn prepare_websocket_turn(
        &self,
        request: &RuntimeRequest,
    ) -> Result<WebSocketTurnPlan, RuntimeError> {
        let guard = self.begin_lifecycle()?;
        let snapshot = RuntimeSnapshot::capture_from(self.catalog.as_ref());
        let model = request_model(request)?;
        let route_snapshot = snapshot
            .resolve_route_snapshot(model)
            .ok_or_else(|| RuntimeError::RouteNotFound(format!("model not configured: {model}")))?;
        let route = route_snapshot.route.clone();
        if is_guardian_request(&request.body)
            || route.provider_kind != RuntimeProviderKind::Official
            || route.wire != RuntimeWireFormat::Responses
            || contains_vellum_synthetic_item(&request.body)
            || self.official_websocket_requires_portable_handoff(
                &snapshot,
                &route,
                &request.body,
            )?
        {
            return Ok(WebSocketTurnPlan {
                route_id: route.route_id,
                dispatch: WebSocketTurnDispatch::PortableReplay {
                    force_official_handoff: false,
                },
            });
        }
        let auth = ResolvedAuth::resolve(
            &route,
            request,
            self.credentials.as_ref(),
            self.official_auth.as_ref(),
            self.grok_sessions.as_ref(),
            Some(self.history.as_ref()),
        )
        .await?;
        let managed_auth = match &auth {
            ResolvedAuth::OfficialManaged {
                token,
                account_id,
                selection_revision,
                selection_verified,
            } => Some(OfficialAuthorization {
                access_token: token.clone(),
                account_id: account_id.clone(),
                selection_revision: *selection_revision,
                selection_verified: *selection_verified,
            }),
            _ => None,
        };
        let (control_account_hash, execution_account_hash) =
            official_usage_identity(&auth, request);
        let continuation_realm = official_continuation_realm(&route, &auth);
        if self.is_cross_realm_official_continuation(
            &route,
            &request.body,
            continuation_realm.as_deref(),
        )? {
            return Ok(WebSocketTurnPlan {
                route_id: route.route_id,
                dispatch: WebSocketTurnDispatch::PortableReplay {
                    force_official_handoff: true,
                },
            });
        }
        let preserve_incoming_auth = matches!(auth, ResolvedAuth::OfficialPreserveIncoming { .. });
        let upstream_url = official_websocket_endpoint(&route.base_url)?;
        let connection_key = OfficialWebSocketConnectionKey {
            upstream_url: upstream_url.clone(),
            auth_posture: official_websocket_auth_posture(&route, &auth),
        };
        Ok(WebSocketTurnPlan {
            route_id: route.route_id.clone(),
            dispatch: WebSocketTurnDispatch::OfficialNative(OfficialWebSocketPlan {
                connection_key,
                upstream_url,
                upstream_model: route.upstream_model.clone(),
                route_id: route.route_id,
                provider_name: route.name,
                auth_headers: auth.upstream_headers(&route.upstream_model),
                preserve_incoming_auth,
                control_account_hash,
                execution_account_hash,
                selection_revision: match &auth {
                    ResolvedAuth::OfficialManaged {
                        selection_revision, ..
                    } => *selection_revision,
                    _ => None,
                },
                continuation_realm,
                managed_auth,
                _guard: guard,
            }),
        })
    }

    /// Resolve one WebSocket connection against a captured route snapshot.
    /// `None` means the selected model is not an Official Responses route and
    /// must continue through the portable HTTP/adaptation bridge.
    ///
    /// Kept for tests and any caller that only needs the Official/portable
    /// split; [`Self::prepare_websocket_turn`] is the primary entry point the
    /// per-turn WebSocket dispatcher uses so it also gets the connection key.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn prepare_official_websocket(
        &self,
        request: &RuntimeRequest,
    ) -> Result<Option<OfficialWebSocketPlan>, RuntimeError> {
        match self.prepare_websocket_turn(request).await?.dispatch {
            WebSocketTurnDispatch::OfficialNative(plan) => Ok(Some(plan)),
            WebSocketTurnDispatch::PortableReplay { .. } => Ok(None),
        }
    }

    /// Refresh a Vellum-managed Official credential exactly once after a
    /// rejected WebSocket handshake. Native Codex auth remains Codex-owned.
    pub(crate) async fn refresh_official_websocket_auth(
        &self,
        plan: &mut OfficialWebSocketPlan,
    ) -> Result<bool, RuntimeError> {
        let Some(rejected) = plan.managed_auth.clone() else {
            return Ok(false);
        };
        let refreshed = self
            .official_auth
            .refresh_after_rejection(&plan.route_id, &rejected)
            .await
            .map_err(|message| {
                RuntimeError::AuthenticationFailed(format!(
                    "official token refresh failed for route `{}`: {message}",
                    plan.route_id
                ))
            })?;
        if !official_refresh_matches_snapshot(&rejected, &refreshed) {
            return Err(RuntimeError::AuthenticationFailed(
                "official token refresh changed or failed to verify the execution account".into(),
            ));
        }
        let auth = ResolvedAuth::OfficialManaged {
            token: refreshed.access_token.clone(),
            account_id: refreshed.account_id.clone(),
            selection_revision: refreshed.selection_revision.or(plan.selection_revision),
            selection_verified: refreshed.selection_verified,
        };
        plan.auth_headers = auth.upstream_headers(&plan.upstream_model);
        plan.managed_auth = Some(refreshed);
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_official_websocket_completion(
        &self,
        plan: &OfficialWebSocketPlan,
        request: &Value,
        conversation_key: &str,
        parent_request_id: &str,
        execution_id: &str,
        response: &Value,
        duration_ms: u64,
        first_byte_ms: Option<u64>,
        first_downstream_frame_ms: Option<u64>,
        connection_id: &str,
        stage_times_ms: Option<Value>,
    ) -> Result<(), RuntimeError> {
        self.record_subagent_completions(request);
        record_exchange_with_official_compactions(
            self.history.as_ref(),
            request,
            response,
            &plan.route_id,
            true,
            Some(conversation_key),
            plan.continuation_realm.as_deref(),
        )
        .map_err(RuntimeError::History)?;
        self.record_subagent_spawn_requests(parent_request_id, execution_id, response);
        let details = usage_details_from_response(response);
        // Routed through the same atomic terminal guard as every other
        // path (P0): an Official native completion racing a parent cancel
        // or a client disconnect can no longer double-write this turn's
        // outcome, and the win also releases this turn's cancel socket.
        self.finish_terminal_once_with_record(
            execution_id,
            "success",
            UsageRecord {
                route_id: plan.route_id.clone(),
                provider: plan.provider_name.clone(),
                model: plan.upstream_model.clone(),
                input_tokens: details.input_tokens,
                output_tokens: details.output_tokens,
                cached_input_tokens: details.cached_input_tokens,
                reasoning_tokens: details.reasoning_tokens,
                tool_calls: details.tool_calls,
                compaction_tokens: details.compaction_tokens,
                review_tokens: 0,
                status: 200,
                error: None,
                duration_ms,
                first_byte_ms,
                first_event_ms: first_byte_ms,
                first_downstream_frame_ms,
                review_run_id: None,
                review_role: None,
                review_reason: None,
                request_id: Some(parent_request_id.to_string()),
                connection_id: Some(connection_id.to_string()),
                conversation_identity: Some(conversation_key.to_string()),
                outcome: Some("success".into()),
                control_account_hash: plan.control_account_hash.clone(),
                execution_account_hash: plan.execution_account_hash.clone(),
                selection_revision: plan.selection_revision,
                stage_times_ms,
                ..Default::default()
            },
        );
        Ok(())
    }

    /// Execute one request. Admission is linearized first; once admitted, the
    /// entire request runs against a single captured snapshot.
    ///
    /// `execution_id` is the internal cancel-registry key for this attempt --
    /// generated by the caller with [`new_execution_id`] *before* calling
    /// `execute` (never `request.metadata.request_id`, which a WebSocket
    /// client may repeat across turns on one connection) so a caller that
    /// races this future against an inbound `response.cancel` frame (the
    /// WebSocket portable-turn loop) can call [`Self::cancel_request_tree`]
    /// with the right key *while `execute` is still in flight*, not only
    /// after it resolves. Any caller that keeps driving the exchange after
    /// this returns -- forwarding an SSE stream, watching for disconnect --
    /// must keep reusing the same id for further cancel/disconnect
    /// bookkeeping via [`Self::cancel_request_tree`],
    /// [`Self::is_request_cancelled`] or [`Self::finish_terminal_once`].
    pub async fn execute(
        &self,
        request: RuntimeRequest,
        execution_id: &str,
    ) -> Result<RuntimeResponse, RuntimeError> {
        let _lifecycle_guard = self.begin_lifecycle()?;
        let (cancel_guard, cancel_rx) = self.register_request_cancel(execution_id);
        let snapshot = RuntimeSnapshot::capture_from(self.catalog.as_ref())
            .with_web_search_enabled(self.web_search_wrapper_enabled)
            .with_review_settings(self.review_source.current());
        let started = std::time::Instant::now();
        let catalog_id = request
            .body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("missing");
        let route = snapshot.resolve_route_snapshot(catalog_id);
        let conversation = request
            .body
            .get("previous_response_id")
            .and_then(Value::as_str)
            .map(redacted_identity)
            .unwrap_or_else(|| "new".into());
        let result = tokio::select! {
            biased;
            _ = self.stopped() => Err(RuntimeError::ProxyStopped("user stopped proxy".into())),
            result = self.execute_against_snapshot(&snapshot, &request, execution_id) => result,
        };
        let (status, category) = match &result {
            Ok(_) => (200_u16, "ok"),
            Err(error) => (error.http_status().as_u16(), error.category()),
        };
        if let Err(error) = &result {
            if let Some(resolved) = route {
                let (auth_mode, provider_profile, upstream_attempted, retry_after) =
                    opencode_usage_fields(&resolved.route, Some(error));
                let outcome = outcome_for_error(error);
                let status = match outcome {
                    "client_cancel" | "client_disconnect" => 499,
                    _ => error.http_status().as_u16(),
                };
                self.finish_terminal_once_with_record(
                    execution_id,
                    outcome,
                    UsageRecord {
                        route_id: resolved.route.route_id.clone(),
                        provider: provider_name(resolved.route.provider_kind),
                        model: resolved.route.upstream_model.clone(),
                        status,
                        error: Some(error.to_string()),
                        duration_ms: started.elapsed().as_millis() as u64,
                        request_id: Some(request.metadata.request_id.clone()),
                        connection_id: request.metadata.connection_id.clone(),
                        outcome: Some(outcome.to_string()),
                        error_category: Some(error.category().to_string()),
                        auth_mode,
                        provider_profile,
                        upstream_attempted,
                        retry_after,
                        ..Default::default()
                    },
                );
            }
        }
        let usage = match &result {
            Ok(RuntimeResponse::Json(value)) => usage_details_from_response(value),
            _ => Default::default(),
        };
        tracing::info!(
            runtime_version = crate::PROXY_RUNTIME_VERSION,
            git_commit = crate::config::build_commit(),
            config_hash = %self.diagnostic_config_hash,
            request_id = %request.metadata.request_id,
            execution_id = %execution_id,
            endpoint = ?request.endpoint,
            catalog_id,
            route_id = route.map(|item| item.route.route_id.as_str()).unwrap_or("unresolved"),
            provider_kind = route.map(|item| format!("{:?}", item.route.provider_kind)).unwrap_or_else(|| "unresolved".into()),
            wire_format = route.map(|item| format!("{:?}", item.route.wire)).unwrap_or_else(|| "unresolved".into()),
            conversation_identity = %conversation,
            continuation_mode = if request.body.get("previous_response_id").is_some() { "resume" } else { "new" },
            compaction_mode = route.map(|_| "configured").unwrap_or("unresolved"),
            provider_status = status,
            category,
            duration_ms = started.elapsed().as_millis() as u64,
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            cached_input_tokens = usage.cached_input_tokens,
            reasoning_tokens = usage.reasoning_tokens,
            tool_calls = usage.tool_calls,
            compaction_tokens = usage.compaction_tokens,
            "proxy request completed"
        );
        match result {
            Ok(RuntimeResponse::Sse(stream)) => Ok(RuntimeResponse::Sse(take_until_cancel(
                stream,
                cancel_rx,
                cancel_guard,
            ))),
            // Buffered responses and errors are already terminal here: letting
            // `cancel_guard` drop at the end of this function releases the
            // socket immediately rather than leaving it for a stream that was
            // never returned.
            other => other,
        }
    }

    /// Executes one request against a captured snapshot. Auto Review (M9):
    /// a guardian request is detected here and dispatched through the
    /// configured review route plan (primary, then failover) instead of the
    /// normal route, mirroring Desktop's `forward_response` guardian branch.
    ///
    /// The policy read here is `snapshot.review_settings()` — resolved once,
    /// at snapshot capture, from [`Self::review_source`] — never `self`
    /// directly. That is what lets a policy edit reach the very next request
    /// (a fresh snapshot re-reads the source) while never retroactively
    /// changing which route an already-dispatched request resolved against
    /// (it keeps reading its own captured snapshot).
    async fn execute_against_snapshot(
        &self,
        snapshot: &RuntimeSnapshot,
        request: &RuntimeRequest,
        execution_id: &str,
    ) -> Result<RuntimeResponse, RuntimeError> {
        let review = snapshot.review_settings();
        if review.before_send && is_guardian_request(&request.body) {
            return self
                .execute_guardian_dispatch(snapshot, request, execution_id)
                .await;
        }
        self.execute_against_snapshot_inner(snapshot, request, execution_id)
            .await
    }

    /// Auto Review / Guardian dispatch (M9; Failover timeout fix). Tries the
    /// primary reviewer, and — if it fails in a fallback-eligible way
    /// (including not producing a valid assessment within
    /// `GUARDIAN_ATTEMPT_TIMEOUT`, see `review_failure_allows_fallback`) —
    /// retries against the configured fallback with whatever remains of
    /// `GUARDIAN_REVIEW_TOTAL_BUDGET`. Each leg is buffered and validated in
    /// full before either winning (its content reaches the client) or being
    /// discarded in favor of the other leg — see `collect_guardian_attempt`
    /// for why a leg dispatches under a synthetic execution id rather than
    /// `execution_id` directly, and why that is what makes it safe for this
    /// function (not the leg's own inner dispatch) to be the one that claims
    /// `execution_id`'s terminal outcome.
    async fn execute_guardian_dispatch(
        &self,
        snapshot: &RuntimeSnapshot,
        request: &RuntimeRequest,
        execution_id: &str,
    ) -> Result<RuntimeResponse, RuntimeError> {
        let review = snapshot.review_settings();
        let routes = snapshot
            .routes
            .iter()
            .map(|snap| ReviewModelRoute {
                route_id: snap.route.route_id.clone(),
                catalog_id: snap.route.catalog_id.clone(),
                upstream_model: snap.route.upstream_model.clone(),
            })
            .collect::<Vec<_>>();
        let conversation_identity = conversation_key_from_request(&request.body);
        let plan = match resolve_guardian_route_plan(review, &routes) {
            Ok(plan) => plan,
            Err(error) => {
                // Pre-dispatch accounting: no reviewer was ever resolved,
                // let alone contacted -- an Always/Failover
                // misconfiguration produced no real route. Claim this
                // dispatch's terminal outcome here, with no fabricated
                // route/provider/model and `upstream_attempted: false`,
                // so the outer `execute()` generic error handler (which
                // resolves a route from the *client-facing* `catalog_id`
                // -- often literally the hidden Official
                // `codex-auto-review` catalog entry) can never attribute
                // this resolution failure to that Official route as if
                // it had actually been contacted. The same
                // first-writer-wins terminal claim every other guardian
                // path uses makes that generic handler a guaranteed
                // no-op afterward.
                self.finish_terminal_once_with_record(
                    execution_id,
                    "review_resolution_failed",
                    UsageRecord {
                        status: 500,
                        error: Some(error.clone()),
                        duration_ms: 0,
                        review_reason: Some(bounded_text(
                            &error,
                            GUARDIAN_FAILURE_REASON_MAX_CHARS,
                        )),
                        request_id: Some(request.metadata.request_id.clone()),
                        connection_id: request.metadata.connection_id.clone(),
                        conversation_identity: conversation_identity.clone(),
                        outcome: Some("review_resolution_failed".into()),
                        error_category: Some(
                            RuntimeError::Review(error.clone()).category().to_string(),
                        ),
                        upstream_attempted: Some(false),
                        ..Default::default()
                    },
                );
                return Err(RuntimeError::Review(error));
            }
        };
        // One id for the whole dispatch: primary and fallback are two
        // legs of the same logical review run, distinguished by
        // `review_role`, not two unrelated runs.
        let review_run_id = format!("review-{}", ulid::Ulid::new());
        let dispatch_snapshot = snapshot.clone();
        let primary_route = dispatch_snapshot.resolve_route_snapshot(&plan.primary_catalog_id);
        let primary_route_id = primary_route
            .map(|snap| snap.route.route_id.clone())
            .unwrap_or_default();
        let primary_provider = primary_route
            .map(|snap| provider_name(snap.route.provider_kind))
            .unwrap_or_default();
        let primary_model = primary_route
            .map(|snap| snap.route.upstream_model.clone())
            .unwrap_or_else(|| plan.primary_catalog_id.clone());
        let primary_name = primary_route
            .map(|snap| snap.route.name.clone())
            .unwrap_or_else(|| plan.primary_catalog_id.clone());
        let mut primary_body = request.body.clone();
        prepare_guardian_request(
            &mut primary_body,
            &plan.primary_catalog_id,
            primary_route.and_then(|snap| snap.route.context_window),
        );
        // `review_run_id`/`review_role` are deliberately *not* set here (they
        // are not part of `request.metadata.clone()`'s defaults either — see
        // below). `collect_guardian_attempt` dispatches this request under a
        // synthetic execution id specifically so the inner dispatch's own
        // success-path self-claim cannot race `execution_id`'s real terminal
        // slot; if that inner claim's `UsageRecord` also carried the *real*
        // `review_run_id`, it would still land in the usage table looking
        // like a second, spurious "success" row for the same review run
        // (its own claim key — the synthetic id — never collides with
        // anything, so nothing rejects the write). Leaving these fields
        // unset makes that discarded inner row inert instead of confusing.
        // Every real accounting row for this leg is instead written
        // explicitly by this function's own callers, below.
        let primary_request = RuntimeRequest {
            body: primary_body,
            endpoint: request.endpoint,
            incoming_auth: request.incoming_auth.clone(),
            execution_environment: request.execution_environment.clone(),
            metadata: RequestMetadata {
                review_run_id: None,
                review_role: None,
                // Auto Review's own billing identity. Set on both legs so a
                // failover onto a second Official reviewer bills the same
                // account as the primary; unset on every non-guardian
                // request, so a normal turn can never be redirected here.
                review_official_account_id: review.official_account_id.clone(),
                ..request.metadata.clone()
            },
        };
        let review_deadline_start = std::time::Instant::now();
        let primary_started = std::time::Instant::now();
        let primary_outcome = self
            .collect_guardian_attempt(
                &dispatch_snapshot,
                &primary_request,
                execution_id,
                "primary",
                GUARDIAN_ATTEMPT_TIMEOUT,
            )
            .await;
        let first_error = match primary_outcome {
            GuardianAttemptOutcome::Cancelled => {
                // Client is gone before primary ever produced anything: no
                // fallback (nobody is left to answer), and no fabricated
                // route/error — mirrors the resolution-failure branch above.
                return Err(RuntimeError::ProviderUnavailable(
                    "client cancelled before the guardian primary attempt completed".into(),
                ));
            }
            GuardianAttemptOutcome::Success {
                response,
                usage_body,
            } => {
                return Ok(self.record_guardian_leg_success(
                    execution_id,
                    &review_run_id,
                    "primary",
                    &primary_route_id,
                    &primary_provider,
                    &primary_model,
                    primary_started,
                    &primary_request,
                    &conversation_identity,
                    response,
                    usage_body,
                ));
            }
            GuardianAttemptOutcome::Failed(error) => error,
        };
        let attempt_fallback =
            plan.fallback_catalog_id.is_some() && review_failure_allows_fallback(&first_error);
        if !attempt_fallback {
            // Either no fallback is configured, or this failure is not
            // one fallback may paper over (auth/credential/config/
            // protocol/internal errors keep their real classification
            // rather than being retried against a different Provider —
            // see `review_failure_allows_fallback`). Primary's failure
            // *is* this dispatch's one terminal outcome, so it must go
            // through `finish_terminal_once_with_record` — never a bare
            // `self.usage.record()` — the same claim every other
            // dispatch path uses to stay exactly-once under a racing
            // cancel/disconnect, and the same claim that makes the
            // generic error handler in `execute()` (which resolves a
            // route from the *client-facing* model — meaningless for a
            // guardian dispatch, often literally `codex-auto-review`) a
            // guaranteed no-op afterward instead of a second, wrongly
            // attributed row.
            let outcome = outcome_for_error(&first_error);
            let status = match outcome {
                "client_cancel" | "client_disconnect" => 499,
                _ => first_error.http_status().as_u16(),
            };
            self.finish_terminal_once_with_record(
                execution_id,
                outcome,
                UsageRecord {
                    route_id: primary_route_id,
                    provider: primary_provider,
                    model: primary_model,
                    status,
                    error: Some(first_error.to_string()),
                    duration_ms: primary_started.elapsed().as_millis() as u64,
                    review_run_id: Some(review_run_id),
                    review_role: Some("primary".to_string()),
                    request_id: Some(request.metadata.request_id.clone()),
                    connection_id: request.metadata.connection_id.clone(),
                    conversation_identity: conversation_identity.clone(),
                    outcome: Some(outcome.to_string()),
                    error_category: Some(first_error.category().to_string()),
                    ..Default::default()
                },
            );
            return Err(first_error);
        }
        // A fallback will be attempted, so primary's failure is *not*
        // the terminal outcome — the fallback's is. Record primary as a
        // plain diagnostic row (deliberately bypassing the terminal
        // claim) so review stats can still see "primary contacted and
        // failed" distinct from "primary never attempted", without
        // consuming the one terminal slot the fallback attempt below
        // still needs to claim.
        let reason = bounded_guardian_failure_reason(&primary_name, &first_error);
        let _ = self.usage.record(&UsageRecord {
            route_id: primary_route_id,
            provider: primary_provider,
            model: primary_model,
            status: first_error.http_status().as_u16(),
            error: Some(first_error.to_string()),
            duration_ms: primary_started.elapsed().as_millis() as u64,
            review_run_id: Some(review_run_id.clone()),
            review_role: Some("primary".to_string()),
            request_id: Some(request.metadata.request_id.clone()),
            connection_id: request.metadata.connection_id.clone(),
            conversation_identity: conversation_identity.clone(),
            outcome: Some(outcome_for_error(&first_error).to_string()),
            error_category: Some(first_error.category().to_string()),
            ..Default::default()
        });
        let fallback = plan
            .fallback_catalog_id
            .as_deref()
            .expect("attempt_fallback implies fallback_catalog_id is Some");
        let fallback_route = snapshot.resolve_route_snapshot(fallback);
        let fallback_route_id = fallback_route
            .map(|snap| snap.route.route_id.clone())
            .unwrap_or_default();
        let fallback_provider = fallback_route
            .map(|snap| provider_name(snap.route.provider_kind))
            .unwrap_or_default();
        let fallback_model = fallback_route
            .map(|snap| snap.route.upstream_model.clone())
            .unwrap_or_else(|| fallback.to_string());
        let mut fallback_body = request.body.clone();
        prepare_guardian_request(
            &mut fallback_body,
            fallback,
            fallback_route.and_then(|snap| snap.route.context_window),
        );
        // See the matching comment on `primary_request`: `review_run_id`/
        // `review_role` are deliberately left unset so the inner dispatch's
        // own (synthetic-id, discarded) self-claim cannot masquerade as a
        // second real accounting row. `primary_failure_reason` is kept —
        // that one *is* read back by `record_guardian_leg_success`.
        let fallback_request = RuntimeRequest {
            body: fallback_body,
            endpoint: request.endpoint,
            incoming_auth: request.incoming_auth.clone(),
            execution_environment: request.execution_environment.clone(),
            metadata: RequestMetadata {
                review_run_id: None,
                review_role: None,
                primary_failure_reason: Some(reason),
                // Auto Review's own billing identity. Set on both legs so a
                // failover onto a second Official reviewer bills the same
                // account as the primary; unset on every non-guardian
                // request, so a normal turn can never be redirected here.
                review_official_account_id: review.official_account_id.clone(),
                ..request.metadata.clone()
            },
        };
        let fallback_started = std::time::Instant::now();
        // Never more than `GUARDIAN_ATTEMPT_TIMEOUT` — the same ceiling
        // primary gets — even when the primary leg failed fast and left most
        // of `GUARDIAN_REVIEW_TOTAL_BUDGET` unspent: a fallback attempt is
        // not owed a longer window just because primary gave up quickly, and
        // an unbounded "whatever remains" budget would make one leg's actual
        // deadline depend on the other leg's failure speed instead of being
        // a fixed, predictable 30s. Never less than a token amount either,
        // so a pathologically-late fallback dispatch still gets *a* real
        // attempt rather than an instant, guaranteed timeout. The combined
        // primary + fallback ceiling this yields (30s + 30s = 60s) never
        // exceeds `GUARDIAN_REVIEW_TOTAL_BUDGET` (65s).
        let fallback_deadline = GUARDIAN_ATTEMPT_TIMEOUT.min(
            GUARDIAN_REVIEW_TOTAL_BUDGET
                .saturating_sub(review_deadline_start.elapsed())
                .max(std::time::Duration::from_secs(1)),
        );
        let fallback_outcome = self
            .collect_guardian_attempt(
                snapshot,
                &fallback_request,
                execution_id,
                "fallback",
                fallback_deadline,
            )
            .await;
        match fallback_outcome {
            GuardianAttemptOutcome::Cancelled => Err(RuntimeError::ProviderUnavailable(
                "client cancelled before the guardian fallback attempt completed".into(),
            )),
            GuardianAttemptOutcome::Success {
                response,
                usage_body,
            } => Ok(self.record_guardian_leg_success(
                execution_id,
                &review_run_id,
                "fallback",
                &fallback_route_id,
                &fallback_provider,
                &fallback_model,
                fallback_started,
                &fallback_request,
                &conversation_identity,
                response,
                usage_body,
            )),
            GuardianAttemptOutcome::Failed(fallback_error) => {
                // The fallback's own failure is this dispatch's terminal
                // outcome: claim it here, with the fallback's real
                // route/error, for the same reason as the no-fallback
                // branch above.
                let outcome = outcome_for_error(&fallback_error);
                let status = match outcome {
                    "client_cancel" | "client_disconnect" => 499,
                    _ => fallback_error.http_status().as_u16(),
                };
                self.finish_terminal_once_with_record(
                    execution_id,
                    outcome,
                    UsageRecord {
                        route_id: fallback_route_id,
                        provider: fallback_provider,
                        model: fallback_model,
                        status,
                        error: Some(fallback_error.to_string()),
                        duration_ms: fallback_started.elapsed().as_millis() as u64,
                        review_run_id: Some(review_run_id),
                        review_role: Some("fallback".to_string()),
                        request_id: Some(request.metadata.request_id.clone()),
                        connection_id: request.metadata.connection_id.clone(),
                        conversation_identity: conversation_identity.clone(),
                        outcome: Some(outcome.to_string()),
                        error_category: Some(fallback_error.category().to_string()),
                        ..Default::default()
                    },
                );
                Err(fallback_error)
            }
        }
    }

    /// Claims `execution_id`'s terminal outcome for a Guardian leg that
    /// buffered and validated successfully, and returns the
    /// `RuntimeResponse` to actually hand back to the caller. Mirrors the
    /// accounting shape `execute_against_snapshot_inner`'s own
    /// buffered-success path writes (`usage_details_from_response`,
    /// `outcome: "success"`) — exactly what this leg's own inner dispatch
    /// would have claimed itself, had it not run under a synthetic, isolated
    /// execution id (see `collect_guardian_attempt`).
    ///
    /// `Json`/`Raw` claim immediately: nothing downstream consumes them
    /// further, so `execute()`'s `cancel_guard` teardown races nothing.
    /// `Sse` cannot claim immediately — `execute()` wraps a returned `Sse`
    /// stream in `take_until_cancel`, racing it against `execution_id`'s own
    /// cancel socket, and claiming terminal here would have already dropped
    /// that socket's sender (see `claim_terminal_once_raw`). A dropped
    /// `watch::Sender` closes the channel, and `cancel.changed()` resolves
    /// the instant a channel closes — so the wrapped stream would end empty
    /// before the client ever read a single buffered chunk. The claim is
    /// deferred instead, via `defer_guardian_terminal_claim`, so it fires
    /// only once every buffered chunk has actually been yielded — the same
    /// "claim on the stream's own natural completion" contract the original
    /// (non-guardian) streaming generators already rely on.
    #[allow(clippy::too_many_arguments)]
    fn record_guardian_leg_success(
        &self,
        execution_id: &str,
        review_run_id: &str,
        role: &str,
        route_id: &str,
        provider: &str,
        model: &str,
        started: std::time::Instant,
        leg_request: &RuntimeRequest,
        conversation_identity: &Option<String>,
        response: RuntimeResponse,
        usage_body: Option<Value>,
    ) -> RuntimeResponse {
        let details = usage_body
            .as_ref()
            .map(usage_details_from_response)
            .unwrap_or_default();
        let review_tokens = details.input_tokens + details.output_tokens;
        let record = UsageRecord {
            route_id: route_id.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            input_tokens: details.input_tokens,
            output_tokens: details.output_tokens,
            cached_input_tokens: details.cached_input_tokens,
            reasoning_tokens: details.reasoning_tokens,
            tool_calls: details.tool_calls,
            compaction_tokens: details.compaction_tokens,
            review_tokens,
            status: 200,
            error: None,
            duration_ms: started.elapsed().as_millis() as u64,
            review_run_id: Some(review_run_id.to_string()),
            review_role: Some(role.to_string()),
            review_reason: leg_request.metadata.primary_failure_reason.clone(),
            request_id: Some(leg_request.metadata.request_id.clone()),
            connection_id: leg_request.metadata.connection_id.clone(),
            conversation_identity: conversation_identity.clone(),
            outcome: Some("success".into()),
            ..Default::default()
        };
        match response {
            RuntimeResponse::Sse(stream) => RuntimeResponse::Sse(
                self.defer_guardian_terminal_claim(stream, execution_id.to_string(), record),
            ),
            other => {
                self.finish_terminal_once_with_record(execution_id, "success", record);
                other
            }
        }
    }

    /// Wraps a Guardian leg's buffered replay stream so the terminal claim
    /// for `execution_id` fires only once — on *however* the stream actually
    /// ends, not only its happy path. Full consumption (every chunk yielded,
    /// the generator's own loop runs to completion) claims `success`; the
    /// client dropping the stream early — zero chunks read, or partway
    /// through — claims `client_disconnect` (499) instead, via
    /// `DeferredGuardianClaimGuard`'s `Drop` impl, which fires exactly once
    /// regardless of which of those two ways the generator's state actually
    /// stops existing. Uses `Arc`-cloned registry/diagnostics/usage fields
    /// (the same pattern the ordinary streaming generators use for their own
    /// on-completion claim) rather than `&self`, since the returned stream
    /// must be `'static`.
    fn defer_guardian_terminal_claim(
        &self,
        inner: BoxStream<'static, Result<Bytes, RuntimeError>>,
        execution_id: String,
        success_record: UsageRecord,
    ) -> BoxStream<'static, Result<Bytes, RuntimeError>> {
        let mut disconnect_record = success_record.clone();
        disconnect_record.status = 499;
        disconnect_record.outcome = Some("client_disconnect".into());
        disconnect_record.error =
            Some("client disconnected before the guardian reply was fully delivered".into());
        let guard = DeferredGuardianClaimGuard {
            parent_cancel: Arc::clone(&self.parent_cancel),
            diagnostics: Arc::clone(&self.diagnostics),
            usage: Arc::clone(&self.usage),
            execution_id,
            success_record,
            disconnect_record,
            succeeded: false,
        };
        async_stream::stream! {
            let mut inner = inner;
            let mut guard = guard;
            while let Some(item) = inner.next().await {
                yield item;
            }
            // Reached only if the loop above ran to completion -- i.e. the
            // caller actually read every buffered chunk. Any earlier drop of
            // this generator (client disconnect mid-replay) skips this line
            // entirely, so `guard`'s own `Drop` impl claims the
            // `client_disconnect` outcome instead.
            guard.succeeded = true;
        }
        .boxed()
    }

    /// One full Guardian attempt: dispatch under a synthetic, attempt-scoped
    /// execution id, then buffer and validate whatever comes back, all
    /// within `deadline`.
    ///
    /// The synthetic id matters: `execute_against_snapshot_inner`'s own
    /// success paths (the buffered-JSON write, and each streaming
    /// generator's own on-completion write) already self-claim *their*
    /// execution id's terminal usage slot the moment the upstream exchange
    /// itself succeeds — before this function ever gets to ask "but is the
    /// content actually a valid Guardian assessment?". Dispatching under an
    /// isolated per-attempt id (`{execution_id}::guardian-{attempt_label}`,
    /// never looked up by anything else) lets that self-claim land
    /// harmlessly in a slot nobody else reads, so `execution_id`'s own
    /// terminal slot stays unclaimed until `execute_guardian_dispatch`
    /// decides the real outcome — which leg actually wins, or whether both
    /// failed.
    async fn collect_guardian_attempt(
        &self,
        snapshot: &RuntimeSnapshot,
        request: &RuntimeRequest,
        real_execution_id: &str,
        attempt_label: &str,
        deadline: std::time::Duration,
    ) -> GuardianAttemptOutcome {
        if self.is_request_cancelled(real_execution_id) {
            return GuardianAttemptOutcome::Cancelled;
        }
        let attempt_execution_id = format!("{real_execution_id}::guardian-{attempt_label}");
        // Pre-claim the synthetic id's own terminal slot *before* dispatching
        // under it. `execute_against_snapshot_inner`'s success paths (the
        // buffered-JSON write, and each streaming generator's own
        // on-completion write) try to self-claim that same synthetic id via
        // the identical first-writer-wins `claim_terminal_once` gate — with
        // the slot already taken, that claim loses and `usage.record` is
        // never called, so the inner dispatch writes nothing at all instead
        // of an orphaned, unattributed row. This dispatch's only real
        // accounting comes from `record_guardian_leg_success` and the
        // explicit failure-path writes in `execute_guardian_dispatch`, both
        // keyed to `real_execution_id`, never the synthetic one.
        self.claim_terminal_once(
            &attempt_execution_id,
            "guardian_attempt_buffered_internally",
        );
        let work = async {
            let response = match self
                .execute_against_snapshot_inner(snapshot, request, &attempt_execution_id)
                .await
            {
                Ok(response) => response,
                Err(error) => return GuardianAttemptOutcome::Failed(error),
            };
            if self.is_request_cancelled(real_execution_id) {
                return GuardianAttemptOutcome::Cancelled;
            }
            match response {
                RuntimeResponse::Json(body) => {
                    if guardian_body_has_assessment(&body) {
                        GuardianAttemptOutcome::Success {
                            usage_body: Some(body.clone()),
                            response: RuntimeResponse::Json(body),
                        }
                    } else {
                        GuardianAttemptOutcome::Failed(RuntimeError::ProviderProtocol(
                            "reviewer completed without a valid outcome payload".into(),
                        ))
                    }
                }
                RuntimeResponse::Raw {
                    status,
                    content_type,
                    body,
                } => match serde_json::from_slice::<Value>(&body) {
                    Ok(value) if guardian_body_has_assessment(&value) => {
                        GuardianAttemptOutcome::Success {
                            usage_body: Some(value),
                            response: RuntimeResponse::Raw {
                                status,
                                content_type,
                                body,
                            },
                        }
                    }
                    Ok(_) => GuardianAttemptOutcome::Failed(RuntimeError::ProviderProtocol(
                        "reviewer completed without a valid outcome payload".into(),
                    )),
                    Err(error) => GuardianAttemptOutcome::Failed(RuntimeError::ProviderProtocol(
                        format!("reviewer response was not valid JSON: {error}"),
                    )),
                },
                RuntimeResponse::Sse(stream) => {
                    self.drain_and_validate_guardian_sse(stream, real_execution_id)
                        .await
                }
            }
        };
        match tokio::time::timeout(deadline, work).await {
            Ok(outcome) => outcome,
            Err(_) => GuardianAttemptOutcome::Failed(RuntimeError::ReviewAttemptTimeout(format!(
                "guardian {attempt_label} attempt did not complete a valid assessment within {}s",
                deadline.as_secs()
            ))),
        }
    }

    /// Drains a Guardian leg's SSE stream — headers, keepalives, and every
    /// delta are buffered, never forwarded — validating as it goes, and
    /// stops the instant a terminal event lands rather than waiting for the
    /// underlying stream to actually close. Some upstreams keep a connection
    /// open a while after their own `response.completed`/`response.failed`
    /// (idle keep-alive, slow connection teardown); waiting for `None` from
    /// `.next()` would let that lingering openness eat into
    /// `GUARDIAN_ATTEMPT_TIMEOUT` for no reason once the actual answer is
    /// already fully in hand.
    ///
    /// `RuntimeResponse::Sse` is always already Responses-shaped by the time
    /// it reaches here regardless of the reviewer route's own upstream wire:
    /// `execute_against_snapshot_inner`'s Chat-wire generator (`chat_stream`)
    /// already normalizes Chat deltas through its own `ChatSseAdapter` into
    /// Responses-shaped events before yielding them (see its
    /// `yield ok(Bytes::from(normalized))` sites) — a second, redundant
    /// adapter pass here would double-convert already-Responses bytes and
    /// never find a valid event. One `SseCompletionTracker`, fed directly, is
    /// the single source of truth for "did this attempt reach a valid
    /// terminal assessment", for every wire. The *original* bytes — already
    /// client-facing-shaped — are what gets buffered for replay; success
    /// replays them in order, byte-for-byte, with no re-chunking or
    /// re-serialization.
    async fn drain_and_validate_guardian_sse(
        &self,
        mut stream: BoxStream<'static, Result<Bytes, RuntimeError>>,
        real_execution_id: &str,
    ) -> GuardianAttemptOutcome {
        let mut chunks: Vec<Bytes> = Vec::new();
        let mut buffered_bytes: usize = 0;
        let mut tracker = SseCompletionTracker::default();

        loop {
            if self.is_request_cancelled(real_execution_id) {
                return GuardianAttemptOutcome::Cancelled;
            }
            let bytes = match stream.next().await {
                Some(Ok(bytes)) => bytes,
                Some(Err(error)) => return GuardianAttemptOutcome::Failed(error),
                None => break,
            };
            buffered_bytes = buffered_bytes.saturating_add(bytes.len());
            if buffered_bytes > THIRD_PARTY_NON_STREAM_MAX_BYTES {
                return GuardianAttemptOutcome::Failed(RuntimeError::ProviderProtocol(format!(
                    "guardian reviewer stream exceeded {THIRD_PARTY_NON_STREAM_MAX_BYTES} bytes"
                )));
            }
            chunks.push(bytes.clone());
            let just_completed = tracker.push_chunk(&bytes);
            // Checked every chunk, not just at stream end: a `response.failed`
            // is exactly as terminal as `response.completed` the instant it
            // lands, and must not wait for the connection to also close.
            if let Some(error) = tracker.terminal_error() {
                return GuardianAttemptOutcome::Failed(RuntimeError::ProviderProtocol(error));
            }
            if let Some(response) = just_completed {
                if !guardian_body_has_assessment(&response) {
                    return GuardianAttemptOutcome::Failed(RuntimeError::ProviderProtocol(
                        "reviewer completed without a valid outcome payload".into(),
                    ));
                }
                let replay =
                    futures_util::stream::iter(chunks.into_iter().map(Ok::<_, RuntimeError>))
                        .boxed();
                return GuardianAttemptOutcome::Success {
                    response: RuntimeResponse::Sse(replay),
                    usage_body: Some(response),
                };
            }
        }
        // The stream closed on its own (`None`) without ever reaching a
        // terminal event -- a premature EOF, distinct from both the
        // above cases but just as much a real, structural failure: retrying
        // the exact same request would not fix a reviewer that hangs up
        // early, so this stays `ProviderProtocol`, not fallback-eligible on
        // its own account (the *timeout* wrapping this whole attempt is what
        // makes an EOF that never even gets this far fallback-eligible).
        GuardianAttemptOutcome::Failed(RuntimeError::ProviderProtocol(
            "guardian reviewer stream ended before a terminal response".into(),
        ))
    }

    fn begin_lifecycle(&self) -> Result<RequestGuard, RuntimeError> {
        self.lifecycle
            .begin()
            .map_err(RuntimeError::LifecycleDraining)
    }

    async fn execute_upstream(
        &self,
        request: UpstreamRequest,
        route: &RuntimeModelRoute,
    ) -> Result<UpstreamResponse, RuntimeError> {
        self.reject_opencode_cooldown(route)?;
        let response = execute_upstream_inner(self.transport.as_ref(), &request, route).await?;
        self.note_opencode_quota(route, response.status, &response.headers, &response.body);
        Ok(response)
    }

    fn reject_opencode_cooldown(&self, route: &RuntimeModelRoute) -> Result<(), RuntimeError> {
        if route.effective_provider_profile().is_none() {
            return Ok(());
        }
        if let Some(entry) = self
            .opencode_quota
            .get(&route.route_id, &route.upstream_model)
        {
            let remaining = entry.remaining_secs();
            return Err(RuntimeError::provider_quota(
                entry.diagnostic,
                Some(remaining),
                false,
            ));
        }
        Ok(())
    }

    fn note_opencode_quota(
        &self,
        route: &RuntimeModelRoute,
        status: u16,
        headers: &[(String, String)],
        body: &[u8],
    ) {
        if route.effective_provider_profile().is_none() || status != 429 {
            return;
        }
        let retry_after = crate::opencode::parse_retry_after(headers)
            .unwrap_or(crate::opencode::DEFAULT_QUOTA_RETRY_AFTER_SECS);
        self.opencode_quota.record(
            &route.route_id,
            &route.upstream_model,
            retry_after,
            provider_error_message(body, status),
        );
    }

    pub fn expire_opencode_cooldown(&self, route_id: &str, model: &str) {
        self.opencode_quota.expire(route_id, model);
    }

    /// Open a streaming upstream exchange, bounding only the header phase
    /// (send ??status + headers observed) at [`STREAM_OPEN_HEADER_TIMEOUT`].
    /// A slow *body* after headers have already arrived never hits this
    /// timeout: `execute_streaming` returns as soon as reqwest observes the
    /// response head, and everything after that point is governed by the
    /// caller's own per-chunk idle deadlines.
    ///
    /// On elapse this returns exactly one [`RuntimeError::StreamOpenTimeout`],
    /// which `ProxyRuntime::execute` already turns into exactly one usage row
    /// (the generic on-`Err` record at the top of `execute`) with
    /// `first_byte_ms` left `None` ??no stream was ever established, so there
    /// is no first byte to report.
    async fn open_upstream_stream(
        &self,
        request: &UpstreamRequest,
    ) -> Result<UpstreamStream, RuntimeError> {
        match tokio::time::timeout(
            STREAM_OPEN_HEADER_TIMEOUT,
            self.transport.execute_streaming(request),
        )
        .await
        {
            Ok(result) => result.map_err(map_transport_error),
            Err(_) => Err(RuntimeError::StreamOpenTimeout(format!(
                "no response headers within {}s",
                STREAM_OPEN_HEADER_TIMEOUT.as_secs()
            ))),
        }
    }

    async fn execute_against_snapshot_inner(
        &self,
        snapshot: &RuntimeSnapshot,
        request: &RuntimeRequest,
        execution_id: &str,
    ) -> Result<RuntimeResponse, RuntimeError> {
        let started = std::time::Instant::now();
        let mut stages = DispatchStages::new();
        let model = request_model(request)?;
        let streaming = request_requests_streaming(request);
        // Resolve the route *and* its captured catalog entry from the snapshot,
        // never from the live catalog: the adapter verifies against the entry
        // captured at admission, and `base_instructions` replacement matches
        // the exact string that entry shipped.
        let route_snapshot = snapshot
            .resolve_route_snapshot(model)
            .ok_or_else(|| RuntimeError::RouteNotFound(format!("model not configured: {model}")))?;
        let route = &route_snapshot.route;
        let conversation_key = self.resolve_request_conversation_key(request).key;
        // Third-party destinations only. Official OpenAI Responses is a native
        // passthrough and must never enter this validator ??prepare_official_websocket,
        // forward_official_compact/search, and official_stream stay on the
        // Official URL unchanged.
        if route.provider_kind != RuntimeProviderKind::Official {
            crate::outbound::check_third_party_url(
                route,
                &route.base_url,
                crate::outbound::route_carries_credential(route),
            )
            .map_err(|error| RuntimeError::InvalidRequest(error.to_string()))?;
        }
        let profile = resolve_with_options(
            route.provider_kind,
            route.wire,
            self.harness_options,
            self.delegation_runtime_wired,
        );
        // Official continuation realm is account-scoped. Resolve its auth
        // snapshot before deciding whether `previous_response_id` may remain
        // native; the same snapshot is retained for the eventual dispatch.
        let mut resolved_official_auth = if route.provider_kind == RuntimeProviderKind::Official {
            Some(
                ResolvedAuth::resolve(
                    route,
                    request,
                    self.credentials.as_ref(),
                    self.official_auth.as_ref(),
                    self.grok_sessions.as_ref(),
                    Some(self.history.as_ref()),
                )
                .await?,
            )
        } else {
            None
        };
        let current_official_realm = resolved_official_auth
            .as_ref()
            .and_then(|auth| official_continuation_realm(route, auth));
        // M4: resolve the continuation and hydrate the chain *before*
        // translation, on a clone of the request body. The recorded exchange
        // keeps the original request (with `previous_response_id`) so chains
        // link; the upstream body gets the chain replayed and the field
        // stripped (third-party contract) or kept (Official server resume).
        let mut execution_body = request.body.clone();
        let mut history_request_body = request.body.clone();
        // Official is a native contract. On the same route, Codex already
        // supplies exactly the state the OpenAI backend expects, including
        // opaque reasoning and compaction items. Vellum must not hydrate,
        // inject include fields, materialize checkpoints, or auto-compact it.
        // The only exception is an actual provider switch into Official, where
        // the foreign chain has to become portable visible history first.
        let official_route_switch =
            self.is_cross_route_official_continuation(route, &request.body)?;
        let official_account_switch = request.metadata.force_portable_official_handoff
            || self.is_cross_realm_official_continuation(
                route,
                &request.body,
                current_official_realm.as_deref(),
            )?;
        // Desktop may replay a complete third-party snapshot without a
        // previous_response_id. A Vellum-synthesized item ID is authoritative
        // evidence that this is a portable handoff, never native OpenAI state.
        let official_snapshot_handoff = route.provider_kind == RuntimeProviderKind::Official
            && contains_vellum_synthetic_item(&request.body);
        let official_handoff =
            official_route_switch || official_account_switch || official_snapshot_handoff;
        let hydration = if route.provider_kind == RuntimeProviderKind::Official && !official_handoff
        {
            HydrationOutcome::native_passthrough(
                crate::request::request_input_items(&execution_body).len(),
            )
        } else {
            self.hydrate_request_history(
                route,
                &request.body,
                &mut execution_body,
                official_handoff,
            )?
        };
        let portable_replay = hydration.continuation_mode
            == ContinuationMode::PortableSemanticReplay
            && (route.provider_kind != RuntimeProviderKind::Official || official_handoff);
        stages.mark("hydrated");
        let mut replay = hydration.replay_context();
        if official_handoff {
            let before = crate::request::request_input_items(&execution_body);
            sanitize_for_official(&mut execution_body);
            let after = crate::request::request_input_items(&execution_body);
            realign_identities_after_item_filter(&mut replay, &before, &after)?;
        }
        let local_compaction_trigger = has_compaction_trigger(&execution_body)
            && route.provider_kind != RuntimeProviderKind::Official;
        if route.provider_kind != RuntimeProviderKind::Official || official_handoff {
            self.materialize_local_compactions(
                route.provider_kind,
                &mut execution_body,
                local_compaction_trigger,
                portable_replay,
                &mut replay,
            )?;
        }
        if route.provider_kind != RuntimeProviderKind::Official {
            self.repair_client_local_compaction(
                route,
                &request.body,
                &conversation_key,
                &mut execution_body,
                &mut replay,
            )?;
        }

        // Trajectory diagnostics and independent task recovery.
        let current_input_items = crate::request::request_input_items(&execution_body);
        if let Ok(portable_items) = crate::compaction::sanitize_portable_history_with_provenance(
            &current_input_items,
            &replay.item_identities,
        ) {
            let portable_values: Vec<Value> =
                portable_items.iter().map(|p| p.value.clone()).collect();
            let portable_identities: Vec<String> = portable_items
                .iter()
                .map(|p| p.occurrence_id.clone())
                .collect();
            // Recovery is resolved from its own policy, never from the
            // compaction engine: changing how a conversation is compacted must
            // not change whether it recovers.
            let recovery_policy = crate::recovery_policy::resolve_recovery_policy(route);
            let loop_policy = LoopGuardPolicy::default();
            if let Ok(trajectory) =
                analyze_trajectory(&portable_values, &portable_identities, &loop_policy)
            {
                let candidate = crate::trajectory::select_loop_guard_candidate(&trajectory);
                let pattern_key = candidate.map(|p| p.pattern_hash.clone());

                let ledger_policy = InvestigationPolicy::default();
                let mut ledger_recovery = None;
                let mut ledger_recovery_diagnostic = None;
                let mut stall_recovery = None;
                let mut stall_state = None;
                let mut efficiency_recovery = None;
                let mut efficiency_state_ref = None;
                let mut pending_task_efficiency_diag = None;
                let mut pending_recovery_snapshot = None;
                let mut action_recovery_escalation = true;
                let mut task_closure_overlay = None;
                if recovery_policy.investigation_recovery {
                    // Usage records are persisted under the runtime-resolved
                    // conversation identity. Provider request bodies do not
                    // always expose their optional conversation key on
                    // ordinary turns, which previously delayed cumulative
                    // usage until compaction/history happened to reveal it.
                    let conv_usages =
                        self.completed_request_usages_for_conversation(&conversation_key);
                    // Durable recovery state is keyed by conversation, so it
                    // survives a restart and follows the conversation across a
                    // provider switch. A corrupt snapshot disables recovery for
                    // this conversation rather than blocking it or seeding
                    // state that never happened.
                    let stored_snapshot = self
                        .history
                        .recovery_snapshot(&conversation_key)
                        .ok()
                        .flatten();
                    let snapshot_outcome = crate::recovery_snapshot::load_or_rebuild(
                        stored_snapshot,
                        &conversation_key,
                        || None,
                    );
                    let recovery_snapshot = match &snapshot_outcome {
                        crate::recovery_snapshot::SnapshotRecovery::Usable(snapshot)
                        | crate::recovery_snapshot::SnapshotRecovery::Rebuilt(snapshot) => {
                            Some(snapshot.as_ref())
                        }
                        crate::recovery_snapshot::SnapshotRecovery::Disabled { reason } => {
                            tracing::warn!(
                                conversation = %conversation_key,
                                reason = %reason,
                                "recovery disabled for this conversation"
                            );
                            None
                        }
                    };
                    let exact_turn_id = request
                        .metadata
                        .codex_identity
                        .as_ref()
                        .filter(|identity| {
                            identity.trust != crate::codex_metadata::CodexIdentityTrust::Conflict
                        })
                        .and_then(|identity| identity.turn_id.as_ref())
                        .map(ToString::to_string);
                    let authoritative_task_revision = exact_turn_id.as_ref().map(|turn_id| {
                        let prior_revision = recovery_snapshot
                            .map(|snapshot| snapshot.task_revision)
                            .unwrap_or_default();
                        if recovery_snapshot
                            .and_then(|snapshot| snapshot.last_user_turn_id.as_deref())
                            == Some(turn_id.as_str())
                        {
                            prior_revision.max(1)
                        } else {
                            prior_revision.saturating_add(1).max(1)
                        }
                    });
                    let input = build_investigation_runtime_input_seeded_with_usage_and_revision(
                        &current_input_items,
                        Some(&replay.item_identities),
                        recovery_snapshot.and_then(|s| s.investigation.as_ref()),
                        recovery_snapshot.and_then(|s| s.efficiency.as_ref()),
                        Some(&conv_usages),
                        authoritative_task_revision,
                    );
                    let stall_policy = task_stall_policy_for_request(
                        local_compaction_trigger,
                        self.task_stall_policy.clone(),
                    );
                    let efficiency_policy = task_efficiency_policy_for_request(
                        local_compaction_trigger,
                        self.task_efficiency_policy.clone(),
                    );
                    action_recovery_escalation = efficiency_policy.action_recovery_escalation;
                    // A compaction request never dispatches its input as the
                    // next model turn. Injecting guidance here would persist
                    // a recovery marker the model never saw and suppress the
                    // real recovery on the following inference request.
                    let reduced = reduce_investigation_input_traced_with_stall_and_efficiency(
                        &input,
                        &ledger_policy,
                        &stall_policy,
                        &efficiency_policy,
                    );
                    let investigation = reduced.state;
                    let efficiency_state = reduced.efficiency_state;
                    self.diagnostics.record(DiagnosticEvent::TaskStall(
                        crate::task_stall::TaskStallDiagnostic::from_state(
                            &investigation.task_stall,
                            &stall_policy,
                            investigation.ledgers.len(),
                            investigation.task_stall.tool_results_since_progress
                                >= stall_policy.recovery_after_tool_results,
                            Some(sha256_hex(&request.metadata.request_id)),
                        ),
                    ));
                    efficiency_recovery = reduced.efficiency_recovery;
                    stall_recovery = reduced.stall_recovery;
                    stall_state = Some(investigation.task_stall.clone());
                    if self.task_stall_bounded_finalization
                        && stall_recovery.is_none()
                        && crate::task_stall::eval_watchdog_should_stop(
                            &investigation.task_stall,
                            &stall_policy,
                        )
                    {
                        self.diagnostics.record(DiagnosticEvent::TaskStallTerminal(
                            crate::task_stall::TaskStallTerminalDiagnostic {
                                request_id_hash: sha256_hex(&request.metadata.request_id),
                                request_index: eval_request_index(&request.metadata.request_id),
                                task_revision: investigation.task_stall.task_revision,
                                recovery_count: investigation.task_stall.recovery_total,
                                post_recovery_no_progress: investigation
                                    .task_stall
                                    .post_recovery_tool_results_without_progress,
                                category: "continued_after_tool_disabled_finalization".into(),
                            },
                        ));
                        return Err(RuntimeError::ToolLoopLimit(
                            "task exploration continued after the bounded stall finalization"
                                .into(),
                        ));
                    }
                    pending_task_efficiency_diag = Some(
                        crate::task_efficiency::TaskEfficiencyDiagnostic::from_state(
                            &efficiency_state,
                            &efficiency_policy,
                            Some(sha256_hex(&request.metadata.request_id)),
                            eval_request_index(&request.metadata.request_id),
                        ),
                    );
                    // Persist the merged recovery state under the durable
                    // conversation identity so a restart or a provider switch
                    // does not lose that this conversation already recovered.
                    // A compaction trigger never reaches here: it is an
                    // auxiliary call, not a turn.
                    if !local_compaction_trigger {
                        let mut persisted = recovery_snapshot.cloned().unwrap_or_else(|| {
                            crate::recovery_snapshot::ConversationRecoverySnapshotV1::new(
                                conversation_key.clone(),
                            )
                        });
                        persisted.investigation = Some(investigation.clone());
                        persisted.efficiency = Some(efficiency_state.clone());
                        persisted.stall = Some(investigation.task_stall.clone());
                        persisted.task_revision =
                            persisted.task_revision.max(efficiency_state.task_revision);
                        if let Some(turn_id) = exact_turn_id {
                            persisted.last_user_turn_id = Some(turn_id);
                        }
                        persisted.usage_watermark = persisted
                            .usage_watermark
                            .max(efficiency_state.cumulative_input_tokens);
                        persisted.updated_at = crate::history::now_unix_secs();
                        if let Err(error) = self.history.put_recovery_snapshot(&persisted) {
                            tracing::warn!(
                                conversation = %conversation_key,
                                %error,
                                "could not persist the recovery snapshot"
                            );
                        }
                        pending_recovery_snapshot = Some(persisted);
                    }
                    efficiency_state_ref = Some(efficiency_state);
                    ledger_recovery_diagnostic =
                        reduced.ledger_recovery.as_ref().and_then(|action| {
                            investigation
                                .ledgers
                                .iter()
                                .find(|entry| entry.ledger_id == action.ledger_id)
                                .map(|entry| {
                                    crate::investigation_reducer::recovery_diagnostic(action, entry)
                                })
                        });
                    ledger_recovery = reduced.ledger_recovery;
                    let traces = reduced.traces;
                    for trace in traces {
                        self.diagnostics
                            .record(DiagnosticEvent::InvestigationOperation(
                                crate::investigation_reducer::operation_diagnostic(
                                    &trace.operation,
                                ),
                            ));
                        self.diagnostics.record(DiagnosticEvent::InvestigationLink(
                            crate::investigation_reducer::link_diagnostic(
                                (!trace.transition.ledger_id.is_empty())
                                    .then_some(trace.transition.ledger_id.as_str()),
                                trace.transition.link_method,
                                trace.transition.link_confidence,
                            ),
                        ));
                        self.diagnostics
                            .record(DiagnosticEvent::InvestigationTransition(
                                crate::investigation_reducer::transition_diagnostic(
                                    &trace.transition,
                                    trace.transition.recovery_state,
                                    trace.transition.redundant_revisit_count,
                                ),
                            ));
                    }
                }
                self.diagnostics.record(DiagnosticEvent::TrajectoryDecision(
                    TrajectoryDecisionDiagnostic {
                        tool_exchange_count: trajectory.exchanges.len() as u64,
                        tool_only_streak: trajectory.tool_only_streak as u64,
                        repeated_pattern_count: (trajectory.repeated_exchanges.len()
                            + trajectory.repeated_sequences.len())
                            as u64,
                        max_repeat_count: trajectory.max_repeat_count as u64,
                        pattern_hash: pattern_key.clone(),
                        action: "observe".to_string(),
                    },
                ));

                // Trajectory analysis is diagnostic only. Repeated calls must not
                // remove tools, inject loop warnings, or terminate a conversation.
                if recovery_policy.task_efficiency {
                    if let Some(eff_state) = efficiency_state_ref.as_mut() {
                        let live_key = format!("{}:{}", conversation_key, route.route_id);
                        let activation_req_idx = eval_request_index(&request.metadata.request_id);
                        let mut registry = self
                            .efficiency_recoveries
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        registry.retain(|_, entry| {
                            entry.touched.elapsed() < LIVE_EFFICIENCY_RECOVERY_TTL
                        });
                        evict_oldest(&mut registry, |entry| entry.touched);
                        let entry = registry.entry(live_key).or_insert_with(|| {
                            LiveEfficiencyRecoveryEntry {
                                state: Default::default(),
                                touched: tokio::time::Instant::now(),
                            }
                        });
                        // Single atomic activation: `reconcile_live_efficiency_recovery`
                        // materializes the active state, request/token anchors and the
                        // conversion record into BOTH `eff_state` and `entry.state`.
                        efficiency_recovery =
                            crate::task_efficiency::reconcile_live_efficiency_recovery(
                                &mut entry.state,
                                eff_state,
                                efficiency_recovery,
                                activation_req_idx,
                            );
                        // Escalation gate is re-checked while the registry entry is
                        // still held so an L1→L2 transition is written back to the
                        // live authority, not just the (possibly un-checkpointed)
                        // reducer state.
                        if let Some(active) = eff_state.active_action_recovery.as_mut() {
                            if action_recovery_escalation
                                && active.level
                                    == crate::task_efficiency::ActionRecoveryLevel::ConvertEvidence
                                && (active.post_recovery_tool_results >= 4
                                    || active.post_recovery_input_tokens >= 25_000)
                            {
                                active.level =
                                    crate::task_efficiency::ActionRecoveryLevel::ActionRequired;
                            }
                        }
                        entry.state.active_action_recovery =
                            eff_state.active_action_recovery.clone();
                        entry.state.recovery_conversions = eff_state.recovery_conversions.clone();
                        if !local_compaction_trigger && eff_state.active_action_recovery.is_none() {
                            task_closure_overlay =
                                crate::task_efficiency::consume_task_closure_overlay(
                                    &mut entry.state,
                                );
                        }
                        entry.touched = tokio::time::Instant::now();
                    }

                    let model_facing = if let Some(eff_state) = efficiency_state_ref.as_ref() {
                        if let Some(active) = eff_state.active_action_recovery.as_ref() {
                            match active.level {
                                crate::task_efficiency::ActionRecoveryLevel::ActionRequired => {
                                    Some("researchSprawlL2".to_string())
                                }
                                crate::task_efficiency::ActionRecoveryLevel::ConvertEvidence => {
                                    Some("researchSprawlL1".to_string())
                                }
                            }
                        } else if efficiency_recovery.is_some() {
                            Some("researchSprawl".to_string())
                        } else if task_closure_overlay.is_some() {
                            Some("taskClosure".to_string())
                        } else if stall_recovery.is_some() {
                            Some("taskStall".to_string())
                        } else {
                            None
                        }
                    } else if efficiency_recovery.is_some() {
                        Some("researchSprawl".to_string())
                    } else if stall_recovery.is_some() {
                        Some("taskStall".to_string())
                    } else {
                        None
                    };

                    if let Some(mut diag) = pending_task_efficiency_diag.take() {
                        if let Some(eff_state) = efficiency_state_ref.as_ref() {
                            diag.research_sprawl_recovery_count =
                                eff_state.research_sprawl_recovery_total;
                            diag.last_recovery_request_index =
                                eff_state.last_recovery_request_index;
                            diag.recovery_conversions = eff_state.recovery_conversions.clone();
                            diag.active_action_recovery = eff_state.active_action_recovery.clone();
                            diag.active_recovery_level =
                                eff_state.active_action_recovery.as_ref().map(|a| a.level);
                            diag.compactions_since_recovery_activation = eff_state
                                .active_action_recovery
                                .as_ref()
                                .map(|a| a.compactions_since_activation);
                        }
                        diag.model_facing_recovery = model_facing;
                        self.diagnostics
                            .record(DiagnosticEvent::TaskEfficiency(diag));
                    }
                    if let (Some(eff_recovery), Some(eff_state)) =
                        (efficiency_recovery.as_ref(), efficiency_state_ref.as_mut())
                    {
                        let req_idx = eval_request_index(&request.metadata.request_id);
                        let synthetic_id = format!(
                            "synthetic:task_efficiency_recovery:{}",
                            eff_recovery.recovery_count
                        );
                        // The marker is a durable replay/rebuild anchor only: it is
                        // written to persisted history (so a later rebuild can
                        // reconstruct the recovery epoch) but is NOT added to the
                        // model-visible execution body. The single model-visible
                        // execution-control block on this and every subsequent
                        // ordinary request is the persistent active-recovery
                        // overlay below (plan §7.1, §18).
                        let synthetic_item = json!({
                            "type": "message",
                            "role": "developer",
                            "id": synthetic_id,
                            "content": eff_recovery.message.clone(),
                            "internal_efficiency_recovery": {
                                "recovery_count": eff_recovery.recovery_count,
                                "request_index": req_idx,
                                "cumulative_input_tokens": eff_state.cumulative_input_tokens,
                            }
                        });
                        history_request_body
                            .get_mut("input")
                            .and_then(Value::as_array_mut)
                            .ok_or_else(|| {
                                RuntimeError::InvalidRequest(
                                    "request body is missing input array".into(),
                                )
                            })?
                            .push(synthetic_item);
                        self.diagnostics
                            .record(DiagnosticEvent::TaskEfficiencyRecovery(
                                crate::task_efficiency::TaskEfficiencyRecoveryDiagnostic {
                                    request_id_hash: sha256_hex(&request.metadata.request_id),
                                    request_index: req_idx,
                                    recovery_count: eff_recovery.recovery_count,
                                    recovery_message_hash: crate::diagnostics::hash_text(
                                        &eff_recovery.message,
                                    ),
                                    input_tokens_since_world_change: eff_state
                                        .input_tokens_since_world_change,
                                    tool_results_since_world_change: eff_state
                                        .tool_results_since_world_change,
                                },
                            ));
                    }

                    // Persistent Active Recovery Overlay: rematerialized on every
                    // ordinary provider request via the shared reducer helper so
                    // exactly one execution-control block reaches the model. Any
                    // hydrated `synthetic:task_efficiency_recovery:` anchor is
                    // stripped from the dispatched body (it stays in persisted
                    // history for rebuilds) (plan §18, §26).
                    if !local_compaction_trigger {
                        if let Some(input_arr) = execution_body
                            .get_mut("input")
                            .and_then(Value::as_array_mut)
                        {
                            let active = efficiency_state_ref
                                .as_ref()
                                .and_then(|e| e.active_action_recovery.as_ref());
                            crate::task_efficiency::apply_active_execution_overlay(
                                input_arr, active,
                            );
                            crate::task_efficiency::apply_task_closure_overlay(
                                input_arr,
                                task_closure_overlay,
                            );
                        }
                    }

                    let has_active_action_recovery = efficiency_state_ref
                        .as_ref()
                        .and_then(|e| e.active_action_recovery.as_ref())
                        .is_some();
                    if has_active_action_recovery || efficiency_recovery.is_some() {
                        // Research sprawl recovery has precedence over task stall and ledger recovery
                    } else if let (Some(stall), Some(stall_state)) =
                        (stall_recovery.as_ref(), stall_state.as_mut())
                    {
                        // The injected marker is an execution-side
                        // effect. Record it in the durable state now,
                        // rather than relying on a future request to
                        // replay a synthetic item that Codex may have
                        // compacted out of its next input.
                        crate::task_stall::reconcile_stall_recovery(
                            stall_state,
                            stall.recovery_count,
                            Some(current_input_items.len()),
                        );
                        let recovery_message = if self.task_stall_bounded_finalization
                            && stall.level == crate::task_stall::StallRecoveryState::Escalated
                        {
                            crate::stall_recovery::bounded_finalization_message()
                        } else {
                            stall.message.clone()
                        };
                        let synthetic_id =
                            format!("synthetic:task_stall_recovery:{}", stall.recovery_count);
                        let synthetic_item = json!({
                            "type": "message",
                            "role": "developer",
                            "id": synthetic_id,
                            "content": recovery_message.clone()
                        });
                        crate::replay::append_synthetic_replay_item(
                            &mut execution_body,
                            &mut replay,
                            synthetic_item.clone(),
                            synthetic_id.clone(),
                        )
                        .map_err(RuntimeError::InvalidRequest)?;
                        history_request_body
                            .get_mut("input")
                            .and_then(Value::as_array_mut)
                            .ok_or_else(|| {
                                RuntimeError::InvalidRequest(
                                    "request body is missing input array".into(),
                                )
                            })?
                            .push(synthetic_item);
                        apply_bounded_stall_finalization(
                            &mut execution_body,
                            stall.level,
                            self.task_stall_bounded_finalization,
                        );
                        self.diagnostics.record(DiagnosticEvent::TaskStallRecovery(
                            crate::task_stall::TaskStallRecoveryDiagnostic {
                                request_id_hash: sha256_hex(&request.metadata.request_id),
                                request_index: eval_request_index(
                                    &request.metadata.request_id,
                                ),
                                recovery_count: stall.recovery_count,
                                recovery_level: stall.level,
                                intervention: if self.task_stall_bounded_finalization
                                    && stall.level
                                        == crate::task_stall::StallRecoveryState::Escalated
                                {
                                    crate::task_stall::TaskStallIntervention::ToolDisabledFinalization
                                } else {
                                    crate::task_stall::TaskStallIntervention::Warning
                                },
                                recovery_message_hash: crate::diagnostics::hash_text(
                                    &recovery_message,
                                ),
                                tool_results_since_progress: stall_state
                                    .tool_results_since_progress,
                                post_recovery_no_progress: stall_state
                                    .post_recovery_tool_results_without_progress,
                                last_progress: stall_state.last_progress.clone(),
                            },
                        ));
                    } else if let Some(recovery) = ledger_recovery {
                        let recovery_diagnostic = ledger_recovery_diagnostic.unwrap_or_else(|| {
                            crate::investigation_diagnostics::InvestigationRecoveryDiagnostic {
                                ledger_id: recovery.ledger_id.clone(),
                                why: "redundant_revisit_without_frontier_or_relevant_progress"
                                    .into(),
                                known_evidence_signatures: Vec::new(),
                                missing_state: "independent evidence still absent".into(),
                                recovery_count: recovery.recovery_count,
                                post_recovery_repeated: recovery.level
                                    == crate::investigation_reducer::RecoveryState::Escalated,
                            }
                        });
                        self.diagnostics
                            .record(DiagnosticEvent::InvestigationRecovery(recovery_diagnostic));
                        let synthetic_id = format!(
                            "synthetic:investigation_recovery:{}:{}",
                            recovery.ledger_id, recovery.recovery_count
                        );
                        let synthetic_item = json!({
                            "type": "message",
                            "role": "developer",
                            "id": synthetic_id,
                            "content": recovery.message
                        });
                        crate::replay::append_synthetic_replay_item(
                            &mut execution_body,
                            &mut replay,
                            synthetic_item.clone(),
                            synthetic_id.clone(),
                        )
                        .map_err(RuntimeError::InvalidRequest)?;
                        history_request_body
                            .get_mut("input")
                            .and_then(Value::as_array_mut)
                            .ok_or_else(|| {
                                RuntimeError::InvalidRequest(
                                    "request body is missing input array".into(),
                                )
                            })?
                            .push(synthetic_item);
                    }
                }
                if let Some(mut persisted) = pending_recovery_snapshot.take() {
                    if let Some(stall) = stall_state.as_ref() {
                        persisted.stall = Some(stall.clone());
                        if let Some(investigation) = persisted.investigation.as_mut() {
                            investigation.task_stall = stall.clone();
                        }
                    }
                    if let Some(efficiency) = efficiency_state_ref.as_ref() {
                        persisted.efficiency = Some(efficiency.clone());
                        persisted.task_revision =
                            persisted.task_revision.max(efficiency.task_revision);
                        persisted.usage_watermark = persisted
                            .usage_watermark
                            .max(efficiency.cumulative_input_tokens);
                    }
                    persisted.updated_at = crate::history::now_unix_secs();
                    if let Err(error) = self.history.put_recovery_snapshot(&persisted) {
                        tracing::warn!(
                            conversation = %conversation_key,
                            %error,
                            "could not persist the recovery snapshot"
                        );
                    }
                }
                if let Some(mut diag) = pending_task_efficiency_diag.take() {
                    diag.model_facing_recovery = None;
                    self.diagnostics
                        .record(DiagnosticEvent::TaskEfficiency(diag));
                }
            }
        }

        self.record_subagent_completions(&request.body);
        if self.record_subagent_child_turn(
            request,
            &route.route_id,
            &route.catalog_id,
            &route.upstream_model,
            &execution_body,
            execution_id,
        ) {
            // The child linked to a parent that was already cancelled
            // (spawn → cancel → late child). Abort before any upstream
            // dispatch: the child is marked cancelled, so the caller observes
            // the fan-out and records the bounded 499 row for this child, and
            // the empty stream ends the turn without a single outbound byte.
            return Ok(RuntimeResponse::Sse(futures_util::stream::empty().boxed()));
        }
        // Codex sends a remote compact task through `/responses` as a private
        // `compaction_trigger` item (present in the bundled 0.142.5, not a
        // 0.147+-only addition). Vellum's provider only ever receives one
        // because Codex's `supports_remote_compaction()` gate is keyed on the
        // provider's display *name* being `"OpenAI"` (see
        // `write_vellum_provider` in `codex.rs`) — third-party providers
        // otherwise never see this item at all and Codex falls back to
        // inline local compaction Vellum never observes. Official routes
        // must keep using the native `/responses` path unchanged: the
        // ChatGPT Codex backend does not expose `/responses/compact` and
        // returns 404 when the trigger is rewritten to that public API path.
        if local_compaction_trigger {
            return self
                .execute_local_compaction_trigger(route, request, &execution_body, &replay)
                .await;
        }
        // Codex Core is the sole auto-compact scheduler: it decides when to
        // compact using the catalog's `auto_compact_token_limit` and sends a
        // real `compaction_trigger` (handled above) when it does. An
        // oversized ordinary turn is never rewritten or short-circuited here
        // — Vellum only ever responds to a genuine compact task Codex itself
        // initiated (see docs/protocol-source-of-truth.md).
        // Resolve the authorization posture exactly once. Dispatch consumes
        // this value ??it never re-reads the credential or OAuth provider, so
        // a rotation mid-request can never split one request across two
        // secrets (request-level TOCTOU, the M3A shape made possible).
        let mut auth = match resolved_official_auth.take() {
            Some(auth) => auth,
            None => {
                ResolvedAuth::resolve(
                    route,
                    request,
                    self.credentials.as_ref(),
                    self.official_auth.as_ref(),
                    self.grok_sessions.as_ref(),
                    Some(self.history.as_ref()),
                )
                .await?
            }
        };
        stages.mark("authorized");
        // The route fields the translation reads, projected out so adapter
        // code cannot be tempted to treat the full route as dispatchable.
        let adapter_route = ProfileAdapterRoute::from(route);
        let prepared_result = prepare_upstream_request_details(
            &execution_body,
            &adapter_route,
            &profile,
            &request.execution_environment,
            Some(&route_snapshot.catalog_entry),
            snapshot.web_search.enabled,
            &replay,
        )
        .map_err(map_prepare_error)?;
        let mut chat_diagnostics = prepared_result.chat_diagnostics;
        let mut prepared = prepared_result.body;
        if route.effective_provider_profile().is_some() {
            crate::opencode::strip_openai_only_fields(&mut prepared);
        }
        let mut encoded_body = serde_json::to_vec(&prepared)
            .map_err(|error| RuntimeError::Internal(format!("encode upstream body: {error}")))?;
        if let Some(diagnostics) = chat_diagnostics.as_mut() {
            diagnostics.body_bytes = Some(encoded_body.len() as u64);
            diagnostics.model = Some(route.upstream_model.clone());
        }
        stages.mark("prepared");
        log::info!(
            "[Dispatch] {} model={} stream={} body_bytes={} stages={}",
            route.route_id,
            route.catalog_id,
            streaming,
            encoded_body.len(),
            stages.summary()
        );
        if streaming {
            return self
                .execute_streaming(
                    route,
                    request,
                    history_request_body.clone(),
                    execution_body,
                    execution_id,
                    conversation_key,
                    encoded_body,
                    auth,
                    started,
                    chat_diagnostics,
                )
                .await;
        }
        let mut response = match self
            .execute_upstream(
                UpstreamRequest {
                    method: "POST".into(),
                    url: upstream_endpoint(&route.base_url, route.wire),
                    headers: json_upstream_headers(&auth, route, &request.body, &encoded_body),
                    body: encoded_body.clone(),
                    timeout: non_streaming_timeout(route),
                    max_response_bytes: third_party_response_cap(route),
                },
                route,
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                record_chat_wire_diagnostics(
                    &self.diagnostics,
                    route,
                    &encoded_body,
                    &mut chat_diagnostics,
                    None,
                );
                return Err(error);
            }
        };
        record_chat_wire_diagnostics(
            &self.diagnostics,
            route,
            &encoded_body,
            &mut chat_diagnostics,
            Some(response.status),
        );
        // Vellum-managed Official tokens expire. On 401/403 refresh once and
        // retry the *same* prepared request with the new credential. The
        // refresh never re-runs `ResolvedAuth::resolve` and never falls back to
        // another auth kind, so the refresh-dedup semantics (compare the
        // rejected token against what's cached) come from the provider.
        if let ResolvedAuth::OfficialManaged {
            token,
            account_id,
            selection_revision,
            selection_verified,
        } = &auth
        {
            if matches!(response.status, 401 | 403) {
                let rejected = OfficialAuthorization {
                    access_token: token.clone(),
                    account_id: account_id.clone(),
                    selection_revision: *selection_revision,
                    selection_verified: *selection_verified,
                };
                match self
                    .official_auth
                    .refresh_after_rejection(&route.route_id, &rejected)
                    .await
                {
                    Ok(refreshed) if official_refresh_matches_snapshot(&rejected, &refreshed) => {
                        auth = ResolvedAuth::OfficialManaged {
                            token: refreshed.access_token,
                            account_id: refreshed.account_id,
                            selection_revision: refreshed.selection_revision,
                            selection_verified: refreshed.selection_verified,
                        };
                        response = self
                            .execute_upstream(
                                UpstreamRequest {
                                    method: "POST".into(),
                                    url: upstream_endpoint(&route.base_url, route.wire),
                                    headers: json_upstream_headers(
                                        &auth,
                                        route,
                                        &request.body,
                                        &encoded_body,
                                    ),
                                    body: encoded_body.clone(),
                                    timeout: non_streaming_timeout(route),
                                    max_response_bytes: third_party_response_cap(route),
                                },
                                route,
                            )
                            .await?;
                    }
                    Ok(_) => {
                        return Err(RuntimeError::AuthenticationFailed(
                            "official token refresh changed or failed to verify the execution account"
                                .into(),
                        ));
                    }
                    Err(message) => {
                        return Err(RuntimeError::AuthenticationFailed(format!(
                            "official token refresh failed for route `{}`: {message}",
                            route.route_id
                        )));
                    }
                }
            }
        }
        // ChatGPT's private Codex endpoint normally accepts the native Codex
        // request unchanged. Some backend revisions, however, explicitly
        // reject the otherwise-valid Responses `tool_choice` field. Preserve
        // the Official passthrough contract on the first attempt and apply a
        // one-field compatibility retry only after that precise rejection.
        if official_rejects_tool_choice(route, response.status, &response.body) {
            if let Some(fallback_body) = without_top_level_tool_choice(&encoded_body) {
                encoded_body = fallback_body;
                response = self
                    .execute_upstream(
                        UpstreamRequest {
                            method: "POST".into(),
                            url: upstream_endpoint(&route.base_url, route.wire),
                            headers: json_upstream_headers(
                                &auth,
                                route,
                                &request.body,
                                &encoded_body,
                            ),
                            body: encoded_body.clone(),
                            timeout: non_streaming_timeout(route),
                            max_response_bytes: third_party_response_cap(route),
                        },
                        route,
                    )
                    .await?;
            }
        }
        let normalized = decode_non_streaming_response(&request.body, route, response)?;
        self.record_subagent_spawn_requests(
            &request.metadata.request_id,
            execution_id,
            &normalized,
        );
        let continuation_realm = official_continuation_realm(route, &auth);
        // M4: durably record the *normalized* exchange (phase assigned,
        // ciphertext already stripped) before the client sees the response,
        // mirroring Desktop's `record_exchange_with_context`. A persistence
        // failure fails the request closed rather than silently losing
        // continuity for the next turn.
        record_exchange_with_official_compactions(
            self.history.as_ref(),
            &history_request_body,
            &normalized,
            &route.route_id,
            route.provider_kind == RuntimeProviderKind::Official,
            Some(&conversation_key),
            continuation_realm.as_deref(),
        )
        .map_err(RuntimeError::History)?;
        // M7: usage accounting mirrors Desktop's per-request ledger. Routed
        // through the same atomic terminal guard every streaming path uses
        // (`finish_terminal_once_with_record`), so a buffered success also
        // claims the terminal slot and releases the cancel socket instead of
        // writing `UsageRecord` unconditionally.
        let details = usage_details_from_response(&normalized);
        let (control_account_hash, execution_account_hash) =
            official_usage_identity(&auth, request);
        let selection_revision = official_selection_revision(&auth);
        let review_tokens = request
            .metadata
            .review_run_id
            .as_ref()
            .map(|_| details.input_tokens + details.output_tokens)
            .unwrap_or(0);
        self.finish_terminal_once_with_record(
            execution_id,
            "success",
            UsageRecord {
                route_id: route.route_id.clone(),
                provider: provider_name(route.provider_kind),
                model: route.upstream_model.clone(),
                input_tokens: details.input_tokens,
                output_tokens: details.output_tokens,
                cached_input_tokens: details.cached_input_tokens,
                reasoning_tokens: details.reasoning_tokens,
                tool_calls: details.tool_calls,
                compaction_tokens: details.compaction_tokens,
                review_tokens,
                status: 200,
                error: None,
                duration_ms: started.elapsed().as_millis() as u64,
                first_byte_ms: None,
                review_run_id: request.metadata.review_run_id.clone(),
                review_role: request.metadata.review_role.clone(),
                review_reason: request.metadata.primary_failure_reason.clone(),
                request_id: Some(request.metadata.request_id.clone()),
                connection_id: request.metadata.connection_id.clone(),
                conversation_identity: Some(conversation_key.clone()),
                // Previously left unset (`None`) on this path: a reader could
                // not tell a genuinely successful non-streaming exchange
                // apart from one where outcome simply was never recorded.
                // Any consumer that joins a child request's own row to
                // determine whether a spawn actually succeeded (rather than
                // merely observing that the parent got *a* tool result back)
                // needs this to be explicit.
                outcome: Some("success".into()),
                control_account_hash,
                execution_account_hash,
                selection_revision,
                auth_mode: opencode_usage_fields(route, None).0,
                provider_profile: opencode_usage_fields(route, None).1,
                upstream_attempted: opencode_usage_fields(route, None).2,
                retry_after: None,
                ..Default::default()
            },
        );
        Ok(RuntimeResponse::Json(normalized))
    }

    /// Execute a `/responses/compact` request (M6). Official routes forward
    /// the compact transparently upstream; third-party routes materialize a
    /// local canonical checkpoint and answer `{"output": [compaction item]}`
    /// without calling an OpenAI-style compact endpoint upstream (Desktop
    /// parity, issue #4).
    pub async fn execute_compact(
        &self,
        request: RuntimeRequest,
    ) -> Result<RuntimeResponse, RuntimeError> {
        let _guard = self.begin_lifecycle()?;
        let snapshot = RuntimeSnapshot::capture_from(self.catalog.as_ref());
        let model = request_model(&request)?;
        let route_snapshot = snapshot
            .resolve_route_snapshot(model)
            .ok_or_else(|| RuntimeError::RouteNotFound(format!("model not configured: {model}")))?;
        let route = &route_snapshot.route;
        if route.provider_kind == RuntimeProviderKind::Official {
            return self.forward_official_compact(route, &request).await;
        }
        let mut input = request
            .body
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| {
                RuntimeError::Compaction("compact request is missing input history".into())
            })?;
        // A prior compaction replaces the client transcript with the durable
        // canonical window (Desktop `normalize_compact_request_from_durable_checkpoint`).
        let previous_response_id = request
            .body
            .get("previous_response_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
        if let Some(previous) = previous_response_id.as_deref() {
            if let Some(replay) = self
                .history
                .compacted_replay_with_provenance(previous)
                .map_err(|error| {
                    RuntimeError::History(format!("compacted replay lookup failed: {error}"))
                })?
            {
                if replay.items.is_empty() {
                    return Err(RuntimeError::Compaction(format!(
                        "durable compaction replay for {previous} is empty"
                    )));
                }
                input = replay.items;
            }
        }
        let items_before = input.len() as u64;
        let tokens_before = estimate_tokens(&input);

        // Production compaction is resolved from the route, not from any
        // per-session or per-route selector: Official never reaches here, and
        // every third-party route uses the embedded Codex 0.150 engine.
        let resolved_engine = crate::compaction_engine::resolve_compaction_engine(route);
        if !resolved_engine.is_vellum_local() {
            return Err(RuntimeError::UnsupportedMode(
                "official routes compact natively and must not reach the local trigger".into(),
            ));
        }

        // A task still carrying retired Canonical markers cannot be continued:
        // the checkpoint schema those markers refer to no longer exists, and
        // dropping them to carry on would resume from a history that never
        // happened.
        if let Err(code) = crate::replay::reject_legacy_canonical_history(&input) {
            return Err(RuntimeError::Compaction(code.into()));
        }

        let codex_local_started = std::time::Instant::now();
        // Sanitize before summarizing so no provider-private material
        // (Official ciphertext, reasoning content, credentials, provider
        // metadata) crosses to another provider.
        let sanitized_source = sanitize_portable_history(&input);
        let (summary, context_retreats) = self
            .request_codex_local_v0_150_summary(route, &request, &sanitized_source)
            .await?;
        let items = codex_local_v0_150::build_replacement_items(&sanitized_source, &summary);
        let checkpoint_hash = crate::history::hash_items(&items);
        let schema_version = crate::history::LOCAL_COMPACTION_RECORD_SCHEMA_V1;

        if items.is_empty() {
            return Err(RuntimeError::Compaction(
                "canonical compact produced an empty window".into(),
            ));
        }
        let compaction_id = new_local_compaction_id()?;
        // Journal the checkpoint with complete source_items and audit records
        let (generation, owner_response_id) =
            if let Some(previous) = previous_response_id.as_deref() {
                let parent = self
                    .history
                    .compacted_replay_parent(previous)
                    .map_err(RuntimeError::History)?
                    .or_else(|| {
                        self.history
                            .compaction_ids_for_response(previous)
                            .ok()
                            .and_then(|ids| ids.last().cloned())
                    });
                let parent_generation = parent
                    .as_deref()
                    .and_then(|id| self.history.compaction_generation(id).ok().flatten())
                    .unwrap_or(0);
                (Some(parent_generation + 1), Some(previous.to_string()))
            } else {
                (Some(1), Some(compaction_id.clone()))
            };

        if let (Some(gen), Some(owner)) = (generation, owner_response_id) {
            let sanitized_source = sanitize_portable_history(&input);
            let source_hash = crate::history::hash_items(&sanitized_source);
            let record_result = self
                .history
                .record_local_compaction(LocalCompactionRecordV1 {
                    response_id: owner,
                    compaction_id: compaction_id.clone(),
                    generation: gen,
                    engine_id: resolved_engine.id().to_string(),
                    engine_provenance: resolved_engine.provenance().map(str::to_string),
                    route_id: route.route_id.clone(),
                    upstream_model: route.upstream_model.clone(),
                    tokens_before,
                    tokens_after: estimate_tokens(&items),
                    elapsed_ms: codex_local_started.elapsed().as_millis() as u64,
                    schema_version,
                    source_items: sanitized_source.clone(),
                    replacement_items: items.clone(),
                    source_hash,
                    replacement_hash: checkpoint_hash.clone(),
                    failure: None,
                    created_at: chrono::Utc::now().timestamp(),
                });

            if let Err(err) = record_result {
                return Err(RuntimeError::History(err));
            }
        }

        // Engine-neutral observability: an eval reads this to confirm the
        // engine that actually ran, written from the resolved engine rather
        // than from anything the request asked for.
        self.record_diagnostic(DiagnosticEvent::LocalCompactionAttempt(
            crate::diagnostics::LocalCompactionAttempt {
                engine_id: resolved_engine.id().to_string(),
                engine_provenance: resolved_engine.provenance().map(str::to_string),
                route_id: route.route_id.clone(),
                upstream_model: route.upstream_model.clone(),
                trigger_origin:
                    crate::diagnostics::CompactionTriggerOrigin::ClientCompactionRequest,
                request_id_hash: Some(sha256_hex(&request.metadata.request_id)),
                source_hash: crate::history::hash_items(&sanitized_source),
                replacement_hash: Some(checkpoint_hash.clone()),
                generation: generation.unwrap_or(1),
                items_before,
                items_after: items.len() as u64,
                tokens_before,
                tokens_after: estimate_tokens(&items),
                elapsed_ms: codex_local_started.elapsed().as_millis() as u64,
                context_retreats,
                failure: None,
                bounded_failure_diagnostic: None,
            },
        ));

        self.record_diagnostic(DiagnosticEvent::CompactionDecision(CompactionDecision {
            engine: CompactionEngine::LocalTrigger,
            outcome: CompactionOutcome::Compacted,
            window: route.context_window.unwrap_or(128_000),
            active_tokens: 0,
            pending_user_tokens: 0,
            pending_tool_tokens: 0,
            output_reserve: route.compaction_policy.output_reserve_tokens,
            tool_reserve: route.compaction_policy.tool_reserve_tokens,
            threshold_percent: route.compaction_policy.threshold_percent,
            items_before: Some(items_before),
            items_after: Some(items.len() as u64),
            tokens_before: Some(tokens_before),
            tokens_after: Some(estimate_tokens(&items)),
            checkpoint_hash: Some(checkpoint_hash),
            checkpoint_id: generation.map(|_| compaction_id.clone()),
            generation,
            reason: Some("standalone_compact".into()),
        }));
        Ok(RuntimeResponse::Json(json!({
            "output": [local_compaction_item(&compaction_id)]
        })))
    }

    /// One Codex Local Compact 0.150 summarizer attempt against the session's
    /// own route, model, auth, and verified reasoning effort.
    ///
    /// Upstream sends the conversation as real input items followed by the
    /// fixed compaction prompt — not a stringified transcript — so this builds
    /// a Responses-shaped body and lowers it through the existing Chat adapter
    /// when the route's wire is Chat. A Chat provider is never sent a
    /// Responses body.
    async fn codex_local_v0_150_summary_attempt(
        &self,
        route: &RuntimeModelRoute,
        request: &RuntimeRequest,
        input: &[Value],
    ) -> Result<String, CodexLocalAttemptError> {
        let mut responses_body = json!({
            "model": route.upstream_model,
            "input": input,
            "stream": false,
            "max_output_tokens": 8192
        });
        apply_verified_reasoning_effort(&mut responses_body, route);

        let body = match route.wire {
            RuntimeWireFormat::Responses => {
                // The local summarizer is an auxiliary request, but it still
                // crosses the same third-party protocol boundary as an
                // ordinary turn.  Normalize Codex-private history variants
                // (notably custom_tool_call/output) into the public Responses
                // dialect before dispatch.  Bypassing this step made strict
                // compatible endpoints reject otherwise portable history.
                let profile = HarnessProfile::generic(RuntimeWireFormat::Responses);
                let normalized = match route.provider_kind {
                    RuntimeProviderKind::GrokCli => {
                        crate::profile_adapter::normalize_grok_responses_request(
                            &mut responses_body,
                            &profile,
                            false,
                        )
                    }
                    RuntimeProviderKind::OpenAiCompatible => {
                        crate::profile_adapter::normalize_compatible_responses_request(
                            &mut responses_body,
                            &profile,
                            false,
                        )
                    }
                    RuntimeProviderKind::Official => Ok(()),
                };
                normalized.map_err(|error| {
                    CodexLocalAttemptError::Fatal(RuntimeError::Compaction(format!(
                        "codex_local_v0_150: cannot normalize summarizer body: {error}"
                    )))
                })?;
                if route.effective_provider_profile().is_some() {
                    crate::opencode::strip_openai_only_fields(&mut responses_body);
                }
                responses_body
            }
            RuntimeWireFormat::Chat => crate::profile_adapter::responses_to_chat_with_profile(
                &responses_body,
                &HarnessProfile::generic(RuntimeWireFormat::Chat),
            )
            .map_err(|error| {
                CodexLocalAttemptError::Fatal(RuntimeError::Compaction(format!(
                    "codex_local_v0_150: cannot lower summarizer body to chat wire: {error}"
                )))
            })?,
        };

        let encoded_body = serde_json::to_vec(&body).map_err(|error| {
            CodexLocalAttemptError::Fatal(RuntimeError::Internal(format!(
                "encode codex_local_v0_150 summary body: {error}"
            )))
        })?;

        // Auxiliary call on the shared session: `resolve_for_compaction` keeps
        // a Grok route on its existing conversation without advancing the turn
        // index, which an ordinary turn resolution would do.
        let auth = ResolvedAuth::resolve_for_compaction(
            route,
            request,
            self.credentials.as_ref(),
            self.official_auth.as_ref(),
            self.grok_sessions.as_ref(),
            Some(self.history.as_ref()),
            &body,
        )
        .await
        .map_err(CodexLocalAttemptError::Fatal)?;

        let started = std::time::Instant::now();
        let response = self
            .execute_upstream(
                UpstreamRequest {
                    method: "POST".into(),
                    url: upstream_endpoint(&route.base_url, route.wire),
                    headers: json_upstream_headers(&auth, route, &request.body, &encoded_body),
                    body: encoded_body,
                    timeout: non_streaming_timeout(route),
                    max_response_bytes: third_party_response_cap(route),
                },
                route,
            )
            .await
            .map_err(|error| match &error {
                RuntimeError::ProviderUnavailable(message) if message.contains("took too long") => {
                    CodexLocalAttemptError::Retryable(error.clone())
                }
                RuntimeError::ProviderUnavailable(_) => {
                    CodexLocalAttemptError::Retryable(error.clone())
                }
                _ => CodexLocalAttemptError::Fatal(error.clone()),
            })?;

        if !(200..300).contains(&response.status) {
            let diagnostic = bounded_provider_diagnostic(&response.body);
            if is_context_window_error(response.status, &diagnostic) {
                return Err(CodexLocalAttemptError::ContextWindow);
            }
            let message = format!(
                "codex_local_v0_150 summary request failed: HTTP {}{}",
                response.status,
                if diagnostic.is_empty() {
                    String::new()
                } else {
                    format!("; upstream: {diagnostic}")
                }
            );
            // Quota and auth failures are reported as-is; compaction never
            // silently retargets another model to escape them.
            return Err(if matches!(response.status, 429) {
                CodexLocalAttemptError::Fatal(RuntimeError::Compaction(message))
            } else if (500..600).contains(&response.status) {
                CodexLocalAttemptError::Retryable(RuntimeError::Compaction(message))
            } else {
                CodexLocalAttemptError::Fatal(RuntimeError::Compaction(message))
            });
        }

        let value: Value = serde_json::from_slice(&response.body).map_err(|error| {
            CodexLocalAttemptError::Fatal(RuntimeError::Compaction(format!(
                "codex_local_v0_150 summary response was not JSON: {error}"
            )))
        })?;

        let details = usage_details_from_response(&value);
        let _ = self.usage.record(&UsageRecord {
            route_id: route.route_id.clone(),
            provider: provider_name(route.provider_kind),
            model: route.upstream_model.clone(),
            input_tokens: details.input_tokens,
            output_tokens: details.output_tokens,
            cached_input_tokens: details.cached_input_tokens,
            reasoning_tokens: details.reasoning_tokens,
            tool_calls: details.tool_calls,
            compaction_tokens: details.input_tokens + details.output_tokens,
            review_tokens: 0,
            status: response.status,
            error: None,
            duration_ms: started.elapsed().as_millis() as u64,
            first_byte_ms: None,
            review_run_id: request.metadata.review_run_id.clone(),
            review_role: None,
            review_reason: Some("compaction".into()),
            ..Default::default()
        });

        // Upstream uses the last assistant message of the summarizer turn
        // verbatim as the summary body. Nothing is parsed out of it.
        let text = summary_response_text(&value, route.wire);
        if text.trim().is_empty() {
            return Err(CodexLocalAttemptError::EmptySummary);
        }
        Ok(text)
    }

    /// Codex Local Compact 0.150 summarizer with upstream's context-window
    /// retreat: on a context-window rejection the **oldest** summarizer input
    /// item is dropped and the call retried, preserving the prefix cache. The
    /// caller's source record is never rewritten — only this local copy of the
    /// summarizer input shrinks.
    async fn request_codex_local_v0_150_summary(
        &self,
        route: &RuntimeModelRoute,
        request: &RuntimeRequest,
        sanitized_history: &[Value],
    ) -> Result<(String, u32), RuntimeError> {
        let mut input = codex_local_v0_150::build_summarizer_input(sanitized_history);
        let mut retries = 0u32;
        let mut retreats = 0u32;
        let max_retries = 3u32;
        loop {
            match self
                .codex_local_v0_150_summary_attempt(route, request, &input)
                .await
            {
                Ok(text) => return Ok((text, retreats)),
                Err(CodexLocalAttemptError::ContextWindow) => {
                    if codex_local_v0_150::remove_oldest_summarizer_item(&mut input) {
                        retreats = retreats.saturating_add(1);
                        // Upstream resets the retry budget after a successful
                        // retreat, because this is progress rather than a
                        // transient failure.
                        retries = 0;
                        continue;
                    }
                    return Err(RuntimeError::Compaction(format!(
                        "codex_local_v0_150 compaction failed: {}",
                        codex_local_v0_150::CompactionFailure::ContextExhausted.code()
                    )));
                }
                Err(CodexLocalAttemptError::EmptySummary) => {
                    return Err(RuntimeError::Compaction(format!(
                        "codex_local_v0_150 compaction failed: {}",
                        codex_local_v0_150::CompactionFailure::EmptySummary.code()
                    )));
                }
                Err(CodexLocalAttemptError::Retryable(error)) => {
                    if retries < max_retries {
                        retries += 1;
                        continue;
                    }
                    return Err(RuntimeError::Compaction(format!(
                        "codex_local_v0_150 compaction failed: {} after {max_retries} retries: {error}",
                        codex_local_v0_150::CompactionFailure::RetryExhausted(String::new()).code()
                    )));
                }
                Err(CodexLocalAttemptError::Fatal(error)) => return Err(error),
            }
        }
    }

    async fn execute_local_compaction_trigger(
        &self,
        route: &RuntimeModelRoute,
        request: &RuntimeRequest,
        hydrated_body: &Value,
        _replay: &ReplayContext,
    ) -> Result<RuntimeResponse, RuntimeError> {
        // P0-2: Separate raw unpruned input from visible mutable input
        let raw_input = hydrated_body
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| {
                RuntimeError::Compaction("compact trigger is missing input history".into())
            })?;
        let _visible_input = raw_input.clone();
        let items_before = raw_input.len() as u64;
        let tokens_before = estimate_tokens(&raw_input);

        // Production compaction is resolved from the route, not from any
        // per-session or per-route selector: Official never reaches here, and
        // every third-party route uses the embedded Codex 0.150 engine.
        let resolved_engine = crate::compaction_engine::resolve_compaction_engine(route);
        if !resolved_engine.is_vellum_local() {
            return Err(RuntimeError::UnsupportedMode(
                "official routes compact natively and must not reach the local trigger".into(),
            ));
        }

        // A task still carrying retired Canonical markers cannot be continued:
        // the checkpoint schema those markers refer to no longer exists, and
        // dropping them to carry on would resume from a history that never
        // happened.
        if let Err(code) = crate::replay::reject_legacy_canonical_history(&raw_input) {
            return Err(RuntimeError::Compaction(code.into()));
        }

        let codex_local_started = std::time::Instant::now();
        // Sanitize before summarizing so no provider-private material
        // (Official ciphertext, reasoning content, credentials, provider
        // metadata) crosses to another provider.
        let sanitized_source = sanitize_portable_history(&raw_input);
        let (summary, context_retreats) = self
            .request_codex_local_v0_150_summary(route, request, &sanitized_source)
            .await?;
        let items = codex_local_v0_150::build_replacement_items(&sanitized_source, &summary);
        let checkpoint_hash = crate::history::hash_items(&items);
        let schema_version = crate::history::LOCAL_COMPACTION_RECORD_SCHEMA_V1;

        let items_after = items.len() as u64;
        let tokens_after = estimate_tokens(&items);
        let compaction_id = new_local_compaction_id()?;
        let (previous_id, generation) = if let Some(previous) = request
            .body
            .get("previous_response_id")
            .and_then(Value::as_str)
        {
            let parent = self
                .history
                .compacted_replay_parent(previous)
                .map_err(RuntimeError::History)?
                .or_else(|| {
                    self.history
                        .compaction_ids_for_response(previous)
                        .ok()
                        .and_then(|ids| ids.last().cloned())
                });
            let gen = parent
                .as_deref()
                .and_then(|id| self.history.compaction_generation(id).ok().flatten())
                .unwrap_or(0)
                + 1;
            (previous.to_string(), gen)
        } else {
            (compaction_id.clone(), 1)
        };

        // P0-2: Preserve raw unpruned items in source_items for full auditability
        let sanitized_source = sanitize_portable_history(&raw_input);
        let source_hash = crate::history::hash_items(&sanitized_source);
        let record_result = self
            .history
            .record_local_compaction(LocalCompactionRecordV1 {
                response_id: previous_id,
                compaction_id: compaction_id.clone(),
                generation,
                engine_id: resolved_engine.id().to_string(),
                engine_provenance: resolved_engine.provenance().map(str::to_string),
                route_id: route.route_id.clone(),
                upstream_model: route.upstream_model.clone(),
                tokens_before,
                tokens_after,
                elapsed_ms: codex_local_started.elapsed().as_millis() as u64,
                schema_version,
                source_items: sanitized_source.clone(),
                replacement_items: items.clone(),
                source_hash,
                replacement_hash: checkpoint_hash.clone(),
                failure: None,
                created_at: chrono::Utc::now().timestamp(),
            });

        if let Err(err) = record_result {
            return Err(RuntimeError::History(err));
        }

        // Engine-neutral observability: an eval reads this to confirm the
        // engine that actually ran, written from the resolved engine rather
        // than from anything the request asked for.
        self.record_diagnostic(DiagnosticEvent::LocalCompactionAttempt(
            crate::diagnostics::LocalCompactionAttempt {
                engine_id: resolved_engine.id().to_string(),
                engine_provenance: resolved_engine.provenance().map(str::to_string),
                route_id: route.route_id.clone(),
                upstream_model: route.upstream_model.clone(),
                trigger_origin:
                    crate::diagnostics::CompactionTriggerOrigin::ClientCompactionRequest,
                request_id_hash: Some(sha256_hex(&request.metadata.request_id)),
                source_hash: crate::history::hash_items(&sanitized_source),
                replacement_hash: Some(checkpoint_hash.clone()),
                generation,
                items_before,
                items_after,
                tokens_before,
                tokens_after,
                elapsed_ms: codex_local_started.elapsed().as_millis() as u64,
                context_retreats,
                failure: None,
                bounded_failure_diagnostic: None,
            },
        ));

        self.record_diagnostic(DiagnosticEvent::CompactionDecision(CompactionDecision {
            engine: CompactionEngine::LocalTrigger,
            outcome: CompactionOutcome::Compacted,
            window: route.context_window.unwrap_or(128_000),
            active_tokens: 0,
            pending_user_tokens: 0,
            pending_tool_tokens: 0,
            output_reserve: route.compaction_policy.output_reserve_tokens,
            tool_reserve: route.compaction_policy.tool_reserve_tokens,
            threshold_percent: route.compaction_policy.threshold_percent,
            items_before: Some(items_before),
            items_after: Some(items_after),
            tokens_before: Some(tokens_before),
            tokens_after: Some(tokens_after),
            checkpoint_hash: Some(checkpoint_hash),
            checkpoint_id: Some(compaction_id.clone()),
            generation: Some(generation),
            reason: Some("codex_trigger".into()),
        }));
        Ok(compaction_trigger_response(
            &request.body,
            local_compaction_item(&compaction_id),
        ))
    }

    /// Expand Vellum-owned compaction markers before dispatch. OpenAI-owned
    /// opaque compaction items must remain byte-for-byte intact on Official
    /// routes so the ChatGPT backend can resume them. A third-party route
    /// cannot interpret that ciphertext and continues to fail closed rather
    /// than silently dropping context.
    fn materialize_local_compactions(
        &self,
        provider_kind: RuntimeProviderKind,
        body: &mut Value,
        allow_foreign_for_local_trigger: bool,
        portable_replay: bool,
        replay: &mut ReplayContext,
    ) -> Result<(), RuntimeError> {
        let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
            return Ok(());
        };
        seed_replay_identities(input.len(), replay)?;
        let original: Vec<(Value, String)> = input
            .drain(..)
            .zip(std::mem::take(&mut replay.item_identities))
            .collect();
        let mut materialized = Vec::with_capacity(original.len());
        let mut identities = Vec::with_capacity(original.len());
        let mut prefix_len = replay.prefix_len;
        let mut suffix_start = replay.suffix_start;
        let mut suffix_end = replay.suffix_end;
        for (index, (item, identity)) in original.into_iter().enumerate() {
            let push_one = |item: Value,
                            identity: String,
                            materialized: &mut Vec<Value>,
                            identities: &mut Vec<String>| {
                materialized.push(item);
                identities.push(identity);
            };
            let apply_delta = |delta: isize,
                               prefix_len: &mut usize,
                               suffix_start: &mut usize,
                               suffix_end: &mut usize| {
                apply_range_delta(prefix_len, index, delta);
                apply_range_delta(suffix_start, index, delta);
                apply_range_delta(suffix_end, index, delta);
            };
            match crate::replay::classify_compaction_item(&item) {
                None => {
                    push_one(item, identity, &mut materialized, &mut identities);
                    continue;
                }
                Some(crate::replay::CompactionDisposition::ProvenClientLocal) => {
                    push_one(item, identity, &mut materialized, &mut identities);
                    continue;
                }
                Some(crate::replay::CompactionDisposition::VellumJournal)
                | Some(crate::replay::CompactionDisposition::ProviderOwnedOpaque)
                | Some(crate::replay::CompactionDisposition::Unknown) => {}
            }
            if item.get("type").and_then(Value::as_str) != Some("compaction") {
                push_one(item, identity, &mut materialized, &mut identities);
                continue;
            }
            let opaque = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // A retired Canonical marker is never reinterpreted as an
            // ordinary item and never silently dropped: the task is refused so
            // the user starts a new one rather than resuming from a history
            // Vellum can no longer reconstruct.
            if crate::replay::is_legacy_canonical_marker(&item) {
                return Err(RuntimeError::Compaction(
                    crate::replay::LEGACY_CANONICAL_UNSUPPORTED.into(),
                ));
            }
            let compaction_id = opaque.strip_prefix(LOCAL_COMPACTION_PREFIX).or_else(|| {
                item.get("id")
                    .and_then(Value::as_str)
                    .filter(|id| id.starts_with("cmp_local_") || id.starts_with("cmp_vellum_"))
            });
            let Some(compaction_id) = compaction_id else {
                if provider_kind == RuntimeProviderKind::Official {
                    push_one(item, identity, &mut materialized, &mut identities);
                    continue;
                }
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    // A record written by a retired engine describes a
                    // checkpoint schema that no longer exists. Installing its
                    // items would resume the conversation from a history
                    // Vellum can no longer reconstruct, so refuse by name.
                    if let Some(journal) = self
                        .history
                        .journal_record_for_compaction(id)
                        .map_err(RuntimeError::History)?
                    {
                        if journal.engine_id != codex_local_v0_150::ENGINE_ID {
                            return Err(RuntimeError::Compaction(
                                crate::replay::LEGACY_CANONICAL_UNSUPPORTED.into(),
                            ));
                        }
                    }
                    if let Some(items) = self
                        .history
                        .canonical_items_for_compaction(id)
                        .map_err(RuntimeError::History)?
                    {
                        if !items.is_empty() {
                            let count = items.len();
                            apply_delta(
                                count as isize - 1,
                                &mut prefix_len,
                                &mut suffix_start,
                                &mut suffix_end,
                            );
                            for (journal_index, journal_item) in items.into_iter().enumerate() {
                                materialized.push(journal_item);
                                identities.push(journal_item_identity(id, journal_index));
                            }
                            continue;
                        }
                    }
                }
                if allow_foreign_for_local_trigger {
                    push_one(item, identity, &mut materialized, &mut identities);
                    continue;
                }
                if portable_replay {
                    apply_delta(-1, &mut prefix_len, &mut suffix_start, &mut suffix_end);
                    continue;
                }
                return Err(RuntimeError::Compaction(
                    "unmaterialized foreign compaction item reached third-party runtime; refusing to drop context"
                        .into(),
                ));
            };
            let journal = self
                .history
                .journal_record_for_compaction(compaction_id)
                .map_err(RuntimeError::History)?
                .ok_or_else(|| {
                    RuntimeError::Compaction(format!(
                        "local compaction journal `{compaction_id}` is missing; refusing to drop context"
                    ))
                })?;
            if journal.engine_id != codex_local_v0_150::ENGINE_ID {
                return Err(RuntimeError::Compaction(
                    crate::replay::LEGACY_CANONICAL_UNSUPPORTED.into(),
                ));
            }
            if journal.schema_version != crate::history::LOCAL_COMPACTION_RECORD_SCHEMA_V1 {
                return Err(RuntimeError::Compaction(format!(
                    "local compaction journal `{compaction_id}` has unsupported record schema {}",
                    journal.schema_version
                )));
            }
            if journal.source_hash.is_empty()
                || crate::history::hash_items(&journal.source_items) != journal.source_hash
            {
                return Err(RuntimeError::Compaction(format!(
                    "local compaction journal `{compaction_id}` failed source hash validation"
                )));
            }
            if journal.checkpoint_hash.is_empty()
                || crate::history::hash_items(&journal.canonical_items) != journal.checkpoint_hash
            {
                return Err(RuntimeError::Compaction(format!(
                    "local compaction journal `{compaction_id}` failed replacement hash validation"
                )));
            }
            let items = journal.canonical_items;
            if items.is_empty() {
                return Err(RuntimeError::Compaction(format!(
                    "local compaction journal `{compaction_id}` is empty; refusing to drop context"
                )));
            }
            apply_delta(
                items.len() as isize - 1,
                &mut prefix_len,
                &mut suffix_start,
                &mut suffix_end,
            );
            for (journal_index, journal_item) in items.into_iter().enumerate() {
                materialized.push(journal_item);
                identities.push(journal_item_identity(compaction_id, journal_index));
            }
        }
        *input = materialized;
        replay.item_identities = identities;
        replay.prefix_len = prefix_len;
        replay.suffix_start = suffix_start;
        replay.suffix_end = suffix_end;
        assert_replay_aligned(input.len(), replay)
    }

    /// Official compact is a transparent upstream operation: forward the body
    /// as-is and return the upstream status/body (issue #4: never rewrite the
    /// Codex compact body). Retries 429/5xx like Desktop.
    async fn forward_official_compact(
        &self,
        route: &RuntimeModelRoute,
        request: &RuntimeRequest,
    ) -> Result<RuntimeResponse, RuntimeError> {
        let auth = ResolvedAuth::resolve(
            route,
            request,
            self.credentials.as_ref(),
            self.official_auth.as_ref(),
            self.grok_sessions.as_ref(),
            Some(self.history.as_ref()),
        )
        .await?;
        let encoded_body = serde_json::to_vec(&request.body)
            .map_err(|error| RuntimeError::Internal(format!("encode compact body: {error}")))?;
        let endpoint = format!("{}/responses/compact", route.base_url.trim_end_matches('/'));
        let mut last_error = String::new();
        for attempt in 0..3 {
            let response = self
                .transport
                .execute(&UpstreamRequest {
                    method: "POST".into(),
                    url: endpoint.clone(),
                    headers: json_upstream_headers(&auth, route, &request.body, &encoded_body),
                    body: encoded_body.clone(),
                    timeout: None,
                    max_response_bytes: None,
                })
                .await;
            match response {
                Ok(response) => {
                    let retryable =
                        (response.status == 429 || response.status >= 500) && attempt < 2;
                    if !retryable {
                        if (200..300).contains(&response.status) {
                            let body: Value =
                                serde_json::from_slice(&response.body).map_err(|error| {
                                    RuntimeError::Compaction(format!(
                                        "official compact returned a non-JSON body: {error}"
                                    ))
                                })?;
                            return Ok(RuntimeResponse::Json(body));
                        }
                        return Err(provider_error_for_status(
                            response.status,
                            &String::from_utf8_lossy(&response.body),
                        ));
                    }
                }
                Err(error) => last_error = map_transport_error(error).to_string(),
            }
            if attempt < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(250 * (attempt + 1) as u64))
                    .await;
            }
        }
        Err(RuntimeError::ProviderUnavailable(format!(
            "official compact upstream failed: {last_error}"
        )))
    }

    /// Execute a `/v1/alpha/search` request (M8). Official routes forward the
    /// search transparently upstream; third-party routes run the configured
    /// [`SearchEngine`] (fail-closed when none is configured, Desktop parity).
    pub async fn execute_search(
        &self,
        request: RuntimeRequest,
    ) -> Result<RuntimeResponse, RuntimeError> {
        let _guard = self.begin_lifecycle()?;
        let snapshot = RuntimeSnapshot::capture_from(self.catalog.as_ref());
        let model = request_model(&request)?;
        let route_snapshot = snapshot
            .resolve_route_snapshot(model)
            .ok_or_else(|| RuntimeError::RouteNotFound(format!("model not configured: {model}")))?;
        let route = &route_snapshot.route;
        if route.provider_kind == RuntimeProviderKind::Official {
            return self.forward_official_search(route, &request).await;
        }
        let search_request: crate::search::SearchRequest =
            serde_json::from_value(request.body.clone()).map_err(|error| {
                RuntimeError::InvalidRequest(format!("invalid Codex search request: {error}"))
            })?;
        match self.search_engine.run(&search_request).await {
            Ok(response) => Ok(RuntimeResponse::Json(
                serde_json::to_value(&response).unwrap_or_else(|_| json!({})),
            )),
            Err(message) => Err(RuntimeError::Search(message)),
        }
    }

    /// Official search is a transparent upstream operation: forward the body
    /// as-is and return the upstream status/body.
    async fn forward_official_search(
        &self,
        route: &RuntimeModelRoute,
        request: &RuntimeRequest,
    ) -> Result<RuntimeResponse, RuntimeError> {
        let mut auth = ResolvedAuth::resolve(
            route,
            request,
            self.credentials.as_ref(),
            self.official_auth.as_ref(),
            self.grok_sessions.as_ref(),
            Some(self.history.as_ref()),
        )
        .await?;
        let encoded_body = serde_json::to_vec(&request.body)
            .map_err(|error| RuntimeError::Internal(format!("encode search body: {error}")))?;
        let endpoint = alpha_search_endpoint(&route.base_url);
        let upstream_request = |auth: &ResolvedAuth| UpstreamRequest {
            method: "POST".into(),
            url: endpoint.clone(),
            headers: json_upstream_headers(auth, route, &request.body, &encoded_body),
            body: encoded_body.clone(),
            timeout: None,
            max_response_bytes: None,
        };
        let mut response = self
            .transport
            .execute(&upstream_request(&auth))
            .await
            .map_err(map_transport_error)?;
        if matches!(response.status, 401 | 403) {
            if let ResolvedAuth::OfficialManaged {
                token,
                account_id,
                selection_revision,
                selection_verified,
            } = &auth
            {
                let rejected = OfficialAuthorization {
                    access_token: token.clone(),
                    account_id: account_id.clone(),
                    selection_revision: *selection_revision,
                    selection_verified: *selection_verified,
                };
                if let Ok(refreshed) = self
                    .official_auth
                    .refresh_after_rejection(&route.route_id, &rejected)
                    .await
                {
                    if !official_refresh_matches_snapshot(&rejected, &refreshed) {
                        return Err(RuntimeError::AuthenticationFailed(
                            "official token refresh changed or failed to verify the execution account"
                                .into(),
                        ));
                    }
                    auth = ResolvedAuth::OfficialManaged {
                        token: refreshed.access_token,
                        account_id: refreshed.account_id,
                        selection_revision: refreshed.selection_revision,
                        selection_verified: refreshed.selection_verified,
                    };
                    response = self
                        .transport
                        .execute(&upstream_request(&auth))
                        .await
                        .map_err(map_transport_error)?;
                }
            }
        }
        let content_type = response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.clone());
        Ok(RuntimeResponse::Raw {
            status: response.status,
            content_type,
            body: response.body,
        })
    }

    /// Streaming dispatch (M5): the request body is already prepared and
    /// hydrated; open an upstream SSE exchange and translate it per dialect.
    /// `baseline` is the request-received instant (the same one server.rs
    /// uses for the downstream mark), so a stream's `first_byte_ms` and the
    /// downstream first-frame mark share one clock.
    #[allow(clippy::too_many_arguments)]
    async fn execute_streaming(
        &self,
        route: &RuntimeModelRoute,
        request: &RuntimeRequest,
        history_request_body: Value,
        dispatch_body: Value,
        execution_id: &str,
        conversation_key: String,
        mut encoded_body: Vec<u8>,
        mut auth: ResolvedAuth,
        baseline: std::time::Instant,
        mut chat_diagnostics: Option<crate::replay::HarnessTranscriptDiagnostics>,
    ) -> Result<RuntimeResponse, RuntimeError> {
        let upstream_request = UpstreamRequest {
            method: "POST".into(),
            url: upstream_endpoint(&route.base_url, route.wire),
            headers: json_upstream_headers(&auth, route, &request.body, &encoded_body),
            body: encoded_body.clone(),
            // Streaming has no overall deadline; the caller enforces
            // per-chunk idle timeouts.
            timeout: None,
            max_response_bytes: None,
        };
        tracing::info!(
            route_id = %route.route_id,
            catalog_id = %route.catalog_id,
            wire = ?route.wire,
            url = %upstream_request.url,
            body_bytes = encoded_body.len(),
            "opening upstream stream"
        );
        if let Err(error) = self.reject_opencode_cooldown(route) {
            record_chat_wire_diagnostics(
                &self.diagnostics,
                route,
                &encoded_body,
                &mut chat_diagnostics,
                None,
            );
            return Err(error);
        }
        let send_started = std::time::Instant::now();
        let mut upstream = match self.open_upstream_stream(&upstream_request).await {
            Ok(upstream) => upstream,
            Err(error) => {
                record_chat_wire_diagnostics(
                    &self.diagnostics,
                    route,
                    &encoded_body,
                    &mut chat_diagnostics,
                    None,
                );
                return Err(error);
            }
        };
        self.note_opencode_quota(route, upstream.status, &upstream.headers, &[]);
        log::info!(
            "[Dispatch] upstream headers route={} status={} send_ms={} body_bytes={}",
            route.route_id,
            upstream.status,
            send_started.elapsed().as_millis(),
            encoded_body.len()
        );
        // Vellum-managed Official tokens expire. On 401/403 refresh once and
        // re-open the stream with the new credential (Desktop parity).
        if let ResolvedAuth::OfficialManaged {
            token,
            account_id,
            selection_revision,
            selection_verified,
        } = &auth
        {
            if matches!(upstream.status, 401 | 403) {
                let rejected = OfficialAuthorization {
                    access_token: token.clone(),
                    account_id: account_id.clone(),
                    selection_revision: *selection_revision,
                    selection_verified: *selection_verified,
                };
                match self
                    .official_auth
                    .refresh_after_rejection(&route.route_id, &rejected)
                    .await
                {
                    Ok(refreshed) if official_refresh_matches_snapshot(&rejected, &refreshed) => {
                        auth = ResolvedAuth::OfficialManaged {
                            token: refreshed.access_token,
                            account_id: refreshed.account_id,
                            selection_revision: refreshed.selection_revision,
                            selection_verified: refreshed.selection_verified,
                        };
                        upstream = match self
                            .open_upstream_stream(&UpstreamRequest {
                                method: "POST".into(),
                                url: upstream_endpoint(&route.base_url, route.wire),
                                headers: json_upstream_headers(
                                    &auth,
                                    route,
                                    &request.body,
                                    &encoded_body,
                                ),
                                body: encoded_body.clone(),
                                timeout: None,
                                max_response_bytes: None,
                            })
                            .await
                        {
                            Ok(upstream) => upstream,
                            Err(error) => {
                                record_chat_wire_diagnostics(
                                    &self.diagnostics,
                                    route,
                                    &encoded_body,
                                    &mut chat_diagnostics,
                                    Some(upstream.status),
                                );
                                return Err(error);
                            }
                        };
                    }
                    Ok(_) => {
                        return Err(RuntimeError::AuthenticationFailed(
                            "official token refresh changed or failed to verify the execution account"
                                .into(),
                        ));
                    }
                    Err(message) => {
                        return Err(RuntimeError::AuthenticationFailed(format!(
                            "official token refresh failed for route `{}`: {message}",
                            route.route_id
                        )));
                    }
                }
            }
        }
        record_chat_wire_diagnostics(
            &self.diagnostics,
            route,
            &encoded_body,
            &mut chat_diagnostics,
            Some(upstream.status),
        );
        if !(200..300).contains(&upstream.status) {
            let status = upstream.status;
            let error_body = drain_upstream_error_body(&mut upstream).await?;
            if official_rejects_tool_choice(route, status, &error_body) {
                if let Some(fallback_body) = without_top_level_tool_choice(&encoded_body) {
                    encoded_body = fallback_body;
                    upstream = self
                        .open_upstream_stream(&UpstreamRequest {
                            method: "POST".into(),
                            url: upstream_endpoint(&route.base_url, route.wire),
                            headers: json_upstream_headers(
                                &auth,
                                route,
                                &request.body,
                                &encoded_body,
                            ),
                            body: encoded_body,
                            timeout: None,
                            max_response_bytes: None,
                        })
                        .await?;
                    if (200..300).contains(&upstream.status) {
                        // Continue into the normal Official stream path.
                    } else {
                        let retry_status = upstream.status;
                        let retry_body = drain_upstream_error_body(&mut upstream).await?;
                        return Err(provider_error_for_status(
                            retry_status,
                            &provider_error_message(&retry_body, retry_status),
                        ));
                    }
                } else {
                    return Err(provider_error_for_status(
                        status,
                        &provider_error_message(&error_body, status),
                    ));
                }
            } else {
                self.note_opencode_quota(route, status, &upstream.headers, &error_body);
                return Err(provider_error_from_upstream(
                    status,
                    &error_body,
                    &upstream.headers,
                ));
            }
        }
        // Persist the original incremental request plus any synthetic
        // investigation recovery item. Persisting the fully hydrated replay
        // would duplicate the continuation chain; persisting the untouched
        // client body would reset recovery count and re-inject forever.
        // `request_body` is what gets recorded; `dispatch_body` is what gets
        // sent. They are not interchangeable: the search-tool loop reopens the
        // upstream stream mid-turn and must resend the fully hydrated
        // continuation, not the incremental client body (a `web_search` hop
        // built from the latter drops every earlier turn).
        let request_body = history_request_body;
        let parent_request_id = request.metadata.request_id.clone();
        // Auto Review provenance (M9 real fix): the buffered path already
        // threads `request.metadata.review_run_id`/`review_role` into its
        // `UsageRecord`; the streaming generators below must carry the same
        // three fields so a guardian dispatch that happens to stream never
        // loses attribution (previously always `None` here, which is why
        // review stats could show a stale/legacy route as "active" once a
        // streaming reviewer model was in use).
        let guardian = GuardianProvenance {
            run_id: request.metadata.review_run_id.clone(),
            role: request.metadata.review_role.clone(),
            reason: request.metadata.primary_failure_reason.clone(),
        };
        let stream_slot = if route.provider_kind == RuntimeProviderKind::Official {
            None
        } else {
            Some(self.resource.try_acquire_third_party_stream()?)
        };
        let execution_id = execution_id.to_string();
        let (control_account_hash, execution_account_hash) =
            official_usage_identity(&auth, request);
        let selection_revision = official_selection_revision(&auth);
        let continuation_realm = official_continuation_realm(route, &auth);
        let agent_attribution = self.agent_attribution_for_execution(&execution_id);
        match route.wire {
            RuntimeWireFormat::Chat => {
                Ok(RuntimeResponse::Sse(inbound_sse_stream(self.chat_stream(
                    route,
                    &parent_request_id,
                    &execution_id,
                    request.metadata.connection_id.clone(),
                    request_body,
                    dispatch_body,
                    conversation_key,
                    request.execution_environment.clone(),
                    auth,
                    upstream,
                    stream_slot,
                    baseline,
                    guardian,
                    agent_attribution,
                ))))
            }
            RuntimeWireFormat::Responses => {
                if route.provider_kind == RuntimeProviderKind::Official {
                    Ok(RuntimeResponse::Sse(inbound_sse_stream(
                        self.official_stream(
                            route,
                            &parent_request_id,
                            &execution_id,
                            request_body,
                            conversation_key,
                            upstream,
                            control_account_hash,
                            execution_account_hash,
                            selection_revision,
                            continuation_realm,
                            baseline,
                            guardian,
                            agent_attribution,
                        ),
                    )))
                } else {
                    Ok(RuntimeResponse::Sse(inbound_sse_stream(
                        self.third_party_stream(
                            route,
                            &parent_request_id,
                            &execution_id,
                            request.metadata.connection_id.clone(),
                            request_body,
                            dispatch_body,
                            conversation_key,
                            request.execution_environment.clone(),
                            auth,
                            upstream,
                            stream_slot,
                            baseline,
                            guardian,
                            agent_attribution,
                        ),
                    )))
                }
            }
        }
    }

    /// Third-party Responses-wire stream: normalize every upstream block
    /// through [`ThirdPartySseNormalizer`], persist the completed exchange
    /// before the terminal frame is observable, and fail closed with a
    /// `response.failed` event on idle/disconnect/no-completion. Mirrors
    /// Desktop's `native_stream_response` third-party path.
    #[allow(clippy::too_many_arguments)]
    fn third_party_stream(
        &self,
        route: &RuntimeModelRoute,
        parent_request_id: &str,
        execution_id: &str,
        connection_id: Option<String>,
        request_body: Value,
        dispatch_body: Value,
        conversation_key: String,
        environment: crate::environment::ExecutionEnvironment,
        auth: ResolvedAuth,
        upstream: UpstreamStream,
        stream_slot: Option<ResourceGuard>,
        baseline: std::time::Instant,
        guardian: GuardianProvenance,
        agent_attribution: Option<crate::usage::AgentUsageAttribution>,
    ) -> impl futures_util::Stream<Item = Result<Bytes, RuntimeError>> + 'static {
        let history = self.history.clone();
        let usage = self.usage.clone();
        let parent_cancel = Arc::clone(&self.parent_cancel);
        let parent_request_id = parent_request_id.to_string();
        let execution_id = execution_id.to_string();
        let diagnostics = Arc::clone(&self.diagnostics);
        let pending_subagent_spawns = Arc::clone(&self.subagent_spawns);
        let search_engine = Arc::clone(&self.search_engine);
        let transport = Arc::clone(&self.transport);
        let opencode_quota = Arc::clone(&self.opencode_quota);
        let catalog = Arc::clone(&self.catalog);
        let harness_options = self.harness_options;
        let delegation_runtime_wired = self.delegation_runtime_wired;
        let web_search_enabled = self.web_search_wrapper_enabled;
        let search_tool_loop_limit = self.search_tool_loop_limit;
        let route = route.clone();
        let (auth_mode, provider_profile, upstream_attempted, _) =
            opencode_usage_fields(&route, None);
        let route_id = route.route_id.clone();
        let model = route.upstream_model.clone();
        let provider = provider_name(route.provider_kind);
        let route_streaming = route.streaming;
        let started = baseline;
        async_stream::stream! {
            let _stream_slot = stream_slot;
            let ok = |bytes: Bytes| Ok::<Bytes, RuntimeError>(bytes);
            let mut source = upstream.body;
            let mut request_body = request_body;
            // The hydrated body the first hop was built from. Search
            // continuation hops resend this one; `request_body` stays the
            // incremental client body that gets recorded into the chain.
            let mut dispatch_body = dispatch_body;
            let mut search_session = SearchLoopSession::new(web_search_enabled, search_tool_loop_limit);
            let mut text = String::new();
            let mut transcript = String::new();
            let mut remainder = Vec::new();
            let mut stream_error: Option<String> = None;
            let mut stream_error_category: Option<String> = None;
            let mut terminal_response_recorded = false;
            let mut first_byte_ms = None;
            let mut usage_recorded = false;
            let mut normalizer = ThirdPartySseNormalizer::new(&request_body, false);
            let mut delta_observer = crate::live_attribution::StreamDeltaObserver::new();
            let stream_requested =
                request_body.get("stream").and_then(Value::as_bool).unwrap_or(false);
            'hops: loop {
            'upstream: loop {
                let item = match tokio::time::timeout(THIRD_PARTY_STREAM_IDLE_TIMEOUT, source.next()).await {
                    Ok(Some(item)) => item,
                    Ok(None) => break,
                    Err(_) => {
                        let message = format!(
                            "Upstream stream produced no data for {} seconds",
                            THIRD_PARTY_STREAM_IDLE_TIMEOUT.as_secs()
                        );
                        stream_error = Some(message.clone());
                        yield ok(Bytes::from(failed_sse_event(&message)));
                        break;
                    }
                };
                match item {
                    Ok(bytes) => {
                        if first_byte_ms.is_none() {
                            first_byte_ms = Some(started.elapsed().as_millis() as u64);
                        }
                        append_utf8_safe(&mut text, &mut remainder, &bytes);
                        let mut parsed = text.clone();
                        let mut consumed = String::new();
                        loop {
                            let block = match take_limited_sse_block(&mut parsed) {
                                Ok(Some(block)) => block,
                                Ok(None) => break,
                                Err(limit) => {
                                    let error = upstream_limit_error(
                                        &provider,
                                        &route_id,
                                        limit.name(),
                                        limit.limit(),
                                        limit.bytes(),
                                    );
                                    let message = error.to_string();
                                    stream_error = Some(message.clone());
                                    let failed = failed_sse_event_with_category(
                                        error.category(),
                                        &message,
                                    );
                                    transcript.push_str(&failed);
                                    yield ok(Bytes::from(failed));
                                    break 'upstream;
                                }
                            };
                            consumed.push_str(&block);
                            consumed.push_str("\n\n");
                            let normalized_blocks = match normalizer.push_block(&block) {
                                Ok(blocks) => blocks,
                                Err(protocol) => {
                                    // A known protocol event with a shape the
                                    // protocol does not define. Never forwarded
                                    // as an opaque event: end the stream here.
                                    //
                                    // The response's status and headers were
                                    // committed before the first upstream block
                                    // was read, so the canonical category rides
                                    // on one bounded terminal event and the
                                    // stream then closes normally. Yielding a
                                    // transport error instead would truncate
                                    // the body with nothing to explain it.
                                    let message = protocol.to_string();
                                    stream_error = Some(message.clone());
                                    let failed = failed_sse_event_with_category(
                                        RuntimeError::ProviderProtocol(String::new()).category(),
                                        &message,
                                    );
                                    transcript.push_str(&failed);
                                    yield ok(Bytes::from(failed));
                                    break 'upstream;
                                }
                            };
                            if let Some(trigger) = normalizer.take_confident_doom_loop_trigger() {
                                let message = format!(
                                    "Grok server detected a repeating reasoning loop ({trigger}); \
                                     rejecting the poisoned turn so the client can retry"
                                );
                                stream_error = Some(message.clone());
                                let failed = failed_sse_event(&message);
                                transcript.push_str(&failed);
                                yield ok(Bytes::from(failed));
                                break 'upstream;
                            }
                            for normalized in normalized_blocks {
                                let blocks = match search_session.observe(&normalized) {
                                    SseAction::Forward => vec![normalized],
                                    SseAction::Drop => Vec::new(),
                                    SseAction::Replace(blocks) => blocks,
                                    SseAction::HoldCompleted => Vec::new(),
                                };
                                for normalized in blocks {
                                transcript.push_str(&normalized);
                                if let Some((kind, _)) = parse_sse_value(&normalized) {
                                    delta_observer.observe_responses_kind(
                                        &kind,
                                        started.elapsed().as_millis() as u64,
                                    );
                                }
                                let terminal_now = !terminal_response_recorded
                                    && sse_event_is_response_completed(&normalized);
                                if terminal_now {
                                    if let Some(mut response) =
                                        completed_response_from_sse(&transcript)
                                    {
                                        normalize_third_party_readable_reasoning(&mut response);
                                        assign_response_message_phases(&mut response);
                                        normalizer.restore_full_reasoning_for_history(&mut response);
                                        let status =
                                            if stream_error.is_none() { 200 } else { 502 };
                                        record_subagent_spawn_requests_in_sink(
                                            Arc::clone(&diagnostics),
                                            Arc::clone(&pending_subagent_spawns),
                                            &parent_request_id,
                                            &execution_id,
                                            &response,
                                        );
                                        let details = usage_details_from_response(&response);
                                        let stream_quality = delta_observer
                                            .classify(
                                                stream_requested,
                                                route_streaming,
                                                true,
                                                Some(started.elapsed().as_millis() as u64),
                                            )
                                            .as_token()
                                            .to_string();
                                        let outcome_str = if stream_error.is_none() {
                                            "success"
                                        } else {
                                            "provider_failure"
                                        };
                                        finish_terminal_once_with_record_raw(
                                            &parent_cancel,
                                            &diagnostics,
                                            &usage,
                                            &execution_id,
                                            outcome_str,
                                            UsageRecord {
                                            route_id: route_id.clone(),
                                            provider: provider.clone(),
                                            model: model.clone(),
                                            input_tokens: details.input_tokens,
                                            output_tokens: details.output_tokens,
                                            cached_input_tokens: details.cached_input_tokens,
                                            reasoning_tokens: details.reasoning_tokens,
                                            tool_calls: details.tool_calls,
                                            compaction_tokens: details.compaction_tokens,
                                            review_tokens: 0,
                                            status,
                                            error: stream_error.clone(),
                                            duration_ms: started.elapsed().as_millis() as u64,
                                            first_byte_ms,
                                            first_event_ms: first_byte_ms,
                                            first_downstream_frame_ms: None,
                                            stream_quality: Some(stream_quality),
                                            first_output_delta_ms: delta_observer
                                                .first_output_delta_ms,
                                            first_reasoning_delta_ms: delta_observer
                                                .first_reasoning_delta_ms,
                                            output_delta_count: delta_observer.output_delta_count,
                                            reasoning_delta_count: delta_observer
                                                .reasoning_delta_count,
                                            review_run_id: guardian.run_id.clone(),
                                            review_role: guardian.role.clone(),
                                            review_reason: guardian.reason.clone(),
                                            request_id: Some(parent_request_id.clone()),
                                            connection_id: connection_id.clone(),
                                            conversation_identity: Some(conversation_key.clone()),
                                            outcome: Some(outcome_str.to_string()),
                                            error_category: stream_error_category.clone(),
                                            auth_mode: auth_mode.clone(),
                                            provider_profile: provider_profile.clone(),
                                            upstream_attempted,
                                            agent_attribution: agent_attribution.clone(),
                                            ..Default::default()
                                            },
                                        );
                                        usage_recorded = true;
                                        match history.record_exchange_with_conversation_key(
                                            &request_body,
                                            &response,
                                            &route_id,
                                            Some(&conversation_key),
                                        ) {
                                            Ok(_) => {
                                                terminal_response_recorded = true;
                                                yield ok(Bytes::from(normalized));
                                            }
                                            Err(error) => {
                                                let message = format!(
                                                    "history durability failed: {error}"
                                                );
                                                stream_error = Some(message.clone());
                                                yield ok(Bytes::from(failed_sse_event(&message)));
                                            }
                                        }
                                        continue;
                                    }
                                }
                                yield ok(Bytes::from(normalized));
                                }
                            }
                        }
                        if !consumed.is_empty() {
                            text = parsed;
                        }
                    }
                    Err(error) => {
                        let message = format!(
                            "Upstream stream disconnected before completion: {error}"
                        );
                        stream_error = Some(message.clone());
                        yield ok(Bytes::from(failed_sse_event(&message)));
                        break;
                    }
                }
            }
            if search_session.can_loop() && stream_error.is_none() {
                let pending = search_session.pending().to_vec();
                match execute_pending_searches(search_engine.as_ref(), &pending).await {
                    Ok(outputs) => {
                        for pending_item in &pending {
                            for event in completed_search_events(pending_item) {
                                transcript.push_str(&event);
                                yield ok(Bytes::from(event));
                            }
                        }
                        // Both bodies take the search items: the dispatched
                        // one so the model sees the result it asked for, the
                        // recorded one so the next turn's hydration replays it.
                        append_search_outputs(&mut dispatch_body, &pending, &outputs);
                        append_search_outputs(&mut request_body, &pending, &outputs);
                        search_session.mark_looped();
                        match open_chat_continuation_stream(
                            transport.as_ref(),
                            catalog.as_ref(),
                            &route,
                            &auth,
                            &dispatch_body,
                            &environment,
                            harness_options,
                            delegation_runtime_wired,
                            web_search_enabled,
                            opencode_quota.as_ref(),
                        )
                        .await
                        {
                            Ok(next) => {
                                source = next.body;
                                text.clear();
                                remainder.clear();
                                continue 'hops;
                            }
                            Err(error) => {
                                stream_error_category = Some(error.category().to_string());
                                let message = error.to_string();
                                stream_error = Some(message.clone());
                                yield ok(Bytes::from(failed_sse_event_with_category(
                                    error.category(),
                                    &message,
                                )));
                            }
                        }
                    }
                    Err(error) => {
                        let message = format!("web_search execution failed: {error}");
                        let category = RuntimeError::Search(String::new()).category();
                        stream_error_category = Some(category.to_string());
                        stream_error = Some(message.clone());
                        yield ok(Bytes::from(failed_sse_event_with_category(category, &message)));
                    }
                }
            } else if search_session.loop_limit_exceeded() && stream_error.is_none() {
                let message = format!(
                    "web_search tool loop limit exceeded: the model kept calling \
                     web_search past {} hop(s) in one turn",
                    search_session.loop_limit()
                );
                stream_error_category = Some(
                    RuntimeError::ToolLoopLimit(String::new())
                        .category()
                        .to_string(),
                );
                stream_error = Some(message.clone());
                let failed = failed_sse_event_with_category(
                    RuntimeError::ToolLoopLimit(String::new()).category(),
                    &message,
                );
                transcript.push_str(&failed);
                yield ok(Bytes::from(failed));
            }
            break 'hops;
            }
            for normalized in normalizer.finish() {
                transcript.push_str(&normalized);
                yield ok(Bytes::from(normalized));
            }
            let completed = completed_response_from_sse(&transcript);
            let terminal_error = terminal_error_from_sse(&transcript);
            if completed.is_none() && terminal_error.is_none() && stream_error.is_none() {
                let message = "Upstream stream ended without response.completed".to_string();
                yield ok(Bytes::from(failed_sse_event(&message)));
            }
            if let Some(mut response) = completed {
                normalize_third_party_readable_reasoning(&mut response);
                assign_response_message_phases(&mut response);
                normalizer.restore_full_reasoning_for_history(&mut response);
                if !terminal_response_recorded {
                    let status = if stream_error.is_none() { 200 } else { 502 };
                    record_subagent_spawn_requests_in_sink(
                        Arc::clone(&diagnostics),
                        Arc::clone(&pending_subagent_spawns),
                        &parent_request_id,
                        &execution_id,
                        &response,
                    );
                    let details = usage_details_from_response(&response);
                    let stream_quality = delta_observer
                        .classify(
                            stream_requested,
                            route_streaming,
                            true,
                            Some(started.elapsed().as_millis() as u64),
                        )
                        .as_token()
                        .to_string();
                    let outcome_str = if stream_error.is_none() { "success" } else { "provider_failure" };
                    finish_terminal_once_with_record_raw(
                        &parent_cancel,
                        &diagnostics,
                        &usage,
                        &execution_id,
                        outcome_str,
                        UsageRecord {
                        route_id: route_id.clone(),
                        provider: provider.clone(),
                        model: model.clone(),
                        input_tokens: details.input_tokens,
                        output_tokens: details.output_tokens,
                        cached_input_tokens: details.cached_input_tokens,
                        reasoning_tokens: details.reasoning_tokens,
                        tool_calls: details.tool_calls,
                        compaction_tokens: details.compaction_tokens,
                        review_tokens: 0,
                        status,
                        error: stream_error.clone(),
                        duration_ms: started.elapsed().as_millis() as u64,
                        first_byte_ms,
                        stream_quality: Some(stream_quality),
                        first_output_delta_ms: delta_observer.first_output_delta_ms,
                        first_reasoning_delta_ms: delta_observer.first_reasoning_delta_ms,
                        output_delta_count: delta_observer.output_delta_count,
                        reasoning_delta_count: delta_observer.reasoning_delta_count,
                        review_run_id: guardian.run_id.clone(),
                        review_role: guardian.role.clone(),
                        review_reason: guardian.reason.clone(),
                        request_id: Some(parent_request_id.clone()),
                        connection_id: connection_id.clone(),
                        conversation_identity: Some(conversation_key.clone()),
                        outcome: Some(outcome_str.to_string()),
                        error_category: stream_error_category.clone(),
                        auth_mode: auth_mode.clone(),
                        provider_profile: provider_profile.clone(),
                        upstream_attempted,
                        agent_attribution: agent_attribution.clone(),
                        ..Default::default()
                        },
                    );
                    usage_recorded = true;
                    if let Err(error) = history.record_exchange_with_conversation_key(
                        &request_body,
                        &response,
                        &route_id,
                        Some(&conversation_key),
                    ) {
                        let message = format!("history durability failed: {error}");
                        yield ok(Bytes::from(failed_sse_event(&message)));
                    }
                }
            }
            if !usage_recorded {
                let outcome_str = if stream_error.is_none() { "success" } else { "provider_failure" };
                finish_terminal_once_with_record_raw(
                    &parent_cancel,
                    &diagnostics,
                    &usage,
                    &execution_id,
                    outcome_str,
                    UsageRecord {
                    route_id: route_id.clone(),
                    provider: provider.clone(),
                    model: model.clone(),
                    status: if stream_error.is_none() { 200 } else { 502 },
                    error: stream_error.clone(),
                    duration_ms: started.elapsed().as_millis() as u64,
                    first_byte_ms,
                    first_event_ms: first_byte_ms,
                    review_run_id: guardian.run_id.clone(),
                    review_role: guardian.role.clone(),
                    review_reason: guardian.reason.clone(),
                    request_id: Some(parent_request_id.clone()),
                    connection_id: connection_id.clone(),
                    conversation_identity: Some(conversation_key.clone()),
                    outcome: Some(outcome_str.to_string()),
                    error_category: stream_error_category.clone(),
                    auth_mode: auth_mode.clone(),
                    provider_profile: provider_profile.clone(),
                    upstream_attempted,
                    agent_attribution: agent_attribution.clone(),
                    ..Default::default()
                    },
                );
            }
        }
    }

    /// Chat-wire stream: translate each upstream block to Responses SSE via
    /// [`ChatSseAdapter`], honor `[DONE]` as the terminal marker, and persist
    /// the completed exchange before the terminal frame. Mirrors Desktop's
    /// `chat_stream_response`.
    #[allow(clippy::too_many_arguments)]
    fn chat_stream(
        &self,
        route: &RuntimeModelRoute,
        parent_request_id: &str,
        execution_id: &str,
        connection_id: Option<String>,
        request_body: Value,
        dispatch_body: Value,
        conversation_key: String,
        environment: crate::environment::ExecutionEnvironment,
        auth: ResolvedAuth,
        upstream: UpstreamStream,
        stream_slot: Option<ResourceGuard>,
        baseline: std::time::Instant,
        guardian: GuardianProvenance,
        agent_attribution: Option<crate::usage::AgentUsageAttribution>,
    ) -> impl futures_util::Stream<Item = Result<Bytes, RuntimeError>> + 'static {
        let history = self.history.clone();
        let usage = self.usage.clone();
        let parent_cancel = Arc::clone(&self.parent_cancel);
        let parent_request_id = parent_request_id.to_string();
        let execution_id = execution_id.to_string();
        let diagnostics = Arc::clone(&self.diagnostics);
        let pending_subagent_spawns = Arc::clone(&self.subagent_spawns);
        let search_engine = Arc::clone(&self.search_engine);
        let transport = Arc::clone(&self.transport);
        let opencode_quota = Arc::clone(&self.opencode_quota);
        let catalog = Arc::clone(&self.catalog);
        let harness_options = self.harness_options;
        let delegation_runtime_wired = self.delegation_runtime_wired;
        let web_search_enabled = self.web_search_wrapper_enabled;
        let search_tool_loop_limit = self.search_tool_loop_limit;
        let route = route.clone();
        let (auth_mode, provider_profile, upstream_attempted, _) =
            opencode_usage_fields(&route, None);
        let route_id = route.route_id.clone();
        let model = route.upstream_model.clone();
        let provider = provider_name(route.provider_kind);
        let route_streaming = route.streaming;
        let started = baseline;
        async_stream::stream! {
            let _stream_slot = stream_slot;
            let ok = |bytes: Bytes| Ok::<Bytes, RuntimeError>(bytes);
            let mut source = upstream.body;
            let mut request_body = request_body;
            // The hydrated body the first hop was built from. Search
            // continuation hops resend this one; `request_body` stays the
            // incremental client body that gets recorded into the chain.
            let mut dispatch_body = dispatch_body;
            let mut search_session = SearchLoopSession::new(web_search_enabled, search_tool_loop_limit);
            let mut utf8 = String::new();
            let mut remainder = Vec::new();
            let mut adapter = ChatSseAdapter::new_with_request(model.clone(), &request_body);
            let mut delta_observer = crate::live_attribution::StreamDeltaObserver::new();
            let stream_requested =
                request_body.get("stream").and_then(Value::as_bool).unwrap_or(false);
            let mut stream_error: Option<String> = None;
            let mut stream_error_category: Option<String> = None;
            let mut completed_seen = false;
            let mut terminal_response_recorded = false;
            let mut first_byte_ms = None;
            let mut usage_recorded = false;
            let mut auto_continue_retries = 0_u8;
            let mut hop_received_bytes: usize;
            'hops: loop {
            let guard_this_hop = auto_continue_retries < crate::auto_continue::MAX_AUTO_CONTINUE_RETRIES
                && crate::auto_continue::request_is_eligible(&dispatch_body);
            let mut guarded_events = Vec::<String>::new();
            let mut auto_continue_announcement = None::<String>;
            hop_received_bytes = 0;
            'upstream: loop {
                let item = match tokio::time::timeout(THIRD_PARTY_STREAM_IDLE_TIMEOUT, source.next()).await {
                    Ok(Some(item)) => item,
                    Ok(None) => break,
                    Err(_) => {
                        let message = format!(
                            "Upstream Chat stream produced no data for {} seconds",
                            THIRD_PARTY_STREAM_IDLE_TIMEOUT.as_secs()
                        );
                        stream_error = Some(message.clone());
                        yield ok(Bytes::from(failed_sse_event(&message)));
                        break;
                    }
                };
                match item {
                    Ok(bytes) => {
                        hop_received_bytes = hop_received_bytes.saturating_add(bytes.len());
                        if first_byte_ms.is_none() {
                            first_byte_ms = Some(started.elapsed().as_millis() as u64);
                        }
                        append_utf8_safe(&mut utf8, &mut remainder, &bytes);
                        loop {
                            let block = match take_limited_sse_block(&mut utf8) {
                                Ok(Some(block)) => block,
                                Ok(None) => break,
                                Err(limit) => {
                                    let error = upstream_limit_error(
                                        &provider,
                                        &route_id,
                                        limit.name(),
                                        limit.limit(),
                                        limit.bytes(),
                                    );
                                    let message = error.to_string();
                                    stream_error = Some(message.clone());
                                    yield ok(Bytes::from(failed_sse_event_with_category(
                                        error.category(),
                                        &message,
                                    )));
                                    break 'upstream;
                                }
                            };
                            let data = block
                                .lines()
                                .find_map(|line| strip_sse_field(line, "data"))
                                .unwrap_or("");
                            let upstream_done = data.trim() == "[DONE]";
                            delta_observer
                                .observe_chat_data(data, started.elapsed().as_millis() as u64);
                            let produced = adapter.push_data(data);
                            let events_to_process = if guard_this_hop {
                                guarded_events.extend(produced);
                                if guarded_events
                                    .iter()
                                    .any(|event| event.contains("event: response.completed"))
                                {
                                    let completed = adapter.history_response();
                                    if let Some(announcement) = crate::auto_continue::should_auto_continue(
                                        &dispatch_body,
                                        &completed,
                                        auto_continue_retries,
                                    ) {
                                        auto_continue_announcement = Some(announcement);
                                        guarded_events.clear();
                                        Vec::new()
                                    } else {
                                        std::mem::take(&mut guarded_events)
                                    }
                                } else {
                                    Vec::new()
                                }
                            } else {
                                produced
                            };
                            for event in events_to_process {
                                let events = match search_session.observe(&event) {
                                    SseAction::Forward => vec![event],
                                    SseAction::Drop => Vec::new(),
                                    SseAction::Replace(blocks) => blocks,
                                    SseAction::HoldCompleted => {
                                        completed_seen = false;
                                        Vec::new()
                                    }
                                };
                                for event in events {
                                if event.contains("event: response.completed") {
                                    completed_seen = true;
                                    if !terminal_response_recorded {
                                        let completed = adapter.history_response();
                                        match history.record_exchange_with_conversation_key(
                                            &request_body,
                                            &completed,
                                            &route_id,
                                            Some(&conversation_key),
                                        ) {
                                            Ok(_) => {
                                                terminal_response_recorded = true;
                                                yield ok(Bytes::from(event));
                                            }
                                            Err(error) => {
                                                let message = format!(
                                                    "history durability failed: {error}"
                                                );
                                                stream_error = Some(message.clone());
                                                yield ok(Bytes::from(failed_sse_event(&message)));
                                            }
                                        }
                                        continue;
                                    }
                                }
                                yield ok(Bytes::from(event));
                                }
                            }
                            // `[DONE]` is the protocol terminal marker. Some
                            // providers keep the HTTP body open afterwards.
                            if upstream_done {
                                break 'upstream;
                            }
                        }
                    }
                    Err(error) => {
                        let message = format!(
                            "Upstream Chat stream disconnected before completion: {error}"
                        );
                        stream_error = Some(message.clone());
                        yield ok(Bytes::from(failed_sse_event(&message)));
                        break;
                    }
                }
            }
            // Some compatible Chat servers omit `[DONE]` and close after a
            // non-null finish_reason. Finalize that hop before deciding whether
            // to search, retry a progress-only completion, or finish the turn.
            if !completed_seen
                && auto_continue_announcement.is_none()
                && stream_error.is_none()
            {
                let deferred = adapter.finish_if_terminal();
                let events_to_process = if guard_this_hop {
                    guarded_events.extend(deferred);
                    if guarded_events
                        .iter()
                        .any(|event| event.contains("event: response.completed"))
                    {
                        let completed = adapter.history_response();
                        if let Some(announcement) = crate::auto_continue::should_auto_continue(
                            &dispatch_body,
                            &completed,
                            auto_continue_retries,
                        ) {
                            auto_continue_announcement = Some(announcement);
                            guarded_events.clear();
                            Vec::new()
                        } else {
                            std::mem::take(&mut guarded_events)
                        }
                    } else {
                        Vec::new()
                    }
                } else {
                    deferred
                };
                for event in events_to_process {
                    let events = match search_session.observe(&event) {
                        SseAction::Forward => vec![event],
                        SseAction::Drop => Vec::new(),
                        SseAction::Replace(blocks) => blocks,
                        SseAction::HoldCompleted => {
                            completed_seen = false;
                            Vec::new()
                        }
                    };
                    for event in events {
                        if event.contains("event: response.completed") {
                            completed_seen = true;
                            if !terminal_response_recorded {
                                let completed = adapter.history_response();
                                match history.record_exchange_with_conversation_key(
                                    &request_body,
                                    &completed,
                                    &route_id,
                                    Some(&conversation_key),
                                ) {
                                    Ok(_) => {
                                        terminal_response_recorded = true;
                                        yield ok(Bytes::from(event));
                                    }
                                    Err(error) => {
                                        let message = format!("history durability failed: {error}");
                                        stream_error = Some(message.clone());
                                        yield ok(Bytes::from(failed_sse_event(&message)));
                                    }
                                }
                                continue;
                            }
                        }
                        yield ok(Bytes::from(event));
                    }
                }
            }
            // A few Chat gateways have returned HTTP 200 and then closed with
            // no SSE payload at all after a tool result. Codex retries that
            // opaque failure several times. Retry once here with an explicit
            // continuation instruction: it both bypasses a poisoned request
            // cache and gives the model a concrete recovery action. A second
            // empty response remains a provider-protocol failure.
            if !completed_seen
                && auto_continue_announcement.is_none()
                && stream_error.is_none()
                && hop_received_bytes == 0
                && guard_this_hop
            {
                auto_continue_announcement = Some(String::new());
            }
            if let Some(announcement) = auto_continue_announcement.take() {
                let retry_context = if announcement.is_empty() {
                    crate::auto_continue::append_empty_retry_context(&mut dispatch_body)
                } else {
                    crate::auto_continue::append_retry_context(&mut dispatch_body, &announcement)
                };
                if let Err(error) = retry_context {
                    stream_error_category = Some("internal_invariant".to_string());
                    stream_error = Some(error.clone());
                    yield ok(Bytes::from(failed_sse_event_with_category(
                        "internal_invariant",
                        &error,
                    )));
                    break 'hops;
                }
                auto_continue_retries = auto_continue_retries.saturating_add(1);
                tracing::warn!(
                    route_id = %route_id,
                    model = %model,
                    retry = auto_continue_retries,
                    empty = announcement.is_empty(),
                    "retrying incomplete Chat completion"
                );
                match open_chat_continuation_stream(
                    transport.as_ref(),
                    catalog.as_ref(),
                    &route,
                    &auth,
                    &dispatch_body,
                    &environment,
                    harness_options,
                    delegation_runtime_wired,
                    web_search_enabled,
                    opencode_quota.as_ref(),
                )
                .await
                {
                    Ok(next) => {
                        source = next.body;
                        utf8.clear();
                        remainder.clear();
                        adapter = ChatSseAdapter::new_with_request(model.clone(), &request_body);
                        continue 'hops;
                    }
                    Err(error) => {
                        stream_error_category = Some(error.category().to_string());
                        let message = error.to_string();
                        stream_error = Some(message.clone());
                        yield ok(Bytes::from(failed_sse_event_with_category(
                            error.category(),
                            &message,
                        )));
                        break 'hops;
                    }
                }
            }
            if search_session.can_loop() && stream_error.is_none() {
                let pending = search_session.pending().to_vec();
                match execute_pending_searches(search_engine.as_ref(), &pending).await {
                    Ok(outputs) => {
                        for pending_item in &pending {
                            for event in completed_search_events(pending_item) {
                                yield ok(Bytes::from(event));
                            }
                        }
                        // Both bodies take the search items: the dispatched
                        // one so the model sees the result it asked for, the
                        // recorded one so the next turn's hydration replays it.
                        append_search_outputs(&mut dispatch_body, &pending, &outputs);
                        append_search_outputs(&mut request_body, &pending, &outputs);
                        search_session.mark_looped();
                        match open_chat_continuation_stream(
                            transport.as_ref(),
                            catalog.as_ref(),
                            &route,
                            &auth,
                            &dispatch_body,
                            &environment,
                            harness_options,
                            delegation_runtime_wired,
                            web_search_enabled,
                            opencode_quota.as_ref(),
                        )
                        .await
                        {
                            Ok(next) => {
                                source = next.body;
                                utf8.clear();
                                remainder.clear();
                                adapter = ChatSseAdapter::new_with_request(
                                    model.clone(),
                                    &request_body,
                                );
                                continue 'hops;
                            }
                            Err(error) => {
                                stream_error_category = Some(error.category().to_string());
                                let message = error.to_string();
                                stream_error = Some(message.clone());
                                yield ok(Bytes::from(failed_sse_event_with_category(
                                    error.category(),
                                    &message,
                                )));
                            }
                        }
                    }
                    Err(error) => {
                        let message = format!("web_search execution failed: {error}");
                        let category = RuntimeError::Search(String::new()).category();
                        stream_error_category = Some(category.to_string());
                        stream_error = Some(message.clone());
                        yield ok(Bytes::from(failed_sse_event_with_category(category, &message)));
                    }
                }
            } else if search_session.loop_limit_exceeded() && stream_error.is_none() {
                let message = format!(
                    "web_search tool loop limit exceeded: the model kept calling \
                     web_search past {} hop(s) in one turn",
                    search_session.loop_limit()
                );
                stream_error_category = Some(
                    RuntimeError::ToolLoopLimit(String::new())
                        .category()
                        .to_string(),
                );
                stream_error = Some(message.clone());
                let failed = failed_sse_event_with_category(
                    RuntimeError::ToolLoopLimit(String::new()).category(),
                    &message,
                );
                yield ok(Bytes::from(failed));
            }
            break 'hops;
            }
            if !completed_seen && stream_error.is_none() {
                let message = if hop_received_bytes == 0 {
                    "Upstream Chat returned HTTP 200 with an empty body".to_string()
                } else {
                    format!(
                        "Upstream Chat stream ended after {hop_received_bytes} bytes without [DONE] or finish_reason"
                    )
                };
                let category = RuntimeError::ProviderProtocol(String::new()).category();
                stream_error_category = Some(category.to_string());
                stream_error = Some(message.clone());
                yield ok(Bytes::from(failed_sse_event_with_category(category, &message)));
            }
            let completed = adapter.history_response();
            if completed_seen && !terminal_response_recorded {
                let status = if stream_error.is_none() { 200 } else { 502 };
                record_subagent_spawn_requests_in_sink(
                    Arc::clone(&diagnostics),
                    Arc::clone(&pending_subagent_spawns),
                    &parent_request_id,
                    &execution_id,
                    &completed,
                );
                let details = usage_details_from_response(&completed);
                let stream_quality = delta_observer
                    .classify(
                        stream_requested,
                        route_streaming,
                        true,
                        Some(started.elapsed().as_millis() as u64),
                    )
                    .as_token()
                    .to_string();
                let outcome_str = if stream_error.is_none() { "success" } else { "provider_failure" };
                finish_terminal_once_with_record_raw(
                    &parent_cancel,
                    &diagnostics,
                    &usage,
                    &execution_id,
                    outcome_str,
                    UsageRecord {
                    route_id: route_id.clone(),
                    provider: provider.clone(),
                    model: model.clone(),
                    input_tokens: details.input_tokens,
                    output_tokens: details.output_tokens,
                    cached_input_tokens: details.cached_input_tokens,
                    reasoning_tokens: details.reasoning_tokens,
                    tool_calls: details.tool_calls,
                    compaction_tokens: details.compaction_tokens,
                    review_tokens: 0,
                    status,
                    error: stream_error.clone(),
                    duration_ms: started.elapsed().as_millis() as u64,
                    first_byte_ms,
                    stream_quality: Some(stream_quality),
                    first_output_delta_ms: delta_observer.first_output_delta_ms,
                    first_reasoning_delta_ms: delta_observer.first_reasoning_delta_ms,
                    output_delta_count: delta_observer.output_delta_count,
                    reasoning_delta_count: delta_observer.reasoning_delta_count,
                    first_event_ms: first_byte_ms,
                    review_run_id: guardian.run_id.clone(),
                    review_role: guardian.role.clone(),
                    review_reason: guardian.reason.clone(),
                    request_id: Some(parent_request_id.clone()),
                    connection_id: connection_id.clone(),
                    conversation_identity: Some(conversation_key.clone()),
                    outcome: Some(outcome_str.to_string()),
                    error_category: stream_error_category.clone(),
                    auth_mode: auth_mode.clone(),
                    provider_profile: provider_profile.clone(),
                    upstream_attempted,
                    agent_attribution: agent_attribution.clone(),
                    ..Default::default()
                    },
                );
                usage_recorded = true;
                if let Err(error) = history.record_exchange_with_conversation_key(
                    &request_body,
                    &completed,
                    &route_id,
                    Some(&conversation_key),
                ) {
                    let message = format!("history durability failed: {error}");
                    yield ok(Bytes::from(failed_sse_event(&message)));
                }
            }
            if !usage_recorded {
                let outcome_str = if stream_error.is_none() { "success" } else { "provider_failure" };
                finish_terminal_once_with_record_raw(
                    &parent_cancel,
                    &diagnostics,
                    &usage,
                    &execution_id,
                    outcome_str,
                    UsageRecord {
                    route_id: route_id.clone(),
                    provider: provider.clone(),
                    model: model.clone(),
                    status: if stream_error.is_none() { 200 } else { 502 },
                    error: stream_error.clone(),
                    duration_ms: started.elapsed().as_millis() as u64,
                    first_byte_ms,
                    first_event_ms: first_byte_ms,
                    review_run_id: guardian.run_id.clone(),
                    review_role: guardian.role.clone(),
                    review_reason: guardian.reason.clone(),
                    request_id: Some(parent_request_id.clone()),
                    connection_id: connection_id.clone(),
                    conversation_identity: Some(conversation_key.clone()),
                    outcome: Some(outcome_str.to_string()),
                    error_category: stream_error_category.clone(),
                    auth_mode: auth_mode.clone(),
                    provider_profile: provider_profile.clone(),
                    upstream_attempted,
                    agent_attribution: agent_attribution.clone(),
                    ..Default::default()
                    },
                );
            }
        }
    }

    /// Official Responses-wire stream: pass the raw bytes through untouched,
    /// track completion incrementally, and persist the completed exchange once.
    /// Mirrors Desktop's `native_stream_response` Official path.
    #[allow(clippy::too_many_arguments)]
    fn official_stream(
        &self,
        route: &RuntimeModelRoute,
        parent_request_id: &str,
        execution_id: &str,
        request_body: Value,
        conversation_key: String,
        upstream: UpstreamStream,
        control_account_hash: Option<String>,
        execution_account_hash: Option<String>,
        selection_revision: Option<u64>,
        continuation_realm: Option<String>,
        baseline: std::time::Instant,
        guardian: GuardianProvenance,
        agent_attribution: Option<crate::usage::AgentUsageAttribution>,
    ) -> impl futures_util::Stream<Item = Result<Bytes, RuntimeError>> + 'static {
        let history = self.history.clone();
        let usage = self.usage.clone();
        let parent_cancel = Arc::clone(&self.parent_cancel);
        let parent_request_id = parent_request_id.to_string();
        let execution_id = execution_id.to_string();
        let diagnostics = Arc::clone(&self.diagnostics);
        let pending_subagent_spawns = Arc::clone(&self.subagent_spawns);
        let route_id = route.route_id.clone();
        let model = route.upstream_model.clone();
        let provider = provider_name(route.provider_kind);
        let started = baseline;
        async_stream::stream! {
            let ok = |bytes: Bytes| Ok::<Bytes, RuntimeError>(bytes);
            let mut source = upstream.body;
            let mut stream_error: Option<String> = None;
            let mut terminal_response_recorded = false;
            let mut first_byte_ms = None;
            let mut tracker = SseCompletionTracker::default();
            loop {
                let item = match tokio::time::timeout(OFFICIAL_STREAM_IDLE_TIMEOUT, source.next()).await {
                    Ok(Some(item)) => item,
                    Ok(None) => break,
                    Err(_) => {
                        let message = format!(
                            "Upstream stream produced no data for {} seconds",
                            OFFICIAL_STREAM_IDLE_TIMEOUT.as_secs()
                        );
                        stream_error = Some(message.clone());
                        yield ok(Bytes::from(failed_sse_event(&message)));
                        break;
                    }
                };
                match item {
                    Ok(bytes) => {
                        if first_byte_ms.is_none() {
                            first_byte_ms = Some(started.elapsed().as_millis() as u64);
                        }
                        if !terminal_response_recorded {
                            if let Some(response) = tracker.push_chunk(&bytes) {
                                match record_exchange_with_official_compactions(
                                    history.as_ref(),
                                    &request_body,
                                    &response,
                                    &route_id,
                                    true,
                                    Some(&conversation_key),
                                    continuation_realm.as_deref(),
                                ) {
                                    Ok(_) => {
                                        terminal_response_recorded = true;
                                        yield ok(bytes);
                                    }
                                    Err(error) => {
                                        let message = format!(
                                            "history durability failed: {error}"
                                        );
                                        stream_error = Some(message.clone());
                                        yield ok(Bytes::from(failed_sse_event(&message)));
                                    }
                                }
                            } else {
                                yield ok(bytes);
                            }
                        } else {
                            tracker.push_chunk(&bytes);
                            yield ok(bytes);
                        }
                    }
                    Err(error) => {
                        let message = format!(
                            "Upstream stream disconnected before completion: {error}"
                        );
                        stream_error = Some(message.clone());
                        yield ok(Bytes::from(failed_sse_event(&message)));
                        break;
                    }
                }
            }
            let completed = tracker.completed_response();
            let terminal_error = tracker.terminal_error();
            if completed.is_none() && terminal_error.is_none() && stream_error.is_none() {
                let message = "Upstream stream ended without response.completed".to_string();
                yield ok(Bytes::from(failed_sse_event(&message)));
            }
            if let Some(response) = completed {
                if !terminal_response_recorded {
                    let status = if stream_error.is_none() { 200 } else { 502 };
                    record_subagent_spawn_requests_in_sink(
                        Arc::clone(&diagnostics),
                        Arc::clone(&pending_subagent_spawns),
                        &parent_request_id,
                        &execution_id,
                        &response,
                    );
                    let details = usage_details_from_response(&response);
                    let outcome_str = if stream_error.is_none() { "success" } else { "provider_failure" };
                    finish_terminal_once_with_record_raw(
                        &parent_cancel,
                        &diagnostics,
                        &usage,
                        &execution_id,
                        outcome_str,
                        UsageRecord {
                        route_id: route_id.clone(),
                        provider: provider.clone(),
                        model: model.clone(),
                        input_tokens: details.input_tokens,
                        output_tokens: details.output_tokens,
                        cached_input_tokens: details.cached_input_tokens,
                        reasoning_tokens: details.reasoning_tokens,
                        tool_calls: details.tool_calls,
                        compaction_tokens: details.compaction_tokens,
                        review_tokens: 0,
                        status,
                        error: stream_error.clone(),
                        duration_ms: started.elapsed().as_millis() as u64,
                        first_byte_ms,
                        first_event_ms: first_byte_ms,
                        request_id: Some(parent_request_id.clone()),
                        conversation_identity: Some(conversation_key.clone()),
                        outcome: Some(outcome_str.to_string()),
                        control_account_hash: control_account_hash.clone(),
                        execution_account_hash: execution_account_hash.clone(),
                        selection_revision,
                        review_run_id: guardian.run_id.clone(),
                        review_role: guardian.role.clone(),
                        review_reason: guardian.reason.clone(),
                        agent_attribution: agent_attribution.clone(),
                        ..Default::default()
                        },
                    );
                    if let Err(error) = record_exchange_with_official_compactions(
                        history.as_ref(),
                        &request_body,
                        &response,
                        &route_id,
                        true,
                        Some(&conversation_key),
                        continuation_realm.as_deref(),
                    ) {
                        let message = format!("history durability failed: {error}");
                        yield ok(Bytes::from(failed_sse_event(&message)));
                    }
                }
            }
        }
    }

    /// Resolve the continuation mode for this request and, for replay modes,
    /// load the local chain and hydrate it into the body that will be
    /// translated upstream. Fails closed ([`RuntimeError::Continuation`])
    /// when a replay mode has no local history — mirroring Desktop's
    /// "cannot continue conversation: previous_response_id `..` has no local
    /// history" behavior.
    fn hydrate_request_history(
        &self,
        route: &RuntimeModelRoute,
        request: &Value,
        body: &mut Value,
        force_portable: bool,
    ) -> Result<HydrationOutcome, RuntimeError> {
        // First-turn store:false / ZDR on Official must request encrypted
        // reasoning even without a previous_response_id, so a later stateless
        // replay has opaque content to reproduce (Desktop parity).
        if route.provider_kind == RuntimeProviderKind::Official
            && request_disables_server_store(request)
            && route.reasoning_capabilities.supports_persisted_reasoning
        {
            ensure_encrypted_reasoning_include(body);
        }
        let input_len = crate::request::request_input_items(body).len();
        let Some(previous) = request
            .get("previous_response_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return Ok(HydrationOutcome::first_turn(input_len));
        };
        let chain = self
            .history
            .get_chain(previous)
            .map_err(RuntimeError::History)?;
        let previous_route_id = chain.last().map(|entry| entry.route_id.as_str());
        let decision = if force_portable {
            crate::continuation::ContinuationDecision {
                mode: ContinuationMode::PortableSemanticReplay,
                same_route: previous_route_id
                    .is_none_or(|previous_route| previous_route == route.route_id),
                store_enabled: !request_disables_server_store(request),
            }
        } else {
            resolve_continuation(route, request, previous_route_id)
        };
        match decision.mode {
            ContinuationMode::ServerPreviousResponseId => {
                Ok(HydrationOutcome::native_passthrough(input_len))
            }
            ContinuationMode::StatelessEncryptedReplay
            | ContinuationMode::PortableSemanticReplay => {
                if chain.is_empty() {
                    return Err(RuntimeError::Continuation(format!(
                        "cannot continue conversation: previous_response_id `{previous}` has no local history (mode={:?})",
                        decision.mode
                    )));
                }
                let mut outcome =
                    crate::replay::hydrate_input_with_mode(body, &chain, decision.mode);
                outcome.same_route = decision.same_route;
                if decision.mode == ContinuationMode::StatelessEncryptedReplay {
                    ensure_encrypted_reasoning_include(body);
                }
                Ok(outcome)
            }
        }
    }

    /// Replace a client-local `context_compaction` / degenerate summary with a
    /// Canonical checkpoint rebuilt from durable source history. Official
    /// native compaction is never rewritten here.
    fn repair_client_local_compaction(
        &self,
        route: &RuntimeModelRoute,
        request: &Value,
        resolved_conversation_key: &str,
        body: &mut Value,
        replay: &mut ReplayContext,
    ) -> Result<(), RuntimeError> {
        let Some(input) = body.get("input").and_then(Value::as_array) else {
            return Ok(());
        };
        if !input.iter().any(is_client_local_compaction_marker) {
            return Ok(());
        }
        let chain = self.durable_compaction_source(
            request,
            Some(resolved_conversation_key),
            &route.route_id,
        )?;
        if chain.is_empty() {
            return Err(RuntimeError::Compaction(
                "client-local compaction has no durable source history; refusing to forward a degenerate summary"
                    .into(),
            ));
        }
        let source = flatten_chain_items(&chain, &[]);
        let canonical =
            rebuild_canonical_checkpoint_from_source(&source).map_err(RuntimeError::Compaction)?;
        seed_replay_identities(input.len(), replay)?;
        let pairs: Vec<(Value, String)> = input
            .iter()
            .cloned()
            .zip(replay.item_identities.iter().cloned())
            .collect();
        let (kept_pairs, repaired, discarded) = strip_client_local_compaction_pairs(&pairs);
        if !repaired {
            return Ok(());
        }
        if discarded.as_ref().is_some_and(|text| {
            canonical.iter().all(|item| {
                serde_json::to_string(item)
                    .ok()
                    .is_none_or(|encoded| !encoded.contains(text))
            })
        }) {
            // discarded client prose is intentionally not in `canonical`.
        }
        let suffix_pairs = kept_pairs
            .into_iter()
            .filter(|(item, _)| {
                !source.iter().any(|stored| {
                    stored == item
                        || (stored.get("id").is_some() && stored.get("id") == item.get("id"))
                })
            })
            .collect::<Vec<_>>();
        let mut rebuilt = canonical.clone();
        rebuilt.extend(suffix_pairs.iter().map(|(item, _)| item.clone()));
        if let Some(object) = body.as_object_mut() {
            object.insert("input".into(), Value::Array(rebuilt.clone()));
        }
        let owner = chain
            .last()
            .map(|entry| entry.response_id.clone())
            .unwrap_or_else(|| format!("cmp_source_{}", ulid::Ulid::new()));
        let compaction_id = new_local_compaction_id()?;
        let generation = self
            .history
            .compaction_ids_for_response(&owner)
            .map_err(RuntimeError::History)?
            .len() as u32
            + 1;
        self.history
            .record_compaction(&owner, &compaction_id, generation, canonical.clone())
            .map_err(RuntimeError::History)?;
        let mut identities: Vec<String> = (0..canonical.len())
            .map(|index| checkpoint_item_identity(&compaction_id, index))
            .collect();
        identities.extend(suffix_pairs.into_iter().map(|(_, identity)| identity));
        replay.compaction_repairs = replay.compaction_repairs.saturating_add(1);
        replay.prefix_len = canonical.len();
        replay.suffix_start = canonical.len();
        replay.suffix_end = rebuilt.len();
        replay.item_identities = identities;
        assert_replay_aligned(rebuilt.len(), replay)
    }

    fn durable_compaction_source(
        &self,
        request: &Value,
        conversation_key: Option<&str>,
        route_id: &str,
    ) -> Result<Vec<HistoryEntry>, RuntimeError> {
        if let Some(previous) = request
            .get("previous_response_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            let chain = self
                .history
                .get_chain(previous)
                .map_err(RuntimeError::History)?;
            if chain.is_empty() {
                return Err(RuntimeError::Compaction(format!(
                    "client-local compaction previous_response_id `{previous}` has no durable source history"
                )));
            }
            return Ok(chain);
        }
        let Some(key) = conversation_key else {
            return Err(RuntimeError::Compaction(
                "client-local compaction has no conversation identity; refusing to forward a degenerate summary"
                    .into(),
            ));
        };
        let chain = self
            .history
            .latest_chain_for_conversation(key, Some(route_id))
            .map_err(|error| {
                if error.contains("ambiguous") {
                    RuntimeError::Compaction(error)
                } else {
                    RuntimeError::History(error)
                }
            })?;
        if chain.is_empty() {
            return Err(RuntimeError::Compaction(
                "client-local compaction has no durable source history; refusing to forward a degenerate summary"
                    .into(),
            ));
        }
        Ok(chain)
    }

    /// True only when a locally known continuation was produced by another
    /// route. Unknown ids stay native: the Official backend may own them even
    /// when this Vellum instance has no journal entry.
    fn is_cross_route_official_continuation(
        &self,
        route: &RuntimeModelRoute,
        request: &Value,
    ) -> Result<bool, RuntimeError> {
        if route.provider_kind != RuntimeProviderKind::Official {
            return Ok(false);
        }
        let Some(previous) = request
            .get("previous_response_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return Ok(false);
        };
        let chain = self
            .history
            .get_chain(previous)
            .map_err(RuntimeError::History)?;
        Ok(chain
            .last()
            .is_some_and(|entry| entry.route_id != route.route_id))
    }

    /// Official response ids are scoped to the ChatGPT account that created
    /// them. When both durable realms are known, an account change is a
    /// provider handoff even though the route id and endpoint are unchanged.
    fn is_cross_realm_official_continuation(
        &self,
        route: &RuntimeModelRoute,
        request: &Value,
        current_realm: Option<&str>,
    ) -> Result<bool, RuntimeError> {
        if route.provider_kind != RuntimeProviderKind::Official {
            return Ok(false);
        }
        let Some(previous) = request
            .get("previous_response_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return Ok(false);
        };
        let previous_realm = self
            .history
            .response_continuation_realm(previous)
            .map_err(RuntimeError::History)?;
        match (previous_realm.as_deref(), current_realm) {
            (Some(previous), Some(current)) => Ok(previous != current),
            // A pre-realm local row cannot prove that its provider-owned id
            // belongs to the currently selected account. Migrate it through
            // one portable replay; the resulting response is realm-tagged.
            // An id absent from local history may still be a valid native
            // Official continuation, so leave that case untouched.
            (None, Some(_)) => self
                .history
                .get_chain(previous)
                .map(|chain| !chain.is_empty())
                .map_err(RuntimeError::History),
            _ => Ok(false),
        }
    }

    /// Native Official WebSocket is only for same-backend Official state.
    /// Unlike [`Self::is_cross_route_official_continuation`] (used by the
    /// generic HTTP execute path, where *any* route change forces the
    /// portable materialization step once), this is the WebSocket socket
    /// reuse test: a continuation produced by a *different but compatible*
    /// Official route (same endpoint, same auth kind ??e.g. Official A?�B)
    /// must still stay native so the turn can share the open segment.
    /// Foreign (third-party) history and Vellum-synthesized items always
    /// take the sanitized HTTP handoff.
    fn official_websocket_requires_portable_handoff(
        &self,
        snapshot: &RuntimeSnapshot,
        route: &RuntimeModelRoute,
        request: &Value,
    ) -> Result<bool, RuntimeError> {
        if contains_vellum_synthetic_item(request) {
            return Ok(true);
        }
        let Some(previous) = request
            .get("previous_response_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return Ok(false);
        };
        let chain = self
            .history
            .get_chain(previous)
            .map_err(RuntimeError::History)?;
        let Some(last) = chain.last() else {
            return Ok(false);
        };
        if last.route_id == route.route_id {
            return Ok(false);
        }
        let Some(previous_route) = snapshot
            .routes
            .iter()
            .find(|item| item.route.route_id == last.route_id)
            .map(|item| &item.route)
        else {
            return Ok(true);
        };
        if previous_route.provider_kind != RuntimeProviderKind::Official
            || previous_route.wire != RuntimeWireFormat::Responses
        {
            return Ok(true);
        }
        let previous_url = official_websocket_endpoint(&previous_route.base_url)?;
        let current_url = official_websocket_endpoint(&route.base_url)?;
        Ok(previous_url != current_url || previous_route.auth_kind != route.auth_kind)
    }
}

fn task_stall_policy_for_request(
    local_compaction_trigger: bool,
    mut policy: TaskStallPolicy,
) -> TaskStallPolicy {
    if local_compaction_trigger {
        policy.mode = crate::task_stall::StallMode::Shadow;
    }
    policy
}

fn task_efficiency_policy_for_request(
    local_compaction_trigger: bool,
    mut policy: TaskEfficiencyPolicy,
) -> TaskEfficiencyPolicy {
    if local_compaction_trigger {
        policy.mode = crate::task_efficiency::EfficiencyMode::Shadow;
    }
    policy
}

fn apply_bounded_stall_finalization(
    execution_body: &mut Value,
    level: crate::task_stall::StallRecoveryState,
    enabled: bool,
) {
    if !enabled || level != crate::task_stall::StallRecoveryState::Escalated {
        return;
    }
    if let Some(object) = execution_body.as_object_mut() {
        object.insert("tools".into(), json!([]));
        object.insert("tool_choice".into(), json!("none"));
        object.insert("parallel_tool_calls".into(), json!(false));
    }
}

fn eval_request_index(request_id: &str) -> Option<u32> {
    request_id
        .starts_with("vellum-eval-")
        .then_some(request_id)
        .and_then(|value| value.rsplit_once("-request-").map(|(_, index)| index))
        .and_then(|value| value.parse().ok())
}

fn redacted_identity(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let prefix = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{prefix}")
}

/// Point A implementation detached from `&ProxyRuntime` so `'static` stream
/// futures can capture only the sink and pending-spawn state.
fn record_subagent_spawn_requests_in_sink(
    diagnostics: Arc<dyn DiagnosticsSink>,
    pending_spawns: Arc<Mutex<Vec<PendingSubagentSpawn>>>,
    parent_request_id: &str,
    parent_execution_id: &str,
    response: &Value,
) {
    let Some(outputs) = response.get("output").and_then(Value::as_array) else {
        return;
    };
    for item in outputs {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            continue;
        }
        let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
        let lowered = name.to_ascii_lowercase();
        if !lowered.contains("spawn") || lowered.contains("shell") {
            continue;
        }
        let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
            continue;
        };
        let arguments_raw = item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}");
        let arguments_len = arguments_raw.len() as u64;
        let parsed = serde_json::from_str::<Value>(arguments_raw)
            .unwrap_or_else(|_| Value::String(arguments_raw.to_string()));
        let encoded = serde_json::to_string(&parsed).unwrap_or_default();
        let arguments = if encoded.len() > 8_192 {
            Value::String(bounded_text(&encoded, 8_192))
        } else {
            parsed.clone()
        };
        let prompt_hash = spawn_prompt(&parsed)
            .map(|prompt| hash_text(&prompt))
            .unwrap_or_else(|| hash_text(""));
        let model = parsed
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
        let reasoning_effort = parsed
            .get("reasoning_effort")
            .or_else(|| parsed.pointer("/reasoning/effort"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let event = DiagnosticEvent::SpawnRequested(SpawnRequested {
            parent_request_id: parent_request_id.to_string(),
            call_id: call_id.to_string(),
            tool_name: name.to_string(),
            arguments: redact_sensitive_json(&arguments),
            arguments_len,
            prompt_hash: prompt_hash.clone(),
            model: model.clone(),
            reasoning_effort,
        });
        if let Err(message) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| diagnostics.record(event)))
        {
            log::warn!("diagnostic sink panicked while recording spawn: {message:?}");
        }
        let pending = PendingSubagentSpawn {
            parent_request_id: parent_request_id.to_string(),
            parent_execution_id: parent_execution_id.to_string(),
            call_id: call_id.to_string(),
            prompt_hash,
            model,
            started: std::time::Instant::now(),
            linked_request_ids: Vec::new(),
            saw_parallel_spawn: false,
            time_window_candidates: Vec::new(),
        };
        let mut spawns = pending_spawns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for existing in spawns.iter_mut() {
            existing.saw_parallel_spawn = true;
        }
        spawns.retain(|existing| existing.call_id != call_id);
        spawns.push(pending);
    }
}

/// The request must name a model before anything can be routed.
fn request_model(request: &RuntimeRequest) -> Result<&str, RuntimeError> {
    request
        .body
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.trim().is_empty())
        .ok_or_else(|| {
            RuntimeError::InvalidRequest("request body is missing the `model` field".into())
        })
}

/// Bound in characters, not bytes: `route_name` and `error.category()` are
/// always short internal identifiers, so this is generous while still
/// keeping a pathological route name from growing a usage record without
/// limit.
const GUARDIAN_FAILURE_REASON_MAX_CHARS: usize = 200;

/// A short, human-legible reason the Guardian primary reviewer was
/// unavailable, for the fallback leg's `UsageRecord::review_reason`.
/// Deliberately never includes the raw upstream error body (which can carry
/// arbitrary provider text) — only the route name, error category and HTTP
/// status, mirroring Desktop's `{route} unavailable (...)` legacy phrasing
/// (`src-tauri/src/proxy_legacy.rs`).
fn bounded_guardian_failure_reason(route_name: &str, error: &RuntimeError) -> String {
    let reason = format!(
        "{route_name} unavailable ({}, HTTP {})",
        error.category(),
        error.http_status().as_u16()
    );
    if reason.chars().count() > GUARDIAN_FAILURE_REASON_MAX_CHARS {
        let truncated: String = reason
            .chars()
            .take(GUARDIAN_FAILURE_REASON_MAX_CHARS)
            .collect();
        format!("{truncated}…")
    } else {
        reason
    }
}

/// Backs `defer_guardian_terminal_claim`'s Sse replay: an exactly-once claim
/// that fires on `Drop`, whichever way the replay generator's own state
/// stops existing. `succeeded` starts `false` and is flipped to `true` only
/// immediately after the generator's own loop yields its very last buffered
/// chunk; a client that drops the stream at any point before that — zero
/// chunks read, or partway through — never reaches that line, so `Drop`
/// fires with `succeeded` still `false` and claims `client_disconnect`
/// instead. Either way this is a single first-writer-wins claim (the same
/// registry `claim_terminal_once_raw` already guards), so it composes safely
/// with every other place that might independently claim the same
/// `execution_id` first.
struct DeferredGuardianClaimGuard {
    parent_cancel: Arc<Mutex<ParentCancelRegistry>>,
    diagnostics: Arc<dyn DiagnosticsSink>,
    usage: Arc<dyn UsageStore>,
    execution_id: String,
    success_record: UsageRecord,
    disconnect_record: UsageRecord,
    succeeded: bool,
}

impl Drop for DeferredGuardianClaimGuard {
    fn drop(&mut self) {
        let (outcome, record) = if self.succeeded {
            ("success", self.success_record.clone())
        } else {
            ("client_disconnect", self.disconnect_record.clone())
        };
        let _ = finish_terminal_once_with_record_raw(
            &self.parent_cancel,
            &self.diagnostics,
            &self.usage,
            &self.execution_id,
            outcome,
            record,
        );
    }
}

/// Outcome of buffering and validating one Guardian attempt (primary or
/// fallback leg). Distinct from a plain `Result` because a client
/// cancellation observed mid-attempt must short-circuit the whole Failover
/// dispatch — no fallback, no fabricated accounting — rather than being
/// treated as a Provider failure eligible for retry.
enum GuardianAttemptOutcome {
    /// A complete, valid assessment was buffered within the deadline.
    /// `usage_body` is the JSON representation used to compute token/usage
    /// accounting for the terminal claim — the merged Responses-shaped body
    /// for a drained SSE stream, or the raw decoded body for `Json`/`Raw` —
    /// independent of what `response` itself replays to the client.
    Success {
        response: RuntimeResponse,
        usage_body: Option<Value>,
    },
    Failed(RuntimeError),
    Cancelled,
}

/// A Guardian response is valid regardless of which wire its reviewer route
/// speaks: try the Responses-shaped decision (nested in `output[]`, or the
/// bare decision object itself), then the Chat-shaped one (a single decision
/// embedded in `choices[0].message`).
fn guardian_body_has_assessment(body: &Value) -> bool {
    crate::review::has_guardian_assessment(body)
        || crate::review::guardian_assessment_from_chat_body(body).is_some()
}

/// Whether a Guardian primary failure may be retried against the configured
/// fallback, or must be surfaced with the primary's own real classification.
///
/// Deliberately narrow: only failures that are plausibly about *this one
/// Provider being temporarily unavailable* — quota exhaustion, the provider
/// being unreachable, the upstream never opening response headers in time, or
/// a Guardian attempt not producing a valid assessment within its bounded
/// deadline (`ReviewAttemptTimeout` — headers/keepalive/a partial delta with
/// no terminal assessment is exactly as fallback-eligible as never
/// connecting at all: the primary simply did not deliver in time) — qualify.
/// Authentication, missing credentials, malformed requests, unsupported
/// routes, protocol/internal errors (including a *completed* attempt whose
/// content is not a valid assessment) and every other category must never be
/// masked by a fallback attempt: they are not "try a different Provider"
/// problems, and hiding them behind a fallback would make a real
/// misconfiguration (bad credential, wrong route) look like healthy
/// failover in review stats.
fn review_failure_allows_fallback(error: &RuntimeError) -> bool {
    matches!(
        error,
        RuntimeError::ProviderQuota { .. }
            | RuntimeError::ProviderUnavailable(_)
            | RuntimeError::StreamOpenTimeout(_)
            | RuntimeError::ReviewAttemptTimeout(_)
    )
}

/// Whether the request asks for a streaming (`text/event-stream`) response.
/// M5 wires the real streaming path; a `stream: false` request stays
/// buffered and is never upgraded.
fn request_requests_streaming(request: &RuntimeRequest) -> bool {
    request.body.get("stream").and_then(Value::as_bool) == Some(true)
}

/// Return a retry body only when the caller actually supplied a top-level
/// Responses `tool_choice`. Parsing failures and nested fields fail closed.
fn without_top_level_tool_choice(encoded_body: &[u8]) -> Option<Vec<u8>> {
    let mut body = serde_json::from_slice::<Value>(encoded_body).ok()?;
    let removed = body.as_object_mut()?.remove("tool_choice")?;
    drop(removed);
    serde_json::to_vec(&body).ok()
}

/// Match only the ChatGPT Codex backend's explicit unsupported-field error.
/// A generic 400, a tool execution error, or any third-party response must
/// never weaken the passthrough request on retry.
fn official_rejects_tool_choice(
    route: &RuntimeModelRoute,
    status: u16,
    response_body: &[u8],
) -> bool {
    if route.provider_kind != RuntimeProviderKind::Official || !(400..500).contains(&status) {
        return false;
    }
    let Ok(body) = serde_json::from_slice::<Value>(response_body) else {
        return false;
    };
    let Some(error) = body.get("error") else {
        return false;
    };
    let param_matches = error.get("param").and_then(Value::as_str) == Some("tool_choice");
    let code_matches = error.get("code").and_then(Value::as_str) == Some("unknown_parameter");
    let message_matches = error
        .get("message")
        .and_then(Value::as_str)
        .is_some_and(|message| {
            let message = message.to_ascii_lowercase();
            message.contains("unknown parameter") && message.contains("tool_choice")
        });
    param_matches && (code_matches || message_matches)
}

async fn drain_upstream_error_body(upstream: &mut UpstreamStream) -> Result<Vec<u8>, RuntimeError> {
    // Provider error envelopes are small; the same idle bound used for SSE
    // prevents a broken upstream from pinning this compatibility check.
    let mut body = Vec::new();
    loop {
        match tokio::time::timeout(STREAM_IDLE_TIMEOUT, upstream.body.next()).await {
            Ok(Some(Ok(bytes))) => body.extend_from_slice(&bytes),
            Ok(Some(Err(error))) => return Err(map_transport_error(error)),
            Ok(None) | Err(_) => break,
        }
    }
    Ok(body)
}

const PROVIDER_ERROR_TEXT_LIMIT: usize = 240;

fn provider_error_message(body: &[u8], status: u16) -> String {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        let joined = json_provider_error_parts(&value).join(" | ");
        if !joined.is_empty() {
            return crate::diagnostics::bounded_text(&joined, PROVIDER_ERROR_TEXT_LIMIT);
        }
    }
    let raw = String::from_utf8_lossy(body);
    let trimmed = raw.trim();
    if !trimmed.is_empty() {
        return crate::diagnostics::bounded_text(trimmed, PROVIDER_ERROR_TEXT_LIMIT);
    }
    format!("upstream stream failed with HTTP {status}")
}

fn record_chat_wire_diagnostics(
    diagnostics_sink: &Arc<dyn DiagnosticsSink>,
    route: &RuntimeModelRoute,
    encoded_body: &[u8],
    chat_diagnostics: &mut Option<crate::replay::HarnessTranscriptDiagnostics>,
    upstream_status: Option<u16>,
) {
    if let Some(mut diagnostics) = chat_diagnostics.take() {
        diagnostics.upstream_status = upstream_status;
        diagnostics_sink.record(DiagnosticEvent::HarnessTranscript(diagnostics));
    }
    log_chat_wire_shape(route, encoded_body, upstream_status);
}

/// Emit a content-free fingerprint for Chat dispatches. This is deliberately
/// kept in the executor, next to the upstream status, so a generic gateway
/// 400 can be diagnosed without logging the prompt, tool arguments, or tool
/// output. The persisted transcript event carries the same shape fields and
/// the log adds the exact body size and final upstream status.
fn log_chat_wire_shape(
    route: &RuntimeModelRoute,
    encoded_body: &[u8],
    upstream_status: Option<u16>,
) {
    if route.wire != RuntimeWireFormat::Chat {
        return;
    }
    let Ok(body) = serde_json::from_slice::<Value>(encoded_body) else {
        log::warn!(
            "[ChatShape] model={} upstream_status={:?} message_count=? role_sequence=? empty_messages=? tool_pairs=? orphan_tool_results=? body_bytes={} invalid_json=true",
            route.upstream_model,
            upstream_status,
            encoded_body.len(),
        );
        return;
    };
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let shape = crate::replay::chat_transcript_diagnostics(messages, &ReplayContext::default());
    let level = if upstream_status.is_some_and(|status| !(200..300).contains(&status)) {
        log::Level::Warn
    } else {
        log::Level::Info
    };
    log::log!(
        level,
        "[ChatShape] model={} upstream_status={:?} message_count={} role_sequence={} empty_messages={} tool_pairs={} orphan_tool_results={} duplicate_tool_ids={} first_role={:?} last_role={:?} body_bytes={}",
        route.upstream_model,
        upstream_status,
        shape.message_count,
        shape.role_sequence,
        shape.empty_messages,
        shape.tool_pairs,
        shape.orphan_tool_results,
        shape.duplicate_tool_ids,
        shape.first_role,
        shape.last_role,
        encoded_body.len(),
    );
}

/// Every upstream request in this runtime carries a JSON body. Some providers
/// accept an untyped body, but OpenCode Go rejects it at the gateway (currently
/// as a misleading HTTP 500), so content type is part of the wire contract and
/// must be present on normal, streaming, compact, review, and retry paths.
/// SSE comment so the inbound HTTP response can flush its status line before
/// the provider's first application event. Clients ignore comment frames.
///
/// Sized past Hyper's HTTP/1 write buffer so a lone comment is actually
/// written to the socket instead of sitting unflushed until the next frame.
pub(crate) const STREAM_OPEN_PAD_BYTES: usize = 16 * 1024;

pub(crate) fn stream_open_frame() -> Bytes {
    let mut frame = Vec::with_capacity(STREAM_OPEN_PAD_BYTES);
    frame.extend_from_slice(b": vellum-stream-open ");
    frame.resize(STREAM_OPEN_PAD_BYTES.saturating_sub(2), b' ');
    frame.extend_from_slice(b"\n\n");
    Bytes::from(frame)
}

/// Prepend the inbound flush comment as an immediately-ready item.
///
/// Yielding the same comment from inside `async_stream!` does not flush
/// Hyper. That generator keeps the first `poll_next` pending until the
/// first real `.await` (`source.next()`), so the inbound HTTP status
/// stays buffered until the provider's first application byte ??observed
/// live as ~265s to HTTP 200 after upstream headers had already arrived.
fn inbound_sse_stream<S>(inner: S) -> BoxStream<'static, Result<Bytes, RuntimeError>>
where
    S: futures_util::Stream<Item = Result<Bytes, RuntimeError>> + Send + 'static,
{
    futures_util::stream::iter(std::iter::once(Ok(stream_open_frame())))
        .chain(inner)
        .boxed()
}

/// End the stream the moment `cancel` fires (a direct cancel of this request,
/// or fan-out from a cancelled parent). Uses the borrow-then-wait pattern so
/// an already-cancelled tree also ends a stream that has not started yet.
/// Wraps a stream with both the cancel-future gate and the execution's
/// [`CancelRegistration`]. The guard travels inside the stream itself so it
/// stays alive for exactly as long as the caller keeps polling this stream --
/// natural exhaustion, the cancel future firing, or the caller simply
/// dropping the stream on disconnect all release the cancel socket the same
/// way, without every call site needing to remember an explicit unregister
/// (P0: sockets used to leak on the ordinary "client read to completion"
/// path).
fn take_until_cancel(
    stream: BoxStream<'static, Result<Bytes, RuntimeError>>,
    cancel: watch::Receiver<bool>,
    guard: CancelRegistration,
) -> BoxStream<'static, Result<Bytes, RuntimeError>> {
    let mut cancel = cancel;
    let cancel_future = async move {
        if *cancel.borrow() {
            return;
        }
        let _ = cancel.changed().await;
    };
    let inner = stream.take_until(cancel_future).boxed();
    GuardedCancelStream {
        inner,
        _guard: guard,
    }
    .boxed()
}

struct GuardedCancelStream {
    inner: BoxStream<'static, Result<Bytes, RuntimeError>>,
    _guard: CancelRegistration,
}

impl futures_util::Stream for GuardedCancelStream {
    type Item = Result<Bytes, RuntimeError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.inner.as_mut().poll_next(cx)
    }
}

/// Elapsed-ms bookmarks for one dispatch. Used to split pre-send work from
/// the upstream handshake so a multi-minute first byte can be attributed.
struct DispatchStages {
    start: std::time::Instant,
    stages: Vec<(&'static str, u64)>,
}

impl DispatchStages {
    fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
            stages: Vec::new(),
        }
    }

    fn mark(&mut self, name: &'static str) {
        self.stages
            .push((name, self.start.elapsed().as_millis() as u64));
    }

    fn summary(&self) -> String {
        self.stages
            .iter()
            .map(|(name, ms)| format!("{name}={ms}ms"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

// Every parameter is a borrowed reference to state the caller already owns
// (transport/catalog/route/auth/environment plus three scalar flags); a
// wrapper struct would only add an allocation-free rename with no behavior
// change, so this stays over clippy's default arg-count threshold on
// purpose rather than growing an ad hoc options type for one call site.
#[allow(clippy::too_many_arguments)]
async fn open_chat_continuation_stream(
    transport: &dyn UpstreamTransport,
    catalog: &dyn RouteCatalog,
    route: &RuntimeModelRoute,
    auth: &ResolvedAuth,
    request_body: &Value,
    environment: &crate::environment::ExecutionEnvironment,
    harness_options: HarnessOptions,
    delegation_runtime_wired: bool,
    web_search_enabled: bool,
    opencode_quota: &crate::opencode::OpenCodeQuotaCooldown,
) -> Result<UpstreamStream, RuntimeError> {
    if route.effective_provider_profile().is_some() {
        if let Some(entry) = opencode_quota.get(&route.route_id, &route.upstream_model) {
            let remaining = entry.remaining_secs();
            return Err(RuntimeError::provider_quota(
                entry.diagnostic,
                Some(remaining),
                false,
            ));
        }
    }
    let snapshot =
        RuntimeSnapshot::capture_from(catalog).with_web_search_enabled(web_search_enabled);
    let catalog_entry = snapshot
        .resolve_route_snapshot(&route.catalog_id)
        .map(|entry| entry.catalog_entry.clone());
    let profile = resolve_with_options(
        route.provider_kind,
        route.wire,
        harness_options,
        delegation_runtime_wired,
    );
    let adapter_route = ProfileAdapterRoute::from(route);
    let mut prepared = prepare_upstream_request_with_environment(
        request_body,
        &adapter_route,
        &profile,
        environment,
        catalog_entry.as_ref(),
        web_search_enabled,
    )
    .map_err(RuntimeError::InvalidRequest)?;
    if route.effective_provider_profile().is_some() {
        crate::opencode::strip_openai_only_fields(&mut prepared);
    }
    let encoded_body = serde_json::to_vec(&prepared)
        .map_err(|error| RuntimeError::Internal(format!("encode Chat continuation: {error}")))?;
    let mut upstream = transport
        .execute_streaming(&UpstreamRequest {
            method: "POST".into(),
            url: upstream_endpoint(&route.base_url, route.wire),
            headers: json_upstream_headers(auth, route, request_body, &encoded_body),
            body: encoded_body,
            timeout: None,
            max_response_bytes: None,
        })
        .await
        .map_err(map_transport_error)?;
    if !(200..300).contains(&upstream.status) {
        let status = upstream.status;
        let error_body = drain_upstream_error_body(&mut upstream).await?;
        if route.effective_provider_profile().is_some() && status == 429 {
            let retry_after = crate::opencode::parse_retry_after(&upstream.headers)
                .unwrap_or(crate::opencode::DEFAULT_QUOTA_RETRY_AFTER_SECS);
            opencode_quota.record(
                &route.route_id,
                &route.upstream_model,
                retry_after,
                provider_error_message(&error_body, status),
            );
        }
        return Err(provider_error_from_upstream(
            status,
            &error_body,
            &upstream.headers,
        ));
    }
    Ok(upstream)
}

fn json_upstream_headers(
    auth: &ResolvedAuth,
    route: &RuntimeModelRoute,
    inbound_body: &Value,
    upstream_body: &[u8],
) -> Vec<(String, String)> {
    let mut headers = auth.upstream_headers(&route.upstream_model);
    headers.push(("content-type".into(), "application/json".into()));
    if route.effective_provider_profile().is_some() {
        headers.extend(crate::opencode::identity_headers(
            inbound_body,
            upstream_body,
        ));
    }
    headers
}

fn opencode_usage_fields(
    route: &RuntimeModelRoute,
    error: Option<&RuntimeError>,
) -> (Option<String>, Option<String>, Option<bool>, Option<u64>) {
    let profile = route
        .effective_provider_profile()
        .map(|profile| profile.as_str().to_string());
    let auth_mode = route
        .effective_access_mode()
        .map(|mode| mode.as_str().to_string());
    match error {
        Some(error) => (
            auth_mode,
            profile,
            error.upstream_attempted(),
            error.retry_after_secs(),
        ),
        None => (
            auth_mode,
            profile.clone(),
            profile.as_ref().map(|_| true),
            None,
        ),
    }
}

/// Stable provider label for usage records (Desktop parity naming).
fn provider_name(kind: RuntimeProviderKind) -> String {
    kind.as_str().to_string()
}

fn official_usage_identity(
    auth: &ResolvedAuth,
    request: &RuntimeRequest,
) -> (Option<String>, Option<String>) {
    if !matches!(
        auth,
        ResolvedAuth::OfficialManaged { .. } | ResolvedAuth::OfficialPreserveIncoming { .. }
    ) {
        return (None, None);
    }
    let control = request
        .incoming_auth
        .openai_account
        .as_deref()
        .map(hashed_account_identity);
    let execution = match auth {
        ResolvedAuth::OfficialManaged { account_id, .. } => {
            account_id.as_deref().map(hashed_account_identity)
        }
        ResolvedAuth::OfficialPreserveIncoming { account, .. } => {
            account.as_deref().map(hashed_account_identity)
        }
        _ => None,
    };
    (control, execution)
}

fn official_selection_revision(auth: &ResolvedAuth) -> Option<u64> {
    match auth {
        ResolvedAuth::OfficialManaged {
            selection_revision, ..
        } => *selection_revision,
        _ => None,
    }
}

fn official_refresh_matches_snapshot(
    rejected: &OfficialAuthorization,
    refreshed: &OfficialAuthorization,
) -> bool {
    refreshed.account_id == rejected.account_id
        && refreshed.selection_revision == rejected.selection_revision
        && refreshed.selection_verified
}

fn hashed_account_identity(account_id: &str) -> String {
    let digest = Sha256::digest(account_id.as_bytes());
    let encoded = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{encoded}")
}

/// Stable realm fingerprint compatible with Desktop's durable
/// `response_realm.realm_fingerprint`. Raw account ids never reach history.
fn official_continuation_realm(route: &RuntimeModelRoute, auth: &ResolvedAuth) -> Option<String> {
    let account = match auth {
        ResolvedAuth::OfficialManaged { account_id, .. } => account_id.as_deref(),
        ResolvedAuth::OfficialPreserveIncoming { account, .. } => account.as_deref(),
        _ => None,
    }?;
    let mut hasher = Sha256::new();
    for part in ["official", route.base_url.as_str(), "chatgpt", account] {
        hasher.update(part.trim().to_ascii_lowercase().as_bytes());
        hasher.update([0]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

/// Map an adapter/translation failure to the runtime category. The
/// delegation gate (M10) is a mode the route cannot honour: it answers 422
/// like Desktop, not a generic 400.
fn map_prepare_error(message: String) -> RuntimeError {
    if message.contains("requires a verified delegation runtime") {
        RuntimeError::UnsupportedMode(message)
    } else {
        RuntimeError::InvalidRequest(message)
    }
}

/// Map a transport-level failure to a stable runtime category. A request that
/// could not even be *constructed* is a defect in this runtime (Internal);
/// every reachability/read failure is a provider problem (ProviderUnavailable).
fn map_transport_error(error: TransportError) -> RuntimeError {
    match error {
        TransportError::InvalidRequest(message) => {
            RuntimeError::Internal(format!("upstream request construction failed: {message}"))
        }
        TransportError::Timeout(message)
        | TransportError::Connect(message)
        | TransportError::ResponseRead(message)
        | TransportError::Other(message) => {
            RuntimeError::ProviderUnavailable(format!("upstream unavailable: {message}"))
        }
        TransportError::ResponseTooLarge { limit, bytes_read } => RuntimeError::ProviderProtocol(
            format!("upstream response exceeded {limit} bytes (bytes_read={bytes_read})"),
        ),
    }
}

async fn execute_upstream_inner(
    transport: &dyn crate::transport::UpstreamTransport,
    request: &UpstreamRequest,
    route: &RuntimeModelRoute,
) -> Result<UpstreamResponse, RuntimeError> {
    match transport.execute(request).await {
        Ok(response) => {
            if let Some(limit) = request.max_response_bytes {
                if response.body.len() > limit {
                    return Err(upstream_limit_error(
                        &provider_name(route.provider_kind),
                        &route.route_id,
                        "third-party non-stream response",
                        limit,
                        response.body.len(),
                    ));
                }
            }
            Ok(response)
        }
        Err(TransportError::ResponseTooLarge { limit, bytes_read }) => Err(upstream_limit_error(
            &provider_name(route.provider_kind),
            &route.route_id,
            "third-party non-stream response",
            limit,
            bytes_read,
        )),
        Err(error) => Err(map_transport_error(error)),
    }
}

fn non_streaming_timeout(route: &RuntimeModelRoute) -> Option<std::time::Duration> {
    if route.provider_kind == RuntimeProviderKind::Official {
        None
    } else {
        Some(NON_STREAMING_TIMEOUT)
    }
}

fn third_party_response_cap(route: &RuntimeModelRoute) -> Option<usize> {
    if route.provider_kind == RuntimeProviderKind::Official {
        None
    } else {
        Some(THIRD_PARTY_NON_STREAM_MAX_BYTES)
    }
}

fn upstream_limit_error(
    provider: &str,
    route: &str,
    limit_name: &str,
    limit_bytes: usize,
    bytes_read: usize,
) -> RuntimeError {
    RuntimeError::ProviderProtocol(format!(
        "{limit_name} exceeded (provider={provider}; route={route}; limit={limit_bytes}; bytes_read={bytes_read})"
    ))
}

/// Absolute upstream URL: the provider base plus the wire suffix (`/responses`
/// or `/chat/completions`), matching Desktop's `upstream_endpoint`.
fn upstream_endpoint(base_url: &str, wire: RuntimeWireFormat) -> String {
    let suffix = match wire {
        RuntimeWireFormat::Responses => "/responses",
        RuntimeWireFormat::Chat => "/chat/completions",
    };
    format!("{base_url}{suffix}")
}

/// Collapse a resolved auth outcome into the coarse posture the WebSocket
/// connection key compares. Tokens rotate; the *kind* of credential that
/// opened the socket does not, so a refresh must never look like a new
/// posture and force a needless reopen.
fn official_websocket_auth_posture(
    route: &RuntimeModelRoute,
    auth: &ResolvedAuth,
) -> OfficialWebSocketAuthPosture {
    match auth {
        ResolvedAuth::None => OfficialWebSocketAuthPosture::None,
        ResolvedAuth::OfficialPreserveIncoming { .. } => {
            OfficialWebSocketAuthPosture::PreserveIncoming
        }
        ResolvedAuth::OfficialManaged { account_id, .. } => OfficialWebSocketAuthPosture::Managed {
            account_id: account_id.clone(),
        },
        ResolvedAuth::Bearer(_) => OfficialWebSocketAuthPosture::Bearer {
            credential_id: route.credential_id.clone(),
        },
        ResolvedAuth::GrokSession { .. } => OfficialWebSocketAuthPosture::Bearer {
            credential_id: route.credential_id.clone(),
        },
    }
}

fn official_websocket_endpoint(base_url: &str) -> Result<String, RuntimeError> {
    let mut url = url::Url::parse(base_url).map_err(|error| {
        RuntimeError::InvalidRequest(format!("invalid Official base URL `{base_url}`: {error}"))
    })?;
    match url.scheme() {
        "https" => url.set_scheme("wss").expect("wss is a valid URL scheme"),
        "http" => url.set_scheme("ws").expect("ws is a valid URL scheme"),
        scheme => {
            return Err(RuntimeError::InvalidRequest(format!(
                "Official WebSocket base URL must use http or https, got `{scheme}`"
            )))
        }
    }
    let path = format!("{}/responses", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url.into())
}

fn summary_response_text(value: &Value, wire: RuntimeWireFormat) -> String {
    match wire {
        RuntimeWireFormat::Responses => response_output_text(value),
        RuntimeWireFormat::Chat => value
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

fn bounded_provider_diagnostic(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.chars().take(512).collect()
}

/// Persist an exchange and, for Official compaction responses, bind every
/// opaque compaction id to a provider-independent source window. The daemon
/// uses the shared HistoryStore directly (there is no Desktop bridge), so
/// this must live in the runtime for bundled macOS/Linux proxy images too.
fn record_exchange_with_official_compactions(
    history: &dyn HistoryStore,
    request: &Value,
    response: &Value,
    route_id: &str,
    official: bool,
    conversation_key: Option<&str>,
    continuation_realm: Option<&str>,
) -> Result<bool, String> {
    // Keep Official wire payloads untouched for Codex, but persist a separate
    // portable projection for a later provider switch. Opaque OpenAI state is
    // valid only on the native route/account realm.
    let (portable_request, portable_response) = if official {
        let mut portable_request = request.clone();
        if let Some(input) = portable_request
            .get_mut("input")
            .and_then(Value::as_array_mut)
        {
            *input = sanitize_portable_history(input);
        }
        let mut portable_response = response.clone();
        if let Some(output) = portable_response
            .get_mut("output")
            .and_then(Value::as_array_mut)
        {
            *output = sanitize_portable_history(output);
        }
        (portable_request, portable_response)
    } else {
        (request.clone(), response.clone())
    };
    let saved = history.record_exchange_with_context(
        &portable_request,
        &portable_response,
        route_id,
        conversation_key,
        continuation_realm,
    )?;
    if !official {
        return Ok(saved);
    }
    let compaction_ids = response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("compaction"))
        .filter_map(|item| item.get("id").and_then(Value::as_str))
        .collect::<Vec<_>>();
    if compaction_ids.is_empty() {
        return Ok(saved);
    }
    let source = request
        .get("input")
        .and_then(Value::as_array)
        .map(|items| sanitize_portable_history(items))
        .unwrap_or_default();
    if source.is_empty() {
        return Ok(saved);
    }
    let owner = response
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .unwrap_or(compaction_ids[0]);
    // `canonical_items` here is the history OpenAI compacted, not the result:
    // the result is ciphertext this proxy never reads, and what a third-party
    // route needs when it meets the same opaque marker later is the readable
    // window `materialize_local_compactions` puts back in its place.
    //
    // The row therefore reads as a compaction *backwards* to anything that
    // assumes the usual before/after — which is exactly what the Context
    // screen did with every Official compaction. `provider_owned` is how the
    // row says which of the two shapes it is instead of leaving each reader
    // to infer it.
    //
    // The stored window doubles as both halves of the record because
    // materialization validates every journal row it installs -- engine id,
    // schema version, and both hashes -- and a row that failed those checks
    // would refuse the whole conversation rather than degrade. `provider_owned`
    // is what tells a *reader* the two halves are the same window recorded
    // twice, not a compaction that shrank nothing.
    let window_hash = crate::history::hash_items(&source);
    for compaction_id in compaction_ids {
        history.record_compaction_record(crate::history::CompactionJournalRecord {
            response_id: owner.to_string(),
            compaction_id: compaction_id.to_string(),
            generation: 1,
            engine_id: crate::codex_local_v0_150::ENGINE_ID.to_string(),
            engine_provenance: Some(crate::codex_local_v0_150::ENGINE_PROVENANCE.to_string()),
            route_id: String::new(),
            upstream_model: String::new(),
            tokens_before: 0,
            tokens_after: 0,
            elapsed_ms: 0,
            schema_version: crate::history::LOCAL_COMPACTION_RECORD_SCHEMA_V1,
            source_items: source.clone(),
            canonical_items: source.clone(),
            source_hash: window_hash.clone(),
            checkpoint_hash: window_hash.clone(),
            provider_owned: true,
        })?;
    }
    Ok(saved)
}

/// Produce the exact Codex remote-compaction-v2 result.  Streaming clients
/// collect output items from `response.output_item.done`, not merely from the
/// terminal response, so all three events are required and the output must
/// contain exactly one compaction item.
fn compaction_trigger_response(request: &Value, item: Value) -> RuntimeResponse {
    let response = json!({
        "id": format!("resp_vellum_compact_{}", ulid::Ulid::new()),
        "object": "response",
        "status": "completed",
        "model": request.get("model").cloned().unwrap_or(Value::Null),
        "output": [item.clone()],
        "parallel_tool_calls": false,
        "tool_choice": "none",
        "tools": [],
        "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0}
    });
    if !request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(true)
    {
        return RuntimeResponse::Json(response);
    }
    let events = [
        (
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "sequence_number": 0,
                "output_index": 0,
                "item": item.clone()
            }),
        ),
        (
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "sequence_number": 1,
                "output_index": 0,
                "item": item
            }),
        ),
        (
            "response.completed",
            json!({
                "type": "response.completed",
                "sequence_number": 2,
                "response": response
            }),
        ),
    ];
    let payload = events
        .into_iter()
        .map(|(event, data)| Ok(Bytes::from(format!("event: {event}\ndata: {data}\n\n"))))
        .collect::<Vec<Result<Bytes, RuntimeError>>>();
    RuntimeResponse::Sse(futures_util::stream::iter(payload).boxed())
}

fn alpha_search_endpoint(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") || base.ends_with("/codex") {
        format!("{base}/alpha/search")
    } else {
        format!("{base}/v1/alpha/search")
    }
}

/// Decode a buffered provider response into the normalized, Codex-shaped body
/// that becomes `RuntimeResponse::Json`. Mirrors Desktop's non-streaming
/// `forward_response` decode step (the `responses_*` parity fixtures are its
/// frozen behavior).
fn decode_non_streaming_response(
    request_body: &Value,
    route: &RuntimeModelRoute,
    response: UpstreamResponse,
) -> Result<Value, RuntimeError> {
    if let Some(limit) = third_party_response_cap(route) {
        if response.body.len() > limit {
            return Err(upstream_limit_error(
                &provider_name(route.provider_kind),
                &route.route_id,
                "third-party non-stream response",
                limit,
                response.body.len(),
            ));
        }
    }
    let status = response.status;
    if !(200..300).contains(&status) {
        return Err(provider_error_from_upstream(
            status,
            &response.body,
            &response.headers,
        ));
    }
    let body: Value = serde_json::from_slice(&response.body).map_err(|error| {
        RuntimeError::ProviderProtocol(format!("upstream returned a non-JSON body: {error}"))
    })?;
    if route.provider_kind == RuntimeProviderKind::Official {
        Ok(body)
    } else {
        normalize_non_streaming_response(request_body, route.wire, &route.upstream_model, body)
            .map_err(RuntimeError::ProviderProtocol)
    }
}

fn json_provider_error_parts(body: &Value) -> Vec<String> {
    let mut parts = Vec::new();
    let mut push = |value: &str| {
        let trimmed = value.trim();
        if !trimmed.is_empty() && !parts.iter().any(|part| part == trimmed) {
            parts.push(trimmed.to_string());
        }
    };
    match body.get("error") {
        Some(Value::String(message)) => push(message),
        Some(Value::Object(error)) => {
            if let Some(message) = error.get("message").and_then(Value::as_str) {
                push(message);
            }
            if let Some(code) = error.get("code").and_then(Value::as_str) {
                push(code);
            }
        }
        _ => {}
    }
    if let Some(detail) = body.get("detail").and_then(Value::as_str) {
        push(detail);
    }
    if let Some(message) = body.get("message").and_then(Value::as_str) {
        push(message);
    }
    parts
}

fn provider_error_for_status(status: u16, message: &str) -> RuntimeError {
    provider_error_for_status_with_retry(status, message, None, true)
}

fn provider_error_from_upstream(
    status: u16,
    body: &[u8],
    headers: &[(String, String)],
) -> RuntimeError {
    let message = provider_error_message(body, status);
    let retry_after = if status == 429 {
        crate::opencode::parse_retry_after(headers)
            .or(Some(crate::opencode::DEFAULT_QUOTA_RETRY_AFTER_SECS))
    } else {
        crate::opencode::parse_retry_after(headers)
    };
    provider_error_for_status_with_retry(status, &message, retry_after, true)
}

fn provider_error_for_status_with_retry(
    status: u16,
    message: &str,
    retry_after_secs: Option<u64>,
    upstream_attempted: bool,
) -> RuntimeError {
    let diagnostic = crate::diagnostics::bounded_text(message, PROVIDER_ERROR_TEXT_LIMIT);
    match status {
        401 | 403 => RuntimeError::ProviderUnauthorized(diagnostic),
        429 => RuntimeError::provider_quota(
            crate::opencode::quota_diagnostic(&diagnostic),
            retry_after_secs,
            upstream_attempted,
        ),
        400 => {
            if message.to_ascii_lowercase().contains("quota") {
                RuntimeError::provider_quota(
                    crate::opencode::quota_diagnostic(&diagnostic),
                    retry_after_secs,
                    upstream_attempted,
                )
            } else {
                RuntimeError::ProviderProtocol(format!("upstream HTTP {status}: {diagnostic}"))
            }
        }
        408 | 409 | 425 | 426 => {
            RuntimeError::ProviderProtocol(format!("upstream HTTP {status}: {diagnostic}"))
        }
        _ if (500..600).contains(&status) => RuntimeError::ProviderUnavailable(diagnostic),
        _ => RuntimeError::ProviderProtocol(format!("upstream HTTP {status}: {diagnostic}")),
    }
}

fn outcome_for_error(error: &RuntimeError) -> &'static str {
    match error.category() {
        "provider_protocol" | "invalid_request" => "protocol_failure",
        "client_cancel" => "client_cancel",
        "proxy_stopped" => "user_stopped_proxy",
        _ => "provider_failure",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::MemoryCredentialProvider;
    use crate::environment::{
        ExecutionEnvironment, ExecutorCapability, RuntimeAmpersandSemantics, RuntimePathStyle,
        RuntimePlatform, RuntimeShellKind,
    };
    use crate::lifecycle::CountingRequestLifecycle;
    use crate::official_auth::UnconfiguredOfficialAuthProvider;
    use crate::request::{IncomingAuthContext, RequestMetadata, RuntimeEndpoint};
    use crate::route::{
        ResolvedRoute, RouteCatalog, RuntimeAuthKind, RuntimeCompactionCapabilities,
        RuntimeModelRoute, RuntimeProviderKind, RuntimeReasoningCapabilities,
        RuntimeToolCapabilities, RuntimeWireFormat,
    };
    use crate::search_loop::DEFAULT_SEARCH_TOOL_LOOP_LIMIT;
    use crate::streaming::format_sse_value;
    use crate::transport::{FixtureTransport, TransportError, UpstreamResponse, UpstreamStream};
    use crate::usage::UsageSummary;
    use serde_json::json;

    #[test]
    fn stream_open_frame_is_an_sse_comment() {
        let frame = stream_open_frame();
        let text = std::str::from_utf8(&frame).unwrap();
        assert!(text.starts_with(':'));
        assert!(text.ends_with("\n\n"));
        assert!(!text.contains("event:"));
        assert!(frame.len() >= STREAM_OPEN_PAD_BYTES);
    }

    #[test]
    fn compaction_request_cannot_consume_a_task_stall_recovery() {
        let recover = TaskStallPolicy::recover();
        assert_eq!(
            task_stall_policy_for_request(true, recover.clone()).mode,
            crate::task_stall::StallMode::Shadow
        );
        assert_eq!(
            task_stall_policy_for_request(false, recover).mode,
            crate::task_stall::StallMode::Recover
        );
    }

    #[test]
    fn compaction_request_cannot_consume_a_task_efficiency_recovery() {
        let recover = TaskEfficiencyPolicy::recover();
        assert_eq!(
            task_efficiency_policy_for_request(true, recover.clone()).mode,
            crate::task_efficiency::EfficiencyMode::Shadow
        );
        assert_eq!(
            task_efficiency_policy_for_request(false, recover).mode,
            crate::task_efficiency::EfficiencyMode::Recover
        );
    }

    #[test]
    fn escalated_eval_stall_gets_exactly_one_tool_disabled_finalization() {
        let mut body = json!({
            "model": "vlm-test",
            "tools": [{"type": "function", "name": "exec_command"}],
            "tool_choice": "auto",
            "parallel_tool_calls": true
        });
        apply_bounded_stall_finalization(
            &mut body,
            crate::task_stall::StallRecoveryState::Warned,
            true,
        );
        assert_ne!(body["tools"], json!([]));

        apply_bounded_stall_finalization(
            &mut body,
            crate::task_stall::StallRecoveryState::Escalated,
            true,
        );
        assert_eq!(body["tools"], json!([]));
        assert_eq!(body["tool_choice"], json!("none"));
        assert_eq!(body["parallel_tool_calls"], json!(false));
    }

    #[tokio::test]
    async fn eval_stall_marker_is_durable_and_escalation_reaches_upstream_without_tools() {
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: chat_ok_body(),
        }]));
        let history = Arc::new(MemoryHistoryStore::new());
        let conversation_key =
            crate::grok_session::conversation_key_from_raw(Some("stall-finalize"))
                .expect("conversation key");
        let mut stall = crate::task_stall::TaskStallState::default();
        stall.recovery_total = 1;
        stall.epoch_recovery_level = 1;
        stall.recovery_state = crate::task_stall::StallRecoveryState::Warned;
        stall.tool_results_since_progress = 13;
        stall.post_recovery_tool_results_without_progress = 8;
        let mut investigation = crate::investigation::InvestigationState::default();
        investigation.task_stall = stall.clone();
        let mut snapshot =
            crate::recovery_snapshot::ConversationRecoverySnapshotV1::new(&conversation_key);
        snapshot.investigation = Some(investigation);
        snapshot.stall = Some(stall);
        snapshot.task_revision = 1;
        snapshot.last_user_turn_id = Some("turn-stall".into());
        history.put_recovery_snapshot(&snapshot).unwrap();

        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![chat_route()],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        )
        .with_history_store(history.clone())
        .with_task_recovery_policies(
            crate::task_stall::TaskStallPolicy::recover(),
            crate::task_efficiency::TaskEfficiencyPolicy::recover(),
        );

        let mut request = request(json!({
            "model": "vlm-test",
            "conversation_id": "stall-finalize",
            "input": [{"role": "user", "content": "finish the current task"}],
            "tools": [{
                "type": "function",
                "name": "exec_command",
                "description": "run a command",
                "parameters": {"type": "object"}
            }]
        }));
        request.metadata.codex_identity = Some(crate::codex_metadata::CodexTurnIdentity {
            turn_id: Some(
                crate::codex_metadata::parse_codex_opaque_id("turn_id", "turn-stall").unwrap(),
            ),
            trust: crate::codex_metadata::CodexIdentityTrust::Exact,
            ..Default::default()
        });
        assert_eq!(
            runtime.resolve_request_conversation_key(&request).key,
            conversation_key
        );
        runtime
            .execute(request, "stall-finalize-exec")
            .await
            .unwrap();

        let forwarded = parsed_request_body(&transport, 0);
        assert!(forwarded.get("tools").is_none(), "{forwarded}");
        assert!(
            forwarded
                .to_string()
                .contains("No further tool calls are available"),
            "tool-disabled finalization guidance must reach the provider: {forwarded}"
        );
        assert!(!forwarded.to_string().contains("Switch to task execution"));
        let persisted = history
            .recovery_snapshot(&conversation_key)
            .unwrap()
            .expect("persisted recovery state");
        assert_eq!(persisted.stall.as_ref().unwrap().recovery_total, 2);
        assert_eq!(persisted.stall.as_ref().unwrap().epoch_recovery_level, 2);
    }

    #[test]
    fn dispatch_stages_record_elapsed_marks_in_order() {
        let mut stages = DispatchStages::new();
        stages.mark("hydrated");
        stages.mark("prepared");
        let summary = stages.summary();
        assert!(summary.starts_with("hydrated="), "{summary}");
        assert!(summary.contains(" prepared="), "{summary}");
        let hydrated = stages.stages[0].1;
        let prepared = stages.stages[1].1;
        assert!(prepared >= hydrated);
    }

    #[test]
    fn official_usage_identity_distinguishes_control_a_from_execution_b() {
        let mut request = request(json!({"model": "official"}));
        request.incoming_auth.openai_account = Some("control-a".into());
        let auth = ResolvedAuth::OfficialManaged {
            token: "never-log-this".into(),
            account_id: Some("execution-b".into()),
            selection_revision: Some(7),
            selection_verified: true,
        };

        let (control, execution) = official_usage_identity(&auth, &request);

        assert_eq!(control, Some(hashed_account_identity("control-a")));
        assert_eq!(execution, Some(hashed_account_identity("execution-b")));
        assert_ne!(control, execution);
        assert!(!control.unwrap().contains("control-a"));
        assert!(!execution.unwrap().contains("execution-b"));
    }

    /// A transport that records every request it receives and answers from a
    /// script in call order ??the observability `FixtureTransport` lacks.
    struct RecordingTransport {
        script: Vec<UpstreamResponse>,
        requests: std::sync::Mutex<Vec<UpstreamRequest>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl RecordingTransport {
        fn new(script: Vec<UpstreamResponse>) -> Self {
            Self {
                script,
                requests: std::sync::Mutex::new(Vec::new()),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl UpstreamTransport for RecordingTransport {
        async fn execute(
            &self,
            request: &UpstreamRequest,
        ) -> Result<UpstreamResponse, TransportError> {
            self.requests.lock().unwrap().push(request.clone());
            let index = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.script[index.min(self.script.len() - 1)].clone())
        }
    }

    fn scripted_ok_response() -> UpstreamResponse {
        UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: b"{}".to_vec(),
        }
    }

    #[test]
    fn every_upstream_json_request_declares_its_content_type() {
        let headers = json_upstream_headers(
            &ResolvedAuth::None,
            &sample_route(None),
            &serde_json::json!({}),
            b"{}",
        );
        assert!(headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("content-type") && value == "application/json"
        }));
    }

    #[test]
    fn upstream_limit_error_names_the_budget_not_the_body() {
        let error = upstream_limit_error(
            "openAiCompatible",
            "mock",
            "third-party non-stream response",
            THIRD_PARTY_NON_STREAM_MAX_BYTES,
            THIRD_PARTY_NON_STREAM_MAX_BYTES + 1,
        );
        assert_eq!(error.category(), "provider_protocol");
        let message = error.to_string();
        assert!(message.contains("provider=openAiCompatible"), "{message}");
        assert!(message.contains("route=mock"), "{message}");
        assert!(
            message.contains(&format!("limit={THIRD_PARTY_NON_STREAM_MAX_BYTES}")),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "bytes_read={}",
                THIRD_PARTY_NON_STREAM_MAX_BYTES + 1
            )),
            "{message}"
        );
        assert!(!message.contains("sk-"), "{message}");
    }

    fn env() -> ExecutionEnvironment {
        ExecutionEnvironment {
            platform: RuntimePlatform::Linux,
            shell: RuntimeShellKind::Bash,
            shell_version: None,
            supports_and_and: true,
            has_unix_utilities: true,
            path_style: RuntimePathStyle::Posix,
            ampersand_semantics: RuntimeAmpersandSemantics::PosixBackground,
            verified_capabilities: vec![ExecutorCapability::ArgvSafeExec],
        }
    }

    fn request(body: Value) -> RuntimeRequest {
        RuntimeRequest {
            body,
            endpoint: RuntimeEndpoint::Responses,
            incoming_auth: IncomingAuthContext::default(),
            execution_environment: env(),
            metadata: RequestMetadata {
                request_id: "req-test".into(),
                received_at_ms: 0,
                review_run_id: None,
                review_role: None,
                primary_failure_reason: None,
                connection_id: None,
                ..Default::default()
            },
        }
    }

    fn sample_route(credential_id: Option<String>) -> RuntimeModelRoute {
        RuntimeModelRoute {
            route_id: "route-1".into(),
            catalog_id: "vlm-test".into(),
            name: "Test".into(),
            base_url: "http://127.0.0.1:0/v1".into(),
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            auth_kind: RuntimeAuthKind::Bearer,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: false,
            streaming: true,
            reasoning: true,
            vision: false,
            upstream_model: "test-model".into(),
            context_window: Some(128_000),
            reasoning_capabilities: RuntimeReasoningCapabilities::default(),
            compaction_capabilities: RuntimeCompactionCapabilities::default(),
            // This shared fixture predates V2 promotion and most callers
            // assert the legacy V1 wire/checkpoint shape. V2-specific tests
            // opt in explicitly below.
            compaction_policy: crate::config::RuntimeCompactionPolicy {
                ..Default::default()
            },
            tool_capabilities: RuntimeToolCapabilities::default(),
            credential_id,
            insecure_http_policy: crate::outbound::InsecureHttpPolicy::Deny,
            provider_profile: None,
            access_mode: None,
            chat_capabilities: crate::route::RuntimeChatCapabilities::default(),
        }
    }

    struct FixedCatalog {
        routes: Vec<RuntimeModelRoute>,
    }

    impl RouteCatalog for FixedCatalog {
        fn active_models(&self) -> Vec<RuntimeModelRoute> {
            self.routes.clone()
        }

        fn resolve_model(&self, catalog_id: &str) -> Option<ResolvedRoute> {
            self.routes
                .iter()
                .find(|route| route.catalog_id == catalog_id)
                .cloned()
                .map(|route| ResolvedRoute { route })
        }

        fn resolve_review_model(&self, catalog_id: &str) -> Option<ResolvedRoute> {
            self.resolve_model(catalog_id)
        }
    }

    fn runtime_with(route: RuntimeModelRoute) -> ProxyRuntime {
        let credentials: Arc<dyn CredentialProvider> = Arc::new(MemoryCredentialProvider::new());
        ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            credentials,
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            Arc::new(FixtureTransport::new(vec![UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: b"{}".to_vec(),
            }])),
        )
    }

    fn completed_test_response(id: &str) -> UpstreamResponse {
        UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&json!({
                "id": id,
                "object": "response",
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }))
            .unwrap(),
        }
    }

    #[tokio::test]
    async fn normalized_codex_identity_owns_persisted_history_and_context_rollover() {
        use crate::codex_metadata::{
            CodexIdentitySource, CodexIdentityTrust, CodexOpaqueId, CodexTurnIdentity,
        };

        let mut route = sample_route(None);
        route.auth_kind = RuntimeAuthKind::None;
        let history = Arc::new(MemoryHistoryStore::new());
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            Arc::new(FixtureTransport::new(vec![
                completed_test_response("resp_parent"),
                completed_test_response("resp_child_1"),
                completed_test_response("resp_child_2"),
                completed_test_response("resp_conflict"),
            ])),
        )
        .with_history_store(history.clone());

        let identity = |thread: &str,
                        parent: Option<&str>,
                        context_window: Option<&str>,
                        trust: CodexIdentityTrust| CodexTurnIdentity {
            session_id: Some(CodexOpaqueId::new("session_id", "session").unwrap()),
            thread_id: Some(CodexOpaqueId::new("thread_id", thread).unwrap()),
            parent_thread_id: parent
                .map(|value| CodexOpaqueId::new("parent_thread_id", value).unwrap()),
            context_window_id: context_window
                .map(|value| CodexOpaqueId::new("context_window_id", value).unwrap()),
            source: CodexIdentitySource::TurnMetadataHeader,
            trust,
            ..Default::default()
        };

        let mut parent = request(json!({"model": "vlm-test", "input": []}));
        parent.metadata.request_id = "req_parent".into();
        parent.metadata.codex_identity = Some(identity(
            "parent",
            None,
            Some("window_parent"),
            CodexIdentityTrust::Structured,
        ));
        runtime.execute(parent, "exec_parent").await.unwrap();

        for (index, window) in ["window_child_1", "window_child_2"].into_iter().enumerate() {
            let mut child = request(json!({"model": "vlm-test", "input": []}));
            child.metadata.request_id = format!("req_child_{index}");
            child.metadata.codex_identity = Some(identity(
                "child",
                Some("parent"),
                Some(window),
                CodexIdentityTrust::Exact,
            ));
            runtime
                .execute(child, &format!("exec_child_{index}"))
                .await
                .unwrap();
        }

        let mut conflict = request(json!({
            "model": "vlm-test",
            "input": [],
            "client_metadata": {
                "session_id": "spoofed_session",
                "thread_id": "spoofed_thread"
            }
        }));
        conflict.metadata.request_id = "req_conflict".into();
        conflict.metadata.codex_identity = Some(identity(
            "conflicted",
            Some("other_parent"),
            None,
            CodexIdentityTrust::Conflict,
        ));
        runtime.execute(conflict, "exec_conflict").await.unwrap();

        let parent_key = history
            .response_conversation_key("resp_parent")
            .unwrap()
            .unwrap();
        let child_key_1 = history
            .response_conversation_key("resp_child_1")
            .unwrap()
            .unwrap();
        let child_key_2 = history
            .response_conversation_key("resp_child_2")
            .unwrap()
            .unwrap();
        let conflict_key = history
            .response_conversation_key("resp_conflict")
            .unwrap()
            .unwrap();

        assert_eq!(parent_key, "codex:session:parent");
        assert_eq!(child_key_1, "codex:session:child");
        assert_eq!(child_key_2, child_key_1);
        assert_ne!(parent_key, child_key_1);
        assert_eq!(conflict_key, "gen_req_conflict");
        assert!(!conflict_key.starts_with("codex:"));
    }

    #[test]
    fn official_owned_compaction_is_preserved_for_official_dispatch() {
        let runtime = runtime_with(sample_route(None));
        let mut body = json!({
            "input": [
                {
                    "type": "compaction",
                    "id": "cmp_official_123",
                    "encrypted_content": "opaque-openai-compaction"
                },
                {"type": "message", "role": "user", "content": "continue"}
            ]
        });
        let expected = body.clone();

        runtime
            .materialize_local_compactions(
                RuntimeProviderKind::Official,
                &mut body,
                false,
                false,
                &mut ReplayContext::default(),
            )
            .unwrap();

        assert_eq!(body, expected);
    }

    #[tokio::test]
    async fn official_dispatch_forwards_official_owned_compaction_end_to_end() {
        let mut route = sample_route(None);
        route.provider_kind = RuntimeProviderKind::Official;
        route.auth_kind = RuntimeAuthKind::None;
        route.upstream_model = "gpt-5.6-sol".into();
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&json!({
                "id": "resp_official_compaction",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": "msg_official_compaction",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{
                        "type": "output_text",
                        "text": "continued",
                        "annotations": []
                    }]
                }],
                "usage": {"input_tokens": 4, "output_tokens": 1, "total_tokens": 5}
            }))
            .unwrap(),
        }]));
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );
        let official_compaction = json!({
            "type": "compaction",
            "id": "cmp_official_123",
            "encrypted_content": "opaque-openai-compaction"
        });

        runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "input": [
                        official_compaction.clone(),
                        {"type": "message", "role": "user", "content": "continue"}
                    ]
                })),
                "test_exec",
            )
            .await
            .unwrap();

        let upstream = parsed_request_body(&transport, 0);
        assert_eq!(upstream["input"][0], official_compaction);
    }

    #[tokio::test]
    async fn third_party_snapshot_without_previous_id_is_sanitized_before_official_dispatch() {
        let mut route = sample_route(None);
        route.provider_kind = RuntimeProviderKind::Official;
        route.auth_kind = RuntimeAuthKind::None;
        route.upstream_model = "gpt-5.6-sol".into();
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&json!({
                "id": "resp_official_after_handoff",
                "object": "response",
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 4, "output_tokens": 1, "total_tokens": 5}
            }))
            .unwrap(),
        }]));
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );

        runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "input": [{
                        "type": "reasoning",
                        "id": "resp_vellum_1786621788663_reasoning",
                        "summary": [{"type": "summary_text", "text": "foreign reasoning"}]
                    }, {
                        "type": "message",
                        "id": "resp_vellum_1786621788663_message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "visible answer"}]
                    }, {
                        "role": "user",
                        "content": "continue with GPT"
                    }]
                })),
                "test_exec",
            )
            .await
            .unwrap();

        let upstream = parsed_request_body(&transport, 0);
        let encoded = serde_json::to_string(&upstream).unwrap();
        assert!(!encoded.contains("resp_vellum_1786621788663_reasoning"));
        assert!(!encoded.contains("foreign reasoning"));
        assert_eq!(
            upstream["input"][0]["id"].as_str().unwrap().get(..4),
            Some("msg_")
        );
        assert_eq!(upstream["input"][1]["content"], "continue with GPT");
    }

    #[tokio::test]
    async fn official_compaction_trigger_stays_on_native_responses_endpoint() {
        let mut route = sample_route(None);
        route.provider_kind = RuntimeProviderKind::Official;
        route.auth_kind = RuntimeAuthKind::None;
        route.upstream_model = "gpt-5.6-sol".into();
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&json!({
                "id": "resp_official_trigger",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "compaction",
                    "id": "cmp_official_trigger",
                    "encrypted_content": "opaque-openai-compaction"
                }],
                "usage": {"input_tokens": 4, "output_tokens": 1, "total_tokens": 5}
            }))
            .unwrap(),
        }]));
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );

        runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "stream": false,
                    "input": [
                        {"type": "message", "role": "user", "content": "compact this"},
                        {"type": "compaction_trigger"}
                    ]
                })),
                "test_exec",
            )
            .await
            .unwrap();

        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, "http://127.0.0.1:0/v1/responses");
        let upstream: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(has_compaction_trigger(&upstream));
    }

    #[tokio::test]
    async fn official_buffered_exchange_is_native_passthrough_except_model_mapping() {
        let mut route = sample_route(None);
        route.provider_kind = RuntimeProviderKind::Official;
        route.auth_kind = RuntimeAuthKind::None;
        route.upstream_model = "gpt-5.6-sol".into();
        route.reasoning_capabilities.supports_persisted_reasoning = true;
        let official_response = json!({
            "id": "resp_official_native",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "reasoning",
                "id": "rs_native",
                "encrypted_content": "opaque-response-ciphertext",
                "summary": []
            }, {
                "type": "message",
                "id": "msg_native",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "done", "annotations": []}]
            }],
            "usage": {"input_tokens": 4, "output_tokens": 1, "total_tokens": 5},
            "openai_extension": {"keep": true}
        });
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&official_response).unwrap(),
        }]));
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );
        let original = json!({
            "model": "vlm-test",
            "store": false,
            "include": ["openai.private_extension"],
            "input": [{
                "type": "reasoning",
                "id": "opaque-native-item-id",
                "encrypted_content": "opaque-request-ciphertext",
                "summary": []
            }, {
                "type": "compaction",
                "id": "cmp_native",
                "encrypted_content": "opaque-compaction"
            }, {
                "type": "message",
                "role": "user",
                "content": "continue"
            }],
            "metadata": {"native": "unchanged"}
        });

        let response = runtime
            .execute(request(original.clone()), "test_exec")
            .await
            .unwrap();

        let mut expected_request = original;
        expected_request["model"] = json!("gpt-5.6-sol");
        assert_eq!(parsed_request_body(&transport, 0), expected_request);
        let RuntimeResponse::Json(response) = response else {
            panic!("buffered Official response must remain buffered JSON");
        };
        assert_eq!(response, official_response);
    }

    #[test]
    fn foreign_compaction_still_fails_closed_for_third_party_dispatch() {
        let runtime = runtime_with(sample_route(None));
        let mut body = json!({
            "input": [{
                "type": "compaction",
                "id": "cmp_official_123",
                "encrypted_content": "opaque-openai-compaction"
            }]
        });

        let error = runtime
            .materialize_local_compactions(
                RuntimeProviderKind::OpenAiCompatible,
                &mut body,
                false,
                false,
                &mut ReplayContext::default(),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            RuntimeError::Compaction(ref message)
                if message.contains("foreign compaction item")
        ));
    }

    #[test]
    fn foreign_compaction_is_sanitizable_during_local_trigger() {
        let runtime = runtime_with(sample_route(None));
        let mut body = json!({
            "input": [
                {"type": "message", "role": "user", "content": "visible task state"},
                {
                    "type": "compaction",
                    "id": "cmp_official_123",
                    "encrypted_content": "opaque-openai-compaction"
                },
                {"type": "compaction_trigger"}
            ]
        });

        runtime
            .materialize_local_compactions(
                RuntimeProviderKind::OpenAiCompatible,
                &mut body,
                true,
                false,
                &mut ReplayContext::default(),
            )
            .unwrap();

        let portable = sanitize_portable_history(body["input"].as_array().unwrap());
        assert_eq!(portable.len(), 1);
        assert_eq!(portable[0]["content"], "visible task state");
    }

    #[test]
    fn foreign_compaction_is_removed_after_verified_portable_replay() {
        let runtime = runtime_with(sample_route(None));
        let mut body = json!({
            "input": [
                {"type": "message", "role": "user", "content": "hydrated visible state"},
                {
                    "type": "compaction",
                    "id": "cmp_official_123",
                    "encrypted_content": "opaque-openai-compaction"
                }
            ]
        });

        runtime
            .materialize_local_compactions(
                RuntimeProviderKind::OpenAiCompatible,
                &mut body,
                false,
                true,
                &mut ReplayContext::default(),
            )
            .unwrap();

        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert_eq!(body["input"][0]["content"], "hydrated visible state");
    }

    #[test]
    fn official_compaction_is_journaled_portably_in_shared_runtime() {
        let history = MemoryHistoryStore::new();
        let request = json!({
            "model": "gpt-5.6-luna",
            "input": [
                {"type": "message", "role": "user", "content": "portable source"},
                {"type": "reasoning", "encrypted_content": "secret"},
                {"type": "compaction_trigger"}
            ]
        });
        let response = json!({
            "id": "resp_official_compact",
            "output": [{
                "type": "compaction",
                "id": "cmp_official_runtime",
                "encrypted_content": "opaque"
            }]
        });

        record_exchange_with_official_compactions(
            &history,
            &request,
            &response,
            "official-route",
            true,
            Some("codex:test-session:test-thread"),
            Some("realm-test"),
        )
        .unwrap();

        let portable = history
            .canonical_items_for_compaction("cmp_official_runtime")
            .unwrap()
            .unwrap();
        assert_eq!(portable.len(), 1);
        assert_eq!(portable[0]["content"], "portable source");
    }

    #[test]
    fn missing_vellum_compaction_journal_fails_closed_even_on_official_route() {
        let runtime = runtime_with(sample_route(None));
        let mut body = json!({
            "input": [{
                "type": "compaction",
                "id": "cmp_vellum_missing",
                "encrypted_content": format!("{LOCAL_COMPACTION_PREFIX}cmp_vellum_missing")
            }]
        });

        let error = runtime
            .materialize_local_compactions(
                RuntimeProviderKind::Official,
                &mut body,
                false,
                false,
                &mut ReplayContext::default(),
            )
            .unwrap_err();

        assert!(matches!(
            error,
            RuntimeError::Compaction(ref message)
                if message.contains("journal `cmp_vellum_missing` is missing")
        ));
    }

    #[tokio::test]
    async fn unknown_model_returns_route_not_found() {
        let runtime = runtime_with(sample_route(None));
        let error = runtime
            .execute(
                request(serde_json::json!({"model": "missing", "input": "hi"})),
                "test_exec",
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::RouteNotFound(ref m) if m.contains("missing")));
    }

    #[tokio::test]
    async fn missing_model_is_an_invalid_request() {
        let runtime = runtime_with(sample_route(None));
        let error = runtime
            .execute(request(serde_json::json!({"input": "hi"})), "test_exec")
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn stream_true_returns_an_sse_stream_with_frozen_third_party_events() {
        // M5: `stream: true` is wired. The runtime must return a stream whose
        // events match the Desktop third-party normalization contract
        // (created -> delta -> completed, no failed frame), not a downgrade.
        let sse = concat!(
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_stream_1\",\"object\":\"response\",\"status\":\"in_progress\"}}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\",\"output_index\":0,\"content_index\":0}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_stream_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_stream_1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hi\",\"annotations\":[]}]}],\"usage\":{\"input_tokens\":3,\"output_tokens\":1,\"total_tokens\":4}}}\n\n",
        );
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: sse.as_bytes().to_vec(),
        }]));
        let runtime = no_auth_runtime(transport);
        let response = runtime
            .execute(
                request(serde_json::json!({
                    "model": "vlm-test",
                    "input": "hi",
                    "stream": true
                })),
                "test_exec",
            )
            .await
            .unwrap();
        let RuntimeResponse::Sse(stream) = response else {
            panic!("stream: true must produce RuntimeResponse::Sse");
        };
        let mut text = String::new();
        let mut stream = Box::pin(stream);
        while let Some(item) = stream.next().await {
            let bytes = item.unwrap();
            text.push_str(std::str::from_utf8(&bytes).unwrap());
        }
        assert!(
            text.starts_with(": vellum-stream-open"),
            "stream must flush an SSE comment before waiting on upstream: {text}"
        );
        let types = text
            .lines()
            .filter_map(|line| line.strip_prefix("event: "))
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            vec![
                "response.created",
                "response.output_text.delta",
                "response.completed",
            ],
            "unexpected stream events: {text}"
        );
        assert!(!text.contains("response.failed"), "stream failed: {text}");
    }

    struct DelayedBodyTransport {
        delay: std::time::Duration,
        body: Vec<u8>,
    }

    #[async_trait::async_trait]
    impl UpstreamTransport for DelayedBodyTransport {
        async fn execute(
            &self,
            _request: &UpstreamRequest,
        ) -> Result<UpstreamResponse, TransportError> {
            tokio::time::sleep(self.delay).await;
            Ok(UpstreamResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                body: self.body.clone(),
            })
        }

        async fn execute_streaming(
            &self,
            _request: &UpstreamRequest,
        ) -> Result<UpstreamStream, TransportError> {
            let delay = self.delay;
            let body = self.body.clone();
            Ok(UpstreamStream {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                body: async_stream::stream! {
                    tokio::time::sleep(delay).await;
                    yield Ok(Bytes::from(body));
                }
                .boxed(),
            })
        }
    }

    #[tokio::test]
    async fn inbound_sse_comment_is_ready_before_the_upstream_body() {
        let sse = concat!(
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_delay\",\"object\":\"response\",\"status\":\"in_progress\"}}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_delay\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
        );
        let mut route = sample_route(None);
        route.auth_kind = RuntimeAuthKind::None;
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            Arc::new(DelayedBodyTransport {
                delay: std::time::Duration::from_secs(3),
                body: sse.as_bytes().to_vec(),
            }),
        );
        let response = runtime
            .execute(
                request(serde_json::json!({
                    "model": "vlm-test",
                    "input": "hi",
                    "stream": true
                })),
                "test_exec",
            )
            .await
            .unwrap();
        let RuntimeResponse::Sse(mut stream) = response else {
            panic!("stream: true must produce RuntimeResponse::Sse");
        };
        let first = tokio::time::timeout(std::time::Duration::from_millis(200), stream.next())
            .await
            .expect("inbound flush comment must not wait on the delayed upstream body")
            .expect("stream opened")
            .expect("flush frame");
        assert_eq!(first, stream_open_frame());
    }

    /// A transport whose header phase never resolves ??the upstream accepted
    /// the connection but never answered. Used to prove
    /// `STREAM_OPEN_HEADER_TIMEOUT` actually bounds the header-open await.
    struct HangingHeadersTransport;

    #[async_trait::async_trait]
    impl UpstreamTransport for HangingHeadersTransport {
        async fn execute(
            &self,
            _request: &UpstreamRequest,
        ) -> Result<UpstreamResponse, TransportError> {
            std::future::pending().await
        }

        async fn execute_streaming(
            &self,
            _request: &UpstreamRequest,
        ) -> Result<UpstreamStream, TransportError> {
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stream_open_header_timeout_aborts_with_one_pre_header_usage_row() {
        let mut route = sample_route(None);
        route.auth_kind = RuntimeAuthKind::None;
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            Arc::new(HangingHeadersTransport),
        );
        let error = runtime
            .execute(
                request(serde_json::json!({
                    "model": "vlm-test",
                    "input": "hi",
                    "stream": true
                })),
                "test_exec",
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, RuntimeError::StreamOpenTimeout(_)),
            "expected StreamOpenTimeout, got {error:?}"
        );
        assert_eq!(error.category(), "stream_open_timeout");

        // Exactly one usage row: `execute`'s generic on-`Err` recorder is the
        // only writer here since the header phase never produced an
        // `UpstreamStream`, so none of `third_party_stream`'s own bookkeeping
        // ever ran.
        let records = runtime.usage_records().unwrap();
        assert_eq!(records.len(), 1, "exactly one usage row: {records:?}");
        assert_eq!(
            records[0].error_category.as_deref(),
            Some("stream_open_timeout"),
            "usage row must be tagged as a pre-header failure: {:?}",
            records[0]
        );
        assert!(
            records[0].first_byte_ms.is_none(),
            "no stream was ever established, so first_byte_ms must stay empty: {:?}",
            records[0]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_body_after_headers_never_trips_the_header_open_timeout() {
        // Headers open instantly (see `DelayedBodyTransport::execute_streaming`);
        // only the body is slow, and by more than `STREAM_OPEN_HEADER_TIMEOUT`.
        // The header-open bound must never fire once headers already arrived.
        let sse = concat!(
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_slow_body\",\"object\":\"response\",\"status\":\"in_progress\"}}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\",\"output_index\":0,\"content_index\":0}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_slow_body\",\"object\":\"response\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_slow_body\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hi\",\"annotations\":[]}]}],\"usage\":{\"input_tokens\":3,\"output_tokens\":1,\"total_tokens\":4}}}\n\n",
        );
        let mut route = sample_route(None);
        route.auth_kind = RuntimeAuthKind::None;
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            Arc::new(DelayedBodyTransport {
                delay: STREAM_OPEN_HEADER_TIMEOUT + std::time::Duration::from_secs(15),
                body: sse.as_bytes().to_vec(),
            }),
        );
        let response = runtime
            .execute(
                request(serde_json::json!({
                    "model": "vlm-test",
                    "input": "hi",
                    "stream": true
                })),
                "test_exec",
            )
            .await
            .unwrap();
        let RuntimeResponse::Sse(mut stream) = response else {
            panic!("stream: true must produce RuntimeResponse::Sse");
        };
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            text.push_str(std::str::from_utf8(&item.unwrap()).unwrap());
        }
        assert!(
            !text.contains("response.failed") && !text.contains("stream_open_timeout"),
            "a slow body must never trip the header-open timeout: {text}"
        );
        assert!(text.contains("response.completed"), "{text}");
    }

    #[tokio::test]
    async fn bearer_without_credential_reference_fails_closed() {
        // `auth_kind = Bearer` with `credential_id = None` is a
        // misconfiguration. A provider that needs no auth says `auth_kind =
        // None`; a Bearer route must name a credential. Missing credential
        // must never degrade into an anonymous request.
        let runtime = runtime_with(sample_route(None));
        let error = runtime
            .execute(
                request(serde_json::json!({"model": "vlm-test", "input": "hi"})),
                "test_exec",
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, RuntimeError::CredentialMissing(ref m) if m.contains("no credential reference")),
            "bearer without a credential reference must fail closed, got {error:?}"
        );
    }

    #[tokio::test]
    async fn bearer_with_missing_referenced_secret_fails_closed() {
        // The route names a credential, but the provider has no secret for it.
        let runtime = runtime_with(sample_route(Some("grok-key".into())));
        let error = runtime
            .execute(
                request(serde_json::json!({"model": "vlm-test", "input": "hi"})),
                "test_exec",
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, RuntimeError::CredentialMissing(ref m) if m.contains("grok-key")),
            "a route that requires a credential must never run anonymously"
        );
    }

    #[tokio::test]
    async fn a_provisioned_bearer_route_dispatches_and_applies_the_bearer_header() {
        let route = sample_route(Some("key-1".into()));
        let store = MemoryCredentialProvider::new();
        store.insert("key-1", "sk-test");
        let credentials: Arc<dyn CredentialProvider> = Arc::new(store);
        let transport = Arc::new(RecordingTransport::new(vec![scripted_ok_response()]));
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            credentials,
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );
        let response = runtime
            .execute(
                request(serde_json::json!({"model": "vlm-test", "input": "hi"})),
                "test_exec",
            )
            .await
            .unwrap();
        assert!(matches!(response, RuntimeResponse::Json(_)));
        let recorded = transport.requests.lock().unwrap();
        assert_eq!(recorded.len(), 1, "the request must dispatch exactly once");
        assert_eq!(recorded[0].method, "POST");
        assert_eq!(recorded[0].url, "http://127.0.0.1:0/v1/responses");
        let bearer = recorded[0]
            .headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .expect("bearer route must send an authorization header");
        assert_eq!(bearer.1, "Bearer sk-test");
    }

    #[tokio::test]
    async fn unconfigured_official_route_fails_closed() {
        let mut route = sample_route(None);
        route.provider_kind = RuntimeProviderKind::Official;
        route.auth_kind = RuntimeAuthKind::ChatGpt;
        let runtime = runtime_with(route);
        let error = runtime
            .execute(
                request(serde_json::json!({"model": "vlm-test", "input": "hi"})),
                "test_exec",
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::AuthenticationFailed(_)));
    }

    #[tokio::test]
    async fn a_well_formed_request_dispatches_end_to_end() {
        // `auth_kind = None` is the explicit "no auth needed" posture. The
        // request must sail through route + profile + prepare and reach the
        // upstream, with the model already mapped to the upstream model.
        let mut route = sample_route(None);
        route.auth_kind = RuntimeAuthKind::None;
        let transport = Arc::new(RecordingTransport::new(vec![scripted_ok_response()]));
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );
        let response = runtime
            .execute(
                request(serde_json::json!({"model": "vlm-test", "input": "hi"})),
                "test_exec",
            )
            .await
            .unwrap();
        assert!(matches!(response, RuntimeResponse::Json(_)));
        let recorded = transport.requests.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        let body: Value = serde_json::from_slice(&recorded[0].body).unwrap();
        assert_eq!(body["model"], "test-model");
        assert!(recorded[0]
            .headers
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case("authorization")));
        assert!(recorded[0].headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("content-type") && value == "application/json"
        }));
    }

    #[tokio::test]
    async fn admission_fails_when_the_proxy_is_draining() {
        let lifecycle = Arc::new(CountingRequestLifecycle::new());
        lifecycle.begin_drain();
        let credentials: Arc<dyn CredentialProvider> = Arc::new(MemoryCredentialProvider::new());
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![sample_route(None)],
            }),
            credentials,
            Arc::new(UnconfiguredOfficialAuthProvider),
            lifecycle,
            Arc::new(FixtureTransport::new(vec![UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: b"{}".to_vec(),
            }])),
        );
        let error = runtime
            .execute(
                request(serde_json::json!({"model": "vlm-test", "input": "hi"})),
                "test_exec",
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::LifecycleDraining(_)));
    }

    #[tokio::test]
    async fn request_snapshot_is_fixed_at_admission() {
        // The runtime must capture the catalog once at admission and never touch
        // the live catalog again. This catalog hands out one snapshot and then
        // empties itself; if any later step consulted the live catalog it would
        // see no route (or panic). The request succeeding proves resolution ran
        // against the captured snapshot, not the live catalog.
        struct SnapshotPoisonCatalog(std::sync::Mutex<Vec<RuntimeModelRoute>>);

        impl RouteCatalog for SnapshotPoisonCatalog {
            fn active_models(&self) -> Vec<RuntimeModelRoute> {
                let mut live = self.0.lock().unwrap();
                let snapshot = live.clone();
                live.clear();
                snapshot
            }

            fn resolve_model(&self, _catalog_id: &str) -> Option<ResolvedRoute> {
                panic!("live catalog was consulted after snapshot capture")
            }

            fn resolve_review_model(&self, _catalog_id: &str) -> Option<ResolvedRoute> {
                panic!("live catalog was consulted after snapshot capture")
            }
        }

        let mut route = sample_route(None);
        route.auth_kind = RuntimeAuthKind::None;
        let transport = Arc::new(RecordingTransport::new(vec![scripted_ok_response()]));
        let runtime = ProxyRuntime::new(
            Arc::new(SnapshotPoisonCatalog(std::sync::Mutex::new(vec![route]))),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );
        let response = runtime
            .execute(
                request(serde_json::json!({"model": "vlm-test", "input": "hi"})),
                "test_exec",
            )
            .await
            .unwrap();
        assert!(
            matches!(response, RuntimeResponse::Json(_)),
            "snapshot capture must isolate the request from the live catalog"
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }

    fn no_auth_runtime(transport: Arc<RecordingTransport>) -> ProxyRuntime {
        let mut route = sample_route(None);
        route.auth_kind = RuntimeAuthKind::None;
        ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport,
        )
    }

    #[tokio::test]
    async fn non_streaming_dispatch_normalizes_the_upstream_response() {
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "id": "resp_1",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "hi back", "annotations": []}]
                }],
                "usage": {"input_tokens": 2, "output_tokens": 1, "total_tokens": 3}
            }"#
            .to_vec(),
        }]));
        let runtime = no_auth_runtime(transport);
        let response = runtime
            .execute(
                request(serde_json::json!({"model": "vlm-test", "input": "hi"})),
                "test_exec",
            )
            .await
            .unwrap();
        match response {
            RuntimeResponse::Json(body) => {
                assert_eq!(body["output"][0]["type"], "message");
                assert_eq!(body["output"][0]["phase"], "final_answer");
                assert_eq!(body["usage"]["input_tokens"], 2);
            }
            RuntimeResponse::Sse(_) | RuntimeResponse::Raw { .. } => {
                unreachable!("non-streaming dispatch returned a non-JSON response")
            }
        }
    }

    #[tokio::test]
    async fn dispatch_freezes_the_multi_tool_turn_semantics() {
        // The same turn the `responses_multi_tool` parity fixture freezes at the
        // Desktop boundary: two parallel function calls, two outputs, and a
        // closing message that must be `commentary` because the turn contains
        // tool calls.
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&serde_json::json!({
                "id": "resp_1",
                "object": "response",
                "status": "completed",
                "output": [
                    {"type": "function_call", "id": "fc_1", "call_id": "call_weather_taipei", "name": "get_weather", "arguments": "{\"city\":\"Taipei\"}", "status": "completed"},
                    {"type": "function_call", "id": "fc_2", "call_id": "call_weather_tokyo", "name": "get_weather", "arguments": "{\"city\":\"Tokyo\"}", "status": "completed"},
                    {"type": "function_call_output", "call_id": "call_weather_taipei", "output": "{\"temperature\":32}"},
                    {"type": "function_call_output", "call_id": "call_weather_tokyo", "output": "{\"temperature\":28}"},
                    {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "Both are warm.", "annotations": []}]}
                ],
                "usage": {"input_tokens": 10, "output_tokens": 9, "total_tokens": 19}
            }))
            .unwrap()
            .to_vec(),
        }]));
        let runtime = no_auth_runtime(transport);
        let response = runtime
            .execute(
                request(serde_json::json!({"model": "vlm-test", "input": "hi"})),
                "test_exec",
            )
            .await
            .unwrap();
        match response {
            RuntimeResponse::Json(body) => {
                let output = body["output"].as_array().unwrap();
                assert_eq!(output[0]["call_id"], "call_weather_taipei");
                assert_eq!(output[1]["call_id"], "call_weather_tokyo");
                assert_eq!(output[2]["type"], "function_call_output");
                assert_eq!(output[3]["type"], "function_call_output");
                assert_eq!(output[4]["phase"], "commentary");
            }
            RuntimeResponse::Sse(_) | RuntimeResponse::Raw { .. } => {
                unreachable!("non-streaming dispatch returned a non-JSON response")
            }
        }
    }

    #[tokio::test]
    async fn chat_wire_dispatch_adapts_the_upstream_completion_to_responses() {
        let mut route = sample_route(None);
        route.auth_kind = RuntimeAuthKind::None;
        route.wire = RuntimeWireFormat::Chat;
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "from chat"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
            }"#
            .to_vec(),
        }]));
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport,
        );
        let response = runtime
            .execute(
                request(serde_json::json!({"model": "vlm-test", "input": "hi"})),
                "test_exec",
            )
            .await
            .unwrap();
        match response {
            RuntimeResponse::Json(body) => {
                assert_eq!(body["object"], "response");
                assert_eq!(body["output"][0]["type"], "message");
                assert_eq!(body["usage"]["input_tokens"], 1);
            }
            RuntimeResponse::Sse(_) | RuntimeResponse::Raw { .. } => {
                unreachable!("non-streaming dispatch returned a non-JSON response")
            }
        }
    }

    #[tokio::test]
    async fn provider_statuses_map_to_stable_error_categories() {
        let cases: &[(u16, Value, &str)] = &[
            (
                401,
                json!({"error": {"message": "unauthorized", "type": "invalid_request_error"}}),
                "provider unauthorized",
            ),
            (
                429,
                json!({"error": {"message": "rate limited", "type": "rate_limit_error"}}),
                "provider quota",
            ),
            (
                500,
                json!({"error": {"message": "boom", "type": "server_error"}}),
                "provider unavailable",
            ),
            (
                400,
                json!({"error": {"message": "bad request", "type": "invalid_request_error"}}),
                "provider protocol",
            ),
            (
                400,
                json!({"error": {"message": "quota exceeded", "type": "insufficient_quota"}}),
                "provider quota",
            ),
        ];
        for (status, body, expected) in cases {
            let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
                status: *status,
                headers: Vec::new(),
                body: serde_json::to_vec(body).unwrap(),
            }]));
            let runtime = no_auth_runtime(transport);
            let error = runtime
                .execute(
                    request(json!({"model": "vlm-test", "input": "hi"})),
                    "test_exec",
                )
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "status {status} must map to {expected}, got {error:?}"
            );
        }
    }

    #[test]
    fn plain_text_426_body_is_preserved_as_the_diagnostic() {
        let message = provider_error_message(
            b"Your Grok CLI version (none) is outdated. Please update to version 0.1.202.",
            426,
        );
        assert!(message.contains("outdated"), "{message}");
        assert!(message.contains("0.1.202"), "{message}");
        assert!(!message.contains("upstream stream failed"), "{message}");
    }

    #[test]
    fn json_error_and_detail_are_both_kept() {
        let message = provider_error_message(
            br#"{"error":"version none","detail":"update via grok update"}"#,
            426,
        );
        assert!(message.contains("version none"), "{message}");
        assert!(message.contains("update via grok update"), "{message}");
    }

    #[test]
    fn json_error_object_and_detail_are_both_kept() {
        let message = provider_error_message(
            br#"{"error":{"message":"bad request","code":"invalid"},"detail":"Input must be a list"}"#,
            400,
        );
        assert!(message.contains("bad request"), "{message}");
        assert!(message.contains("invalid"), "{message}");
        assert!(message.contains("Input must be a list"), "{message}");
    }

    #[test]
    fn provider_protocol_diagnostic_keeps_the_upstream_http_status() {
        let error = provider_error_for_status(400, "messages parameter is illegal");
        let rendered = error.to_string();
        assert!(rendered.contains("HTTP 400"), "{rendered}");
        assert!(
            rendered.contains("messages parameter is illegal"),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn a_plain_text_error_status_keeps_the_provider_body() {
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 426,
            headers: Vec::new(),
            body: b"Your Grok CLI version (none) is outdated.".to_vec(),
        }]));
        let runtime = no_auth_runtime(transport);
        let error = runtime
            .execute(
                request(json!({"model": "vlm-test", "input": "hi"})),
                "test_exec",
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::ProviderProtocol(_)));
        assert!(error.to_string().contains("outdated"), "{error}");
    }

    #[tokio::test]
    async fn a_non_json_upstream_body_is_a_provider_protocol_error() {
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: b"<html>gateway</html>".to_vec(),
        }]));
        let runtime = no_auth_runtime(transport);
        let error = runtime
            .execute(
                request(json!({"model": "vlm-test", "input": "hi"})),
                "test_exec",
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::ProviderProtocol(_)));
    }

    /// A credential provider that counts reads, so a test can prove the auth
    /// posture is resolved exactly once per request.
    struct CountingCredentialProvider {
        inner: MemoryCredentialProvider,
        reads: std::sync::atomic::AtomicUsize,
    }

    impl CountingCredentialProvider {
        fn new() -> Self {
            Self {
                inner: MemoryCredentialProvider::new(),
                reads: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn insert(&self, id: &str, value: &str) {
            self.inner.insert(id, value);
        }

        fn read_count(&self) -> usize {
            self.reads.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl CredentialProvider for CountingCredentialProvider {
        async fn get_secret(&self, id: &str) -> Result<Option<String>, String> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.get_secret(id).await
        }

        async fn list_ids(&self) -> Result<Vec<String>, String> {
            self.inner.list_ids().await
        }
    }

    #[tokio::test]
    async fn auth_is_resolved_exactly_once_per_request() {
        // The dispatch must consume the `ResolvedAuth` value resolved before the
        // adapter ran; it must never re-read the credential store. Rotating the
        // secret after resolution must therefore not leak into this request's
        // headers, and the store is read exactly once.
        let store = Arc::new(CountingCredentialProvider::new());
        store.insert("key-1", "sk-one");
        let route = sample_route(Some("key-1".into()));
        let transport = Arc::new(RecordingTransport::new(vec![scripted_ok_response()]));
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            store.clone(),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );
        runtime
            .execute(
                request(json!({"model": "vlm-test", "input": "hi"})),
                "test_exec",
            )
            .await
            .unwrap();
        let recorded = transport.requests.lock().unwrap();
        let bearer = recorded[0]
            .headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .expect("bearer route must send an authorization header");
        assert_eq!(bearer.1, "Bearer sk-one");
        assert_eq!(
            store.read_count(),
            1,
            "the credential store must be read exactly once per request"
        );
    }

    /// A catalog that hands out an explicit model-visible entry per route, the
    /// way a production config ships `base_instructions`.
    struct CatalogWithEntry {
        routes: Vec<RuntimeModelRoute>,
        entries: std::collections::HashMap<String, serde_json::Value>,
    }

    impl RouteCatalog for CatalogWithEntry {
        fn active_models(&self) -> Vec<RuntimeModelRoute> {
            self.routes.clone()
        }

        fn resolve_model(&self, catalog_id: &str) -> Option<ResolvedRoute> {
            self.routes
                .iter()
                .find(|route| route.catalog_id == catalog_id)
                .cloned()
                .map(|route| ResolvedRoute { route })
        }

        fn resolve_review_model(&self, catalog_id: &str) -> Option<ResolvedRoute> {
            self.resolve_model(catalog_id)
        }

        fn catalog_entry(&self, catalog_id: &str) -> Option<serde_json::Value> {
            self.entries.get(catalog_id).cloned()
        }
    }

    #[tokio::test]
    async fn the_catalog_entry_is_the_request_time_baseline_authority() {
        // P0-2: the entry the adapter verifies against and replaces the prompt
        // baseline from must come from the catalog, not a synthetic projection.
        // With `base_instructions` present and a matching incoming
        // `instructions`, the tool-aware prompt replaces the baseline instead
        // of being appended to it.
        let mut route = sample_route(None);
        route.auth_kind = RuntimeAuthKind::None;
        let baseline = "the catalog baseline";
        let catalog = CatalogWithEntry {
            routes: vec![route],
            entries: [(
                "vlm-test".to_string(),
                json!({
                    "id": "vlm-test",
                    "name": "Test",
                    "route_id": "route-1",
                    "base_url": "http://127.0.0.1:0/v1",
                    "provider_kind": "openAiCompatible",
                    "wire": "responses",
                    "upstream_model": "test-model",
                    "context_window": 128000,
                    "base_instructions": baseline,
                    "use_responses_lite": false,
                    "supports_parallel_tool_calls": false,
                }),
            )]
            .into_iter()
            .collect(),
        };
        let transport = Arc::new(RecordingTransport::new(vec![scripted_ok_response()]));
        let runtime = ProxyRuntime::new(
            Arc::new(catalog),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );
        runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "input": "hi",
                    "instructions": baseline,
                    "tools": [{
                        "type": "function",
                        "name": "get_weather",
                        "description": "Get the weather",
                        "parameters": {"type": "object"}
                    }]
                })),
                "test_exec",
            )
            .await
            .unwrap();
        let recorded = transport.requests.lock().unwrap();
        let body: Value = serde_json::from_slice(&recorded[0].body).unwrap();
        let instructions = body["instructions"].as_str().unwrap();
        assert!(
            !instructions.contains(baseline),
            "the catalog baseline must be replaced by the tool-aware prompt, not appended to: {instructions}"
        );
        assert!(
            instructions.contains("get_weather"),
            "the replacement prompt must describe the tool surface: {instructions}"
        );
    }

    /// A fake managed Official provider that refreshes once and dedups against
    /// an already-superseded rejected token.
    struct RefreshingOfficial {
        token: std::sync::Mutex<String>,
        refresh_calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl OfficialAuthProvider for RefreshingOfficial {
        async fn authorize(
            &self,
            _route_id: &str,
        ) -> Result<crate::official_auth::OfficialAuthDecision, String> {
            Ok(crate::official_auth::OfficialAuthDecision::Managed(
                crate::official_auth::OfficialAuthorization {
                    access_token: self.token.lock().unwrap().clone(),
                    account_id: Some("acct-1".into()),
                    selection_revision: Some(0),
                    selection_verified: true,
                },
            ))
        }

        async fn refresh_after_rejection(
            &self,
            _route_id: &str,
            _rejected: &crate::official_auth::OfficialAuthorization,
        ) -> Result<crate::official_auth::OfficialAuthorization, String> {
            self.refresh_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut token = self.token.lock().unwrap();
            *token = "refreshed-token".into();
            Ok(crate::official_auth::OfficialAuthorization {
                access_token: token.clone(),
                account_id: Some("acct-1".into()),
                selection_revision: Some(0),
                selection_verified: true,
            })
        }
    }

    fn official_runtime_with_transport(transport: Arc<RecordingTransport>) -> ProxyRuntime {
        let mut route = sample_route(None);
        route.provider_kind = RuntimeProviderKind::Official;
        route.auth_kind = RuntimeAuthKind::ChatGpt;
        ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(RefreshingOfficial {
                token: std::sync::Mutex::new("official-token".into()),
                refresh_calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(CountingRequestLifecycle::new()),
            transport,
        )
    }

    #[tokio::test]
    async fn official_websocket_plan_uses_native_endpoint_and_refreshes_managed_auth_once() {
        let transport = Arc::new(RecordingTransport::new(Vec::new()));
        let runtime = official_runtime_with_transport(transport);
        let request = request(json!({
            "type": "response.create",
            "model": "vlm-test",
            "input": "hi",
            "stream": true
        }));
        let mut plan = runtime
            .prepare_official_websocket(&request)
            .await
            .unwrap()
            .expect("Official Responses route must select native WebSocket");
        assert_eq!(plan.upstream_url, "ws://127.0.0.1:0/v1/responses");
        assert_eq!(plan.upstream_model, "test-model");
        assert!(plan
            .auth_headers
            .iter()
            .any(|(name, value)| name == "authorization" && value == "Bearer official-token"));

        assert!(runtime
            .refresh_official_websocket_auth(&mut plan)
            .await
            .unwrap());
        assert!(plan
            .auth_headers
            .iter()
            .any(|(name, value)| name == "authorization" && value == "Bearer refreshed-token"));
    }

    #[tokio::test]
    async fn legacy_local_official_response_without_realm_gets_one_portable_handoff() {
        let history = Arc::new(MemoryHistoryStore::new());
        history
            .record_exchange(
                &json!({
                    "model": "vlm-test",
                    "input": [{"role": "user", "content": "legacy question"}]
                }),
                &json!({
                    "id": "resp_legacy_without_realm",
                    "output": [{"role": "assistant", "content": "legacy answer"}]
                }),
                "route-1",
            )
            .unwrap();
        let runtime =
            official_runtime_with_transport(Arc::new(RecordingTransport::new(Vec::new())))
                .with_history_store(history);

        let local = request(json!({
            "type": "response.create",
            "model": "vlm-test",
            "previous_response_id": "resp_legacy_without_realm",
            "input": [{"role": "user", "content": "continue"}],
            "stream": true
        }));
        assert!(matches!(
            runtime
                .prepare_websocket_turn(&local)
                .await
                .unwrap()
                .dispatch,
            WebSocketTurnDispatch::PortableReplay {
                force_official_handoff: true
            }
        ));

        let external = request(json!({
            "type": "response.create",
            "model": "vlm-test",
            "previous_response_id": "resp_not_in_local_history",
            "input": [{"role": "user", "content": "continue"}],
            "stream": true
        }));
        assert!(matches!(
            runtime
                .prepare_websocket_turn(&external)
                .await
                .unwrap()
                .dispatch,
            WebSocketTurnDispatch::OfficialNative(_)
        ));
    }

    #[test]
    fn official_refresh_must_preserve_the_rejected_selection_snapshot() {
        let rejected = crate::official_auth::OfficialAuthorization {
            access_token: "old-token".into(),
            account_id: Some("acct-a".into()),
            selection_revision: Some(7),
            selection_verified: true,
        };
        let same_snapshot = crate::official_auth::OfficialAuthorization {
            access_token: "new-token".into(),
            account_id: Some("acct-a".into()),
            selection_revision: Some(7),
            selection_verified: true,
        };
        assert!(official_refresh_matches_snapshot(&rejected, &same_snapshot));

        let switched_account = crate::official_auth::OfficialAuthorization {
            account_id: Some("acct-b".into()),
            ..same_snapshot.clone()
        };
        let switched_revision = crate::official_auth::OfficialAuthorization {
            selection_revision: Some(8),
            ..same_snapshot.clone()
        };
        let unverified = crate::official_auth::OfficialAuthorization {
            selection_verified: false,
            ..same_snapshot
        };
        assert!(!official_refresh_matches_snapshot(
            &rejected,
            &switched_account
        ));
        assert!(!official_refresh_matches_snapshot(
            &rejected,
            &switched_revision
        ));
        assert!(!official_refresh_matches_snapshot(&rejected, &unverified));
    }

    #[tokio::test]
    async fn official_websocket_plan_defers_portable_handoff_to_http_sanitization() {
        let transport = Arc::new(RecordingTransport::new(Vec::new()));
        let runtime = official_runtime_with_transport(transport);
        let request = request(json!({
            "type": "response.create",
            "model": "vlm-test",
            "input": [{
                "type": "message",
                "id": "resp_vellum_portable",
                "role": "user",
                "content": [{"type": "input_text", "text": "hi"}]
            }],
            "stream": true
        }));

        assert!(runtime
            .prepare_official_websocket(&request)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn official_buffered_request_retries_without_rejected_tool_choice() {
        let transport = Arc::new(RecordingTransport::new(vec![
            UpstreamResponse {
                status: 400,
                headers: Vec::new(),
                body: br#"{"error":{"message":"Unknown parameter: 'tool_choice'.","type":"invalid_request_error","param":"tool_choice","code":"unknown_parameter"}}"#.to_vec(),
            },
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: br#"{"id":"resp_ok","object":"response","status":"completed","output":[]}"#.to_vec(),
            },
        ]));
        let runtime = official_runtime_with_transport(transport.clone());

        runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "input": "hi",
                    "tool_choice": "auto"
                })),
                "test_exec",
            )
            .await
            .unwrap();

        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let second: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(first["tool_choice"], "auto");
        assert!(second.get("tool_choice").is_none());
        assert_eq!(first["model"], second["model"]);
        assert_eq!(first["input"], second["input"]);
    }

    #[tokio::test]
    async fn official_streaming_request_retries_without_rejected_tool_choice() {
        let completed = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_ok\",\"object\":\"response\",\"status\":\"completed\",\"output\":[]}}\n\n"
        );
        let transport = Arc::new(RecordingTransport::new(vec![
            UpstreamResponse {
                status: 400,
                headers: vec![("content-type".into(), "application/json".into())],
                body: br#"{"error":{"message":"Unknown parameter: 'tool_choice'.","type":"invalid_request_error","param":"tool_choice","code":"unknown_parameter"}}"#.to_vec(),
            },
            UpstreamResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                body: completed.as_bytes().to_vec(),
            },
        ]));
        let runtime = official_runtime_with_transport(transport.clone());

        let response = runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "input": "hi",
                    "stream": true,
                    "tool_choice": "auto"
                })),
                "test_exec",
            )
            .await
            .unwrap();
        assert!(matches!(response, RuntimeResponse::Sse(_)));

        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let second: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(first["tool_choice"], "auto");
        assert!(second.get("tool_choice").is_none());
        assert_eq!(first["stream"], second["stream"]);
    }

    #[tokio::test]
    async fn official_search_refreshes_and_preserves_wire_response() {
        let mut route = sample_route(None);
        route.provider_kind = RuntimeProviderKind::Official;
        route.auth_kind = RuntimeAuthKind::ChatGpt;
        route.base_url = "https://api.openai.test/v1".into();
        let transport = Arc::new(RecordingTransport::new(vec![
            UpstreamResponse {
                status: 401,
                headers: vec![("content-type".into(), "application/json".into())],
                body: br#"{"error":"expired"}"#.to_vec(),
            },
            UpstreamResponse {
                status: 206,
                headers: vec![("content-type".into(), "application/x-test".into())],
                body: b"opaque-search-body".to_vec(),
            },
        ]));
        let official = Arc::new(RefreshingOfficial {
            token: std::sync::Mutex::new("stale-token".into()),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            official.clone(),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );
        let mut search_request = request(json!({
            "model": "vlm-test",
            "commands": {"search_query": [{"q": "rust"}]}
        }));
        search_request.endpoint = crate::request::RuntimeEndpoint::Search;
        let response = runtime.execute_search(search_request).await.unwrap();
        let RuntimeResponse::Raw {
            status,
            content_type,
            body,
        } = response
        else {
            panic!("Official search must preserve its raw wire response");
        };
        assert_eq!(status, 206);
        assert_eq!(content_type.as_deref(), Some("application/x-test"));
        assert_eq!(body, b"opaque-search-body");
        assert_eq!(
            official
                .refresh_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].url, "https://api.openai.test/v1/alpha/search");
        assert_eq!(requests[0].body, requests[1].body);
        assert!(requests[0]
            .headers
            .iter()
            .any(|(_, value)| value == "Bearer stale-token"));
        assert!(requests[1]
            .headers
            .iter()
            .any(|(_, value)| value == "Bearer refreshed-token"));
    }

    #[tokio::test]
    async fn official_managed_401_refreshes_once_and_retries_the_same_request() {
        // P1-1: a 401 on a Vellum-managed Official token must refresh once and
        // resend the identical prepared request with the new token ??never a
        // second `ResolvedAuth::resolve`, never a different auth kind.
        let mut route = sample_route(None);
        route.provider_kind = RuntimeProviderKind::Official;
        route.auth_kind = RuntimeAuthKind::ChatGpt;
        let transport = Arc::new(RecordingTransport::new(vec![
            UpstreamResponse {
                status: 401,
                headers: Vec::new(),
                body: br#"{"error":{"message":"expired token"}}"#.to_vec(),
            },
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: br#"{
                    "id": "resp_ok",
                    "object": "response",
                    "status": "completed",
                    "output": [],
                    "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
                }"#
                .to_vec(),
            },
        ]));
        let official = Arc::new(RefreshingOfficial {
            token: std::sync::Mutex::new("stale-token".into()),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            official.clone(),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );
        let response = runtime
            .execute(
                request(json!({"model": "vlm-test", "input": "hi"})),
                "test_exec",
            )
            .await
            .unwrap();
        assert!(matches!(response, RuntimeResponse::Json(_)));
        let recorded = transport.requests.lock().unwrap();
        assert_eq!(
            recorded.len(),
            2,
            "a managed Official 401 must dispatch exactly twice (original + one refresh retry)"
        );
        assert_eq!(
            recorded[0].body, recorded[1].body,
            "the retry must resend the identical prepared request body"
        );
        let bearer = |index: usize| {
            recorded[index]
                .headers
                .iter()
                .find(|(name, _)| name == "authorization")
                .expect("managed Official must send an authorization header")
                .1
                .clone()
        };
        assert_eq!(bearer(0), "Bearer stale-token");
        assert_eq!(bearer(1), "Bearer refreshed-token");
        assert_eq!(
            official
                .refresh_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the refresh must happen exactly once"
        );
    }

    #[tokio::test]
    async fn a_non_official_401_is_reported_and_never_retried() {
        // The refresh retry is reserved for Vellum-managed Official tokens.
        // A plain no-auth 401 is surfaced to the caller after one dispatch.
        let mut route = sample_route(None);
        route.auth_kind = RuntimeAuthKind::None;
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 401,
            headers: Vec::new(),
            body: br#"{"error":{"message":"unauthorized"}}"#.to_vec(),
        }]));
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );
        let error = runtime
            .execute(
                request(json!({"model": "vlm-test", "input": "hi"})),
                "test_exec",
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::ProviderUnauthorized(_)));
        assert_eq!(
            transport.requests.lock().unwrap().len(),
            1,
            "a non-Official 401 must not be retried"
        );
    }

    /// A transport that always fails with a fixed, typed error.
    struct FailingTransport(TransportError);

    #[async_trait::async_trait]
    impl UpstreamTransport for FailingTransport {
        async fn execute(
            &self,
            _request: &UpstreamRequest,
        ) -> Result<UpstreamResponse, TransportError> {
            Err(self.0.clone())
        }
    }

    #[tokio::test]
    async fn transport_failures_map_to_stable_categories() {
        let mut route = sample_route(None);
        route.auth_kind = RuntimeAuthKind::None;
        let cases: &[(TransportError, RuntimeError)] = &[
            (
                TransportError::Connect("connection refused".into()),
                RuntimeError::ProviderUnavailable(
                    "upstream unavailable: connection refused".into(),
                ),
            ),
            (
                TransportError::Timeout("took too long".into()),
                RuntimeError::ProviderUnavailable("upstream unavailable: took too long".into()),
            ),
            (
                TransportError::ResponseRead("connection dropped".into()),
                RuntimeError::ProviderUnavailable(
                    "upstream unavailable: connection dropped".into(),
                ),
            ),
            (
                TransportError::Other("mystery".into()),
                RuntimeError::ProviderUnavailable("upstream unavailable: mystery".into()),
            ),
            (
                TransportError::InvalidRequest("bad url".into()),
                RuntimeError::Internal("upstream request construction failed: bad url".into()),
            ),
        ];
        for (transport_error, expected) in cases {
            let runtime = ProxyRuntime::new(
                Arc::new(FixedCatalog {
                    routes: vec![route.clone()],
                }),
                Arc::new(MemoryCredentialProvider::new()),
                Arc::new(UnconfiguredOfficialAuthProvider),
                Arc::new(CountingRequestLifecycle::new()),
                Arc::new(FailingTransport(transport_error.clone())),
            );
            let error = runtime
                .execute(
                    request(json!({"model": "vlm-test", "input": "hi"})),
                    "test_exec",
                )
                .await
                .unwrap_err();
            assert_eq!(
                &error, expected,
                "transport error {transport_error:?} must map to {expected:?}"
            );
        }
    }

    #[tokio::test]
    async fn chat_transport_failure_records_shape_diagnostics_before_status() {
        let mut route = sample_route(None);
        route.auth_kind = RuntimeAuthKind::None;
        route.wire = RuntimeWireFormat::Chat;
        let events = Arc::new(RecordingStore::new());
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            Arc::new(FailingTransport(TransportError::Connect(
                "connection refused".into(),
            ))),
        )
        .with_diagnostics_sink(Arc::clone(&events) as Arc<dyn DiagnosticsSink>);

        let error = runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "input": [{"role": "user", "content": "CHAT_SECRET_TEXT"}]
                })),
                "chat_transport_error",
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::ProviderUnavailable(_)));

        let transcripts: Vec<_> = events
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                DiagnosticEvent::HarnessTranscript(item) => Some(item.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(transcripts.len(), 1, "{transcripts:?}");
        assert_eq!(transcripts[0].upstream_status, None);
        assert_eq!(transcripts[0].model.as_deref(), Some("test-model"));
        assert!(transcripts[0].body_bytes.unwrap_or_default() > 0);
        assert!(transcripts[0].role_sequence.contains("user"));
        let encoded = serde_json::to_string(&transcripts[0]).unwrap();
        assert!(!encoded.contains("CHAT_SECRET_TEXT"), "{encoded}");
    }

    #[tokio::test]
    async fn streaming_chat_transport_failure_records_shape_diagnostics_before_status() {
        let mut route = sample_route(None);
        route.auth_kind = RuntimeAuthKind::None;
        route.wire = RuntimeWireFormat::Chat;
        let events = Arc::new(RecordingStore::new());
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            Arc::new(FailingTransport(TransportError::Timeout(
                "open timed out".into(),
            ))),
        )
        .with_diagnostics_sink(Arc::clone(&events) as Arc<dyn DiagnosticsSink>);

        let error = runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "stream": true,
                    "input": [{"role": "user", "content": "STREAM_CHAT_SECRET_TEXT"}]
                })),
                "streaming_chat_transport_error",
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::ProviderUnavailable(_)));

        let transcripts: Vec<_> = events
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                DiagnosticEvent::HarnessTranscript(item) => Some(item.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(transcripts.len(), 1, "{transcripts:?}");
        assert_eq!(transcripts[0].upstream_status, None);
        assert_eq!(transcripts[0].model.as_deref(), Some("test-model"));
        assert!(transcripts[0].body_bytes.unwrap_or_default() > 0);
        assert!(transcripts[0].role_sequence.contains("user"));
        let encoded = serde_json::to_string(&transcripts[0]).unwrap();
        assert!(!encoded.contains("STREAM_CHAT_SECRET_TEXT"), "{encoded}");
    }

    /// The two-turn script the M4 continuation fixtures freeze (resp_t1 then
    /// resp_t2), as raw transport responses.
    fn two_turn_response_script() -> Vec<UpstreamResponse> {
        let turn = |response_id: &str, message_id: &str, text: &str| UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&json!({
                "id": response_id,
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": message_id,
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": text, "annotations": []}]
                }],
                "usage": {"input_tokens": 6, "output_tokens": 2, "total_tokens": 8}
            }))
            .unwrap(),
        };
        vec![
            turn("resp_t1", "msg_t1", "First reply"),
            turn("resp_t2", "msg_t2", "Second reply"),
        ]
    }

    fn parsed_request_body(transport: &RecordingTransport, index: usize) -> Value {
        let body = &transport.requests.lock().unwrap()[index].body;
        serde_json::from_slice(body).expect("recorded upstream body must be JSON")
    }

    struct FixedNewsEngine;

    #[async_trait::async_trait]
    impl SearchEngine for FixedNewsEngine {
        async fn run(
            &self,
            _request: &crate::search::SearchRequest,
        ) -> Result<crate::search::SearchResponse, String> {
            Ok(crate::search::SearchResponse {
                encrypted_output: None,
                output: "ok".into(),
                results: vec![crate::search::SearchResult {
                    kind: "page".into(),
                    ref_id: "turn1search0".into(),
                    url: Some("https://example.test/news".into()),
                    title: Some("News".into()),
                    snippet: Some("hello world".into()),
                    image_url: None,
                    width: None,
                    height: None,
                    pageno: None,
                }],
            })
        }
    }

    /// A `web_search` function-call turn's SSE, shaped as a real third-party
    /// Responses stream (not the already-rewritten form `search_loop`'s own
    /// unit tests use) so this exercises the normalizer + search-loop wiring
    /// together the way `exec.rs` actually sees it.
    fn web_search_function_call_sse(
        resp_id: &str,
        call_id: &str,
        item_id: &str,
        query: &str,
    ) -> String {
        let mut out = String::new();
        out.push_str(&format_sse_value(
            "response.created",
            json!({"type":"response.created","response":{"id":resp_id,"object":"response","status":"in_progress"}}),
        ));
        out.push_str(&format_sse_value(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type":"function_call","id":item_id,"call_id":call_id,"name":"web_search","arguments":"","status":"in_progress"}
            }),
        ));
        out.push_str(&format_sse_value(
            "response.function_call_arguments.delta",
            json!({"type":"response.function_call_arguments.delta","item_id":item_id,"output_index":0,"delta":query}),
        ));
        out.push_str(&format_sse_value(
            "response.function_call_arguments.done",
            json!({"type":"response.function_call_arguments.done","item_id":item_id,"output_index":0,"arguments":query}),
        ));
        out.push_str(&format_sse_value(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type":"function_call","id":item_id,"call_id":call_id,"name":"web_search","arguments":query,"status":"completed"}
            }),
        ));
        out.push_str(&format_sse_value(
            "response.completed",
            json!({
                "type": "response.completed",
                "response": {
                    "id": resp_id,
                    "object": "response",
                    "status": "completed",
                    "output": [{"type":"function_call","id":item_id,"call_id":call_id,"name":"web_search","arguments":query,"status":"completed"}],
                    "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}
                }
            }),
        ));
        out
    }

    fn final_answer_sse(resp_id: &str, msg_id: &str, text: &str) -> String {
        let mut out = String::new();
        out.push_str(&format_sse_value(
            "response.created",
            json!({"type":"response.created","response":{"id":resp_id,"object":"response","status":"in_progress"}}),
        ));
        out.push_str(&format_sse_value(
            "response.output_item.added",
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":msg_id,"role":"assistant","status":"in_progress","content":[]}}),
        ));
        out.push_str(&format_sse_value(
            "response.output_text.delta",
            json!({"type":"response.output_text.delta","item_id":msg_id,"output_index":0,"content_index":0,"delta":text}),
        ));
        out.push_str(&format_sse_value(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"type":"message","id":msg_id,"role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]}
            }),
        ));
        out.push_str(&format_sse_value(
            "response.completed",
            json!({
                "type": "response.completed",
                "response": {
                    "id": resp_id,
                    "object": "response",
                    "status": "completed",
                    "output": [{"type":"message","id":msg_id,"role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]}],
                    "usage": {"input_tokens": 20, "output_tokens": 8, "total_tokens": 28}
                }
            }),
        ));
        out
    }

    fn chat_text_completion_sse(text: &str) -> String {
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices": [{"delta": {"content": text}, "finish_reason": null}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})
        )
    }

    fn chat_text_completion_sse_without_done(text: &str) -> String {
        format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"choices": [{"delta": {"content": text}, "finish_reason": null}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})
        )
    }

    fn chat_tool_completion_sse() -> String {
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_find_node",
                            "type": "function",
                            "function": {
                                "name": "shell",
                                "arguments": "{\"cmd\":\"find node and pnpm\"}"
                            }
                        }]
                    },
                    "finish_reason": null
                }]
            }),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]})
        )
    }

    fn auto_continue_chat_runtime(transport: Arc<RecordingTransport>) -> ProxyRuntime {
        let mut route = sample_route(None);
        route.auth_kind = RuntimeAuthKind::None;
        route.wire = RuntimeWireFormat::Chat;
        route.upstream_model = "omen-alpha".into();
        ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport,
        )
    }

    fn captured_tool_result_request() -> RuntimeRequest {
        request(json!({
            "model": "vlm-test",
            "stream": true,
            "tools": [{
                "type": "function",
                "name": "shell",
                "description": "Run a shell command",
                "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}
            }],
            "input": [
                {
                    "type": "function_call",
                    "id": "fc_previous",
                    "call_id": "call_previous",
                    "name": "shell",
                    "arguments": "{\"cmd\":\"ssh mac command -v pnpm\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_previous",
                    "output": "ssh: connect to host 192.0.2.20 port 22: Permission denied"
                }
            ]
        }))
    }

    #[tokio::test]
    async fn captured_omen_progress_completion_retries_into_the_promised_tool_call() {
        let announcement = "Continuing: 偵測遠端 Mac 上的 node/pnpm 安裝位置。";
        let transport = Arc::new(RecordingTransport::new(vec![
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                // The captured provider family can close after finish_reason
                // without sending `[DONE]`; this exercises that terminal path.
                body: chat_text_completion_sse_without_done(announcement).into_bytes(),
            },
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: chat_tool_completion_sse().into_bytes(),
            },
        ]));
        let runtime = auto_continue_chat_runtime(transport.clone());
        let response = runtime
            .execute(
                captured_tool_result_request(),
                "auto-continue-captured-omen",
            )
            .await
            .unwrap();
        let RuntimeResponse::Sse(mut stream) = response else {
            panic!("streaming Chat request must return SSE");
        };
        let mut transcript = String::new();
        while let Some(chunk) = stream.next().await {
            transcript.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }

        assert_eq!(transport.requests.lock().unwrap().len(), 2);
        assert!(!transcript.contains(announcement));
        assert!(transcript.contains("call_find_node"), "{transcript}");
        assert!(transcript.contains("\"name\":\"shell\""), "{transcript}");
        assert_eq!(
            transcript.matches("event: response.completed").count(),
            1,
            "only the retry may complete the client response: {transcript}"
        );

        let second = parsed_request_body(&transport, 1).to_string();
        assert!(second.contains(announcement), "{second}");
        assert!(
            second.contains("only announced the next action"),
            "{second}"
        );
    }

    #[tokio::test]
    async fn repeated_progress_completion_is_bounded_to_one_retry() {
        let announcement = "Continuing: 偵測遠端 Mac 上的 node/pnpm 安裝位置。";
        let response = UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: chat_text_completion_sse(announcement).into_bytes(),
        };
        let transport = Arc::new(RecordingTransport::new(vec![response.clone(), response]));
        let runtime = auto_continue_chat_runtime(transport.clone());
        let response = runtime
            .execute(captured_tool_result_request(), "auto-continue-bounded")
            .await
            .unwrap();
        let RuntimeResponse::Sse(mut stream) = response else {
            panic!("streaming Chat request must return SSE");
        };
        let mut transcript = String::new();
        while let Some(chunk) = stream.next().await {
            transcript.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }

        assert_eq!(transport.requests.lock().unwrap().len(), 2);
        assert!(transcript.contains(announcement), "{transcript}");
        assert_eq!(transcript.matches("event: response.completed").count(), 1);
    }

    #[tokio::test]
    async fn empty_tool_result_continuation_retries_once_then_keeps_protocol_failure() {
        let transport = Arc::new(RecordingTransport::new(vec![
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: Vec::new(),
            },
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: Vec::new(),
            },
        ]));
        let runtime = auto_continue_chat_runtime(transport.clone());
        let response = runtime
            .execute(captured_tool_result_request(), "auto-continue-empty")
            .await
            .unwrap();
        let RuntimeResponse::Sse(mut stream) = response else {
            panic!("streaming Chat request must return SSE");
        };
        let mut transcript = String::new();
        while let Some(chunk) = stream.next().await {
            transcript.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }

        assert_eq!(transport.requests.lock().unwrap().len(), 2);
        assert_eq!(transcript.matches("event: response.failed").count(), 1);
        assert!(transcript.contains("provider_protocol"), "{transcript}");
        assert!(
            transcript.contains("HTTP 200 with an empty body"),
            "{transcript}"
        );
        let retry = parsed_request_body(&transport, 1).to_string();
        assert!(retry.contains("empty successful stream"), "{retry}");
    }

    #[tokio::test]
    async fn empty_tool_result_continuation_recovers_on_the_same_model() {
        let transport = Arc::new(RecordingTransport::new(vec![
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: Vec::new(),
            },
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: chat_tool_completion_sse().into_bytes(),
            },
        ]));
        let runtime = auto_continue_chat_runtime(transport.clone());
        let response = runtime
            .execute(
                captured_tool_result_request(),
                "auto-continue-empty-recovered",
            )
            .await
            .unwrap();
        let RuntimeResponse::Sse(mut stream) = response else {
            panic!("streaming Chat request must return SSE");
        };
        let mut transcript = String::new();
        while let Some(chunk) = stream.next().await {
            transcript.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }

        assert_eq!(transport.requests.lock().unwrap().len(), 2);
        assert_eq!(transcript.matches("event: response.completed").count(), 1);
        assert!(
            !transcript.contains("event: response.failed"),
            "{transcript}"
        );
        assert!(transcript.contains("call_find_node"), "{transcript}");
        let first = parsed_request_body(&transport, 0);
        let retry = parsed_request_body(&transport, 1);
        assert_eq!(first["model"], retry["model"]);
    }

    fn web_search_runtime(
        transport: Arc<RecordingTransport>,
        engine: Arc<dyn SearchEngine>,
        loop_limit: u8,
    ) -> ProxyRuntime {
        let mut route = sample_route(None);
        route.auth_kind = RuntimeAuthKind::None;
        ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport,
        )
        .with_search_engine(engine)
        .with_web_search_wrapper_enabled(true)
        .with_search_tool_loop_limit(loop_limit)
    }

    /// End-to-end coverage for the gap the branch shipped without: a real
    /// `ProxyRuntime::execute` dispatch (not just `SearchLoopSession` in
    /// isolation) against a mock upstream that emits a `web_search` function
    /// call. Proves the client never sees the raw call, the continuation
    /// request carries the right `function_call`/`function_call_output`
    /// pair, and exactly one usage row is written across both hops.
    #[tokio::test]
    async fn third_party_web_search_loop_hides_function_call_and_records_one_usage_row() {
        let hop1 =
            web_search_function_call_sse("resp_1", "call_1", "fc_1", "{\"query\":\"today news\"}");
        let hop2 = final_answer_sse("resp_2", "msg_1", "Here is the news.");
        let transport = Arc::new(RecordingTransport::new(vec![
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: hop1.into_bytes(),
            },
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: hop2.into_bytes(),
            },
        ]));
        let runtime = web_search_runtime(
            transport.clone(),
            Arc::new(FixedNewsEngine),
            DEFAULT_SEARCH_TOOL_LOOP_LIMIT,
        );

        let response = runtime
            .execute(
                request(json!({"model": "vlm-test", "input": [{"type": "message", "role": "user", "content": "what's the news today"}], "stream": true})),
                "search-loop-1",
            )
            .await
            .unwrap();
        let RuntimeResponse::Sse(mut stream) = response else {
            panic!("stream: true must produce RuntimeResponse::Sse");
        };
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            text.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }

        assert!(
            !text.contains("\"type\":\"function_call\""),
            "the client must never see the raw web_search function_call: {text}"
        );
        assert!(
            text.contains("web_search_call"),
            "client must see the web_search_call lifecycle: {text}"
        );
        assert!(
            text.contains("Here is the news."),
            "final answer must reach the client: {text}"
        );
        assert_eq!(
            text.matches("\"type\":\"response.completed\"").count(),
            1,
            "only the final hop's response.completed may reach the client: {text}"
        );

        assert_eq!(
            transport.calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "exactly one search hop + one continuation call"
        );
        let continuation = parsed_request_body(&transport, 1);
        let input = continuation["input"]
            .as_array()
            .expect("continuation input array");
        assert!(
            input.iter().any(|item| item["type"] == "function_call"
                && item["name"] == "web_search"
                && item["call_id"] == "call_1"),
            "continuation must replay the original web_search function_call to the provider: {continuation}"
        );
        assert!(
            input
                .iter()
                .any(|item| item["type"] == "function_call_output"
                    && item["call_id"] == "call_1"
                    && item["output"]
                        .as_str()
                        .unwrap_or("")
                        .contains("example.test/news")),
            "continuation must carry the bounded Brave output back to the provider: {continuation}"
        );

        let records = runtime.usage_records().unwrap();
        assert_eq!(
            records.len(),
            1,
            "one client turn must record exactly one usage row across N search hops: {records:?}"
        );
        assert_eq!(records[0].status, 200);
        assert!(
            records[0]
                .conversation_identity
                .as_deref()
                .is_some_and(|identity| !identity.is_empty()),
            "streaming usage must retain the durable conversation owner: {records:?}"
        );
    }

    /// A `web_search` hop mid-conversation must resend the hydrated
    /// continuation, not the incremental client body. Codex sends only the
    /// new items plus `previous_response_id`; the transcript exists solely
    /// because Vellum hydrates it. Building the second hop from the client
    /// body instead therefore drops every earlier turn, and the model answers
    /// the search as if the conversation had just started (observed live: a
    /// 106k-token turn collapsing to 16k on the hop after `web_search`).
    #[tokio::test]
    async fn web_search_continuation_keeps_the_hydrated_history_not_the_client_delta() {
        let history = Arc::new(MemoryHistoryStore::new());
        history
            .record_exchange(
                &json!({
                    "model": "vlm-test",
                    "input": [{
                        "type": "message",
                        "role": "user",
                        "content": "remember the passphrase orbital-marmalade"
                    }]
                }),
                &json!({
                    "id": "resp_turn_1",
                    "output": [{
                        "type": "message",
                        "role": "assistant",
                        "content": "noted, orbital-marmalade"
                    }]
                }),
                "route-1",
            )
            .unwrap();

        let hop1 =
            web_search_function_call_sse("resp_2", "call_1", "fc_1", "{\"query\":\"today news\"}");
        let hop2 = final_answer_sse("resp_3", "msg_1", "Here is the news.");
        let transport = Arc::new(RecordingTransport::new(vec![
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: hop1.into_bytes(),
            },
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: hop2.into_bytes(),
            },
        ]));
        let runtime = web_search_runtime(
            transport.clone(),
            Arc::new(FixedNewsEngine),
            DEFAULT_SEARCH_TOOL_LOOP_LIMIT,
        )
        .with_history_store(history);

        let response = runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "previous_response_id": "resp_turn_1",
                    "input": [{
                        "type": "message",
                        "role": "user",
                        "content": "search for what I asked about"
                    }],
                    "stream": true
                })),
                "search-loop-hydrated",
            )
            .await
            .unwrap();
        let RuntimeResponse::Sse(mut stream) = response else {
            panic!("stream: true must produce RuntimeResponse::Sse");
        };
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            text.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }
        assert!(
            text.contains("Here is the news."),
            "final answer must reach the client: {text}"
        );

        let first_hop = parsed_request_body(&transport, 0);
        let continuation = parsed_request_body(&transport, 1);
        let first_input = first_hop["input"].as_array().expect("first hop input");
        let continuation_input = continuation["input"]
            .as_array()
            .expect("continuation input array");

        assert!(
            continuation.to_string().contains("orbital-marmalade"),
            "the search continuation must still carry the hydrated earlier turn: {continuation}"
        );
        // Two items appended (the replayed function_call and its output) and
        // nothing dropped: the hop is the first hop's body plus the search
        // result, never a rebuild from the incremental client request.
        assert_eq!(
            continuation_input.len(),
            first_input.len() + 2,
            "continuation must extend the dispatched history, not replace it:              first={first_hop} continuation={continuation}"
        );

        // The recorded chain keeps the search pair too, so the *next* turn
        // hydrates a transcript that includes what the search returned.
        let chain = runtime.history.get_chain("resp_3").unwrap();
        let recorded = chain
            .last()
            .expect("the completed exchange must be recorded");
        assert!(
            recorded
                .input_items
                .iter()
                .any(|item| item["type"] == "function_call_output" && item["call_id"] == "call_1"),
            "the recorded exchange must retain the web_search output: {:?}",
            recorded.input_items
        );
    }

    /// The other half of the gap: a model that keeps calling `web_search`
    /// past the configured cap must get an explicit `tool_loop_limit` error,
    /// not the generic "stream ended without response.completed" message.
    #[tokio::test]
    async fn third_party_web_search_loop_stops_at_the_configured_cap_with_tool_loop_limit() {
        let always_search =
            web_search_function_call_sse("resp_loop", "call_x", "fc_x", "{\"query\":\"more\"}");
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: always_search.into_bytes(),
        }]));
        let runtime = web_search_runtime(transport.clone(), Arc::new(FixedNewsEngine), 1);

        let response = runtime
            .execute(
                request(json!({"model": "vlm-test", "input": [{"type": "message", "role": "user", "content": "keep searching"}], "stream": true})),
                "search-loop-cap",
            )
            .await
            .unwrap();
        let RuntimeResponse::Sse(mut stream) = response else {
            panic!("stream: true must produce RuntimeResponse::Sse");
        };
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            text.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }

        assert!(
            text.contains("\"category\":\"tool_loop_limit\""),
            "hitting the per-turn search cap must surface an explicit tool_loop_limit error: {text}"
        );
        assert!(
            !text.contains("Upstream stream ended without response.completed"),
            "the cap must not degrade into the generic truncated-stream message: {text}"
        );

        let records = runtime.usage_records().unwrap();
        assert_eq!(
            records.len(),
            1,
            "the capped-out turn must still record exactly one usage row: {records:?}"
        );
    }

    struct FailingSearchEngine;

    #[async_trait::async_trait]
    impl SearchEngine for FailingSearchEngine {
        async fn run(
            &self,
            _request: &crate::search::SearchRequest,
        ) -> Result<crate::search::SearchResponse, String> {
            Err("bounded Brave failure".into())
        }
    }

    #[tokio::test]
    async fn third_party_web_search_failure_records_one_terminal_usage_row() {
        let hop1 = web_search_function_call_sse("resp_1", "call_1", "fc_1", "{\"query\":\"news\"}");
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: hop1.into_bytes(),
        }]));
        let runtime = web_search_runtime(
            transport,
            Arc::new(FailingSearchEngine),
            DEFAULT_SEARCH_TOOL_LOOP_LIMIT,
        );

        let response = runtime
            .execute(
                request(json!({"model": "vlm-test", "input": "news", "stream": true})),
                "search-failure-1",
            )
            .await
            .unwrap();
        let RuntimeResponse::Sse(mut stream) = response else {
            panic!("stream: true must produce RuntimeResponse::Sse");
        };
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            text.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }

        assert!(text.contains("web_search execution failed"), "{text}");
        let records = runtime.usage_records().unwrap();
        assert_eq!(
            records.len(),
            1,
            "a failed Brave call must write exactly one terminal usage row: {records:?}"
        );
        assert_eq!(records[0].outcome.as_deref(), Some("provider_failure"));
        assert_eq!(records[0].status, 502);
        assert_eq!(records[0].error_category.as_deref(), Some("search"));
    }

    struct SignalingSearchEngine {
        started: Arc<tokio::sync::Notify>,
        completed: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl SearchEngine for SignalingSearchEngine {
        async fn run(
            &self,
            _request: &crate::search::SearchRequest,
        ) -> Result<crate::search::SearchResponse, String> {
            self.started.notify_one();
            std::future::pending::<()>().await;
            self.completed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::search::SearchResponse {
                encrypted_output: None,
                output: "unreachable".into(),
                results: Vec::new(),
            })
        }
    }

    /// Proves the multi-hop search loop reuses the existing cancellation
    /// tree correctly: dropping the wrapped stream on cancel must drop the
    /// whole generator, including a Brave call that is still in flight, and
    /// must never reach a second upstream call or leave a stray usage row.
    #[tokio::test]
    async fn client_cancel_mid_search_drops_the_in_flight_brave_call() {
        let hop1 =
            web_search_function_call_sse("resp_1", "call_1", "fc_1", "{\"query\":\"today news\"}");
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: hop1.into_bytes(),
        }]));
        let started = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let engine = Arc::new(SignalingSearchEngine {
            started: started.clone(),
            completed: completed.clone(),
        });
        let runtime = Arc::new(web_search_runtime(
            transport.clone(),
            engine,
            DEFAULT_SEARCH_TOOL_LOOP_LIMIT,
        ));

        let exec_runtime = runtime.clone();
        let handle = tokio::spawn(async move {
            let response = exec_runtime
                .execute(
                    request(json!({"model": "vlm-test", "input": [{"type": "message", "role": "user", "content": "news"}], "stream": true})),
                    "cancel-search-1",
                )
                .await
                .unwrap();
            let RuntimeResponse::Sse(mut stream) = response else {
                panic!("stream: true must produce RuntimeResponse::Sse");
            };
            let mut collected = Vec::new();
            while let Some(chunk) = stream.next().await {
                collected.extend_from_slice(&chunk.unwrap());
            }
            collected
        });

        started.notified().await;
        runtime.cancel_request_tree("cancel-search-1");

        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("the stream must terminate promptly once cancelled mid-search")
            .expect("spawned task must not panic");

        assert!(
            !completed.load(std::sync::atomic::Ordering::SeqCst),
            "cancelling mid-search must drop the in-flight Brave call, not let it run to completion"
        );
        assert_eq!(
            transport.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "cancellation during the search must never reach the continuation upstream call"
        );
        assert!(
            runtime.usage_records().unwrap().len() <= 1,
            "the runtime stream must never race its HTTP/WebSocket boundary into an extra usage row"
        );
    }

    fn no_auth_runtime_with_history(
        transport: Arc<RecordingTransport>,
        history_path: &std::path::Path,
    ) -> ProxyRuntime {
        let mut route = sample_route(None);
        route.auth_kind = RuntimeAuthKind::None;
        let history: Arc<dyn HistoryStore> =
            Arc::new(crate::history::FileHistoryStore::open(history_path).unwrap());
        ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport,
        )
        .with_history_store(history)
    }

    #[tokio::test]
    async fn third_party_continuation_hydrates_chain_and_strips_previous_response_id() {
        // One runtime instance with the default in-memory history: turn 2
        // must replay the recorded turn-1 chain into `input` and strip
        // `previous_response_id`, exactly the PortableSemanticReplay
        // contract the parity fixtures freeze.
        let transport = Arc::new(RecordingTransport::new(two_turn_response_script()));
        let runtime = no_auth_runtime(transport.clone());
        runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "input": [{"role": "user", "content": "first turn"}]
                })),
                "test_exec",
            )
            .await
            .unwrap();
        runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "previous_response_id": "resp_t1",
                    "input": [{"role": "user", "content": "second turn"}]
                })),
                "test_exec",
            )
            .await
            .unwrap();

        let turn_2_upstream = parsed_request_body(&transport, 1);
        assert!(
            turn_2_upstream.get("previous_response_id").is_none(),
            "third-party continuation must strip previous_response_id: {turn_2_upstream}"
        );
        assert_eq!(turn_2_upstream["model"], "test-model");
        assert_eq!(
            turn_2_upstream["input"],
            json!([
                {"role": "user", "content": "first turn"},
                {
                    "type": "message",
                    "id": "msg_t1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "First reply", "annotations": []}],
                    "phase": "final_answer"
                },
                {"role": "user", "content": "second turn"}
            ]),
            "PortableSemanticReplay must replay the exact chain shape: {turn_2_upstream}"
        );
    }

    #[tokio::test]
    async fn official_to_deepseek_switch_keeps_portable_continuation_connected() {
        let mut official = sample_route(None);
        official.route_id = "official".into();
        official.catalog_id = "gpt-official".into();
        official.provider_kind = RuntimeProviderKind::Official;
        official.auth_kind = RuntimeAuthKind::None;
        official.upstream_model = "gpt-5.6-sol".into();
        let mut deepseek = sample_route(None);
        deepseek.route_id = "deepseek".into();
        deepseek.catalog_id = "deepseek-coder".into();
        deepseek.auth_kind = RuntimeAuthKind::None;
        deepseek.upstream_model = "deepseek-v4".into();
        let first_response = json!({
            "id": "resp_gpt_turn",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "reasoning",
                "id": "rs_gpt",
                "encrypted_content": "official-only-ciphertext",
                "summary": [{"type": "summary_text", "text": "Checked the code."}]
            }, {
                "type": "message",
                "id": "msg_gpt",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "First reply", "annotations": []}]
            }],
            "usage": {"input_tokens": 6, "output_tokens": 2, "total_tokens": 8}
        });
        let transport = Arc::new(RecordingTransport::new(vec![
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: serde_json::to_vec(&first_response).unwrap(),
            },
            two_turn_response_script().remove(1),
        ]));
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![official, deepseek],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );

        runtime
            .execute(
                request(json!({
                    "model": "gpt-official",
                    "input": [{"role": "user", "content": "first turn"}]
                })),
                "test_exec",
            )
            .await
            .unwrap();
        runtime
            .execute(
                request(json!({
                    "model": "deepseek-coder",
                    "previous_response_id": "resp_gpt_turn",
                    "input": [{"role": "user", "content": "second turn"}]
                })),
                "test_exec",
            )
            .await
            .unwrap();

        let switched = parsed_request_body(&transport, 1);
        assert!(switched.get("previous_response_id").is_none());
        assert_eq!(switched["model"], "deepseek-v4");
        let encoded = serde_json::to_string(&switched["input"]).unwrap();
        assert!(encoded.contains("first turn"));
        assert!(encoded.contains("First reply"));
        assert!(encoded.contains("second turn"));
        assert!(!encoded.contains("official-only-ciphertext"));
    }

    #[tokio::test]
    async fn continuation_without_local_history_fails_closed() {
        // Fresh runtime (empty history) + previous_response_id on a
        // third-party route: PortableSemanticReplay has nothing to replay, so
        // the request must error instead of silently dropping continuity.
        let transport = Arc::new(RecordingTransport::new(vec![scripted_ok_response()]));
        let runtime = no_auth_runtime(transport.clone());
        let error = runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "previous_response_id": "resp_ghost",
                    "input": [{"role": "user", "content": "second turn"}]
                })),
                "test_exec",
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, RuntimeError::Continuation(message) if message.contains("no local history")),
            "expected fail-closed Continuation, got {error:?}"
        );
        assert_eq!(
            transport.requests.lock().unwrap().len(),
            0,
            "a failed continuation must never reach the upstream"
        );
    }

    #[tokio::test]
    async fn continuation_history_survives_runtime_restart() {
        // Turn 1 through runtime #1 with a durable file store; drop the
        // runtime; turn 2 through a brand-new runtime #2 over the same file
        // store must hydrate the same chain (the M4 restart-continuation
        // acceptance).
        let temp = tempfile::tempdir().unwrap();
        let history_path = temp.path().join("history.jsonl");

        let turn_1_transport = Arc::new(RecordingTransport::new(two_turn_response_script()));
        let runtime_one = no_auth_runtime_with_history(turn_1_transport, &history_path);
        runtime_one
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "input": [{"role": "user", "content": "first turn"}]
                })),
                "test_exec",
            )
            .await
            .unwrap();
        drop(runtime_one);

        let turn_2_transport = Arc::new(RecordingTransport::new(two_turn_response_script()));
        let runtime_two = no_auth_runtime_with_history(turn_2_transport.clone(), &history_path);
        runtime_two
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "previous_response_id": "resp_t1",
                    "input": [{"role": "user", "content": "second turn"}]
                })),
                "test_exec",
            )
            .await
            .unwrap();

        let turn_2_upstream = parsed_request_body(&turn_2_transport, 0);
        assert!(turn_2_upstream.get("previous_response_id").is_none());
        assert_eq!(
            turn_2_upstream["input"],
            json!([
                {"role": "user", "content": "first turn"},
                {
                    "type": "message",
                    "id": "msg_t1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "First reply", "annotations": []}],
                    "phase": "final_answer"
                },
                {"role": "user", "content": "second turn"}
            ]),
            "a restarted runtime must hydrate the durable chain: {turn_2_upstream}"
        );
    }

    fn compaction_conversation_turn(n: u32) -> UpstreamResponse {
        let text = format!(
            "Reply number {n}. This reply carries a little more prose so the \
             scripted transcript has real token weight for the shrink gate."
        );
        UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&json!({
                "id": format!("resp_ct_{n}"),
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": format!("msg_ct_{n}"),
                    "role": "assistant",
                    "status": "completed",
                    "content": [{
                        "type": "output_text",
                        "text": text,
                        "annotations": []
                    }]
                }],
                "usage": {
                    "input_tokens": 10 + n,
                    "output_tokens": 4 + n,
                    "total_tokens": 14 + 2 * n
                }
            }))
            .unwrap(),
        }
    }

    fn compaction_summary_turn() -> UpstreamResponse {
        let summary_text = json!({
            "goal": "Answer the conversation turns.",
            "done": [
                "Turn 1 answered",
                "Turn 2 answered",
                "Turn 3 answered",
                "Turn 4 answered"
            ],
            "in_progress": ["Next turn pending"],
            "decisions": ["Keep replies concise"],
            "next_steps": ["Answer the next turn"],
            "constraints": [],
            "user_preferences": []
        })
        .to_string();
        UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&json!({
                "id": "resp_sum_1",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": "msg_sum_1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": summary_text, "annotations": []}]
                }],
                "usage": {"input_tokens": 40, "output_tokens": 30, "total_tokens": 70}
            }))
            .unwrap(),
        }
    }

    #[tokio::test]
    async fn restart_after_compaction_chains_through_the_durable_journal() {
        // M6 acceptance "checkpoint survives restart": runtime #1 compacts a
        // 30-turn conversation (journaling the canonical window durably),
        // records two more turns, then a brand-new runtime #2 over the same
        // file must rewrite its compact input from the checkpoint and chain a
        // second compaction (generation 2) through the durable journal.
        let temp = tempfile::tempdir().unwrap();
        let history_path = temp.path().join("history.jsonl");
        let summary_turn = compaction_summary_turn();

        let mut script_one = Vec::new();
        for n in 1..=30 {
            script_one.push(compaction_conversation_turn(n));
        }
        script_one.push(summary_turn.clone());
        script_one.push(compaction_conversation_turn(31));
        script_one.push(compaction_conversation_turn(32));
        let runtime_one = no_auth_runtime_with_history(
            Arc::new(RecordingTransport::new(script_one)),
            &history_path,
        );

        for n in 1..=30 {
            let mut body = json!({
                "model": "vlm-test",
                "input": [{"role": "user", "content": format!("turn {n}")}]
            });
            if n > 1 {
                body["previous_response_id"] = json!(format!("resp_ct_{}", n - 1));
            }
            runtime_one
                .execute(request(body), "test_exec")
                .await
                .unwrap();
        }

        let mut compact_input = Vec::new();
        for n in 1..=30 {
            compact_input.push(json!({"role": "user", "content": format!("turn {n}")}));
            let text = format!(
                "Reply number {n}. This reply carries a little more prose so the \
                 scripted transcript has real token weight for the shrink gate."
            );
            compact_input.push(json!({
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": text,
                    "annotations": []
                }]
            }));
        }
        compact_input.push(json!({"role": "user", "content": "turn 31"}));
        let compact_one = runtime_one
            .execute_compact(request(json!({
                "model": "vlm-test",
                "previous_response_id": "resp_ct_30",
                "input": compact_input
            })))
            .await
            .unwrap();
        let RuntimeResponse::Json(compact_one_body) = compact_one else {
            panic!("compact must return JSON");
        };
        assert_eq!(compact_one_body["output"][0]["type"], "compaction");

        for n in 31..=32 {
            runtime_one
                .execute(
                    request(json!({
                        "model": "vlm-test",
                        "previous_response_id": format!("resp_ct_{}", n - 1),
                        "input": [{"role": "user", "content": format!("turn {n}")}]
                    })),
                    "test_exec",
                )
                .await
                .unwrap();
        }
        drop(runtime_one);

        // Runtime #2 over the same durable store: the compact input is
        // rewritten from the journaled checkpoint + post-compact exchanges.
        let runtime_two = no_auth_runtime_with_history(
            Arc::new(RecordingTransport::new(vec![summary_turn.clone()])),
            &history_path,
        );
        let compact_two = runtime_two
            .execute_compact(request(json!({
                "model": "vlm-test",
                "previous_response_id": "resp_ct_32",
                "input": [{"role": "user", "content": "unused"}]
            })))
            .await
            .unwrap();
        let RuntimeResponse::Json(compact_two_body) = compact_two else {
            panic!("compact must return JSON");
        };
        assert_eq!(
            compact_two_body["output"][0]["type"], "compaction",
            "restart compact must succeed: {compact_two_body}"
        );
        drop(runtime_two);

        let store = crate::history::FileHistoryStore::open(&history_path).unwrap();
        let first_ids = store.compaction_ids_for_response("resp_ct_30").unwrap();
        assert_eq!(
            first_ids.len(),
            1,
            "the first compact journals to resp_ct_30"
        );
        let second_ids = store.compaction_ids_for_response("resp_ct_32").unwrap();
        assert_eq!(
            second_ids.len(),
            1,
            "the restart compact journals to resp_ct_32"
        );
        assert_eq!(
            store.compaction_generation(&second_ids[0]).unwrap(),
            Some(2),
            "the restart compaction must chain generation 1 -> 2"
        );
        // A third compact must find the newest journal through the walk.
        let parent = store.compacted_replay_parent("resp_ct_32").unwrap();
        assert_eq!(parent.as_deref(), second_ids.first().map(String::as_str));
    }

    #[tokio::test]
    async fn delegation_not_wired_advertises_single_agent() {
        // M10: without a verified delegation runtime the runtime fails closed
        // on delegating reasoning efforts instead of promising a
        // multi-agent contract it cannot honour.
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "id": "resp_1",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "hi", "annotations": []}]
                }],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }"#
            .to_vec(),
        }]));
        let runtime = no_auth_runtime(transport);
        let error = runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "input": "hi",
                    "reasoning": {"effort": "ultra"}
                })),
                "test_exec",
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("requires a verified delegation runtime"),
            "a non-delegated runtime must refuse `ultra`: {error}"
        );
    }

    #[tokio::test]
    async fn grok_oversized_turn_passes_through_unmodified_without_hidden_compaction() {
        // Codex Core is the sole auto-compact scheduler (see
        // docs/protocol-source-of-truth.md): a Grok Build route over its
        // configured threshold must dispatch the real turn's input verbatim,
        // with no proxy-side summary call and no rewrite. Vellum only ever
        // compacts in response to a genuine `compaction_trigger` Codex sends.
        let script = vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&json!({
                "id": "resp_grok_1",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": "msg_grok_1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "Grok reply", "annotations": []}]
                }],
                "usage": {"input_tokens": 8, "output_tokens": 4, "total_tokens": 12}
            }))
            .unwrap(),
        }];
        let mut route = sample_route(None);
        route.provider_kind = RuntimeProviderKind::GrokCli;
        route.auth_kind = RuntimeAuthKind::None;
        route.context_window = Some(1_000);
        route.compaction_policy = crate::config::RuntimeCompactionPolicy {
            threshold_percent: 60,
            output_reserve_tokens: 0,
            tool_reserve_tokens: 0,
            grok_threshold_percent: Some(50),
        };
        let transport = Arc::new(RecordingTransport::new(script));
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );
        let original_input = json!([
            {"role": "user", "content": "grok context ".repeat(160)},
            {"role": "assistant", "content": [{"type": "output_text", "text": "Earlier reply", "annotations": []}]},
            {"role": "user", "content": "current turn"}
        ]);
        runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "input": original_input.clone()
                })),
                "test_exec",
            )
            .await
            .unwrap();

        // Exactly one upstream call, and its input must be byte-for-byte the
        // turn Codex sent — no rewrite, no injected summary, no second
        // (auxiliary summary) call.
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
        let real_turn = parsed_request_body(&transport, 0);
        assert_eq!(
            real_turn["input"], original_input,
            "an ordinary oversized turn must reach upstream unmodified: {real_turn}"
        );
        assert!(
            !runtime
                .usage_records()
                .unwrap()
                .iter()
                .any(|record| record.review_reason.as_deref() == Some("compaction")),
            "no compaction usage row may be recorded for an ordinary turn"
        );
    }

    fn grok_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[tokio::test]
    async fn grok_auxiliary_compaction_reuses_session_and_omits_turn_index() {
        // The Codex Local Compact 0.150 summarizer is an auxiliary call even
        // on a Grok route, not a numbered turn: it must land on the
        // conversation's existing session (never a fresh one, never the old
        // `session=vellum/turn=0` placeholder), use a distinct
        // `xai-compact-*` request identity, and never carry
        // `x-grok-turn-idx`.
        let script = vec![
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: serde_json::to_vec(&json!({
                    "id": "resp_user_1",
                    "object": "response",
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "id": "msg_user_1",
                        "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": "hello", "annotations": []}]
                    }],
                    "usage": {"input_tokens": 2, "output_tokens": 1, "total_tokens": 3}
                }))
                .unwrap(),
            },
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: serde_json::to_vec(&json!({
                    "id": "resp_canonical_sum",
                    "object": "response",
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "id": "msg_canonical_sum",
                        "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": "Goal: continue. Decisions: none. Remaining: the current turn.", "annotations": []}]
                    }],
                    "usage": {"input_tokens": 4, "output_tokens": 4, "total_tokens": 8}
                }))
                .unwrap(),
            },
            // A second identical canonical-summary attempt (retry).
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: serde_json::to_vec(&json!({
                    "id": "resp_canonical_sum_retry",
                    "object": "response",
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "id": "msg_canonical_sum_retry",
                        "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": "Goal: continue. Decisions: none. Remaining: the current turn.", "annotations": []}]
                    }],
                    "usage": {"input_tokens": 4, "output_tokens": 4, "total_tokens": 8}
                }))
                .unwrap(),
            },
        ];
        let credentials = MemoryCredentialProvider::new();
        credentials.insert(
            "grok-cli",
            r#"{"accessToken":"tok","clientVersion":"1.0.3","userId":"user-1"}"#,
        );
        let mut route = sample_route(Some("grok-cli".into()));
        route.provider_kind = RuntimeProviderKind::GrokCli;
        route.auth_kind = RuntimeAuthKind::GrokSession;
        route.upstream_model = "grok-4.6".into();
        let transport = Arc::new(RecordingTransport::new(script));
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route.clone()],
            }),
            Arc::new(credentials),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );
        let user_request = request(json!({
            "model": "vlm-test",
            "prompt_cache_key": "thread-compact",
            "input": [{"role": "user", "content": "hello"}]
        }));
        runtime
            .execute(user_request.clone(), "test_exec")
            .await
            .unwrap();

        let first_summary = runtime
            .request_codex_local_v0_150_summary(
                &route,
                &user_request,
                &[json!({"type": "message", "role": "user", "content": "old context"})],
            )
            .await
            .expect("codex local compaction summary");
        // A repeat of the identical auxiliary call must reuse the same
        // request identity, not mint a new one.
        let retry_summary = runtime
            .request_codex_local_v0_150_summary(
                &route,
                &user_request,
                &[json!({"type": "message", "role": "user", "content": "old context"})],
            )
            .await
            .expect("retried codex local compaction summary");
        let _ = (first_summary, retry_summary);

        let recorded = transport.requests.lock().unwrap();
        assert_eq!(recorded.len(), 3);
        let user = &recorded[0].headers;
        let canonical = &recorded[1].headers;
        let canonical_retry = &recorded[2].headers;
        let user_session = grok_header(user, "x-grok-session-id").expect("user session");
        assert!(!user_session.is_empty());
        assert_ne!(user_session, "vellum");
        assert_eq!(grok_header(user, "x-grok-turn-idx"), Some("0"));
        for headers in [canonical, canonical_retry] {
            assert_eq!(
                grok_header(headers, "x-grok-session-id"),
                Some(user_session),
                "compaction must reuse the conversation's session, never invent a new one"
            );
            assert_eq!(grok_header(headers, "x-grok-conv-id"), Some(user_session));
            let req_id = grok_header(headers, "x-grok-req-id").expect("compaction request id");
            assert!(
                req_id.starts_with("xai-compact-"),
                "compaction must use a distinct request identity, got {req_id}"
            );
            assert!(
                grok_header(headers, "x-grok-turn-idx").is_none(),
                "auxiliary compaction must not send x-grok-turn-idx: {headers:?}"
            );
            assert_eq!(grok_header(headers, "x-grok-client-version"), Some("1.0.3"));
            assert_eq!(grok_header(headers, "x-grok-user-id"), Some("user-1"));
        }
        // The retry of the identical chunk reuses the exact same request id
        // as the first attempt.
        assert_eq!(
            grok_header(canonical, "x-grok-req-id"),
            grok_header(canonical_retry, "x-grok-req-id"),
            "a retry of the identical auxiliary call must reuse its request identity"
        );
        // The compaction usage rows are tagged as compaction, never counted
        // as an ordinary successful turn.
        let compaction_rows = runtime
            .usage_records()
            .unwrap()
            .into_iter()
            .filter(|record| record.review_reason.as_deref() == Some("compaction"))
            .count();
        assert_eq!(
            compaction_rows, 2,
            "both summarizer calls must be tagged as compaction"
        );
    }

    #[tokio::test]
    async fn compatible_responses_compaction_normalizes_private_custom_tool_history() {
        let credentials = MemoryCredentialProvider::new();
        credentials.insert("key-1", "test-token");
        let mut route = sample_route(Some("key-1".into()));
        route.provider_kind = RuntimeProviderKind::OpenAiCompatible;
        route.wire = RuntimeWireFormat::Responses;
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&json!({
                "id": "resp_summary",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Continue from the tool result."}]
                }],
                "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}
            }))
            .unwrap(),
        }]));
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route.clone()],
            }),
            Arc::new(credentials),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );
        let compact_request = request(json!({"model": route.catalog_id, "input": []}));
        runtime
            .request_codex_local_v0_150_summary(
                &route,
                &compact_request,
                &[
                    json!({
                        "type": "custom_tool_call",
                        "id": "ctc_1",
                        "call_id": "call_1",
                        "name": "apply_patch",
                        "input": "*** Begin Patch"
                    }),
                    json!({
                        "type": "custom_tool_call_output",
                        "id": "ctco_1",
                        "call_id": "call_1",
                        "output": "Done!"
                    }),
                ],
            )
            .await
            .expect("compatible summarizer request");

        let recorded = transport.requests.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        let body: Value = serde_json::from_slice(&recorded[0].body).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "call_1");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input.last().unwrap()["role"], "user");
    }

    #[tokio::test]
    async fn compaction_reports_the_engine_that_actually_ran() {
        // The merge gate requires requested and observed engine to match, so
        // the diagnostic must be written from the resolved engine rather than
        // from anything the request asked for.
        let mut route = chat_route();
        route.credential_id = Some("key-1".into());
        let credentials = MemoryCredentialProvider::new();
        credentials.insert("key-1", "test-token");
        let summarizer_response = json!({
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Handoff summary. Next: continue."},
                "finish_reason": "stop"
            }]
        });
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&summarizer_response).unwrap(),
        }]));
        let events = Arc::new(RecordingStore::new());
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(credentials),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport,
        )
        .with_diagnostics_sink(Arc::clone(&events) as Arc<dyn DiagnosticsSink>);

        let mut compact_request = request(json!({
            "model": "vlm-test",
            "input": [
                {"type": "message", "role": "user", "content": "build the thing"},
                {"type": "message", "role": "assistant", "content": "investigating"},
                {"type": "message", "role": "user", "content": "keep going"}
            ]
        }));
        compact_request.endpoint = crate::request::RuntimeEndpoint::Compact;
        runtime.execute_compact(compact_request).await.unwrap();

        let attempts: Vec<_> = events
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                DiagnosticEvent::LocalCompactionAttempt(attempt) => Some(attempt.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(attempts.len(), 1, "exactly one attempt: {attempts:?}");
        let attempt = &attempts[0];
        assert_eq!(attempt.engine_id, "codex_local_v0_150");
        assert_eq!(
            attempt.engine_provenance.as_deref(),
            Some("openai-codex@rust-v0.150.0-alpha.8+fcbdb578")
        );
        // The summarizer runs on the session's own model, never a separate
        // compactor.
        assert_eq!(attempt.upstream_model, "test-model");
        assert!(attempt.failure.is_none(), "{attempt:?}");
        assert_eq!(attempt.context_retreats, 0, "upstream accepted the input");
        assert!(attempt.tokens_before > 0);
        assert!(!attempt.source_hash.is_empty());
        assert!(attempt.replacement_hash.is_some());
    }

    #[tokio::test]
    async fn standalone_compact_without_a_previous_response_still_claims_a_durable_id() {
        // `execute_compact` only journals via `history.record_compaction`
        // when the request carries `previous_response_id` — a fully
        // stateless standalone compact is never journaled. The diagnostic
        // must say so honestly: `checkpoint_id`/`generation` absent, not a
        // fabricated id nothing can materialize after a restart.
        let summary_text = json!({
            "goal": "Answer the turns.",
            "done": ["Earlier turns handled"],
            "in_progress": [],
            "decisions": [],
            "next_steps": ["Answer the current turn"],
            "constraints": [],
            "user_preferences": []
        })
        .to_string();
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&json!({
                "id": "resp_stateless_sum",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": "msg_stateless_sum",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": summary_text, "annotations": []}]
                }],
                "usage": {"input_tokens": 40, "output_tokens": 30, "total_tokens": 70}
            }))
            .unwrap(),
        }]));
        let events = Arc::new(RecordingStore::new());
        // Codex 0.150 always calls the summarizer, so this route now needs a
        // resolvable credential where the prune-only path did not.
        let credentials = MemoryCredentialProvider::new();
        credentials.insert("key-1", "test-token");
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![sample_route(Some("key-1".into()))],
            }),
            Arc::new(credentials),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport,
        )
        .with_diagnostics_sink(Arc::clone(&events) as Arc<dyn DiagnosticsSink>);

        let mut input = Vec::new();
        for i in 0..20 {
            input.push(json!({"type": "message", "role": "user", "content": format!("turn {i}")}));
            input.push(
                json!({"type": "message", "role": "assistant", "content": format!("reply {i}")}),
            );
        }
        let mut compact_request = request(json!({
            "model": "vlm-test",
            "input": input
        }));
        compact_request.endpoint = crate::request::RuntimeEndpoint::Compact;
        let response = runtime.execute_compact(compact_request).await.unwrap();
        let RuntimeResponse::Json(body) = response else {
            panic!("compact must return JSON");
        };
        assert_eq!(body["output"][0]["type"], "compaction");
        let compaction_id = body["output"][0]["id"]
            .as_str()
            .expect("compaction item must carry its id")
            .to_string();

        let decisions: Vec<_> = events
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                DiagnosticEvent::CompactionDecision(decision) => Some(decision.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(decisions.len(), 1, "{decisions:?}");
        // Codex 0.150 journals a standalone compact under its own compaction
        // id even with no previous_response_id to bind to. That is what lets
        // the session resume after a restart (see
        // `standalone_compaction_survives_restart`); the retired V1 path left
        // such a compact unjournaled and therefore unresumable.
        assert_eq!(
            decisions[0].checkpoint_id.as_deref(),
            Some(compaction_id.as_str()),
            "a standalone compact must claim the durable id it was journaled under: {:?}",
            decisions[0]
        );
        assert_eq!(decisions[0].generation, Some(1));
    }

    #[tokio::test]
    async fn guardian_bills_every_leg_to_the_account_auto_review_names() {
        /// Records which account each leg asked to be billed to.
        struct RecordingOfficial(Arc<std::sync::Mutex<Vec<Option<String>>>>);

        #[async_trait::async_trait]
        impl crate::official_auth::OfficialAuthProvider for RecordingOfficial {
            async fn authorize(
                &self,
                route_id: &str,
            ) -> Result<crate::official_auth::OfficialAuthDecision, String> {
                self.authorize_as(route_id, None).await
            }

            async fn authorize_as(
                &self,
                _route_id: &str,
                account_id: Option<&str>,
            ) -> Result<crate::official_auth::OfficialAuthDecision, String> {
                self.0.lock().unwrap().push(account_id.map(str::to_owned));
                Ok(crate::official_auth::OfficialAuthDecision::Managed(
                    crate::official_auth::OfficialAuthorization {
                        access_token: "review-token".into(),
                        // Answers as the account it was asked for, so the
                        // identity check in `ResolvedAuth::resolve` passes
                        // and this test measures the threading, not the
                        // refusal (that is covered in `auth.rs`).
                        account_id: account_id.map(str::to_owned),
                        selection_revision: Some(0),
                        selection_verified: true,
                    },
                ))
            }

            async fn refresh_after_rejection(
                &self,
                _route_id: &str,
                _rejected: &crate::official_auth::OfficialAuthorization,
            ) -> Result<crate::official_auth::OfficialAuthorization, String> {
                Err("unused".into())
            }
        }

        let transport = Arc::new(RecordingTransport::new(vec![
            UpstreamResponse {
                status: 500,
                headers: Vec::new(),
                body: br#"{"error":{"message":"primary unavailable"}}"#.to_vec(),
            },
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: serde_json::to_vec(&json!({
                    "id": "resp_backup",
                    "object": "response",
                    "model": "backup-reviewer",
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "id": "msg_backup",
                        "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": "{\"outcome\":\"allow\"}", "annotations": []}]
                    }],
                    "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
                }))
                .unwrap(),
            },
        ]));
        let mut primary = sample_route(None);
        primary.route_id = "primary".into();
        primary.catalog_id = "primary-catalog".into();
        primary.upstream_model = "primary-reviewer".into();
        primary.auth_kind = RuntimeAuthKind::ChatGpt;
        let mut backup = primary.clone();
        backup.route_id = "backup".into();
        backup.catalog_id = "backup-catalog".into();
        backup.upstream_model = "backup-reviewer".into();
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![primary, backup],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(RecordingOfficial(asked.clone())),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        )
        .with_review_config(crate::review::ReviewSettings {
            before_send: true,
            route_id: "primary".into(),
            model: "primary-reviewer".into(),
            policy: Some(crate::review::ReviewPolicy::Failover),
            fallback_catalog_id: Some("backup-catalog".into()),
            official_account_id: Some("acct-review".into()),
            ..Default::default()
        });
        runtime
            .execute(
                request(json!({
                    "model": crate::review::AUTO_REVIEW_MODEL,
                    "input": [{"role": "user", "content": "Review this action"}]
                })),
                "test_exec",
            )
            .await
            .unwrap();
        // Both legs, not just the primary: a failover onto a second reviewer
        // must not quietly land on the default account.
        let asked = asked.lock().unwrap().clone();
        assert_eq!(
            asked,
            vec![
                Some("acct-review".to_string()),
                Some("acct-review".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn guardian_failover_executes_backup_after_primary_failure() {
        let transport = Arc::new(RecordingTransport::new(vec![
            UpstreamResponse {
                status: 500,
                headers: Vec::new(),
                body: br#"{"error":{"message":"primary unavailable"}}"#.to_vec(),
            },
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: serde_json::to_vec(&json!({
                    "id": "resp_backup",
                    "object": "response",
                    "model": "backup-reviewer",
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "id": "msg_backup",
                        "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": "{\"outcome\":\"allow\"}", "annotations": []}]
                    }],
                    "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
                }))
                .unwrap(),
            },
        ]));
        let mut primary = sample_route(None);
        primary.route_id = "primary".into();
        primary.catalog_id = "primary-catalog".into();
        primary.upstream_model = "primary-reviewer".into();
        primary.auth_kind = RuntimeAuthKind::None;
        let mut backup = primary.clone();
        backup.route_id = "backup".into();
        backup.catalog_id = "backup-catalog".into();
        backup.upstream_model = "backup-reviewer".into();
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![primary, backup],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        )
        .with_review_config(crate::review::ReviewSettings {
            before_send: true,
            route_id: "primary".into(),
            model: "primary-reviewer".into(),
            policy: Some(crate::review::ReviewPolicy::Failover),
            fallback_catalog_id: Some("backup-catalog".into()),
            ..Default::default()
        });
        let response = runtime
            .execute(
                request(json!({
                    "model": crate::review::AUTO_REVIEW_MODEL,
                    "input": [{"role": "user", "content": "Review this action"}]
                })),
                "test_exec",
            )
            .await
            .unwrap();
        let RuntimeResponse::Json(body) = response else {
            panic!("guardian fallback must return JSON");
        };
        assert_eq!(body["model"], "backup-reviewer");
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let primary_body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let backup_body: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(primary_body["model"], "primary-reviewer");
        assert_eq!(backup_body["model"], "backup-reviewer");
    }

    #[tokio::test]
    async fn guardian_usage_is_attributed_to_the_review_run() {
        // M9 review usage: a guardian execution must record a UsageRecord
        // carrying the review run id so the usage UI can attribute the turn.
        let reviewer_transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&json!({
                "id": "resp_review_1",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": "msg_review_1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "{\"outcome\":\"allow\"}", "annotations": []}]
                }],
                "usage": {"input_tokens": 4, "output_tokens": 2, "total_tokens": 6}
            }))
            .unwrap(),
        }]));
        let mut reviewer_route = sample_route(None);
        reviewer_route.route_id = "reviewer".into();
        reviewer_route.catalog_id = "reviewer-model".into();
        reviewer_route.upstream_model = "primary-reviewer".into();
        reviewer_route.auth_kind = RuntimeAuthKind::None;
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![reviewer_route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            reviewer_transport,
        )
        .with_usage_store(Arc::clone(&usage) as Arc<dyn UsageStore>)
        .with_review_config(crate::review::ReviewSettings {
            before_send: true,
            route_id: "reviewer".into(),
            model: "primary-reviewer".into(),
            policy: Some(crate::review::ReviewPolicy::Always),
            ..Default::default()
        });
        runtime
            .execute(
                request(json!({
                    "model": crate::review::AUTO_REVIEW_MODEL,
                    "input": [{"role": "user", "content": "Review this action"}]
                })),
                "test_exec",
            )
            .await
            .unwrap();
        let records = usage.records();
        assert!(
            records.iter().any(|record| record
                .review_run_id
                .as_deref()
                .is_some_and(|id| id.starts_with("review-")))
                && records
                    .iter()
                    .any(|record| record.model == "primary-reviewer"),
            "guardian usage must be attributed to the review run on the reviewer \
             route: {records:?}"
        );
        assert_eq!(records[0].review_tokens, 6);
    }

    /// A settings source whose value can be flipped mid-test, standing in
    /// for Desktop's `AppState`-backed source.
    struct MutableReviewSettingsSource(std::sync::Mutex<crate::review::ReviewSettings>);

    impl crate::review::ReviewSettingsSource for MutableReviewSettingsSource {
        fn current(&self) -> crate::review::ReviewSettings {
            self.0.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn a_review_settings_source_is_re_read_on_every_new_request() {
        // The root-cause bug this guards: a shared `ProxyRuntime` that copies
        // `ReviewSettings` once at construction never sees a later policy
        // edit until the proxy restarts. With a live source instead, the
        // very next `execute()` call must observe a change flipped in
        // between two requests -- with no new `ProxyRuntime` built.
        let transport = Arc::new(RecordingTransport::new(vec![
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: serde_json::to_vec(&json!({
                    "id": "resp_before",
                    "object": "response",
                    "status": "completed",
                    "output": [{
                        "type": "message", "id": "msg_before", "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": "{\"outcome\":\"allow\"}", "annotations": []}]
                    }],
                    "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
                }))
                .unwrap(),
            },
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: serde_json::to_vec(&json!({
                    "id": "resp_after",
                    "object": "response",
                    "status": "completed",
                    "output": [{
                        "type": "message", "id": "msg_after", "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": "{\"outcome\":\"allow\"}", "annotations": []}]
                    }],
                    "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
                }))
                .unwrap(),
            },
        ]));
        let mut first = sample_route(None);
        first.route_id = "first".into();
        first.catalog_id = "first-catalog".into();
        first.upstream_model = "first-reviewer".into();
        first.auth_kind = RuntimeAuthKind::None;
        let mut second = first.clone();
        second.route_id = "second".into();
        second.catalog_id = "second-catalog".into();
        second.upstream_model = "second-reviewer".into();
        let source = Arc::new(MutableReviewSettingsSource(std::sync::Mutex::new(
            crate::review::ReviewSettings {
                before_send: true,
                route_id: "first".into(),
                model: "first-reviewer".into(),
                policy: Some(crate::review::ReviewPolicy::Always),
                ..Default::default()
            },
        )));
        let runtime =
            ProxyRuntime::new(
                Arc::new(FixedCatalog {
                    routes: vec![first, second],
                }),
                Arc::new(MemoryCredentialProvider::new()),
                Arc::new(UnconfiguredOfficialAuthProvider),
                Arc::new(CountingRequestLifecycle::new()),
                transport.clone(),
            )
            .with_review_settings_source(
                source.clone() as Arc<dyn crate::review::ReviewSettingsSource>
            );

        let guardian_request = || {
            request(json!({
                "model": crate::review::AUTO_REVIEW_MODEL,
                "input": [{"role": "user", "content": "Review this action"}]
            }))
        };
        runtime
            .execute(guardian_request(), "req-before")
            .await
            .expect("first request dispatches under the original policy");

        // Flip the policy between requests -- no new `ProxyRuntime`.
        {
            let mut guard = source.0.lock().unwrap();
            guard.route_id = "second".into();
            guard.model = "second-reviewer".into();
        }

        runtime
            .execute(guardian_request(), "req-after")
            .await
            .expect("second request dispatches under the updated policy");

        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let before_body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let after_body: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(
            before_body["model"], "first-reviewer",
            "the first request must still use the policy in effect when it ran"
        );
        assert_eq!(
            after_body["model"], "second-reviewer",
            "the second request must pick up the policy change with no proxy restart"
        );
    }

    #[tokio::test]
    async fn guardian_primary_failure_and_fallback_success_share_one_review_run_id() {
        // Primary and fallback are two legs of the same logical review run:
        // both records must carry the identical `review_run_id`, and the
        // fallback's `review_reason` must name the primary route without
        // leaking the raw upstream error body.
        let transport = Arc::new(RecordingTransport::new(vec![
            UpstreamResponse {
                status: 503,
                headers: Vec::new(),
                body: br#"{"error":{"message":"a very long and possibly sensitive upstream diagnostic body that must never leak into review_reason"}}"#.to_vec(),
            },
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: serde_json::to_vec(&json!({
                    "id": "resp_backup",
                    "object": "response",
                    "status": "completed",
                    "output": [{
                        "type": "message", "id": "msg_backup", "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": "{\"outcome\":\"allow\"}", "annotations": []}]
                    }],
                    "usage": {"input_tokens": 2, "output_tokens": 1, "total_tokens": 3}
                }))
                .unwrap(),
            },
        ]));
        let mut primary = sample_route(None);
        primary.route_id = "primary".into();
        primary.catalog_id = "primary-catalog".into();
        primary.name = "Primary Reviewer".into();
        primary.upstream_model = "primary-reviewer".into();
        primary.auth_kind = RuntimeAuthKind::None;
        let mut backup = primary.clone();
        backup.route_id = "backup".into();
        backup.catalog_id = "backup-catalog".into();
        backup.upstream_model = "backup-reviewer".into();
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![primary, backup],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        )
        .with_usage_store(Arc::clone(&usage) as Arc<dyn UsageStore>)
        .with_review_config(crate::review::ReviewSettings {
            before_send: true,
            route_id: "primary".into(),
            model: "primary-reviewer".into(),
            policy: Some(crate::review::ReviewPolicy::Failover),
            fallback_catalog_id: Some("backup-catalog".into()),
            ..Default::default()
        });
        runtime
            .execute(
                request(json!({
                    "model": crate::review::AUTO_REVIEW_MODEL,
                    "input": [{"role": "user", "content": "Review this action"}]
                })),
                "test_exec",
            )
            .await
            .unwrap();
        let records = usage.records();
        let primary_record = records
            .iter()
            .find(|record| record.review_role.as_deref() == Some("primary"))
            .expect("the failed primary attempt must be recorded, not silently dropped");
        let fallback_record = records
            .iter()
            .find(|record| record.review_role.as_deref() == Some("fallback"))
            .expect("the successful fallback attempt must be recorded");
        assert_eq!(primary_record.status, 503);
        assert_eq!(fallback_record.status, 200);
        assert_eq!(
            primary_record.review_run_id, fallback_record.review_run_id,
            "primary and fallback are one logical review run"
        );
        assert!(primary_record.review_run_id.is_some());
        let reason = fallback_record
            .review_reason
            .as_deref()
            .expect("fallback must record why the primary was unavailable");
        assert!(
            reason.contains("Primary Reviewer"),
            "reason should name the failed route: {reason}"
        );
        assert!(
            !reason.contains("possibly sensitive upstream diagnostic"),
            "the bounded reason must never leak the raw upstream error body: {reason}"
        );
        assert!(
            reason.chars().count() <= GUARDIAN_FAILURE_REASON_MAX_CHARS + 1,
            "reason must stay bounded: {reason}"
        );
    }

    #[tokio::test]
    async fn guardian_primary_failure_without_fallback_claims_terminal_exactly_once() {
        // No fallback configured: primary's failure IS the dispatch's one
        // terminal outcome. Before the round-2 fix this wrote a plain
        // diagnostic row for primary and then relied on `execute()`'s
        // generic error handler to also write a terminal row keyed to the
        // *client-facing* model (`codex-auto-review`, unresolvable) --
        // producing either zero or a wrongly-attributed extra row.
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 503,
            headers: Vec::new(),
            body: br#"{"error":{"message":"primary down"}}"#.to_vec(),
        }]));
        let mut primary = sample_route(None);
        primary.route_id = "primary".into();
        primary.catalog_id = "primary-catalog".into();
        primary.upstream_model = "primary-reviewer".into();
        primary.auth_kind = RuntimeAuthKind::None;
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![primary],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        )
        .with_usage_store(Arc::clone(&usage) as Arc<dyn UsageStore>)
        .with_review_config(crate::review::ReviewSettings {
            before_send: true,
            route_id: "primary".into(),
            model: "primary-reviewer".into(),
            policy: Some(crate::review::ReviewPolicy::Always),
            fallback_catalog_id: None,
            ..Default::default()
        });
        let error = runtime
            .execute(
                request(json!({
                    "model": crate::review::AUTO_REVIEW_MODEL,
                    "input": [{"role": "user", "content": "Review this action"}]
                })),
                "test_exec_no_fallback",
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::ProviderUnavailable(_)));
        let records = usage.records();
        assert_eq!(
            records.len(),
            1,
            "no fallback means exactly one terminal row, not a diagnostic \
             row plus a second wrongly-attributed row: {records:?}"
        );
        assert_eq!(records[0].route_id, "primary");
        assert_eq!(records[0].model, "primary-reviewer");
        assert_eq!(records[0].review_role.as_deref(), Some("primary"));
        assert_eq!(records[0].status, 503);
        assert_eq!(records[0].outcome.as_deref(), Some("provider_failure"));
    }

    #[tokio::test]
    async fn guardian_fallback_failure_claims_terminal_with_the_fallback_route() {
        // Primary and fallback both fail: the terminal row must be the
        // fallback's own route/error, not the primary's, and there must be
        // exactly two rows total (primary diagnostic + fallback terminal) --
        // `execute()`'s generic error handler must find the terminal slot
        // already claimed and write nothing.
        let transport = Arc::new(RecordingTransport::new(vec![
            UpstreamResponse {
                status: 503,
                headers: Vec::new(),
                body: br#"{"error":{"message":"primary down"}}"#.to_vec(),
            },
            UpstreamResponse {
                status: 503,
                headers: Vec::new(),
                body: br#"{"error":{"message":"backup also down"}}"#.to_vec(),
            },
        ]));
        let mut primary = sample_route(None);
        primary.route_id = "primary".into();
        primary.catalog_id = "primary-catalog".into();
        primary.upstream_model = "primary-reviewer".into();
        primary.auth_kind = RuntimeAuthKind::None;
        let mut backup = primary.clone();
        backup.route_id = "backup".into();
        backup.catalog_id = "backup-catalog".into();
        backup.upstream_model = "backup-reviewer".into();
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![primary, backup],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        )
        .with_usage_store(Arc::clone(&usage) as Arc<dyn UsageStore>)
        .with_review_config(crate::review::ReviewSettings {
            before_send: true,
            route_id: "primary".into(),
            model: "primary-reviewer".into(),
            policy: Some(crate::review::ReviewPolicy::Failover),
            fallback_catalog_id: Some("backup-catalog".into()),
            ..Default::default()
        });
        let error = runtime
            .execute(
                request(json!({
                    "model": crate::review::AUTO_REVIEW_MODEL,
                    "input": [{"role": "user", "content": "Review this action"}]
                })),
                "test_exec_fallback_also_fails",
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::ProviderUnavailable(_)));
        let records = usage.records();
        assert_eq!(
            records.len(),
            2,
            "primary diagnostic + fallback terminal, no third row from the \
             generic error handler: {records:?}"
        );
        let same_run_id = records[0].review_run_id.clone();
        assert!(same_run_id.is_some());
        assert!(records
            .iter()
            .all(|record| record.review_run_id == same_run_id));
        let terminal = records
            .iter()
            .find(|record| record.review_role.as_deref() == Some("fallback"))
            .expect("fallback attempt must be recorded");
        assert_eq!(terminal.route_id, "backup");
        assert_eq!(terminal.model, "backup-reviewer");
        assert_eq!(terminal.status, 503);
        let primary_row = records
            .iter()
            .find(|record| record.review_role.as_deref() == Some("primary"))
            .expect("primary attempt must still be recorded as a diagnostic row");
        assert_eq!(primary_row.route_id, "primary");
    }

    /// Test-only transport: hangs forever on both `execute` and
    /// `execute_streaming`, exactly like `HangingHeadersTransport`, but
    /// wrapped so a `SequencedTransport` can compose it with a second,
    /// differently-behaved transport for the next call.
    struct PendingForeverTransport;

    #[async_trait::async_trait]
    impl UpstreamTransport for PendingForeverTransport {
        async fn execute(
            &self,
            _request: &UpstreamRequest,
        ) -> Result<UpstreamResponse, TransportError> {
            std::future::pending().await
        }

        async fn execute_streaming(
            &self,
            _request: &UpstreamRequest,
        ) -> Result<UpstreamStream, TransportError> {
            std::future::pending().await
        }
    }

    /// Opens headers immediately, yields the given chunks (a keepalive
    /// comment, a `response.created`, a partial delta — whatever the test
    /// wants), then never yields again and never ends the stream. Models a
    /// primary that is not dead, just stuck mid-attempt with no terminal
    /// assessment in sight.
    struct PartialThenStallTransport {
        chunks: Vec<Vec<u8>>,
    }

    #[async_trait::async_trait]
    impl UpstreamTransport for PartialThenStallTransport {
        async fn execute(
            &self,
            _request: &UpstreamRequest,
        ) -> Result<UpstreamResponse, TransportError> {
            std::future::pending().await
        }

        async fn execute_streaming(
            &self,
            _request: &UpstreamRequest,
        ) -> Result<UpstreamStream, TransportError> {
            let chunks = self.chunks.clone();
            Ok(UpstreamStream {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                body: async_stream::stream! {
                    for chunk in chunks {
                        yield Ok(Bytes::from(chunk));
                    }
                    std::future::pending::<()>().await;
                    #[allow(unreachable_code)]
                    {
                        yield Ok(Bytes::new());
                    }
                }
                .boxed(),
            })
        }
    }

    /// Dispatches the Nth call (0-indexed) to `first` if `n == 0`, else to
    /// `rest`. Lets a Failover test give the primary attempt one behavior
    /// (hang, stall, invalid content) and the fallback attempt another
    /// (succeed) without needing a bespoke transport per test.
    struct SequencedTransport {
        calls: std::sync::atomic::AtomicUsize,
        first: Arc<dyn UpstreamTransport>,
        rest: Arc<dyn UpstreamTransport>,
    }

    impl SequencedTransport {
        fn new(first: Arc<dyn UpstreamTransport>, rest: Arc<dyn UpstreamTransport>) -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                first,
                rest,
            }
        }
    }

    #[async_trait::async_trait]
    impl UpstreamTransport for SequencedTransport {
        async fn execute(
            &self,
            request: &UpstreamRequest,
        ) -> Result<UpstreamResponse, TransportError> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                self.first.execute(request).await
            } else {
                self.rest.execute(request).await
            }
        }

        async fn execute_streaming(
            &self,
            request: &UpstreamRequest,
        ) -> Result<UpstreamStream, TransportError> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                self.first.execute_streaming(request).await
            } else {
                self.rest.execute_streaming(request).await
            }
        }
    }

    /// One completed Responses-shaped SSE stream carrying a valid guardian
    /// assessment, ready to hand to `DelayedBodyTransport`/inline fixtures.
    /// Shape mirrors `inbound_sse_comment_is_ready_before_the_upstream_body`
    /// (a `response.created` event before `response.completed` — the
    /// third-party SSE normalizer's own working precedent for a minimal
    /// round trip), with the guardian decision placed directly in the
    /// completed event's own `output` array rather than a separate
    /// `response.output_item.done` event.
    fn guardian_assessment_sse(model: &str) -> Vec<u8> {
        let created = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",",
            "\"object\":\"response\",\"status\":\"in_progress\"}}\n\n",
        );
        let completed = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",",
            "\"object\":\"response\",\"status\":\"completed\",\"model\":\"__MODEL__\",",
            "\"output\":[{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",",
            "\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",",
            "\"text\":\"{\\\"outcome\\\":\\\"allow\\\"}\",\"annotations\":[]}]}],",
            "\"usage\":{\"input_tokens\":3,\"output_tokens\":2,\"total_tokens\":5}}}\n\n",
        )
        .replace("__MODEL__", model);
        format!("{created}{completed}").into_bytes()
    }

    fn guardian_failover_routes() -> (RuntimeModelRoute, RuntimeModelRoute) {
        let mut primary = sample_route(None);
        primary.route_id = "primary".into();
        primary.catalog_id = "primary-catalog".into();
        primary.upstream_model = "primary-reviewer".into();
        primary.auth_kind = RuntimeAuthKind::None;
        let mut backup = primary.clone();
        backup.route_id = "backup".into();
        backup.catalog_id = "backup-catalog".into();
        backup.upstream_model = "backup-reviewer".into();
        (primary, backup)
    }

    fn guardian_failover_runtime(
        primary: RuntimeModelRoute,
        backup: RuntimeModelRoute,
        transport: Arc<dyn UpstreamTransport>,
        usage: Arc<MemoryUsageStore>,
    ) -> ProxyRuntime {
        ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![primary, backup],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport,
        )
        .with_usage_store(Arc::clone(&usage) as Arc<dyn UsageStore>)
        .with_review_config(crate::review::ReviewSettings {
            before_send: true,
            route_id: "primary".into(),
            model: "primary-reviewer".into(),
            policy: Some(crate::review::ReviewPolicy::Failover),
            fallback_catalog_id: Some("backup-catalog".into()),
            ..Default::default()
        })
    }

    fn guardian_request() -> RuntimeRequest {
        let mut req = request(json!({
            "model": crate::review::AUTO_REVIEW_MODEL,
            "stream": true,
            "input": [{"role": "user", "content": "Review this action"}]
        }));
        req.body["stream"] = json!(true);
        req
    }

    /// Primary never opens response headers at all (`PendingForeverTransport`
    /// via `execute_streaming`'s default `std::future::pending`). The whole
    /// attempt — not just the header phase — must be judged against
    /// `GUARDIAN_ATTEMPT_TIMEOUT`, and the fallback must run and win.
    #[tokio::test(start_paused = true)]
    async fn guardian_primary_never_opens_headers_falls_over_after_attempt_timeout() {
        let (primary, backup) = guardian_failover_routes();
        let transport = Arc::new(SequencedTransport::new(
            Arc::new(PendingForeverTransport),
            Arc::new(DelayedBodyTransport {
                delay: std::time::Duration::from_millis(10),
                body: guardian_assessment_sse("backup-reviewer"),
            }),
        ));
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime = guardian_failover_runtime(primary, backup, transport, Arc::clone(&usage));
        let response = runtime
            .execute(guardian_request(), "test_headers_never_open")
            .await
            .unwrap();
        let RuntimeResponse::Sse(mut stream) = response else {
            panic!("guardian fallback must replay the buffered SSE stream");
        };
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next().await {
            collected.extend_from_slice(&chunk.unwrap());
        }
        assert!(
            String::from_utf8_lossy(&collected).contains("backup-reviewer"),
            "client must see the fallback's own content, byte-for-byte"
        );
        let records = usage.records();
        assert_eq!(
            records.len(),
            2,
            "primary diagnostic + fallback terminal: {records:?}"
        );
        let primary_row = records
            .iter()
            .find(|record| record.review_role.as_deref() == Some("primary"))
            .expect("primary attempt must be recorded as a diagnostic row");
        assert_eq!(
            primary_row.error_category.as_deref(),
            Some("review_attempt_timeout")
        );
        assert_eq!(primary_row.status, 504);
        let fallback_row = records
            .iter()
            .find(|record| record.review_role.as_deref() == Some("fallback"))
            .expect("fallback attempt must claim the terminal row");
        assert_eq!(fallback_row.outcome.as_deref(), Some("success"));
        assert_eq!(fallback_row.route_id, "backup");
        assert_eq!(primary_row.review_run_id, fallback_row.review_run_id);
    }

    /// Reproduces the reported MiMO -> Qwen sequence as one production
    /// dispatch: the primary reaches its 30s deadline, then the 88K fallback
    /// receives a fresh route-bounded request with a semantic user turn and
    /// returns a valid assessment.
    #[tokio::test(start_paused = true)]
    async fn guardian_timeout_failover_reprojects_large_context_for_fallback_window() {
        let (mut primary, mut backup) = guardian_failover_routes();
        primary.context_window = Some(200_000);
        backup.context_window = Some(88_064);
        let backup_transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: guardian_assessment_sse("backup-reviewer"),
        }]));
        let transport = Arc::new(SequencedTransport::new(
            Arc::new(PendingForeverTransport),
            backup_transport.clone(),
        ));
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime = guardian_failover_runtime(primary, backup, transport, Arc::clone(&usage));
        let mut request = guardian_request();
        request.body["instructions"] = Value::String(format!(
            "You are judging one planned coding-agent action.\n# User Authorization Scoring\n{}\n# Outcome Policy\nPLANNED_ACTION_AT_THE_END",
            "large inherited context ".repeat(40_000)
        ));

        let response = runtime
            .execute(request, "test_timeout_large_fallback")
            .await
            .unwrap();
        let RuntimeResponse::Sse(mut stream) = response else {
            panic!("guardian fallback must replay the buffered SSE stream");
        };
        while stream.next().await.is_some() {}

        let sent = backup_transport.requests.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
        assert!(
            crate::compaction::estimate_json_tokens(&body) <= 88_064 * 4 / 5,
            "fallback request exceeded its conservative route budget"
        );
        assert!(body.to_string().contains("exactly one JSON object"));
        let records = usage.records();
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0].error_category.as_deref(),
            Some("review_attempt_timeout")
        );
        assert_eq!(records[1].outcome.as_deref(), Some("success"));
    }

    /// Primary opens headers and streams a `response.created` plus one
    /// reasoning delta, then stalls forever with no terminal event. Proves
    /// the deadline covers the whole attempt, not just the header-open phase
    /// `STREAM_OPEN_HEADER_TIMEOUT` already bounds.
    #[tokio::test(start_paused = true)]
    async fn guardian_primary_stalls_mid_stream_after_partial_content_falls_over() {
        let (primary, backup) = guardian_failover_routes();
        let partial = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"in_progress\"}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"thinking...\"}\n\n",
        )
        .as_bytes()
        .to_vec();
        let transport = Arc::new(SequencedTransport::new(
            Arc::new(PartialThenStallTransport {
                chunks: vec![partial],
            }),
            Arc::new(DelayedBodyTransport {
                delay: std::time::Duration::from_millis(10),
                body: guardian_assessment_sse("backup-reviewer"),
            }),
        ));
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime = guardian_failover_runtime(primary, backup, transport, Arc::clone(&usage));
        let response = runtime
            .execute(guardian_request(), "test_partial_then_stall")
            .await
            .unwrap();
        assert!(matches!(response, RuntimeResponse::Sse(_)));
        let records = usage.records();
        let primary_row = records
            .iter()
            .find(|record| record.review_role.as_deref() == Some("primary"))
            .expect("primary attempt must be recorded");
        assert_eq!(
            primary_row.error_category.as_deref(),
            Some("review_attempt_timeout"),
            "a partial, non-terminal stream is exactly as fallback-eligible as no headers at all"
        );
    }

    /// Primary completes cleanly and fast, well inside the 30s attempt
    /// deadline, with a valid assessment. The fallback route must never be
    /// contacted at all.
    #[tokio::test(start_paused = true)]
    async fn guardian_primary_completing_within_deadline_never_calls_fallback() {
        let (primary, backup) = guardian_failover_routes();
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: guardian_assessment_sse("primary-reviewer"),
        }]));
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime =
            guardian_failover_runtime(primary, backup, transport.clone(), Arc::clone(&usage));
        let response = runtime
            .execute(guardian_request(), "test_primary_fast_success")
            .await
            .unwrap();
        let RuntimeResponse::Sse(mut stream) = response else {
            panic!("primary success must replay the buffered SSE stream");
        };
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next().await {
            collected.extend_from_slice(&chunk.unwrap());
        }
        assert!(String::from_utf8_lossy(&collected).contains("primary-reviewer"));
        assert_eq!(
            transport.requests.lock().unwrap().len(),
            1,
            "fallback must never be contacted when primary succeeds in time"
        );
        let records = usage.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].review_role.as_deref(), Some("primary"));
        assert_eq!(records[0].outcome.as_deref(), Some("success"));
    }

    /// Primary completes cleanly and on time, but its content is not a valid
    /// Guardian assessment. Per `review_failure_allows_fallback`, a completed
    /// attempt with bad content keeps its real `provider_protocol`
    /// classification and must never be masked by a fallback attempt — only
    /// a *timed-out* attempt is fallback-eligible.
    #[tokio::test(start_paused = true)]
    async fn guardian_primary_completes_with_invalid_assessment_keeps_protocol_error_no_fallback() {
        let (primary, backup) = guardian_failover_routes();
        let junk = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"not a decision at all\",\"annotations\":[]}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
        )
        .as_bytes()
        .to_vec();
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: junk,
        }]));
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime =
            guardian_failover_runtime(primary, backup, transport.clone(), Arc::clone(&usage));
        let error = runtime
            .execute(guardian_request(), "test_invalid_assessment")
            .await
            .unwrap_err();
        assert!(
            matches!(error, RuntimeError::ProviderProtocol(_)),
            "expected ProviderProtocol, got {error:?}"
        );
        assert_eq!(
            transport.requests.lock().unwrap().len(),
            1,
            "an invalid-but-complete attempt must never trigger a fallback retry"
        );
        let records = usage.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].review_role.as_deref(), Some("primary"));
        assert_eq!(
            records[0].error_category.as_deref(),
            Some("provider_protocol")
        );
    }

    /// Chat-wire reviewer: the primary's Chat SSE deltas must be routed
    /// through `ChatSseAdapter` into Responses-shaped events for validation,
    /// while the bytes actually replayed to the client stay the original
    /// Chat-wire bytes untouched.
    #[tokio::test(start_paused = true)]
    async fn guardian_chat_wire_primary_validates_the_already_normalized_responses_shaped_stream() {
        // `execute_streaming` dispatches a Chat-wire route through
        // `chat_stream`, which already normalizes raw Chat deltas into
        // Responses-shaped SSE (via its own internal `ChatSseAdapter`)
        // *before* `RuntimeResponse::Sse` is ever returned — so what this
        // dispatch buffers and replays is that already-normalized,
        // client-facing stream, for every wire, not the reviewer's raw Chat
        // bytes. See `drain_and_validate_guardian_sse`.
        let (mut primary, backup) = guardian_failover_routes();
        primary.wire = RuntimeWireFormat::Chat;
        let chat_sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"outcome\\\":\\\"allow\\\"}\"}}]}\n\n",
            "data: [DONE]\n\n",
        )
        .as_bytes()
        .to_vec();
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: chat_sse,
        }]));
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime =
            guardian_failover_runtime(primary, backup, transport.clone(), Arc::clone(&usage));
        let response = runtime
            .execute(guardian_request(), "test_chat_wire_primary")
            .await
            .unwrap();
        let RuntimeResponse::Sse(mut stream) = response else {
            panic!("primary success must replay the buffered SSE stream");
        };
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next().await {
            collected.extend_from_slice(&chunk.unwrap());
        }
        let text = String::from_utf8_lossy(&collected);
        assert!(
            text.contains("response.completed"),
            "the replayed stream must be Responses-shaped: {text}"
        );
        assert!(
            text.contains(r#"\"outcome\":\"allow\""#) || text.contains(r#""outcome":"allow""#),
            "the guardian's decision must survive the Chat-to-Responses normalization: {text}"
        );
        assert_eq!(
            transport.requests.lock().unwrap().len(),
            1,
            "fallback must never be contacted when the Chat-wire primary validates"
        );
        let records = usage.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome.as_deref(), Some("success"));
    }

    /// A client cancellation observed before the primary attempt starts must
    /// stop the whole dispatch — no fallback attempt, no accounting row
    /// fabricated for an attempt that never ran.
    #[tokio::test(start_paused = true)]
    async fn guardian_client_cancel_before_primary_starts_never_attempts_fallback() {
        let (primary, backup) = guardian_failover_routes();
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: guardian_assessment_sse("primary-reviewer"),
        }]));
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime =
            guardian_failover_runtime(primary, backup, transport.clone(), Arc::clone(&usage));
        runtime.cancel_request("test_client_cancel_early");
        let error = runtime
            .execute(guardian_request(), "test_client_cancel_early")
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::ProviderUnavailable(_)));
        assert_eq!(
            transport.requests.lock().unwrap().len(),
            0,
            "a cancel observed before dispatch even starts must never reach the upstream"
        );
        assert_eq!(
            usage.records().len(),
            0,
            "no attempt ran, so there is nothing to attribute a usage row to"
        );
    }

    fn guardian_failed_sse(model: &str) -> Vec<u8> {
        let created = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",",
            "\"object\":\"response\",\"status\":\"in_progress\"}}\n\n",
        );
        let failed = concat!(
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_1\",",
            "\"object\":\"response\",\"status\":\"failed\",\"model\":\"__MODEL__\",",
            "\"error\":{\"message\":\"reviewer refused the request\"}}}\n\n",
        )
        .replace("__MODEL__", model);
        format!("{created}{failed}").into_bytes()
    }

    /// A primary that reaches `response.completed` and then keeps the
    /// connection open indefinitely (no more real events, connection never
    /// closes) must succeed immediately on the completed event, not hang
    /// until `GUARDIAN_ATTEMPT_TIMEOUT` waiting for a close that will never
    /// come. Proven the same way `guardian_primary_completing_within_deadline_never_calls_fallback`
    /// proves fast-success: fallback must never be contacted, which would
    /// only happen if draining incorrectly waited for stream EOF and hit the
    /// attempt deadline instead of the terminal event.
    #[tokio::test(start_paused = true)]
    async fn guardian_primary_succeeds_immediately_on_completed_even_when_the_connection_stalls_after(
    ) {
        let (primary, backup) = guardian_failover_routes();
        let transport = Arc::new(PartialThenStallTransport {
            chunks: vec![guardian_assessment_sse("primary-reviewer")],
        });
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime = guardian_failover_runtime(primary, backup, transport, Arc::clone(&usage));
        let response = runtime
            .execute(guardian_request(), "test_completed_then_stall")
            .await
            .unwrap();
        let RuntimeResponse::Sse(mut stream) = response else {
            panic!("primary success must replay the buffered SSE stream");
        };
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next().await {
            collected.extend_from_slice(&chunk.unwrap());
        }
        assert!(String::from_utf8_lossy(&collected).contains("primary-reviewer"));
        let records = usage.records();
        assert_eq!(
            records.len(),
            1,
            "the primary's own success must be the only row -- fallback was never reachable \
             (the mock transport only serves one call), so any fallback attempt at all would \
             have hung forever: {records:?}"
        );
        assert_eq!(records[0].review_role.as_deref(), Some("primary"));
        assert_eq!(records[0].outcome.as_deref(), Some("success"));
    }

    /// A primary that reaches `response.failed` and then stalls must fail
    /// immediately with `ProviderProtocol` (a real, structural reviewer
    /// error -- not fallback-eligible), not sit until the attempt deadline
    /// and get *misclassified* as `ReviewAttemptTimeout` (which *is*
    /// fallback-eligible) just because draining incorrectly waited for the
    /// connection to close instead of stopping on the terminal event.
    #[tokio::test(start_paused = true)]
    async fn guardian_primary_fails_immediately_on_response_failed_even_when_the_connection_stalls_after(
    ) {
        let (primary, backup) = guardian_failover_routes();
        let transport = Arc::new(PartialThenStallTransport {
            chunks: vec![guardian_failed_sse("primary-reviewer")],
        });
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime = guardian_failover_runtime(primary, backup, transport, Arc::clone(&usage));
        let error = runtime
            .execute(guardian_request(), "test_failed_then_stall")
            .await
            .unwrap_err();
        assert!(
            matches!(error, RuntimeError::ProviderProtocol(_)),
            "expected ProviderProtocol (not misclassified as a fallback-eligible timeout), got {error:?}"
        );
        let records = usage.records();
        assert_eq!(
            records.len(),
            1,
            "a real response.failed must never trigger a fallback attempt (which would have \
             hung forever against this single-call mock transport): {records:?}"
        );
        assert_eq!(records[0].review_role.as_deref(), Some("primary"));
        assert_eq!(
            records[0].error_category.as_deref(),
            Some("provider_protocol")
        );
    }

    /// The fallback leg gets its own fresh `GUARDIAN_ATTEMPT_TIMEOUT`, not
    /// leftover confusion from primary's own deadline: primary fails fast,
    /// fallback opens headers and starts responding, then stalls before ever
    /// reaching a terminal event -- fallback's own attempt must time out on
    /// its own account and correctly claim the terminal `ReviewAttemptTimeout`
    /// row (no third leg exists to retry against).
    #[tokio::test(start_paused = true)]
    async fn guardian_fallback_quick_start_but_stuck_times_out_on_its_own_deadline() {
        let (primary, backup) = guardian_failover_routes();
        let transport = Arc::new(SequencedTransport::new(
            Arc::new(PendingForeverTransport),
            Arc::new(PartialThenStallTransport {
                chunks: vec![concat!(
                    "event: response.created\n",
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_2\",",
                    "\"object\":\"response\",\"status\":\"in_progress\"}}\n\n",
                )
                .as_bytes()
                .to_vec()],
            }),
        ));
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime = guardian_failover_runtime(primary, backup, transport, Arc::clone(&usage));
        let error = runtime
            .execute(guardian_request(), "test_fallback_quick_start_stuck")
            .await
            .unwrap_err();
        assert!(
            matches!(error, RuntimeError::ReviewAttemptTimeout(_)),
            "expected ReviewAttemptTimeout, got {error:?}"
        );
        let records = usage.records();
        let fallback_row = records
            .iter()
            .find(|record| record.review_role.as_deref() == Some("fallback"))
            .expect("fallback attempt must claim the terminal row");
        assert_eq!(
            fallback_row.error_category.as_deref(),
            Some("review_attempt_timeout")
        );
        assert_eq!(fallback_row.route_id, "backup");
    }

    /// A client that drops the replay stream before reading a single chunk
    /// must still produce exactly one terminal row -- `client_disconnect`
    /// (499) -- via `DeferredGuardianClaimGuard`'s `Drop` impl, not silently
    /// vanish with no accounting at all.
    #[tokio::test(start_paused = true)]
    async fn guardian_client_dropping_the_replay_before_any_chunk_records_client_disconnect() {
        let (primary, backup) = guardian_failover_routes();
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: guardian_assessment_sse("primary-reviewer"),
        }]));
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime = guardian_failover_runtime(primary, backup, transport, Arc::clone(&usage));
        let response = runtime
            .execute(guardian_request(), "test_zero_chunk_drop")
            .await
            .unwrap();
        let RuntimeResponse::Sse(stream) = response else {
            panic!("primary success must return the buffered SSE stream");
        };
        drop(stream);
        let records = usage.records();
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].outcome.as_deref(), Some("client_disconnect"));
        assert_eq!(records[0].status, 499);
    }

    /// A client that reads some but not all of the replayed chunks before
    /// dropping the stream must record `client_disconnect`, not `success` --
    /// full delivery is the only thing that disarms the guard.
    #[tokio::test(start_paused = true)]
    async fn guardian_client_dropping_the_replay_partway_records_client_disconnect() {
        let (primary, backup) = guardian_failover_routes();
        // Two distinct SSE blocks so there is a real "partway" to stop at.
        let mut body = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",",
            "\"object\":\"response\",\"status\":\"in_progress\"}}\n\n",
        )
        .as_bytes()
        .to_vec();
        body.extend_from_slice(&guardian_assessment_sse("primary-reviewer"));
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body,
        }]));
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime = guardian_failover_runtime(primary, backup, transport, Arc::clone(&usage));
        let response = runtime
            .execute(guardian_request(), "test_partial_replay_drop")
            .await
            .unwrap();
        let RuntimeResponse::Sse(mut stream) = response else {
            panic!("primary success must return the buffered SSE stream");
        };
        // The default `RecordingTransport::execute_streaming` (via the trait
        // default) yields the whole body as a single chunk, so reading just
        // the first item already exercises "some but not all of the
        // conceptual replay was observed" from the guard's point of view:
        // the loop inside `defer_guardian_terminal_claim` never reaches its
        // own end because the caller stops polling here.
        assert!(stream.next().await.is_some());
        drop(stream);
        let records = usage.records();
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].outcome.as_deref(), Some("client_disconnect"));
    }

    /// The mirror image of the two drop tests above: reading the replay to
    /// completion must claim `success`, never `client_disconnect`, even
    /// though the guard's `Drop` still runs at the end either way.
    #[tokio::test(start_paused = true)]
    async fn guardian_client_reading_the_full_replay_records_success_not_disconnect() {
        let (primary, backup) = guardian_failover_routes();
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: guardian_assessment_sse("primary-reviewer"),
        }]));
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime = guardian_failover_runtime(primary, backup, transport, Arc::clone(&usage));
        let response = runtime
            .execute(guardian_request(), "test_full_replay")
            .await
            .unwrap();
        let RuntimeResponse::Sse(mut stream) = response else {
            panic!("primary success must return the buffered SSE stream");
        };
        while stream.next().await.is_some() {}
        drop(stream);
        let records = usage.records();
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].outcome.as_deref(), Some("success"));
    }

    #[test]
    fn review_failure_allows_fallback_only_for_provider_availability_categories() {
        let allowed = [
            RuntimeError::provider_quota("quota exceeded", Some(30), true),
            RuntimeError::ProviderUnavailable("upstream down".into()),
            RuntimeError::StreamOpenTimeout("30s".into()),
            RuntimeError::ReviewAttemptTimeout("30s".into()),
        ];
        for error in &allowed {
            assert!(
                review_failure_allows_fallback(error),
                "{error:?} must be allowed to fail over"
            );
        }
        let disallowed = [
            RuntimeError::InvalidRequest("bad body".into()),
            RuntimeError::CredentialMissing("no key".into()),
            RuntimeError::AuthenticationFailed("bad token".into()),
            RuntimeError::ProviderUnauthorized("401".into()),
            RuntimeError::ProviderProtocol("malformed".into()),
            RuntimeError::RouteNotFound("missing".into()),
            RuntimeError::RouteInactive("disabled".into()),
            RuntimeError::Internal("bug".into()),
            RuntimeError::UnsupportedMode("unsupported".into()),
        ];
        for error in &disallowed {
            assert!(
                !review_failure_allows_fallback(error),
                "{error:?} must keep its real classification, never be masked by a fallback"
            );
        }
    }

    #[tokio::test]
    async fn guardian_auth_failure_never_triggers_fallback() {
        // A 401 (`ProviderUnauthorized`) is a credential problem, not a
        // "this Provider is temporarily unavailable" problem --
        // `review_failure_allows_fallback` must keep it from being masked by
        // a fallback attempt. The fallback transport must never even be
        // called, and the surfaced error must be primary's real
        // classification.
        let transport = Arc::new(RecordingTransport::new(vec![
            UpstreamResponse {
                status: 401,
                headers: Vec::new(),
                body: br#"{"error":{"message":"invalid api key"}}"#.to_vec(),
            },
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: serde_json::to_vec(&json!({
                    "id": "resp_backup",
                    "object": "response",
                    "status": "completed",
                    "output": [{
                        "type": "message", "id": "msg_backup", "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": "{\"outcome\":\"allow\"}", "annotations": []}]
                    }],
                    "usage": {"input_tokens": 2, "output_tokens": 1, "total_tokens": 3}
                }))
                .unwrap(),
            },
        ]));
        let mut primary = sample_route(None);
        primary.route_id = "primary".into();
        primary.catalog_id = "primary-catalog".into();
        primary.upstream_model = "primary-reviewer".into();
        primary.auth_kind = RuntimeAuthKind::None;
        let mut backup = primary.clone();
        backup.route_id = "backup".into();
        backup.catalog_id = "backup-catalog".into();
        backup.upstream_model = "backup-reviewer".into();
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![primary, backup],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        )
        .with_usage_store(Arc::clone(&usage) as Arc<dyn UsageStore>)
        .with_review_config(crate::review::ReviewSettings {
            before_send: true,
            route_id: "primary".into(),
            model: "primary-reviewer".into(),
            policy: Some(crate::review::ReviewPolicy::Failover),
            fallback_catalog_id: Some("backup-catalog".into()),
            ..Default::default()
        });
        let error = runtime
            .execute(
                request(json!({
                    "model": crate::review::AUTO_REVIEW_MODEL,
                    "input": [{"role": "user", "content": "Review this action"}]
                })),
                "test_exec_auth_no_fallback_attempt",
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, RuntimeError::ProviderUnauthorized(_)),
            "an auth failure must keep its real classification: {error:?}"
        );
        assert_eq!(
            transport.requests.lock().unwrap().len(),
            1,
            "the fallback route must never be contacted for a disallowed failure category"
        );
        let records = usage.records();
        assert_eq!(records.len(), 1, "exactly one terminal row: {records:?}");
        assert_eq!(records[0].route_id, "primary");
        assert_eq!(records[0].review_role.as_deref(), Some("primary"));
        assert_eq!(records[0].status, 401);
        assert_eq!(
            records[0].error_category.as_deref(),
            Some("provider_unauthorized")
        );
    }

    #[test]
    fn journal_materialization_converts_item_and_identity_together() {
        let history = Arc::new(MemoryHistoryStore::new());
        history
            .record_compaction(
                "resp_1",
                "cmp_vellum_journal",
                1,
                vec![
                    json!({"role": "user", "content": "checkpoint one"}),
                    json!({"role": "assistant", "content": "checkpoint two"}),
                ],
            )
            .unwrap();
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![sample_route(None)],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            Arc::new(RecordingTransport::new(Vec::new())),
        )
        .with_history_store(history);
        let mut body = json!({
            "input": [
                {
                    "type": "compaction",
                    "id": "cmp_vellum_journal",
                    "encrypted_content": format!("{LOCAL_COMPACTION_PREFIX}cmp_vellum_journal")
                },
                {"role": "user", "content": "continue the task"}
            ]
        });
        let mut replay = ReplayContext {
            item_identities: vec!["req:0".into(), "req:1".into()],
            suffix_start: 1,
            suffix_end: 2,
            ..Default::default()
        };
        runtime
            .materialize_local_compactions(
                RuntimeProviderKind::OpenAiCompatible,
                &mut body,
                false,
                false,
                &mut replay,
            )
            .unwrap();
        assert_eq!(body["input"].as_array().unwrap().len(), 3);
        assert_eq!(
            replay.item_identities,
            vec![
                journal_item_identity("cmp_vellum_journal", 0),
                journal_item_identity("cmp_vellum_journal", 1),
                "req:1".to_string(),
            ]
        );
        assert_eq!(replay.suffix_start, 2);
        assert_eq!(replay.suffix_end, 3);
    }

    #[test]
    fn identity_length_mismatch_is_an_internal_protocol_error() {
        let runtime = runtime_with(sample_route(None));
        let mut body = json!({
            "input": [
                {"role": "user", "content": "one"},
                {"role": "user", "content": "two"}
            ]
        });
        let mut replay = ReplayContext {
            item_identities: vec!["only-one".into()],
            ..Default::default()
        };
        let error = runtime
            .materialize_local_compactions(
                RuntimeProviderKind::OpenAiCompatible,
                &mut body,
                false,
                false,
                &mut replay,
            )
            .unwrap_err();
        assert!(
            matches!(error, RuntimeError::Internal(ref message) if message.contains("item_identities")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn client_local_repair_and_chat_projection_keep_aligned_identities() {
        let history = Arc::new(MemoryHistoryStore::new());
        history
            .record_exchange(
                &json!({
                    "model": "vlm-test",
                    "input": [{"role": "user", "content": "SECRET_PROMPT_TEXT"}]
                }),
                &json!({
                    "id": "resp_source",
                    "output": [{"type": "message", "id": "msg_1", "role": "assistant", "content": "ok"}]
                }),
                "route-1",
            )
            .unwrap();
        let mut chat_route = sample_route(None);
        chat_route.wire = RuntimeWireFormat::Chat;
        chat_route.auth_kind = RuntimeAuthKind::None;
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&json!({
                "id": "chatcmpl_1",
                "object": "chat.completion",
                "choices": [{"message": {"role": "assistant", "content": "continued"}}],
                "usage": {"prompt_tokens": 4, "completion_tokens": 1, "total_tokens": 5}
            }))
            .unwrap(),
        }]));
        let events = Arc::new(RecordingStore::new());
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![chat_route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport,
        )
        .with_history_store(history)
        .with_diagnostics_sink(Arc::clone(&events) as Arc<dyn DiagnosticsSink>);
        runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "previous_response_id": "resp_source",
                    "input": [
                        {"type": "context_compaction", "id": "cc_1"},
                        {"role": "user", "content": "continue after checkpoint"}
                    ]
                })),
                "repair_chat",
            )
            .await
            .unwrap();
        let transcripts: Vec<_> = events
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                DiagnosticEvent::HarnessTranscript(item) => Some(item.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(transcripts.len(), 1, "{transcripts:?}");
        assert!(
            !transcripts[0].user_turn_hashes.is_empty(),
            "production diagnostics must hash occurrence ids: {transcripts:?}"
        );
        assert!(transcripts[0].message_count >= 1);
        assert!(transcripts[0].role_sequence.contains("user"));
        assert_eq!(transcripts[0].empty_messages, 0);
        assert_eq!(transcripts[0].orphan_tool_results, 0);
        assert!(transcripts[0].body_bytes.unwrap_or_default() > 0);
        assert_eq!(transcripts[0].model.as_deref(), Some("test-model"));
        assert_eq!(transcripts[0].upstream_status, Some(200));
        let encoded = serde_json::to_string(&transcripts[0]).unwrap();
        assert!(
            !encoded.contains("SECRET_PROMPT_TEXT"),
            "diagnostics must not leak prompt text: {encoded}"
        );
        assert!(
            !encoded.contains("continue after checkpoint"),
            "diagnostics must not leak suffix text: {encoded}"
        );
        assert!(transcripts[0]
            .user_turn_hashes
            .iter()
            .all(|item| item.starts_with("sha256:")));
    }

    fn cancel_runtime() -> ProxyRuntime {
        ProxyRuntime::new(
            Arc::new(FixedCatalog { routes: Vec::new() }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            Arc::new(RecordingTransport::new(Vec::new())),
        )
    }

    #[test]
    fn cancel_all_in_flight_for_stop_fires_every_socket_and_records_user_stopped() {
        let runtime = cancel_runtime();
        let (_a, rx_a) = runtime.register_request_cancel("exec_a");
        let (_b, rx_b) = runtime.register_request_cancel("exec_b");
        let ids = runtime.cancel_all_in_flight_for_stop();
        assert!(ids.contains(&"exec_a".to_string()));
        assert!(ids.contains(&"exec_b".to_string()));
        assert!(*rx_a.borrow());
        assert!(*rx_b.borrow());
        assert!(runtime.is_request_cancelled("exec_a"));
        assert!(runtime.is_request_cancelled("exec_b"));
        let records = runtime.usage_records().unwrap_or_default();
        assert!(
            records.iter().any(|row| {
                row.outcome.as_deref() == Some("user_stopped_proxy")
                    && row.error_category.as_deref() == Some("proxy_stopped")
                    && row.error.as_deref() == Some("user stopped proxy")
            }),
            "stop must record user_stopped_proxy, not success or a provider error: {records:?}"
        );
        // A later success claim must not overwrite the stop.
        assert!(!runtime.finish_terminal_once(
            "exec_a", "route", "provider", "model", "req", None, 1, None, None, None, "success",
            None, None, None,
        ));
    }

    #[test]
    fn binding_children_fires_fanout_when_parent_cancelled() {
        let runtime = cancel_runtime();
        let (_parent_guard, parent_cancel) = runtime.register_request_cancel("parent-1");
        let (_child_guard, child_cancel) = runtime.register_request_cancel("child-1");
        assert!(!runtime.is_request_cancelled("child-1"));
        assert!(!runtime.bind_child_to_parent("parent-1", "child-1"));

        let fanned = runtime.cancel_request_tree("parent-1");
        assert_eq!(fanned, vec!["child-1".to_string()]);
        assert!(runtime.is_request_cancelled("parent-1"));
        assert!(runtime.is_request_cancelled("child-1"));
        // Both the parent's own and the bound child's channels observe the
        // cancellation through the same one-shot channel.
        assert!(*parent_cancel.borrow());
        assert!(*child_cancel.borrow());
        // Idempotent: a second cancel of the same parent no-ops the fan-out.
        let second = runtime.cancel_request_tree("parent-1");
        assert_eq!(second, vec!["child-1".to_string()]);
    }

    #[test]
    fn child_linking_to_already_cancelled_parent_aborts() {
        let runtime = cancel_runtime();
        runtime.cancel_request("parent-1");
        assert!(runtime.is_request_cancelled("parent-1"));
        // Point B binding of a late child must report the parent cancel.
        let aborted = runtime.bind_child_to_parent("parent-1", "child-1");
        assert!(aborted);
        // The abort path also fires the child's own channel.
        runtime.cancel_request("child-1");
        assert!(runtime.is_request_cancelled("child-1"));
    }

    #[test]
    fn cancelling_one_tree_leaves_unrelated_requests_running() {
        let runtime = cancel_runtime();
        let (_other_guard, other) = runtime.register_request_cancel("beta-child");
        let (_alpha_guard, _alpha_rx) = runtime.register_request_cancel("alpha-parent");
        runtime.bind_child_to_parent("alpha-parent", "alpha-child");

        runtime.cancel_request_tree("alpha-parent");
        assert!(runtime.is_request_cancelled("alpha-child"));
        assert!(!runtime.is_request_cancelled("beta-child"));
        assert!(!*other.borrow());
    }

    #[test]
    fn direct_cancel_fires_own_channel_and_marks_it() {
        let runtime = cancel_runtime();
        let (_guard, rx) = runtime.register_request_cancel("solo-1");
        runtime.cancel_request("solo-1");
        assert!(runtime.is_request_cancelled("solo-1"));
        assert!(*rx.borrow());
    }

    #[test]
    fn cancel_landed_before_registration_is_immediately_visible() {
        // P0 regression: a late re-registration of an execution id that a
        // tree cancel already marked used to mint a fresh
        // `watch::channel(false)` that hid the cancellation until the next
        // touch, so a child racing its parent's cancel kept executing with no
        // proactive wakeup. The newborn receiver must observe the pre-existing
        // cancel at birth.
        let runtime = cancel_runtime();
        let (_child_guard, _child_rx) = runtime.register_request_cancel("child-1");
        runtime.bind_child_to_parent("parent-1", "child-1");
        runtime.cancel_request_tree("parent-1");
        assert!(runtime.is_request_cancelled("child-1"));
        let (_guard, rx) = runtime.register_request_cancel("child-1");
        assert!(
            *rx.borrow(),
            "re-registering an already-cancelled execution id must start already-cancelled"
        );
    }

    /// A usage store that rejects every write, so terminal paths must keep the
    /// first-writer claim and leak the failure as a bounded diagnostic.
    struct RejectingUsageStore;

    impl UsageStore for RejectingUsageStore {
        fn record(&self, _value: &UsageRecord) -> Result<(), String> {
            Err("disk full".to_string())
        }

        fn summary(&self, _route_id: &str) -> Result<UsageSummary, String> {
            Err("unused".to_string())
        }

        fn records(&self) -> Result<Vec<UsageRecord>, String> {
            Err("unused".to_string())
        }

        fn mark_first_downstream_frame(
            &self,
            _request_id: &str,
            _elapsed_ms: u64,
        ) -> Result<(), String> {
            Err("unused".to_string())
        }
    }

    struct RecordingStore {
        events: std::sync::Mutex<Vec<DiagnosticEvent>>,
    }

    impl RecordingStore {
        fn new() -> Self {
            Self {
                events: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl DiagnosticsSink for RecordingStore {
        fn record(&self, event: DiagnosticEvent) {
            self.events.lock().unwrap().push(event);
        }

        fn detail_level(&self) -> DetailLevel {
            crate::bounded_detail_level(DetailLevel::T1Summary, None, 0)
        }
    }

    #[test]
    fn rejected_usage_write_keeps_claim_and_records_one_diagnostic() {
        let runtime = cancel_runtime();
        let events = std::sync::Arc::new(RecordingStore::new());
        let runtime = runtime
            .with_usage_store(std::sync::Arc::new(RejectingUsageStore) as Arc<dyn UsageStore>)
            .with_diagnostics_sink(Arc::clone(&events) as Arc<dyn DiagnosticsSink>);
        let (_guard, _rx) = runtime.register_request_cancel("solo-3");
        let accepted = runtime.finish_terminal_once_with_record(
            "solo-3",
            "success",
            UsageRecord {
                route_id: "opened".into(),
                provider: "fixture".into(),
                model: "fixture-model".into(),
                status: 200,
                duration_ms: 1,
                request_id: Some("req-solo-3".into()),
                outcome: Some("success".into()),
                ..Default::default()
            },
        );
        assert!(accepted, "the first terminal write still wins the claim");
        assert!(
            !runtime.claim_terminal_once("solo-3", "client_cancel"),
            "a rejected usage write must never roll the first-writer claim back"
        );
        let failures: Vec<DiagnosticEvent> = events
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, DiagnosticEvent::UsageWriteFailure(_)))
            .cloned()
            .collect();
        assert_eq!(
            failures.len(),
            1,
            "exactly one bounded diagnostic: {failures:?}"
        );
        match &failures[0] {
            DiagnosticEvent::UsageWriteFailure(failure) => {
                assert_eq!(failure.execution_id, "solo-3");
                assert_eq!(failure.outcome, "success");
                assert!(
                    failure.error.len() <= 512,
                    "the diagnostic must stay bounded: {failure:?}"
                );
            }
            other => panic!("unexpected event kind: {other:?}"),
        }
    }

    #[test]
    fn registry_cap_evicts_oldest_when_a_map_overflows() {
        // The last-resort bound is a hard cap independent of TTL arithmetic:
        // once any registry map exceeds REGISTRY_MAX_ENTRIES, the next prune
        // evicts the oldest entries. Sizes are asserted directly so a future
        // change that accidentally removes the bound fails loudly without
        // needing a 20k-turn timing test.
        let runtime = cancel_runtime();
        let overflow = 64usize;
        {
            let mut registry = runtime
                .parent_cancel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for index in 0..REGISTRY_MAX_ENTRIES + overflow {
                let id = format!("cap-{index}");
                let (sender, _) = watch::channel(false);
                registry.sockets.insert(
                    id.clone(),
                    TimestampedSender {
                        sender,
                        created: tokio::time::Instant::now(),
                    },
                );
                registry.terminal.insert(
                    id.clone(),
                    TimestampedOutcome {
                        created: tokio::time::Instant::now(),
                    },
                );
                registry
                    .cancelled
                    .insert(id.clone(), tokio::time::Instant::now());
                registry.parents.insert(
                    id.clone(),
                    ParentCancelEntry {
                        cancelled: false,
                        created: tokio::time::Instant::now(),
                        children: Vec::new(),
                    },
                );
            }
        }
        // One public method touch runs the prune pass over every map. The
        // probe's own socket lands after that prune, so its map may sit one
        // entry over the cap until the next touch.
        let (guard, _rx) = runtime.register_request_cancel("cap-probe");
        drop(guard);
        let snapshot = runtime.registry_len();
        assert!(
            snapshot.terminal <= REGISTRY_MAX_ENTRIES
                && snapshot.cancelled <= REGISTRY_MAX_ENTRIES
                && snapshot.parents <= REGISTRY_MAX_ENTRIES
                && snapshot.sockets <= REGISTRY_MAX_ENTRIES + 1,
            "overflowing the maps must evict down to the cap, not keep growing: {snapshot:?}"
        );
        assert!(
            snapshot.terminal == REGISTRY_MAX_ENTRIES,
            "the pre-filled terminal map must shed exactly its overflow back down to the cap: {snapshot:?}"
        );
    }

    #[test]
    fn terminal_outcome_is_first_write_wins() {
        let runtime = cancel_runtime();
        assert!(runtime.claim_terminal_once("child-1", "client_cancel"));
        assert!(
            !runtime.claim_terminal_once("child-1", "success"),
            "a late completed must never overwrite the cancel that already landed"
        );
        assert!(runtime.claim_terminal_once("other-1", "success"));
    }

    #[test]
    fn a_normal_completion_releases_its_own_socket() {
        // P0 regression: sockets used to leak forever on the ordinary
        // (uncancelled) completion path, because only an explicit
        // `unregister_request_cancel` call removed them and success paths
        // never made one. `claim_terminal_once` now releases the socket
        // itself on the winning call.
        let runtime = cancel_runtime();
        let (_guard, _rx) = runtime.register_request_cancel("solo-2");
        assert!(runtime.claim_terminal_once("solo-2", "success"));
        // The socket is gone: cancelling it now is a silent no-op, not a
        // signal delivered to a receiver nobody is holding anymore.
        runtime.cancel_request("solo-2");
        assert!(runtime.is_request_cancelled("solo-2"));
    }

    #[tokio::test(start_paused = true)]
    async fn stale_parent_bindings_do_not_fan_out() {
        let runtime = cancel_runtime();
        let (parent_guard, _parent_rx) = runtime.register_request_cancel("parent-1");
        let (child_guard, _child_rx) = runtime.register_request_cancel("child-1");
        runtime.bind_child_to_parent("parent-1", "child-1");

        tokio::time::sleep(SUBAGENT_LINK_WINDOW).await;
        // Any registry access prunes stale entries first.
        let (_probe_guard, _probe_rx) = runtime.register_request_cancel("probe");
        let fanned = runtime.cancel_request_tree("parent-1");
        assert!(
            fanned.is_empty(),
            "expired parent must not fan out: {fanned:?}"
        );
        drop(parent_guard);
        drop(child_guard);
    }

    #[tokio::test(start_paused = true)]
    async fn registry_returns_to_baseline_after_many_normal_turns() {
        // P0 regression: simulate a burst of ordinary (uncancelled,
        // undisconnected) turns and assert the registry does not grow
        // without bound -- each turn's socket and terminal entry must be
        // released/expired rather than accumulating forever.
        let runtime = cancel_runtime();
        for index in 0..10_000 {
            let id = format!("burst-{index}");
            let (guard, _rx) = runtime.register_request_cancel(&id);
            assert!(runtime.claim_terminal_once(&id, "success"));
            drop(guard);
        }
        {
            let registry = runtime
                .parent_cancel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert_eq!(
                registry.sockets.len(),
                0,
                "every claimed socket must be released on the winning terminal write"
            );
        }

        tokio::time::sleep(TERMINAL_DEDUPE_TTL + std::time::Duration::from_secs(1)).await;
        // One more registry touch to trigger a prune pass.
        let (guard, _rx) = runtime.register_request_cancel("burst-final");
        drop(guard);
        let terminal_len = {
            let registry = runtime
                .parent_cancel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            registry.terminal.len()
        };
        assert!(
            terminal_len < 10_000,
            "terminal dedupe entries must expire after their TTL, not accumulate forever: {terminal_len}"
        );
    }

    fn chat_ok_body() -> Vec<u8> {
        br#"{
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "continued"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }"#
        .to_vec()
    }

    fn chat_route() -> RuntimeModelRoute {
        let mut route = sample_route(None);
        route.auth_kind = RuntimeAuthKind::None;
        route.wire = RuntimeWireFormat::Chat;
        route
    }

    fn seed_history(store: &dyn HistoryStore) {
        store
            .record_exchange(
                &json!({
                    "prompt_cache_key": "thread-repair",
                    "input": [{"role": "user", "content": "Implement the parser"}]
                }),
                &json!({
                    "id": "resp_src",
                    "object": "response",
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "id": "msg_src",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "working"}]
                    }]
                }),
                "route-1",
            )
            .unwrap();
    }

    #[tokio::test]
    async fn client_local_compaction_rebuilds_from_durable_source() {
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: chat_ok_body(),
        }]));
        let history = Arc::new(MemoryHistoryStore::new());
        seed_history(history.as_ref());
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![chat_route()],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        )
        .with_history_store(history);
        runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "prompt_cache_key": "thread-repair",
                    "previous_response_id": "resp_src",
                    "input": [
                        {"type": "context_compaction", "id": "cc_1"},
                        {"role": "user", "content": "DEGENERATE client summary"},
                        {"role": "user", "content": "continue after compact"}
                    ]
                })),
                "test_exec",
            )
            .await
            .unwrap();
        let recorded = transport.requests.lock().unwrap();
        let body: Value = serde_json::from_slice(&recorded[0].body).unwrap();
        let serialized = body.to_string();
        assert!(
            serialized.contains("DEGENERATE client summary"),
            "unverified following user turns must not be deleted: {serialized}"
        );
        assert!(
            serialized.contains("continue after compact"),
            "{serialized}"
        );
        assert!(
            serialized.contains("Conversation checkpoint")
                || serialized.contains("Implement the parser"),
            "canonical rebuild or original goal must reach upstream: {serialized}"
        );
    }

    #[tokio::test]
    async fn client_local_compaction_keeps_ordinary_developer_after_marker() {
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: chat_ok_body(),
        }]));
        let history = Arc::new(MemoryHistoryStore::new());
        seed_history(history.as_ref());
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![chat_route()],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        )
        .with_history_store(history);
        runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "previous_response_id": "resp_src",
                    "input": [
                        {"type": "context_compaction", "id": "cc_1"},
                        {"role": "developer", "content": "keep this developer note"},
                        {"role": "user", "content": "next step"}
                    ]
                })),
                "test_exec",
            )
            .await
            .unwrap();
        let recorded = transport.requests.lock().unwrap();
        let body: Value = serde_json::from_slice(&recorded[0].body).unwrap();
        let serialized = body.to_string();
        assert!(
            serialized.contains("keep this developer note"),
            "{serialized}"
        );
        assert!(serialized.contains("next step"), "{serialized}");
    }

    #[tokio::test]
    async fn client_local_compaction_without_identity_fails_closed() {
        let mut route = chat_route();
        route.auth_kind = RuntimeAuthKind::None;
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            Arc::new(RecordingTransport::new(vec![UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: chat_ok_body(),
            }])),
        );
        let error = runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "input": [
                        {"type": "context_compaction", "id": "cc_1"},
                        {"role": "user", "content": "continue"}
                    ]
                })),
                "test_exec",
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, RuntimeError::Compaction(ref message) if message.contains("identity") || message.contains("durable")),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn official_compaction_item_is_not_rewritten_by_local_overlay() {
        let mut route = sample_route(None);
        route.provider_kind = RuntimeProviderKind::Official;
        route.auth_kind = RuntimeAuthKind::None;
        let transport = Arc::new(RecordingTransport::new(vec![scripted_ok_response()]));
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );
        runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "input": [{
                        "type": "compaction",
                        "id": "comp_openai_1",
                        "encrypted_content": "official-opaque-ciphertext"
                    }]
                })),
                "test_exec",
            )
            .await
            .unwrap();
        let recorded = transport.requests.lock().unwrap();
        let body: Value = serde_json::from_slice(&recorded[0].body).unwrap();
        assert_eq!(
            body["input"][0]["encrypted_content"],
            "official-opaque-ciphertext"
        );
        assert_eq!(body["input"][0]["type"], "compaction");
    }

    #[tokio::test]
    async fn file_history_restart_still_repairs_client_local_compaction() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.jsonl");
        {
            let store = crate::history::FileHistoryStore::open(&path).unwrap();
            seed_history(&store);
        }
        let store = Arc::new(crate::history::FileHistoryStore::open(&path).unwrap());
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: chat_ok_body(),
        }]));
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![chat_route()],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        )
        .with_history_store(store);
        runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "previous_response_id": "resp_src",
                    "input": [
                        {"type": "context_compaction", "id": "cc_1"},
                        {"role": "user", "content": "continue after restart"}
                    ]
                })),
                "test_exec",
            )
            .await
            .unwrap();
        let recorded = transport.requests.lock().unwrap();
        let body: Value = serde_json::from_slice(&recorded[0].body).unwrap();
        assert!(body.to_string().contains("continue after restart"));
    }

    #[tokio::test]
    async fn repeated_tools_and_long_tool_streaks_remain_callable() {
        // Real incidents: distinct successful patches shared an output, and
        // Omen Alpha made 25 different calls without an assistant message.
        for scenario in ["identical_calls", "different_patches", "long_streak"] {
            let transport = Arc::new(RecordingTransport::new(
                (0..3)
                    .map(|_| UpstreamResponse {
                        status: 200,
                        headers: Vec::new(),
                        body: chat_ok_body(),
                    })
                    .collect(),
            ));
            let runtime = ProxyRuntime::new(
                Arc::new(FixedCatalog {
                    routes: vec![chat_route()],
                }),
                Arc::new(MemoryCredentialProvider::new()),
                Arc::new(UnconfiguredOfficialAuthProvider),
                Arc::new(CountingRequestLifecycle::new()),
                transport.clone(),
            );
            let mut input = vec![json!({
                "type": "message", "role": "user", "content": "continue the task"
            })];
            let count = if scenario == "long_streak" { 25 } else { 3 };
            for i in 0..count {
                let id = format!("call_{i}");
                if scenario == "different_patches" {
                    input.push(json!({
                        "type": "custom_tool_call", "call_id": id,
                        "name": "apply_patch",
                        "input": format!("*** Begin Patch\n*** Update File: a.txt\n@@\n-old{i}\n+new{i}\n*** End Patch")
                    }));
                    input.push(json!({
                        "type": "custom_tool_call_output", "call_id": id,
                        "output": "Success. Updated the following files: M a.txt"
                    }));
                } else {
                    input.push(json!({
                        "type": "function_call", "call_id": id, "name": "get_status",
                        "arguments": if scenario == "long_streak" {
                            json!({"index": i}).to_string()
                        } else { "{}".to_string() }
                    }));
                    input.push(json!({
                        "type": "function_call_output", "call_id": id,
                        "output": "status: pending"
                    }));
                }
            }
            // Repeated requests also cover retries/resumption after a guard
            // would previously have consumed its finalization allowance.
            for attempt in 0..3 {
                runtime
                    .execute(
                        request(json!({
                            "model": "vlm-test", "conversation_id": scenario,
                            "input": input.clone(),
                            "tools": [{"type": "function", "name": "get_status",
                                "parameters": {"type": "object", "properties": {}}}]
                        })),
                        &format!("attempt_{attempt}"),
                    )
                    .await
                    .unwrap();
            }
            let recorded = transport.requests.lock().unwrap();
            assert_eq!(recorded.len(), 3, "{scenario}");
            for request in recorded.iter() {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                assert!(!body["tools"].as_array().unwrap().is_empty(), "{scenario}");
                assert_ne!(body["tool_choice"], json!("none"), "{scenario}");
                let text = body.to_string();
                assert!(
                    !text.contains("The last tool sequence repeated"),
                    "{scenario}"
                );
                assert!(
                    !text.contains("disabled all tools for this final response"),
                    "{scenario}"
                );
            }
        }
    }

    #[tokio::test]
    async fn canonical_v2_default_investigation_recovery_reaches_upstream_request() {
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: chat_ok_body(),
        }]));

        let mut route = chat_route();
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );

        let mut input = vec![json!({
            "type": "message",
            "role": "user",
            "content": "Locate NEEDLE in state.txt, then implement the requested change"
        })];
        let probes = [
            "rg NEEDLE state.txt",
            "rg -n NEEDLE state.txt",
            "Select-String -Path state.txt -Pattern NEEDLE",
            "Get-Content state.txt | Select-String NEEDLE",
            "rg --fixed-strings NEEDLE state.txt",
        ];
        for (index, command) in probes.iter().enumerate() {
            let call_id = format!("investigation_{index}");
            input.push(json!({
                "type": "function_call",
                "call_id": call_id,
                "name": "exec_command",
                "arguments": {"cmd": command}
            }));
            input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": "",
                "exit_code": 1
            }));
        }

        runtime
            .execute(
                request(json!({
                    "model": "vlm-test",
                    "conversation_id": "canonical_v3_eval_qualification",
                    "input": input
                })),
                "canonical_v3_eval_qualification",
            )
            .await
            .expect("V3 recovery request must continue to the provider");

        let recorded = transport.requests.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        let forwarded: Value = serde_json::from_slice(&recorded[0].body).unwrap();
        let forwarded_text = forwarded.to_string();
        assert!(
            forwarded_text.contains("already investigated the same unresolved question"),
            "the production request must include V3 recovery guidance: {forwarded_text}"
        );
        assert!(forwarded_text.contains("Next action must change"));
    }

    #[tokio::test]
    async fn standalone_compaction_survives_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history_v2.jsonl");

        let mut v2_route = chat_route();

        let mut input = vec![
            json!({"type": "message", "role": "user", "content": "Task description: build API"}),
        ];
        for i in 0..4 {
            input.push(json!({"type": "function_call", "call_id": format!("test_{i}"), "name": "run_test", "arguments": "{}"}));
            input.push(json!({"type": "function_call_output", "call_id": format!("test_{i}"), "output": format!("passed step {i}")}));
        }

        let compaction_id: String;

        // Session 1: Run standalone /responses/compact without previous_response_id
        {
            let store = Arc::new(crate::history::FileHistoryStore::open(&path).unwrap());
            let summarizer_response = json!({
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Task description: build API. Four run_test calls passed steps 0-3."
                    },
                    "finish_reason": "stop"
                }]
            });
            let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: serde_json::to_vec(&summarizer_response).unwrap(),
            }]));
            let runtime = ProxyRuntime::new(
                Arc::new(FixedCatalog {
                    routes: vec![v2_route.clone()],
                }),
                Arc::new(MemoryCredentialProvider::new()),
                Arc::new(UnconfiguredOfficialAuthProvider),
                Arc::new(CountingRequestLifecycle::new()),
                transport,
            )
            .with_history_store(store);

            let mut compact_req = request(json!({
                "model": "vlm-test",
                "input": input
            }));
            compact_req.endpoint = crate::request::RuntimeEndpoint::Compact;

            let response = runtime.execute_compact(compact_req).await.unwrap();
            let RuntimeResponse::Json(body) = response else {
                panic!("expected JSON")
            };
            let item = &body["output"][0];
            assert_eq!(item["type"], "compaction");
            compaction_id = item["id"].as_str().unwrap().to_string();
            assert!(compaction_id.starts_with("cmp_local_"));
        }

        // Session 2: Proxy restart from disk history; subsequent turn materializes compaction_id
        {
            let store = Arc::new(crate::history::FileHistoryStore::open(&path).unwrap());
            let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: chat_ok_body(),
            }]));
            let runtime = ProxyRuntime::new(
                Arc::new(FixedCatalog {
                    routes: vec![v2_route],
                }),
                Arc::new(MemoryCredentialProvider::new()),
                Arc::new(UnconfiguredOfficialAuthProvider),
                Arc::new(CountingRequestLifecycle::new()),
                transport.clone(),
            )
            .with_history_store(store);

            let next_req = request(json!({
                "model": "vlm-test",
                "input": [
                    {"type": "compaction", "id": compaction_id},
                    {"role": "user", "content": "Now add metrics endpoint"}
                ]
            }));

            let res = runtime.execute(next_req, "exec_restart_turn").await;
            assert!(
                res.is_ok(),
                "turn after restart must succeed: {:?}",
                res.err()
            );

            let recorded = transport.requests.lock().unwrap();
            let body: Value = serde_json::from_slice(&recorded[0].body).unwrap();
            // Must NOT contain opaque "compaction" item, but canonical summary and new user turn
            let text = body.to_string();
            assert!(!text.contains("\"type\":\"compaction\""));
            assert!(text.contains("Now add metrics endpoint"));
            assert!(text.contains("Task description: build API"));
        }
    }

    #[tokio::test]
    async fn conversationless_loop_state_does_not_cross_requests() {
        let transport = Arc::new(RecordingTransport::new(vec![
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: chat_ok_body(),
            },
            UpstreamResponse {
                status: 200,
                headers: Vec::new(),
                body: chat_ok_body(),
            },
        ]));

        let mut r = chat_route();

        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog { routes: vec![r] }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport.clone(),
        );

        let mut input_loop =
            vec![json!({"type": "message", "role": "user", "content": "solve task"})];
        for i in 0..3 {
            input_loop.push(json!({"type": "function_call", "call_id": format!("c_{i}"), "name": "poll", "arguments": "{}"}));
            input_loop.push(json!({"type": "function_call_output", "call_id": format!("c_{i}"), "output": "status: pending"}));
        }

        // Request 1 without conversation_id
        let req1 = request(json!({
            "model": "vlm-test",
            "input": input_loop.clone()
        }));

        let res1 = runtime.execute(req1, "exec_stateless_1").await;
        assert!(
            res1.is_ok(),
            "first stateless request must recover: {:?}",
            res1.err()
        );

        // Request 2 without conversation_id (separate stateless call)
        let req2 = request(json!({
            "model": "vlm-test",
            "input": input_loop
        }));

        let res2 = runtime.execute(req2, "exec_stateless_2").await;
        assert!(res2.is_ok(), "second independent stateless request must not be aborted by first request's loop state: {:?}", res2.err());
    }

    #[tokio::test]
    async fn compact_store_identity_requires_actual_durable_source() {
        let mut v2_route = chat_route();

        // This route is Chat-wire, so the summarizer response must be
        // Chat-shaped. Codex 0.150 asks for prose and uses the last assistant
        // message verbatim as the summary body.
        let summarizer_response = json!({
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "User goal. Next: continue."},
                "finish_reason": "stop"
            }]
        });
        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&summarizer_response).unwrap(),
        }]));

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history_compact_fallback.jsonl");
        let store = Arc::new(crate::history::FileHistoryStore::open(&path).unwrap());
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![v2_route],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport,
        )
        .with_history_store(store);

        // Request has previous_response_id that does NOT exist in store
        let mut compact_req = request(json!({
            "model": "vlm-test",
            "previous_response_id": "non_existent_prev_id",
            "input": [
                {"type": "message", "role": "user", "content": "User prompt"}
            ]
        }));
        compact_req.endpoint = crate::request::RuntimeEndpoint::Compact;

        let res = runtime.execute_compact(compact_req).await;
        assert!(res.is_ok(), "compaction with raw input fallback succeeds");
        let response = res.unwrap();
        let RuntimeResponse::Json(outcome_body) = response else {
            panic!("expected JSON")
        };
        let installed = outcome_body
            .get("output")
            .and_then(Value::as_array)
            .unwrap();
        assert!(!installed.is_empty());
    }

    #[tokio::test]
    async fn previous_response_only_repeated_tools_keep_running() {
        let mut v2_route = chat_route();

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history_loop_stateful.jsonl");
        let store = Arc::new(crate::history::FileHistoryStore::open(&path).unwrap());

        let initial_items =
            vec![json!({"type": "message", "role": "user", "content": "test loop"})];

        store
            .record_exchange(
                &json!({
                    "model": "vlm-test",
                    "conversation_id": "conv-stateful-123",
                    "input": initial_items
                }),
                &json!({
                    "id": "resp_turn_0",
                    "object": "response",
                    "status": "completed",
                    "output": []
                }),
                &v2_route.route_id,
            )
            .unwrap();

        let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
            status: 200,
            headers: Vec::new(),
            body: chat_ok_body(),
        }]));

        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![v2_route.clone()],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            transport,
        )
        .with_history_store(store.clone());

        let loop_items_turn1 = vec![
            json!({"type": "function_call", "call_id": "c1", "name": "poll", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": "status: pending"}),
            json!({"type": "function_call", "call_id": "c2", "name": "poll", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "c2", "output": "status: pending"}),
            json!({"type": "function_call", "call_id": "c3", "name": "poll", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "c3", "output": "status: pending"}),
        ];

        // First turn referencing previous_response_id (without explicit conversation_id)
        // Replayed repeated calls remain valid continuation input.
        let req1 = request(json!({
            "model": "vlm-test",
            "previous_response_id": "resp_turn_0",
            "input": loop_items_turn1.clone()
        }));

        let res1 = runtime.execute(req1, "exec_turn_1").await;
        assert!(
            res1.is_ok(),
            "first continuation succeeds: {:?}",
            res1.err()
        );

        // Record turn 1 in history
        store
            .record_exchange(
                &json!({
                    "model": "vlm-test",
                    "previous_response_id": "resp_turn_0",
                    "input": loop_items_turn1
                }),
                &json!({
                    "id": "resp_turn_1",
                    "object": "response",
                    "status": "completed",
                    "output": []
                }),
                &v2_route.route_id,
            )
            .unwrap();

        let loop_items_turn2 = vec![
            json!({"type": "function_call", "call_id": "c4", "name": "poll", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "c4", "output": "status: pending"}),
            json!({"type": "function_call", "call_id": "c5", "name": "poll", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "c5", "output": "status: pending"}),
            json!({"type": "function_call", "call_id": "c6", "name": "poll", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "c6", "output": "status: pending"}),
        ];

        // Subsequent turns must not accumulate loop strikes across history.
        let req2 = request(json!({
            "model": "vlm-test",
            "previous_response_id": "resp_turn_1",
            "input": loop_items_turn2
        }));

        let res2 = runtime.execute(req2, "exec_turn_2").await;
        assert!(res2.is_ok(), "second continuation succeeds");

        let req3 = request(json!({
            "model": "vlm-test",
            "previous_response_id": "resp_turn_1",
            "input": loop_items_turn2.clone()
        }));
        let res3 = runtime.execute(req3, "exec_turn_3").await;
        assert!(res3.is_ok(), "replayed continuation must remain callable");
    }

    #[tokio::test]
    async fn same_parent_thread_two_active_turns_cancel_isolated() {
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog { routes: Vec::new() }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            Arc::new(RecordingTransport::new(Vec::new())),
        )
        .with_subagent_identity_mode(SubagentIdentityMode::OfficialPreferred);

        let s_id = "0195328e-8765-7123-9876-0123456789ab";
        let p_id = "th_parent";
        let t1_id = "turn_1";
        let t2_id = "turn_2";

        // Register parent turn 1 (E1)
        let (_sock_p1, rx_p1) = runtime.register_request_cancel("E1");
        let parent_t1_body = json!({
            "client_metadata": {
                "session_id": s_id,
                "thread_id": p_id,
                "turn_id": t1_id,
                "x-codex-turn-metadata": format!("{{\"session_id\":\"{s_id}\",\"thread_id\":\"{p_id}\",\"turn_id\":\"{t1_id}\"}}")
            },
            "model": "parent-model"
        });
        let mut req_p1 = request(parent_t1_body);
        req_p1.metadata.codex_identity = crate::codex_metadata::extract_codex_turn_identity(
            crate::codex_metadata::CodexMetadataSources::new(
                &axum::http::HeaderMap::new(),
                &req_p1.body,
                req_p1.endpoint,
            ),
        )
        .ok();
        runtime.observe_codex_turn(&req_p1, "E1", "r1", "m1");

        // Register parent turn 2 (E2)
        let (_sock_p2, rx_p2) = runtime.register_request_cancel("E2");
        let parent_t2_body = json!({
            "client_metadata": {
                "session_id": s_id,
                "thread_id": p_id,
                "turn_id": t2_id,
                "x-codex-turn-metadata": format!("{{\"session_id\":\"{s_id}\",\"thread_id\":\"{p_id}\",\"turn_id\":\"{t2_id}\"}}")
            },
            "model": "parent-model"
        });
        let mut req_p2 = request(parent_t2_body);
        req_p2.metadata.codex_identity = crate::codex_metadata::extract_codex_turn_identity(
            crate::codex_metadata::CodexMetadataSources::new(
                &axum::http::HeaderMap::new(),
                &req_p2.body,
                req_p2.endpoint,
            ),
        )
        .ok();
        runtime.observe_codex_turn(&req_p2, "E2", "r1", "m1");

        // Register child 1 (EC1) spawned from Parent Turn 1
        let (_sock_c1, rx_c1) = runtime.register_request_cancel("EC1");
        let child_1_body = json!({
            "client_metadata": {
                "session_id": s_id,
                "thread_id": "th_child_1",
                "turn_id": "c_turn_1",
                "parent_thread_id": p_id,
                "x-codex-turn-metadata": format!("{{\"session_id\":\"{s_id}\",\"thread_id\":\"th_child_1\",\"turn_id\":\"c_turn_1\",\"parent_thread_id\":\"{p_id}\",\"parent_turn_id\":\"{t1_id}\"}}")
            },
            "model": "child-model"
        });
        let mut req_c1 = request(child_1_body.clone());
        req_c1.metadata.codex_identity = crate::codex_metadata::extract_codex_turn_identity(
            crate::codex_metadata::CodexMetadataSources::new(
                &axum::http::HeaderMap::new(),
                &req_c1.body,
                req_c1.endpoint,
            ),
        )
        .ok();
        runtime.record_subagent_child_turn(
            &req_c1,
            "r1",
            "child-model",
            "child-model",
            &child_1_body,
            "EC1",
        );

        // Register child 2 (EC2) spawned from Parent Turn 2
        let (_sock_c2, rx_c2) = runtime.register_request_cancel("EC2");
        let child_2_body = json!({
            "client_metadata": {
                "session_id": s_id,
                "thread_id": "th_child_2",
                "turn_id": "c_turn_2",
                "parent_thread_id": p_id,
                "x-codex-turn-metadata": format!("{{\"session_id\":\"{s_id}\",\"thread_id\":\"th_child_2\",\"turn_id\":\"c_turn_2\",\"parent_thread_id\":\"{p_id}\",\"parent_turn_id\":\"{t2_id}\"}}")
            },
            "model": "child-model"
        });
        let mut req_c2 = request(child_2_body.clone());
        req_c2.metadata.codex_identity = crate::codex_metadata::extract_codex_turn_identity(
            crate::codex_metadata::CodexMetadataSources::new(
                &axum::http::HeaderMap::new(),
                &req_c2.body,
                req_c2.endpoint,
            ),
        )
        .ok();
        runtime.record_subagent_child_turn(
            &req_c2,
            "r1",
            "child-model",
            "child-model",
            &child_2_body,
            "EC2",
        );

        // Now cancel Parent Turn 1 (E1)
        let cancelled = runtime.cancel_request_tree("E1");
        assert_eq!(cancelled, vec!["EC1".to_string()]);

        // E1 and EC1 must be cancelled
        assert!(*rx_p1.borrow());
        assert!(*rx_c1.borrow());

        // E2 and EC2 must NOT be cancelled (isolated turns on same parent thread)
        assert!(!*rx_p2.borrow());
        assert!(!*rx_c2.borrow());
    }

    struct FailingJournalHistoryStore;
    impl crate::history::HistoryStore for FailingJournalHistoryStore {
        fn record_exchange(
            &self,
            _request: &Value,
            _response: &Value,
            _route_id: &str,
        ) -> Result<bool, String> {
            Ok(true)
        }
        fn get_chain(&self, _head: &str) -> Result<Vec<crate::history::HistoryEntry>, String> {
            Ok(Vec::new())
        }
        fn record_compaction(
            &self,
            _resp_id: &str,
            _cmp_id: &str,
            _gen: u32,
            _items: Vec<Value>,
        ) -> Result<(), String> {
            Err("simulated journal disk failure".into())
        }
        fn record_compaction_record(
            &self,
            _record: crate::history::CompactionJournalRecord,
        ) -> Result<(), String> {
            Err("simulated journal disk failure".into())
        }
    }

    #[test]
    fn ordinary_turn_usage_is_visible_through_resolved_conversation_identity() {
        let usage = Arc::new(MemoryUsageStore::new());
        let runtime = ProxyRuntime::new(
            Arc::new(FixedCatalog {
                routes: vec![chat_route()],
            }),
            Arc::new(MemoryCredentialProvider::new()),
            Arc::new(UnconfiguredOfficialAuthProvider),
            Arc::new(CountingRequestLifecycle::new()),
            Arc::new(RecordingTransport::new(Vec::new())),
        )
        .with_usage_store(Arc::clone(&usage) as Arc<dyn UsageStore>);
        let req = request(json!({
            "model": "vlm-test",
            "input": [{"type": "message", "role": "user", "content": "continue"}]
        }));
        assert!(
            conversation_key_from_request(&req.body).is_none(),
            "regression requires an ordinary body with no provider conversation key"
        );
        let resolved = runtime.resolve_request_conversation_key(&req).key;
        usage
            .record(&UsageRecord {
                route_id: "route-chat".into(),
                provider: "test".into(),
                model: "upstream-chat".into(),
                input_tokens: 52_000,
                output_tokens: 900,
                status: 200,
                outcome: Some("success".into()),
                conversation_identity: Some(resolved.clone()),
                ..Default::default()
            })
            .unwrap();

        let completed = runtime.completed_request_usages_for_conversation(&resolved);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].input_tokens, 52_000);
        assert_eq!(completed[0].output_tokens, 900);
    }

    #[tokio::test]
    async fn test_official_route_zero_prune() {
        let mut official_route = sample_route(None);
        official_route.provider_kind = RuntimeProviderKind::Official;
        official_route.wire = RuntimeWireFormat::Responses;
        assert_eq!(official_route.provider_kind, RuntimeProviderKind::Official);

        // Ordinary dispatch with an official route should never mutate or prune function_call_output
        let large_output = "Z".repeat(50_000);
        let items = vec![
            json!({"type": "message", "role": "user", "content": "read large file"}),
            json!({
                "type": "function_call",
                "call_id": "call_official",
                "name": "exec_command",
                "arguments": r#"{"command":"cat data.bin"}"#
            }),
            json!({
                "type": "function_call_output",
                "call_id": "call_official",
                "output": large_output
            }),
        ];

        // An ordinary request payload
        let body = json!({
            "model": "gpt-4o",
            "input": items
        });

        // Verify that estimate_request_tokens sees the full size and no pruning alters body
        let tokens = crate::tool_result_pruner::estimate_request_tokens(&body);
        assert!(tokens > 10_000);
        let out_str = body["input"][2]["output"].as_str().unwrap();
        assert_eq!(out_str.len(), 50_000);
    }

    /// Drives the exact production helper (`apply_active_execution_overlay`) that
    /// `execute` calls for every ordinary provider request, across a hydrated
    /// body that already carries the prior turn's overlay AND the durable
    /// `synthetic:task_efficiency_recovery:` anchor.
    #[test]
    fn t6_every_ordinary_request_sees_exactly_one_overlay() {
        let active = crate::task_efficiency::ActiveExecutionRecovery {
            recovery_count: 1,
            level: crate::task_efficiency::ActionRecoveryLevel::ConvertEvidence,
            started_request_index: Some(11),
            started_input_tokens: 50_000,
            marker_source_index: Some(5),
            post_recovery_tool_results: 1,
            post_recovery_input_tokens: 5_000,
            exploration_ops: 1,
            validation_ops: 0,
            mutation_ops: 0,
            compactions_since_activation: 0,
        };

        // Turn 11 hydrates with the durable anchor (V1 prose) still in history.
        let mut input: Vec<Value> = vec![
            json!({"type": "message", "role": "user", "content": "hello"}),
            json!({
                "type": "message",
                "role": "developer",
                "id": "synthetic:task_efficiency_recovery:1",
                "content": crate::task_efficiency::action_conversion_guidance_message(),
            }),
        ];

        for req_i in 11..=13 {
            crate::task_efficiency::apply_active_execution_overlay(&mut input, Some(&active));

            let control_blocks: Vec<&str> = input
                .iter()
                .filter_map(|item| item.get("id").and_then(Value::as_str))
                .filter(|id| {
                    id.starts_with("synthetic:active_execution_recovery:")
                        || id.starts_with("synthetic:task_efficiency_recovery:")
                })
                .collect();
            assert_eq!(
                control_blocks,
                vec!["synthetic:active_execution_recovery:1:convert_evidence"],
                "request #{req_i}: model must see exactly one execution-control block \
                 (the overlay), never the durable anchor"
            );
        }
    }

    /// The overlay helper is idempotent: repeated calls never accumulate.
    #[test]
    fn t7_overlay_not_accumulated_in_dispatched_body() {
        let active = crate::task_efficiency::ActiveExecutionRecovery {
            recovery_count: 2,
            level: crate::task_efficiency::ActionRecoveryLevel::ActionRequired,
            started_request_index: Some(11),
            started_input_tokens: 50_000,
            marker_source_index: None,
            post_recovery_tool_results: 4,
            post_recovery_input_tokens: 26_000,
            exploration_ops: 4,
            validation_ops: 0,
            mutation_ops: 0,
            compactions_since_activation: 0,
        };
        let mut input: Vec<Value> =
            vec![json!({"type": "message", "role": "user", "content": "turn 1"})];

        for _ in 0..5 {
            crate::task_efficiency::apply_active_execution_overlay(&mut input, Some(&active));
        }
        let overlays = input
            .iter()
            .filter(|item| {
                item.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .starts_with("synthetic:active_execution_recovery:")
            })
            .count();
        assert_eq!(
            overlays, 1,
            "dispatched body must carry exactly one overlay"
        );
    }

    /// The overlay item the production helper emits carries no runtime counters.
    #[test]
    fn t8_overlay_item_never_carries_internal_counters() {
        let active = crate::task_efficiency::ActiveExecutionRecovery {
            recovery_count: 1,
            level: crate::task_efficiency::ActionRecoveryLevel::ConvertEvidence,
            started_request_index: Some(11),
            started_input_tokens: 61_103,
            marker_source_index: Some(5),
            post_recovery_tool_results: 8,
            post_recovery_input_tokens: 40_000,
            exploration_ops: 8,
            validation_ops: 0,
            mutation_ops: 0,
            compactions_since_activation: 2,
        };
        let mut input: Vec<Value> = Vec::new();
        crate::task_efficiency::apply_active_execution_overlay(&mut input, Some(&active));

        let overlay = input
            .iter()
            .find(|item| {
                item.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .starts_with("synthetic:active_execution_recovery:")
            })
            .expect("overlay present");
        let obj = overlay.as_object().unwrap();
        assert_eq!(
            obj.keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            ["content", "id", "role", "type"]
                .into_iter()
                .map(String::from)
                .collect::<std::collections::BTreeSet<_>>()
        );
        let rendered = serde_json::to_string(overlay).unwrap();
        for needle in [
            "61",
            "103",
            "40000",
            "40_000",
            "post_recovery",
            "started_input",
        ] {
            assert!(
                !rendered.contains(needle),
                "overlay leaked internal counter fragment `{needle}`: {rendered}"
            );
        }
    }

    #[test]
    /// Reproduces the `20260827` live regression: request #11 activates recovery
    /// through the real live gate; the streaming client begins request #12 before
    /// the marker/checkpoint is durable, so its rebuilt reducer state is empty.
    /// The reconciler must restore the active recovery AND the conversion record
    /// AND the activation request index from the live authority (plan §29 P1).
    fn p1_real_request_rebuild_preserves_active_recovery_and_conversions() {
        let policy = crate::task_efficiency::TaskEfficiencyPolicy::recover();
        let mut live = crate::task_efficiency::LiveEfficiencyRecoveryState::default();

        // Request #11: rebuilt reducer state reflects a research-sprawl epoch.
        let mut eff_11 = crate::task_efficiency::TaskEfficiencyState::default();
        eff_11.task_revision = 1;
        eff_11.cumulative_input_tokens = 61_000;
        eff_11.input_tokens_since_world_change = 61_000;
        eff_11.tool_results_since_world_change = 6;
        eff_11.recent_exploration_ops = 6;
        let action_11 = crate::task_efficiency::decide_research_sprawl_recovery(&eff_11, &policy);
        assert!(action_11.is_some(), "request #11 is a sprawl candidate");
        let claimed = crate::task_efficiency::reconcile_live_efficiency_recovery(
            &mut live,
            &mut eff_11,
            action_11,
            Some(11),
        )
        .expect("request #11 claims recovery");
        assert_eq!(claimed.recovery_count, 1);
        assert_eq!(eff_11.recovery_conversions.len(), 1);
        assert_eq!(
            eff_11
                .active_action_recovery
                .as_ref()
                .unwrap()
                .started_request_index,
            Some(11)
        );
        assert_eq!(
            eff_11.recovery_conversions[0].recovery_request_index,
            Some(11)
        );

        // Request #12: the marker never became durable, so this rebuild is blank.
        let mut eff_12 = crate::task_efficiency::TaskEfficiencyState::default();
        eff_12.task_revision = 1;
        assert!(eff_12.active_action_recovery.is_none());
        assert!(eff_12.recovery_conversions.is_empty());

        let action_12 = crate::task_efficiency::decide_research_sprawl_recovery(&eff_12, &policy);
        let reinjected = crate::task_efficiency::reconcile_live_efficiency_recovery(
            &mut live,
            &mut eff_12,
            action_12,
            Some(12),
        );
        assert!(reinjected.is_none(), "no second recovery in the same epoch");
        let active = eff_12
            .active_action_recovery
            .as_ref()
            .expect("active recovery restored from live authority");
        assert_eq!(active.recovery_count, 1);
        assert_eq!(active.started_request_index, Some(11));
        assert_eq!(
            eff_12.recovery_conversions.len(),
            1,
            "conversion record must survive the rebuild (the live regression)"
        );
        assert_eq!(eff_12.recovery_conversions[0].recovery_count, 1);
        assert!(!eff_12.recovery_conversions[0].converted);
    }

    /// A live recovery must continue aging and must credit the first workspace
    /// mutation even when the rebuilt request has not reconstructed the
    /// durable marker. This is the exact shape observed in live eval
    /// `20260827-103903-51100`: the overlay remained visible, while all
    /// post-recovery counters stayed at zero and the successful apply_patch was
    /// reported as `converted=false`.
    #[test]
    fn p1_raced_rebuild_escalates_and_credits_workspace_conversion() {
        let policy = crate::task_efficiency::TaskEfficiencyPolicy::recover();
        let mut live = crate::task_efficiency::LiveEfficiencyRecoveryState::default();

        let mut activated = crate::task_efficiency::TaskEfficiencyState::default();
        activated.task_revision = 1;
        activated.cumulative_input_tokens = 50_000;
        activated.input_tokens_since_world_change = 50_000;
        activated.total_tool_results = 6;
        activated.tool_results_since_world_change = 6;
        activated.recent_exploration_ops = 6;
        let action = crate::task_efficiency::decide_research_sprawl_recovery(&activated, &policy);
        crate::task_efficiency::reconcile_live_efficiency_recovery(
            &mut live,
            &mut activated,
            action,
            Some(10),
        )
        .expect("recovery activation");

        // The next rebuild has cumulative usage but no durable marker/state.
        let mut raced = crate::task_efficiency::TaskEfficiencyState::default();
        raced.task_revision = 1;
        raced.cumulative_input_tokens = 76_000;
        raced.total_tool_results = 10;
        crate::task_efficiency::reconcile_live_efficiency_recovery(
            &mut live,
            &mut raced,
            None,
            Some(14),
        );
        assert_eq!(
            raced
                .active_action_recovery
                .as_ref()
                .expect("live recovery restored")
                .post_recovery_input_tokens,
            26_000,
            "token-age must not remain stuck at zero"
        );
        assert_eq!(
            raced
                .active_action_recovery
                .as_ref()
                .expect("live recovery restored")
                .post_recovery_tool_results,
            4,
            "tool-result age must not remain stuck at zero"
        );

        // A later rebuild sees the actual mutation but still lacks the marker.
        let mut mutated = crate::task_efficiency::TaskEfficiencyState::default();
        mutated.task_revision = 1;
        mutated.cumulative_input_tokens = 82_000;
        mutated.last_world_change = Some(crate::progress_fingerprint::ProgressFingerprint::new(
            crate::progress_fingerprint::ProgressKind::WorkspaceMutation,
            "apply_patch:continuity.py",
        ));
        mutated.first_workspace_mutation_request_index = Some(15);
        crate::task_efficiency::reconcile_live_efficiency_recovery(
            &mut live,
            &mut mutated,
            None,
            Some(15),
        );

        assert!(mutated.active_action_recovery.is_none());
        let conversion = mutated
            .recovery_conversions
            .iter()
            .find(|record| record.recovery_count == 1)
            .expect("conversion record retained");
        assert!(conversion.converted);
        assert_eq!(conversion.workspace_mutation_request_index, Some(15));
        assert_eq!(conversion.requests_to_workspace_mutation, Some(5));
        assert_eq!(conversion.tokens_to_workspace_mutation, Some(32_000));
        assert_eq!(conversion.mutation_ops, 1);
    }

    /// After a compaction checkpoint restores the active recovery, the first
    /// ordinary request rebuilds through the real `apply_active_execution_overlay`
    /// helper and the model still sees exactly one control block — even though the
    /// hydrated history carries the durable anchor (plan §12.3, §29 P2).
    #[test]
    fn p2_post_compaction_first_request_rematerializes_single_overlay() {
        // Checkpoint-carried active recovery (compaction bumped its counter only).
        let mut active = crate::task_efficiency::ActiveExecutionRecovery {
            recovery_count: 1,
            level: crate::task_efficiency::ActionRecoveryLevel::ConvertEvidence,
            started_request_index: Some(11),
            started_input_tokens: 61_000,
            marker_source_index: Some(5),
            post_recovery_tool_results: 2,
            post_recovery_input_tokens: 10_000,
            exploration_ops: 2,
            validation_ops: 0,
            mutation_ops: 0,
            compactions_since_activation: 0,
        };
        active.compactions_since_activation += 1;

        // Post-compaction hydrated body: canonical checkpoint + carried anchor.
        let mut input: Vec<Value> = vec![
            json!({"type": "vellum_canonical_checkpoint", "metadata": {"schema_version": 2}}),
            json!({"type": "message", "role": "user", "content": "resume"}),
            json!({
                "type": "message",
                "role": "developer",
                "id": "synthetic:task_efficiency_recovery:1",
                "content": "durable anchor prose",
            }),
        ];
        crate::task_efficiency::apply_active_execution_overlay(&mut input, Some(&active));

        let control: Vec<&str> = input
            .iter()
            .filter_map(|i| i.get("id").and_then(Value::as_str))
            .filter(|id| id.contains("recovery"))
            .collect();
        assert_eq!(
            control,
            vec!["synthetic:active_execution_recovery:1:convert_evidence"]
        );
    }

    /// L2 must not de-escalate when a raced rebuild reports L1: the reconciler
    /// merges the live L2 view over the reducer's L1 view (plan §12.3, §29 P3).
    #[test]
    fn p3_l2_does_not_deescalate_on_raced_rebuild() {
        let mut live = crate::task_efficiency::LiveEfficiencyRecoveryState {
            recovery_total: 1,
            active: true,
            active_task_revision: 1,
            active_world_change: None,
            started_total_tool_results: 0,
            active_action_recovery: Some(crate::task_efficiency::ActiveExecutionRecovery {
                recovery_count: 1,
                level: crate::task_efficiency::ActionRecoveryLevel::ActionRequired,
                started_request_index: Some(11),
                started_input_tokens: 61_000,
                marker_source_index: Some(5),
                post_recovery_tool_results: 4,
                post_recovery_input_tokens: 26_000,
                exploration_ops: 4,
                validation_ops: 0,
                mutation_ops: 0,
                compactions_since_activation: 1,
            }),
            recovery_conversions: vec![crate::task_efficiency::RecoveryConversionRecord {
                recovery_count: 1,
                recovery_request_index: Some(11),
                workspace_mutation_request_index: None,
                requests_to_workspace_mutation: None,
                tokens_to_workspace_mutation: None,
                validation_change_request_index: None,
                requests_to_validation_change: None,
                tokens_to_validation_change: None,
                exploration_ops: 4,
                validation_ops: 0,
                mutation_ops: 0,
                converted: false,
            }],
            active_task_closure: None,
        };

        // Raced rebuild only recovered an L1 view from a partial checkpoint.
        let mut raced = crate::task_efficiency::TaskEfficiencyState::default();
        raced.task_revision = 1;
        raced.epoch_recovery_level = 1;
        raced.research_sprawl_recovery_total = 1;
        raced.active_action_recovery = Some(crate::task_efficiency::ActiveExecutionRecovery {
            recovery_count: 1,
            level: crate::task_efficiency::ActionRecoveryLevel::ConvertEvidence,
            started_request_index: None,
            started_input_tokens: 0,
            marker_source_index: None,
            post_recovery_tool_results: 0,
            post_recovery_input_tokens: 0,
            exploration_ops: 0,
            validation_ops: 0,
            mutation_ops: 0,
            compactions_since_activation: 0,
        });

        crate::task_efficiency::reconcile_live_efficiency_recovery(
            &mut live,
            &mut raced,
            None,
            Some(20),
        );
        let active = raced.active_action_recovery.as_ref().unwrap();
        assert_eq!(
            active.level,
            crate::task_efficiency::ActionRecoveryLevel::ActionRequired,
            "L2 must not de-escalate to L1"
        );
        assert_eq!(active.started_request_index, Some(11));
        assert_eq!(active.compactions_since_activation, 1);
    }
}

/// Outcome of a single Codex Local Compact 0.150 summarizer attempt.
///
/// Every variant maps to a truthful classification on the compaction record;
/// none of them cause a silent switch to a different model or route.
#[derive(Debug)]
enum CodexLocalAttemptError {
    /// Provider rejected the request because the input exceeds its context
    /// window. Upstream responds by dropping the oldest input item and
    /// retrying rather than failing.
    ContextWindow,
    /// The summarizer produced no visible assistant text.
    EmptySummary,
    /// Transient transport/server failure worth a bounded retry.
    Retryable(RuntimeError),
    /// Everything else, surfaced to the caller unchanged.
    Fatal(RuntimeError),
}

/// Recognize a provider's "input is longer than my context window" rejection.
///
/// Codex has a typed `ContextWindowExceeded` error from a single provider.
/// Vellum spans several, so this matches on the status plus the bounded
/// provider diagnostic. It is deliberately conservative: a request that is
/// merely malformed must not be mistaken for a context-window rejection, or
/// the retreat loop would silently discard history to fix a bug it cannot fix.
fn is_context_window_error(status: u16, diagnostic: &str) -> bool {
    if !matches!(status, 400 | 413 | 422) {
        return false;
    }
    let haystack = diagnostic.to_ascii_lowercase();
    const SIGNALS: &[&str] = &[
        "context_length_exceeded",
        "context length exceeded",
        "maximum context length",
        "context window",
        "too many tokens",
        "reduce the length of the messages",
        "input is too long",
        "prompt is too long",
        "request too large",
    ];
    SIGNALS.iter().any(|signal| haystack.contains(signal))
}

#[cfg(test)]
mod codex_local_v0_150_transport_tests {
    use super::is_context_window_error;

    #[test]
    fn recognizes_provider_context_window_rejections() {
        assert!(is_context_window_error(
            400,
            "This model's maximum context length is 128000 tokens"
        ));
        assert!(is_context_window_error(400, "context_length_exceeded"));
        assert!(is_context_window_error(
            400,
            "Please reduce the length of the messages"
        ));
        assert!(is_context_window_error(413, "Request too large for gpt-4"));
        assert!(is_context_window_error(422, "prompt is too long"));
    }

    #[test]
    fn is_case_insensitive() {
        assert!(is_context_window_error(400, "CONTEXT WINDOW exceeded"));
    }

    #[test]
    fn does_not_mistake_other_four_hundreds_for_context_pressure() {
        // A malformed body must not send the retreat loop discarding history
        // to fix a bug that dropping history cannot fix.
        assert!(!is_context_window_error(400, "invalid value for 'model'"));
        assert!(!is_context_window_error(
            400,
            "unknown parameter: reasoning"
        ));
        assert!(!is_context_window_error(401, "invalid api key"));
        assert!(!is_context_window_error(429, "rate limit exceeded"));
        assert!(!is_context_window_error(500, "internal server error"));
    }

    #[test]
    fn quota_status_is_never_a_context_window_error() {
        // 429 carries quota semantics; treating it as context pressure would
        // silently shrink the user's history on a billing failure.
        assert!(!is_context_window_error(429, "maximum context length"));
    }
}

/// Apply the route's verified reasoning effort to an auxiliary (non-turn) body.
///
/// Codex Local Compact 0.150 sends its summarizer to the session's own model,
/// so the auxiliary body must carry the same Effort encoding an ordinary turn
/// would carry. The effort is applied only when the route advertises it as
/// verified, so a route whose probe never confirmed the value leaves the body
/// untouched rather than sending an encoding the deployment would reject.
fn apply_verified_reasoning_effort(body: &mut Value, route: &RuntimeModelRoute) {
    let Some(effort) = route
        .reasoning_capabilities
        .default_reasoning_effort
        .as_deref()
    else {
        return;
    };
    if !route
        .reasoning_capabilities
        .reasoning_efforts
        .iter()
        .any(|value| value == effort)
    {
        return;
    }
    crate::profile_adapter::apply_reasoning_effort_transport(
        body,
        effort,
        route.reasoning_capabilities.reasoning_effort_transport,
    );
}
