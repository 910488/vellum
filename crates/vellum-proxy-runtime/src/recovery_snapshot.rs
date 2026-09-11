//! Durable, conversation-keyed recovery state.
//!
//! Recovery used to live only in a process-live admission cache keyed partly by
//! route. That cache is still useful — it prevents concurrent turns on the same
//! route from racing each other — but it cannot survive a restart, and it does
//! not follow a conversation across a provider switch or across a compaction.
//! Both of those lost recovery state silently: the conversation kept going, but
//! Vellum had forgotten that it had already intervened.
//!
//! [`ConversationRecoverySnapshotV1`] is the durable half. It is keyed by the
//! conversation's durable identity, not by route, so a provider switch carries
//! its recovery state with it. Every admission decision merges the live cache
//! with this snapshot before deciding.
//!
//! Compaction never reads, consumes, or advances a snapshot: a compaction
//! trigger is an auxiliary call, not a turn.

use serde::{Deserialize, Serialize};

use crate::investigation::InvestigationState;
use crate::task_efficiency::TaskEfficiencyState;
use crate::task_stall::TaskStallState;

/// Schema version for [`ConversationRecoverySnapshotV1`].
pub const CONVERSATION_RECOVERY_SNAPSHOT_V1: u32 = 1;

/// One recovery intervention that has already been applied, recorded so the
/// same intervention is not re-applied after a restart or provider switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryRecord {
    /// Which behavior produced it (`task_stall`, `task_efficiency`,
    /// `investigation_recovery`).
    pub kind: String,
    /// Task revision the intervention was applied at.
    pub task_revision: u64,
    /// Request index the intervention was applied at.
    pub request_index: u64,
}

/// Durable recovery state for one conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationRecoverySnapshotV1 {
    #[serde(default = "default_snapshot_schema_version")]
    pub schema_version: u32,
    /// Durable conversation identity. The primary key — never a route id, so
    /// the snapshot follows the conversation across a provider switch.
    pub conversation_key: String,
    #[serde(default)]
    pub investigation: Option<InvestigationState>,
    #[serde(default)]
    pub stall: Option<TaskStallState>,
    #[serde(default)]
    pub efficiency: Option<TaskEfficiencyState>,
    /// Highest task revision observed for this conversation.
    #[serde(default)]
    pub task_revision: u64,
    /// Last exact Codex top-level turn id folded into `task_revision`.
    /// Tool-loop requests reuse this id, so replay and local compaction cannot
    /// manufacture new user revisions from carried `role=user` history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_user_turn_id: Option<String>,
    /// Highest cumulative usage observed, so a restart does not re-count usage
    /// that already drove an intervention.
    #[serde(default)]
    pub usage_watermark: u64,
    /// Interventions already applied.
    #[serde(default)]
    pub recovery_records: Vec<RecoveryRecord>,
    /// Conversions already applied.
    #[serde(default)]
    pub conversion_records: Vec<RecoveryRecord>,
    /// Hash of the source history the snapshot was derived from, so corruption
    /// is detectable rather than silently trusted.
    #[serde(default)]
    pub source_hash: String,
    #[serde(default)]
    pub updated_at: i64,
}

fn default_snapshot_schema_version() -> u32 {
    CONVERSATION_RECOVERY_SNAPSHOT_V1
}

impl ConversationRecoverySnapshotV1 {
    /// A fresh snapshot for a conversation with no recovery history.
    pub fn new(conversation_key: impl Into<String>) -> Self {
        Self {
            schema_version: CONVERSATION_RECOVERY_SNAPSHOT_V1,
            conversation_key: conversation_key.into(),
            investigation: None,
            stall: None,
            efficiency: None,
            task_revision: 0,
            last_user_turn_id: None,
            usage_watermark: 0,
            recovery_records: Vec::new(),
            conversion_records: Vec::new(),
            source_hash: String::new(),
            updated_at: 0,
        }
    }

    /// Validate before the snapshot is allowed to influence a decision.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != CONVERSATION_RECOVERY_SNAPSHOT_V1 {
            return Err(format!(
                "unsupported recovery snapshot schema {}",
                self.schema_version
            ));
        }
        if self.conversation_key.is_empty() {
            return Err("recovery snapshot has no conversation key".into());
        }
        Ok(())
    }

    /// True when this intervention has already been applied.
    pub fn already_recovered(&self, kind: &str, task_revision: u64) -> bool {
        self.recovery_records
            .iter()
            .any(|record| record.kind == kind && record.task_revision == task_revision)
    }

    /// Record an applied intervention, keeping the watermark monotonic.
    pub fn record_recovery(&mut self, kind: &str, task_revision: u64, request_index: u64) {
        if self.already_recovered(kind, task_revision) {
            return;
        }
        self.recovery_records.push(RecoveryRecord {
            kind: kind.to_string(),
            task_revision,
            request_index,
        });
        self.task_revision = self.task_revision.max(task_revision);
    }

    /// Merge the process-live view into this durable snapshot.
    ///
    /// The live cache may be ahead of the snapshot (an intervention applied
    /// this process but not yet persisted) or behind it (a fresh process after
    /// a restart). Merging takes the union of applied interventions and the
    /// maximum of both watermarks, so neither direction loses state.
    pub fn merge_live(&mut self, live: &ConversationRecoverySnapshotV1) {
        self.task_revision = self.task_revision.max(live.task_revision);
        if live.task_revision >= self.task_revision && live.last_user_turn_id.is_some() {
            self.last_user_turn_id = live.last_user_turn_id.clone();
        }
        self.usage_watermark = self.usage_watermark.max(live.usage_watermark);
        for record in &live.recovery_records {
            if !self
                .recovery_records
                .iter()
                .any(|existing| existing == record)
            {
                self.recovery_records.push(record.clone());
            }
        }
        for record in &live.conversion_records {
            if !self
                .conversion_records
                .iter()
                .any(|existing| existing == record)
            {
                self.conversion_records.push(record.clone());
            }
        }
        if self.investigation.is_none() {
            self.investigation = live.investigation.clone();
        }
        if self.stall.is_none() {
            self.stall = live.stall.clone();
        }
        if self.efficiency.is_none() {
            self.efficiency = live.efficiency.clone();
        }
    }
}

/// What to do when a stored snapshot cannot be trusted.
///
/// A conversation must never be blocked because its recovery bookkeeping is
/// damaged, and it must never be silently given stale state. So a corrupt
/// snapshot is rebuilt where possible, and where it cannot be rebuilt recovery
/// is disabled for that conversation with an explicit diagnostic — the
/// conversation itself continues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotRecovery {
    /// The snapshot was usable as stored.
    Usable(Box<ConversationRecoverySnapshotV1>),
    /// The snapshot was rebuilt from the preserved source record and the
    /// history that followed it.
    Rebuilt(Box<ConversationRecoverySnapshotV1>),
    /// The snapshot could not be rebuilt. Recovery is off for this
    /// conversation; the diagnostic explains why.
    Disabled { reason: String },
}

/// Load a snapshot, rebuilding or disabling as needed.
///
/// `rebuild` is invoked only when the stored snapshot is missing or invalid. It
/// returns `None` when the conversation carries too little evidence to
/// reconstruct recovery state.
pub fn load_or_rebuild(
    stored: Option<ConversationRecoverySnapshotV1>,
    conversation_key: &str,
    rebuild: impl FnOnce() -> Option<ConversationRecoverySnapshotV1>,
) -> SnapshotRecovery {
    if let Some(snapshot) = stored {
        match snapshot.validate() {
            Ok(()) if snapshot.conversation_key == conversation_key => {
                return SnapshotRecovery::Usable(Box::new(snapshot));
            }
            Ok(()) => {
                return SnapshotRecovery::Disabled {
                    reason: format!(
                        "recovery snapshot belongs to conversation {} but was loaded for {}",
                        snapshot.conversation_key, conversation_key
                    ),
                };
            }
            Err(error) => {
                if let Some(rebuilt) = rebuild() {
                    return SnapshotRecovery::Rebuilt(Box::new(rebuilt));
                }
                return SnapshotRecovery::Disabled {
                    reason: format!("recovery snapshot invalid and not rebuildable: {error}"),
                };
            }
        }
    }
    match rebuild() {
        Some(rebuilt) => SnapshotRecovery::Rebuilt(Box::new(rebuilt)),
        None => SnapshotRecovery::Usable(Box::new(ConversationRecoverySnapshotV1::new(
            conversation_key,
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(key: &str) -> ConversationRecoverySnapshotV1 {
        ConversationRecoverySnapshotV1::new(key)
    }

    #[test]
    fn merge_keeps_latest_exact_user_turn_identity() {
        let mut durable = snapshot("conv-1");
        durable.task_revision = 1;
        durable.last_user_turn_id = Some("turn-1".into());
        let mut live = snapshot("conv-1");
        live.task_revision = 2;
        live.last_user_turn_id = Some("turn-2".into());
        durable.merge_live(&live);
        assert_eq!(durable.task_revision, 2);
        assert_eq!(durable.last_user_turn_id.as_deref(), Some("turn-2"));
    }

    #[test]
    fn a_fresh_snapshot_validates_and_has_no_history() {
        let s = snapshot("conv-1");
        assert_eq!(s.validate(), Ok(()));
        assert!(!s.already_recovered("task_stall", 1));
    }

    #[test]
    fn an_empty_conversation_key_fails_closed() {
        let mut s = snapshot("conv-1");
        s.conversation_key = String::new();
        assert!(s.validate().is_err());
    }

    #[test]
    fn an_unknown_schema_version_fails_closed() {
        let mut s = snapshot("conv-1");
        s.schema_version = 99;
        assert!(s.validate().is_err());
    }

    #[test]
    fn recording_an_intervention_is_idempotent() {
        let mut s = snapshot("conv-1");
        s.record_recovery("task_stall", 3, 10);
        s.record_recovery("task_stall", 3, 11);
        assert_eq!(s.recovery_records.len(), 1, "same revision applies once");
        assert!(s.already_recovered("task_stall", 3));
        // A later revision is a genuinely new intervention.
        s.record_recovery("task_stall", 4, 12);
        assert_eq!(s.recovery_records.len(), 2);
        assert_eq!(s.task_revision, 4);
    }

    #[test]
    fn different_behaviors_are_tracked_separately() {
        let mut s = snapshot("conv-1");
        s.record_recovery("task_stall", 2, 5);
        assert!(!s.already_recovered("task_efficiency", 2));
    }

    #[test]
    fn merging_a_live_view_loses_nothing_in_either_direction() {
        let mut durable = snapshot("conv-1");
        durable.record_recovery("task_stall", 1, 1);
        durable.usage_watermark = 500;

        let mut live = snapshot("conv-1");
        live.record_recovery("task_efficiency", 2, 7);
        live.usage_watermark = 900;

        durable.merge_live(&live);
        assert!(durable.already_recovered("task_stall", 1), "durable kept");
        assert!(
            durable.already_recovered("task_efficiency", 2),
            "live added"
        );
        assert_eq!(durable.usage_watermark, 900, "watermark is the max");
        assert_eq!(durable.task_revision, 2);
    }

    #[test]
    fn merging_the_same_record_twice_does_not_duplicate_it() {
        let mut durable = snapshot("conv-1");
        durable.record_recovery("task_stall", 1, 1);
        let live = durable.clone();
        durable.merge_live(&live);
        assert_eq!(durable.recovery_records.len(), 1);
    }

    #[test]
    fn a_missing_snapshot_yields_a_fresh_one_when_nothing_can_be_rebuilt() {
        let outcome = load_or_rebuild(None, "conv-1", || None);
        match outcome {
            SnapshotRecovery::Usable(s) => assert_eq!(s.conversation_key, "conv-1"),
            other => panic!("expected a fresh usable snapshot, got {other:?}"),
        }
    }

    #[test]
    fn a_corrupt_snapshot_is_rebuilt_when_possible() {
        let mut corrupt = snapshot("conv-1");
        corrupt.schema_version = 99;
        let outcome = load_or_rebuild(Some(corrupt), "conv-1", || {
            let mut rebuilt = snapshot("conv-1");
            rebuilt.record_recovery("task_stall", 1, 1);
            Some(rebuilt)
        });
        match outcome {
            SnapshotRecovery::Rebuilt(s) => assert!(s.already_recovered("task_stall", 1)),
            other => panic!("expected a rebuild, got {other:?}"),
        }
    }

    #[test]
    fn an_unrebuildable_corrupt_snapshot_disables_recovery_rather_than_inventing_state() {
        let mut corrupt = snapshot("conv-1");
        corrupt.schema_version = 99;
        let outcome = load_or_rebuild(Some(corrupt), "conv-1", || None);
        match outcome {
            SnapshotRecovery::Disabled { reason } => {
                assert!(reason.contains("not rebuildable"), "{reason}");
            }
            other => panic!("expected recovery to be disabled, got {other:?}"),
        }
    }

    #[test]
    fn a_snapshot_from_another_conversation_is_never_applied() {
        let foreign = snapshot("conv-other");
        let outcome = load_or_rebuild(Some(foreign), "conv-1", || None);
        assert!(matches!(outcome, SnapshotRecovery::Disabled { .. }));
    }

    #[test]
    fn a_snapshot_round_trips_through_json() {
        let mut s = snapshot("conv-1");
        s.record_recovery("investigation_recovery", 4, 20);
        s.usage_watermark = 1234;
        let encoded = serde_json::to_string(&s).expect("encode");
        let decoded: ConversationRecoverySnapshotV1 =
            serde_json::from_str(&encoded).expect("decode");
        assert_eq!(decoded, s);
        assert_eq!(decoded.validate(), Ok(()));
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;
    use crate::history::{HistoryStore, MemoryHistoryStore};

    #[test]
    fn a_snapshot_round_trips_through_the_history_store() {
        let store = MemoryHistoryStore::new();
        assert_eq!(store.recovery_snapshot("conv-1").unwrap(), None);

        let mut snapshot = ConversationRecoverySnapshotV1::new("conv-1");
        snapshot.record_recovery("task_efficiency", 2, 7);
        snapshot.usage_watermark = 60_000;
        store.put_recovery_snapshot(&snapshot).unwrap();

        let loaded = store.recovery_snapshot("conv-1").unwrap().expect("stored");
        assert!(loaded.already_recovered("task_efficiency", 2));
        assert_eq!(loaded.usage_watermark, 60_000);
    }

    #[test]
    fn a_snapshot_is_keyed_by_conversation_not_route() {
        // The whole point of the durable half: the same conversation keeps its
        // recovery state after a provider switch, and a different conversation
        // never inherits it.
        let store = MemoryHistoryStore::new();
        let mut snapshot = ConversationRecoverySnapshotV1::new("conv-1");
        snapshot.record_recovery("task_stall", 1, 1);
        store.put_recovery_snapshot(&snapshot).unwrap();

        assert!(store
            .recovery_snapshot("conv-1")
            .unwrap()
            .expect("same conversation")
            .already_recovered("task_stall", 1));
        assert_eq!(store.recovery_snapshot("conv-2").unwrap(), None);
    }

    #[test]
    fn an_invalid_snapshot_is_refused_by_the_store() {
        let store = MemoryHistoryStore::new();
        let mut snapshot = ConversationRecoverySnapshotV1::new("conv-1");
        snapshot.schema_version = 99;
        assert!(store.put_recovery_snapshot(&snapshot).is_err());
        assert_eq!(store.recovery_snapshot("conv-1").unwrap(), None);
    }
}
