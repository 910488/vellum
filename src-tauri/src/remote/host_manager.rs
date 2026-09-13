//! Desktop-side remote host manager coordination surface.
//!
//! Aggregates remote-agent lifecycle status and existing broker host records.
//! Renderer surfaces may pass only hostId; trusted SSH/process metadata is
//! resolved from the local CachedHost store.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::remote::agent_client::{RemoteAgentClient, ResolvedAgentTarget};
use crate::remote::local_cache::CachedHost;
use crate::state::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteHostAggregateStatus {
    pub host_id: String,
    pub manager_state: String,
    pub cached_host: Option<CachedHost>,
    pub agent: Option<Value>,
    pub agent_error: Option<String>,
    pub available_actions: Vec<String>,
    pub blocked_reasons: Vec<String>,
    /// M30: versioned host inventory from `host.inventoryV2`, when the agent
    /// supports it. Older agents yield `None` and the UI keeps the legacy view.
    #[serde(default)]
    pub inventory: Option<Value>,
    /// Safe native ChatGPT identity/synchronization state. Never contains
    /// access or refresh tokens.
    #[serde(default)]
    pub chatgpt: Option<Value>,
}

pub struct RemoteHostManager;

fn manager_state(
    detached_ready: bool,
    native_running: bool,
    proxy_running: bool,
    proxy_ready: bool,
    active_references: bool,
) -> &'static str {
    if !active_references && !proxy_running {
        "unmanaged"
    } else if detached_ready && proxy_ready && active_references {
        "detachedReady"
    } else if native_running && proxy_ready && active_references {
        "nativeActive"
    } else if proxy_ready {
        "readyToPlan"
    } else {
        "drifted"
    }
}

/// `HostManagerSnapshot`'s optional fields serialize as explicit `null`
/// rather than being omitted; treat that the same as "the probe returned
/// nothing" so snapshot-sourced values match the `.ok()`-on-RPC-failure
/// shape the legacy per-field calls produced.
fn non_null(value: Option<&Value>) -> Option<Value> {
    value.filter(|value| !value.is_null()).cloned()
}

fn manager_snapshot_method_not_found(error: &AppError) -> bool {
    let message = error.to_string();
    message.contains("agent.methodNotFound: host.managerSnapshot")
        // Compatibility with pre-category agents: serde's exact unknown enum
        // variant proves this method, rather than the SSH connection, failed.
        || message.contains("unknown variant `host.managerSnapshot`")
        || message.contains("unknown variant 'host.managerSnapshot'")
}

fn native_detached_ready(agent: &Value) -> bool {
    let daemon = agent
        .pointer("/nativeCodex/daemonRunning")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && agent
            .pointer("/nativeCodex/cliLauncher/ready")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    if !daemon {
        return false;
    }
    let os = agent
        .pointer("/capabilities/os")
        .and_then(Value::as_str)
        .unwrap_or("linux");
    if crate::remote::platform::normalize_os(os) == Some("darwin") {
        agent
            .pointer("/capabilities/guiSessionAvailable")
            .and_then(Value::as_bool)
            .unwrap_or(true)
            && agent
                .pointer("/capabilities/persistenceScope")
                .and_then(Value::as_str)
                == Some(crate::remote::platform::PERSISTENCE_LOGIN)
    } else {
        agent
            .pointer("/capabilities/lingerEnabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && agent
                .pointer("/capabilities/userSystemdAvailable")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    }
}

impl RemoteHostManager {
    pub fn list_hosts(state: &AppState) -> AppResult<Vec<CachedHost>> {
        state.remote().list_hosts()
    }

    pub fn resolve_target(state: &AppState, host_id: &str) -> AppResult<ResolvedAgentTarget> {
        let host = state
            .remote()
            .list_hosts()?
            .into_iter()
            .find(|host| host.id == host_id)
            .ok_or_else(|| AppError::Message(format!("unknown remote host: {host_id}")))?;
        Ok(ResolvedAgentTarget::from_cached_host(&host))
    }

    pub fn probe_agent(state: &AppState, host_id: &str) -> AppResult<Value> {
        let target = Self::resolve_target(state, host_id)?;
        RemoteAgentClient::new(target).host_status()
    }

    pub fn support_bundle(state: &AppState, host_id: &str) -> AppResult<Value> {
        let target = Self::resolve_target(state, host_id)?;
        let client = RemoteAgentClient::new(target);
        client.agent_version()?;
        client.support_bundle()
    }

    pub fn aggregate_status(
        state: &AppState,
        host_id: &str,
    ) -> AppResult<RemoteHostAggregateStatus> {
        let cached_host = state
            .remote()
            .list_hosts()?
            .into_iter()
            .find(|host| host.id == host_id);
        let Some(host) = cached_host.clone() else {
            return Ok(RemoteHostAggregateStatus {
                host_id: host_id.to_string(),
                manager_state: "unmanaged".into(),
                cached_host: None,
                agent: None,
                agent_error: Some(format!("unknown remote host: {host_id}")),
                available_actions: Vec::new(),
                blocked_reasons: vec!["hostNotConfigured".into()],
                inventory: None,
                chatgpt: None,
            });
        };
        let target = ResolvedAgentTarget::from_cached_host(&host);
        let client = RemoteAgentClient::new(target);
        let expected_account_id = super::desktop_control_account_id(state);

        // M35: prefer the single-round-trip snapshot — one SSH process for
        // status + inventory + account together, instead of the three
        // sequential ones below. Older agents that predate
        // `host.managerSnapshot` answer an unknown-method error here; fall
        // back to the original sequential RPCs so this keeps working
        // against an older/unpinned remote agent.
        let snapshot = state.remote().host_manager_snapshot(host_id, || {
            client.host_manager_snapshot(expected_account_id.as_deref(), None)
        });
        match snapshot {
            Ok((snapshot, _generation)) => {
                let agent = snapshot.get("status").cloned().unwrap_or(Value::Null);
                let inventory = snapshot.get("inventory").cloned();
                let chatgpt = non_null(snapshot.get("account"));
                return Ok(Self::compose_status(
                    host,
                    cached_host,
                    agent,
                    inventory,
                    chatgpt,
                ));
            }
            Err(error) if manager_snapshot_method_not_found(&error) => {
                // Compatibility only: an old agent has no snapshot method.
            }
            Err(error) => {
                // Network, timeout, and authentication failures must not fan
                // out into three more doomed SSH processes. The renderer
                // retains its last-good status and marks this refresh stale.
                return Err(error);
            }
        }
        match client.host_status() {
            Ok(agent) => {
                // Older agents reject `host.inventoryV2`; fall back silently.
                let inventory = client.host_inventory_v2().ok();
                let chatgpt = client
                    .codex_account_status(expected_account_id.as_deref())
                    .ok();
                Ok(Self::compose_status(
                    host,
                    cached_host,
                    agent,
                    inventory,
                    chatgpt,
                ))
            }
            Err(error) => Ok(RemoteHostAggregateStatus {
                host_id: host.id.clone(),
                manager_state: "unmanaged".into(),
                cached_host,
                agent: None,
                agent_error: Some(error.to_string()),
                available_actions: Vec::new(),
                blocked_reasons: vec!["agentUnavailable".into()],
                inventory: None,
                chatgpt: None,
            }),
        }
    }

    /// Derive manager state / available actions / blocked reasons from one
    /// `host.status`-shaped agent response, whether it arrived via the
    /// single `host.managerSnapshot` round trip or the legacy sequential
    /// RPCs. Kept as one function so the two callers can never drift.
    fn compose_status(
        host: CachedHost,
        cached_host: Option<CachedHost>,
        agent: Value,
        inventory: Option<Value>,
        chatgpt: Option<Value>,
    ) -> RemoteHostAggregateStatus {
        {
            let native_compatible = agent
                .pointer("/nativeCodex/compatible")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let native_running = agent
                .pointer("/nativeCodex/daemonRunning")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let detached_ready = native_detached_ready(&agent);
            let running = agent
                .pointer("/proxy/running")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let present = agent
                .pointer("/proxy/present")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let installed = present
                || agent
                    .pointer("/proxy/installId")
                    .is_some_and(|value| !value.is_null());
            let ready = agent
                .pointer("/proxy/ready")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let configured = agent
                .pointer("/configuration/present")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let credentials_ready = agent
                .pointer("/configuration/credentialsReady")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let profiles = agent
                .get("managedProfiles")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let runtime_ready = profiles.iter().any(|profile| {
                profile
                    .pointer("/runtime/ready")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            });
            let restart_required = profiles.iter().any(|profile| {
                profile.pointer("/lease/state").and_then(Value::as_str) == Some("restartRequired")
            });
            let runtime_recovery_required = profiles.iter().any(|profile| {
                profile.pointer("/lease/state").and_then(Value::as_str) == Some("active")
                    && !profile
                        .pointer("/runtime/ready")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
            });
            let active_references = profiles.iter().any(|profile| {
                matches!(
                    profile.pointer("/lease/state").and_then(Value::as_str),
                    Some("active" | "restartRequired" | "recoveryRequired")
                )
            });
            let injectable_profile = profiles.iter().any(|profile| {
                !profile
                    .pointer("/lease/present")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            });
            let active_stopped_profile = profiles.iter().any(|profile| {
                profile.pointer("/lease/state").and_then(Value::as_str) == Some("active")
                    && !profile
                        .pointer("/runtime/ready")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
            });
            let mut available_actions = vec!["inspect".into(), "doctor".into()];
            let mut blocked_reasons = Vec::new();
            if let Some(inventory) = &inventory {
                for blocker in inventory
                    .pointer("/blockers")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(code) = blocker.get("code").and_then(Value::as_str) {
                        blocked_reasons.push(code.to_string());
                    }
                }
                for action in inventory
                    .pointer("/availableActions")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(action) = action.as_str() {
                        available_actions.push(action.to_string());
                    }
                }
            }
            if agent
                .pointer("/proxy/lastError")
                .is_some_and(|value| !value.is_null())
            {
                available_actions.push("repair".into());
            }
            if running {
                if !active_references {
                    available_actions.push("stop".into());
                }
                // A running proxy referenced by the native managed profile is the
                // normal steady state, not a deployment blocker. `deployment::apply`
                // owns the safe transition: temporarily restore the lease, stop the
                // proxy only when its configuration changes, then start and re-adopt.
                // Keep the standalone `stop` action unavailable while referenced,
                // but do not tell the renderer that Plan/Apply is blocked.
            } else {
                if !installed {
                    available_actions.push("install".into());
                } else {
                    available_actions.push("configure".into());
                    if configured && credentials_ready {
                        available_actions.push("start".into());
                    }
                }
            }
            if ready {
                available_actions.push("createManagedProfile".into());
                if injectable_profile {
                    available_actions.push("inject".into());
                }
                if active_stopped_profile {
                    available_actions.push("startManaged".into());
                }
            } else {
                blocked_reasons.push("injectionRequiresReadyProxy".into());
            }
            if active_references {
                available_actions.push("restore".into());
            }
            if runtime_ready {
                available_actions.extend(["connect".into(), "stopManaged".into()]);
            }
            if !configured {
                blocked_reasons.push("proxyConfigurationMissing".into());
            } else if !credentials_ready {
                blocked_reasons.push("credentialsMissing".into());
            }
            if restart_required {
                available_actions.push("repair".into());
                blocked_reasons.push("codexRestartRequired".into());
            }
            if runtime_recovery_required {
                available_actions.push("repair".into());
                blocked_reasons.push("managedRuntimeRecoveryRequired".into());
            }
            if native_compatible {
                available_actions.extend([
                    "planDeployment".into(),
                    "restartNative".into(),
                    "detach".into(),
                ]);
            } else {
                blocked_reasons.push("nativeCodexVersionMismatch".into());
            }
            available_actions.sort();
            available_actions.dedup();
            blocked_reasons.sort();
            blocked_reasons.dedup();
            if let Some(account) = &chatgpt {
                match account.get("state").and_then(Value::as_str) {
                    Some("pairingRequired") => {
                        blocked_reasons.push("officialAccountPairingRequired".into())
                    }
                    Some("activationRequired") => {
                        blocked_reasons.push("officialAccountActivationRequired".into())
                    }
                    Some("desktopAccountUnavailable") => {
                        blocked_reasons.push("desktopOfficialAccountMissing".into())
                    }
                    _ => {}
                }
            }
            blocked_reasons.sort();
            blocked_reasons.dedup();
            RemoteHostAggregateStatus {
                host_id: host.id.clone(),
                manager_state: manager_state(
                    detached_ready,
                    native_running,
                    running,
                    ready,
                    active_references,
                )
                .into(),
                cached_host,
                agent: Some(agent),
                agent_error: None,
                available_actions,
                blocked_reasons,
                inventory,
                chatgpt,
            }
        }
    }

    pub fn start_proxy(
        state: &AppState,
        host_id: &str,
        operation_id: &str,
        host_port: Option<u16>,
        image: Option<&str>,
    ) -> AppResult<Value> {
        let target = Self::resolve_target(state, host_id)?;
        let result = RemoteAgentClient::new(target).proxy_start(operation_id, host_port, image);
        Self::invalidate_snapshot_on_success(state, host_id, &result);
        result
    }

    pub fn install_proxy(
        state: &AppState,
        host_id: &str,
        operation_id: &str,
        image: &str,
        image_digest: Option<&str>,
    ) -> AppResult<Value> {
        let target = Self::resolve_target(state, host_id)?;
        let result =
            RemoteAgentClient::new(target).proxy_install(operation_id, image, image_digest);
        Self::invalidate_snapshot_on_success(state, host_id, &result);
        result
    }

    pub fn configure_proxy(
        state: &AppState,
        host_id: &str,
        operation_id: &str,
        config_toml: &str,
    ) -> AppResult<Value> {
        let target = Self::resolve_target(state, host_id)?;
        let result = RemoteAgentClient::new(target).proxy_configure(operation_id, config_toml);
        Self::invalidate_snapshot_on_success(state, host_id, &result);
        result
    }

    pub fn stop_proxy(state: &AppState, host_id: &str, operation_id: &str) -> AppResult<Value> {
        let target = Self::resolve_target(state, host_id)?;
        let result = RemoteAgentClient::new(target).proxy_stop(operation_id);
        Self::invalidate_snapshot_on_success(state, host_id, &result);
        result
    }

    /// Cached `host.managerSnapshot` state is now provably stale after a
    /// successful mutation — drop it so the very next probe (the operation's
    /// own completion poll, or the renderer's next refresh) hits the network
    /// instead of replaying pre-mutation state for up to `SNAPSHOT_CACHE_TTL`.
    /// A failed mutation leaves the cache alone: nothing changed on the host.
    fn invalidate_snapshot_on_success(state: &AppState, host_id: &str, result: &AppResult<Value>) {
        if result.is_ok() {
            state.remote().invalidate_snapshot(host_id);
        }
    }

    pub fn repair(state: &AppState, host_id: &str, operation_id: &str) -> AppResult<Value> {
        // Repair mutates host state (may restart the daemon, reconcile
        // profiles, etc.) — invalidate up front rather than trying to catch
        // every return path below with a success check.
        state.remote().invalidate_snapshot(host_id);
        let target = Self::resolve_target(state, host_id)?;
        let client = RemoteAgentClient::new(target);
        client.agent_version()?;
        let before = client.host_status()?;
        crate::remote::provision_remote_boundary_key(
            &client,
            &state.data_root(),
            host_id,
            operation_id,
        )?;
        let native = if before
            .pointer("/nativeCodex/standaloneInstalled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            Some(client.codex_bootstrap_native(&format!("{operation_id}-codex-launcher"))?)
        } else {
            None
        };
        let proxy = client.repair(&format!("{operation_id}-proxy"))?;
        let mut runtimes = Vec::new();
        for profile_id in managed_profiles_needing_app_server_restart(
            before
                .get("managedProfiles")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        ) {
            // Restart the Codex app-server only. Never pass a broker port:
            // production start_managed must not bind the legacy listener.
            runtimes.push(client.codex_start_managed(
                &format!("{operation_id}-runtime-{profile_id}"),
                &profile_id,
                None,
            )?);
        }
        crate::remote::confirm_remote_boundary_key_consumers(
            &client,
            &state.data_root(),
            host_id,
            &[
                crate::proxy::BoundaryKeyConsumer::Proxy,
                crate::proxy::BoundaryKeyConsumer::NativeCodex,
            ],
            false,
        )?;
        Ok(serde_json::json!({
            "status": "repaired",
            "native": native,
            "proxy": proxy,
            "managedRuntimes": runtimes,
            "after": client.host_status()?,
        }))
    }
}

/// Active leases whose Codex app-server is not ready. Broker listener
/// state is ignored — production repair restarts the app-server only.
pub(crate) fn managed_profiles_needing_app_server_restart(profiles: &[Value]) -> Vec<String> {
    profiles
        .iter()
        .filter(|profile| {
            profile.pointer("/lease/state").and_then(Value::as_str) == Some("active")
                && !profile
                    .pointer("/runtime/ready")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        })
        .filter_map(|profile| {
            profile
                .pointer("/profile/profileId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

#[cfg(test)]
mod manager_state_tests {
    use super::{
        managed_profiles_needing_app_server_restart, manager_state, native_detached_ready,
    };
    use serde_json::json;

    #[test]
    fn restored_host_with_agent_but_no_proxy_or_lease_is_unmanaged() {
        assert_eq!(
            manager_state(false, false, false, false, false),
            "unmanaged"
        );
    }

    #[test]
    fn active_lease_with_stopped_proxy_is_drifted() {
        assert_eq!(manager_state(false, true, false, false, true), "drifted");
    }

    #[test]
    fn unreferenced_ready_proxy_remains_ready_to_plan() {
        assert_eq!(
            manager_state(false, false, true, true, false),
            "readyToPlan"
        );
    }

    #[test]
    fn repair_restarts_unready_app_server_and_ignores_broker() {
        let profiles = vec![
            json!({
                "profile": {"profileId": "needs-app-server"},
                "lease": {"state": "active"},
                "runtime": {"ready": false, "brokerActive": false, "brokerPort": 45100}
            }),
            json!({
                "profile": {"profileId": "app-server-up-broker-down"},
                "lease": {"state": "active"},
                "runtime": {"ready": true, "brokerActive": false, "brokerPort": 45100}
            }),
            json!({
                "profile": {"profileId": "inactive"},
                "lease": {"state": "idle"},
                "runtime": {"ready": false, "brokerActive": true}
            }),
        ];
        assert_eq!(
            managed_profiles_needing_app_server_restart(&profiles),
            vec!["needs-app-server".to_string()]
        );
    }

    #[test]
    fn detach_readiness_requires_the_codex_app_login_shell_launcher() {
        let mut agent = json!({
            "nativeCodex": {"daemonRunning": true},
            "capabilities": {"lingerEnabled": true, "userSystemdAvailable": true}
        });
        assert!(!native_detached_ready(&agent));
        agent["nativeCodex"]["cliLauncher"] = json!({"ready": true});
        assert!(native_detached_ready(&agent));
    }
}

fn grok_remote_credential_secret(
    credential: crate::grok_auth::GrokCredential,
    account_home: &std::path::Path,
    external_home: &std::path::Path,
) -> AppResult<String> {
    let client_version = crate::grok_auth::client_version(account_home)
        .or_else(|| crate::grok_auth::client_version(external_home))
        .or_else(|| crate::grok_auth::cli_version(external_home))
        .ok_or_else(|| AppError::Message("Grok client version metadata is missing".into()))?;
    let agent_id = crate::grok_auth::agent_id(account_home)
        .or_else(|| crate::grok_auth::agent_id(external_home));
    let user_id = credential
        .user_id
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AppError::Message("Grok user identity metadata is missing".into()))?;
    let mut secret = serde_json::json!({
        "accessToken": credential.access_token.as_str(),
        "clientVersion": client_version,
        "userId": user_id,
    });
    if let Some(agent_id) = agent_id {
        secret["agentId"] = Value::String(agent_id);
    }
    serde_json::to_string(&secret)
        .map_err(|error| AppError::Message(format!("failed encoding Grok credential: {error}")))
}

pub(crate) fn grok_remote_credential_secret_for_deployment(
    credential: crate::grok_auth::GrokCredential,
    account_home: &std::path::Path,
    external_home: &std::path::Path,
) -> AppResult<String> {
    grok_remote_credential_secret(credential, account_home, external_home)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zeroize::Zeroizing;

    fn fixture_host() -> CachedHost {
        CachedHost {
            id: "host-1".into(),
            name: "test host".into(),
            ssh_alias: "vellum-test-host".into(),
            broker_url: "wss://broker.test/ws".into(),
            device_id: "device-1".into(),
            device_token: None,
            last_thread_id: None,
            last_ack_seq: 0,
            cursor_scheme: "v1".into(),
        }
    }

    /// `compose_status` must produce the exact same shape whether its inputs
    /// came from the new single-round-trip `host.managerSnapshot` or the
    /// legacy three separate RPCs — this is the pure derivation both paths
    /// share, exercised directly so a future edit to one can't silently
    /// diverge from the other.
    #[test]
    fn compose_status_flags_account_pairing_state_as_a_blocked_reason() {
        let host = fixture_host();
        let agent = json!({
            "nativeCodex": {"compatible": true, "daemonRunning": true},
            "proxy": {"running": true, "present": true, "ready": true},
            "configuration": {"present": true, "credentialsReady": true},
            "managedProfiles": [],
        });
        let inventory = json!({"blockers": [], "availableActions": []});
        let chatgpt = json!({"state": "pairingRequired"});

        let status = RemoteHostManager::compose_status(
            host.clone(),
            Some(host),
            agent,
            Some(inventory),
            Some(chatgpt),
        );

        assert_eq!(status.host_id, "host-1");
        assert!(status
            .blocked_reasons
            .contains(&"officialAccountPairingRequired".to_string()));
        assert!(status.agent_error.is_none());
        assert!(status.agent.is_some());
    }

    #[test]
    fn compose_status_never_fabricates_account_state_when_none_was_probed() {
        let host = fixture_host();
        let agent = json!({
            "nativeCodex": {"compatible": true, "daemonRunning": false},
            "proxy": {"running": false, "present": false, "ready": false},
            "configuration": {"present": false, "credentialsReady": false},
            "managedProfiles": [],
        });

        let status = RemoteHostManager::compose_status(host.clone(), Some(host), agent, None, None);

        assert!(status.chatgpt.is_none());
        assert!(status.inventory.is_none());
        assert!(!status
            .blocked_reasons
            .iter()
            .any(|reason| reason.starts_with("officialAccount")));
    }

    #[test]
    fn non_null_treats_explicit_json_null_the_same_as_absent() {
        assert_eq!(non_null(Some(&Value::Null)), None);
        assert_eq!(non_null(None), None);
        assert_eq!(
            non_null(Some(&json!({"state": "synchronized"}))),
            Some(json!({"state": "synchronized"}))
        );
    }

    #[test]
    fn snapshot_fallback_accepts_only_explicit_method_not_found() {
        assert!(manager_snapshot_method_not_found(&AppError::Message(
            "agent.methodNotFound: host.managerSnapshot".into()
        )));
        assert!(manager_snapshot_method_not_found(&AppError::Message(
            "invalid agent request: unknown variant `host.managerSnapshot`".into()
        )));
        assert!(!manager_snapshot_method_not_found(&AppError::Message(
            "ssh agent timed out after 60s".into()
        )));
        assert!(!manager_snapshot_method_not_found(&AppError::Message(
            "Permission denied (publickey)".into()
        )));
    }

    #[test]
    fn grok_remote_secret_contains_headless_identity_without_auth_file_shape() {
        let account = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::write(account.path().join("agent_id"), "agent-managed\n").unwrap();
        std::fs::write(
            external.path().join("version.json"),
            r#"{"version":"0.2.112"}"#,
        )
        .unwrap();
        let secret = grok_remote_credential_secret(
            crate::grok_auth::GrokCredential {
                access_token: Zeroizing::new("token-value".into()),
                user_id: Some("user-1".into()),
                email: None,
            },
            account.path(),
            external.path(),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&secret).unwrap();
        assert_eq!(value["accessToken"], "token-value");
        assert_eq!(value["clientVersion"], "0.2.112");
        assert_eq!(value["agentId"], "agent-managed");
        assert_eq!(value["userId"], "user-1");
        assert!(value.get("email").is_none());
    }

    #[test]
    fn boundary_key_provisioning_precedes_bootstrap_and_start_managed_rpcs_in_source_order() {
        let source = include_str!("host_manager.rs");
        let provision_at = source
            .find("provision_remote_boundary_key(")
            .expect("repair() must call provision_remote_boundary_key");
        let bootstrap_at = source
            .find("client.codex_bootstrap_native(")
            .expect("repair() must call codex_bootstrap_native");
        let start_managed_at = source
            .find("client.codex_start_managed(")
            .expect("repair() must call codex_start_managed");
        let confirm_at = source
            .find("confirm_remote_boundary_key_consumers(")
            .expect("repair() must confirm resulting consumer identities");
        assert!(
            provision_at < bootstrap_at,
            "boundary-key provisioning must run before codex.bootstrapNative"
        );
        assert!(
            provision_at < start_managed_at,
            "boundary-key provisioning must run before codex.startManaged"
        );
        assert!(
            bootstrap_at < confirm_at && start_managed_at < confirm_at,
            "consumer confirmation must follow repair lifecycle RPCs"
        );
    }
}
