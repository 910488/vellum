//! User-added hosts live exclusively in Vellum-owned files. Initial bootstrap
//! authenticates with the user's keys; only then can its wrapper key be used.
use super::discovery::RemoteHostCandidate;
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn config_path(root: &Path) -> PathBuf {
    root.join("remote-ssh/config")
}
pub fn is_added_alias(alias: &str) -> bool {
    alias.starts_with("vellum-added-") || alias.starts_with("vellum-remote-added-")
}
pub fn config_args(root: &Path, alias: &str) -> Vec<String> {
    if is_added_alias(alias) {
        vec![
            "-F".into(),
            config_path(root).to_string_lossy().replace('\\', "/"),
        ]
    } else {
        vec![]
    }
}
pub fn command(root: &Path, alias: &str) -> Command {
    let mut command = super::process::background_command("ssh");
    command.args(config_args(root, alias));
    command
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddedHost {
    pub candidate: RemoteHostCandidate,
    pub login_command: String,
    pub key_install_command: String,
}
fn registry(root: &Path) -> PathBuf {
    root.join("remote-ssh/added-hosts.json")
}
pub(super) fn isolation_options(config: &str, host_id: &str) -> String {
    let begin = format!("# vellum-remote-managed:begin {host_id}-bootstrap");
    let end = format!("# vellum-remote-managed:end {host_id}-bootstrap");
    let mut inside = false;
    let mut options = String::new();
    for line in config.lines() {
        if line == begin {
            inside = true;
            continue;
        }
        if line == end {
            break;
        }
        if !inside {
            continue;
        }
        let key = line.split_whitespace().next().unwrap_or_default();
        if matches!(
            key,
            "proxyjump"
                | "proxycommand"
                | "hostkeyalias"
                | "userknownhostsfile"
                | "globalknownhostsfile"
        ) {
            options.push_str(line);
            options.push('\n');
        }
    }
    options
}
pub fn list(root: &Path) -> AppResult<Vec<RemoteHostCandidate>> {
    match fs::read(registry(root)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| AppError::Message(format!("SshAddedHostsInvalid: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(AppError::Message(e.to_string())),
    }
}
pub fn add(root: &Path, input: &str, home: &Path) -> AppResult<AddedHost> {
    add_with_resolver(root, input, home, |user, host, port| {
        // Resolve the original hostname before renaming it to a private alias:
        // Host-specific identities/trust/proxy settings would otherwise stop matching.
        let output = super::process::background_command("ssh")
            .args(["-G", "-l", user, "-p", &port.to_string(), "--", host])
            .output()
            .map_err(|e| AppError::Message(format!("SshConfigResolutionFailed: {e}")))?;
        if !output.status.success() {
            return Err(AppError::Message(
                "SshConfigResolutionFailed: ssh -G failed".into(),
            ));
        }
        String::from_utf8(output.stdout)
            .map_err(|e| AppError::Message(format!("SshConfigResolutionFailed: {e}")))
    })
}

fn add_with_resolver(
    root: &Path,
    input: &str,
    home: &Path,
    resolve: impl FnOnce(&str, &str, u16) -> AppResult<String>,
) -> AppResult<AddedHost> {
    let (user, raw_host, port) =
        super::ssh_isolation::parse_user_host_port(input).map_err(AppError::Message)?;
    let host = raw_host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(&raw_host);
    if !input.contains('@')
        || user.len() > 128
        || host.len() > 255
        || user.is_empty()
        || user.starts_with('-')
        || !user
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
        || host.is_empty()
        || host.starts_with('-')
        || port == 0
        || !host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-:".contains(&c))
        || (host.contains(':') && !raw_host.starts_with('['))
    {
        return Err(AppError::Message(
            "SshDestinationInvalid: expected user@host[:port]".into(),
        ));
    }
    let id = format!(
        "added-{}",
        &hex::encode(Sha256::digest(
            format!("{user}@{}:{port}", host.to_ascii_lowercase()).as_bytes()
        ))[..20]
    );
    let effective = resolve(&user, host, port)?;
    let mut hostname = host.to_owned();
    let mut options = String::new();
    for line in effective.lines() {
        let Some((key, value)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let value = original_host_tokens(value.trim(), host);
        let value = value.as_str();
        if key == "hostname" {
            hostname = value.to_owned();
        }
        if value.is_empty() {
            continue;
        }
        match key {
            "identityfile" | "identityagent" | "certificatefile" => {
                options.push_str(&format!(
                    "    {key} \"{}\"\n",
                    value.replace('\\', "/").replace('"', "\\\"")
                ));
            }
            "hostkeyalias"
            | "userknownhostsfile"
            | "globalknownhostsfile"
            | "proxyjump"
            | "proxycommand"
            | "identitiesonly" => options.push_str(&format!("    {key} {value}\n")),
            _ => {}
        }
    }
    if hostname.is_empty()
        || hostname.starts_with('-')
        || !hostname
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-:".contains(&c))
    {
        return Err(AppError::Message(
            "SshConfigResolutionFailed: invalid resolved hostname".into(),
        ));
    }
    let alias = format!("vellum-{id}");
    let candidate = RemoteHostCandidate {
        codex_host_id: None,
        vellum_host_id: id.clone(),
        display_name: format!("{user}@{host}:{port}"),
        ssh_alias: alias.clone(),
        hostname: Some(hostname.clone()),
        user: Some(user.clone()),
        port: Some(port),
        source: "vellumAdded".into(),
        validated: true,
        validation_error: None,
    };
    let path = config_path(root);
    fs::create_dir_all(
        path.parent()
            .ok_or_else(|| AppError::Message("SshConfigPathInvalid".into()))?,
    )
    .map_err(|e| AppError::Message(e.to_string()))?;
    let guard = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("remote-ssh/config.lock"))
        .map_err(|e| AppError::Message(e.to_string()))?;
    fs2::FileExt::lock_exclusive(&guard).map_err(|e| AppError::Message(e.to_string()))?;
    let existing = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => format!(
            "Host *\n    Include \"{}\"\n",
            home.join(".ssh/config")
                .to_string_lossy()
                .replace('\\', "/")
                .replace('"', "\\\"")
        ),
        Err(e) => return Err(AppError::Message(e.to_string())),
    };
    let marker = format!("{id}-bootstrap");
    let clean = super::ssh_isolation::remove_ssh_config(&existing, &marker);
    // Before the user's Include so its defaults cannot replace explicit fields.
    let block = format!("# vellum-remote-managed:begin {marker}\nHost {alias}\n    HostName {hostname}\n    User {user}\n    Port {port}\n{options}# vellum-remote-managed:end {marker}\n");
    crate::enhanced_runtime::atomic::write_atomic(&path, format!("{block}{clean}").as_bytes())
        .map_err(|e| AppError::Message(e.to_string()))?;
    let mut hosts = list(root)?;
    hosts.retain(|h| h.vellum_host_id != id);
    hosts.push(candidate.clone());
    crate::enhanced_runtime::atomic::write_atomic(
        &registry(root),
        &serde_json::to_vec_pretty(&hosts).map_err(|e| AppError::Message(e.to_string()))?,
    )
    .map_err(|e| AppError::Message(e.to_string()))?;
    let config = path.to_string_lossy().replace('\\', "/");
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let login_command = format!("ssh -F {} -- {alias}", quote(&config));
    let key_install_command = format!("ssh-copy-id -F {} {alias}", quote(&config));
    Ok(AddedHost {
        candidate,
        login_command,
        key_install_command,
    })
}

// ssh -G preserves tokens in path/command settings. %n must keep referring to
// the original destination, rather than the generated private alias.
fn original_host_tokens(value: &str, host: &str) -> String {
    let mut result = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            match chars.next() {
                Some('n') => result.push_str(host),
                Some(next) => {
                    result.push('%');
                    result.push(next);
                }
                None => result.push('%'),
            }
        } else {
            result.push(ch);
        }
    }
    result
}

#[cfg(test)]
pub(super) fn fake_add(root: &Path, input: &str, home: &Path) -> AppResult<AddedHost> {
    add_with_resolver(root, input, home, |_, host, _| {
        Ok(format!("hostname {host}\n"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_added_aliases_use_the_private_config_including_after_isolation() {
        let root = Path::new("data with spaces");
        for alias in ["vellum-added-123", "vellum-remote-added-123"] {
            let args = config_args(root, alias);
            assert_eq!(args[0], "-F");
            assert!(args[1].ends_with("/remote-ssh/config"));
            assert_eq!(command(root, alias).get_args().count(), 2);
        }
        assert!(config_args(root, "personal").is_empty());
    }
    #[test]
    fn add_is_idempotent_and_never_writes_user_config_or_installs_isolation_key() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let root = temp.path().join("vellum");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::write(home.join(".ssh/config"), "Host personal\n  User keep\n").unwrap();
        let before = fs::read(home.join(".ssh/config")).unwrap();
        let one = fake_add(&root, "dev@example.test:2222", &home).unwrap();
        let two = fake_add(&root, "dev@example.test:2222", &home).unwrap();
        assert_eq!(one.candidate.vellum_host_id, two.candidate.vellum_host_id);
        assert_eq!(list(&root).unwrap().len(), 1);
        assert_eq!(fs::read(home.join(".ssh/config")).unwrap(), before);
        let contents = fs::read_to_string(config_path(&root)).unwrap();
        assert!(contents.contains("Port 2222"));
        assert!(!contents.contains("IdentityFile"));
        assert_eq!(contents.matches("Host vellum-added-").count(), 1);
        assert!(one.key_install_command.starts_with("ssh-copy-id -F "));
    }
    #[test]
    fn add_refuses_option_shell_and_config_injection_before_writing() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("vellum");
        for input in [
            "-oProxyCommand=x",
            "dev@host\nHost evil",
            "dev@host;touch",
            "dev@host:0",
            "host",
            "dev@$(exec)",
        ] {
            assert!(fake_add(&root, input, temp.path()).is_err(), "{input}");
        }
        assert!(!root.exists());
    }

    #[test]
    fn original_host_specific_trust_identity_and_jump_survive_private_alias_registration() {
        let temp = tempfile::tempdir().unwrap();
        let host = add_with_resolver(temp.path(), "dev@host.test:2222", temp.path(), |user, host, port| {
            assert_eq!((user, host, port), ("dev", "host.test", 2222));
            Ok("hostname 192.0.2.10\nidentityfile ~/.ssh/custom key\nidentityagent none\nuserknownhostsfile ~/.ssh/custom_known_hosts\nhostkeyalias pinned-name\nproxyjump bastion\n".into())
        }).unwrap();
        assert_eq!(host.candidate.hostname.as_deref(), Some("192.0.2.10"));
        let config = fs::read_to_string(config_path(temp.path())).unwrap();
        assert!(config.contains("identityfile \"~/.ssh/custom key\""));
        assert!(config.contains("identityagent \"none\""));
        assert!(config.contains("hostkeyalias pinned-name"));
        assert!(config.contains("userknownhostsfile ~/.ssh/custom_known_hosts"));
        assert!(config.contains("proxyjump bastion"));
        let options = isolation_options(&config, &host.candidate.vellum_host_id);
        assert!(options.contains("proxyjump bastion"));
        assert!(options.contains("userknownhostsfile ~/.ssh/custom_known_hosts"));
        assert!(!options.contains("identityfile"));
        assert_eq!(
            original_host_tokens("~/.ssh/%n-%%n", "host.test"),
            "~/.ssh/host.test-%%n"
        );
    }
}
