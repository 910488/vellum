//! Process creation policy for Desktop-owned non-interactive helpers.
//!
//! Windows attaches console programs (`ssh.exe`, `curl.exe`, the bundled
//! `codex.exe`, PowerShell, `taskkill`, ...) to a fresh console window when a
//! GUI application spawns them without creation flags. Every helper Vellum
//! spawns is non-interactive and pipes its own streams, so that console is
//! never useful — it only flashes on screen and steals focus. This was
//! originally Remote-Manager-only (`ssh`/`curl`); it now covers every
//! non-interactive subprocess the Desktop app spawns, on both
//! `std::process::Command` and `tokio::process::Command`.
//!
//! The one deliberate exception is reopening the Codex GUI itself
//! (`commands::runtime::launch_codex`) — that command is meant to bring a
//! real window to the foreground, so it must never go through either
//! helper here.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub(crate) fn resolve_ssh_tool_at(
    program: &OsStr,
    windows: bool,
    root: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    if windows
        && matches!(
            program.to_str(),
            Some("ssh" | "scp" | "ssh-keyscan" | "ssh-keygen")
        )
    {
        if let Some(root) = root {
            let candidate = root
                .join("System32/OpenSSH")
                .join(format!("{}.exe", program.to_string_lossy()));
            if exists(&candidate) {
                return candidate;
            }
        }
    }
    program.into()
}

/// Native Win32 OpenSSH shares Windows ssh-agent and its argv conventions.
/// PATH is a compatibility fallback only; deployment preflight reports its absence.
pub(crate) fn resolve_ssh_tool(program: impl AsRef<OsStr>) -> PathBuf {
    resolve_ssh_tool_at(
        program.as_ref(),
        cfg!(windows),
        std::env::var_os("SystemRoot").as_deref().map(Path::new),
        Path::is_file,
    )
}

pub(crate) fn native_ssh_blocker() -> Option<&'static str> {
    if cfg!(windows)
        && ["ssh", "scp", "ssh-keygen"]
            .iter()
            .any(|tool| !resolve_ssh_tool(tool).is_absolute())
    {
        Some("windowsOpenSshUnavailable")
    } else {
        None
    }
}

#[cfg(windows)]
use std::os::windows::process::CommandExt as _;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Construct a non-interactive helper command without flashing a console on
/// Windows. Other platforms keep the ordinary `std::process::Command` policy.
pub fn background_command(program: impl AsRef<OsStr>) -> std::process::Command {
    let command = std::process::Command::new(resolve_ssh_tool(program));
    #[cfg(windows)]
    let command = {
        let mut command = command;
        command.creation_flags(CREATE_NO_WINDOW);
        command
    };
    command
}

/// Same policy, for callers that need `tokio::process::Command` (async
/// subprocess spawns).
pub fn background_tokio_command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    let command = tokio::process::Command::new(resolve_ssh_tool(program));
    #[cfg(windows)]
    let command = {
        let mut command = command;
        command.creation_flags(CREATE_NO_WINDOW);
        command
    };
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_preserves_the_requested_program() {
        let command = background_command("ssh");
        assert_eq!(command.get_program(), resolve_ssh_tool("ssh"));
    }

    #[test]
    fn tokio_builder_preserves_the_requested_program() {
        let command = background_tokio_command("codex");
        assert_eq!(command.as_std().get_program(), "codex");
    }

    #[test]
    fn native_openssh_precedes_path_and_only_missing_native_tools_fall_back() {
        let root = Path::new("C:/Windows");
        for name in ["ssh", "scp", "ssh-keyscan", "ssh-keygen"] {
            let native = root.join("System32/OpenSSH").join(format!("{name}.exe"));
            assert_eq!(
                resolve_ssh_tool_at(OsStr::new(name), true, Some(root), |p| p == native),
                native
            );
            assert_eq!(
                resolve_ssh_tool_at(OsStr::new(name), true, Some(root), |_| false),
                PathBuf::from(name)
            );
            assert_eq!(
                resolve_ssh_tool_at(OsStr::new(name), false, Some(root), |_| true),
                PathBuf::from(name)
            );
        }
        assert_eq!(
            resolve_ssh_tool_at(OsStr::new("git"), true, Some(root), |_| true),
            PathBuf::from("git")
        );
    }
}
