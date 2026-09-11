//! Read-only discovery of Codex App managed SSH connections.
//!
//! Codex's global state is a private implementation detail, so this parser is
//! deliberately tolerant and snapshot based.  We never rewrite the file and
//! always retain an OpenSSH-config fallback when the shape changes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::AppResult;
use crate::remote::local_cache::CachedHost;
use crate::remote::process::background_command;

const CODEX_CONNECTIONS_KEY: &str = "codex-managed-remote-connections";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteHostCandidate {
    pub codex_host_id: Option<String>,
    pub vellum_host_id: String,
    pub display_name: String,
    pub ssh_alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub source: String,
    pub validated: bool,
    pub validation_error: Option<String>,
}

pub fn discover(cached: &[CachedHost]) -> AppResult<Vec<RemoteHostCandidate>> {
    discover_from_paths(
        codex_global_state_path().as_deref(),
        ssh_config_path().as_deref(),
        cached,
        resolve_ssh,
    )
}

fn codex_global_state_path() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".codex")
            .join(".codex-global-state.json"),
    )
}

fn ssh_config_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".ssh").join("config"))
}

fn discover_from_paths<F>(
    state_path: Option<&Path>,
    ssh_path: Option<&Path>,
    cached: &[CachedHost],
    mut resolver: F,
) -> AppResult<Vec<RemoteHostCandidate>>
where
    F: FnMut(&str) -> Result<SshResolved, String>,
{
    let mut seeds = Vec::new();
    if let Some(path) = state_path {
        if path.is_file() {
            match read_stable_snapshot(path).and_then(|raw| parse_codex_connections(&raw)) {
                Ok(items) => seeds.extend(items),
                Err(error) => log::warn!(
                    "[RemoteDiscovery] Codex connection snapshot incompatible; using OpenSSH fallback: {error}"
                ),
            }
        }
    }

    let mut known_aliases = seeds
        .iter()
        .map(|item| item.ssh_alias.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    if let Some(path) = ssh_path {
        if let Ok(raw) = fs::read_to_string(path) {
            for alias in parse_ssh_aliases(&raw) {
                if known_aliases.insert(alias.to_ascii_lowercase()) {
                    seeds.push(ConnectionSeed {
                        codex_host_id: None,
                        display_name: alias.clone(),
                        ssh_alias: alias,
                        source: "openSsh".into(),
                    });
                }
            }
        }
    }

    let cached_by_alias = cached
        .iter()
        .map(|host| (host.ssh_alias.to_ascii_lowercase(), host.id.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut output = Vec::with_capacity(seeds.len());
    for seed in seeds {
        let vellum_host_id = cached_by_alias
            .get(&seed.ssh_alias.to_ascii_lowercase())
            .cloned()
            .unwrap_or_else(|| stable_host_id(seed.codex_host_id.as_deref(), &seed.ssh_alias));
        match resolver(&seed.ssh_alias) {
            Ok(resolved) => output.push(RemoteHostCandidate {
                codex_host_id: seed.codex_host_id,
                vellum_host_id,
                display_name: seed.display_name,
                ssh_alias: seed.ssh_alias,
                hostname: resolved.hostname,
                user: resolved.user,
                port: resolved.port,
                source: seed.source,
                validated: true,
                validation_error: None,
            }),
            Err(error) => output.push(RemoteHostCandidate {
                codex_host_id: seed.codex_host_id,
                vellum_host_id,
                display_name: seed.display_name,
                ssh_alias: seed.ssh_alias,
                hostname: None,
                user: None,
                port: None,
                source: seed.source,
                validated: false,
                validation_error: Some(error),
            }),
        }
    }
    output.sort_by(|left, right| left.display_name.cmp(&right.display_name));
    Ok(output)
}

#[derive(Debug)]
struct ConnectionSeed {
    codex_host_id: Option<String>,
    display_name: String,
    ssh_alias: String,
    source: String,
}

fn parse_codex_connections(raw: &str) -> Result<Vec<ConnectionSeed>, String> {
    let root: Value = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    let entries = root
        .get(CODEX_CONNECTIONS_KEY)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{CODEX_CONNECTIONS_KEY} is not an array"))?;
    let mut output = Vec::new();
    for entry in entries {
        // Older Codex builds persisted an OpenSSH `alias`. Current builds may
        // instead persist the complete SSH target in `hostname` (for example
        // `tester@192.0.2.10`) and leave `alias` null. Both are valid inputs
        // to `ssh -G -- <target>`; prefer the alias so existing identities and
        // ProxyJump rules continue to resolve exactly as before.
        let Some(ssh_target) = codex_ssh_target(entry) else {
            log::warn!("[RemoteDiscovery] ignoring Codex connection without a safe SSH target");
            continue;
        };
        let host_id = entry
            .get("hostId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let display_name = entry
            .get("displayName")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(&ssh_target)
            .to_string();
        output.push(ConnectionSeed {
            codex_host_id: host_id,
            display_name,
            ssh_alias: ssh_target,
            source: "codexApp".into(),
        });
    }
    Ok(output)
}

fn codex_ssh_target(entry: &Value) -> Option<String> {
    if let Some(alias) = entry
        .get("alias")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| valid_ssh_alias(value))
    {
        return Some(alias.to_string());
    }

    let hostname = entry
        .get("hostname")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| valid_ssh_alias(value))?;
    if hostname.contains('@') {
        return Some(hostname.to_string());
    }
    let user = entry
        .get("user")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| valid_ssh_user(value));
    Some(match user {
        Some(user) => format!("{user}@{hostname}"),
        None => hostname.to_string(),
    })
}

fn read_stable_snapshot(path: &Path) -> Result<String, String> {
    for _ in 0..3 {
        let before = fs::metadata(path).map_err(|error| error.to_string())?;
        let raw = fs::read_to_string(path).map_err(|error| error.to_string())?;
        let after = fs::metadata(path).map_err(|error| error.to_string())?;
        if before.len() == after.len() && before.modified().ok() == after.modified().ok() {
            return Ok(raw);
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err("Codex global state changed while being read".into())
}

#[derive(Debug)]
struct SshResolved {
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
}

fn resolve_ssh(alias: &str) -> Result<SshResolved, String> {
    if !valid_ssh_alias(alias) {
        return Err("unsafe SSH alias".into());
    }
    let output = background_command("ssh")
        .args(["-G", "--", alias])
        .output()
        .map_err(|error| format!("ssh -G unavailable: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let mut result = SshResolved {
        hostname: None,
        user: None,
        port: None,
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        match key {
            "hostname" => result.hostname = Some(value.trim().to_string()),
            "user" => result.user = Some(value.trim().to_string()),
            "port" => result.port = value.trim().parse().ok(),
            _ => {}
        }
    }
    if result.hostname.as_deref().is_none_or(str::is_empty) {
        return Err("ssh -G returned no hostname".into());
    }
    Ok(result)
}

fn parse_ssh_aliases(raw: &str) -> Vec<String> {
    let mut aliases = BTreeSet::new();
    for line in raw.lines() {
        let line = line.trim();
        let Some(rest) = line
            .strip_prefix("Host ")
            .or_else(|| line.strip_prefix("host "))
        else {
            continue;
        };
        for alias in rest.split_whitespace() {
            if valid_ssh_alias(alias) && !alias.contains(['*', '!', '?']) {
                aliases.insert(alias.to_string());
            }
        }
    }
    aliases.into_iter().collect()
}

fn valid_ssh_alias(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.starts_with('-')
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '@'))
}

fn valid_ssh_user(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
}

fn stable_host_id(codex_host_id: Option<&str>, alias: &str) -> String {
    use sha2::{Digest, Sha256};
    let source = codex_host_id.unwrap_or(alias);
    let digest = hex::encode(Sha256::digest(source.as_bytes()));
    format!("remote-{}", &digest[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codex_connections_without_private_or_secret_fields() {
        let raw = r#"{"codex-managed-remote-connections":[{"hostId":"remote-1","displayName":"Jetson","source":"sshConfig","alias":"jetson","hostname":"10.0.0.2","identity":"must-not-leak"}]}"#;
        let seeds = parse_codex_connections(raw).unwrap();
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].ssh_alias, "jetson");
        assert_eq!(seeds[0].codex_host_id.as_deref(), Some("remote-1"));
    }

    #[test]
    fn parses_current_codex_hostname_target_when_alias_is_null() {
        let raw = r#"{"codex-managed-remote-connections":[{"hostId":"remote-ssh-codex-managed:gpu-dev","displayName":"gpu-dev","alias":null,"hostname":"tester@192.0.2.10","identity":"must-not-leak"}]}"#;
        let seeds = parse_codex_connections(raw).unwrap();
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].ssh_alias, "tester@192.0.2.10");
        assert_eq!(
            seeds[0].codex_host_id.as_deref(),
            Some("remote-ssh-codex-managed:gpu-dev")
        );
        assert_eq!(seeds[0].display_name, "gpu-dev");
    }

    #[test]
    fn combines_current_codex_user_and_hostname_fields() {
        let raw = r#"{"codex-managed-remote-connections":[{"displayName":"Example host","alias":null,"hostname":"192.0.2.10","user":"operator"}]}"#;
        let seeds = parse_codex_connections(raw).unwrap();
        assert_eq!(seeds[0].ssh_alias, "operator@192.0.2.10");
    }

    #[test]
    fn malformed_codex_connection_does_not_hide_valid_siblings() {
        let raw = r#"{"codex-managed-remote-connections":[{"displayName":"invalid","alias":null},{"displayName":"Jetson","hostname":"tester@192.0.2.10"}]}"#;
        let seeds = parse_codex_connections(raw).unwrap();
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].ssh_alias, "tester@192.0.2.10");
    }

    #[test]
    fn falls_back_to_openssh_and_deduplicates_codex_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state.json");
        let ssh = temp.path().join("config");
        fs::write(
            &state,
            r#"{"codex-managed-remote-connections":[{"hostId":"one","alias":"alpha"}]}"#,
        )
        .unwrap();
        fs::write(&ssh, "Host alpha beta *\n  User test\n").unwrap();
        let found = discover_from_paths(Some(&state), Some(&ssh), &[], |alias| {
            Ok(SshResolved {
                hostname: Some(format!("{alias}.example")),
                user: Some("test".into()),
                port: Some(22),
            })
        })
        .unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(
            found
                .iter()
                .filter(|item| item.ssh_alias == "alpha")
                .count(),
            1
        );
        assert!(found
            .iter()
            .any(|item| item.ssh_alias == "beta" && item.source == "openSsh"));
    }

    #[test]
    fn rejects_aliases_that_can_become_ssh_options() {
        assert!(!valid_ssh_alias("-oProxyCommand=bad"));
        assert!(!valid_ssh_alias("host;bad"));
        assert!(valid_ssh_alias("user@host.example"));
    }
}
