//! Apply gates. Updates never interrupt an in-progress turn, tool, or approval.

use serde::{Deserialize, Serialize};

use super::machine::UpdatePhase;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApplyDecision {
    Allow,
    Wait { reason: String },
    Refuse { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopApplyInput {
    pub ui_open: bool,
    pub proxy_active_requests: u64,
    pub core_in_progress: bool,
    pub observation_available: bool,
}

pub fn desktop_may_apply(input: &DesktopApplyInput) -> ApplyDecision {
    if input.ui_open {
        return ApplyDecision::Wait {
            reason: "waitingForRestart".into(),
        };
    }
    if !input.observation_available {
        return ApplyDecision::Wait {
            reason: "observationUnavailable".into(),
        };
    }
    if input.proxy_active_requests > 0 {
        return ApplyDecision::Wait {
            reason: "proxyBusy".into(),
        };
    }
    if input.core_in_progress {
        return ApplyDecision::Wait {
            reason: "coreBusy".into(),
        };
    }
    ApplyDecision::Allow
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdleEvidence {
    pub proxy_idle: bool,
    pub native_sessions_idle: bool,
    pub tools_idle: bool,
    pub approvals_idle: bool,
    pub heartbeat_expired: bool,
    pub observation_fresh: bool,
    pub unknown_work: bool,
    pub open_turns: u32,
    pub durable_handoff_ready: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteIdleInput {
    pub policy_enabled: bool,
    pub evidence: IdleEvidence,
}

pub fn remote_idle_decision(input: &RemoteIdleInput) -> ApplyDecision {
    if !input.policy_enabled {
        return ApplyDecision::Wait {
            reason: "hostPolicyOff".into(),
        };
    }
    if !input.evidence.observation_fresh {
        return ApplyDecision::Wait {
            reason: "staleObservation".into(),
        };
    }
    if input.evidence.unknown_work {
        return ApplyDecision::Refuse {
            reason: "unknownWork".into(),
        };
    }
    // Heartbeat expiry is connectivity, not idle.
    if input.evidence.heartbeat_expired && !input.evidence.observation_fresh {
        return ApplyDecision::Wait {
            reason: "heartbeatExpiredIsNotIdle".into(),
        };
    }
    if input.evidence.heartbeat_expired {
        return ApplyDecision::Refuse {
            reason: "heartbeatExpiredIsNotIdle".into(),
        };
    }
    if !input.evidence.proxy_idle
        || !input.evidence.native_sessions_idle
        || !input.evidence.tools_idle
        || !input.evidence.approvals_idle
        || input.evidence.open_turns > 0
    {
        return ApplyDecision::Wait {
            reason: "notIdle".into(),
        };
    }
    ApplyDecision::Allow
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorePromoteInput {
    pub pending_verified: bool,
    pub bridge_compatible: bool,
    pub official_protocol_ok: bool,
    pub attestation_ok: bool,
    pub user_turn_accepted: bool,
}

pub fn core_may_promote(input: &CorePromoteInput) -> ApplyDecision {
    if !input.pending_verified {
        return ApplyDecision::Refuse {
            reason: "pendingUnverified".into(),
        };
    }
    if !input.bridge_compatible {
        return ApplyDecision::Refuse {
            reason: "bridgeIncompatible".into(),
        };
    }
    if !input.official_protocol_ok {
        return ApplyDecision::Refuse {
            reason: "officialProtocolIncompatible".into(),
        };
    }
    if !input.attestation_ok {
        if input.user_turn_accepted {
            return ApplyDecision::Refuse {
                reason: "attestationFailedAfterTurn".into(),
            };
        }
        return ApplyDecision::Refuse {
            reason: "attestationFailedBeforeTurn".into(),
        };
    }
    ApplyDecision::Allow
}

/// Finishing a download must not clear restart reasons. The function is the
/// shipped predicate used by the engine after `stage`.
pub fn download_does_not_clear_restart_reasons(
    before: &[String],
    after: &[String],
    after_phase: UpdatePhase,
) -> bool {
    let staged = matches!(
        after_phase,
        UpdatePhase::WaitingForRestart | UpdatePhase::WaitingForIdle | UpdatePhase::Staged
    );
    staged && before == after
}

/// Preview-only Enhanced idle handoff. Default off. `openTurns = 0` is never
/// sufficient without a core-reported durable/handoff-ready signal.
pub fn idle_handoff_decision(enabled: bool, evidence: &IdleEvidence) -> ApplyDecision {
    if !enabled {
        return ApplyDecision::Wait {
            reason: "nextCoreStart".into(),
        };
    }
    match evidence.durable_handoff_ready {
        Some(true)
            if evidence.proxy_idle
                && evidence.tools_idle
                && evidence.approvals_idle
                && evidence.native_sessions_idle
                && evidence.open_turns == 0 =>
        {
            ApplyDecision::Allow
        }
        Some(true) => ApplyDecision::Wait {
            reason: "notIdle".into(),
        },
        Some(false) | None => ApplyDecision::Wait {
            reason: "missingDurableHandoff".into(),
        },
    }
}

pub fn wait_kind_for(component: super::UpdateComponent) -> super::machine::WaitKind {
    match component {
        super::UpdateComponent::Desktop => super::machine::WaitKind::Restart,
        super::UpdateComponent::Remote => super::machine::WaitKind::Idle,
        super::UpdateComponent::Core => super::machine::WaitKind::Restart,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updates::UpdateComponent;

    #[test]
    fn open_ui_never_auto_closes_for_desktop() {
        let decision = desktop_may_apply(&DesktopApplyInput {
            ui_open: true,
            proxy_active_requests: 0,
            core_in_progress: false,
            observation_available: true,
        });
        assert_eq!(
            decision,
            ApplyDecision::Wait {
                reason: "waitingForRestart".into()
            }
        );
    }

    #[test]
    fn unknown_proxy_work_keeps_desktop_staged() {
        let decision = desktop_may_apply(&DesktopApplyInput {
            ui_open: false,
            proxy_active_requests: 0,
            core_in_progress: false,
            observation_available: false,
        });
        assert_eq!(
            decision,
            ApplyDecision::Wait {
                reason: "observationUnavailable".into()
            }
        );
    }

    #[test]
    fn heartbeat_expiry_is_not_remote_idle() {
        let decision = remote_idle_decision(&RemoteIdleInput {
            policy_enabled: true,
            evidence: IdleEvidence {
                proxy_idle: true,
                native_sessions_idle: true,
                tools_idle: true,
                approvals_idle: true,
                heartbeat_expired: true,
                observation_fresh: true,
                unknown_work: false,
                open_turns: 0,
                durable_handoff_ready: None,
            },
        });
        assert_eq!(
            decision,
            ApplyDecision::Refuse {
                reason: "heartbeatExpiredIsNotIdle".into()
            }
        );
    }

    #[test]
    fn new_turn_or_approval_blocks_remote() {
        let mut evidence = IdleEvidence {
            proxy_idle: true,
            native_sessions_idle: true,
            tools_idle: true,
            approvals_idle: false,
            heartbeat_expired: false,
            observation_fresh: true,
            unknown_work: false,
            open_turns: 0,
            durable_handoff_ready: None,
        };
        assert!(matches!(
            remote_idle_decision(&RemoteIdleInput {
                policy_enabled: true,
                evidence: evidence.clone(),
            }),
            ApplyDecision::Wait { .. }
        ));
        evidence.approvals_idle = true;
        evidence.open_turns = 1;
        assert!(matches!(
            remote_idle_decision(&RemoteIdleInput {
                policy_enabled: true,
                evidence,
            }),
            ApplyDecision::Wait { .. }
        ));
    }

    #[test]
    fn core_failed_after_turn_does_not_silently_switch_planes() {
        let decision = core_may_promote(&CorePromoteInput {
            pending_verified: true,
            bridge_compatible: true,
            official_protocol_ok: true,
            attestation_ok: false,
            user_turn_accepted: true,
        });
        assert_eq!(
            decision,
            ApplyDecision::Refuse {
                reason: "attestationFailedAfterTurn".into()
            }
        );
    }

    #[test]
    fn idle_handoff_requires_core_reported_durable_ready() {
        let idle = IdleEvidence {
            proxy_idle: true,
            native_sessions_idle: true,
            tools_idle: true,
            approvals_idle: true,
            heartbeat_expired: false,
            observation_fresh: true,
            unknown_work: false,
            open_turns: 0,
            durable_handoff_ready: None,
        };
        assert_eq!(
            idle_handoff_decision(true, &idle),
            ApplyDecision::Wait {
                reason: "missingDurableHandoff".into()
            }
        );
        assert_eq!(
            idle_handoff_decision(false, &idle),
            ApplyDecision::Wait {
                reason: "nextCoreStart".into()
            }
        );
    }

    #[test]
    fn download_complete_keeps_restart_reasons() {
        let reasons = vec!["catalogRestored".into()];
        assert!(download_does_not_clear_restart_reasons(
            &reasons,
            &reasons,
            UpdatePhase::WaitingForRestart
        ));
        assert!(wait_kind_for(UpdateComponent::Desktop).as_restart());
    }
}
