//! Codex profile activity returned by ChatGPT's authoritative profile service.
//!
//! The Codex desktop profile does not derive its heatmap from `state_5.sqlite`.
//! It reads `/backend-api/wham/profiles/me` and renders these fields directly.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

const PROFILE_URL: &str = "https://chatgpt.com/backend-api/wham/profiles/me";
// Codex desktop's profile query defaults to SIX_HOURS of stale time.
const PROFILE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone)]
struct CachedProfile {
    fetched_at: std::time::Instant,
    usage: CodexProfileUsage,
}

fn profile_cache() -> &'static tokio::sync::RwLock<HashMap<String, CachedProfile>> {
    static CACHE: OnceLock<tokio::sync::RwLock<HashMap<String, CachedProfile>>> = OnceLock::new();
    CACHE.get_or_init(|| tokio::sync::RwLock::new(HashMap::new()))
}

#[derive(Debug, thiserror::Error)]
pub enum CodexProfileError {
    #[error("OpenAI OAuth token was rejected")]
    Unauthorized,
    #[error("OpenAI profile request failed: {0}")]
    Request(String),
    #[error("OpenAI profile response could not be parsed: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexProfileDay {
    pub date: String,
    pub tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexProfileUsage {
    pub daily_usage: Vec<CodexProfileDay>,
    pub lifetime_tokens: u64,
    pub peak_daily_tokens: u64,
    pub current_streak_days: u32,
    pub longest_streak_days: u32,
    pub longest_task_duration_ms: u64,
}

pub async fn query(
    access_token: &str,
    account_id: &str,
    force_refresh: bool,
) -> Result<CodexProfileUsage, CodexProfileError> {
    if !force_refresh {
        if let Some(cached) = profile_cache().read().await.get(account_id).cloned() {
            if cached.fetched_at.elapsed() < PROFILE_CACHE_TTL {
                return Ok(cached.usage);
            }
        }
    }
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|error| CodexProfileError::Request(error.to_string()))?;
    let mut request = client
        .get(PROFILE_URL)
        .bearer_auth(access_token)
        .header("ChatGPT-Account-Id", account_id)
        .header("User-Agent", "codex-cli")
        .header("Accept", "application/json");
    if force_refresh {
        request = request
            .header(reqwest::header::CACHE_CONTROL, "no-cache, no-store")
            .header(reqwest::header::PRAGMA, "no-cache")
            .query(&[("_vellum_refresh", chrono::Utc::now().timestamp_millis())]);
    }

    let response = request
        .send()
        .await
        .map_err(|error| CodexProfileError::Request(error.to_string()))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| CodexProfileError::Request(error.to_string()))?;
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        return Err(CodexProfileError::Unauthorized);
    }
    if !status.is_success() {
        return Err(CodexProfileError::Request(format!(
            "HTTP {status}: {}",
            String::from_utf8_lossy(&bytes)
                .chars()
                .take(300)
                .collect::<String>()
        )));
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| CodexProfileError::Parse(error.to_string()))?;
    let profile = parse_profile(&value)?;
    profile_cache().write().await.insert(
        account_id.to_owned(),
        CachedProfile {
            fetched_at: std::time::Instant::now(),
            usage: profile.clone(),
        },
    );
    Ok(profile)
}

pub fn parse_profile(value: &Value) -> Result<CodexProfileUsage, CodexProfileError> {
    let stats = value
        .get("stats")
        .and_then(Value::as_object)
        .ok_or_else(|| CodexProfileError::Parse("missing stats".into()))?;
    let daily_usage = stats
        .get("daily_usage_buckets")
        .and_then(Value::as_array)
        .map(|buckets| {
            buckets
                .iter()
                .filter_map(|bucket| {
                    Some(CodexProfileDay {
                        date: bucket.get("start_date")?.as_str()?.to_owned(),
                        tokens: bucket.get("tokens")?.as_u64()?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(CodexProfileUsage {
        daily_usage,
        lifetime_tokens: u64_field(stats, "lifetime_tokens")?,
        peak_daily_tokens: u64_field(stats, "peak_daily_tokens")?,
        current_streak_days: u64_field(stats, "current_streak_days")?
            .try_into()
            .unwrap_or(u32::MAX),
        longest_streak_days: u64_field(stats, "longest_streak_days")?
            .try_into()
            .unwrap_or(u32::MAX),
        longest_task_duration_ms: stats
            .get("longest_running_turn_sec")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_mul(1_000),
    })
}

fn u64_field(
    stats: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<u64, CodexProfileError> {
    stats
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| CodexProfileError::Parse(format!("missing or invalid stats.{field}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_the_same_fields_used_by_codex_profile() {
        let profile = parse_profile(&json!({
            "stats": {
                "daily_usage_buckets": [
                    {"start_date": "2026-07-19", "tokens": 324455603},
                    {"start_date": "2026-07-20", "tokens": 23619995}
                ],
                "lifetime_tokens": 4085208823_u64,
                "peak_daily_tokens": 324455603,
                "current_streak_days": 0,
                "longest_streak_days": 92,
                "longest_running_turn_sec": 6692
            }
        }))
        .unwrap();
        assert_eq!(profile.daily_usage.len(), 2);
        assert_eq!(profile.daily_usage[0].tokens, 324_455_603);
        assert_eq!(profile.lifetime_tokens, 4_085_208_823);
        assert_eq!(profile.longest_task_duration_ms, 6_692_000);
    }

    #[test]
    fn rejects_bodies_without_authoritative_totals() {
        assert!(parse_profile(&json!({"stats": {}})).is_err());
    }

    #[tokio::test]
    #[ignore = "requires a Vellum-managed OpenAI OAuth account and network access"]
    async fn live_managed_oauth_profile_query() {
        let root = dirs::data_local_dir().unwrap().join("vellum");
        let manager = crate::codex_oauth::CodexOAuthManager::new(root);
        let accounts = manager.status().await.accounts;
        assert!(
            !accounts.is_empty(),
            "Vellum has no managed OpenAI OAuth account"
        );
        let mut lifetime_tokens = 0_u64;
        for account in accounts {
            let auth = manager.valid_auth_for(&account.account_id).await.unwrap();
            let profile = query(&auth.access_token, &auth.account_id, true)
                .await
                .unwrap();
            assert!(profile.lifetime_tokens >= profile.peak_daily_tokens);
            assert!(!profile.daily_usage.is_empty());
            lifetime_tokens = lifetime_tokens.saturating_add(profile.lifetime_tokens);
        }
        assert!(lifetime_tokens > 0);
    }
}
