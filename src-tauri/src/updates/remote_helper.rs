//! Idempotent remote full-package apply. The helper is not the agent process:
//! SSH drop, Vellum close, or agent self-replace must not lose the journal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::apply::{remote_idle_decision, ApplyDecision, IdleEvidence, RemoteIdleInput};
use super::journal::{Journal, JournalEntry};
use super::machine::UpdatePhase;
use super::write_atomic;
use super::UpdateComponent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteApplyPlan {
    pub operation_id: String,
    pub host_id: String,
    pub target_version: String,
    pub package_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteStep {
    Lock,
    Stage,
    IdleGate,
    ReplaceSet,
    Validate,
    Commit,
}

impl RemoteStep {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lock => "lock",
            Self::Stage => "stage",
            Self::IdleGate => "idleGate",
            Self::ReplaceSet => "replaceSet",
            Self::Validate => "validate",
            Self::Commit => "commit",
        }
    }
}

pub trait RemoteBackend {
    fn try_lock(&mut self, host_id: &str, operation_id: &str) -> Result<bool, String>;
    fn observe(&self) -> IdleEvidence;
    fn stage_package(&mut self, sha256: &str) -> Result<(), String>;
    /// Replace agent, broker, and proxy as one set. Must keep working if the
    /// agent binary is the thing being replaced.
    fn replace_set(&mut self) -> Result<(), String>;
    /// Inventory + handshake + task list + events — never `/health` alone.
    fn validate_full(&self) -> Result<(), String>;
    fn rollback_set(&mut self) -> Result<(), String>;
    /// Journal that lives on the host so Vellum close cannot lose progress.
    fn host_journal_steps(&self) -> Vec<String> {
        Vec::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteApplyOutcome {
    pub decision: ApplyDecision,
    pub steps: Vec<String>,
    pub replaced: bool,
}

/// Host-side journal under `updates/remote/<host>/<operation_id>.json`.
pub fn remote_journal_path(root: &Path, host_id: &str, operation_id: &str) -> PathBuf {
    root.join("updates")
        .join("remote")
        .join(host_id)
        .join(format!("{operation_id}.json"))
}

pub fn load_remote_steps(root: &Path, host_id: &str, operation_id: &str) -> Vec<String> {
    let Ok(bytes) = std::fs::read(remote_journal_path(root, host_id, operation_id)) else {
        return Vec::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn save_remote_steps(
    root: &Path,
    host_id: &str,
    operation_id: &str,
    steps: &[String],
) -> Result<(), String> {
    let path = remote_journal_path(root, host_id, operation_id);
    let bytes = serde_json::to_vec_pretty(steps).map_err(|error| error.to_string())?;
    write_atomic(&path, &bytes).map_err(|error| error.to_string())
}

pub fn apply_remote_package<B: RemoteBackend>(
    root: &Path,
    journal: &mut Journal,
    backend: &mut B,
    plan: &RemoteApplyPlan,
    policy_enabled: bool,
) -> Result<RemoteApplyOutcome, String> {
    if journal.already_applied(&plan.operation_id) {
        return Ok(RemoteApplyOutcome {
            decision: ApplyDecision::Allow,
            steps: vec!["alreadyApplied".into()],
            replaced: false,
        });
    }
    let mut steps = load_remote_steps(root, &plan.host_id, &plan.operation_id);
    for step in backend.host_journal_steps() {
        if !steps.contains(&step) {
            steps.push(step);
        }
    }
    if steps.iter().any(|step| step == "commit") {
        journal.upsert(JournalEntry {
            operation_id: plan.operation_id.clone(),
            component: UpdateComponent::Remote,
            phase: UpdatePhase::Applied,
            target_version: Some(plan.target_version.clone()),
            reason: None,
            host_id: Some(plan.host_id.clone()),
            created_at: 0,
            updated_at: 0,
        });
        let _ = journal.save(root);
        return Ok(RemoteApplyOutcome {
            decision: ApplyDecision::Allow,
            steps: vec!["alreadyApplied".into()],
            replaced: false,
        });
    }
    if !backend.try_lock(&plan.host_id, &plan.operation_id)? {
        return Ok(RemoteApplyOutcome {
            decision: ApplyDecision::Wait {
                reason: "hostLockHeld".into(),
            },
            steps,
            replaced: false,
        });
    }
    record_step(&mut steps, root, plan, RemoteStep::Lock)?;

    if !steps.iter().any(|step| step == "stage") {
        backend.stage_package(&plan.package_sha256)?;
        record_step(&mut steps, root, plan, RemoteStep::Stage)?;
    }

    let idle = remote_idle_decision(&RemoteIdleInput {
        policy_enabled,
        evidence: backend.observe(),
    });
    record_step(&mut steps, root, plan, RemoteStep::IdleGate)?;
    if idle != ApplyDecision::Allow {
        return Ok(RemoteApplyOutcome {
            decision: idle,
            steps,
            replaced: false,
        });
    }

    if !steps.iter().any(|step| step == "replaceSet") {
        backend.replace_set().inspect_err(|_| {
            let _ = backend.rollback_set();
        })?;
        record_step(&mut steps, root, plan, RemoteStep::ReplaceSet)?;
    }

    backend.validate_full().inspect_err(|_| {
        let _ = backend.rollback_set();
    })?;
    record_step(&mut steps, root, plan, RemoteStep::Validate)?;

    journal.upsert(JournalEntry {
        operation_id: plan.operation_id.clone(),
        component: UpdateComponent::Remote,
        phase: UpdatePhase::Applied,
        target_version: Some(plan.target_version.clone()),
        reason: None,
        host_id: Some(plan.host_id.clone()),
        created_at: 0,
        updated_at: 0,
    });
    journal.save(root).map_err(|error| error.to_string())?;
    record_step(&mut steps, root, plan, RemoteStep::Commit)?;

    Ok(RemoteApplyOutcome {
        decision: ApplyDecision::Allow,
        steps,
        replaced: true,
    })
}

fn record_step(
    steps: &mut Vec<String>,
    root: &Path,
    plan: &RemoteApplyPlan,
    step: RemoteStep,
) -> Result<(), String> {
    let name = step.as_str().to_string();
    if !steps.contains(&name) {
        steps.push(name);
        save_remote_steps(root, &plan.host_id, &plan.operation_id, steps)?;
    }
    Ok(())
}

/// In-memory host used by tests to inject mid-apply turns, SSH drops, and
/// agent self-replace. The helper does not depend on a live agent PID.
#[derive(Default)]
pub struct FakeRemoteHost {
    pub locked: BTreeMap<String, String>,
    pub staged: bool,
    pub replaced: bool,
    pub rolled_back: bool,
    pub validated: bool,
    pub ssh_drop_on_stage: bool,
    pub agent_replaced_during_swap: bool,
    pub evidence: IdleEvidence,
    pub health_only: bool,
}

impl RemoteBackend for FakeRemoteHost {
    fn try_lock(&mut self, host_id: &str, operation_id: &str) -> Result<bool, String> {
        match self.locked.get(host_id) {
            None => {
                self.locked.insert(host_id.into(), operation_id.into());
                Ok(true)
            }
            Some(held) if held == operation_id => Ok(true),
            Some(_) => Ok(false),
        }
    }

    fn observe(&self) -> IdleEvidence {
        self.evidence.clone()
    }

    fn stage_package(&mut self, _sha256: &str) -> Result<(), String> {
        if self.ssh_drop_on_stage && !self.staged {
            self.ssh_drop_on_stage = false;
            return Err("ssh dropped".into());
        }
        self.staged = true;
        Ok(())
    }

    fn replace_set(&mut self) -> Result<(), String> {
        if !self.staged {
            return Err("replace without stage".into());
        }
        // Agent self-replace must not abort the helper.
        let _ = self.agent_replaced_during_swap;
        self.replaced = true;
        Ok(())
    }

    fn validate_full(&self) -> Result<(), String> {
        if self.health_only {
            return Err("refusing /health-only validation".into());
        }
        if !self.replaced {
            return Err("not replaced".into());
        }
        Ok(())
    }

    fn rollback_set(&mut self) -> Result<(), String> {
        self.rolled_back = true;
        self.replaced = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> IdleEvidence {
        IdleEvidence {
            proxy_idle: true,
            native_sessions_idle: true,
            tools_idle: true,
            approvals_idle: true,
            heartbeat_expired: false,
            observation_fresh: true,
            unknown_work: false,
            open_turns: 0,
            durable_handoff_ready: None,
        }
    }

    fn plan() -> RemoteApplyPlan {
        RemoteApplyPlan {
            operation_id: "op-remote-1".into(),
            host_id: "host-a".into(),
            target_version: "0.4.0".into(),
            package_sha256: "aa".into(),
        }
    }

    #[test]
    fn mid_apply_new_turn_does_not_replace() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::default();
        let mut host = FakeRemoteHost {
            evidence: IdleEvidence {
                open_turns: 1,
                observation_fresh: true,
                ..idle()
            },
            ..FakeRemoteHost::default()
        };
        let outcome =
            apply_remote_package(dir.path(), &mut journal, &mut host, &plan(), true).unwrap();
        assert!(matches!(outcome.decision, ApplyDecision::Wait { .. }));
        assert!(!host.replaced);
        assert!(!journal.already_applied("op-remote-1"));
    }

    #[test]
    fn ssh_drop_then_same_operation_id_resumes_without_double_replace() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::default();
        let mut host = FakeRemoteHost {
            evidence: idle(),
            ssh_drop_on_stage: true,
            ..FakeRemoteHost::default()
        };
        let first = apply_remote_package(dir.path(), &mut journal, &mut host, &plan(), true);
        assert!(first.unwrap_err().contains("ssh dropped"));
        let second =
            apply_remote_package(dir.path(), &mut journal, &mut host, &plan(), true).unwrap();
        assert_eq!(second.decision, ApplyDecision::Allow);
        assert!(second.replaced);
        let third =
            apply_remote_package(dir.path(), &mut journal, &mut host, &plan(), true).unwrap();
        assert_eq!(third.steps, vec!["alreadyApplied".to_string()]);
        assert!(!third.replaced);
    }

    #[test]
    fn agent_self_replace_does_not_abort_the_helper() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::default();
        let mut host = FakeRemoteHost {
            evidence: idle(),
            agent_replaced_during_swap: true,
            ..FakeRemoteHost::default()
        };
        let outcome =
            apply_remote_package(dir.path(), &mut journal, &mut host, &plan(), true).unwrap();
        assert!(outcome.replaced);
        assert!(journal.already_applied("op-remote-1"));
    }

    #[test]
    fn health_only_validation_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::default();
        let mut host = FakeRemoteHost {
            evidence: idle(),
            health_only: true,
            ..FakeRemoteHost::default()
        };
        let error =
            apply_remote_package(dir.path(), &mut journal, &mut host, &plan(), true).unwrap_err();
        assert!(error.contains("/health-only"));
        assert!(host.rolled_back);
        assert!(!journal.already_applied("op-remote-1"));
    }
}
