use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use vellum_enhanced_codex::EnhancedRuntimeLockFile;

use crate::model::{ModelRoute, ProviderKind, Route};

use super::attestation::BridgeAttestationV1;
use super::env_lease::{self, ReleaseOutcome};
use super::launch_manifest::{
    sha256_file, LaunchManifestV1, RuntimeBinaryIdentity, ATTESTATION_FILE,
    LAUNCH_MANIFEST_SCHEMA_VERSION, QUALIFICATION_JOURNAL_FILE,
};
use super::protocol_compat::{
    self, ProtocolCompatibility, ProtocolDelta, ProtocolSurface, ProtocolVerdict,
};
use super::qualification::QualificationResult;
use super::{EnhancedRuntimeManifest, TrustedModelProviderMap, FEATURE_DEFAULTS_MVP};
use super::{ExecutionPlane, ThreadRuntimeBindingStore};

const SETTINGS_FILE: &str = "enhanced-runtime/desktop.json";
const MODEL_MAP_FILE: &str = "enhanced-runtime/model-provider-map.json";
const BINDING_DB_FILE: &str = "enhanced-runtime/thread-bindings.sqlite3";
/// Where a managed Enhanced core is kept, keyed by the artifact digest so an
/// install for one pinned release can never be mistaken for another's.
const MANAGED_CORE_DIR: &str = "enhanced-runtime/core";
/// Packaged lockfile bytes. Desktop prepare reuse hashes this same
/// `include_bytes` artifact — an installed app has no source-tree path.
pub(crate) const LOCK_BYTES: &[u8] = include_bytes!("../../../enhanced-runtime.lock.json");
/// Name of the packaged stdio bridge that ships beside the Vellum executable.
pub const BRIDGE_EXECUTABLE_STEM: &str = "vellum-codex-app-server";

/// Resolve hashed Codex conversation keys to their durable execution plane.
/// Missing or unreadable metadata stays unknown; callers must not infer that
/// a third-party route was adopted merely because the provider is third-party.
pub(crate) fn thread_execution_planes(data_root: &Path) -> HashMap<String, ExecutionPlane> {
    let path = data_root.join(BINDING_DB_FILE);
    if !path.exists() {
        return HashMap::new();
    }
    let Ok(store) = ThreadRuntimeBindingStore::open(path) else {
        return HashMap::new();
    };
    let Ok(bindings) = store.list() else {
        return HashMap::new();
    };
    let mut planes = HashMap::new();
    for binding in bindings {
        for raw in [&binding.thread_id, &binding.native_thread_id] {
            if let Some(key) = crate::history::conversation_key_from_raw(Some(raw)) {
                planes.insert(key, binding.plane);
            }
        }
    }
    planes
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DesktopRuntimeSettings {
    enabled: bool,
    official_codex_executable: PathBuf,
    enhanced_codex_executable: PathBuf,
    bridge_executable: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopRuntimeStatus {
    pub configured: bool,
    /// The user asked for Enhanced and the settings verified.
    pub enabled: bool,
    /// The pinned Enhanced artifact, protocol hash, and model map verified.
    /// This is a fact about files on disk and says nothing about Codex Desktop.
    pub artifact_ready: bool,
    /// Codex Desktop is currently running through this launch's bridge, with
    /// both children initialized. Proven by the bridge's own attestation.
    pub active: bool,
    /// A live bridge with Codex Desktop as parent was observed, even if one
    /// child is still starting or has failed and `active` is therefore false.
    pub bridge_observed: bool,
    /// The only value the UI should treat as "Enhanced is running".
    pub ready: bool,
    /// Product-facing lifecycle derived from desired configuration, lease
    /// ownership, and observed Desktop adoption.
    pub activation_state: String,
    /// Whether CODEX_CLI_PATH is owned, cleanly released, foreign, or an
    /// orphaned Vellum bridge left by an older build.
    pub environment_state: String,
    /// What CODEX_CLI_PATH currently names. The state word alone cannot be
    /// checked by hand; the path can.
    pub environment_value: Option<String>,
    /// The executable the live bridge process is actually running from, which
    /// is not always the configured one — an older bridge can outlive the
    /// build that started it.
    pub observed_bridge_executable: Option<PathBuf>,
    /// The desired and observed execution planes differ. Restarting Codex
    /// Desktop is required before the requested state is true in practice.
    pub restart_required: bool,
    pub launch_id: Option<String>,
    pub bridge_state: Option<String>,
    pub bridge_pid: Option<u32>,
    pub official_child_pid: Option<u32>,
    pub enhanced_child_pid: Option<u32>,
    pub enhanced_runtime_digest: Option<String>,
    pub active_runtime_digest: Option<String>,
    pub active_feature_profile: Option<String>,
    pub official_codex_executable: Option<PathBuf>,
    pub enhanced_codex_executable: Option<PathBuf>,
    pub bridge_executable: Option<PathBuf>,
    /// Whether this install can find the Enhanced core at all. It is not a
    /// user-chosen path, so "missing" is a property of the build, not of
    /// something the user forgot to do -- and the UI has to say that
    /// differently from "you have not picked a file yet".
    pub core_available: bool,
    /// How the Enhanced core's protocol compares to the Codex Desktop found on
    /// this machine. `None` only when the comparison could not be run at all
    /// (a binary is missing, or its schema probe failed) — which is different
    /// from a comparison that ran and found problems, and the UI says so.
    pub protocol: Option<ProtocolCompatibility>,
    /// Codex Desktop is going through the Enhanced core right now, whatever
    /// launch that bridge belongs to. A bridge left from an earlier launch is
    /// still serving every turn; only `active` additionally requires that it
    /// be serving this launch's configuration.
    pub serving: bool,
    /// Enhanced is running, or would run, against a Codex Desktop nobody has
    /// qualified this pairing against. Everything works as far as the routed
    /// protocol goes; the differences in `protocol.deltas` are real and
    /// unproven, so this is the state that has to be labelled rather than the
    /// clean fallback, where plain Codex is simply itself.
    pub unverified: bool,
    pub last_qualification: Option<QualificationResult>,
    /// Helper executables the official Codex install has beside its own binary
    /// but the Enhanced core does not. Not a blocker: chat and routing work
    /// without them. Sandboxed shell commands do not.
    pub missing_helpers: Vec<String>,
    /// The live launch is running a core that the stored settings no longer
    /// name -- Codex Desktop replaced its own install underneath it. Kept as
    /// its own field rather than left as prose in `blockers` because Vellum
    /// repairs this one itself, and a repair needs a condition it can test.
    pub launch_core_drift: bool,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DesktopRuntimeLaunch {
    pub bridge_executable: PathBuf,
    pub launch_id: String,
    pub manifest_path: PathBuf,
    pub attestation_path: PathBuf,
}

/// SHA-256 of the bridge sidecar this build shipped, recorded by `build.rs`.
///
/// Empty for a plain `cargo build`, which stages no sidecar; a packaged release
/// always has it, and `verify_settings` then refuses any other bridge binary.
pub fn bundled_bridge_sha256() -> Option<&'static str> {
    let recorded = env!("VELLUM_BUNDLED_BRIDGE_SHA256");
    (!recorded.is_empty()).then_some(recorded)
}

/// The bridge that shipped with this Vellum install. `CODEX_CLI_PATH` points
/// here, so it must be a real sibling file rather than a mode flag on the
/// Desktop binary.
pub fn packaged_bridge_executable() -> Option<PathBuf> {
    let relative = env!("VELLUM_BUNDLED_BRIDGE_RELATIVE_PATH");
    if relative.is_empty() {
        return None;
    }
    #[cfg(debug_assertions)]
    {
        let staged = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
        if staged.is_file() {
            return Some(staged);
        }
    }
    let name = Path::new(relative).file_name()?;
    // A dev build leaves it beside the executable; an installed build gets it
    // from the bundled `binaries/` resource directory.
    let candidates = crate::install_paths::bundled_binary_candidates(name);

    // Name and location do not identify this file; its bytes do.
    //
    // An install can hold more than one binary called
    // `vellum-codex-app-server.exe`. Vellum builds it twice by design -- once
    // into an isolated target directory, staged into `binaries/` and hashed
    // into this host, and once as an ordinary workspace bin that the bundler
    // also copies next to the executable. Two separate compilations of the
    // same source are not byte-identical, and a stale copy left by an earlier
    // install is a third way to get a same-named impostor.
    //
    // Picking by position meant picking the sibling, which is the one this
    // release did *not* record -- so `verify_settings` refused the bridge, and
    // Enhanced silently failed to arm with a hash mismatch that named no file.
    // Ask for the bytes this release shipped and the question answers itself.
    if let Some(expected) = bundled_bridge_sha256() {
        let matching = candidates.iter().find(|candidate| {
            candidate.is_file()
                && hex_sha256_file(candidate)
                    .is_ok_and(|digest| format!("sha256:{digest}") == expected)
        });
        if let Some(matching) = matching {
            return Some(matching.clone());
        }
    }

    // No recorded digest (a plain `cargo build`), or nothing matched. Fall back
    // to first-found so the failure stays the explicit hash mismatch downstream
    // rather than "no bridge at all", which would name the wrong problem.
    candidates.into_iter().find(|candidate| candidate.is_file())
}

/// The Enhanced Codex core this Vellum install runs.
///
/// Deliberately not a user-chosen path. `verify_settings` already requires the
/// core's sha256 to equal `enhanced-runtime.lock.json`'s `artifactSha256`, so
/// there was never a choice to make here — exactly one file on the machine can
/// pass, and asking a user to go find it turns a fixed answer into a scavenger
/// hunt whose failure mode ("no such file", one screen later) does not name the
/// real requirement.
///
/// The search mirrors the bridge's, plus the managed store under the data root
/// that a fetched artifact lands in. The first existing candidate wins; the
/// hash check downstream is what decides whether it is the right one.
pub fn packaged_enhanced_executable(data_root: &Path) -> Option<PathBuf> {
    let managed = data_root
        .join(MANAGED_CORE_DIR)
        .join(pinned_artifact_directory()?)
        .join(enhanced_executable_name());
    if managed.is_file() {
        return Some(managed);
    }
    let relative = env!("VELLUM_BUNDLED_ENHANCED_RELATIVE_PATH");
    if relative.is_empty() {
        return None;
    }
    let staged = PathBuf::from(relative);
    // A dev pointer names the fork's build output directly, wherever it is.
    if staged.is_absolute() {
        return staged.is_file().then_some(staged);
    }
    #[cfg(debug_assertions)]
    {
        let in_crate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
        if in_crate.is_file() {
            return Some(in_crate);
        }
    }
    let name = Path::new(relative).file_name()?;
    crate::install_paths::bundled_binary_candidates(name)
        .into_iter()
        .find(|candidate| candidate.is_file())
}

fn enhanced_executable_name() -> &'static str {
    if cfg!(windows) {
        "codex.exe"
    } else {
        "codex"
    }
}

/// `sha256_<hex>` for the artifact this release is pinned to, matching the
/// managed store's directory naming.
fn pinned_artifact_directory() -> Option<String> {
    let lock = EnhancedRuntimeLockFile::parse(LOCK_BYTES).ok()?;
    let digest = lock
        .artifact_for_target(env!("VELLUM_BUILD_TARGET"))?
        .to_string();
    let hex = digest.strip_prefix("sha256:")?;
    Some(format!("sha256_{hex}"))
}

/// Arms or disarms Enhanced.
///
/// Neither binary is a parameter any more. Both are decided by the release:
/// the bridge by [`packaged_bridge_executable`], the core by
/// [`packaged_enhanced_executable`]. The only thing a caller chooses is
/// `enabled`.
pub fn configure_desktop_runtime(
    data_root: &Path,
    official_codex_executable: PathBuf,
    enabled: bool,
) -> Result<(), DesktopRuntimeManagerError> {
    let previous = read_settings(data_root)?;
    let (enhanced_codex_executable, bridge_executable) = if enabled {
        (
            packaged_enhanced_executable(data_root)
                .ok_or(DesktopRuntimeManagerError::PackagedEnhancedUnavailable)?,
            packaged_bridge_executable()
                .ok_or(DesktopRuntimeManagerError::PackagedBridgeUnavailable)?,
        )
    } else {
        // Turning it off must not depend on being able to find either binary:
        // the point of this branch is to give the environment back. Keep
        // whatever was recorded, and fall back to the packaged paths only to
        // name something for the release call below.
        let previous_paths = previous.as_ref().map(|settings| {
            (
                settings.enhanced_codex_executable.clone(),
                settings.bridge_executable.clone(),
            )
        });
        previous_paths.unwrap_or_else(|| {
            (
                packaged_enhanced_executable(data_root).unwrap_or_default(),
                packaged_bridge_executable().unwrap_or_default(),
            )
        })
    };
    let settings = DesktopRuntimeSettings {
        enabled,
        official_codex_executable,
        enhanced_codex_executable,
        bridge_executable,
    };
    if enabled {
        verify_settings(data_root, &settings)?;
        if let Some(previous) = previous.as_ref() {
            if previous.bridge_executable != settings.bridge_executable {
                env_lease::release_configured_bridge(data_root, &previous.bridge_executable)
                    .map_err(|error| {
                        DesktopRuntimeManagerError::EnvironmentLease(error.to_string())
                    })?;
            }
        }
    } else {
        // Disabling has to give the shared per-user environment back before it
        // stops being our business.
        let configured_bridge = previous
            .map(|settings| settings.bridge_executable)
            .unwrap_or_else(|| settings.bridge_executable.clone());
        env_lease::release_configured_bridge(data_root, &configured_bridge)
            .map_err(|error| DesktopRuntimeManagerError::EnvironmentLease(error.to_string()))?;
    }
    let path = data_root.join(SETTINGS_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(&settings)?)?;
    Ok(())
}

/// Turns Enhanced off and restores `CODEX_CLI_PATH` with the three-way rule.
pub fn disable_desktop_runtime(
    data_root: &Path,
) -> Result<ReleaseOutcome, DesktopRuntimeManagerError> {
    let settings = read_settings(data_root)?;
    let outcome = if let Some(settings) = settings.as_ref() {
        env_lease::release_configured_bridge(data_root, &settings.bridge_executable)
    } else {
        env_lease::release(data_root)
    }
    .map_err(|error| DesktopRuntimeManagerError::EnvironmentLease(error.to_string()))?;
    if let Some(mut settings) = settings {
        settings.enabled = false;
        std::fs::write(
            data_root.join(SETTINGS_FILE),
            serde_json::to_vec_pretty(&settings)?,
        )?;
    }
    Ok(outcome)
}

/// Releases only the process-launch lease while preserving the user's
/// Enhanced Runtime preference. The local proxy owns the data plane the
/// bridge's third-party child talks to, so stopping that proxy must disarm
/// future Codex Desktop launches without silently turning the preference off.
/// A later proxy start may arm it again by calling [`prepare_desktop_launch`].
pub fn release_desktop_launch(
    data_root: &Path,
) -> Result<ReleaseOutcome, DesktopRuntimeManagerError> {
    let outcome = match read_settings(data_root)? {
        Some(settings) => {
            env_lease::release_configured_bridge(data_root, &settings.bridge_executable)
        }
        None => env_lease::release(data_root),
    }
    .map_err(|error| DesktopRuntimeManagerError::EnvironmentLease(error.to_string()))?;
    Ok(outcome)
}

/// The Enhanced launch lease is owned by the local Proxy, not by a Settings
/// toggle or a Codex restart button. Preference may stay enabled; the
/// environment must not stay armed after the Proxy stops.
pub fn desktop_launch_desire(proxy_running: bool) -> DesktopLaunchDesire {
    if proxy_running {
        DesktopLaunchDesire::Arm
    } else {
        DesktopLaunchDesire::Disarm
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopLaunchDesire {
    Arm,
    Disarm,
}

/// Arm the verified bridge whenever the Proxy is serving. There is no separate
/// user preference: Enhanced is part of the Proxy runtime, and stopping the
/// Proxy always restores the native Codex launch path.
///
/// Both stored executable paths are repaired first, because both go stale with
/// nobody touching them: every build stages a new hash-named bridge sidecar,
/// and Codex Desktop updates its own core into a new hash-named directory.
/// Neither is a field a user can edit, so neither may be allowed to block
/// arming — see [`adopt_packaged_bridge`] and [`adopt_discovered_official`].
pub fn sync_desktop_launch(
    data_root: &Path,
    routes: &[Route],
    models: &[ModelRoute],
    proxy_running: bool,
) -> Result<Option<DesktopRuntimeLaunch>, DesktopRuntimeManagerError> {
    adopt_packaged_bridge(data_root)?;
    adopt_discovered_official(data_root)?;
    match desktop_launch_desire(proxy_running) {
        DesktopLaunchDesire::Arm => {
            let official = crate::remote::desktop_codex::desktop_identity()
                .map_err(|error| DesktopRuntimeManagerError::DesktopProbe(error.to_string()))?;
            configure_desktop_runtime(data_root, PathBuf::from(official.binary), true)?;
            prepare_desktop_launch(data_root, routes, models)
        }
        DesktopLaunchDesire::Disarm => {
            release_desktop_launch(data_root)?;
            Ok(None)
        }
    }
}

/// Points the stored settings at the bridge this build actually ships.
///
/// The bridge is not a choice. Its hash is compiled into this binary and
/// `verify_settings` rejects every other file, which is exactly why the UI
/// states it rather than offering it. But the *path* to it is stored, and each
/// build stages a new sidecar under a new hash-named file — so the stored path
/// goes stale by itself, with no user action involved.
///
/// That turned an internal build detail into "App Server bridge does not match
/// the one this release shipped", raised at Proxy start, about a field the user
/// is not allowed to edit. Correcting it is safe for the same reason it is not
/// a choice: the packaged bridge is the only value that can ever verify.
///
/// The lease is handed back under the old bridge's own name first. Once the
/// settings stop naming that file, nothing else can release what it took.
/// Helper executables the Enhanced core cannot reach but the official one can.
///
/// Codex does not carry its Windows sandbox helpers inside `codex.exe`. It
/// looks for them beside its own binary — `<dir>/<name>.exe`, or
/// `<dir>/resources/<name>.exe` — and when the lookup misses it falls back to
/// the bare file name, which Windows then tries to resolve through PATH. That
/// is the "Windows 找不到 'codex-windows-sandbox-setup.exe'" dialog: not a
/// corrupted install, just a helper that was never built.
///
/// A fork built with `cargo build -p codex-cli` produces one file. The official
/// install ships four. So the list is taken from the official install rather
/// than hard-coded: whatever helpers that one can reach, the Enhanced core has
/// to be able to reach too, and a future Codex that adds a fifth needs no
/// change here.
///
/// Missing helpers do not fail verification, and they do not all cost the same.
/// Threads and routing are unaffected either way; what breaks is whichever
/// capability uses the helper that is absent — sandboxed command execution for
/// the sandbox pair, code mode for the code-mode host, and code mode is off by
/// default (`Feature::CodeMode`, `default_enabled: false`), so its host being
/// absent costs nothing until someone turns it on. The list is reported rather
/// than ranked here; the wording that goes with it is the UI's to get right.
fn missing_enhanced_helpers(official: &Path, enhanced: &Path) -> Vec<String> {
    /// Codex's own name for the sibling folder it also searches.
    const RESOURCES_DIRNAME: &str = "resources";

    let (Some(official_dir), Some(enhanced_dir)) = (official.parent(), enhanced.parent()) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(official_dir) else {
        return Vec::new();
    };
    let reachable = |name: &std::ffi::OsStr| {
        enhanced_dir.join(name).is_file()
            || enhanced_dir.join(RESOURCES_DIRNAME).join(name).is_file()
    };
    let mut missing = entries
        .flatten()
        .map(|entry| entry.file_name())
        .filter(|name| {
            let text = name.to_string_lossy().to_ascii_lowercase();
            text.ends_with(".exe") && text != "codex.exe" && !reachable(name)
        })
        .map(|name| name.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    missing.sort();
    missing
}

fn adopt_packaged_bridge(data_root: &Path) -> Result<(), DesktopRuntimeManagerError> {
    let Some(mut settings) = read_settings(data_root)? else {
        return Ok(());
    };
    let Some(packaged) = packaged_bridge_executable() else {
        return Ok(());
    };
    if canonical(&settings.bridge_executable) == canonical(&packaged) {
        return Ok(());
    }
    env_lease::release_configured_bridge(data_root, &settings.bridge_executable)
        .map_err(|error| DesktopRuntimeManagerError::EnvironmentLease(error.to_string()))?;
    settings.bridge_executable = packaged;
    std::fs::write(
        data_root.join(SETTINGS_FILE),
        serde_json::to_vec_pretty(&settings)?,
    )?;
    Ok(())
}

pub fn desktop_runtime_status(data_root: &Path) -> DesktopRuntimeStatus {
    // A Codex Desktop that updated itself since the last arming would otherwise
    // show a stale-path blocker on a card with no control that could fix it.
    // Ignored on failure: this is a repair, and a status read that cannot
    // repair should still report.
    let _ = adopt_discovered_official(data_root);
    match read_settings(data_root) {
        Ok(Some(settings)) => status_for_settings(data_root, &settings),
        Ok(None) => unconfigured_status(data_root),
        Err(error) => empty_status(data_root, true, vec![error.to_string()]),
    }
}

/// The attestation of whatever bridge is serving Codex Desktop at this moment.
///
/// `is_live` is what makes this safe to gate a restart on: a bridge that has
/// already exited leaves its last attestation behind, and treating that file as
/// current would refuse every restart from then on.
pub fn live_bridge_attestation(data_root: &Path) -> Option<BridgeAttestationV1> {
    let attestation = BridgeAttestationV1::read(&data_root.join(ATTESTATION_FILE)).ok()?;
    attestation.is_live().then_some(attestation)
}

/// Writes the launch manifest, takes the `CODEX_CLI_PATH` lease, and returns
/// the launch identity the caller must then observe in the attestation.
pub fn prepare_desktop_launch(
    data_root: &Path,
    routes: &[Route],
    models: &[ModelRoute],
) -> Result<Option<DesktopRuntimeLaunch>, DesktopRuntimeManagerError> {
    let Some(settings) = read_settings(data_root)? else {
        return Ok(None);
    };
    if !settings.enabled {
        return Ok(None);
    }
    prepare_desktop_launch_guarded(data_root, routes, models, settings, || Ok(()), false)
}

/// Proxy arming performs expensive verification without holding the lifecycle
/// lock. Every shared-state write is deferred until the caller admits this
/// generation, and the returned guard lives through the final lease commit.
pub(crate) fn sync_proxy_desktop_launch<G>(
    data_root: &Path,
    routes: &[Route],
    models: &[ModelRoute],
    commit_guard: impl FnOnce() -> Result<G, DesktopRuntimeManagerError>,
) -> Result<Option<DesktopRuntimeLaunch>, DesktopRuntimeManagerError> {
    let official = crate::remote::desktop_codex::desktop_identity()
        .map_err(|error| DesktopRuntimeManagerError::DesktopProbe(error.to_string()))?;
    let settings = DesktopRuntimeSettings {
        enabled: true,
        official_codex_executable: PathBuf::from(official.binary),
        enhanced_codex_executable: packaged_enhanced_executable(data_root)
            .ok_or(DesktopRuntimeManagerError::PackagedEnhancedUnavailable)?,
        bridge_executable: packaged_bridge_executable()
            .ok_or(DesktopRuntimeManagerError::PackagedBridgeUnavailable)?,
    };
    prepare_desktop_launch_guarded(data_root, routes, models, settings, commit_guard, true)
}

fn prepare_desktop_launch_guarded<G>(
    data_root: &Path,
    routes: &[Route],
    models: &[ModelRoute],
    settings: DesktopRuntimeSettings,
    commit_guard: impl FnOnce() -> Result<G, DesktopRuntimeManagerError>,
    save_settings: bool,
) -> Result<Option<DesktopRuntimeLaunch>, DesktopRuntimeManagerError> {
    let mut settings = settings;
    if crate::updates::consume_core_pending_apply() {
        if let Some(pending) = crate::updates::pending_core(data_root) {
            if pending.path.is_file() && !pending.digest.is_empty() {
                settings.enhanced_codex_executable = pending.path;
            }
        }
    }
    let identity = verify_settings(data_root, &settings)?;
    let model_map = TrustedModelProviderMap::from_catalog(routes, models);
    let model_map_path = data_root.join(MODEL_MAP_FILE);
    let model_map_bytes = serde_json::to_vec_pretty(&model_map)?;

    let official_ids = routes
        .iter()
        .filter(|route| route.provider_kind == ProviderKind::Official)
        .map(|_| "openai-official".to_string())
        .collect::<BTreeSet<_>>();
    let third_party_ids = routes
        .iter()
        .filter(|route| route.provider_kind != ProviderKind::Official)
        .map(|route| route.id.clone())
        .collect::<BTreeSet<_>>();
    if third_party_ids.is_empty() {
        return Err(DesktopRuntimeManagerError::NoThirdPartyProviders);
    }
    if official_ids.is_empty() {
        return Err(DesktopRuntimeManagerError::NoOfficialProvider);
    }

    let official_home = official_codex_home();
    let (official_task_home, enhanced_task_home) = shared_task_homes(&official_home);

    let launch_id = ulid::Ulid::new().to_string();
    let manifest = LaunchManifestV1 {
        schema_version: LAUNCH_MANIFEST_SCHEMA_VERSION,
        launch_id: launch_id.clone(),
        created_at: chrono::Utc::now().timestamp(),
        official: RuntimeBinaryIdentity {
            artifact_sha256: sha256_file(&settings.official_codex_executable)?,
            executable: settings.official_codex_executable.clone(),
            runtime_digest: identity.official_digest,
            codex_home: official_task_home,
        },
        enhanced: RuntimeBinaryIdentity {
            artifact_sha256: sha256_file(&settings.enhanced_codex_executable)?,
            executable: settings.enhanced_codex_executable.clone(),
            runtime_digest: identity.enhanced_digest,
            // Both app-server children must publish into one task authority.
            // Remote Control is one relay connection, so a digest-scoped
            // second CODEX_HOME made every Enhanced thread invisible to it.
            // The Enhanced behavior itself is selected by process env and
            // runtime digest, not by storing task data in another home.
            codex_home: enhanced_task_home,
        },
        model_provider_map_sha256: format!("sha256:{:x}", Sha256::digest(&model_map_bytes)),
        model_provider_map_path: model_map_path,
        binding_db: data_root.join(BINDING_DB_FILE),
        official_provider_ids: official_ids.into_iter().collect(),
        third_party_provider_ids: third_party_ids.into_iter().collect(),
        feature_profile: FEATURE_DEFAULTS_MVP.into(),
        enhanced_commit: identity.enhanced_commit,
        attestation_path: data_root.join(ATTESTATION_FILE),
        qualification_journal_path: data_root.join(QUALIFICATION_JOURNAL_FILE),
        relay: packaged_relay(),
    };
    let manifest_path = LaunchManifestV1::path_in(data_root);

    let _commit_guard = commit_guard()?;
    if save_settings {
        if let Some(previous) = read_settings(data_root)? {
            if previous.bridge_executable != settings.bridge_executable {
                env_lease::release_configured_bridge(data_root, &previous.bridge_executable)
                    .map_err(|error| {
                        DesktopRuntimeManagerError::EnvironmentLease(error.to_string())
                    })?;
            }
        }
        let path = data_root.join(SETTINGS_FILE);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        super::atomic::write_atomic(&path, &serde_json::to_vec_pretty(&settings)?)?;
    }
    model_map.write(&manifest.model_provider_map_path)?;

    // Keep the launch identity of a bridge that is already serving this exact
    // configuration.
    //
    // The launch id is what proves "the Desktop that came back is running the
    // thing we prepared", so it has to change whenever anything it describes
    // changes. But it was being minted on *every* call, including calls that
    // would describe byte-for-byte the same launch — and since adoption is
    // judged by `attestation.matches_launch(manifest.launch_id)`, re-minting
    // told the screen that a healthy, adopted bridge was no longer ours. The
    // user then restarted Codex Desktop to fix it, which prepared yet another
    // launch, which invalidated it again.
    //
    // Reuse is only safe when the manifest we would write is identical apart
    // from its identity, and something live is actually on it. Both halves
    // matter: identical content means the id still describes what is running,
    // and a live adopted attestation means there is a launch to keep.
    let manifest = match LaunchManifestV1::read(&manifest_path) {
        Ok(existing) if same_launch_content(&existing, &manifest) => {
            match BridgeAttestationV1::read(&existing.attestation_path) {
                Ok(attestation)
                    if attestation.matches_launch(&existing.launch_id)
                        && attestation.is_live()
                        && adopted_by_codex_desktop(&attestation) =>
                {
                    existing
                }
                _ => {
                    manifest.write(&manifest_path)?;
                    manifest
                }
            }
        }
        _ => {
            manifest.write(&manifest_path)?;
            manifest
        }
    };
    let launch_id = manifest.launch_id.clone();

    // The manifest has to exist before the lease: Codex Desktop can start the
    // bridge the instant the environment changes.
    env_lease::acquire(
        data_root,
        &settings.bridge_executable.to_string_lossy(),
        &launch_id,
    )
    .map_err(|error| DesktopRuntimeManagerError::EnvironmentLease(error.to_string()))?;

    Ok(Some(DesktopRuntimeLaunch {
        bridge_executable: settings.bridge_executable,
        launch_id,
        manifest_path,
        attestation_path: manifest.attestation_path,
    }))
}

/// Do two manifests describe the same launch, ignoring which launch it is?
///
/// Written as a full destructuring rather than a field list so that adding a
/// field to the manifest fails to compile here. A field that this forgets to
/// compare is a field that can change under a reused launch id — which is the
/// one thing the id is supposed to rule out.
fn same_launch_content(left: &LaunchManifestV1, right: &LaunchManifestV1) -> bool {
    let LaunchManifestV1 {
        schema_version,
        // Identity, not content: these are exactly what reuse preserves.
        launch_id: _,
        created_at: _,
        official,
        enhanced,
        model_provider_map_path,
        model_provider_map_sha256,
        binding_db,
        official_provider_ids,
        third_party_provider_ids,
        feature_profile,
        enhanced_commit,
        attestation_path,
        qualification_journal_path,
        relay,
    } = left;
    *schema_version == right.schema_version
        && *official == right.official
        && *enhanced == right.enhanced
        && *model_provider_map_path == right.model_provider_map_path
        && *model_provider_map_sha256 == right.model_provider_map_sha256
        && *binding_db == right.binding_db
        && *official_provider_ids == right.official_provider_ids
        && *third_party_provider_ids == right.third_party_provider_ids
        && *feature_profile == right.feature_profile
        && *enhanced_commit == right.enhanced_commit
        && *attestation_path == right.attestation_path
        && *qualification_journal_path == right.qualification_journal_path
        && *relay == right.relay
}

/// Reads the attestation for the launch Vellum most recently prepared.
pub fn observed_launch(data_root: &Path) -> Option<(LaunchManifestV1, BridgeAttestationV1)> {
    let manifest = LaunchManifestV1::read(&LaunchManifestV1::path_in(data_root)).ok()?;
    let attestation = BridgeAttestationV1::read(&manifest.attestation_path).ok()?;
    Some((manifest, attestation))
}

/// Whether a bridge attestation proves Codex Desktop adopted the bridge.
///
/// A bridge started by Vellum or by a gate runner is a real bridge, but it is
/// not evidence that the packaged app is using it. Only a Desktop parent is.
pub fn adopted_by_codex_desktop(attestation: &BridgeAttestationV1) -> bool {
    let Some(recorded_parent) = attestation.parent_executable.as_deref() else {
        return false;
    };
    let Some(live_parent_pid) = super::process_info::parent_pid(attestation.bridge_pid) else {
        return false;
    };
    let Some(live_parent) = super::process_info::executable_of(live_parent_pid) else {
        return false;
    };
    live_parent_pid == attestation.parent_pid
        && canonical(&live_parent) == canonical(recorded_parent)
        && is_known_codex_desktop_executable(&live_parent)
}

fn is_known_codex_desktop_executable(path: &Path) -> bool {
    is_known_codex_desktop_executable_for(path, dirs::home_dir().as_deref())
}

/// On macOS Codex Desktop is `ChatGPT.app`, and its parent executable is
/// `Contents/MacOS/ChatGPT`. The Windows-only list here used to be the whole
/// check, so on a Mac a bridge Desktop really had spawned was reported as
/// "not started by Codex Desktop" and never counted as adopted.
fn is_known_codex_desktop_executable_for(path: &Path, home: Option<&Path>) -> bool {
    // Both the NTFS and default APFS volumes are case-insensitive.
    let normalize = |path: &Path| {
        path.to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase()
    };
    let normalized = normalize(path);
    let windows_install = (normalized.contains("\\windowsapps\\openai.codex_")
        || normalized.contains("\\programs\\openai\\codex\\"))
        && (normalized.ends_with("\\codex.exe") || normalized.ends_with("\\chatgpt.exe"));
    windows_install
        || crate::install_paths::macos_codex_desktop_executable_candidates(home)
            .iter()
            .any(|candidate| normalize(candidate) == normalized)
}

struct VerifiedIdentity {
    official_digest: String,
    enhanced_digest: String,
    enhanced_commit: String,
    protocol: ProtocolCompatibility,
}

/// Cache key for a protocol comparison: both binaries by path, size and mtime.
///
/// The comparison costs two `generate-json-schema` subprocesses and ~300 file
/// parses, and `status_for_settings` runs on every status refresh. Keyed this
/// way a hit is always correct rather than a staleness tradeoff — the answer
/// can only change when one of the two binaries is replaced, which is exactly
/// what the key measures. `desktop_identity` caches its own probe the same way.
type BinaryStamp = (PathBuf, u64, std::time::SystemTime);

/// The pinned core and the Desktop core, in that order.
type ProtocolCacheKey = (BinaryStamp, BinaryStamp);
type ProtocolCache = std::sync::Mutex<Option<(ProtocolCacheKey, ProtocolCompatibility)>>;

static PROTOCOL_CACHE: std::sync::OnceLock<ProtocolCache> = std::sync::OnceLock::new();

pub fn invalidate_protocol_cache() {
    if let Some(cache) = PROTOCOL_CACHE.get() {
        *cache.lock().expect("state poisoned") = None;
    }
}

fn packaged_relay() -> Option<super::launch_manifest::RelayBinaryIdentity> {
    let expected = env!("VELLUM_BUNDLED_RELAY_SHA256");
    if expected.is_empty() {
        return None;
    }
    let name = if cfg!(windows) {
        "vellum-codex-relay.exe"
    } else {
        "vellum-codex-relay"
    };
    let mut candidates = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join(name)];
    candidates.extend(crate::install_paths::bundled_binary_candidates(name));
    candidates
        .into_iter()
        .find(|path| {
            hex_sha256_file(path).is_ok_and(|actual| format!("sha256:{actual}") == expected)
        })
        .map(|executable| super::launch_manifest::RelayBinaryIdentity {
            executable,
            artifact_sha256: expected.into(),
        })
}

fn binary_stamp(path: &Path) -> Result<BinaryStamp, DesktopRuntimeManagerError> {
    let metadata = std::fs::metadata(path)?;
    Ok((path.to_path_buf(), metadata.len(), metadata.modified()?))
}

/// Whether the Enhanced core can serve the Codex Desktop installed here.
///
/// Both sides are probed rather than compared against anything stored: the
/// Enhanced core describes its own protocol, and so does Desktop's, so there is
/// no snapshot in the repository to drift out of date. See
/// [`super::protocol_compat`] for why the verdict is graded instead of a hash
/// equality.
fn protocol_compatibility(
    enhanced: &Path,
    official: &Path,
) -> Result<ProtocolCompatibility, DesktopRuntimeManagerError> {
    let key = (binary_stamp(enhanced)?, binary_stamp(official)?);
    let cache = PROTOCOL_CACHE.get_or_init(|| std::sync::Mutex::new(None));
    if let Some((cached_key, cached)) = cache.lock().expect("state poisoned").as_ref() {
        if cached_key == &key {
            return Ok(cached.clone());
        }
    }
    let pinned = ProtocolSurface::probe(enhanced)
        .map_err(|error| DesktopRuntimeManagerError::ProtocolProbe(error.to_string()))?;
    let desktop = ProtocolSurface::probe(official)
        .map_err(|error| DesktopRuntimeManagerError::ProtocolProbe(error.to_string()))?;
    let result = protocol_compat::compare(&pinned, &desktop);
    *cache.lock().expect("state poisoned") = Some((key, result.clone()));
    Ok(result)
}

/// Points the stored settings at the Codex core Desktop actually has.
///
/// Codex Desktop updates itself by dropping a new hash-named directory under
/// `bin/` and leaving the old one in place. Nothing about that is a user
/// action, and the Official core is not a field anyone can edit — but it is a
/// stored path, so it goes stale on its own and surfaced as "configured
/// Official core does not match Desktop" on a card with no way to act on it.
///
/// Adopting is safe for the same reason the bridge's equivalent is: the
/// discovered binary is the only value that can ever be right, since it is the
/// one `desktop_identity` will keep reporting. Whether the newly discovered
/// core is *compatible* is a separate question, and deliberately still asked —
/// `verify_settings` runs the protocol comparison against whatever this adopts.
fn adopt_discovered_official(data_root: &Path) -> Result<(), DesktopRuntimeManagerError> {
    let Some(mut settings) = read_settings(data_root)? else {
        return Ok(());
    };
    let Ok(identity) = crate::remote::desktop_codex::desktop_identity() else {
        return Ok(());
    };
    let discovered = PathBuf::from(&identity.binary);
    if canonical(&settings.official_codex_executable) == canonical(&discovered) {
        return Ok(());
    }
    settings.official_codex_executable = discovered;
    std::fs::write(
        data_root.join(SETTINGS_FILE),
        serde_json::to_vec_pretty(&settings)?,
    )?;
    Ok(())
}

fn status_for_settings(
    data_root: &Path,
    settings: &DesktopRuntimeSettings,
) -> DesktopRuntimeStatus {
    let mut status = empty_status(data_root, true, Vec::new());
    status.enabled = settings.enabled;
    status.official_codex_executable = Some(settings.official_codex_executable.clone());
    status.enhanced_codex_executable = Some(settings.enhanced_codex_executable.clone());
    status.bridge_executable = Some(settings.bridge_executable.clone());
    status.missing_helpers = missing_enhanced_helpers(
        &settings.official_codex_executable,
        &settings.enhanced_codex_executable,
    );
    status.launch_core_drift = apply_observed_launch(data_root, settings, &mut status);
    apply_environment_state(data_root, settings, &mut status);

    // Once an old development bridge is fully released, present the sidecar
    // pinned into this host as the next enablement candidate. Keep the stored
    // path while it is still active/orphaned so Disable can release that exact
    // value first.
    let mut verification_settings = settings.clone();
    if !status.enabled && !status.bridge_observed && status.environment_state == "released" {
        if let Some(packaged) = packaged_bridge_executable() {
            verification_settings.bridge_executable = packaged.clone();
            status.bridge_executable = Some(packaged);
        }
    }
    if status
        .last_qualification
        .as_ref()
        .is_some_and(|qualification| {
            !qualification_matches_settings(qualification, &verification_settings)
        })
    {
        status.last_qualification = None;
    }
    match verify_settings(data_root, &verification_settings) {
        Ok(identity) => {
            status.artifact_ready = true;
            status.enhanced_runtime_digest = Some(identity.enhanced_digest);
            status.unverified = identity.protocol.verdict == ProtocolVerdict::Unverified;
            status.protocol = Some(identity.protocol);
        }
        Err(error) => {
            status.blockers.push(error.to_string());
            // A verdict is worth showing even when something else failed
            // verification: it is the difference between "this Codex cannot be
            // served" and "this Codex is fine, the bridge binary is not".
            status.protocol = protocol_compatibility(
                &verification_settings.enhanced_codex_executable,
                &verification_settings.official_codex_executable,
            )
            .ok();
        }
    }
    status.ready = status.enabled
        && status.artifact_ready
        && status.active
        && status.environment_state == "leased";
    status.restart_required = status.enabled != status.active
        || (!status.enabled && status.bridge_observed)
        || (status.active && status.environment_state != "leased")
        // A launch left on a superseded core is exactly what a managed restart
        // fixes, and the only thing that fixes it: the path is adopted already,
        // so nothing changes until the children are respawned from it.
        || status.launch_core_drift;
    status.activation_state = activation_state(&status).into();
    status
}

fn qualification_matches_settings(
    qualification: &QualificationResult,
    settings: &DesktopRuntimeSettings,
) -> bool {
    let Ok(bytes) = std::fs::read(&qualification.report_path) else {
        return false;
    };
    let Ok(report) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    let expected = [
        ("bridge", &settings.bridge_executable),
        ("official", &settings.official_codex_executable),
        ("enhanced", &settings.enhanced_codex_executable),
    ];
    expected.into_iter().all(|(key, path)| {
        let actual = report
            .get(key)
            .and_then(|identity| identity.get("sha256"))
            .and_then(serde_json::Value::as_str);
        sha256_file(path).ok().as_deref() == actual
    })
}

fn activation_state(status: &DesktopRuntimeStatus) -> &'static str {
    if status.ready {
        "active"
    } else if !status.enabled && (status.active || status.bridge_observed) {
        "disablePendingRestart"
    } else if !status.enabled && status.environment_state == "orphanedBridge" {
        "environmentDrift"
    } else if !status.enabled {
        "disabled"
    } else if !status.artifact_ready {
        "artifactBlocked"
    } else if status.environment_state != "leased" && status.environment_state != "released" {
        "environmentDrift"
    } else if status.bridge_state.as_deref() == Some("failed") {
        "failed"
    } else {
        "awaitingDesktopRestart"
    }
}

fn apply_environment_state(
    data_root: &Path,
    settings: &DesktopRuntimeSettings,
    status: &mut DesktopRuntimeStatus,
) {
    let current = match env_lease::read_user_environment(env_lease::CODEX_CLI_PATH) {
        Ok(current) => current,
        Err(error) => {
            status.environment_state = "unreadable".into();
            status.blockers.push(error.to_string());
            return;
        }
    };
    let bridge = settings.bridge_executable.to_string_lossy();
    status.environment_value = current.clone();
    status.environment_state = match env_lease::EnvironmentLease::read(data_root) {
        Some(lease)
            if current.as_deref() == Some(lease.applied_value.as_str())
                && lease.applied_value.as_str() == bridge.as_ref() =>
        {
            "leased"
        }
        Some(_) => {
            status.blockers.push(
                "CODEX_CLI_PATH changed while Vellum's Enhanced Runtime lease was active".into(),
            );
            "foreignValue"
        }
        // Our own filename, no lease: an older Vellum build's sidecar, whether
        // or not it is the bridge this install would configure today. Calling
        // that foreign left the user with a variable nothing could clear.
        None if current.as_deref().is_some_and(|current| {
            current == bridge.as_ref() || env_lease::names_vellum_bridge(current)
        }) =>
        {
            status.blockers.push(format!(
                "a Vellum bridge remains in CODEX_CLI_PATH without an ownership lease: {}",
                current.as_deref().unwrap_or_default()
            ));
            "orphanedBridge"
        }
        None if current.is_some() => "foreignValue",
        None => "released",
    }
    .into();
}

/// Returns whether the running launch is serving a core the settings have
/// since moved off — a restart condition on its own, and one no other check
/// here can see.
fn apply_observed_launch(
    data_root: &Path,
    settings: &DesktopRuntimeSettings,
    status: &mut DesktopRuntimeStatus,
) -> bool {
    let configured_bridge = settings.bridge_executable.as_path();
    let Some((manifest, attestation)) = observed_launch(data_root) else {
        if status.enabled {
            status
                .blockers
                .push("no bridge attestation for a Vellum-prepared launch".into());
        }
        return false;
    };
    // A stopped previous launch is useful evidence while enablement is
    // waiting for Desktop to come back, but it is not the "current launch" of
    // an already-disabled runtime. Do not make a clean disabled screen look
    // as if dead ready PIDs were still in service.
    if !status.enabled && !attestation.is_live() {
        return false;
    }
    status.launch_id = Some(manifest.launch_id.clone());
    status.bridge_state = Some(attestation.state.as_str().to_string());
    status.bridge_pid = Some(attestation.bridge_pid);
    status.official_child_pid = attestation.official.pid;
    status.enhanced_child_pid = attestation.enhanced.pid;
    if let Some(identity) = &attestation.enhanced_identity {
        status.active_runtime_digest = Some(identity.runtime_digest.clone());
        status.active_feature_profile = Some(identity.feature_profile.clone());
    }

    let adopted = adopted_by_codex_desktop(&attestation);
    // The attestation proves what the *children* are; it says nothing about
    // which bridge binary wrote it. An older sidecar still held by Codex
    // Desktop keeps refreshing this file, and reading it as "Desktop is on the
    // configured bridge" is how the screen came to claim a verified artifact
    // was live while a stale development build served every turn.
    status.observed_bridge_executable = super::process_info::executable_of(attestation.bridge_pid);
    let bridge_matches = status
        .observed_bridge_executable
        .as_deref()
        .is_some_and(|observed| canonical(observed) == canonical(configured_bridge));
    status.bridge_observed = attestation.matches_launch(&manifest.launch_id)
        && attestation.is_live()
        && adopted
        && bridge_matches;
    let identity_matches = attestation
        .enhanced_identity
        .as_ref()
        .is_some_and(|identity| {
            identity.runtime_digest == manifest.enhanced.runtime_digest
                && identity.enhanced_commit == manifest.enhanced_commit
                && identity.qwen_tool_reliability == manifest.feature_profile.qwen_tool_reliability
                && identity.deepseek_context_recovery
                    == manifest.feature_profile.deepseek_context_recovery
                && identity.qwen_bounded_continuation
                    == manifest.feature_profile.qwen_bounded_continuation
        });
    let digests_match = attestation.official.runtime_digest == manifest.official.runtime_digest
        && attestation.enhanced.runtime_digest == manifest.enhanced.runtime_digest
        && identity_matches;
    // Two different facts, and collapsing them is what made the button read
    // "not loaded" while Enhanced was answering every turn: `serving` is
    // whether Codex Desktop is going through the Enhanced core at all,
    // `active` is whether it is doing so on *this* launch's configuration.
    // `active` stays strict -- it is what gates Ready, and the regression it
    // guards is a verified artifact printing Ready while Desktop ran Official.
    status.serving = attestation.is_serving() && adopted;
    status.active = attestation.is_active_for(&manifest.launch_id)
        && adopted
        && bridge_matches
        && digests_match;
    if !status.active && (status.enabled || attestation.is_live()) {
        status
            .blockers
            .extend(attestation.blockers_for(&manifest.launch_id));
        if !adopted {
            status.blockers.push(
                "the running bridge was not started by Codex Desktop (EnhancedDesktopBridgeNotObserved)"
                    .into(),
            );
        }
        if !bridge_matches && attestation.is_live() {
            status.blockers.push(format!(
                "the live bridge runs from {}, not the configured App Server bridge",
                status
                    .observed_bridge_executable
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "an unreadable executable".into())
            ));
        }
        if !digests_match {
            status
                .blockers
                .push("bridge attestation digests do not match the launch manifest".into());
        }
    }

    // Codex Desktop updates by dropping a new hash-named directory under
    // `bin/`, and `adopt_discovered_official` follows it in the stored
    // settings. The launch already running does not follow — it keeps the core
    // it spawned — and nothing else here can tell, because every other check
    // compares the settings against what Desktop reports and those two moved
    // together. The screen therefore read Live, with `enabled == active` and a
    // held lease, while the child served the previous core.
    //
    // Which would still be only stale, except Desktop's updater then cleans out
    // the directory it replaced, and the one file it cannot delete is the
    // executable this launch is holding open. The old core keeps running beside
    // helpers that no longer exist, so every tool call needing one dies inside
    // Codex with a bare "cannot find the file specified" and nothing on this
    // screen accounting for it. Observed on 2026-09-05: the live launch held
    // `bin\9ba750cce02d5e5c\codex.exe`, whose three sibling helpers had been
    // removed under it, while settings had already adopted the replacement.
    let drift = [
        (
            "Official",
            &manifest.official.executable,
            &settings.official_codex_executable,
        ),
        (
            "Enhanced",
            &manifest.enhanced.executable,
            &settings.enhanced_codex_executable,
        ),
    ]
    .into_iter()
    .filter(|(_, launched, configured)| canonical(launched) != canonical(configured))
    .map(|(plane, launched, configured)| {
        format!(
            "the live launch runs the {plane} core from {}, not the configured {}",
            launched.display(),
            configured.display()
        )
    })
    .collect::<Vec<_>>();
    if !attestation.is_live() || drift.is_empty() {
        return false;
    }
    status.blockers.extend(drift);
    true
}

/// The launch whose Proxy transaction should be rebuilt, if any.
///
/// Detecting the drift was never the hard part; the hard part is that nothing
/// the user can reach fixes it. The stored path has already been corrected by
/// `adopt_discovered_official`, so there is no setting left to change. A new
/// Proxy launch transaction must be prepared before Codex can respawn onto it.
/// Vellum owns that Proxy restart; the UI keeps the subsequent Codex restart
/// explicit because it replaces the user's editor process.
///
/// So this is a repair, not a policy, and the conditions are the ones that
/// make it a repair rather than an ambush:
///
/// - `launch_core_drift` is the fault itself, and it is only ever set against
///   a live attestation, so there is a running launch to rebuild.
/// - `enabled` keeps the restart out of a runtime the user has turned off.
///   Disabling already schedules its own restart, and stealing it here would
///   relaunch Desktop to apply a state nobody asked to apply now.
/// - `proxy_running` keeps a stopped Proxy stopped. Drift there is harmless;
///   the launch it describes is already on its way out.
///
/// Whether this launch has already been repaired once is deliberately not
/// asked here. That latch has to be claimed atomically against concurrent
/// status polls, so it belongs to the caller that owns the lock.
pub fn superseded_launch_repair(
    status: &DesktopRuntimeStatus,
    proxy_running: bool,
) -> Option<&str> {
    (status.launch_core_drift && status.enabled && proxy_running)
        .then_some(status.launch_id.as_deref())
        .flatten()
}

fn empty_status(data_root: &Path, configured: bool, blockers: Vec<String>) -> DesktopRuntimeStatus {
    DesktopRuntimeStatus {
        configured,
        enabled: false,
        artifact_ready: false,
        active: false,
        bridge_observed: false,
        ready: false,
        activation_state: "disabled".into(),
        environment_state: "released".into(),
        environment_value: None,
        observed_bridge_executable: None,
        restart_required: false,
        launch_id: None,
        bridge_state: None,
        bridge_pid: None,
        official_child_pid: None,
        enhanced_child_pid: None,
        enhanced_runtime_digest: None,
        active_runtime_digest: None,
        active_feature_profile: None,
        official_codex_executable: None,
        enhanced_codex_executable: None,
        bridge_executable: None,
        core_available: packaged_enhanced_executable(data_root).is_some(),
        serving: false,
        protocol: None,
        unverified: false,
        last_qualification: QualificationResult::read(data_root),
        missing_helpers: Vec::new(),
        launch_core_drift: false,
        blockers,
    }
}

fn unconfigured_status(data_root: &Path) -> DesktopRuntimeStatus {
    let mut status = empty_status(data_root, false, Vec::new());
    status.enhanced_codex_executable = packaged_enhanced_executable(data_root);
    if status.enhanced_codex_executable.is_none() {
        status
            .blockers
            .push("this Vellum build does not contain the pinned Enhanced Codex core".into());
    }
    status.bridge_executable = packaged_bridge_executable();
    if status.bridge_executable.is_none() {
        status.blockers.push(format!(
            "packaged {BRIDGE_EXECUTABLE_STEM} is missing from this install"
        ));
    }
    match crate::remote::desktop_codex::desktop_identity() {
        Ok(identity) => status.official_codex_executable = Some(PathBuf::from(identity.binary)),
        Err(error) => status.blockers.push(error.to_string()),
    }
    status
}

fn read_settings(
    data_root: &Path,
) -> Result<Option<DesktopRuntimeSettings>, DesktopRuntimeManagerError> {
    let path = data_root.join(SETTINGS_FILE);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&std::fs::read(path)?)?))
}

fn verify_settings(
    data_root: &Path,
    settings: &DesktopRuntimeSettings,
) -> Result<VerifiedIdentity, DesktopRuntimeManagerError> {
    for (name, path) in [
        ("official Codex", &settings.official_codex_executable),
        ("Enhanced Codex", &settings.enhanced_codex_executable),
        ("App Server bridge", &settings.bridge_executable),
    ] {
        if !path.is_absolute() || !path.is_file() {
            return Err(DesktopRuntimeManagerError::ExecutableUnavailable {
                name,
                path: path.clone(),
            });
        }
    }
    // A release knows which bridge bytes it shipped, and Codex Desktop will
    // execute whatever `CODEX_CLI_PATH` names — so the configured bridge has to
    // be that file and not merely a file with the right name.
    if let Some(expected) = bundled_bridge_sha256() {
        let actual = sha256_file(&settings.bridge_executable)?;
        if actual != expected {
            return Err(DesktopRuntimeManagerError::BridgeArtifactMismatch {
                expected: expected.to_string(),
                actual,
            });
        }
    }
    let lock = EnhancedRuntimeLockFile::parse(LOCK_BYTES)
        .map_err(|error| DesktopRuntimeManagerError::Lock(error.to_string()))?;
    let target = env!("VELLUM_BUILD_TARGET");
    if !lock.identity_complete_for_target(target) {
        return Err(DesktopRuntimeManagerError::IncompleteLock);
    }
    let desktop_identity = crate::remote::desktop_codex::desktop_identity()
        .map_err(|error| DesktopRuntimeManagerError::DesktopProbe(error.to_string()))?;
    let discovered_official = PathBuf::from(&desktop_identity.binary);
    if canonical(&discovered_official) != canonical(&settings.official_codex_executable) {
        return Err(DesktopRuntimeManagerError::OfficialBinaryMismatch {
            expected: discovered_official,
            actual: settings.official_codex_executable.clone(),
        });
    }
    let enhanced_hash = format!(
        "sha256:{}",
        hex_sha256_file(&settings.enhanced_codex_executable)?
    );
    if lock.artifact_for_target(target) != Some(enhanced_hash.as_str())
        && !crate::updates::signed_core_digest(data_root, &enhanced_hash)
    {
        return Err(DesktopRuntimeManagerError::ArtifactMismatch {
            expected: lock
                .artifact_for_target(target)
                .unwrap_or_default()
                .to_string(),
            actual: enhanced_hash,
        });
    }
    // Not a hash equality any more. Codex Desktop updates itself, so requiring
    // the exact pinned protocol meant every Codex release disarmed Enhanced
    // over differences that were usually additive and never on a path the
    // bridge routes. Only a routed method being gone or shape-broken withholds
    // arming; everything else is carried on the result so the UI can say the
    // pairing runs but was never qualified.
    let protocol = protocol_compatibility(
        &settings.enhanced_codex_executable,
        &settings.official_codex_executable,
    )?;
    if !protocol.verdict.may_arm() {
        return Err(DesktopRuntimeManagerError::OfficialProtocolIncompatible {
            details: protocol
                .routed_deltas()
                .map(ProtocolDelta::describe)
                .collect::<Vec<_>>()
                .join("; "),
        });
    }
    let manifest = EnhancedRuntimeManifest::from_lock(&lock, FEATURE_DEFAULTS_MVP, target)
        .map_err(|error| DesktopRuntimeManagerError::Manifest(error.to_string()))?;
    manifest
        .verify_against_lock(&lock)
        .map_err(|error| DesktopRuntimeManagerError::Manifest(error.to_string()))?;
    Ok(VerifiedIdentity {
        official_digest: format!(
            "sha256:{}",
            hex_sha256_file(&settings.official_codex_executable)?
        ),
        enhanced_digest: manifest
            .runtime_digest()
            .map_err(|error| DesktopRuntimeManagerError::Manifest(error.to_string()))?,
        enhanced_commit: manifest.enhanced_commit.clone(),
        protocol,
    })
}

fn hex_sha256_file(path: &Path) -> Result<String, std::io::Error> {
    Ok(hex::encode(Sha256::digest(std::fs::read(path)?)))
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn official_codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|path| path.join(".codex")))
        .unwrap_or_else(|| std::env::temp_dir().join(".codex"))
}

fn shared_task_homes(official_home: &Path) -> (PathBuf, PathBuf) {
    // Runtime binaries may differ; task identity must not. Remote Control and
    // Desktop both discover rollouts and SQLite thread state through this one
    // canonical home.
    (official_home.to_path_buf(), official_home.to_path_buf())
}

/// Project only the portable control files required for the Enhanced child to
/// preserve the Desktop login and Vellum Provider configuration. Rollouts,
/// SQLite state, logs, and thread storage deliberately remain isolated.
#[derive(Debug, thiserror::Error)]
pub enum DesktopRuntimeManagerError {
    #[error("{name} executable is unavailable: {}", path.display())]
    ExecutableUnavailable { name: &'static str, path: PathBuf },
    #[error("enhanced-runtime.lock.json does not contain a complete release identity")]
    IncompleteLock,
    #[error("Official Codex protocol probe failed: {0}")]
    DesktopProbe(String),
    #[error("configured Official core does not match Desktop: expected {}, got {}", expected.display(), actual.display())]
    OfficialBinaryMismatch { expected: PathBuf, actual: PathBuf },
    #[error("Official Codex protocol mismatch: expected {expected}, got {actual}")]
    OfficialProtocolMismatch { expected: String, actual: String },
    #[error("Codex Desktop broke a method the bridge routes: {details}")]
    OfficialProtocolIncompatible { details: String },
    #[error("cannot read a Codex core's protocol: {0}")]
    ProtocolProbe(String),
    #[error("Enhanced artifact mismatch: expected {expected}, got {actual}")]
    ArtifactMismatch { expected: String, actual: String },
    #[error("App Server bridge does not match the one this release shipped: expected {expected}, got {actual}")]
    BridgeArtifactMismatch { expected: String, actual: String },
    #[error("this Vellum build does not contain a pinned App Server bridge")]
    PackagedBridgeUnavailable,
    #[error("this Vellum build does not contain the pinned Enhanced Codex core")]
    PackagedEnhancedUnavailable,
    #[error("no enabled third-party Provider is available for Enhanced Codex")]
    NoThirdPartyProviders,
    #[error("no Official Provider is enabled; Official GPT must keep its own runtime")]
    NoOfficialProvider,
    #[error("runtime lock is invalid: {0}")]
    Lock(String),
    #[error("runtime manifest is invalid: {0}")]
    Manifest(String),
    #[error("cannot lease the per-user CODEX_CLI_PATH: {0}")]
    EnvironmentLease(String),
    #[error(transparent)]
    LaunchManifest(#[from] super::launch_manifest::LaunchManifestError),
    #[error(transparent)]
    ModelMap(#[from] super::model_provider_map::ModelProviderMapError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enhanced_runtime::attestation::{AttestationWriter, ChildAttestation};
    use crate::enhanced_runtime::ExecutionPlane;

    /// Vellum 0.2.8 on a Mac: the bridge was running under ChatGPT.app with a
    /// matching launch id, and status still said it was not started by Codex
    /// Desktop.
    #[test]
    fn the_macos_chatgpt_app_is_codex_desktop() {
        let home = Path::new("/Users/someone");
        for path in [
            "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT",
            "/Applications/Codex.app/Contents/MacOS/Codex",
            "/Users/someone/Applications/ChatGPT.app/Contents/MacOS/ChatGPT",
            "/Users/someone/Applications/Codex.app/Contents/MacOS/Codex",
        ] {
            assert!(
                is_known_codex_desktop_executable_for(Path::new(path), Some(home)),
                "{path}"
            );
        }
    }

    #[test]
    fn a_mac_process_that_is_not_desktop_itself_is_not_adoption() {
        let home = Path::new("/Users/someone");
        for path in [
            // Desktop's bundled CLI, and a gate runner's official core.
            "/Applications/ChatGPT.app/Contents/Resources/codex",
            "/Applications/Codex.app/Contents/Resources/codex",
            // Vellum launching the bridge is not Desktop using it.
            "/Applications/Vellum.app/Contents/MacOS/vellum-proxy-desktop",
            // A copy somewhere else is not the installed app.
            "/tmp/ChatGPT.app/Contents/MacOS/ChatGPT",
            // Another user's per-user install.
            "/Users/other/Applications/ChatGPT.app/Contents/MacOS/ChatGPT",
        ] {
            assert!(
                !is_known_codex_desktop_executable_for(Path::new(path), Some(home)),
                "{path}"
            );
        }
    }

    #[test]
    fn the_windows_desktop_installs_are_still_recognized() {
        assert!(is_known_codex_desktop_executable_for(
            Path::new(
                r"C:\Program Files\WindowsApps\OpenAI.Codex_26.903.0.0_x64__abc\app\Codex.exe"
            ),
            None
        ));
        assert!(is_known_codex_desktop_executable_for(
            Path::new(r"C:\Users\person\AppData\Local\Programs\OpenAI\Codex\ChatGPT.exe"),
            None
        ));
        assert!(!is_known_codex_desktop_executable_for(
            Path::new(r"C:\Program Files\Vellum\vellum-proxy-desktop.exe"),
            None
        ));
    }

    #[test]
    fn desktop_launch_follows_the_proxy_without_a_second_toggle() {
        assert_eq!(desktop_launch_desire(true), DesktopLaunchDesire::Arm);
        assert_eq!(desktop_launch_desire(false), DesktopLaunchDesire::Disarm);
    }

    #[test]
    fn official_and_enhanced_children_share_the_canonical_task_home() {
        let home = PathBuf::from(r"C:\Users\person\.codex");
        let (official, enhanced) = shared_task_homes(&home);
        assert_eq!(official, home);
        assert_eq!(enhanced, home);
    }

    #[test]
    fn session_plane_index_comes_from_the_durable_runtime_binding() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(BINDING_DB_FILE);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let store = ThreadRuntimeBindingStore::open(&path).unwrap();
        store
            .insert_immutable(&crate::enhanced_runtime::ThreadRuntimeBinding::new(
                "thread-enhanced",
                ExecutionPlane::EnhancedCodex,
                "sha256:runtime",
                "qwen",
                "qwen-model",
                "thread-native",
                1,
            ))
            .unwrap();

        let planes = thread_execution_planes(temp.path());
        let enhanced = crate::history::conversation_key_from_raw(Some("thread-enhanced")).unwrap();
        let native = crate::history::conversation_key_from_raw(Some("thread-native")).unwrap();
        assert_eq!(planes.get(&enhanced), Some(&ExecutionPlane::EnhancedCodex));
        assert_eq!(planes.get(&native), Some(&ExecutionPlane::EnhancedCodex));
    }

    /// A fork built with `cargo build -p codex-cli` produces one file; the
    /// official install ships four. The three that are missing are not part of
    /// `codex.exe` — Codex looks for them beside itself at run time — so the
    /// gap only shows up at the first sandboxed shell command, as a Windows
    /// "cannot find" dialog. Saying it at import time is the whole point.
    #[test]
    fn helpers_the_official_install_reaches_are_reported_when_the_fork_lacks_them() {
        let temp = tempfile::tempdir().unwrap();
        let official_dir = temp.path().join("official");
        let fork_dir = temp.path().join("fork");
        std::fs::create_dir_all(&official_dir).unwrap();
        std::fs::create_dir_all(&fork_dir).unwrap();
        for name in [
            "codex.exe",
            "codex-windows-sandbox-setup.exe",
            "codex-command-runner.exe",
            "codex-code-mode-host.exe",
        ] {
            std::fs::write(official_dir.join(name), b"official").unwrap();
        }
        let official = official_dir.join("codex.exe");
        let enhanced = fork_dir.join("codex.exe");
        std::fs::write(&enhanced, b"fork").unwrap();

        assert_eq!(
            missing_enhanced_helpers(&official, &enhanced),
            vec![
                "codex-code-mode-host.exe".to_string(),
                "codex-command-runner.exe".to_string(),
                "codex-windows-sandbox-setup.exe".to_string(),
            ]
        );

        // Codex searches a `resources` sibling too, so a helper staged there is
        // reachable and must not be reported.
        std::fs::create_dir_all(fork_dir.join("resources")).unwrap();
        std::fs::write(
            fork_dir.join("resources").join("codex-command-runner.exe"),
            b"staged",
        )
        .unwrap();
        std::fs::write(fork_dir.join("codex-code-mode-host.exe"), b"built").unwrap();
        assert_eq!(
            missing_enhanced_helpers(&official, &enhanced),
            vec!["codex-windows-sandbox-setup.exe".to_string()]
        );

        std::fs::write(fork_dir.join("codex-windows-sandbox-setup.exe"), b"built").unwrap();
        assert!(missing_enhanced_helpers(&official, &enhanced).is_empty());
        // The core itself is never a helper, however the two directories differ.
        assert!(!missing_enhanced_helpers(&official, &enhanced).contains(&"codex.exe".to_string()));
    }

    /// The stored bridge path goes stale on its own: every build stages a new
    /// sidecar under a new hash-named file. Since the bridge is not a choice —
    /// the UI states it, and only the packaged one can ever verify — a stale
    /// value must never be able to fail anything. It gets corrected instead.
    #[test]
    fn a_stale_bridge_path_is_corrected_to_the_one_this_build_ships() {
        let Some(packaged) = packaged_bridge_executable() else {
            // Nothing was staged for this build; there is no "correct" value.
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let stale = temp
            .path()
            .join("vellum-codex-app-server-from-an-older-build.exe");
        std::fs::write(&stale, b"an older sidecar").unwrap();
        let settings = DesktopRuntimeSettings {
            enabled: true,
            official_codex_executable: temp.path().join("official.exe"),
            enhanced_codex_executable: temp.path().join("enhanced.exe"),
            bridge_executable: stale.clone(),
        };
        let path = temp.path().join(SETTINGS_FILE);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec_pretty(&settings).unwrap()).unwrap();

        adopt_packaged_bridge(temp.path()).unwrap();

        let healed = read_settings(temp.path()).unwrap().unwrap();
        assert_eq!(canonical(&healed.bridge_executable), canonical(&packaged));
        // Correcting the bridge is not permission to change anything else.
        assert!(healed.enabled);
        assert_eq!(
            healed.enhanced_codex_executable,
            temp.path().join("enhanced.exe")
        );

        // Running it again with the value already correct must not rewrite or
        // disturb anything — this runs on every Proxy start.
        adopt_packaged_bridge(temp.path()).unwrap();
        let twice = read_settings(temp.path()).unwrap().unwrap();
        assert_eq!(twice.bridge_executable, healed.bridge_executable);
    }

    #[test]
    fn a_stopped_proxy_releases_the_launch_lease_even_if_enhanced_is_still_enabled() {
        let temp = tempfile::tempdir().unwrap();
        let settings = DesktopRuntimeSettings {
            enabled: true,
            official_codex_executable: temp.path().join("official.exe"),
            enhanced_codex_executable: temp.path().join("enhanced.exe"),
            bridge_executable: temp.path().join("bridge.exe"),
        };
        let path = temp.path().join(SETTINGS_FILE);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec_pretty(&settings).unwrap()).unwrap();

        let launch = sync_desktop_launch(temp.path(), &[], &[], false).unwrap();
        assert!(launch.is_none());
        assert!(read_settings(temp.path()).unwrap().unwrap().enabled);
    }

    #[test]
    fn missing_configuration_is_disabled_and_not_ready() {
        let temp = tempfile::tempdir().unwrap();
        let status = desktop_runtime_status(temp.path());
        assert!(!status.configured);
        assert!(!status.enabled);
        assert!(!status.artifact_ready);
        assert!(!status.active);
        assert!(!status.ready);
    }

    #[test]
    fn activation_state_separates_requested_observed_and_environment_state() {
        let temp = tempfile::tempdir().unwrap();
        let mut status = empty_status(temp.path(), true, Vec::new());
        status.enabled = true;
        status.artifact_ready = true;
        status.environment_state = "released".into();
        assert_eq!(activation_state(&status), "awaitingDesktopRestart");

        status.environment_state = "foreignValue".into();
        assert_eq!(activation_state(&status), "environmentDrift");

        status.enabled = false;
        status.active = true;
        assert_eq!(activation_state(&status), "disablePendingRestart");

        status.active = false;
        status.bridge_observed = true;
        assert_eq!(activation_state(&status), "disablePendingRestart");

        status.enabled = true;
        status.bridge_observed = false;
        status.environment_state = "leased".into();
        status.ready = true;
        assert_eq!(activation_state(&status), "active");
    }

    #[test]
    fn qualification_is_current_only_for_the_same_three_binaries_and_protocol() {
        let temp = tempfile::tempdir().unwrap();
        let official = temp.path().join("official.exe");
        let enhanced = temp.path().join("enhanced.exe");
        let bridge = temp.path().join("bridge.exe");
        std::fs::write(&official, b"official").unwrap();
        std::fs::write(&enhanced, b"enhanced").unwrap();
        std::fs::write(&bridge, b"bridge").unwrap();
        let report_path = temp.path().join("report.json");
        std::fs::write(
            &report_path,
            serde_json::to_vec(&serde_json::json!({
                "bridge": {"sha256": sha256_file(&bridge).unwrap()},
                "official": {"sha256": sha256_file(&official).unwrap()},
                "enhanced": {"sha256": sha256_file(&enhanced).unwrap()},
            }))
            .unwrap(),
        )
        .unwrap();
        let qualification = QualificationResult {
            run_id: "run".into(),
            mode: "installed".into(),
            started_at: 1,
            finished_at: 2,
            passed: true,
            promotion_ready: true,
            report_path,
            failures: Vec::new(),
        };
        let settings = DesktopRuntimeSettings {
            enabled: true,
            official_codex_executable: official,
            enhanced_codex_executable: enhanced,
            bridge_executable: bridge.clone(),
        };
        assert!(qualification_matches_settings(&qualification, &settings));
        std::fs::write(bridge, b"changed").unwrap();
        assert!(!qualification_matches_settings(&qualification, &settings));
    }

    /// The Enhanced core stopped being a parameter. `verify_settings` already
    /// required its sha256 to equal the lockfile's `artifactSha256`, so exactly
    /// one file on the machine could ever pass -- a "choice" with one legal
    /// answer is not a choice, it is a scavenger hunt with a misleading failure
    /// message. Arming now resolves it, and a build that does not carry it says
    /// so in those terms instead of asking for a file.
    /// A same-named impostor next to the executable must not win.
    ///
    /// This is the shape that shipped: the installer put the workspace's own
    /// build of `vellum-codex-app-server.exe` beside the executable and the
    /// staged one under `binaries/`, and the resolver took the sibling because
    /// it looked there first. `verify_settings` then refused it, and Enhanced
    /// reported a hash mismatch between two numbers, naming neither file.
    #[test]
    fn the_packaged_bridge_is_chosen_by_its_bytes_not_its_position() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path();
        let name = "vellum-codex-app-server.exe";
        std::fs::write(directory.join(name), b"a different compilation").unwrap();
        std::fs::create_dir_all(directory.join("binaries")).unwrap();
        let shipped = directory.join("binaries").join(name);
        std::fs::write(&shipped, b"the bytes this release recorded").unwrap();

        let expected = format!("sha256:{}", hex_sha256_file(&shipped).unwrap());
        let candidates =
            crate::install_paths::binary_candidates_in(directory, std::ffi::OsStr::new(name));

        let chosen = candidates
            .iter()
            .find(|candidate| {
                candidate.is_file()
                    && hex_sha256_file(candidate)
                        .is_ok_and(|digest| format!("sha256:{digest}") == expected)
            })
            .expect("the recorded bytes must be found wherever they sit");
        assert_eq!(chosen, &shipped, "position must not decide identity");

        // And the sibling, which position would have chosen, is exactly the one
        // `verify_settings` would have rejected.
        let sibling = format!("sha256:{}", hex_sha256_file(&candidates[0]).unwrap());
        assert_ne!(sibling, expected);
    }

    #[test]
    fn arming_resolves_the_core_and_never_accepts_one_from_the_caller() {
        let signature = {
            let source = include_str!("desktop_manager.rs");
            let start = source
                .find("pub fn configure_desktop_runtime(")
                .expect("configure_desktop_runtime must exist");
            let end = source[start..]
                .find(')')
                .map(|offset| start + offset)
                .expect("signature must terminate");
            source[start..end].to_string()
        };
        assert!(
            !signature.contains("enhanced_codex_executable"),
            "the caller must not be able to name the core: {signature}"
        );
        assert!(
            !signature.contains("bridge_executable"),
            "nor the bridge: {signature}"
        );

        // Nothing is staged under a temp root, so the managed store misses and
        // the answer comes from the build. Whatever it says, "arm" and "status"
        // must agree -- a status that claims a core the arm path cannot find
        // would send the user looking for a setting that does not exist.
        let temp = tempfile::tempdir().unwrap();
        let resolved = packaged_enhanced_executable(temp.path());
        assert_eq!(
            resolved.is_some(),
            desktop_runtime_status(temp.path()).core_available
        );
        if let Some(core) = resolved.as_ref() {
            assert!(core.is_absolute() && core.is_file());
        }
    }

    /// A core that is not the pinned artifact has to fail closed, however it
    /// came to be configured.
    ///
    /// This used to plant the wrong binary in the *Official* slot, which is no
    /// longer a lever that can fail: `adopt_discovered_official` rewrites that
    /// path on every status read, deliberately, because Codex Desktop renames
    /// its own core on update and no user can edit the field. So the unpinned
    /// artifact now goes where the pin actually applies.
    #[test]
    fn unpinned_artifact_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let binary = std::env::current_exe().unwrap();
        configure_desktop_runtime(temp.path(), binary.clone(), false).unwrap();

        let mut settings = read_settings(temp.path()).unwrap().unwrap();
        settings.enhanced_codex_executable = binary.clone();
        settings.bridge_executable = binary;
        std::fs::write(
            temp.path().join(SETTINGS_FILE),
            serde_json::to_vec_pretty(&settings).unwrap(),
        )
        .unwrap();

        let status = desktop_runtime_status(temp.path());
        assert!(status.configured);
        assert!(!status.ready);
        assert!(
            status
                .blockers
                .iter()
                .any(|blocker| blocker.contains("Enhanced artifact mismatch")),
            "an unpinned core verified: {:?}",
            status.blockers
        );
    }

    /// The regression this whole gate exists for: a verified artifact used to
    /// be enough to print "Ready" while Desktop kept running Official Codex.
    #[test]
    fn a_recorded_sidecar_hash_is_well_formed_and_matches_the_staged_file() {
        let Some(recorded) = bundled_bridge_sha256() else {
            // A plain `cargo build` stages no sidecar; the release build does.
            return;
        };
        assert!(recorded.starts_with("sha256:"));
        assert_eq!(recorded.len(), "sha256:".len() + 64);
        if let Some(packaged) = packaged_bridge_executable() {
            assert_eq!(sha256_file(&packaged).unwrap(), recorded);
        }
    }

    #[test]
    fn a_verified_artifact_alone_is_never_active() {
        let temp = tempfile::tempdir().unwrap();
        let mut status = empty_status(temp.path(), true, Vec::new());
        status.enabled = true;
        status.artifact_ready = true;
        let settings = settings_for_launch(None, temp.path(), binary_of_this_process());
        apply_observed_launch(temp.path(), &settings, &mut status);
        status.ready = status.enabled && status.artifact_ready && status.active;
        assert!(!status.active);
        assert!(!status.ready);
        assert!(status
            .blockers
            .iter()
            .any(|blocker| blocker.contains("no bridge attestation")));
    }

    #[test]
    fn a_bridge_not_started_by_desktop_is_reported_as_not_observed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let manifest = write_minimal_launch(root);

        // The attestation names this process as the bridge, so pointing the
        // configured bridge at this executable isolates the adoption check.
        let mut status = empty_status(root, true, Vec::new());
        let settings = settings_for_launch(Some(&manifest), root, binary_of_this_process());
        apply_observed_launch(root, &settings, &mut status);
        // The test harness, not Codex Desktop, is this process's parent.
        assert!(!status.active);
        assert_eq!(status.launch_id.as_deref(), Some("launch-1"));
        assert!(status
            .blockers
            .iter()
            .any(|blocker| blocker.contains("EnhancedDesktopBridgeNotObserved")));

        let mut stale = BridgeAttestationV1::read(&manifest.attestation_path).unwrap();
        stale.bridge_pid = 0;
        stale.write(&manifest.attestation_path).unwrap();
        let mut disabled = empty_status(root, true, Vec::new());
        apply_observed_launch(root, &settings, &mut disabled);
        assert!(disabled.launch_id.is_none());
        assert!(disabled.bridge_pid.is_none());
        assert!(disabled.blockers.is_empty());
    }

    /// The screen once said "verified artifact" and "running on the Vellum
    /// bridge" at the same time as "launch configuration is inconsistent",
    /// because a development sidecar from an older build was still the process
    /// Codex Desktop held. A live bridge that is not the configured bridge is
    /// evidence against adoption, not for it.
    #[test]
    fn a_live_bridge_from_another_build_is_not_the_configured_one() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let manifest = write_minimal_launch(root);
        let other_bridge = root.join("vellum-codex-app-server-old.exe");
        std::fs::write(&other_bridge, b"old").unwrap();

        let mut status = empty_status(root, true, Vec::new());
        status.enabled = true;
        let settings = settings_for_launch(Some(&manifest), root, other_bridge);
        apply_observed_launch(root, &settings, &mut status);
        assert!(!status.bridge_observed);
        assert!(!status.active);
        assert_eq!(
            status.observed_bridge_executable,
            Some(binary_of_this_process())
        );
        assert!(status
            .blockers
            .iter()
            .any(|blocker| blocker.contains("not the configured App Server bridge")));
    }

    /// One prepared launch with a live attestation, so the observation tests
    /// can each vary a single fact about it.
    /// Reusing a launch id is only sound while the id still describes what is
    /// running. Every field the manifest carries has to be able to veto reuse.
    #[test]
    fn launch_content_equality_notices_every_field_that_matters() {
        let root = tempfile::tempdir().unwrap();
        let base = write_minimal_launch(root.path());

        // Identity is what reuse preserves, so it must not count as content.
        let mut renamed = base.clone();
        renamed.launch_id = "launch-2".into();
        renamed.created_at = base.created_at + 99;
        assert!(same_launch_content(&base, &renamed));

        // A different Enhanced binary is a different launch, even byte-for-byte
        // identical in every other respect. This is the one that matters: it is
        // the artifact Codex Desktop will execute.
        let mut swapped = base.clone();
        swapped.enhanced.artifact_sha256 = "sha256:something-else".into();
        assert!(!same_launch_content(&base, &swapped));

        for mutate in [
            (|m: &mut LaunchManifestV1| m.schema_version += 1) as fn(&mut LaunchManifestV1),
            |m| m.official.runtime_digest = "sha256:other".into(),
            |m| m.enhanced.codex_home = PathBuf::from("elsewhere"),
            |m| m.model_provider_map_sha256 = "sha256:other".into(),
            |m| m.model_provider_map_path = PathBuf::from("elsewhere"),
            |m| m.binding_db = PathBuf::from("elsewhere"),
            |m| m.official_provider_ids.push("extra".into()),
            |m| m.third_party_provider_ids.push("extra".into()),
            |m| m.feature_profile.qwen_tool_reliability = !m.feature_profile.qwen_tool_reliability,
            |m| m.enhanced_commit = "d".repeat(40),
            |m| m.attestation_path = PathBuf::from("elsewhere"),
            |m| m.qualification_journal_path = PathBuf::from("elsewhere"),
        ] {
            let mut changed = base.clone();
            mutate(&mut changed);
            assert!(
                !same_launch_content(&base, &changed),
                "a changed field was treated as the same launch: {changed:?}"
            );
        }
    }

    /// 2026-09-05: Codex Desktop updated itself while a Vellum launch was
    /// running. `adopt_discovered_official` moved the stored path to the new
    /// `bin\27d6a192e9c98618\codex.exe`, the live children stayed on
    /// `bin\9ba750cce02d5e5c\codex.exe`, and every check here compared settings
    /// against what Desktop reported — which had moved too. So the screen said
    /// Live with `restart_required` false while Desktop's updater stripped the
    /// three sibling helpers out of the directory the running core still held
    /// open, and every tool call died inside Codex with "cannot find the file
    /// specified". Only the launch manifest knew, and nobody read it.
    #[test]
    fn a_launch_left_on_a_superseded_core_asks_for_the_restart_that_fixes_it() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = write_minimal_launch(temp.path());
        let adopted = temp.path().join("official-after-desktop-update.exe");
        std::fs::write(&adopted, b"the replacement Desktop now reports").unwrap();
        let settings = DesktopRuntimeSettings {
            enabled: true,
            official_codex_executable: adopted.clone(),
            enhanced_codex_executable: manifest.enhanced.executable.clone(),
            bridge_executable: binary_of_this_process(),
        };

        let mut status = empty_status(temp.path(), true, Vec::new());
        status.enabled = true;
        assert!(apply_observed_launch(temp.path(), &settings, &mut status));
        assert!(
            status.blockers.iter().any(|blocker| {
                blocker.contains("Official core from")
                    && blocker.contains("official-after-desktop-update.exe")
            }),
            "the blocker must name both cores: {:?}",
            status.blockers
        );

        // The same launch against the path it was actually started from is the
        // ordinary case and must stay quiet — this runs on every status read.
        let settled = DesktopRuntimeSettings {
            official_codex_executable: manifest.official.executable.clone(),
            ..settings
        };
        let mut status = empty_status(temp.path(), true, Vec::new());
        status.enabled = true;
        assert!(!apply_observed_launch(temp.path(), &settled, &mut status));
        assert!(
            !status
                .blockers
                .iter()
                .any(|blocker| blocker.contains("the live launch runs")),
            "unexpected drift blocker: {:?}",
            status.blockers
        );
    }

    /// The repair is what makes the drift worth detecting, so the conditions
    /// that withhold it are the ones worth pinning: a runtime the user turned
    /// off must not be relaunched to apply that, and a stopped Proxy must not
    /// be repaired at all — the managed restart it would run disarms, so
    /// "repairing" there means stopping Codex Desktop and bringing it back on
    /// plain Official.
    #[test]
    fn a_superseded_launch_is_repaired_only_where_a_restart_would_repair_it() {
        let mut drifted = empty_status(Path::new("."), true, Vec::new());
        drifted.enabled = true;
        drifted.launch_core_drift = true;
        drifted.launch_id = Some("01LAUNCH".into());

        assert_eq!(superseded_launch_repair(&drifted, true), Some("01LAUNCH"));
        assert_eq!(
            superseded_launch_repair(&drifted, false),
            None,
            "a stopped Proxy has no armed launch to rebuild"
        );

        let disabled = DesktopRuntimeStatus {
            enabled: false,
            ..drifted.clone()
        };
        assert_eq!(superseded_launch_repair(&disabled, true), None);

        let settled = DesktopRuntimeStatus {
            launch_core_drift: false,
            ..drifted.clone()
        };
        assert_eq!(
            superseded_launch_repair(&settled, true),
            None,
            "every status read passes through here; only the fault may fire"
        );

        let unidentified = DesktopRuntimeStatus {
            launch_id: None,
            ..drifted
        };
        assert_eq!(
            superseded_launch_repair(&unidentified, true),
            None,
            "without a launch id there is nothing to spend the one attempt on"
        );
    }

    /// Settings that agree with the launch under test, so a case about
    /// something else never trips the superseded-core check.
    fn settings_for_launch(
        manifest: Option<&LaunchManifestV1>,
        root: &Path,
        bridge: PathBuf,
    ) -> DesktopRuntimeSettings {
        DesktopRuntimeSettings {
            enabled: true,
            official_codex_executable: manifest.map_or_else(
                || root.join("official.exe"),
                |m| m.official.executable.clone(),
            ),
            enhanced_codex_executable: manifest.map_or_else(
                || root.join("enhanced.exe"),
                |m| m.enhanced.executable.clone(),
            ),
            bridge_executable: bridge,
        }
    }

    fn write_minimal_launch(root: &Path) -> LaunchManifestV1 {
        let official = root.join("official.exe");
        let enhanced = root.join("enhanced.exe");
        let map = root.join("model-provider-map.json");
        std::fs::write(&official, b"official").unwrap();
        std::fs::write(&enhanced, b"enhanced").unwrap();
        std::fs::write(&map, b"{}").unwrap();
        let manifest = LaunchManifestV1 {
            schema_version: LAUNCH_MANIFEST_SCHEMA_VERSION,
            launch_id: "launch-1".into(),
            created_at: 1,
            official: RuntimeBinaryIdentity {
                artifact_sha256: sha256_file(&official).unwrap(),
                executable: official,
                runtime_digest: "sha256:official".into(),
                codex_home: root.join("official-home"),
            },
            enhanced: RuntimeBinaryIdentity {
                artifact_sha256: sha256_file(&enhanced).unwrap(),
                executable: enhanced,
                runtime_digest: "sha256:enhanced".into(),
                codex_home: root.join("enhanced-home"),
            },
            model_provider_map_sha256: sha256_file(&map).unwrap(),
            model_provider_map_path: map,
            binding_db: root.join("bindings.sqlite3"),
            official_provider_ids: vec!["openai-official".into()],
            third_party_provider_ids: vec!["qwen".into()],
            feature_profile: FEATURE_DEFAULTS_MVP.into(),
            enhanced_commit: "c".repeat(40),
            attestation_path: root.join(ATTESTATION_FILE),
            qualification_journal_path: root.join(QUALIFICATION_JOURNAL_FILE),
            relay: None,
        };
        manifest.write(&LaunchManifestV1::path_in(root)).unwrap();

        let mut writer = AttestationWriter::new(
            manifest.attestation_path.clone(),
            manifest.launch_id.clone(),
            manifest.model_provider_map_sha256.clone(),
            manifest.binding_db.clone(),
            ChildAttestation {
                binary_sha256: manifest.official.artifact_sha256.clone(),
                runtime_digest: manifest.official.runtime_digest.clone(),
                ..ChildAttestation::default()
            },
            ChildAttestation {
                binary_sha256: manifest.enhanced.artifact_sha256.clone(),
                runtime_digest: manifest.enhanced.runtime_digest.clone(),
                ..ChildAttestation::default()
            },
        );
        writer.mark_initialized(ExecutionPlane::OfficialCodex);
        writer.mark_initialized(ExecutionPlane::EnhancedCodex);
        writer.flush().unwrap();
        manifest
    }

    fn binary_of_this_process() -> PathBuf {
        std::env::current_exe().unwrap()
    }
}
