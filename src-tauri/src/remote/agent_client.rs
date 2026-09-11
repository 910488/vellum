//! SSH/local process client for `vellum-remote-agent`.
//!
//! Production transport writes request JSON to the agent process stdin so the
//! remote shell never re-tokenizes JSON. Desktop commands accept only hostId
//! and resolve trusted SSH metadata from the local host cache.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::remote::local_cache::CachedHost;
use crate::remote::process::background_command;
use crate::remote::ssh_trust;

const SSH_CONNECT_TIMEOUT_SECS: u64 = 10;
const RPC_TIMEOUT_SECS: u64 = 60;
const SUPPORTED_AGENT_PROTOCOLS: &[u64] = &[1, 2, 3];

/// Trusted process target resolved from CachedHost metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedAgentTarget {
    pub host_id: String,
    pub ssh_destination: Option<String>,
    pub agent_bin: String,
    pub state_root: Option<PathBuf>,
}

impl ResolvedAgentTarget {
    pub fn from_cached_host(host: &CachedHost) -> Self {
        let ssh_destination = {
            let alias = host.ssh_alias.trim();
            if alias.is_empty() {
                None
            } else {
                Some(alias.to_string())
            }
        };
        Self {
            host_id: host.id.clone(),
            ssh_destination,
            agent_bin: "vellum-remote-agent".into(),
            state_root: None,
        }
    }

    pub fn local_dev(
        host_id: impl Into<String>,
        agent_bin: impl Into<String>,
        state_root: Option<PathBuf>,
    ) -> Self {
        Self {
            host_id: host_id.into(),
            ssh_destination: None,
            agent_bin: agent_bin.into(),
            state_root,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RemoteAgentClient {
    target: ResolvedAgentTarget,
}

impl RemoteAgentClient {
    pub fn new(target: ResolvedAgentTarget) -> Self {
        Self { target }
    }

    pub fn target(&self) -> &ResolvedAgentTarget {
        &self.target
    }

    pub fn agent_version(&self) -> AppResult<Value> {
        let value = self.rpc(json!({"method": "agent.version"}))?;
        ensure_agent_compatible(&value)?;
        Ok(value)
    }

    pub fn host_status(&self) -> AppResult<Value> {
        let value = self.rpc(json!({"method": "host.status"}))?;
        ensure_agent_compatible(&value)?;
        Ok(value)
    }

    pub fn host_capabilities(&self) -> AppResult<Value> {
        self.rpc(json!({"method": "host.capabilities"}))
    }

    /// M30: versioned host inventory (system / docker / codex / proxy facts).
    /// Older agents answer an unknown-method error; callers treat that as
    /// `None` and fall back to `host.status`.
    pub fn host_inventory_v2(&self) -> AppResult<Value> {
        let value = self.rpc(json!({"method": "host.inventoryV2"}))?;
        ensure_agent_compatible(&value)?;
        Ok(value)
    }

    /// M35: single-round-trip status, inventory, account, and native session
    /// summary. Replaces the sequential `host_status`, `host_inventory_v2`,
    /// `codex_account_status`, and `codex_session_status` calls a refresh
    /// used to make, cutting a refresh from up to four SSH processes down to
    /// one. Older agents that predate this method answer an unknown-method
    /// error; callers fall back to the individual RPCs above (see
    /// `RemoteHostManager::aggregate_status`).
    pub fn host_manager_snapshot(
        &self,
        expected_account_id: Option<&str>,
        thread_id: Option<&str>,
    ) -> AppResult<Value> {
        let mut request = json!({"method": "host.managerSnapshot"});
        if let Some(account_id) = expected_account_id {
            request["expectedAccountId"] = json!(account_id);
        }
        if let Some(thread_id) = thread_id {
            request["threadId"] = json!(thread_id);
        }
        let value = self.rpc(request)?;
        ensure_agent_compatible(&value)?;
        Ok(value)
    }

    /// M31: stage raw artifact bytes on the remote host via stdin (`cat >`),
    /// so JSON-adjacent transports are never used for binary payloads.
    pub fn stage_artifact(&self, remote_path: &str, bytes: &[u8]) -> AppResult<()> {
        let Some(destination) = self.target.ssh_destination.as_deref() else {
            return Err(AppError::Message(
                "staging requires an SSH destination".into(),
            ));
        };
        let data_root = ssh_trust::runtime_data_root();
        let trust_target = ssh_trust::resolve_ssh_target(destination)?;
        ssh_trust::require_trust_or_error(&data_root, &trust_target)?;
        let mut cmd = background_command("ssh");
        cmd.arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg(format!("ConnectTimeout={SSH_CONNECT_TIMEOUT_SECS}"))
            .args(ssh_trust::strict_host_key_args(&data_root))
            .arg("-T")
            .arg(destination)
            .arg("cat")
            .arg(">")
            .arg(remote_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = run_with_stdin_bytes(cmd, bytes, "ssh stage")?;
        if !output.status.success() {
            return Err(AppError::Message(format!(
                "stage failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(())
    }

    /// M31: remove a staged artifact on the remote host (best effort).
    ///
    /// This used to carry no `StrictHostKeyChecking` option at all, relying
    /// on whatever `ssh_config(5)` defaults the host happened to have --
    /// inconsistent with every other SSH call site here. It now goes through
    /// the same Vellum-private trust store as the rest of this file, and
    /// silently skips (rather than connecting insecurely) if the host has
    /// not been confirmed, since cleanup here is explicitly best-effort.
    pub fn remove_remote_file(&self, remote_path: &str) {
        let Some(destination) = self.target.ssh_destination.as_deref() else {
            return;
        };
        let data_root = ssh_trust::runtime_data_root();
        let Ok(trust_target) = ssh_trust::resolve_ssh_target(destination) else {
            return;
        };
        if ssh_trust::require_trust_or_error(&data_root, &trust_target).is_err() {
            return;
        }
        let mut cmd = background_command("ssh");
        cmd.args(["-o", "BatchMode=yes"])
            .arg("-o")
            .arg(format!("ConnectTimeout={SSH_CONNECT_TIMEOUT_SECS}"))
            .args(ssh_trust::strict_host_key_args(&data_root))
            .arg("-T")
            .arg(destination)
            .arg("rm")
            .arg("-f")
            .arg(remote_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = cmd.status();
    }

    /// M31: install a digest-pinned Codex artifact already staged on the host.
    pub fn codex_install_pinned(
        &self,
        operation_id: &str,
        staged_path: &str,
        expected_sha256: &str,
        expected_version: &str,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "codex.installPinned",
            "operationId": operation_id,
            "stagedPath": staged_path,
            "expectedSha256": expected_sha256,
            "expectedVersion": expected_version,
        }))
    }

    /// M31: update to the pinned Codex version (agent enforces no-downgrade).
    pub fn codex_update_pinned(
        &self,
        operation_id: &str,
        staged_path: &str,
        expected_sha256: &str,
        pinned_version: &str,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "codex.updatePinned",
            "operationId": operation_id,
            "stagedPath": staged_path,
            "expectedSha256": expected_sha256,
            "pinnedVersion": pinned_version,
        }))
    }

    /// M31: verification snapshot of the remote standalone install.
    pub fn codex_verify_installation(&self, compatible_range: Option<&str>) -> AppResult<Value> {
        let mut request = json!({"method": "codex.verifyInstallation"});
        if let Some(range) = compatible_range {
            request["compatibleRange"] = json!(range);
        }
        self.rpc(request)
    }

    /// M34: read-only native session status from the app-server control API.
    /// Never falls back to Broker state; unsupported versions return an
    /// explicit `NativeSessionObservabilityUnsupported` error.
    pub fn codex_session_status(&self, thread_id: Option<&str>) -> AppResult<Value> {
        let mut request = json!({"method": "codex.sessionStatus"});
        if let Some(thread_id) = thread_id {
            request["threadId"] = json!(thread_id);
        }
        self.rpc(request)
    }

    pub fn codex_account_status(&self, expected_account_id: Option<&str>) -> AppResult<Value> {
        let mut request = json!({"method": "codex.accountStatus"});
        if let Some(account_id) = expected_account_id {
            request["expectedAccountId"] = json!(account_id);
        }
        self.rpc(request)
    }

    pub fn codex_account_login_start(
        &self,
        operation_id: &str,
        expected_account_id: &str,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "codex.accountLoginStart",
            "operationId": operation_id,
            "expectedAccountId": expected_account_id,
        }))
    }

    pub fn codex_account_login_poll(&self) -> AppResult<Value> {
        self.rpc(json!({"method": "codex.accountLoginPoll"}))
    }

    pub fn codex_account_activate(&self, operation_id: &str, account_id: &str) -> AppResult<Value> {
        self.rpc(json!({
            "method": "codex.accountActivate",
            "operationId": operation_id,
            "accountId": account_id,
        }))
    }

    pub fn codex_remote_control_pair_start(
        &self,
        expected_control_account_hash: &str,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "codex.remoteControlPairStart",
            "expectedControlAccountHash": expected_control_account_hash,
        }))
    }

    pub fn proxy_official_account_login_start(
        &self,
        operation_id: &str,
        display_name: &str,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "proxy.officialAccountLoginStart",
            "operationId": operation_id,
            "displayName": display_name,
        }))
    }

    pub fn proxy_official_account_login_poll(&self, login_id: &str) -> AppResult<Value> {
        self.rpc(json!({
            "method": "proxy.officialAccountLoginPoll",
            "loginId": login_id,
        }))
    }

    pub fn proxy_official_account_list(&self) -> AppResult<Value> {
        self.rpc(json!({"method": "proxy.officialAccountList"}))
    }

    pub fn proxy_official_account_select(
        &self,
        operation_id: &str,
        account_id_hash: &str,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "proxy.officialAccountSelect",
            "operationId": operation_id,
            "accountIdHash": account_id_hash,
        }))
    }

    pub fn proxy_official_account_remove(
        &self,
        operation_id: &str,
        account_id_hash: &str,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "proxy.officialAccountRemove",
            "operationId": operation_id,
            "accountIdHash": account_id_hash,
        }))
    }

    /// M31: idempotent durable user-service reconcile for the managed daemon.
    pub fn services_reconcile(&self, operation_id: &str) -> AppResult<Value> {
        self.rpc(json!({
            "method": "services.reconcile",
            "operationId": operation_id,
        }))
    }

    /// Self-update the remote agent from a staged, digest-verified binary.
    pub fn agent_update(
        &self,
        operation_id: &str,
        staged_path: &str,
        expected_sha256: &str,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "agent.update",
            "operationId": operation_id,
            "stagedPath": staged_path,
            "expectedSha256": expected_sha256,
        }))
    }

    pub fn proxy_status(&self) -> AppResult<Value> {
        self.rpc(json!({"method": "proxy.status"}))
    }

    pub fn proxy_install(
        &self,
        operation_id: &str,
        image: &str,
        image_digest: Option<&str>,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "proxy.install",
            "operationId": operation_id,
            "image": image,
            "imageDigest": image_digest,
        }))
    }

    pub fn proxy_load_image(
        &self,
        operation_id: &str,
        staged_path: &str,
        expected_sha256: &str,
        image: &str,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "proxy.loadImage",
            "operationId": operation_id,
            "stagedPath": staged_path,
            "expectedSha256": expected_sha256,
            "image": image,
        }))
    }

    pub fn proxy_start(
        &self,
        operation_id: &str,
        host_port: Option<u16>,
        image: Option<&str>,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "proxy.start",
            "operationId": operation_id,
            "hostPort": host_port,
            "image": image,
        }))
    }

    pub fn proxy_stop(&self, operation_id: &str) -> AppResult<Value> {
        self.rpc(json!({
            "method": "proxy.stop",
            "operationId": operation_id,
        }))
    }

    pub fn proxy_restart(&self, operation_id: &str) -> AppResult<Value> {
        self.rpc(json!({
            "method": "proxy.restart",
            "operationId": operation_id,
        }))
    }

    pub fn proxy_configure(&self, operation_id: &str, config_toml: &str) -> AppResult<Value> {
        self.rpc(json!({
            "method": "proxy.configure", "operationId": operation_id,
            "configToml": config_toml,
        }))
    }

    pub fn credential_put(
        &self,
        operation_id: &str,
        credential_id: &str,
        secret: &str,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "credential.put", "operationId": operation_id,
            "credentialId": credential_id, "secret": secret,
        }))
    }

    pub fn credential_delete(&self, operation_id: &str, credential_id: &str) -> AppResult<Value> {
        self.rpc(json!({
            "method": "credential.delete", "operationId": operation_id,
            "credentialId": credential_id,
        }))
    }

    pub fn credential_status(&self, credential_id: &str) -> AppResult<Value> {
        self.rpc(json!({"method": "credential.status", "credentialId": credential_id}))
    }

    pub fn grok_install_account(
        &self,
        operation_id: &str,
        credential_id: &str,
        auth_json: &str,
        version_json: Option<&str>,
        agent_id: Option<&str>,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "grok.installAccount",
            "operationId": operation_id,
            "credentialId": credential_id,
            "authJson": auth_json,
            "versionJson": version_json,
            "agentId": agent_id,
        }))
    }

    pub fn grok_start_login(&self, operation_id: &str) -> AppResult<Value> {
        self.rpc(json!({"method": "grok.startLogin", "operationId": operation_id}))
    }

    pub fn grok_poll_login(&self) -> AppResult<Value> {
        self.rpc(json!({"method": "grok.pollLogin"}))
    }

    pub fn grok_cancel_login(&self, operation_id: &str) -> AppResult<Value> {
        self.rpc(json!({"method": "grok.cancelLogin", "operationId": operation_id}))
    }

    pub fn grok_status(&self) -> AppResult<Value> {
        self.rpc(json!({"method": "grok.status"}))
    }

    pub fn grok_refresh(&self, operation_id: &str) -> AppResult<Value> {
        self.rpc(json!({"method": "grok.refresh", "operationId": operation_id}))
    }

    pub fn grok_remove(&self, operation_id: &str) -> AppResult<Value> {
        self.rpc(json!({"method": "grok.remove", "operationId": operation_id}))
    }

    pub fn codex_create_managed_profile(
        &self,
        operation_id: &str,
        profile_id: &str,
        auth_json: Option<&str>,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "codex.createManagedProfile", "operationId": operation_id,
            "profileId": profile_id, "authJson": auth_json,
        }))
    }

    pub fn codex_adopt_existing(
        &self,
        operation_id: &str,
        profile_id: &str,
        codex_home: &str,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "codex.adoptExisting", "operationId": operation_id,
            "profileId": profile_id, "codexHome": codex_home, "explicitAdopt": true,
        }))
    }

    pub fn codex_discover_native(&self) -> AppResult<Value> {
        self.rpc(json!({"method": "codex.discoverNative"}))
    }

    pub fn codex_plan_native_adopt(&self) -> AppResult<Value> {
        self.rpc(json!({"method": "codex.planNativeAdopt"}))
    }

    pub fn codex_apply_native_adopt(
        &self,
        operation_id: &str,
        catalog_json: &str,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "codex.applyNativeAdopt",
            "operationId": operation_id,
            "catalogJson": catalog_json,
        }))
    }

    pub fn codex_bootstrap_native(&self, operation_id: &str) -> AppResult<Value> {
        self.rpc(json!({
            "method": "codex.bootstrapNative",
            "operationId": operation_id,
        }))
    }

    pub fn codex_restart_native(&self, operation_id: &str) -> AppResult<Value> {
        self.rpc(json!({
            "method": "codex.restartNative",
            "operationId": operation_id,
        }))
    }

    pub fn codex_stop_app_owned(&self, operation_id: &str) -> AppResult<Value> {
        self.rpc(json!({
            "method": "codex.stopAppOwned",
            "operationId": operation_id,
        }))
    }

    pub fn codex_restore_native(&self, operation_id: &str) -> AppResult<Value> {
        self.rpc(json!({
            "method": "codex.restoreNative",
            "operationId": operation_id,
        }))
    }

    pub fn codex_status(&self, profile_id: &str) -> AppResult<Value> {
        self.rpc(json!({"method": "codex.status", "profileId": profile_id}))
    }

    pub fn codex_plan_injection(&self, profile_id: &str) -> AppResult<Value> {
        self.rpc(json!({"method": "codex.planInjection", "profileId": profile_id}))
    }

    pub fn codex_inject(
        &self,
        operation_id: &str,
        profile_id: &str,
        catalog_json: &str,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "codex.inject", "operationId": operation_id,
            "profileId": profile_id, "catalogJson": catalog_json,
        }))
    }

    pub fn codex_restore(&self, operation_id: &str, profile_id: &str) -> AppResult<Value> {
        self.rpc(json!({
            "method": "codex.restore", "operationId": operation_id,
            "profileId": profile_id,
        }))
    }

    pub fn codex_start_managed(
        &self,
        operation_id: &str,
        profile_id: &str,
        broker_port: Option<u16>,
    ) -> AppResult<Value> {
        self.rpc(json!({
            "method": "codex.startManaged", "operationId": operation_id,
            "profileId": profile_id, "brokerPort": broker_port,
        }))
    }

    pub fn codex_stop_managed(&self, operation_id: &str, profile_id: &str) -> AppResult<Value> {
        self.rpc(json!({
            "method": "codex.stopManaged", "operationId": operation_id,
            "profileId": profile_id,
        }))
    }

    pub fn runtime_status(&self, profile_id: &str) -> AppResult<Value> {
        self.rpc(json!({"method": "runtime.status", "profileId": profile_id}))
    }

    pub fn lease_status(&self, profile_id: &str) -> AppResult<Value> {
        self.rpc(json!({"method": "lease.status", "profileId": profile_id}))
    }

    pub fn doctor(&self) -> AppResult<Value> {
        self.rpc(json!({"method": "doctor.run"}))
    }

    pub fn repair(&self, operation_id: &str) -> AppResult<Value> {
        self.rpc(json!({"method": "repair.run", "operationId": operation_id}))
    }

    pub fn support_bundle(&self) -> AppResult<Value> {
        self.rpc(json!({"method": "support.bundle"}))
    }

    pub fn rpc(&self, request: Value) -> AppResult<Value> {
        let request_json = serde_json::to_string(&request)
            .map_err(|error| AppError::Message(format!("encode agent request failed: {error}")))?;

        let output = if let Some(destination) = &self.target.ssh_destination {
            invoke_ssh_stdin_rpc(
                destination,
                &self.target.agent_bin,
                self.target.state_root.as_ref(),
                &request_json,
            )?
        } else {
            invoke_local_stdin_rpc(
                &self.target.agent_bin,
                self.target.state_root.as_ref(),
                &request_json,
            )?
        };

        parse_agent_response(&output)
    }
}

fn ensure_agent_compatible(value: &Value) -> AppResult<()> {
    let protocol = value
        .get("agentProtocol")
        .and_then(Value::as_u64)
        .ok_or_else(|| AppError::Message("agent response missing agentProtocol".into()))?;
    if !SUPPORTED_AGENT_PROTOCOLS.contains(&protocol) {
        return Err(AppError::Message(format!(
            "IncompatibleAgentProtocol: remote={protocol}, supported={SUPPORTED_AGENT_PROTOCOLS:?}"
        )));
    }
    Ok(())
}

fn invoke_local_stdin_rpc(
    agent_bin: &str,
    state_root: Option<&PathBuf>,
    request_json: &str,
) -> AppResult<std::process::Output> {
    let mut cmd = background_command(agent_bin);
    if let Some(state_root) = state_root {
        cmd.arg("--state-root").arg(state_root);
    }
    cmd.arg("rpc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_with_stdin(cmd, request_json, "local agent")
}

fn invoke_ssh_stdin_rpc(
    destination: &str,
    agent_bin: &str,
    state_root: Option<&PathBuf>,
    request_json: &str,
) -> AppResult<std::process::Output> {
    // Remote argv only. Request JSON is written to stdin so the remote shell
    // cannot re-tokenize braces/quotes inside the payload.
    let remote_command = if let Some(state_root) = state_root {
        format!(
            "{} --state-root {} rpc",
            shell_quote(agent_bin),
            shell_quote(&state_root.to_string_lossy())
        )
    } else {
        // A bootstrap install lives in ~/.local/bin, which many SSH servers do
        // not add to non-interactive PATH. The fallback keeps existing custom
        // installations working without making renderer input executable.
        format!(
            "if [ -x \"$HOME/.local/bin/vellum-remote-agent\" ]; then exec \"$HOME/.local/bin/vellum-remote-agent\" rpc; else exec {} rpc; fi",
            shell_quote(agent_bin)
        )
    };

    let data_root = ssh_trust::runtime_data_root();
    let trust_target = ssh_trust::resolve_ssh_target(destination)?;
    ssh_trust::require_trust_or_error(&data_root, &trust_target)?;

    let mut cmd = background_command("ssh");
    cmd.arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg(format!("ConnectTimeout={SSH_CONNECT_TIMEOUT_SECS}"))
        .args(ssh_trust::strict_host_key_args(&data_root))
        .arg("-T")
        .arg(destination)
        .arg(remote_command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_with_stdin(cmd, request_json, "ssh agent")
}

fn run_with_stdin(
    cmd: Command,
    request_json: &str,
    label: &str,
) -> AppResult<std::process::Output> {
    run_with_stdin_bytes(cmd, request_json.as_bytes(), label)
}

fn run_with_stdin_bytes(
    mut cmd: Command,
    payload: &[u8],
    label: &str,
) -> AppResult<std::process::Output> {
    let mut child = cmd
        .spawn()
        .map_err(|error| AppError::Message(format!("{label} spawn failed: {error}")))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::Message(format!("{label} stdin unavailable")))?;
        stdin
            .write_all(payload)
            .map_err(|error| AppError::Message(format!("{label} stdin write failed: {error}")))?;
        // Close stdin by dropping the handle so the agent sees EOF.
    }

    // Best-effort overall timeout using a join with sleep on a worker thread.
    // std::process::Child does not have a native timeout API that is portable
    // without wait_timeout crates; we poll.
    let deadline = std::time::Instant::now() + Duration::from_secs(RPC_TIMEOUT_SECS);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .map_err(|error| AppError::Message(format!("{label} wait failed: {error}")));
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(AppError::Message(format!(
                        "{label} timed out after {RPC_TIMEOUT_SECS}s"
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(AppError::Message(format!("{label} poll failed: {error}")));
            }
        }
    }
}

fn parse_agent_response(output: &std::process::Output) -> AppResult<Value> {
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(line) = stdout.lines().rev().find(|line| !line.trim().is_empty()) {
            if let Ok(response) = serde_json::from_str::<Value>(line.trim()) {
                if let Some(message) = response.pointer("/error/message").and_then(Value::as_str) {
                    return Err(AppError::Message(message.to_string()));
                }
            }
        }
        return Err(AppError::Message(format!(
            "agent failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    let response: Value = serde_json::from_str(line).map_err(|error| {
        AppError::Message(format!("invalid agent response: {error}; raw={line}"))
    })?;
    match response.get("type").and_then(Value::as_str) {
        Some("ok") => response
            .get("result")
            .cloned()
            .ok_or_else(|| AppError::Message("agent ok response missing result".into())),
        Some("error") => {
            let message = response
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("agent error");
            Err(AppError::Message(message.to_string()))
        }
        _ => Err(AppError::Message(format!(
            "unexpected agent response: {response}"
        ))),
    }
}

/// Quote a remote argv fragment for a POSIX-like remote shell.
/// Only used for trusted host metadata (agent bin / state root), never for JSON.
fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".into();
    }
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_leaves_safe_tokens() {
        assert_eq!(shell_quote("vellum-remote-agent"), "vellum-remote-agent");
        assert_eq!(shell_quote("user@host"), "user@host");
    }

    #[test]
    fn shell_quote_wraps_spaces() {
        assert_eq!(shell_quote("/opt/my agent"), "'/opt/my agent'");
    }

    #[test]
    fn compatibility_matrix_fails_closed_for_unknown_protocols() {
        assert!(ensure_agent_compatible(&json!({"agentProtocol": 1})).is_ok());
        assert!(ensure_agent_compatible(&json!({"agentProtocol": 3})).is_ok());
        let error = ensure_agent_compatible(&json!({"agentProtocol": 99})).unwrap_err();
        assert!(error.to_string().contains("IncompatibleAgentProtocol"));
        assert!(ensure_agent_compatible(&json!({})).is_err());
    }
}
