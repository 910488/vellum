//! Multi-Axis Task Efficiency Control Plane (Vellum Token Efficiency).
//!
//! Separates progress authority into three independent axes:
//! - WorldChange (User instructions, confirmed workspace/vcs mutations)
//! - EvidenceGain (Independent external facts, file reads, queries)
//! - ValidationChange (Test / verification state transitions)
//!
//! Provides token accounting per axis and detects research sprawl
//! (gathering substantial evidence without converting to action).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::evidence_operation::{EvidenceChannel, EvidenceOperation, EvidenceOperationKind};
use crate::progress_fingerprint::{
    efficiency_axis, fingerprints_from_operation, ProgressFingerprint, ProgressKind,
    SeenProgressTracker,
};

pub const TASK_EFFICIENCY_SCHEMA_VERSION: u32 = 1;

fn sha256_hex(payload: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(payload.as_bytes()))
}

/// Independent progress axes. One axis advancing never resets another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EfficiencyProgressAxis {
    WorldChange,
    EvidenceGain,
    ValidationChange,
}

/// Coarse operation classification for exploration vs mutation balance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EfficiencyOperationClass {
    Explore,
    Mutate,
    Validate,
    Poll,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EfficiencyMode {
    Off,
    Shadow,
    Recover,
}

impl EfficiencyMode {
    pub fn from_env() -> Self {
        let raw = std::env::var("VELLUM_RESEARCH_SPRAWL_MODE").unwrap_or_default();
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

/// Authoritative request token usage from completed provider exchange.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CompletedRequestUsage {
    #[serde(default)]
    pub usage_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEfficiencyPolicy {
    pub mode: EfficiencyMode,
    /// Token budget of input tokens consumed without a WorldChange before flagging candidate.
    pub research_token_budget: u64,
    /// Minimum ratio of exploration ops in the recent window (e.g. 0.6 = 60%).
    pub min_exploration_ratio: f64,
    /// Minimum completed tool results since world change before flagging candidate.
    pub min_tool_results_since_world_change: u32,
    pub progress_history_capacity: usize,
    /// Eval-only A/B gate for L1 -> L2 escalation. Production defaults on.
    #[serde(default = "default_true")]
    pub action_recovery_escalation: bool,
}

fn default_true() -> bool {
    true
}

impl Default for TaskEfficiencyPolicy {
    fn default() -> Self {
        Self {
            mode: EfficiencyMode::from_env(),
            research_token_budget: 50_000,
            min_exploration_ratio: 0.6,
            min_tool_results_since_world_change: 5,
            progress_history_capacity: 32,
            action_recovery_escalation: !env_switch_is_off(
                "VELLUM_EVAL_ACTION_RECOVERY_ESCALATION",
            ),
        }
    }
}

impl TaskEfficiencyPolicy {
    pub fn shadow() -> Self {
        Self {
            mode: EfficiencyMode::Shadow,
            ..Self::default_without_env()
        }
    }

    pub fn recover() -> Self {
        Self {
            mode: EfficiencyMode::Recover,
            ..Self::default_without_env()
        }
    }

    fn default_without_env() -> Self {
        Self {
            mode: EfficiencyMode::Shadow,
            research_token_budget: 50_000,
            min_exploration_ratio: 0.6,
            min_tool_results_since_world_change: 5,
            progress_history_capacity: 32,
            action_recovery_escalation: true,
        }
    }
}

fn env_switch_is_off(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "off" | "false"
            )
        })
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryConversionRecord {
    pub recovery_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_request_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_mutation_request_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_to_workspace_mutation: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_to_workspace_mutation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_change_request_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_to_validation_change: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_to_validation_change: Option<u64>,
    #[serde(default)]
    pub exploration_ops: u32,
    #[serde(default)]
    pub validation_ops: u32,
    #[serde(default)]
    pub mutation_ops: u32,
    #[serde(default)]
    pub converted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActionRecoveryLevel {
    ConvertEvidence,
    ActionRequired,
}

impl ActionRecoveryLevel {
    /// The more urgent of two levels. `ActionRequired` never de-escalates back
    /// to `ConvertEvidence` (plan §12.3, §29 P3).
    pub fn max_escalation(self, other: Self) -> Self {
        match (self, other) {
            (Self::ActionRequired, _) | (_, Self::ActionRequired) => Self::ActionRequired,
            _ => Self::ConvertEvidence,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveExecutionRecovery {
    pub recovery_count: u32,
    pub level: ActionRecoveryLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_request_index: Option<u32>,
    #[serde(default)]
    pub started_input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker_source_index: Option<usize>,
    #[serde(default)]
    pub post_recovery_tool_results: u32,
    #[serde(default)]
    pub post_recovery_input_tokens: u64,
    #[serde(default)]
    pub exploration_ops: u32,
    #[serde(default)]
    pub validation_ops: u32,
    #[serde(default)]
    pub mutation_ops: u32,
    #[serde(default)]
    pub compactions_since_activation: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EfficiencyRecoveryAnchor {
    pub recovery_count: u32,
    pub request_index: Option<u32>,
    pub cumulative_input_tokens_before_recovery: u64,
    pub marker_source_index: usize,
}

/// Durable efficiency state tracked across the entire task lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEfficiencyState {
    #[serde(default = "default_efficiency_schema")]
    pub schema_version: u32,
    #[serde(default)]
    pub task_revision: u64,

    #[serde(default)]
    pub tool_results_since_world_change: u32,
    #[serde(default)]
    pub tool_results_since_evidence_gain: u32,
    #[serde(default)]
    pub tool_results_since_validation_change: u32,

    #[serde(default)]
    pub input_tokens_since_world_change: u64,
    #[serde(default)]
    pub input_tokens_since_evidence_gain: u64,
    #[serde(default)]
    pub input_tokens_since_validation_change: u64,

    #[serde(default)]
    pub max_input_tokens_since_world_change: u64,
    #[serde(default)]
    pub max_tool_results_since_world_change: u32,

    #[serde(default)]
    pub cumulative_input_tokens: u64,
    #[serde(default)]
    pub cumulative_cached_input_tokens: u64,
    #[serde(default)]
    pub cumulative_output_tokens: u64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_world_change: Option<ProgressFingerprint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_evidence_gain: Option<ProgressFingerprint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_validation_change: Option<ProgressFingerprint>,

    #[serde(default)]
    pub last_world_change_seq: u64,
    #[serde(default)]
    pub last_evidence_gain_seq: u64,
    #[serde(default)]
    pub last_validation_change_seq: u64,

    #[serde(default)]
    pub recent_world_change: SeenProgressTracker,
    #[serde(default)]
    pub recent_evidence_gain: SeenProgressTracker,
    #[serde(default)]
    pub recent_validation_change: SeenProgressTracker,

    #[serde(default)]
    pub research_sprawl_recovery_total: u32,
    #[serde(default)]
    pub epoch_recovery_level: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_recovery_request_index: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_world_change_request_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_to_first_world_change: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_workspace_mutation_request_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_to_first_workspace_mutation: Option<u64>,

    #[serde(default)]
    pub recent_exploration_ops: u32,
    #[serde(default)]
    pub recent_mutation_ops: u32,
    #[serde(default)]
    pub recent_validation_ops: u32,

    #[serde(default)]
    pub total_tool_results: u64,
    #[serde(default)]
    pub last_activity_seq: u64,

    #[serde(default)]
    pub last_applied_usage_seq: u64,
    #[serde(default)]
    pub seen_request_ids: Vec<String>,
    #[serde(skip)]
    pub last_recovery_source_index: Option<usize>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_recovery_request_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_recovery_input_tokens: Option<u64>,
    #[serde(default)]
    pub recovery_conversions: Vec<RecoveryConversionRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_action_recovery: Option<ActiveExecutionRecovery>,

    #[serde(default)]
    pub exploration_ops_after_recovery: u32,
    #[serde(default)]
    pub validation_ops_after_recovery: u32,
    #[serde(default)]
    pub mutation_ops_after_recovery: u32,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_to_next_workspace_mutation: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_to_next_workspace_mutation: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_to_next_validation_change: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_to_next_validation_change: Option<u64>,
}

fn default_efficiency_schema() -> u32 {
    TASK_EFFICIENCY_SCHEMA_VERSION
}

impl Default for TaskEfficiencyState {
    fn default() -> Self {
        Self {
            schema_version: TASK_EFFICIENCY_SCHEMA_VERSION,
            task_revision: 0,
            tool_results_since_world_change: 0,
            tool_results_since_evidence_gain: 0,
            tool_results_since_validation_change: 0,
            input_tokens_since_world_change: 0,
            input_tokens_since_evidence_gain: 0,
            input_tokens_since_validation_change: 0,
            max_input_tokens_since_world_change: 0,
            max_tool_results_since_world_change: 0,
            cumulative_input_tokens: 0,
            cumulative_cached_input_tokens: 0,
            cumulative_output_tokens: 0,
            last_world_change: None,
            last_evidence_gain: None,
            last_validation_change: None,
            last_world_change_seq: 0,
            last_evidence_gain_seq: 0,
            last_validation_change_seq: 0,
            recent_world_change: SeenProgressTracker::with_capacity(32),
            recent_evidence_gain: SeenProgressTracker::with_capacity(32),
            recent_validation_change: SeenProgressTracker::with_capacity(32),
            research_sprawl_recovery_total: 0,
            epoch_recovery_level: 0,
            last_recovery_request_index: None,
            first_world_change_request_index: None,
            tokens_to_first_world_change: None,
            first_workspace_mutation_request_index: None,
            tokens_to_first_workspace_mutation: None,
            recent_exploration_ops: 0,
            recent_mutation_ops: 0,
            recent_validation_ops: 0,
            total_tool_results: 0,
            last_activity_seq: 0,
            last_applied_usage_seq: 0,
            seen_request_ids: Vec::new(),
            last_recovery_source_index: None,
            active_recovery_request_index: None,
            active_recovery_input_tokens: None,
            recovery_conversions: Vec::new(),
            active_action_recovery: None,
            exploration_ops_after_recovery: 0,
            validation_ops_after_recovery: 0,
            mutation_ops_after_recovery: 0,
            requests_to_next_workspace_mutation: None,
            tokens_to_next_workspace_mutation: None,
            requests_to_next_validation_change: None,
            tokens_to_next_validation_change: None,
        }
    }
}

impl TaskEfficiencyState {
    /// Applies completed request token usage.
    pub fn apply_completed_usage(&mut self, usage: &CompletedRequestUsage) -> bool {
        self.apply_request_usage_with_seq(
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens,
            usage.request_id.as_deref(),
            usage.usage_seq,
        )
    }

    /// Applies authoritative token usage from a completed provider request.
    /// Deduplicates using `request_id` hash to protect against retry double-counting.
    pub fn apply_request_usage(
        &mut self,
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
        request_id: Option<&str>,
    ) {
        self.apply_request_usage_with_seq(
            input_tokens,
            cached_input_tokens,
            output_tokens,
            request_id,
            0,
        );
    }

    /// Applies authoritative token usage with a monotonic sequence watermark.
    /// If `usage_seq > 0 && usage_seq <= self.last_applied_usage_seq`, returns `false` (already folded).
    pub fn apply_request_usage_with_seq(
        &mut self,
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
        request_id: Option<&str>,
        usage_seq: u64,
    ) -> bool {
        // P0-1b: Monotonic sequence watermark check protects against replay double-counting beyond ring buffer size
        if usage_seq > 0 && usage_seq <= self.last_applied_usage_seq {
            return false;
        }

        // P1-3: Non-empty request_id only for transient retry deduplication
        if let Some(req_id) = request_id.filter(|s| !s.trim().is_empty()) {
            let hash = sha256_hex(req_id);
            if self.seen_request_ids.iter().any(|id| id == &hash) {
                return false;
            }
            self.seen_request_ids.push(hash);
            if self.seen_request_ids.len() > 64 {
                let drop = self.seen_request_ids.len() - 64;
                self.seen_request_ids.drain(0..drop);
            }
        }

        if usage_seq > 0 {
            self.last_applied_usage_seq = self.last_applied_usage_seq.max(usage_seq);
        }

        self.cumulative_input_tokens = self.cumulative_input_tokens.saturating_add(input_tokens);
        self.cumulative_cached_input_tokens = self
            .cumulative_cached_input_tokens
            .saturating_add(cached_input_tokens);
        self.cumulative_output_tokens = self.cumulative_output_tokens.saturating_add(output_tokens);

        self.input_tokens_since_world_change = self
            .input_tokens_since_world_change
            .saturating_add(input_tokens);
        self.max_input_tokens_since_world_change = self
            .max_input_tokens_since_world_change
            .max(self.input_tokens_since_world_change);
        self.input_tokens_since_evidence_gain = self
            .input_tokens_since_evidence_gain
            .saturating_add(input_tokens);
        self.input_tokens_since_validation_change = self
            .input_tokens_since_validation_change
            .saturating_add(input_tokens);
        true
    }

    /// Ratio of exploration operations in the recent activity window.
    pub fn exploration_ratio(&self) -> f64 {
        let total = self
            .recent_exploration_ops
            .saturating_add(self.recent_mutation_ops)
            .saturating_add(self.recent_validation_ops);
        if total == 0 {
            return 0.0;
        }
        (self.recent_exploration_ops as f64) / (total as f64)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEfficiencyTransition {
    pub genuine_world_change: bool,
    pub genuine_evidence_gain: bool,
    pub genuine_validation_change: bool,
    pub counted: bool,
    pub research_sprawl_candidate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EfficiencyRecoveryReason {
    ResearchSprawl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EfficiencyRecoveryDecision {
    pub reason: EfficiencyRecoveryReason,
    pub recovery_count: u32,
    pub input_tokens_since_world_change: u64,
    pub tool_results_since_world_change: u32,
    pub recent_exploration_ops: u32,
    pub recent_validation_ops: u32,
    pub recent_mutation_ops: u32,
    pub last_world_change_kind: Option<ProgressKind>,
}

pub fn render_efficiency_recovery_context(decision: &EfficiencyRecoveryDecision) -> String {
    let _ = decision;
    r#"[Vellum execution checkpoint: research sprawl]

The runtime has observed substantial new information without a recent
recognized task-state change.

Stop broadening the investigation unless one specific missing fact blocks
execution.

Use the evidence already collected to do one of:
1. make a concrete workspace change,
2. run one focused verification that resolves the remaining uncertainty,
3. identify one specific prerequisite that prevents either action,
4. if the required workspace result and focused verification are already
   complete, finish the task now without searching for additional evidence.

Do not merely acknowledge this checkpoint; change the next execution step."#
        .to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResearchSprawlRecoveryAction {
    pub decision: EfficiencyRecoveryDecision,
    pub message: String,
    pub recovery_count: u32,
}

/// Immediate, conversation-scoped admission state for model-facing recovery.
///
/// The durable synthetic marker remains the restart/replay authority, but a
/// streaming client may start its next request as soon as it observes the
/// terminal SSE event, before the stream collector has persisted that marker.
/// This state closes that race without replacing durable history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveEfficiencyRecoveryState {
    pub recovery_total: u32,
    pub active: bool,
    pub active_task_revision: u64,
    pub active_world_change: Option<ProgressFingerprint>,
    /// Total completed tool results observed when this live epoch activated.
    /// This process-live baseline lets raced rebuilds recover post-recovery
    /// tool age even before their durable marker is visible.
    pub started_total_tool_results: u64,
    pub active_action_recovery: Option<ActiveExecutionRecovery>,
    pub recovery_conversions: Vec<RecoveryConversionRecord>,
    /// Short process-live handoff after a recovery is converted into a real
    /// workspace mutation. It focuses the next few ordinary turns on the
    /// remaining explicit requirements, verification, and task completion.
    pub active_task_closure: Option<ActiveTaskClosure>,
}

const TASK_CLOSURE_OVERLAY_REQUESTS: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveTaskClosure {
    pub task_revision: u64,
    pub remaining_ordinary_requests: u8,
    pub validation_observed: bool,
    pub last_world_change: Option<ProgressFingerprint>,
    pub last_validation_change: Option<ProgressFingerprint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskClosurePhase {
    VerifyAndFinish,
    FinishAfterValidation,
}

fn activate_task_closure(live: &mut LiveEfficiencyRecoveryState, state: &TaskEfficiencyState) {
    live.active_task_closure = Some(ActiveTaskClosure {
        task_revision: state.task_revision,
        remaining_ordinary_requests: TASK_CLOSURE_OVERLAY_REQUESTS,
        validation_observed: false,
        last_world_change: state.last_world_change.clone(),
        last_validation_change: state.last_validation_change.clone(),
    });
}

/// Refresh closure guidance from real post-conversion progress and expire it
/// across task boundaries. Closure is a bounded execution hint, not a new
/// durable checkpoint epoch.
fn reconcile_task_closure(live: &mut LiveEfficiencyRecoveryState, state: &TaskEfficiencyState) {
    let Some(closure) = live.active_task_closure.as_mut() else {
        return;
    };
    if closure.task_revision != state.task_revision {
        live.active_task_closure = None;
        return;
    }
    if closure.last_world_change != state.last_world_change {
        if state
            .last_world_change
            .as_ref()
            .is_some_and(|fingerprint| fingerprint.kind == ProgressKind::WorkspaceMutation)
        {
            closure.remaining_ordinary_requests = TASK_CLOSURE_OVERLAY_REQUESTS;
            closure.last_world_change = state.last_world_change.clone();
        } else {
            live.active_task_closure = None;
            return;
        }
    }
    if closure.last_validation_change != state.last_validation_change {
        closure.validation_observed = true;
        closure.remaining_ordinary_requests = closure.remaining_ordinary_requests.max(2);
        closure.last_validation_change = state.last_validation_change.clone();
    }
}

/// Consume one ordinary-turn closure overlay. Compaction requests do not call
/// this helper and therefore cannot spend the short guidance lifetime.
pub fn consume_task_closure_overlay(
    live: &mut LiveEfficiencyRecoveryState,
) -> Option<TaskClosurePhase> {
    let closure = live.active_task_closure.as_mut()?;
    let phase = if closure.validation_observed {
        TaskClosurePhase::FinishAfterValidation
    } else {
        TaskClosurePhase::VerifyAndFinish
    };
    closure.remaining_ordinary_requests = closure.remaining_ordinary_requests.saturating_sub(1);
    if closure.remaining_ordinary_requests == 0 {
        live.active_task_closure = None;
    }
    Some(phase)
}

/// Merge two views of the same active recovery epoch, preferring populated
/// identity fields and the higher escalation level. Used when a rebuilt reducer
/// state and the live registry entry both carry a recovery for the same epoch
/// but one lost information to a persistence race.
fn merge_active_execution_recovery(
    a: &ActiveExecutionRecovery,
    b: &ActiveExecutionRecovery,
) -> ActiveExecutionRecovery {
    let (base, other) = if a.recovery_count >= b.recovery_count {
        (a, b)
    } else {
        (b, a)
    };
    ActiveExecutionRecovery {
        recovery_count: base.recovery_count,
        level: base.level.max_escalation(other.level),
        started_request_index: base.started_request_index.or(other.started_request_index),
        started_input_tokens: if base.started_input_tokens != 0 {
            base.started_input_tokens
        } else {
            other.started_input_tokens
        },
        marker_source_index: base.marker_source_index.or(other.marker_source_index),
        post_recovery_tool_results: base
            .post_recovery_tool_results
            .max(other.post_recovery_tool_results),
        post_recovery_input_tokens: base
            .post_recovery_input_tokens
            .max(other.post_recovery_input_tokens),
        exploration_ops: base.exploration_ops.max(other.exploration_ops),
        validation_ops: base.validation_ops.max(other.validation_ops),
        mutation_ops: base.mutation_ops.max(other.mutation_ops),
        compactions_since_activation: base
            .compactions_since_activation
            .max(other.compactions_since_activation),
    }
}

fn merge_recovery_conversion_record(
    a: &RecoveryConversionRecord,
    b: &RecoveryConversionRecord,
) -> RecoveryConversionRecord {
    RecoveryConversionRecord {
        recovery_count: a.recovery_count,
        recovery_request_index: a.recovery_request_index.or(b.recovery_request_index),
        workspace_mutation_request_index: a
            .workspace_mutation_request_index
            .or(b.workspace_mutation_request_index),
        requests_to_workspace_mutation: a
            .requests_to_workspace_mutation
            .or(b.requests_to_workspace_mutation),
        tokens_to_workspace_mutation: a
            .tokens_to_workspace_mutation
            .or(b.tokens_to_workspace_mutation),
        validation_change_request_index: a
            .validation_change_request_index
            .or(b.validation_change_request_index),
        requests_to_validation_change: a
            .requests_to_validation_change
            .or(b.requests_to_validation_change),
        tokens_to_validation_change: a
            .tokens_to_validation_change
            .or(b.tokens_to_validation_change),
        exploration_ops: a.exploration_ops.max(b.exploration_ops),
        validation_ops: a.validation_ops.max(b.validation_ops),
        mutation_ops: a.mutation_ops.max(b.mutation_ops),
        converted: a.converted || b.converted,
    }
}

fn merge_recovery_conversions(
    state_records: &[RecoveryConversionRecord],
    live_records: &[RecoveryConversionRecord],
) -> Vec<RecoveryConversionRecord> {
    let mut merged = state_records.to_vec();
    for live_record in live_records {
        if let Some(existing) = merged
            .iter_mut()
            .find(|record| record.recovery_count == live_record.recovery_count)
        {
            *existing = merge_recovery_conversion_record(existing, live_record);
        } else {
            merged.push(live_record.clone());
        }
    }
    merged.sort_by_key(|record| record.recovery_count);
    merged
}

/// Atomically reconcile a reducer decision with the live conversation gate.
/// Returns `None` when the same WorldChange epoch already received recovery.
///
/// `activation_request_index` is the eval request index of the request that is
/// activating recovery *this call*; it is only consumed on a fresh activation.
/// On activation the active recovery state, the request/token anchors, and the
/// `RecoveryConversionRecord` are all materialized into both the reducer state
/// and the live registry in one step, so a streaming client that begins the
/// next request before the durable marker/checkpoint lands still observes the
/// complete recovery lifecycle (plan §15, §29 P1).
pub fn reconcile_live_efficiency_recovery(
    live: &mut LiveEfficiencyRecoveryState,
    state: &mut TaskEfficiencyState,
    action: Option<ResearchSprawlRecoveryAction>,
    activation_request_index: Option<u32>,
) -> Option<ResearchSprawlRecoveryAction> {
    reconcile_task_closure(live, state);
    live.recovery_total = live
        .recovery_total
        .max(state.research_sprawl_recovery_total);

    let reducer_has_active_marker =
        state.epoch_recovery_level > 0 && state.research_sprawl_recovery_total > 0;
    if reducer_has_active_marker && state.research_sprawl_recovery_total >= live.recovery_total {
        if !live.active {
            let already_post_recovery = state
                .active_action_recovery
                .as_ref()
                .map(|active| active.post_recovery_tool_results)
                .unwrap_or(0);
            live.started_total_tool_results = state
                .total_tool_results
                .saturating_sub(u64::from(already_post_recovery));
        }
        live.active = true;
        live.active_task_revision = state.task_revision;
        live.active_world_change = state.last_world_change.clone();
    } else if live.active
        && (live.active_task_revision != state.task_revision
            || live.active_world_change != state.last_world_change)
    {
        // A raced/full-history rebuild can observe the first post-recovery
        // workspace mutation without reconstructing the durable recovery
        // marker. In that shape the reducer correctly advances
        // `last_world_change`, but cannot credit the conversion because its
        // local epoch gate was never opened. Close that gap from the live
        // authority before deactivating it. A revised user instruction (or an
        // older mutation whose request index predates activation) remains a
        // non-conversion boundary.
        if state
            .last_world_change
            .as_ref()
            .is_some_and(|fingerprint| fingerprint.kind == ProgressKind::WorkspaceMutation)
        {
            if let Some(active) = live.active_action_recovery.as_mut() {
                let mutation_request_index = state
                    .first_workspace_mutation_request_index
                    .or(activation_request_index);
                let mutation_is_post_recovery =
                    match (active.started_request_index, mutation_request_index) {
                        (Some(started), Some(mutation)) => mutation > started,
                        _ => true,
                    };
                if mutation_is_post_recovery {
                    active.mutation_ops = active.mutation_ops.saturating_add(1);
                    if let Some(record) = live
                        .recovery_conversions
                        .iter_mut()
                        .find(|record| record.recovery_count == active.recovery_count)
                    {
                        record.workspace_mutation_request_index = mutation_request_index;
                        record.requests_to_workspace_mutation =
                            match (active.started_request_index, mutation_request_index) {
                                (Some(started), Some(mutation)) => {
                                    Some(mutation.saturating_sub(started))
                                }
                                _ => None,
                            };
                        let token_delta = state
                            .cumulative_input_tokens
                            .saturating_sub(active.started_input_tokens);
                        // Zero here means the completed-usage ledger has not
                        // caught up with the mutation request yet. Report the
                        // latency as unavailable instead of manufacturing an
                        // exact-looking zero-token conversion.
                        record.tokens_to_workspace_mutation =
                            (token_delta > 0).then_some(token_delta);
                        record.mutation_ops = record.mutation_ops.saturating_add(1);
                        record.converted = true;
                        activate_task_closure(live, state);
                    }
                }
            }
        }
        live.active = false;
        live.active_action_recovery = None;
    }

    // Merge by recovery epoch. A rebuilt transcript and the process-live
    // authority may both be non-empty while one trails the other by a stream
    // persistence boundary; replacing either vector wholesale would regress
    // conversion counters or discard a completed conversion.
    let merged_conversions =
        merge_recovery_conversions(&state.recovery_conversions, &live.recovery_conversions);
    state.recovery_conversions = merged_conversions.clone();
    live.recovery_conversions = merged_conversions;

    // Synchronize active_action_recovery, preserving identity fields and the
    // higher escalation level on both sides.
    match (
        state.active_action_recovery.clone(),
        live.active_action_recovery.clone(),
    ) {
        (None, Some(l)) if live.active => state.active_action_recovery = Some(l),
        (Some(s), Some(l)) => {
            let merged = merge_active_execution_recovery(&s, &l);
            state.active_action_recovery = Some(merged.clone());
            live.active_action_recovery = Some(merged);
        }
        (Some(s), None) if live.active => live.active_action_recovery = Some(s),
        _ => {}
    }

    // The live gate deliberately survives a request whose rebuilt transcript
    // has not made the durable marker visible yet. Keep its token-age current
    // from the reducer's authoritative cumulative usage so the 25k L1->L2
    // escalation cannot remain stuck at zero in exactly that raced shape.
    if live.active {
        if let Some(active) = live.active_action_recovery.as_mut() {
            let post_recovery_tool_results = state
                .total_tool_results
                .saturating_sub(live.started_total_tool_results)
                .min(u64::from(u32::MAX)) as u32;
            active.post_recovery_tool_results = active
                .post_recovery_tool_results
                .max(post_recovery_tool_results);
            active.post_recovery_input_tokens = active.post_recovery_input_tokens.max(
                state
                    .cumulative_input_tokens
                    .saturating_sub(active.started_input_tokens),
            );
            if let Some(state_active) = state.active_action_recovery.as_mut() {
                state_active.post_recovery_tool_results = state_active
                    .post_recovery_tool_results
                    .max(active.post_recovery_tool_results);
                state_active.post_recovery_input_tokens = state_active
                    .post_recovery_input_tokens
                    .max(active.post_recovery_input_tokens);
            }
        }
    }

    state.research_sprawl_recovery_total = live.recovery_total;
    if live.active {
        state.epoch_recovery_level = 1;
        return None;
    }

    let mut action = action?;
    let recovery_count = live
        .recovery_total
        .saturating_add(1)
        .max(action.recovery_count);
    action.recovery_count = recovery_count;
    action.decision.recovery_count = recovery_count;
    live.recovery_total = recovery_count;
    live.active = true;
    // A renewed recovery supersedes completion guidance from an older epoch.
    live.active_task_closure = None;
    live.active_task_revision = state.task_revision;
    live.active_world_change = state.last_world_change.clone();
    live.started_total_tool_results = state.total_tool_results;
    state.research_sprawl_recovery_total = recovery_count;
    state.epoch_recovery_level = 1;

    // Atomic activation: request/token anchors + active state + conversion
    // record, materialized into the reducer state and the live registry so the
    // next request rebuild cannot drop any of them (plan §29 P1).
    state.last_recovery_request_index = activation_request_index;
    if state.active_recovery_request_index.is_none() {
        state.active_recovery_request_index = activation_request_index;
        state.active_recovery_input_tokens = Some(state.cumulative_input_tokens);
        state.exploration_ops_after_recovery = 0;
        state.validation_ops_after_recovery = 0;
        state.mutation_ops_after_recovery = 0;
    }
    let started_request_index = activation_request_index.or(state.active_recovery_request_index);

    let active_rec = ActiveExecutionRecovery {
        recovery_count,
        level: ActionRecoveryLevel::ConvertEvidence,
        started_request_index,
        started_input_tokens: state.cumulative_input_tokens,
        marker_source_index: state.last_recovery_source_index,
        post_recovery_tool_results: 0,
        post_recovery_input_tokens: 0,
        exploration_ops: 0,
        validation_ops: 0,
        mutation_ops: 0,
        compactions_since_activation: 0,
    };
    state.active_action_recovery = Some(active_rec.clone());
    live.active_action_recovery = Some(active_rec);

    if !state
        .recovery_conversions
        .iter()
        .any(|r| r.recovery_count == recovery_count)
    {
        state.recovery_conversions.push(RecoveryConversionRecord {
            recovery_count,
            recovery_request_index: started_request_index,
            workspace_mutation_request_index: None,
            requests_to_workspace_mutation: None,
            tokens_to_workspace_mutation: None,
            validation_change_request_index: None,
            requests_to_validation_change: None,
            tokens_to_validation_change: None,
            exploration_ops: 0,
            validation_ops: 0,
            mutation_ops: 0,
            converted: false,
        });
    }
    live.recovery_conversions = state.recovery_conversions.clone();

    Some(action)
}

pub struct CompletedEfficiencyObservation<'a> {
    pub operation: &'a EvidenceOperation,
    pub attributed: bool,
    pub user_instruction_revision: u64,
    pub request_index: Option<u32>,
}

/// Folds one completed tool observation into `TaskEfficiencyState`.
pub fn reduce_task_efficiency_observation(
    state: &mut TaskEfficiencyState,
    observation: CompletedEfficiencyObservation<'_>,
    policy: &TaskEfficiencyPolicy,
) -> TaskEfficiencyTransition {
    if policy.mode == EfficiencyMode::Off || !observation.operation.completed {
        return TaskEfficiencyTransition {
            genuine_world_change: false,
            genuine_evidence_gain: false,
            genuine_validation_change: false,
            counted: false,
            research_sprawl_candidate: false,
        };
    }

    state.schema_version = TASK_EFFICIENCY_SCHEMA_VERSION;
    state
        .recent_world_change
        .set_capacity(policy.progress_history_capacity);
    state
        .recent_evidence_gain
        .set_capacity(policy.progress_history_capacity);
    state
        .recent_validation_change
        .set_capacity(policy.progress_history_capacity);

    state.last_activity_seq = state.last_activity_seq.saturating_add(1);
    let seq = state.last_activity_seq;

    let op_class = classify_operation(observation.operation);

    // Track rolling activity counts (capped to prevent unbound growth)
    let total_recent = state
        .recent_exploration_ops
        .saturating_add(state.recent_mutation_ops)
        .saturating_add(state.recent_validation_ops);
    if total_recent >= 64 {
        state.recent_exploration_ops /= 2;
        state.recent_mutation_ops /= 2;
        state.recent_validation_ops /= 2;
    }
    match op_class {
        EfficiencyOperationClass::Explore => {
            state.recent_exploration_ops = state.recent_exploration_ops.saturating_add(1);
        }
        EfficiencyOperationClass::Mutate => {
            state.recent_mutation_ops = state.recent_mutation_ops.saturating_add(1);
        }
        EfficiencyOperationClass::Validate => {
            state.recent_validation_ops = state.recent_validation_ops.saturating_add(1);
        }
        _ => {}
    }

    let polled = op_class == EfficiencyOperationClass::Poll;
    let counted = !polled;

    if counted {
        state.total_tool_results = state.total_tool_results.saturating_add(1);
        state.tool_results_since_world_change =
            state.tool_results_since_world_change.saturating_add(1);
        state.max_tool_results_since_world_change = state
            .max_tool_results_since_world_change
            .max(state.tool_results_since_world_change);
        state.tool_results_since_evidence_gain =
            state.tool_results_since_evidence_gain.saturating_add(1);
        state.tool_results_since_validation_change =
            state.tool_results_since_validation_change.saturating_add(1);
    }

    let mut genuine_world = false;
    let mut genuine_evidence = false;
    let mut genuine_validation = false;

    let is_post_recovery =
        efficiency_post_recovery_eligible(state, observation.operation.source_index);

    // User instruction revision increment is always a WorldChange
    if observation.user_instruction_revision > state.task_revision {
        // Section 12: Terminate prior conversion window on new user instruction
        if is_post_recovery {
            state.active_recovery_request_index = None;
            state.active_recovery_input_tokens = None;
            state.active_action_recovery = None;
        }
        let fp = ProgressFingerprint::new(
            ProgressKind::UserInstruction,
            &format!("rev:{}", observation.user_instruction_revision),
        );
        if state.recent_world_change.is_genuine_advance(&fp) {
            genuine_world = true;
            state.last_world_change = Some(fp);
            state.last_world_change_seq = seq;
        }
        state.task_revision = observation.user_instruction_revision;
    }

    if !polled {
        for fp in fingerprints_from_operation(observation.operation) {
            match efficiency_axis(fp.kind) {
                EfficiencyProgressAxis::WorldChange => {
                    if state.recent_world_change.is_genuine_advance(&fp) {
                        genuine_world = true;
                        state.last_world_change = Some(fp);
                        state.last_world_change_seq = seq;
                    }
                }
                EfficiencyProgressAxis::EvidenceGain => {
                    if state.recent_evidence_gain.is_genuine_advance(&fp) {
                        genuine_evidence = true;
                        state.last_evidence_gain = Some(fp);
                        state.last_evidence_gain_seq = seq;
                    }
                }
                EfficiencyProgressAxis::ValidationChange => {
                    if state.recent_validation_change.is_genuine_advance(&fp) {
                        genuine_validation = true;
                        state.last_validation_change = Some(fp);
                        state.last_validation_change_seq = seq;
                    }
                }
            }
        }
    }

    let conversion_window_active =
        state.active_recovery_request_index.is_some() && is_post_recovery;
    let cur_recovery_count = state.research_sprawl_recovery_total;

    if conversion_window_active {
        match op_class {
            EfficiencyOperationClass::Explore => {
                state.exploration_ops_after_recovery =
                    state.exploration_ops_after_recovery.saturating_add(1);
                if let Some(rec) = state
                    .recovery_conversions
                    .iter_mut()
                    .find(|r| r.recovery_count == cur_recovery_count)
                {
                    rec.exploration_ops = rec.exploration_ops.saturating_add(1);
                }
            }
            EfficiencyOperationClass::Validate => {
                state.validation_ops_after_recovery =
                    state.validation_ops_after_recovery.saturating_add(1);
                if let Some(rec) = state
                    .recovery_conversions
                    .iter_mut()
                    .find(|r| r.recovery_count == cur_recovery_count)
                {
                    rec.validation_ops = rec.validation_ops.saturating_add(1);
                }
            }
            EfficiencyOperationClass::Mutate => {
                state.mutation_ops_after_recovery =
                    state.mutation_ops_after_recovery.saturating_add(1);
                if let Some(rec) = state
                    .recovery_conversions
                    .iter_mut()
                    .find(|r| r.recovery_count == cur_recovery_count)
                {
                    rec.mutation_ops = rec.mutation_ops.saturating_add(1);
                }
            }
            _ => {}
        }
    }

    if let Some(ref mut active) = state.active_action_recovery {
        if is_post_recovery {
            if counted {
                active.post_recovery_tool_results =
                    active.post_recovery_tool_results.saturating_add(1);
            }
            active.post_recovery_input_tokens = state
                .cumulative_input_tokens
                .saturating_sub(active.started_input_tokens);
            match op_class {
                EfficiencyOperationClass::Explore => {
                    active.exploration_ops = active.exploration_ops.saturating_add(1);
                }
                EfficiencyOperationClass::Validate => {
                    active.validation_ops = active.validation_ops.saturating_add(1);
                }
                EfficiencyOperationClass::Mutate => {
                    active.mutation_ops = active.mutation_ops.saturating_add(1);
                }
                _ => {}
            }
            // Escalation gate: 4 tools or 25,000 post-recovery input tokens
            if policy.action_recovery_escalation
                && active.level == ActionRecoveryLevel::ConvertEvidence
                && (active.post_recovery_tool_results >= 4
                    || active.post_recovery_input_tokens >= 25_000)
            {
                active.level = ActionRecoveryLevel::ActionRequired;
            }
        }
    }

    let is_workspace_mutation = observation.operation.channel
        == crate::evidence_operation::EvidenceChannel::Workspace
        && observation.operation.operation
            == crate::evidence_operation::EvidenceOperationKind::Mutate;

    // Reset axis-specific counters strictly on genuine advances for that axis
    if genuine_world {
        state.tool_results_since_world_change = 0;
        state.input_tokens_since_world_change = 0;

        // Activity window resets strictly with WorldChange epoch
        state.recent_exploration_ops = 0;
        state.recent_mutation_ops = 0;
        state.recent_validation_ops = 0;

        if is_post_recovery {
            // A new user instruction closes the prior conversion window before
            // this operation is folded. Do not credit a mutation made for the
            // revised task to the earlier recovery epoch.
            if conversion_window_active && is_workspace_mutation {
                if let Some(rec) = state
                    .recovery_conversions
                    .iter_mut()
                    .find(|r| r.recovery_count == cur_recovery_count)
                {
                    if rec.requests_to_workspace_mutation.is_none() {
                        if let (Some(rec_req), Some(cur_req)) =
                            (rec.recovery_request_index, observation.request_index)
                        {
                            let delta = cur_req.saturating_sub(rec_req);
                            rec.requests_to_workspace_mutation = Some(delta);
                            rec.workspace_mutation_request_index = Some(cur_req);
                            rec.converted = true;
                            state.requests_to_next_workspace_mutation = Some(delta);
                        }
                    }
                    if rec.tokens_to_workspace_mutation.is_none() {
                        if let Some(rec_tokens) = state.active_recovery_input_tokens {
                            let delta = state.cumulative_input_tokens.saturating_sub(rec_tokens);
                            rec.tokens_to_workspace_mutation = Some(delta);
                            state.tokens_to_next_workspace_mutation = Some(delta);
                        }
                    }
                }
            }
            state.epoch_recovery_level = 0;
            state.last_recovery_source_index = None;
            state.active_recovery_request_index = None;
            state.active_recovery_input_tokens = None;
            state.active_action_recovery = None;
        } else if state.epoch_recovery_level == 0 {
            state.epoch_recovery_level = 0;
        }

        if state.tokens_to_first_world_change.is_none() {
            state.tokens_to_first_world_change = Some(state.cumulative_input_tokens);
            state.first_world_change_request_index = observation.request_index;
        }

        // Distinguish user prompt instruction reset from real workspace mutations:
        if is_workspace_mutation && state.tokens_to_first_workspace_mutation.is_none() {
            state.tokens_to_first_workspace_mutation = Some(state.cumulative_input_tokens);
            state.first_workspace_mutation_request_index = observation.request_index;
        }
    }

    if genuine_evidence {
        state.tool_results_since_evidence_gain = 0;
        state.input_tokens_since_evidence_gain = 0;
        // IndependentEvidence DOES NOT reset WorldChange counters or tokens!
    }

    if genuine_validation {
        state.tool_results_since_validation_change = 0;
        state.input_tokens_since_validation_change = 0;
        if conversion_window_active {
            if let Some(rec) = state
                .recovery_conversions
                .iter_mut()
                .find(|r| r.recovery_count == cur_recovery_count)
            {
                if rec.requests_to_validation_change.is_none() {
                    if let (Some(rec_req), Some(cur_req)) =
                        (rec.recovery_request_index, observation.request_index)
                    {
                        let delta = cur_req.saturating_sub(rec_req);
                        rec.requests_to_validation_change = Some(delta);
                        rec.validation_change_request_index = Some(cur_req);
                        state.requests_to_next_validation_change = Some(delta);
                    }
                }
                if rec.tokens_to_validation_change.is_none() {
                    if let Some(rec_tokens) = state.active_recovery_input_tokens {
                        let delta = state.cumulative_input_tokens.saturating_sub(rec_tokens);
                        rec.tokens_to_validation_change = Some(delta);
                        state.tokens_to_next_validation_change = Some(delta);
                    }
                }
            }
        }
    }

    let candidate = research_sprawl_candidate(state, policy);

    TaskEfficiencyTransition {
        genuine_world_change: genuine_world,
        genuine_evidence_gain: genuine_evidence,
        genuine_validation_change: genuine_validation,
        counted,
        research_sprawl_candidate: candidate,
    }
}

/// Evaluates whether the agent has entered a research sprawl pattern:
/// substantial tokens and tool operations consumed without material world changes,
/// heavily dominated by exploration.
pub fn research_sprawl_candidate(
    state: &TaskEfficiencyState,
    policy: &TaskEfficiencyPolicy,
) -> bool {
    state.input_tokens_since_world_change >= policy.research_token_budget
        && state.exploration_ratio() >= policy.min_exploration_ratio
        && state.tool_results_since_world_change >= policy.min_tool_results_since_world_change
}

/// Decides whether to inject Action Conversion guidance into the conversation.
/// Guidance only; never hard aborts or modifies workspace directly.
pub fn efficiency_post_recovery_eligible(state: &TaskEfficiencyState, source_index: usize) -> bool {
    if state.epoch_recovery_level == 0 {
        return false;
    }
    match state.last_recovery_source_index {
        Some(marker_index) => source_index > marker_index,
        None => true,
    }
}

/// Deterministic user-instruction boundary applied after every operation and
/// usage in a history window has been folded.
///
/// A revised user instruction that arrives with no intervening tool result
/// still advances `task_revision`, still counts as a WorldChange, and still
/// closes any active research-sprawl recovery immediately. Without this the
/// efficiency reducer only learns about the new revision when it folds the
/// next tool operation, so the provider request built between the two turns
/// would carry the previous task's L1/L2 overlay and action pressure.
///
/// The prior conversion record is left untouched (`converted == false`): an
/// instruction reset is never an action-conversion success (plan §13, §25 T5).
pub fn apply_user_instruction_revision_boundary(
    state: &mut TaskEfficiencyState,
    final_task_revision: u64,
) {
    if final_task_revision <= state.task_revision {
        return;
    }

    let had_active_recovery = state.epoch_recovery_level > 0
        || state.active_action_recovery.is_some()
        || state.active_recovery_request_index.is_some();

    let fp = ProgressFingerprint::new(
        ProgressKind::UserInstruction,
        &format!("rev:{final_task_revision}"),
    );
    if state.recent_world_change.is_genuine_advance(&fp) {
        state.last_activity_seq = state.last_activity_seq.saturating_add(1);
        state.last_world_change = Some(fp);
        state.last_world_change_seq = state.last_activity_seq;
        // A fresh WorldChange epoch: the prior task's sprawl counters must not
        // leak into a sprawl candidate decision for the revised task.
        state.tool_results_since_world_change = 0;
        state.input_tokens_since_world_change = 0;
        state.recent_exploration_ops = 0;
        state.recent_mutation_ops = 0;
        state.recent_validation_ops = 0;
    }

    state.task_revision = final_task_revision;

    if had_active_recovery {
        state.epoch_recovery_level = 0;
        state.last_recovery_source_index = None;
        state.active_recovery_request_index = None;
        state.active_recovery_input_tokens = None;
        state.active_action_recovery = None;
    }
}

pub fn decide_research_sprawl_recovery(
    state: &TaskEfficiencyState,
    policy: &TaskEfficiencyPolicy,
) -> Option<ResearchSprawlRecoveryAction> {
    if !policy.mode.injects_recovery() {
        return None;
    }
    if !research_sprawl_candidate(state, policy) {
        return None;
    }
    // Only inject once per stall epoch; resets when WorldChange occurs.
    if state.epoch_recovery_level == 0 {
        let recovery_count = state.research_sprawl_recovery_total.saturating_add(1);
        let decision = EfficiencyRecoveryDecision {
            reason: EfficiencyRecoveryReason::ResearchSprawl,
            recovery_count,
            input_tokens_since_world_change: state.input_tokens_since_world_change,
            tool_results_since_world_change: state.tool_results_since_world_change,
            recent_exploration_ops: state.recent_exploration_ops,
            recent_validation_ops: state.recent_validation_ops,
            recent_mutation_ops: state.recent_mutation_ops,
            last_world_change_kind: state.last_world_change.as_ref().map(|fp| fp.kind),
        };
        let message = render_efficiency_recovery_context(&decision);
        return Some(ResearchSprawlRecoveryAction {
            decision,
            message,
            recovery_count,
        });
    }
    None
}

pub const CONVERT_EVIDENCE_RECOVERY_PROMPT: &str = "\
[Vellum execution control: convert evidence to action]

You have already gathered enough information to stop broadening the investigation.
Prefer execution over additional context gathering.

Make the smallest reversible workspace change supported by the evidence you have,
then run one focused verification and refine from the result.

Do not seek exhaustive certainty before acting. Continue investigation only if you
can name one concrete unknown that directly blocks the next workspace change; if
so, resolve only that unknown and then act.";

pub const ACTION_REQUIRED_RECOVERY_PROMPT: &str = "\
[Vellum execution control: action required]

The previous recovery instruction did not produce a recognized execution change.
Stop further broad, historical, or exploratory investigation.

Make the smallest reversible workspace change that advances the user's task now,
then verify that change with the narrowest relevant check.

Do not search for more context merely to increase confidence.
If execution is genuinely impossible because of an explicit external blocker,
state that blocker clearly instead of expanding the investigation.";

pub fn render_active_execution_overlay(level: ActionRecoveryLevel) -> String {
    match level {
        ActionRecoveryLevel::ConvertEvidence => CONVERT_EVIDENCE_RECOVERY_PROMPT.to_string(),
        ActionRecoveryLevel::ActionRequired => ACTION_REQUIRED_RECOVERY_PROMPT.to_string(),
    }
}

/// Synthetic id of the persistent active-recovery overlay for a given epoch/level.
pub fn active_execution_overlay_id(recovery_count: u32, level: ActionRecoveryLevel) -> String {
    let level_str = match level {
        ActionRecoveryLevel::ConvertEvidence => "convert_evidence",
        ActionRecoveryLevel::ActionRequired => "action_required",
    };
    format!("synthetic:active_execution_recovery:{recovery_count}:{level_str}")
}

/// Rematerialize the single model-visible execution-control block for an ordinary
/// provider request.
///
/// Every prior `synthetic:active_execution_recovery:` overlay and every durable
/// `synthetic:task_efficiency_recovery:` anchor is first stripped from the
/// model-visible input, so the overlay pushed here (when a recovery is active)
/// is the ONLY execution-control block the model sees (plan §7.1, §18, §26 T6/T7).
/// The durable anchor still lives in persisted history for rebuilds; it is only
/// removed from what is dispatched upstream.
pub fn apply_active_execution_overlay(
    input: &mut Vec<serde_json::Value>,
    active: Option<&ActiveExecutionRecovery>,
) {
    input.retain(|item| {
        let id = item
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        !id.starts_with("synthetic:active_execution_recovery:")
            && !id.starts_with("synthetic:task_efficiency_recovery:")
    });
    if let Some(active) = active {
        input.push(serde_json::json!({
            "type": "message",
            "role": "developer",
            "id": active_execution_overlay_id(active.recovery_count, active.level),
            "content": render_active_execution_overlay(active.level),
        }));
    }
}

pub const TASK_CLOSURE_VERIFY_PROMPT: &str = "\
[Vellum execution control: close the task]

A workspace mutation has converted the prior investigation into action.
Do not reopen broad investigation. Review only the explicit user requirements
already in context, address any still-unmet requirement with the smallest
necessary change, run the narrowest relevant verification, and then finish.

If verification already passed and no explicit requirement remains, return the
final answer now. Do not expand into unrelated infrastructure, history, or
background research.";

pub const TASK_CLOSURE_VALIDATED_PROMPT: &str = "\
[Vellum execution control: finish after verification]

The workspace was changed and a new verification result is now available.
Do not reopen broad investigation. If the verification passed and the explicit
user requirements are satisfied, return the final answer now. If it failed,
make one targeted correction, run the narrowest verification, and then finish.";

/// Keep at most one short-lived task-closure instruction in model-visible
/// input. Unlike the durable recovery anchor, this overlay is process-live and
/// is removed automatically when its bounded ordinary-turn lifetime expires.
pub fn apply_task_closure_overlay(
    input: &mut Vec<serde_json::Value>,
    phase: Option<TaskClosurePhase>,
) {
    input.retain(|item| {
        !item
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .starts_with("synthetic:task_closure:")
    });
    if let Some(phase) = phase {
        let (id, content) = match phase {
            TaskClosurePhase::VerifyAndFinish => (
                "synthetic:task_closure:verify_and_finish",
                TASK_CLOSURE_VERIFY_PROMPT,
            ),
            TaskClosurePhase::FinishAfterValidation => (
                "synthetic:task_closure:finish_after_validation",
                TASK_CLOSURE_VALIDATED_PROMPT,
            ),
        };
        input.push(serde_json::json!({
            "type": "message",
            "role": "developer",
            "id": id,
            "content": content,
        }));
    }
}

/// Canonical action conversion recovery guidance.
pub fn action_conversion_guidance_message() -> String {
    let dummy_decision = EfficiencyRecoveryDecision {
        reason: EfficiencyRecoveryReason::ResearchSprawl,
        recovery_count: 1,
        input_tokens_since_world_change: 0,
        tool_results_since_world_change: 0,
        recent_exploration_ops: 0,
        recent_validation_ops: 0,
        recent_mutation_ops: 0,
        last_world_change_kind: None,
    };
    render_efficiency_recovery_context(&dummy_decision)
}

/// Scans raw history items for `synthetic:task_efficiency_recovery:{count}` markers.
pub fn research_sprawl_markers(items: &[serde_json::Value]) -> (u32, Option<usize>) {
    let mut max = 0u32;
    let mut index = None;
    for (i, item) in items.iter().enumerate() {
        let id = item
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let Some(rest) = id.strip_prefix("synthetic:task_efficiency_recovery:") else {
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

/// Reconciles synthetic markers from history before folding observations.
pub fn reconcile_efficiency_recovery(
    state: &mut TaskEfficiencyState,
    marker_count: u32,
    marker_source_index: Option<usize>,
) {
    reconcile_efficiency_recovery_with_anchors(state, marker_count, marker_source_index, &[]);
}

pub fn activate_efficiency_recovery_anchor(
    state: &mut TaskEfficiencyState,
    anchor: &EfficiencyRecoveryAnchor,
) {
    if anchor.recovery_count > state.research_sprawl_recovery_total {
        state.research_sprawl_recovery_total = anchor.recovery_count;
    }
    state.epoch_recovery_level = 1;
    state.last_recovery_source_index = Some(anchor.marker_source_index);
    state.last_recovery_request_index = anchor.request_index;
    state.active_recovery_request_index = anchor.request_index;
    state.active_recovery_input_tokens = Some(anchor.cumulative_input_tokens_before_recovery);
    state.exploration_ops_after_recovery = 0;
    state.validation_ops_after_recovery = 0;
    state.mutation_ops_after_recovery = 0;

    let active_rec = ActiveExecutionRecovery {
        recovery_count: anchor.recovery_count,
        level: ActionRecoveryLevel::ConvertEvidence,
        started_request_index: anchor.request_index,
        started_input_tokens: anchor.cumulative_input_tokens_before_recovery,
        marker_source_index: Some(anchor.marker_source_index),
        post_recovery_tool_results: 0,
        post_recovery_input_tokens: 0,
        exploration_ops: 0,
        validation_ops: 0,
        mutation_ops: 0,
        compactions_since_activation: 0,
    };
    state.active_action_recovery = Some(active_rec);

    if !state
        .recovery_conversions
        .iter()
        .any(|r| r.recovery_count == anchor.recovery_count)
    {
        state.recovery_conversions.push(RecoveryConversionRecord {
            recovery_count: anchor.recovery_count,
            recovery_request_index: anchor.request_index,
            workspace_mutation_request_index: None,
            requests_to_workspace_mutation: None,
            tokens_to_workspace_mutation: None,
            validation_change_request_index: None,
            requests_to_validation_change: None,
            tokens_to_validation_change: None,
            exploration_ops: 0,
            validation_ops: 0,
            mutation_ops: 0,
            converted: false,
        });
    }
}

pub fn reconcile_efficiency_recovery_with_anchors(
    state: &mut TaskEfficiencyState,
    marker_count: u32,
    marker_source_index: Option<usize>,
    anchors: &[EfficiencyRecoveryAnchor],
) {
    if let Some(anchor) = anchors.iter().find(|a| a.recovery_count == marker_count) {
        activate_efficiency_recovery_anchor(state, anchor);
        return;
    }

    if marker_count > state.research_sprawl_recovery_total {
        state.research_sprawl_recovery_total = marker_count;
        state.epoch_recovery_level = 1;
        state.last_recovery_source_index = marker_source_index;

        if state.active_recovery_request_index.is_none() {
            state.active_recovery_request_index = state.last_recovery_request_index;
            state.active_recovery_input_tokens = Some(state.cumulative_input_tokens);
        }
        state.exploration_ops_after_recovery = 0;
        state.validation_ops_after_recovery = 0;
        state.mutation_ops_after_recovery = 0;

        if !state
            .recovery_conversions
            .iter()
            .any(|r| r.recovery_count == marker_count)
        {
            state.recovery_conversions.push(RecoveryConversionRecord {
                recovery_count: marker_count,
                recovery_request_index: state.active_recovery_request_index,
                workspace_mutation_request_index: None,
                requests_to_workspace_mutation: None,
                tokens_to_workspace_mutation: None,
                validation_change_request_index: None,
                requests_to_validation_change: None,
                tokens_to_validation_change: None,
                exploration_ops: 0,
                validation_ops: 0,
                mutation_ops: 0,
                converted: false,
            });
        }
    } else if state.epoch_recovery_level > 0 && state.last_recovery_source_index.is_none() {
        state.last_recovery_source_index = marker_source_index;
    }
}

pub fn classify_operation(operation: &EvidenceOperation) -> EfficiencyOperationClass {
    if is_status_poll(operation) {
        return EfficiencyOperationClass::Poll;
    }
    match operation.operation {
        EvidenceOperationKind::Mutate
            if matches!(
                operation.channel,
                EvidenceChannel::Workspace | EvidenceChannel::Vcs
            ) =>
        {
            EfficiencyOperationClass::Mutate
        }
        EvidenceOperationKind::Verify => EfficiencyOperationClass::Validate,
        EvidenceOperationKind::Inspect | EvidenceOperationKind::Query => {
            EfficiencyOperationClass::Explore
        }
        _ => EfficiencyOperationClass::Other,
    }
}

fn is_status_poll(operation: &EvidenceOperation) -> bool {
    if operation.channel != EvidenceChannel::Process {
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
    haystack_has_token(
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

fn haystack_has_token(haystack: &str, tokens: &[&str]) -> bool {
    haystack
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-')
        .any(|part| !part.is_empty() && tokens.contains(&part))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TaskEfficiencyDiagnostic {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_index: Option<u32>,
    pub efficiency_mode: String,
    pub task_revision: u64,

    pub tool_results_since_world_change: u32,
    pub tool_results_since_evidence_gain: u32,
    pub tool_results_since_validation_change: u32,

    pub input_tokens_since_world_change: u64,
    pub input_tokens_since_evidence_gain: u64,
    pub input_tokens_since_validation_change: u64,

    #[serde(default)]
    pub max_input_tokens_since_world_change: u64,
    #[serde(default)]
    pub max_tool_results_since_world_change: u32,

    pub cumulative_input_tokens: u64,
    pub cumulative_cached_input_tokens: u64,
    pub cumulative_output_tokens: u64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_world_change_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_evidence_gain_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_validation_change_kind: Option<String>,

    pub research_sprawl_candidate: bool,
    pub research_sprawl_recovery_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_to_first_world_change: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_world_change_request_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_to_first_workspace_mutation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_workspace_mutation_request_index: Option<u32>,
    #[serde(default)]
    pub last_applied_usage_seq: u64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_recovery_request_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_to_next_workspace_mutation: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_to_next_workspace_mutation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_to_next_validation_change: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_to_next_validation_change: Option<u64>,
    #[serde(default)]
    pub exploration_ops_after_recovery: u32,
    #[serde(default)]
    pub validation_ops_after_recovery: u32,
    #[serde(default)]
    pub mutation_ops_after_recovery: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_facing_recovery: Option<String>,
    #[serde(default)]
    pub recovery_conversions: Vec<RecoveryConversionRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_action_recovery: Option<ActiveExecutionRecovery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_recovery_level: Option<ActionRecoveryLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compactions_since_recovery_activation: Option<u32>,
}

impl TaskEfficiencyDiagnostic {
    pub fn from_state(
        state: &TaskEfficiencyState,
        policy: &TaskEfficiencyPolicy,
        request_id_hash: Option<String>,
        request_index: Option<u32>,
    ) -> Self {
        Self {
            request_id_hash,
            request_index,
            efficiency_mode: format!("{:?}", policy.mode).to_lowercase(),
            task_revision: state.task_revision,
            tool_results_since_world_change: state.tool_results_since_world_change,
            tool_results_since_evidence_gain: state.tool_results_since_evidence_gain,
            tool_results_since_validation_change: state.tool_results_since_validation_change,
            input_tokens_since_world_change: state.input_tokens_since_world_change,
            input_tokens_since_evidence_gain: state.input_tokens_since_evidence_gain,
            input_tokens_since_validation_change: state.input_tokens_since_validation_change,
            max_input_tokens_since_world_change: state.max_input_tokens_since_world_change,
            max_tool_results_since_world_change: state.max_tool_results_since_world_change,
            cumulative_input_tokens: state.cumulative_input_tokens,
            cumulative_cached_input_tokens: state.cumulative_cached_input_tokens,
            cumulative_output_tokens: state.cumulative_output_tokens,
            last_world_change_kind: state
                .last_world_change
                .as_ref()
                .map(|f| format!("{:?}", f.kind)),
            last_evidence_gain_kind: state
                .last_evidence_gain
                .as_ref()
                .map(|f| format!("{:?}", f.kind)),
            last_validation_change_kind: state
                .last_validation_change
                .as_ref()
                .map(|f| format!("{:?}", f.kind)),
            research_sprawl_candidate: research_sprawl_candidate(state, policy),
            research_sprawl_recovery_count: state.research_sprawl_recovery_total,
            tokens_to_first_world_change: state.tokens_to_first_world_change,
            first_world_change_request_index: state.first_world_change_request_index,
            tokens_to_first_workspace_mutation: state.tokens_to_first_workspace_mutation,
            first_workspace_mutation_request_index: state.first_workspace_mutation_request_index,
            last_applied_usage_seq: state.last_applied_usage_seq,
            last_recovery_request_index: state.last_recovery_request_index,
            requests_to_next_workspace_mutation: state.requests_to_next_workspace_mutation,
            tokens_to_next_workspace_mutation: state.tokens_to_next_workspace_mutation,
            requests_to_next_validation_change: state.requests_to_next_validation_change,
            tokens_to_next_validation_change: state.tokens_to_next_validation_change,
            exploration_ops_after_recovery: state.exploration_ops_after_recovery,
            validation_ops_after_recovery: state.validation_ops_after_recovery,
            mutation_ops_after_recovery: state.mutation_ops_after_recovery,
            model_facing_recovery: None,
            recovery_conversions: state.recovery_conversions.clone(),
            active_action_recovery: state.active_action_recovery.clone(),
            active_recovery_level: state.active_action_recovery.as_ref().map(|a| a.level),
            compactions_since_recovery_activation: state
                .active_action_recovery
                .as_ref()
                .map(|a| a.compactions_since_activation),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TaskEfficiencyRecoveryDiagnostic {
    pub request_id_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_index: Option<u32>,
    pub recovery_count: u32,
    pub recovery_message_hash: String,
    pub input_tokens_since_world_change: u64,
    pub tool_results_since_world_change: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence_operation::{
        CanonicalSubject, CanonicalSubjectKind, EvidenceResult, EvidenceSignature,
        EvidenceSignatureLevel, FactProvenance,
    };

    fn make_read_op(path: &str, digest: &str) -> EvidenceOperation {
        EvidenceOperation {
            source_index: 1,
            occurrence_id: "occ1".into(),
            operation: EvidenceOperationKind::Inspect,
            channel: EvidenceChannel::Workspace,
            subjects: vec![CanonicalSubject::new(CanonicalSubjectKind::File, path)],
            result: EvidenceResult::Completed,
            stable_evidence: vec![EvidenceSignature {
                level: EvidenceSignatureLevel::NormalizedDigest,
                digest: format!("sha256:{digest}"),
                provenance: FactProvenance::Independent,
                fact: None,
            }],
            descriptor: format!("Inspect/Workspace [{path}] read_file {path}"),
            completed: true,
        }
    }

    fn make_mutate_op(path: &str, digest: &str) -> EvidenceOperation {
        EvidenceOperation {
            source_index: 2,
            occurrence_id: "occ2".into(),
            operation: EvidenceOperationKind::Mutate,
            channel: EvidenceChannel::Workspace,
            subjects: vec![CanonicalSubject::new(CanonicalSubjectKind::File, path)],
            result: EvidenceResult::Completed,
            stable_evidence: vec![EvidenceSignature {
                level: EvidenceSignatureLevel::NormalizedDigest,
                digest: format!("sha256:{digest}"),
                provenance: FactProvenance::Independent,
                fact: None,
            }],
            descriptor: format!("Mutate/Workspace [{path}] apply_patch {path}"),
            completed: true,
        }
    }

    fn make_verify_op(test_name: &str, digest: &str) -> EvidenceOperation {
        EvidenceOperation {
            source_index: 3,
            occurrence_id: "occ3".into(),
            operation: EvidenceOperationKind::Verify,
            channel: EvidenceChannel::Workspace,
            subjects: vec![CanonicalSubject::new(CanonicalSubjectKind::Test, test_name)],
            result: EvidenceResult::Completed,
            stable_evidence: vec![EvidenceSignature {
                level: EvidenceSignatureLevel::NormalizedDigest,
                digest: format!("sha256:{digest}"),
                provenance: FactProvenance::Independent,
                fact: None,
            }],
            descriptor: format!("Verify/Workspace [{test_name}] cargo test"),
            completed: true,
        }
    }

    #[test]
    fn evidence_gain_does_not_reset_world_change_counter_or_tokens() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy::shadow();

        state.apply_request_usage(10_000, 2_000, 500, Some("req1"));
        assert_eq!(state.input_tokens_since_world_change, 10_000);
        assert_eq!(state.input_tokens_since_evidence_gain, 10_000);

        let read_op = make_read_op("src/main.rs", "content-1");
        let transition = reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &read_op,
                attributed: true,
                user_instruction_revision: 0,
                request_index: Some(1),
            },
            &policy,
        );

        assert!(transition.genuine_evidence_gain);
        assert!(!transition.genuine_world_change);
        // Evidence gain reset its own counter and tokens
        assert_eq!(state.tool_results_since_evidence_gain, 0);
        assert_eq!(state.input_tokens_since_evidence_gain, 0);

        // Crucial invariant: WorldChange counter and tokens were NOT reset!
        assert_eq!(state.tool_results_since_world_change, 1);
        assert_eq!(state.input_tokens_since_world_change, 10_000);
    }

    #[test]
    fn validation_change_does_not_reset_world_change_counter() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy::shadow();

        state.apply_request_usage(15_000, 0, 100, Some("req1"));
        let verify_op = make_verify_op("test_cipher", "verify-1");
        let transition = reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &verify_op,
                attributed: true,
                user_instruction_revision: 0,
                request_index: Some(1),
            },
            &policy,
        );

        assert!(transition.genuine_validation_change);
        assert!(!transition.genuine_world_change);
        assert_eq!(state.tool_results_since_validation_change, 0);
        assert_eq!(state.input_tokens_since_validation_change, 0);
        assert_eq!(state.tool_results_since_world_change, 1);
        assert_eq!(state.input_tokens_since_world_change, 15_000);
    }

    #[test]
    fn workspace_mutation_resets_world_change_and_records_tokens_to_first() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy::shadow();

        state.apply_request_usage(30_000, 0, 200, Some("req1"));
        assert_eq!(state.tokens_to_first_world_change, None);

        let mutate_op = make_mutate_op("src/cipher.rs", "patch-1");
        let transition = reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &mutate_op,
                attributed: true,
                user_instruction_revision: 0,
                request_index: Some(1),
            },
            &policy,
        );

        assert!(transition.genuine_world_change);
        assert_eq!(state.tool_results_since_world_change, 0);
        assert_eq!(state.input_tokens_since_world_change, 0);
        assert_eq!(state.tokens_to_first_world_change, Some(30_000));
        assert_eq!(state.first_world_change_request_index, Some(1));
    }

    #[test]
    fn research_sprawl_candidate_triggers_and_recovers() {
        let mut state = TaskEfficiencyState::default();
        let mut policy = TaskEfficiencyPolicy::recover();
        policy.research_token_budget = 40_000;
        policy.min_tool_results_since_world_change = 3;
        policy.min_exploration_ratio = 0.6;

        // Model performs 4 read operations consuming 50k tokens
        for i in 0..4 {
            state.apply_request_usage(12_500, 0, 100, Some(&format!("req{i}")));
            let op = make_read_op(&format!("file_{i}.rs"), &format!("digest_{i}"));
            reduce_task_efficiency_observation(
                &mut state,
                CompletedEfficiencyObservation {
                    operation: &op,
                    attributed: true,
                    user_instruction_revision: 0,
                    request_index: Some(i),
                },
                &policy,
            );
        }

        assert_eq!(state.tool_results_since_world_change, 4);
        assert_eq!(state.input_tokens_since_world_change, 50_000);
        assert!(state.exploration_ratio() >= 0.6);
        assert!(research_sprawl_candidate(&state, &policy));

        let recovery = decide_research_sprawl_recovery(&state, &policy);
        assert!(recovery.is_some());
        let action = recovery.unwrap();
        assert_eq!(action.recovery_count, 1);
        assert!(action
            .message
            .contains("[Vellum execution checkpoint: research sprawl]"));

        // Simulate reconciling the injected marker into state
        reconcile_efficiency_recovery(&mut state, 1, Some(10));
        assert_eq!(state.epoch_recovery_level, 1);

        // A second check in the same epoch does not duplicate injection
        let second_decision = decide_research_sprawl_recovery(&state, &policy);
        assert!(second_decision.is_none());

        // Now model performs a workspace mutation post-recovery (source_index 11 > 10)
        let mut mutate_op = make_mutate_op("src/fix.rs", "patch-fix");
        mutate_op.source_index = 11;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &mutate_op,
                attributed: true,
                user_instruction_revision: 0,
                request_index: Some(5),
            },
            &policy,
        );

        // Epoch is reset after genuine world change
        assert_eq!(state.epoch_recovery_level, 0);
        assert_eq!(state.tool_results_since_world_change, 0);
        assert_eq!(state.input_tokens_since_world_change, 0);
    }

    #[test]
    fn recovery_marker_ordering_pre_vs_post() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy::recover();

        // Simulate reconciling an injected recovery marker at source index 10
        reconcile_efficiency_recovery(&mut state, 1, Some(10));
        assert_eq!(state.epoch_recovery_level, 1);
        assert_eq!(state.last_recovery_source_index, Some(10));

        // 1. Replaying a pre-marker workspace mutation (source_index = 5 <= 10)
        // MUST NOT clear the recovery epoch!
        let mut pre_mutate_op = make_mutate_op("src/old.rs", "patch-pre");
        pre_mutate_op.source_index = 5;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &pre_mutate_op,
                attributed: true,
                user_instruction_revision: 0,
                request_index: Some(1),
            },
            &policy,
        );
        assert_eq!(
            state.epoch_recovery_level, 1,
            "pre-marker mutation must not clear recovery epoch"
        );

        // 2. Executing a post-marker workspace mutation (source_index = 12 > 10)
        // MUST clear the recovery epoch!
        let mut post_mutate_op = make_mutate_op("src/new.rs", "patch-post");
        post_mutate_op.source_index = 12;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &post_mutate_op,
                attributed: true,
                user_instruction_revision: 0,
                request_index: Some(2),
            },
            &policy,
        );
        assert_eq!(
            state.epoch_recovery_level, 0,
            "post-marker mutation must clear recovery epoch"
        );
    }

    #[test]
    fn tokens_to_first_workspace_mutation_ignores_user_instruction() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy::shadow();

        state.apply_request_usage(10_000, 0, 100, Some("req1"));
        let read_op = make_read_op("src/main.rs", "d1");

        // Turn where user instruction revision advanced to 1, but tool operation is only Read
        let transition = reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &read_op,
                attributed: true,
                user_instruction_revision: 1,
                request_index: Some(1),
            },
            &policy,
        );

        // World change triggered by user instruction
        assert!(transition.genuine_world_change);
        assert_eq!(state.tokens_to_first_world_change, Some(10_000));
        // BUT workspace mutation metric MUST STILL BE NONE!
        assert_eq!(state.tokens_to_first_workspace_mutation, None);
        assert_eq!(state.first_workspace_mutation_request_index, None);

        // Next request consumes more tokens and performs an actual WorkspaceMutation
        state.apply_request_usage(20_000, 0, 100, Some("req2"));
        let mutate_op = make_mutate_op("src/main.rs", "patch1");
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &mutate_op,
                attributed: true,
                user_instruction_revision: 1,
                request_index: Some(2),
            },
            &policy,
        );

        // Now workspace mutation metric is populated with cumulative tokens (30,000)
        assert_eq!(state.tokens_to_first_workspace_mutation, Some(30_000));
        assert_eq!(state.first_workspace_mutation_request_index, Some(2));
    }

    #[test]
    fn activity_window_resets_on_world_change() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy::shadow();

        for i in 0..10 {
            let op = make_read_op(&format!("f{i}.rs"), &format!("d{i}"));
            reduce_task_efficiency_observation(
                &mut state,
                CompletedEfficiencyObservation {
                    operation: &op,
                    attributed: true,
                    user_instruction_revision: 0,
                    request_index: Some(i),
                },
                &policy,
            );
        }
        assert_eq!(state.recent_exploration_ops, 10);

        // Workspace mutation resets exploration counters
        let mutate_op = make_mutate_op("src/fix.rs", "patch");
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &mutate_op,
                attributed: true,
                user_instruction_revision: 0,
                request_index: Some(11),
            },
            &policy,
        );
        assert_eq!(state.recent_exploration_ops, 0);
        assert_eq!(state.recent_mutation_ops, 0);
    }
    #[test]
    fn test_usage_sequence_monotonic_watermark_ignores_replay() {
        let mut state = TaskEfficiencyState::default();

        // Feed 100 requests with usage_seq 1..=100
        for i in 1..=100 {
            let applied =
                state.apply_request_usage_with_seq(1_000, 0, 100, Some(&format!("req-{i}")), i);
            assert!(applied, "Request {i} should be applied");
        }

        assert_eq!(state.cumulative_input_tokens, 100_000);
        assert_eq!(state.last_applied_usage_seq, 100);

        // Replay all 100 requests from 1..=100 (simulating restart replay where req 1..36 fell out of 64-entry seen set)
        for i in 1..=100 {
            let applied =
                state.apply_request_usage_with_seq(1_000, 0, 100, Some(&format!("req-{i}")), i);
            assert!(
                !applied,
                "Replayed request {i} must be rejected by monotonic watermark"
            );
        }

        assert_eq!(
            state.cumulative_input_tokens, 100_000,
            "Cumulative tokens must not double-count replayed requests"
        );

        // Subsequent new request 101 is applied
        let applied_101 = state.apply_request_usage_with_seq(2_000, 0, 200, Some("req-101"), 101);
        assert!(applied_101);
        assert_eq!(state.cumulative_input_tokens, 102_000);
        assert_eq!(state.last_applied_usage_seq, 101);
    }

    #[test]
    fn test_empty_request_id_does_not_collide_in_dedupe() {
        let mut state = TaskEfficiencyState::default();

        // Apply two usages with None request_id
        state.apply_request_usage(1000, 0, 100, None);
        state.apply_request_usage(1000, 0, 100, None);

        // Apply two usages with empty request_id
        state.apply_request_usage(1000, 0, 100, Some(""));
        state.apply_request_usage(1000, 0, 100, Some("   "));

        assert_eq!(
            state.cumulative_input_tokens, 4000,
            "Empty/None request_id must not collide as duplicate keys"
        );
        assert!(
            state.seen_request_ids.is_empty(),
            "Empty/None request_id must not be added to seen_request_ids"
        );
    }

    #[test]
    fn test_research_sprawl_shadow_mode_does_not_inject() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy {
            mode: EfficiencyMode::Shadow,
            research_token_budget: 80_000,
            min_exploration_ratio: 0.70,
            min_tool_results_since_world_change: 12,
            ..TaskEfficiencyPolicy::default()
        };

        state.input_tokens_since_world_change = 85_000;
        state.tool_results_since_world_change = 15;
        state.recent_exploration_ops = 14;
        state.recent_mutation_ops = 1;
        state.recent_validation_ops = 0;

        assert!(research_sprawl_candidate(&state, &policy));
        assert!(
            decide_research_sprawl_recovery(&state, &policy).is_none(),
            "Shadow mode must not inject recovery"
        );
    }

    #[test]
    fn test_research_sprawl_recover_mode_injects_decision() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy {
            mode: EfficiencyMode::Recover,
            research_token_budget: 80_000,
            min_exploration_ratio: 0.70,
            min_tool_results_since_world_change: 12,
            ..TaskEfficiencyPolicy::default()
        };

        state.input_tokens_since_world_change = 85_000;
        state.tool_results_since_world_change = 15;
        state.recent_exploration_ops = 14;
        state.recent_mutation_ops = 1;
        state.recent_validation_ops = 0;

        assert!(research_sprawl_candidate(&state, &policy));
        let action = decide_research_sprawl_recovery(&state, &policy)
            .expect("Recover mode must emit ResearchSprawlRecoveryAction");
        assert_eq!(
            action.decision.reason,
            EfficiencyRecoveryReason::ResearchSprawl
        );
        assert_eq!(action.recovery_count, 1);
        assert_eq!(action.decision.input_tokens_since_world_change, 85_000);
        assert_eq!(action.decision.tool_results_since_world_change, 15);
        assert!(action
            .message
            .contains("[Vellum execution checkpoint: research sprawl]"));
    }

    #[test]
    fn test_same_epoch_does_not_duplicate_recovery() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy {
            mode: EfficiencyMode::Recover,
            research_token_budget: 80_000,
            min_exploration_ratio: 0.70,
            min_tool_results_since_world_change: 12,
            ..TaskEfficiencyPolicy::default()
        };

        state.input_tokens_since_world_change = 85_000;
        state.tool_results_since_world_change = 15;
        state.recent_exploration_ops = 14;

        // Reconcile first recovery marker
        reconcile_efficiency_recovery(&mut state, 1, Some(10));
        assert_eq!(state.epoch_recovery_level, 1);
        assert_eq!(state.research_sprawl_recovery_total, 1);

        // Same epoch must not inject again
        assert!(decide_research_sprawl_recovery(&state, &policy).is_none());
    }

    #[test]
    fn test_evidence_gain_does_not_clear_recovery() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy {
            mode: EfficiencyMode::Recover,
            ..TaskEfficiencyPolicy::default()
        };

        reconcile_efficiency_recovery(&mut state, 1, Some(10));
        assert_eq!(state.epoch_recovery_level, 1);

        // EvidenceGain operation occurs post-marker
        let mut read_op = make_read_op("src/lib.rs", "digest-1");
        read_op.source_index = 15;
        let trans = reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &read_op,
                attributed: true,
                user_instruction_revision: 0,
                request_index: Some(15),
            },
            &policy,
        );

        assert!(trans.genuine_evidence_gain);
        assert_eq!(
            state.epoch_recovery_level, 1,
            "EvidenceGain must not clear recovery level"
        );
        assert!(decide_research_sprawl_recovery(&state, &policy).is_none());
    }

    #[test]
    fn test_pre_marker_mutation_does_not_clear_recovery() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy {
            mode: EfficiencyMode::Recover,
            ..TaskEfficiencyPolicy::default()
        };

        reconcile_efficiency_recovery(&mut state, 1, Some(10));
        assert_eq!(state.epoch_recovery_level, 1);

        // Mutation at source_index 5 <= marker_index 10
        let mut mutate_op = make_mutate_op("src/lib.rs", "patch-0");
        mutate_op.source_index = 5;
        let trans = reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &mutate_op,
                attributed: true,
                user_instruction_revision: 0,
                request_index: Some(5),
            },
            &policy,
        );

        assert!(trans.genuine_world_change);
        assert_eq!(
            state.epoch_recovery_level, 1,
            "Pre-marker mutation must not clear recovery level"
        );
    }

    #[test]
    fn test_post_marker_mutation_clears_recovery() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy {
            mode: EfficiencyMode::Recover,
            ..TaskEfficiencyPolicy::default()
        };

        reconcile_efficiency_recovery(&mut state, 1, Some(10));
        assert_eq!(state.epoch_recovery_level, 1);

        // Mutation at source_index 15 > marker_index 10
        let mut mutate_op = make_mutate_op("src/lib.rs", "patch-1");
        mutate_op.source_index = 15;
        let trans = reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &mutate_op,
                attributed: true,
                user_instruction_revision: 0,
                request_index: Some(15),
            },
            &policy,
        );

        assert!(trans.genuine_world_change);
        assert_eq!(
            state.epoch_recovery_level, 0,
            "Post-marker mutation must clear recovery level"
        );
        assert_eq!(state.last_recovery_source_index, None);
    }

    #[test]
    fn test_prompt_renderer_invariants() {
        let decision = EfficiencyRecoveryDecision {
            reason: EfficiencyRecoveryReason::ResearchSprawl,
            recovery_count: 1,
            input_tokens_since_world_change: 85_000,
            tool_results_since_world_change: 15,
            recent_exploration_ops: 14,
            recent_validation_ops: 0,
            recent_mutation_ops: 1,
            last_world_change_kind: None,
        };

        let message = render_efficiency_recovery_context(&decision);

        assert!(message.starts_with("[Vellum execution checkpoint: research sprawl]"));
        assert!(message.contains(
            "Stop broadening the investigation unless one specific missing fact blocks\nexecution."
        ));
        assert!(message.contains("1. make a concrete workspace change,"));
        assert!(message
            .contains("2. run one focused verification that resolves the remaining uncertainty,"));
        assert!(
            message.contains("3. identify one specific prerequisite that prevents either action,")
        );
        assert!(message.contains("finish the task now without searching for additional evidence."));
        assert!(message.contains(
            "Do not merely acknowledge this checkpoint; change the next execution step."
        ));

        // Invariant: No raw JSON or internal hashes
        assert!(!message.contains('{'));
        assert!(!message.contains('}'));
        assert!(!message.contains("sha256"));
        assert!(!message.contains("0x"));
    }

    #[test]
    fn test_conversion_mutation_immediately_after_recovery() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy {
            mode: EfficiencyMode::Recover,
            ..TaskEfficiencyPolicy::default()
        };

        state.last_recovery_request_index = Some(10);
        state.cumulative_input_tokens = 50_000;
        reconcile_efficiency_recovery(&mut state, 1, Some(10));
        assert_eq!(state.active_recovery_request_index, Some(10));
        assert_eq!(state.active_recovery_input_tokens, Some(50_000));

        state.cumulative_input_tokens = 55_000;
        let mut mutate_op = make_mutate_op("src/fix.rs", "patch");
        mutate_op.source_index = 11;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &mutate_op,
                attributed: true,
                user_instruction_revision: 0,
                request_index: Some(11),
            },
            &policy,
        );

        assert_eq!(state.mutation_ops_after_recovery, 1);
        assert_eq!(state.requests_to_next_workspace_mutation, Some(1));
        assert_eq!(state.tokens_to_next_workspace_mutation, Some(5_000));
        assert_eq!(
            state.active_recovery_request_index, None,
            "Active window must terminate on WorldChange"
        );
        assert_eq!(state.epoch_recovery_level, 0);
    }

    #[test]
    fn test_conversion_several_explores_then_mutation() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy {
            mode: EfficiencyMode::Recover,
            ..TaskEfficiencyPolicy::default()
        };

        state.last_recovery_request_index = Some(10);
        state.cumulative_input_tokens = 50_000;
        reconcile_efficiency_recovery(&mut state, 1, Some(10));

        // Two explore operations
        for (i, req) in [(11, 11), (12, 12)] {
            let mut read_op = make_read_op(&format!("src/file_{i}.rs"), &format!("digest_{i}"));
            read_op.source_index = i;
            reduce_task_efficiency_observation(
                &mut state,
                CompletedEfficiencyObservation {
                    operation: &read_op,
                    attributed: true,
                    user_instruction_revision: 0,
                    request_index: Some(req),
                },
                &policy,
            );
        }

        assert_eq!(state.exploration_ops_after_recovery, 2);
        assert_eq!(state.mutation_ops_after_recovery, 0);

        // One workspace mutation at request 13
        state.cumulative_input_tokens = 58_000;
        let mut mutate_op = make_mutate_op("src/fix.rs", "patch");
        mutate_op.source_index = 13;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &mutate_op,
                attributed: true,
                user_instruction_revision: 0,
                request_index: Some(13),
            },
            &policy,
        );

        assert_eq!(state.exploration_ops_after_recovery, 2);
        assert_eq!(state.mutation_ops_after_recovery, 1);
        assert_eq!(state.requests_to_next_workspace_mutation, Some(3));
        assert_eq!(state.tokens_to_next_workspace_mutation, Some(8_000));
        assert_eq!(state.active_recovery_request_index, None);
    }

    #[test]
    fn test_conversion_validation_only() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy {
            mode: EfficiencyMode::Recover,
            ..TaskEfficiencyPolicy::default()
        };

        state.last_recovery_request_index = Some(10);
        state.cumulative_input_tokens = 50_000;
        reconcile_efficiency_recovery(&mut state, 1, Some(10));

        state.cumulative_input_tokens = 53_000;
        let mut verify_op = make_verify_op("test_unit", "digest_test");
        verify_op.source_index = 11;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &verify_op,
                attributed: true,
                user_instruction_revision: 0,
                request_index: Some(11),
            },
            &policy,
        );

        assert_eq!(state.validation_ops_after_recovery, 1);
        assert_eq!(state.requests_to_next_validation_change, Some(1));
        assert_eq!(state.tokens_to_next_validation_change, Some(3_000));
        assert_eq!(state.requests_to_next_workspace_mutation, None);
        assert_eq!(
            state.active_recovery_request_index,
            Some(10),
            "Window remains active until WorldChange"
        );
    }

    #[test]
    fn test_conversion_window_closed_on_new_user_instruction() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy {
            mode: EfficiencyMode::Recover,
            ..TaskEfficiencyPolicy::default()
        };

        state.last_recovery_request_index = Some(10);
        reconcile_efficiency_recovery(&mut state, 1, Some(10));
        assert_eq!(state.active_recovery_request_index, Some(10));

        // New user instruction revision
        let mut read_op = make_read_op("src/file.rs", "digest");
        read_op.source_index = 11;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &read_op,
                attributed: true,
                user_instruction_revision: 1, // Revision increments
                request_index: Some(11),
            },
            &policy,
        );

        assert_eq!(
            state.active_recovery_request_index, None,
            "User instruction revision must close prior active window"
        );
    }

    #[test]
    fn new_user_instruction_does_not_credit_same_observation_mutation_to_prior_recovery() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy::recover();

        state.task_revision = 1;
        state.cumulative_input_tokens = 50_000;
        reconcile_efficiency_recovery_with_anchors(
            &mut state,
            1,
            Some(10),
            &[EfficiencyRecoveryAnchor {
                recovery_count: 1,
                request_index: Some(10),
                cumulative_input_tokens_before_recovery: 50_000,
                marker_source_index: 10,
            }],
        );

        state.cumulative_input_tokens = 57_000;
        let mut mutate_op = make_mutate_op("src/revised.rs", "patch");
        mutate_op.source_index = 11;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &mutate_op,
                attributed: true,
                user_instruction_revision: 2,
                request_index: Some(11),
            },
            &policy,
        );

        let conversion = &state.recovery_conversions[0];
        assert_eq!(conversion.requests_to_workspace_mutation, None);
        assert_eq!(conversion.tokens_to_workspace_mutation, None);
        assert!(!conversion.converted);
        assert_eq!(state.active_recovery_request_index, None);
        assert_eq!(state.epoch_recovery_level, 0);
    }

    #[test]
    fn live_gate_prevents_stream_persistence_race_from_reinjecting_same_epoch() {
        let policy = TaskEfficiencyPolicy::recover();
        let mut live = LiveEfficiencyRecoveryState::default();

        let mut first = TaskEfficiencyState::default();
        first.input_tokens_since_world_change = 80_000;
        first.tool_results_since_world_change = 8;
        first.recent_exploration_ops = 8;
        let first_action = decide_research_sprawl_recovery(&first, &policy);
        let claimed =
            reconcile_live_efficiency_recovery(&mut live, &mut first, first_action, Some(11))
                .expect("first eligible request must claim recovery");
        assert_eq!(claimed.recovery_count, 1);
        assert_eq!(first.research_sprawl_recovery_total, 1);
        // Atomic activation materializes the conversion record on both sides.
        assert_eq!(first.recovery_conversions.len(), 1);
        assert_eq!(live.recovery_conversions.len(), 1);
        assert_eq!(
            first
                .active_action_recovery
                .as_ref()
                .unwrap()
                .started_request_index,
            Some(11)
        );
        live.recovery_conversions[0].exploration_ops = 3;

        // The next streaming request can arrive before the synthetic marker
        // is durable, so its rebuilt reducer state still looks like count=0.
        let mut raced = TaskEfficiencyState::default();
        raced.input_tokens_since_world_change = 90_000;
        raced.tool_results_since_world_change = 9;
        raced.recent_exploration_ops = 9;
        raced.recovery_conversions.push(RecoveryConversionRecord {
            recovery_count: 1,
            recovery_request_index: Some(11),
            workspace_mutation_request_index: None,
            requests_to_workspace_mutation: None,
            tokens_to_workspace_mutation: None,
            validation_change_request_index: None,
            requests_to_validation_change: None,
            tokens_to_validation_change: None,
            exploration_ops: 1,
            validation_ops: 0,
            mutation_ops: 0,
            converted: false,
        });
        let raced_action = decide_research_sprawl_recovery(&raced, &policy);
        assert!(
            raced_action.is_some(),
            "the raw reducer reproduces the race"
        );
        assert!(
            reconcile_live_efficiency_recovery(&mut live, &mut raced, raced_action, Some(12))
                .is_none(),
            "the live gate must suppress duplicate recovery in the same epoch"
        );
        assert_eq!(raced.research_sprawl_recovery_total, 1);
        assert_eq!(raced.epoch_recovery_level, 1);
        // The raced rebuild that lost its records recovers them from the live authority.
        assert_eq!(raced.recovery_conversions.len(), 1);
        assert_eq!(raced.recovery_conversions[0].exploration_ops, 3);
        assert_eq!(live.recovery_conversions[0].exploration_ops, 3);
        assert!(raced.active_action_recovery.is_some());

        // A genuine WorldChange opens the next epoch and continues the
        // monotonic recovery count rather than starting again at one.
        let mut next_epoch = raced.clone();
        next_epoch.epoch_recovery_level = 0;
        next_epoch.last_world_change = Some(ProgressFingerprint::new(
            ProgressKind::WorkspaceMutation,
            "src/fixed.rs",
        ));
        next_epoch.input_tokens_since_world_change = 80_000;
        next_epoch.tool_results_since_world_change = 8;
        next_epoch.recent_exploration_ops = 8;
        let next_action = decide_research_sprawl_recovery(&next_epoch, &policy);
        let claimed =
            reconcile_live_efficiency_recovery(&mut live, &mut next_epoch, next_action, Some(20))
                .expect("a new WorldChange epoch may recover once");
        assert_eq!(claimed.recovery_count, 2);
        assert_eq!(next_epoch.research_sprawl_recovery_total, 2);
    }

    #[test]
    fn test_blocker_c_recovery_explore_mutation_computes_latency_and_tokens() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy::default();

        state.cumulative_input_tokens = 80_000;
        state.input_tokens_since_world_change = 80_000;
        state.tool_results_since_world_change = 15;
        state.recent_exploration_ops = 14;
        state.task_revision = 1;

        reconcile_efficiency_recovery_with_anchors(
            &mut state,
            1,
            Some(10),
            &[EfficiencyRecoveryAnchor {
                recovery_count: 1,
                request_index: Some(13),
                cumulative_input_tokens_before_recovery: 80_000,
                marker_source_index: 10,
            }],
        );
        assert_eq!(state.active_recovery_request_index, Some(13));
        assert_eq!(state.active_recovery_input_tokens, Some(80_000));

        // Post-recovery Explore op at request 14
        state.cumulative_input_tokens = 90_000;
        let mut op_explore = make_read_op("file1.rs", "d1");
        op_explore.source_index = 11;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &op_explore,
                attributed: true,
                user_instruction_revision: 1,
                request_index: Some(14),
            },
            &policy,
        );
        assert_eq!(state.exploration_ops_after_recovery, 1);
        assert_eq!(state.recovery_conversions[0].exploration_ops, 1);

        // Post-recovery Mutate op at request 15
        state.cumulative_input_tokens = 105_000;
        let mut op_mutate = make_mutate_op("fix.rs", "d2");
        op_mutate.source_index = 12;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &op_mutate,
                attributed: true,
                user_instruction_revision: 1,
                request_index: Some(15),
            },
            &policy,
        );

        assert_eq!(state.requests_to_next_workspace_mutation, Some(2));
        assert_eq!(state.tokens_to_next_workspace_mutation, Some(25_000));
        assert_eq!(
            state.recovery_conversions[0].requests_to_workspace_mutation,
            Some(2)
        );
        assert_eq!(
            state.recovery_conversions[0].tokens_to_workspace_mutation,
            Some(25_000)
        );
        assert_eq!(
            state.recovery_conversions[0].workspace_mutation_request_index,
            Some(15)
        );
        assert_eq!(state.recovery_conversions[0].converted, true);

        assert!(state.active_recovery_request_index.is_none());
        assert!(state.active_recovery_input_tokens.is_none());
    }

    #[test]
    fn test_blocker_d_multi_recovery_isolated_conversions() {
        let mut state = TaskEfficiencyState::default();
        let policy = TaskEfficiencyPolicy::default();
        state.task_revision = 1;

        // --- EPOCH 1: Recovery #1 at request 10, cumulative 50k ---
        reconcile_efficiency_recovery_with_anchors(
            &mut state,
            1,
            Some(5),
            &[EfficiencyRecoveryAnchor {
                recovery_count: 1,
                request_index: Some(10),
                cumulative_input_tokens_before_recovery: 50_000,
                marker_source_index: 5,
            }],
        );

        // Explore at request 11
        state.cumulative_input_tokens = 60_000;
        let mut op_exp1 = make_read_op("f1.txt", "d1");
        op_exp1.source_index = 6;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &op_exp1,
                attributed: true,
                user_instruction_revision: 1,
                request_index: Some(11),
            },
            &policy,
        );

        // Mutation at request 12
        state.cumulative_input_tokens = 70_000;
        let mut op_mut1 = make_mutate_op("m1.rs", "d2");
        op_mut1.source_index = 7;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &op_mut1,
                attributed: true,
                user_instruction_revision: 1,
                request_index: Some(12),
            },
            &policy,
        );

        assert_eq!(state.recovery_conversions.len(), 1);
        assert_eq!(state.recovery_conversions[0].recovery_count, 1);
        assert_eq!(
            state.recovery_conversions[0].requests_to_workspace_mutation,
            Some(2)
        );
        assert_eq!(
            state.recovery_conversions[0].tokens_to_workspace_mutation,
            Some(20_000)
        );
        assert_eq!(state.recovery_conversions[0].converted, true);

        // --- EPOCH 2: Recovery #2 at request 30, cumulative 120k ---
        reconcile_efficiency_recovery_with_anchors(
            &mut state,
            2,
            Some(20),
            &[
                EfficiencyRecoveryAnchor {
                    recovery_count: 1,
                    request_index: Some(10),
                    cumulative_input_tokens_before_recovery: 50_000,
                    marker_source_index: 5,
                },
                EfficiencyRecoveryAnchor {
                    recovery_count: 2,
                    request_index: Some(30),
                    cumulative_input_tokens_before_recovery: 120_000,
                    marker_source_index: 20,
                },
            ],
        );

        // 3 explore ops post recovery 2
        for i in 1..=3u32 {
            state.cumulative_input_tokens = 120_000 + (i as u64) * 5_000;
            let mut op = make_read_op(&format!("search{i}"), "ds");
            op.source_index = 20 + i as usize;
            reduce_task_efficiency_observation(
                &mut state,
                CompletedEfficiencyObservation {
                    operation: &op,
                    attributed: true,
                    user_instruction_revision: 1,
                    request_index: Some(30 + i),
                },
                &policy,
            );
        }

        // Mutation at request 38
        state.cumulative_input_tokens = 160_000;
        let mut op_mut2 = make_mutate_op("m2.rs", "d3");
        op_mut2.source_index = 25;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &op_mut2,
                attributed: true,
                user_instruction_revision: 1,
                request_index: Some(38),
            },
            &policy,
        );

        assert_eq!(state.recovery_conversions.len(), 2);
        assert_eq!(state.recovery_conversions[0].recovery_count, 1);
        assert_eq!(
            state.recovery_conversions[0].recovery_request_index,
            Some(10)
        );
        assert_eq!(
            state.recovery_conversions[0].requests_to_workspace_mutation,
            Some(2)
        );
        assert_eq!(
            state.recovery_conversions[0].tokens_to_workspace_mutation,
            Some(20_000)
        );
        assert_eq!(state.recovery_conversions[0].exploration_ops, 1);
        assert_eq!(state.recovery_conversions[0].converted, true);

        assert_eq!(state.recovery_conversions[1].recovery_count, 2);
        assert_eq!(
            state.recovery_conversions[1].recovery_request_index,
            Some(30)
        );
        assert_eq!(
            state.recovery_conversions[1].requests_to_workspace_mutation,
            Some(8)
        );
        assert_eq!(
            state.recovery_conversions[1].tokens_to_workspace_mutation,
            Some(40_000)
        );
        assert_eq!(state.recovery_conversions[1].exploration_ops, 3);
        assert_eq!(state.recovery_conversions[1].converted, true);
    }

    #[test]
    fn t1_activation_enters_l1() {
        let mut state = TaskEfficiencyState::default();
        state.task_revision = 1;
        let anchor = EfficiencyRecoveryAnchor {
            recovery_count: 1,
            request_index: Some(10),
            cumulative_input_tokens_before_recovery: 50_000,
            marker_source_index: 5,
        };
        activate_efficiency_recovery_anchor(&mut state, &anchor);
        let active = state
            .active_action_recovery
            .as_ref()
            .expect("active recovery");
        assert_eq!(active.recovery_count, 1);
        assert_eq!(active.level, ActionRecoveryLevel::ConvertEvidence);
        assert_eq!(active.started_request_index, Some(10));
        assert_eq!(active.started_input_tokens, 50_000);
        assert_eq!(active.post_recovery_tool_results, 0);
        assert_eq!(active.compactions_since_activation, 0);
    }

    #[test]
    fn t2_evidence_gain_does_not_close_recovery() {
        let mut state = TaskEfficiencyState::default();
        state.task_revision = 1;
        let policy = TaskEfficiencyPolicy::recover();
        let anchor = EfficiencyRecoveryAnchor {
            recovery_count: 1,
            request_index: Some(10),
            cumulative_input_tokens_before_recovery: 50_000,
            marker_source_index: 5,
        };
        activate_efficiency_recovery_anchor(&mut state, &anchor);

        let mut op = make_read_op("src/main.rs", "content-1");
        op.source_index = 6;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &op,
                attributed: true,
                user_instruction_revision: 1,
                request_index: Some(11),
            },
            &policy,
        );

        let active = state.active_action_recovery.as_ref().expect("still active");
        assert_eq!(active.level, ActionRecoveryLevel::ConvertEvidence);
        assert_eq!(active.post_recovery_tool_results, 1);
        assert_eq!(active.exploration_ops, 1);
    }

    #[test]
    fn t3_validation_change_does_not_close_recovery() {
        let mut state = TaskEfficiencyState::default();
        state.task_revision = 1;
        let policy = TaskEfficiencyPolicy::recover();
        let anchor = EfficiencyRecoveryAnchor {
            recovery_count: 1,
            request_index: Some(10),
            cumulative_input_tokens_before_recovery: 50_000,
            marker_source_index: 5,
        };
        activate_efficiency_recovery_anchor(&mut state, &anchor);

        let mut op = make_verify_op("test_unit", "val-1");
        op.source_index = 6;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &op,
                attributed: true,
                user_instruction_revision: 1,
                request_index: Some(11),
            },
            &policy,
        );

        let active = state.active_action_recovery.as_ref().expect("still active");
        assert_eq!(active.level, ActionRecoveryLevel::ConvertEvidence);
        assert_eq!(active.post_recovery_tool_results, 1);
        assert_eq!(active.validation_ops, 1);
        assert_eq!(
            state.recovery_conversions[0].validation_change_request_index,
            Some(11)
        );
        assert_eq!(state.recovery_conversions[0].converted, false);
    }

    #[test]
    fn t4_workspace_mutation_closes_recovery_and_records_conversion() {
        let mut state = TaskEfficiencyState::default();
        state.task_revision = 1;
        let policy = TaskEfficiencyPolicy::recover();
        let anchor = EfficiencyRecoveryAnchor {
            recovery_count: 1,
            request_index: Some(10),
            cumulative_input_tokens_before_recovery: 50_000,
            marker_source_index: 5,
        };
        activate_efficiency_recovery_anchor(&mut state, &anchor);

        let mut op = make_mutate_op("src/main.rs", "patch-1");
        op.source_index = 6;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &op,
                attributed: true,
                user_instruction_revision: 1,
                request_index: Some(11),
            },
            &policy,
        );

        assert!(state.active_action_recovery.is_none());
        assert_eq!(state.recovery_conversions[0].converted, true);
        assert_eq!(
            state.recovery_conversions[0].workspace_mutation_request_index,
            Some(11)
        );
        assert_eq!(
            state.recovery_conversions[0].requests_to_workspace_mutation,
            Some(1)
        );
    }

    #[test]
    fn t5_user_instruction_resets_recovery_without_marking_converted() {
        let mut state = TaskEfficiencyState::default();
        state.task_revision = 1;
        let policy = TaskEfficiencyPolicy::recover();
        let anchor = EfficiencyRecoveryAnchor {
            recovery_count: 1,
            request_index: Some(10),
            cumulative_input_tokens_before_recovery: 50_000,
            marker_source_index: 5,
        };
        activate_efficiency_recovery_anchor(&mut state, &anchor);

        let mut op = make_read_op("src/main.rs", "content-1");
        op.source_index = 6;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &op,
                attributed: true,
                user_instruction_revision: 2, // Revision increment 2 > 1
                request_index: Some(11),
            },
            &policy,
        );

        assert!(state.active_action_recovery.is_none());
        assert_eq!(state.recovery_conversions[0].converted, false);
    }

    #[test]
    fn t13_four_tools_triggers_l2_escalation() {
        let mut state = TaskEfficiencyState::default();
        state.task_revision = 1;
        let policy = TaskEfficiencyPolicy::recover();
        let anchor = EfficiencyRecoveryAnchor {
            recovery_count: 1,
            request_index: Some(10),
            cumulative_input_tokens_before_recovery: 50_000,
            marker_source_index: 5,
        };
        activate_efficiency_recovery_anchor(&mut state, &anchor);

        for i in 1..=3 {
            let mut op = make_read_op(&format!("src/file{i}.rs"), &format!("digest{i}"));
            op.source_index = 5 + i;
            reduce_task_efficiency_observation(
                &mut state,
                CompletedEfficiencyObservation {
                    operation: &op,
                    attributed: true,
                    user_instruction_revision: 1,
                    request_index: Some(10 + i as u32),
                },
                &policy,
            );
            assert_eq!(
                state.active_action_recovery.as_ref().unwrap().level,
                ActionRecoveryLevel::ConvertEvidence
            );
        }

        let mut op4 = make_read_op("src/file4.rs", "digest4");
        op4.source_index = 9;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &op4,
                attributed: true,
                user_instruction_revision: 1,
                request_index: Some(14),
            },
            &policy,
        );
        assert_eq!(
            state.active_action_recovery.as_ref().unwrap().level,
            ActionRecoveryLevel::ActionRequired
        );
        assert_eq!(
            state
                .active_action_recovery
                .as_ref()
                .unwrap()
                .post_recovery_tool_results,
            4
        );
    }

    #[test]
    fn t14_token_threshold_triggers_l2_escalation() {
        let mut state = TaskEfficiencyState::default();
        state.task_revision = 1;
        let policy = TaskEfficiencyPolicy::recover();
        let anchor = EfficiencyRecoveryAnchor {
            recovery_count: 1,
            request_index: Some(10),
            cumulative_input_tokens_before_recovery: 50_000,
            marker_source_index: 5,
        };
        activate_efficiency_recovery_anchor(&mut state, &anchor);

        // Advance tokens by 26_000 (>= 25_000)
        state.cumulative_input_tokens = 76_000;

        let mut op = make_read_op("src/file1.rs", "d1");
        op.source_index = 6;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &op,
                attributed: true,
                user_instruction_revision: 1,
                request_index: Some(11),
            },
            &policy,
        );
        let active = state.active_action_recovery.as_ref().unwrap();
        assert_eq!(active.level, ActionRecoveryLevel::ActionRequired);
        assert_eq!(active.post_recovery_tool_results, 1);
        assert_eq!(active.post_recovery_input_tokens, 26_000);
    }

    #[test]
    fn focused_eval_can_disable_l2_without_disabling_persistent_recovery() {
        let mut state = TaskEfficiencyState::default();
        state.task_revision = 1;
        let mut policy = TaskEfficiencyPolicy::recover();
        policy.action_recovery_escalation = false;
        activate_efficiency_recovery_anchor(
            &mut state,
            &EfficiencyRecoveryAnchor {
                recovery_count: 1,
                request_index: Some(10),
                cumulative_input_tokens_before_recovery: 50_000,
                marker_source_index: 5,
            },
        );
        state.cumulative_input_tokens = 90_000;

        for i in 1..=4 {
            let mut op = make_read_op(&format!("src/file{i}.rs"), &format!("digest{i}"));
            op.source_index = 5 + i;
            reduce_task_efficiency_observation(
                &mut state,
                CompletedEfficiencyObservation {
                    operation: &op,
                    attributed: true,
                    user_instruction_revision: 1,
                    request_index: Some(10 + i as u32),
                },
                &policy,
            );
        }

        let active = state.active_action_recovery.as_ref().unwrap();
        assert_eq!(active.level, ActionRecoveryLevel::ConvertEvidence);
        assert_eq!(active.post_recovery_tool_results, 4);
        assert_eq!(active.post_recovery_input_tokens, 40_000);
    }

    #[test]
    fn t15_mutation_before_threshold_prevents_l2() {
        let mut state = TaskEfficiencyState::default();
        state.task_revision = 1;
        let policy = TaskEfficiencyPolicy::recover();
        let anchor = EfficiencyRecoveryAnchor {
            recovery_count: 1,
            request_index: Some(10),
            cumulative_input_tokens_before_recovery: 50_000,
            marker_source_index: 5,
        };
        activate_efficiency_recovery_anchor(&mut state, &anchor);

        for i in 1..=2 {
            let mut op = make_read_op(&format!("src/file{i}.rs"), &format!("d{i}"));
            op.source_index = 5 + i;
            reduce_task_efficiency_observation(
                &mut state,
                CompletedEfficiencyObservation {
                    operation: &op,
                    attributed: true,
                    user_instruction_revision: 1,
                    request_index: Some(10 + i as u32),
                },
                &policy,
            );
        }

        let mut op_mut = make_mutate_op("src/main.rs", "m1");
        op_mut.source_index = 8;
        reduce_task_efficiency_observation(
            &mut state,
            CompletedEfficiencyObservation {
                operation: &op_mut,
                attributed: true,
                user_instruction_revision: 1,
                request_index: Some(13),
            },
            &policy,
        );

        assert!(state.active_action_recovery.is_none());
        assert_eq!(state.recovery_conversions[0].converted, true);
        assert_eq!(state.recovery_conversions[0].mutation_ops, 1);
        assert_eq!(state.recovery_conversions[0].exploration_ops, 2);
    }

    #[test]
    fn t16_validation_alone_does_not_prevent_escalation() {
        let mut state = TaskEfficiencyState::default();
        state.task_revision = 1;
        let policy = TaskEfficiencyPolicy::recover();
        let anchor = EfficiencyRecoveryAnchor {
            recovery_count: 1,
            request_index: Some(10),
            cumulative_input_tokens_before_recovery: 50_000,
            marker_source_index: 5,
        };
        activate_efficiency_recovery_anchor(&mut state, &anchor);

        let mut op1 = make_read_op("src/1.rs", "1");
        op1.source_index = 6;
        let mut op2 = make_read_op("src/2.rs", "2");
        op2.source_index = 7;
        let mut op3 = make_verify_op("test_all", "v1");
        op3.source_index = 8;
        let mut op4 = make_read_op("src/3.rs", "3");
        op4.source_index = 9;

        for (idx, op) in [op1, op2, op3, op4].iter().enumerate() {
            reduce_task_efficiency_observation(
                &mut state,
                CompletedEfficiencyObservation {
                    operation: op,
                    attributed: true,
                    user_instruction_revision: 1,
                    request_index: Some(11 + idx as u32),
                },
                &policy,
            );
        }

        let active = state.active_action_recovery.as_ref().unwrap();
        assert_eq!(active.level, ActionRecoveryLevel::ActionRequired);
        assert_eq!(active.post_recovery_tool_results, 4);
        assert_eq!(active.exploration_ops, 3);
        assert_eq!(active.validation_ops, 1);
    }

    #[test]
    fn zero_tool_op_user_instruction_boundary_closes_active_recovery() {
        let policy = TaskEfficiencyPolicy::recover();
        let mut state = TaskEfficiencyState::default();
        state.task_revision = 1;

        let anchor = EfficiencyRecoveryAnchor {
            recovery_count: 1,
            request_index: Some(11),
            cumulative_input_tokens_before_recovery: 61_000,
            marker_source_index: 20,
        };
        activate_efficiency_recovery_anchor(&mut state, &anchor);
        state.input_tokens_since_world_change = 61_000;
        state.tool_results_since_world_change = 6;
        state.recent_exploration_ops = 6;
        assert!(state.active_action_recovery.is_some());

        // A revised user instruction arrives with no intervening tool result.
        apply_user_instruction_revision_boundary(&mut state, 2);

        assert_eq!(state.task_revision, 2);
        assert!(
            state.active_action_recovery.is_none(),
            "recovery must close"
        );
        assert_eq!(state.epoch_recovery_level, 0);
        assert_eq!(state.active_recovery_request_index, None);
        assert_eq!(
            state.input_tokens_since_world_change, 0,
            "sprawl counters reset"
        );
        assert_eq!(state.recent_exploration_ops, 0);
        // The prior conversion record survives but is NOT a conversion success.
        assert_eq!(state.recovery_conversions.len(), 1);
        assert!(!state.recovery_conversions[0].converted);

        // The revised task must not immediately re-trigger recovery off the
        // previous task's accumulated sprawl.
        assert!(decide_research_sprawl_recovery(&state, &policy).is_none());
    }

    #[test]
    fn user_instruction_boundary_is_a_noop_when_revision_already_current() {
        let mut state = TaskEfficiencyState::default();
        state.task_revision = 3;
        let anchor = EfficiencyRecoveryAnchor {
            recovery_count: 1,
            request_index: Some(11),
            cumulative_input_tokens_before_recovery: 61_000,
            marker_source_index: 20,
        };
        activate_efficiency_recovery_anchor(&mut state, &anchor);

        apply_user_instruction_revision_boundary(&mut state, 3);

        assert!(
            state.active_action_recovery.is_some(),
            "an unchanged revision must not disturb an active recovery"
        );
        assert_eq!(state.epoch_recovery_level, 1);
    }

    #[test]
    fn converted_mutation_starts_bounded_task_closure_and_validation_strengthens_it() {
        let policy = TaskEfficiencyPolicy::recover();
        let mut live = LiveEfficiencyRecoveryState::default();
        let mut state = TaskEfficiencyState::default();
        state.task_revision = 1;
        state.cumulative_input_tokens = 60_000;
        state.input_tokens_since_world_change = 60_000;
        state.total_tool_results = 6;
        state.tool_results_since_world_change = 6;
        state.recent_exploration_ops = 6;
        let action = decide_research_sprawl_recovery(&state, &policy);
        reconcile_live_efficiency_recovery(&mut live, &mut state, action, Some(10))
            .expect("recovery activates");

        let mut mutated = TaskEfficiencyState::default();
        mutated.task_revision = 1;
        mutated.cumulative_input_tokens = 70_000;
        mutated.last_world_change = Some(ProgressFingerprint::new(
            ProgressKind::WorkspaceMutation,
            "apply_patch:src/fix.rs",
        ));
        mutated.first_workspace_mutation_request_index = Some(12);
        reconcile_live_efficiency_recovery(&mut live, &mut mutated, None, Some(12));

        assert!(live.active_task_closure.is_some());
        assert_eq!(
            consume_task_closure_overlay(&mut live),
            Some(TaskClosurePhase::VerifyAndFinish)
        );

        let mut verified = mutated.clone();
        verified.last_validation_change = Some(ProgressFingerprint::new(
            ProgressKind::VerificationState,
            "cargo test:passed",
        ));
        reconcile_live_efficiency_recovery(&mut live, &mut verified, None, Some(13));
        assert_eq!(
            consume_task_closure_overlay(&mut live),
            Some(TaskClosurePhase::FinishAfterValidation)
        );
    }

    #[test]
    fn task_closure_overlay_is_single_short_lived_and_task_scoped() {
        let mut live = LiveEfficiencyRecoveryState::default();
        let state = TaskEfficiencyState {
            task_revision: 4,
            last_world_change: Some(ProgressFingerprint::new(
                ProgressKind::WorkspaceMutation,
                "write:src/lib.rs",
            )),
            ..Default::default()
        };
        activate_task_closure(&mut live, &state);

        let mut input = vec![serde_json::json!({
            "type": "message",
            "role": "developer",
            "id": "synthetic:task_closure:stale",
            "content": "stale"
        })];
        for _ in 0..3 {
            let phase = consume_task_closure_overlay(&mut live);
            apply_task_closure_overlay(&mut input, phase);
            assert_eq!(
                input
                    .iter()
                    .filter(|item| item
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .starts_with("synthetic:task_closure:"))
                    .count(),
                1
            );
        }
        assert!(live.active_task_closure.is_none());
        apply_task_closure_overlay(&mut input, consume_task_closure_overlay(&mut live));
        assert!(input.is_empty(), "expired guidance must be stripped");

        activate_task_closure(&mut live, &state);
        let mut revised = state.clone();
        revised.task_revision = 5;
        reconcile_task_closure(&mut live, &revised);
        assert!(live.active_task_closure.is_none());
    }
}
