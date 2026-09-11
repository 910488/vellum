//! Durable session health summary (M34).
//!
//! A compact per-host view of remote sessions read **only** from the native
//! Codex app-server control API through the agent (`codex.sessionStatus`).
//! The Broker's connection snapshot is deliberately not used: a native thread
//! that survives a detached desktop must still be visible, and when the
//! native capability is unavailable the summary reports that explicitly
//! instead of fabricating thread detail.

use serde::{Deserialize, Serialize};

use crate::error::AppResult;
use crate::remote::RemoteHostAggregateStatus;
use crate::state::AppState;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSessionSummary {
    pub host_id: String,
    pub manager_state: String,
    pub detached_ready: bool,
    /// `nativeAppServer` when the daemon answered, `unsupported` when the
    /// capability is unavailable, `daemonDown` when no daemon is running.
    pub observability: String,
    pub threads: Vec<RemoteThreadSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteThreadSummary {
    pub thread_id: String,
    pub status: String,
    pub active_turn: bool,
    pub active_turn_id: Option<String>,
    pub turn_count: u64,
    pub last_turn_status: Option<String>,
}

pub fn summarize(state: &AppState, host_id: &str) -> AppResult<RemoteSessionSummary> {
    let status: RemoteHostAggregateStatus =
        crate::remote::RemoteHostManager::aggregate_status(state, host_id)?;
    let detached_ready = status.manager_state == "detachedReady";
    let target = crate::remote::RemoteHostManager::resolve_target(state, host_id)?;
    let client = crate::remote::RemoteAgentClient::new(target);

    // M35: `aggregate_status` above already populated (or reused) this
    // host's cached `host.managerSnapshot`, which already includes the
    // native session summary — read it instead of a second
    // `codex.sessionStatus` SSH call. `session: null` in the snapshot is a
    // real answer (daemon not running / observability unsupported), not a
    // missing value, so only fall back to the direct RPC when no snapshot
    // was cached at all (older agent that doesn't support the snapshot
    // method, so `aggregate_status` itself fell back to the legacy path).
    let session_status = match state.remote().cached_snapshot(host_id) {
        Some(snapshot) => Ok(snapshot
            .get("session")
            .cloned()
            .unwrap_or(serde_json::Value::Null)),
        None => client.codex_session_status(None),
    };
    let Ok(session_status) = session_status else {
        return Ok(RemoteSessionSummary {
            host_id: host_id.into(),
            manager_state: status.manager_state,
            detached_ready,
            observability: "unsupported".into(),
            threads: Vec::new(),
        });
    };
    let daemon_running = session_status
        .pointer("/daemonRunning")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let observability = if !daemon_running {
        "daemonDown"
    } else {
        session_status
            .pointer("/observability")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unsupported")
    };
    let threads = session_status
        .pointer("/threads")
        .and_then(serde_json::Value::as_array)
        .map(|threads| {
            threads
                .iter()
                .map(|thread| RemoteThreadSummary {
                    thread_id: thread
                        .get("threadId")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    status: thread
                        .get("status")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    active_turn: thread
                        .get("active")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                    active_turn_id: thread
                        .get("activeTurnId")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                    turn_count: thread
                        .get("turnCount")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    last_turn_status: thread
                        .get("lastTurnStatus")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(RemoteSessionSummary {
        host_id: host_id.into(),
        manager_state: status.manager_state,
        detached_ready,
        observability: observability.into(),
        threads,
    })
}
