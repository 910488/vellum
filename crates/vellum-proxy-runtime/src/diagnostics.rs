use serde::{Deserialize, Serialize};

use crate::config::ProxyRuntimeIdentity;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeDiagnostics {
    pub ok: bool,
    pub ready: bool,
    pub identity: ProxyRuntimeIdentity,
    pub model_count: usize,
    pub listener_ready: bool,
    pub config_valid: bool,
    pub secrets_readable: bool,
    pub notes: Vec<String>,
}

impl RuntimeDiagnostics {
    pub fn summary(&self) -> String {
        format!(
            "ok={} ready={} models={} install_id={}",
            self.ok, self.ready, self.model_count, self.identity.install_id
        )
    }
}

/// How much content the runtime may capture before a diagnostic event leaves
/// the process. The decision is made *before* expensive capture work so a
/// sink never pays for content that is then discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DetailLevel {
    /// Compact summary: hashes, counts and bounded prefixes only. This is the
    /// safe default and is sufficient to explain prompt-cache reuse without
    /// storing conversation content.
    #[default]
    T1Summary,
    /// Full prompt text (instructions / spawn arguments), still no input
    /// bodies.
    T2FullPrompt,
    /// Full request/response bodies. Bounded in time and volume by the
    /// caller and never re-enabled automatically after a restart.
    T3FullContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionEngine {
    Canonical,
    GrokNative,
    LocalTrigger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionOutcome {
    Skipped,
    Compacted,
    Failed,
}

/// The inputs to a single auto-compaction decision, captured whether or not
/// compaction actually ran so a later reader can reproduce the same decision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDecision {
    pub engine: CompactionEngine,
    pub outcome: CompactionOutcome,
    pub window: u64,
    pub active_tokens: u64,
    pub pending_user_tokens: u64,
    pub pending_tool_tokens: u64,
    pub output_reserve: u64,
    pub tool_reserve: u64,
    pub threshold_percent: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items_before: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items_after: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_before: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_after: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_hash: Option<String>,
    /// The durable journal id Codex's opaque `compaction` item points at
    /// (`history.record_compaction`'s `compaction_id`). Only set for
    /// [`CompactionEngine::LocalTrigger`] — a real journaled checkpoint, not
    /// the pure threshold-preview engines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<String>,
    /// This checkpoint's position in the durable chain for its owning
    /// response id (1 for the first compaction on a conversation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSocketTransport {
    Official,
    HttpBridge,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebSocketOpened {
    pub connection_id: String,
    pub transport: WebSocketTransport,
    pub upstream_host: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebSocketClosed {
    pub connection_id: String,
    pub turns: u64,
    pub frames_in: u64,
    pub frames_out: u64,
    pub duration_ms: u64,
    pub reason: String,
}

/// A privacy-preserving Official account selection transition. Raw account
/// ids, email addresses and grants never enter the diagnostic event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OfficialAccountSelected {
    pub previous_account_hash: Option<String>,
    pub new_account_hash: String,
    pub selection_revision: u64,
    pub source: String,
    pub selection_verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SpawnRequested {
    pub parent_request_id: String,
    pub call_id: String,
    pub tool_name: String,
    /// The spawn tool arguments, capped at 8 KiB in T1. T2 stores the same
    /// object uncapped. This is the injected prompt at its source.
    pub arguments: serde_json::Value,
    pub arguments_len: u64,
    pub prompt_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentLinkMethod {
    OfficialThreadMetadata,
    OfficialSubagentActivity,
    CompatibilityHeader,
    CallId,
    ContentHash,
    ConfiguredModel,
    TimeWindow,
    #[default]
    None,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkConfidence {
    High,
    Medium,
    Low,
    #[default]
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InputItemManifest {
    pub role: String,
    pub hash: String,
    pub estimated_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChildTurn {
    pub request_id: String,
    pub route_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_request_id: Option<String>,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub link_method: SubagentLinkMethod,
    pub link_confidence: LinkConfidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions_len: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions_prefix: Option<String>,
    pub input_item_count: u64,
    pub input_manifest: Vec<InputItemManifest>,
    pub estimated_input_tokens: u64,
    /// T3 only: redacted full request body. Never enabled across a restart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_context: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_source: Option<crate::codex_metadata::CodexIdentitySource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_trust: Option<crate::codex_metadata::CodexIdentityTrust>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SpawnCompleted {
    pub call_id: String,
    /// Bounded tool output text (T1/T2); the hash is authoritative for
    /// equality and avoids storing unbounded provider payloads.
    pub output: String,
    pub output_len: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_hash: Option<String>,
    pub duration_ms: u64,
    /// The request id of the child turn this spawn was confidently linked to
    /// (`Some` only when [`record_subagent_child_turn`](crate::exec) resolved
    /// a single high/medium-confidence match at Point B). This is the join
    /// key an aggregation layer uses to look up the child's *own* recorded
    /// outcome (success / provider error / ...) instead of treating this
    /// event's mere existence as proof of success. `None` means the parent's
    /// tool call closed without Vellum ever confidently identifying which
    /// child request answered it (e.g. an ambiguous parallel spawn) — a
    /// consuming aggregator must not guess a link in that case.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_request_id: Option<String>,
}

/// A terminal usage write that lost rather than a coordinate win: the
/// first-writer-wins claim succeeded (so no duplicate terminator is possible),
/// but the durable [`UsageStore`](crate::usage::UsageStore) rejected the
/// record. The terminal claim is never rolled back on this path -- a lost
/// write must not resurrect the turn as if it had never ended -- but the
/// rejection has to be observable, otherwise usage silently disappears with
/// nothing to investigate. Fields are bounded and never carry secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWriteFailure {
    /// Internal cancel-registry key for the turn whose terminal row failed to
    /// persist. Never the external `request_id`; see
    /// [`crate::exec::new_execution_id`].
    pub execution_id: String,
    /// The terminal outcome that was claimed before the write failed.
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub route_id: String,
    /// The bounded store-provided rejection. Diagnostic text, not a payload
    /// that could contain credentials.
    pub error: String,
}

/// Diagnostic observation for trajectory analysis and loop guard decisions (spec 70).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TrajectoryDecisionDiagnostic {
    pub tool_exchange_count: u64,
    pub tool_only_streak: u64,
    pub repeated_pattern_count: u64,
    pub max_repeat_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern_hash: Option<String>,
    pub action: String,
}

/// Trigger origin for a canonical compaction attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTriggerOrigin {
    ClientCompactionRequest,
    InternalSoftThreshold,
    ContextExceededRecovery,
}

/// Client scheduler kind (fixed to unknown per protocol truth).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientSchedulerKind {
    Unknown,
}

/// Outcome of the quality gate evaluation for a compaction candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalQualityOutcome {
    Accepted,
    Rejected,
    NotEvaluated,
}

/// Structured reason why deterministic fallback was used instead of semantic extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionFallbackReason {
    SemanticRequestFailed,
    RepairRequestFailed,
    RepairedPayloadInvalid,
    EvidenceValidationFailed,
    PruneRelievedPressure,
}

/// Additive diagnostic event emitted once for every compaction attempt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalCompactionAttempt {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id_hash: Option<String>,
    pub source_hash: String,
    pub candidate_generation: u32,
    #[serde(default)]
    pub prior_checkpoint_used: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_checkpoint_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_hash: Option<String>,
    pub trigger_origin: CompactionTriggerOrigin,
    pub client_scheduler_kind: ClientSchedulerKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_breakdown: Option<crate::context_projection::CanonicalTokenBreakdown>,
    pub quality_outcome: CanonicalQualityOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quality_errors: Vec<String>,
    pub fallback_used: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<CompactionFallbackReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounded_failure_diagnostic: Option<String>,
}

/// One locally materialized compaction, as it actually happened.
///
/// This is the engine-neutral replacement for the retired Canonical attempt
/// diagnostic. It is the record an eval reads to confirm that the engine it
/// asked for is the engine that ran: `engine_id` and `engine_provenance` are
/// written from the resolved engine, never from the request, so a mismatch
/// between requested and observed engine is detectable rather than assumed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LocalCompactionAttempt {
    /// Resolved engine that produced this compaction, e.g.
    /// `codex_local_v0_150`.
    pub engine_id: String,
    /// Upstream provenance for that engine, where one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_provenance: Option<String>,
    /// Route and upstream model the summarizer actually ran on. Always the
    /// session's own model — a separate compactor is no longer representable.
    pub route_id: String,
    pub upstream_model: String,
    /// What caused the compaction to run.
    pub trigger_origin: CompactionTriggerOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id_hash: Option<String>,
    /// Hash of the sanitized source window that was summarized.
    pub source_hash: String,
    /// Hash of the installed replacement window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_hash: Option<String>,
    /// Lineage position within the conversation's compaction chain.
    pub generation: u32,
    pub items_before: u64,
    pub items_after: u64,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub elapsed_ms: u64,
    /// How many times the summarizer input had to shed its oldest item to fit
    /// the provider's context window.
    #[serde(default)]
    pub context_retreats: u32,
    /// Stable failure code when the attempt produced no replacement. `None`
    /// means the compaction succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounded_failure_diagnostic: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalQualityDiagnostic {
    pub schema_version: u32,
    pub source_tokens: u64,
    pub checkpoint_tokens: u64,
    pub compression_ratio_bps: u32,
    pub semantic_claims: u64,
    pub grounded_claims: u64,
    pub rejected_claims: u64,
    pub deterministic_coverage_bps: u32,
    pub repeated_exchanges_collapsed: u64,
    pub fallback_used: bool,
    pub prior_checkpoint_used: bool,
}

/// Diagnostic event emitted when incoming Codex metadata has conflicting critical fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexMetadataConflictDiagnostic {
    pub request_id: String,
    pub execution_id: String,
    pub fields: Vec<String>,
    pub structured_source_present: bool,
    pub compatibility_source_present: bool,
}

/// Diagnostic event emitted when an exact subagent parent-child edge is established in the graph.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubagentGraphLinked {
    pub parent_thread_id: String,
    pub child_thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_turn_id: Option<String>,
    pub method: SubagentLinkMethod,
    pub confidence: LinkConfidence,
}

/// Diagnostic event comparing official graph linking vs legacy heuristic linking in shadow mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubagentLinkComparison {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub official_parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy_parent_request_id: Option<String>,
    pub agree: bool,
    pub official_confidence: LinkConfidence,
    pub legacy_confidence: LinkConfidence,
}

/// Diagnostic event emitted when semantic validation fails on a compaction candidate delta.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SemanticValidationDiagnostic {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id_hash: Option<String>,
    pub candidate_generation: u32,
    pub invalid_evidence_refs: usize,
    pub rejected_claim_count: usize,
    pub errors: Vec<SemanticValidationFieldError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SemanticValidationFieldError {
    pub field: String,
    pub reason: String,
}

/// A structured diagnostic observation. The runtime never knows which backing
/// store receives it (sqlite, JSONL or nothing); it only calls the seam.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiagnosticEvent {
    CompactionDecision(CompactionDecision),
    TrajectoryDecision(TrajectoryDecisionDiagnostic),
    CanonicalQuality(CanonicalQualityDiagnostic),
    CanonicalCompactionAttempt(CanonicalCompactionAttempt),
    LocalCompactionAttempt(LocalCompactionAttempt),
    SemanticValidation(SemanticValidationDiagnostic),
    WebSocketOpened(WebSocketOpened),
    WebSocketClosed(WebSocketClosed),
    OfficialAccountSelected(OfficialAccountSelected),
    SpawnRequested(SpawnRequested),
    ChildTurn(ChildTurn),
    SpawnCompleted(SpawnCompleted),
    UsageWriteFailure(UsageWriteFailure),
    HarnessTranscript(crate::replay::HarnessTranscriptDiagnostics),
    CodexMetadataConflict(CodexMetadataConflictDiagnostic),
    SubagentGraphLinked(SubagentGraphLinked),
    SubagentLinkComparison(SubagentLinkComparison),
    InvestigationOperation(crate::investigation_diagnostics::InvestigationOperationDiagnostic),
    InvestigationLink(crate::investigation_diagnostics::InvestigationLinkDiagnostic),
    InvestigationTransition(crate::investigation_diagnostics::InvestigationTransitionDiagnostic),
    InvestigationRecovery(crate::investigation_diagnostics::InvestigationRecoveryDiagnostic),
    TaskStall(crate::task_stall::TaskStallDiagnostic),
    TaskStallRecovery(crate::task_stall::TaskStallRecoveryDiagnostic),
    TaskStallTerminal(crate::task_stall::TaskStallTerminalDiagnostic),
    TaskEfficiency(crate::task_efficiency::TaskEfficiencyDiagnostic),
    TaskEfficiencyRecovery(crate::task_efficiency::TaskEfficiencyRecoveryDiagnostic),
}

/// Fire-and-forget diagnostic seam. Implementations must never panic and must
/// never affect the outcome of a request (INV-D-001); any failure is silently
/// downgraded to a warning by the caller.
pub trait DiagnosticsSink: Send + Sync {
    fn record(&self, event: DiagnosticEvent);
    fn detail_level(&self) -> DetailLevel;
}

/// Stable short SHA-256 identity for diagnostic text. Only the first 16 bytes
/// are emitted; this is for equality, not cryptographic attribution.
pub fn hash_text(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(value.as_bytes());
    let prefix = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{prefix}")
}

/// The prompt-bearing field from a subagent spawn tool call. Codex variants
/// have used `instructions`, `message`, `task`, and `prompt`; prefer the most
/// explicit field and never concatenate unrelated metadata into the identity.
pub fn spawn_prompt(arguments: &serde_json::Value) -> Option<String> {
    ["instructions", "message", "task", "prompt"]
        .iter()
        .find_map(|key| {
            arguments
                .get(*key)
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(|value| value.trim().to_string())
        })
}

/// Extract the dominant user-visible text from a Responses input item. Used
/// only for content-hash linking; unknown structures still contribute a full
/// JSON hash through the manifest.
pub fn input_item_text(item: &serde_json::Value) -> Option<String> {
    if let Some(text) = item.get("text").and_then(serde_json::Value::as_str) {
        return Some(text.trim().to_string());
    }
    item.get("content").and_then(|content| {
        content
            .as_array()
            .and_then(|parts| {
                parts.iter().find_map(|part| {
                    part.get("text")
                        .and_then(serde_json::Value::as_str)
                        .filter(|text| !text.trim().is_empty())
                        .map(|text| text.trim().to_string())
                })
            })
            .or_else(|| content.as_str().map(|text| text.trim().to_string()))
    })
}

/// Build a content-only manifest for one request input item. The item body is
/// never retained in T1; role, hash, and token estimate are sufficient to see
/// where a shared prompt prefix diverges.
pub fn input_item_manifest(item: &serde_json::Value) -> (InputItemManifest, Option<String>) {
    let role = item
        .get("role")
        .and_then(serde_json::Value::as_str)
        .or_else(|| item.get("type").and_then(serde_json::Value::as_str))
        .unwrap_or("unknown")
        .to_string();
    let encoded = serde_json::to_string(item).unwrap_or_default();
    let text = input_item_text(item);
    let identity = text.as_deref().unwrap_or(encoded.as_str());
    let estimated_tokens = (identity.chars().count().div_ceil(4)).max(1) as u64;
    (
        InputItemManifest {
            role,
            hash: hash_text(identity),
            estimated_tokens,
        },
        text,
    )
}

/// Bound diagnostic text while preserving the original length separately.
pub fn bounded_text(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

/// T3 full-context capture is process-local. It cannot survive a restart
/// because the session is never persisted, and it cannot stay open longer
/// than this duration even inside a live process.
pub const T3_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Combined T3 payload budget for one process-local capture window.
pub const T3_MAX_BYTES: u64 = 200 * 1024 * 1024;

/// Whether a T3 window is still allowed to capture full request bodies.
pub fn t3_window_open(elapsed: std::time::Duration, captured_bytes: u64) -> bool {
    elapsed < T3_MAX_AGE && captured_bytes < T3_MAX_BYTES
}

/// Apply the shipped T3 bounds to a requested detail level.
///
/// `window_elapsed = None` is a restart (or a process that never opted in):
/// T3 is forgotten and the safe default is T1.
pub fn bounded_detail_level(
    requested: DetailLevel,
    window_elapsed: Option<std::time::Duration>,
    captured_bytes: u64,
) -> DetailLevel {
    if requested != DetailLevel::T3FullContext {
        return requested;
    }
    match window_elapsed {
        Some(elapsed) if t3_window_open(elapsed, captured_bytes) => DetailLevel::T3FullContext,
        _ => DetailLevel::T1Summary,
    }
}

/// Drop T3-only fields so a closed window cannot leak full bodies.
pub fn strip_t3_fields(event: &mut DiagnosticEvent) {
    if let DiagnosticEvent::ChildTurn(child) = event {
        child.full_context = None;
    }
}

/// Process-local T3 opt-in. Dropping this value (or restarting the process)
/// returns capture to T1; the window is never written to disk.
#[derive(Debug)]
pub struct T3CaptureSession {
    requested: DetailLevel,
    started: Option<std::time::Instant>,
    bytes: u64,
}

impl Default for T3CaptureSession {
    fn default() -> Self {
        Self {
            requested: DetailLevel::T1Summary,
            started: None,
            bytes: 0,
        }
    }
}

impl T3CaptureSession {
    pub fn enable_t3(&mut self) {
        self.requested = DetailLevel::T3FullContext;
        self.started = Some(std::time::Instant::now());
        self.bytes = 0;
    }

    pub fn record_bytes(&mut self, n: u64) {
        self.bytes = self.bytes.saturating_add(n);
    }

    pub fn detail_level(&self) -> DetailLevel {
        bounded_detail_level(
            self.requested,
            self.started.map(|started| started.elapsed()),
            self.bytes,
        )
    }
}

/// Default sink: captures nothing and reports the conservative detail level.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSink;

impl DiagnosticsSink for NoopSink {
    fn record(&self, _event: DiagnosticEvent) {}
    fn detail_level(&self) -> DetailLevel {
        DetailLevel::T1Summary
    }
}

/// Header keys that must never leave the process (INV-D-002).
pub const SENSITIVE_HEADER_KEYS: &[&str] = &[
    "authorization",
    "openai-account",
    "api_key",
    "api-key",
    "cookie",
    "set-cookie",
];

/// Provider key prefixes whose token-shaped values must be scrubbed from any
/// captured string. Keep this list bounded and conservative.
pub const PROVIDER_KEY_PREFIXES: &[&str] =
    &["sk-", "xai-", "gsk_", "sk_", "key-", "Bearer ", "bearer "];

/// Redact a single string: replace provider-key-shaped tokens with a fixed
/// marker. Intended for the few fields that can carry free-form credentials.
pub fn redact_sensitive_text(input: &str) -> String {
    if input.is_empty() {
        return input.to_string();
    }
    let mut output = input.to_string();
    for prefix in PROVIDER_KEY_PREFIXES {
        let marker = format!("{prefix}[REDACTED]");
        // Replace every occurrence of `prefix` + a run of non-whitespace, non-
        // punctuation token characters. This is deliberately conservative: it
        // only scrubs values that look like a provider key, not normal prose.
        output = redact_token_runs(&output, prefix, &marker);
    }
    output
}

fn redact_token_runs(input: &str, prefix: &str, marker: &str) -> String {
    let mut output = String::with_capacity(input.len() + 16);
    let mut rest = input;
    while let Some(index) = rest.find(prefix) {
        output.push_str(&rest[..index]);
        let after_prefix = &rest[index + prefix.len()..];
        let token_len = after_prefix
            .char_indices()
            .take_while(|(_, ch)| ch.is_ascii_alphanumeric() || matches!(*ch, '-' | '_' | '.'))
            .map(|(i, ch)| i + ch.len_utf8())
            .last()
            .unwrap_or(0);
        if token_len > 0 {
            output.push_str(marker);
            rest = &after_prefix[token_len..];
        } else {
            // The prefix is not followed by a token; keep it and move on.
            output.push_str(prefix);
            rest = after_prefix;
        }
    }
    output.push_str(rest);
    output
}

/// Recursively strip sensitive header keys from a JSON value and scrub
/// token-shaped values in every remaining string. Used before any event leaves
/// the runtime (T1/T2/T3 share this single entry point).
pub fn redact_sensitive_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut redacted = serde_json::Map::new();
            for (key, child) in map {
                if SENSITIVE_HEADER_KEYS
                    .iter()
                    .any(|sensitive| key.eq_ignore_ascii_case(sensitive))
                {
                    continue;
                }
                redacted.insert(key.clone(), redact_sensitive_json(child));
            }
            serde_json::Value::Object(redacted)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact_sensitive_json).collect())
        }
        serde_json::Value::String(text) => serde_json::Value::String(redact_sensitive_text(text)),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detail_level_defaults_to_t1_summary() {
        assert_eq!(DetailLevel::default(), DetailLevel::T1Summary);
    }

    #[test]
    fn redaction_strips_sensitive_headers_and_provider_keys() {
        let input = json!({
            "Authorization": "Bearer sk-1234567890abcdef",
            "Cookie": "session=secret",
            "api_key": "sk-live-abcdef",
            "keep": {"nested": "xai-abc123", "text": "hello world"}
        });
        let redacted = redact_sensitive_json(&input);
        assert!(redacted.get("Authorization").is_none());
        assert!(redacted.get("Cookie").is_none());
        assert!(redacted.get("api_key").is_none());
        assert_eq!(
            redacted["keep"]["nested"].as_str().unwrap(),
            "xai-[REDACTED]"
        );
        assert_eq!(redacted["keep"]["text"].as_str().unwrap(), "hello world");
    }

    #[test]
    fn redaction_leaves_prose_without_key_prefixes_untouched() {
        assert_eq!(
            redact_sensitive_text("discuss the sky briefly"),
            "discuss the sky briefly"
        );
        assert_eq!(redact_sensitive_text(""), "");
    }

    #[test]
    fn t3_bounds_close_the_window_on_age_volume_or_restart() {
        assert_eq!(
            bounded_detail_level(
                DetailLevel::T3FullContext,
                Some(T3_MAX_AGE - std::time::Duration::from_secs(1)),
                0,
            ),
            DetailLevel::T3FullContext
        );
        assert_eq!(
            bounded_detail_level(DetailLevel::T3FullContext, Some(T3_MAX_AGE), 0),
            DetailLevel::T1Summary
        );
        assert_eq!(
            bounded_detail_level(
                DetailLevel::T3FullContext,
                Some(std::time::Duration::from_secs(1)),
                T3_MAX_BYTES,
            ),
            DetailLevel::T1Summary
        );
        assert_eq!(
            bounded_detail_level(DetailLevel::T3FullContext, None, 0),
            DetailLevel::T1Summary
        );
        assert_eq!(
            bounded_detail_level(DetailLevel::T2FullPrompt, None, T3_MAX_BYTES),
            DetailLevel::T2FullPrompt
        );
        assert!(!t3_window_open(T3_MAX_AGE, 0));
        assert!(!t3_window_open(
            std::time::Duration::from_secs(1),
            T3_MAX_BYTES
        ));
        assert!(t3_window_open(
            T3_MAX_AGE - std::time::Duration::from_secs(1),
            T3_MAX_BYTES - 1,
        ));
    }

    #[test]
    fn t3_session_starts_closed_and_is_process_local() {
        let session = T3CaptureSession::default();
        assert_eq!(session.detail_level(), DetailLevel::T1Summary);
        let mut session = T3CaptureSession::default();
        session.enable_t3();
        assert_eq!(session.detail_level(), DetailLevel::T3FullContext);
        session.record_bytes(T3_MAX_BYTES);
        assert_eq!(session.detail_level(), DetailLevel::T1Summary);
    }
}
