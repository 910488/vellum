//! Independently releasable updates for desktop, remote, and Enhanced core.
//!
//! Trust is Ed25519 over the raw manifest bytes, then SHA-256 of each asset.
//! SHA-256 alone is never a source of trust. Live GitHub auto-update stays
//! disabled until a production verifying key is compiled in.

mod apply;
mod cache;
mod core_slots;
mod desktop_install;
mod engine;
mod github;
mod journal;
mod machine;
mod manifest;
mod observe;
mod remote_helper;
mod remote_host;
mod select;
mod trust;

pub use apply::{
    core_may_promote, desktop_may_apply, download_does_not_clear_restart_reasons,
    idle_handoff_decision, remote_idle_decision, ApplyDecision, CorePromoteInput,
    DesktopApplyInput, IdleEvidence, RemoteIdleInput,
};
pub use cache::{extract_verified_archive, stage_bytes, CacheError};
pub use core_slots::{
    arm_core_pending_apply, conclude_pending_core, consume_core_pending_apply,
    next_start_should_apply_pending, pending_core, persist_slots, promote_pending,
    resolve_enhanced_runtime, rollback_to_previous, signed_core_digest, CoreConclude, CoreSlots,
    ResolvedCore, RuntimeSource,
};
pub use engine::{
    apply_staged_on_exit, apply_update, cancel_download, check_updates, download_update,
    get_status, progress_from, rollback_update, set_preferences, set_remote_policy, UpdateEngine,
};
pub use github::{
    parse_github_releases, GithubAsset, GithubRelease, ListedReleases, MemorySource, ReleaseSource,
};
pub use observe::{desktop_apply_input, live_idle_evidence, live_idle_evidence_from};
pub use remote_helper::{apply_remote_package, FakeRemoteHost, RemoteApplyPlan, RemoteBackend};
pub use remote_host::{idle_from_host_facts, observe_remote_host, AppRemoteBackend};

pub use journal::{Journal, JournalEntry};
pub use machine::{UpdateEvent, UpdatePhase};
pub use manifest::{
    sign_raw, verify_asset_sha256, verify_signed_manifest, AssetRef, CompatRange, UpdateManifest,
    MANIFEST_SCHEMA_VERSION,
};
pub use select::{select_compatible, Channel, ReleaseCandidate, SelectContext, Selection};
pub use trust::{live_auto_update_enabled, TrustStore};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("update"),
        std::process::id()
    ));
    {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(_) if path.exists() => {
            let result =
                std::fs::remove_file(path).and_then(|()| std::fs::rename(&temporary, path));
            if result.is_err() {
                let _ = std::fs::remove_file(&temporary);
            }
            result
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::model::UPDATES_PROGRESS_EVENT;
use crate::state::AppState;

const AUTO_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const AUTO_CHECK_JITTER_SECS: u64 = 30 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UpdateComponent {
    Desktop,
    Remote,
    Core,
}

impl UpdateComponent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Remote => "remote",
            Self::Core => "core",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "desktop" => Some(Self::Desktop),
            "remote" => Some(Self::Remote),
            "core" => Some(Self::Core),
            _ => None,
        }
    }

    pub fn github_repo(self) -> &'static str {
        match self {
            Self::Desktop | Self::Remote | Self::Core => "910488/vellum",
        }
    }

    pub fn tag_prefix(self) -> &'static str {
        match self {
            Self::Desktop => "desktop-v",
            Self::Remote => "remote-v",
            Self::Core => "core-v",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePreferences {
    pub channel: Channel,
    pub auto_check: bool,
    pub auto_download: bool,
    /// Preview-only Enhanced idle handoff. Default off; v1 stays on next-start.
    pub core_idle_handoff: bool,
}

impl Default for UpdatePreferences {
    fn default() -> Self {
        Self {
            channel: Channel::Stable,
            auto_check: true,
            auto_download: true,
            core_idle_handoff: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteUpdatePolicy {
    pub host_id: String,
    pub idle_auto_update: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LayerStatus {
    pub component: UpdateComponent,
    pub current_version: String,
    pub available_version: Option<String>,
    pub staged_version: Option<String>,
    pub channel: Channel,
    pub phase: UpdatePhase,
    pub apply_condition: String,
    pub release_notes: Option<String>,
    pub failure_reason: Option<String>,
    pub operation_id: Option<String>,
    pub target_version: Option<String>,
    pub download_bytes: u64,
    pub download_total: u64,
    pub live_auto_update: bool,
    pub hosts: Vec<HostUpdateStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HostUpdateStatus {
    pub host_id: String,
    pub phase: UpdatePhase,
    pub current_version: Option<String>,
    pub staged_version: Option<String>,
    pub idle_auto_update: bool,
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatusSnapshot {
    pub desktop: LayerStatus,
    pub remote: LayerStatus,
    pub core: LayerStatus,
    pub preferences: UpdatePreferences,
    pub live_auto_update: bool,
    pub attention: UpdateAttention,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum UpdateAttention {
    None,
    Available,
    WaitingIdle,
    WaitingRestart,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateOperation {
    pub operation_id: String,
    pub component: UpdateComponent,
    pub phase: UpdatePhase,
    pub target_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateProgress {
    pub operation_id: String,
    pub component: UpdateComponent,
    pub phase: UpdatePhase,
    pub bytes: u64,
    pub total: u64,
    pub target_version: Option<String>,
    pub message: Option<String>,
}

pub fn spawn_auto_check(app: AppHandle, data_root: PathBuf) {
    tauri::async_runtime::spawn(async move {
        // Startup check after a short delay so window paint is not blocked.
        tokio::time::sleep(Duration::from_secs(8)).await;
        loop {
            let state = (*app.state::<AppState>()).clone();
            if should_auto_check(live_auto_update_enabled(), &get_status(&state).preferences) {
                if let Ok(snapshot) = check_updates(&state, None).await {
                    let _ = app.emit(UPDATES_PROGRESS_EVENT, &snapshot.attention);
                    if snapshot.preferences.auto_download {
                        for component in [
                            UpdateComponent::Desktop,
                            UpdateComponent::Remote,
                            UpdateComponent::Core,
                        ] {
                            let layer = match component {
                                UpdateComponent::Desktop => &snapshot.desktop,
                                UpdateComponent::Remote => &snapshot.remote,
                                UpdateComponent::Core => &snapshot.core,
                            };
                            if layer.live_auto_update && layer.phase == UpdatePhase::Available {
                                let _ = download_update(&state, component, None).await;
                            }
                        }
                    }
                }
            }
            let jitter = auto_check_jitter_secs();
            tokio::time::sleep(AUTO_CHECK_INTERVAL + Duration::from_secs(jitter)).await;
        }
    });
    let _ = data_root;
}

fn should_auto_check(live_enabled: bool, preferences: &UpdatePreferences) -> bool {
    live_enabled && preferences.auto_check
}

fn auto_check_jitter_secs() -> u64 {
    use sha2::{Digest, Sha256};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let digest = Sha256::digest(now.to_le_bytes());
    u64::from(digest[0]) % (AUTO_CHECK_JITTER_SECS + 1)
}

pub(crate) fn engine(data_root: &std::path::Path) -> UpdateEngine {
    UpdateEngine::open(
        data_root,
        TrustStore::bundled(),
        Arc::new(github::GithubSource::new()),
    )
}

#[cfg(test)]
mod auto_check_tests {
    use super::*;

    #[test]
    fn preference_disables_scheduled_checks() {
        let preferences = UpdatePreferences {
            auto_check: false,
            ..UpdatePreferences::default()
        };
        assert!(!should_auto_check(true, &preferences));
        let preferences = UpdatePreferences {
            auto_check: true,
            ..preferences
        };
        assert!(should_auto_check(true, &preferences));
        assert!(!should_auto_check(false, &preferences));
    }
}
