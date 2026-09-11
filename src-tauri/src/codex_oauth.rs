//! OpenAI Codex OAuth device-code login and automatic token refresh.
//!
//! Refresh tokens are never written to the account metadata file. They are
//! stored through Vellum's encrypted credential store (AES-GCM + DPAPI key).

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEVICE_START_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_POLL_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
const REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const USER_AGENT: &str = "vellum-codex-oauth";
const REFRESH_BUFFER_MS: i64 = 60_000;
const DEFAULT_EXPIRES_IN: u64 = 900;

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("authorization pending")]
    AuthorizationPending,
    #[error("device authorization expired")]
    Expired,
    #[error("account not found: {0}")]
    AccountNotFound(String),
    #[error("refresh token was rejected: {0}")]
    RefreshRejected(String),
    #[error("OAuth request failed: {0}")]
    Request(String),
    #[error("OAuth response could not be parsed: {0}")]
    Parse(String),
    #[error("OAuth storage failed: {0}")]
    Storage(String),
    #[error("authentication_failed: {0}")]
    AuthenticationFailed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexOAuthAccount {
    pub account_id: String,
    pub email: Option<String>,
    pub authenticated_at: i64,
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexOAuthStatus {
    pub authenticated: bool,
    pub default_account_id: Option<String>,
    #[serde(default)]
    pub selection_revision: u64,
    #[serde(default)]
    pub selection_verified: bool,
    #[serde(default)]
    pub selected_at: Option<i64>,
    pub accounts: Vec<CodexOAuthAccount>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DeviceLogin {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Debug, Clone)]
pub struct AppliedOAuth {
    pub account_id: String,
    pub access_token: String,
    pub selection_revision: u64,
    pub selection_verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AccountMetadata {
    account_id: String,
    email: Option<String>,
    authenticated_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AccountStore {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    accounts: HashMap<String, AccountMetadata>,
    #[serde(default)]
    default_account_id: Option<String>,
    #[serde(default)]
    selection_revision: u64,
    #[serde(default)]
    selected_at: Option<i64>,
    #[serde(default)]
    selection_verified: bool,
}

#[derive(Debug, Clone)]
struct CachedToken {
    access_token: String,
    expires_at_ms: i64,
}

impl CachedToken {
    fn expiring_soon(&self) -> bool {
        self.expires_at_ms - chrono::Utc::now().timestamp_millis() < REFRESH_BUFFER_MS
    }
}

#[derive(Debug, Clone)]
struct PendingDevice {
    user_code: String,
    expires_at_ms: i64,
}

#[derive(Debug, Deserialize)]
struct DeviceStartResponse {
    device_auth_id: String,
    user_code: String,
    #[serde(default)]
    interval: Option<serde_json::Value>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DevicePollResponse {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Debug, Clone, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TokenClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default, rename = "https://api.openai.com/auth")]
    openai_auth: Option<OpenAiAuthClaim>,
    #[serde(default)]
    organizations: Vec<OrganizationClaim>,
    #[serde(default)]
    exp: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct OpenAiAuthClaim {
    chatgpt_account_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct OrganizationClaim {
    id: Option<String>,
}

pub struct CodexOAuthManager {
    root: PathBuf,
    client: reqwest::Client,
    accounts: RwLock<HashMap<String, AccountMetadata>>,
    default_account_id: RwLock<Option<String>>,
    selection_revision: RwLock<u64>,
    selected_at: RwLock<Option<i64>>,
    selection_verified: RwLock<bool>,
    selection_lock: Mutex<()>,
    access_tokens: RwLock<HashMap<String, CachedToken>>,
    refresh_locks: RwLock<HashMap<String, Arc<Mutex<()>>>>,
    pending: RwLock<HashMap<String, PendingDevice>>,
}

impl CodexOAuthManager {
    pub fn new(root: PathBuf) -> Self {
        let store = load_store(&root).unwrap_or_default();
        Self::from_store(root, store)
    }

    /// Build an evaluation-scoped manager pinned to one already-managed
    /// account without changing Vellum's persisted Desktop default.
    pub(crate) fn new_with_default_override(
        root: PathBuf,
        account_id: &str,
    ) -> Result<Self, OAuthError> {
        let mut store =
            load_store(&root).map_err(|error| OAuthError::Storage(error.to_string()))?;
        if !store.accounts.contains_key(account_id) {
            return Err(OAuthError::AccountNotFound(account_id.into()));
        }
        store.default_account_id = Some(account_id.into());
        Ok(Self::from_store(root, store))
    }

    /// Evaluation-only view of Codex Desktop's current native login. The
    /// token remains on the host and is never mounted into an agent container.
    /// No refresh token is copied, so this manager cannot rotate or invalidate
    /// Desktop's credential chain.
    pub(crate) fn new_with_native_access_token(
        root: PathBuf,
        auth_path: &Path,
    ) -> Result<Self, OAuthError> {
        let value: serde_json::Value = serde_json::from_slice(
            &std::fs::read(auth_path).map_err(|error| OAuthError::Storage(error.to_string()))?,
        )
        .map_err(|error| OAuthError::Parse(error.to_string()))?;
        let access_token = value
            .pointer("/tokens/access_token")
            .and_then(serde_json::Value::as_str)
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| OAuthError::Parse("native Codex auth has no access token".into()))?;
        let claims = parse_claims(access_token)
            .ok_or_else(|| OAuthError::Parse("native Codex access token is not a JWT".into()))?;
        let account_id = claims
            .chatgpt_account_id
            .clone()
            .or_else(|| {
                claims
                    .openai_auth
                    .as_ref()
                    .and_then(|auth| auth.chatgpt_account_id.clone())
            })
            .or_else(|| claims.organizations.first().and_then(|org| org.id.clone()))
            .ok_or_else(|| {
                OAuthError::Parse("native Codex token has no ChatGPT account id".into())
            })?;
        let expires_at_ms = claims
            .exp
            .map(|seconds| seconds.saturating_mul(1_000))
            .ok_or_else(|| OAuthError::Parse("native Codex token has no expiry".into()))?;
        let metadata = AccountMetadata {
            account_id: account_id.clone(),
            email: claims.email,
            authenticated_at: chrono::Utc::now().timestamp(),
        };
        let mut accounts = HashMap::new();
        accounts.insert(account_id.clone(), metadata);
        let manager = Self::from_store(
            root,
            AccountStore {
                version: 1,
                accounts,
                default_account_id: Some(account_id.clone()),
                selection_revision: 0,
                selected_at: Some(chrono::Utc::now().timestamp_millis()),
                selection_verified: true,
            },
        );
        manager
            .access_tokens
            .try_write()
            .expect("new OAuth manager lock is uncontended")
            .insert(
                account_id,
                CachedToken {
                    access_token: access_token.into(),
                    expires_at_ms,
                },
            );
        Ok(manager)
    }

    fn from_store(root: PathBuf, store: AccountStore) -> Self {
        Self {
            root,
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(15))
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
            accounts: RwLock::new(store.accounts),
            default_account_id: RwLock::new(store.default_account_id),
            selection_revision: RwLock::new(store.selection_revision),
            selected_at: RwLock::new(store.selected_at),
            selection_verified: RwLock::new(store.selection_verified),
            selection_lock: Mutex::const_new(()),
            access_tokens: RwLock::new(HashMap::new()),
            refresh_locks: RwLock::new(HashMap::new()),
            pending: RwLock::new(HashMap::new()),
        }
    }

    /// Non-blocking peek of the default ChatGPT account subject for realm
    /// fingerprinting on the request hot path.
    pub fn peek_default_account_id(&self) -> Option<String> {
        self.default_account_id
            .try_read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Identity only — never a token — for the blocking remote commands, which
    /// run outside the async runtime and cannot await the store. Ordered by
    /// account id so a pairing inventory presents the same rows in the same
    /// places on every refresh. An empty result means the lock was held, not
    /// that there are no accounts, so callers must not read it as "signed out".
    pub fn peek_accounts(&self) -> Vec<(String, Option<String>)> {
        let Ok(accounts) = self.accounts.try_read() else {
            return Vec::new();
        };
        let mut listed: Vec<(String, Option<String>)> = accounts
            .values()
            .map(|account| (account.account_id.clone(), account.email.clone()))
            .collect();
        listed.sort_by(|left, right| left.0.cmp(&right.0));
        listed
    }

    pub async fn start_device_flow(&self) -> Result<DeviceLogin, OAuthError> {
        let response = self
            .client
            .post(DEVICE_START_URL)
            .header("User-Agent", USER_AGENT)
            .json(&serde_json::json!({ "client_id": CLIENT_ID }))
            .send()
            .await
            .map_err(request_error)?;
        if !response.status().is_success() {
            return Err(response_error("device-code request", response).await);
        }
        let device: DeviceStartResponse = response
            .json()
            .await
            .map_err(|error| OAuthError::Parse(error.to_string()))?;
        let expires_in = device.expires_in.unwrap_or(DEFAULT_EXPIRES_IN);
        let interval = parse_interval(device.interval.as_ref());
        let now = chrono::Utc::now().timestamp_millis();
        let mut pending = self.pending.write().await;
        pending.retain(|_, item| item.expires_at_ms > now);
        pending.insert(
            device.device_auth_id.clone(),
            PendingDevice {
                user_code: device.user_code.clone(),
                expires_at_ms: now + expires_in as i64 * 1_000,
            },
        );
        Ok(DeviceLogin {
            device_code: device.device_auth_id,
            user_code: device.user_code,
            verification_uri: VERIFICATION_URL.into(),
            expires_in,
            interval,
        })
    }

    pub async fn poll_device_flow(
        &self,
        device_code: &str,
    ) -> Result<Option<CodexOAuthAccount>, OAuthError> {
        let pending = self.pending.read().await.get(device_code).cloned();
        let Some(pending) = pending else {
            return Err(OAuthError::Expired);
        };
        if pending.expires_at_ms <= chrono::Utc::now().timestamp_millis() {
            self.pending.write().await.remove(device_code);
            return Err(OAuthError::Expired);
        }
        let response = self
            .client
            .post(DEVICE_POLL_URL)
            .header("User-Agent", USER_AGENT)
            .json(&serde_json::json!({
                "device_auth_id": device_code,
                "user_code": pending.user_code,
            }))
            .send()
            .await
            .map_err(request_error)?;
        if matches!(
            response.status(),
            reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::NOT_FOUND
        ) {
            return Err(OAuthError::AuthorizationPending);
        }
        if response.status() == reqwest::StatusCode::GONE {
            return Err(OAuthError::Expired);
        }
        if !response.status().is_success() {
            return Err(response_error("device-code poll", response).await);
        }
        let success: DevicePollResponse = response
            .json()
            .await
            .map_err(|error| OAuthError::Parse(error.to_string()))?;
        let tokens = self
            .exchange_code(&success.authorization_code, &success.code_verifier)
            .await?;
        let (account_id, email) = extract_login_identity(&tokens)?;
        let refresh_token = tokens
            .refresh_token
            .as_deref()
            .ok_or_else(|| OAuthError::Parse("token response has no refresh_token".into()))?;
        crate::credentials::save(&self.root, &credential_key(&account_id), refresh_token)
            .map_err(|error| OAuthError::Storage(error.to_string()))?;

        let authenticated_at = chrono::Utc::now().timestamp();
        self.accounts.write().await.insert(
            account_id.clone(),
            AccountMetadata {
                account_id: account_id.clone(),
                email: email.clone(),
                authenticated_at,
            },
        );
        {
            let mut default = self.default_account_id.write().await;
            if default.is_none() {
                *default = Some(account_id.clone());
            }
        }
        self.access_tokens.write().await.insert(
            account_id.clone(),
            CachedToken {
                access_token: tokens.access_token,
                expires_at_ms: expires_at(tokens.expires_in),
            },
        );
        self.pending.write().await.remove(device_code);
        self.persist().await?;
        let default = self.resolve_default().await;
        Ok(Some(CodexOAuthAccount {
            account_id: account_id.clone(),
            email,
            authenticated_at,
            is_default: default.as_deref() == Some(&account_id),
        }))
    }

    pub async fn status(&self) -> CodexOAuthStatus {
        let accounts = self.accounts.read().await.clone();
        let default = self.resolve_default().await;
        let mut values: Vec<_> = accounts
            .into_values()
            .map(|account| CodexOAuthAccount {
                is_default: default.as_deref() == Some(&account.account_id),
                account_id: account.account_id,
                email: account.email,
                authenticated_at: account.authenticated_at,
            })
            .collect();
        values.sort_by(|a, b| {
            b.is_default
                .cmp(&a.is_default)
                .then_with(|| b.authenticated_at.cmp(&a.authenticated_at))
        });
        CodexOAuthStatus {
            authenticated: !values.is_empty(),
            default_account_id: default,
            selection_revision: *self.selection_revision.read().await,
            selection_verified: *self.selection_verified.read().await,
            selected_at: *self.selected_at.read().await,
            accounts: values,
        }
    }

    pub async fn set_default(&self, account_id: &str) -> Result<(), OAuthError> {
        let _selection_guard = self.selection_lock.lock().await;
        if !self.accounts.read().await.contains_key(account_id) {
            return Err(OAuthError::AccountNotFound(account_id.into()));
        }
        // Validate the target grant before publishing it. This also refreshes
        // an expiring token and validates the access-token identity, so a
        // failed switch can never leave a half-selected account behind.
        let target = self.valid_token_for(account_id).await?;
        let previous = (
            self.default_account_id.read().await.clone(),
            *self.selection_revision.read().await,
            *self.selected_at.read().await,
            *self.selection_verified.read().await,
        );
        if previous.0.as_deref() == Some(account_id) {
            *self.selection_verified.write().await = true;
            self.persist().await?;
            return Ok(());
        }
        *self.default_account_id.write().await = Some(account_id.into());
        *self.selection_revision.write().await = previous.1.saturating_add(1);
        *self.selected_at.write().await = Some(chrono::Utc::now().timestamp_millis());
        *self.selection_verified.write().await = true;
        if let Err(error) = self.persist().await {
            *self.default_account_id.write().await = previous.0;
            *self.selection_revision.write().await = previous.1;
            *self.selected_at.write().await = previous.2;
            *self.selection_verified.write().await = previous.3;
            return Err(error);
        }
        let _ = target;
        Ok(())
    }

    pub async fn remove_account(&self, account_id: &str) -> Result<(), OAuthError> {
        if self.accounts.write().await.remove(account_id).is_none() {
            return Err(OAuthError::AccountNotFound(account_id.into()));
        }
        self.access_tokens.write().await.remove(account_id);
        self.refresh_locks.write().await.remove(account_id);
        crate::credentials::remove(&self.root, &credential_key(account_id))
            .map_err(|error| OAuthError::Storage(error.to_string()))?;
        if self.default_account_id.read().await.as_deref() == Some(account_id) {
            *self.default_account_id.write().await = self.fallback_default().await;
        }
        self.persist().await
    }

    pub async fn clear(&self) -> Result<(), OAuthError> {
        let ids: Vec<String> = self.accounts.read().await.keys().cloned().collect();
        for account_id in ids {
            crate::credentials::remove(&self.root, &credential_key(&account_id))
                .map_err(|error| OAuthError::Storage(error.to_string()))?;
        }
        self.accounts.write().await.clear();
        self.access_tokens.write().await.clear();
        self.refresh_locks.write().await.clear();
        self.pending.write().await.clear();
        *self.default_account_id.write().await = None;
        self.persist().await
    }

    /// Returns `None` when Vellum has no managed ChatGPT account. This is the
    /// native Codex-login mode: the proxy preserves the incoming credentials.
    pub async fn valid_default_auth(&self) -> Result<Option<AppliedOAuth>, OAuthError> {
        let _selection_guard = self.selection_lock.lock().await;
        let Some(account_id) = self.resolve_default().await else {
            return Ok(None);
        };
        let revision = *self.selection_revision.read().await;
        let verified = *self.selection_verified.read().await;
        let access_token = self.valid_token_for(&account_id).await?;
        if !verified {
            *self.selection_verified.write().await = true;
            self.persist().await?;
        }
        Ok(Some(AppliedOAuth {
            account_id,
            access_token,
            selection_revision: revision,
            selection_verified: true,
        }))
    }

    /// Resolve a valid access token for one specific managed ChatGPT account.
    /// Profile statistics use this to aggregate every account instead of only
    /// the account selected for request routing.
    pub async fn valid_auth_for(&self, account_id: &str) -> Result<AppliedOAuth, OAuthError> {
        if !self.accounts.read().await.contains_key(account_id) {
            return Err(OAuthError::AccountNotFound(account_id.into()));
        }
        let access_token = self.valid_token_for(account_id).await?;
        Ok(AppliedOAuth {
            account_id: account_id.into(),
            access_token,
            selection_revision: *self.selection_revision.read().await,
            selection_verified: *self.selection_verified.read().await,
        })
    }

    pub async fn refresh_after_rejection(
        &self,
        account_id: &str,
        rejected_token: &str,
    ) -> Result<AppliedOAuth, OAuthError> {
        let lock = self.refresh_lock(account_id).await;
        let _guard = lock.lock().await;
        if let Some(cached) = self.access_tokens.read().await.get(account_id) {
            if cached.access_token != rejected_token && !cached.expiring_soon() {
                return Ok(AppliedOAuth {
                    account_id: account_id.into(),
                    access_token: cached.access_token.clone(),
                    selection_revision: *self.selection_revision.read().await,
                    selection_verified: *self.selection_verified.read().await,
                });
            }
        }
        let access_token = self.refresh_locked(account_id).await?;
        Ok(AppliedOAuth {
            account_id: account_id.into(),
            access_token,
            selection_revision: *self.selection_revision.read().await,
            selection_verified: *self.selection_verified.read().await,
        })
    }

    pub async fn force_refresh_default(&self) -> Result<AppliedOAuth, OAuthError> {
        let account_id = self
            .resolve_default()
            .await
            .ok_or_else(|| OAuthError::AccountNotFound("no ChatGPT account".into()))?;
        let lock = self.refresh_lock(&account_id).await;
        let _guard = lock.lock().await;
        let access_token = self.refresh_locked(&account_id).await?;
        Ok(AppliedOAuth {
            account_id,
            access_token,
            selection_revision: *self.selection_revision.read().await,
            selection_verified: *self.selection_verified.read().await,
        })
    }

    async fn valid_token_for(&self, account_id: &str) -> Result<String, OAuthError> {
        if let Some(cached) = self.access_tokens.read().await.get(account_id) {
            if !cached.expiring_soon() {
                return Ok(cached.access_token.clone());
            }
        }
        let lock = self.refresh_lock(account_id).await;
        let _guard = lock.lock().await;
        if let Some(cached) = self.access_tokens.read().await.get(account_id) {
            if !cached.expiring_soon() {
                return Ok(cached.access_token.clone());
            }
        }
        self.refresh_locked(account_id).await
    }

    async fn refresh_locked(&self, account_id: &str) -> Result<String, OAuthError> {
        if !self.accounts.read().await.contains_key(account_id) {
            return Err(OAuthError::AccountNotFound(account_id.into()));
        }
        let refresh_token = crate::credentials::load(&self.root, &credential_key(account_id))
            .map_err(|error| OAuthError::Storage(error.to_string()))?
            .ok_or_else(|| OAuthError::RefreshRejected("encrypted token is missing".into()))?;
        let response = self
            .client
            .post(TOKEN_URL)
            .header("User-Agent", USER_AGENT)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token.as_str()),
                ("client_id", CLIENT_ID),
            ])
            .send()
            .await
            .map_err(request_error)?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            if matches!(
                status,
                reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
            ) || body.contains("invalid_grant")
                || body.contains("refresh_token_reused")
            {
                return Err(OAuthError::RefreshRejected(oauth_error_detail(&body)));
            }
            return Err(OAuthError::Request(format!(
                "refresh failed: HTTP {status}: {}",
                safe_body(&body)
            )));
        }
        let tokens: TokenResponse = response
            .json()
            .await
            .map_err(|error| OAuthError::Parse(error.to_string()))?;
        validate_token_identity(account_id, &tokens)?;
        let (_, refreshed_email) = extract_login_identity(&tokens)?;
        if let Some(email) = refreshed_email {
            let changed = {
                let mut accounts = self.accounts.write().await;
                accounts.get_mut(account_id).is_some_and(|account| {
                    if account.email.as_deref() == Some(email.as_str()) {
                        false
                    } else {
                        account.email = Some(email.clone());
                        true
                    }
                })
            };
            if changed {
                self.persist().await?;
            }
        }
        if let Some(rotated) = tokens.refresh_token.as_deref() {
            crate::credentials::save(&self.root, &credential_key(account_id), rotated)
                .map_err(|error| OAuthError::Storage(error.to_string()))?;
        }
        self.access_tokens.write().await.insert(
            account_id.into(),
            CachedToken {
                access_token: tokens.access_token.clone(),
                expires_at_ms: expires_at(tokens.expires_in),
            },
        );
        Ok(tokens.access_token)
    }

    async fn exchange_code(&self, code: &str, verifier: &str) -> Result<TokenResponse, OAuthError> {
        let response = self
            .client
            .post(TOKEN_URL)
            .header("User-Agent", USER_AGENT)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", REDIRECT_URI),
                ("client_id", CLIENT_ID),
                ("code_verifier", verifier),
            ])
            .send()
            .await
            .map_err(request_error)?;
        if !response.status().is_success() {
            return Err(response_error("token exchange", response).await);
        }
        response
            .json()
            .await
            .map_err(|error| OAuthError::Parse(error.to_string()))
    }

    async fn refresh_lock(&self, account_id: &str) -> Arc<Mutex<()>> {
        if let Some(lock) = self.refresh_locks.read().await.get(account_id) {
            return Arc::clone(lock);
        }
        let mut locks = self.refresh_locks.write().await;
        Arc::clone(
            locks
                .entry(account_id.into())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    async fn resolve_default(&self) -> Option<String> {
        let stored = self.default_account_id.read().await.clone();
        let accounts = self.accounts.read().await;
        stored
            .filter(|id| accounts.contains_key(id))
            .or_else(|| fallback_default_id(&accounts))
    }

    async fn fallback_default(&self) -> Option<String> {
        let accounts = self.accounts.read().await;
        fallback_default_id(&accounts)
    }

    async fn persist(&self) -> Result<(), OAuthError> {
        let accounts = self.accounts.read().await.clone();
        let default_account_id = self.resolve_default().await;
        let store = AccountStore {
            version: 1,
            accounts,
            default_account_id,
            selection_revision: *self.selection_revision.read().await,
            selected_at: *self.selected_at.read().await,
            selection_verified: *self.selection_verified.read().await,
        };
        write_store(&self.root, &store)
    }
}

/// Read only the account identity from Codex Desktop's native auth file.
/// Token material never leaves this function. Older files that lack the
/// explicit account field fall back to the access-token identity claim.
pub(crate) fn native_codex_account_id(auth_path: &Path) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(auth_path).ok()?).ok()?;
    value
        .pointer("/tokens/account_id")
        .or_else(|| value.pointer("/tokens/accountId"))
        .and_then(serde_json::Value::as_str)
        .filter(|account| !account.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| {
            let token = value
                .pointer("/tokens/access_token")
                .and_then(serde_json::Value::as_str)?;
            parse_claims(token).and_then(|claims| claim_identity(&claims))
        })
}

fn fallback_default_id(accounts: &HashMap<String, AccountMetadata>) -> Option<String> {
    accounts
        .values()
        .max_by_key(|account| account.authenticated_at)
        .map(|account| account.account_id.clone())
}

fn metadata_path(root: &Path) -> PathBuf {
    root.join("codex_oauth_accounts.json")
}

fn load_store(root: &Path) -> Result<AccountStore, OAuthError> {
    let bytes = match std::fs::read(metadata_path(root)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AccountStore::default())
        }
        Err(error) => return Err(OAuthError::Storage(error.to_string())),
    };
    serde_json::from_slice(&bytes).map_err(|error| OAuthError::Parse(error.to_string()))
}

fn write_store(root: &Path, store: &AccountStore) -> Result<(), OAuthError> {
    std::fs::create_dir_all(root).map_err(|error| OAuthError::Storage(error.to_string()))?;
    let path = metadata_path(root);
    let tmp = root.join(format!(
        "codex_oauth_accounts.json.tmp-{}",
        ulid::Ulid::new()
    ));
    let bytes =
        serde_json::to_vec_pretty(store).map_err(|error| OAuthError::Parse(error.to_string()))?;
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)
        .map_err(|error| OAuthError::Storage(error.to_string()))?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| OAuthError::Storage(error.to_string()))?;
    drop(file);
    replace_file_atomically(&tmp, &path)
}

fn replace_file_atomically(tmp: &Path, path: &Path) -> Result<(), OAuthError> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let source: Vec<u16> = tmp
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let destination: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let replaced = unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if replaced == 0 {
            let error = std::io::Error::last_os_error();
            let _ = std::fs::remove_file(tmp);
            return Err(OAuthError::Storage(error.to_string()));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(tmp, path).map_err(|error| OAuthError::Storage(error.to_string()))
    }
}

fn credential_key(account_id: &str) -> String {
    let digest = Sha256::digest(account_id.as_bytes());
    let suffix = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("codex-oauth-{suffix}")
}

fn parse_interval(value: Option<&serde_json::Value>) -> u64 {
    let interval = match value {
        Some(serde_json::Value::Number(number)) => number.as_u64().unwrap_or(5),
        Some(serde_json::Value::String(value)) => value.parse().unwrap_or(5),
        _ => 5,
    };
    interval.max(1) + 3
}

fn expires_at(expires_in: Option<i64>) -> i64 {
    chrono::Utc::now().timestamp_millis() + expires_in.unwrap_or(3_600).max(1) * 1_000
}

fn parse_claims(token: &str) -> Option<TokenClaims> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
fn extract_identity(tokens: &TokenResponse) -> (Option<String>, Option<String>) {
    extract_login_identity(tokens)
        .ok()
        .map_or((None, None), |(account, email)| (Some(account), email))
}

fn claim_identity(claims: &TokenClaims) -> Option<String> {
    claims
        .chatgpt_account_id
        .clone()
        .or_else(|| {
            claims
                .openai_auth
                .as_ref()
                .and_then(|auth| auth.chatgpt_account_id.clone())
        })
        .or_else(|| claims.organizations.first().and_then(|org| org.id.clone()))
}

fn extract_login_identity(tokens: &TokenResponse) -> Result<(String, Option<String>), OAuthError> {
    let access_claims = parse_claims(&tokens.access_token).ok_or_else(|| {
        OAuthError::AuthenticationFailed("access token has no verifiable JWT claims".into())
    })?;
    let access_account = claim_identity(&access_claims).ok_or_else(|| {
        OAuthError::AuthenticationFailed("access token has no ChatGPT account id".into())
    })?;
    let id_claims = tokens
        .id_token
        .as_deref()
        .map(|token| {
            parse_claims(token).ok_or_else(|| {
                OAuthError::AuthenticationFailed("id token has no verifiable JWT claims".into())
            })
        })
        .transpose()?;
    if let Some(id_account) = id_claims.as_ref().and_then(claim_identity) {
        if id_account != access_account {
            return Err(OAuthError::AuthenticationFailed(
                "id token and access token account identities differ".into(),
            ));
        }
    }
    let email = access_claims
        .email
        .clone()
        .or_else(|| id_claims.and_then(|claims| claims.email));
    Ok((access_account, email))
}

fn validate_token_identity(
    expected_account_id: &str,
    tokens: &TokenResponse,
) -> Result<(), OAuthError> {
    let (actual, _) = extract_login_identity(tokens)?;
    if actual != expected_account_id {
        return Err(OAuthError::AuthenticationFailed(format!(
            "token account identity `{actual}` does not match expected account"
        )));
    }
    Ok(())
}

fn request_error(error: reqwest::Error) -> OAuthError {
    OAuthError::Request(error.to_string())
}

async fn response_error(operation: &str, response: reqwest::Response) -> OAuthError {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    OAuthError::Request(format!(
        "{operation} failed: HTTP {status}: {}",
        safe_body(&body)
    ))
}

fn safe_body(body: &str) -> String {
    body.chars().take(500).collect()
}

fn oauth_error_detail(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("error_description"))
                .or_else(|| value.get("error"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| safe_body(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_poll_interval_has_safety_margin() {
        assert_eq!(parse_interval(Some(&serde_json::json!(5))), 8);
        assert_eq!(parse_interval(Some(&serde_json::json!("2"))), 5);
    }

    #[test]
    fn identity_supports_namespaced_claim() {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "email": "person@example.test",
                "https://api.openai.com/auth": {"chatgpt_account_id": "acct-1"}
            })
            .to_string(),
        );
        let token = format!("header.{payload}.signature");
        let (account, email) = extract_identity(&TokenResponse {
            access_token: token,
            refresh_token: None,
            id_token: None,
            expires_in: None,
        });
        assert_eq!(account.as_deref(), Some("acct-1"));
        assert_eq!(email.as_deref(), Some("person@example.test"));
    }

    #[test]
    fn identity_rejects_conflicting_id_and_access_token_claims() {
        let claim = |account: &str| {
            let payload = URL_SAFE_NO_PAD
                .encode(serde_json::json!({"chatgpt_account_id": account}).to_string());
            format!("header.{payload}.signature")
        };
        let error = extract_login_identity(&TokenResponse {
            access_token: claim("acct-access"),
            refresh_token: Some("refresh".into()),
            id_token: Some(claim("acct-id")),
            expires_in: Some(3600),
        })
        .unwrap_err();
        assert!(error.to_string().starts_with("authentication_failed:"));
    }

    #[test]
    fn identity_uses_id_token_email_when_access_token_omits_it() {
        let claim = |value: serde_json::Value| {
            let payload = URL_SAFE_NO_PAD.encode(value.to_string());
            format!("header.{payload}.signature")
        };
        let (account, email) = extract_identity(&TokenResponse {
            access_token: claim(serde_json::json!({"chatgpt_account_id": "acct-1"})),
            refresh_token: None,
            id_token: Some(claim(serde_json::json!({
                "chatgpt_account_id": "acct-1",
                "email": "person@example.test"
            }))),
            expires_in: None,
        });
        assert_eq!(account.as_deref(), Some("acct-1"));
        assert_eq!(email.as_deref(), Some("person@example.test"));
    }

    #[test]
    fn refresh_identity_must_match_expected_account() {
        let payload = URL_SAFE_NO_PAD
            .encode(serde_json::json!({"chatgpt_account_id": "acct-other"}).to_string());
        let token = format!("header.{payload}.signature");
        let error = validate_token_identity(
            "acct-expected",
            &TokenResponse {
                access_token: token,
                refresh_token: None,
                id_token: None,
                expires_in: Some(3600),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("authentication_failed"));
    }

    #[tokio::test]
    async fn account_switch_publishes_a_verified_monotonic_revision() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().to_path_buf());
        for id in ["acct-a", "acct-b"] {
            manager.accounts.write().await.insert(
                id.into(),
                AccountMetadata {
                    account_id: id.into(),
                    email: None,
                    authenticated_at: 1,
                },
            );
            manager.access_tokens.write().await.insert(
                id.into(),
                CachedToken {
                    access_token: format!("token-{id}"),
                    expires_at_ms: i64::MAX,
                },
            );
        }
        *manager.default_account_id.write().await = Some("acct-a".into());
        manager.persist().await.unwrap();
        let before = manager.status().await;
        manager.set_default("acct-b").await.unwrap();
        let after = manager.status().await;
        assert_eq!(after.default_account_id.as_deref(), Some("acct-b"));
        assert_eq!(after.selection_revision, before.selection_revision + 1);
        assert!(after.selection_verified);
        let applied = manager.valid_default_auth().await.unwrap().unwrap();
        assert_eq!(applied.account_id, "acct-b");
        assert_eq!(applied.selection_revision, after.selection_revision);
    }

    #[tokio::test]
    async fn account_switch_storage_failure_rolls_back_the_selection_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("not-a-directory");
        std::fs::write(&root, b"occupied").unwrap();
        let manager = CodexOAuthManager::new(root);
        for id in ["acct-a", "acct-b"] {
            manager.accounts.write().await.insert(
                id.into(),
                AccountMetadata {
                    account_id: id.into(),
                    email: None,
                    authenticated_at: 1,
                },
            );
            manager.access_tokens.write().await.insert(
                id.into(),
                CachedToken {
                    access_token: format!("token-{id}"),
                    expires_at_ms: i64::MAX,
                },
            );
        }
        *manager.default_account_id.write().await = Some("acct-a".into());
        *manager.selection_revision.write().await = 9;
        *manager.selection_verified.write().await = true;
        let before = manager.status().await;

        assert!(manager.set_default("acct-b").await.is_err());
        let after = manager.status().await;
        assert_eq!(after.default_account_id, before.default_account_id);
        assert_eq!(after.selection_revision, before.selection_revision);
        assert_eq!(after.selected_at, before.selected_at);
        assert_eq!(after.selection_verified, before.selection_verified);
    }

    #[tokio::test]
    async fn concurrent_account_switches_leave_a_verified_non_decreasing_revision() {
        let temp = tempfile::tempdir().unwrap();
        let manager = std::sync::Arc::new(CodexOAuthManager::new(temp.path().to_path_buf()));
        for id in ["acct-a", "acct-b", "acct-c"] {
            manager.accounts.write().await.insert(
                id.into(),
                AccountMetadata {
                    account_id: id.into(),
                    email: None,
                    authenticated_at: 1,
                },
            );
            manager.access_tokens.write().await.insert(
                id.into(),
                CachedToken {
                    access_token: format!("token-{id}"),
                    expires_at_ms: i64::MAX,
                },
            );
        }
        *manager.default_account_id.write().await = Some("acct-a".into());
        manager.persist().await.unwrap();
        let initial = manager.status().await.selection_revision;
        let tasks = ["acct-b", "acct-c", "acct-a", "acct-b", "acct-c"]
            .into_iter()
            .map(|account_id| {
                let manager = std::sync::Arc::clone(&manager);
                tokio::spawn(async move {
                    manager.set_default(account_id).await.unwrap();
                })
            });
        for task in tasks {
            task.await.unwrap();
        }
        let final_status = manager.status().await;
        assert!(final_status.selection_revision >= initial);
        assert!(final_status.selection_verified);
        assert!(final_status.default_account_id.is_some());
    }

    #[tokio::test]
    async fn refresh_token_is_encrypted_outside_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().to_path_buf());
        let account_id = "acct-test";
        crate::credentials::save(temp.path(), &credential_key(account_id), "refresh-secret")
            .unwrap();
        manager.accounts.write().await.insert(
            account_id.into(),
            AccountMetadata {
                account_id: account_id.into(),
                email: Some("person@example.test".into()),
                authenticated_at: 1,
            },
        );
        *manager.default_account_id.write().await = Some(account_id.into());
        manager.persist().await.unwrap();
        let metadata = std::fs::read_to_string(metadata_path(temp.path())).unwrap();
        assert!(!metadata.contains("refresh-secret"));
        let encrypted = std::fs::read(
            temp.path()
                .join("credentials")
                .join(format!("{}.bin", credential_key(account_id))),
        )
        .unwrap();
        assert!(!String::from_utf8_lossy(&encrypted).contains("refresh-secret"));
    }

    #[test]
    fn native_codex_identity_reader_returns_no_token_material() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("auth.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "tokens": {
                    "account_id": "acct-native",
                    "access_token": "secret-access",
                    "refresh_token": "secret-refresh"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            native_codex_account_id(&path).as_deref(),
            Some("acct-native")
        );
    }
}
