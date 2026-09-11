//! Trace artifacts for live smoke runs.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSmokeTrace {
    pub run_id: String,
    pub short_id: String,
    pub jetson: String,
    pub arch: String,
    pub codex_version: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub epoch_before: Option<u64>,
    pub epoch_after: Option<u64>,
    pub disconnect_seq: Option<u64>,
    pub final_seq: Option<u64>,
    pub events_generated_while_detached: Option<u64>,
    pub gap: bool,
    pub duplicate: bool,
    pub turn_completed: bool,
    pub notes: Vec<String>,
    pub extra: Value,
    pub local_artifact_dir: Option<PathBuf>,
}

impl LiveSmokeTrace {
    pub fn new(
        run_id: impl Into<String>,
        short_id: impl Into<String>,
        jetson: impl Into<String>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            short_id: short_id.into(),
            jetson: jetson.into(),
            arch: "aarch64".into(),
            codex_version: None,
            started_at: Utc::now(),
            finished_at: None,
            thread_id: None,
            turn_id: None,
            epoch_before: None,
            epoch_after: None,
            disconnect_seq: None,
            final_seq: None,
            events_generated_while_detached: None,
            gap: false,
            duplicate: false,
            turn_completed: false,
            notes: Vec::new(),
            extra: Value::Object(Default::default()),
            local_artifact_dir: None,
        }
    }

    pub fn note(&mut self, message: impl Into<String>) {
        self.notes.push(message.into());
    }

    pub async fn write_summary(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let body = serde_json::to_vec_pretty(self)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        tokio::fs::write(path, body).await
    }
}
