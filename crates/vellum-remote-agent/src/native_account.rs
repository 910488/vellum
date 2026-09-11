//! ChatGPT account authority bridge for native remote Codex.
//!
//! Every host keeps an independent Codex OAuth grant per ChatGPT account.
//! Vellum only returns account identity and pairing state over RPC; token
//! material remains in private files on the remote host.

use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::state::AgentPaths;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
struct AccountLease {
    original_captured: bool,
    original_present: bool,
    active_account_id: Option<String>,
    pending_account_id: Option<String>,
    pending_login_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NativeAccountStatus {
    pub account_id: Option<String>,
    pub expected_account_id: Option<String>,
    pub state: String,
    pub paired: bool,
    pub active: bool,
    pub login_pending: bool,
    pub login_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NativeAccountLogin {
    pub login_id: String,
    pub verification_url: String,
    pub user_code: String,
    pub expected_account_id: String,
}

#[derive(Debug)]
pub struct AccountActivationSnapshot {
    auth: Option<Vec<u8>>,
    lease: Option<Vec<u8>>,
    original_auth_existed: bool,
}

pub fn status(
    paths: &AgentPaths,
    codex_home: &Path,
    expected: Option<&str>,
) -> Result<NativeAccountStatus, String> {
    let lease = read_lease(paths)?;
    let account_id = read_account_id(&codex_home.join("auth.json"))?;
    let paired = expected.is_some_and(|id| slot_path(paths, id).is_file());
    let active = expected.is_some_and(|id| account_id.as_deref() == Some(id));
    let state = match expected {
        None => "desktopAccountUnavailable",
        Some(_) if active => "synchronized",
        Some(_) if paired => "activationRequired",
        Some(_) if lease.pending_account_id.as_deref() == expected => "pairingPending",
        Some(_) => "pairingRequired",
    };
    Ok(NativeAccountStatus {
        account_id,
        expected_account_id: expected.map(str::to_owned),
        state: state.into(),
        paired,
        active,
        login_pending: lease.pending_account_id.is_some(),
        login_id: lease.pending_login_id,
    })
}

pub fn start_login(
    paths: &AgentPaths,
    codex_home: &Path,
    expected_account_id: &str,
) -> Result<NativeAccountLogin, String> {
    validate_account_id(expected_account_id)?;
    let native = crate::native_codex::discover_native()?;
    if native.daemon_running && Path::new(&native.codex_home) == codex_home {
        let sessions = crate::native_session::query_session_status(codex_home, None)
            .map_err(|error| format!("OfficialAccountPairingObservabilityRequired: {error}"))?;
        if sessions
            .threads
            .iter()
            .any(|thread| thread.active || thread.active_turn_id.is_some())
        {
            return Err("OfficialAccountPairingBlocked: activeTurnInProgress".into());
        }
    }
    let mut lease = read_lease(paths)?;
    capture_original(paths, codex_home, &mut lease)?;
    save_active_slot(paths, codex_home, &lease)?;
    snapshot_pending_auth(paths, codex_home)?;

    let (login_id, verification_url, user_code) = start_device_code_login(codex_home)?;

    lease.pending_account_id = Some(expected_account_id.to_string());
    lease.pending_login_id = Some(login_id.clone());
    write_lease(paths, &lease)?;
    Ok(NativeAccountLogin {
        login_id,
        verification_url,
        user_code,
        expected_account_id: expected_account_id.to_string(),
    })
}

fn start_device_code_login(codex_home: &Path) -> Result<(String, String, String), String> {
    let response = crate::native_session::ws_jsonrpc_call(
        codex_home,
        "account/login/start",
        serde_json::json!({ "type": "chatgptDeviceCode" }),
    )?;
    if let Some(error) = response.get("error") {
        return Err(format!("NativeAccountPairingUnsupported: {error}"));
    }
    let result = response
        .get("result")
        .ok_or_else(|| "NativeAccountPairingUnsupported: missing result".to_string())?;
    let login_id = string_field(result, &["loginId", "id"])
        .ok_or_else(|| "NativeAccountPairingUnsupported: missing loginId".to_string())?;
    let verification_url = string_field(result, &["verificationUrl", "verificationUri", "authUrl"])
        .ok_or_else(|| "NativeAccountPairingUnsupported: missing verification URL".to_string())?;
    let user_code = string_field(result, &["userCode", "code"])
        .ok_or_else(|| "NativeAccountPairingUnsupported: missing userCode".to_string())?;
    Ok((login_id, verification_url, user_code))
}

/// Public so `mobile_account.rs` can key its catalog by the exact same
/// hash `slot_path` uses internally — the catalog never needs to see (or
/// even briefly hold) the raw account id.
pub fn hash_account_id(account_id: &str) -> String {
    hex::encode(Sha256::digest(account_id.as_bytes()))
}

/// Hash-only view of the daemon's current control identity. This is used to
/// gate Remote device pairing without ever returning the raw account id over
/// the Desktop/SSH RPC boundary.
pub fn active_account_hash(codex_home: &Path) -> Result<Option<String>, String> {
    read_account_id(&codex_home.join("auth.json"))
        .map(|account_id| account_id.map(|account_id| hash_account_id(&account_id)))
}

pub fn poll_login(paths: &AgentPaths, codex_home: &Path) -> Result<NativeAccountStatus, String> {
    let mut lease = read_lease(paths)?;
    let expected = lease
        .pending_account_id
        .clone()
        .ok_or_else(|| "NativeAccountPairingNotPending".to_string())?;
    let observed = read_account_id(&codex_home.join("auth.json"))?;
    match observed.as_deref() {
        Some(account_id) if account_id == expected => {
            copy_private(&codex_home.join("auth.json"), &slot_path(paths, &expected))?;
            lease.active_account_id = Some(expected.clone());
            lease.pending_account_id = None;
            lease.pending_login_id = None;
            write_lease(paths, &lease)?;
            remove_if_exists(&pending_auth_path(paths))?;
            status(paths, codex_home, Some(&expected))
        }
        Some(account_id) if pending_auth_changed(paths, codex_home)? => {
            restore_pending_auth(paths, codex_home)?;
            lease.pending_account_id = None;
            lease.pending_login_id = None;
            write_lease(paths, &lease)?;
            Err(format!(
                "OfficialAccountMismatch: expected {expected}, authenticated {account_id}"
            ))
        }
        _ => status(paths, codex_home, Some(&expected)),
    }
}

pub fn activate(
    paths: &AgentPaths,
    codex_home: &Path,
    account_id: &str,
) -> Result<NativeAccountStatus, String> {
    validate_account_id(account_id)?;
    let native = crate::native_codex::discover_native()?;
    if native.daemon_running && Path::new(&native.codex_home) == codex_home {
        if !native.restart_safe {
            return Err(
                "OfficialAccountSwitchBlocked: nativeDaemonAppOwned; stop it explicitly first"
                    .into(),
            );
        }
        let sessions = crate::native_session::query_session_status(codex_home, None)
            .map_err(|error| format!("OfficialAccountSwitchObservabilityRequired: {error}"))?;
        if sessions
            .threads
            .iter()
            .any(|thread| thread.active || thread.active_turn_id.is_some())
        {
            return Err("OfficialAccountSwitchBlocked: activeTurnInProgress".into());
        }
    }
    let auth = codex_home.join("auth.json");
    let current_account_id = read_account_id(&auth)?;
    let slot = slot_path(paths, account_id);
    // The host's pre-Vellum grant is already a valid independent grant. When
    // it is the requested account, adopt it without another device login. If
    // switching to an existing slot, archive the current grant first so a
    // later Windows account switch can round-trip back to it.
    if !slot.is_file() && current_account_id.as_deref() != Some(account_id) {
        return Err(format!(
            "OfficialAccountPairingRequired: account {account_id} is not paired on this host"
        ));
    }
    let mut lease = read_lease(paths)?;
    capture_original(paths, codex_home, &mut lease)?;
    if let Some(current_account_id) = current_account_id.as_deref() {
        validate_account_id(current_account_id)?;
        copy_private(&auth, &slot_path(paths, current_account_id))?;
        lease.active_account_id = Some(current_account_id.to_string());
        write_lease(paths, &lease)?;
    }
    save_active_slot(paths, codex_home, &lease)?;
    copy_private(&slot, &codex_home.join("auth.json"))?;
    let observed = read_account_id(&codex_home.join("auth.json"))?;
    if observed.as_deref() != Some(account_id) {
        return Err("OfficialAccountSlotCorrupt: account identity mismatch".into());
    }
    lease.active_account_id = Some(account_id.to_string());
    lease.pending_account_id = None;
    lease.pending_login_id = None;
    write_lease(paths, &lease)?;
    status(paths, codex_home, Some(account_id))
}

pub fn activation_snapshot(
    paths: &AgentPaths,
    codex_home: &Path,
) -> Result<AccountActivationSnapshot, String> {
    Ok(AccountActivationSnapshot {
        auth: read_optional(&codex_home.join("auth.json"))?,
        lease: read_optional(&lease_path(paths))?,
        original_auth_existed: original_auth_path(paths).is_file(),
    })
}

pub fn rollback_activation(
    paths: &AgentPaths,
    codex_home: &Path,
    snapshot: AccountActivationSnapshot,
) -> Result<(), String> {
    restore_optional(&codex_home.join("auth.json"), snapshot.auth.as_deref())?;
    restore_optional(&lease_path(paths), snapshot.lease.as_deref())?;
    if !snapshot.original_auth_existed && snapshot.lease.is_none() {
        remove_if_exists(&original_auth_path(paths))?;
    }
    Ok(())
}

pub fn restore(paths: &AgentPaths, codex_home: &Path) -> Result<Value, String> {
    let lease = read_lease(paths)?;
    if !lease.original_captured {
        return Ok(serde_json::json!({"restored": false, "reason": "notManaged"}));
    }
    save_active_slot(paths, codex_home, &lease)?;
    let auth = codex_home.join("auth.json");
    if lease.original_present {
        copy_private(&original_auth_path(paths), &auth)?;
    } else {
        remove_if_exists(&auth)?;
    }
    remove_if_exists(&lease_path(paths))?;
    remove_if_exists(&original_auth_path(paths))?;
    remove_if_exists(&pending_auth_path(paths))?;
    Ok(serde_json::json!({"restored": true}))
}

fn read_account_id(path: &Path) -> Result<Option<String>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let raw = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let value: Value = serde_json::from_slice(&raw)
        .map_err(|error| format!("invalid Codex auth.json: {error}"))?;
    Ok(value
        .pointer("/tokens/account_id")
        .or_else(|| value.pointer("/tokens/accountId"))
        .and_then(Value::as_str)
        .map(str::to_owned))
}

fn capture_original(
    paths: &AgentPaths,
    codex_home: &Path,
    lease: &mut AccountLease,
) -> Result<(), String> {
    if lease.original_captured {
        return Ok(());
    }
    let auth = codex_home.join("auth.json");
    lease.original_captured = true;
    lease.original_present = auth.is_file();
    if lease.original_present {
        copy_private(&auth, &original_auth_path(paths))?;
    }
    write_lease(paths, lease)
}

fn save_active_slot(
    paths: &AgentPaths,
    codex_home: &Path,
    lease: &AccountLease,
) -> Result<(), String> {
    if let Some(active) = &lease.active_account_id {
        let auth = codex_home.join("auth.json");
        if read_account_id(&auth)?.as_deref() == Some(active) {
            copy_private(&auth, &slot_path(paths, active))?;
        }
    }
    Ok(())
}

fn snapshot_pending_auth(paths: &AgentPaths, codex_home: &Path) -> Result<(), String> {
    let auth = codex_home.join("auth.json");
    if auth.is_file() {
        copy_private(&auth, &pending_auth_path(paths))
    } else {
        remove_if_exists(&pending_auth_path(paths))
    }
}

fn pending_auth_changed(paths: &AgentPaths, codex_home: &Path) -> Result<bool, String> {
    let auth = codex_home.join("auth.json");
    let pending = pending_auth_path(paths);
    let current = fs::read(auth).unwrap_or_default();
    let previous = fs::read(pending).unwrap_or_default();
    Ok(current != previous)
}

fn restore_pending_auth(paths: &AgentPaths, codex_home: &Path) -> Result<(), String> {
    let pending = pending_auth_path(paths);
    let auth = codex_home.join("auth.json");
    if pending.is_file() {
        copy_private(&pending, &auth)
    } else {
        remove_if_exists(&auth)
    }
}

fn string_field(value: &Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_str))
        .map(str::to_owned)
}

fn validate_account_id(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        Err("InvalidChatGptAccountId".into())
    } else {
        Ok(())
    }
}

pub(crate) fn account_root(paths: &AgentPaths) -> &Path {
    &paths.codex_accounts_dir
}

fn lease_path(paths: &AgentPaths) -> PathBuf {
    account_root(paths).join("lease.json")
}

fn original_auth_path(paths: &AgentPaths) -> PathBuf {
    account_root(paths).join("original-auth.json")
}

fn pending_auth_path(paths: &AgentPaths) -> PathBuf {
    account_root(paths).join("pending-auth.json")
}

fn slot_path(paths: &AgentPaths, account_id: &str) -> PathBuf {
    account_root(paths)
        .join("slots")
        .join(format!("{}.json", hash_account_id(account_id)))
}

fn read_lease(paths: &AgentPaths) -> Result<AccountLease, String> {
    let path = lease_path(paths);
    if !path.is_file() {
        return Ok(AccountLease::default());
    }
    serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
        .map_err(|error| format!("invalid ChatGPT account lease: {error}"))
}

fn write_lease(paths: &AgentPaths, lease: &AccountLease) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(lease).map_err(|error| error.to_string())?;
    atomic_private_write(&lease_path(paths), &bytes)
}

pub(crate) fn copy_private(source: &Path, target: &Path) -> Result<(), String> {
    let bytes = fs::read(source).map_err(|error| format!("read {}: {error}", source.display()))?;
    atomic_private_write(target, &bytes)
}

pub(crate) fn atomic_private_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
        let tmp = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|error| error.to_string())?;
        file.write_all(bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::rename(&tmp, path).map_err(|error| error.to_string())?;
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        let tmp = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
        fs::write(&tmp, bytes).map_err(|error| error.to_string())?;
        // Windows does not replace an existing file with rename(2)-style
        // semantics. The temp file is still fully written before the short
        // replacement window, and all readers either see the old or new JSON.
        if path.exists() {
            fs::remove_file(path).map_err(|error| error.to_string())?;
        }
        fs::rename(&tmp, path).map_err(|error| error.to_string())
    }
}

fn remove_if_exists(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn restore_optional(path: &Path, bytes: Option<&[u8]>) -> Result<(), String> {
    match bytes {
        Some(bytes) => atomic_private_write(path, bytes),
        None => remove_if_exists(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn auth(account: &str, secret: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "tokens": {"account_id": account, "refresh_token": secret}
        }))
        .unwrap()
    }

    #[test]
    fn status_never_serializes_token_material() {
        let temp = tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        paths.ensure().unwrap();
        let home = temp.path().join("codex");
        fs::create_dir_all(&home).unwrap();
        atomic_private_write(&home.join("auth.json"), &auth("acct-1", "secret")).unwrap();
        let encoded =
            serde_json::to_string(&status(&paths, &home, Some("acct-1")).unwrap()).unwrap();
        assert!(encoded.contains("acct-1"));
        assert!(!encoded.contains("secret"));
        assert!(!encoded.contains("refresh_token"));
    }

    #[test]
    fn activate_archives_rotated_slot_and_restore_recovers_original() {
        let temp = tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        paths.ensure().unwrap();
        let home = temp.path().join("codex");
        fs::create_dir_all(&home).unwrap();
        atomic_private_write(
            &home.join("auth.json"),
            &auth("original", "original-secret"),
        )
        .unwrap();
        let mut lease = AccountLease::default();
        capture_original(&paths, &home, &mut lease).unwrap();
        atomic_private_write(&slot_path(&paths, "acct-1"), &auth("acct-1", "one")).unwrap();
        atomic_private_write(&slot_path(&paths, "acct-2"), &auth("acct-2", "two")).unwrap();
        activate(&paths, &home, "acct-1").unwrap();
        atomic_private_write(&home.join("auth.json"), &auth("acct-1", "one-rotated")).unwrap();
        activate(&paths, &home, "acct-2").unwrap();
        assert!(fs::read_to_string(slot_path(&paths, "acct-1"))
            .unwrap()
            .contains("one-rotated"));
        restore(&paths, &home).unwrap();
        let restored = fs::read_to_string(home.join("auth.json")).unwrap();
        assert!(restored.contains("original-secret"));
    }

    #[test]
    fn switching_away_archives_the_original_grant_for_account_round_trips() {
        let temp = tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        paths.ensure().unwrap();
        let home = temp.path().join("codex");
        fs::create_dir_all(&home).unwrap();
        atomic_private_write(
            &home.join("auth.json"),
            &auth("original", "original-secret"),
        )
        .unwrap();
        atomic_private_write(&slot_path(&paths, "other"), &auth("other", "other-secret")).unwrap();

        activate(&paths, &home, "other").unwrap();
        assert!(slot_path(&paths, "original").is_file());
        let returned = activate(&paths, &home, "original").unwrap();
        assert_eq!(returned.account_id.as_deref(), Some("original"));
        assert!(returned.active);
        assert!(returned.paired);
        assert!(fs::read_to_string(home.join("auth.json"))
            .unwrap()
            .contains("original-secret"));
    }

    #[test]
    fn failed_restart_can_roll_back_auth_and_lease_atomically() {
        let temp = tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        paths.ensure().unwrap();
        let home = temp.path().join("codex");
        fs::create_dir_all(&home).unwrap();
        atomic_private_write(
            &home.join("auth.json"),
            &auth("original", "original-secret"),
        )
        .unwrap();
        atomic_private_write(&slot_path(&paths, "acct-1"), &auth("acct-1", "one")).unwrap();
        let snapshot = activation_snapshot(&paths, &home).unwrap();
        activate(&paths, &home, "acct-1").unwrap();
        rollback_activation(&paths, &home, snapshot).unwrap();
        assert!(fs::read_to_string(home.join("auth.json"))
            .unwrap()
            .contains("original-secret"));
        assert!(!lease_path(&paths).exists());
        assert!(!original_auth_path(&paths).exists());
    }
}
