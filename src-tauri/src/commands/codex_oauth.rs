use crate::codex_oauth::{CodexOAuthAccount, CodexOAuthStatus, DeviceLogin, OAuthError};
use crate::error::{AppError, AppResult};
use crate::model::QuotaSnapshot;
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;
use tokio::sync::Mutex;

const RESET_CREDITS_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const FIVE_HOUR_TRIGGER_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const FIVE_HOUR_TRIGGER_MODEL: &str = "gpt-5.6-luna";
static RESET_LOCK: Mutex<()> = Mutex::const_new(());
static FIVE_HOUR_TRIGGER_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexResetCredit {
    pub id: String,
    #[serde(default, alias = "reset_type")]
    pub reset_type: Option<String>,
    pub status: String,
    #[serde(default, alias = "expires_at")]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexResetCredits {
    #[serde(default, alias = "available_count")]
    pub available_count: i64,
    #[serde(default)]
    pub credits: Vec<CodexResetCredit>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexResetResult {
    pub code: String,
    pub windows_reset: Option<i64>,
}

fn app_error(error: OAuthError) -> AppError {
    AppError::Message(error.to_string())
}

fn reset_client() -> AppResult<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| AppError::Message(format!("無法建立 Reset 連線：{error}")))
}

fn safe_upstream_error(status: reqwest::StatusCode, body: &str, fallback: &str) -> AppError {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| fallback.to_owned());
    let message: String = message.chars().take(500).collect();
    AppError::Message(format!("{message}（HTTP {status}）"))
}

fn redeem_request_id() -> AppResult<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| AppError::Message(format!("無法產生 Reset 請求識別碼：{error}")))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15],
    ))
}

async fn fetch_reset_credits(
    client: &reqwest::Client,
    access_token: &str,
    account_id: &str,
) -> AppResult<CodexResetCredits> {
    let response = client
        .get(RESET_CREDITS_URL)
        .bearer_auth(access_token)
        .header("ChatGPT-Account-Id", account_id)
        .header("User-Agent", "codex-cli")
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|error| AppError::Message(format!("無法查詢 Reset：{error}")))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| AppError::Message(format!("無法讀取 Reset 回應：{error}")))?;
    if !status.is_success() {
        return Err(safe_upstream_error(status, &body, "無法查詢 Reset"));
    }
    serde_json::from_str(&body)
        .map_err(|error| AppError::Message(format!("Reset 回應格式無效：{error}")))
}

#[tauri::command]
pub async fn get_codex_oauth_status(state: State<'_, AppState>) -> AppResult<CodexOAuthStatus> {
    Ok(state.codex_oauth().status().await)
}

fn five_hour_trigger_client() -> AppResult<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|error| AppError::Message(format!("無法建立 5 小時視窗啟動連線：{error}")))
}

fn five_hour_trigger_body() -> Value {
    serde_json::json!({
        "model": FIVE_HOUR_TRIGGER_MODEL,
        "store": false,
        "stream": true,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": "Reply with exactly OK."
            }]
        }]
    })
}

async fn send_five_hour_trigger(
    client: &reqwest::Client,
    access_token: &str,
    account_id: &str,
) -> Result<(), (reqwest::StatusCode, String)> {
    let response = client
        .post(FIVE_HOUR_TRIGGER_URL)
        .bearer_auth(access_token)
        .header("ChatGPT-Account-Id", account_id)
        .header("User-Agent", "codex-cli")
        .header("Accept", "text/event-stream")
        .header("OpenAI-Beta", "responses=experimental")
        .json(&five_hour_trigger_body())
        .send()
        .await
        .map_err(|error| {
            (
                reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                format!("5 小時視窗啟動請求失敗：{error}"),
            )
        })?;
    let status = response.status();
    let body = response.text().await.map_err(|error| {
        (
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            format!("無法讀取 5 小時視窗啟動回應：{error}"),
        )
    })?;
    if !status.is_success() {
        return Err((status, body));
    }
    Ok(())
}

#[tauri::command]
pub async fn get_codex_quota_pool(
    state: State<'_, AppState>,
) -> AppResult<crate::codex_oauth::QuotaPoolStatus> {
    Ok(state.codex_oauth().quota_pool_status().await)
}

#[tauri::command]
pub async fn set_codex_quota_pool(
    settings: crate::codex_oauth::QuotaPoolSettings,
    state: State<'_, AppState>,
) -> AppResult<crate::codex_oauth::QuotaPoolStatus> {
    state
        .codex_oauth()
        .set_quota_pool(settings)
        .await
        .map_err(app_error)
}

#[tauri::command]
pub async fn start_codex_oauth_login(
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<DeviceLogin> {
    let login = state
        .codex_oauth()
        .start_device_flow()
        .await
        .map_err(app_error)?;
    app.opener()
        .open_url(&login.verification_uri, None::<String>)
        .map_err(|error| AppError::Message(format!("無法開啟 Codex 登入頁面：{error}")))?;
    Ok(login)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn poll_codex_oauth_login(
    device_code: String,
    state: State<'_, AppState>,
) -> AppResult<Option<CodexOAuthAccount>> {
    match state
        .codex_oauth()
        .poll_device_flow(device_code.trim())
        .await
    {
        Ok(account) => Ok(account),
        Err(OAuthError::AuthorizationPending) => Ok(None),
        Err(error) => Err(app_error(error)),
    }
}

#[tauri::command(rename_all = "camelCase")]
pub async fn set_default_codex_oauth_account(
    account_id: String,
    state: State<'_, AppState>,
) -> AppResult<CodexOAuthStatus> {
    let account_id = account_id.trim().to_string();
    let manager = state.codex_oauth();
    let previous = manager.status().await;
    let peers = crate::enhanced_runtime::process_info::leftover_vellum_hosts();
    if !state.proxy_status().running && !peers.is_empty() {
        return Err(AppError::Message(format!(
            "OfficialAccountInjectorUnavailable: 另一個 Vellum 程序仍在注入 Official 帳號（{}）。\
             這個程序寫入的預設帳號不會被執行中的 Codex 使用。請先關閉該程序，再由這個 Vellum 啟動 Proxy。",
            crate::enhanced_runtime::process_info::format_process_images(&peers)
        )));
    }
    manager.set_default(&account_id).await.map_err(app_error)?;
    let selected = manager.status().await;
    if selected.default_account_id.as_deref() != Some(account_id.as_str())
        || (previous.default_account_id.as_deref() != Some(account_id.as_str())
            && selected.selection_revision <= previous.selection_revision)
        || !selected.selection_verified
    {
        return Err(AppError::Message(
            "authentication_failed: Official account selection was not verified".into(),
        ));
    }
    let _ = state.usage_store().record_diagnostic_event(
        &vellum_proxy_runtime::DiagnosticEvent::OfficialAccountSelected(
            vellum_proxy_runtime::OfficialAccountSelected {
                previous_account_hash: previous
                    .default_account_id
                    .as_deref()
                    .map(vellum_proxy_runtime::account_hash),
                new_account_hash: vellum_proxy_runtime::account_hash(&account_id),
                selection_revision: selected.selection_revision,
                source: "desktop_command".into(),
                selection_verified: selected.selection_verified,
            },
        ),
    );
    if crate::enhanced_runtime::desktop_runtime_status(&state.data_root()).active {
        state.record_live_applied(crate::model::RuntimeNotice::new(
            "officialAccountSwitchNativePlane",
        ));
    }
    // The switch follows one host: the one Remote Manager is showing. Fanning
    // out to the whole inventory meant every machine in it silently changed
    // identity on a Desktop click, and a host that was asleep, unreachable, or
    // had never paired this account just failed into a discarded Result. One
    // host is a change the user can see, on the screen where they can see it;
    // the rest are reconciled from Remote Manager, which reports drift against
    // Desktop and offers to align.
    //
    // Still off the calling path, because a switch must not wait on SSH. That
    // is also why the outcome becomes a notice rather than an error: this
    // Desktop-side selection has already been verified and committed by the
    // time we get here, and a remote that could not follow does not undo it.
    let owned = state.inner().clone();
    let sync_account_id = account_id.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let Ok(Some(host_id)) = owned.remote().active_host() else {
            return;
        };
        let outcome =
            crate::remote::RemoteHostManager::resolve_target(&owned, &host_id).and_then(|target| {
                crate::remote::RemoteAgentClient::new(target).codex_account_activate(
                    &format!("account-switch-{}", ulid::Ulid::new()),
                    &sync_account_id,
                )
            });
        match outcome {
            Ok(_) => owned.remote().invalidate_snapshot(&host_id),
            Err(error) => owned.record_live_applied(
                crate::model::RuntimeNotice::new("remoteOfficialAccountSwitchNotFollowed")
                    .with("host", host_id)
                    .with("detail", error.to_string()),
            ),
        }
    });
    Ok(selected)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn remove_codex_oauth_account(
    account_id: String,
    state: State<'_, AppState>,
) -> AppResult<CodexOAuthStatus> {
    let manager = state.codex_oauth();
    manager
        .remove_account(account_id.trim())
        .await
        .map_err(app_error)?;
    Ok(manager.status().await)
}

#[tauri::command]
pub async fn logout_codex_oauth(state: State<'_, AppState>) -> AppResult<CodexOAuthStatus> {
    let manager = state.codex_oauth();
    manager.clear().await.map_err(app_error)?;
    Ok(manager.status().await)
}

#[tauri::command]
pub async fn refresh_codex_oauth(state: State<'_, AppState>) -> AppResult<CodexOAuthStatus> {
    let manager = state.codex_oauth();
    manager.force_refresh_default().await.map_err(app_error)?;
    Ok(manager.status().await)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn get_codex_oauth_reset_credits(
    account_id: String,
    state: State<'_, AppState>,
) -> AppResult<CodexResetCredits> {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return Err(AppError::Message("缺少 ChatGPT 帳號識別碼".into()));
    }
    let auth = state
        .codex_oauth()
        .valid_auth_for(account_id)
        .await
        .map_err(app_error)?;
    fetch_reset_credits(&reset_client()?, &auth.access_token, &auth.account_id).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn consume_codex_oauth_reset(
    account_id: String,
    credit_id: String,
    state: State<'_, AppState>,
) -> AppResult<CodexResetResult> {
    // Reset credit 是稀缺且非冪等的資源。驗證與消耗必須序列化，
    // 避免雙擊或多個視窗同時消耗同一筆 credit。
    let _guard = RESET_LOCK
        .try_lock()
        .map_err(|_| AppError::Message("另一筆 Reset 正在執行，請稍候再試。".into()))?;
    let account_id = account_id.trim();
    let credit_id = credit_id.trim();
    if account_id.is_empty() || credit_id.is_empty() {
        return Err(AppError::Message("缺少 ChatGPT 帳號或 Reset 識別碼".into()));
    }

    let auth = state
        .codex_oauth()
        .valid_auth_for(account_id)
        .await
        .map_err(app_error)?;
    let client = reset_client()?;
    let credits = fetch_reset_credits(&client, &auth.access_token, &auth.account_id).await?;
    if !credits
        .credits
        .iter()
        .any(|credit| credit.id == credit_id && credit.status == "available")
    {
        return Err(AppError::Message(
            "選取的 Reset 已不可使用，請重新整理。".into(),
        ));
    }

    let response = client
        .post(format!("{RESET_CREDITS_URL}/consume"))
        .bearer_auth(&auth.access_token)
        .header("ChatGPT-Account-Id", &auth.account_id)
        .header("User-Agent", "codex-cli")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "redeem_request_id": redeem_request_id()?,
            "credit_id": credit_id,
        }))
        .send()
        .await
        .map_err(|error| AppError::Message(format!("Reset 請求失敗：{error}")))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| AppError::Message(format!("無法讀取 Reset 回應：{error}")))?;
    if !status.is_success() {
        return Err(safe_upstream_error(status, &body, "Reset 失敗"));
    }
    let value: Value = serde_json::from_str(&body)
        .map_err(|error| AppError::Message(format!("Reset 回應格式無效：{error}")))?;
    Ok(CodexResetResult {
        code: value
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        windows_reset: value.get("windows_reset").and_then(Value::as_i64),
    })
}

#[tauri::command(rename_all = "camelCase")]
pub async fn get_codex_oauth_account_quota(
    account_id: String,
    force_refresh: Option<bool>,
    state: State<'_, AppState>,
) -> AppResult<Vec<QuotaSnapshot>> {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return Err(AppError::Message("缺少 ChatGPT 帳號識別碼".into()));
    }
    let manager = state.codex_oauth();
    let auth = manager
        .valid_auth_for(account_id)
        .await
        .map_err(app_error)?;
    match crate::codex_quota::query(
        &auth.access_token,
        &auth.account_id,
        &auth.credential_id,
        force_refresh.unwrap_or(false),
    )
    .await
    {
        Ok(windows) => Ok(windows),
        Err(crate::codex_quota::CodexQuotaError::Unauthorized) => {
            let refreshed = manager
                .refresh_after_rejection(&auth.credential_id, &auth.access_token)
                .await
                .map_err(app_error)?;
            crate::codex_quota::query(
                &refreshed.access_token,
                &refreshed.account_id,
                &refreshed.credential_id,
                true,
            )
            .await
            .map_err(|error| AppError::Message(format!("OpenAI 額度查詢失敗：{error}")))
        }
        Err(error) => Err(AppError::Message(format!("OpenAI 額度查詢失敗：{error}"))),
    }
}

#[tauri::command(rename_all = "camelCase")]
pub async fn trigger_codex_oauth_five_hour_window(
    account_id: String,
    state: State<'_, AppState>,
) -> AppResult<()> {
    // This deliberately spends a tiny amount of real quota. Serialize it so a
    // double-click or a second Vellum window cannot submit duplicate triggers.
    let _guard = FIVE_HOUR_TRIGGER_LOCK
        .try_lock()
        .map_err(|_| AppError::Message("另一筆 5 小時視窗啟動請求正在執行，請稍候再試。".into()))?;
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return Err(AppError::Message("缺少 ChatGPT 帳號識別碼".into()));
    }

    let manager = state.codex_oauth();
    let auth = manager
        .valid_auth_for(account_id)
        .await
        .map_err(app_error)?;
    let client = five_hour_trigger_client()?;
    match send_five_hour_trigger(&client, &auth.access_token, &auth.account_id).await {
        Ok(()) => Ok(()),
        Err((reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN, _)) => {
            let refreshed = manager
                .refresh_after_rejection(&auth.credential_id, &auth.access_token)
                .await
                .map_err(app_error)?;
            send_five_hour_trigger(&client, &refreshed.access_token, &refreshed.account_id)
                .await
                .map_err(|(status, body)| safe_upstream_error(status, &body, "無法啟動 5 小時視窗"))
        }
        Err((status, body)) => Err(safe_upstream_error(status, &body, "無法啟動 5 小時視窗")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_credit_payload_accepts_upstream_snake_case() {
        let payload = r#"{
            "available_count": 1,
            "credits": [{
                "id": "credit-1",
                "reset_type": "full",
                "status": "available",
                "expires_at": "2026-08-01T00:00:00Z"
            }]
        }"#;
        let parsed: CodexResetCredits = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed.available_count, 1);
        assert_eq!(parsed.credits[0].reset_type.as_deref(), Some("full"));
        let frontend = serde_json::to_value(parsed).unwrap();
        assert_eq!(frontend["availableCount"], 1);
        assert_eq!(frontend["credits"][0]["resetType"], "full");
    }

    #[test]
    fn five_hour_trigger_is_minimal_and_non_persistent() {
        let body = five_hour_trigger_body();
        assert_eq!(body["model"], FIVE_HOUR_TRIGGER_MODEL);
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["input"][0]["content"][0]["text"],
            "Reply with exactly OK."
        );
        assert!(body.get("tools").is_none());
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn redeem_request_id_is_uuid_v4_shaped() {
        let id = redeem_request_id().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
    }
}
