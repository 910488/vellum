//! Read only the user's effective UserKnownHostsFile. Never import whole files.
use super::{
    persist_trust, preferred_fingerprint, runtime_data_root, AppError, AppResult,
    PendingHostFingerprint, SshTarget,
};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) struct UserTrust {
    lookup: String,
    files: Vec<PathBuf>,
}
impl UserTrust {
    pub(super) fn has_trust_hint(&self) -> bool {
        self.files
            .iter()
            .filter_map(|path| fs::read_to_string(path).ok())
            .any(|text| {
                text.lines().any(|line| {
                    let mut fields = line.split_whitespace();
                    let first = fields.next().unwrap_or_default();
                    let name = if first.starts_with('@') {
                        fields.next().unwrap_or_default()
                    } else {
                        first
                    };
                    !first.starts_with('#')
                        && first != "@cert-authority"
                        && name_matches(name, &self.lookup)
                })
            })
    }
}
pub(super) fn resolve(destination: &str, target: &SshTarget) -> AppResult<UserTrust> {
    let output = crate::remote::ssh_added::command(&runtime_data_root(), destination)
        .args(["-G", "--", destination])
        .output()
        .map_err(|e| AppError::Message(format!("SSH trust config resolution failed: {e}")))?;
    if !output.status.success() {
        return Err(AppError::Message(
            "SSH trust config resolution failed".into(),
        ));
    }
    Ok(parse(
        &String::from_utf8_lossy(&output.stdout),
        dirs::home_dir().as_deref(),
        destination,
        target,
    ))
}
// ssh -G can emit quoted paths with spaces. Unsupported/unknown expansions
// deliberately disable inheritance; an explicit confirmation remains possible.
fn words(raw: &str) -> Option<Vec<String>> {
    let mut values = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    for ch in raw.chars() {
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            } else {
                word.push(ch);
            }
        } else if ch.is_whitespace() && quote.is_none() {
            if !word.is_empty() {
                values.push(std::mem::take(&mut word));
            }
        } else {
            word.push(ch);
        }
    }
    if quote.is_some() {
        return None;
    }
    if !word.is_empty() {
        values.push(word);
    }
    Some(values)
}
fn parse(raw: &str, home: Option<&Path>, destination: &str, target: &SshTarget) -> UserTrust {
    let mut lookup = target.known_hosts_pattern().to_ascii_lowercase();
    let mut file_values = None;
    let mut user = String::new();
    let mut local_user = String::new();
    for line in raw.lines() {
        let Some((key, value)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        match key.to_ascii_lowercase().as_str() {
            // OpenSSH sshconnect.c:get_hostfile_hostname_ipaddr uses an alias
            // literally, including on non-default ports (does not add :port).
            "hostkeyalias" => lookup = value.trim().into(),
            "userknownhostsfile" => file_values = words(value),
            "user" => user = value.trim().into(),
            "localuser" => local_user = value.trim().into(),
            _ => {}
        }
    }
    let files = file_values
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| {
            if value == "none" {
                return None;
            }
            let home = home?.to_string_lossy().replace('\\', "/");
            let mut expanded = String::new();
            let mut chars = value.chars();
            while let Some(ch) = chars.next() {
                if ch != '%' {
                    expanded.push(ch);
                    continue;
                }
                match chars.next()? {
                    '%' => expanded.push('%'),
                    'd' => expanded.push_str(&home),
                    'h' => expanded.push_str(&target.host),
                    'p' => expanded.push_str(&target.port.to_string()),
                    'r' => expanded.push_str(&user),
                    'u' if !local_user.is_empty() => expanded.push_str(&local_user),
                    'n' => expanded.push_str(destination.rsplit('@').next()?),
                    'k' => expanded.push_str(&lookup),
                    _ => return None,
                }
            }
            if let Some(rest) = expanded.strip_prefix("~/") {
                expanded = format!("{home}/{rest}");
            }
            let path = PathBuf::from(expanded);
            path.is_absolute().then_some(path)
        })
        .collect();
    UserTrust { lookup, files }
}

fn glob(pattern: &str, host: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let host = host.to_ascii_lowercase();
    let mut previous = vec![false; host.len() + 1];
    previous[0] = true;
    for ch in pattern.bytes() {
        let mut next = vec![false; host.len() + 1];
        if ch == b'*' {
            next[0] = previous[0];
        }
        for (i, byte) in host.bytes().enumerate() {
            next[i + 1] = if ch == b'*' {
                previous[i + 1] || next[i]
            } else {
                previous[i] && (ch == b'?' || ch == byte)
            };
        }
        previous = next;
    }
    previous[host.len()]
}
fn name_matches(names: &str, host: &str) -> bool {
    if let Some(hashed) = names.strip_prefix("|1|") {
        let Some((salt, digest)) = hashed.split_once('|') else {
            return false;
        };
        let (Ok(salt), Ok(digest)) = (STANDARD.decode(salt), STANDARD.decode(digest)) else {
            return false;
        };
        let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(&salt) else {
            return false;
        };
        mac.update(host.as_bytes());
        return mac.verify_slice(&digest).is_ok();
    }
    let mut matched = false;
    for name in names.split(',') {
        if let Some(negative) = name.strip_prefix('!') {
            if glob(negative, host) {
                return false;
            }
        } else if glob(name, host) {
            matched = true;
        }
    }
    matched
}
#[derive(Debug, PartialEq)]
enum Decision {
    Unknown,
    Same,
    Revoked,
    Changed,
}
fn assess(config: &UserTrust, candidate: &PendingHostFingerprint) -> Decision {
    let Some(encoded) = candidate.key_line.split_whitespace().nth(2) else {
        return Decision::Unknown;
    };
    let Ok(scanned) = STANDARD.decode(encoded) else {
        return Decision::Unknown;
    };
    let mut same = false;
    let mut revoked = false;
    let mut changed = false;
    let mut unreadable = false;
    for path in &config.files {
        let Ok(text) = fs::read_to_string(path) else {
            unreadable = true;
            continue;
        };
        for line in text
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
        {
            let mut fields = line.split_whitespace();
            let Some(first) = fields.next() else {
                continue;
            };
            let (marker, names) = if first.starts_with('@') {
                (Some(first), fields.next().unwrap_or_default())
            } else {
                (None, first)
            };
            if !name_matches(names, &config.lookup) {
                continue;
            }
            if marker == Some("@revoked") {
                revoked = true;
                continue;
            }
            if marker.is_some() {
                continue;
            } // CA trust cannot prove a scanned host key
            let (Some(kind), Some(key)) = (fields.next(), fields.next()) else {
                unreadable = true;
                continue;
            };
            let Ok(key) = STANDARD.decode(key) else {
                unreadable = true;
                continue;
            };
            if kind == candidate.key_type && key == scanned {
                same = true;
            } else {
                changed = true;
            }
        }
    }
    if changed {
        Decision::Changed
    } else if revoked {
        Decision::Revoked
    } else if same && !unreadable {
        Decision::Same
    } else {
        Decision::Unknown
    }
}
pub(super) fn refuse_changed(
    config: &UserTrust,
    candidates: &[PendingHostFingerprint],
) -> AppResult<()> {
    if candidates
        .iter()
        .any(|c| assess(config, c) == Decision::Changed)
    {
        return Err(AppError::Message("SshHostKeyFingerprintMismatch: user known_hosts has a different key for this host; no trust was written".into()));
    }
    Ok(())
}
pub(super) fn inherit(
    root: &Path,
    target: &SshTarget,
    config: &UserTrust,
    candidates: Vec<PendingHostFingerprint>,
) -> AppResult<bool> {
    // Choose the preferred offered key first; never import another key/host.
    let Some(candidate) = preferred_fingerprint(candidates) else {
        return Ok(false);
    };
    match assess(config, &candidate) {
        Decision::Changed => Err(AppError::Message("SshHostKeyFingerprintMismatch: user known_hosts has a different key for this host; no trust was written".into())),
        Decision::Same => { persist_trust(root, target, candidate, "userKnownHosts")?; Ok(true) }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{known_hosts_path, load_store, parse_keyscan_output, ConfirmedRecord};
    use super::*;
    const KEY: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIBaSZz5FL6QOOBF+KXsnX3YHy7uwQ+DdzngG/JSpVjOQ";
    fn fixture(
        port: u16,
    ) -> (
        tempfile::TempDir,
        SshTarget,
        UserTrust,
        PendingHostFingerprint,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let target = SshTarget {
            host: "host.test".into(),
            port,
        };
        let config = UserTrust {
            lookup: target.known_hosts_pattern(),
            files: vec![temp.path().join("user-known-hosts")],
        };
        let candidate =
            parse_keyscan_output(&target, format!("host.test ssh-ed25519 {KEY}\n").as_bytes())
                .remove(0);
        (temp, target, config, candidate)
    }
    #[test]
    fn hashed_match_imports_only_scanned_key_and_records_user_source() {
        let (temp, target, config, candidate) = fixture(22);
        let salt = b"known-hosts-test-salt";
        let mut mac = Hmac::<Sha1>::new_from_slice(salt).unwrap();
        mac.update(b"host.test");
        let line = format!(
            "|1|{}|{} ssh-ed25519 {KEY}\nother.test ssh-ed25519 AAAA\n",
            STANDARD.encode(salt),
            STANDARD.encode(mac.finalize().into_bytes())
        );
        fs::write(&config.files[0], &line).unwrap();
        assert!(config.has_trust_hint());
        assert!(inherit(
            &temp.path().join("vellum"),
            &target,
            &config,
            vec![candidate]
        )
        .unwrap());
        assert_eq!(fs::read_to_string(&config.files[0]).unwrap(), line);
        let root = temp.path().join("vellum");
        let stored = fs::read_to_string(known_hosts_path(&root)).unwrap();
        assert_eq!(stored.lines().count(), 1);
        assert!(!stored.contains("other.test"));
        assert_eq!(
            load_store(&root).unwrap().confirmed[&target.store_key()].source,
            "userKnownHosts"
        );
    }
    #[test]
    fn exact_port_comma_names_negations_and_hostkeyalias_are_respected() {
        let (temp, target, config, candidate) = fixture(2222);
        fs::write(
            &config.files[0],
            format!("[other.test]:2222,[host.test]:2222 ssh-ed25519 {KEY}\n"),
        )
        .unwrap();
        assert_eq!(assess(&config, &candidate), Decision::Same);
        let other_port = UserTrust {
            lookup: "[host.test]:2223".into(),
            files: config.files.clone(),
        };
        assert_eq!(assess(&other_port, &candidate), Decision::Unknown);
        let default_port = UserTrust {
            lookup: "host.test".into(),
            files: config.files.clone(),
        };
        assert_eq!(assess(&default_port, &candidate), Decision::Unknown);
        assert!(!name_matches("*.test,!host.test", "host.test"));
        let raw = format!(
            "user dev\nhostkeyalias pinned-name\nuserknownhostsfile \"{}\"\n",
            config.files[0].display()
        );
        let parsed = parse(&raw, Some(temp.path()), "alias", &target);
        assert_eq!(parsed.lookup, "pinned-name");
        assert_eq!(parsed.files, config.files);
        fs::write(&config.files[0], format!("pinned-name ssh-ed25519 {KEY}\n")).unwrap();
        assert_eq!(assess(&parsed, &candidate), Decision::Same);
    }
    #[test]
    fn revoked_ca_and_unreadable_or_missing_user_files_require_confirmation_without_writes() {
        let (temp, target, config, candidate) = fixture(22);
        let root = temp.path().join("vellum");
        for marker in ["@revoked", "@cert-authority"] {
            fs::write(
                &config.files[0],
                format!("{marker} host.test ssh-ed25519 {KEY}\n"),
            )
            .unwrap();
            assert!(!inherit(&root, &target, &config, vec![candidate.clone()]).unwrap());
            assert!(!root.exists());
        }
        fs::write(
            &config.files[0],
            format!("host.test ssh-ed25519 {KEY}\n@revoked host.test ssh-ed25519 {KEY}\n"),
        )
        .unwrap();
        assert_eq!(assess(&config, &candidate), Decision::Revoked);
        fs::write(&config.files[0], [0xff]).unwrap();
        assert!(!inherit(&root, &target, &config, vec![candidate.clone()]).unwrap());
        fs::remove_file(&config.files[0]).unwrap();
        assert!(!config.has_trust_hint());
        assert!(!inherit(&root, &target, &config, vec![candidate.clone()]).unwrap());
        fs::create_dir(&config.files[0]).unwrap();
        assert!(!inherit(&root, &target, &config, vec![candidate]).unwrap());
        assert!(!root.exists());
    }
    #[test]
    fn another_key_for_same_host_hard_fails_even_beside_a_matching_key() {
        let (temp, target, config, candidate) = fixture(22);
        let root = temp.path().join("vellum");
        for text in [
            String::from("host.test ssh-ed25519 AAAA\n"),
            format!("host.test ssh-ed25519 {KEY}\nhost.test ssh-ed25519 AAAA\n"),
        ] {
            fs::write(&config.files[0], &text).unwrap();
            assert!(refuse_changed(&config, std::slice::from_ref(&candidate))
                .unwrap_err()
                .to_string()
                .contains("SshHostKeyFingerprintMismatch"));
            assert!(inherit(&root, &target, &config, vec![candidate.clone()]).is_err());
            assert!(!root.exists());
            assert_eq!(fs::read_to_string(&config.files[0]).unwrap(), text);
        }
    }
    #[test]
    fn config_path_expansions_and_legacy_confirmation_source_are_safe() {
        let temp = tempfile::tempdir().unwrap();
        let target = SshTarget {
            host: "host.test".into(),
            port: 2222,
        };
        let parsed = parse(
            "user dev\nuserknownhostsfile ~/.ssh/known_hosts %d/keys-%h-%p-%r none\n",
            Some(temp.path()),
            "alias",
            &target,
        );
        assert_eq!(
            parsed.files,
            [
                temp.path().join(".ssh/known_hosts"),
                temp.path().join("keys-host.test-2222-dev")
            ]
        );
        let legacy = serde_json::json!({"host":"host.test", "port":22,"keyType":"ssh-ed25519", "fingerprint":"SHA256:test", "confirmedAt":"2026-10-16T00:00:00Z"});
        assert_eq!(
            serde_json::from_value::<ConfirmedRecord>(legacy)
                .unwrap()
                .source,
            "explicitConfirmation"
        );
    }

    #[test]
    fn inheritance_requires_key_type_and_uses_only_the_preferred_scanned_key() {
        let (temp, target, config, candidate) = fixture(22);
        fs::write(&config.files[0], format!("host.test ssh-ed25519 {KEY}\n")).unwrap();
        let mut rsa = candidate.clone();
        rsa.key_type = "ssh-rsa".into();
        assert_eq!(assess(&config, &rsa), Decision::Changed);
        let root = temp.path().join("vellum");
        assert!(inherit(&root, &target, &config, vec![rsa, candidate]).unwrap());
        let contents = fs::read_to_string(known_hosts_path(&root)).unwrap();
        assert_eq!(contents.lines().count(), 1);
        assert!(!contents.contains("ssh-rsa"));
    }
}
