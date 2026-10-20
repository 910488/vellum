use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use vellum_proxy_runtime::claude_code::{AccountToken, ClaudeAccounts};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const TOKEN_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";
const STORAGE_ID: &str = "claudecodeaccounts";
/// A gate decision may use usage this old; past it the usage is read again.
const USAGE_FRESH_SECS: i64 = 120;
/// A gate check never stalls an inference request for longer than this.
const GATE_USAGE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default, Serialize, Deserialize)]
struct StoredAccounts {
    accounts: Vec<StoredAccount>,
    selected: Option<String>,
    /// The account a gate moved the selection away from, until a manual pick.
    #[serde(skip)]
    switched_from: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct StoredAccount {
    id: String,
    label: String,
    access_token: String,
    refresh_token: String,
    expires_at: i64,
    #[serde(default)]
    refresh_blocked: bool,
    #[serde(default)]
    gate: AccountGate,
    #[serde(skip)]
    usage: Option<AccountUsage>,
    /// Last usage read attempt, so a failing read is not retried per request.
    #[serde(skip)]
    usage_checked_at: i64,
}
/// Percent of each window to keep: automatic use stops once the remaining
/// share reaches it. 0 leaves that window ungated.
#[derive(Clone, Copy, Default, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountGate {
    #[serde(default)]
    pub five_hour: u8,
    #[serde(default)]
    pub seven_day: u8,
}
impl AccountGate {
    fn is_set(&self) -> bool {
        self.five_hour > 0 || self.seven_day > 0
    }
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    pub id: String,
    pub label: String,
    pub expires_at: i64,
    pub reauthorize_required: bool,
    pub usage: Option<AccountUsage>,
    pub gate: AccountGate,
    /// Remaining usage, as last read, is at or below a gate.
    pub gated: bool,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    pub utilization: f64,
    pub resets_at: Option<String>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountUsage {
    pub observed_at: i64,
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountsView {
    pub accounts: Vec<AccountView>,
    pub selected: Option<String>,
    pub switched_from: Option<String>,
    pub login_pending: bool,
    pub login_error: Option<String>,
}
struct PendingLogin {
    state: String,
    verifier: String,
    redirect: String,
    cancel: tokio::sync::oneshot::Sender<()>,
}
#[derive(Default)]
struct LoginState {
    pending: Option<PendingLogin>,
    error: Option<String>,
}

#[async_trait]
trait TokenTransport: Send + Sync {
    async fn exchange(&self, body: Value) -> Result<Value, String>;
}
struct HttpTokens;
#[async_trait]
impl TokenTransport for HttpTokens {
    async fn exchange(&self, body: Value) -> Result<Value, String> {
        token_request(body).await
    }
}
pub struct CodeAccounts {
    root: PathBuf,
    stored: Mutex<Option<StoredAccounts>>,
    login: Mutex<LoginState>,
    tokens: Arc<dyn TokenTransport>,
}
impl CodeAccounts {
    pub fn new(root: PathBuf) -> Arc<Self> {
        Self::with_tokens(root, Arc::new(HttpTokens))
    }
    fn with_tokens(root: PathBuf, tokens: Arc<dyn TokenTransport>) -> Arc<Self> {
        Arc::new(Self {
            root,
            stored: Mutex::new(None),
            login: Mutex::new(LoginState::default()),
            tokens,
        })
    }
    fn load(&self, stored: &mut Option<StoredAccounts>) -> Result<(), String> {
        if stored.is_none() {
            *stored = Some(
                match crate::credentials::load(&self.root, STORAGE_ID)
                    .map_err(|_| "Cannot decrypt Claude accounts")?
                {
                    Some(text) => serde_json::from_str(&text)
                        .map_err(|_| "Invalid encrypted Claude account data")?,
                    None => StoredAccounts::default(),
                },
            );
        }
        Ok(())
    }
    fn save(&self, stored: &StoredAccounts) -> Result<(), String> {
        crate::credentials::save(
            &self.root,
            STORAGE_ID,
            &serde_json::to_string(stored).map_err(|_| "Cannot encode Claude accounts")?,
        )
        .map_err(|_| "Cannot save encrypted Claude accounts".into())
    }
    pub async fn view(&self) -> Result<AccountsView, String> {
        let mut stored = self.stored.lock().await;
        self.load(&mut stored)?;
        let data = stored.as_ref().unwrap();
        let login = self.login.lock().await;
        let now = chrono::Utc::now().timestamp();
        Ok(AccountsView {
            accounts: data
                .accounts
                .iter()
                .map(|a| AccountView {
                    id: a.id.clone(),
                    label: a.label.clone(),
                    expires_at: a.expires_at,
                    reauthorize_required: a.refresh_blocked,
                    usage: a.usage.clone(),
                    gate: a.gate,
                    gated: gate_reached(a, now),
                })
                .collect(),
            selected: data.selected.clone(),
            switched_from: data.switched_from.clone(),
            login_pending: login.pending.is_some(),
            login_error: login.error.clone(),
        })
    }
    pub async fn select(&self, id: Option<String>) -> Result<(), String> {
        let mut stored = self.stored.lock().await;
        self.load(&mut stored)?;
        let data = stored.as_mut().unwrap();
        if id
            .as_ref()
            .is_some_and(|id| !data.accounts.iter().any(|a| &a.id == id))
        {
            return Err("Claude account not found".into());
        }
        let previous = std::mem::replace(&mut data.selected, id);
        if let Err(error) = self.save(data) {
            data.selected = previous;
            return Err(error);
        }
        data.switched_from = None;
        Ok(())
    }
    pub async fn set_gate(&self, id: &str, gate: AccountGate) -> Result<(), String> {
        let gate = AccountGate {
            five_hour: gate.five_hour.min(100),
            seven_day: gate.seven_day.min(100),
        };
        let mut stored = self.stored.lock().await;
        self.load(&mut stored)?;
        let data = stored.as_mut().unwrap();
        let account = data
            .accounts
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or("Claude account not found")?;
        let previous = std::mem::replace(&mut account.gate, gate);
        if let Err(error) = self.save(data) {
            if let Some(account) = data.accounts.iter_mut().find(|a| a.id == id) {
                account.gate = previous;
            }
            return Err(error);
        }
        Ok(())
    }
    pub async fn remove(&self, id: &str) -> Result<(), String> {
        let mut stored = self.stored.lock().await;
        self.load(&mut stored)?;
        let data = stored.as_mut().unwrap();
        if data.selected.as_deref() == Some(id) {
            return Err("Select another execution account before removing this account".into());
        }
        if let Some(index) = data.accounts.iter().position(|a| a.id == id) {
            let account = data.accounts.remove(index);
            if let Err(error) = self.save(data) {
                data.accounts.insert(index, account);
                return Err(error);
            }
        }
        Ok(())
    }
    pub async fn begin(self: &Arc<Self>) -> Result<String, String> {
        let mut login = self.login.lock().await;
        if login.pending.is_some() {
            return Err("A Claude authorization is already pending".into());
        }
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:54545").await {
            Ok(listener) => listener,
            Err(_) => tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|_| "Cannot bind a loopback Claude OAuth callback")?,
        };
        let redirect = format!(
            "http://localhost:{}/callback",
            listener
                .local_addr()
                .map_err(|_| "Cannot read Claude OAuth callback address")?
                .port()
        );
        let state = random_secret()?;
        let verifier = random_secret()?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = url::Url::parse("https://claude.ai/oauth/authorize").unwrap();
        url.query_pairs_mut().extend_pairs([
            ("code", "true"),
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", &redirect),
            ("scope", "user:profile user:inference"),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
        ]);
        let (cancel, wait) = tokio::sync::oneshot::channel();
        login.pending = Some(PendingLogin {
            state: state.clone(),
            verifier,
            redirect,
            cancel,
        });
        login.error = None;
        let manager = self.clone();
        tokio::spawn(async move {
            use axum::{
                extract::{Query, State},
                routing::get,
                Router,
            };
            let (done, mut completion) = tokio::sync::mpsc::channel::<()>(1);
            let app=Router::new().route("/callback",get(|State((manager,done)):State<(Arc<CodeAccounts>,tokio::sync::mpsc::Sender<()>)>,Query(params):Query<std::collections::HashMap<String,String>>| async move {
                let result=manager.complete(&params).await;
                // A wrong-state request must not terminate a legitimate login.
                if manager.login.lock().await.pending.is_none() {let _=done.send(()).await;}
                match result {Ok(())=>(axum::http::StatusCode::OK,"Claude authorization completed. Return to Vellum."),Err(_)=>(axum::http::StatusCode::BAD_REQUEST,"Claude authorization failed. Check Vellum or retry authorization.")}
            })).with_state((manager.clone(),done));
            let _=axum::serve(listener,app).with_graceful_shutdown(async move {
                tokio::select! { _=completion.recv()=>{}, _=wait=>{}, _=tokio::time::sleep(Duration::from_secs(300))=>{} }
            }).await;
            let mut login = manager.login.lock().await;
            if login.pending.as_ref().is_some_and(|p| p.state == state) {
                login.pending = None;
                if login.error.is_none() {
                    login.error = Some("Claude authorization expired or was cancelled".into());
                }
            }
        });
        Ok(url.into())
    }
    async fn complete(
        &self,
        params: &std::collections::HashMap<String, String>,
    ) -> Result<(), String> {
        let mut login = self.login.lock().await;
        let pending = login
            .pending
            .as_ref()
            .ok_or("No pending Claude authorization")?;
        if params.get("state") != Some(&pending.state) {
            return Err("OAuth state mismatch".into());
        }
        if params.contains_key("error") {
            login.pending = None;
            let error = "Claude authorization was declined or cancelled in the browser";
            login.error = Some(error.into());
            return Err(error.into());
        }
        let code = params
            .get("code")
            .filter(|code| code.len() < 4096)
            .ok_or("Missing authorization code")?;
        let body = json!({"grant_type":"authorization_code","code":code,"redirect_uri":pending.redirect,"client_id":CLIENT_ID,"code_verifier":pending.verifier,"state":pending.state});
        // Release the login lock before account-store IO: view locks in the opposite order.
        let pending = login.pending.take();
        drop(login);
        let result = self.exchange_and_store(body).await;
        let mut login = self.login.lock().await;
        if let Err(error) = &result {
            login.error = Some(error.clone());
        }
        drop(pending);
        result
    }
    async fn exchange_and_store(&self, body: Value) -> Result<(), String> {
        let response = self.tokens.exchange(body).await?;
        let access = required(&response, "access_token")?;
        let profile = if response
            .pointer("/account/uuid")
            .and_then(Value::as_str)
            .is_some()
        {
            response.clone()
        } else {
            let response = client()?
                .get("https://api.anthropic.com/api/oauth/profile")
                .bearer_auth(&access)
                .header("anthropic-beta", "oauth-2025-04-20")
                .send()
                .await
                .map_err(|_| "Claude account profile transport failed")?;
            if !response.status().is_success() {
                return Err(format!(
                    "Claude profile HTTP {}",
                    response.status().as_u16()
                ));
            }
            response
                .json::<Value>()
                .await
                .map_err(|_| "Invalid Claude profile")?
        };
        let id = profile
            .pointer("/account/uuid")
            .and_then(Value::as_str)
            .filter(|id| id.len() < 128)
            .ok_or("Claude account identity missing")?
            .to_string();
        let label = profile
            .pointer("/account/email_address")
            .or_else(|| profile.pointer("/account/email"))
            .and_then(Value::as_str)
            .unwrap_or(&id)
            .chars()
            .take(160)
            .collect();
        let refresh_token = required(&response, "refresh_token")?;
        let mut stored = self.stored.lock().await;
        self.load(&mut stored)?;
        let data = stored.as_mut().unwrap();
        let old = data
            .accounts
            .iter()
            .position(|a| a.id == id)
            .map(|index| data.accounts.remove(index));
        data.accounts.push(StoredAccount {
            id: id.clone(),
            label,
            access_token: access,
            refresh_token,
            expires_at: expiry(&response),
            refresh_blocked: false,
            // Authorizing again keeps the gates set for this account.
            gate: old.as_ref().map(|a| a.gate).unwrap_or_default(),
            usage: None,
            usage_checked_at: 0,
        });
        if let Err(error) = self.save(data) {
            data.accounts.pop();
            if let Some(old) = old {
                data.accounts.push(old);
            }
            return Err(error);
        }
        Ok(())
    }
    pub async fn cancel_login(&self) {
        if let Some(pending) = self.login.lock().await.pending.take() {
            let _ = pending.cancel.send(());
        }
    }
    pub async fn refresh_usage(&self, id: &str) -> Result<(), String> {
        let mut stored = self.stored.lock().await;
        self.load(&mut stored)?;
        self.read_usage(stored.as_mut().unwrap(), id).await
    }
    async fn read_usage(&self, data: &mut StoredAccounts, id: &str) -> Result<(), String> {
        if let Some(account) = data.accounts.iter_mut().find(|a| a.id == id) {
            account.usage_checked_at = chrono::Utc::now().timestamp();
        }
        let lease = self.token_for(data, id).await?;
        let response = client()?
            .get("https://api.anthropic.com/api/oauth/usage")
            .bearer_auth(lease.access_token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .send()
            .await
            .map_err(|_| "Claude usage transport failed")?;
        if !response.status().is_success() {
            return Err(format!("Claude usage HTTP {}", response.status().as_u16()));
        }
        let value: Value = response
            .json()
            .await
            .map_err(|_| "Invalid Claude usage response")?;
        let usage = AccountUsage {
            observed_at: chrono::Utc::now().timestamp(),
            five_hour: usage_window(&value, "five_hour"),
            seven_day: usage_window(&value, "seven_day"),
        };
        if usage.five_hour.is_none() && usage.seven_day.is_none() {
            return Err("Claude subscription usage is unavailable for this account".into());
        }
        data.accounts
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or("Claude account missing")?
            .usage = Some(usage);
        Ok(())
    }
    async fn token_for(
        &self,
        data: &mut StoredAccounts,
        selected: &str,
    ) -> Result<AccountToken, String> {
        let index = data
            .accounts
            .iter()
            .position(|a| a.id == selected)
            .ok_or("Selected Claude account missing")?;
        if data.accounts[index].refresh_blocked {
            return Err(
                "Claude account needs authorization after an incomplete token refresh".into(),
            );
        }
        if data.accounts[index].expires_at <= chrono::Utc::now().timestamp() + 60 {
            data.accounts[index].refresh_blocked = true;
            self.save(data)?;
            let response=self.tokens.exchange(json!({"grant_type":"refresh_token","refresh_token":data.accounts[index].refresh_token,"client_id":CLIENT_ID})).await?;
            if let Some(identity) = response.pointer("/account/uuid") {
                if identity.as_str() != Some(selected) {
                    return Err("Claude refresh account identity changed; authorize again".into());
                }
            }
            let access_token = required(&response, "access_token")?;
            let refresh_token = response
                .get("refresh_token")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| data.accounts[index].refresh_token.clone());
            data.accounts[index].access_token = access_token;
            data.accounts[index].refresh_token = refresh_token;
            data.accounts[index].expires_at = expiry(&response);
            data.accounts[index].refresh_blocked = false;
            if let Err(error) = self.save(data) {
                data.accounts[index].refresh_blocked = true;
                return Err(error);
            }
        }
        Ok(AccountToken {
            account_id: selected.into(),
            access_token: data.accounts[index].access_token.clone(),
        })
    }
}

#[async_trait]
impl ClaudeAccounts for CodeAccounts {
    async fn selected_token(&self) -> Result<Option<AccountToken>, String> {
        let mut stored = self.stored.lock().await;
        self.load(&mut stored)?;
        let data = stored.as_mut().unwrap();
        let Some(selected) = data.selected.clone() else {
            return Ok(None);
        };
        if self.gate_closed(data, &selected).await {
            let candidates: Vec<String> = data
                .accounts
                .iter()
                .filter(|a| a.id != selected && !a.refresh_blocked)
                .map(|a| a.id.clone())
                .collect();
            for id in candidates {
                if self.gate_closed(data, &id).await {
                    continue;
                }
                data.selected = Some(id.clone());
                if self.save(data).is_err() {
                    data.selected = Some(selected.clone());
                    break;
                }
                data.switched_from = Some(selected);
                return self.token_for(data, &id).await.map(Some);
            }
            // Every account is at its gate: keep the current one, so the
            // request still runs and any upstream limit is reported as is.
        }
        self.token_for(data, &selected).await.map(Some)
    }
}
impl CodeAccounts {
    /// Re-reads usage for a gated account when it is stale, best effort.
    async fn gate_closed(&self, data: &mut StoredAccounts, id: &str) -> bool {
        let now = chrono::Utc::now().timestamp();
        let Some(account) = data.accounts.iter().find(|a| a.id == id) else {
            return false;
        };
        let stale = account
            .usage
            .as_ref()
            .is_none_or(|u| now - u.observed_at > USAGE_FRESH_SECS);
        if account.gate.is_set() && stale && now - account.usage_checked_at > USAGE_FRESH_SECS {
            let _ = tokio::time::timeout(GATE_USAGE_TIMEOUT, self.read_usage(data, id)).await;
        }
        data.accounts
            .iter()
            .find(|a| a.id == id)
            .is_some_and(|a| gate_reached(a, now))
    }
}
/// Remaining percent of a window; a window past its reset is whole again.
fn remaining(window: Option<&UsageWindow>, now: i64) -> Option<f64> {
    let window = window?;
    let reset = window
        .resets_at
        .as_deref()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .is_some_and(|at| at.timestamp() <= now);
    Some(if reset {
        100.0
    } else {
        100.0 - window.utilization
    })
}
fn gate_reached(account: &StoredAccount, now: i64) -> bool {
    let Some(usage) = account.usage.as_ref() else {
        return false;
    };
    let closed = |floor: u8, window: Option<&UsageWindow>| {
        floor > 0 && remaining(window, now).is_some_and(|left| left <= f64::from(floor))
    };
    closed(account.gate.five_hour, usage.five_hour.as_ref())
        || closed(account.gate.seven_day, usage.seven_day.as_ref())
}
fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| "Cannot create Claude OAuth transport".into())
}
async fn token_request(body: Value) -> Result<Value, String> {
    let response = client()?
        .post(TOKEN_ENDPOINT)
        .json(&body)
        .send()
        .await
        .map_err(|_| {
            "Claude token transport failed; authorize again before retrying an ambiguous refresh"
        })?;
    if !response.status().is_success() {
        return Err(format!("Claude token HTTP {}", response.status().as_u16()));
    }
    response
        .json()
        .await
        .map_err(|_| "Invalid Claude token response".into())
}
fn required(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() < 16384)
        .map(str::to_owned)
        .ok_or_else(|| format!("Claude OAuth response missing {key}"))
}
fn expiry(value: &Value) -> i64 {
    chrono::Utc::now().timestamp()
        + value
            .get("expires_in")
            .and_then(Value::as_i64)
            .unwrap_or(3600)
            .clamp(1, 86400)
}
pub fn random_secret() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| "OS randomness unavailable")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn usage_window(value: &Value, key: &str) -> Option<UsageWindow> {
    let value = value.get(key)?;
    let utilization = value.get("utilization")?.as_f64()?;
    if !utilization.is_finite() || !(0.0..=100.0).contains(&utilization) {
        return None;
    }
    let resets_at = value
        .get("resets_at")
        .and_then(Value::as_str)
        .filter(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok())
        .map(str::to_owned);
    Some(UsageWindow {
        utilization,
        resets_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct AuthorizationTokens(Mutex<Option<Value>>);
    #[async_trait]
    impl TokenTransport for AuthorizationTokens {
        async fn exchange(&self, body: Value) -> Result<Value, String> {
            *self.0.lock().await = Some(body);
            Ok(
                json!({"access_token":"fake-access","refresh_token":"fake-refresh","expires_in":3600,"account":{"uuid":"B","email_address":"B@example.invalid"}}),
            )
        }
    }
    struct ChangedIdentityTokens;
    #[async_trait]
    impl TokenTransport for ChangedIdentityTokens {
        async fn exchange(&self, _: Value) -> Result<Value, String> {
            Ok(
                json!({"access_token":"another-account-token","refresh_token":"another-refresh","expires_in":3600,"account":{"uuid":"C"}}),
            )
        }
    }
    #[tokio::test]
    async fn refresh_identity_mismatch_cannot_charge_a_different_account() {
        let temp = tempfile::tempdir().unwrap();
        let manager =
            CodeAccounts::with_tokens(temp.path().into(), Arc::new(ChangedIdentityTokens));
        seed(&manager, true);
        assert!(manager.selected_token().await.is_err());
        assert!(manager.view().await.unwrap().accounts[0].reauthorize_required);
        let restarted = CodeAccounts::with_tokens(temp.path().into(), tokens(false));
        assert!(restarted.selected_token().await.is_err());
    }
    #[tokio::test]
    async fn browser_authorization_binds_pkce_state_minimal_scopes_and_actual_loopback_redirect() {
        let temp = tempfile::tempdir().unwrap();
        let transport = Arc::new(AuthorizationTokens(Mutex::new(None)));
        let manager = CodeAccounts::with_tokens(temp.path().into(), transport.clone());
        let url = url::Url::parse(&manager.begin().await.unwrap()).unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["scope"], "user:profile user:inference");
        let callback = url::Url::parse(&query["redirect_uri"]).unwrap();
        assert_eq!(callback.host_str(), Some("localhost"));
        assert_eq!(callback.path(), "/callback");
        assert!(callback.port().unwrap() > 0);
        {
            let login = manager.login.lock().await;
            let pending = login.pending.as_ref().unwrap();
            assert_eq!(query["state"], pending.state);
            assert_eq!(
                query["code_challenge"],
                URL_SAFE_NO_PAD.encode(Sha256::digest(pending.verifier.as_bytes()))
            );
            assert_eq!(query["redirect_uri"], pending.redirect);
        }
        let response = client()
            .unwrap()
            .get(format!(
                "http://127.0.0.1:{}/callback",
                callback.port().unwrap()
            ))
            .query(&[("state", query["state"].as_str()), ("code", "fake-code")])
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let exchange = transport.0.lock().await.clone().unwrap();
        assert_eq!(exchange["redirect_uri"], query["redirect_uri"]);
        assert_eq!(exchange["grant_type"], "authorization_code");
        let view = manager.view().await.unwrap();
        assert_eq!(view.accounts[0].id, "B");
        assert!(!view.login_pending);
        assert!(!serde_json::to_string(&view)
            .unwrap()
            .contains("fake-access"));
    }
    struct Tokens {
        calls: AtomicUsize,
        fail: bool,
    }
    #[async_trait]
    impl TokenTransport for Tokens {
        async fn exchange(&self, body: Value) -> Result<Value, String> {
            assert_eq!(body["grant_type"], "refresh_token");
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            if self.fail {
                Err("ambiguous transport failure".into())
            } else {
                Ok(
                    json!({"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}),
                )
            }
        }
    }
    fn tokens(fail: bool) -> Arc<Tokens> {
        Arc::new(Tokens {
            calls: AtomicUsize::new(0),
            fail,
        })
    }
    fn seed(manager: &CodeAccounts, expired: bool) {
        manager
            .save(&StoredAccounts {
                selected: Some("B".into()),
                accounts: vec![StoredAccount {
                    id: "B".into(),
                    label: "B display".into(),
                    access_token: "secret-access-B".into(),
                    refresh_token: "secret-refresh-B".into(),
                    expires_at: chrono::Utc::now().timestamp() + if expired { -10 } else { 3600 },
                    refresh_blocked: false,
                    gate: AccountGate::default(),
                    usage: None,
                    usage_checked_at: 0,
                }],
                switched_from: None,
            })
            .unwrap();
    }
    #[tokio::test]
    async fn secrets_stay_encrypted_and_are_absent_from_ui_status() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CodeAccounts::with_tokens(temp.path().into(), tokens(false));
        seed(&manager, false);
        let bytes = std::fs::read(temp.path().join("credentials/claudecodeaccounts.bin")).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("secret-access-B"));
        let view = serde_json::to_string(&manager.view().await.unwrap()).unwrap();
        assert!(!view.contains("secret-"));
        assert!(manager.remove("B").await.is_err());
        manager.select(None).await.unwrap();
        manager.remove("B").await.unwrap();
        assert!(manager.view().await.unwrap().accounts.is_empty());
    }
    #[tokio::test]
    async fn parallel_requests_share_one_refresh_and_persist_rotated_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let transport = tokens(false);
        let manager = CodeAccounts::with_tokens(temp.path().into(), transport.clone());
        seed(&manager, true);
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let manager = manager.clone();
            tasks.spawn(async move {
                manager
                    .selected_token()
                    .await
                    .unwrap()
                    .unwrap()
                    .access_token
            });
        }
        while let Some(result) = tasks.join_next().await {
            assert_eq!(result.unwrap(), "rotated-access");
        }
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
        let restarted = CodeAccounts::with_tokens(temp.path().into(), transport.clone());
        assert_eq!(
            restarted
                .selected_token()
                .await
                .unwrap()
                .unwrap()
                .access_token,
            "rotated-access"
        );
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn failed_refresh_is_not_replayed_even_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let transport = tokens(true);
        let manager = CodeAccounts::with_tokens(temp.path().into(), transport.clone());
        seed(&manager, true);
        assert!(manager.selected_token().await.is_err());
        assert!(manager.selected_token().await.is_err());
        let restarted = CodeAccounts::with_tokens(temp.path().into(), transport.clone());
        assert!(restarted.selected_token().await.is_err());
        assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
        assert!(restarted.view().await.unwrap().accounts[0].reauthorize_required);
    }
    #[tokio::test]
    async fn wrong_oauth_state_does_not_consume_the_legitimate_login() {
        let temp = tempfile::tempdir().unwrap();
        let transport = tokens(false);
        let manager = CodeAccounts::with_tokens(temp.path().into(), transport.clone());
        let (cancel, _wait) = tokio::sync::oneshot::channel();
        manager.login.lock().await.pending = Some(PendingLogin {
            state: "expected".into(),
            verifier: "verifier".into(),
            redirect: "http://localhost:54545/callback".into(),
            cancel,
        });
        assert!(manager
            .complete(&std::collections::HashMap::from([
                ("state".into(), "wrong".into()),
                ("code".into(), "code".into())
            ]))
            .await
            .is_err());
        assert!(manager.login.lock().await.pending.is_some());
        assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn browser_denial_clears_only_the_matching_login_without_token_exchange() {
        let temp = tempfile::tempdir().unwrap();
        let transport = tokens(false);
        let manager = CodeAccounts::with_tokens(temp.path().into(), transport.clone());
        let (cancel, _wait) = tokio::sync::oneshot::channel();
        manager.login.lock().await.pending = Some(PendingLogin {
            state: "expected".into(),
            verifier: "verifier".into(),
            redirect: "http://localhost:12345/callback".into(),
            cancel,
        });
        assert!(manager
            .complete(&std::collections::HashMap::from([
                ("state".into(), "expected".into()),
                ("error".into(), "access_denied".into())
            ]))
            .await
            .is_err());
        let view = manager.view().await.unwrap();
        assert!(!view.login_pending);
        assert!(view.login_error.is_some());
        assert!(view.accounts.is_empty());
        assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
    }
    fn account(
        id: &str,
        gate: AccountGate,
        five_hour_used: f64,
        resets_at: Option<String>,
    ) -> StoredAccount {
        StoredAccount {
            id: id.into(),
            label: format!("{id} display"),
            access_token: format!("secret-access-{id}"),
            refresh_token: format!("secret-refresh-{id}"),
            expires_at: chrono::Utc::now().timestamp() + 3600,
            refresh_blocked: false,
            gate,
            usage: Some(AccountUsage {
                observed_at: chrono::Utc::now().timestamp(),
                five_hour: Some(UsageWindow {
                    utilization: five_hour_used,
                    resets_at,
                }),
                seven_day: Some(UsageWindow {
                    utilization: 10.0,
                    resets_at: None,
                }),
            }),
            usage_checked_at: 0,
        }
    }
    async fn install(manager: &CodeAccounts, accounts: Vec<StoredAccount>) {
        let data = StoredAccounts {
            selected: Some(accounts[0].id.clone()),
            accounts,
            switched_from: None,
        };
        manager.save(&data).unwrap();
        *manager.stored.lock().await = Some(data);
    }
    const FIVE_HOUR_20: AccountGate = AccountGate {
        five_hour: 20,
        seven_day: 0,
    };
    #[tokio::test]
    async fn gate_moves_the_next_request_to_an_account_under_its_gate() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CodeAccounts::with_tokens(temp.path().into(), tokens(false));
        install(
            &manager,
            vec![
                account("A", FIVE_HOUR_20, 85.0, None),
                account("B", FIVE_HOUR_20, 90.0, None),
                account("C", AccountGate::default(), 99.0, None),
            ],
        )
        .await;
        assert_eq!(
            manager.selected_token().await.unwrap().unwrap().account_id,
            "C"
        );
        let view = manager.view().await.unwrap();
        assert_eq!(view.selected.as_deref(), Some("C"));
        assert_eq!(view.switched_from.as_deref(), Some("A"));
        assert_eq!(
            view.accounts.iter().map(|a| a.gated).collect::<Vec<_>>(),
            [true, true, false]
        );
        let restarted = CodeAccounts::with_tokens(temp.path().into(), tokens(false));
        let view = restarted.view().await.unwrap();
        assert_eq!(view.selected.as_deref(), Some("C"));
        assert_eq!(view.accounts[0].gate, FIVE_HOUR_20);
        manager.select(Some("A".into())).await.unwrap();
        assert!(manager.view().await.unwrap().switched_from.is_none());
    }
    #[tokio::test]
    async fn gate_reopens_after_reset_and_keeps_the_account_when_all_are_gated() {
        let temp = tempfile::tempdir().unwrap();
        let manager = CodeAccounts::with_tokens(temp.path().into(), tokens(false));
        let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
        install(
            &manager,
            vec![
                account("A", FIVE_HOUR_20, 95.0, Some(past)),
                account("B", FIVE_HOUR_20, 90.0, None),
            ],
        )
        .await;
        assert_eq!(
            manager.selected_token().await.unwrap().unwrap().account_id,
            "A"
        );
        install(
            &manager,
            vec![
                account("A", FIVE_HOUR_20, 95.0, None),
                account(
                    "B",
                    AccountGate {
                        five_hour: 0,
                        seven_day: 95,
                    },
                    0.0,
                    None,
                ),
            ],
        )
        .await;
        assert_eq!(
            manager.selected_token().await.unwrap().unwrap().account_id,
            "A"
        );
        let view = manager.view().await.unwrap();
        assert!(view.switched_from.is_none());
        manager
            .set_gate(
                "A",
                AccountGate {
                    five_hour: 0,
                    seven_day: 0,
                },
            )
            .await
            .unwrap();
        assert!(!manager.view().await.unwrap().accounts[0].gated);
        assert!(manager.set_gate("missing", FIVE_HOUR_20).await.is_err());
    }
    #[test]
    fn usage_is_typed_and_unknown_shapes_are_unavailable() {
        assert!(usage_window(
            &json!({"five_hour":{"utilization":50,"resets_at":"2026-10-19T10:00:00Z"}}),
            "five_hour"
        )
        .is_some());
        assert!(usage_window(&json!({"five_hour":{"utilization":500}}), "five_hour").is_none());
        assert!(usage_window(&json!({}), "five_hour").is_none());
    }
}
