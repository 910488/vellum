//! Managed SSH login wrapper. Applied only to the dedicated Remote key.
//!
//! The wrapper sets `CODEX_HOME` / `CODEX_INSTALL_DIR` for that key's sessions
//! and leaves scp/sftp/rsync usable. It never rewrites `~/.codex`.

use std::path::Path;

pub const WRAPPER_MARKER: &str = "# Managed by Vellum Remote Manager";
pub const AUTH_KEYS_MARKER_PREFIX: &str = "vellum-remote-managed:";

pub fn managed_ssh_wrapper_contents(
    codex_home: &Path,
    install_dir: &Path,
    extra_path: &Path,
) -> String {
    format!(
        "#!/bin/sh\n{WRAPPER_MARKER}\n\
export CODEX_HOME={home}\n\
export CODEX_INSTALL_DIR={install}\n\
export PATH={install}:{extra}:\"$PATH\"\n\
case \"${{SSH_ORIGINAL_COMMAND-}}\" in\n\
  \"\") exec \"${{SHELL:-/bin/bash}}\" -l ;;\n\
  scp\\ *|scp|sftp*|rsync\\ *|/usr/libexec/sftp-server*) exec \"${{SHELL:-/bin/bash}}\" -c \"$SSH_ORIGINAL_COMMAND\" ;;\n\
  *) exec \"${{SHELL:-/bin/bash}}\" -lc \"$SSH_ORIGINAL_COMMAND\" ;;\nesac\n",
        home = shell_single_quote(&codex_home.to_string_lossy()),
        install = shell_single_quote(&install_dir.to_string_lossy()),
        extra = shell_single_quote(&extra_path.to_string_lossy()),
    )
}

pub fn wrapper_rewrites_local_codex_home(contents: &str, local_codex_home: &str) -> bool {
    let export = format!("export CODEX_HOME={}", shell_single_quote(local_codex_home));
    contents.contains(&export)
}

pub fn is_managed_wrapper(contents: &str) -> bool {
    contents.starts_with(&format!("#!/bin/sh\n{WRAPPER_MARKER}\n"))
}

pub fn marked_authorized_keys_line(
    host_id: &str,
    wrapper_path: &Path,
    public_key: &str,
) -> String {
    let command = format!(
        "command={},no-agent-forwarding,no-X11-forwarding {}",
        shell_double_quote(&wrapper_path.to_string_lossy()),
        public_key.trim()
    );
    format!("# {AUTH_KEYS_MARKER_PREFIX}{host_id}\n{command} {AUTH_KEYS_MARKER_PREFIX}{host_id}\n")
}

pub fn upsert_authorized_keys(existing: &str, host_id: &str, line_block: &str) -> String {
    let stripped = remove_authorized_keys(existing, host_id);
    let mut out = stripped;
    if !out.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out.push_str(line_block);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub fn remove_authorized_keys(existing: &str, host_id: &str) -> String {
    let marker = format!("{AUTH_KEYS_MARKER_PREFIX}{host_id}");
    let mut skip_next = false;
    let mut lines = Vec::new();
    for line in existing.lines() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if line.trim() == format!("# {marker}") {
            skip_next = true;
            continue;
        }
        if line.contains(&marker) {
            continue;
        }
        lines.push(line);
    }
    let mut out = lines.join("\n");
    if existing.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn shell_double_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn wrapper_sets_managed_home_and_does_not_rewrite_local_codex() {
        let home = PathBuf::from("/Users/joshhuang/.vellum-remote/codex");
        let install = home.join("packages/standalone/current");
        let extra = PathBuf::from("/Users/joshhuang/.local/bin");
        let contents = managed_ssh_wrapper_contents(&home, &install, &extra);
        assert!(is_managed_wrapper(&contents));
        assert!(contents.contains("export CODEX_HOME='/Users/joshhuang/.vellum-remote/codex'"));
        assert!(contents.contains("export CODEX_INSTALL_DIR="));
        assert!(!wrapper_rewrites_local_codex_home(
            &contents,
            "/Users/joshhuang/.codex"
        ));
        assert!(contents.contains("scp"));
        assert!(contents.contains("sftp"));
        assert!(contents.contains("rsync"));
    }

    #[test]
    fn authorized_keys_add_and_remove_only_touch_marked_entries() {
        let existing = "ssh-ed25519 AAAAUSER user-key\n";
        let host_id = "mac-mini";
        let block = marked_authorized_keys_line(
            host_id,
            Path::new("/Users/joshhuang/.vellum-remote/ssh-wrapper"),
            "ssh-ed25519 AAAAMANAGED vellum",
        );
        let added = upsert_authorized_keys(existing, host_id, &block);
        assert!(added.contains("ssh-ed25519 AAAAUSER user-key"));
        assert!(added.contains("vellum-remote-managed:mac-mini"));
        assert!(added.contains("command="));
        let removed = remove_authorized_keys(&added, host_id);
        assert_eq!(removed, existing);
        let other = upsert_authorized_keys(&added, "other", "# vellum-remote-managed:other\nssh-ed25519 AAAOTHER other vellum-remote-managed:other\n");
        let removed_one = remove_authorized_keys(&other, host_id);
        assert!(removed_one.contains("AAAOTHER"));
        assert!(removed_one.contains("AAAAUSER"));
        assert!(!removed_one.contains("AAAAMANAGED"));
    }
}
