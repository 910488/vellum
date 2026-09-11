//! Credential-free, launch-scoped observations. Never a task authority.
use super::{atomic::write_atomic, attestation::BridgeAttestationV1, ThreadRuntimeBinding};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub const OBSERVATION_FILE: &str = "enhanced-runtime/observations.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionObservation {
    pub thread_id: String,
    pub parent_thread_id: Option<String>,
    pub plane: String,
    pub model: Option<String>,
    pub state: String,
    pub last_activity: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteObservation {
    pub state: String,
    pub owner_pid: Option<u32>,
    pub clients: BTreeMap<String, String>,
    pub handshake_observed: bool,
    pub list_observed: bool,
    pub history_observed: bool,
    pub stream_observed: bool,
    pub control_observed: bool,
    pub last_failure_stage: Option<String>,
    pub last_error_code: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeObservations {
    pub schema_version: u32,
    pub launch_id: String,
    pub bridge_pid: u32,
    pub updated_at: i64,
    pub freshness: String,
    pub sessions: BTreeMap<String, SessionObservation>,
    pub remote: RemoteObservation,
}

impl RuntimeObservations {
    pub fn new(launch_id: String) -> Self {
        Self {
            schema_version: 1,
            launch_id,
            bridge_pid: std::process::id(),
            freshness: "current".into(),
            remote: RemoteObservation {
                state: "unavailable".into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    pub fn write(&mut self, path: &Path) -> std::io::Result<()> {
        self.updated_at = chrono::Utc::now().timestamp();
        write_atomic(path, &serde_json::to_vec(self)?)
    }

    /// Seed durable bindings as `attached`, not as running sessions. A bridge
    /// restart must not make real task ownership disappear from the Enhanced
    /// Core tab, but a binding alone is not evidence of a currently loaded
    /// thread or active turn.
    pub fn seed_bindings(&mut self, bindings: &[ThreadRuntimeBinding]) {
        for binding in bindings {
            self.sessions
                .entry(binding.thread_id.clone())
                .or_insert_with(|| SessionObservation {
                    thread_id: binding.thread_id.clone(),
                    plane: binding.plane.as_str().into(),
                    model: (!binding.model_id.is_empty()).then(|| binding.model_id.clone()),
                    state: "attached".into(),
                    last_activity: binding.created_at,
                    ..Default::default()
                });
        }
    }

    pub fn observe(&mut self, plane: &str, value: &Value) {
        let params = value.get("params").unwrap_or(&Value::Null);
        let method = value["method"].as_str().unwrap_or_default();
        let thread = params["threadId"]
            .as_str()
            .or_else(|| params.pointer("/thread/id").and_then(Value::as_str))
            .or_else(|| value.pointer("/result/thread/id").and_then(Value::as_str));
        let Some(thread) = thread else { return };
        let row = self
            .sessions
            .entry(thread.to_owned())
            .or_insert_with(|| SessionObservation {
                thread_id: thread.to_owned(),
                plane: plane.into(),
                state: "observed".into(),
                ..Default::default()
            });
        row.last_activity = chrono::Utc::now().timestamp();
        if let Some(model) = value
            .pointer("/result/thread/model")
            .and_then(Value::as_str)
        {
            row.model = Some(model.into());
        }
        match method {
            "turn/started" => row.state = "running".into(),
            "turn/completed" => {
                row.state =
                    if params.pointer("/turn/status").and_then(Value::as_str) == Some("failed") {
                        "failed".into()
                    } else {
                        "idle".into()
                    };
            }
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                row.state = "approval".into()
            }
            "thread/closed" | "thread/unloaded" => row.state = "unloaded".into(),
            _ => {}
        }
    }
}

pub fn read(data_root: &Path) -> RuntimeObservations {
    let mut report = std::fs::read(data_root.join(OBSERVATION_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<RuntimeObservations>(&bytes).ok())
        .filter(|report| report.schema_version == 1)
        .unwrap_or_else(|| RuntimeObservations {
            freshness: "unavailable".into(),
            ..Default::default()
        });
    if report.freshness == "unavailable" {
        return report;
    }
    let attestation =
        BridgeAttestationV1::read(&data_root.join(super::launch_manifest::ATTESTATION_FILE)).ok();
    report.freshness = match attestation {
        Some(ref live)
            if live.is_live()
                && live.bridge_pid == report.bridge_pid
                && live.launch_id == report.launch_id =>
        {
            if chrono::Utc::now().timestamp() - report.updated_at > 15 {
                "stale"
            } else {
                "current"
            }
        }
        _ => "offline",
    }
    .into();
    report
}

pub fn export(data_root: &Path) -> std::io::Result<PathBuf> {
    let report = read(data_root);
    let sessions: Vec<_> = report
        .sessions
        .values()
        .map(|row| {
            serde_json::json!({
                "threadHash": hash(&row.thread_id),
                "parentHash": row.parent_thread_id.as_deref().map(hash),
                "plane": row.plane, "state": row.state, "lastActivity": row.last_activity
            })
        })
        .collect();
    let payload = serde_json::json!({
        "schemaVersion": 1, "contractVersion": super::contracts::CONTRACT_VERSION,
        "freshness": report.freshness, "updatedAt": report.updated_at,
        "sessions": sessions,
        "remote": {
            "state": report.remote.state, "clientCount": report.remote.clients.len(),
            "handshakeObserved": report.remote.handshake_observed,
            "listObserved": report.remote.list_observed,
            "historyObserved": report.remote.history_observed,
            "streamObserved": report.remote.stream_observed,
            "controlObserved": report.remote.control_observed,
            "lastFailureStage": report.remote.last_failure_stage,
            "lastErrorCode": report.remote.last_error_code
        }
    });
    let path = data_root
        .join("enhanced-runtime/diagnostics")
        .join(format!("{}.json", ulid::Ulid::new()));
    write_atomic(&path, &serde_json::to_vec_pretty(&payload)?)?;
    Ok(path)
}

fn hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn historical_files_are_not_live_connections() {
        let root = tempfile::tempdir().unwrap();
        let mut report = RuntimeObservations::new("old".into());
        report.write(&root.path().join(OBSERVATION_FILE)).unwrap();
        assert_eq!(read(root.path()).freshness, "offline");
    }
    #[test]
    fn export_does_not_include_thread_ids_or_message_content() {
        let root = tempfile::tempdir().unwrap();
        let mut report = RuntimeObservations::new("launch".into());
        report.observe(
            "official-codex",
            &serde_json::json!({
                "method":"turn/started", "params":{"threadId":"private-thread","input":"SECRET"}
            }),
        );
        report.write(&root.path().join(OBSERVATION_FILE)).unwrap();
        let bytes = std::fs::read_to_string(export(root.path()).unwrap()).unwrap();
        assert!(!bytes.contains("private-thread"));
        assert!(!bytes.contains("SECRET"));
        assert!(bytes.contains("running"));
    }

    #[test]
    fn durable_bindings_survive_a_bridge_restart_as_attached_not_running() {
        let mut report = RuntimeObservations::new("launch".into());
        report.seed_bindings(&[ThreadRuntimeBinding::new(
            "thread-1",
            super::super::ExecutionPlane::EnhancedCodex,
            "digest",
            "qwen",
            "qwen-model",
            "thread-1",
            42,
        )]);
        let row = &report.sessions["thread-1"];
        assert_eq!(row.state, "attached");
        assert_eq!(row.plane, "enhanced-codex");
        assert_eq!(row.model.as_deref(), Some("qwen-model"));
        assert_eq!(row.last_activity, 42);
    }
}
