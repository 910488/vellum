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

#[cfg(windows)]
use std::os::windows::process::CommandExt as _;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Construct a non-interactive helper command without flashing a console on
/// Windows. Other platforms keep the ordinary `std::process::Command` policy.
pub fn background_command(program: impl AsRef<OsStr>) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

/// Same policy, for callers that need `tokio::process::Command` (async
/// subprocess spawns).
pub fn background_tokio_command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_preserves_the_requested_program() {
        let command = background_command("ssh");
        assert_eq!(command.get_program(), "ssh");
    }

    #[test]
    fn tokio_builder_preserves_the_requested_program() {
        let command = background_tokio_command("codex");
        assert_eq!(command.as_std().get_program(), "codex");
    }
}
