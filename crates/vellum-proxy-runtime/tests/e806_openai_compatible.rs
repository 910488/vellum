//! e806 OpenAI-compatible contract coverage (synthetic, redacted fixtures).
//!
//! e806 is a *remote* OpenAI-compatible API, not a Jetson host-local Ollama
//! server. Its route is `openAiCompatible + bearer + chat` with
//! `server_side_resume=false`; resume is Vellum durable canonical history, never
//! an opaque provider response id. These tests freeze the wire contract against
//! scripted fixtures so transport regressions (missing `Content-Type`,
//! `/v1/v1` URLs, leaked Authorization, dropped terminal events) fail locally
//! without touching the live API. No secret value ever appears in a fixture
//! body: the credential used here is the synthetic marker `sk-e806-redacted`.

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde_json::{json, Value};
use vellum_proxy_runtime::config::RuntimeCompactionPolicy;
use vellum_proxy_runtime::credentials::MemoryCredentialProvider;
use vellum_proxy_runtime::environment::ExecutionEnvironment;
use vellum_proxy_runtime::error::RuntimeError;
use vellum_proxy_runtime::exec::{ProxyRuntime, RuntimeResponse};
use vellum_proxy_runtime::lifecycle::CountingRequestLifecycle;
use vellum_proxy_runtime::official_auth::UnconfiguredOfficialAuthProvider;
use vellum_proxy_runtime::request::{
    IncomingAuthContext, RequestMetadata, RuntimeEndpoint, RuntimeRequest,
};
use vellum_proxy_runtime::route::{
    ResolvedRoute, RouteCatalog, RuntimeAuthKind, RuntimeCompactionCapabilities, RuntimeModelRoute,
    RuntimeProviderKind, RuntimeReasoningCapabilities, RuntimeReasoningEffortTransport,
    RuntimeToolCapabilities, RuntimeWireFormat,
};
use vellum_proxy_runtime::transport::{
    TransportError, UpstreamRequest, UpstreamResponse, UpstreamTransport,
};

const E806_SECRET: &str = "sk-e806-redacted";

fn e806_route() -> RuntimeModelRoute {
    RuntimeModelRoute {
        route_id: "cc".into(),
        catalog_id: "vlm-e806-qwen3-6".into(),
        name: "Ollama API (e806)".into(),
        base_url: "https://api.provider.example/v1".into(),
        provider_kind: RuntimeProviderKind::OpenAiCompatible,
        auth_kind: RuntimeAuthKind::Bearer,
        wire: RuntimeWireFormat::Chat,
        server_side_resume: false,
        streaming: true,
        reasoning: true,
        vision: false,
        upstream_model: "qwen3.6".into(),
        context_window: Some(131_072),
        reasoning_capabilities: RuntimeReasoningCapabilities {
            supports_persisted_reasoning: false,
            preserves_reasoning_in_compaction: false,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            reasoning_effort_transport: RuntimeReasoningEffortTransport::None,
        },
        compaction_capabilities: RuntimeCompactionCapabilities {
            supports_server_side_compaction: false,
            supports_standalone_compaction: true,
            compact_threshold_tokens: None,
        },
        compaction_policy: RuntimeCompactionPolicy::default(),
        tool_capabilities: RuntimeToolCapabilities { tool_calling: true },
        credential_id: Some("cc".into()),
        insecure_http_policy: vellum_proxy_runtime::InsecureHttpPolicy::Deny,
        provider_profile: None,
        access_mode: None,
        chat_capabilities: vellum_proxy_runtime::route::RuntimeChatCapabilities::qwen_bridge(),
    }
}

struct FixedCatalog {
    routes: Vec<RuntimeModelRoute>,
}

impl RouteCatalog for FixedCatalog {
    fn active_models(&self) -> Vec<RuntimeModelRoute> {
        self.routes.clone()
    }

    fn resolve_model(&self, catalog_id: &str) -> Option<ResolvedRoute> {
        self.routes
            .iter()
            .find(|route| route.catalog_id == catalog_id)
            .cloned()
            .map(|route| ResolvedRoute { route })
    }

    fn resolve_review_model(&self, catalog_id: &str) -> Option<ResolvedRoute> {
        self.resolve_model(catalog_id)
    }
}

/// A transport that records every upstream request and answers from a script
/// in call order (repeating the last turn, like `FixtureTransport`).
struct RecordingTransport {
    script: Vec<UpstreamResponse>,
    requests: Mutex<Vec<UpstreamRequest>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl RecordingTransport {
    fn new(script: Vec<UpstreamResponse>) -> Self {
        Self {
            script,
            requests: Mutex::new(Vec::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl UpstreamTransport for RecordingTransport {
    async fn execute(&self, request: &UpstreamRequest) -> Result<UpstreamResponse, TransportError> {
        self.requests.lock().unwrap().push(request.clone());
        let index = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.script[index.min(self.script.len() - 1)].clone())
    }
}

fn runtime_with(transport: Arc<RecordingTransport>) -> ProxyRuntime {
    let credentials = Arc::new(MemoryCredentialProvider::new());
    credentials.insert("cc", E806_SECRET);
    ProxyRuntime::new(
        Arc::new(FixedCatalog {
            routes: vec![e806_route()],
        }),
        credentials,
        Arc::new(UnconfiguredOfficialAuthProvider),
        Arc::new(CountingRequestLifecycle::new()),
        transport,
    )
}

fn request(body: Value) -> RuntimeRequest {
    RuntimeRequest {
        body,
        endpoint: RuntimeEndpoint::Responses,
        incoming_auth: IncomingAuthContext::default(),
        execution_environment: ExecutionEnvironment::posix_reference(),
        metadata: RequestMetadata {
            request_id: "req-e806-test".into(),
            received_at_ms: 0,
            review_run_id: None,
            review_role: None,
            primary_failure_reason: None,
            connection_id: None,
            ..Default::default()
        },
    }
}

fn ok_json(body: Value) -> UpstreamResponse {
    UpstreamResponse {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: serde_json::to_vec(&body).unwrap(),
    }
}

fn error_json(status: u16, message: &str) -> UpstreamResponse {
    UpstreamResponse {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: serde_json::to_vec(&json!({"error": {"message": message, "type": "test_error"}}))
            .unwrap(),
    }
}

#[tokio::test]
async fn e806_upstream_request_is_bearer_chat_stateless_without_v1_duplication() {
    let transport = Arc::new(RecordingTransport::new(vec![ok_json(json!({
        "id": "chatcmpl-e806-shape",
        "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))]));
    let runtime = runtime_with(transport.clone());
    let response = runtime
        .execute(
            request(json!({"model": "vlm-e806-qwen3-6", "input": "hi"})),
            "test_exec",
        )
        .await
        .unwrap();
    assert!(matches!(response, RuntimeResponse::Json(_)));

    let recorded = transport.requests.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    let upstream = &recorded[0];
    assert_eq!(upstream.method, "POST");
    // One `/v1` from the base, one `/chat/completions` suffix: no `/v1/v1/...`.
    assert_eq!(
        upstream.url, "https://api.provider.example/v1/chat/completions",
        "e806 base URL must not be duplicated"
    );
    assert!(!upstream.url.contains("/v1/v1"));
    let content_type = upstream
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .expect("content-type must be declared");
    assert_eq!(content_type.1, "application/json");
    let authorization = upstream
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .expect("bearer route must send authorization");
    assert_eq!(authorization.1, format!("Bearer {E806_SECRET}"));
    let body: Value = serde_json::from_slice(&upstream.body).unwrap();
    assert_eq!(body["model"], "qwen3.6", "upstream model mapping");
}

#[tokio::test]
async fn e806_chat_completion_normalizes_usage_reasoning_and_content() {
    let transport = Arc::new(RecordingTransport::new(vec![ok_json(json!({
        "id": "chatcmpl-e806-1",
        "object": "chat.completion",
        "created": 1_700_000_000,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "The answer is 42.",
                "reasoning_content": "Let me think step by step."
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19}
    }))]));
    let runtime = runtime_with(transport);
    let response = runtime
        .execute(
            request(json!({"model": "vlm-e806-qwen3-6", "input": "1 + 1?"})),
            "test_exec",
        )
        .await
        .unwrap();
    let RuntimeResponse::Json(body) = response else {
        panic!("non-streaming dispatch returned a non-JSON response");
    };
    assert_eq!(body["object"], "response");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["id"], "chatcmpl-e806-1");
    let output = body["output"].as_array().unwrap();
    let reasoning = output
        .iter()
        .find(|item| item["type"] == "reasoning")
        .expect("reasoning_content must be normalized to a reasoning item");
    assert_eq!(
        reasoning["summary"][0]["text"],
        "Let me think step by step."
    );
    let message = output
        .iter()
        .find(|item| item["type"] == "message")
        .expect("content must be normalized to a message item");
    assert_eq!(message["role"], "assistant");
    assert_eq!(message["content"][0]["text"], "The answer is 42.");
    assert_eq!(body["usage"]["input_tokens"], 12);
    assert_eq!(body["usage"]["output_tokens"], 7);
    assert_eq!(body["usage"]["total_tokens"], 19);
    // Ollama-style `reasoning` field must be accepted just like
    // `reasoning_content` (both spellings are already handled upstream).
    assert_eq!(
        vellum_proxy_runtime::chat_reasoning_text(&json!({"reasoning": "draft"})),
        Some("draft")
    );
}

#[tokio::test]
async fn e806_chat_sse_stream_ends_with_completed_and_never_fails() {
    let sse = concat!(
        "data: {\"id\":\"chatcmpl-e806-stream\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"thinking\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-e806-stream\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-e806-stream\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"id\":\"chatcmpl-e806-stream\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":7,\"total_tokens\":127}}\n\n",
        "data: [DONE]\n\n",
    );
    let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: sse.as_bytes().to_vec(),
    }]));
    let runtime = runtime_with(transport);
    let response = runtime
        .execute(
            request(json!({
                "model": "vlm-e806-qwen3-6",
                "input": "hi",
                "stream": true
            })),
            "test_exec",
        )
        .await
        .unwrap();
    let RuntimeResponse::Sse(stream) = response else {
        panic!("stream: true must produce RuntimeResponse::Sse");
    };
    let mut text = String::new();
    let mut stream = Box::pin(stream);
    while let Some(item) = stream.next().await {
        text.push_str(std::str::from_utf8(&item.unwrap()).unwrap());
    }
    let events = text
        .lines()
        .filter_map(|line| line.strip_prefix("event: "))
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert!(
        events.contains(&"response.completed".to_string()),
        "chat stream must terminate with response.completed: {text}"
    );
    assert!(!text.contains("response.failed"), "stream failed: {text}");
    assert!(text.contains("reasoning"), "reasoning delta lost: {text}");
    assert!(text.contains("Hello"), "content delta lost: {text}");
    let records = runtime.usage_records().expect("usage ledger");
    assert_eq!(
        records.len(),
        1,
        "one terminal row per request: {records:?}"
    );
    assert_eq!(records[0].input_tokens, 120);
    assert_eq!(records[0].output_tokens, 7);
    assert_eq!(records[0].outcome.as_deref(), Some("success"));
}

#[tokio::test]
async fn e806_interrupted_chat_stream_fails_closed_with_a_terminal_event() {
    // A stream that ends without `[DONE]` and without a completed frame must
    // surface a terminal failure event, never a silent hang.
    let sse = "data: {\"id\":\"chatcmpl-e806-cut\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
    let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: sse.as_bytes().to_vec(),
    }]));
    let runtime = runtime_with(transport);
    let response = runtime
        .execute(
            request(json!({
                "model": "vlm-e806-qwen3-6",
                "input": "hi",
                "stream": true
            })),
            "test_exec",
        )
        .await
        .unwrap();
    let RuntimeResponse::Sse(stream) = response else {
        panic!("stream: true must produce RuntimeResponse::Sse");
    };
    let mut text = String::new();
    let mut stream = Box::pin(stream);
    while let Some(item) = stream.next().await {
        text.push_str(std::str::from_utf8(&item.unwrap()).unwrap());
    }
    assert!(
        text.contains("response.failed")
            || text.contains("Upstream Chat stream ended without [DONE]"),
        "interrupted stream must fail closed with a terminal event: {text}"
    );
}

#[tokio::test]
async fn e806_finish_reason_completes_when_done_marker_is_omitted() {
    let sse = "data: {\"id\":\"chatcmpl-e806-terminal\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"image understood\"},\"finish_reason\":\"stop\"}]}\n\n";
    let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: sse.as_bytes().to_vec(),
    }]));
    let runtime = runtime_with(transport);
    let response = runtime
        .execute(
            request(json!({
                "model": "vlm-e806-qwen3-6",
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "describe it"},
                        {"type": "input_image", "image_url": "data:image/png;base64,aW1hZ2U="}
                    ]
                }],
                "stream": true
            })),
            "test_exec_finish_reason_without_done",
        )
        .await
        .unwrap();
    let RuntimeResponse::Sse(mut stream) = response else {
        panic!("stream: true must produce RuntimeResponse::Sse");
    };
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        text.push_str(std::str::from_utf8(&item.unwrap()).unwrap());
    }
    assert!(text.contains("response.completed"), "{text}");
    assert!(text.contains("image understood"), "{text}");
    assert!(!text.contains("response.failed"), "{text}");
}

#[tokio::test]
async fn e806_typed_tool_call_and_tool_result_continuation_round_trip() {
    let turn_one = ok_json(json!({
        "id": "chatcmpl-e806-tool-1",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_e806_weather",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Taipei\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8}
    }));
    let turn_two = ok_json(json!({
        "id": "chatcmpl-e806-tool-2",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "It is sunny in Taipei."},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14}
    }));
    let transport = Arc::new(RecordingTransport::new(vec![turn_one, turn_two]));
    let runtime = runtime_with(transport.clone());

    // Turn 1: the typed tool call arrives as a normalized function_call item.
    let response = runtime
        .execute(
            request(json!({
                "model": "vlm-e806-qwen3-6",
                "input": "What is the weather in Taipei?"
            })),
            "test_exec",
        )
        .await
        .unwrap();
    let RuntimeResponse::Json(body) = response else {
        panic!("non-streaming dispatch returned a non-JSON response");
    };
    let call = body["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("typed tool call must be preserved");
    assert_eq!(call["call_id"], "call_e806_weather");
    assert_eq!(call["name"], "get_weather");
    assert_eq!(call["arguments"], "{\"city\":\"Taipei\"}");

    // Turn 2: resume through Vellum durable canonical history (never an
    // opaque provider response id). The hydrated chat body must carry the
    // assistant tool_calls followed by a `tool` role result, with no adjacent
    // assistant messages.
    let response = runtime
        .execute(request(json!({
            "model": "vlm-e806-qwen3-6",
            "previous_response_id": "chatcmpl-e806-tool-1",
            "input": [{"type": "function_call_output", "call_id": "call_e806_weather", "output": "sunny"}]
        })), "test_exec")
        .await
        .unwrap();
    let RuntimeResponse::Json(body) = response else {
        panic!("non-streaming dispatch returned a non-JSON response");
    };
    assert_eq!(
        body["output"][0]["content"][0]["text"],
        "It is sunny in Taipei."
    );

    let recorded = transport.requests.lock().unwrap();
    assert_eq!(recorded.len(), 2);
    let continuation: Value = serde_json::from_slice(&recorded[1].body).unwrap();
    let messages = continuation["messages"].as_array().expect("chat messages");
    let roles = messages
        .iter()
        .map(|message| message["role"].as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    for window in roles.windows(2) {
        assert_ne!(
            window[0], window[1],
            "chat history must not contain adjacent same-role messages: {roles:?}"
        );
    }
    let assistant = messages
        .iter()
        .find(|message| {
            message["role"] == "assistant"
                && message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|calls| !calls.is_empty())
        })
        .expect("hydrated assistant tool_calls message");
    assert_eq!(
        assistant["tool_calls"][0]["function"]["name"],
        "get_weather"
    );
    assert_eq!(assistant["tool_calls"][0]["id"], "call_e806_weather");
    let tool = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("tool result must be hydrated as a tool message");
    assert_eq!(tool["tool_call_id"], "call_e806_weather");
    assert_eq!(tool["content"], "sunny");
}

#[tokio::test]
async fn e806_http_errors_map_to_stable_categories_and_never_leak_secrets() {
    for (status, message, expected) in [
        (401, "invalid token", "provider unauthorized"),
        (429, "rate limited", "provider quota"),
        (500, "gateway exploded", "provider unavailable"),
        (503, "unavailable", "provider unavailable"),
    ] {
        let transport = Arc::new(RecordingTransport::new(vec![error_json(status, message)]));
        let runtime = runtime_with(transport);
        let error = runtime
            .execute(
                request(json!({"model": "vlm-e806-qwen3-6", "input": "hi"})),
                "test_exec",
            )
            .await
            .unwrap_err();
        let rendered = error.to_string();
        assert!(
            rendered.contains(expected),
            "status {status} must map to {expected}, got {rendered}"
        );
        assert!(
            !rendered.contains(E806_SECRET),
            "runtime errors must never leak the bearer secret: {rendered}"
        );
    }
}

#[tokio::test]
async fn e806_non_json_error_body_maps_5xx_to_provider_unavailable_with_bounded_diagnostic() {
    let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
        status: 502,
        headers: vec![("content-type".into(), "text/html".into())],
        body: b"<html>bad gateway</html>".to_vec(),
    }]));
    let runtime = runtime_with(transport);
    let error = runtime
        .execute(
            request(json!({"model": "vlm-e806-qwen3-6", "input": "hi"})),
            "test_exec",
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::ProviderUnavailable(_)),
        "HTTP 5xx maps to provider_unavailable before body shape is considered, got {error:?}"
    );
    let rendered = error.to_string();
    assert!(
        rendered.contains("provider unavailable"),
        "the bounded provider diagnostic must survive the mapping: {rendered}"
    );
    assert!(
        rendered.contains("bad gateway"),
        "a bounded HTML/plain-text diagnostic is kept for triage: {rendered}"
    );
    assert!(!rendered.contains(E806_SECRET));
}
