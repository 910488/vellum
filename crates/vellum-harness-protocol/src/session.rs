use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::HarnessId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessSelection {
    pub harness_id: HarnessId,
    pub model_id: Option<String>,
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessSessionHandle {
    pub runtime_instance_id: String,
    pub native_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessSessionBinding {
    pub ui_thread_id: String,
    pub harness_id: HarnessId,
    pub runtime_instance_id: String,
    pub native_session_id: String,
    pub workspace: PathBuf,
    pub capability_snapshot_hash: String,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

/// Journal entries are mirrors for reconnect, audit, evaluation and debugging.
/// They are never valid input for rebuilding a native harness session.
pub const EVENT_JOURNAL_RUNTIME_REPLAY_SOURCE: bool = false;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionBinding {
    pub ui_request_id: String,
    pub native_request_id: String,
    pub thread_id: String,
    pub harness_id: HarnessId,
}
