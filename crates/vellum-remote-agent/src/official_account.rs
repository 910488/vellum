//! Remote proxy Official execution-account lifecycle.
//!
//! This is intentionally separate from `native_account`: native account A is
//! the Codex daemon/Remote-control identity, while grants managed here are
//! used only by the Vellum proxy for outbound Official authorization. No
//! function in this module reads or writes `$CODEX_HOME/auth.json`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use vellum_proxy_runtime::{write_grant_atomic, FileOfficialGrant};

use crate::native_account::atomic_private_write;
use crate::state::AgentPaths;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEVICE_START_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_POLL_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
const REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const USER_AGENT: &str = "vellum-remote-official-account";
const DEFAULT_EXPIRES_IN: u64 = 900;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Catalog {
    #[serde(default)]
    entries: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogEntry {
    account_id_hash: String,
    display_name: String,
    authenticated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingLogin {
    login_id: String,
    device_auth_id: String,
    user_code: String,
    display_name: String,
    expires_at_ms: i64,
    interval: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OfficialAccountView {
    pub account_id_hash: String,
    pub display_name: String,
    pub authenticated_at: String,
    pub selected: bool,
    #[serde(default)]
    pub selection_revision: u64,
    #[serde(default)]
    pub selection_verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OfficialAccountLogin {
    pub login_id: String,
    pub user_code: String,
    pub verification_url: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OfficialAccountPoll {
    pub state: String,
    pub account_id_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceStartResponse {
    device_auth_id: String,
    user_code: String,
    expires_in: Option<u64>,
    interval: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct DevicePollResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SelectedState {
    #[serde(default)]
    version: u32,
    account_id_hash: String,
    #[serde(default)]
    selection_revision: u64,
    #[serde(default)]
    selected_at: Option<i64>,
    #[serde(default = "legacy_selection_verified")]
    selection_verified: bool,
}

fn legacy_selection_verified() -> bool {
    true
}

pub fn login_start(paths: &AgentPaths, display_name: &str) -> Result<OfficialAccountLogin, String> {
    let display_name = display_name.trim();
    if display_name.is_empty() || display_name.chars().count() > 128 {
        return Err("InvalidOfficialAccountDisplayName".into());
    }
    prepare(paths)?;
    let client = client()?;
    let response = client
        .post(DEVICE_START_URL)
        .header("User-Agent", USER_AGENT)
        .json(&serde_json::json!({"client_id": CLIENT_ID}))
        .send()
        .map_err(|error| format!("OfficialAccountLoginUnavailable: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("OfficialAccountLoginFailed: HTTP {status}"));
    }
    let response: DeviceStartResponse = response
        .json()
        .map_err(|error| format!("OfficialAccountLoginProtocol: {error}"))?;
    let expires_in = response.expires_in.unwrap_or(DEFAULT_EXPIRES_IN);
    let interval = parse_interval(response.interval.as_ref());
    let login_id = ulid::Ulid::new().to_string();
    let pending = PendingLogin {
        login_id: login_id.clone(),
        device_auth_id: response.device_auth_id,
        user_code: response.user_code.clone(),
        display_name: display_name.to_string(),
        expires_at_ms: chrono::Utc::now().timestamp_millis() + expires_in as i64 * 1_000,
        interval,
    };
    write_private_json(&pending_path(paths, &login_id), &pending)?;
    Ok(OfficialAccountLogin {
        login_id,
        user_code: response.user_code,
        verification_url: VERIFICATION_URL.into(),
        expires_in,
        interval,
    })
}

pub fn login_poll(paths: &AgentPaths, login_id: &str) -> Result<OfficialAccountPoll, String> {
    validate_login_id(login_id)?;
    let pending_path = pending_path(paths, login_id);
    let pending: PendingLogin = read_json(&pending_path, "Official pending login")?;
    if pending.login_id != login_id {
        return Err("OfficialAccountLoginIdentityMismatch".into());
    }
    if pending.expires_at_ms <= chrono::Utc::now().timestamp_millis() {
        remove_if_exists(&pending_path)?;
        return Ok(OfficialAccountPoll {
            state: "expired".into(),
            account_id_hash: None,
        });
    }

    let client = client()?;
    let response = client
        .post(DEVICE_POLL_URL)
        .header("User-Agent", USER_AGENT)
        .json(&serde_json::json!({
            "device_auth_id": pending.device_auth_id,
            "user_code": pending.user_code,
        }))
        .send()
        .map_err(|error| format!("OfficialAccountLoginUnavailable: {error}"))?;
    let status = response.status();
    if matches!(
        status,
        reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::NOT_FOUND
    ) {
        return Ok(OfficialAccountPoll {
            state: "pending".into(),
            account_id_hash: None,
        });
    }
    if status == reqwest::StatusCode::GONE {
        remove_if_exists(&pending_path)?;
        return Ok(OfficialAccountPoll {
            state: "expired".into(),
            account_id_hash: None,
        });
    }
    if !status.is_success() {
        return Err(format!("OfficialAccountLoginFailed: HTTP {status}"));
    }
    let polled: DevicePollResponse = response
        .json()
        .map_err(|error| format!("OfficialAccountLoginProtocol: {error}"))?;
    let token_response = client
        .post(TOKEN_URL)
        .header("User-Agent", USER_AGENT)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", polled.authorization_code.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", CLIENT_ID),
            ("code_verifier", polled.code_verifier.as_str()),
        ])
        .send()
        .map_err(|error| format!("OfficialAccountTokenExchangeUnavailable: {error}"))?;
    let token_status = token_response.status();
    if !token_status.is_success() {
        return Err(format!(
            "OfficialAccountTokenExchangeFailed: HTTP {token_status}"
        ));
    }
    let tokens: TokenResponse = token_response
        .json()
        .map_err(|error| format!("OfficialAccountTokenProtocol: {error}"))?;
    let identity = extract_account_identity(&tokens)?;
    let refresh_token = tokens
        .refresh_token
        .ok_or_else(|| "OfficialAccountTokenProtocol: missing refresh token".to_string())?;
    let account_id_hash = hash_account_id(&identity.credential_id);
    let grant = FileOfficialGrant {
        credential_id: Some(identity.credential_id),
        account_id: identity.workspace_id,
        access_token: tokens.access_token,
        refresh_token,
        expires_at_ms: chrono::Utc::now().timestamp_millis()
            + tokens.expires_in.unwrap_or(3_600).max(1) * 1_000,
    };
    write_grant_atomic(&grant_path(paths, &account_id_hash), &grant)?;

    let mut catalog = read_catalog(paths)?;
    let authenticated_at = chrono::Utc::now().to_rfc3339();
    if let Some(entry) = catalog
        .entries
        .iter_mut()
        .find(|entry| entry.account_id_hash == account_id_hash)
    {
        entry.display_name = pending.display_name;
        entry.authenticated_at = authenticated_at;
    } else {
        catalog.entries.push(CatalogEntry {
            account_id_hash: account_id_hash.clone(),
            display_name: pending.display_name,
            authenticated_at,
        });
    }
    write_catalog(paths, &catalog)?;
    remove_if_exists(&pending_path)?;
    Ok(OfficialAccountPoll {
        state: "authenticated".into(),
        account_id_hash: Some(account_id_hash),
    })
}

pub fn list(paths: &AgentPaths) -> Result<Vec<OfficialAccountView>, String> {
    let catalog = read_catalog(paths)?;
    // selected.json is the proxy's source of truth. Deriving the UI state
    // from the same pointer prevents a crash between the two atomic file
    // replacements from showing a stale account as selected.
    let mut selected = read_selected_state(paths)?;
    if let Some(state) = selected.as_mut().filter(|state| state.version == 0) {
        if grant_path(paths, &state.account_id_hash).is_file() {
            state.version = 1;
            state
                .selected_at
                .get_or_insert(chrono::Utc::now().timestamp_millis());
            write_selected(paths, state)?;
        }
    }
    Ok(catalog
        .entries
        .into_iter()
        .filter(|entry| grant_path(paths, &entry.account_id_hash).is_file())
        .map(|entry| OfficialAccountView {
            selected: selected
                .as_ref()
                .is_some_and(|state| state.account_id_hash == entry.account_id_hash),
            selection_revision: selected
                .as_ref()
                .filter(|state| state.account_id_hash == entry.account_id_hash)
                .map_or(0, |state| state.selection_revision),
            selection_verified: selected
                .as_ref()
                .filter(|state| state.account_id_hash == entry.account_id_hash)
                .is_some_and(|state| state.selection_verified),
            account_id_hash: entry.account_id_hash,
            display_name: entry.display_name,
            authenticated_at: entry.authenticated_at,
        })
        .collect())
}

pub fn select(
    paths: &AgentPaths,
    account_id_hash: &str,
) -> Result<Vec<OfficialAccountView>, String> {
    let _selection_guard = selection_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    validate_account_hash(account_id_hash)?;
    let grant_path = grant_path(paths, account_id_hash);
    if !grant_path.is_file() {
        return Err("OfficialExecutionAccountNotFound".into());
    }
    let grant: FileOfficialGrant = read_json(&grant_path, "Official execution grant")?;
    if hash_account_id(grant_identity_id(&grant)) != account_id_hash {
        return Err("OfficialAccountIdentityMismatch".into());
    }
    if !grant_token_identity_matches(&grant) {
        return Err("OfficialAccountAuthenticationFailed: access token identity differs".into());
    }
    let catalog = read_catalog(paths)?;
    if !catalog
        .entries
        .iter()
        .any(|entry| entry.account_id_hash == account_id_hash)
    {
        return Err("OfficialExecutionAccountNotFound".into());
    }
    let previous = read_selected_state(paths)?;
    let next = SelectedState {
        version: 1,
        account_id_hash: account_id_hash.to_string(),
        selection_revision: previous
            .as_ref()
            .map_or(1, |state| state.selection_revision.saturating_add(1)),
        selected_at: Some(chrono::Utc::now().timestamp_millis()),
        selection_verified: true,
    };
    write_selected(paths, &next)?;
    list(paths)
}

/// Return Official model requests to the control account carried by native
/// Codex. The managed grants remain available for a later selection.
pub fn clear_selection(paths: &AgentPaths) -> Result<Vec<OfficialAccountView>, String> {
    let _selection_guard = selection_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    remove_if_exists(&selected_path(paths))?;
    list(paths)
}

fn selection_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub fn remove(
    paths: &AgentPaths,
    account_id_hash: &str,
) -> Result<Vec<OfficialAccountView>, String> {
    validate_account_hash(account_id_hash)?;
    remove_if_exists(&grant_path(paths, account_id_hash))?;
    let mut catalog = read_catalog(paths)?;
    catalog
        .entries
        .retain(|entry| entry.account_id_hash != account_id_hash);
    if read_selected_state(paths)?
        .as_ref()
        .is_some_and(|state| state.account_id_hash == account_id_hash)
    {
        remove_if_exists(&selected_path(paths))?;
    }
    write_catalog(paths, &catalog)?;
    list(paths)
}

pub fn selected_hash(paths: &AgentPaths) -> Result<Option<String>, String> {
    Ok(read_selected_state(paths)?.map(|state| state.account_id_hash))
}

/// Whether the stable `official-selected` credential reference can authorize.
/// No selection is valid: in that state the proxy preserves the control
/// account authorization already present on the Codex request.
pub fn selected_credential_ready(paths: &AgentPaths) -> bool {
    let Ok(selected) = read_selected_state(paths) else {
        return false;
    };
    let Some(selected) = selected else {
        return true;
    };
    if !selected.selection_verified {
        return false;
    }
    let path = grant_path(paths, &selected.account_id_hash);
    let Ok(grant) = read_json::<FileOfficialGrant>(&path, "Official execution grant") else {
        return false;
    };
    hash_account_id(grant_identity_id(&grant)) == selected.account_id_hash
        && grant_token_identity_matches(&grant)
}

fn prepare(paths: &AgentPaths) -> Result<(), String> {
    for path in [
        paths.official_accounts_dir.clone(),
        paths.official_accounts_dir.join("grants"),
        paths.official_accounts_dir.join("pending"),
    ] {
        fs::create_dir_all(&path)
            .map_err(|error| format!("create Official account state: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("secure Official account state: {error}"))?;
        }
    }
    Ok(())
}

fn client() -> Result<Client, String> {
    Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|error| format!("build Official OAuth client: {error}"))
}

fn catalog_path(paths: &AgentPaths) -> PathBuf {
    paths.official_accounts_dir.join("catalog.json")
}

fn selected_path(paths: &AgentPaths) -> PathBuf {
    paths.official_accounts_dir.join("selected.json")
}

fn pending_path(paths: &AgentPaths, login_id: &str) -> PathBuf {
    paths
        .official_accounts_dir
        .join("pending")
        .join(format!("{login_id}.json"))
}

fn grant_path(paths: &AgentPaths, account_id_hash: &str) -> PathBuf {
    paths
        .official_accounts_dir
        .join("grants")
        .join(format!("{account_id_hash}.json"))
}

fn read_catalog(paths: &AgentPaths) -> Result<Catalog, String> {
    let path = catalog_path(paths);
    if !path.is_file() {
        return Ok(Catalog::default());
    }
    read_json(&path, "Official account catalog")
}

fn write_catalog(paths: &AgentPaths, catalog: &Catalog) -> Result<(), String> {
    prepare(paths)?;
    write_private_json(&catalog_path(paths), catalog)
}

fn write_selected(paths: &AgentPaths, selected: &SelectedState) -> Result<(), String> {
    write_private_json(&selected_path(paths), selected)
}

fn read_selected_state(paths: &AgentPaths) -> Result<Option<SelectedState>, String> {
    let path = selected_path(paths);
    if !path.is_file() {
        return Ok(None);
    }
    let selected: Value = read_json(&path, "Official selected account")?;
    let account_id_hash = selected
        .get("accountIdHash")
        .and_then(Value::as_str)
        .ok_or_else(|| "invalid Official selected account: missing accountIdHash".to_string())?;
    validate_account_hash(account_id_hash)?;
    Ok(Some(SelectedState {
        version: selected.get("version").and_then(Value::as_u64).unwrap_or(0) as u32,
        account_id_hash: account_id_hash.to_string(),
        selection_revision: selected
            .get("selectionRevision")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        selected_at: selected.get("selectedAt").and_then(Value::as_i64),
        selection_verified: selected
            .get("selectionVerified")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    }))
}

fn write_private_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    atomic_private_write(path, &bytes)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path, label: &str) -> Result<T, String> {
    serde_json::from_slice(&fs::read(path).map_err(|error| format!("read {label}: {error}"))?)
        .map_err(|error| format!("invalid {label}: {error}"))
}

fn remove_if_exists(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove Official account state: {error}")),
    }
}

fn hash_account_id(account_id: &str) -> String {
    hex::encode(Sha256::digest(account_id.as_bytes()))
}

fn grant_identity_id(grant: &FileOfficialGrant) -> &str {
    grant.credential_id.as_deref().unwrap_or(&grant.account_id)
}

fn grant_token_identity_matches(grant: &FileOfficialGrant) -> bool {
    if parse_account_id(&grant.access_token).as_deref() != Some(grant.account_id.as_str()) {
        return false;
    }
    grant.credential_id.as_deref().is_none_or(|expected| {
        vellum_proxy_runtime::chatgpt_identity_from_jwt(&grant.access_token)
            .is_some_and(|identity| identity.credential_id == expected)
    })
}

fn validate_account_hash(value: &str) -> Result<(), String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("InvalidOfficialAccountHash".into());
    }
    Ok(())
}

fn validate_login_id(value: &str) -> Result<(), String> {
    if value.len() != 26 || !value.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err("InvalidOfficialAccountLoginId".into());
    }
    Ok(())
}

fn parse_interval(value: Option<&Value>) -> u64 {
    let interval = match value {
        Some(Value::Number(number)) => number.as_u64().unwrap_or(5),
        Some(Value::String(value)) => value.parse().unwrap_or(5),
        _ => 5,
    };
    interval.max(1) + 3
}

fn extract_account_identity(
    tokens: &TokenResponse,
) -> Result<vellum_proxy_runtime::ChatGptIdentity, String> {
    let access =
        vellum_proxy_runtime::chatgpt_identity_from_jwt(&tokens.access_token).ok_or_else(|| {
            "OfficialAccountTokenProtocol: access token has no credential identity".to_string()
        })?;
    if let Some(id_token) = tokens.id_token.as_deref() {
        let id = vellum_proxy_runtime::chatgpt_identity_from_jwt(id_token).ok_or_else(|| {
            "OfficialAccountAuthenticationFailed: id token has no credential identity".to_string()
        })?;
        if id.workspace_id != access.workspace_id
            || !vellum_proxy_runtime::same_chatgpt_principal(&id, &access)
        {
            return Err(
                "OfficialAccountAuthenticationFailed: token account identities differ".into(),
            );
        }
    }
    Ok(access)
}

fn parse_account_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let value: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    value
        .get("chatgpt_account_id")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
                .and_then(Value::as_str)
        })
        .or_else(|| value.pointer("/organizations/0/id").and_then(Value::as_str))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt_for_account(account_id: &str) -> String {
        let payload = URL_SAFE_NO_PAD
            .encode(serde_json::json!({"chatgpt_account_id": account_id}).to_string());
        format!("header.{payload}.signature")
    }

    fn jwt_for_identity(user_id: &str, account_id: &str) -> String {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "sub": user_id,
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": account_id,
                    "chatgpt_user_id": user_id
                }
            })
            .to_string(),
        );
        format!("header.{payload}.signature")
    }

    #[test]
    fn execution_login_separates_two_users_in_one_workspace() {
        let tokens = |user: &str| TokenResponse {
            access_token: jwt_for_identity(user, "workspace-crypto"),
            refresh_token: Some(format!("refresh-{user}")),
            id_token: Some(jwt_for_identity(user, "workspace-crypto")),
            expires_in: Some(3_600),
        };
        let jp = extract_account_identity(&tokens("user-jp")).unwrap();
        let crypto = extract_account_identity(&tokens("user-crypto")).unwrap();
        assert_eq!(jp.workspace_id, crypto.workspace_id);
        assert_ne!(jp.credential_id, crypto.credential_id);
        let mismatched = FileOfficialGrant {
            credential_id: Some(jp.credential_id),
            account_id: crypto.workspace_id,
            access_token: tokens("user-crypto").access_token,
            refresh_token: "refresh".into(),
            expires_at_ms: 1,
        };
        assert!(!grant_token_identity_matches(&mismatched));
    }

    #[test]
    fn select_and_remove_never_expose_raw_account_or_token() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        paths.ensure().unwrap();
        let account_id = "acct-raw-secret";
        let hash = hash_account_id(account_id);
        write_grant_atomic(
            &grant_path(&paths, &hash),
            &FileOfficialGrant {
                credential_id: None,
                account_id: account_id.into(),
                access_token: jwt_for_account(account_id),
                refresh_token: "fixture".into(),
                expires_at_ms: i64::MAX,
            },
        )
        .unwrap();
        write_catalog(
            &paths,
            &Catalog {
                entries: vec![CatalogEntry {
                    account_id_hash: hash.clone(),
                    display_name: "Execution B".into(),
                    authenticated_at: "now".into(),
                }],
            },
        )
        .unwrap();

        let selected = select(&paths, &hash).unwrap();
        let encoded = serde_json::to_string(&selected).unwrap();
        assert!(!encoded.contains(account_id));
        assert!(!encoded.contains("access-secret"));
        assert!(!encoded.contains("refresh-secret"));
        assert!(selected[0].selected);
        let selected_state: Value =
            serde_json::from_slice(&fs::read(selected_path(&paths)).unwrap()).unwrap();
        assert_eq!(selected_state["version"], 1);
        assert_eq!(selected_state["selectionRevision"], 1);

        assert!(remove(&paths, &hash).unwrap().is_empty());
        assert!(!selected_path(&paths).exists());
    }

    #[test]
    fn clear_selection_keeps_managed_grant_available() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        paths.ensure().unwrap();
        let account_id = "acct-managed";
        let hash = hash_account_id(account_id);
        let grant = grant_path(&paths, &hash);
        write_grant_atomic(
            &grant,
            &FileOfficialGrant {
                credential_id: None,
                account_id: account_id.into(),
                access_token: jwt_for_account(account_id),
                refresh_token: "fixture".into(),
                expires_at_ms: i64::MAX,
            },
        )
        .unwrap();
        write_catalog(
            &paths,
            &Catalog {
                entries: vec![CatalogEntry {
                    account_id_hash: hash.clone(),
                    display_name: "Execution B".into(),
                    authenticated_at: "now".into(),
                }],
            },
        )
        .unwrap();

        assert!(select(&paths, &hash).unwrap()[0].selected);
        let accounts = clear_selection(&paths).unwrap();

        assert_eq!(accounts.len(), 1);
        assert!(!accounts[0].selected);
        assert!(grant.exists());
        assert!(!selected_path(&paths).exists());
    }

    #[test]
    fn account_hash_validation_fails_closed() {
        assert!(validate_account_hash("../escape").is_err());
        assert!(validate_account_hash(&"g".repeat(64)).is_err());
        assert!(validate_account_hash(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn legacy_selected_json_is_migrated_with_revision_zero() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        paths.ensure().unwrap();
        let account_id = "acct-legacy";
        let hash = hash_account_id(account_id);
        write_grant_atomic(
            &grant_path(&paths, &hash),
            &FileOfficialGrant {
                credential_id: None,
                account_id: account_id.into(),
                access_token: jwt_for_account(account_id),
                refresh_token: "refresh".into(),
                expires_at_ms: i64::MAX,
            },
        )
        .unwrap();
        write_catalog(
            &paths,
            &Catalog {
                entries: vec![CatalogEntry {
                    account_id_hash: hash.clone(),
                    display_name: "Legacy".into(),
                    authenticated_at: "now".into(),
                }],
            },
        )
        .unwrap();
        write_private_json(
            &selected_path(&paths),
            &serde_json::json!({"accountIdHash": hash}),
        )
        .unwrap();
        let listed = list(&paths).unwrap();
        assert_eq!(listed[0].selection_revision, 0);
        let migrated: Value =
            serde_json::from_slice(&fs::read(selected_path(&paths)).unwrap()).unwrap();
        assert_eq!(migrated["version"], 1);
        assert_eq!(migrated["selectionRevision"], 0);
    }
}
