//! Keeps Codex Desktop on the current Enhanced launch without restarting it.
//!
//! Codex Desktop reads `CODEX_CLI_PATH` once, but it starts the bridge that
//! variable names every time its App Server exits, within a fraction of a
//! second. The bridge in turn reads the launch manifest only when it starts.
//! So whenever the live bridge is serving something other than the current
//! launch -- a launch Vellum has since rebuilt, or plain Official because the
//! lease was briefly gone or Desktop moved to a core nobody had checked --
//! ending that one process is the whole repair. Desktop starts a fresh bridge
//! and the fresh bridge reads the launch that is there now.
//!
//! Before this, the same states lit "restart Codex" and waited for the user,
//! and every Vellum update and every Codex update produced one of them.
//!
//! The steward only ever ends a bridge with no turn open, on a launch Vellum
//! holds the lease for, once per launch. Ending it mid-turn would cost the
//! user the turn; ending it twice for the same launch means the first attempt
//! found something a second one will not fix.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use crate::enhanced_runtime::attestation::BridgeAttestationV1;
use crate::enhanced_runtime::LaunchManifestV1;
use crate::model::RuntimeNotice;
use crate::state::AppState;

const FIRST_PASS: Duration = Duration::from_secs(10);
const PASS_INTERVAL: Duration = Duration::from_secs(20);
const RECYCLE_READY_TIMEOUT: Duration = Duration::from_secs(90);
const RECYCLE_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) fn spawn(state: AppState) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_PASS).await;
        let mut recycled = HashSet::<String>::new();
        loop {
            if state.proxy_status().running {
                steward_pass(&state, &mut recycled).await;
            }
            tokio::time::sleep(PASS_INTERVAL).await;
        }
    });
}

async fn steward_pass(state: &AppState, recycled: &mut HashSet<String>) {
    let data_root = state.data_root();
    let pass_root = data_root.clone();
    let refresh_state = state.clone();
    // Successor checks probe Codex binaries; none of it belongs on a runtime
    // worker thread.
    let refreshed = tokio::task::spawn_blocking(move || {
        let Ok(_guard) = refresh_state.lifecycle_lock().try_lock() else {
            return false;
        };
        if let Err(error) =
            crate::enhanced_runtime::desktop_manager::refresh_official_successors(&pass_root)
        {
            log::warn!("[Enhanced] Official successor refresh failed kind={error:?}");
        }
        crate::enhanced_runtime::desktop_manager::roll_forward_official_successor(&pass_root)
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    if refreshed {
        state.record_live_applied(RuntimeNotice::new("enhancedLaunchFollowedCodexUpdate"));
    }
    let status_state = state.clone();
    let Ok(status) = tokio::task::spawn_blocking(move || {
        let guard = status_state.lifecycle_lock().try_lock();
        let status = crate::enhanced_runtime::desktop_runtime_status(&status_state.data_root());
        if guard.is_ok() {
            status_state
                .reconcile_codex_restart(crate::commands::runtime::codex_process_identity());
            status_state.reconcile_enhanced_adoption(
                (status.ready || status.bridge_protection_active) && !status.restart_required,
            );
        }
        status
    })
    .await
    else {
        return;
    };
    // Rebuilding first and recycling second is the whole point of the order:
    // the bridge Desktop starts next must find the new launch already there.
    crate::commands::runtime::repair_superseded_launch(state, &status);
    if let Some(target) = recycle_target(state, &data_root, &status, recycled) {
        let launch_id = target.launch_id.clone();
        if recycle(state, target).await {
            recycled.insert(launch_id);
        }
    }
}

struct RecycleTarget {
    launch_id: String,
    attestation_path: std::path::PathBuf,
    bridge_only: bool,
    bridge_pid: u32,
    reason: &'static str,
}

/// Why the live bridge should give way to the current launch, if it should.
///
/// Only the attestation and the manifest decide this; whether it is *safe*
/// right now (lease, open turns, which process the pid is) is checked by the
/// caller, separately, because those answers change by the second.
fn recycle_reason(
    manifest: &LaunchManifestV1,
    attestation: &BridgeAttestationV1,
    launch_would_drift: bool,
) -> Option<&'static str> {
    // A fresh bridge would only fall back again: the launch itself names a
    // core Desktop no longer has, and rebuilding it is the repair's job.
    if launch_would_drift {
        return None;
    }
    if !attestation.matches_launch(&manifest.launch_id) {
        return Some("supersededLaunch");
    }
    if !attestation.incomplete_official.is_empty() {
        return Some("incompleteOfficial");
    }
    match attestation.fallback.as_deref() {
        Some("leaseReleased") => Some("leaseRestored"),
        Some(_) => Some("successorApproved"),
        None => None,
    }
}

fn recycle_target(
    state: &AppState,
    data_root: &Path,
    status: &crate::enhanced_runtime::DesktopRuntimeStatus,
    recycled: &HashSet<String>,
) -> Option<RecycleTarget> {
    // A pending Enhanced core is promoted or rolled back by the managed
    // restart, which watches the new launch through its first turn. Swapping
    // the bridge underneath it here would skip that verdict.
    if !state.proxy_status().running || !status.enabled || status.environment_state != "leased" {
        return None;
    }
    let manifest = LaunchManifestV1::read(&LaunchManifestV1::path_in(data_root)).ok()?;
    let Some(attestation) = crate::enhanced_runtime::live_bridge_attestation(data_root) else {
        log::info!(
            "[Launch] bridge recycle skipped reason=unreadableAttestation launch={}",
            manifest.launch_id
        );
        return None;
    };
    if let Some(reason) = recycle_skip(
        status.pending_core,
        recycled.contains(&manifest.launch_id),
        attestation.has_open_turn(),
    ) {
        log::info!(
            "[Launch] bridge recycle skipped reason={reason} launch={} pid={}",
            manifest.launch_id,
            attestation.bridge_pid
        );
        return None;
    }
    let lease = crate::enhanced_runtime::env_lease::EnvironmentLease::read(data_root)?;
    if lease.launch_id != manifest.launch_id {
        return None;
    }
    let reason = recycle_reason(
        &manifest,
        &attestation,
        crate::enhanced_runtime::official_drift::would_drift(&manifest)
            || !crate::enhanced_runtime::official_drift::launch_is_complete(&manifest),
    )?;
    if !crate::enhanced_runtime::desktop_manager::adopted_by_codex_desktop(&attestation)
        || attestation.has_open_turn()
    {
        return None;
    }
    Some(RecycleTarget {
        launch_id: manifest.launch_id.clone(),
        attestation_path: manifest.attestation_path.clone(),
        bridge_only: manifest.enhanced.is_none(),
        bridge_pid: attestation.bridge_pid,
        reason,
    })
}

fn recycle_skip(pending: bool, spent: bool, open: bool) -> Option<&'static str> {
    if pending {
        Some("pendingPromotion")
    } else if spent {
        Some("alreadyRecycled")
    } else if open {
        Some("openTurn")
    } else {
        None
    }
}

/// True once the bridge was ended, whatever came back after it.
async fn recycle(state: &AppState, target: RecycleTarget) -> bool {
    let data_root = state.data_root();
    let kill_state = state.clone();
    let bridge_pid = target.bridge_pid;
    let expected_launch = target.launch_id.clone();
    let killed = tokio::task::spawn_blocking(move || {
        // Launch publication and managed restarts own the bridge while they
        // hold this; the next pass will look again.
        let Ok(_guard) = kill_state.lifecycle_lock().try_lock() else {
            return false;
        };
        // Read again immediately before acting: a turn opened since the pass
        // began is the one thing this must never cut short.
        let Some(current) = crate::enhanced_runtime::live_bridge_attestation(&data_root) else {
            log::info!(
                "[Launch] bridge recycle skipped reason=unreadableAttestation pid={bridge_pid}"
            );
            return false;
        };
        if current.bridge_pid != bridge_pid || current.has_open_turn() {
            log::info!(
                "[Launch] bridge recycle skipped reason=bridgeChangedOrOpenTurn pid={bridge_pid}"
            );
            return false;
        }
        let status = crate::enhanced_runtime::desktop_runtime_status(&data_root);
        if status.pending_core {
            log::info!("[Launch] bridge recycle skipped reason=pendingPromotion pid={bridge_pid}");
            return false;
        }
        let Ok(manifest) = LaunchManifestV1::read(&LaunchManifestV1::path_in(&data_root)) else {
            return false;
        };
        let lease = crate::enhanced_runtime::env_lease::EnvironmentLease::read(&data_root);
        if manifest.launch_id != expected_launch
            || !kill_state.proxy_status().running
            || !status.enabled
            || status.environment_state != "leased"
            || !lease.is_some_and(|lease| lease.launch_id == manifest.launch_id)
            || !crate::enhanced_runtime::desktop_manager::adopted_by_codex_desktop(&current)
            || crate::enhanced_runtime::official_drift::would_drift(&manifest)
            || !crate::enhanced_runtime::official_drift::launch_is_complete(&manifest)
        {
            return false;
        }
        let is_our_bridge = crate::enhanced_runtime::process_info::executable_of(bridge_pid)
            .is_some_and(|path| {
                crate::enhanced_runtime::process_info::is_bundled_sidecar_executable(&path)
            });
        if !is_our_bridge {
            return false;
        }
        let Some(latest) = crate::enhanced_runtime::live_bridge_attestation(&data_root) else {
            return false;
        };
        if latest.bridge_pid != bridge_pid || latest.has_open_turn() {
            log::info!(
                "[Launch] bridge recycle skipped reason=bridgeChangedOrOpenTurn pid={bridge_pid}"
            );
            return false;
        }
        match crate::enhanced_runtime::process_info::terminate_tree(bridge_pid) {
            Ok(()) => true,
            Err(error) => {
                log::warn!("[Enhanced] cannot recycle bridge pid={bridge_pid}: {error}");
                false
            }
        }
    })
    .await
    .unwrap_or(false);
    if !killed {
        return false;
    }
    log::info!(
        "[Launch] recycled idle bridge pid={} reason={} launch={}",
        target.bridge_pid,
        target.reason,
        target.launch_id
    );
    let observed = await_launch_served(&target).await;
    let status_state = state.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let _guard = status_state.lifecycle_lock().blocking_lock();
        let status = crate::enhanced_runtime::desktop_runtime_status(&status_state.data_root());
        // #77 settles older reasons before any new process identity is bound.
        // Bridge recycling itself does not satisfy catalog/config reasons.
        status_state.reconcile_codex_restart(crate::commands::runtime::codex_process_identity());
        status_state.reconcile_enhanced_adoption(
            observed
                && (status.ready || status.bridge_protection_active)
                && !status.restart_required,
        );
    })
    .await;
    state.record_live_applied(
        RuntimeNotice::new(if observed {
            "enhancedBridgeRecycled"
        } else {
            "enhancedBridgeRecycleNotObserved"
        })
        .with("reason", target.reason)
        .with("launchId", target.launch_id),
    );
    // Spent whatever the outcome: a bridge that does not come back ready has
    // met a fault another recycle of the same launch will not fix.
    true
}

fn served(attestation: &BridgeAttestationV1, launch_id: &str, bridge_only: bool) -> bool {
    if bridge_only {
        attestation.matches_launch(launch_id)
            && attestation.fallback.is_none()
            && attestation.incomplete_official.is_empty()
            && attestation.is_live()
            && attestation.official.initialized
            && !attestation.official.exited
    } else {
        attestation.is_active_for(launch_id)
    }
}

async fn await_launch_served(target: &RecycleTarget) -> bool {
    let deadline = std::time::Instant::now() + RECYCLE_READY_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if let Ok(attestation) = BridgeAttestationV1::read(&target.attestation_path) {
            if attestation.bridge_pid != target.bridge_pid
                && served(&attestation, &target.launch_id, target.bridge_only)
                && crate::enhanced_runtime::desktop_manager::adopted_by_codex_desktop(&attestation)
            {
                return true;
            }
        }
        tokio::time::sleep(RECYCLE_POLL_INTERVAL).await;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enhanced_runtime::attestation::{BridgeLifecycle, ChildAttestation};

    fn manifest(launch_id: &str) -> LaunchManifestV1 {
        serde_json::from_value(serde_json::json!({
            "schemaVersion": 1,
            "launchId": launch_id,
            "createdAt": 1,
            "official": {
                "executable": "C:/codex.exe",
                "artifactSha256": "sha256:a",
                "runtimeDigest": "sha256:a",
                "codexHome": "C:/home"
            },
            "modelProviderMapPath": "C:/map.json",
            "modelProviderMapSha256": "sha256:m",
            "bindingDb": "C:/bindings.sqlite3",
            "officialProviderIds": ["openai-official"],
            "thirdPartyProviderIds": [],
            "featureProfile": {},
            "enhancedCommit": "",
            "attestationPath": "C:/bridge-attestation.json",
            "qualificationJournalPath": "C:/journal.jsonl",
            "relay": null
        }))
        .unwrap()
    }

    fn attestation(launch_id: &str, fallback: Option<&str>) -> BridgeAttestationV1 {
        BridgeAttestationV1 {
            schema_version: 1,
            launch_id: launch_id.into(),
            bridge_pid: std::process::id(),
            parent_pid: 0,
            parent_executable: None,
            started_at: 0,
            updated_at: 0,
            state: BridgeLifecycle::Ready,
            official: ChildAttestation::default(),
            enhanced: ChildAttestation::default(),
            model_provider_map_sha256: String::new(),
            binding_db: "C:/bindings.sqlite3".into(),
            binding_db_identity: String::new(),
            enhanced_identity: None,
            session_features_applied: 0,
            open_turns: 0,
            fallback: fallback.map(str::to_owned),
            incomplete_official: Vec::new(),
            failure_reason: None,
        }
    }

    #[test]
    fn a_bridge_on_the_current_launch_is_left_alone() {
        assert_eq!(
            recycle_reason(&manifest("L2"), &attestation("L2", None), false),
            None
        );
    }

    #[test]
    fn a_bridge_from_a_rebuilt_launch_gives_way() {
        assert_eq!(
            recycle_reason(&manifest("L2"), &attestation("L1", None), false),
            Some("supersededLaunch")
        );
    }

    #[test]
    fn a_fallback_bridge_gives_way_once_the_launch_can_be_served() {
        let manifest = manifest("L2");
        assert_eq!(
            recycle_reason(&manifest, &attestation("L2", Some("leaseReleased")), false),
            Some("leaseRestored")
        );
        assert_eq!(
            recycle_reason(&manifest, &attestation("L2", Some("officialDrift")), false),
            Some("successorApproved")
        );
    }

    #[test]
    fn nothing_is_recycled_into_a_launch_that_would_fall_back_again() {
        let manifest = manifest("L2");
        assert_eq!(
            recycle_reason(&manifest, &attestation("L1", None), true),
            None
        );
        assert_eq!(
            recycle_reason(&manifest, &attestation("L2", Some("officialDrift")), true),
            None
        );
    }

    #[test]
    fn idle_superseded_launch_is_recycled_once_and_busy_passes_can_retry() {
        let manifest = manifest("L2");
        let mut bridge = attestation("L1", None);
        assert_eq!(
            recycle_reason(&manifest, &bridge, false),
            Some("supersededLaunch")
        );
        let mut recycled = HashSet::new();
        bridge.open_turns = 1;
        assert_eq!(
            recycle_skip(false, recycled.contains("L2"), bridge.has_open_turn()),
            Some("openTurn")
        );
        bridge.open_turns = 0;
        assert_eq!(
            recycle_skip(false, recycled.contains("L2"), bridge.has_open_turn()),
            None
        );
        recycled.insert("L2");
        assert_eq!(
            recycle_skip(false, recycled.contains("L2"), false),
            Some("alreadyRecycled")
        );
        assert_eq!(recycle_skip(true, false, false), Some("pendingPromotion"));
    }

    #[test]
    fn incomplete_official_is_recycled_only_into_a_complete_launch() {
        let mut bridge = attestation("L2", None);
        bridge
            .incomplete_official
            .push("codex-code-mode-host.exe".into());
        assert_eq!(
            recycle_reason(&manifest("L2"), &bridge, false),
            Some("incompleteOfficial")
        );
        assert_eq!(recycle_reason(&manifest("L2"), &bridge, true), None);
    }
}
