//! The bridge's qualification journal and the last stored gate result.
//!
//! The Enhanced fork reports what its ports did over two notifications. Those
//! notifications are diagnostics, not a control plane: the bridge records them
//! and stops them there rather than forwarding anything Codex Desktop did not
//! ask for. Everything written here is identity, counters, and hashes — never
//! prompts, tool output, or authorization.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use vellum_enhanced_codex::{field_name_is_forbidden, EnhancedEventKind};

use super::atomic::{append_line, write_atomic};
use super::attestation::EnhancedRuntimeIdentity;

pub use vellum_enhanced_codex::{ENHANCED_EVENT_NOTIFICATION, ENHANCED_IDENTITY_NOTIFICATION};
pub const LAST_QUALIFICATION_FILE: &str = "enhanced-runtime/last-qualification.json";

/// Only the `enhanced.*` fields that already exist in the portable telemetry
/// contract. An unknown field is a protocol change, not something to pass on.
const ALLOWED_EVENT_FIELDS: &[&str] = &[
    "threadIdHash",
    "runtimeDigest",
    "featureProfile",
    "providerId",
    "modelId",
    "requestIndex",
    "callIdHash",
    "beforeTokenEstimate",
    "afterTokenEstimate",
    "charsRemoved",
    "retryIndex",
    "continuationIndex",
    "unfinishedSignalCount",
    "outcome",
    "succeeded",
];

pub fn known_event_names() -> Vec<&'static str> {
    vec![
        EnhancedEventKind::SessionFeaturesApplied.name(),
        EnhancedEventKind::ToolCallAdmitted.name(),
        EnhancedEventKind::ToolCallCompleted.name(),
        EnhancedEventKind::ToolDuplicateDetected.name(),
        EnhancedEventKind::ToolDuplicateSuppressed.name(),
        EnhancedEventKind::ToolCallIdCollision.name(),
        EnhancedEventKind::ContextPruneStarted.name(),
        EnhancedEventKind::ContextPruneCompleted.name(),
        EnhancedEventKind::ContextCompactionAvoided.name(),
        EnhancedEventKind::ContextOverflowRetry.name(),
        EnhancedEventKind::ContextOverflowRetryRefused.name(),
        EnhancedEventKind::ContextPressureChecked.name(),
        EnhancedEventKind::ContinuationEvaluated.name(),
        EnhancedEventKind::ContinuationAllowed.name(),
        EnhancedEventKind::ContinuationExhausted.name(),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnhancedEventRecord {
    pub name: String,
    pub fields: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum JournalEntry {
    #[serde(rename_all = "camelCase")]
    Identity {
        at: i64,
        launch_id: String,
        identity: EnhancedRuntimeIdentity,
    },
    #[serde(rename_all = "camelCase")]
    Event {
        at: i64,
        launch_id: String,
        event: EnhancedEventRecord,
    },
    #[serde(rename_all = "camelCase")]
    Rejected {
        at: i64,
        launch_id: String,
        method: String,
        reason: String,
    },
}

pub struct QualificationJournal {
    path: PathBuf,
    launch_id: String,
    counts: BTreeMap<String, u64>,
}

impl QualificationJournal {
    pub fn new(path: PathBuf, launch_id: String) -> Self {
        Self {
            path,
            launch_id,
            counts: BTreeMap::new(),
        }
    }

    pub fn counts(&self) -> &BTreeMap<String, u64> {
        &self.counts
    }

    pub fn record_identity(&mut self, identity: &EnhancedRuntimeIdentity) {
        self.append(JournalEntry::Identity {
            at: chrono::Utc::now().timestamp(),
            launch_id: self.launch_id.clone(),
            identity: identity.clone(),
        });
    }

    pub fn record_event(&mut self, event: EnhancedEventRecord) {
        *self.counts.entry(event.name.clone()).or_default() += 1;
        self.append(JournalEntry::Event {
            at: chrono::Utc::now().timestamp(),
            launch_id: self.launch_id.clone(),
            event,
        });
    }

    pub fn record_rejected(&mut self, method: &str, reason: impl Into<String>) {
        self.append(JournalEntry::Rejected {
            at: chrono::Utc::now().timestamp(),
            launch_id: self.launch_id.clone(),
            method: method.to_string(),
            reason: reason.into(),
        });
    }

    fn append(&self, entry: JournalEntry) {
        if let Ok(line) = serde_json::to_string(&entry) {
            // A journal write must never take the bridge down; the gates read
            // the attestation for liveness and the journal only for evidence.
            let _ = append_line(&self.path, &line);
        }
    }

    pub fn read(path: &Path) -> Vec<JournalEntry> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

/// Parses `vellum/enhancedRuntimeIdentity` params into a runtime identity.
pub fn parse_enhanced_identity(params: &Value) -> Result<EnhancedRuntimeIdentity, String> {
    let commit = string_field(params, "enhancedCommit")?;
    let digest = string_field(params, "runtimeDigest")?;
    let profile = string_field(params, "featureProfile")?;
    let ports = params
        .get("ports")
        .ok_or_else(|| "missing ports".to_string())?;
    Ok(EnhancedRuntimeIdentity {
        enhanced_commit: commit,
        runtime_digest: digest,
        feature_profile: profile,
        qwen_tool_reliability: bool_field(ports, "qwenToolReliability")?,
        deepseek_context_recovery: bool_field(ports, "deepseekContextRecovery")?,
        qwen_bounded_continuation: bool_field(ports, "qwenBoundedContinuation")?,
    })
}

/// Parses `vellum/enhancedEvent` params, keeping only allowlisted fields.
pub fn parse_enhanced_event(params: &Value) -> Result<EnhancedEventRecord, String> {
    let name = string_field(params, "name")?;
    if !known_event_names().contains(&name.as_str()) {
        return Err(format!("unknown enhanced event name {name}"));
    }
    let mut fields = BTreeMap::new();
    if let Some(map) = params.get("fields").and_then(Value::as_object) {
        for (key, value) in map {
            if field_name_is_forbidden(key) {
                return Err(format!("event field {key} is not allowed"));
            }
            if !ALLOWED_EVENT_FIELDS.contains(&key.as_str()) {
                return Err(format!("event field {key} is not in the enhanced contract"));
            }
            if value.is_object() || value.is_array() {
                return Err(format!("event field {key} must be a scalar"));
            }
            fields.insert(key.clone(), value.clone());
        }
    }
    Ok(EnhancedEventRecord { name, fields })
}

fn string_field(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("missing {key}"))
}

fn bool_field(value: &Value, key: &str) -> Result<bool, String> {
    value
        .get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("missing {key}"))
}

/// The last qualification run, surfaced in Settings so the state a release
/// engineer sees is the state that was actually proven.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualificationResult {
    pub run_id: String,
    pub mode: String,
    pub started_at: i64,
    pub finished_at: i64,
    pub passed: bool,
    pub promotion_ready: bool,
    pub report_path: PathBuf,
    pub failures: Vec<String>,
}

impl QualificationResult {
    pub fn path_in(data_root: &Path) -> PathBuf {
        data_root.join(LAST_QUALIFICATION_FILE)
    }

    pub fn read(data_root: &Path) -> Option<Self> {
        let bytes = std::fs::read(Self::path_in(data_root)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn store(&self, data_root: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        write_atomic(&Self::path_in(data_root), &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identity_notification_requires_every_port_flag() {
        let complete = json!({
            "enhancedCommit": "c".repeat(40),
            "runtimeDigest": "sha256:d",
            "featureProfile": "E5",
            "ports": {
                "qwenToolReliability": true,
                "deepseekContextRecovery": true,
                "qwenBoundedContinuation": false
            }
        });
        let identity = parse_enhanced_identity(&complete).unwrap();
        assert_eq!(identity.feature_profile, "E5");
        assert!(!identity.qwen_bounded_continuation);

        let mut partial = complete.clone();
        partial["ports"]
            .as_object_mut()
            .unwrap()
            .remove("deepseekContextRecovery");
        assert!(parse_enhanced_identity(&partial).is_err());
    }

    #[test]
    fn event_fields_are_allowlisted_and_secrets_are_refused() {
        let allowed = json!({
            "name": "enhanced.tool.duplicate_suppressed",
            "fields": {"callIdHash": "sha256:x", "requestIndex": 2}
        });
        let event = parse_enhanced_event(&allowed).unwrap();
        assert_eq!(event.fields.len(), 2);

        let secret = json!({
            "name": "enhanced.tool.duplicate_suppressed",
            "fields": {"authorization": "Bearer x"}
        });
        assert!(parse_enhanced_event(&secret).is_err());

        let unknown_field = json!({
            "name": "enhanced.tool.duplicate_suppressed",
            "fields": {"toolArguments": "rm -rf /"}
        });
        assert!(parse_enhanced_event(&unknown_field).is_err());

        let unknown_name = json!({"name": "enhanced.something.new", "fields": {}});
        assert!(parse_enhanced_event(&unknown_name).is_err());
    }

    /// The fork builds these notifications with the portable helpers; the
    /// bridge parses them here. If the two ever disagree, Enhanced silently
    /// stops reporting which runtime ran the turn.
    #[test]
    fn the_portable_builders_produce_exactly_what_the_bridge_accepts() {
        let identity = vellum_enhanced_codex::identity_notification(
            "c".repeat(40),
            "sha256:digest",
            "E5",
            vellum_enhanced_codex::EnhancedRuntimeFeatures::all_on(),
        );
        assert_eq!(identity["method"], ENHANCED_IDENTITY_NOTIFICATION);
        let parsed = parse_enhanced_identity(&identity["params"]).unwrap();
        assert_eq!(parsed.feature_profile, "E5");
        assert!(parsed.qwen_bounded_continuation);

        for kind in [
            EnhancedEventKind::ToolDuplicateSuppressed,
            EnhancedEventKind::ContextPruneCompleted,
            EnhancedEventKind::ContextOverflowRetryRefused,
            EnhancedEventKind::ContinuationExhausted,
        ] {
            let event = vellum_enhanced_codex::EnhancedEvent::new(
                kind,
                vellum_enhanced_codex::EnhancedEventFields {
                    retry_index: Some(1),
                    call_id_hash: Some("sha256:x".into()),
                    ..Default::default()
                },
            );
            let notification = vellum_enhanced_codex::event_notification(&event);
            assert_eq!(notification["method"], ENHANCED_EVENT_NOTIFICATION);
            let record = parse_enhanced_event(&notification["params"]).unwrap();
            assert_eq!(record.name, kind.name());
            assert_eq!(record.fields.len(), 2);
        }
    }

    #[test]
    fn journal_counts_events_and_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("journal.jsonl");
        let mut journal = QualificationJournal::new(path.clone(), "launch-1".into());
        journal.record_event(EnhancedEventRecord {
            name: "enhanced.tool.duplicate_suppressed".into(),
            fields: BTreeMap::new(),
        });
        journal.record_event(EnhancedEventRecord {
            name: "enhanced.tool.duplicate_suppressed".into(),
            fields: BTreeMap::new(),
        });
        journal.record_rejected("thread/unknownNotification", "not an enhanced contract");
        assert_eq!(journal.counts()["enhanced.tool.duplicate_suppressed"], 2);
        assert_eq!(QualificationJournal::read(&path).len(), 3);
    }

    #[test]
    fn last_qualification_result_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let result = QualificationResult {
            run_id: "run-1".into(),
            mode: "installed".into(),
            started_at: 1,
            finished_at: 2,
            passed: false,
            promotion_ready: false,
            report_path: temp.path().join("report.json"),
            failures: vec!["EnhancedDesktopBridgeNotObserved".into()],
        };
        result.store(temp.path()).unwrap();
        assert_eq!(QualificationResult::read(temp.path()).unwrap(), result);
    }
}
