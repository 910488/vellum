use crate::codex_oauth::{AppliedOAuth, CodexOAuthManager, OAuthError};
use crate::model::{QuotaPeriodUnit, QuotaSnapshot};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex;

const TRIGGER_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
pub const TRIGGER_MODEL: &str = "gpt-5.6-luna";
pub const TRIGGER_EFFORT: &str = "none";
const RESET_SAFETY_DELAY: Duration = Duration::from_secs(2);
const VERIFY_DELAY: Duration = Duration::from_secs(3);
const ZERO_WINDOW_SAMPLE_DELAY: Duration = Duration::from_secs(3);
const RECENT_TRIGGER_COOLDOWN: Duration = Duration::from_secs(15 * 60);
const ERROR_RETRY_DELAY: Duration = Duration::from_secs(15 * 60);
const VERIFY_RETRY_DELAY: Duration = Duration::from_secs(2 * 60);
const IDLE_DELAY: Duration = Duration::from_secs(24 * 60 * 60);
static TRIGGER_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug, thiserror::Error)]
pub enum CadenceError {
    #[error("{0}")]
    OAuth(#[from] OAuthError),
    #[error("無法建立 5 小時視窗連線：{0}")]
    Client(String),
    #[error("5 小時視窗請求失敗：{0}")]
    Request(reqwest::Error),
    #[error("{message}（HTTP {status}）")]
    Upstream {
        status: reqwest::StatusCode,
        message: String,
    },
}

fn trigger_client() -> Result<&'static reqwest::Client, CadenceError> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    match CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(45))
            .build()
            .map_err(|error| error.to_string())
    }) {
        Ok(client) => Ok(client),
        Err(message) => Err(CadenceError::Client(message.clone())),
    }
}

pub(crate) fn trigger_body() -> Value {
    serde_json::json!({
        "model": TRIGGER_MODEL,
        "reasoning": { "effort": TRIGGER_EFFORT },
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

async fn send_trigger(auth: &AppliedOAuth) -> Result<(), CadenceError> {
    let response = trigger_client()?
        .post(TRIGGER_URL)
        .bearer_auth(&auth.access_token)
        .header("ChatGPT-Account-Id", &auth.account_id)
        .header("User-Agent", "codex-cli")
        .header("Accept", "text/event-stream")
        .header("OpenAI-Beta", "responses=experimental")
        .json(&trigger_body())
        .send()
        .await
        .map_err(CadenceError::Request)?;
    let status = response.status();
    let body = response.text().await.map_err(CadenceError::Request)?;
    if status.is_success() {
        return Ok(());
    }
    let message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "無法啟動 5 小時視窗".into());
    Err(CadenceError::Upstream {
        status,
        message: message.chars().take(500).collect(),
    })
}

/// Send exactly one tiny request for an account. The lock is shared by the
/// automatic scheduler and the diagnostic/manual command to prevent races.
pub async fn trigger_account(
    manager: &Arc<CodexOAuthManager>,
    account_id: &str,
) -> Result<(), CadenceError> {
    let _guard = TRIGGER_LOCK.lock().await;
    let auth = manager.valid_auth_for(account_id).await?;
    match send_trigger(&auth).await {
        Ok(()) => Ok(()),
        Err(CadenceError::Upstream {
            status: reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN,
            ..
        }) => {
            let refreshed = manager
                .refresh_after_rejection(&auth.credential_id, &auth.access_token)
                .await?;
            send_trigger(&refreshed).await
        }
        Err(error) => Err(error),
    }
}

async fn query_quota(
    manager: &Arc<CodexOAuthManager>,
    account_id: &str,
) -> Result<Vec<QuotaSnapshot>, String> {
    let mut auth = manager
        .valid_auth_for(account_id)
        .await
        .map_err(|error| error.to_string())?;
    match crate::codex_quota::query(
        &auth.access_token,
        &auth.account_id,
        &auth.credential_id,
        true,
    )
    .await
    {
        Ok(windows) => Ok(windows),
        Err(crate::codex_quota::CodexQuotaError::Unauthorized) => {
            auth = manager
                .refresh_after_rejection(&auth.credential_id, &auth.access_token)
                .await
                .map_err(|error| error.to_string())?;
            crate::codex_quota::query(
                &auth.access_token,
                &auth.account_id,
                &auth.credential_id,
                true,
            )
            .await
            .map_err(|error| error.to_string())
        }
        Err(error) => Err(error.to_string()),
    }
}

fn is_five_hour(window: &QuotaSnapshot) -> bool {
    window.period.unit == QuotaPeriodUnit::Hour && window.period.amount == Some(5)
}

fn is_weekly(window: &QuotaSnapshot) -> bool {
    window.period.unit == QuotaPeriodUnit::Week
}

fn reset_timestamp(window: &QuotaSnapshot) -> Option<i64> {
    window
        .reset_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.timestamp())
}

fn delay_until(timestamp: i64, now: i64) -> Duration {
    Duration::from_secs(timestamp.saturating_sub(now).max(1) as u64)
}

fn account_tag(account_id: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(account_id.as_bytes()));
    digest[..12].to_owned()
}

#[derive(Debug, Clone, Copy)]
struct FiveHourObservation {
    used_percent: f64,
    reset_at: i64,
}

fn five_hour_observation(windows: &[QuotaSnapshot]) -> Option<FiveHourObservation> {
    let window = windows.iter().find(|window| is_five_hour(window))?;
    Some(FiveHourObservation {
        used_percent: window.used_percent,
        reset_at: reset_timestamp(window)?,
    })
}

/// An unused upstream window reports a projection approximately equal to
/// `observed_at + 5h`; each read moves that projection forward. An active
/// window keeps one reset timestamp even when its tiny usage rounds to 0%.
fn reset_moves_with_clock(
    first_reset: i64,
    second_reset: i64,
    first_observed_at: i64,
    second_observed_at: i64,
) -> bool {
    let elapsed = second_observed_at.saturating_sub(first_observed_at);
    let reset_shift = second_reset.saturating_sub(first_reset);
    reset_shift >= elapsed.saturating_sub(1).max(2)
}

async fn sample_zero_window(
    manager: &Arc<CodexOAuthManager>,
    account_id: &str,
    first: FiveHourObservation,
) -> Result<(FiveHourObservation, bool), String> {
    let first_observed_at = Utc::now().timestamp();
    tokio::time::sleep(ZERO_WINDOW_SAMPLE_DELAY).await;
    let windows = query_quota(manager, account_id).await?;
    let second = five_hour_observation(&windows)
        .ok_or_else(|| "second quota sample has no usable 5-hour window".to_string())?;
    let second_observed_at = Utc::now().timestamp();
    let sliding = second.used_percent <= f64::EPSILON
        && reset_moves_with_clock(
            first.reset_at,
            second.reset_at,
            first_observed_at,
            second_observed_at,
        );
    Ok((second, sliding))
}

fn recent_trigger_delay(
    recent_triggers: &HashMap<String, i64>,
    account_id: &str,
) -> Option<Duration> {
    let elapsed = Utc::now()
        .timestamp()
        .saturating_sub(*recent_triggers.get(account_id)?);
    let cooldown = RECENT_TRIGGER_COOLDOWN.as_secs() as i64;
    (elapsed < cooldown).then(|| Duration::from_secs((cooldown - elapsed).max(1) as u64))
}

async fn verify_trigger(
    manager: &Arc<CodexOAuthManager>,
    account_id: &str,
    tag: &str,
) -> Option<i64> {
    tokio::time::sleep(VERIFY_DELAY).await;
    let first_windows = match query_quota(manager, account_id).await {
        Ok(windows) => windows,
        Err(error) => {
            log::warn!("[FiveHourCadence] event=verify_error account={tag} error={error}");
            return None;
        }
    };
    let Some(first) = five_hour_observation(&first_windows) else {
        log::warn!("[FiveHourCadence] event=verify_error account={tag} error=missing_5h_window");
        return None;
    };
    if first.used_percent > f64::EPSILON {
        return Some(first.reset_at);
    }
    match sample_zero_window(manager, account_id, first).await {
        Ok((second, false)) => Some(second.reset_at),
        Ok((second, true)) => {
            log::warn!(
                "[FiveHourCadence] event=trigger_unverified account={tag} reason=reset_still_sliding first_reset_at={} second_reset_at={}",
                first.reset_at,
                second.reset_at
            );
            None
        }
        Err(error) => {
            log::warn!("[FiveHourCadence] event=verify_error account={tag} error={error}");
            None
        }
    }
}

async fn inspect_account(
    manager: &Arc<CodexOAuthManager>,
    account_id: &str,
    weekly_floor: u8,
    recent_triggers: &mut HashMap<String, i64>,
) -> Duration {
    let tag = account_tag(account_id);
    let windows = match query_quota(manager, account_id).await {
        Ok(windows) => windows,
        Err(error) => {
            log::warn!("[FiveHourCadence] event=quota_error account={tag} error={error}");
            return ERROR_RETRY_DELAY;
        }
    };
    let Some(mut five_hour) = five_hour_observation(&windows) else {
        log::warn!("[FiveHourCadence] event=skip account={tag} reason=missing_5h_window");
        return ERROR_RETRY_DELAY;
    };
    let Some(weekly) = windows.iter().find(|window| is_weekly(window)) else {
        log::warn!("[FiveHourCadence] event=skip account={tag} reason=missing_weekly_window");
        return ERROR_RETRY_DELAY;
    };
    let weekly_remaining = (100.0 - weekly.used_percent).clamp(0.0, 100.0);
    if weekly_remaining <= f64::from(weekly_floor) {
        let retry = reset_timestamp(weekly)
            .map(|reset| {
                delay_until(
                    reset + RESET_SAFETY_DELAY.as_secs() as i64,
                    Utc::now().timestamp(),
                )
            })
            .unwrap_or(ERROR_RETRY_DELAY);
        log::info!(
            "[FiveHourCadence] event=skip account={tag} reason=weekly_gate remaining={weekly_remaining:.2} floor={weekly_floor}"
        );
        return retry;
    }
    let mut trigger_reason = "expired_window";
    if five_hour.used_percent <= f64::EPSILON {
        match sample_zero_window(manager, account_id, five_hour).await {
            Ok((second, sliding)) => {
                log::info!(
                    "[FiveHourCadence] event=window_probe account={tag} first_reset_at={} second_reset_at={} sliding={sliding}",
                    five_hour.reset_at,
                    second.reset_at
                );
                five_hour = second;
                if sliding {
                    trigger_reason = "inactive_sliding_reset";
                } else {
                    let due_at = five_hour.reset_at + RESET_SAFETY_DELAY.as_secs() as i64;
                    if due_at > Utc::now().timestamp() {
                        recent_triggers.remove(account_id);
                        log::info!(
                            "[FiveHourCadence] event=scheduled account={tag} used_percent={:.2} reset_at={} due_at={} reset_stable=true",
                            five_hour.used_percent,
                            five_hour.reset_at,
                            due_at
                        );
                        return delay_until(due_at, Utc::now().timestamp());
                    }
                }
            }
            Err(error) => {
                log::warn!("[FiveHourCadence] event=quota_probe_error account={tag} error={error}");
                return ERROR_RETRY_DELAY;
            }
        }
    }

    let now = Utc::now().timestamp();
    let due_at = five_hour.reset_at + RESET_SAFETY_DELAY.as_secs() as i64;
    if trigger_reason == "expired_window" && due_at > now {
        recent_triggers.remove(account_id);
        log::info!(
            "[FiveHourCadence] event=scheduled account={tag} used_percent={:.2} reset_at={} due_at={}",
            five_hour.used_percent,
            five_hour.reset_at,
            due_at
        );
        return delay_until(due_at, now);
    }

    if let Some(delay) = recent_trigger_delay(recent_triggers, account_id) {
        log::info!(
            "[FiveHourCadence] event=awaiting_observation account={tag} reason=recent_trigger cooldown_seconds={}",
            delay.as_secs()
        );
        return delay;
    }

    log::info!(
        "[FiveHourCadence] event=trigger_start account={tag} reason={trigger_reason} observed_reset_at={} model={TRIGGER_MODEL} effort={TRIGGER_EFFORT}",
        five_hour.reset_at
    );
    if let Err(error) = trigger_account(manager, account_id).await {
        log::warn!("[FiveHourCadence] event=trigger_error account={tag} error={error}");
        return ERROR_RETRY_DELAY;
    }
    recent_triggers.insert(account_id.to_owned(), Utc::now().timestamp());
    if let Some(new_reset) = verify_trigger(manager, account_id, &tag).await {
        recent_triggers.remove(account_id);
        log::info!(
            "[FiveHourCadence] event=trigger_verified account={tag} new_reset_at={new_reset} reset_stable=true"
        );
        delay_until(
            new_reset + RESET_SAFETY_DELAY.as_secs() as i64,
            Utc::now().timestamp(),
        )
    } else {
        recent_trigger_delay(recent_triggers, account_id).unwrap_or(VERIFY_RETRY_DELAY)
    }
}

async fn run_cycle(
    manager: &Arc<CodexOAuthManager>,
    recent_triggers: &mut HashMap<String, i64>,
) -> Duration {
    let settings = manager.quota_pool_status().await.settings;
    let members: Vec<_> = settings
        .members
        .into_iter()
        .filter(|member| member.maintain_five_hour_window)
        .collect();
    if members.is_empty() {
        return IDLE_DELAY;
    }
    let mut next = IDLE_DELAY;
    for member in members {
        next = next.min(
            inspect_account(
                manager,
                &member.account_id,
                member.weekly_floor,
                recent_triggers,
            )
            .await,
        );
    }
    next
}

pub fn spawn(manager: Arc<CodexOAuthManager>) {
    let notify = manager.cadence_notifier();
    tauri::async_runtime::spawn(async move {
        let mut recent_triggers = HashMap::new();
        log::info!(
            "[FiveHourCadence] event=scheduler_start model={TRIGGER_MODEL} effort={TRIGGER_EFFORT} safety_delay_seconds={}",
            RESET_SAFETY_DELAY.as_secs()
        );
        loop {
            let delay = run_cycle(&manager, &mut recent_triggers).await;
            tokio::select! {
                _ = tokio::time::sleep(delay) => {},
                _ = notify.notified() => {
                    log::info!("[FiveHourCadence] event=settings_changed");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiny_trigger_uses_luna_with_no_reasoning() {
        let body = trigger_body();
        assert_eq!(body["model"], TRIGGER_MODEL);
        assert_eq!(body["reasoning"]["effort"], "none");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn detects_unused_reset_projection_moving_with_observation_clock() {
        assert!(reset_moves_with_clock(10_000, 10_004, 5_000, 5_004));
        assert!(reset_moves_with_clock(10_000, 10_003, 5_000, 5_004));
    }

    #[test]
    fn keeps_stable_active_reset_even_when_usage_rounds_to_zero() {
        assert!(!reset_moves_with_clock(10_000, 10_000, 5_000, 5_004));
        assert!(!reset_moves_with_clock(10_000, 10_001, 5_000, 5_004));
    }
}
