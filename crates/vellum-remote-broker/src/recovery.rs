//! Recovery helpers for indeterminate command states.

use serde::{Deserialize, Serialize};

use crate::event_store::EventStore;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RecoveredCommandState {
    Completed,
    SafeToRetry,
    Indeterminate,
    Orphaned,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredCommandMeta {
    pub idempotency_key: String,
    pub command_kind: String,
    pub state: String,
}

pub fn classify_after_upstream_restart(
    command_kind: &str,
    previously_completed: bool,
) -> RecoveredCommandState {
    if previously_completed {
        return RecoveredCommandState::Completed;
    }
    match command_kind {
        "thread.start" | "turn.start" | "turn.interrupt" | "approval.respond" => {
            RecoveredCommandState::Indeterminate
        }
        _ => RecoveredCommandState::SafeToRetry,
    }
}

/// On broker startup, any command left in `processing` for side-effectful kinds
/// becomes indeterminate and must not be auto-resent.
pub fn reconcile_processing_commands(
    store: &EventStore,
) -> Result<u64, crate::event_store::StoreError> {
    store.mark_processing_indeterminate()
}
