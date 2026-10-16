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
    #[serde(default)]
    pub preflight: Option<super::preflight::DeploymentPreflight>,
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
    #[serde(default)]
    pub chatgpt_pairing_suggested: bool,
    #[serde(default)]
    pub omitted_catalog_ids: Vec<String>,
}

/// Prepare an SSH host and converge it to its complete native Remote Manager
/// desired state. The workflow is intentionally idempotent: an already-ready
/// host reuses its saved model selection and each agent mutation has its own
/// deterministic operation id.
pub async fn one_click_bootstrap<F>(
    state: &AppState,
    host_id: &str,
    progress: F,
) -> AppResult<OneClickBootstrapResult>
where
    F: FnMut(&str, u8, &str),
{
    one_click_bootstrap_controlled(state, host_id, progress, None).await
}

pub(crate) async fn one_click_bootstrap_controlled<F>(
    state: &AppState,
    host_id: &str,
    mut progress: F,
    control: Option<&super::converge_control::DeploymentControl>,
) -> AppResult<OneClickBootstrapResult>
where
    F: FnMut(&str, u8, &str),
{
    progress(
        "hostPreflight",
        5,
        "Checking platform, credentials and bundled deployment artifacts",
    );
    let mut prepared = prepare_with_control(state, host_id, &mut progress, control).await?;

    let native_unavailable = prepared
        .blocked_reasons
        .iter()
        .any(|reason| reason.starts_with("nativeCodexUnavailable:"));
    let codex_update = prepared.preflight.as_ref().is_some_and(|report| {
        report
            .auto_fix
            .iter()
            .any(|fix| fix == "codex.installPinned")
    });
    if (native_unavailable || codex_update) && crate::remote::pinned_install::release_status().ready
    {
        check_cancelled(control)?;
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
        prepared = prepare_with_control(state, host_id, &mut progress, control).await?;
    }
    if !prepared.blocked_reasons.is_empty() {
        return Err(AppError::Message(format!(
            "OneClickBootstrapBlocked: {}; repair: {}",
            prepared.blocked_reasons.join(", "),
            prepared.repair_commands.join(" | ")
        )));
    }

    let client = RemoteAgentClient::new(RemoteHostManager::resolve_target(state, host_id)?);
    if let Some(control) = control {
        if wait_for_idle(&client, control, &mut progress).await? {
            prepared = prepare_with_control(state, host_id, &mut progress, Some(control)).await?;
        }
    }
    check_cancelled(control)?;

    progress(
        "deploymentPlan",
        45,
        "Resolving qualified models and credentials",
    );
    let desired = crate::remote::desired_state::load(&state.data_root(), host_id)?;
    let (plan, model_selection) =
        crate::remote::deployment::plan_for_converge(state, host_id, &desired)?;
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
    check_cancelled(control)?;
    let mut applied = crate::remote::deployment::apply(state, host_id, &plan.plan_id).await?;
    check_cancelled(control)?;

    progress(
        "verification",
        92,
        "Verifying proxy, native daemon and detach readiness",
    );
    let expected_port = applied_proxy_port(&applied.proxy);
    let (verified, retried) = verify_with_retry(
        || RemoteHostManager::aggregate_status(state, host_id),
        || {
            check_cancelled(control)?;
            client
                .proxy_start(
                    &format!("{}-verify-start", applied.operation_id),
                    None,
                    None,
                )
                .map(|_| ())
        },
        |status| verify_deployment_status(status, expected_port),
        || {
            progress(
                "verificationRetry",
                96,
                "Starting proxy and retrying verification once",
            )
        },
    )?;
    check_cancelled(control)?;
    if retried {
        applied.completed_steps.push("verification.retried".into());
    }
    // Return the observed service, rather than the possibly stale apply reply.
    let observed_proxy = verified
        .agent
        .as_ref()
        .and_then(|agent| agent.get("proxy"))
        .cloned()
        .ok_or_else(|| {
            AppError::Message("OneClickBootstrapVerificationFailed: proxy status missing".into())
        })?;
    if let Some(detail) = applied.proxy.get_mut("detail") {
        *detail = observed_proxy;
    } else {
        applied.proxy = observed_proxy;
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
        proxy_ready: true,
        native_daemon_running: true,
        detached_ready: true,
        chatgpt_pairing_suggested: model_selection.chatgpt_pairing_suggested,
        omitted_catalog_ids: model_selection.omitted_catalog_ids,
    })
}

fn applied_proxy_port(proxy: &serde_json::Value) -> Option<u64> {
    // proxy.start returns an OperationResult; no-op apply returns raw status.
    proxy
        .get("detail")
        .unwrap_or(proxy)
        .get("hostPort")
        .and_then(serde_json::Value::as_u64)
}

/// The retry boundary deliberately exposes only observation and idempotent
/// start. Install, configure, credential sync and takeover cannot run again.
fn verify_with_retry<T>(
    mut observe: impl FnMut() -> AppResult<T>,
    mut start: impl FnMut() -> AppResult<()>,
    validate: impl Fn(&T) -> AppResult<()>,
    mut retry_progress: impl FnMut(),
) -> AppResult<(T, bool)> {
    let mut read = || -> AppResult<T> {
        let status = observe()?;
        validate(&status)?;
        Ok(status)
    };
    if let Ok(status) = read() {
        return Ok((status, false));
    }
    retry_progress();
    // An SSH reply may be lost after start succeeded. Always observe once
    // more, even if start returned an error; readiness is the authority.
    let start_failed = start().is_err();
    read().map(|status| (status, true)).map_err(|error: AppError| AppError::Message(format!(
        "OneClickBootstrapVerificationFailed: retryExhausted; retryStartFailed={start_failed}; {error}"
    )))
}

fn verify_deployment_status(
    status: &crate::remote::RemoteHostAggregateStatus,
    expected_port: Option<u64>,
) -> AppResult<()> {
    let agent = status.agent.as_ref();
    let proxy_ready = agent
        .and_then(|agent| agent.pointer("/proxy/ready"))
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    let native_running = agent
        .and_then(|agent| agent.pointer("/nativeCodex/daemonRunning"))
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    let actual_port = agent
        .and_then(|agent| agent.pointer("/proxy/hostPort"))
        .and_then(serde_json::Value::as_u64);
    let port_matches = expected_port.is_some_and(|port| (1..=65535).contains(&port))
        && actual_port == expected_port;
    if !proxy_ready || !native_running || status.manager_state != "detachedReady" || !port_matches {
        return Err(AppError::Message(format!(
            "OneClickBootstrapVerificationFailed: managerState={}, proxyReady={proxy_ready}, nativeDaemonRunning={native_running}, proxyPortMatches={port_matches}", status.manager_state
        )));
    }
    Ok(())
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
    bootstrap_with_progress(state, host_id, |_| Ok(()), None)
}

fn bootstrap_with_progress(
    state: &AppState,
    host_id: &str,
    mut before_write: impl FnMut(&super::preflight::DeploymentPreflight) -> AppResult<()>,
    control: Option<&super::converge_control::DeploymentControl>,
) -> AppResult<BootstrapResult> {
    super::preflight::before_deployment(
        || inspect_bootstrap(state, host_id),
        |report, context| {
            before_write(&report)?;
            bootstrap_prepared(state, host_id, context, report, control)
        },
    )
}

fn check_cancelled(control: Option<&super::converge_control::DeploymentControl>) -> AppResult<()> {
    control.map_or(Ok(()), |control| control.check())
}

struct NativeWaitClient<'a> {
    client: &'a RemoteAgentClient,
}
impl super::converge_control::WaitClient for NativeWaitClient<'_> {
    fn observe(&self) -> AppResult<(serde_json::Value, Option<serde_json::Value>)> {
        let status = self.client.host_status()?;
        let sessions = if status
            .pointer("/nativeCodex/daemonRunning")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            Some(self.client.codex_session_status(None)?)
        } else {
            None
        };
        Ok((status, sessions))
    }
    fn stop_app_owned(&self, native: &serde_json::Value) -> AppResult<()> {
        self.client
            .codex_stop_app_owned_expected(
                &format!("converge-takeover-{}", ulid::Ulid::new()),
                native,
            )
            .map(|_| ())
    }
}

async fn wait_for_idle(
    client: &RemoteAgentClient,
    control: &super::converge_control::DeploymentControl,
    progress: &mut impl FnMut(&str, u8, &str),
) -> AppResult<bool> {
    use super::converge_control::{step, WaitStep};
    let client = NativeWaitClient { client };
    loop {
        match step(control, &client)? {
            WaitStep::Ready => return Ok(false),
            WaitStep::TookOver => return Ok(true),
            WaitStep::WaitingForIdle => progress(
                "waitingForIdle",
                7,
                "Waiting for Codex turns, tools and approvals to finish",
            ),
            WaitStep::AwaitingTakeover => progress(
                "awaitingTakeover",
                8,
                "Confirm takeover of the idle Codex App server",
            ),
        }
        control.wait().await;
    }
}

async fn prepare_with_control(
    state: &AppState,
    host_id: &str,
    progress: &mut impl FnMut(&str, u8, &str),
    control: Option<&super::converge_control::DeploymentControl>,
) -> AppResult<BootstrapResult> {
    loop {
        check_cancelled(control)?;
        let result = bootstrap_with_progress(
            state,
            host_id,
            |_| {
                check_cancelled(control)?;
                progress("autoFix", 10, "Converging approved deployment repairs");
                Ok(())
            },
            control,
        );
        match result {
            Err(error)
                if control.is_some()
                    && [
                        "activeWorkInProgress",
                        "activeTurnInProgress",
                        "nativeDaemonAppOwned",
                        "incompleteObservation",
                    ]
                    .iter()
                    .any(|code| error.to_string().contains(code)) =>
            {
                let client =
                    RemoteAgentClient::new(RemoteHostManager::resolve_target(state, host_id)?);
                wait_for_idle(
                    &client,
                    control
                        .ok_or_else(|| AppError::Message("DeploymentControlUnavailable".into()))?,
                    progress,
                )
                .await?;
            }
            result => return result,
        }
    }
}

struct BootstrapContext {
    probe: BootstrapProbe,
    target: super::ResolvedAgentTarget,
    agent_digest: String,
}

fn inspect_bootstrap(
    state: &AppState,
    host_id: &str,
) -> AppResult<(super::preflight::DeploymentPreflight, BootstrapContext)> {
    inspect_bootstrap_with(state, host_id, &SshBootstrapProbe)
}

trait BootstrapProbeClient {
    fn probe(&self, data_root: &Path, alias: &str) -> AppResult<BootstrapProbe>;
    fn ps(&self, data_root: &Path, alias: &str) -> AppResult<super::preflight::PsProbeOutput>;
}

struct SshBootstrapProbe;
impl BootstrapProbeClient for SshBootstrapProbe {
    fn probe(&self, data_root: &Path, alias: &str) -> AppResult<BootstrapProbe> {
        probe(data_root, alias)
    }
    fn ps(&self, data_root: &Path, alias: &str) -> AppResult<super::preflight::PsProbeOutput> {
        super::preflight::PsProcessClient::probe_start_time(&SshPsProcessClient {
            data_root,
            alias,
        })
    }
}

fn inspect_bootstrap_with(
    state: &AppState,
    host_id: &str,
    transport: &dyn BootstrapProbeClient,
) -> AppResult<(super::preflight::DeploymentPreflight, BootstrapContext)> {
    let target = RemoteHostManager::resolve_target(state, host_id)?;
    let alias = target
        .ssh_destination
        .as_deref()
        .ok_or_else(|| AppError::Message("bootstrap requires an SSH destination".into()))?;
    let data_root = state.data_root();
    let probe = transport.probe(&data_root, alias)?;
    let mut report = super::preflight::DeploymentPreflight::default();
    let platform = match super::platform::RemotePlatform::from_os_arch(&probe.os, &probe.arch) {
        Ok(platform) => platform,
        Err(error) => {
            report.blockers.push(error.code.into());
            report
                .repairs
                .insert(error.code.into(), error.message.clone());
            report.repair_commands.push(error.message);
            return Ok((
                report,
                BootstrapContext {
                    probe,
                    target,
                    agent_digest: String::new(),
                },
            ));
        }
    };
    struct PsAdapter<'a> {
        transport: &'a dyn BootstrapProbeClient,
        root: &'a Path,
        alias: &'a str,
    }
    impl super::preflight::PsProcessClient for PsAdapter<'_> {
        fn probe_start_time(&self) -> AppResult<super::preflight::PsProbeOutput> {
            self.transport.ps(self.root, self.alias)
        }
    }
    if let Some(blocker) = check_linux_prerequisites(
        &probe,
        &PsAdapter {
            transport,
            root: &data_root,
            alias,
        },
    )? {
        report.blockers.push(blocker.code.into());
        report
            .repairs
            .insert(blocker.code.into(), blocker.repair.into());
        report.repair_commands.push(blocker.repair.into());
        return Ok((
            report,
            BootstrapContext {
                probe,
                target,
                agent_digest: String::new(),
            },
        ));
    }
    if platform.service_manager() == super::platform::SERVICE_MANAGER_LAUNCHD
        && !probe.gui_session_available
    {
        report.blockers.push("guiSessionUnavailable".into());
    }
    if !report.blockers.is_empty() {
        return Ok((
            report,
            BootstrapContext {
                probe,
                target,
                agent_digest: String::new(),
            },
        ));
    }
    super::deployment::preflight_models(state, host_id)?;
    let manifest = super::pinned_install::load_verified_manifest()
        .map_err(|error| AppError::Message(format!("bundledReleaseUnavailable: {error}")))?;
    let agent_artifact = super::pinned_install::component_artifact_for_os_arch(
        &manifest,
        "vellum-remote-agent",
        &probe.os,
        &probe.arch,
    )?;
    let proxy_artifact = super::pinned_install::component_artifact_for_os_arch(
        &manifest,
        "vellum-proxy-daemon",
        &probe.os,
        &probe.arch,
    )?;
    let codex_artifact = super::pinned_install::artifact_for_os_arch(
        &manifest.codex.artifacts,
        &probe.os,
        &probe.arch,
    )?;
    let mut payload = 0u64;
    for artifact in [&agent_artifact, &proxy_artifact, codex_artifact] {
        let size = super::pinned_install::inspect_bundled_artifact(artifact)
            .map_err(|error| AppError::Message(format!("bundledReleaseUnavailable: {error}")))?;
        payload = payload.checked_add(size).ok_or_else(|| {
            AppError::Message("insufficientDiskSpace: release size overflow".into())
        })?;
    }
    super::space::gate_replace(probe.disk_free_bytes, payload).map_err(AppError::Message)?;
    let expected_agent = agent_artifact
        .sha256
        .strip_prefix("sha256:")
        .unwrap_or(&agent_artifact.sha256);
    if !probe.agent_installed || probe.agent_sha256.as_deref() != Some(expected_agent) {
        report.auto_fix.extend([
            "agent.update".into(),
            "native.observeAfterAgentUpdate".into(),
        ]);
    } else {
        let client = RemoteAgentClient::new(target.clone());
        let status = client.host_status()?;
        inspect_existing_agent(&client, &status, &manifest, state, host_id, &mut report)?;
    }
    report
        .auto_fix
        .extend(["credentials.provision".into(), "proxy.converge".into()]);
    report.auto_fix.sort();
    report.auto_fix.dedup();
    Ok((
        report,
        BootstrapContext {
            probe,
            target,
            agent_digest: agent_artifact.sha256,
        },
    ))
}

fn bootstrap_prepared(
    state: &AppState,
    host_id: &str,
    context: BootstrapContext,
    preflight: super::preflight::DeploymentPreflight,
    control: Option<&super::converge_control::DeploymentControl>,
) -> AppResult<BootstrapResult> {
    let BootstrapContext {
        probe,
        target,
        agent_digest,
    } = context;
    let alias = target
        .ssh_destination
        .clone()
        .ok_or_else(|| AppError::Message("bootstrap requires SSH".into()))?;
    let operation_id = format!("bootstrap-{}", ulid::Ulid::new());
    let data_root = state.data_root();
    let platform = super::platform::RemotePlatform::from_os_arch(&probe.os, &probe.arch)
        .map_err(|error| AppError::Message(error.code.into()))?;
    let normalized_arch = platform.bundle_dir();
    let remote_broker_sha256 = probe.broker_sha256.clone();
    let mut result = BootstrapResult {
        preflight: Some(preflight),
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

    let agent_needs_install = result
        .preflight
        .as_ref()
        .is_some_and(|report| report.auto_fix.iter().any(|fix| fix == "agent.update"));
    let agent_artifact = if agent_needs_install {
        let artifact = resolve_bootstrap_artifact(
            find_artifact("vellum-remote-agent", normalized_arch),
            "vellum-remote-agent",
            &probe.os,
            &probe.arch,
        )?;
        if artifact.digest.trim_start_matches("sha256:")
            != agent_digest.trim_start_matches("sha256:")
        {
            return Err(AppError::Message(
                "bundledReleaseUnavailable: selected agent differs from signed artifact".into(),
            ));
        }
        Some(artifact)
    } else {
        None
    };
    check_cancelled(control)?;
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

    if let Some(artifact) = agent_artifact {
        check_cancelled(control)?;
        install_artifact(
            &data_root,
            &alias,
            &artifact.path,
            "vellum-remote-agent",
            &agent_digest,
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
    check_cancelled(control)?;
    client.agent_version()?;
    result.completed_steps.push("agent.compatible".into());
    // A fresh/updated Agent can now observe the real native state and port.
    // Recheck before boundary credentials, linger or Codex lifecycle writes.
    let manifest = super::pinned_install::load_verified_manifest()?;
    let observed = client.host_status()?;
    let mut rechecked = super::preflight::DeploymentPreflight::default();
    inspect_existing_agent(
        &client,
        &observed,
        &manifest,
        state,
        host_id,
        &mut rechecked,
    )?;
    super::preflight::before_deployment(
        || Ok((rechecked, ())),
        |report, _| {
            if let Some(preflight) = result.preflight.as_mut() {
                preflight.auto_fix.extend(report.auto_fix);
                preflight.auto_fix.sort();
                preflight.auto_fix.dedup();
            }
            Ok(())
        },
    )?;
    if platform.docker_is_blocker() && !result.docker_available {
        result.blocked_reasons.push("dockerUnavailable".into());
        result.repair_commands.push(
            "依 Linux 發行版安裝 Docker，並將目前使用者加入可執行 docker 的群組後重新登入。".into(),
        );
    }
    check_cancelled(control)?;
    if platform.service_manager() == crate::remote::platform::SERVICE_MANAGER_SYSTEMD_USER {
        let (scope, linger) =
            linux_persistence_scope(result.systemd_user_available, result.linger_enabled, || {
                ensure_linger(&data_root, &alias).is_ok()
            });
        result.persistence_scope = Some(scope.into());
        result.linger_enabled = linger;
        if scope == "onDemand" {
            result.service_manager = Some("agent-supervisor".into());
            result
                .completed_steps
                .push("host.onDemandPersistence".into());
        } else if scope == "login" {
            result.completed_steps.push("host.loginPersistence".into());
        } else if !probe.linger_enabled {
            result.completed_steps.push("host.lingerEnabled".into());
        }
    } else if !probe.gui_session_available {
        result.blocked_reasons.push("guiSessionUnavailable".into());
        result.repair_commands.push(
            "登入 macOS 圖形工作階段後再部署。Remote Proxy 是登入後常駐，不會改電源或自動登入。"
                .into(),
        );
    }
    check_cancelled(control)?;
    crate::remote::provision_remote_boundary_key(&client, &data_root, host_id, &operation_id)?;
    result
        .completed_steps
        .push("credentials.boundaryReady".into());
    check_cancelled(control)?;
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

fn inspect_existing_agent(
    client: &RemoteAgentClient,
    status: &serde_json::Value,
    manifest: &super::pinned_install::ReleaseManifest,
    state: &AppState,
    host_id: &str,
    report: &mut super::preflight::DeploymentPreflight,
) -> AppResult<()> {
    let desired = super::desired_state::load(&state.data_root(), host_id)?;
    client.proxy_select_port(desired.proxy_port)?;
    let sessions = if status
        .pointer("/nativeCodex/daemonRunning")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        Some(client.codex_session_status(None)?)
    } else {
        None
    };
    classify_existing_agent(
        status,
        sessions.as_ref(),
        &manifest.codex.pinned_version,
        report,
    );
    if !report.blockers.is_empty() {
        return Ok(());
    }
    if status
        .pointer("/nativeCodex/compatible")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        super::deployment::select_for_converge(state, host_id, &desired)?;
    } else {
        // Account RPCs need a usable Codex install. Local credential eligibility
        // was checked already; observe the remote grant after pinned install.
        report
            .auto_fix
            .push("models.observeAfterCodexInstall".into());
    }
    Ok(())
}

fn classify_existing_agent(
    status: &serde_json::Value,
    sessions: Option<&serde_json::Value>,
    pinned_version: &str,
    report: &mut super::preflight::DeploymentPreflight,
) {
    use serde_json::Value;
    match status
        .pointer("/configuration/state")
        .and_then(Value::as_str)
    {
        Some("incompatible") => report
            .blockers
            .push("proxyConfigurationSchemaTooNew".into()),
        Some("unreadable") => report.blockers.push("proxyConfigurationUnreadable".into()),
        Some("current") => {}
        _ => report.auto_fix.push("proxy.configure".into()),
    }
    let native = status.get("nativeCodex");
    let version_matches = native
        .and_then(|native| native.get("codexVersion").or_else(|| native.get("version")))
        .and_then(Value::as_str)
        .and_then(|version| version.split_whitespace().last())
        == Some(pinned_version);
    if !version_matches
        || native
            .and_then(|native| native.get("compatible"))
            .and_then(Value::as_bool)
            != Some(true)
    {
        report.auto_fix.push("codex.installPinned".into());
    }
    if native
        .and_then(|native| native.get("daemonRunning"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        if let Err(code) = super::observation::require_idle_for_destructive_op(sessions, native) {
            report.blockers.push(code.into());
        }
        if native
            .and_then(|native| native.get("restartSafe"))
            .and_then(Value::as_bool)
            != Some(true)
        {
            report.blockers.push("nativeDaemonAppOwned".into());
        }
    }
}

#[derive(Debug, Default)]
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
    distribution: String,
    distribution_like: String,
}

struct SshPsProcessClient<'a> {
    data_root: &'a Path,
    alias: &'a str,
}

impl super::preflight::PsProcessClient for SshPsProcessClient<'_> {
    fn probe_start_time(&self) -> AppResult<super::preflight::PsProbeOutput> {
        let output = ssh_script(
            self.data_root,
            self.alias,
            super::preflight::PS_PROBE_SCRIPT.as_bytes(),
            "sh -s --",
        )?;
        super::preflight::parse_ps_probe_output(&String::from_utf8_lossy(&output.stdout))
    }
}

fn check_linux_prerequisites(
    probe: &BootstrapProbe,
    client: &dyn super::preflight::PsProcessClient,
) -> AppResult<Option<super::preflight::PrerequisiteBlocker>> {
    super::preflight::ps_start_time_preflight(
        &probe.os,
        &probe.distribution,
        &probe.distribution_like,
        client,
    )
}

/// Direct apply must enforce the same read-only gate as bootstrap, before any
/// agent mutation. Local development targets have no SSH host to provision.
pub(crate) fn preflight_linux_host(state: &AppState, host_id: &str) -> AppResult<()> {
    let target = RemoteHostManager::resolve_target(state, host_id)?;
    let Some(alias) = target.ssh_destination else {
        return Ok(());
    };
    let data_root = state.data_root();
    let probe = probe(&data_root, &alias)?;
    if let Some(blocker) = check_linux_prerequisites(
        &probe,
        &SshPsProcessClient {
            data_root: &data_root,
            alias: &alias,
        },
    )? {
        return Err(AppError::Message(format!(
            "DeploymentPreflightBlocked: {}; repair: {}",
            blocker.code, blocker.repair
        )));
    }
    Ok(())
}

fn probe(data_root: &Path, alias: &str) -> AppResult<BootstrapProbe> {
    let script = r#"set -eu
printf 'os=%s\n' "$(uname -s)"
printf 'arch=%s\n' "$(uname -m)"
printf 'home=%s\n' "$HOME"
if [ -r /etc/os-release ]; then
  awk -F= '$1 == "ID" { gsub(/[\042\047]/, "", $2); print "distribution=" $2 } $1 == "ID_LIKE" { gsub(/[\042\047]/, "", $2); print "distributionLike=" $2 }' /etc/os-release
fi
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
        distribution: value("distribution"),
        distribution_like: value("distributionLike"),
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
    ssh_script(
        data_root,
        alias,
        applied.wrapper_contents.as_bytes(),
        &applied.wrapper_upload_command(),
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
    if path.is_file() {
        if pub_path.is_file() {
            return fs::read_to_string(&pub_path)
                .map_err(|error| AppError::Message(error.to_string()));
        }
        // Hosts already trust this private key. Generating over it would make
        // ssh-keygen ask whether to overwrite, and would lock those hosts out.
        let output = background_command("ssh-keygen")
            .arg("-y")
            .arg("-f")
            .arg(path)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| AppError::Message(format!("SshKeygenFailed: {error}")))?;
        if !output.status.success() {
            return Err(AppError::Message(format!(
                "SshKeygenFailed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let public = String::from_utf8(output.stdout)
            .map_err(|_| AppError::Message("SshKeygenFailed: public key is not UTF-8".into()))?;
        crate::enhanced_runtime::atomic::write_atomic(&pub_path, public.as_bytes())
            .map_err(|error| AppError::Message(error.to_string()))?;
        return Ok(public);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| AppError::Message(error.to_string()))?;
    }
    let status = background_command("ssh-keygen")
        .stdin(Stdio::null())
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

fn linux_persistence_scope(
    systemd: bool,
    linger: bool,
    enable_linger: impl FnOnce() -> bool,
) -> (&'static str, bool) {
    if !systemd {
        return ("onDemand", false);
    }
    if linger || enable_linger() {
        ("linger", true)
    } else {
        ("login", false)
    }
}

fn ensure_linger(data_root: &Path, alias: &str) -> AppResult<()> {
    let script = r#"set -eu
user=$(id -un)
if [ "$(loginctl show-user "$user" -p Linger --value 2>/dev/null || true)" != yes ]; then
  loginctl --no-ask-password enable-linger "$user"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_bootstrap_inspection_on_fresh_unsupported_or_busybox_host_only_reads_transport() {
        struct FakeTransport {
            os: &'static str,
            calls: std::cell::RefCell<Vec<&'static str>>,
        }
        impl BootstrapProbeClient for FakeTransport {
            fn probe(&self, _: &Path, _: &str) -> AppResult<BootstrapProbe> {
                self.calls.borrow_mut().push("read:probe");
                Ok(BootstrapProbe {
                    os: self.os.into(),
                    arch: "aarch64".into(),
                    ..Default::default()
                })
            }
            fn ps(&self, _: &Path, _: &str) -> AppResult<super::super::preflight::PsProbeOutput> {
                self.calls.borrow_mut().push("read:ps");
                Ok(super::super::preflight::PsProbeOutput {
                    success: false,
                    stdout: "BusyBox ps".into(),
                })
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        state
            .remote()
            .import_discovered_host(&super::super::discovery::RemoteHostCandidate {
                codex_host_id: None,
                vellum_host_id: "fake-preflight".into(),
                display_name: "fake".into(),
                ssh_alias: "fake-only".into(),
                hostname: None,
                user: None,
                port: None,
                source: "test".into(),
                validated: false,
                validation_error: None,
            })
            .unwrap();
        for (os, code, expected) in [
            (
                "unsupported",
                "unsupportedRemotePlatform",
                vec!["read:probe"],
            ),
            (
                "linux",
                "psStartTimeUnavailable",
                vec!["read:probe", "read:ps"],
            ),
        ] {
            let transport = FakeTransport {
                os,
                calls: Default::default(),
            };
            let result = super::super::preflight::before_deployment(
                || inspect_bootstrap_with(&state, "fake-preflight", &transport),
                |_, _| {
                    transport.calls.borrow_mut().push("write:deployment");
                    Ok(())
                },
            );
            assert!(result.unwrap_err().to_string().contains(code));
            assert_eq!(*transport.calls.borrow(), expected);
        }
    }

    #[test]
    fn existing_preflight_classifies_repairs_without_replacing_incompatible_configuration() {
        for (state, fix, blocker) in [
            ("current", false, None),
            ("upgradeRequired", true, None),
            ("repairRequired", true, None),
            (
                "incompatible",
                false,
                Some("proxyConfigurationSchemaTooNew"),
            ),
            ("unreadable", false, Some("proxyConfigurationUnreadable")),
        ] {
            let mut report = super::super::preflight::DeploymentPreflight::default();
            classify_existing_agent(
                &serde_json::json!({"configuration": {"state": state}, "nativeCodex": {"codexVersion": "codex-cli pinned", "compatible": true, "daemonRunning": false}}),
                None,
                "pinned",
                &mut report,
            );
            assert_eq!(
                report.auto_fix.iter().any(|fix| fix == "proxy.configure"),
                fix
            );
            assert!(!report
                .auto_fix
                .iter()
                .any(|fix| fix == "codex.installPinned"));
            assert_eq!(report.blockers.first().map(String::as_str), blocker);
        }
    }

    #[test]
    fn existing_preflight_requires_complete_idle_observation_before_deployment_writes() {
        let native = serde_json::json!({"nativeCodex": {"daemonRunning": true, "restartSafe": false, "compatible": false}});
        let active = serde_json::json!({"observability": "nativeAppServer", "threads": [{"active": true, "activeTools": false, "pendingApprovals": false}]});
        let mut report = super::super::preflight::DeploymentPreflight::default();
        classify_existing_agent(&native, Some(&active), "pinned", &mut report);
        assert!(report.blockers.contains(&"activeWorkInProgress".into()));
        assert!(report.blockers.contains(&"nativeDaemonAppOwned".into()));
        assert!(report.auto_fix.contains(&"codex.installPinned".into()));
        report.blockers.clear();
        classify_existing_agent(&native, None, "pinned", &mut report);
        assert!(report.blockers.contains(&"incompleteObservation".into()));
    }

    #[test]
    fn verification_reads_the_port_from_start_replies_and_noop_status() {
        assert_eq!(
            applied_proxy_port(&serde_json::json!({"hostPort": 15730})),
            Some(15730)
        );
        assert_eq!(
            applied_proxy_port(&serde_json::json!({"detail": {"hostPort": 15731}})),
            Some(15731)
        );
        assert_eq!(applied_proxy_port(&serde_json::json!({})), None);
    }

    fn verification_status(
        manager: &str,
        proxy_ready: bool,
        daemon_running: bool,
        port: u16,
    ) -> crate::remote::RemoteHostAggregateStatus {
        crate::remote::RemoteHostAggregateStatus {
            host_id: "fake-host".into(),
            manager_state: manager.into(),
            cached_host: None,
            agent: Some(serde_json::json!({
                "proxy": {"ready": proxy_ready, "hostPort": port},
                "nativeCodex": {"daemonRunning": daemon_running},
            })),
            agent_error: None,
            available_actions: vec![],
            blocked_reasons: vec![],
            inventory: None,
            chatgpt: None,
        }
    }

    #[test]
    fn verification_already_ready_does_not_start_or_retry() {
        let calls = std::cell::Cell::new(0);
        let (_, retried) = verify_with_retry(
            || {
                calls.set(calls.get() + 1);
                Ok(verification_status("detachedReady", true, true, 15730))
            },
            || panic!("ready host must not start again"),
            |status| verify_deployment_status(status, Some(15730)),
            || panic!("ready host must not report retry"),
        )
        .unwrap();
        assert!(!retried);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn verification_retries_only_observation_and_start_once() {
        let calls = std::cell::RefCell::new(Vec::new());
        let mut observations = std::collections::VecDeque::from([
            verification_status("drifted", false, true, 15730),
            verification_status("detachedReady", true, true, 15730),
        ]);
        let (_, retried) = verify_with_retry(
            || {
                calls.borrow_mut().push("verify");
                Ok(observations.pop_front().expect("only two observations"))
            },
            || {
                calls.borrow_mut().push("start");
                Ok(())
            },
            |status| verify_deployment_status(status, Some(15730)),
            || calls.borrow_mut().push("retry-progress"),
        )
        .unwrap();
        assert!(retried);
        assert_eq!(
            *calls.borrow(),
            vec!["verify", "retry-progress", "start", "verify"]
        );
    }

    #[test]
    fn verification_transport_failure_and_lost_start_reply_are_reobserved() {
        let mut observations = std::collections::VecDeque::from([
            Err(AppError::Message("SSH reply lost".into())),
            Ok(verification_status("detachedReady", true, true, 15730)),
        ]);
        let starts = std::cell::Cell::new(0);
        let (_, retried) = verify_with_retry(
            || observations.pop_front().expect("only two observations"),
            || {
                starts.set(starts.get() + 1);
                Err(AppError::Message("start reply lost".into()))
            },
            |status| verify_deployment_status(status, Some(15730)),
            || {},
        )
        .unwrap();
        assert!(retried);
        assert_eq!(starts.get(), 1);
    }

    #[test]
    fn verification_exhaustion_is_bounded_and_preserves_final_read_failure() {
        let reads = std::cell::Cell::new(0);
        let starts = std::cell::Cell::new(0);
        let result: AppResult<((), bool)> = verify_with_retry(
            || {
                reads.set(reads.get() + 1);
                Err(AppError::Message("host still unreachable".into()))
            },
            || {
                starts.set(starts.get() + 1);
                Err(AppError::Message("start failed".into()))
            },
            |_| Ok(()),
            || {},
        );
        let error = result.unwrap_err().to_string();
        assert!(error.contains("OneClickBootstrapVerificationFailed: retryExhausted"));
        assert!(error.contains("retryStartFailed=true"));
        assert!(error.contains("host still unreachable"));
        assert_eq!((reads.get(), starts.get()), (2, 1));
    }

    #[test]
    fn verification_requires_detached_daemon_ready_proxy_and_original_port() {
        for status in [
            verification_status("nativeActive", true, true, 15730),
            verification_status("detachedReady", false, true, 15730),
            verification_status("detachedReady", true, false, 15730),
            verification_status("detachedReady", true, true, 15731),
        ] {
            assert!(verify_deployment_status(&status, Some(15730)).is_err());
        }
        let ready = verification_status("detachedReady", true, true, 15730);
        assert!(verify_deployment_status(&ready, None).is_err());
        let mut missing = ready;
        missing.agent = None;
        assert!(verify_deployment_status(&missing, Some(15730)).is_err());
    }

    #[test]
    fn verification_does_not_report_recovery_when_start_picks_a_different_port() {
        let mut observations = std::collections::VecDeque::from([
            verification_status("drifted", false, true, 15730),
            verification_status("detachedReady", true, true, 15731),
        ]);
        let result = verify_with_retry(
            || Ok(observations.pop_front().unwrap()),
            || Ok(()),
            |status| verify_deployment_status(status, Some(15730)),
            || {},
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("proxyPortMatches=false"));
    }

    #[test]
    fn persistence_falls_back_without_blocking_or_prompting_for_privileges() {
        assert_eq!(
            linux_persistence_scope(false, false, || panic!("no systemd: do not enable linger")),
            ("onDemand", false)
        );
        assert_eq!(
            linux_persistence_scope(true, true, || panic!("linger already enabled")),
            ("linger", true)
        );
        assert_eq!(
            linux_persistence_scope(true, false, || false),
            ("login", false)
        );
        assert_eq!(
            linux_persistence_scope(true, false, || true),
            ("linger", true)
        );
    }

    #[test]
    fn linux_ps_preflight_is_independent_of_systemd() {
        struct FakePs {
            success: bool,
            stdout: &'static str,
        }
        impl super::super::preflight::PsProcessClient for FakePs {
            fn probe_start_time(&self) -> AppResult<super::super::preflight::PsProbeOutput> {
                Ok(super::super::preflight::PsProbeOutput {
                    success: self.success,
                    stdout: self.stdout.into(),
                })
            }
        }
        for systemd in [false, true] {
            let probe = BootstrapProbe {
                os: "Linux".into(),
                systemd_user_available: systemd,
                distribution: "debian".into(),
                ..Default::default()
            };
            for stdout in ["sh: 2: ps: not found", "ps: unrecognized option: p"] {
                let blocker = check_linux_prerequisites(
                    &probe,
                    &FakePs {
                        success: false,
                        stdout,
                    },
                )
                .unwrap()
                .unwrap();
                assert_eq!(blocker.code, "psStartTimeUnavailable");
                assert_eq!(blocker.repair, "apt-get install procps");
            }
            assert!(check_linux_prerequisites(
                &probe,
                &FakePs {
                    success: true,
                    stdout: "Fri Oct 16 10:00:00 2026",
                }
            )
            .unwrap()
            .is_none());
        }
    }

    #[test]
    fn ps_preflight_runs_before_remote_installation() {
        let source = include_str!("bootstrap.rs");
        let preflight = source.find("check_linux_prerequisites(").unwrap();
        let isolation = source
            .find("install_managed_ssh_isolation(&data_root")
            .unwrap();
        assert!(
            preflight < isolation,
            "preflight must precede the first remote mutation"
        );
    }

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
    fn an_isolation_key_without_its_public_half_is_kept_not_regenerated() {
        let temp = tempfile::tempdir().unwrap();
        let identity = temp.path().join("vellum_ed25519");
        let created = ensure_isolation_identity(&identity).unwrap();
        let private = fs::read(&identity).unwrap();
        fs::remove_file(identity.with_extension("pub")).unwrap();

        let recovered = ensure_isolation_identity(&identity).unwrap();

        assert_eq!(fs::read(&identity).unwrap(), private);
        let key = |line: &str| {
            line.split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(key(&recovered), key(&created));
        assert_eq!(
            fs::read_to_string(identity.with_extension("pub")).unwrap(),
            recovered
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
