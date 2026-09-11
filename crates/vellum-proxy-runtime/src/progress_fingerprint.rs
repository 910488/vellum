//! Stable progress identities for TaskStallGuard.
//!
//! Fingerprints must not include source_index or timestamps, or every exchange
//! would look like new progress.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::evidence_operation::{
    EvidenceChannel, EvidenceOperation, EvidenceOperationKind, EvidenceResult, FactProvenance,
};

fn sha256_hex(payload: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(payload.as_bytes()))
}

use crate::task_efficiency::EfficiencyProgressAxis;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProgressKind {
    UserInstruction,
    WorkspaceMutation,
    VerificationState,
    IndependentEvidence,
}

pub fn efficiency_axis(kind: ProgressKind) -> EfficiencyProgressAxis {
    match kind {
        ProgressKind::UserInstruction | ProgressKind::WorkspaceMutation => {
            EfficiencyProgressAxis::WorldChange
        }
        ProgressKind::VerificationState => EfficiencyProgressAxis::ValidationChange,
        ProgressKind::IndependentEvidence => EfficiencyProgressAxis::EvidenceGain,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressFingerprint {
    pub kind: ProgressKind,
    pub digest: String,
}

impl ProgressFingerprint {
    pub fn new(kind: ProgressKind, material: &str) -> Self {
        Self {
            kind,
            digest: sha256_hex(material),
        }
    }
}

const DEFAULT_SEEN_CAPACITY: usize = 32;

/// Bounded tracker so A→B→A→B cannot reset stall forever.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeenProgressTracker {
    #[serde(default)]
    recent: Vec<ProgressFingerprint>,
    #[serde(default = "default_seen_capacity")]
    capacity: usize,
}

fn default_seen_capacity() -> usize {
    DEFAULT_SEEN_CAPACITY
}

impl SeenProgressTracker {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            recent: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn is_genuine_advance(&mut self, fp: &ProgressFingerprint) -> bool {
        if self.recent.iter().any(|seen| seen == fp) {
            return false;
        }
        self.recent.push(fp.clone());
        self.trim_to_capacity();
        true
    }

    pub fn capacity(&self) -> usize {
        self.capacity.max(1)
    }

    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        self.trim_to_capacity();
    }

    fn trim_to_capacity(&mut self) {
        let cap = self.capacity.max(1);
        if self.recent.len() > cap {
            let drop = self.recent.len() - cap;
            self.recent.drain(0..drop);
        }
    }
}

/// Derive candidate progress fingerprints from one completed tool observation.
/// Session / self-generated conversation never yields IndependentEvidence.
pub fn fingerprints_from_operation(operation: &EvidenceOperation) -> Vec<ProgressFingerprint> {
    let mut out = Vec::new();
    match operation.operation {
        EvidenceOperationKind::Mutate
            if matches!(
                operation.channel,
                EvidenceChannel::Workspace | EvidenceChannel::Vcs
            ) =>
        {
            let payload = stable_digest(operation);
            let identity = if payload.is_empty() {
                command_identity(&operation.descriptor).to_string()
            } else {
                payload
            };
            let material = format!(
                "mutate|{:?}|{:?}|{}|{}",
                operation.channel,
                operation.result,
                subject_keys(operation),
                identity
            );
            out.push(ProgressFingerprint::new(
                ProgressKind::WorkspaceMutation,
                &material,
            ));
        }
        EvidenceOperationKind::Verify => {
            let material = format!(
                "verify|{:?}|{}|{}|{}",
                operation.result,
                subject_keys(operation),
                command_identity(&operation.descriptor),
                stable_digest(operation)
            );
            out.push(ProgressFingerprint::new(
                ProgressKind::VerificationState,
                &material,
            ));
        }
        _ => {}
    }

    if operation.channel != EvidenceChannel::Session {
        for signature in &operation.stable_evidence {
            if signature.provenance != FactProvenance::Independent {
                continue;
            }
            if matches!(operation.result, EvidenceResult::Error { .. }) {
                continue;
            }
            out.push(ProgressFingerprint::new(
                ProgressKind::IndependentEvidence,
                &signature.digest,
            ));
        }
    }
    out.sort_by(|a, b| a.digest.cmp(&b.digest));
    out.dedup();
    out
}

fn command_identity(descriptor: &str) -> &str {
    descriptor
        .split(']')
        .nth(1)
        .map(str::trim)
        .unwrap_or(descriptor)
}

fn subject_keys(operation: &EvidenceOperation) -> String {
    operation
        .subjects
        .iter()
        .map(|s| s.key.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn stable_digest(operation: &EvidenceOperation) -> String {
    operation
        .stable_evidence
        .iter()
        .map(|signature| signature.digest.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence_operation::{
        CanonicalSubject, CanonicalSubjectKind, EvidenceSignature, EvidenceSignatureLevel,
    };

    fn mutate(path: &str) -> EvidenceOperation {
        EvidenceOperation {
            source_index: 1,
            occurrence_id: "occ".into(),
            operation: EvidenceOperationKind::Mutate,
            channel: EvidenceChannel::Workspace,
            subjects: vec![CanonicalSubject::new(CanonicalSubjectKind::File, path)],
            result: EvidenceResult::Completed,
            stable_evidence: vec![],
            descriptor: format!("Mutate/Workspace [{path}] apply_patch {path}"),
            completed: true,
        }
    }

    #[test]
    fn identical_patch_is_not_a_second_genuine_advance() {
        let mut seen = SeenProgressTracker::with_capacity(8);
        let fps = fingerprints_from_operation(&mutate("_extract_session.py"));
        assert_eq!(fps.len(), 1);
        assert!(seen.is_genuine_advance(&fps[0]));
        assert!(!seen.is_genuine_advance(&fps[0]));
    }

    #[test]
    fn abab_cycle_does_not_reset_forever() {
        let mut seen = SeenProgressTracker::with_capacity(8);
        let a = fingerprints_from_operation(&mutate("a.py"));
        let b = fingerprints_from_operation(&mutate("b.py"));
        assert!(seen.is_genuine_advance(&a[0]));
        assert!(seen.is_genuine_advance(&b[0]));
        assert!(!seen.is_genuine_advance(&a[0]));
        assert!(!seen.is_genuine_advance(&b[0]));
    }

    fn verify_fail(digest: &str) -> EvidenceOperation {
        EvidenceOperation {
            source_index: 1,
            occurrence_id: "occ".into(),
            operation: EvidenceOperationKind::Verify,
            channel: EvidenceChannel::Workspace,
            subjects: vec![CanonicalSubject::new(
                CanonicalSubjectKind::Test,
                "cargo_test",
            )],
            result: EvidenceResult::Error {
                family: crate::tool_output_normalizer::ErrorFamily::Other,
            },
            stable_evidence: vec![EvidenceSignature {
                level: EvidenceSignatureLevel::NormalizedDigest,
                digest: digest.into(),
                provenance: FactProvenance::Independent,
                fact: None,
            }],
            descriptor: "Verify/Workspace [cargo_test] cargo test".into(),
            completed: true,
        }
    }

    #[test]
    fn distinct_verification_failures_are_distinct_progress() {
        let mut seen = SeenProgressTracker::with_capacity(8);
        let fail_a = fingerprints_from_operation(&verify_fail("sha256:fail-a"));
        let fail_b = fingerprints_from_operation(&verify_fail("sha256:fail-b"));
        assert_eq!(fail_a.len(), 1);
        assert_ne!(fail_a[0], fail_b[0]);
        assert!(seen.is_genuine_advance(&fail_a[0]));
        assert!(seen.is_genuine_advance(&fail_b[0]));
        assert!(!seen.is_genuine_advance(&fail_a[0]));
        assert!(!seen.is_genuine_advance(&fail_b[0]));
    }

    #[test]
    fn session_independent_payload_is_not_progress() {
        let op = EvidenceOperation {
            source_index: 2,
            occurrence_id: "occ".into(),
            operation: EvidenceOperationKind::Query,
            channel: EvidenceChannel::Session,
            subjects: vec![],
            result: EvidenceResult::Completed,
            stable_evidence: vec![EvidenceSignature {
                level: EvidenceSignatureLevel::NormalizedDigest,
                digest: "sha256:payload".into(),
                provenance: FactProvenance::Independent,
                fact: None,
            }],
            descriptor: "Query/Session [] python inspect.py".into(),
            completed: true,
        };
        assert!(fingerprints_from_operation(&op).is_empty());
    }

    #[test]
    fn efficiency_axis_maps_correctly() {
        assert_eq!(
            efficiency_axis(ProgressKind::UserInstruction),
            EfficiencyProgressAxis::WorldChange
        );
        assert_eq!(
            efficiency_axis(ProgressKind::WorkspaceMutation),
            EfficiencyProgressAxis::WorldChange
        );
        assert_eq!(
            efficiency_axis(ProgressKind::VerificationState),
            EfficiencyProgressAxis::ValidationChange
        );
        assert_eq!(
            efficiency_axis(ProgressKind::IndependentEvidence),
            EfficiencyProgressAxis::EvidenceGain
        );
    }
}
