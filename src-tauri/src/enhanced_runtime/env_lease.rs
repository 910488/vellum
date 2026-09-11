//! A borrowed `CODEX_CLI_PATH`, with the borrow written down.
//!
//! Codex Desktop is a packaged app: `explorer.exe` starts it, and nothing we
//! put in our own process environment reaches it. The only handle we have is
//! the per-user environment, which is shared state we do not own. So we lease
//! it: record what was there first, then set ours, then broadcast so already
//! running shells and the packaged app pick it up.
//!
//! Release is deliberately three-way, because "put it back" has three
//! different meanings:
//!
//! 1. nothing was set before  -> delete the variable
//! 2. someone else's value was set -> restore that exact value
//! 3. someone changed it after we set ours -> leave their value alone
//!
//! Collapsing (3) into (1) or (2) is how a tool quietly breaks another tool's
//! configuration, so the lease refuses to write when it no longer owns the
//! value.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::atomic::write_atomic;

pub const CODEX_CLI_PATH: &str = "CODEX_CLI_PATH";
const LEASE_FILE: &str = "enhanced-runtime/env-lease.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentLease {
    pub variable: String,
    /// `None` means the variable did not exist before Vellum took the lease.
    pub previous_value: Option<String>,
    pub applied_value: String,
    pub launch_id: String,
    pub acquired_at: i64,
}

impl EnvironmentLease {
    pub fn path_in(data_root: &Path) -> PathBuf {
        data_root.join(LEASE_FILE)
    }

    pub fn read(data_root: &Path) -> Option<Self> {
        let bytes = std::fs::read(Self::path_in(data_root)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// The variable was removed because nothing was set before the lease.
    Removed,
    /// The pre-lease value was written back.
    Restored,
    /// Someone else owns the value now; it was left untouched.
    ForeignValueRetained,
    /// There was no lease to release.
    NoLease,
    /// A prior Vellum bridge value survived without its lease. The user
    /// explicitly disabled that exact configured bridge, so it was removed.
    OrphanedBridgeRemoved,
}

/// Takes the lease and applies `value` to the per-user environment.
pub fn acquire(
    data_root: &Path,
    value: &str,
    launch_id: &str,
) -> Result<EnvironmentLease, EnvironmentLeaseError> {
    let current = read_user_environment(CODEX_CLI_PATH)?;
    let existing = EnvironmentLease::read(data_root);
    if acquisition_conflicts(existing.as_ref(), current.as_deref(), value) {
        let applied = existing
            .as_ref()
            .map(|lease| lease.applied_value.clone())
            .unwrap_or_else(|| value.to_string());
        return Err(EnvironmentLeaseError::OwnershipConflict { applied, current });
    }
    let previous_value =
        plan_previous_value_for_acquire(existing.as_ref(), current.as_deref(), value);
    let lease = EnvironmentLease {
        variable: CODEX_CLI_PATH.to_string(),
        previous_value,
        applied_value: value.to_string(),
        launch_id: launch_id.to_string(),
        acquired_at: chrono::Utc::now().timestamp(),
    };
    // Persist before mutating: a crash between the two must still leave enough
    // to restore, and a stale lease is recoverable while a lost one is not.
    write_atomic(
        &EnvironmentLease::path_in(data_root),
        &serde_json::to_vec_pretty(&lease)?,
    )
    .map_err(EnvironmentLeaseError::Io)?;
    write_user_environment(CODEX_CLI_PATH, Some(value))?;
    broadcast_environment_change();
    Ok(lease)
}

/// Whether a `CODEX_CLI_PATH` value names a Vellum App Server bridge.
///
/// Any build of ours is ours. An older development sidecar left behind by a
/// previous Vellum carries our filename, and treating it as "another tool's
/// value" was both untrue and unrecoverable: the lease refused to overwrite
/// it, and Disable refused to clear it, so the only exit was regedit.
pub fn names_vellum_bridge(value: &str) -> bool {
    Path::new(value)
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|name| name.starts_with(super::desktop_manager::BRIDGE_EXECUTABLE_STEM))
}

/// Releases a normal lease, or clears a Vellum bridge that outlived its lease.
///
/// Disable is an explicit instruction to let go of the variable, so it covers
/// the configured bridge and any other Vellum bridge equally. A value that is
/// not ours stays foreign and untouched.
pub fn release_configured_bridge(
    data_root: &Path,
    configured_bridge: &Path,
) -> Result<ReleaseOutcome, EnvironmentLeaseError> {
    if EnvironmentLease::read(data_root).is_some() {
        return release(data_root);
    }
    let Some(current) = read_user_environment(CODEX_CLI_PATH)? else {
        return Ok(ReleaseOutcome::NoLease);
    };
    let configured = configured_bridge.to_string_lossy();
    if current != configured.as_ref() && !names_vellum_bridge(&current) {
        return Ok(ReleaseOutcome::NoLease);
    }
    write_user_environment(CODEX_CLI_PATH, None)?;
    broadcast_environment_change();
    Ok(ReleaseOutcome::OrphanedBridgeRemoved)
}

fn acquisition_conflicts(
    existing: Option<&EnvironmentLease>,
    current: Option<&str>,
    requested: &str,
) -> bool {
    match existing {
        // An absent value has no external owner. This commonly happens when
        // Windows removed CODEX_CLI_PATH but a crash left our durable lease
        // behind. Re-acquiring is safe and records `None` as the new previous
        // value; only a different present value is foreign.
        Some(lease) => current.is_some_and(|current| current != lease.applied_value),
        // Replacing one of our own bridges is an upgrade, not a conflict.
        None => {
            current.is_some_and(|current| current != requested && !names_vellum_bridge(current))
        }
    }
}

/// Gives the lease back using the three-way rule above.
pub fn release(data_root: &Path) -> Result<ReleaseOutcome, EnvironmentLeaseError> {
    let Some(lease) = EnvironmentLease::read(data_root) else {
        return Ok(ReleaseOutcome::NoLease);
    };
    let current = read_user_environment(&lease.variable)?;
    let outcome = if current.as_deref() != Some(lease.applied_value.as_str()) {
        ReleaseOutcome::ForeignValueRetained
    } else {
        match lease.previous_value.as_deref() {
            Some(previous) => {
                write_user_environment(&lease.variable, Some(previous))?;
                broadcast_environment_change();
                ReleaseOutcome::Restored
            }
            None => {
                write_user_environment(&lease.variable, None)?;
                broadcast_environment_change();
                ReleaseOutcome::Removed
            }
        }
    };
    let _ = std::fs::remove_file(EnvironmentLease::path_in(data_root));
    Ok(outcome)
}

/// Reads the per-user (not process) value of `variable`.
pub fn read_user_environment(variable: &str) -> Result<Option<String>, EnvironmentLeaseError> {
    #[cfg(target_os = "windows")]
    {
        let script = format!(
            "$value = [Environment]::GetEnvironmentVariable('{variable}','User'); if ($null -eq $value) {{ Write-Output '<unset>' }} else {{ Write-Output $value }}"
        );
        let output = powershell(&script)?;
        Ok((output != "<unset>").then_some(output))
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = crate::process::background_command("launchctl");
        command.args(["getenv", variable]);
        let output = command
            .output()
            .map_err(|error| EnvironmentLeaseError::Command(error.to_string()))?;
        if !output.status.success() {
            return Ok(None);
        }
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok((!value.is_empty()).then_some(value))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = variable;
        Err(EnvironmentLeaseError::Unsupported)
    }
}

fn write_user_environment(
    variable: &str,
    value: Option<&str>,
) -> Result<(), EnvironmentLeaseError> {
    #[cfg(target_os = "windows")]
    {
        let script = match value {
            Some(value) => format!(
                "[Environment]::SetEnvironmentVariable('{variable}',{},'User')",
                powershell_literal(value)
            ),
            None => format!("[Environment]::SetEnvironmentVariable('{variable}',$null,'User')"),
        };
        powershell(&script)?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = crate::process::background_command("launchctl");
        match value {
            Some(value) => command.args(["setenv", variable, value]),
            None => command.args(["unsetenv", variable]),
        };
        let status = command
            .status()
            .map_err(|error| EnvironmentLeaseError::Command(error.to_string()))?;
        if status.success() {
            Ok(())
        } else {
            Err(EnvironmentLeaseError::Command(format!(
                "launchctl exited with {status}"
            )))
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = (variable, value);
        Err(EnvironmentLeaseError::Unsupported)
    }
}

/// Tells already running processes, including a packaged app about to be
/// started by the shell, that the user environment changed.
pub fn broadcast_environment_change() {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Foundation::{LPARAM, WPARAM};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
        };

        let environment = "Environment\0".encode_utf16().collect::<Vec<u16>>();
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0 as WPARAM,
                environment.as_ptr() as LPARAM,
                SMTO_ABORTIFHUNG,
                5_000,
                std::ptr::null_mut(),
            );
        }
    }
}

#[cfg(target_os = "windows")]
/// Runs a PowerShell snippet and returns its stdout as text.
///
/// The encoding prelude is not decoration. With stdout redirected to a pipe,
/// PowerShell encodes it with the console output code page — cp950 on a
/// Traditional Chinese install, cp932 on a Japanese one — and decoding those
/// bytes as UTF-8 turns every non-ASCII character into U+FFFD.
///
/// That is not cosmetic here. `CODEX_CLI_PATH` holds a path, this function is
/// how Vellum reads back the path it just wrote, and the lease compares the
/// two. Installed anywhere with a non-ASCII path — under `OneDrive/文档`, say
/// — Vellum read its own value back as `????`, concluded another tool had
/// taken the variable, and reported `environmentDrift`. The handover could
/// never reach `active`, and nothing in the message said why.
fn powershell(script: &str) -> Result<String, EnvironmentLeaseError> {
    let mut command = crate::process::background_command("powershell");
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &format!(
            "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false; {script}"
        ),
    ]);
    let output = command
        .output()
        .map_err(|error| EnvironmentLeaseError::Command(error.to_string()))?;
    if !output.status.success() {
        return Err(EnvironmentLeaseError::Command(
            decode_utf8(&output.stderr).trim().to_string(),
        ));
    }
    Ok(decode_utf8(&output.stdout).trim().to_string())
}

/// UTF-8 with any byte order mark removed; some PowerShell hosts emit one once
/// the output encoding is set, and a leading U+FEFF would break every
/// comparison the lease makes.
#[cfg(target_os = "windows")]
fn decode_utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_start_matches('\u{feff}')
        .to_string()
}

/// PowerShell single-quoted string literal; the only escape is a doubled `'`.
#[cfg(target_os = "windows")]
fn powershell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[derive(Debug, thiserror::Error)]
pub enum EnvironmentLeaseError {
    #[error(
        "CODEX_CLI_PATH is owned by another value (requested={applied}, current={current:?}); refusing to overwrite it"
    )]
    OwnershipConflict {
        applied: String,
        current: Option<String>,
    },
    #[error("per-user environment leases are only implemented on Windows and macOS")]
    Unsupported,
    #[error("cannot read or write the per-user environment: {0}")]
    Command(String),
    #[error(transparent)]
    Io(std::io::Error),
    #[error("environment lease JSON is invalid: {0}")]
    Json(String),
}

impl From<serde_json::Error> for EnvironmentLeaseError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

/// The restore rule on its own, so the three-way semantics can be tested
/// without touching the machine's real per-user environment.
pub fn plan_release(lease: &EnvironmentLease, current: Option<&str>) -> ReleaseOutcome {
    if current != Some(lease.applied_value.as_str()) {
        return ReleaseOutcome::ForeignValueRetained;
    }
    match lease.previous_value {
        Some(_) => ReleaseOutcome::Restored,
        None => ReleaseOutcome::Removed,
    }
}

/// The acquire rule on its own, for the same reason.
pub fn plan_previous_value(
    existing: Option<&EnvironmentLease>,
    current: Option<&str>,
) -> Option<String> {
    match (existing, current) {
        (Some(lease), Some(current)) if lease.applied_value == current => {
            lease.previous_value.clone()
        }
        _ => current.map(str::to_string),
    }
}

fn plan_previous_value_for_acquire(
    existing: Option<&EnvironmentLease>,
    current: Option<&str>,
    value: &str,
) -> Option<String> {
    match (existing, current) {
        // We still own the value: keep the original pre-lease value rather
        // than recording our own path as the thing to restore.
        (Some(lease), Some(current)) if lease.applied_value == current => {
            lease.previous_value.clone()
        }
        // A Vellum bridge left without a lease is not a third party's value
        // to restore later — it is our own leak, and it ends here.
        (None, Some(current)) if current == value || names_vellum_bridge(current) => None,
        _ => current.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    /// The lease compares the path it wrote against the path it reads back, so
    /// the reader has to be byte-faithful. On a console code page that cannot
    /// represent the path — cp950 here — the old reader returned `????` and the
    /// runtime called its own value foreign.
    #[cfg(target_os = "windows")]
    #[test]
    fn powershell_output_survives_a_non_ascii_path() {
        let path = r"C:\Users\developer\OneDrive\文档\code\GLM Harness測試\vellum";
        let echoed =
            super::powershell(&format!("Write-Output '{path}'")).expect("powershell should run");
        assert_eq!(echoed, path);
        assert!(
            !echoed.contains('\u{fffd}'),
            "the reader must not lose characters it cannot encode"
        );
    }

    use super::*;

    fn lease(previous: Option<&str>, applied: &str) -> EnvironmentLease {
        EnvironmentLease {
            variable: CODEX_CLI_PATH.into(),
            previous_value: previous.map(str::to_string),
            applied_value: applied.into(),
            launch_id: "launch-1".into(),
            acquired_at: 1,
        }
    }

    #[test]
    fn restore_is_three_way() {
        let unset_before = lease(None, "C:/vellum/bridge.exe");
        assert_eq!(
            plan_release(&unset_before, Some("C:/vellum/bridge.exe")),
            ReleaseOutcome::Removed
        );

        let foreign_before = lease(Some("C:/other/codex.exe"), "C:/vellum/bridge.exe");
        assert_eq!(
            plan_release(&foreign_before, Some("C:/vellum/bridge.exe")),
            ReleaseOutcome::Restored
        );

        assert_eq!(
            plan_release(&foreign_before, Some("C:/someone-else/codex.exe")),
            ReleaseOutcome::ForeignValueRetained
        );
        assert_eq!(
            plan_release(&foreign_before, None),
            ReleaseOutcome::ForeignValueRetained
        );
    }

    #[test]
    fn reacquiring_our_own_value_keeps_the_original_pre_lease_value() {
        let existing = lease(Some("C:/other/codex.exe"), "C:/vellum/bridge.exe");
        assert_eq!(
            plan_previous_value(Some(&existing), Some("C:/vellum/bridge.exe")),
            Some("C:/other/codex.exe".to_string())
        );
        assert_eq!(plan_previous_value(None, None), None);
    }
    #[test]
    fn acquiring_an_orphaned_exact_bridge_does_not_make_a_self_referential_lease() {
        let value = "C:/vellum/bridge.exe";
        assert_eq!(
            plan_previous_value_for_acquire(None, Some(value), value),
            None
        );
        assert_eq!(
            plan_previous_value_for_acquire(None, Some("C:/other/codex.exe"), value),
            Some("C:/other/codex.exe".to_string())
        );
    }

    #[test]
    fn reacquire_refuses_a_foreign_replacement() {
        let existing = lease(Some("C:/original/codex.exe"), "C:/vellum/bridge.exe");
        assert!(!acquisition_conflicts(
            Some(&existing),
            Some("C:/vellum/bridge.exe"),
            "C:/vellum/bridge.exe"
        ));
        assert!(acquisition_conflicts(
            Some(&existing),
            Some("C:/third-party/codex.exe"),
            "C:/vellum/bridge.exe"
        ));
        assert!(!acquisition_conflicts(
            Some(&existing),
            None,
            "C:/vellum/bridge.exe"
        ));
        assert_eq!(
            plan_previous_value_for_acquire(Some(&existing), None, "C:/vellum/bridge.exe"),
            None,
            "an absent current value must not resurrect an obsolete pre-lease owner"
        );
        assert!(acquisition_conflicts(
            None,
            Some("C:/third-party/codex.exe"),
            "C:/vellum/bridge.exe"
        ));
        assert!(!acquisition_conflicts(
            None,
            Some("C:/vellum/bridge.exe"),
            "C:/vellum/bridge.exe"
        ));
        assert!(!acquisition_conflicts(None, None, "C:/vellum/bridge.exe"));
    }

    /// The state a development machine actually got stuck in: an older build's
    /// sidecar sitting in `CODEX_CLI_PATH` with no lease beside it. Acquire
    /// refused to overwrite it and Disable refused to clear it, so the screen
    /// said "another tool manages this" about a file Vellum had written.
    #[test]
    fn a_vellum_bridge_left_without_a_lease_is_still_ours() {
        const STALE: &str = "C:/vellum/target/debug/vellum-codex-app-server.exe";
        const CURRENT: &str = "C:/vellum/binaries/dev/vellum-codex-app-server-d9ac75f7.exe";
        assert!(names_vellum_bridge(STALE));
        assert!(names_vellum_bridge(CURRENT));
        assert!(!names_vellum_bridge("C:/other-tool/codex.exe"));

        // Replacing one of ours is an upgrade, and the value it replaces is not
        // something to restore later.
        assert!(!acquisition_conflicts(None, Some(STALE), CURRENT));
        assert_eq!(
            plan_previous_value_for_acquire(None, Some(STALE), CURRENT),
            None
        );

        // Someone else's value is still off limits.
        assert!(acquisition_conflicts(
            None,
            Some("C:/other-tool/codex.exe"),
            CURRENT
        ));
        assert_eq!(
            plan_previous_value_for_acquire(None, Some("C:/other-tool/codex.exe"), CURRENT),
            Some("C:/other-tool/codex.exe".to_string())
        );
    }

    #[test]
    fn lease_file_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        let value = lease(Some("C:/other/codex.exe"), "C:/vellum/bridge.exe");
        write_atomic(
            &EnvironmentLease::path_in(temp.path()),
            &serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
        assert_eq!(EnvironmentLease::read(temp.path()), Some(value));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn powershell_literals_escape_quotes() {
        assert_eq!(powershell_literal("C:/a b/c.exe"), "'C:/a b/c.exe'");
        assert_eq!(powershell_literal("it's"), "'it''s'");
    }
}
