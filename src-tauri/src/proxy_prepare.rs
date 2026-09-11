//! Reusable proxy prepare, separate from the start transaction.
//!
//! Prepare validates the live executable, schema compatibility, Codex
//! capability discovery, and a settings fingerprint. The start transaction
//! must not repeat those stages when the captured identities still match.

use crate::error::{AppError, AppResult};
use crate::state::AppState;
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::watch;
use vellum_proxy_runtime::ArtifactIdentity;

#[derive(Debug, Clone)]
pub struct PrepareStage {
    pub name: &'static str,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone)]
pub struct PrepareSnapshot {
    pub executable: Option<ArtifactIdentity>,
    pub lockfile: Option<ArtifactIdentity>,
    /// SHA-256 of the packaged `enhanced-runtime.lock.json` bytes. Desktop
    /// reuse is bound to this digest, not a source-tree path.
    pub packaged_lockfile_digest: Option<String>,
    pub protocol_version: String,
    pub settings_fingerprint: String,
    pub capability_ok: bool,
    pub schema_ok: bool,
    pub schema_ran: bool,
    pub capability_ran: bool,
    pub catalog_ran: bool,
    pub stages: Vec<PrepareStage>,
}

impl PrepareSnapshot {
    pub fn still_valid<H: PrepareHost + ?Sized>(&self, host: &H) -> bool {
        let executable_ok = match (&self.executable, host.executable_path()) {
            (None, None) => true,
            (Some(identity), Some(path)) if identity.path == path => identity.still_matches(),
            _ => false,
        };
        if !executable_ok {
            return false;
        }
        let packaged_ok = match (
            self.packaged_lockfile_digest.as_deref(),
            host.packaged_lockfile_digest().as_deref(),
        ) {
            (Some(expected), Some(live)) => expected == live,
            (None, None) => true,
            _ => false,
        };
        if !packaged_ok {
            return false;
        }
        let lockfile_ok = match (&self.lockfile, host.lockfile_path()) {
            (None, None) => true,
            (Some(identity), Some(path)) if identity.path == path => identity.still_matches(),
            _ => false,
        };
        lockfile_ok
            && self.protocol_version == host.protocol_version()
            && self.settings_fingerprint == host.settings_fingerprint()
    }
}

pub trait PrepareHost: Send + Sync {
    fn executable_path(&self) -> Option<PathBuf>;
    fn lockfile_path(&self) -> Option<PathBuf>;
    /// Digest of the packaged Enhanced lockfile. Desktop always returns Some
    /// so a missing on-disk path cannot masquerade as a match.
    fn packaged_lockfile_digest(&self) -> Option<String> {
        None
    }
    fn protocol_version(&self) -> String;
    fn settings_fingerprint(&self) -> String;
    fn discover_capability(&self) -> Result<(), String>;
    fn probe_schema(&self) -> Result<(), String>;
    fn precompute_catalog(&self) -> Result<(), String>;
}

#[derive(Default)]
pub struct PrepareCoordinator {
    cached: Option<Arc<PrepareSnapshot>>,
}

impl PrepareCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run or reuse prepare. Concurrent callers share this mutex at the
    /// AppState layer, so the same runtime is not validated twice in parallel.
    pub async fn prepare<H: PrepareHost + ?Sized>(
        &mut self,
        host: &H,
        cancel: watch::Receiver<bool>,
    ) -> AppResult<Arc<PrepareSnapshot>> {
        self.prepare_with_progress(host, cancel, |_, _| {}).await
    }

    pub async fn prepare_with_progress<H: PrepareHost + ?Sized>(
        &mut self,
        host: &H,
        cancel: watch::Receiver<bool>,
        progress: impl FnMut(&str, u64),
    ) -> AppResult<Arc<PrepareSnapshot>> {
        if let Some(cached) = &self.cached {
            if !*cancel.borrow() && cached.still_valid(host) {
                return Ok(Arc::clone(cached));
            }
        }
        let snapshot = run_prepare(host, cancel, progress).await?;
        let snapshot = Arc::new(snapshot);
        self.cached = Some(Arc::clone(&snapshot));
        Ok(snapshot)
    }

    pub fn invalidate(&mut self) {
        self.cached = None;
    }
}

async fn run_prepare<H: PrepareHost + ?Sized>(
    host: &H,
    cancel: watch::Receiver<bool>,
    mut progress: impl FnMut(&str, u64),
) -> AppResult<PrepareSnapshot> {
    fail_if_cancelled(&cancel)?;
    let fingerprint = host.settings_fingerprint();
    let mut stages = Vec::new();
    let started = Instant::now();
    progress("discovery", 0);

    let executable = if let Some(path) = host.executable_path() {
        let path = path.clone();
        Some(
            tokio::task::spawn_blocking(move || ArtifactIdentity::capture(&path))
                .await
                .map_err(|error| AppError::Message(format!("prepare join failed: {error}")))?
                .map_err(|error| {
                    AppError::Message(format!("cannot identify Codex runtime: {error}"))
                })?,
        )
    } else {
        None
    };
    stages.push(PrepareStage {
        name: "discovery",
        elapsed_ms: started.elapsed().as_millis() as u64,
    });
    fail_if_cancelled(&cancel)?;

    let lock_started = Instant::now();
    progress("artifact_hash", 0);
    let packaged_lockfile_digest = host.packaged_lockfile_digest();
    let lockfile = if packaged_lockfile_digest.is_some() {
        None
    } else if let Some(path) = host.lockfile_path() {
        let path = path.clone();
        Some(
            tokio::task::spawn_blocking(move || ArtifactIdentity::capture(&path))
                .await
                .map_err(|error| AppError::Message(format!("prepare join failed: {error}")))?
                .map_err(|error| {
                    AppError::Message(format!("cannot identify Enhanced lockfile: {error}"))
                })?,
        )
    } else {
        None
    };
    stages.push(PrepareStage {
        name: "artifact_hash",
        elapsed_ms: lock_started.elapsed().as_millis() as u64,
    });
    fail_if_cancelled(&cancel)?;

    let cap_started = Instant::now();
    progress("capability", 0);
    host.discover_capability().map_err(AppError::Message)?;
    stages.push(PrepareStage {
        name: "capability",
        elapsed_ms: cap_started.elapsed().as_millis() as u64,
    });
    fail_if_cancelled(&cancel)?;

    let schema_started = Instant::now();
    progress("schema", 0);
    let schema_ok = match host.probe_schema() {
        Ok(()) => true,
        Err(error) => {
            log::warn!("[Proxy] Enhanced schema prepare failed (proxy may still start): {error}");
            false
        }
    };
    stages.push(PrepareStage {
        name: "schema",
        elapsed_ms: schema_started.elapsed().as_millis() as u64,
    });
    fail_if_cancelled(&cancel)?;

    let catalog_started = Instant::now();
    progress("catalog", 0);
    host.precompute_catalog().map_err(AppError::Message)?;
    stages.push(PrepareStage {
        name: "catalog",
        elapsed_ms: catalog_started.elapsed().as_millis() as u64,
    });
    fail_if_cancelled(&cancel)?;
    if fingerprint != host.settings_fingerprint() {
        return Err(AppError::Message(
            "proxy settings changed during prepare; retry Start".into(),
        ));
    }

    Ok(PrepareSnapshot {
        executable,
        lockfile,
        packaged_lockfile_digest,
        protocol_version: host.protocol_version(),
        settings_fingerprint: fingerprint,
        capability_ok: true,
        schema_ok,
        schema_ran: true,
        capability_ran: true,
        catalog_ran: true,
        stages,
    })
}

pub fn packaged_lockfile_sha256() -> String {
    use sha2::Digest;
    format!(
        "{:x}",
        Sha256::digest(crate::enhanced_runtime::desktop_manager::LOCK_BYTES)
    )
}

fn fail_if_cancelled(cancel: &watch::Receiver<bool>) -> AppResult<()> {
    if *cancel.borrow() {
        Err(AppError::Message("user stopped proxy".into()))
    } else {
        Ok(())
    }
}

pub fn settings_fingerprint(state: &AppState) -> String {
    let mut hasher = Sha256::new();
    if let Ok(bytes) = serde_json::to_vec(&state.routes()) {
        hasher.update(bytes);
    }
    if let Ok(bytes) = serde_json::to_vec(&state.model_routes()) {
        hasher.update(bytes);
    }
    if let Ok(bytes) = serde_json::to_vec(&state.subagent_settings()) {
        hasher.update(bytes);
    }
    format!("{:x}", hasher.finalize())
}

/// Production host: real Codex binary, lockfile, capability, and schema.
pub struct DesktopPrepareHost<'a> {
    pub state: &'a AppState,
    pub base_url: String,
}

impl PrepareHost for DesktopPrepareHost<'_> {
    fn executable_path(&self) -> Option<PathBuf> {
        crate::codex::local_codex_candidates()
            .into_iter()
            .find(|path| path.is_file())
    }

    fn lockfile_path(&self) -> Option<PathBuf> {
        None
    }

    fn packaged_lockfile_digest(&self) -> Option<String> {
        Some(packaged_lockfile_sha256())
    }

    fn protocol_version(&self) -> String {
        vellum_proxy_runtime::PROXY_RUNTIME_VERSION.to_string()
    }

    fn settings_fingerprint(&self) -> String {
        settings_fingerprint(self.state)
    }

    fn discover_capability(&self) -> Result<(), String> {
        crate::codex::ensure_vellum_provider_supported(&self.base_url)
            .map_err(|e| e.to_string())?;
        crate::codex::ensure_native_subagent_supported(&self.state.subagent_settings())
            .map_err(|e| e.to_string())
    }

    fn probe_schema(&self) -> Result<(), String> {
        let Some(binary) = self.executable_path() else {
            return Err("Codex runtime binary was not found".into());
        };
        crate::enhanced_runtime::protocol_compat::ProtocolSurface::probe(&binary)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn precompute_catalog(&self) -> Result<(), String> {
        // Prepare can outlive Stop. It must never overwrite the live catalog
        // or its restore snapshot outside the lifecycle transaction.
        Ok(())
    }
}

/// Test double that records which expensive stages actually ran.
#[cfg(test)]
pub struct CountingPrepareHost {
    pub executable: PathBuf,
    pub lockfile: PathBuf,
    pub protocol_version: String,
    pub fingerprint: String,
    pub capability_calls: AtomicU64,
    pub schema_calls: AtomicU64,
    pub catalog_calls: AtomicU64,
    pub fail_schema: bool,
}

#[cfg(test)]
impl CountingPrepareHost {
    pub fn new(dir: &Path) -> Self {
        let executable = dir.join("codex.bin");
        let lockfile = dir.join("runtime.lock");
        std::fs::write(&executable, b"codex-runtime-v1").unwrap();
        std::fs::write(&lockfile, b"lock-v1").unwrap();
        Self {
            executable,
            lockfile,
            protocol_version: "test-protocol".into(),
            fingerprint: "settings-v1".into(),
            capability_calls: AtomicU64::new(0),
            schema_calls: AtomicU64::new(0),
            catalog_calls: AtomicU64::new(0),
            fail_schema: false,
        }
    }
}

#[cfg(test)]
impl PrepareHost for CountingPrepareHost {
    fn executable_path(&self) -> Option<PathBuf> {
        Some(self.executable.clone())
    }
    fn lockfile_path(&self) -> Option<PathBuf> {
        Some(self.lockfile.clone())
    }
    fn protocol_version(&self) -> String {
        self.protocol_version.clone()
    }
    fn settings_fingerprint(&self) -> String {
        self.fingerprint.clone()
    }
    fn discover_capability(&self) -> Result<(), String> {
        self.capability_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn probe_schema(&self) -> Result<(), String> {
        self.schema_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_schema {
            Err("schema incompatible".into())
        } else {
            Ok(())
        }
    }
    fn precompute_catalog(&self) -> Result<(), String> {
        self.catalog_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_cancel() -> watch::Receiver<bool> {
        let (_tx, rx) = watch::channel(false);
        rx
    }

    #[tokio::test]
    async fn prepare_reuses_snapshot_only_while_identity_matches() {
        let dir = tempfile::tempdir().unwrap();
        let host = CountingPrepareHost::new(dir.path());
        let mut coordinator = PrepareCoordinator::new();
        let first = coordinator.prepare(&host, live_cancel()).await.unwrap();
        assert!(first.capability_ran && first.schema_ran && first.catalog_ran);
        assert_eq!(host.capability_calls.load(Ordering::SeqCst), 1);
        assert_eq!(host.schema_calls.load(Ordering::SeqCst), 1);

        let second = coordinator.prepare(&host, live_cancel()).await.unwrap();
        assert_eq!(host.capability_calls.load(Ordering::SeqCst), 1);
        assert_eq!(host.schema_calls.load(Ordering::SeqCst), 1);
        assert_eq!(host.catalog_calls.load(Ordering::SeqCst), 1);
        assert!(second.still_valid(&host));

        std::fs::write(&host.executable, b"codex-runtime-v2").unwrap();
        assert!(!second.still_valid(&host));
        let third = coordinator.prepare(&host, live_cancel()).await.unwrap();
        assert_eq!(host.capability_calls.load(Ordering::SeqCst), 2);
        assert_eq!(host.schema_calls.load(Ordering::SeqCst), 2);
        assert!(third.still_valid(&host));
    }

    #[tokio::test]
    async fn lockfile_or_protocol_change_forces_reprepare() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = CountingPrepareHost::new(dir.path());
        let mut coordinator = PrepareCoordinator::new();
        let first = coordinator.prepare(&host, live_cancel()).await.unwrap();
        std::fs::write(&host.lockfile, b"lock-v2").unwrap();
        assert!(!first.still_valid(&host));
        host.protocol_version = "other".into();
        std::fs::write(&host.lockfile, b"lock-v1").unwrap();
        assert!(!first.still_valid(&host));
    }

    #[tokio::test]
    async fn schema_failure_is_recorded_not_fatal_for_prepare() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = CountingPrepareHost::new(dir.path());
        host.fail_schema = true;
        let mut coordinator = PrepareCoordinator::new();
        let snapshot = coordinator.prepare(&host, live_cancel()).await.unwrap();
        assert!(!snapshot.schema_ok);
        assert!(snapshot.capability_ok);
        assert_eq!(host.schema_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelled_prepare_does_not_cache() {
        let dir = tempfile::tempdir().unwrap();
        let host = CountingPrepareHost::new(dir.path());
        let mut coordinator = PrepareCoordinator::new();
        let (tx, rx) = watch::channel(true);
        drop(tx);
        let err = coordinator.prepare(&host, rx).await.unwrap_err();
        assert!(err.to_string().contains("user stopped proxy"));
        assert!(coordinator.cached.is_none());
    }

    struct PackagedLockHost {
        inner: CountingPrepareHost,
        digest: Option<String>,
    }

    impl PrepareHost for PackagedLockHost {
        fn executable_path(&self) -> Option<PathBuf> {
            None
        }
        fn lockfile_path(&self) -> Option<PathBuf> {
            None
        }
        fn packaged_lockfile_digest(&self) -> Option<String> {
            self.digest.clone()
        }
        fn protocol_version(&self) -> String {
            self.inner.protocol_version.clone()
        }
        fn settings_fingerprint(&self) -> String {
            self.inner.fingerprint.clone()
        }
        fn discover_capability(&self) -> Result<(), String> {
            Ok(())
        }
        fn probe_schema(&self) -> Result<(), String> {
            Ok(())
        }
        fn precompute_catalog(&self) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn missing_packaged_lockfile_paths_are_not_a_match_when_digest_is_required() {
        let dir = tempfile::tempdir().unwrap();
        let host = PackagedLockHost {
            inner: CountingPrepareHost::new(dir.path()),
            digest: Some("abc123".into()),
        };
        let snapshot = PrepareSnapshot {
            executable: None,
            lockfile: None,
            packaged_lockfile_digest: None,
            protocol_version: host.protocol_version(),
            settings_fingerprint: host.settings_fingerprint(),
            capability_ok: true,
            schema_ok: true,
            schema_ran: true,
            capability_ran: true,
            catalog_ran: true,
            stages: vec![],
        };
        assert!(
            !snapshot.still_valid(&host),
            "(None, None) path match must not reuse prepare when the host has a packaged digest"
        );
    }

    #[test]
    fn packaged_lockfile_digest_change_invalidates_prepare() {
        let dir = tempfile::tempdir().unwrap();
        let host = PackagedLockHost {
            inner: CountingPrepareHost::new(dir.path()),
            digest: Some("abc123".into()),
        };
        let snapshot = PrepareSnapshot {
            executable: None,
            lockfile: None,
            packaged_lockfile_digest: Some("abc123".into()),
            protocol_version: host.protocol_version(),
            settings_fingerprint: host.settings_fingerprint(),
            capability_ok: true,
            schema_ok: true,
            schema_ran: true,
            capability_ran: true,
            catalog_ran: true,
            stages: vec![],
        };
        assert!(snapshot.still_valid(&host));
        let changed = PackagedLockHost {
            inner: CountingPrepareHost::new(dir.path()),
            digest: Some("def456".into()),
        };
        assert!(!snapshot.still_valid(&changed));
    }

    #[test]
    fn desktop_prepare_host_binds_packaged_lockfile_not_source_tree_path() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::with_test_fixtures(dir.path().to_path_buf());
        let host = DesktopPrepareHost {
            state: &state,
            base_url: "http://127.0.0.1:15721/v1".into(),
        };
        assert!(
            host.lockfile_path().is_none(),
            "installed Desktop has no CARGO_MANIFEST_DIR lockfile"
        );
        let digest = host
            .packaged_lockfile_digest()
            .expect("packaged digest required");
        assert_eq!(digest, packaged_lockfile_sha256());
    }
}
