//! Third-party server-side `web_search` loop.
//!
//! Codex cannot execute a `function_call{name: web_search}` — that name is
//! not in its dynamic tool registry. Official Responses treats search as a
//! built-in `web_search_call` item. Vellum therefore intercepts the function
//! call on third-party routes, runs Brave, emits `web_search_call` lifecycle
//! events for the UI, and feeds a bounded `function_call_output` back to the
//! same provider. Official stays native passthrough and never enters this
//! loop.

use crate::search::{SearchCommands, SearchEngine, SearchRequest, SearchResponse, SearchResult};
use crate::streaming::{format_sse_value, parse_sse_value};
use serde_json::{json, Value};

/// Default per-turn cap on server-side search hops so a model that keeps
/// calling `web_search` cannot spin the loop forever. Configurable per
/// [`SearchLoopSession::new`] caller (`ProxyRuntime::with_search_tool_loop_limit`);
/// this is only the fallback when nothing overrides it.
pub const DEFAULT_SEARCH_TOOL_LOOP_LIMIT: u8 = 3;
const MAX_RESULTS: usize = 8;
const MAX_SNIPPET_CHARS: usize = 280;
const MAX_TITLE_CHARS: usize = 300;
const MAX_URL_CHARS: usize = 1_024;
const MAX_OUTPUT_BYTES: usize = 4_000;

/// One intercepted `web_search` function call waiting for Brave.
#[derive(Debug, Clone)]
pub struct PendingSearch {
    pub call_id: String,
    pub output_index: usize,
    pub item_id: String,
    pub arguments: String,
    pub query: String,
    pub function_item: Value,
}

/// Observes a third-party Responses stream and rewrites `web_search`
/// function-call events so Codex never tries to execute them.
#[derive(Debug)]
pub struct SearchLoopSession {
    enabled: bool,
    pending: Vec<PendingSearch>,
    loops: u8,
    loop_limit: u8,
}

impl Default for SearchLoopSession {
    fn default() -> Self {
        Self::new(false, DEFAULT_SEARCH_TOOL_LOOP_LIMIT)
    }
}

/// How the stream layer should treat one normalized SSE block.
#[derive(Debug, Clone, PartialEq)]
pub enum SseAction {
    /// Forward the original block unchanged.
    Forward,
    /// Drop the block (arguments deltas, original function_call done).
    Drop,
    /// Replace the block with these already-formatted SSE blocks.
    Replace(Vec<String>),
    /// `response.completed` arrived while searches still need to run.
    HoldCompleted,
}

impl SearchLoopSession {
    pub fn new(enabled: bool, loop_limit: u8) -> Self {
        Self {
            enabled,
            pending: Vec::new(),
            loops: 0,
            loop_limit: loop_limit.max(1),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn pending(&self) -> &[PendingSearch] {
        &self.pending
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn loops(&self) -> u8 {
        self.loops
    }

    pub fn can_loop(&self) -> bool {
        self.enabled && self.has_pending() && self.loops < self.loop_limit
    }

    /// True once the model has pending `web_search` calls but the turn has
    /// already used up its `tool_loop_limit` hops. Distinct from
    /// `!can_loop()` in general (which is also true once `enabled` is
    /// false or there's simply nothing pending) so callers can tell "capped
    /// out" apart from "nothing to do" and surface a `tool_loop_limit`
    /// error instead of a generic stream-truncation message.
    pub fn loop_limit_exceeded(&self) -> bool {
        self.enabled && self.has_pending() && self.loops >= self.loop_limit
    }

    pub fn loop_limit(&self) -> u8 {
        self.loop_limit
    }

    pub fn mark_looped(&mut self) {
        self.loops = self.loops.saturating_add(1);
        self.pending.clear();
    }

    pub fn observe(&mut self, block: &str) -> SseAction {
        if !self.enabled {
            return SseAction::Forward;
        }
        let Some((event, value)) = parse_sse_value(block) else {
            return SseAction::Forward;
        };
        match event.as_str() {
            "response.output_item.added" => self.on_item_added(&value),
            "response.function_call_arguments.delta" => self.on_arguments_delta(&value),
            "response.function_call_arguments.done" => self.on_arguments_done(&value),
            "response.output_item.done" => self.on_item_done(&value),
            "response.created" | "response.in_progress" if self.loops > 0 => SseAction::Drop,
            "response.completed" if self.has_pending() => SseAction::HoldCompleted,
            _ => SseAction::Forward,
        }
    }

    fn on_item_added(&mut self, value: &Value) -> SseAction {
        let item = match value.get("item") {
            Some(item) => item,
            None => return SseAction::Forward,
        };
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return SseAction::Forward;
        }
        if item.get("name").and_then(Value::as_str) != Some("web_search") {
            return SseAction::Forward;
        }
        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or("call_web_search")
            .to_string();
        let output_index = value
            .get("output_index")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let item_id = format!("ws_{call_id}");
        let arguments = item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let query = extract_search_query(&arguments).unwrap_or_default();
        self.pending.push(PendingSearch {
            call_id: call_id.clone(),
            output_index,
            item_id: item_id.clone(),
            arguments,
            query: query.clone(),
            function_item: item.clone(),
        });
        let mut added = value.clone();
        added["item"] = web_search_call_item(&item_id, "in_progress", &query);
        SseAction::Replace(vec![
            format_sse_value("response.output_item.added", added),
            format_sse_value(
                "response.web_search_call.in_progress",
                json!({
                    "type": "response.web_search_call.in_progress",
                    "output_index": output_index,
                    "item_id": item_id,
                }),
            ),
        ])
    }

    fn tracked_call_id<'a>(&self, value: &'a Value) -> Option<&'a str> {
        let id = value
            .get("item_id")
            .and_then(Value::as_str)
            .or_else(|| value.pointer("/item/call_id").and_then(Value::as_str))
            .or_else(|| value.pointer("/item/id").and_then(Value::as_str))?;
        self.pending
            .iter()
            .any(|pending| {
                pending.call_id == id
                    || pending.item_id == id
                    || pending.function_item.get("id").and_then(Value::as_str) == Some(id)
            })
            .then_some(id)
    }

    fn pending_mut_for(&mut self, value: &Value) -> Option<&mut PendingSearch> {
        let id = value
            .get("item_id")
            .and_then(Value::as_str)
            .or_else(|| value.pointer("/item/call_id").and_then(Value::as_str))
            .or_else(|| value.pointer("/item/id").and_then(Value::as_str))?
            .to_string();
        self.pending.iter_mut().find(|pending| {
            pending.call_id == id
                || pending.item_id == id
                || pending.function_item.get("id").and_then(Value::as_str) == Some(id.as_str())
        })
    }

    fn on_arguments_delta(&mut self, value: &Value) -> SseAction {
        let Some(pending) = self.pending_mut_for(value) else {
            return SseAction::Forward;
        };
        if let Some(delta) = value.get("delta").and_then(Value::as_str) {
            pending.arguments.push_str(delta);
            if let Some(query) = extract_search_query(&pending.arguments) {
                pending.query = query;
            }
        }
        SseAction::Drop
    }

    fn on_arguments_done(&mut self, value: &Value) -> SseAction {
        let Some(pending) = self.pending_mut_for(value) else {
            return SseAction::Forward;
        };
        if let Some(arguments) = value.get("arguments").and_then(Value::as_str) {
            if !arguments.is_empty() {
                pending.arguments = arguments.to_string();
            }
        }
        if let Some(query) = extract_search_query(&pending.arguments) {
            pending.query = query;
        }
        pending.function_item["arguments"] = json!(pending.arguments.clone());
        SseAction::Drop
    }

    fn on_item_done(&mut self, value: &Value) -> SseAction {
        if self.tracked_call_id(value).is_none() {
            return SseAction::Forward;
        }
        // Hold the done event until Brave finishes so the UI item stays
        // in_progress through the search.
        if let Some(item) = value.get("item") {
            if let Some(pending) = self.pending_mut_for(value) {
                if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                    if !arguments.is_empty() {
                        pending.arguments = arguments.to_string();
                        pending.function_item["arguments"] = json!(arguments);
                    }
                }
                if let Some(query) = extract_search_query(&pending.arguments) {
                    pending.query = query;
                }
            }
        }
        SseAction::Drop
    }
}

pub fn extract_search_query(arguments: &str) -> Option<String> {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        for key in ["query", "q", "search_query", "text"] {
            if let Some(query) = value.get(key).and_then(Value::as_str) {
                let query = query.trim();
                if !query.is_empty() {
                    return Some(query.to_string());
                }
            }
        }
    }
    None
}

pub fn search_request_for_query(query: &str) -> SearchRequest {
    SearchRequest {
        commands: Some(SearchCommands {
            search_query: Some(vec![json!({ "q": query })]),
            ..SearchCommands::default()
        }),
        ..SearchRequest::default()
    }
}

pub fn bounded_search_output(response: &SearchResponse) -> String {
    let mut results: Vec<Value> = response
        .results
        .iter()
        .take(MAX_RESULTS)
        .map(bounded_result)
        .collect();

    let mut truncated = response.results.len() > results.len();
    loop {
        let payload = json!({
            "results": results,
            "encrypted_output": serde_json::Value::Null,
            "truncated": truncated,
        })
        .to_string();
        if payload.len() <= MAX_OUTPUT_BYTES {
            return payload;
        }

        truncated = true;
        if results.len() > 1 {
            results.pop();
            continue;
        }

        // One unusually long result (most commonly a signed URL) can still
        // exceed the byte budget even after the ordinary per-field caps.
        // Rebound that last result more aggressively and serialize again.
        // This deliberately never truncates the serialized String itself:
        // byte-truncating JSON can split a UTF-8 scalar and panic, or leave a
        // syntactically invalid function_call_output for the provider.
        if let Some(result) = results.first_mut() {
            bound_json_string(result, "url", 256);
            bound_json_string(result, "title", 160);
            bound_json_string(result, "snippet", 160);
        } else {
            // The fixed envelope is far below MAX_OUTPUT_BYTES. Keeping this
            // branch explicit makes the termination argument obvious if the
            // envelope gains fields later.
            return json!({
                "results": [],
                "encrypted_output": serde_json::Value::Null,
                "truncated": true,
            })
            .to_string();
        }
    }
}

fn bounded_result(result: &SearchResult) -> Value {
    json!({
        "url": result.url.as_deref().map(|value| take_chars(value, MAX_URL_CHARS)),
        "title": result.title.as_deref().map(|value| take_chars(value, MAX_TITLE_CHARS)),
        "snippet": result.snippet.as_deref().map(|value| take_chars(value, MAX_SNIPPET_CHARS)),
    })
}

fn take_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn bound_json_string(value: &mut Value, key: &str, limit: usize) {
    let Some(text) = value.get(key).and_then(Value::as_str) else {
        return;
    };
    value[key] = json!(take_chars(text, limit));
}

fn web_search_call_item(item_id: &str, status: &str, query: &str) -> Value {
    json!({
        "type": "web_search_call",
        "id": item_id,
        "status": status,
        "action": {
            "type": "search",
            "query": query,
        }
    })
}

/// Lifecycle events after Brave returns. Codex sees a completed
/// `web_search_call`, never a `function_call` it would try to execute.
pub fn completed_search_events(pending: &PendingSearch) -> Vec<String> {
    let item = web_search_call_item(&pending.item_id, "completed", &pending.query);
    vec![
        format_sse_value(
            "response.web_search_call.searching",
            json!({
                "type": "response.web_search_call.searching",
                "output_index": pending.output_index,
                "item_id": pending.item_id,
            }),
        ),
        format_sse_value(
            "response.web_search_call.completed",
            json!({
                "type": "response.web_search_call.completed",
                "output_index": pending.output_index,
                "item_id": pending.item_id,
            }),
        ),
        format_sse_value(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": pending.output_index,
                "item": item,
            }),
        ),
    ]
}

pub fn function_call_output_item(pending: &PendingSearch, output: &str) -> Value {
    json!({
        "type": "function_call_output",
        "call_id": pending.call_id,
        "output": output,
    })
}

/// Append the original function call (provider-visible) and its output to the
/// next upstream request. Client-visible `web_search_call` items are not
/// forwarded to third-party providers.
pub fn append_search_outputs(
    request_body: &mut Value,
    pending: &[PendingSearch],
    outputs: &[String],
) {
    let input = request_body
        .as_object_mut()
        .map(|object| object.entry("input").or_insert_with(|| json!([])));
    let Some(Value::Array(input)) = input else {
        return;
    };
    for (pending, output) in pending.iter().zip(outputs.iter()) {
        let mut function_item = pending.function_item.clone();
        function_item["type"] = json!("function_call");
        function_item["name"] = json!("web_search");
        function_item["call_id"] = json!(pending.call_id.clone());
        function_item["arguments"] = json!(pending.arguments.clone());
        function_item["status"] = json!("completed");
        input.push(function_item);
        input.push(function_call_output_item(pending, output));
    }
}

pub async fn execute_pending_searches(
    engine: &dyn SearchEngine,
    pending: &[PendingSearch],
) -> Result<Vec<String>, String> {
    let mut outputs = Vec::with_capacity(pending.len());
    for item in pending {
        if item.query.trim().is_empty() {
            return Err("web_search call missing query".into());
        }
        let response = engine.run(&search_request_for_query(&item.query)).await?;
        outputs.push(bounded_search_output(&response));
    }
    Ok(outputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    fn added_block(name: &str, call_id: &str) -> String {
        format_sse_value(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": 1,
                "item": {
                    "type": "function_call",
                    "id": format!("fc_{call_id}"),
                    "call_id": call_id,
                    "name": name,
                    "arguments": "",
                    "status": "in_progress"
                }
            }),
        )
    }

    #[test]
    fn disabled_session_forwards_web_search_function_call() {
        let mut session = SearchLoopSession::new(false, DEFAULT_SEARCH_TOOL_LOOP_LIMIT);
        let block = added_block("web_search", "call_1");
        assert_eq!(session.observe(&block), SseAction::Forward);
        assert!(!session.has_pending());
    }

    #[test]
    fn enabled_session_rewrites_web_search_and_holds_completed() {
        let mut session = SearchLoopSession::new(true, DEFAULT_SEARCH_TOOL_LOOP_LIMIT);
        let added = added_block("web_search", "call_1");
        match session.observe(&added) {
            SseAction::Replace(blocks) => {
                assert!(blocks[0].contains("web_search_call"));
                assert!(!blocks[0].contains("\"type\":\"function_call\""));
                assert!(blocks[1].contains("response.web_search_call.in_progress"));
            }
            other => panic!("expected rewrite, got {other:?}"),
        }
        let delta = format_sse_value(
            "response.function_call_arguments.delta",
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "fc_call_1",
                "output_index": 1,
                "delta": "{\"query\":\"today news\"}"
            }),
        );
        assert_eq!(session.observe(&delta), SseAction::Drop);
        let done = format_sse_value(
            "response.function_call_arguments.done",
            json!({
                "type": "response.function_call_arguments.done",
                "item_id": "fc_call_1",
                "output_index": 1,
                "arguments": "{\"query\":\"today news\"}"
            }),
        );
        assert_eq!(session.observe(&done), SseAction::Drop);
        assert_eq!(session.pending()[0].query, "today news");
        let completed = format_sse_value(
            "response.completed",
            json!({"type": "response.completed", "response": {"status": "completed", "output": []}}),
        );
        assert_eq!(session.observe(&completed), SseAction::HoldCompleted);
    }

    #[test]
    fn other_function_calls_are_not_intercepted() {
        let mut session = SearchLoopSession::new(true, DEFAULT_SEARCH_TOOL_LOOP_LIMIT);
        let block = added_block("shell", "call_shell");
        assert_eq!(session.observe(&block), SseAction::Forward);
        assert!(!session.has_pending());
    }

    #[test]
    fn extract_query_accepts_query_and_q() {
        assert_eq!(
            extract_search_query(r#"{"query":"rust release"}"#).as_deref(),
            Some("rust release")
        );
        assert_eq!(
            extract_search_query(r#"{"q":"today news"}"#).as_deref(),
            Some("today news")
        );
        assert_eq!(extract_search_query("{}"), None);
    }

    struct FixedEngine;

    #[async_trait]
    impl SearchEngine for FixedEngine {
        async fn run(&self, _request: &SearchRequest) -> Result<SearchResponse, String> {
            Ok(SearchResponse {
                encrypted_output: None,
                output: "ok".into(),
                results: vec![SearchResult {
                    kind: "page".into(),
                    ref_id: "turn1search0".into(),
                    url: Some("https://example.test/news".into()),
                    title: Some("News".into()),
                    snippet: Some("hello".into()),
                    image_url: None,
                    width: None,
                    height: None,
                    pageno: None,
                }],
            })
        }
    }

    #[tokio::test]
    async fn execute_pending_searches_returns_bounded_json() {
        let pending = PendingSearch {
            call_id: "call_1".into(),
            output_index: 1,
            item_id: "ws_call_1".into(),
            arguments: r#"{"query":"today"}"#.into(),
            query: "today".into(),
            function_item: json!({"type":"function_call","name":"web_search","call_id":"call_1"}),
        };
        let outputs = execute_pending_searches(&FixedEngine, std::slice::from_ref(&pending))
            .await
            .unwrap();
        assert!(outputs[0].contains("https://example.test/news"));
        assert!(outputs[0].contains("\"encrypted_output\":null"));
        let events = completed_search_events(&pending);
        assert!(events
            .iter()
            .any(|event| event.contains("web_search_call.completed")));
        let mut body = json!({"input":[{"type":"message","role":"user","content":"q"}]});
        append_search_outputs(&mut body, &[pending], &outputs);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["name"], "web_search");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_1");
    }

    #[test]
    fn bounded_output_is_valid_utf8_json_for_multibyte_results_over_four_kib() {
        let response = SearchResponse {
            encrypted_output: None,
            output: "ignored".into(),
            results: (0..8)
                .map(|index| SearchResult {
                    kind: "page".into(),
                    ref_id: format!("turn1search{index}"),
                    url: Some(format!(
                        "https://example.test/{index}/{}",
                        "新聞".repeat(900)
                    )),
                    title: Some("今日國際新聞與市場摘要".repeat(200)),
                    snippet: Some("這是一段包含多位元中文字元的搜尋摘要。".repeat(300)),
                    image_url: None,
                    width: None,
                    height: None,
                    pageno: None,
                })
                .collect(),
        };

        let output = bounded_search_output(&response);
        assert!(output.len() <= MAX_OUTPUT_BYTES);
        let value: Value =
            serde_json::from_str(&output).expect("bounded output must stay valid JSON");
        assert_eq!(value["truncated"], true);
        assert!(value["results"]
            .as_array()
            .is_some_and(|results| !results.is_empty()));
    }

    #[test]
    fn bounded_output_marks_result_count_truncation_even_when_bytes_fit() {
        let response = SearchResponse {
            encrypted_output: None,
            output: String::new(),
            results: (0..(MAX_RESULTS + 1))
                .map(|index| SearchResult {
                    kind: "page".into(),
                    ref_id: format!("turn1search{index}"),
                    url: Some(format!("https://example.test/{index}")),
                    title: Some("News".into()),
                    snippet: None,
                    image_url: None,
                    width: None,
                    height: None,
                    pageno: None,
                })
                .collect(),
        };

        let value: Value = serde_json::from_str(&bounded_search_output(&response)).unwrap();
        assert_eq!(value["truncated"], true);
        assert_eq!(value["results"].as_array().unwrap().len(), MAX_RESULTS);
    }
}
