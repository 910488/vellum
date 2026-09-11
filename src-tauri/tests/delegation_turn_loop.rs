//! End-to-end proof of the Phase 8 delegation runtime (issue #6).
//!
//! Delegation is opted into through `VELLUM_DELEGATION_VERIFIED`, which the
//! harness reads from the process environment. Environment variables are
//! process-global, so this lives in its own integration binary.
//!
//! What it proves: a `multi_agent__spawn` call is executed by Vellum, not by
//! Codex — the child runs as its own provider conversation with its own model
//! and effort, and the parent turn continues with the result. Codex never sees
//! the delegation call, because it has no runtime for one.

use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use vellum_lib::model::{AuthKind, CreateRouteInput, ProviderKind, WireFormat};
use vellum_lib::state::AppState;

fn response_from_sse(body: &str) -> Value {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
        .find(|event| event.get("type").and_then(Value::as_str) == Some("response.completed"))
        .and_then(|event| event.get("response").cloned())
        .unwrap_or_else(|| panic!("SSE response had no response.completed: {body}"))
}

/// No enabled route may address anything but loopback. `AppState` seeds a real
/// Grok route, and an earlier test in this repo reached it for real.
fn assert_only_loopback_routes(state: &AppState) {
    for route in state.routes().into_iter().filter(|route| route.enabled) {
        let parsed = reqwest::Url::parse(&route.base_url)
            .unwrap_or_else(|error| panic!("route `{}`: {error}", route.name));
        let host = parsed.host_str().unwrap_or_default();
        assert!(
            matches!(host, "127.0.0.1" | "localhost" | "::1"),
            "route `{}` resolves to `{host}`; tests must never leave loopback",
            route.name
        );
    }
}

#[tokio::test]
#[ignore = "M10 production contract is SingleAgent until remote delegation is qualified"]
async fn a_spawned_child_runs_upstream_and_the_parent_continues_with_its_result() {
    std::env::set_var("VELLUM_DELEGATION_VERIFIED", "1");
    // macOS CI/SSH sessions cannot display the Keychain authorization UI.
    // This opt-in is accepted only by debug builds and fresh test roots.
    std::env::set_var("VELLUM_TEST_FILE_KEY", "integration-only");

    let upstream_calls = Arc::new(AtomicUsize::new(0));
    let upstream_requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let counter = Arc::clone(&upstream_calls);
    let captured = Arc::clone(&upstream_requests);
    // The catalog id is generated at route creation, so the mock provider is
    // told which model to delegate to once it is known.
    let child_target = Arc::new(Mutex::new(String::new()));
    let target_for_handler = Arc::clone(&child_target);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let handler = move |Json(request): Json<Value>| {
        let counter = Arc::clone(&counter);
        let captured = Arc::clone(&captured);
        let target = Arc::clone(&target_for_handler);
        async move {
            captured.lock().unwrap().push(request.clone());
            let index = counter.fetch_add(1, Ordering::SeqCst);
            // Turn 1: the parent delegates. Turn 2: the child answers. Turn 3:
            // the parent continues with the child's result.
            let output = match index {
                0 => json!([{
                    "type": "function_call",
                    "name": "multi_agent__spawn",
                    "call_id": "call_spawn_1",
                    "arguments": json!({
                        "model": target.lock().unwrap().clone(),
                        "instructions": "count the untranslated keys"
                    }).to_string()
                }]),
                1 => json!([{
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "12 keys are untranslated"}]
                }]),
                _ => json!([
                    {
                        "type": "reasoning",
                        "id": "rs_provider_private",
                        "encrypted_content": "opaque-secret",
                        "summary": [{"type": "summary_text", "text": "Used the child result."}]
                    },
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "the child found 12"}]
                    }
                ]),
            };
            Json(json!({
                "id": format!("resp_{index}"),
                "object": "response",
                "status": "completed",
                "output": output,
                "usage": {"input_tokens": 10, "output_tokens": 4, "total_tokens": 14}
            }))
        }
    };
    let upstream = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            Router::new().route("/v1/responses", post(handler)),
        )
        .await;
    });

    let temp = tempfile::tempdir().unwrap();
    // Route visibility must not depend on the developer machine already
    // having Grok Build installed. A zero-byte executable is sufficient for
    // the installation check; all traffic in this test goes to the loopback
    // mock provider and never executes the binary.
    let grok_home = temp.path().join("grok-home");
    let grok_bin = grok_home.join("bin");
    let grok_binary = if cfg!(windows) { "grok.exe" } else { "grok" };
    std::fs::create_dir_all(&grok_bin).unwrap();
    std::fs::write(grok_bin.join(grok_binary), []).unwrap();
    std::fs::write(
        grok_home.join("auth.json"),
        r#"{"accounts":[{"session":{"access_token":"integration-test-token","user_id":"integration-test"}}]}"#,
    )
    .unwrap();
    std::env::set_var("GROK_HOME", &grok_home);
    let state = AppState::with_data_dir(temp.path().to_path_buf());
    let routes = state.create_route(
        CreateRouteInput {
            name: "Grok Delegation".into(),
            base_url: format!("http://{address}/v1"),
            model: "grok-parent".into(),
            wire: WireFormat::Responses,
            streaming: false,
            reasoning: false,
            server_side_resume: false,
            provider_kind: Some(ProviderKind::GrokCli),
            api_key: None,
            models: Some(vec!["grok-parent".into(), "grok-child".into()]),
            selected_models: None,
            context_window: Some(128_000),
            catalog_scope: None,
            model_capabilities: Vec::new(),
        },
        ProviderKind::GrokCli,
        AuthKind::None,
    );
    let route_id = routes
        .iter()
        .find(|route| route.name == "Grok Delegation")
        .unwrap()
        .id
        .clone();
    for route in &routes {
        if route.id != route_id {
            state.set_route_enabled(&route.id, false);
        }
    }
    state.activate_proxy_routes();
    assert_only_loopback_routes(&state);

    let models = state.active_model_routes();
    let parent = models
        .iter()
        .find(|model| model.upstream_model == "grok-parent")
        .unwrap();
    *child_target.lock().unwrap() = parent.catalog_id.clone();

    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_address = proxy_listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let proxy_state = state.clone();
    let proxy_server = tokio::spawn(async move {
        vellum_lib::proxy::serve_local_proxy(
            proxy_state,
            proxy_listener,
            test_boundary_key(),
            shutdown_rx,
        )
        .await
        .unwrap();
    });

    let response = boundary_client()
        .post(format!("http://{proxy_address}/v1/responses"))
        .header("x-vellum-grok-session-id", "delegation-e2e")
        .json(&json!({
            "model": parent.catalog_id,
            "input": [{"role": "user", "content": "how many keys need translation?"}],
            "stream": true,
            "tools": [{"type": "shell"}]
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let text = response.text().await.unwrap();

    assert!(status.is_success(), "turn failed: {status} {text}");
    let body: Value = if content_type.contains("text/event-stream") {
        response_from_sse(&text)
    } else {
        serde_json::from_str(&text).unwrap()
    };
    assert!(
        !body.to_string().contains("multi_agent__spawn"),
        "Codex has no delegation runtime and must never receive the call"
    );
    assert!(
        !body.to_string().contains("opaque-secret")
            && !body.to_string().contains("encrypted_content"),
        "a delegated continuation must receive the same readable-replay normalization as the initial turn"
    );

    let requests = upstream_requests.lock().unwrap().clone();
    assert!(
        requests.len() >= 2,
        "the parent turn must continue after the delegation call, saw {}",
        requests.len()
    );
    assert!(
        requests.iter().all(|request| request["stream"] == false),
        "parent, child, and continuation must be buffered before Vellum intercepts them"
    );
    // The continuation carries the delegation exchange as a paired call/output.
    let continuation = requests.last().unwrap();
    let input = continuation["input"].as_array().unwrap();
    let calls = input
        .iter()
        .filter(|item| item.get("call_id").and_then(Value::as_str) == Some("call_spawn_1"))
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2, "one call and one output for the delegation");
    let answer = calls
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    let payload: Value = serde_json::from_str(answer["output"].as_str().unwrap()).unwrap();
    // The child really ran: it has an identity, a state, and its own cost.
    assert_eq!(
        payload["state"], "completed",
        "child did not complete: {payload}"
    );
    assert_eq!(payload["model"], parent.catalog_id);
    assert_eq!(payload["upstreamModel"], "grok-parent");
    assert!(
        payload["inputTokens"].as_u64().unwrap() > 0,
        "child usage must be attributed to the child: {payload}"
    );

    // Three provider turns: the parent delegating, the child, the continuation.
    assert_eq!(upstream_calls.load(Ordering::SeqCst), 3);
    // The child ran as its own conversation, with no tools and its own prompt.
    let child_turn = &requests[1];
    assert!(child_turn.get("tools").is_none(), "a child gets no tools");
    assert!(child_turn["instructions"]
        .as_str()
        .unwrap()
        .contains("delegated sub-agent"));
    assert_eq!(
        child_turn["input"][0]["content"],
        "count the untranslated keys"
    );

    // Every real provider request is durable, and the child remains separately
    // attributable instead of being folded into the parent's final turn.
    let summary = state.usage_store().summary(&route_id).unwrap();
    assert_eq!(summary.turns, 3, "all three provider turns must be counted");
    assert_eq!(summary.total_tokens, 42);
    let attribution = state.usage_store().harness_attribution().unwrap();
    assert_eq!(attribution.len(), 3);
    assert_eq!(
        attribution
            .iter()
            .map(|row| row.role.as_str())
            .collect::<Vec<_>>(),
        vec!["parent", "child", "parent"]
    );
    assert!(attribution
        .iter()
        .all(|row| row.session_hash.starts_with("sha256:")
            && !row.session_hash.contains("vellum-child")
            && !row.agent_id.contains("delegation-e2e")
            && row
                .parent_agent_id
                .as_deref()
                .is_none_or(|parent| !parent.contains("delegation-e2e"))));

    let _ = shutdown_tx.send(());
    let _ = proxy_server.await;
    upstream.abort();
    std::env::remove_var("VELLUM_DELEGATION_VERIFIED");
}

/// A fixed boundary key for these tests. The proxy refuses every request that
/// does not present one, so the harness has to speak the same protocol Codex
/// does through its managed provider header — which keeps the guard itself
/// under test here rather than bypassed.
const TEST_BOUNDARY_KEY: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

fn test_boundary_key() -> vellum_proxy_runtime::BoundaryKey {
    vellum_proxy_runtime::BoundaryKey::parse(TEST_BOUNDARY_KEY).expect("valid test key")
}

/// A client that presents the boundary key on every request.
fn boundary_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        vellum_proxy_runtime::BOUNDARY_KEY_HEADER,
        reqwest::header::HeaderValue::from_static(TEST_BOUNDARY_KEY),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .expect("client builds")
}
