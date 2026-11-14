//! Keeps Codex Desktop's projects in its sidebar from the moment it starts.
//!
//! Codex Desktop 26.1103 can keep each signed-in account's projects apart. The
//! record of that is `electron-account-renderer-state-v1` in
//! `.codex-global-state.json`. Once that key exists, every launch begins by
//! putting the projects away, and they come back only when Desktop learns from
//! the App Server who is signed in. The sidebar does not wait for that:
//!
//! - Desktop opens its window before it even starts the App Server.
//! - The window reads the project list once, as it loads.
//! - Putting the projects back does not tell the window.
//!
//! So the sidebar shows no projects whenever the sign-in is learned after that
//! read, and on 2026-11-09 it was. How fast the App Server answers decides the
//! race; nothing on the App Server side can win it every time.
//!
//! Desktop writes the key only when it rebuilds its windows for an account
//! event, and a bridge restart under a running Desktop is one. A Desktop that
//! has only ever had one account gains nothing from the key: there is no other
//! account's state to keep apart, and without it Desktop reads its projects
//! directly at launch, with no race at all. So while Desktop is closed, the key
//! is removed when it scopes a single account and holds nothing for any other.
//!
//! Nothing else is touched:
//!
//! - A key that holds another account's state stays. So does one written by a
//!   Desktop that stopped before it learned the account, whose projects are
//!   still put away inside the key.
//! - The file is rewritten only if serializing it again reproduces it byte for
//!   byte, so the removed key is the only difference.
//! - If Desktop writes the file while it is being read, Desktop's write wins.
//!
//! Desktop puts the key back the next time it rebuilds its windows for an
//! account event. It is removed again the next time Desktop closes.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

const GLOBAL_STATE_FILE: &str = ".codex-global-state.json";
/// Desktop reads this copy when the file itself does not parse.
const GLOBAL_STATE_BACKUP: &str = ".codex-global-state.json.bak";
const ACCOUNT_SCOPE_KEY: &str = "electron-account-renderer-state-v1";

static REWRITE: Mutex<()> = Mutex::new(());

/// What was found in one copy of Desktop's global state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountScope {
    /// It scoped one account and held nothing for any other, and is gone.
    Cleared,
    /// There is none; Desktop starts with its projects in place.
    Absent,
    /// It holds state for another account, or for an account Desktop never
    /// got to learn.
    InUse,
    /// The file is missing, does not parse, or would not survive a rewrite
    /// unchanged.
    Unrecognized,
    /// Desktop changed the file while it was being read.
    Raced,
    /// The rewrite could not be saved.
    Unwritable,
}

impl AccountScope {
    fn label(self) -> &'static str {
        match self {
            Self::Cleared => "cleared",
            Self::Absent => "absent",
            Self::InUse => "inUse",
            Self::Unrecognized => "unrecognized",
            Self::Raced => "raced",
            Self::Unwritable => "unwritable",
        }
    }
}

/// Removes a redundant account scope from Desktop's global state and its
/// backup. Call only while Codex Desktop is closed: a running Desktop keeps
/// the state in memory and writes it back.
pub(crate) fn clear_redundant_account_scope(codex_home: &Path) -> AccountScope {
    let Ok(_guard) = REWRITE.lock() else {
        return AccountScope::Unwritable;
    };
    let outcome = clear_in(&codex_home.join(GLOBAL_STATE_FILE));
    let backup = clear_in(&codex_home.join(GLOBAL_STATE_BACKUP));
    if backup == AccountScope::Cleared || outcome != AccountScope::Absent {
        log::info!(
            "[DesktopState] account scope state={} backup={}",
            outcome.label(),
            backup.label()
        );
    }
    outcome
}

fn clear_in(path: &Path) -> AccountScope {
    let Ok(bytes) = std::fs::read(path) else {
        return AccountScope::Unrecognized;
    };
    let rewritten = match without_redundant_scope(&bytes) {
        Ok(rewritten) => rewritten,
        Err(outcome) => return outcome,
    };
    if std::fs::read(path).ok().as_deref() != Some(&bytes[..]) {
        return AccountScope::Raced;
    }
    match crate::enhanced_runtime::atomic::write_atomic(path, &rewritten) {
        Ok(()) => AccountScope::Cleared,
        Err(error) => {
            log::warn!("[DesktopState] could not rewrite global state: {error}");
            AccountScope::Unwritable
        }
    }
}

/// `bytes` without the account scope, when the scope is redundant.
fn without_redundant_scope(bytes: &[u8]) -> Result<Vec<u8>, AccountScope> {
    let Ok(Value::Object(mut state)) = serde_json::from_slice::<Value>(bytes) else {
        return Err(AccountScope::Unrecognized);
    };
    let Some(scope) = state.get(ACCOUNT_SCOPE_KEY) else {
        return Err(AccountScope::Absent);
    };
    match scope_is_redundant(scope) {
        Some(true) => {}
        Some(false) => return Err(AccountScope::InUse),
        None => return Err(AccountScope::Unrecognized),
    }
    // Desktop writes compact JSON. Unless the parsed file serializes back to
    // the same bytes, removing one key would also change something else.
    let reproduced = serde_json::to_vec(&state).map_err(|_| AccountScope::Unrecognized)?;
    if reproduced != bytes {
        return Err(AccountScope::Unrecognized);
    }
    state.shift_remove(ACCOUNT_SCOPE_KEY);
    serde_json::to_vec(&state).map_err(|_| AccountScope::Unrecognized)
}

/// Whether the scope keeps anything apart. `None` for a shape this was not
/// written against.
fn scope_is_redundant(scope: &Value) -> Option<bool> {
    let scope = scope.as_object()?;
    let inactive = scope.get("inactive")?.as_object()?;
    if scope.len() != 2 {
        return None;
    }
    match scope.get("owner")? {
        Value::String(owner) if !owner.is_empty() => Some(inactive.is_empty()),
        // Desktop stopped before it learned who was signed in; the projects
        // are put away under `inactive` and only Desktop can put them back.
        Value::Null => Some(false),
        _ => None,
    }
}

/// The home Codex Desktop keeps its global state in.
fn desktop_codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .unwrap_or_else(|| std::env::temp_dir().join(".codex"))
}

/// Clears a redundant account scope whenever Codex Desktop is closed: once
/// when Vellum starts without Desktop running, and again each time Desktop
/// exits, before an update or anything else can start it again.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub(crate) fn spawn_exit_watch() {
    const POLL_WHILE_CLOSED: std::time::Duration = std::time::Duration::from_secs(3);
    let spawned = std::thread::Builder::new()
        .name("desktop-state".into())
        .spawn(|| {
            let home = desktop_codex_home();
            if crate::commands::runtime::codex_desktop_pid().is_none() {
                clear_redundant_account_scope(&home);
            }
            loop {
                match crate::commands::runtime::codex_desktop_pid() {
                    Some(pid) => {
                        wait_for_exit(pid);
                        // Electron helpers can outlive the main process for a
                        // moment, but only the main process writes this file.
                        clear_redundant_account_scope(&home);
                    }
                    None => std::thread::sleep(POLL_WHILE_CLOSED),
                }
            }
        });
    if let Err(error) = spawned {
        log::warn!("[DesktopState] could not watch Codex Desktop: {error}");
    }
}

/// Clears a redundant account scope during a managed restart, after Desktop
/// has stopped and before it is launched again.
pub(crate) fn clear_before_launch() -> AccountScope {
    clear_redundant_account_scope(&desktop_codex_home())
}

#[cfg(target_os = "windows")]
fn wait_for_exit(pid: u32) {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE,
    };

    // SAFETY: the handle is checked before use and closed exactly once.
    let exited = unsafe {
        let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if process.is_null() {
            false
        } else {
            let result = WaitForSingleObject(process, INFINITE);
            CloseHandle(process);
            result == WAIT_OBJECT_0
        }
    };
    if !exited {
        // Without a handle, ask the way Desktop was found in the first place.
        while crate::commands::runtime::codex_desktop_pid() == Some(pid) {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}

#[cfg(target_os = "macos")]
fn wait_for_exit(pid: u32) {
    // `kill(pid, 0)` sends nothing; it only asks whether the pid exists.
    // Unlike `process_info::pid_is_alive` it spawns no process per check,
    // which matters for a loop that runs as long as Desktop does.
    let alive = || {
        // SAFETY: signal 0 performs only the existence and permission check.
        unsafe {
            libc::kill(pid as libc::pid_t, 0) == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
    };
    while alive() {
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "458c93c6135d0000000000000000000000000000000000000000000000000000";

    fn state(scope: &str) -> String {
        format!(
            r#"{{"local-projects":{{"p1":{{"id":"p1","rootPaths":["E:\\code"],"createdAt":1730000000000}}}},"{ACCOUNT_SCOPE_KEY}":{scope},"project-order":["p1"],"ratio":0.25}}"#
        )
    }

    #[test]
    fn a_scope_for_one_account_is_removed_and_nothing_else_changes() {
        let before = state(&format!(r#"{{"owner":"{OWNER}","inactive":{{}}}}"#));
        let after = without_redundant_scope(before.as_bytes()).unwrap();
        assert_eq!(
            String::from_utf8(after).unwrap(),
            r#"{"local-projects":{"p1":{"id":"p1","rootPaths":["E:\\code"],"createdAt":1730000000000}},"project-order":["p1"],"ratio":0.25}"#
        );
    }

    #[test]
    fn a_scope_that_keeps_another_account_apart_stays() {
        let before = state(&format!(
            r#"{{"owner":"{OWNER}","inactive":{{"other":{{"atoms":{{}},"globals":{{"local-projects":{{}}}}}}}}}}"#
        ));
        assert_eq!(
            without_redundant_scope(before.as_bytes()),
            Err(AccountScope::InUse)
        );
    }

    #[test]
    fn a_scope_written_before_the_account_was_known_stays() {
        let before = state(r#"{"owner":null,"inactive":{}}"#);
        assert_eq!(
            without_redundant_scope(before.as_bytes()),
            Err(AccountScope::InUse)
        );
    }

    #[test]
    fn unfamiliar_shapes_are_left_alone() {
        for scope in [
            format!(r#"{{"owner":"{OWNER}","inactive":{{}},"version":2}}"#),
            format!(r#"{{"owner":"{OWNER}"}}"#),
            r#"{"owner":"","inactive":{}}"#.to_string(),
            r#"{"owner":7,"inactive":{}}"#.to_string(),
            r#"[]"#.to_string(),
        ] {
            assert_eq!(
                without_redundant_scope(state(&scope).as_bytes()),
                Err(AccountScope::Unrecognized),
                "{scope}"
            );
        }
        assert_eq!(
            without_redundant_scope(b"not json"),
            Err(AccountScope::Unrecognized)
        );
        assert_eq!(
            without_redundant_scope(br#"{"local-projects":{}}"#),
            Err(AccountScope::Absent)
        );
    }

    #[test]
    fn a_file_that_would_not_round_trip_is_left_alone() {
        // Spacing Desktop never writes, and a number spelled differently.
        for before in [
            format!(
                r#"{{"{ACCOUNT_SCOPE_KEY}": {{"owner":"{OWNER}","inactive":{{}}}}}}"#
            ),
            format!(
                r#"{{"{ACCOUNT_SCOPE_KEY}":{{"owner":"{OWNER}","inactive":{{}}}},"n":1E3}}"#
            ),
        ] {
            assert_eq!(
                without_redundant_scope(before.as_bytes()),
                Err(AccountScope::Unrecognized),
                "{before}"
            );
        }
    }

    #[test]
    fn the_file_and_its_backup_are_both_cleared() {
        let home = tempfile::tempdir().unwrap();
        let before = state(&format!(r#"{{"owner":"{OWNER}","inactive":{{}}}}"#));
        std::fs::write(home.path().join(GLOBAL_STATE_FILE), &before).unwrap();
        std::fs::write(home.path().join(GLOBAL_STATE_BACKUP), &before).unwrap();

        assert_eq!(
            clear_redundant_account_scope(home.path()),
            AccountScope::Cleared
        );
        for name in [GLOBAL_STATE_FILE, GLOBAL_STATE_BACKUP] {
            let text = std::fs::read_to_string(home.path().join(name)).unwrap();
            assert!(!text.contains(ACCOUNT_SCOPE_KEY), "{name}");
            assert!(text.contains(r#""project-order":["p1"]"#), "{name}");
        }
        assert_eq!(
            clear_redundant_account_scope(home.path()),
            AccountScope::Absent
        );
    }

    #[test]
    fn a_missing_file_is_not_created() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            clear_redundant_account_scope(home.path()),
            AccountScope::Unrecognized
        );
        assert!(!home.path().join(GLOBAL_STATE_FILE).exists());
    }
}
