//! Macro task-progress backstop. Does not require investigation identity.

use serde::{Deserialize, Serialize};

use crate::evidence_operation::{EvidenceOperation, EvidenceOperationKind};
use crate::progress_fingerprint::{
    fingerprints_from_operation, ProgressFingerprint, ProgressKind, SeenProgressTracker,
};

pub const TASK_STALL_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StallMode {
    Off,
    Shadow,
    Recover,
}

impl StallMode {
    pub fn from_env() -> Self {
        let raw = std::env::var("VELLUM_TASK_STALL_MODE").unwrap_or_default();
        match raw.to_ascii_lowercase().as_str() {
            "recover" => Self::Recover,
            "off" => Self::Off,
            _ => Self::Shadow,
        }
    }

    pub fn injects_recovery(self) -> bool {
        matches!(self, Self::Recover)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StallRecoveryState {
    #[default]
    None,
    Warned,
    Escalated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStallPolicy {
    pub mode: StallMode,
    pub recovery_after_tool_results: u32,
    pub post_recovery_patience: u32,
    pub progress_history_capacity: usize,
}

impl Default for TaskStallPolicy {
    fn default() -> Self {
        Self {
            mode: StallMode::from_env(),
            // Calibrated against the captured 76-operation failure trace and
            // benign exploration/retry controls. This is deliberately a
            // policy value, not a semantic claim that five operations always
            // mean a stalled task.
            recovery_after_tool_results: 5,
            post_recovery_patience: 8,
            progress_history_capacity: 32,
        }
    }
}

impl TaskStallPolicy {
    pub fn shadow() -> Self {
        Self {
            mode: StallMode::Shadow,
            ..Self::default_without_env()
        }
    }

    pub fn recover() -> Self {
        Self {
            mode: StallMode::Recover,
            ..Self::default_without_env()
        }
    }

    fn default_without_env() -> Self {
        Self {
            mode: StallMode::Shadow,
            recovery_after_tool_results: 5,
            post_recovery_patience: 8,
            progress_history_capacity: 32,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStallState {
    #[serde(default = "default_stall_schema")]
    pub schema_version: u32,
    #[serde(default)]
    pub task_revision: u64,
    #[serde(default)]
    pub tool_results_since_progress: u32,
    #[serde(default)]
    pub total_tool_results: u64,
    #[serde(default)]
    pub last_progress_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_progress: Option<ProgressFingerprint>,
    #[serde(default)]
    pub recent_progress: SeenProgressTracker,
    /// Durable count of stall recoveries injected across the whole task.
    #[serde(default, alias = "recoveryCount")]
    pub recovery_total: u32,
    /// Recovery level of the current stall epoch: 0 / 1 / 2.
    #[serde(default)]
    pub epoch_recovery_level: u8,
    #[serde(default)]
    pub recovery_state: StallRecoveryState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_recovery_seq: Option<u64>,
    #[serde(default)]
    pub post_recovery_tool_results_without_progress: u32,
    #[serde(default)]
    pub last_activity_seq: u64,
    #[serde(default)]
    pub unattributed_operations: u64,
    /// Source index of the latest stall-recovery marker in raw history.
    /// Not durable; used so pre-marker operations do not count as post-recovery.
    #[serde(skip)]
    pub last_recovery_source_index: Option<usize>,
}

fn default_stall_schema() -> u32 {
    TASK_STALL_SCHEMA_VERSION
}

impl Default for TaskStallState {
    fn default() -> Self {
        Self {
            schema_version: TASK_STALL_SCHEMA_VERSION,
            task_revision: 0,
            tool_results_since_progress: 0,
            total_tool_results: 0,
            last_progress_seq: 0,
            last_progress: None,
            recent_progress: SeenProgressTracker::with_capacity(32),
            recovery_total: 0,
            epoch_recovery_level: 0,
            recovery_state: StallRecoveryState::None,
            last_recovery_seq: None,
            post_recovery_tool_results_without_progress: 0,
            last_activity_seq: 0,
            unattributed_operations: 0,
            last_recovery_source_index: None,
        }
    }
}

impl TaskStallState {
    /// Old checkpoints stored `recoveryCount` + `recoveryState` without an epoch.
    pub fn migrate(&mut self) {
        if self.epoch_recovery_level == 0 {
            self.epoch_recovery_level = match self.recovery_state {
                StallRecoveryState::Escalated => 2,
                StallRecoveryState::Warned => 1,
                StallRecoveryState::None => 0,
            };
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskStallTransition {
    pub genuine_progress: bool,
    pub counted: bool,
    pub stall_candidate: bool,
    pub recovery: Option<StallRecoveryAction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StallRecoveryAction {
    pub message: String,
    pub recovery_count: u32,
    pub level: StallRecoveryState,
}

/// One completed tool result. Attribution miss still counts as work.
pub struct CompletedToolObservation<'a> {
    pub operation: &'a EvidenceOperation,
    pub attributed: bool,
    pub user_instruction_revision: u64,
}

/// Fold one completed tool result. Updates counters and fingerprints only.
/// Never decides or records a recovery side effect.
pub fn reduce_task_stall_observation(
    state: &mut TaskStallState,
    observation: CompletedToolObservation<'_>,
    policy: &TaskStallPolicy,
) -> TaskStallTransition {
    if policy.mode == StallMode::Off || !observation.operation.completed {
        return TaskStallTransition {
            genuine_progress: false,
            counted: false,
            stall_candidate: false,
            recovery: None,
        };
    }

    state.schema_version = TASK_STALL_SCHEMA_VERSION;
    state.migrate();
    state
        .recent_progress
        .set_capacity(policy.progress_history_capacity);
    state.last_activity_seq = state.last_activity_seq.saturating_add(1);
    let seq = state.last_activity_seq;
    state.total_tool_results = state.total_tool_results.saturating_add(1);
    if !observation.attributed {
        state.unattributed_operations = state.unattributed_operations.saturating_add(1);
    }

    let mut genuine = false;
    if observation.user_instruction_revision > state.task_revision {
        let fp = ProgressFingerprint::new(
            ProgressKind::UserInstruction,
            &format!("rev:{}", observation.user_instruction_revision),
        );
        if state.recent_progress.is_genuine_advance(&fp) {
            genuine = true;
            state.last_progress = Some(fp);
        }
        state.task_revision = observation.user_instruction_revision;
    }

    let polled = is_status_poll(observation.operation);
    if !polled {
        for fp in fingerprints_from_operation(observation.operation) {
            if state.recent_progress.is_genuine_advance(&fp) {
                genuine = true;
                state.last_progress = Some(fp);
            }
        }
    }

    let counted = !polled;
    if genuine {
        state.tool_results_since_progress = 0;
        state.post_recovery_tool_results_without_progress = 0;
        state.last_progress_seq = seq;
        // Pre-marker progress is how the stall formed; it must not cancel an
        // already-injected recovery epoch while replaying the same window.
        if post_recovery_eligible(state, observation.operation.source_index)
            || state.epoch_recovery_level == 0
        {
            state.epoch_recovery_level = 0;
            state.recovery_state = StallRecoveryState::None;
        }
    } else if counted {
        state.tool_results_since_progress = state.tool_results_since_progress.saturating_add(1);
        if post_recovery_eligible(state, observation.operation.source_index) {
            state.post_recovery_tool_results_without_progress = state
                .post_recovery_tool_results_without_progress
                .saturating_add(1);
        }
    }

    let stall_candidate =
        counted && state.tool_results_since_progress >= policy.recovery_after_tool_results;
    TaskStallTransition {
        genuine_progress: genuine,
        counted,
        stall_candidate,
        recovery: None,
    }
}

/// Back-compat alias: observation fold only, no recovery side effect.
pub fn reduce_task_stall(
    state: &mut TaskStallState,
    observation: CompletedToolObservation<'_>,
    policy: &TaskStallPolicy,
) -> TaskStallTransition {
    reduce_task_stall_observation(state, observation, policy)
}

fn post_recovery_eligible(state: &TaskStallState, source_index: usize) -> bool {
    if state.epoch_recovery_level == 0 {
        return false;
    }
    match state.last_recovery_source_index {
        Some(marker_index) => source_index > marker_index,
        None => true,
    }
}

/// Decide recovery once from the final progress state. Does not mutate `state`.
pub fn decide_task_stall_recovery(
    state: &TaskStallState,
    policy: &TaskStallPolicy,
) -> Option<StallRecoveryAction> {
    if !policy.mode.injects_recovery() {
        return None;
    }
    if state.tool_results_since_progress < policy.recovery_after_tool_results {
        return None;
    }
    if state.epoch_recovery_level == 0 {
        return Some(StallRecoveryAction {
            message: crate::stall_recovery::level_one_message(state),
            recovery_count: state.recovery_total.saturating_add(1),
            level: StallRecoveryState::Warned,
        });
    }
    if state.epoch_recovery_level == 1
        && state.post_recovery_tool_results_without_progress >= policy.post_recovery_patience
    {
        return Some(StallRecoveryAction {
            message: crate::stall_recovery::level_two_message(),
            recovery_count: state.recovery_total.saturating_add(1),
            level: StallRecoveryState::Escalated,
        });
    }
    None
}

/// Eval-only early stop after recovery produced no further progress.
pub fn eval_watchdog_should_stop(state: &TaskStallState, policy: &TaskStallPolicy) -> bool {
    state.recovery_total >= 1
        && state.epoch_recovery_level >= 1
        && state.post_recovery_tool_results_without_progress >= policy.post_recovery_patience
        && state.tool_results_since_progress >= policy.recovery_after_tool_results
}

pub fn stall_recovery_markers(items: &[serde_json::Value]) -> (u32, Option<usize>) {
    let mut max = 0u32;
    let mut index = None;
    for (i, item) in items.iter().enumerate() {
        let id = item
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let Some(rest) = id.strip_prefix("synthetic:task_stall_recovery:") else {
            continue;
        };
        if let Ok(count) = rest.parse::<u32>() {
            if count >= max {
                max = count;
                index = Some(i);
            }
        }
    }
    (max, index)
}

pub fn stall_recovery_marker_count(items: &[serde_json::Value]) -> u32 {
    stall_recovery_markers(items).0
}

/// Apply durable marker identity before observation fold / recovery decision.
/// Only raises `recovery_total` when history contains a newer injected marker.
pub fn reconcile_stall_recovery(
    state: &mut TaskStallState,
    marker_count: u32,
    marker_source_index: Option<usize>,
) {
    state.migrate();
    if marker_count > state.recovery_total {
        let newly = marker_count.saturating_sub(state.recovery_total);
        state.recovery_total = marker_count;
        state.epoch_recovery_level = state
            .epoch_recovery_level
            .saturating_add(newly.min(2) as u8)
            .min(2);
        state.recovery_state = if state.epoch_recovery_level >= 2 {
            StallRecoveryState::Escalated
        } else {
            StallRecoveryState::Warned
        };
        state.last_recovery_source_index = marker_source_index;
    } else if state.epoch_recovery_level > 0 && state.last_recovery_source_index.is_none() {
        state.last_recovery_source_index = marker_source_index;
    }
}

fn is_status_poll(operation: &EvidenceOperation) -> bool {
    if operation.channel != crate::evidence_operation::EvidenceChannel::Process {
        return false;
    }
    if !matches!(
        operation.operation,
        EvidenceOperationKind::Execute
            | EvidenceOperationKind::Inspect
            | EvidenceOperationKind::Query
    ) {
        return false;
    }
    let lower = operation.descriptor.to_ascii_lowercase();
    descriptor_has_token(
        &lower,
        &[
            "get-process",
            "get-job",
            "ps",
            "top",
            "htop",
            "wait-process",
            "wait-job",
            "receive-job",
            "wait",
        ],
    )
}

fn descriptor_has_token(haystack: &str, tokens: &[&str]) -> bool {
    haystack
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-')
        .any(|part| !part.is_empty() && tokens.contains(&part))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TaskStallDiagnostic {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id_hash: Option<String>,
    pub stall_mode: String,
    pub task_revision: u64,
    pub tool_results_since_progress: u32,
    pub last_progress_seq: u64,
    pub last_progress_kind: Option<String>,
    pub last_progress_digest: Option<String>,
    pub recovery_injected_count: u32,
    pub post_recovery_no_progress: u32,
    pub investigation_ledger_count: u64,
    pub unattributed_operations: u64,
    pub stall_candidate: bool,
    /// The persisted state has reached the point where a bounded
    /// finalization may be offered. This is not proof that the request was
    /// terminated; see `TaskStallTerminalDiagnostic` for that observation.
    #[serde(default)]
    pub watchdog_candidate: bool,
    /// Legacy field retained so older artifacts still deserialize. New
    /// diagnostics never set it: candidate and terminal are separate events.
    #[serde(default)]
    pub watchdog_stop: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStallIntervention {
    #[default]
    Warning,
    ToolDisabledFinalization,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TaskStallRecoveryDiagnostic {
    pub request_id_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_index: Option<u32>,
    pub recovery_count: u32,
    pub recovery_level: StallRecoveryState,
    #[serde(default)]
    pub intervention: TaskStallIntervention,
    pub recovery_message_hash: String,
    pub tool_results_since_progress: u32,
    pub post_recovery_no_progress: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_progress: Option<ProgressFingerprint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TaskStallTerminalDiagnostic {
    pub request_id_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_index: Option<u32>,
    pub task_revision: u64,
    pub recovery_count: u32,
    pub post_recovery_no_progress: u32,
    pub category: String,
}

impl TaskStallDiagnostic {
    pub fn from_state(
        state: &TaskStallState,
        policy: &TaskStallPolicy,
        ledger_count: usize,
        stall_candidate: bool,
        request_id_hash: Option<String>,
    ) -> Self {
        Self {
            request_id_hash,
            stall_mode: match policy.mode {
                StallMode::Off => "off".into(),
                StallMode::Shadow => "shadow".into(),
                StallMode::Recover => "recover".into(),
            },
            task_revision: state.task_revision,
            tool_results_since_progress: state.tool_results_since_progress,
            last_progress_seq: state.last_progress_seq,
            last_progress_kind: state
                .last_progress
                .as_ref()
                .map(|fp| format!("{:?}", fp.kind)),
            last_progress_digest: state.last_progress.as_ref().map(|fp| fp.digest.clone()),
            recovery_injected_count: state.recovery_total,
            post_recovery_no_progress: state.post_recovery_tool_results_without_progress,
            investigation_ledger_count: ledger_count as u64,
            unattributed_operations: state.unattributed_operations,
            stall_candidate,
            watchdog_candidate: eval_watchdog_should_stop(state, policy),
            watchdog_stop: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence_operation::{
        CanonicalSubject, CanonicalSubjectKind, EvidenceChannel, EvidenceResult, EvidenceSignature,
        EvidenceSignatureLevel, FactProvenance,
    };

    fn op(
        kind: EvidenceOperationKind,
        channel: EvidenceChannel,
        key: &str,
        idx: usize,
    ) -> EvidenceOperation {
        EvidenceOperation {
            source_index: idx,
            occurrence_id: format!("occ:{idx}"),
            operation: kind,
            channel,
            subjects: vec![CanonicalSubject::new(CanonicalSubjectKind::File, key)],
            result: EvidenceResult::Completed,
            stable_evidence: vec![],
            descriptor: format!("{kind:?}/{channel:?} [{key}] tool {key}"),
            completed: true,
        }
    }

    #[test]
    fn skipped_attribution_still_counts_as_work() {
        let mut state = TaskStallState::default();
        let policy = TaskStallPolicy::recover();
        let empty = EvidenceOperation {
            source_index: 1,
            occurrence_id: "occ:1".into(),
            operation: EvidenceOperationKind::Inspect,
            channel: EvidenceChannel::Workspace,
            subjects: vec![],
            result: EvidenceResult::Completed,
            stable_evidence: vec![],
            descriptor: "Inspect/Workspace [] cat README.md".into(),
            completed: true,
        };
        for i in 0..policy.recovery_after_tool_results {
            empty_idx(&mut state, &policy, &empty, i);
        }
        assert!(state.tool_results_since_progress >= policy.recovery_after_tool_results);
        assert!(state.unattributed_operations >= policy.recovery_after_tool_results as u64);
    }

    fn empty_idx(
        state: &mut TaskStallState,
        policy: &TaskStallPolicy,
        empty: &EvidenceOperation,
        i: u32,
    ) {
        let mut obs = empty.clone();
        obs.source_index = i as usize;
        reduce_task_stall(
            state,
            CompletedToolObservation {
                operation: &obs,
                attributed: false,
                user_instruction_revision: 0,
            },
            policy,
        );
    }

    #[test]
    fn helper_write_resets_once_then_reads_accumulate() {
        let mut state = TaskStallState::default();
        let policy = TaskStallPolicy::recover();
        let write = op(
            EvidenceOperationKind::Mutate,
            EvidenceChannel::Workspace,
            "_extract_session.py",
            1,
        );
        let t = reduce_task_stall(
            &mut state,
            CompletedToolObservation {
                operation: &write,
                attributed: true,
                user_instruction_revision: 0,
            },
            &policy,
        );
        assert!(t.genuine_progress);
        assert_eq!(state.tool_results_since_progress, 0);
        reduce_task_stall(
            &mut state,
            CompletedToolObservation {
                operation: &write,
                attributed: true,
                user_instruction_revision: 0,
            },
            &policy,
        );
        assert_eq!(
            state.tool_results_since_progress, 1,
            "identical helper rewrite is not new progress"
        );
        let read = op(
            EvidenceOperationKind::Query,
            EvidenceChannel::Session,
            "prior_task_state",
            2,
        );
        for i in 0..policy.recovery_after_tool_results {
            let mut r = read.clone();
            r.source_index = 10 + i as usize;
            reduce_task_stall(
                &mut state,
                CompletedToolObservation {
                    operation: &r,
                    attributed: true,
                    user_instruction_revision: 0,
                },
                &policy,
            );
        }
        assert!(state.tool_results_since_progress >= policy.recovery_after_tool_results);
        let recovery = decide_task_stall_recovery(&state, &policy);
        assert!(
            recovery.is_some(),
            "session rereads must stall after a helper rewrite"
        );
        assert_eq!(
            state.recovery_total, 0,
            "observation fold must not mint recovery_total"
        );
    }

    #[test]
    fn session_found_signature_is_not_independent_progress() {
        let mut state = TaskStallState::default();
        let policy = TaskStallPolicy::recover();
        let mut found = op(
            EvidenceOperationKind::Query,
            EvidenceChannel::Session,
            "prior_task_state",
            1,
        );
        found.result = EvidenceResult::Found;
        found.stable_evidence = vec![EvidenceSignature {
            level: EvidenceSignatureLevel::StructuredFact,
            digest: "sha256:session-hit".into(),
            provenance: FactProvenance::Independent,
            fact: Some("echo".into()),
        }];
        reduce_task_stall(
            &mut state,
            CompletedToolObservation {
                operation: &found,
                attributed: true,
                user_instruction_revision: 0,
            },
            &policy,
        );
        assert_eq!(state.tool_results_since_progress, 1);
        assert!(state.last_progress.is_none());
    }

    #[test]
    fn fail_then_same_fail_is_not_new_progress_fail_then_pass_is() {
        let mut state = TaskStallState::default();
        let policy = TaskStallPolicy::shadow();
        let mut fail = op(
            EvidenceOperationKind::Verify,
            EvidenceChannel::Workspace,
            "cargo_test",
            1,
        );
        fail.result = crate::evidence_operation::EvidenceResult::Error {
            family: crate::tool_output_normalizer::ErrorFamily::Other,
        };
        reduce_task_stall(
            &mut state,
            CompletedToolObservation {
                operation: &fail,
                attributed: true,
                user_instruction_revision: 0,
            },
            &policy,
        );
        let after_first = state.tool_results_since_progress;
        reduce_task_stall(
            &mut state,
            CompletedToolObservation {
                operation: &fail,
                attributed: true,
                user_instruction_revision: 0,
            },
            &policy,
        );
        assert!(state.tool_results_since_progress > after_first);
        let mut pass = fail.clone();
        pass.result = EvidenceResult::Completed;
        pass.source_index = 3;
        let t = reduce_task_stall(
            &mut state,
            CompletedToolObservation {
                operation: &pass,
                attributed: true,
                user_instruction_revision: 0,
            },
            &policy,
        );
        assert!(t.genuine_progress);
        assert_eq!(state.tool_results_since_progress, 0);
    }

    #[test]
    fn process_polls_do_not_trip_stall() {
        let mut state = TaskStallState::default();
        let policy = TaskStallPolicy::recover();
        let poll = EvidenceOperation {
            source_index: 1,
            occurrence_id: "occ:1".into(),
            operation: EvidenceOperationKind::Execute,
            channel: EvidenceChannel::Process,
            subjects: vec![],
            result: EvidenceResult::Completed,
            stable_evidence: vec![],
            descriptor: "Execute/Process [] Get-Process python".into(),
            completed: true,
        };
        for i in 0..policy.recovery_after_tool_results + 4 {
            let mut p = poll.clone();
            p.source_index = i as usize;
            reduce_task_stall(
                &mut state,
                CompletedToolObservation {
                    operation: &p,
                    attributed: true,
                    user_instruction_revision: 0,
                },
                &policy,
            );
        }
        assert_eq!(state.recovery_total, 0);
        assert_eq!(state.tool_results_since_progress, 0);
        assert!(decide_task_stall_recovery(&state, &policy).is_none());
    }

    #[test]
    fn wait_result_is_exempt_from_stall() {
        let mut state = TaskStallState::default();
        let policy = TaskStallPolicy::recover();
        let wait = EvidenceOperation {
            source_index: 1,
            occurrence_id: "occ:1".into(),
            operation: EvidenceOperationKind::Execute,
            channel: EvidenceChannel::Process,
            subjects: vec![],
            result: EvidenceResult::Completed,
            stable_evidence: vec![],
            descriptor: "Execute/Process [] Wait-Process -Id 42".into(),
            completed: true,
        };
        for i in 0..policy.recovery_after_tool_results + 2 {
            let mut w = wait.clone();
            w.source_index = i as usize;
            reduce_task_stall(
                &mut state,
                CompletedToolObservation {
                    operation: &w,
                    attributed: true,
                    user_instruction_revision: 0,
                },
                &policy,
            );
        }
        assert_eq!(state.tool_results_since_progress, 0);
        assert!(decide_task_stall_recovery(&state, &policy).is_none());
    }

    #[test]
    fn workspace_search_for_wait_is_not_a_process_poll() {
        let mut state = TaskStallState::default();
        let policy = TaskStallPolicy::recover();
        let search = EvidenceOperation {
            source_index: 1,
            occurrence_id: "occ:1".into(),
            operation: EvidenceOperationKind::Query,
            channel: EvidenceChannel::Workspace,
            subjects: vec![],
            result: EvidenceResult::Completed,
            stable_evidence: vec![],
            descriptor: "Query/Workspace [] rg wait src".into(),
            completed: true,
        };
        for i in 0..policy.recovery_after_tool_results {
            let mut operation = search.clone();
            operation.source_index = i as usize;
            reduce_task_stall(
                &mut state,
                CompletedToolObservation {
                    operation: &operation,
                    attributed: false,
                    user_instruction_revision: 0,
                },
                &policy,
            );
        }
        assert_eq!(
            state.tool_results_since_progress,
            policy.recovery_after_tool_results
        );
        assert!(decide_task_stall_recovery(&state, &policy).is_some());
    }

    #[test]
    fn git_status_and_kill_count_toward_stall() {
        let mut state = TaskStallState::default();
        let policy = TaskStallPolicy::recover();
        let git_status = EvidenceOperation {
            source_index: 1,
            occurrence_id: "occ:git".into(),
            operation: EvidenceOperationKind::Inspect,
            channel: EvidenceChannel::Vcs,
            subjects: vec![],
            result: EvidenceResult::Completed,
            stable_evidence: vec![],
            descriptor: "Inspect/Vcs [repository] git status".into(),
            completed: true,
        };
        for i in 0..20 {
            let mut g = git_status.clone();
            g.source_index = i;
            reduce_task_stall(
                &mut state,
                CompletedToolObservation {
                    operation: &g,
                    attributed: true,
                    user_instruction_revision: 0,
                },
                &policy,
            );
        }
        assert!(state.tool_results_since_progress >= 16);
        assert!(decide_task_stall_recovery(&state, &policy).is_some());

        let mut kill_state = TaskStallState::default();
        let kill = EvidenceOperation {
            source_index: 1,
            occurrence_id: "occ:kill".into(),
            operation: EvidenceOperationKind::Execute,
            channel: EvidenceChannel::Process,
            subjects: vec![],
            result: EvidenceResult::Completed,
            stable_evidence: vec![],
            descriptor: "Execute/Process [] kill -9 1234".into(),
            completed: true,
        };
        for i in 0..policy.recovery_after_tool_results {
            let mut k = kill.clone();
            k.source_index = i as usize;
            k.descriptor = format!("Execute/Process [] pkill -f worker-{i}");
            reduce_task_stall(
                &mut kill_state,
                CompletedToolObservation {
                    operation: &k,
                    attributed: true,
                    user_instruction_revision: 0,
                },
                &policy,
            );
        }
        assert_eq!(
            kill_state.tool_results_since_progress,
            policy.recovery_after_tool_results
        );
        assert!(decide_task_stall_recovery(&kill_state, &policy).is_some());
    }

    #[test]
    fn watchdog_only_after_recovery_patience() {
        let mut state = TaskStallState::default();
        let policy = TaskStallPolicy::recover();
        assert!(!eval_watchdog_should_stop(&state, &policy));
        let read = op(
            EvidenceOperationKind::Query,
            EvidenceChannel::Workspace,
            "x",
            1,
        );
        for i in 0..policy.recovery_after_tool_results {
            let mut r = read.clone();
            r.source_index = i as usize;
            reduce_task_stall(
                &mut state,
                CompletedToolObservation {
                    operation: &r,
                    attributed: true,
                    user_instruction_revision: 0,
                },
                &policy,
            );
        }
        assert!(decide_task_stall_recovery(&state, &policy).is_some());
        assert!(!eval_watchdog_should_stop(&state, &policy));
        reconcile_stall_recovery(&mut state, 1, Some(50));
        assert_eq!(state.recovery_total, 1);
        assert_eq!(state.epoch_recovery_level, 1);
        assert!(!eval_watchdog_should_stop(&state, &policy));
        for i in 0..policy.post_recovery_patience {
            let mut r = read.clone();
            r.source_index = 100 + i as usize;
            reduce_task_stall(
                &mut state,
                CompletedToolObservation {
                    operation: &r,
                    attributed: true,
                    user_instruction_revision: 0,
                },
                &policy,
            );
        }
        assert!(eval_watchdog_should_stop(&state, &policy));
        let diagnostic = TaskStallDiagnostic::from_state(&state, &policy, 1, true, None);
        assert!(diagnostic.watchdog_candidate);
        assert!(!diagnostic.watchdog_stop);
        let level_two = decide_task_stall_recovery(&state, &policy).expect("level 2");
        assert_eq!(level_two.level, StallRecoveryState::Escalated);
        assert_eq!(level_two.recovery_count, 2);
    }

    #[test]
    fn genuine_progress_starts_a_new_level_one_epoch() {
        let mut state = TaskStallState::default();
        let policy = TaskStallPolicy::recover();
        let read = op(
            EvidenceOperationKind::Query,
            EvidenceChannel::Workspace,
            "x",
            1,
        );
        for i in 0..policy.recovery_after_tool_results {
            let mut r = read.clone();
            r.source_index = i as usize;
            reduce_task_stall(
                &mut state,
                CompletedToolObservation {
                    operation: &r,
                    attributed: true,
                    user_instruction_revision: 0,
                },
                &policy,
            );
        }
        reconcile_stall_recovery(&mut state, 1, Some(20));
        assert_eq!(state.epoch_recovery_level, 1);
        let write = op(
            EvidenceOperationKind::Mutate,
            EvidenceChannel::Workspace,
            "src/a.rs",
            30,
        );
        let t = reduce_task_stall(
            &mut state,
            CompletedToolObservation {
                operation: &write,
                attributed: true,
                user_instruction_revision: 0,
            },
            &policy,
        );
        assert!(t.genuine_progress);
        assert_eq!(state.epoch_recovery_level, 0);
        assert_eq!(state.recovery_total, 1);
        for i in 0..policy.recovery_after_tool_results {
            let mut r = read.clone();
            r.source_index = 40 + i as usize;
            reduce_task_stall(
                &mut state,
                CompletedToolObservation {
                    operation: &r,
                    attributed: true,
                    user_instruction_revision: 0,
                },
                &policy,
            );
        }
        let next = decide_task_stall_recovery(&state, &policy).expect("new epoch level 1");
        assert_eq!(next.level, StallRecoveryState::Warned);
        assert_eq!(next.recovery_count, 2);
    }

    #[test]
    fn progress_history_capacity_is_applied_from_policy() {
        let mut state = TaskStallState::default();
        let mut policy = TaskStallPolicy::shadow();
        policy.progress_history_capacity = 2;
        let a = op(
            EvidenceOperationKind::Mutate,
            EvidenceChannel::Workspace,
            "a.rs",
            1,
        );
        let b = op(
            EvidenceOperationKind::Mutate,
            EvidenceChannel::Workspace,
            "b.rs",
            2,
        );
        let c = op(
            EvidenceOperationKind::Mutate,
            EvidenceChannel::Workspace,
            "c.rs",
            3,
        );
        reduce_task_stall(
            &mut state,
            CompletedToolObservation {
                operation: &a,
                attributed: true,
                user_instruction_revision: 0,
            },
            &policy,
        );
        reduce_task_stall(
            &mut state,
            CompletedToolObservation {
                operation: &b,
                attributed: true,
                user_instruction_revision: 0,
            },
            &policy,
        );
        reduce_task_stall(
            &mut state,
            CompletedToolObservation {
                operation: &c,
                attributed: true,
                user_instruction_revision: 0,
            },
            &policy,
        );
        assert_eq!(state.recent_progress.capacity(), 2);
        let again_a = reduce_task_stall(
            &mut state,
            CompletedToolObservation {
                operation: &a,
                attributed: true,
                user_instruction_revision: 0,
            },
            &policy,
        );
        assert!(
            again_a.genuine_progress,
            "capacity 2 must drop the oldest mutation fingerprint"
        );
    }
}
