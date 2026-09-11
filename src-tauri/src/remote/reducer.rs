//! `RemoteThreadState` — kept only for `model.rs`'s forward-looking re-export
//! (`pub use crate::remote::reducer::RemoteThreadState as RemoteThreadSummary;`).
//!
//! The reducer logic that consumed this type (`RemoteThreadReducer`,
//! snapshot/event application against a live broker WebSocket connection)
//! had zero callers from the shipped Desktop frontend and was removed as
//! dead code in the Stage G security-hardening pass — see
//! `docs/adr/ADR-001-codex-host-native.md` and
//! `src-tauri/src/remote/connection_manager.rs`.

use serde::{Deserialize, Serialize};
use vellum_remote_protocol::{PendingApproval, ThreadEvent, ThreadRuntimeStatus, WriterLeaseView};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SyncState {
    Idle,
    Snapshot,
    Replay,
    Live,
    Gap,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteThreadState {
    pub host_id: String,
    pub thread_id: String,
    pub sync_state: SyncState,
    pub last_applied_seq: u64,
    pub snapshot_seq: u64,
    pub status: ThreadRuntimeStatus,
    pub pending_approvals: Vec<PendingApproval>,
    pub writer_lease: Option<WriterLeaseView>,
    pub events: Vec<ThreadEvent>,
}
