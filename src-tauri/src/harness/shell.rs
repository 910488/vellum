//! Terminal capability handshake and the argv-safe execution contract.
//!
//! M3 (refactor(proxy-runtime): move harness contract to runtime): the pure
//! derivation and execution contract live in
//! `vellum-proxy-runtime::harness::shell`, shared with the headless daemon.
//! This module re-exports that type set and keeps the *host* detection — the
//! functions that read this process's environment (`PATH`, `MSYSTEM`,
//! `SHELL`, `VELLUM_SHELL`) — here, where the proxy host is owned. The shared
//! runtime never probes the live host; a Windows proxy is a Desktop fact, not
//! a runtime fact, and the model's shell is the executor's shell, not this
//! process's.

use std::path::PathBuf;
use std::sync::OnceLock;

pub use vellum_proxy_runtime::harness::shell::*;

static DETECTED: OnceLock<TerminalCapabilities> = OnceLock::new();

/// Session-scoped handshake. Detected once, then reused for the lifetime of
/// the process so the prompt, the tool descriptions, and any post-compaction
/// continuation all describe the same shell.
pub fn detected() -> &'static TerminalCapabilities {
    DETECTED.get_or_init(|| TerminalCapabilities::from_probe(&real_probe()))
}

fn real_probe() -> ShellProbe {
    ShellProbe {
        platform: Some(vellum_proxy_runtime::harness::shell::default_platform()),
        pwsh: which("pwsh"),
        pwsh_version: None,
        powershell: which("powershell"),
        powershell_version: None,
        bash: which("bash"),
        zsh: which("zsh"),
        msystem: std::env::var("MSYSTEM").ok(),
        shell_env: std::env::var("SHELL").ok(),
        forced_shell: std::env::var("VELLUM_SHELL")
            .ok()
            .as_deref()
            .and_then(vellum_proxy_runtime::harness::shell::parse_shell_kind),
    }
}

/// Minimal PATH lookup: avoids depending on an external `which` crate and
/// keeps detection free of subprocess spawning.
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(target_os = "windows") {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|extension| extension.to_ascii_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };
    for directory in std::env::split_paths(&path) {
        for extension in &extensions {
            let candidate = directory.join(format!("{program}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
