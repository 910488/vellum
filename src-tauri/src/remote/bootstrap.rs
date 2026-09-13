//! Fixed user-scope bootstrap transport used before the remote Agent exists.
//!
//! Renderer input is limited to a cached host id. Every remote program and
//! path below is owned by this module; artifact bytes travel over stdin and
//! are digest-verified before atomic installation.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};
use crate::remote::process::background_command;
use crate::remote::ssh_trust;
use crate::remote::{RemoteAgentClient, RemoteHostManager};
use crate::state::AppState;

struct ResolvedBootstrapArtifact {
    path: PathBuf,
    digest: String,
    remove_after_install: bool,
}

impl Drop for ResolvedBootstrapArtifact {
    fn drop(&mut self) {
        if self.remove_after_install {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapResult {
    pub operation_id: String,
    pub state: String,
    pub os: String,
    pub arch: String,
    pub agent_installed: bool,
    pub broker_installed: bool,
    pub docker_available: bool,
    pub systemd_user_available: bool,
    pub linger_enabled: bool,
    pub native_codex: Option<serde_json::Value>,
    pub completed_steps: Vec<String>,
    pub blocked_reasons: Vec<String>,
    pub repair_commands: Vec<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OneClickBootstrapResult {
    pub state: String,
    pub bootstrap: BootstrapResult,
    pub plan_id: String,
    pub selected_catalog_ids: Vec<String>,
    pub apply: crate::remote::RemoteApplyResult,
    pub manager_state: String,
    pub proxy_ready: bool,
    pub native_daemon_running: bool,
    pub detached_ready: bool,
}

/// Prepare an SSH host and converge it to its complete native Remote Manager
/// desired state. The workflow is intentionally idempotent: an already-ready
/// host reuses its saved model selection and each agent mutation has its own
/// deterministic operation id.
pub async fn one_click_bootstrap<F>(
    state: &AppState,
    host_id: &str,
    mut progress: F,
) -> AppResult<OneClickBootstrapResult>
where
    F: FnMut(&str, u8, &str),
{
    progress(
        "hostPreflight",
        5,
        "Checking SSH host, Docker and user services",
    );
    let mut prepared = bootstrap(state, host_id)?;

    let native_unavailable = prepared
        .blocked_reasons
        .iter()
        .any(|reason| reason.starts_with("nativeCodexUnavailable:"));
    if native_unavailable && crate::remote::pinned_install::release_status().ready {
        progress(
            "installCodex",
            20,
            "Installing the bundled pinned Codex CLI",
        );
        crate::remote::pinned_install::install_pinned_codex(
            state,
            host_id,
            &format!("{}-install-codex", prepared.operation_id),
        )?;
        progress("nativeDaemon", 35, "Enabling durable Codex remote control");
        prepared = bootstrap(state, host_id)?;
    }
    if !prepared.blocked_reasons.is_empty() {
        return Err(AppError::Message(format!(
            "OneClickBootstrapBlocked: {}; repair: {}",
            prepared.blocked_reasons.join(", "),
            prepared.repair_commands.join(" | ")
        )));
    }

    let client = RemoteAgentClient::new(RemoteHostManager::resolve_target(state, host_id)?);
    let before = client.host_status()?;
    let sessions = client.codex_session_status(None)?;
    let active_session = sessions
        .get("threads")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .any(|thread| {
            thread.get("active").and_then(serde_json::Value::as_bool) == Some(true)
                || thread
                    .get("activeTurnId")
                    .is_some_and(|turn_id| !turn_id.is_null())
        });
    if before
        .pointer("/nativeCodex/activeTurn")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
        || active_session
    {
        return Err(AppError::Message(
            "OneClickBootstrapBlocked: activeTurnInProgress; wait for the current Codex turn to finish"
                .into(),
        ));
    }

    progress(
        "deploymentPlan",
        45,
        "Resolving qualified models and credentials",
    );
    let desired = crate::remote::desired_state::load(&state.data_root(), host_id)?;
    let plan = crate::remote::deployment::plan(
        state,
        host_id,
        crate::remote::RemoteModelSelection {
            catalog_ids: desired.selected_catalog_ids,
            policy: crate::remote::deployment::RemotePolicyOverrides::default(),
        },
    )?;
    if !plan.blocked_reasons.is_empty() {
        return Err(AppError::Message(format!(
            "OneClickBootstrapBlocked: {}",
            plan.blocked_reasons.join(", ")
        )));
    }

    progress(
        "deploymentApply",
        60,
        "Installing proxy, credentials and native catalog",
    );
    let applied = crate::remote::deployment::apply(state, host_id, &plan.plan_id).await?;

    progress(
        "verification",
        92,
        "Verifying proxy, native daemon and detach readiness",
    );
    let verified = RemoteHostManager::aggregate_status(state, host_id)?;
    let proxy_ready = verified
        .agent
        .as_ref()
        .and_then(|agent| agent.pointer("/proxy/ready"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let native_daemon_running = verified
        .agent
        .as_ref()
        .and_then(|agent| agent.pointer("/nativeCodex/daemonRunning"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let detached_ready = verified.manager_state == "detachedReady" && native_daemon_running;
    if !proxy_ready || !native_daemon_running || !detached_ready {
        return Err(AppError::Message(format!(
            "OneClickBootstrapVerificationFailed: managerState={}, proxyReady={proxy_ready}, nativeDaemonRunning={native_daemon_running}, detachedReady={detached_ready}",
            verified.manager_state
        )));
    }
    progress(
        "verified",
        100,
        "Remote host is ready for native Codex projects",
    );
    Ok(OneClickBootstrapResult {
        state: verified.manager_state.clone(),
        bootstrap: prepared,
        plan_id: plan.plan_id,
        selected_catalog_ids: plan.selected_catalog_ids,
        apply: applied,
        manager_state: verified.manager_state,
        proxy_ready,
        native_daemon_running,
        detached_ready,
    })
}

/// Qualification entry point for a genuinely new Vellum host. It proves the
/// bundled release is usable and that no prior Vellum installation/state is
/// present before allowing the ordinary idempotent bootstrap to mutate it.
pub async fn one_click_clean_host_bootstrap<F>(
    state: &AppState,
    host_id: &str,
    mut progress: F,
) -> AppResult<OneClickBootstrapResult>
where
    F: FnMut(&str, u8, &str),
{
    progress(
        "cleanHostPreflight",
        2,
        "Verifying bundled artifacts and a clean Vellum host baseline",
    );
    crate::remote::pinned_install::load_verified_manifest()?;
    let target = RemoteHostManager::resolve_target(state, host_id)?;
    let alias = target
        .ssh_destination
        .ok_or_else(|| AppError::Message("clean-host bootstrap requires SSH".into()))?;
    let baseline = probe(&state.data_root(), &alias)?;
    let platform =
        crate::remote::platform::RemotePlatform::from_os_arch(&baseline.os, &baseline.arch)
            .map_err(|error| AppError::Message(format!("{}: {}", error.code, error.message)))?;
    let bundle_dir = platform.bundle_dir();
    let binaries = if platform.broker_required() {
        vec!["vellum-remote-agent", "vellum-remote-broker"]
    } else {
        vec!["vellum-remote-agent", "vellum-proxy-daemon"]
    };
    for binary_name in binaries {
        if let Some(path) = find_artifact(binary_name, bundle_dir) {
            let digest = file_sha256(&path).ok_or_else(|| {
                AppError::Message(format!("cannot hash bootstrap artifact {}", path.display()))
            })?;
            verify_release_manifest_digest_strict(&path, &digest)?;
        }
    }
    let mut found = Vec::new();
    if baseline.agent_installed {
        found.push("agent");
    }
    if baseline.broker_installed {
        found.push("broker");
    }
    if baseline.managed_footprint_present {
        found.push("managedState");
    }
    if !found.is_empty() {
        return Err(AppError::Message(format!(
            "CleanHostRequired: existing Vellum footprint detected ({})",
            found.join(",")
        )));
    }
    one_click_bootstrap(state, host_id, progress).await
}

pub fn bootstrap(state: &AppState, host_id: &str) -> AppResult<BootstrapResult> {
    let target = RemoteHostManager::resolve_target(state, host_id)?;
    let alias = target
        .ssh_destination
        .clone()
        .ok_or_else(|| AppError::Message("bootstrap requires an SSH destination".into()))?;
    let operation_id = format!("bootstrap-{}", ulid::Ulid::new());
    let data_root = state.data_root();
    let probe = probe(&data_root, &alias)?;
    let platform =
        match crate::remote::platform::RemotePlatform::from_os_arch(&probe.os, &probe.arch) {
            Ok(platform) => platform,
            Err(error) => {
                return Ok(blocked(operation_id, probe, error.code, &error.message));
            }
        };
    let normalized_arch = platform.bundle_dir();
    let remote_agent_sha256 = probe.agent_sha256.clone();
    let remote_broker_sha256 = probe.broker_sha256.clone();
    let mut result = BootstrapResult {
        operation_id: operation_id.clone(),
        state: "bootstrapping".into(),
        os: probe.os.clone(),
        arch: probe.arch.clone(),
        agent_installed: probe.agent_installed,
        broker_installed: probe.broker_installed,
        docker_available: probe.docker_available,
        systemd_user_available: probe.systemd_user_available,
        linger_enabled: probe.linger_enabled,
        native_codex: None,
        completed_steps: vec!["host.probed".into()],
        blocked_reasons: Vec::new(),
        repair_commands: Vec::new(),
        platform: Some(platform.artifact_key().into()),
        proxy_backend: Some(platform.proxy_backend_name().into()),
        service_manager: Some(platform.service_manager().into()),
        persistence_scope: Some(platform.persistence_scope().into()),
        managed_codex_home: probe.managed_codex_home.clone(),
    };

    match install_managed_ssh_isolation(&data_root, host_id, &alias, &probe) {
        Ok(alias_name) => {
            result
                .completed_steps
                .push(format!("ssh.isolationInstalled:{alias_name}"));
        }
        Err(error) => {
            result
                .blocked_reasons
                .push(format!("sshIsolationFailed:{error}"));
            result.repair_commands.push(
                "Create the dedicated Remote SSH key and marked authorized_keys entry, then Bootstrap again."
                    .into(),
            );
        }
    }

    let agent_artifact = find_artifact("vellum-remote-agent", normalized_arch);
    let agent_needs_install = !result.agent_installed
        || agent_artifact
            .as_deref()
            .and_then(file_sha256)
            .is_some_and(|digest| Some(digest) != remote_agent_sha256);
    if agent_needs_install {
        let artifact = resolve_bootstrap_artifact(
            agent_artifact,
            "vellum-remote-agent",
            &probe.os,
            &probe.arch,
        )?;
        install_artifact(
            &data_root,
            &alias,
            &artifact.path,
            "vellum-remote-agent",
            &artifact.digest,
            probe.disk_free_bytes,
        )?;
        result.agent_installed = true;
        result.completed_steps.push("agent.installed".into());
    }
    if platform.broker_required()
        && (!result.broker_installed
            || find_artifact("vellum-remote-broker", normalized_arch)
                .as_deref()
                .and_then(file_sha256)
                .is_some_and(|digest| Some(digest) != remote_broker_sha256))
    {
        match resolve_bootstrap_artifact(
            find_artifact("vellum-remote-broker", normalized_arch),
            "vellum-remote-broker",
            &probe.os,
            &probe.arch,
        ) {
            Ok(artifact) => {
                install_artifact(
                    &data_root,
                    &alias,
                    &artifact.path,
                    "vellum-remote-broker",
                    &artifact.digest,
                    probe.disk_free_bytes,
                )?;
                result.broker_installed = true;
                result
                    .completed_steps
                    .push("broker.installedInactive".into());
            }
            Err(_) => {
                // Broker is legacy diagnostics only and is not part of the
                // production Codex data path. A missing Broker artifact must
                // not block native Agent/Proxy bootstrap.
                result
                    .completed_steps
                    .push("broker.skippedDiagnosticOnly".into());
            }
        }
    }

    let client = RemoteAgentClient::new(target);
    client.agent_version()?;
    result.completed_steps.push("agent.compatible".into());
    if platform.docker_is_blocker() && !result.docker_available {
        result.blocked_reasons.push("dockerUnavailable".into());
        result.repair_commands.push(
            "依 Linux 發行版安裝 Docker，並將目前使用者加入可執行 docker 的群組後重新登入。".into(),
        );
    }
    if platform.service_manager() == crate::remote::platform::SERVICE_MANAGER_SYSTEMD_USER {
        if !result.systemd_user_available {
            result.blocked_reasons.push("systemdUserUnavailable".into());
            result.repair_commands.push(
                "確認 systemd user manager 可用，並由系統管理員執行：loginctl enable-linger $USER"
                    .into(),
            );
        } else if !result.linger_enabled {
            match ensure_linger(&data_root, &alias) {
                Ok(()) => {
                    result.linger_enabled = true;
                    result.completed_steps.push("host.lingerEnabled".into());
                }
                Err(error) => {
                    result
                        .blocked_reasons
                        .push(format!("lingerEnableFailed:{error}"));
                    result.repair_commands.push(
                        "Enable lingering for the SSH user (loginctl enable-linger $USER), then retry Bootstrap"
                            .into(),
                    );
                }
            }
        }
    } else if !probe.gui_session_available {
        result.blocked_reasons.push("guiSessionUnavailable".into());
        result.repair_commands.push(
            "登入 macOS 圖形工作階段後再部署。Remote Proxy 是登入後常駐，不會改電源或自動登入。"
                .into(),
        );
    }
    crate::remote::provision_remote_boundary_key(&client, &data_root, host_id, &operation_id)?;
    result
        .completed_steps
        .push("credentials.boundaryReady".into());
    match client.codex_bootstrap_native(&format!("{operation_id}-codex")) {
        Ok(native) => {
            crate::remote::confirm_remote_boundary_key_consumers(
                &client,
                &data_root,
                host_id,
                &[crate::proxy::BoundaryKeyConsumer::NativeCodex],
                true,
            )?;
            let durable = native
                .get("durable")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let standalone_installed = native
                .get("standaloneInstalled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            result.native_codex = Some(native);
            if durable {
                result
                    .completed_steps
                    .push("codex.nativeDaemonReady".into());
            } else {
                if standalone_installed {
                    result.completed_steps.push("codex.standaloneStaged".into());
                }
                result.blocked_reasons.push("nativeDaemonAppOwned".into());
                result.repair_commands.push(
                    "Disconnect the Codex App remote project so its direct app-server exits, then run Bootstrap again; Vellum will not kill an App-owned session."
                        .into(),
                );
            }
        }
        Err(error) => {
            result
                .blocked_reasons
                .push(format!("nativeCodexUnavailable:{error}"));
        }
    }
    result.state = if result.blocked_reasons.is_empty() {
        "readyToPlan"
    } else {
        "recoveryRequired"
    }
    .into();
    Ok(result)
}

#[derive(Debug)]
struct BootstrapProbe {
    os: String,
    arch: String,
    agent_installed: bool,
    broker_installed: bool,
    docker_available: bool,
    systemd_user_available: bool,
    linger_enabled: bool,
    managed_footprint_present: bool,
    agent_sha256: Option<String>,
    broker_sha256: Option<String>,
    gui_session_available: bool,
    user_home: Option<String>,
    managed_codex_home: Option<String>,
    disk_free_bytes: Option<u64>,
}

fn probe(data_root: &Path, alias: &str) -> AppResult<BootstrapProbe> {
    let script = r#"set -eu
printf 'os=%s\n' "$(uname -s)"
printf 'arch=%s\n' "$(uname -m)"
printf 'home=%s\n' "$HOME"
digest_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    return 1
  fi
}
if command -v vellum-remote-agent >/dev/null 2>&1 || [ -x "$HOME/.local/bin/vellum-remote-agent" ]; then echo agent=1; else echo agent=0; fi
if command -v vellum-remote-broker >/dev/null 2>&1 || [ -x "$HOME/.local/bin/vellum-remote-broker" ]; then echo broker=1; else echo broker=0; fi
if [ -x "$HOME/.local/bin/vellum-remote-agent" ]; then printf 'agentSha=%s\n' "$(digest_file "$HOME/.local/bin/vellum-remote-agent")"; fi
if [ -x "$HOME/.local/bin/vellum-remote-broker" ]; then printf 'brokerSha=%s\n' "$(digest_file "$HOME/.local/bin/vellum-remote-broker")"; fi
if docker version --format '{{.Server.Os}}' >/dev/null 2>&1; then echo docker=1; else echo docker=0; fi
if systemctl --user show-environment >/dev/null 2>&1; then echo systemd=1; else echo systemd=0; fi
user=$(id -un)
if [ "$(loginctl show-user "$user" -p Linger --value 2>/dev/null || true)" = yes ]; then echo linger=1; else echo linger=0; fi
uid=$(id -u)
if launchctl print "gui/$uid" >/dev/null 2>&1; then echo gui=1; else echo gui=0; fi
if [ -e "$HOME/.local/state/vellum" ] || [ -e "$HOME/.local/share/vellum" ] || [ -e "$HOME/Library/Application Support/vellum-remote" ] || ls "$HOME/.config/systemd/user"/vellum-* >/dev/null 2>&1; then echo managed=1; else echo managed=0; fi
if command -v df >/dev/null 2>&1; then
  avail=$(df -kP "$HOME" | awk 'NR==2 {print $4}')
  echo "diskFree=$((avail * 1024))"
fi
"#;
    let output = ssh_script(data_root, alias, script.as_bytes(), "sh -s --")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let value = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    Ok(BootstrapProbe {
        os: value("os"),
        arch: value("arch"),
        agent_installed: value("agent") == "1",
        broker_installed: value("broker") == "1",
        docker_available: value("docker") == "1",
        systemd_user_available: value("systemd") == "1",
        linger_enabled: value("linger") == "1",
        managed_footprint_present: value("managed") == "1",
        agent_sha256: Some(value("agentSha")).filter(|value| !value.is_empty()),
        broker_sha256: Some(value("brokerSha")).filter(|value| !value.is_empty()),
        gui_session_available: value("gui") == "1",
        user_home: Some(value("home")).filter(|value| !value.is_empty()),
        managed_codex_home: {
            let user_home = value("home");
            if user_home.is_empty() {
                None
            } else {
                Some(crate::remote::platform::managed_codex_home_posix(
                    &user_home,
                    &value("os"),
                ))
            }
        },
        disk_free_bytes: value("diskFree").parse().ok(),
    })
}

fn install_managed_ssh_isolation(
    data_root: &Path,
    host_id: &str,
    alias: &str,
    probe: &BootstrapProbe,
) -> AppResult<String> {
    let (user, hostname, port) =
        crate::remote::ssh_isolation::resolve_user_host_port(alias).map_err(AppError::Message)?;
    let identity_dir = data_root.join("remote-ssh").join(host_id);
    let identity = identity_dir.join("id_ed25519");
    let public_key = ensure_isolation_identity(&identity)?;
    let ssh_config = dirs::home_dir()
        .map(|home| home.join(".ssh").join("config"))
        .ok_or_else(|| AppError::Message("SshConfigHomeMissing".into()))?;
    let existing_keys = ssh_script(
        data_root,
        alias,
        b"",
        "cat \"$HOME/.ssh/authorized_keys\" 2>/dev/null || true",
    )?;
    let existing_text = String::from_utf8_lossy(&existing_keys.stdout);
    let user_home = probe.user_home.as_deref().unwrap_or("/tmp/vellum-remote");
    let home = probe
        .managed_codex_home
        .as_deref()
        .unwrap_or("/tmp/vellum-remote/codex");
    let install_dir = format!("{}/packages/standalone/current", home.trim_end_matches('/'));
    let extra = format!("{}/.local/bin", user_home.trim_end_matches('/'));
    let applied = crate::remote::ssh_isolation::apply_managed_ssh_isolation(
        crate::remote::ssh_isolation::IsolationApplyRequest {
            ssh_config_path: &ssh_config,
            identity_file: &identity,
            host_id,
            hostname: &hostname,
            user: &user,
            port,
            public_key: public_key.trim(),
            managed_codex_home: home,
            install_dir: &install_dir,
            extra_path: &extra,
            existing_authorized_keys: existing_text.as_ref(),
        },
    )
    .map_err(AppError::Message)?;
    let wrapper_quoted = crate::remote::digest::shell_single_quote(&applied.wrapper_path);
    ssh_script(
        data_root,
        alias,
        applied.wrapper_contents.as_bytes(),
        &format!(
            "umask 077; mkdir -p \"$(dirname {wrapper_quoted})\" && cat > {wrapper_quoted} && chmod 0700 {wrapper_quoted}"
        ),
    )?;
    ssh_script(
        data_root,
        alias,
        applied.authorized_keys.as_bytes(),
        "umask 077; mkdir -p \"$HOME/.ssh\" && cat > \"$HOME/.ssh/authorized_keys\"",
    )?;
    Ok(applied.alias)
}

fn ensure_isolation_identity(path: &Path) -> AppResult<String> {
    let pub_path = path.with_extension("pub");
    if path.is_file() && pub_path.is_file() {
        return fs::read_to_string(&pub_path).map_err(|error| AppError::Message(error.to_string()));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| AppError::Message(error.to_string()))?;
    }
    let status = std::process::Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-f",
            &path.to_string_lossy(),
            "-N",
            "",
            "-q",
        ])
        .status()
        .map_err(|error| AppError::Message(format!("SshKeygenFailed: {error}")))?;
    if !status.success() {
        return Err(AppError::Message("SshKeygenFailed".into()));
    }
    fs::read_to_string(&pub_path).map_err(|error| AppError::Message(error.to_string()))
}

fn ensure_linger(data_root: &Path, alias: &str) -> AppResult<()> {
    let script = r#"set -eu
user=$(id -un)
if [ "$(loginctl show-user "$user" -p Linger --value 2>/dev/null || true)" != yes ]; then
  loginctl enable-linger "$user"
fi
[ "$(loginctl show-user "$user" -p Linger --value)" = yes ]
"#;
    ssh_script(data_root, alias, script.as_bytes(), "sh -s --")?;
    Ok(())
}

fn resolve_bootstrap_artifact(
    bundled: Option<PathBuf>,
    binary_name: &str,
    os: &str,
    arch: &str,
) -> AppResult<ResolvedBootstrapArtifact> {
    if let Some(path) = bundled {
        let digest = file_sha256(&path).ok_or_else(|| {
            AppError::Message(format!("read bootstrap artifact {} failed", path.display()))
        })?;
        verify_release_manifest_digest(&path, &digest)?;
        return Ok(ResolvedBootstrapArtifact {
            path,
            digest,
            remove_after_install: false,
        });
    }

    let manifest = crate::remote::pinned_install::load_verified_manifest().map_err(|error| {
        AppError::Message(format!(
            "RemoteComponentArtifactMissing: expected bundled {binary_name} {os} {arch} release artifact; {error}"
        ))
    })?;
    let artifact = crate::remote::pinned_install::component_artifact_for_os_arch(
        &manifest,
        binary_name,
        os,
        arch,
    )?;
    let path = crate::remote::pinned_install::download_artifact(&artifact)?;
    Ok(ResolvedBootstrapArtifact {
        path,
        digest: artifact.sha256,
        remove_after_install: true,
    })
}

fn install_artifact(
    data_root: &Path,
    alias: &str,
    artifact: &Path,
    binary_name: &str,
    expected_digest: &str,
    disk_free_bytes: Option<u64>,
) -> AppResult<()> {
    if !matches!(
        binary_name,
        "vellum-remote-agent" | "vellum-remote-broker" | "vellum-proxy-daemon"
    ) {
        return Err(AppError::Message("invalid bootstrap artifact name".into()));
    }
    let bytes = fs::read(artifact).map_err(|error| AppError::Message(error.to_string()))?;
    let digest = hex::encode(Sha256::digest(&bytes));
    if !expected_digest.eq_ignore_ascii_case(&digest) {
        return Err(AppError::Message(format!(
            "RemoteArtifactDigestMismatch: expected {expected_digest}, got {digest}"
        )));
    }
    crate::remote::space::gate_replace(disk_free_bytes, bytes.len() as u64)
        .map_err(AppError::Message)?;
    let remote =
        crate::remote::digest::install_artifact_remote_script(binary_name, digest.as_str());
    ssh_script(data_root, alias, &bytes, &remote)?;
    Ok(())
}

fn verify_release_manifest_digest(artifact: &Path, digest: &str) -> AppResult<()> {
    let remote_root = artifact
        .ancestors()
        .find(|candidate| candidate.join("manifest.json").is_file());
    let Some(remote_root) = remote_root else {
        if cfg!(debug_assertions) {
            return Ok(());
        }
        return Err(AppError::Message(
            "RemoteArtifactManifestMissing: release bootstrap is fail-closed".into(),
        ));
    };
    let manifest = crate::remote::pinned_install::load_verified_manifest()?;
    let relative = artifact
        .strip_prefix(remote_root)
        .map_err(|error| AppError::Message(error.to_string()))?
        .to_string_lossy()
        .replace('\\', "/");
    let expected = manifest.resource_digest(&relative).ok_or_else(|| {
        AppError::Message(format!("artifact absent from release manifest: {relative}"))
    })?;
    if !expected.eq_ignore_ascii_case(digest) {
        return Err(AppError::Message(format!(
            "RemoteArtifactDigestMismatch: expected {expected}, got {digest}"
        )));
    }
    Ok(())
}

fn verify_release_manifest_digest_strict(artifact: &Path, digest: &str) -> AppResult<()> {
    let remote_root = artifact
        .ancestors()
        .find(|candidate| candidate.join("manifest.json").is_file())
        .ok_or_else(|| {
            AppError::Message(format!(
                "CleanHostQualificationArtifactOutsideBundle: {} is not contained in the embedded release bundle",
                artifact.display()
            ))
        })?;
    let manifest = crate::remote::pinned_install::load_verified_manifest()?;
    let relative = artifact
        .strip_prefix(remote_root)
        .map_err(|error| AppError::Message(error.to_string()))?
        .to_string_lossy()
        .replace('\\', "/");
    let expected = manifest.resource_digest(&relative).ok_or_else(|| {
        AppError::Message(format!("artifact absent from release manifest: {relative}"))
    })?;
    if !expected.eq_ignore_ascii_case(digest) {
        return Err(AppError::Message(format!(
            "RemoteArtifactDigestMismatch: expected {expected}, got {digest}"
        )));
    }
    Ok(())
}

fn ssh_script(
    data_root: &Path,
    alias: &str,
    stdin: &[u8],
    remote_command: &str,
) -> AppResult<std::process::Output> {
    let trust_target = ssh_trust::resolve_ssh_target(alias)?;
    ssh_trust::require_trust_or_error(data_root, &trust_target)?;

    let mut args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
    ];
    args.extend(ssh_trust::strict_host_key_args(data_root));
    args.push("-T".to_string());
    args.push("--".to_string());
    args.push(alias.to_string());
    args.push(remote_command.to_string());

    let mut child = background_command("ssh")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| AppError::Message(format!("bootstrap SSH failed: {error}")))?;
    child
        .stdin
        .take()
        .ok_or_else(|| AppError::Message("bootstrap SSH stdin unavailable".into()))?
        .write_all(stdin)
        .map_err(|error| AppError::Message(error.to_string()))?;
    let output = child
        .wait_with_output()
        .map_err(|error| AppError::Message(error.to_string()))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(AppError::Message(format!(
            "bootstrap SSH command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

#[cfg(test)]
fn normalize_arch(value: &str) -> Option<&'static str> {
    crate::remote::platform::normalize_arch(value)
}

fn find_artifact(name: &str, bundle_dir: &str) -> Option<PathBuf> {
    let env_key = format!(
        "VELLUM_REMOTE_{}_{}",
        name.trim_start_matches("vellum-remote-")
            .replace('-', "_")
            .to_ascii_uppercase(),
        bundle_dir.replace('-', "_").to_ascii_uppercase()
    );
    if let Some(path) = std::env::var_os(env_key)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
    {
        return Some(path);
    }
    crate::install_paths::remote_resource_roots()
        .into_iter()
        .map(|root| root.join(bundle_dir).join(name))
        .find(|path| path.is_file())
}

fn file_sha256(path: &Path) -> Option<String> {
    fs::read(path)
        .ok()
        .map(|bytes| hex::encode(Sha256::digest(bytes)))
}

fn blocked(
    operation_id: String,
    probe: BootstrapProbe,
    reason: &str,
    repair: &str,
) -> BootstrapResult {
    BootstrapResult {
        operation_id,
        state: "recoveryRequired".into(),
        os: probe.os.clone(),
        arch: probe.arch.clone(),
        agent_installed: probe.agent_installed,
        broker_installed: probe.broker_installed,
        docker_available: probe.docker_available,
        systemd_user_available: probe.systemd_user_available,
        linger_enabled: probe.linger_enabled,
        native_codex: None,
        completed_steps: vec!["host.probed".into()],
        blocked_reasons: vec![reason.into()],
        repair_commands: vec![repair.into()],
        platform: crate::remote::platform::RemotePlatform::from_os_arch(&probe.os, &probe.arch)
            .ok()
            .map(|item| item.artifact_key().into()),
        proxy_backend: None,
        service_manager: None,
        persistence_scope: None,
        managed_codex_home: probe.managed_codex_home,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_installs_managed_ssh_isolation_before_agent_copy() {
        let source = include_str!("bootstrap.rs").replace("\r\n", "\n");
        let iso = source
            .find("install_managed_ssh_isolation(")
            .expect("bootstrap must call install_managed_ssh_isolation");
        let apply = source
            .find("apply_managed_ssh_isolation(")
            .expect("bootstrap isolation must call apply_managed_ssh_isolation");
        let agent = source
            .find("install_artifact(\n            &data_root")
            .expect("agent artifact install_artifact call");
        assert!(iso < agent, "SSH isolation must run before agent copy");
        assert!(
            apply > iso,
            "apply_managed_ssh_isolation is the install helper"
        );
    }

    #[test]
    fn install_artifact_stops_on_space_shortfall_before_ssh() {
        let temp = tempfile::tempdir().unwrap();
        let artifact = temp.path().join("vellum-remote-agent");
        fs::write(&artifact, b"verified bytes").unwrap();
        let digest = hex::encode(Sha256::digest(b"verified bytes"));
        let err = install_artifact(
            temp.path(),
            "must-not-be-contacted",
            &artifact,
            "vellum-remote-agent",
            &digest,
            Some(10),
        )
        .unwrap_err();
        assert!(err.to_string().contains("insufficientDiskSpace"), "{err}");
    }

    #[test]
    fn bootstrap_arch_matrix_is_explicit() {
        assert_eq!(normalize_arch("x86_64"), Some("amd64"));
        assert_eq!(normalize_arch("aarch64"), Some("arm64"));
        assert_eq!(normalize_arch("riscv64"), None);
        let darwin =
            crate::remote::platform::RemotePlatform::from_os_arch("Darwin", "arm64").unwrap();
        assert_eq!(darwin.bundle_dir(), "darwin-arm64");
        assert!(
            crate::remote::platform::RemotePlatform::from_os_arch("Darwin", "x86_64")
                .unwrap_err()
                .ui_unsupported
        );
        assert_eq!(
            crate::remote::digest::parse_digest_output(&format!(
                "{}  file with spaces\n",
                "a".repeat(64)
            )),
            Some("a".repeat(64))
        );
        assert!(crate::remote::digest::remote_file_digest_snippet().contains("shasum -a 256"));
    }

    #[test]
    fn artifact_environment_names_are_not_renderer_data() {
        assert!(find_artifact("not-an-artifact", "amd64").is_none());
    }

    #[test]
    fn bootstrap_rechecks_the_selected_digest_before_opening_ssh() {
        let temp = tempfile::tempdir().unwrap();
        let artifact = temp.path().join("vellum-remote-agent");
        fs::write(&artifact, b"verified bytes").unwrap();
        let data_root = tempfile::tempdir().unwrap();

        let error = install_artifact(
            data_root.path(),
            "must-not-be-contacted",
            &artifact,
            "vellum-remote-agent",
            &"0".repeat(64),
            Some(1024 * 1024 * 1024),
        )
        .unwrap_err();

        assert!(error.to_string().contains("RemoteArtifactDigestMismatch"));
    }

    #[test]
    fn clean_host_gate_rejects_artifacts_outside_the_embedded_bundle() {
        let temp = tempfile::tempdir().unwrap();
        let artifact = temp.path().join("vellum-remote-agent");
        fs::write(&artifact, b"locally built bytes").unwrap();
        let digest = file_sha256(&artifact).unwrap();

        let error = verify_release_manifest_digest_strict(&artifact, &digest).unwrap_err();
        assert!(error
            .to_string()
            .contains("CleanHostQualificationArtifactOutsideBundle"));
    }

    #[test]
    #[ignore = "mutates an explicitly selected live SSH host"]
    fn live_artifact_bootstrap() {
        let alias = std::env::var("VELLUM_LIVE_SSH_ALIAS").expect("live SSH alias");
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().join("desktop"));
        let host = state
            .remote()
            .import_discovered_host(&crate::remote::discovery::RemoteHostCandidate {
                codex_host_id: None,
                vellum_host_id: "live-bootstrap".into(),
                display_name: "live-bootstrap".into(),
                ssh_alias: alias,
                hostname: None,
                user: None,
                port: None,
                source: "live-test".into(),
                validated: true,
                validation_error: None,
            })
            .unwrap();
        let result = bootstrap(&state, &host.id).unwrap();
        assert!(result.agent_installed);
        assert!(
            result.native_codex.is_some(),
            "{:?}",
            result.blocked_reasons
        );
    }

    #[tokio::test]
    #[ignore = "installs the bundled release onto an explicitly selected pristine SSH host"]
    async fn live_bundled_clean_host_bootstrap_qualification() {
        let alias = std::env::var("VELLUM_LIVE_SSH_ALIAS").expect("live SSH alias");
        let state = AppState::new();
        let candidates =
            crate::remote::discovery::discover(&state.remote().list_hosts().unwrap()).unwrap();
        let candidate = candidates
            .into_iter()
            .find(|candidate| candidate.ssh_alias == alias)
            .expect("configured Codex/OpenSSH host");
        state.remote().import_discovered_host(&candidate).unwrap();

        let result = one_click_clean_host_bootstrap(
            &state,
            &candidate.vellum_host_id,
            |phase, percent, message| println!("{percent:3}% {phase}: {message}"),
        )
        .await
        .expect("bundled clean-host bootstrap");
        assert_eq!(result.state, "detachedReady");
        assert!(result.proxy_ready && result.native_daemon_running && result.detached_ready);
    }

    #[tokio::test]
    #[ignore = "converges an explicitly selected live SSH host to detachedReady"]
    async fn live_one_click_bootstrap_is_idempotent() {
        let alias = std::env::var("VELLUM_LIVE_SSH_ALIAS").expect("live SSH alias");
        let state = AppState::new();
        let candidates =
            crate::remote::discovery::discover(&state.remote().list_hosts().unwrap()).unwrap();
        let candidate = candidates
            .into_iter()
            .find(|candidate| candidate.ssh_alias == alias)
            .expect("configured Codex/OpenSSH host");
        state.remote().import_discovered_host(&candidate).unwrap();

        let first = one_click_bootstrap(
            &state,
            &candidate.vellum_host_id,
            |phase, percent, message| println!("{percent:3}% {phase}: {message}"),
        )
        .await
        .expect("first one-click bootstrap");
        assert_eq!(first.state, "detachedReady");
        assert!(first.proxy_ready && first.native_daemon_running && first.detached_ready);

        let second = one_click_bootstrap(
            &state,
            &candidate.vellum_host_id,
            |phase, percent, message| println!("{percent:3}% {phase}: {message}"),
        )
        .await
        .expect("idempotent one-click bootstrap");
        assert_eq!(second.state, "detachedReady");
        assert!(second
            .apply
            .completed_steps
            .iter()
            .any(|step| step == "proxy.alreadyReady"));
        assert!(second
            .apply
            .completed_steps
            .iter()
            .any(|step| step == "nativeAdopt.alreadyActive"));
    }

    #[test]
    fn boundary_key_provisioning_precedes_the_first_bootstrap_native_rpc_in_source_order() {
        let source = include_str!("bootstrap.rs");
        let provision_at = source
            .find("crate::remote::provision_remote_boundary_key(&client, &data_root, host_id, &operation_id)?;")
            .expect("bootstrap() must call provision_remote_boundary_key before starting native Codex");
        let rpc_at = source
            .find("client.codex_bootstrap_native(")
            .expect("bootstrap() must call codex_bootstrap_native");
        let confirm_at = source
            .find("confirm_remote_boundary_key_consumers(")
            .expect("bootstrap() must confirm the started native instance");
        assert!(
            provision_at < rpc_at,
            "boundary-key provisioning must run before the first codex.bootstrapNative RPC"
        );
        assert!(
            rpc_at < confirm_at,
            "native confirmation must follow bootstrap"
        );
    }
}
