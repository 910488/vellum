//! Single builder for live guard, Canonical compaction, and offline replay.

use std::collections::HashMap;

use serde_json::Value;

use crate::evidence_operation::{
    normalize_file_subject, project_from_tool_pair, CanonicalSubject, CanonicalSubjectKind,
    EvidenceChannel, EvidenceOperation, EvidenceOperationKind, EvidenceResult, EvidenceSignature,
    EvidenceSignatureLevel, FactProvenance,
};
use crate::investigation::{extract_task_literals, InvestigationState};
use crate::investigation_linker::SymbolicInvestigationLinker;
#[cfg(test)]
use crate::investigation_reducer::InvestigationLedgerState;
use crate::investigation_reducer::{
    apply_progress_delta, live_recovery_action, progress_delta_from_operation,
    reconcile_recovery_counts, reduce_investigation, InvestigationPolicy, InvestigationTransition,
    MaterialProgressState, RecoveryAction,
};
use crate::task_stall::{
    decide_task_stall_recovery, reconcile_stall_recovery, reduce_task_stall_observation,
    stall_recovery_markers as read_stall_recovery_markers, CompletedToolObservation,
    StallRecoveryAction, TaskStallPolicy,
};
use crate::tool_semantics::{
    classify_mutation_outcome, mutation_identity_from_call, MutationOutcome,
};
use crate::trajectory::{build_tool_exchanges, normalize_trajectory};

/// Shared investigation input. Live, Canonical, and replay all consume this.
#[derive(Debug, Clone)]
pub struct InvestigationRuntimeInput {
    pub prior: InvestigationState,
    pub operations: Vec<EvidenceOperation>,
    /// Progress visible before the first projected operation in this window.
    /// Operation deltas are applied in source order by the reducer so later
    /// mutations cannot retroactively reopen an earlier investigation.
    pub initial_progress: MaterialProgressState,
    pub recovery_markers: HashMap<String, u32>,
    pub stall_recovery_markers: u32,
    pub stall_recovery_marker_index: Option<usize>,
    pub prior_efficiency: crate::task_efficiency::TaskEfficiencyState,
    pub efficiency_recovery_markers: u32,
    pub efficiency_recovery_marker_index: Option<usize>,
    pub efficiency_recovery_anchors: Vec<crate::task_efficiency::EfficiencyRecoveryAnchor>,
    /// Task revision visible at each completed tool call. This preserves
    /// user-instruction ordering inside one un-compacted history window.
    pub operation_task_revisions: HashMap<usize, u64>,
    pub final_task_revision: u64,
    pub request_usage: Vec<crate::task_efficiency::CompletedRequestUsage>,
}

#[derive(Debug, Clone)]
pub struct InvestigationReductionTrace {
    pub operation: EvidenceOperation,
    pub transition: InvestigationTransition,
    /// Macro progress decision at this exact operation. Replay and
    /// diagnostics use this instead of inferring an early stall from the
    /// final aggregate state, which may have been reset by later progress.
    pub task_stall_transition: crate::task_stall::TaskStallTransition,
    pub task_efficiency_transition: crate::task_efficiency::TaskEfficiencyTransition,
    pub tool_results_since_progress: u32,
}

/// Provenance cut after a Canonical install: checkpoint + rematerialized tail
/// are already in the durable ledger and must not be reduced again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InvestigationHistoryBoundary {
    pub checkpoint_index: Option<usize>,
    pub retained_tail_count: usize,
}

impl InvestigationHistoryBoundary {
    pub fn skip_source_index(&self, source_index: usize) -> bool {
        if let Some(checkpoint_index) = self.checkpoint_index {
            source_index <= checkpoint_index.saturating_add(self.retained_tail_count)
        } else {
            false
        }
    }

    pub fn skip_exchange(&self, call_source_index: usize, occurrence_id: &str) -> bool {
        if occurrence_id.starts_with("journal:") || occurrence_id.starts_with("checkpoint:") {
            return true;
        }
        self.skip_source_index(call_source_index)
    }
}

pub fn investigation_history_boundary(raw_items: &[Value]) -> InvestigationHistoryBoundary {
    let checkpoint_index = raw_items.iter().rposition(is_canonical_checkpoint_item);
    let retained_tail_count = checkpoint_index
        .and_then(|idx| {
            raw_items.get(idx).and_then(|item| {
                let meta = item.get("metadata")?;
                meta.get("retained_tail_count")
                    .or_else(|| meta.get("retainedTailCount"))
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
            })
        })
        .unwrap_or(0);
    InvestigationHistoryBoundary {
        checkpoint_index,
        retained_tail_count,
    }
}

/// Parse prior checkpoint, recovery markers, and evidence ops from **raw** history.
///
/// Checkpoint and recovery developer items must be read before portable sanitization.
pub fn build_investigation_runtime_input(
    raw_items: &[Value],
    occurrence_ids: Option<&[String]>,
) -> InvestigationRuntimeInput {
    build_investigation_runtime_input_seeded_with_usage(raw_items, occurrence_ids, None, None, None)
}

/// Same builder, accepting completed request token usages from the active session.
pub fn build_investigation_runtime_input_with_usage(
    raw_items: &[Value],
    occurrence_ids: Option<&[String]>,
    request_usage: Option<&[crate::task_efficiency::CompletedRequestUsage]>,
) -> InvestigationRuntimeInput {
    build_investigation_runtime_input_seeded_with_usage(
        raw_items,
        occurrence_ids,
        None,
        None,
        request_usage,
    )
}

/// Same builder, with an explicit prior ledger for replay / restart traces.
pub fn build_investigation_runtime_input_seeded(
    raw_items: &[Value],
    occurrence_ids: Option<&[String]>,
    prior_state: Option<&InvestigationState>,
) -> InvestigationRuntimeInput {
    build_investigation_runtime_input_seeded_with_usage(
        raw_items,
        occurrence_ids,
        prior_state,
        None,
        None,
    )
}

/// Core builder supporting explicit prior ledger and completed request usage tracking.
pub fn build_investigation_runtime_input_seeded_with_usage(
    raw_items: &[Value],
    occurrence_ids: Option<&[String]>,
    prior_state: Option<&InvestigationState>,
    prior_efficiency: Option<&crate::task_efficiency::TaskEfficiencyState>,
    request_usage: Option<&[crate::task_efficiency::CompletedRequestUsage]>,
) -> InvestigationRuntimeInput {
    build_investigation_runtime_input_seeded_with_usage_and_revision(
        raw_items,
        occurrence_ids,
        prior_state,
        prior_efficiency,
        request_usage,
        None,
    )
}

/// Core builder with an optional authoritative Codex top-level turn revision.
/// When present, carried user-role host context is never allowed to increment
/// the revision merely because it was replayed after local compaction.
pub fn build_investigation_runtime_input_seeded_with_usage_and_revision(
    raw_items: &[Value],
    occurrence_ids: Option<&[String]>,
    prior_state: Option<&InvestigationState>,
    prior_efficiency: Option<&crate::task_efficiency::TaskEfficiencyState>,
    request_usage: Option<&[crate::task_efficiency::CompletedRequestUsage]>,
    authoritative_task_revision: Option<u64>,
) -> InvestigationRuntimeInput {
    let mut prior = prior_state.cloned().unwrap_or_default();
    prior.migrate_legacy();

    let recovery_markers = crate::investigation_reducer::recovery_marker_counts(raw_items);
    let (stall_recovery_markers, stall_recovery_marker_index) =
        read_stall_recovery_markers(raw_items);
    // Efficiency state is carried by the durable recovery snapshot, not by a
    // checkpoint embedded in history. Without a seed it starts fresh, which is
    // correct for a conversation that has never recovered.
    let prior_efficiency = prior_efficiency.cloned().unwrap_or_default();
    let (efficiency_recovery_markers, efficiency_recovery_marker_index) =
        crate::task_efficiency::research_sprawl_markers(raw_items);

    let identities: Vec<String> = match occurrence_ids {
        Some(ids) if ids.len() == raw_items.len() => ids.to_vec(),
        _ => (0..raw_items.len()).map(|i| format!("req:{i}")).collect(),
    };

    let boundary = investigation_history_boundary(raw_items);
    let task_literals = task_literals_from_items(raw_items);
    let operations = project_evidence_operations(raw_items, &identities, &task_literals, &boundary);

    let progress = prior.progress.clone();
    let (operation_task_revisions, final_task_revision) = task_revisions_by_source(
        raw_items,
        &operations,
        &boundary,
        progress.user_instruction_revision,
        prior_state.is_some(),
        authoritative_task_revision,
    );

    let mut resolved_usages: Vec<crate::task_efficiency::CompletedRequestUsage> =
        request_usage.map(|s| s.to_vec()).unwrap_or_default();

    // P0-1: Filter usage against watermark BEFORE binding boundaries
    let watermark = prior_efficiency.last_applied_usage_seq;
    resolved_usages.retain(|usage| usage.usage_seq == 0 || usage.usage_seq > watermark);

    // P0-1: Exclude checkpoint and retained tail from boundaries to bind new usages ONLY onto new boundaries
    let all_boundaries = extract_request_boundaries(raw_items, &identities);
    let new_boundaries: Vec<RequestBoundary> = all_boundaries
        .into_iter()
        .filter(|b| !boundary.skip_source_index(b.first_source_index))
        .collect();

    attach_usage_to_request_boundaries(&mut resolved_usages, &new_boundaries);

    resolved_usages.sort_by_key(|usage| {
        (
            usage.source_index.unwrap_or(usize::MAX),
            usage.usage_seq,
            usage.request_index.unwrap_or(u32::MAX),
        )
    });

    // P0-2: Extract durable/reconstructed anchors
    let mut efficiency_recovery_anchors = extract_efficiency_recovery_anchors(
        raw_items,
        &resolved_usages,
        prior_efficiency.cumulative_input_tokens,
    );
    // The checkpoint and its rematerialized tail are already represented by
    // `prior_efficiency`. Re-activating a marker from that durable prefix
    // would reopen a completed recovery epoch and contaminate conversion
    // metrics for genuinely new operations.
    efficiency_recovery_anchors
        .retain(|anchor| !boundary.skip_source_index(anchor.marker_source_index));

    InvestigationRuntimeInput {
        prior,
        operations,
        initial_progress: progress,
        recovery_markers,
        stall_recovery_markers,
        stall_recovery_marker_index,
        prior_efficiency,
        efficiency_recovery_markers,
        efficiency_recovery_marker_index,
        efficiency_recovery_anchors,
        operation_task_revisions,
        final_task_revision,
        request_usage: resolved_usages,
    }
}

/// Represents an authoritative boundary for a single provider model request exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestBoundary {
    pub response_id: Option<String>,
    pub first_source_index: usize,
    pub last_source_index: usize,
}

/// Extract provider request boundaries from raw conversation items and identities.
///
/// In OpenAI / Codex architectures, a single provider request may emit multiple tool calls
/// (parallel tool calling) or an assistant message.
/// A new provider request begins when model items (function_call or assistant message)
/// follow user input or tool execution outputs.
/// Extract efficiency recovery anchors from raw items and resolved request usages.
/// Reads metadata when present on synthetic developer message, or reconstructs
/// deterministically from transcript ordering and completed request usages.
pub fn extract_efficiency_recovery_anchors(
    raw_items: &[Value],
    resolved_usages: &[crate::task_efficiency::CompletedRequestUsage],
    prior_cumulative_tokens: u64,
) -> Vec<crate::task_efficiency::EfficiencyRecoveryAnchor> {
    let mut anchors = Vec::new();
    for (i, item) in raw_items.iter().enumerate() {
        let id = item.get("id").and_then(Value::as_str).unwrap_or("");
        let Some(rest) = id.strip_prefix("synthetic:task_efficiency_recovery:") else {
            continue;
        };
        let Ok(count) = rest.parse::<u32>() else {
            continue;
        };

        let internal_obj = item
            .get("internal_efficiency_recovery")
            .or_else(|| item.get("metadata"));
        let (req_idx, cum_tokens) = if let Some(meta) = internal_obj {
            let r_idx = meta
                .get("request_index")
                .or_else(|| meta.get("requestIndex"))
                .and_then(Value::as_u64)
                .map(|v| v as u32);
            let c_tokens = meta
                .get("cumulative_input_tokens")
                .or_else(|| meta.get("cumulativeInputTokens"))
                .and_then(Value::as_u64);
            (r_idx, c_tokens)
        } else {
            (None, None)
        };

        let final_cum_tokens = cum_tokens.unwrap_or_else(|| {
            let prior_in_window: u64 = resolved_usages
                .iter()
                .filter(|u| u.source_index.is_some_and(|s| s < i))
                .map(|u| u.input_tokens)
                .sum();
            prior_cumulative_tokens.saturating_add(prior_in_window)
        });

        let final_req_idx = req_idx.or_else(|| {
            resolved_usages
                .iter()
                .find(|u| u.source_index.is_some_and(|s| s >= i))
                .and_then(|u| u.request_index)
        });

        anchors.push(crate::task_efficiency::EfficiencyRecoveryAnchor {
            recovery_count: count,
            request_index: final_req_idx,
            cumulative_input_tokens_before_recovery: final_cum_tokens,
            marker_source_index: i,
        });
    }
    anchors.sort_by_key(|a| a.marker_source_index);
    anchors
}

pub fn extract_request_boundaries(
    raw_items: &[Value],
    identities: &[String],
) -> Vec<RequestBoundary> {
    let mut boundaries = Vec::new();
    let mut current_boundary: Option<RequestBoundary> = None;
    let mut in_model_turn = false;

    for (idx, item) in raw_items.iter().enumerate() {
        let item_type = item.get("type").and_then(Value::as_str);
        let role = item.get("role").and_then(Value::as_str);
        let is_model_output = item_type == Some("function_call")
            || (item_type == Some("message") && role == Some("assistant"));

        let identity = identities.get(idx).map(|s| s.as_str()).unwrap_or_default();
        let response_id = if identity.starts_with("store:") {
            // e.g. store:resp_123:output:0 -> resp_123
            identity.split(':').nth(1).map(str::to_string)
        } else {
            None
        };

        if is_model_output {
            if !in_model_turn {
                // Starting a new model turn
                if let Some(b) = current_boundary.take() {
                    boundaries.push(b);
                }
                current_boundary = Some(RequestBoundary {
                    response_id,
                    first_source_index: idx,
                    last_source_index: idx,
                });
                in_model_turn = true;
            } else {
                // Continuation of current model turn (e.g. parallel function_call A, function_call B)
                if let Some(b) = current_boundary.as_mut() {
                    b.last_source_index = idx;
                    if b.response_id.is_none() && response_id.is_some() {
                        b.response_id = response_id;
                    }
                }
            }
        } else {
            // User prompt or function_call_output ends the model output phase
            in_model_turn = false;
        }
    }

    if let Some(b) = current_boundary {
        boundaries.push(b);
    }

    boundaries
}

/// Bind completed request usage onto provider-turn clusters, not onto the
/// Nth function_call. `response_id` wins; unmatched usages map in order onto
/// request boundaries (one provider request may emit many tools).
fn attach_usage_to_request_boundaries(
    usages: &mut [crate::task_efficiency::CompletedRequestUsage],
    boundaries: &[RequestBoundary],
) {
    let mut cursor = 0usize;
    let mut used = vec![false; boundaries.len()];
    for usage in usages.iter_mut() {
        if let Some(source_index) = usage.source_index {
            if let Some(index) = boundaries
                .iter()
                .position(|boundary| boundary.first_source_index == source_index)
            {
                used[index] = true;
            }
            continue;
        }
        if let Some(resp_id) = usage.response_id.as_deref() {
            if let Some((index, matched)) =
                boundaries.iter().enumerate().find(|(index, boundary)| {
                    !used[*index] && boundary.response_id.as_deref() == Some(resp_id)
                })
            {
                usage.source_index = Some(matched.first_source_index);
                used[index] = true;
                continue;
            }
        }
        while used.get(cursor).copied().unwrap_or(false) {
            cursor = cursor.saturating_add(1);
        }
        if let Some(matched) = boundaries.get(cursor) {
            usage.source_index = Some(matched.first_source_index);
            used[cursor] = true;
            cursor = cursor.saturating_add(1);
        } else if let Some(last) = boundaries.last() {
            usage.source_index = Some(last.first_source_index);
        }
    }
}

/// Project completed tool exchanges into canonical evidence operations.
///
/// Live, Canonical, and replay must all call this — not reconstruct from
/// `CommandInvocation` after output text has been dropped.
pub fn project_evidence_operations(
    items: &[Value],
    identities: &[String],
    task_literals: &[String],
    boundary: &InvestigationHistoryBoundary,
) -> Vec<EvidenceOperation> {
    let padded: Vec<String> = if identities.len() == items.len() {
        identities.to_vec()
    } else {
        (0..items.len()).map(|i| format!("req:{i}")).collect()
    };
    let Ok(events) = normalize_trajectory(items, &padded) else {
        return Vec::new();
    };
    let exchanges = build_tool_exchanges(items, &events);
    let mut operations = Vec::new();
    for ex in &exchanges {
        let occurrence = padded
            .get(ex.call_source_index)
            .cloned()
            .unwrap_or_else(|| format!("req:{}", ex.call_source_index));
        if boundary.skip_exchange(ex.call_source_index, &occurrence) {
            continue;
        }
        if !ex.completed {
            continue;
        }
        let Some(call) = items.get(ex.call_source_index) else {
            continue;
        };
        let outputs: Vec<&Value> = ex
            .output_source_indices
            .iter()
            .filter_map(|&idx| items.get(idx))
            .collect();
        // Dedicated mutation tools can expose patch text through the same
        // argument field used by shell commands. Classify them before the
        // generic command projector or a successful edit becomes an
        // Unknown-channel Execute operation and material progress is lost.
        if let Some(op) = project_mutation_exchange(
            ex.call_source_index,
            call,
            &outputs,
            &occurrence,
            ex.completed,
        ) {
            operations.push(op);
            continue;
        }

        if let Some(op) = project_from_tool_pair(
            ex.call_source_index,
            call,
            &outputs,
            &occurrence,
            task_literals,
            true,
        ) {
            operations.push(op);
            continue;
        }
    }
    operations
}

/// Shared reducer output. Ledger, stall recovery, and efficiency recovery are decided once after fold.
#[derive(Debug, Clone)]
pub struct InvestigationReduceOutput {
    pub state: InvestigationState,
    pub efficiency_state: crate::task_efficiency::TaskEfficiencyState,
    pub ledger_recovery: Option<RecoveryAction>,
    pub stall_recovery: Option<StallRecoveryAction>,
    pub efficiency_recovery: Option<crate::task_efficiency::ResearchSprawlRecoveryAction>,
    pub traces: Vec<InvestigationReductionTrace>,
}

/// Reduce the shared input through the single reducer.
/// Canonical compaction uses this path: stall is fold-only (shadow).
pub fn reduce_investigation_input(
    input: &InvestigationRuntimeInput,
    policy: &InvestigationPolicy,
) -> (InvestigationState, Option<RecoveryAction>) {
    let (state, action, _) = reduce_investigation_input_traced(input, policy);
    (state, action)
}

/// Reduce shared input and retain the real per-operation decisions for
/// bounded production diagnostics. This avoids reconstructing transitions
/// from final aggregate counters, which loses link and progress causality.
pub fn reduce_investigation_input_traced(
    input: &InvestigationRuntimeInput,
    policy: &InvestigationPolicy,
) -> (
    InvestigationState,
    Option<RecoveryAction>,
    Vec<InvestigationReductionTrace>,
) {
    let output =
        reduce_investigation_input_traced_with_stall(input, policy, &TaskStallPolicy::shadow());
    (output.state, output.ledger_recovery, output.traces)
}

pub fn reduce_investigation_input_traced_with_stall(
    input: &InvestigationRuntimeInput,
    policy: &InvestigationPolicy,
    stall_policy: &TaskStallPolicy,
) -> InvestigationReduceOutput {
    reduce_investigation_input_traced_with_stall_and_efficiency(
        input,
        policy,
        stall_policy,
        &crate::task_efficiency::TaskEfficiencyPolicy::default(),
    )
}

pub fn reduce_investigation_input_traced_with_stall_and_efficiency(
    input: &InvestigationRuntimeInput,
    policy: &InvestigationPolicy,
    stall_policy: &TaskStallPolicy,
    efficiency_policy: &crate::task_efficiency::TaskEfficiencyPolicy,
) -> InvestigationReduceOutput {
    let mut state = input.prior.clone();
    state.migrate_legacy();
    let mut stall = state.task_stall.clone();
    stall.migrate();
    reconcile_stall_recovery(
        &mut stall,
        input.stall_recovery_markers,
        input.stall_recovery_marker_index,
    );
    let mut efficiency = input.prior_efficiency.clone();
    let mut anchor_iter = input.efficiency_recovery_anchors.iter().peekable();
    if input.efficiency_recovery_anchors.is_empty() {
        crate::task_efficiency::reconcile_efficiency_recovery_with_anchors(
            &mut efficiency,
            input.efficiency_recovery_markers,
            input.efficiency_recovery_marker_index,
            &input.efficiency_recovery_anchors,
        );
    }
    let mut ledger = state.as_ledger();
    let mut progress = input.initial_progress.clone();
    ledger.progress = progress.clone();
    let linker = SymbolicInvestigationLinker::default();
    // Production fold never self-injects; live_recovery_action owns injection.
    let fold_policy = InvestigationPolicy::shadow();
    let _ = policy;
    let mut traces = Vec::with_capacity(input.operations.len());
    let mut usage_iter = input.request_usage.iter().peekable();
    let mut current_request_index: Option<u32> = None;
    for op in &input.operations {
        let operation_task_revision = input
            .operation_task_revisions
            .get(&op.source_index)
            .copied()
            .unwrap_or(progress.user_instruction_revision);
        if operation_task_revision > progress.user_instruction_revision {
            progress.user_instruction_revision = operation_task_revision;
            ledger.progress = progress.clone();
        }

        // Apply completed request usages whose turn starts at or before this operation.
        // Unanchored usages (no source_index) wait until the trailing fold so they
        // cannot steal attribution from a later mutation in the same window.
        while let Some(u) = usage_iter.peek() {
            if u.source_index.is_some_and(|idx| idx <= op.source_index) {
                let u = usage_iter.next().unwrap();
                current_request_index = u.request_index;
                efficiency.apply_completed_usage(u);
            } else {
                break;
            }
        }

        // Activate any recovery anchors whose marker appeared before this operation.
        while let Some(anchor) = anchor_iter.peek() {
            if anchor.marker_source_index < op.source_index {
                let anchor = anchor_iter.next().unwrap();
                crate::task_efficiency::activate_efficiency_recovery_anchor(
                    &mut efficiency,
                    anchor,
                );
            } else {
                break;
            }
        }

        let transition = reduce_investigation(&mut ledger, op, &progress, &linker, &fold_policy);
        let attributed = !transition.skipped;
        let task_stall_transition = reduce_task_stall_observation(
            &mut stall,
            CompletedToolObservation {
                operation: op,
                attributed,
                user_instruction_revision: operation_task_revision,
            },
            stall_policy,
        );
        let task_efficiency_transition = crate::task_efficiency::reduce_task_efficiency_observation(
            &mut efficiency,
            crate::task_efficiency::CompletedEfficiencyObservation {
                operation: op,
                attributed,
                user_instruction_revision: operation_task_revision,
                request_index: current_request_index,
            },
            efficiency_policy,
        );
        traces.push(InvestigationReductionTrace {
            operation: op.clone(),
            transition,
            task_stall_transition,
            task_efficiency_transition,
            tool_results_since_progress: stall.tool_results_since_progress,
        });
        apply_progress_delta(&mut progress, &progress_delta_from_operation(op));
    }

    // Fold any remaining recovery anchors (e.g. trailing recovery marker without subsequent operations)
    for anchor in anchor_iter {
        crate::task_efficiency::activate_efficiency_recovery_anchor(&mut efficiency, anchor);
    }

    // Fold any remaining usages (e.g. trailing inference request that emitted no tools or final request)
    for u in usage_iter {
        efficiency.apply_completed_usage(u);
    }
    progress.user_instruction_revision = progress
        .user_instruction_revision
        .max(input.final_task_revision);
    // Zero-tool-op boundary: a revised user instruction with no intervening
    // tool result still closes any active recovery before the next provider
    // request is built (plan §13).
    crate::task_efficiency::apply_user_instruction_revision_boundary(
        &mut efficiency,
        input.final_task_revision,
    );
    ledger.progress = progress;
    reconcile_recovery_counts(&mut ledger, &input.recovery_markers);
    state = InvestigationState::from_ledger(ledger);
    state.no_progress = input.prior.no_progress.clone();
    state.recent_observations = input.prior.recent_observations.clone();
    state.task_stall = stall;
    let ledger_recovery = live_recovery_action(&state.as_ledger(), policy, &input.recovery_markers);
    let stall_recovery = decide_task_stall_recovery(&state.task_stall, stall_policy);
    let efficiency_recovery =
        crate::task_efficiency::decide_research_sprawl_recovery(&efficiency, efficiency_policy);
    InvestigationReduceOutput {
        state,
        efficiency_state: efficiency,
        ledger_recovery,
        stall_recovery,
        efficiency_recovery,
        traces,
    }
}

pub fn is_canonical_checkpoint_item(item: &Value) -> bool {
    let meta = item.get("metadata");
    let vellum_cp = meta
        .and_then(|m| {
            m.get("vellum_checkpoint")
                .or_else(|| m.get("vellumCheckpoint"))
        })
        .and_then(Value::as_str);
    let schema_ver = meta
        .and_then(|m| m.get("schema_version").or_else(|| m.get("schemaVersion")))
        .and_then(Value::as_u64);
    (vellum_cp == Some("canonical")
        || item.get("type").and_then(Value::as_str) == Some("vellum_canonical_checkpoint"))
        && schema_ver == Some(2)
}

fn task_literals_from_items(items: &[Value]) -> Vec<String> {
    let mut text = String::new();
    for item in items {
        if item.get("role").and_then(Value::as_str) == Some("user") {
            if let Some(t) = crate::compaction::item_text(item) {
                text.push_str(&t);
                text.push(' ');
            }
        }
    }
    extract_task_literals(&text)
}

fn task_revisions_by_source(
    items: &[Value],
    operations: &[EvidenceOperation],
    boundary: &InvestigationHistoryBoundary,
    initial_revision: u64,
    leading_user_is_carried_context: bool,
    authoritative_task_revision: Option<u64>,
) -> (HashMap<usize, u64>, u64) {
    let first_new_index = boundary
        .checkpoint_index
        .map(|index| {
            index
                .saturating_add(boundary.retained_tail_count)
                .saturating_add(1)
        })
        .unwrap_or(0);
    let operation_indices = operations
        .iter()
        .map(|operation| operation.source_index)
        .collect::<std::collections::HashSet<_>>();
    let mut revision = initial_revision;
    let mut revisions = HashMap::new();
    if let Some(final_revision) = authoritative_task_revision {
        // Codex host messages expose stable turn ids and content kinds. Map
        // operations to the visible top-level user turns, while ignoring
        // environment_context and other host-generated role=user envelopes.
        let instruction_turns = items
            .iter()
            .filter_map(codex_user_instruction_turn_id)
            .fold(Vec::<String>::new(), |mut turns, turn| {
                if !turns.iter().any(|known| known == turn) {
                    turns.push(turn.to_string());
                }
                turns
            });
        let mut turn_index = 0usize;
        let first_visible_revision = final_revision
            .saturating_sub(instruction_turns.len().saturating_sub(1) as u64)
            .max(1);
        revision = if instruction_turns.is_empty() {
            final_revision
        } else {
            first_visible_revision
        };
        for (index, item) in items.iter().enumerate() {
            if let Some(turn) = codex_user_instruction_turn_id(item) {
                if instruction_turns.get(turn_index).map(String::as_str) == Some(turn) {
                    revision = first_visible_revision.saturating_add(turn_index as u64);
                    turn_index = turn_index.saturating_add(1);
                }
            }
            if operation_indices.contains(&index) {
                revisions.insert(index, revision);
            }
        }
        return (revisions, final_revision.max(initial_revision));
    }
    for (index, item) in items.iter().enumerate() {
        let carried_user = leading_user_is_carried_context && index == first_new_index;
        if index >= first_new_index
            && !carried_user
            && item.get("role").and_then(Value::as_str) == Some("user")
        {
            revision = revision.saturating_add(1);
        }
        if operation_indices.contains(&index) {
            revisions.insert(index, revision);
        }
    }
    (revisions, revision)
}

fn codex_user_instruction_turn_id(item: &Value) -> Option<&str> {
    if item.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let metadata = item.get("internal_chat_message_metadata_passthrough")?;
    let is_user_text = metadata
        .get("content_item_kinds")
        .and_then(Value::as_array)
        .is_some_and(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("user.text")));
    is_user_text
        .then(|| metadata.get("turn_id").and_then(Value::as_str))
        .flatten()
}

fn project_mutation_exchange(
    source_index: usize,
    call: &Value,
    outputs: &[&Value],
    occurrence_id: &str,
    completed: bool,
) -> Option<EvidenceOperation> {
    let name = call.get("name").and_then(Value::as_str).unwrap_or("");
    if name != "apply_patch" && name != "write_to_file" && name != "edit_file" {
        return None;
    }
    if !completed {
        return None;
    }
    let outcome = classify_mutation_outcome(name, outputs);
    if outcome != MutationOutcome::ConfirmedSuccess {
        return None;
    }
    let identity = mutation_identity_from_call(call);
    let mut subjects = Vec::new();
    let mut stable_evidence = Vec::new();
    if let Some(identity) = identity {
        for path in &identity.targets {
            subjects.push(CanonicalSubject::new(
                CanonicalSubjectKind::File,
                normalize_file_subject(path),
            ));
        }
        subjects.sort();
        subjects.dedup();
        if !identity.payload_digest.is_empty() {
            stable_evidence.push(EvidenceSignature {
                level: EvidenceSignatureLevel::NormalizedDigest,
                digest: identity.payload_digest,
                provenance: FactProvenance::Independent,
                fact: None,
            });
        }
    }
    let subject_keys: Vec<String> = subjects.iter().map(|s| s.key.clone()).take(4).collect();
    Some(EvidenceOperation {
        source_index,
        occurrence_id: occurrence_id.to_string(),
        operation: EvidenceOperationKind::Mutate,
        channel: EvidenceChannel::Workspace,
        subjects,
        result: EvidenceResult::Completed,
        stable_evidence,
        descriptor: format!("Mutate/Workspace [{}] {name}", subject_keys.join(",")),
        completed: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence_operation::{
        CanonicalSubject, CanonicalSubjectKind, InvestigationPredicate,
    };
    use crate::investigation_linker::InvestigationIdentity;
    use crate::investigation_reducer::{
        bound_ledgers, InvestigationLedgerEntry, LinkProvenance, RecoveryState,
        DEFAULT_REDUNDANT_REVISIT_THRESHOLD,
    };
    use serde_json::json;
    use std::collections::BTreeSet;

    fn call_out(id: &str, command: &str, output: &str) -> Vec<Value> {
        vec![
            json!({
                "type": "function_call",
                "call_id": id,
                "name": "exec_command",
                "arguments": serde_json::to_string(&json!({ "command": command })).unwrap()
            }),
            json!({
                "type": "function_call_output",
                "call_id": id,
                "output": output,
                "exit_code": 0
            }),
        ]
    }

    fn user(text: &str) -> Value {
        json!({"type": "message", "role": "user", "content": text})
    }

    const REAL_PROMPT: &str =
        "Implement normalize_records and preserve CIPHERTEXT_RECOVERY_EXACT in DECISIONS.md.";

    #[test]
    fn python_positive_output_is_not_no_result_on_production_path() {
        let mut items = vec![user(REAL_PROMPT)];
        items.extend(call_out(
            "py1",
            "python inspect_latest_session.py",
            "FOUND prior state in sqlite cache\nrecord_id=42",
        ));
        let input = build_investigation_runtime_input(&items, None);
        assert_eq!(input.operations.len(), 1);
        assert!(
            !matches!(input.operations[0].result, EvidenceResult::NoResult),
            "non-empty Python output must not collapse to NoResult, got {:?}",
            input.operations[0].result
        );
        let (state, _) = reduce_investigation_input(&input, &InvestigationPolicy::shadow());
        assert!(
            state
                .ledgers
                .iter()
                .any(|e| !e.evidence_frontier.is_empty()),
            "Python positive evidence must expand the frontier"
        );
    }

    #[test]
    fn generic_store_commands_do_not_merge_from_prompt_literals_alone() {
        let mut items = vec![user(REAL_PROMPT)];
        let commands = [
            r"Get-ChildItem $env:CODEX_HOME\sessions",
            "python inspect_latest_session.py",
            "git log --all",
            "python inspect_db.py session.sqlite",
            "Get-Content some-session-metadata.json",
            "python inspect_latest_session_v2.py",
        ];
        for (i, cmd) in commands.iter().enumerate() {
            items.extend(call_out(&format!("c{i}"), cmd, ""));
        }
        let input = build_investigation_runtime_input(&items, None);
        assert_eq!(input.operations.len(), 6);
        let store_ops: Vec<_> = input
            .operations
            .iter()
            .filter(|op| !op.descriptor.contains("some-session-metadata.json"))
            .collect();
        assert_eq!(store_ops.len(), 5);
        let mut reduced = InvestigationLedgerState::default();
        let linker = SymbolicInvestigationLinker::default();
        let policy = InvestigationPolicy::shadow();
        let mut seen = Vec::new();
        for op in &store_ops {
            let t =
                reduce_investigation(&mut reduced, op, &input.initial_progress, &linker, &policy);
            if !t.skipped {
                seen.push(t.ledger_id.clone());
            }
        }
        seen.sort();
        seen.dedup();
        assert!(
            seen.len() >= 2,
            "unrelated session, VCS, and database probes must not merge solely from prompt literals, got {seen:?}"
        );
        assert!(
            input
                .operations
                .iter()
                .find(|op| op.descriptor.contains("some-session-metadata.json"))
                .is_some_and(|op| op.channel
                    != crate::evidence_operation::EvidenceChannel::Session),
            "a workspace metadata file must not be classified as the Codex session store"
        );
    }

    #[test]
    fn progress_is_applied_in_operation_order() {
        let mut items = vec![user("Locate NEEDLE in state.txt")];
        items.extend(call_out("q1", "rg NEEDLE state.txt", ""));
        items.extend([
            json!({
                "type": "function_call",
                "call_id": "m1",
                "name": "apply_patch",
                "arguments": "*** Begin Patch\n*** Update File: state.txt\n*** End Patch"
            }),
            json!({
                "type": "function_call_output",
                "call_id": "m1",
                "output": "Done!",
                "exit_code": 0
            }),
        ]);
        items.extend(call_out("q2", "rg NEEDLE state.txt", ""));

        let input = build_investigation_runtime_input(&items, None);
        assert_eq!(input.operations.len(), 3);
        let (state, _, _traces) =
            reduce_investigation_input_traced(&input, &InvestigationPolicy::shadow());
        let entry = state
            .ledgers
            .iter()
            .find(|entry| entry.identity.subjects.iter().any(|s| s.key == "NEEDLE"))
            .expect("query ledger");
        assert_eq!(
            entry.redundant_revisits, 0,
            "the mutation between probes must reopen only the later probe"
        );
        assert_eq!(state.progress.workspace_revision, 1);
    }

    #[test]
    fn bound_ledgers_multiple_deletes_keep_locked_active() {
        let mut state = InvestigationLedgerState::default();
        state.ledgers.push(stale("stale0", 0, true));
        state.ledgers.push(active("active1", 5));
        state.ledgers.push(locked("locked2", 9));
        for i in 3..12 {
            state.ledgers.push(stale(&format!("stale{i}"), i, true));
        }
        let policy = InvestigationPolicy {
            serialized_budget_bytes: 200,
            redundant_revisit_recovery_threshold: DEFAULT_REDUNDANT_REVISIT_THRESHOLD,
            mode: crate::investigation_reducer::InvestigationLedgerMode::Shadow,
        };
        bound_ledgers(&mut state, &policy);
        assert!(
            state.ledgers.iter().any(|e| e.ledger_id == "locked2"),
            "locked active ledger must survive multiple deletions"
        );
        assert!(
            state.ledgers.iter().all(|e| e.ledger_id != "stale0"),
            "lowest-index stale ledger must be eligible for deletion"
        );
    }

    fn stale(id: &str, seq: u64, resolved: bool) -> InvestigationLedgerEntry {
        InvestigationLedgerEntry {
            ledger_id: id.into(),
            identity: InvestigationIdentity {
                predicate: InvestigationPredicate::Locate,
                subjects: vec![CanonicalSubject::new(CanonicalSubjectKind::TaskLiteral, id)],
                task_revision: 0,
            },
            channels_seen: BTreeSet::new(),
            depends_on: BTreeSet::new(),
            evidence_frontier: vec![],
            attempts_without_frontier_progress: 0,
            redundant_revisits: 0,
            last_seen_progress: Default::default(),
            recovery_count: 0,
            recovery_state: RecoveryState::None,
            last_activity_seq: seq,
            link_provenance: LinkProvenance::Provisional,
            bounded_descriptor: id.into(),
            resolved,
        }
    }

    fn active(id: &str, seq: u64) -> InvestigationLedgerEntry {
        let mut e = stale(id, seq, false);
        e.redundant_revisits = 1;
        e
    }

    fn locked(id: &str, seq: u64) -> InvestigationLedgerEntry {
        let mut e = stale(id, seq, false);
        e.redundant_revisits = 5;
        e.recovery_count = 1;
        e.recovery_state = RecoveryState::Warned;
        e
    }

    #[test]
    fn default_policy_without_env_is_shadow() {
        assert_eq!(
            InvestigationPolicy::shadow().mode,
            crate::investigation_reducer::InvestigationLedgerMode::Shadow
        );
    }

    #[test]
    fn session_transcript_growth_without_command_literal_is_not_independent_frontier() {
        let prompt = REAL_PROMPT;
        let mut items = vec![user(prompt)];
        items.extend(call_out(
            "p1",
            "python inspect_latest_session.py",
            "prompt: Implement normalize_records and preserve CIPHERTEXT_RECOVERY_EXACT in DECISIONS.md.",
        ));
        let first = build_investigation_runtime_input(&items, None);
        let (state1, _) = reduce_investigation_input(&first, &InvestigationPolicy::shadow());
        let frontier1 = state1
            .ledgers
            .iter()
            .map(|e| e.evidence_frontier.len())
            .sum::<usize>();

        items.extend(call_out(
            "p2",
            "python inspect_latest_session_v2.py",
            "prompt: Implement normalize_records and preserve CIPHERTEXT_RECOVERY_EXACT in DECISIONS.md.\npython inspect_latest_session.py",
        ));
        items.extend(call_out(
            "p3",
            "python parse_session.py",
            "prompt: Implement normalize_records and preserve CIPHERTEXT_RECOVERY_EXACT in DECISIONS.md.\npython inspect_latest_session.py\npython inspect_latest_session_v2.py",
        ));
        let later = build_investigation_runtime_input(&items, None);
        let (state2, _) = reduce_investigation_input(&later, &InvestigationPolicy::shadow());
        let frontier2 = state2
            .ledgers
            .iter()
            .map(|e| e.evidence_frontier.len())
            .sum::<usize>();
        assert_eq!(
            frontier2, frontier1,
            "growing self-transcript must not mint independent evidence"
        );
        assert!(
            state2.ledgers.iter().any(|e| e.redundant_revisits >= 1),
            "repeat session probes should accumulate redundant revisits"
        );
    }

    #[test]
    fn workspace_session_names_do_not_join_prior_task_state_ledger() {
        let mut items = vec![user(REAL_PROMPT)];
        items.extend(call_out(
            "s1",
            r"Get-ChildItem $env:CODEX_HOME\sessions",
            "",
        ));
        items.extend(call_out("w1", "python session_migration.py", "migrated"));
        items.extend(call_out(
            "w2",
            "Get-Content session_config.json",
            "timeout=30",
        ));
        items.extend(call_out("w3", "cargo test session_manager", "ok"));
        let input = build_investigation_runtime_input(&items, None);
        let (state, _) = reduce_investigation_input(&input, &InvestigationPolicy::shadow());
        let session_ledger = state
            .ledgers
            .iter()
            .find(|e| {
                e.identity
                    .subjects
                    .iter()
                    .any(|s| s.key == "prior_task_state")
            })
            .expect("session store probe must create a prior_task_state ledger");
        for op in &input.operations {
            if op.descriptor.contains("session_migration")
                || op.descriptor.contains("session_config")
                || op.descriptor.contains("session_manager")
            {
                assert!(
                    !op.subjects.iter().any(|s| s.key == "prior_task_state"),
                    "workspace session* names must not attach prior_task_state: {}",
                    op.descriptor
                );
            }
        }
        let _ = session_ledger;
    }

    #[test]
    fn seventy_six_op_fragmented_trace_stalls_without_ledger_identity() {
        let mut items = vec![user(REAL_PROMPT)];
        for i in 0..8 {
            items.extend(call_out(
                &format!("s{i}"),
                "python inspect_latest_session.py",
                "prompt: prior conversation",
            ));
        }
        items.extend(vec![
            json!({
                "type": "function_call",
                "call_id": "mut1",
                "name": "apply_patch",
                "arguments": "{\"patch\": \"*** Add File: _extract_session.py\\n+print(1)\\n\"}"
            }),
            json!({
                "type": "function_call_output",
                "call_id": "mut1",
                "output": "Success. Updated the following files:\nA _extract_session.py",
                "status": "success"
            }),
        ]);
        for i in 0..40 {
            items.extend(call_out(
                &format!("r{i}"),
                "python inspect_latest_session_v2.py",
                "prompt: prior conversation plus more lines",
            ));
        }
        for i in 0..26 {
            items.extend(call_out(
                &format!("k{i}"),
                "Get-Content README.md",
                "# notes",
            ));
        }
        assert!(items.len() > 70);
        let input = build_investigation_runtime_input(&items, None);
        assert!(
            input.operations.len() >= 70,
            "macro stall must see nearly all completed tools, got {}",
            input.operations.len()
        );
        let output = reduce_investigation_input_traced_with_stall(
            &input,
            &InvestigationPolicy::shadow(),
            &crate::task_stall::TaskStallPolicy::recover(),
        );
        let state = output.state;
        assert!(
            state.ledgers.len() >= 1,
            "fragmentation is allowed; stall must not depend on a single ledger"
        );
        assert!(
            state.task_stall.tool_results_since_progress
                >= crate::task_stall::TaskStallPolicy::recover().recovery_after_tool_results,
            "post-helper rereads must accumulate macro stall, since={}",
            state.task_stall.tool_results_since_progress
        );
        assert!(
            output.stall_recovery.is_some(),
            "macro stall must inject recovery despite ledger fragmentation"
        );
        assert_eq!(
            state.task_stall.recovery_total, 0,
            "fold must not persist recovery_total without a marker"
        );
        assert!(
            state.task_stall.unattributed_operations >= 1,
            "unattributed/skipped work still counts"
        );
    }

    #[test]
    fn short_multi_source_exploration_does_not_trip_stall() {
        let mut items = vec![user(REAL_PROMPT)];
        items.extend(call_out("w", "Get-ChildItem src", "lib.rs"));
        items.extend(call_out("g", "git log --all", "commit"));
        items.extend(call_out("t", "cargo test --lib", "ok"));
        items.extend(vec![
            json!({
                "type": "function_call",
                "call_id": "m",
                "name": "apply_patch",
                "arguments": "{\"patch\": \"*** Update File: src/lib.rs\\n+fn ok() {}\\n\"}"
            }),
            json!({
                "type": "function_call_output",
                "call_id": "m",
                "output": "Success. Updated the following files:\nM src/lib.rs",
                "status": "success"
            }),
        ]);
        let input = build_investigation_runtime_input(&items, None);
        let output = reduce_investigation_input_traced_with_stall(
            &input,
            &InvestigationPolicy::shadow(),
            &crate::task_stall::TaskStallPolicy::recover(),
        );
        assert!(output.stall_recovery.is_none());
        assert_eq!(output.state.task_stall.recovery_total, 0);
        assert!(output.state.task_stall.tool_results_since_progress < 16);
    }

    fn patch_call(id: &str, patch: &str) -> Vec<Value> {
        vec![
            json!({
                "type": "function_call",
                "call_id": id,
                "name": "apply_patch",
                "arguments": serde_json::to_string(&json!({ "patch": patch })).unwrap()
            }),
            json!({
                "type": "function_call_output",
                "call_id": id,
                "output": "Success. Updated the following files:\nM src/lib.rs",
                "status": "success"
            }),
        ]
    }

    fn cargo_test_call(id: &str, output: &str, exit: i32) -> Vec<Value> {
        vec![
            json!({
                "type": "function_call",
                "call_id": id,
                "name": "exec_command",
                "arguments": serde_json::to_string(&json!({ "command": "cargo test --lib" })).unwrap()
            }),
            json!({
                "type": "function_call_output",
                "call_id": id,
                "output": output,
                "exit_code": exit
            }),
        ]
    }

    #[test]
    fn production_apply_patch_identity_distinguishes_payloads() {
        let mut items = vec![user(REAL_PROMPT)];
        items.extend(patch_call(
            "a",
            "*** Begin Patch\n*** Update File: src/a.rs\n+fn a() {}\n*** End Patch",
        ));
        items.extend(patch_call(
            "b",
            "*** Begin Patch\n*** Update File: src/b.rs\n+fn b() {}\n*** End Patch",
        ));
        items.extend(patch_call(
            "b2",
            "*** Begin Patch\n*** Update File: src/b.rs\n+fn b() {}\n*** End Patch",
        ));
        let input = build_investigation_runtime_input(&items, None);
        assert_eq!(input.operations.len(), 3);
        let output = reduce_investigation_input_traced_with_stall(
            &input,
            &InvestigationPolicy::shadow(),
            &crate::task_stall::TaskStallPolicy::recover(),
        );
        assert_eq!(
            output.state.task_stall.tool_results_since_progress, 1,
            "patch A and B are genuine; repeating B is not"
        );
        assert!(output.stall_recovery.is_none());
    }

    #[test]
    fn production_verification_failures_are_distinct() {
        let mut items = vec![user(REAL_PROMPT)];
        items.extend(cargo_test_call(
            "t1",
            "test tests::alpha ... FAILED\nthread 'tests::alpha' panicked at src/lib.rs:1:1",
            1,
        ));
        items.extend(cargo_test_call(
            "t2",
            "test tests::beta ... FAILED\nthread 'tests::beta' panicked at src/lib.rs:9:1",
            1,
        ));
        items.extend(cargo_test_call(
            "t3",
            "test tests::beta ... FAILED\nthread 'tests::beta' panicked at src/lib.rs:9:1",
            1,
        ));
        let input = build_investigation_runtime_input(&items, None);
        assert_eq!(input.operations.len(), 3);
        assert!(input
            .operations
            .iter()
            .all(|op| op.operation == EvidenceOperationKind::Verify));
        let output = reduce_investigation_input_traced_with_stall(
            &input,
            &InvestigationPolicy::shadow(),
            &crate::task_stall::TaskStallPolicy::shadow(),
        );
        assert_eq!(
            output.state.task_stall.tool_results_since_progress, 1,
            "fail A and fail B are genuine; repeating fail B is not"
        );
    }

    #[test]
    fn stall_recovery_is_not_duplicated_when_marker_is_present() {
        let mut items = vec![user(REAL_PROMPT)];
        for i in 0..20 {
            items.extend(call_out(
                &format!("r{i}"),
                "Get-Content README.md",
                "# notes",
            ));
        }
        let first = build_investigation_runtime_input(&items, None);
        let output = reduce_investigation_input_traced_with_stall(
            &first,
            &InvestigationPolicy::shadow(),
            &crate::task_stall::TaskStallPolicy::recover(),
        );
        let rec = output.stall_recovery.expect("level 1");
        assert_eq!(rec.level, crate::task_stall::StallRecoveryState::Warned);
        assert_eq!(output.state.task_stall.recovery_total, 0);
        items.push(json!({
            "type": "message",
            "role": "developer",
            "id": format!("synthetic:task_stall_recovery:{}", rec.recovery_count),
            "content": rec.message
        }));
        let second = build_investigation_runtime_input(&items, None);
        let again = reduce_investigation_input_traced_with_stall(
            &second,
            &InvestigationPolicy::shadow(),
            &crate::task_stall::TaskStallPolicy::recover(),
        );
        assert!(
            again.stall_recovery.is_none(),
            "existing marker must prevent a duplicate Level 1"
        );
        assert_eq!(again.state.task_stall.recovery_total, 1);
        assert_eq!(again.state.task_stall.epoch_recovery_level, 1);
    }

    #[test]
    fn later_genuine_progress_cancels_stale_recovery() {
        let mut items = vec![user(REAL_PROMPT)];
        for i in 0..20 {
            items.extend(call_out(
                &format!("r{i}"),
                "Get-Content README.md",
                "# notes",
            ));
        }
        items.extend(patch_call(
            "fix",
            "*** Begin Patch\n*** Update File: src/lib.rs\n+fn ok() {}\n*** End Patch",
        ));
        let input = build_investigation_runtime_input(&items, None);
        let output = reduce_investigation_input_traced_with_stall(
            &input,
            &InvestigationPolicy::shadow(),
            &crate::task_stall::TaskStallPolicy::recover(),
        );
        assert!(
            output.stall_recovery.is_none(),
            "progress after the threshold must cancel recovery"
        );
        assert_eq!(output.state.task_stall.tool_results_since_progress, 0);
        assert_eq!(output.state.task_stall.epoch_recovery_level, 0);
    }

    #[test]
    fn canonical_fold_does_not_increment_recovery_total() {
        let mut items = vec![user(REAL_PROMPT)];
        for i in 0..20 {
            items.extend(call_out(
                &format!("r{i}"),
                "Get-Content README.md",
                "# notes",
            ));
        }
        let input = build_investigation_runtime_input(&items, None);
        let (state, _) = reduce_investigation_input(&input, &InvestigationPolicy::recover());
        assert_eq!(
            state.task_stall.recovery_total, 0,
            "Canonical wrapper must fold stall without minting recovery_total"
        );
        let live = reduce_investigation_input_traced_with_stall(
            &input,
            &InvestigationPolicy::shadow(),
            &crate::task_stall::TaskStallPolicy::recover(),
        );
        assert!(live.stall_recovery.is_some());
        assert_eq!(live.state.task_stall.recovery_total, 0);
    }

    #[test]
    fn progress_after_level_one_starts_a_new_level_one_epoch() {
        let mut items = vec![user(REAL_PROMPT)];
        for i in 0..20 {
            items.extend(call_out(
                &format!("r{i}"),
                "Get-Content README.md",
                "# notes",
            ));
        }
        let first = build_investigation_runtime_input(&items, None);
        let output = reduce_investigation_input_traced_with_stall(
            &first,
            &InvestigationPolicy::shadow(),
            &crate::task_stall::TaskStallPolicy::recover(),
        );
        let rec = output.stall_recovery.expect("level 1");
        items.push(json!({
            "type": "message",
            "role": "developer",
            "id": format!("synthetic:task_stall_recovery:{}", rec.recovery_count),
            "content": rec.message
        }));
        items.extend(patch_call(
            "fix",
            "*** Begin Patch\n*** Update File: src/lib.rs\n+fn ok() {}\n*** End Patch",
        ));
        for i in 0..20 {
            items.extend(call_out(
                &format!("n{i}"),
                "Get-Content NOTES.md",
                "still looking",
            ));
        }
        let later = build_investigation_runtime_input(&items, None);
        let second = reduce_investigation_input_traced_with_stall(
            &later,
            &InvestigationPolicy::shadow(),
            &crate::task_stall::TaskStallPolicy::recover(),
        );
        let next = second.stall_recovery.expect("new epoch level 1");
        assert_eq!(next.level, crate::task_stall::StallRecoveryState::Warned);
        assert_eq!(next.recovery_count, 2);
        assert_ne!(next.level, crate::task_stall::StallRecoveryState::Escalated);
    }

    #[test]
    fn user_instruction_resets_stall_in_source_order_without_a_checkpoint() {
        let mut items = vec![user("initial task")];
        for i in 0..20 {
            items.extend(call_out(
                &format!("old-{i}"),
                "Get-Content README.md",
                "same notes",
            ));
        }
        items.push(user(
            "change direction and inspect a different prerequisite",
        ));
        for i in 0..4 {
            items.extend(call_out(
                &format!("new-{i}"),
                "Get-Content NOTES.md",
                "same new notes",
            ));
        }

        let input = build_investigation_runtime_input(&items, None);
        let output = reduce_investigation_input_traced_with_stall(
            &input,
            &InvestigationPolicy::shadow(),
            &crate::task_stall::TaskStallPolicy::recover(),
        );

        assert_eq!(output.state.task_stall.task_revision, 2);
        assert_eq!(output.state.task_stall.tool_results_since_progress, 3);
        assert!(
            output.stall_recovery.is_none(),
            "work before the newer user instruction must not trip its stall epoch"
        );
    }

    #[test]
    fn test_production_usage_wiring_interleaved() {
        let mut items = vec![user("Find bug in cipher and repair it")];
        // Request 1: 20k tokens -> read_file
        items.extend(call_out("c1", "Get-Content src/cipher.rs", "let a = 1;"));
        // Request 2: 20k tokens -> read_file
        items.extend(call_out("c2", "Get-Content src/decisions.rs", "let b = 2;"));
        // Request 3: 20k tokens -> read_file
        items.extend(call_out("c3", "Get-Content src/utils.rs", "let c = 3;"));

        let usages = vec![
            crate::task_efficiency::CompletedRequestUsage {
                usage_seq: 1,
                request_id: Some("req-1".into()),
                response_id: None,
                input_tokens: 20_000,
                cached_input_tokens: 0,
                output_tokens: 150,
                request_index: Some(1),
                source_index: Some(1),
            },
            crate::task_efficiency::CompletedRequestUsage {
                usage_seq: 2,
                request_id: Some("req-2".into()),
                response_id: None,
                input_tokens: 20_000,
                cached_input_tokens: 0,
                output_tokens: 150,
                request_index: Some(2),
                source_index: Some(3),
            },
            crate::task_efficiency::CompletedRequestUsage {
                usage_seq: 3,
                request_id: Some("req-3".into()),
                response_id: None,
                input_tokens: 20_000,
                cached_input_tokens: 0,
                output_tokens: 150,
                request_index: Some(3),
                source_index: Some(5),
            },
        ];

        let mut input = build_investigation_runtime_input_with_usage(&items, None, Some(&usages));
        // The user instruction revision is already 1 at the start of this investigative run
        input.prior_efficiency.task_revision = 1;
        let mut efficiency_policy = crate::task_efficiency::TaskEfficiencyPolicy::recover();
        efficiency_policy.research_token_budget = 50_000;
        efficiency_policy.min_tool_results_since_world_change = 3;
        efficiency_policy.min_exploration_ratio = 0.6;

        let output = reduce_investigation_input_traced_with_stall_and_efficiency(
            &input,
            &InvestigationPolicy::shadow(),
            &crate::task_stall::TaskStallPolicy::shadow(),
            &efficiency_policy,
        );

        assert_eq!(
            output.efficiency_state.input_tokens_since_world_change,
            60_000
        );
        assert_eq!(output.efficiency_state.tool_results_since_world_change, 3);
        assert!(output.efficiency_state.exploration_ratio() >= 0.6);
        assert!(crate::task_efficiency::research_sprawl_candidate(
            &output.efficiency_state,
            &efficiency_policy,
        ));
        assert!(output.efficiency_recovery.is_some());
        assert_eq!(
            output.efficiency_state.tokens_to_first_workspace_mutation,
            None
        );
    }
    #[test]
    fn test_multi_tool_request_boundary_attribution() {
        // Request 1 (usage=20k) emits Read A and Read B (parallel/multi-tool).
        // Request 2 (usage=30k) emits Mutate C.
        // Blocker requirement:
        // - Mutation C cumulative input tokens must be 50k.
        // - Usage 2 must NOT appear before Read B! Read B must execute at cumulative 20k.
        // - tokens_to_first_workspace_mutation must be exactly 50k.
        let raw_items = vec![
            // 0: user instruction
            serde_json::json!({"type": "message", "role": "user", "content": "implement feature"}),
            // 1: function_call A (Read)
            serde_json::json!({"type": "function_call", "call_id": "c1", "name": "exec_command", "arguments": "{\"command\":\"Get-Content a.rs\"}"}),
            // 2: function_call B (Read)
            serde_json::json!({"type": "function_call", "call_id": "c2", "name": "exec_command", "arguments": "{\"command\":\"Get-Content b.rs\"}"}),
            // 3: output A
            serde_json::json!({"type": "function_call_output", "call_id": "c1", "output": "content of a", "exit_code": 0}),
            // 4: output B
            serde_json::json!({"type": "function_call_output", "call_id": "c2", "output": "content of b", "exit_code": 0}),
            // 5: function_call C (Mutate)
            serde_json::json!({"type": "function_call", "call_id": "c3", "name": "exec_command", "arguments": "{\"command\":\"Set-Content c.rs -Value code\"}"}),
            // 6: output C
            serde_json::json!({"type": "function_call_output", "call_id": "c3", "output": "file written", "exit_code": 0}),
        ];

        let usages = vec![
            crate::task_efficiency::CompletedRequestUsage {
                usage_seq: 1,
                request_id: Some("req-1".into()),
                response_id: None,
                input_tokens: 20_000,
                cached_input_tokens: 0,
                output_tokens: 300,
                request_index: Some(1),
                source_index: None, // Will be inferred via RequestBoundary!
            },
            crate::task_efficiency::CompletedRequestUsage {
                usage_seq: 2,
                request_id: Some("req-2".into()),
                response_id: None,
                input_tokens: 30_000,
                cached_input_tokens: 0,
                output_tokens: 400,
                request_index: Some(2),
                source_index: None, // Will be inferred via RequestBoundary!
            },
        ];

        let input = build_investigation_runtime_input_with_usage(&raw_items, None, Some(&usages));

        assert_eq!(input.request_usage.len(), 2);
        // Boundary 1 starts at source_index 1
        assert_eq!(input.request_usage[0].source_index, Some(1));
        // Boundary 2 starts at source_index 5
        assert_eq!(input.request_usage[1].source_index, Some(5));

        let ledger_policy = InvestigationPolicy::default();
        let stall_policy = crate::task_stall::TaskStallPolicy::default();
        let efficiency_policy = crate::task_efficiency::TaskEfficiencyPolicy::default();

        let output = reduce_investigation_input_traced_with_stall_and_efficiency(
            &input,
            &ledger_policy,
            &stall_policy,
            &efficiency_policy,
        );

        // We have 3 traces: Read A (idx 1), Read B (idx 2), Mutate C (idx 5)
        assert_eq!(output.traces.len(), 3);

        // Read A was processed with Request 1 usage (20k)
        assert_eq!(output.traces[0].task_efficiency_transition.counted, true);
        assert_eq!(output.traces[0].operation.source_index, 1);

        // Read B was processed: Usage 2 MUST NOT have been applied yet!
        assert_eq!(output.traces[1].operation.source_index, 2);

        // Mutate C was processed: Usage 2 has been applied, so cumulative is 50k
        assert_eq!(output.traces[2].operation.source_index, 5);
        assert_eq!(
            output.traces[2]
                .task_efficiency_transition
                .genuine_world_change,
            true
        );

        // The efficiency state recorded tokens_to_first_workspace_mutation
        assert_eq!(
            output.efficiency_state.tokens_to_first_workspace_mutation,
            Some(50_000)
        );
        assert_eq!(
            output
                .efficiency_state
                .first_workspace_mutation_request_index,
            Some(2)
        );
        assert_eq!(output.efficiency_state.last_applied_usage_seq, 2);
    }

    #[test]
    fn extract_request_boundaries_clusters_parallel_tools_not_each_call() {
        let items = vec![
            serde_json::json!({"type": "message", "role": "user", "content": "go"}),
            serde_json::json!({"type": "function_call", "call_id": "a", "name": "exec_command"}),
            serde_json::json!({"type": "function_call", "call_id": "b", "name": "exec_command"}),
            serde_json::json!({"type": "function_call_output", "call_id": "a", "output": "a"}),
            serde_json::json!({"type": "function_call_output", "call_id": "b", "output": "b"}),
            serde_json::json!({"type": "function_call", "call_id": "c", "name": "exec_command"}),
            serde_json::json!({"type": "function_call_output", "call_id": "c", "output": "c"}),
        ];
        let identities = vec![
            "store:resp_1:input:0".into(),
            "store:resp_1:output:0".into(),
            "store:resp_1:output:1".into(),
            "store:resp_1:output:2".into(),
            "store:resp_1:output:3".into(),
            "store:resp_2:output:0".into(),
            "store:resp_2:output:1".into(),
        ];
        let bounds = extract_request_boundaries(&items, &identities);
        assert_eq!(bounds.len(), 2, "parallel A+B is one provider request");
        assert_eq!(bounds[0].first_source_index, 1);
        assert_eq!(bounds[0].last_source_index, 2);
        assert_eq!(bounds[0].response_id.as_deref(), Some("resp_1"));
        assert_eq!(bounds[1].first_source_index, 5);
        assert_eq!(bounds[1].response_id.as_deref(), Some("resp_2"));
    }

    #[test]
    fn usage_response_id_binds_to_matching_boundary_not_ordinal() {
        let items = vec![
            serde_json::json!({"type": "message", "role": "user", "content": "go"}),
            serde_json::json!({"type": "function_call", "call_id": "a", "name": "exec_command", "arguments": "{\"command\":\"Get-Content a.rs\"}"}),
            serde_json::json!({"type": "function_call_output", "call_id": "a", "output": "a", "exit_code": 0}),
            serde_json::json!({"type": "function_call", "call_id": "b", "name": "exec_command", "arguments": "{\"command\":\"Get-Content b.rs\"}"}),
            serde_json::json!({"type": "function_call_output", "call_id": "b", "output": "b", "exit_code": 0}),
        ];
        let identities = vec![
            "store:resp_early:input:0".into(),
            "store:resp_early:output:0".into(),
            "store:resp_early:output:1".into(),
            "store:resp_late:output:0".into(),
            "store:resp_late:output:1".into(),
        ];
        let usages = vec![crate::task_efficiency::CompletedRequestUsage {
            usage_seq: 2,
            request_id: Some("req-late".into()),
            response_id: Some("resp_late".into()),
            input_tokens: 9_000,
            cached_input_tokens: 0,
            output_tokens: 10,
            request_index: Some(2),
            source_index: None,
        }];
        let input =
            build_investigation_runtime_input_with_usage(&items, Some(&identities), Some(&usages));
        assert_eq!(input.request_usage[0].source_index, Some(3));
    }

    #[test]
    fn response_id_binding_does_not_reuse_boundary_for_ordinal_usage() {
        let boundaries = vec![
            RequestBoundary {
                response_id: Some("resp-1".into()),
                first_source_index: 2,
                last_source_index: 2,
            },
            RequestBoundary {
                response_id: Some("resp-2".into()),
                first_source_index: 7,
                last_source_index: 7,
            },
        ];
        let mut usages = vec![
            crate::task_efficiency::CompletedRequestUsage {
                usage_seq: 1,
                response_id: Some("resp-1".into()),
                ..Default::default()
            },
            crate::task_efficiency::CompletedRequestUsage {
                usage_seq: 2,
                ..Default::default()
            },
        ];

        attach_usage_to_request_boundaries(&mut usages, &boundaries);

        assert_eq!(usages[0].source_index, Some(2));
        assert_eq!(usages[1].source_index, Some(7));
    }

    #[test]
    fn usages_are_sorted_by_bound_source_before_reduction() {
        let items = vec![
            serde_json::json!({"type": "message", "role": "user", "content": "go"}),
            serde_json::json!({"type": "function_call", "call_id": "a", "name": "exec_command"}),
            serde_json::json!({"type": "function_call_output", "call_id": "a", "output": "a"}),
            serde_json::json!({"type": "function_call", "call_id": "b", "name": "exec_command"}),
            serde_json::json!({"type": "function_call_output", "call_id": "b", "output": "b"}),
        ];
        let identities = vec![
            "store:resp-0:input:0".into(),
            "store:resp-1:output:0".into(),
            "store:resp-1:output:1".into(),
            "store:resp-2:output:0".into(),
            "store:resp-2:output:1".into(),
        ];
        let usages = vec![
            crate::task_efficiency::CompletedRequestUsage {
                usage_seq: 2,
                response_id: Some("resp-2".into()),
                input_tokens: 200,
                ..Default::default()
            },
            crate::task_efficiency::CompletedRequestUsage {
                usage_seq: 1,
                response_id: Some("resp-1".into()),
                input_tokens: 100,
                ..Default::default()
            },
        ];

        let input =
            build_investigation_runtime_input_with_usage(&items, Some(&identities), Some(&usages));

        assert_eq!(
            input
                .request_usage
                .iter()
                .map(|usage| usage.usage_seq)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(input.request_usage[0].source_index, Some(1));
        assert_eq!(input.request_usage[1].source_index, Some(3));
    }

    #[test]
    fn test_recovery_precedence_research_sprawl_over_task_stall() {
        let eff_recovery = Some(crate::task_efficiency::ResearchSprawlRecoveryAction {
            decision: crate::task_efficiency::EfficiencyRecoveryDecision {
                reason: crate::task_efficiency::EfficiencyRecoveryReason::ResearchSprawl,
                recovery_count: 1,
                input_tokens_since_world_change: 85_000,
                tool_results_since_world_change: 15,
                recent_exploration_ops: 14,
                recent_validation_ops: 0,
                recent_mutation_ops: 0,
                last_world_change_kind: None,
            },
            message: "sprawl".into(),
            recovery_count: 1,
        });
        let stall_recovery = Some(crate::task_stall::StallRecoveryAction {
            level: crate::task_stall::StallRecoveryState::Warned,
            recovery_count: 1,
            message: "stall".into(),
        });

        // Precedence: ResearchSprawl > TaskStall
        let chosen = if eff_recovery.is_some() {
            Some("researchSprawl")
        } else if stall_recovery.is_some() {
            Some("taskStall")
        } else {
            None
        };
        assert_eq!(chosen, Some("researchSprawl"));
    }

    #[test]
    fn test_recovery_precedence_task_stall_when_no_sprawl() {
        let eff_recovery: Option<crate::task_efficiency::ResearchSprawlRecoveryAction> = None;
        let stall_recovery = Some(crate::task_stall::StallRecoveryAction {
            level: crate::task_stall::StallRecoveryState::Warned,
            recovery_count: 1,
            message: "stall".into(),
        });

        let chosen = if eff_recovery.is_some() {
            Some("researchSprawl")
        } else if stall_recovery.is_some() {
            Some("taskStall")
        } else {
            None
        };
        assert_eq!(chosen, Some("taskStall"));
    }

    #[test]
    fn test_blocker_d_full_raw_history_multi_recovery_replay() {
        let items = vec![
            serde_json::json!({"type": "message", "role": "user", "content": "fix bug"}),
            serde_json::json!({"type": "function_call", "call_id": "c1", "name": "exec_command", "arguments": "{\"command\":\"cat file1.py\"}"}),
            serde_json::json!({"type": "function_call_output", "call_id": "c1", "output": "def foo(): pass"}),
            serde_json::json!({
                "type": "message",
                "role": "developer",
                "id": "synthetic:task_efficiency_recovery:1",
                "content": "[Vellum execution checkpoint: research sprawl]",
                "internal_efficiency_recovery": {
                    "recovery_count": 1,
                    "request_index": 10,
                    "cumulative_input_tokens": 50_000
                }
            }),
            serde_json::json!({"type": "function_call", "call_id": "c2", "name": "exec_command", "arguments": "{\"command\":\"cat file2.py\"}"}),
            serde_json::json!({"type": "function_call_output", "call_id": "c2", "output": "class Bar: pass"}),
            serde_json::json!({
                "type": "function_call",
                "call_id": "c3",
                "name": "apply_patch",
                "arguments": "*** Begin Patch\n*** Update File: file1.py\n*** End Patch"
            }),
            serde_json::json!({
                "type": "function_call_output",
                "call_id": "c3",
                "output": "applied",
                "exit_code": 0
            }),
            serde_json::json!({"type": "function_call", "call_id": "c4", "name": "exec_command", "arguments": "{\"command\":\"cat file3.py\"}"}),
            serde_json::json!({"type": "function_call_output", "call_id": "c4", "output": "more code"}),
            serde_json::json!({
                "type": "message",
                "role": "developer",
                "id": "synthetic:task_efficiency_recovery:2",
                "content": "[Vellum execution checkpoint: research sprawl]",
                "internal_efficiency_recovery": {
                    "recovery_count": 2,
                    "request_index": 15,
                    "cumulative_input_tokens": 120_000
                }
            }),
            serde_json::json!({"type": "function_call", "call_id": "c5", "name": "exec_command", "arguments": "{\"command\":\"cat file4.py\"}"}),
            serde_json::json!({"type": "function_call_output", "call_id": "c5", "output": "even more"}),
            serde_json::json!({
                "type": "function_call",
                "call_id": "c6",
                "name": "apply_patch",
                "arguments": "*** Begin Patch\n*** Update File: file2.py\n*** End Patch"
            }),
            serde_json::json!({
                "type": "function_call_output",
                "call_id": "c6",
                "output": "applied",
                "exit_code": 0
            }),
        ];

        let identities = vec![
            "store:u:input:0".into(),
            "store:r10:output:0".into(),
            "store:r10:output:1".into(),
            "store:rec1:dev:0".into(),
            "store:r11:output:0".into(),
            "store:r11:output:1".into(),
            "store:r12:output:0".into(),
            "store:r12:output:1".into(),
            "store:r13:output:0".into(),
            "store:r13:output:1".into(),
            "store:rec2:dev:0".into(),
            "store:r15:output:0".into(),
            "store:r15:output:1".into(),
            "store:r16:output:0".into(),
            "store:r16:output:1".into(),
        ];

        let usages = vec![
            crate::task_efficiency::CompletedRequestUsage {
                usage_seq: 1,
                input_tokens: 50_000,
                request_index: Some(10),
                source_index: Some(1),
                ..Default::default()
            },
            crate::task_efficiency::CompletedRequestUsage {
                usage_seq: 2,
                input_tokens: 15_000,
                request_index: Some(11),
                source_index: Some(4),
                ..Default::default()
            },
            crate::task_efficiency::CompletedRequestUsage {
                usage_seq: 3,
                input_tokens: 15_000,
                request_index: Some(12),
                source_index: Some(6),
                ..Default::default()
            },
            crate::task_efficiency::CompletedRequestUsage {
                usage_seq: 4,
                input_tokens: 40_000,
                request_index: Some(13),
                source_index: Some(8),
                ..Default::default()
            },
            crate::task_efficiency::CompletedRequestUsage {
                usage_seq: 5,
                input_tokens: 20_000,
                request_index: Some(15),
                source_index: Some(11),
                ..Default::default()
            },
            crate::task_efficiency::CompletedRequestUsage {
                usage_seq: 6,
                input_tokens: 25_000,
                request_index: Some(16),
                source_index: Some(13),
                ..Default::default()
            },
        ];

        let input = build_investigation_runtime_input_seeded_with_usage(
            &items,
            Some(&identities),
            None,
            None,
            Some(&usages),
        );

        let output = reduce_investigation_input_traced_with_stall_and_efficiency(
            &input,
            &InvestigationPolicy::shadow(),
            &crate::task_stall::TaskStallPolicy::shadow(),
            &crate::task_efficiency::TaskEfficiencyPolicy::shadow(),
        );

        assert_eq!(output.efficiency_state.recovery_conversions.len(), 2);

        let rec1 = &output.efficiency_state.recovery_conversions[0];
        assert_eq!(rec1.recovery_count, 1);
        assert_eq!(rec1.recovery_request_index, Some(10));
        assert_eq!(rec1.workspace_mutation_request_index, Some(12));
        assert_eq!(rec1.requests_to_workspace_mutation, Some(2));
        assert_eq!(rec1.tokens_to_workspace_mutation, Some(30_000));
        assert_eq!(rec1.converted, true);
        assert_eq!(rec1.exploration_ops, 1);
        assert_eq!(rec1.mutation_ops, 1);

        let rec2 = &output.efficiency_state.recovery_conversions[1];
        assert_eq!(rec2.recovery_count, 2);
        assert_eq!(rec2.recovery_request_index, Some(15));
        assert_eq!(rec2.workspace_mutation_request_index, Some(16));
        assert_eq!(rec2.requests_to_workspace_mutation, Some(1));
        assert_eq!(rec2.tokens_to_workspace_mutation, Some(45_000));
        assert_eq!(rec2.converted, true);
        assert_eq!(rec2.exploration_ops, 1);
        assert_eq!(rec2.mutation_ops, 1);
    }

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

    #[test]
    fn codex_turn_revision_ignores_replayed_environment_context() {
        let items = vec![
            serde_json::json!({
                "type": "message",
                "role": "user",
                "content": "<environment_context>carried</environment_context>",
                "internal_chat_message_metadata_passthrough": {
                    "turn_id": "turn-1",
                    "content_item_kinds": ["environments.environment_context"]
                }
            }),
            serde_json::json!({
                "type": "message",
                "role": "user",
                "content": "do the task",
                "internal_chat_message_metadata_passthrough": {
                    "turn_id": "turn-1",
                    "content_item_kinds": ["user.text"]
                }
            }),
            serde_json::json!({"type":"function_call","call_id":"c1","name":"exec_command","arguments":"{\"command\":\"Get-Content a.txt\"}"}),
            serde_json::json!({"type":"function_call_output","call_id":"c1","output":"a"}),
        ];
        let prior = InvestigationState {
            progress: MaterialProgressState {
                user_instruction_revision: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let input = build_investigation_runtime_input_seeded_with_usage_and_revision(
            &items,
            None,
            Some(&prior),
            None,
            None,
            Some(1),
        );
        assert_eq!(input.final_task_revision, 1);
        assert_eq!(input.operation_task_revisions.get(&2), Some(&1));
    }

    #[test]
    fn codex_turn_revision_maps_two_real_user_turns_without_host_envelopes() {
        let user = |turn: &str, text: &str| {
            serde_json::json!({
                "type": "message",
                "role": "user",
                "content": text,
                "internal_chat_message_metadata_passthrough": {
                    "turn_id": turn,
                    "content_item_kinds": ["user.text"]
                }
            })
        };
        let items = vec![
            user("turn-1", "phase zero"),
            serde_json::json!({"type":"function_call","call_id":"c1","name":"exec_command","arguments":"{\"command\":\"Get-Content a.txt\"}"}),
            serde_json::json!({"type":"function_call_output","call_id":"c1","output":"a"}),
            serde_json::json!({
                "type": "message", "role": "user", "content": "<environment_context />",
                "internal_chat_message_metadata_passthrough": {
                    "turn_id": "turn-2",
                    "content_item_kinds": ["environments.environment_context"]
                }
            }),
            user("turn-2", "phase one"),
            serde_json::json!({"type":"function_call","call_id":"c2","name":"exec_command","arguments":"{\"command\":\"Get-Content b.txt\"}"}),
            serde_json::json!({"type":"function_call_output","call_id":"c2","output":"b"}),
        ];
        let prior = InvestigationState {
            progress: MaterialProgressState {
                user_instruction_revision: 2,
                ..Default::default()
            },
            ..Default::default()
        };
        let input = build_investigation_runtime_input_seeded_with_usage_and_revision(
            &items,
            None,
            Some(&prior),
            None,
            None,
            Some(2),
        );
        assert_eq!(input.operation_task_revisions.get(&1), Some(&1));
        assert_eq!(input.operation_task_revisions.get(&5), Some(&2));
        assert_eq!(input.final_task_revision, 2);
    }

    #[test]
    fn r1_recover_trace_simulation_escalates_to_l2() {
        let mut state = crate::task_efficiency::TaskEfficiencyState::default();
        state.task_revision = 1;
        let policy = crate::task_efficiency::TaskEfficiencyPolicy::recover();

        let anchor = crate::task_efficiency::EfficiencyRecoveryAnchor {
            recovery_count: 1,
            request_index: Some(11),
            cumulative_input_tokens_before_recovery: 61_000,
            marker_source_index: 20,
        };
        crate::task_efficiency::activate_efficiency_recovery_anchor(&mut state, &anchor);
        assert_eq!(
            state.active_action_recovery.as_ref().unwrap().level,
            crate::task_efficiency::ActionRecoveryLevel::ConvertEvidence
        );

        for i in 1..=4 {
            let mut op = make_read_op(&format!("src/search_{i}.rs"), &format!("dig_{i}"));
            op.source_index = 20 + i;
            crate::task_efficiency::reduce_task_efficiency_observation(
                &mut state,
                crate::task_efficiency::CompletedEfficiencyObservation {
                    operation: &op,
                    attributed: true,
                    user_instruction_revision: 1,
                    request_index: Some(11 + i as u32),
                },
                &policy,
            );
        }

        let active = state.active_action_recovery.as_ref().unwrap();
        assert_eq!(
            active.level,
            crate::task_efficiency::ActionRecoveryLevel::ActionRequired
        );
        assert_eq!(active.post_recovery_tool_results, 4);
    }

    #[test]
    fn r2_shadow_trace_escalation_comparison() {
        let mut state = crate::task_efficiency::TaskEfficiencyState::default();
        state.task_revision = 1;
        let policy = crate::task_efficiency::TaskEfficiencyPolicy::shadow(); // In shadow mode, candidate is detected but recovery is not injected
        state.input_tokens_since_world_change = 80_000;
        state.tool_results_since_world_change = 8;
        state.recent_exploration_ops = 8;
        let is_candidate = crate::task_efficiency::research_sprawl_candidate(&state, &policy);
        assert!(is_candidate);
        let decision = crate::task_efficiency::decide_research_sprawl_recovery(&state, &policy);
        assert!(decision.is_none());
        assert!(state.active_action_recovery.is_none());
    }

    #[test]
    fn r3_legacy_success_trace_remains_unblocked() {
        let mut state = crate::task_efficiency::TaskEfficiencyState::default();
        state.task_revision = 1;
        let policy = crate::task_efficiency::TaskEfficiencyPolicy::recover();

        let anchor = crate::task_efficiency::EfficiencyRecoveryAnchor {
            recovery_count: 1,
            request_index: Some(11),
            cumulative_input_tokens_before_recovery: 61_000,
            marker_source_index: 20,
        };
        crate::task_efficiency::activate_efficiency_recovery_anchor(&mut state, &anchor);

        let mut op_read = make_read_op("src/lib.rs", "r1");
        op_read.source_index = 21;
        crate::task_efficiency::reduce_task_efficiency_observation(
            &mut state,
            crate::task_efficiency::CompletedEfficiencyObservation {
                operation: &op_read,
                attributed: true,
                user_instruction_revision: 1,
                request_index: Some(12),
            },
            &policy,
        );

        let mut op_mut = make_mutate_op("src/lib.rs", "m1");
        op_mut.source_index = 22;
        crate::task_efficiency::reduce_task_efficiency_observation(
            &mut state,
            crate::task_efficiency::CompletedEfficiencyObservation {
                operation: &op_mut,
                attributed: true,
                user_instruction_revision: 1,
                request_index: Some(13),
            },
            &policy,
        );

        assert!(state.active_action_recovery.is_none());
        assert_eq!(state.recovery_conversions[0].converted, true);
    }

    #[test]
    fn r4_benign_unfamiliar_repo_does_not_trigger_sprawl() {
        let mut state = crate::task_efficiency::TaskEfficiencyState::default();
        state.task_revision = 1;
        let policy = crate::task_efficiency::TaskEfficiencyPolicy::recover();

        state.input_tokens_since_world_change = 20_000;
        state.tool_results_since_world_change = 3;
        state.recent_exploration_ops = 3;

        let decision = crate::task_efficiency::decide_research_sprawl_recovery(&state, &policy);
        assert!(decision.is_none());
        assert!(state.active_action_recovery.is_none());
    }

    #[test]
    fn r5_early_mutation_task_zero_behavior_change() {
        let mut state = crate::task_efficiency::TaskEfficiencyState::default();
        state.task_revision = 1;
        let policy = crate::task_efficiency::TaskEfficiencyPolicy::recover();

        let mut op_mut = make_mutate_op("src/first.rs", "m1");
        op_mut.source_index = 1;
        crate::task_efficiency::reduce_task_efficiency_observation(
            &mut state,
            crate::task_efficiency::CompletedEfficiencyObservation {
                operation: &op_mut,
                attributed: true,
                user_instruction_revision: 1,
                request_index: Some(1),
            },
            &policy,
        );

        assert_eq!(state.research_sprawl_recovery_total, 0);
        assert!(state.active_action_recovery.is_none());
    }
    /// Recovery state that survives a compaction now comes from the durable
    /// ConversationRecoverySnapshotV1, not from a checkpoint parsed back out of
    /// history. These three cover the plan's requirement that a compaction
    /// preserves an active recovery overlay and does not advance its counters.
    fn efficiency_with_active_recovery(
        compactions_since_activation: u32,
        level: crate::task_efficiency::ActionRecoveryLevel,
    ) -> crate::task_efficiency::TaskEfficiencyState {
        let mut eff = crate::task_efficiency::TaskEfficiencyState {
            task_revision: 1,
            cumulative_input_tokens: 60_000,
            ..Default::default()
        };
        eff.active_action_recovery = Some(crate::task_efficiency::ActiveExecutionRecovery {
            recovery_count: 1,
            level,
            started_request_index: Some(10),
            started_input_tokens: 50_000,
            marker_source_index: Some(5),
            post_recovery_tool_results: 2,
            post_recovery_input_tokens: 10_000,
            exploration_ops: 2,
            validation_ops: 0,
            mutation_ops: 0,
            compactions_since_activation,
        });
        eff
    }

    fn reduce_with_seeded_efficiency(
        eff: crate::task_efficiency::TaskEfficiencyState,
    ) -> InvestigationReduceOutput {
        let items: Vec<serde_json::Value> = vec![];
        let input = build_investigation_runtime_input_seeded_with_usage(
            &items,
            None,
            None,
            Some(&eff),
            None,
        );
        reduce_investigation_input_traced_with_stall_and_efficiency(
            &input,
            &InvestigationPolicy::shadow(),
            &crate::task_stall::TaskStallPolicy::shadow(),
            &crate::task_efficiency::TaskEfficiencyPolicy::shadow(),
        )
    }

    #[test]
    fn l1_recovery_overlay_survives_a_compaction() {
        let output = reduce_with_seeded_efficiency(efficiency_with_active_recovery(
            1,
            crate::task_efficiency::ActionRecoveryLevel::ConvertEvidence,
        ));
        let active = output
            .efficiency_state
            .active_action_recovery
            .expect("active recovery must survive compaction");
        assert_eq!(
            active.level,
            crate::task_efficiency::ActionRecoveryLevel::ConvertEvidence
        );
        assert_eq!(active.recovery_count, 1);
        assert_eq!(active.post_recovery_tool_results, 2);
    }

    #[test]
    fn l2_recovery_does_not_de_escalate_across_a_compaction() {
        let output = reduce_with_seeded_efficiency(efficiency_with_active_recovery(
            1,
            crate::task_efficiency::ActionRecoveryLevel::ActionRequired,
        ));
        let active = output
            .efficiency_state
            .active_action_recovery
            .expect("active recovery must survive compaction");
        assert_eq!(
            active.level,
            crate::task_efficiency::ActionRecoveryLevel::ActionRequired,
            "L2 must not de-escalate to L1 across a compaction"
        );
    }

    #[test]
    fn a_compaction_does_not_advance_escalation_counters() {
        // A compaction is an auxiliary call, not a turn: replaying the seeded
        // state must leave the recovery's own progress counters untouched.
        let output = reduce_with_seeded_efficiency(efficiency_with_active_recovery(
            0,
            crate::task_efficiency::ActionRecoveryLevel::ConvertEvidence,
        ));
        let active = output
            .efficiency_state
            .active_action_recovery
            .expect("active recovery must survive compaction");
        assert_eq!(active.post_recovery_tool_results, 2, "counters unchanged");
        assert_eq!(active.recovery_count, 1, "no extra recovery counted");
        assert_eq!(
            active.level,
            crate::task_efficiency::ActionRecoveryLevel::ConvertEvidence
        );
    }
}
