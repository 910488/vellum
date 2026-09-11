//! Official (ChatGPT-plan) auth port (plan §4.3).
//!
//! Mirrors two semantics `src-tauri/src/codex_oauth.rs` already has in
//! production, which a plain "give me a token" contract would lose:
//!
//! - `valid_default_auth() -> Ok(None)` means Vellum has no managed ChatGPT
//!   account for this route — that's not an error, it's native Codex-login
//!   mode, and the proxy must preserve whatever authorization the incoming
//!   request already carries instead of injecting anything.
//! - `refresh_after_rejection(account_id, rejected_token)` compares the
//!   *rejected* token against whatever is currently cached before doing real
//!   refresh work, so a request that loses a race against another request's
//!   already-completed refresh reuses the new cached token instead of
//!   refreshing twice.
//!
//! Neither adapter (Desktop's real `codex_oauth`, the daemon's headless
//! equivalent) is wired here — this only fixes the contract.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const USER_AGENT: &str = "vellum-remote-official-auth";
const REFRESH_BUFFER_MS: i64 = 60_000;
pub const SELECTED_OFFICIAL_CREDENTIAL_ID: &str = "official-selected";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfficialAuthorization {
    pub access_token: String,
    pub account_id: Option<String>,
    pub selection_revision: Option<u64>,
    pub selection_verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfficialAuthDecision {
    /// No Vellum-managed ChatGPT account for this route: preserve the
    /// incoming request's own authorization/account headers untouched.
    PreserveIncoming,
    Managed(OfficialAuthorization),
}

#[async_trait]
pub trait OfficialAuthProvider: Send + Sync {
    async fn authorize(&self, route_id: &str) -> Result<OfficialAuthDecision, String>;

    /// Authorize against a *named* managed account instead of whichever one
    /// is currently the default. Used by the Auto Review pipeline so a
    /// review that runs on the Official plane can be billed to an account
    /// the user picked for reviewing, without moving their normal turns.
    ///
    /// The default implementation ignores `account_id` and delegates, which
    /// is the honest answer for every host that has exactly one managed
    /// grant (the headless daemon, the remote agent, the unconfigured and
    /// native-Codex providers): it cannot choose, so it does not pretend
    /// to. Callers must not assume the request was honoured -- the returned
    /// `Managed` decision carries the account that actually paid, and
    /// `ResolvedAuth::resolve` refuses the request when that is not the
    /// account that was asked for. Silently billing a different account is
    /// worse than failing.
    async fn authorize_as(
        &self,
        route_id: &str,
        _account_id: Option<&str>,
    ) -> Result<OfficialAuthDecision, String> {
        self.authorize(route_id).await
    }

    /// Called after the provider rejects a `Managed` token (401/expired).
    /// Implementations must check `rejected` against whatever is currently
    /// cached before refreshing — if another caller already refreshed past
    /// it, return the newer cached token instead of refreshing again. Must
    /// not fall back to a different auth kind — only refresh or fail.
    async fn refresh_after_rejection(
        &self,
        route_id: &str,
        rejected: &OfficialAuthorization,
    ) -> Result<OfficialAuthorization, String>;
}

/// On-disk grant shared by the remote agent's device-login boundary and the
/// headless proxy. It lives under the proxy's private writable data mount,
/// never in Codex's `CODEX_HOME`, so changing the execution/billing identity
/// cannot change or restart the Remote-control daemon identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileOfficialGrant {
    pub account_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: i64,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SelectedOfficialAccount {
    #[serde(default)]
    version: u32,
    account_id_hash: String,
    #[serde(default)]
    selection_revision: u64,
    #[serde(default)]
    selected_at: Option<i64>,
    /// Legacy selected.json files had no verification bit. The grant/account
    /// hash check is the migration-time verification for those files.
    #[serde(default = "legacy_selection_verified")]
    selection_verified: bool,
}

fn legacy_selection_verified() -> bool {
    true
}

/// Headless managed Official authentication. Routes without a credential
/// keep native passthrough; routes with a credential resolve one grant from
/// `data_dir/official-auth` and only replace authorization headers. Request
/// bodies and Responses HTTP/WebSocket events remain native passthrough.
pub struct FileManagedOfficialAuthProvider {
    root: PathBuf,
    route_credentials: HashMap<String, Option<String>>,
    refresh_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    client: reqwest::Client,
    token_url: String,
}

impl FileManagedOfficialAuthProvider {
    pub fn new(
        root: PathBuf,
        route_credentials: impl IntoIterator<Item = (String, Option<String>)>,
    ) -> Self {
        Self {
            root,
            route_credentials: route_credentials.into_iter().collect(),
            refresh_locks: Mutex::new(HashMap::new()),
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(15))
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
            token_url: TOKEN_URL.to_string(),
        }
    }

    #[cfg(test)]
    fn with_token_url(mut self, token_url: impl Into<String>) -> Self {
        self.token_url = token_url.into();
        self
    }

    fn credential_for_route(&self, route_id: &str) -> Result<Option<&str>, String> {
        self.route_credentials
            .get(route_id)
            .map(|credential| credential.as_deref())
            .ok_or_else(|| format!("unknown Official route `{route_id}`"))
    }

    fn grant_path(&self, credential_id: &str) -> Result<PathBuf, String> {
        validate_credential_id(credential_id)?;
        if credential_id == SELECTED_OFFICIAL_CREDENTIAL_ID {
            let selected_path = self.root.join("selected.json");
            let selected = read_selected_state(&selected_path)?;
            validate_account_hash(&selected.account_id_hash)?;
            return Ok(self
                .root
                .join("grants")
                .join(format!("{}.json", selected.account_id_hash)));
        }
        Ok(self
            .root
            .join("grants")
            .join(format!("{credential_id}.json")))
    }

    fn read_grant(&self, credential_id: &str) -> Result<(PathBuf, FileOfficialGrant), String> {
        let path = self.grant_path(credential_id)?;
        let grant: FileOfficialGrant = serde_json::from_slice(
            &std::fs::read(&path)
                .map_err(|error| format!("Official execution credential unavailable: {error}"))?,
        )
        .map_err(|error| format!("invalid Official execution credential: {error}"))?;
        if credential_id == SELECTED_OFFICIAL_CREDENTIAL_ID {
            let selected = read_selected_state(&self.root.join("selected.json"))?;
            let expected = hash_account_id(&grant.account_id);
            if expected != selected.account_id_hash {
                return Err("Official selected account and grant identity differ".into());
            }
        }
        if parse_jwt_account_id(&grant.access_token).as_deref() != Some(grant.account_id.as_str()) {
            return Err(
                "Official grant access-token identity is missing or differs from grant".into(),
            );
        }
        Ok((path, grant))
    }

    fn refresh_lock(&self, credential_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .refresh_locks
            .lock()
            .expect("Official refresh locks poisoned");
        Arc::clone(
            locks
                .entry(credential_id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    async fn valid_managed(&self, credential_id: &str) -> Result<OfficialAuthorization, String> {
        let (_, current) = self.read_grant(credential_id)?;
        if current.expires_at_ms - chrono::Utc::now().timestamp_millis() >= REFRESH_BUFFER_MS {
            return Ok(authorization(&current));
        }
        self.refresh(credential_id, Some(&current.access_token))
            .await
    }

    async fn refresh(
        &self,
        credential_id: &str,
        rejected_token: Option<&str>,
    ) -> Result<OfficialAuthorization, String> {
        let lock = self.refresh_lock(credential_id);
        let _guard = lock.lock().await;
        let (path, mut current) = self.read_grant(credential_id)?;
        if rejected_token.is_some_and(|rejected| rejected != current.access_token)
            && current.expires_at_ms - chrono::Utc::now().timestamp_millis() >= REFRESH_BUFFER_MS
        {
            return Ok(authorization(&current));
        }
        let response = self
            .client
            .post(&self.token_url)
            .header("User-Agent", USER_AGENT)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", current.refresh_token.as_str()),
                ("client_id", CLIENT_ID),
            ])
            .send()
            .await
            .map_err(|error| format!("Official execution credential refresh failed: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            // Never include the provider body here: OAuth error bodies may
            // echo sensitive credential material.
            return Err(format!(
                "Official execution credential refresh rejected: HTTP {status}"
            ));
        }
        let refreshed: RefreshResponse = response
            .json()
            .await
            .map_err(|error| format!("invalid Official refresh response: {error}"))?;
        validate_refresh_identity(&current.account_id, &refreshed)?;
        current.access_token = refreshed.access_token;
        if let Some(rotated) = refreshed.refresh_token {
            current.refresh_token = rotated;
        }
        current.expires_at_ms = chrono::Utc::now().timestamp_millis()
            + refreshed.expires_in.unwrap_or(3_600).max(1) * 1_000;
        write_grant_atomic(&path, &current)?;
        Ok(authorization(&current))
    }
}

#[async_trait]
impl OfficialAuthProvider for FileManagedOfficialAuthProvider {
    async fn authorize(&self, route_id: &str) -> Result<OfficialAuthDecision, String> {
        let Some(credential_id) = self.credential_for_route(route_id)? else {
            return Ok(OfficialAuthDecision::PreserveIncoming);
        };
        // Remote deployments use one stable indirection for Official routes
        // so selecting execution account B never requires rewriting proxy
        // configuration or restarting the Codex Remote daemon. With no
        // selection, account A's request-scoped authorization passes through.
        // Once selected.json exists, malformed or missing grant state fails
        // closed instead of silently billing A.
        if credential_id == SELECTED_OFFICIAL_CREDENTIAL_ID {
            let selected_path = self.root.join("selected.json");
            let selected = match std::fs::metadata(&selected_path) {
                Ok(_) => read_selected_state(&selected_path)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(OfficialAuthDecision::PreserveIncoming);
                }
                Err(error) => {
                    return Err(format!(
                        "Official selected-account state unavailable: {error}"
                    ));
                }
            };
            if !selected.selection_verified {
                return Err("Official selected-account state is not verified".into());
            }
            let mut auth = self.valid_managed(credential_id).await?;
            let current = read_selected_state(&selected_path)?;
            if current.account_id_hash != selected.account_id_hash
                || current.selection_revision != selected.selection_revision
            {
                return Err("Official account selection changed during authorization".into());
            }
            auth.selection_revision = Some(selected.selection_revision);
            auth.selection_verified = selected.selection_verified;
            if selected.version == 0 {
                let mut migrated = selected;
                migrated.version = 1;
                migrated
                    .selected_at
                    .get_or_insert(chrono::Utc::now().timestamp_millis());
                write_selected_state(&selected_path, &migrated)?;
            }
            return Ok(OfficialAuthDecision::Managed(auth));
        }
        self.valid_managed(credential_id).await.map(|mut auth| {
            auth.selection_revision = Some(0);
            auth.selection_verified = true;
            OfficialAuthDecision::Managed(auth)
        })
    }

    async fn refresh_after_rejection(
        &self,
        route_id: &str,
        rejected: &OfficialAuthorization,
    ) -> Result<OfficialAuthorization, String> {
        let credential_id = self
            .credential_for_route(route_id)?
            .ok_or_else(|| format!("route `{route_id}` uses native Codex authentication"))?;
        let mut refreshed = self
            .refresh(credential_id, Some(&rejected.access_token))
            .await?;
        if refreshed.account_id != rejected.account_id {
            return Err("Official token refresh changed the execution account".into());
        }
        refreshed.selection_revision = rejected.selection_revision;
        refreshed.selection_verified = rejected.selection_verified;
        Ok(refreshed)
    }
}

fn authorization(grant: &FileOfficialGrant) -> OfficialAuthorization {
    OfficialAuthorization {
        access_token: grant.access_token.clone(),
        account_id: Some(grant.account_id.clone()),
        selection_revision: None,
        selection_verified: true,
    }
}

fn hash_account_id(account_id: &str) -> String {
    Sha256::digest(account_id.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_refresh_identity(
    expected_account_id: &str,
    refreshed: &RefreshResponse,
) -> Result<(), String> {
    let access_account = parse_jwt_account_id(&refreshed.access_token).ok_or_else(|| {
        "Official refresh response has no verifiable access-token account identity".to_string()
    })?;
    if access_account != expected_account_id {
        return Err(
            "Official refresh response account identity differs from selected grant".into(),
        );
    }
    if let Some(id_token) = refreshed.id_token.as_deref() {
        let id_account = parse_jwt_account_id(id_token).ok_or_else(|| {
            "Official refresh id-token has no verifiable account identity".to_string()
        })?;
        if id_account != access_account {
            return Err("Official refresh id-token and access-token identities differ".into());
        }
    }
    Ok(())
}

fn parse_jwt_account_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let claims: serde_json::Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .ok()?,
    )
    .ok()?;
    claims
        .get("chatgpt_account_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            claims
                .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            claims
                .pointer("/organizations/0/id")
                .and_then(serde_json::Value::as_str)
        })
        .map(str::to_string)
}

fn read_selected_state(path: &Path) -> Result<SelectedOfficialAccount, String> {
    let value: serde_json::Value = serde_json::from_slice(
        &std::fs::read(path)
            .map_err(|error| format!("read Official selected-account state: {error}"))?,
    )
    .map_err(|error| format!("invalid Official selected-account state: {error}"))?;
    let account_id_hash = value
        .get("accountIdHash")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Official selected-account state has no accountIdHash".to_string())?;
    validate_account_hash(account_id_hash)?;
    Ok(SelectedOfficialAccount {
        version: value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32,
        account_id_hash: account_id_hash.to_string(),
        selection_revision: value
            .get("selectionRevision")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        selected_at: value.get("selectedAt").and_then(serde_json::Value::as_i64),
        selection_verified: value
            .get("selectionVerified")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
    })
}

fn write_selected_state(path: &Path, selected: &SelectedOfficialAccount) -> Result<(), String> {
    write_json_atomic(path, selected, "Official selected-account state")
}

pub fn write_grant_atomic(path: &Path, grant: &FileOfficialGrant) -> Result<(), String> {
    write_json_atomic(path, grant, "Official grant")
}

fn write_json_atomic(path: &Path, value: &impl Serialize, label: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Official grant path has no parent".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create {label} directory: {error}"))?;
    let bytes = serde_json::to_vec(value).map_err(|error| format!("encode {label}: {error}"))?;
    let tmp = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp)
        .map_err(|error| format!("create {label} temp file: {error}"))?;
    use std::io::Write as _;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("write {label}: {error}"))?;
    drop(file);
    #[cfg(windows)]
    if path.exists() {
        std::fs::remove_file(path).map_err(|error| format!("replace {label}: {error}"))?;
    }
    std::fs::rename(&tmp, path).map_err(|error| format!("replace {label}: {error}"))?;
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync Official grant directory: {error}"))?;
    Ok(())
}

fn validate_credential_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return Err("invalid Official credential id".into());
    }
    Ok(())
}

fn validate_account_hash(value: &str) -> Result<(), String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid Official account hash".into());
    }
    Ok(())
}

/// Fails closed. A host with no configured Official credential must never
/// silently authorize a request under a different auth kind. This is
/// intentionally *not* the same as Desktop's `PreserveIncoming` case: a
/// remote daemon route requiring Official with nothing provisioned is a
/// misconfiguration, not native-login mode, so it must refuse rather than
/// pass through whatever the caller sent.
pub struct UnconfiguredOfficialAuthProvider;

#[async_trait]
impl OfficialAuthProvider for UnconfiguredOfficialAuthProvider {
    async fn authorize(&self, route_id: &str) -> Result<OfficialAuthDecision, String> {
        Err(format!(
            "route `{route_id}` requires Official credentials, none configured on this host"
        ))
    }

    async fn refresh_after_rejection(
        &self,
        route_id: &str,
        _rejected: &OfficialAuthorization,
    ) -> Result<OfficialAuthorization, String> {
        Err(format!(
            "route `{route_id}` requires Official credentials, none configured on this host"
        ))
    }
}

/// Remote native Codex owns the ChatGPT credential lifecycle. The loopback
/// proxy may preserve the authorization/account headers supplied by that
/// Codex process, but it must never manufacture or persist a token itself.
pub struct NativeCodexOfficialAuthProvider;

#[async_trait]
impl OfficialAuthProvider for NativeCodexOfficialAuthProvider {
    async fn authorize(&self, _route_id: &str) -> Result<OfficialAuthDecision, String> {
        Ok(OfficialAuthDecision::PreserveIncoming)
    }

    async fn refresh_after_rejection(
        &self,
        route_id: &str,
        _rejected: &OfficialAuthorization,
    ) -> Result<OfficialAuthorization, String> {
        Err(format!(
            "route `{route_id}` uses native Codex authentication; refresh must be performed by Codex"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    #[tokio::test]
    async fn unconfigured_provider_fails_closed_instead_of_downgrading() {
        let provider = UnconfiguredOfficialAuthProvider;
        let error = provider.authorize("route-1").await.unwrap_err();
        assert!(error.contains("route-1"));

        let rejected = OfficialAuthorization {
            access_token: "expired".into(),
            account_id: Some("acct-1".into()),
            selection_revision: None,
            selection_verified: true,
        };
        assert!(provider
            .refresh_after_rejection("route-1", &rejected)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn native_codex_provider_preserves_request_scoped_auth() {
        let provider = NativeCodexOfficialAuthProvider;
        assert_eq!(
            provider.authorize("official").await.unwrap(),
            OfficialAuthDecision::PreserveIncoming
        );
    }

    /// A fake standing in for Desktop's `valid_default_auth()`/
    /// `refresh_after_rejection()` pair, proving the trait can express both
    /// real semantics: native passthrough, and refresh-dedup against a
    /// concurrently-completed refresh.
    struct FakeManagedProvider {
        current_token: std::sync::Mutex<String>,
        refresh_calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl OfficialAuthProvider for FakeManagedProvider {
        async fn authorize(&self, _route_id: &str) -> Result<OfficialAuthDecision, String> {
            Ok(OfficialAuthDecision::Managed(OfficialAuthorization {
                access_token: self.current_token.lock().unwrap().clone(),
                account_id: Some("acct-1".into()),
                selection_revision: Some(0),
                selection_verified: true,
            }))
        }

        async fn refresh_after_rejection(
            &self,
            _route_id: &str,
            rejected: &OfficialAuthorization,
        ) -> Result<OfficialAuthorization, String> {
            let mut current = self.current_token.lock().unwrap();
            if *current != rejected.access_token {
                // Someone else already refreshed past the token we were
                // rejected on; reuse it instead of refreshing again.
                return Ok(OfficialAuthorization {
                    access_token: current.clone(),
                    account_id: Some("acct-1".into()),
                    selection_revision: Some(0),
                    selection_verified: true,
                });
            }
            self.refresh_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *current = "refreshed-token".into();
            Ok(OfficialAuthorization {
                access_token: current.clone(),
                account_id: Some("acct-1".into()),
                selection_revision: Some(0),
                selection_verified: true,
            })
        }
    }

    #[tokio::test]
    async fn refresh_after_rejection_skips_a_redundant_refresh_when_another_caller_already_won() {
        let provider = FakeManagedProvider {
            current_token: std::sync::Mutex::new("stale-token".into()),
            refresh_calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let rejected = OfficialAuthorization {
            access_token: "stale-token".into(),
            account_id: Some("acct-1".into()),
            selection_revision: Some(0),
            selection_verified: true,
        };

        let first = provider
            .refresh_after_rejection("route-1", &rejected)
            .await
            .unwrap();
        assert_eq!(first.access_token, "refreshed-token");
        assert_eq!(
            provider
                .refresh_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        // A second caller rejected on the same stale token must reuse the
        // token the first caller already refreshed to, not refresh again.
        let second = provider
            .refresh_after_rejection("route-1", &rejected)
            .await
            .unwrap();
        assert_eq!(second.access_token, "refreshed-token");
        assert_eq!(
            provider
                .refresh_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a caller rejected on an already-superseded token must not trigger another refresh"
        );
    }

    #[tokio::test]
    async fn file_provider_preserves_native_auth_for_routes_without_a_credential() {
        let temp = tempfile::tempdir().unwrap();
        let provider = FileManagedOfficialAuthProvider::new(
            temp.path().to_path_buf(),
            [("official".to_string(), None)],
        );
        assert_eq!(
            provider.authorize("official").await.unwrap(),
            OfficialAuthDecision::PreserveIncoming
        );
    }

    #[tokio::test]
    async fn selected_file_resolves_an_unexpired_managed_grant() {
        let temp = tempfile::tempdir().unwrap();
        let hash = hash_account_id("acct-execution-b");
        let grant_path = temp.path().join("grants").join(format!("{hash}.json"));
        write_grant_atomic(
            &grant_path,
            &FileOfficialGrant {
                account_id: "acct-execution-b".into(),
                access_token: jwt_for_account("acct-execution-b"),
                refresh_token: "refresh-b".into(),
                expires_at_ms: chrono::Utc::now().timestamp_millis() + 600_000,
            },
        )
        .unwrap();
        std::fs::write(
            temp.path().join("selected.json"),
            serde_json::json!({"accountIdHash": hash}).to_string(),
        )
        .unwrap();
        let provider = FileManagedOfficialAuthProvider::new(
            temp.path().to_path_buf(),
            [(
                "official".to_string(),
                Some(SELECTED_OFFICIAL_CREDENTIAL_ID.to_string()),
            )],
        );

        assert_eq!(
            provider.authorize("official").await.unwrap(),
            OfficialAuthDecision::Managed(OfficialAuthorization {
                access_token: jwt_for_account("acct-execution-b"),
                account_id: Some("acct-execution-b".into()),
                selection_revision: Some(0),
                selection_verified: true,
            })
        );
    }

    #[tokio::test]
    async fn selected_indirection_without_selection_preserves_control_account_auth() {
        let temp = tempfile::tempdir().unwrap();
        let provider = FileManagedOfficialAuthProvider::new(
            temp.path().to_path_buf(),
            [(
                "official".to_string(),
                Some(SELECTED_OFFICIAL_CREDENTIAL_ID.to_string()),
            )],
        );

        assert_eq!(
            provider.authorize("official").await.unwrap(),
            OfficialAuthDecision::PreserveIncoming
        );
    }

    async fn token_server(
        status: u16,
        body: serde_json::Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::routing::post;
        use axum::Router;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/token",
            post(move || {
                let body = body.clone();
                async move {
                    (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        axum::Json(body),
                    )
                }
            }),
        );
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}/token"), handle)
    }

    fn jwt_for_account(account_id: &str) -> String {
        let payload = URL_SAFE_NO_PAD
            .encode(serde_json::json!({"chatgpt_account_id": account_id}).to_string());
        format!("header.{payload}.signature")
    }

    #[tokio::test]
    async fn selected_account_401_refresh_rotates_the_same_grant_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let hash = hash_account_id("acct-b");
        let path = temp.path().join("grants").join(format!("{hash}.json"));
        write_grant_atomic(
            &path,
            &FileOfficialGrant {
                account_id: "acct-b".into(),
                access_token: jwt_for_account("acct-b"),
                refresh_token: "old-refresh".into(),
                expires_at_ms: i64::MAX,
            },
        )
        .unwrap();
        std::fs::write(
            temp.path().join("selected.json"),
            serde_json::json!({"accountIdHash": hash}).to_string(),
        )
        .unwrap();
        let (token_url, server) = token_server(
            200,
            serde_json::json!({
                "access_token": jwt_for_account("acct-b"),
                "refresh_token": "rotated-refresh",
                "expires_in": 3600
            }),
        )
        .await;
        let provider = FileManagedOfficialAuthProvider::new(
            temp.path().to_path_buf(),
            [(
                "official".to_string(),
                Some(SELECTED_OFFICIAL_CREDENTIAL_ID.to_string()),
            )],
        )
        .with_token_url(token_url);

        let refreshed = provider
            .refresh_after_rejection(
                "official",
                &OfficialAuthorization {
                    access_token: jwt_for_account("acct-b"),
                    account_id: Some("acct-b".into()),
                    selection_revision: Some(0),
                    selection_verified: true,
                },
            )
            .await
            .unwrap();

        assert_eq!(refreshed.access_token, jwt_for_account("acct-b"));
        assert_eq!(refreshed.account_id.as_deref(), Some("acct-b"));
        let persisted: FileOfficialGrant =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(persisted.refresh_token, "rotated-refresh");
        assert_eq!(persisted.account_id, "acct-b");
        server.abort();
    }

    #[tokio::test]
    async fn selected_account_refresh_never_migrates_an_old_turn_to_a_new_account() {
        let temp = tempfile::tempdir().unwrap();
        let hash = hash_account_id("acct-b");
        write_grant_atomic(
            &temp.path().join("grants").join(format!("{hash}.json")),
            &FileOfficialGrant {
                account_id: "acct-b".into(),
                access_token: jwt_for_account("acct-b"),
                refresh_token: "refresh-b".into(),
                expires_at_ms: i64::MAX,
            },
        )
        .unwrap();
        std::fs::write(
            temp.path().join("selected.json"),
            serde_json::json!({"accountIdHash": hash}).to_string(),
        )
        .unwrap();
        let provider = FileManagedOfficialAuthProvider::new(
            temp.path().to_path_buf(),
            [(
                "official".to_string(),
                Some(SELECTED_OFFICIAL_CREDENTIAL_ID.to_string()),
            )],
        );

        let error = provider
            .refresh_after_rejection(
                "official",
                &OfficialAuthorization {
                    access_token: jwt_for_account("acct-a"),
                    account_id: Some("acct-a".into()),
                    selection_revision: Some(1),
                    selection_verified: true,
                },
            )
            .await
            .unwrap_err();
        assert!(error.contains("changed the execution account"));
    }

    #[tokio::test]
    async fn refresh_rejection_never_includes_provider_body_or_token() {
        let temp = tempfile::tempdir().unwrap();
        let hash = hash_account_id("acct-c");
        write_grant_atomic(
            &temp.path().join("grants").join(format!("{hash}.json")),
            &FileOfficialGrant {
                account_id: "acct-c".into(),
                access_token: jwt_for_account("acct-c"),
                refresh_token: "fixture".into(),
                expires_at_ms: i64::MAX,
            },
        )
        .unwrap();
        std::fs::write(
            temp.path().join("selected.json"),
            serde_json::json!({"accountIdHash": hash}).to_string(),
        )
        .unwrap();
        let (token_url, server) = token_server(
            401,
            serde_json::json!({"error": "refresh-secret echoed by provider"}),
        )
        .await;
        let provider = FileManagedOfficialAuthProvider::new(
            temp.path().to_path_buf(),
            [(
                "official".to_string(),
                Some(SELECTED_OFFICIAL_CREDENTIAL_ID.to_string()),
            )],
        )
        .with_token_url(token_url);

        let error = provider
            .refresh_after_rejection(
                "official",
                &OfficialAuthorization {
                    access_token: jwt_for_account("acct-c"),
                    account_id: Some("acct-c".into()),
                    selection_revision: Some(0),
                    selection_verified: true,
                },
            )
            .await
            .unwrap_err();

        assert!(error.contains("HTTP 401"));
        assert!(!error.contains("refresh-secret"));
        assert!(!error.contains("acct-c"));
        server.abort();
    }
}
