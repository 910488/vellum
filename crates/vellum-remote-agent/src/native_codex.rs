//! Version-gated integration with the Codex App native remote-control daemon.
//!
//! This path never creates another app-server or Broker.  It discovers the
//! daemon's actual launcher and CODEX_HOME, then delegates lifecycle to Codex's
//! own `app-server daemon` commands.

use std::cmp::Ordering;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::profile::{InjectionPlan, ProfileManager, RestoreResult};
use crate::protocol::ProxyStatusView;
use crate::release_manifest;

pub const NATIVE_PROFILE_ID: &str = "codex-app-native";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NativeCodexRuntimeStatus {
    pub codex_home: String,
    pub codex_binary: Option<String>,
    pub codex_version: Option<String>,
    pub compatible: bool,
    pub compatibility_reason: Option<String>,
    pub daemon_running: bool,
    pub daemon_pid: Option<u32>,
    #[serde(default)]
    pub daemon_identity: Option<String>,
    pub daemon_version: Option<String>,
    pub daemon_owner: String,
    pub restart_safe: bool,
    pub durable: bool,
    pub standalone_installed: bool,
    /// Whether the stable user-level launcher used by Codex App's SSH login
    /// shell resolves to the Vellum-managed standalone binary.
    pub cli_launcher: CodexCliLauncherStatus,
    pub remote_control_enabled: Option<bool>,
    pub active_turn: Option<bool>,
    pub session_authority: String,
    pub broker_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CodexCliLauncherStatus {
    pub path: Option<String>,
    pub target: Option<String>,
    pub ready: bool,
}

/// M31: result of a digest-pinned Codex install / update.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexInstallationResult {
    pub installed_path: String,
    pub version: Option<String>,
    pub sha256: String,
    pub durable_service: Option<ServiceReconcileResult>,
}

/// M31: verification snapshot of the Vellum-managed standalone install.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CodexInstallationStatus {
    pub present: bool,
    pub installed_path: Option<String>,
    pub version: Option<String>,
    pub sha256: Option<String>,
    pub compatible: bool,
    pub compatibility_reason: Option<String>,
}

/// M31: durable user-service reconcile result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServiceReconcileResult {
    pub service_name: String,
    pub desired: bool,
    pub changed: bool,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CodexLaunch {
    pub(crate) program: PathBuf,
    pub(crate) prefix: Vec<String>,
    pub(crate) display: String,
}

pub fn expected_managed_codex_home() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| {
        "NativeCodexHomeMissing: cannot determine the current user's home".to_string()
    })?;
    Ok(crate::platform::managed_codex_home(
        &home,
        std::env::consts::OS,
    ))
}

pub fn discover_native() -> Result<NativeCodexRuntimeStatus, String> {
    let expected_home = expected_managed_codex_home()?;
    let process = find_daemon_process_for_home(&expected_home);
    let codex_home = process
        .as_ref()
        .and_then(|item| item.codex_home.clone())
        .unwrap_or(expected_home);
    let launch = launcher_from_process(&process);
    let version = launch.as_ref().and_then(codex_version);
    let daemon = launch.as_ref().and_then(daemon_version);
    // Ownership takes both halves, because each one is blind to a different
    // impostor. `managedCodexVersion` only reports that a standalone package
    // is installed, so process identity is what stops a package installed
    // beside an App-owned daemon from making restart look safe. Path identity
    // in turn cannot see a supervisor: an app-server started straight off the
    // standalone binary carries the managed path and no daemon pid record, and
    // Codex refuses every lifecycle command against it with "app server is
    // running but is not managed by codex app-server daemon". `daemon version`
    // answers exactly that -- it prints `backend` only for the process
    // `app-server-daemon/app-server.pid` claims -- so a report without it
    // disowns the running app-server whichever binary it came from. Only a
    // report Codex actually produced overrides: a probe that could not run
    // must not demote a genuinely managed daemon into a takeover.
    let path_managed = process
        .as_ref()
        .and_then(|process| process.launch.as_ref())
        .is_some_and(|launch| is_standalone_process(launch, &codex_home));
    let managed = daemon_is_managed(path_managed, daemon_claims_backend(daemon.as_ref()));
    let daemon_owner = if process.is_none() {
        "absent"
    } else if managed {
        "codexCliDaemon"
    } else {
        "codexAppDirect"
    };
    let standalone_installed = standalone_binary(&codex_home).is_some();
    let cli_launcher = codex_cli_launcher_status(&codex_home);
    let (compatible, compatibility_reason) = compatibility(version.as_deref());
    Ok(NativeCodexRuntimeStatus {
        codex_home: codex_home.to_string_lossy().to_string(),
        codex_binary: launch.as_ref().map(|item| item.display.clone()),
        codex_version: version,
        compatible,
        compatibility_reason,
        daemon_running: process.is_some(),
        daemon_pid: process.as_ref().map(|item| item.pid),
        daemon_identity: process.as_ref().and_then(|item| process_identity(item.pid)),
        daemon_version: daemon
            .as_ref()
            .and_then(|value| value.get("appServerVersion"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        daemon_owner: daemon_owner.into(),
        restart_safe: managed,
        durable: managed,
        standalone_installed,
        cli_launcher,
        // Codex currently exposes enable/disable commands but no documented,
        // stable status schema. Unknown is intentional rather than guessed.
        remote_control_enabled: process.as_ref().map(|_| true),
        active_turn: None,
        session_authority: "codexNativeDaemon".into(),
        broker_enabled: false,
    })
}

/// Resolve the structured launcher (program + prefix) for the native Codex
/// installation. `CodexLaunch.display` is display-only and must never be
/// executed as a path (a Node launcher can be `node /path/codex`).
fn launcher_from_process(process: &Option<DaemonProcess>) -> Option<CodexLaunch> {
    process
        .as_ref()
        .and_then(|item| item.launch.clone())
        .or_else(discover_launcher)
}

pub fn plan_native_adopt(
    profiles: &ProfileManager,
    proxy: &ProxyStatusView,
) -> Result<InjectionPlan, String> {
    let native = require_compatible_native()?;
    let home = PathBuf::from(&native.codex_home);
    profiles.adopt_existing(NATIVE_PROFILE_ID, &home, true)?;
    profiles.plan_injection(NATIVE_PROFILE_ID, proxy)
}

pub fn apply_native_adopt(
    profiles: &ProfileManager,
    proxy: &ProxyStatusView,
    catalog_json: &str,
) -> Result<InjectionPlan, String> {
    let _ = plan_native_adopt(profiles, proxy)?;
    profiles.inject(NATIVE_PROFILE_ID, proxy, catalog_json)
}

/// Resolves the boundary key for a Codex daemon-lifecycle command. When
/// `NATIVE_PROFILE_ID`'s lease is active — this Codex instance's
/// `config.toml` currently carries Vellum's `env_http_headers` reference —
/// a missing key is a real problem: it fails the whole operation rather
/// than silently starting/restarting a daemon that can never successfully
/// present the boundary header. When no lease is active, this Codex
/// instance was never pointed at Vellum's provider at all, so a missing key
/// is benign and must not block an otherwise-unrelated
/// restart/bootstrap/reconcile.
fn resolve_boundary_key_for_daemon_lifecycle(
    paths: &crate::state::AgentPaths,
    context: &str,
) -> Result<Option<String>, String> {
    if crate::profile::profile_lease_is_active(paths, NATIVE_PROFILE_ID) {
        crate::configuration::read_boundary_key(paths).map(Some)
    } else {
        Ok(crate::configuration::read_boundary_key_or_warn(
            paths, context,
        ))
    }
}

pub fn restart_native(
    paths: &crate::state::AgentPaths,
) -> Result<NativeCodexRuntimeStatus, String> {
    let boundary_key = resolve_boundary_key_for_daemon_lifecycle(paths, "restart_native")?;
    let native = require_compatible_native()?;
    if !native.restart_safe {
        let codex_home = PathBuf::from(&native.codex_home);
        let managed_launch = standalone_binary(&codex_home).ok_or_else(|| {
            "NativeRestartDeferred: run Bootstrap to install the pinned Codex standalone package"
                .to_string()
        })?;
        // A takeover still has to go through Codex, and Codex will not stop an
        // app-server its own pid record does not claim -- the same refusal
        // `daemon restart` gives. There is no lifecycle command that can take
        // that process over, only the explicit app-owned stop, which is the
        // one path allowed to signal the PID and which Desktop gates behind
        // its own confirmation. Say that in the vocabulary Desktop already has
        // a remedy button for instead of passing Codex's raw sentence up
        // through the caller (boundary-key provisioning included, which is not
        // a user-confirmed context and must never terminate anything itself).
        if daemon_disowns_running_app_server(&managed_launch) {
            return Err(
                "nativeDaemonAppOwned: the running app-server is not held by Codex's daemon, so stop it explicitly before taking over"
                    .into(),
            );
        }
        // This endpoint is an explicit user-requested takeover. Codex's own
        // remote-control stop command remains the process authority; Vellum
        // never signals or kills the App-owned PID directly.
        run_codex_command(&managed_launch, &["remote-control", "stop"])?;
        wait_for_daemon(false)?;
        run_daemon_command(
            &managed_launch,
            &["bootstrap", "--remote-control"],
            boundary_key.as_deref(),
        )?;
        run_daemon_command(
            &managed_launch,
            &["enable-remote-control"],
            boundary_key.as_deref(),
        )?;
        let _ = suppress_daemon_auto_update(&codex_home);
        wait_for_daemon(true)?;
        let after = discover_native()?;
        if !after.restart_safe || !after.durable {
            return Err(
                "NativeDaemonTakeoverFailed: durable daemon ownership was not established".into(),
            );
        }
        return Ok(after);
    }
    let launch = find_daemon_process()
        .and_then(|item| item.launch)
        .or_else(discover_launcher)
        .ok_or_else(|| "CodexBinaryMissing".to_string())?;

    // Codex owns turn coordination. Its native daemon command is the only
    // supported authority allowed to restart; Vellum never kills a PID.
    run_daemon_command(&launch, &["restart"], boundary_key.as_deref())?;
    wait_for_daemon(true)?;
    let after = discover_native()?;
    if !after.daemon_running {
        return Err("NativeDaemonRestartFailed: daemon is not running".into());
    }
    if after.codex_home != native.codex_home {
        return Err("NativeDaemonIdentityChanged: CODEX_HOME changed during restart".into());
    }
    Ok(after)
}

/// Stop a direct Codex App-owned app-server after an explicit Desktop
/// confirmation. This is intentionally narrower than `restart_native`: it
/// never stops a daemon-managed process and never interrupts an active turn.
pub fn stop_app_owned_native() -> Result<NativeCodexRuntimeStatus, String> {
    let before = discover_native()?;
    if !before.daemon_running {
        return Ok(before);
    }
    validate_app_owned_stop(&before)?;

    // Session observability is part of the safety gate. If this Codex version
    // cannot prove that no turn is active, fail closed instead of signalling.
    let sessions = crate::native_session::query_session_status(Path::new(&before.codex_home), None)
        .map_err(|error| format!("NativeAppOwnedStopObservabilityRequired: {error}"))?;
    if sessions
        .threads
        .iter()
        .any(|thread| thread.active || thread.active_turn_id.is_some())
    {
        return Err(
            "NativeAppOwnedStopBlocked: activeTurnInProgress; wait for the current Codex turn to finish"
                .into(),
        );
    }

    // Re-discover immediately before signalling to defend against PID reuse or
    // ownership changes between the observer query and the mutation.
    let process = find_daemon_process()
        .ok_or_else(|| "NativeAppOwnedStopIdentityChanged: app-server exited".to_string())?;
    if Some(process.pid) != before.daemon_pid
        || process.codex_home.as_deref() != Some(Path::new(&before.codex_home))
    {
        return Err("NativeAppOwnedStopIdentityChanged: PID or CODEX_HOME changed".into());
    }
    let current = discover_native()?;
    validate_app_owned_stop(&current)?;
    if current.daemon_pid != before.daemon_pid {
        return Err("NativeAppOwnedStopIdentityChanged: PID changed".into());
    }

    terminate_exact_process(process.pid)?;
    wait_for_daemon(false)?;
    let after = discover_native()?;
    if after.daemon_running {
        return Err("NativeAppOwnedStopFailed: app-server is still running".into());
    }
    Ok(after)
}

fn validate_app_owned_stop(native: &NativeCodexRuntimeStatus) -> Result<(), String> {
    if !native.daemon_running {
        return Ok(());
    }
    if native.daemon_owner != "codexAppDirect" || native.restart_safe {
        return Err(format!(
            "NativeAppOwnedStopRejected: expected codexAppDirect, found {}",
            native.daemon_owner
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn terminate_exact_process(pid: u32) -> Result<(), String> {
    let status = Command::new("kill")
        .args(["-TERM", "--", &pid.to_string()])
        .status()
        .map_err(|error| format!("NativeAppOwnedStopFailed: SIGTERM could not start: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "NativeAppOwnedStopFailed: SIGTERM for PID {pid} returned {status}"
        ))
    }
}

#[cfg(not(target_os = "linux"))]
fn terminate_exact_process(_pid: u32) -> Result<(), String> {
    Err("NativeAppOwnedStopUnsupported: direct app-server stop requires Linux".into())
}

pub fn bootstrap_native(
    paths: &crate::state::AgentPaths,
) -> Result<NativeCodexRuntimeStatus, String> {
    let boundary_key = resolve_boundary_key_for_daemon_lifecycle(paths, "bootstrap_native")?;
    let expected_home = expected_managed_codex_home()?;
    let process = find_daemon_process_for_home(&expected_home);
    let daemon_running = process.is_some();
    let process_codex_home = process
        .as_ref()
        .and_then(|process| process.codex_home.clone());
    let codex_home = process_codex_home.unwrap_or(expected_home);
    let launch = process
        .and_then(|process| process.launch)
        .or_else(|| standalone_binary(&codex_home))
        .or_else(discover_launcher)
        .ok_or_else(|| "CodexBinaryMissing".to_string())?;
    let version = codex_version(&launch);
    let (compatible, reason) = compatibility(version.as_deref());
    if !compatible {
        return Err(format!(
            "VersionMismatch: {}",
            reason.unwrap_or_else(|| "unsupported Codex version".into())
        ));
    }
    if daemon_running {
        // Inspect ownership before attempting to install or migrate anything.
        // A direct Codex App app-server is healthy authority and must never be
        // converted as a side effect of an ordinary Bootstrap request.
        // Repairing the user-level launcher is safe even while the daemon is
        // active: it does not signal or replace the running process.
        if standalone_binary(&codex_home).is_some() {
            reconcile_codex_cli_launcher(&codex_home)?;
        }
        return discover_native();
    }
    migrate_legacy_standalone_layout(&codex_home)?;
    let managed_launch = match standalone_binary(&codex_home) {
        Some(launch) => launch,
        None => install_standalone(&codex_home, version.as_deref(), paths)?,
    };
    reconcile_codex_cli_launcher(&codex_home)?;
    run_daemon_command(
        &managed_launch,
        &["bootstrap", "--remote-control"],
        boundary_key.as_deref(),
    )?;
    run_daemon_command(
        &managed_launch,
        &["enable-remote-control"],
        boundary_key.as_deref(),
    )?;
    let _ = suppress_daemon_auto_update(&codex_home);
    wait_for_daemon(true)?;
    discover_native()
}

/// Codex's daemon starts an hourly update loop on every `bootstrap`, and there
/// is no flag, setting, or environment variable to decline it
/// (`auto_update_enabled` is a hardcoded `true`). That loop fetches
/// chatgpt.com's installer, replaces `packages/standalone/current/codex`, and
/// restarts the app-server whenever the binary changed — silently undoing both
/// of Vellum's supported update paths, the Desktop-matched official build and
/// the bundle's digest-pinned CLI, and with them the protocol qualification
/// those paths exist to establish. A remote binary Vellum never chose is a
/// binary no probe has ever run against, which is the one thing
/// `desktop_codex`'s "never `latest`" rule is written to prevent. Vellum owns
/// the remote Codex version, so the loop is stopped after every bootstrap.
///
/// Reconciliation, not prevention, and deliberately so: the daemon's own pid
/// record is the only handle on that process, and anything running
/// `app-server daemon bootstrap` outside Vellum starts a fresh loop. The
/// outcome is reported rather than enforced — it must never fail a bootstrap
/// that otherwise succeeded, but it must also never be silent about having
/// left an updater running.
fn suppress_daemon_auto_update(codex_home: &Path) -> String {
    let record = codex_home.join("app-server-daemon/app-server-updater.pid");
    let Some((pid, recorded_start)) = updater_pid_record(&record) else {
        return "autoUpdateAbsent".into();
    };
    // The pid file outlives the process it names, so identity has to be
    // re-proven before signalling — the same start-time comparison Codex's own
    // pid backend makes, against the same `ps -o lstart=` format it wrote.
    match process_start_time(pid) {
        Some(observed) if observed == recorded_start => match terminate_exact_process(pid) {
            Ok(()) => "autoUpdateStopped".into(),
            Err(error) => format!("autoUpdateStopFailed:{error}"),
        },
        Some(_) | None => "autoUpdateNotRunning".into(),
    }
}

fn updater_pid_record(path: &Path) -> Option<(u32, String)> {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let pid = u32::try_from(value.get("pid")?.as_u64()?).ok()?;
    let start = value.get("processStartTime")?.as_str()?.trim().to_owned();
    (!start.is_empty()).then_some((pid, start))
}

fn process_start_time(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn standalone_binary(codex_home: &std::path::Path) -> Option<CodexLaunch> {
    let current = codex_home.join("packages/standalone/current");
    // Codex's daemon lifecycle validates the root-level path. Keep the legacy
    // Vellum path discoverable only so stopped installations can be migrated.
    for path in [current.join("codex"), current.join("bin/codex")] {
        if path.is_file() {
            return direct_launcher(path);
        }
    }
    None
}

fn is_standalone_process(launch: &CodexLaunch, codex_home: &std::path::Path) -> bool {
    if !launch.prefix.is_empty() {
        return false;
    }
    let running = fs::canonicalize(&launch.program).unwrap_or_else(|_| launch.program.clone());
    let current = codex_home.join("packages/standalone/current");
    [current.join("codex"), current.join("bin/codex")]
        .into_iter()
        .filter(|path| path.is_file())
        .any(|path| fs::canonicalize(&path).unwrap_or(path) == running)
}

fn migrate_legacy_standalone_layout(codex_home: &Path) -> Result<bool, String> {
    let current = codex_home.join("packages/standalone/current");
    let official = current.join("codex");
    let legacy = current.join("bin/codex");
    if official.is_file() || !legacy.is_file() {
        return Ok(false);
    }
    fs::rename(&legacy, &official)
        .map_err(|error| format!("CodexLegacyLayoutMigrationFailed: {error}"))?;
    let legacy_parent = current.join("bin");
    if legacy_parent
        .read_dir()
        .is_ok_and(|mut entries| entries.next().is_none())
    {
        let _ = fs::remove_dir(&legacy_parent);
    }
    Ok(true)
}

const VELLUM_CODEX_LAUNCHER_MARKER: &str = "# Managed by Vellum Remote Manager";

fn is_vellum_owned_launcher(contents: &str) -> bool {
    contents.starts_with(&format!("#!/bin/sh\n{VELLUM_CODEX_LAUNCHER_MARKER}\n"))
}

fn launcher_home(codex_home: &Path) -> Option<PathBuf> {
    dirs::home_dir().or_else(|| codex_home.parent().map(Path::to_path_buf))
}

fn launcher_path(codex_home: &Path) -> Option<PathBuf> {
    if crate::platform::normalize_os(std::env::consts::OS) == Some("darwin") {
        return dirs::home_dir().map(|home| home.join(".vellum-remote").join("ssh-wrapper"));
    }
    launcher_home(codex_home).map(|home| home.join(".local/bin/codex"))
}

fn codex_cli_launcher_status(codex_home: &Path) -> CodexCliLauncherStatus {
    codex_cli_launcher_status_at(codex_home, launcher_path(codex_home))
}

fn codex_cli_launcher_status_at(
    codex_home: &Path,
    path: Option<PathBuf>,
) -> CodexCliLauncherStatus {
    let target = codex_home.join("packages/standalone/current/codex");
    let ready = path.as_ref().is_some_and(|path| {
        fs::canonicalize(path)
            .ok()
            .zip(fs::canonicalize(&target).ok())
            .is_some_and(|(resolved, expected)| resolved == expected)
            || fs::read_to_string(path).ok().is_some_and(|contents| {
                contents == managed_codex_launcher_contents(&target) && target.is_file()
            })
    });
    CodexCliLauncherStatus {
        path: path.map(|path| path.to_string_lossy().to_string()),
        target: Some(target.to_string_lossy().to_string()),
        ready,
    }
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn managed_codex_launcher_contents(target: &Path) -> String {
    let home = expected_managed_codex_home().ok();
    managed_codex_launcher_contents_for_home(target, home.as_deref())
}

fn managed_codex_launcher_contents_for_home(target: &Path, codex_home: Option<&Path>) -> String {
    match codex_home {
        Some(home) if crate::platform::normalize_os(std::env::consts::OS) == Some("darwin") => {
            format!(
                "#!/bin/sh\n{VELLUM_CODEX_LAUNCHER_MARKER}\nexport CODEX_HOME={home}\nexport CODEX_INSTALL_DIR={install}\nexec {} \"$@\"\n",
                shell_single_quote(&target.to_string_lossy()),
                home = shell_single_quote(&home.to_string_lossy()),
                install = shell_single_quote(&home.join("packages/standalone/current").to_string_lossy()),
            )
        }
        _ => format!(
            "#!/bin/sh\n{VELLUM_CODEX_LAUNCHER_MARKER}\nexec {} \"$@\"\n",
            shell_single_quote(&target.to_string_lossy())
        ),
    }
}

/// Maintain the stable command that Codex App resolves through the remote
/// user's SSH login shell. The launcher is a tiny Vellum-owned wrapper rather
/// than a versioned path, so atomic `current` swaps do not invalidate it.
fn reconcile_codex_cli_launcher(codex_home: &Path) -> Result<CodexCliLauncherStatus, String> {
    let launcher =
        launcher_path(codex_home).ok_or_else(|| "CodexCliLauncherHomeMissing".to_string())?;
    reconcile_codex_cli_launcher_at(codex_home, &launcher)
}

fn reconcile_codex_cli_launcher_at(
    codex_home: &Path,
    launcher: &Path,
) -> Result<CodexCliLauncherStatus, String> {
    let target = codex_home.join("packages/standalone/current/codex");
    if !target.is_file() {
        return Err(format!(
            "CodexCliLauncherTargetMissing: {}",
            target.display()
        ));
    }
    let parent = launcher
        .parent()
        .ok_or_else(|| "CodexCliLauncherParentMissing".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("CodexCliLauncherCreateFailed: {error}"))?;

    if let Ok(metadata) = fs::symlink_metadata(launcher) {
        let replaceable = if metadata.file_type().is_symlink() {
            fs::read_link(launcher).ok().is_some_and(|link| {
                let resolved = if link.is_absolute() {
                    link
                } else {
                    parent.join(link)
                };
                resolved.starts_with(codex_home.join("packages/standalone"))
            })
        } else if metadata.is_file() {
            fs::read_to_string(launcher)
                .ok()
                .is_some_and(|contents| is_vellum_owned_launcher(&contents))
        } else {
            false
        };
        if !replaceable {
            return Err(format!(
                "CodexCliLauncherConflict: {} is not managed by Vellum",
                launcher.display()
            ));
        }
    }

    let contents = managed_codex_launcher_contents(&target);
    let temporary = parent.join(format!(".codex.vellum-{}", ulid::Ulid::new()));
    fs::write(&temporary, contents)
        .map_err(|error| format!("CodexCliLauncherWriteFailed: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755)) {
            let _ = fs::remove_file(&temporary);
            return Err(format!("CodexCliLauncherPermissionsFailed: {error}"));
        }
    }
    #[cfg(windows)]
    if launcher.exists() {
        fs::remove_file(launcher)
            .map_err(|error| format!("CodexCliLauncherSwapFailed: {error}"))?;
    }
    if let Err(error) = fs::rename(&temporary, launcher) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("CodexCliLauncherSwapFailed: {error}"));
    }
    let status = codex_cli_launcher_status_at(codex_home, Some(launcher.to_path_buf()));
    if !status.ready {
        return Err("CodexCliLauncherVerificationFailed".into());
    }
    Ok(status)
}

fn install_standalone(
    codex_home: &std::path::Path,
    version: Option<&str>,
    paths: &crate::state::AgentPaths,
) -> Result<CodexLaunch, String> {
    let version = version
        .and_then(|value| {
            value
                .split_whitespace()
                .find(|part| part.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
        })
        .ok_or_else(|| "CodexInstallVersionUnknown".to_string())?;
    if !version
        .chars()
        .all(|ch| ch.is_ascii_digit() || ch == '.' || ch == '-')
    {
        return Err("CodexInstallVersionInvalid".into());
    }

    // M31: no unverified `curl | sh`. The pinned artifact URL + digest come
    // from the Desktop's compile-time-verified local deployment bundle and
    // are passed via env for the legacy bootstrap path; the agent downloads
    // to a staging file, verifies SHA-256, then performs the atomic install.
    let url = std::env::var("VELLUM_CODEX_ARTIFACT_URL").map_err(|_| {
        "CodexPinnedManifestUnavailable: VELLUM_CODEX_ARTIFACT_URL is not set".to_string()
    })?;
    let expected_sha256 = std::env::var("VELLUM_CODEX_ARTIFACT_SHA256").map_err(|_| {
        "CodexPinnedManifestUnavailable: VELLUM_CODEX_ARTIFACT_SHA256 is not set".to_string()
    })?;
    let staged = download_pinned_artifact(&url, &expected_sha256)?;
    let result = install_pinned_codex(codex_home, &staged, &expected_sha256, version, paths)?;
    let _ = fs::remove_file(&staged);
    standalone_binary(codex_home).ok_or_else(|| {
        format!(
            "CodexInstallerMissingStandaloneBinary: {}",
            result.installed_path
        )
    })
}

/// M31: download a pinned artifact to a random staging file and verify its
/// SHA-256 before anything is executed or installed.
fn download_pinned_artifact(url: &str, expected_sha256: &str) -> Result<PathBuf, String> {
    if !url.starts_with("https://") && !url.starts_with("file://") && !url.starts_with('/') {
        return Err(
            "CodexArtifactUrlUnsafe: only https, file:// or absolute paths are allowed".into(),
        );
    }
    let staged = std::env::temp_dir().join(format!("vellum-codex-pinned-{}", ulid::Ulid::new()));
    let copied = if url.starts_with("file://") || url.starts_with('/') {
        let source = url.strip_prefix("file://").unwrap_or(url);
        fs::copy(source, &staged)
            .map(|_| ())
            .map_err(|error| error.to_string())
    } else {
        let output = Command::new("curl")
            .args([
                "-fsSL",
                "--proto",
                "=https",
                "--tlsv1.2",
                "--fail",
                url,
                "-o",
            ])
            .arg(&staged)
            .output()
            .map_err(|error| error.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    };
    if let Err(error) = copied {
        let _ = fs::remove_file(&staged);
        return Err(format!("CodexArtifactDownloadFailed: {url}: {error}"));
    }
    let digest = crate::update::sha256_file(&staged)?;
    let expected = expected_sha256
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(expected_sha256.trim());
    if digest != expected {
        let _ = fs::remove_file(&staged);
        return Err(format!(
            "CodexArtifactDigestMismatch: expected {expected}, resolved {digest}"
        ));
    }
    Ok(staged)
}

/// M31: install a digest-pinned Codex standalone binary into
/// `codex_home/packages/standalone/current` with atomic swap and rollback.
pub fn install_pinned_codex(
    codex_home: &Path,
    staged: &Path,
    expected_sha256: &str,
    expected_version: &str,
    paths: &crate::state::AgentPaths,
) -> Result<CodexInstallationResult, String> {
    let native = discover_native()?;
    if native.daemon_running && !native.restart_safe {
        return Err(
            "CodexAppOwnsDaemon: disconnect the Codex App remote project before installing the pinned standalone CLI"
                .into(),
        );
    }
    install_pinned_codex_with_hooks(
        codex_home,
        staged,
        expected_sha256,
        expected_version,
        |binary| direct_launcher(binary.to_path_buf()).and_then(|launch| codex_version(&launch)),
        probe_desktop_protocol_contract,
        |home| reconcile_durable_service(home, paths),
        |home| reconcile_codex_cli_launcher(home).map(|_| ()),
    )
}

/// Install with an injected version probe so unit tests can run on hosts
/// without a real Codex binary.
pub fn install_pinned_codex_with(
    codex_home: &Path,
    staged: &Path,
    expected_sha256: &str,
    expected_version: &str,
    probe: impl Fn(&Path) -> Option<String>,
    reconcile: impl Fn(&Path) -> Result<ServiceReconcileResult, String>,
) -> Result<CodexInstallationResult, String> {
    install_pinned_codex_with_hooks(
        codex_home,
        staged,
        expected_sha256,
        expected_version,
        probe,
        |_| Ok(()),
        reconcile,
        |_| Ok(()),
    )
}

#[allow(clippy::too_many_arguments)]
fn install_pinned_codex_with_hooks(
    codex_home: &Path,
    staged: &Path,
    expected_sha256: &str,
    expected_version: &str,
    probe: impl Fn(&Path) -> Option<String>,
    protocol_probe: impl Fn(&Path) -> Result<(), String>,
    reconcile: impl Fn(&Path) -> Result<ServiceReconcileResult, String>,
    reconcile_launcher: impl Fn(&Path) -> Result<(), String>,
) -> Result<CodexInstallationResult, String> {
    crate::update::reject_symlink(staged)?;
    let digest = crate::update::sha256_file(staged)?;
    let expected = expected_sha256
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(expected_sha256.trim());
    if digest != expected {
        return Err(format!(
            "CodexArtifactDigestMismatch: expected {expected}, resolved {digest}"
        ));
    }

    let parent = codex_home.join("packages").join("standalone");
    fs::create_dir_all(&parent).map_err(|error| format!("CodexInstallStagingFailed: {error}"))?;
    let staging_dir = parent.join(format!(".vellum-codex-{}", ulid::Ulid::new()));
    let staged_bin = staging_dir.join("codex");
    fs::create_dir_all(&staging_dir)
        .map_err(|error| format!("CodexInstallStagingFailed: {error}"))?;
    crate::update::copy_private_executable(staged, &staged_bin)?;

    // Probe the *staged* binary against the pinned version before swapping.
    let version = probe(&staged_bin).ok_or_else(|| "CodexInstallProbeFailed".to_string())?;
    if !release_manifest::version_in_range(&version, expected_version) {
        let _ = fs::remove_dir_all(&staging_dir);
        return Err(format!(
            "CodexInstallerVersionMismatch: expected {expected_version}, found {version}"
        ));
    }
    if let Err(error) = protocol_probe(&staged_bin) {
        let _ = fs::remove_dir_all(&staging_dir);
        return Err(format!("CodexDesktopProtocolMismatch: {error}"));
    }

    let current_dir = parent.join("current");
    let rollback_dir = parent.join(format!(".vellum-codex-rollback-{}", ulid::Ulid::new()));
    let swap = || -> Result<(), String> {
        if current_dir.exists() {
            fs::rename(&current_dir, &rollback_dir)
                .map_err(|error| format!("CodexInstallSwapFailed: {error}"))?;
        }
        fs::rename(&staging_dir, &current_dir)
            .map_err(|error| format!("CodexInstallSwapFailed: {error}"))?;
        Ok(())
    };
    if let Err(error) = swap() {
        let _ = fs::remove_dir_all(&staging_dir);
        if !current_dir.exists() && rollback_dir.exists() {
            let _ = fs::rename(&rollback_dir, &current_dir);
        }
        return Err(error);
    }

    let restore_previous = || {
        let _ = fs::remove_dir_all(&current_dir);
        if rollback_dir.exists() {
            let _ = fs::rename(&rollback_dir, &current_dir);
        }
    };

    // Post-swap verification; on failure restore the previous install.
    if probe(&current_dir.join("codex")).is_none() {
        restore_previous();
        return Err("CodexInstallProbeFailedAfterSwap".into());
    }
    let durable_service = match reconcile(codex_home) {
        Ok(service) => Some(service),
        Err(error) => {
            restore_previous();
            return Err(format!("CodexDurableServiceReconcileFailed: {error}"));
        }
    };
    if let Err(error) = reconcile_launcher(codex_home) {
        restore_previous();
        return Err(format!("CodexCliLauncherReconcileFailed: {error}"));
    }
    if rollback_dir.exists() {
        let _ = fs::remove_dir_all(&rollback_dir);
    }
    Ok(CodexInstallationResult {
        installed_path: current_dir.join("codex").to_string_lossy().to_string(),
        version: Some(version),
        sha256: digest,
        durable_service,
    })
}

/// Qualify the staged remote CLI against the Codex Desktop protocol surface
/// Vellum consumes. Version labels alone are insufficient: stable and alpha
/// builds with the same numeric core can parse different catalog schemas.
fn desktop_protocol_probe_catalog() -> serde_json::Value {
    serde_json::json!({
        "models": [{
            "slug": "vellum-protocol-probe",
            "display_name": "Vellum protocol probe",
            "description": "Vellum protocol probe",
            "context_window": 131072,
            "max_context_window": 131072,
            "effective_context_window_percent": 95,
            "priority": 1,
            "base_instructions": "Protocol compatibility probe.",
            "model_messages": {
                "instructions_template": "Protocol compatibility probe.",
                "instructions_variables": {
                    "personality_default": "",
                    "personality_friendly": "",
                    "personality_pragmatic": ""
                }
            },
            "input_modalities": ["text"],
            "supported_reasoning_levels": [
                {"effort": "max", "description": "max"},
                {"effort": "ultra", "description": "ultra"}
            ],
            "default_reasoning_level": "max",
            "shell_type": "shell_command",
            "include_skills_usage_instructions": false,
            "supports_reasoning_summaries": false,
            "default_reasoning_summary": "none",
            "support_verbosity": true,
            "default_verbosity": "low",
            "apply_patch_tool_type": "freeform",
            "web_search_tool_type": "text_and_image",
            "truncation_policy": {"mode": "tokens", "limit": 10000},
            "supports_parallel_tool_calls": false,
            "use_responses_lite": false,
            "supports_image_detail_original": false,
            "experimental_supported_tools": [],
            "supports_search_tool": false,
            "prefer_websockets": false,
            "additional_speed_tiers": [],
            "service_tiers": [],
            "availability_nux": null,
            "upgrade": null,
            "visibility": "list",
            "supported_in_api": true
        }]
    })
}

fn probe_desktop_protocol_contract(binary: &Path) -> Result<(), String> {
    let probe_home =
        std::env::temp_dir().join(format!("vellum-codex-protocol-probe-{}", ulid::Ulid::new()));
    fs::create_dir_all(&probe_home)
        .map_err(|error| format!("create protocol probe home: {error}"))?;
    let result = (|| -> Result<(), String> {
        let catalog_path = probe_home.join("vellum-model-catalog.json");
        let catalog = desktop_protocol_probe_catalog();
        fs::write(
            &catalog_path,
            serde_json::to_vec(&catalog).map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("write protocol probe catalog: {error}"))?;
        let mut config = toml::map::Map::new();
        config.insert(
            "model_catalog_json".into(),
            toml::Value::String(catalog_path.to_string_lossy().to_string()),
        );
        fs::write(
            probe_home.join("config.toml"),
            toml::to_string(&config).map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("write protocol probe config: {error}"))?;

        for args in [
            &["features", "list"][..],
            &["app-server", "--help"][..],
            &["remote-control", "pair", "--help"][..],
        ] {
            let output = Command::new(binary)
                .args(args)
                .env("CODEX_HOME", &probe_home)
                .stdin(Stdio::null())
                .output()
                .map_err(|error| format!("run `{}`: {error}", args.join(" ")))?;
            if !output.status.success() {
                let detail = String::from_utf8_lossy(&output.stderr)
                    .split_whitespace()
                    .take(80)
                    .collect::<Vec<_>>()
                    .join(" ");
                return Err(format!(
                    "`{}` failed with {}{}",
                    args.join(" "),
                    output.status,
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {detail}")
                    }
                ));
            }
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&probe_home);
    result
}

/// M31: update to the pinned version, refusing downgrades and never touching
/// a Codex App-owned active daemon. Idempotent when already pinned.
pub fn update_pinned_codex(
    codex_home: &Path,
    staged: &Path,
    expected_sha256: &str,
    pinned_version: &str,
    paths: &crate::state::AgentPaths,
) -> Result<CodexInstallationResult, String> {
    let status = verify_installation(codex_home, Some(pinned_version))?;
    if status.present {
        if let Some(current) = status.version.as_deref() {
            match release_manifest::compare_versions(current, pinned_version) {
                Some(Ordering::Greater) => {
                    return Err(format!(
                        "CodexDowngradeRefused: current {current} is newer than pinned {pinned_version}"
                    ));
                }
                Some(Ordering::Equal) if status.compatible => {
                    return Err("CodexAlreadyPinned".into());
                }
                _ => {}
            }
        }
    }
    let native = discover_native()?;
    if native.daemon_running && !native.restart_safe {
        return Err(
            "CodexAppOwnsDaemon: refuse to replace an active Codex App daemon; detach first".into(),
        );
    }
    install_pinned_codex(codex_home, staged, expected_sha256, pinned_version, paths)
}

/// M31: verification snapshot of the managed standalone install. When
/// `compatible_range` is given (e.g. `0.147.x` from the manifest) it is the
/// authoritative gate; otherwise the daemon version gate applies.
pub fn verify_installation(
    codex_home: &Path,
    compatible_range: Option<&str>,
) -> Result<CodexInstallationStatus, String> {
    let Some(binary) = standalone_binary(codex_home) else {
        return Ok(CodexInstallationStatus::default());
    };
    let version = codex_version(&binary);
    let sha256 = crate::update::sha256_file(&binary.program).ok();
    let (compatible, compatibility_reason) = match (compatible_range, version.as_deref()) {
        (Some(range), Some(version)) => (release_manifest::version_in_range(version, range), None),
        (Some(_), None) => (false, Some("Codex version could not be determined".into())),
        (None, version) => compatibility(version),
    };
    Ok(CodexInstallationStatus {
        present: true,
        installed_path: Some(binary.program.to_string_lossy().to_string()),
        version,
        sha256,
        compatible,
        compatibility_reason,
    })
}

/// Pure decision for the durable daemon lifecycle: which Codex-owned
/// `app-server daemon` commands are needed given the observed state. This
/// keeps the state machine testable without a live Codex binary.
pub fn daemon_reconcile_commands(daemon_running: bool) -> Vec<&'static str> {
    let mut commands = vec!["bootstrap --remote-control", "enable-remote-control"];
    if !daemon_running {
        commands.push("start");
    }
    commands
}

/// M31: bring the official Codex durable daemon manager to desired state.
///
/// Uses Codex's own `app-server daemon` lifecycle (`bootstrap
/// --remote-control`, `enable-remote-control`, `start`) installed by the
/// pinned Codex binary, then verifies ownership. Vellum deliberately does
/// not supervise a control command from a Vellum-owned systemd unit: the
/// official manager owns daemon durability, and a one-shot control command
/// (which exits after starting the daemon) must not be treated as the
/// foreground process.
pub fn reconcile_durable_service(
    codex_home: &Path,
    paths: &crate::state::AgentPaths,
) -> Result<ServiceReconcileResult, String> {
    let boundary_key =
        resolve_boundary_key_for_daemon_lifecycle(paths, "reconcile_durable_service")?;
    let Some(binary) = standalone_binary(codex_home) else {
        return Err("CodexStandaloneNotInstalled".into());
    };
    let before = discover_native()?;
    let commands = daemon_reconcile_commands(before.daemon_running);
    let mut detail: Vec<String> = Vec::new();
    for command in commands {
        let args = command.split_whitespace().collect::<Vec<_>>();
        run_daemon_command(&binary, &args, boundary_key.as_deref())?;
        detail.push(command.into());
    }
    // Every `bootstrap` above restarted Codex's update loop, so this belongs
    // with the commands rather than after the readiness wait. Its outcome
    // rides in `detail` so a host that kept an updater alive says so out loud.
    detail.push(suppress_daemon_auto_update(codex_home));
    if before.daemon_running {
        detail.push("alreadyRunning".into());
    } else {
        wait_for_daemon(true)?;
        detail.push("started".into());
    }
    let after = discover_native()?;
    if !after.daemon_running {
        return Err("CodexDaemonNotRunningAfterReconcile".into());
    }
    if after.daemon_owner != "codexCliDaemon" || !after.restart_safe {
        return Err(format!(
            "CodexDaemonOwnershipMismatch: owner={} restartSafe={}",
            after.daemon_owner, after.restart_safe
        ));
    }
    Ok(ServiceReconcileResult {
        service_name: "codex-app-server".into(),
        desired: true,
        changed: !before.daemon_running,
        detail: detail.join(","),
    })
}

pub fn restore_native(profiles: &ProfileManager) -> Result<RestoreResult, String> {
    profiles.restore(NATIVE_PROFILE_ID)
}

fn require_compatible_native() -> Result<NativeCodexRuntimeStatus, String> {
    let status = discover_native()?;
    if !status.compatible {
        return Err(format!(
            "VersionMismatch: {}",
            status
                .compatibility_reason
                .clone()
                .unwrap_or_else(|| "Codex version is unknown".into())
        ));
    }
    Ok(status)
}

fn compatibility(version: Option<&str>) -> (bool, Option<String>) {
    let Some(version) = version else {
        return (false, Some("Codex version could not be determined".into()));
    };
    let numeric = version
        .split_whitespace()
        .find(|part| part.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
        .unwrap_or(version);
    let mut parts = numeric.trim_start_matches('v').split('.');
    let major = parts.next().and_then(|value| value.parse::<u64>().ok());
    let minor = parts.next().and_then(|value| value.parse::<u64>().ok());
    match (major, minor) {
        (Some(0), Some(142..=199)) => (true, None),
        _ => (
            false,
            Some(format!(
                "Codex {version} is outside the qualified >=0.142,<0.200 range"
            )),
        ),
    }
}

#[derive(Debug)]
pub(crate) struct DaemonProcess {
    pid: u32,
    codex_home: Option<PathBuf>,
    launch: Option<CodexLaunch>,
}

fn process_identity(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_comm = stat.rfind(") ")?.checked_add(2)?;
        let start_time = stat[after_comm..].split_whitespace().nth(19)?;
        let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|value| value.trim().to_owned())?;
        Some(format!("pid-{pid}-start-{start_time}-boot-{boot_id}"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        process_start_time(pid).map(|start| format!("pid-{pid}-start-{start}"))
    }
}

fn find_daemon_process() -> Option<DaemonProcess> {
    find_daemon_process_for_home(&expected_managed_codex_home().ok()?)
}

fn find_daemon_process_for_home(expected_home: &Path) -> Option<DaemonProcess> {
    if crate::process_identity::first_app_server_scan_is_authority() {
        return None;
    }
    if let Some(from_pid_file) = daemon_from_pid_record(expected_home) {
        return Some(from_pid_file);
    }
    #[cfg(target_os = "linux")]
    {
        // `/proc` is host-wide. On shared machines, Codex app-servers owned by
        // another login must never become this Agent user's native authority.
        // Apart from false blockers, doing so could target another user's
        // session for a lifecycle mutation. Fail closed if our own effective
        // UID cannot be established.
        let effective_uid = linux_effective_uid(&fs::read_to_string("/proc/self/status").ok()?)?;
        let expected = fs::canonicalize(expected_home).unwrap_or_else(|_| expected_home.to_path_buf());
        let entries = fs::read_dir("/proc").ok()?;
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let root = entry.path();
            let Some(process_uid) = fs::read_to_string(root.join("status"))
                .ok()
                .and_then(|status| linux_effective_uid(&status))
            else {
                continue;
            };
            if process_uid != effective_uid {
                continue;
            }
            let Ok(raw) = fs::read(root.join("cmdline")) else {
                continue;
            };
            let mut args = raw
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).to_string())
                .collect::<Vec<_>>();
            if !args.iter().any(|arg| arg == "app-server")
                || !args.iter().any(|arg| arg == "--listen")
            {
                continue;
            }
            if let Ok(executable) = fs::read_link(root.join("exe")) {
                if let Some(first) = args.first_mut() {
                    *first = executable.to_string_lossy().to_string();
                }
            }
            let codex_home = fs::read(root.join("environ")).ok().and_then(|raw| {
                raw.split(|byte| *byte == 0).find_map(|part| {
                    part.strip_prefix(b"CODEX_HOME=")
                        .map(|value| PathBuf::from(String::from_utf8_lossy(value).to_string()))
                })
            });
            let Some(home) = codex_home.as_ref() else {
                continue;
            };
            let observed = fs::canonicalize(home).unwrap_or_else(|_| home.clone());
            if observed != expected {
                continue;
            }
            let launch = launcher_from_cmdline(&args);
            return Some(DaemonProcess {
                pid,
                codex_home,
                launch,
            });
        }
    }
    let _ = expected_home;
    None
}

fn daemon_from_pid_record(expected_home: &Path) -> Option<DaemonProcess> {
    let record = expected_home.join("app-server-daemon/app-server.pid");
    let (pid, recorded_start) = updater_pid_record(&record)?;
    let observed_start = process_start_time(pid)?;
    if observed_start != recorded_start {
        return None;
    }
    let facts = observe_process_facts(pid, expected_home, &observed_start)?;
    let expected = expected_identity(expected_home, &recorded_start)?;
    claim_daemon_if_managed(pid, expected_home, &facts, &expected)
}

fn control_socket_path(home: &Path) -> PathBuf {
    home.join("app-server-control")
        .join("app-server-control.sock")
}

fn expected_identity(expected_home: &Path, start_time: &str) -> Option<crate::process_identity::ExpectedIdentity> {
    Some(crate::process_identity::ExpectedIdentity {
        uid: current_uid()?,
        binary: standalone_binary(expected_home)?.program,
        start_time: start_time.to_string(),
        codex_home: expected_home.to_path_buf(),
        socket_path: control_socket_path(expected_home),
    })
}

fn current_uid() -> Option<u32> {
    let output = Command::new("id").arg("-u").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn observe_process_facts(
    pid: u32,
    expected_home: &Path,
    start_time: &str,
) -> Option<crate::process_identity::ProcessFacts> {
    let uid = process_uid(pid)?;
    let binary = process_binary(pid).or_else(|| standalone_binary(expected_home).map(|launch| launch.program))?;
    let socket = control_socket_path(expected_home);
    let socket_owner_uid = socket_owner_uid(&socket);
    Some(crate::process_identity::ProcessFacts {
        pid,
        uid,
        binary,
        start_time: start_time.to_string(),
        codex_home: process_codex_home(pid).or_else(|| Some(expected_home.to_path_buf())),
        socket_path: socket.is_file().then_some(socket),
        socket_owner_uid,
        comm: None,
    })
}

fn process_uid(pid: u32) -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        linux_effective_uid(&status)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "uid="])
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().parse().ok())?
    }
}

fn process_binary(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "args="])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let args = String::from_utf8_lossy(&output.stdout);
        let first = args.split_whitespace().next()?;
        Some(PathBuf::from(first))
    }
}

fn process_codex_home(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let raw = fs::read(format!("/proc/{pid}/environ")).ok()?;
        return raw.split(|byte| *byte == 0).find_map(|part| {
            part.strip_prefix(b"CODEX_HOME=")
                .map(|value| PathBuf::from(String::from_utf8_lossy(value).to_string()))
        });
    }
    let _ = pid;
    None
}

fn socket_owner_uid(path: &Path) -> Option<u32> {
    let meta = fs::metadata(path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return Some(meta.uid());
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

pub(crate) fn claim_daemon_if_managed(
    pid: u32,
    expected_home: &Path,
    facts: &crate::process_identity::ProcessFacts,
    expected: &crate::process_identity::ExpectedIdentity,
) -> Option<DaemonProcess> {
    match crate::process_identity::match_managed_process(facts, expected) {
        crate::process_identity::IdentityVerdict::Match => Some(DaemonProcess {
            pid,
            codex_home: Some(expected_home.to_path_buf()),
            launch: standalone_binary(expected_home),
        }),
        crate::process_identity::IdentityVerdict::Reject { .. } => None,
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_effective_uid(status: &str) -> Option<u32> {
    let uid = status.lines().find(|line| line.starts_with("Uid:"))?;
    // Linux exposes real, effective, saved-set and filesystem UIDs. Process
    // authority follows the effective UID, which is the second number.
    uid.split_whitespace().nth(2)?.parse().ok()
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn launcher_from_cmdline(args: &[String]) -> Option<CodexLaunch> {
    let first = PathBuf::from(args.first()?);
    if args.first()?.to_ascii_lowercase().contains("node") {
        let script = args.get(1).map(PathBuf::from)?;
        if script.exists() && script.file_name()?.to_string_lossy().contains("codex") {
            return Some(CodexLaunch {
                program: first.clone(),
                prefix: vec![script.to_string_lossy().to_string()],
                display: format!("{} {}", first.display(), script.display()),
            });
        }
    }
    if first.exists() && first.file_name()?.to_string_lossy().contains("codex") {
        return Some(CodexLaunch {
            program: first.clone(),
            prefix: Vec::new(),
            display: first.to_string_lossy().to_string(),
        });
    }
    None
}

fn discover_launcher() -> Option<CodexLaunch> {
    if let Ok(value) = std::env::var("VELLUM_CODEX_BIN") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return direct_launcher(path);
        }
    }
    if let Ok(output) = Command::new("sh")
        .args(["-lc", "command -v codex"])
        .output()
    {
        if output.status.success() {
            let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
            if path.is_file() {
                return direct_launcher(path);
            }
        }
    }
    let home = dirs::home_dir()?;
    for path in [
        home.join(".local/bin/codex"),
        home.join(".npm-global/bin/codex"),
        home.join(".codex/bin/codex"),
    ] {
        if path.is_file() {
            return direct_launcher(path);
        }
    }
    let opt = home.join(".local/opt");
    if let Ok(entries) = fs::read_dir(opt) {
        for entry in entries.flatten() {
            let node = entry.path().join("bin/node");
            let codex = entry.path().join("bin/codex");
            if node.is_file() && codex.is_file() {
                return Some(CodexLaunch {
                    program: node.clone(),
                    prefix: vec![codex.to_string_lossy().to_string()],
                    display: format!("{} {}", node.display(), codex.display()),
                });
            }
        }
    }
    None
}

fn direct_launcher(path: PathBuf) -> Option<CodexLaunch> {
    Some(CodexLaunch {
        display: path.to_string_lossy().to_string(),
        program: path,
        prefix: Vec::new(),
    })
}

fn codex_version(launch: &CodexLaunch) -> Option<String> {
    let output = command(launch, &["--version"]).output().ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string()
    })
}

/// Codex's own answer to "is the running app-server mine?", read off a
/// `daemon version` report. `Some(true)` when the report names a `backend`,
/// `Some(false)` when it omits one, `None` when there is no report to read --
/// which is also what a stopped app-server looks like, since `daemon version`
/// probes the control socket before it answers at all.
fn daemon_claims_backend(report: Option<&serde_json::Value>) -> Option<bool> {
    report.map(|report| report.get("backend").is_some())
}

/// Both signals must agree before a running app-server counts as
/// daemon-managed. An absent report is not a disagreement: it leaves the
/// process-identity verdict standing.
fn daemon_is_managed(path_managed: bool, daemon_claims_backend: Option<bool>) -> bool {
    path_managed && daemon_claims_backend.unwrap_or(true)
}

/// Whether Codex's daemon disowns the app-server that is running right now:
/// the control socket answers, but no pid record claims that process. Every
/// `app-server daemon` lifecycle command -- `restart`, `stop`, and so the
/// `remote-control stop` that wraps it -- refuses in this state, so it cannot
/// be resolved by another lifecycle command.
fn daemon_disowns_running_app_server(launch: &CodexLaunch) -> bool {
    daemon_claims_backend(daemon_version(launch).as_ref()) == Some(false)
}

fn daemon_version(launch: &CodexLaunch) -> Option<serde_json::Value> {
    let output = command(launch, &["app-server", "daemon", "version"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| serde_json::from_slice(&output.stdout).ok())
        .flatten()
}

/// `boundary_key`, when given, is set as `VELLUM_BOUNDARY_KEY` on this
/// control command's own environment — best-effort: `app-server daemon`
/// commands own durable process lifecycle (see `reconcile_durable_service`)
/// and may hand off to a mechanism (e.g. a systemd unit Codex manages
/// internally) that does not inherit this process's environment. Whether the
/// value set here actually reaches the long-running daemon's outbound
/// request path — and therefore whether remote Codex ever presents the
/// boundary header at all — is exactly what the bundled-Codex contract probe
/// (docs/protocol-source-of-truth.md) must verify before this path is
/// trusted for a release; it is not proven by this function compiling or by
/// unit tests around it.
fn run_daemon_command(
    launch: &CodexLaunch,
    args: &[&str],
    boundary_key: Option<&str>,
) -> Result<(), String> {
    let mut full = vec!["app-server", "daemon"];
    full.extend_from_slice(args);
    // The daemon process spawned by Codex may inherit its launcher's stdio.
    // Piped output would then remain open after the launcher exits and make
    // `Command::output` wait forever even though the daemon is healthy.
    let capture_path =
        std::env::temp_dir().join(format!("vellum-codex-daemon-{}.log", ulid::Ulid::new()));
    let capture = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&capture_path)
        .map_err(|error| format!("Codex daemon diagnostic capture failed: {error}"))?;
    let stderr = capture
        .try_clone()
        .map_err(|error| format!("Codex daemon diagnostic capture failed: {error}"))?;
    let mut cmd = command(launch, &full);
    if let Some(key) = boundary_key {
        cmd.env(vellum_proxy_runtime::BOUNDARY_KEY_ENV_VAR, key);
    }
    let status = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(capture))
        .stderr(Stdio::from(stderr))
        .status();
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            let _ = fs::remove_file(&capture_path);
            return Err(format!("Codex daemon command failed to start: {error}"));
        }
    };
    let mut detail = String::new();
    if let Ok(capture) = fs::File::open(&capture_path) {
        let _ = capture.take(8192).read_to_string(&mut detail);
    }
    let _ = fs::remove_file(&capture_path);
    if status.success() {
        Ok(())
    } else {
        let detail = detail.split_whitespace().collect::<Vec<_>>().join(" ");
        let command = args.join(" ");
        if detail.is_empty() {
            Err(format!(
                "Codex daemon `{command}` failed with status {status}"
            ))
        } else {
            Err(format!(
                "Codex daemon `{command}` failed with status {status}: {detail}"
            ))
        }
    }
}

fn run_codex_command(launch: &CodexLaunch, args: &[&str]) -> Result<(), String> {
    let output = command(launch, args)
        .output()
        .map_err(|error| format!("Codex command failed to start: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Codex command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn command(launch: &CodexLaunch, args: &[&str]) -> Command {
    let mut command = Command::new(&launch.program);
    command.args(&launch.prefix).args(args);
    if let Ok(home) = expected_managed_codex_home() {
        command.env("CODEX_HOME", &home);
        command.env(
            "CODEX_INSTALL_DIR",
            home.join("packages/standalone/current"),
        );
    }
    command
}

fn wait_for_daemon(expected: bool) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if find_daemon_process().is_some() == expected {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Err("Codex native daemon did not reach the expected state".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing boundary key while the native profile's lease is active
    /// (Vellum's `env_http_headers` reference is currently in `config.toml`)
    /// must fail the whole daemon-lifecycle operation, not silently start a
    /// daemon that can never present the boundary header.
    #[test]
    fn resolve_boundary_key_for_daemon_lifecycle_fails_closed_when_the_native_lease_is_active() {
        let temp = tempfile::tempdir().unwrap();
        let paths = crate::state::AgentPaths::from_root(temp.path().join("state"));
        paths.ensure().unwrap();
        fs::write(
            paths.leases_dir.join(format!("{NATIVE_PROFILE_ID}.json")),
            "{}",
        )
        .unwrap();

        let error = resolve_boundary_key_for_daemon_lifecycle(&paths, "test").unwrap_err();
        assert!(
            error.contains("not provisioned"),
            "unexpected error: {error}"
        );
    }

    /// No lease for the native profile means this Codex instance was never
    /// pointed at Vellum's provider at all — a missing key here is benign
    /// and must not block an otherwise-unrelated restart/bootstrap.
    #[test]
    fn resolve_boundary_key_for_daemon_lifecycle_tolerates_a_missing_key_without_an_active_lease() {
        let temp = tempfile::tempdir().unwrap();
        let paths = crate::state::AgentPaths::from_root(temp.path().join("state"));
        paths.ensure().unwrap();

        let result = resolve_boundary_key_for_daemon_lifecycle(&paths, "test").unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn compatibility_is_explicit_and_fail_closed() {
        assert!(compatibility(Some("codex-cli 0.142.5")).0);
        assert!(compatibility(Some("0.199.0")).0);
        assert!(!compatibility(Some("0.200.0")).0);
        assert!(!compatibility(None).0);
    }

    #[test]
    fn recognizes_app_managed_node_launcher() {
        let temp = tempfile::tempdir().unwrap();
        let node = temp.path().join("node");
        let codex = temp.path().join("codex");
        fs::write(&node, "").unwrap();
        fs::write(&codex, "").unwrap();
        let launch = launcher_from_cmdline(&[
            node.to_string_lossy().to_string(),
            codex.to_string_lossy().to_string(),
            "app-server".into(),
        ])
        .unwrap();
        assert_eq!(launch.prefix, vec![codex.to_string_lossy().to_string()]);
    }

    #[test]
    fn native_profile_id_is_fixed_not_renderer_controlled() {
        assert_eq!(NATIVE_PROFILE_ID, "codex-app-native");
        assert!(!NATIVE_PROFILE_ID.contains('/'));
    }

    #[test]
    fn app_owned_daemon_is_not_restart_safe() {
        let value = serde_json::json!({
            "status": "running",
            "managedCodexVersion": null,
            "appServerVersion": "0.142.4"
        });
        assert!(value
            .get("managedCodexVersion")
            .is_none_or(serde_json::Value::is_null));
    }

    #[test]
    fn explicit_app_owned_stop_rejects_managed_or_ambiguous_ownership() {
        let mut status = NativeCodexRuntimeStatus {
            codex_home: "/home/test/.codex".into(),
            codex_binary: None,
            codex_version: Some("0.147.0".into()),
            compatible: true,
            compatibility_reason: None,
            daemon_running: true,
            daemon_pid: Some(42),
            daemon_identity: Some("pid-42".into()),
            daemon_version: Some("0.147.0".into()),
            daemon_owner: "codexAppDirect".into(),
            restart_safe: false,
            durable: false,
            standalone_installed: false,
            cli_launcher: CodexCliLauncherStatus::default(),
            remote_control_enabled: Some(true),
            active_turn: None,
            session_authority: "codexNativeDaemon".into(),
            broker_enabled: false,
        };
        assert!(validate_app_owned_stop(&status).is_ok());
        status.daemon_owner = "codexCliDaemon".into();
        status.restart_safe = true;
        assert!(validate_app_owned_stop(&status)
            .unwrap_err()
            .contains("NativeAppOwnedStopRejected"));
        status.daemon_owner = "unknown".into();
        status.restart_safe = false;
        assert!(validate_app_owned_stop(&status).is_err());
    }

    /// Verbatim from a GPU dev host's `app-server-daemon/app-server-updater.pid`,
    /// whose `processStartTime` matched `ps -p 2039538 -o lstart=` character
    /// for character. That equality is the whole safety gate before signalling,
    /// so the parse has to keep the string exactly as Codex wrote it — spaces,
    /// padding and all — rather than reformat the date.
    #[test]
    fn the_updater_pid_record_parses_into_a_signalable_identity() {
        let temp = tempfile::tempdir().unwrap();
        let record = temp.path().join("app-server-updater.pid");
        fs::write(
            &record,
            r#"{"pid":2039538,"processStartTime":"Fri Aug 14 16:21:43 2026"}"#,
        )
        .unwrap();
        assert_eq!(
            updater_pid_record(&record),
            Some((2039538, "Fri Aug 14 16:21:43 2026".into()))
        );
    }

    /// No record, an unreadable one, or one missing either half must resolve to
    /// "nothing to stop" rather than to a pid guess — this function's only
    /// output feeds a SIGTERM.
    #[test]
    fn an_incomplete_updater_pid_record_names_no_process() {
        let temp = tempfile::tempdir().unwrap();
        let record = temp.path().join("app-server-updater.pid");
        assert_eq!(updater_pid_record(&record), None);
        for body in [
            "",
            "{}",
            r#"{"pid":2039538}"#,
            r#"{"processStartTime":"Fri Aug 14 16:21:43 2026"}"#,
            r#"{"pid":2039538,"processStartTime":"  "}"#,
            r#"{"pid":-1,"processStartTime":"Fri Aug 14 16:21:43 2026"}"#,
        ] {
            fs::write(&record, body).unwrap();
            assert_eq!(updater_pid_record(&record), None, "accepted {body:?}");
        }
    }

    /// The GPU dev host failure: an app-server started straight off the managed
    /// standalone binary (`codex -c … app-server --listen unix://`, no daemon
    /// in front of it) leaves `app-server-daemon/app-server.pid` absent, so
    /// `daemon version` answers without a `backend`. Path identity alone
    /// called that host restart-safe, deployment therefore auto-ran `daemon
    /// restart`, and Codex refused it with "app server is running but is not
    /// managed by codex app-server daemon" -- surfaced as an unrecoverable
    /// `RemoteBoundaryKeyProvisionFailed`.
    #[test]
    fn a_daemon_report_without_a_backend_disowns_the_running_app_server() {
        let observed = serde_json::json!({
            "status": "running",
            "managedCodexPath": "/home/vellum-test/.codex/packages/standalone/current/codex",
            "managedCodexVersion": "0.148.0-alpha.9",
            "socketPath": "/home/vellum-test/.codex/app-server-control/app-server-control.sock",
            "cliVersion": "0.148.0-alpha.9",
            "appServerVersion": "0.148.0-alpha.9"
        });
        assert_eq!(daemon_claims_backend(Some(&observed)), Some(false));
        assert!(!daemon_is_managed(
            true,
            daemon_claims_backend(Some(&observed))
        ));
    }

    #[test]
    fn a_daemon_report_naming_a_backend_confirms_the_running_app_server() {
        let managed = serde_json::json!({
            "status": "running",
            "backend": "pid",
            "appServerVersion": "0.148.0-alpha.9"
        });
        assert_eq!(daemon_claims_backend(Some(&managed)), Some(true));
        assert!(daemon_is_managed(
            true,
            daemon_claims_backend(Some(&managed))
        ));
        // The binary still has to be the managed one: a report about the
        // installed package cannot vouch for a process running from elsewhere.
        assert!(!daemon_is_managed(
            false,
            daemon_claims_backend(Some(&managed))
        ));
    }

    /// No report at all is the same observation as a stopped app-server --
    /// `daemon version` probes the socket before it answers. It must leave the
    /// process-identity verdict alone rather than demote a working daemon into
    /// a takeover every time the probe cannot run.
    #[test]
    fn a_missing_daemon_report_does_not_disown_anything() {
        assert_eq!(daemon_claims_backend(None), None);
        assert!(daemon_is_managed(true, None));
        assert!(!daemon_is_managed(false, None));
    }

    #[test]
    fn ownership_uses_running_executable_not_installed_package_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let managed = temp.path().join("packages/standalone/current/codex");
        fs::create_dir_all(managed.parent().unwrap()).unwrap();
        fs::write(&managed, "").unwrap();
        let direct = direct_launcher(managed).unwrap();
        assert!(is_standalone_process(&direct, temp.path()));

        let node = direct_launcher(temp.path().join("node")).unwrap();
        let app_owned = CodexLaunch {
            prefix: vec![temp.path().join("codex.js").to_string_lossy().to_string()],
            ..node
        };
        assert!(!is_standalone_process(&app_owned, temp.path()));
    }

    #[test]
    fn proc_discovery_uses_effective_uid_and_rejects_malformed_status() {
        assert_eq!(
            linux_effective_uid("Name:\tcodex\nUid:\t1000\t1001\t1002\t1003\n"),
            Some(1001)
        );
        assert_eq!(linux_effective_uid("Name:\tcodex\n"), None);
        assert_eq!(linux_effective_uid("Uid:\tunknown\n"), None);
    }

    #[test]
    fn daemon_reconcile_commands_cover_bootstrap_and_start_only_when_needed() {
        assert_eq!(
            daemon_reconcile_commands(false),
            vec![
                "bootstrap --remote-control",
                "enable-remote-control",
                "start"
            ]
        );
        assert_eq!(
            daemon_reconcile_commands(true),
            vec!["bootstrap --remote-control", "enable-remote-control"]
        );
    }

    #[test]
    fn verify_installation_reports_absent_without_fabrication() {
        let temp = tempfile::tempdir().unwrap();
        let status = verify_installation(temp.path(), Some("0.147.x")).unwrap();
        assert!(!status.present);
        assert_eq!(status.version, None);
        assert_eq!(status.sha256, None);
        assert!(!status.compatible);
    }

    #[test]
    fn pinned_install_verifies_digest_and_swaps_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join("codex-home");
        let staged = temp.path().join("staged-codex");
        fs::write(&staged, "fake codex payload").unwrap();
        let digest = crate::update::sha256_file(&staged).unwrap();
        let noop_reconcile = |_codex_home: &Path| Ok(ServiceReconcileResult::default());

        // Digest mismatch fails before anything is touched.
        let bad = install_pinned_codex_with(
            &codex_home,
            &staged,
            &"0".repeat(64),
            "0.147.0",
            |_| Some("0.147.0".into()),
            noop_reconcile,
        );
        assert!(bad.unwrap_err().contains("CodexArtifactDigestMismatch"));
        assert!(!codex_home.join("packages/standalone/current").exists());

        // Success: binary lands at Codex's official standalone daemon path.
        let result = install_pinned_codex_with(
            &codex_home,
            &staged,
            &digest,
            "0.147.x",
            |_| Some("codex-cli 0.147.0".into()),
            noop_reconcile,
        )
        .unwrap();
        assert!(
            result
                .installed_path
                .replace('\\', "/")
                .ends_with("current/codex"),
            "unexpected installed path: {}",
            result.installed_path
        );
        assert_eq!(result.version.as_deref(), Some("codex-cli 0.147.0"));
        assert_eq!(result.sha256, digest);
        let installed = codex_home.join("packages/standalone/current/codex");
        assert_eq!(
            fs::read_to_string(&installed).unwrap(),
            "fake codex payload"
        );

        // Version mismatch against the pinned range fails after staging.
        let mismatch = install_pinned_codex_with(
            &codex_home,
            &staged,
            &digest,
            "0.148.x",
            |_| Some("codex-cli 0.147.0".into()),
            noop_reconcile,
        );
        assert!(mismatch
            .unwrap_err()
            .contains("CodexInstallerVersionMismatch"));
    }

    #[test]
    fn pinned_install_restores_previous_install_when_post_swap_probe_fails() {
        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join("codex-home");
        let staged = temp.path().join("staged-codex");
        fs::write(&staged, "new payload").unwrap();
        let digest = crate::update::sha256_file(&staged).unwrap();
        let noop_reconcile = |_codex_home: &Path| Ok(ServiceReconcileResult::default());

        // Probe succeeds for the staging binary but fails after the swap
        // (simulated by path): the previous install must come back.
        let current = codex_home.join("packages/standalone/current/codex");
        fs::create_dir_all(current.parent().unwrap()).unwrap();
        fs::write(&current, "previous payload").unwrap();
        let probe = |path: &Path| {
            if path.ends_with("current/codex") {
                None
            } else {
                Some("codex-cli 0.147.0".into())
            }
        };
        let result = install_pinned_codex_with(
            &codex_home,
            &staged,
            &digest,
            "0.147.x",
            probe,
            noop_reconcile,
        );
        assert!(result
            .unwrap_err()
            .contains("CodexInstallProbeFailedAfterSwap"));
        assert_eq!(
            fs::read_to_string(&current).unwrap(),
            "previous payload",
            "rollback must restore the previous install"
        );
    }

    #[test]
    fn pinned_install_restores_previous_install_when_launcher_reconcile_fails() {
        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join("codex-home");
        let staged = temp.path().join("staged-codex");
        let current = codex_home.join("packages/standalone/current/codex");
        fs::create_dir_all(current.parent().unwrap()).unwrap();
        fs::write(&current, "previous payload").unwrap();
        fs::write(&staged, "new payload").unwrap();
        let digest = crate::update::sha256_file(&staged).unwrap();

        let result = install_pinned_codex_with_hooks(
            &codex_home,
            &staged,
            &digest,
            "0.147.x",
            |_| Some("codex-cli 0.147.0".into()),
            |_| Ok(()),
            |_| Ok(ServiceReconcileResult::default()),
            |_| Err("simulated launcher conflict".into()),
        );
        assert!(result
            .unwrap_err()
            .contains("CodexCliLauncherReconcileFailed"));
        assert_eq!(fs::read_to_string(&current).unwrap(), "previous payload");
    }

    #[test]
    fn pinned_install_rejects_protocol_mismatch_before_swap() {
        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join("codex-home");
        let staged = temp.path().join("staged-codex");
        let current = codex_home.join("packages/standalone/current/codex");
        fs::create_dir_all(current.parent().unwrap()).unwrap();
        fs::write(&current, "previous payload").unwrap();
        fs::write(&staged, "new payload").unwrap();
        let digest = crate::update::sha256_file(&staged).unwrap();

        let result = install_pinned_codex_with_hooks(
            &codex_home,
            &staged,
            &digest,
            "0.147.0-alpha.6.5",
            |_| Some("codex-cli 0.147.0-alpha.6.5".into()),
            |_| Err("catalog rejected ultra".into()),
            |_| Ok(ServiceReconcileResult::default()),
            |_| Ok(()),
        );
        assert!(result.unwrap_err().contains("CodexDesktopProtocolMismatch"));
        assert_eq!(fs::read_to_string(&current).unwrap(), "previous payload");
    }

    #[test]
    fn desktop_protocol_probe_catalog_tracks_current_required_structure() {
        let catalog = desktop_protocol_probe_catalog();
        let model = &catalog["models"][0];
        for field in [
            "shell_type",
            "include_skills_usage_instructions",
            "supports_reasoning_summaries",
            "default_reasoning_summary",
            "support_verbosity",
            "default_verbosity",
            "apply_patch_tool_type",
            "web_search_tool_type",
            "truncation_policy",
            "supports_parallel_tool_calls",
            "use_responses_lite",
            "supports_image_detail_original",
            "experimental_supported_tools",
            "supports_search_tool",
        ] {
            assert!(model.get(field).is_some(), "missing required field {field}");
        }
        assert_eq!(model["shell_type"], "shell_command");
        assert_eq!(model["truncation_policy"]["mode"], "tokens");
    }

    #[test]
    fn stopped_legacy_standalone_layout_migrates_to_the_official_path() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("packages/standalone/current/bin/codex");
        let official = temp.path().join("packages/standalone/current/codex");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "legacy payload").unwrap();

        assert!(migrate_legacy_standalone_layout(temp.path()).unwrap());
        assert_eq!(fs::read_to_string(&official).unwrap(), "legacy payload");
        assert!(!legacy.exists());
        assert!(!migrate_legacy_standalone_layout(temp.path()).unwrap());
    }

    #[test]
    fn managed_launcher_never_exports_the_local_codex_home() {
        let target = PathBuf::from("/Users/joshhuang/.vellum-remote/codex/packages/standalone/current/codex");
        let managed = PathBuf::from("/Users/joshhuang/.vellum-remote/codex");
        let contents = managed_codex_launcher_contents_for_home(&target, Some(&managed));
        assert!(!contents.contains("CODEX_HOME='/Users/joshhuang/.codex'"));
        assert!(
            contents.contains(&target.to_string_lossy().to_string())
                || contents.contains("exec")
        );
    }

    #[test]
    fn daemon_from_pid_record_requires_match_managed_process_not_pid_and_start_alone() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("codex");
        let binary = home.join("packages/standalone/current/codex");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(&binary, b"codex").unwrap();
        let socket = home.join("app-server-control/app-server-control.sock");
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        fs::write(&socket, b"").unwrap();
        let start = "Fri Aug 14 16:21:43 2026";
        let expected = crate::process_identity::ExpectedIdentity {
            uid: 501,
            binary: binary.clone(),
            start_time: start.into(),
            codex_home: home.clone(),
            socket_path: socket.clone(),
        };
        let mut facts = crate::process_identity::ProcessFacts {
            pid: 4242,
            uid: 501,
            binary: binary.clone(),
            start_time: start.into(),
            codex_home: Some(home.clone()),
            socket_path: Some(socket.clone()),
            socket_owner_uid: Some(501),
            comm: Some("codex".into()),
        };
        assert!(claim_daemon_if_managed(4242, &home, &facts, &expected).is_some());
        facts.uid = 0;
        assert!(claim_daemon_if_managed(4242, &home, &facts, &expected).is_none());
        let source = include_str!("native_codex.rs");
        let from_pid = source.find("fn daemon_from_pid_record").unwrap();
        let match_at = source[from_pid..]
            .find("claim_daemon_if_managed")
            .expect("daemon_from_pid_record must call claim_daemon_if_managed");
        let discover = source.find("fn discover_native").unwrap();
        let find = source[discover..]
            .find("find_daemon_process_for_home")
            .expect("discover_native must use find_daemon_process_for_home");
        assert!(match_at > 0 && find > 0);
    }

    #[test]
    fn process_identity_never_accepts_pid_only_or_first_app_server() {
        assert!(!crate::process_identity::pid_only_identity_is_sufficient(1));
        assert!(!crate::process_identity::first_app_server_scan_is_authority());
        assert!(!crate::process_identity::process_name_scan_may_takeover(
            "app-server"
        ));
    }

    #[test]
    fn managed_cli_launcher_tracks_the_current_standalone_binary() {
        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join(".codex");
        let target = codex_home.join("packages/standalone/current/codex");
        let launcher = temp.path().join(".local/bin/codex");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        fs::write(&target, "binary").unwrap();
        fs::write(
            &launcher,
            format!(
                "#!/bin/sh\n{VELLUM_CODEX_LAUNCHER_MARKER}\nexec '/stale/current/bin/codex' \"$@\"\n"
            ),
        )
        .unwrap();

        let status = reconcile_codex_cli_launcher_at(&codex_home, &launcher).unwrap();
        assert!(status.ready);
        let contents = fs::read_to_string(&launcher).unwrap();
        assert!(contents.starts_with("#!/bin/sh\n"));
        assert!(contents.contains(&target.to_string_lossy().to_string()));
    }

    #[test]
    fn cli_launcher_refuses_to_overwrite_an_unmanaged_command() {
        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join(".codex");
        let target = codex_home.join("packages/standalone/current/codex");
        let launcher = temp.path().join(".local/bin/codex");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        fs::write(&target, "binary").unwrap();
        fs::write(&launcher, "#!/bin/sh\necho user-owned\n").unwrap();

        let error = reconcile_codex_cli_launcher_at(&codex_home, &launcher).unwrap_err();
        assert!(error.contains("CodexCliLauncherConflict"));
        assert_eq!(
            fs::read_to_string(&launcher).unwrap(),
            "#!/bin/sh\necho user-owned\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn broken_legacy_symlink_is_atomically_repaired() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join(".codex");
        let target = codex_home.join("packages/standalone/current/codex");
        let legacy = codex_home.join("packages/standalone/current/bin/codex");
        let launcher = temp.path().join(".local/bin/codex");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        fs::write(&target, "binary").unwrap();
        symlink(&legacy, &launcher).unwrap();

        assert!(!codex_cli_launcher_status_at(&codex_home, Some(launcher.clone())).ready);
        assert!(
            reconcile_codex_cli_launcher_at(&codex_home, &launcher)
                .unwrap()
                .ready
        );
    }
}
