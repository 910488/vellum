//! Per-host desired model state (M32).
//!
//! The desktop persists, per remote host, the model selection it most
//! recently planned and applied. `desired_revision` advances monotonically
//! per host on every plan; `observed_revision` is updated only after an
//! apply completes, so the UI can always show whether the host matches the
//! desired state and what "reapply desired state" would restore.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::remote::deployment::RemotePolicyOverrides;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct RemoteHostDesiredState {
    pub host_id: String,
    pub desired_revision: u64,
    pub observed_revision: u64,
    pub last_plan_id: Option<String>,
    pub selected_catalog_ids: Vec<String>,
    pub config_hash: Option<String>,
    pub catalog_hash: Option<String>,
    /// Per-host policy overrides (`autoReviewEnabled`, compaction threshold,
    /// standalone web search) from the last plan for this host. Without
    /// this, `reapply_desired_state` would rebuild from
    /// `RemotePolicyOverrides::default()` and silently drop a host-specific
    /// override — e.g. a host with Auto Review deliberately disabled would
    /// have it silently re-enabled on the next reapply.
    #[serde(default)]
    pub policy_overrides: RemotePolicyOverrides,
    /// Fingerprint of the Auto Review policy this plan carried (M9 remote
    /// drift). Set at plan time; compared against
    /// `applied_review_policy_fingerprint` to tell the Remote Manager UI
    /// "your local Auto Review settings changed since this host's last
    /// successful apply" independently of the opaque whole-config hash.
    #[serde(default)]
    pub review_policy_fingerprint: Option<String>,
    /// The review policy fingerprint from the last *successful* `apply()`.
    /// `None` until this host has been applied at least once.
    #[serde(default)]
    pub applied_review_policy_fingerprint: Option<String>,
}

fn desired_state_dir(root: &Path) -> PathBuf {
    root.join("remote-desired-state")
}

pub fn load(root: &Path, host_id: &str) -> AppResult<RemoteHostDesiredState> {
    let path = desired_state_dir(root).join(format!("{host_id}.json"));
    let Ok(bytes) = std::fs::read(&path) else {
        return Ok(RemoteHostDesiredState {
            host_id: host_id.into(),
            ..Default::default()
        });
    };
    let mut state: RemoteHostDesiredState = serde_json::from_slice(&bytes)
        .map_err(|error| AppError::Message(format!("invalid desired state {path:?}: {error}")))?;
    state.host_id = host_id.into();
    Ok(state)
}

pub fn save(root: &Path, state: &RemoteHostDesiredState) -> AppResult<()> {
    let dir = desired_state_dir(root);
    std::fs::create_dir_all(&dir)
        .map_err(|error| AppError::Message(format!("create desired state dir: {error}")))?;
    let path = dir.join(format!("{}.json", state.host_id));
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|error| AppError::Message(format!("encode desired state: {error}")))?;
    std::fs::write(&path, bytes)
        .map_err(|error| AppError::Message(format!("write desired state: {error}")))
}

/// Next monotonic plan revision for a host: `observed + 1` (or 1 when the
/// host has never been applied). Never reuses a previous revision.
pub fn next_revision(root: &Path, host_id: &str) -> AppResult<u64> {
    let state = load(root, host_id)?;
    Ok(state.observed_revision.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_host_defaults_to_revision_zero() {
        let temp = tempfile::tempdir().unwrap();
        let state = load(temp.path(), "h-1").unwrap();
        assert_eq!(state.host_id, "h-1");
        assert_eq!(state.desired_revision, 0);
        assert_eq!(state.observed_revision, 0);
        assert_eq!(next_revision(temp.path(), "h-1").unwrap(), 1);
    }

    #[test]
    fn save_load_round_trip_preserves_revisions() {
        let temp = tempfile::tempdir().unwrap();
        let state = RemoteHostDesiredState {
            host_id: "h-1".into(),
            desired_revision: 3,
            observed_revision: 2,
            last_plan_id: Some("plan-2".into()),
            selected_catalog_ids: vec!["a".into(), "b".into()],
            config_hash: Some("cfg".into()),
            catalog_hash: Some("cat".into()),
            ..Default::default()
        };
        save(temp.path(), &state).unwrap();
        let loaded = load(temp.path(), "h-1").unwrap();
        assert_eq!(loaded, state);
        assert_eq!(next_revision(temp.path(), "h-1").unwrap(), 3);
    }

    #[test]
    fn revisions_are_per_host() {
        let temp = tempfile::tempdir().unwrap();
        save(
            temp.path(),
            &RemoteHostDesiredState {
                host_id: "h-1".into(),
                observed_revision: 5,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(next_revision(temp.path(), "h-1").unwrap(), 6);
        assert_eq!(next_revision(temp.path(), "h-2").unwrap(), 1);
    }
}
