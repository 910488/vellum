use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::InstallRecord;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HostCapabilities {
    pub os: String,
    pub arch: String,
    pub docker_available: bool,
    pub docker_mode: String,
    pub rootless_docker: bool,
    pub user_systemd_available: bool,
    pub linger_enabled: bool,
    pub codex_binary: Option<String>,
    pub codex_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_manager: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistence_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_codex_home: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gui_session_available: Option<bool>,
}

/// Versioned host inventory (M30). A single-call superset of
/// `HostCapabilities` plus runtime facts, shaped for the Remote Manager UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct HostInventoryV2 {
    pub host_id: String,
    pub agent_version: String,
    pub agent_protocol: u32,
    pub system: SystemInventory,
    pub docker: DockerInventory,
    pub codex: CodexInventory,
    pub proxy: ProxyStatusView,
    #[serde(default)]
    pub blockers: Vec<HostBlocker>,
    #[serde(default)]
    pub available_actions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_manager: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistence_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_codex_home: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
pub struct SystemInventory {
    pub os: String,
    pub arch: String,
    pub hostname: Option<String>,
    pub cpu_cores: Option<u64>,
    pub memory_bytes: Option<u64>,
    pub disk_total_bytes: Option<u64>,
    pub disk_free_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
pub struct DockerInventory {
    pub available: bool,
    /// `rootless` / `rootful` / `unavailable`.
    pub mode: String,
    pub rootless: bool,
    /// `os/arch` reported by the Docker server.
    pub daemon: Option<String>,
    pub server_version: Option<String>,
    pub client_version: Option<String>,
    pub context: Option<String>,
    pub user_in_docker_group: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
pub struct CodexInventory {
    pub binary: Option<String>,
    pub version: Option<String>,
    /// `native` (Codex App daemon / standalone) / `path` (PATH or
    /// `VELLUM_CODEX_BIN`) / `missing`.
    pub source: String,
    pub codex_home: Option<String>,
    pub standalone_installed: bool,
    /// Stable `codex` command is ready for Codex App's SSH login shell.
    pub app_cli_discoverable: bool,
    pub app_cli_path: Option<String>,
    pub app_server_supported: bool,
    pub compatible: bool,
    pub compatibility_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HostBlocker {
    pub code: String,
    pub message: String,
    pub repairable: bool,
}

impl HostBlocker {
    pub fn new(code: impl Into<String>, message: impl Into<String>, repairable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            repairable,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    pub host_id: String,
    pub agent_version: String,
    pub agent_protocol: u32,
    pub capabilities: HostCapabilities,
    pub proxy: ProxyStatusView,
    pub install: Option<InstallRecord>,
    #[serde(default)]
    pub configuration: Value,
    #[serde(default)]
    pub managed_profiles: Vec<Value>,
    #[serde(default)]
    pub native_codex: Option<Value>,
    #[serde(default)]
    pub grok: Option<Value>,
    #[serde(default)]
    pub chatgpt: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProxyStatusView {
    pub present: bool,
    pub running: bool,
    pub ready: bool,
    pub container_id: Option<String>,
    pub image: Option<String>,
    pub image_digest: Option<String>,
    pub host_port: Option<u16>,
    pub install_id: Option<String>,
    pub config_hash: Option<String>,
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_executable_digest: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProxyLogsView {
    pub source: String,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

/// M35: one-SSH-round-trip superset of `host.status` + `host.inventoryV2` +
/// `codex.accountStatus` + `codex.sessionStatus`, so a Remote Manager refresh
/// never has to open more than one agent process to repaint the page. Older
/// desktops that only know the four separate methods keep working
/// unchanged; this is purely additive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HostManagerSnapshot {
    pub host_id: String,
    pub agent_version: String,
    pub agent_protocol: u32,
    pub status: HostStatus,
    pub inventory: HostInventoryV2,
    /// Native ChatGPT account identity/pairing state for `expected_account_id`
    /// (the desktop's own default account), when the caller supplied one.
    /// `None` when no `expected_account_id` was given or the probe failed —
    /// callers already treat a failed `codex.accountStatus` as "unknown", so
    /// this keeps the same fail-soft contract.
    #[serde(default)]
    pub account: Option<Value>,
    /// Native app-server session summary. `None` when the daemon is not
    /// running or the control API is unsupported on this Codex version —
    /// never a hard failure of the whole snapshot.
    #[serde(default)]
    pub session: Option<Value>,
    pub probe_timings_ms: HostManagerProbeTimings,
}

/// Per-phase elapsed time for one `host.managerSnapshot` call, so a slow
/// stage (e.g. Docker probing, or the native app-server socket) is visible
/// in support diagnostics without needing to reproduce the freeze.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct HostManagerProbeTimings {
    pub total_ms: u64,
    pub native_discover_ms: u64,
    pub status_ms: u64,
    pub inventory_ms: u64,
    pub account_ms: u64,
    pub session_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OperationResult {
    pub operation_id: String,
    pub idempotent_replay: bool,
    pub status: String,
    pub detail: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "method",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AgentRequest {
    #[serde(rename = "agent.version")]
    AgentVersion,
    #[serde(rename = "host.status")]
    HostStatus,
    #[serde(rename = "host.capabilities")]
    HostCapabilities,
    #[serde(rename = "host.inventoryV2")]
    HostInventoryV2,
    /// M35: single-round-trip status + inventory + account + session probe.
    #[serde(rename = "host.managerSnapshot")]
    HostManagerSnapshot {
        #[serde(default)]
        expected_account_id: Option<String>,
        #[serde(default)]
        thread_id: Option<String>,
    },
    #[serde(rename = "proxy.status")]
    ProxyStatus,
    #[serde(rename = "proxy.install")]
    ProxyInstall {
        operation_id: String,
        image: String,
        image_digest: Option<String>,
    },
    #[serde(rename = "proxy.loadImage")]
    ProxyLoadImage {
        operation_id: String,
        staged_path: String,
        expected_sha256: String,
        image: String,
    },
    #[serde(rename = "proxy.start")]
    ProxyStart {
        operation_id: String,
        host_port: Option<u16>,
        image: Option<String>,
    },
    #[serde(rename = "proxy.stop")]
    ProxyStop { operation_id: String },
    #[serde(rename = "proxy.restart")]
    ProxyRestart { operation_id: String },
    #[serde(rename = "proxy.configure")]
    ProxyConfigure {
        operation_id: String,
        config_toml: String,
    },
    #[serde(rename = "proxy.update")]
    ProxyUpdate {
        operation_id: String,
        image: String,
        image_digest: Option<String>,
    },
    #[serde(rename = "proxy.logs")]
    ProxyLogs {
        #[serde(default)]
        max_bytes: Option<u64>,
    },
    #[serde(rename = "proxy.rollback")]
    ProxyRollback { operation_id: String },
    #[serde(rename = "credential.put")]
    CredentialPut {
        operation_id: String,
        credential_id: String,
        secret: String,
    },
    #[serde(rename = "credential.delete")]
    CredentialDelete {
        operation_id: String,
        credential_id: String,
    },
    #[serde(rename = "credential.status")]
    CredentialStatus { credential_id: String },
    #[serde(rename = "grok.installAccount")]
    GrokInstallAccount {
        operation_id: String,
        credential_id: String,
        auth_json: String,
        version_json: Option<String>,
        agent_id: Option<String>,
    },
    #[serde(rename = "grok.startLogin")]
    GrokStartLogin { operation_id: String },
    #[serde(rename = "grok.pollLogin")]
    GrokPollLogin,
    #[serde(rename = "grok.cancelLogin")]
    GrokCancelLogin { operation_id: String },
    #[serde(rename = "grok.status")]
    GrokStatus,
    #[serde(rename = "grok.refresh")]
    GrokRefresh { operation_id: String },
    #[serde(rename = "grok.remove")]
    GrokRemove { operation_id: String },
    #[serde(rename = "codex.createManagedProfile")]
    CodexCreateManagedProfile {
        operation_id: String,
        profile_id: String,
        auth_json: Option<String>,
    },
    #[serde(rename = "codex.adoptExisting")]
    CodexAdoptExisting {
        operation_id: String,
        profile_id: String,
        codex_home: String,
        explicit_adopt: bool,
    },
    #[serde(rename = "codex.discoverNative")]
    CodexDiscoverNative,
    #[serde(rename = "codex.planNativeAdopt")]
    CodexPlanNativeAdopt,
    #[serde(rename = "codex.applyNativeAdopt")]
    CodexApplyNativeAdopt {
        operation_id: String,
        catalog_json: String,
    },
    #[serde(rename = "codex.bootstrapNative")]
    CodexBootstrapNative { operation_id: String },
    #[serde(rename = "codex.installPinned")]
    CodexInstallPinned {
        operation_id: String,
        staged_path: String,
        expected_sha256: String,
        expected_version: String,
    },
    #[serde(rename = "codex.updatePinned")]
    CodexUpdatePinned {
        operation_id: String,
        staged_path: String,
        expected_sha256: String,
        pinned_version: String,
    },
    #[serde(rename = "codex.verifyInstallation")]
    CodexVerifyInstallation { compatible_range: Option<String> },
    #[serde(rename = "codex.sessionStatus")]
    CodexSessionStatus { thread_id: Option<String> },
    #[serde(rename = "codex.accountStatus")]
    CodexAccountStatus { expected_account_id: Option<String> },
    /// Synchronize the remote daemon's control identity with Desktop. This is
    /// deliberately distinct from proxy Official execution accounts.
    #[serde(rename = "codex.accountLoginStart")]
    CodexAccountLoginStart {
        operation_id: String,
        expected_account_id: String,
    },
    #[serde(rename = "codex.accountLoginPoll")]
    CodexAccountLoginPoll,
    #[serde(rename = "codex.accountActivate")]
    CodexAccountActivate {
        operation_id: String,
        account_id: String,
    },
    /// Pair a phone only after proving the daemon is still using Desktop's
    /// control account. Official Remote itself requires the same account and
    /// workspace on both devices.
    #[serde(rename = "codex.remoteControlPairStart")]
    CodexRemoteControlPairStart {
        expected_control_account_hash: String,
    },
    #[serde(rename = "proxy.officialAccountLoginStart")]
    ProxyOfficialAccountLoginStart {
        operation_id: String,
        display_name: String,
    },
    #[serde(rename = "proxy.officialAccountLoginPoll")]
    ProxyOfficialAccountLoginPoll { login_id: String },
    #[serde(rename = "proxy.officialAccountList")]
    ProxyOfficialAccountList,
    #[serde(rename = "proxy.officialAccountSelect")]
    ProxyOfficialAccountSelect {
        operation_id: String,
        account_id_hash: String,
    },
    #[serde(rename = "proxy.officialAccountRemove")]
    ProxyOfficialAccountRemove {
        operation_id: String,
        account_id_hash: String,
    },
    #[serde(rename = "services.reconcile")]
    ServicesReconcile { operation_id: String },
    #[serde(rename = "codex.restartNative")]
    CodexRestartNative { operation_id: String },
    #[serde(rename = "codex.stopAppOwned")]
    CodexStopAppOwned { operation_id: String },
    #[serde(rename = "codex.restoreNative")]
    CodexRestoreNative { operation_id: String },
    #[serde(rename = "codex.status")]
    CodexStatus { profile_id: String },
    #[serde(rename = "codex.planInjection")]
    CodexPlanInjection { profile_id: String },
    #[serde(rename = "codex.inject")]
    CodexInject {
        operation_id: String,
        profile_id: String,
        catalog_json: String,
    },
    #[serde(rename = "codex.startManaged")]
    CodexStartManaged {
        operation_id: String,
        profile_id: String,
        broker_port: Option<u16>,
    },
    #[serde(rename = "codex.stopManaged")]
    CodexStopManaged {
        operation_id: String,
        profile_id: String,
    },
    #[serde(rename = "codex.restore")]
    CodexRestore {
        operation_id: String,
        profile_id: String,
    },
    #[serde(rename = "lease.status")]
    LeaseStatus { profile_id: String },
    #[serde(rename = "runtime.status")]
    RuntimeStatus { profile_id: String },
    #[serde(rename = "doctor.run")]
    DoctorRun,
    #[serde(rename = "repair.run")]
    RepairRun { operation_id: String },
    #[serde(rename = "support.bundle")]
    SupportBundle,
    #[serde(rename = "agent.update")]
    AgentUpdate {
        operation_id: String,
        staged_path: String,
        expected_sha256: String,
    },
    #[serde(rename = "agent.rollback")]
    AgentRollback { operation_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AgentResponse {
    #[serde(rename = "ok")]
    Ok { result: Value },
    #[serde(rename = "error")]
    Error { error: AgentError },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentError {
    pub code: String,
    pub message: String,
    pub repairable: bool,
}

impl AgentError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, repairable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            repairable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_install_wire_contract_is_camel_case() {
        let value = serde_json::to_value(AgentRequest::ProxyInstall {
            operation_id: "op-1".into(),
            image: "vellum-proxy:local".into(),
            image_digest: None,
        })
        .unwrap();

        assert_eq!(value["method"], "proxy.install");
        assert_eq!(value["operationId"], "op-1");
        assert_eq!(value["image"], "vellum-proxy:local");
        assert!(value.get("operation_id").is_none());
        assert!(value.get("image_digest").is_none());
        assert!(value.get("imageDigest").is_some());

        let round_tripped: AgentRequest = serde_json::from_value(value).unwrap();
        assert_eq!(
            round_tripped,
            AgentRequest::ProxyInstall {
                operation_id: "op-1".into(),
                image: "vellum-proxy:local".into(),
                image_digest: None,
            }
        );
    }

    #[test]
    fn proxy_load_image_wire_contract_is_camel_case() {
        let request = AgentRequest::ProxyLoadImage {
            operation_id: "load-1".into(),
            staged_path: "/tmp/vellum-proxy-stage-a.tar".into(),
            expected_sha256: "a".repeat(64),
            image: "vellum-proxy:0.2.2".into(),
        };
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["method"], "proxy.loadImage");
        assert_eq!(value["operationId"], "load-1");
        assert!(value.get("stagedPath").is_some());
        assert!(value.get("expectedSha256").is_some());
        assert!(value.get("staged_path").is_none());
        assert_eq!(
            serde_json::from_value::<AgentRequest>(value).unwrap(),
            request
        );
    }

    #[test]
    fn proxy_start_wire_contract_is_camel_case() {
        let value = serde_json::to_value(AgentRequest::ProxyStart {
            operation_id: "op-2".into(),
            host_port: Some(15721),
            image: None,
        })
        .unwrap();

        assert_eq!(value["method"], "proxy.start");
        assert_eq!(value["operationId"], "op-2");
        assert_eq!(value["hostPort"], 15721);
        assert!(value.get("host_port").is_none());

        let round_tripped: AgentRequest = serde_json::from_value(value).unwrap();
        assert_eq!(
            round_tripped,
            AgentRequest::ProxyStart {
                operation_id: "op-2".into(),
                host_port: Some(15721),
                image: None,
            }
        );
    }

    #[test]
    fn proxy_stop_wire_contract_is_camel_case() {
        let value = serde_json::to_value(AgentRequest::ProxyStop {
            operation_id: "op-3".into(),
        })
        .unwrap();

        assert_eq!(value["method"], "proxy.stop");
        assert_eq!(value["operationId"], "op-3");
        assert!(value.get("operation_id").is_none());
    }

    #[test]
    fn proxy_logs_and_rollback_wire_contract_is_camel_case() {
        let logs = serde_json::to_value(AgentRequest::ProxyLogs {
            max_bytes: Some(4096),
        })
        .unwrap();
        assert_eq!(logs["method"], "proxy.logs");
        assert_eq!(logs["maxBytes"], 4096);
        assert!(logs.get("max_bytes").is_none());
        let bare: AgentRequest =
            serde_json::from_value(serde_json::json!({"method": "proxy.logs"})).unwrap();
        assert_eq!(bare, AgentRequest::ProxyLogs { max_bytes: None });

        let rollback = serde_json::to_value(AgentRequest::ProxyRollback {
            operation_id: "op-rb".into(),
        })
        .unwrap();
        assert_eq!(rollback["method"], "proxy.rollback");
        assert_eq!(rollback["operationId"], "op-rb");
        assert!(rollback.get("operation_id").is_none());
    }

    #[test]
    fn host_manager_snapshot_wire_contract_is_camel_case() {
        let value = serde_json::to_value(AgentRequest::HostManagerSnapshot {
            expected_account_id: Some("acct-1".into()),
            thread_id: None,
        })
        .unwrap();
        assert_eq!(value["method"], "host.managerSnapshot");
        assert_eq!(value["expectedAccountId"], "acct-1");
        assert!(value.get("expected_account_id").is_none());
        assert!(value.get("thread_id").is_none());

        let round_tripped: AgentRequest = serde_json::from_value(value).unwrap();
        assert_eq!(
            round_tripped,
            AgentRequest::HostManagerSnapshot {
                expected_account_id: Some("acct-1".into()),
                thread_id: None,
            }
        );

        // Both fields are optional: bare `{"method": "host.managerSnapshot"}`
        // must still parse for a caller that only wants status + inventory.
        let bare: AgentRequest =
            serde_json::from_value(serde_json::json!({"method": "host.managerSnapshot"})).unwrap();
        assert_eq!(
            bare,
            AgentRequest::HostManagerSnapshot {
                expected_account_id: None,
                thread_id: None,
            }
        );
    }

    #[test]
    fn host_manager_snapshot_response_never_leaks_secret_fields() {
        let snapshot = HostManagerSnapshot {
            host_id: "h-1".into(),
            agent_version: "0.3.0".into(),
            agent_protocol: 1,
            status: HostStatus {
                host_id: "h-1".into(),
                agent_version: "0.3.0".into(),
                agent_protocol: 1,
                capabilities: HostCapabilities {
                    os: "linux".into(),
                    arch: "x86_64".into(),
                    docker_available: true,
                    docker_mode: "rootful".into(),
                    rootless_docker: false,
                    user_systemd_available: true,
                    linger_enabled: true,
                    codex_binary: None,
                    codex_version: None,
                    platform: None,
                    proxy_backend: None,
                    service_manager: None,
                    persistence_scope: None,
                    managed_codex_home: None,
                    gui_session_available: None,
                },
                proxy: ProxyStatusView::default(),
                install: None,
                configuration: Value::Null,
                managed_profiles: Vec::new(),
                native_codex: None,
                grok: None,
                chatgpt: None,
            },
            inventory: HostInventoryV2::default(),
            account: Some(serde_json::json!({
                "accountId": "acct-1",
                "state": "synchronized",
                "paired": true,
                "active": true,
            })),
            session: None,
            probe_timings_ms: HostManagerProbeTimings::default(),
        };
        let value = serde_json::to_value(&snapshot).unwrap();
        let text = value.to_string();
        for secret_marker in ["token", "authJson", "refresh", "secret", "password"] {
            assert!(
                !text.to_lowercase().contains(&secret_marker.to_lowercase()),
                "host.managerSnapshot response must never carry {secret_marker}: {text}"
            );
        }
        assert_eq!(value["account"]["accountId"], "acct-1");
        assert_eq!(value["probeTimingsMs"]["totalMs"], 0);
    }

    #[test]
    fn configure_and_profile_contracts_never_use_snake_case() {
        let config = serde_json::to_value(AgentRequest::ProxyConfigure {
            operation_id: "op-4".into(),
            config_toml: "listen = '127.0.0.1:1'".into(),
        })
        .unwrap();
        assert_eq!(config["method"], "proxy.configure");
        assert_eq!(config["operationId"], "op-4");
        assert!(config.get("config_toml").is_none());

        let profile = serde_json::to_value(AgentRequest::CodexCreateManagedProfile {
            operation_id: "op-5".into(),
            profile_id: "default".into(),
            auth_json: None,
        })
        .unwrap();
        assert_eq!(profile["method"], "codex.createManagedProfile");
        assert_eq!(profile["profileId"], "default");
    }

    #[test]
    fn native_chatgpt_account_rpcs_are_identity_only_and_camel_case() {
        let start = AgentRequest::CodexAccountLoginStart {
            operation_id: "login-1".into(),
            expected_account_id: "acct-1".into(),
        };
        let value = serde_json::to_value(&start).unwrap();
        assert_eq!(value["method"], "codex.accountLoginStart");
        assert_eq!(value["operationId"], "login-1");
        assert_eq!(value["expectedAccountId"], "acct-1");
        assert!(value.get("token").is_none());
        assert!(value.get("authJson").is_none());
        assert_eq!(
            serde_json::from_value::<AgentRequest>(value).unwrap(),
            start
        );

        let activate = serde_json::to_value(AgentRequest::CodexAccountActivate {
            operation_id: "activate-1".into(),
            account_id: "acct-1".into(),
        })
        .unwrap();
        assert_eq!(activate["method"], "codex.accountActivate");
        assert_eq!(activate["accountId"], "acct-1");
        assert!(!activate.to_string().contains("refresh"));

        let pair = serde_json::to_value(AgentRequest::CodexRemoteControlPairStart {
            expected_control_account_hash: "a".repeat(64),
        })
        .unwrap();
        assert_eq!(pair["method"], "codex.remoteControlPairStart");
        assert_eq!(pair["expectedControlAccountHash"], "a".repeat(64));

        let execution = AgentRequest::ProxyOfficialAccountLoginStart {
            operation_id: "official-login-1".into(),
            display_name: "Execution B".into(),
        };
        let execution_value = serde_json::to_value(&execution).unwrap();
        assert_eq!(execution_value["method"], "proxy.officialAccountLoginStart");
        assert!(!execution_value.to_string().contains("token"));

        for request in [
            AgentRequest::ProxyOfficialAccountLoginPoll {
                login_id: "01K2ABCDEF0123456789ABCDEF".into(),
            },
            AgentRequest::ProxyOfficialAccountList,
            AgentRequest::ProxyOfficialAccountSelect {
                operation_id: "select-1".into(),
                account_id_hash: "b".repeat(64),
            },
            AgentRequest::ProxyOfficialAccountRemove {
                operation_id: "remove-1".into(),
                account_id_hash: "b".repeat(64),
            },
        ] {
            let value = serde_json::to_value(&request).unwrap();
            assert_eq!(
                serde_json::from_value::<AgentRequest>(value.clone()).unwrap(),
                request
            );
            let encoded = value.to_string();
            assert!(!encoded.contains("access_token"));
            assert!(!encoded.contains("refresh_token"));
        }
    }

    #[test]
    fn host_capabilities_no_longer_carry_host_local_ollama_inventory() {
        // e806 is a remote OpenAI-compatible API, not a Jetson host-local
        // Ollama server. The agent must not probe or advertise a loopback
        // Ollama inventory anymore.
        let value = serde_json::to_value(HostCapabilities {
            os: "linux".into(),
            arch: "aarch64".into(),
            docker_available: true,
            docker_mode: "system:linux/aarch64".into(),
            rootless_docker: false,
            user_systemd_available: true,
            linger_enabled: true,
            codex_binary: Some("codex".into()),
            codex_version: Some("0.147.0".into()),
            platform: None,
            proxy_backend: None,
            service_manager: None,
            persistence_scope: None,
            managed_codex_home: None,
            gui_session_available: None,
        })
        .unwrap();
        let object = value.as_object().unwrap();
        assert!(object.get("ollamaAvailable").is_none());
        assert!(object.get("ollamaVersion").is_none());
        assert!(object.get("ollamaModels").is_none());
        let encoded = value.to_string();
        assert!(!encoded.contains("ollama"));
    }

    #[test]
    fn host_capabilities_ignore_unknown_ollama_fields_from_older_agents() {
        // An older agent may still send the removed Ollama fields. Desktop and
        // newer agents must tolerate the extra keys instead of failing
        // deserialization.
        let legacy = r#"{
            "os": "linux",
            "arch": "aarch64",
            "dockerAvailable": true,
            "dockerMode": "system:linux/aarch64",
            "rootlessDocker": false,
            "userSystemdAvailable": true,
            "lingerEnabled": true,
            "codexBinary": "codex",
            "codexVersion": "0.147.0",
            "ollamaAvailable": true,
            "ollamaVersion": "0.32.6",
            "ollamaModels": ["qwen2.5-coder:0.5b"]
        }"#;
        let parsed: HostCapabilities = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.os, "linux");
        assert_eq!(parsed.arch, "aarch64");
        assert_eq!(parsed.codex_version.as_deref(), Some("0.147.0"));
    }

    #[test]
    fn host_inventory_v2_wire_contract_is_camel_case_and_defaults_gracefully() {
        let value = serde_json::to_value(AgentRequest::HostInventoryV2).unwrap();
        assert_eq!(value["method"], "host.inventoryV2");
        let round_tripped: AgentRequest = serde_json::from_value(value).unwrap();
        assert_eq!(round_tripped, AgentRequest::HostInventoryV2);

        // Missing optional/collection fields default instead of failing, so
        // older desktop builds can parse a newer agent's inventory.
        let minimal = r#"{
            "hostId": "h-1",
            "agentVersion": "0.1.0",
            "agentProtocol": 1,
            "system": {"os": "linux", "arch": "aarch64"},
            "docker": {"available": false, "mode": "unavailable"},
            "codex": {"source": "missing"},
            "proxy": {"present": false, "running": false, "ready": false}
        }"#;
        let parsed: HostInventoryV2 = serde_json::from_str(minimal).unwrap();
        assert_eq!(parsed.host_id, "h-1");
        assert!(parsed.blockers.is_empty());
        assert!(parsed.available_actions.is_empty());
        assert_eq!(parsed.system.cpu_cores, None);

        let encoded = serde_json::to_value(&parsed).unwrap();
        assert!(encoded.get("system").is_some());
        assert!(encoded.get("cpuCores").is_none());
        assert!(encoded.get("cpu_cores").is_none());
        assert_eq!(encoded["codex"]["source"], "missing");
        assert_eq!(parsed.platform, None);
        assert_eq!(parsed.proxy_backend, None);
        assert_eq!(parsed.managed_codex_home, None);
    }

    #[test]
    fn protocol_4_inventory_fields_are_optional_on_linux_shaped_payloads() {
        let value = serde_json::json!({
            "hostId": "mac",
            "agentVersion": "0.3.0",
            "agentProtocol": 4,
            "system": {"os": "macos", "arch": "aarch64"},
            "docker": {"available": false, "mode": "unavailable"},
            "codex": {"source": "native", "codexHome": "/Users/joshhuang/.vellum-remote/codex"},
            "proxy": {"present": true, "running": true, "ready": true, "hostPort": 15722, "proxyBackend": "native", "nativeExecutableDigest": "a".repeat(64)},
            "platform": "darwin-arm64",
            "proxyBackend": "native",
            "serviceManager": "launchd",
            "persistenceScope": "login",
            "managedCodexHome": "/Users/joshhuang/.vellum-remote/codex"
        });
        let parsed: HostInventoryV2 = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.platform.as_deref(), Some("darwin-arm64"));
        assert_eq!(parsed.proxy_backend.as_deref(), Some("native"));
        assert_eq!(parsed.service_manager.as_deref(), Some("launchd"));
        assert_eq!(parsed.persistence_scope.as_deref(), Some("login"));
        assert_eq!(
            parsed.managed_codex_home.as_deref(),
            Some("/Users/joshhuang/.vellum-remote/codex")
        );
        assert_eq!(parsed.proxy.host_port, Some(15722));
        assert_eq!(parsed.proxy.proxy_backend.as_deref(), Some("native"));
        assert!(parsed.proxy.image.is_none());
    }

    #[test]
    fn host_blocker_serializes_camel_case() {
        let blocker = HostBlocker::new("dockerUnavailable", "docker is down", true);
        let value = serde_json::to_value(&blocker).unwrap();
        assert_eq!(value["code"], "dockerUnavailable");
        assert_eq!(value["message"], "docker is down");
        assert_eq!(value["repairable"], true);
        assert!(value.get("repairable_").is_none());
    }

    #[test]
    fn pinned_install_rpcs_are_camel_case_and_round_trip() {
        let install = serde_json::to_value(AgentRequest::CodexInstallPinned {
            operation_id: "op-install".into(),
            staged_path: "/tmp/staged-codex".into(),
            expected_sha256: "a".repeat(64),
            expected_version: "0.147.0".into(),
        })
        .unwrap();
        assert_eq!(install["method"], "codex.installPinned");
        assert_eq!(install["operationId"], "op-install");
        assert_eq!(install["stagedPath"], "/tmp/staged-codex");
        assert_eq!(install["expectedSha256"], "a".repeat(64));
        assert_eq!(install["expectedVersion"], "0.147.0");
        assert!(install.get("staged_path").is_none());
        let round_tripped: AgentRequest = serde_json::from_value(install).unwrap();
        assert_eq!(
            round_tripped,
            AgentRequest::CodexInstallPinned {
                operation_id: "op-install".into(),
                staged_path: "/tmp/staged-codex".into(),
                expected_sha256: "a".repeat(64),
                expected_version: "0.147.0".into(),
            }
        );

        let update = serde_json::to_value(AgentRequest::CodexUpdatePinned {
            operation_id: "op-update".into(),
            staged_path: "/tmp/staged-codex".into(),
            expected_sha256: "b".repeat(64),
            pinned_version: "0.147.2".into(),
        })
        .unwrap();
        assert_eq!(update["method"], "codex.updatePinned");
        assert_eq!(update["pinnedVersion"], "0.147.2");
        assert!(update.get("pinned_version").is_none());

        let verify = serde_json::to_value(AgentRequest::CodexVerifyInstallation {
            compatible_range: Some("0.147.x".into()),
        })
        .unwrap();
        assert_eq!(verify["method"], "codex.verifyInstallation");
        assert_eq!(verify["compatibleRange"], "0.147.x");
        assert!(verify.get("compatible_range").is_none());

        let session = serde_json::to_value(AgentRequest::CodexSessionStatus {
            thread_id: Some("t-9".into()),
        })
        .unwrap();
        assert_eq!(session["method"], "codex.sessionStatus");
        assert_eq!(session["threadId"], "t-9");
        assert!(session.get("thread_id").is_none());
        let round_tripped: AgentRequest = serde_json::from_value(session).unwrap();
        assert_eq!(
            round_tripped,
            AgentRequest::CodexSessionStatus {
                thread_id: Some("t-9".into())
            }
        );

        let stop = serde_json::to_value(AgentRequest::CodexStopAppOwned {
            operation_id: "op-stop-app-owned".into(),
        })
        .unwrap();
        assert_eq!(stop["method"], "codex.stopAppOwned");
        assert_eq!(stop["operationId"], "op-stop-app-owned");
        assert!(stop.get("operation_id").is_none());
        let round_tripped: AgentRequest = serde_json::from_value(stop).unwrap();
        assert_eq!(
            round_tripped,
            AgentRequest::CodexStopAppOwned {
                operation_id: "op-stop-app-owned".into()
            }
        );

        let reconcile = serde_json::to_value(AgentRequest::ServicesReconcile {
            operation_id: "op-reconcile".into(),
        })
        .unwrap();
        assert_eq!(reconcile["method"], "services.reconcile");
        assert_eq!(reconcile["operationId"], "op-reconcile");
    }
}
