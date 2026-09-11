//! OpenCode Zen compatibility: public auth, identity headers, MiMo Chat
//! Completions, and 429 cooldown. Normal tests use a scripted gateway; the
//! explicit ignored gate exercises one real public MiMo request.

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
    ResolvedRoute, RouteCatalog, RuntimeAccessMode, RuntimeAuthKind, RuntimeCompactionCapabilities,
    RuntimeModelRoute, RuntimeProviderKind, RuntimeProviderProfile, RuntimeReasoningCapabilities,
    RuntimeReasoningEffortTransport, RuntimeToolCapabilities, RuntimeWireFormat,
};
use vellum_proxy_runtime::transport::{
    ReqwestTransport, TransportError, UpstreamRequest, UpstreamResponse, UpstreamStream,
    UpstreamTransport,
};
use vellum_proxy_runtime::{OPENCODE_PUBLIC_TOKEN, OPENCODE_ZEN_BASE_URL};

fn mimo_route() -> RuntimeModelRoute {
    RuntimeModelRoute {
        route_id: "opencode-zen".into(),
        catalog_id: "vlm-mimo-v2-5-free".into(),
        name: "OpenCode Zen".into(),
        base_url: OPENCODE_ZEN_BASE_URL.into(),
        provider_kind: RuntimeProviderKind::OpenAiCompatible,
        auth_kind: RuntimeAuthKind::None,
        wire: RuntimeWireFormat::Chat,
        server_side_resume: false,
        streaming: true,
        reasoning: true,
        vision: false,
        upstream_model: "mimo-v2.5-free".into(),
        context_window: Some(200_000),
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
        credential_id: None,
        insecure_http_policy: vellum_proxy_runtime::InsecureHttpPolicy::Deny,
        provider_profile: Some(RuntimeProviderProfile::OpenCodeZen),
        access_mode: Some(RuntimeAccessMode::AnonymousFree),
        chat_capabilities: vellum_proxy_runtime::route::RuntimeChatCapabilities::default(),
    }
}

fn laguna_route() -> RuntimeModelRoute {
    let mut route = mimo_route();
    route.catalog_id = "vlm-laguna-s-2-1-free".into();
    route.upstream_model = "laguna-s-2.1-free".into();
    route.name = "OpenCode Laguna".into();
    route.reasoning = false;
    route
}

fn live_inconclusive(error: &RuntimeError) -> bool {
    matches!(
        error.category(),
        "provider_quota" | "provider_unavailable" | "stream_open_timeout"
    )
}

struct RecordingLiveTransport {
    inner: ReqwestTransport,
    requests: Mutex<Vec<UpstreamRequest>>,
}

impl RecordingLiveTransport {
    fn new() -> Self {
        Self {
            inner: ReqwestTransport::new(),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn chat_messages(&self, index: usize) -> Value {
        let requests = self.requests.lock().unwrap();
        serde_json::from_slice(&requests[index].body).expect("upstream chat json")
    }
}

#[async_trait::async_trait]
impl UpstreamTransport for RecordingLiveTransport {
    async fn execute(&self, request: &UpstreamRequest) -> Result<UpstreamResponse, TransportError> {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.execute(request).await
    }

    async fn execute_streaming(
        &self,
        request: &UpstreamRequest,
    ) -> Result<UpstreamStream, TransportError> {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.execute_streaming(request).await
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

    fn call_count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
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
    ProxyRuntime::new(
        Arc::new(FixedCatalog {
            routes: vec![mimo_route()],
        }),
        Arc::new(MemoryCredentialProvider::new()),
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
            request_id: "req-opencode-test".into(),
            received_at_ms: 0,
            review_run_id: None,
            review_role: None,
            primary_failure_reason: None,
            connection_id: None,
            ..Default::default()
        },
    }
}

fn header<'a>(request: &'a UpstreamRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn chat_ok(body: Value) -> UpstreamResponse {
    UpstreamResponse {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: serde_json::to_vec(&body).unwrap(),
    }
}

#[tokio::test]
async fn zen_free_mimo_uses_public_token_and_redacted_identity_headers() {
    let transport = Arc::new(RecordingTransport::new(vec![chat_ok(json!({
        "id": "chatcmpl-mimo-1",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))]));
    let runtime = runtime_with(transport.clone());
    runtime
        .execute(
            request(json!({
                "model": "vlm-mimo-v2-5-free",
                "prompt_cache_key": "codex-thread-secret",
                "metadata": {"thread_id": "thread_raw"},
                "input": "hi"
            })),
            "test_exec",
        )
        .await
        .unwrap();
    let upstream = transport.requests.lock().unwrap()[0].clone();
    assert!(
        upstream.url.ends_with("/chat/completions"),
        "MiMo must stay on Chat Completions, got {}",
        upstream.url
    );
    let expected_auth = format!("Bearer {OPENCODE_PUBLIC_TOKEN}");
    assert_eq!(
        header(&upstream, "authorization"),
        Some(expected_auth.as_str())
    );
    assert_eq!(header(&upstream, "x-opencode-client"), Some("vellum"));
    let ua = header(&upstream, "user-agent").unwrap();
    assert!(ua.starts_with("opencode/vellum-"), "{ua}");
    let session = header(&upstream, "x-opencode-session").unwrap();
    let req_id = header(&upstream, "x-opencode-request").unwrap();
    assert!(!session.contains("codex-thread-secret"));
    assert!(!session.contains("thread_raw"));
    assert!(!req_id.contains("codex-thread-secret"));
    let body: Value = serde_json::from_slice(&upstream.body).unwrap();
    assert!(body.get("prompt_cache_key").is_none());
    assert!(body.get("previous_response_id").is_none());
    assert!(body.get("store").is_none());
    assert_eq!(body["model"], "mimo-v2.5-free");
    assert!(body.get("messages").is_some());
}

#[tokio::test]
async fn session_is_stable_across_turns_and_retries_reuse_request_id() {
    let transport = Arc::new(RecordingTransport::new(vec![chat_ok(json!({
        "id": "chatcmpl-mimo-turn",
        "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))]));
    let runtime = runtime_with(transport.clone());
    let turn_one = request(json!({
        "model": "vlm-mimo-v2-5-free",
        "prompt_cache_key": "thread-abc",
        "input": "one"
    }));
    let turn_two = request(json!({
        "model": "vlm-mimo-v2-5-free",
        "prompt_cache_key": "thread-abc",
        "input": "two"
    }));
    runtime.execute(turn_one.clone(), "exec-1").await.unwrap();
    runtime.execute(turn_two, "exec-2").await.unwrap();
    runtime.execute(turn_one, "exec-1-retry").await.unwrap();
    let requests = transport.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    let session = |index: usize| {
        header(&requests[index], "x-opencode-session")
            .unwrap()
            .to_string()
    };
    let req = |index: usize| {
        header(&requests[index], "x-opencode-request")
            .unwrap()
            .to_string()
    };
    assert_eq!(session(0), session(1));
    assert_eq!(session(0), session(2));
    assert_ne!(
        req(0),
        req(1),
        "different turns must not share a request id"
    );
    assert_eq!(
        req(0),
        req(2),
        "retry of the same body reuses the request id"
    );
}

#[tokio::test]
async fn mimo_streaming_tool_call_round_trip_does_not_leak_thread_identity() {
    let sse = concat!(
        "data: {\"id\":\"chatcmpl-mimo-stream\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_mimo\",\"type\":\"function\",\"function\":{\"name\":\"vellum_probe_tool\",\"arguments\":\"{\\\"value\\\":\\\"ok\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-mimo-stream\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: sse.as_bytes().to_vec(),
    }]));
    let runtime = runtime_with(transport.clone());
    let response = runtime
        .execute(
            request(json!({
                "model": "vlm-mimo-v2-5-free",
                "prompt_cache_key": "codex-thread-secret",
                "stream": true,
                "tools": [{
                    "type": "function",
                    "name": "vellum_probe_tool",
                    "parameters": {"type": "object", "properties": {"value": {"type": "string"}}}
                }],
                "input": "Call the tool."
            })),
            "test_exec",
        )
        .await
        .unwrap();
    let RuntimeResponse::Sse(stream) = response else {
        panic!("stream: true must produce SSE");
    };
    let mut text = String::new();
    let mut stream = Box::pin(stream);
    while let Some(item) = stream.next().await {
        text.push_str(std::str::from_utf8(&item.unwrap()).unwrap());
    }
    assert!(
        text.contains("function_call") || text.contains("vellum_probe_tool"),
        "{text}"
    );
    assert!(
        text.contains("response.completed") || text.contains("function_call"),
        "stream must complete a tool call: {text}"
    );
    assert!(!text.contains("codex-thread-secret"));
    let upstream = &transport.requests.lock().unwrap()[0];
    assert!(upstream.url.ends_with("/chat/completions"));
    let body: Value = serde_json::from_slice(&upstream.body).unwrap();
    assert!(body.get("messages").is_some());
    assert!(body.get("tools").is_some());
    assert_eq!(body["stream"], true);
    assert!(body.get("prompt_cache_key").is_none());
    assert!(!header(upstream, "x-opencode-session")
        .unwrap()
        .contains("codex-thread-secret"));
    let records = runtime.usage_records().unwrap();
    assert!(records.iter().any(|record| {
        record.outcome.as_deref() == Some("success")
            && record.auth_mode.as_deref() == Some("anonymousFree")
            && record.provider_profile.as_deref() == Some("openCodeZen")
            && record.upstream_attempted == Some(true)
    }));
}

#[tokio::test]
async fn quota_cooldown_turns_later_retries_into_local_errors() {
    let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
        status: 429,
        headers: vec![
            ("content-type".into(), "application/json".into()),
            ("retry-after".into(), "45".into()),
        ],
        body: serde_json::to_vec(&json!({"error": {"message": "rate limited by zen"}})).unwrap(),
    }]));
    let runtime = runtime_with(transport.clone());
    let body = json!({
        "model": "vlm-mimo-v2-5-free",
        "prompt_cache_key": "thread-abc",
        "input": "hi"
    });
    let first = runtime
        .execute(request(body.clone()), "exec-1")
        .await
        .unwrap_err();
    assert_eq!(first.category(), "provider_quota");
    assert_eq!(first.retry_after_secs(), Some(45));
    assert_eq!(first.upstream_attempted(), Some(true));

    let second = runtime
        .execute(request(body.clone()), "exec-2")
        .await
        .unwrap_err();
    assert_eq!(second.category(), "provider_quota");
    assert_eq!(second.upstream_attempted(), Some(false));
    assert_eq!(transport.call_count(), 1);

    runtime.expire_opencode_cooldown("opencode-zen", "mimo-v2.5-free");
    let third = runtime.execute(request(body), "exec-3").await.unwrap_err();
    assert_eq!(third.upstream_attempted(), Some(true));
    assert_eq!(transport.call_count(), 2);

    let records = runtime.usage_records().unwrap();
    assert!(records.iter().any(|record| {
        record.auth_mode.as_deref() == Some("anonymousFree")
            && record.provider_profile.as_deref() == Some("openCodeZen")
            && record.upstream_attempted == Some(false)
            && record.retry_after.is_some()
    }));
}

#[tokio::test]
#[ignore = "live OpenCode public quota; run explicitly during release qualification"]
async fn live_zen_free_mimo_accepts_a_codex_streaming_request() {
    let runtime = ProxyRuntime::new(
        Arc::new(FixedCatalog {
            routes: vec![mimo_route()],
        }),
        Arc::new(MemoryCredentialProvider::new()),
        Arc::new(UnconfiguredOfficialAuthProvider),
        Arc::new(CountingRequestLifecycle::new()),
        Arc::new(ReqwestTransport::new()),
    );
    let response = runtime
        .execute(
            request(json!({
                "model": "vlm-mimo-v2-5-free",
                "prompt_cache_key": "vellum-opencode-live-gate",
                "store": false,
                "stream": true,
                "tool_choice": "none",
                "tools": [],
                "input": "Reply with exactly VELLUM_OPENCODE_OK"
            })),
            "live_opencode_mimo",
        )
        .await
        .expect("OpenCode should accept the translated Codex request");
    let RuntimeResponse::Sse(mut stream) = response else {
        panic!("live MiMo gate must use the streaming path");
    };
    let mut transcript = String::new();
    while let Some(chunk) = stream.next().await {
        transcript.push_str(
            std::str::from_utf8(&chunk.expect("valid upstream stream chunk"))
                .expect("SSE must be UTF-8"),
        );
    }
    assert!(
        transcript.contains("response.output_text.delta"),
        "expected an application delta: {transcript}"
    );
    assert!(
        transcript.contains("response.completed"),
        "expected a terminal response: {transcript}"
    );
    let records = runtime.usage_records().expect("live usage row");
    assert!(records.iter().any(|record| {
        record.outcome.as_deref() == Some("success")
            && record.auth_mode.as_deref() == Some("anonymousFree")
            && record.provider_profile.as_deref() == Some("openCodeZen")
            && record.upstream_attempted == Some(true)
    }));
}

/// Provider-routing live gate: proves the shared runtime's tool-call
/// continuation contract (function_call round-trip, historical prompt
/// dedupe, tail-role placement) against a real OpenCode Zen upstream. Not an
/// Auto Review test -- no Guardian is ever invoked here; Laguna is only the
/// provider under test. See
/// `live_guardian_provider_routing_gate_follow_origin_and_exact_assessment`
/// for the Guardian routing/accounting live gate (also not a Codex task
/// E2E -- see its own doc comment for what a real E2E still needs).
#[tokio::test]
#[ignore = "live OpenCode public quota; provider-routing gate (Laguna two-turn tool continuation)"]
async fn live_provider_routing_gate_laguna_two_turn_tool_continuation() {
    let transport = Arc::new(RecordingLiveTransport::new());
    let runtime = ProxyRuntime::new(
        Arc::new(FixedCatalog {
            routes: vec![laguna_route()],
        }),
        Arc::new(MemoryCredentialProvider::new()),
        Arc::new(UnconfiguredOfficialAuthProvider),
        Arc::new(CountingRequestLifecycle::new()),
        transport.clone(),
    );
    let user = "Call echo_marker with text=VELLUM_LAGUNA_TOOL.";
    let tools = json!([{
        "type": "function",
        "name": "echo_marker",
        "description": "Echo a marker",
        "parameters": {
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        }
    }]);
    let first = match runtime
        .execute(
            request(json!({
                "model": "vlm-laguna-s-2-1-free",
                "prompt_cache_key": "vellum-laguna-live-gate",
                "store": false,
                "stream": false,
                "tool_choice": "required",
                "tools": tools,
                "input": user
            })),
            "live_laguna_turn_1",
        )
        .await
    {
        Ok(RuntimeResponse::Json(body)) => body,
        Ok(_) => panic!("Laguna turn 1 must be a buffered JSON completion"),
        Err(error) if live_inconclusive(&error) => {
            panic!("INCONCLUSIVE: Laguna turn 1 {}: {error}", error.category());
        }
        Err(error) => panic!("Laguna turn 1 failed: {} {error}", error.category()),
    };
    let calls: Vec<Value> = first
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "function_call")
        .cloned()
        .collect();
    if calls.is_empty() {
        panic!("Laguna turn 1 did not emit a function_call: {first}");
    }
    let mut continuation_input = vec![json!({
        "type": "message",
        "role": "user",
        "content": [{ "type": "input_text", "text": user }]
    })];
    continuation_input.extend(calls.iter().cloned());
    for call in &calls {
        continuation_input.push(json!({
            "type": "function_call_output",
            "call_id": call["call_id"],
            "output": "VELLUM_LAGUNA_TOOL"
        }));
    }
    let second = match runtime
        .execute(
            request(json!({
                "model": "vlm-laguna-s-2-1-free",
                "prompt_cache_key": "vellum-laguna-live-gate",
                "store": false,
                "stream": false,
                "tools": tools,
                "input": continuation_input
            })),
            "live_laguna_turn_2",
        )
        .await
    {
        Ok(RuntimeResponse::Json(body)) => body,
        Ok(_) => panic!("Laguna turn 2 must be a buffered JSON completion"),
        Err(error) if live_inconclusive(&error) => {
            panic!("INCONCLUSIVE: Laguna turn 2 {}: {error}", error.category());
        }
        Err(error) => panic!("Laguna turn 2 failed: {} {error}", error.category()),
    };
    assert!(
        second.get("output").is_some(),
        "Laguna continuation must return Responses output: {second}"
    );
    let chat = transport.chat_messages(1);
    let messages = chat["messages"].as_array().expect("Chat messages");
    let user_turns = messages
        .iter()
        .filter(|message| message["role"] == "user" && message["content"] == user)
        .count();
    assert_eq!(
        user_turns, 1,
        "historical user prompt must appear once: {messages:?}"
    );
    assert_eq!(
        messages.last().and_then(|message| message["role"].as_str()),
        Some("tool"),
        "Laguna default continuation keeps the tool result as the tail: {messages:?}"
    );
    eprintln!("QUALIFIED: provider-routing gate (Laguna two-turn tool continuation)");
}

#[tokio::test]
async fn cooldown_does_not_switch_models_or_invent_a_generic_error() {
    let transport = Arc::new(RecordingTransport::new(vec![UpstreamResponse {
        status: 429,
        headers: vec![("retry-after".into(), "12".into())],
        body: serde_json::to_vec(&json!({"error": {"message": "FreeUsageLimitError"}})).unwrap(),
    }]));
    let runtime = runtime_with(transport);
    let error = runtime
        .execute(
            request(json!({"model": "vlm-mimo-v2-5-free", "input": "hi"})),
            "exec",
        )
        .await
        .unwrap_err();
    match error {
        RuntimeError::ProviderQuota {
            message,
            retry_after_secs,
            upstream_attempted,
        } => {
            assert!(message.contains("FreeUsageLimitError"), "{message}");
            assert_eq!(retry_after_secs, Some(12));
            assert!(upstream_attempted);
        }
        other => panic!("expected structured quota error, got {other:?}"),
    }
}
