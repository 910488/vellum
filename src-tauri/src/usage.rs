use crate::error::{AppError, AppResult};
use crate::model::{
    CompactionLogEntry, ProviderUsage, RequestLog, RequestLogEntry, RequestLogRouteOption,
    ReviewProviderStat, ReviewStats, RouteTelemetry, SubagentLinkConfidence, SubagentLogEntry,
    SubagentRun, SubagentRunState,
};
use crate::review::AUTO_REVIEW_MODEL;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::PathBuf;
use std::sync::Mutex;

/// One page of the request log. Event lists stay independently capped so
/// Start/Stop timing is not coupled to how many request rows the UI shows.
#[derive(Debug, Clone)]
pub struct RequestLogPage {
    pub limit: u32,
    pub offset: u32,
    pub failed_only: bool,
    pub route_id: Option<String>,
    pub event_limit: u32,
    pub focus_entry_id: Option<i64>,
}

impl Default for RequestLogPage {
    fn default() -> Self {
        Self {
            limit: 20,
            offset: 0,
            failed_only: false,
            route_id: None,
            event_limit: 500,
            focus_entry_id: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct UsageRecord {
    pub route_id: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub tool_calls: u64,
    pub compaction_tokens: u64,
    pub review_tokens: u64,
    pub status: u16,
    pub error: Option<String>,
    pub duration_ms: u64,
    pub first_byte_ms: Option<u64>,
    pub review_run_id: Option<String>,
    pub review_role: Option<String>,
    pub review_reason: Option<String>,
    pub request_id: Option<String>,
    pub connection_id: Option<String>,
    pub conversation_identity: Option<String>,
    pub first_event_ms: Option<u64>,
    pub first_downstream_frame_ms: Option<u64>,
    pub outcome: Option<String>,
    pub error_category: Option<String>,
    pub stage_times_ms: Option<String>,
    /// Stream-quality classification (incremental / end_flush / buffered /
    /// no_delta). `None` on rows whose dispatch did not observe deltas.
    pub stream_quality: Option<String>,
    /// Request-relative time of the first application output delta.
    pub first_output_delta_ms: Option<u64>,
    /// Request-relative time of the first explicit reasoning delta.
    pub first_reasoning_delta_ms: Option<u64>,
    /// Application output delta count.
    pub output_delta_count: u64,
    /// Explicit reasoning delta count, recorded independently.
    pub reasoning_delta_count: u64,
    pub control_account_hash: Option<String>,
    pub execution_account_hash: Option<String>,
    pub selection_revision: Option<u64>,
    pub auth_mode: Option<String>,
    pub provider_profile: Option<String>,
    pub upstream_attempted: Option<bool>,
    pub retry_after: Option<u64>,
}

/// Legacy translated-harness usage ledger.
///
/// Nothing writes this any more: its only writer was the retired Desktop
/// provider pipeline, deleted with the Vellum Canonical surface. The reader,
/// the schema and the table are kept so an existing database still opens and
/// its rows stay readable. The live shared-runtime path never wrote here;
/// parent/child diagnostics live in `diagnostic_events` / `subagent_events`.
/// This is not production attribution, and it is not the in-memory JSON from
/// `harness/multi_agent.rs` `usage_attribution()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessUsageRecord {
    pub route_id: String,
    pub provider: String,
    pub session_hash: String,
    pub agent_id: String,
    pub parent_agent_id: Option<String>,
    pub role: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub status: u16,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default)]
pub struct UsageSummary {
    pub latest_input_tokens: u64,
    pub turns: u32,
    pub total_tokens: u64,
    pub latest_first_byte_ms: Option<u32>,
    pub trend: Vec<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalUsageDay {
    pub date: String,
    pub route_id: String,
    pub provider: String,
    pub tokens: u64,
    pub requests: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalProviderTotal {
    pub route_id: String,
    pub provider: String,
    pub tokens: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalUsageActivity {
    pub days: Vec<LocalUsageDay>,
    pub providers: Vec<LocalProviderTotal>,
    pub longest_request_ms: u64,
}

/// How long a run may stay non-terminal (`requested`/`running`, or a
/// no-child `spawn_requested` with no sibling ambiguity) before the
/// aggregation gives up waiting and calls it `failed`. Mirrors the shared
/// runtime's own subagent correlation window (`SUBAGENT_LINK_WINDOW` in
/// `crates/vellum-proxy-runtime/src/exec.rs`, 900s): once the runtime itself
/// has stopped trying to link new child turns to a spawn, a run claiming
/// "still going" would be misleading.
const SUBAGENT_RUN_BOUND_SECS: i64 = 900;

/// One raw row from `subagent_events`, trimmed to the columns the run
/// aggregation needs. `id` and `link_method` are not read here; the log
/// listing (`SubagentLogEntry`) already exposes the untrimmed feed.
struct RawSubagentRow {
    created_at: i64,
    kind: String,
    call_id: Option<String>,
    parent_request_id: Option<String>,
    child_request_id: Option<String>,
    child_model: Option<String>,
    child_effort: Option<String>,
    link_confidence: Option<String>,
    payload: String,
}

/// A child request's own `request_usage` row, joined by `request_id`. This —
/// not the mere existence of a `SpawnCompleted` event — is what decides
/// whether a run actually succeeded.
struct ChildUsageRow {
    id: i64,
    status: u16,
    error: Option<String>,
    outcome: Option<String>,
    error_category: Option<String>,
    duration_ms: Option<u64>,
    model: Option<String>,
    route_id: Option<String>,
}

/// One authoritative native Codex parent/child edge. Native V2 emits this
/// instead of the legacy spawn-requested/child-turn/spawn-completed trio.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeSubagentGraphEdge {
    parent_thread_id: String,
    child_thread_id: String,
    confidence: String,
    #[serde(skip)]
    created_at: i64,
}

#[derive(Debug, Clone)]
struct NativeThreadUsageRow {
    id: i64,
    status: u16,
    error: Option<String>,
    outcome: Option<String>,
    error_category: Option<String>,
    duration_ms: Option<u64>,
    model: Option<String>,
    route_id: Option<String>,
    created_at: i64,
}

/// The (at most) three raw rows sharing one `call_id`: the parent's spawn
/// tool call, the linked child turn (if Vellum found exactly one confident
/// match), and the parent's own completion signal.
#[derive(Default)]
struct CallGroup {
    requested: Option<RawSubagentRow>,
    child: Option<RawSubagentRow>,
    completed: Option<RawSubagentRow>,
}

impl CallGroup {
    fn requested_at(&self) -> i64 {
        self.requested
            .as_ref()
            .map(|row| row.created_at)
            .or_else(|| self.child.as_ref().map(|row| row.created_at))
            .or_else(|| self.completed.as_ref().map(|row| row.created_at))
            .unwrap_or(0)
    }

    fn child_request_id(&self) -> Option<String> {
        self.child
            .as_ref()
            .and_then(|row| row.child_request_id.clone())
            .or_else(|| {
                self.completed
                    .as_ref()
                    .and_then(|row| row.child_request_id.clone())
            })
    }

    fn parent_request_id(&self) -> Option<String> {
        self.requested
            .as_ref()
            .and_then(|row| row.parent_request_id.clone())
            .or_else(|| {
                self.child
                    .as_ref()
                    .and_then(|row| row.parent_request_id.clone())
            })
    }

    fn link_confidence(&self) -> SubagentLinkConfidence {
        match self
            .child
            .as_ref()
            .and_then(|row| row.link_confidence.as_deref())
        {
            // `high` is an exact content-hash match of the spawned prompt
            // (`SubagentLinkMethod::ContentHash`); everything else that still
            // produced a link (`medium` = configured-model match, `low` =
            // solitary time-window) is a heuristic, not a certainty.
            Some("high") => SubagentLinkConfidence::Exact,
            Some("medium") | Some("low") => SubagentLinkConfidence::Heuristic,
            _ => SubagentLinkConfidence::Unlinked,
        }
    }

    /// Resolve this group's final [`SubagentRun`]. `usage` is the already
    /// joined child outcome (`None` if no confident link exists, or the
    /// child's own row has not landed yet); `ambiguous` was decided by the
    /// caller, which alone can see this group's siblings.
    #[allow(clippy::too_many_arguments)]
    fn resolve_run(
        &self,
        call_id: String,
        child_request_id: Option<String>,
        usage: Option<&ChildUsageRow>,
        stale: bool,
        ambiguous: bool,
        parent_usage_entry_id: Option<i64>,
        child_usage_entry_id: Option<i64>,
    ) -> SubagentRun {
        let is_success = usage.is_some_and(|value| {
            matches!(value.outcome.as_deref(), Some("success"))
                || (value.outcome.is_none()
                    && (200..300).contains(&value.status)
                    && value.error.is_none())
        });
        let is_cancelled = usage.is_some_and(|value| {
            matches!(
                value.outcome.as_deref(),
                Some("client_cancel") | Some("client_disconnect")
            )
        });
        let is_failure = usage.is_some_and(|value| {
            !is_success
                && !is_cancelled
                && (matches!(
                    value.outcome.as_deref(),
                    Some("provider_failure") | Some("protocol_failure")
                ) || value.status >= 400
                    || value.error.is_some())
        });

        let state = if usage.is_some() {
            if is_cancelled {
                SubagentRunState::Cancelled
            } else if is_success {
                if self.completed.is_some() {
                    SubagentRunState::Completed
                } else if stale {
                    // The child itself succeeded, but the parent never
                    // observed a matching completion within the correlation
                    // window — a protocol-level gap, not a success to claim.
                    SubagentRunState::Failed
                } else {
                    SubagentRunState::Running
                }
            } else if is_failure {
                SubagentRunState::Failed
            } else {
                SubagentRunState::Running
            }
        } else if child_request_id.is_some() {
            // Linked, but the child's own usage row has not landed yet.
            if self.completed.is_some() {
                // The parent already saw a completion signal for a child we
                // cannot yet confirm the outcome of — never claim success.
                SubagentRunState::Unlinked
            } else {
                SubagentRunState::Running
            }
        } else if ambiguous {
            SubagentRunState::Ambiguous
        } else if self.completed.is_some() {
            SubagentRunState::Unlinked
        } else if stale {
            SubagentRunState::Failed
        } else {
            SubagentRunState::Requested
        };

        let requested_model = self
            .requested
            .as_ref()
            .and_then(|row| row.child_model.clone());
        let requested_effort = self
            .requested
            .as_ref()
            .and_then(|row| row.child_effort.clone());
        let model = self
            .child
            .as_ref()
            .and_then(|row| row.child_model.clone())
            .or_else(|| usage.and_then(|value| value.model.clone()))
            .or(requested_model);
        let effort = self
            .child
            .as_ref()
            .and_then(|row| row.child_effort.clone())
            .or(requested_effort);
        let route_id = usage.and_then(|value| value.route_id.clone()).or_else(|| {
            self.child
                .as_ref()
                .and_then(|row| route_id_from_payload(&row.payload))
        });
        let duration_ms = usage.and_then(|value| value.duration_ms).or_else(|| {
            self.completed
                .as_ref()
                .and_then(|row| duration_ms_from_payload(&row.payload))
        });

        let parent_request_id = self.parent_request_id();
        let parent_request_hash = parent_request_id
            .as_deref()
            .map(vellum_proxy_runtime::hash_text);
        let child_request_hash = child_request_id
            .as_deref()
            .map(vellum_proxy_runtime::hash_text);

        SubagentRun {
            call_id: Some(call_id),
            state,
            parent_request_id,
            child_request_id,
            parent_request_hash,
            child_request_hash,
            parent_usage_entry_id,
            child_usage_entry_id,
            route_id,
            model,
            effort,
            link_confidence: self.link_confidence(),
            requested_at: self.requested_at(),
            completed_at: self.completed.as_ref().map(|row| row.created_at),
            duration_ms,
            outcome: usage.and_then(|value| value.outcome.clone()),
            error_category: usage.and_then(|value| value.error_category.clone()),
        }
    }
}

fn route_id_from_payload(payload: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|value| {
            value
                .get("routeId")
                .and_then(|v| v.as_str().map(str::to_string))
        })
}

fn duration_ms_from_payload(payload: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|value| value.get("durationMs").and_then(serde_json::Value::as_u64))
}

pub struct UsageStore {
    connection: Mutex<Connection>,
}

impl UsageStore {
    pub fn open(path: PathBuf) -> AppResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| AppError::Message(format!("無法建立統計目錄：{error}")))?;
        }
        let connection = Connection::open(path)
            .map_err(|error| AppError::Message(format!("無法開啟使用統計：{error}")))?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| AppError::Message(format!("configure usage journal: {error}")))?;
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(|error| AppError::Message(format!("configure usage sync mode: {error}")))?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS request_usage (
                    id             INTEGER PRIMARY KEY AUTOINCREMENT,
                    route_id       TEXT NOT NULL,
                    provider       TEXT NOT NULL,
                    model          TEXT NOT NULL,
                    input_tokens   INTEGER NOT NULL,
                    output_tokens  INTEGER NOT NULL,
                    status         INTEGER NOT NULL,
                    error          TEXT,
                    duration_ms    INTEGER NOT NULL,
                    first_byte_ms  INTEGER,
                    created_at     INTEGER NOT NULL DEFAULT (unixepoch())
                 );
                 CREATE INDEX IF NOT EXISTS ix_usage_route_created
                   ON request_usage(route_id, created_at);
                 -- Legacy ledger, no writer left. Kept so existing databases
                 -- still open. Not live diagnostics.
                 CREATE TABLE IF NOT EXISTS harness_usage_attribution (
                    id               INTEGER PRIMARY KEY AUTOINCREMENT,
                    route_id         TEXT NOT NULL,
                    provider         TEXT NOT NULL,
                    session_hash     TEXT NOT NULL,
                    agent_id         TEXT NOT NULL,
                    parent_agent_id  TEXT,
                    role             TEXT NOT NULL,
                    model            TEXT NOT NULL,
                    input_tokens     INTEGER NOT NULL,
                    output_tokens    INTEGER NOT NULL,
                    status           INTEGER NOT NULL,
                    duration_ms      INTEGER NOT NULL,
                    created_at       INTEGER NOT NULL DEFAULT (unixepoch())
                 );
                 CREATE INDEX IF NOT EXISTS ix_harness_usage_session
                   ON harness_usage_attribution(session_hash, id);
                 CREATE TABLE IF NOT EXISTS diagnostic_events (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    kind        TEXT NOT NULL,
                    request_id  TEXT,
                    route_id    TEXT,
                    payload     TEXT NOT NULL,
                    created_at  INTEGER NOT NULL DEFAULT (unixepoch())
                 );
                 CREATE INDEX IF NOT EXISTS ix_diag_kind_created
                   ON diagnostic_events(kind, created_at);
                 CREATE INDEX IF NOT EXISTS ix_diag_request
                   ON diagnostic_events(request_id);
                 CREATE TABLE IF NOT EXISTS subagent_events (
                    id                INTEGER PRIMARY KEY AUTOINCREMENT,
                    kind              TEXT NOT NULL,
                    call_id           TEXT,
                    parent_request_id TEXT,
                    child_request_id  TEXT,
                    child_model       TEXT,
                    child_effort      TEXT,
                    link_method       TEXT,
                    link_confidence   TEXT,
                    payload           TEXT NOT NULL,
                    created_at        INTEGER NOT NULL DEFAULT (unixepoch())
                 );
                 CREATE INDEX IF NOT EXISTS ix_subagent_call
                   ON subagent_events(call_id, id);",
            )
            .map_err(|error| AppError::Message(format!("無法建立使用統計表：{error}")))?;
        for migration in [
            "ALTER TABLE request_usage ADD COLUMN review_run_id TEXT",
            "ALTER TABLE request_usage ADD COLUMN review_role TEXT",
            "ALTER TABLE request_usage ADD COLUMN review_reason TEXT",
            "ALTER TABLE request_usage ADD COLUMN cached_input_tokens INTEGER",
            "ALTER TABLE request_usage ADD COLUMN reasoning_tokens INTEGER",
            "ALTER TABLE request_usage ADD COLUMN tool_calls INTEGER",
            "ALTER TABLE request_usage ADD COLUMN compaction_tokens INTEGER",
            "ALTER TABLE request_usage ADD COLUMN review_tokens INTEGER",
            "ALTER TABLE request_usage ADD COLUMN request_id TEXT",
            "ALTER TABLE request_usage ADD COLUMN connection_id TEXT",
            "ALTER TABLE request_usage ADD COLUMN conversation_identity TEXT",
            "ALTER TABLE request_usage ADD COLUMN first_event_ms INTEGER",
            "ALTER TABLE request_usage ADD COLUMN first_downstream_frame_ms INTEGER",
            "ALTER TABLE request_usage ADD COLUMN outcome TEXT",
            "ALTER TABLE request_usage ADD COLUMN error_category TEXT",
            "ALTER TABLE request_usage ADD COLUMN stage_times_ms TEXT",
            "ALTER TABLE request_usage ADD COLUMN stream_quality TEXT",
            "ALTER TABLE request_usage ADD COLUMN first_output_delta_ms INTEGER",
            "ALTER TABLE request_usage ADD COLUMN first_reasoning_delta_ms INTEGER",
            "ALTER TABLE request_usage ADD COLUMN output_delta_count INTEGER",
            "ALTER TABLE request_usage ADD COLUMN reasoning_delta_count INTEGER",
            "ALTER TABLE request_usage ADD COLUMN control_account_hash TEXT",
            "ALTER TABLE request_usage ADD COLUMN execution_account_hash TEXT",
            "ALTER TABLE request_usage ADD COLUMN selection_revision INTEGER",
            "ALTER TABLE request_usage ADD COLUMN auth_mode TEXT",
            "ALTER TABLE request_usage ADD COLUMN provider_profile TEXT",
            "ALTER TABLE request_usage ADD COLUMN upstream_attempted INTEGER",
            "ALTER TABLE request_usage ADD COLUMN retry_after INTEGER",
            "ALTER TABLE request_usage ADD COLUMN agent_attribution_json TEXT",
        ] {
            // Duplicate-column means the migration was already applied.
            let _ = connection.execute(migration, []);
        }
        connection
            .execute(
                "CREATE INDEX IF NOT EXISTS ix_usage_review_run
                   ON request_usage(review_run_id, id)",
                [],
            )
            .map_err(|error| AppError::Message(format!("create review usage index: {error}")))?;
        connection
            .execute(
                "CREATE INDEX IF NOT EXISTS ix_usage_connection
                   ON request_usage(connection_id, id)",
                [],
            )
            .map_err(|error| {
                AppError::Message(format!("create connection usage index: {error}"))
            })?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn record(&self, value: &UsageRecord) -> AppResult<()> {
        self.record_with_agent_attribution(value, None)
    }

    /// Persist a shared-runtime usage row together with its optional Codex
    /// thread/agent attribution. Legacy Desktop callers continue through
    /// [`Self::record`] and leave the new column empty.
    pub fn record_with_agent_attribution(
        &self,
        value: &UsageRecord,
        agent_attribution_json: Option<&str>,
    ) -> AppResult<()> {
        self.connection
            .lock()
            .expect("usage db poisoned")
            .execute(
                "INSERT INTO request_usage
                   (route_id, provider, model, input_tokens, output_tokens,
                    cached_input_tokens, reasoning_tokens, tool_calls,
                    compaction_tokens, review_tokens, status, error,
                    duration_ms, first_byte_ms, review_run_id, review_role,
                    review_reason, request_id, connection_id,
                    conversation_identity, first_event_ms,
                    first_downstream_frame_ms, outcome, error_category,
                    stage_times_ms, stream_quality, first_output_delta_ms,
                    first_reasoning_delta_ms, output_delta_count,
                    reasoning_delta_count, control_account_hash,
                    execution_account_hash, selection_revision, auth_mode,
                    provider_profile, upstream_attempted, retry_after,
                    agent_attribution_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                         ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22,
                         ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32,
                         ?33, ?34, ?35, ?36, ?37, ?38)",
                params![
                    value.route_id,
                    value.provider,
                    value.model,
                    value.input_tokens,
                    value.output_tokens,
                    value.cached_input_tokens,
                    value.reasoning_tokens,
                    value.tool_calls,
                    value.compaction_tokens,
                    value.review_tokens,
                    value.status,
                    value.error,
                    value.duration_ms,
                    value.first_byte_ms,
                    value.review_run_id,
                    value.review_role,
                    value.review_reason,
                    value.request_id,
                    value.connection_id,
                    value.conversation_identity,
                    value.first_event_ms,
                    value.first_downstream_frame_ms,
                    value.outcome,
                    value.error_category,
                    value.stage_times_ms,
                    value.stream_quality,
                    value.first_output_delta_ms,
                    value.first_reasoning_delta_ms,
                    value.output_delta_count,
                    value.reasoning_delta_count,
                    value.control_account_hash,
                    value.execution_account_hash,
                    value.selection_revision.map(|value| value as i64),
                    value.auth_mode,
                    value.provider_profile,
                    value.upstream_attempted.map(i64::from),
                    value.retry_after.map(|value| value as i64),
                    agent_attribution_json
                ],
            )
            .map_err(|error| AppError::Message(format!("寫入使用統計失敗：{error}")))?;
        Ok(())
    }

    pub fn mark_first_downstream_frame(&self, request_id: &str, elapsed_ms: u64) -> AppResult<()> {
        self.connection
            .lock()
            .expect("usage db poisoned")
            .execute(
                "UPDATE request_usage
                    SET first_downstream_frame_ms = ?1
                  WHERE id = (
                    SELECT id FROM request_usage
                     WHERE request_id = ?2
                     ORDER BY id DESC
                     LIMIT 1
                  )
                    AND first_downstream_frame_ms IS NULL",
                params![elapsed_ms as i64, request_id],
            )
            .map_err(|error| AppError::Message(format!("更新下游時間失敗：{error}")))?;
        Ok(())
    }

    /// Persist one structured diagnostic event. `payload` is the full JSON
    /// event; `kind` and `request_id` are projected into indexed columns so
    /// the two acceptance queries stay simple.
    pub fn record_diagnostic_event(
        &self,
        event: &vellum_proxy_runtime::DiagnosticEvent,
    ) -> AppResult<()> {
        let value = serde_json::to_value(event)
            .map_err(|error| AppError::Message(format!("serialize diagnostic event: {error}")))?;
        let kind = value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let request_id = value
            .get("request_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let payload = serde_json::to_string(&value)
            .map_err(|error| AppError::Message(format!("encode diagnostic event: {error}")))?;
        let connection = self.connection.lock().expect("usage db poisoned");
        connection
            .execute(
                "INSERT INTO diagnostic_events (kind, request_id, route_id, payload)
                 VALUES (?1, ?2, NULL, ?3)",
                params![kind, request_id, payload],
            )
            .map_err(|error| AppError::Message(format!("record diagnostic event: {error}")))?;
        let id = connection.last_insert_rowid();
        if id % 256 == 0 {
            self.evict_diagnostics_inner(&connection)?;
        }
        if matches!(
            event,
            vellum_proxy_runtime::DiagnosticEvent::SpawnRequested(_)
                | vellum_proxy_runtime::DiagnosticEvent::ChildTurn(_)
                | vellum_proxy_runtime::DiagnosticEvent::SpawnCompleted(_)
        ) {
            self.record_subagent_event_inner(&connection, event, kind, payload)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn record_subagent_event_inner(
        &self,
        connection: &Connection,
        event: &vellum_proxy_runtime::DiagnosticEvent,
        kind: String,
        payload: String,
    ) -> AppResult<()> {
        let (call_id, parent_request_id, child_request_id, child_model, child_effort) = match event
        {
            vellum_proxy_runtime::DiagnosticEvent::SpawnRequested(value) => (
                Some(value.call_id.clone()),
                Some(value.parent_request_id.clone()),
                None,
                value.model.clone(),
                value.reasoning_effort.clone(),
            ),
            vellum_proxy_runtime::DiagnosticEvent::ChildTurn(value) => (
                value.call_id.clone(),
                value.parent_request_id.clone(),
                Some(value.request_id.clone()),
                Some(value.model.clone()),
                value.effort.clone(),
            ),
            vellum_proxy_runtime::DiagnosticEvent::SpawnCompleted(value) => (
                Some(value.call_id.clone()),
                None,
                value.child_request_id.clone(),
                None,
                None,
            ),
            _ => return Ok(()),
        };
        let (link_method, link_confidence) = match event {
            vellum_proxy_runtime::DiagnosticEvent::ChildTurn(value) => {
                let method = serde_json::to_value(value.link_method)
                    .ok()
                    .and_then(|encoded| encoded.as_str().map(str::to_string));
                let confidence = serde_json::to_value(value.link_confidence)
                    .ok()
                    .and_then(|encoded| encoded.as_str().map(str::to_string));
                (method, confidence)
            }
            _ => (None, None),
        };
        connection
            .execute(
                "INSERT INTO subagent_events
                   (kind, call_id, parent_request_id, child_request_id,
                    child_model, child_effort, link_method, link_confidence, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    kind,
                    call_id,
                    parent_request_id,
                    child_request_id,
                    child_model,
                    child_effort,
                    link_method,
                    link_confidence,
                    payload
                ],
            )
            .map_err(|error| AppError::Message(format!("record subagent event: {error}")))?;
        Ok(())
    }

    fn evict_diagnostics_inner(&self, connection: &Connection) -> AppResult<()> {
        let cutoff = crate::history::DEFAULT_RETENTION_DAYS.saturating_mul(86_400) as i64;
        connection
            .execute(
                "DELETE FROM diagnostic_events WHERE created_at < unixepoch() - ?1",
                [cutoff],
            )
            .map_err(|error| AppError::Message(format!("evict diagnostic events: {error}")))?;
        connection
            .execute(
                "DELETE FROM subagent_events WHERE created_at < unixepoch() - ?1",
                [cutoff],
            )
            .map_err(|error| AppError::Message(format!("evict subagent events: {error}")))?;
        // E-8: request_usage previously grew forever. Reuse the same retention
        // so the default configuration is strictly cheaper than the old one.
        connection
            .execute(
                "DELETE FROM request_usage WHERE created_at < unixepoch() - ?1",
                [cutoff],
            )
            .map_err(|error| AppError::Message(format!("evict request usage: {error}")))?;
        // Bounded payload guard: keep the newest 100_000 diagnostic rows. The
        // T1 summary is ~1-3 KB, so this is comfortably below the 64 MB target
        // while still preserving a large diagnostic window.
        connection
            .execute(
                "DELETE FROM diagnostic_events
                 WHERE id NOT IN (
                    SELECT id FROM diagnostic_events ORDER BY id DESC LIMIT 100000
                 )",
                [],
            )
            .map_err(|error| AppError::Message(format!("trim diagnostic events: {error}")))?;
        self.evict_t3_payloads_inner(connection)?;
        Ok(())
    }

    /// T3 bodies cannot outlive the shipped capture window. Age uses
    /// [`vellum_proxy_runtime::T3_MAX_AGE`]; volume uses
    /// [`vellum_proxy_runtime::T3_MAX_BYTES`].
    fn evict_t3_payloads_inner(&self, connection: &Connection) -> AppResult<()> {
        let cutoff = vellum_proxy_runtime::T3_MAX_AGE.as_secs() as i64;
        connection
            .execute(
                "DELETE FROM diagnostic_events
                 WHERE created_at < unixepoch() - ?1
                   AND instr(payload, '\"fullContext\"') > 0",
                [cutoff],
            )
            .map_err(|error| AppError::Message(format!("evict aged T3 payloads: {error}")))?;

        let mut total = connection
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(payload)), 0)
                 FROM diagnostic_events
                 WHERE instr(payload, '\"fullContext\"') > 0",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0) as u64;
        while total >= vellum_proxy_runtime::T3_MAX_BYTES {
            let deleted = connection
                .execute(
                    "DELETE FROM diagnostic_events
                     WHERE id = (
                        SELECT id FROM diagnostic_events
                         WHERE instr(payload, '\"fullContext\"') > 0
                         ORDER BY id ASC
                         LIMIT 1
                     )",
                    [],
                )
                .map_err(|error| AppError::Message(format!("trim T3 volume: {error}")))?;
            if deleted == 0 {
                break;
            }
            total = connection
                .query_row(
                    "SELECT COALESCE(SUM(LENGTH(payload)), 0)
                     FROM diagnostic_events
                     WHERE instr(payload, '\"fullContext\"') > 0",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0) as u64;
        }
        Ok(())
    }

    /// Persist one row in the legacy harness ledger.
    ///
    /// Retained for its tests only — the pipeline that called this is gone,
    /// and the shared-runtime diagnostics path never used it.
    pub fn record_harness_attribution(&self, value: &HarnessUsageRecord) -> AppResult<()> {
        self.connection
            .lock()
            .expect("usage db poisoned")
            .execute(
                "INSERT INTO harness_usage_attribution
                   (route_id, provider, session_hash, agent_id, parent_agent_id,
                    role, model, input_tokens, output_tokens, status, duration_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    value.route_id,
                    value.provider,
                    value.session_hash,
                    value.agent_id,
                    value.parent_agent_id,
                    value.role,
                    value.model,
                    value.input_tokens,
                    value.output_tokens,
                    value.status,
                    value.duration_ms,
                ],
            )
            .map_err(|error| {
                AppError::Message(format!("record harness usage attribution: {error}"))
            })?;
        Ok(())
    }

    pub fn harness_attribution_for_session(
        &self,
        session_hash: &str,
    ) -> AppResult<Vec<HarnessUsageRecord>> {
        let connection = self.connection.lock().expect("usage db poisoned");
        let mut statement = connection
            .prepare(
                "SELECT route_id, provider, session_hash, agent_id, parent_agent_id,
                        role, model, input_tokens, output_tokens, status, duration_ms
                 FROM harness_usage_attribution
                 WHERE session_hash = ?1
                 ORDER BY id ASC",
            )
            .map_err(|error| AppError::Message(format!("prepare harness attribution: {error}")))?;
        let rows = statement
            .query_map([session_hash], |row| {
                Ok(HarnessUsageRecord {
                    route_id: row.get(0)?,
                    provider: row.get(1)?,
                    session_hash: row.get(2)?,
                    agent_id: row.get(3)?,
                    parent_agent_id: row.get(4)?,
                    role: row.get(5)?,
                    model: row.get(6)?,
                    input_tokens: row.get(7)?,
                    output_tokens: row.get(8)?,
                    status: row.get(9)?,
                    duration_ms: row.get(10)?,
                })
            })
            .map_err(|error| AppError::Message(format!("read harness attribution: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("decode harness attribution: {error}")))?;
        Ok(rows)
    }

    pub fn harness_attribution(&self) -> AppResult<Vec<HarnessUsageRecord>> {
        let connection = self.connection.lock().expect("usage db poisoned");
        let mut statement = connection
            .prepare(
                "SELECT route_id, provider, session_hash, agent_id, parent_agent_id,
                        role, model, input_tokens, output_tokens, status, duration_ms
                 FROM harness_usage_attribution
                 ORDER BY id ASC",
            )
            .map_err(|error| AppError::Message(format!("prepare harness attribution: {error}")))?;
        let rows = statement
            .query_map([], |row| {
                Ok(HarnessUsageRecord {
                    route_id: row.get(0)?,
                    provider: row.get(1)?,
                    session_hash: row.get(2)?,
                    agent_id: row.get(3)?,
                    parent_agent_id: row.get(4)?,
                    role: row.get(5)?,
                    model: row.get(6)?,
                    input_tokens: row.get(7)?,
                    output_tokens: row.get(8)?,
                    status: row.get(9)?,
                    duration_ms: row.get(10)?,
                })
            })
            .map_err(|error| AppError::Message(format!("read harness attribution: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("decode harness attribution: {error}")))?;
        Ok(rows)
    }

    pub fn summary(&self, route_id: &str) -> AppResult<UsageSummary> {
        let connection = self.connection.lock().expect("usage db poisoned");
        let mut statement = connection
            .prepare(
                "SELECT input_tokens, first_byte_ms
                 FROM request_usage
                 WHERE route_id = ?1
                 ORDER BY id DESC
                 LIMIT 12",
            )
            .map_err(|error| AppError::Message(format!("查詢使用統計失敗：{error}")))?;
        let rows = statement
            .query_map([route_id], |row| {
                Ok((row.get::<_, u64>(0)?, row.get::<_, Option<u64>>(1)?))
            })
            .map_err(|error| AppError::Message(format!("讀取使用統計失敗：{error}")))?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        let maximum = rows
            .iter()
            .map(|(tokens, _)| *tokens)
            .max()
            .unwrap_or(1)
            .max(1);
        let mut trend = rows
            .iter()
            .map(|(tokens, _)| *tokens as f64 / maximum as f64)
            .collect::<Vec<_>>();
        trend.reverse();
        let (turns, total_tokens) = connection
            .query_row(
                "SELECT COUNT(*),
                        COALESCE(SUM(input_tokens + output_tokens), 0)
                 FROM request_usage
                 WHERE route_id = ?1",
                [route_id],
                |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u64>(1)?)),
            )
            .map_err(|error| AppError::Message(format!("讀取 Provider 累計用量失敗：{error}")))?;
        Ok(UsageSummary {
            latest_input_tokens: rows.first().map(|row| row.0).unwrap_or(0),
            turns,
            total_tokens,
            latest_first_byte_ms: rows
                .first()
                .and_then(|row| row.1)
                .and_then(|value| u32::try_from(value).ok()),
            trend,
        })
    }

    pub fn latest_successful_route(&self) -> AppResult<Option<RouteTelemetry>> {
        let connection = self.connection.lock().expect("usage db poisoned");
        let result = connection.query_row(
            "SELECT route_id, provider, model, created_at
             FROM request_usage
             WHERE status >= 200
               AND status < 400
               AND review_run_id IS NULL
               AND lower(model) <> lower(?1)
             ORDER BY id DESC
             LIMIT 1",
            [AUTO_REVIEW_MODEL],
            |row| {
                Ok(RouteTelemetry {
                    route_id: row.get(0)?,
                    provider: row.get(1)?,
                    model: row.get(2)?,
                    created_at: row.get(3)?,
                })
            },
        );
        match result {
            Ok(telemetry) => Ok(Some(telemetry)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(AppError::Message(format!(
                "read latest successful route telemetry: {error}"
            ))),
        }
    }

    pub fn review_stats(&self) -> AppResult<ReviewStats> {
        let connection = self.connection.lock().expect("usage db poisoned");
        let (total_runs, fallback_runs) = connection
            .query_row(
                "SELECT COUNT(DISTINCT CASE
                          WHEN review_run_id IS NOT NULL THEN review_run_id
                          ELSE 'legacy-review-' || id END),
                        COUNT(DISTINCT CASE
                          WHEN review_role = 'fallback' AND status < 400
                          THEN COALESCE(review_run_id, 'legacy-review-' || id) END)
                 FROM request_usage
                 WHERE review_run_id IS NOT NULL OR lower(model) = lower(?1)",
                [AUTO_REVIEW_MODEL],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
            )
            .map_err(|error| AppError::Message(format!("read review totals: {error}")))?;

        let mut statement = connection
            .prepare(
                "SELECT route_id, MIN(provider), model,
                        SUM(CASE WHEN COALESCE(review_role, 'primary') = 'primary'
                                      AND status < 400 THEN 1 ELSE 0 END),
                        SUM(CASE WHEN review_role = 'fallback' AND status < 400 THEN 1 ELSE 0 END),
                        SUM(CASE WHEN status >= 400 THEN 1 ELSE 0 END),
                        MAX(created_at)
                 FROM request_usage
                 WHERE review_run_id IS NOT NULL OR lower(model) = lower(?1)
                 GROUP BY route_id, model
                 ORDER BY MAX(created_at) DESC",
            )
            .map_err(|error| AppError::Message(format!("prepare review stats: {error}")))?;
        let providers = statement
            .query_map([AUTO_REVIEW_MODEL], |row| {
                Ok(ReviewProviderStat {
                    route_id: row.get(0)?,
                    provider: row.get(1)?,
                    model: row.get(2)?,
                    primary_runs: row.get(3)?,
                    fallback_runs: row.get(4)?,
                    failed_runs: row.get(5)?,
                    last_used_at: row.get(6)?,
                })
            })
            .map_err(|error| AppError::Message(format!("read review stats: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("decode review stats: {error}")))?;
        drop(statement);

        let active = connection.query_row(
            "SELECT route_id, model, review_role, review_reason
             FROM request_usage
             WHERE review_run_id IS NOT NULL OR lower(model) = lower(?1)
             ORDER BY id DESC
             LIMIT 1",
            [AUTO_REVIEW_MODEL],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        );
        let (active_route_id, active_model, active_is_fallback, active_reason) = match active {
            Ok((route_id, model, role, reason)) => (
                Some(route_id),
                Some(model),
                role.as_deref() == Some("fallback"),
                reason,
            ),
            Err(rusqlite::Error::QueryReturnedNoRows) => (None, None, false, None),
            Err(error) => {
                return Err(AppError::Message(format!(
                    "read active review provider: {error}"
                )))
            }
        };

        Ok(ReviewStats {
            total_runs,
            fallback_runs,
            providers,
            active_route_id,
            active_model,
            active_is_fallback,
            active_reason,
        })
    }

    pub fn request_log(&self, limit: u32) -> AppResult<RequestLog> {
        self.request_log_page(&RequestLogPage {
            limit,
            ..RequestLogPage::default()
        })
    }

    pub fn request_log_page(&self, page: &RequestLogPage) -> AppResult<RequestLog> {
        let connection = self.connection.lock().expect("usage db poisoned");
        let failed: i64 = i64::from(page.failed_only);
        let route = page.route_id.clone().unwrap_or_default();
        let limit = page.limit.clamp(1, 100);
        let event_limit = page.event_limit.clamp(1, 500);
        let mut offset = page.offset;

        let entry_total: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM request_usage
                 WHERE (?1 = 0 OR status >= 400)
                   AND (?2 = '' OR route_id = ?2)",
                params![failed, route],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| AppError::Message(format!("count request log: {error}")))?
            .max(0) as u32;

        if let Some(focus) = page.focus_entry_id {
            let newer: u32 = connection
                .query_row(
                    "SELECT COUNT(*) FROM request_usage
                     WHERE id > ?1
                       AND (?2 = 0 OR status >= 400)
                       AND (?3 = '' OR route_id = ?3)",
                    params![focus, failed, route],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|error| AppError::Message(format!("focus request log: {error}")))?
                .max(0) as u32;
            offset = (newer / limit) * limit;
        }
        if entry_total == 0 {
            offset = 0;
        } else {
            let max_offset = ((entry_total - 1) / limit) * limit;
            if offset > max_offset {
                offset = max_offset;
            }
        }

        let mut statement = connection
            .prepare(
                "SELECT id, route_id, provider, model, input_tokens, output_tokens,
                        COALESCE(cached_input_tokens, 0), status, error, duration_ms,
                        first_byte_ms, created_at, connection_id,
                        stream_quality, first_output_delta_ms,
                        first_reasoning_delta_ms, COALESCE(output_delta_count, 0),
                        COALESCE(reasoning_delta_count, 0),
                        request_id, control_account_hash, execution_account_hash
                 FROM request_usage
                 WHERE (?1 = 0 OR status >= 400)
                   AND (?2 = '' OR route_id = ?2)
                 ORDER BY id DESC
                 LIMIT ?3 OFFSET ?4",
            )
            .map_err(|error| AppError::Message(format!("查詢請求紀錄失敗：{error}")))?;
        let entries = statement
            .query_map(params![failed, route, limit, offset], |row| {
                Ok(RequestLogEntry {
                    id: row.get(0)?,
                    route_id: row.get(1)?,
                    provider: row.get(2)?,
                    model: row.get(3)?,
                    input_tokens: row.get(4)?,
                    output_tokens: row.get(5)?,
                    cached_input_tokens: row.get(6)?,
                    status: row.get(7)?,
                    error: row.get(8)?,
                    duration_ms: row.get(9)?,
                    first_byte_ms: row.get(10)?,
                    created_at: row.get(11)?,
                    connection_id: row.get(12)?,
                    stream_quality: row.get(13)?,
                    first_output_delta_ms: row.get(14)?,
                    first_reasoning_delta_ms: row.get(15)?,
                    output_delta_count: row.get(16)?,
                    reasoning_delta_count: row.get(17)?,
                    request_id_hash: row
                        .get::<_, Option<String>>(18)?
                        .map(|id| vellum_proxy_runtime::hash_text(&id)),
                    control_account_hash: row.get(19)?,
                    execution_account_hash: row.get(20)?,
                })
            })
            .map_err(|error| AppError::Message(format!("讀取請求紀錄失敗：{error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("解析請求紀錄失敗：{error}")))?;

        let mut route_statement = connection
            .prepare(
                "SELECT route_id, MAX(provider) FROM request_usage
                 GROUP BY route_id
                 ORDER BY MAX(provider) ASC, route_id ASC",
            )
            .map_err(|error| AppError::Message(format!("query request log routes: {error}")))?;
        let entry_routes = route_statement
            .query_map([], |row| {
                Ok(RequestLogRouteOption {
                    route_id: row.get(0)?,
                    provider: row.get(1)?,
                })
            })
            .map_err(|error| AppError::Message(format!("read request log routes: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("decode request log routes: {error}")))?;

        let mut statement = connection
            .prepare(
                "SELECT provider, COUNT(*), COALESCE(SUM(input_tokens), 0),
                        COALESCE(SUM(output_tokens), 0),
                        COALESCE(SUM(CASE WHEN status >= 400 THEN 1 ELSE 0 END), 0)
                 FROM request_usage
                 GROUP BY provider
                 ORDER BY COUNT(*) DESC, provider ASC",
            )
            .map_err(|error| AppError::Message(format!("查詢供應商統計失敗：{error}")))?;
        let providers = statement
            .query_map([], |row| {
                Ok(ProviderUsage {
                    provider: row.get(0)?,
                    requests: row.get(1)?,
                    input_tokens: row.get(2)?,
                    output_tokens: row.get(3)?,
                    failed_requests: row.get(4)?,
                })
            })
            .map_err(|error| AppError::Message(format!("讀取供應商統計失敗：{error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("解析供應商統計失敗：{error}")))?;
        let compaction_events = Self::compaction_log_inner(&connection, event_limit)?;
        let subagent_events = Self::subagent_log_inner(&connection, event_limit)?;
        let subagent_runs = Self::subagent_runs_inner(&connection, event_limit)?;
        let compaction_total = compaction_events.len() as u32;
        let subagent_total = subagent_runs.len() as u32;
        Ok(RequestLog {
            entries,
            providers,
            entry_total,
            entry_offset: offset,
            compaction_total,
            subagent_total,
            entry_routes,
            compaction_events,
            subagent_events,
            subagent_runs,
        })
    }

    /// Monotonic diagnostic cursor used by the evaluator to isolate one case.
    pub fn diagnostic_cursor(&self) -> AppResult<i64> {
        self.connection
            .lock()
            .expect("usage db poisoned")
            .query_row(
                "SELECT COALESCE(MAX(id), 0) FROM diagnostic_events",
                [],
                |row| row.get(0),
            )
            .map_err(|error| AppError::Message(format!("read diagnostic cursor: {error}")))
    }

    /// Return the retained structured runtime diagnostics as a bounded JSONL
    /// projection for the support bundle. T3 full-context fields are removed
    /// before the payload leaves the database; the bundle exporter applies
    /// identifier hashing, secret redaction, and path redaction afterward.
    pub fn diagnostic_log_for_support(&self, max_bytes: usize) -> AppResult<(String, bool)> {
        let connection = self.connection.lock().expect("usage db poisoned");
        let mut statement = connection
            .prepare("SELECT payload FROM diagnostic_events ORDER BY id DESC")
            .map_err(|error| {
                AppError::Message(format!("prepare support diagnostic log: {error}"))
            })?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| AppError::Message(format!("read support diagnostic log: {error}")))?;
        let mut newest_first = Vec::new();
        let mut bytes = 0usize;
        let mut truncated = false;
        for row in rows {
            let payload = row.map_err(|error| {
                AppError::Message(format!("decode support diagnostic log: {error}"))
            })?;
            let Ok(mut event) =
                serde_json::from_str::<vellum_proxy_runtime::DiagnosticEvent>(&payload)
            else {
                continue;
            };
            vellum_proxy_runtime::strip_t3_fields(&mut event);
            let line = serde_json::to_string(&event).map_err(|error| {
                AppError::Message(format!("encode support diagnostic log: {error}"))
            })?;
            let next = line.len().saturating_add(1);
            if bytes.saturating_add(next) > max_bytes {
                truncated = true;
                break;
            }
            bytes = bytes.saturating_add(next);
            newest_first.push(line);
        }
        newest_first.reverse();
        let mut output = newest_first.join("\n");
        if !output.is_empty() {
            output.push('\n');
        }
        Ok((output, truncated))
    }

    /// Return canonical attempts emitted strictly after `cursor`, in durable
    /// emission order. Eval cases run sequentially, so snapshotting the cursor
    /// before a case captures accepted and failed attempts without fabricating
    /// them from a successful journal record.
    pub fn local_compaction_attempts_after(
        &self,
        cursor: i64,
    ) -> AppResult<Vec<vellum_proxy_runtime::diagnostics::LocalCompactionAttempt>> {
        let connection = self.connection.lock().expect("usage db poisoned");
        let mut statement = connection
            .prepare(
                "SELECT payload
                 FROM diagnostic_events
                 WHERE kind = 'local_compaction_attempt'
                   AND id > ?1
                 ORDER BY id ASC
                 LIMIT 500",
            )
            .map_err(|error| AppError::Message(format!("prepare attempt log: {error}")))?;
        let rows = statement
            .query_map([cursor], |row| row.get::<_, String>(0))
            .map_err(|error| AppError::Message(format!("read attempt log: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("decode attempt log: {error}")))?;
        let mut attempts = Vec::new();
        for payload in rows {
            if let Ok(vellum_proxy_runtime::DiagnosticEvent::LocalCompactionAttempt(att)) =
                serde_json::from_str(&payload)
            {
                attempts.push(att);
            }
        }
        Ok(attempts)
    }

    /// Return semantic validation failures emitted by this eval case.
    pub fn semantic_validation_diagnostics_after(
        &self,
        cursor: i64,
    ) -> AppResult<Vec<vellum_proxy_runtime::diagnostics::SemanticValidationDiagnostic>> {
        let connection = self.connection.lock().expect("usage db poisoned");
        let mut statement = connection
            .prepare(
                "SELECT payload FROM diagnostic_events
                 WHERE kind = 'semantic_validation' AND id > ?1
                 ORDER BY id ASC LIMIT 500",
            )
            .map_err(|error| {
                AppError::Message(format!("prepare semantic validation log: {error}"))
            })?;
        let rows = statement
            .query_map([cursor], |row| row.get::<_, String>(0))
            .map_err(|error| AppError::Message(format!("read semantic validation log: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                AppError::Message(format!("decode semantic validation log: {error}"))
            })?;
        Ok(rows
            .into_iter()
            .filter_map(|payload| serde_json::from_str(&payload).ok())
            .filter_map(|event| match event {
                vellum_proxy_runtime::DiagnosticEvent::SemanticValidation(diagnostic) => {
                    Some(diagnostic)
                }
                _ => None,
            })
            .collect())
    }

    pub fn task_stall_diagnostics_after(
        &self,
        cursor: i64,
    ) -> AppResult<Vec<vellum_proxy_runtime::task_stall::TaskStallDiagnostic>> {
        let connection = self.connection.lock().expect("usage db poisoned");
        let mut statement = connection
            .prepare(
                "SELECT payload FROM diagnostic_events
                 WHERE kind = 'task_stall' AND id > ?1
                 ORDER BY id ASC LIMIT 2000",
            )
            .map_err(|error| AppError::Message(format!("prepare task stall log: {error}")))?;
        let rows = statement
            .query_map([cursor], |row| row.get::<_, String>(0))
            .map_err(|error| AppError::Message(format!("read task stall log: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("decode task stall log: {error}")))?;
        Ok(rows
            .into_iter()
            .filter_map(|payload| serde_json::from_str(&payload).ok())
            .filter_map(|event| match event {
                vellum_proxy_runtime::DiagnosticEvent::TaskStall(diagnostic) => Some(diagnostic),
                _ => None,
            })
            .collect())
    }

    pub fn task_stall_recoveries_after(
        &self,
        cursor: i64,
    ) -> AppResult<Vec<vellum_proxy_runtime::task_stall::TaskStallRecoveryDiagnostic>> {
        let connection = self.connection.lock().expect("usage db poisoned");
        let mut statement = connection
            .prepare(
                "SELECT payload FROM diagnostic_events
                 WHERE kind = 'task_stall_recovery' AND id > ?1
                 ORDER BY id ASC LIMIT 500",
            )
            .map_err(|error| {
                AppError::Message(format!("prepare task stall recovery log: {error}"))
            })?;
        let rows = statement
            .query_map([cursor], |row| row.get::<_, String>(0))
            .map_err(|error| AppError::Message(format!("read task stall recovery log: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                AppError::Message(format!("decode task stall recovery log: {error}"))
            })?;
        Ok(rows
            .into_iter()
            .filter_map(|payload| serde_json::from_str(&payload).ok())
            .filter_map(|event| match event {
                vellum_proxy_runtime::DiagnosticEvent::TaskStallRecovery(diagnostic) => {
                    Some(diagnostic)
                }
                _ => None,
            })
            .collect())
    }

    pub fn task_stall_terminals_after(
        &self,
        cursor: i64,
    ) -> AppResult<Vec<vellum_proxy_runtime::task_stall::TaskStallTerminalDiagnostic>> {
        let connection = self.connection.lock().expect("usage db poisoned");
        let mut statement = connection
            .prepare(
                "SELECT payload FROM diagnostic_events
                 WHERE kind = 'task_stall_terminal' AND id > ?1
                 ORDER BY id ASC LIMIT 500",
            )
            .map_err(|error| {
                AppError::Message(format!("prepare task stall terminal log: {error}"))
            })?;
        let rows = statement
            .query_map([cursor], |row| row.get::<_, String>(0))
            .map_err(|error| AppError::Message(format!("read task stall terminal log: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                AppError::Message(format!("decode task stall terminal log: {error}"))
            })?;
        Ok(rows
            .into_iter()
            .filter_map(|payload| serde_json::from_str(&payload).ok())
            .filter_map(|event| match event {
                vellum_proxy_runtime::DiagnosticEvent::TaskStallTerminal(diagnostic) => {
                    Some(diagnostic)
                }
                _ => None,
            })
            .collect())
    }

    pub fn task_efficiency_diagnostics_after(
        &self,
        cursor: i64,
    ) -> AppResult<Vec<vellum_proxy_runtime::task_efficiency::TaskEfficiencyDiagnostic>> {
        let connection = self.connection.lock().expect("usage db poisoned");
        let mut statement = connection
            .prepare(
                "SELECT payload FROM diagnostic_events
                 WHERE kind = 'task_efficiency' AND id > ?1
                 ORDER BY id ASC LIMIT 2000",
            )
            .map_err(|error| AppError::Message(format!("prepare task efficiency log: {error}")))?;
        let rows = statement
            .query_map([cursor], |row| row.get::<_, String>(0))
            .map_err(|error| AppError::Message(format!("read task efficiency log: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("decode task efficiency log: {error}")))?;
        Ok(rows
            .into_iter()
            .filter_map(|payload| serde_json::from_str(&payload).ok())
            .filter_map(|event| match event {
                vellum_proxy_runtime::DiagnosticEvent::TaskEfficiency(diagnostic) => {
                    Some(diagnostic)
                }
                _ => None,
            })
            .collect())
    }

    pub fn task_efficiency_recoveries_after(
        &self,
        cursor: i64,
    ) -> AppResult<Vec<vellum_proxy_runtime::task_efficiency::TaskEfficiencyRecoveryDiagnostic>>
    {
        let connection = self.connection.lock().expect("usage db poisoned");
        let mut statement = connection
            .prepare(
                "SELECT payload FROM diagnostic_events
                 WHERE kind = 'task_efficiency_recovery' AND id > ?1
                 ORDER BY id ASC LIMIT 500",
            )
            .map_err(|error| {
                AppError::Message(format!("prepare task efficiency recovery log: {error}"))
            })?;
        let rows = statement
            .query_map([cursor], |row| row.get::<_, String>(0))
            .map_err(|error| {
                AppError::Message(format!("read task efficiency recovery log: {error}"))
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                AppError::Message(format!("decode task efficiency recovery log: {error}"))
            })?;
        Ok(rows
            .into_iter()
            .filter_map(|payload| serde_json::from_str(&payload).ok())
            .filter_map(|event| match event {
                vellum_proxy_runtime::DiagnosticEvent::TaskEfficiencyRecovery(diagnostic) => {
                    Some(diagnostic)
                }
                _ => None,
            })
            .collect())
    }

    pub fn recent_local_compaction_attempts(
        &self,
        limit: u32,
    ) -> AppResult<Vec<vellum_proxy_runtime::diagnostics::LocalCompactionAttempt>> {
        let connection = self.connection.lock().expect("usage db poisoned");
        let mut statement = connection
            .prepare(
                "SELECT payload FROM (
                   SELECT id, payload
                     FROM diagnostic_events
                    WHERE kind = 'local_compaction_attempt'
                    ORDER BY id DESC
                    LIMIT ?1
                 ) ORDER BY id ASC",
            )
            .map_err(|error| AppError::Message(format!("prepare recent attempt log: {error}")))?;
        let rows = statement
            .query_map([limit.clamp(1, 500)], |row| row.get::<_, String>(0))
            .map_err(|error| AppError::Message(format!("read recent attempt log: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("decode recent attempt log: {error}")))?;
        Ok(rows
            .into_iter()
            .filter_map(|payload| serde_json::from_str(&payload).ok())
            .filter_map(|event| match event {
                vellum_proxy_runtime::DiagnosticEvent::LocalCompactionAttempt(attempt) => {
                    Some(attempt)
                }
                _ => None,
            })
            .collect())
    }

    fn compaction_log_inner(
        connection: &Connection,
        limit: u32,
    ) -> AppResult<Vec<CompactionLogEntry>> {
        let mut statement = connection
            .prepare(
                "SELECT id, created_at, payload, kind
                 FROM diagnostic_events
                 WHERE kind IN ('compaction_decision', 'local_compaction_attempt')
                 ORDER BY id DESC
                 LIMIT ?1",
            )
            .map_err(|error| AppError::Message(format!("prepare compaction log: {error}")))?;
        let rows = statement
            .query_map([limit.clamp(1, 500)], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|error| AppError::Message(format!("read compaction log: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("decode compaction log: {error}")))?;
        Ok(rows
            .into_iter()
            .map(|(id, created_at, payload, kind)| {
                let value = serde_json::from_str::<serde_json::Value>(&payload)
                    .unwrap_or(serde_json::Value::Null);
                if kind == "local_compaction_attempt" {
                    let source_visible = value
                        .pointer("/tokenBreakdown/sourceModelVisibleTokens")
                        .and_then(serde_json::Value::as_u64);
                    let replacement_visible = value
                        .pointer("/tokenBreakdown/replacementModelVisibleTokens")
                        .and_then(serde_json::Value::as_u64);
                    let replacement_durable = value
                        .pointer("/tokenBreakdown/replacementDurableTokens")
                        .and_then(serde_json::Value::as_u64);
                    let quality_outcome = value
                        .get("qualityOutcome")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    let candidate_generation = value
                        .get("candidateGeneration")
                        .and_then(serde_json::Value::as_u64)
                        .map(|v| v as u32);
                    let fallback_reason = value
                        .get("fallbackReason")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    let failure_cat = value
                        .get("failureCategory")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    CompactionLogEntry {
                        id,
                        created_at,
                        engine: "canonical_attempt".to_string(),
                        outcome: quality_outcome.clone().unwrap_or_else(|| "unknown".into()),
                        reason: failure_cat,
                        tokens_before: source_visible,
                        tokens_after: replacement_visible,
                        source_model_visible_tokens: source_visible,
                        replacement_model_visible_tokens: replacement_visible,
                        replacement_durable_tokens: replacement_durable,
                        quality_outcome,
                        candidate_generation,
                        fallback_reason,
                        items_before: None,
                        items_after: None,
                        window: None,
                        active_tokens: None,
                        threshold_percent: None,
                        checkpoint_id: None,
                        generation: candidate_generation,
                    }
                } else {
                    let source_visible = value
                        .pointer("/tokenBreakdown/sourceModelVisibleTokens")
                        .and_then(serde_json::Value::as_u64);
                    let replacement_visible = value
                        .pointer("/tokenBreakdown/replacementModelVisibleTokens")
                        .and_then(serde_json::Value::as_u64);
                    let replacement_durable = value
                        .pointer("/tokenBreakdown/replacementDurableTokens")
                        .and_then(serde_json::Value::as_u64);
                    let quality_outcome = value
                        .get("qualityOutcome")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    let candidate_generation = value
                        .get("candidateGeneration")
                        .and_then(serde_json::Value::as_u64)
                        .map(|v| v as u32);
                    let fallback_reason = value
                        .get("fallbackReason")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    CompactionLogEntry {
                        id,
                        created_at,
                        engine: value
                            .get("engine")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("unknown")
                            .to_string(),
                        outcome: value
                            .get("outcome")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("unknown")
                            .to_string(),
                        reason: value
                            .get("reason")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                        tokens_before: value
                            .get("tokensBefore")
                            .and_then(serde_json::Value::as_u64),
                        tokens_after: value.get("tokensAfter").and_then(serde_json::Value::as_u64),
                        source_model_visible_tokens: source_visible,
                        replacement_model_visible_tokens: replacement_visible,
                        replacement_durable_tokens: replacement_durable,
                        quality_outcome,
                        candidate_generation,
                        fallback_reason,
                        items_before: value.get("itemsBefore").and_then(serde_json::Value::as_u64),
                        items_after: value.get("itemsAfter").and_then(serde_json::Value::as_u64),
                        window: value.get("window").and_then(serde_json::Value::as_u64),
                        active_tokens: value
                            .get("activeTokens")
                            .and_then(serde_json::Value::as_u64),
                        threshold_percent: value
                            .get("thresholdPercent")
                            .and_then(serde_json::Value::as_u64)
                            .map(|v| v as u32),
                        checkpoint_id: value
                            .get("checkpointId")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                        generation: value
                            .get("generation")
                            .and_then(serde_json::Value::as_u64)
                            .map(|v| v as u32),
                    }
                }
            })
            .collect())
    }

    fn subagent_log_inner(connection: &Connection, limit: u32) -> AppResult<Vec<SubagentLogEntry>> {
        let mut statement = connection
            .prepare(
                "SELECT id, created_at, kind, call_id, parent_request_id,
                        child_request_id, child_model, link_method, link_confidence
                 FROM subagent_events
                 ORDER BY id DESC
                 LIMIT ?1",
            )
            .map_err(|error| AppError::Message(format!("prepare subagent log: {error}")))?;
        let rows = statement
            .query_map([limit.clamp(1, 500)], |row| {
                Ok(SubagentLogEntry {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    kind: row.get(2)?,
                    call_id: row.get(3)?,
                    parent_request_id: row.get(4)?,
                    child_request_id: row.get(5)?,
                    child_model: row.get(6)?,
                    link_method: row.get(7)?,
                    link_confidence: row.get(8)?,
                })
            })
            .map_err(|error| AppError::Message(format!("read subagent log: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("decode subagent log: {error}")))?;
        Ok(rows)
    }

    /// Aggregate the raw `subagent_events` feed into one [`SubagentRun`] per
    /// spawned child agent, joined against the child's own `request_usage`
    /// row for a real outcome.
    ///
    /// A `SpawnCompleted` event only proves the parent's tool call got *a*
    /// result back — never that the child request behind it succeeded. This
    /// is why the state machine below keys off the joined child outcome
    /// (`ChildUsageRow`), not off which raw event kinds are present.
    fn subagent_runs_inner(connection: &Connection, limit: u32) -> AppResult<Vec<SubagentRun>> {
        let limit = limit.clamp(1, 500);
        // Several raw events collapse into one run, so pull a wider raw
        // window than the run count the caller asked for. Still well below
        // the diagnostic retention ceiling (100_000 rows, see
        // `evict_diagnostics_inner`).
        let raw_limit = (i64::from(limit)).saturating_mul(8).min(4_000);

        let mut statement = connection
            .prepare(
                "SELECT id, created_at, kind, call_id, parent_request_id,
                        child_request_id, child_model, child_effort,
                        link_method, link_confidence, payload
                 FROM subagent_events
                 ORDER BY id DESC
                 LIMIT ?1",
            )
            .map_err(|error| AppError::Message(format!("prepare subagent runs: {error}")))?;
        let mut rows = statement
            .query_map([raw_limit], |row| {
                Ok(RawSubagentRow {
                    created_at: row.get(1)?,
                    kind: row.get(2)?,
                    call_id: row.get(3)?,
                    parent_request_id: row.get(4)?,
                    child_request_id: row.get(5)?,
                    child_model: row.get(6)?,
                    child_effort: row.get(7)?,
                    link_confidence: row.get(9)?,
                    payload: row.get(10)?,
                })
            })
            .map_err(|error| AppError::Message(format!("read subagent runs: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("decode subagent runs: {error}")))?;
        // The query is newest-first (matches every other log query in this
        // file); flip to oldest-first so "the earliest row for this call_id
        // wins" reads the way it is written below.
        rows.reverse();

        let mut groups: std::collections::BTreeMap<String, CallGroup> =
            std::collections::BTreeMap::new();
        let mut orphan_children: Vec<RawSubagentRow> = Vec::new();
        for row in rows {
            match (row.kind.as_str(), &row.call_id) {
                ("spawn_requested", Some(call_id)) => {
                    let group = groups.entry(call_id.clone()).or_default();
                    if group.requested.is_none() {
                        group.requested = Some(row);
                    }
                }
                ("child_turn", Some(call_id)) => {
                    let group = groups.entry(call_id.clone()).or_default();
                    if group.child.is_none() {
                        group.child = Some(row);
                    }
                }
                ("child_turn", None) => orphan_children.push(row),
                ("spawn_completed", Some(call_id)) => {
                    let group = groups.entry(call_id.clone()).or_default();
                    if group.completed.is_none() {
                        group.completed = Some(row);
                    }
                }
                // "spawn_completed" with no call_id cannot happen (the field
                // is not optional on the event); ignore defensively.
                _ => {}
            }
        }

        let child_request_ids: std::collections::BTreeSet<String> = groups
            .values()
            .filter_map(CallGroup::child_request_id)
            .chain(
                orphan_children
                    .iter()
                    .filter_map(|row| row.child_request_id.clone()),
            )
            .collect();
        let parent_request_ids: std::collections::BTreeSet<String> = groups
            .values()
            .filter_map(CallGroup::parent_request_id)
            .chain(
                orphan_children
                    .iter()
                    .filter_map(|row| row.parent_request_id.clone()),
            )
            .collect();
        let usage_by_request = Self::child_usage_by_request(connection, &child_request_ids)?;
        let parent_usage_by_request =
            Self::usage_entry_by_request(connection, &parent_request_ids)?;

        // First pass: which call_ids ended up with no confidently-linked
        // child at all. Needed before the second pass so a group can see its
        // siblings' resolution, not just its own.
        let unresolved: Vec<(String, i64)> = groups
            .iter()
            .filter(|(_, group)| group.child_request_id().is_none())
            .map(|(call_id, group)| (call_id.clone(), group.requested_at()))
            .collect();
        let has_overlapping_orphan = |a: i64, b: i64| {
            let lo = a.min(b);
            let hi = a.max(b) + SUBAGENT_RUN_BOUND_SECS;
            orphan_children
                .iter()
                .any(|orphan| orphan.created_at >= lo && orphan.created_at <= hi)
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs() as i64)
            .unwrap_or(0);

        let mut runs = Vec::with_capacity(groups.len() + orphan_children.len());
        for (call_id, group) in &groups {
            let requested_at = group.requested_at();
            let child_request_id = group.child_request_id();
            let usage = child_request_id
                .as_deref()
                .and_then(|id| usage_by_request.get(id));
            let stale = now.saturating_sub(requested_at) > SUBAGENT_RUN_BOUND_SECS;
            let ambiguous = child_request_id.is_none()
                && unresolved.iter().any(|(other_id, other_at)| {
                    other_id != call_id
                        && (other_at - requested_at).abs() <= SUBAGENT_RUN_BOUND_SECS
                        && has_overlapping_orphan(requested_at, *other_at)
                });
            let child_usage_entry_id = usage.map(|usage| usage.id);
            let parent_usage_entry_id = group
                .parent_request_id()
                .as_deref()
                .and_then(|id| parent_usage_by_request.get(id))
                .copied();

            runs.push(group.resolve_run(
                call_id.clone(),
                child_request_id,
                usage,
                stale,
                ambiguous,
                parent_usage_entry_id,
                child_usage_entry_id,
            ));
        }
        for orphan in orphan_children {
            let usage = orphan
                .child_request_id
                .as_deref()
                .and_then(|id| usage_by_request.get(id));
            let child_usage_entry_id = usage.map(|usage| usage.id);
            let parent_usage_entry_id = orphan
                .parent_request_id
                .as_deref()
                .and_then(|id| parent_usage_by_request.get(id))
                .copied();
            runs.push(SubagentRun {
                call_id: None,
                state: SubagentRunState::Unlinked,
                parent_request_hash: orphan
                    .parent_request_id
                    .as_deref()
                    .map(vellum_proxy_runtime::hash_text),
                child_request_hash: orphan
                    .child_request_id
                    .as_deref()
                    .map(vellum_proxy_runtime::hash_text),
                parent_request_id: orphan.parent_request_id.clone(),
                child_request_id: orphan.child_request_id.clone(),
                parent_usage_entry_id,
                child_usage_entry_id,
                route_id: usage
                    .and_then(|value| value.route_id.clone())
                    .or_else(|| route_id_from_payload(&orphan.payload)),
                model: orphan
                    .child_model
                    .clone()
                    .or_else(|| usage.and_then(|value| value.model.clone())),
                effort: orphan.child_effort.clone(),
                link_confidence: SubagentLinkConfidence::Unlinked,
                requested_at: orphan.created_at,
                completed_at: None,
                duration_ms: usage.and_then(|value| value.duration_ms),
                outcome: usage.and_then(|value| value.outcome.clone()),
                error_category: usage.and_then(|value| value.error_category.clone()),
            });
        }

        // Native Codex V2 does not emit the legacy three-event spawn ledger.
        // Its exact parent/child authority is `subagent_graph_linked`; omitting
        // that source made the UI report only old translated-harness runs.
        runs.extend(Self::native_subagent_runs_inner(
            connection, raw_limit, now,
        )?);

        runs.sort_by_key(|run| std::cmp::Reverse(run.requested_at));
        runs.truncate(limit as usize);
        Ok(runs)
    }

    fn native_subagent_runs_inner(
        connection: &Connection,
        raw_limit: i64,
        now: i64,
    ) -> AppResult<Vec<SubagentRun>> {
        let mut statement = connection
            .prepare(
                "SELECT created_at, payload
                 FROM diagnostic_events
                 WHERE kind = 'subagent_graph_linked'
                 ORDER BY id DESC
                 LIMIT ?1",
            )
            .map_err(|error| {
                AppError::Message(format!("prepare native subagent graph: {error}"))
            })?;
        let rows = statement
            .query_map([raw_limit], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| AppError::Message(format!("read native subagent graph: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("decode native subagent graph: {error}")))?;
        drop(statement);

        // The runtime can observe the same official metadata more than once
        // across continuations. Count the graph edge, not diagnostic rows.
        let mut edges = std::collections::BTreeMap::new();
        for (created_at, payload) in rows {
            let Ok(mut edge) = serde_json::from_str::<NativeSubagentGraphEdge>(&payload) else {
                continue;
            };
            edge.created_at = created_at;
            edges
                .entry((edge.parent_thread_id.clone(), edge.child_thread_id.clone()))
                .or_insert(edge);
        }

        let thread_ids = edges
            .values()
            .flat_map(|edge| [&edge.parent_thread_id, &edge.child_thread_id])
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let usage_by_thread = Self::native_usage_by_thread(connection, &thread_ids)?;

        Ok(edges
            .into_values()
            .map(|edge| {
                let usage = usage_by_thread.get(&edge.child_thread_id);
                let last_activity = usage
                    .map(|value| value.created_at)
                    .unwrap_or(edge.created_at);
                let stale = now.saturating_sub(last_activity) > SUBAGENT_RUN_BOUND_SECS;
                let is_success = usage.is_some_and(|value| {
                    matches!(value.outcome.as_deref(), Some("success"))
                        || (value.outcome.is_none()
                            && (200..300).contains(&value.status)
                            && value.error.is_none())
                });
                let is_cancelled = usage.is_some_and(|value| {
                    matches!(
                        value.outcome.as_deref(),
                        Some("client_cancel") | Some("client_disconnect")
                    )
                });
                let is_failure = usage.is_some_and(|value| {
                    !is_success
                        && !is_cancelled
                        && (value.status >= 400
                            || value.error.is_some()
                            || matches!(
                                value.outcome.as_deref(),
                                Some("provider_failure") | Some("protocol_failure")
                            ))
                });
                let state = if is_cancelled {
                    SubagentRunState::Cancelled
                } else if is_failure {
                    SubagentRunState::Failed
                } else if is_success && stale {
                    SubagentRunState::Completed
                } else if usage.is_some() {
                    SubagentRunState::Running
                } else if stale {
                    SubagentRunState::Failed
                } else {
                    SubagentRunState::Requested
                };
                let terminal = matches!(
                    state,
                    SubagentRunState::Completed
                        | SubagentRunState::Failed
                        | SubagentRunState::Cancelled
                );
                let parent_hash = vellum_proxy_runtime::hash_text(&edge.parent_thread_id);
                let child_hash = vellum_proxy_runtime::hash_text(&edge.child_thread_id);
                SubagentRun {
                    call_id: Some(format!("native-{parent_hash}-{child_hash}")),
                    state,
                    parent_request_id: None,
                    child_request_id: None,
                    parent_request_hash: Some(parent_hash),
                    child_request_hash: Some(child_hash),
                    parent_usage_entry_id: usage_by_thread
                        .get(&edge.parent_thread_id)
                        .map(|value| value.id),
                    child_usage_entry_id: usage.map(|value| value.id),
                    route_id: usage.and_then(|value| value.route_id.clone()),
                    model: usage.and_then(|value| value.model.clone()),
                    effort: None,
                    link_confidence: match edge.confidence.as_str() {
                        "high" => SubagentLinkConfidence::Exact,
                        "medium" | "low" => SubagentLinkConfidence::Heuristic,
                        _ => SubagentLinkConfidence::Unlinked,
                    },
                    requested_at: edge.created_at,
                    completed_at: terminal.then_some(last_activity),
                    duration_ms: usage.and_then(|value| value.duration_ms),
                    outcome: usage.and_then(|value| value.outcome.clone()),
                    error_category: usage.and_then(|value| value.error_category.clone()),
                }
            })
            .collect())
    }

    fn native_usage_by_thread(
        connection: &Connection,
        thread_ids: &std::collections::BTreeSet<String>,
    ) -> AppResult<std::collections::HashMap<String, NativeThreadUsageRow>> {
        let mut found = std::collections::HashMap::new();
        let mut statement = connection
            .prepare(
                "SELECT id, status, error, outcome, error_category, duration_ms,
                        model, route_id, created_at
                 FROM request_usage
                 WHERE conversation_identity LIKE '%:' || ?1
                 ORDER BY id DESC
                 LIMIT 1",
            )
            .map_err(|error| AppError::Message(format!("prepare native child usage: {error}")))?;
        for thread_id in thread_ids {
            let usage = statement
                .query_row([thread_id], |row| {
                    Ok(NativeThreadUsageRow {
                        id: row.get(0)?,
                        status: row.get::<_, i64>(1)? as u16,
                        error: row.get(2)?,
                        outcome: row.get(3)?,
                        error_category: row.get(4)?,
                        duration_ms: row.get::<_, Option<i64>>(5)?.map(|value| value as u64),
                        model: row.get(6)?,
                        route_id: row.get(7)?,
                        created_at: row.get(8)?,
                    })
                })
                .optional()
                .map_err(|error| AppError::Message(format!("read native child usage: {error}")))?;
            if let Some(usage) = usage {
                found.insert(thread_id.clone(), usage);
            }
        }
        Ok(found)
    }

    /// Look up every child request's own `request_usage` row in one prepared
    /// statement per id. `child_request_ids` is bounded by the raw event
    /// window above, so this stays cheap.
    fn child_usage_by_request(
        connection: &Connection,
        child_request_ids: &std::collections::BTreeSet<String>,
    ) -> AppResult<std::collections::HashMap<String, ChildUsageRow>> {
        let mut usage_by_request = std::collections::HashMap::new();
        if child_request_ids.is_empty() {
            return Ok(usage_by_request);
        }
        let mut statement = connection
            .prepare(
                "SELECT id, status, error, outcome, error_category, duration_ms, model, route_id
                 FROM request_usage
                 WHERE request_id = ?1
                 ORDER BY id DESC
                 LIMIT 1",
            )
            .map_err(|error| AppError::Message(format!("prepare child usage: {error}")))?;
        for request_id in child_request_ids {
            let found = statement
                .query_row(params![request_id], |row| {
                    Ok(ChildUsageRow {
                        id: row.get(0)?,
                        status: row.get::<_, i64>(1)? as u16,
                        error: row.get(2)?,
                        outcome: row.get(3)?,
                        error_category: row.get(4)?,
                        duration_ms: row.get::<_, Option<i64>>(5)?.map(|value| value as u64),
                        model: row.get(6)?,
                        route_id: row.get(7)?,
                    })
                })
                .optional()
                .map_err(|error| AppError::Message(format!("read child usage: {error}")))?;
            if let Some(usage) = found {
                usage_by_request.insert(request_id.clone(), usage);
            }
        }
        Ok(usage_by_request)
    }

    /// Resolve the `request_usage.id` for a set of request ids (parent side).
    /// This is the local navigation anchor the Log screen scrolls to; it never
    /// carries identity beyond the numeric row id.
    fn usage_entry_by_request(
        connection: &Connection,
        request_ids: &std::collections::BTreeSet<String>,
    ) -> AppResult<std::collections::HashMap<String, i64>> {
        let mut entry_by_request = std::collections::HashMap::new();
        if request_ids.is_empty() {
            return Ok(entry_by_request);
        }
        let mut statement = connection
            .prepare("SELECT id FROM request_usage WHERE request_id = ?1 ORDER BY id DESC LIMIT 1")
            .map_err(|error| AppError::Message(format!("prepare usage entry: {error}")))?;
        for request_id in request_ids {
            if let Some(id) = statement
                .query_row(params![request_id], |row| row.get::<_, i64>(0))
                .optional()
                .map_err(|error| AppError::Message(format!("read usage entry: {error}")))?
            {
                entry_by_request.insert(request_id.clone(), id);
            }
        }
        Ok(entry_by_request)
    }

    /// Aggregate every recorded proxy request once. The caller decides which
    /// routes to exclude (normally OpenAI, whose authoritative totals come
    /// from the Codex profile service).
    pub fn activity(&self, excluded_route_ids: &[String]) -> AppResult<LocalUsageActivity> {
        let connection = self.connection.lock().expect("usage db poisoned");
        let excluded = excluded_route_ids
            .iter()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>();

        let mut statement = connection
            .prepare(
                "SELECT date(created_at, 'unixepoch', 'localtime'), route_id, MIN(provider),
                        COALESCE(SUM(input_tokens + output_tokens), 0), COUNT(*)
                 FROM request_usage
                 GROUP BY date(created_at, 'unixepoch', 'localtime'), route_id
                 ORDER BY 1 ASC",
            )
            .map_err(|error| AppError::Message(format!("prepare usage activity: {error}")))?;
        let days = statement
            .query_map([], |row| {
                Ok(LocalUsageDay {
                    date: row.get(0)?,
                    route_id: row.get(1)?,
                    provider: row.get(2)?,
                    tokens: row.get(3)?,
                    requests: row.get(4)?,
                })
            })
            .map_err(|error| AppError::Message(format!("read usage activity: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("decode usage activity: {error}")))?
            .into_iter()
            .filter(|row| !excluded.contains(row.route_id.as_str()))
            .collect();
        drop(statement);

        let mut statement = connection
            .prepare(
                "SELECT route_id, MIN(provider),
                        COALESCE(SUM(input_tokens + output_tokens), 0)
                 FROM request_usage
                 GROUP BY route_id
                 ORDER BY 3 DESC, 2 ASC",
            )
            .map_err(|error| AppError::Message(format!("prepare provider activity: {error}")))?;
        let providers = statement
            .query_map([], |row| {
                Ok(LocalProviderTotal {
                    route_id: row.get(0)?,
                    provider: row.get(1)?,
                    tokens: row.get(2)?,
                })
            })
            .map_err(|error| AppError::Message(format!("read provider activity: {error}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Message(format!("decode provider activity: {error}")))?
            .into_iter()
            .filter(|row| !excluded.contains(row.route_id.as_str()))
            .collect();
        drop(statement);
        let mut statement = connection
            .prepare("SELECT route_id, duration_ms FROM request_usage")
            .map_err(|error| AppError::Message(format!("prepare request durations: {error}")))?;
        let longest_request_ms = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })
            .map_err(|error| AppError::Message(format!("read request durations: {error}")))?
            .filter_map(Result::ok)
            .filter(|(route_id, _)| !excluded.contains(route_id.as_str()))
            .map(|(_, duration)| duration)
            .max()
            .unwrap_or(0);

        Ok(LocalUsageActivity {
            days,
            providers,
            longest_request_ms,
        })
    }
}

pub fn usage_from_response(response: &serde_json::Value) -> (u64, u64) {
    let usage = response.get("usage");
    let input = usage
        .and_then(|usage| {
            usage
                .get("input_tokens")
                .or_else(|| usage.get("prompt_tokens"))
        })
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .and_then(|usage| {
            usage
                .get("output_tokens")
                .or_else(|| usage.get("completion_tokens"))
        })
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    (input, output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_attempt(generation: u32) -> vellum_proxy_runtime::DiagnosticEvent {
        use vellum_proxy_runtime::diagnostics::{CompactionTriggerOrigin, LocalCompactionAttempt};
        vellum_proxy_runtime::DiagnosticEvent::LocalCompactionAttempt(LocalCompactionAttempt {
            engine_id: "codex_local_v0_150".into(),
            engine_provenance: Some("openai-codex@rust-v0.150.0-alpha.8+fcbdb578".into()),
            route_id: "route-1".into(),
            upstream_model: "grok-4.6".into(),
            trigger_origin: CompactionTriggerOrigin::ClientCompactionRequest,
            request_id_hash: Some(format!("request-{generation}")),
            source_hash: format!("source-{generation}"),
            replacement_hash: Some(format!("replacement-{generation}")),
            generation,
            items_before: 40,
            items_after: 4,
            tokens_before: 100_000,
            tokens_after: 12_000,
            elapsed_ms: 4_200,
            context_retreats: 0,
            failure: None,
            bounded_failure_diagnostic: None,
        })
    }

    #[test]
    fn local_attempt_cursor_returns_real_events_in_emission_order() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        store.record_diagnostic_event(&local_attempt(1)).unwrap();
        let cursor = store.diagnostic_cursor().unwrap();
        store.record_diagnostic_event(&local_attempt(2)).unwrap();
        store.record_diagnostic_event(&local_attempt(3)).unwrap();

        let after = store.local_compaction_attempts_after(cursor).unwrap();
        assert_eq!(
            after
                .iter()
                .map(|attempt| attempt.generation)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        let recent = store.recent_local_compaction_attempts(2).unwrap();
        assert_eq!(
            recent
                .iter()
                .map(|attempt| attempt.generation)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn semantic_validation_cursor_returns_structured_case_diagnostics() {
        use vellum_proxy_runtime::diagnostics::{
            SemanticValidationDiagnostic, SemanticValidationFieldError,
        };

        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        let cursor = store.diagnostic_cursor().unwrap();
        store
            .record_diagnostic_event(&vellum_proxy_runtime::DiagnosticEvent::SemanticValidation(
                SemanticValidationDiagnostic {
                    request_id_hash: Some("request-hash".into()),
                    candidate_generation: 2,
                    invalid_evidence_refs: 1,
                    rejected_claim_count: 3,
                    errors: vec![SemanticValidationFieldError {
                        field: "failedAttemptsAdd".into(),
                        reason: "not grounded".into(),
                    }],
                },
            ))
            .unwrap();

        let diagnostics = store.semantic_validation_diagnostics_after(cursor).unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].candidate_generation, 2);
        assert_eq!(diagnostics[0].errors[0].field, "failedAttemptsAdd");
    }

    #[test]
    fn task_stall_cursor_returns_state_and_proven_recovery_injection() {
        use vellum_proxy_runtime::task_stall::{
            StallRecoveryState, TaskStallDiagnostic, TaskStallIntervention,
            TaskStallRecoveryDiagnostic, TaskStallTerminalDiagnostic,
        };

        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        let cursor = store.diagnostic_cursor().unwrap();
        store
            .record_diagnostic_event(&vellum_proxy_runtime::DiagnosticEvent::TaskStall(
                TaskStallDiagnostic {
                    request_id_hash: Some("sha256:req".into()),
                    stall_mode: "recover".into(),
                    task_revision: 2,
                    tool_results_since_progress: 24,
                    last_progress_seq: 8,
                    last_progress_kind: None,
                    last_progress_digest: None,
                    recovery_injected_count: 1,
                    post_recovery_no_progress: 8,
                    investigation_ledger_count: 3,
                    unattributed_operations: 4,
                    stall_candidate: true,
                    watchdog_candidate: true,
                    watchdog_stop: false,
                },
            ))
            .unwrap();
        store
            .record_diagnostic_event(&vellum_proxy_runtime::DiagnosticEvent::TaskStallRecovery(
                TaskStallRecoveryDiagnostic {
                    request_id_hash: "sha256:req".into(),
                    request_index: Some(17),
                    recovery_count: 2,
                    recovery_level: StallRecoveryState::Escalated,
                    intervention: TaskStallIntervention::ToolDisabledFinalization,
                    recovery_message_hash: "sha256:message".into(),
                    tool_results_since_progress: 24,
                    post_recovery_no_progress: 8,
                    last_progress: None,
                },
            ))
            .unwrap();
        store
            .record_diagnostic_event(&vellum_proxy_runtime::DiagnosticEvent::TaskStallTerminal(
                TaskStallTerminalDiagnostic {
                    request_id_hash: "sha256:req".into(),
                    request_index: Some(18),
                    task_revision: 2,
                    recovery_count: 2,
                    post_recovery_no_progress: 8,
                    category: "continued_after_tool_disabled_finalization".into(),
                },
            ))
            .unwrap();

        let diagnostics = store.task_stall_diagnostics_after(cursor).unwrap();
        let recoveries = store.task_stall_recoveries_after(cursor).unwrap();
        let terminals = store.task_stall_terminals_after(cursor).unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].watchdog_candidate);
        assert!(!diagnostics[0].watchdog_stop);
        assert_eq!(recoveries.len(), 1);
        assert_eq!(recoveries[0].request_index, Some(17));
        assert_eq!(recoveries[0].recovery_level, StallRecoveryState::Escalated);
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].request_index, Some(18));
    }

    #[test]
    fn provider_request_is_counted_once() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        store
            .record(&UsageRecord {
                route_id: "weikuwu".into(),
                provider: "weikuwu".into(),
                model: "glm".into(),
                input_tokens: 10,
                output_tokens: 2,
                status: 200,
                error: None,
                duration_ms: 20,
                first_byte_ms: Some(4),
                review_run_id: None,
                review_role: None,
                review_reason: None,
                ..Default::default()
            })
            .unwrap();
        let summary = store.summary("weikuwu").unwrap();
        assert_eq!(summary.turns, 1);
        assert_eq!(summary.latest_input_tokens, 10);
        assert_eq!(summary.total_tokens, 12);
    }

    #[test]
    fn legacy_usage_db_migrates_and_old_rows_have_null_new_columns() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.sqlite3");
        // Simulate a pre-migration database with only the original columns.
        {
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE request_usage (
                        id             INTEGER PRIMARY KEY AUTOINCREMENT,
                        route_id       TEXT NOT NULL,
                        provider       TEXT NOT NULL,
                        model          TEXT NOT NULL,
                        input_tokens   INTEGER NOT NULL,
                        output_tokens  INTEGER NOT NULL,
                        status         INTEGER NOT NULL,
                        error          TEXT,
                        duration_ms    INTEGER NOT NULL,
                        first_byte_ms  INTEGER,
                        created_at     INTEGER NOT NULL DEFAULT (unixepoch())
                     );
                     INSERT INTO request_usage
                       (route_id, provider, model, input_tokens, output_tokens,
                        status, duration_ms)
                     VALUES ('route', 'provider', 'model', 1, 2, 200, 10);",
                )
                .unwrap();
        }

        // Opening twice proves the additive migration is idempotent.
        let _first = UsageStore::open(path.clone()).unwrap();
        let store = UsageStore::open(path).unwrap();
        let connection = store.connection.lock().expect("usage db poisoned");

        let cached_input_tokens = connection
            .query_row(
                "SELECT cached_input_tokens FROM request_usage WHERE route_id = 'route'",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )
            .unwrap();
        assert_eq!(cached_input_tokens, None);

        let request_id = connection
            .query_row(
                "SELECT request_id FROM request_usage WHERE route_id = 'route'",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap();
        assert_eq!(request_id, None);
    }

    #[test]
    fn diagnostic_event_is_stored_with_kind_and_payload() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        let event = vellum_proxy_runtime::DiagnosticEvent::CompactionDecision(
            vellum_proxy_runtime::CompactionDecision {
                engine: vellum_proxy_runtime::CompactionEngine::Canonical,
                outcome: vellum_proxy_runtime::CompactionOutcome::Skipped,
                window: 128_000,
                active_tokens: 100,
                pending_user_tokens: 10,
                pending_tool_tokens: 5,
                output_reserve: 1_000,
                tool_reserve: 500,
                threshold_percent: 60,
                items_before: None,
                items_after: None,
                tokens_before: None,
                tokens_after: None,
                checkpoint_hash: None,
                checkpoint_id: None,
                generation: None,
                reason: None,
            },
        );
        store.record_diagnostic_event(&event).unwrap();

        let connection = store.connection.lock().expect("usage db poisoned");
        let kind = connection
            .query_row("SELECT kind FROM diagnostic_events", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap();
        assert_eq!(kind, "compaction_decision");
        let payload = connection
            .query_row("SELECT payload FROM diagnostic_events", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap();
        assert!(payload.contains("\"skipped\""));
    }

    /// `compaction_log_inner` used to project the full diagnostic payload
    /// down to just engine/outcome/reason/tokensBefore/tokensAfter, silently
    /// dropping window/thresholdPercent/itemsBefore/itemsAfter and — most
    /// importantly — `checkpointId`/`generation`, the very fields added to
    /// fix a diagnostics-honesty bug (a stateless standalone compact
    /// reporting a checkpoint id that was never journaled). A reader of the
    /// Log screen's compaction history had no way to see any of that detail
    /// even though it was captured in full in the stored payload.
    #[test]
    fn compaction_log_surfaces_the_full_stored_diagnostic_not_a_thin_slice() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        let event = vellum_proxy_runtime::DiagnosticEvent::CompactionDecision(
            vellum_proxy_runtime::CompactionDecision {
                engine: vellum_proxy_runtime::CompactionEngine::LocalTrigger,
                outcome: vellum_proxy_runtime::CompactionOutcome::Compacted,
                window: 128_000,
                active_tokens: 90_000,
                pending_user_tokens: 10,
                pending_tool_tokens: 5,
                output_reserve: 1_000,
                tool_reserve: 500,
                threshold_percent: 60,
                items_before: Some(42),
                items_after: Some(9),
                tokens_before: Some(90_000),
                tokens_after: Some(12_000),
                checkpoint_hash: Some("deadbeef".into()),
                checkpoint_id: Some("chk-123".into()),
                generation: Some(2),
                reason: Some("codex_trigger".into()),
            },
        );
        store.record_diagnostic_event(&event).unwrap();

        let log = store.request_log(10).unwrap();
        let entry = log.compaction_events.first().expect("one compaction event");
        assert_eq!(entry.window, Some(128_000));
        assert_eq!(entry.active_tokens, Some(90_000));
        assert_eq!(entry.threshold_percent, Some(60));
        assert_eq!(entry.items_before, Some(42));
        assert_eq!(entry.items_after, Some(9));
        assert_eq!(entry.checkpoint_id.as_deref(), Some("chk-123"));
        assert_eq!(entry.generation, Some(2));
    }

    /// The honesty fix this session (`execute_compact` in
    /// `vellum-proxy-runtime`) reports `checkpointId: None` for a stateless
    /// standalone compact — no `previous_response_id`, never journaled. The
    /// Log screen must show that absence as-is, not as a fabricated id.
    #[test]
    fn compaction_log_leaves_checkpoint_id_absent_when_never_journaled() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        let event = vellum_proxy_runtime::DiagnosticEvent::CompactionDecision(
            vellum_proxy_runtime::CompactionDecision {
                engine: vellum_proxy_runtime::CompactionEngine::LocalTrigger,
                outcome: vellum_proxy_runtime::CompactionOutcome::Compacted,
                window: 128_000,
                active_tokens: 90_000,
                pending_user_tokens: 10,
                pending_tool_tokens: 5,
                output_reserve: 1_000,
                tool_reserve: 500,
                threshold_percent: 60,
                items_before: Some(42),
                items_after: Some(9),
                tokens_before: Some(90_000),
                tokens_after: Some(12_000),
                checkpoint_hash: Some("deadbeef".into()),
                checkpoint_id: None,
                generation: None,
                reason: Some("standalone_compact".into()),
            },
        );
        store.record_diagnostic_event(&event).unwrap();

        let log = store.request_log(10).unwrap();
        let entry = log.compaction_events.first().expect("one compaction event");
        assert_eq!(entry.checkpoint_id, None);
        assert_eq!(entry.generation, None);
    }

    #[test]
    fn request_log_pages_entries_without_returning_the_whole_window() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        for index in 0..5 {
            let mut row = child_usage_fixture(
                &format!("req-{index}"),
                if index == 3 { "route-b" } else { "route-a" },
                "model",
                if index == 1 { 429 } else { 200 },
                Some("success"),
                None,
            );
            row.provider = if index == 3 {
                "Beta".into()
            } else {
                "Alpha".into()
            };
            store.record(&row).unwrap();
        }

        let page = store
            .request_log_page(&RequestLogPage {
                limit: 2,
                offset: 2,
                ..RequestLogPage::default()
            })
            .unwrap();
        assert_eq!(page.entry_total, 5);
        assert_eq!(page.entry_offset, 2);
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.entry_routes.len(), 2);

        let failed = store
            .request_log_page(&RequestLogPage {
                limit: 10,
                failed_only: true,
                ..RequestLogPage::default()
            })
            .unwrap();
        assert_eq!(failed.entry_total, 1);
        assert_eq!(failed.entries.len(), 1);
        assert_eq!(failed.entries[0].status, 429);

        let focused = store
            .request_log_page(&RequestLogPage {
                limit: 2,
                focus_entry_id: Some(page.entries[0].id),
                ..RequestLogPage::default()
            })
            .unwrap();
        assert_eq!(focused.entry_offset, 2);
        assert!(focused
            .entries
            .iter()
            .any(|entry| entry.id == page.entries[0].id));
    }

    fn spawn_requested_fixture(
        call_id: &str,
        parent_request_id: &str,
        model: &str,
    ) -> vellum_proxy_runtime::DiagnosticEvent {
        vellum_proxy_runtime::DiagnosticEvent::SpawnRequested(
            vellum_proxy_runtime::SpawnRequested {
                parent_request_id: parent_request_id.into(),
                call_id: call_id.into(),
                tool_name: "multi_agent_spawn".into(),
                arguments: serde_json::json!({}),
                arguments_len: 0,
                prompt_hash: vellum_proxy_runtime::hash_text("fixture prompt"),
                model: Some(model.into()),
                reasoning_effort: Some("medium".into()),
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn child_turn_fixture(
        call_id: Option<&str>,
        parent_request_id: Option<&str>,
        request_id: &str,
        route_id: &str,
        model: &str,
        link_method: vellum_proxy_runtime::SubagentLinkMethod,
        link_confidence: vellum_proxy_runtime::LinkConfidence,
    ) -> vellum_proxy_runtime::DiagnosticEvent {
        vellum_proxy_runtime::DiagnosticEvent::ChildTurn(vellum_proxy_runtime::ChildTurn {
            request_id: request_id.into(),
            route_id: route_id.into(),
            call_id: call_id.map(str::to_string),
            parent_request_id: parent_request_id.map(str::to_string),
            model: model.into(),
            effort: Some("medium".into()),
            link_method,
            link_confidence,
            instructions_hash: None,
            instructions_len: None,
            instructions_prefix: None,
            input_item_count: 0,
            input_manifest: Vec::new(),
            estimated_input_tokens: 0,
            full_context: None,
            ..Default::default()
        })
    }

    fn spawn_completed_fixture(
        call_id: &str,
        child_request_id: Option<&str>,
        duration_ms: u64,
    ) -> vellum_proxy_runtime::DiagnosticEvent {
        vellum_proxy_runtime::DiagnosticEvent::SpawnCompleted(
            vellum_proxy_runtime::SpawnCompleted {
                call_id: call_id.into(),
                output: "ok".into(),
                output_len: 2,
                output_hash: Some(vellum_proxy_runtime::hash_text("ok")),
                duration_ms,
                child_request_id: child_request_id.map(str::to_string),
            },
        )
    }

    #[test]
    fn support_diagnostic_log_removes_t3_context_and_reports_its_bound() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        let mut event = child_turn_fixture(
            Some("call-private"),
            Some("parent-private"),
            "request-private",
            "route-a",
            "model-a",
            vellum_proxy_runtime::SubagentLinkMethod::ContentHash,
            vellum_proxy_runtime::LinkConfidence::High,
        );
        let vellum_proxy_runtime::DiagnosticEvent::ChildTurn(child) = &mut event else {
            unreachable!();
        };
        child.full_context = Some(serde_json::json!({"prompt": "must-not-export"}));
        store.record_diagnostic_event(&event).unwrap();

        let (jsonl, truncated) = store.diagnostic_log_for_support(64 * 1024).unwrap();
        assert!(!truncated);
        assert!(jsonl.contains("request-private"));
        assert!(!jsonl.contains("fullContext"));
        assert!(!jsonl.contains("must-not-export"));

        let (empty, truncated) = store.diagnostic_log_for_support(1).unwrap();
        assert!(truncated);
        assert!(empty.is_empty());
    }

    fn native_graph_fixture(
        parent_thread_id: &str,
        child_thread_id: &str,
    ) -> vellum_proxy_runtime::DiagnosticEvent {
        vellum_proxy_runtime::DiagnosticEvent::SubagentGraphLinked(
            vellum_proxy_runtime::SubagentGraphLinked {
                parent_thread_id: parent_thread_id.into(),
                child_thread_id: child_thread_id.into(),
                parent_turn_id: None,
                root_turn_id: None,
                method: vellum_proxy_runtime::SubagentLinkMethod::OfficialThreadMetadata,
                confidence: vellum_proxy_runtime::LinkConfidence::High,
            },
        )
    }

    fn child_usage_fixture(
        request_id: &str,
        route_id: &str,
        model: &str,
        status: u16,
        outcome: Option<&str>,
        error: Option<&str>,
    ) -> UsageRecord {
        UsageRecord {
            route_id: route_id.into(),
            provider: "fixture-provider".into(),
            model: model.into(),
            status,
            error: error.map(str::to_string),
            duration_ms: 42,
            request_id: Some(request_id.into()),
            outcome: outcome.map(str::to_string),
            ..Default::default()
        }
    }

    fn runs_for(store: &UsageStore) -> Vec<SubagentRun> {
        let connection = store.connection.lock().expect("usage db poisoned");
        UsageStore::subagent_runs_inner(&connection, 100).unwrap()
    }

    /// The base-case shape described in the runtime diagnostics test file:
    /// one spawn, one high-confidence child link, one completion — all
    /// consistent, none duplicated.
    #[test]
    fn subagent_run_normal_spawn_produces_exactly_one_completed_run() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();

        store
            .record_diagnostic_event(&spawn_requested_fixture(
                "call-ok",
                "parent-1",
                "child-model",
            ))
            .unwrap();
        store
            .record_diagnostic_event(&child_turn_fixture(
                Some("call-ok"),
                Some("parent-1"),
                "child-ok",
                "route-a",
                "child-model",
                vellum_proxy_runtime::SubagentLinkMethod::ContentHash,
                vellum_proxy_runtime::LinkConfidence::High,
            ))
            .unwrap();
        store
            .record(&child_usage_fixture(
                "child-ok",
                "route-a",
                "child-model",
                200,
                Some("success"),
                None,
            ))
            .unwrap();
        store
            .record_diagnostic_event(&spawn_completed_fixture("call-ok", Some("child-ok"), 500))
            .unwrap();

        let runs = runs_for(&store);
        assert_eq!(runs.len(), 1, "exactly one run, not one per raw event");
        let run = &runs[0];
        assert_eq!(run.call_id.as_deref(), Some("call-ok"));
        assert_eq!(run.parent_request_id.as_deref(), Some("parent-1"));
        assert_eq!(run.child_request_id.as_deref(), Some("child-ok"));
        assert_eq!(run.route_id.as_deref(), Some("route-a"));
        assert_eq!(run.model.as_deref(), Some("child-model"));
        assert_eq!(run.effort.as_deref(), Some("medium"));
        assert_eq!(run.link_confidence, SubagentLinkConfidence::Exact);
        assert_eq!(run.outcome.as_deref(), Some("success"));
        assert_eq!(run.state, SubagentRunState::Completed);
        // The public identity is a stable short hash, never the raw id; the
        // child's own usage row is the navigation anchor.
        assert_eq!(
            run.parent_request_hash,
            Some(vellum_proxy_runtime::hash_text("parent-1"))
        );
        assert_eq!(
            run.child_request_hash,
            Some(vellum_proxy_runtime::hash_text("child-ok"))
        );
        assert!(run.child_usage_entry_id.is_some());
        assert!(
            run.parent_usage_entry_id.is_none(),
            "no parent usage row in this fixture"
        );
    }

    #[test]
    fn native_graph_edges_are_counted_once_and_join_real_child_usage() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        let parent = "01-native-parent";
        let child = "01-native-child";

        // Re-observing official metadata must not inflate the child count.
        store
            .record_diagnostic_event(&native_graph_fixture(parent, child))
            .unwrap();
        store
            .record_diagnostic_event(&native_graph_fixture(parent, child))
            .unwrap();
        let mut child_usage = child_usage_fixture(
            "child-provider-request",
            "route-qwen",
            "qwen",
            200,
            Some("success"),
            None,
        );
        child_usage.conversation_identity = Some(format!("codex:root:{child}"));
        store.record(&child_usage).unwrap();
        {
            let connection = store.connection.lock().expect("usage db poisoned");
            connection
                .execute(
                    "UPDATE diagnostic_events SET created_at = unixepoch() - 901
                     WHERE kind = 'subagent_graph_linked'",
                    [],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE request_usage SET created_at = unixepoch() - 901
                     WHERE conversation_identity = ?1",
                    [format!("codex:root:{child}")],
                )
                .unwrap();
        }

        let runs = runs_for(&store);
        assert_eq!(runs.len(), 1, "one native graph edge is one child run");
        let run = &runs[0];
        assert_eq!(run.state, SubagentRunState::Completed);
        assert_eq!(run.route_id.as_deref(), Some("route-qwen"));
        assert_eq!(run.model.as_deref(), Some("qwen"));
        assert_eq!(run.link_confidence, SubagentLinkConfidence::Exact);
        assert!(run.child_usage_entry_id.is_some());
        let encoded = serde_json::to_string(run).unwrap();
        assert!(!encoded.contains(parent));
        assert!(!encoded.contains(child));
    }

    /// The public run JSON exposes only hashed identities and local entry
    /// anchors — never the raw parent/child request ids.
    #[test]
    fn subagent_run_serializes_hashed_identity_not_raw_request_ids() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        store
            .record_diagnostic_event(&spawn_requested_fixture(
                "call-h",
                "parent-raw-1",
                "child-model",
            ))
            .unwrap();
        store
            .record_diagnostic_event(&child_turn_fixture(
                Some("call-h"),
                Some("parent-raw-1"),
                "child-raw-1",
                "route-a",
                "child-model",
                vellum_proxy_runtime::SubagentLinkMethod::ContentHash,
                vellum_proxy_runtime::LinkConfidence::High,
            ))
            .unwrap();
        store
            .record(&child_usage_fixture(
                "child-raw-1",
                "route-a",
                "child-model",
                200,
                Some("success"),
                None,
            ))
            .unwrap();
        store
            .record_diagnostic_event(&spawn_completed_fixture("call-h", Some("child-raw-1"), 500))
            .unwrap();

        let runs = runs_for(&store);
        let encoded = serde_json::to_string(&runs).unwrap();
        assert!(
            !encoded.contains("parent-raw-1"),
            "raw parent id leaked: {encoded}"
        );
        assert!(
            !encoded.contains("child-raw-1"),
            "raw child id leaked: {encoded}"
        );
        assert!(encoded.contains("parentRequestHash"));
        assert!(encoded.contains("childRequestHash"));
        assert!(encoded.contains("childUsageEntryId"));
    }

    /// The exact bug this feature closes: a `SpawnCompleted` event firing is
    /// not proof the child succeeded. When the joined child's own usage row
    /// says the provider failed, the run must land in `Failed`, never
    /// `Completed`.
    #[test]
    fn subagent_run_lands_failed_when_child_provider_errors_despite_spawn_completed() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();

        store
            .record_diagnostic_event(&spawn_requested_fixture(
                "call-fail",
                "parent-1",
                "child-model",
            ))
            .unwrap();
        store
            .record_diagnostic_event(&child_turn_fixture(
                Some("call-fail"),
                Some("parent-1"),
                "child-fail",
                "route-a",
                "child-model",
                vellum_proxy_runtime::SubagentLinkMethod::ContentHash,
                vellum_proxy_runtime::LinkConfidence::High,
            ))
            .unwrap();
        store
            .record(&child_usage_fixture(
                "child-fail",
                "route-a",
                "child-model",
                502,
                Some("provider_failure"),
                Some("upstream 502"),
            ))
            .unwrap();
        // The parent still got a `function_call_output` back (perhaps the
        // harness surfaced the error text as the tool result) — this must
        // not be enough to call the run a success.
        store
            .record_diagnostic_event(&spawn_completed_fixture(
                "call-fail",
                Some("child-fail"),
                500,
            ))
            .unwrap();

        let runs = runs_for(&store);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].state, SubagentRunState::Failed);
        assert_eq!(runs[0].outcome.as_deref(), Some("provider_failure"));
    }

    #[test]
    fn subagent_run_lands_cancelled_on_explicit_client_cancel() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();

        store
            .record_diagnostic_event(&spawn_requested_fixture(
                "call-cancel",
                "parent-1",
                "child-model",
            ))
            .unwrap();
        store
            .record_diagnostic_event(&child_turn_fixture(
                Some("call-cancel"),
                Some("parent-1"),
                "child-cancel",
                "route-a",
                "child-model",
                vellum_proxy_runtime::SubagentLinkMethod::ContentHash,
                vellum_proxy_runtime::LinkConfidence::High,
            ))
            .unwrap();
        store
            .record(&child_usage_fixture(
                "child-cancel",
                "route-a",
                "child-model",
                499,
                Some("client_cancel"),
                None,
            ))
            .unwrap();

        let runs = runs_for(&store);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].state, SubagentRunState::Cancelled);
    }

    #[test]
    fn subagent_run_lands_cancelled_on_parent_disconnect_mid_spawn() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();

        store
            .record_diagnostic_event(&spawn_requested_fixture(
                "call-disconnect",
                "parent-1",
                "child-model",
            ))
            .unwrap();
        store
            .record_diagnostic_event(&child_turn_fixture(
                Some("call-disconnect"),
                Some("parent-1"),
                "child-disconnect",
                "route-a",
                "child-model",
                vellum_proxy_runtime::SubagentLinkMethod::ContentHash,
                vellum_proxy_runtime::LinkConfidence::High,
            ))
            .unwrap();
        store
            .record(&child_usage_fixture(
                "child-disconnect",
                "route-a",
                "child-model",
                499,
                Some("client_disconnect"),
                None,
            ))
            .unwrap();

        let runs = runs_for(&store);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].state, SubagentRunState::Cancelled);
    }

    /// Two parallel spawns sharing an identical prompt: the runtime already
    /// refuses to guess which child answered which `call_id` (an orphan
    /// `ChildTurn` with `call_id = None`). The aggregation must not invent a
    /// hard link either — both spawns land `Ambiguous`, and the orphan child
    /// becomes its own `Unlinked` run.
    #[test]
    fn subagent_run_marks_parallel_ambiguous_spawns_without_a_false_hard_link() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();

        store
            .record_diagnostic_event(&spawn_requested_fixture(
                "call-a",
                "parent-a",
                "child-model",
            ))
            .unwrap();
        store
            .record_diagnostic_event(&spawn_requested_fixture(
                "call-b",
                "parent-b",
                "child-model",
            ))
            .unwrap();
        store
            .record_diagnostic_event(&child_turn_fixture(
                None,
                None,
                "child-ambiguous",
                "route-a",
                "child-model",
                vellum_proxy_runtime::SubagentLinkMethod::None,
                vellum_proxy_runtime::LinkConfidence::None,
            ))
            .unwrap();

        let runs = runs_for(&store);
        assert_eq!(runs.len(), 3);
        let call_a = runs
            .iter()
            .find(|run| run.call_id.as_deref() == Some("call-a"))
            .expect("call-a run");
        let call_b = runs
            .iter()
            .find(|run| run.call_id.as_deref() == Some("call-b"))
            .expect("call-b run");
        let orphan = runs
            .iter()
            .find(|run| run.call_id.is_none())
            .expect("orphan run");
        assert_eq!(call_a.state, SubagentRunState::Ambiguous);
        assert_eq!(call_b.state, SubagentRunState::Ambiguous);
        assert_eq!(orphan.state, SubagentRunState::Unlinked);
        // Neither ambiguous spawn may claim the orphan child as its own.
        assert_eq!(call_a.child_request_id, None);
        assert_eq!(call_b.child_request_id, None);
        assert_eq!(orphan.child_request_id.as_deref(), Some("child-ambiguous"));
    }

    /// The store is sqlite-backed, so a run built purely from durable rows
    /// (subagent_events + request_usage) must reconstruct identically after
    /// the process restarts and reopens the same file.
    #[test]
    fn subagent_run_reconstructs_identically_after_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.sqlite3");
        {
            let store = UsageStore::open(path.clone()).unwrap();
            store
                .record_diagnostic_event(&spawn_requested_fixture(
                    "call-durable",
                    "parent-1",
                    "child-model",
                ))
                .unwrap();
            store
                .record_diagnostic_event(&child_turn_fixture(
                    Some("call-durable"),
                    Some("parent-1"),
                    "child-durable",
                    "route-a",
                    "child-model",
                    vellum_proxy_runtime::SubagentLinkMethod::ContentHash,
                    vellum_proxy_runtime::LinkConfidence::High,
                ))
                .unwrap();
            store
                .record(&child_usage_fixture(
                    "child-durable",
                    "route-a",
                    "child-model",
                    200,
                    Some("success"),
                    None,
                ))
                .unwrap();
            store
                .record_diagnostic_event(&spawn_completed_fixture(
                    "call-durable",
                    Some("child-durable"),
                    500,
                ))
                .unwrap();
        }

        let reopened = UsageStore::open(path).unwrap();
        let runs = runs_for(&reopened);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].call_id.as_deref(), Some("call-durable"));
        assert_eq!(runs[0].state, SubagentRunState::Completed);
        assert_eq!(runs[0].child_request_id.as_deref(), Some("child-durable"));
    }

    /// T1 default: the run-aggregation layer must never surface prompt text,
    /// output text, token content, email addresses or account ids, even when
    /// the raw diagnostic feed underneath happens to carry a bounded prefix
    /// of them (existing `DetailLevel::T1Summary` behavior, unrelated to this
    /// layer).
    #[test]
    fn subagent_run_serialization_carries_no_prompt_output_email_or_account_id() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        let secret_needles = [
            "do-not-leak-this-secret-token",
            "person@example.com",
            "acct_1234567890",
        ];

        store
            .record_diagnostic_event(&spawn_requested_fixture(
                "call-t1",
                "parent-1",
                "child-model",
            ))
            .unwrap();
        store
            .record_diagnostic_event(&vellum_proxy_runtime::DiagnosticEvent::ChildTurn(
                vellum_proxy_runtime::ChildTurn {
                    request_id: "child-t1".into(),
                    route_id: "route-a".into(),
                    call_id: Some("call-t1".into()),
                    parent_request_id: Some("parent-1".into()),
                    model: "child-model".into(),
                    effort: Some("medium".into()),
                    link_method: vellum_proxy_runtime::SubagentLinkMethod::ContentHash,
                    link_confidence: vellum_proxy_runtime::LinkConfidence::High,
                    instructions_hash: Some(vellum_proxy_runtime::hash_text(secret_needles[0])),
                    instructions_len: Some(secret_needles[0].len() as u64),
                    instructions_prefix: Some(format!(
                        "contact {} account {} secret: {}",
                        secret_needles[1], secret_needles[2], secret_needles[0]
                    )),
                    input_item_count: 0,
                    input_manifest: Vec::new(),
                    estimated_input_tokens: 0,
                    full_context: None,
                    ..Default::default()
                },
            ))
            .unwrap();
        store
            .record(&child_usage_fixture(
                "child-t1",
                "route-a",
                "child-model",
                200,
                Some("success"),
                None,
            ))
            .unwrap();
        store
            .record_diagnostic_event(&spawn_completed_fixture("call-t1", Some("child-t1"), 500))
            .unwrap();

        let runs = runs_for(&store);
        let encoded = serde_json::to_string(&runs).unwrap();
        for needle in secret_needles {
            assert!(
                !encoded.contains(needle),
                "run-layer JSON leaked bounded T1 content: {needle}"
            );
        }
    }

    #[test]
    fn harness_parent_and_child_usage_survives_in_a_separate_attribution_ledger() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        for (agent_id, parent_agent_id, role, input, output) in [
            ("agent_root", None, "parent", 100, 20),
            ("agent_1", Some("agent_root"), "child", 40, 8),
            ("agent_root", None, "parent", 120, 12),
        ] {
            store
                .record_harness_attribution(&HarnessUsageRecord {
                    route_id: "grok".into(),
                    provider: "Grok Build".into(),
                    session_hash: "sha256:session".into(),
                    agent_id: agent_id.into(),
                    parent_agent_id: parent_agent_id.map(str::to_string),
                    role: role.into(),
                    model: "grok-4.5".into(),
                    input_tokens: input,
                    output_tokens: output,
                    status: 200,
                    duration_ms: 10,
                })
                .unwrap();
        }

        let records = store
            .harness_attribution_for_session("sha256:session")
            .unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[1].role, "child");
        assert_eq!(records[1].parent_agent_id.as_deref(), Some("agent_root"));
        assert_eq!(
            records
                .iter()
                .map(|row| row.input_tokens + row.output_tokens)
                .sum::<u64>(),
            300
        );
    }

    #[test]
    fn provider_total_tokens_include_input_and_output_for_only_that_route() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        for (route_id, input, output) in [
            ("weikuwu", 100, 20),
            ("weikuwu", 200, 30),
            ("grok-cli", 9_000, 1_000),
        ] {
            store
                .record(&UsageRecord {
                    route_id: route_id.into(),
                    provider: route_id.into(),
                    model: "model".into(),
                    input_tokens: input,
                    output_tokens: output,
                    status: 200,
                    error: None,
                    duration_ms: 10,
                    first_byte_ms: None,
                    review_run_id: None,
                    review_role: None,
                    review_reason: None,
                    ..Default::default()
                })
                .unwrap();
        }
        let summary = store.summary("weikuwu").unwrap();
        assert_eq!(summary.turns, 2);
        assert_eq!(summary.total_tokens, 350);
    }

    #[test]
    fn latest_successful_route_ignores_failures_and_auto_review_traffic() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        for (route_id, provider, model, status, review_run_id) in [
            ("weikuwu", "weikuwu", "GLM-5.2", 200, None),
            ("grok-cli", "Grok Build", "grok-4.5", 500, None),
            (
                "openai",
                crate::state::OFFICIAL_ROUTE_NAME,
                "gpt-5.4-mini",
                200,
                Some("review-1"),
            ),
        ] {
            store
                .record(&UsageRecord {
                    route_id: route_id.into(),
                    provider: provider.into(),
                    model: model.into(),
                    input_tokens: 10,
                    output_tokens: 2,
                    status,
                    error: (status >= 400).then(|| "upstream failed".into()),
                    duration_ms: 10,
                    first_byte_ms: None,
                    review_run_id: review_run_id.map(str::to_string),
                    review_role: review_run_id.map(|_| "primary".into()),
                    review_reason: None,
                    ..Default::default()
                })
                .unwrap();
        }

        let telemetry = store.latest_successful_route().unwrap().unwrap();
        assert_eq!(telemetry.route_id, "weikuwu");
        assert_eq!(telemetry.provider, "weikuwu");
        assert_eq!(telemetry.model, "GLM-5.2");
    }

    /* 每一列都存了當下的供應商名稱。同一條線路改過名（使用者改的，或是
    Vellum 換掉內建名稱）之後，舊列跟新列的名稱不一樣 —— 以名稱分組
    就會把同一家裂成兩列，帳看起來像多了一家供應商。 */
    #[test]
    fn renaming_a_provider_does_not_split_its_totals_into_two_rows() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        for (provider, input) in [
            ("OpenAI 官方", 1_000_u64),
            (crate::state::OFFICIAL_ROUTE_NAME, 500),
        ] {
            store
                .record(&UsageRecord {
                    route_id: "openai-official".into(),
                    provider: provider.into(),
                    model: "gpt-5.6-sol".into(),
                    input_tokens: input,
                    output_tokens: 0,
                    status: 200,
                    error: None,
                    duration_ms: 10,
                    first_byte_ms: None,
                    review_run_id: None,
                    review_role: None,
                    review_reason: None,
                    ..Default::default()
                })
                .unwrap();
        }

        let activity = store.activity(&[]).unwrap();
        assert_eq!(activity.providers.len(), 1);
        assert_eq!(activity.providers[0].route_id, "openai-official");
        assert_eq!(activity.providers[0].tokens, 1_500);

        // 同一天的兩筆也不能因為名稱不同就拆成兩列。
        assert_eq!(activity.days.len(), 1);
        assert_eq!(activity.days[0].tokens, 1_500);
    }

    #[test]
    fn activity_excludes_authoritative_official_route_without_dropping_third_parties() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        for (route_id, provider, input, output, duration) in [
            ("openai", crate::state::OFFICIAL_ROUTE_NAME, 1_000, 20, 90),
            ("grok", "Grok Build", 300, 40, 120),
            ("grok", "Grok Build", 500, 60, 250),
        ] {
            store
                .record(&UsageRecord {
                    route_id: route_id.into(),
                    provider: provider.into(),
                    model: "model".into(),
                    input_tokens: input,
                    output_tokens: output,
                    status: 200,
                    error: None,
                    duration_ms: duration,
                    first_byte_ms: None,
                    review_run_id: None,
                    review_role: None,
                    review_reason: None,
                    ..Default::default()
                })
                .unwrap();
        }

        let activity = store.activity(&["openai".into()]).unwrap();
        assert_eq!(activity.providers.len(), 1);
        assert_eq!(activity.providers[0].route_id, "grok");
        assert_eq!(activity.providers[0].tokens, 900);
        assert_eq!(activity.days.iter().map(|day| day.tokens).sum::<u64>(), 900);
        assert_eq!(activity.longest_request_ms, 250);
    }

    #[test]
    fn review_stats_count_logical_runs_and_fallback_provider() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        for (route_id, provider, model, status, role, reason) in [
            ("grok-cli", "Grok Build", "grok-4.5", 429, "primary", None),
            (
                "openai",
                "OpenAI Official",
                "gpt-5.4-mini",
                200,
                "fallback",
                Some("Grok Build unavailable (HTTP 429)"),
            ),
        ] {
            store
                .record(&UsageRecord {
                    route_id: route_id.into(),
                    provider: provider.into(),
                    model: model.into(),
                    input_tokens: 0,
                    output_tokens: 0,
                    status,
                    error: (status >= 400).then(|| "quota".into()),
                    duration_ms: 10,
                    first_byte_ms: None,
                    review_run_id: Some("review-1".into()),
                    review_role: Some(role.into()),
                    review_reason: reason.map(str::to_string),
                    ..Default::default()
                })
                .unwrap();
        }
        let stats = store.review_stats().unwrap();
        assert_eq!(stats.total_runs, 1);
        assert_eq!(stats.fallback_runs, 1);
        assert_eq!(stats.active_route_id.as_deref(), Some("openai"));
        assert!(stats.active_is_fallback);
        assert_eq!(stats.providers.len(), 2);
    }

    #[test]
    fn review_stats_include_legacy_codex_auto_review_requests() {
        let temp = tempfile::tempdir().unwrap();
        let store = UsageStore::open(temp.path().join("usage.sqlite3")).unwrap();
        store
            .record(&UsageRecord {
                route_id: "openai".into(),
                provider: "OpenAI Official".into(),
                model: AUTO_REVIEW_MODEL.into(),
                input_tokens: 100,
                output_tokens: 12,
                status: 200,
                error: None,
                duration_ms: 10,
                first_byte_ms: None,
                review_run_id: None,
                review_role: None,
                review_reason: None,
                ..Default::default()
            })
            .unwrap();

        let stats = store.review_stats().unwrap();
        assert_eq!(stats.total_runs, 1);
        assert_eq!(stats.providers.len(), 1);
        assert_eq!(stats.providers[0].primary_runs, 1);
        assert_eq!(stats.active_model.as_deref(), Some(AUTO_REVIEW_MODEL));
    }
}
