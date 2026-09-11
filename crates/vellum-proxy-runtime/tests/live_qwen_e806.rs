//! Live e806 Qwen smoke. Requires `VELLUM_LIVE_E806_KEY` and is ignored by
//! default so `cargo test --lib` stays offline.

use std::sync::Arc;

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
use vellum_proxy_runtime::ReqwestTransport;

const CATALOG_ID: &str = "vlm-e806-qwen3-6";
const UPSTREAM_MODEL: &str = "qwen3.6:27b-mtp-q4_K_M";

fn live_key() -> Option<String> {
    std::env::var("VELLUM_LIVE_E806_KEY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn e806_route() -> RuntimeModelRoute {
    RuntimeModelRoute {
        route_id: "cc".into(),
        catalog_id: CATALOG_ID.into(),
        name: "e806 Qwen".into(),
        base_url: "https://api.provider.example/v1".into(),
        provider_kind: RuntimeProviderKind::OpenAiCompatible,
        auth_kind: RuntimeAuthKind::Bearer,
        wire: RuntimeWireFormat::Chat,
        server_side_resume: false,
        streaming: true,
        reasoning: true,
        vision: false,
        upstream_model: UPSTREAM_MODEL.into(),
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
        credential_id: Some("cc".into()),
        insecure_http_policy: vellum_proxy_runtime::InsecureHttpPolicy::Deny,
        provider_profile: None,
        access_mode: None,
        chat_capabilities: vellum_proxy_runtime::route::RuntimeChatCapabilities::qwen_bridge(),
    }
}

struct LiveCatalog {
    route: RuntimeModelRoute,
}

impl RouteCatalog for LiveCatalog {
    fn active_models(&self) -> Vec<RuntimeModelRoute> {
        vec![self.route.clone()]
    }
    fn resolve_model(&self, catalog_id: &str) -> Option<ResolvedRoute> {
        (self.route.catalog_id == catalog_id).then(|| ResolvedRoute {
            route: self.route.clone(),
        })
    }
    fn resolve_review_model(&self, catalog_id: &str) -> Option<ResolvedRoute> {
        self.resolve_model(catalog_id)
    }
}

fn runtime(key: &str) -> ProxyRuntime {
    let credentials = Arc::new(MemoryCredentialProvider::new());
    credentials.insert("cc", key);
    ProxyRuntime::new(
        Arc::new(LiveCatalog {
            route: e806_route(),
        }),
        credentials,
        Arc::new(UnconfiguredOfficialAuthProvider),
        Arc::new(CountingRequestLifecycle::new()),
        Arc::new(ReqwestTransport::new()),
    )
}

fn request(body: Value) -> RuntimeRequest {
    RuntimeRequest {
        body,
        endpoint: RuntimeEndpoint::Responses,
        incoming_auth: IncomingAuthContext::default(),
        execution_environment: ExecutionEnvironment::posix_reference(),
        metadata: RequestMetadata {
            request_id: "req_live_qwen".into(),
            received_at_ms: 0,
            review_run_id: None,
            review_role: None,
            primary_failure_reason: None,
            connection_id: None,
            ..Default::default()
        },
    }
}

fn visible_text(response: &Value) -> String {
    if let Some(text) = response.get("output_text").and_then(Value::as_str) {
        if !text.trim().is_empty() {
            return text.to_string();
        }
    }
    response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("content"))
        .flat_map(|content| match content {
            Value::String(text) => vec![text.clone()],
            Value::Array(parts) => parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .or_else(|| part.get("input_text"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>()
        .join("")
}

async fn execute_json(runtime: &ProxyRuntime, body: Value) -> Value {
    match runtime.execute(request(body), "test_exec").await {
        Ok(RuntimeResponse::Json(value)) => value,
        Ok(RuntimeResponse::Sse(_)) => panic!("expected buffered JSON, got SSE"),
        Ok(RuntimeResponse::Raw { status, .. }) => {
            panic!("expected buffered JSON, got raw HTTP {status}")
        }
        Err(error) => panic!("live Qwen request failed: {error}"),
    }
}

#[tokio::test]
async fn live_qwen_marker_and_trailing_user_query() {
    let Some(key) = live_key() else {
        eprintln!("skip live Qwen: VELLUM_LIVE_E806_KEY is unset");
        return;
    };
    let runtime = runtime(&key);

    let simple = execute_json(
        &runtime,
        json!({
            "model": CATALOG_ID,
            "stream": false,
            "max_output_tokens": 512,
            "input": "Reply with exactly VELLUM_QWEN_OK and nothing else."
        }),
    )
    .await;
    let simple_text = visible_text(&simple);
    assert_eq!(
        simple.get("status").and_then(Value::as_str),
        Some("completed"),
        "{simple}"
    );
    assert!(
        simple_text.contains("VELLUM_QWEN_OK"),
        "simple Qwen marker missing from {simple_text} / {simple}"
    );

    let trailing = execute_json(
        &runtime,
        json!({
            "model": CATALOG_ID,
            "stream": false,
            "max_output_tokens": 512,
            "tools": [{
                "type": "function",
                "name": "shell_command",
                "description": "Run a shell command",
                "parameters": {"type": "object", "properties": {}}
            }],
            "input": [
                {"type": "message", "role": "user", "content": "Reply with exactly VELLUM_QWEN_TRAILING_OK and nothing else."},
                {"type": "function_call", "call_id": "call_1", "name": "shell_command", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
            ]
        }),
    )
    .await;
    let trailing_text = visible_text(&trailing);
    let error = trailing
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        !error.to_ascii_lowercase().contains("no user query"),
        "trailing-user Qwen still rejected: {error}"
    );
    assert_eq!(
        trailing.get("status").and_then(Value::as_str),
        Some("completed"),
        "{trailing}"
    );
    assert!(
        trailing_text.contains("VELLUM_QWEN_TRAILING_OK"),
        "trailing Qwen marker missing from {trailing_text} / {trailing}"
    );

    // Codex 0.153 native multi-agent V2 starts a child with an
    // `agent_message`, not an ordinary user `message`. This is the production
    // shape captured from child thread
    // 01a06b6f-013c-7a80-85ed-4ff62cd57c18. It must survive the complete
    // Responses -> Qwen Chat route as semantic input.
    let child_assignment = execute_json(
        &runtime,
        json!({
            "model": CATALOG_ID,
            "stream": false,
            "max_output_tokens": 512,
            "instructions": "Follow the assignment from the parent agent.",
            "input": [{
                "type": "agent_message",
                "author": "/root",
                "recipient": "/root/explore_project",
                "content": [
                    {
                        "type": "input_text",
                        "text": "Message Type: NEW_TASK\nTask name: explore_project"
                    },
                    {
                        "type": "encrypted_content",
                        "encrypted_content": "Reply with exactly VELLUM_QWEN_SUBAGENT_OK and nothing else."
                    }
                ]
            }]
        }),
    )
    .await;
    let child_text = visible_text(&child_assignment);
    assert_eq!(
        child_assignment.get("status").and_then(Value::as_str),
        Some("completed"),
        "{child_assignment}"
    );
    assert!(
        child_text.contains("VELLUM_QWEN_SUBAGENT_OK"),
        "native subagent assignment did not reach Qwen: {child_text} / {child_assignment}"
    );
    assert!(
        !child_text.contains("omitted from history"),
        "native subagent assignment was replaced with a fallback marker: {child_text}"
    );
}
