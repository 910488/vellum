//! Parent/child cancellation fan-out through the shared runtime's compute
//! boundary (commit 4). The runtime executes the parent (which captures the
//! spawn at Point A), the child (which links at Point B), and then the test
//! cancels the parent tree directly through [`ProxyRuntime::cancel_request_tree`]
//! — the same public surface the Desktop bridge and the WebSocket handler use.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use futures_util::stream::StreamExt;
use serde_json::{json, Value};
use vellum_proxy_runtime::config::RuntimeCompactionPolicy;
use vellum_proxy_runtime::credentials::MemoryCredentialProvider;
use vellum_proxy_runtime::environment::ExecutionEnvironment;
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
    TransportError, UpstreamRequest, UpstreamResponse, UpstreamStream, UpstreamTransport,
};

struct FixedCatalog;

impl RouteCatalog for FixedCatalog {
    fn active_models(&self) -> Vec<RuntimeModelRoute> {
        vec![route()]
    }
    fn resolve_model(&self, id: &str) -> Option<ResolvedRoute> {
        (id == "child-model").then(|| ResolvedRoute { route: route() })
    }
    fn resolve_review_model(&self, id: &str) -> Option<ResolvedRoute> {
        self.resolve_model(id)
    }
    fn catalog_entry(&self, _id: &str) -> Option<Value> {
        None
    }
}

fn route() -> RuntimeModelRoute {
    RuntimeModelRoute {
        route_id: "cancel-route".into(),
        catalog_id: "child-model".into(),
        name: "Cancel Fixture".into(),
        base_url: "https://cancel.invalid/v1".into(),
        provider_kind: RuntimeProviderKind::OpenAiCompatible,
        auth_kind: RuntimeAuthKind::None,
        wire: RuntimeWireFormat::Responses,
        server_side_resume: false,
        streaming: true,
        reasoning: true,
        vision: false,
        upstream_model: "child-upstream".into(),
        context_window: Some(128_000),
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
        insecure_http_policy: vellum_proxy_runtime::outbound::InsecureHttpPolicy::Deny,
        provider_profile: None,
        access_mode: None,
        chat_capabilities: vellum_proxy_runtime::route::RuntimeChatCapabilities::default(),
    }
}

/// Scripted transport: buffered answers for the parent (the spawn capture),
/// streaming answers for the child (the in-flight SSE we expect to drop).
struct ScriptedCancelTransport {
    buffered: Mutex<Vec<UpstreamResponse>>,
    streaming: Mutex<Vec<Option<UpstreamStream>>>,
    buffered_calls: AtomicUsize,
    streaming_calls: AtomicUsize,
}

impl ScriptedCancelTransport {
    fn new(buffered: Vec<UpstreamResponse>, streaming: Vec<UpstreamStream>) -> Self {
        Self {
            buffered: Mutex::new(buffered),
            streaming: Mutex::new(streaming.into_iter().map(Some).collect()),
            buffered_calls: AtomicUsize::new(0),
            streaming_calls: AtomicUsize::new(0),
        }
    }

    fn streaming_calls(&self) -> usize {
        self.streaming_calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl UpstreamTransport for ScriptedCancelTransport {
    async fn execute(
        &self,
        _request: &UpstreamRequest,
    ) -> Result<UpstreamResponse, TransportError> {
        self.buffered_calls.fetch_add(1, Ordering::SeqCst);
        let responses = self.buffered.lock().unwrap();
        Ok(responses[0].clone())
    }

    async fn execute_streaming(
        &self,
        _request: &UpstreamRequest,
    ) -> Result<UpstreamStream, TransportError> {
        self.streaming_calls.fetch_add(1, Ordering::SeqCst);
        let mut streams = self.streaming.lock().unwrap();
        streams
            .first_mut()
            .and_then(Option::take)
            .ok_or_else(|| TransportError::Other("no scripted stream left".into()))
    }
}

fn response(body: Value) -> UpstreamResponse {
    UpstreamResponse {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: serde_json::to_vec(&body).unwrap(),
    }
}

fn open_sse_stream() -> UpstreamStream {
    let chunk: Result<Bytes, TransportError> = Ok(Bytes::from_static(
        b"data: {\"id\":\"child-chunk\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
    ));
    let body: futures_util::stream::BoxStream<'static, Result<Bytes, TransportError>> =
        futures_util::stream::iter(vec![chunk])
            .chain(futures_util::stream::pending::<Result<Bytes, TransportError>>())
            .boxed();
    UpstreamStream {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body,
    }
}

fn spawn_parent_response(call_id: &str, prompt: &str) -> UpstreamResponse {
    response(json!({
        "id": format!("resp-{call_id}"),
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "function_call",
            "call_id": call_id,
            "name": "multi_agent_spawn",
            "arguments": serde_json::to_string(&json!({
                "message": prompt,
                "task_name": "cancel-child",
                "fork_turns": "all"
            })).unwrap()
        }],
        "usage": {"input_tokens": 20, "output_tokens": 5}
    }))
}

fn runtime(transport: Arc<ScriptedCancelTransport>) -> ProxyRuntime {
    ProxyRuntime::new(
        Arc::new(FixedCatalog),
        Arc::new(MemoryCredentialProvider::new()),
        Arc::new(UnconfiguredOfficialAuthProvider),
        Arc::new(CountingRequestLifecycle::new()),
        transport,
    )
}

fn request(id: &str, body: Value) -> RuntimeRequest {
    RuntimeRequest {
        body,
        endpoint: RuntimeEndpoint::Responses,
        incoming_auth: IncomingAuthContext::default(),
        execution_environment: ExecutionEnvironment::posix_reference(),
        metadata: RequestMetadata {
            request_id: id.into(),
            received_at_ms: 0,
            review_run_id: None,
            review_role: None,
            primary_failure_reason: None,
            connection_id: None,
            ..Default::default()
        },
    }
}

fn child_body() -> Value {
    json!({
        "model": "child-model",
        "instructions": "Reply exactly CHILD_RESULT",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "Reply exactly CHILD_RESULT"}]
        }],
        "stream": true
    })
}

/// Scenario 2 (commit-level): cancelling a parent drops every confidently
/// bound child's in-flight upstream stream, without the client touching the
/// child sockets.
#[tokio::test]
async fn parent_cancel_fans_out_to_bound_child_upstream() {
    let transport = Arc::new(ScriptedCancelTransport::new(
        vec![spawn_parent_response(
            "call-1",
            "Reply exactly CHILD_RESULT",
        )],
        vec![open_sse_stream()],
    ));
    let runtime = runtime(Arc::clone(&transport));

    let parent = runtime
        .execute(
            request(
                "parent-1",
                json!({"model": "child-model", "input": "spawn a child"}),
            ),
            "parent-1-exec",
        )
        .await
        .expect("parent must execute");
    assert!(matches!(parent, RuntimeResponse::Json(_)));

    let response = runtime
        .execute(request("child-1", child_body()), "child-1-exec")
        .await
        .expect("child must execute");
    let RuntimeResponse::Sse(mut stream) = response else {
        panic!("child must stream: {response:?}");
    };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), stream.next())
        .await
        .expect("child stream must produce the open frame")
        .expect("child stream must not error on open");

    let children = runtime.cancel_request_tree("parent-1-exec");
    assert_eq!(children, vec!["child-1-exec".to_string()]);
    assert!(runtime.is_request_cancelled("child-1-exec"));

    let next = tokio::time::timeout(std::time::Duration::from_secs(3), stream.next())
        .await
        .expect("child stream must end promptly after the parent cancel");
    assert!(
        next.is_none(),
        "child upstream stream must be dropped when its parent is cancelled"
    );
    // Only the spawn capture and the one child stream were ever dispatched.
    assert_eq!(transport.streaming_calls(), 1);
}

/// Scenario 5 (commit-level): a child that links after its parent was
/// already cancelled aborts before any upstream byte is sent.
#[tokio::test]
async fn late_child_linking_to_cancelled_parent_aborts_without_upstream() {
    let transport = Arc::new(ScriptedCancelTransport::new(
        vec![spawn_parent_response(
            "call-5",
            "Reply exactly CHILD_RESULT",
        )],
        vec![open_sse_stream()],
    ));
    let runtime = runtime(Arc::clone(&transport));

    runtime
        .execute(
            request(
                "parent-5",
                json!({"model": "child-model", "input": "spawn a child"}),
            ),
            "parent-5-exec",
        )
        .await
        .expect("parent must execute");

    runtime.cancel_request("parent-5-exec");
    assert!(runtime.is_request_cancelled("parent-5-exec"));

    let response = runtime
        .execute(request("child-5", child_body()), "child-5-exec")
        .await
        .expect("late child must still return a stream (an empty one)");
    let RuntimeResponse::Sse(mut stream) = response else {
        panic!("aborted child must present as an empty stream: {response:?}");
    };
    let next = tokio::time::timeout(std::time::Duration::from_secs(3), stream.next())
        .await
        .expect("aborted child stream must end promptly");
    assert!(
        next.is_none(),
        "late child linking to a cancelled parent must never reach upstream"
    );
    assert!(runtime.is_request_cancelled("child-5-exec"));
    assert_eq!(
        transport.streaming_calls(),
        0,
        "an aborted child must never open an upstream stream"
    );
}
