//! Search pipeline (plan M8).
//!
//! The `/v1/alpha/search` wire contract (Codex standalone search client) and
//! the [`RefStore`] that keeps `search/open/click/find` ref ids stable across
//! the multiple search calls of one task. The backend itself stays behind the
//! [`SearchEngine`] trait; the default engine fails closed exactly like
//! Desktop's Disabled mode.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Retention window for ref sessions (Desktop parity: 24h process lifetime).
pub const REF_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// One search result returned to Codex at `/v1/alpha/search`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pageno: Option<u64>,
}

/// The wire response returned to Codex at `/v1/alpha/search`. Vellum never
/// fabricates an OpenAI ciphertext; `encrypted_output` stays `null`.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub encrypted_output: Option<String>,
    pub output: String,
    pub results: Vec<SearchResult>,
}

/// Codex's standalone search wire format: one object whose command fields
/// each contain an array of operations.
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

/// A backend-agnostic result before ref ids are assigned.
#[derive(Debug, Clone)]
pub struct RawResult {
    pub kind: String,
    pub url: Option<String>,
    pub title: Option<String>,
    pub snippet: Option<String>,
    pub image_url: Option<String>,
}

/// Maps `ref_id` -> discovered url/title/snippet and any fetched page text.
/// Process-lifetime cache of public URLs/snippets only; ref ids stay valid
/// across the multiple `/alpha/search` calls of one task (search/open/click/
/// find continuity).
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
    pub fn record(&mut self, results: Vec<RawResult>) -> Vec<SearchResult> {
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

    pub fn lookup(&mut self, ref_id: &str) -> Option<&SearchResult> {
        self.last_used = Instant::now();
        self.entries.get(ref_id)
    }

    fn cache_page(&mut self, ref_id: &str, page: CachedPage) {
        self.last_used = Instant::now();
        self.pages.insert(ref_id.to_string(), page);
    }

    fn page(&mut self, ref_id: &str) -> Option<&CachedPage> {
        self.last_used = Instant::now();
        self.pages.get(ref_id)
    }

    fn expired(&self, max_age: Duration) -> bool {
        self.last_used.elapsed() > max_age
    }

    pub fn record_pdf_screenshot(
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

    /// Drop result entries when the store grows without bound (evict
    /// oldest-turn entries, Desktop parity).
    pub fn prune(&mut self, max_age: Duration) {
        if self.expired(max_age) {
            self.entries.clear();
            self.pages.clear();
            self.counter = 0;
            self.turn = 0;
            self.last_used = Instant::now();
            return;
        }
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
            self.pages
                .retain(|ref_id, _| self.entries.contains_key(ref_id));
        }
    }
}

#[derive(Debug, Clone)]
struct CachedPage {
    url: String,
    title: Option<String>,
    text: String,
    links: Vec<ExtractedLink>,
}

/// The backend seam: a real daemon wires Brave; the default fails closed
/// exactly like Desktop's Disabled mode.
#[async_trait]
pub trait SearchEngine: Send + Sync {
    async fn run(&self, request: &SearchRequest) -> Result<SearchResponse, String>;
}

/// Default engine when search is not configured. Desktop's default settings
/// have `enabled = false`, so its engine fails with this exact message; the
/// runtime's fail-closed default must match it byte-for-byte.
pub struct DisabledSearchEngine;

#[async_trait]
impl SearchEngine for DisabledSearchEngine {
    async fn run(&self, _request: &SearchRequest) -> Result<SearchResponse, String> {
        Err("third-party web search is disabled".to_string())
    }
}

#[derive(Debug, Clone)]
struct ExtractedLink {
    url: String,
    text: String,
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

fn strip_html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    let mut in_skip = false;
    while i < bytes.len() {
        if html[i..].starts_with('<') {
            let lower = html[i..]
                .split('>')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if lower.starts_with("<script") || lower.starts_with("<style") {
                in_skip = true;
            }
            if let Some(end) = html[i..].find('>') {
                let tag = html[i..i + end + 1].to_ascii_lowercase();
                if tag.starts_with("</script") || tag.starts_with("</style") {
                    in_skip = false;
                }
                i += end + 1;
                if !in_skip {
                    out.push(' ');
                }
                continue;
            }
            break;
        }
        out.push(html[i..].chars().next().unwrap_or(' '));
        i += html[i..].chars().next().unwrap_or(' ').len_utf8();
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_anchors(html: &str) -> Vec<ExtractedLink> {
    let mut out = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut search = 0;
    while let Some(pos) = lower[search..].find("<a ") {
        let abs = search + pos;
        let tag_end = match html[abs..].find('>') {
            Some(end) => abs + end,
            None => break,
        };
        let tag = &html[abs..tag_end];
        let href = extract_attr(tag, "href");
        let close = match html[tag_end..].to_ascii_lowercase().find("</a>") {
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

/// The pluggable backend seam (Desktop `SearchBackend`).
#[async_trait]
pub trait SearchBackend: Send + Sync {
    fn kind(&self) -> &'static str;
    async fn text_search(&self, query: &str) -> Result<Vec<RawResult>, String>;
    async fn image_search(&self, query: &str) -> Result<Vec<RawResult>, String>;
}

/// Parse a Brave Search API response into raw results.
pub fn parse_brave_web_results(response: &Value) -> Vec<RawResult> {
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

/// Brave Search API backend (requires an API key).
pub struct BraveBackend {
    api_key: String,
    client: reqwest::Client,
}

impl BraveBackend {
    pub fn new(api_key: String) -> Result<Self, String> {
        Ok(Self {
            api_key,
            client: reqwest::Client::new(),
        })
    }
}

#[async_trait]
impl SearchBackend for BraveBackend {
    fn kind(&self) -> &'static str {
        "brave"
    }

    async fn text_search(&self, query: &str) -> Result<Vec<RawResult>, String> {
        let response = self
            .client
            .get("https://api.search.brave.com/res/v1/web/search")
            .header("X-Subscription-Token", &self.api_key)
            .header("Accept", "application/json")
            .query(&[("q", query), ("count", "10")])
            .send()
            .await
            .map_err(|error| format!("Brave search failed: {error}"))?
            .error_for_status()
            .map_err(|error| format!("Brave search failed: {error}"))?
            .json::<Value>()
            .await
            .map_err(|error| format!("Brave search parse failed: {error}"))?;
        Ok(parse_brave_web_results(&response))
    }

    async fn image_search(&self, _query: &str) -> Result<Vec<RawResult>, String> {
        Ok(Vec::new())
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

/// A functional engine backed by a real [`SearchBackend`]: session-scoped ref
/// continuity for `search/open/click/find` over the `/v1/alpha/search` wire.
pub struct HttpSearchEngine {
    backend: Box<dyn SearchBackend>,
    refs: Mutex<HashMap<String, RefStore>>,
    fetch_client: reqwest::Client,
}

impl HttpSearchEngine {
    pub fn new(backend: Box<dyn SearchBackend>) -> Self {
        Self {
            backend,
            refs: Mutex::new(HashMap::new()),
            fetch_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent("Vellum-Proxy-Runtime/0.1")
                .build()
                .unwrap_or_default(),
        }
    }

    async fn fetch_page(&self, url: &str) -> Result<CachedPage, String> {
        let parsed = url::Url::parse(url).map_err(|error| format!("invalid page URL: {error}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err("page URL must use http or https".to_string());
        }
        let response = self
            .fetch_client
            .get(parsed)
            .send()
            .await
            .map_err(|error| format!("open failed: {error}"))?
            .error_for_status()
            .map_err(|error| format!("open failed: {error}"))?;
        let final_url = response.url().to_string();
        let html = response
            .text()
            .await
            .map_err(|error| format!("open body failed: {error}"))?;
        let title = extract_title(&html);
        let links = extract_anchors(&html)
            .into_iter()
            .filter_map(|link| {
                let url = url::Url::parse(&final_url).ok()?.join(&link.url).ok()?;
                matches!(url.scheme(), "http" | "https").then(|| ExtractedLink {
                    url: url.to_string(),
                    text: link.text,
                })
            })
            .collect();
        Ok(CachedPage {
            url: final_url,
            title,
            text: strip_html_to_text(&html),
            links,
        })
    }

    async fn open(&self, session: &str, item: &Value) -> Result<String, String> {
        let ref_id = item.get("ref_id").and_then(Value::as_str);
        let direct_url = item
            .get("url")
            .and_then(Value::as_str)
            .or_else(|| item.as_str());
        let url = if let Some(url) = direct_url {
            url.to_string()
        } else if let Some(ref_id) = ref_id {
            let mut sessions = self.refs.lock().map_err(|_| "search refs store poisoned")?;
            let Some(url) = sessions
                .get_mut(session)
                .and_then(|refs| refs.lookup(ref_id))
                .and_then(|result| result.url.clone())
            else {
                return Ok(format!("[web.run] open: unknown ref_id '{ref_id}'."));
            };
            url
        } else {
            return Ok("[web.run] open command needs a ref_id or url.".into());
        };
        let page = self.fetch_page(&url).await?;
        let output = render_page(&page);
        if let Some(ref_id) = ref_id {
            let mut sessions = self.refs.lock().map_err(|_| "search refs store poisoned")?;
            sessions
                .entry(session.to_string())
                .or_default()
                .cache_page(ref_id, page);
        }
        Ok(output)
    }

    async fn click(&self, session: &str, item: &Value) -> Result<String, String> {
        let ref_id = item
            .get("ref_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let link_index = item.get("id").and_then(Value::as_u64).unwrap_or(0);
        let source_url = {
            let mut sessions = self.refs.lock().map_err(|_| "search refs store poisoned")?;
            sessions
                .get_mut(session)
                .and_then(|refs| refs.lookup(ref_id))
                .and_then(|result| result.url.clone())
                .ok_or_else(|| format!("click: unknown ref_id '{ref_id}'"))?
        };
        let source = {
            let mut sessions = self.refs.lock().map_err(|_| "search refs store poisoned")?;
            sessions
                .get_mut(session)
                .and_then(|refs| refs.page(ref_id))
                .cloned()
        };
        let source = match source {
            Some(page) => page,
            None => self.fetch_page(&source_url).await?,
        };
        let target = link_index
            .checked_sub(1)
            .and_then(|index| source.links.get(index as usize))
            .ok_or_else(|| format!("click: link id {link_index} was not found in '{ref_id}'"))?;
        let page = self.fetch_page(&target.url).await?;
        Ok(render_page(&page))
    }

    fn find(&self, session: &str, item: &Value) -> Result<String, String> {
        let ref_id = item
            .get("ref_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let pattern = item
            .get("pattern")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut sessions = self.refs.lock().map_err(|_| "search refs store poisoned")?;
        let page = sessions
            .get_mut(session)
            .and_then(|refs| refs.page(ref_id))
            .ok_or_else(|| format!("find: open '{ref_id}' before searching within it"))?;
        let needle = pattern.to_ascii_lowercase();
        let matches = page
            .text
            .lines()
            .enumerate()
            .filter(|(_, line)| line.to_ascii_lowercase().contains(&needle))
            .take(20)
            .map(|(index, line)| format!("L{}: {}", index + 1, line.trim()))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            Ok(format!("[find] '{pattern}' was not found in {ref_id}."))
        } else {
            Ok(format!(
                "[find] {ref_id} / {pattern}\n{}",
                matches.join("\n")
            ))
        }
    }
}

fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open_end = html[start..].find('>')? + start + 1;
    let end = lower[open_end..].find("</title>")? + open_end;
    let title = strip_html_to_text(&html[open_end..end]);
    (!title.is_empty()).then_some(title)
}

fn render_page(page: &CachedPage) -> String {
    let mut output = format!(
        "[open] {}\n{}\n{}",
        page.url,
        page.title.as_deref().unwrap_or_default(),
        page.text.lines().take(200).collect::<Vec<_>>().join("\n")
    );
    if !page.links.is_empty() {
        output.push_str("\nLinks:\n");
        for (index, link) in page.links.iter().take(40).enumerate() {
            output.push_str(&format!("{}. [{}] {}\n", index + 1, link.text, link.url));
        }
    }
    output
}

#[async_trait]
impl SearchEngine for HttpSearchEngine {
    async fn run(&self, request: &SearchRequest) -> Result<SearchResponse, String> {
        let session = session_key(request);
        let commands = request.commands.clone().unwrap_or_default();

        // Collect the backend work first (never hold the refs lock across an
        // await), then record results under the lock.
        let mut text_queries = Vec::new();
        let mut image_queries = Vec::new();
        if let Some(queries) = &commands.search_query {
            for item in queries {
                let query = item.get("q").and_then(Value::as_str).unwrap_or_default();
                if query.is_empty() {
                    continue;
                }
                text_queries.push(query.to_string());
            }
        }
        if let Some(queries) = &commands.image_query {
            for item in queries {
                let query = item.get("q").and_then(Value::as_str).unwrap_or_default();
                if query.is_empty() {
                    continue;
                }
                image_queries.push(query.to_string());
            }
        }
        let mut raw_results = Vec::new();
        for query in &text_queries {
            raw_results.extend(self.backend.text_search(query).await?);
        }
        for query in &image_queries {
            raw_results.extend(self.backend.image_search(query).await?);
        }

        let results = {
            let mut refs_by_session = self
                .refs
                .lock()
                .map_err(|_| "search refs store poisoned".to_string())?;
            refs_by_session.retain(|_, refs| !refs.expired(REF_RETENTION));
            let refs = refs_by_session.entry(session.clone()).or_default();
            refs.prune(REF_RETENTION);
            refs.record(raw_results)
        };
        let mut output_lines = Vec::new();
        let open_items = commands.open.unwrap_or_default();
        let click_items = commands.click.unwrap_or_default();
        let find_items = commands.find.unwrap_or_default();
        for item in open_items {
            output_lines.push(self.open(&session, &item).await?);
        }
        for item in click_items {
            output_lines.push(self.click(&session, &item).await?);
        }
        for item in find_items {
            output_lines.push(self.find(&session, &item)?);
        }
        Ok(SearchResponse {
            encrypted_output: None,
            output: output_lines.join("\n"),
            results,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ref_ids_are_turn_scoped_and_lookup_roundtrips() {
        let mut store = RefStore::new();
        let results = store.record(vec![
            RawResult {
                kind: "web_result".into(),
                url: Some("https://example.com".into()),
                title: Some("Example".into()),
                snippet: Some("A snippet".into()),
                image_url: None,
            },
            RawResult {
                kind: "web_result".into(),
                url: Some("https://example.org".into()),
                title: None,
                snippet: None,
                image_url: None,
            },
        ]);
        assert_eq!(results[0].ref_id, "turn1search0");
        assert_eq!(results[1].ref_id, "turn1search1");
        assert_eq!(
            store.lookup("turn1search0").unwrap().url.as_deref(),
            Some("https://example.com")
        );

        // A second search advances the turn so ref ids never collide.
        let second = store.record(vec![RawResult {
            kind: "web_result".into(),
            url: Some("https://example.net".into()),
            title: None,
            snippet: None,
            image_url: None,
        }]);
        assert_eq!(second[0].ref_id, "turn2search0");
        assert_eq!(
            store.lookup("turn2search0").unwrap().url.as_deref(),
            Some("https://example.net")
        );
    }

    #[tokio::test]
    async fn disabled_engine_fails_closed_with_desktop_message() {
        let engine = DisabledSearchEngine;
        let error = engine.run(&SearchRequest::default()).await.unwrap_err();
        assert_eq!(error, "third-party web search is disabled");
    }

    #[test]
    fn brave_json_parser_maps_results() {
        let brave = json!({
            "web": {"results": [
                {"url": "https://brave.example/a", "title": "Brave A", "description": "desc a"}
            ]}
        });
        let parsed = parse_brave_web_results(&brave);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].url.as_deref(), Some("https://brave.example/a"));
        assert_eq!(parsed[0].snippet.as_deref(), Some("desc a"));
    }

    #[tokio::test]
    async fn http_engine_keeps_ref_continuity_across_commands() {
        struct ScriptedBackend;
        #[async_trait]
        impl SearchBackend for ScriptedBackend {
            fn kind(&self) -> &'static str {
                "scripted"
            }
            async fn text_search(&self, _query: &str) -> Result<Vec<RawResult>, String> {
                Ok(vec![RawResult {
                    kind: "text_result".into(),
                    url: Some("https://example.com".into()),
                    title: Some("Example".into()),
                    snippet: None,
                    image_url: None,
                }])
            }
            async fn image_search(&self, _query: &str) -> Result<Vec<RawResult>, String> {
                Ok(Vec::new())
            }
        }
        let engine = HttpSearchEngine::new(Box::new(ScriptedBackend));
        let first = engine
            .run(&SearchRequest {
                id: Some("sess-1".into()),
                commands: Some(SearchCommands {
                    search_query: Some(vec![json!({"q": "rust"})]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(first.results.len(), 1);
        assert_eq!(first.results[0].ref_id, "turn1search0");
        assert_eq!(first.results[0].url.as_deref(), Some("https://example.com"));

        let second = engine
            .run(&SearchRequest {
                id: Some("sess-1".into()),
                commands: Some(SearchCommands {
                    open: Some(vec![json!({"ref_id": "turn1search0"})]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            second.output.contains("https://example.com"),
            "open must resolve the ref: {}",
            second.output
        );
        let foreign = engine
            .run(&SearchRequest {
                id: Some("sess-2".into()),
                commands: Some(SearchCommands {
                    open: Some(vec![json!({"ref_id": "turn1search0"})]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            !foreign.output.contains("https://example.com"),
            "ref ids must be session-scoped: {}",
            foreign.output
        );
    }

    #[tokio::test]
    async fn open_click_and_find_fetch_and_reuse_session_pages() {
        use axum::{routing::get, Router};

        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    "<html><title>Root</title><body>alpha needle<a href='/next'>Next page</a></body></html>"
                }),
            )
            .route(
                "/next",
                get(|| async { "<html><title>Next</title><body>clicked content</body></html>" }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        struct LocalBackend(String);
        #[async_trait]
        impl SearchBackend for LocalBackend {
            fn kind(&self) -> &'static str {
                "local-test"
            }

            async fn text_search(&self, _query: &str) -> Result<Vec<RawResult>, String> {
                Ok(vec![RawResult {
                    kind: "text_result".into(),
                    url: Some(self.0.clone()),
                    title: Some("Root".into()),
                    snippet: Some("alpha".into()),
                    image_url: None,
                }])
            }

            async fn image_search(&self, _query: &str) -> Result<Vec<RawResult>, String> {
                Ok(Vec::new())
            }
        }

        let engine = HttpSearchEngine::new(Box::new(LocalBackend(format!("http://{address}/"))));
        engine
            .run(&SearchRequest {
                id: Some("session".into()),
                commands: Some(SearchCommands {
                    search_query: Some(vec![serde_json::json!({"q": "root"})]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        let opened = engine
            .run(&SearchRequest {
                id: Some("session".into()),
                commands: Some(SearchCommands {
                    open: Some(vec![serde_json::json!({"ref_id": "turn1search0"})]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(opened.output.contains("alpha needle"));
        assert!(opened.output.contains("1. [Next page]"));

        let found = engine
            .run(&SearchRequest {
                id: Some("session".into()),
                commands: Some(SearchCommands {
                    find: Some(vec![serde_json::json!({
                        "ref_id": "turn1search0",
                        "pattern": "needle"
                    })]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(found.output.contains("needle"));

        let clicked = engine
            .run(&SearchRequest {
                id: Some("session".into()),
                commands: Some(SearchCommands {
                    click: Some(vec![serde_json::json!({"ref_id": "turn1search0", "id": 1})]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(clicked.output.contains("clicked content"));
    }

    #[test]
    fn ref_store_honors_retention_age() {
        let mut refs = RefStore::new();
        refs.record(vec![RawResult {
            kind: "text_result".into(),
            url: Some("https://example.com".into()),
            title: None,
            snippet: None,
            image_url: None,
        }]);
        refs.last_used = Instant::now() - Duration::from_secs(10);
        refs.prune(Duration::from_secs(1));
        assert!(refs.entries.is_empty());
    }
}
