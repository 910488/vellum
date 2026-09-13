//! Fail-closed observation gate for Update / Restart / Restore.
//!
//! Incomplete turn, tool, or approval observation blocks the destructive
//! operation. Active work also blocks.

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DestructiveObservation {
    pub turns_observed: bool,
    pub tools_observed: bool,
    pub approvals_observed: bool,
    pub active_turn: bool,
    pub active_tools: bool,
    pub pending_approvals: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestructiveDecision {
    Allow,
    Block { code: &'static str },
}

pub fn destructive_op_decision(obs: &DestructiveObservation) -> DestructiveDecision {
    if !obs.turns_observed || !obs.tools_observed || !obs.approvals_observed {
        return DestructiveDecision::Block {
            code: "incompleteObservation",
        };
    }
    if obs.active_turn || obs.active_tools || obs.pending_approvals {
        return DestructiveDecision::Block {
            code: "activeWorkInProgress",
        };
    }
    DestructiveDecision::Allow
}

pub fn observation_from_session(
    session: Option<&serde_json::Value>,
    native: Option<&serde_json::Value>,
) -> DestructiveObservation {
    let Some(session) = session else {
        return DestructiveObservation::default();
    };
    if session.get("observability").and_then(|v| v.as_str()) != Some("nativeAppServer") {
        return DestructiveObservation::default();
    }
    let threads = session.get("threads").and_then(|v| v.as_array());
    let Some(threads) = threads else {
        return DestructiveObservation::default();
    };
    let active_turn = threads.iter().any(|thread| {
        thread.get("active").and_then(|v| v.as_bool()) == Some(true)
            || thread.get("activeTurnId").is_some_and(|id| !id.is_null())
    }) || native
        .and_then(|value| value.get("activeTurn"))
        .and_then(|v| v.as_bool())
        == Some(true);
    let tools_field = threads
        .iter()
        .filter_map(|thread| thread.get("activeTools").and_then(|v| v.as_bool()))
        .collect::<Vec<_>>();
    let approvals_field = threads
        .iter()
        .filter_map(|thread| thread.get("pendingApprovals").and_then(|v| v.as_bool()))
        .collect::<Vec<_>>();
    DestructiveObservation {
        turns_observed: true,
        tools_observed: tools_field.len() == threads.len(),
        approvals_observed: approvals_field.len() == threads.len(),
        active_turn,
        active_tools: tools_field.iter().any(|value| *value),
        pending_approvals: approvals_field.iter().any(|value| *value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_turn_tool_or_approval_observation_blocks() {
        let mut obs = DestructiveObservation {
            turns_observed: true,
            tools_observed: true,
            approvals_observed: true,
            ..DestructiveObservation::default()
        };
        assert_eq!(destructive_op_decision(&obs), DestructiveDecision::Allow);
        obs.tools_observed = false;
        assert_eq!(
            destructive_op_decision(&obs),
            DestructiveDecision::Block {
                code: "incompleteObservation"
            }
        );
        obs.tools_observed = true;
        obs.active_turn = true;
        assert_eq!(
            destructive_op_decision(&obs),
            DestructiveDecision::Block {
                code: "activeWorkInProgress"
            }
        );
    }

    #[test]
    fn missing_session_is_incomplete_not_idle() {
        let obs = observation_from_session(None, None);
        assert_eq!(
            destructive_op_decision(&obs),
            DestructiveDecision::Block {
                code: "incompleteObservation"
            }
        );
        let session = serde_json::json!({
            "observability": "nativeAppServer",
            "threads": [{
                "threadId": "t1",
                "active": false,
                "activeTurnId": null,
                "activeTools": false,
                "pendingApprovals": false
            }]
        });
        let obs = observation_from_session(Some(&session), None);
        assert_eq!(destructive_op_decision(&obs), DestructiveDecision::Allow);
        let partial = serde_json::json!({
            "observability": "nativeAppServer",
            "threads": [{ "threadId": "t1", "active": false }]
        });
        let obs = observation_from_session(Some(&partial), None);
        assert_eq!(
            destructive_op_decision(&obs),
            DestructiveDecision::Block {
                code: "incompleteObservation"
            }
        );
    }
}
