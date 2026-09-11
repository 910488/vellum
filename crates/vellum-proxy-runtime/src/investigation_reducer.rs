//! Single deterministic investigation reducer.
//!
//! Live loop guard and Canonical V2 serialization both consume this module.
//! Semantic linking cannot decide correctness.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::evidence_operation::{
    EvidenceChannel, EvidenceOperation, EvidenceOperationKind, EvidenceResult, EvidenceSignature,
    FactProvenance,
};
use crate::investigation_diagnostics::{
    result_class, InvestigationLinkDiagnostic, InvestigationOperationDiagnostic,
    InvestigationRecoveryDiagnostic, InvestigationTransitionDiagnostic,
};
use crate::investigation_linker::{
    InvestigationIdentity, InvestigationLinkConfidence, InvestigationLinker, LedgerDescriptor,
    LinkMethod,
};

fn sha256_hex(payload: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(payload.as_bytes()))
}

/// Durable material-progress revisions. Session transcript append is not a domain.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaterialProgressState {
    #[serde(default)]
    pub workspace_revision: u64,
    #[serde(default)]
    pub vcs_revision: u64,
    #[serde(default)]
    pub test_revision: u64,
    #[serde(default)]
    pub user_instruction_revision: u64,
    #[serde(default)]
    pub external_revision: u64,
}

/// Incremental progress observed in one operation or window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaterialProgressDelta {
    pub workspace: u64,
    pub vcs: u64,
    pub test: u64,
    pub user_instruction: u64,
    pub external: u64,
}

pub fn progress_delta_from_operation(operation: &EvidenceOperation) -> MaterialProgressDelta {
    let mut delta = MaterialProgressDelta::default();
    match operation.operation {
        EvidenceOperationKind::Mutate => match operation.channel {
            EvidenceChannel::Workspace => delta.workspace = 1,
            EvidenceChannel::Vcs => delta.vcs = 1,
            EvidenceChannel::External => delta.external = 1,
            _ => {}
        },
        EvidenceOperationKind::Verify
            if matches!(
                operation.result,
                EvidenceResult::Completed | EvidenceResult::Found
            ) =>
        {
            delta.test = 1;
        }
        _ => {}
    }
    delta
}

pub fn apply_progress_delta(state: &mut MaterialProgressState, delta: &MaterialProgressDelta) {
    state.workspace_revision = state.workspace_revision.saturating_add(delta.workspace);
    state.vcs_revision = state.vcs_revision.saturating_add(delta.vcs);
    state.test_revision = state.test_revision.saturating_add(delta.test);
    state.user_instruction_revision = state
        .user_instruction_revision
        .saturating_add(delta.user_instruction);
    state.external_revision = state.external_revision.saturating_add(delta.external);
}

impl MaterialProgressState {
    pub fn snapshot_for(&self, channels: &BTreeSet<EvidenceChannel>) -> MaterialProgressSnapshot {
        MaterialProgressSnapshot {
            workspace_revision: self.workspace_revision,
            vcs_revision: if channels.contains(&EvidenceChannel::Vcs) {
                self.vcs_revision
            } else {
                0
            },
            test_revision: self.test_revision,
            user_instruction_revision: self.user_instruction_revision,
            external_revision: if channels.contains(&EvidenceChannel::External) {
                self.external_revision
            } else {
                0
            },
        }
    }

    pub fn relevant_progress_since(
        &self,
        last: &MaterialProgressSnapshot,
        depends_on: &BTreeSet<EvidenceChannel>,
    ) -> bool {
        if self.user_instruction_revision > last.user_instruction_revision {
            return true;
        }
        for channel in depends_on {
            let progressed = match channel {
                EvidenceChannel::Workspace => self.workspace_revision > last.workspace_revision,
                EvidenceChannel::Vcs => self.vcs_revision > last.vcs_revision,
                EvidenceChannel::External => self.external_revision > last.external_revision,
                EvidenceChannel::Session
                | EvidenceChannel::Database
                | EvidenceChannel::Process
                | EvidenceChannel::Unknown => false,
            };
            if progressed {
                return true;
            }
        }
        if depends_on.contains(&EvidenceChannel::Workspace)
            && self.test_revision > last.test_revision
        {
            return true;
        }
        false
    }
}

/// Per-ledger snapshot of the progress domains it last observed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaterialProgressSnapshot {
    #[serde(default)]
    pub workspace_revision: u64,
    #[serde(default)]
    pub vcs_revision: u64,
    #[serde(default)]
    pub test_revision: u64,
    #[serde(default)]
    pub user_instruction_revision: u64,
    #[serde(default)]
    pub external_revision: u64,
}

/// Recovery escalation recorded on the ledger.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RecoveryState {
    #[default]
    None,
    Warned,
    Escalated,
}

impl RecoveryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Warned => "warned",
            Self::Escalated => "escalated",
        }
    }
}

/// How the ledger was first created. Legacy entries cannot hard-recover.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum LinkProvenance {
    Exact,
    Symbolic,
    SemanticShadow,
    LegacyLiteral,
    #[default]
    Provisional,
}

impl From<LinkMethod> for LinkProvenance {
    fn from(method: LinkMethod) -> Self {
        match method {
            LinkMethod::Exact => Self::Exact,
            LinkMethod::Symbolic => Self::Symbolic,
            LinkMethod::SemanticShadow => Self::SemanticShadow,
            LinkMethod::Provisional => Self::Provisional,
            LinkMethod::LegacyLiteral => Self::LegacyLiteral,
        }
    }
}

/// One durable investigation aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationLedgerEntry {
    pub ledger_id: String,
    pub identity: InvestigationIdentity,
    #[serde(default)]
    pub channels_seen: BTreeSet<EvidenceChannel>,
    #[serde(default)]
    pub depends_on: BTreeSet<EvidenceChannel>,
    #[serde(default)]
    pub evidence_frontier: Vec<EvidenceSignature>,
    #[serde(default)]
    pub attempts_without_frontier_progress: u32,
    #[serde(default)]
    pub redundant_revisits: u32,
    #[serde(default)]
    pub last_seen_progress: MaterialProgressSnapshot,
    #[serde(default)]
    pub recovery_count: u32,
    #[serde(default)]
    pub recovery_state: RecoveryState,
    #[serde(default)]
    pub last_activity_seq: u64,
    #[serde(default)]
    pub link_provenance: LinkProvenance,
    #[serde(default)]
    pub bounded_descriptor: String,
    #[serde(default)]
    pub resolved: bool,
}

/// Reducer-owned investigation aggregate stored on the checkpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationLedgerState {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub ledgers: Vec<InvestigationLedgerEntry>,
    #[serde(default)]
    pub progress: MaterialProgressState,
    #[serde(default)]
    pub next_seq: u64,
    #[serde(default)]
    pub task_stall: crate::task_stall::TaskStallState,
}

pub const INVESTIGATION_LEDGER_SCHEMA_VERSION: u32 = 3;
pub const DEFAULT_LEDGER_BYTE_BUDGET: usize = 8_192;
pub const DEFAULT_REDUNDANT_REVISIT_THRESHOLD: u32 = 3;

/// Runtime activation of ledger recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvestigationLedgerMode {
    Off,
    Shadow,
    Recover,
}

impl InvestigationLedgerMode {
    pub fn from_env() -> Self {
        let raw = std::env::var("VELLUM_INVESTIGATION_LEDGER_MODE")
            .or_else(|_| std::env::var("INVESTIGATION_LEDGER_MODE"))
            .unwrap_or_else(|_| "recover".into());
        Self::parse(&raw)
    }

    fn parse(raw: &str) -> Self {
        match raw.to_ascii_lowercase().as_str() {
            "off" => Self::Off,
            "shadow" => Self::Shadow,
            "recover" => Self::Recover,
            _ => Self::Recover,
        }
    }

    pub fn from_env_or_default_shadow() -> Self {
        Self::from_env()
    }

    pub fn injects_recovery(self) -> bool {
        matches!(self, Self::Recover)
    }
}

/// Policy knobs frozen independently of any single benchmark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvestigationPolicy {
    pub redundant_revisit_recovery_threshold: u32,
    pub serialized_budget_bytes: usize,
    pub mode: InvestigationLedgerMode,
}

impl Default for InvestigationPolicy {
    fn default() -> Self {
        Self {
            redundant_revisit_recovery_threshold: DEFAULT_REDUNDANT_REVISIT_THRESHOLD,
            serialized_budget_bytes: DEFAULT_LEDGER_BYTE_BUDGET,
            mode: InvestigationLedgerMode::from_env(),
        }
    }
}

impl InvestigationPolicy {
    pub fn shadow() -> Self {
        Self {
            mode: InvestigationLedgerMode::Shadow,
            ..Self::default_without_env()
        }
    }

    pub fn recover() -> Self {
        Self {
            mode: InvestigationLedgerMode::Recover,
            ..Self::default_without_env()
        }
    }

    fn default_without_env() -> Self {
        Self {
            redundant_revisit_recovery_threshold: DEFAULT_REDUNDANT_REVISIT_THRESHOLD,
            serialized_budget_bytes: DEFAULT_LEDGER_BYTE_BUDGET,
            mode: InvestigationLedgerMode::Shadow,
        }
    }
}

/// Recovery the caller may inject. Stage 1 never aborts from the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryAction {
    pub ledger_id: String,
    pub message: String,
    pub recovery_count: u32,
    pub level: RecoveryState,
}

/// One reducer step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvestigationTransition {
    pub ledger_id: String,
    pub link_method: LinkMethod,
    pub link_confidence: InvestigationLinkConfidence,
    pub created: bool,
    pub frontier_expanded: bool,
    pub coverage_expanded: bool,
    pub relevant_progress: bool,
    pub redundant_revisit: bool,
    pub redundant_revisit_count: u32,
    pub recovery_state: RecoveryState,
    pub recovery: Option<RecoveryAction>,
    pub skipped: bool,
}

/// Apply one completed evidence operation to durable ledger state.
pub fn reduce_investigation(
    state: &mut InvestigationLedgerState,
    operation: &EvidenceOperation,
    progress: &MaterialProgressState,
    linker: &dyn InvestigationLinker,
    policy: &InvestigationPolicy,
) -> InvestigationTransition {
    state.schema_version = INVESTIGATION_LEDGER_SCHEMA_VERSION;
    state.progress = progress.clone();
    state.next_seq = state.next_seq.saturating_add(1);
    let seq = state.next_seq;

    if !operation.completed || !is_investigation_operation(operation) {
        apply_progress_only(state, operation, progress);
        return InvestigationTransition {
            ledger_id: String::new(),
            link_method: LinkMethod::Provisional,
            link_confidence: InvestigationLinkConfidence::None,
            created: false,
            frontier_expanded: false,
            coverage_expanded: false,
            relevant_progress: false,
            redundant_revisit: false,
            redundant_revisit_count: 0,
            recovery_state: RecoveryState::None,
            recovery: None,
            skipped: true,
        };
    }

    let identity =
        InvestigationIdentity::from_operation(operation, progress.user_instruction_revision);
    if identity.subjects.is_empty() {
        apply_progress_only(state, operation, progress);
        return InvestigationTransition {
            ledger_id: String::new(),
            link_method: LinkMethod::Provisional,
            link_confidence: InvestigationLinkConfidence::None,
            created: false,
            frontier_expanded: false,
            coverage_expanded: false,
            relevant_progress: false,
            redundant_revisit: false,
            redundant_revisit_count: 0,
            recovery_state: RecoveryState::None,
            recovery: None,
            skipped: true,
        };
    }

    let candidates: Vec<LedgerDescriptor> = state
        .ledgers
        .iter()
        .map(|entry| LedgerDescriptor {
            ledger_id: entry.ledger_id.clone(),
            identity: entry.identity.clone(),
            bounded_descriptor: entry.bounded_descriptor.clone(),
        })
        .collect();
    let decision = linker.link(operation, &identity, &candidates);

    let ledger_id = if let Some(id) = decision.ledger_id.clone() {
        id
    } else {
        stable_ledger_id(&identity)
    };

    let created = decision.created
        || !state
            .ledgers
            .iter()
            .any(|entry| entry.ledger_id == ledger_id);
    if created {
        state.ledgers.push(InvestigationLedgerEntry {
            ledger_id: ledger_id.clone(),
            identity,
            channels_seen: BTreeSet::new(),
            depends_on: BTreeSet::new(),
            evidence_frontier: Vec::new(),
            attempts_without_frontier_progress: 0,
            redundant_revisits: 0,
            last_seen_progress: progress.snapshot_for(&BTreeSet::new()),
            recovery_count: 0,
            recovery_state: RecoveryState::None,
            last_activity_seq: seq,
            link_provenance: decision.method.into(),
            bounded_descriptor: operation.descriptor.chars().take(160).collect(),
            resolved: false,
        });
    }

    let Some(index) = state
        .ledgers
        .iter()
        .position(|entry| entry.ledger_id == ledger_id)
    else {
        return InvestigationTransition {
            ledger_id,
            link_method: decision.method,
            link_confidence: decision.confidence,
            created,
            frontier_expanded: false,
            coverage_expanded: false,
            relevant_progress: false,
            redundant_revisit: false,
            redundant_revisit_count: 0,
            recovery_state: RecoveryState::None,
            recovery: None,
            skipped: true,
        };
    };

    let (
        frontier_expanded,
        coverage_expanded,
        relevant_progress,
        redundant_revisit,
        redundant_revisit_count,
        recovery_state,
        recovery,
    ) = {
        let entry = &mut state.ledgers[index];
        entry.last_activity_seq = seq;
        entry.depends_on.insert(operation.channel);
        if operation.operation == EvidenceOperationKind::Mutate {
            // Mutations are progress, not investigations.
        }

        let relevant_progress =
            progress.relevant_progress_since(&entry.last_seen_progress, &entry.depends_on);
        if relevant_progress {
            entry.last_seen_progress = progress.snapshot_for(&entry.depends_on);
            if entry.recovery_state != RecoveryState::None {
                entry.recovery_state = RecoveryState::None;
            }
        }

        let coverage_expanded = entry.channels_seen.insert(operation.channel);

        let mut frontier_expanded = false;
        for signature in &operation.stable_evidence {
            if signature.provenance == FactProvenance::SelfGeneratedConversation {
                continue;
            }
            if !entry.evidence_frontier.contains(signature) {
                entry.evidence_frontier.push(signature.clone());
                frontier_expanded = true;
            }
        }

        if can_resolve_ledger(entry.identity.predicate, operation) {
            entry.resolved = true;
            entry.attempts_without_frontier_progress = 0;
        }

        let redundant_revisit = !coverage_expanded
            && !frontier_expanded
            && !relevant_progress
            && !entry.resolved
            && entry.channels_seen.contains(&operation.channel);

        if redundant_revisit {
            entry.redundant_revisits = entry.redundant_revisits.saturating_add(1);
            entry.attempts_without_frontier_progress =
                entry.attempts_without_frontier_progress.saturating_add(1);
        } else if frontier_expanded || coverage_expanded || relevant_progress {
            entry.attempts_without_frontier_progress = 0;
        }

        let recovery = maybe_recovery(entry, policy, redundant_revisit);
        (
            frontier_expanded,
            coverage_expanded,
            relevant_progress,
            redundant_revisit,
            entry.redundant_revisits,
            entry.recovery_state,
            recovery,
        )
    };

    bound_ledgers(state, policy);
    InvestigationTransition {
        ledger_id,
        link_method: decision.method,
        link_confidence: decision.confidence,
        created,
        frontier_expanded,
        coverage_expanded,
        relevant_progress,
        redundant_revisit,
        redundant_revisit_count,
        recovery_state,
        recovery,
        skipped: false,
    }
}

fn maybe_recovery(
    entry: &mut InvestigationLedgerEntry,
    policy: &InvestigationPolicy,
    redundant_revisit: bool,
) -> Option<RecoveryAction> {
    if entry.resolved {
        return None;
    }
    if matches!(entry.link_provenance, LinkProvenance::LegacyLiteral)
        && entry.redundant_revisits == 0
    {
        return None;
    }
    if !redundant_revisit {
        return None;
    }
    if entry.redundant_revisits < policy.redundant_revisit_recovery_threshold {
        return None;
    }
    if !policy.mode.injects_recovery() {
        return None;
    }
    if entry.recovery_count >= 2 {
        return None;
    }

    let level = if entry.recovery_count == 0 {
        RecoveryState::Warned
    } else {
        RecoveryState::Escalated
    };
    entry.recovery_count = entry.recovery_count.saturating_add(1);
    entry.recovery_state = level;
    Some(RecoveryAction {
        ledger_id: entry.ledger_id.clone(),
        message: recovery_message(entry, level),
        recovery_count: entry.recovery_count,
        level,
    })
}

fn recovery_message(entry: &InvestigationLedgerEntry, level: RecoveryState) -> String {
    let known: Vec<String> = entry
        .evidence_frontier
        .iter()
        .filter_map(|sig| sig.fact.clone())
        .take(6)
        .collect();
    let channels: Vec<String> = entry
        .channels_seen
        .iter()
        .map(|ch| format!("{ch:?}"))
        .collect();
    let subjects: Vec<String> = entry
        .identity
        .subjects
        .iter()
        .map(|s| s.key.clone())
        .take(4)
        .collect();
    let header = if level == RecoveryState::Escalated {
        "You returned to the same unresolved investigation after recovery, and the evidence frontier still has not changed."
    } else {
        "You have already investigated the same unresolved question with multiple methods, and there is currently no new independent evidence."
    };
    format!(
        "{header}\n\n\
Known:\n{}\n\n\
Still missing:\n- independent evidence for {}\n\n\
Do not repeat:\n- equivalent investigation of already-explored sources ({})\n\n\
Next action must change:\nevidence source / strategy / task execution",
        if known.is_empty() {
            format!("- explored channels: {}", channels.join(", "))
        } else {
            known
                .iter()
                .map(|line| format!("- {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        },
        subjects.join(", "),
        channels.join(", ")
    )
}

fn apply_progress_only(
    _state: &mut InvestigationLedgerState,
    operation: &EvidenceOperation,
    progress: &MaterialProgressState,
) {
    let _ = (operation, progress);
}

/// Locate/Exists cannot be resolved by session or self-generated conversation.
pub fn can_resolve(
    predicate: crate::evidence_operation::InvestigationPredicate,
    channel: crate::evidence_operation::EvidenceChannel,
    provenance: FactProvenance,
    result: &EvidenceResult,
) -> bool {
    if !matches!(result, EvidenceResult::Found) {
        return false;
    }
    if provenance != FactProvenance::Independent {
        return false;
    }
    if matches!(
        predicate,
        crate::evidence_operation::InvestigationPredicate::Locate
            | crate::evidence_operation::InvestigationPredicate::Exists
    ) {
        if channel == crate::evidence_operation::EvidenceChannel::Session {
            return false;
        }
        if provenance == FactProvenance::SelfGeneratedConversation {
            return false;
        }
    }
    true
}

fn can_resolve_ledger(
    predicate: crate::evidence_operation::InvestigationPredicate,
    operation: &EvidenceOperation,
) -> bool {
    operation.stable_evidence.iter().any(|signature| {
        can_resolve(
            predicate,
            operation.channel,
            signature.provenance,
            &operation.result,
        )
    })
}

fn is_investigation_operation(operation: &EvidenceOperation) -> bool {
    match operation.operation {
        EvidenceOperationKind::Query
        | EvidenceOperationKind::Inspect
        | EvidenceOperationKind::Verify => true,
        EvidenceOperationKind::Execute => !operation.subjects.is_empty(),
        EvidenceOperationKind::Mutate => false,
    }
}

fn stable_ledger_id(identity: &InvestigationIdentity) -> String {
    let mut payload = format!("{:?}|{}", identity.predicate, identity.task_revision);
    for subject in &identity.subjects {
        payload.push('|');
        payload.push_str(&format!("{:?}:{}", subject.kind, subject.key));
    }
    format!("inv:{}", &sha256_hex(&payload)[7..23])
}

/// Deterministic retention: never drop an active looping ledger first.
pub fn bound_ledgers(state: &mut InvestigationLedgerState, policy: &InvestigationPolicy) {
    if state.ledgers.len() <= 1 {
        return;
    }
    let encoded_len = serde_json::to_vec(state)
        .map(|bytes| bytes.len())
        .unwrap_or(0);
    if encoded_len <= policy.serialized_budget_bytes && state.ledgers.len() <= 24 {
        return;
    }

    let mut drop_order: Vec<String> = {
        let mut ranked: Vec<&InvestigationLedgerEntry> = state.ledgers.iter().collect();
        ranked.sort_by_key(|entry| {
            let locked = matches!(
                entry.recovery_state,
                RecoveryState::Warned | RecoveryState::Escalated
            ) || entry.redundant_revisits
                >= policy.redundant_revisit_recovery_threshold;
            let active_unresolved = !entry.resolved;
            (
                if locked { 0u8 } else { 1 },
                if active_unresolved { 0u8 } else { 1 },
                std::cmp::Reverse(entry.last_activity_seq),
                std::cmp::Reverse(entry.redundant_revisits),
                if entry.resolved { 1u8 } else { 0 },
            )
        });
        ranked.into_iter().map(|e| e.ledger_id.clone()).collect()
    };

    while state.ledgers.len() > 1 {
        let encoded_len = serde_json::to_vec(state)
            .map(|bytes| bytes.len())
            .unwrap_or(0);
        if encoded_len <= policy.serialized_budget_bytes && state.ledgers.len() <= 24 {
            break;
        }
        let Some(drop_id) = drop_order.pop() else {
            break;
        };
        let locked = state
            .ledgers
            .iter()
            .find(|e| e.ledger_id == drop_id)
            .is_some_and(|e| {
                matches!(
                    e.recovery_state,
                    RecoveryState::Warned | RecoveryState::Escalated
                )
            });
        if locked {
            continue;
        }
        state.ledgers.retain(|e| e.ledger_id != drop_id);
    }
}

/// Restore recovery_count from previously injected synthetic developer items.
pub fn apply_recovery_markers(state: &mut InvestigationLedgerState, items: &[serde_json::Value]) {
    for item in items {
        let role = item.get("role").and_then(serde_json::Value::as_str);
        if role != Some("developer") {
            continue;
        }
        let id = item
            .get("id")
            .or_else(|| item.get("metadata").and_then(|m| m.get("occurrence_id")))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        // Format: synthetic:investigation_recovery:{ledger_id}:{count}
        let Some(rest) = id.strip_prefix("synthetic:investigation_recovery:") else {
            continue;
        };
        let Some((ledger_id, count_str)) = rest.rsplit_once(':') else {
            continue;
        };
        let Ok(count) = count_str.parse::<u32>() else {
            continue;
        };
        if let Some(entry) = state
            .ledgers
            .iter_mut()
            .find(|entry| entry.ledger_id == ledger_id)
        {
            entry.recovery_count = entry.recovery_count.max(count);
            entry.recovery_state = if count >= 2 {
                RecoveryState::Escalated
            } else {
                RecoveryState::Warned
            };
        }
    }
}

pub fn operation_diagnostic(operation: &EvidenceOperation) -> InvestigationOperationDiagnostic {
    InvestigationOperationDiagnostic {
        operation_kind: operation.operation,
        channel: operation.channel,
        bounded_subjects: operation
            .subjects
            .iter()
            .map(|s| s.key.clone())
            .take(6)
            .collect(),
        result_class: result_class(&operation.result),
        stable_evidence_digest: operation.stable_evidence.first().map(|s| s.digest.clone()),
        source_index: operation.source_index,
    }
}

pub fn link_diagnostic(
    ledger_id: Option<&str>,
    method: LinkMethod,
    confidence: InvestigationLinkConfidence,
) -> InvestigationLinkDiagnostic {
    InvestigationLinkDiagnostic {
        candidate_ledger: ledger_id.map(str::to_string),
        link_method: method,
        confidence_tier: confidence,
        model: None,
        model_version: None,
        similarity: None,
        top2_margin: None,
    }
}

pub fn transition_diagnostic(
    transition: &InvestigationTransition,
    recovery_state: RecoveryState,
    redundant_revisits: u32,
) -> InvestigationTransitionDiagnostic {
    InvestigationTransitionDiagnostic {
        ledger_id: transition.ledger_id.clone(),
        frontier_expanded: transition.frontier_expanded,
        channel_coverage_expanded: transition.coverage_expanded,
        relevant_progress: transition.relevant_progress,
        redundant_revisit_count: redundant_revisits,
        recovery_state: recovery_state.as_str().to_string(),
    }
}

/// Decide live recovery after a full reduce pass. Does not mutate counts until the
/// caller injects a synthetic item; `existing_counts` come from history markers.
pub fn live_recovery_action(
    state: &InvestigationLedgerState,
    policy: &InvestigationPolicy,
    existing_counts: &std::collections::HashMap<String, u32>,
) -> Option<RecoveryAction> {
    if !policy.mode.injects_recovery() {
        return None;
    }
    let mut best: Option<&InvestigationLedgerEntry> = None;
    for entry in &state.ledgers {
        if entry.resolved {
            continue;
        }
        if matches!(entry.link_provenance, LinkProvenance::LegacyLiteral)
            && entry.redundant_revisits < policy.redundant_revisit_recovery_threshold
        {
            continue;
        }
        if entry.redundant_revisits < policy.redundant_revisit_recovery_threshold {
            continue;
        }
        if best.is_none_or(|cur| entry.redundant_revisits > cur.redundant_revisits) {
            best = Some(entry);
        }
    }
    let entry = best?;
    let already = entry
        .recovery_count
        .max(existing_counts.get(&entry.ledger_id).copied().unwrap_or(0));
    if already >= 2 {
        return None;
    }
    let level = if already == 0 {
        RecoveryState::Warned
    } else {
        RecoveryState::Escalated
    };
    Some(RecoveryAction {
        ledger_id: entry.ledger_id.clone(),
        message: recovery_message(entry, level),
        recovery_count: already.saturating_add(1),
        level,
    })
}

pub fn reconcile_recovery_counts(
    state: &mut InvestigationLedgerState,
    markers: &std::collections::HashMap<String, u32>,
) {
    for entry in &mut state.ledgers {
        if let Some(count) = markers.get(&entry.ledger_id) {
            entry.recovery_count = entry.recovery_count.max(*count);
            if entry.recovery_count >= 2 {
                entry.recovery_state = RecoveryState::Escalated;
            } else if entry.recovery_count >= 1 {
                entry.recovery_state = RecoveryState::Warned;
            }
        }
    }
}

pub fn recovery_marker_counts(
    items: &[serde_json::Value],
) -> std::collections::HashMap<String, u32> {
    let mut counts = std::collections::HashMap::new();
    for item in items {
        let id = item
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let Some(rest) = id.strip_prefix("synthetic:investigation_recovery:") else {
            continue;
        };
        let Some((ledger_id, count_str)) = rest.rsplit_once(':') else {
            continue;
        };
        if let Ok(count) = count_str.parse::<u32>() {
            let entry = counts.entry(ledger_id.to_string()).or_insert(0);
            *entry = (*entry).max(count);
        }
    }
    counts
}

pub fn recovery_diagnostic(
    action: &RecoveryAction,
    entry: &InvestigationLedgerEntry,
) -> InvestigationRecoveryDiagnostic {
    InvestigationRecoveryDiagnostic {
        ledger_id: action.ledger_id.clone(),
        why: "redundant_revisit_without_frontier_or_relevant_progress".into(),
        known_evidence_signatures: entry
            .evidence_frontier
            .iter()
            .map(|s| s.digest.clone())
            .take(8)
            .collect(),
        missing_state: "independent evidence still absent".into(),
        recovery_count: action.recovery_count,
        post_recovery_repeated: action.level == RecoveryState::Escalated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence_operation::{
        CanonicalSubject, CanonicalSubjectKind, EvidenceChannel, EvidenceOperationKind,
        EvidenceSignatureLevel,
    };
    use crate::investigation_linker::SymbolicInvestigationLinker;

    fn query(
        channel: EvidenceChannel,
        subject: &str,
        source: usize,
        digest: &str,
    ) -> EvidenceOperation {
        EvidenceOperation {
            source_index: source,
            occurrence_id: format!("occ:{source}"),
            operation: EvidenceOperationKind::Query,
            channel,
            subjects: vec![CanonicalSubject::new(
                CanonicalSubjectKind::TaskLiteral,
                subject,
            )],
            result: EvidenceResult::NoResult,
            stable_evidence: vec![EvidenceSignature {
                level: EvidenceSignatureLevel::StructuredFact,
                digest: digest.into(),
                provenance: FactProvenance::Independent,
                fact: Some(format!("not_found:{subject}:{channel:?}")),
            }],
            descriptor: format!("Query/{channel:?} [{subject}] search {subject}"),
            completed: true,
        }
    }

    #[test]
    fn first_new_channel_is_coverage_not_redundant() {
        let mut state = InvestigationLedgerState::default();
        let linker = SymbolicInvestigationLinker::default();
        let policy = InvestigationPolicy::recover();
        let progress = MaterialProgressState::default();
        let t1 = reduce_investigation(
            &mut state,
            &query(EvidenceChannel::Workspace, "NEEDLE", 1, "d1"),
            &progress,
            &linker,
            &policy,
        );
        assert!(t1.coverage_expanded);
        assert!(!t1.redundant_revisit);
        let t2 = reduce_investigation(
            &mut state,
            &query(EvidenceChannel::Vcs, "NEEDLE", 2, "d1"),
            &progress,
            &linker,
            &policy,
        );
        assert!(t2.coverage_expanded);
        assert!(!t2.redundant_revisit);
        assert_eq!(state.ledgers.len(), 1);
    }

    #[test]
    fn ledger_mode_defaults_to_recover_and_allows_explicit_observation_modes() {
        assert_eq!(
            InvestigationLedgerMode::parse(""),
            InvestigationLedgerMode::Recover
        );
        assert_eq!(
            InvestigationLedgerMode::parse("recover"),
            InvestigationLedgerMode::Recover
        );
        assert_eq!(
            InvestigationLedgerMode::parse("shadow"),
            InvestigationLedgerMode::Shadow
        );
        assert_eq!(
            InvestigationLedgerMode::parse("off"),
            InvestigationLedgerMode::Off
        );
        assert_eq!(
            InvestigationLedgerMode::parse("unexpected"),
            InvestigationLedgerMode::Recover
        );
    }

    #[test]
    fn unrelated_read_does_not_split_or_reset_ledger() {
        let mut state = InvestigationLedgerState::default();
        let linker = SymbolicInvestigationLinker::default();
        let policy = InvestigationPolicy::recover();
        let progress = MaterialProgressState::default();
        reduce_investigation(
            &mut state,
            &query(EvidenceChannel::Workspace, "NEEDLE", 1, "d1"),
            &progress,
            &linker,
            &policy,
        );
        let unrelated = EvidenceOperation {
            source_index: 2,
            occurrence_id: "occ:2".into(),
            operation: EvidenceOperationKind::Inspect,
            channel: EvidenceChannel::Workspace,
            subjects: vec![],
            result: EvidenceResult::Completed,
            stable_evidence: vec![],
            descriptor: "Inspect/Workspace [] cat README.md".into(),
            completed: true,
        };
        let skip = reduce_investigation(&mut state, &unrelated, &progress, &linker, &policy);
        assert!(skip.skipped);
        let again = reduce_investigation(
            &mut state,
            &query(EvidenceChannel::Workspace, "NEEDLE", 3, "d1"),
            &progress,
            &linker,
            &policy,
        );
        assert!(again.redundant_revisit);
        assert_eq!(state.ledgers.len(), 1);
    }

    #[test]
    fn session_self_fact_does_not_expand_frontier() {
        let mut state = InvestigationLedgerState::default();
        let linker = SymbolicInvestigationLinker::default();
        let policy = InvestigationPolicy::recover();
        let progress = MaterialProgressState::default();
        let mut op = query(EvidenceChannel::Session, "NEEDLE", 1, "self1");
        op.stable_evidence[0].provenance = FactProvenance::SelfGeneratedConversation;
        let t = reduce_investigation(&mut state, &op, &progress, &linker, &policy);
        assert!(!t.frontier_expanded);
        assert!(state.ledgers[0].evidence_frontier.is_empty());
    }

    #[test]
    fn unrelated_workspace_edit_does_not_reopen_git_investigation() {
        let mut state = InvestigationLedgerState::default();
        let linker = SymbolicInvestigationLinker::default();
        let policy = InvestigationPolicy::recover();
        let mut progress = MaterialProgressState::default();
        reduce_investigation(
            &mut state,
            &query(EvidenceChannel::Vcs, "NEEDLE", 1, "d1"),
            &progress,
            &linker,
            &policy,
        );
        progress.workspace_revision += 1;
        let t = reduce_investigation(
            &mut state,
            &query(EvidenceChannel::Vcs, "NEEDLE", 2, "d1"),
            &progress,
            &linker,
            &policy,
        );
        assert!(!t.relevant_progress);
        assert!(t.redundant_revisit);
    }

    #[test]
    fn relevant_workspace_progress_allows_reprobe() {
        let mut state = InvestigationLedgerState::default();
        let linker = SymbolicInvestigationLinker::default();
        let policy = InvestigationPolicy::recover();
        let mut progress = MaterialProgressState::default();
        reduce_investigation(
            &mut state,
            &query(EvidenceChannel::Workspace, "NEEDLE", 1, "d1"),
            &progress,
            &linker,
            &policy,
        );
        progress.workspace_revision += 1;
        let t = reduce_investigation(
            &mut state,
            &query(EvidenceChannel::Workspace, "NEEDLE", 2, "d1"),
            &progress,
            &linker,
            &policy,
        );
        assert!(t.relevant_progress);
        assert!(!t.redundant_revisit);
    }

    #[test]
    fn resolved_identity_reuses_its_stable_ledger_id() {
        let mut state = InvestigationLedgerState::default();
        let linker = SymbolicInvestigationLinker::default();
        let policy = InvestigationPolicy::shadow();
        let progress = MaterialProgressState::default();
        let mut found = query(EvidenceChannel::Workspace, "NEEDLE", 1, "found");
        found.result = EvidenceResult::Found;

        let first = reduce_investigation(&mut state, &found, &progress, &linker, &policy);
        assert!(first.created);
        assert!(state.ledgers[0].resolved);

        found.source_index = 2;
        found.occurrence_id = "occ:2".into();
        let second = reduce_investigation(&mut state, &found, &progress, &linker, &policy);
        assert!(!second.created);
        assert_eq!(second.ledger_id, first.ledger_id);
        assert_eq!(
            state.ledgers.len(),
            1,
            "stable ledger IDs must remain unique"
        );
    }

    #[test]
    fn session_found_does_not_resolve_locate_ledger() {
        let mut state = InvestigationLedgerState::default();
        let linker = SymbolicInvestigationLinker::default();
        let policy = InvestigationPolicy::shadow();
        let progress = MaterialProgressState::default();
        let mut found = query(EvidenceChannel::Session, "NEEDLE", 1, "found");
        found.result = EvidenceResult::Found;
        reduce_investigation(&mut state, &found, &progress, &linker, &policy);
        assert!(
            !state.ledgers[0].resolved,
            "session hits must not resolve Locate/Exists"
        );
    }

    #[test]
    fn redundant_revisits_on_seen_channel_trigger_recovery_not_abort() {
        let mut state = InvestigationLedgerState::default();
        let linker = SymbolicInvestigationLinker::default();
        let policy = InvestigationPolicy::recover();
        let progress = MaterialProgressState::default();
        let mut last = None;
        for i in 1..=4 {
            last = Some(reduce_investigation(
                &mut state,
                &query(EvidenceChannel::Workspace, "NEEDLE", i, "d1"),
                &progress,
                &linker,
                &policy,
            ));
        }
        let last = last.unwrap();
        assert!(last.recovery.is_some());
        assert_eq!(last.recovery.as_ref().unwrap().level, RecoveryState::Warned);
        assert_eq!(state.ledgers[0].recovery_count, 1);
    }

    #[test]
    fn error_family_does_not_share_no_result_frontier() {
        let mut state = InvestigationLedgerState::default();
        let linker = SymbolicInvestigationLinker::default();
        let policy = InvestigationPolicy::recover();
        let progress = MaterialProgressState::default();
        reduce_investigation(
            &mut state,
            &query(EvidenceChannel::Workspace, "NEEDLE", 1, "nope"),
            &progress,
            &linker,
            &policy,
        );
        let mut err = query(EvidenceChannel::Workspace, "NEEDLE", 2, "perm");
        err.result = EvidenceResult::Error {
            family: crate::tool_output_normalizer::ErrorFamily::PermissionDenied,
        };
        let t = reduce_investigation(&mut state, &err, &progress, &linker, &policy);
        assert!(t.frontier_expanded);
    }

    #[test]
    fn new_user_instruction_isolates_task_revision() {
        let mut state = InvestigationLedgerState::default();
        let linker = SymbolicInvestigationLinker::default();
        let policy = InvestigationPolicy::recover();
        let mut progress = MaterialProgressState::default();
        reduce_investigation(
            &mut state,
            &query(EvidenceChannel::Workspace, "NEEDLE", 1, "d1"),
            &progress,
            &linker,
            &policy,
        );
        progress.user_instruction_revision += 1;
        reduce_investigation(
            &mut state,
            &query(EvidenceChannel::Workspace, "NEEDLE", 2, "d1"),
            &progress,
            &linker,
            &policy,
        );
        assert_eq!(state.ledgers.len(), 2);
    }

    #[test]
    fn bound_ledgers_keeps_locked_active_entry() {
        let mut state = InvestigationLedgerState::default();
        state.ledgers.push(InvestigationLedgerEntry {
            ledger_id: "locked".into(),
            identity: InvestigationIdentity {
                predicate: crate::evidence_operation::InvestigationPredicate::Locate,
                subjects: vec![CanonicalSubject::new(
                    CanonicalSubjectKind::TaskLiteral,
                    "A",
                )],
                task_revision: 0,
            },
            channels_seen: BTreeSet::from([EvidenceChannel::Workspace]),
            depends_on: BTreeSet::from([EvidenceChannel::Workspace]),
            evidence_frontier: vec![],
            attempts_without_frontier_progress: 5,
            redundant_revisits: 5,
            last_seen_progress: MaterialProgressSnapshot::default(),
            recovery_count: 1,
            recovery_state: RecoveryState::Warned,
            last_activity_seq: 9,
            link_provenance: LinkProvenance::Exact,
            bounded_descriptor: "locked".into(),
            resolved: false,
        });
        for i in 0..20 {
            state.ledgers.push(InvestigationLedgerEntry {
                ledger_id: format!("old{i}"),
                identity: InvestigationIdentity {
                    predicate: crate::evidence_operation::InvestigationPredicate::Locate,
                    subjects: vec![CanonicalSubject::new(
                        CanonicalSubjectKind::TaskLiteral,
                        format!("T{i}"),
                    )],
                    task_revision: 0,
                },
                channels_seen: BTreeSet::new(),
                depends_on: BTreeSet::new(),
                evidence_frontier: vec![],
                attempts_without_frontier_progress: 0,
                redundant_revisits: 0,
                last_seen_progress: MaterialProgressSnapshot::default(),
                recovery_count: 0,
                recovery_state: RecoveryState::None,
                last_activity_seq: i,
                link_provenance: LinkProvenance::Provisional,
                bounded_descriptor: "old".into(),
                resolved: true,
            });
        }
        let policy = InvestigationPolicy {
            serialized_budget_bytes: 64,
            ..InvestigationPolicy::recover()
        };
        bound_ledgers(&mut state, &policy);
        assert!(
            state.ledgers.iter().any(|e| e.ledger_id == "locked"),
            "active looping ledger must be retained"
        );
    }
}
