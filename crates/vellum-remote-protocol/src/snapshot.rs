//! Snapshot and lease projection types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::approval::PendingApproval;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ThreadRuntimeStatus {
    Unknown,
    StoredNotLoaded,
    Loading,
    Idle,
    Running,
    WaitingForApproval,
    Interrupted,
    Completed,
    Failed,
    Orphaned,
    Indeterminate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActiveTurnView {
    pub turn_id: String,
    pub state: ThreadRuntimeStatus,
    pub started_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WriterLeaseView {
    pub lease_id: String,
    pub holder_device_id: String,
    pub expires_at: DateTime<Utc>,
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSnapshot {
    pub thread_id: String,
    pub snapshot_seq: u64,
    pub status: ThreadRuntimeStatus,
    pub loaded: bool,
    #[serde(default)]
    pub active_turn: Option<ActiveTurnView>,
    #[serde(default)]
    pub pending_approvals: Vec<PendingApproval>,
    #[serde(default)]
    pub items: Vec<Value>,
    #[serde(default)]
    pub usage: Option<Value>,
    #[serde(default)]
    pub writer_lease: Option<WriterLeaseView>,
}
