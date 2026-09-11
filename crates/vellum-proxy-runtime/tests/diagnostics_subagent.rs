//! Subagent diagnostic capture through the ordinary HTTP execution path.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use vellum_proxy_runtime::config::RuntimeCompactionPolicy;
use vellum_proxy_runtime::credentials::MemoryCredentialProvider;
use vellum_proxy_runtime::environment::ExecutionEnvironment;
use vellum_proxy_runtime::exec::ProxyRuntime;
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
use vellum_proxy_runtime::transport::{UpstreamRequest, UpstreamResponse, UpstreamTransport};
use vellum_proxy_runtime::{
    DetailLevel, DiagnosticEvent, DiagnosticsSink, LinkConfidence, SubagentLinkMethod,
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
        route_id: "diagnostic-route".into(),
        catalog_id: "child-model".into(),
        name: "Diagnostics Fixture".into(),
        base_url: "https://diagnostics.invalid/v1".into(),
        provider_kind: RuntimeProviderKind::OpenAiCompatible,
        auth_kind: RuntimeAuthKind::None,
        wire: RuntimeWireFormat::Responses,
        server_side_resume: false,
        streaming: false,
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

struct RecordingTransport {
    responses: Vec<UpstreamResponse>,
    calls: std::sync::atomic::AtomicUsize,
    requests: Mutex<Vec<Value>>,
}

impl RecordingTransport {
    fn new(responses: Vec<UpstreamResponse>) -> Self {
        Self {
            responses,
            calls: std::sync::atomic::AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl UpstreamTransport for RecordingTransport {
    async fn execute(
        &self,
        request: &UpstreamRequest,
    ) -> Result<UpstreamResponse, vellum_proxy_runtime::TransportError> {
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::from_slice(&request.body).unwrap());
        let index = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.responses[index.min(self.responses.len() - 1)].clone())
    }
}

#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<DiagnosticEvent>>,
}

impl DiagnosticsSink for RecordingSink {
    fn record(&self, event: DiagnosticEvent) {
        self.events.lock().unwrap().push(event);
    }
    fn detail_level(&self) -> DetailLevel {
        DetailLevel::T1Summary
    }
}

fn runtime(transport: Arc<RecordingTransport>, sink: Arc<RecordingSink>) -> ProxyRuntime {
    ProxyRuntime::new(
        Arc::new(FixedCatalog),
        Arc::new(MemoryCredentialProvider::new()),
        Arc::new(UnconfiguredOfficialAuthProvider),
        Arc::new(CountingRequestLifecycle::new()),
        transport,
    )
    .with_diagnostics_sink(sink)
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

fn response(body: Value) -> UpstreamResponse {
    UpstreamResponse {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: serde_json::to_vec(&body).unwrap(),
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
                "task_name": "diagnostic-child",
                "fork_turns": "all"
            })).unwrap()
        }],
        "usage": {"input_tokens": 20, "output_tokens": 5}
    }))
}

fn completed_response(id: &str) -> UpstreamResponse {
    response(json!({
        "id": id,
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "CHILD_RESULT"}]
        }],
        "usage": {"input_tokens": 30, "output_tokens": 7}
    }))
}

fn child_events(events: &[DiagnosticEvent]) -> Vec<&DiagnosticEvent> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event,
                DiagnosticEvent::SpawnRequested(_)
                    | DiagnosticEvent::ChildTurn(_)
                    | DiagnosticEvent::SpawnCompleted(_)
            )
        })
        .collect()
}

#[tokio::test]
async fn spawn_child_and_completion_produce_three_linkable_events() {
    let sink = Arc::new(RecordingSink::default());
    let transport = Arc::new(RecordingTransport::new(vec![
        spawn_parent_response("call-1", "Reply exactly CHILD_RESULT"),
        completed_response("resp-child"),
        completed_response("resp-parent-final"),
    ]));
    let runtime = runtime(transport, Arc::clone(&sink));

    runtime
        .execute(
            request(
                "parent-1",
                json!({"model": "child-model", "input": "spawn a child"}),
            ),
            "test_exec",
        )
        .await
        .unwrap();
    runtime
        .execute(
            request(
                "child-1",
                json!({
                    "model": "child-model",
                    "instructions": "Reply exactly CHILD_RESULT",
                    "input": [{
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "Reply exactly CHILD_RESULT"}]
                    }]
                }),
            ),
            "test_exec",
        )
        .await
        .unwrap();
    runtime
        .execute(
            request(
                "parent-2",
                json!({
                    "model": "child-model",
                    "input": [{
                        "type": "function_call_output",
                        "call_id": "call-1",
                        "output": "{\"result\":\"CHILD_RESULT\"}"
                    }]
                }),
            ),
            "test_exec",
        )
        .await
        .unwrap();

    let events = sink.events.lock().unwrap().clone();
    let subagents = child_events(&events);
    assert_eq!(subagents.len(), 3);
    assert!(matches!(subagents[0], DiagnosticEvent::SpawnRequested(_)));
    assert!(matches!(subagents[1], DiagnosticEvent::ChildTurn(_)));
    assert!(matches!(subagents[2], DiagnosticEvent::SpawnCompleted(_)));
    let DiagnosticEvent::ChildTurn(child) = subagents[1] else {
        panic!("expected child turn");
    };
    assert_eq!(child.call_id.as_deref(), Some("call-1"));
    assert_eq!(child.parent_request_id.as_deref(), Some("parent-1"));
    assert_eq!(child.request_id, "child-1");
    assert_eq!(child.model, "child-model");
    assert_eq!(child.link_method, SubagentLinkMethod::ContentHash);
    assert_eq!(child.link_confidence, LinkConfidence::High);
    let DiagnosticEvent::SpawnCompleted(done) = subagents[2] else {
        panic!("expected spawn completed");
    };
    // The join key an aggregation layer needs to look up the child's own
    // recorded outcome instead of trusting that this event fired at all.
    assert_eq!(done.child_request_id.as_deref(), Some("child-1"));
}

#[tokio::test]
async fn spawn_completed_shares_call_id_with_spawn_requested() {
    let sink = Arc::new(RecordingSink::default());
    let transport = Arc::new(RecordingTransport::new(vec![
        spawn_parent_response("call-hard", "Reply exactly CHILD_RESULT"),
        completed_response("resp-parent-final"),
    ]));
    let runtime = runtime(transport, Arc::clone(&sink));

    runtime
        .execute(
            request(
                "parent-hard-1",
                json!({"model": "child-model", "input": "spawn a child"}),
            ),
            "test_exec",
        )
        .await
        .unwrap();
    runtime
        .execute(
            request(
                "parent-hard-2",
                json!({
                    "model": "child-model",
                    "input": [{
                        "type": "function_call_output",
                        "call_id": "call-hard",
                        "output": "{\"result\":\"CHILD_RESULT\"}"
                    }]
                }),
            ),
            "test_exec",
        )
        .await
        .unwrap();

    let events = sink.events.lock().unwrap().clone();
    let requested = events.iter().find_map(|event| match event {
        DiagnosticEvent::SpawnRequested(value) => Some(value.clone()),
        _ => None,
    });
    let completed = events.iter().find_map(|event| match event {
        DiagnosticEvent::SpawnCompleted(value) => Some(value.clone()),
        _ => None,
    });
    let requested = requested.expect("spawn_requested");
    let completed = completed.expect("spawn_completed");
    assert_eq!(requested.call_id, "call-hard");
    assert_eq!(completed.call_id, requested.call_id);
    // No child request was ever executed between the spawn and its
    // completion in this test, so Point B never ran and no confident link
    // exists. An aggregator must see that absence explicitly (`None`), never
    // a guessed request id.
    assert_eq!(completed.child_request_id, None);
}

#[tokio::test]
async fn parallel_ambiguous_spawns_do_not_get_a_false_hard_edge() {
    let sink = Arc::new(RecordingSink::default());
    let transport = Arc::new(RecordingTransport::new(vec![
        spawn_parent_response("call-a", "SAME_PROMPT"),
        spawn_parent_response("call-b", "SAME_PROMPT"),
        completed_response("resp-child"),
    ]));
    let runtime = runtime(transport, Arc::clone(&sink));
    for id in ["parent-a", "parent-b"] {
        runtime
            .execute(
                request(id, json!({"model": "child-model", "input": "spawn"})),
                "test_exec",
            )
            .await
            .unwrap();
    }
    runtime
        .execute(
            request(
                "child-ambiguous",
                json!({
                    "model": "child-model",
                    "instructions": "SAME_PROMPT",
                    "input": []
                }),
            ),
            "test_exec",
        )
        .await
        .unwrap();

    let events = sink.events.lock().unwrap().clone();
    let child = events
        .iter()
        .find_map(|event| match event {
            DiagnosticEvent::ChildTurn(value) => Some(value.clone()),
            _ => None,
        })
        .expect("child turn");
    assert_eq!(child.call_id, None);
    assert_eq!(child.parent_request_id, None);
    assert_eq!(child.link_method, SubagentLinkMethod::None);
    assert_eq!(child.link_confidence, LinkConfidence::None);
}

#[tokio::test]
async fn t1_child_payload_contains_manifest_not_input_bodies() {
    let sink = Arc::new(RecordingSink::default());
    let transport = Arc::new(RecordingTransport::new(vec![
        spawn_parent_response("call-t1", "SECRET_FREE_PROMPT"),
        completed_response("resp-child"),
    ]));
    let runtime = runtime(transport, Arc::clone(&sink));
    runtime
        .execute(
            request(
                "parent-t1",
                json!({"model": "child-model", "input": "spawn"}),
            ),
            "test_exec",
        )
        .await
        .unwrap();
    runtime
        .execute(
            request(
                "child-t1",
                json!({
                    "model": "child-model",
                    "input": [{
                        "type": "message",
                        "role": "user",
                        "secret_body": "must-not-store",
                        "content": [{"type": "input_text", "text": "SECRET_FREE_PROMPT"}]
                    }]
                }),
            ),
            "test_exec",
        )
        .await
        .unwrap();

    let events = sink.events.lock().unwrap().clone();
    let child = events
        .iter()
        .find_map(|event| match event {
            DiagnosticEvent::ChildTurn(value) => Some(value.clone()),
            _ => None,
        })
        .expect("child turn");
    assert!(child.full_context.is_none());
    assert_eq!(child.input_manifest.len(), 1);
    assert!(child.input_manifest[0].hash.starts_with("sha256:"));
    let encoded = serde_json::to_string(&child).unwrap();
    assert!(!encoded.contains("must-not-store"));
}
