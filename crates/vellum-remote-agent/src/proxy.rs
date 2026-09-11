use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::docker::{
    resolve_image_with, ContainerSummary, DockerClient, ManagedContainerLabels,
    ProcessDockerClient, RunProxySpec,
};
use crate::mutation_lock::HostMutationGuard;
use crate::operations::{BeginOutcome, OperationRecord};
use crate::permissions::prepare_proxy_mounts;
use crate::protocol::{OperationResult, ProxyStatusView};
use crate::state::{AgentStateStore, InstallRecord};

#[cfg(not(test))]
const PROXY_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(test)]
const PROXY_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct ProxyStartRequest {
    pub operation_id: String,
    pub host_port: Option<u16>,
    pub image: Option<String>,
}

pub struct ProxyManager {
    store: AgentStateStore,
    docker: Arc<dyn DockerClient>,
}

impl ProxyManager {
    pub fn new(store: AgentStateStore) -> Self {
        Self {
            store,
            docker: Arc::new(ProcessDockerClient::new()),
        }
    }

    pub fn with_docker(store: AgentStateStore, docker: Arc<dyn DockerClient>) -> Self {
        Self { store, docker }
    }

    /// This host's proxy boundary key, or empty when none is provisioned yet.
    ///
    /// Empty is the honest answer before the first deployment: the probe then
    /// gets a 401 and reports the proxy as not ready, which is exactly the
    /// state it is in — a proxy nobody can reach is not serving anyone.
    fn boundary_key(&self) -> String {
        crate::configuration::read_boundary_key(self.store.paths()).unwrap_or_default()
    }

    /// Read-only status may return diagnostics when Docker is unavailable.
    pub fn status(&self) -> Result<ProxyStatusView, String> {
        let state = self.store.load()?;
        let containers = match self.docker.list_proxy_containers() {
            Ok(containers) => containers,
            Err(error) => {
                return Ok(ProxyStatusView {
                    present: false,
                    running: false,
                    ready: false,
                    container_id: None,
                    image: None,
                    image_digest: state.install.as_ref().and_then(|i| i.image_digest.clone()),
                    host_port: state.install.as_ref().map(|i| i.host_port),
                    install_id: state.install.as_ref().map(|i| i.install_id.clone()),
                    config_hash: state.install.as_ref().map(|i| i.config_hash.clone()),
                    last_error: Some(format!("docker unavailable: {error}")),
                });
            }
        };
        status_from_inventory(
            &state.install,
            &containers,
            &self.docker,
            &self.boundary_key(),
        )
    }

    pub fn install(
        &self,
        operation_id: &str,
        image: &str,
        image_digest: Option<String>,
    ) -> Result<OperationResult, String> {
        let request = json!({
            "image": image,
            "imageDigest": image_digest,
        });
        self.with_mutation_lock(|| {
            self.run_operation_locked(operation_id, "proxy.install", &request, |record| {
                self.install_inner(record, image, image_digest.clone())
            })
        })
    }

    pub fn load_image_archive(
        &self,
        operation_id: &str,
        staged_path: &Path,
        expected_sha256: &str,
        image: &str,
    ) -> Result<OperationResult, String> {
        let request = json!({
            "stagedPath": staged_path,
            "expectedSha256": expected_sha256,
            "image": image,
        });
        self.with_mutation_lock(|| {
            self.run_operation_locked(operation_id, "proxy.loadImage", &request, |_record| {
                let file_name = staged_path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default();
                if staged_path.parent() != Some(Path::new("/tmp"))
                    || !file_name.starts_with("vellum-proxy-stage-")
                    || !file_name.ends_with(".tar")
                {
                    return Err("ProxyImageStagePathRejected".into());
                }
                crate::update::reject_symlink(staged_path)?;
                let actual = crate::update::sha256_file(staged_path)?;
                if !actual.eq_ignore_ascii_case(expected_sha256) {
                    return Err(format!(
                        "ProxyImageArchiveDigestMismatch: expected {expected_sha256}, got {actual}"
                    ));
                }
                let identity = self.docker.load_image_archive(staged_path, image)?;
                if identity.repo_digest.is_none() && identity.image_id.is_none() {
                    return Err("ProxyImageLoadUnverifiable: loaded image has no identity".into());
                }
                Ok(json!({
                    "image": image,
                    "archiveSha256": actual,
                    "repoDigest": identity.repo_digest,
                    "imageId": identity.image_id,
                }))
            })
        })
    }

    pub fn start(&self, request: ProxyStartRequest) -> Result<OperationResult, String> {
        let payload = json!({
            "hostPort": request.host_port,
            "image": request.image,
        });
        self.with_mutation_lock(|| {
            self.run_operation_locked(&request.operation_id, "proxy.start", &payload, |record| {
                self.start_inner(record, &request)
            })
        })
    }

    pub fn stop(&self, operation_id: &str) -> Result<OperationResult, String> {
        let payload = json!({});
        self.with_mutation_lock(|| {
            self.run_operation_locked(operation_id, "proxy.stop", &payload, |record| {
                self.stop_inner(record, false)
            })
        })
    }

    /// Coordinated restart preserves install/config identity and is allowed
    /// while managed profiles reference the proxy. Plain stop remains blocked
    /// in that state so an injected profile is never left pointing at nothing.
    pub fn restart(&self, operation_id: &str) -> Result<OperationResult, String> {
        let payload = json!({});
        self.with_mutation_lock(|| {
            self.run_operation_locked(operation_id, "proxy.restart", &payload, |record| {
                self.stop_inner(record, true)?;
                self.start_inner(
                    record,
                    &ProxyStartRequest {
                        operation_id: operation_id.into(),
                        host_port: None,
                        image: None,
                    },
                )
            })
        })
    }

    pub fn repair(&self, operation_id: &str) -> Result<OperationResult, String> {
        let payload = json!({});
        self.with_mutation_lock(|| {
            self.run_operation_locked(operation_id, "repair.run", &payload, |record| {
                let state = self.store.load()?;
                if state.install.is_none() {
                    return Err("RepairRequiresInstall: no managed proxy install exists".into());
                }
                let inventory = self.require_docker_observable()?;
                let status = status_from_inventory(
                    &state.install,
                    &inventory,
                    &self.docker,
                    &self.boundary_key(),
                )?;
                if status.ready {
                    return serde_json::to_value(status).map_err(|error| error.to_string());
                }
                self.stop_inner(record, true)?;
                self.start_inner(
                    record,
                    &ProxyStartRequest {
                        operation_id: operation_id.into(),
                        host_port: None,
                        image: None,
                    },
                )
            })
        })
    }

    pub fn update(
        &self,
        operation_id: &str,
        image: &str,
        image_digest: Option<String>,
    ) -> Result<OperationResult, String> {
        let payload = json!({
            "image": image,
            "imageDigest": image_digest,
        });
        self.with_mutation_lock(|| {
            self.run_operation_locked(operation_id, "proxy.update", &payload, |record| {
                self.update_inner(record, image, image_digest.clone())
            })
        })
    }

    fn with_mutation_lock<T>(&self, work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        self.store.paths().ensure()?;
        let _guard = HostMutationGuard::acquire(&self.store.paths().mutation_lock)?;
        work()
    }

    fn require_docker_observable(&self) -> Result<Vec<ContainerSummary>, String> {
        self.docker
            .list_proxy_containers()
            .map_err(|error| format!("docker inventory unavailable; refusing mutation: {error}"))
    }

    fn run_operation_locked<F>(
        &self,
        operation_id: &str,
        method: &str,
        request: &Value,
        work: F,
    ) -> Result<OperationResult, String>
    where
        F: FnOnce(&mut OperationRecord) -> Result<Value, String>,
    {
        self.store.paths().ensure()?;
        let journal = self.store.operations();
        match journal.begin_or_replay(operation_id, method, request)? {
            BeginOutcome::Replay { result } => Ok(OperationResult {
                operation_id: operation_id.into(),
                idempotent_replay: true,
                status: "ok".into(),
                detail: result,
            }),
            BeginOutcome::Fresh { mut record } | BeginOutcome::Resume { mut record } => {
                // Fail closed before marking Executing when Docker cannot be observed.
                let _ = self.require_docker_observable()?;
                journal.mark_executing(&mut record)?;
                match work(&mut record) {
                    Ok(detail) => {
                        journal.mark_completed(&mut record, detail.clone())?;
                        Ok(OperationResult {
                            operation_id: operation_id.into(),
                            idempotent_replay: false,
                            status: "ok".into(),
                            detail,
                        })
                    }
                    Err(error) => {
                        let _ = journal.mark_failed(&mut record, error.clone());
                        Err(error)
                    }
                }
            }
        }
    }

    fn install_inner(
        &self,
        _record: &mut OperationRecord,
        image: &str,
        image_digest: Option<String>,
    ) -> Result<Value, String> {
        // Mutations must never rewrite install ownership metadata while Docker
        // inventory is unobservable or a managed container is still present.
        let containers = self.require_docker_observable()?;
        let state = self.store.load()?;
        let view = status_from_inventory(
            &state.install,
            &containers,
            &self.docker,
            &self.boundary_key(),
        )?;
        if view.present {
            return Err(
                "UpdateRequiresStop: stop the managed proxy container before install/update".into(),
            );
        }

        // Pull (when needed), inspect, and pin an immutable digest before any
        // install metadata rewrite. Expected digests fail closed on mismatch.
        let resolved = resolve_image_with(self.docker.as_ref(), image, image_digest.as_deref())?;
        verify_configured_signature(&resolved.run_ref)?;
        let paths = self.store.paths();
        let _mounts = prepare_proxy_mounts(paths)?;

        let mut state = self.store.load()?;
        let host_id = state.host_id.clone();
        let install_id = state
            .install
            .as_ref()
            .map(|item| item.install_id.clone())
            .unwrap_or_else(|| format!("install-{}", ulid::Ulid::new()));
        let host_port = state
            .install
            .as_ref()
            .map(|item| item.host_port)
            .unwrap_or(15721);
        let config_hash = config_hash_for(&resolved.source, host_port, &install_id);
        write_default_proxy_config(paths, &host_id, &install_id, &config_hash, &resolved.source)?;
        let install = InstallRecord {
            install_id,
            host_id,
            image: resolved.source.clone(),
            image_digest: resolved.repo_digest.clone(),
            config_hash,
            host_port,
            updated_at: Utc::now(),
        };
        state.install = Some(install.clone());
        self.store.save(&state)?;
        serde_json::to_value(&install).map_err(|error| error.to_string())
    }

    fn start_inner(
        &self,
        _record: &mut OperationRecord,
        request: &ProxyStartRequest,
    ) -> Result<Value, String> {
        let containers = self.require_docker_observable()?;
        let state = self.store.load()?;
        let status = status_from_inventory(
            &state.install,
            &containers,
            &self.docker,
            &self.boundary_key(),
        )?;
        if status
            .last_error
            .as_deref()
            .is_some_and(|msg| msg.contains("stale managed container"))
        {
            return Err(
                "UpdateRequiresStop: stale managed container present; stop before start/update"
                    .into(),
            );
        }
        if status.running && status.ready {
            return serde_json::to_value(&status).map_err(|error| error.to_string());
        }

        let mut state = self.store.load()?;
        let image = request
            .image
            .clone()
            .or_else(|| state.install.as_ref().map(|i| i.image.clone()))
            .unwrap_or_else(|| "vellum-proxy:local".into());
        if state.install.is_none() {
            // Nested install reuses the already-held host mutation lock. Do not
            // call the public install() path, which would try to re-acquire it.
            let nested = self.run_operation_locked(
                &format!("{}-auto-install", request.operation_id),
                "proxy.install",
                &json!({
                    "image": image,
                    "imageDigest": Value::Null,
                }),
                |record| self.install_inner(record, &image, None),
            )?;
            let _ = nested;
            state = self.store.load()?;
        }
        let install = state
            .install
            .clone()
            .ok_or_else(|| "install record missing after install".to_string())?;
        let host_port = request.host_port.unwrap_or(install.host_port);

        let existing = self.require_docker_observable()?;
        if let Some(container) = existing
            .into_iter()
            .find(|container| labels_for(&install).matches(&container.labels))
        {
            if !container.running {
                self.docker.remove_container(&container.id)?;
            } else {
                let boundary_key = self.boundary_key();
                let ready = self.docker.inspect_readyz(host_port, &boundary_key)?;
                let websocket = self
                    .docker
                    .inspect_responses_websocket(host_port, &boundary_key)?;
                if !ready || !websocket {
                    return Err(
                        "proxy container running but Responses HTTP/WebSocket contract failed"
                            .into(),
                    );
                }
                let view = self.status()?;
                return serde_json::to_value(&view).map_err(|error| error.to_string());
            }
        }

        let paths = self.store.paths();
        let mounts = prepare_proxy_mounts(paths)?;
        if !paths.proxy_config_dir.join("proxy.toml").exists() {
            write_default_proxy_config(
                paths,
                &install.host_id,
                &install.install_id,
                &install.config_hash,
                &image,
            )?;
        }

        // Production runs prefer repo@sha256 when a digest is known; fall back
        // to the tag only when inspect could not produce one (local builds).
        let resolved = resolve_image_with(
            self.docker.as_ref(),
            &image,
            install.image_digest.as_deref(),
        )?;
        let summary = self.docker.run_proxy_container(&RunProxySpec {
            image: resolved.run_ref.clone(),
            name: format!("vellum-proxy-{}", install.install_id),
            host_port,
            labels: labels_for(&install),
            config_dir: paths.proxy_config_dir.to_string_lossy().to_string(),
            data_dir: paths.proxy_data_dir.to_string_lossy().to_string(),
            history_dir: paths.proxy_history_dir.to_string_lossy().to_string(),
            logs_dir: paths.proxy_logs_dir.to_string_lossy().to_string(),
            secrets_dir: paths.secrets_dir.to_string_lossy().to_string(),
            user: mounts.user.clone(),
        })?;

        if !wait_for_runtime_contract(self.docker.as_ref(), host_port, &self.boundary_key()) {
            let _ = self.docker.stop_container(&summary.id);
            let _ = self.docker.remove_container(&summary.id);
            return Err("proxy started but Responses HTTP/WebSocket contract did not pass".into());
        }

        let mut state = self.store.load()?;
        if let Some(install) = state.install.as_mut() {
            install.host_port = host_port;
            install.image = resolved.source.clone();
            if resolved.repo_digest.is_some() {
                install.image_digest = resolved.repo_digest.clone();
            }
            install.updated_at = Utc::now();
        }
        self.store.save(&state)?;
        let install = state
            .install
            .as_ref()
            .ok_or_else(|| "install record missing after start".to_string())?;
        let view = status_from_container(install, &summary, &self.docker, &self.boundary_key())?;
        if !view.ready {
            let _ = self.docker.stop_container(&summary.id);
            let _ = self.docker.remove_container(&summary.id);
            return Err(view
                .last_error
                .clone()
                .unwrap_or_else(|| "proxy identity/ready verification failed".into()));
        }
        serde_json::to_value(&view).map_err(|error| error.to_string())
    }

    fn stop_inner(
        &self,
        _record: &mut OperationRecord,
        coordinated_restart: bool,
    ) -> Result<Value, String> {
        let references = active_profile_references(self.store.paths())?;
        if !coordinated_restart && !references.is_empty() {
            return Err(format!(
                "ProxyInUse: managed profile leases still reference this proxy: {}",
                references.join(", ")
            ));
        }
        let containers = self.require_docker_observable()?;
        let state = self.store.load()?;
        let install = match state.install {
            Some(install) => install,
            None => {
                return Ok(json!({"present": false, "running": false, "ready": false}));
            }
        };

        let labels = labels_for(&install);
        for container in containers {
            if labels.matches(&container.labels)
                || (coordinated_restart && same_install_ownership(&install, &container))
            {
                if container.running {
                    self.docker.stop_container(&container.id)?;
                }
                self.docker.remove_container(&container.id)?;
            } else if container.name.contains("vellum-proxy")
                && !is_labeled_managed_proxy(&container)
            {
                return Err(format!(
                    "refusing to remove unlabeled/mismatched container {}",
                    container.id
                ));
            }
        }

        Ok(json!({"present": false, "running": false, "ready": false}))
    }

    fn update_inner(
        &self,
        record: &mut OperationRecord,
        image: &str,
        image_digest: Option<String>,
    ) -> Result<Value, String> {
        self.install_inner(record, image, image_digest)
    }
}

fn wait_for_runtime_contract(
    docker: &dyn DockerClient,
    host_port: u16,
    boundary_key: &str,
) -> bool {
    let deadline = std::time::Instant::now() + PROXY_READY_TIMEOUT;
    loop {
        if docker
            .inspect_readyz(host_port, boundary_key)
            .unwrap_or(false)
            && docker
                .inspect_responses_websocket(host_port, boundary_key)
                .unwrap_or(false)
        {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn verify_configured_signature(image: &str) -> Result<(), String> {
    let Ok(identity) = std::env::var("VELLUM_COSIGN_IDENTITY") else {
        return Ok(());
    };
    if identity.trim().is_empty() {
        return Err("ImageSignaturePolicyInvalid: VELLUM_COSIGN_IDENTITY is blank".into());
    }
    if !image.contains("@sha256:") {
        return Err(
            "ImageSignatureRequiresDigest: cosign policy requires an immutable image digest".into(),
        );
    }
    let output = std::process::Command::new("cosign")
        .args([
            "verify",
            "--certificate-identity",
            identity.trim(),
            "--certificate-oidc-issuer-regexp",
            ".+",
            image,
        ])
        .output()
        .map_err(|error| format!("ImageSignatureVerifierUnavailable: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "ImageSignatureVerificationFailed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn same_install_ownership(install: &InstallRecord, container: &ContainerSummary) -> bool {
    is_labeled_managed_proxy(container)
        && container.labels.get("io.vellum.host-id") == Some(&install.host_id)
        && container.labels.get("io.vellum.install-id") == Some(&install.install_id)
}

fn is_labeled_managed_proxy(container: &ContainerSummary) -> bool {
    container
        .labels
        .get("io.vellum.managed")
        .is_some_and(|value| value == "true")
        && container
            .labels
            .get("io.vellum.component")
            .is_some_and(|value| value == "proxy")
}

fn active_profile_references(paths: &crate::state::AgentPaths) -> Result<Vec<String>, String> {
    if !paths.leases_dir.exists() {
        return Ok(Vec::new());
    }
    let mut profiles = Vec::new();
    for entry in std::fs::read_dir(&paths.leases_dir)
        .map_err(|error| format!("failed reading lease directory: {error}"))?
    {
        let entry = entry.map_err(|error| format!("failed reading lease entry: {error}"))?;
        if entry.path().extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(entry.path())
            .map_err(|error| format!("failed reading lease {}: {error}", entry.path().display()))?;
        let lease: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("invalid lease {}: {error}", entry.path().display()))?;
        let state = lease
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if matches!(state, "active" | "restartRequired" | "recoveryRequired") {
            profiles.push(
                lease
                    .get("profileId")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
            );
        }
    }
    profiles.sort();
    profiles.dedup();
    Ok(profiles)
}

fn labels_for(install: &InstallRecord) -> ManagedContainerLabels {
    ManagedContainerLabels {
        host_id: install.host_id.clone(),
        install_id: install.install_id.clone(),
        config_hash: install.config_hash.clone(),
        image_version: install.image.clone(),
    }
}

fn status_from_inventory(
    install: &Option<InstallRecord>,
    containers: &[ContainerSummary],
    docker: &Arc<dyn DockerClient>,
    boundary_key: &str,
) -> Result<ProxyStatusView, String> {
    let Some(install) = install else {
        return Ok(ProxyStatusView {
            present: false,
            ..ProxyStatusView::default()
        });
    };

    if let Some(container) = containers
        .iter()
        .find(|container| labels_for(install).matches(&container.labels))
    {
        return status_from_container(install, container, docker, boundary_key);
    }

    let stale = containers.iter().find(|container| {
        container
            .labels
            .get("io.vellum.managed")
            .map(String::as_str)
            == Some("true")
            && container
                .labels
                .get("io.vellum.component")
                .map(String::as_str)
                == Some("proxy")
            && container.labels.get("io.vellum.host-id") == Some(&install.host_id)
            && container.labels.get("io.vellum.install-id") == Some(&install.install_id)
            && container.labels.get("io.vellum.config-hash") != Some(&install.config_hash)
    });
    if let Some(container) = stale {
        return Ok(ProxyStatusView {
            present: true,
            running: container.running,
            ready: false,
            container_id: Some(container.id.clone()),
            image: Some(container.image.clone()),
            image_digest: install.image_digest.clone(),
            host_port: container.host_port.or(Some(install.host_port)),
            install_id: Some(install.install_id.clone()),
            config_hash: Some(install.config_hash.clone()),
            last_error: Some(
                "stale managed container present with mismatched config hash; stop/update required"
                    .into(),
            ),
        });
    }

    Ok(ProxyStatusView {
        present: false,
        running: false,
        ready: false,
        container_id: None,
        image: None,
        image_digest: install.image_digest.clone(),
        host_port: Some(install.host_port),
        install_id: Some(install.install_id.clone()),
        config_hash: Some(install.config_hash.clone()),
        last_error: None,
    })
}

fn status_from_container(
    install: &InstallRecord,
    container: &ContainerSummary,
    docker: &Arc<dyn DockerClient>,
    boundary_key: &str,
) -> Result<ProxyStatusView, String> {
    let host_port = container.host_port.or(Some(install.host_port));
    let probe = if container.running {
        host_port.and_then(|port| docker.inspect_readyz_identity(port, boundary_key).ok())
    } else {
        None
    };
    let identity_matches = probe.as_ref().is_some_and(|probe| {
        probe
            .install_id
            .as_deref()
            .is_none_or(|id| id == install.install_id)
            && probe
                .config_hash
                .as_deref()
                .is_none_or(|hash| hash == install.config_hash)
    });
    let websocket_ready = if container.running {
        host_port.is_some_and(|port| {
            docker
                .inspect_responses_websocket(port, boundary_key)
                .unwrap_or(false)
        })
    } else {
        false
    };
    let ready =
        probe.as_ref().is_some_and(|probe| probe.ready) && identity_matches && websocket_ready;
    let last_error = probe.as_ref().and_then(|probe| {
        if probe.ready && !identity_matches {
            Some("readyz identity mismatch for managed install/config".into())
        } else if probe.ready && !websocket_ready {
            Some("Responses WebSocket upgrade unavailable".into())
        } else {
            None
        }
    });
    Ok(ProxyStatusView {
        present: true,
        running: container.running,
        ready,
        container_id: Some(container.id.clone()),
        image: Some(container.image.clone()),
        image_digest: install.image_digest.clone(),
        host_port,
        install_id: Some(install.install_id.clone()),
        config_hash: Some(install.config_hash.clone()),
        last_error,
    })
}

fn config_hash_for(image: &str, host_port: u16, install_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(image.as_bytes());
    hasher.update(host_port.to_string().as_bytes());
    hasher.update(install_id.as_bytes());
    hex::encode(hasher.finalize())
}

fn write_default_proxy_config(
    paths: &crate::state::AgentPaths,
    host_id: &str,
    install_id: &str,
    config_hash: &str,
    image: &str,
) -> Result<(), String> {
    paths.ensure()?;
    let config = format!(
        "schema_version = 2\n\
listen = \"0.0.0.0:15721\"\n\
data_dir = \"/var/lib/vellum/data\"\n\
history_dir = \"/var/lib/vellum/history\"\n\
log_dir = \"/var/log/vellum\"\n\
credentials_dir = \"/run/secrets\"\n\
require_secrets = false\n\
strict_upstream = false\n\
\n\
[inbound_access]\n\
credentialId = \"__vellum_proxy_boundary__\"\n\
\n\
[identity]\n\
install_id = \"{install_id}\"\n\
host_id = \"{host_id}\"\n\
image_version = \"{image}\"\n\
config_hash = \"{config_hash}\"\n\
\n\
[execution_environment]\n\
platform = \"linux\"\n\
shell = \"bash\"\n\
supportsAndAnd = true\n\
hasUnixUtilities = true\n\
pathStyle = \"posix\"\n\
ampersandSemantics = \"posix-background\"\n\
\n\
[[models]]\n\
route_id = \"mock\"\n\
catalog_id = \"vellum-mock\"\n\
name = \"Vellum Mock\"\n\
base_url = \"http://127.0.0.1:1/v1\"\n\
provider_kind = \"openAiCompatible\"\n\
auth_kind = \"none\"\n\
wire = \"responses\"\n\
server_side_resume = false\n\
streaming = true\n\
reasoning = false\n\
vision = false\n\
upstream_model = \"mock-1\"\n\
context_window = 128000\n"
    );
    let path = paths.proxy_config_dir.join("proxy.toml");
    std::fs::write(&path, config)
        .map_err(|error| format!("failed writing {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&path, perms)
            .map_err(|error| format!("failed chmod 0600 on {}: {error}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker::FakeDockerClient;
    use crate::operations::OperationState;
    use crate::permissions::host_runtime_identity;
    use crate::state::AgentPaths;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;
    use std::time::Duration;

    fn manager(temp: &tempfile::TempDir) -> (ProxyManager, Arc<FakeDockerClient>) {
        let store = AgentStateStore::new(AgentPaths::from_root(temp.path().to_path_buf()));
        let docker = Arc::new(FakeDockerClient::default());
        *docker.readyz.lock().unwrap() = true;
        *docker.responses_websocket.lock().unwrap() = true;
        (ProxyManager::with_docker(store, docker.clone()), docker)
    }

    #[test]
    fn start_stop_are_idempotent_and_label_safe() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _) = manager(&temp);
        manager
            .install("op-install", "vellum-proxy:test", None)
            .unwrap();
        assert!(
            manager
                .install("op-install", "vellum-proxy:test", None)
                .unwrap()
                .idempotent_replay
        );
        manager
            .start(ProxyStartRequest {
                operation_id: "op-start".into(),
                host_port: Some(15721),
                image: None,
            })
            .unwrap();
        assert!(
            manager
                .start(ProxyStartRequest {
                    operation_id: "op-start".into(),
                    host_port: Some(15721),
                    image: None,
                })
                .unwrap()
                .idempotent_replay
        );
        manager.stop("op-stop").unwrap();
        assert!(manager.stop("op-stop").unwrap().idempotent_replay);
        assert!(!manager.status().unwrap().running);
    }

    #[test]
    fn operation_journal_survives_intervening_ops() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _) = manager(&temp);
        manager.install("op-a", "vellum-proxy:a", None).unwrap();
        manager
            .start(ProxyStartRequest {
                operation_id: "op-b".into(),
                host_port: Some(15721),
                image: None,
            })
            .unwrap();
        assert!(
            manager
                .install("op-a", "vellum-proxy:a", None)
                .unwrap()
                .idempotent_replay
        );
    }

    #[test]
    fn interrupted_install_resumes_and_reconciles_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _) = manager(&temp);
        let request = json!({"image": "vellum-proxy:a", "imageDigest": null});
        let journal = manager.store.operations();
        let mut record = match journal
            .begin_or_replay("op-killed", "proxy.install", &request)
            .unwrap()
        {
            BeginOutcome::Fresh { record } => record,
            _ => panic!("expected fresh operation"),
        };
        journal.mark_executing(&mut record).unwrap();

        let result = manager
            .install("op-killed", "vellum-proxy:a", None)
            .unwrap();
        assert!(!result.idempotent_replay);
        assert!(manager.store.load().unwrap().install.is_some());
        assert_eq!(
            journal.load("op-killed").unwrap().unwrap().state,
            OperationState::Completed
        );
    }

    #[test]
    fn start_rejects_ready_endpoint_with_wrong_identity_and_rolls_back_container() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, docker) = manager(&temp);
        manager
            .install("op-install", "vellum-proxy:test", None)
            .unwrap();
        *docker.readyz_install_id.lock().unwrap() = Some("another-install".into());
        let error = manager
            .start(ProxyStartRequest {
                operation_id: "op-start".into(),
                host_port: Some(15721),
                image: None,
            })
            .unwrap_err();
        assert!(error.contains("identity mismatch"));
        assert!(docker.containers.lock().unwrap().is_empty());
    }

    #[test]
    fn start_rejects_http_ready_proxy_without_responses_websocket() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, docker) = manager(&temp);
        manager
            .install("op-install", "vellum-proxy:test", None)
            .unwrap();
        *docker.responses_websocket.lock().unwrap() = false;
        let error = manager
            .start(ProxyStartRequest {
                operation_id: "op-start".into(),
                host_port: Some(15721),
                image: None,
            })
            .unwrap_err();
        assert!(error.contains("HTTP/WebSocket contract"));
        assert!(docker.containers.lock().unwrap().is_empty());
    }

    #[test]
    fn same_operation_id_different_request_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _) = manager(&temp);
        manager.install("op-same", "vellum-proxy:a", None).unwrap();
        let err = manager
            .install("op-same", "vellum-proxy:b", None)
            .unwrap_err();
        assert!(err.contains("OperationConflict"));
    }

    #[test]
    fn update_refuses_while_running_and_does_not_orphan() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, docker) = manager(&temp);
        manager
            .install("op-install", "vellum-proxy:a", None)
            .unwrap();
        manager
            .start(ProxyStartRequest {
                operation_id: "op-start".into(),
                host_port: Some(15721),
                image: None,
            })
            .unwrap();
        let before_len = docker.containers.lock().unwrap().len();
        let before_image = manager.store.load().unwrap().install.unwrap().image;
        let err = manager
            .update("op-update", "vellum-proxy:b", None)
            .unwrap_err();
        assert!(err.contains("UpdateRequiresStop"));
        assert_eq!(docker.containers.lock().unwrap().len(), before_len);
        assert_eq!(
            manager.store.load().unwrap().install.unwrap().image,
            before_image
        );
    }

    #[test]
    fn stop_refuses_while_a_managed_profile_references_the_proxy() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, docker) = manager(&temp);
        manager
            .install("op-install", "vellum-proxy:a", None)
            .unwrap();
        manager
            .start(ProxyStartRequest {
                operation_id: "op-start".into(),
                host_port: Some(15721),
                image: None,
            })
            .unwrap();
        let leases = &manager.store.paths().leases_dir;
        std::fs::create_dir_all(leases).unwrap();
        std::fs::write(
            leases.join("profile-a.json"),
            serde_json::to_vec(&json!({
                "profileId": "profile-a",
                "state": "active"
            }))
            .unwrap(),
        )
        .unwrap();

        let error = manager.stop("op-stop").unwrap_err();
        assert!(error.contains("ProxyInUse"), "{error}");
        assert_eq!(docker.containers.lock().unwrap().len(), 1);

        let before = manager.store.load().unwrap().install.unwrap();
        let restarted = manager.restart("op-restart").unwrap();
        assert_eq!(restarted.detail["installId"], before.install_id);
        assert_eq!(restarted.detail["configHash"], before.config_hash);
        assert_eq!(docker.containers.lock().unwrap().len(), 1);
    }

    #[test]
    fn stop_ignores_a_different_labeled_managed_proxy_install() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, docker) = manager(&temp);
        manager
            .install("op-install", "vellum-proxy:a", None)
            .unwrap();
        manager
            .start(ProxyStartRequest {
                operation_id: "op-start".into(),
                host_port: Some(45721),
                image: None,
            })
            .unwrap();
        docker.containers.lock().unwrap().insert(
            0,
            ContainerSummary {
                id: "foreign-managed".into(),
                name: "vellum-proxy-install-foreign".into(),
                image: "vellum-proxy:production".into(),
                running: true,
                labels: BTreeMap::from([
                    ("io.vellum.managed".into(), "true".into()),
                    ("io.vellum.component".into(), "proxy".into()),
                    ("io.vellum.host-id".into(), "foreign-host".into()),
                    ("io.vellum.install-id".into(), "foreign-install".into()),
                    ("io.vellum.config-hash".into(), "foreign-config".into()),
                ]),
                host_port: Some(15721),
            },
        );

        manager.stop("op-stop").unwrap();
        let containers = docker.containers.lock().unwrap();
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].id, "foreign-managed");
        assert!(containers[0].running);
    }

    #[test]
    fn stop_still_refuses_an_unlabeled_proxy_impostor() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, docker) = manager(&temp);
        manager
            .install("op-install", "vellum-proxy:a", None)
            .unwrap();
        docker.containers.lock().unwrap().push(ContainerSummary {
            id: "unlabeled".into(),
            name: "vellum-proxy-impostor".into(),
            image: "unknown".into(),
            running: true,
            labels: BTreeMap::new(),
            host_port: Some(45721),
        });

        let error = manager.stop("op-stop").unwrap_err();
        assert!(error.contains("unlabeled/mismatched"), "{error}");
        assert_eq!(docker.containers.lock().unwrap().len(), 1);
    }

    #[test]
    fn repair_replaces_only_a_stale_container_owned_by_the_same_install() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, docker) = manager(&temp);
        manager
            .install("op-install", "vellum-proxy:a", None)
            .unwrap();
        let install = manager.store.load().unwrap().install.unwrap();
        docker.containers.lock().unwrap().push(ContainerSummary {
            id: "stale-owned".into(),
            name: "vellum-proxy-stale".into(),
            image: install.image.clone(),
            running: true,
            labels: BTreeMap::from([
                ("io.vellum.managed".into(), "true".into()),
                ("io.vellum.component".into(), "proxy".into()),
                ("io.vellum.host-id".into(), install.host_id.clone()),
                ("io.vellum.install-id".into(), install.install_id.clone()),
                ("io.vellum.config-hash".into(), "old-config".into()),
            ]),
            host_port: Some(install.host_port),
        });

        let before = manager.status().unwrap();
        assert!(!before.ready);
        assert!(before
            .last_error
            .unwrap()
            .contains("stale managed container"));
        manager.repair("op-repair").unwrap();
        let containers = docker.containers.lock().unwrap();
        assert_eq!(containers.len(), 1);
        assert_ne!(containers[0].id, "stale-owned");
        assert_eq!(
            containers[0].labels.get("io.vellum.config-hash"),
            Some(&install.config_hash)
        );
    }

    #[test]
    fn docker_error_is_not_reported_as_absent_without_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        {
            let (seed, _) = manager(&temp);
            seed.install("op-install", "vellum-proxy:a", None).unwrap();
        }
        let store = AgentStateStore::new(AgentPaths::from_root(temp.path().to_path_buf()));
        let manager = ProxyManager::with_docker(store, Arc::new(FailingListDocker));
        let status = manager.status().unwrap();
        assert!(status
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("docker unavailable"));
    }

    #[test]
    fn install_does_not_mutate_state_when_docker_inventory_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        {
            let (seed, _) = manager(&temp);
            seed.install("op-seed", "vellum-proxy:a", None).unwrap();
        }
        let before = std::fs::read_to_string(temp.path().join("agent/state.json")).unwrap();
        let store = AgentStateStore::new(AgentPaths::from_root(temp.path().to_path_buf()));
        let manager = ProxyManager::with_docker(store, Arc::new(FailingListDocker));
        let err = manager
            .install("op-install-outage", "vellum-proxy:b", None)
            .unwrap_err();
        assert!(err.contains("docker inventory unavailable"));
        let after = std::fs::read_to_string(temp.path().join("agent/state.json")).unwrap();
        assert_eq!(before, after);
        assert_eq!(
            manager.store.load().unwrap().install.unwrap().image,
            "vellum-proxy:a"
        );
    }

    #[test]
    fn update_does_not_mutate_state_when_docker_inventory_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        {
            let (seed, _) = manager(&temp);
            seed.install("op-seed", "vellum-proxy:a", None).unwrap();
            seed.start(ProxyStartRequest {
                operation_id: "op-start".into(),
                host_port: Some(15721),
                image: None,
            })
            .unwrap();
        }
        let before = std::fs::read_to_string(temp.path().join("agent/state.json")).unwrap();
        let store = AgentStateStore::new(AgentPaths::from_root(temp.path().to_path_buf()));
        let manager = ProxyManager::with_docker(store, Arc::new(FailingListDocker));
        let err = manager
            .update("op-update-outage", "vellum-proxy:b", None)
            .unwrap_err();
        assert!(err.contains("docker inventory unavailable"));
        let after = std::fs::read_to_string(temp.path().join("agent/state.json")).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn concurrent_same_operation_executes_mutation_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let docker = Arc::new(CountingDocker::default());
        *docker.inner.readyz.lock().unwrap() = true;
        *docker.inner.responses_websocket.lock().unwrap() = true;
        {
            let store = AgentStateStore::new(AgentPaths::from_root(root.clone()));
            let manager = ProxyManager::with_docker(store, docker.clone());
            manager
                .install("op-install", "vellum-proxy:a", None)
                .unwrap();
        }
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let root = root.clone();
            let docker = docker.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                let store = AgentStateStore::new(AgentPaths::from_root(root));
                let manager = ProxyManager::with_docker(store, docker);
                barrier.wait();
                manager.start(ProxyStartRequest {
                    operation_id: "op-start-same".into(),
                    host_port: Some(15721),
                    image: None,
                })
            }));
        }
        let mut replays = 0;
        for handle in handles {
            let result = handle.join().unwrap().unwrap();
            if result.idempotent_replay {
                replays += 1;
            }
        }
        assert_eq!(replays, 1);
        assert_eq!(docker.run_count(), 1);
    }

    #[test]
    fn concurrent_start_and_stop_are_serialized() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let docker = Arc::new(CountingDocker::default());
        *docker.inner.readyz.lock().unwrap() = true;
        *docker.inner.responses_websocket.lock().unwrap() = true;
        {
            let store = AgentStateStore::new(AgentPaths::from_root(root.clone()));
            let manager = ProxyManager::with_docker(store, docker.clone());
            manager
                .install("op-install", "vellum-proxy:a", None)
                .unwrap();
            manager
                .start(ProxyStartRequest {
                    operation_id: "op-start-seed".into(),
                    host_port: Some(15721),
                    image: None,
                })
                .unwrap();
        }
        // Clear mutation spans from seed setup; we only care about the concurrent phase.
        docker.spans.lock().unwrap().clear();
        let barrier = Arc::new(Barrier::new(2));
        let start_root = root.clone();
        let stop_root = root.clone();
        let start_docker = docker.clone();
        let stop_docker = docker.clone();
        let start_barrier = barrier.clone();
        let stop_barrier = barrier;

        let start_handle = thread::spawn(move || {
            let store = AgentStateStore::new(AgentPaths::from_root(start_root));
            let manager = ProxyManager::with_docker(store, start_docker);
            start_barrier.wait();
            manager.start(ProxyStartRequest {
                operation_id: "op-start-concurrent".into(),
                host_port: Some(15721),
                image: None,
            })
        });
        let stop_handle = thread::spawn(move || {
            let store = AgentStateStore::new(AgentPaths::from_root(stop_root));
            let manager = ProxyManager::with_docker(store, stop_docker);
            stop_barrier.wait();
            thread::sleep(Duration::from_millis(5));
            manager.stop("op-stop-concurrent")
        });
        assert!(start_handle.join().unwrap().is_ok());
        assert!(stop_handle.join().unwrap().is_ok());

        // Docker mutations are the critical section. Exclusive host lock means
        // run/stop/remove spans must not overlap across concurrent RPC processes.
        let spans = docker.spans.lock().unwrap().clone();
        assert!(
            !spans.is_empty(),
            "expected docker mutation spans from concurrent ops"
        );
        for (i, a) in spans.iter().enumerate() {
            for b in spans.iter().skip(i + 1) {
                let overlap = a.0 < b.1 && b.0 < a.1;
                assert!(!overlap, "overlapping docker spans: {spans:?}");
            }
        }
    }

    #[test]
    fn install_persists_repo_digest_and_start_uses_pinned_ref() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, docker) = manager(&temp);
        docker.repo_digests.lock().unwrap().insert(
            "registry.example/vellum-proxy:1.0".into(),
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        );
        docker.image_ids.lock().unwrap().insert(
            "registry.example/vellum-proxy:1.0".into(),
            "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into(),
        );
        manager
            .install(
                "op-install-digest",
                "registry.example/vellum-proxy:1.0",
                Some(
                    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                        .into(),
                ),
            )
            .unwrap();
        let install = manager.store.load().unwrap().install.unwrap();
        assert_eq!(
            install.image_digest.as_deref(),
            Some("sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert!(docker
            .pulled
            .lock()
            .unwrap()
            .iter()
            .any(|image| image == "registry.example/vellum-proxy:1.0"));

        manager
            .start(ProxyStartRequest {
                operation_id: "op-start-digest".into(),
                host_port: Some(15721),
                image: None,
            })
            .unwrap();
        assert_eq!(
            docker.last_run_image.lock().unwrap().as_deref(),
            Some(
                "registry.example/vellum-proxy@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            )
        );
        let (uid, gid) = host_runtime_identity().unwrap();
        let expected_user = format!("{uid}:{gid}");
        assert_eq!(
            docker.last_run_user.lock().unwrap().as_deref(),
            Some(expected_user.as_str())
        );
    }

    #[test]
    fn local_image_start_uses_tag_not_fake_repo_digest() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, docker) = manager(&temp);
        docker.image_ids.lock().unwrap().insert(
            "vellum-proxy:local".into(),
            "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into(),
        );
        manager
            .install("op-install-local", "vellum-proxy:local", None)
            .unwrap();
        let install = manager.store.load().unwrap().install.unwrap();
        assert!(install.image_digest.is_none());
        manager
            .start(ProxyStartRequest {
                operation_id: "op-start-local".into(),
                host_port: Some(15721),
                image: None,
            })
            .unwrap();
        assert_eq!(
            docker.last_run_image.lock().unwrap().as_deref(),
            Some("vellum-proxy:local")
        );
    }

    #[test]
    fn install_rejects_digest_mismatch_without_state_rewrite() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, docker) = manager(&temp);
        docker.repo_digests.lock().unwrap().insert(
            "registry.example/vellum-proxy:1.0".into(),
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        );
        let before = manager.store.load().unwrap();
        let err = manager
            .install(
                "op-digest-mismatch",
                "registry.example/vellum-proxy:1.0",
                Some(
                    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                        .into(),
                ),
            )
            .unwrap_err();
        assert!(err.contains("ImageDigestMismatch"));
        assert_eq!(manager.store.load().unwrap(), before);
    }

    #[test]
    fn prepare_mounts_run_as_host_effective_user() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, docker) = manager(&temp);
        manager
            .install("op-install", "vellum-proxy:local", None)
            .unwrap();
        manager
            .start(ProxyStartRequest {
                operation_id: "op-start".into(),
                host_port: Some(15721),
                image: None,
            })
            .unwrap();
        let (uid, gid) = host_runtime_identity().unwrap();
        let expected_user = format!("{uid}:{gid}");
        assert_eq!(
            docker.last_run_user.lock().unwrap().as_deref(),
            Some(expected_user.as_str())
        );
    }

    #[derive(Debug, Default)]
    struct FailingListDocker;

    impl DockerClient for FailingListDocker {
        fn list_proxy_containers(&self) -> Result<Vec<ContainerSummary>, String> {
            Err("permission denied".into())
        }
        fn run_proxy_container(&self, _spec: &RunProxySpec) -> Result<ContainerSummary, String> {
            Err("unsupported".into())
        }
        fn stop_container(&self, _id: &str) -> Result<(), String> {
            Err("unsupported".into())
        }
        fn remove_container(&self, _id: &str) -> Result<(), String> {
            Err("unsupported".into())
        }
        fn inspect_readyz(&self, _host_port: u16, _boundary_key: &str) -> Result<bool, String> {
            Err("unsupported".into())
        }
        fn inspect_responses_websocket(
            &self,
            _host_port: u16,
            _boundary_key: &str,
        ) -> Result<bool, String> {
            Err("unsupported".into())
        }
        fn pull_image(&self, _image: &str) -> Result<(), String> {
            Err("unsupported".into())
        }
        fn inspect_image_identity(
            &self,
            _image: &str,
        ) -> Result<crate::docker::ImageIdentity, String> {
            Err("unsupported".into())
        }
    }

    #[derive(Debug, Default)]
    struct CountingDocker {
        inner: FakeDockerClient,
        runs: Mutex<u64>,
        spans: Mutex<Vec<(std::time::Instant, std::time::Instant)>>,
    }

    impl CountingDocker {
        fn run_count(&self) -> u64 {
            *self.runs.lock().unwrap()
        }

        fn timed<T>(&self, work: impl FnOnce() -> T) -> T {
            let start = std::time::Instant::now();
            thread::sleep(Duration::from_millis(30));
            let value = work();
            let end = std::time::Instant::now();
            self.spans.lock().unwrap().push((start, end));
            value
        }
    }

    impl DockerClient for CountingDocker {
        fn list_proxy_containers(&self) -> Result<Vec<ContainerSummary>, String> {
            self.inner.list_proxy_containers()
        }
        fn run_proxy_container(&self, spec: &RunProxySpec) -> Result<ContainerSummary, String> {
            *self.runs.lock().unwrap() += 1;
            self.timed(|| self.inner.run_proxy_container(spec))
        }
        fn stop_container(&self, id: &str) -> Result<(), String> {
            self.timed(|| self.inner.stop_container(id))
        }
        fn remove_container(&self, id: &str) -> Result<(), String> {
            self.timed(|| self.inner.remove_container(id))
        }
        fn inspect_readyz(&self, host_port: u16, boundary_key: &str) -> Result<bool, String> {
            self.inner.inspect_readyz(host_port, boundary_key)
        }
        fn inspect_responses_websocket(
            &self,
            host_port: u16,
            boundary_key: &str,
        ) -> Result<bool, String> {
            self.inner
                .inspect_responses_websocket(host_port, boundary_key)
        }
        fn pull_image(&self, image: &str) -> Result<(), String> {
            self.inner.pull_image(image)
        }
        fn inspect_image_identity(
            &self,
            image: &str,
        ) -> Result<crate::docker::ImageIdentity, String> {
            self.inner.inspect_image_identity(image)
        }
    }
}
