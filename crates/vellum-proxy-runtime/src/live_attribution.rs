//! Fail-closed live-request attribution for Official / Grok / Qwen / OpenCode.
//!
//! Timing compare, redaction, Official body/header equality, the 50-turn Luna
//! budget, first-frame fail-closed, and the Official decision table are pure
//! so they can be unit-tested without a network. Live I/O lives in the Desktop
//! evaluator and must call these helpers; it must not re-implement them.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use fs2::FileExt;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::config::{build_commit, current_binary_sha256};
use crate::diagnostics::{hash_text, redact_sensitive_json, redact_sensitive_text};

/// Pinned Official model for every live Luna transport turn.
pub const PINNED_OFFICIAL_MODEL: &str = "gpt-5.6-luna";
/// Environment pin. Any other value is rejected before a request is sent.
pub const LIVE_OFFICIAL_MODEL_ENV: &str = "VELLUM_LIVE_OFFICIAL_MODEL";
/// Local hard cap on Official short responses (candidate + installed + reserved).
pub const OFFICIAL_LIVE_TURN_CAP: u32 = 200;
/// Minimum remaining Official quota (percent) required to start SWE-bench.
pub const LUNA_SWE_MIN_REMAINING_PERCENT: f64 = 30.0;
/// Extra Vellum delay is allowed up to `max(250 ms, direct TTFT × 20%)`.
pub const PROXY_EXTRA_DELAY_FLOOR_MS: u64 = 250;
pub const PROXY_EXTRA_DELAY_PERCENT: u64 = 20;
/// Official live WebSocket / HTTP first-frame deadline.
pub const OFFICIAL_FIRST_FRAME_DEADLINE_MS: u64 = 20_000;
/// A first delta that arrives this close to `completed` is an end-of-stream flush.
pub const END_FLUSH_WINDOW_MS: u64 = 50;

const FORBIDDEN_ARTIFACT_KEYS: &[&str] = &[
    "access_token",
    "account_id",
    "accountid",
    "chatgpt-account-id",
    "chatgpt_account_id",
    "email",
    "authorization",
    "token",
    "refresh_token",
    "api_key",
    "apikey",
    "secret",
    "cookie",
    "set-cookie",
    "raw_body",
    "raw_provider_body",
    "provider_body",
    "upstream_body",
];

/// How a live turn actually delivered its application deltas. Deliberately
/// distinct from [`LiveOutcome`] so "transport succeeded but the stream
/// flushed once at the end" is recorded instead of collapsed into one flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamQuality {
    /// The expected application delta arrived more than `END_FLUSH_WINDOW_MS`
    /// before the terminal event: a genuinely incremental stream.
    Incremental,
    /// Deltas arrived, but the first valid output delta is within
    /// `END_FLUSH_WINDOW_MS` of the terminal: a single end-of-stream flush.
    EndFlush,
    /// A legitimate non-streaming JSON response, or the route capability
    /// explicitly declared non-streaming.
    Buffered,
    /// The streaming terminal succeeded but produced no expected
    /// output/tool delta (or silently downgraded a stream to a JSON body).
    NoDelta,
}

impl StreamQuality {
    /// Stable wire/storage token for the classification.
    pub fn as_token(self) -> &'static str {
        match self {
            Self::Incremental => "incremental",
            Self::EndFlush => "end_flush",
            Self::Buffered => "buffered",
            Self::NoDelta => "no_delta",
        }
    }
}

/// Is `kind` an application *output* delta (text or tool arguments)? Text
/// tests judge only `response.output_text.delta`, tool tests only function
/// arguments deltas — never a reasoning channel.
pub fn is_output_delta(kind: &str) -> bool {
    kind.contains("output_text.delta") || kind.contains("function_call_arguments.delta")
}

/// Is `kind` a function-call argument delta (the streamed application output
/// of a tool call, as opposed to commentary text)?
pub fn is_tool_delta(kind: &str) -> bool {
    kind.contains("function_call_arguments.delta")
}

/// Is `kind` an explicit reasoning-channel delta? Recorded independently and
/// never qualifies a text stream.
pub fn is_reasoning_delta(kind: &str) -> bool {
    kind.contains("reasoning") && kind.ends_with(".delta")
}

/// Inputs to [`classify_stream_quality`]. `streamed` is whether the transport
/// actually produced an SSE application stream (`false` for a buffered JSON
/// response).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamQualityInput {
    /// The client requested `stream: true`.
    pub streaming_requested: bool,
    /// The route's capability advertises streaming.
    pub capability_streaming: bool,
    /// The transport produced an SSE application stream.
    pub streamed: bool,
    /// Application output deltas (`output_text.delta` / Chat `delta.content` /
    /// function-arguments deltas).
    pub output_delta_count: u64,
    /// Time of the first output delta, relative to the request start.
    pub first_output_delta_ms: Option<u64>,
    /// Function-call argument deltas, tracked independently of commentary text
    /// so a tool call is classified on its argument stream rather than on any
    /// non-deterministic preamble text.
    pub tool_delta_count: u64,
    /// Request-relative time of the first function-call argument delta.
    pub first_tool_delta_ms: Option<u64>,
    /// Terminal (`response.completed`) time, request-relative.
    pub terminal_ms: Option<u64>,
    /// Explicit reasoning-channel deltas (independent record only).
    pub reasoning_delta_count: u64,
    /// Time of the first reasoning delta, request-relative.
    pub first_reasoning_delta_ms: Option<u64>,
}

/// Pure four-way stream-quality classifier.
///
/// - `buffered`: a legitimate non-streaming JSON response, or the capability
///   explicitly declared non-streaming.
/// - `no_delta`: the stream ended successfully but produced no expected
///   output/tool delta.
/// - `incremental`: the first output delta arrived more than 50ms before the
///   terminal.
/// - `end_flush`: deltas existed, but only as a single flush within 50ms of
///   the terminal.
pub fn classify_stream_quality(input: &StreamQualityInput) -> StreamQuality {
    if !input.streamed {
        if !input.capability_streaming || !input.streaming_requested {
            return StreamQuality::Buffered;
        }
        return StreamQuality::NoDelta;
    }
    // A tool call's stream quality is its function-argument deltas, not any
    // commentary text the model may emit before the call. Commentary is
    // non-deterministic and would otherwise let a single end-of-stream argument
    // flush masquerade as `incremental` (or vice versa).
    let (delta_count, first_delta_ms) = if input.tool_delta_count > 0 {
        (input.tool_delta_count, input.first_tool_delta_ms)
    } else {
        (input.output_delta_count, input.first_output_delta_ms)
    };
    if delta_count == 0 {
        return StreamQuality::NoDelta;
    }
    match (first_delta_ms, input.terminal_ms) {
        (Some(first), Some(terminal))
            if first < terminal && terminal.saturating_sub(first) > END_FLUSH_WINDOW_MS =>
        {
            StreamQuality::Incremental
        }
        _ => StreamQuality::EndFlush,
    }
}

/// Streaming qualification gate. Only `incremental` passes for every wire. A
/// third-party Chat turn may keep its transport `outcome=ok` on a legitimate
/// `end_flush`, but it still fails qualification here.
pub fn streaming_qualifies(quality: StreamQuality) -> bool {
    matches!(quality, StreamQuality::Incremental)
}

/// Probe persistence projection: only an incremental stream is recorded as
/// `streaming=true`; an end-flush/buffered route is projected to
/// `streaming=false` so Codex stops expecting incremental replies.
pub fn probe_streaming_projection(quality: StreamQuality) -> bool {
    matches!(quality, StreamQuality::Incremental)
}

/// Lightweight production stream observer. Counts application output deltas
/// (text/tool) and explicit reasoning deltas and records the first of each,
/// so a streaming dispatch can classify [`StreamQuality`] before its usage
/// row is persisted. Pure and cheap: nothing here touches content payloads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamDeltaObserver {
    /// Application output deltas (reasoning is never counted here).
    pub output_delta_count: u64,
    /// Explicit reasoning-channel deltas, recorded independently.
    pub reasoning_delta_count: u64,
    /// Request-relative time of the first output delta.
    pub first_output_delta_ms: Option<u64>,
    /// Function-call argument deltas, tracked independently of commentary text.
    pub tool_delta_count: u64,
    /// Request-relative time of the first function-call argument delta.
    pub first_tool_delta_ms: Option<u64>,
    /// Request-relative time of the first reasoning delta.
    pub first_reasoning_delta_ms: Option<u64>,
}

impl StreamDeltaObserver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe one Responses-wire event by its `type` name. Call with the
    /// request-relative elapsed milliseconds whenever an upstream event block
    /// passes (including the passthrough and normalization paths).
    pub fn observe_responses_kind(&mut self, kind: &str, now_ms: u64) {
        if is_output_delta(kind) {
            self.output_delta_count += 1;
            self.first_output_delta_ms.get_or_insert(now_ms);
            if is_tool_delta(kind) {
                self.tool_delta_count += 1;
                self.first_tool_delta_ms.get_or_insert(now_ms);
            }
        } else if is_reasoning_delta(kind) {
            self.reasoning_delta_count += 1;
            self.first_reasoning_delta_ms.get_or_insert(now_ms);
        }
    }

    /// Observe a Chat-wire upstream chunk (`delta.content` qualifies,
    /// `delta.reasoning_content` is the separate reasoning channel).
    pub fn observe_chat_data(&mut self, data: &str, now_ms: u64) {
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return;
        };
        let content = value
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !content.is_empty() {
            self.output_delta_count += 1;
            self.first_output_delta_ms.get_or_insert(now_ms);
        }
        let reasoning = value
            .pointer("/choices/0/delta/reasoning_content")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !reasoning.is_empty() {
            self.reasoning_delta_count += 1;
            self.first_reasoning_delta_ms.get_or_insert(now_ms);
        }
    }

    /// Classify against a terminal (e.g. `response.completed`) time.
    pub fn classify(
        &self,
        streaming_requested: bool,
        capability_streaming: bool,
        streamed: bool,
        terminal_ms: Option<u64>,
    ) -> StreamQuality {
        classify_stream_quality(&StreamQualityInput {
            streaming_requested,
            capability_streaming,
            streamed,
            output_delta_count: self.output_delta_count,
            first_output_delta_ms: self.first_output_delta_ms,
            tool_delta_count: self.tool_delta_count,
            first_tool_delta_ms: self.first_tool_delta_ms,
            terminal_ms,
            reasoning_delta_count: self.reasoning_delta_count,
            first_reasoning_delta_ms: self.first_reasoning_delta_ms,
        })
    }
}

/// One redacted live-request record. Account identity is a fixed hash only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveRequestArtifact {
    pub schema_version: u32,
    pub artifact_commit: String,
    pub binary_commit: String,
    pub binary_sha256: String,
    pub route: String,
    pub catalog_model: String,
    pub wire: String,
    pub path: LivePath,
    pub connection_ms: Option<u64>,
    pub upstream_first_frame_ms: Option<u64>,
    pub downstream_first_frame_ms: Option<u64>,
    pub completed_ms: Option<u64>,
    pub delta_count: u64,
    pub frame_count: u64,
    pub usage: Option<Value>,
    pub usage_record_count: Option<u64>,
    pub request_id: Option<String>,
    pub outcome: LiveOutcome,
    pub error_category: Option<String>,
    /// HTTP status the upstream returned for a rejected request; `None` when
    /// the request never reached a status decision (send/connection error).
    #[serde(default)]
    pub upstream_status: Option<u16>,
    /// Bounded (truncated) upstream error body for diagnosis. Redacted with the
    /// rest of the artifact; never set from a body that was not read.
    #[serde(default)]
    pub diagnostic: Option<String>,
    pub account_hash: Option<String>,
    pub kind: Option<String>,
    /// Direct-vs-Proxy A/B stage this run belongs to, `None` for the older
    /// single-turn artifacts that predate the A/B harness.
    #[serde(default)]
    pub stage: Option<AbStage>,
    /// Identity of the single WebSocket connection all three A/B stages
    /// (Cold/Warm/Idle) were sent on, read from the runtime's usage ledger.
    #[serde(default)]
    pub connection_id: Option<String>,
    /// Time from sending this stage's `create` to its first frame, distinct
    /// from `connection_ms` (the earlier WS handshake cost).
    #[serde(default)]
    pub turn_first_frame_ms: Option<u64>,
    /// Cold-stage only: `connection_ms + turn_first_frame_ms`, the fair
    /// end-to-end cost a fresh connection actually pays.
    #[serde(default)]
    pub cold_total_first_frame_ms: Option<u64>,
    /// `downstream_first_frame_ms - upstream_first_frame_ms`, both read from
    /// the usage ledger. Never computed from a single client-observed
    /// timestamp duplicated into both fields.
    #[serde(default)]
    pub bridge_delay_ms: Option<u64>,
    /// v2 stream-quality classification, additive over v1 (`None` on
    /// artifacts written before v2).
    #[serde(default)]
    pub stream_quality: Option<StreamQuality>,
    /// Request-relative first output (text/tool) delta.
    #[serde(default)]
    pub first_output_delta_ms: Option<u64>,
    /// Request-relative first explicit reasoning-channel delta.
    #[serde(default)]
    pub first_reasoning_delta_ms: Option<u64>,
    /// Application output delta count (`output_text.delta` /
    /// function-arguments, or Chat `delta.content`). Reasoning deltas are
    /// not counted here.
    #[serde(default)]
    pub output_delta_count: u64,
    /// Explicit reasoning-channel delta count, recorded independently.
    #[serde(default)]
    pub reasoning_delta_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LivePath {
    Direct,
    Proxy,
}

/// One of the three sequential turns the A/B harness sends over a single
/// WebSocket connection: immediately after connect, immediately after the
/// prior turn, and after an idle gap with control frames still being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbStage {
    Cold,
    Warm,
    Idle,
}

/// Idle gap the harness holds the socket open for, processing Ping/Pong,
/// before sending the Idle-stage turn.
pub const AB_IDLE_GAP_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveOutcome {
    Ok,
    Fail,
    Timeout,
    Quota,
    Rejected,
    EnvBlocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveAttributionError {
    OfficialModelMismatch { requested: String },
    OfficialTurnCapExceeded { spent: u32, cap: u32 },
    FirstFrameTimeout { deadline_ms: u64 },
    OfficialRequestParity(String),
    ForbiddenField(String),
    ArtifactLeakedSecret(String),
    QwenStandIn { requested: String, selected: String },
    QwenNotInCatalog { selected: String },
    DeltaAfterTerminal,
    EndOfStreamFlush,
    UpgradeRequired,
    BudgetIo(String),
}

impl std::fmt::Display for LiveAttributionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OfficialModelMismatch { requested } => write!(
                f,
                "Official live model must be {PINNED_OFFICIAL_MODEL}, got {requested}"
            ),
            Self::OfficialTurnCapExceeded { spent, cap } => write!(
                f,
                "Official live turn cap exceeded: spent {spent} of {cap}; refusing to send"
            ),
            Self::FirstFrameTimeout { deadline_ms } => {
                write!(f, "strict first-frame timeout after {deadline_ms} ms")
            }
            Self::OfficialRequestParity(detail) => {
                write!(f, "Official direct/proxy request parity: {detail}")
            }
            Self::ForbiddenField(field) => {
                write!(f, "Official live body must not include {field}")
            }
            Self::ArtifactLeakedSecret(field) => {
                write!(f, "live artifact leaked forbidden field {field}")
            }
            Self::QwenStandIn {
                requested,
                selected,
            } => write!(
                f,
                "refusing to stand in {selected} for requested Qwen {requested}"
            ),
            Self::QwenNotInCatalog { selected } => {
                write!(f, "selected Qwen model {selected} is not in /models")
            }
            Self::DeltaAfterTerminal => {
                write!(f, "no delta arrived before the terminal event")
            }
            Self::EndOfStreamFlush => {
                write!(f, "deltas arrived only as a single end-of-stream flush")
            }
            Self::UpgradeRequired => write!(f, "provider returned HTTP 426"),
            Self::BudgetIo(detail) => write!(f, "Official budget file: {detail}"),
        }
    }
}

impl std::error::Error for LiveAttributionError {}

/// Resolve the Official live model. Unset defaults to Luna; any other value is
/// a hard reject so a stray env cannot spend a different Official SKU.
pub fn resolve_official_live_model(
    env_value: Option<&str>,
) -> Result<&'static str, LiveAttributionError> {
    match env_value.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(PINNED_OFFICIAL_MODEL),
        Some(value) if value == PINNED_OFFICIAL_MODEL => Ok(PINNED_OFFICIAL_MODEL),
        Some(value) => Err(LiveAttributionError::OfficialModelMismatch {
            requested: value.to_string(),
        }),
    }
}

pub fn official_catalog_is_luna(catalog_id: &str) -> bool {
    let normalized = catalog_id.to_ascii_lowercase();
    normalized == PINNED_OFFICIAL_MODEL
        || normalized.contains("gpt-5.6-luna")
        || normalized.contains("gpt-5-6-luna")
}

struct BudgetLock {
    file: std::fs::File,
    lock_path: PathBuf,
}

impl Drop for BudgetLock {
    fn drop(&mut self) {
        if let Err(error) = FileExt::unlock(&self.file) {
            tracing::warn!(
                "failed to unlock Official budget ledger {}: {error}",
                self.lock_path.display()
            );
        }
    }
}

fn acquire_budget_lock(artifact_dir: &Path) -> Result<BudgetLock, LiveAttributionError> {
    std::fs::create_dir_all(artifact_dir).map_err(|error| {
        LiveAttributionError::BudgetIo(format!("create {}: {error}", artifact_dir.display()))
    })?;
    let lock_path = artifact_dir.join("official-budget.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| {
            LiveAttributionError::BudgetIo(format!("open lock {}: {error}", lock_path.display()))
        })?;
    let start = std::time::Instant::now();
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(BudgetLock { file, lock_path }),
            Err(e) if budget_lock_is_busy(&e) => {
                if start.elapsed() > std::time::Duration::from_secs(10) {
                    return Err(LiveAttributionError::BudgetIo(
                        "timeout acquiring official-budget cross-process lock".into(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => {
                return Err(LiveAttributionError::BudgetIo(format!(
                    "acquire lock {}: {e}",
                    lock_path.display()
                )))
            }
        }
    }
}

fn budget_lock_is_busy(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || cfg!(windows) && error.raw_os_error() == Some(33)
}

/// Process-local (and optionally persisted) Official send budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfficialTurnBudget {
    pub spent: u32,
    pub cap: u32,
    /// When set, the campaign is stopped (429/quota/unattributed failure
    /// observed) and no further Official send is admitted, across processes.
    #[serde(default)]
    pub stopped: Option<String>,
}

impl OfficialTurnBudget {
    pub fn new() -> Self {
        Self {
            spent: 0,
            cap: OFFICIAL_LIVE_TURN_CAP,
            stopped: None,
        }
    }

    pub fn remaining(&self) -> u32 {
        self.cap.saturating_sub(self.spent)
    }

    /// Admit one Official send. The 51st call (and later) is rejected, as is
    /// any call after the campaign was stopped by a 429/quota/unattributed
    /// failure — both must not produce a network request.
    pub fn admit(&mut self) -> Result<u32, LiveAttributionError> {
        if self.stopped.is_some() || self.spent >= self.cap {
            return Err(LiveAttributionError::OfficialTurnCapExceeded {
                spent: self.spent,
                cap: self.cap,
            });
        }
        self.spent += 1;
        Ok(self.spent)
    }

    /// Stop the campaign immediately (429 / quota / unattributable transport
    /// failure). All later admits fail before any request is sent.
    pub fn stop(&mut self, reason: &str) {
        self.stopped = Some(reason.to_string());
    }

    pub fn persist_path(artifact_dir: &Path) -> PathBuf {
        artifact_dir.join("official-budget.json")
    }

    /// Atomic admit for a caller that already holds the ledger on disk:
    /// acquires cross-process lock, loads, admits, and persists with temp-file + rename
    /// so crashes never leave a half-written counter and concurrent processes never lose updates.
    pub fn admit_and_persist(artifact_dir: &Path) -> Result<u32, LiveAttributionError> {
        let _lock = acquire_budget_lock(artifact_dir)?;
        let mut budget = Self::load(artifact_dir)?;
        let spent = budget.admit()?;
        budget.save(artifact_dir)?;
        Ok(spent)
    }

    /// Atomic stop: acquires cross-process lock, loads, marks stopped, and persists.
    pub fn stop_and_persist(artifact_dir: &Path, reason: &str) -> Result<(), LiveAttributionError> {
        let _lock = acquire_budget_lock(artifact_dir)?;
        let mut budget = Self::load(artifact_dir)?;
        budget.stop(reason);
        budget.save(artifact_dir)
    }

    pub fn load(artifact_dir: &Path) -> Result<Self, LiveAttributionError> {
        let path = Self::persist_path(artifact_dir);
        if !path.is_file() {
            return Ok(Self::new());
        }
        let text = std::fs::read_to_string(&path).map_err(|error| {
            LiveAttributionError::BudgetIo(format!("read {}: {error}", path.display()))
        })?;
        let loaded: Self = serde_json::from_str(&text).map_err(|error| {
            LiveAttributionError::BudgetIo(format!("parse {}: {error}", path.display()))
        })?;
        // A leftover file written under a previous OFFICIAL_LIVE_TURN_CAP
        // (e.g. the old cap of 8) must migrate its cap to the current value
        // while PRESERVING spent, so the counter stays one shared ledger
        // across candidate/installed/retry run contexts instead of quietly
        // resetting every time the cap constant changes. `spent` is clamped
        // to the new cap only as a defensive floor; a smaller-cap ledger's
        // spent can never exceed its own cap, so this never fires in
        // practice.
        if loaded.cap != OFFICIAL_LIVE_TURN_CAP {
            return Ok(Self {
                spent: loaded.spent.min(OFFICIAL_LIVE_TURN_CAP),
                cap: OFFICIAL_LIVE_TURN_CAP,
                stopped: loaded.stopped,
            });
        }
        Ok(loaded)
    }

    pub fn save(&self, artifact_dir: &Path) -> Result<(), LiveAttributionError> {
        std::fs::create_dir_all(artifact_dir).map_err(|error| {
            LiveAttributionError::BudgetIo(format!("create {}: {error}", artifact_dir.display()))
        })?;
        let path = Self::persist_path(artifact_dir);
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).map_err(|error| {
            LiveAttributionError::BudgetIo(format!("serialize budget: {error}"))
        })?;
        std::fs::write(&tmp, text).map_err(|error| {
            LiveAttributionError::BudgetIo(format!("write {}: {error}", tmp.display()))
        })?;
        std::fs::rename(&tmp, &path).map_err(|error| {
            LiveAttributionError::BudgetIo(format!("rename {}: {error}", path.display()))
        })
    }
}

impl Default for OfficialTurnBudget {
    fn default() -> Self {
        Self::new()
    }
}

/// Official WebSocket `response.create` / HTTP POST body. `store:false`,
/// `stream:true`, and no `max_output_tokens`.
pub fn official_live_create_body(model: &str, input_text: &str) -> Value {
    json!({
        "type": "response.create",
        "model": model,
        "store": false,
        "stream": true,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": input_text
            }]
        }]
    })
}

/// HTTP POST `/v1/responses` body. Same contract as the WS create frame,
/// without the WebSocket envelope type.
pub fn official_live_http_body(model: &str, input_text: &str) -> Value {
    let mut body = official_live_create_body(model, input_text);
    if let Some(object) = body.as_object_mut() {
        object.remove("type");
    }
    body
}

pub fn official_live_marker_input(marker: &str) -> String {
    format!("Reply with exactly {marker} and nothing else.")
}

/// Headers that Official direct WSS and proxy WSS must both send. Auth values
/// differ; names and the `openai-beta` value must match.
pub fn official_live_metadata_headers() -> Vec<(String, String)> {
    vec![
        ("openai-beta".into(), "responses=experimental".into()),
        (
            "x-codex-turn-metadata".into(),
            "vellum-live-attribution".into(),
        ),
    ]
}

pub fn assert_no_max_output_tokens(body: &Value) -> Result<(), LiveAttributionError> {
    if body.get("max_output_tokens").is_some() {
        return Err(LiveAttributionError::ForbiddenField(
            "max_output_tokens".into(),
        ));
    }
    Ok(())
}

pub fn assert_official_live_body(
    body: &Value,
    model: &str,
    input_text: &str,
) -> Result<(), LiveAttributionError> {
    assert_no_max_output_tokens(body)?;
    let got_model = body.get("model").and_then(Value::as_str).unwrap_or("");
    if got_model != model {
        return Err(LiveAttributionError::OfficialRequestParity(format!(
            "model {got_model} != {model}"
        )));
    }
    if body.get("store") != Some(&Value::Bool(false)) {
        return Err(LiveAttributionError::OfficialRequestParity(
            "store must be false".into(),
        ));
    }
    if body.get("stream") != Some(&Value::Bool(true)) {
        return Err(LiveAttributionError::OfficialRequestParity(
            "stream must be true".into(),
        ));
    }
    let rendered = body.to_string();
    if !rendered.contains(input_text) {
        return Err(LiveAttributionError::OfficialRequestParity(
            "input text missing".into(),
        ));
    }
    Ok(())
}

/// Compare the Official request that will go direct vs through the proxy.
pub fn assert_official_request_parity(
    direct_body: &Value,
    proxy_body: &Value,
    direct_headers: &[(String, String)],
    proxy_headers: &[(String, String)],
) -> Result<(), LiveAttributionError> {
    if direct_body != proxy_body {
        return Err(LiveAttributionError::OfficialRequestParity(
            "bodies differ".into(),
        ));
    }
    let model = direct_body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("");
    if model != PINNED_OFFICIAL_MODEL && !official_catalog_is_luna(model) {
        return Err(LiveAttributionError::OfficialModelMismatch {
            requested: model.to_string(),
        });
    }
    if direct_body.get("store") != Some(&Value::Bool(false))
        || proxy_body.get("store") != Some(&Value::Bool(false))
    {
        return Err(LiveAttributionError::OfficialRequestParity(
            "store must be false on both bodies".into(),
        ));
    }
    if direct_body.get("stream") != Some(&Value::Bool(true))
        || proxy_body.get("stream") != Some(&Value::Bool(true))
    {
        return Err(LiveAttributionError::OfficialRequestParity(
            "stream must be true on both bodies".into(),
        ));
    }
    assert_no_max_output_tokens(direct_body)?;
    assert_no_max_output_tokens(proxy_body)?;

    let required = ["openai-beta", "authorization", "chatgpt-account-id"];
    for name in required {
        let direct = header_value(direct_headers, name);
        let proxy = header_value(proxy_headers, name);
        if direct.is_none() {
            return Err(LiveAttributionError::OfficialRequestParity(format!(
                "direct missing {name}"
            )));
        }
        if proxy.is_none() {
            return Err(LiveAttributionError::OfficialRequestParity(format!(
                "proxy missing {name}"
            )));
        }
    }
    let direct_beta = header_value(direct_headers, "openai-beta");
    let proxy_beta = header_value(proxy_headers, "openai-beta");
    if direct_beta != proxy_beta {
        return Err(LiveAttributionError::OfficialRequestParity(
            "openai-beta values differ".into(),
        ));
    }
    Ok(())
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.as_str()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstFrameObservation {
    Observed { elapsed_ms: u64 },
    TimedOut { deadline_ms: u64 },
}

/// Strict first-frame: a timeout is a failure, never a log-then-PASS.
pub fn evaluate_first_frame(
    observation: FirstFrameObservation,
) -> Result<u64, LiveAttributionError> {
    match observation {
        FirstFrameObservation::Observed { elapsed_ms } => Ok(elapsed_ms),
        FirstFrameObservation::TimedOut { deadline_ms } => {
            Err(LiveAttributionError::FirstFrameTimeout { deadline_ms })
        }
    }
}

pub fn extra_delay_budget_ms(direct_ttft_ms: u64) -> u64 {
    PROXY_EXTRA_DELAY_FLOOR_MS.max(direct_ttft_ms.saturating_mul(PROXY_EXTRA_DELAY_PERCENT) / 100)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimingVerdict {
    WithinBudget,
    UpstreamConnectionDelay,
    BridgeDelay,
    ProviderLatency,
}

pub fn classify_proxy_delay(
    direct_ttft_ms: u64,
    proxy_ttft_ms: u64,
    upstream_first_frame_ms: Option<u64>,
    downstream_first_frame_ms: Option<u64>,
) -> TimingVerdict {
    let extra = proxy_ttft_ms.saturating_sub(direct_ttft_ms);
    if extra <= extra_delay_budget_ms(direct_ttft_ms) {
        return TimingVerdict::WithinBudget;
    }
    match (upstream_first_frame_ms, downstream_first_frame_ms) {
        (Some(upstream), Some(downstream)) => {
            let bridge = downstream.saturating_sub(upstream);
            let upstream_extra = upstream.saturating_sub(direct_ttft_ms);
            if bridge >= upstream_extra {
                TimingVerdict::BridgeDelay
            } else {
                TimingVerdict::UpstreamConnectionDelay
            }
        }
        (Some(upstream), None) => {
            if upstream.saturating_sub(direct_ttft_ms) > extra_delay_budget_ms(direct_ttft_ms) {
                TimingVerdict::UpstreamConnectionDelay
            } else {
                TimingVerdict::BridgeDelay
            }
        }
        _ => TimingVerdict::UpstreamConnectionDelay,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OfficialAttribution {
    EnvOrUpstream,
    VellumHandshake,
    UpstreamConnectionDelay,
    BridgeDelay,
    WithinBudget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnSuccess {
    pub ok: bool,
}

pub fn attribute_official(
    direct_ok: bool,
    proxy_ok: bool,
    timing: Option<TimingVerdict>,
) -> OfficialAttribution {
    match (direct_ok, proxy_ok) {
        (false, _) => OfficialAttribution::EnvOrUpstream,
        (true, false) => OfficialAttribution::VellumHandshake,
        (true, true) => match timing.unwrap_or(TimingVerdict::WithinBudget) {
            TimingVerdict::WithinBudget | TimingVerdict::ProviderLatency => {
                OfficialAttribution::WithinBudget
            }
            TimingVerdict::UpstreamConnectionDelay => OfficialAttribution::UpstreamConnectionDelay,
            TimingVerdict::BridgeDelay => OfficialAttribution::BridgeDelay,
        },
    }
}

/// One side's (Direct or Proxy) outcome for one A/B stage. `ms` is the
/// stage-appropriate fair-comparison number: `cold_total_first_frame_ms` for
/// Cold (handshake cost is real and must be charged), `turn_first_frame_ms`
/// for Warm/Idle (same socket, no handshake to charge).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbStageTiming {
    pub ok: bool,
    pub ms: Option<u64>,
}

impl AbStageTiming {
    pub fn ok(ms: u64) -> Self {
        Self {
            ok: true,
            ms: Some(ms),
        }
    }

    pub fn fail() -> Self {
        Self {
            ok: false,
            ms: None,
        }
    }
}

/// Direct-vs-Proxy Cold/Warm/Idle timings for the Official A/B run. Pure
/// input to `classify_official_ab`, built from six `LiveRequestArtifact`s
/// (or, in tests, directly from synthetic values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbComparisonInput {
    pub direct_cold: AbStageTiming,
    pub direct_warm: AbStageTiming,
    pub direct_idle: AbStageTiming,
    pub proxy_cold: AbStageTiming,
    pub proxy_warm: AbStageTiming,
    pub proxy_idle: AbStageTiming,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbVerdict {
    /// Direct / Direct: fail / fail (or Direct itself failed) — the
    /// reference call didn't work, so the environment/provider is suspect,
    /// not Vellum.
    ProviderOrUpstreamEnvironmentIssue,
    /// Direct ok, Proxy fail (outside the idle-only cases below) — Vellum
    /// handshake/auth/routing/bridge issue.
    VellumHandshakeAuthRoutingOrBridgeIssue,
    /// Cold stage over budget, Warm stage within budget — connection,
    /// catalog, or auth-path issue that only shows up on a fresh connect.
    ConnectionCatalogOrAuthPathIssue,
    /// Warm stage over budget — an application bridge / scheduling /
    /// persistence issue, since the connection cost is already amortized.
    ApplicationBridgeSchedulingOrPersistenceIssue,
    /// Direct idle turn ok, Proxy idle turn fails — Vellum's control-frame
    /// or idle-connection handling is the problem.
    VellumControlFrameOrIdleHandlingIssue,
    /// Both sides fail the idle turn — the provider's own idle policy or
    /// environment, not Vellum.
    UpstreamIdlePolicyOrEnvironmentIssue,
    /// All three stages ok within budget on both sides — Official is not a
    /// Vellum-side problem.
    OfficialIssueClosedNoVellumProblem,
}

fn ab_stage_over_budget(direct: AbStageTiming, proxy: AbStageTiming) -> bool {
    match (direct.ms, proxy.ms) {
        (Some(direct_ms), Some(proxy_ms)) => {
            proxy_ms.saturating_sub(direct_ms) > extra_delay_budget_ms(direct_ms)
        }
        // Missing timing on an otherwise-ok stage cannot be proven within
        // budget; fail closed toward "over budget" rather than hide it.
        _ => !(direct.ok && proxy.ok),
    }
}

/// Pure classifier for the Official Direct-vs-Proxy decision table. Checked
/// top-to-bottom, first match wins, exactly mirroring the table row order.
pub fn classify_official_ab(input: &AbComparisonInput) -> AbVerdict {
    let direct_core_ok = input.direct_cold.ok && input.direct_warm.ok;
    let proxy_core_ok = input.proxy_cold.ok && input.proxy_warm.ok;

    if !direct_core_ok {
        return AbVerdict::ProviderOrUpstreamEnvironmentIssue;
    }
    if !proxy_core_ok {
        return AbVerdict::VellumHandshakeAuthRoutingOrBridgeIssue;
    }
    if ab_stage_over_budget(input.direct_cold, input.proxy_cold)
        && !ab_stage_over_budget(input.direct_warm, input.proxy_warm)
    {
        return AbVerdict::ConnectionCatalogOrAuthPathIssue;
    }
    if ab_stage_over_budget(input.direct_warm, input.proxy_warm) {
        return AbVerdict::ApplicationBridgeSchedulingOrPersistenceIssue;
    }
    if input.direct_idle.ok && !input.proxy_idle.ok {
        return AbVerdict::VellumControlFrameOrIdleHandlingIssue;
    }
    if !input.direct_idle.ok && !input.proxy_idle.ok {
        return AbVerdict::UpstreamIdlePolicyOrEnvironmentIssue;
    }
    AbVerdict::OfficialIssueClosedNoVellumProblem
}

pub fn should_stop_official_luna(error_category: Option<&str>, status: Option<u16>) -> bool {
    if status == Some(429) {
        return true;
    }
    matches!(
        error_category,
        Some("provider_quota") | Some("unattributed") | Some("quota")
    )
}

pub fn luna_quota_allows_swe(remaining_percent: f64) -> bool {
    remaining_percent >= LUNA_SWE_MIN_REMAINING_PERCENT
}

pub fn is_upgrade_required(status: u16) -> bool {
    status == 426
}

pub fn classify_http_status(status: u16) -> &'static str {
    match status {
        401 | 403 => "provider_unauthorized",
        426 => "provider_protocol",
        429 => "provider_quota",
        408 | 409 | 425 => "provider_protocol",
        500..=599 => "provider_unavailable",
        _ if status >= 400 => "provider_protocol",
        _ => "ok",
    }
}

pub fn assert_no_upgrade_required(status: Option<u16>) -> Result<(), LiveAttributionError> {
    if status == Some(426) {
        return Err(LiveAttributionError::UpgradeRequired);
    }
    Ok(())
}

pub fn assert_delta_before_terminal(
    delta_count: u64,
    first_delta_ms: Option<u64>,
    completed_ms: u64,
) -> Result<(), LiveAttributionError> {
    if delta_count == 0 || first_delta_ms.is_none() {
        return Err(LiveAttributionError::DeltaAfterTerminal);
    }
    let first = first_delta_ms.unwrap();
    if first >= completed_ms {
        return Err(LiveAttributionError::DeltaAfterTerminal);
    }
    if completed_ms.saturating_sub(first) <= END_FLUSH_WINDOW_MS && delta_count > 0 {
        return Err(LiveAttributionError::EndOfStreamFlush);
    }
    Ok(())
}

pub fn looks_like_qwen_38(id: &str) -> bool {
    let normalized = id.to_ascii_lowercase();
    normalized.contains("qwen3.8")
        || normalized.contains("qwen-3.8")
        || normalized.contains("qwen3-8")
}

pub fn looks_like_qwen_36(id: &str) -> bool {
    let normalized = id.to_ascii_lowercase();
    normalized.contains("qwen3.6")
        || normalized.contains("qwen-3.6")
        || normalized.contains("qwen3-6")
}

/// Refuse to pass qwen3.6 as if it were the requested Qwen 3.8.
pub fn refuse_qwen_standin(requested: &str, selected: &str) -> Result<(), LiveAttributionError> {
    if looks_like_qwen_38(requested) && looks_like_qwen_36(selected) {
        return Err(LiveAttributionError::QwenStandIn {
            requested: requested.to_string(),
            selected: selected.to_string(),
        });
    }
    Ok(())
}

pub fn select_qwen_from_models(
    requested: &str,
    catalog_ids: &[String],
) -> Result<String, LiveAttributionError> {
    let selected = catalog_ids
        .iter()
        .find(|id| id.as_str() == requested)
        .cloned()
        .or_else(|| {
            catalog_ids
                .iter()
                .find(|id| id.eq_ignore_ascii_case(requested))
                .cloned()
        });
    let Some(selected) = selected else {
        return Err(LiveAttributionError::QwenNotInCatalog {
            selected: requested.to_string(),
        });
    };
    refuse_qwen_standin(requested, &selected)?;
    Ok(selected)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveKind {
    Marker,
    Sse,
    TypedTool,
    Continuation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenCodeModelCaps {
    pub catalog_id: String,
    pub wire: String,
    pub streaming: bool,
    pub tools: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenCodeModelPlan {
    pub catalog_id: String,
    pub wire: String,
    pub kinds: Vec<LiveKind>,
    pub fallback: Option<String>,
}

/// Plan one enabled OpenCode model from *its* capabilities. Missing tools
/// fall back on this route only; another model's settings never leak in.
pub fn plan_opencode_model(caps: &OpenCodeModelCaps) -> Option<OpenCodeModelPlan> {
    if !caps.enabled {
        return None;
    }
    let mut kinds = vec![LiveKind::Marker];
    if caps.streaming {
        kinds.push(LiveKind::Sse);
    }
    if caps.tools {
        kinds.push(LiveKind::TypedTool);
    }
    kinds.push(LiveKind::Continuation);
    let fallback = if !caps.streaming || !caps.tools {
        Some("route_fallback".into())
    } else {
        None
    };
    Some(OpenCodeModelPlan {
        catalog_id: caps.catalog_id.clone(),
        wire: caps.wire.clone(),
        kinds,
        fallback,
    })
}

pub fn assert_opencode_plans_isolated(plans: &[OpenCodeModelPlan]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for plan in plans {
        if !seen.insert(&plan.catalog_id) {
            return Err(format!("duplicate OpenCode plan for {}", plan.catalog_id));
        }
        if plan.wire != "chat" && plan.wire != "responses" {
            return Err(format!(
                "OpenCode model {} has unknown wire {}",
                plan.catalog_id, plan.wire
            ));
        }
    }
    Ok(())
}

pub fn account_hash(account_id: &str) -> String {
    hash_text(account_id)
}

pub fn identity_fields() -> (String, String, String) {
    (
        build_commit().to_string(),
        build_commit().to_string(),
        current_binary_sha256(),
    )
}

pub fn new_live_artifact(
    route: &str,
    catalog_model: &str,
    wire: &str,
    path: LivePath,
    account_id: Option<&str>,
) -> LiveRequestArtifact {
    let (artifact_commit, binary_commit, binary_sha256) = identity_fields();
    LiveRequestArtifact {
        schema_version: 2,
        artifact_commit,
        binary_commit,
        binary_sha256,
        route: route.to_string(),
        catalog_model: catalog_model.to_string(),
        wire: wire.to_string(),
        path,
        connection_ms: None,
        upstream_first_frame_ms: None,
        downstream_first_frame_ms: None,
        completed_ms: None,
        delta_count: 0,
        frame_count: 0,
        usage: None,
        usage_record_count: None,
        request_id: None,
        outcome: LiveOutcome::Fail,
        error_category: None,
        upstream_status: None,
        diagnostic: None,
        account_hash: account_id.map(account_hash),
        kind: None,
        stage: None,
        connection_id: None,
        turn_first_frame_ms: None,
        cold_total_first_frame_ms: None,
        bridge_delay_ms: None,
        stream_quality: None,
        first_output_delta_ms: None,
        first_reasoning_delta_ms: None,
        output_delta_count: 0,
        reasoning_delta_count: 0,
    }
}

/// `connection_ms + turn_first_frame_ms`, the fair Cold-stage total. Only
/// meaningful for `AbStage::Cold`; Warm/Idle reuse the same connection so
/// their handshake cost was already paid once.
pub fn cold_total_first_frame_ms(
    connection_ms: Option<u64>,
    turn_first_frame_ms: Option<u64>,
) -> Option<u64> {
    match (connection_ms, turn_first_frame_ms) {
        (Some(connection), Some(turn)) => Some(connection + turn),
        _ => None,
    }
}

/// `downstream - upstream`, both sourced from the usage ledger. A caller
/// that only has one client-observed timestamp must not call this with the
/// same value twice; that would silently report zero bridge delay always.
pub fn bridge_delay_ms(
    upstream_first_frame_ms: Option<u64>,
    downstream_first_frame_ms: Option<u64>,
) -> Option<u64> {
    match (upstream_first_frame_ms, downstream_first_frame_ms) {
        (Some(upstream), Some(downstream)) => Some(downstream.saturating_sub(upstream)),
        _ => None,
    }
}

/// One usage-ledger row's timing/identity fields for a single request,
/// matched by `request_id`. This is the only legitimate source for
/// `upstream_first_frame_ms` / `downstream_first_frame_ms` / `connection_id`
/// on an A/B artifact — never the harness's own single client-side clock
/// read twice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedgerFrame {
    pub upstream_first_frame_ms: Option<u64>,
    pub downstream_first_frame_ms: Option<u64>,
    pub connection_id: Option<String>,
    pub usage_record_count: u64,
}

/// Read the ledger frame for `request_id` out of the runtime's real usage
/// records (`UsageRecord::first_event_ms` is the first parseable upstream
/// event; `UsageRecord::first_downstream_frame_ms` is the first application
/// frame actually sent downstream — two distinct pipeline stages already
/// recorded by the runtime, per `crate::usage`).
pub fn ledger_frame_for_request(
    records: &[crate::usage::UsageRecord],
    request_id: &str,
) -> LedgerFrame {
    let matching: Vec<&crate::usage::UsageRecord> = records
        .iter()
        .filter(|record| record.request_id.as_deref() == Some(request_id))
        .collect();
    let latest = matching.last().copied();
    LedgerFrame {
        upstream_first_frame_ms: latest.and_then(|record| record.first_event_ms),
        downstream_first_frame_ms: latest.and_then(|record| record.first_downstream_frame_ms),
        connection_id: latest.and_then(|record| record.connection_id.clone()),
        usage_record_count: matching.len() as u64,
    }
}

/// Apply a ledger frame onto an artifact's timing fields. Kept as one
/// function so every call site derives `bridge_delay_ms` /
/// `cold_total_first_frame_ms` the same way instead of hand-rolling it.
pub fn apply_ledger_frame(artifact: &mut LiveRequestArtifact, ledger: &LedgerFrame) {
    artifact.upstream_first_frame_ms = ledger.upstream_first_frame_ms;
    artifact.downstream_first_frame_ms = ledger.downstream_first_frame_ms;
    artifact.connection_id = ledger.connection_id.clone();
    artifact.usage_record_count = Some(ledger.usage_record_count);
    artifact.bridge_delay_ms = bridge_delay_ms(
        ledger.upstream_first_frame_ms,
        ledger.downstream_first_frame_ms,
    );
    if artifact.stage == Some(AbStage::Cold) {
        artifact.cold_total_first_frame_ms =
            cold_total_first_frame_ms(artifact.connection_ms, artifact.turn_first_frame_ms);
    }
}

fn key_is_forbidden(key: &str) -> bool {
    let normalized = key.replace('_', "-").to_ascii_lowercase();
    FORBIDDEN_ARTIFACT_KEYS.iter().any(|forbidden| {
        normalized == *forbidden || normalized.contains(&forbidden.replace('_', "-"))
    })
}

fn looks_like_email(value: &str) -> bool {
    let Some((user, domain)) = value.split_once('@') else {
        return false;
    };
    !user.is_empty() && domain.contains('.') && !domain.contains(' ')
}

fn strip_forbidden(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut kept = Map::new();
            for (key, child) in map {
                if key_is_forbidden(key) {
                    continue;
                }
                kept.insert(key.clone(), strip_forbidden(child));
            }
            Value::Object(kept)
        }
        Value::Array(items) => Value::Array(items.iter().map(strip_forbidden).collect()),
        Value::String(text) if looks_like_email(text) => Value::String(account_hash(text)),
        Value::String(text) => Value::String(redact_sensitive_text(text)),
        other => other.clone(),
    }
}

/// Redact a live artifact. Account/email/token/raw provider body never leave.
pub fn redact_live_artifact(value: &Value) -> Value {
    strip_forbidden(&redact_sensitive_json(value))
}

pub fn artifact_to_redacted_json(
    artifact: &LiveRequestArtifact,
) -> Result<Value, LiveAttributionError> {
    let raw = serde_json::to_value(artifact)
        .map_err(|error| LiveAttributionError::BudgetIo(format!("serialize artifact: {error}")))?;
    let redacted = redact_live_artifact(&raw);
    assert_artifact_is_redacted(&redacted)?;
    Ok(redacted)
}

pub fn assert_artifact_is_redacted(value: &Value) -> Result<(), LiveAttributionError> {
    walk_for_leaks(value, "")
}

fn walk_for_leaks(value: &Value, path: &str) -> Result<(), LiveAttributionError> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key_is_forbidden(key) {
                    return Err(LiveAttributionError::ArtifactLeakedSecret(key.clone()));
                }
                let next = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                walk_for_leaks(child, &next)?;
            }
            Ok(())
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                walk_for_leaks(child, &format!("{path}[{index}]"))?;
            }
            Ok(())
        }
        Value::String(text) => {
            if looks_like_email(text) {
                return Err(LiveAttributionError::ArtifactLeakedSecret(format!(
                    "{path} email"
                )));
            }
            if text.starts_with("sk-") || text.starts_with("eyJ") || text.contains("Bearer ") {
                return Err(LiveAttributionError::ArtifactLeakedSecret(format!(
                    "{path} token"
                )));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

pub fn write_redacted_artifact(
    artifact_dir: &Path,
    name: &str,
    artifact: &LiveRequestArtifact,
) -> Result<PathBuf, LiveAttributionError> {
    std::fs::create_dir_all(artifact_dir).map_err(|error| {
        LiveAttributionError::BudgetIo(format!("create {}: {error}", artifact_dir.display()))
    })?;
    let path = artifact_dir.join(name);
    let redacted = artifact_to_redacted_json(artifact)?;
    let text = serde_json::to_string_pretty(&redacted)
        .map_err(|error| LiveAttributionError::BudgetIo(format!("pretty artifact: {error}")))?;
    std::fs::write(&path, text).map_err(|error| {
        LiveAttributionError::BudgetIo(format!("write {}: {error}", path.display()))
    })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn official_model_pin_rejects_anything_but_luna() {
        assert_eq!(
            resolve_official_live_model(None).unwrap(),
            PINNED_OFFICIAL_MODEL
        );
        assert_eq!(
            resolve_official_live_model(Some("gpt-5.6-luna")).unwrap(),
            PINNED_OFFICIAL_MODEL
        );
        let error = resolve_official_live_model(Some("gpt-5.4-mini")).unwrap_err();
        assert!(matches!(
            error,
            LiveAttributionError::OfficialModelMismatch { .. }
        ));
        assert!(official_catalog_is_luna("vlm-abc-gpt-5-6-luna"));
        assert!(!official_catalog_is_luna("gpt-5.4-mini"));
        assert!(!official_catalog_is_luna("gpt-5.6-sol"));
    }

    #[test]
    fn official_turn_cap_is_two_hundred_and_the_two_hundred_and_first_is_rejected() {
        let mut budget = OfficialTurnBudget::new();
        assert_eq!(budget.remaining(), 200);
        assert_eq!(OFFICIAL_LIVE_TURN_CAP, 200);
        for expected in 1..=OFFICIAL_LIVE_TURN_CAP {
            assert_eq!(budget.admit().unwrap(), expected);
        }
        let error = budget.admit().unwrap_err();
        assert!(matches!(
            error,
            LiveAttributionError::OfficialTurnCapExceeded {
                spent: OFFICIAL_LIVE_TURN_CAP,
                cap: OFFICIAL_LIVE_TURN_CAP
            }
        ));
        assert_eq!(budget.spent, OFFICIAL_LIVE_TURN_CAP);
        assert_eq!(budget.remaining(), 0);
    }

    #[test]
    fn leftover_cap_eight_budget_migrates_cap_to_two_hundred_and_preserves_spent() {
        // The Luna campaign ledger used to be {spent: 8, cap: 8}. It must
        // become {spent: 8, cap: 200}: same counter, higher ceiling. Resetting
        // spent to 0 here would double-count turns already charged against
        // the account across candidate/installed/retry run contexts.
        let leftover = tempfile::tempdir().unwrap();
        let stale = OfficialTurnBudget {
            spent: 8,
            cap: 8,
            stopped: None,
        };
        stale.save(leftover.path()).unwrap();
        let migrated = OfficialTurnBudget::load(leftover.path()).unwrap();
        assert_eq!(
            migrated,
            OfficialTurnBudget {
                spent: 8,
                cap: 200,
                stopped: None
            }
        );
        assert_eq!(migrated.remaining(), 192);

        let fresh = tempfile::tempdir().unwrap();
        let loaded = OfficialTurnBudget::load(fresh.path()).unwrap();
        assert_eq!(loaded.spent, 0);
        assert_eq!(loaded.cap, 200);
        assert_eq!(loaded.remaining(), 200);
    }

    #[test]
    fn official_budget_is_one_shared_ledger_across_candidate_installed_and_retry() {
        // Each of candidate / installed / retry is a separate process
        // invocation of run_official against the same --artifact-dir. Model
        // that as three independent load -> admit -> save cycles and prove
        // the counter accumulates instead of resetting per context.
        let dir = tempfile::tempdir().unwrap();
        let stale = OfficialTurnBudget {
            spent: 8,
            cap: 8,
            stopped: None,
        };
        stale.save(dir.path()).unwrap();

        let mut candidate = OfficialTurnBudget::load(dir.path()).unwrap();
        assert_eq!(candidate.spent, 8);
        candidate.admit().unwrap();
        candidate.admit().unwrap();
        candidate.save(dir.path()).unwrap();

        let mut installed = OfficialTurnBudget::load(dir.path()).unwrap();
        assert_eq!(installed.spent, 10, "installed must see candidate's spends");
        installed.admit().unwrap();
        installed.save(dir.path()).unwrap();

        let retry = OfficialTurnBudget::load(dir.path()).unwrap();
        assert_eq!(
            retry.spent, 11,
            "retry must see candidate + installed spends"
        );
        assert_eq!(retry.cap, OFFICIAL_LIVE_TURN_CAP);
    }

    #[test]
    fn safety_ceiling_two_hundred_is_a_hard_cap_not_a_target() {
        // 200 bounds how far a live run is allowed to go on Luna, it is not a
        // quota the harness should always try to fully spend.
        let mut budget = OfficialTurnBudget::new();
        for _ in 0..5 {
            budget.admit().unwrap();
        }
        assert!(
            budget.remaining() > 0,
            "stopping early (e.g. on 429/quota) must leave remaining budget unspent"
        );
        assert_eq!(
            OFFICIAL_LIVE_TURN_CAP, 200,
            "the ceiling itself must stay 200"
        );
    }

    #[test]
    fn official_budget_round_trips_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut budget = OfficialTurnBudget::new();
        budget.admit().unwrap();
        budget.admit().unwrap();
        budget.save(dir.path()).unwrap();
        let loaded = OfficialTurnBudget::load(dir.path()).unwrap();
        assert_eq!(loaded.spent, 2);
        assert_eq!(loaded.remaining(), OFFICIAL_LIVE_TURN_CAP - 2);
    }

    #[test]
    fn stopped_budget_rejects_every_later_admit_and_persists_across_processes() {
        let dir = tempfile::tempdir().unwrap();
        let mut budget = OfficialTurnBudget::new();
        budget.admit().unwrap();
        budget.stop("provider_quota");
        assert!(budget.admit().is_err(), "stopped campaigns admit nothing");
        budget.save(dir.path()).unwrap();

        // A separate process (a fresh load) sees the same stop and refuses.
        let mut reloaded = OfficialTurnBudget::load(dir.path()).unwrap();
        assert_eq!(reloaded.spent, 1);
        assert_eq!(reloaded.stopped.as_deref(), Some("provider_quota"));
        assert!(reloaded.admit().is_err());
        assert_eq!(reloaded.spent, 1, "a refused admit must not spend");

        // stop_and_persist round-trips the reason.
        OfficialTurnBudget::stop_and_persist(dir.path(), "unattributed").unwrap();
        let again = OfficialTurnBudget::load(dir.path()).unwrap();
        assert_eq!(again.stopped.as_deref(), Some("unattributed"));
    }

    #[test]
    fn admit_and_persist_is_atomic_across_callers_and_never_leaves_a_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            OfficialTurnBudget::admit_and_persist(dir.path()).unwrap(),
            1
        );
        assert_eq!(
            OfficialTurnBudget::admit_and_persist(dir.path()).unwrap(),
            2
        );
        assert_eq!(
            OfficialTurnBudget::load(dir.path()).unwrap().spent,
            2,
            "each caller must see the previous caller's spend"
        );
        assert!(
            !dir.path().join("official-budget.json.tmp").exists(),
            "the atomic temp file must be renamed away"
        );
        assert!(dir.path().join("official-budget.json").is_file());
    }

    #[test]
    fn official_live_body_is_store_false_stream_true_without_max_output_tokens() {
        let body = official_live_create_body(PINNED_OFFICIAL_MODEL, "hello-luna");
        assert_official_live_body(&body, PINNED_OFFICIAL_MODEL, "hello-luna").unwrap();
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("max_output_tokens").is_none());
        let http = official_live_http_body(PINNED_OFFICIAL_MODEL, "hello-luna");
        assert!(http.get("type").is_none());
        assert_eq!(http["store"], false);
        let mut tainted = body.clone();
        tainted["max_output_tokens"] = json!(64);
        assert!(assert_no_max_output_tokens(&tainted).is_err());
    }

    #[test]
    fn official_direct_and_proxy_bodies_and_required_headers_must_match() {
        let input = official_live_marker_input("VELLUM_LUNA");
        let body = official_live_create_body(PINNED_OFFICIAL_MODEL, &input);
        let headers = |token: &str| {
            let mut headers = official_live_metadata_headers();
            headers.push(("authorization".into(), format!("Bearer {token}")));
            headers.push(("chatgpt-account-id".into(), "acct_direct".into()));
            headers
        };
        assert_official_request_parity(&body, &body, &headers("tok-a"), &headers("tok-b")).unwrap();

        let mut other = body.clone();
        other["model"] = json!("gpt-5.4-mini");
        assert!(assert_official_request_parity(
            &body,
            &other,
            &headers("tok-a"),
            &headers("tok-b")
        )
        .is_err());

        let mut missing_beta = headers("tok-a");
        missing_beta.retain(|(name, _)| name != "openai-beta");
        assert!(
            assert_official_request_parity(&body, &body, &missing_beta, &headers("tok-b")).is_err()
        );
    }

    #[test]
    fn first_frame_timeout_is_a_failure() {
        let ok = evaluate_first_frame(FirstFrameObservation::Observed { elapsed_ms: 120 });
        assert_eq!(ok.unwrap(), 120);
        let error = evaluate_first_frame(FirstFrameObservation::TimedOut {
            deadline_ms: OFFICIAL_FIRST_FRAME_DEADLINE_MS,
        })
        .unwrap_err();
        assert!(matches!(
            error,
            LiveAttributionError::FirstFrameTimeout {
                deadline_ms: OFFICIAL_FIRST_FRAME_DEADLINE_MS
            }
        ));
    }

    #[test]
    fn extra_delay_budget_is_max_of_floor_and_twenty_percent() {
        assert_eq!(extra_delay_budget_ms(100), 250);
        assert_eq!(extra_delay_budget_ms(2000), 400);
        assert_eq!(
            classify_proxy_delay(1000, 1100, Some(1050), Some(1080)),
            TimingVerdict::WithinBudget
        );
        assert_eq!(
            classify_proxy_delay(1000, 2000, Some(1900), Some(1980)),
            TimingVerdict::UpstreamConnectionDelay
        );
        assert_eq!(
            classify_proxy_delay(1000, 2000, Some(1100), Some(1950)),
            TimingVerdict::BridgeDelay
        );
    }

    #[test]
    fn official_attribution_table() {
        assert_eq!(
            attribute_official(false, false, None),
            OfficialAttribution::EnvOrUpstream
        );
        assert_eq!(
            attribute_official(false, true, None),
            OfficialAttribution::EnvOrUpstream
        );
        assert_eq!(
            attribute_official(true, false, None),
            OfficialAttribution::VellumHandshake
        );
        assert_eq!(
            attribute_official(true, true, Some(TimingVerdict::WithinBudget)),
            OfficialAttribution::WithinBudget
        );
        assert_eq!(
            attribute_official(true, true, Some(TimingVerdict::BridgeDelay)),
            OfficialAttribution::BridgeDelay
        );
    }

    #[test]
    fn luna_stops_on_429_quota_or_unattributed() {
        assert!(should_stop_official_luna(None, Some(429)));
        assert!(should_stop_official_luna(Some("provider_quota"), None));
        assert!(should_stop_official_luna(Some("unattributed"), None));
        assert!(!should_stop_official_luna(
            Some("provider_protocol"),
            Some(502)
        ));
        assert!(!luna_quota_allows_swe(29.9));
        assert!(luna_quota_allows_swe(30.0));
    }

    #[test]
    fn redaction_strips_account_email_token_and_raw_body() {
        let raw = json!({
            "route": "openai-official",
            "account_id": "acct_live_secret",
            "email": "developer@example.test",
            "authorization": "Bearer sk-secret-token",
            "token": "sk-live-abcdef",
            "raw_body": {"output_text": "secret provider bytes"},
            "provider_body": "raw upstream",
            "account_hash": account_hash("acct_live_secret"),
            "catalog_model": PINNED_OFFICIAL_MODEL
        });
        let redacted = redact_live_artifact(&raw);
        assert!(redacted.get("account_id").is_none());
        assert!(redacted.get("email").is_none());
        assert!(redacted.get("authorization").is_none());
        assert!(redacted.get("token").is_none());
        assert!(redacted.get("raw_body").is_none());
        assert!(redacted.get("provider_body").is_none());
        assert_eq!(redacted["catalog_model"], PINNED_OFFICIAL_MODEL);
        assert_eq!(redacted["account_hash"], account_hash("acct_live_secret"));
        assert_artifact_is_redacted(&redacted).unwrap();
        assert_ne!(account_hash("acct_live_secret"), "acct_live_secret");
        assert_eq!(
            account_hash("acct_live_secret"),
            account_hash("acct_live_secret")
        );
    }

    #[test]
    fn written_artifact_has_required_fields_and_no_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let mut artifact = new_live_artifact(
            "openai-official",
            PINNED_OFFICIAL_MODEL,
            "responses",
            LivePath::Proxy,
            Some("acct_live_secret"),
        );
        artifact.connection_ms = Some(12);
        artifact.upstream_first_frame_ms = Some(80);
        artifact.downstream_first_frame_ms = Some(95);
        artifact.completed_ms = Some(400);
        artifact.delta_count = 4;
        artifact.frame_count = 9;
        artifact.usage = Some(json!({"input_tokens": 10, "output_tokens": 4}));
        artifact.outcome = LiveOutcome::Ok;
        artifact.kind = Some("marker".into());
        let path = write_redacted_artifact(dir.path(), "official-proxy.json", &artifact).unwrap();
        let loaded: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        for field in [
            "artifact_commit",
            "binary_commit",
            "binary_sha256",
            "route",
            "catalog_model",
            "wire",
            "path",
            "connection_ms",
            "upstream_first_frame_ms",
            "downstream_first_frame_ms",
            "completed_ms",
            "delta_count",
            "frame_count",
            "usage",
            "outcome",
        ] {
            assert!(loaded.get(field).is_some(), "missing {field}");
        }
        assert_eq!(loaded["catalog_model"], PINNED_OFFICIAL_MODEL);
        assert_eq!(loaded["path"], "proxy");
        assert_artifact_is_redacted(&loaded).unwrap();
        let dumped = loaded.to_string();
        assert!(!dumped.contains("acct_live_secret"));
        assert!(!dumped.contains("developer@example.test"));
        assert!(!dumped.contains("sk-"));
    }

    #[test]
    fn qwen_38_cannot_be_replaced_by_qwen_36() {
        let error = refuse_qwen_standin("qwen3.8", "qwen3.6:27b").unwrap_err();
        assert!(matches!(error, LiveAttributionError::QwenStandIn { .. }));
        let catalog = vec!["qwen3.6:27b".into(), "vlm-e806-qwen3-6".into()];
        assert!(select_qwen_from_models("qwen3.8", &catalog).is_err());
        assert_eq!(
            select_qwen_from_models("qwen3.6:27b", &catalog).unwrap(),
            "qwen3.6:27b"
        );
    }

    #[test]
    fn streaming_requires_a_delta_before_completed_and_rejects_end_flush() {
        assert!(assert_delta_before_terminal(3, Some(40), 400).is_ok());
        assert!(assert_delta_before_terminal(0, None, 400).is_err());
        assert!(assert_delta_before_terminal(2, Some(400), 400).is_err());
        assert!(assert_delta_before_terminal(5, Some(390), 400).is_err());
    }

    fn stream_input(
        output_deltas: u64,
        first_output: Option<u64>,
        terminal: Option<u64>,
    ) -> StreamQualityInput {
        StreamQualityInput {
            streaming_requested: true,
            capability_streaming: true,
            streamed: true,
            output_delta_count: output_deltas,
            first_output_delta_ms: first_output,
            tool_delta_count: 0,
            first_tool_delta_ms: None,
            terminal_ms: terminal,
            reasoning_delta_count: 0,
            first_reasoning_delta_ms: None,
        }
    }

    #[test]
    fn stream_quality_classifies_the_four_categories_and_the_fifty_ms_boundary() {
        // incremental: first output delta > 50ms before terminal.
        assert_eq!(
            classify_stream_quality(&stream_input(3, Some(40), Some(400))),
            StreamQuality::Incremental
        );
        // The 50ms window is flush side: a 51ms lead (349 -> 400) is
        // incremental, an exact 50ms lead is still an end-of-stream flush.
        assert_eq!(
            classify_stream_quality(&stream_input(1, Some(349), Some(400))),
            StreamQuality::Incremental
        );
        // end_flush: first output delta within 50ms of the terminal.
        assert_eq!(
            classify_stream_quality(&stream_input(1, Some(350), Some(400))),
            StreamQuality::EndFlush
        );
        assert_eq!(
            classify_stream_quality(&stream_input(5, Some(390), Some(400))),
            StreamQuality::EndFlush
        );
        assert_eq!(
            classify_stream_quality(&stream_input(2, Some(400), Some(400))),
            StreamQuality::EndFlush
        );
        // buffered: a non-streaming JSON response / declared non-streaming.
        assert_eq!(
            classify_stream_quality(&StreamQualityInput {
                streaming_requested: false,
                capability_streaming: false,
                streamed: false,
                output_delta_count: 0,
                first_output_delta_ms: None,
                tool_delta_count: 0,
                first_tool_delta_ms: None,
                terminal_ms: Some(400),
                reasoning_delta_count: 0,
                first_reasoning_delta_ms: None,
            }),
            StreamQuality::Buffered
        );
        // no_delta: streamed terminal with no output/tool delta, or a stream
        // that silently downgraded to a JSON body.
        assert_eq!(
            classify_stream_quality(&stream_input(0, None, Some(400))),
            StreamQuality::NoDelta
        );
        assert_eq!(
            classify_stream_quality(&StreamQualityInput {
                streaming_requested: true,
                capability_streaming: true,
                streamed: false,
                output_delta_count: 0,
                first_output_delta_ms: None,
                tool_delta_count: 0,
                first_tool_delta_ms: None,
                terminal_ms: Some(200),
                reasoning_delta_count: 0,
                first_reasoning_delta_ms: None,
            }),
            StreamQuality::NoDelta
        );
    }

    #[test]
    fn reasoning_deltas_never_qualify_a_text_stream_and_probe_projects_only_incremental() {
        // A stream with only reasoning deltas and no output delta is no_delta.
        let mut input = stream_input(0, None, Some(400));
        input.reasoning_delta_count = 9;
        input.first_reasoning_delta_ms = Some(10);
        assert_eq!(classify_stream_quality(&input), StreamQuality::NoDelta);

        assert!(streaming_qualifies(StreamQuality::Incremental));
        assert!(!streaming_qualifies(StreamQuality::EndFlush));
        assert!(!streaming_qualifies(StreamQuality::Buffered));
        assert!(!streaming_qualifies(StreamQuality::NoDelta));

        assert!(probe_streaming_projection(StreamQuality::Incremental));
        assert!(!probe_streaming_projection(StreamQuality::EndFlush));
        assert!(!probe_streaming_projection(StreamQuality::Buffered));
        assert!(!probe_streaming_projection(StreamQuality::NoDelta));

        assert!(is_output_delta("response.output_text.delta"));
        assert!(is_output_delta("response.function_call_arguments.delta"));
        assert!(!is_output_delta("response.reasoning_summary_text.delta"));
        assert!(is_reasoning_delta("response.reasoning_summary_text.delta"));
        assert!(is_reasoning_delta("response.reasoning_part.delta"));
        assert!(!is_reasoning_delta("response.output_text.delta"));
    }

    #[test]
    fn tool_call_quality_is_classified_on_arguments_not_commentary() {
        // Commentary text streams incrementally, but the function arguments are
        // a single end-of-stream flush: the tool call is `end_flush`, not
        // `incremental`, so a provider-native argument flush is never mistaken
        // for a Vellum streaming regression.
        let mut input = stream_input(11, Some(40), Some(400));
        input.tool_delta_count = 1;
        input.first_tool_delta_ms = Some(390);
        assert_eq!(classify_stream_quality(&input), StreamQuality::EndFlush);

        // Incremental argument deltas still classify as incremental.
        let mut incremental = stream_input(11, Some(40), Some(400));
        incremental.tool_delta_count = 7;
        incremental.first_tool_delta_ms = Some(40);
        assert_eq!(
            classify_stream_quality(&incremental),
            StreamQuality::Incremental
        );

        assert!(is_tool_delta("response.function_call_arguments.delta"));
        assert!(!is_tool_delta("response.output_text.delta"));
    }

    #[test]
    fn v1_artifact_without_stream_quality_fields_still_reads_as_schema_two() {
        // Older v1 artifacts carry no v2 fields; serde defaults must keep them
        // readable, and new artifacts must mark schema_version 2.
        let v1: Value = serde_json::from_str(
            r#"{"schema_version":1,"artifact_commit":"a","binary_commit":"b",
               "binary_sha256":"c","route":"openai-official","catalog_model":"gpt-5.6-luna",
               "wire":"responses","path":"proxy","delta_count":3,"frame_count":5,
               "outcome":"ok","account_hash":"x"}"#,
        )
        .unwrap();
        let artifact: LiveRequestArtifact = serde_json::from_value(v1).unwrap();
        assert_eq!(artifact.schema_version, 1);
        assert_eq!(artifact.stream_quality, None);
        assert_eq!(artifact.output_delta_count, 0);
        assert_eq!(artifact.reasoning_delta_count, 0);

        let fresh = new_live_artifact(
            "openai-official",
            PINNED_OFFICIAL_MODEL,
            "responses",
            LivePath::Proxy,
            None,
        );
        assert_eq!(fresh.schema_version, 2);
        let raw: Value = serde_json::to_value(&fresh).unwrap();
        assert_eq!(raw["schema_version"], 2);
        for field in [
            "stream_quality",
            "first_output_delta_ms",
            "first_reasoning_delta_ms",
            "output_delta_count",
            "reasoning_delta_count",
        ] {
            assert!(raw.get(field).is_some(), "missing {field} on a v2 artifact");
        }
    }

    #[test]
    fn grok_426_is_upgrade_required() {
        assert!(is_upgrade_required(426));
        assert_eq!(classify_http_status(426), "provider_protocol");
        assert!(assert_no_upgrade_required(Some(426)).is_err());
        assert!(assert_no_upgrade_required(Some(200)).is_ok());
    }

    #[test]
    fn opencode_plans_are_per_model_and_isolated() {
        let chat = OpenCodeModelCaps {
            catalog_id: "oc-chat".into(),
            wire: "chat".into(),
            streaming: true,
            tools: false,
            enabled: true,
        };
        let responses = OpenCodeModelCaps {
            catalog_id: "oc-responses".into(),
            wire: "responses".into(),
            streaming: true,
            tools: true,
            enabled: true,
        };
        let disabled = OpenCodeModelCaps {
            catalog_id: "oc-off".into(),
            wire: "chat".into(),
            streaming: true,
            tools: true,
            enabled: false,
        };
        let chat_plan = plan_opencode_model(&chat).unwrap();
        let responses_plan = plan_opencode_model(&responses).unwrap();
        assert!(plan_opencode_model(&disabled).is_none());
        assert!(chat_plan.kinds.contains(&LiveKind::Marker));
        assert!(!chat_plan.kinds.contains(&LiveKind::TypedTool));
        assert_eq!(chat_plan.fallback.as_deref(), Some("route_fallback"));
        assert!(responses_plan.kinds.contains(&LiveKind::TypedTool));
        assert!(responses_plan.fallback.is_none());
        assert_opencode_plans_isolated(&[chat_plan, responses_plan]).unwrap();
    }

    fn ab_all_ok() -> AbComparisonInput {
        AbComparisonInput {
            direct_cold: AbStageTiming::ok(400),
            direct_warm: AbStageTiming::ok(100),
            direct_idle: AbStageTiming::ok(100),
            proxy_cold: AbStageTiming::ok(450),
            proxy_warm: AbStageTiming::ok(120),
            proxy_idle: AbStageTiming::ok(130),
        }
    }

    #[test]
    fn ab_classification_row1_both_fail_is_provider_or_upstream() {
        let mut input = ab_all_ok();
        input.direct_cold = AbStageTiming::fail();
        input.proxy_cold = AbStageTiming::fail();
        assert_eq!(
            classify_official_ab(&input),
            AbVerdict::ProviderOrUpstreamEnvironmentIssue
        );
    }

    #[test]
    fn ab_classification_row2_direct_ok_proxy_fail_is_vellum_issue() {
        let mut input = ab_all_ok();
        input.proxy_warm = AbStageTiming::fail();
        assert_eq!(
            classify_official_ab(&input),
            AbVerdict::VellumHandshakeAuthRoutingOrBridgeIssue
        );
    }

    #[test]
    fn ab_classification_row3_cold_over_budget_warm_ok_is_connection_path_issue() {
        let mut input = ab_all_ok();
        input.direct_cold = AbStageTiming::ok(100);
        input.proxy_cold = AbStageTiming::ok(600); // 500 extra > max(250, 20)
        input.direct_warm = AbStageTiming::ok(100);
        input.proxy_warm = AbStageTiming::ok(150); // 50 extra <= 250, within budget
        assert_eq!(
            classify_official_ab(&input),
            AbVerdict::ConnectionCatalogOrAuthPathIssue
        );
    }

    #[test]
    fn ab_classification_row4_warm_over_budget_is_application_bridge_issue() {
        let mut input = ab_all_ok();
        input.direct_warm = AbStageTiming::ok(100);
        input.proxy_warm = AbStageTiming::ok(600); // 500 extra > budget
        assert_eq!(
            classify_official_ab(&input),
            AbVerdict::ApplicationBridgeSchedulingOrPersistenceIssue
        );
    }

    #[test]
    fn ab_classification_row5_direct_idle_ok_proxy_idle_fail_is_vellum_control_frame_issue() {
        let mut input = ab_all_ok();
        input.proxy_idle = AbStageTiming::fail();
        assert_eq!(
            classify_official_ab(&input),
            AbVerdict::VellumControlFrameOrIdleHandlingIssue
        );
    }

    #[test]
    fn ab_classification_row6_both_idle_fail_is_upstream_idle_policy_issue() {
        let mut input = ab_all_ok();
        input.direct_idle = AbStageTiming::fail();
        input.proxy_idle = AbStageTiming::fail();
        assert_eq!(
            classify_official_ab(&input),
            AbVerdict::UpstreamIdlePolicyOrEnvironmentIssue
        );
    }

    #[test]
    fn ab_classification_row7_all_three_stages_ok_within_budget_closes_official() {
        assert_eq!(
            classify_official_ab(&ab_all_ok()),
            AbVerdict::OfficialIssueClosedNoVellumProblem
        );
    }

    #[test]
    fn ab_ledger_reads_upstream_and_downstream_as_distinct_stages() {
        use crate::usage::UsageRecord;
        let records = vec![
            UsageRecord {
                request_id: Some("req-a".into()),
                connection_id: Some("conn-a".into()),
                first_event_ms: Some(80),
                first_downstream_frame_ms: Some(95),
                ..Default::default()
            },
            UsageRecord {
                request_id: Some("req-b".into()),
                connection_id: Some("conn-a".into()),
                first_event_ms: Some(40),
                first_downstream_frame_ms: Some(41),
                ..Default::default()
            },
        ];
        let frame = ledger_frame_for_request(&records, "req-a");
        assert_eq!(frame.upstream_first_frame_ms, Some(80));
        assert_eq!(frame.downstream_first_frame_ms, Some(95));
        // The two ledger fields must never collapse to the same value by
        // construction: they are read from two different UsageRecord
        // fields, not one client timestamp copied twice.
        assert_ne!(
            frame.upstream_first_frame_ms,
            frame.downstream_first_frame_ms
        );
        assert_eq!(frame.connection_id.as_deref(), Some("conn-a"));
        assert_eq!(frame.usage_record_count, 1);

        let missing = ledger_frame_for_request(&records, "req-does-not-exist");
        assert_eq!(missing, LedgerFrame::default());
    }

    #[test]
    fn apply_ledger_frame_computes_bridge_delay_and_cold_total_from_the_ledger() {
        let mut artifact = new_live_artifact(
            "openai-official",
            PINNED_OFFICIAL_MODEL,
            "responses",
            LivePath::Proxy,
            None,
        );
        artifact.stage = Some(AbStage::Cold);
        artifact.connection_ms = Some(30);
        artifact.turn_first_frame_ms = Some(70);
        let ledger = LedgerFrame {
            upstream_first_frame_ms: Some(50),
            downstream_first_frame_ms: Some(66),
            connection_id: Some("conn-xyz".into()),
            usage_record_count: 1,
        };
        apply_ledger_frame(&mut artifact, &ledger);
        assert_eq!(artifact.upstream_first_frame_ms, Some(50));
        assert_eq!(artifact.downstream_first_frame_ms, Some(66));
        assert_eq!(artifact.bridge_delay_ms, Some(16));
        assert_eq!(artifact.cold_total_first_frame_ms, Some(100));
        assert_eq!(artifact.connection_id.as_deref(), Some("conn-xyz"));
        assert_eq!(artifact.usage_record_count, Some(1));

        // Warm/Idle never get a cold_total, even with the same ledger.
        artifact.stage = Some(AbStage::Warm);
        artifact.cold_total_first_frame_ms = None;
        apply_ledger_frame(&mut artifact, &ledger);
        assert_eq!(artifact.cold_total_first_frame_ms, None);
    }

    #[test]
    fn ab_artifact_with_stage_and_connection_id_still_redacts_clean() {
        let mut artifact = new_live_artifact(
            "openai-official",
            PINNED_OFFICIAL_MODEL,
            "responses",
            LivePath::Direct,
            Some("acct_ab_secret"),
        );
        artifact.stage = Some(AbStage::Idle);
        artifact.connection_id = Some("conn-live-ab-1".into());
        artifact.turn_first_frame_ms = Some(42);
        artifact.outcome = LiveOutcome::Ok;
        let redacted = artifact_to_redacted_json(&artifact).unwrap();
        assert_eq!(redacted["stage"], "idle");
        assert_eq!(redacted["connection_id"], "conn-live-ab-1");
        assert_artifact_is_redacted(&redacted).unwrap();
        let dumped = redacted.to_string();
        assert!(!dumped.contains('@'));
        assert!(!dumped.contains("acct_ab_secret"));

        // A connection id that accidentally carries an email must still be
        // caught by the generic redaction walk, not silently pass through.
        artifact.connection_id = Some("leak@example.com".into());
        let raw = serde_json::to_value(&artifact).unwrap();
        let redacted_leak = redact_live_artifact(&raw);
        assert!(!redacted_leak.to_string().contains('@'));
    }
}
