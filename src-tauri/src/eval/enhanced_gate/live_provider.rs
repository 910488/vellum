//! A recording pass-through to a real provider.
//!
//! The scripted provider answers from a table, which is what makes the bridge
//! gate reproducible — and also what keeps it a statement about the runtime
//! rather than about any model. This one keeps every observation the scripted
//! provider makes and gets the answers from a real endpoint instead, so the
//! Enhanced ports run their real logic over real model output: real tool calls,
//! real truncation, real context-overflow errors in the server's own words.
//!
//! Two jobs beyond forwarding.
//!
//! It translates. The gate child speaks an abbreviated Responses dialect —
//! `{"type":"message","role":"user","text":...}` — which its own scripted
//! server accepts and a real one rejects outright, refusing the whole request
//! because it cannot determine the type of the item. In production that
//! translation is Vellum's adapter; here it is these few lines, kept
//! deliberately small and in one place.
//!
//! It declares the tools. The scripted provider never needed a `tools` array
//! because it decided the calls itself. A real model cannot call a tool it has
//! not been told about, so the child's two tools are declared on every request.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use super::provider::{
    contains_any_key, ProviderObservations, REMOTE_COMPACTION_FIELDS, THREAD_HEADER,
};

/// What the child can call. Mirrors `child.rs`'s tool implementations; a real
/// model needs the schema before it can produce a call at all.
fn child_tools() -> Value {
    json!([
        {
            "type": "function",
            "name": "workspace_append",
            "description": "Append one line to the workspace side-effect log.",
            "parameters": {
                "type": "object",
                "properties": {"line": {"type": "string", "description": "The line to append."}},
                "required": ["line"]
            }
        },
        {
            "type": "function",
            "name": "read_large",
            "description": "Read a large blob of text of the requested size in bytes.",
            "parameters": {
                "type": "object",
                "properties": {"bytes": {"type": "integer", "description": "How many bytes to read."}},
                "required": ["bytes"]
            }
        }
    ])
}

#[derive(Debug, Clone)]
pub struct LiveProviderConfig {
    /// Base URL of the real endpoint, without the `/v1/responses` path.
    pub base_url: String,
    /// The upstream model id. Catalog ids are Vellum's routing keys and mean
    /// nothing to the provider, so every request is rewritten to this.
    pub upstream_model: String,
    /// Output caps by catalog model. A cap forces real truncation, which is
    /// how a real provider says "unfinished" — the signal bounded continuation
    /// exists to handle. Per model, because only one case wants to be cut off.
    pub max_output_tokens: BTreeMap<String, u64>,
    pub request_timeout: Duration,
}

/// One real exchange, kept so the report can quote the endpoint rather than
/// paraphrase it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveExchange {
    pub catalog_model: String,
    pub thread: String,
    pub input_items: u64,
    pub input_chars: u64,
    pub upstream_status: u16,
    pub response_status: String,
    pub output_types: Vec<String>,
    pub tool_calls: Vec<String>,
    /// What the endpoint says it generated. Compared against a requested cap,
    /// this is the only way to tell a truncated answer from a finished one when
    /// a server reports both as `completed`.
    pub output_tokens: Option<u64>,
    pub elapsed_ms: u64,
    /// Present only when the endpoint refused. Its own words, not ours.
    pub error_message: Option<String>,
}

pub struct LiveProvider {
    config: LiveProviderConfig,
    client: reqwest::Client,
    observations: Mutex<ProviderObservations>,
    exchanges: Mutex<Vec<LiveExchange>>,
    served: AtomicU64,
}

impl LiveProvider {
    fn new(config: LiveProviderConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .unwrap_or_default();
        Self {
            config,
            client,
            observations: Mutex::new(ProviderObservations::default()),
            exchanges: Mutex::new(Vec::new()),
            served: AtomicU64::new(0),
        }
    }

    pub fn observations(&self) -> ProviderObservations {
        let guard = self.observations.lock().expect("live provider poisoned");
        ProviderObservations {
            requests_by_model: guard.requests_by_model.clone(),
            requests_by_thread: guard.requests_by_thread.clone(),
            remote_compaction_requests: guard.remote_compaction_requests,
            max_surface_items: guard.max_surface_items.clone(),
            surface_chars_by_request: guard.surface_chars_by_request.clone(),
        }
    }

    pub fn exchanges(&self) -> Vec<LiveExchange> {
        self.exchanges
            .lock()
            .expect("live provider poisoned")
            .clone()
    }

    pub fn served(&self) -> u64 {
        self.served.load(Ordering::SeqCst)
    }

    /// Exchanges for one catalog model, in the order they happened.
    pub fn exchanges_for_model(&self, model: &str) -> Vec<LiveExchange> {
        self.exchanges()
            .into_iter()
            .filter(|exchange| exchange.catalog_model == model)
            .collect()
    }

    /// Identical to the scripted provider's, so a hard gate proven on the
    /// script means the same thing here.
    fn observe(&self, model: &str, thread: &str, body: &Value) {
        let mut guard = self.observations.lock().expect("live provider poisoned");
        *guard
            .requests_by_model
            .entry(model.to_string())
            .or_insert(0) += 1;
        *guard
            .requests_by_thread
            .entry(thread.to_string())
            .or_insert(0) += 1;
        if contains_any_key(body, REMOTE_COMPACTION_FIELDS) {
            guard.remote_compaction_requests += 1;
        }
        let items = body
            .get("input")
            .and_then(Value::as_array)
            .map(|items| items.len() as u64)
            .unwrap_or(0);
        let entry = guard
            .max_surface_items
            .entry(thread.to_string())
            .or_insert(0);
        *entry = (*entry).max(items);
        let chars = serde_json::to_string(body.get("input").unwrap_or(&Value::Null))
            .map(|text| text.len() as u64)
            .unwrap_or(0);
        guard
            .surface_chars_by_request
            .entry(thread.to_string())
            .or_default()
            .push(chars);
    }
}

/// The child's dialect into canonical Responses input items.
///
/// `control` items are dropped: they are the gate's own bookkeeping and have no
/// meaning to a provider, which refuses the entire request over one item it
/// cannot type. Everything else is either renamed or passed through unchanged,
/// because it is already canonical.
pub fn canonical_input(items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .filter_map(|item| match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                // `text` is the child's shorthand; a real endpoint wants
                // `content` and refuses the item outright without it.
                let content = item
                    .get("content")
                    .cloned()
                    .or_else(|| item.get("text").cloned())
                    .unwrap_or_else(|| Value::String(String::new()));
                Some(json!({"type": "message", "role": role, "content": content}))
            }
            Some("control") => None,
            Some(_) => Some(item.clone()),
            None => None,
        })
        .collect()
}

fn upstream_body(config: &LiveProviderConfig, catalog_model: &str, body: &Value) -> Value {
    let items = body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut upstream = json!({
        "model": config.upstream_model,
        "stream": false,
        "input": canonical_input(&items),
        "tools": child_tools(),
    });
    if let Some(limit) = config.max_output_tokens.get(catalog_model) {
        upstream["max_output_tokens"] = json!(limit);
    }
    upstream
}

/// Provider error types the runtime already knows, from the ones a given server
/// happens to use.
///
/// This is the one place the pass-through is allowed to rewrite a response, and
/// it rewrites exactly one field. The runtime acts on `context_length_exceeded`
/// and llama.cpp says `exceed_context_size_error`; without the mapping the
/// overflow port never fires and the run would report that the recovery path is
/// broken when what is actually different is the spelling. In production this
/// mapping is Vellum's adapter.
///
/// The message is never touched. "Preserves the provider error" is only worth
/// asserting if the words in the report are the server's own.
fn canonical_error_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "exceed_context_size_error" | "context_length_exceeded" | "context_size_exceeded_error" => {
            Some("context_length_exceeded")
        }
        _ => None,
    }
}

fn normalize_error(mut value: Value) -> Value {
    let Some(kind) = value
        .pointer("/error/type")
        .and_then(Value::as_str)
        .and_then(canonical_error_kind)
    else {
        return value;
    };
    if let Some(error) = value.get_mut("error").and_then(Value::as_object_mut) {
        // Keep what the server said, under the name the runtime acts on.
        error.insert("upstreamType".into(), error["type"].clone());
        error.insert("type".into(), Value::String(kind.to_string()));
    }
    value
}

struct Described {
    status: String,
    output_types: Vec<String>,
    tool_calls: Vec<String>,
    output_tokens: Option<u64>,
    error_message: Option<String>,
}

fn describe(response: &Value) -> Described {
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("provider error")
            .to_string();
        return Described {
            status: "error".into(),
            output_types: Vec::new(),
            tool_calls: Vec::new(),
            output_tokens: None,
            error_message: Some(message),
        };
    }
    let status = response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let types = output
        .iter()
        .filter_map(|item| item.get("type").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    let calls = output
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        .filter_map(|item| item.get("call_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    Described {
        status,
        output_types: types,
        tool_calls: calls,
        output_tokens: response
            .pointer("/usage/output_tokens")
            .and_then(Value::as_u64),
        error_message: None,
    }
}

pub struct RunningLiveProvider {
    pub base_url: String,
    pub state: Arc<LiveProvider>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl RunningLiveProvider {
    pub async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

pub async fn spawn(config: LiveProviderConfig) -> std::io::Result<RunningLiveProvider> {
    let state = Arc::new(LiveProvider::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let app = Router::new()
        .route("/v1/responses", post(responses))
        .with_state(Arc::clone(&state));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await;
    });
    Ok(RunningLiveProvider {
        base_url: format!("http://{address}"),
        state,
        shutdown: Some(shutdown_tx),
    })
}

async fn responses(
    State(state): State<Arc<LiveProvider>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    let catalog_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let thread = headers
        .get(THREAD_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    state.observe(&catalog_model, &thread, &body);
    state.served.fetch_add(1, Ordering::SeqCst);

    let input_items = body
        .get("input")
        .and_then(Value::as_array)
        .map(|items| items.len() as u64)
        .unwrap_or(0);
    let input_chars = serde_json::to_string(body.get("input").unwrap_or(&Value::Null))
        .map(|text| text.len() as u64)
        .unwrap_or(0);

    let url = format!(
        "{}/v1/responses",
        state.config.base_url.trim_end_matches('/')
    );
    let started = Instant::now();
    let sent = state
        .client
        .post(&url)
        .json(&upstream_body(&state.config, &catalog_model, &body))
        .send()
        .await;

    let (status, value) = match sent {
        Ok(response) => {
            let status = response.status();
            let value = response.json::<Value>().await.unwrap_or_else(
                |error| json!({"error": {"type": "provider_error", "message": error.to_string()}}),
            );
            (status, normalize_error(value))
        }
        Err(error) => (
            axum::http::StatusCode::BAD_GATEWAY,
            // Reaching the endpoint is the operator's problem, not the
            // runtime's, so name which one this is.
            json!({"error": {
                "type": "provider_unreachable",
                "message": format!("cannot reach {url}: {error}")
            }}),
        ),
    };

    let described = describe(&value);
    state
        .exchanges
        .lock()
        .expect("live provider poisoned")
        .push(LiveExchange {
            catalog_model,
            thread,
            input_items,
            input_chars,
            upstream_status: status.as_u16(),
            response_status: described.status,
            output_types: described.output_types,
            tool_calls: described.tool_calls,
            output_tokens: described.output_tokens,
            elapsed_ms: started.elapsed().as_millis() as u64,
            error_message: described.error_message,
        });

    // The child reads `error`, `status` and `output`; pass the endpoint's own
    // body through untouched so it reads exactly what the model server said.
    (axum::http::StatusCode::OK, Json(value))
}

/// How many times each call id was asked for, across the given exchanges.
pub fn tool_call_counts(exchanges: &[LiveExchange]) -> BTreeMap<String, u64> {
    let mut counts = BTreeMap::new();
    for exchange in exchanges {
        for call in &exchange.tool_calls {
            *counts.entry(call.clone()).or_insert(0) += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_child_shorthand_becomes_a_canonical_message_item() {
        let items = vec![json!({"type": "message", "role": "user", "text": "hello"})];
        let canonical = canonical_input(&items);
        assert_eq!(
            canonical,
            vec![json!({"type": "message", "role": "user", "content": "hello"})]
        );
    }

    /// A real endpoint refuses the whole request over a single item it cannot
    /// type, so the gate's own bookkeeping must not reach it.
    #[test]
    fn control_items_are_dropped_and_tool_items_pass_through_unchanged() {
        let call = json!({
            "type": "function_call",
            "call_id": "c1",
            "name": "read_large",
            "arguments": "{}"
        });
        let output = json!({"type": "function_call_output", "call_id": "c1", "output": "xxx"});
        let items = vec![
            json!({"type": "control", "key": "k", "value": 1}),
            call.clone(),
            output.clone(),
        ];
        assert_eq!(canonical_input(&items), vec![call, output]);
    }

    #[test]
    fn an_already_canonical_message_keeps_its_content() {
        let items = vec![json!({"type": "message", "role": "user", "content": "kept"})];
        assert_eq!(
            canonical_input(&items),
            vec![json!({"type": "message", "role": "user", "content": "kept"})]
        );
    }

    /// Catalog ids are Vellum's routing keys. Sending one upstream asks a real
    /// endpoint for a model that does not exist there.
    #[test]
    fn the_catalog_id_is_replaced_by_the_upstream_model_and_tools_are_declared() {
        let config = LiveProviderConfig {
            base_url: "http://example.invalid".into(),
            upstream_model: "qwen".into(),
            max_output_tokens: BTreeMap::from([("gate-qwen-tools".to_string(), 64)]),
            request_timeout: Duration::from_secs(1),
        };
        let body = json!({
            "model": "gate-qwen-tools",
            "input": [{"type": "message", "role": "user", "text": "hi"}]
        });
        let upstream = upstream_body(&config, "gate-qwen-tools", &body);
        assert_eq!(upstream["model"], json!("qwen"));
        assert_eq!(upstream["max_output_tokens"], json!(64));
        assert_eq!(upstream["tools"].as_array().unwrap().len(), 2);
        assert_eq!(upstream["input"][0]["content"], json!("hi"));
    }

    #[test]
    fn a_refusal_is_described_by_the_endpoints_own_message() {
        let value =
            json!({"error": {"type": "invalid_request_error", "message": "context too long"}});
        let described = describe(&value);
        assert_eq!(described.status, "error");
        assert!(described.output_types.is_empty() && described.tool_calls.is_empty());
        assert_eq!(described.error_message.as_deref(), Some("context too long"));
    }

    #[test]
    fn a_completion_reports_its_output_types_and_call_ids() {
        let value = json!({
            "status": "completed",
            "usage": {"output_tokens": 31},
            "output": [
                {"type": "reasoning"},
                {"type": "function_call", "call_id": "call-1", "name": "read_large"}
            ]
        });
        let described = describe(&value);
        assert_eq!(described.status, "completed");
        assert_eq!(
            described.output_types,
            vec!["reasoning".to_string(), "function_call".into()]
        );
        assert_eq!(described.tool_calls, vec!["call-1".to_string()]);
        assert_eq!(described.output_tokens, Some(31));
        assert!(described.error_message.is_none());
    }

    /// The runtime acts on `context_length_exceeded`; llama.cpp says
    /// `exceed_context_size_error`. Without the mapping the overflow port never
    /// fires and the run blames the recovery path for a spelling difference.
    #[test]
    fn a_real_overflow_is_renamed_but_its_words_are_left_alone() {
        let raw = json!({"error": {
            "code": 400,
            "type": "exceed_context_size_error",
            "message": "request (104052 tokens) exceeds the available context size (88064 tokens), try increasing it",
            "n_prompt_tokens": 104052,
            "n_ctx": 88064
        }});
        let normalized = normalize_error(raw);
        assert_eq!(
            normalized["error"]["type"],
            json!("context_length_exceeded")
        );
        assert_eq!(
            normalized["error"]["upstreamType"],
            json!("exceed_context_size_error")
        );
        assert!(normalized["error"]["message"]
            .as_str()
            .unwrap()
            .contains("104052 tokens"));
        // Everything the server sent is still there to quote.
        assert_eq!(normalized["error"]["n_ctx"], json!(88064));
    }

    #[test]
    fn an_error_the_runtime_does_not_act_on_is_passed_through_untouched() {
        let raw = json!({"error": {"type": "invalid_request_error", "message": "bad tool schema"}});
        assert_eq!(normalize_error(raw.clone()), raw);
    }

    #[test]
    fn repeated_call_ids_are_counted_per_id() {
        let exchange = |calls: Vec<&str>| LiveExchange {
            catalog_model: "m".into(),
            thread: "t".into(),
            input_items: 1,
            input_chars: 10,
            upstream_status: 200,
            response_status: "completed".into(),
            output_types: vec!["function_call".into()],
            tool_calls: calls.into_iter().map(str::to_string).collect(),
            output_tokens: Some(1),
            elapsed_ms: 1,
            error_message: None,
        };
        let counts = tool_call_counts(&[exchange(vec!["a", "b"]), exchange(vec!["a"])]);
        assert_eq!(counts.get("a"), Some(&2));
        assert_eq!(counts.get("b"), Some(&1));
    }
}
