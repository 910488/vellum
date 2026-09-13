//! What the bridge says about itself, and what Vellum is allowed to believe.
//!
//! "Enhanced is ready" used to mean "the Enhanced artifact verified" — a claim
//! about a file on disk, which stayed true while Codex Desktop happily kept
//! running Official Codex. This file is the counter-evidence: the bridge is the
//! only process that can observe both children, so the bridge writes the
//! attestation and Vellum only reads it. A missing, stale, or mismatched
//! attestation is never `ready`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::atomic::write_atomic;
use super::process_info::{executable_of, pid_is_alive};

pub const ATTESTATION_SCHEMA_VERSION: u32 = 1;
/// Failure reasons come from child stderr and provider errors. They are shown
/// in Settings, so they are bounded rather than unbounded pass-through text.
pub const MAX_FAILURE_REASON_CHARS: usize = 480;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BridgeLifecycle {
    /// Children spawned, at least one initialize outstanding.
    Starting,
    /// Both children initialized and every identity check passed.
    Ready,
    /// The bridge is still serving Official traffic but Enhanced is unusable.
    Degraded,
    /// The bridge could not serve either plane.
    Failed,
    /// The bridge exited cleanly.
    Stopped,
}

impl BridgeLifecycle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChildAttestation {
    pub pid: Option<u32>,
    pub binary_sha256: String,
    pub runtime_digest: String,
    pub initialized: bool,
    pub exited: bool,
}

/// Identity the Enhanced fork reports over `vellum/enhancedRuntimeIdentity`.
/// Vellum never infers it from the model name or from the binary path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnhancedRuntimeIdentity {
    pub enhanced_commit: String,
    pub runtime_digest: String,
    pub feature_profile: String,
    pub qwen_tool_reliability: bool,
    pub deepseek_context_recovery: bool,
    pub qwen_bounded_continuation: bool,
    #[serde(default)]
    pub repetition_notice: bool,
    #[serde(default)]
    pub intent_continuation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeAttestationV1 {
    pub schema_version: u32,
    pub launch_id: String,
    pub bridge_pid: u32,
    pub parent_pid: u32,
    pub parent_executable: Option<PathBuf>,
    pub started_at: i64,
    pub updated_at: i64,
    pub state: BridgeLifecycle,
    pub official: ChildAttestation,
    pub enhanced: ChildAttestation,
    pub model_provider_map_sha256: String,
    pub binding_db: PathBuf,
    pub binding_db_identity: String,
    pub enhanced_identity: Option<EnhancedRuntimeIdentity>,
    /// Sessions that reported the effective hook profile from inside the
    /// agent-loop state they actually use. Identity alone proves only process
    /// configuration; this counter proves at least one session consumed it.
    #[serde(default)]
    pub session_features_applied: u64,
    /// How many threads have a turn the bridge has not seen finish.
    ///
    /// Vellum's own in-flight counter only sees Proxy traffic, so a Codex
    /// Desktop turn on the Official plane never touches it. Restarting on the
    /// strength of that counter is how a thread created seconds earlier was
    /// lost before Codex had written it anywhere. The bridge is the only
    /// process that sees these turns, so the bridge counts them.
    ///
    /// A count, not identifiers: the restart guard needs to know whether to
    /// stop, and thread ids have no business in a file Settings displays.
    #[serde(default)]
    pub open_turns: u32,
    pub failure_reason: Option<String>,
}

impl BridgeAttestationV1 {
    pub fn read(path: &Path) -> Result<Self, AttestationError> {
        let bytes = std::fs::read(path).map_err(|error| AttestationError::Read {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        let attestation = serde_json::from_slice::<Self>(&bytes)?;
        if attestation.schema_version != ATTESTATION_SCHEMA_VERSION {
            return Err(AttestationError::UnsupportedSchema(
                attestation.schema_version,
            ));
        }
        Ok(attestation)
    }

    pub fn write(&self, path: &Path) -> Result<(), AttestationError> {
        write_atomic(path, &serde_json::to_vec_pretty(self)?).map_err(|error| {
            AttestationError::Write {
                path: path.to_path_buf(),
                message: error.to_string(),
            }
        })
    }

    /// A bridge that already exited leaves its last attestation behind. Reading
    /// that file and calling it `ready` is exactly the false positive this
    /// whole mechanism exists to prevent.
    pub fn is_live(&self) -> bool {
        !matches!(self.state, BridgeLifecycle::Stopped) && pid_is_alive(self.bridge_pid)
    }

    /// Is Codex Desktop in the middle of something a restart would destroy?
    pub fn has_open_turn(&self) -> bool {
        self.open_turns > 0
    }

    /// The attestation only counts for the launch Vellum just performed.
    pub fn matches_launch(&self, launch_id: &str) -> bool {
        self.launch_id == launch_id
    }

    /// Enhanced is answering for Codex Desktop right now.
    ///
    /// Deliberately not keyed to a launch id. A bridge from an earlier launch
    /// that Desktop is still holding serves every turn through the Enhanced
    /// core, and calling that "not loaded" is simply false -- it is what made
    /// the status button say Enhanced was absent while both children were up
    /// and answering. What the launch id decides is whether it is serving
    /// *this* configuration, which is [`Self::is_active_for`].
    pub fn is_serving(&self) -> bool {
        self.is_live()
            && self.state == BridgeLifecycle::Ready
            && self.official.initialized
            && !self.official.exited
            && self.enhanced.initialized
            && !self.enhanced.exited
    }

    /// True only when the bridge is alive, running the launch we asked for, and
    /// both children answered `initialize` on the expected identities.
    pub fn is_active_for(&self, launch_id: &str) -> bool {
        self.matches_launch(launch_id)
            && self.is_live()
            && self.state == BridgeLifecycle::Ready
            && self.official.initialized
            && !self.official.exited
            && self.enhanced.initialized
            && !self.enhanced.exited
            && self.enhanced_identity.is_some()
    }

    pub fn blockers_for(&self, launch_id: &str) -> Vec<String> {
        let mut blockers = Vec::new();
        if !self.matches_launch(launch_id) {
            blockers.push(format!(
                "bridge attestation is for launch {}, expected {launch_id}",
                self.launch_id
            ));
        }
        if !pid_is_alive(self.bridge_pid) {
            blockers.push(format!(
                "bridge process {} is no longer running",
                self.bridge_pid
            ));
        }
        if !self.official.initialized {
            blockers.push("Official Codex child did not complete initialize".into());
        }
        if !self.enhanced.initialized {
            blockers.push("Enhanced Codex child did not complete initialize".into());
        }
        if self.official.exited {
            blockers.push("Official Codex child exited".into());
        }
        if self.enhanced.exited {
            blockers.push("Enhanced Codex child exited".into());
        }
        if self.enhanced_identity.is_none() {
            blockers.push("Enhanced Codex child did not report its runtime identity".into());
        }
        if let Some(reason) = &self.failure_reason {
            blockers.push(reason.clone());
        }
        blockers
    }
}

/// Writer side. The bridge owns one of these for its whole lifetime.
pub struct AttestationWriter {
    path: PathBuf,
    attestation: BridgeAttestationV1,
}

impl AttestationWriter {
    pub fn new(
        path: PathBuf,
        launch_id: String,
        model_provider_map_sha256: String,
        binding_db: PathBuf,
        official: ChildAttestation,
        enhanced: ChildAttestation,
    ) -> Self {
        let now = chrono::Utc::now().timestamp();
        let bridge_pid = std::process::id();
        let parent_pid = super::process_info::parent_pid(bridge_pid).unwrap_or(0);
        Self {
            path,
            attestation: BridgeAttestationV1 {
                schema_version: ATTESTATION_SCHEMA_VERSION,
                launch_id,
                bridge_pid,
                parent_pid,
                parent_executable: executable_of(parent_pid),
                started_at: now,
                updated_at: now,
                state: BridgeLifecycle::Starting,
                binding_db_identity: binding_db_identity(&binding_db),
                binding_db,
                official,
                enhanced,
                model_provider_map_sha256,
                enhanced_identity: None,
                session_features_applied: 0,
                open_turns: 0,
                failure_reason: None,
            },
        }
    }

    pub fn snapshot(&self) -> &BridgeAttestationV1 {
        &self.attestation
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_child_pid(&mut self, plane: super::ExecutionPlane, pid: u32) {
        self.child_mut(plane).pid = Some(pid);
    }

    pub fn mark_initialized(&mut self, plane: super::ExecutionPlane) {
        self.child_mut(plane).initialized = true;
        self.recompute_state();
    }

    pub fn mark_exited(&mut self, plane: super::ExecutionPlane) {
        let child = self.child_mut(plane);
        child.exited = true;
        child.pid = None;
        self.recompute_state();
    }

    pub fn set_enhanced_identity(&mut self, identity: EnhancedRuntimeIdentity) {
        self.attestation.enhanced_identity = Some(identity);
    }

    pub fn record_session_features_applied(&mut self) {
        self.attestation.session_features_applied =
            self.attestation.session_features_applied.saturating_add(1);
        let _ = self.flush();
    }

    /// Called by the bridge whenever a turn opens or closes. Writing through
    /// on every change is deliberate: the reader is a different process that
    /// is about to decide whether to kill this one.
    pub fn set_open_turns(&mut self, open: u32) {
        if self.attestation.open_turns == open {
            return;
        }
        self.attestation.open_turns = open;
        let _ = self.flush();
    }

    pub fn set_failure_reason(&mut self, reason: impl Into<String>) {
        let mut reason = reason.into();
        if reason.chars().count() > MAX_FAILURE_REASON_CHARS {
            reason = reason
                .chars()
                .take(MAX_FAILURE_REASON_CHARS)
                .collect::<String>()
                + "…";
        }
        self.attestation.failure_reason = Some(reason);
    }

    pub fn mark_stopped(&mut self) {
        self.attestation.state = BridgeLifecycle::Stopped;
    }

    pub fn mark_failed(&mut self, reason: impl Into<String>) {
        self.set_failure_reason(reason);
        self.attestation.state = BridgeLifecycle::Failed;
    }

    fn recompute_state(&mut self) {
        let attestation = &mut self.attestation;
        if attestation.state == BridgeLifecycle::Stopped {
            return;
        }
        attestation.state = if attestation.official.exited {
            BridgeLifecycle::Failed
        } else if attestation.enhanced.exited {
            BridgeLifecycle::Degraded
        } else if attestation.official.initialized && attestation.enhanced.initialized {
            BridgeLifecycle::Ready
        } else {
            BridgeLifecycle::Starting
        };
    }

    fn child_mut(&mut self, plane: super::ExecutionPlane) -> &mut ChildAttestation {
        match plane {
            super::ExecutionPlane::OfficialCodex => &mut self.attestation.official,
            super::ExecutionPlane::EnhancedCodex => &mut self.attestation.enhanced,
        }
    }

    pub fn flush(&mut self) -> Result<(), AttestationError> {
        self.attestation.updated_at = chrono::Utc::now().timestamp();
        self.attestation.write(&self.path)
    }
}

/// Identity of the durable binding database. The path alone is not enough —
/// deleting and recreating the file has to read as a different store.
pub fn binding_db_identity(path: &Path) -> String {
    let length = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    format!("{}#{length}", path.display())
}

#[derive(Debug, thiserror::Error)]
pub enum AttestationError {
    #[error("unsupported bridge attestation schema {0}")]
    UnsupportedSchema(u32),
    #[error("cannot read {}: {message}", path.display())]
    Read { path: PathBuf, message: String },
    #[error("cannot write {}: {message}", path.display())]
    Write { path: PathBuf, message: String },
    #[error("bridge attestation JSON is invalid: {0}")]
    Json(String),
}

impl From<serde_json::Error> for AttestationError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enhanced_runtime::ExecutionPlane;

    fn writer(temp: &tempfile::TempDir) -> AttestationWriter {
        AttestationWriter::new(
            temp.path().join("attestation.json"),
            "launch-1".into(),
            "sha256:map".into(),
            temp.path().join("bindings.sqlite3"),
            ChildAttestation {
                binary_sha256: "sha256:official".into(),
                runtime_digest: "sha256:official-digest".into(),
                ..ChildAttestation::default()
            },
            ChildAttestation {
                binary_sha256: "sha256:enhanced".into(),
                runtime_digest: "sha256:enhanced-digest".into(),
                ..ChildAttestation::default()
            },
        )
    }

    fn report_identity(writer: &mut AttestationWriter) {
        writer.set_enhanced_identity(EnhancedRuntimeIdentity {
            enhanced_commit: "c".repeat(40),
            runtime_digest: "sha256:enhanced-digest".into(),
            feature_profile: "E5".into(),
            qwen_tool_reliability: true,
            deepseek_context_recovery: true,
            qwen_bounded_continuation: true,
            repetition_notice: false,
            intent_continuation: false,
        });
    }

    #[test]
    fn ready_needs_both_children_not_just_a_verified_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let mut writer = writer(&temp);
        writer.flush().unwrap();
        let loaded = BridgeAttestationV1::read(writer.path()).unwrap();
        assert_eq!(loaded.state, BridgeLifecycle::Starting);
        assert!(!loaded.is_active_for("launch-1"));

        writer.mark_initialized(ExecutionPlane::OfficialCodex);
        writer.flush().unwrap();
        assert!(!BridgeAttestationV1::read(writer.path())
            .unwrap()
            .is_active_for("launch-1"));

        writer.mark_initialized(ExecutionPlane::EnhancedCodex);
        writer.flush().unwrap();
        let initialized = BridgeAttestationV1::read(writer.path()).unwrap();
        assert_eq!(initialized.state, BridgeLifecycle::Ready);
        assert!(!initialized.is_active_for("launch-1"));

        report_identity(&mut writer);
        writer.flush().unwrap();
        assert!(BridgeAttestationV1::read(writer.path())
            .unwrap()
            .is_active_for("launch-1"));
    }

    #[test]
    fn a_different_launch_id_is_never_active() {
        let temp = tempfile::tempdir().unwrap();
        let mut writer = writer(&temp);
        writer.mark_initialized(ExecutionPlane::OfficialCodex);
        writer.mark_initialized(ExecutionPlane::EnhancedCodex);
        report_identity(&mut writer);
        writer.flush().unwrap();
        let loaded = BridgeAttestationV1::read(writer.path()).unwrap();
        assert!(loaded.is_active_for("launch-1"));
        assert!(!loaded.is_active_for("launch-2"));
        assert!(loaded
            .blockers_for("launch-2")
            .iter()
            .any(|blocker| blocker.contains("launch-1")));
    }

    #[test]
    fn a_dead_bridge_pid_is_never_active() {
        let temp = tempfile::tempdir().unwrap();
        let mut writer = writer(&temp);
        writer.mark_initialized(ExecutionPlane::OfficialCodex);
        writer.mark_initialized(ExecutionPlane::EnhancedCodex);
        writer.attestation.bridge_pid = 0;
        writer.flush().unwrap();
        let loaded = BridgeAttestationV1::read(writer.path()).unwrap();
        assert_eq!(loaded.state, BridgeLifecycle::Ready);
        assert!(!loaded.is_live());
        assert!(!loaded.is_active_for("launch-1"));
    }

    #[test]
    fn enhanced_exit_degrades_and_official_exit_fails() {
        let temp = tempfile::tempdir().unwrap();
        let mut writer = writer(&temp);
        writer.mark_initialized(ExecutionPlane::OfficialCodex);
        writer.mark_initialized(ExecutionPlane::EnhancedCodex);
        writer.mark_exited(ExecutionPlane::EnhancedCodex);
        assert_eq!(writer.snapshot().state, BridgeLifecycle::Degraded);
        writer.mark_exited(ExecutionPlane::OfficialCodex);
        assert_eq!(writer.snapshot().state, BridgeLifecycle::Failed);
    }

    #[test]
    fn failure_reasons_are_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let mut writer = writer(&temp);
        writer.mark_failed("x".repeat(4_000));
        let reason = writer.snapshot().failure_reason.clone().unwrap();
        assert!(reason.chars().count() <= MAX_FAILURE_REASON_CHARS + 1);
    }
}
