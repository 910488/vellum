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
use super::launch_manifest::{
    missing_official_helpers, sha256_file, LaunchManifestV1, RuntimeBinaryIdentity,
};

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

/// [`resolve_official`] for a bridge Codex Desktop itself started, which can
/// see the core that Desktop ships.
///
/// Desktop copies its core into `bin\<hash>\` only some time after it starts,
/// so right after an update the newest copy there is still the pinned one and
/// the new Desktop's first App Server got a core older than the Desktop. While
/// Desktop is held at a startup dialog it never makes that copy at all. The
/// core Desktop ships is the one it would run without Vellum, so one that
/// nobody approved for this launch is served as plain Official instead of the
/// pin.
pub fn resolve_official_for_desktop(
    manifest: &LaunchManifestV1,
    desktop: Option<&Path>,
) -> Result<(RuntimeBinaryIdentity, OfficialSource), super::LaunchManifestError> {
    if let Some(chosen) = desktop
        .and_then(shipped_core)
        .and_then(|shipped| choose_shipped(manifest, &shipped))
    {
        return Ok(chosen);
    }
    resolve_official(manifest)
}

/// [`shipped_core`], when the file is there.
pub fn desktop_shipped_core(desktop: &Path) -> Option<PathBuf> {
    shipped_core(desktop).filter(|core| core.is_absolute() && core.is_file())
}

/// The core a Codex Desktop executable ships beside it: the Store package has
/// `app\ChatGPT.exe` and `app\resources\codex.exe`.
#[cfg(windows)]
fn shipped_core(desktop: &Path) -> Option<PathBuf> {
    Some(desktop.parent()?.join("resources").join("codex.exe"))
}

/// macOS overwrites the core in place, which the pin's hash check already
/// catches; there is no stale copy for Desktop to be served from.
#[cfg(not(windows))]
fn shipped_core(_desktop: &Path) -> Option<PathBuf> {
    None
}

/// Desktop's shipped core when it is not the pin, or `None` to leave the
/// choice to [`resolve_official`].
fn choose_shipped(
    manifest: &LaunchManifestV1,
    shipped: &Path,
) -> Option<(RuntimeBinaryIdentity, OfficialSource)> {
    if !shipped.is_absolute() || !shipped.is_file() {
        return None;
    }
    let sha256 = sha256_file(shipped).ok()?;
    if sha256 == manifest.official.artifact_sha256 {
        return None;
    }
    if !missing_official_helpers(shipped, &expected_helpers(manifest, &sha256)).is_empty() {
        return None;
    }
    if manifest.accepts_official_successor(&sha256) {
        return Some((
            official_identity(manifest, shipped.to_path_buf(), sha256),
            OfficialSource::Successor,
        ));
    }
    record_drift(
        manifest,
        shipped,
        &sha256,
        "Codex Desktop ships an Official core this launch never checked".into(),
    );
    Some((
        official_identity(manifest, shipped.to_path_buf(), sha256),
        OfficialSource::Drifted,
    ))
}

pub(crate) fn observe_official(
    manifest: &LaunchManifestV1,
) -> Result<(RuntimeBinaryIdentity, OfficialSource), super::LaunchManifestError> {
    resolve_official_inner(manifest, desktop_official_candidates(), false)
}

/// Whether a bridge started now would have to serve this launch as plain
/// Official. Asks the same question as [`resolve_official`] without leaving a
/// drift record: Vellum asking is not a bridge having fallen back.
pub fn would_drift(manifest: &LaunchManifestV1) -> bool {
    observe_official(manifest).map_or(true, |(_, source)| source == OfficialSource::Drifted)
}

/// A recycle must improve the running bridge, never respawn into the same
/// incomplete directory. Errors are unsafe, just like missing helpers.
pub fn launch_is_complete(manifest: &LaunchManifestV1) -> bool {
    observe_official(manifest).is_ok_and(|(official, _)| {
        missing_official_helpers(
            &official.executable,
            &expected_helpers(manifest, &official.artifact_sha256),
        )
        .is_empty()
    })
}

pub fn expected_helpers(manifest: &LaunchManifestV1, sha256: &str) -> Vec<String> {
    let mut expected = manifest.official_helpers.clone();
    if let Some(successor) = manifest
        .official_successors
        .iter()
        .find(|successor| successor.artifact_sha256 == sha256)
    {
        expected.extend(successor.helpers.clone());
    }
    expected.sort();
    expected.dedup();
    expected
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
    let verification = manifest.official.verify_on_disk("official");
    let incomplete =
        missing_official_helpers(&manifest.official.executable, &manifest.official_helpers);
    if verification.is_ok() && incomplete.is_empty() {
        return Ok((manifest.official.clone(), OfficialSource::Pinned));
    }
    if let Some(successor) = candidates
        .iter()
        .find_map(|candidate| approved_successor(manifest, candidate))
    {
        return Ok((successor, OfficialSource::Successor));
    }
    let replacement = candidates.into_iter().find_map(|candidate| {
        let sha = sha256_file(&candidate).ok()?;
        missing_official_helpers(&candidate, &expected_helpers(manifest, &sha))
            .is_empty()
            .then_some((candidate, sha))
    });
    // Keep serving a hash-verified pin when there is no complete replacement.
    // The bridge records the missing helpers; Vellum can converge later.
    let Some((current, sha256)) = replacement else {
        return verification.map(|()| (manifest.official.clone(), OfficialSource::Pinned));
    };
    let error = verification
        .err()
        .map(|error| error.to_string())
        .unwrap_or_else(|| format!("Official core missing helpers: {}", incomplete.join(", ")));
    if record {
        record_drift(manifest, &current, &sha256, error);
    }
    Ok((
        official_identity(manifest, current, sha256),
        OfficialSource::Drifted,
    ))
}

fn record_drift(manifest: &LaunchManifestV1, current: &Path, sha256: &str, reason: String) {
    OfficialDriftRecord {
        launch_id: manifest.launch_id.clone(),
        observed_at: chrono::Utc::now().timestamp(),
        recorded_executable: manifest.official.executable.clone(),
        recorded_sha256: manifest.official.artifact_sha256.clone(),
        current_executable: current.to_path_buf(),
        current_sha256: sha256.to_owned(),
        reason,
    }
    .write_beside(&manifest.attestation_path);
}

fn approved_successor(
    manifest: &LaunchManifestV1,
    candidate: &Path,
) -> Option<RuntimeBinaryIdentity> {
    let sha256 = sha256_file(candidate).ok()?;
    if !missing_official_helpers(candidate, &expected_helpers(manifest, &sha256)).is_empty() {
        return None;
    }
    (manifest.accepts_official_successor(&sha256) || sha256 == manifest.official.artifact_sha256)
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
            official_helpers: Vec::new(),
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
            helpers: super::super::launch_manifest::official_helpers(path),
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

    fn helper_fixture(root: &Path) -> (LaunchManifestV1, PathBuf, PathBuf) {
        let old = root.join("old/codex.exe");
        let new = root.join("new/codex.exe");
        write_all(&[(&old, "v1"), (&new, "v2")]);
        let helper = old.parent().unwrap().join("codex-code-mode-host.exe");
        fs::write(&helper, "host").unwrap();
        let mut manifest = manifest(root, old);
        manifest.official_helpers =
            super::super::launch_manifest::official_helpers(&manifest.official.executable);
        (manifest, helper, new)
    }

    #[test]
    fn a_complete_pin_with_recorded_helpers_stays_pinned() {
        let temp = tempfile::tempdir().unwrap();
        let (manifest, _, _) = helper_fixture(temp.path());
        let (_, source) = resolve_official_with(&manifest, Vec::new()).unwrap();
        assert_eq!(source, OfficialSource::Pinned);
        assert!(missing_official_helpers(
            &manifest.official.executable,
            &manifest.official_helpers
        )
        .is_empty());
    }

    #[test]
    fn a_hash_valid_incomplete_pin_uses_a_complete_successor() {
        let temp = tempfile::tempdir().unwrap();
        let (mut manifest, helper, new) = helper_fixture(temp.path());
        fs::remove_file(helper).unwrap();
        let new_helper = new
            .parent()
            .unwrap()
            .join("resources/codex-code-mode-host.exe");
        fs::create_dir_all(new_helper.parent().unwrap()).unwrap();
        fs::write(new_helper, "host").unwrap();
        manifest.official_successors.push(successor(&new));
        let (official, source) = resolve_official_with(&manifest, vec![new.clone()]).unwrap();
        assert_eq!(source, OfficialSource::Successor);
        assert_eq!(official.executable, new);
    }

    #[test]
    fn an_incomplete_pin_without_a_complete_replacement_keeps_its_missing_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let (manifest, helper, _) = helper_fixture(temp.path());
        fs::remove_file(helper).unwrap();
        let (official, source) = resolve_official_with(&manifest, Vec::new()).unwrap();
        assert_eq!(source, OfficialSource::Pinned);
        assert_eq!(
            missing_official_helpers(&official.executable, &manifest.official_helpers),
            vec!["codex-code-mode-host.exe"]
        );
        assert!(OfficialDriftRecord::for_launch(&manifest).is_none());
    }

    #[test]
    fn a_successor_still_unpacking_its_helpers_is_never_selected() {
        let temp = tempfile::tempdir().unwrap();
        let (mut manifest, helper, new) = helper_fixture(temp.path());
        manifest.official_successors.push(successor(&new));
        fs::remove_file(helper).unwrap();
        let (official, source) = resolve_official_with(&manifest, vec![new]).unwrap();
        assert_eq!(source, OfficialSource::Pinned);
        assert_eq!(official, manifest.official);
    }

    #[test]
    fn an_incomplete_pin_can_use_a_complete_desktop_core_as_plain_official() {
        let temp = tempfile::tempdir().unwrap();
        let (manifest, helper, new) = helper_fixture(temp.path());
        fs::remove_file(helper).unwrap();
        fs::write(
            new.parent().unwrap().join("codex-code-mode-host.exe"),
            "host",
        )
        .unwrap();
        let (official, source) = resolve_official_with(&manifest, vec![new.clone()]).unwrap();
        assert_eq!(source, OfficialSource::Drifted);
        assert_eq!(official.executable, new);
    }

    #[test]
    fn a_complete_copy_with_the_same_digest_remains_trusted() {
        let temp = tempfile::tempdir().unwrap();
        let (manifest, helper, new) = helper_fixture(temp.path());
        fs::copy(&manifest.official.executable, &new).unwrap();
        fs::remove_file(helper).unwrap();
        fs::write(
            new.parent().unwrap().join("codex-code-mode-host.exe"),
            "host",
        )
        .unwrap();
        let (official, source) = resolve_official_with(&manifest, vec![new.clone()]).unwrap();
        assert_eq!(source, OfficialSource::Successor);
        assert_eq!(official.executable, new);
        assert_eq!(official.artifact_sha256, manifest.official.artifact_sha256);
        assert!(OfficialDriftRecord::for_launch(&manifest).is_none());
    }

    #[test]
    fn desktops_own_unchecked_core_displaces_a_pin_that_survived_the_update() {
        let temp = tempfile::tempdir().unwrap();
        let (manifest, _, _) = helper_fixture(temp.path());
        let shipped = temp.path().join("app/resources/codex.exe");
        write_all(&[
            (&shipped, "v2"),
            (
                &shipped.parent().unwrap().join("codex-code-mode-host.exe"),
                "host",
            ),
        ]);
        // The pin is intact and complete, which on its own keeps it pinned.
        assert_eq!(
            resolve_official_with(&manifest, Vec::new()).unwrap().1,
            OfficialSource::Pinned
        );

        let (official, source) = choose_shipped(&manifest, &shipped).unwrap();
        assert_eq!(source, OfficialSource::Drifted);
        assert_eq!(official.executable, shipped);
        assert_eq!(official.artifact_sha256, sha256_file(&shipped).unwrap());
        assert_eq!(official.codex_home, manifest.official.codex_home);
        let record = OfficialDriftRecord::for_launch(&manifest).unwrap();
        assert_eq!(record.recorded_executable, manifest.official.executable);
        assert_eq!(record.current_executable, shipped);
    }

    #[test]
    fn desktops_own_approved_core_runs_as_a_successor() {
        let temp = tempfile::tempdir().unwrap();
        let (mut manifest, _, _) = helper_fixture(temp.path());
        let shipped = temp.path().join("app/resources/codex.exe");
        write_all(&[
            (&shipped, "v2"),
            (
                &shipped.parent().unwrap().join("codex-code-mode-host.exe"),
                "host",
            ),
        ]);
        manifest.official_successors.push(successor(&shipped));

        let (official, source) = choose_shipped(&manifest, &shipped).unwrap();
        assert_eq!(source, OfficialSource::Successor);
        assert_eq!(official.executable, shipped);
        assert!(OfficialDriftRecord::for_launch(&manifest).is_none());
    }

    #[test]
    fn desktops_own_core_defers_when_it_is_the_pin_incomplete_or_absent() {
        let temp = tempfile::tempdir().unwrap();
        let (manifest, _, _) = helper_fixture(temp.path());
        let same = temp.path().join("same/app/resources/codex.exe");
        let incomplete = temp.path().join("incomplete/app/resources/codex.exe");
        write_all(&[(&same, "v1"), (&incomplete, "v2")]);
        let same_host = same.parent().unwrap().join("codex-code-mode-host.exe");
        fs::write(same_host, "host").unwrap();

        assert!(
            choose_shipped(&manifest, &same).is_none(),
            "already the pin"
        );
        assert!(
            choose_shipped(&manifest, &incomplete).is_none(),
            "missing codex-code-mode-host.exe"
        );
        assert!(choose_shipped(&manifest, &temp.path().join("absent/codex.exe")).is_none());
        assert!(choose_shipped(&manifest, Path::new("codex.exe")).is_none());
        assert!(OfficialDriftRecord::for_launch(&manifest).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn the_shipped_core_sits_in_resources_beside_desktop() {
        let temp = tempfile::tempdir().unwrap();
        let (manifest, _, _) = helper_fixture(temp.path());
        let desktop = temp
            .path()
            .join("OpenAI.Codex_26.1021.0.0_x64/app/ChatGPT.exe");
        let shipped = desktop.parent().unwrap().join("resources/codex.exe");
        write_all(&[
            (&desktop, "desktop"),
            (&shipped, "v2"),
            (
                &shipped.parent().unwrap().join("codex-code-mode-host.exe"),
                "host",
            ),
        ]);

        let (official, source) =
            resolve_official_for_desktop(&manifest, Some(desktop.as_path())).unwrap();
        assert_eq!(source, OfficialSource::Drifted);
        assert_eq!(official.executable, shipped);
        assert_eq!(desktop_shipped_core(&desktop), Some(shipped));
        assert_eq!(
            desktop_shipped_core(&temp.path().join("elsewhere/app/ChatGPT.exe")),
            None
        );
    }

    #[test]
    fn successor_specific_helpers_are_required_even_when_the_pin_has_fewer() {
        let temp = tempfile::tempdir().unwrap();
        let (mut manifest, _, new) = helper_fixture(temp.path());
        let host = new.parent().unwrap().join("codex-code-mode-host.exe");
        let extra = new.parent().unwrap().join("codex-extra.exe");
        fs::write(host, "host").unwrap();
        fs::write(&extra, "extra").unwrap();
        manifest.official_successors.push(successor(&new));
        fs::remove_file(extra).unwrap();
        let (official, source) = resolve_official_with(&manifest, vec![new]).unwrap();
        assert_eq!(source, OfficialSource::Pinned);
        assert_eq!(official, manifest.official);
    }
}
