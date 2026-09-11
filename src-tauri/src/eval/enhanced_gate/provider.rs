//! A scripted local Responses provider.
//!
//! The gate needs provider behaviour that is reproducible to the byte: the same
//! duplicated tool-call id, the same overflow, the same unfinished signal, every
//! run. It also needs to be able to say what the runtime sent it — most
//! importantly that a third-party turn never carried a remote compaction
//! trigger. So the gate runs a real HTTP server on loopback and the runtime
//! talks to it over the wire like any other provider.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use super::scenarios::{GateCatalog, GateScenario, SLOW_RESPONSE_MILLIS};

/// Header the runtime uses to tell the provider which thread it is serving.
/// A real provider would not need it; the script does, to stay deterministic
/// per conversation rather than per process.
pub const THREAD_HEADER: &str = "x-vellum-gate-thread";

/// Fields that would mean Vellum still owned compaction. Seeing any of them on
/// a third-party request is a hard gate failure, not a warning.
pub(crate) const REMOTE_COMPACTION_FIELDS: &[&str] = &[
    "compaction_trigger",
    "compactionTrigger",
    "canonical_checkpoint",
    "vellum_compaction",
];

#[derive(Debug, Default)]
pub struct ProviderObservations {
    pub requests_by_model: BTreeMap<String, u64>,
    pub requests_by_thread: BTreeMap<String, u64>,
    pub remote_compaction_requests: u64,
    pub max_surface_items: BTreeMap<String, u64>,
    pub surface_chars_by_request: BTreeMap<String, Vec<u64>>,
}

pub struct ScriptedProvider {
    catalog: GateCatalog,
    observations: Mutex<ProviderObservations>,
    thread_requests: Mutex<BTreeMap<String, u64>>,
    responses_served: AtomicU64,
}

impl ScriptedProvider {
    pub fn new(catalog: GateCatalog) -> Self {
        Self {
            catalog,
            observations: Mutex::new(ProviderObservations::default()),
            thread_requests: Mutex::new(BTreeMap::new()),
            responses_served: AtomicU64::new(0),
        }
    }

    pub fn observations(&self) -> ProviderObservations {
        let guard = self.observations.lock().expect("provider state poisoned");
        ProviderObservations {
            requests_by_model: guard.requests_by_model.clone(),
            requests_by_thread: guard.requests_by_thread.clone(),
            remote_compaction_requests: guard.remote_compaction_requests,
            max_surface_items: guard.max_surface_items.clone(),
            surface_chars_by_request: guard.surface_chars_by_request.clone(),
        }
    }

    pub fn responses_served(&self) -> u64 {
        self.responses_served.load(Ordering::SeqCst)
    }

    fn next_index(&self, thread: &str) -> u64 {
        let mut guard = self
            .thread_requests
            .lock()
            .expect("provider state poisoned");
        let entry = guard.entry(thread.to_string()).or_insert(0);
        let index = *entry;
        *entry += 1;
        index
    }

    fn observe(&self, model: &str, thread: &str, body: &Value) {
        let mut guard = self.observations.lock().expect("provider state poisoned");
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

    /// The whole script, as one pure function of (model, request index).
    fn script(&self, model: &str, index: u64) -> ScriptedReply {
        let Some(entry) = self.catalog.get(model) else {
            return ScriptedReply::Error {
                status: 400,
                kind: "model_not_found".into(),
                message: format!("model {model} is not in the gate catalog"),
            };
        };
        match entry.scenario {
            GateScenario::PlainAnswer => ScriptedReply::Completed {
                output: vec![assistant_message("done")],
            },
            GateScenario::DuplicateToolCall => match index {
                // The same call id twice, from two separate provider turns.
                // A runtime without the tool reliability port runs the side
                // effect twice.
                0 | 1 => ScriptedReply::Completed {
                    output: vec![function_call(
                        "call-duplicate",
                        "workspace_append",
                        json!({"line": "side-effect"}),
                    )],
                },
                _ => ScriptedReply::Completed {
                    output: vec![assistant_message("done")],
                },
            },
            GateScenario::LargeToolResult => match index {
                0 => ScriptedReply::Completed {
                    output: vec![function_call(
                        "call-large",
                        "read_large",
                        json!({"bytes": 24_000}),
                    )],
                },
                _ => ScriptedReply::Completed {
                    output: vec![assistant_message("done")],
                },
            },
            // Both overflow scripts hand out a large tool result first, so the
            // surface has something a prune can actually remove. Overflowing an
            // empty surface would only prove the retry never fires.
            GateScenario::ContextOverflow => match index {
                0 => ScriptedReply::Completed {
                    output: vec![function_call(
                        "call-large",
                        "read_large",
                        json!({"bytes": 24_000}),
                    )],
                },
                1 => ScriptedReply::Error {
                    status: 400,
                    kind: "context_length_exceeded".into(),
                    message: "input exceeds the context window".into(),
                },
                _ => ScriptedReply::Completed {
                    output: vec![assistant_message("recovered")],
                },
            },
            GateScenario::PersistentContextOverflow => match index {
                0 => ScriptedReply::Completed {
                    output: vec![function_call(
                        "call-large",
                        "read_large",
                        json!({"bytes": 24_000}),
                    )],
                },
                _ => ScriptedReply::Error {
                    status: 400,
                    kind: "context_length_exceeded".into(),
                    message: "input exceeds the context window".into(),
                },
            },
            GateScenario::UnfinishedWork | GateScenario::SlowUnfinishedWork => {
                ScriptedReply::Incomplete {
                    output: vec![assistant_message("partial")],
                    reason: "max_output_tokens".into(),
                }
            }
        }
    }

    /// Deliberate latency, so a cancel or a steer can land mid-turn instead of
    /// racing a reply that already happened.
    fn delay_for(&self, model: &str) -> Option<std::time::Duration> {
        matches!(
            self.catalog.get(model).map(|entry| entry.scenario),
            Some(GateScenario::SlowUnfinishedWork)
        )
        .then(|| std::time::Duration::from_millis(SLOW_RESPONSE_MILLIS))
    }
}

#[derive(Debug, Clone)]
enum ScriptedReply {
    Completed {
        output: Vec<Value>,
    },
    Incomplete {
        output: Vec<Value>,
        reason: String,
    },
    Error {
        status: u16,
        kind: String,
        message: String,
    },
}

fn assistant_message(text: &str) -> Value {
    json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]})
}

fn function_call(call_id: &str, name: &str, arguments: Value) -> Value {
    json!({
        "type": "function_call",
        "call_id": call_id,
        "name": name,
        "arguments": arguments.to_string()
    })
}

pub(crate) fn contains_any_key(value: &Value, keys: &[&str]) -> bool {
    match value {
        Value::Object(map) => map
            .iter()
            .any(|(key, nested)| keys.contains(&key.as_str()) || contains_any_key(nested, keys)),
        Value::Array(items) => items.iter().any(|item| contains_any_key(item, keys)),
        _ => false,
    }
}

pub struct RunningProvider {
    pub base_url: String,
    pub state: Arc<ScriptedProvider>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl RunningProvider {
    pub async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

pub async fn spawn(catalog: GateCatalog) -> std::io::Result<RunningProvider> {
    let state = Arc::new(ScriptedProvider::new(catalog));
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
    Ok(RunningProvider {
        base_url: format!("http://{address}"),
        state,
        shutdown: Some(shutdown_tx),
    })
}

async fn responses(
    State(state): State<Arc<ScriptedProvider>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let thread = headers
        .get(THREAD_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    state.observe(&model, &thread, &body);
    if let Some(delay) = state.delay_for(&model) {
        tokio::time::sleep(delay).await;
    }
    let index = state.next_index(&format!("{thread}:{model}"));
    state.responses_served.fetch_add(1, Ordering::SeqCst);
    match state.script(&model, index) {
        ScriptedReply::Completed { output } => (
            axum::http::StatusCode::OK,
            Json(json!({"id": format!("resp_{index}"), "status": "completed", "output": output})),
        ),
        ScriptedReply::Incomplete { output, reason } => (
            axum::http::StatusCode::OK,
            Json(json!({
                "id": format!("resp_{index}"),
                "status": "incomplete",
                "incomplete_details": {"reason": reason},
                "output": output
            })),
        ),
        ScriptedReply::Error {
            status,
            kind,
            message,
        } => (
            axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::BAD_REQUEST),
            Json(json!({"error": {"type": kind, "message": message}})),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::enhanced_gate::scenarios::{default_catalog, QWEN_MODEL};

    #[test]
    fn a_remote_compaction_field_is_detected_at_any_depth() {
        assert!(contains_any_key(
            &json!({"input": [{"meta": {"compaction_trigger": true}}]}),
            REMOTE_COMPACTION_FIELDS
        ));
        assert!(!contains_any_key(
            &json!({"input": [{"type": "message"}]}),
            REMOTE_COMPACTION_FIELDS
        ));
    }

    #[test]
    fn the_duplicate_script_repeats_one_call_id_then_finishes() {
        let provider = ScriptedProvider::new(default_catalog());
        let first = provider.script(QWEN_MODEL, 0);
        let second = provider.script(QWEN_MODEL, 1);
        for reply in [&first, &second] {
            let ScriptedReply::Completed { output } = reply else {
                panic!("expected a tool call")
            };
            assert_eq!(output[0]["call_id"], "call-duplicate");
        }
        assert!(matches!(
            provider.script(QWEN_MODEL, 2),
            ScriptedReply::Completed { .. }
        ));
    }

    #[tokio::test]
    async fn serves_a_scripted_reply_and_counts_requests() {
        let provider = spawn(default_catalog()).await.unwrap();
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{}/v1/responses", provider.base_url))
            .header(THREAD_HEADER, "thread-1")
            .json(&json!({"model": QWEN_MODEL, "input": []}))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let body = response.json::<Value>().await.unwrap();
        assert_eq!(body["output"][0]["call_id"], "call-duplicate");
        let observations = provider.state.observations();
        assert_eq!(observations.requests_by_model[QWEN_MODEL], 1);
        assert_eq!(observations.remote_compaction_requests, 0);
        provider.stop().await;
    }
}
