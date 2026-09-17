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
    /// Stable Vellum credential selector. This is intentionally not the
    /// ChatGPT workspace id: two users may hold seats in one workspace.
    pub account_id: String,
    pub workspace_id: String,
    pub workspace_name: Option<String>,
    pub plan_type: Option<String>,
    pub workspace_kind: String,
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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum QuotaPoolStrategy {
    #[default]
    #[serde(alias = "most", alias = "soonest")]
    Rank,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QuotaPoolMember {
    pub account_id: String,
    #[serde(default)]
    pub in_pool: bool,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub weekly_floor: u8,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QuotaPoolSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub strategy: QuotaPoolStrategy,
    #[serde(default)]
    pub members: Vec<QuotaPoolMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QuotaPoolStatus {
    #[serde(flatten)]
    pub settings: QuotaPoolSettings,
    pub active_account_id: Option<String>,
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
    /// Stable selector used to find and refresh this exact credential.
    pub credential_id: String,
    /// ChatGPT workspace/billing account sent in ChatGPT-Account-Id.
    pub account_id: String,
    pub access_token: String,
    pub selection_revision: u64,
    pub selection_verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AccountMetadata {
    /// Stable credential selector (user principal + workspace).
    account_id: String,
    /// ChatGPT workspace/billing account. Legacy version-1 rows omit this and
    /// use `account_id` for both meanings until that login is added again.
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    workspace_name: Option<String>,
    #[serde(default)]
    plan_type: Option<String>,
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
    #[serde(default)]
    quota_pool: QuotaPoolSettings,
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

pub struct CodexOAuthManager {
    root: PathBuf,
    client: reqwest::Client,
    accounts: RwLock<HashMap<String, AccountMetadata>>,
    default_account_id: RwLock<Option<String>>,
    selection_revision: RwLock<u64>,
    selected_at: RwLock<Option<i64>>,
    selection_verified: RwLock<bool>,
    quota_pool: RwLock<QuotaPoolSettings>,
    active_pool_account_id: RwLock<Option<String>>,
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
        store.quota_pool.enabled = false;
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
        let identity = vellum_proxy_runtime::chatgpt_identity_from_jwt(access_token)
            .ok_or_else(|| OAuthError::Parse("native Codex access token is not a JWT".into()))?;
        let expires_at_ms = jwt_exp(access_token)
            .map(|seconds| seconds.saturating_mul(1_000))
            .ok_or_else(|| OAuthError::Parse("native Codex token has no expiry".into()))?;
        let metadata = AccountMetadata {
            account_id: identity.credential_id.clone(),
            chatgpt_account_id: Some(identity.workspace_id.clone()),
            workspace_name: identity.workspace_name,
            plan_type: identity.plan_type,
            email: identity.email,
            authenticated_at: chrono::Utc::now().timestamp(),
        };
        let mut accounts = HashMap::new();
        accounts.insert(identity.credential_id.clone(), metadata);
        let manager = Self::from_store(
            root,
            AccountStore {
                version: 2,
                accounts,
                default_account_id: Some(identity.credential_id.clone()),
                selection_revision: 0,
                selected_at: Some(chrono::Utc::now().timestamp_millis()),
                selection_verified: true,
                quota_pool: QuotaPoolSettings::default(),
            },
        );
        manager
            .access_tokens
            .try_write()
            .expect("new OAuth manager lock is uncontended")
            .insert(
                identity.credential_id,
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
            quota_pool: RwLock::new(store.quota_pool),
            active_pool_account_id: RwLock::new(None),
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
    pub fn peek_accounts(&self) -> Vec<(String, Option<String>, Option<String>)> {
        let Ok(accounts) = self.accounts.try_read() else {
            return Vec::new();
        };
        let mut listed: Vec<(String, Option<String>, Option<String>)> = accounts
            .values()
            .map(|account| {
                (
                    account.account_id.clone(),
                    account.email.clone(),
                    account.workspace_name.clone(),
                )
            })
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
        let identity = extract_login_identity(&tokens)?;
        let account_id = identity.credential_id.clone();
        let email = identity.email.clone();
        let refresh_token = tokens
            .refresh_token
            .as_deref()
            .ok_or_else(|| OAuthError::Parse("token response has no refresh_token".into()))?;
        crate::credentials::save(&self.root, &credential_key(&account_id), refresh_token)
            .map_err(|error| OAuthError::Storage(error.to_string()))?;

        // Version-1 used the workspace id as the credential selector. Once
        // this exact user signs in again, replace only their matching legacy
        // row; a legacy row for another user in the same workspace must stay
        // intact until that user authenticates too.
        let legacy_account_id = {
            let accounts = self.accounts.read().await;
            matching_legacy_account(&accounts, &identity.workspace_id, email.as_deref())
        };
        if let Some(legacy_account_id) = legacy_account_id.as_ref() {
            crate::credentials::remove(&self.root, &credential_key(legacy_account_id))
                .map_err(|error| OAuthError::Storage(error.to_string()))?;
            self.accounts.write().await.remove(legacy_account_id);
            self.access_tokens.write().await.remove(legacy_account_id);
            let mut default = self.default_account_id.write().await;
            if default.as_deref() == Some(legacy_account_id.as_str()) {
                *default = Some(account_id.clone());
                *self.selection_revision.write().await += 1;
                *self.selected_at.write().await = Some(chrono::Utc::now().timestamp_millis());
                *self.selection_verified.write().await = true;
            }
        }

        let authenticated_at = chrono::Utc::now().timestamp();
        self.accounts.write().await.insert(
            account_id.clone(),
            AccountMetadata {
                account_id: account_id.clone(),
                chatgpt_account_id: Some(identity.workspace_id.clone()),
                workspace_name: identity.workspace_name.clone(),
                plan_type: identity.plan_type.clone(),
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
            workspace_id: identity.workspace_id,
            workspace_name: identity.workspace_name,
            plan_type: identity.plan_type,
            workspace_kind: identity.workspace_kind.as_str().into(),
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
                workspace_id: account
                    .chatgpt_account_id
                    .clone()
                    .unwrap_or_else(|| account.account_id.clone()),
                workspace_name: account.workspace_name,
                workspace_kind: vellum_proxy_runtime::ChatGptWorkspaceKind::from_plan_type(
                    account.plan_type.as_deref(),
                )
                .as_str()
                .into(),
                plan_type: account.plan_type,
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

    pub async fn quota_pool_status(&self) -> QuotaPoolStatus {
        let settings = self.quota_pool.read().await.clone();
        QuotaPoolStatus {
            settings,
            active_account_id: self.active_pool_account_id.read().await.clone(),
        }
    }

    pub async fn set_quota_pool(
        &self,
        mut settings: QuotaPoolSettings,
    ) -> Result<QuotaPoolStatus, OAuthError> {
        let _selection_guard = self.selection_lock.lock().await;
        let accounts = self.accounts.read().await;
        let mut seen = std::collections::HashSet::new();
        settings.members.retain(|member| {
            accounts.contains_key(&member.account_id) && seen.insert(member.account_id.clone())
        });
        drop(accounts);
        for member in &mut settings.members {
            member.weekly_floor = member.weekly_floor.min(100);
            if !member.in_pool {
                member.paused = false;
            }
        }
        let previous = self.quota_pool.read().await.clone();
        *self.quota_pool.write().await = settings;
        if let Err(error) = self.persist().await {
            *self.quota_pool.write().await = previous;
            return Err(error);
        }
        if !self.quota_pool.read().await.enabled {
            *self.active_pool_account_id.write().await = None;
        }
        Ok(self.quota_pool_status().await)
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
        // `set_default` is an explicit user selection, even when the account
        // was already the implicit first-login default. Always publish a new
        // revision and timestamp so the UI, diagnostics, and request boundary
        // can distinguish a verified choice from an untouched fallback.
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
        self.quota_pool
            .write()
            .await
            .members
            .retain(|member| member.account_id != account_id);
        if self.active_pool_account_id.read().await.as_deref() == Some(account_id) {
            *self.active_pool_account_id.write().await = None;
        }
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
        *self.quota_pool.write().await = QuotaPoolSettings::default();
        *self.active_pool_account_id.write().await = None;
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
        let workspace_id = self.workspace_id_for(&account_id).await?;
        if !verified {
            *self.selection_verified.write().await = true;
            self.persist().await?;
        }
        Ok(Some(AppliedOAuth {
            credential_id: account_id,
            account_id: workspace_id,
            access_token,
            selection_revision: revision,
            selection_verified: true,
        }))
    }

    /// Resolve one account for a new ordinary Official request. An empty pool
    /// intentionally falls back to the manual selection; once the user adds a
    /// member, exhaustion fails closed instead of silently charging a pool-external
    /// account. Auto Review calls `valid_auth_for` and remains explicitly billed.
    pub async fn valid_routing_auth(&self) -> Result<Option<AppliedOAuth>, OAuthError> {
        let settings = self.quota_pool.read().await.clone();
        if !settings.enabled || !settings.members.iter().any(|member| member.in_pool) {
            *self.active_pool_account_id.write().await = None;
            return self.valid_default_auth().await;
        }

        let mut failures = Vec::new();
        for member in &settings.members {
            if !member.in_pool || member.paused {
                continue;
            }
            let mut auth = match self.valid_auth_for(&member.account_id).await {
                Ok(auth) => auth,
                Err(error) => {
                    failures.push(format!("{}: {error}", member.account_id));
                    continue;
                }
            };
            let windows = match crate::codex_quota::query_with_cache_ttl(
                &auth.access_token,
                &auth.account_id,
                &auth.credential_id,
                false,
                crate::codex_quota::ROUTING_QUOTA_CACHE_TTL,
            )
            .await
            {
                Ok(windows) => windows,
                Err(crate::codex_quota::CodexQuotaError::Unauthorized) => {
                    auth = match self
                        .refresh_after_rejection(&auth.credential_id, &auth.access_token)
                        .await
                    {
                        Ok(refreshed) => refreshed,
                        Err(error) => {
                            failures.push(format!("{}: {error}", member.account_id));
                            continue;
                        }
                    };
                    match crate::codex_quota::query(
                        &auth.access_token,
                        &auth.account_id,
                        &auth.credential_id,
                        true,
                    )
                    .await
                    {
                        Ok(windows) => windows,
                        Err(error) => {
                            failures.push(format!("{}: {error}", member.account_id));
                            continue;
                        }
                    }
                }
                Err(error) => {
                    failures.push(format!("{}: {error}", member.account_id));
                    continue;
                }
            };
            if let Some(observation) = quota_pool_observation(&windows, member.weekly_floor) {
                if observation.usable {
                    if *self.quota_pool.read().await != settings {
                        return Err(OAuthError::AuthenticationFailed(
                            "quota pool changed during account selection; retry the request".into(),
                        ));
                    }
                    *self.active_pool_account_id.write().await = Some(auth.credential_id.clone());
                    return Ok(Some(auth));
                }
            } else {
                failures.push(format!(
                    "{}: required 5-hour/weekly windows are missing",
                    member.account_id
                ));
            }
        }

        *self.active_pool_account_id.write().await = None;
        let detail = if failures.is_empty() {
            "all members are paused, throttled, or at their weekly gate".to_string()
        } else {
            failures.join("; ")
        };
        Err(OAuthError::AuthenticationFailed(format!(
            "quota pool has no usable ChatGPT account: {detail}"
        )))
    }

    /// Resolve a valid access token for one specific managed ChatGPT account.
    /// Profile statistics use this to aggregate every account instead of only
    /// the account selected for request routing.
    pub async fn valid_auth_for(&self, account_id: &str) -> Result<AppliedOAuth, OAuthError> {
        if !self.accounts.read().await.contains_key(account_id) {
            return Err(OAuthError::AccountNotFound(account_id.into()));
        }
        let access_token = self.valid_token_for(account_id).await?;
        let workspace_id = self.workspace_id_for(account_id).await?;
        Ok(AppliedOAuth {
            credential_id: account_id.into(),
            account_id: workspace_id,
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
        // Runtime callbacks carry the upstream workspace id, which is not a
        // unique credential selector. Resolve it by the rejected access token
        // when the direct selector lookup misses.
        let account_id = self
            .resolve_credential_selector(account_id, rejected_token)
            .await?;
        let lock = self.refresh_lock(&account_id).await;
        let _guard = lock.lock().await;
        if let Some(cached) = self.access_tokens.read().await.get(&account_id) {
            if cached.access_token != rejected_token && !cached.expiring_soon() {
                return Ok(AppliedOAuth {
                    credential_id: account_id.clone(),
                    account_id: self.workspace_id_for(&account_id).await?,
                    access_token: cached.access_token.clone(),
                    selection_revision: *self.selection_revision.read().await,
                    selection_verified: *self.selection_verified.read().await,
                });
            }
        }
        let access_token = self.refresh_locked(&account_id).await?;
        Ok(AppliedOAuth {
            credential_id: account_id.clone(),
            account_id: self.workspace_id_for(&account_id).await?,
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
        let workspace_id = self.workspace_id_for(&account_id).await?;
        Ok(AppliedOAuth {
            credential_id: account_id,
            account_id: workspace_id,
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

    async fn workspace_id_for(&self, account_id: &str) -> Result<String, OAuthError> {
        self.accounts
            .read()
            .await
            .get(account_id)
            .map(|account| {
                account
                    .chatgpt_account_id
                    .clone()
                    .unwrap_or_else(|| account.account_id.clone())
            })
            .ok_or_else(|| OAuthError::AccountNotFound(account_id.into()))
    }

    async fn resolve_credential_selector(
        &self,
        account_or_workspace_id: &str,
        rejected_token: &str,
    ) -> Result<String, OAuthError> {
        if self
            .accounts
            .read()
            .await
            .contains_key(account_or_workspace_id)
        {
            return Ok(account_or_workspace_id.to_string());
        }
        let tokens = self.access_tokens.read().await;
        let accounts = self.accounts.read().await;
        accounts
            .iter()
            .find(|(credential_id, account)| {
                account.chatgpt_account_id.as_deref() == Some(account_or_workspace_id)
                    && tokens
                        .get(*credential_id)
                        .is_some_and(|cached| cached.access_token == rejected_token)
            })
            .map(|(credential_id, _)| credential_id.clone())
            .ok_or_else(|| OAuthError::AccountNotFound(account_or_workspace_id.into()))
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
        let expected = self
            .accounts
            .read()
            .await
            .get(account_id)
            .cloned()
            .ok_or_else(|| OAuthError::AccountNotFound(account_id.into()))?;
        let refreshed_identity = validate_token_identity(&expected, &tokens)?;
        let changed = {
            let mut accounts = self.accounts.write().await;
            accounts.get_mut(account_id).is_some_and(|account| {
                let mut changed = false;
                if refreshed_identity.email.is_some() && account.email != refreshed_identity.email {
                    account.email = refreshed_identity.email.clone();
                    changed = true;
                }
                if account.chatgpt_account_id.is_none() {
                    account.chatgpt_account_id = Some(refreshed_identity.workspace_id.clone());
                    changed = true;
                }
                if account.workspace_name != refreshed_identity.workspace_name {
                    account.workspace_name = refreshed_identity.workspace_name.clone();
                    changed = true;
                }
                if account.plan_type != refreshed_identity.plan_type {
                    account.plan_type = refreshed_identity.plan_type.clone();
                    changed = true;
                }
                changed
            })
        };
        if changed {
            self.persist().await?;
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
            version: 2,
            accounts,
            default_account_id,
            selection_revision: *self.selection_revision.read().await,
            selected_at: *self.selected_at.read().await,
            selection_verified: *self.selection_verified.read().await,
            quota_pool: self.quota_pool.read().await.clone(),
        };
        write_store(&self.root, &store)
    }
}

#[derive(Debug, Clone, Copy)]
struct QuotaPoolObservation {
    usable: bool,
}

/// Leave enough headroom for the request being admitted now. The upstream
/// usage endpoint reports completed consumption, so treating the last sliver
/// as spendable can select an account whose in-flight request crosses 100%.
const FIVE_HOUR_ROUTING_RESERVE_PERCENT: f64 = 5.0;

fn quota_pool_observation(
    windows: &[crate::model::QuotaSnapshot],
    weekly_floor: u8,
) -> Option<QuotaPoolObservation> {
    let weekly = windows
        .iter()
        .find(|window| window.period.unit == crate::model::QuotaPeriodUnit::Week)?;
    let five_hour = windows.iter().find(|window| {
        window.period.unit == crate::model::QuotaPeriodUnit::Hour && window.period.amount == Some(5)
    })?;
    let weekly_remaining = 100.0 - weekly.used_percent.clamp(0.0, 100.0);
    let five_hour_remaining = 100.0 - five_hour.used_percent.clamp(0.0, 100.0);
    let burnable = (weekly_remaining - f64::from(weekly_floor)).max(0.0);
    Some(QuotaPoolObservation {
        usable: burnable > 0.0 && five_hour_remaining > FIVE_HOUR_ROUTING_RESERVE_PERCENT,
    })
}

/// Read only the account identity from Codex Desktop's native auth file.
/// Token material never leaves this function. Older files that lack the
/// explicit account field fall back to the access-token identity claim.
pub(crate) fn native_codex_account_id(auth_path: &Path) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(auth_path).ok()?).ok()?;
    value
        .pointer("/tokens/access_token")
        .and_then(serde_json::Value::as_str)
        .and_then(vellum_proxy_runtime::chatgpt_identity_from_jwt)
        .map(|identity| identity.credential_id)
        .or_else(|| {
            value
                .pointer("/tokens/account_id")
                .or_else(|| value.pointer("/tokens/accountId"))
                .and_then(serde_json::Value::as_str)
                .filter(|account| !account.trim().is_empty())
                .map(str::to_owned)
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

fn matching_legacy_account(
    accounts: &HashMap<String, AccountMetadata>,
    workspace_id: &str,
    email: Option<&str>,
) -> Option<String> {
    let email = email?.trim();
    if email.is_empty() {
        return None;
    }
    accounts.values().find_map(|account| {
        (account.chatgpt_account_id.is_none()
            && account.account_id == workspace_id
            && account
                .email
                .as_deref()
                .is_some_and(|candidate| candidate.trim().eq_ignore_ascii_case(email)))
        .then(|| account.account_id.clone())
    })
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

fn jwt_exp(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()?
        .get("exp")?
        .as_i64()
}

#[cfg(test)]
fn extract_identity(tokens: &TokenResponse) -> (Option<String>, Option<String>) {
    extract_login_identity(tokens)
        .ok()
        .map_or((None, None), |identity| {
            (Some(identity.workspace_id), identity.email)
        })
}

fn extract_login_identity(
    tokens: &TokenResponse,
) -> Result<vellum_proxy_runtime::ChatGptIdentity, OAuthError> {
    let access_identity = vellum_proxy_runtime::chatgpt_identity_from_jwt(&tokens.access_token)
        .ok_or_else(|| {
            OAuthError::AuthenticationFailed("access token has no verifiable JWT claims".into())
        })?;
    let id_identity = tokens
        .id_token
        .as_deref()
        .map(|token| {
            vellum_proxy_runtime::chatgpt_identity_from_jwt(token).ok_or_else(|| {
                OAuthError::AuthenticationFailed("id token has no verifiable JWT claims".into())
            })
        })
        .transpose()?;
    if let Some(id_identity) = id_identity.as_ref() {
        if id_identity.workspace_id != access_identity.workspace_id
            || !vellum_proxy_runtime::same_chatgpt_principal(id_identity, &access_identity)
        {
            return Err(OAuthError::AuthenticationFailed(
                "id token and access token account identities differ".into(),
            ));
        }
    }
    let mut identity = access_identity;
    if let Some(id_identity) = id_identity {
        identity.email = identity.email.or(id_identity.email);
        identity.workspace_name = identity.workspace_name.or(id_identity.workspace_name);
        identity.plan_type = identity.plan_type.or(id_identity.plan_type);
    }
    Ok(identity)
}

fn validate_token_identity(
    expected: &AccountMetadata,
    tokens: &TokenResponse,
) -> Result<vellum_proxy_runtime::ChatGptIdentity, OAuthError> {
    let actual = extract_login_identity(tokens)?;
    let expected_workspace = expected
        .chatgpt_account_id
        .as_deref()
        .unwrap_or(&expected.account_id);
    let credential_matches =
        expected.chatgpt_account_id.is_none() || actual.credential_id == expected.account_id;
    if actual.workspace_id != expected_workspace || !credential_matches {
        return Err(OAuthError::AuthenticationFailed(
            "token account identity does not match expected credential".to_string(),
        ));
    }
    Ok(actual)
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

    fn quota_window(
        unit: crate::model::QuotaPeriodUnit,
        amount: Option<i64>,
        used_percent: f64,
        reset_at: Option<&str>,
    ) -> crate::model::QuotaSnapshot {
        crate::model::QuotaSnapshot {
            route_id: "account".into(),
            used_percent,
            period: crate::model::QuotaPeriod { unit, amount },
            reset_at: reset_at.map(str::to_owned),
            tier: None,
            stale: false,
        }
    }

    #[test]
    fn quota_pool_gate_uses_weekly_remaining_and_five_hour_throttle() {
        let windows = vec![
            quota_window(crate::model::QuotaPeriodUnit::Hour, Some(5), 62.0, None),
            quota_window(
                crate::model::QuotaPeriodUnit::Week,
                None,
                41.0,
                Some("2026-09-20T00:00:00Z"),
            ),
        ];
        let open = quota_pool_observation(&windows, 30).unwrap();
        assert!(open.usable);

        let gated = quota_pool_observation(&windows, 59).unwrap();
        assert!(!gated.usable);

        let throttled = vec![
            quota_window(crate::model::QuotaPeriodUnit::Hour, Some(5), 100.0, None),
            windows[1].clone(),
        ];
        assert!(!quota_pool_observation(&throttled, 0).unwrap().usable);

        let inside_request_reserve = vec![
            quota_window(crate::model::QuotaPeriodUnit::Hour, Some(5), 96.0, None),
            windows[1].clone(),
        ];
        assert!(
            !quota_pool_observation(&inside_request_reserve, 0)
                .unwrap()
                .usable
        );

        let outside_request_reserve = vec![
            quota_window(crate::model::QuotaPeriodUnit::Hour, Some(5), 94.0, None),
            windows[1].clone(),
        ];
        assert!(
            quota_pool_observation(&outside_request_reserve, 0)
                .unwrap()
                .usable
        );
    }

    #[test]
    fn quota_pool_requires_both_authoritative_windows() {
        let weekly_only = vec![quota_window(
            crate::model::QuotaPeriodUnit::Week,
            None,
            10.0,
            None,
        )];
        assert!(quota_pool_observation(&weekly_only, 0).is_none());
    }

    #[test]
    fn legacy_quota_pool_strategies_migrate_to_member_order() {
        for legacy in ["most", "soonest"] {
            let settings: QuotaPoolSettings = serde_json::from_value(serde_json::json!({
                "enabled": true,
                "strategy": legacy,
                "members": []
            }))
            .unwrap();
            assert_eq!(settings.strategy, QuotaPoolStrategy::Rank);
            assert_eq!(serde_json::to_value(settings).unwrap()["strategy"], "rank");
        }
    }

    #[test]
    fn device_poll_interval_has_safety_margin() {
        assert_eq!(parse_interval(Some(&serde_json::json!(5))), 8);
        assert_eq!(parse_interval(Some(&serde_json::json!("2"))), 5);
    }

    #[test]
    fn identity_supports_namespaced_claim() {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "sub": "user-1",
                "email": "person@example.test",
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "acct-1",
                    "chatgpt_user_id": "user-1"
                }
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
            let payload = URL_SAFE_NO_PAD.encode(
                serde_json::json!({
                    "sub": "user-1",
                    "https://api.openai.com/auth": {
                        "chatgpt_account_id": account,
                        "chatgpt_user_id": "user-1"
                    }
                })
                .to_string(),
            );
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
            access_token: claim(serde_json::json!({
                "sub": "user-1",
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "acct-1",
                    "chatgpt_user_id": "user-1"
                }
            })),
            refresh_token: None,
            id_token: Some(claim(serde_json::json!({
                "sub": "user-1",
                "email": "person@example.test",
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "acct-1",
                    "chatgpt_user_id": "user-1"
                }
            }))),
            expires_in: None,
        });
        assert_eq!(account.as_deref(), Some("acct-1"));
        assert_eq!(email.as_deref(), Some("person@example.test"));
    }

    #[test]
    fn login_identity_separates_users_sharing_one_workspace() {
        let tokens = |user: &str, email: &str| {
            let payload = URL_SAFE_NO_PAD.encode(
                serde_json::json!({
                    "sub": user,
                    "email": email,
                    "https://api.openai.com/auth": {
                        "chatgpt_account_id": "workspace-crypto",
                        "chatgpt_user_id": user
                    }
                })
                .to_string(),
            );
            TokenResponse {
                access_token: format!("header.{payload}.signature"),
                refresh_token: Some(format!("refresh-{user}")),
                id_token: None,
                expires_in: Some(3_600),
            }
        };
        let jp = extract_login_identity(&tokens("user-jp", "jp@example.test")).unwrap();
        let crypto = extract_login_identity(&tokens("user-crypto", "crypto@example.test")).unwrap();
        assert_eq!(jp.workspace_id, crypto.workspace_id);
        assert_ne!(jp.credential_id, crypto.credential_id);
    }

    #[test]
    fn legacy_migration_only_replaces_the_same_user() {
        let accounts = HashMap::from([(
            "workspace-crypto".into(),
            AccountMetadata {
                account_id: "workspace-crypto".into(),
                chatgpt_account_id: None,
                workspace_name: None,
                plan_type: None,
                email: Some("crypto@example.test".into()),
                authenticated_at: 1,
            },
        )]);
        assert_eq!(
            matching_legacy_account(&accounts, "workspace-crypto", Some("CRYPTO@example.test"))
                .as_deref(),
            Some("workspace-crypto")
        );
        assert_eq!(
            matching_legacy_account(&accounts, "workspace-crypto", Some("jp@example.test")),
            None
        );
        assert_eq!(
            matching_legacy_account(&accounts, "workspace-crypto", None),
            None
        );
    }

    #[test]
    fn refresh_identity_must_match_expected_account() {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "sub": "user-other",
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "acct-other",
                    "chatgpt_user_id": "user-other"
                }
            })
            .to_string(),
        );
        let token = format!("header.{payload}.signature");
        let expected = AccountMetadata {
            account_id: "credential-expected".into(),
            chatgpt_account_id: Some("acct-expected".into()),
            workspace_name: None,
            plan_type: None,
            email: None,
            authenticated_at: 1,
        };
        let error = validate_token_identity(
            &expected,
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
    async fn quota_pool_routes_distinct_credentials_in_one_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().to_path_buf());
        let mut members = Vec::new();
        for (id, used) in [("pool-a", 70.0), ("pool-b", 20.0)] {
            manager.accounts.write().await.insert(
                id.into(),
                AccountMetadata {
                    account_id: id.into(),
                    chatgpt_account_id: Some("shared-workspace".into()),
                    workspace_name: None,
                    plan_type: None,
                    email: Some(format!("{id}@example.test")),
                    authenticated_at: 1,
                },
            );
            manager.access_tokens.write().await.insert(
                id.into(),
                CachedToken {
                    access_token: id.into(),
                    expires_at_ms: i64::MAX,
                },
            );
            crate::codex_quota::seed_test_quota(
                id,
                "shared-workspace",
                vec![
                    quota_window(crate::model::QuotaPeriodUnit::Hour, Some(5), 10.0, None),
                    quota_window(crate::model::QuotaPeriodUnit::Week, None, used, None),
                ],
            )
            .await;
            members.push(QuotaPoolMember {
                account_id: id.into(),
                in_pool: true,
                paused: false,
                weekly_floor: 0,
            });
        }
        *manager.default_account_id.write().await = Some("pool-a".into());
        let mut settings = QuotaPoolSettings {
            enabled: true,
            strategy: QuotaPoolStrategy::Rank,
            members,
        };
        manager.set_quota_pool(settings.clone()).await.unwrap();
        assert_eq!(load_store(temp.path()).unwrap().quota_pool, settings);
        let pinned =
            CodexOAuthManager::new_with_default_override(temp.path().to_path_buf(), "pool-a")
                .unwrap();
        assert!(!pinned.quota_pool_status().await.settings.enabled);
        assert_eq!(
            manager
                .valid_routing_auth()
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            "pool-a"
        );
        settings.members.swap(0, 1);
        manager.set_quota_pool(settings.clone()).await.unwrap();
        assert_eq!(
            manager
                .valid_routing_auth()
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            "pool-b"
        );
        settings.members[0].weekly_floor = 80;
        manager.set_quota_pool(settings.clone()).await.unwrap();
        assert_eq!(
            manager
                .valid_routing_auth()
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            "pool-a"
        );
        settings.members[1].paused = true;
        manager.set_quota_pool(settings.clone()).await.unwrap();
        assert!(manager.valid_routing_auth().await.is_err());
        settings.enabled = false;
        manager.set_quota_pool(settings).await.unwrap();
        assert_eq!(
            manager
                .valid_routing_auth()
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            "pool-a"
        );
    }

    #[tokio::test]
    #[ignore = "uses local managed accounts and live upstream quota; no Reset credits"]
    async fn live_quota_pool_routing() {
        // Real grants are refreshed through their original encrypted store.
        // All pool configuration and selection tests run in a temporary manager.
        let root = crate::state::app_data_dir();
        let live = CodexOAuthManager::new(root.clone());
        let original_pool = live.quota_pool_status().await.settings;
        let temp = tempfile::tempdir().unwrap();
        let mut store = load_store(&root).unwrap();
        store.quota_pool = QuotaPoolSettings::default();
        let manager = CodexOAuthManager::from_store(temp.path().to_path_buf(), store);
        let mut members = Vec::new();
        for (index, account) in live.status().await.accounts.iter().enumerate() {
            let auth = match live.valid_auth_for(&account.account_id).await {
                Ok(auth) => auth,
                Err(_) => {
                    eprintln!("account {}: authentication unavailable", index + 1);
                    continue;
                }
            };
            let windows = match crate::codex_quota::query(
                &auth.access_token,
                &auth.account_id,
                &auth.credential_id,
                true,
            )
            .await
            {
                Ok(windows) => windows,
                Err(_) => {
                    eprintln!("account {}: upstream quota unavailable", index + 1);
                    continue;
                }
            };
            let Some(observation) = quota_pool_observation(&windows, 0) else {
                eprintln!("account {}: required windows missing", index + 1);
                continue;
            };
            eprintln!(
                "account {}: live pool usable={}",
                index + 1,
                observation.usable
            );
            manager.access_tokens.write().await.insert(
                auth.credential_id.clone(),
                CachedToken {
                    expires_at_ms: jwt_exp(&auth.access_token).unwrap() * 1000,
                    access_token: auth.access_token,
                },
            );
            if observation.usable {
                members.push(QuotaPoolMember {
                    account_id: auth.credential_id,
                    in_pool: true,
                    paused: false,
                    weekly_floor: 0,
                });
            }
        }
        assert!(
            members.len() >= 2,
            "live multi-account test requires at least two usable accounts"
        );
        let ordered = QuotaPoolSettings {
            enabled: true,
            strategy: QuotaPoolStrategy::Rank,
            members: members.clone(),
        };
        manager.set_quota_pool(ordered.clone()).await.unwrap();
        assert_eq!(load_store(temp.path()).unwrap().quota_pool, ordered);
        let selected = manager.valid_routing_auth().await.unwrap().unwrap();
        assert_eq!(selected.credential_id, members[0].account_id);
        eprintln!("live member-order routing: PASS");
        let mut settings = QuotaPoolSettings {
            enabled: true,
            strategy: QuotaPoolStrategy::Rank,
            members,
        };
        settings.members[0].weekly_floor = 100;
        manager.set_quota_pool(settings.clone()).await.unwrap();
        assert_eq!(
            manager
                .valid_routing_auth()
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            settings.members[1].account_id
        );
        settings.members[0].weekly_floor = 0;
        settings.members[0].paused = true;
        manager.set_quota_pool(settings.clone()).await.unwrap();
        assert_eq!(
            manager
                .valid_routing_auth()
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            settings.members[1].account_id
        );
        for member in &mut settings.members {
            member.weekly_floor = 100;
        }
        manager.set_quota_pool(settings).await.unwrap();
        assert!(manager.valid_routing_auth().await.is_err());
        // Exercise the real Desktop authorization adapter and proxy HTTP path,
        // selecting a different live account on each of two tiny turns.
        let manager = Arc::new(manager);
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("settings.json")).unwrap()).unwrap();
        let routes: Vec<crate::model::Route> =
            serde_json::from_value(config["routes"].clone()).unwrap();
        let state = crate::state::AppState::with_eval_routes_and_oauth(
            temp.path().join("runtime"),
            routes,
            manager.clone(),
        );
        state.activate_proxy_routes();
        let mut review = state.review_settings();
        review.before_send = false;
        review.on_edit = false;
        review.before_compact = false;
        state.set_review_settings(review).unwrap();
        let desktop = crate::proxy_runtime_bridge::DesktopProxyRuntimeState::new(state).unwrap();
        use vellum_proxy_runtime::ProxyRuntimeState;
        let model = desktop
            .active_model_routes()
            .into_iter()
            .find(|route| {
                route.route_id == "openai-official"
                    && vellum_proxy_runtime::official_catalog_is_luna(&route.upstream_model)
            })
            .expect("live Official Luna catalog required");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let boundary = vellum_proxy_runtime::BoundaryKey::generate().unwrap();
        let router = vellum_proxy_runtime::build_headless_router(
            Arc::new(desktop),
            vellum_proxy_runtime::InboundAccessPolicy::authenticated(
                vellum_proxy_runtime::BOUNDARY_CREDENTIAL_ID,
                boundary.clone(),
                address.port(),
            ),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        for selected_index in 0..2 {
            let mut pool = manager.quota_pool_status().await.settings;
            for (index, member) in pool.members.iter_mut().enumerate() {
                member.paused = false;
                member.weekly_floor = if index == selected_index { 0 } else { 100 };
            }
            let expected = pool.members[selected_index].account_id.clone();
            manager.set_quota_pool(pool).await.unwrap();
            let response = reqwest::Client::new()
                .post(format!("http://{address}/v1/responses"))
                .header(
                    vellum_proxy_runtime::BOUNDARY_KEY_HEADER,
                    boundary.expose_for_storage(),
                )
                .timeout(std::time::Duration::from_secs(60))
                .json(&vellum_proxy_runtime::official_live_http_body(
                    &model.catalog_id,
                    &vellum_proxy_runtime::official_live_marker_input("POOL_OK"),
                ))
                .send()
                .await
                .unwrap();
            assert!(
                response.status().is_success(),
                "live proxy HTTP {}",
                response.status()
            );
            let body = response.text().await.unwrap();
            assert!(
                body.contains("response.completed") && body.contains("POOL_OK"),
                "live turn did not complete with marker"
            );
            assert_eq!(
                manager
                    .quota_pool_status()
                    .await
                    .active_account_id
                    .as_deref(),
                Some(expected.as_str())
            );
            eprintln!("live proxy inference account {}: PASS", selected_index + 1);
        }
        server.abort();
        assert_eq!(load_store(&root).unwrap().quota_pool, original_pool);
        eprintln!("live gate, pause, exhaustion, persistence: PASS; production pool unchanged");
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
                    chatgpt_account_id: None,
                    workspace_name: None,
                    plan_type: None,
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
    async fn explicitly_reselecting_implicit_default_publishes_selection_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().to_path_buf());
        manager.accounts.write().await.insert(
            "acct-a".into(),
            AccountMetadata {
                account_id: "acct-a".into(),
                chatgpt_account_id: None,
                workspace_name: None,
                plan_type: None,
                email: None,
                authenticated_at: 1,
            },
        );
        manager.access_tokens.write().await.insert(
            "acct-a".into(),
            CachedToken {
                access_token: "token-acct-a".into(),
                expires_at_ms: i64::MAX,
            },
        );
        *manager.default_account_id.write().await = Some("acct-a".into());
        manager.persist().await.unwrap();

        manager.set_default("acct-a").await.unwrap();

        let selected = manager.status().await;
        assert_eq!(selected.default_account_id.as_deref(), Some("acct-a"));
        assert_eq!(selected.selection_revision, 1);
        assert!(selected.selected_at.is_some());
        assert!(selected.selection_verified);
        let persisted: serde_json::Value = serde_json::from_slice(
            &std::fs::read(metadata_path(temp.path())).unwrap(),
        )
        .unwrap();
        assert_eq!(persisted["default_account_id"], "acct-a");
        assert_eq!(persisted["selection_revision"], 1);
        assert!(persisted["selected_at"].as_i64().is_some());
        assert_eq!(persisted["selection_verified"], true);
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
                    chatgpt_account_id: None,
                    workspace_name: None,
                    plan_type: None,
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
                    chatgpt_account_id: None,
                    workspace_name: None,
                    plan_type: None,
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
                chatgpt_account_id: None,
                workspace_name: None,
                plan_type: None,
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
