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
//! Most updates are known in advance: the Store stages the next package long
//! before Desktop restarts into it. Vellum checks that core against the launch
//! and lists it in the manifest as an approved successor, and the bridge then
//! runs it with the full Enhanced plane as though it had been pinned.
//!
//! Anything else falls back to the core Desktop itself ships, run as plain
//! Official Codex with no Enhanced plane, and leaves a record here so Vellum can
//! rebuild the launch. That widens nothing: the fallback is the binary Desktop
//! would run if Vellum were not installed. The Enhanced core is still only ever
//! started beside a core Vellum verified.

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

/// Which core [`resolve_official`] chose, and on what authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficialSource {
    /// The core this launch was prepared with.
    Pinned,
    /// A core Vellum checked against this launch ahead of Desktop's update. It
    /// runs with the full Enhanced plane, exactly like the pinned one.
    Successor,
    /// Desktop's current core, which nobody checked. Served as plain Official
    /// until Vellum rebuilds the launch.
    Drifted,
}

/// The Official core this launch should run, and which kind of choice it was.
pub fn resolve_official(
    manifest: &LaunchManifestV1,
) -> Result<(RuntimeBinaryIdentity, OfficialSource), super::LaunchManifestError> {
    resolve_official_with(manifest, desktop_official_candidates())
}

/// Whether a bridge started now would have to serve this launch as plain
/// Official. Asks the same question as [`resolve_official`] without leaving a
/// drift record: Vellum asking is not a bridge having fallen back.
pub fn would_drift(manifest: &LaunchManifestV1) -> bool {
    resolve_official_inner(manifest, desktop_official_candidates(), false)
        .is_ok_and(|(_, source)| source == OfficialSource::Drifted)
}

fn resolve_official_with(
    manifest: &LaunchManifestV1,
    candidates: Vec<PathBuf>,
) -> Result<(RuntimeBinaryIdentity, OfficialSource), super::LaunchManifestError> {
    resolve_official_inner(manifest, candidates, true)
}

fn resolve_official_inner(
    manifest: &LaunchManifestV1,
    candidates: Vec<PathBuf>,
    record: bool,
) -> Result<(RuntimeBinaryIdentity, OfficialSource), super::LaunchManifestError> {
    let candidates = candidates
        .into_iter()
        .filter(|candidate| candidate.is_absolute() && candidate.is_file())
        .collect::<Vec<_>>();
    // Desktop's current core comes first. An approved successor wins even while
    // the pinned file is still on disk: after an update the old directory only
    // survives until Desktop's cleanup, which deletes the helpers beside the old
    // core (code mode host, sandbox setup) but not an executable a launch holds.
    if let Some(current) = candidates.first() {
        if !same_file(current, &manifest.official.executable) {
            if let Some(successor) = approved_successor(manifest, current) {
                return Ok((successor, OfficialSource::Successor));
            }
        }
    }
    let error = match manifest.official.verify_on_disk("official") {
        Ok(()) => return Ok((manifest.official.clone(), OfficialSource::Pinned)),
        Err(error) => error,
    };
    if let Some(successor) = candidates
        .iter()
        .find_map(|candidate| approved_successor(manifest, candidate))
    {
        return Ok((successor, OfficialSource::Successor));
    }
    let Some((current, sha256)) = candidates
        .into_iter()
        .find_map(|candidate| sha256_file(&candidate).ok().map(|sha| (candidate, sha)))
    else {
        return Err(error);
    };
    if record {
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
    }
    Ok((
        official_identity(manifest, current, sha256),
        OfficialSource::Drifted,
    ))
}

fn approved_successor(
    manifest: &LaunchManifestV1,
    candidate: &Path,
) -> Option<RuntimeBinaryIdentity> {
    if manifest.official_successors.is_empty() {
        return None;
    }
    let sha256 = sha256_file(candidate).ok()?;
    manifest
        .accepts_official_successor(&sha256)
        .then(|| official_identity(manifest, candidate.to_path_buf(), sha256))
}

/// An Official core's runtime digest is its file digest; see
/// `desktop_manager::verify_settings`.
fn official_identity(
    manifest: &LaunchManifestV1,
    executable: PathBuf,
    sha256: String,
) -> RuntimeBinaryIdentity {
    RuntimeBinaryIdentity {
        executable,
        runtime_digest: sha256.clone(),
        artifact_sha256: sha256,
        codex_home: manifest.official.codex_home.clone(),
    }
}

fn same_file(left: &Path, right: &Path) -> bool {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(left) == canonical(right)
}

/// Cores Desktop might move to next: a version the Store has staged but not yet
/// registered, then the ones Desktop already unpacked. Vellum checks these
/// against a launch before Desktop starts any of them, so an update can find
/// its core already approved instead of drifting.
pub fn successor_candidates() -> Vec<PathBuf> {
    let mut candidates = staged_desktop_cores();
    candidates.extend(desktop_official_candidates());
    candidates
}

/// A Store update stages the whole new package beside the running one, long
/// before Desktop restarts into it, and its core runs from there as is.
#[cfg(windows)]
fn staged_desktop_cores() -> Vec<PathBuf> {
    /// How many staged versions are worth checking. Older ones are leftovers
    /// the Store has not removed yet; nothing will start them again.
    const NEWEST: usize = 2;
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => "x64",
    };
    let suffix = format!("_{arch}__2p2nqsd0c76g0");
    let Some(root) = std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .map(|root| PathBuf::from(root).join("WindowsApps"))
    else {
        return Vec::new();
    };
    let mut staged = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let version = name
                .strip_prefix("OpenAI.Codex_")?
                .strip_suffix(suffix.as_str())?
                .split('.')
                .map(|part| part.parse::<u64>().ok())
                .collect::<Option<Vec<_>>>()?;
            let core = entry.path().join("app").join("resources").join("codex.exe");
            core.is_file().then_some((version, core))
        })
        .collect::<Vec<_>>();
    staged.sort_by(|left, right| right.0.cmp(&left.0));
    staged
        .into_iter()
        .take(NEWEST)
        .map(|(_, core)| core)
        .collect()
}

#[cfg(not(windows))]
fn staged_desktop_cores() -> Vec<PathBuf> {
    Vec::new()
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
            enhanced: Some(identity(enhanced)),
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
            official_successors: Vec::new(),
        }
    }

    #[test]
    fn a_verified_core_is_used_as_pinned_and_leaves_no_record() {
        let temp = tempfile::tempdir().unwrap();
        let official = temp.path().join("old").join("codex.exe");
        fs::create_dir_all(official.parent().unwrap()).unwrap();
        fs::write(&official, b"official v1").unwrap();
        let manifest = manifest(temp.path(), official.clone());

        let (resolved, source) = resolve_official_with(&manifest, Vec::new()).unwrap();
        assert_eq!(source, OfficialSource::Pinned);
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

        let (resolved, source) =
            resolve_official_with(&manifest, vec![old.clone(), new.clone()]).unwrap();
        assert_eq!(source, OfficialSource::Drifted);
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
        // Force the same timestamp regardless of the filesystem's resolution.
        // A future timestamp keeps the racy window open even on a slow runner.
        let modified = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        let set_modified = || {
            fs::OpenOptions::new()
                .write(true)
                .open(&official)
                .unwrap()
                .set_modified(modified)
                .unwrap();
        };
        set_modified();
        let before = fs::metadata(&official).unwrap();
        let manifest = manifest(temp.path(), official.clone());
        fs::write(&official, b"v2").unwrap();
        set_modified();
        let after = fs::metadata(&official).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());

        let (resolved, source) = resolve_official_with(&manifest, vec![official.clone()]).unwrap();
        assert_eq!(source, OfficialSource::Drifted);
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

    fn successor(path: &Path) -> crate::enhanced_runtime::launch_manifest::OfficialSuccessor {
        crate::enhanced_runtime::launch_manifest::OfficialSuccessor {
            artifact_sha256: sha256_file(path).unwrap(),
            source: path.to_path_buf(),
        }
    }

    fn write_all(files: &[(&PathBuf, &str)]) {
        for (path, body) in files {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
    }

    #[test]
    fn an_approved_update_runs_as_a_successor_even_while_the_old_core_survives() {
        let temp = tempfile::tempdir().unwrap();
        let old = temp.path().join("bin").join("old").join("codex.exe");
        let new = temp.path().join("bin").join("new").join("codex.exe");
        let staged = temp.path().join("WindowsApps").join("codex.exe");
        write_all(&[(&old, "v1"), (&new, "v2"), (&staged, "v2")]);
        let mut manifest = manifest(temp.path(), old.clone());
        manifest.official_successors.push(successor(&staged));

        let (resolved, source) =
            resolve_official_with(&manifest, vec![new.clone(), old.clone()]).unwrap();
        assert_eq!(source, OfficialSource::Successor);
        assert_eq!(resolved.executable, new);
        assert_eq!(resolved.runtime_digest, resolved.artifact_sha256);
        assert_eq!(resolved.codex_home, manifest.official.codex_home);
        assert!(OfficialDriftRecord::for_launch(&manifest).is_none());
    }

    #[test]
    fn an_unapproved_newer_core_does_not_displace_a_verified_pin() {
        let temp = tempfile::tempdir().unwrap();
        let old = temp.path().join("bin").join("old").join("codex.exe");
        let new = temp.path().join("bin").join("new").join("codex.exe");
        let other = temp.path().join("other.exe");
        write_all(&[(&old, "v1"), (&new, "v2"), (&other, "v3")]);
        let mut manifest = manifest(temp.path(), old.clone());
        manifest.official_successors.push(successor(&other));

        let (resolved, source) =
            resolve_official_with(&manifest, vec![new.clone(), old.clone()]).unwrap();
        assert_eq!(source, OfficialSource::Pinned);
        assert_eq!(resolved.executable, old);
    }

    #[test]
    fn a_deleted_pin_prefers_any_approved_successor_over_drift() {
        let temp = tempfile::tempdir().unwrap();
        let old = temp.path().join("bin").join("old").join("codex.exe");
        let unknown = temp.path().join("bin").join("unknown").join("codex.exe");
        let approved = temp.path().join("bin").join("approved").join("codex.exe");
        write_all(&[(&old, "v1"), (&unknown, "v9"), (&approved, "v2")]);
        let mut manifest = manifest(temp.path(), old.clone());
        manifest.official_successors.push(successor(&approved));
        fs::remove_file(&old).unwrap();

        let (resolved, source) =
            resolve_official_with(&manifest, vec![unknown, approved.clone()]).unwrap();
        assert_eq!(source, OfficialSource::Successor);
        assert_eq!(resolved.executable, approved);
        assert!(OfficialDriftRecord::for_launch(&manifest).is_none());
    }
}
