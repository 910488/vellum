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

// Fetch a bounded group concurrently, retaining policy order regardless of
// upstream completion order. Rank routing does not use this batch collector.
async fn collect_balanced_quota_reads<T, F>(reads: Vec<F>) -> Vec<T>
where
    F: std::future::Future<Output = T>,
{
    use futures_util::StreamExt;
    futures_util::stream::iter(reads)
        .buffered(4)
        .collect()
        .await
}

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
    Balanced,
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
    /// Opt-in cadence keeper. Independent from pool membership so a manually
    /// selected account can keep its five-hour window warm too.
    #[serde(default)]
    pub maintain_five_hour_window: bool,
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
    smart_router: parking_lot::Mutex<crate::quota_pool_router::SmartRouter>,
    exhausted_pool_accounts: parking_lot::Mutex<HashMap<String, u64>>,
    pool_reset_times: parking_lot::Mutex<HashMap<String, u64>>,
    pool_diagnostics: parking_lot::Mutex<crate::quota_pool_diagnostics::DecisionObserver>,
    selection_lock: Mutex<()>,
    access_tokens: RwLock<HashMap<String, CachedToken>>,
    refresh_locks: RwLock<HashMap<String, Arc<Mutex<()>>>>,
    pending: RwLock<HashMap<String, PendingDevice>>,
    cadence_notify: Arc<tokio::sync::Notify>,
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
            smart_router: parking_lot::Mutex::new(crate::quota_pool_router::SmartRouter::default()),
            exhausted_pool_accounts: parking_lot::Mutex::new(HashMap::new()),
            pool_reset_times: parking_lot::Mutex::new(HashMap::new()),
            pool_diagnostics: parking_lot::Mutex::new(Default::default()),
            selection_lock: Mutex::const_new(()),
            access_tokens: RwLock::new(HashMap::new()),
            refresh_locks: RwLock::new(HashMap::new()),
            pending: RwLock::new(HashMap::new()),
            cadence_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub(crate) fn set_pool_diagnostics_sink(
        &self,
        sink: Arc<dyn vellum_proxy_runtime::DiagnosticsSink>,
    ) {
        self.pool_diagnostics.lock().set_sink(sink);
    }

    fn record_pool_diagnostic(
        &self,
        selected: Option<&str>,
        reason: &str,
        candidates: Vec<vellum_proxy_runtime::QuotaPoolCandidateDiagnostic>,
    ) {
        self.pool_diagnostics
            .lock()
            .record(vellum_proxy_runtime::QuotaPoolDecision {
                selected_account_hash: selected.map(pool_account_hash),
                reason: reason.into(),
                candidates,
            });
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
            {
                let mut pool = self.quota_pool.write().await;
                migrate_quota_pool_member(&mut pool, legacy_account_id, &account_id);
            }
            if self.active_pool_account_id.read().await.as_deref()
                == Some(legacy_account_id.as_str())
            {
                *self.active_pool_account_id.write().await = Some(account_id.clone());
            }
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

    pub fn cadence_notifier(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.cadence_notify)
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
        let current = self.quota_pool.read().await;
        if !current.enabled {
            *self.active_pool_account_id.write().await = None;
        }
        if !current.enabled || current.strategy != previous.strategy {
            self.smart_router.lock().clear();
        }
        drop(current);
        self.cadence_notify.notify_one();
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
        let result = self.persist().await;
        self.cadence_notify.notify_one();
        result
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
        let result = self.persist().await;
        self.cadence_notify.notify_one();
        result
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
    /// member, quota readings choose the automatic rotation order. When no
    /// account passes those readings, an explicit manual selection can still
    /// reach upstream and receive its actual usage-limit response.
    /// Auto Review calls `valid_auth_for` and remains explicitly billed.
    pub async fn valid_routing_auth(&self) -> Result<Option<AppliedOAuth>, OAuthError> {
        self.valid_routing_auth_with_context(&Default::default())
            .await
    }

    pub fn observe_routing_usage(&self, request_id: &str, input: u64, cached: u64, status: u16) {
        self.smart_router
            .lock()
            .observe(request_id, input, cached, status);
    }

    pub async fn valid_routing_auth_with_context(
        &self,
        context: &vellum_proxy_runtime::official_auth::OfficialRoutingContext,
    ) -> Result<Option<AppliedOAuth>, OAuthError> {
        self.routing_auth(context, false).await
    }

    pub async fn retry_pool_after_quota(
        &self,
        account_hash: &str,
        reset_at: Option<u64>,
        context: &vellum_proxy_runtime::official_auth::OfficialRoutingContext,
        allow_retry: bool,
    ) -> Result<Option<AppliedOAuth>, OAuthError> {
        let settings = self.quota_pool.read().await.clone();
        if !settings.enabled
            || !settings
                .members
                .iter()
                .any(|m| m.in_pool && pool_account_hash(&m.account_id) == account_hash)
        {
            return Ok(None);
        }
        let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
        // Prefer the provider's reset. Without one, use the observed window
        // reset retained by routing, or a conservative five-hour window.
        let until = reset_at.filter(|t| *t > now).unwrap_or_else(|| {
            self.pool_reset_times
                .lock()
                .get(account_hash)
                .copied()
                .filter(|t| *t > now)
                .unwrap_or(now + 18_000_000)
        });
        self.exhausted_pool_accounts
            .lock()
            .insert(account_hash.to_owned(), until);
        if !allow_retry {
            return Ok(None);
        }
        let mut context = context.clone();
        context.socket_affinity = None;
        self.routing_auth(&context, true).await
    }

    async fn routing_auth(
        &self,
        context: &vellum_proxy_runtime::official_auth::OfficialRoutingContext,
        require_fit: bool,
    ) -> Result<Option<AppliedOAuth>, OAuthError> {
        let settings = self.quota_pool.read().await.clone();
        let selection_id = ulid::Ulid::new();
        if settings.enabled {
            if let Some(affinity) = context
                .socket_affinity
                .as_ref()
                .filter(|affinity| affinity.turn_in_progress)
            {
                let account = self
                    .accounts
                    .read()
                    .await
                    .keys()
                    .find(|id| affinity.is_account(id))
                    .cloned();
                if let Some(account) = account {
                    // Register the invocation even when no quota lookup is
                    // allowed to move this root turn. Its eventual usage must
                    // still feed load and capacity observations.
                    let plan = self
                        .accounts
                        .read()
                        .await
                        .get(&account)
                        .and_then(|a| a.plan_type.clone());
                    self.smart_router.lock().choose(
                        &[crate::quota_pool_router::Candidate {
                            account_id: account.clone(),
                            capacity_prior: crate::quota_pool_router::plan_capacity_prior(
                                plan.as_deref(),
                            ),
                            plan_type: plan,
                            headroom: 100.0,
                            ..Default::default()
                        }],
                        context,
                        chrono::Utc::now().timestamp_millis().max(0) as u64,
                    );
                    log::info!("[QuotaPool] selection={selection_id} reason=active_turn_affinity account={} context_tokens={}", affinity.account_hash, context.estimated_input_tokens);
                    return self.valid_auth_for(&account).await.map(Some);
                }
                return Err(OAuthError::AuthenticationFailed("the active turn's execution account is no longer available; retry at a turn boundary".into()));
            }
        }
        if !settings.enabled || !settings.members.iter().any(|member| member.in_pool) {
            if require_fit {
                return Ok(None);
            }
            log::info!(
                "[QuotaPool] selection={selection_id} event=bypass reason={}",
                if settings.enabled {
                    "empty_pool"
                } else {
                    "disabled"
                }
            );
            *self.active_pool_account_id.write().await = None;
            return self.valid_default_auth().await;
        }

        // Balanced needs every eligible candidate. Read independent members
        // concurrently instead of adding each cold quota round-trip to dispatch
        // latency. Buffered preserves member order for ties and manual fallback;
        // rank mode retains its first-usable-member short circuit.
        let balanced_reads = if settings.strategy == QuotaPoolStrategy::Balanced {
            let reads = settings
                .members
                .iter()
                .filter(|member| member.in_pool && !member.paused)
                .map(|member| {
                    let account_id = member.account_id.clone();
                    async move {
                        let auth = self.valid_auth_for(&account_id).await;
                        let quota = match &auth {
                            Ok(auth) => Some(
                                crate::codex_quota::query_with_cache_ttl(
                                    &auth.access_token,
                                    &auth.account_id,
                                    &auth.credential_id,
                                    false,
                                    crate::codex_quota::ROUTING_QUOTA_CACHE_TTL,
                                )
                                .await,
                            ),
                            Err(_) => None,
                        };
                        (auth, quota)
                    }
                })
                .collect::<Vec<_>>();
            collect_balanced_quota_reads(reads).await
        } else {
            Vec::new()
        };
        let mut balanced_reads = balanced_reads.into_iter();

        let mut diagnostic_candidates = if settings.strategy == QuotaPoolStrategy::Balanced {
            let plans = self.accounts.read().await;
            let router = self.smart_router.lock();
            settings
                .members
                .iter()
                .filter(|m| m.in_pool)
                .map(|m| {
                    let prior = crate::quota_pool_router::plan_capacity_prior(
                        plans
                            .get(&m.account_id)
                            .and_then(|a| a.plan_type.as_deref()),
                    );
                    let (capacity, measured, external, low) =
                        router.diagnostic_capacity(&m.account_id, prior);
                    let (_, _, five_external, five_low) =
                        router.diagnostic_five_capacity(&m.account_id, capacity);
                    vellum_proxy_runtime::QuotaPoolCandidateDiagnostic {
                        account_hash: pool_account_hash(&m.account_id),
                        exclusion_reason: Some("quota_unavailable".into()),
                        headroom: None,
                        weekly_used: None,
                        five_hour_used: None,
                        weekly_reset_at: None,
                        five_hour_reset_at: None,
                        tokens_per_point: capacity,
                        capacity_source: if measured { "measured" } else { "prior" }.into(),
                        external_points: external,
                        low_sample_count: low,
                        five_hour_tokens_per_point: None,
                        five_hour_capacity_source: None,
                        five_hour_external_points: five_external,
                        five_hour_low_sample_count: five_low,
                        request_budget_fits: None,
                        request_budget_enforced: false,
                    }
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let mut failures = Vec::new();
        let mut rank = 0;
        let mut fallback_auth = None;
        let mut eligible = Vec::new();
        let mut routing_members = settings.members.iter().collect::<Vec<_>>();
        if settings.strategy == QuotaPoolStrategy::Rank {
            if let Some(affinity) = context.socket_affinity.as_ref() {
                routing_members.sort_by_key(|member| !affinity.is_account(&member.account_id));
            }
        }
        for member in routing_members {
            if !member.in_pool {
                continue;
            }
            rank += 1;
            if self
                .exhausted_pool_accounts
                .lock()
                .get(&pool_account_hash(&member.account_id))
                .is_some_and(|until| *until > chrono::Utc::now().timestamp_millis().max(0) as u64)
            {
                log_pool_candidate(
                    &mut diagnostic_candidates,
                    selection_id,
                    "skip",
                    &member.account_id,
                    rank,
                    "upstream_quota_until_reset",
                );
                if settings.strategy == QuotaPoolStrategy::Balanced && !member.paused {
                    let _ = balanced_reads.next();
                }
                continue;
            }
            if context.socket_affinity.as_ref().is_some_and(|affinity| {
                !affinity.turn_in_progress
                    && affinity.quota_exhausted
                    && affinity.is_account(&member.account_id)
            }) {
                // A usage-limit frame is authoritative even while the cached
                // quota snapshot is still below its gate. Capacity 429s do not
                // set this marker, and the running root turn is never moved.
                log_pool_candidate(
                    &mut diagnostic_candidates,
                    selection_id,
                    "skip",
                    &member.account_id,
                    rank,
                    "upstream_usage_limit",
                );
                if settings.strategy == QuotaPoolStrategy::Balanced && !member.paused {
                    let _ = balanced_reads.next();
                }
                continue;
            }
            if member.paused {
                log_pool_candidate(
                    &mut diagnostic_candidates,
                    selection_id,
                    "skip",
                    &member.account_id,
                    rank,
                    "paused",
                );
                continue;
            }
            let (auth_result, quota_result) = match balanced_reads.next() {
                Some(read) => read,
                None => (self.valid_auth_for(&member.account_id).await, None),
            };
            let mut auth = match auth_result {
                Ok(auth) => auth,
                Err(error) => {
                    log_pool_candidate(
                        &mut diagnostic_candidates,
                        selection_id,
                        "skip",
                        &member.account_id,
                        rank,
                        "auth_unavailable",
                    );
                    failures.push(format!("{}: {error}", member.account_id));
                    continue;
                }
            };
            if fallback_auth.is_none() {
                fallback_auth = Some(auth.clone());
            }
            let quota_result = match quota_result {
                Some(result) => result,
                None => {
                    crate::codex_quota::query_with_cache_ttl(
                        &auth.access_token,
                        &auth.account_id,
                        &auth.credential_id,
                        false,
                        crate::codex_quota::ROUTING_QUOTA_CACHE_TTL,
                    )
                    .await
                }
            };
            let windows = match quota_result {
                Ok(windows) => windows,
                Err(crate::codex_quota::CodexQuotaError::Unauthorized) => {
                    log_pool_candidate(
                        &mut diagnostic_candidates,
                        selection_id,
                        "retry",
                        &member.account_id,
                        rank,
                        "quota_unauthorized",
                    );
                    auth = match self
                        .refresh_after_rejection(&auth.credential_id, &auth.access_token)
                        .await
                    {
                        Ok(refreshed) => refreshed,
                        Err(error) => {
                            log_pool_candidate(
                                &mut diagnostic_candidates,
                                selection_id,
                                "skip",
                                &member.account_id,
                                rank,
                                "refresh_failed",
                            );
                            failures.push(format!("{}: {error}", member.account_id));
                            continue;
                        }
                    };
                    if fallback_auth
                        .as_ref()
                        .is_some_and(|fallback: &AppliedOAuth| {
                            fallback.credential_id == auth.credential_id
                        })
                    {
                        fallback_auth = Some(auth.clone());
                    }
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
                            log_pool_candidate(
                                &mut diagnostic_candidates,
                                selection_id,
                                "skip",
                                &member.account_id,
                                rank,
                                "quota_query_failed",
                            );
                            failures.push(format!("{}: {error}", member.account_id));
                            continue;
                        }
                    }
                }
                Err(error) => {
                    log_pool_candidate(
                        &mut diagnostic_candidates,
                        selection_id,
                        "skip",
                        &member.account_id,
                        rank,
                        "quota_query_failed",
                    );
                    failures.push(format!("{}: {error}", member.account_id));
                    continue;
                }
            };
            if let Some(observation) = quota_pool_observation(&windows, member.weekly_floor) {
                let reset = if observation.five_hour_used.is_some_and(|used| used >= 90.0) {
                    observation.five_hour_reset_at
                } else if observation.weekly_headroom <= 5.0 {
                    observation.weekly_reset_at
                } else {
                    observation
                        .five_hour_reset_at
                        .or(observation.weekly_reset_at)
                };
                if let Some(reset) = reset {
                    self.pool_reset_times
                        .lock()
                        .insert(pool_account_hash(&member.account_id), reset);
                }
                if settings.strategy == QuotaPoolStrategy::Balanced {
                    // Reconcile resets even when this account is still gated.
                    let plan_type = self
                        .accounts
                        .read()
                        .await
                        .get(&member.account_id)
                        .and_then(|a| a.plan_type.clone());
                    let observed = crate::quota_pool_router::Candidate {
                        account_id: member.account_id.clone(),
                        headroom: observation.headroom,
                        capacity_prior: crate::quota_pool_router::plan_capacity_prior(
                            plan_type.as_deref(),
                        ),
                        plan_type,
                        weekly_used: Some(observation.weekly_used),
                        weekly_headroom: Some(observation.weekly_headroom),
                        weekly_reset_at: observation.weekly_reset_at,
                        five_hour_used: observation.five_hour_used,
                        five_hour_reset_at: observation.five_hour_reset_at,
                    };
                    let (capacity, measured, external, low) = {
                        let mut router = self.smart_router.lock();
                        router.update_quota_observation(&observed);
                        router.diagnostic_capacity(&member.account_id, observed.capacity_prior)
                    };
                    if let Some(d) = diagnostic_candidates
                        .iter_mut()
                        .find(|d| d.account_hash == pool_account_hash(&member.account_id))
                    {
                        d.exclusion_reason = observation.skip_reason.map(str::to_owned);
                        d.tokens_per_point = capacity;
                        d.capacity_source = if measured { "measured" } else { "prior" }.into();
                        d.external_points = external;
                        d.low_sample_count = low;
                        d.headroom = Some(observation.headroom);
                        d.weekly_used = Some(observation.weekly_used);
                        d.five_hour_used = observation.five_hour_used;
                        if observation.five_hour_used.is_some() {
                            let (capacity, measured, external, low) = self
                                .smart_router
                                .lock()
                                .diagnostic_five_capacity(&member.account_id, d.tokens_per_point);
                            d.five_hour_tokens_per_point = Some(capacity);
                            d.five_hour_capacity_source =
                                Some(if measured { "measured" } else { "prior" }.into());
                            d.five_hour_external_points = external;
                            d.five_hour_low_sample_count = low;
                        }
                        d.weekly_reset_at = observation.weekly_reset_at;
                        d.five_hour_reset_at = observation.five_hour_reset_at;
                    }
                }
                if observation.usable {
                    if settings.strategy == QuotaPoolStrategy::Balanced || require_fit {
                        eligible.push((auth, rank, observation));
                        continue;
                    }
                    if *self.quota_pool.read().await != settings {
                        log_pool_candidate(
                            &mut diagnostic_candidates,
                            selection_id,
                            "reject",
                            &member.account_id,
                            rank,
                            "settings_changed",
                        );
                        return Err(OAuthError::AuthenticationFailed(
                            "quota pool changed during account selection; retry the request".into(),
                        ));
                    }
                    *self.active_pool_account_id.write().await = Some(auth.credential_id.clone());
                    log_pool_candidate(
                        &mut diagnostic_candidates,
                        selection_id,
                        "selected",
                        &auth.credential_id,
                        rank,
                        "usable",
                    );
                    return Ok(Some(auth));
                }
                log_pool_candidate(
                    &mut diagnostic_candidates,
                    selection_id,
                    "skip",
                    &member.account_id,
                    rank,
                    observation.skip_reason.unwrap_or("quota_unavailable"),
                );
            } else {
                log_pool_candidate(
                    &mut diagnostic_candidates,
                    selection_id,
                    "skip",
                    &member.account_id,
                    rank,
                    "missing_quota_windows",
                );
                failures.push(format!(
                    "{}: required weekly window is missing",
                    member.account_id
                ));
            }
        }

        if !eligible.is_empty() {
            let _selection_guard = self.selection_lock.lock().await;
            if *self.quota_pool.read().await != settings {
                return Err(OAuthError::AuthenticationFailed(
                    "quota pool changed during account selection; retry the request".into(),
                ));
            }
            let plans = self.accounts.read().await;
            let candidates = eligible
                .iter()
                .map(
                    |(auth, _, observation)| crate::quota_pool_router::Candidate {
                        account_id: auth.credential_id.clone(),
                        headroom: observation.headroom,
                        plan_type: plans
                            .get(&auth.credential_id)
                            .and_then(|a| a.plan_type.clone()),
                        capacity_prior: crate::quota_pool_router::plan_capacity_prior(
                            plans
                                .get(&auth.credential_id)
                                .and_then(|account| account.plan_type.as_deref()),
                        ),
                        weekly_used: Some(observation.weekly_used),
                        weekly_headroom: Some(observation.weekly_headroom),
                        weekly_reset_at: observation.weekly_reset_at,
                        five_hour_used: observation.five_hour_used,
                        five_hour_reset_at: observation.five_hour_reset_at,
                    },
                )
                .collect::<Vec<_>>();
            drop(plans);
            let (index, reason) = {
                let mut router = self.smart_router.lock();
                let result = router.choose(
                    &candidates,
                    context,
                    chrono::Utc::now().timestamp_millis().max(0) as u64,
                );
                if require_fit && router.last_admission.get(result.0) != Some(&true) {
                    return Ok(None);
                }
                for (i, candidate) in candidates.iter().enumerate() {
                    if let Some(d) = diagnostic_candidates
                        .iter_mut()
                        .find(|d| d.account_hash == pool_account_hash(&candidate.account_id))
                    {
                        let (capacity, measured, external, low) = router
                            .diagnostic_capacity(&candidate.account_id, candidate.capacity_prior);
                        d.tokens_per_point = capacity;
                        d.capacity_source = if measured { "measured" } else { "prior" }.into();
                        d.external_points = external;
                        d.low_sample_count = low;
                        if candidate.five_hour_used.is_some() {
                            let (five_capacity, five_measured, external, low) =
                                router.diagnostic_five_capacity(&candidate.account_id, capacity);
                            d.five_hour_tokens_per_point = Some(five_capacity);
                            d.five_hour_capacity_source =
                                Some(if five_measured { "measured" } else { "prior" }.into());
                            d.five_hour_external_points = external;
                            d.five_hour_low_sample_count = low;
                        }
                        d.request_budget_fits = router.last_admission.get(i).copied();
                        d.request_budget_enforced = router.budget_enforced;
                        if router.budget_enforced && d.request_budget_fits == Some(false) {
                            d.exclusion_reason = Some("gate_request_budget".into());
                        }
                    }
                }
                result
            };
            let (auth, rank, _) = eligible.swap_remove(index);
            *self.active_pool_account_id.write().await = Some(auth.credential_id.clone());
            log_pool_candidate(
                &mut diagnostic_candidates,
                selection_id,
                "selected",
                &auth.credential_id,
                rank,
                reason,
            );
            self.record_pool_diagnostic(Some(&auth.credential_id), reason, diagnostic_candidates);
            log::info!("[QuotaPool] selection={selection_id} strategy=balanced context_tokens={} context_limit={}",
                context.estimated_input_tokens, context.context_window.unwrap_or(0));
            return Ok(Some(auth));
        }

        if require_fit {
            return Ok(None);
        }

        // The usage endpoint is advisory. A depleted or unreadable snapshot
        // must not prevent a valid credential from reaching the provider.
        // Prefer the user's manual selection, including an account outside the
        // pool, but do not override an explicit pause on a pooled member.
        let manual = self.resolve_default().await;
        let manual_paused = manual.as_ref().is_some_and(|account_id| {
            if self
                .exhausted_pool_accounts
                .lock()
                .get(&pool_account_hash(account_id))
                .is_some_and(|until| *until > chrono::Utc::now().timestamp_millis().max(0) as u64)
            {
                return true;
            }
            settings
                .members
                .iter()
                .any(|member| member.account_id == *account_id && member.in_pool && member.paused)
        });
        let fallback = if manual_paused {
            fallback_auth
        } else if let Some(account_id) = manual {
            match self.valid_auth_for(&account_id).await {
                Ok(auth) => Some(auth),
                Err(error) => {
                    failures.push(format!("{account_id}: {error}"));
                    fallback_auth
                }
            }
        } else {
            fallback_auth
        };
        if let Some(auth) = fallback {
            let _selection_guard = self.selection_lock.lock().await;
            if *self.quota_pool.read().await != settings {
                return Err(OAuthError::AuthenticationFailed(
                    "quota pool changed during account selection; retry the request".into(),
                ));
            }
            if settings.strategy == QuotaPoolStrategy::Balanced {
                let plan_type = self
                    .accounts
                    .read()
                    .await
                    .get(&auth.credential_id)
                    .and_then(|a| a.plan_type.clone());
                self.smart_router.lock().choose(
                    &[crate::quota_pool_router::Candidate {
                        account_id: auth.credential_id.clone(),
                        headroom: 0.0,
                        capacity_prior: crate::quota_pool_router::plan_capacity_prior(
                            plan_type.as_deref(),
                        ),
                        plan_type,
                        weekly_used: None,
                        ..Default::default()
                    }],
                    context,
                    chrono::Utc::now().timestamp_millis().max(0) as u64,
                );
            }
            *self.active_pool_account_id.write().await = Some(auth.credential_id.clone());
            let fallback_rank = settings
                .members
                .iter()
                .filter(|member| member.in_pool)
                .position(|member| member.account_id == auth.credential_id)
                .map_or(0, |index| index + 1);
            log_pool_candidate(
                &mut diagnostic_candidates,
                selection_id,
                "selected",
                &auth.credential_id,
                fallback_rank,
                "manual_fallback",
            );
            if settings.strategy == QuotaPoolStrategy::Balanced {
                self.record_pool_diagnostic(
                    Some(&auth.credential_id),
                    "manual_fallback",
                    diagnostic_candidates,
                );
            }
            return Ok(Some(auth));
        }

        if settings.strategy == QuotaPoolStrategy::Balanced {
            self.record_pool_diagnostic(None, "no_valid_credential", diagnostic_candidates);
        }
        *self.active_pool_account_id.write().await = None;
        log::info!(
            "[QuotaPool] selection={selection_id} event=exhausted reason=no_valid_credential"
        );
        let detail = if failures.is_empty() {
            "all members are paused or have no valid credential".to_string()
        } else {
            failures.join("; ")
        };
        Err(OAuthError::AuthenticationFailed(format!(
            "quota pool has no available ChatGPT credential: {detail}"
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
        let tokens = self.access_tokens.read().await;
        let accounts = self.accounts.read().await;
        if let Some((credential_id, _)) = accounts.iter().find(|(credential_id, account)| {
            (credential_id.as_str() == account_or_workspace_id
                || account.chatgpt_account_id.as_deref() == Some(account_or_workspace_id))
                && tokens
                    .get(*credential_id)
                    .is_some_and(|cached| cached.access_token == rejected_token)
        }) {
            return Ok(credential_id.clone());
        }
        // The direct selector remains useful when this process has no cached
        // token, but an exact token match wins if a legacy row's key equals
        // another user's workspace id.
        accounts
            .contains_key(account_or_workspace_id)
            .then(|| account_or_workspace_id.to_string())
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
    skip_reason: Option<&'static str>,
    headroom: f64,
    weekly_used: f64,
    weekly_headroom: f64,
    weekly_reset_at: Option<u64>,
    five_hour_used: Option<f64>,
    five_hour_reset_at: Option<u64>,
}

fn pool_account_hash(account_id: &str) -> String {
    vellum_proxy_runtime::account_hash(account_id)
}

fn log_pool_candidate(
    candidates: &mut [vellum_proxy_runtime::QuotaPoolCandidateDiagnostic],
    selection_id: ulid::Ulid,
    event: &'static str,
    account_id: &str,
    rank: usize,
    reason: &'static str,
) {
    if event == "skip" {
        if let Some(c) = candidates
            .iter_mut()
            .find(|c| c.account_hash == pool_account_hash(account_id))
        {
            c.exclusion_reason = Some(reason.into());
        }
    }
    log_pool_decision(selection_id, event, account_id, rank, reason);
}

fn log_pool_decision(
    selection_id: ulid::Ulid,
    event: &'static str,
    account_id: &str,
    rank: usize,
    reason: &'static str,
) {
    log::info!(
        "{}",
        pool_decision_message(selection_id, event, account_id, rank, reason)
    );
}

fn pool_decision_message(
    selection_id: ulid::Ulid,
    event: &'static str,
    account_id: &str,
    rank: usize,
    reason: &'static str,
) -> String {
    let digest = format!("{:x}", Sha256::digest(account_id.as_bytes()));
    format!(
        "[QuotaPool] selection={selection_id} event={event} account={} rank={rank} reason={reason}",
        &digest[..12]
    )
}

/// Leave enough headroom for the request being admitted now. The upstream
/// usage endpoint reports completed consumption, so treating the last sliver
/// as spendable can select an account whose in-flight request crosses 100%.
const FIVE_HOUR_ROUTING_RESERVE_PERCENT: f64 = 5.0;

fn quota_reset_ms(window: &crate::model::QuotaSnapshot) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(window.reset_at.as_deref()?)
        .ok()
        .map(|t| t.timestamp_millis().max(0) as u64)
}

fn quota_pool_observation(
    windows: &[crate::model::QuotaSnapshot],
    weekly_floor: u8,
) -> Option<QuotaPoolObservation> {
    quota_pool_observation_at(
        windows,
        weekly_floor,
        chrono::Utc::now().timestamp_millis().max(0) as u64,
    )
}

fn quota_pool_observation_at(
    windows: &[crate::model::QuotaSnapshot],
    weekly_floor: u8,
    now: u64,
) -> Option<QuotaPoolObservation> {
    let weekly = windows
        .iter()
        .find(|window| window.period.unit == crate::model::QuotaPeriodUnit::Week)?;
    let five_hour = windows.iter().find(|window| {
        window.period.unit == crate::model::QuotaPeriodUnit::Hour && window.period.amount == Some(5)
    });
    let weekly_remaining = 100.0 - weekly.used_percent.clamp(0.0, 100.0);
    let five_hour_remaining = five_hour.map(|window| 100.0 - window.used_percent.clamp(0.0, 100.0));
    let burnable = (weekly_remaining - f64::from(weekly_floor)).max(0.0);
    let skip_reason = if burnable <= 0.0 {
        Some(
            if quota_reset_ms(weekly).is_some_and(|reset| reset <= now) {
                "weekly_reset_pending"
            } else {
                "weekly_floor"
            },
        )
    } else if five_hour_remaining
        .is_some_and(|remaining| remaining <= FIVE_HOUR_ROUTING_RESERVE_PERCENT)
    {
        Some(
            if five_hour
                .and_then(quota_reset_ms)
                .is_some_and(|reset| reset <= now)
            {
                "five_hour_reset_pending"
            } else {
                "five_hour_reserve"
            },
        )
    } else {
        None
    };
    Some(QuotaPoolObservation {
        usable: skip_reason.is_none(),
        skip_reason,
        weekly_used: weekly.used_percent.clamp(0.0, 100.0),
        weekly_headroom: burnable,
        weekly_reset_at: quota_reset_ms(weekly),
        five_hour_used: five_hour.map(|w| w.used_percent.clamp(0.0, 100.0)),
        five_hour_reset_at: five_hour.and_then(quota_reset_ms),
        // Compare remaining fractions rather than raw tokens across plans.
        // Reserve/floor headroom is normalized to each spendable window.
        headroom: (burnable / (100.0 - f64::from(weekly_floor)).max(1.0) * 100.0).min(
            five_hour_remaining.map_or(100.0, |remaining| {
                ((remaining - FIVE_HOUR_ROUTING_RESERVE_PERCENT).max(0.0)
                    / (100.0 - FIVE_HOUR_ROUTING_RESERVE_PERCENT))
                    * 100.0
            }),
        ),
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

fn migrate_quota_pool_member(settings: &mut QuotaPoolSettings, old_id: &str, new_id: &str) {
    let Some(old_index) = settings
        .members
        .iter()
        .position(|member| member.account_id == old_id)
    else {
        return;
    };
    let mut old = settings.members.remove(old_index);
    if let Some(existing) = settings
        .members
        .iter_mut()
        .find(|member| member.account_id == new_id)
    {
        // An already configured new row keeps its order. Preserve an active
        // legacy membership if that new row has not joined the pool yet.
        if old.in_pool && !existing.in_pool {
            existing.in_pool = true;
            existing.paused = old.paused;
            existing.weekly_floor = old.weekly_floor;
        }
        existing.maintain_five_hour_window |= old.maintain_five_hour_window;
    } else {
        old.account_id = new_id.to_string();
        settings.members.insert(old_index, old);
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
            luna_reserve: None,
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

        // Keep the historical fixture before its reset; elapsed snapshots
        // now have a distinct diagnostic reason without changing the gate.
        let gated = quota_pool_observation_at(&windows, 59, 0).unwrap();
        assert!(!gated.usable);
        assert_eq!(gated.skip_reason, Some("weekly_floor"));

        let throttled = vec![
            quota_window(crate::model::QuotaPeriodUnit::Hour, Some(5), 100.0, None),
            windows[1].clone(),
        ];
        let throttled = quota_pool_observation(&throttled, 0).unwrap();
        assert!(!throttled.usable);
        assert_eq!(throttled.skip_reason, Some("five_hour_reserve"));

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
    fn quota_pool_log_uses_a_stable_account_hash_and_reason() {
        let selection = ulid::Ulid::new();
        let message =
            pool_decision_message(selection, "skip", "private-account-id", 2, "weekly_floor");
        assert!(message.contains(&format!("selection={selection}")));
        assert!(message.contains("event=skip"));
        assert!(message.contains("rank=2 reason=weekly_floor"));
        assert!(!message.contains("private-account-id"));
        assert_eq!(
            message,
            pool_decision_message(selection, "skip", "private-account-id", 2, "weekly_floor")
        );
    }

    #[test]
    fn quota_pool_accepts_weekly_only_and_still_enforces_weekly_floor() {
        let weekly_only = vec![quota_window(
            crate::model::QuotaPeriodUnit::Week,
            None,
            10.0,
            None,
        )];
        assert!(quota_pool_observation(&weekly_only, 0).unwrap().usable);
        assert_eq!(
            quota_pool_observation(&weekly_only, 90)
                .unwrap()
                .skip_reason,
            Some("weekly_floor")
        );
        assert!(quota_pool_observation(&[], 0).is_none());
        assert!(quota_pool_observation(
            &[quota_window(
                crate::model::QuotaPeriodUnit::Hour,
                Some(5),
                0.0,
                None
            )],
            0
        )
        .is_none());
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
    fn legacy_login_keeps_its_pool_membership() {
        let mut settings = QuotaPoolSettings {
            enabled: true,
            strategy: QuotaPoolStrategy::Rank,
            members: vec![QuotaPoolMember {
                account_id: "old-workspace-id".into(),
                in_pool: true,
                paused: false,
                weekly_floor: 35,
                maintain_five_hour_window: true,
            }],
        };
        migrate_quota_pool_member(&mut settings, "old-workspace-id", "new-credential-id");
        assert_eq!(settings.members.len(), 1);
        assert_eq!(settings.members[0].account_id, "new-credential-id");
        assert!(settings.members[0].in_pool);
        assert_eq!(settings.members[0].weekly_floor, 35);
        assert!(settings.members[0].maintain_five_hour_window);

        settings.members.insert(
            0,
            QuotaPoolMember {
                account_id: "old-workspace-id".into(),
                in_pool: true,
                paused: false,
                weekly_floor: 42,
                maintain_five_hour_window: false,
            },
        );
        settings.members[1].in_pool = false;
        migrate_quota_pool_member(&mut settings, "old-workspace-id", "new-credential-id");
        assert_eq!(settings.members.len(), 1);
        assert!(settings.members[0].in_pool);
        assert_eq!(settings.members[0].weekly_floor, 42);
    }

    #[tokio::test]
    async fn workspace_refresh_chooses_the_rejected_users_credential() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().to_path_buf());
        for (credential, token, workspace) in [
            ("shared-workspace", "legacy-token", None),
            (
                "new-user-credential",
                "new-user-token",
                Some("shared-workspace"),
            ),
        ] {
            manager.accounts.write().await.insert(
                credential.into(),
                AccountMetadata {
                    account_id: credential.into(),
                    chatgpt_account_id: workspace.map(str::to_string),
                    workspace_name: None,
                    plan_type: None,
                    email: None,
                    authenticated_at: 1,
                },
            );
            manager.access_tokens.write().await.insert(
                credential.into(),
                CachedToken {
                    access_token: token.into(),
                    expires_at_ms: i64::MAX,
                },
            );
        }
        assert_eq!(
            manager
                .resolve_credential_selector("shared-workspace", "new-user-token")
                .await
                .unwrap(),
            "new-user-credential"
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
    async fn balanced_quota_reads_overlap_with_a_bound_and_preserve_member_order() {
        let (ready_tx, mut ready_rx) = tokio::sync::mpsc::channel(8);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let reads = (0..6)
            .map(|index| {
                let ready_tx = ready_tx.clone();
                let gate = Arc::clone(&gate);
                async move {
                    ready_tx.send(index).await.unwrap();
                    gate.acquire().await.unwrap().forget();
                    index
                }
            })
            .collect();
        let batch = tokio::spawn(collect_balanced_quota_reads(reads));
        // The first four all start before any can finish. A sequential
        // implementation stalls here, while an unbounded one starts six.
        for expected in 0..4 {
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(5), ready_rx.recv())
                    .await
                    .unwrap(),
                Some(expected)
            );
        }
        assert!(ready_rx.try_recv().is_err());
        gate.add_permits(4);
        for expected in 4..6 {
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(5), ready_rx.recv())
                    .await
                    .unwrap(),
                Some(expected)
            );
        }
        gate.add_permits(2);
        assert_eq!(batch.await.unwrap(), vec![0, 1, 2, 3, 4, 5]);
    }

    #[tokio::test]
    async fn quota_retry_requires_fit_and_excludes_exhausted_account_until_reset() {
        use vellum_proxy_runtime::official_auth::OfficialRoutingContext;
        for strategy in [QuotaPoolStrategy::Balanced, QuotaPoolStrategy::Rank] {
            let temp = tempfile::tempdir().unwrap();
            let manager = CodexOAuthManager::new(temp.path().to_path_buf());
            let mut members = Vec::new();
            for id in ["quota-retry-a", "quota-retry-b"] {
                let payload = URL_SAFE_NO_PAD.encode(serde_json::json!({
                    "sub": id, "chatgpt_account_id": "quota-retry-workspace", "chatgpt_plan_type":"plus",
                }).to_string());
                let token = format!("header.{payload}.signature");
                manager.accounts.write().await.insert(
                    id.into(),
                    AccountMetadata {
                        account_id: id.into(),
                        chatgpt_account_id: Some("quota-retry-workspace".into()),
                        workspace_name: None,
                        plan_type: Some("plus".into()),
                        email: None,
                        authenticated_at: 1,
                    },
                );
                manager.access_tokens.write().await.insert(
                    id.into(),
                    CachedToken {
                        access_token: token.clone(),
                        expires_at_ms: i64::MAX,
                    },
                );
                crate::codex_quota::seed_test_quota(
                    &token,
                    "quota-retry-workspace",
                    vec![
                        quota_window(crate::model::QuotaPeriodUnit::Hour, Some(5), 10.0, None),
                        quota_window(crate::model::QuotaPeriodUnit::Week, None, 10.0, None),
                    ],
                )
                .await;
                members.push(QuotaPoolMember {
                    account_id: id.into(),
                    in_pool: true,
                    paused: false,
                    weekly_floor: 0,
                    maintain_five_hour_window: false,
                });
            }
            *manager.quota_pool.write().await = QuotaPoolSettings {
                enabled: true,
                strategy,
                members,
            };
            *manager.default_account_id.write().await = Some("quota-retry-a".into());
            let mut context = OfficialRoutingContext {
                session_key: Some("quota-retry-session".into()),
                request_id: "quota-retry-request".into(),
                estimated_input_tokens: 1000,
                ..Default::default()
            };
            let hash = pool_account_hash("quota-retry-a");
            let reset = chrono::Utc::now().timestamp_millis().max(0) as u64 + 60_000;
            assert_eq!(
                manager
                    .retry_pool_after_quota(&hash, Some(reset), &context, true)
                    .await
                    .unwrap()
                    .unwrap()
                    .credential_id,
                "quota-retry-b"
            );
            assert_eq!(manager.exhausted_pool_accounts.lock()[&hash], reset);
            assert_eq!(
                manager
                    .valid_routing_auth_with_context(&context)
                    .await
                    .unwrap()
                    .unwrap()
                    .credential_id,
                "quota-retry-b"
            );
            context.estimated_input_tokens = 10_000_000;
            assert!(
                manager
                    .retry_pool_after_quota(&hash, Some(reset), &context, true)
                    .await
                    .unwrap()
                    .is_none(),
                "no fitting member must never use manual fallback"
            );
            context.estimated_input_tokens = 1000;
            assert!(manager
                .retry_pool_after_quota(&hash, Some(reset), &context, false)
                .await
                .unwrap()
                .is_none());
            manager.exhausted_pool_accounts.lock().insert(hash, 0);
            manager
                .exhausted_pool_accounts
                .lock()
                .insert(pool_account_hash("quota-retry-b"), reset);
            assert_eq!(
                manager
                    .valid_routing_auth_with_context(&context)
                    .await
                    .unwrap()
                    .unwrap()
                    .credential_id,
                "quota-retry-a",
                "account is eligible again after reset"
            );
        }
    }

    #[tokio::test]
    async fn quota_pool_balanced_routes_with_context_and_preserves_explicit_billing() {
        use vellum_proxy_runtime::official_auth::OfficialRoutingContext;
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().to_path_buf());
        let mut members = Vec::new();
        for (id, used) in [("balanced-a", 60.0), ("balanced-b", 10.0)] {
            let payload = URL_SAFE_NO_PAD.encode(serde_json::json!({
                "sub": id, "chatgpt_account_id": "shared-balanced", "chatgpt_plan_type": "business",
            }).to_string());
            let token = format!("header.{payload}.signature");
            manager.accounts.write().await.insert(
                id.into(),
                AccountMetadata {
                    account_id: id.into(),
                    chatgpt_account_id: Some("shared-balanced".into()),
                    workspace_name: None,
                    plan_type: Some("business".into()),
                    email: None,
                    authenticated_at: 1,
                },
            );
            manager.access_tokens.write().await.insert(
                id.into(),
                CachedToken {
                    access_token: token.clone(),
                    expires_at_ms: i64::MAX,
                },
            );
            crate::codex_quota::seed_test_quota(
                &token,
                "shared-balanced",
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
                maintain_five_hour_window: false,
            });
        }
        *manager.default_account_id.write().await = Some("balanced-a".into());
        let mut settings = QuotaPoolSettings {
            enabled: true,
            strategy: QuotaPoolStrategy::Balanced,
            members,
        };
        manager.set_quota_pool(settings.clone()).await.unwrap();
        assert_eq!(
            load_store(temp.path()).unwrap().quota_pool.strategy,
            QuotaPoolStrategy::Balanced
        );
        let context = OfficialRoutingContext {
            session_key: Some("session-balanced".into()),
            estimated_input_tokens: 80_000,
            request_id: "req-balanced".into(),
            ..Default::default()
        };
        assert_eq!(
            manager
                .valid_routing_auth_with_context(&context)
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            "balanced-b"
        );
        assert_eq!(
            manager
                .quota_pool_status()
                .await
                .active_account_id
                .as_deref(),
            Some("balanced-b")
        );
        assert_eq!(
            manager
                .valid_auth_for("balanced-a")
                .await
                .unwrap()
                .credential_id,
            "balanced-a"
        );
        settings.members.swap(0, 1);
        manager.set_quota_pool(settings.clone()).await.unwrap();
        assert_eq!(
            manager
                .valid_routing_auth_with_context(&context)
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            "balanced-b"
        );
        settings.members[0].paused = true;
        manager.set_quota_pool(settings.clone()).await.unwrap();
        assert_eq!(
            manager
                .valid_routing_auth_with_context(&context)
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            "balanced-a"
        );
        settings.members[1].weekly_floor = 100;
        manager.set_quota_pool(settings).await.unwrap();
        assert_eq!(
            manager
                .valid_routing_auth_with_context(&context)
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            "balanced-a"
        );
    }

    #[tokio::test]
    async fn quota_pool_enable_preserves_active_turn_and_defers_usage_limit_switch_to_boundary() {
        use vellum_proxy_runtime::official_auth::OfficialRoutingContext;
        use vellum_proxy_runtime::request::OfficialSocketAffinity;
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().to_path_buf());
        let mut members = Vec::new();
        for (id, used) in [("midturn-a", 90.0), ("midturn-b", 10.0)] {
            let payload = URL_SAFE_NO_PAD.encode(
                serde_json::json!({"sub":id,"chatgpt_account_id":id,"chatgpt_plan_type":"plus"})
                    .to_string(),
            );
            let token = format!("header.{payload}.signature");
            manager.accounts.write().await.insert(
                id.into(),
                AccountMetadata {
                    account_id: id.into(),
                    chatgpt_account_id: Some(id.into()),
                    workspace_name: None,
                    plan_type: Some("plus".into()),
                    email: None,
                    authenticated_at: 1,
                },
            );
            manager.access_tokens.write().await.insert(
                id.into(),
                CachedToken {
                    access_token: token.clone(),
                    expires_at_ms: i64::MAX,
                },
            );
            crate::codex_quota::seed_test_quota(
                &token,
                id,
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
                maintain_five_hour_window: false,
            });
        }
        *manager.default_account_id.write().await = Some("midturn-a".into());
        assert_eq!(
            manager
                .valid_routing_auth()
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            "midturn-a"
        );
        let mut settings = QuotaPoolSettings {
            enabled: true,
            strategy: QuotaPoolStrategy::Balanced,
            members,
        };
        manager.set_quota_pool(settings.clone()).await.unwrap();
        let mut context = OfficialRoutingContext {
            socket_affinity: Some(OfficialSocketAffinity {
                account_hash: OfficialSocketAffinity::hash_account("midturn-a"),
                turn_in_progress: true,
                input_tokens: 300_000,
                quota_exhausted: false,
            }),
            session_key: Some("main:existing-thread".into()),
            estimated_input_tokens: 300_000,
            request_id: "midturn-continue".into(),
            ..Default::default()
        };
        assert_eq!(
            manager
                .valid_routing_auth_with_context(&context)
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            "midturn-a"
        );
        context.socket_affinity.as_mut().unwrap().turn_in_progress = false;
        assert_eq!(
            manager
                .valid_routing_auth_with_context(&context)
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            "midturn-a",
            "usable accounts remain bound across roots"
        );
        context.socket_affinity.as_mut().unwrap().quota_exhausted = true;
        context.socket_affinity.as_mut().unwrap().turn_in_progress = true;
        assert_eq!(
            manager
                .valid_routing_auth_with_context(&context)
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            "midturn-a",
            "an authoritative usage-limit 429 cannot move an unfinished root turn"
        );
        context.socket_affinity.as_mut().unwrap().turn_in_progress = false;
        assert_eq!(
            manager
                .valid_routing_auth_with_context(&context)
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            "midturn-b",
            "next root can bypass the still-warm stale quota snapshot"
        );
        context.socket_affinity.as_mut().unwrap().quota_exhausted = false;
        settings.members[0].paused = true;
        manager.set_quota_pool(settings).await.unwrap();
        context.socket_affinity.as_mut().unwrap().turn_in_progress = true;
        assert_eq!(
            manager
                .valid_routing_auth_with_context(&context)
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            "midturn-a"
        );
        context.socket_affinity.as_mut().unwrap().turn_in_progress = false;
        assert_eq!(
            manager
                .valid_routing_auth_with_context(&context)
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            "midturn-b"
        );
    }

    #[tokio::test]
    async fn quota_pool_manual_fallback_preserves_real_plan_and_capacity() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().into());
        let id = "diagnostic-fallback-pro";
        manager.accounts.write().await.insert(
            id.into(),
            AccountMetadata {
                account_id: id.into(),
                chatgpt_account_id: Some(id.into()),
                workspace_name: None,
                plan_type: Some("Pro".into()),
                email: None,
                authenticated_at: 1,
            },
        );
        manager.access_tokens.write().await.insert(
            id.into(),
            CachedToken {
                access_token: "synthetic-fallback".into(),
                expires_at_ms: i64::MAX,
            },
        );
        *manager.default_account_id.write().await = Some(id.into());
        *manager.quota_pool.write().await = QuotaPoolSettings {
            enabled: true,
            strategy: QuotaPoolStrategy::Balanced,
            members: vec![QuotaPoolMember {
                account_id: id.into(),
                in_pool: true,
                paused: false,
                weekly_floor: 0,
                maintain_five_hour_window: false,
            }],
        };
        let context = vellum_proxy_runtime::official_auth::OfficialRoutingContext {
            session_key: Some("fallback-plan-session".into()),
            request_id: "fallback-plan-request".into(),
            estimated_input_tokens: 40_000,
            ..Default::default()
        };
        for used in [10.0, 12.0, 100.0, 12.0] {
            crate::codex_quota::seed_test_quota(
                "synthetic-fallback",
                id,
                vec![quota_window(
                    crate::model::QuotaPeriodUnit::Week,
                    None,
                    used,
                    None,
                )],
            )
            .await;
            manager
                .valid_routing_auth_with_context(&context)
                .await
                .unwrap();
            if used == 10.0 {
                manager.observe_routing_usage("fallback-plan-request", 40_000, 0, 200);
            } else {
                // The advisory fallback must not masquerade as Pro -> None
                // and erase the learned 20k effective tokens per weekly point.
                assert_eq!(
                    manager.smart_router.lock().diagnostic_capacity(id, 20.0).0,
                    20_000.0
                );
            }
        }
    }

    #[tokio::test]
    async fn quota_pool_diagnostics_cover_gates_budget_and_recovery() {
        use vellum_proxy_runtime::{DetailLevel, DiagnosticEvent, DiagnosticsSink};
        #[derive(Default)]
        struct Sink(std::sync::Mutex<Vec<DiagnosticEvent>>);
        impl DiagnosticsSink for Sink {
            fn record(&self, event: DiagnosticEvent) {
                self.0.lock().unwrap().push(event);
            }
            fn detail_level(&self) -> DetailLevel {
                DetailLevel::T1Summary
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().into());
        let sink = Arc::new(Sink::default());
        manager.set_pool_diagnostics_sink(sink.clone());
        let mut members = Vec::new();
        // Unique synthetic identities avoid interference with parallel tests.
        for (id, weekly, five) in [
            ("diagnostic-open", 10.0, 10.0),
            ("diagnostic-week", 100.0, 10.0),
            ("diagnostic-five", 10.0, 96.0),
            ("diagnostic-budget", 10.0, 94.0),
            ("diagnostic-missing", 0.0, 0.0),
        ] {
            manager.accounts.write().await.insert(
                id.into(),
                AccountMetadata {
                    account_id: id.into(),
                    chatgpt_account_id: Some(id.into()),
                    workspace_name: None,
                    plan_type: None,
                    email: None,
                    authenticated_at: 1,
                },
            );
            manager.access_tokens.write().await.insert(
                id.into(),
                CachedToken {
                    access_token: format!("synthetic-{id}"),
                    expires_at_ms: i64::MAX,
                },
            );
            let windows = if id == "diagnostic-missing" {
                vec![]
            } else {
                vec![
                    quota_window(crate::model::QuotaPeriodUnit::Week, None, weekly, None),
                    quota_window(crate::model::QuotaPeriodUnit::Hour, Some(5), five, None),
                ]
            };
            crate::codex_quota::seed_test_quota(&format!("synthetic-{id}"), id, windows).await;
            members.push(QuotaPoolMember {
                account_id: id.into(),
                in_pool: true,
                paused: false,
                weekly_floor: 0,
                maintain_five_hour_window: false,
            });
        }
        *manager.quota_pool.write().await = QuotaPoolSettings {
            enabled: true,
            strategy: QuotaPoolStrategy::Balanced,
            members,
        };
        let context = vellum_proxy_runtime::official_auth::OfficialRoutingContext {
            session_key: Some("diagnostic-session".into()),
            request_id: "diagnostic-request".into(),
            estimated_input_tokens: 2000,
            ..Default::default()
        };
        for _ in 0..2 {
            manager
                .valid_routing_auth_with_context(&context)
                .await
                .unwrap();
        }
        {
            let events = sink.0.lock().unwrap();
            assert_eq!(events.len(), 1);
            let DiagnosticEvent::QuotaPoolDecision(d) = &events[0] else {
                panic!("wrong event")
            };
            for (id, reason) in [
                ("diagnostic-week", "weekly_floor"),
                ("diagnostic-five", "five_hour_reserve"),
                ("diagnostic-budget", "gate_request_budget"),
                ("diagnostic-missing", "missing_quota_windows"),
            ] {
                let c = d
                    .candidates
                    .iter()
                    .find(|c| c.account_hash == pool_account_hash(id))
                    .unwrap();
                assert_eq!(c.exclusion_reason.as_deref(), Some(reason));
            }
            let encoded = serde_json::to_string(d).unwrap();
            assert!(!encoded.contains("synthetic-"));
            assert!(!encoded.contains("diagnostic-open"));
        }
        // Missing quota is a temporary exclusion, not a persistent blacklist.
        crate::codex_quota::seed_test_quota(
            "synthetic-diagnostic-missing",
            "diagnostic-missing",
            vec![quota_window(
                crate::model::QuotaPeriodUnit::Week,
                None,
                0.0,
                None,
            )],
        )
        .await;
        manager
            .valid_routing_auth_with_context(&context)
            .await
            .unwrap();
        let events = sink.0.lock().unwrap();
        assert_eq!(events.len(), 2);
        let DiagnosticEvent::QuotaPoolDecision(d) = &events[1] else {
            panic!("wrong event")
        };
        assert!(d
            .candidates
            .iter()
            .find(|c| c.account_hash == pool_account_hash("diagnostic-missing"))
            .unwrap()
            .exclusion_reason
            .is_none());
    }

    #[test]
    fn quota_pool_reports_stale_reset_and_accepts_refreshed_window() {
        let mut windows = vec![quota_window(
            crate::model::QuotaPeriodUnit::Week,
            None,
            100.0,
            Some("2026-10-01T00:00:00Z"),
        )];
        assert_eq!(
            quota_pool_observation_at(&windows, 0, u64::MAX)
                .unwrap()
                .skip_reason,
            Some("weekly_reset_pending")
        );
        windows[0].used_percent = 0.0;
        assert!(
            quota_pool_observation_at(&windows, 0, u64::MAX)
                .unwrap()
                .usable
        );
    }

    #[tokio::test]
    async fn quota_pool_routes_across_users_and_workspaces() {
        // Rank routing has no session affinity or migration hysteresis.
        // The seeded offline snapshots must not expire while settings are
        // persisted: after opening pool-c, pool-b remains exactly at its
        // weekly floor and pool-a remains paused, so only pool-c is usable.
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().to_path_buf());
        let mut members = Vec::new();
        for (id, user, workspace, plan, used, weekly_floor) in [
            ("pool-a", "user-one", "personal-a", "plus", 70.0, 0),
            ("pool-b", "user-one", "business-b", "business", 20.0, 0),
            ("pool-c", "user-two", "business-b", "business", 10.0, 100),
        ] {
            let payload = URL_SAFE_NO_PAD.encode(
                serde_json::json!({
                    "sub": user,
                    "chatgpt_account_id": workspace,
                    "chatgpt_plan_type": plan,
                })
                .to_string(),
            );
            let token = format!("header.{payload}.signature");
            manager.accounts.write().await.insert(
                id.into(),
                AccountMetadata {
                    account_id: id.into(),
                    chatgpt_account_id: Some(workspace.into()),
                    workspace_name: None,
                    plan_type: Some(plan.into()),
                    email: Some(format!("{id}@example.test")),
                    authenticated_at: 1,
                },
            );
            manager.access_tokens.write().await.insert(
                id.into(),
                CachedToken {
                    access_token: token.clone(),
                    expires_at_ms: i64::MAX,
                },
            );
            crate::codex_quota::seed_test_quota(
                &token,
                workspace,
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
                weekly_floor,
                maintain_five_hour_window: false,
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
        // The manual account is paused, so the remaining valid pool member
        // can still attempt a request even below its automatic gate.
        assert_eq!(
            manager
                .valid_routing_auth()
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            "pool-b"
        );
        settings.enabled = false;
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
        settings.enabled = true;
        settings.members[2].weekly_floor = 0;
        manager.set_quota_pool(settings).await.unwrap();
        assert_eq!(
            manager
                .valid_routing_auth()
                .await
                .unwrap()
                .unwrap()
                .credential_id,
            "pool-c"
        );
    }

    #[tokio::test]
    async fn exhausted_shared_workspace_allows_manually_selected_seat_to_send() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CodexOAuthManager::new(temp.path().to_path_buf());
        let mut members = Vec::new();
        for (id, user) in [("seat-a", "user-a"), ("seat-b", "user-b")] {
            let payload = URL_SAFE_NO_PAD.encode(
                serde_json::json!({
                    "sub": user,
                    "chatgpt_account_id": "shared-business",
                    "chatgpt_plan_type": "business",
                })
                .to_string(),
            );
            let token = format!("header.{payload}.signature");
            manager.accounts.write().await.insert(
                id.into(),
                AccountMetadata {
                    account_id: id.into(),
                    chatgpt_account_id: Some("shared-business".into()),
                    workspace_name: None,
                    plan_type: Some("business".into()),
                    email: Some(format!("{user}@example.test")),
                    authenticated_at: 1,
                },
            );
            manager.access_tokens.write().await.insert(
                id.into(),
                CachedToken {
                    access_token: token.clone(),
                    expires_at_ms: i64::MAX,
                },
            );
            crate::codex_quota::seed_test_quota(
                &token,
                "shared-business",
                vec![
                    quota_window(crate::model::QuotaPeriodUnit::Hour, Some(5), 100.0, None),
                    quota_window(crate::model::QuotaPeriodUnit::Week, None, 100.0, None),
                ],
            )
            .await;
            members.push(QuotaPoolMember {
                account_id: id.into(),
                in_pool: true,
                paused: false,
                weekly_floor: 0,
                maintain_five_hour_window: false,
            });
        }
        *manager.default_account_id.write().await = Some("seat-a".into());
        manager
            .set_quota_pool(QuotaPoolSettings {
                enabled: true,
                strategy: QuotaPoolStrategy::Rank,
                members,
            })
            .await
            .unwrap();

        manager.set_default("seat-b").await.unwrap();
        let selected = manager.valid_routing_auth().await.unwrap().unwrap();
        assert_eq!(selected.credential_id, "seat-b");
        assert_eq!(selected.account_id, "shared-business");
        assert_eq!(
            manager
                .quota_pool_status()
                .await
                .active_account_id
                .as_deref(),
            Some("seat-b")
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
                    maintain_five_hour_window: false,
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
        assert!(manager.valid_routing_auth().await.unwrap().is_some());
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
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(metadata_path(temp.path())).unwrap()).unwrap();
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
