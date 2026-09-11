//! Bounded investigation reducer diagnostics. Raw tool payloads are never stored.

use serde::{Deserialize, Serialize};

use crate::evidence_operation::{EvidenceChannel, EvidenceOperationKind, EvidenceResult};
use crate::investigation_linker::{InvestigationLinkConfidence, LinkMethod};
use crate::tool_output_normalizer::ErrorFamily;

/// Projection of a single evidence operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationOperationDiagnostic {
    pub operation_kind: EvidenceOperationKind,
    pub channel: EvidenceChannel,
    pub bounded_subjects: Vec<String>,
    pub result_class: String,
    pub stable_evidence_digest: Option<String>,
    pub source_index: usize,
}

/// How an operation was linked to a ledger.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationLinkDiagnostic {
    pub candidate_ledger: Option<String>,
    pub link_method: LinkMethod,
    pub confidence_tier: InvestigationLinkConfidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top2_margin: Option<f32>,
}

/// Reducer transition for one operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationTransitionDiagnostic {
    pub ledger_id: String,
    pub frontier_expanded: bool,
    pub channel_coverage_expanded: bool,
    pub relevant_progress: bool,
    pub redundant_revisit_count: u32,
    pub recovery_state: String,
}

/// Why recovery fired, without embedding sensitive raw output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationRecoveryDiagnostic {
    pub ledger_id: String,
    pub why: String,
    pub known_evidence_signatures: Vec<String>,
    pub missing_state: String,
    pub recovery_count: u32,
    pub post_recovery_repeated: bool,
}

pub fn result_class(result: &EvidenceResult) -> String {
    match result {
        EvidenceResult::Found => "found".into(),
        EvidenceResult::NoResult => "no_result".into(),
        EvidenceResult::Error { family } => format!("error:{family:?}"),
        EvidenceResult::Completed => "completed".into(),
        EvidenceResult::Unknown => "unknown".into(),
    }
}

pub fn error_family_label(family: ErrorFamily) -> String {
    format!("{family:?}")
}
