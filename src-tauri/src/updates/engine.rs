//! Named operations, persisted state, and single-flight.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Mutex};

use super::apply::{wait_kind_for, ApplyDecision};

use super::cache::{recover_switch, stage_bytes, HostDisk};
use super::core_slots::{set_pending, CoreSlots};
use super::github::{matching_tags, SharedSource};
use super::journal::{Journal, JournalEntry};
use super::machine::{transition, UpdateEvent, UpdatePhase, WaitKind};
use super::manifest::{
    current_arch, current_platform, select_asset, verify_signed_manifest, AssetTarget,
    UpdateManifest,
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
    /// Unix seconds of the last check that reached GitHub.
    #[serde(default)]
    last_checked_at: Option<i64>,
    #[serde(default)]
    release_indexes: HashMap<String, ReleaseIndex>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReleaseIndex {
    etag: Option<String>,
    releases: Vec<super::github::GithubRelease>,
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
    #[serde(default)]
    signed_manifest_path: Option<PathBuf>,
    #[serde(default)]
    signed_signature_path: Option<PathBuf>,
    /// Platform the staged asset was picked for. Remote stages one host's
    /// package, so another host must not be given it.
    #[serde(default)]
    staged_target: Option<AssetTarget>,
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
            signed_manifest_path: None,
            signed_signature_path: None,
            staged_target: None,
        }
    }
}

pub struct RemoteDeployRequest<'a> {
    pub host_id: &'a str,
    pub operation_id: &'a str,
    pub version: &'a str,
    pub sha256: &'a str,
    pub staged: &'a Path,
    /// Platform the staged package was built for; `None` for state written
    /// before targets were recorded, which the executor must not deploy.
    pub staged_target: Option<&'a AssetTarget>,
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
        let _ = staged;
        CoreSlots::load(&self.root)
            .pending
            .filter(|slot| slot.version == version)
            .ok_or_else(|| "verified core candidate is not pending".to_string())
            .map(|_| ())
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

    /// Whether what a waiting layer staged is still in place: the pending core
    /// slot for Core, the downloaded asset for the others.
    fn still_staged(
        &self,
        component: UpdateComponent,
        record: &LayerRecord,
        version: &str,
    ) -> bool {
        match component {
            UpdateComponent::Core => {
                super::pending_core(&self.root).is_some_and(|slot| slot.version == version)
            }
            _ => record.staged_asset.as_deref().is_some_and(Path::is_file),
        }
    }

    /// Removes core trees, archives, and installers that nothing refers to.
    ///
    /// Nothing else ever deleted them: each preview left a 416 MB core tree
    /// (twice over when it was re-staged) plus its archives, ~6 GB within a
    /// few days, until a Mac ran out of disk mid-download. Only versions
    /// older than the newest one still referenced go, so a download that
    /// started after the state was read is never touched.
    pub fn prune_retired_versions(&self) {
        if !self.state_path().is_file() {
            return;
        }
        let state = self.load();
        let slots = CoreSlots::load(&self.root);
        let updates = self.root.join("updates");
        for component in [UpdateComponent::Desktop, UpdateComponent::Core] {
            let Some(record) = state.layers.get(component.as_str()) else {
                continue;
            };
            let mut keep: HashSet<String> = [
                Some(&record.current_version),
                record.available_version.as_ref(),
                record.staged_version.as_ref(),
                record.target_version.as_ref(),
            ]
            .into_iter()
            .flatten()
            .cloned()
            .collect();
            if component == UpdateComponent::Core {
                keep.extend(
                    [&slots.active, &slots.pending, &slots.previous]
                        .into_iter()
                        .flatten()
                        .map(|slot| slot.version.clone()),
                );
            }
            let Some(newest_kept) = keep
                .iter()
                .filter_map(|version| super::manifest::parse_version(version))
                .max()
            else {
                continue;
            };
            let name = component.as_str();
            for dir in [updates.join(name), updates.join("cache").join(name)] {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let version = entry.file_name().to_string_lossy().into_owned();
                    if !path.is_dir() {
                        continue;
                    }
                    if keep.contains(&version) {
                        drop_finished_switch_backup(&path);
                        continue;
                    }
                    if super::manifest::parse_version(&version)
                        .is_none_or(|parsed| parsed >= newest_kept)
                    {
                        continue;
                    }
                    match std::fs::remove_dir_all(&path) {
                        Ok(()) => log::info!("[Updates] removed retired {name} {version}"),
                        Err(error) => {
                            log::warn!("[Updates] cannot remove retired {name} {version}: {error}")
                        }
                    }
                }
            }
        }
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
        let core = self.layer_status(&state, UpdateComponent::Core, current_core, live);
        let attention = attention_from([&desktop, &remote, &core]);
        // The command already resolves and verifies current_core. Do not hash
        // the runtime again on each progress poll. Pending is a staged candidate,
        // not an attestation that it is running; launch still verifies its tree.
        let pending = CoreSlots::load(&self.root)
            .pending
            .filter(|slot| slot.path.is_file());
        let core_runtime = super::CoreRuntimeStatus {
            missing: current_core == "0.0.0",
            active_version: (current_core != "0.0.0").then(|| current_core.to_string()),
            pending_version: pending.map(|slot| slot.version),
        };
        UpdateStatusSnapshot {
            desktop,
            remote,
            core,
            core_runtime,
            preferences: state.preferences,
            live_auto_update: live,
            attention,
            checked_at: state.last_checked_at,
            restart_schedule: None,
        }
    }

    fn layer_status(
        &self,
        state: &PersistedState,
        component: UpdateComponent,
        current: &str,
        live: bool,
    ) -> LayerStatus {
        let mut record = state
            .layers
            .get(component.as_str())
            .cloned()
            .unwrap_or_else(|| LayerRecord::new(current));
        if component == UpdateComponent::Core
            && super::pending_core(&self.root).is_none()
            && record.staged_version.as_deref() == Some(current)
            && matches!(
                record.phase,
                UpdatePhase::WaitingForRestart | UpdatePhase::WaitingForIdle | UpdatePhase::Staged
            )
        {
            record.phase = UpdatePhase::Applied;
            record.available_version = None;
            record.target_version = None;
            record.failure_reason = None;
        } else if component == UpdateComponent::Core
            && super::pending_core(&self.root).is_none()
            && record.staged_version.is_some()
            && record.staged_version.as_deref() != Some(current)
            && matches!(
                record.phase,
                UpdatePhase::WaitingForRestart | UpdatePhase::WaitingForIdle | UpdatePhase::Staged
            )
        {
            record.phase = UpdatePhase::Failed;
            record.failure_reason = Some("pendingCoreRejectedBeforeLaunch".into());
        }
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
            None => vec![
                UpdateComponent::Desktop,
                UpdateComponent::Remote,
                UpdateComponent::Core,
            ],
        };
        let mut listings = HashMap::new();
        for item in &components {
            let repo = item.github_repo();
            if listings.contains_key(repo) {
                continue;
            }
            let previous = state.release_indexes.get(repo);
            let mut listed = self
                .source
                .list_releases(repo, previous.and_then(|index| index.etag.as_deref()))
                .map_err(AppError::Message)?;
            if listed.not_modified {
                if let Some(previous) = previous {
                    listed.releases = previous.releases.clone();
                }
            } else if !listed.rate_limited {
                state.release_indexes.insert(
                    repo.into(),
                    ReleaseIndex {
                        etag: listed.etag.clone(),
                        releases: listed.releases.clone(),
                    },
                );
            }
            listings.insert(repo, listed);
        }
        for item in components {
            let current = currents.of(item);
            let record = Self::layer_mut(&mut state, item, current);
            let was_applied = record.phase == UpdatePhase::Applied;
            // A release that is already staged and waiting stays waiting. Marking
            // it Available again makes the next download re-stage it: for Core
            // that re-extracts the whole tree on every check.
            let waiting = matches!(
                record.phase,
                UpdatePhase::Staged | UpdatePhase::WaitingForRestart | UpdatePhase::WaitingForIdle
            )
            .then(|| (record.phase, record.staged_version.clone()));
            // Preserve repair state unless a strictly newer release supersedes
            // the failed target; failure must never disable future checks.
            let incomplete = (item == UpdateComponent::Desktop
                && record.failure_reason.as_deref() == Some("desktopInstallIncomplete"))
            .then(|| record.clone());
            record.phase =
                transition(record.phase, &UpdateEvent::StartCheck).unwrap_or(UpdatePhase::Checking);
            record.operation_id = Some(op.operation_id.clone());
            let listed = &listings[item.github_repo()];
            if listed.rate_limited {
                if let Some(incomplete) = incomplete {
                    *record = incomplete;
                    continue;
                }
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
            let mut signed_core_candidates = HashMap::new();
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
                let cached = listed
                    .not_modified
                    .then(|| {
                        release
                            .tag_name
                            .strip_prefix(item.tag_prefix())
                            .filter(|version| super::manifest::parse_version(version).is_some())
                            .and_then(|version| {
                                load_checked_manifest(&self.root, item, version).ok()
                            })
                    })
                    .flatten();
                let (raw, sig) = match cached {
                    Some(cached) => cached,
                    None => (
                        self.source
                            .fetch_bytes(&manifest_asset.browser_download_url)
                            .map_err(AppError::Message)?,
                        self.source
                            .fetch_bytes(&sig_asset.browser_download_url)
                            .map_err(AppError::Message)?,
                    ),
                };
                match verify_signed_manifest(&raw, &sig, &self.trust) {
                    Ok(manifest) => {
                        persist_checked_manifest(&self.root, item, &manifest.version, &raw, &sig)
                            .map_err(|error| AppError::Message(error.to_string()))?;
                        if item == UpdateComponent::Core {
                            signed_core_candidates.insert(manifest.release_tag.clone(), (raw, sig));
                        }
                        candidates.push(ReleaseCandidate {
                            manifest,
                            github_prerelease: release.prerelease,
                            published_at: release.published_at.clone(),
                        });
                    }
                    Err(error) => {
                        record.failure_reason = Some(error.to_string());
                        record.phase = UpdatePhase::Failed;
                    }
                }
            }
            let ctx = SelectContext {
                component: item,
                channel: state.preferences.channel,
                current_version: incomplete
                    .as_ref()
                    .and_then(|record| {
                        record
                            .target_version
                            .as_deref()
                            .or(record.staged_version.as_deref())
                    })
                    .filter(|target| {
                        super::manifest::parse_version(target)
                            .zip(super::manifest::parse_version(current))
                            .is_some_and(|(target, current)| target > current)
                    })
                    .unwrap_or(current),
                platform: current_platform(),
                arch: current_arch(),
                installed_desktop: currents.desktop,
                bridge_api: currents.bridge_api,
                remote_protocol: currents.remote_protocol,
            };
            if let Some(picked) = select_compatible(&candidates, &ctx) {
                let selected = candidates
                    .into_iter()
                    .find(|candidate| {
                        candidate.manifest.version == picked.version
                            && candidate.manifest.release_tag == picked.release_tag
                    })
                    .expect("selection must refer to one of its candidates");
                let manifest = selected.manifest;
                if item == UpdateComponent::Core {
                    let Some((raw, sig)) = signed_core_candidates.remove(&picked.release_tag)
                    else {
                        let record = Self::layer_mut(&mut state, item, current);
                        record.failure_reason = Some("selectedCoreSignatureMissing".into());
                        record.phase = UpdatePhase::Failed;
                        continue;
                    };
                    if let Err(error) =
                        persist_checked_manifest(&self.root, item, &manifest.version, &raw, &sig)
                    {
                        let record = Self::layer_mut(&mut state, item, current);
                        record.failure_reason = Some(error.to_string());
                        record.phase = UpdatePhase::Failed;
                        continue;
                    }
                }
                let record = Self::layer_mut(&mut state, item, current);
                record.available_version = Some(picked.version.clone());
                record.target_version = Some(picked.version);
                record.release_notes = manifest.release_notes.clone();
                record.download_total = select_asset(&manifest, item, &AssetTarget::local())
                    .map_or(0, |asset| asset.size);
                record.download_bytes = 0;
                record.manifest = Some(manifest);
                record.phase = match waiting {
                    Some((phase, Some(staged)))
                        if record.target_version.as_deref() == Some(staged.as_str())
                            && self.still_staged(item, record, &staged) =>
                    {
                        record.download_bytes = record.download_total;
                        phase
                    }
                    _ => transition(record.phase, &UpdateEvent::FoundAvailable)
                        .unwrap_or(UpdatePhase::Available),
                };
                record.failure_reason = None;
            } else {
                let record = Self::layer_mut(&mut state, item, current);
                if let Some(incomplete) = incomplete {
                    *record = incomplete;
                    continue;
                }
                record.phase = if was_applied {
                    UpdatePhase::Applied
                } else {
                    transition(record.phase, &UpdateEvent::NoneAvailable)
                        .unwrap_or(UpdatePhase::Idle)
                };
            }
        }
        state.last_checked_at = Some(now_secs());
        self.save(&state)?;
        Ok(op)
    }

    /// Downloads the asset for this machine (Desktop, Core).
    pub async fn download(
        &self,
        component: UpdateComponent,
        currents: Currents<'_>,
        restart_reasons: Vec<String>,
    ) -> AppResult<UpdateOperation> {
        self.download_for(component, currents, restart_reasons, AssetTarget::local())
            .await
    }

    /// Downloads the asset for `target`. Remote passes the SSH host's
    /// platform; using this machine's would stage, say, the macOS package for
    /// a Linux host.
    pub async fn download_for(
        &self,
        component: UpdateComponent,
        currents: Currents<'_>,
        restart_reasons: Vec<String>,
        target: AssetTarget,
    ) -> AppResult<UpdateOperation> {
        let key = format!("download:{}", component.as_str());
        self.single_flight(
            key,
            self.download_inner(component, currents, restart_reasons, target),
        )
        .await
    }

    async fn download_inner(
        &self,
        component: UpdateComponent,
        currents: Currents<'_>,
        restart_reasons: Vec<String>,
        target: AssetTarget,
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
        let op = UpdateOperation {
            target_version: Some(manifest.version.clone()),
            ..op
        };
        let asset = select_asset(&manifest, component, &target).cloned();
        if asset.is_none() {
            record.phase = UpdatePhase::Failed;
            record.failure_reason = Some("missingAsset".into());
        }
        let Some(asset) = asset else {
            self.save(&state)?;
            return Ok(op);
        };
        record.download_total = asset.size;
        record.download_bytes = 0;
        self.save(&state)?;
        let url = format!("asset://{}", asset.name);
        let mut last_progress = std::time::Instant::now() - std::time::Duration::from_secs(1);
        let mut progress = |bytes| {
            if bytes != asset.size
                && last_progress.elapsed() < std::time::Duration::from_millis(500)
            {
                return;
            }
            last_progress = std::time::Instant::now();
            let mut latest = self.load();
            if let Some(record) = latest.layers.get_mut(component.as_str()) {
                if record.operation_id.as_deref() == Some(&op.operation_id)
                    && record.phase == UpdatePhase::Downloading
                {
                    record.download_bytes = bytes;
                    if let Err(error) = self.save(&latest) {
                        log::warn!("[Updates] progress persistence failed: {error}");
                    }
                }
            }
        };
        let fetched = self
            .source
            .fetch_bytes_with_progress(&url, &mut progress)
            .or_else(|_| {
                self.source.fetch_bytes_with_progress(
                    &format!(
                        "https://github.com/{}/releases/download/{}/{}",
                        component.github_repo(),
                        manifest.release_tag,
                        asset.name
                    ),
                    &mut progress,
                )
            });
        let bytes = match fetched {
            Ok(bytes) => bytes,
            Err(error) => {
                let record = Self::layer_mut(&mut state, component, &current);
                record.phase = UpdatePhase::Failed;
                record.failure_reason = Some(error.clone());
                self.save(&state)?;
                self.record_journal(&op, UpdatePhase::Failed, Some(error.clone()))?;
                return Err(AppError::Message(error));
            }
        };
        let record = Self::layer_mut(&mut state, component, &current);
        record.download_bytes = bytes.len() as u64;
        record.phase =
            transition(record.phase, &UpdateEvent::StartVerify).unwrap_or(UpdatePhase::Verifying);
        let dest = super::cache::component_cache(&self.root, component.as_str(), &manifest.version)
            .join(&asset.name);
        let staged = stage_bytes(&dest, &bytes, &asset.sha256, asset.size, &HostDisk);
        match staged {
            Ok(path) => {
                if let Err(error) = super::manifest::verify_asset_file(&path, &asset.sha256) {
                    record.phase = UpdatePhase::Failed;
                    record.failure_reason = Some(error.to_string());
                    let phase = record.phase;
                    let reason = record.failure_reason.clone();
                    self.save(&state)?;
                    self.record_journal(&op, phase, reason)?;
                    return Ok(op);
                }
                if component == UpdateComponent::Core {
                    let Some(core) = manifest.core.as_ref() else {
                        record.phase = UpdatePhase::Failed;
                        record.failure_reason = Some("missingCoreManifest".into());
                        self.save(&state)?;
                        return Ok(op);
                    };
                    let previous_launch = CoreSlots::load(&self.root)
                        .active
                        .and_then(|slot| slot.launch_id);
                    match load_checked_manifest(&self.root, component, &manifest.version)
                        .and_then(|(raw, sig)| {
                            verify_signed_manifest(&raw, &sig, &self.trust)
                                .map_err(|error| error.to_string())
                                .and_then(|checked| {
                                    if checked == manifest {
                                        Ok((raw, sig))
                                    } else {
                                        Err("persisted core manifest does not match the selected release".into())
                                    }
                                })
                        })
                        .and_then(|(raw, sig)| {
                            super::stage_verified_core(&self.root, &path, &manifest.version, core)
                                .and_then(|slot| {
                                    super::core_slots::persist_signed_core_trust(
                                        &self.root,
                                        &manifest.version,
                                        &raw,
                                        &sig,
                                    )
                                    .map_err(|error| error.to_string())?;
                                    set_pending(&self.root, slot).map_err(|error| error.to_string())
                                })
                        }) {
                        Ok(_) => {
                            debug_assert_eq!(
                                CoreSlots::load(&self.root)
                                    .active
                                    .and_then(|slot| slot.launch_id),
                                previous_launch
                            );
                        }
                        Err(error) => {
                            record.phase = UpdatePhase::Failed;
                            record.failure_reason = Some(error);
                            let phase = record.phase;
                            let reason = record.failure_reason.clone();
                            self.save(&state)?;
                            self.record_journal(&op, phase, reason)?;
                            return Ok(op);
                        }
                    }
                }
                record.staged_asset = Some(path.clone());
                record.staged_version = Some(manifest.version.clone());
                record.staged_target = Some(target.clone());
                let wait = wait_kind_for(component);
                record.phase =
                    transition(record.phase, &UpdateEvent::Staged { wait }).unwrap_or(match wait {
                        WaitKind::Idle => UpdatePhase::WaitingForIdle,
                        WaitKind::Restart => UpdatePhase::WaitingForRestart,
                    });
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

    /// Settles a Desktop update the previous process handed to the installer.
    /// Only the relaunched build can tell whether the install happened, so
    /// Applied is decided here by the running version, never at launch time.
    pub fn reconcile_desktop_after_restart(&self, running: &str) -> AppResult<Option<UpdatePhase>> {
        self.reconcile_desktop_with_integrity(running, Ok(()))
    }

    fn reconcile_desktop_with_integrity(
        &self,
        running: &str,
        integrity: Result<(), String>,
    ) -> AppResult<Option<UpdatePhase>> {
        let mut state = self.load();
        let Some(record) = state.layers.get_mut(UpdateComponent::Desktop.as_str()) else {
            return Ok(None);
        };
        if record.phase != UpdatePhase::Applying
            && !(record.phase == UpdatePhase::Applied && integrity.is_err())
        {
            return Ok(None);
        }
        let target = record
            .target_version
            .clone()
            .or_else(|| record.staged_version.clone());
        let reached = target
            .as_deref()
            .and_then(super::manifest::parse_version)
            .zip(super::manifest::parse_version(running))
            .is_some_and(|(target, running)| running >= target);
        let integrity_error = integrity.err();
        if reached && integrity_error.is_none() {
            record.phase = UpdatePhase::Applied;
            record.current_version = running.to_string();
            record.available_version = None;
            record.staged_asset = None;
            record.staged_version = None;
            record.staged_target = None;
            record.failure_reason = None;
        } else {
            // The staged installer stays, so downloading again reuses it.
            record.phase = UpdatePhase::Failed;
            record.failure_reason = Some(
                if integrity_error.is_some() {
                    "desktopInstallIncomplete"
                } else {
                    "desktopInstallDidNotComplete"
                }
                .into(),
            );
        }
        let phase = record.phase;
        let reason = integrity_error.or_else(|| record.failure_reason.clone());
        let op = UpdateOperation {
            operation_id: record
                .operation_id
                .clone()
                .unwrap_or_else(|| ulid::Ulid::new().to_string()),
            component: UpdateComponent::Desktop,
            phase,
            target_version: target,
        };
        self.save(&state)?;
        self.record_journal(&op, phase, reason)?;
        Ok(Some(phase))
    }

    pub(crate) fn prepare_cached_desktop_repair(&self) -> AppResult<()> {
        let mut state = self.load();
        let Some(record) = state.layers.get_mut(UpdateComponent::Desktop.as_str()) else {
            return Ok(());
        };
        if record.phase != UpdatePhase::Failed
            || record.failure_reason.as_deref() != Some("desktopInstallIncomplete")
        {
            return Ok(());
        }
        let staged = record
            .staged_asset
            .as_ref()
            .ok_or_else(|| AppError::Message("nothingStaged".into()))?;
        let manifest = record
            .manifest
            .as_ref()
            .ok_or_else(|| AppError::Message("missingManifest".into()))?;
        let asset = select_asset(manifest, UpdateComponent::Desktop, &AssetTarget::local())
            .ok_or_else(|| AppError::Message("missingAsset".into()))?;
        super::manifest::verify_asset_file(staged, &asset.sha256)
            .map_err(|error| AppError::Message(error.to_string()))?;
        record.phase = UpdatePhase::WaitingForRestart;
        self.save(&state)
    }

    fn reconcile_identical_bundled_core(&self) -> AppResult<()> {
        let bundled =
            crate::enhanced_runtime::desktop_manager::packaged_enhanced_executable(&self.root);
        self.reconcile_core_with_bundled(bundled.as_deref())
    }

    fn reconcile_core_with_bundled(&self, bundled: Option<&Path>) -> AppResult<()> {
        // A verified active slot is still a runnable core; do not bypass its
        // managed next-start handoff merely because no bundled image exists.
        if bundled.is_none()
            && CoreSlots::load(&self.root)
                .active
                .as_ref()
                .is_some_and(|active| {
                    super::core_slots::verified_slot(&self.root, active, &self.trust)
                })
        {
            return Ok(());
        }
        let Some(slot) = CoreSlots::load(&self.root).pending else {
            return Ok(());
        };
        if !super::core_slots::verified_slot(&self.root, &slot, &self.trust)
            || bundled.is_some_and(|path| !super::core_slots::bundled_tree_matches(&slot, path))
        {
            return Ok(());
        }
        super::core_slots::promote_pending(&self.root)
            .map_err(|error| AppError::Message(error.to_string()))?;
        let mut state = self.load();
        let record = Self::layer_mut(&mut state, UpdateComponent::Core, &slot.version);
        record.current_version = slot.version.clone();
        record.phase = UpdatePhase::Applied;
        record.failure_reason = None;
        record.available_version = None;
        record.target_version = Some(slot.version.clone());
        record.staged_version = None;
        let op = UpdateOperation {
            operation_id: record
                .operation_id
                .clone()
                .unwrap_or_else(|| ulid::Ulid::new().to_string()),
            component: UpdateComponent::Core,
            phase: UpdatePhase::Applied,
            target_version: Some(slot.version),
        };
        self.save(&state)?;
        self.record_journal(
            &op,
            UpdatePhase::Applied,
            Some(
                if bundled.is_some() {
                    "pending core already matches the verified bundled tree"
                } else {
                    "verified pending core recovered a missing bundled core"
                }
                .into(),
            ),
        )
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
            target_version: record
                .target_version
                .clone()
                .or_else(|| record.staged_version.clone()),
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
                let staged_name = staged
                    .as_deref()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str());
                let sha = record
                    .manifest
                    .as_ref()
                    .and_then(|manifest| {
                        manifest
                            .assets
                            .iter()
                            .find(|asset| Some(asset.name.as_str()) == staged_name)
                    })
                    .map(|asset| asset.sha256.clone())
                    .unwrap_or_default();
                let staged_target = record.staged_target.clone();
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
                        staged_target: staged_target.as_ref(),
                        policy_enabled: true,
                        evidence: evidence.ok_or_else(|| {
                            AppError::Message("remote apply requires live idle evidence".into())
                        })?,
                    }),
                    UpdateComponent::Core => self.executor.install_core(&staged, &version),
                };
                match installed {
                    Ok(()) => {
                        if component == UpdateComponent::Core {
                            // The downloaded tree is pending, but only a live
                            // bridge attestation may promote it. Keep the UI in
                            // waiting-for-restart until that observation exists.
                            super::arm_core_pending_apply();
                            record.phase = UpdatePhase::WaitingForRestart;
                        } else if component == UpdateComponent::Desktop {
                            // The installer has only been started; this process
                            // is about to exit. The relaunched build settles the
                            // outcome in `reconcile_desktop_after_restart`.
                            record.phase = UpdatePhase::Applying;
                        } else {
                            record.phase = transition(record.phase, &UpdateEvent::StartValidate)
                                .unwrap_or(UpdatePhase::Validating);
                            record.phase = transition(record.phase, &UpdateEvent::Succeeded)
                                .unwrap_or(UpdatePhase::Applied);
                            record.current_version = version;
                        }
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
        if component == UpdateComponent::Core {
            super::core_slots::rollback_to_previous(&self.root)
                .map_err(|error| AppError::Message(error.to_string()))?;
        }
        let record = Self::layer_mut(&mut state, component, &current);
        record.phase = UpdatePhase::RolledBack;
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

/// `commit_candidate` keeps the replaced tree as `previous` only so a crash
/// mid-switch can recover; once the switch finished it is dead weight.
fn drop_finished_switch_backup(slot: &Path) {
    let previous = slot.join("previous");
    if !previous.is_dir() {
        return;
    }
    let done = std::fs::read(slot.join("switch.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|marker| marker["phase"] == "done");
    if done && slot.join("current").join(".complete").is_file() {
        if let Err(error) = std::fs::remove_dir_all(&previous) {
            log::warn!("[Updates] cannot remove {}: {error}", previous.display());
        }
    }
}

fn checked_manifest_dir(root: &Path, component: UpdateComponent, version: &str) -> PathBuf {
    root.join("updates")
        .join("manifests")
        .join(component.as_str())
        .join(version)
}

fn persist_checked_manifest(
    root: &Path,
    component: UpdateComponent,
    version: &str,
    raw: &[u8],
    signature: &[u8],
) -> std::io::Result<()> {
    let dir = checked_manifest_dir(root, component, version);
    std::fs::create_dir_all(&dir)?;
    write_atomic(&dir.join("update-manifest.json"), raw)?;
    write_atomic(&dir.join("update-manifest.json.sig"), signature)?;
    Ok(())
}

fn load_checked_manifest(
    root: &Path,
    component: UpdateComponent,
    version: &str,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let dir = checked_manifest_dir(root, component, version);
    let raw = std::fs::read(dir.join("update-manifest.json"))
        .map_err(|_| "signed core manifest is missing after check".to_string())?;
    let signature = std::fs::read(dir.join("update-manifest.json.sig"))
        .map_err(|_| "signed core signature is missing after check".to_string())?;
    Ok((raw, signature))
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
    let mut snapshot = engine.snapshot(currents.desktop, currents.remote, currents.core);
    snapshot.restart_schedule = super::restart_schedule(state);
    snapshot
}

pub async fn check_updates(
    state: &AppState,
    component: Option<UpdateComponent>,
) -> AppResult<UpdateStatusSnapshot> {
    let engine = super::engine(&state.data_root());
    let core_version = current_core_version(state);
    let currents = current_versions(&core_version);
    engine.check(component, currents).await?;
    let mut snapshot = engine.snapshot(currents.desktop, currents.remote, currents.core);
    snapshot.restart_schedule = super::restart_schedule(state);
    Ok(snapshot)
}

pub async fn download_update(
    state: &AppState,
    component: UpdateComponent,
    host_id: Option<String>,
) -> AppResult<UpdateOperation> {
    let target = match component {
        UpdateComponent::Remote => {
            let host_id = host_id
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| AppError::Message("remoteHostRequired".into()))?;
            let state = state.clone();
            tokio::task::spawn_blocking(move || {
                super::remote_host::host_asset_target(&state, &host_id)
            })
            .await
            .map_err(|error| AppError::Message(error.to_string()))?
            .map_err(AppError::Message)?
        }
        UpdateComponent::Desktop | UpdateComponent::Core => AssetTarget::local(),
    };
    let engine = super::engine(&state.data_root());
    let core_version = current_core_version(state);
    let currents = current_versions(&core_version);
    let reasons = state
        .runtime_status()
        .restart_reasons
        .iter()
        .map(|notice| notice.code.clone())
        .collect();
    engine
        .download_for(component, currents, reasons, target)
        .await
}

/// Startup half of a Desktop update: see
/// [`UpdateEngine::reconcile_desktop_after_restart`].
pub fn reconcile_desktop_after_restart(state: &AppState) -> AppResult<Option<UpdatePhase>> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            super::install_integrity::cleanup_retired_images(dir);
        }
    }
    let integrity = super::install_integrity::installed_bridge_integrity();
    let engine = super::engine(&state.data_root());
    let settled =
        engine.reconcile_desktop_with_integrity(env!("CARGO_PKG_VERSION"), integrity.clone())?;
    if integrity.is_ok() {
        engine.reconcile_identical_bundled_core()?;
    }
    Ok(settled)
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
    apply_update_with_ui(state, component, host_id, true)
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
        // The Remote layer stages one package for every host, so a package
        // downloaded for a Linux host must not reach a Mac (and vice versa).
        let host_target = super::remote_host::host_asset_target(&self.state, host)?;
        match request.staged_target {
            Some(staged) if *staged == host_target => {}
            Some(_) => return Err("stagedForDifferentHost".into()),
            None => return Err("stagedTargetUnknown".into()),
        }
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
    current_core_version_at(&state.data_root())
}

/// What the cached core version was verified against: the slot index and
/// the active executable's size and mtime. Any install, rollback or edit of
/// the executable changes one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CoreVersionKey {
    root: PathBuf,
    slots: Option<Vec<u8>>,
    executable: Option<(u64, Option<SystemTime>)>,
}

impl CoreVersionKey {
    fn read(root: &Path) -> Self {
        let slots = std::fs::read(CoreSlots::path(root)).ok();
        let executable = CoreSlots::load(root).active.and_then(|slot| {
            let metadata = std::fs::metadata(&slot.path).ok()?;
            Some((metadata.len(), metadata.modified().ok()))
        });
        Self {
            root: root.to_path_buf(),
            slots,
            executable,
        }
    }
}

static CORE_VERSION_CACHE: std::sync::Mutex<Option<(CoreVersionKey, String)>> =
    std::sync::Mutex::new(None);

/// Status, check and apply only need the running core's version. Verifying
/// it hashes the whole signed tree (about 400 MB on macOS), and the update
/// panel asks for status every second, so reuse the last verification until
/// the slot index or the executable changes. Launch still verifies the
/// tree itself through `resolve_enhanced_runtime`.
fn current_core_version_at(root: &Path) -> String {
    let key = CoreVersionKey::read(root);
    if let Ok(cache) = CORE_VERSION_CACHE.lock() {
        if let Some((cached_key, version)) = cache.as_ref() {
            if *cached_key == key {
                return version.clone();
            }
        }
    }
    let version = match super::resolve_enhanced_runtime(root) {
        Some(runtime) if super::manifest::parse_version(&runtime.version).is_some() => {
            runtime.version
        }
        Some(_) => env!("CARGO_PKG_VERSION").to_string(),
        // Missing is not the installed Desktop version. Offer the compatible
        // core release even when it matches the current Desktop version.
        None => "0.0.0".into(),
    };
    if let Ok(mut cache) = CORE_VERSION_CACHE.lock() {
        *cache = Some((key, version.clone()));
    }
    version
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

    #[test]
    fn cached_core_version_is_keyed_on_the_slot_index_and_executable() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let empty = CoreVersionKey::read(root);
        assert_eq!(empty, CoreVersionKey::read(root));

        let exe = root.join("codex");
        std::fs::write(&exe, b"core-a").unwrap();
        let slots = CoreSlots::path(root);
        std::fs::create_dir_all(slots.parent().unwrap()).unwrap();
        let index = serde_json::json!({
            "active": {
                "version": "0.2.10",
                "digest": "sha256:00",
                "protocol": "",
                "path": exe,
                "helpers": []
            }
        });
        std::fs::write(&slots, serde_json::to_vec(&index).unwrap()).unwrap();
        let installed = CoreVersionKey::read(root);
        assert_ne!(empty, installed, "an install changes the slot index");
        assert!(installed.executable.is_some());

        std::fs::write(&exe, b"core-a-but-longer").unwrap();
        assert_ne!(
            installed,
            CoreVersionKey::read(root),
            "replacing the executable invalidates the cached verification"
        );
    }
    use crate::updates::core_slots::CoreSlot;
    use crate::updates::github::{GithubAsset, GithubRelease, ListedReleases, MemorySource};
    use crate::updates::manifest::{
        sign_raw, AssetRef, CompatRange, DataFormat, MANIFEST_SCHEMA_VERSION,
    };
    use crate::updates::TrustStore;
    use ed25519_dalek::SigningKey;
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::io::Write;
    use std::sync::Arc;

    /// A name each layer's asset rules accept on the platform running the test.
    fn fixture_asset_name(component: UpdateComponent) -> &'static str {
        match component {
            UpdateComponent::Core => "payload.zip",
            UpdateComponent::Remote => "vellum-remote-fixture.tar.gz",
            UpdateComponent::Desktop if cfg!(target_os = "macos") => "Vellum.app.tar.gz",
            UpdateComponent::Desktop => "Vellum_x64-setup.exe",
        }
    }

    #[test]
    fn missing_core_is_reported_in_the_update_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, _) = fixture_engine(
            dir.path(),
            UpdateComponent::Core,
            "0.3.2",
            &core_archive(),
            Arc::new(RecordingExecutor::default()),
        );
        let snapshot = engine.snapshot("0.3.1", "0.3.1", "0.0.0");
        assert!(snapshot.core_runtime.missing);
        assert!(snapshot.core_runtime.active_version.is_none());
        assert!(snapshot.core_runtime.pending_version.is_none());
        assert_eq!(snapshot.core.current_version, "0.0.0");
    }

    fn signed_fixture(
        signing: &SigningKey,
        component: UpdateComponent,
        version: &str,
        bytes: &[u8],
    ) -> (Vec<u8>, Vec<u8>, UpdateManifest) {
        let hash = hex::encode(Sha256::digest(bytes));
        let core = (component == UpdateComponent::Core).then(|| {
            use crate::updates::manifest::{CoreFile, CoreManifest, CoreTarget};
            CoreManifest {
                upstream_commit: "a".repeat(40),
                enhanced_commit: "b".repeat(40),
                feature_profile: "test".into(),
                protocol_schema_sha256: format!(
                    "sha256:{}",
                    hex::encode(Sha256::digest(b"schema"))
                ),
                protocol_compat: CompatRange::new(">=1 <2"),
                targets: vec![CoreTarget {
                    platform: current_platform().into(),
                    arch: current_arch().into(),
                    asset: "payload.zip".into(),
                    executable: "codex.exe".into(),
                    helpers: vec!["codex-command-runner.exe".into()],
                    uncompressed_size: 10,
                    protocol_schema_sha256: String::new(),
                    files: vec![
                        CoreFile {
                            path: "codex.exe".into(),
                            size: 4,
                            sha256: hex::encode(Sha256::digest(b"core")),
                            executable: true,
                        },
                        CoreFile {
                            path: "codex-command-runner.exe".into(),
                            size: 6,
                            sha256: hex::encode(Sha256::digest(b"helper")),
                            executable: true,
                        },
                    ],
                }],
            }
        });
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
                name: fixture_asset_name(component).into(),
                size: bytes.len() as u64,
                sha256: hash,
            }],
            core,
        };
        let raw = serde_json::to_vec(&manifest).unwrap();
        let sig = sign_raw(&raw, signing).to_vec();
        (raw, sig, manifest)
    }

    fn core_archive() -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut bytes);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("codex.exe", options).unwrap();
            zip.write_all(b"core").unwrap();
            zip.start_file("codex-command-runner.exe", options).unwrap();
            zip.write_all(b"helper").unwrap();
            zip.finish().unwrap();
        }
        bytes.into_inner()
    }

    #[tokio::test]
    async fn download_does_not_clear_restart_reasons_or_active_core() {
        let dir = tempfile::tempdir().unwrap();
        let signing = SigningKey::from_bytes(&[3u8; 32]);
        let trust = TrustStore::from_key("test-key", signing.verifying_key());
        let payload = core_archive();
        let (raw, sig, _) = signed_fixture(&signing, UpdateComponent::Core, "0.2.0", &payload);
        let mut releases = HashMap::new();
        releases.insert("https://example/manifest".into(), raw);
        releases.insert("https://example/sig".into(), sig);
        releases.insert("asset://payload.zip".into(), payload);
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
                protocol_compat: "*".into(),
                path: PathBuf::from("old"),
                helpers: Vec::new(),
                files: Vec::new(),
                enhanced_commit: String::new(),
                feature_profile: String::new(),
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
        assert_eq!(snap.core_runtime.pending_version.as_deref(), Some("0.2.0"));

        // Checking again finds the same release. It is already pending, so it
        // keeps waiting instead of being offered (and re-staged) again.
        engine
            .check(Some(UpdateComponent::Core), currents)
            .await
            .unwrap();
        let rechecked = engine.snapshot("0.2.9", "0.2.9", "bundled");
        assert_eq!(rechecked.core.phase, UpdatePhase::WaitingForRestart);
        assert_eq!(rechecked.core.staged_version.as_deref(), Some("0.2.0"));

        // Once the pending slot is gone the same release is offered again.
        let mut without_pending = CoreSlots::load(dir.path());
        without_pending.pending = None;
        without_pending.save(dir.path()).unwrap();
        engine
            .check(Some(UpdateComponent::Core), currents)
            .await
            .unwrap();
        assert_eq!(
            engine.snapshot("0.2.9", "0.2.9", "bundled").core.phase,
            UpdatePhase::Available
        );
        engine
            .download(UpdateComponent::Core, currents, Vec::new())
            .await
            .unwrap();
        assert_eq!(
            CoreSlots::load(dir.path()).pending.unwrap().version,
            "0.2.0"
        );

        super::super::core_slots::promote_pending(dir.path()).unwrap();
        let promoted = engine.snapshot("0.2.9", "0.2.9", "0.2.0");
        assert_eq!(promoted.core.phase, UpdatePhase::Applied);
        assert_eq!(promoted.core.current_version, "0.2.0");
        assert!(promoted.core.available_version.is_none());
        assert!(!promoted.core_runtime.missing);
        assert_eq!(
            promoted.core_runtime.active_version.as_deref(),
            Some("0.2.0")
        );
        assert!(promoted.core_runtime.pending_version.is_none());
    }

    #[tokio::test]
    async fn automatic_check_includes_signed_core_releases() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Arc::new(RecordingExecutor::default());
        let (engine, currents) = fixture_engine(
            dir.path(),
            UpdateComponent::Core,
            "0.3.0",
            b"ignored-for-core",
            recorder,
        );
        assert!(engine
            .snapshot("0.2.9", "0.2.9", "bundled")
            .checked_at
            .is_none());
        engine.check(None, currents).await.unwrap();
        let snapshot = engine.snapshot("0.2.9", "0.2.9", "bundled");
        assert_eq!(snapshot.core.phase, UpdatePhase::Available);
        // The panel says when Vellum last looked, so the check must be remembered.
        assert!(snapshot.checked_at.is_some());
    }

    #[tokio::test]
    async fn conditional_check_reuses_signed_manifests_and_lists_each_repo_once() {
        struct ConditionalSource {
            inner: SharedSource,
            lists: std::sync::atomic::AtomicUsize,
            fetches: std::sync::atomic::AtomicUsize,
        }
        impl super::super::github::ReleaseSource for ConditionalSource {
            fn list_releases(
                &self,
                repo: &str,
                etag: Option<&str>,
            ) -> Result<ListedReleases, String> {
                use std::sync::atomic::Ordering;
                self.lists.fetch_add(1, Ordering::SeqCst);
                if etag == Some("release-index-v1") {
                    return Ok(ListedReleases {
                        etag: etag.map(str::to_string),
                        not_modified: true,
                        rate_limited: false,
                        retry_after_secs: 0,
                        releases: Vec::new(),
                    });
                }
                let mut listed = self.inner.list_releases(repo, etag)?;
                listed.etag = Some("release-index-v1".into());
                Ok(listed)
            }
            fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
                self.fetches
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.inner.fetch_bytes(url)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let (mut engine, currents) = fixture_engine(
            dir.path(),
            UpdateComponent::Core,
            "0.3.0",
            b"core",
            Arc::new(RecordingExecutor::default()),
        );
        let source = Arc::new(ConditionalSource {
            inner: engine.source.clone(),
            lists: Default::default(),
            fetches: Default::default(),
        });
        engine.source = source.clone();
        engine.check(None, currents).await.unwrap();
        assert_eq!(source.lists.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(source.fetches.load(std::sync::atomic::Ordering::SeqCst), 2);
        // Re-open like the production command path: ETag/index survive engine creation.
        let reopened = UpdateEngine::open(dir.path(), engine.trust.clone(), source.clone());
        reopened.check(None, currents).await.unwrap();
        assert_eq!(source.lists.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(source.fetches.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            reopened
                .snapshot("0.2.9", "0.2.9", "bundled")
                .core
                .available_version
                .as_deref(),
            Some("0.3.0")
        );
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
        releases.insert(
            format!("asset://{}", fixture_asset_name(UpdateComponent::Desktop)),
            payload.to_vec(),
        );
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

    #[test]
    fn retired_update_versions_are_pruned_but_referenced_and_newer_ones_stay() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let engine = UpdateEngine::open(
            root,
            TrustStore::bundled(),
            Arc::new(MemorySource {
                releases: HashMap::new(),
                listed: ListedReleases {
                    etag: None,
                    not_modified: false,
                    rate_limited: false,
                    retry_after_secs: 0,
                    releases: Vec::new(),
                },
            }),
        );
        let updates = root.join("updates");
        let tree = |parts: &[&str]| {
            let path = parts
                .iter()
                .fold(updates.clone(), |path, part| path.join(part));
            std::fs::create_dir_all(&path).unwrap();
            path
        };
        // Nothing is pruned until there is state to say what is in use.
        tree(&["core", "0.1.0", "current"]);
        engine.prune_retired_versions();
        assert!(updates.join("core").join("0.1.0").is_dir());

        let mut state = PersistedState::default();
        state
            .layers
            .insert("desktop".into(), LayerRecord::new("0.2.0"));
        let mut core = LayerRecord::new("0.4.0");
        core.staged_version = Some("0.5.0".into());
        state.layers.insert("core".into(), core);
        engine.save(&state).unwrap();
        let slot = |version: &str| {
            serde_json::json!({"version": version, "digest": "d", "protocol": "p",
                "path": "x", "helpers": [], "launchId": null})
        };
        let slots: CoreSlots = serde_json::from_value(serde_json::json!({
            "active": slot("0.4.0"), "pending": slot("0.5.0"), "previous": slot("0.3.0")
        }))
        .unwrap();
        slots.save(root).unwrap();

        for version in ["0.1.0", "0.2.0", "0.3.0", "0.4.0", "0.5.0", "0.6.0"] {
            tree(&["core", version, "current"]);
            tree(&["cache", "core", version]);
        }
        for version in ["0.1.0", "0.2.0", "0.3.0"] {
            tree(&["desktop", version, "current"]);
            tree(&["cache", "desktop", version]);
        }
        tree(&["core", "not-a-version"]);
        // A finished switch leaves its backup behind; an unfinished one keeps it.
        tree(&["core", "0.5.0", "previous"]);
        std::fs::write(updates.join("core/0.5.0/current/.complete"), b"ok").unwrap();
        std::fs::write(
            updates.join("core/0.5.0/switch.json"),
            br#"{"phase":"done"}"#,
        )
        .unwrap();
        tree(&["core", "0.4.0", "previous"]);
        std::fs::write(
            updates.join("core/0.4.0/switch.json"),
            br#"{"phase":"renamingCandidate"}"#,
        )
        .unwrap();

        engine.prune_retired_versions();

        let present = |parts: &[&str]| {
            parts
                .iter()
                .fold(updates.clone(), |p, part| p.join(part))
                .exists()
        };
        for base in [&["core"][..], &["cache", "core"][..]] {
            let at = |version: &str| present(&[base, &[version]].concat());
            assert!(
                !at("0.1.0") && !at("0.2.0"),
                "retired cores stay in {base:?}"
            );
            assert!(
                at("0.3.0") && at("0.4.0") && at("0.5.0"),
                "slot versions go"
            );
            assert!(
                at("0.6.0"),
                "a version newer than every reference is a download in flight"
            );
        }
        assert!(present(&["core", "not-a-version"]));
        assert!(!present(&["core", "0.5.0", "previous"]));
        assert!(present(&["core", "0.4.0", "previous"]));
        for base in [&["desktop"][..], &["cache", "desktop"][..]] {
            let at = |version: &str| present(&[base, &[version]].concat());
            assert!(!at("0.1.0"), "old desktop installer stays in {base:?}");
            assert!(at("0.2.0") && at("0.3.0"));
        }
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
        let core_payload = (component == UpdateComponent::Core).then(core_archive);
        let payload = core_payload.as_deref().unwrap_or(payload);
        let (raw, sig, _) = signed_fixture(&signing, component, version, payload);
        let mut releases = HashMap::new();
        releases.insert("https://example/manifest".into(), raw);
        releases.insert("https://example/sig".into(), sig);
        releases.insert(
            format!("asset://{}", fixture_asset_name(component)),
            payload.to_vec(),
        );
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
        // Launching the installer is not success: this process still runs
        // the old build until it exits.
        assert_eq!(applied.phase, UpdatePhase::Applying);
        assert!(
            !recorder.desktop.lock().unwrap().is_empty(),
            "Allow must invoke install_desktop; a phase-only flip is a bug"
        );
        assert_eq!(
            engine.snapshot("0.2.9", "0.2.9", "bundled").desktop.phase,
            UpdatePhase::Applying
        );

        assert_eq!(
            engine.reconcile_desktop_after_restart("0.3.1").unwrap(),
            Some(UpdatePhase::Applied)
        );
        let snap = engine.snapshot("0.3.1", "0.2.9", "bundled");
        assert_eq!(snap.desktop.phase, UpdatePhase::Applied);
        assert_eq!(snap.desktop.current_version, "0.3.1");
        assert_eq!(snap.desktop.staged_version, None);
        assert_eq!(
            engine.reconcile_desktop_after_restart("0.3.1").unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn desktop_restart_on_the_old_version_reports_the_install_failed() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Arc::new(RecordingExecutor::default());
        let (engine, currents) = fixture_engine(
            dir.path(),
            UpdateComponent::Desktop,
            "0.3.1",
            b"desk-payload",
            recorder,
        );
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

        // The installer was cancelled or failed; the old build came back.
        assert_eq!(
            engine.reconcile_desktop_after_restart("0.2.9").unwrap(),
            Some(UpdatePhase::Failed)
        );
        let snap = engine.snapshot("0.2.9", "0.2.9", "bundled");
        assert_eq!(snap.desktop.phase, UpdatePhase::Failed);
        assert_eq!(
            snap.desktop.failure_reason.as_deref(),
            Some("desktopInstallDidNotComplete")
        );
        assert_eq!(snap.desktop.current_version, "0.2.9");
        assert_eq!(snap.desktop.staged_version.as_deref(), Some("0.3.1"));
    }

    #[tokio::test]
    async fn matching_desktop_version_with_stale_bridge_fails_and_keeps_cached_repair() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, currents) = fixture_engine(
            dir.path(),
            UpdateComponent::Desktop,
            "0.3.1",
            b"desk-payload",
            Arc::new(RecordingExecutor::default()),
        );
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
        assert_eq!(
            engine
                .reconcile_desktop_with_integrity(
                    "0.3.1",
                    Err("expected new bridge, got old bridge".into())
                )
                .unwrap(),
            Some(UpdatePhase::Failed)
        );
        let state = engine.load();
        let desktop = &state.layers["desktop"];
        assert_eq!(
            desktop.failure_reason.as_deref(),
            Some("desktopInstallIncomplete")
        );
        assert!(desktop.staged_asset.as_ref().unwrap().is_file());
        engine
            .check(Some(UpdateComponent::Desktop), currents)
            .await
            .unwrap();
        assert_eq!(
            engine.snapshot("0.3.1", "0.2.9", "bundled").desktop.phase,
            UpdatePhase::Failed
        );
        engine.prepare_cached_desktop_repair().unwrap();
        assert_eq!(
            engine.snapshot("0.3.1", "0.2.9", "bundled").desktop.phase,
            UpdatePhase::WaitingForRestart
        );
    }

    #[tokio::test]
    async fn incomplete_desktop_install_can_be_superseded_only_by_a_newer_release() {
        for running in ["0.2.9", "0.3.1"] {
            let dir = tempfile::tempdir().unwrap();
            let executor = Arc::new(RecordingExecutor::default());
            let (engine, currents) = fixture_engine(
                dir.path(),
                UpdateComponent::Desktop,
                "0.3.1",
                b"old installer",
                executor.clone(),
            );
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
            engine
                .reconcile_desktop_with_integrity("0.3.1", Err("stale bridge".into()))
                .unwrap();
            let current = Currents {
                desktop: running,
                ..currents
            };
            for published in ["0.3.0", "0.3.1", "0.3.2"] {
                let (next, _) = fixture_engine(
                    dir.path(),
                    UpdateComponent::Desktop,
                    published,
                    b"new installer",
                    executor.clone(),
                );
                next.check(Some(UpdateComponent::Desktop), current)
                    .await
                    .unwrap();
                let desktop = next.snapshot(running, "0.2.9", "bundled").desktop;
                if published != "0.3.2" {
                    assert_eq!(desktop.phase, UpdatePhase::Failed);
                    assert_eq!(
                        desktop.failure_reason.as_deref(),
                        Some("desktopInstallIncomplete")
                    );
                    assert_eq!(desktop.staged_version.as_deref(), Some("0.3.1"));
                } else {
                    assert_eq!(desktop.phase, UpdatePhase::Available);
                    assert_eq!(desktop.target_version.as_deref(), Some("0.3.2"));
                    assert!(desktop.failure_reason.is_none());
                    next.prepare_cached_desktop_repair().unwrap();
                    assert_eq!(
                        next.snapshot(running, "0.2.9", "bundled").desktop.phase,
                        UpdatePhase::Available
                    );
                    next.download(UpdateComponent::Desktop, current, Vec::new())
                        .await
                        .unwrap();
                    assert_eq!(
                        next.snapshot(running, "0.2.9", "bundled")
                            .desktop
                            .staged_version
                            .as_deref(),
                        Some("0.3.2")
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn missing_bundled_core_recovers_verified_pending_with_helpers() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, currents) = fixture_engine(
            dir.path(),
            UpdateComponent::Core,
            "0.3.1",
            &core_archive(),
            Arc::new(RecordingExecutor::default()),
        );
        engine
            .check(Some(UpdateComponent::Core), currents)
            .await
            .unwrap();
        engine
            .download(UpdateComponent::Core, currents, Vec::new())
            .await
            .unwrap();
        let pending = CoreSlots::load(dir.path()).pending.unwrap();
        engine.reconcile_core_with_bundled(None).unwrap();
        let resolved =
            super::super::core_slots::resolve_enhanced_runtime_with(dir.path(), &engine.trust)
                .unwrap();
        assert_eq!(resolved.path, pending.path);
        assert_eq!(resolved.helpers, pending.helpers);
        assert!(!resolved.helpers.is_empty());
        assert!(CoreSlots::load(dir.path()).pending.is_none());
        let record = &engine.load().layers["core"];
        assert_eq!(record.phase, UpdatePhase::Applied);
        assert_eq!(record.current_version, "0.3.1");
        assert_eq!(record.target_version.as_deref(), Some("0.3.1"));
    }

    #[tokio::test]
    async fn missing_bundled_core_does_not_promote_a_tampered_pending_helper() {
        let dir = tempfile::tempdir().unwrap();
        let (engine, currents) = fixture_engine(
            dir.path(),
            UpdateComponent::Core,
            "0.3.1",
            &core_archive(),
            Arc::new(RecordingExecutor::default()),
        );
        engine
            .check(Some(UpdateComponent::Core), currents)
            .await
            .unwrap();
        engine
            .download(UpdateComponent::Core, currents, Vec::new())
            .await
            .unwrap();
        let pending = CoreSlots::load(dir.path()).pending.unwrap();
        std::fs::write(&pending.helpers[0], b"tampered").unwrap();
        engine.reconcile_core_with_bundled(None).unwrap();
        assert!(CoreSlots::load(dir.path()).pending.is_some());
        assert!(CoreSlots::load(dir.path()).active.is_none());
    }

    #[tokio::test]
    async fn identical_bundled_core_settles_pending_only_after_all_signed_files_match() {
        let dir = tempfile::tempdir().unwrap();
        let payload = core_archive();
        let (engine, currents) = fixture_engine(
            dir.path(),
            UpdateComponent::Core,
            "0.3.1",
            &payload,
            Arc::new(RecordingExecutor::default()),
        );
        engine
            .check(Some(UpdateComponent::Core), currents)
            .await
            .unwrap();
        let downloaded = engine
            .download(UpdateComponent::Core, currents, Vec::new())
            .await
            .unwrap();
        assert_eq!(downloaded.target_version.as_deref(), Some("0.3.1"));
        assert_eq!(
            Journal::load(dir.path())
                .by_id(&downloaded.operation_id)
                .unwrap()
                .target_version
                .as_deref(),
            Some("0.3.1")
        );
        let bundled_dir = dir.path().join("bundled");
        std::fs::create_dir(&bundled_dir).unwrap();
        let bundled = bundled_dir.join("vellum-enhanced-codex.exe");
        std::fs::write(&bundled, b"core").unwrap();
        engine.reconcile_core_with_bundled(Some(&bundled)).unwrap();
        assert!(
            CoreSlots::load(dir.path()).pending.is_some(),
            "matching executable alone cannot skip helper verification"
        );
        std::fs::write(bundled_dir.join("codex-command-runner.exe"), b"helper").unwrap();
        engine.reconcile_core_with_bundled(Some(&bundled)).unwrap();
        let slots = CoreSlots::load(dir.path());
        assert!(slots.pending.is_none());
        assert_eq!(slots.active.unwrap().version, "0.3.1");
        engine
            .check(
                Some(UpdateComponent::Core),
                Currents {
                    core: "0.3.1",
                    ..currents
                },
            )
            .await
            .unwrap();
        let state = engine.load();
        assert_eq!(state.layers["core"].phase, UpdatePhase::Applied);
        assert_eq!(state.layers["core"].current_version, "0.3.1");
        assert_eq!(
            state.layers["core"].target_version.as_deref(),
            Some("0.3.1")
        );
    }

    #[test]
    fn reconcile_ignores_a_desktop_layer_that_was_not_applying() {
        let dir = tempfile::tempdir().unwrap();
        let signing = SigningKey::from_bytes(&[6u8; 32]);
        let engine = UpdateEngine::open(
            dir.path(),
            TrustStore::from_key("test-key", signing.verifying_key()),
            Arc::new(MemorySource {
                releases: HashMap::new(),
                listed: ListedReleases {
                    etag: None,
                    not_modified: false,
                    rate_limited: false,
                    retry_after_secs: 0,
                    releases: Vec::new(),
                },
            }),
        );
        assert_eq!(
            engine.reconcile_desktop_after_restart("0.3.1").unwrap(),
            None
        );
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
        }
        let signing = SigningKey::from_bytes(&[9u8; 32]);
        let trust = TrustStore::from_key("test-key", signing.verifying_key());
        let payload = b"nsis-bytes";
        let hash = hex::encode(Sha256::digest(payload));
        // Each platform only installs its own kind of Desktop asset.
        let (installer_name, current_exe) = if cfg!(target_os = "macos") {
            (
                "Vellum-macos-arm64.app.tar.gz",
                "/Applications/Vellum.app/Contents/MacOS/vellum-proxy-desktop",
            )
        } else {
            (
                "Vellum_0.5.0_x64-setup.exe",
                "C:/Program Files/Vellum/vellum-proxy-desktop.exe",
            )
        };
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
                name: installer_name.into(),
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
        releases.insert(format!("asset://{installer_name}"), payload.to_vec());
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
                current_exe: PathBuf::from(current_exe),
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
            "close/restart apply must launch the installer, not only copy the staged file"
        );
        if cfg!(target_os = "macos") {
            assert_eq!(launched[0].0, PathBuf::from("/bin/sh"));
            assert_eq!(launched[0].1[5], "/Applications/Vellum.app");
        } else {
            assert!(launched[0].1.iter().any(|arg| arg == "/P"));
            assert!(launched[0].1.iter().any(|arg| arg == "/UPDATE"));
            assert!(launched[0].1.iter().any(|arg| arg == "/R"));
        }
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

    #[test]
    fn core_rollback_does_not_report_success_when_slot_save_fails() {
        let dir = tempfile::tempdir().unwrap();
        let signing = SigningKey::from_bytes(&[5u8; 32]);
        let trust = TrustStore::from_key("test-key", signing.verifying_key());
        let engine = UpdateEngine::open(
            dir.path(),
            trust,
            Arc::new(MemorySource {
                releases: HashMap::new(),
                listed: ListedReleases {
                    etag: None,
                    not_modified: false,
                    rate_limited: false,
                    retry_after_secs: 0,
                    releases: Vec::new(),
                },
            }),
        );
        CoreSlots {
            active: Some(CoreSlot {
                version: "new".into(),
                digest: "sha256:new".into(),
                protocol: "p".into(),
                protocol_compat: "*".into(),
                path: PathBuf::from("new"),
                helpers: Vec::new(),
                files: Vec::new(),
                enhanced_commit: String::new(),
                feature_profile: String::new(),
                launch_id: None,
            }),
            previous: Some(CoreSlot {
                version: "old".into(),
                digest: "sha256:old".into(),
                protocol: "p".into(),
                protocol_compat: "*".into(),
                path: PathBuf::from("old"),
                helpers: Vec::new(),
                files: Vec::new(),
                enhanced_commit: String::new(),
                feature_profile: String::new(),
                launch_id: None,
            }),
            ..CoreSlots::default()
        }
        .save(dir.path())
        .unwrap();
        let mut state = PersistedState::default();
        state.layers.insert(
            UpdateComponent::Core.as_str().to_string(),
            LayerRecord {
                phase: UpdatePhase::Applied,
                current_version: "new".into(),
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
                signed_manifest_path: None,
                signed_signature_path: None,
                staged_target: None,
            },
        );
        engine.save(&state).unwrap();
        let slots_path = super::super::core_slots::CoreSlots::path(dir.path());
        std::fs::remove_file(&slots_path).unwrap();
        std::fs::create_dir_all(&slots_path).unwrap();
        let result = engine.rollback(UpdateComponent::Core);
        assert!(
            result.is_err(),
            "slot-save failure must not return RolledBack"
        );
        let after = engine.load();
        assert_eq!(
            after.layers.get("core").map(|layer| layer.phase),
            Some(UpdatePhase::Applied),
            "persisted phase must stay Applied when the slot switch fails"
        );
        assert_ne!(
            after.layers.get("core").map(|layer| layer.phase),
            Some(UpdatePhase::RolledBack)
        );
    }
}
