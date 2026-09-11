//! Grok 週額度查詢（doc/08）。
//!
//! 沒有 REST 端點——額度只能透過 ACP（JSON-RPC over stdio）從
//! `grok agent stdio` 問出來。方法名 `_x.ai/billing` 的底線是必要的。
//!
//! 從 cc-switch 的 `proxy/providers/grok_billing.rs` 移植：
//!   - 拿掉對 `database::Database` 的依賴 → 改成落 JSON 檔（路徑當參數傳）。
//!   - 拿掉 `SubscriptionQuota` 轉換 → 直接產 vellum 的 `QuotaSnapshot`。
//!   - ACP transport（ChildGuard、reader thread、recv_timeout）照搬。

use crate::error::{AppError, AppResult};
use crate::model::{QuotaPeriod, QuotaPeriodUnit, QuotaSnapshot};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const DEFAULT_CACHE_MINUTES: u32 = 15;
const MIN_CACHE_MINUTES: u32 = 5;
const MAX_CACHE_MINUTES: u32 = 120;
const INIT_TIMEOUT: Duration = Duration::from_secs(10);
const BILLING_TIMEOUT: Duration = Duration::from_secs(20);

/// ACP 方法名。底線前綴必要（doc/08 實測）。
const BILLING_METHOD: &str = "_x.ai/billing";

pub fn clamp_cache_minutes(m: u32) -> u32 {
    m.clamp(MIN_CACHE_MINUTES, MAX_CACHE_MINUTES)
}

/// 預設 15 分鐘；可在 5–120 之間調整。
pub fn default_cache_ttl() -> Duration {
    Duration::from_secs(u64::from(DEFAULT_CACHE_MINUTES) * 60)
}

/// 落盤快照（與記憶體快取同一份，含查詢時間用來算 TTL）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BillingSnapshot {
    pub route_id: String,
    pub subscription_tier: Option<String>,
    pub credit_usage_percent: f64,
    pub period_type: Option<String>,
    pub period_start: Option<String>,
    pub period_end: Option<String>,
    pub detail: Value,
    pub queried_at: String,
    pub stale: bool,
}

impl BillingSnapshot {
    /// 轉成 UI 用的固定形狀（model.rs）。百分比夾在 0–100。
    pub fn to_quota_snapshot(&self) -> QuotaSnapshot {
        QuotaSnapshot {
            route_id: self.route_id.clone(),
            used_percent: self.credit_usage_percent.clamp(0.0, 100.0),
            period: period(self.period_type.as_deref()),
            reset_at: self.period_end.clone(),
            tier: self.subscription_tier.clone(),
            stale: self.stale,
        }
    }
}

fn period(period_type: Option<&str>) -> QuotaPeriod {
    match period_type {
        Some(t) if t.contains("MONTH") => QuotaPeriod::named(QuotaPeriodUnit::Month),
        Some(t) if t.contains("WEEK") => QuotaPeriod::named(QuotaPeriodUnit::Week),
        _ => QuotaPeriod::unspecified(),
    }
}

/// 解析 `_x.ai/billing` 回傳值（純函式，可單測）。
pub fn parse_billing_response(value: &Value) -> AppResult<BillingParsed> {
    let config = value
        .get("config")
        .cloned()
        .unwrap_or_else(|| value.clone());
    let percent = config
        .get("creditUsagePercent")
        .and_then(Value::as_f64)
        .or_else(|| config.get("credit_usage_percent").and_then(Value::as_f64))
        .unwrap_or(0.0);
    let period = config
        .get("currentPeriod")
        .or_else(|| config.get("current_period"));
    let period_type = period
        .and_then(|p| p.get("type"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let period_start = period
        .and_then(|p| p.get("start"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let period_end = period
        .and_then(|p| p.get("end"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            config
                .get("billingPeriodEnd")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let subscription_tier = value
        .get("subscription_tier")
        .or_else(|| value.get("subscriptionTier"))
        .and_then(Value::as_str)
        .map(str::to_string);

    Ok(BillingParsed {
        credit_usage_percent: percent,
        period_type,
        period_start,
        period_end,
        subscription_tier,
        detail: value.clone(),
    })
}

#[derive(Debug, Clone)]
pub struct BillingParsed {
    pub credit_usage_percent: f64,
    pub period_type: Option<String>,
    pub period_start: Option<String>,
    pub period_end: Option<String>,
    pub subscription_tier: Option<String>,
    pub detail: Value,
}

/// 快取有效性：週期沒過 **且** 在 TTL 內，兩個條件都要（doc/08）。
pub fn is_usable(snap: &BillingSnapshot, ttl: Duration) -> bool {
    period_still_open(snap) && within_ttl(snap, ttl)
}

pub fn period_still_open(snap: &BillingSnapshot) -> bool {
    match snap.period_end.as_deref().and_then(parse_rfc3339) {
        Some(end) => end > Utc::now(),
        None => true,
    }
}

pub fn within_ttl(snap: &BillingSnapshot, ttl: Duration) -> bool {
    let Some(queried) = parse_rfc3339(&snap.queried_at) else {
        return false;
    };
    match Utc::now().signed_duration_since(queried).to_std() {
        Ok(age) => age <= ttl,
        Err(_) => true, // 時鐘偏移/未來時間：當作新鮮。
    }
}

fn parse_rfc3339(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

// ---------------------------------------------------------------------------
// 額度服務：記憶體快取 + 落盤 + TTL/週期雙重檢查。fetch 用 trait 注入，才測得動。
// ---------------------------------------------------------------------------

/// 落盤根目錄下的檔名前綴。完整路徑：`<root>/quota-<route_id>.json`。
fn snapshot_path(root: &Path, route_id: &str) -> PathBuf {
    // route_id 是內部產生（seed 的固定 id），但還是消毒一下檔名。
    let safe: String = route_id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    root.join(format!("quota-{safe}.json"))
}

/// 額度來源。測試可注入假來源；正式流程用 [`GrokAgentSource`]。
pub trait BillingSource: Send + Sync {
    fn fetch(&self, route_id: &str) -> AppResult<BillingParsed>;
}

/// 走真實 ACP：spawn `grok agent stdio`。
pub struct GrokAgentSource;

impl BillingSource for GrokAgentSource {
    fn fetch(&self, _route_id: &str) -> AppResult<BillingParsed> {
        query_billing_via_grok_agent()
    }
}

pub struct QuotaService {
    /// 落盤根目錄（測試傳 temp dir）。
    root: PathBuf,
    cache_ttl: Duration,
    cache: Mutex<HashMap<String, BillingSnapshot>>,
    source: Box<dyn BillingSource>,
}

impl QuotaService {
    pub fn new(root: PathBuf, ttl: Duration, source: Box<dyn BillingSource>) -> Self {
        Self {
            root,
            cache_ttl: ttl,
            cache: Mutex::new(HashMap::new()),
            source,
        }
    }

    /// 便利建構：用 GrokAgentSource + 預設 TTL。
    pub fn with_grok(root: PathBuf) -> Self {
        Self::new(root, default_cache_ttl(), Box::new(GrokAgentSource))
    }

    /// 取額度。找不到 grok 執行檔或查詢失敗時回 `Ok(None)`（UI 那格隱藏），
    /// 只有快取存在時才回帶 stale 旗標的舊值（doc/08）。
    pub fn get(&self, route_id: &str, force_refresh: bool) -> AppResult<Option<QuotaSnapshot>> {
        match self.get_snapshot(route_id, force_refresh) {
            Ok(snap) => Ok(Some(snap.to_quota_snapshot())),
            Err(e) if e.to_string().contains("grok binary not found") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 取原始快照。force_refresh 一律重查。
    pub fn get_snapshot(&self, route_id: &str, force_refresh: bool) -> AppResult<BillingSnapshot> {
        if !force_refresh {
            if let Some(snap) = self
                .cached(route_id)
                .filter(|s| is_usable(s, self.cache_ttl))
            {
                return Ok(snap);
            }
            // 落盤只在「同時」週期未過且 TTL 內才算新鮮——
            // 否則整個計費週期都不會刷新（cc-switch 踩過的 bug）。
            if let Some(snap) = self
                .load_persisted(route_id)
                .filter(|s| is_usable(s, self.cache_ttl))
            {
                self.store_memory(snap.clone());
                return Ok(snap);
            }
        }

        match self.source.fetch(route_id) {
            Ok(parsed) => {
                let snap = BillingSnapshot {
                    route_id: route_id.to_string(),
                    subscription_tier: parsed.subscription_tier,
                    credit_usage_percent: parsed.credit_usage_percent,
                    period_type: parsed.period_type,
                    period_start: parsed.period_start,
                    period_end: parsed.period_end,
                    detail: parsed.detail,
                    queried_at: Utc::now().to_rfc3339(),
                    stale: false,
                };
                let _ = self.persist(&snap);
                self.store_memory(snap.clone());
                Ok(snap)
            }
            Err(error) => {
                log::warn!("[quota] fetch failed for {route_id}: {error}");
                if let Some(mut snap) = self
                    .cached(route_id)
                    .or_else(|| self.load_persisted(route_id))
                {
                    snap.stale = true;
                    return Ok(snap);
                }
                Err(error)
            }
        }
    }

    /// Query and cache Grok billing for one isolated account profile.
    ///
    /// The cache key includes the account id so switching accounts can never
    /// reuse another account's quota snapshot.
    pub fn get_for_account(
        &self,
        route_id: &str,
        account_id: &str,
        grok_home: &Path,
        force_refresh: bool,
    ) -> AppResult<QuotaSnapshot> {
        let cache_key = grok_account_cache_key(route_id, account_id);
        if !force_refresh {
            if let Some(snapshot) = self
                .cached(&cache_key)
                .or_else(|| self.load_persisted(&cache_key))
                .filter(|snapshot| is_usable(snapshot, self.cache_ttl))
            {
                return Ok(snapshot.to_quota_snapshot());
            }
        }
        match query_billing_via_grok_agent_at(grok_home) {
            Ok(parsed) => {
                let snapshot = BillingSnapshot {
                    route_id: cache_key,
                    subscription_tier: parsed.subscription_tier,
                    credit_usage_percent: parsed.credit_usage_percent,
                    period_type: parsed.period_type,
                    period_start: parsed.period_start,
                    period_end: parsed.period_end,
                    detail: parsed.detail,
                    queried_at: Utc::now().to_rfc3339(),
                    stale: false,
                };
                let _ = self.persist(&snapshot);
                self.store_memory(snapshot.clone());
                let mut quota = snapshot.to_quota_snapshot();
                quota.route_id = route_id.to_string();
                Ok(quota)
            }
            Err(error) => {
                if let Some(mut snapshot) = self
                    .cached(&cache_key)
                    .or_else(|| self.load_persisted(&cache_key))
                {
                    snapshot.stale = true;
                    let mut quota = snapshot.to_quota_snapshot();
                    quota.route_id = route_id.to_string();
                    return Ok(quota);
                }
                Err(error)
            }
        }
    }

    fn cached(&self, route_id: &str) -> Option<BillingSnapshot> {
        self.cache.lock().ok()?.get(route_id).cloned()
    }

    fn store_memory(&self, snap: BillingSnapshot) {
        if let Ok(mut guard) = self.cache.lock() {
            guard.insert(snap.route_id.clone(), snap);
        }
    }

    fn persist(&self, snap: &BillingSnapshot) -> AppResult<()> {
        let path = snapshot_path(&self.root, &snap.route_id);
        let json = serde_json::to_string_pretty(snap)
            .map_err(|e| AppError::Message(format!("serialize quota snapshot: {e}")))?;
        std::fs::create_dir_all(&self.root)
            .map_err(|e| AppError::Message(format!("create quota dir: {e}")))?;
        std::fs::write(&path, json)
            .map_err(|e| AppError::Message(format!("write quota snapshot: {e}")))?;
        Ok(())
    }

    fn load_persisted(&self, route_id: &str) -> Option<BillingSnapshot> {
        let path = snapshot_path(&self.root, route_id);
        let raw = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str::<BillingSnapshot>(&raw) {
            Ok(snap) => Some(snap),
            Err(error) => {
                log::warn!("[quota] discarding corrupt snapshot for {route_id}: {error}");
                None
            }
        }
    }
}

fn grok_account_cache_key(route_id: &str, account_id: &str) -> String {
    format!("{route_id}::grok-account::{account_id}")
}

// ---------------------------------------------------------------------------
// ACP transport（照搬 cc-switch，拿掉 cc-switch 專屬依賴）
// ---------------------------------------------------------------------------

/// drop 時不會 kill 的 `std::process::Child`，用 RAII 包起來。
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn initialize_frame(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientInfo": { "name": "vellum", "version": env!("CARGO_PKG_VERSION") },
            "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false } }
        }
    })
}

fn billing_frame(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": BILLING_METHOD,
        // 必要：省略 params 會回 -32602。
        "params": {}
    })
}

/// 最小 ACP client：initialize → _x.ai/billing。
pub fn query_billing_via_grok_agent() -> AppResult<BillingParsed> {
    query_billing_via_grok_agent_at(&crate::grok_auth::grok_home())
}

pub fn query_billing_via_grok_agent_at(grok_home: &Path) -> AppResult<BillingParsed> {
    let grok = crate::grok_auth::grok_executable(grok_home);

    let mut command = crate::process::background_command(&grok);
    command
        .args(["agent", "stdio"])
        .env("GROK_HOME", grok_home)
        .env("GROK_DISABLE_AUTOUPDATER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let child = command.spawn().map_err(|e| {
        AppError::Message(format!(
            "failed to spawn grok agent ({}): {e}",
            grok.display()
        ))
    })?;
    let mut guard = ChildGuard(child);

    let mut stdin = guard
        .0
        .stdin
        .take()
        .ok_or_else(|| AppError::Message("grok agent stdin missing".into()))?;
    let stdout = guard
        .0
        .stdout
        .take()
        .ok_or_else(|| AppError::Message("grok agent stdout missing".into()))?;

    // 讀取放獨立 thread 推 channel——主路徑用 recv_timeout 才有真 timeout。
    // 阻塞式 read_line + 外層 timeout 在 agent 沉默時會永遠卡住。
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if tx.send(line).is_err() {
                return;
            }
        }
    });

    // initialize 要先送並等回——agent 會亂序回應。
    write_rpc(&mut stdin, &initialize_frame(1))?;
    let _ = read_rpc_until_id(&rx, 1, INIT_TIMEOUT);

    write_rpc(&mut stdin, &billing_frame(2))?;
    let response = read_rpc_until_id(&rx, 2, BILLING_TIMEOUT)?;

    // 量測到的外層是 {"id":2,"result":{"config":…,"subscription_tier":…}}。
    let payload = response
        .pointer("/result/result")
        .or_else(|| response.get("result"))
        .cloned()
        .unwrap_or(response);

    parse_billing_response(&payload)
}

fn write_rpc(stdin: &mut impl Write, value: &Value) -> AppResult<()> {
    let line = serde_json::to_string(value)
        .map_err(|e| AppError::Message(format!("serialize acp request: {e}")))?;
    writeln!(stdin, "{line}").map_err(|e| AppError::Message(format!("write acp request: {e}")))?;
    stdin
        .flush()
        .map_err(|e| AppError::Message(format!("flush acp request: {e}")))?;
    Ok(())
}

/// 讀到指定 id 的回應為止。跳過無 id 的 notification 與其他 id。
pub fn read_rpc_until_id(rx: &Receiver<String>, id: u64, timeout: Duration) -> AppResult<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(AppError::Message(format!(
                "timeout waiting for acp response id={id}"
            )));
        };
        let line = match rx.recv_timeout(remaining) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => {
                return Err(AppError::Message(format!(
                    "timeout waiting for acp response id={id}"
                )))
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(AppError::Message("grok agent closed stdout".into()))
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if value.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(err) = value.get("error") {
            return Err(AppError::Message(format!("acp error: {err}")));
        }
        return Ok(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn grok_quota_cache_is_scoped_to_account_and_route() {
        assert_ne!(
            grok_account_cache_key("grok", "account-a"),
            grok_account_cache_key("grok", "account-b")
        );
        assert_ne!(
            grok_account_cache_key("grok-a", "account-a"),
            grok_account_cache_key("grok-b", "account-a")
        );
    }

    fn sample_weekly() -> Value {
        json!({
            "config": {
                "creditUsagePercent": 38.0,
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-07-21T13:23:39.929110+00:00",
                    "end":   "2026-07-28T13:23:39.929110+00:00"
                },
                "onDemandCap": {"val": 0},
                "prepaidBalance": {"val": 0}
            },
            "subscription_tier": "SuperGrok"
        })
    }

    /// 週期永不過期、 queried `age` 前的快照。
    fn snapshot_aged(route_id: &str, age: chrono::Duration) -> BillingSnapshot {
        BillingSnapshot {
            route_id: route_id.to_string(),
            subscription_tier: Some("SuperGrok".into()),
            credit_usage_percent: 38.0,
            period_type: Some("USAGE_PERIOD_TYPE_WEEKLY".into()),
            period_start: None,
            period_end: Some((Utc::now() + chrono::Duration::days(3)).to_rfc3339()),
            detail: json!({}),
            queried_at: (Utc::now() - age).to_rfc3339(),
            stale: false,
        }
    }

    // ---- wire format（實測過）----

    #[test]
    fn billing_frame_uses_the_verified_method_name() {
        let frame = billing_frame(2);
        assert_eq!(frame["method"], json!("_x.ai/billing"));
        assert_ne!(frame["method"], json!("ext_method"));
        assert!(
            frame.get("params").is_some_and(Value::is_object),
            "params must be an object"
        );
        assert_eq!(frame["jsonrpc"], json!("2.0"));
        assert_eq!(frame["id"], json!(2));
    }

    #[test]
    fn initialize_frame_shape() {
        let frame = initialize_frame(1);
        assert_eq!(frame["method"], json!("initialize"));
        assert_eq!(frame["params"]["protocolVersion"], json!(1));
        assert!(frame["params"]["clientInfo"]["name"].is_string());
    }

    #[test]
    fn parses_live_response_envelope() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "config": {
                    "creditUsagePercent": 43.0,
                    "currentPeriod": {
                        "type": "USAGE_PERIOD_TYPE_WEEKLY",
                        "start": "2026-07-21T13:23:39.929110+00:00",
                        "end":   "2026-07-28T13:23:39.929110+00:00"
                    }
                },
                "subscription_tier": "SuperGrok"
            }
        });
        let payload = response
            .pointer("/result/result")
            .or_else(|| response.get("result"))
            .cloned()
            .unwrap();
        let parsed = parse_billing_response(&payload).unwrap();
        assert!((parsed.credit_usage_percent - 43.0).abs() < f64::EPSILON);
        assert_eq!(parsed.subscription_tier.as_deref(), Some("SuperGrok"));
        assert!(parsed
            .period_end
            .as_deref()
            .unwrap()
            .starts_with("2026-07-28"));
    }

    // ---- payload parsing ----

    #[test]
    fn parses_billing_response_shape() {
        let parsed = parse_billing_response(&sample_weekly()).unwrap();
        assert!((parsed.credit_usage_percent - 38.0).abs() < f64::EPSILON);
        assert_eq!(parsed.subscription_tier.as_deref(), Some("SuperGrok"));
        assert!(parsed
            .period_end
            .as_deref()
            .unwrap()
            .starts_with("2026-07-28"));
        assert!(parsed.period_type.as_deref().unwrap().contains("WEEKLY"));
    }

    #[test]
    fn parses_monthly_period() {
        let mut v = sample_weekly();
        v["config"]["currentPeriod"]["type"] = json!("USAGE_PERIOD_TYPE_MONTHLY");
        let parsed = parse_billing_response(&v).unwrap();
        let snap = BillingSnapshot {
            route_id: "p".into(),
            subscription_tier: parsed.subscription_tier,
            credit_usage_percent: parsed.credit_usage_percent,
            period_type: parsed.period_type,
            period_start: parsed.period_start,
            period_end: parsed.period_end,
            detail: parsed.detail,
            queried_at: Utc::now().to_rfc3339(),
            stale: false,
        };
        assert_eq!(snap.to_quota_snapshot().period.unit, QuotaPeriodUnit::Month);
    }

    #[test]
    fn handles_missing_optional_fields() {
        let v = json!({"config": {"creditUsagePercent": 10.0}, "subscription_tier": "Free"});
        let parsed = parse_billing_response(&v).unwrap();
        assert!((parsed.credit_usage_percent - 10.0).abs() < f64::EPSILON);
        assert!(parsed.period_end.is_none());
    }

    #[test]
    fn percent_above_100_is_clamped_for_display() {
        let mut snap = snapshot_aged("p", chrono::Duration::zero());
        snap.credit_usage_percent = 125.0;
        let q = snap.to_quota_snapshot();
        assert_eq!(q.used_percent, 100.0);
    }

    // ---- TTL / 週期雙重檢查 ----

    #[test]
    fn fresh_snapshot_within_ttl_and_open_period_is_usable() {
        let snap = snapshot_aged("p", chrono::Duration::minutes(1));
        assert!(is_usable(&snap, Duration::from_secs(60 * 15)));
    }

    #[test]
    fn snapshot_outside_ttl_is_not_usable_even_if_period_open() {
        // cc-switch 踩過的 bug：只檢週期 → 整週不刷新。
        let snap = snapshot_aged("p", chrono::Duration::minutes(30));
        assert!(!is_usable(&snap, Duration::from_secs(60 * 15)));
    }

    #[test]
    fn snapshot_in_expired_period_is_not_usable_even_within_ttl() {
        let mut snap = snapshot_aged("p", chrono::Duration::minutes(1));
        snap.period_end = Some((Utc::now() - chrono::Duration::days(1)).to_rfc3339());
        assert!(!is_usable(&snap, Duration::from_secs(60 * 15)));
    }

    #[test]
    fn snapshot_with_no_period_end_is_treated_as_open() {
        let mut snap = snapshot_aged("p", chrono::Duration::minutes(1));
        snap.period_end = None;
        assert!(is_usable(&snap, Duration::from_secs(60 * 15)));
    }

    // ---- transport timeout（regression：舊 reader 卡在 read_line）----

    #[test]
    fn read_rpc_times_out_when_agent_is_silent() {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let started = Instant::now();
        let result = read_rpc_until_id(&rx, 2, Duration::from_millis(300));
        drop(tx);
        assert!(result.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout did not fire promptly: {:?}",
            started.elapsed()
        );
        assert!(result.unwrap_err().to_string().contains("timeout"));
    }

    #[test]
    fn read_rpc_reports_closed_stdout() {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        drop(tx);
        let err = read_rpc_until_id(&rx, 2, Duration::from_secs(5)).unwrap_err();
        assert!(err.to_string().contains("closed stdout"));
    }

    #[test]
    fn read_rpc_skips_notifications_and_other_ids() {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        tx.send(r#"{"jsonrpc":"2.0","method":"_x.ai/announcements/update","params":{}}"#.into())
            .unwrap();
        tx.send(r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#.into())
            .unwrap();
        tx.send(r#"not json at all"#.into()).unwrap();
        tx.send(r#"{"jsonrpc":"2.0","id":2,"result":{"config":{}}}"#.into())
            .unwrap();
        let value = read_rpc_until_id(&rx, 2, Duration::from_secs(2)).unwrap();
        assert_eq!(value["id"], json!(2));
    }

    #[test]
    fn read_rpc_surfaces_method_not_found() {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        tx.send(
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"Method not found"}}"#
                .into(),
        )
        .unwrap();
        let err = read_rpc_until_id(&rx, 2, Duration::from_secs(2)).unwrap_err();
        assert!(err.to_string().contains("-32601"));
    }

    // ---- 服務層（用注入的假來源，碰 temp dir）----

    /// 假來源：存「成功值」或「失敗訊息」。AppError 不是 Clone（凍結形狀），
    /// 所以拆開存，fetch 時再組。
    struct FakeSource {
        ok: Mutex<Option<BillingParsed>>,
        err: Mutex<Option<String>>,
    }
    impl BillingSource for FakeSource {
        fn fetch(&self, _route_id: &str) -> AppResult<BillingParsed> {
            if let Some(msg) = self.err.lock().unwrap().clone() {
                return Err(AppError::Message(msg));
            }
            self.ok
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| AppError::Message("no mock value".into()))
        }
    }
    /// `BillingSource` 的共享指標，讓測試與服務持有同一份可變來源。
    struct SharedFake(Arc<FakeSource>);
    impl BillingSource for SharedFake {
        fn fetch(&self, route_id: &str) -> AppResult<BillingParsed> {
            self.0.fetch(route_id)
        }
    }

    impl FakeSource {
        fn parsed() -> BillingParsed {
            let mut parsed = parse_billing_response(&sample_weekly()).unwrap();
            parsed.period_start = Some((Utc::now() - chrono::Duration::days(4)).to_rfc3339());
            parsed.period_end = Some((Utc::now() + chrono::Duration::days(3)).to_rfc3339());
            parsed
        }
        fn success() -> Self {
            Self {
                ok: Mutex::new(Some(Self::parsed())),
                err: Mutex::new(None),
            }
        }
        fn failure(msg: &str) -> Self {
            Self {
                ok: Mutex::new(None),
                err: Mutex::new(Some(msg.to_string())),
            }
        }
        fn set_failure(&self, msg: &str) {
            *self.ok.lock().unwrap() = None;
            *self.err.lock().unwrap() = Some(msg.to_string());
        }
        /// 包成服務能持有的 Box（測試仍握著 Arc 可改它）。
        fn boxed_shared(self) -> (Arc<Self>, Box<dyn BillingSource>) {
            let arc = Arc::new(self);
            let boxed: Box<dyn BillingSource> = Box::new(SharedFake(Arc::clone(&arc)));
            (arc, boxed)
        }
    }

    #[test]
    fn service_caches_within_ttl() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = QuotaService::new(
            dir.path().to_path_buf(),
            Duration::from_secs(60 * 15),
            Box::new(FakeSource::success()),
        );
        let _ = svc.get_snapshot("grok-cli", false).unwrap();
        // 再取一次應命中記憶體（不會再落盤）。
        let snap = svc.get_snapshot("grok-cli", false).unwrap();
        assert!((snap.credit_usage_percent - 38.0).abs() < f64::EPSILON);
    }

    #[test]
    fn service_falls_back_to_stale_on_failure() {
        let dir = tempfile::TempDir::new().unwrap();
        let (fake, source) = FakeSource::success().boxed_shared();
        let svc = QuotaService::new(
            dir.path().to_path_buf(),
            Duration::from_secs(60 * 15),
            source,
        );
        let _ = svc.get_snapshot("grok-cli", true).unwrap();
        fake.set_failure("boom");
        let snap = svc.get_snapshot("grok-cli", true).unwrap();
        assert!(snap.stale);
        assert!((snap.credit_usage_percent - 38.0).abs() < f64::EPSILON);
    }

    #[test]
    fn service_persists_and_reloads_across_restart() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();

        let svc = QuotaService::new(
            root.clone(),
            Duration::from_secs(60 * 15),
            Box::new(FakeSource::success()),
        );
        let snap = svc.get_snapshot("grok-cli", true).unwrap();
        assert!(!snap.stale);

        // 檔案落盤了。
        let path = snapshot_path(&root, "grok-cli");
        assert!(path.exists(), "snapshot file should exist");

        // 重啟：應能從檔案載入（仍在 TTL/週期內）。
        let svc2 = QuotaService::new(
            root,
            Duration::from_secs(60 * 15),
            Box::new(SharedFake(Arc::new(FakeSource::failure("would re-fetch")))),
        );
        let loaded = svc2.get_snapshot("grok-cli", false).unwrap();
        assert!((loaded.credit_usage_percent - 38.0).abs() < f64::EPSILON);
        assert!(!loaded.stale, "within TTL must stay fresh from disk");
    }

    #[test]
    fn service_get_returns_none_when_binary_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = QuotaService::new(
            dir.path().to_path_buf(),
            Duration::from_secs(60 * 15),
            Box::new(FakeSource::failure("grok binary not found")),
        );
        assert!(svc.get("grok-cli", true).unwrap().is_none());
    }

    #[test]
    fn corrupt_persisted_file_is_discarded() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = snapshot_path(dir.path(), "grok-cli");
        std::fs::write(&path, "{not json").unwrap();

        let svc = QuotaService::new(
            dir.path().to_path_buf(),
            Duration::from_secs(60 * 15),
            Box::new(FakeSource::success()),
        );
        let snap = svc.get_snapshot("grok-cli", false).unwrap();
        assert!((snap.credit_usage_percent - 38.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cache_minutes_clamped() {
        assert_eq!(clamp_cache_minutes(1), MIN_CACHE_MINUTES);
        assert_eq!(clamp_cache_minutes(999), MAX_CACHE_MINUTES);
        assert_eq!(clamp_cache_minutes(30), 30);
    }

    /// 對真實 agent 的活體測試。預設不跑（spawn 行程、需登入、約 3s）：
    /// `cargo test --lib quota -- --ignored --nocapture`
    #[test]
    #[ignore = "requires a local grok binary and an authenticated session"]
    fn live_query_against_real_agent() {
        let parsed = query_billing_via_grok_agent().expect("live billing query failed");
        println!(
            "LIVE tier={:?} percent={} period={:?} end={:?}",
            parsed.subscription_tier,
            parsed.credit_usage_percent,
            parsed.period_type,
            parsed.period_end
        );
        assert!(parsed.credit_usage_percent >= 0.0);
        assert!(parsed.period_end.is_some(), "expected a reset timestamp");
    }
}
