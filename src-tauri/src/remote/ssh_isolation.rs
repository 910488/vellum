//! Dedicated SSH alias and key for Remote Manager.
//!
//! SSH config and `authorized_keys` only add or remove marked managed entries.
//! Unmarked Host blocks and user keys stay untouched. The launcher env never
//! rewrites `~/.codex`.

use std::path::Path;

pub const WRAPPER_MARKER: &str = "# Managed by Vellum Remote Manager";
pub const AUTH_KEYS_MARKER_PREFIX: &str = "vellum-remote-managed:";

pub const SSH_BEGIN_PREFIX: &str = "# vellum-remote-managed:begin ";
pub const SSH_END_PREFIX: &str = "# vellum-remote-managed:end ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSshConfig {
    pub host_id: String,
    pub alias: String,
    pub hostname: String,
    pub user: String,
    pub port: u16,
    pub identity_file: String,
}

impl ManagedSshConfig {
    pub fn block(&self) -> String {
        format!(
            "{SSH_BEGIN_PREFIX}{id}\nHost {alias}\n    HostName {host}\n    User {user}\n    Port {port}\n    IdentityFile {identity}\n    IdentitiesOnly yes\n{SSH_END_PREFIX}{id}\n",
            id = self.host_id,
            alias = self.alias,
            host = self.hostname,
            user = self.user,
            port = self.port,
            identity = self.identity_file,
        )
    }
}

pub fn upsert_ssh_config(existing: &str, config: &ManagedSshConfig) -> String {
    let stripped = remove_ssh_config(existing, &config.host_id);
    let mut out = stripped;
    if !out.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&config.block());
    out
}

pub fn remove_ssh_config(existing: &str, host_id: &str) -> String {
    let begin = format!("{SSH_BEGIN_PREFIX}{host_id}");
    let end = format!("{SSH_END_PREFIX}{host_id}");
    let mut skipping = false;
    let mut lines = Vec::new();
    for line in existing.lines() {
        if line.trim() == begin {
            skipping = true;
            continue;
        }
        if line.trim() == end {
            skipping = false;
            continue;
        }
        if !skipping {
            lines.push(line);
        }
    }
    let mut out = lines.join("\n");
    if existing.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

pub fn managed_alias(host_id: &str) -> String {
    let safe: String = host_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    format!("vellum-remote-{safe}")
}

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
    let quoted_wrapper = format!(
        "\"{}\"",
        wrapper_path.to_string_lossy().replace('"', "\\\"")
    );
    let command = format!(
        "command={quoted_wrapper},no-agent-forwarding,no-X11-forwarding {}",
        public_key.trim()
    );
    format!("# {AUTH_KEYS_MARKER_PREFIX}{host_id}\n{command} {AUTH_KEYS_MARKER_PREFIX}{host_id}\n")
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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

pub fn upsert_remote_authorized_keys(existing: &str, host_id: &str, block: &str) -> String {
    upsert_authorized_keys(existing, host_id, block)
}

pub fn remove_remote_authorized_keys(existing: &str, host_id: &str) -> String {
    remove_authorized_keys(existing, host_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn ssh_config_add_and_remove_only_touch_marked_blocks() {
        let existing = "Host gpu-dev\n    HostName 192.0.2.10\n    User operator\n";
        let config = ManagedSshConfig {
            host_id: "mac-mini".into(),
            alias: managed_alias("mac-mini"),
            hostname: "100.78.101.55".into(),
            user: "joshhuang".into(),
            port: 22,
            identity_file: "~/.vellum/remote-ssh/mac-mini/id_ed25519".into(),
        };
        let added = upsert_ssh_config(existing, &config);
        assert!(added.contains("Host gpu-dev"));
        assert!(added.contains("Host vellum-remote-mac-mini"));
        assert!(added.contains("IdentitiesOnly yes"));
        assert!(added.contains(&format!("{SSH_BEGIN_PREFIX}mac-mini")));
        let removed = remove_ssh_config(&added, "mac-mini");
        assert_eq!(removed, existing);
        assert!(!removed.contains("vellum-remote-mac-mini"));
        assert!(removed.contains("Host gpu-dev"));
    }

    #[test]
    fn launcher_env_does_not_rewrite_local_codex_home() {
        let home = Path::new("/Users/joshhuang/.vellum-remote/codex");
        let contents = managed_ssh_wrapper_contents(
            home,
            &home.join("packages/standalone/current"),
            Path::new("/Users/joshhuang/.local/bin"),
        );
        assert!(!wrapper_rewrites_local_codex_home(
            &contents,
            "/Users/joshhuang/.codex"
        ));
        assert!(contents.contains("CODEX_HOME='/Users/joshhuang/.vellum-remote/codex'"));
        assert!(!contents.contains("CODEX_HOME='/Users/joshhuang/.codex'"));
        let keys = "ssh-ed25519 AAAAUSER mine\n";
        let block = marked_authorized_keys_line(
            "mac-mini",
            Path::new("/Users/joshhuang/.vellum-remote/ssh-wrapper"),
            "ssh-ed25519 AAAAMANAGED vellum",
        );
        let added = upsert_remote_authorized_keys(keys, "mac-mini", &block);
        let removed = remove_remote_authorized_keys(&added, "mac-mini");
        assert_eq!(removed, keys);
        assert!(AUTH_KEYS_MARKER_PREFIX.starts_with("vellum-remote-managed"));
    }
}
