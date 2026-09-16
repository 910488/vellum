use crate::model::{QuotaPeriod, QuotaPeriodUnit, QuotaSnapshot};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::OnceLock;

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const QUOTA_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone)]
struct CachedQuota {
    fetched_at: std::time::Instant,
    windows: Vec<QuotaSnapshot>,
}

fn quota_cache() -> &'static tokio::sync::RwLock<HashMap<String, CachedQuota>> {
    static CACHE: OnceLock<tokio::sync::RwLock<HashMap<String, CachedQuota>>> = OnceLock::new();
    CACHE.get_or_init(|| tokio::sync::RwLock::new(HashMap::new()))
}

#[derive(Debug, thiserror::Error)]
pub enum CodexQuotaError {
    #[error("OpenAI OAuth token was rejected")]
    Unauthorized,
    #[error("OpenAI quota request failed: {0}")]
    Request(String),
    #[error("OpenAI quota response could not be parsed: {0}")]
    Parse(String),
}

pub async fn query(
    access_token: &str,
    account_id: &str,
    route_id: &str,
    force_refresh: bool,
) -> Result<Vec<QuotaSnapshot>, CodexQuotaError> {
    // The display route id is not necessarily a credential selector. Key by a
    // one-way digest of the access token so two users in the same workspace do
    // not share quota, while no token material is retained in the cache key.
    let cache_key = format!("{account_id}:{:x}", Sha256::digest(access_token.as_bytes()));
    if !force_refresh {
        if let Some(cached) = quota_cache().read().await.get(&cache_key).cloned() {
            if cached.fetched_at.elapsed() < QUOTA_CACHE_TTL {
                return Ok(cached
                    .windows
                    .into_iter()
                    .map(|mut window| {
                        window.route_id = route_id.to_string();
                        window
                    })
                    .collect());
            }
        }
    }
    // A failed forced refresh must not resurrect an older usable reading.
    quota_cache().write().await.remove(&cache_key);
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|error| CodexQuotaError::Request(error.to_string()))?;
    let mut request = client
        .get(USAGE_URL)
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
        .map_err(|error| CodexQuotaError::Request(error.to_string()))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| CodexQuotaError::Request(error.to_string()))?;
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        return Err(CodexQuotaError::Unauthorized);
    }
    if !status.is_success() {
        return Err(CodexQuotaError::Request(format!(
            "HTTP {status}: {}",
            String::from_utf8_lossy(&bytes)
                .chars()
                .take(300)
                .collect::<String>()
        )));
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| CodexQuotaError::Parse(error.to_string()))?;
    let windows = parse_quota_windows(route_id, &value)?;
    let mut cache = quota_cache().write().await;
    cache.retain(|_, entry| entry.fetched_at.elapsed() < QUOTA_CACHE_TTL);
    cache.insert(
        cache_key,
        CachedQuota {
            fetched_at: std::time::Instant::now(),
            windows: windows.clone(),
        },
    );
    Ok(windows)
}

pub fn parse_quota_windows(
    route_id: &str,
    value: &Value,
) -> Result<Vec<QuotaSnapshot>, CodexQuotaError> {
    let rate_limit = value
        .get("rate_limit")
        .and_then(Value::as_object)
        .ok_or_else(|| CodexQuotaError::Parse("missing rate_limit".into()))?;
    let mut windows = Vec::new();
    for key in ["primary_window", "secondary_window"] {
        let Some(window) = rate_limit.get(key).and_then(Value::as_object) else {
            continue;
        };
        let Some(used_percent) = window.get("used_percent").and_then(Value::as_f64) else {
            continue;
        };
        let seconds = window.get("limit_window_seconds").and_then(Value::as_i64);
        let reset_at = window
            .get("reset_at")
            .and_then(Value::as_i64)
            .and_then(|timestamp| chrono::DateTime::from_timestamp(timestamp, 0))
            .map(|value| value.to_rfc3339());
        windows.push(QuotaSnapshot {
            route_id: route_id.into(),
            used_percent: used_percent.clamp(0.0, 100.0),
            period: period(seconds),
            reset_at,
            tier: None,
            stale: false,
        });
    }
    if windows.is_empty() {
        return Err(CodexQuotaError::Parse(
            "rate_limit contains no usable windows".into(),
        ));
    }
    Ok(windows)
}

/// 視窗長度只做成結構，句子交給前端組。604800 秒就是「一週」，不是
/// 「7 天」——命名視窗與數量視窗在別的語言裡不見得同一種寫法。
fn period(seconds: Option<i64>) -> QuotaPeriod {
    match seconds {
        Some(604_800) => QuotaPeriod::named(QuotaPeriodUnit::Week),
        Some(2_592_000) => QuotaPeriod::named(QuotaPeriodUnit::Month),
        Some(seconds) if seconds >= 86_400 => {
            QuotaPeriod::counted(QuotaPeriodUnit::Day, seconds / 86_400)
        }
        Some(seconds) if seconds >= 3_600 => {
            QuotaPeriod::counted(QuotaPeriodUnit::Hour, seconds / 3_600)
        }
        _ => QuotaPeriod::unspecified(),
    }
}

#[cfg(test)]
pub(crate) async fn seed_test_quota(token: &str, account: &str, windows: Vec<QuotaSnapshot>) {
    let key = format!("{account}:{:x}", Sha256::digest(token.as_bytes()));
    quota_cache().write().await.insert(
        key,
        CachedQuota {
            fetched_at: std::time::Instant::now(),
            windows,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn cached_quota_uses_the_callers_route_id() {
        let windows = parse_quota_windows(
            "original",
            &json!({ "rate_limit": {
                "primary_window": { "used_percent": 10, "limit_window_seconds": 18000 }
            }}),
        )
        .unwrap();
        seed_test_quota("cache-route-test", "workspace", windows).await;
        let result = query("cache-route-test", "workspace", "new-route", false)
            .await
            .unwrap();
        assert_eq!(result[0].route_id, "new-route");
    }

    #[test]
    fn parses_primary_and_secondary_codex_windows() {
        let windows = parse_quota_windows(
            "openai-official",
            &json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 21.5,
                        "limit_window_seconds": 18000,
                        "reset_at": 1_788_000_000
                    },
                    "secondary_window": {
                        "used_percent": 67.0,
                        "limit_window_seconds": 604800,
                        "reset_at": 1_788_500_000
                    }
                }
            }),
        )
        .unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].period.unit, QuotaPeriodUnit::Hour);
        assert_eq!(windows[0].period.amount, Some(5));
        assert_eq!(windows[0].used_percent, 21.5);
        assert_eq!(windows[1].period.unit, QuotaPeriodUnit::Week);
        assert_eq!(windows[1].period.amount, None);
        assert!(windows[1].reset_at.is_some());
    }

    #[test]
    fn clamps_invalid_percentages_and_ignores_missing_windows() {
        let windows = parse_quota_windows(
            "openai-official",
            &json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 140,
                        "limit_window_seconds": 2592000
                    }
                }
            }),
        )
        .unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].used_percent, 100.0);
        assert_eq!(windows[0].period.unit, QuotaPeriodUnit::Month);
    }

    #[test]
    fn rejects_success_bodies_without_rate_limit_windows() {
        assert!(parse_quota_windows("openai-official", &json!({"rate_limit": {}})).is_err());
    }

    #[tokio::test]
    #[ignore = "requires a Vellum-managed OpenAI OAuth account and network access"]
    async fn live_managed_oauth_quota_query() {
        let root = dirs::data_local_dir().unwrap().join("vellum");
        let manager = crate::codex_oauth::CodexOAuthManager::new(root);
        let auth = manager
            .valid_default_auth()
            .await
            .unwrap()
            .expect("Vellum has no managed OpenAI OAuth account");
        let windows = query(
            &auth.access_token,
            &auth.account_id,
            "openai-official",
            true,
        )
        .await
        .unwrap();
        assert!(!windows.is_empty());
        assert!(windows
            .iter()
            .all(|window| (0.0..=100.0).contains(&window.used_percent)));
    }
}
