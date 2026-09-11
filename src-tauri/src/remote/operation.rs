//! Desktop-side background operation status, persisted for reconnect/reconcile.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteOperationProgress {
    pub operation_id: String,
    pub host_id: String,
    pub kind: String,
    pub phase: String,
    pub percent: u8,
    pub state: String,
    pub message: Option<String>,
    pub result: Option<Value>,
    pub updated_at: DateTime<Utc>,
}

impl RemoteOperationProgress {
    pub fn queued(operation_id: String, host_id: String, kind: &str) -> Self {
        Self {
            operation_id,
            host_id,
            kind: kind.into(),
            phase: "queued".into(),
            percent: 0,
            state: "running".into(),
            message: None,
            result: None,
            updated_at: Utc::now(),
        }
    }
}

pub fn save(root: &Path, progress: &RemoteOperationProgress) -> AppResult<()> {
    let dir = operation_dir(root);
    fs::create_dir_all(&dir).map_err(|error| AppError::Message(error.to_string()))?;
    let path = dir.join(format!("{}.json", progress.operation_id));
    let tmp = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
    let bytes = serde_json::to_vec_pretty(progress)
        .map_err(|error| AppError::Message(error.to_string()))?;
    fs::write(&tmp, bytes).map_err(|error| AppError::Message(error.to_string()))?;
    fs::rename(tmp, path).map_err(|error| AppError::Message(error.to_string()))
}

pub fn load(root: &Path, operation_id: &str) -> AppResult<RemoteOperationProgress> {
    if operation_id.is_empty()
        || !operation_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err(AppError::Message("invalid operation id".into()));
    }
    let bytes = fs::read(operation_dir(root).join(format!("{operation_id}.json")))
        .map_err(|error| AppError::Message(format!("operation not found: {error}")))?;
    serde_json::from_slice(&bytes).map_err(|error| AppError::Message(error.to_string()))
}

fn operation_dir(root: &Path) -> PathBuf {
    root.join("remote-operations")
}
