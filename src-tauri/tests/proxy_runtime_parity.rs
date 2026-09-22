//! M1 behavioral-parity harness (Vellum Proxy Runtime extraction plan).
//!
//! Golden-file baselines are frozen from the Desktop pipeline and must stay
//! byte-identical; since M3C each non-streaming fixture is also a *live
//! differential*: the same scripted fake upstream drives both Desktop's
//! `serve_local_proxy` and the shared runtime's headless `/v1/responses`, and
//! the two normalized outputs must match exactly. That is the point of the
//! extraction — one execution engine, no second implementation to drift.
//!
//! The M4 continuation fixtures (`responses_previous_response_id`,
//! `responses_store_false`, `responses_encrypted_reasoning`) freeze the
//! Desktop two-turn contract for third-party routes: the client sends
//! `previous_response_id`, Desktop resolves PortableSemanticReplay, rewrites
//! the body (chain replayed into `input`, `previous_response_id` stripped,
//! opaque reasoning never replayed upstream), and records the exchange for
//! the next turn. The shared runtime now owns the same durable history +
//! hydration path, so these fixtures are *true upstream hydration
//! differentials*: the runtime's captured turn-2 upstream request must equal
//! Desktop's captured turn-2 upstream request (after `normalize()`), not
//! just the client-facing responses. `stream_basic`, `stream_tool_call` and
//! `stream_reasoning` freeze the full Desktop third-party streaming contract
//! the same way: every client-visible event type is asserted in wire order,
//! key payload shapes are asserted structurally (tool-call arguments
//! accumulation, reasoning summary retention, message phases), and the
//! completed response is frozen as a golden baseline. Since the M5 streaming
//! migration the runtime serves real SSE, so those three fixtures are *live
//! SSE differentials*: the runtime's client-visible event sequence must equal
//! Desktop's (after `normalize()`).

use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use vellum_lib::model::{
    AuthKind, CreateRouteInput, ProviderKind, ReviewPolicy, ReviewSettings, WireFormat,
};
use vellum_lib::state::AppState;
use vellum_proxy_runtime::config::{ProxyRuntimeConfig, ProxyRuntimeIdentity, RuntimeRouteConfig};
use vellum_proxy_runtime::environment::ExecutionEnvironment;
use vellum_proxy_runtime::route::{RuntimeAuthKind, RuntimeProviderKind, RuntimeWireFormat};
use vellum_proxy_runtime::usage::usage_details_from_response;
use vellum_proxy_runtime::{serve_proxy, ProxyRuntimeState, StaticProxyState};
use vellum_proxy_testkit::{FakeUpstream, ScriptedTurn};

/// Strips exactly the fields the plan allows stripping as run-to-run noise:
/// generated response ids (`id` starting with `resp_`), random local
/// compaction ids (`id` starting with `cmp_`, e.g. `cmp_vellum_<random>`),
/// `request_id`,
/// timestamp fields (`created_at`/`created`), and `session_id`. Everything
/// else — event order, tool call ids, reasoning, usage, status, error
/// semantics, compaction output — must compare byte-for-byte, so a real
/// regression cannot hide behind an overly aggressive normalizer.
fn normalize(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let strip_id = matches!(
                map.get("id"),
                Some(Value::String(id))
                    if id.starts_with("resp_") || id.starts_with("cmp_")
            );
            if strip_id {
                map.remove("id");
            }
            map.remove("request_id");
            map.remove("created_at");
            map.remove("created");
            map.remove("session_id");
            for child in map.values_mut() {
                normalize(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize(item);
            }
        }
        // Local compaction envelopes embed the random `cmp_vellum_<hex>` id
        // inside `encrypted_content`; canonicalize the generated part so the
        // frozen golden stays deterministic while the
        // `vcompact.codex0150.` envelope and the item shape remain
        // byte-for-byte oracles.
        Value::String(text) if text.starts_with("vcompact.codex0150.cmp_local_") => {
            *text = "vcompact.codex0150.cmp_local_<random>".to_string();
        }
        _ => {}
    }
}

fn assert_matches_baseline(fixture_name: &str, actual_normalized: &Value) {
    let path = format!("tests/fixtures/proxy_runtime_parity/{fixture_name}.json");
    if std::env::var("VELLUM_PARITY_RECORD").is_ok() {
        std::fs::create_dir_all("tests/fixtures/proxy_runtime_parity").unwrap();
        std::fs::write(
            &path,
            serde_json::to_string_pretty(actual_normalized).unwrap(),
        )
        .unwrap();
        return;
    }
    let expected: Value = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap(),
        Err(_) => panic!(
            "no baseline at {path}; run with VELLUM_PARITY_RECORD=1 to record one, then re-run without it to confirm determinism"
        ),
    };
    assert_eq!(
        actual_normalized, &expected,
        "fixture `{fixture_name}` drifted from its frozen Desktop baseline"
    );
}

fn response_from_sse(body: &str) -> Value {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
        .find(|event| event.get("type").and_then(Value::as_str) == Some("response.completed"))
        .and_then(|event| event.get("response").cloned())
        .unwrap_or_else(|| panic!("SSE response had no response.completed: {body}"))
}

fn event_types_from_sse(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
        .filter_map(|event| {
            event
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// Parses a client-visible SSE body into `(event name, data)` pairs, keeping
/// every protocol event in wire order. The event name comes from the `event:`
/// line; blocks without a `data:` line are skipped.
fn sse_events(body: &str) -> Vec<(String, Value)> {
    let mut events = Vec::new();
    for block in body.split("\n\n") {
        let mut event = "message".to_string();
        let mut data = String::new();
        for line in block.lines() {
            if let Some(name) = line.strip_prefix("event: ") {
                event = name.to_string();
            } else if let Some(payload) = line.strip_prefix("data: ") {
                data = payload.to_string();
            }
        }
        if !data.is_empty() {
            let value: Value = serde_json::from_str(&data)
                .unwrap_or_else(|error| panic!("SSE data is not JSON ({error}): {data}"));
            events.push((event, value));
        }
    }
    events
}

fn assistant_tool_call_message(request: &Value) -> &Value {
    request["messages"]
        .as_array()
        .expect("Chat request messages")
        .iter()
        .find(|message| message.get("tool_calls").is_some())
        .expect("assistant tool-call message")
}

/// M5 streaming differential: the shared runtime must reproduce the exact
/// client-visible SSE event sequence Desktop produced (after `normalize()`),
/// including tool-call argument deltas and reasoning summary retention. The
/// comparison authority is the full event list, not just the final text.
async fn assert_stream_differential(
    fixture_name: &str,
    script: Vec<ScriptedTurn>,
    wire: RuntimeWireFormat,
    request_body: Value,
    desktop_sse: &str,
) {
    let harness = start_runtime(script, wire).await;
    let response = boundary_client()
        .post(&harness.address)
        .json(&request_body)
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
    let runtime_sse = response.text().await.unwrap();
    let _ = harness.shutdown.send(());
    let _ = harness.server.await;
    assert!(
        status.is_success(),
        "fixture `{fixture_name}`: runtime stream returned {status}: {runtime_sse}"
    );
    assert!(
        content_type.contains("text/event-stream"),
        "fixture `{fixture_name}`: expected text/event-stream, got `{content_type}`"
    );
    let mut desktop_events = sse_events(desktop_sse);
    let mut runtime_events = sse_events(&runtime_sse);
    for (_, data) in &mut desktop_events {
        normalize(data);
    }
    for (_, data) in &mut runtime_events {
        normalize(data);
    }
    assert_eq!(
        runtime_events, desktop_events,
        "fixture `{fixture_name}`: the shared runtime's SSE stream diverged from Desktop's"
    );
}

struct Fixture {
    #[allow(dead_code)]
    state: AppState,
    upstream: FakeUpstream,
    proxy_address: std::net::SocketAddr,
    catalog_id: String,
    script: Vec<ScriptedTurn>,
    shutdown: oneshot::Sender<()>,
    proxy_server: JoinHandle<()>,
    #[allow(dead_code)]
    temp: TempDir,
}

impl Fixture {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.proxy_address)
    }

    async fn teardown(self) {
        let _ = self.shutdown.send(());
        let _ = self.proxy_server.await;
    }
}

async fn setup(wire: WireFormat, script: Vec<ScriptedTurn>) -> Fixture {
    setup_with_reasoning(wire, script, true).await
}

async fn setup_with_reasoning(
    wire: WireFormat,
    script: Vec<ScriptedTurn>,
    reasoning: bool,
) -> Fixture {
    let upstream = FakeUpstream::start(script.clone()).await;
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::with_data_dir(temp.path().to_path_buf());

    let routes = state.create_route(
        CreateRouteInput {
            name: "Parity Test Provider".into(),
            base_url: upstream.base_url(),
            model: "test-model".into(),
            wire,
            streaming: true,
            reasoning,
            server_side_resume: false,
            provider_kind: Some(ProviderKind::OpenAiCompatible),
            api_key: None,
            models: Some(vec!["test-model".into()]),
            selected_models: None,
            context_window: Some(128_000),
            catalog_scope: None,
            model_capabilities: Vec::new(),
        },
        ProviderKind::OpenAiCompatible,
        AuthKind::None,
    );
    let route_id = routes
        .iter()
        .find(|route| route.name == "Parity Test Provider")
        .unwrap()
        .id
        .clone();
    for route in &routes {
        if route.id != route_id {
            state.set_route_enabled(&route.id, false);
        }
    }
    state.activate_proxy_routes();

    let models = state.active_model_routes();
    let catalog_id = models
        .iter()
        .find(|model| model.upstream_model == "test-model")
        .unwrap()
        .catalog_id
        .clone();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let proxy_state = state.clone();
    let proxy_server = tokio::spawn(async move {
        vellum_lib::proxy::serve_local_proxy(
            proxy_state,
            listener,
            test_boundary_key(),
            shutdown_rx,
        )
        .await
        .unwrap();
    });

    Fixture {
        state,
        upstream,
        proxy_address,
        catalog_id,
        script,
        shutdown: shutdown_tx,
        proxy_server,
        temp,
    }
}

/// The runtime catalog id the headless side of a differential uses.
const RUNTIME_CATALOG_ID: &str = "parity-model";

struct RuntimeHarness {
    address: String,
    upstream: FakeUpstream,
    state: Arc<StaticProxyState>,
    shutdown: oneshot::Sender<()>,
    server: JoinHandle<()>,
    _data: tempfile::TempDir,
}

/// Start the *shared runtime* headless `/v1/responses` against a fresh fake
/// upstream serving `script`. The route mirrors the Desktop fixture route
/// (OpenAI-compatible, no auth, reasoning on, same upstream model) so the
/// only difference between the two paths is the implementation under test.
async fn start_runtime(script: Vec<ScriptedTurn>, wire: RuntimeWireFormat) -> RuntimeHarness {
    let upstream = FakeUpstream::start(script).await;
    let data = tempfile::tempdir().unwrap();
    let mut config = ProxyRuntimeConfig {
        schema_version: 2,
        identity: ProxyRuntimeIdentity {
            install_id: "parity".into(),
            host_id: "parity".into(),
            image_version: "0.1.0".into(),
            config_hash: "parity".into(),
            ..Default::default()
        },
        models: vec![RuntimeRouteConfig {
            route_id: "parity".into(),
            catalog_id: RUNTIME_CATALOG_ID.into(),
            name: "Parity Runtime Provider".into(),
            base_url: upstream.base_url(),
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            auth_kind: RuntimeAuthKind::None,
            wire,
            server_side_resume: false,
            streaming: true,
            reasoning: true,
            vision: false,
            upstream_model: "test-model".into(),
            context_window: Some(128_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: Default::default(),
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: None,
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        }],
        ..ProxyRuntimeConfig::default()
    };
    // The parity fixtures were recorded against the explicit Linux/Bash
    // reference contract. Say so in the config instead of inheriting a
    // default: the differential must never accidentally change meaning when
    // the executor defaults move.
    config.execution_environment = ExecutionEnvironment::posix_reference();
    config.data_dir = data.path().join("data");
    config.history_dir = data.path().join("history");
    config.log_dir = data.path().join("logs");
    let state = StaticProxyState::from_config(config).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let state = Arc::new(state);
    let policy = test_access_policy(address.port());
    let server = tokio::spawn({
        let state = Arc::clone(&state);
        async move {
            let _ = serve_proxy(state, listener, policy, shutdown_rx).await;
        }
    });
    RuntimeHarness {
        address: format!("http://{address}/v1/responses"),
        upstream,
        state,
        shutdown: shutdown_tx,
        server,
        _data: data,
    }
}

/// Run the *shared runtime* headless `/v1/responses` against its own fake
/// upstream serving `script`, returning the raw status and body.
async fn runtime_responses(
    script: Vec<ScriptedTurn>,
    wire: RuntimeWireFormat,
    request_body: Value,
) -> (u16, Value) {
    let harness = start_runtime(script, wire).await;
    let response = boundary_client()
        .post(&harness.address)
        .json(&request_body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body: Value = response.json().await.unwrap();
    let _ = harness.shutdown.send(());
    let _ = harness.server.await;
    (status, body)
}

/// Two sequential turns against one shared-runtime instance, mirroring the
/// Desktop two-turn continuation fixtures. The runtime records each exchange
/// in its own history store, so turn 2 hydrates the chain exactly like
/// Desktop. Returns both turns' client-facing bodies plus the runtime's
/// captured upstream requests (for the upstream-request differential).
async fn runtime_responses_two_turns(
    script: Vec<ScriptedTurn>,
    wire: RuntimeWireFormat,
    first_request: Value,
    second_request: Value,
) -> ((u16, Value), (u16, Value), Vec<Value>) {
    let harness = start_runtime(script, wire).await;
    let client = boundary_client();
    let first = client
        .post(&harness.address)
        .json(&first_request)
        .send()
        .await
        .unwrap();
    let first_status = first.status().as_u16();
    let first_body: Value = first.json().await.unwrap();
    let second = client
        .post(&harness.address)
        .json(&second_request)
        .send()
        .await
        .unwrap();
    let second_status = second.status().as_u16();
    let second_body: Value = second.json().await.unwrap();
    let captured_upstream = harness.upstream.captured_requests();
    let _ = harness.shutdown.send(());
    let _ = harness.server.await;
    (
        (first_status, first_body),
        (second_status, second_body),
        captured_upstream,
    )
}

/// Drive the shared runtime against the same script the Desktop fixture used
/// and assert the normalized outputs are byte-identical. `request_body` must
/// already reference `RUNTIME_CATALOG_ID`.
async fn assert_runtime_differential(
    fixture: &Fixture,
    wire: RuntimeWireFormat,
    desktop_normalized: &Value,
    request_body: Value,
    fixture_name: &str,
) {
    let (_status, mut runtime_body) =
        runtime_responses(fixture.script.clone(), wire, request_body).await;
    normalize(&mut runtime_body);
    assert_eq!(
        runtime_body, *desktop_normalized,
        "fixture `{fixture_name}`: the shared runtime's /v1/responses diverged from Desktop's"
    );
}

/// One turn against the Desktop fixture, returning the raw status and body.
/// The two-turn continuation fixtures drive each turn through the shared
/// Desktop pipeline (the history DB persists between turns) and freeze the
/// upstream-request contract the runtime differential will eventually match.
async fn desktop_turn(fixture: &Fixture, request_body: Value) -> (u16, Value) {
    let response = boundary_client()
        .post(fixture.url("/v1/responses"))
        .json(&request_body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert!(status.is_success(), "unexpected status {status}: {body}");
    (status.as_u16(), body)
}

/// Upstream hydration differential for the M4 continuation fixtures: both
/// turns against the shared runtime must match Desktop's normalized
/// outputs, and the runtime's captured turn-2 upstream request must equal
/// Desktop's captured turn-2 upstream request (after `normalize()`). That
/// is the M4 acceptance — the runtime rewrites the continuation body exactly
/// like Desktop instead of merely echoing the same client-facing response.
async fn assert_runtime_differential_two_turns(
    fixture: &Fixture,
    wire: RuntimeWireFormat,
    desktop_first_normalized: &Value,
    desktop_second_normalized: &Value,
    first_request: Value,
    second_request: Value,
    fixture_name: &str,
) {
    let ((_, mut runtime_first), (_, mut runtime_second), runtime_upstream) =
        runtime_responses_two_turns(fixture.script.clone(), wire, first_request, second_request)
            .await;
    normalize(&mut runtime_first);
    normalize(&mut runtime_second);
    assert_eq!(
        runtime_first, *desktop_first_normalized,
        "fixture `{fixture_name}`: turn 1 of the shared runtime's /v1/responses diverged from Desktop's"
    );
    assert_eq!(
        runtime_second, *desktop_second_normalized,
        "fixture `{fixture_name}`: turn 2 of the shared runtime's /v1/responses diverged from Desktop's"
    );

    // M4 upstream-request differential: turn 2 must reach the upstream with
    // the same rewritten body Desktop produced (chain replayed into `input`,
    // `previous_response_id` stripped, model/store passthrough preserved).
    let desktop_upstream = fixture.upstream.captured_requests();
    assert!(
        desktop_upstream.len() >= 2 && runtime_upstream.len() >= 2,
        "fixture `{fixture_name}`: both sides must capture two upstream turns \
         (desktop {}, runtime {})",
        desktop_upstream.len(),
        runtime_upstream.len()
    );
    let mut desktop_turn_2_upstream = desktop_upstream[1].clone();
    let mut runtime_turn_2_upstream = runtime_upstream[1].clone();
    normalize(&mut desktop_turn_2_upstream);
    normalize(&mut runtime_turn_2_upstream);
    assert_eq!(
        runtime_turn_2_upstream, desktop_turn_2_upstream,
        "fixture `{fixture_name}`: the shared runtime's turn-2 upstream request diverged from Desktop's"
    );
}

/// Differential for the provider-error fixtures. The response normalization
/// path does not run (there is no success body to normalize), so parity here
/// means: same success/error class, the same `vellum_proxy_error` envelope,
/// and the same error `code` when both sides echo the upstream status. The
/// one documented divergence is 5xx: Desktop echoes the upstream status (500)
/// while the runtime's taxonomy maps every 5xx to 503 ProviderUnavailable.
async fn assert_error_differential(
    fixture: &Fixture,
    desktop_status: u16,
    desktop_body: &Value,
    request_body: Value,
    fixture_name: &str,
) {
    let (runtime_status, mut runtime_body) = runtime_responses(
        fixture.script.clone(),
        RuntimeWireFormat::Responses,
        request_body,
    )
    .await;
    normalize(&mut runtime_body);
    assert_eq!(
        desktop_status < 400,
        runtime_status < 400,
        "fixture `{fixture_name}`: success/error class diverged (desktop {desktop_status}, runtime {runtime_status})"
    );
    assert_eq!(
        runtime_body["error"]["type"], desktop_body["error"]["type"],
        "fixture `{fixture_name}`: error envelope family diverged"
    );
    if desktop_body["error"]["code"].as_u64() < Some(500) {
        assert_eq!(
            runtime_body["error"]["code"], desktop_body["error"]["code"],
            "fixture `{fixture_name}`: error code diverged"
        );
    }
}

#[tokio::test]
async fn responses_basic() {
    let fixture = setup(
        WireFormat::Responses,
        vec![ScriptedTurn::Json(json!({
            "id": "resp_1",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "Hello there", "annotations": []}]
            }],
            "usage": {"input_tokens": 5, "output_tokens": 3, "total_tokens": 8}
        }))],
    )
    .await;

    let response = boundary_client()
        .post(fixture.url("/v1/responses"))
        .json(&json!({
            "model": fixture.catalog_id,
            "input": [{"role": "user", "content": "hello"}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let mut body: Value = response.json().await.unwrap();
    assert!(status.is_success(), "unexpected status {status}: {body}");
    assert_eq!(fixture.upstream.call_count(), 1);

    normalize(&mut body);
    assert_matches_baseline("responses_basic", &body);
    assert_runtime_differential(
        &fixture,
        RuntimeWireFormat::Responses,
        &body,
        json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "hello"}],
            "stream": false
        }),
        "responses_basic",
    )
    .await;
    fixture.teardown().await;
}

#[tokio::test]
async fn responses_reasoning() {
    let fixture = setup(
        WireFormat::Responses,
        vec![ScriptedTurn::Json(json!({
            "id": "resp_2",
            "object": "response",
            "status": "completed",
            "output": [
                {
                    "type": "reasoning",
                    "id": "rs_provider_private",
                    "encrypted_content": "opaque-secret",
                    "summary": [{"type": "summary_text", "text": "Thought about the answer."}]
                },
                {
                    "type": "message",
                    "id": "msg_2",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "The answer is 42.", "annotations": []}]
                }
            ],
            "usage": {"input_tokens": 6, "output_tokens": 4, "total_tokens": 10}
        }))],
    )
    .await;

    let response = boundary_client()
        .post(fixture.url("/v1/responses"))
        .json(&json!({
            "model": fixture.catalog_id,
            "input": [{"role": "user", "content": "what is the answer?"}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let mut body: Value = response.json().await.unwrap();
    assert!(status.is_success(), "unexpected status {status}: {body}");
    let text = body.to_string();
    assert!(
        !text.contains("opaque-secret") && !text.contains("encrypted_content"),
        "provider-private reasoning payload must never reach the client: {body}"
    );

    normalize(&mut body);
    assert_matches_baseline("responses_reasoning", &body);
    assert_runtime_differential(
        &fixture,
        RuntimeWireFormat::Responses,
        &body,
        json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "what is the answer?"}],
            "stream": false
        }),
        "responses_reasoning",
    )
    .await;
    fixture.teardown().await;
}

#[tokio::test]
async fn responses_tool_call() {
    let fixture = setup(
        WireFormat::Responses,
        vec![ScriptedTurn::Json(json!({
            "id": "resp_3",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_abc123",
                "name": "get_weather",
                "arguments": "{\"city\":\"Taipei\"}",
                "status": "completed"
            }],
            "usage": {"input_tokens": 7, "output_tokens": 2, "total_tokens": 9}
        }))],
    )
    .await;

    let response = boundary_client()
        .post(fixture.url("/v1/responses"))
        .json(&json!({
            "model": fixture.catalog_id,
            "input": [{"role": "user", "content": "what's the weather in Taipei?"}],
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "description": "Get the weather",
                "parameters": {"type": "object"}
            }],
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let mut body: Value = response.json().await.unwrap();
    assert!(status.is_success(), "unexpected status {status}: {body}");
    let call = body["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        .unwrap_or_else(|| panic!("no function_call in output: {body}"));
    assert_eq!(call["call_id"], "call_abc123");
    assert_eq!(call["name"], "get_weather");
    assert_eq!(call["arguments"], "{\"city\":\"Taipei\"}");

    normalize(&mut body);
    assert_matches_baseline("responses_tool_call", &body);
    assert_runtime_differential(
        &fixture,
        RuntimeWireFormat::Responses,
        &body,
        json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "what's the weather in Taipei?"}],
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "description": "Get the weather",
                "parameters": {"type": "object"}
            }],
            "stream": false
        }),
        "responses_tool_call",
    )
    .await;
    fixture.teardown().await;
}

#[tokio::test]
async fn responses_multi_tool() {
    let fixture = setup(
        WireFormat::Responses,
        vec![ScriptedTurn::Json(json!({
            "id": "resp_4",
            "object": "response",
            "status": "completed",
            "output": [
                {
                    "type": "function_call",
                    "id": "fc_2",
                    "call_id": "call_weather_taipei",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"Taipei\"}",
                    "status": "completed"
                },
                {
                    "type": "function_call",
                    "id": "fc_3",
                    "call_id": "call_weather_tokyo",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"Tokyo\"}",
                    "status": "completed"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_weather_taipei",
                    "output": "{\"temp\":32}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_weather_tokyo",
                    "output": "{\"temp\":28}"
                },
                {
                    "type": "message",
                    "id": "msg_3",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "Both cities are warm today.", "annotations": []}]
                }
            ],
            "usage": {"input_tokens": 18, "output_tokens": 16, "total_tokens": 34}
        }))],
    )
    .await;

    let response = boundary_client()
        .post(fixture.url("/v1/responses"))
        .json(&json!({
            "model": fixture.catalog_id,
            "input": [{"role": "user", "content": "what's the weather in Taipei and Tokyo?"}],
            "tools": [
                {
                    "type": "function",
                    "name": "get_weather",
                    "description": "Get the weather",
                    "parameters": {"type": "object"}
                }
            ],
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let mut body: Value = response.json().await.unwrap();
    assert!(status.is_success(), "unexpected status {status}: {body}");

    let calls: Vec<&Value> = body["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        .collect();
    assert_eq!(calls.len(), 2, "expected both parallel tool calls: {body}");
    assert_eq!(calls[0]["call_id"], "call_weather_taipei");
    assert_eq!(calls[0]["name"], "get_weather");
    assert_eq!(calls[0]["arguments"], "{\"city\":\"Taipei\"}");
    assert_eq!(calls[1]["call_id"], "call_weather_tokyo");
    assert_eq!(calls[1]["name"], "get_weather");
    assert_eq!(calls[1]["arguments"], "{\"city\":\"Tokyo\"}");

    let outputs: Vec<&Value> = body["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
        .collect();
    assert_eq!(outputs.len(), 2, "expected both tool outputs: {body}");
    assert_eq!(outputs[0]["call_id"], "call_weather_taipei");
    assert_eq!(outputs[1]["call_id"], "call_weather_tokyo");

    normalize(&mut body);
    assert_matches_baseline("responses_multi_tool", &body);
    assert_runtime_differential(
        &fixture,
        RuntimeWireFormat::Responses,
        &body,
        json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "what's the weather in Taipei and Tokyo?"}],
            "tools": [
                {
                    "type": "function",
                    "name": "get_weather",
                    "description": "Get the weather",
                    "parameters": {"type": "object"}
                }
            ],
            "stream": false
        }),
        "responses_multi_tool",
    )
    .await;
    fixture.teardown().await;
}

#[tokio::test]
async fn chat_to_responses() {
    let fixture = setup(
        WireFormat::Chat,
        vec![ScriptedTurn::Json(json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "created": 1_700_000_000,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Hi from the chat adapter."},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 4, "completion_tokens": 6, "total_tokens": 10}
        }))],
    )
    .await;

    let response = boundary_client()
        .post(fixture.url("/v1/responses"))
        .json(&json!({
            "model": fixture.catalog_id,
            "input": [{"role": "user", "content": "hi"}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let mut body: Value = response.json().await.unwrap();
    assert!(status.is_success(), "unexpected status {status}: {body}");
    // Proves the Chat adapter ran: the wire on the way in was Chat Completions
    // JSON, but the client only ever speaks Responses.
    assert_eq!(body["object"], "response");
    assert!(body["output"].as_array().is_some());
    let request = &fixture.upstream.captured_requests()[0];
    assert!(
        request.get("messages").is_some(),
        "the Chat wire request must use `messages`, not `input`: {request}"
    );

    normalize(&mut body);
    assert_matches_baseline("chat_to_responses", &body);
    assert_runtime_differential(
        &fixture,
        RuntimeWireFormat::Chat,
        &body,
        json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "hi"}],
            "stream": false
        }),
        "chat_to_responses",
    )
    .await;
    fixture.teardown().await;
}

/// Full Desktop HTTP regression for the OpenCode Go / DeepSeek failure seen in
/// production. The saved capability snapshot can say `reasoning = false`
/// even though an earlier provider turn emitted readable thinking. When the
/// provider authoritatively rejects the continuation, the running proxy must
/// rebuild it with tool-bound `reasoning_content`, retry once, and finish the
/// same client stream without exposing the intermediate 400.
#[tokio::test]
async fn chat_stream_recovers_reasoning_replay_after_false_negative_probe() {
    let success = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"content": "Recovered."}, "finish_reason": null}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})
    );
    let fixture = setup_with_reasoning(
        WireFormat::Chat,
        vec![
            ScriptedTurn::Status(
                400,
                json!({
                    "error": {
                        "type": "invalid_request_error",
                        "message": "The reasoning_content in the thinking mode must be passed back to the API."
                    }
                }),
            ),
            ScriptedTurn::Sse(vec![success]),
        ],
        false,
    )
    .await;

    let response = boundary_client()
        .post(fixture.url("/v1/responses"))
        .json(&json!({
            "model": fixture.catalog_id,
            "stream": true,
            "tools": [{
                "type": "function",
                "name": "shell",
                "description": "Run a command",
                "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}
            }],
            "input": [
                {"type": "message", "role": "user", "content": "inspect the workspace"},
                {"type": "reasoning", "summary": [{
                    "type": "summary_text",
                    "text": "I need to inspect it first."
                }]},
                {"type": "function_call", "call_id": "call_inspect", "name": "shell", "arguments": "{\"cmd\":\"dir\"}"},
                {"type": "function_call_output", "call_id": "call_inspect", "output": "ok"}
            ]
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let stream = response.text().await.unwrap();
    assert!(status.is_success(), "unexpected status {status}: {stream}");
    assert!(stream.contains("Recovered."), "{stream}");
    assert_eq!(fixture.upstream.call_count(), 2);

    let requests = fixture.upstream.captured_requests();
    assert!(assistant_tool_call_message(&requests[0])
        .get("reasoning_content")
        .is_none());
    assert_eq!(
        assistant_tool_call_message(&requests[1])["reasoning_content"],
        "I need to inspect it first."
    );

    fixture.teardown().await;
}

async fn provider_error_case(fixture_name: &str, status_code: u16) {
    let fixture = setup(
        WireFormat::Responses,
        vec![ScriptedTurn::Status(
            status_code,
            json!({"error": {"message": format!("synthetic upstream {status_code}")}}),
        )],
    )
    .await;

    let response = boundary_client()
        .post(fixture.url("/v1/responses"))
        .json(&json!({
            "model": fixture.catalog_id,
            "input": [{"role": "user", "content": "trigger an error"}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let mut body: Value = response.json().await.unwrap();
    assert_eq!(status.as_u16(), status_code, "body: {body}");
    assert_eq!(fixture.upstream.call_count(), 1);

    normalize(&mut body);
    assert_matches_baseline(fixture_name, &body);
    assert_error_differential(
        &fixture,
        status.as_u16(),
        &body,
        json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "trigger an error"}],
            "stream": false
        }),
        fixture_name,
    )
    .await;
    fixture.teardown().await;
}

#[tokio::test]
async fn provider_401() {
    provider_error_case("provider_401", 401).await;
}

#[tokio::test]
async fn provider_429() {
    provider_error_case("provider_429", 429).await;
}

#[tokio::test]
async fn provider_500() {
    provider_error_case("provider_500", 500).await;
}

#[tokio::test]
async fn invalid_request() {
    let fixture = setup(
        WireFormat::Responses,
        vec![ScriptedTurn::Json(json!({"unused": true}))],
    )
    .await;

    let response = boundary_client()
        .post(fixture.url("/v1/responses"))
        .json(&json!({
            // No `model` field: the request is malformed before any route or
            // upstream is involved.
            "input": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert!(
        status.is_client_error(),
        "expected a 4xx for a request missing `model`, got {status}: {body}"
    );
    assert_eq!(
        fixture.upstream.call_count(),
        0,
        "a malformed request must never reach the upstream"
    );

    // The shared runtime must reject the same model-less request with the
    // same 4xx family and envelope, and never touch its upstream.
    let (runtime_status, runtime_body) = runtime_responses(
        fixture.script.clone(),
        RuntimeWireFormat::Responses,
        json!({"input": [{"role": "user", "content": "hello"}]}),
    )
    .await;
    assert!(
        (400..500).contains(&runtime_status),
        "the shared runtime must reject a model-less request with 4xx, got {runtime_status}: {runtime_body}"
    );
    assert_eq!(runtime_body["error"]["type"], "vellum_proxy_error");

    fixture.teardown().await;
}

#[tokio::test]
async fn zstd_request() {
    let fixture = setup(
        WireFormat::Responses,
        vec![ScriptedTurn::Json(json!({
            "id": "resp_4",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "message",
                "id": "msg_4",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "Hello from zstd", "annotations": []}]
            }],
            "usage": {"input_tokens": 5, "output_tokens": 3, "total_tokens": 8}
        }))],
    )
    .await;

    let payload = json!({
        "model": fixture.catalog_id,
        "input": [{"role": "user", "content": "hello, compressed"}],
        "stream": false
    });
    let compressed =
        zstd::stream::encode_all(serde_json::to_vec(&payload).unwrap().as_slice(), 3).unwrap();

    let response = boundary_client()
        .post(fixture.url("/v1/responses"))
        .header("content-type", "application/json")
        .header("content-encoding", "zstd")
        .body(compressed)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        status.as_u16(),
        415,
        "inbound zstd must be refused before decompress: {status}: {body}"
    );
    assert_eq!(body["error"]["type"], "vellum_proxy_error");
    assert_eq!(body["error"]["category"], "unsupported_media");
    assert_eq!(body["error"]["code"], 415);
    assert_eq!(
        fixture.upstream.call_count(),
        0,
        "a compressed body must never be forwarded upstream"
    );
    fixture.teardown().await;
}

#[tokio::test]
async fn responses_previous_response_id() {
    let fixture = setup(
        WireFormat::Responses,
        vec![
            ScriptedTurn::Json(json!({
                "id": "resp_t1",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": "msg_t1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "First reply", "annotations": []}]
                }],
                "usage": {"input_tokens": 6, "output_tokens": 2, "total_tokens": 8}
            })),
            ScriptedTurn::Json(json!({
                "id": "resp_t2",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": "msg_t2",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "Second reply", "annotations": []}]
                }],
                "usage": {"input_tokens": 9, "output_tokens": 2, "total_tokens": 11}
            })),
        ],
    )
    .await;

    let (_, mut first_body) = desktop_turn(
        &fixture,
        json!({
            "model": fixture.catalog_id,
            "input": [{"role": "user", "content": "first turn"}],
            "stream": false
        }),
    )
    .await;
    let previous_response_id = first_body["id"].clone();
    assert_eq!(previous_response_id, "resp_t1");

    let (_, mut second_body) = desktop_turn(
        &fixture,
        json!({
            "model": fixture.catalog_id,
            "previous_response_id": previous_response_id,
            "input": [{"role": "user", "content": "second turn"}],
            "stream": false
        }),
    )
    .await;
    assert_eq!(fixture.upstream.call_count(), 2);

    // Frozen upstream-request contract for turn 2: the continuation was
    // rewritten locally (PortableSemanticReplay), never handed to the
    // third-party server.
    let upstream = fixture.upstream.captured_requests();
    let turn_2_upstream = &upstream[1];
    assert!(
        turn_2_upstream["previous_response_id"].is_null(),
        "third-party continuation must strip previous_response_id: {turn_2_upstream}"
    );
    assert_eq!(
        turn_2_upstream["model"], "test-model",
        "upstream model projection must match the route: {turn_2_upstream}"
    );
    // Frozen Desktop replay shape, not just text: roles, item types, message
    // metadata (id/status/phase from the recorded turn-1 response), and the
    // content representation are all part of the oracle an M4 hydration
    // rewrite must reproduce byte-for-byte.
    assert_eq!(
        turn_2_upstream["input"],
        json!([
            {"role": "user", "content": "first turn"},
            {
                "type": "message",
                "id": "msg_t1",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "First reply", "annotations": []}],
                "phase": "final_answer"
            },
            {"role": "user", "content": "second turn"}
        ]),
        "PortableSemanticReplay must replay the exact chain shape into input: {turn_2_upstream}"
    );

    normalize(&mut first_body);
    normalize(&mut second_body);
    assert_matches_baseline("responses_previous_response_id", &second_body);
    assert_runtime_differential_two_turns(
        &fixture,
        RuntimeWireFormat::Responses,
        &first_body,
        &second_body,
        json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "first turn"}],
            "stream": false
        }),
        json!({
            "model": RUNTIME_CATALOG_ID,
            "previous_response_id": "resp_t1",
            "input": [{"role": "user", "content": "second turn"}],
            "stream": false
        }),
        "responses_previous_response_id",
    )
    .await;
    fixture.teardown().await;
}

#[tokio::test]
async fn responses_store_false() {
    let fixture = setup(
        WireFormat::Responses,
        vec![
            ScriptedTurn::Json(json!({
                "id": "resp_t1",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": "msg_t1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "First reply", "annotations": []}]
                }],
                "usage": {"input_tokens": 6, "output_tokens": 2, "total_tokens": 8}
            })),
            ScriptedTurn::Json(json!({
                "id": "resp_t2",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": "msg_t2",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "Second reply", "annotations": []}]
                }],
                "usage": {"input_tokens": 9, "output_tokens": 2, "total_tokens": 11}
            })),
        ],
    )
    .await;

    let (_, mut first_body) = desktop_turn(
        &fixture,
        json!({
            "model": fixture.catalog_id,
            "input": [{"role": "user", "content": "first turn"}],
            "store": false,
            "stream": false
        }),
    )
    .await;
    let previous_response_id = first_body["id"].clone();
    assert_eq!(previous_response_id, "resp_t1");

    let (_, mut second_body) = desktop_turn(
        &fixture,
        json!({
            "model": fixture.catalog_id,
            "previous_response_id": previous_response_id,
            "input": [{"role": "user", "content": "second turn"}],
            "store": false,
            "stream": false
        }),
    )
    .await;
    assert_eq!(fixture.upstream.call_count(), 2);

    // `store: false` must not change the local continuation contract: the
    // proxy still records the exchange (third-party endpoints never hold the
    // conversation), so turn 2 replays the chain instead of the upstream
    // server resuming anything.
    let upstream = fixture.upstream.captured_requests();
    let turn_2_upstream = &upstream[1];
    assert!(
        turn_2_upstream["previous_response_id"].is_null(),
        "store:false continuation must strip previous_response_id: {turn_2_upstream}"
    );
    assert_eq!(
        turn_2_upstream["store"], false,
        "store must remain a passthrough field on the upstream body: {turn_2_upstream}"
    );
    // Frozen Desktop replay shape (same contract as the plain
    // `previous_response_id` fixture): roles, item types, message metadata,
    // and content representation must all survive, not just the text.
    assert_eq!(
        turn_2_upstream["input"],
        json!([
            {"role": "user", "content": "first turn"},
            {
                "type": "message",
                "id": "msg_t1",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "First reply", "annotations": []}],
                "phase": "final_answer"
            },
            {"role": "user", "content": "second turn"}
        ]),
        "store:false continuation must replay the exact chain shape into input: {turn_2_upstream}"
    );

    normalize(&mut first_body);
    normalize(&mut second_body);
    assert_matches_baseline("responses_store_false", &second_body);
    assert_runtime_differential_two_turns(
        &fixture,
        RuntimeWireFormat::Responses,
        &first_body,
        &second_body,
        json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "first turn"}],
            "store": false,
            "stream": false
        }),
        json!({
            "model": RUNTIME_CATALOG_ID,
            "previous_response_id": "resp_t1",
            "input": [{"role": "user", "content": "second turn"}],
            "store": false,
            "stream": false
        }),
        "responses_store_false",
    )
    .await;
    fixture.teardown().await;
}

#[tokio::test]
async fn responses_encrypted_reasoning() {
    let fixture = setup(
        WireFormat::Responses,
        vec![
            ScriptedTurn::Json(json!({
                "id": "resp_t1",
                "object": "response",
                "status": "completed",
                "output": [
                    {
                        "type": "reasoning",
                        "id": "rs_enc_1",
                        "encrypted_content": "opaque-secret",
                        "summary": [{"type": "summary_text", "text": "Thought through the turn."}]
                    },
                    {
                        "type": "message",
                        "id": "msg_t1",
                        "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": "First reply", "annotations": []}]
                    }
                ],
                "usage": {"input_tokens": 6, "output_tokens": 2, "total_tokens": 8}
            })),
            ScriptedTurn::Json(json!({
                "id": "resp_t2",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": "msg_t2",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "Second reply", "annotations": []}]
                }],
                "usage": {"input_tokens": 9, "output_tokens": 2, "total_tokens": 11}
            })),
        ],
    )
    .await;

    let (_, mut first_body) = desktop_turn(
        &fixture,
        json!({
            "model": fixture.catalog_id,
            "input": [{"role": "user", "content": "first turn"}],
            "stream": false
        }),
    )
    .await;
    // Provider ciphertext never reaches the client: it is folded into the
    // readable summary before the response is returned (or recorded).
    assert!(
        first_body["output"][0]["encrypted_content"].is_null(),
        "opaque ciphertext must be stripped from the client-facing response: {first_body}"
    );
    assert_eq!(
        first_body["output"][0]["summary"][0]["text"], "Thought through the turn.",
        "the readable reasoning summary must survive normalization: {first_body}"
    );
    let previous_response_id = first_body["id"].clone();
    assert_eq!(previous_response_id, "resp_t1");

    let (_, mut second_body) = desktop_turn(
        &fixture,
        json!({
            "model": fixture.catalog_id,
            "previous_response_id": previous_response_id,
            "input": [{"role": "user", "content": "second turn"}],
            "stream": false
        }),
    )
    .await;
    assert_eq!(fixture.upstream.call_count(), 2);

    // Frozen Desktop replay contract, asserted structurally: the replayed
    // reasoning item keeps its readable summary and its id, never carries
    // ciphertext back to a third-party upstream, and the assistant message
    // retains its recorded metadata (id/status/phase).
    let upstream = fixture.upstream.captured_requests();
    let turn_2_upstream = &upstream[1];
    assert!(
        turn_2_upstream["previous_response_id"].is_null(),
        "third-party continuation must strip previous_response_id: {turn_2_upstream}"
    );
    assert_eq!(
        turn_2_upstream["input"],
        json!([
            {"role": "user", "content": "first turn"},
            {
                "type": "reasoning",
                "id": "rs_enc_1",
                "summary": [{"type": "summary_text", "text": "Thought through the turn."}]
            },
            {
                "type": "message",
                "id": "msg_t1",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "First reply", "annotations": []}],
                "phase": "final_answer"
            },
            {"role": "user", "content": "second turn"}
        ]),
        "PortableSemanticReplay must replay the exact chain shape and never carry ciphertext: {turn_2_upstream}"
    );

    normalize(&mut first_body);
    normalize(&mut second_body);
    assert_matches_baseline("responses_encrypted_reasoning", &second_body);
    assert_runtime_differential_two_turns(
        &fixture,
        RuntimeWireFormat::Responses,
        &first_body,
        &second_body,
        json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "first turn"}],
            "stream": false
        }),
        json!({
            "model": RUNTIME_CATALOG_ID,
            "previous_response_id": "resp_t1",
            "input": [{"role": "user", "content": "second turn"}],
            "stream": false
        }),
        "responses_encrypted_reasoning",
    )
    .await;
    fixture.teardown().await;
}

#[tokio::test]
async fn stream_basic() {
    let sse = vec![
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_stream_1\",\"object\":\"response\",\"status\":\"in_progress\"}}\n\n".to_string(),
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\",\"output_index\":0,\"content_index\":0}\n\n".to_string(),
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_stream_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_stream_1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hi\",\"annotations\":[]}]}],\"usage\":{\"input_tokens\":3,\"output_tokens\":1,\"total_tokens\":4}}}\n\n".to_string(),
    ];
    let fixture = setup(WireFormat::Responses, vec![ScriptedTurn::Sse(sse)]).await;

    let response = boundary_client()
        .post(fixture.url("/v1/responses"))
        .json(&json!({
            "model": fixture.catalog_id,
            "input": [{"role": "user", "content": "hi"}],
            "stream": true
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
    assert!(status.is_success(), "unexpected status {status}: {text}");
    assert!(
        content_type.contains("text/event-stream"),
        "expected text/event-stream, got `{content_type}`"
    );

    let event_types = event_types_from_sse(&text);
    let created_at = event_types
        .iter()
        .position(|event| event == "response.created");
    let delta_at = event_types
        .iter()
        .position(|event| event == "response.output_text.delta");
    let completed_at = event_types
        .iter()
        .position(|event| event == "response.completed");
    assert!(
        matches!((created_at, delta_at, completed_at), (Some(c), Some(d), Some(z)) if c < d && d < z),
        "expected created < delta < completed in {event_types:?}"
    );

    let mut completed = response_from_sse(&text);
    normalize(&mut completed);
    assert_matches_baseline("stream_basic", &completed);
    // M5 live differential: the shared runtime must reproduce the exact
    // client-visible stream Desktop produced.
    assert_stream_differential(
        "stream_basic",
        fixture.script.clone(),
        RuntimeWireFormat::Responses,
        json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "hi"}],
            "stream": true
        }),
        &text,
    )
    .await;
    fixture.teardown().await;
}

/// M5-pre fixture: freeze Desktop's third-party *tool-call streaming*
/// contract. The comparison authority is the full event sequence (not just
/// the final text): every client-visible event type in wire order plus the
/// structural shapes that a runtime migration must reproduce exactly --
/// function-call item identity, incremental argument deltas, and the
/// completed item/response.
#[tokio::test]
async fn stream_tool_call() {
    let sse = vec![
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_stream_tc_1","object":"response","status":"in_progress"}}

"#
        .to_string(),
        r#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"id":"fc_stream_1","type":"function_call","status":"in_progress","name":"get_weather","call_id":"call_stream_1","arguments":""}}

"#
        .to_string(),
        r#"event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","item_id":"fc_stream_1","output_index":0,"delta":"{\"city\":\""}

"#
        .to_string(),
        r#"event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","item_id":"fc_stream_1","output_index":0,"delta":"Taipei\"}"}

"#
        .to_string(),
        r#"event: response.function_call_arguments.done
data: {"type":"response.function_call_arguments.done","item_id":"fc_stream_1","output_index":0,"arguments":"{\"city\":\"Taipei\"}"}

"#
        .to_string(),
        r#"event: response.output_item.done
data: {"type":"response.output_item.done","output_index":0,"item":{"id":"fc_stream_1","type":"function_call","status":"completed","name":"get_weather","call_id":"call_stream_1","arguments":"{\"city\":\"Taipei\"}"}}

"#
        .to_string(),
        r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_stream_tc_1","object":"response","status":"completed","output":[{"id":"fc_stream_1","type":"function_call","status":"completed","name":"get_weather","call_id":"call_stream_1","arguments":"{\"city\":\"Taipei\"}"}],"usage":{"input_tokens":9,"output_tokens":5,"total_tokens":14}}}

"#
        .to_string(),
    ];
    let fixture = setup(WireFormat::Responses, vec![ScriptedTurn::Sse(sse)]).await;

    let response = boundary_client()
        .post(fixture.url("/v1/responses"))
        .json(&json!({
            "model": fixture.catalog_id,
            "input": [{"role": "user", "content": "What is the weather in Taipei?"}],
            "stream": true
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
    assert!(status.is_success(), "unexpected status {status}: {text}");
    assert!(
        content_type.contains("text/event-stream"),
        "expected text/event-stream, got `{content_type}`"
    );

    // Full event-sequence authority: the client must observe exactly these
    // events, in this order, with the function-call item added before the
    // first argument delta and completed only after the arguments settle.
    let events = sse_events(&text);
    let types = events
        .iter()
        .map(|(event, _)| event.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        types,
        vec![
            "response.created",
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.completed",
        ],
        "tool-call stream diverged from the frozen Desktop event sequence: {text}"
    );

    // Structural payload oracles: item identity and argument accumulation.
    let added = &events[1].1;
    assert_eq!(added["type"], "response.output_item.added");
    assert_eq!(added["item"]["type"], "function_call");
    assert_eq!(added["item"]["name"], "get_weather");
    assert_eq!(added["item"]["call_id"], "call_stream_1");
    assert_eq!(added["item"]["id"], "fc_stream_1");

    let first_delta = events[2].1["delta"].as_str().unwrap();
    let second_delta = events[3].1["delta"].as_str().unwrap();
    assert_eq!(
        format!("{first_delta}{second_delta}"),
        r#"{"city":"Taipei"}"#,
        "function-call argument deltas must accumulate in wire order"
    );
    assert_eq!(
        events[4].1["arguments"], r#"{"city":"Taipei"}"#,
        "function_call_arguments.done must carry the fully accumulated arguments"
    );
    let done_item = &events[5].1["item"];
    assert_eq!(done_item["status"], "completed");
    assert_eq!(done_item["arguments"], r#"{"city":"Taipei"}"#);

    let completed = &events[6].1["response"];
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["output"][0]["type"], "function_call");
    assert_eq!(completed["output"][0]["arguments"], r#"{"city":"Taipei"}"#);

    let mut completed_body = response_from_sse(&text);
    normalize(&mut completed_body);
    assert_matches_baseline("stream_tool_call", &completed_body);
    assert_stream_differential(
        "stream_tool_call",
        fixture.script.clone(),
        RuntimeWireFormat::Responses,
        json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "What is the weather in Taipei?"}],
            "stream": true
        }),
        &text,
    )
    .await;
    fixture.teardown().await;
}

/// M5-pre fixture: freeze Desktop's third-party *reasoning streaming*
/// contract. Full event sequence plus structural oracles: incremental
/// reasoning summary deltas, the reasoning item carrying only its readable
/// summary (never ciphertext), the message item's assigned phase, and the
/// completed response retaining the reasoning output.
#[tokio::test]
async fn stream_reasoning() {
    let sse = vec![
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_stream_rs_1","object":"response","status":"in_progress"}}

"#
        .to_string(),
        r#"event: response.reasoning_summary_text.delta
data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_stream_1","output_index":0,"summary_index":0,"delta":"Thought through "}

"#
        .to_string(),
        r#"event: response.reasoning_summary_text.delta
data: {"type":"response.reasoning_summary_text.delta","item_id":"rs_stream_1","output_index":0,"summary_index":0,"delta":"the turn."}

"#
        .to_string(),
        r#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"id":"rs_stream_1","type":"reasoning","status":"in_progress","summary":[{"type":"summary_text","text":"Thought through the turn."}]}}

"#
        .to_string(),
        r#"event: response.output_item.done
data: {"type":"response.output_item.done","output_index":0,"item":{"id":"rs_stream_1","type":"reasoning","status":"completed","summary":[{"type":"summary_text","text":"Thought through the turn."}]}}

"#
        .to_string(),
        r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"The answer is 42."}

"#
        .to_string(),
        r#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":1,"item":{"id":"msg_stream_rs_1","type":"message","role":"assistant","status":"in_progress","content":[]}}

"#
        .to_string(),
        r#"event: response.output_item.done
data: {"type":"response.output_item.done","output_index":1,"item":{"id":"msg_stream_rs_1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"The answer is 42.","annotations":[]}]}}

"#
        .to_string(),
        r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_stream_rs_1","object":"response","status":"completed","output":[{"id":"rs_stream_1","type":"reasoning","status":"completed","summary":[{"type":"summary_text","text":"Thought through the turn."}]},{"id":"msg_stream_rs_1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"The answer is 42.","annotations":[]}]}],"usage":{"input_tokens":8,"output_tokens":12,"total_tokens":20}}}

"#
        .to_string(),
    ];
    let fixture = setup(WireFormat::Responses, vec![ScriptedTurn::Sse(sse)]).await;

    let response = boundary_client()
        .post(fixture.url("/v1/responses"))
        .json(&json!({
            "model": fixture.catalog_id,
            "input": [{"role": "user", "content": "What is 6 times 7?"}],
            "stream": true
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
    assert!(status.is_success(), "unexpected status {status}: {text}");
    assert!(
        content_type.contains("text/event-stream"),
        "expected text/event-stream, got `{content_type}`"
    );

    // Full event-sequence authority. Desktop buffers the final message
    // `output_item.done` and re-emits it directly before `response.completed`
    // (with its phase assigned), so the client sees exactly this order.
    let events = sse_events(&text);
    let types = events
        .iter()
        .map(|(event, _)| event.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        types,
        vec![
            "response.created",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.delta",
            "response.output_item.added",
            "response.output_item.done",
            "response.output_text.delta",
            "response.output_item.added",
            "response.output_item.done",
            "response.completed",
        ],
        "reasoning stream diverged from the frozen Desktop event sequence: {text}"
    );

    // Incremental summary deltas concatenate to the frozen readable summary.
    let first_delta = events[1].1["delta"].as_str().unwrap();
    let second_delta = events[2].1["delta"].as_str().unwrap();
    assert_eq!(
        format!("{first_delta}{second_delta}"),
        "Thought through the turn."
    );

    // The reasoning item carries only its readable summary -- no
    // `encrypted_content`, no raw `content`, no `reasoning_content`.
    for reasoning_event in [&events[3].1, &events[4].1] {
        let item = &reasoning_event["item"];
        assert_eq!(item["type"], "reasoning");
        assert_eq!(item["id"], "rs_stream_1");
        assert_eq!(
            item["summary"],
            json!([{"type": "summary_text", "text": "Thought through the turn."}])
        );
        assert!(
            item.get("encrypted_content").is_none(),
            "reasoning item must never expose ciphertext: {item}"
        );
        assert!(item.get("content").is_none());
        assert!(item.get("reasoning_content").is_none());
    }

    // The final message item is phase-assigned by Desktop and completes the
    // turn after the reasoning item.
    let message_done = &events[7].1["item"];
    assert_eq!(message_done["type"], "message");
    assert_eq!(message_done["id"], "msg_stream_rs_1");
    assert_eq!(message_done["status"], "completed");
    assert_eq!(message_done["phase"], "final_answer");
    assert_eq!(
        message_done["content"],
        json!([{
            "type": "output_text",
            "text": "The answer is 42.",
            "annotations": []
        }])
    );

    let completed = &events[8].1["response"];
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["output"][0]["type"], "reasoning");
    assert_eq!(completed["output"][1]["type"], "message");

    let mut completed_body = response_from_sse(&text);
    normalize(&mut completed_body);
    assert_matches_baseline("stream_reasoning", &completed_body);
    assert_stream_differential(
        "stream_reasoning",
        fixture.script.clone(),
        RuntimeWireFormat::Responses,
        json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "What is 6 times 7?"}],
            "stream": true
        }),
        &text,
    )
    .await;
    fixture.teardown().await;
}

/// Connect a WebSocket client, send one JSON request, and collect every text
/// message until the terminal `response.completed`/`response.failed` frame or
/// a close. Returns the raw message strings in wire order.
async fn websocket_messages(url: &str, request: Value) -> Vec<String> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    let ws_url = url.replacen("http://", "ws://", 1);
    // The upgrade goes through the same boundary guard as any other request.
    let handshake = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        ws_url.as_str(),
    )
    .map(|mut request| {
        request.headers_mut().insert(
            vellum_proxy_runtime::BOUNDARY_KEY_HEADER,
            TEST_BOUNDARY_KEY.parse().expect("valid header value"),
        );
        request
    })
    .expect("websocket request builds");
    let (mut socket, _) = tokio_tungstenite::connect_async(handshake)
        .await
        .expect("websocket upgrade failed");
    socket
        .send(WsMessage::Text(request.to_string().into()))
        .await
        .expect("send websocket request");
    let mut messages = Vec::new();
    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
            .await
            .expect("websocket timed out waiting for a message")
            .expect("websocket stream ended");
        let Ok(message) = next else {
            break;
        };
        let text = match message {
            WsMessage::Text(text) => text.to_string(),
            WsMessage::Binary(bytes) => String::from_utf8(bytes.to_vec()).unwrap(),
            WsMessage::Close(_) | WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => {
                continue
            }
        };
        let is_terminal = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .is_some_and(|kind| matches!(kind.as_str(), "response.completed" | "response.failed"));
        messages.push(text);
        if is_terminal {
            break;
        }
    }
    let _ = socket.close(None).await;
    messages
}

/// M5 WebSocket parity fixture (buffered path): a Codex-style WebSocket
/// client sends one JSON request without a `stream` flag; Desktop and the
/// shared runtime must replay the buffered response as the same
/// `response.output_item.*` / `response.output_text.*` / `response.completed`
/// message sequence with monotonically increasing `sequence_number`s.
#[tokio::test]
async fn ws_basic() {
    let script = vec![ScriptedTurn::Json(json!({
        "id": "resp_ws_1",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "id": "msg_ws_1",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": "Hello there", "annotations": []}]
        }],
        "usage": {"input_tokens": 5, "output_tokens": 3, "total_tokens": 8}
    }))];

    let fixture = setup(WireFormat::Responses, script.clone()).await;
    let desktop = websocket_messages(
        &fixture.url("/v1/responses"),
        json!({
            "model": fixture.catalog_id,
            "input": [{"role": "user", "content": "hello"}]
        }),
    )
    .await;
    fixture.teardown().await;

    // Frozen Desktop message sequence, asserted structurally: added -> text
    // delta -> text done -> item done -> completed, with sequence numbers
    // incrementing from 0.
    let desktop_values = desktop
        .iter()
        .map(|message| serde_json::from_str::<Value>(message).unwrap())
        .collect::<Vec<_>>();
    let types = desktop_values
        .iter()
        .map(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        types,
        vec![
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.output_item.done",
            "response.completed",
        ],
        "WebSocket buffered replay diverged from the frozen Desktop message order: {desktop:?}"
    );
    assert_eq!(desktop_values[0]["item"]["type"], "message");
    assert_eq!(desktop_values[1]["delta"], "Hello there");
    assert_eq!(desktop_values[2]["text"], "Hello there");
    assert_eq!(
        desktop_values[4]["response"]["output"][0]["content"][0]["text"],
        "Hello there"
    );
    for (index, value) in desktop_values.iter().enumerate() {
        assert_eq!(
            value["sequence_number"], index as u64,
            "sequence numbers must be monotonic from 0: {desktop:?}"
        );
    }

    let harness = start_runtime(script, RuntimeWireFormat::Responses).await;
    let runtime = websocket_messages(
        &harness.address,
        json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "hello"}]
        }),
    )
    .await;
    let _ = harness.shutdown.send(());
    let _ = harness.server.await;

    let mut desktop_normalized = desktop_values;
    let mut runtime_normalized = runtime
        .iter()
        .map(|message| serde_json::from_str::<Value>(message).unwrap())
        .collect::<Vec<_>>();
    for value in &mut desktop_normalized {
        normalize(value);
    }
    for value in &mut runtime_normalized {
        normalize(value);
    }
    assert_eq!(
        runtime_normalized, desktop_normalized,
        "fixture `ws_basic`: the shared runtime's WebSocket message sequence diverged from Desktop's"
    );
}

/// M5 WebSocket parity fixture (streaming path): with `stream: true`, the
/// WebSocket client must observe the same SSE event data messages Desktop
/// produced, in the same order.
#[tokio::test]
async fn ws_stream() {
    let sse = vec![
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_ws_s_1\",\"object\":\"response\",\"status\":\"in_progress\"}}\n\n".to_string(),
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\",\"output_index\":0,\"content_index\":0}\n\n".to_string(),
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_ws_s_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_ws_s_1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hi\",\"annotations\":[]}]}],\"usage\":{\"input_tokens\":3,\"output_tokens\":1,\"total_tokens\":4}}}\n\n".to_string(),
    ];

    let fixture = setup(WireFormat::Responses, vec![ScriptedTurn::Sse(sse.clone())]).await;
    let desktop = websocket_messages(
        &fixture.url("/v1/responses"),
        json!({
            "model": fixture.catalog_id,
            "input": [{"role": "user", "content": "hi"}],
            "stream": true
        }),
    )
    .await;
    fixture.teardown().await;

    // Frozen Desktop event sequence over WebSocket: the SSE `data:` payloads
    // arrive as individual text messages in wire order.
    let desktop_values = desktop
        .iter()
        .map(|message| serde_json::from_str::<Value>(message).unwrap())
        .collect::<Vec<_>>();
    let types = desktop_values
        .iter()
        .map(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        types,
        vec![
            "response.created",
            "response.output_text.delta",
            "response.completed",
        ],
        "WebSocket streaming replay diverged from the frozen Desktop event order: {desktop:?}"
    );

    let harness = start_runtime(vec![ScriptedTurn::Sse(sse)], RuntimeWireFormat::Responses).await;
    let runtime = websocket_messages(
        &harness.address,
        json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "hi"}],
            "stream": true
        }),
    )
    .await;
    let _ = harness.shutdown.send(());
    let _ = harness.server.await;

    let mut desktop_normalized = desktop_values;
    let mut runtime_normalized = runtime
        .iter()
        .map(|message| serde_json::from_str::<Value>(message).unwrap())
        .collect::<Vec<_>>();
    for value in &mut desktop_normalized {
        normalize(value);
    }
    for value in &mut runtime_normalized {
        normalize(value);
    }
    assert_eq!(
        runtime_normalized, desktop_normalized,
        "fixture `ws_stream`: the shared runtime's WebSocket streaming sequence diverged from Desktop's"
    );
}

/// M7 usage parity fixture: one non-streaming turn must produce the same
/// usage accounting on Desktop and the shared runtime (same turn count, same
/// input/output token totals).
#[tokio::test]
async fn usage_basic() {
    let script = vec![ScriptedTurn::Json(json!({
        "id": "resp_usage_1",
        "object": "response",
        "status": "completed",
        "output": [
            {
                "type": "message",
                "id": "msg_usage_1",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "Hello there", "annotations": []}]
            },
            {"type": "function_call", "id": "call_usage_1", "call_id": "call_usage_1", "name": "lookup", "arguments": "{}", "status": "completed"}
        ],
        "usage": {
            "input_tokens": 5,
            "output_tokens": 3,
            "total_tokens": 8,
            "input_tokens_details": {"cached_tokens": 2},
            "output_tokens_details": {"reasoning_tokens": 1}
        }
    }))];

    let fixture = setup(WireFormat::Responses, script.clone()).await;
    let response = boundary_client()
        .post(fixture.url("/v1/responses"))
        .json(&json!({
            "model": fixture.catalog_id,
            "input": [{"role": "user", "content": "hello"}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let desktop_route_id = fixture
        .state
        .active_model_routes()
        .iter()
        .find(|model| model.upstream_model == "test-model")
        .expect("parity route must be active")
        .route_id
        .clone();
    let desktop = fixture
        .state
        .usage_store()
        .summary(&desktop_route_id)
        .unwrap();
    fixture.teardown().await;

    let harness = start_runtime(script, RuntimeWireFormat::Responses).await;
    let runtime_response = boundary_client()
        .post(&harness.address)
        .json(&json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "hello"}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    assert!(runtime_response.status().is_success());
    let runtime = harness
        .state
        .proxy_runtime()
        .usage_summary("parity")
        .unwrap();
    let records = harness.state.proxy_runtime().usage_records().unwrap();
    let _ = harness.shutdown.send(());
    let _ = harness.server.await;

    // M7 acceptance: Desktop UsageRecord == Runtime UsageRecord.
    assert_eq!(
        runtime.turns, desktop.turns,
        "fixture `usage_basic`: usage turn counts diverged (desktop={}, runtime={})",
        desktop.turns, runtime.turns
    );
    assert_eq!(
        runtime.latest_input_tokens, desktop.latest_input_tokens,
        "fixture `usage_basic`: input token accounting diverged"
    );
    assert_eq!(
        runtime.total_tokens, desktop.total_tokens,
        "fixture `usage_basic`: total token accounting diverged (desktop={}, runtime={})",
        desktop.total_tokens, runtime.total_tokens
    );
    assert_eq!(runtime.turns, 1);
    assert_eq!(runtime.total_tokens, 8);
    let record = records
        .last()
        .expect("runtime must persist one UsageRecord");
    assert_eq!(record.cached_input_tokens, 2);
    assert_eq!(record.reasoning_tokens, 1);
    assert_eq!(record.tool_calls, 1);
    assert_eq!(record.compaction_tokens, 0);
    assert_eq!(record.review_tokens, 0);
    let desktop_wire_details = usage_details_from_response(&json!({
        "usage": {
            "input_tokens": 5,
            "output_tokens": 3,
            "input_tokens_details": {"cached_tokens": 2},
            "output_tokens_details": {"reasoning_tokens": 1}
        },
        "output": [{"type": "function_call"}]
    }));
    assert_eq!(
        record.cached_input_tokens,
        desktop_wire_details.cached_input_tokens
    );
    assert_eq!(
        record.reasoning_tokens,
        desktop_wire_details.reasoning_tokens
    );
    assert_eq!(record.tool_calls, desktop_wire_details.tool_calls);
}

/// M8 search parity fixture: with no search engine configured, `/v1/alpha/search`
/// must fail closed identically on Desktop and the shared runtime (503 +
/// the "web-search"/"alpha" envelope, Desktop's Disabled-mode contract).
#[tokio::test]
async fn search_disabled() {
    let script = vec![ScriptedTurn::Json(json!({"hello": "world"}))];
    let fixture = setup(WireFormat::Responses, script.clone()).await;
    let response = boundary_client()
        .post(fixture.url("/v1/alpha/search"))
        .json(&json!({
            "id": "search_1",
            "model": fixture.catalog_id,
            "input": [{"role": "user", "content": "search for rust serde"}],
            "commands": {"search_query": [{"q": "rust serde"}]}
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let mut body: Value = response.json().await.unwrap();
    assert_eq!(
        status.as_u16(),
        503,
        "Desktop disabled search must fail closed with 503: {body}"
    );
    fixture.teardown().await;

    let harness = start_runtime(script, RuntimeWireFormat::Responses).await;
    let runtime_response = boundary_client()
        .post(format!(
            "{}/v1/alpha/search",
            harness.address.trim_end_matches("/v1/responses")
        ))
        .json(&json!({
            "id": "search_1",
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "search for rust serde"}],
            "commands": {"search_query": [{"q": "rust serde"}]}
        }))
        .send()
        .await
        .unwrap();
    let runtime_status = runtime_response.status();
    let mut runtime_body: Value = runtime_response.json().await.unwrap();
    let _ = harness.shutdown.send(());
    let _ = harness.server.await;
    assert_eq!(
        runtime_status.as_u16(),
        503,
        "fixture `search_disabled`: the shared runtime's disabled search must \
         fail closed with 503: {runtime_body}"
    );

    normalize(&mut body);
    normalize(&mut runtime_body);
    assert_eq!(
        runtime_body, body,
        "fixture `search_disabled`: disabled-search envelopes diverged between \
         Desktop and the shared runtime"
    );
}

/// M9 review parity fixture: a guardian request (model `codex-auto-review`)
/// must be detected on both sides and dispatched through the configured
/// review route (primary reviewer), producing the same client response and
/// the same upstream request.
#[tokio::test]
async fn guardian_basic() {
    let script = vec![
        ScriptedTurn::Json(json!({
            "id": "resp_parent_1",
            "object": "response",
            "status": "completed",
            "output": [],
            "usage": {"input_tokens": 2, "output_tokens": 1, "total_tokens": 3}
        })),
        ScriptedTurn::Json(json!({
            "id": "resp_review_1",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "message",
                "id": "msg_review_1",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "{\"outcome\":\"allow\"}", "annotations": []}]
            }],
            "usage": {"input_tokens": 4, "output_tokens": 2, "total_tokens": 6}
        })),
    ];

    // Desktop authority: a main route plus an active reviewer route, with the
    // review policy pinned to the reviewer.
    let upstream = FakeUpstream::start(script.clone()).await;
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::with_data_dir(temp.path().to_path_buf());
    let main_routes = state.create_route(
        CreateRouteInput {
            name: "Parity Test Provider".into(),
            base_url: upstream.base_url(),
            model: "test-model".into(),
            wire: WireFormat::Responses,
            streaming: true,
            reasoning: true,
            server_side_resume: false,
            provider_kind: Some(ProviderKind::OpenAiCompatible),
            api_key: None,
            models: Some(vec!["test-model".into()]),
            selected_models: Some(vec!["test-model".into()]),
            context_window: Some(128_000),
            catalog_scope: None,
            model_capabilities: Vec::new(),
        },
        ProviderKind::OpenAiCompatible,
        AuthKind::None,
    );
    let main_route = main_routes
        .iter()
        .find(|route| route.name == "Parity Test Provider")
        .unwrap()
        .id
        .clone();
    let review_routes = state.create_route(
        CreateRouteInput {
            name: "Reviewer".into(),
            base_url: upstream.base_url(),
            model: "picker-review".into(),
            wire: WireFormat::Responses,
            streaming: true,
            reasoning: true,
            server_side_resume: false,
            provider_kind: Some(ProviderKind::OpenAiCompatible),
            api_key: None,
            models: Some(vec!["picker-review".into(), "primary-reviewer".into()]),
            selected_models: Some(vec!["picker-review".into()]),
            context_window: Some(128_000),
            catalog_scope: None,
            model_capabilities: Vec::new(),
        },
        ProviderKind::OpenAiCompatible,
        AuthKind::None,
    );
    let reviewer_route = review_routes
        .iter()
        .find(|route| route.name == "Reviewer")
        .unwrap()
        .id
        .clone();
    for route in &main_routes {
        if route.id != main_route {
            state.set_route_enabled(&route.id, false);
        }
    }
    state.activate_proxy_routes();
    let desktop_parent_catalog = state
        .active_model_routes()
        .into_iter()
        .find(|model| model.upstream_model == "test-model")
        .unwrap()
        .catalog_id;
    state
        .set_review_settings(ReviewSettings {
            on_edit: false,
            before_send: true,
            before_compact: false,
            route_id: reviewer_route,
            model: "primary-reviewer".into(),
            policy: Some(ReviewPolicy::Always),
            fallback_catalog_id: None,
            official_account_id: None,
        })
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let proxy_state = state.clone();
    let proxy_server = tokio::spawn(async move {
        vellum_lib::proxy::serve_local_proxy(
            proxy_state,
            listener,
            test_boundary_key(),
            shutdown_rx,
        )
        .await
        .unwrap();
    });

    let session_id = "0195328e-8765-7123-9876-0123456789ab";
    let parent_metadata = format!(
        "{{\"session_id\":\"{session_id}\",\"thread_id\":\"parent\",\"turn_id\":\"parent-turn\"}}"
    );
    let child_metadata = format!(
        "{{\"session_id\":\"{session_id}\",\"thread_id\":\"review-child\",\"turn_id\":\"review-turn\",\"parent_thread_id\":\"parent\",\"parent_turn_id\":\"parent-turn\"}}"
    );
    let parent_response = boundary_client()
        .post(format!("http://{proxy_address}/v1/responses"))
        .json(&json!({
            "model": desktop_parent_catalog,
            "input": [{"role": "user", "content": "Prepare a protected action"}],
            "stream": false,
            "client_metadata": {"x-codex-turn-metadata": parent_metadata}
        }))
        .send()
        .await
        .unwrap();
    assert!(
        parent_response.status().is_success(),
        "Desktop parent request failed before guardian E2E: {}",
        parent_response.status()
    );

    let desktop_request = json!({
        "model": vellum_proxy_runtime::review::AUTO_REVIEW_MODEL,
        "input": [{"role": "user", "content": "Review this action"}],
        "stream": false,
        "client_metadata": {"x-codex-turn-metadata": child_metadata}
    });
    let response = boundary_client()
        .post(format!("http://{proxy_address}/v1/responses"))
        .json(&desktop_request)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let mut body: Value = response.json().await.unwrap();
    assert!(
        status.is_success(),
        "Desktop guardian request failed: {status}: {body}"
    );
    let desktop_upstream = upstream.captured_requests();
    let _ = shutdown_tx.send(());
    let _ = proxy_server.await;

    // Runtime: the same two-route topology + review policy.
    let runtime_upstream = FakeUpstream::start(script).await;
    let mut config = ProxyRuntimeConfig {
        schema_version: 2,
        identity: ProxyRuntimeIdentity {
            install_id: "parity".into(),
            host_id: "parity".into(),
            image_version: "0.1.0".into(),
            config_hash: "parity".into(),
            ..Default::default()
        },
        models: vec![
            RuntimeRouteConfig {
                route_id: "parity".into(),
                catalog_id: RUNTIME_CATALOG_ID.into(),
                name: "Parity Runtime Provider".into(),
                base_url: runtime_upstream.base_url(),
                provider_kind: RuntimeProviderKind::OpenAiCompatible,
                auth_kind: RuntimeAuthKind::None,
                wire: RuntimeWireFormat::Responses,
                server_side_resume: false,
                streaming: true,
                reasoning: true,
                vision: false,
                upstream_model: "test-model".into(),
                context_window: Some(128_000),
                reasoning_capabilities: Default::default(),
                compaction_capabilities: Default::default(),
                compaction_policy: Default::default(),
                tool_capabilities: Default::default(),
                credential_id: None,
                catalog_entry: None,
                chat_capabilities: Default::default(),
                insecure_http_policy: Default::default(),
                access_mode: None,
            },
            RuntimeRouteConfig {
                route_id: "reviewer".into(),
                catalog_id: "reviewer-model".into(),
                name: "Runtime Reviewer".into(),
                base_url: runtime_upstream.base_url(),
                provider_kind: RuntimeProviderKind::OpenAiCompatible,
                auth_kind: RuntimeAuthKind::None,
                wire: RuntimeWireFormat::Responses,
                server_side_resume: false,
                streaming: true,
                reasoning: true,
                vision: false,
                upstream_model: "primary-reviewer".into(),
                context_window: Some(128_000),
                reasoning_capabilities: Default::default(),
                compaction_capabilities: Default::default(),
                compaction_policy: Default::default(),
                tool_capabilities: Default::default(),
                credential_id: None,
                catalog_entry: None,
                chat_capabilities: Default::default(),
                insecure_http_policy: Default::default(),
                access_mode: None,
            },
        ],
        review: vellum_proxy_runtime::review::ReviewSettings {
            before_send: true,
            route_id: "reviewer".into(),
            model: "primary-reviewer".into(),
            policy: Some(vellum_proxy_runtime::review::ReviewPolicy::Always),
            ..Default::default()
        },
        ..ProxyRuntimeConfig::default()
    };
    config.execution_environment = ExecutionEnvironment::posix_reference();
    let runtime_data = tempfile::tempdir().unwrap();
    config.data_dir = runtime_data.path().join("data");
    config.history_dir = runtime_data.path().join("history");
    config.log_dir = runtime_data.path().join("logs");
    let runtime_state = Arc::new(StaticProxyState::from_config(config).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let runtime_address = listener.local_addr().unwrap();
    let (runtime_shutdown_tx, runtime_shutdown_rx) = oneshot::channel();
    let server_state = Arc::clone(&runtime_state);
    let runtime_policy = test_access_policy(runtime_address.port());
    let server = tokio::spawn(async move {
        let _ = serve_proxy(server_state, listener, runtime_policy, runtime_shutdown_rx).await;
    });
    let runtime_parent_response = boundary_client()
        .post(format!("http://{runtime_address}/v1/responses"))
        .json(&json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "Prepare a protected action"}],
            "stream": false,
            "client_metadata": {"x-codex-turn-metadata": parent_metadata}
        }))
        .send()
        .await
        .unwrap();
    assert!(
        runtime_parent_response.status().is_success(),
        "Runtime parent request failed before guardian E2E: {}",
        runtime_parent_response.status()
    );
    let runtime_response = boundary_client()
        .post(format!("http://{runtime_address}/v1/responses"))
        .json(&json!({
            "model": vellum_proxy_runtime::review::AUTO_REVIEW_MODEL,
            "input": [{"role": "user", "content": "Review this action"}],
            "stream": false,
            "client_metadata": {"x-codex-turn-metadata": child_metadata}
        }))
        .send()
        .await
        .unwrap();
    let runtime_status = runtime_response.status();
    let mut runtime_body: Value = runtime_response.json().await.unwrap();
    let _ = runtime_shutdown_tx.send(());
    let _ = server.await;
    assert!(
        runtime_status.is_success(),
        "fixture `guardian_basic`: the shared runtime's guardian request failed \
         ({runtime_status}): {runtime_body}"
    );

    // Client response parity.
    normalize(&mut body);
    normalize(&mut runtime_body);
    assert_eq!(
        runtime_body, body,
        "fixture `guardian_basic`: guardian responses diverged between Desktop \
         and the shared runtime"
    );
    // Upstream parity: both sides forwarded to the reviewer with the same
    // upstream model and body.
    let mut desktop_upstream_body = desktop_upstream[1].clone();
    let mut runtime_upstream_body = runtime_upstream.captured_requests()[1].clone();
    normalize(&mut desktop_upstream_body);
    normalize(&mut runtime_upstream_body);
    assert_eq!(
        runtime_upstream_body, desktop_upstream_body,
        "fixture `guardian_basic`: guardian upstream requests diverged"
    );
    assert_eq!(
        runtime_upstream_body["model"], "primary-reviewer",
        "guardian requests must reach the reviewer upstream model"
    );
}

/// Root-cause regression (M9 protocol fix): the shared runtime must never
/// freeze `ReviewSettings` at proxy construction. This starts one Desktop
/// proxy server, dispatches a guardian request, flips Auto Review to a
/// different Provider through `AppState::set_review_settings`, dispatches
/// again against the *same* running proxy (never restarted, never a new
/// `DesktopProxyRuntimeState`), and confirms the second request reaches the
/// newly configured reviewer.
#[tokio::test]
async fn guardian_settings_change_takes_effect_without_restarting_the_proxy() {
    let script = vec![ScriptedTurn::Json(json!({
        "id": "resp_review_1",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "id": "msg_review_1",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": "{\"outcome\":\"allow\"}", "annotations": []}]
        }],
        "usage": {"input_tokens": 4, "output_tokens": 2, "total_tokens": 6}
    }))];
    let upstream = FakeUpstream::start(script).await;
    let temp = tempfile::tempdir().unwrap();
    let state = AppState::with_data_dir(temp.path().to_path_buf());

    let reviewer_a = state
        .create_route(
            CreateRouteInput {
                name: "Reviewer A".into(),
                base_url: upstream.base_url(),
                model: "reviewer-a-model".into(),
                wire: WireFormat::Responses,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(ProviderKind::OpenAiCompatible),
                api_key: None,
                models: Some(vec!["reviewer-a-model".into()]),
                selected_models: Some(vec!["reviewer-a-model".into()]),
                context_window: Some(128_000),
                catalog_scope: None,
                model_capabilities: Vec::new(),
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::None,
        )
        .into_iter()
        .find(|route| route.name == "Reviewer A")
        .unwrap()
        .id;
    let reviewer_b = state
        .create_route(
            CreateRouteInput {
                name: "Reviewer B".into(),
                base_url: upstream.base_url(),
                model: "reviewer-b-model".into(),
                wire: WireFormat::Responses,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(ProviderKind::OpenAiCompatible),
                api_key: None,
                models: Some(vec!["reviewer-b-model".into()]),
                selected_models: Some(vec!["reviewer-b-model".into()]),
                context_window: Some(128_000),
                catalog_scope: None,
                model_capabilities: Vec::new(),
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::None,
        )
        .into_iter()
        .find(|route| route.name == "Reviewer B")
        .unwrap()
        .id;

    state
        .set_review_settings(ReviewSettings {
            on_edit: false,
            before_send: true,
            before_compact: false,
            route_id: reviewer_a,
            model: "reviewer-a-model".into(),
            policy: Some(ReviewPolicy::Always),
            fallback_catalog_id: None,
            official_account_id: None,
        })
        .unwrap();
    state.activate_proxy_routes();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let proxy_state = state.clone();
    let proxy_server = tokio::spawn(async move {
        vellum_lib::proxy::serve_local_proxy(
            proxy_state,
            listener,
            test_boundary_key(),
            shutdown_rx,
        )
        .await
        .unwrap();
    });

    let guardian_request = || {
        json!({
            "model": vellum_proxy_runtime::review::AUTO_REVIEW_MODEL,
            "input": [{"role": "user", "content": "Review this action"}],
            "stream": false
        })
    };
    let first = boundary_client()
        .post(format!("http://{proxy_address}/v1/responses"))
        .json(&guardian_request())
        .send()
        .await
        .unwrap();
    assert!(
        first.status().is_success(),
        "first guardian request must succeed: {}",
        first.status()
    );

    // Flip the policy on the SAME running proxy: no restart, no new
    // `ProxyRuntime`/`DesktopProxyRuntimeState`.
    state
        .set_review_settings(ReviewSettings {
            on_edit: false,
            before_send: true,
            before_compact: false,
            route_id: reviewer_b,
            model: "reviewer-b-model".into(),
            policy: Some(ReviewPolicy::Always),
            fallback_catalog_id: None,
            official_account_id: None,
        })
        .unwrap();

    let second = boundary_client()
        .post(format!("http://{proxy_address}/v1/responses"))
        .json(&guardian_request())
        .send()
        .await
        .unwrap();
    assert!(
        second.status().is_success(),
        "second guardian request must succeed: {}",
        second.status()
    );

    let _ = shutdown_tx.send(());
    let _ = proxy_server.await;

    let requests = upstream.captured_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0]["model"], "reviewer-a-model",
        "the first request must use the policy in effect when it was sent"
    );
    assert_eq!(
        requests[1]["model"], "reviewer-b-model",
        "the second request must use the updated policy with no proxy restart"
    );
}

/// M10 delegation fixture: with no verified delegation runtime, both Desktop
/// and the shared runtime must fail closed on a delegating reasoning effort
/// (`ultra`) instead of forwarding a multi-agent contract neither can honour.
#[tokio::test]
async fn delegation_ultra_fails_closed() {
    let script = vec![ScriptedTurn::Json(json!({"hello": "world"}))];
    let fixture = setup(WireFormat::Responses, script.clone()).await;
    let response = boundary_client()
        .post(fixture.url("/v1/responses"))
        .json(&json!({
            "model": fixture.catalog_id,
            "input": [{"role": "user", "content": "hi"}],
            "reasoning": {"effort": "ultra"}
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        status.as_u16(),
        422,
        "Desktop must refuse `ultra` on a non-delegated route: {body}"
    );
    let desktop_message = body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    assert!(
        desktop_message.contains("requires a verified delegation runtime"),
        "Desktop gate message: {desktop_message}"
    );
    fixture.teardown().await;

    let harness = start_runtime(script, RuntimeWireFormat::Responses).await;
    let runtime_response = boundary_client()
        .post(&harness.address)
        .json(&json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [{"role": "user", "content": "hi"}],
            "reasoning": {"effort": "ultra"}
        }))
        .send()
        .await
        .unwrap();
    let runtime_status = runtime_response.status();
    let runtime_body: Value = runtime_response.json().await.unwrap();
    let _ = harness.shutdown.send(());
    let _ = harness.server.await;
    assert_eq!(
        runtime_status.as_u16(),
        422,
        "fixture `delegation_ultra_fails_closed`: the shared runtime must refuse \
         `ultra` on a non-delegated route: {runtime_body}"
    );
    let runtime_message = runtime_body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    assert!(
        runtime_message.contains("requires a verified delegation runtime"),
        "fixture `delegation_ultra_fails_closed`: runtime gate message: {runtime_message}"
    );
    // Fail-closed parity: both sides reject with the same status and the
    // same gate rationale (the envelope shapes stay in their own boundary
    // families by design).
    assert_eq!(runtime_status.as_u16(), status.as_u16());
}

/// Codex Core is the sole auto-compact scheduler (see
/// docs/protocol-source-of-truth.md): an ordinary oversized turn must reach
/// upstream exactly as Codex sent it -- no proxy-side summary call, no
/// rewrite, no short-circuit -- on both Desktop and the shared runtime. Vellum
/// compacts only in answer to a genuine `compaction_trigger`.
///
/// This used to configure an aggressive Canonical threshold first, to prove
/// the proxy stayed out of the way even when it had been told to compact
/// hard. There is no such setting any more: compaction follows from the
/// provider, and Desktop has no way to ask for a different one. The assertion
/// below is what survives, and it is now the stronger claim of the two.
#[tokio::test]
async fn compact_auto_trigger() {
    let reply_turn = ScriptedTurn::Json(json!({
        "id": "resp_auto_1",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "id": "msg_auto_1",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": "Ordinary reply", "annotations": []}]
        }],
        "usage": {"input_tokens": 8, "output_tokens": 4, "total_tokens": 12}
    }));
    let script = vec![reply_turn];

    // A large active history (well past the 1% threshold of 128k = 1280).
    let big_user = "context word ".repeat(1400);
    let auto_input = json!([
        {"role": "user", "content": big_user},
        {"role": "assistant", "content": [{"type": "output_text", "text": "Earlier reply", "annotations": []}]},
        {"role": "user", "content": "current turn"}
    ]);

    let fixture = setup(WireFormat::Responses, script.clone()).await;
    let response = boundary_client()
        .post(fixture.url("/v1/responses"))
        .json(&json!({
            "model": fixture.catalog_id,
            "input": auto_input,
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let mut body: Value = response.json().await.unwrap();
    assert!(
        status.is_success(),
        "Desktop ordinary oversized turn failed: {status}: {body}"
    );
    let desktop_upstream = fixture.upstream.captured_requests();
    assert_eq!(
        desktop_upstream.len(),
        1,
        "an ordinary turn must reach upstream in exactly one call, never a \
         summary call plus the real turn (got {} upstream calls)",
        desktop_upstream.len()
    );
    fixture.teardown().await;

    // Runtime: same topology with the same aggressive threshold.
    let runtime_upstream = FakeUpstream::start(script).await;
    let mut config = ProxyRuntimeConfig {
        schema_version: 2,
        identity: ProxyRuntimeIdentity {
            install_id: "parity".into(),
            host_id: "parity".into(),
            image_version: "0.1.0".into(),
            config_hash: "parity".into(),
            ..Default::default()
        },
        models: vec![RuntimeRouteConfig {
            route_id: "parity".into(),
            catalog_id: RUNTIME_CATALOG_ID.into(),
            name: "Parity Runtime Provider".into(),
            base_url: runtime_upstream.base_url(),
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            auth_kind: RuntimeAuthKind::None,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: false,
            streaming: true,
            reasoning: true,
            vision: false,
            upstream_model: "test-model".into(),
            context_window: Some(128_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: vellum_proxy_runtime::config::RuntimeCompactionPolicy {
                threshold_percent: 1,
                output_reserve_tokens: 0,
                tool_reserve_tokens: 0,
                grok_threshold_percent: None,
            },
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: None,
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        }],
        ..ProxyRuntimeConfig::default()
    };
    config.execution_environment = ExecutionEnvironment::posix_reference();
    let runtime_data = tempfile::tempdir().unwrap();
    config.data_dir = runtime_data.path().join("data");
    config.history_dir = runtime_data.path().join("history");
    config.log_dir = runtime_data.path().join("logs");
    let runtime_state = Arc::new(StaticProxyState::from_config(config).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let runtime_address = listener.local_addr().unwrap();
    let (runtime_shutdown_tx, runtime_shutdown_rx) = oneshot::channel();
    let server_state = Arc::clone(&runtime_state);
    let runtime_policy = test_access_policy(runtime_address.port());
    let server = tokio::spawn(async move {
        let _ = serve_proxy(server_state, listener, runtime_policy, runtime_shutdown_rx).await;
    });
    let runtime_response = boundary_client()
        .post(format!("http://{runtime_address}/v1/responses"))
        .json(&json!({
            "model": RUNTIME_CATALOG_ID,
            "input": [
                {"role": "user", "content": big_user},
                {"role": "assistant", "content": [{"type": "output_text", "text": "Earlier reply", "annotations": []}]},
                {"role": "user", "content": "current turn"}
            ],
            "stream": false
        }))
        .send()
        .await
        .unwrap();
    let runtime_status = runtime_response.status();
    let mut runtime_body: Value = runtime_response.json().await.unwrap();
    let _ = runtime_shutdown_tx.send(());
    let _ = server.await;
    assert!(
        runtime_status.is_success(),
        "fixture `compact_auto_trigger`: the shared runtime's ordinary oversized \
         turn failed ({runtime_status}): {runtime_body}"
    );
    let runtime_upstream_body = runtime_upstream.captured_requests();
    assert_eq!(
        runtime_upstream_body.len(),
        1,
        "the shared runtime must also reach upstream in exactly one call"
    );

    normalize(&mut body);
    normalize(&mut runtime_body);
    assert_eq!(
        runtime_body, body,
        "fixture `compact_auto_trigger`: client responses diverged between \
         Desktop and the shared runtime"
    );
    let mut desktop_real_turn = desktop_upstream[0].clone();
    let mut runtime_real_turn = runtime_upstream_body[0].clone();
    normalize(&mut desktop_real_turn);
    normalize(&mut runtime_real_turn);
    assert_eq!(
        runtime_real_turn, desktop_real_turn,
        "fixture `compact_auto_trigger`: the upstream requests diverged"
    );
    // The upstream input must be exactly what Codex sent — never rewritten.
    assert_eq!(
        runtime_real_turn.get("input"),
        Some(&auto_input),
        "an ordinary oversized turn must never be rewritten: {runtime_real_turn}"
    );
}

/* `compact_canonical`, later `compact_local_engine`, lived here: a frozen
Desktop `/responses/compact` fixture plus a live differential against the
shared runtime.

It went with the setter that drove it. The fixture had to force small
compaction reserves onto one route, because the built-in ones dwarf a scripted
transcript, and the only way to do that was the global policy setter that the
retired control plane took with it. Nothing about the engine changed -- a
third-party route still compacts through the embedded Codex 0.150 engine when
Codex triggers one -- so the contract is still covered where it still runs, in
`vellum-proxy-runtime`'s own compaction tests, which reach the engine directly
instead of through a Desktop setting that no longer exists. */

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

fn test_access_policy(port: u16) -> vellum_proxy_runtime::InboundAccessPolicy {
    vellum_proxy_runtime::InboundAccessPolicy::authenticated(
        vellum_proxy_runtime::BOUNDARY_CREDENTIAL_ID,
        test_boundary_key(),
        port,
    )
}
