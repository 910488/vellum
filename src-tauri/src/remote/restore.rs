//! Safe one-click withdrawal of Vellum's native Codex lease.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::remote::{RemoteAgentClient, RemoteHostManager};
use crate::state::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OneClickRestoreResult {
    pub state: String,
    pub restore: Value,
    pub native_runtime: Option<Value>,
    pub proxy: Value,
    pub manager_state: String,
    pub completed_steps: Vec<String>,
}

pub fn one_click_restore<F>(
    state: &AppState,
    host_id: &str,
    operation_id: &str,
    mut progress: F,
) -> AppResult<OneClickRestoreResult>
where
    F: FnMut(&str, u8, &str),
{
    let client = RemoteAgentClient::new(RemoteHostManager::resolve_target(state, host_id)?);
    progress(
        "restorePreflight",
        5,
        "Checking native Codex turns and managed state",
    );
    let before = client.host_status()?;
    let sessions = client.codex_session_status(None)?;
    if active_turn_present(&before, &sessions) {
        return Err(AppError::Message(
            "OneClickRestoreBlocked: activeTurnInProgress; wait for the current Codex turn to finish"
                .into(),
        ));
    }

    progress(
        "restoreLease",
        30,
        "Restoring Codex config and model catalog lease",
    );
    let restored = client.codex_restore_native(&format!("{operation_id}-lease"))?;
    let manual_action_required = restored
        .get("manualActionRequired")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut completed_steps = vec!["codex.leaseRestored".into()];

    let daemon_running = before
        .pointer("/nativeCodex/daemonRunning")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let native_runtime = if daemon_running && !manual_action_required {
        progress(
            "restartNative",
            55,
            "Restarting native Codex with restored configuration",
        );
        completed_steps.push("codex.nativeRestarted".into());
        Some(client.codex_restart_native(&format!("{operation_id}-restart"))?)
    } else {
        completed_steps.push(if manual_action_required {
            "codex.restartSkippedForConflict".into()
        } else {
            "codex.nativeAlreadyStopped".into()
        });
        None
    };

    progress("stopProxy", 75, "Stopping the Vellum managed proxy");
    let proxy = client.proxy_stop(&format!("{operation_id}-proxy"))?;
    completed_steps.push("proxy.stopped".into());

    progress(
        "restoreVerification",
        95,
        "Verifying Vellum is no longer in the data path",
    );
    let verified = RemoteHostManager::aggregate_status(state, host_id)?;
    let proxy_running = verified
        .agent
        .as_ref()
        .and_then(|agent| agent.pointer("/proxy/running"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let active_lease = verified
        .agent
        .as_ref()
        .and_then(|agent| agent.get("managedProfiles"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|profile| {
            profile
                .pointer("/profile/profileId")
                .and_then(Value::as_str)
                == Some("codex-app-native")
                && matches!(
                    profile.pointer("/lease/state").and_then(Value::as_str),
                    Some("active" | "restartRequired" | "recoveryRequired")
                )
        });
    if proxy_running || active_lease {
        return Err(AppError::Message(format!(
            "OneClickRestoreVerificationFailed: proxyRunning={proxy_running}, activeNativeLease={active_lease}"
        )));
    }
    if manual_action_required {
        return Err(AppError::Message(format!(
            "OneClickRestoreConflict: {}",
            restored
                .get("conflicts")
                .cloned()
                .unwrap_or(Value::Array(Vec::new()))
        )));
    }

    progress(
        "restored",
        100,
        "Vellum managed routing has been safely withdrawn",
    );
    Ok(OneClickRestoreResult {
        state: "restored".into(),
        restore: restored,
        native_runtime,
        proxy,
        manager_state: verified.manager_state,
        completed_steps,
    })
}

/// Shared with `boundary_key::provision_remote_boundary_key`, which must
/// never force a native-Codex restart out from under a turn in progress.
pub(crate) fn active_turn_present(host: &Value, sessions: &Value) -> bool {
    host.pointer("/nativeCodex/activeTurn")
        .and_then(Value::as_bool)
        == Some(true)
        || sessions
            .get("threads")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|thread| {
                thread.get("active").and_then(Value::as_bool) == Some(true)
                    || thread
                        .get("activeTurnId")
                        .is_some_and(|turn_id| !turn_id.is_null())
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn restore_preflight_detects_only_structured_active_turns() {
        assert!(active_turn_present(
            &json!({}),
            &json!({"threads":[{"active":true,"activeTurnId":"turn-1"}]})
        ));
        assert!(!active_turn_present(
            &json!({"message":"activeTurnId in text is not state"}),
            &json!({"threads":[{"active":false,"activeTurnId":null}]})
        ));
    }

    #[test]
    #[ignore = "withdraws Vellum routing from an explicitly selected live SSH host"]
    fn live_one_click_restore_is_unmanaged() {
        let alias = std::env::var("VELLUM_LIVE_SSH_ALIAS").expect("live SSH alias");
        let state = AppState::new();
        let candidates =
            crate::remote::discovery::discover(&state.remote().list_hosts().unwrap()).unwrap();
        let candidate = candidates
            .into_iter()
            .find(|candidate| candidate.ssh_alias == alias)
            .expect("configured Codex/OpenSSH host");
        state.remote().import_discovered_host(&candidate).unwrap();
        let result = one_click_restore(
            &state,
            &candidate.vellum_host_id,
            &format!("live-restore-{}", ulid::Ulid::new()),
            |phase, percent, message| println!("{percent:3}% {phase}: {message}"),
        )
        .expect("live one-click restore");
        assert_eq!(result.state, "restored");
        assert_eq!(result.manager_state, "unmanaged");
    }
}
