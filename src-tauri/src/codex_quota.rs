use crate::model::{LunaReserveQuota, QuotaPeriod, QuotaPeriodUnit, QuotaSnapshot};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const QUOTA_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);
pub const ROUTING_QUOTA_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone)]
struct CachedQuota {
    #[cfg(test)]
    offline_fixture: bool,
    fetched_at: std::time::Instant,
    windows: Vec<QuotaSnapshot>,
}

impl CachedQuota {
    fn is_fresh(&self, ttl: std::time::Duration) -> bool {
        // Synthetic manager fixtures have no upstream to refresh from. Keep
        // them deterministic on slow CI; real cache entries still use the TTL.
        #[cfg(test)]
        if self.offline_fixture {
            return true;
        }
        cache_is_fresh(self.fetched_at, ttl)
    }
}

fn quota_cache() -> &'static tokio::sync::RwLock<HashMap<String, CachedQuota>> {
    static CACHE: OnceLock<tokio::sync::RwLock<HashMap<String, CachedQuota>>> = OnceLock::new();
    CACHE.get_or_init(|| tokio::sync::RwLock::new(HashMap::new()))
}

// Keep the connection pool across routing cache misses. Rebuilding a client
// for every member used to pay DNS/TCP/TLS setup on each two-second refresh.
fn quota_client() -> Result<&'static reqwest::Client, CodexQuotaError> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| CodexQuotaError::Request(error.clone()))
}

// Coalesce concurrent misses for the same token/workspace. Weak entries keep
// the registry bounded by active queries without retaining token material.
type QuotaQueryLock = tokio::sync::Mutex<Option<(std::time::Instant, CodexQuotaError)>>;

fn quota_query_lock(key: &str) -> Arc<QuotaQueryLock> {
    type QueryLocks = HashMap<String, Weak<QuotaQueryLock>>;
    static LOCKS: OnceLock<Mutex<QueryLocks>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(None));
    locks.insert(key.to_string(), Arc::downgrade(&lock));
    lock
}

fn cache_is_fresh(fetched_at: std::time::Instant, ttl: std::time::Duration) -> bool {
    fetched_at.elapsed() < ttl
}

#[derive(Debug, Clone, thiserror::Error)]
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
    query_with_cache_ttl(
        access_token,
        account_id,
        route_id,
        force_refresh,
        QUOTA_CACHE_TTL,
    )
    .await
}

/// Query quota with a caller-selected cache age. UI/profile reads tolerate a
/// short display cache, but request routing must react much sooner when a
/// five-hour window reaches its edge.
pub async fn query_with_cache_ttl(
    access_token: &str,
    account_id: &str,
    route_id: &str,
    force_refresh: bool,
    cache_ttl: std::time::Duration,
) -> Result<Vec<QuotaSnapshot>, CodexQuotaError> {
    query_with_client(
        quota_client()?,
        USAGE_URL,
        access_token,
        account_id,
        route_id,
        force_refresh,
        cache_ttl,
    )
    .await
}

async fn query_with_client(
    client: &reqwest::Client,
    usage_url: &str,
    access_token: &str,
    account_id: &str,
    route_id: &str,
    force_refresh: bool,
    cache_ttl: std::time::Duration,
) -> Result<Vec<QuotaSnapshot>, CodexQuotaError> {
    // The display route id is not necessarily a credential selector. Key by a
    // one-way digest of the access token so two users in the same workspace do
    // not share quota, while no token material is retained in the cache key.
    let cache_key = format!("{account_id}:{:x}", Sha256::digest(access_token.as_bytes()));
    let requested_at = std::time::Instant::now();
    let query_lock = quota_query_lock(&cache_key);
    let mut guard = query_lock.lock().await;
    if !force_refresh {
        if let Some((failed_at, error)) = guard.as_ref() {
            if *failed_at >= requested_at {
                // Share a failed in-flight read too, rather than making each
                // queued caller pay another upstream timeout. Later requests
                // can retry; this is not a persistent error cache.
                return Err(error.clone());
            }
        }
    }
    // Recheck after acquiring the per-account lock: another caller may have
    // refreshed while we waited. Forced reads still always reach upstream.
    if !force_refresh {
        if let Some(cached) = quota_cache().read().await.get(&cache_key).cloned() {
            if cached.is_fresh(cache_ttl) {
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
    let windows = match fetch_quota_windows(
        client,
        usage_url,
        access_token,
        account_id,
        route_id,
        force_refresh,
    )
    .await
    {
        Ok(windows) => windows,
        Err(error) => {
            *guard = Some((std::time::Instant::now(), error.clone()));
            return Err(error);
        }
    };
    *guard = None;
    let mut cache = quota_cache().write().await;
    cache.retain(|_, entry| entry.is_fresh(QUOTA_CACHE_TTL));
    cache.insert(
        cache_key,
        CachedQuota {
            #[cfg(test)]
            offline_fixture: false,
            fetched_at: std::time::Instant::now(),
            windows: windows.clone(),
        },
    );
    Ok(windows)
}

async fn fetch_quota_windows(
    client: &reqwest::Client,
    usage_url: &str,
    access_token: &str,
    account_id: &str,
    route_id: &str,
    force_refresh: bool,
) -> Result<Vec<QuotaSnapshot>, CodexQuotaError> {
    let mut request = client
        .get(usage_url)
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
    let mut windows = parse_quota_windows(route_id, &value)?;
    // Only associate fallback with the explicitly identified requested account.
    if value.get("account_id").and_then(Value::as_str) != Some(account_id) {
        for window in &mut windows {
            window.luna_reserve = None;
        }
    }
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
            luna_reserve: None,
        });
    }
    if windows.is_empty() {
        return Err(CodexQuotaError::Parse(
            "rate_limit contains no usable windows".into(),
        ));
    }
    // Keep Reserve out of the ordinary windows used by pool routing/cadence.
    if let Some(reserve) = parse_luna_reserve(value) {
        if let Some(five_hour) = windows.iter_mut().find(|window| {
            window.period.unit == QuotaPeriodUnit::Hour && window.period.amount == Some(5)
        }) {
            five_hour.luna_reserve = Some(reserve);
        }
    }
    Ok(windows)
}

fn parse_luna_reserve(value: &Value) -> Option<LunaReserveQuota> {
    let reserve = value
        .get("additional_rate_limits")?
        .as_array()?
        .iter()
        .find(|limit| limit.get("limit_name").and_then(Value::as_str) == Some("gpt-reserve"))?
        .get("rate_limit")?;
    // Reserve can have multiple windows. Show the one currently limiting it.
    let window = ["primary_window", "secondary_window"]
        .into_iter()
        .filter_map(|key| {
            let window = reserve.get(key)?;
            let used = window.get("used_percent")?.as_f64()?;
            used.is_finite().then_some((used.clamp(0.0, 100.0), window))
        })
        .max_by(|left, right| left.0.total_cmp(&right.0))?;
    let active = value
        .pointer("/rate_limit/allowed")
        .and_then(Value::as_bool)
        == Some(false)
        && value
            .pointer("/rate_limit_upsell/banner_type")
            .and_then(Value::as_str)
            == Some("luna_reserve")
        && reserve.get("allowed").and_then(Value::as_bool) == Some(true);
    Some(LunaReserveQuota {
        active,
        used_percent: window.0,
        reset_at: window
            .1
            .get("reset_at")
            .and_then(Value::as_i64)
            .and_then(|timestamp| chrono::DateTime::from_timestamp(timestamp, 0))
            .map(|reset| reset.to_rfc3339()),
    })
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
            offline_fixture: true,
            fetched_at: std::time::Instant::now(),
            windows,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Exercise the production cache/HTTP path with fake tokens and a local
    // keep-alive server; no managed accounts or external usage are accessed.
    async fn local_usage_server() -> (
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/usage", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&connections);
        let requests = Arc::new(AtomicUsize::new(0));
        let request_counter = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut socket, _) = accepted.unwrap();
                        counter.fetch_add(1, Ordering::SeqCst);
                        let request_counter = Arc::clone(&request_counter);
                        clients.spawn(async move {
                            let mut pending = Vec::new();
                            loop {
                                let mut chunk = [0_u8; 4096];
                                let count = socket.read(&mut chunk).await.unwrap();
                                if count == 0 { break; }
                                pending.extend_from_slice(&chunk[..count]);
                                if !pending.windows(4).any(|part| part == b"\r\n\r\n") {
                                    continue;
                                }
                                request_counter.fetch_add(1, Ordering::SeqCst);
                                let unauthorized = pending.windows(8).any(|part| part == b"rejected");
                                pending.clear();
                                // Ensure concurrent callers overlap before the cache fills.
                                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                                let (status, body) = if unauthorized {
                                    ("401 Unauthorized", "{}")
                                } else {
                                    ("200 OK", r#"{"rate_limit":{"primary_window":{"used_percent":10,"limit_window_seconds":18000}}}"#)
                                };
                                let response = format!(
                                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}", body.len()
                                );
                                if socket.write_all(response.as_bytes()).await.is_err() { break; }
                            }
                        });
                    }
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        });
        (url, connections, requests, server)
    }

    #[tokio::test]
    async fn routing_cache_misses_reuse_the_http_connection() {
        use std::sync::atomic::Ordering;
        let (url, connections, requests, server) = local_usage_server().await;
        let client = quota_client().unwrap();
        // Zero TTL guarantees two real reads rather than hiding a new
        // connection behind the cache. Different accounts share transport only.
        for account in ["reuse-first", "reuse-second"] {
            let result = query_with_client(
                client,
                &url,
                "fake-reuse-token",
                account,
                "route",
                false,
                std::time::Duration::ZERO,
            )
            .await
            .unwrap();
            assert_eq!(result[0].used_percent, 10.0);
        }
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn concurrent_routing_misses_share_one_read_and_keep_route_ids() {
        use std::sync::atomic::Ordering;
        let (url, connections, requests, server) = local_usage_server().await;
        let client = quota_client().unwrap();
        let reads = (0..8).map(|index| {
            let url = &url;
            async move {
                let route = format!("concurrent-{index}");
                let windows = query_with_client(
                    client,
                    url,
                    "fake-concurrent-token",
                    "concurrent-account",
                    &route,
                    false,
                    ROUTING_QUOTA_CACHE_TTL,
                )
                .await
                .unwrap();
                assert_eq!(windows[0].route_id, route);
            }
        });
        futures_util::future::join_all(reads).await;
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn concurrent_failed_reads_share_the_failure_without_serial_retries() {
        use std::sync::atomic::Ordering;
        let (url, _, requests, server) = local_usage_server().await;
        let client = quota_client().unwrap();
        let reads = (0..8).map(|_| {
            query_with_client(
                client,
                &url,
                "rejected",
                "concurrent-failure",
                "route",
                false,
                ROUTING_QUOTA_CACHE_TTL,
            )
        });
        for result in futures_util::future::join_all(reads).await {
            assert!(matches!(result, Err(CodexQuotaError::Unauthorized)));
        }
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert!(matches!(
            query_with_client(
                client,
                &url,
                "rejected",
                "concurrent-failure",
                "route",
                false,
                ROUTING_QUOTA_CACHE_TTL
            )
            .await,
            Err(CodexQuotaError::Unauthorized)
        ));
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn failed_forced_refresh_cannot_restore_a_cached_reading() {
        let (url, _, _, server) = local_usage_server().await;
        let client = quota_client().unwrap();
        seed_test_quota("rejected", "refresh-test", vec![]).await;
        assert!(matches!(
            query_with_client(
                client,
                &url,
                "rejected",
                "refresh-test",
                "route",
                true,
                QUOTA_CACHE_TTL
            )
            .await,
            Err(CodexQuotaError::Unauthorized)
        ));
        let key = format!("refresh-test:{:x}", Sha256::digest(b"rejected"));
        assert!(!quota_cache().read().await.contains_key(&key));
        server.abort();
    }

    fn reserve_usage() -> Value {
        json!({
            "rate_limit": { "allowed": false,
                "primary_window": { "used_percent": 100, "limit_window_seconds": 18000 },
                "secondary_window": { "used_percent": 80, "limit_window_seconds": 604800 }
            },
            "rate_limit_upsell": { "banner_type": "luna_reserve" },
            "additional_rate_limits": [{ "limit_name": "gpt-reserve", "rate_limit": {
                "allowed": true,
                "primary_window": { "used_percent": 20, "reset_at": 1788000000 },
                "secondary_window": { "used_percent": 40, "reset_at": 1788500000 }
            }}]
        })
    }

    #[test]
    fn reserve_is_display_metadata_without_replacing_pool_routing_windows() {
        let windows = parse_quota_windows("account", &reserve_usage()).unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].used_percent, 100.0);
        assert_eq!(windows[1].used_percent, 80.0);
        assert!(windows[1].luna_reserve.is_none());
        let reserve = windows[0].luna_reserve.as_ref().unwrap();
        assert!(reserve.active);
        assert_eq!(reserve.used_percent, 40.0);
        assert_eq!(
            reserve.reset_at.as_deref(),
            chrono::DateTime::from_timestamp(1788500000, 0)
                .map(|reset| reset.to_rfc3339())
                .as_deref()
        );
    }

    #[test]
    fn reserve_requires_explicit_native_fallback_signals() {
        for (pointer, replacement) in [
            ("/rate_limit/allowed", json!(true)),
            ("/rate_limit/allowed", Value::Null),
            ("/rate_limit_upsell/banner_type", json!("usage_limit")),
            ("/additional_rate_limits/0/rate_limit/allowed", json!(false)),
            ("/additional_rate_limits/0/rate_limit/allowed", Value::Null),
        ] {
            let mut value = reserve_usage();
            *value.pointer_mut(pointer).unwrap() = replacement;
            let windows = parse_quota_windows("account", &value).unwrap();
            assert!(!windows[0].luna_reserve.as_ref().unwrap().active);
        }
        let mut value = reserve_usage();
        value["additional_rate_limits"] = json!([]);
        assert!(parse_quota_windows("account", &value).unwrap()[0]
            .luna_reserve
            .is_none());
        value = reserve_usage();
        value["additional_rate_limits"][0]["rate_limit"] = json!({ "allowed": true });
        assert!(parse_quota_windows("account", &value).unwrap()[0]
            .luna_reserve
            .is_none());
    }

    #[test]
    fn request_routing_expires_quota_reads_before_the_display_cache() {
        let fetched_at = std::time::Instant::now() - std::time::Duration::from_secs(3);
        assert!(!cache_is_fresh(fetched_at, ROUTING_QUOTA_CACHE_TTL));
        assert!(cache_is_fresh(fetched_at, QUOTA_CACHE_TTL));
    }

    #[tokio::test]
    async fn offline_quota_fixtures_survive_routing_and_display_cache_expiry() {
        use std::sync::atomic::Ordering;
        let (url, _, requests, server) = local_usage_server().await;
        let token = "offline-expired-fixture";
        let account = "offline-expired-workspace";
        let windows = parse_quota_windows(
            account,
            &json!({ "rate_limit": {
                "primary_window": { "used_percent": 10, "limit_window_seconds": 18000 }
            }}),
        )
        .unwrap();
        seed_test_quota(token, account, windows).await;
        let key = format!("{account}:{:x}", Sha256::digest(token.as_bytes()));
        // Reproduce a slow CI worker without sleeping or contacting upstream.
        quota_cache()
            .write()
            .await
            .get_mut(&key)
            .unwrap()
            .fetched_at = std::time::Instant::now() - std::time::Duration::from_secs(60);
        // A real refresh prunes stale cache entries as well. Offline fixtures
        // must survive that sweep when other tests refresh in parallel.
        query_with_client(
            quota_client().unwrap(),
            &url,
            "local-refresh-token",
            "local-refresh-workspace",
            "local-refresh-route",
            false,
            ROUTING_QUOTA_CACHE_TTL,
        )
        .await
        .unwrap();
        let result = query_with_client(
            quota_client().unwrap(),
            &url,
            token,
            account,
            "offline-route",
            false,
            ROUTING_QUOTA_CACHE_TTL,
        )
        .await
        .unwrap();
        assert_eq!(result[0].used_percent, 10.0);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert_eq!(result[0].route_id, "offline-route");
        server.abort();
    }

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
