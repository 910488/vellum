//! Live smoke for the native Codex `[agents]` default selected in Vellum.
//!
//! Unlike `delegation_turn_loop`, this test launches the real Codex CLI. The
//! parent receives a native spawn tool call, Codex starts the child, and the
//! captured child request must use the catalog id persisted by Vellum's
//! sub-agent settings. All provider traffic stays on loopback.

use axum::body::Body;
use axum::http::{header, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use vellum_lib::catalog::write_catalog_with_model_routes;
use vellum_lib::codex::{apply_proxy_config, CodexPaths};
use vellum_lib::model::{
    AuthKind, CreateRouteInput, ProviderKind, SubagentMode, SubagentSettings, WireFormat,
};
use vellum_lib::state::AppState;

#[derive(Clone, Debug)]
struct CapturedExchange {
    headers: axum::http::HeaderMap,
    body: Value,
}

#[derive(Clone, Default)]
struct Capture {
    requests: Arc<Mutex<Vec<CapturedExchange>>>,
}

impl Capture {
    fn push(&self, headers: axum::http::HeaderMap, body: Value) -> usize {
        let mut requests = self.requests.lock().expect("capture poisoned");
        requests.push(CapturedExchange { headers, body });
        requests.len() - 1
    }

    fn snapshot(&self) -> Vec<CapturedExchange> {
        self.requests.lock().expect("capture poisoned").clone()
    }
}

fn tool_name(tool: &Value) -> Option<&str> {
    tool.get("name")
        .and_then(Value::as_str)
        .or_else(|| tool.pointer("/function/name").and_then(Value::as_str))
}

fn tool_parameters(tool: &Value) -> Option<&Value> {
    tool.get("parameters")
        .or_else(|| tool.pointer("/function/parameters"))
}

/// Replies that actually came back from a spawned child.
///
/// Codex delivers these as `agent_message` history items; Vellum's translated
/// adapter renders them as assistant messages carrying the child's text. Either
/// shape counts -- what must not count is a `function_call_output` from the
/// spawn call itself, which only says a child was *started*.
fn child_replies_in(request: &Value) -> usize {
    request
        .get("input")
        .and_then(Value::as_array)
        .map(|input| {
            input
                .iter()
                .filter(|item| {
                    item.get("type").and_then(Value::as_str) != Some("function_call_output")
                        && item_text(item).contains("CHILD_ROUTE_OK")
                })
                .count()
        })
        .unwrap_or(0)
}

fn already_spawned(request: &Value) -> bool {
    request
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|input| {
            input.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("function_call")
                    && item
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| name.contains("spawn"))
            })
        })
}

fn item_text(item: &Value) -> String {
    match item.get("content") {
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(
                "
",
            ),
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

fn spawn_arguments(tool: &Value, task_name: &str) -> Value {
    let required = tool_parameters(tool)
        .and_then(|parameters| parameters.get("required"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut arguments = serde_json::Map::new();
    for key in required.iter().filter_map(Value::as_str) {
        let value = match key {
            "task_name" => json!(task_name),
            "message" | "task" | "instructions" | "prompt" => {
                json!("Reply exactly CHILD_ROUTE_OK")
            }
            "fork_turns" => json!("all"),
            "agent_type" => json!("default"),
            // A native default-model test must not smuggle an explicit model
            // into the spawn call. If a future protocol makes it mandatory,
            // fail with a useful assertion instead of weakening the test.
            "model" => panic!("native spawn unexpectedly requires an explicit model"),
            other => panic!("unsupported required spawn argument `{other}`"),
        };
        arguments.insert(key.to_string(), value);
    }
    Value::Object(arguments)
}

fn completed_response(id: &str, output: Value) -> Value {
    json!({
        "id": id,
        "object": "response",
        "status": "completed",
        "output": output,
        "usage": {"input_tokens": 11, "output_tokens": 5, "total_tokens": 16}
    })
}

fn completed_sse(response: Value) -> Response<Body> {
    let response_id = response
        .get("id")
        .and_then(Value::as_str)
        .expect("response id");
    let mut events = vec![json!({
        "type": "response.created",
        "response": {
            "id": response_id,
            "object": "response",
            "status": "in_progress",
        },
    })];
    for (output_index, item) in response
        .get("output")
        .and_then(Value::as_array)
        .expect("response output")
        .iter()
        .enumerate()
    {
        let item_id = item.get("id").and_then(Value::as_str).expect("item id");
        let item_type = item.get("type").and_then(Value::as_str).expect("item type");
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        if item_type == "function_call" {
            added["arguments"] = json!("");
        } else if item_type == "message" {
            added["content"] = json!([]);
        }
        events.push(json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": added,
        }));
        match item_type {
            "function_call" => {
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .expect("function arguments");
                events.push(json!({
                    "type": "response.function_call_arguments.done",
                    "item_id": item_id,
                    "output_index": output_index,
                    "arguments": arguments,
                }));
            }
            "message" => {
                let text = item
                    .pointer("/content/0/text")
                    .and_then(Value::as_str)
                    .expect("message output text");
                events.push(json!({
                    "type": "response.output_text.delta",
                    "item_id": item_id,
                    "output_index": output_index,
                    "content_index": 0,
                    "delta": text,
                }));
                events.push(json!({
                    "type": "response.output_text.done",
                    "item_id": item_id,
                    "output_index": output_index,
                    "content_index": 0,
                    "text": text,
                }));
            }
            other => panic!("unsupported mock output item `{other}`"),
        }
        events.push(json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": item,
        }));
    }
    events.push(json!({
        "type": "response.completed",
        "response": response,
    }));
    let body = events
        .into_iter()
        .map(|event| {
            let name = event
                .get("type")
                .and_then(Value::as_str)
                .expect("SSE event type");
            format!(
                "event: {name}\ndata: {}\n\n",
                serde_json::to_string(&event).expect("serialize SSE event")
            )
        })
        .collect::<String>();
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(body))
        .expect("completed SSE response")
}

fn codex_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("VELLUM_LIVE_CODEX_BIN") {
        return PathBuf::from(path);
    }
    if let Some(home) = dirs::home_dir() {
        let sandbox_bin = home.join(".codex/.sandbox-bin/codex.exe");
        if sandbox_bin.is_file() {
            return sandbox_bin;
        }
    }
    if cfg!(windows) {
        let local = std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA");
        let candidate = PathBuf::from(local).join("Programs/OpenAI/Codex/bin/codex.exe");
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("codex")
}

fn copy_auth(destination: &Path) {
    let source = std::env::var_os("VELLUM_LIVE_CODEX_AUTH")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex/auth.json")))
        .expect("resolve Codex auth.json");
    std::fs::copy(&source, destination).unwrap_or_else(|error| {
        panic!(
            "copy isolated Codex auth from {}: {error}",
            source.display()
        )
    });
}

#[tokio::test]
#[ignore = "launches the installed Codex CLI; set VELLUM_LIVE_NATIVE_SUBAGENT=YES"]
async fn ui_selected_third_party_model_is_used_by_a_real_native_codex_spawn() {
    assert_eq!(
        std::env::var("VELLUM_LIVE_NATIVE_SUBAGENT").as_deref(),
        Ok("YES"),
        "set VELLUM_LIVE_NATIVE_SUBAGENT=YES explicitly"
    );
    std::env::set_var("VELLUM_TEST_FILE_KEY", "native-subagent-live-only");

    let capture = Capture::default();
    let target_child = Arc::new(Mutex::new(String::new()));
    let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let capture_for_handler = capture.clone();
    let child_for_handler = Arc::clone(&target_child);
    let polls_for_handler = Arc::clone(&polls);
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let upstream_address = upstream_listener.local_addr().expect("upstream address");
    let handler = move |headers: axum::http::HeaderMap, Json(request): Json<Value>| {
        let capture = capture_for_handler.clone();
        let expected_child = Arc::clone(&child_for_handler);
        let poll_count = Arc::clone(&polls_for_handler);
        async move {
            let index = capture.push(headers, request.clone());
            let requested_model = request
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let child = expected_child.lock().expect("child id poisoned").clone();
            let response = if requested_model == child {
                completed_response(
                    &format!("resp-child-{index}"),
                    json!([{
                        "type": "message",
                        "id": format!("msg-child-{index}"),
                        "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": "CHILD_ROUTE_OK", "annotations": []}]
                    }]),
                )
            } else if child_replies_in(&request) >= 2 {
                completed_response(
                    &format!("resp-parent-final-{index}"),
                    json!([{
                        "type": "message",
                        "id": format!("msg-parent-final-{index}"),
                        "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": "PARENT_SAW_CHILD_ROUTE_OK", "annotations": []}]
                    }]),
                )
            } else if already_spawned(&request) {
                // The parent has spawned and is waiting. A spawn acknowledgement
                // is not a child result -- finishing here is what let this gate
                // pass green while no child had said anything yet. Poll instead,
                // so the parent only finishes once the children's replies have
                // actually travelled back through Vellum's translation.
                let list = request
                    .get("tools")
                    .and_then(Value::as_array)
                    .and_then(|tools| {
                        tools.iter().find(|tool| {
                            tool_name(tool).is_some_and(|name| name.contains("list_agents"))
                        })
                    })
                    .unwrap_or_else(|| {
                        panic!("native multi-agent advertised no list_agents tool: {request}")
                    });
                let polls = poll_count.fetch_add(1, Ordering::SeqCst);
                assert!(
                    polls < 12,
                    "parent polled {polls} times without receiving two child replies"
                );
                completed_response(
                    &format!("resp-parent-wait-{index}"),
                    json!([{
                        "type": "function_call",
                        "id": format!("fc-parent-wait-{index}"),
                        "status": "completed",
                        "name": tool_name(list).expect("list_agents tool name"),
                        "call_id": format!("native_wait_{index}"),
                        "arguments": "{}"
                    }]),
                )
            } else {
                let spawn = request
                    .get("tools")
                    .and_then(Value::as_array)
                    .and_then(|tools| {
                        tools.iter().find(|tool| {
                            tool_name(tool).is_some_and(|name| {
                                let name = name.to_ascii_lowercase();
                                name.contains("spawn") && !name.contains("shell")
                            })
                        })
                    })
                    .unwrap_or_else(|| {
                        panic!("real Codex request advertised no spawn tool: {request}")
                    });
                let name = tool_name(spawn).expect("spawn tool name");
                completed_response(
                    &format!("resp-parent-spawn-{index}"),
                    json!([{
                        "type": "function_call",
                        "id": format!("fc-parent-spawn-a-{index}"),
                        "status": "completed",
                        "name": name,
                        "call_id": "native_spawn_1",
                        "arguments": spawn_arguments(spawn, "selected_provider_child_a").to_string()
                    }, {
                        "type": "function_call",
                        "id": format!("fc-parent-spawn-b-{index}"),
                        "status": "completed",
                        "name": name,
                        "call_id": "native_spawn_2",
                        "arguments": spawn_arguments(spawn, "selected_provider_child_b").to_string()
                    }]),
                )
            };
            completed_sse(response)
        }
    };
    let upstream = tokio::spawn(async move {
        axum::serve(
            upstream_listener,
            Router::new().route("/v1/responses", post(handler)),
        )
        .await
        .expect("mock upstream server");
    });

    let temp = tempfile::tempdir().expect("temp root");
    let state = AppState::with_data_dir(temp.path().join("vellum"));
    let routes = state.create_route(
        CreateRouteInput {
            name: "Native Subagent Loopback".into(),
            base_url: format!("http://{upstream_address}/v1"),
            model: "parent-model".into(),
            wire: WireFormat::Responses,
            streaming: false,
            reasoning: false,
            server_side_resume: false,
            provider_kind: Some(ProviderKind::OpenAiCompatible),
            api_key: None,
            models: Some(vec!["parent-model".into(), "third-party-child".into()]),
            selected_models: Some(vec!["parent-model".into(), "third-party-child".into()]),
            context_window: Some(128_000),
            catalog_scope: None,
            model_capabilities: Vec::new(),
        },
        ProviderKind::OpenAiCompatible,
        AuthKind::None,
    );
    let route = routes
        .iter()
        .find(|route| route.name == "Native Subagent Loopback")
        .expect("loopback route")
        .clone();
    for candidate in routes {
        if candidate.id != route.id {
            state.set_route_enabled(&candidate.id, false);
        }
    }
    let models = state.model_routes();
    let parent = models
        .iter()
        .find(|model| model.route_id == route.id && model.upstream_model == "parent-model")
        .expect("parent catalog model")
        .clone();
    let child = models
        .iter()
        .find(|model| model.route_id == route.id && model.upstream_model == "third-party-child")
        .expect("child catalog model")
        .clone();
    *target_child.lock().expect("child id poisoned") = child.upstream_model.clone();
    state
        .set_subagent_settings(SubagentSettings {
            mode: SubagentMode::Custom,
            route_id: Some(route.id.clone()),
            catalog_id: Some(child.catalog_id.clone()),
            reasoning_effort: None,
        })
        .expect("persist UI-equivalent subagent setting");

    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind Vellum proxy");
    let proxy_address = proxy_listener.local_addr().expect("proxy address");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    state.activate_proxy_routes();
    let proxy_state = state.clone();
    let proxy = tokio::spawn(async move {
        vellum_lib::proxy::serve_local_proxy(
            proxy_state,
            proxy_listener,
            test_boundary_key(),
            shutdown_rx,
        )
        .await
        .expect("Vellum proxy");
    });

    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home).expect("create isolated CODEX_HOME");
    copy_auth(&codex_home.join("auth.json"));
    std::fs::write(
        codex_home.join("config.toml"),
        "approval_policy = \"never\"\nsandbox_mode = \"read-only\"\ndisable_response_storage = true\n[features]\nmulti_agent = true\n",
    )
    .expect("seed Codex config");
    let paths = CodexPaths {
        config: codex_home.join("config.toml"),
        auth: codex_home.join("auth.json"),
        models_cache: codex_home.join("models_cache.json"),
        catalog: state.data_root().join("vellum-model-catalog.json"),
        lease: state.data_root().join("codex-config-lease.json"),
    };
    write_catalog_with_model_routes(&paths.catalog, &state.routes(), &models, None)
        .expect("write fresh model catalog");
    // Production catalog generation must expose the real Enhanced Codex V2
    // loop. The gate must not patch the catalog into passing first.
    let catalog: Value =
        serde_json::from_slice(&std::fs::read(&paths.catalog).expect("read generated catalog"))
            .expect("parse generated catalog");
    let parent_entry = catalog
        .get("models")
        .and_then(Value::as_array)
        .and_then(|models| {
            models.iter().find(|model| {
                model.get("slug").and_then(Value::as_str) == Some(parent.catalog_id.as_str())
            })
        })
        .expect("parent catalog entry");
    assert_eq!(parent_entry["multi_agent_version"], "v2");
    assert!(parent_entry["base_instructions"]
        .as_str()
        .is_some_and(|text| text.contains("exact returned function schema")));
    apply_proxy_config(
        &paths,
        &format!("http://{proxy_address}/v1"),
        &state.subagent_settings(),
        TEST_BOUNDARY_KEY,
        proxy_address.port(),
    )
    .expect("apply Vellum config and [agents] defaults");
    let config = std::fs::read_to_string(&paths.config).expect("read applied config");
    assert!(config.contains(&format!(
        "default_subagent_model = \"{}\"",
        child.catalog_id
    )));

    let binary = codex_binary();
    assert!(
        binary.is_file(),
        "Codex binary missing: {}",
        binary.display()
    );
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    let codex_home_for_child = codex_home.clone();
    let run = tokio::task::spawn_blocking(move || {
        std::process::Command::new(binary)
            .args([
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--dangerously-bypass-approvals-and-sandbox",
                "--enable",
                "multi_agent",
                "-m",
                &parent.catalog_id,
                "Use exactly two sub-agents with the identical instruction 'Reply exactly CHILD_ROUTE_OK', wait for both, then report PARENT_SAW_CHILD_ROUTE_OK.",
            ])
            .env("CODEX_HOME", codex_home_for_child)
            .env("OPENAI_API_KEY", "loopback-only")
            .current_dir(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("launch native Codex")
    });
    let output = tokio::time::timeout(Duration::from_secs(180), run)
        .await
        .expect("native Codex smoke timed out")
        .expect("Codex join");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "native Codex failed: status={} stdout={} stderr={}",
        output.status,
        stdout,
        stderr
    );
    assert!(
        stdout.contains("PARENT_SAW_CHILD_ROUTE_OK"),
        "parent did not consume the child result: {stdout}"
    );
    let requests = capture.snapshot();
    // Set VELLUM_TEST_DUMP to a path to inspect the exact upstream exchange
    // this live run produced. Nothing about the assertions below depends on it.
    if let Some(dump) = std::env::var_os("VELLUM_TEST_DUMP") {
        let bodies = requests
            .iter()
            .map(|ex| ex.body.clone())
            .collect::<Vec<_>>();
        std::fs::write(
            PathBuf::from(dump),
            serde_json::to_vec_pretty(&bodies).expect("serialize captured bodies"),
        )
        .expect("write captured bodies");
    }
    let requested_models = requests
        .iter()
        .filter_map(|ex| ex.body.get("model").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(
        requested_models.contains(&child.upstream_model.as_str()),
        "real spawned child did not reach the upstream model mapped by UI-selected catalog id {} (expected upstream {}); requests={:?}",
        child.catalog_id,
        child.upstream_model,
        requested_models
    );
    let model_counts = requests
        .iter()
        .fold(BTreeMap::<String, usize>::new(), |mut counts, ex| {
            if let Some(model) = ex.body.get("model").and_then(Value::as_str) {
                *counts.entry(model.to_string()).or_default() += 1;
            }
            counts
        });
    // A bare count tells you nothing about which extra turn appeared. Print the
    // ordered exchange so a regression names the offending request instead of
    // just its arithmetic.
    let traffic = requests
        .iter()
        .enumerate()
        .map(|(index, ex)| {
            let model = ex.body.get("model").and_then(Value::as_str).unwrap_or("?");
            let previous = ex
                .body
                .get("previous_response_id")
                .and_then(Value::as_str)
                .unwrap_or("-");
            let kinds = ex
                .body
                .get("input")
                .and_then(Value::as_array)
                .map(|input| {
                    input
                        .iter()
                        .filter_map(|item| item.get("type").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            format!("#{index} model={model} previous={previous} input=[{kinds}]")
        })
        .collect::<Vec<_>>()
        .join(
            "
  ",
        );
    // Codex warms a prompt cache before each thread's first real turn with a
    // `generate: false`, empty-input request. Those are Codex's own bookkeeping,
    // not delegated work, and counting them is what made this gate pass while no
    // child had produced anything. Count real turns only.
    let real_child_turns = requests
        .iter()
        .filter(|ex| {
            ex.body.get("model").and_then(Value::as_str) == Some(child.upstream_model.as_str())
                && ex.body.get("generate") != Some(&json!(false))
        })
        .count();
    assert!(
        real_child_turns >= 2,
        "each spawned child must run a real turn on the UI-selected model:
  {traffic}"
    );
    assert!(
        model_counts
            .keys()
            .all(|model| model == &child.upstream_model || model == &parent.upstream_model),
        "no traffic may reach a model outside the parent/child pair:
  {traffic}"
    );
    assert!(
        requests.iter().all(|exchange| {
            exchange
                .headers
                .get(vellum_proxy_runtime::BOUNDARY_KEY_HEADER)
                .is_none()
        }),
        "the local boundary credential must never reach provider traffic"
    );

    let conn =
        rusqlite::Connection::open(state.data_root().join("usage.sqlite3")).expect("open usage db");
    let mut stmt = conn
        .prepare("SELECT kind, payload FROM diagnostic_events")
        .expect("prepare");
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect");
    for (k, p) in &rows {
        eprintln!("[DIAGNOSTIC EVENT] kind={k}: {p}");
    }

    let graph_links = rows
        .iter()
        .filter(|(k, _)| k == "subagent_graph_linked")
        .map(|(_, p)| {
            serde_json::from_str::<Value>(p).expect("parse subagent_graph_linked payload")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        graph_links.len(),
        2,
        "both parallel children need exact graph edges"
    );

    let parent_thread = graph_links[0]
        .get("parentThreadId")
        .and_then(Value::as_str)
        .expect("graph_linked must have parentThreadId");
    assert!(
        !parent_thread.is_empty(),
        "parent thread ID must not be empty"
    );
    let child_threads = graph_links
        .iter()
        .map(|link| {
            assert_eq!(
                link.get("parentThreadId").and_then(Value::as_str),
                Some(parent_thread)
            );
            assert_eq!(
                link.get("method").and_then(Value::as_str),
                Some("official_thread_metadata")
            );
            assert_eq!(link.get("confidence").and_then(Value::as_str), Some("high"));
            link.get("childThreadId")
                .and_then(Value::as_str)
                .expect("graph_linked must have childThreadId")
                .to_string()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        child_threads.len(),
        2,
        "identical prompts must retain two child identities"
    );
    assert!(child_threads.iter().all(|thread| thread != parent_thread));

    let mut usage_stmt = conn
        .prepare("SELECT agent_attribution_json FROM request_usage WHERE model = ?1 ORDER BY id")
        .expect("prepare child usage query");
    let usage_threads = usage_stmt
        .query_map([&child.upstream_model], |row| row.get::<_, String>(0))
        .expect("query child usage")
        .map(|row| {
            let attribution: Value =
                serde_json::from_str(&row.expect("child attribution row")).unwrap();
            assert_eq!(
                attribution.get("parentThreadId").and_then(Value::as_str),
                Some(parent_thread)
            );
            attribution
                .get("threadId")
                .and_then(Value::as_str)
                .expect("child usage thread id")
                .to_string()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(usage_threads, child_threads);

    // Real turns only, for the same reason as the count above: a prompt-cache
    // warm-up carries no history and owns no conversation.
    let child_request_indices = requests
        .iter()
        .enumerate()
        .filter_map(|(index, exchange)| {
            (exchange.body.get("model").and_then(Value::as_str)
                == Some(child.upstream_model.as_str())
                && exchange.body.get("generate") != Some(&json!(false)))
            .then_some(index)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        child_request_indices.len(),
        2,
        "traffic:
  {traffic}"
    );
    let history = state.history_store();
    let child_history_keys = child_request_indices
        .iter()
        .map(|index| {
            history
                .response_conversation_key(&format!("resp-child-{index}"))
                .expect("read child history owner")
                .expect("child history owner")
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(child_history_keys.len(), 2);
    assert!(child_history_keys
        .iter()
        .all(|key| { child_threads.iter().any(|thread| key.ends_with(thread)) }));
    let parent_history_key = requests
        .iter()
        .enumerate()
        .filter(|(_, exchange)| {
            exchange.body.get("model").and_then(Value::as_str)
                != Some(child.upstream_model.as_str())
        })
        .find_map(|(index, _)| {
            history
                .response_conversation_key(&format!("resp-parent-spawn-{index}"))
                .ok()
                .flatten()
        })
        .expect("parent history owner");
    assert!(parent_history_key.ends_with(parent_thread));
    assert!(child_history_keys
        .iter()
        .all(|key| key != &parent_history_key));

    let comparison = rows
        .iter()
        .filter(|(k, _)| k == "subagent_link_comparison")
        .map(|(_, p)| serde_json::from_str::<Value>(p).expect("parse comparison payload"))
        .find(|p| p.get("officialParent").and_then(Value::as_str) == Some(parent_thread))
        .expect("must record subagent_link_comparison with officialParent matching parent thread");

    assert_eq!(
        comparison.get("officialConfidence").and_then(Value::as_str),
        Some("high")
    );

    let _ = shutdown_tx.send(());
    let _ = proxy.await;
    upstream.abort();
    std::env::remove_var("VELLUM_TEST_FILE_KEY");
}

#[test]
fn subagent_spawn_protocol_graph_and_wire_verification() {
    use axum::http::HeaderMap;
    use vellum_proxy_runtime::codex_metadata::*;
    use vellum_proxy_runtime::subagent_graph::*;

    let mut graph = SubagentGraphRegistry::new();

    // 1. Parent exchange with canonical client_metadata
    let parent_body = json!({
        "client_metadata": {
            "session_id": "0195328e-8765-7123-9876-0123456789ab",
            "thread_id": "th_parent_001",
            "turn_id": "turn_001",
            "x-codex-turn-metadata": "{\"session_id\":\"0195328e-8765-7123-9876-0123456789ab\",\"thread_id\":\"th_parent_001\",\"turn_id\":\"turn_001\"}"
        },
        "model": "parent-model"
    });
    let parent_headers = HeaderMap::new();
    let parent_identity = extract_codex_turn_identity(CodexMetadataSources::new(
        &parent_headers,
        &parent_body,
        vellum_proxy_runtime::RuntimeEndpoint::Responses,
    ))
    .unwrap();

    let parent_obs = TurnObservation {
        execution_id: "exec_parent_1".into(),
        request_id: "req_parent_1".into(),
        connection_id: None,
        route_id: "route_parent".into(),
        model: "parent-model".into(),
        observed_at_ms: 1000,
    };

    let parent_res = graph.observe_turn(&parent_identity, parent_obs).unwrap();
    assert!(parent_res.execution_bound);
    let parent_key = parent_res.thread.unwrap();

    // 2. Child exchange with parent_thread_id and parent_turn_id
    let child_body = json!({
        "client_metadata": {
            "session_id": "0195328e-8765-7123-9876-0123456789ab",
            "thread_id": "th_child_002",
            "turn_id": "turn_002",
            "parent_thread_id": "th_parent_001",
            "x-codex-turn-metadata": "{\"session_id\":\"0195328e-8765-7123-9876-0123456789ab\",\"thread_id\":\"th_child_002\",\"turn_id\":\"turn_002\",\"parent_thread_id\":\"th_parent_001\",\"parent_turn_id\":\"turn_001\",\"subagent_kind\":\"collab_spawn\"}"
        },
        "model": "child-model"
    });
    let child_headers = HeaderMap::new();
    let child_identity = extract_codex_turn_identity(CodexMetadataSources::new(
        &child_headers,
        &child_body,
        vellum_proxy_runtime::RuntimeEndpoint::Responses,
    ))
    .unwrap();

    let child_obs = TurnObservation {
        execution_id: "exec_child_1".into(),
        request_id: "req_child_1".into(),
        connection_id: None,
        route_id: "route_child".into(),
        model: "child-model".into(),
        observed_at_ms: 1050,
    };

    let child_res = graph.observe_turn(&child_identity, child_obs).unwrap();
    assert!(child_res.edge_created);
    let child_key = child_res.thread.unwrap();

    // 3. Assert graph relationships
    assert_eq!(graph.children_of(&parent_key), vec![child_key.clone()]);
    assert_eq!(
        graph.descendants_of(&parent_key, 10).unwrap(),
        vec![child_key.clone()]
    );
    assert_eq!(
        graph.active_descendant_executions(&parent_key, 10).unwrap(),
        vec!["exec_child_1".to_string()]
    );

    // 4. Assert exact parent turn execution resolution
    let (parent_execs, conf) = graph.resolve_parent_executions(
        &child_identity.session_id.unwrap(),
        &child_identity.parent_thread_id.unwrap(),
        child_identity.parent_turn_id.as_ref(),
    );
    assert_eq!(
        conf,
        vellum_proxy_runtime::diagnostics::LinkConfidence::High
    );
    assert_eq!(parent_execs, vec!["exec_parent_1".to_string()]);
}

/// A fixed boundary key for these tests. The proxy refuses every request that
/// does not present one, so the harness has to speak the same protocol Codex
/// does through its managed provider header — which keeps the guard itself
/// under test here rather than bypassed.
const TEST_BOUNDARY_KEY: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

fn test_boundary_key() -> vellum_proxy_runtime::BoundaryKey {
    vellum_proxy_runtime::BoundaryKey::parse(TEST_BOUNDARY_KEY).expect("valid test key")
}
