//! Link an evidence operation to an existing investigation ledger.
//!
//! Linking never decides no-progress, recovery, or abort.

use serde::{Deserialize, Serialize};

use crate::evidence_operation::{
    predicate_for_operation, CanonicalSubject, EvidenceOperation, InvestigationPredicate,
};

/// How an operation was attached to a ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LinkMethod {
    Exact,
    Symbolic,
    SemanticShadow,
    Provisional,
    LegacyLiteral,
}

/// Confidence of a link. Semantic matches never exceed High in stage 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum InvestigationLinkConfidence {
    None,
    Low,
    Medium,
    High,
    Exact,
}

/// Identity of an investigation. Scope and progress are not part of identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationIdentity {
    pub predicate: InvestigationPredicate,
    pub subjects: Vec<CanonicalSubject>,
    pub task_revision: u64,
}

impl InvestigationIdentity {
    pub fn from_operation(operation: &EvidenceOperation, task_revision: u64) -> Self {
        let mut subjects = operation.subjects.clone();
        subjects.sort();
        subjects.dedup();
        Self {
            predicate: predicate_for_operation(operation.operation, descriptor_command(operation)),
            subjects,
            task_revision,
        }
    }
}

fn descriptor_command(operation: &EvidenceOperation) -> &str {
    operation
        .descriptor
        .split(']')
        .nth(1)
        .map(str::trim)
        .unwrap_or(operation.descriptor.as_str())
}

/// Compact view of a ledger used by linkers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerDescriptor {
    pub ledger_id: String,
    pub identity: InvestigationIdentity,
    pub bounded_descriptor: String,
}

/// Ranked candidate from a linker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkCandidate {
    pub ledger_id: String,
    pub confidence: InvestigationLinkConfidence,
    pub method: LinkMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f32>,
}

/// Optional semantic ranker. Stage 1 production default is [`NullSemanticLinker`].
pub trait InvestigationSemanticLinker: Send + Sync {
    fn rank_existing(
        &self,
        operation_descriptor: &str,
        candidates: &[LedgerDescriptor],
    ) -> Result<Vec<LinkCandidate>, String>;
}

/// Production default: never proposes a semantic merge.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSemanticLinker;

impl InvestigationSemanticLinker for NullSemanticLinker {
    fn rank_existing(
        &self,
        _operation_descriptor: &str,
        _candidates: &[LedgerDescriptor],
    ) -> Result<Vec<LinkCandidate>, String> {
        Ok(Vec::new())
    }
}

/// Combined linker: exact/symbolic first, optional semantic shadow afterwards.
pub trait InvestigationLinker: Send + Sync {
    fn link(
        &self,
        operation: &EvidenceOperation,
        identity: &InvestigationIdentity,
        candidates: &[LedgerDescriptor],
    ) -> LinkDecision;
}

/// Outcome of linking. A miss creates a provisional ledger rather than forcing a merge.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkDecision {
    pub ledger_id: Option<String>,
    pub method: LinkMethod,
    pub confidence: InvestigationLinkConfidence,
    pub created: bool,
    /// Semantic-only shadow ranking; never used for correctness in stage 1.
    pub shadow_candidates: Vec<LinkCandidate>,
}

/// Deterministic exact + symbolic linker with an optional shadow semantic backend.
#[derive(Debug)]
pub struct SymbolicInvestigationLinker<S> {
    semantic: S,
}

impl Default for SymbolicInvestigationLinker<NullSemanticLinker> {
    fn default() -> Self {
        Self {
            semantic: NullSemanticLinker,
        }
    }
}

impl<S: InvestigationSemanticLinker> SymbolicInvestigationLinker<S> {
    pub fn new(semantic: S) -> Self {
        Self { semantic }
    }
}

impl<S: InvestigationSemanticLinker> InvestigationLinker for SymbolicInvestigationLinker<S> {
    fn link(
        &self,
        operation: &EvidenceOperation,
        identity: &InvestigationIdentity,
        candidates: &[LedgerDescriptor],
    ) -> LinkDecision {
        if let Some(exact) = candidates
            .iter()
            .find(|c| identities_exact(&c.identity, identity))
        {
            return LinkDecision {
                ledger_id: Some(exact.ledger_id.clone()),
                method: LinkMethod::Exact,
                confidence: InvestigationLinkConfidence::Exact,
                created: false,
                shadow_candidates: shadow_rank(&self.semantic, operation, candidates),
            };
        }
        if let Some(symbolic) = candidates
            .iter()
            .filter(|c| identities_symbolic(&c.identity, identity))
            .max_by_key(|c| subject_overlap(&c.identity.subjects, &identity.subjects))
        {
            return LinkDecision {
                ledger_id: Some(symbolic.ledger_id.clone()),
                method: LinkMethod::Symbolic,
                confidence: InvestigationLinkConfidence::High,
                created: false,
                shadow_candidates: shadow_rank(&self.semantic, operation, candidates),
            };
        }
        LinkDecision {
            ledger_id: None,
            method: LinkMethod::Provisional,
            confidence: InvestigationLinkConfidence::None,
            created: true,
            shadow_candidates: shadow_rank(&self.semantic, operation, candidates),
        }
    }
}

fn shadow_rank<S: InvestigationSemanticLinker>(
    semantic: &S,
    operation: &EvidenceOperation,
    candidates: &[LedgerDescriptor],
) -> Vec<LinkCandidate> {
    semantic
        .rank_existing(&operation.descriptor, candidates)
        .unwrap_or_default()
}

fn identities_exact(a: &InvestigationIdentity, b: &InvestigationIdentity) -> bool {
    a.predicate == b.predicate
        && a.task_revision == b.task_revision
        && a.subjects == b.subjects
        && !a.subjects.is_empty()
}

fn identities_symbolic(a: &InvestigationIdentity, b: &InvestigationIdentity) -> bool {
    a.predicate == b.predicate
        && a.task_revision == b.task_revision
        && subject_overlap(&a.subjects, &b.subjects) >= 1
}

fn subject_overlap(a: &[CanonicalSubject], b: &[CanonicalSubject]) -> usize {
    a.iter().filter(|subject| b.contains(subject)).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence_operation::{
        CanonicalSubjectKind, EvidenceChannel, EvidenceOperationKind, EvidenceResult,
    };

    fn op(predicate_cmd: &str, subjects: Vec<CanonicalSubject>) -> EvidenceOperation {
        EvidenceOperation {
            source_index: 1,
            occurrence_id: "occ".into(),
            operation: EvidenceOperationKind::Query,
            channel: EvidenceChannel::Workspace,
            subjects,
            result: EvidenceResult::NoResult,
            stable_evidence: vec![],
            descriptor: format!("Query/Workspace [] {predicate_cmd}"),
            completed: true,
        }
    }

    #[test]
    fn same_subject_different_predicate_does_not_merge() {
        let file = CanonicalSubject::new(CanonicalSubjectKind::File, "DECISIONS.md");
        let locate = InvestigationIdentity {
            predicate: InvestigationPredicate::Locate,
            subjects: vec![file.clone()],
            task_revision: 0,
        };
        let exists = InvestigationIdentity {
            predicate: InvestigationPredicate::Exists,
            subjects: vec![file],
            task_revision: 0,
        };
        assert!(!identities_exact(&locate, &exists));
        assert!(!identities_symbolic(&locate, &exists));
    }

    #[test]
    fn same_predicate_different_task_does_not_merge() {
        let file = CanonicalSubject::new(CanonicalSubjectKind::TaskLiteral, "NEEDLE");
        let a = InvestigationIdentity {
            predicate: InvestigationPredicate::Locate,
            subjects: vec![file.clone()],
            task_revision: 0,
        };
        let b = InvestigationIdentity {
            predicate: InvestigationPredicate::Locate,
            subjects: vec![file],
            task_revision: 1,
        };
        assert!(!identities_exact(&a, &b));
        assert!(!identities_symbolic(&a, &b));
    }

    #[test]
    fn overlapping_subjects_same_predicate_link_symbolically() {
        let linker = SymbolicInvestigationLinker::default();
        let subjects = vec![CanonicalSubject::new(
            CanonicalSubjectKind::TaskLiteral,
            "NEEDLE",
        )];
        let operation = op("Select-String NEEDLE", subjects.clone());
        let identity = InvestigationIdentity::from_operation(&operation, 0);
        let existing = vec![LedgerDescriptor {
            ledger_id: "inv:1".into(),
            identity: identity.clone(),
            bounded_descriptor: "prior".into(),
        }];
        let decision = linker.link(&operation, &identity, &existing);
        assert_eq!(decision.method, LinkMethod::Exact);
        assert_eq!(decision.ledger_id.as_deref(), Some("inv:1"));
        assert!(!decision.created);
    }

    #[test]
    fn miss_creates_provisional_rather_than_forcing_merge() {
        let linker = SymbolicInvestigationLinker::default();
        let operation = op(
            "rg other",
            vec![CanonicalSubject::new(
                CanonicalSubjectKind::TaskLiteral,
                "OTHER",
            )],
        );
        let identity = InvestigationIdentity::from_operation(&operation, 0);
        let existing = vec![LedgerDescriptor {
            ledger_id: "inv:1".into(),
            identity: InvestigationIdentity {
                predicate: InvestigationPredicate::Locate,
                subjects: vec![CanonicalSubject::new(
                    CanonicalSubjectKind::TaskLiteral,
                    "NEEDLE",
                )],
                task_revision: 0,
            },
            bounded_descriptor: "prior".into(),
        }];
        let decision = linker.link(&operation, &identity, &existing);
        assert!(decision.created);
        assert_eq!(decision.method, LinkMethod::Provisional);
    }
}
