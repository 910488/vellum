//! Named operations, persisted state, and single-flight.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Mutex};

use super::apply::{wait_kind_for, ApplyDecision};

use super::cache::{recover_switch, stage_bytes, HostDisk};
use super::core_slots::{set_pending, CoreSlot, CoreSlots};
use super::github::{matching_tags, SharedSource};
use super::journal::{Journal, JournalEntry};
use super::machine::{transition, UpdateEvent, UpdatePhase, WaitKind};
use super::manifest::{
    current_arch, current_platform, verify_signed_manifest, AssetRef, UpdateManifest,
};
use super::select::{select_compatible, ReleaseCandidate, SelectContext};
use super::trust::TrustStore;
use super::write_atomic;
use super::{
    HostUpdateStatus, LayerStatus, RemoteUpdatePolicy, UpdateAttention, UpdateComponent,
    UpdateOperation, UpdatePreferences, UpdateProgress, UpdateStatusSnapshot,
};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

#[derive(Clone)]
enum Flight {
    Pending,
    Done(Result<UpdateOperation, String>),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PersistedState {
    preferences: UpdatePreferences,
    remote_policies: Vec<RemoteUpdatePolicy>,
    layers: HashMap<String, LayerRecord>,
    restart_reasons_at_stage: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LayerRecord {
    phase: UpdatePhase,
    current_version: String,
    available_version: Option<String>,
    staged_version: Option<String>,
    operation_id: Option<String>,
    target_version: Option<String>,
    failure_reason: Option<String>,
    release_notes: Option<String>,
    download_bytes: u64,
    download_total: u64,
    staged_asset: Option<PathBuf>,
    manifest: Option<UpdateManifest>,
}

impl LayerRecord {
    fn new(current: impl Into<String>) -> Self {
        Self {
            phase: UpdatePhase::Idle,
            current_version: current.into(),
            available_version: None,
            staged_version: None,
            operation_id: None,
            target_version: None,
            failure_reason: None,
            release_notes: None,
            download_bytes: 0,
            download_total: 0,
            staged_asset: None,
            manifest: None,
        }
    }
}

pub struct RemoteDeployRequest<'a> {
    pub host_id: &'a str,
    pub operation_id: &'a str,
    pub version: &'a str,
    pub sha256: &'a str,
    pub staged: &'a Path,
    pub policy_enabled: bool,
    pub evidence: &'a crate::updates::IdleEvidence,
}

pub trait ApplyExecutor: Send + Sync {
    fn install_desktop(&self, staged: &Path, version: &str) -> Result<(), String>;
    fn deploy_remote(&self, request: RemoteDeployRequest<'_>) -> Result<(), String>;
    fn install_core(&self, staged: &Path, version: &str) -> Result<(), String>;
}

/// Cache/tree helper used when an `ApplyExecutor` is not injected.
/// Desktop apply still has to launch NSIS / replace the macOS app; remote
/// apply requires `AppApplyExecutor` (SSH + host helper).
pub struct FsApplyExecutor {
    pub root: PathBuf,
    pub desktop_runner: std::sync::Arc<dyn super::desktop_install::DesktopRunner>,
    pub current_exe: PathBuf,
}

impl FsApplyExecutor {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            desktop_runner: std::sync::Arc::new(super::desktop_install::HostDesktopRunner),
            current_exe: std::env::current_exe()
                .unwrap_or_else(|_| PathBuf::from("vellum-proxy-desktop")),
        }
    }
}

impl ApplyExecutor for FsApplyExecutor {
    fn install_desktop(&self, staged: &Path, version: &str) -> Result<(), String> {
        super::desktop_install::commit_and_launch_desktop(
            &self.root,
            staged,
            version,
            &self.current_exe,
            self.desktop_runner.as_ref(),
        )
        .map(|_| ())
    }

    fn deploy_remote(&self, _request: RemoteDeployRequest<'_>) -> Result<(), String> {
        Err("signed remote-v* apply requires AppApplyExecutor (SSH host helper)".into())
    }

    fn install_core(&self, staged: &Path, version: &str) -> Result<(), String> {
        let slot = self.root.join("updates").join("core").join(version);
        let candidate = slot.join("candidate");
        std::fs::create_dir_all(&candidate).map_err(|error| error.to_string())?;
        if staged.is_file() {
            let name = staged.file_name().ok_or("staged core asset has no name")?;
            std::fs::copy(staged, candidate.join(name)).map_err(|error| error.to_string())?;
        }
        super::cache::mark_complete(&candidate).map_err(|error| error.to_string())?;
        super::cache::commit_candidate(&slot).map_err(|error| error.to_string())?;
        Ok(())
    }
}

pub struct UpdateEngine {
    root: PathBuf,
    trust: TrustStore,
    source: SharedSource,
    flights: Mutex<HashMap<String, watch::Receiver<Flight>>>,
    executor: std::sync::Arc<dyn ApplyExecutor>,
}

impl UpdateEngine {
    pub fn open(root: &Path, trust: TrustStore, source: SharedSource) -> Self {
        Self::open_with(
            root,
            trust,
            source,
            std::sync::Arc::new(FsApplyExecutor::new(root.to_path_buf())),
        )
    }

    pub fn open_with(
        root: &Path,
        trust: TrustStore,
        source: SharedSource,
        executor: std::sync::Arc<dyn ApplyExecutor>,
    ) -> Self {
        let _ = recover_switch(&root.join("updates"));
        Self {
            root: root.to_path_buf(),
            trust,
            source,
            flights: Mutex::new(HashMap::new()),
            executor,
        }
    }

    fn state_path(&self) -> PathBuf {
        self.root.join("updates").join("state.json")
    }

    fn load(&self) -> PersistedState {
        let Ok(bytes) = std::fs::read(self.state_path()) else {
            return PersistedState::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    fn save(&self, state: &PersistedState) -> AppResult<()> {
        let bytes = serde_json::to_vec_pretty(state)
            .map_err(|error| AppError::Message(error.to_string()))?;
        write_atomic(&self.state_path(), &bytes)
            .map_err(|error| AppError::Message(error.to_string()))
    }

    fn layer_mut<'a>(
        state: &'a mut PersistedState,
        component: UpdateComponent,
        current: &str,
    ) -> &'a mut LayerRecord {
        state
            .layers
            .entry(component.as_str().to_string())
            .or_insert_with(|| LayerRecord::new(current))
    }

    pub fn snapshot(
        &self,
        current_desktop: &str,
        current_remote: &str,
        current_core: &str,
    ) -> UpdateStatusSnapshot {
        let state = self.load();
        let live = self.trust.live_enabled();
        let desktop = self.layer_status(&state, UpdateComponent::Desktop, current_desktop, live);
        let remote = self.layer_status(&state, UpdateComponent::Remote, current_remote, live);
        // The core archive contract does not yet identify and hash the
        // extracted executable/helpers. Keep this layer fail-closed until the
        // signed manifest can bind the runnable tree rather than only its archive.
        let core = self.layer_status(&state, UpdateComponent::Core, current_core, false);
        let attention = attention_from([&desktop, &remote, &core]);
        UpdateStatusSnapshot {
            desktop,
            remote,
            core,
            preferences: state.preferences,
            live_auto_update: live,
            attention,
        }
    }

    fn layer_status(
        &self,
        state: &PersistedState,
        component: UpdateComponent,
        current: &str,
        live: bool,
    ) -> LayerStatus {
        let record = state
            .layers
            .get(component.as_str())
            .cloned()
            .unwrap_or_else(|| LayerRecord::new(current));
        let hosts = if component == UpdateComponent::Remote {
            state
                .remote_policies
                .iter()
                .map(|policy| HostUpdateStatus {
                    host_id: policy.host_id.clone(),
                    phase: record.phase,
                    current_version: Some(current.to_string()),
                    staged_version: record.staged_version.clone(),
                    idle_auto_update: policy.idle_auto_update,
                    failure_reason: record.failure_reason.clone(),
                })
                .collect()
        } else {
            Vec::new()
        };
        LayerStatus {
            component,
            current_version: current.to_string(),
            available_version: record.available_version,
            staged_version: record.staged_version,
            channel: state.preferences.channel,
            phase: record.phase,
            apply_condition: apply_condition(component, record.phase),
            release_notes: record.release_notes,
            failure_reason: record.failure_reason,
            operation_id: record.operation_id,
            target_version: record.target_version,
            download_bytes: record.download_bytes,
            download_total: record.download_total,
            live_auto_update: live,
            hosts,
        }
    }

    async fn single_flight(
        &self,
        key: String,
        work: impl std::future::Future<Output = AppResult<UpdateOperation>>,
    ) -> AppResult<UpdateOperation> {
        let (tx, already) = {
            let mut flights = self.flights.lock().await;
            if let Some(rx) = flights.get(&key) {
                (None, Some(rx.clone()))
            } else {
                let (tx, rx) = watch::channel(Flight::Pending);
                flights.insert(key.clone(), rx);
                (Some(tx), None)
            }
        };
        if let Some(mut rx) = already {
            loop {
                let snapshot = rx.borrow().clone();
                match snapshot {
                    Flight::Done(Ok(op)) => return Ok(op),
                    Flight::Done(Err(error)) => return Err(AppError::Message(error)),
                    Flight::Pending => {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
        let tx = tx.expect("new flight sender");
        let result = work.await;
        let encoded = match &result {
            Ok(op) => Flight::Done(Ok(op.clone())),
            Err(error) => Flight::Done(Err(error.to_string())),
        };
        let _ = tx.send(encoded);
        self.flights.lock().await.remove(&key);
        result
    }

    pub async fn check(
        &self,
        component: Option<UpdateComponent>,
        currents: Currents<'_>,
    ) -> AppResult<UpdateOperation> {
        let key = format!("check:{}", component.map(|c| c.as_str()).unwrap_or("all"));
        self.single_flight(key, self.check_inner(component, currents))
            .await
    }

    async fn check_inner(
        &self,
        component: Option<UpdateComponent>,
        currents: Currents<'_>,
    ) -> AppResult<UpdateOperation> {
        let op = new_operation(
            component.unwrap_or(UpdateComponent::Desktop),
            UpdatePhase::Checking,
        );
        let mut state = self.load();
        let components = match component {
            Some(one) => vec![one],
            None => vec![UpdateComponent::Desktop, UpdateComponent::Remote],
        };
        for item in components {
            let current = currents.of(item);
            let record = Self::layer_mut(&mut state, item, current);
            record.phase =
                transition(record.phase, &UpdateEvent::StartCheck).unwrap_or(UpdatePhase::Checking);
            record.operation_id = Some(op.operation_id.clone());
            let listed = self
                .source
                .list_releases(item.github_repo(), None)
                .map_err(AppError::Message)?;
            if listed.rate_limited {
                record.failure_reason = Some("githubRateLimited".into());
                record.phase = UpdatePhase::Blocked;
                continue;
            }
            if !self.trust.live_enabled() && listed.releases.is_empty() {
                record.phase = UpdatePhase::Idle;
                record.failure_reason = None;
                continue;
            }
            let mut candidates = Vec::new();
            for release in matching_tags(&listed.releases, item) {
                let Some(manifest_asset) = release
                    .assets
                    .iter()
                    .find(|asset| asset.name == "update-manifest.json")
                else {
                    continue;
                };
                let Some(sig_asset) = release
                    .assets
                    .iter()
                    .find(|asset| asset.name == "update-manifest.json.sig")
                else {
                    continue;
                };
                let raw = self
                    .source
                    .fetch_bytes(&manifest_asset.browser_download_url)
                    .map_err(AppError::Message)?;
                let sig = self
                    .source
                    .fetch_bytes(&sig_asset.browser_download_url)
                    .map_err(AppError::Message)?;
                match verify_signed_manifest(&raw, &sig, &self.trust) {
                    Ok(manifest) => candidates.push(ReleaseCandidate {
                        manifest,
                        github_prerelease: release.prerelease,
                        published_at: release.published_at.clone(),
                    }),
                    Err(error) => {
                        record.failure_reason = Some(error.to_string());
                        record.phase = UpdatePhase::Failed;
                    }
                }
            }
            let ctx = SelectContext {
                component: item,
                channel: state.preferences.channel,
                current_version: current,
                platform: current_platform(),
                arch: current_arch(),
                installed_desktop: currents.desktop,
                bridge_api: currents.bridge_api,
                remote_protocol: currents.remote_protocol,
            };
            if let Some(picked) = select_compatible(&candidates, &ctx) {
                let notes = candidates
                    .iter()
                    .find(|c| c.manifest.version == picked.version)
                    .and_then(|c| c.manifest.release_notes.clone());
                let manifest = candidates
                    .into_iter()
                    .find(|c| c.manifest.version == picked.version)
                    .map(|c| c.manifest);
                let record = Self::layer_mut(&mut state, item, current);
                record.available_version = Some(picked.version.clone());
                record.target_version = Some(picked.version);
                record.release_notes = notes;
                record.manifest = manifest;
                record.phase = transition(record.phase, &UpdateEvent::FoundAvailable)
                    .unwrap_or(UpdatePhase::Available);
                record.failure_reason = None;
            } else {
                let record = Self::layer_mut(&mut state, item, current);
                record.phase = transition(record.phase, &UpdateEvent::NoneAvailable)
                    .unwrap_or(UpdatePhase::Idle);
            }
        }
        self.save(&state)?;
        Ok(op)
    }

    pub async fn download(
        &self,
        component: UpdateComponent,
        currents: Currents<'_>,
        restart_reasons: Vec<String>,
    ) -> AppResult<UpdateOperation> {
        let key = format!("download:{}", component.as_str());
        self.single_flight(
            key,
            self.download_inner(component, currents, restart_reasons),
        )
        .await
    }

    async fn download_inner(
        &self,
        component: UpdateComponent,
        currents: Currents<'_>,
        restart_reasons: Vec<String>,
    ) -> AppResult<UpdateOperation> {
        tokio::task::yield_now().await;
        let op = new_operation(component, UpdatePhase::Downloading);
        let mut state = self.load();
        // Preserve restart reasons across the download; never clear them here.
        state.restart_reasons_at_stage = restart_reasons;
        let current = currents.of(component).to_string();
        let record = Self::layer_mut(&mut state, component, &current);
        record.phase = transition(record.phase, &UpdateEvent::StartDownload)
            .unwrap_or(UpdatePhase::Downloading);
        record.operation_id = Some(op.operation_id.clone());
        let manifest = record.manifest.clone();
        if manifest.is_none() {
            record.phase = UpdatePhase::Failed;
            record.failure_reason = Some("missingManifest".into());
        }
        let Some(manifest) = manifest else {
            self.save(&state)?;
            return Ok(op);
        };
        let asset = pick_asset(&manifest).cloned();
        if asset.is_none() {
            record.phase = UpdatePhase::Failed;
            record.failure_reason = Some("missingAsset".into());
        }
        let Some(asset) = asset else {
            self.save(&state)?;
            return Ok(op);
        };
        record.download_total = asset.size;
        let url = format!("asset://{}", asset.name);
        let bytes = self
            .source
            .fetch_bytes(&url)
            .or_else(|_| {
                self.source.fetch_bytes(&format!(
                    "https://github.com/{}/releases/download/{}/{}",
                    component.github_repo(),
                    manifest.release_tag,
                    asset.name
                ))
            })
            .map_err(AppError::Message)?;
        record.download_bytes = bytes.len() as u64;
        record.phase =
            transition(record.phase, &UpdateEvent::StartVerify).unwrap_or(UpdatePhase::Verifying);
        let dest = super::cache::component_cache(&self.root, component.as_str(), &manifest.version)
            .join(&asset.name);
        let staged = stage_bytes(&dest, &bytes, &asset.sha256, asset.size, &HostDisk);
        match staged {
            Ok(path) => {
                let _ = super::manifest::verify_asset_file(&path, &asset.sha256);
                record.staged_asset = Some(path.clone());
                record.staged_version = Some(manifest.version.clone());
                let wait = wait_kind_for(component);
                record.phase =
                    transition(record.phase, &UpdateEvent::Staged { wait }).unwrap_or(match wait {
                        WaitKind::Idle => UpdatePhase::WaitingForIdle,
                        WaitKind::Restart => UpdatePhase::WaitingForRestart,
                    });
                if component == UpdateComponent::Core {
                    let previous_launch = CoreSlots::load(&self.root)
                        .active
                        .and_then(|slot| slot.launch_id);
                    let _ = set_pending(
                        &self.root,
                        CoreSlot {
                            version: manifest.version.clone(),
                            digest: asset.sha256.clone(),
                            protocol: manifest
                                .core
                                .as_ref()
                                .map(|core| core.protocol_schema_sha256.clone())
                                .unwrap_or_default(),
                            path,
                            helpers: manifest
                                .core
                                .as_ref()
                                .map(|core| core.helpers.iter().map(PathBuf::from).collect())
                                .unwrap_or_default(),
                            launch_id: None,
                        },
                    );
                    debug_assert_eq!(
                        CoreSlots::load(&self.root)
                            .active
                            .and_then(|slot| slot.launch_id),
                        previous_launch
                    );
                }
            }
            Err(error) => {
                record.phase = UpdatePhase::Failed;
                record.failure_reason = Some(error.to_string());
            }
        }
        let phase = record.phase;
        let reason = record.failure_reason.clone();
        self.save(&state)?;
        self.record_journal(&op, phase, reason)?;
        Ok(op)
    }

    pub fn cancel(&self, component: UpdateComponent) -> AppResult<UpdateOperation> {
        let mut state = self.load();
        let current = state
            .layers
            .get(component.as_str())
            .map(|layer| layer.current_version.clone())
            .unwrap_or_else(|| "0.0.0".into());
        let record = Self::layer_mut(&mut state, component, &current);
        record.phase =
            transition(record.phase, &UpdateEvent::Cancel).unwrap_or(UpdatePhase::Available);
        let op = UpdateOperation {
            operation_id: record
                .operation_id
                .clone()
                .unwrap_or_else(|| ulid::Ulid::new().to_string()),
            component,
            phase: record.phase,
            target_version: record.target_version.clone(),
        };
        self.save(&state)?;
        Ok(op)
    }

    pub fn set_prefs(&self, preferences: UpdatePreferences) -> AppResult<UpdatePreferences> {
        let mut state = self.load();
        state.preferences = preferences.clone();
        self.save(&state)?;
        Ok(preferences)
    }

    pub fn set_host_policy(&self, policy: RemoteUpdatePolicy) -> AppResult<RemoteUpdatePolicy> {
        let mut state = self.load();
        if let Some(existing) = state
            .remote_policies
            .iter_mut()
            .find(|item| item.host_id == policy.host_id)
        {
            *existing = policy.clone();
        } else {
            state.remote_policies.push(policy.clone());
        }
        self.save(&state)?;
        Ok(policy)
    }

    pub fn apply(
        &self,
        component: UpdateComponent,
        decision: ApplyDecision,
        currents: Currents<'_>,
        host_id: Option<&str>,
        evidence: Option<&crate::updates::IdleEvidence>,
    ) -> AppResult<UpdateOperation> {
        let mut state = self.load();
        let current = currents.of(component).to_string();
        let record = Self::layer_mut(&mut state, component, &current);
        let op = UpdateOperation {
            operation_id: record
                .operation_id
                .clone()
                .unwrap_or_else(|| ulid::Ulid::new().to_string()),
            component,
            phase: record.phase,
            target_version: record.target_version.clone(),
        };
        match decision {
            ApplyDecision::Allow => {
                match transition(record.phase, &UpdateEvent::StartApply) {
                    Ok(next) => record.phase = next,
                    Err(_) => {
                        let phase = record.phase;
                        return Ok(UpdateOperation { phase, ..op });
                    }
                }
                let staged = record.staged_asset.clone();
                let version = record
                    .staged_version
                    .clone()
                    .unwrap_or_else(|| current.clone());
                let sha = record
                    .manifest
                    .as_ref()
                    .and_then(pick_asset)
                    .map(|asset| asset.sha256.clone())
                    .unwrap_or_default();
                let Some(staged) = staged else {
                    record.phase = UpdatePhase::Failed;
                    record.failure_reason = Some("nothingStaged".into());
                    let phase = record.phase;
                    let reason = record.failure_reason.clone();
                    self.save(&state)?;
                    self.record_journal(&op, phase, reason)?;
                    return Ok(UpdateOperation { phase, ..op });
                };
                let installed = match component {
                    UpdateComponent::Desktop => self.executor.install_desktop(&staged, &version),
                    UpdateComponent::Remote => self.executor.deploy_remote(RemoteDeployRequest {
                        host_id: host_id.unwrap_or(""),
                        operation_id: &op.operation_id,
                        version: &version,
                        sha256: &sha,
                        staged: &staged,
                        policy_enabled: true,
                        evidence: evidence.ok_or_else(|| {
                            AppError::Message("remote apply requires live idle evidence".into())
                        })?,
                    }),
                    UpdateComponent::Core => self.executor.install_core(&staged, &version),
                };
                match installed {
                    Ok(()) => {
                        record.phase = transition(record.phase, &UpdateEvent::StartValidate)
                            .unwrap_or(UpdatePhase::Validating);
                        record.phase = transition(record.phase, &UpdateEvent::Succeeded)
                            .unwrap_or(UpdatePhase::Applied);
                        record.current_version = version;
                        record.failure_reason = None;
                    }
                    Err(error) => {
                        record.phase = UpdatePhase::Failed;
                        record.failure_reason = Some(error);
                    }
                }
            }
            ApplyDecision::Wait { .. } => {
                // Downloaded/pending stay staged. A wait is not applied and
                // must not invoke the installer.
            }
            ApplyDecision::Refuse { reason } => {
                record.phase = transition(record.phase, &UpdateEvent::Block { recoverable: true })
                    .unwrap_or(UpdatePhase::Blocked);
                record.failure_reason = Some(reason);
            }
        }
        let phase = record.phase;
        let reason = record.failure_reason.clone();
        self.save(&state)?;
        self.record_journal(&op, phase, reason)?;
        Ok(UpdateOperation { phase, ..op })
    }

    pub fn rollback(&self, component: UpdateComponent) -> AppResult<UpdateOperation> {
        let mut state = self.load();
        let current = state
            .layers
            .get(component.as_str())
            .map(|layer| layer.current_version.clone())
            .unwrap_or_else(|| "0.0.0".into());
        let record = Self::layer_mut(&mut state, component, &current);
        record.phase = UpdatePhase::RolledBack;
        if component == UpdateComponent::Core {
            let _ = super::core_slots::rollback_to_previous(&self.root);
        }
        let op = new_operation(component, UpdatePhase::RolledBack);
        record.operation_id = Some(op.operation_id.clone());
        self.save(&state)?;
        Ok(op)
    }

    fn record_journal(
        &self,
        op: &UpdateOperation,
        phase: UpdatePhase,
        reason: Option<String>,
    ) -> AppResult<()> {
        let mut journal = Journal::load(&self.root);
        journal.upsert(JournalEntry {
            operation_id: op.operation_id.clone(),
            component: op.component,
            phase,
            target_version: op.target_version.clone(),
            reason,
            host_id: None,
            created_at: now_secs(),
            updated_at: now_secs(),
        });
        journal
            .save(&self.root)
            .map_err(|error| AppError::Message(error.to_string()))
    }
}

#[derive(Clone, Copy)]
pub struct Currents<'a> {
    pub desktop: &'a str,
    pub remote: &'a str,
    pub core: &'a str,
    pub bridge_api: &'a str,
    pub remote_protocol: &'a str,
}

impl<'a> Currents<'a> {
    fn of(self, component: UpdateComponent) -> &'a str {
        match component {
            UpdateComponent::Desktop => self.desktop,
            UpdateComponent::Remote => self.remote,
            UpdateComponent::Core => self.core,
        }
    }
}

fn pick_asset(manifest: &UpdateManifest) -> Option<&AssetRef> {
    manifest
        .assets
        .iter()
        .find(|asset| asset.platform == current_platform() && asset.arch == current_arch())
}

fn apply_condition(component: UpdateComponent, phase: UpdatePhase) -> String {
    match (component, phase) {
        (UpdateComponent::Desktop, UpdatePhase::WaitingForRestart | UpdatePhase::Staged) => {
            "restartVellum".into()
        }
        (UpdateComponent::Remote, UpdatePhase::WaitingForIdle | UpdatePhase::Staged) => {
            "hostIdle".into()
        }
        (UpdateComponent::Core, UpdatePhase::WaitingForRestart | UpdatePhase::Staged) => {
            "nextCoreStart".into()
        }
        (_, UpdatePhase::Failed | UpdatePhase::RolledBack) => "failed".into(),
        (_, UpdatePhase::Available) => "download".into(),
        _ => phase.as_str().into(),
    }
}

fn attention_from(layers: [&LayerStatus; 3]) -> UpdateAttention {
    if layers
        .iter()
        .any(|layer| matches!(layer.phase, UpdatePhase::Failed | UpdatePhase::RolledBack))
    {
        return UpdateAttention::Failed;
    }
    if layers
        .iter()
        .any(|layer| layer.phase == UpdatePhase::WaitingForIdle)
    {
        return UpdateAttention::WaitingIdle;
    }
    if layers
        .iter()
        .any(|layer| layer.phase == UpdatePhase::WaitingForRestart)
    {
        return UpdateAttention::WaitingRestart;
    }
    if layers
        .iter()
        .any(|layer| layer.phase == UpdatePhase::Available)
    {
        return UpdateAttention::Available;
    }
    UpdateAttention::None
}

fn new_operation(component: UpdateComponent, phase: UpdatePhase) -> UpdateOperation {
    UpdateOperation {
        operation_id: ulid::Ulid::new().to_string(),
        component,
        phase,
        target_version: None,
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn get_status(state: &AppState) -> UpdateStatusSnapshot {
    let engine = super::engine(&state.data_root());
    let core_version = current_core_version(state);
    let currents = current_versions(&core_version);
    engine.snapshot(currents.desktop, currents.remote, currents.core)
}

pub async fn check_updates(
    state: &AppState,
    component: Option<UpdateComponent>,
) -> AppResult<UpdateStatusSnapshot> {
    let engine = super::engine(&state.data_root());
    let core_version = current_core_version(state);
    let currents = current_versions(&core_version);
    engine.check(component, currents).await?;
    Ok(engine.snapshot(currents.desktop, currents.remote, currents.core))
}

pub async fn download_update(
    state: &AppState,
    component: UpdateComponent,
    _host_id: Option<String>,
) -> AppResult<UpdateOperation> {
    require_live_component(component)?;
    let engine = super::engine(&state.data_root());
    let core_version = current_core_version(state);
    let currents = current_versions(&core_version);
    let reasons = state
        .runtime_status()
        .restart_reasons
        .iter()
        .map(|notice| notice.code.clone())
        .collect();
    engine.download(component, currents, reasons).await
}

pub fn cancel_download(state: &AppState, component: UpdateComponent) -> AppResult<UpdateOperation> {
    super::engine(&state.data_root()).cancel(component)
}

pub fn set_preferences(
    state: &AppState,
    preferences: UpdatePreferences,
) -> AppResult<UpdatePreferences> {
    super::engine(&state.data_root()).set_prefs(preferences)
}

pub fn set_remote_policy(
    state: &AppState,
    policy: RemoteUpdatePolicy,
) -> AppResult<RemoteUpdatePolicy> {
    super::engine(&state.data_root()).set_host_policy(policy)
}

pub fn apply_update(
    state: &AppState,
    component: UpdateComponent,
    host_id: Option<String>,
) -> AppResult<UpdateOperation> {
    require_live_component(component)?;
    apply_update_with_ui(state, component, host_id, true)
}

fn require_live_component(component: UpdateComponent) -> AppResult<()> {
    if component == UpdateComponent::Core {
        return Err(AppError::Message(
            "Enhanced core update is preview-only until extracted executable and helper hashes are signed"
                .into(),
        ));
    }
    Ok(())
}

/// Called when the Vellum window is closing. Desktop apply is allowed only
/// when the UI is no longer open and Proxy/core are idle.
pub fn apply_staged_on_exit(state: &AppState) -> AppResult<UpdateOperation> {
    apply_update_with_ui(state, UpdateComponent::Desktop, None, false)
}

fn apply_update_with_ui(
    state: &AppState,
    component: UpdateComponent,
    host_id: Option<String>,
    ui_open: bool,
) -> AppResult<UpdateOperation> {
    let engine = UpdateEngine::open_with(
        &state.data_root(),
        crate::updates::TrustStore::bundled(),
        std::sync::Arc::new(crate::updates::github::GithubSource::new()),
        std::sync::Arc::new(AppApplyExecutor {
            state: state.clone(),
            host_id: host_id.clone().unwrap_or_default(),
        }),
    );
    let core_version = current_core_version(state);
    let currents = current_versions(&core_version);
    let evidence = match component {
        UpdateComponent::Remote => host_id
            .as_deref()
            .map(|id| crate::updates::observe_remote_host(state, id))
            .unwrap_or(crate::updates::IdleEvidence {
                unknown_work: true,
                observation_fresh: false,
                ..crate::updates::IdleEvidence::default()
            }),
        _ => crate::updates::live_idle_evidence(state, false),
    };
    let decision = match component {
        UpdateComponent::Desktop => {
            crate::updates::desktop_may_apply(&crate::updates::desktop_apply_input(state, ui_open))
        }
        UpdateComponent::Remote => {
            let enabled = engine
                .load()
                .remote_policies
                .iter()
                .find(|policy| host_id.as_deref() == Some(policy.host_id.as_str()))
                .is_some_and(|policy| policy.idle_auto_update);
            crate::updates::remote_idle_decision(&crate::updates::RemoteIdleInput {
                policy_enabled: enabled,
                evidence: evidence.clone(),
            })
        }
        UpdateComponent::Core => crate::updates::idle_handoff_decision(
            engine.load().preferences.core_idle_handoff,
            &evidence,
        ),
    };
    engine.apply(
        component,
        decision,
        currents,
        host_id.as_deref(),
        Some(&evidence),
    )
}

pub struct AppApplyExecutor {
    pub state: AppState,
    pub host_id: String,
}

impl ApplyExecutor for AppApplyExecutor {
    fn install_desktop(&self, staged: &Path, version: &str) -> Result<(), String> {
        let current_exe =
            std::env::current_exe().map_err(|error| format!("current exe: {error}"))?;
        super::desktop_install::commit_and_launch_desktop(
            &self.state.data_root(),
            staged,
            version,
            &current_exe,
            &super::desktop_install::HostDesktopRunner,
        )
        .map(|_| ())
    }

    fn deploy_remote(&self, request: RemoteDeployRequest<'_>) -> Result<(), String> {
        let host = if request.host_id.is_empty() {
            self.host_id.as_str()
        } else {
            request.host_id
        };
        let expected =
            super::remote_host::expected_identity_from_staged(request.staged, request.version);
        let mut ops = super::remote_host::SshRemoteHostOps::from_state(
            &self.state,
            host,
            request.operation_id,
        )
        .map_err(|error| error.to_string())?;
        ops.expected = expected.clone();
        let mut journal = super::journal::Journal::load(&self.state.data_root());
        let mut backend = super::remote_host::AppRemoteBackend {
            ops,
            staged_local: request.staged.to_path_buf(),
            package_sha256: request.sha256.to_string(),
            uploaded_sha256: None,
            expected,
        };
        let outcome = super::remote_helper::apply_remote_package(
            &self.state.data_root(),
            &mut journal,
            &mut backend,
            &super::remote_helper::RemoteApplyPlan {
                operation_id: request.operation_id.into(),
                host_id: host.into(),
                target_version: request.version.into(),
                package_sha256: request.sha256.into(),
            },
            request.policy_enabled,
        )?;
        match outcome.decision {
            crate::updates::ApplyDecision::Allow => Ok(()),
            crate::updates::ApplyDecision::Wait { reason }
            | crate::updates::ApplyDecision::Refuse { reason } => Err(reason),
        }
    }

    fn install_core(&self, staged: &Path, version: &str) -> Result<(), String> {
        FsApplyExecutor::new(self.state.data_root()).install_core(staged, version)
    }
}

pub fn rollback_update(state: &AppState, component: UpdateComponent) -> AppResult<UpdateOperation> {
    super::engine(&state.data_root()).rollback(component)
}

fn current_versions(core: &str) -> Currents<'_> {
    Currents {
        desktop: env!("CARGO_PKG_VERSION"),
        remote: env!("CARGO_PKG_VERSION"),
        core,
        bridge_api: "1.0.0",
        remote_protocol: "3.0.0",
    }
}

fn current_core_version(state: &AppState) -> String {
    CoreSlots::load(&state.data_root())
        .active
        .map(|slot| slot.version)
        .filter(|version| super::manifest::parse_version(version).is_some())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

pub fn progress_from(op: &UpdateOperation) -> UpdateProgress {
    UpdateProgress {
        operation_id: op.operation_id.clone(),
        component: op.component,
        phase: op.phase,
        bytes: 0,
        total: 0,
        target_version: op.target_version.clone(),
        message: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updates::github::{GithubAsset, GithubRelease, ListedReleases, MemorySource};
    use crate::updates::manifest::{
        sign_raw, AssetRef, CompatRange, DataFormat, MANIFEST_SCHEMA_VERSION,
    };
    use crate::updates::TrustStore;
    use ed25519_dalek::SigningKey;
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn signed_fixture(
        signing: &SigningKey,
        component: UpdateComponent,
        version: &str,
        bytes: &[u8],
    ) -> (Vec<u8>, Vec<u8>, UpdateManifest) {
        let hash = hex::encode(Sha256::digest(bytes));
        let manifest = UpdateManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            component,
            version: version.into(),
            source_commit: "c".into(),
            release_tag: format!("{}{version}", component.tag_prefix()),
            sequence: 4,
            key_id: "test-key".into(),
            prerelease: false,
            release_notes: Some("fixture notes".into()),
            min_desktop_version: "0.1.0".into(),
            bridge_api_compat: CompatRange::new("*"),
            remote_protocol_compat: CompatRange::new("*"),
            data_format: DataFormat::default(),
            assets: vec![AssetRef {
                platform: current_platform().into(),
                arch: current_arch().into(),
                name: "payload.bin".into(),
                size: bytes.len() as u64,
                sha256: hash,
            }],
            core: None,
        };
        let raw = serde_json::to_vec(&manifest).unwrap();
        let sig = sign_raw(&raw, signing).to_vec();
        (raw, sig, manifest)
    }

    #[tokio::test]
    async fn download_does_not_clear_restart_reasons_or_active_core() {
        let dir = tempfile::tempdir().unwrap();
        let signing = SigningKey::from_bytes(&[3u8; 32]);
        let trust = TrustStore::from_key("test-key", signing.verifying_key());
        let payload = b"core-bytes";
        let (raw, sig, _) = signed_fixture(&signing, UpdateComponent::Core, "0.2.0", payload);
        let mut releases = HashMap::new();
        releases.insert("https://example/manifest".into(), raw);
        releases.insert("https://example/sig".into(), sig);
        releases.insert("asset://payload.bin".into(), payload.to_vec());
        let listed = ListedReleases {
            etag: None,
            not_modified: false,
            rate_limited: false,
            retry_after_secs: 0,
            releases: vec![GithubRelease {
                tag_name: "core-v0.2.0".into(),
                prerelease: false,
                draft: false,
                published_at: None,
                assets: vec![
                    GithubAsset {
                        name: "update-manifest.json".into(),
                        size: 1,
                        browser_download_url: "https://example/manifest".into(),
                    },
                    GithubAsset {
                        name: "update-manifest.json.sig".into(),
                        size: 64,
                        browser_download_url: "https://example/sig".into(),
                    },
                ],
            }],
        };
        let engine = UpdateEngine::open(
            dir.path(),
            trust,
            Arc::new(MemorySource { releases, listed }),
        );
        let slots = CoreSlots {
            active: Some(CoreSlot {
                version: "bundled".into(),
                digest: "sha256:old".into(),
                protocol: "p".into(),
                path: PathBuf::from("old"),
                helpers: Vec::new(),
                launch_id: Some("launch-keep".into()),
            }),
            ..CoreSlots::default()
        };
        slots.save(dir.path()).unwrap();
        let currents = Currents {
            desktop: "0.2.9",
            remote: "0.2.9",
            core: "bundled",
            bridge_api: "1.0.0",
            remote_protocol: "3.0.0",
        };
        engine
            .check(Some(UpdateComponent::Core), currents)
            .await
            .unwrap();
        engine
            .download(
                UpdateComponent::Core,
                currents,
                vec!["catalogRestored".into()],
            )
            .await
            .unwrap();
        let persisted = engine.load();
        assert_eq!(
            persisted.restart_reasons_at_stage,
            vec!["catalogRestored".to_string()]
        );
        let slots = CoreSlots::load(dir.path());
        assert_eq!(
            slots.active.unwrap().launch_id.as_deref(),
            Some("launch-keep")
        );
        assert_eq!(slots.pending.unwrap().version, "0.2.0");
        let snap = engine.snapshot("0.2.9", "0.2.9", "bundled");
        assert_eq!(snap.core.phase, UpdatePhase::WaitingForRestart);
        assert_eq!(snap.core.current_version, "bundled");
        assert_eq!(snap.core.staged_version.as_deref(), Some("0.2.0"));
        assert_eq!(snap.core.available_version.as_deref(), Some("0.2.0"));
    }

    #[tokio::test]
    async fn duplicate_download_shares_one_flight() {
        let dir = tempfile::tempdir().unwrap();
        let signing = SigningKey::from_bytes(&[4u8; 32]);
        let trust = TrustStore::from_key("test-key", signing.verifying_key());
        let payload = b"desk";
        let (raw, sig, _) = signed_fixture(&signing, UpdateComponent::Desktop, "0.3.0", payload);
        let mut releases = HashMap::new();
        releases.insert("https://example/manifest".into(), raw);
        releases.insert("https://example/sig".into(), sig);
        releases.insert("asset://payload.bin".into(), payload.to_vec());
        let listed = ListedReleases {
            etag: None,
            not_modified: false,
            rate_limited: false,
            retry_after_secs: 0,
            releases: vec![GithubRelease {
                tag_name: "desktop-v0.3.0".into(),
                prerelease: false,
                draft: false,
                published_at: None,
                assets: vec![
                    GithubAsset {
                        name: "update-manifest.json".into(),
                        size: 1,
                        browser_download_url: "https://example/manifest".into(),
                    },
                    GithubAsset {
                        name: "update-manifest.json.sig".into(),
                        size: 64,
                        browser_download_url: "https://example/sig".into(),
                    },
                ],
            }],
        };
        let engine = Arc::new(UpdateEngine::open(
            dir.path(),
            trust,
            Arc::new(MemorySource { releases, listed }),
        ));
        let currents = Currents {
            desktop: "0.2.9",
            remote: "0.2.9",
            core: "bundled",
            bridge_api: "1.0.0",
            remote_protocol: "3.0.0",
        };
        engine
            .check(Some(UpdateComponent::Desktop), currents)
            .await
            .unwrap();
        let a = engine.download(UpdateComponent::Desktop, currents, Vec::new());
        let b = engine.download(UpdateComponent::Desktop, currents, Vec::new());
        let (first, second) = tokio::join!(a, b);
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(first.operation_id, second.operation_id);
    }

    struct RecordingExecutor {
        desktop: std::sync::Mutex<Vec<String>>,
        remote: std::sync::Mutex<Vec<String>>,
        core: std::sync::Mutex<Vec<String>>,
        fail: bool,
    }

    impl Default for RecordingExecutor {
        fn default() -> Self {
            Self {
                desktop: std::sync::Mutex::new(Vec::new()),
                remote: std::sync::Mutex::new(Vec::new()),
                core: std::sync::Mutex::new(Vec::new()),
                fail: false,
            }
        }
    }

    impl ApplyExecutor for RecordingExecutor {
        fn install_desktop(&self, staged: &Path, version: &str) -> Result<(), String> {
            self.desktop
                .lock()
                .expect("desktop recorder")
                .push(format!("{}:{version}", staged.display()));
            if self.fail {
                Err("install skipped".into())
            } else {
                Ok(())
            }
        }

        fn deploy_remote(&self, request: RemoteDeployRequest<'_>) -> Result<(), String> {
            self.remote.lock().expect("remote recorder").push(format!(
                "{}:{}:{}",
                request.host_id, request.operation_id, request.version
            ));
            if self.fail {
                Err("deploy skipped".into())
            } else {
                Ok(())
            }
        }

        fn install_core(&self, staged: &Path, version: &str) -> Result<(), String> {
            self.core
                .lock()
                .expect("core recorder")
                .push(format!("{}:{version}", staged.display()));
            if self.fail {
                Err("install skipped".into())
            } else {
                Ok(())
            }
        }
    }

    fn fixture_engine(
        dir: &Path,
        component: UpdateComponent,
        version: &str,
        payload: &[u8],
        executor: Arc<dyn ApplyExecutor>,
    ) -> (UpdateEngine, Currents<'static>) {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let trust = TrustStore::from_key("test-key", signing.verifying_key());
        let (raw, sig, _) = signed_fixture(&signing, component, version, payload);
        let mut releases = HashMap::new();
        releases.insert("https://example/manifest".into(), raw);
        releases.insert("https://example/sig".into(), sig);
        releases.insert("asset://payload.bin".into(), payload.to_vec());
        let listed = ListedReleases {
            etag: None,
            not_modified: false,
            rate_limited: false,
            retry_after_secs: 0,
            releases: vec![GithubRelease {
                tag_name: format!("{}{version}", component.tag_prefix()),
                prerelease: false,
                draft: false,
                published_at: None,
                assets: vec![
                    GithubAsset {
                        name: "update-manifest.json".into(),
                        size: 1,
                        browser_download_url: "https://example/manifest".into(),
                    },
                    GithubAsset {
                        name: "update-manifest.json.sig".into(),
                        size: 64,
                        browser_download_url: "https://example/sig".into(),
                    },
                ],
            }],
        };
        let engine = UpdateEngine::open_with(
            dir,
            trust,
            Arc::new(MemorySource { releases, listed }),
            executor,
        );
        let currents = Currents {
            desktop: "0.2.9",
            remote: "0.2.9",
            core: "bundled",
            bridge_api: "1.0.0",
            remote_protocol: "3.0.0",
        };
        (engine, currents)
    }

    #[tokio::test]
    async fn apply_wait_does_not_install_and_allow_must_call_executor() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Arc::new(RecordingExecutor::default());
        let (engine, currents) = fixture_engine(
            dir.path(),
            UpdateComponent::Desktop,
            "0.3.1",
            b"desk-payload",
            recorder.clone(),
        );
        engine
            .check(Some(UpdateComponent::Desktop), currents)
            .await
            .unwrap();
        engine
            .download(UpdateComponent::Desktop, currents, Vec::new())
            .await
            .unwrap();
        let waiting = engine
            .apply(
                UpdateComponent::Desktop,
                ApplyDecision::Wait {
                    reason: "waitingForRestart".into(),
                },
                currents,
                None,
                None,
            )
            .unwrap();
        assert_eq!(waiting.phase, UpdatePhase::WaitingForRestart);
        assert!(recorder.desktop.lock().unwrap().is_empty());
        assert_ne!(waiting.phase, UpdatePhase::Applied);

        let applied = engine
            .apply(
                UpdateComponent::Desktop,
                ApplyDecision::Allow,
                currents,
                None,
                None,
            )
            .unwrap();
        assert_eq!(applied.phase, UpdatePhase::Applied);
        assert!(
            !recorder.desktop.lock().unwrap().is_empty(),
            "Allow must invoke install_desktop; a phase-only flip is a bug"
        );
        let snap = engine.snapshot("0.3.1", "0.2.9", "bundled");
        assert_eq!(snap.desktop.phase, UpdatePhase::Applied);
        assert_eq!(snap.desktop.current_version, "0.3.1");
        assert_eq!(snap.desktop.staged_version.as_deref(), Some("0.3.1"));
    }

    #[tokio::test]
    async fn apply_allow_fails_when_executor_skips_install() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Arc::new(RecordingExecutor {
            fail: true,
            ..RecordingExecutor::default()
        });
        let (engine, currents) = fixture_engine(
            dir.path(),
            UpdateComponent::Core,
            "0.4.0",
            b"core-payload",
            recorder.clone(),
        );
        engine
            .check(Some(UpdateComponent::Core), currents)
            .await
            .unwrap();
        engine
            .download(UpdateComponent::Core, currents, Vec::new())
            .await
            .unwrap();
        let before = CoreSlots::load(dir.path());
        assert_eq!(
            before
                .active
                .as_ref()
                .and_then(|slot| slot.launch_id.as_deref()),
            None
        );
        assert_eq!(before.pending.as_ref().unwrap().version, "0.4.0");
        let op = engine
            .apply(
                UpdateComponent::Core,
                ApplyDecision::Allow,
                currents,
                None,
                None,
            )
            .unwrap();
        assert_eq!(op.phase, UpdatePhase::Failed);
        assert!(
            !recorder.core.lock().unwrap().is_empty(),
            "failed apply still has to attempt install_core"
        );
        let after = CoreSlots::load(dir.path());
        assert_eq!(after.pending.unwrap().version, "0.4.0");
        assert!(after.active.is_none());
    }

    #[tokio::test]
    async fn fs_apply_commits_candidate_tree_on_allow() {
        let dir = tempfile::tempdir().unwrap();
        type SpawnLog = Vec<(PathBuf, Vec<String>)>;
        let spawns = Arc::new(std::sync::Mutex::new(SpawnLog::new()));
        struct SpawnRecorder {
            inner: Arc<std::sync::Mutex<SpawnLog>>,
        }
        impl super::super::desktop_install::DesktopRunner for SpawnRecorder {
            fn spawn_detached(&self, program: &Path, args: &[String]) -> Result<u32, String> {
                self.inner
                    .lock()
                    .expect("spawns")
                    .push((program.to_path_buf(), args.to_vec()));
                Ok(11)
            }
            fn extract_tar_gz(&self, _archive: &Path, _dest_app: &Path) -> Result<(), String> {
                Err("unexpected mac extract".into())
            }
        }
        let signing = SigningKey::from_bytes(&[9u8; 32]);
        let trust = TrustStore::from_key("test-key", signing.verifying_key());
        let payload = b"nsis-bytes";
        let hash = hex::encode(Sha256::digest(payload));
        let manifest = UpdateManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            component: UpdateComponent::Desktop,
            version: "0.5.0".into(),
            source_commit: "c".into(),
            release_tag: "desktop-v0.5.0".into(),
            sequence: 4,
            key_id: "test-key".into(),
            prerelease: false,
            release_notes: Some("fixture notes".into()),
            min_desktop_version: "0.1.0".into(),
            bridge_api_compat: CompatRange::new("*"),
            remote_protocol_compat: CompatRange::new("*"),
            data_format: crate::updates::manifest::DataFormat::default(),
            assets: vec![AssetRef {
                platform: current_platform().into(),
                arch: current_arch().into(),
                name: "Vellum_0.5.0_x64-setup.exe".into(),
                size: payload.len() as u64,
                sha256: hash,
            }],
            core: None,
        };
        let raw = serde_json::to_vec(&manifest).unwrap();
        let sig = sign_raw(&raw, &signing).to_vec();
        let mut releases = HashMap::new();
        releases.insert("https://example/manifest".into(), raw);
        releases.insert("https://example/sig".into(), sig);
        releases.insert(
            "asset://Vellum_0.5.0_x64-setup.exe".into(),
            payload.to_vec(),
        );
        let listed = ListedReleases {
            etag: None,
            not_modified: false,
            rate_limited: false,
            retry_after_secs: 0,
            releases: vec![GithubRelease {
                tag_name: "desktop-v0.5.0".into(),
                prerelease: false,
                draft: false,
                published_at: None,
                assets: vec![
                    GithubAsset {
                        name: "update-manifest.json".into(),
                        size: 1,
                        browser_download_url: "https://example/manifest".into(),
                    },
                    GithubAsset {
                        name: "update-manifest.json.sig".into(),
                        size: 64,
                        browser_download_url: "https://example/sig".into(),
                    },
                ],
            }],
        };
        let engine = UpdateEngine::open_with(
            dir.path(),
            trust,
            Arc::new(MemorySource { releases, listed }),
            Arc::new(FsApplyExecutor {
                root: dir.path().to_path_buf(),
                desktop_runner: Arc::new(SpawnRecorder {
                    inner: spawns.clone(),
                }),
                current_exe: PathBuf::from("C:/Program Files/Vellum/vellum-proxy-desktop.exe"),
            }),
        );
        let currents = Currents {
            desktop: "0.2.9",
            remote: "0.2.9",
            core: "bundled",
            bridge_api: "1.0.0",
            remote_protocol: "3.0.0",
        };
        engine
            .check(Some(UpdateComponent::Desktop), currents)
            .await
            .unwrap();
        engine
            .download(UpdateComponent::Desktop, currents, Vec::new())
            .await
            .unwrap();
        engine
            .apply(
                UpdateComponent::Desktop,
                ApplyDecision::Allow,
                currents,
                None,
                None,
            )
            .unwrap();
        let current = dir
            .path()
            .join("updates")
            .join("desktop")
            .join("0.5.0")
            .join("current");
        assert!(
            current.join(".complete").exists(),
            "Allow must commit the candidate tree, not only flip the phase"
        );
        let launched = spawns.lock().unwrap();
        assert_eq!(
            launched.len(),
            1,
            "close/restart apply must exec NSIS, not only copy the staged file"
        );
        assert!(launched[0].1.iter().any(|arg| arg == "/S"));
        assert!(launched[0].1.iter().any(|arg| arg == "/UPDATE"));
    }

    #[tokio::test]
    async fn remote_allow_without_live_evidence_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Arc::new(RecordingExecutor::default());
        let (engine, currents) = fixture_engine(
            dir.path(),
            UpdateComponent::Remote,
            "0.6.0",
            b"remote-pkg",
            recorder.clone(),
        );
        engine
            .check(Some(UpdateComponent::Remote), currents)
            .await
            .unwrap();
        engine
            .download(UpdateComponent::Remote, currents, Vec::new())
            .await
            .unwrap();
        let err = engine
            .apply(
                UpdateComponent::Remote,
                ApplyDecision::Allow,
                currents,
                Some("host-a"),
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("live idle evidence"));
        assert!(recorder.remote.lock().unwrap().is_empty());
        let idle = crate::updates::IdleEvidence {
            proxy_idle: true,
            native_sessions_idle: true,
            tools_idle: true,
            approvals_idle: true,
            heartbeat_expired: false,
            observation_fresh: true,
            unknown_work: false,
            open_turns: 0,
            durable_handoff_ready: None,
        };
        let op = engine
            .apply(
                UpdateComponent::Remote,
                ApplyDecision::Allow,
                currents,
                Some("host-a"),
                Some(&idle),
            )
            .unwrap();
        assert_eq!(op.phase, UpdatePhase::Applied);
        assert_eq!(
            recorder.remote.lock().unwrap().clone(),
            vec![format!("host-a:{}:0.6.0", op.operation_id)]
        );
    }
}
