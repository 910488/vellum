//! Codex Desktop replacing its own core underneath a launch manifest.
//!
//! The manifest pins the Official core by path and hash at the moment Vellum
//! prepared the launch. Desktop updates on its own schedule: on Windows it
//! unpacks a new `bin\<hash>\` directory and deletes the old executable once
//! nothing holds it; on macOS it overwrites `CodexCLI.app` in place. Either way
//! the next app-server Desktop starts reads a manifest that names a core that
//! is gone or different, and a bridge that simply exits there leaves Desktop
//! with no app-server at all ("cannot find Codex host" on Windows, "cannot load
//! organization settings" on macOS).
//!
//! The bridge therefore falls back to the core Desktop itself ships, run as
//! plain Official Codex with no Enhanced plane, and leaves a record here so
//! Vellum can rebuild the launch. That widens nothing: the fallback is the
//! binary Desktop would run if Vellum were not installed. The Enhanced core is
//! still only ever started from a verified manifest.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::atomic::write_atomic;
use super::launch_manifest::{sha256_file, LaunchManifestV1, RuntimeBinaryIdentity};

pub const OFFICIAL_DRIFT_FILE: &str = "official-drift.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OfficialDriftRecord {
    pub launch_id: String,
    pub observed_at: i64,
    pub recorded_executable: PathBuf,
    pub recorded_sha256: String,
    pub current_executable: PathBuf,
    pub current_sha256: String,
    pub reason: String,
}

impl OfficialDriftRecord {
    pub fn path_beside(attestation_path: &Path) -> Option<PathBuf> {
        attestation_path
            .parent()
            .map(|parent| parent.join(OFFICIAL_DRIFT_FILE))
    }

    /// The drift reported against `manifest`, if any. A record for an older
    /// launch was already answered by the launch that replaced it.
    pub fn for_launch(manifest: &LaunchManifestV1) -> Option<Self> {
        let path = Self::path_beside(&manifest.attestation_path)?;
        let record = serde_json::from_slice::<Self>(&std::fs::read(path).ok()?).ok()?;
        (record.launch_id == manifest.launch_id).then_some(record)
    }

    fn write_beside(&self, attestation_path: &Path) {
        if let (Some(path), Ok(bytes)) = (
            Self::path_beside(attestation_path),
            serde_json::to_vec_pretty(self),
        ) {
            let _ = write_atomic(&path, &bytes);
        }
    }
}

/// The Official core this manifest pinned, or Desktop's current one when the
/// pinned core no longer verifies. The flag says which.
pub fn resolve_official(
    manifest: &LaunchManifestV1,
) -> Result<(RuntimeBinaryIdentity, bool), super::LaunchManifestError> {
    resolve_official_with(manifest, desktop_official_candidates())
}

fn resolve_official_with(
    manifest: &LaunchManifestV1,
    candidates: Vec<PathBuf>,
) -> Result<(RuntimeBinaryIdentity, bool), super::LaunchManifestError> {
    let error = match manifest.official.verify_on_disk("official") {
        Ok(()) => return Ok((manifest.official.clone(), false)),
        Err(error) => error,
    };
    let Some((current, sha256)) = candidates
        .into_iter()
        .filter(|candidate| candidate.is_absolute() && candidate.is_file())
        .find_map(|candidate| sha256_file(&candidate).ok().map(|sha| (candidate, sha)))
    else {
        return Err(error);
    };
    OfficialDriftRecord {
        launch_id: manifest.launch_id.clone(),
        observed_at: chrono::Utc::now().timestamp(),
        recorded_executable: manifest.official.executable.clone(),
        recorded_sha256: manifest.official.artifact_sha256.clone(),
        current_executable: current.clone(),
        current_sha256: sha256.clone(),
        reason: error.to_string(),
    }
    .write_beside(&manifest.attestation_path);
    Ok((
        RuntimeBinaryIdentity {
            executable: current,
            runtime_digest: sha256.clone(),
            artifact_sha256: sha256,
            codex_home: manifest.official.codex_home.clone(),
        },
        true,
    ))
}

/// Only cores that belong to Codex Desktop itself, newest first. Package
/// manager CLIs are deliberately absent: a Homebrew `codex` is a node script
/// that a GUI-launched bridge cannot even run.
fn desktop_official_candidates() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|local| crate::codex::desktop_runtime_codex_candidates(&local))
            .unwrap_or_default()
    }
    #[cfg(target_os = "macos")]
    {
        crate::install_paths::macos_codex_desktop_cli_candidates(dirs::home_dir().as_deref())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enhanced_runtime::FeatureFlags;
    use std::fs;

    fn manifest(root: &Path, official: PathBuf) -> LaunchManifestV1 {
        let identity = |executable: PathBuf| RuntimeBinaryIdentity {
            artifact_sha256: sha256_file(&executable).unwrap(),
            runtime_digest: "sha256:digest".into(),
            codex_home: root.join("home"),
            executable,
        };
        let enhanced = root.join("enhanced.exe");
        fs::write(&enhanced, b"enhanced").unwrap();
        LaunchManifestV1 {
            schema_version: 1,
            launch_id: "01LAUNCH".into(),
            created_at: 1,
            official: identity(official),
            enhanced: identity(enhanced),
            model_provider_map_path: root.join("map.json"),
            model_provider_map_sha256: "sha256:map".into(),
            binding_db: root.join("bindings.sqlite3"),
            official_provider_ids: vec!["openai-official".into()],
            third_party_provider_ids: Vec::new(),
            feature_profile: FeatureFlags::default(),
            enhanced_commit: "commit".into(),
            attestation_path: root.join("bridge-attestation.json"),
            qualification_journal_path: root.join("journal.jsonl"),
            relay: None,
        }
    }

    #[test]
    fn a_verified_core_is_used_as_pinned_and_leaves_no_record() {
        let temp = tempfile::tempdir().unwrap();
        let official = temp.path().join("old").join("codex.exe");
        fs::create_dir_all(official.parent().unwrap()).unwrap();
        fs::write(&official, b"official v1").unwrap();
        let manifest = manifest(temp.path(), official.clone());

        let (resolved, drifted) = resolve_official_with(&manifest, Vec::new()).unwrap();
        assert!(!drifted);
        assert_eq!(resolved.executable, official);
        assert!(OfficialDriftRecord::for_launch(&manifest).is_none());
    }

    #[test]
    fn a_deleted_core_falls_back_to_desktops_current_one() {
        let temp = tempfile::tempdir().unwrap();
        let old = temp.path().join("bin").join("old").join("codex.exe");
        let new = temp.path().join("bin").join("new").join("codex.exe");
        for (path, body) in [(&old, "v1"), (&new, "v2")] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        let manifest = manifest(temp.path(), old.clone());
        fs::remove_file(&old).unwrap();

        let (resolved, drifted) =
            resolve_official_with(&manifest, vec![old.clone(), new.clone()]).unwrap();
        assert!(drifted);
        assert_eq!(resolved.executable, new);
        assert_eq!(resolved.artifact_sha256, sha256_file(&new).unwrap());
        assert_eq!(resolved.codex_home, manifest.official.codex_home);
        let record = OfficialDriftRecord::for_launch(&manifest).unwrap();
        assert_eq!(record.recorded_executable, old);
        assert_eq!(record.current_executable, new);
    }

    #[test]
    fn a_core_overwritten_in_place_is_rehashed_not_refused() {
        let temp = tempfile::tempdir().unwrap();
        let official = temp.path().join("CodexCLI");
        fs::write(&official, b"v1").unwrap();
        let manifest = manifest(temp.path(), official.clone());
        fs::write(&official, b"v2").unwrap();

        let (resolved, drifted) =
            resolve_official_with(&manifest, vec![official.clone()]).unwrap();
        assert!(drifted);
        assert_eq!(resolved.executable, official);
        assert_ne!(resolved.artifact_sha256, manifest.official.artifact_sha256);
    }

    #[test]
    fn no_desktop_core_keeps_the_original_failure() {
        let temp = tempfile::tempdir().unwrap();
        let official = temp.path().join("codex.exe");
        fs::write(&official, b"v1").unwrap();
        let manifest = manifest(temp.path(), official.clone());
        fs::remove_file(&official).unwrap();

        assert!(resolve_official_with(&manifest, vec![temp.path().join("missing")]).is_err());
        assert!(OfficialDriftRecord::for_launch(&manifest).is_none());
    }

    #[test]
    fn a_record_for_an_older_launch_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let official = temp.path().join("codex.exe");
        fs::write(&official, b"v1").unwrap();
        let mut manifest = manifest(temp.path(), official.clone());
        fs::write(&official, b"v2").unwrap();
        resolve_official_with(&manifest, vec![official]).unwrap();

        manifest.launch_id = "01NEWER".into();
        assert!(OfficialDriftRecord::for_launch(&manifest).is_none());
    }
}
