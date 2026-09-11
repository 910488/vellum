//! Usage accounting (plan M7).
//!
//! The runtime records one [`UsageRecord`] per executed exchange so Desktop's
//! `UsageRecord` and the runtime's are directly comparable. The
//! [`UsageStore`] trait is the seam a real daemon implements; the default
//! [`MemoryUsageStore`] backs fixtures and tests.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One executed exchange. Mirrors Desktop's `UsageRecord` fields that carry
/// observable meaning (tokens, status, error, duration, first byte).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
    /// Correlates with the request that produced this record (E-2 already
    /// surfaces it; this persists it).
    #[serde(default)]
    pub request_id: Option<String>,
    /// WebSocket connection identity for this exchange, `None` until the
    /// bridge assigns one (D3).
    #[serde(default)]
    pub connection_id: Option<String>,
    /// Redacted conversation identity (`sha256:` prefix), never the raw id.
    #[serde(default)]
    pub conversation_identity: Option<String>,
    /// First parseable upstream event (SSE or application WS frame).
    #[serde(default)]
    pub first_event_ms: Option<u64>,
    /// First application frame successfully sent downstream.
    #[serde(default)]
    pub first_downstream_frame_ms: Option<u64>,
    /// Turn outcome: success, provider_failure, protocol_failure,
    /// client_cancel, client_disconnect.
    #[serde(default)]
    pub outcome: Option<String>,
    /// Stable error category (`provider_protocol`, `provider_quota`, ...).
    #[serde(default)]
    pub error_category: Option<String>,
    /// Named stage timestamps in milliseconds from turn start.
    #[serde(default)]
    pub stage_times_ms: Option<Value>,
    /// Stream-quality classification (`incremental` / `end_flush` /
    /// `buffered` / `no_delta`) computed from observed application deltas.
    /// `None` when the dispatch path does not observe deltas (Optional).
    #[serde(default)]
    pub stream_quality: Option<String>,
    /// Request-relative time of the first application output delta.
    #[serde(default)]
    pub first_output_delta_ms: Option<u64>,
    /// Request-relative time of the first explicit reasoning delta.
    #[serde(default)]
    pub first_reasoning_delta_ms: Option<u64>,
    /// Application output delta count.
    #[serde(default)]
    pub output_delta_count: u64,
    /// Explicit reasoning delta count, recorded independently.
    #[serde(default)]
    pub reasoning_delta_count: u64,
    /// Hashed Codex daemon / Remote control identity A. Never raw account id.
    #[serde(default)]
    pub control_account_hash: Option<String>,
    /// Hashed provider execution identity. Equals A for native passthrough,
    /// or identifies Vellum-managed Official account B.
    #[serde(default)]
    pub execution_account_hash: Option<String>,
    /// Revision of the immutable Official account selection snapshot.
    #[serde(default)]
    pub selection_revision: Option<u64>,
    /// OpenCode `anonymousFree` / `credentialed`. Never a secret.
    #[serde(default)]
    pub auth_mode: Option<String>,
    /// OpenCode `openCodeZen` / `openCodeGo`. Other providers leave this unset.
    #[serde(default)]
    pub provider_profile: Option<String>,
    /// Whether this row issued an upstream completion. Cooldown hits are
    /// `Some(false)`.
    #[serde(default)]
    pub upstream_attempted: Option<bool>,
    /// Seconds from `Retry-After` or the remaining local cooldown.
    #[serde(default)]
    pub retry_after: Option<u64>,
    /// Optional Codex subagent attribution for grouped analytics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_attribution: Option<AgentUsageAttribution>,
}

/// Optional subagent attribution metadata for usage records.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentUsageAttribution {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_method: Option<crate::diagnostics::SubagentLinkMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_confidence: Option<crate::diagnostics::LinkConfidence>,
}

/// Build AgentUsageAttribution from observed turn identity and graph registry.
pub fn build_agent_usage_attribution(
    identity: Option<&crate::codex_metadata::CodexTurnIdentity>,
    graph: Option<&crate::subagent_graph::SubagentGraphRegistry>,
) -> Option<AgentUsageAttribution> {
    let id = identity?;
    if id.trust == crate::codex_metadata::CodexIdentityTrust::None && id.thread_id.is_none() {
        return None;
    }

    let depth = if id.trust == crate::codex_metadata::CodexIdentityTrust::Conflict {
        None
    } else if let (Some(g), Some(s_id), Some(t_id)) = (graph, &id.session_id, &id.thread_id) {
        let thread_key = crate::subagent_graph::CodexThreadKey::new(s_id.clone(), t_id.clone());
        g.node_depth(&thread_key, 32)
    } else {
        None
    };

    let link_method = match id.source {
        crate::codex_metadata::CodexIdentitySource::TurnMetadataHeader
        | crate::codex_metadata::CodexIdentitySource::CanonicalClientMetadata => {
            Some(crate::diagnostics::SubagentLinkMethod::OfficialThreadMetadata)
        }
        crate::codex_metadata::CodexIdentitySource::FlatCompatibilityHeaders => {
            Some(crate::diagnostics::SubagentLinkMethod::CompatibilityHeader)
        }
        _ => None,
    };

    let link_confidence = match id.trust {
        crate::codex_metadata::CodexIdentityTrust::Exact
        | crate::codex_metadata::CodexIdentityTrust::Structured => {
            Some(crate::diagnostics::LinkConfidence::High)
        }
        crate::codex_metadata::CodexIdentityTrust::Partial => {
            Some(crate::diagnostics::LinkConfidence::Medium)
        }
        _ => None,
    };

    Some(AgentUsageAttribution {
        session_id: id.session_id.as_deref().map(str::to_string),
        thread_id: id.thread_id.as_deref().map(str::to_string),
        parent_thread_id: id.parent_thread_id.as_deref().map(str::to_string),
        parent_turn_id: id.parent_turn_id.as_deref().map(str::to_string),
        root_turn_id: id.root_turn_id.as_deref().map(str::to_string),
        context_window_id: id.context_window_id.as_deref().map(str::to_string),
        agent_name: id.agent_name.clone(),
        subagent_kind: id.subagent_kind.clone(),
        depth,
        link_method,
        link_confidence,
    })
}

/// Aggregated usage for one route (Desktop `UsageSummary` equivalent).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummary {
    pub latest_input_tokens: u64,
    pub turns: u32,
    pub total_tokens: u64,
    pub latest_first_byte_ms: Option<u32>,
}

/// Durable usage backing. Implementations must be `Send + Sync`.
pub trait UsageStore: Send + Sync {
    fn record(&self, value: &UsageRecord) -> Result<(), String>;
    fn summary(&self, route_id: &str) -> Result<UsageSummary, String>;
    fn records(&self) -> Result<Vec<UsageRecord>, String>;
    /// Stamp the first successful downstream application-frame send on the
    /// matching in-flight record. HTTP-only paths leave the field unset.
    fn mark_first_downstream_frame(&self, request_id: &str, elapsed_ms: u64) -> Result<(), String>;
}

/// In-memory usage store, the default for fixtures and tests.
#[derive(Debug, Default)]
pub struct MemoryUsageStore {
    records: Mutex<Vec<UsageRecord>>,
}

impl MemoryUsageStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// All recorded records (newest last), for diagnostics and tests.
    pub fn records(&self) -> Vec<UsageRecord> {
        self.records
            .lock()
            .map(|records| records.clone())
            .unwrap_or_default()
    }
}

impl UsageStore for MemoryUsageStore {
    fn record(&self, value: &UsageRecord) -> Result<(), String> {
        self.records
            .lock()
            .map_err(|_| "memory usage store lock poisoned".to_string())?
            .push(value.clone());
        Ok(())
    }

    fn summary(&self, route_id: &str) -> Result<UsageSummary, String> {
        let records = self
            .records
            .lock()
            .map_err(|_| "memory usage store lock poisoned".to_string())?
            .iter()
            .filter(|record| record.route_id == route_id)
            .cloned()
            .collect::<Vec<_>>();
        let latest = records.last();
        Ok(UsageSummary {
            latest_input_tokens: latest.map(|record| record.input_tokens).unwrap_or(0),
            turns: records.len() as u32,
            total_tokens: records
                .iter()
                .map(|record| record.input_tokens + record.output_tokens)
                .sum(),
            latest_first_byte_ms: latest
                .and_then(|record| record.first_byte_ms)
                .map(|value| value as u32),
        })
    }

    fn records(&self) -> Result<Vec<UsageRecord>, String> {
        self.records
            .lock()
            .map(|records| records.clone())
            .map_err(|_| "memory usage store lock poisoned".to_string())
    }

    fn mark_first_downstream_frame(&self, request_id: &str, elapsed_ms: u64) -> Result<(), String> {
        let mut records = self
            .records
            .lock()
            .map_err(|_| "memory usage store lock poisoned".to_string())?;
        if let Some(record) = records
            .iter_mut()
            .rev()
            .find(|record| record.request_id.as_deref() == Some(request_id))
        {
            if record.first_downstream_frame_ms.is_none() {
                record.first_downstream_frame_ms = Some(elapsed_ms);
            }
        }
        Ok(())
    }
}

/// Append-only durable usage ledger used by the headless daemon. Opening the
/// store validates every existing record, so readiness fails closed on a
/// corrupt or unreadable ledger rather than silently dropping accounting.
#[derive(Debug)]
pub struct FileUsageStore {
    path: PathBuf,
    records: Mutex<Vec<UsageRecord>>,
}

impl FileUsageStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create usage directory {}: {error}", parent.display()))?;
        }
        let mut records = Vec::new();
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .map_err(|error| format!("read usage ledger {}: {error}", path.display()))?;
            for (index, line) in raw.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                records.push(serde_json::from_str(line).map_err(|error| {
                    format!(
                        "parse usage record {} in {}: {error}",
                        index + 1,
                        path.display()
                    )
                })?);
            }
        } else {
            std::fs::File::create(&path)
                .and_then(|file| file.sync_all())
                .map_err(|error| format!("create usage ledger {}: {error}", path.display()))?;
        }
        Ok(Self {
            path,
            records: Mutex::new(records),
        })
    }
}

impl UsageStore for FileUsageStore {
    fn record(&self, value: &UsageRecord) -> Result<(), String> {
        let encoded =
            serde_json::to_vec(value).map_err(|error| format!("encode usage record: {error}"))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|error| format!("open usage ledger {}: {error}", self.path.display()))?;
        file.write_all(&encoded)
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.sync_data())
            .map_err(|error| format!("append usage ledger {}: {error}", self.path.display()))?;
        self.records
            .lock()
            .map_err(|_| "file usage store lock poisoned".to_string())?
            .push(value.clone());
        Ok(())
    }

    fn summary(&self, route_id: &str) -> Result<UsageSummary, String> {
        let records = self
            .records
            .lock()
            .map_err(|_| "file usage store lock poisoned".to_string())?;
        let matching = records
            .iter()
            .filter(|record| record.route_id == route_id)
            .collect::<Vec<_>>();
        let latest = matching.last().copied();
        Ok(UsageSummary {
            latest_input_tokens: latest.map(|record| record.input_tokens).unwrap_or(0),
            turns: matching.len() as u32,
            total_tokens: matching
                .iter()
                .map(|record| record.input_tokens + record.output_tokens)
                .sum(),
            latest_first_byte_ms: latest
                .and_then(|record| record.first_byte_ms)
                .map(|value| value as u32),
        })
    }

    fn records(&self) -> Result<Vec<UsageRecord>, String> {
        self.records
            .lock()
            .map(|records| records.clone())
            .map_err(|_| "file usage store lock poisoned".to_string())
    }

    fn mark_first_downstream_frame(&self, request_id: &str, elapsed_ms: u64) -> Result<(), String> {
        let mut records = self
            .records
            .lock()
            .map_err(|_| "file usage store lock poisoned".to_string())?;
        let Some(record) = records
            .iter_mut()
            .rev()
            .find(|record| record.request_id.as_deref() == Some(request_id))
        else {
            return Ok(());
        };
        if record.first_downstream_frame_ms.is_some() {
            return Ok(());
        }
        record.first_downstream_frame_ms = Some(elapsed_ms);
        persist_usage_records(&self.path, &records)
    }
}

fn persist_usage_records(path: &Path, records: &[UsageRecord]) -> Result<(), String> {
    let tmp = path.with_extension("jsonl.tmp");
    let mut file = std::fs::File::create(&tmp)
        .map_err(|error| format!("rewrite usage ledger {}: {error}", tmp.display()))?;
    for record in records {
        let encoded =
            serde_json::to_vec(record).map_err(|error| format!("encode usage record: {error}"))?;
        file.write_all(&encoded)
            .and_then(|_| file.write_all(b"\n"))
            .map_err(|error| format!("write usage ledger {}: {error}", tmp.display()))?;
    }
    file.sync_data()
        .map_err(|error| format!("sync usage ledger {}: {error}", tmp.display()))?;
    drop(file);
    std::fs::rename(&tmp, path)
        .map_err(|error| format!("persist rewritten usage ledger {}: {error}", path.display()))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageDetails {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub tool_calls: u64,
    pub compaction_tokens: u64,
}

/// Extract `(input_tokens, output_tokens)` from a normalized response
/// (Desktop `usage_from_response` equivalent: Responses wire, with
/// Chat-style `prompt_tokens`/`completion_tokens` fallbacks).
pub fn usage_from_response(response: &Value) -> (u64, u64) {
    let details = usage_details_from_response(response);
    (details.input_tokens, details.output_tokens)
}

pub fn usage_details_from_response(response: &Value) -> UsageDetails {
    let usage = response.get("usage");
    let input = usage
        .and_then(|usage| {
            usage
                .get("input_tokens")
                .or_else(|| usage.get("prompt_tokens"))
        })
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .and_then(|usage| {
            usage
                .get("output_tokens")
                .or_else(|| usage.get("completion_tokens"))
        })
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached_input_tokens = usage
        .and_then(|usage| {
            usage
                .pointer("/input_tokens_details/cached_tokens")
                .or_else(|| usage.pointer("/prompt_tokens_details/cached_tokens"))
        })
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning_tokens = usage
        .and_then(|usage| {
            usage
                .pointer("/output_tokens_details/reasoning_tokens")
                .or_else(|| usage.pointer("/completion_tokens_details/reasoning_tokens"))
        })
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tool_calls = response
        .get("output")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    matches!(
                        item.get("type").and_then(Value::as_str),
                        Some("function_call" | "custom_tool_call" | "web_search_call")
                    )
                })
                .count() as u64
        })
        .unwrap_or(0);
    let has_compaction = response
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction"))
        });
    UsageDetails {
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens,
        reasoning_tokens,
        tool_calls,
        compaction_tokens: if has_compaction { input + output } else { 0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summary_aggregates_records_per_route() {
        let store = MemoryUsageStore::new();
        store
            .record(&UsageRecord {
                route_id: "parity".into(),
                provider: "openai_compatible".into(),
                model: "test-model".into(),
                input_tokens: 10,
                output_tokens: 2,
                cached_input_tokens: 0,
                reasoning_tokens: 0,
                tool_calls: 0,
                compaction_tokens: 0,
                review_tokens: 0,
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
        store
            .record(&UsageRecord {
                route_id: "other".into(),
                provider: "openai_compatible".into(),
                model: "test-model".into(),
                input_tokens: 99,
                output_tokens: 1,
                cached_input_tokens: 0,
                reasoning_tokens: 0,
                tool_calls: 0,
                compaction_tokens: 0,
                review_tokens: 0,
                status: 200,
                error: None,
                duration_ms: 1,
                first_byte_ms: None,
                review_run_id: None,
                review_role: None,
                review_reason: None,
                ..Default::default()
            })
            .unwrap();
        let summary = store.summary("parity").unwrap();
        assert_eq!(summary.turns, 1);
        assert_eq!(summary.latest_input_tokens, 10);
        assert_eq!(summary.total_tokens, 12);
        assert_eq!(summary.latest_first_byte_ms, Some(4));
    }

    #[test]
    fn file_usage_store_survives_reopen_and_rejects_corruption() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.jsonl");
        let record = UsageRecord {
            route_id: "route-a".into(),
            provider: "provider-a".into(),
            model: "model-a".into(),
            input_tokens: 5,
            output_tokens: 3,
            cached_input_tokens: 1,
            reasoning_tokens: 2,
            tool_calls: 1,
            compaction_tokens: 0,
            review_tokens: 0,
            status: 200,
            error: None,
            duration_ms: 7,
            first_byte_ms: Some(2),
            review_run_id: None,
            review_role: None,
            review_reason: None,
            ..Default::default()
        };
        FileUsageStore::open(&path)
            .unwrap()
            .record(&record)
            .unwrap();
        let reopened = FileUsageStore::open(&path).unwrap();
        assert_eq!(reopened.records().unwrap(), vec![record]);
        assert_eq!(reopened.summary("route-a").unwrap().total_tokens, 8);

        std::fs::write(&path, "not-json\n").unwrap();
        assert!(FileUsageStore::open(&path).is_err());
    }

    #[test]
    fn legacy_usage_ledger_without_identity_fields_still_opens() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.jsonl");
        // A pre-identity-field ledger line: exactly the fields that existed
        // before request_id / connection_id / conversation_identity.
        let legacy = serde_json::json!({
            "routeId": "route-a",
            "provider": "provider-a",
            "model": "model-a",
            "inputTokens": 5,
            "outputTokens": 3,
            "cachedInputTokens": 1,
            "reasoningTokens": 2,
            "toolCalls": 1,
            "compactionTokens": 0,
            "reviewTokens": 0,
            "status": 200,
            "error": null,
            "durationMs": 7,
            "firstByteMs": 2,
            "reviewRunId": null,
            "reviewRole": null,
            "reviewReason": null
        });
        std::fs::write(&path, format!("{legacy}\n")).unwrap();

        let store = FileUsageStore::open(&path).unwrap();
        let records = store.records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].request_id, None);
        assert_eq!(records[0].connection_id, None);
        assert_eq!(records[0].conversation_identity, None);
    }

    #[test]
    fn mark_first_downstream_frame_updates_only_the_matching_record() {
        let store = MemoryUsageStore::new();
        store
            .record(&UsageRecord {
                route_id: "compat".into(),
                provider: "openAiCompatible".into(),
                model: "qwen".into(),
                request_id: Some("req_1".into()),
                first_byte_ms: Some(10),
                first_downstream_frame_ms: None,
                ..Default::default()
            })
            .unwrap();
        store.mark_first_downstream_frame("req_1", 18).unwrap();
        store.mark_first_downstream_frame("req_1", 99).unwrap();
        let records = store.records();
        assert_eq!(records[0].first_byte_ms, Some(10));
        assert_eq!(records[0].first_downstream_frame_ms, Some(18));
    }

    #[test]
    fn usage_from_response_reads_responses_and_chat_fields() {
        assert_eq!(
            usage_from_response(&json!({"usage": {"input_tokens": 3, "output_tokens": 7}})),
            (3, 7)
        );
        assert_eq!(
            usage_from_response(&json!({"usage": {"prompt_tokens": 4, "completion_tokens": 6}})),
            (4, 6)
        );
        assert_eq!(usage_from_response(&json!({})), (0, 0));
    }

    #[test]
    fn detailed_usage_tracks_cache_reasoning_tools_and_compaction() {
        let details = usage_details_from_response(&json!({
            "usage": {
                "input_tokens": 100,
                "output_tokens": 30,
                "input_tokens_details": {"cached_tokens": 70},
                "output_tokens_details": {"reasoning_tokens": 12}
            },
            "output": [
                {"type": "function_call"},
                {"type": "custom_tool_call"},
                {"type": "compaction"}
            ]
        }));
        assert_eq!(details.input_tokens, 100);
        assert_eq!(details.output_tokens, 30);
        assert_eq!(details.cached_input_tokens, 70);
        assert_eq!(details.reasoning_tokens, 12);
        assert_eq!(details.tool_calls, 2);
        assert_eq!(details.compaction_tokens, 130);
    }

    #[test]
    fn child_usage_contains_thread_and_parent_attribution() {
        let mut graph = crate::subagent_graph::SubagentGraphRegistry::new();
        let child_id = crate::codex_metadata::CodexTurnIdentity {
            session_id: Some(
                crate::codex_metadata::CodexOpaqueId::new("session_id", "s_usage").unwrap(),
            ),
            thread_id: Some(
                crate::codex_metadata::CodexOpaqueId::new("thread_id", "t_child_u").unwrap(),
            ),
            parent_thread_id: Some(
                crate::codex_metadata::CodexOpaqueId::new("parent_thread_id", "t_parent_u")
                    .unwrap(),
            ),
            source: crate::codex_metadata::CodexIdentitySource::TurnMetadataHeader,
            trust: crate::codex_metadata::CodexIdentityTrust::Exact,
            ..Default::default()
        };

        graph
            .observe_turn(
                &child_id,
                crate::subagent_graph::TurnObservation {
                    execution_id: "exec_u".into(),
                    request_id: "req_u".into(),
                    connection_id: None,
                    route_id: "test".into(),
                    model: "gpt-4".into(),
                    observed_at_ms: 1000,
                },
            )
            .unwrap();

        let attr = build_agent_usage_attribution(Some(&child_id), Some(&graph)).unwrap();
        assert_eq!(attr.session_id.as_deref(), Some("s_usage"));
        assert_eq!(attr.thread_id.as_deref(), Some("t_child_u"));
        assert_eq!(attr.parent_thread_id.as_deref(), Some("t_parent_u"));
        assert_eq!(attr.depth, Some(1));
        assert_eq!(
            attr.link_confidence,
            Some(crate::diagnostics::LinkConfidence::High)
        );
    }
}
