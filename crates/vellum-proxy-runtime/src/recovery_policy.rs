//! Runtime recovery policy — independent of the compaction engine.
//!
//! Task Stall, Task Efficiency, and Investigation Recovery used to be gated on
//! `canonical_engine == V2`, which tied three unrelated behaviors to whichever
//! compaction engine a route happened to be configured for. Switching
//! compaction engines then silently switched recovery off. They are now gated
//! on their own policy, resolved from the route's provider kind:
//!
//! * OpenAI Official keeps its original behavior and receives no
//!   Vellum-injected recovery.
//! * Every third-party route and Review keeps recovery.
//!
//! Compaction does not consume, advance, or clear recovery. A compaction
//! trigger is an auxiliary call, not a turn; the next ordinary turn re-applies
//! at most one recovery overlay.

use serde::{Deserialize, Serialize};

use crate::route::{RuntimeModelRoute, RuntimeProviderKind};

/// Which Vellum recovery behaviors apply to a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeRecoveryPolicy {
    /// Detect a task that has stopped making progress.
    pub task_stall: bool,
    /// Detect repeated, non-advancing action loops.
    pub task_efficiency: bool,
    /// Detect and redirect re-investigation of settled questions.
    pub investigation_recovery: bool,
}

impl RuntimeRecoveryPolicy {
    /// No Vellum recovery at all. Official routes run exactly as the provider
    /// would run them.
    pub const DISABLED: Self = Self {
        task_stall: false,
        task_efficiency: false,
        investigation_recovery: false,
    };

    /// The full third-party recovery set.
    pub const ENABLED: Self = Self {
        task_stall: true,
        task_efficiency: true,
        investigation_recovery: true,
    };

    /// True when any recovery behavior is active.
    pub fn any(self) -> bool {
        self.task_stall || self.task_efficiency || self.investigation_recovery
    }
}

impl Default for RuntimeRecoveryPolicy {
    fn default() -> Self {
        Self::ENABLED
    }
}

/// Resolve the recovery policy for a route.
///
/// Deliberately not a function of the compaction engine: changing how a
/// conversation is compacted must not change whether it recovers.
pub fn resolve_recovery_policy(route: &RuntimeModelRoute) -> RuntimeRecoveryPolicy {
    match route.provider_kind {
        RuntimeProviderKind::Official => RuntimeRecoveryPolicy::DISABLED,
        _ => RuntimeRecoveryPolicy::ENABLED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route_with(kind: RuntimeProviderKind) -> RuntimeModelRoute {
        RuntimeModelRoute {
            route_id: "route-1".into(),
            catalog_id: "vlm-sample".into(),
            name: "Sample".into(),
            base_url: "http://127.0.0.1:0/v1".into(),
            provider_kind: kind,
            auth_kind: crate::route::RuntimeAuthKind::Bearer,
            wire: crate::route::RuntimeWireFormat::Responses,
            server_side_resume: false,
            streaming: true,
            reasoning: true,
            vision: false,
            upstream_model: "sample-model".into(),
            context_window: Some(128_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: Default::default(),
            tool_capabilities: Default::default(),
            credential_id: Some("sample-credential".into()),
            insecure_http_policy: crate::outbound::InsecureHttpPolicy::Deny,
            provider_profile: None,
            access_mode: None,
            chat_capabilities: Default::default(),
        }
    }

    #[test]
    fn official_gets_no_vellum_recovery() {
        let policy = resolve_recovery_policy(&route_with(RuntimeProviderKind::Official));
        assert_eq!(policy, RuntimeRecoveryPolicy::DISABLED);
        assert!(!policy.any());
    }

    #[test]
    fn third_party_keeps_every_recovery_behavior() {
        for kind in [
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeProviderKind::GrokCli,
        ] {
            let policy = resolve_recovery_policy(&route_with(kind));
            assert_eq!(policy, RuntimeRecoveryPolicy::ENABLED);
            assert!(policy.task_stall);
            assert!(policy.task_efficiency);
            assert!(policy.investigation_recovery);
        }
    }

    #[test]
    fn recovery_does_not_depend_on_the_compaction_engine() {
        // The whole point of this module: two routes that differ only in
        // compaction configuration must resolve to the same recovery policy.
        let mut a = route_with(RuntimeProviderKind::OpenAiCompatible);
        let mut b = route_with(RuntimeProviderKind::OpenAiCompatible);
        a.compaction_policy.threshold_percent = 60;
        b.compaction_policy.threshold_percent = 90;
        assert_eq!(resolve_recovery_policy(&a), resolve_recovery_policy(&b));
    }
}
