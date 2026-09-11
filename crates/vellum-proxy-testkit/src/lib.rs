//! A reusable, deterministic fake upstream for exercising Vellum's proxy HTTP
//! pipeline end-to-end (generalizes the inline pattern used by
//! `src-tauri/tests/delegation_turn_loop.rs`).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::Value;
use tokio::task::JoinHandle;

/// One scripted upstream turn.
#[derive(Debug, Clone)]
pub enum ScriptedTurn {
    /// Non-streaming JSON response body.
    Json(Value),
    /// Pre-built SSE body: each string is one `event: ...\ndata: ...\n\n`
    /// block, concatenated in order and served with
    /// `content-type: text/event-stream`.
    Sse(Vec<String>),
    /// HTTP error: status code + JSON error body.
    Status(u16, Value),
    /// Raw bytes that are not valid JSON, to exercise malformed-response
    /// handling.
    Malformed(String),
}

struct Shared {
    script: Vec<ScriptedTurn>,
    calls: AtomicUsize,
    requests: Mutex<Vec<Value>>,
}

/// A fake upstream that serves scripted `ScriptedTurn`s to Vellum's proxy in
/// call order, shared across the three routes it exposes.
pub struct FakeUpstream {
    pub address: SocketAddr,
    shared: Arc<Shared>,
    server: JoinHandle<()>,
}

impl FakeUpstream {
    /// Binds to `127.0.0.1:0` and serves `POST /v1/responses`,
    /// `POST /v1/chat/completions`, and `POST /v1/responses/compact`. Every
    /// call to any of those routes consumes the next entry in `script`, in
    /// call order, from one process-wide counter shared across all three
    /// routes. If the script is exhausted, the last entry repeats instead of
    /// panicking, so a fixture that probes with an extra request still gets a
    /// deterministic reply.
    pub async fn start(script: Vec<ScriptedTurn>) -> Self {
        assert!(
            !script.is_empty(),
            "FakeUpstream needs at least one scripted turn"
        );
        let shared = Arc::new(Shared {
            script,
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake upstream listener");
        let address = listener.local_addr().expect("fake upstream local addr");

        let router = Router::new()
            .route("/v1/responses", post(handle))
            .route("/v1/chat/completions", post(handle))
            .route("/v1/responses/compact", post(handle))
            .with_state(Arc::clone(&shared));

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        Self {
            address,
            shared,
            server,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    /// All request bodies received so far, in order, across all three
    /// routes.
    pub fn captured_requests(&self) -> Vec<Value> {
        self.shared.requests.lock().unwrap().clone()
    }

    pub fn call_count(&self) -> usize {
        self.shared.calls.load(Ordering::SeqCst)
    }

    /// Stop the background server task. Not required before drop — the
    /// server task is aborted automatically — but useful when a test wants
    /// to assert no further calls can land.
    pub fn stop(&self) {
        self.server.abort();
    }
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn handle(State(shared): State<Arc<Shared>>, body: Bytes) -> Response {
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    shared.requests.lock().unwrap().push(request);
    let index = shared.calls.fetch_add(1, Ordering::SeqCst);
    let turn_index = index.min(shared.script.len() - 1);
    match &shared.script[turn_index] {
        ScriptedTurn::Json(value) => Json(value.clone()).into_response(),
        ScriptedTurn::Sse(events) => {
            let body = events.concat();
            (
                StatusCode::OK,
                [("content-type", "text/event-stream")],
                body,
            )
                .into_response()
        }
        ScriptedTurn::Status(status, value) => (
            StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(value.clone()),
        )
            .into_response(),
        ScriptedTurn::Malformed(raw) => (
            StatusCode::OK,
            [("content-type", "application/json")],
            raw.clone(),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn json_turns_are_returned_as_posted() {
        let upstream =
            FakeUpstream::start(vec![ScriptedTurn::Json(json!({"hello": "world"}))]).await;
        let response = reqwest::Client::new()
            .post(format!("{}/responses", upstream.base_url()))
            .json(&json!({"model": "m"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body, json!({"hello": "world"}));
    }

    #[tokio::test]
    async fn sse_turns_come_back_with_event_stream_content_type() {
        let sse = vec![
            "event: response.created\ndata: {}\n\n".to_string(),
            "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n".to_string(),
        ];
        let upstream = FakeUpstream::start(vec![ScriptedTurn::Sse(sse.clone())]).await;
        let response = reqwest::Client::new()
            .post(format!("{}/responses", upstream.base_url()))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(content_type.contains("text/event-stream"));
        let text = response.text().await.unwrap();
        assert_eq!(text, sse.concat());
    }

    #[tokio::test]
    async fn status_turns_return_the_given_status_code() {
        let upstream = FakeUpstream::start(vec![ScriptedTurn::Status(
            429,
            json!({"error": "rate limited"}),
        )])
        .await;
        let response = reqwest::Client::new()
            .post(format!("{}/responses", upstream.base_url()))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body, json!({"error": "rate limited"}));
    }

    #[tokio::test]
    async fn call_counter_advances_and_captures_requests_in_order() {
        let upstream = FakeUpstream::start(vec![
            ScriptedTurn::Json(json!({"turn": 0})),
            ScriptedTurn::Json(json!({"turn": 1})),
        ])
        .await;
        let client = reqwest::Client::new();
        for index in 0..2 {
            let response = client
                .post(format!("{}/responses", upstream.base_url()))
                .json(&json!({"turn": index}))
                .send()
                .await
                .unwrap();
            let body: Value = response.json().await.unwrap();
            assert_eq!(body, json!({"turn": index}));
        }
        assert_eq!(upstream.call_count(), 2);
        assert_eq!(
            upstream.captured_requests(),
            vec![json!({"turn": 0}), json!({"turn": 1})]
        );
    }

    #[tokio::test]
    async fn exhausting_the_script_repeats_the_last_turn() {
        let upstream = FakeUpstream::start(vec![
            ScriptedTurn::Json(json!({"turn": 0})),
            ScriptedTurn::Json(json!({"turn": 1})),
        ])
        .await;
        let client = reqwest::Client::new();
        for _ in 0..5 {
            let response = client
                .post(format!("{}/responses", upstream.base_url()))
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
        }
        let last: Value = client
            .post(format!("{}/responses", upstream.base_url()))
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(last, json!({"turn": 1}));
        assert_eq!(upstream.call_count(), 6);
    }
}
