//! Vellum-private SSH host-key trust store.
//!
//! Every `ssh` invocation Vellum makes (agent RPCs, artifact staging, the
//! bootstrap probe script) used to pass `-o StrictHostKeyChecking=accept-new`
//! against the *user's own* `~/.ssh/known_hosts` (or whatever `ssh_config(5)`
//! defaults dictate). `accept-new` trusts a host's key the first time it is
//! seen with no prompt and no visible decision point — a MITM present only
//! during that first connection is trusted silently and permanently.
//!
//! This module gives Vellum its own known_hosts file
//! (`<data_root>/remote/known_hosts`) that nothing but an explicit user
//! confirmation can write to, plus a small JSON side-store recording *that a
//! human confirmed a fingerprint*, independent of whatever bytes happen to be
//! in the known_hosts file. Trust decisions are keyed by the SSH-resolved
//! `(hostname, port)` pair, never by an SSH config alias: two aliases can
//! point at the same host on different ports, and one alias can silently
//! re-resolve to a different host later (dynamic DNS, an edited
//! `~/.ssh/config`). Both cases must require their own confirmation, which
//! falls out for free from keying on the resolved target instead of the
//! alias string.
//!
//! There is no code path in this module that can produce a "trusted but
//! never confirmed" record: the only way an entry enters the confirmation
//! store is [`confirm_and_trust`], which re-derives the fingerprint from the
//! host itself and stamps `confirmed_at` at write time. A record's mere
//! presence in the store *is* the confirmation; there is no separate
//! defaultable/nullable flag that could be constructed as `false` and later
//! misread as `true`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use base64::{
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
    Engine as _,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};
use crate::remote::process::background_command;
use crate::state::AppState;

const DEFAULT_SSH_PORT: u16 = 22;
const KEYSCAN_TIMEOUT_SECS: u64 = 10;

/// Preference order used when a host offers more than one key type. Matches
/// OpenSSH's own default `HostKeyAlgorithms` ordering (best assurance first).
const KEY_TYPE_PRIORITY: [&str; 5] = [
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "ssh-rsa",
];

/// A resolved SSH connection target: the actual hostname/IP and port `ssh`
/// will contact, as opposed to whatever alias or `user@host` string the
/// caller started with. Trust is always keyed on this, never on the alias.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
}

impl SshTarget {
    fn store_key(&self) -> String {
        format!("{}|{}", self.host.to_ascii_lowercase(), self.port)
    }

    /// Host pattern exactly as `known_hosts(5)` expects it: bare host for the
    /// default port, `[host]:port` otherwise.
    fn known_hosts_pattern(&self) -> String {
        if self.port == DEFAULT_SSH_PORT {
            self.host.clone()
        } else {
            format!("[{}]:{}", self.host, self.port)
        }
    }
}

/// A host key a host is currently offering, captured via `ssh-keyscan`
/// without connecting or authenticating and without trusting anything yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingHostFingerprint {
    pub host: String,
    pub port: u16,
    pub key_type: String,
    /// `SHA256:<base64-no-pad>` -- exactly what `ssh-keygen -lf` prints, so a
    /// user can cross-check it against another channel unmodified.
    pub fingerprint: String,
    /// Raw `known_hosts`-format line (`<pattern> <keytype> <base64key>`).
    /// Kept out of the wire format: the frontend never needs it, and
    /// `confirm_and_trust` re-derives it from a fresh keyscan rather than
    /// trusting whatever the renderer echoes back.
    #[serde(skip)]
    key_line: String,
}

/// Whether a resolved target's key has already been confirmed by the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshTrustStatus {
    pub host: String,
    pub port: u16,
    pub confirmed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfirmedRecord {
    host: String,
    port: u16,
    key_type: String,
    fingerprint: String,
    confirmed_at: DateTime<Utc>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConfirmationStore {
    #[serde(default)]
    confirmed: BTreeMap<String, ConfirmedRecord>,
}

/// `<data_root>/remote/known_hosts` -- Vellum's own, never the user's
/// `~/.ssh/known_hosts`.
pub fn known_hosts_path(data_root: &Path) -> PathBuf {
    data_root.join("remote").join("known_hosts")
}

fn confirmations_path(data_root: &Path) -> PathBuf {
    data_root
        .join("remote")
        .join("known_hosts_confirmations.json")
}

/// The `data_root` `ssh`-invoking call sites fall back to when they only
/// have a `&self` client/helper in scope and not a live `AppState` (most of
/// `RemoteAgentClient`'s callers, several of them in files outside this
/// stage's scope). Production only ever runs one `AppState`, constructed
/// from this exact same default, so the two are always the same directory
/// outside of tests -- and the SSH-touching paths this feeds are not part of
/// the deterministic test suite (real `ssh`/`ssh-keyscan` are not available
/// in the sandbox).
pub fn runtime_data_root() -> PathBuf {
    crate::state::app_data_dir()
}

fn load_store(data_root: &Path) -> AppResult<ConfirmationStore> {
    let path = confirmations_path(data_root);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            AppError::Message(format!("corrupt SSH trust confirmation store: {error}"))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(ConfirmationStore::default())
        }
        Err(error) => Err(AppError::Message(error.to_string())),
    }
}

fn save_store(data_root: &Path, store: &ConfirmationStore) -> AppResult<()> {
    let path = confirmations_path(data_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| AppError::Message(error.to_string()))?;
    }
    let bytes =
        serde_json::to_vec_pretty(store).map_err(|error| AppError::Message(error.to_string()))?;
    fs::write(&path, bytes).map_err(|error| AppError::Message(error.to_string()))
}

/// Has this exact resolved `(host, port)` already been explicitly confirmed?
pub fn is_confirmed(data_root: &Path, target: &SshTarget) -> AppResult<bool> {
    let store = load_store(data_root)?;
    Ok(store.confirmed.contains_key(&target.store_key()))
}

/// Fail closed with a message pointing at the confirmation step, unless the
/// target has already been explicitly confirmed. Callers that reach a real
/// `ssh`/`scp`-style connection must call this before connecting.
pub fn require_trust_or_error(data_root: &Path, target: &SshTarget) -> AppResult<()> {
    if is_confirmed(data_root, target)? {
        return Ok(());
    }
    Err(AppError::Message(format!(
        "SshHostKeyNotConfirmed: {}:{} has not been confirmed yet. Fetch its fingerprint and confirm it before connecting.",
        target.host, target.port
    )))
}

/// The `-o` pairs every trust-checked `ssh` invocation must carry once a
/// host's key has been confirmed: strict checking against Vellum's own
/// known_hosts file, never the ambient `~/.ssh/known_hosts` and never
/// `accept-new`.
pub fn strict_host_key_args(data_root: &Path) -> Vec<String> {
    vec![
        "-o".into(),
        "StrictHostKeyChecking=yes".into(),
        "-o".into(),
        format!(
            "UserKnownHostsFile={}",
            known_hosts_path(data_root).display()
        ),
    ]
}

/// Resolve an SSH config alias (or a bare `user@host`/`user@host:port`
/// destination, which `ssh -G` also accepts) to the actual hostname/IP and
/// port `ssh` will contact. Never trusts anything; this only reads config.
pub fn resolve_ssh_target(destination: &str) -> AppResult<SshTarget> {
    let output = background_command("ssh")
        .args(["-G", "--", destination])
        .output()
        .map_err(|error| AppError::Message(format!("ssh -G unavailable: {error}")))?;
    if !output.status.success() {
        return Err(AppError::Message(format!(
            "failed to resolve SSH destination {destination}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut host = None;
    let mut port = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        match key {
            "hostname" => host = Some(value.trim().to_string()),
            "port" => port = value.trim().parse::<u16>().ok(),
            _ => {}
        }
    }
    let host = host.filter(|value| !value.is_empty()).ok_or_else(|| {
        AppError::Message(format!("ssh -G returned no hostname for {destination}"))
    })?;
    Ok(SshTarget {
        host,
        port: port.unwrap_or(DEFAULT_SSH_PORT),
    })
}

/// SHA256 fingerprint of a base64-encoded host key blob, formatted exactly
/// like `ssh-keygen -lf`: SHA256 of the *decoded* key bytes, base64-encoded
/// without padding, prefixed `SHA256:`.
fn fingerprint_of(key_base64: &str) -> AppResult<String> {
    let raw = STANDARD
        .decode(key_base64.trim())
        .map_err(|error| AppError::Message(format!("invalid host key encoding: {error}")))?;
    let digest = Sha256::digest(&raw);
    Ok(format!("SHA256:{}", STANDARD_NO_PAD.encode(digest)))
}

/// Fetch every host key `target` currently offers via `ssh-keyscan`. This
/// never connects/authenticates as a client and never writes anything --
/// it only asks the host "what keys do you have", the same thing
/// `ssh-keyscan` is designed to do without trusting the answer.
pub fn fetch_pending_fingerprints(target: &SshTarget) -> AppResult<Vec<PendingHostFingerprint>> {
    let mut diagnostics = Vec::new();
    for program in keyscan_programs() {
        match run_keyscan(&program, target) {
            Ok(output) => {
                let results = parse_keyscan_output(target, &output.stdout);
                if !results.is_empty() {
                    return Ok(results);
                }
                diagnostics.push(keyscan_diagnostic(&program, &output));
            }
            Err(error) => {
                diagnostics.push(format!("{} could not start: {error}", program.display()))
            }
        }
    }
    Err(AppError::Message(format!(
        "SSH host key scan failed for {}:{}; {}",
        target.host,
        target.port,
        diagnostics.join("; ")
    )))
}

fn run_keyscan(program: &Path, target: &SshTarget) -> std::io::Result<Output> {
    background_command(program.as_os_str())
        .args([
            "-T",
            &KEYSCAN_TIMEOUT_SECS.to_string(),
            "-p",
            &target.port.to_string(),
            &target.host,
        ])
        .output()
}

fn parse_keyscan_output(target: &SshTarget, stdout: &[u8]) -> Vec<PendingHostFingerprint> {
    let text = String::from_utf8_lossy(stdout);
    let mut results = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(3, ' ');
        let Some(_host_field) = parts.next() else {
            continue;
        };
        let Some(key_type) = parts.next() else {
            continue;
        };
        let Some(key_base64) = parts.next() else {
            continue;
        };
        let Ok(fingerprint) = fingerprint_of(key_base64) else {
            continue;
        };
        results.push(PendingHostFingerprint {
            host: target.host.clone(),
            port: target.port,
            key_type: key_type.to_string(),
            fingerprint,
            key_line: format!(
                "{} {} {}",
                target.known_hosts_pattern(),
                key_type,
                key_base64.trim()
            ),
        });
    }
    results
}

fn keyscan_diagnostic(program: &Path, output: &Output) -> String {
    let detail = String::from_utf8_lossy(&output.stderr)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let detail = detail.chars().take(500).collect::<String>();
    let status = output
        .status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "terminated".into());
    if detail.is_empty() {
        format!("{} exited {status} without a host key", program.display())
    } else {
        format!("{} exited {status}: {detail}", program.display())
    }
}

fn keyscan_programs() -> Vec<PathBuf> {
    let mut programs = Vec::new();
    #[cfg(windows)]
    {
        // OpenSSH_for_Windows 9.5's ssh-keyscan aborts against otherwise
        // compatible Ubuntu 22.04 servers that advertise
        // sntrup761x25519-sha512@openssh.com, even though `ssh` itself
        // correctly negotiates curve25519. Git for Windows' keyscan does not
        // have that incompatibility, so prefer it when installed in one of
        // Git's standard locations. The system tool remains the fallback.
        let mut roots = Vec::new();
        if let Some(program_files) = std::env::var_os("ProgramFiles") {
            roots.push(PathBuf::from(program_files).join("Git"));
        }
        if let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)") {
            roots.push(PathBuf::from(program_files_x86).join("Git"));
        }
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            roots.push(PathBuf::from(local_app_data).join("Programs").join("Git"));
        }
        for root in roots {
            let candidate = root.join("usr").join("bin").join("ssh-keyscan.exe");
            if candidate.is_file() && !programs.contains(&candidate) {
                programs.push(candidate);
            }
        }
    }
    programs.push(PathBuf::from("ssh-keyscan"));
    programs
}

/// Pick the single fingerprint to show the user, preferring the strongest
/// key type a host offered. Deterministic so the same host always surfaces
/// the same fingerprint for confirmation.
pub fn preferred_fingerprint(
    mut candidates: Vec<PendingHostFingerprint>,
) -> Option<PendingHostFingerprint> {
    candidates.sort_by_key(|candidate| {
        KEY_TYPE_PRIORITY
            .iter()
            .position(|known| *known == candidate.key_type)
            .unwrap_or(usize::MAX)
    });
    candidates.into_iter().next()
}

/// Record the user's explicit confirmation of `expected_fingerprint` for
/// `target`, and only then append its key to Vellum's own known_hosts file.
/// The fingerprint is re-derived from a fresh `ssh-keyscan` rather than
/// trusting the caller's claim, so a host that changed keys between "fetch"
/// and "confirm" (or a caller passing an arbitrary string) cannot get a
/// stale/wrong key written as trusted.
pub fn confirm_and_trust(
    data_root: &Path,
    target: &SshTarget,
    expected_fingerprint: &str,
) -> AppResult<()> {
    let candidates = fetch_pending_fingerprints(target)?;
    confirm_matching_fingerprint(data_root, target, expected_fingerprint, candidates)
}

/// Persist trust only when `expected_fingerprint` is among the keys the host
/// is currently offering. A rotation between "fetch" and "confirm" (or a
/// caller passing an arbitrary string) hard-fails with no write and no
/// auto-accept of the new key.
fn confirm_matching_fingerprint(
    data_root: &Path,
    target: &SshTarget,
    expected_fingerprint: &str,
    candidates: Vec<PendingHostFingerprint>,
) -> AppResult<()> {
    let matched = candidates
        .into_iter()
        .find(|candidate| candidate.fingerprint == expected_fingerprint)
        .ok_or_else(|| {
            AppError::Message(format!(
                "SshHostKeyFingerprintMismatch: {}:{} is no longer offering the confirmed fingerprint; refusing to trust it",
                target.host, target.port
            ))
        })?;
    append_known_hosts_line(data_root, &matched.key_line)?;
    let mut store = load_store(data_root)?;
    store.confirmed.insert(
        target.store_key(),
        ConfirmedRecord {
            host: target.host.clone(),
            port: target.port,
            key_type: matched.key_type,
            fingerprint: matched.fingerprint,
            confirmed_at: Utc::now(),
        },
    );
    save_store(data_root, &store)
}

fn append_known_hosts_line(data_root: &Path, line: &str) -> AppResult<()> {
    let path = known_hosts_path(data_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| AppError::Message(error.to_string()))?;
    }
    let mut contents = fs::read_to_string(&path).unwrap_or_default();
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(line);
    contents.push('\n');
    fs::write(&path, contents).map_err(|error| AppError::Message(error.to_string()))
}

fn resolved_target_for_host(state: &AppState, host_id: &str) -> AppResult<SshTarget> {
    let resolved_agent_target = crate::remote::RemoteHostManager::resolve_target(state, host_id)?;
    let destination = resolved_agent_target
        .ssh_destination
        .ok_or_else(|| AppError::Message("host has no SSH destination".into()))?;
    resolve_ssh_target(&destination)
}

/// Fetch (without trusting) the resolved host+port and confirmation state
/// for a cached host, so the frontend can decide whether to show the
/// confirmation prompt at all.
#[tauri::command]
pub async fn remote_ssh_trust_status(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<SshTrustStatus> {
    let owned = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let target = resolved_target_for_host(&owned, &host_id)?;
        let confirmed = is_confirmed(&owned.data_root(), &target)?;
        Ok(SshTrustStatus {
            host: target.host,
            port: target.port,
            confirmed,
        })
    })
    .await
    .map_err(|error| AppError::Message(format!("SSH trust status task failed: {error}")))?
}

/// Fetch the host's currently offered key fingerprint without trusting it.
#[tauri::command]
pub async fn remote_ssh_fetch_fingerprint(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<PendingHostFingerprint> {
    let owned = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let target = resolved_target_for_host(&owned, &host_id)?;
        let candidates = fetch_pending_fingerprints(&target)?;
        preferred_fingerprint(candidates)
            .ok_or_else(|| AppError::Message("no SSH host key offered".into()))
    })
    .await
    .map_err(|error| AppError::Message(format!("SSH fingerprint task failed: {error}")))?
}

/// Record the user's explicit confirmation of `fingerprint` and append the
/// matching key to Vellum's known_hosts file. Must only be called from a
/// user-initiated action (a button click), never automatically.
#[tauri::command]
pub async fn remote_ssh_confirm_fingerprint(
    state: tauri::State<'_, AppState>,
    host_id: String,
    fingerprint: String,
) -> AppResult<()> {
    let owned = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let target = resolved_target_for_host(&owned, &host_id)?;
        confirm_and_trust(&owned.data_root(), &target, &fingerprint)
    })
    .await
    .map_err(|error| AppError::Message(format!("SSH trust confirmation task failed: {error}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cross-checked against a real, locally generated ed25519 key with
    /// `ssh-keygen -lf`: `256 SHA256:fWGOBFJCuLczHBhYs/jCgMIJ/NUUnYJmt5/owwnZAc4 hostexample.test (ED25519)`.
    const GOLDEN_KEY_BASE64: &str =
        "AAAAC3NzaC1lZDI1NTE5AAAAIBaSZz5FL6QOOBF+KXsnX3YHy7uwQ+DdzngG/JSpVjOQ";
    const GOLDEN_FINGERPRINT: &str = "SHA256:fWGOBFJCuLczHBhYs/jCgMIJ/NUUnYJmt5/owwnZAc4";

    #[test]
    fn fingerprint_matches_ssh_keygen_output_for_a_known_key() {
        assert_eq!(
            fingerprint_of(GOLDEN_KEY_BASE64).unwrap(),
            GOLDEN_FINGERPRINT
        );
    }

    #[test]
    fn fingerprint_changes_with_the_key() {
        let other = fingerprint_of(
            "AAAAC3NzaC1lZDI1NTE5AAAAIQDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDA==",
        );
        // Not every mutated string is valid base64/a valid Ed25519 point, but
        // when it does decode it must not collide with the golden fingerprint.
        if let Ok(fingerprint) = other {
            assert_ne!(fingerprint, GOLDEN_FINGERPRINT);
        }
    }

    #[test]
    fn fingerprint_rejects_invalid_base64() {
        assert!(fingerprint_of("not-base64!!!").is_err());
    }

    #[test]
    fn store_key_is_case_insensitive_on_host_but_keys_by_port() {
        let a = SshTarget {
            host: "Jetson.example".into(),
            port: 22,
        };
        let b = SshTarget {
            host: "jetson.example".into(),
            port: 22,
        };
        let c = SshTarget {
            host: "jetson.example".into(),
            port: 2222,
        };
        assert_eq!(a.store_key(), b.store_key());
        assert_ne!(a.store_key(), c.store_key());
    }

    #[test]
    fn known_hosts_pattern_brackets_non_default_ports() {
        let default_port = SshTarget {
            host: "10.0.0.2".into(),
            port: 22,
        };
        let custom_port = SshTarget {
            host: "10.0.0.2".into(),
            port: 2222,
        };
        assert_eq!(default_port.known_hosts_pattern(), "10.0.0.2");
        assert_eq!(custom_port.known_hosts_pattern(), "[10.0.0.2]:2222");
    }

    #[test]
    fn preferred_fingerprint_favors_ed25519_over_rsa() {
        let rsa = PendingHostFingerprint {
            host: "h".into(),
            port: 22,
            key_type: "ssh-rsa".into(),
            fingerprint: "SHA256:rsa".into(),
            key_line: String::new(),
        };
        let ed25519 = PendingHostFingerprint {
            host: "h".into(),
            port: 22,
            key_type: "ssh-ed25519".into(),
            fingerprint: "SHA256:ed25519".into(),
            key_line: String::new(),
        };
        let picked = preferred_fingerprint(vec![rsa, ed25519.clone()]).unwrap();
        assert_eq!(picked, ed25519);
    }

    #[test]
    fn keyscan_parser_accepts_a_fallback_tools_valid_host_key() {
        let target = SshTarget {
            host: "192.0.2.10".into(),
            port: 22,
        };
        let stdout =
            format!("# banner from the server\n192.0.2.10 ssh-ed25519 {GOLDEN_KEY_BASE64}\n");

        let parsed = parse_keyscan_output(&target, stdout.as_bytes());

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].fingerprint, GOLDEN_FINGERPRINT);
        assert_eq!(parsed[0].key_type, "ssh-ed25519");
        assert_eq!(
            parsed[0].key_line,
            format!("192.0.2.10 ssh-ed25519 {GOLDEN_KEY_BASE64}")
        );
    }

    #[test]
    fn malformed_primary_keyscan_output_does_not_become_trusted_data() {
        let target = SshTarget {
            host: "192.0.2.10".into(),
            port: 22,
        };
        let stdout = b"# choose_kex: unsupported KEX method\nnot-a-host-key\n";

        assert!(parse_keyscan_output(&target, stdout).is_empty());
        let programs = keyscan_programs();
        assert!(programs.contains(&PathBuf::from("ssh-keyscan")));
        #[cfg(windows)]
        if programs.iter().any(|program| program.is_absolute()) {
            assert_ne!(programs[0], PathBuf::from("ssh-keyscan"));
        }
    }

    #[test]
    #[ignore = "requires VELLUM_LIVE_SSH_KEYSCAN_HOST and an explicitly selected live SSH host"]
    fn live_keyscan_fallback_observes_a_host_without_trusting_it() {
        let host = std::env::var("VELLUM_LIVE_SSH_KEYSCAN_HOST")
            .expect("set VELLUM_LIVE_SSH_KEYSCAN_HOST to the selected host or IP");
        let port = std::env::var("VELLUM_LIVE_SSH_KEYSCAN_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(DEFAULT_SSH_PORT);
        let target = SshTarget { host, port };

        let fingerprints = fetch_pending_fingerprints(&target).unwrap();

        assert!(!fingerprints.is_empty());
        assert!(fingerprints
            .iter()
            .all(|candidate| candidate.fingerprint.starts_with("SHA256:")));
    }

    #[test]
    fn a_target_is_unconfirmed_until_explicitly_confirmed() {
        let temp = tempfile::tempdir().unwrap();
        let target = SshTarget {
            host: "10.0.0.5".into(),
            port: 22,
        };
        // No legacy migration path exists in this codebase; absence of a
        // record must mean "not confirmed", not "trust by default".
        assert!(!is_confirmed(temp.path(), &target).unwrap());
        assert!(require_trust_or_error(temp.path(), &target).is_err());
    }

    #[test]
    fn confirming_writes_both_the_known_hosts_line_and_the_confirmation_record() {
        let temp = tempfile::tempdir().unwrap();
        let target = SshTarget {
            host: "10.0.0.5".into(),
            port: 2222,
        };

        // Simulate the write path directly (no real ssh-keyscan in the
        // sandbox): call the same append/store helpers confirm_and_trust
        // uses, so this stays a fast, deterministic unit test.
        append_known_hosts_line(
            temp.path(),
            "[10.0.0.5]:2222 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBaSZz5FL6QOOBF+KXsnX3YHy7uwQ+DdzngG/JSpVjOQ",
        )
        .unwrap();
        let mut store = load_store(temp.path()).unwrap();
        store.confirmed.insert(
            target.store_key(),
            ConfirmedRecord {
                host: target.host.clone(),
                port: target.port,
                key_type: "ssh-ed25519".into(),
                fingerprint: GOLDEN_FINGERPRINT.into(),
                confirmed_at: Utc::now(),
            },
        );
        save_store(temp.path(), &store).unwrap();

        assert!(is_confirmed(temp.path(), &target).unwrap());
        assert!(require_trust_or_error(temp.path(), &target).is_ok());
        let known_hosts = fs::read_to_string(known_hosts_path(temp.path())).unwrap();
        assert!(known_hosts.contains("[10.0.0.5]:2222 ssh-ed25519"));

        // A different port on the same host is a distinct, still-unconfirmed
        // entry: two aliases pointing at the same IP on different ports must
        // not be conflated.
        let other_port = SshTarget {
            host: "10.0.0.5".into(),
            port: 22,
        };
        assert!(!is_confirmed(temp.path(), &other_port).unwrap());
    }

    #[test]
    fn strict_args_never_contain_accept_new() {
        let temp = tempfile::tempdir().unwrap();
        let args = strict_host_key_args(temp.path());
        assert!(args.contains(&"StrictHostKeyChecking=yes".to_string()));
        assert!(args
            .iter()
            .any(|arg| arg.starts_with("UserKnownHostsFile=")));
        assert!(!args.iter().any(|arg| arg.contains("accept-new")));
    }

    #[test]
    fn known_hosts_path_lives_under_the_vellum_data_root_not_the_users_home() {
        let temp = tempfile::tempdir().unwrap();
        let path = known_hosts_path(temp.path());
        assert!(path.starts_with(temp.path()));
        assert_eq!(path.file_name().unwrap(), "known_hosts");
    }

    #[test]
    fn corrupt_confirmation_store_fails_closed_instead_of_silently_resetting() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("remote")).unwrap();
        fs::write(confirmations_path(temp.path()), b"not json").unwrap();
        assert!(load_store(temp.path()).is_err());
    }

    fn offered(target: &SshTarget, fingerprint: &str, key_line: &str) -> PendingHostFingerprint {
        PendingHostFingerprint {
            host: target.host.clone(),
            port: target.port,
            key_type: "ssh-ed25519".into(),
            fingerprint: fingerprint.into(),
            key_line: key_line.into(),
        }
    }

    #[test]
    fn first_confirm_records_trust_only_for_the_offered_fingerprint() {
        let temp = tempfile::tempdir().unwrap();
        let target = SshTarget {
            host: "10.0.0.5".into(),
            port: 22,
        };
        assert!(!is_confirmed(temp.path(), &target).unwrap());
        confirm_matching_fingerprint(
            temp.path(),
            &target,
            GOLDEN_FINGERPRINT,
            vec![offered(
                &target,
                GOLDEN_FINGERPRINT,
                &format!(
                    "{} ssh-ed25519 {GOLDEN_KEY_BASE64}",
                    target.known_hosts_pattern()
                ),
            )],
        )
        .unwrap();
        assert!(is_confirmed(temp.path(), &target).unwrap());
        assert!(require_trust_or_error(temp.path(), &target).is_ok());
    }

    #[test]
    fn already_trusted_target_stays_confirmed_across_reloads() {
        let temp = tempfile::tempdir().unwrap();
        let target = SshTarget {
            host: "10.0.0.5".into(),
            port: 22,
        };
        confirm_matching_fingerprint(
            temp.path(),
            &target,
            GOLDEN_FINGERPRINT,
            vec![offered(
                &target,
                GOLDEN_FINGERPRINT,
                &format!(
                    "{} ssh-ed25519 {GOLDEN_KEY_BASE64}",
                    target.known_hosts_pattern()
                ),
            )],
        )
        .unwrap();
        assert!(is_confirmed(temp.path(), &target).unwrap());
        assert!(require_trust_or_error(temp.path(), &target).is_ok());
    }

    #[test]
    fn host_key_change_hard_fails_without_auto_accept() {
        let temp = tempfile::tempdir().unwrap();
        let target = SshTarget {
            host: "10.0.0.5".into(),
            port: 22,
        };
        confirm_matching_fingerprint(
            temp.path(),
            &target,
            GOLDEN_FINGERPRINT,
            vec![offered(
                &target,
                GOLDEN_FINGERPRINT,
                &format!(
                    "{} ssh-ed25519 {GOLDEN_KEY_BASE64}",
                    target.known_hosts_pattern()
                ),
            )],
        )
        .unwrap();

        let err = confirm_matching_fingerprint(
            temp.path(),
            &target,
            GOLDEN_FINGERPRINT,
            vec![offered(
                &target,
                "SHA256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                &format!("{} ssh-ed25519 otherkey", target.known_hosts_pattern()),
            )],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("SshHostKeyFingerprintMismatch"),
            "{err}"
        );
        assert!(is_confirmed(temp.path(), &target).unwrap());
        let known_hosts = fs::read_to_string(known_hosts_path(temp.path())).unwrap();
        assert!(
            !known_hosts.contains("otherkey"),
            "a rotated key must not be written as trusted: {known_hosts}"
        );
    }
}
