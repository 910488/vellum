use crate::error::{AppError, AppResult};
use crate::model::{ModelRoute, Route};
use serde_json::Value;
use vellum_proxy_runtime::environment::ExecutionEnvironment;
use vellum_proxy_runtime::profile_adapter::ProfileAdapterRoute;

// M3 (refactor(proxy-runtime): extract request adapters): the pure request/
// response translation helpers that used to live in this file now live in
// `vellum-proxy-runtime`, shared with the headless daemon. These re-exports
// keep every existing `crate::adapter::*` call site in this crate resolving
// exactly as before.
pub use vellum_proxy_runtime::adapter::{
    chat_reasoning_text, content_to_plain_string, content_to_text, custom_tool_input,
    prepare_openai_official_native, sanitize_for_official, strip_cross_realm_fields,
    strip_cross_realm_fields_for_official, strip_opaque_reasoning_keep_summary, ChatSseAdapter,
    CodexToolContext, NamespaceToolContext,
};
pub use vellum_proxy_runtime::body::error_envelope;
pub use vellum_proxy_runtime::profile_adapter::normalize_compatible_responses_request;

/// Thin wrapper: the harness-profile-aware request translation now lives in
/// `vellum-proxy-runtime::profile_adapter`, shared with the headless daemon
/// (plan M3B). Desktop resolves the profile, the route projection, and the
/// executor contract — the host facts this crate owns — then hands the pure
/// runtime translation explicit inputs so Desktop and the daemon translate
/// identical requests identically. The runtime reports `Result<_, String>`
/// (it must not depend on this crate's Desktop-flavored `AppError`); every
/// existing call site here still gets `AppResult` via `?`.
///
/// **NOT FOR EXECUTION.** This projects onto [`ProfileAdapterRoute`], the
/// narrow field set the *translation* reads. It deliberately excludes
/// credential, compaction, and tool capabilities: feeding this projection to a
/// router as if it were a full `RuntimeModelRoute` would fabricate
/// `Bearer`/`credential_id=None` and wrong capability defaults. Execution
/// (M3C onward) resolves the full route separately.
fn profile_adapter_route(route: &Route, model: &ModelRoute) -> ProfileAdapterRoute {
    ProfileAdapterRoute {
        upstream_model: model.upstream_model.clone(),
        reasoning: model.reasoning,
        vision: model.vision,
        provider_kind: route.provider_kind.into(),
        wire: route.wire.into(),
        base_url: route.base_url.clone(),
        chat_capabilities: route
            .model_capabilities
            .iter()
            .find(|capability| capability.model.eq_ignore_ascii_case(&model.upstream_model))
            .map(|capability| capability.chat_capabilities.clone())
            .unwrap_or_default(),
        reasoning_effort_transport: model.reasoning_effort_transport.into(),
        tool_capabilities: vellum_proxy_runtime::route::RuntimeToolCapabilities {
            tool_calling: route
                .model_capabilities
                .iter()
                .find(|capability| capability.model.eq_ignore_ascii_case(&model.upstream_model))
                .and_then(|capability| capability.tool_calling)
                .unwrap_or(true),
        },
    }
}

pub fn prepare_upstream_request(
    original: &Value,
    route: &Route,
    model: &ModelRoute,
) -> AppResult<Value> {
    prepare_upstream_request_with_catalog(original, route, model, None)
}

/// Translate one Codex request for an upstream provider.
///
/// Issue #6 review, P0-1: the resolved [`crate::harness::HarnessProfile`] is
/// the single authority for this whole function. It is resolved once, here,
/// and passed down into tool translation, prompt generation, and history
/// translation, so no downstream branch can quietly decide a different
/// environment from the one the catalog described.
///
/// `catalog_entry` is the entry Codex will actually read for this model. When
/// supplied it is verified against the resolved profile and a mismatch is an
/// **error**, not a log line: a catalog that advertises a capability the
/// runtime cannot execute is the precise failure this harness exists to
/// prevent, and forwarding the request anyway would defeat the check.
/// **Not the live request path.** Kept for the eval/parity harness — the
/// production dispatch path is
/// `vellum_proxy_runtime::exec::ProxyRuntime::execute`, which resolves the
/// local `web_search` wrapper's availability from
/// `RuntimeSnapshot::web_search.enabled` (see `DesktopProxyRuntimeState::new`
/// in `proxy_runtime_bridge.rs` for where that bool is sourced). Eval does not
/// have that state in scope, so this always resolves the wrapper as
/// unavailable, unchanged from before this contract
/// was fixed; see [`prepare_upstream_request_with_catalog_and_shell`] for the
/// entry point that actually threads the flag through.
pub fn prepare_upstream_request_with_catalog(
    original: &Value,
    route: &Route,
    model: &ModelRoute,
    catalog_entry: Option<&Value>,
) -> AppResult<Value> {
    prepare_upstream_request_with_catalog_and_shell(
        original,
        route,
        model,
        catalog_entry,
        crate::harness::shell::detected(),
        false,
    )
}

/// Evaluation-only variant whose shell contract is explicit.
///
/// The production path above always uses the host/session handshake.  Docker
/// evaluation runs the proxy on Windows and the agent on Linux, so deriving
/// the prompt from the proxy host would advertise PowerShell to a Bash tool.
///
/// `web_search_enabled` mirrors `vellum_proxy_runtime::profile_adapter::prepare_upstream_request_with_environment`'s
/// parameter of the same name: whether the local `web_search` compatibility
/// wrapper should stay on the outgoing tool list. Callers that have no
/// concept of a live search configuration (eval/parity, the legacy fixture
/// router) pass `false`.
pub fn prepare_upstream_request_with_catalog_and_shell(
    original: &Value,
    route: &Route,
    model: &ModelRoute,
    catalog_entry: Option<&Value>,
    capabilities: &crate::harness::shell::TerminalCapabilities,
    web_search_enabled: bool,
) -> AppResult<Value> {
    let profile = crate::harness::resolve(route.provider_kind, route.wire);
    let route_projection = profile_adapter_route(route, model);
    let environment = ExecutionEnvironment::from(capabilities);
    vellum_proxy_runtime::profile_adapter::prepare_upstream_request_with_environment(
        original,
        &route_projection,
        &profile,
        &environment,
        catalog_entry,
        web_search_enabled,
    )
    .map_err(AppError::Message)
}

/// Dump exactly what the model can see on this route.
///
/// Issue #6 Phase 0. Takes the **prepared upstream body**, not the incoming
/// Codex request: the point of the snapshot is to record what was actually
/// sent, so the tools are read after translation and the prompt hash is taken
/// over the real outgoing `instructions`. Hashing a locally re-derived prompt
/// would let the trace describe something the model never received.
///
/// Codex builds the native tool router for Official routes, so there the
/// snapshot only reports the catalog contract.
pub fn harness_snapshot(
    original: &Value,
    prepared: &Value,
    route: &Route,
    catalog_entry: Option<&Value>,
) -> AppResult<crate::harness::ModelVisibleHarnessSnapshot> {
    harness_snapshot_with_shell(
        original,
        prepared,
        route,
        catalog_entry,
        crate::harness::shell::detected(),
    )
}

pub fn harness_snapshot_with_shell(
    original: &Value,
    prepared: &Value,
    route: &Route,
    catalog_entry: Option<&Value>,
    capabilities: &crate::harness::shell::TerminalCapabilities,
) -> AppResult<crate::harness::ModelVisibleHarnessSnapshot> {
    let profile = crate::harness::resolve(route.provider_kind, route.wire);
    let environment = ExecutionEnvironment::from(capabilities);
    vellum_proxy_runtime::profile_adapter::harness_snapshot_with_environment(
        original,
        prepared,
        route.wire.into(),
        &profile,
        &environment,
        catalog_entry,
    )
    .map_err(AppError::Message)
}

/// Convert a canonical Responses body to Chat Completions. The pure
/// translation now lives in `vellum-proxy-runtime::profile_adapter`, shared
/// with the headless daemon.
pub fn responses_to_chat(body: &Value) -> AppResult<Value> {
    vellum_proxy_runtime::profile_adapter::responses_to_chat(body).map_err(AppError::Message)
}

pub fn responses_to_chat_with_profile(
    body: &Value,
    profile: &crate::harness::HarnessProfile,
) -> AppResult<Value> {
    vellum_proxy_runtime::profile_adapter::responses_to_chat_with_profile(body, profile)
        .map_err(AppError::Message)
}

/// Thin wrapper: the pure translation now lives in `vellum-proxy-runtime`
/// and reports `Result<Value, String>` (that crate has no `AppError`, and
/// must not depend on this crate's Desktop-flavored error type). Every
/// existing call site here still gets `AppResult<Value>` via `?` as before.
pub fn chat_response_to_responses(body: &Value, model: &str) -> AppResult<Value> {
    vellum_proxy_runtime::adapter::chat_response_to_responses(body, model)
        .map_err(AppError::Message)
}

/// See [`chat_response_to_responses`] for why this is a wrapper rather than a
/// plain re-export.
pub fn chat_response_to_responses_with_context(
    body: &Value,
    model: &str,
    tool_context: &CodexToolContext,
) -> AppResult<Value> {
    vellum_proxy_runtime::adapter::chat_response_to_responses_with_context(
        body,
        model,
        tool_context,
    )
    .map_err(AppError::Message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::shell::{Platform, ShellProbe};
    use crate::model::{AuthKind, ProviderKind, WireFormat};
    use serde_json::json;

    #[test]
    fn llama_cpp_object_tool_arguments_are_normalized_for_codex() {
        let response = chat_response_to_responses(
            &json!({
                "id": "chatcmpl_llama",
                "choices": [{
                    "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_llama",
                            "type": "function",
                            "function": {
                                "name": "shell_command",
                                "arguments": {"command": "git status"}
                            }
                        }]
                    }
                }]
            }),
            "qwen",
        )
        .unwrap();
        let call = &response["output"][0];
        assert_eq!(call["type"], "function_call");
        assert!(call["id"].as_str().unwrap().starts_with("fc_"));
        assert_eq!(call["call_id"], "call_llama");
        assert_eq!(
            serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap(),
            json!({"command": "git status"})
        );
    }

    #[test]
    fn llama_cpp_stream_object_tool_arguments_are_normalized_for_codex() {
        let mut adapter = ChatSseAdapter::new("qwen");
        let events = adapter.push_data(
            &json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_llama_stream",
                            "function": {
                                "name": "shell_command",
                                "arguments": {"command": "git status"}
                            }
                        }]
                    }
                }]
            })
            .to_string(),
        );
        assert!(events
            .join("")
            .contains("response.function_call_arguments.delta"));
        let response = adapter.completed_response();
        let call = response["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call")
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap(),
            json!({"command": "git status"})
        );
    }

    #[test]
    fn namespace_tool_context_restores_flattened_web_run_calls() {
        let context = NamespaceToolContext::from_request(&json!({
            "tools": [{
                "type": "namespace",
                "name": "web",
                "tools": [{"type": "function", "name": "run"}]
            }]
        }));
        let mut item = json!({
            "type": "function_call",
            "name": "web__run",
            "arguments": "{}"
        });
        assert!(context.restore_item(&mut item));
        assert_eq!(item["namespace"], "web");
        assert_eq!(item["name"], "run");
    }

    fn chat_route() -> (Route, ModelRoute) {
        let route = Route {
            id: "third".into(),
            name: "Third".into(),
            base_url: "https://example.test/v1".into(),
            model: "glm".into(),
            wire: WireFormat::Chat,
            is_current: false,
            server_side_resume: false,
            streaming: true,
            reasoning: true,
            provider_kind: ProviderKind::OpenAiCompatible,
            auth_kind: AuthKind::None,
            enabled: true,
            models: vec!["glm".into()],
            selected_models: None,
            context_window: None,
            model_capabilities: Vec::new(),
            insecure_http_policy: Default::default(),
            catalog_scope: Default::default(),
        };
        let model = ModelRoute {
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            reasoning_effort_transport: Default::default(),
            catalog_id: "vlm-model".into(),
            display_name: "GLM".into(),
            route_id: route.id.clone(),
            upstream_model: "glm".into(),
            context_window: None,
            wire: WireFormat::Chat,
            reasoning: true,
            streaming: true,
            vision: false,
        };
        (route, model)
    }

    #[test]
    fn nvidia_nim_reasoning_model_receives_vendor_chat_options() {
        let (mut route, mut model) = chat_route();
        route.base_url = "https://integrate.api.nvidia.com/v1".into();
        model.upstream_model = "nvidia/nemotron-3-super-120b-a12b".into();
        let request = json!({
            "model": model.catalog_id,
            "input": [{"type": "message", "role": "user", "content": "hi"}],
            "stream": true,
            "max_output_tokens": 4096,
            "prompt_cache_key": "codex-session-cache"
        });
        let converted = prepare_upstream_request(&request, &route, &model).unwrap();
        assert_eq!(converted["model"], "nvidia/nemotron-3-super-120b-a12b");
        assert_eq!(converted["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(converted["reasoning_budget"], 4096);
        assert_eq!(converted["temperature"], 1);
        assert_eq!(converted["top_p"], 0.95);
        assert!(converted.get("prompt_cache_key").is_none());
    }

    #[test]
    fn non_nvidia_chat_provider_keeps_prompt_cache_key() {
        let (route, model) = chat_route();
        let request = json!({
            "model": model.catalog_id,
            "input": [{"type": "message", "role": "user", "content": "hi"}],
            "stream": true,
            "prompt_cache_key": "provider-supported-cache"
        });
        let converted = prepare_upstream_request(&request, &route, &model).unwrap();
        assert_eq!(converted["prompt_cache_key"], "provider-supported-cache");
    }

    fn grok_route() -> (Route, ModelRoute) {
        let (mut route, mut model) = chat_route();
        route.id = "grok".into();
        route.name = "Grok Build".into();
        route.provider_kind = ProviderKind::GrokCli;
        route.wire = WireFormat::Responses;
        route.model = "grok-4.5".into();
        route.models = vec!["grok-4.5".into()];
        model.route_id = route.id.clone();
        model.upstream_model = "grok-4.5".into();
        model.wire = WireFormat::Responses;
        (route, model)
    }

    /// Issue #6 DoD: a golden test over the complete Grok model-visible
    /// request. Every tool Codex declares must land in exactly one bucket —
    /// exact translated schema, or explicitly not exposed — and the assertions
    /// below pin the names, the required fields, and the
    /// `additionalProperties` policy rather than just the count.
    ///
    /// `web_search_enabled: true` here stands in for "Brave search enabled
    /// and a usable key configured" (see
    /// `prepare_upstream_request_with_catalog_and_shell`'s doc). The local
    /// `web_search` wrapper is part of this golden surface precisely because
    /// this test represents that configured case, not the always-offered or
    /// always-stripped extremes.
    #[test]
    fn golden_grok_model_visible_request() {
        let (route, model) = grok_route();
        let request = json!({
            "model": "vlm-grok",
            "input": [{"role": "user", "content": "migrate the locales"}],
            "tools": [
                {"type": "custom", "name": "apply_patch", "description": "Apply a patch"},
                {"type": "shell"},
                {"type": "web_search"},
                {"type": "function", "name": "update_plan", "description": "Track steps",
                 "parameters": {"type": "object", "properties": {"plan": {"type": "array"}}}},
                {"type": "namespace", "name": "workspace", "description": "Workspace helpers.",
                 "tools": [{"type": "function", "name": "read", "description": "Read a file.",
                            "parameters": {"type": "object"}}]},
                {"type": "code_interpreter"},
                {"type": "mcp"},
                {"type": "computer"},
                {"type": "some_future_tool"}
            ]
        });
        let converted = prepare_upstream_request_with_catalog_and_shell(
            &request,
            &route,
            &model,
            None,
            crate::harness::shell::detected(),
            true,
        )
        .unwrap();
        let tools = converted["tools"].as_array().unwrap();
        let names = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "shell",
                "web_search",
                "update_plan",
                "workspace__read",
                "apply_patch"
            ]
        );
        assert!(tools.iter().all(|tool| tool["type"] == "function"));

        let by_name = |name: &str| {
            tools
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap_or_else(|| panic!("missing tool {name}"))
                .clone()
        };

        // apply_patch: exact closed contract carrying the patch grammar.
        let patch = by_name("apply_patch");
        assert_eq!(patch["parameters"]["required"], json!(["patch"]));
        assert_eq!(patch["parameters"]["additionalProperties"], false);
        assert!(patch["description"]
            .as_str()
            .unwrap()
            .contains("*** Begin Patch"));

        // shell: argv array, closed, never a permissive object.
        let shell = by_name("shell");
        assert_eq!(
            shell["parameters"]["properties"]["command"]["anyOf"][0]["type"],
            "array"
        );
        assert_eq!(shell["parameters"]["required"], json!(["command"]));
        assert_eq!(shell["parameters"]["additionalProperties"], false);

        // web_search: exact read-only contract.
        assert_eq!(
            by_name("web_search")["parameters"]["required"],
            json!(["query"])
        );

        // Namespace guidance survives the flattening.
        let read = by_name("workspace__read");
        assert_eq!(
            read["description"],
            "[workspace] Workspace helpers. Read a file."
        );

        // Nothing anywhere in the surface may be a permissive object.
        for tool in tools {
            assert_ne!(
                tool["parameters"]["additionalProperties"],
                json!(true),
                "{} must not use a permissive schema",
                tool["name"]
            );
        }

        // The other half of the contract: search disabled (or unusable, e.g.
        // no Brave key) strips the wrapper instead of always offering it.
        let disabled = prepare_upstream_request_with_catalog_and_shell(
            &request,
            &route,
            &model,
            None,
            crate::harness::shell::detected(),
            false,
        )
        .unwrap();
        assert!(
            disabled["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool["name"].as_str() != Some("web_search")),
            "web_search must be stripped when the local wrapper is not usable: {disabled}"
        );
    }

    /// Issue #6 Phase 8: delegation appears only once its runtime is verified,
    /// and then under its own namespace.
    #[test]
    fn delegation_tools_appear_only_when_the_runtime_is_verified() {
        let (route, model) = grok_route();
        let request = json!({"model": "vlm-grok", "input": "hi", "tools": [{"type": "shell"}]});

        let default = prepare_upstream_request(&request, &route, &model).unwrap();
        assert!(!default["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"].as_str().unwrap().starts_with("multi_agent")));

        // Constructed directly: `resolve` withholds the delegated policy until
        // a runtime dispatches spawn/message/wait, but the surface it produces
        // is the contract under test here.
        let mut profile = crate::harness::HarnessProfile::grok_sol_translated_direct();
        profile.multi_agent_policy = crate::harness::MultiAgentPolicy::VellumDelegated;
        let mut body = request.clone();
        vellum_proxy_runtime::profile_adapter::normalize_grok_responses_request(
            &mut body, &profile, false,
        )
        .unwrap();
        let names = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        // Only `spawn`: delegation is synchronous, so `message` and `wait`
        // could never apply and are not advertised.
        assert!(names.contains(&"multi_agent__spawn".to_string()));
        assert!(!names.iter().any(|name| name.contains("message")));
        assert!(!names.iter().any(|name| name.contains("wait")));
        // The shell tool is still there: delegation adds, it does not replace.
        assert!(names.contains(&"shell".to_string()));
    }

    /// Issue #6: even if a stale catalog still lists `ultra`, the request path
    /// refuses it rather than forwarding a mode this route cannot honour.
    #[test]
    fn a_request_asking_for_ultra_is_refused_without_a_delegation_runtime() {
        let (route, model) = grok_route();
        let request = json!({
            "model": "vlm-grok",
            "input": "hi",
            "reasoning": {"effort": "ultra"}
        });
        let error = prepare_upstream_request(&request, &route, &model)
            .unwrap_err()
            .to_string();
        assert!(error.contains("verified delegation runtime"), "{error}");

        // Every other effort still passes through untouched.
        let converted = prepare_upstream_request(
            &json!({"model": "vlm-grok", "input": "hi", "reasoning": {"effort": "xhigh"}}),
            &route,
            &model,
        )
        .unwrap();
        assert_eq!(converted["reasoning"]["effort"], "xhigh");
    }

    #[test]
    fn undispatchable_builtins_are_dropped_rather_than_faked() {
        let (route, model) = grok_route();
        for kind in ["code_interpreter", "mcp", "computer", "some_future_tool"] {
            let converted = prepare_upstream_request(
                &json!({"model": "vlm-grok", "input": "hi", "tools": [{"type": kind}]}),
                &route,
                &model,
            )
            .unwrap();
            let advertised = converted
                .get("tools")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            assert_eq!(
                advertised, 0,
                "{kind} must not be advertised without a runtime behind it"
            );
        }
    }

    #[test]
    fn shell_history_keeps_argv_boundaries_instead_of_joining() {
        let (route, model) = grok_route();
        let converted = prepare_upstream_request(
            &json!({
                "model": "vlm-grok",
                "input": [{
                    "type": "local_shell_call",
                    "id": "shell_1",
                    "action": {
                        "command": ["git", "-C", "C:\\路徑 with space", "status"],
                        "working_directory": "C:\\repo"
                    }
                }]
            }),
            &route,
            &model,
        )
        .unwrap();
        let call = &converted["input"][0];
        assert_eq!(call["type"], "function_call");
        assert_eq!(call["name"], "shell");
        let arguments: Value = serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap();
        // The array round-trips exactly; the old `join(" ")` produced a line
        // that re-parses as five arguments and taught the model to skip quoting.
        assert_eq!(
            arguments["command"],
            json!(["git", "-C", "C:\\路徑 with space", "status"])
        );
        assert_eq!(arguments["workdir"], "C:\\repo");
        assert_eq!(
            crate::harness::shell::parse_shell_command(&arguments).unwrap(),
            vec!["git", "-C", "C:\\路徑 with space", "status"]
        );
    }

    #[test]
    fn unsupported_private_items_are_marked_without_leaking_codex_internals() {
        let (route, model) = grok_route();
        let converted = prepare_upstream_request(
            &json!({
                "model": "vlm-grok",
                "input": [{
                    "type": "computer_call",
                    "call_id": "c1",
                    "action": {"type": "screenshot"},
                    "internal_secret": "private"
                }]
            }),
            &route,
            &model,
        )
        .unwrap();
        let text = converted["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("unsupported on this route"));
        assert!(text.contains("computer_call"));
        assert!(!text.contains("screenshot"));
        assert!(!text.contains("private"));
    }

    fn harness_catalog_entry() -> Value {
        // Mirrors the real catalog generator: the entry carries the catalog's
        // own `base_instructions`, which is what the request-time prompt
        // replacement matches against (never a live probe of this process).
        let profile = crate::harness::resolve(ProviderKind::GrokCli, WireFormat::Responses);
        let baseline =
            crate::harness::prompt::instructions(&profile, crate::harness::shell::detected(), &[]);
        json!({
            "slug": "vlm-grok",
            "base_instructions": baseline,
            "shell_type": "shell_command",
            "apply_patch_tool_type": "freeform",
            "use_responses_lite": false,
            "supports_parallel_tool_calls": false
        })
    }

    #[test]
    fn harness_snapshot_reports_the_translated_surface_and_the_dropped_tools() {
        let (route, model) = grok_route();
        let request = json!({
            "model": "vlm-grok",
            "input": "hi",
            "tools": [
                {"type": "custom", "name": "apply_patch"},
                {"type": "shell"},
                {"type": "mcp"}
            ]
        });
        let catalog_entry = harness_catalog_entry();
        let prepared =
            prepare_upstream_request_with_catalog(&request, &route, &model, Some(&catalog_entry))
                .unwrap();
        let snapshot = harness_snapshot(&request, &prepared, &route, Some(&catalog_entry)).unwrap();
        assert_eq!(snapshot.tool_names(), vec!["apply_patch", "shell"]);
        assert_eq!(snapshot.unsupported_tools[0].name, "mcp");
        assert_eq!(snapshot.prompt_source, "solCompatibleGrok");
        assert_eq!(snapshot.tool_mode, "translatedDirect");
        assert!(snapshot.shell.is_some());
        assert_eq!(snapshot.provider_capabilities["use_responses_lite"], false);
        assert!(snapshot.provider_capabilities["tool_mode"].is_null());
        // The hash is stable for an unchanged contract.
        assert_eq!(
            snapshot.hash(),
            harness_snapshot(&request, &prepared, &route, Some(&catalog_entry))
                .unwrap()
                .hash()
        );

        // A catalog entry that claims a private Sol flag fails closed.
        let spoofed = json!({"slug": "vlm-grok", "use_responses_lite": true});
        let error = harness_snapshot(&request, &prepared, &route, Some(&spoofed)).unwrap_err();
        assert!(error.to_string().contains("harness profile mismatch"));
    }

    /// Issue #6 review, P0-2: the tool-aware prompt must reach the model, and
    /// the recorded hash must cover the prompt that was actually sent.
    #[test]
    fn request_time_prompt_is_sent_and_is_what_the_snapshot_hashes() {
        let (route, model) = grok_route();
        let catalog_entry = harness_catalog_entry();
        let baseline = crate::harness::prompt::instructions(
            &crate::harness::resolve(route.provider_kind, route.wire),
            crate::harness::shell::detected(),
            &[],
        );
        let request = json!({
            "model": "vlm-grok",
            "instructions": format!("{baseline}\n\n# AGENTS.md\nProject rule: never touch vendor/."),
            "input": "hi",
            "tools": [
                {"type": "custom", "name": "apply_patch"},
                {"type": "shell"},
                {"type": "function", "name": "search_files", "parameters": {"type": "object"}}
            ]
        });
        let prepared =
            prepare_upstream_request_with_catalog(&request, &route, &model, Some(&catalog_entry))
                .unwrap();

        let sent = prepared["instructions"].as_str().unwrap();
        // The request-time surface is named in the prompt the model receives,
        // not merely in a diagnostic that never leaves the process.
        assert!(sent.contains("Tools available in this request:"));
        assert!(sent.contains("state-changing: apply_patch, shell"));
        assert!(sent.contains("read-only: search_files"));
        assert!(!sent.contains("Tools available on this route:"));
        // The user's own instructions survive: only the baseline is replaced.
        assert!(sent.contains("Project rule: never touch vendor/."));

        let snapshot = harness_snapshot(&request, &prepared, &route, Some(&catalog_entry)).unwrap();
        assert_eq!(
            snapshot.prompt_hash,
            crate::harness::snapshot::hash_text(sent),
            "the recorded prompt hash must cover the instructions actually sent"
        );
    }

    #[test]
    fn request_time_prompt_reaches_the_chat_system_message() {
        let (route, model) = chat_route();
        let prepared = prepare_upstream_request(
            &json!({
                "model": "vlm-model",
                "input": "hi",
                "tools": [{"type": "custom", "name": "apply_patch"}, {"type": "shell"}]
            }),
            &route,
            &model,
        )
        .unwrap();
        let system = prepared["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "system")
            .and_then(|message| message["content"].as_str())
            .expect("chat routes carry the prompt as a system message");
        assert!(system.contains("Tools available in this request:"));
        let snapshot = harness_snapshot(&Value::Null, &prepared, &route, None).unwrap();
        assert_eq!(
            snapshot.prompt_hash,
            crate::harness::snapshot::hash_text(system)
        );
    }

    /// Issue #6 review, M3B P0-1: the request-time prompt replacement must
    /// match against the *catalog's* `base_instructions`, never a live probe of
    /// the proxy process's shell. When the Codex executor is a Linux bash
    /// contract under a Windows proxy, a host-derived baseline would leave the
    /// old Windows shell contract in place and append a second, contradictory
    /// bash contract. The catalog's own baseline swaps cleanly.
    #[test]
    fn executor_contract_replaces_the_catalog_baseline_not_the_proxy_host_baseline() {
        let (route, model) = grok_route();
        let profile = crate::harness::resolve(route.provider_kind, route.wire);

        // The catalog was written on a Windows PowerShell host.
        let host_caps = crate::harness::shell::TerminalCapabilities::from_probe(&ShellProbe {
            platform: Some(Platform::Windows),
            powershell: Some("C:/Windows/System32/powershell.exe".into()),
            powershell_version: Some("5.1.26200.1".into()),
            ..ShellProbe::default()
        });
        let catalog_baseline = crate::harness::prompt::instructions(&profile, &host_caps, &[]);
        let catalog_entry = json!({
            "slug": "vlm-grok",
            "base_instructions": catalog_baseline,
            "shell_type": "shell_command",
            "apply_patch_tool_type": "freeform",
            "use_responses_lite": false,
            "supports_parallel_tool_calls": false
        });
        let request = json!({
            "model": "vlm-grok",
            "instructions": format!(
                "{catalog_baseline}\n\n# AGENTS.md\nProject rule: never touch vendor/."
            ),
            "input": "hi",
            "tools": [
                {"type": "custom", "name": "apply_patch"},
                {"type": "shell"}
            ]
        });

        // The executor is the Linux bash contract, not this proxy process.
        let executor_caps = crate::harness::shell::TerminalCapabilities::from_probe(&ShellProbe {
            platform: Some(Platform::Linux),
            shell_env: Some("/bin/bash".into()),
            ..ShellProbe::default()
        });
        let prepared = prepare_upstream_request_with_catalog_and_shell(
            &request,
            &route,
            &model,
            Some(&catalog_entry),
            &executor_caps,
            false,
        )
        .unwrap();
        let sent = prepared["instructions"].as_str().unwrap();
        // The bash executor contract replaced the Windows catalog baseline in
        // one step — the prompt never carries two shell contracts.
        assert!(sent.contains("Tools available in this request:"));
        assert!(sent.contains("use `rg` rather than `grep`"));
        assert!(!sent.contains("no `&&`"));
        assert!(!sent.contains("Select-String"));
        assert!(!sent.contains("Tools available on this route:"));
        // The user's own instructions survive the replacement.
        assert!(sent.contains("Project rule: never touch vendor/."));
    }

    /// Issue #6 review, P0-1: a spoofed catalog entry must fail the request,
    /// not merely log. `prepare_upstream_request_with_catalog` is the live path.
    #[test]
    fn a_catalog_claiming_private_sol_flags_fails_the_request() {
        let (route, model) = grok_route();
        let request = json!({"model": "vlm-grok", "input": "hi", "tools": []});
        for spoofed in [
            json!({"slug": "vlm-grok", "use_responses_lite": true}),
            json!({"slug": "vlm-grok", "supports_parallel_tool_calls": true}),
            json!({"slug": "vlm-grok", "tool_mode": "code_mode_only"}),
            json!({"slug": "vlm-grok", "multi_agent_version": "v1"}),
        ] {
            let error =
                prepare_upstream_request_with_catalog(&request, &route, &model, Some(&spoofed))
                    .unwrap_err();
            assert!(
                error.to_string().contains("harness profile mismatch"),
                "{spoofed} must be rejected, got {error}"
            );
        }
        prepare_upstream_request_with_catalog(
            &request,
            &route,
            &model,
            Some(&json!({"slug": "vlm-grok", "multi_agent_version": "v2"})),
        )
        .unwrap();
        // The entry the generator actually produces is accepted.
        prepare_upstream_request_with_catalog(
            &request,
            &route,
            &model,
            Some(&harness_catalog_entry()),
        )
        .unwrap();
    }

    /// Issue #6 review, P0-1: the profile governs translation, not a constant.
    /// Dropping the patch contract must remove the tool *and* the prompt line
    /// that tells the model to use it — they cannot disagree.
    #[test]
    fn profile_patch_contract_drives_both_the_tool_and_the_prompt() {
        let tool = json!({"type": "custom", "name": "apply_patch"});
        let mut profile = crate::harness::resolve(ProviderKind::GrokCli, WireFormat::Responses);
        assert_eq!(
            vellum_proxy_runtime::profile_adapter::grok_function_tools(&tool, &profile)[0]["name"],
            "apply_patch"
        );

        profile.patch_contract = crate::harness::PatchContract::Unsupported;
        assert!(
            vellum_proxy_runtime::profile_adapter::grok_function_tools(&tool, &profile).is_empty()
        );
        assert!(vellum_proxy_runtime::profile_adapter::chat_tools(&tool, &profile).is_empty());
        let prompt =
            crate::harness::prompt::instructions(&profile, crate::harness::shell::detected(), &[]);
        assert!(prompt.contains("exposes no patch tool"));
    }

    /// Issue #6 review, P0-3: what history emits must satisfy the schema the
    /// same harness advertises for that tool.
    #[test]
    fn emitted_shell_history_validates_against_the_advertised_shell_schema() {
        let (route, model) = grok_route();
        let crate::harness::tools::BuiltinDecision::Exact { parameters, .. } =
            crate::harness::tools::builtin_tool("shell")
        else {
            panic!("shell must have an exact contract");
        };
        let converted = prepare_upstream_request(
            &json!({
                "model": "vlm-grok",
                "input": [
                    {"type": "local_shell_call", "id": "s1",
                     "action": {"command": ["git", "-C", "C:\\路徑 with space", "status"],
                                "working_directory": "C:\\repo", "timeout_ms": 1000}},
                    {"type": "local_shell_call", "id": "s2",
                     "action": {"command": "git status | head -5"}}
                ],
                "tools": [{"type": "shell"}]
            }),
            &route,
            &model,
        )
        .unwrap();
        for call in converted["input"].as_array().unwrap() {
            let arguments: Value =
                serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap();
            crate::harness::tools::validate_against_schema(&arguments, &parameters)
                .unwrap_or_else(|error| panic!("history item violates its own schema: {error}"));
            assert!(arguments.get("commandLine").is_none());
            assert!(arguments.get("timeoutMs").is_none());
        }
    }

    #[test]
    fn official_snapshot_reports_the_native_contract_only() {
        let (mut route, _) = grok_route();
        route.provider_kind = ProviderKind::Official;
        let sol = json!({
            "slug": "gpt-5.6-sol",
            "base_instructions": "sol instructions",
            "tool_mode": "code_mode_only",
            "use_responses_lite": true,
            "supports_parallel_tool_calls": true,
            "multi_agent_version": "v2",
            "priority": 1
        });
        let snapshot = harness_snapshot(&Value::Null, &json!({}), &route, Some(&sol)).unwrap();
        assert_eq!(snapshot.prompt_source, "codexOfficialBundledCatalog");
        assert_eq!(snapshot.tool_mode, "codexNative");
        // Vellum translates nothing here, so it enumerates nothing.
        assert!(snapshot.tools.is_empty());
        assert_eq!(
            snapshot.provider_capabilities["tool_mode"],
            "code_mode_only"
        );
    }

    #[test]
    fn removes_provider_owned_reasoning_before_cross_realm_replay() {
        let mut value = json!({
            "input": [
                {"role": "assistant", "reasoning_content": "private"},
                {"type": "reasoning", "summary": [{"text": "private"}]}
            ],
            "encrypted_content": "nested"
        });
        strip_cross_realm_fields(&mut value);
        assert!(value.pointer("/input/0/reasoning_content").is_none());
        assert_eq!(value["input"].as_array().unwrap().len(), 1);
        assert!(value.get("encrypted_content").is_none());
    }

    #[test]
    fn official_handoff_drops_foreign_reasoning_but_preserves_compaction_ciphertext() {
        let mut value = json!({
            "input": [
                {
                    "type": "compaction",
                    "id": "cmp_official",
                    "encrypted_content": "opaque-official-state"
                },
                {
                    "type": "reasoning",
                    "id": "rs_grok",
                    "encrypted_content": "opaque-grok-state"
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": "continue"
                }
            ]
        });

        strip_cross_realm_fields_for_official(&mut value);

        let input = value["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "compaction");
        assert_eq!(input[0]["encrypted_content"], "opaque-official-state");
        assert_eq!(input[1]["role"], "user");
    }

    #[test]
    fn official_replay_keeps_only_verifiable_reasoning_items() {
        let mut value = json!({
            "input": [
                {"type": "reasoning", "id": "third_party", "summary": []},
                {
                    "type": "reasoning",
                    "id": "resp_vellum_1785249670601_reasoning",
                    "encrypted_content": "synthetic"
                },
                {"type": "reasoning", "id": "rs_official", "encrypted_content": "opaque"},
                {
                    "type": "message",
                    "id": "resp_vellum_1_message",
                    "role": "assistant",
                    "reasoning_content": "chat-only"
                },
                {
                    "type": "function_call",
                    "id": "call_1f25ce1d5b244415912270b7",
                    "call_id": "call_1f25ce1d5b244415912270b7",
                    "name": "shell_command",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1f25ce1d5b244415912270b7",
                    "output": "ok"
                },
                {
                    "type": "custom_tool_call",
                    "id": "fc_4d647f67-f38d-9151-a08a-aa7d58f4578a_0",
                    "call_id": "call_patch",
                    "name": "apply_patch",
                    "input": "*** Begin Patch\n*** End Patch"
                }
            ]
        });
        sanitize_for_official(&mut value);
        let input = value["input"].as_array().unwrap();
        assert_eq!(input.len(), 5);
        assert_eq!(input[0]["id"], "rs_official");
        assert_eq!(input[0]["encrypted_content"], "opaque");
        assert!(input[1].get("reasoning_content").is_none());
        assert!(input[1]["id"].as_str().unwrap().starts_with("msg_"));
        assert!(input[2]["id"].as_str().unwrap().starts_with("fc_"));
        assert_eq!(input[2]["call_id"], "call_1f25ce1d5b244415912270b7");
        assert_eq!(input[3]["call_id"], "call_1f25ce1d5b244415912270b7");
        assert!(input[4]["id"].as_str().unwrap().starts_with("ctc_"));
        assert_eq!(input[4]["call_id"], "call_patch");
    }

    #[test]
    fn expands_namespace_and_maps_builtin_tools() {
        let (route, model) = chat_route();
        let request = json!({
            "model": "vlm-model",
            "input": "hi",
            "tools": [
                {"type": "namespace", "tools": [{"type": "function", "name": "read", "parameters": {"type": "object"}}]},
                {"type": "web_search"}
            ]
        });
        let converted = prepare_upstream_request(&request, &route, &model).unwrap();
        assert_eq!(converted["tools"].as_array().unwrap().len(), 2);
        assert_eq!(converted["tools"][0]["function"]["name"], "read");
        assert_eq!(converted["tools"][1]["function"]["name"], "web_search");
    }

    #[test]
    fn reasoning_chat_uses_native_tool_calls_without_forcing_tool_choice() {
        let (route, model) = chat_route();
        let converted = prepare_upstream_request(
            &json!({
                "model": "vlm-model",
                "input": "clone the repository",
                "tools": [{
                    "type": "function",
                    "name": "shell_command",
                    "parameters": {"type": "object"}
                }]
            }),
            &route,
            &model,
        )
        .unwrap();
        let messages = converted["messages"].as_array().unwrap();
        let guard = messages
            .iter()
            .filter_map(|message| message.get("content").and_then(Value::as_str))
            .find(|content| content.contains("native Chat Completions"))
            .expect("tool protocol guard");
        assert!(guard.contains("Never serialize <tool_call>, <think>"));
        assert!(converted.get("tool_choice").is_none());
        assert_eq!(converted["stream_options"]["include_usage"], true);
    }

    #[test]
    fn non_streaming_chat_content_hides_private_markup() {
        let response = chat_response_to_responses(
            &json!({
                "id": "chat_markup",
                "choices": [{"message": {
                    "content": "準備執行。<think>private reasoning</think><tool_call>{\"name\":\"shell_command\"}</tool_call>請稍候。"
                }}]
            }),
            "glm",
        )
        .unwrap();
        let text = response["output"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert_eq!(text, "準備執行。請稍候。");
    }

    #[test]
    fn streaming_chat_content_hides_split_think_and_malformed_tool_markup() {
        let mut adapter = ChatSseAdapter::new("glm");
        let first = adapter
            .push_data(&json!({"choices": [{"delta": {"content": "我先處理。<thi"}}]}).to_string());
        let second = adapter.push_data(
            &json!({"choices": [{"delta": {
                "content": "nk>private reasoning</think><tool_"
            }}]})
            .to_string(),
        );
        let third = adapter.push_data(
            &json!({"choices": [{"delta": {
                "content": "call>shell_commandcommand={\"command\":\"git clone secret\"}</think>完成。"
            }}]})
            .to_string(),
        );
        let done = adapter.push_data("[DONE]");
        let events = [first, second, third, done].concat().join("");
        assert!(!events.contains("private reasoning"));
        assert!(!events.contains("shell_commandcommand"));
        assert!(!events.contains("<think>"));
        assert!(!events.contains("<tool_call>"));

        let response = adapter.completed_response();
        let text = response["output"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert_eq!(text, "我先處理。完成。");
    }

    #[test]
    fn grok_request_removes_private_codex_model_input_variants() {
        let (route, model) = grok_route();
        let request = json!({
            "model": "vlm-grok",
            "previous_response_id": "resp_other_realm",
            "store": false,
            "input": [
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [{
                        "type": "namespace",
                        "name": "workspace",
                        "tools": [{
                            "type": "function",
                            "name": "read",
                            "parameters": {"type": "object"}
                        }]
                    }]
                },
                {
                    "type": "local_shell_call",
                    "id": "shell_1",
                    "action": {"command": ["git", "status"]},
                    "internal_trace": "private"
                },
                {"type": "local_shell_call_output", "call_id": "shell_1", "output": "clean"},
                {"type": "custom_tool_call", "call_id": "custom_1", "name": "apply_patch", "input": "*** patch"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "continue"}]}
            ],
            "tools": [{"type": "web_search"}],
            "reasoning": {"effort": "xhigh"},
            "stream": true
        });
        // `web_search_enabled: true` — see `golden_grok_model_visible_request`'s
        // doc for why this test represents the "Brave configured" case.
        let converted = prepare_upstream_request_with_catalog_and_shell(
            &request,
            &route,
            &model,
            None,
            crate::harness::shell::detected(),
            true,
        )
        .unwrap();
        // Grok continuity uses stable session headers and readable checkpoints,
        // not OpenAI previous_response_id. `store` is a legal Grok wire field.
        assert!(converted.get("previous_response_id").is_none());
        assert_eq!(converted.get("store"), Some(&json!(false)));
        assert_eq!(converted["reasoning"]["effort"], "xhigh");

        let input = converted["input"].as_array().unwrap();
        let input_types = input
            .iter()
            .filter_map(|item| item.get("type").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert!(!input_types.contains(&"additional_tools"));
        assert!(!input_types.contains(&"local_shell_call"));
        assert!(!input_types.contains(&"custom_tool_call"));
        assert_eq!(input_types[0], "function_call");

        let tools = converted["tools"].as_array().unwrap();
        assert!(tools.iter().all(|tool| tool["type"] == "function"));
        assert!(tools.iter().any(|tool| tool["name"] == "workspace__read"));
        assert!(tools.iter().any(|tool| tool["name"] == "web_search"));
    }

    #[test]
    fn grok_request_deduplicates_top_level_and_embedded_exec_tools() {
        let (route, model) = grok_route();
        let schema = json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"]
        });
        let converted = prepare_upstream_request(
            &json!({
                "model": "vlm-grok",
                "input": [
                    {
                        "type": "additional_tools",
                        "tools": [{
                            "type": "function",
                            "name": "exec",
                            "parameters": schema.clone()
                        }]
                    },
                    {"type": "message", "role": "user", "content": "continue"}
                ],
                "tools": [{
                    "type": "function",
                    "name": "exec",
                    "parameters": schema
                }]
            }),
            &route,
            &model,
        )
        .unwrap();
        let exec = converted["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|tool| tool["name"] == "exec")
            .count();
        assert_eq!(exec, 1);
    }

    #[test]
    fn conflicting_duplicate_tool_schemas_fail_before_upstream() {
        let (route, model) = grok_route();
        let error = prepare_upstream_request(
            &json!({
                "model": "vlm-grok",
                "input": [],
                "tools": [
                    {
                        "type": "function",
                        "name": "exec",
                        "parameters": {"type": "object", "required": ["path"]}
                    },
                    {
                        "type": "function",
                        "name": "exec",
                        "parameters": {"type": "object", "required": ["command"]}
                    }
                ]
            }),
            &route,
            &model,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("conflicting duplicate function definition 'exec'"));
    }

    #[test]
    fn current_top_level_tool_schema_overrides_stale_embedded_schema() {
        let (route, model) = grok_route();
        // Custom tools are closed schemas (issue #6 Phase 2): an extra field
        // on a free-form tool silently drops the real payload.
        let current_schema = json!({
            "type": "object",
            "properties": {"input": {"type": "string"}},
            "required": ["input"],
            "additionalProperties": false
        });
        let converted = prepare_upstream_request(
            &json!({
                "model": "vlm-grok",
                "input": [{
                    "type": "additional_tools",
                    "tools": [{
                        "type": "function",
                        "name": "_fetch",
                        "parameters": {
                            "type": "object",
                            "properties": {"request": {"type": "string"}},
                            "required": ["request"]
                        }
                    }]
                }],
                "tools": [{
                    "type": "custom",
                    "name": "_fetch",
                    "description": "Fetch through the current Codex custom tool"
                }]
            }),
            &route,
            &model,
        )
        .unwrap();
        let fetch_tools = converted["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|tool| tool["name"] == "_fetch")
            .collect::<Vec<_>>();
        assert_eq!(fetch_tools.len(), 1);
        assert_eq!(fetch_tools[0]["parameters"], current_schema);
    }

    #[test]
    fn compatible_responses_normalizes_private_codex_items() {
        let (mut route, mut model) = chat_route();
        route.wire = WireFormat::Responses;
        model.wire = WireFormat::Responses;
        let converted = prepare_upstream_request(
            &json!({
                "model": "vlm-qwen",
                "previous_response_id": "foreign-response",
                "input": [
                    {
                        "type": "additional_tools",
                        "tools": [{
                            "type": "function",
                            "name": "exec",
                            "parameters": {"type": "object"}
                        }]
                    },
                    {
                        "type": "local_shell_call",
                        "id": "shell_1",
                        "action": {"command": ["git", "status"]}
                    },
                    {"type": "local_shell_call_output", "call_id": "shell_1", "output": "clean"},
                    {"type": "message", "role": "user", "content": "continue"}
                ],
                "stream": true
            }),
            &route,
            &model,
        )
        .unwrap();
        assert!(converted.get("previous_response_id").is_none());
        assert!(converted["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["type"] != "additional_tools" && item["type"] != "local_shell_call"));
        assert_eq!(converted["tools"][0]["name"], "exec");
    }

    #[test]
    fn chat_conversion_uses_additional_tools_without_leaking_a_message() {
        let profile = crate::harness::HarnessProfile::generic(WireFormat::Chat.into());
        let schema = json!({
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"]
        });
        let converted = responses_to_chat_with_profile(
            &json!({
                "model": "compatible-model",
                "input": [
                    {
                        "type": "additional_tools",
                        "role": "developer",
                        "tools": [{
                            "type": "function",
                            "name": "echo",
                            "parameters": schema.clone()
                        }]
                    },
                    {"type": "message", "role": "user", "content": "Call echo."}
                ],
                "tools": [{
                    "type": "function",
                    "name": "echo",
                    "parameters": schema
                }],
                "stream": false
            }),
            &profile,
        )
        .unwrap();

        assert_eq!(converted["tools"].as_array().unwrap().len(), 1);
        assert!(converted["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|message| !message.to_string().contains("additional_tools")));
        assert_eq!(
            converted["messages"].as_array().unwrap().last().unwrap(),
            &json!({"role": "user", "content": "Call echo."})
        );
    }

    #[test]
    fn compatible_responses_accepts_codex_fetch_schema_refresh() {
        let (mut route, mut model) = chat_route();
        route.wire = WireFormat::Responses;
        model.wire = WireFormat::Responses;
        let converted = prepare_upstream_request(
            &json!({
                "model": "vlm-glm",
                "input": [{
                    "type": "additional_tools",
                    "tools": [{
                        "type": "function",
                        "name": "_fetch",
                        "parameters": {
                            "type": "object",
                            "properties": {"legacy_request": {"type": "string"}}
                        }
                    }]
                }],
                "tools": [{
                    "type": "custom",
                    "name": "_fetch",
                    "description": "Current Codex fetch tool"
                }]
            }),
            &route,
            &model,
        )
        .unwrap();
        let fetch = converted["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|tool| tool["name"] == "_fetch")
            .collect::<Vec<_>>();
        assert_eq!(fetch.len(), 1);
        assert_eq!(fetch[0]["parameters"]["required"], json!(["input"]));
    }

    #[test]
    fn compatible_responses_prefers_current_custom_fetch_over_top_level_wrapper() {
        let (mut route, mut model) = chat_route();
        route.wire = WireFormat::Responses;
        model.wire = WireFormat::Responses;
        let converted = prepare_upstream_request(
            &json!({
                "model": "vlm-glm",
                "input": [{"type": "message", "role": "user", "content": "continue"}],
                "tools": [
                    {
                        "type": "function",
                        "name": "_fetch",
                        "description": "Generated compatibility wrapper",
                        "parameters": {
                            "type": "object",
                            "properties": {"request": {"type": "string"}},
                            "required": ["request"]
                        }
                    },
                    {
                        "type": "custom",
                        "name": "_fetch",
                        "description": "Current Codex free-form fetch tool"
                    }
                ]
            }),
            &route,
            &model,
        )
        .unwrap();
        let fetch = converted["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|tool| tool["name"] == "_fetch")
            .collect::<Vec<_>>();
        assert_eq!(fetch.len(), 1);
        assert_eq!(fetch[0]["parameters"]["required"], json!(["input"]));
        assert!(fetch[0]["parameters"]["properties"]
            .get("request")
            .is_none());
    }

    #[test]
    fn chat_glm_prefers_current_custom_fetch_over_top_level_wrapper() {
        let (route, model) = chat_route();
        let converted = prepare_upstream_request(
            &json!({
                "model": "vlm-glm",
                "input": [{"type": "message", "role": "user", "content": "continue"}],
                "tools": [
                    {
                        "type": "function",
                        "name": "_fetch",
                        "parameters": {
                            "type": "object",
                            "properties": {"request": {"type": "string"}},
                            "required": ["request"]
                        }
                    },
                    {"type": "custom", "name": "_fetch"}
                ]
            }),
            &route,
            &model,
        )
        .unwrap();
        let fetch = converted["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|tool| tool["function"]["name"] == "_fetch")
            .collect::<Vec<_>>();
        assert_eq!(fetch.len(), 1);
        assert_eq!(
            fetch[0]["function"]["parameters"]["required"],
            json!(["input"])
        );
    }

    #[test]
    fn chat_glm_keeps_namespace_for_conflicting_fetch_helpers() {
        let (route, model) = chat_route();
        let converted = prepare_upstream_request(
            &json!({
                "model": "vlm-glm",
                "input": [{"type": "message", "role": "user", "content": "continue"}],
                "tools": [
                    {
                        "type": "namespace",
                        "name": "codex_app",
                        "tools": [{
                            "type": "function",
                            "name": "_fetch",
                            "parameters": {
                                "type": "object",
                                "properties": {"request": {"type": "string"}},
                                "required": ["request"]
                            }
                        }]
                    },
                    {
                        "type": "namespace",
                        "name": "plugin_management",
                        "tools": [{
                            "type": "function",
                            "name": "_fetch",
                            "parameters": {
                                "type": "object",
                                "properties": {"cursor": {"type": "string"}},
                                "required": ["cursor"]
                            }
                        }]
                    }
                ]
            }),
            &route,
            &model,
        )
        .unwrap();
        let names = converted["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["codex_app___fetch", "plugin_management___fetch"]
        );
    }

    #[test]
    fn chat_glm_custom_fetch_and_namespaced_fetch_are_distinct_contracts() {
        let (route, model) = chat_route();
        let converted = prepare_upstream_request(
            &json!({
                "model": "vlm-glm",
                "input": [{"type": "message", "role": "user", "content": "continue"}],
                "tools": [
                    {
                        "type": "namespace",
                        "name": "codex_app",
                        "tools": [{
                            "type": "function",
                            "name": "_fetch",
                            "parameters": {
                                "type": "object",
                                "properties": {"request": {"type": "string"}},
                                "required": ["request"]
                            }
                        }]
                    },
                    {"type": "custom", "name": "_fetch"}
                ]
            }),
            &route,
            &model,
        )
        .unwrap();
        let names = converted["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["codex_app___fetch", "_fetch"]);
    }

    #[test]
    fn grok_request_uses_readable_reasoning_without_ciphertext() {
        let (route, model) = grok_route();
        let converted = prepare_upstream_request(
            &json!({
                "model": "vlm-grok",
                "input": [
                    {
                        "type": "reasoning",
                        "id": "reasoning_native",
                        "encrypted_content": "grok-opaque-state",
                        "summary": [{"type": "summary_text", "text": "keep the decision"}],
                        "internal_trace": "remove-me"
                    },
                    {
                        "type": "reasoning",
                        "id": "reasoning_empty",
                        "encrypted_content": null,
                        "summary": []
                    },
                    {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "continue"}]
                    }
                ]
            }),
            &route,
            &model,
        )
        .unwrap();
        let input = converted["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "reasoning");
        assert!(input[0].get("encrypted_content").is_none());
        assert_eq!(input[0]["summary"][0]["text"], "keep the decision");
        assert!(input[0].get("internal_trace").is_none());
        assert_eq!(input[1]["role"], "user");
    }

    #[test]
    fn grok_compaction_item_fails_closed_instead_of_dropping_context() {
        let (route, model) = grok_route();
        let error = prepare_upstream_request(
            &json!({
                "input": [{"type": "compaction", "id": "cmp_unmaterialized"}]
            }),
            &route,
            &model,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unmaterialized compaction"));
    }

    #[tokio::test]
    #[ignore = "requires a local Grok login, network access, and consumes a small request"]
    async fn live_grok_accepts_the_normalized_codex_request_shape() {
        let (route, model) = grok_route();
        let request = json!({
            "model": "vlm-grok",
            "input": [
                {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [{
                        "type": "namespace",
                        "name": "workspace",
                        "tools": [{
                            "type": "function",
                            "name": "read",
                            "parameters": {"type": "object"}
                        }]
                    }]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Reply with OK only."}]
                }
            ],
            "tools": [{"type": "web_search"}],
            "stream": false
        });
        let converted = prepare_upstream_request(&request, &route, &model).unwrap();
        let credential = crate::grok_auth::resolve().await.unwrap();
        let root = crate::grok_auth::grok_home();
        let version =
            crate::grok_auth::client_version(&root).unwrap_or_else(|| "unknown".to_string());
        let mut builder = reqwest::Client::new()
            .post("https://cli-chat-proxy.grok.com/v1/responses")
            .bearer_auth(credential.access_token.as_str())
            .header("x-xai-token-auth", "xai-grok-cli")
            .header("x-authenticateresponse", "authenticate-response")
            .header("x-grok-client-mode", "headless")
            .header("x-grok-req-id", "vellum-live-shape-test")
            .header("x-grok-model-override", "grok-4.5")
            .header("x-grok-session-id", "vellum-live-shape-test")
            .header("x-grok-conv-id", "vellum-live-shape-test")
            .header("x-grok-turn-idx", "0")
            .header("x-grok-client-identifier", "grok-shell")
            .header("x-grok-client-version", version)
            .json(&converted);
        if let Some(agent_id) = crate::grok_auth::agent_id(&root) {
            builder = builder.header("x-grok-agent-id", agent_id);
        }
        if let Some(user_id) = credential.user_id {
            builder = builder.header("x-grok-user-id", user_id);
        }
        let response = builder.send().await.unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert!(status.is_success(), "Grok returned {status}: {body}");
    }

    #[test]
    fn reasoning_is_not_emitted_as_answer_text() {
        let response = chat_response_to_responses(
            &json!({
                "id": "chat_1",
                "choices": [{"message": {"content": "answer", "reasoning_content": "thought"}}]
            }),
            "glm",
        )
        .unwrap();
        assert_eq!(response["output"][0]["type"], "reasoning");
        assert_eq!(response["output"][1]["content"][0]["text"], "answer");
        assert_eq!(response["output"][1]["phase"], "final_answer");
    }

    #[test]
    fn non_streaming_chat_content_parts_are_preserved_for_guardian_validation() {
        let response = chat_response_to_responses(
            &json!({
                "id": "chat_guardian_parts",
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": [{
                            "type": "text",
                            "text": "{\"outcome\":\"allow\"}"
                        }]
                    }
                }]
            }),
            "nemotron-3-ultra",
        )
        .unwrap();
        assert_eq!(response["status"], "completed");
        assert_eq!(
            response["output"][0]["content"][0]["text"],
            "{\"outcome\":\"allow\"}"
        );
    }

    #[test]
    fn non_streaming_reasoning_only_completion_fails_closed() {
        let response = chat_response_to_responses(
            &json!({
                "id": "chat_reasoning_only",
                "choices": [{"message": {
                    "content": "",
                    "reasoning_content": "I should call a tool next."
                }}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14}
            }),
            "glm",
        )
        .unwrap();
        assert_eq!(response["status"], "failed");
        assert_eq!(response["error"]["type"], "empty_completion");
        assert_eq!(response["usage"]["total_tokens"], 14);
    }

    #[test]
    fn streaming_chat_tool_calls_are_materialized_as_responses_items() {
        let mut adapter = ChatSseAdapter::new("glm");
        let first = adapter.push_data(
            &json!({
                "choices": [{"delta": {
                    "content": "I will inspect.",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_123",
                        "function": {"name": "shell_command", "arguments": "{\"command\":"}
                    }]
                }}]
            })
            .to_string(),
        );
        let second = adapter.push_data(
            &json!({
                "choices": [{"delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {"arguments": "\"git status\"}"}
                    }]
                }}]
            })
            .to_string(),
        );
        let done = adapter.push_data("[DONE]");
        let events = [first, second, done].concat().join("");
        assert!(events.contains("response.function_call_arguments.delta"));
        assert!(events.contains("response.function_call_arguments.done"));
        let response = adapter.completed_response();
        let call = response["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call")
            .unwrap();
        assert_eq!(call["call_id"], "call_123");
        assert_eq!(call["name"], "shell_command");
        assert_eq!(call["arguments"], "{\"command\":\"git status\"}");
        let message = response["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "message")
            .unwrap();
        assert_eq!(message["phase"], "commentary");
        assert!(events.contains("\"phase\":\"commentary\""));
    }

    #[test]
    fn custom_tools_round_trip_as_native_codex_custom_items() {
        let request = json!({
            "model": "glm",
            "stream": true,
            "tools": [{
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply a patch"
            }],
            "input": [
                {"role": "user", "content": "edit the file"},
                {
                    "type": "custom_tool_call",
                    "call_id": "old_call",
                    "name": "apply_patch",
                    "input": "*** Begin Patch\n*** End Patch"
                },
                {
                    "type": "custom_tool_call_output",
                    "call_id": "old_call",
                    "output": "Done"
                }
            ]
        });
        let converted = responses_to_chat(&request).unwrap();
        // Issue #6 Phase 3: apply_patch is a first-class exact contract, not an
        // anonymous `{input: string}` blob.
        assert_eq!(converted["tools"][0]["function"]["name"], "apply_patch");
        let parameters = &converted["tools"][0]["function"]["parameters"];
        assert_eq!(parameters["required"][0], "patch");
        assert_eq!(parameters["additionalProperties"], false);
        assert!(converted["tools"][0]["function"]["description"]
            .as_str()
            .unwrap()
            .contains("*** Begin Patch"));
        // The history item now carries the patch under the declared field.
        let call = converted["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|message| message.pointer("/tool_calls/0/function/arguments"))
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(call).unwrap()["patch"],
            "*** Begin Patch\n*** End Patch"
        );
        assert!(converted["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message
                    .pointer("/tool_calls/0/function/name")
                    .and_then(Value::as_str)
                    == Some("apply_patch")
            }));
        assert!(converted["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message.get("role").and_then(Value::as_str) == Some("tool")
                    && message.get("tool_call_id").and_then(Value::as_str) == Some("old_call")
            }));

        let mut adapter = ChatSseAdapter::new_with_request("glm", &request);
        let first = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_patch",
                        "function": {
                            "name": "apply_patch",
                            "arguments": "{\"input\":\"*** Begin"
                        }
                    }]
                }
            }]
        });
        let second = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {"arguments": " Patch\\n*** End Patch\"}"}
                    }]
                }
            }]
        });
        let mut events = adapter.push_data(&first.to_string()).join("");
        events.push_str(&adapter.push_data(&second.to_string()).join(""));
        events.push_str(&adapter.push_data("[DONE]").join(""));
        assert!(events.contains("\"type\":\"custom_tool_call\""));
        assert!(events.contains("response.custom_tool_call_input.done"));
        assert!(!events.contains("response.function_call_arguments.done"));
        let completed = adapter.completed_response();
        let item = completed
            .get("output")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .find(|item| item.get("type").and_then(Value::as_str) == Some("custom_tool_call"))
            .unwrap();
        assert_eq!(item["name"], "apply_patch");
        assert_eq!(item["input"], "*** Begin Patch\n*** End Patch");
        assert!(item["id"].as_str().unwrap().starts_with("ctc_"));
        assert_eq!(item["call_id"], "call_patch");
    }

    #[test]
    fn apply_patch_returned_under_the_exact_field_restores_a_native_custom_item() {
        // A model answering the new exact contract replies with `patch`, and
        // Codex Desktop still has to receive a native `custom_tool_call` so it
        // rebuilds its patch card and drives the real apply_patch runtime.
        let request = json!({
            "model": "grok",
            "tools": [{"type": "custom", "name": "apply_patch"}]
        });
        let context = CodexToolContext::from_request(&request);
        let mut item = json!({
            "type": "function_call",
            "id": "fc_patch",
            "call_id": "call_patch",
            "name": "apply_patch",
            "arguments": "{\"patch\":\"*** Begin Patch\\n*** End Patch\"}"
        });
        assert!(context.restore_output_item(&mut item));
        assert_eq!(item["type"], "custom_tool_call");
        assert!(item["id"].as_str().unwrap().starts_with("ctc_"));
        assert_eq!(item["input"], "*** Begin Patch\n*** End Patch");
        assert!(item.get("arguments").is_none());
    }

    #[test]
    fn grok_style_apply_patch_fences_are_normalized_before_codex_runtime() {
        let request = json!({
            "tools": [{"type": "custom", "name": "apply_patch"}]
        });
        let context = CodexToolContext::from_request(&request);
        let mut item = json!({
            "type": "function_call",
            "call_id": "call_patch",
            "name": "apply_patch",
            "arguments": "{\"patch\":\"*** Begin Patch ***\\n*** Update File: solution.py\\n@@\\n-old\\n+new\\n*** End Patch ***\\n\"}"
        });
        assert!(context.restore_output_item(&mut item));
        assert_eq!(
            item["input"],
            "*** Begin Patch\n*** Update File: solution.py\n@@\n-old\n+new\n*** End Patch\n"
        );
    }

    #[test]
    fn structured_tool_output_is_text_for_chat_provider_compatibility() {
        let request = json!({
            "model": "z-ai/glm-5.2",
            "input": [{
                "type": "function_call_output",
                "call_id": "call_structured",
                "output": {
                    "content": [{"type": "output_text", "text": "done"}],
                    "metadata": {"exit_code": 0}
                }
            }]
        });

        let converted = responses_to_chat(&request).unwrap();
        let tool_message = &converted["messages"][0];
        assert_eq!(tool_message["role"], "tool");
        assert_eq!(tool_message["tool_call_id"], "call_structured");
        assert!(tool_message["content"].is_string());
    }

    #[test]
    fn streaming_chat_emits_official_lifecycle_and_final_answer_phase() {
        let mut adapter = ChatSseAdapter::new("glm");
        let mut events = adapter.push_data(
            &json!({
                "choices": [{"delta": {
                    "reasoning_content": "checked",
                    "content": "done"
                }}]
            })
            .to_string(),
        );
        events.extend(adapter.push_data("[DONE]"));
        let events = events.join("");
        for expected in [
            "response.created",
            "response.in_progress",
            "response.reasoning_summary_part.added",
            "response.reasoning_summary_text.delta",
            "response.reasoning_summary_text.done",
            "response.reasoning_summary_part.done",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
            "\"phase\":\"final_answer\"",
        ] {
            assert!(events.contains(expected), "missing event/field: {expected}");
        }
    }

    #[test]
    fn message_accompanying_tool_call_is_commentary() {
        let response = chat_response_to_responses(
            &json!({
                "id": "chat_with_tool",
                "choices": [{"message": {
                    "content": "I will inspect the workspace.",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {"name": "shell_command", "arguments": "{\"command\":\"git status\"}"}
                    }]
                }}]
            }),
            "glm",
        )
        .unwrap();
        let message = response["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "message")
            .unwrap();
        assert_eq!(message["phase"], "commentary");
    }

    #[test]
    fn streaming_chat_usage_is_preserved_in_completed_response() {
        let mut adapter = ChatSseAdapter::new("glm");
        let mut events = adapter.push_data(
            &json!({
                "choices": [{"delta": {"content": "完成"}}],
                "usage": null
            })
            .to_string(),
        );
        events.extend(
            adapter.push_data(
                &json!({
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 1234,
                        "completion_tokens": 56,
                        "total_tokens": 1290,
                        "prompt_tokens_details": {"cached_tokens": 900},
                        "completion_tokens_details": {"reasoning_tokens": 12}
                    }
                })
                .to_string(),
            ),
        );
        events.extend(adapter.push_data("[DONE]"));
        assert!(events.join("").contains("response.completed"));

        let response = adapter.completed_response();
        assert_eq!(response["usage"]["input_tokens"], 1234);
        assert_eq!(
            response["usage"]["input_tokens_details"]["cached_tokens"],
            900
        );
        assert_eq!(response["usage"]["output_tokens"], 56);
        assert_eq!(
            response["usage"]["output_tokens_details"]["reasoning_tokens"],
            12
        );
        assert_eq!(response["usage"]["total_tokens"], 1290);
    }

    #[test]
    fn empty_chat_completion_fails_instead_of_silently_completing_task() {
        let mut adapter = ChatSseAdapter::new("glm");
        let events = adapter.push_data("[DONE]").join("");
        assert!(events.contains("response.failed"));
        assert!(events.contains("empty_completion"));
        assert!(!events.contains("response.completed"));
    }

    #[test]
    fn readable_replay_removes_ciphertext_but_keeps_summary() {
        let mut value = json!({
            "input": [
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [{"type": "summary_text", "text": "keep decision"}],
                    "encrypted_content": "secret-cipher"
                },
                {
                    "type": "reasoning",
                    "id": "rs_empty",
                    "summary": [],
                    "encrypted_content": "only-cipher"
                }
            ]
        });
        strip_opaque_reasoning_keep_summary(&mut value);
        let encoded = serde_json::to_string(&value).unwrap();
        assert!(!encoded.contains("secret-cipher"));
        assert!(!encoded.contains("only-cipher"));
        assert!(encoded.contains("keep decision"));
        assert!(!encoded.contains("rs_empty"));
    }

    #[test]
    fn readable_replay_promotes_provider_reasoning_content_to_summary() {
        let mut value = json!({
            "input": [{
                "type": "reasoning",
                "reasoning_content": "retain the migration decision",
                "encrypted_content": "foreign"
            }]
        });
        strip_opaque_reasoning_keep_summary(&mut value);
        assert_eq!(
            value["input"][0]["summary"][0]["text"],
            "retain the migration decision"
        );
        assert!(value["input"][0].get("reasoning_content").is_none());
        assert!(value["input"][0].get("encrypted_content").is_none());
    }

    #[test]
    fn chat_conversion_replays_readable_summary_in_reasoning_channel() {
        let converted = vellum_proxy_runtime::responses_to_chat_with_options(
            &json!({
                "model": "reasoning-model",
                "stream": false,
                "input": [
                    {
                        "type": "reasoning",
                        "summary": [{"type": "summary_text", "text": "retain this decision"}]
                    },
                    {"role": "assistant", "content": "working"},
                    {"role": "user", "content": "continue"}
                ]
            }),
            &vellum_proxy_runtime::HarnessProfile::generic(
                vellum_proxy_runtime::RuntimeWireFormat::Chat,
            ),
            &vellum_proxy_runtime::RuntimeChatCapabilities::tool_call_bound_reasoning(),
            &vellum_proxy_runtime::ReplayContext::default(),
        )
        .unwrap();
        let messages = converted["messages"].as_array().unwrap();
        assert!(messages.iter().any(|message| {
            message.get("reasoning_content").and_then(Value::as_str) == Some("retain this decision")
        }));
        assert!(!serde_json::to_string(&converted)
            .unwrap()
            .contains("encrypted_content"));
    }

    fn stream_events(chunks: &[Value]) -> String {
        let mut adapter = ChatSseAdapter::new("qwen");
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(adapter.push_data(&chunk.to_string()));
        }
        events.extend(adapter.push_data("[DONE]"));
        events.join("")
    }

    #[test]
    fn no_outgoing_chat_message_carries_null_content() {
        // Ollama rejects the entire request with
        // `invalid message content type: <nil>` for any message whose content
        // is null and which has no tool_calls. Replaying a turn that contained
        // reasoning produced exactly that shape, so the next request after any
        // thinking turn failed with HTTP 400.
        let converted = responses_to_chat(&json!({
            "model": "qwen",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "list files"}]},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "I should look"}]},
                {"type": "function_call", "call_id": "c1", "name": "ls", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "a.txt"},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "thanks"}]}
            ]
        }))
        .unwrap();

        let messages = converted["messages"].as_array().unwrap();
        assert!(
            messages.len() >= 4,
            "expected a replayed turn, got {messages:?}"
        );
        for message in messages {
            assert!(
                !message["content"].is_null(),
                "null content in {message} — upstream rejects the whole request"
            );
        }
    }

    #[test]
    fn ollama_tool_continuation_never_ends_with_adjacent_assistant_messages() {
        // Regression fixture from Codex task 019fd78f: after three successful
        // tool calls the model emitted two reasoning-only continuations. The
        // old item-by-item conversion left two assistant messages at the end,
        // which Ollama rejected before the model could continue.
        let request = json!({
            "model": "qwen",
            "input": [
                {"type": "message", "role": "user", "content": "inspect the repository"},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "inspect first"}]},
                {"type": "message", "role": "assistant", "content": "I will inspect it."},
                {"type": "function_call", "call_id": "c1", "name": "shell_command", "arguments": "{\"command\":\"dir\"}"},
                {"type": "function_call_output", "call_id": "c1", "output": "timed out"},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "retry narrowly"}]},
                {"type": "function_call", "call_id": "c2", "name": "shell_command", "arguments": "{\"command\":\"Get-ChildItem\"}"},
                {"type": "function_call_output", "call_id": "c2", "output": "path error"},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "quote the path"}]},
                {"type": "function_call", "call_id": "c3", "name": "shell_command", "arguments": "{\"command\":\"pwd\"}"},
                {"type": "function_call_output", "call_id": "c3", "output": "parser error"},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "continue from the workspace"}]},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "use the next tool"}]}
            ]
        });
        let converted = responses_to_chat(&request).unwrap();

        let messages = converted["messages"].as_array().unwrap();
        assert!(messages
            .windows(2)
            .all(|pair| { !(pair[0]["role"] == "assistant" && pair[1]["role"] == "assistant") }));
        let tail = messages.last().unwrap();
        assert_eq!(
            tail["role"], "tool",
            "default Chat continuation keeps the tool result as the tail: {messages:?}"
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["role"] == "user"
                    && message["content"] == "inspect the repository")
                .count(),
            1
        );

        let first_tool_index = messages
            .iter()
            .position(|message| message["role"] == "tool")
            .unwrap();
        let assistant = &messages[first_tool_index - 1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["tool_calls"].as_array().unwrap().len(), 1);
        assert!(assistant["content"]
            .as_str()
            .unwrap()
            .contains("I will inspect it."));

        let bridged = vellum_proxy_runtime::responses_to_chat_with_options(
            &request,
            &vellum_proxy_runtime::HarnessProfile::generic(
                vellum_proxy_runtime::RuntimeWireFormat::Chat,
            ),
            &vellum_proxy_runtime::RuntimeChatCapabilities::qwen_bridge(),
            &vellum_proxy_runtime::ReplayContext::default(),
        )
        .unwrap();
        let bridged_messages = bridged["messages"].as_array().unwrap();
        assert_eq!(
            bridged_messages.last().unwrap()["content"],
            vellum_proxy_runtime::NEUTRAL_USER_BRIDGE_TEXT
        );
    }

    #[test]
    fn streamed_reasoning_reaches_codex_whichever_field_the_provider_uses() {
        // Ollama streams `reasoning`; vLLM and DeepSeek stream
        // `reasoning_content`. Only the second was recognised, so on Ollama
        // every thinking token was dropped: Codex saw an empty stream for the
        // whole reasoning phase, and if the token budget ran out before the
        // first word of the answer the turn produced nothing at all.
        for field in ["reasoning", "reasoning_content"] {
            let events = stream_events(&[
                json!({"choices": [{"delta": {field: "weighing the options"}}]}),
                json!({"choices": [{"delta": {"content": "42"}}]}),
            ]);

            assert!(
                events.contains("response.reasoning_summary_text.delta"),
                "no reasoning event for `{field}`"
            );
            assert!(
                events.contains("weighing the options"),
                "reasoning text dropped for `{field}`"
            );
            assert!(events.contains("42"), "answer dropped for `{field}`");
        }
    }

    #[test]
    fn a_turn_that_only_reasons_reports_the_thinking_and_still_fails_closed() {
        // The budget ran out mid-thought, so there is no answer. Two things have
        // to hold: the reasoning the model did produce must reach Codex rather
        // than being thrown away, and the turn must still fail explicitly
        // instead of completing with nothing — Vellum cannot invent an answer
        // the model never gave.
        let events = stream_events(&[
            json!({"choices": [{"delta": {"reasoning": "still thinking"}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "length"}]}),
        ]);

        assert!(
            events.contains("still thinking"),
            "reasoning-only turn came out empty"
        );
        assert!(
            events.contains("empty_completion"),
            "must not pretend it answered"
        );
        assert!(!events.contains("response.completed"));
    }

    #[test]
    fn non_streamed_reasoning_is_read_from_either_field() {
        for field in ["reasoning", "reasoning_content"] {
            let converted = chat_response_to_responses(
                &json!({
                    "choices": [{"message": {"role": "assistant", "content": "42", field: "thought it through"}}]
                }),
                "qwen",
            )
            .unwrap();

            let output = serde_json::to_string(&converted["output"]).unwrap();
            assert!(
                output.contains("thought it through"),
                "reasoning dropped for `{field}` in {output}"
            );
        }
    }

    /// A 1x1 PNG, small enough to compare byte for byte.
    const IMAGE_DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    fn user_turn_with_image(image_part: Value) -> Value {
        json!({
            "model": "qwen",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "這是什麼電腦"},
                    image_part
                ]
            }]
        })
    }

    #[test]
    fn an_attached_image_reaches_the_chat_request() {
        // The whole point: the bytes have to leave Vellum. Flattening content to
        // text used to drop image parts, and the model then answered a question
        // about a picture it never received by inventing one.
        let converted = responses_to_chat(&user_turn_with_image(
            json!({"type": "input_image", "image_url": IMAGE_DATA_URL}),
        ))
        .unwrap();

        let content = &converted["messages"][0]["content"];
        assert!(
            content.is_array(),
            "expected multimodal content, got {content}"
        );
        let parts = content.as_array().unwrap();
        assert_eq!(parts[0], json!({"type": "text", "text": "這是什麼電腦"}));
        assert_eq!(
            parts[1],
            json!({"type": "image_url", "image_url": {"url": IMAGE_DATA_URL}})
        );

        // Assert on the serialized request too: this is what actually goes on
        // the wire, and it is the thing that was missing.
        let wire = serde_json::to_string(&converted).unwrap();
        assert!(
            wire.contains("iVBORw0KGgoAAAANSUhEUg"),
            "image payload absent from {wire}"
        );
    }

    #[test]
    fn an_image_url_object_is_accepted_as_well_as_a_bare_string() {
        let converted = responses_to_chat(&user_turn_with_image(json!({
            "type": "input_image",
            "image_url": {"url": IMAGE_DATA_URL},
            "detail": "high"
        })))
        .unwrap();

        assert_eq!(
            converted["messages"][0]["content"][1],
            json!({"type": "image_url", "image_url": {"url": IMAGE_DATA_URL, "detail": "high"}})
        );
    }

    #[test]
    fn a_text_only_turn_still_sends_a_plain_string() {
        // Providers and the rest of this module expect a string here, so the
        // richer shape must appear only when an image actually needs it.
        let converted = responses_to_chat(&json!({
            "model": "qwen",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }]
        }))
        .unwrap();

        assert_eq!(converted["messages"][0]["content"], json!("hello"));
    }

    #[test]
    fn an_unfetchable_image_reference_does_not_force_the_multimodal_shape() {
        // `file_id` points at something stored on the provider's side; a
        // third-party endpoint cannot resolve it, so there is nothing to send
        // and the turn stays exactly as it was before.
        let converted = responses_to_chat(&user_turn_with_image(
            json!({"type": "input_image", "file_id": "file-123"}),
        ))
        .unwrap();

        assert_eq!(converted["messages"][0]["content"], json!("這是什麼電腦"));
    }

    #[test]
    fn images_stay_out_of_tool_results() {
        // Tool messages must remain plain strings whatever their payload looks
        // like, or providers reject the turn.
        let converted = responses_to_chat(&json!({
            "model": "qwen",
            "input": [{
                "type": "function_call_output",
                "call_id": "call_1",
                "output": [
                    {"type": "input_text", "text": "ok"},
                    {"type": "input_image", "image_url": IMAGE_DATA_URL}
                ]
            }]
        }))
        .unwrap();

        let message = &converted["messages"][0];
        assert_eq!(message["role"], "tool");
        assert!(
            message["content"].is_string(),
            "tool content must be a string"
        );
    }
}
