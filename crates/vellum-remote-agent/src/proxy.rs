use std::path::{Path, PathBuf};
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
use crate::protocol::{OperationResult, ProxyLogsView, ProxyStatusView};
use crate::state::{AgentStateStore, InstallRecord};

#[cfg(not(test))]
pub(crate) const PROXY_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(test)]
pub(crate) const PROXY_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct ProxyStartRequest {
    pub operation_id: String,
    pub host_port: Option<u16>,
    pub image: Option<String>,
}

pub struct ProxyManager {
    store: AgentStateStore,
    docker: Arc<dyn DockerClient>,
    launchd: Arc<dyn crate::native_proxy::LaunchdClient>,
    launch_agents_dir: PathBuf,
    launchd_domain: String,
    prefer_native: bool,
    prefer_docker: bool,
}

impl ProxyManager {
    pub fn new(store: AgentStateStore) -> Self {
        let linux = cfg!(target_os = "linux");
        let launch_agents_dir = if linux {
            dirs::home_dir()
                .map(|home| crate::native_proxy::systemd_user_dir(&home))
                .unwrap_or_else(|| store.paths().root.join("systemd/user"))
        } else {
            crate::native_proxy::default_launch_agents_dir(store.paths())
        };
        let launchd_domain = if linux {
            "user".into()
        } else {
            crate::native_proxy::ProcessLaunchdClient::domain().unwrap_or_else(|_| "gui/0".into())
        };
        let service: Arc<dyn crate::native_proxy::ProxyServiceClient> = if linux {
            if crate::persistence::probe().systemd_user {
                Arc::new(crate::native_proxy::SystemdUserClient::default())
            } else {
                Arc::new(crate::on_demand::OnDemandClient::new(store.paths().clone()))
            }
        } else {
            Arc::new(crate::native_proxy::ProcessLaunchdClient)
        };
        Self {
            store,
            docker: Arc::new(ProcessDockerClient::new()),
            launchd: service,
            launch_agents_dir,
            launchd_domain,
            prefer_native: cfg!(any(target_os = "linux", target_os = "macos")),
            prefer_docker: false,
        }
    }

    pub fn with_docker(store: AgentStateStore, docker: Arc<dyn DockerClient>) -> Self {
        let launch_agents_dir = crate::native_proxy::default_launch_agents_dir(store.paths());
        Self {
            store,
            docker,
            launchd: Arc::new(crate::native_proxy::ProcessLaunchdClient),
            launch_agents_dir,
            launchd_domain: "gui/0".into(),
            prefer_native: false,
            prefer_docker: true,
        }
    }

    pub fn with_native(
        store: AgentStateStore,
        launchd: Arc<dyn crate::native_proxy::LaunchdClient>,
        launch_agents_dir: PathBuf,
        launchd_domain: impl Into<String>,
    ) -> Self {
        Self {
            store,
            docker: Arc::new(ProcessDockerClient::new()),
            launchd,
            launch_agents_dir,
            launchd_domain: launchd_domain.into(),
            prefer_native: true,
            prefer_docker: false,
        }
    }

    /// This host's proxy boundary key, or empty when none is provisioned yet.
    ///
    /// Empty is the honest answer before the first deployment: the probe then
    /// gets a 401 and reports the proxy as not ready, which is exactly the
    /// state it is in — a proxy nobody can reach is not serving anyone.
    fn boundary_key(&self) -> String {
        crate::configuration::read_boundary_key(self.store.paths()).unwrap_or_default()
    }

    /// Deployment planning uses the same read-only selection as native start.
    pub fn select_port(&self, preferred: Option<u16>) -> Result<u16, String> {
        if !self.native_backend_active() {
            return Err("NativeProxyPortSelectionUnsupported".into());
        }
        let state = self.store.load()?;
        let installed = state.install.as_ref().map(|install| install.host_port);
        let job = self.launchd.inspect_agent(
            &self.launchd_domain,
            crate::native_proxy::LAUNCH_AGENT_LABEL,
        )?;
        let mut managed_port = if job.running { installed } else { None };
        if let Some(old) = state
            .install
            .as_ref()
            .filter(|install| install.backend() == crate::platform::ProxyBackend::Docker)
        {
            if self
                .require_docker_observable()?
                .iter()
                .any(|container| container.running && same_install_ownership(old, container))
            {
                // Migration will stop this exact labelled container before
                // native start. Its existing listener is not a foreign owner.
                managed_port = Some(old.host_port);
            }
        }
        crate::native_proxy::select_native_port(
            self.launchd.as_ref(),
            preferred
                .or(installed)
                .unwrap_or_else(|| self.launchd.default_port()),
            managed_port,
        )
    }

    /// Discard the private rollback snapshot only after native runtime verification.
    pub fn migration_commit(&self, operation_id: &str) -> Result<OperationResult, String> {
        self.with_mutation_lock(|| {
            self.run_operation_locked(operation_id, "proxy.migrationCommit", &json!({}), |_| {
                let Some(snapshot) = crate::migration::load(self.store.paths())? else {
                    // Every converge reaches this commit, including apply.noop.
                    // Older agents already discarded the snapshot but left Docker
                    // containers behind. Cleanup is best effort and repeatable.
                    let cleanup = self.cleanup_legacy_orphans();
                    if cleanup.is_err() {
                        eprintln!("ProxyMigrationContainerCleanupFailed");
                    }
                    return Ok(json!({"migrationPending": false,
                        "orphanContainersRemoved": cleanup.as_ref().ok(),
                        "cleanupWarning": cleanup.err().map(|_| "ProxyMigrationContainerCleanupFailed")}));
                };
                let state = self.store.load()?;
                let install = state
                    .install
                    .as_ref()
                    .ok_or("ProxyMigrationInstallMissing")?;
                if snapshot.rolled_back
                    || state.host_id != snapshot.install.host_id
                    || install.backend() != crate::platform::ProxyBackend::Native
                    || install.host_id != snapshot.install.host_id
                    || install.install_id != snapshot.install.install_id
                {
                    return Err("ProxyMigrationCommitOwnershipMismatch".into());
                }
                let status = crate::native_proxy::observe_native(
                    &self.store,
                    self.launchd.as_ref(),
                    &self.launch_agents_dir,
                    &self.launchd_domain,
                    &self.boundary_key(),
                )?;
                if !status.ready {
                    return Err("ProxyMigrationNativeNotReady".into());
                }
                crate::migration::clear(self.store.paths())?;
                // The snapshot is no longer a rollback target. Re-observe labels
                // before deleting; names alone never grant removal authority.
                // Post-commit cleanup is optional: an unavailable Docker
                // inventory is not a deletion failure or a native dependency.
                let cleanup: Result<(), String> = (|| {
                    let containers = self.docker.list_proxy_containers().unwrap_or_default();
                    for container in containers.iter().filter(|container| {
                        snapshot.stopped_ids.contains(&container.id)
                            && same_install_ownership(&snapshot.install, container)
                            && !container.running
                    }) {
                        self.docker.remove_container(&container.id)?;
                    }
                    Ok(())
                })();
                if cleanup.is_err() {
                    eprintln!("ProxyMigrationContainerCleanupFailed");
                }
                Ok(json!({"migrationCommitted": true, "cleanupWarning":
                    cleanup.err().map(|_| "ProxyMigrationContainerCleanupFailed")}))
            })
        })
    }

    /// Caller holds the host mutation lock. A pending snapshot, even a rolled
    /// back one, still owns its containers; never infer authority from names.
    fn cleanup_legacy_orphans(&self) -> Result<usize, String> {
        if crate::migration::load(self.store.paths())?.is_some() {
            return Ok(0);
        }
        let state = self.store.load()?;
        let Some(install) = state.install.as_ref().filter(|install| {
            install.backend() == crate::platform::ProxyBackend::Native
                && install.host_id == state.host_id
        }) else {
            return Ok(0);
        };
        let status = crate::native_proxy::observe_native(
            &self.store,
            self.launchd.as_ref(),
            &self.launch_agents_dir,
            &self.launchd_domain,
            &self.boundary_key(),
        )?;
        if !status.ready {
            return Ok(0);
        }
        // This is optional garbage collection, not migration/ownership
        // validation. Without an inventory, skip it silently. Keep strict
        // require_docker_observable checks on all deployment/rollback paths.
        let Ok(containers) = self.docker.list_proxy_containers() else {
            return Ok(0);
        };
        let mut removed = 0;
        for container in containers
            .iter()
            .filter(|container| !container.running && same_install_ownership(install, container))
        {
            self.docker.remove_container(&container.id)?;
            removed += 1;
        }
        Ok(removed)
    }

    pub fn migration_rollback(&self, operation_id: &str) -> Result<OperationResult, String> {
        self.with_mutation_lock(|| {
            self.run_operation_locked(operation_id, "proxy.migrationRollback", &json!({}), |_| {
                Ok(json!({"migrationRestored": self.rollback_migration_inner()?}))
            })
        })
    }

    fn rollback_migration_inner(&self) -> Result<bool, String> {
        let Some(mut snapshot) = crate::migration::load(self.store.paths())? else {
            return Ok(false);
        };
        if snapshot.rolled_back {
            return Ok(false);
        }
        let mut state = self.store.load()?;
        if state.host_id != snapshot.install.host_id
            || snapshot.install.backend() != crate::platform::ProxyBackend::Docker
            || state.install.as_ref().is_none_or(|current| {
                current.install_id != snapshot.install.install_id
                    || current.host_id != snapshot.install.host_id
            })
        {
            return Err("ProxyMigrationRecoveryOwnershipMismatch".into());
        }
        // Validate every ID and its current ownership before changing any
        // service or file. Never infer authority from a container name.
        let containers = self.require_docker_observable()?;
        for id in &snapshot.stopped_ids {
            if !containers.iter().any(|container| {
                container.id == *id && same_install_ownership(&snapshot.install, container)
            }) {
                return Err(format!("ProxyMigrationRecoveryContainerChanged: {id}"));
            }
        }
        let job = self.launchd.inspect_agent(
            &self.launchd_domain,
            crate::native_proxy::LAUNCH_AGENT_LABEL,
        )?;
        if job.loaded || job.running {
            self.launchd.unload_agent(
                &self.launchd_domain,
                crate::native_proxy::LAUNCH_AGENT_LABEL,
            )?;
        }
        crate::migration::restore_files(self.store.paths(), &snapshot)?;
        state.install = Some(snapshot.install.clone());
        self.store.save(&state)?;
        for id in &snapshot.stopped_ids {
            // Re-observe immediately before each restart as well: a changed
            // label/ID between RPCs is never a reason to start a foreign job.
            let current = self.require_docker_observable()?;
            let container = current
                .iter()
                .find(|container| {
                    container.id == *id && same_install_ownership(&snapshot.install, container)
                })
                .ok_or_else(|| format!("ProxyMigrationRecoveryContainerChanged: {id}"))?;
            if !container.running {
                self.docker.start_container(id)?;
            }
        }
        if !snapshot.stopped_ids.is_empty()
            && !wait_for_runtime_contract(
                self.docker.as_ref(),
                snapshot.install.host_port,
                &self.boundary_key(),
            )
        {
            return Err("ProxyMigrationRecoveryNotReady".into());
        }
        snapshot.rolled_back = true;
        crate::migration::save(self.store.paths(), &snapshot)?;
        Ok(true)
    }

    /// Read-only status may return diagnostics when Docker is unavailable.
    pub fn status(&self) -> Result<ProxyStatusView, String> {
        let state = self.store.load()?;
        if crate::migration::load(self.store.paths())?.is_some_and(|snapshot| {
            snapshot.rolled_back
                && snapshot.install.host_id == state.host_id
                && state.install.as_ref().is_some_and(|install| {
                    install.backend() == crate::platform::ProxyBackend::Docker
                        && install.host_id == snapshot.install.host_id
                        && install.install_id == snapshot.install.install_id
                })
        }) {
            let containers = self.require_docker_observable()?;
            let mut status = status_from_inventory(
                &state.install,
                &containers,
                &self.docker,
                &self.boundary_key(),
            )?;
            status.last_error = Some("proxyMigrationRolledBack".into());
            return Ok(status);
        }
        if self.native_backend_active()
            || state
                .install
                .as_ref()
                .is_some_and(|install| install.backend() == crate::platform::ProxyBackend::Native)
        {
            return crate::native_proxy::observe_native(
                &self.store,
                self.launchd.as_ref(),
                &self.launch_agents_dir,
                &self.launchd_domain,
                &self.boundary_key(),
            );
        }
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
                    proxy_backend: Some(crate::platform::PROXY_BACKEND_DOCKER.into()),
                    native_executable_digest: None,
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
        if self.native_backend_active() && !image.trim().is_empty() {
            return Err(
                "NativeProxyImageUnsupported: install the native proxy binary with proxy.update"
                    .into(),
            );
        }
        if self.native_backend_active() && image.trim().is_empty() {
            let request = json!({
                "proxyBackend": crate::platform::PROXY_BACKEND_NATIVE,
                "nativeExecutableDigest": image_digest,
            });
            return self.with_mutation_lock(|| {
                self.run_operation_locked(operation_id, "proxy.install", &request, |record| {
                    self.install_native_inner(record, image_digest.clone())
                })
            });
        }
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
        if self.native_backend_active() {
            return Err(
                "NativeProxyImageUnsupported: native proxy does not load Docker archives".into(),
            );
        }
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
        if self.native_backend_active() {
            return self.with_mutation_lock(|| {
                self.run_operation_locked(
                    &request.operation_id,
                    "proxy.start",
                    &payload,
                    |record| self.start_native_inner(record, &request),
                )
            });
        }
        self.with_mutation_lock(|| {
            self.run_operation_locked(&request.operation_id, "proxy.start", &payload, |record| {
                self.start_inner(record, &request)
            })
        })
    }

    pub fn stop(&self, operation_id: &str) -> Result<OperationResult, String> {
        let payload = json!({});
        if self.native_backend_active() {
            return self.with_mutation_lock(|| {
                self.run_operation_locked(operation_id, "proxy.stop", &payload, |record| {
                    self.stop_native_inner(record, false)
                })
            });
        }
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
                if self.native_backend_active() {
                    self.stop_native_inner(record, true)?;
                    return self.start_native_inner(
                        record,
                        &ProxyStartRequest {
                            operation_id: operation_id.into(),
                            host_port: None,
                            image: None,
                        },
                    );
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

    pub fn repair(&self, operation_id: &str) -> Result<OperationResult, String> {
        let payload = json!({});
        self.with_mutation_lock(|| {
            self.run_operation_locked(operation_id, "repair.run", &payload, |record| {
                let state = self.store.load()?;
                if state.install.is_none() {
                    return Err("RepairRequiresInstall: no managed proxy install exists".into());
                }
                if self.native_backend_active() {
                    let status = crate::native_proxy::observe_native(
                        &self.store,
                        self.launchd.as_ref(),
                        &self.launch_agents_dir,
                        &self.launchd_domain,
                        &self.boundary_key(),
                    )?;
                    if status.ready {
                        return serde_json::to_value(status).map_err(|error| error.to_string());
                    }
                    self.stop_native_inner(record, true)?;
                    return self.start_native_inner(
                        record,
                        &ProxyStartRequest {
                            operation_id: operation_id.into(),
                            host_port: None,
                            image: None,
                        },
                    );
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
                if self.native_backend_active() {
                    self.update_native_inner(record, image, image_digest.clone())
                } else {
                    self.update_inner(record, image, image_digest.clone())
                }
            })
        })
    }

    pub fn rollback(&self, operation_id: &str) -> Result<OperationResult, String> {
        let payload = json!({});
        self.with_mutation_lock(|| {
            self.run_operation_locked(operation_id, "proxy.rollback", &payload, |record| {
                if self.native_backend_active() {
                    self.rollback_native_inner(record)
                } else {
                    Err(
                        "ProxyRollbackRequiresNativeBackend: docker rollback is stop plus a previous image install"
                            .into(),
                    )
                }
            })
        })
    }

    pub fn logs(&self, max_bytes: Option<u64>) -> Result<ProxyLogsView, String> {
        let max = max_bytes.unwrap_or(64 * 1024).min(usize::MAX as u64) as usize;
        if self.native_backend_active() {
            return crate::native_proxy::logs_native(&self.store, self.launchd.as_ref(), max);
        }
        docker_file_logs(&self.store.paths().proxy_logs_dir, max)
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

    fn native_backend_active(&self) -> bool {
        if self.prefer_docker {
            return false;
        }
        if self.prefer_native {
            return true;
        }
        if let Ok(state) = self.store.load() {
            if let Some(install) = state.install {
                return install.backend() == crate::platform::ProxyBackend::Native;
            }
        }
        crate::platform::current_platform()
            .map(|platform| platform.proxy_backend() == crate::platform::ProxyBackend::Native)
            .unwrap_or(false)
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
                if !self.native_backend_active() {
                    // Fail closed before marking Executing when Docker cannot be observed.
                    let _ = self.require_docker_observable()?;
                }
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
                        let error = if self.native_backend_active()
                            && matches!(
                                method,
                                "proxy.install"
                                    | "proxy.update"
                                    | "proxy.start"
                                    | "proxy.restart"
                                    | "proxy.migrationCommit"
                            ) {
                            match self.rollback_migration_inner() {
                                Ok(true) => format!("ProxyMigrationRolledBack: {error}"),
                                Ok(false) => error,
                                Err(recovery) => format!(
                                    "ProxyMigrationRecoveryFailed: {recovery}; cause: {error}"
                                ),
                            }
                        } else {
                            error
                        };
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
        write_default_proxy_config(
            paths,
            &host_id,
            &install_id,
            &config_hash,
            &resolved.source,
            "0.0.0.0:15721",
        )?;
        let install = InstallRecord {
            install_id,
            host_id,
            image: resolved.source.clone(),
            image_digest: resolved.repo_digest.clone(),
            config_hash,
            host_port,
            updated_at: Utc::now(),
            proxy_backend: Some(crate::platform::PROXY_BACKEND_DOCKER.into()),
            native_executable_digest: None,
        };
        state.install = Some(install.clone());
        self.store.save(&state)?;
        serde_json::to_value(&install).map_err(|error| error.to_string())
    }

    fn install_native_inner(
        &self,
        _record: &mut OperationRecord,
        native_executable_digest: Option<String>,
    ) -> Result<Value, String> {
        if native_executable_digest
            .as_deref()
            .is_some_and(|value| value.starts_with("vellum-proxy:") || value.contains("ghcr.io/"))
        {
            return Err(
                "NativeProxyDigestInvalid: native executable digest must not be a container image name"
                    .into(),
            );
        }
        let paths = self.store.paths();
        prepare_proxy_mounts(paths)?;
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
            .unwrap_or_else(|| self.launchd.default_port());
        let binary = crate::native_proxy::native_proxy_bin(paths);
        if !binary.is_file() {
            return Err(format!("NativeProxyBinaryMissing: {}", binary.display()));
        }
        let payload = std::fs::metadata(&binary)
            .map(|meta| meta.len())
            .unwrap_or(0);
        crate::space::gate_replace(crate::space::free_bytes_for_path(&paths.root), payload)?;
        let actual_digest = crate::native_proxy::digest_native_binary(&binary)?;
        if let Some(expected) = native_executable_digest.as_deref() {
            if !actual_digest.eq_ignore_ascii_case(expected) {
                return Err(format!(
                    "NativeProxyDigestMismatch: expected {expected}, got {actual_digest}"
                ));
            }
        }
        let digest = native_executable_digest.unwrap_or(actual_digest);
        // Keep the old Docker record until migration is fully persisted, so a
        // retry after interruption still inventories and stops the owned container.
        if let Some(old) = state
            .install
            .as_ref()
            .filter(|item| item.backend() == crate::platform::ProxyBackend::Docker)
        {
            // Reject an unavailable service manager before interrupting Docker.
            self.launchd.inspect_agent(
                &self.launchd_domain,
                crate::native_proxy::LAUNCH_AGENT_LABEL,
            )?;
            let mut snapshot = crate::migration::load(paths)?;
            if snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.rolled_back)
            {
                snapshot = None;
            }
            for container in self.require_docker_observable()? {
                if same_install_ownership(old, &container) && container.running {
                    if snapshot.is_none() {
                        snapshot = Some(crate::migration::capture(paths, old.clone())?);
                    }
                    let saved = snapshot.as_mut().ok_or("ProxyMigrationSnapshotMissing")?;
                    if saved.install.host_id != old.host_id
                        || saved.install.install_id != old.install_id
                    {
                        return Err("ProxyMigrationSnapshotOwnershipMismatch".into());
                    }
                    if !saved.stopped_ids.contains(&container.id) {
                        saved.stopped_ids.push(container.id.clone());
                    }
                    crate::migration::save(paths, saved)?;
                    self.docker.stop_container(&container.id)?;
                }
            }
            if snapshot.is_none() {
                crate::migration::clear(paths)?;
            }
        }
        let config_path = paths.proxy_config_dir.join("proxy.toml");
        let config_hash = if config_path.is_file() {
            // Preserve even an old config that the normal configure phase will
            // upgrade. Never replace user/model/credential state with mock routes.
            state
                .install
                .as_ref()
                .map(|item| item.config_hash.clone())
                .unwrap_or_else(|| config_hash_for(&digest, host_port, &install_id))
        } else {
            let hash = config_hash_for(&digest, host_port, &install_id);
            write_default_proxy_config(
                paths,
                &host_id,
                &install_id,
                &hash,
                "vellum-proxy-daemon",
                &crate::native_proxy::native_listen(host_port),
            )?;
            hash
        };
        let spec = crate::native_proxy::LaunchAgentSpec::new(
            binary,
            paths.proxy_config_dir.join("proxy.toml"),
            crate::native_proxy::native_listen(host_port),
            &paths.logs_dir,
            paths.root.clone(),
        );
        self.launchd.write_service(&spec, &self.launch_agents_dir)?;
        let install = InstallRecord {
            install_id,
            host_id,
            image: String::new(),
            image_digest: None,
            config_hash,
            host_port,
            updated_at: Utc::now(),
            proxy_backend: Some(crate::platform::PROXY_BACKEND_NATIVE.into()),
            native_executable_digest: Some(digest),
        };
        state.install = Some(install.clone());
        self.store.save(&state)?;
        serde_json::to_value(&install).map_err(|error| error.to_string())
    }

    fn start_native_inner(
        &self,
        _record: &mut OperationRecord,
        request: &ProxyStartRequest,
    ) -> Result<Value, String> {
        if self
            .store
            .load()?
            .install
            .as_ref()
            .is_none_or(|install| install.backend() != crate::platform::ProxyBackend::Native)
        {
            self.install_native_inner(_record, None)?;
        }
        let view = crate::native_proxy::start_native(
            &self.store,
            self.launchd.as_ref(),
            &self.launch_agents_dir,
            &self.launchd_domain,
            request.host_port,
            &self.boundary_key(),
        )?;
        if crate::migration::load(self.store.paths())?.is_some_and(|snapshot| !snapshot.rolled_back)
            && !view.ready
        {
            return Err("ProxyMigrationNativeNotReady".into());
        }
        serde_json::to_value(&view).map_err(|error| error.to_string())
    }

    fn stop_native_inner(
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
        let view = crate::native_proxy::stop_native(
            &self.store,
            self.launchd.as_ref(),
            &self.launch_agents_dir,
            &self.launchd_domain,
            &self.boundary_key(),
        )?;
        serde_json::to_value(&view).map_err(|error| error.to_string())
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
                "0.0.0.0:15721",
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

    fn update_native_inner(
        &self,
        record: &mut OperationRecord,
        image: &str,
        image_digest: Option<String>,
    ) -> Result<Value, String> {
        if looks_like_container_image(image) {
            return Err(
                "NativeProxyDigestInvalid: native proxy.update must not receive a container image name"
                    .into(),
            );
        }
        let resume = self.launchd.stop_before_replace()
            && self
                .launchd
                .inspect_agent(
                    &self.launchd_domain,
                    crate::native_proxy::LAUNCH_AGENT_LABEL,
                )?
                .running;
        let target = crate::native_proxy::native_proxy_bin(self.store.paths());
        std::fs::create_dir_all(target.parent().expect("native binary directory"))
            .map_err(|error| format!("NativeProxyBinaryDirectoryFailed: {error}"))?;
        let staged = image.trim();
        if !staged.is_empty() {
            let expected =
                match image_digest.as_deref() {
                    Some(digest) if !digest.is_empty() => digest.to_string(),
                    _ => return Err(
                        "NativeProxyDigestMissing: staged native proxy requires an expected digest"
                            .into(),
                    ),
                };
            let payload = std::fs::metadata(staged)
                .map(|meta| meta.len())
                .unwrap_or(0);
            crate::space::gate_replace(
                crate::space::free_bytes_for_path(self.store.paths().root.as_path()),
                payload,
            )?;
            if self.launchd.stop_before_replace() {
                // Reject an invalid staged payload before interrupting a
                // healthy detached process. install_binary verifies again.
                crate::update::reject_symlink(Path::new(staged))?;
                let actual = crate::update::sha256_file(Path::new(staged))?;
                let expected = expected
                    .trim()
                    .strip_prefix("sha256:")
                    .unwrap_or(expected.trim());
                if actual != expected {
                    return Err(format!(
                        "AgentDigestMismatch: expected {expected}, resolved {actual}"
                    ));
                }
                self.launchd.unload_agent(
                    &self.launchd_domain,
                    crate::native_proxy::LAUNCH_AGENT_LABEL,
                )?;
            }
            crate::update::install_binary(Path::new(staged), &target, &expected)?;
        }
        let digest = image_digest
            .clone()
            .filter(|value| !value.is_empty())
            .or_else(|| crate::native_proxy::digest_native_binary(&target).ok());
        self.install_native_inner(record, digest)?;
        if resume {
            let view = crate::native_proxy::start_native(
                &self.store,
                self.launchd.as_ref(),
                &self.launch_agents_dir,
                &self.launchd_domain,
                None,
                &self.boundary_key(),
            )?;
            return serde_json::to_value(&view).map_err(|error| error.to_string());
        }
        let view = crate::native_proxy::reload_native_if_loaded(
            &self.store,
            self.launchd.as_ref(),
            &self.launch_agents_dir,
            &self.launchd_domain,
            &self.boundary_key(),
        )?;
        serde_json::to_value(&view).map_err(|error| error.to_string())
    }

    fn rollback_native_inner(&self, record: &mut OperationRecord) -> Result<Value, String> {
        if self.store.load()?.install.is_none() {
            return Err("NativeProxyInstallMissing: install the native proxy first".into());
        }
        let target = crate::native_proxy::native_proxy_bin(self.store.paths());
        if self.launchd.stop_before_replace() {
            let rollback = target.with_extension("vellum-rollback");
            crate::update::reject_symlink(&rollback)?;
            if !rollback.is_file() {
                return Err("AgentRollbackUnavailable: no rollback binary exists".into());
            }
        }
        let resume = self.launchd.stop_before_replace()
            && self
                .launchd
                .inspect_agent(
                    &self.launchd_domain,
                    crate::native_proxy::LAUNCH_AGENT_LABEL,
                )?
                .running;
        if self.launchd.stop_before_replace() {
            self.launchd.unload_agent(
                &self.launchd_domain,
                crate::native_proxy::LAUNCH_AGENT_LABEL,
            )?;
        }
        crate::update::rollback_binary(&target)?;
        let digest = crate::native_proxy::digest_native_binary(&target)?;
        self.install_native_inner(record, Some(digest))?;
        if resume {
            let view = crate::native_proxy::start_native(
                &self.store,
                self.launchd.as_ref(),
                &self.launch_agents_dir,
                &self.launchd_domain,
                None,
                &self.boundary_key(),
            )?;
            return serde_json::to_value(&view).map_err(|error| error.to_string());
        }
        let view = crate::native_proxy::reload_native_if_loaded(
            &self.store,
            self.launchd.as_ref(),
            &self.launch_agents_dir,
            &self.launchd_domain,
            &self.boundary_key(),
        )?;
        serde_json::to_value(&view).map_err(|error| error.to_string())
    }
}

fn looks_like_container_image(image: &str) -> bool {
    let trimmed = image.trim();
    trimmed.starts_with("vellum-proxy:")
        || trimmed.contains("ghcr.io/")
        || trimmed.contains("@sha256:")
}

fn docker_file_logs(dir: &Path, max_bytes: usize) -> Result<ProxyLogsView, String> {
    let stdout_path = dir.join("proxy.out.log");
    let stderr_path = dir.join("proxy.err.log");
    let mut view =
        crate::native_proxy::read_launchd_log_files(&stdout_path, &stderr_path, max_bytes)?;
    view.source = "docker-files".into();
    Ok(view)
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
            ..Default::default()
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
        ..Default::default()
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
        proxy_backend: Some(install.backend().as_str().into()),
        native_executable_digest: install.native_executable_digest.clone(),
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
    listen: &str,
) -> Result<(), String> {
    paths.ensure()?;
    let config = format!(
        "schema_version = 2\n\
listen = \"{listen}\"\n\
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

    #[test]
    fn linux_native_update_installs_without_docker_and_uses_systemd_unit() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        let service = Arc::new(crate::native_proxy::MemoryLaunchd {
            systemd: true,
            ready: Mutex::new(true),
            ..crate::native_proxy::MemoryLaunchd::default()
        });
        let units = temp.path().join("home/.config/systemd/user");
        let mut manager = ProxyManager::with_native(
            AgentStateStore::new(paths.clone()),
            service.clone(),
            units.clone(),
            "user",
        );
        manager.docker = Arc::new(FailingListDocker);
        let staged = temp.path().join("proxy-stage");
        std::fs::write(&staged, b"native-linux").unwrap();
        let digest = crate::native_proxy::digest_native_binary(&staged).unwrap();
        assert!(manager
            .update("missing-digest", staged.to_str().unwrap(), None)
            .unwrap_err()
            .contains("NativeProxyDigestMissing"));
        manager
            .update("install", staged.to_str().unwrap(), Some(digest.clone()))
            .unwrap();
        assert!(units
            .join(crate::native_proxy::SYSTEMD_PROXY_UNIT)
            .is_file());
        let installed = manager.store.load().unwrap().install.unwrap();
        assert_eq!(installed.host_port, 15721);
        assert_eq!(
            installed.native_executable_digest.as_deref(),
            Some(digest.as_str())
        );
        assert!(installed.image.is_empty());
        assert!(!manager.status().unwrap().running);
        manager
            .start(ProxyStartRequest {
                operation_id: "start".into(),
                host_port: None,
                image: None,
            })
            .unwrap();
        assert!(manager.status().unwrap().ready);
        assert_eq!(
            service.loads.lock().unwrap()[0],
            units.join(crate::native_proxy::SYSTEMD_PROXY_UNIT)
        );
        manager.stop("stop").unwrap();
        assert!(!manager.status().unwrap().running);
    }

    #[test]
    fn docker_migration_preserves_config_credentials_and_foreign_containers() {
        let temp = tempfile::tempdir().unwrap();
        let (old, docker) = manager(&temp);
        old.install("legacy-install", "vellum-proxy:old", None)
            .unwrap();
        old.start(ProxyStartRequest {
            operation_id: "legacy-start".into(),
            host_port: None,
            image: None,
        })
        .unwrap();
        let state = old.store.load().unwrap();
        let paths = old.store.paths().clone();
        let config = std::fs::read(paths.proxy_config_dir.join("proxy.toml")).unwrap();
        std::fs::write(paths.secrets_dir.join("provider.json"), b"keep-secret").unwrap();
        {
            let mut containers = docker.containers.lock().unwrap();
            let mut unlabeled = containers[0].clone();
            unlabeled.id = "unlabeled".into();
            unlabeled.labels.clear();
            let mut foreign = containers[0].clone();
            foreign.id = "foreign".into();
            foreign
                .labels
                .insert("io.vellum.host-id".into(), "another-host".into());
            let mut foreign_install = containers[0].clone();
            foreign_install.id = "foreign-install".into();
            foreign_install
                .labels
                .insert("io.vellum.install-id".into(), "another-install".into());
            containers.extend([unlabeled, foreign, foreign_install]);
        }
        let service = Arc::new(crate::native_proxy::MemoryLaunchd {
            systemd: true,
            ready: Mutex::new(true),
            ..Default::default()
        });
        let mut native = ProxyManager::with_native(
            AgentStateStore::new(paths.clone()),
            service.clone(),
            temp.path().join("units"),
            "user",
        );
        native.docker = docker.clone();
        *service.port_busy.lock().unwrap() = true;
        assert_eq!(
            native.select_port(None).unwrap(),
            15721,
            "labelled migration listener retains its port"
        );
        docker.containers.lock().unwrap()[0].running = false;
        assert!(
            native
                .select_port(None)
                .unwrap_err()
                .starts_with("proxyPortConflict:"),
            "foreign/unlabelled listeners must not count as managed"
        );
        docker.containers.lock().unwrap()[0].running = true;
        *service.port_busy.lock().unwrap() = false;
        let staged = temp.path().join("proxy-stage");
        std::fs::write(&staged, b"native-linux").unwrap();
        let digest = crate::native_proxy::digest_native_binary(&staged).unwrap();
        native
            .update("migrate", staged.to_str().unwrap(), Some(digest.clone()))
            .unwrap();
        assert!(
            native
                .update("migrate", staged.to_str().unwrap(), Some(digest))
                .unwrap()
                .idempotent_replay
        );
        let migrated = native.store.load().unwrap().install.unwrap();
        assert_eq!(migrated.install_id, state.install.unwrap().install_id);
        assert_eq!(migrated.host_port, 15721);
        assert_eq!(
            std::fs::read(paths.proxy_config_dir.join("proxy.toml")).unwrap(),
            config
        );
        assert_eq!(
            std::fs::read(paths.secrets_dir.join("provider.json")).unwrap(),
            b"keep-secret"
        );
        let containers = docker.containers.lock().unwrap();
        assert!(!containers[0].running);
        assert!(containers[1..].iter().all(|container| container.running));
        drop(containers);
        native
            .start(ProxyStartRequest {
                operation_id: "native-start".into(),
                host_port: None,
                image: None,
            })
            .unwrap();
        assert!(native.status().unwrap().ready);
        assert_eq!(service.loads.lock().unwrap().len(), 1);
        native.migration_commit("migration-commit").unwrap();
        assert!(crate::migration::load(&paths).unwrap().is_none());
        let containers = docker.containers.lock().unwrap();
        assert_eq!(containers.len(), 3);
        assert!(containers
            .iter()
            .all(|c| c.id == "unlabeled" || c.id == "foreign" || c.id == "foreign-install"));
        drop(containers);
        native.migration_rollback("after-commit").unwrap();
        assert!(docker.restarted.lock().unwrap().is_empty());
    }

    fn migration_fixture(
        temp: &tempfile::TempDir,
    ) -> (
        ProxyManager,
        Arc<FakeDockerClient>,
        Arc<crate::native_proxy::MemoryLaunchd>,
    ) {
        let (old, docker) = manager(temp);
        old.install("old-install", "vellum-proxy:old", None)
            .unwrap();
        old.start(ProxyStartRequest {
            operation_id: "old-start".into(),
            host_port: None,
            image: None,
        })
        .unwrap();
        let paths = old.store.paths().clone();
        crate::configuration::put_credential(&paths, "provider-credential", "original-secret")
            .unwrap();
        {
            let mut containers = docker.containers.lock().unwrap();
            let mut foreign = containers[0].clone();
            foreign.id = "foreign".into();
            foreign.labels.clear();
            let mut stopped = containers[0].clone();
            stopped.id = "already-stopped".into();
            stopped.running = false;
            containers.extend([foreign, stopped]);
        }
        let service = Arc::new(crate::native_proxy::MemoryLaunchd {
            systemd: true,
            ready: Mutex::new(true),
            ..Default::default()
        });
        let mut native = ProxyManager::with_native(
            AgentStateStore::new(paths),
            service.clone(),
            temp.path().join("units"),
            "user",
        );
        native.docker = docker.clone();
        (native, docker, service)
    }

    fn stage_migration(
        native: &ProxyManager,
        temp: &tempfile::TempDir,
        id: &str,
    ) -> Result<OperationResult, String> {
        let staged = temp.path().join("native-stage");
        std::fs::write(&staged, b"native-linux").unwrap();
        let digest = crate::native_proxy::digest_native_binary(&staged).unwrap();
        native.update(id, staged.to_str().unwrap(), Some(digest))
    }

    #[test]
    fn migration_service_write_failure_restores_only_previously_running_owned_container() {
        let temp = tempfile::tempdir().unwrap();
        let (native, docker, service) = migration_fixture(&temp);
        let before = native.store.load().unwrap();
        *service.fail_write.lock().unwrap() = true;
        let error = stage_migration(&native, &temp, "migrate").unwrap_err();
        assert!(error.contains("ProxyMigrationRolledBack"), "{error}");
        assert_eq!(native.store.load().unwrap(), before);
        let containers = docker.containers.lock().unwrap();
        assert!(containers[0].running);
        assert!(containers[1].running);
        assert!(!containers[2].running);
        assert_eq!(
            *docker.restarted.lock().unwrap(),
            vec![containers[0].id.clone()]
        );
        assert!(
            docker.removed.lock().unwrap().is_empty(),
            "rollback must retain the Docker fallback"
        );
        drop(containers);
        assert!(native.status().unwrap().ready);
        assert_eq!(
            native.status().unwrap().last_error.as_deref(),
            Some("proxyMigrationRolledBack")
        );
        *service.fail_write.lock().unwrap() = false;
        stage_migration(&native, &temp, "retry-migration").unwrap();
        native
            .start(ProxyStartRequest {
                operation_id: "retry-start".into(),
                host_port: None,
                image: None,
            })
            .unwrap();
        native.migration_commit("retry-commit").unwrap();
        assert!(crate::migration::load(native.store.paths())
            .unwrap()
            .is_none());
        assert!(native.status().unwrap().ready);
        assert!(docker
            .containers
            .lock()
            .unwrap()
            .iter()
            .all(|c| c.id == "foreign" || c.id == "already-stopped"));
    }

    #[test]
    fn migration_commit_cleanup_failure_is_nonfatal_and_preserves_images() {
        let temp = tempfile::tempdir().unwrap();
        let (native, docker, _) = migration_fixture(&temp);
        stage_migration(&native, &temp, "migrate").unwrap();
        native
            .start(ProxyStartRequest {
                operation_id: "native-start".into(),
                host_port: None,
                image: None,
            })
            .unwrap();
        let images = docker.repo_digests.lock().unwrap().clone();
        *docker.fail_remove.lock().unwrap() = true;
        let result = native.migration_commit("commit").unwrap();
        assert!(serde_json::to_value(result)
            .unwrap()
            .to_string()
            .contains("ProxyMigrationContainerCleanupFailed"));
        assert!(crate::migration::load(native.store.paths())
            .unwrap()
            .is_none());
        assert!(native.status().unwrap().ready);
        assert_eq!(docker.removed.lock().unwrap().len(), 1);
        assert_eq!(*docker.repo_digests.lock().unwrap(), images);
    }

    #[test]
    fn converge_removes_old_committed_orphans_and_is_repeatable() {
        let temp = tempfile::tempdir().unwrap();
        let (native, docker, _) = migration_fixture(&temp);
        stage_migration(&native, &temp, "migrate").unwrap();
        native
            .start(ProxyStartRequest {
                operation_id: "start".into(),
                host_port: None,
                image: None,
            })
            .unwrap();
        // Simulate the old agent's successful commit: snapshot gone, containers retained.
        crate::migration::clear(native.store.paths()).unwrap();
        let images = docker.repo_digests.lock().unwrap().clone();
        native.migration_commit("converge").unwrap();
        assert_eq!(docker.removed.lock().unwrap().len(), 2);
        assert_eq!(docker.containers.lock().unwrap()[0].id, "foreign");
        native.migration_commit("converge-again").unwrap();
        assert_eq!(docker.removed.lock().unwrap().len(), 2);
        assert_eq!(*docker.repo_digests.lock().unwrap(), images);
    }

    #[test]
    fn orphan_cleanup_refuses_pending_migrations() {
        let temp = tempfile::tempdir().unwrap();
        let (native, docker, _) = migration_fixture(&temp);
        stage_migration(&native, &temp, "migrate").unwrap();
        native
            .start(ProxyStartRequest {
                operation_id: "start".into(),
                host_port: None,
                image: None,
            })
            .unwrap();
        assert_eq!(
            native
                .with_mutation_lock(|| native.cleanup_legacy_orphans())
                .unwrap(),
            0
        );
        assert!(docker.removed.lock().unwrap().is_empty());
        assert!(crate::migration::load(native.store.paths())
            .unwrap()
            .is_some());
    }

    #[test]
    fn orphan_cleanup_requires_exact_labels_stopped_container_and_ready_native() {
        for condition in ["labels", "running", "unready", "backend"] {
            let temp = tempfile::tempdir().unwrap();
            let (native, docker, service) = migration_fixture(&temp);
            stage_migration(&native, &temp, "migrate").unwrap();
            native
                .start(ProxyStartRequest {
                    operation_id: "start".into(),
                    host_port: None,
                    image: None,
                })
                .unwrap();
            crate::migration::clear(native.store.paths()).unwrap();
            match condition {
                "labels" => {
                    let mut containers = docker.containers.lock().unwrap();
                    containers[0].labels.clear();
                    containers[2]
                        .labels
                        .insert("io.vellum.install-id".into(), "another-install".into());
                }
                "running" => {
                    for c in docker.containers.lock().unwrap().iter_mut() {
                        c.running = true;
                    }
                }
                "unready" => *service.ready.lock().unwrap() = false,
                "backend" => {
                    let mut state = native.store.load().unwrap();
                    state.install.as_mut().unwrap().proxy_backend = Some("docker".into());
                    native.store.save(&state).unwrap();
                }
                _ => unreachable!(),
            }
            native.migration_commit("converge").unwrap();
            assert!(docker.removed.lock().unwrap().is_empty(), "{condition}");
        }
    }

    #[test]
    fn orphan_cleanup_failure_does_not_fail_converge_and_next_converge_retries() {
        let temp = tempfile::tempdir().unwrap();
        let (native, docker, _) = migration_fixture(&temp);
        stage_migration(&native, &temp, "migrate").unwrap();
        native
            .start(ProxyStartRequest {
                operation_id: "start".into(),
                host_port: None,
                image: None,
            })
            .unwrap();
        crate::migration::clear(native.store.paths()).unwrap();
        *docker.fail_remove.lock().unwrap() = true;
        let result = native.migration_commit("failed-cleanup").unwrap();
        assert!(serde_json::to_value(result)
            .unwrap()
            .to_string()
            .contains("ProxyMigrationContainerCleanupFailed"));
        assert_eq!(docker.containers.lock().unwrap().len(), 3);
        *docker.fail_remove.lock().unwrap() = false;
        native.migration_commit("retry-cleanup").unwrap();
        assert_eq!(docker.containers.lock().unwrap().len(), 1);
    }

    #[test]
    fn unavailable_docker_skips_cleanup_without_warning() {
        for error in [
            "docker executable not found: No such file or directory",
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock",
            "permission denied connecting to Docker",
        ] {
            for pending in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let (native, docker, _) = migration_fixture(&temp);
                stage_migration(&native, &temp, "migrate").unwrap();
                native
                    .start(ProxyStartRequest {
                        operation_id: "start".into(),
                        host_port: None,
                        image: None,
                    })
                    .unwrap();
                if !pending {
                    crate::migration::clear(native.store.paths()).unwrap();
                }
                *docker.fail_list.lock().unwrap() = Some(error.into());
                assert_eq!(
                    native
                        .with_mutation_lock(|| native.cleanup_legacy_orphans())
                        .unwrap(),
                    0
                );
                let result = native.migration_commit("commit").unwrap();
                assert_eq!(result.status, "ok");
                assert!(result.detail["cleanupWarning"].is_null(), "{error}");
                if !pending {
                    assert_eq!(result.detail["orphanContainersRemoved"], 0);
                }
                assert!(docker.removed.lock().unwrap().is_empty());
                assert_eq!(docker.containers.lock().unwrap().len(), 3);
            }
        }
    }

    #[test]
    fn migration_start_failure_restores_config_credentials_and_is_reentrant() {
        let temp = tempfile::tempdir().unwrap();
        let (native, docker, service) = migration_fixture(&temp);
        let paths = native.store.paths();
        let original = std::fs::read(paths.proxy_config_dir.join("proxy.toml")).unwrap();
        stage_migration(&native, &temp, "migrate").unwrap();
        // Later RPCs may have replaced both config and a credential before start fails.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let directory = paths.secrets_dir.join("proxy-migration-rollback");
            assert_eq!(
                std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(directory.join("snapshot.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let mut changed = original.clone();
        changed.extend_from_slice(b"\n# changed during migration\n");
        std::fs::write(paths.proxy_config_dir.join("proxy.toml"), changed).unwrap();
        std::fs::write(
            paths.secrets_dir.join("provider-credential"),
            b"replacement-secret",
        )
        .unwrap();
        *service.fail_load.lock().unwrap() = true;
        let error = native
            .start(ProxyStartRequest {
                operation_id: "native-start".into(),
                host_port: None,
                image: None,
            })
            .unwrap_err();
        assert!(error.contains("ProxyMigrationRolledBack"), "{error}");
        assert!(!error.contains("original-secret"));
        assert!(error.contains("injected service load failure"), "{error}");
        assert_eq!(
            std::fs::read(paths.proxy_config_dir.join("proxy.toml")).unwrap(),
            original
        );
        assert_eq!(
            std::fs::read(paths.secrets_dir.join("provider-credential")).unwrap(),
            b"original-secret"
        );
        native.migration_rollback("retry-rollback").unwrap();
        assert_eq!(docker.restarted.lock().unwrap().len(), 1);
        assert!(!*service.running.lock().unwrap());
        let bundle = crate::support::create_support_bundle(paths, json!({})).unwrap();
        let bytes = std::fs::read(bundle.path).unwrap();
        assert!(!bytes
            .windows(b"original-secret".len())
            .any(|window| window == b"original-secret"));
        assert!(!bytes
            .windows(b"replacement-secret".len())
            .any(|window| window == b"replacement-secret"));
    }

    #[test]
    fn migration_native_contract_timeout_restores_docker_and_never_commits() {
        let temp = tempfile::tempdir().unwrap();
        let (native, docker, service) = migration_fixture(&temp);
        stage_migration(&native, &temp, "stage-timeout").unwrap();
        *service.ready.lock().unwrap() = false;
        let error = native
            .start(ProxyStartRequest {
                operation_id: "timeout-start".into(),
                host_port: None,
                image: None,
            })
            .unwrap_err();
        assert!(error.contains("ProxyMigrationRolledBack"), "{error}");
        assert!(error.contains("NativeProxyRuntimeNotReady"), "{error}");
        assert!(service.probes.load(std::sync::atomic::Ordering::Relaxed) > 1);
        assert!(docker.containers.lock().unwrap()[0].running);
        assert_eq!(docker.restarted.lock().unwrap().len(), 1);
    }

    #[test]
    fn migration_recovery_failure_keeps_snapshot_for_retry_after_process_restart() {
        let temp = tempfile::tempdir().unwrap();
        let (native, docker, service) = migration_fixture(&temp);
        stage_migration(&native, &temp, "migrate").unwrap();
        *docker.fail_restart.lock().unwrap() = true;
        assert!(native.migration_rollback("failed-recovery").is_err());
        assert!(
            !crate::migration::load(native.store.paths())
                .unwrap()
                .unwrap()
                .rolled_back
        );
        let mut restarted = ProxyManager::with_native(
            AgentStateStore::new(native.store.paths().clone()),
            service,
            temp.path().join("units"),
            "user",
        );
        restarted.docker = docker.clone();
        *docker.fail_restart.lock().unwrap() = false;
        restarted.migration_rollback("retry-recovery").unwrap();
        assert!(docker.containers.lock().unwrap()[0].running);
        assert!(
            crate::migration::load(native.store.paths())
                .unwrap()
                .unwrap()
                .rolled_back
        );
    }

    #[test]
    fn migration_recovery_rejects_changed_ownership_before_restoring_files() {
        let temp = tempfile::tempdir().unwrap();
        let (native, docker, _) = migration_fixture(&temp);
        stage_migration(&native, &temp, "migrate").unwrap();
        docker.containers.lock().unwrap()[0].labels.clear();
        std::fs::write(
            native.store.paths().secrets_dir.join("provider-credential"),
            b"new-secret",
        )
        .unwrap();
        let error = native.migration_rollback("rollback").unwrap_err();
        assert!(error.contains("ProxyMigrationRecoveryContainerChanged"));
        assert!(docker.restarted.lock().unwrap().is_empty());
        assert_eq!(
            std::fs::read(native.store.paths().secrets_dir.join("provider-credential")).unwrap(),
            b"new-secret"
        );
        assert!(!docker.containers.lock().unwrap()[0].running);
    }

    #[test]
    fn docker_migration_fails_closed_when_legacy_inventory_is_unobservable() {
        let temp = tempfile::tempdir().unwrap();
        let (old, _) = manager(&temp);
        old.install("legacy-install", "vellum-proxy:old", None)
            .unwrap();
        let paths = old.store.paths().clone();
        let before = old.store.load().unwrap();
        let bin = crate::native_proxy::native_proxy_bin(&paths);
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"native").unwrap();
        let mut native = ProxyManager::with_native(
            AgentStateStore::new(paths),
            Arc::new(crate::native_proxy::MemoryLaunchd::default()),
            temp.path().join("units"),
            "user",
        );
        native.docker = Arc::new(FailingListDocker);
        assert!(native
            .install("migrate", "", None)
            .unwrap_err()
            .contains("refusing mutation"));
        assert_eq!(native.store.load().unwrap(), before);
    }

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
    fn docker_rollback_is_refused_and_does_not_touch_install() {
        let temp = tempfile::tempdir().unwrap();
        let (manager, _) = manager(&temp);
        manager
            .install("op-install", "vellum-proxy:local", None)
            .unwrap();
        let before = manager.store.load().unwrap();
        let err = manager.rollback("op-rb").unwrap_err();
        assert!(err.contains("ProxyRollbackRequiresNativeBackend"));
        assert_eq!(manager.store.load().unwrap(), before);
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

    #[test]
    fn native_install_and_update_gate_space_before_plist_or_binary_swap() {
        let source = include_str!("proxy.rs");
        let install_fn = source
            .find("fn install_native_inner")
            .expect("install_native_inner");
        let install_body = &source[install_fn..];
        let install_gate = install_body
            .find("space::gate_replace")
            .expect("install_native_inner must call gate_replace");
        let plist = install_body
            .find("write_service")
            .expect("install writes plist");
        assert!(
            install_gate < plist,
            "space gate must run before writing the LaunchAgent plist"
        );
        let update_fn = source
            .find("fn update_native_inner")
            .expect("update_native_inner");
        let update_body = &source[update_fn..];
        let update_gate = update_body
            .find("space::gate_replace")
            .expect("update_native_inner must call gate_replace");
        let binary = update_body
            .find("install_binary")
            .expect("update replaces the binary");
        assert!(
            update_gate < binary,
            "space gate must run before binary swap"
        );
    }

    #[test]
    fn native_install_start_stop_status_uses_launchd_not_a_fabricated_running_flag() {
        let temp = tempfile::tempdir().unwrap();
        let store = AgentStateStore::new(AgentPaths::from_root(temp.path().join("state")));
        store.paths().ensure().unwrap();
        let bin = crate::native_proxy::native_proxy_bin(store.paths());
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"vellum-proxy-daemon").unwrap();
        let digest = crate::native_proxy::digest_native_binary(&bin).unwrap();
        let agents = temp.path().join("Library").join("LaunchAgents");
        let launchd = Arc::new(crate::native_proxy::MemoryLaunchd {
            ready: Mutex::new(true),
            ..crate::native_proxy::MemoryLaunchd::default()
        });
        let manager = ProxyManager::with_native(store, launchd.clone(), agents.clone(), "gui/501");

        let installed = manager
            .install("op-n-install", "", Some(digest.clone()))
            .unwrap();
        let record: InstallRecord = serde_json::from_value(installed.detail).unwrap();
        assert_eq!(record.host_port, 15722);
        assert_eq!(record.backend(), crate::platform::ProxyBackend::Native);
        assert!(record.image.is_empty());
        assert_ne!(record.image, "vellum-proxy:native");
        assert_eq!(
            record.native_executable_digest.as_deref(),
            Some(digest.as_str())
        );
        let plist_path = crate::native_proxy::LaunchAgentSpec::plist_path(&agents);
        assert!(plist_path.is_file());
        assert!(
            !manager.status().unwrap().running,
            "install must not fabricate a running process"
        );

        manager
            .start(ProxyStartRequest {
                operation_id: "op-n-start".into(),
                host_port: Some(15722),
                image: None,
            })
            .unwrap();
        let started = manager.status().unwrap();
        assert!(
            started.running,
            "status must follow launchctl, not a constant"
        );
        assert!(started.ready);
        assert_eq!(started.host_port, Some(15722));
        let plist = std::fs::read_to_string(&plist_path).unwrap();
        assert!(plist.contains("<string>serve</string>"));
        assert!(plist.contains("<string>127.0.0.1:15722</string>"));
        assert!(plist.contains(&bin.to_string_lossy().to_string()));
        assert_eq!(launchd.loads.lock().unwrap().len(), 1);
        assert_eq!(launchd.loads.lock().unwrap()[0], plist_path);

        manager.stop("op-n-stop").unwrap();
        let stopped = manager.status().unwrap();
        assert!(!stopped.running);
        assert!(!stopped.ready);
        assert!(
            plist_path.is_file(),
            "stop unloads launchd, it does not delete the plist"
        );
        assert_eq!(
            launchd.unloads.lock().unwrap().as_slice(),
            [crate::native_proxy::LAUNCH_AGENT_LABEL]
        );
        assert_eq!(
            manager.store.load().unwrap().install.unwrap().host_port,
            15722
        );
    }

    #[test]
    fn native_logs_repair_update_rollback_use_launchd_not_docker() {
        let temp = tempfile::tempdir().unwrap();
        let store = AgentStateStore::new(AgentPaths::from_root(temp.path().join("state")));
        store.paths().ensure().unwrap();
        let bin = crate::native_proxy::native_proxy_bin(store.paths());
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"proxy-v1").unwrap();
        let digest_v1 = crate::native_proxy::digest_native_binary(&bin).unwrap();
        let agents = temp.path().join("Library").join("LaunchAgents");
        let launchd = Arc::new(crate::native_proxy::MemoryLaunchd {
            ready: Mutex::new(true),
            stdout_log: Mutex::new("native stdout".into()),
            stderr_log: Mutex::new("native stderr".into()),
            ..crate::native_proxy::MemoryLaunchd::default()
        });
        let manager = ProxyManager::with_native(store, launchd.clone(), agents.clone(), "gui/501");

        manager
            .install("op-n-install", "", Some(digest_v1.clone()))
            .unwrap();
        manager
            .start(ProxyStartRequest {
                operation_id: "op-n-start".into(),
                host_port: Some(15722),
                image: None,
            })
            .unwrap();
        assert_eq!(launchd.loads.lock().unwrap().len(), 1);

        let logs = manager.logs(Some(4096)).unwrap();
        assert_eq!(logs.source, "launchd");
        assert_eq!(logs.stdout, "native stdout");
        assert_eq!(logs.stderr, "native stderr");
        assert_eq!(*launchd.log_reads.lock().unwrap(), 1);

        *launchd.ready.lock().unwrap() = false;
        *launchd.ready_after.lock().unwrap() =
            Some(launchd.probes.load(std::sync::atomic::Ordering::Relaxed) + 2);
        manager.repair("op-n-repair").unwrap();
        assert_eq!(launchd.unloads.lock().unwrap().len(), 1);
        assert_eq!(launchd.loads.lock().unwrap().len(), 2);
        *launchd.ready.lock().unwrap() = true;
        let ready = manager.repair("op-n-repair-noop").unwrap();
        assert_eq!(ready.detail["ready"], true);
        assert_eq!(launchd.loads.lock().unwrap().len(), 2);

        let staged = temp.path().join("proxy-v2");
        std::fs::write(&staged, b"proxy-v2").unwrap();
        let digest_v2 = crate::native_proxy::digest_native_binary(&staged).unwrap();
        manager
            .update(
                "op-n-update",
                staged.to_str().unwrap(),
                Some(digest_v2.clone()),
            )
            .unwrap();
        assert_eq!(std::fs::read(&bin).unwrap(), b"proxy-v2");
        assert_eq!(
            manager
                .store
                .load()
                .unwrap()
                .install
                .unwrap()
                .native_executable_digest
                .as_deref(),
            Some(digest_v2.as_str())
        );
        assert_eq!(launchd.unloads.lock().unwrap().len(), 2);
        assert_eq!(launchd.loads.lock().unwrap().len(), 3);
        assert!(manager
            .update("op-n-update-image", "vellum-proxy:local", None,)
            .unwrap_err()
            .contains("NativeProxyDigestInvalid"));

        manager.rollback("op-n-rollback").unwrap();
        assert_eq!(std::fs::read(&bin).unwrap(), b"proxy-v1");
        assert_eq!(
            manager
                .store
                .load()
                .unwrap()
                .install
                .unwrap()
                .native_executable_digest
                .as_deref(),
            Some(digest_v1.as_str())
        );
        assert_eq!(launchd.unloads.lock().unwrap().len(), 3);
        assert_eq!(launchd.loads.lock().unwrap().len(), 4);
        assert_eq!(manager.status().unwrap().host_port, Some(15722));
        assert_eq!(
            manager.status().unwrap().proxy_backend.as_deref(),
            Some("native")
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
