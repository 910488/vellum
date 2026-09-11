//! Standalone web search engine for non-OpenAI providers.
//!
//! Codex's native `web.run` namespace tool is backed, on OpenAI, by the
//! hosted `/api/codex/alpha/search`. Vellum exposes the same wire contract at
//! `POST /v1/alpha/search` so that third-party models (Qwen, GLM, Grok,
//! NVIDIA, ...) can drive Codex's native WebSearch task item, source list and
//! continuation flow.
//!
//! The engine is pluggable (`SearchBackend`); Brave Search is the only
//! configured backend (an API key is required — there is no zero-config
//! fallback). The wire types in this module intentionally mirror
//! `codex_api::search`; keeping the real object-of-arrays shape is what lets
//! Codex dispatch `web.run` without a Vellum-specific shim.
//!
//! Safety: only HTTP/HTTPS is fetched; localhost, private, link-local, cloud
//! metadata and DNS-rebinding hosts are blocked; redirects are re-validated;
//! provider keys, cookies and workspace content are never forwarded to an
//! independent search backend; HTML/PDF/image responses are size, time and
//! redirect bounded; external content is treated as untrusted context.

/// Guard against silently re-introducing the whole-process kill. web.run
/// parses attacker-controlled HTML/PDF and `SearchEngine::run` relies on
/// `catch_unwind` to turn a parser bug into a 502. `panic = "abort"` turns it
/// back into a process crash (0xC0000409, seen 2026-08-05 and 2026-08-10).
#[cfg(panic = "abort")]
compile_error!(
    "web.run parses attacker-controlled HTML/PDF; SearchEngine::run relies on \
     catch_unwind. `panic = \"abort\"` turns any parser bug back into a whole-process \
     kill (see 0xC0000409 crashes, 2026-08-05/08-10)."
);

use crate::error::{AppError, AppResult};
use base64::Engine as _;
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const BRAVE_SEARCH_CREDENTIAL_ID: &str = "vellum-web-search-brave";

/// User-facing search mode. Mirrors Codex's `web_search` setting and gates
/// how aggressively the engine may reach out.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    /// No web tool is offered at all.
    #[default]
    Disabled,
    /// Only results already in the local cache.
    Cached,
    /// Search-index results are allowed, but no arbitrary page fetch.
    Indexed,
    /// Search and fetch public pages.
    Live,
}

impl SearchMode {
    pub fn allows_search(self) -> bool {
        matches!(self, Self::Indexed | Self::Live)
    }
    pub fn allows_fetch(self) -> bool {
        matches!(self, Self::Live)
    }
}

/// Domain policy applied to queries and page fetches.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DomainPolicy {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub block: Vec<String>,
}

/// Persisted third-party web search configuration. Default for a fresh
/// install: third-party search OFF, no key. Brave Search is the only
/// backend; there is no other backend to select.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchSettings {
    /// Master switch for third-party (non-OpenAI) web search.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub mode: SearchMode,
    /// Loaded from Vellum's encrypted credential store at runtime. Never
    /// serialize a Brave secret into the ordinary settings JSON.
    #[serde(default, skip_serializing, skip_deserializing)]
    pub brave_api_key: Option<String>,
    #[serde(default)]
    pub domain_policy: DomainPolicy,
    /// Default requested result context size.
    #[serde(default = "default_context_size")]
    pub search_context_size: String,
}

fn default_context_size() -> String {
    "medium".to_string()
}

impl Default for WebSearchSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: SearchMode::Live,
            brave_api_key: None,
            domain_policy: DomainPolicy::default(),
            search_context_size: default_context_size(),
        }
    }
}

impl WebSearchSettings {
    /// Brave is usable exactly when an API key is configured. There is no
    /// other backend to silently fall back to.
    pub fn brave_usable(&self) -> bool {
        self.brave_api_key.is_some()
    }

    /// The tool is advertised only when the switch is on and Brave can run.
    pub fn advertised(&self) -> bool {
        self.enabled && self.brave_usable()
    }

    pub fn with_stored_brave_key(mut self, data_root: &std::path::Path) -> Self {
        self.brave_api_key = crate::credentials::load(data_root, BRAVE_SEARCH_CREDENTIAL_ID)
            .ok()
            .flatten();
        self
    }
}

/// One result row surfaced to Codex and the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    #[serde(rename = "type")]
    pub kind: String,
    pub ref_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Image URL returned by an image backend or a bounded data URL generated
    /// for a PDF page screenshot. Kept separate from the source page URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pageno: Option<u64>,
}

/// The wire response returned to Codex at `/v1/alpha/search`.
///
/// `output` becomes the `function_call_output` body the model sees; `results`
/// feed Codex's task UI, source list and subsequent `open/click/find` calls.
/// `encrypted_output` is intentionally `null` ??Vellum never fabricates an
/// OpenAI ciphertext; returning null is the documented "plaintext" form.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub encrypted_output: Option<String>,
    pub output: String,
    pub results: Vec<SearchResult>,
}

/// Codex's standalone search wire format is one object whose command fields
/// each contain an array of operations.  Earlier Vellum code modelled this as
/// `Vec<{search_query: ...}>`; that shape only passed local mocks and rejected
/// real Codex requests before a backend was reached.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchCommands {
    #[serde(default)]
    pub search_query: Option<Vec<Value>>,
    #[serde(default)]
    pub image_query: Option<Vec<Value>>,
    #[serde(default)]
    pub open: Option<Vec<Value>>,
    #[serde(default)]
    pub click: Option<Vec<Value>>,
    #[serde(default)]
    pub find: Option<Vec<Value>>,
    #[serde(default)]
    pub screenshot: Option<Vec<Value>>,
    #[serde(default)]
    pub finance: Option<Vec<Value>>,
    #[serde(default)]
    pub weather: Option<Vec<Value>>,
    #[serde(default)]
    pub sports: Option<Vec<Value>>,
    #[serde(default)]
    pub time: Option<Vec<Value>>,
    #[serde(default)]
    pub response_length: Option<Value>,
}

/// The inbound `/v1/alpha/search` request, compatible with Codex's
/// standalone search client.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchRequest {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning: Option<Value>,
    #[serde(default)]
    pub input: Option<Value>,
    #[serde(default)]
    pub commands: Option<SearchCommands>,
    #[serde(default)]
    pub settings: Option<Value>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
}

/// Pull a string field out of a leniently-typed command value.
fn field_str(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn field_array_str(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Extract a query string from either `{ "query": "..." }` or a bare string.
fn query_of(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    field_str(value, "query")
        .or_else(|| field_str(value, "q"))
        .filter(|text| !text.trim().is_empty())
}

fn structured_query(value: &Value, domain: &str) -> String {
    let mut parts = Vec::new();
    match domain {
        "finance" => {
            for key in ["ticker", "type", "market"] {
                if let Some(value) = field_str(value, key).filter(|value| !value.is_empty()) {
                    parts.push(value);
                }
            }
        }
        "weather" => {
            for key in ["location", "start"] {
                if let Some(value) = field_str(value, key).filter(|value| !value.is_empty()) {
                    parts.push(value);
                }
            }
            if let Some(days) = value.get("duration").and_then(Value::as_u64) {
                parts.push(format!("{days} day forecast"));
            }
        }
        "sports" => {
            for key in ["fn", "league", "team", "opponent", "date_from", "date_to"] {
                if let Some(value) = field_str(value, key).filter(|value| !value.is_empty()) {
                    parts.push(value);
                }
            }
        }
        _ => {}
    }
    if parts.is_empty() {
        query_of(value).unwrap_or_default()
    } else {
        parts.join(" ")
    }
}

fn parse_utc_offset(value: &str) -> Option<i32> {
    let bytes = value.as_bytes();
    if bytes.len() != 6 || bytes[3] != b':' || !matches!(bytes[0], b'+' | b'-') {
        return None;
    }
    let hours = value[1..3].parse::<i32>().ok()?;
    let minutes = value[4..6].parse::<i32>().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    let seconds = hours
        .checked_mul(3600)?
        .checked_add(minutes.checked_mul(60)?)?;
    Some(if bytes[0] == b'-' { -seconds } else { seconds })
}

/// A fetched page cached against its ref id, with extracted text.
#[derive(Debug, Clone)]
struct CachedPage {
    url: String,
    title: Option<String>,
    text: String,
    fetched_at: Instant,
}

/// Maps `ref_id` -> discovered url/title/snippet and any fetched page text.
///
/// This is a process-lifetime cache. It intentionally contains only public
/// URLs/snippets and is not written to disk. The proxy owns one shared engine,
/// so ref ids remain valid across the multiple `/alpha/search` calls that make
/// up one Codex search task.
pub struct RefStore {
    entries: HashMap<String, SearchResult>,
    pages: HashMap<String, CachedPage>,
    counter: u64,
    /// Monotonic "turn" used to build `turnNsearchM` style ids.
    turn: u64,
    last_used: Instant,
}

impl Default for RefStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RefStore {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            pages: HashMap::new(),
            counter: 0,
            turn: 0,
            last_used: Instant::now(),
        }
    }

    fn next_ref(&mut self) -> String {
        self.last_used = Instant::now();
        let id = format!("turn{}search{}", self.turn, self.counter);
        self.counter += 1;
        id
    }

    /// Record a fresh batch of results, returning the stored copies with ref
    /// ids assigned. Advances the turn counter so a new search never collides
    /// with a previous one.
    fn record(&mut self, results: Vec<RawResult>) -> Vec<SearchResult> {
        if !results.is_empty() {
            self.turn += 1;
            self.counter = 0;
        }
        results
            .into_iter()
            .map(|raw| {
                let ref_id = self.next_ref();
                let stored = SearchResult {
                    kind: raw.kind,
                    ref_id: ref_id.clone(),
                    url: raw.url,
                    title: raw.title.clone(),
                    snippet: raw.snippet.clone(),
                    image_url: raw.image_url,
                    width: None,
                    height: None,
                    pageno: None,
                };
                self.entries.insert(ref_id.clone(), stored.clone());
                stored
            })
            .collect()
    }

    fn lookup(&self, ref_id: &str) -> Option<&SearchResult> {
        self.entries.get(ref_id)
    }

    fn record_pdf_screenshot(
        &mut self,
        source_url: String,
        pageno: u64,
        image_url: String,
        width: u16,
        height: u16,
        snippet: Option<String>,
    ) -> SearchResult {
        let ref_id = self.next_ref();
        let result = SearchResult {
            kind: "image_result".into(),
            ref_id: ref_id.clone(),
            url: Some(source_url),
            title: Some(format!("PDF page {}", pageno + 1)),
            snippet,
            image_url: Some(image_url),
            width: Some(width),
            height: Some(height),
            pageno: Some(pageno),
        };
        self.entries.insert(ref_id, result.clone());
        result
    }

    fn cache_page(&mut self, ref_id: &str, page: CachedPage) {
        self.pages.insert(ref_id.to_string(), page);
    }

    fn page(&self, ref_id: &str) -> Option<&CachedPage> {
        self.pages.get(ref_id)
    }

    /// Drop entries older than the retention window. `Instant` is monotonic so
    /// this is process-lifetime scoped, which is correct: a restart clears the
    /// ref space, and continuation turns within one session stay consistent.
    fn prune(&mut self, max_age: Duration) {
        let stale_pages: Vec<String> = self
            .pages
            .iter()
            .filter(|(_, page)| page.fetched_at.elapsed() > max_age)
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale_pages {
            self.pages.remove(&key);
        }
        // Result entries are tiny; keep them for the whole session unless the
        // store grows without bound, then evict oldest-turn entries.
        if self.entries.len() > 4096 {
            let keep_turn = self.turn.saturating_sub(8);
            self.entries.retain(|_, result| {
                result
                    .ref_id
                    .strip_prefix("turn")
                    .and_then(|rest| rest.split("search").next())
                    .and_then(|n| n.parse::<u64>().ok())
                    .map(|n| n >= keep_turn)
                    .unwrap_or(true)
            });
        }
    }
}

fn session_key(request: &SearchRequest) -> String {
    request
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or("anonymous")
        .to_string()
}

fn prune_ref_sessions(sessions: &mut HashMap<String, RefStore>) {
    sessions.retain(|_, store| store.last_used.elapsed() <= REF_RETENTION);
    while sessions.len() > 256 {
        let Some(oldest) = sessions
            .iter()
            .max_by_key(|(_, store)| store.last_used.elapsed())
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        sessions.remove(&oldest);
    }
}

/// A backend-agnostic result before ref ids are assigned.
#[derive(Debug, Clone)]
pub struct RawResult {
    pub kind: String,
    pub url: Option<String>,
    pub title: Option<String>,
    pub snippet: Option<String>,
    pub image_url: Option<String>,
}

/// A pluggable independent search backend.
///
/// Backends must never receive provider credentials, cookies or workspace
/// content. They answer queries with public, ranked results.
#[async_trait::async_trait]
pub trait SearchBackend: Send + Sync {
    async fn text_search(&self, query: &str) -> AppResult<Vec<RawResult>>;
    async fn image_search(&self, query: &str) -> AppResult<Vec<RawResult>>;
    async fn text_search_with_recency(
        &self,
        query: &str,
        _recency_days: Option<u64>,
    ) -> AppResult<Vec<RawResult>> {
        self.text_search(query).await
    }
    async fn image_search_with_recency(
        &self,
        query: &str,
        _recency_days: Option<u64>,
    ) -> AppResult<Vec<RawResult>> {
        self.image_search(query).await
    }
}

/// Build the Brave backend. Fails when no API key is configured — there is
/// no other backend to fall back to.
pub fn build_backend(settings: &WebSearchSettings) -> AppResult<Box<dyn SearchBackend>> {
    let key = settings
        .brave_api_key
        .as_deref()
        .ok_or_else(|| AppError::Message("Brave Search requires an API key".into()))?;
    Ok(Box::new(BraveBackend::new(key.to_string())?))
}

/// Stand-in backend used when Brave has no configured key. Every query fails
/// closed with the same honest message a save-time validation error would
/// have given; the point is that *building* a `SearchEngine` must never fail
/// just because search is enabled but unconfigured — startup/readiness of
/// the whole proxy must not depend on whether the user has gotten around to
/// pasting in a Brave key yet.
struct MissingBraveKeyBackend;

#[async_trait::async_trait]
impl SearchBackend for MissingBraveKeyBackend {
    async fn text_search(&self, _query: &str) -> AppResult<Vec<RawResult>> {
        Err(AppError::Message("Brave Search requires an API key".into()))
    }

    async fn image_search(&self, _query: &str) -> AppResult<Vec<RawResult>> {
        Err(AppError::Message("Brave Search requires an API key".into()))
    }
}

/// Brave Search API backend.
pub struct BraveBackend {
    api_key: String,
    client: reqwest::Client,
}

fn parse_brave_web_results(response: &Value) -> Vec<RawResult> {
    response
        .get("web")
        .and_then(|web| web.get("results"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| RawResult {
                    kind: "text_result".into(),
                    url: item.get("url").and_then(Value::as_str).map(str::to_string),
                    title: item
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    snippet: item
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    image_url: None,
                })
                .collect()
        })
        .unwrap_or_default()
}

impl BraveBackend {
    pub fn new(api_key: String) -> AppResult<Self> {
        Ok(Self {
            api_key,
            client: safe_http_client()?,
        })
    }
}

#[async_trait::async_trait]
impl SearchBackend for BraveBackend {
    async fn text_search(&self, query: &str) -> AppResult<Vec<RawResult>> {
        let response = self
            .client
            .get("https://api.search.brave.com/res/v1/web/search")
            .header("X-Subscription-Token", &self.api_key)
            .header("Accept", "application/json")
            .query(&[("q", query), ("count", "10")])
            .send()
            .await
            .map_err(|error| AppError::Message(format!("Brave search failed: {error}")))?
            .error_for_status()
            .map_err(|error| AppError::Message(format!("Brave search failed: {error}")))?
            .json::<Value>()
            .await
            .map_err(|error| AppError::Message(format!("Brave search parse failed: {error}")))?;
        Ok(parse_brave_web_results(&response))
    }

    async fn image_search(&self, query: &str) -> AppResult<Vec<RawResult>> {
        let response = self
            .client
            .get("https://api.search.brave.com/res/v1/images/search")
            .header("X-Subscription-Token", &self.api_key)
            .header("Accept", "application/json")
            .query(&[("q", query), ("count", "10")])
            .send()
            .await
            .map_err(|error| AppError::Message(format!("Brave image search failed: {error}")))?
            .error_for_status()
            .map_err(|error| AppError::Message(format!("Brave image search failed: {error}")))?
            .json::<Value>()
            .await
            .map_err(|error| AppError::Message(format!("Brave image parse failed: {error}")))?;
        Ok(response
            .get("results")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let props = item.get("properties")?;
                        Some(RawResult {
                            kind: "image_result".into(),
                            url: item
                                .get("url")
                                .or_else(|| item.get("source"))
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            title: props
                                .get("title")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            image_url: props
                                .get("url")
                                .or_else(|| item.get("thumbnail").and_then(|v| v.get("src")))
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            snippet: props
                                .get("description")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn text_search_with_recency(
        &self,
        query: &str,
        recency_days: Option<u64>,
    ) -> AppResult<Vec<RawResult>> {
        let Some(freshness) = recency_bucket(recency_days) else {
            return self.text_search(query).await;
        };
        let response = self
            .client
            .get("https://api.search.brave.com/res/v1/web/search")
            .header("X-Subscription-Token", &self.api_key)
            .header("Accept", "application/json")
            .query(&[("q", query), ("count", "10"), ("freshness", freshness)])
            .send()
            .await
            .map_err(|error| AppError::Message(format!("Brave search failed: {error}")))?
            .error_for_status()
            .map_err(|error| AppError::Message(format!("Brave search failed: {error}")))?
            .json::<Value>()
            .await
            .map_err(|error| AppError::Message(format!("Brave search parse failed: {error}")))?;
        Ok(parse_brave_web_results(&response))
    }
}

fn recency_bucket(days: Option<u64>) -> Option<&'static str> {
    match days? {
        0..=1 => Some("pd"),
        2..=7 => Some("pw"),
        8..=31 => Some("pm"),
        32..=365 => Some("py"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// SSRF safety, HTTP client, HTML extraction
// ---------------------------------------------------------------------------

/// Build a redirect-bounded client that NEVER carries provider credentials.
/// Redirects are re-validated so an attacker cannot bounce an "open" public
/// URL to an internal host.
pub fn safe_http_client() -> AppResult<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        // Redirects must be handled explicitly so every Location target is
        // validated before the next socket is opened.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| AppError::Message(format!("HTTP client build failed: {error}")))
}

async fn pinned_safe_client(url: &url::Url) -> AppResult<reqwest::Client> {
    let host = url
        .host_str()
        .ok_or_else(|| AppError::Message("URL has no host".into()))?;
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none());
    if host.parse::<std::net::IpAddr>().is_err() {
        let port = url.port_or_known_default().unwrap_or(443);
        let mut resolved = tokio::net::lookup_host((host, port))
            .await
            .map_err(|error| AppError::Message(format!("DNS lookup failed for {host}: {error}")))?;
        let address = resolved
            .find(|address| !ip_is_blocked(&address.ip()))
            .ok_or_else(|| {
                AppError::Message(format!(
                    "host '{host}' resolves only to blocked private/link-local addresses"
                ))
            })?;
        // Pin the validated address so reqwest cannot perform a second DNS
        // lookup that resolves the same name into a private address.
        builder = builder.resolve(host, address);
    }
    builder
        .build()
        .map_err(|error| AppError::Message(format!("safe HTTP client build failed: {error}")))
}

const MAX_REDIRECTS: usize = 5;
const MAX_FETCH_BYTES: usize = 5 * 1024 * 1024;
const MAX_FETCH_SECS: u64 = 30;

/// True only for http/https. Blocks localhost, private, link-local, cloud
/// metadata and loopback hosts before any connection is attempted.
pub fn is_safe_url(raw: &str) -> Result<String, String> {
    let url = url::Url::parse(raw).map_err(|error| format!("invalid URL: {error}"))?;
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(format!("scheme '{other}' is not allowed; only http/https")),
    }
    let host = url.host().ok_or_else(|| "URL has no host".to_string())?;
    if host_is_blocked(&host) {
        return Err(format!(
            "host '{host}' is blocked (localhost / private / link-local / metadata)"
        ));
    }
    // Reject userinfo that could smuggle credentials to the backend.
    if url.username() != "" || url.password().is_some() {
        return Err("URL must not carry userinfo".into());
    }
    Ok(url.to_string())
}

/// Hosts that must never be fetched. Uses the parsed `url::Host` so compressed
/// IPv6 literals such as `::1` are handled correctly.
fn host_is_blocked(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Ipv4(ip) => ip_is_blocked(&std::net::IpAddr::V4(*ip)),
        url::Host::Ipv6(ip) => ip_is_blocked(&std::net::IpAddr::V6(*ip)),
        url::Host::Domain(domain) => domain_is_blocked(domain),
    }
}

fn domain_is_blocked(domain: &str) -> bool {
    let host = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return true;
    }
    const BLOCKED_NAMES: &[&str] = &[
        "localhost",
        "ip6-localhost",
        "ip6-loopback",
        "metadata.google.internal",
        "metadata",
        "metadata.azure.com",
    ];
    if BLOCKED_NAMES.contains(&host.as_str()) {
        return true;
    }
    host.ends_with(".local") || host.ends_with(".internal")
}

fn ip_is_blocked(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let octets = v4.octets();
            let [a, b, c, _d] = octets;
            if a == 127 || a == 0 || a == 10 {
                return true;
            }
            if a == 172 && (16..=31).contains(&b) {
                return true;
            }
            if a == 192 && b == 168 {
                return true;
            }
            if a == 169 && b == 254 {
                return true;
            }
            if a == 100 && (64..=127).contains(&b) {
                return true;
            }
            if a == 198 && (18..=19).contains(&b) {
                return true;
            }
            if a >= 240 {
                return true;
            }
            if a == 192 && b == 0 && c == 2 {
                return true;
            }
            if a == 198 && b == 51 && c == 100 {
                return true;
            }
            if a == 203 && b == 0 && c == 113 {
                return true;
            }
            false
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] == 0x2001 && v6.segments()[1] == 0xdb8)
        }
    }
}

/// A stripped, line-numbered rendering of an HTML document. External content
/// is untrusted context, so we only ever return plain text + a link index.
pub fn extract_text(html: &str) -> (String, Vec<ExtractedLink>) {
    extract_text_window(html, 1, usize::MAX)
}

fn extract_text_window(
    html: &str,
    one_based_start: u64,
    max_lines: usize,
) -> (String, Vec<ExtractedLink>) {
    let text = strip_html_to_text(html);
    let links = extract_anchors(html);
    let mut numbered = String::new();
    let start = one_based_start.saturating_sub(1) as usize;
    for (index, line) in text.lines().enumerate().skip(start).take(max_lines) {
        numbered.push_str(&format!("{}: {}\n", index + 1, line));
    }
    (numbered, links)
}

#[derive(Debug, Clone)]
pub struct ExtractedLink {
    pub url: String,
    pub text: String,
}

/// Strip HTML to readable text: drop scripts/styles, decode entities, collapse
/// whitespace. Treated as UNTRUSTED context by callers.
///
/// The walk advances one UTF-8 character at a time (`chars().next()`), so the
/// cursor is always on a char boundary by construction: the old
/// byte-at-a-time skip branch left the cursor inside a multi-byte character
/// and the next `html[i..]` slice panicked (0xC0000409 whenever a
/// `<script>`/`<style>` block contained CJK, emoji, or non-ASCII punctuation).
/// Tag matching never lowercases the whole remaining document, and the scan
/// for `>` is monotonic: every successful scan advances the cursor past that
/// `>`, and a failed scan is remembered so it can only happen once — the walk
/// is strictly O(n) even for a page made of thousands of lone `<`s, without
/// ever truncating a real tag (a window would cut long inline SVG / data-URI
/// attributes and leak them into the extracted text).
fn strip_html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_skip = false;
    let mut gt_exhausted = false;
    let mut index = 0;
    while let Some(ch) = html[index..].chars().next() {
        if ch == '<' {
            let rest = &html[index..];
            let head = rest.as_bytes();
            // `<script>` / `<style>` are ASCII, so compare the leading bytes
            // directly; this never slices mid-character and never scans past
            // the tag head.
            if (head.len() >= 7 && head[..7].eq_ignore_ascii_case(b"<script"))
                || (head.len() >= 6 && head[..6].eq_ignore_ascii_case(b"<style"))
            {
                in_skip = true;
            }
            // A real tag starts with a letter, '/', '!' or '?'. Anything else
            // (space, digit, punctuation, CJK...) is a lone '<' in text:
            // consume it without scanning ahead.
            let tag_like =
                head.len() > 1 && matches!(head[1], b'a'..=b'z' | b'A'..=b'Z' | b'/' | b'!' | b'?');
            if tag_like && !gt_exhausted {
                match rest.as_bytes().iter().position(|&byte| byte == b'>') {
                    Some(end) => {
                        let next = index + end + 1;
                        let tag = html[index..next].to_ascii_lowercase();
                        if in_skip && (tag.contains("</script") || tag.contains("</style")) {
                            in_skip = false;
                        }
                        // Block-level breaks become newlines.
                        let tag_name = tag
                            .trim_start_matches('<')
                            .trim_start_matches('/')
                            .split(|c: char| c.is_whitespace() || c == '>')
                            .next();
                        if matches!(
                            tag_name,
                            Some("p")
                                | Some("br")
                                | Some("div")
                                | Some("li")
                                | Some("h1")
                                | Some("h2")
                                | Some("h3")
                                | Some("h4")
                                | Some("h5")
                                | Some("h6")
                                | Some("tr")
                        ) {
                            out.push('\n');
                        }
                        index = next;
                    }
                    None => {
                        // No `>` anywhere in the rest of the document; `index`
                        // is monotonic, so no later scan can find one either.
                        // Remembering it keeps the walk O(n) without capping
                        // tag length.
                        gt_exhausted = true;
                        index += ch.len_utf8();
                    }
                }
            } else {
                index += ch.len_utf8();
            }
            continue;
        }
        if in_skip {
            index += ch.len_utf8();
            continue;
        }
        // Decode a few common entities; treat the rest literally.
        if html[index..].starts_with("&nbsp;") {
            out.push(' ');
            index += 6;
        } else if html[index..].starts_with("&amp;") {
            out.push('&');
            index += 5;
        } else if html[index..].starts_with("&lt;") {
            out.push('<');
            index += 4;
        } else if html[index..].starts_with("&gt;") {
            out.push('>');
            index += 4;
        } else if html[index..].starts_with("&quot;") {
            out.push('"');
            index += 6;
        } else if html[index..].starts_with("&#39;") {
            out.push('\'');
            index += 5;
        } else {
            out.push(ch);
            index += ch.len_utf8();
        }
    }
    // Collapse runs of whitespace into single spaces, keep line breaks.
    out.split('\n')
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Pull `<a href="...">text</a>` pairs out of HTML, tolerating quote styles.
fn extract_anchors(html: &str) -> Vec<ExtractedLink> {
    let mut out = Vec::new();
    // One lowercase pass: ASCII lowercasing never changes the byte length of
    // a UTF-8 string, so every scan below can run on `lower` and the matched
    // indices slice `html` directly. This avoids copying the remaining
    // document for every anchor, which was O(n²) on pages with thousands of
    // links (a 5 MiB page with ~5k links allocated several GB per open).
    let lower = html.to_ascii_lowercase();
    let mut search = 0;
    while let Some(pos) = lower[search..].find("<a ") {
        let abs = search + pos;
        let tag_end = match lower[abs..].find('>') {
            Some(end) => abs + end,
            None => break,
        };
        let tag = &html[abs..tag_end];
        let href = extract_attr(tag, "href");
        let close = match lower[tag_end..].find("</a>") {
            Some(end) => tag_end + end,
            None => {
                search = tag_end + 1;
                continue;
            }
        };
        let text = strip_html_to_text(&html[tag_end + 1..close]);
        if let Some(href) = href {
            if !href.is_empty() && !href.starts_with('#') && !href.starts_with("javascript:") {
                out.push(ExtractedLink {
                    url: href,
                    text: text.trim().to_string(),
                });
            }
        }
        search = close + 4;
    }
    out
}

fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let idx = lower.find(&needle)?;
    let rest = &tag[idx + needle.len()..];
    let value = if let Some(stripped) = rest.strip_prefix('"') {
        stripped.split('"').next().unwrap_or("")
    } else if let Some(stripped) = rest.strip_prefix('\'') {
        stripped.split('\'').next().unwrap_or("")
    } else {
        rest.split_whitespace().next().unwrap_or("")
    };
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Read at most `MAX_FETCH_BYTES` from a response stream with a hard deadline.
async fn read_bounded(response: reqwest::Response) -> AppResult<Vec<u8>> {
    use futures_util::StreamExt;
    let deadline = Instant::now() + Duration::from_secs(MAX_FETCH_SECS);
    let mut stream = response.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        if Instant::now() > deadline {
            return Err(AppError::Message("fetch exceeded time limit".into()));
        }
        let chunk =
            chunk.map_err(|error| AppError::Message(format!("stream read failed: {error}")))?;
        if buf.len() + chunk.len() > MAX_FETCH_BYTES {
            return Err(AppError::Message("fetch exceeded size limit".into()));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

const REF_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

struct SearchOutcome {
    text: String,
    query: Option<String>,
}

struct FetchedPage {
    url: String,
    title: Option<String>,
    text: String,
    pdf_bytes: Option<Vec<u8>>,
}

/// Orchestrates one `/v1/alpha/search` request. Holds the ref store, the
/// resolved backend and the effective mode. Thread-safe via an internal mutex
/// over the ref store; backends are themselves `Send + Sync`.
pub struct SearchEngine {
    settings: WebSearchSettings,
    backend: Box<dyn SearchBackend>,
    /// Ref ids are only meaningful inside one Codex session. Keeping separate
    /// stores prevents another local session from guessing `turn1search0` and
    /// opening a URL discovered by a different conversation.
    refs: Mutex<HashMap<String, RefStore>>,
}

impl SearchEngine {
    /// Build an engine for the persisted settings. Never fails just because
    /// Brave has no configured key — proxy readiness/startup must not depend
    /// on whether third-party search happens to be configured. An enabled
    /// engine with no key still builds; it fails closed the moment it is
    /// actually asked to search (see `MissingBraveKeyBackend`). Saving a
    /// settings change with search enabled but no key is rejected up front
    /// by `commands::web_search::validate_settings` instead — this
    /// constructor is the last-resort fallback for state that predates that
    /// check (or bypassed it), not the primary enforcement point.
    pub fn new(settings: WebSearchSettings) -> AppResult<Self> {
        let backend: Box<dyn SearchBackend> = match build_backend(&settings) {
            Ok(backend) => backend,
            Err(_) => Box::new(MissingBraveKeyBackend),
        };
        Ok(Self {
            settings,
            backend,
            refs: Mutex::new(HashMap::new()),
        })
    }

    /// Build an engine with an explicit backend (used by tests / connection
    /// probing).
    pub fn with_backend(
        settings: WebSearchSettings,
        backend: Box<dyn SearchBackend>,
    ) -> AppResult<Self> {
        Ok(Self {
            settings,
            backend,
            refs: Mutex::new(HashMap::new()),
        })
    }

    pub fn settings(&self) -> &WebSearchSettings {
        &self.settings
    }

    /// Run a connectivity probe against Brave.
    pub async fn probe(&self) -> String {
        match self.backend.text_search("openai").await {
            Ok(results) => format!("ok: {} results", results.len()),
            Err(error) => format!("error: {error}"),
        }
    }

    /// Execute a full `/v1/alpha/search` request and compose the response.
    ///
    /// The command handlers below process attacker-controlled HTML/PDF from
    /// third-party sites. A parser bug must degrade one request to a 502, not
    /// kill the whole proxy process, so the entire pipeline is fenced with
    /// `catch_unwind` (release builds no longer use `panic = "abort"`).
    pub async fn run(&self, request: &SearchRequest) -> AppResult<SearchResponse> {
        let result = FutureExt::catch_unwind(AssertUnwindSafe(self.run_inner(request))).await;
        match result {
            Ok(response) => response,
            Err(payload) => Err(AppError::Message(format!(
                "web search engine panicked: {}",
                crate::panic_hook::payload_text(&*payload)
            ))),
        }
    }

    async fn run_inner(&self, request: &SearchRequest) -> AppResult<SearchResponse> {
        if !self.settings.enabled {
            return Err(AppError::Message(
                "third-party web search is disabled".into(),
            ));
        }
        if self.settings.mode == SearchMode::Disabled {
            return Err(AppError::Message(
                "web search mode is Disabled; no tool should have been offered".into(),
            ));
        }
        let session = session_key(request);
        {
            let mut sessions = self
                .refs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            prune_ref_sessions(&mut sessions);
            sessions
                .entry(session.clone())
                .or_default()
                .prune(REF_RETENTION);
        }

        let commands = request.commands.clone().unwrap_or_default();
        let has_commands = commands
            .search_query
            .as_ref()
            .is_some_and(|items| !items.is_empty())
            || commands
                .image_query
                .as_ref()
                .is_some_and(|items| !items.is_empty())
            || commands
                .open
                .as_ref()
                .is_some_and(|items| !items.is_empty())
            || commands
                .click
                .as_ref()
                .is_some_and(|items| !items.is_empty())
            || commands
                .find
                .as_ref()
                .is_some_and(|items| !items.is_empty())
            || commands
                .screenshot
                .as_ref()
                .is_some_and(|items| !items.is_empty())
            || commands
                .finance
                .as_ref()
                .is_some_and(|items| !items.is_empty())
            || commands
                .weather
                .as_ref()
                .is_some_and(|items| !items.is_empty())
            || commands
                .sports
                .as_ref()
                .is_some_and(|items| !items.is_empty())
            || commands
                .time
                .as_ref()
                .is_some_and(|items| !items.is_empty())
            || commands.response_length.is_some();
        if !has_commands {
            return Ok(SearchResponse {
                encrypted_output: None,
                output: "No search commands were provided.".to_string(),
                results: Vec::new(),
            });
        }

        let mut output_parts: Vec<String> = Vec::new();
        let mut all_results: Vec<SearchResult> = Vec::new();
        let mut max_chars = response_length_default(&request.settings);
        let mut last_query: Option<String> = None;
        let request_policy =
            effective_domain_policy(&self.settings.domain_policy, &request.settings);
        let request_access = request_external_access(&request.settings);
        let needs_external_access = commands
            .search_query
            .as_ref()
            .is_some_and(|items| !items.is_empty())
            || commands
                .image_query
                .as_ref()
                .is_some_and(|items| !items.is_empty())
            || commands
                .open
                .as_ref()
                .is_some_and(|items| !items.is_empty())
            || commands
                .click
                .as_ref()
                .is_some_and(|items| !items.is_empty())
            || commands
                .find
                .as_ref()
                .is_some_and(|items| !items.is_empty())
            || commands
                .screenshot
                .as_ref()
                .is_some_and(|items| !items.is_empty())
            || commands
                .finance
                .as_ref()
                .is_some_and(|items| !items.is_empty())
            || commands
                .weather
                .as_ref()
                .is_some_and(|items| !items.is_empty())
            || commands
                .sports
                .as_ref()
                .is_some_and(|items| !items.is_empty());
        if request_access == Some(false) && needs_external_access {
            return Err(AppError::Message(
                "request settings prohibit external web access".into(),
            ));
        }

        for command in commands.search_query.as_deref().unwrap_or_default() {
            let outcome = self
                .do_search(
                    &session,
                    Some(command),
                    &mut all_results,
                    false,
                    &request_policy,
                )
                .await?;
            output_parts.push(outcome.text);
            last_query = outcome.query;
        }
        for command in commands.image_query.as_deref().unwrap_or_default() {
            let outcome = self
                .do_search(
                    &session,
                    Some(command),
                    &mut all_results,
                    true,
                    &request_policy,
                )
                .await?;
            output_parts.push(outcome.text);
            last_query = outcome.query;
        }
        for command in commands.open.as_deref().unwrap_or_default() {
            output_parts.push(self.do_open(&session, Some(command)).await?);
        }
        for command in commands.click.as_deref().unwrap_or_default() {
            output_parts.push(self.do_click(&session, Some(command)).await?);
        }
        for command in commands.find.as_deref().unwrap_or_default() {
            output_parts.push(self.do_find(&session, Some(command)).await?);
        }
        for command in commands.screenshot.as_deref().unwrap_or_default() {
            let (output, result) = self.do_screenshot(&session, Some(command)).await?;
            output_parts.push(output);
            if let Some(result) = result {
                all_results.push(result);
            }
        }
        for command in commands.finance.as_deref().unwrap_or_default() {
            let query = structured_query(command, "finance");
            output_parts.push(
                self.do_structured_query(
                    &session,
                    &query,
                    "finance",
                    &request_policy,
                    &mut all_results,
                )
                .await?,
            );
            last_query = Some(query);
        }
        for command in commands.weather.as_deref().unwrap_or_default() {
            let query = structured_query(command, "weather");
            output_parts.push(
                self.do_structured_query(
                    &session,
                    &query,
                    "weather",
                    &request_policy,
                    &mut all_results,
                )
                .await?,
            );
            last_query = Some(query);
        }
        for command in commands.sports.as_deref().unwrap_or_default() {
            let query = structured_query(command, "sports");
            output_parts.push(
                self.do_structured_query(
                    &session,
                    &query,
                    "sports",
                    &request_policy,
                    &mut all_results,
                )
                .await?,
            );
            last_query = Some(query);
        }
        for command in commands.time.as_deref().unwrap_or_default() {
            output_parts.push(self.do_time(Some(command), &last_query)?);
        }
        if let Some(len) = commands.response_length.as_ref() {
            max_chars = response_length_value(len).max(1024);
        }

        let mut output = output_parts.join("\n\n");
        output = truncate_chars(&output, max_chars);
        Ok(SearchResponse {
            encrypted_output: None,
            output,
            results: all_results,
        })
    }

    async fn do_search(
        &self,
        session: &str,
        command: Option<&Value>,
        results: &mut Vec<SearchResult>,
        image: bool,
        policy: &DomainPolicy,
    ) -> AppResult<SearchOutcome> {
        let Some(value) = command else {
            return Ok(SearchOutcome {
                text: "[web.run] search command had no query.".into(),
                query: None,
            });
        };
        let Some(query) = query_of(value) else {
            return Ok(SearchOutcome {
                text: "[web.run] search command had an empty query.".into(),
                query: None,
            });
        };
        if !self.settings.mode.allows_search() {
            return Err(AppError::Message(format!(
                "search mode does not allow querying an index: {:?}",
                self.settings.mode
            )));
        }
        let domains = field_array_str(value, "domains");
        let recency = value.get("recency").and_then(Value::as_u64);
        let raw = if image {
            self.backend
                .image_search_with_recency(&query, recency)
                .await?
        } else {
            self.backend
                .text_search_with_recency(&query, recency)
                .await?
        };
        let filtered = apply_domain_policy(raw, policy, &domains);
        let mut stored = {
            let mut sessions = self
                .refs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sessions
                .entry(session.to_string())
                .or_default()
                .record(filtered)
        };
        let text = render_search_results(&query, &stored);
        results.append(&mut stored);
        Ok(SearchOutcome {
            text,
            query: Some(query),
        })
    }

    async fn do_open(&self, session: &str, command: Option<&Value>) -> AppResult<String> {
        let value = command.cloned().unwrap_or(Value::Null);
        let url = match resolve_ref_or_url(&value, &self.refs, session) {
            RefTarget::Url(url) => url,
            RefTarget::ResolvedWithoutUrl(ref_id) => {
                return Ok(no_url_ref_message("open", &ref_id))
            }
            RefTarget::UnknownRef(ref_id) => return Ok(stale_ref_message("open", &ref_id)),
            RefTarget::Missing => return Ok("[web.run] open command needs a ref_id or url.".into()),
        };
        if !self.settings.mode.allows_fetch() {
            return Err(AppError::Message(format!(
                "search mode does not allow fetching pages: {:?}",
                self.settings.mode
            )));
        }
        let page = self.fetch_page(&url).await?;
        let lineno = value.get("lineno").and_then(Value::as_u64).unwrap_or(1);
        let (numbered, links) = extract_text_window(&page.text, lineno, 200);
        let mut out = format!(
            "[open] {}\n{}\n",
            page.url,
            page.title.as_deref().unwrap_or("")
        );
        out.push_str(&numbered);
        if !links.is_empty() {
            out.push_str("\nLinks:\n");
            for (i, link) in links.iter().take(40).enumerate() {
                out.push_str(&format!("{}. [{}] {}\n", i + 1, link.text, link.url));
            }
        }
        if let Some(ref_id) = field_str(&value, "ref_id") {
            let mut sessions = self
                .refs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sessions.entry(session.to_string()).or_default().cache_page(
                &ref_id,
                CachedPage {
                    url: page.url.clone(),
                    title: page.title.clone(),
                    text: page.text.clone(),
                    fetched_at: Instant::now(),
                },
            );
        }
        Ok(out)
    }

    async fn do_click(&self, session: &str, command: Option<&Value>) -> AppResult<String> {
        let value = command.cloned().unwrap_or(Value::Null);
        let ref_id = field_str(&value, "ref_id").unwrap_or_default();
        let selector = field_str(&value, "selector")
            .or_else(|| field_str(&value, "text"))
            .or_else(|| field_str(&value, "link"))
            .unwrap_or_default();
        let link_id = value.get("id").and_then(Value::as_u64);
        if !self.settings.mode.allows_fetch() {
            return Err(AppError::Message(format!(
                "search mode does not allow fetching pages: {:?}",
                self.settings.mode
            )));
        }
        let (resolved, source_url) = {
            let sessions = self
                .refs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let found = sessions.get(session).and_then(|refs| refs.lookup(&ref_id));
            (found.is_some(), found.and_then(|result| result.url.clone()))
        };
        let Some(source_url) = source_url else {
            return Ok(if ref_id.is_empty() {
                "[web.run] click needs a ref_id.".into()
            } else if resolved {
                no_url_ref_message("click", &ref_id)
            } else {
                stale_ref_message("click", &ref_id)
            });
        };
        let page = self.fetch_page(&source_url).await?;
        let links = extract_anchors(&page.text);
        let needle = selector.to_ascii_lowercase();
        let target = link_id
            .and_then(|id| id.checked_sub(1))
            .and_then(|index| links.get(index as usize).cloned())
            .or_else(|| {
                (!needle.is_empty()).then(|| {
                    links.into_iter().find(|link| {
                        let text = link.text.to_ascii_lowercase();
                        text.contains(&needle) || link.url.to_ascii_lowercase().contains(&needle)
                    })
                })?
            });
        match target {
            Some(link) => {
                let absolute = url::Url::parse(&source_url)
                    .ok()
                    .and_then(|base| base.join(&link.url).ok())
                    .map(|url| url.to_string())
                    .unwrap_or(link.url.clone());
                let safe = is_safe_url(&absolute).map_err(AppError::Message)?;
                let dest = self.fetch_page(&safe).await?;
                Ok(format!(
                    "[click] {} -> {}\n{}\n{}",
                    link.text,
                    dest.url,
                    dest.title.as_deref().unwrap_or(""),
                    truncate_chars(&strip_html_to_text(&dest.text), 4000)
                ))
            }
            None => Ok(format!(
                "[web.run] click: no link matching '{selector}' on {source_url}."
            )),
        }
    }

    async fn do_find(&self, session: &str, command: Option<&Value>) -> AppResult<String> {
        let value = command.cloned().unwrap_or(Value::Null);
        let ref_id = field_str(&value, "ref_id").unwrap_or_default();
        let query = field_str(&value, "pattern")
            .or_else(|| field_str(&value, "query"))
            .or_else(|| field_str(&value, "text"))
            .unwrap_or_default();
        if query.is_empty() {
            return Ok("[web.run] find needs a query.".into());
        }
        // `resolved` rides out of the same lock as the rest, so the failure
        // message below never needs a second acquisition.
        let (url, cached_text, title, resolved) = {
            let sessions = self
                .refs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let refs = sessions.get(session);
            if let Some(page) = refs.and_then(|refs| refs.page(&ref_id)) {
                (
                    page.url.clone(),
                    Some(page.text.clone()),
                    page.title.clone(),
                    true,
                )
            } else if let Some(result) = refs.and_then(|refs| refs.lookup(&ref_id)) {
                (
                    result.url.clone().unwrap_or_default(),
                    None,
                    result.title.clone(),
                    true,
                )
            } else {
                let literal = url::Url::parse(&ref_id)
                    .ok()
                    .filter(|url| matches!(url.scheme(), "http" | "https"))
                    .map(|url| url.to_string())
                    .unwrap_or_default();
                (literal, None, None, false)
            }
        };
        let text = match cached_text {
            Some(text) => text,
            None if !self.settings.mode.allows_fetch() => {
                return Err(AppError::Message(format!(
                    "search mode does not allow fetching pages: {:?}",
                    self.settings.mode
                )))
            }
            None => {
                if url.is_empty() {
                    return Ok(if ref_id.is_empty() {
                        "[web.run] find needs a ref_id.".into()
                    } else if resolved {
                        no_url_ref_message("find", &ref_id)
                    } else {
                        stale_ref_message("find", &ref_id)
                    });
                }
                self.fetch_page(&url).await?.text
            }
        };
        let needle = query.to_ascii_lowercase();
        let header = match &title {
            Some(title) if !title.is_empty() => format!("[find] '{query}' on {url} ({title}):"),
            _ => format!("[find] '{query}' on {url}:"),
        };
        let hits: Vec<String> = text
            .lines()
            .enumerate()
            .filter(|(_, line)| line.to_ascii_lowercase().contains(&needle))
            .take(20)
            .map(|(i, line)| format!("  L{}: {}", i + 1, line.trim()))
            .collect();
        if hits.is_empty() {
            Ok(format!("[find] '{query}' not found on {url}."))
        } else {
            Ok(format!("{header}\n{}", hits.join("\n")))
        }
    }

    async fn do_screenshot(
        &self,
        session: &str,
        command: Option<&Value>,
    ) -> AppResult<(String, Option<SearchResult>)> {
        let value = command.cloned().unwrap_or(Value::Null);
        let url = match resolve_ref_or_url(&value, &self.refs, session) {
            RefTarget::Url(url) => url,
            RefTarget::ResolvedWithoutUrl(ref_id) => {
                return Ok((no_url_ref_message("screenshot", &ref_id), None))
            }
            RefTarget::UnknownRef(ref_id) => {
                return Ok((stale_ref_message("screenshot", &ref_id), None))
            }
            RefTarget::Missing => {
                return Ok(("[web.run] screenshot needs a ref_id or url.".into(), None))
            }
        };
        let pageno = value.get("pageno").and_then(Value::as_u64).unwrap_or(0);
        if !self.settings.mode.allows_fetch() {
            return Err(AppError::Message(format!(
                "search mode does not allow fetching pages: {:?}",
                self.settings.mode
            )));
        }
        let page = self.fetch_page(&url).await?;
        let bytes = page.pdf_bytes.ok_or_else(|| {
            AppError::Message("screenshot only supports PDF sources, matching Codex web.run".into())
        })?;
        let capture = crate::web_search_pdf::render_pdf_page(&bytes, pageno as usize)
            .map_err(AppError::Message)?;
        let data_url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&capture.png)
        );
        let snippet = (!capture.page_text.trim().is_empty())
            .then(|| truncate_chars(capture.page_text.trim(), 2_000));
        let result = {
            let mut sessions = self
                .refs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sessions
                .entry(session.to_string())
                .or_default()
                .record_pdf_screenshot(
                    page.url.clone(),
                    pageno,
                    data_url,
                    capture.width,
                    capture.height,
                    snippet.clone(),
                )
        };
        let mut output = format!(
            "[screenshot] {} page {} of {} rendered as {} ({}x{} PNG).",
            page.url,
            pageno + 1,
            capture.page_count,
            result.ref_id,
            capture.width,
            capture.height
        );
        if let Some(text) = snippet {
            output.push_str("\nExtracted page text:\n");
            output.push_str(&text);
        }
        Ok((output, Some(result)))
    }

    async fn do_structured_query(
        &self,
        session: &str,
        query: &str,
        domain: &str,
        policy: &DomainPolicy,
        results: &mut Vec<SearchResult>,
    ) -> AppResult<String> {
        if query.trim().is_empty() {
            return Ok(format!("[{domain}] no query provided."));
        }
        if !self.settings.mode.allows_search() {
            return Err(AppError::Message(format!(
                "search mode does not allow querying an index: {:?}",
                self.settings.mode
            )));
        }
        let focused = format!("{domain}: {query}");
        let raw = self.backend.text_search(&focused).await?;
        let raw = apply_domain_policy(raw, policy, &[]);
        let mut stored = {
            let mut sessions = self
                .refs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sessions.entry(session.to_string()).or_default().record(raw)
        };
        let mut text = format!("[{domain}] {query}\n");
        for result in stored.iter().take(5) {
            text.push_str(&format!(
                "- {} {}\n",
                result.title.as_deref().unwrap_or("(untitled)"),
                result.url.as_deref().unwrap_or("")
            ));
            if let Some(snippet) = &result.snippet {
                text.push_str(&format!("    {}\n", snippet));
            }
        }
        if stored.is_empty() {
            text.push_str("No sources found.\n");
        }
        results.append(&mut stored);
        Ok(text)
    }

    fn do_time(&self, command: Option<&Value>, last_query: &Option<String>) -> AppResult<String> {
        let value = command.cloned().unwrap_or(Value::Null);
        let tz_query = field_str(&value, "utc_offset")
            .or_else(|| field_str(&value, "timezone"))
            .or_else(|| field_str(&value, "query"))
            .or_else(|| last_query.clone())
            .unwrap_or_else(|| "+00:00".to_string());
        let now = chrono::Utc::now();
        let rendered = parse_utc_offset(&tz_query)
            .map(|seconds| {
                let local = now + chrono::Duration::seconds(i64::from(seconds));
                format!("{} {tz_query}", local.format("%Y-%m-%d %H:%M:%S"))
            })
            .unwrap_or_else(|| format!("{} UTC", now.format("%Y-%m-%d %H:%M:%S")));
        Ok(format!("[time] {rendered}"))
    }

    async fn fetch_page(&self, url: &str) -> AppResult<FetchedPage> {
        let mut current = url::Url::parse(&is_safe_url(url).map_err(AppError::Message)?)
            .map_err(|error| AppError::Message(format!("invalid safe URL: {error}")))?;
        let mut redirect_count = 0usize;
        let response = loop {
            let client = pinned_safe_client(&current).await?;
            let response = client
                .get(current.clone())
                .header(
                    "Accept",
                    "text/html,application/xhtml+xml,text/plain,application/pdf,*/*",
                )
                .send()
                .await
                .map_err(|error| {
                    AppError::Message(format!("fetch failed for {current}: {error}"))
                })?;
            if !response.status().is_redirection() {
                break response;
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| AppError::Message("redirect omitted Location header".into()))?;
            if current.as_str() == location {
                return Err(AppError::Message("redirect loop detected".into()));
            }
            redirect_count += 1;
            if redirect_count > MAX_REDIRECTS {
                return Err(AppError::Message("fetch exceeded redirect limit".into()));
            }
            let next = current
                .join(location)
                .map_err(|error| AppError::Message(format!("invalid redirect URL: {error}")))?;
            current = url::Url::parse(&is_safe_url(next.as_str()).map_err(AppError::Message)?)
                .map_err(|error| AppError::Message(format!("invalid redirect URL: {error}")))?;
        };
        let response = response.error_for_status().map_err(|error| {
            AppError::Message(format!("fetch returned an error for {current}: {error}"))
        })?;
        let final_url = current.to_string();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let bytes = read_bounded(response).await?;
        if content_type.contains("pdf") {
            let text =
                crate::web_search_pdf::extract_pdf_text(&bytes).map_err(AppError::Message)?;
            return Ok(FetchedPage {
                url: final_url,
                title: None,
                text,
                pdf_bytes: Some(bytes),
            });
        }
        if bytes.starts_with(b"%PDF-") {
            let text =
                crate::web_search_pdf::extract_pdf_text(&bytes).map_err(AppError::Message)?;
            return Ok(FetchedPage {
                url: final_url,
                title: None,
                text,
                pdf_bytes: Some(bytes),
            });
        }
        let text = String::from_utf8_lossy(&bytes).to_string();
        let title = extract_title(&text);
        Ok(FetchedPage {
            url: final_url,
            title,
            text,
            pdf_bytes: None,
        })
    }
}

fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open_end = html[start..].find('>')? + start;
    let close = lower[open_end..].find("</title>")? + open_end;
    let title = html[open_end + 1..close].trim();
    if title.is_empty() {
        None
    } else {
        Some(strip_html_to_text(title))
    }
}

/// Wire-visible message for a `ref_id` that this process can no longer
/// resolve. Tool output is intentionally English — models mirror the tool's
/// language — and naming the cause ("ref store was reset") is what steers the
/// model back to running a fresh search instead of guessing a URL.
fn stale_ref_message(command: &str, ref_id: &str) -> String {
    format!(
        "[web.run] {command}: ref_id '{ref_id}' is no longer valid (the ref store was reset); run a new search first."
    )
}

/// Message for a `ref_id` that resolved in the store but whose search result
/// carries no URL (Brave may omit the field). Re-searching returns the same
/// URL-less result, so the honest guidance is to pick another result or pass
/// an explicit URL instead of looping.
fn no_url_ref_message(command: &str, ref_id: &str) -> String {
    format!(
        "[web.run] {command}: ref_id '{ref_id}' resolved but its search result has no URL; re-searching returns the same result, so pick a different result or use an explicit url."
    )
}

/// What a command's `ref_id`/`url` fields resolved to. The *reason* a lookup
/// failed travels out of the single lock acquisition with the result, so
/// callers can report it without taking the lock a second time and racing a
/// prune between the two.
enum RefTarget {
    /// A usable absolute URL.
    Url(String),
    /// `ref_id` is in this session's store but its search result carries no
    /// URL (Brave may omit the field).
    ResolvedWithoutUrl(String),
    /// `ref_id` was given but this process cannot resolve it.
    UnknownRef(String),
    /// No `ref_id`/`url`/`uri`/`href` field at all.
    Missing,
}

fn resolve_ref_or_url(
    value: &Value,
    refs: &Mutex<HashMap<String, RefStore>>,
    session: &str,
) -> RefTarget {
    let explicit_url = || {
        field_str(value, "url")
            .or_else(|| field_str(value, "uri"))
            .or_else(|| field_str(value, "href"))
    };
    let Some(ref_id) = field_str(value, "ref_id") else {
        return explicit_url().map_or(RefTarget::Missing, RefTarget::Url);
    };
    let stored = {
        let sessions = refs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        sessions
            .get(session)
            .and_then(|refs| refs.lookup(&ref_id))
            .map(|result| result.url.clone())
    };
    match stored {
        Some(Some(url)) => RefTarget::Url(url),
        // The ref resolved but has no URL. `no_url_ref_message` tells the
        // model to pass an explicit url instead, so an explicit url sent
        // alongside the stale ref_id has to actually work — otherwise the
        // advice sends the model into a loop.
        Some(None) => explicit_url().map_or(RefTarget::ResolvedWithoutUrl(ref_id), RefTarget::Url),
        None => {
            if url::Url::parse(&ref_id)
                .ok()
                .is_some_and(|url| matches!(url.scheme(), "http" | "https"))
            {
                return RefTarget::Url(ref_id);
            }
            explicit_url().map_or(RefTarget::UnknownRef(ref_id), RefTarget::Url)
        }
    }
}

fn render_search_results(query: &str, results: &[SearchResult]) -> String {
    if results.is_empty() {
        return format!("[search] No results for '{query}'.");
    }
    let mut out = format!("[search] {query}\n");
    for (i, result) in results.iter().take(20).enumerate() {
        out.push_str(&format!(
            "{}. [{}] {}\n",
            i + 1,
            result.ref_id,
            result.title.as_deref().unwrap_or("(untitled)")
        ));
        if let Some(url) = &result.url {
            out.push_str(&format!("   {url}\n"));
        }
        if let Some(snippet) = &result.snippet {
            out.push_str(&format!("   {}\n", snippet));
        }
    }
    out
}

fn apply_domain_policy(
    results: Vec<RawResult>,
    policy: &DomainPolicy,
    command_domains: &[String],
) -> Vec<RawResult> {
    let command_allow: Vec<String> = command_domains
        .iter()
        .map(|domain| normalize_domain(domain))
        .filter(|domain| !domain.is_empty())
        .collect();
    let allow_unrestricted = policy.allow.is_empty() && command_allow.is_empty();
    let allow: Vec<String> = if policy.allow.is_empty() {
        command_allow
    } else if command_allow.is_empty() {
        policy.allow.clone()
    } else {
        let mut intersection = Vec::new();
        for requested in &command_allow {
            for configured in &policy.allow {
                if host_matches_domain(requested, configured) {
                    intersection.push(requested.clone());
                } else if host_matches_domain(configured, requested) {
                    intersection.push(configured.clone());
                }
            }
        }
        intersection.sort();
        intersection.dedup();
        intersection
    };
    results
        .into_iter()
        .filter(|result| {
            let Some(url) = &result.url else {
                return true;
            };
            let host = url::Url::parse(url.as_str())
                .ok()
                .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase))
                .unwrap_or_default();
            if policy
                .block
                .iter()
                .any(|blocked| host_matches_domain(&host, blocked))
            {
                return false;
            }
            if allow_unrestricted {
                return true;
            }
            allow
                .iter()
                .any(|allowed| host_matches_domain(&host, allowed))
        })
        .collect()
}

fn host_matches_domain(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Request-level filters can only narrow the persisted policy. They never
/// relax a blocked domain configured by the user.
fn effective_domain_policy(
    persisted: &DomainPolicy,
    request_settings: &Option<Value>,
) -> DomainPolicy {
    let filters = request_settings
        .as_ref()
        .and_then(|value| value.get("filters"));
    let request_allow = filters
        .and_then(|value| value.get("allowed_domains"))
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(normalize_domain)
                .filter(|domain| !domain.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut block = persisted
        .block
        .iter()
        .map(|domain| normalize_domain(domain))
        .filter(|domain| !domain.is_empty())
        .collect::<Vec<_>>();
    if let Some(values) = filters
        .and_then(|value| value.get("blocked_domains"))
        .and_then(Value::as_array)
    {
        block.extend(
            values
                .iter()
                .filter_map(Value::as_str)
                .map(normalize_domain)
                .filter(|domain| !domain.is_empty()),
        );
    }
    block.sort();
    block.dedup();

    let persisted_allow: Vec<String> = persisted
        .allow
        .iter()
        .map(|domain| normalize_domain(domain))
        .filter(|domain| !domain.is_empty())
        .collect();
    let allow = match (persisted_allow.is_empty(), request_allow.is_empty()) {
        (true, true) => Vec::new(),
        (false, true) => persisted_allow,
        (true, false) => request_allow,
        (false, false) => persisted_allow
            .into_iter()
            .filter(|domain| {
                request_allow
                    .iter()
                    .any(|requested| domains_overlap(domain, requested))
            })
            .collect(),
    };
    DomainPolicy { allow, block }
}

fn normalize_domain(value: &str) -> String {
    value
        .trim()
        .trim_start_matches("*.")
        .trim_start_matches('.')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn domains_overlap(left: &str, right: &str) -> bool {
    left == right || left.ends_with(&format!(".{right}")) || right.ends_with(&format!(".{left}"))
}

/// `external_web_access` is boolean in Live/Cached mode and the string
/// `indexed` for index-only access. Only explicit false fully prohibits this
/// local backend; persisted Vellum mode remains the upper capability bound.
fn request_external_access(settings: &Option<Value>) -> Option<bool> {
    settings
        .as_ref()
        .and_then(|value| value.get("external_web_access"))
        .and_then(Value::as_bool)
}

fn response_length_default(settings: &Option<Value>) -> usize {
    let size = settings
        .as_ref()
        .and_then(|value| value.get("search_context_size").and_then(Value::as_str))
        .unwrap_or("medium");
    response_length_from_size(size)
}

fn response_length_from_size(size: &str) -> usize {
    match size {
        "low" => 2_000,
        "medium" => 6_000,
        "high" => 16_000,
        _ => 6_000,
    }
}

fn response_length_value(value: &Value) -> usize {
    if let Some(number) = value.get("value").and_then(Value::as_str) {
        return response_length_from_size(number);
    }
    if let Some(text) = value.as_str() {
        return response_length_from_size(text);
    }
    if let Some(number) = value.as_u64() {
        return number as usize;
    }
    6_000
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str("\n[truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssrf_blocks_private_and_metadata_hosts() {
        assert!(is_safe_url("http://127.0.0.1/").is_err());
        assert!(is_safe_url("http://10.0.0.1/").is_err());
        assert!(is_safe_url("http://192.168.1.1/").is_err());
        assert!(is_safe_url("http://169.254.169.254/").is_err());
        assert!(is_safe_url("http://localhost/").is_err());
        assert!(is_safe_url("http://[::1]/").is_err());
        assert!(is_safe_url("ftp://example.com/").is_err());
        assert!(is_safe_url("http://user:pass@example.com/").is_err());
        assert!(is_safe_url("https://example.com/path").is_ok());
    }

    #[test]
    fn ssrf_blocks_documentation_ranges() {
        assert!(is_safe_url("http://192.0.2.1/").is_err());
        assert!(is_safe_url("http://198.51.100.1/").is_err());
        assert!(is_safe_url("http://203.0.113.1/").is_err());
        assert!(is_safe_url("http://100.64.0.1/").is_err());
    }

    #[test]
    fn html_extraction_strips_tags_and_numbers_lines() {
        let html = "<html><head><title>Hi</title><style>x{}</style></head>\
                    <body><h1>Title</h1><p>Hello &amp; world</p>\
                    <a href=\"/l/?uddg=https%3A%2F%2Freal.example\">link</a></body></html>";
        let (text, links) = extract_text(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello & world"));
        assert!(text.starts_with("1: "));
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].url, "/l/?uddg=https%3A%2F%2Freal.example");
    }

    #[test]
    fn strip_html_to_text_skips_cjk_emoji_and_arrows_inside_scripts() {
        // Every multibyte character class that used to land the byte cursor
        // mid-character and panic on the next `html[i..]` slice.
        let html = "<html><body><script>const a = '狀'; const b = '🙂';</script>\
                    <style>.x::before{content:\"→\"}</style>\
                    <p>中文正文</p></body></html>";
        let text = strip_html_to_text(html);
        assert!(!text.contains("狀"));
        assert!(!text.contains("🙂"));
        assert!(!text.contains("→"));
        assert!(text.contains("中文正文"));
    }

    #[test]
    fn strip_html_to_text_tolerates_unclosed_scripts_and_lone_lt() {
        // `<script>` / `<style>` that never close, plus a lone `<` inside
        // script text, must not panic and must not leak script content.
        let html = "<script>const s = '<未閉合'; <style>color: #333; <p>body";
        let text = strip_html_to_text(html);
        assert!(!text.contains("未閉合"));
        assert!(!text.contains("color"));
        assert!(!text.contains("body"));
    }

    #[test]
    fn strip_html_to_text_stays_linear_on_unclosed_tag_storms() {
        // Regression guard for the O(n²) tag scan: a tag-like '<' used to
        // rescan the whole remaining document, so "<a" repeated to the 5 MiB
        // fetch cap burned ~11 s of CPU (or, with a scan window, truncated
        // real tags and leaked attribute content — see the long-tag test).
        // The `gt_exhausted` latch makes the failed scan happen exactly once,
        // so the walk is strictly O(n): 3 MiB of "<a" completes in
        // milliseconds.
        let n = 1_500_000; // "<a" × n == 3 MiB
        let html = "<a".repeat(n);
        let started = std::time::Instant::now();
        let text = strip_html_to_text(&html);
        assert_eq!(text.len(), n); // every lone '<' drops, 'a' survives
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "strip_html_to_text took {:?} on {} tag-like lone '<'s",
            started.elapsed(),
            n
        );
    }

    #[test]
    fn strip_html_to_text_consumes_long_tags_without_leaking_attributes() {
        // A tag whose attribute payload exceeds 8 KiB (inline SVG path data,
        // base64 data URIs, SSR data-* JSON) must be consumed whole. The
        // earlier TAG_SCAN_LIMIT window cut the scan short and leaked the
        // attributes into the extracted text.
        let path = "M0 0 L1 1 ".repeat(1_200); // ~14 KiB of attributes
        let html = format!("<p>before</p><svg><path d=\"{path}\"/></svg><p>after</p>");
        let text = strip_html_to_text(&html);
        assert!(text.contains("before"));
        assert!(text.contains("after"));
        assert!(
            !text.contains("M0 0"),
            "path data leaked into extracted text: {}",
            text
        );
    }

    #[test]
    fn domain_policy_filters_results() {
        let policy = DomainPolicy {
            allow: vec![],
            block: vec!["spam.test".to_string()],
        };
        let results = vec![
            RawResult {
                kind: "text_result".into(),
                url: Some("https://good.test/a".into()),
                title: None,
                snippet: None,
                image_url: None,
            },
            RawResult {
                kind: "text_result".into(),
                url: Some("https://spam.test/b".into()),
                title: None,
                snippet: None,
                image_url: None,
            },
        ];
        let kept = apply_domain_policy(results, &policy, &[]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].url.as_deref(), Some("https://good.test/a"));
    }

    #[test]
    fn domain_matching_does_not_accept_suffix_confusion() {
        assert!(host_matches_domain("docs.example.com", "example.com"));
        assert!(host_matches_domain("example.com", "example.com"));
        assert!(!host_matches_domain("notexample.com", "example.com"));
    }

    #[test]
    fn command_domains_cannot_relax_persisted_allowlist() {
        let policy = DomainPolicy {
            allow: vec!["example.com".into()],
            block: Vec::new(),
        };
        let results = vec![
            RawResult {
                kind: "text_result".into(),
                url: Some("https://docs.example.com/a".into()),
                title: None,
                snippet: None,
                image_url: None,
            },
            RawResult {
                kind: "text_result".into(),
                url: Some("https://other.test/a".into()),
                title: None,
                snippet: None,
                image_url: None,
            },
        ];
        let kept = apply_domain_policy(results.clone(), &policy, &["com".into()]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].url.as_deref(), Some("https://docs.example.com/a"));
        assert!(apply_domain_policy(results, &policy, &["unrelated.test".into()]).is_empty());
    }

    #[test]
    fn request_filters_only_narrow_persisted_policy() {
        let persisted = DomainPolicy {
            allow: vec!["example.com".into(), "rust-lang.org".into()],
            block: vec!["blocked.example.com".into()],
        };
        let settings = Some(json!({
            "filters": {
                "allowed_domains": ["docs.example.com", "unrelated.test"],
                "blocked_domains": ["private.example.com"]
            }
        }));
        let effective = effective_domain_policy(&persisted, &settings);
        assert_eq!(effective.allow, vec!["example.com"]);
        assert_eq!(
            effective.block,
            vec!["blocked.example.com", "private.example.com"]
        );
    }

    #[test]
    fn recency_maps_to_backend_native_ranges() {
        assert_eq!(recency_bucket(Some(1)), Some("pd"));
        assert_eq!(recency_bucket(Some(7)), Some("pw"));
        assert_eq!(recency_bucket(Some(30)), Some("pm"));
        assert_eq!(recency_bucket(Some(365)), Some("py"));
        assert_eq!(recency_bucket(Some(366)), None);
    }

    #[test]
    fn open_window_preserves_absolute_line_numbers() {
        let (text, _) = extract_text_window("one<br>two<br>three<br>four", 3, 2);
        assert_eq!(text, "3: three\n4: four\n");
    }

    #[test]
    fn open_and_screenshot_accept_literal_url_in_ref_id_field() {
        let refs = Mutex::new(HashMap::new());
        assert!(matches!(
            resolve_ref_or_url(
                &json!({"ref_id": "https://example.com/document.pdf"}),
                &refs,
                "session",
            ),
            RefTarget::Url(url) if url == "https://example.com/document.pdf"
        ));
    }

    #[test]
    fn parses_the_native_codex_object_of_arrays_shape() {
        let commands: SearchCommands = serde_json::from_value(json!({
            "search_query": [{"q": "rust", "recency": 7}],
            "open": [{"ref_id": "turn1search0", "lineno": 12}],
            "response_length": "short"
        }))
        .unwrap();
        assert_eq!(commands.search_query.as_ref().map(Vec::len), Some(1));
        assert_eq!(commands.open.as_ref().map(Vec::len), Some(1));
        assert_eq!(commands.response_length.as_ref(), Some(&json!("short")));
    }

    #[test]
    fn response_uses_the_native_codex_snake_case_wire_keys() {
        let value = serde_json::to_value(SearchResponse {
            encrypted_output: None,
            output: "ok".into(),
            results: vec![SearchResult {
                kind: "text_result".into(),
                ref_id: "turn0search0".into(),
                url: Some("https://example.com".into()),
                title: None,
                snippet: None,
                image_url: None,
                width: None,
                height: None,
                pageno: None,
            }],
        })
        .unwrap();
        assert!(value.get("encrypted_output").is_some());
        assert!(value.get("encryptedOutput").is_none());
        assert_eq!(value["results"][0]["ref_id"], "turn0search0");
        assert!(value["results"][0].get("refId").is_none());
    }

    struct FakeBackend {
        results: Vec<RawResult>,
    }

    #[async_trait::async_trait]
    impl SearchBackend for FakeBackend {
        async fn text_search(&self, _query: &str) -> AppResult<Vec<RawResult>> {
            Ok(self.results.clone())
        }
        async fn image_search(&self, _query: &str) -> AppResult<Vec<RawResult>> {
            Ok(self
                .results
                .iter()
                .map(|r| {
                    let mut r = r.clone();
                    r.kind = "image_result".into();
                    r
                })
                .collect())
        }
    }

    /// Backend that panics on its first search call, then behaves normally.
    /// Mirrors a parser bug inside an attacker-controlled page: with
    /// `panic = "abort"` gone, the engine must turn that into a request error
    /// instead of killing the whole process.
    struct PanicOnceBackend {
        results: Vec<RawResult>,
        panicked: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl SearchBackend for PanicOnceBackend {
        async fn text_search(&self, _query: &str) -> AppResult<Vec<RawResult>> {
            if !self
                .panicked
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                panic!("boom: backend exploded");
            }
            Ok(self.results.clone())
        }
        async fn image_search(&self, _query: &str) -> AppResult<Vec<RawResult>> {
            Ok(Vec::new())
        }
    }

    fn test_settings() -> WebSearchSettings {
        WebSearchSettings {
            enabled: true,
            mode: SearchMode::Indexed,
            ..WebSearchSettings::default()
        }
    }

    fn empty_request() -> SearchRequest {
        SearchRequest {
            id: None,
            model: None,
            reasoning: None,
            input: None,
            commands: None,
            settings: None,
            max_output_tokens: None,
        }
    }

    #[tokio::test]
    async fn search_query_records_and_cites_results() {
        let backend = Box::new(FakeBackend {
            results: vec![
                RawResult {
                    kind: "text_result".into(),
                    url: Some("https://a.example/".into()),
                    title: Some("A".into()),
                    snippet: Some("snip a".into()),
                    image_url: None,
                },
                RawResult {
                    kind: "text_result".into(),
                    url: Some("https://b.example/".into()),
                    title: Some("B".into()),
                    snippet: None,
                    image_url: None,
                },
            ],
        });
        let engine = SearchEngine::with_backend(test_settings(), backend).unwrap();
        let request = SearchRequest {
            commands: Some(SearchCommands {
                search_query: Some(vec![json!({"q": "news"})]),
                ..SearchCommands::default()
            }),
            ..empty_request()
        };
        let response = engine.run(&request).await.unwrap();
        assert!(response.output.contains("[search] news"));
        assert_eq!(response.results.len(), 2);
        assert_eq!(response.results[0].ref_id, "turn1search0");
        assert_eq!(response.results[1].ref_id, "turn1search1");
    }

    #[tokio::test]
    async fn ref_ids_are_isolated_between_codex_sessions() {
        let backend = Box::new(FakeBackend {
            results: vec![RawResult {
                kind: "text_result".into(),
                url: Some("https://private-to-session-a.example/".into()),
                title: Some("A".into()),
                snippet: None,
                image_url: None,
            }],
        });
        let engine = SearchEngine::with_backend(test_settings(), backend).unwrap();
        let searched = engine
            .run(&SearchRequest {
                id: Some("session-a".into()),
                commands: Some(SearchCommands {
                    search_query: Some(vec![json!({"q": "secret"})]),
                    ..SearchCommands::default()
                }),
                ..empty_request()
            })
            .await
            .unwrap();
        assert_eq!(searched.results[0].ref_id, "turn1search0");

        let opened = engine
            .run(&SearchRequest {
                id: Some("session-b".into()),
                commands: Some(SearchCommands {
                    open: Some(vec![json!({"ref_id": "turn1search0"})]),
                    ..SearchCommands::default()
                }),
                ..empty_request()
            })
            .await
            .unwrap();
        assert!(
            opened
                .output
                .contains("ref_id 'turn1search0' is no longer valid"),
            "output was: {}",
            opened.output
        );
        assert!(!opened.output.contains("needs a ref_id or url"));
        assert!(!opened.output.contains("private-to-session-a"));
    }

    #[test]
    fn stale_ref_message_is_english_and_names_the_cause() {
        let message = stale_ref_message("open", "turn4search9");
        assert!(message.starts_with("[web.run] open:"));
        assert!(message.contains("ref_id 'turn4search9'"));
        assert!(message.contains("is no longer valid"));
        assert!(message.contains("ref store was reset"));
        assert!(message.contains("run a new search first"));
    }

    #[test]
    fn explicit_url_wins_over_a_ref_id_that_carries_no_url() {
        // `no_url_ref_message` tells the model to pass an explicit url. Models
        // keep the ref_id in the command when they do, so the explicit url has
        // to win — otherwise the advice loops the model forever.
        let refs = Mutex::new(HashMap::new());
        {
            let mut sessions = refs.lock().unwrap();
            sessions
                .entry("session".to_string())
                .or_insert_with(RefStore::new)
                .record(vec![RawResult {
                    kind: "text_result".into(),
                    url: None,
                    title: Some("No link".into()),
                    snippet: None,
                    image_url: None,
                }]);
        }

        let with_url = json!({"ref_id": "turn1search0", "url": "https://explicit.example/"});
        assert!(matches!(
            resolve_ref_or_url(&with_url, &refs, "session"),
            RefTarget::Url(url) if url == "https://explicit.example/"
        ));

        // Without one there is nothing to fall back to, so the caller still
        // gets the honest "resolved but has no URL" reason.
        let bare = json!({"ref_id": "turn1search0"});
        assert!(matches!(
            resolve_ref_or_url(&bare, &refs, "session"),
            RefTarget::ResolvedWithoutUrl(ref_id) if ref_id == "turn1search0"
        ));

        // An unknown ref with no explicit url stays distinguishable from both.
        let unknown = json!({"ref_id": "turn9search9"});
        assert!(matches!(
            resolve_ref_or_url(&unknown, &refs, "session"),
            RefTarget::UnknownRef(ref_id) if ref_id == "turn9search9"
        ));
        assert!(matches!(
            resolve_ref_or_url(&json!({}), &refs, "session"),
            RefTarget::Missing
        ));
    }

    #[tokio::test]
    async fn click_distinguishes_ref_without_url_from_missing_ref() {
        // Brave may store a search result with no URL. Clicking it must not
        // claim "the ref store was reset" — re-searching yields the same
        // URL-less result.
        let backend = Box::new(FakeBackend {
            results: vec![RawResult {
                kind: "text_result".into(),
                url: None,
                title: Some("No link".into()),
                snippet: None,
                image_url: None,
            }],
        });
        let mut settings = test_settings();
        settings.mode = SearchMode::Live;
        let engine = SearchEngine::with_backend(settings, backend).unwrap();
        let searched = engine
            .run(&SearchRequest {
                id: Some("no-url".into()),
                commands: Some(SearchCommands {
                    search_query: Some(vec![json!({"q": "query"})]),
                    ..SearchCommands::default()
                }),
                ..empty_request()
            })
            .await
            .unwrap();
        let ref_id = searched.results[0].ref_id.clone();

        let clicked = engine
            .run(&SearchRequest {
                id: Some("no-url".into()),
                commands: Some(SearchCommands {
                    click: Some(vec![json!({"ref_id": ref_id, "selector": "x"})]),
                    ..SearchCommands::default()
                }),
                ..empty_request()
            })
            .await
            .unwrap();
        assert!(
            clicked.output.contains("has no URL"),
            "output was: {}",
            clicked.output
        );
        assert!(!clicked.output.contains("ref store was reset"));
    }

    #[test]
    fn extract_anchors_handles_mixed_case_and_unicode_text() {
        let html = "<a href=\"https://a.example/\">A</a> \
                    <a HREF='https://b.example/'>中文 標題</a> \
                    <A href=\"#fragment\">frag</A> \
                    <a href=\"javascript:alert(1)\">x</a>";
        let links = extract_anchors(html);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].url, "https://a.example/");
        assert_eq!(links[0].text, "A");
        assert_eq!(links[1].url, "https://b.example/");
        assert_eq!(links[1].text, "中文 標題");
    }

    #[tokio::test]
    async fn engine_panics_are_caught_and_the_engine_survives() {
        let backend = Box::new(PanicOnceBackend {
            results: vec![RawResult {
                kind: "text_result".into(),
                url: Some("https://panic.example/".into()),
                title: Some("panic".into()),
                snippet: None,
                image_url: None,
            }],
            panicked: std::sync::atomic::AtomicBool::new(false),
        });
        let engine = SearchEngine::with_backend(test_settings(), backend).unwrap();
        let request = SearchRequest {
            id: Some("panic-test".into()),
            commands: Some(SearchCommands {
                search_query: Some(vec![json!({"q": "boom"})]),
                ..SearchCommands::default()
            }),
            ..empty_request()
        };

        let first = engine.run(&request).await;
        let message = first
            .expect_err("a backend panic must surface as a request error")
            .to_string();
        assert!(message.contains("panicked"), "message was: {message}");
        assert!(
            message.contains("boom: backend exploded"),
            "message was: {message}"
        );

        // The engine and its ref store must stay usable after the caught panic.
        let second = engine.run(&request).await;
        let response = second.expect("engine recovers after a caught panic");
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].ref_id, "turn1search0");
    }

    #[tokio::test]
    async fn engine_builds_without_a_brave_key_but_fails_closed_on_query() {
        // There is no zero-config fallback backend anymore, but building the
        // engine must still succeed with search enabled and no key —
        // otherwise proxy startup would depend on whether the user has
        // configured Brave yet. The query itself is what fails closed.
        let settings = WebSearchSettings {
            enabled: true,
            mode: SearchMode::Live,
            ..WebSearchSettings::default()
        };
        assert!(settings.brave_api_key.is_none());
        let engine = SearchEngine::new(settings).expect("engine must build without a Brave key");
        let request = SearchRequest {
            commands: Some(SearchCommands {
                search_query: Some(vec![json!({"q": "openai"})]),
                ..SearchCommands::default()
            }),
            ..empty_request()
        };
        let error = engine
            .run(&request)
            .await
            .expect_err("a query must fail closed without a Brave key")
            .to_string();
        assert!(
            error.contains("Brave Search requires an API key"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn disabled_mode_refuses_search() {
        let mut settings = test_settings();
        settings.mode = SearchMode::Cached;
        let engine =
            SearchEngine::with_backend(settings, Box::new(FakeBackend { results: vec![] }))
                .unwrap();
        let request = SearchRequest {
            commands: Some(SearchCommands {
                search_query: Some(vec![json!({"q": "x"})]),
                ..SearchCommands::default()
            }),
            ..empty_request()
        };
        assert!(engine.run(&request).await.is_err());
    }

    #[tokio::test]
    async fn time_command_is_local() {
        let engine =
            SearchEngine::with_backend(test_settings(), Box::new(FakeBackend { results: vec![] }))
                .unwrap();
        let request = SearchRequest {
            commands: Some(SearchCommands {
                time: Some(vec![json!({"utc_offset": "+08:00"})]),
                ..SearchCommands::default()
            }),
            ..empty_request()
        };
        let response = engine.run(&request).await.unwrap();
        assert!(response.output.contains("[time]"));
        assert!(response.output.contains("+08:00"));
    }

    #[tokio::test]
    async fn empty_commands_returns_noop() {
        let engine =
            SearchEngine::with_backend(test_settings(), Box::new(FakeBackend { results: vec![] }))
                .unwrap();
        let response = engine.run(&empty_request()).await.unwrap();
        assert!(response.output.contains("No search commands"));
    }

    #[tokio::test]
    async fn find_without_query_is_safe() {
        let engine =
            SearchEngine::with_backend(test_settings(), Box::new(FakeBackend { results: vec![] }))
                .unwrap();
        let request = SearchRequest {
            commands: Some(SearchCommands {
                find: Some(vec![json!({"ref_id": "turn0search0"})]),
                ..SearchCommands::default()
            }),
            ..empty_request()
        };
        let response = engine.run(&request).await.unwrap();
        assert!(response.output.contains("find needs a query"));
    }
}
