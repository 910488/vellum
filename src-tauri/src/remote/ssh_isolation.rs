//! Dedicated SSH alias and key for Remote Manager.
//!
//! SSH config and `authorized_keys` only add or remove marked managed entries.
//! Unmarked Host blocks and user keys stay untouched. The launcher env never
//! rewrites `~/.codex`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::remote::process::background_command;

pub const WRAPPER_MARKER: &str = "# Managed by Vellum Remote Manager";
pub const AUTH_KEYS_MARKER_PREFIX: &str = "vellum-remote-managed:";

pub const SSH_BEGIN_PREFIX: &str = "# vellum-remote-managed:begin ";
pub const SSH_END_PREFIX: &str = "# vellum-remote-managed:end ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedIsolation {
    pub alias: String,
    pub ssh_config_path: PathBuf,
    pub identity_file: PathBuf,
    pub wrapper_path: String,
    pub wrapper_contents: String,
    pub authorized_keys: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolationApplyRequest<'a> {
    pub ssh_config_path: &'a Path,
    pub identity_file: &'a Path,
    pub host_id: &'a str,
    pub hostname: &'a str,
    pub user: &'a str,
    pub port: u16,
    pub public_key: &'a str,
    /// Paths interpreted by the remote Unix shell. Keep these as strings so a
    /// Windows Desktop cannot silently rewrite their separators.
    pub managed_codex_home: &'a str,
    pub install_dir: &'a str,
    pub extra_path: &'a str,
    pub existing_authorized_keys: &'a str,
}

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
    codex_home: &str,
    install_dir: &str,
    extra_path: &str,
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
        home = shell_single_quote(codex_home),
        install = shell_single_quote(install_dir),
        extra = shell_single_quote(extra_path),
    )
}

pub fn wrapper_rewrites_local_codex_home(contents: &str, local_codex_home: &str) -> bool {
    let export = format!("export CODEX_HOME={}", shell_single_quote(local_codex_home));
    contents.contains(&export)
}

pub fn is_managed_wrapper(contents: &str) -> bool {
    contents.starts_with(&format!("#!/bin/sh\n{WRAPPER_MARKER}\n"))
}

pub fn marked_authorized_keys_line(host_id: &str, wrapper_path: &str, public_key: &str) -> String {
    let quoted_wrapper = format!("\"{}\"", wrapper_path.replace('"', "\\\""));
    let command = format!(
        "command={quoted_wrapper},no-agent-forwarding,no-X11-forwarding {}",
        public_key.trim()
    );
    format!("# {AUTH_KEYS_MARKER_PREFIX}{host_id}\n{command} {AUTH_KEYS_MARKER_PREFIX}{host_id}\n")
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn remote_posix_join(base: &str, child: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        child.trim_start_matches('/')
    )
}

fn remote_posix_parent(path: &str) -> Option<&str> {
    let path = path.trim_end_matches('/');
    let (parent, _) = path.rsplit_once('/')?;
    Some(if parent.is_empty() { "/" } else { parent })
}

fn validate_remote_posix_path(label: &str, path: &str) -> Result<(), String> {
    if !path.starts_with('/') || path.contains('\\') || path.contains('\0') {
        return Err(format!("RemotePosixPathInvalid:{label}:{path}"));
    }
    Ok(())
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

pub fn parse_user_host_port(destination: &str) -> Result<(String, String, u16), String> {
    let dest = destination
        .trim()
        .strip_prefix("ssh://")
        .unwrap_or(destination.trim());
    if dest.is_empty() {
        return Err("SshDestinationEmpty".into());
    }
    let (user_host, port) = match dest.rsplit_once(':') {
        Some((left, maybe_port))
            if maybe_port.chars().all(|ch| ch.is_ascii_digit()) && !maybe_port.is_empty() =>
        {
            let port = maybe_port
                .parse::<u16>()
                .map_err(|_| format!("SshPortInvalid: {maybe_port}"))?;
            (left, port)
        }
        _ => (dest, 22),
    };
    let (user, host) = match user_host.rsplit_once('@') {
        Some((user, host)) if !user.is_empty() && !host.is_empty() => (user, host),
        _ if !user_host.is_empty() => ("user", user_host),
        _ => return Err("SshDestinationEmpty".into()),
    };
    Ok((user.to_string(), host.to_string(), port))
}

/// Resolve aliases through the same OpenSSH configuration used by the real
/// connection. A cached target such as `jetson-orin` does not carry its
/// configured `User`, `HostName`, or `Port`; inventing `User user` here makes
/// the dedicated alias unusable even though the original alias works.
pub fn resolve_user_host_port(destination: &str) -> Result<(String, String, u16), String> {
    let output = background_command("ssh")
        .args(["-G", "--", destination])
        .output()
        .map_err(|error| format!("ssh -G unavailable: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "failed to resolve SSH destination {destination}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_resolved_ssh_config(&String::from_utf8_lossy(&output.stdout))
}

fn parse_resolved_ssh_config(raw: &str) -> Result<(String, String, u16), String> {
    let mut user = None;
    let mut host = None;
    let mut port = None;
    for line in raw.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        match key {
            "user" => user = Some(value.trim().to_string()),
            "hostname" => host = Some(value.trim().to_string()),
            "port" => port = value.trim().parse::<u16>().ok(),
            _ => {}
        }
    }
    let user = user
        .filter(|value| !value.is_empty())
        .ok_or("ssh -G returned no user")?;
    let host = host
        .filter(|value| !value.is_empty())
        .ok_or("ssh -G returned no hostname")?;
    Ok((user, host, port.unwrap_or(22)))
}

/// Writes a marked SSH config block and returns the wrapper + authorized_keys
/// payload the remote install path must apply. Tests drive this function on
/// temp files; bootstrap calls it before copying the agent.
pub fn apply_managed_ssh_isolation(
    request: IsolationApplyRequest<'_>,
) -> Result<AppliedIsolation, String> {
    if request.identity_file.as_os_str().is_empty() {
        return Err("SshIdentityMissing".into());
    }
    validate_remote_posix_path("managedCodexHome", request.managed_codex_home)?;
    validate_remote_posix_path("installDir", request.install_dir)?;
    validate_remote_posix_path("extraPath", request.extra_path)?;
    let config = ManagedSshConfig {
        host_id: request.host_id.into(),
        alias: managed_alias(request.host_id),
        hostname: request.hostname.into(),
        user: request.user.into(),
        port: request.port,
        identity_file: request.identity_file.to_string_lossy().into_owned(),
    };
    let existing = fs::read_to_string(request.ssh_config_path).unwrap_or_default();
    let next = upsert_ssh_config(&existing, &config);
    if let Some(parent) = request.ssh_config_path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("SshConfigDirCreateFailed: {error}"))?;
    }
    fs::write(request.ssh_config_path, next.as_bytes())
        .map_err(|error| format!("SshConfigWriteFailed: {error}"))?;
    let wrapper_root =
        remote_posix_parent(request.managed_codex_home).unwrap_or(request.managed_codex_home);
    let wrapper_path = remote_posix_join(wrapper_root, "ssh-wrapper");
    let wrapper_contents = managed_ssh_wrapper_contents(
        request.managed_codex_home,
        request.install_dir,
        request.extra_path,
    );
    if wrapper_rewrites_local_codex_home(&wrapper_contents, "~/.codex")
        || wrapper_rewrites_local_codex_home(
            &wrapper_contents,
            &format!(
                "{}/.codex",
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("/Users/user"))
                    .display()
            ),
        )
    {
        return Err("SshWrapperMustNotRewriteLocalCodexHome".into());
    }
    let block = marked_authorized_keys_line(request.host_id, &wrapper_path, request.public_key);
    let authorized_keys =
        upsert_remote_authorized_keys(request.existing_authorized_keys, request.host_id, &block);
    Ok(AppliedIsolation {
        alias: config.alias,
        ssh_config_path: request.ssh_config_path.to_path_buf(),
        identity_file: request.identity_file.to_path_buf(),
        wrapper_path,
        wrapper_contents,
        authorized_keys,
    })
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
        let home = "/Users/joshhuang/.vellum-remote/codex";
        let contents = managed_ssh_wrapper_contents(
            home,
            "/Users/joshhuang/.vellum-remote/codex/packages/standalone/current",
            "/Users/joshhuang/.local/bin",
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
            "/Users/joshhuang/.vellum-remote/ssh-wrapper",
            "ssh-ed25519 AAAAMANAGED vellum",
        );
        let added = upsert_remote_authorized_keys(keys, "mac-mini", &block);
        let removed = remove_remote_authorized_keys(&added, "mac-mini");
        assert_eq!(removed, keys);
        assert!(AUTH_KEYS_MARKER_PREFIX.starts_with("vellum-remote-managed"));
    }

    #[test]
    fn apply_managed_ssh_isolation_writes_config_and_wrapper_without_touching_local_home() {
        let temp = tempfile::tempdir().unwrap();
        let ssh_config = temp.path().join("config");
        fs::write(&ssh_config, "Host gpu-dev\n    HostName 192.0.2.10\n").unwrap();
        let identity = temp.path().join("id_ed25519");
        let home = "/Users/joshhuang/.vellum-remote/codex";
        let applied = apply_managed_ssh_isolation(IsolationApplyRequest {
            ssh_config_path: &ssh_config,
            identity_file: &identity,
            host_id: "mac-mini",
            hostname: "100.78.101.55",
            user: "joshhuang",
            port: 22,
            public_key: "ssh-ed25519 AAAAMANAGED vellum",
            managed_codex_home: home,
            install_dir: "/Users/joshhuang/.vellum-remote/codex/packages/standalone/current",
            extra_path: "/Users/joshhuang/.local/bin",
            existing_authorized_keys: "ssh-ed25519 AAAAUSER mine\n",
        })
        .unwrap();
        let config = fs::read_to_string(&ssh_config).unwrap();
        assert!(config.contains("Host gpu-dev"));
        assert!(config.contains("Host vellum-remote-mac-mini"));
        assert!(config.contains("IdentityFile"));
        assert_eq!(applied.alias, "vellum-remote-mac-mini");
        assert!(applied
            .wrapper_contents
            .contains("CODEX_HOME='/Users/joshhuang/.vellum-remote/codex'"));
        assert!(!applied
            .wrapper_contents
            .contains("CODEX_HOME='/Users/joshhuang/.codex'"));
        assert!(applied
            .authorized_keys
            .contains("ssh-ed25519 AAAAUSER mine"));
        assert!(applied
            .authorized_keys
            .contains("vellum-remote-managed:mac-mini"));
        assert_eq!(
            parse_user_host_port("joshhuang@100.78.101.55").unwrap(),
            ("joshhuang".into(), "100.78.101.55".into(), 22)
        );
    }

    #[test]
    fn linux_remote_paths_never_use_windows_separators() {
        let temp = tempfile::tempdir().unwrap();
        let applied = apply_managed_ssh_isolation(IsolationApplyRequest {
            ssh_config_path: &temp.path().join("config"),
            identity_file: &temp.path().join("id_ed25519"),
            host_id: "linux-host",
            hostname: "example.test",
            user: "josh",
            port: 22,
            public_key: "ssh-ed25519 AAAAMANAGED vellum",
            managed_codex_home: "/home/josh/.codex",
            install_dir: "/home/josh/.codex/packages/standalone/current",
            extra_path: "/home/josh/.local/bin",
            existing_authorized_keys: "",
        })
        .unwrap();

        assert_eq!(applied.wrapper_path, "/home/josh/ssh-wrapper");
        assert!(applied
            .authorized_keys
            .contains("command=\"/home/josh/ssh-wrapper\""));
        assert!(applied
            .wrapper_contents
            .contains("export PATH='/home/josh/.codex/packages/standalone/current':'/home/josh/.local/bin':\"$PATH\""));
        assert!(!applied.authorized_keys.contains('\\'));
        for line in applied
            .wrapper_contents
            .lines()
            .filter(|line| line.starts_with("export "))
        {
            assert!(
                !line.contains('\\'),
                "remote path export was not POSIX: {line}"
            );
        }
    }

    #[test]
    fn remote_paths_with_windows_separators_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let error = apply_managed_ssh_isolation(IsolationApplyRequest {
            ssh_config_path: &temp.path().join("config"),
            identity_file: &temp.path().join("id_ed25519"),
            host_id: "linux-host",
            hostname: "example.test",
            user: "josh",
            port: 22,
            public_key: "ssh-ed25519 AAAAMANAGED vellum",
            managed_codex_home: "/home/josh\\.codex",
            install_dir: "/home/josh/.codex/packages/standalone/current",
            extra_path: "/home/josh/.local/bin",
            existing_authorized_keys: "",
        })
        .unwrap_err();

        assert!(error.starts_with("RemotePosixPathInvalid:managedCodexHome:"));
    }

    #[test]
    fn resolved_ssh_config_preserves_alias_user_host_and_port() {
        assert_eq!(
            parse_resolved_ssh_config(
                "host jetson-orin\nuser josh\nhostname 100.107.71.86\nport 2202\n"
            )
            .unwrap(),
            ("josh".into(), "100.107.71.86".into(), 2202)
        );
    }
}
