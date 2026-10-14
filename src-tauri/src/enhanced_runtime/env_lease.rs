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

/// Where releases of Vellum before the HTTPS relay leased
/// `CODEX_API_BASE_URL`. Nothing takes that lease now; a leftover one is
/// given back by [`release_backend_base_url`].
const BACKEND_LEASE_FILE: &str = "enhanced-runtime/backend-env-lease.json";

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
        Self::read_file(&Self::path_in(data_root))
    }

    fn read_file(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

pub fn release_backend_base_url(data_root: &Path) -> Result<ReleaseOutcome, EnvironmentLeaseError> {
    release_file(&data_root.join(BACKEND_LEASE_FILE))
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
    acquire_variable(
        &EnvironmentLease::path_in(data_root),
        CODEX_CLI_PATH,
        value,
        launch_id,
        names_vellum_bridge,
    )
}

fn acquire_variable(
    lease_path: &Path,
    variable: &str,
    value: &str,
    launch_id: &str,
    is_ours: fn(&str) -> bool,
) -> Result<EnvironmentLease, EnvironmentLeaseError> {
    let current = read_user_environment(variable)?;
    let existing = EnvironmentLease::read_file(lease_path);
    if acquisition_conflicts(existing.as_ref(), current.as_deref(), value, is_ours) {
        let applied = existing
            .as_ref()
            .map(|lease| lease.applied_value.clone())
            .unwrap_or_else(|| value.to_string());
        return Err(EnvironmentLeaseError::OwnershipConflict {
            variable: variable.to_string(),
            applied,
            current,
        });
    }
    let previous_value =
        plan_previous_value_for_acquire(existing.as_ref(), current.as_deref(), value, is_ours);
    let lease = EnvironmentLease {
        variable: variable.to_string(),
        previous_value,
        applied_value: value.to_string(),
        launch_id: launch_id.to_string(),
        acquired_at: chrono::Utc::now().timestamp(),
    };
    // Persist before mutating: a crash between the two must still leave enough
    // to restore, and a stale lease is recoverable while a lost one is not.
    write_atomic(lease_path, &serde_json::to_vec_pretty(&lease)?)
        .map_err(EnvironmentLeaseError::Io)?;
    write_user_environment(variable, Some(value))?;
    let observed = read_user_environment(variable)?;
    if !environment_write_was_observed(value, observed.as_deref()) {
        // A successful command exit is not proof that the GUI launch domain
        // accepted the value. Leaving the durable lease behind would make the
        // next attempt look owned while Desktop still launches natively.
        let _ = release_file(lease_path);
        return Err(EnvironmentLeaseError::WriteNotObserved {
            variable: variable.to_string(),
            expected: value.to_string(),
            observed,
        });
    }
    broadcast_environment_change();
    Ok(lease)
}

fn environment_write_was_observed(expected: &str, observed: Option<&str>) -> bool {
    observed == Some(expected)
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
    is_ours: fn(&str) -> bool,
) -> bool {
    match existing {
        // An absent value has no external owner. This commonly happens when
        // Windows removed CODEX_CLI_PATH but a crash left our durable lease
        // behind. Re-acquiring is safe and records `None` as the new previous
        // value; only a different present value is foreign.
        Some(lease) => current.is_some_and(|current| current != lease.applied_value),
        // Replacing one of our own bridges is an upgrade, not a conflict.
        None => current.is_some_and(|current| current != requested && !is_ours(current)),
    }
}

/// Gives the lease back using the three-way rule above.
pub fn release(data_root: &Path) -> Result<ReleaseOutcome, EnvironmentLeaseError> {
    release_file(&EnvironmentLease::path_in(data_root))
}

fn release_file(lease_path: &Path) -> Result<ReleaseOutcome, EnvironmentLeaseError> {
    let Some(lease) = EnvironmentLease::read_file(lease_path) else {
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
    let _ = std::fs::remove_file(lease_path);
    Ok(outcome)
}

/// Reads the per-user (not process) value of `variable`.
pub fn read_user_environment(variable: &str) -> Result<Option<String>, EnvironmentLeaseError> {
    #[cfg(test)]
    {
        Ok(test_environment::read(variable))
    }
    #[cfg(all(not(test), target_os = "windows"))]
    {
        user_registry::read(user_registry::ENVIRONMENT, variable)
            .map_err(|error| EnvironmentLeaseError::Command(error.to_string()))
    }
    #[cfg(all(not(test), target_os = "macos"))]
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
    #[cfg(all(not(test), not(any(target_os = "windows", target_os = "macos"))))]
    {
        let _ = variable;
        Err(EnvironmentLeaseError::Unsupported)
    }
}

fn write_user_environment(
    variable: &str,
    value: Option<&str>,
) -> Result<(), EnvironmentLeaseError> {
    #[cfg(test)]
    {
        test_environment::write(variable, value);
        Ok(())
    }
    #[cfg(all(not(test), target_os = "windows"))]
    {
        user_registry::write(user_registry::ENVIRONMENT, variable, value)
            .map_err(|error| EnvironmentLeaseError::Command(error.to_string()))
    }
    #[cfg(all(not(test), target_os = "macos"))]
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
    #[cfg(all(not(test), not(any(target_os = "windows", target_os = "macos"))))]
    {
        let _ = (variable, value);
        Err(EnvironmentLeaseError::Unsupported)
    }
}

/// Tells already running processes, including a packaged app about to be
/// started by the shell, that the user environment changed.
pub fn broadcast_environment_change() {
    #[cfg(all(not(test), target_os = "windows"))]
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

/// Unit tests get a per-thread stand-in for the per-user environment.
///
/// The real one is shared with the developer's own Codex Desktop. A test that
/// disabled Enhanced in a temp data root found no lease there, read the real
/// `CODEX_CLI_PATH`, saw a Vellum bridge, and deleted it, so `cargo test`
/// quietly disarmed the Vellum the developer was running.
#[cfg(test)]
pub(crate) mod test_environment {
    use std::cell::RefCell;
    use std::collections::HashMap;

    thread_local! {
        static VALUES: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    }

    pub fn read(variable: &str) -> Option<String> {
        VALUES.with(|values| values.borrow().get(variable).cloned())
    }

    pub fn write(variable: &str, value: Option<&str>) {
        VALUES.with(|values| {
            let mut values = values.borrow_mut();
            match value {
                Some(value) => values.insert(variable.to_string(), value.to_string()),
                None => values.remove(variable),
            };
        });
    }
}

#[cfg(target_os = "windows")]
/// The per-user environment is `HKCU\Environment`, read and written in
/// process.
///
/// This used to go through PowerShell's `[Environment]` API, and a child
/// process failed in two ways. Its stdout used the console code page, so a
/// non-ASCII path such as `OneDrive/文档` came back as `????` without an
/// encoding prelude. And while Windows ends the session (shutdown, restart,
/// sign-out) new console processes fail to start, so the exit teardown could
/// not hand `CODEX_CLI_PATH` back: Codex opened after the next boot still
/// launched the bridge of a Vellum that was not running.
mod user_registry {
    use std::io;
    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, WIN32_ERROR,
    };
    use windows_sys::Win32::System::Registry::{
        RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_NOEXPAND,
        RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    };

    pub(super) const ENVIRONMENT: &str = "Environment";

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn failed(status: WIN32_ERROR) -> io::Error {
        io::Error::from_raw_os_error(status as i32)
    }

    /// The stored text, unexpanded, so writing it back restores it exactly.
    pub(super) fn read(subkey: &str, name: &str) -> io::Result<Option<String>> {
        let (subkey, name) = (wide(subkey), wide(name));
        let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
        loop {
            let mut bytes = 0u32;
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    subkey.as_ptr(),
                    name.as_ptr(),
                    flags,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut bytes,
                )
            };
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            if status != ERROR_SUCCESS {
                return Err(failed(status));
            }
            let mut buffer = vec![0u16; (bytes as usize).div_ceil(2)];
            let status = unsafe {
                RegGetValueW(
                    HKEY_CURRENT_USER,
                    subkey.as_ptr(),
                    name.as_ptr(),
                    flags,
                    std::ptr::null_mut(),
                    buffer.as_mut_ptr().cast(),
                    &mut bytes,
                )
            };
            match status {
                ERROR_SUCCESS => {
                    buffer.truncate(bytes as usize / 2);
                    while buffer.last() == Some(&0) {
                        buffer.pop();
                    }
                    let value = String::from_utf16(&buffer)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    return Ok((!value.is_empty()).then_some(value));
                }
                ERROR_FILE_NOT_FOUND => return Ok(None),
                // Another writer grew the value between the two calls.
                ERROR_MORE_DATA => continue,
                status => return Err(failed(status)),
            }
        }
    }

    pub(super) fn write(subkey: &str, name: &str, value: Option<&str>) -> io::Result<()> {
        let (subkey, name) = (wide(subkey), wide(name));
        let status = match value {
            Some(value) => {
                let data = wide(value);
                unsafe {
                    RegSetKeyValueW(
                        HKEY_CURRENT_USER,
                        subkey.as_ptr(),
                        name.as_ptr(),
                        REG_SZ,
                        data.as_ptr().cast(),
                        (data.len() * 2) as u32,
                    )
                }
            }
            None => {
                match unsafe {
                    RegDeleteKeyValueW(HKEY_CURRENT_USER, subkey.as_ptr(), name.as_ptr())
                } {
                    ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
                    status => status,
                }
            }
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(failed(status))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::System::Registry::RegDeleteKeyW;

        /// A throwaway key, never `Environment`: the real per-user
        /// environment is shared with the developer's own Codex Desktop.
        struct ScratchKey(String);

        impl Drop for ScratchKey {
            fn drop(&mut self) {
                let key = wide(&self.0);
                unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, key.as_ptr()) };
            }
        }

        #[test]
        fn values_round_trip_byte_for_byte() {
            let key = ScratchKey(format!(
                r"Software\vellum-env-lease-test-{}",
                std::process::id()
            ));
            let path = r"C:\Users\developer\OneDrive\文档\code\GLM Harness測試\%HOME%\bridge.exe";

            assert_eq!(read(&key.0, "CODEX_CLI_PATH").unwrap(), None);
            write(&key.0, "CODEX_CLI_PATH", Some(path)).unwrap();
            assert_eq!(
                read(&key.0, "CODEX_CLI_PATH").unwrap().as_deref(),
                Some(path)
            );
            write(&key.0, "CODEX_CLI_PATH", None).unwrap();
            assert_eq!(read(&key.0, "CODEX_CLI_PATH").unwrap(), None);
            // Removing what is already gone is the steady state of a release.
            write(&key.0, "CODEX_CLI_PATH", None).unwrap();
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EnvironmentLeaseError {
    #[error(
        "{variable} is owned by another value (requested={applied}, current={current:?}); refusing to overwrite it"
    )]
    OwnershipConflict {
        variable: String,
        applied: String,
        current: Option<String>,
    },
    #[error("per-user environment leases are only implemented on Windows and macOS")]
    Unsupported,
    #[error("cannot read or write the per-user environment: {0}")]
    Command(String),
    #[error(
        "{variable} write was not observed in the user launch environment (expected={expected}, observed={observed:?})"
    )]
    WriteNotObserved {
        variable: String,
        expected: String,
        observed: Option<String>,
    },
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
    is_ours: fn(&str) -> bool,
) -> Option<String> {
    match (existing, current) {
        // We still own the value: keep the original pre-lease value rather
        // than recording our own path as the thing to restore.
        (Some(lease), Some(current)) if lease.applied_value == current => {
            lease.previous_value.clone()
        }
        // A Vellum bridge left without a lease is not a third party's value
        // to restore later — it is our own leak, and it ends here.
        (None, Some(current)) if current == value || is_ours(current) => None,
        _ => current.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The bridge lease's ownership rule, which most of these tests exercise.
    fn acquisition_conflicts(
        existing: Option<&EnvironmentLease>,
        current: Option<&str>,
        requested: &str,
    ) -> bool {
        super::acquisition_conflicts(existing, current, requested, names_vellum_bridge)
    }

    fn plan_previous_value_for_acquire(
        existing: Option<&EnvironmentLease>,
        current: Option<&str>,
        value: &str,
    ) -> Option<String> {
        super::plan_previous_value_for_acquire(existing, current, value, names_vellum_bridge)
    }

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
    fn an_unobserved_user_environment_write_is_not_success() {
        assert!(environment_write_was_observed(
            "/Applications/Vellum.app/bridge",
            Some("/Applications/Vellum.app/bridge")
        ));
        assert!(!environment_write_was_observed(
            "/Applications/Vellum.app/bridge",
            None
        ));
        assert!(!environment_write_was_observed(
            "/Applications/Vellum.app/bridge",
            Some("/Applications/Other.app/codex")
        ));
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

    /// Disabling in a temp data root has no lease to read, so it falls back to
    /// the user environment. Under test that must be the stand-in: the real
    /// value belongs to whatever Vellum the developer is running.
    #[test]
    fn tests_release_the_stand_in_not_the_real_user_environment() {
        let real_before = real_user_environment(CODEX_CLI_PATH);
        let temp = tempfile::tempdir().unwrap();
        let bridge = temp.path().join("vellum-codex-app-server.exe");
        test_environment::write(CODEX_CLI_PATH, Some(&bridge.to_string_lossy()));

        let outcome = release_configured_bridge(temp.path(), &bridge).unwrap();

        assert_eq!(outcome, ReleaseOutcome::OrphanedBridgeRemoved);
        assert_eq!(read_user_environment(CODEX_CLI_PATH).unwrap(), None);
        assert_eq!(real_user_environment(CODEX_CLI_PATH), real_before);
    }

    /// Reads the per-user value straight from the OS, bypassing the stand-in.
    fn real_user_environment(variable: &str) -> Option<String> {
        #[cfg(target_os = "windows")]
        {
            user_registry::read(user_registry::ENVIRONMENT, variable)
                .ok()
                .flatten()
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = variable;
            None
        }
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
}
