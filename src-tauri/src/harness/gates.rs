//! Completion and commit acceptance gates.
//!
//! Issue #6 Phase 6. In the observed session the first round finished only the
//! foundation, treated the task as committable, and a later review had to add
//! the runtime gate that was actually blocking. `task.completed` is an event
//! about a tool call; it says nothing about whether the issue is done. These
//! gates make the difference machine-checkable, so a partial result has to be
//! declared partial instead of described as finished.

use super::failure::FailureLedger;
use super::shell::ExecInput;
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateStatus {
    Pending,
    Passed,
    Failed,
}

impl GateStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptanceGate {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<ExecInput>,
    /// Non-command gate: something that must be inspected or verified rather
    /// than executed, e.g. "the issue checklist was re-read".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_check: Option<String>,
    pub required: bool,
    pub status: GateStatus,
}

impl AcceptanceGate {
    pub fn command(id: &str, command: ExecInput, required: bool) -> Self {
        Self {
            id: id.into(),
            command: Some(command),
            artifact_check: None,
            required,
            status: GateStatus::Pending,
        }
    }

    pub fn artifact(id: &str, artifact_check: &str, required: bool) -> Self {
        Self {
            id: id.into(),
            command: None,
            artifact_check: Some(artifact_check.into()),
            required,
            status: GateStatus::Pending,
        }
    }
}

/// Whether the work may be described as complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionVerdict {
    Complete,
    /// Something required has not passed. Both lists are reported to the model
    /// verbatim so its final message can be accurate about what is missing.
    Partial {
        pending: Vec<String>,
        failed: Vec<String>,
    },
}

impl CompletionVerdict {
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }

    /// The only commit scope allowed right now.
    pub fn commit_scope(&self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial { .. } => "partial",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CompletionLedger {
    gates: Vec<AcceptanceGate>,
}

impl CompletionLedger {
    pub fn new(gates: Vec<AcceptanceGate>) -> Self {
        Self { gates }
    }

    /// The minimum bar from the issue's Definition of Done. Callers add
    /// task-specific gates on top; they never remove these.
    pub fn default_gates() -> Self {
        Self::new(vec![
            AcceptanceGate::artifact(
                "issue-checklist-reviewed",
                "Every checklist item in the issue was re-read and each is either done or explicitly reported as not done",
                true,
            ),
            AcceptanceGate::artifact(
                "diff-inspected",
                "`git diff` (and `git status`) were read in full for the changes being claimed",
                true,
            ),
            AcceptanceGate::artifact("typecheck-passed", "Type checking ran and passed", true),
            AcceptanceGate::artifact("tests-passed", "The test suite ran and passed", true),
            AcceptanceGate::artifact("build-passed", "The build ran and passed", true),
            AcceptanceGate::artifact(
                "static-copy-scan-passed",
                "No hardcoded user-facing copy or stray static strings were introduced",
                true,
            ),
            AcceptanceGate::artifact(
                "no-unintended-files",
                "No temporary, generated, or scratch files remain in the working tree",
                true,
            ),
            AcceptanceGate::artifact(
                "no-pending-phase",
                "No required phase of the requested work is still outstanding",
                true,
            ),
        ])
    }

    pub fn gates(&self) -> &[AcceptanceGate] {
        &self.gates
    }

    pub fn add(&mut self, gate: AcceptanceGate) {
        self.gates.push(gate);
    }

    pub fn mark(&mut self, id: &str, status: GateStatus) -> bool {
        match self.gates.iter_mut().find(|gate| gate.id == id) {
            Some(gate) => {
                gate.status = status;
                true
            }
            None => false,
        }
    }

    pub fn verdict(&self) -> CompletionVerdict {
        let pending = self
            .gates
            .iter()
            .filter(|gate| gate.required && gate.status == GateStatus::Pending)
            .map(|gate| gate.id.clone())
            .collect::<Vec<_>>();
        let failed = self
            .gates
            .iter()
            .filter(|gate| gate.required && gate.status == GateStatus::Failed)
            .map(|gate| gate.id.clone())
            .collect::<Vec<_>>();
        if pending.is_empty() && failed.is_empty() {
            CompletionVerdict::Complete
        } else {
            CompletionVerdict::Partial { pending, failed }
        }
    }

    /// Fail closed on a completion claim. The message names the specific gates
    /// so it can be surfaced to the model instead of a bare refusal.
    pub fn assert_can_claim_complete(&self) -> AppResult<()> {
        match self.verdict() {
            CompletionVerdict::Complete => Ok(()),
            CompletionVerdict::Partial { pending, failed } => {
                let mut parts = Vec::new();
                if !pending.is_empty() {
                    parts.push(format!("pending: {}", pending.join(", ")));
                }
                if !failed.is_empty() {
                    parts.push(format!("failed: {}", failed.join(", ")));
                }
                Err(AppError::Message(format!(
                    "cannot claim completion while required gates are unmet ({}); commit scope must be `partial` and the remaining work must be stated",
                    parts.join("; ")
                )))
            }
        }
    }

    /// Work that a partial checkpoint commit must carry forward.
    pub fn remaining_work(&self) -> Vec<Value> {
        self.gates
            .iter()
            .filter(|gate| gate.status != GateStatus::Passed)
            .map(|gate| {
                json!({
                    "id": gate.id,
                    "required": gate.required,
                    "status": gate.status.as_str(),
                    "check": gate.artifact_check.clone().unwrap_or_else(|| {
                        gate.command
                            .as_ref()
                            .map(|command| {
                                std::iter::once(command.program.clone())
                                    .chain(command.args.iter().cloned())
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            })
                            .unwrap_or_default()
                    })
                })
            })
            .collect()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "scope": self.verdict().commit_scope(),
            "gates": self.gates.iter().map(|gate| json!({
                "id": gate.id,
                "required": gate.required,
                "status": gate.status.as_str()
            })).collect::<Vec<_>>()
        })
    }
}

/// The record written before a context compaction when the failure budget has
/// been exceeded, so forbidden retries, failure categories, and pending gates
/// survive the summary instead of being compressed away.
pub fn execution_checkpoint(failures: &FailureLedger, completion: &CompletionLedger) -> Value {
    json!({
        "kind": "vellum.execution_checkpoint",
        "failures": failures.to_json(),
        "completion": completion.to_json(),
        "remainingWork": completion.remaining_work(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::failure::{FailureLedger, FailureSignal};

    fn all_but(ledger: &mut CompletionLedger, skipped: &str) {
        let ids = ledger
            .gates()
            .iter()
            .map(|gate| gate.id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            if id != skipped {
                ledger.mark(&id, GateStatus::Passed);
            }
        }
    }

    #[test]
    fn a_pending_required_gate_forces_a_partial_scope() {
        let mut ledger = CompletionLedger::default_gates();
        all_but(&mut ledger, "tests-passed");
        let verdict = ledger.verdict();
        assert_eq!(verdict.commit_scope(), "partial");
        assert!(matches!(
            verdict,
            CompletionVerdict::Partial { ref pending, .. } if pending == &["tests-passed"]
        ));
        let error = ledger.assert_can_claim_complete().unwrap_err().to_string();
        assert!(error.contains("tests-passed"));
        assert!(error.contains("`partial`"));
    }

    #[test]
    fn a_failed_required_gate_is_reported_separately_from_a_pending_one() {
        let mut ledger = CompletionLedger::default_gates();
        all_but(&mut ledger, "build-passed");
        ledger.mark("build-passed", GateStatus::Failed);
        ledger.mark("typecheck-passed", GateStatus::Pending);
        match ledger.verdict() {
            CompletionVerdict::Partial { pending, failed } => {
                assert_eq!(pending, vec!["typecheck-passed"]);
                assert_eq!(failed, vec!["build-passed"]);
            }
            CompletionVerdict::Complete => panic!("must not be complete"),
        }
    }

    #[test]
    fn completion_requires_every_required_gate() {
        let mut ledger = CompletionLedger::default_gates();
        all_but(&mut ledger, "");
        assert!(ledger.verdict().is_complete());
        assert_eq!(ledger.verdict().commit_scope(), "complete");
        ledger.assert_can_claim_complete().unwrap();
        assert!(ledger.remaining_work().is_empty());
    }

    #[test]
    fn optional_gates_do_not_block_completion() {
        let mut ledger = CompletionLedger::default_gates();
        all_but(&mut ledger, "");
        ledger.add(AcceptanceGate::artifact(
            "changelog",
            "changelog entry",
            false,
        ));
        assert!(ledger.verdict().is_complete());
        // It is still reported as outstanding work.
        assert_eq!(ledger.remaining_work().len(), 1);
    }

    #[test]
    fn default_gates_cover_the_issue_definition_of_done() {
        let ledger = CompletionLedger::default_gates();
        let ids = ledger
            .gates()
            .iter()
            .map(|gate| gate.id.as_str())
            .collect::<Vec<_>>();
        for expected in [
            "issue-checklist-reviewed",
            "diff-inspected",
            "typecheck-passed",
            "tests-passed",
            "build-passed",
            "static-copy-scan-passed",
            "no-unintended-files",
            "no-pending-phase",
        ] {
            assert!(ids.contains(&expected), "missing gate {expected}");
        }
        assert!(ledger.gates().iter().all(|gate| gate.required));
    }

    #[test]
    fn checkpoint_carries_forbidden_retries_categories_and_pending_gates() {
        let mut failures = FailureLedger::new();
        let command = vec!["pwsh".to_string(), "-c".to_string(), "bulk".to_string()];
        for _ in 0..2 {
            failures.record_failure(
                "bulk-migrate",
                &FailureSignal {
                    command: &command,
                    exit_code: Some(1),
                    timed_out: false,
                    stderr: "ParserError",
                },
                "regex-bulk",
            );
        }
        failures.record_temp_script("scratch/migrate.py");

        let mut completion = CompletionLedger::default_gates();
        completion.mark("diff-inspected", GateStatus::Passed);

        let checkpoint = execution_checkpoint(&failures, &completion);
        assert_eq!(checkpoint["kind"], "vellum.execution_checkpoint");
        assert_eq!(
            checkpoint["failures"]["forbiddenRetries"][0]["stepId"],
            "bulk-migrate"
        );
        assert_eq!(checkpoint["failures"]["categories"]["syntax"], 2);
        assert_eq!(
            checkpoint["failures"]["tempScriptsLeaked"][0],
            "scratch/migrate.py"
        );
        assert_eq!(checkpoint["completion"]["scope"], "partial");
        let remaining = checkpoint["remainingWork"].as_array().unwrap();
        assert!(remaining
            .iter()
            .any(|gate| gate["id"] == "tests-passed" && gate["status"] == "pending"));
        assert!(!remaining.iter().any(|gate| gate["id"] == "diff-inspected"));
    }
}
