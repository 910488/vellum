use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tower_http::catch_panic::CatchPanicLayer;

use crate::body::{
    error_envelope, models_list_payload, parse_json_request, request_body_error_response,
    ModelListEntry, RequestBodyError, MAX_REQUEST_BODY_BYTES,
};
use crate::error::RuntimeError;
use crate::exec::{stream_open_frame, ProxyRuntime, RuntimeResponse};
use crate::inbound::{InboundAccessPolicy, BOUNDARY_KEY_HEADER};
use crate::request::{IncomingAuthContext, RequestMetadata, RuntimeEndpoint, RuntimeRequest};
use crate::resource::{collect_request_body, CollectBodyError, ResourcePolicy};
use crate::state::ProxyRuntimeState;
use crate::streaming::{failed_sse_event, failed_sse_event_with_category};
use crate::websocket::responses_websocket;
use crate::PROXY_RUNTIME_VERSION;

pub fn health_payload(state: &dyn ProxyRuntimeState) -> Value {
    let diag = state.diagnostics();
    json!({
        "ok": diag.ok,
        "models": diag.model_count,
        "install_id": diag.identity.install_id,
        "host_id": diag.identity.host_id,
    })
}

pub fn readyz_payload(state: &dyn ProxyRuntimeState) -> (StatusCode, Value) {
    let diag = state.diagnostics();
    let body = json!({
        "ready": diag.ready,
        "identity": diag.identity,
        "model_count": diag.model_count,
        "listener_ready": diag.listener_ready,
        "config_valid": diag.config_valid,
        "secrets_readable": diag.secrets_readable,
        "notes": diag.notes,
    });
    if diag.ready {
        (StatusCode::OK, body)
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, body)
    }
}

pub fn version_payload(state: &dyn ProxyRuntimeState) -> Value {
    let identity = state.identity().with_binary_hash();
    json!({
        "proxy_runtime": PROXY_RUNTIME_VERSION,
        "install_id": identity.install_id,
        "host_id": identity.host_id,
        "image_version": identity.image_version,
        "config_hash": identity.config_hash,
        "version": identity.version,
        "commit": identity.commit,
        "binary_hash": identity.binary_hash,
    })
}

/// Build the proxy's HTTP surface.
///
/// `access_policy` is not optional and has no permissive default: a production
/// caller must pass [`InboundAccessPolicy::authenticated`]. Every route below
/// sits behind it, `/health` and `/readyz` included — an unauthenticated
/// liveness endpoint is still a probe a browser or a foreign local process can
/// use to find the proxy and fingerprint the install.
pub fn build_headless_router<S>(state: Arc<S>, access_policy: InboundAccessPolicy) -> Router
where
    S: ProxyRuntimeState,
{
    let resources = state.proxy_runtime().resource_policy();
    build_headless_router_with_resources(state, access_policy, resources)
}

/// Same surface as [`build_headless_router`], with an explicit resource
/// policy. Production callers go through [`build_headless_router`], which
/// uses the runtime's production (or config-lowered) policy.
pub fn build_headless_router_with_resources<S>(
    state: Arc<S>,
    access_policy: InboundAccessPolicy,
    resources: ResourcePolicy,
) -> Router
where
    S: ProxyRuntimeState,
{
    Router::new()
        .route("/health", get(health::<S>))
        .route("/readyz", get(readyz::<S>))
        .route("/version", get(version::<S>))
        .route("/diagnostics/usage", get(usage_diagnostics::<S>))
        .route("/v1/models", get(list_models::<S>))
        .route(
            "/v1/responses",
            get(responses_websocket::<S>).post(responses::<S>),
        )
        .route(
            "/responses",
            get(responses_websocket::<S>).post(responses::<S>),
        )
        .route("/v1/responses/compact", post(compact::<S>))
        .route("/responses/compact", post(compact::<S>))
        .route("/v1/alpha/search", post(search::<S>))
        .with_state(state)
        // Applied after `with_state` so it covers every route uniformly:
        // HTTP, SSE, the WebSocket upgrade, health, diagnostics, models,
        // search and compact all pass through the same guard.
        .layer(axum::middleware::from_fn_with_state(
            resources,
            resource_admission,
        ))
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(access_policy),
            inbound_guard,
        ))
        // Outermost request containment. Provider-controlled data is validated
        // for shape before any handler reads it, so this is not the mechanism
        // that keeps a hostile frame from panicking a stream — that belongs in
        // the parsers. It is here so that a bug anywhere under the boundary
        // answers with the canonical envelope instead of a dropped connection,
        // and so one request can never take down a shared runtime serving
        // others. Release builds deliberately keep `panic = "unwind"`.
        .layer(CatchPanicLayer::custom(panic_to_canonical_error))
}

/// Admit or refuse one inbound request, then strip the boundary key.
///
/// The key is removed on every path, including the test-only policy, so no
/// downstream handler can forward it: it authenticates the caller to *this*
/// proxy and has no meaning to any upstream provider.
async fn inbound_guard(
    State(policy): State<Arc<InboundAccessPolicy>>,
    mut request: Request,
    next: axum::middleware::Next,
) -> Response {
    if let Err(error) = policy.check(request.headers(), request.uri()) {
        return error.into_error_response();
    }
    request.headers_mut().remove(BOUNDARY_KEY_HEADER);
    next.run(request).await
}

/// Admit one request against the resource policy.
///
/// `/health` and `/readyz` take no long-task slot. Official WebSocket
/// upgrades take a WS slot independent of HTTP. Everything else takes one
/// HTTP slot. Shortage is the canonical `resource_exhausted` envelope.
async fn resource_admission(
    State(policy): State<ResourcePolicy>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path();
    let is_probe = path == "/health" || path == "/readyz";
    let is_official_ws =
        request.method() == Method::GET && (path == "/v1/responses" || path == "/responses");
    let _slot = if is_probe {
        None
    } else if is_official_ws {
        match policy.try_acquire_official_ws() {
            Ok(guard) => Some(guard),
            Err(error) => return error.into_error_response(),
        }
    } else {
        match policy.try_acquire_http() {
            Ok(guard) => Some(guard),
            Err(error) => return error.into_error_response(),
        }
    };
    next.run(request).await
}

async fn read_json_body(
    request: Request,
    endpoint: &str,
    policy: &ResourcePolicy,
) -> Result<(axum::http::HeaderMap, Value), Box<Response>> {
    let (parts, body) = request.into_parts();
    let headers = parts.headers;
    let body = match collect_request_body(body, MAX_REQUEST_BODY_BYTES, policy).await {
        Ok((body, _budget)) => body,
        Err(CollectBodyError::Resource(error)) => {
            return Err(Box::new(error.into_error_response()))
        }
        Err(CollectBodyError::WireTooLarge | CollectBodyError::Read(_)) => {
            return Err(Box::new(request_body_error_response(
                endpoint,
                RequestBodyError::WireTooLarge,
            )))
        }
    };
    match parse_json_request(&headers, &body) {
        Ok(body) => Ok((headers, body)),
        Err(error) => Err(Box::new(request_body_error_response(endpoint, error))),
    }
}

/// Render a caught panic as the canonical internal-error envelope.
///
/// The payload is never included: a panic message can carry arbitrary local
/// state, and the client is not the right audience for it.
fn panic_to_canonical_error(panic: Box<dyn std::any::Any + Send + 'static>) -> Response {
    let detail = panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic payload".into());
    log::error!("[Proxy] request handler panicked: {detail}");
    RuntimeError::Internal("the request failed unexpectedly".into()).into_error_response()
}

async fn health<S: ProxyRuntimeState>(State(state): State<Arc<S>>) -> Json<Value> {
    Json(health_payload(state.as_ref()))
}

async fn readyz<S: ProxyRuntimeState>(State(state): State<Arc<S>>) -> Response {
    let (status, body) = readyz_payload(state.as_ref());
    (status, Json(body)).into_response()
}

async fn version<S: ProxyRuntimeState>(State(state): State<Arc<S>>) -> Json<Value> {
    Json(version_payload(state.as_ref()))
}

async fn usage_diagnostics<S: ProxyRuntimeState>(State(state): State<Arc<S>>) -> Response {
    let runtime = state.proxy_runtime();
    let mut routes = Vec::new();
    for route in state.active_model_routes() {
        let summary = match runtime.usage_summary(&route.route_id) {
            Ok(summary) => summary,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "usage ledger unavailable"})),
                )
                    .into_response();
            }
        };
        routes.push(json!({
            "routeId": route.route_id,
            "catalogId": route.catalog_id,
            "model": route.upstream_model,
            "summary": summary,
        }));
    }
    Json(json!({
        "runtimeVersion": PROXY_RUNTIME_VERSION,
        "identity": state.identity(),
        "routes": routes,
    }))
    .into_response()
}

async fn list_models<S: ProxyRuntimeState>(State(state): State<Arc<S>>) -> Json<Value> {
    let routes = state.active_model_routes();
    let mut payload = models_list_payload(
        routes
            .iter()
            .map(|model| ModelListEntry {
                catalog_id: model.catalog_id.clone(),
                // `owned_by` on the wire is Desktop's route_id, unconditionally.
                // ModelRouteView.owned_by is daemon-side metadata for other
                // uses; it must never override the wire owner authority.
                owned_by: model.route_id.clone(),
                context_window: model.context_window,
            })
            .collect(),
    );
    let runtime = state.proxy_runtime();
    let search_enabled = runtime.web_search_wrapper_enabled();
    payload["models"] = Value::Array(
        routes
            .iter()
            .map(|route| {
                codex_model_metadata(
                    route,
                    runtime
                        .model_catalog_entry(&route.catalog_id)
                        .unwrap_or(Value::Null),
                    search_enabled,
                )
            })
            .collect(),
    );
    Json(payload)
}

fn codex_model_metadata(
    route: &crate::state::ModelRouteView,
    entry: Value,
    search_enabled: bool,
) -> Value {
    let mut metadata = json!({
        "slug": route.catalog_id,
        "display_name": route.catalog_id,
        "description": route.catalog_id,
        "default_reasoning_level": "medium",
        "supported_reasoning_levels": [
            {"effort": "low", "description": "Fast responses with lighter reasoning"},
            {"effort": "medium", "description": "Balances speed and reasoning depth"},
            {"effort": "high", "description": "Greater reasoning depth"},
            {"effort": "xhigh", "description": "Extra high reasoning depth"}
        ],
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 1,
        "additional_speed_tiers": [],
        "service_tiers": [],
        "availability_nux": null,
        "upgrade": null,
        "model_messages": {},
        "include_skills_usage_instructions": false,
        "include_plugin_usage_instructions": false,
        "default_reasoning_summary": "none",
        "support_verbosity": true,
        "apply_patch_tool_type": "freeform",
        "web_search_tool_type": "text_and_image",
        "truncation_policy": {"mode": "tokens", "limit": 10000},
        "supports_parallel_tool_calls": true,
        "supports_image_detail_original": true,
        "context_window": route.context_window.filter(|window| *window > 0).unwrap_or(128000),
        "max_context_window": route.context_window.filter(|window| *window > 0).unwrap_or(128000),
        "comp_hash": "vellum-proxy-runtime",
        "effective_context_window_percent": 95,
        "experimental_supported_tools": [],
        "input_modalities": ["text", "image"],
        "supports_search_tool": search_enabled,
        "use_responses_lite": false,
        "tool_mode": "code_mode_only",
        "multi_agent_version": "v2"
    });
    if let (Some(target), Some(source)) = (metadata.as_object_mut(), entry.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
    if let Some(target) = metadata.as_object_mut() {
        // Runtime route projections use `id`; modern Codex catalog entries use
        // `slug`. The route's public catalog id remains authoritative.
        target.remove("id");
        target.insert("slug".into(), Value::String(route.catalog_id.clone()));
        if route.catalog_id.starts_with("vlm-") {
            target.insert("supports_search_tool".into(), Value::Bool(search_enabled));
        }
    }
    metadata
}

pub(crate) fn build_request_metadata(
    headers: &HeaderMap,
    body: &Value,
    endpoint: RuntimeEndpoint,
    connection_id: Option<String>,
) -> RequestMetadata {
    let request_id = headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("req_{}", ulid::Ulid::new()));

    let header_scope = if connection_id.is_some() {
        crate::codex_metadata::CodexMetadataHeaderScope::ConnectionProjection
    } else {
        crate::codex_metadata::CodexMetadataHeaderScope::SameRequest
    };

    let sources = crate::codex_metadata::CodexMetadataSources {
        headers,
        body,
        endpoint,
        header_scope,
    };

    let codex_identity = match crate::codex_metadata::extract_codex_turn_identity(sources) {
        Ok(identity) => {
            if identity.source != crate::codex_metadata::CodexIdentitySource::None
                || identity.trust != crate::codex_metadata::CodexIdentityTrust::None
            {
                Some(identity)
            } else {
                None
            }
        }
        Err(e) => {
            tracing::warn!("Failed to extract Codex turn identity: {e}");
            None
        }
    };

    let codex_capabilities = codex_identity
        .as_ref()
        .map(|id| crate::codex_metadata::resolve_codex_capabilities(None, Some(id), None));

    RequestMetadata {
        request_id,
        received_at_ms: now_ms(),
        review_run_id: None,
        review_role: None,
        primary_failure_reason: None,
        connection_id,
        codex_identity,
        codex_capabilities,
        force_portable_official_handoff: false,
        // Always `None` at the HTTP boundary. Only the guardian dispatch may
        // name a billing account, from the operator's own saved Auto Review
        // policy -- a client must never be able to pick which of this host's
        // ChatGPT accounts pays for its request by sending a header.
        review_official_account_id: None,
    }
}

async fn responses<S: ProxyRuntimeState>(
    State(state): State<Arc<S>>,
    request: Request,
) -> Response {
    let resources = state.proxy_runtime().resource_policy();
    let (headers, body) = match read_json_body(request, "/responses", &resources).await {
        Ok(parsed) => parsed,
        Err(response) => return *response,
    };
    let catalog_id = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // The runtime only ever sees the fields it needs to forward — never a
    // full header dump, so it cannot leak unrelated headers upstream.
    let incoming_auth = IncomingAuthContext {
        authorization: headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
        openai_account: headers
            .get("openai-account")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    };
    let metadata = build_request_metadata(&headers, &body, RuntimeEndpoint::Responses, None);
    let request_id = metadata.request_id.clone();
    let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let ledger_request_id = request_id.clone();
    let execution_id = crate::exec::new_execution_id();
    let runtime_request = RuntimeRequest {
        body,
        endpoint: RuntimeEndpoint::Responses,
        incoming_auth,
        // The executor contract comes from explicit config/state, never from
        // this process's own environment (plan §25.2): the headless daemon is
        // forbidden from self-deriving the contract its upstream model sees.
        execution_environment: state.execution_environment(),
        metadata,
    };
    if streaming {
        // Commit the inbound HTTP status before `execute` waits on upstream
        // headers. Hyper holds the status line until this handler returns a
        // body that is ready on the first poll; awaiting execute first is
        // what made live third-party turns sit at ~265s to HTTP 200.
        let runtime = state.proxy_runtime();
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<Result<Bytes, crate::error::RuntimeError>>(8);
        let started = std::time::Instant::now();
        tokio::spawn(async move {
            let send = |item: Result<Bytes, crate::error::RuntimeError>| {
                let tx = tx.clone();
                async move {
                    let _ = tx.send(item).await;
                }
            };
            let mut first_downstream_ms = None;
            let result = runtime.execute(runtime_request, &execution_id).await;
            match result {
                Ok(RuntimeResponse::Sse(mut inner)) => {
                    let mut forwarded = 0u32;
                    let mut disconnected = false;
                    while let Some(item) = inner.next().await {
                        forwarded += 1;
                        // Only an application frame counts as the downstream
                        // first frame; the prepended `: vellum-stream-open`
                        // flush comment is not one and must not collapse the
                        // upstream/downstream split.
                        let is_application = item
                            .as_ref()
                            .map(|bytes| !bytes.starts_with(b": "))
                            .unwrap_or(false);
                        if first_downstream_ms.is_none() && is_application {
                            first_downstream_ms = Some(started.elapsed().as_millis() as u64);
                        }
                        if tx.send(item).await.is_err() {
                            disconnected = true;
                            break;
                        }
                    }
                    // The usage row is written inside the stream once its
                    // terminal event is recorded, so the downstream mark must
                    // come after the loop rather than on the first frame.
                    if let Some(ms) = first_downstream_ms {
                        runtime.mark_first_downstream_frame(&ledger_request_id, ms);
                    }
                    if disconnected {
                        // The client is gone. Over HTTP SSE there is no
                        // `response.cancel` backchannel, so the dropped body is
                        // the bridge's only cancel signal. Fan the cancel out to
                        // every confidently bound sub-agent child and release
                        // the registry slot, then give the parent its own
                        // bounded terminal row (INV-CANCEL-1/2 parity with the
                        // WebSocket bridge).
                        runtime.cancel_request_tree(&execution_id);
                        runtime.unregister_request_cancel(&execution_id);
                        record_terminal_bridge_outcome(
                            &runtime,
                            &catalog_id,
                            &execution_id,
                            &ledger_request_id,
                            started,
                            first_downstream_ms,
                            "client_disconnect",
                        );
                    } else if runtime.is_request_cancelled(&execution_id) {
                        // The stream ended without a terminal event because a
                        // parent-cancel fan-out dropped this request's upstream.
                        // Give a child its own bounded 499 row like the
                        // WebSocket bridge does.
                        runtime.unregister_request_cancel(&execution_id);
                        record_terminal_bridge_outcome(
                            &runtime,
                            &catalog_id,
                            &execution_id,
                            &ledger_request_id,
                            started,
                            first_downstream_ms,
                            "client_cancel",
                        );
                    }
                    let _ = forwarded;
                }
                Ok(RuntimeResponse::Json(value)) => {
                    let payload = serde_json::to_string(&value).unwrap_or_else(|_| "{}".into());
                    send(Ok(Bytes::from(format!(
                        "event: response.completed\ndata: {payload}\n\n"
                    ))))
                    .await;
                }
                Ok(RuntimeResponse::Raw { status, body, .. }) => {
                    let message =
                        format!("upstream HTTP {status}: {}", String::from_utf8_lossy(&body));
                    send(Ok(Bytes::from(failed_sse_event(&message)))).await;
                }
                Err(error) => {
                    send(Ok(Bytes::from(failed_sse_event_with_category(
                        error.category(),
                        &error.to_string(),
                    ))))
                    .await;
                }
            }
        });
        let inner = async_stream::stream! {
            while let Some(item) = rx.recv().await {
                yield item;
            }
        };
        return sse_http_response(
            futures_util::stream::iter(std::iter::once(Ok(stream_open_frame())))
                .chain(inner)
                .boxed(),
        );
    }
    match state
        .proxy_runtime()
        .execute(runtime_request, &execution_id)
        .await
    {
        Ok(RuntimeResponse::Json(value)) => (StatusCode::OK, Json(value)).into_response(),
        Ok(RuntimeResponse::Sse(stream)) => sse_http_response(stream),
        Ok(RuntimeResponse::Raw {
            status,
            content_type,
            body,
        }) => raw_response(status, content_type, body),
        Err(error) => desktop_compatible_error(state.as_ref(), &catalog_id, "/responses", error),
    }
}

/// Record the bounded terminal row for an HTTP bridge request whose stream was
/// cut short without a terminal event: a parent dropped by client disconnect,
/// or a child dropped by parent-cancel fan-out. First-writer-wins on
/// `execution_id` (the internal cancel-registry key, never the external
/// `request_id`) keeps this from overwriting a race that already recorded a
/// terminal outcome (INV-CANCEL-1).
fn record_terminal_bridge_outcome(
    runtime: &ProxyRuntime,
    catalog_id: &str,
    execution_id: &str,
    request_id: &str,
    started: std::time::Instant,
    first_downstream_ms: Option<u64>,
    outcome: &str,
) {
    let (route_id, provider, model) = runtime.resolved_usage_identity(catalog_id);
    let duration_ms = started.elapsed().as_millis() as u64;
    let stages = json!({
        "clientAcceptMs": 0,
        "requestForwardedMs": 0,
        "firstUpstreamApplicationFrameMs": None::<u64>,
        "firstDownstreamFrameMs": first_downstream_ms,
        "terminalMs": duration_ms,
    });
    let error = if outcome == "client_disconnect" {
        "client closed"
    } else {
        "cancelled by parent"
    };
    runtime.finish_terminal_once(
        execution_id,
        &route_id,
        &provider,
        &model,
        request_id,
        None,
        duration_ms,
        None,
        None,
        first_downstream_ms,
        outcome,
        Some(outcome),
        Some(error),
        Some(stages),
    );
}

fn sse_http_response(
    stream: futures_util::stream::BoxStream<
        'static,
        Result<axum::body::Bytes, crate::error::RuntimeError>,
    >,
) -> Response {
    let mut response = Response::new(Body::from_stream(stream));
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    headers.insert(
        axum::http::header::CONNECTION,
        HeaderValue::from_static("keep-alive"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
}

fn desktop_compatible_error(
    state: &dyn ProxyRuntimeState,
    catalog_id: &str,
    endpoint: &str,
    error: crate::error::RuntimeError,
) -> Response {
    let provider_error = match &error {
        crate::error::RuntimeError::ProviderUnauthorized(message) => Some((401, message.as_str())),
        crate::error::RuntimeError::ProviderQuota { message, .. } => Some((429, message.as_str())),
        // The provider answered with a 5xx. Transport reachability errors use
        // the same taxonomy internally but have an "upstream unavailable"
        // marker and retain the runtime's 503 classification.
        crate::error::RuntimeError::ProviderUnavailable(message)
            if !message.starts_with("upstream unavailable:") =>
        {
            Some((500, message.as_str()))
        }
        _ => None,
    };
    let Some((status, message)) = provider_error else {
        return error.into_error_response();
    };
    let route = state.route_for_model(catalog_id);
    let provider = route
        .as_ref()
        .and_then(|route| route.owned_by.as_deref())
        .unwrap_or("unknown");
    let model = route
        .as_ref()
        .map(|route| route.upstream_model.as_str())
        .unwrap_or(catalog_id);
    let cause = serde_json::json!({"error": {"message": message}}).to_string();
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        Json(error_envelope(status, provider, model, endpoint, &cause)),
    )
        .into_response()
}

async fn compact<S: ProxyRuntimeState>(State(state): State<Arc<S>>, request: Request) -> Response {
    let resources = state.proxy_runtime().resource_policy();
    let (headers, body) = match read_json_body(request, "/responses/compact", &resources).await {
        Ok(parsed) => parsed,
        Err(response) => return *response,
    };
    let incoming_auth = IncomingAuthContext {
        authorization: headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
        openai_account: headers
            .get("openai-account")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    };
    let metadata = build_request_metadata(&headers, &body, RuntimeEndpoint::Compact, None);
    let runtime_request = RuntimeRequest {
        body,
        endpoint: RuntimeEndpoint::Compact,
        incoming_auth,
        execution_environment: state.execution_environment(),
        metadata,
    };
    match state.proxy_runtime().execute_compact(runtime_request).await {
        Ok(RuntimeResponse::Json(value)) => (StatusCode::OK, Json(value)).into_response(),
        Ok(RuntimeResponse::Sse(_)) => unreachable!("compact never returns SSE"),
        Ok(RuntimeResponse::Raw {
            status,
            content_type,
            body,
        }) => raw_response(status, content_type, body),
        Err(error) => error.into_error_response(),
    }
}

async fn search<S: ProxyRuntimeState>(State(state): State<Arc<S>>, request: Request) -> Response {
    let resources = state.proxy_runtime().resource_policy();
    let (headers, body) = match read_json_body(request, "/v1/alpha/search", &resources).await {
        Ok(parsed) => parsed,
        Err(response) => return *response,
    };
    let incoming_auth = IncomingAuthContext {
        authorization: headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
        openai_account: headers
            .get("openai-account")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
    };
    let metadata = build_request_metadata(&headers, &body, RuntimeEndpoint::Search, None);
    let runtime_request = RuntimeRequest {
        body,
        endpoint: RuntimeEndpoint::Search,
        incoming_auth,
        execution_environment: state.execution_environment(),
        metadata,
    };
    match state.proxy_runtime().execute_search(runtime_request).await {
        Ok(RuntimeResponse::Json(value)) => (StatusCode::OK, Json(value)).into_response(),
        Ok(RuntimeResponse::Sse(_)) => unreachable!("search never returns SSE"),
        Ok(RuntimeResponse::Raw {
            status,
            content_type,
            body,
        }) => raw_response(status, content_type, body),
        // Desktop parity: a disabled search engine answers 503 with the
        // "web-search"/"alpha" envelope; other engine failures answer 502.
        Err(crate::error::RuntimeError::Search(message)) => {
            let status = if message.to_ascii_lowercase().contains("disabled") {
                503
            } else {
                502
            };
            (
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                Json(error_envelope(
                    status,
                    "web-search",
                    "alpha",
                    "/v1/alpha/search",
                    &message,
                )),
            )
                .into_response()
        }
        Err(error) => error.into_error_response(),
    }
}

fn raw_response(status: u16, content_type: Option<String>, body: Vec<u8>) -> Response {
    let mut response =
        Response::builder().status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY));
    if let Some(content_type) = content_type {
        if let Ok(value) = HeaderValue::from_str(&content_type) {
            response = response.header(axum::http::header::CONTENT_TYPE, value);
        }
    }
    response
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProxyRuntimeConfig, ProxyRuntimeIdentity, RuntimeRouteConfig};
    use crate::environment::{
        ExecutionEnvironment, RuntimeAmpersandSemantics, RuntimePathStyle, RuntimePlatform,
        RuntimeShellKind,
    };
    use crate::route::{RuntimeAuthKind, RuntimeProviderKind, RuntimeWireFormat};
    use crate::state::StaticProxyState;
    use axum::body::{Body, Bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::{json, Value};
    use std::sync::Mutex;
    use tower::ServiceExt;

    /// Existing behavior tests drive the runtime, not the boundary. The guard
    /// has its own coverage below; using the test-only policy here keeps those
    /// tests about the thing they assert.
    fn unguarded() -> InboundAccessPolicy {
        InboundAccessPolicy::test_only_disabled()
    }

    fn runtime_route(base_url: String) -> RuntimeRouteConfig {
        RuntimeRouteConfig {
            route_id: "mock".into(),
            catalog_id: "vellum-mock".into(),
            name: "Vellum Mock".into(),
            base_url,
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            auth_kind: RuntimeAuthKind::None,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: false,
            streaming: true,
            reasoning: true,
            vision: false,
            upstream_model: "mock-1".into(),
            context_window: Some(128_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            // Existing HTTP behavior tests assert the V1 checkpoint wire
            // shape. Keep that baseline explicit now that production defaults
            // to V2; dedicated V2 tests exercise the promoted engine.
            compaction_policy: crate::config::RuntimeCompactionPolicy {
                ..Default::default()
            },
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: Some(json!({
                "slug": "stale-catalog-slug",
                "display_name": "Official Catalog Name",
                "base_instructions": "official catalog instructions",
                "model_messages": {"instructions_template": "official template"},
                "prefer_websockets": true
            })),
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        }
    }

    fn sample_state_with_base(base_url: String) -> Arc<StaticProxyState> {
        let config = ProxyRuntimeConfig {
            schema_version: 2,
            identity: ProxyRuntimeIdentity {
                install_id: "install-1".into(),
                host_id: "host-1".into(),
                image_version: "0.1.0".into(),
                config_hash: "abc".into(),
                ..Default::default()
            },
            models: vec![runtime_route(base_url)],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        Arc::new(state)
    }

    fn sample_state() -> Arc<StaticProxyState> {
        // A base URL nothing listens on; only the /v1/responses tests need a
        // live upstream, and they build their own.
        sample_state_with_base("http://127.0.0.1:1/v1".into())
    }

    #[test]
    fn version_and_readyz_expose_build_identity() {
        let state = sample_state();
        let started = std::time::Instant::now();
        let (status, body) = readyz_payload(state.as_ref());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(50),
            "/readyz must not hash the process image ({:?})",
            started.elapsed()
        );
        assert_eq!(status, StatusCode::OK);
        let identity = state.identity();
        assert_eq!(body["identity"]["commit"], identity.commit);
        assert!(
            body["identity"]["binary_hash"]
                .as_str()
                .unwrap_or("")
                .is_empty(),
            "/readyz must not wait on executable hashing"
        );
        let version = version_payload(state.as_ref());
        assert!(!version["version"].as_str().unwrap_or("").is_empty());
        assert!(!version["commit"].as_str().unwrap_or("").is_empty());
        assert!(!version["binary_hash"].as_str().unwrap_or("").is_empty());
        assert_eq!(version["config_hash"], "abc");
        assert_eq!(version["commit"], identity.commit);
    }

    /// A live fake upstream serving scripted turns in call order over real
    /// HTTP. Returns the address and a shared call counter.
    async fn start_upstream(
        script: Vec<(u16, Value)>,
    ) -> (std::net::SocketAddr, Arc<Mutex<usize>>) {
        #[derive(Clone)]
        struct Stub {
            turns: Arc<Vec<(u16, Value)>>,
            calls: Arc<Mutex<usize>>,
        }
        let calls = Arc::new(Mutex::new(0usize));
        let stub = Stub {
            turns: Arc::new(script),
            calls: Arc::clone(&calls),
        };
        let app = Router::new()
            .route(
                "/v1/responses",
                post(move |State(stub): State<Stub>| async move {
                    let mut calls = stub.calls.lock().unwrap();
                    let index = *calls;
                    *calls += 1;
                    let (status, body) = stub.turns[index.min(stub.turns.len() - 1)].clone();
                    (
                        StatusCode::from_u16(status).expect("valid status"),
                        Json(body),
                    )
                        .into_response()
                }),
            )
            .with_state(stub);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (address, calls)
    }

    /// A live fake upstream that records every prepared request body, so a
    /// test can assert on exactly what the runtime sent (not just that it
    /// called upstream).
    async fn start_capturing_upstream() -> (std::net::SocketAddr, Arc<Mutex<Vec<Value>>>) {
        #[derive(Clone)]
        struct Stub {
            bodies: Arc<Mutex<Vec<Value>>>,
        }
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let stub = Stub {
            bodies: Arc::clone(&bodies),
        };
        let app = Router::new()
            .route(
                "/v1/responses",
                post(
                    move |State(stub): State<Stub>, body: axum::body::Bytes| async move {
                        let parsed = serde_json::from_slice(&body).unwrap_or(Value::Null);
                        stub.bodies.lock().unwrap().push(parsed);
                        (
                            StatusCode::OK,
                            Json(json!({
                                "id": "resp_captured",
                                "object": "response",
                                "status": "completed",
                                "output": [],
                                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
                            })),
                        )
                            .into_response()
                    },
                ),
            )
            .with_state(stub);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (address, bodies)
    }

    /// Real HTTP Chat-Completions summarizer used by the long-context tests.
    /// It rejects a Responses-shaped body, so the test cannot pass through a
    /// permissive mock when the selected route is Chat.
    async fn start_chat_compaction_upstream() -> (std::net::SocketAddr, Arc<Mutex<Vec<usize>>>) {
        #[derive(Clone)]
        struct Stub {
            prompt_sizes: Arc<Mutex<Vec<usize>>>,
        }
        let prompt_sizes = Arc::new(Mutex::new(Vec::new()));
        let stub = Stub {
            prompt_sizes: Arc::clone(&prompt_sizes),
        };
        let app = Router::new()
            .route(
                "/v1/chat/completions",
                post(move |State(stub): State<Stub>, Json(body): Json<Value>| async move {
                    if body.get("input").is_some() || body.get("messages").is_none() {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error": {"message": "expected Chat Completions wire"}})),
                        )
                            .into_response();
                    }
                    let prompt = body
                        .pointer("/messages/0/content")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    stub.prompt_sizes.lock().unwrap().push(prompt.len());
                    let summary = json!({
                        "goal": "Preserve the long-running task",
                        "acceptanceCriteria": ["Resume without re-reading the codebase"],
                        "constraints": ["Keep exact decisions"],
                        "userPreferences": [],
                        "done": ["Investigated the repository"],
                        "inProgress": ["Implement the requested change"],
                        "blocked": [],
                        "decisions": ["Use the existing adapter boundary"],
                        "changedFiles": [],
                        "relevantFiles": ["src/main.rs"],
                        "commands": ["cargo test"],
                        "tests": ["baseline passed"],
                        "unresolved": [],
                        "errors": [],
                        "criticalContext": ["Do not discard prior tool results"],
                        "references": [],
                        "nextSteps": ["Continue implementation"]
                    })
                    .to_string();
                    (
                        StatusCode::OK,
                        Json(json!({
                            "id": "chatcmpl_compact",
                            "object": "chat.completion",
                            "choices": [{
                                "index": 0,
                                "message": {"role": "assistant", "content": summary},
                                "finish_reason": "stop"
                            }],
                            "usage": {"prompt_tokens": 1000, "completion_tokens": 200, "total_tokens": 1200}
                        })),
                    )
                        .into_response()
                }),
            )
            // Codex 0.150 sends the entire history in one summarizer call rather
            // than in bounded chunks, so this stub must accept a body far larger
            // than axum's 2MB default.
            .layer(axum::extract::DefaultBodyLimit::disable())
            .with_state(stub);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (address, prompt_sizes)
    }

    async fn start_chat_compaction_and_resume_upstream(
    ) -> (std::net::SocketAddr, Arc<Mutex<Vec<Value>>>) {
        #[derive(Clone)]
        struct Stub {
            bodies: Arc<Mutex<Vec<Value>>>,
        }
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let stub = Stub {
            bodies: Arc::clone(&bodies),
        };
        let app = Router::new()
            .route(
                "/v1/chat/completions",
                post(move |State(stub): State<Stub>, Json(body): Json<Value>| async move {
                    // Codex 0.150 sends the conversation first and appends the
                    // compaction prompt as the LAST message, so the summarizer
                    // cannot be recognised by looking at `messages[0]`.
                    let is_summary = body
                        .get("messages")
                        .and_then(Value::as_array)
                        .and_then(|messages| messages.last())
                        .and_then(|message| message.get("content"))
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.contains("CONTEXT CHECKPOINT COMPACTION"));
                    stub.bodies.lock().unwrap().push(body);
                    let content = if is_summary {
                        json!({
                            "goal": "Resume the large task",
                            "done": ["Repository investigation completed"],
                            "decisions": ["Preserve the adapter boundary"],
                            "nextSteps": ["Finish the implementation"]
                        })
                        .to_string()
                    } else {
                        "continued successfully".into()
                    };
                    Json(json!({
                        "id": if is_summary { "chatcmpl_summary" } else { "chatcmpl_resume" },
                        "object": "chat.completion",
                        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120}
                    }))
                }),
            )
            .with_state(stub);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (address, bodies)
    }

    #[tokio::test]
    async fn health_and_readyz_and_models() {
        let app = build_headless_router(sample_state(), unguarded());
        for path in [
            "/health",
            "/readyz",
            "/version",
            "/diagnostics/usage",
            "/v1/models",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "path={path}");
        }
    }

    #[tokio::test]
    async fn responses_dispatches_to_a_real_upstream_and_normalizes() {
        // P0-1: /v1/responses must call upstream for real. The mock response
        // path is gone; this proves the whole headless chain (route → snapshot
        // → adapter → HTTP transport → normalize) executes end to end.
        let (address, calls) = start_upstream(vec![(
            200,
            json!({
                "id": "resp_1",
                "object": "response",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "hi back", "annotations": []}]
                }],
                "usage": {"input_tokens": 2, "output_tokens": 1, "total_tokens": 3}
            }),
        )])
        .await;
        let app = build_headless_router(
            sample_state_with_base(format!("http://{address}/v1")),
            unguarded(),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model":"vellum-mock","input":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["object"], "response");
        assert_eq!(body["output"][0]["phase"], "final_answer");
        assert_eq!(body["output"][0]["content"][0]["text"], "hi back");
        assert_eq!(
            *calls.lock().unwrap(),
            1,
            "the headless responses handler must reach the upstream exactly once"
        );
    }

    #[tokio::test]
    async fn chat_compaction_trigger_summarizes_400k_700k_and_900k_over_real_http() {
        // This reproduces the actual Codex Desktop flow: a streaming
        // `/v1/responses` request contains `compaction_trigger`, while the
        // selected OpenCode-compatible route speaks Chat Completions.  The
        // payloads are sized using the runtime's own estimator and cross the
        // 400k/700k/900k targets without bypassing the HTTP parser/router.
        for target_tokens in [400_000_u64, 700_000, 900_000] {
            let (address, prompt_sizes) = start_chat_compaction_upstream().await;
            let mut route = runtime_route(format!("http://{address}/v1"));
            route.wire = RuntimeWireFormat::Chat;
            route.context_window = Some(1_000_000);
            let config = ProxyRuntimeConfig {
                schema_version: 2,
                identity: ProxyRuntimeIdentity {
                    install_id: format!("long-{target_tokens}"),
                    host_id: "host-1".into(),
                    image_version: "0.1.0".into(),
                    config_hash: "long-context-e2e".into(),
                    ..Default::default()
                },
                models: vec![route],
                ..ProxyRuntimeConfig::default()
            };
            let mut state = StaticProxyState::from_config(config).unwrap();
            state.mark_listener_ready();
            let app = build_headless_router(Arc::new(state), unguarded());

            // ASCII text gives a deterministic ~4 bytes/token estimate. Keep
            // a recent tail turn so canonical compaction has both early
            // history and retained continuation state.
            let per_turn = ((target_tokens as usize) * 4 / 80).max(1);
            let mut input = Vec::with_capacity(83);
            for turn in 0..80 {
                input.push(json!({
                    "type": "message",
                    "role": if turn % 2 == 0 { "user" } else { "assistant" },
                    "content": format!("turn-{turn}:{}", "x".repeat(per_turn))
                }));
            }
            input.push(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "repository investigation complete", "annotations": []}]}));
            input.push(json!({"type": "message", "role": "user", "content": "continue from the established decisions"}));
            input.push(json!({"type": "compaction_trigger"}));
            let body = json!({
                "model": "vellum-mock",
                "stream": true,
                "input": input
            });
            let estimated = crate::compaction::estimate_tokens(body["input"].as_array().unwrap());
            assert!(
                estimated >= target_tokens,
                "fixture must reach {target_tokens} tokens, got {estimated}"
            );
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/responses")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "target={target_tokens}");
            assert_eq!(
                response
                    .headers()
                    .get("content-type")
                    .and_then(|value| value.to_str().ok()),
                Some("text/event-stream")
            );
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let transcript = String::from_utf8(bytes.to_vec()).unwrap();
            assert_eq!(
                transcript
                    .matches("event: response.output_item.done")
                    .count(),
                1
            );
            assert_eq!(transcript.matches("\"type\":\"compaction\"").count(), 3);
            assert!(transcript.contains("event: response.completed"));
            // Codex 0.150 does not chunk. The whole history goes to the
            // summarizer in a single call; it shrinks only if the provider
            // rejects it for context, in which case the oldest item is dropped
            // and the call retried. This upstream accepts the request, so
            // exactly one call is expected regardless of history size.
            let sizes = prompt_sizes.lock().unwrap();
            assert_eq!(
                sizes.len(),
                1,
                "{target_tokens} tokens must be summarized in one unchunked call: {sizes:?}"
            );
        }
    }

    /// Sleeps `delay` before writing any upstream response. Used to prove
    /// the inbound HTTP status is committed before the upstream handshake.
    async fn start_header_delayed_sse_upstream(delay: std::time::Duration) -> std::net::SocketAddr {
        let app = Router::new().route(
            "/v1/responses",
            post(move || async move {
                tokio::time::sleep(delay).await;
                (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    concat!(
                        "event: response.created\n",
                        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"status\":\"in_progress\"}}\n\n",
                        "event: response.completed\n",
                        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
                    ),
                )
                    .into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        address
    }

    #[tokio::test]
    async fn inbound_http_status_does_not_wait_for_upstream_headers() {
        let delay = std::time::Duration::from_secs(3);
        let address = start_header_delayed_sse_upstream(delay).await;
        let app = build_headless_router(
            sample_state_with_base(format!("http://{address}/v1")),
            unguarded(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let started = std::time::Instant::now();
        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/v1/responses"))
            .header("content-type", "application/json")
            .json(&json!({
                "model": "vellum-mock",
                "stream": true,
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hello"}]
                }]
            }))
            .send()
            .await
            .unwrap();
        let header_ms = started.elapsed().as_millis();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            header_ms < 1000,
            "inbound HTTP 200 must flush before the delayed upstream handshake, took {header_ms}ms"
        );
    }

    /// Headers immediately, first SSE byte after `delay`. Used to prove the
    /// inbound HTTP status is not held until the provider's first event.
    async fn start_delayed_sse_upstream(delay: std::time::Duration) -> std::net::SocketAddr {
        let app = Router::new().route(
            "/v1/responses",
            post(move || async move {
                let stream = async_stream::stream! {
                    tokio::time::sleep(delay).await;
                    yield Ok::<_, std::convert::Infallible>(Bytes::from(concat!(
                        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_stream_1\",\"object\":\"response\",\"status\":\"in_progress\"}}\n\n",
                        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\",\"output_index\":0,\"content_index\":0}\n\n",
                        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_stream_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_stream_1\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hi\",\"annotations\":[]}]}],\"usage\":{\"input_tokens\":3,\"output_tokens\":1,\"total_tokens\":4}}}\n\n",
                    )));
                };
                let mut response = Response::new(Body::from_stream(stream));
                response.headers_mut().insert(
                    axum::http::header::CONTENT_TYPE,
                    HeaderValue::from_static("text/event-stream"),
                );
                response
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        address
    }

    #[tokio::test]
    async fn inbound_http_status_does_not_wait_for_the_first_upstream_sse_byte() {
        let delay = std::time::Duration::from_secs(3);
        let address = start_delayed_sse_upstream(delay).await;
        let app = build_headless_router(
            sample_state_with_base(format!("http://{address}/v1")),
            unguarded(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let started = std::time::Instant::now();
        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/v1/responses"))
            .header("content-type", "application/json")
            .json(&json!({
                "model": "vellum-mock",
                "stream": true,
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hello"}]
                }]
            }))
            .send()
            .await
            .unwrap();
        let header_ms = started.elapsed().as_millis();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            header_ms < 1000,
            "inbound HTTP 200 must flush before the delayed upstream body, took {header_ms}ms"
        );
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.contains("event-stream"),
            "expected text/event-stream, got {content_type}"
        );
        let header_at = std::time::Instant::now();
        let body = response.text().await.unwrap();
        let body_ms = header_at.elapsed().as_millis();
        assert!(
            body.contains("response.created") && body.contains("response.completed"),
            "application events must still arrive after the inbound flush comment: {body}"
        );
        assert!(
            body_ms >= 2000,
            "delayed upstream body should not be forwarded instantly, took {body_ms}ms"
        );
        assert!(
            body_ms < 8000,
            "application events must not be dropped after inbound flush, took {body_ms}ms"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawned_execute_forwards_application_events_on_a_multithreaded_runtime() {
        let delay = std::time::Duration::from_millis(1500);
        let address = start_delayed_sse_upstream(delay).await;
        let app = build_headless_router(
            sample_state_with_base(format!("http://{address}/v1")),
            unguarded(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/v1/responses"))
            .header("content-type", "application/json")
            .json(&json!({
                "model": "vellum-mock",
                "stream": true,
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hello"}]
                }]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let started = std::time::Instant::now();
        let body = response.text().await.unwrap();
        assert!(
            body.contains("response.completed"),
            "multithreaded runtime must forward application events: {body}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(8),
            "application events stalled after inbound flush"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_sse_usage_records_both_upstream_and_downstream_first_frame() {
        // A delayed upstream makes upstream (first parsed byte) and
        // downstream (first frame forwarded to the client) genuinely
        // distinct pipeline stages, so a collapse would be visible.
        let delay = std::time::Duration::from_millis(600);
        let address = start_delayed_sse_upstream(delay).await;
        let state = sample_state_with_base(format!("http://{address}/v1"));
        let app = build_headless_router(state.clone(), unguarded());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/v1/responses"))
            .header("content-type", "application/json")
            .header("x-request-id", "req_http_sse_ledger")
            .json(&json!({
                "model": "vellum-mock",
                "stream": true,
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hello"}]
                }]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        assert!(
            body.contains("response.completed"),
            "application events must arrive: {body}"
        );

        // The spawned forwarder marks the downstream first frame after the
        // stream ends (the usage row is written inside the stream); poll a
        // moment for the mark to land.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let frame = loop {
            let records = state.proxy_runtime().usage_records().unwrap();
            let frame =
                crate::live_attribution::ledger_frame_for_request(&records, "req_http_sse_ledger");
            if frame.upstream_first_frame_ms.is_some() && frame.downstream_first_frame_ms.is_some()
            {
                break frame;
            }
            if std::time::Instant::now() >= deadline {
                break frame;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        let upstream = frame.upstream_first_frame_ms.expect("upstream first frame");
        let downstream = frame
            .downstream_first_frame_ms
            .expect("downstream first frame");
        // The upstream first-frame must include the delayed provider's wait:
        // a collapse would report ~1ms here (stream-construction baseline)
        // instead of the ~600ms the provider actually took.
        assert!(
            upstream >= 500,
            "upstream first frame ({upstream}ms) must include the provider delay"
        );
        assert!(
            downstream >= upstream,
            "downstream first frame ({downstream}ms) must not precede the upstream first frame ({upstream}ms)"
        );
    }

    /// A live fake upstream that streams a fixed SSE transcript verbatim, so a
    /// test can put an exact hostile byte sequence on the wire.
    async fn start_sse_upstream(transcript: &'static str) -> std::net::SocketAddr {
        let app = Router::new().route(
            "/v1/responses",
            post(move || async move {
                (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    transcript,
                )
                    .into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        address
    }

    async fn stream_hostile_transcript(transcript: &'static str) -> String {
        let address = start_sse_upstream(transcript).await;
        let app = build_headless_router(
            sample_state_with_base(format!("http://{address}/v1")),
            unguarded(),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "model": "vellum-mock",
                            "stream": true,
                            "input": [{
                                "type": "message",
                                "role": "user",
                                "content": [{"type": "input_text", "text": "hello"}]
                            }]
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// V-11: a two-line hostile frame from a third-party provider must not kill
    /// the streaming task. The stream ends with one bounded terminal event
    /// carrying the canonical `provider_protocol` category, and the malformed
    /// frame is never forwarded as an opaque passthrough event.
    #[tokio::test]
    async fn a_malformed_known_event_ends_the_stream_with_a_canonical_terminal_event() {
        let transcript = stream_hostile_transcript(concat!(
            "event: response.reasoning_summary_text.done\n",
            "data: \"not-an-object\"\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n",
        ))
        .await;

        assert!(
            transcript.contains("event: response.failed"),
            "stream must end with a terminal failure event: {transcript}"
        );
        assert!(
            transcript.contains("\"category\":\"provider_protocol\""),
            "terminal event must carry the canonical category: {transcript}"
        );
        assert!(
            !transcript.contains("not-an-object"),
            "the malformed frame must never reach the client: {transcript}"
        );
        assert!(
            !transcript.contains("event: response.completed"),
            "nothing after the protocol failure may be forwarded: {transcript}"
        );
        assert_eq!(
            transcript.matches("event: response.failed").count(),
            1,
            "exactly one terminal event: {transcript}"
        );
    }

    /// The same failure after the stream is already underway: frames that
    /// arrived before the malformed one are kept, and the stream still closes
    /// with exactly one terminal event rather than being truncated.
    #[tokio::test]
    async fn frames_before_a_malformed_event_are_kept_and_the_stream_closes_once() {
        let transcript = stream_hostile_transcript(concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial answer\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":\"not-an-object\"}\n\n",
        ))
        .await;

        assert!(
            transcript.contains("partial answer"),
            "already-delivered content must survive: {transcript}"
        );
        assert!(
            transcript.contains("\"category\":\"provider_protocol\""),
            "terminal event must carry the canonical category: {transcript}"
        );
        assert_eq!(
            transcript.matches("event: response.failed").count(),
            1,
            "exactly one terminal event: {transcript}"
        );
    }

    /// The passthrough contract still holds: an event outside the protocol's
    /// reserved namespace reaches the client untouched whatever its shape.
    #[tokio::test]
    async fn unknown_events_still_pass_through_a_real_stream() {
        let transcript = stream_hostile_transcript(concat!(
            "event: provider.telemetry\n",
            "data: \"provider-specific-scalar\"\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"visible\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n",
        ))
        .await;

        assert!(
            transcript.contains("provider-specific-scalar"),
            "unknown events keep passthrough: {transcript}"
        );
        assert!(transcript.contains("event: response.completed"));
        assert!(
            !transcript.contains("\"category\":\"provider_protocol\""),
            "a well-formed stream must not be failed: {transcript}"
        );
    }

    #[tokio::test]
    async fn local_compaction_reference_materializes_on_the_next_real_http_turn() {
        let (address, bodies) = start_chat_compaction_and_resume_upstream().await;
        let mut route = runtime_route(format!("http://{address}/v1"));
        route.wire = RuntimeWireFormat::Chat;
        route.context_window = Some(1_000_000);
        let config = ProxyRuntimeConfig {
            schema_version: 2,
            identity: ProxyRuntimeIdentity {
                install_id: "resume-e2e".into(),
                host_id: "host-1".into(),
                image_version: "0.1.0".into(),
                config_hash: "resume-e2e".into(),
                ..Default::default()
            },
            models: vec![route],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let app = build_headless_router(Arc::new(state), unguarded());
        let compact = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "model": "vellum-mock",
                            "stream": false,
                            "input": [
                                {"type": "message", "role": "user", "content": "old ".repeat(100_000)},
                                {"type": "message", "role": "assistant", "content": "investigated"},
                                {"type": "message", "role": "user", "content": "continue"},
                                {"type": "compaction_trigger"}
                            ]
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(compact.status(), StatusCode::OK);
        let compact_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(compact.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let compact_item = compact_body["output"][0].clone();
        assert_eq!(compact_item["type"], "compaction");

        let resumed = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "model": "vellum-mock",
                            "stream": false,
                            "input": [compact_item, {"type": "message", "role": "user", "content": "finish now"}]
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resumed.status(), StatusCode::OK);
        let captured = bodies.lock().unwrap();
        let final_turn = captured.last().unwrap();
        let serialized = final_turn.to_string();
        assert!(
            serialized.contains("Resume the large task"),
            "canonical checkpoint must reach the resumed provider turn: {serialized}"
        );
        assert!(serialized.contains("Preserve the adapter boundary"));
        assert!(serialized.contains("finish now"));
        assert!(!serialized.contains("vcomp1."));
        assert!(!serialized.contains("\"type\":\"compaction\""));
    }

    /// Reviewer P0 (M3C): the headless production path must take the executor
    /// contract from explicit config/state, never fabricate the POSIX
    /// reference. A config that declares a Windows/PowerShell executor must
    /// make the prepared upstream prompt describe that shell — the reference
    /// contract's text must be absent.
    #[tokio::test]
    async fn responses_honors_the_states_executor_environment() {
        let (address, bodies) = start_capturing_upstream().await;
        let config = ProxyRuntimeConfig {
            schema_version: 2,
            identity: ProxyRuntimeIdentity {
                install_id: "install-1".into(),
                host_id: "host-1".into(),
                image_version: "0.1.0".into(),
                config_hash: "abc".into(),
                ..Default::default()
            },
            execution_environment: ExecutionEnvironment {
                platform: RuntimePlatform::Windows,
                shell: RuntimeShellKind::Pwsh,
                shell_version: Some("7.5.0".into()),
                supports_and_and: false,
                has_unix_utilities: false,
                path_style: RuntimePathStyle::Windows,
                ampersand_semantics: RuntimeAmpersandSemantics::PowershellCore,
                verified_capabilities: Vec::new(),
            },
            models: vec![runtime_route(format!("http://{address}/v1"))],
            ..ProxyRuntimeConfig::default()
        };
        let mut state = StaticProxyState::from_config(config).unwrap();
        state.mark_listener_ready();
        let app = build_headless_router(Arc::new(state), unguarded());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model":"vellum-mock","input":"hi","tools":[{"type":"custom","name":"apply_patch"},{"type":"shell"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1, "the upstream must be called exactly once");
        let instructions = bodies[0]["instructions"]
            .as_str()
            .expect("a tool-aware prompt must be written when tools are present");
        // The Windows/PowerShell contract from config/state:
        assert!(instructions.contains("no `&&`"), "{instructions}");
        assert!(instructions.contains("Select-String"), "{instructions}");
        assert!(
            instructions.contains("`&` is the call operator"),
            "{instructions}"
        );
        assert!(instructions.contains("here-strings"), "{instructions}");
        // The POSIX reference contract must not leak in from anywhere:
        assert!(
            !instructions.contains("`&` backgrounds a job"),
            "{instructions}"
        );
        assert!(
            !instructions.contains("use `rg` rather than `grep`"),
            "{instructions}"
        );
    }

    #[tokio::test]
    async fn upstream_401_maps_to_the_centralized_error_envelope() {
        let (address, calls) = start_upstream(vec![(
            401,
            json!({"error": {"message": "synthetic upstream 401"}}),
        )])
        .await;
        let app = build_headless_router(
            sample_state_with_base(format!("http://{address}/v1")),
            unguarded(),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model":"vellum-mock","input":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(payload["error"]["type"], "vellum_proxy_error");
        assert_eq!(payload["error"]["code"], 401);
        assert!(payload["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("upstream_status: HTTP 401")));
        assert_eq!(
            *calls.lock().unwrap(),
            1,
            "a non-managed 401 must dispatch exactly once"
        );
    }

    #[tokio::test]
    async fn a_missing_model_never_reaches_the_upstream() {
        let (address, calls) = start_upstream(vec![(200, json!({"unused": true}))]).await;
        let app = build_headless_router(
            sample_state_with_base(format!("http://{address}/v1")),
            unguarded(),
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"input":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            *calls.lock().unwrap(),
            0,
            "a malformed request must never reach the upstream"
        );
    }

    #[tokio::test]
    async fn models_payload_matches_desktops_shape_and_ownership_authority() {
        let app = build_headless_router(sample_state(), unguarded());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: Value = serde_json::from_slice(&bytes).unwrap();
        let entry = &payload["data"][0];
        assert_eq!(entry["id"], "vellum-mock");
        // Desktop's wire owner is always route_id; the daemon config no longer
        // carries its own `owned_by`, so the route_id is authoritative.
        assert_eq!(entry["owned_by"], "mock");
        assert_eq!(entry["context_window"], 128_000);
        assert!(
            entry.get("upstream_model").is_none(),
            "the wire shape must match Desktop's exactly: {entry}"
        );
        let native = &payload["models"][0];
        assert_eq!(native["slug"], "vellum-mock");
        assert_eq!(native["context_window"], 128_000);
        assert_eq!(native["shell_type"], "shell_command");
        assert_eq!(native["display_name"], "Official Catalog Name");
        assert_eq!(native["base_instructions"], "official catalog instructions");
        assert_eq!(
            native["model_messages"]["instructions_template"],
            "official template"
        );
        assert_eq!(native["prefer_websockets"], true);
        assert_eq!(
            native["slug"], "vellum-mock",
            "the public route id remains authoritative over a stale catalog slug"
        );
        assert!(native.get("id").is_none());
    }

    #[tokio::test]
    async fn invalid_json_gets_vellums_error_envelope_not_axums_default_rejection() {
        let app = build_headless_router(sample_state(), unguarded());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(payload["error"]["type"], "vellum_proxy_error");
    }

    #[tokio::test]
    async fn responses_rejects_a_zstd_encoded_body_with_415() {
        let (address, calls) = start_upstream(vec![(
            200,
            json!({
                "id": "resp_zstd",
                "object": "response",
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }),
        )])
        .await;
        let app = build_headless_router(
            sample_state_with_base(format!("http://{address}/v1")),
            unguarded(),
        );
        let compressed =
            zstd::stream::encode_all(br#"{"model":"vellum-mock","input":"hi"}"#.as_slice(), 3)
                .unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .header("content-encoding", "zstd")
                    .body(Body::from(compressed))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(payload["error"]["category"], "unsupported_media");
        assert_eq!(payload["error"]["code"], 415);
        assert_eq!(
            *calls.lock().unwrap(),
            0,
            "a compressed body must never be decompressed or forwarded"
        );
    }

    #[tokio::test]
    async fn a_full_http_semaphore_answers_resource_exhausted_and_leaves_health_open() {
        let state = sample_state();
        let policy = state.proxy_runtime().resource_policy();
        let _held: Vec<_> = (0..policy.active_http_requests())
            .map(|_| policy.try_acquire_http().expect("slot available"))
            .collect();
        let app = build_headless_router(state, unguarded());

        let health = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);

        let readyz = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/readyz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(readyz.status(), StatusCode::OK);

        let blocked = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model":"vellum-mock","input":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);
        let bytes = axum::body::to_bytes(blocked.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(payload["error"]["type"], "vellum_proxy_error");
        assert_eq!(payload["error"]["category"], "resource_exhausted");
        assert_eq!(payload["error"]["code"], 429);
    }

    /// A live fake upstream that streams a scripted SSE transcript per call.
    /// A request whose body contains `bounded_on` gets a *bounded* response
    /// that closes right after the blocks (a turn that must reach its terminal
    /// event); every other request holds the connection open so a test can
    /// abort or cancel a turn mid-flight. Each block is delayed so the bridge
    /// is still actively forwarding when a client drops its body or the parent
    /// cancel fans out (the only cancel signals HTTP SSE carries).
    async fn start_scripted_sse_upstream(
        variants: Vec<Vec<String>>,
        bounded_on: Option<&str>,
    ) -> (std::net::SocketAddr, Arc<Mutex<usize>>) {
        let calls = Arc::new(Mutex::new(0usize));
        let shared = Arc::new(Mutex::new(variants));
        let bounded_on = bounded_on.map(str::to_owned);
        let calls_for_return = Arc::clone(&calls);
        let app = Router::new().route(
            "/v1/responses",
            post(move |request: Request<Body>| {
                let shared = shared.clone();
                let calls = calls.clone();
                let bounded_on = bounded_on.clone();
                async move {
                    let consumed = axum::body::to_bytes(request.into_body(), 1024 * 1024)
                        .await
                        .unwrap();
                    let bounded = bounded_on
                        .as_ref()
                        .map(|marker| {
                            std::str::from_utf8(&consumed)
                                .map(|text| text.contains(marker))
                                .unwrap_or(false)
                        })
                        .unwrap_or(false);
                    let index = {
                        let mut calls = calls.lock().unwrap();
                        let index = *calls;
                        *calls += 1;
                        index
                    };
                    let blocks = {
                        let variants = shared.lock().unwrap();
                        if variants.is_empty() {
                            Vec::new()
                        } else {
                            variants[index.min(variants.len() - 1)].clone()
                        }
                    };
                    let stream = async_stream::stream! {
                        for block in blocks {
                            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                            yield Ok::<_, std::convert::Infallible>(Bytes::from(block));
                        }
                        if bounded {
                            return;
                        }
                        // Hold the turn open: the runtime keeps reading (and a
                        // test can still cancel) while the provider is "still
                        // generating".
                        futures_util::future::pending::<()>().await;
                        unreachable!();
                    };
                    let mut response = Response::new(Body::from_stream(stream));
                    response.headers_mut().insert(
                        axum::http::header::CONTENT_TYPE,
                        HeaderValue::from_static("text/event-stream"),
                    );
                    response
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (address, calls_for_return)
    }

    fn streaming_body(text: &str) -> Value {
        json!({
            "model": "vellum-mock",
            "stream": true,
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": text}]
            }]
        })
    }

    /// An HTTP SSE client aborting mid-turn is the bridge's only cancel signal
    /// (there is no `response.cancel` backchannel). It must fan the cancel out
    /// to bound children, release the registry slot, and give the parent its
    /// own bounded 499 row rather than silently leaving an open request.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_stream_client_disconnect_records_a_bounded_499_and_registers_the_cancel() {
        let (address, _calls) = start_scripted_sse_upstream(vec![vec![
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"d1\"}}\n\n".into(),
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n".into(),
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"y\"}\n\n".into(),
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"z\"}\n\n".into(),
        ]], None)
        .await;
        let state = sample_state_with_base(format!("http://{address}/v1"));
        let app = build_headless_router(state.clone(), unguarded());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let response = reqwest::Client::new()
            .post(format!("http://{proxy}/v1/responses"))
            .header("content-type", "application/json")
            .header("x-request-id", "req_disconnect")
            .json(&streaming_body("hello"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Read one frame so the bridge is actively forwarding, then abort.
        let mut stream = response.bytes_stream();
        let first_frame = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("a first downstream frame must arrive before the abort")
            .expect("the stream must not end before the first frame");
        assert!(
            first_frame.is_ok(),
            "the first frame must not error: {first_frame:?}"
        );
        drop(stream);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let row = loop {
            let records = state.proxy_runtime().usage_records().unwrap();
            if let Some(row) = records
                .iter()
                .find(|record| record.request_id.as_deref() == Some("req_disconnect"))
            {
                break row.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the disconnected parent never received its bounded 499 row"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert_eq!(row.status, 499);
        assert_eq!(row.outcome.as_deref(), Some("client_disconnect"));
        assert_eq!(row.error.as_deref(), Some("client closed"));
        // `is_request_cancelled` is deliberately no longer checkable from
        // outside by the external `x-request-id`: the cancel registry is
        // keyed by an internally minted execution id (never a client-visible
        // id a WebSocket client could otherwise repeat across turns). The
        // bounded 499 row above is the black-box proof that the disconnect
        // path ran `cancel_request_tree`/registered the cancellation.
    }

    /// A child whose upstream is dropped by parent-cancel fan-out gets its own
    /// bounded 499 row from the HTTP bridge, exactly like the WebSocket bridge.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_stream_fanned_out_child_records_its_own_499_row() {
        let arguments = json!({"message": "Reply exactly CHILD_RESULT"});
        let spawn_item = json!({
            "id": "fc_1",
            "type": "function_call",
            "call_id": "call-child",
            "name": "multi_agent_spawn",
            "arguments": arguments.to_string(),
        })
        .to_string();
        let parent_blocks = vec![
            format!(
                "event: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"output\":[{spawn_item}]}}\n\n"
            ),
            format!(
                "event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"p1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[{spawn_item}],\"usage\":{{\"input_tokens\":2,\"output_tokens\":1,\"total_tokens\":3}}}}}}\n\n"
            ),
        ];
        let child_blocks = vec![
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"c1\"}}\n\n".into(),
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"CHILD\"}\n\n".into(),
        ];
        let (address, calls) =
            start_scripted_sse_upstream(vec![parent_blocks, child_blocks], Some("spawn a child"))
                .await;
        let state = sample_state_with_base(format!("http://{address}/v1"));
        let app = build_headless_router(state.clone(), unguarded());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let parent = reqwest::Client::new()
            .post(format!("http://{proxy}/v1/responses"))
            .header("content-type", "application/json")
            .header("x-request-id", "parent-linked")
            .json(&streaming_body("spawn a child"))
            .send()
            .await
            .unwrap();
        let parent_body = parent.text().await.unwrap();
        assert!(
            parent_body.contains("response.completed"),
            "the parent turn must reach its terminal event: {parent_body}"
        );

        let child_body = json!({
            "model": "vellum-mock",
            "instructions": "Reply exactly CHILD_RESULT",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "Reply exactly CHILD_RESULT"}]
            }],
            "stream": true
        });
        let child = reqwest::Client::new()
            .post(format!("http://{proxy}/v1/responses"))
            .header("content-type", "application/json")
            .header("x-request-id", "child-linked")
            .json(&child_body)
            .send()
            .await
            .unwrap();
        let mut child_stream = child.bytes_stream();

        // Wait until the child actually opened an upstream stream (the second
        // dispatch), which also means it linked to the parent at Point B and
        // registered its cancel channel.
        let dispatch_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while *calls.lock().unwrap() < 2 {
            assert!(
                std::time::Instant::now() < dispatch_deadline,
                "the child never reached upstream"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // The parent already completed, so its own cancel socket is long
        // released; this simulates *some* other trigger (an Official
        // `response.cancel`, another WebSocket route) reaching this parent's
        // tree after the fact. The registry is keyed by an internal execution
        // id a black-box test has no other way to name -- resolve it off the
        // still-pending spawn (Point C, the closing `function_call_output`,
        // hasn't run yet) rather than the external `x-request-id`, which is
        // deliberately never trusted as a cancellation key in production.
        let parent_execution_id = state
            .proxy_runtime()
            .debug_pending_spawn_parent_execution_id("parent-linked")
            .expect("Point B must have linked the child to a still-pending spawn");
        let children = state
            .proxy_runtime()
            .cancel_request_tree(&parent_execution_id);
        assert_eq!(children.len(), 1, "exactly the linked child must fan out");
        assert!(state.proxy_runtime().is_request_cancelled(&children[0]));

        // The child's HTTP body must end promptly without a terminal event.
        let read_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut body_done = false;
        while std::time::Instant::now() < read_deadline {
            match tokio::time::timeout(std::time::Duration::from_millis(200), child_stream.next())
                .await
            {
                Ok(None) => {
                    body_done = true;
                    break;
                }
                Ok(Some(Err(_))) => {
                    body_done = true;
                    break;
                }
                Ok(Some(Ok(_))) => continue,
                Err(_) => continue,
            }
        }
        assert!(
            body_done,
            "the fanned-out child's HTTP body must end after the parent cancel"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let row = loop {
            let records = state.proxy_runtime().usage_records().unwrap();
            if let Some(row) = records
                .iter()
                .find(|record| record.request_id.as_deref() == Some("child-linked"))
            {
                break row.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the fanned-out child never received its own 499 row"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert_eq!(row.status, 499);
        assert_eq!(row.outcome.as_deref(), Some("client_cancel"));
        assert_eq!(row.error.as_deref(), Some("cancelled by parent"));
    }
}
