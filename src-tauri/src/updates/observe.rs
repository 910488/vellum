//! Live idle / apply observation. Predicates in `apply.rs` stay pure; this
//! module is how production fills them from the running host.

use crate::enhanced_runtime::{live_bridge_attestation, BridgeAttestationV1};
use crate::state::AppState;

use super::apply::{DesktopApplyInput, IdleEvidence};

pub fn live_idle_evidence(state: &AppState, heartbeat_expired: bool) -> IdleEvidence {
    live_idle_evidence_from(
        state.active_requests(),
        live_bridge_attestation(&state.data_root()).as_ref(),
        state.proxy_status().running,
        heartbeat_expired,
    )
}

pub fn live_idle_evidence_from(
    proxy_active_requests: u64,
    attestation: Option<&BridgeAttestationV1>,
    proxy_running: bool,
    heartbeat_expired: bool,
) -> IdleEvidence {
    let open_turns = attestation.map(|item| item.open_turns).unwrap_or(0);
    let observed = attestation.is_some() || !proxy_running;
    IdleEvidence {
        proxy_idle: proxy_active_requests == 0,
        native_sessions_idle: open_turns == 0,
        tools_idle: open_turns == 0,
        approvals_idle: open_turns == 0,
        heartbeat_expired,
        observation_fresh: observed && !heartbeat_expired,
        unknown_work: proxy_running && attestation.is_none(),
        open_turns,
        // Phase-1 cores do not report durable handoff. Missing is None, never
        // inferred from openTurns = 0.
        durable_handoff_ready: None,
    }
}

pub fn desktop_apply_input(state: &AppState, ui_open: bool) -> DesktopApplyInput {
    let attestation = live_bridge_attestation(&state.data_root());
    DesktopApplyInput {
        ui_open,
        proxy_active_requests: state.active_requests(),
        core_in_progress: attestation
            .as_ref()
            .is_some_and(|item| item.has_open_turn()),
        observation_available: attestation.is_some() || !state.proxy_status().running,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enhanced_runtime::{BridgeLifecycle, ChildAttestation};

    fn attestation(open: u32) -> BridgeAttestationV1 {
        BridgeAttestationV1 {
            schema_version: 1,
            launch_id: "l".into(),
            bridge_pid: std::process::id(),
            parent_pid: 0,
            parent_executable: None,
            started_at: 0,
            updated_at: 0,
            state: BridgeLifecycle::Ready,
            official: ChildAttestation::default(),
            enhanced: ChildAttestation::default(),
            model_provider_map_sha256: String::new(),
            binding_db: std::path::PathBuf::new(),
            binding_db_identity: String::new(),
            enhanced_identity: None,
            session_features_applied: 0,
            open_turns: open,
            failure_reason: None,
        }
    }

    #[test]
    fn open_turns_are_not_idle_and_do_not_invent_durable_handoff() {
        let evidence = live_idle_evidence_from(0, Some(&attestation(2)), true, false);
        assert!(!evidence.native_sessions_idle);
        assert_eq!(evidence.open_turns, 2);
        assert!(evidence.durable_handoff_ready.is_none());
        assert!(evidence.observation_fresh);
    }

    #[test]
    fn running_proxy_without_attestation_is_unknown_work() {
        let evidence = live_idle_evidence_from(0, None, true, false);
        assert!(evidence.unknown_work);
        assert!(!evidence.observation_fresh);
    }

    #[test]
    fn heartbeat_expiry_is_not_fresh_idle() {
        let evidence = live_idle_evidence_from(0, Some(&attestation(0)), true, true);
        assert!(evidence.heartbeat_expired);
        assert!(!evidence.observation_fresh);
    }
}
