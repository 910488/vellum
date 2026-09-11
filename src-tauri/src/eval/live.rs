//! Production Desktop live-attribution driver.
//!
//! Grok goes `DesktopCredentials` → the same Desktop runtime → `/responses`.
//! Official direct WSS always runs, even when the proxy turn fails. Official
//! first-frame timeout is fail-closed. Every request writes a redacted
//! artifact; the 50-turn Luna cap rejects surplus sends before they leave.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::Message;

use crate::error::{AppError, AppResult};
use crate::model::WireFormat;
use crate::proxy_runtime_bridge::DesktopProxyRuntimeState;
use crate::state::AppState;
use vellum_proxy_runtime::{
    account_hash, apply_ledger_frame, artifact_to_redacted_json, assert_no_upgrade_required,
    assert_official_request_parity, bounded_text, classify_http_status, classify_proxy_delay,
    classify_stream_quality, cold_total_first_frame_ms, evaluate_first_frame,
    extra_delay_budget_ms, is_output_delta, is_reasoning_delta, is_tool_delta, is_upgrade_required,
    ledger_frame_for_request, looks_like_qwen_36, looks_like_qwen_38, new_live_artifact,
    official_catalog_is_luna, official_live_create_body, official_live_http_body,
    official_live_marker_input, official_live_metadata_headers, plan_opencode_model,
    prepare_upstream_request_with_environment, redact_live_artifact, refuse_qwen_standin,
    resolve_official_live_model, select_qwen_from_models, should_stop_official_luna,
    streaming_qualifies, write_redacted_artifact, AbStage, FirstFrameObservation,
    GrokSessionRegistry, IncomingAuthContext, LedgerFrame, LiveAttributionError, LiveKind,
    LiveOutcome, LivePath, LiveRequestArtifact, OfficialTurnBudget, OpenCodeModelCaps,
    ProfileAdapterRoute, ProxyRuntimeState, RequestMetadata, ResolvedAuth, RuntimeEndpoint,
    RuntimeRequest, StreamQuality, StreamQualityInput, UnconfiguredOfficialAuthProvider,
    AB_IDLE_GAP_SECS, BOUNDARY_KEY_HEADER, LIVE_OFFICIAL_MODEL_ENV,
    OFFICIAL_FIRST_FRAME_DEADLINE_MS, PINNED_OFFICIAL_MODEL,
};

const OFFICIAL_DIRECT_WSS: &str = "wss://chatgpt.com/backend-api/codex/responses";

#[derive(Debug, Clone)]
pub struct LiveRunOptions {
    pub route: LiveRouteFilter,
    pub phase: LivePhase,
    pub artifact_dir: PathBuf,
    pub official_model_env: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveRouteFilter {
    Qwen,
    OpenCode,
    Grok,
    GrokAttribution,
    Official,
    Brave,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivePhase {
    Candidate,
}

impl LiveRouteFilter {
    pub fn parse(value: &str) -> AppResult<Self> {
        match value {
            "qwen" => Ok(Self::Qwen),
            "opencode" => Ok(Self::OpenCode),
            "grok" => Ok(Self::Grok),
            "grok-attribution" => Ok(Self::GrokAttribution),
            "official" => Ok(Self::Official),
            "brave" => Ok(Self::Brave),
            "all" => Ok(Self::All),
            other => Err(AppError::Message(format!(
                "unknown live route {other}; expected qwen|opencode|grok|grok-attribution|official|brave|all"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Qwen => "qwen",
            Self::OpenCode => "opencode",
            Self::Grok => "grok",
            Self::GrokAttribution => "grok-attribution",
            Self::Official => "official",
            Self::Brave => "brave",
            Self::All => "all",
        }
    }
}

impl LivePhase {
    pub fn parse(value: &str) -> AppResult<Self> {
        match value {
            "candidate" => Ok(Self::Candidate),
            "installed" => Err(AppError::Message(
                "live --phase installed is not a valid acceptance mode. Candidate live starts an in-process router from the current source tree and must not be labeled installed. Run Installed Gate D against the installed Desktop runtime instead.".into(),
            )),
            other => Err(AppError::Message(format!(
                "unknown live phase {other}; expected candidate"
            ))),
        }
    }
}

pub fn default_artifact_dir() -> PathBuf {
    std::env::var_os("VELLUM_LIVE_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/vellum-live"))
}

pub fn parse_live_args(args: &[String]) -> AppResult<LiveRunOptions> {
    let route = args
        .windows(2)
        .find(|pair| pair[0] == "--route")
        .map(|pair| pair[1].as_str())
        .unwrap_or("all");
    let phase = args
        .windows(2)
        .find(|pair| pair[0] == "--phase")
        .map(|pair| pair[1].as_str())
        .unwrap_or("candidate");
    let artifact_dir = args
        .windows(2)
        .find(|pair| pair[0] == "--artifact-dir")
        .map(|pair| PathBuf::from(&pair[1]))
        .unwrap_or_else(default_artifact_dir);
    Ok(LiveRunOptions {
        route: LiveRouteFilter::parse(route)?,
        phase: LivePhase::parse(phase)?,
        artifact_dir,
        official_model_env: std::env::var(LIVE_OFFICIAL_MODEL_ENV).ok(),
    })
}

pub fn write_env_failure(artifact_dir: &Path, route: &str, detail: &str) -> PathBuf {
    let _ = std::fs::create_dir_all(artifact_dir);
    let path = artifact_dir.join(format!("env-{route}.log"));
    let body = format!("route={route}\nblocked=true\ndetail={detail}\n");
    let _ = std::fs::write(&path, body);
    path
}

pub fn production_vellum_data_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|root| root.join("vellum"))
}

fn copy_if_exists(src: &Path, dst: &Path) {
    if !src.is_file() {
        return;
    }
    if let Some(parent) = dst.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::copy(src, dst);
}

/// Stage the production Desktop profile into an isolated data dir so the live
/// runner uses real credentials through `DesktopCredentials` without writing
/// back into the user's settings. Candidate live never opens
/// `%LOCALAPPDATA%\vellum` as an `AppState` root; the installed Desktop owns
/// that profile. Installed Gate D must drive the real Desktop, not this
/// in-process router.
pub fn stage_production_desktop_state() -> AppResult<(tempfile::TempDir, AppState)> {
    let src = production_vellum_data_dir().ok_or_else(|| {
        AppError::Message("cannot resolve the production Vellum data directory".into())
    })?;
    if !src.is_dir() {
        return Err(AppError::Message(format!(
            "production Vellum data directory missing: {}",
            src.display()
        )));
    }
    let temp = tempfile::tempdir()
        .map_err(|error| AppError::Message(format!("create live desktop staging dir: {error}")))?;
    let dst = temp.path();
    copy_if_exists(&src.join("settings.json"), &dst.join("settings.json"));
    copy_if_exists(&src.join("vellum-key.dpapi"), &dst.join("vellum-key.dpapi"));
    copy_if_exists(
        &src.join("vellum-key.protection"),
        &dst.join("vellum-key.protection"),
    );
    copy_if_exists(
        &src.join("codex_oauth_accounts.json"),
        &dst.join("codex_oauth_accounts.json"),
    );
    copy_if_exists(
        &src.join("grok_accounts").join("accounts.json"),
        &dst.join("grok_accounts").join("accounts.json"),
    );
    let cred_src = src.join("credentials");
    if cred_src.is_dir() {
        let cred_dst = dst.join("credentials");
        let _ = std::fs::create_dir_all(&cred_dst);
        if let Ok(entries) = std::fs::read_dir(cred_src) {
            for entry in entries.flatten() {
                if entry.path().is_file() {
                    let _ = std::fs::copy(entry.path(), cred_dst.join(entry.file_name()));
                }
            }
        }
    }
    let state = AppState::with_data_dir(dst.to_path_buf());
    state.activate_proxy_routes();
    let mut review = state.review_settings();
    review.before_send = false;
    review.on_edit = false;
    review.before_compact = false;
    state.set_review_settings(review)?;
    Ok((temp, state))
}

fn live_first_frame_budget() -> Duration {
    let secs = std::env::var("VELLUM_LIVE_FIRST_FRAME_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(90)
        .clamp(15, 600);
    Duration::from_secs(secs)
}

fn http_client() -> AppResult<reqwest::Client> {
    let request_timeout = live_first_frame_budget() + Duration::from_secs(60);
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(request_timeout)
        .build()
        .map_err(|error| AppError::Message(format!("build live HTTP client: {error}")))
}

struct LiveSession {
    _staging: tempfile::TempDir,
    state: AppState,
    desktop: DesktopProxyRuntimeState,
    addr: SocketAddr,
    boundary: String,
    _server: tokio::task::JoinHandle<()>,
}

impl LiveSession {
    async fn start() -> AppResult<Self> {
        eprintln!("live: staging production Desktop state");
        let (staging, state) = stage_production_desktop_state()?;
        eprintln!("live: building DesktopProxyRuntimeState");
        let desktop = DesktopProxyRuntimeState::new(state.clone()).map_err(AppError::Message)?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| AppError::Message(format!("bind live desktop router: {error}")))?;
        let addr = listener
            .local_addr()
            .map_err(|error| AppError::Message(format!("live router addr: {error}")))?;
        let boundary = vellum_proxy_runtime::BoundaryKey::generate()
            .map_err(|error| AppError::Message(format!("live boundary key: {error}")))?;
        let exposed = boundary.expose_for_storage().to_string();
        let app = vellum_proxy_runtime::build_headless_router(
            Arc::new(desktop.clone()),
            vellum_proxy_runtime::InboundAccessPolicy::authenticated(
                vellum_proxy_runtime::BOUNDARY_CREDENTIAL_ID,
                boundary,
                addr.port(),
            ),
        );
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        eprintln!("live: waiting for /readyz on {addr}");
        let ready = http_client()?
            .get(format!("http://{addr}/readyz"))
            .header(BOUNDARY_KEY_HEADER, &exposed)
            .send()
            .await
            .map_err(|error| AppError::Message(format!("live /readyz: {error}")))?;
        if !ready.status().is_success() {
            return Err(AppError::Message(format!(
                "Desktop router /readyz failed: {}",
                ready.status()
            )));
        }
        Ok(Self {
            _staging: staging,
            state,
            desktop,
            addr,
            boundary: exposed,
            _server: server,
        })
    }

    fn client(&self) -> reqwest::Client {
        http_client().expect("live HTTP client")
    }
}

pub async fn run_live(options: LiveRunOptions) -> AppResult<Vec<PathBuf>> {
    std::fs::create_dir_all(&options.artifact_dir).map_err(|error| {
        AppError::Message(format!(
            "create artifact dir {}: {error}",
            options.artifact_dir.display()
        ))
    })?;
    let session = match LiveSession::start().await {
        Ok(session) => {
            eprintln!("live: desktop router ready");
            session
        }
        Err(error) => {
            let path = write_env_failure(
                &options.artifact_dir,
                options.route.as_str(),
                &error.to_string(),
            );
            return Err(AppError::Message(format!(
                "live launcher blocked; wrote {}: {error}",
                path.display()
            )));
        }
    };
    let mut written = Vec::new();
    let filters = match options.route {
        LiveRouteFilter::All => vec![
            LiveRouteFilter::Qwen,
            LiveRouteFilter::OpenCode,
            LiveRouteFilter::Grok,
            LiveRouteFilter::Official,
        ],
        other => vec![other],
    };
    for filter in filters {
        let result = match filter {
            LiveRouteFilter::Qwen => run_qwen(&session, &options).await,
            LiveRouteFilter::OpenCode => run_opencode(&session, &options).await,
            LiveRouteFilter::Grok => run_grok(&session, &options).await,
            LiveRouteFilter::GrokAttribution => run_grok_attribution(&session, &options).await,
            LiveRouteFilter::Official => run_official(&session, &options).await,
            LiveRouteFilter::Brave => run_brave_roundtrip(&session, &options).await,
            LiveRouteFilter::All => unreachable!(),
        };
        match result {
            Ok(paths) => written.extend(paths),
            Err(error) => {
                write_env_failure(&options.artifact_dir, filter.as_str(), &error.to_string());
                return Err(error);
            }
        }
    }
    Ok(written)
}

async fn list_models(session: &LiveSession) -> AppResult<Value> {
    let response = session
        .client()
        .get(format!("http://{}/v1/models", session.addr))
        .header(BOUNDARY_KEY_HEADER, &session.boundary)
        .send()
        .await
        .map_err(|error| AppError::Message(format!("GET /v1/models: {error}")))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| AppError::Message(format!("/v1/models body: {error}")))?;
    if !status.is_success() {
        return Err(AppError::Message(format!(
            "GET /v1/models failed: {status} {body}"
        )));
    }
    serde_json::from_str(&body)
        .map_err(|error| AppError::Message(format!("parse /v1/models: {error}")))
}

fn catalog_ids(models: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(data) = models.get("data").and_then(Value::as_array) {
        for entry in data {
            if let Some(id) = entry.get("id").and_then(Value::as_str) {
                ids.push(id.to_string());
            }
        }
    }
    if let Some(native) = models.get("models").and_then(Value::as_array) {
        for entry in native {
            if let Some(id) = entry
                .get("slug")
                .or_else(|| entry.get("id"))
                .and_then(Value::as_str)
            {
                if !ids.iter().any(|existing| existing == id) {
                    ids.push(id.to_string());
                }
            }
        }
    }
    ids
}

/// Read `request_id`'s upstream/downstream first-frame timing out of the
/// Desktop runtime's own usage ledger and apply it onto `artifact`, instead
/// of a caller-observed timestamp copied into both fields (which always
/// makes `bridge_delay_ms` zero, defeating the point of the two fields).
fn apply_desktop_ledger(
    session: &LiveSession,
    artifact: &mut LiveRequestArtifact,
    request_id: &str,
) {
    let records = session
        .desktop
        .proxy_runtime()
        .usage_records()
        .unwrap_or_default();
    let ledger = ledger_frame_for_request(&records, request_id);
    apply_ledger_frame(artifact, &ledger);
}

#[allow(clippy::too_many_arguments)]
async fn post_responses_sse(
    session: &LiveSession,
    route: &str,
    catalog_model: &str,
    wire: &str,
    path: LivePath,
    kind: LiveKind,
    body: Value,
    account_id: Option<&str>,
) -> AppResult<LiveRequestArtifact> {
    let mut artifact = new_live_artifact(route, catalog_model, wire, path, account_id);
    artifact.kind = Some(format!("{kind:?}").to_ascii_lowercase());
    let started = Instant::now();
    let ledger_request_id = format!("live-{}", ulid::Ulid::new());
    let request = session
        .client()
        .post(format!("http://{}/v1/responses", session.addr))
        .header(BOUNDARY_KEY_HEADER, &session.boundary)
        .header("content-type", "application/json")
        .header("x-request-id", &ledger_request_id)
        .json(&body);
    eprintln!("live: POST /v1/responses model={catalog_model} kind={kind:?}");
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            eprintln!("live: POST /v1/responses failed: {error}");
            artifact.outcome = LiveOutcome::Fail;
            artifact.error_category = Some("provider_unavailable".into());
            artifact.completed_ms = Some(started.elapsed().as_millis() as u64);
            return Ok(finish_error(artifact, error.to_string()));
        }
    };
    eprintln!(
        "live: POST /v1/responses status={} after {}ms",
        response.status(),
        started.elapsed().as_millis()
    );
    artifact.connection_ms = Some(started.elapsed().as_millis() as u64);
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let detail = match response.text().await {
            Ok(text) => text.chars().take(600).collect::<String>(),
            Err(_) => "(no body)".to_string(),
        };
        artifact.upstream_status = Some(status);
        artifact.diagnostic = Some(detail);
        if is_upgrade_required(status) {
            artifact.outcome = LiveOutcome::Fail;
            artifact.error_category = Some("provider_protocol".into());
            let _ = assert_no_upgrade_required(Some(status));
        } else if status == 429 {
            artifact.outcome = LiveOutcome::Quota;
            artifact.error_category = Some("provider_quota".into());
        } else {
            artifact.outcome = LiveOutcome::Fail;
            artifact.error_category = Some(classify_http_status(status).into());
        }
        artifact.completed_ms = Some(started.elapsed().as_millis() as u64);
        return Ok(artifact);
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !content_type.contains("event-stream") {
        let streaming_requested = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        match response.json::<Value>().await {
            Ok(value) => {
                artifact.frame_count = 1;
                artifact.delta_count = 0;
                apply_desktop_ledger(session, &mut artifact, &ledger_request_id);
                artifact.completed_ms = Some(started.elapsed().as_millis() as u64);
                artifact.usage = value.pointer("/usage").cloned();
                artifact.request_id = value.get("id").and_then(Value::as_str).map(str::to_string);
                let quality = classify_stream_quality(&StreamQualityInput {
                    streaming_requested,
                    capability_streaming: streaming_requested,
                    streamed: false,
                    output_delta_count: 0,
                    first_output_delta_ms: None,
                    tool_delta_count: 0,
                    first_tool_delta_ms: None,
                    terminal_ms: artifact.completed_ms,
                    reasoning_delta_count: 0,
                    first_reasoning_delta_ms: None,
                });
                artifact.stream_quality = Some(quality);
                artifact.outcome = match quality {
                    // A buffered JSON response is only `buffered` when the
                    // route/capability declared non-streaming; otherwise the
                    // stream contract was silently downgraded.
                    StreamQuality::Buffered => LiveOutcome::Ok,
                    StreamQuality::NoDelta => LiveOutcome::Fail,
                    _ => LiveOutcome::Fail,
                };
                if artifact.outcome == LiveOutcome::Fail {
                    artifact.error_category = Some("no_stream_delta".into());
                }
            }
            Err(error) => {
                artifact.outcome = LiveOutcome::Fail;
                artifact.error_category = Some(error.to_string());
                artifact.completed_ms = Some(started.elapsed().as_millis() as u64);
            }
        }
        return Ok(artifact);
    }

    let mut first_frame = None;
    let mut output_delta_count = 0_u64;
    let mut reasoning_delta_count = 0_u64;
    let mut first_output_delta_ms = None;
    let mut first_reasoning_delta_ms = None;
    let mut tool_delta_count = 0_u64;
    let mut first_tool_delta_ms = None;
    let mut frame_count = 0_u64;
    let mut usage = None;
    let mut request_id = None;
    let mut outcome = LiveOutcome::Fail;
    let mut error_category = None;
    let mut stream = response.bytes_stream();
    let mut pending = String::new();
    let streamed = true;
    // First-frame budget starts after headers. Counting from request start
    // would mark a slow-but-valid upstream connect as a timeout and skip
    // the stream entirely (seen on Qwen 3.8 proxy: 280s to HTTP 200).
    let first_frame_budget = live_first_frame_budget();
    let headers_at = Instant::now();
    loop {
        if headers_at.elapsed() > first_frame_budget && first_frame.is_none() {
            let _ = evaluate_first_frame(FirstFrameObservation::TimedOut {
                deadline_ms: first_frame_budget.as_millis() as u64,
            });
            outcome = LiveOutcome::Timeout;
            error_category = Some("first_frame_timeout".into());
            break;
        }
        let chunk_budget = if first_frame.is_none() {
            first_frame_budget.saturating_sub(headers_at.elapsed())
        } else {
            Duration::from_secs(30)
        };
        let next = tokio::time::timeout(chunk_budget, stream.next()).await;
        let chunk = match next {
            Ok(Some(Ok(bytes))) => bytes,
            Ok(Some(Err(error))) => {
                error_category = Some("provider_protocol".into());
                let _ = error;
                break;
            }
            Ok(None) => break,
            Err(_) => {
                if first_frame.is_none() {
                    outcome = LiveOutcome::Timeout;
                    error_category = Some("first_frame_timeout".into());
                }
                break;
            }
        };
        pending.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(index) = pending.find("\n\n") {
            let block = pending[..index].to_string();
            pending = pending[index + 2..].to_string();
            if let Some(data) = sse_data(&block) {
                if first_frame.is_none() {
                    match evaluate_first_frame(FirstFrameObservation::Observed {
                        elapsed_ms: started.elapsed().as_millis() as u64,
                    }) {
                        Ok(ms) => first_frame = Some(ms),
                        Err(_) => {
                            outcome = LiveOutcome::Timeout;
                            error_category = Some("first_frame_timeout".into());
                            break;
                        }
                    }
                }
                frame_count += 1;
                if let Ok(value) = serde_json::from_str::<Value>(&data) {
                    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
                    if is_output_delta(kind) {
                        output_delta_count += 1;
                        first_output_delta_ms.get_or_insert(started.elapsed().as_millis() as u64);
                        if is_tool_delta(kind) {
                            tool_delta_count += 1;
                            first_tool_delta_ms.get_or_insert(started.elapsed().as_millis() as u64);
                        }
                    } else if is_reasoning_delta(kind) {
                        reasoning_delta_count += 1;
                        first_reasoning_delta_ms
                            .get_or_insert(started.elapsed().as_millis() as u64);
                    }
                    if let Some(id) = value
                        .pointer("/response/id")
                        .or_else(|| value.get("id"))
                        .and_then(Value::as_str)
                    {
                        request_id = Some(id.to_string());
                    }
                    if let Some(found) = value.pointer("/response/usage").cloned() {
                        usage = Some(found);
                    }
                    if kind == "response.completed" {
                        outcome = LiveOutcome::Ok;
                    }
                    if kind == "response.failed" {
                        outcome = LiveOutcome::Fail;
                        error_category = Some(
                            value
                                .pointer("/response/error/type")
                                .and_then(Value::as_str)
                                .unwrap_or("provider_protocol")
                                .to_string(),
                        );
                    }
                }
            }
        }
        if matches!(
            outcome,
            LiveOutcome::Ok | LiveOutcome::Quota | LiveOutcome::Timeout
        ) {
            break;
        }
    }
    apply_desktop_ledger(session, &mut artifact, &ledger_request_id);
    artifact.completed_ms = Some(started.elapsed().as_millis() as u64);
    artifact.delta_count = output_delta_count + reasoning_delta_count;
    artifact.output_delta_count = output_delta_count;
    artifact.reasoning_delta_count = reasoning_delta_count;
    artifact.first_output_delta_ms = first_output_delta_ms;
    artifact.first_reasoning_delta_ms = first_reasoning_delta_ms;
    artifact.frame_count = frame_count;
    artifact.usage = usage;
    artifact.request_id = request_id;
    artifact.outcome = outcome;
    artifact.error_category = error_category;
    let quality = classify_stream_quality(&StreamQualityInput {
        streaming_requested: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        capability_streaming: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        streamed,
        output_delta_count,
        first_output_delta_ms,
        tool_delta_count,
        first_tool_delta_ms,
        terminal_ms: artifact.completed_ms,
        reasoning_delta_count,
        first_reasoning_delta_ms,
    });
    artifact.stream_quality = Some(quality);
    if artifact.outcome == LiveOutcome::Ok {
        // Outcome policy: `no_delta` is a protocol failure on a turn that was
        // expected to stream text; `end_flush` fails strict Responses/Official
        // but a third-party Chat turn may keep its transport outcome while
        // failing streaming qualification (recorded via the quality field).
        match quality {
            StreamQuality::Incremental | StreamQuality::Buffered => {}
            StreamQuality::EndFlush if wire == "chat" => {}
            StreamQuality::EndFlush => {
                artifact.outcome = LiveOutcome::Fail;
                artifact.error_category = Some("end_of_stream_flush".into());
            }
            StreamQuality::NoDelta => {
                artifact.outcome = LiveOutcome::Fail;
                artifact.error_category = Some("no_stream_delta".into());
            }
        }
    }
    Ok(artifact)
}

fn sse_data(block: &str) -> Option<String> {
    let mut data = Vec::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.trim_start());
        }
    }
    if data.is_empty() {
        None
    } else {
        Some(data.join("\n"))
    }
}

fn finish_error(mut artifact: LiveRequestArtifact, detail: String) -> LiveRequestArtifact {
    if artifact.error_category.is_none() {
        artifact.error_category = Some(detail);
    }
    artifact
}

/// Median of non-empty sample; falls back to 0 for an empty slice (callers
/// guard emptiness before use).
fn median_u64(values: &[u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2
    }
}

async fn run_grok(session: &LiveSession, options: &LiveRunOptions) -> AppResult<Vec<PathBuf>> {
    let routes = session.desktop.active_model_routes();
    // Validate the Grok credential once, then run every enabled Grok route
    // (4.6 and 4.5 each get the Marker/Sse/TypedTool matrix).
    let secret = session
        .desktop
        .credentials()
        .await
        .get_secret("grok-cli")
        .await
        .map_err(AppError::Message)?
        .ok_or_else(|| AppError::Message("DesktopCredentials returned no Grok secret".into()))?;
    let parsed: Value = serde_json::from_str(&secret)
        .map_err(|error| AppError::Message(format!("DesktopCredentials Grok JSON: {error}")))?;
    if parsed["client_version"]
        .as_str()
        .is_none_or(|value| value.is_empty())
    {
        return Err(AppError::Message(
            "DesktopCredentials must include client_version".into(),
        ));
    }
    if parsed["access_token"]
        .as_str()
        .is_none_or(|value| value.len() < 20)
    {
        return Err(AppError::Message(
            "DesktopCredentials must include an access token".into(),
        ));
    }
    let account = parsed["user_id"].as_str();
    let grok_routes: Vec<String> = {
        let mut seen = Vec::new();
        let mut base: Vec<_> = routes
            .iter()
            .filter(|route| route.route_id == "grok-cli")
            .collect();
        // Prefer the newest first (4.6 before 4.5), then keep any remainder.
        base.sort_by_key(|route| {
            let newer = route.catalog_id.contains("4.6") || route.upstream_model.contains("4.6");
            std::cmp::Reverse(newer)
        });
        for route in base {
            if !seen.contains(&route.catalog_id) {
                seen.push(route.catalog_id.clone());
            }
        }
        seen
    };
    if grok_routes.is_empty() {
        return Err(AppError::Message("no enabled Grok catalog route".into()));
    }
    let mut written = Vec::new();
    let mut failures = Vec::new();
    for catalog_id in grok_routes {
        written
            .extend(run_grok_matrix(session, options, &catalog_id, account, &mut failures).await?);
    }
    if !failures.is_empty() {
        eprintln!("live: Grok matrix had failures: {}", failures.join(", "));
    }
    Ok(written)
}

async fn run_grok_matrix(
    session: &LiveSession,
    options: &LiveRunOptions,
    catalog_id: &str,
    account: Option<&str>,
    failures: &mut Vec<String>,
) -> AppResult<Vec<PathBuf>> {
    let mut written = Vec::new();
    for (kind, body) in grok_kind_bodies(catalog_id) {
        let artifact = post_responses_sse(
            session,
            "grok-cli",
            catalog_id,
            "responses",
            LivePath::Proxy,
            kind,
            body,
            account,
        )
        .await?;
        if matches!(artifact.outcome, LiveOutcome::Fail | LiveOutcome::Timeout)
            && artifact
                .error_category
                .as_deref()
                .is_some_and(|value| value.contains("426"))
        {
            return Err(AppError::Message("Grok returned HTTP 426".into()));
        }
        let name = format!("grok-{}-{:?}-proxy.json", catalog_id, kind)
            .to_ascii_lowercase()
            .replace([':', '/'], "_");
        written.push(
            write_redacted_artifact(&options.artifact_dir, &name, &artifact)
                .map_err(|error| AppError::Message(error.to_string()))?,
        );
        if artifact.outcome != LiveOutcome::Ok {
            // A quality-classified finding (e.g. `end_flush`) is evidence, not
            // a reason to drop the remaining kinds: write every artifact and
            // report the full matrix at the end.
            failures.push(format!(
                "{catalog_id}:{kind:?}={}",
                artifact.error_category.unwrap_or_default()
            ));
        }
    }
    Ok(written)
}

/// Majority verdict for one Grok case (e.g. `grok-4.6` SSE) across N=3
/// direct and N=3 proxy runs, per the finalization plan:
///
/// - Direct and proxy at least 2/3 `end_flush` → `provider_behavior`.
/// - Direct at least 2/3 `incremental` while proxy is at least 2/3
///   `end_flush` → `vellum_regression`.
/// - Both lanes at least 2/3 `incremental` → `both_lanes_incremental`: the
///   case no longer reproduces at all.
/// - Direct at least 2/3 `end_flush` while proxy is at least 2/3
///   `incremental` → `no_vellum_regression`: the proxy still delivers what
///   the provider natively flushes.
/// - Anything else (split results or provider errors before any quality was
///   classified) → `inconclusive`; never declared passed.
fn grok_attribution_verdict(
    direct: &[Option<StreamQuality>],
    proxy: &[Option<StreamQuality>],
) -> &'static str {
    if direct.len() < 2 || proxy.len() < 2 {
        return "inconclusive";
    }
    // A lane sample that failed before any quality was classified is a
    // provider error: the case is inconclusive, never attributed.
    if direct.iter().any(Option::is_none) || proxy.iter().any(Option::is_none) {
        return "inconclusive";
    }
    let direct_flush = direct
        .iter()
        .filter(|quality| matches!(quality, Some(StreamQuality::EndFlush)))
        .count();
    let direct_incremental = direct
        .iter()
        .filter(|quality| matches!(quality, Some(StreamQuality::Incremental)))
        .count();
    let proxy_flush = proxy
        .iter()
        .filter(|quality| matches!(quality, Some(StreamQuality::EndFlush)))
        .count();
    if direct_flush >= 2 && proxy_flush >= 2 {
        return "provider_behavior";
    }
    if direct_incremental >= 2 && proxy_flush >= 2 {
        return "vellum_regression";
    }
    // Both lanes streaming incrementally means the case no longer reproduces
    // at all: no provider fault and no Vellum regression to fix.
    let proxy_incremental = proxy
        .iter()
        .filter(|quality| matches!(quality, Some(StreamQuality::Incremental)))
        .count();
    if direct_incremental >= 2 && proxy_incremental >= 2 {
        return "both_lanes_incremental";
    }
    // Direct is the provider ground truth: a provider-native `end_flush` that
    // the proxy still delivers incrementally is not a Vellum regression.
    if direct_flush >= 2 && proxy_incremental >= 2 {
        return "no_vellum_regression";
    }
    "inconclusive"
}

/// Direct Grok: the same production credential/header resolution the Desktop
/// runtime uses (`ResolvedAuth` Grok session headers over the same
/// `RuntimeModelRoute`), sent as a raw HTTP POST to the upstream, bypassing
/// the Vellum streaming adapter entirely.
async fn grok_direct_responses_sse(
    session: &LiveSession,
    catalog_id: &str,
    upstream_model: &str,
    kind: LiveKind,
    body: Value,
    account: Option<&str>,
) -> AppResult<LiveRequestArtifact> {
    let mut artifact = new_live_artifact(
        "grok-cli",
        catalog_id,
        "responses",
        LivePath::Direct,
        account,
    );
    artifact.kind = Some(format!("{kind:?}").to_ascii_lowercase());
    let route = session
        .state
        .routes()
        .into_iter()
        .find(|route| route.id == "grok-cli")
        .ok_or_else(|| AppError::Message("Grok direct: grok-cli route missing".into()))?;
    let model = session
        .state
        .model_routes()
        .into_iter()
        .find(|model| {
            model.route_id == "grok-cli"
                && (model.upstream_model == upstream_model || model.catalog_id == upstream_model)
        })
        .ok_or_else(|| {
            AppError::Message(format!(
                "Grok direct: model route for {upstream_model} missing"
            ))
        })?;
    let runtime_route = crate::proxy_runtime_bridge::runtime_route(&session.state, &route, &model);
    let request = RuntimeRequest {
        body: body.clone(),
        endpoint: RuntimeEndpoint::Responses,
        incoming_auth: IncomingAuthContext::default(),
        execution_environment: session.desktop.execution_environment(),
        metadata: RequestMetadata {
            request_id: format!("grok-direct-{}", ulid::Ulid::new()),
            received_at_ms: 0,
            review_run_id: None,
            review_role: None,
            primary_failure_reason: None,
            connection_id: None,
            ..Default::default()
        },
    };
    let credentials = session.desktop.credentials().await;
    let auth = ResolvedAuth::resolve(
        &runtime_route,
        &request,
        credentials.as_ref(),
        &UnconfiguredOfficialAuthProvider,
        &GrokSessionRegistry::new(),
        None,
    )
    .await
    .map_err(|error| AppError::Message(format!("Grok direct auth resolution: {error}")))?;
    // The direct lane must post the *same normalized upstream request* the
    // runtime would send: the same adapter profile, environment, and
    // web-search wrapper decision. Only the Vellum streaming adapter itself
    // is bypassed; an unnormalized body is rejected by the upstream.
    let adapter_route = ProfileAdapterRoute::from(&runtime_route);
    let profile = vellum_proxy_runtime::harness::resolve_with_options(
        runtime_route.provider_kind,
        runtime_route.wire,
        crate::harness::harness_options_from_env(),
        crate::harness::multi_agent::RUNTIME_WIRED,
    );
    let web_search_enabled = session.state.web_search_settings().enabled
        && crate::credentials::load(
            &session.state.data_root(),
            crate::web_search::BRAVE_SEARCH_CREDENTIAL_ID,
        )
        .ok()
        .flatten()
        .is_some();
    let prepared = prepare_upstream_request_with_environment(
        &body,
        &adapter_route,
        &profile,
        &request.execution_environment,
        None,
        web_search_enabled,
    )
    .map_err(|error| AppError::Message(format!("Grok direct normalization: {error}")))?;
    let mut headers = auth.upstream_headers(&runtime_route.upstream_model);
    headers.push(("content-type".into(), "application/json".into()));
    let url = format!("{}/responses", runtime_route.base_url.trim_end_matches('/'));
    let started = Instant::now();
    eprintln!("live: Grok direct POST {url} model={upstream_model} kind={kind:?}");
    let client = http_client()?;
    let mut request_builder = client.post(&url).json(&prepared);
    for (name, value) in headers {
        request_builder = request_builder.header(name, value);
    }
    let response = match request_builder.send().await {
        Ok(response) => response,
        Err(error) => {
            artifact.outcome = LiveOutcome::Fail;
            artifact.error_category = Some("provider_unavailable".into());
            artifact.completed_ms = Some(started.elapsed().as_millis() as u64);
            return Ok(finish_error(artifact, error.to_string()));
        }
    };
    artifact.connection_ms = Some(started.elapsed().as_millis() as u64);
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let detail = match response.text().await {
            Ok(text) => text.chars().take(600).collect::<String>(),
            Err(_) => "(no body)".to_string(),
        };
        eprintln!("live: Grok direct upstream rejected status={status}: {detail}",);
        artifact.outcome = if status == 429 {
            LiveOutcome::Quota
        } else {
            LiveOutcome::Fail
        };
        artifact.error_category = if is_upgrade_required(status) {
            Some("provider_protocol".into())
        } else if status == 429 {
            Some("provider_quota".into())
        } else {
            Some(classify_http_status(status).into())
        };
        artifact.upstream_status = Some(status);
        artifact.diagnostic = Some(detail);
        artifact.completed_ms = Some(started.elapsed().as_millis() as u64);
        return Ok(artifact);
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let streaming_requested = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if !content_type.contains("event-stream") {
        match response.json::<Value>().await {
            Ok(value) => {
                artifact.frame_count = 1;
                artifact.completed_ms = Some(started.elapsed().as_millis() as u64);
                artifact.usage = value.pointer("/usage").cloned();
                artifact.request_id = value.get("id").and_then(Value::as_str).map(str::to_string);
                artifact.stream_quality = Some(classify_stream_quality(&StreamQualityInput {
                    streaming_requested,
                    capability_streaming: streaming_requested,
                    streamed: false,
                    output_delta_count: 0,
                    first_output_delta_ms: None,
                    tool_delta_count: 0,
                    first_tool_delta_ms: None,
                    terminal_ms: artifact.completed_ms,
                    reasoning_delta_count: 0,
                    first_reasoning_delta_ms: None,
                }));
                artifact.outcome = LiveOutcome::Ok;
            }
            Err(error) => {
                artifact.outcome = LiveOutcome::Fail;
                artifact.error_category = Some(error.to_string());
                artifact.completed_ms = Some(started.elapsed().as_millis() as u64);
            }
        }
        return Ok(artifact);
    }

    let mut first_frame = None;
    let mut output_delta_count = 0_u64;
    let mut reasoning_delta_count = 0_u64;
    let mut first_output_delta_ms = None;
    let mut first_reasoning_delta_ms = None;
    let mut tool_delta_count = 0_u64;
    let mut first_tool_delta_ms = None;
    let mut frame_count = 0_u64;
    let mut usage = None;
    let mut request_id = None;
    let mut outcome = LiveOutcome::Fail;
    let mut error_category = None;
    let mut stream = response.bytes_stream();
    let mut pending = String::new();
    let first_frame_budget = live_first_frame_budget();
    let headers_at = Instant::now();
    loop {
        if headers_at.elapsed() > first_frame_budget && first_frame.is_none() {
            outcome = LiveOutcome::Timeout;
            error_category = Some("first_frame_timeout".into());
            break;
        }
        let chunk_budget = if first_frame.is_none() {
            first_frame_budget.saturating_sub(headers_at.elapsed())
        } else {
            Duration::from_secs(30)
        };
        let next = tokio::time::timeout(chunk_budget, stream.next()).await;
        let chunk = match next {
            Ok(Some(Ok(bytes))) => bytes,
            Ok(Some(Err(error))) => {
                error_category = Some("provider_protocol".into());
                let _ = error;
                break;
            }
            Ok(None) => break,
            Err(_) => {
                if first_frame.is_none() {
                    outcome = LiveOutcome::Timeout;
                    error_category = Some("first_frame_timeout".into());
                }
                break;
            }
        };
        pending.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(index) = pending.find("\n\n") {
            let block = pending[..index].to_string();
            pending = pending[index + 2..].to_string();
            if let Some(data) = sse_data(&block) {
                if first_frame.is_none() {
                    first_frame = Some(started.elapsed().as_millis() as u64);
                }
                frame_count += 1;
                if let Ok(value) = serde_json::from_str::<Value>(&data) {
                    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
                    if is_output_delta(kind) {
                        output_delta_count += 1;
                        first_output_delta_ms.get_or_insert(started.elapsed().as_millis() as u64);
                        if is_tool_delta(kind) {
                            tool_delta_count += 1;
                            first_tool_delta_ms.get_or_insert(started.elapsed().as_millis() as u64);
                        }
                    } else if is_reasoning_delta(kind) {
                        reasoning_delta_count += 1;
                        first_reasoning_delta_ms
                            .get_or_insert(started.elapsed().as_millis() as u64);
                    }
                    if let Some(id) = value
                        .pointer("/response/id")
                        .or_else(|| value.get("id"))
                        .and_then(Value::as_str)
                    {
                        request_id = Some(id.to_string());
                    }
                    if let Some(found) = value.pointer("/response/usage").cloned() {
                        usage = Some(found);
                    }
                    if kind == "response.completed" {
                        outcome = LiveOutcome::Ok;
                    }
                    if kind == "response.failed" {
                        outcome = LiveOutcome::Fail;
                        error_category = Some(
                            value
                                .pointer("/response/error/type")
                                .and_then(Value::as_str)
                                .unwrap_or("provider_protocol")
                                .to_string(),
                        );
                    }
                }
            }
        }
        if matches!(outcome, LiveOutcome::Ok | LiveOutcome::Timeout) {
            break;
        }
    }
    artifact.downstream_first_frame_ms = first_frame;
    artifact.upstream_first_frame_ms = first_frame;
    artifact.frame_count = frame_count;
    artifact.usage = usage;
    artifact.request_id = request_id;
    artifact.outcome = outcome;
    artifact.completed_ms = Some(started.elapsed().as_millis() as u64);
    artifact.delta_count = output_delta_count;
    artifact.output_delta_count = output_delta_count;
    artifact.reasoning_delta_count = reasoning_delta_count;
    artifact.first_output_delta_ms = first_output_delta_ms;
    artifact.first_reasoning_delta_ms = first_reasoning_delta_ms;
    let quality = classify_stream_quality(&StreamQualityInput {
        streaming_requested,
        capability_streaming: true,
        streamed: true,
        output_delta_count,
        first_output_delta_ms,
        tool_delta_count,
        first_tool_delta_ms,
        terminal_ms: artifact.completed_ms,
        reasoning_delta_count,
        first_reasoning_delta_ms,
    });
    artifact.stream_quality = Some(quality);
    if outcome == LiveOutcome::Ok {
        match quality {
            StreamQuality::Incremental => {}
            StreamQuality::EndFlush => {
                artifact.outcome = LiveOutcome::Fail;
                artifact.error_category = Some("end_of_stream_flush".into());
            }
            StreamQuality::NoDelta => {
                artifact.outcome = LiveOutcome::Fail;
                artifact.error_category = Some("no_stream_delta".into());
            }
            StreamQuality::Buffered => {}
        }
    }
    if let Some(error) = error_category {
        if artifact.error_category.is_none() {
            artifact.error_category = Some(error);
        }
    }
    Ok(artifact)
}

/// Run the 3-direct/3-proxy Grok attribution matrix for the two failing cases
/// observed in qualification (`grok-4.6` SSE, `grok-4.5` typed-tool, or the
/// Sse/TypedTool kinds of every enabled Grok route when only one model is
/// present), then a buffered fallback run per case so the catalog decision is
/// evidence-based: `end_flush` on both lanes is provider behavior, a
/// Vellum-only `end_flush` is a regression to fix, and the buffered outcome
/// decides between `streaming=false` projection and hiding the model.
async fn run_grok_attribution(
    session: &LiveSession,
    options: &LiveRunOptions,
) -> AppResult<Vec<PathBuf>> {
    let routes = session.desktop.active_model_routes();
    let secret = session
        .desktop
        .credentials()
        .await
        .get_secret("grok-cli")
        .await
        .map_err(AppError::Message)?
        .ok_or_else(|| AppError::Message("DesktopCredentials returned no Grok secret".into()))?;
    let parsed: Value = serde_json::from_str(&secret)
        .map_err(|error| AppError::Message(format!("DesktopCredentials Grok JSON: {error}")))?;
    if parsed["client_version"]
        .as_str()
        .is_none_or(|value| value.is_empty())
        || parsed["access_token"]
            .as_str()
            .is_none_or(|value| value.len() < 20)
    {
        return Err(AppError::Message(
            "DesktopCredentials must include client_version and an access token".into(),
        ));
    }
    let account = parsed["user_id"].as_str();
    let mut grok_routes: Vec<String> = {
        let mut seen = Vec::new();
        let mut base: Vec<_> = routes
            .iter()
            .filter(|route| route.route_id == "grok-cli")
            .collect();
        base.sort_by_key(|route| {
            let newer = route.catalog_id.contains("4.6") || route.upstream_model.contains("4.6");
            std::cmp::Reverse(newer)
        });
        for route in base {
            if !seen.contains(&route.catalog_id) {
                seen.push(route.catalog_id.clone());
            }
        }
        seen
    };
    if grok_routes.is_empty() {
        return Err(AppError::Message("no enabled Grok catalog route".into()));
    }
    // The two observed failing cases default; an explicit env list overrides.
    if let Some(cases) = std::env::var("VELLUM_LIVE_GROK_CASES")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        grok_routes = cases
            .split(',')
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect();
    }
    let repeats = std::env::var("VELLUM_LIVE_GROK_REPEATS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(3)
        .max(1);
    let mut written = Vec::new();
    let mut case_summaries = Vec::new();
    for catalog_id in &grok_routes {
        for (kind, body) in grok_kind_bodies(catalog_id) {
            if !matches!(kind, LiveKind::Sse | LiveKind::TypedTool) {
                continue;
            }
            let mut direct_qualities = Vec::new();
            let mut proxy_qualities = Vec::new();
            for index in 0..repeats {
                let direct = grok_direct_responses_sse(
                    session,
                    catalog_id,
                    catalog_id,
                    kind,
                    body.clone(),
                    account,
                )
                .await?;
                direct_qualities.push(direct.stream_quality);
                written.push(
                    write_redacted_artifact(
                        &options.artifact_dir,
                        &format!(
                            "grok-attribution-{}-{:?}-direct-{index}.json",
                            catalog_id.replace([':', '/'], "_"),
                            kind
                        )
                        .to_ascii_lowercase(),
                        &direct,
                    )
                    .map_err(|error| AppError::Message(error.to_string()))?,
                );
                let proxy = post_responses_sse(
                    session,
                    "grok-cli",
                    catalog_id,
                    "responses",
                    LivePath::Proxy,
                    kind,
                    body.clone(),
                    account,
                )
                .await?;
                proxy_qualities.push(proxy.stream_quality);
                written.push(
                    write_redacted_artifact(
                        &options.artifact_dir,
                        &format!(
                            "grok-attribution-{}-{:?}-proxy-{index}.json",
                            catalog_id.replace([':', '/'], "_"),
                            kind
                        )
                        .to_ascii_lowercase(),
                        &proxy,
                    )
                    .map_err(|error| AppError::Message(error.to_string()))?,
                );
            }
            let verdict = grok_attribution_verdict(&direct_qualities, &proxy_qualities);
            // Buffered fallback: the same case with `stream: false` decides
            // the catalog action when the provider is at fault.
            let buffered = post_responses_sse(
                session,
                "grok-cli",
                catalog_id,
                "responses",
                LivePath::Proxy,
                kind,
                json!({
                    "model": catalog_id,
                    "store": false,
                    "stream": false,
                    "tool_choice": "none",
                    "tools": [],
                    "input": [{
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": official_live_marker_input("VELLUM_GROK_BUFFERED_OK") }]
                    }]
                }),
                account,
            )
            .await?;
            let buffered_action = if buffered.outcome == LiveOutcome::Ok
                && matches!(buffered.stream_quality, Some(StreamQuality::Buffered))
            {
                "keep_model_with_streaming_false"
            } else {
                "hide_from_active_catalog"
            };
            written.push(
                write_redacted_artifact(
                    &options.artifact_dir,
                    &format!(
                        "grok-attribution-{}-{:?}-buffered.json",
                        catalog_id.replace([':', '/'], "_"),
                        kind
                    )
                    .to_ascii_lowercase(),
                    &buffered,
                )
                .map_err(|error| AppError::Message(error.to_string()))?,
            );
            case_summaries.push(json!({
                "catalog_model": catalog_id,
                "kind": format!("{kind:?}").to_ascii_lowercase(),
                "verdict": verdict,
                "rule": "direct>=2/3 end_flush && proxy>=2/3 end_flush => provider_behavior; direct>=2/3 incremental && proxy>=2/3 end_flush => vellum_regression; both>=2/3 incremental => both_lanes_incremental; direct>=2/3 end_flush && proxy>=2/3 incremental => no_vellum_regression; else inconclusive",
                "direct_qualities": direct_qualities.iter().map(|quality| quality.map(|q| q.as_token())).collect::<Vec<_>>(),
                "proxy_qualities": proxy_qualities.iter().map(|quality| quality.map(|q| q.as_token())).collect::<Vec<_>>(),
                "buffered_action": buffered_action,
                "buffered_quality": buffered.stream_quality.map(|q| q.as_token()),
                "note": "end_flush on both lanes is provider behavior; a Vellum-only end_flush is a regression; inconclusive is never a pass"
            }));
        }
    }
    let summary_path = options.artifact_dir.join("grok-attribution.json");
    std::fs::write(
        &summary_path,
        serde_json::to_string_pretty(&json!({
            "repeats_per_lane": repeats,
            "cases": case_summaries,
        }))
        .unwrap_or_else(|_| "{}".into()),
    )
    .map_err(|error| AppError::Message(format!("write grok attribution: {error}")))?;
    written.push(summary_path);
    Ok(written)
}

fn grok_kind_bodies(model: &str) -> Vec<(LiveKind, Value)> {
    let marker = official_live_marker_input("VELLUM_GROK_OK");
    vec![
        (
            LiveKind::Marker,
            json!({
                "model": model,
                "store": false,
                "stream": true,
                "tool_choice": "none",
                "tools": [],
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": marker }]
                }]
            }),
        ),
        (
            LiveKind::Sse,
            json!({
                "model": model,
                "store": false,
                "stream": true,
                "tool_choice": "none",
                "tools": [],
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "Count 1 2 3, one number per token." }]
                }]
            }),
        ),
        (
            LiveKind::TypedTool,
            json!({
                "model": model,
                "store": false,
                "stream": true,
                "tools": [{
                    "type": "function",
                    "name": "echo_marker",
                    "description": "Echo a marker",
                    "parameters": {
                        "type": "object",
                        "properties": { "text": { "type": "string" } },
                        "required": ["text"]
                    }
                }],
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "Call echo_marker with text=VELLUM_GROK_TOOL." }]
                }]
            }),
        ),
    ]
}

async fn qwen_direct_chat_sse(
    base_url: &str,
    upstream_model: &str,
    secret: &str,
    route_id: &str,
    catalog_id: &str,
    input: &str,
) -> LiveRequestArtifact {
    let mut artifact = new_live_artifact(route_id, catalog_id, "chat", LivePath::Direct, None);
    artifact.kind = Some("sse".into());
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let started = Instant::now();
    eprintln!("live: Qwen direct POST {url} model={upstream_model}");
    let request = match http_client() {
        Ok(client) => client
            .post(&url)
            .header("authorization", format!("Bearer {secret}"))
            .header("content-type", "application/json")
            .json(&json!({
                "model": upstream_model,
                "stream": true,
                "messages": [{ "role": "user", "content": input }]
            })),
        Err(error) => {
            artifact.outcome = LiveOutcome::EnvBlocked;
            artifact.error_category = Some(error.to_string());
            return artifact;
        }
    };
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            artifact.outcome = LiveOutcome::Fail;
            artifact.error_category = Some(error.to_string());
            artifact.completed_ms = Some(started.elapsed().as_millis() as u64);
            return artifact;
        }
    };
    artifact.connection_ms = Some(started.elapsed().as_millis() as u64);
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let detail = match response.text().await {
            Ok(text) => text.chars().take(600).collect::<String>(),
            Err(_) => "(no body)".to_string(),
        };
        artifact.upstream_status = Some(status);
        artifact.diagnostic = Some(detail);
        artifact.outcome = if status == 429 {
            LiveOutcome::Quota
        } else {
            LiveOutcome::Fail
        };
        artifact.error_category = Some(classify_http_status(status).into());
        artifact.completed_ms = Some(started.elapsed().as_millis() as u64);
        return artifact;
    }
    let mut first_frame = None;
    let mut output_delta_count = 0_u64;
    let mut reasoning_delta_count = 0_u64;
    let mut first_output_delta_ms = None;
    let mut first_reasoning_delta_ms = None;
    let mut terminal_ms = None;
    let mut pending = String::new();
    let mut stream = response.bytes_stream();
    loop {
        let next = tokio::time::timeout(Duration::from_secs(60), stream.next()).await;
        let chunk = match next {
            Ok(Some(Ok(bytes))) => bytes,
            Ok(Some(Err(_))) | Err(_) | Ok(None) => break,
        };
        if first_frame.is_none() {
            first_frame = Some(started.elapsed().as_millis() as u64);
        }
        pending.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(index) = pending.find("\n\n") {
            let block = pending[..index].to_string();
            pending = pending[index + 2..].to_string();
            artifact.frame_count += 1;
            if let Some(data) = sse_data(&block) {
                if data.trim() == "[DONE]" {
                    artifact.outcome = LiveOutcome::Ok;
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<Value>(&data) {
                    let content = value
                        .pointer("/choices/0/delta/content")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if !content.is_empty() {
                        output_delta_count += 1;
                        first_output_delta_ms.get_or_insert(started.elapsed().as_millis() as u64);
                    }
                    let reasoning = value
                        .pointer("/choices/0/delta/reasoning_content")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if !reasoning.is_empty() {
                        reasoning_delta_count += 1;
                        first_reasoning_delta_ms
                            .get_or_insert(started.elapsed().as_millis() as u64);
                    }
                    if value
                        .pointer("/choices/0/finish_reason")
                        .and_then(Value::as_str)
                        .is_some()
                    {
                        artifact.outcome = LiveOutcome::Ok;
                        terminal_ms = Some(started.elapsed().as_millis() as u64);
                    }
                }
            }
        }
        if artifact.outcome == LiveOutcome::Ok {
            break;
        }
    }
    artifact.downstream_first_frame_ms = first_frame;
    artifact.upstream_first_frame_ms = first_frame;
    artifact.completed_ms = Some(started.elapsed().as_millis() as u64);
    artifact.output_delta_count = output_delta_count;
    artifact.reasoning_delta_count = reasoning_delta_count;
    artifact.first_output_delta_ms = first_output_delta_ms;
    artifact.first_reasoning_delta_ms = first_reasoning_delta_ms;
    let quality = classify_stream_quality(&StreamQualityInput {
        streaming_requested: true,
        capability_streaming: true,
        streamed: true,
        output_delta_count,
        first_output_delta_ms,
        tool_delta_count: 0,
        first_tool_delta_ms: None,
        terminal_ms: terminal_ms.or(artifact.completed_ms),
        reasoning_delta_count,
        first_reasoning_delta_ms,
    });
    artifact.stream_quality = Some(quality);
    if artifact.outcome == LiveOutcome::Ok {
        // Third-party Chat: a legitimate `end_flush` keeps the transport
        // outcome; `no_delta` on expected text is a protocol failure.
        match quality {
            StreamQuality::Incremental | StreamQuality::EndFlush => {}
            StreamQuality::NoDelta => {
                artifact.outcome = LiveOutcome::Fail;
                artifact.error_category = Some("no_stream_delta".into());
            }
            StreamQuality::Buffered => {
                artifact.outcome = LiveOutcome::Ok;
            }
        }
    } else if artifact.outcome != LiveOutcome::Quota {
        artifact.outcome = LiveOutcome::Fail;
        artifact
            .error_category
            .get_or_insert_with(|| "no_terminal".into());
    }
    artifact
}

fn prefer_qwen_38(ids: &[String]) -> Option<String> {
    ids.iter()
        .find(|id| looks_like_qwen_38(id))
        .cloned()
        .or_else(|| std::env::var("VELLUM_LIVE_QWEN_MODEL").ok())
}

/// Prompt for the Brave round-trip model turn. Deliberately generic: the
/// point is the local `web_search` wrapper -> Brave backend path, not the
/// answer content.
const BRAVE_ROUNDTRIP_PROMPT: &str =
    "Use the web_search tool to search the web for the most recent release \
     notes of the Rust programming language, then report the first result \
     URL you find.";

/// Extract the `query` string from an accumulated `function_call_arguments`
/// payload if present.
fn function_call_query(accumulated: &str) -> Option<String> {
    serde_json::from_str::<Value>(accumulated)
        .ok()?
        .get("query")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
}

/// A single bounded Brave text result, reduced to the public facts the
/// terminal turn is allowed to feed back: title, URL, snippet. Never the
/// query, never a key, never the encrypted output.
struct BraveSearchHit {
    url: String,
    title: Option<String>,
    snippet: Option<String>,
}

/// Cap on each rendered result field so a hostile or verbose provider snippet
/// cannot bloat the tool output handed back to the model.
const BRAVE_RESULT_FIELD_MAX_CHARS: usize = 400;

/// Collect bounded `text_result` entries from a `/v1/alpha/search` response.
/// `image_result` and other kinds are ignored; entries without a usable URL
/// are dropped.
fn brave_text_results(value: &Value) -> Vec<BraveSearchHit> {
    value
        .pointer("/results")
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .filter(|result| result.get("type").and_then(Value::as_str) == Some("text_result"))
                .filter_map(|result| {
                    let url = result
                        .get("url")
                        .and_then(Value::as_str)?
                        .trim()
                        .to_string();
                    if url.is_empty() {
                        return None;
                    }
                    Some(BraveSearchHit {
                        url,
                        title: result
                            .get("title")
                            .and_then(Value::as_str)
                            .map(|text| bounded_text(text, BRAVE_RESULT_FIELD_MAX_CHARS)),
                        snippet: result
                            .get("snippet")
                            .and_then(Value::as_str)
                            .map(|text| bounded_text(text, BRAVE_RESULT_FIELD_MAX_CHARS)),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Render the bounded results as the `function_call_output` text handed back
/// to the model. Every URL is present verbatim so a citation match can be
/// proven; the whole string is bounded to the terminal-answer cap.
fn render_brave_results(hits: &[BraveSearchHit]) -> String {
    let mut rendered = String::new();
    for (index, hit) in hits.iter().enumerate() {
        rendered.push_str(&format!(
            "{}. {}\n",
            index + 1,
            hit.title.as_deref().unwrap_or("(untitled)")
        ));
        rendered.push_str(&format!("   {}\n", hit.url));
        if let Some(snippet) = hit.snippet.as_deref().filter(|text| !text.is_empty()) {
            rendered.push_str(&format!("   {snippet}\n"));
        }
    }
    bounded_text(&rendered, TERMINAL_ANSWER_MAX_CHARS)
}

/// Does the terminal answer cite at least one URL actually returned by Brave?
/// Proves the model used the fed-back result rather than hallucinating.
fn terminal_cites_any_url(answer: &str, urls: &[String]) -> bool {
    urls.iter().any(|url| answer.contains(url.as_str()))
}

/// Cap on the terminal-answer text stored in an artifact. Bounded so a
/// verbose model reply can never bloat the artifact or smuggle surplus
/// provider text out of the round-trip.
const TERMINAL_ANSWER_MAX_CHARS: usize = 2_000;

/// Instruction for the second Brave turn: given the search output fed back
/// through `function_call_output`, produce a short terminal answer that cites
/// the first result URL. Deliberately never names the original query.
const BRAVE_TERMINAL_PROMPT: &str = "Using only the search result returned \
     above, give a short terminal answer to the original request and cite the \
     first result URL.";

/// The `call_id` to echo back in the follow-up `function_call_output`, taken
/// from the first turn's `web_search` call. Prefers the explicit
/// `item.call_id`, then `item.id`, then the event-level `item_id`.
fn function_call_call_id(value: &Value) -> Option<String> {
    value
        .get("item")
        .and_then(|item| item.get("call_id"))
        .or_else(|| value.get("item").and_then(|item| item.get("id")))
        .or_else(|| value.get("item_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|id| !id.is_empty())
}

/// The tool name of an `output_item` function call, if that item is a function
/// call. Used to require a genuine `web_search` call in the first turn rather
/// than trusting any tool the model happened to invoke.
fn function_call_tool_name(value: &Value) -> Option<String> {
    let item = value.get("item")?;
    match item.get("type").and_then(Value::as_str) {
        Some("web_search_call") => Some("web_search_call".into()),
        Some("function_call") => item
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|name| !name.is_empty()),
        _ => None,
    }
}

/// The `output_text.delta` payload of a Responses-wire event, if any. Only the
/// application text channel qualifies; reasoning and tool-arguments never do.
fn sse_output_text_delta(value: &Value) -> Option<&str> {
    value
        .get("type")
        .and_then(Value::as_str)
        .filter(|kind| kind.contains("output_text.delta"))
        .and_then(|_| value.get("delta").and_then(Value::as_str))
}

/// Assemble a bounded terminal answer from raw Responses SSE text, keeping
/// only `response.output_text.delta` payloads. Tool-arguments deltas and
/// reasoning are never treated as the answer. Pure and unit-testable.
fn terminal_answer_from_sse(raw: &str) -> String {
    let mut answer = String::new();
    for block in raw.split("\n\n") {
        let Some(data) = sse_data(block) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        if let Some(text) = sse_output_text_delta(&value) {
            answer.push_str(text);
            if answer.chars().count() >= TERMINAL_ANSWER_MAX_CHARS {
                break;
            }
        }
    }
    bounded_text(&answer, TERMINAL_ANSWER_MAX_CHARS)
}

/// The first Brave turn's observed result: transport status, the `web_search`
/// function call the model emitted (tool name, call id, full arguments, parsed
/// query), and which terminal event closed the turn. `terminal` is `None` when
/// the turn timed out or dropped before any terminal.
struct BraveModelTurn {
    elapsed_ms: u64,
    status: u16,
    tool_name: Option<String>,
    call_id: Option<String>,
    arguments: Option<String>,
    query: Option<String>,
    terminal: Option<String>,
    answer: Option<String>,
}

/// One model turn that should emit a `web_search` function call, run through
/// the Desktop router exactly like production. The turn is only usable when
/// *all* of the following are true: HTTP 2xx, a genuine `web_search` function
/// call with call id and complete arguments, and a normal terminal event.
async fn brave_model_turn(session: &LiveSession, body: &Value) -> AppResult<BraveModelTurn> {
    let started = Instant::now();
    let response = http_client()?
        .post(format!("http://{}/v1/responses", session.addr))
        .header(BOUNDARY_KEY_HEADER, &session.boundary)
        .header("x-request-id", format!("brave-turn-{}", ulid::Ulid::new()))
        .json(body)
        .send()
        .await
        .map_err(|error| AppError::Message(format!("Brave model turn: {error}")))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Ok(BraveModelTurn {
            elapsed_ms: started.elapsed().as_millis() as u64,
            status,
            tool_name: None,
            call_id: None,
            arguments: None,
            query: None,
            terminal: None,
            answer: None,
        });
    }
    let mut accumulated = String::new();
    let mut pending = String::new();
    let mut stream = response.bytes_stream();
    let first_frame_budget = live_first_frame_budget();
    let headers_at = Instant::now();
    let mut finished = false;
    let mut terminal = None;
    let mut query = None;
    let mut call_id = None;
    let mut tool_name = None;
    let mut raw_sse = String::new();
    loop {
        if !finished && headers_at.elapsed() > first_frame_budget && pending.is_empty() {
            break;
        }
        let chunk_budget = if pending.is_empty() && terminal.is_none() {
            first_frame_budget.saturating_sub(headers_at.elapsed())
        } else {
            Duration::from_secs(30)
        };
        let next = tokio::time::timeout(chunk_budget, stream.next()).await;
        let chunk = match next {
            Ok(Some(Ok(bytes))) => bytes,
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => break,
        };
        pending.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(index) = pending.find("\n\n") {
            let block = pending[..index].to_string();
            pending = pending[index + 2..].to_string();
            raw_sse.push_str(&block);
            raw_sse.push_str("\n\n");
            let Some(data) = sse_data(&block) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
            if kind == "response.function_call_arguments.delta" {
                if let Some(delta) = value
                    .get("delta")
                    .or_else(|| value.get("arguments"))
                    .and_then(Value::as_str)
                {
                    accumulated.push_str(delta);
                }
            } else if kind == "response.function_call_arguments.done" {
                if let Some(arguments) = value.get("arguments").and_then(Value::as_str) {
                    accumulated = arguments.to_string();
                }
                if let Some(found) = function_call_query(&accumulated) {
                    query = Some(found);
                }
                if call_id.is_none() {
                    call_id = function_call_call_id(&value);
                }
            } else if kind == "response.output_item.added" || kind == "response.output_item.done" {
                if tool_name.is_none() {
                    tool_name = function_call_tool_name(&value);
                }
                if query.is_none() {
                    query = value
                        .pointer("/item/action/query")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .map(str::to_string);
                }
            } else if kind == "response.completed" || kind == "response.failed" {
                finished = true;
                terminal = Some(kind.to_string());
            }
            if finished {
                break;
            }
        }
        if finished {
            break;
        }
    }
    let answer = terminal_answer_from_sse(&raw_sse);
    Ok(BraveModelTurn {
        elapsed_ms: started.elapsed().as_millis() as u64,
        status,
        tool_name,
        call_id,
        arguments: (!accumulated.is_empty()).then_some(accumulated),
        query,
        terminal,
        answer: (!answer.is_empty()).then_some(answer),
    })
}

/// Terminal turn request body: replay the assistant `function_call` verbatim,
/// then feed the bounded search results back through a `function_call_output`
/// reusing the same `call_id`, then ask for a bounded terminal answer that
/// cites a returned URL. Pure and unit-testable; the original query is never
/// echoed back, only the public result facts (title/URL/snippet).
fn brave_terminal_body(
    model: &str,
    tool_name: &str,
    call_id: &str,
    arguments: &str,
    output: &str,
) -> Value {
    json!({
        "model": model,
        "store": false,
        "stream": true,
        "input": [
            {
                "type": "function_call",
                "call_id": call_id,
                "name": tool_name,
                "arguments": arguments
            },
            { "type": "function_call_output", "call_id": call_id, "output": output },
            {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": BRAVE_TERMINAL_PROMPT }]
            }
        ]
    })
}

/// The second Brave turn's observed result: transport status, the streamed
/// terminal answer (empty/None when none arrived), and the terminal event that
/// closed the turn (`None` on timeout/early drop).
struct BraveTerminalTurn {
    status: u16,
    answer: Option<String>,
    terminal: Option<String>,
}

/// Second Brave turn: replay the assistant `function_call`, feed the bounded
/// search results back through `function_call_output` (reusing the first
/// turn's `web_search` call id), and ask for a bounded terminal text answer
/// that cites a returned URL. Returns elapsed time, the streamed answer, and
/// the terminal event (`response.completed`/`response.failed`).
async fn brave_terminal_turn(
    session: &LiveSession,
    model: &str,
    tool_name: &str,
    call_id: &str,
    arguments: &str,
    output: &str,
) -> AppResult<BraveTerminalTurn> {
    let body = brave_terminal_body(model, tool_name, call_id, arguments, output);
    let response = http_client()?
        .post(format!("http://{}/v1/responses", session.addr))
        .header(BOUNDARY_KEY_HEADER, &session.boundary)
        .header(
            "x-request-id",
            format!("brave-terminal-{}", ulid::Ulid::new()),
        )
        .json(&body)
        .send()
        .await
        .map_err(|error| AppError::Message(format!("Brave terminal turn: {error}")))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Ok(BraveTerminalTurn {
            status,
            answer: None,
            terminal: None,
        });
    }
    let mut pending = String::new();
    let mut stream = response.bytes_stream();
    let first_frame_budget = live_first_frame_budget();
    let headers_at = Instant::now();
    let mut finished = false;
    let mut terminal = None;
    let mut saw_output = false;
    let mut raw = String::new();
    loop {
        if !finished && headers_at.elapsed() > first_frame_budget && pending.is_empty() {
            break;
        }
        let chunk_budget = if pending.is_empty() && !saw_output {
            first_frame_budget.saturating_sub(headers_at.elapsed())
        } else {
            Duration::from_secs(30)
        };
        let next = tokio::time::timeout(chunk_budget, stream.next()).await;
        let chunk = match next {
            Ok(Some(Ok(bytes))) => bytes,
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => break,
        };
        pending.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(index) = pending.find("\n\n") {
            let block = pending[..index].to_string();
            pending = pending[index + 2..].to_string();
            raw.push_str(&block);
            raw.push_str("\n\n");
            let Some(data) = sse_data(&block) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
            if kind == "response.completed" || kind == "response.failed" {
                finished = true;
                terminal = Some(kind.to_string());
            }
            if sse_output_text_delta(&value).is_some() {
                saw_output = true;
            }
        }
        if finished {
            break;
        }
    }
    // Only a genuinely streamed text answer is recorded; a tool-only or empty
    // reply yields None so the artifact never fabricates one.
    let answer = if saw_output {
        let text = terminal_answer_from_sse(&raw);
        (!text.is_empty()).then_some(text)
    } else {
        None
    };
    Ok(BraveTerminalTurn {
        status,
        answer,
        terminal,
    })
}

/// Brave-specific facts attached to one round-trip artifact, kept separate
/// from the generic `LiveRequestArtifact` transport fields. Never records the
/// query, the encrypted output, or any key.
#[derive(Default)]
struct BraveRoundtripRecord {
    search_result_count: Option<u64>,
    first_result_url: Option<String>,
    terminal_answer: Option<String>,
    terminal_citation_matched: Option<bool>,
}

/// Write one Brave round-trip artifact with its Brave-specific facts, all
/// bounded and redacted. The query and any provider key are never recorded:
/// the terminal text and artifact go through the shared redaction.
fn write_brave_roundtrip_artifact(
    artifact_dir: &Path,
    model: &str,
    artifact: &LiveRequestArtifact,
    record: &BraveRoundtripRecord,
) -> AppResult<PathBuf> {
    let name = format!("brave-roundtrip-{}.json", model.replace([':', '/'], "_"));
    let mut value = live_artifact_json(artifact)?;
    if let Some(count) = record.search_result_count {
        value["search_result_count"] = Value::from(count);
    }
    if let Some(url) = record
        .first_result_url
        .as_deref()
        .filter(|url| !url.is_empty())
    {
        value["first_result_url"] = Value::String(bounded_text(url, TERMINAL_ANSWER_MAX_CHARS));
    }
    if let Some(answer) = record
        .terminal_answer
        .as_deref()
        .filter(|text| !text.is_empty())
    {
        value["terminal_answer"] = Value::String(bounded_text(answer, TERMINAL_ANSWER_MAX_CHARS));
    }
    if let Some(matched) = record.terminal_citation_matched {
        value["terminal_citation_matched"] = Value::Bool(matched);
    }
    let redacted = redact_live_artifact(&value);
    let path = artifact_dir.join(&name);
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&redacted).unwrap_or_else(|_| "{}".into()),
    )
    .map_err(|error| AppError::Message(format!("write {}: {error}", path.display())))?;
    Ok(path)
}

/// One real Brave round-trip through the Desktop router: a third-party model
/// turn that must emit a genuine `web_search` function call (call id + full
/// arguments), the query is executed via `/v1/alpha/search` (DesktopSearch ->
/// Brave backend), and the bounded results are fed back in a second turn that
/// replays the assistant `function_call` and its `function_call_output` before
/// asking for a terminal answer that cites a returned URL.
///
/// Success requires `response.completed`, a non-empty final text, and at least
/// one returned URL actually cited. A model that never calls `web_search` is a
/// `model_did_not_call_search` failure — never silently replaced with a canned
/// query. Every other failure (non-2xx, `response.failed`, missing call id,
/// empty results, empty answer, no citation) records its own error category,
/// HTTP status, and bounded diagnostic. The artifact records only public
/// facts: result count, first result URL, bounded redacted terminal answer,
/// citation match, timing, and outcome — never the query or any key.
async fn run_brave_roundtrip(
    session: &LiveSession,
    options: &LiveRunOptions,
) -> AppResult<Vec<PathBuf>> {
    let wrapper_enabled = session.state.web_search_settings().enabled
        && crate::credentials::load(
            &session.state.data_root(),
            crate::web_search::BRAVE_SEARCH_CREDENTIAL_ID,
        )
        .ok()
        .flatten()
        .is_some();
    if !wrapper_enabled {
        let path = write_env_failure(
            &options.artifact_dir,
            "brave",
            "web_search wrapper disabled (settings.enabled=false or no Brave credential)",
        );
        return Ok(vec![path]);
    }
    let routes = session.desktop.active_model_routes();
    let model = match std::env::var("VELLUM_LIVE_BRAVE_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        Some(override_model) => override_model,
        None => {
            let mut base: Vec<_> = routes
                .iter()
                .filter(|route| route.route_id == "grok-cli")
                .collect();
            base.sort_by_key(|route| {
                let newer =
                    route.catalog_id.contains("4.6") || route.upstream_model.contains("4.6");
                std::cmp::Reverse(newer)
            });
            base.first()
                .map(|route| route.catalog_id.clone())
                .ok_or_else(|| {
                    AppError::Message(
                        "no enabled Grok catalog route for the brave round-trip".into(),
                    )
                })?
        }
    };
    let mut artifact = new_live_artifact("grok-cli", &model, "responses", LivePath::Proxy, None);
    let started = Instant::now();
    let turn = brave_model_turn(
        session,
        &json!({
            "model": model,
            "store": false,
            "stream": true,
            "tools": [{
                "type": "function",
                "name": "web_search",
                "description": "Search the web and return ranked results. Read-only.",
                "parameters": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"],
                    "additionalProperties": false
                }
            }],
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": BRAVE_ROUNDTRIP_PROMPT}]
            }]
        }),
    )
    .await?;
    artifact.connection_ms = Some(turn.elapsed_ms);

    // Shared terminal writer: stamps the end-to-end time and writes the (bounded,
    // redacted) artifact once, so no early-return path forgets a field.
    let finish = |artifact: &mut LiveRequestArtifact, record: &BraveRoundtripRecord| {
        artifact.completed_ms = Some(started.elapsed().as_millis() as u64);
        Ok(vec![write_brave_roundtrip_artifact(
            &options.artifact_dir,
            &model,
            artifact,
            record,
        )?])
    };

    // Transport failure: never a fallback query, never a pass.
    if !(200..300).contains(&turn.status) {
        artifact.outcome = LiveOutcome::Fail;
        artifact.upstream_status = Some(turn.status);
        artifact.error_category = Some(classify_http_status(turn.status).into());
        artifact.diagnostic = Some(format!("model turn HTTP {}", turn.status));
        return finish(&mut artifact, &BraveRoundtripRecord::default());
    }
    if turn.terminal.as_deref() == Some("response.failed") {
        artifact.outcome = LiveOutcome::Fail;
        artifact.error_category = Some("response_failed".into());
        artifact.diagnostic = Some("model turn ended in response.failed".into());
        return finish(&mut artifact, &BraveRoundtripRecord::default());
    }
    if turn.terminal.is_none() {
        artifact.outcome = LiveOutcome::Timeout;
        artifact.error_category = Some("model_turn_timeout".into());
        artifact.diagnostic = Some("model turn produced no terminal event".into());
        return finish(&mut artifact, &BraveRoundtripRecord::default());
    }
    artifact.kind = Some("web_search".into());
    // Server-side loop: Codex sees `web_search_call` and a terminal answer on
    // the same turn. A leaked `function_call{name: web_search}` is the old
    // defect (Codex cannot execute it) and must not be completed by the
    // harness calling `/v1/alpha/search`.
    if turn.tool_name.as_deref() == Some("web_search_call")
        && turn.terminal.as_deref() == Some("response.completed")
    {
        let Some(answer) = turn.answer.clone().filter(|text| !text.trim().is_empty()) else {
            artifact.outcome = LiveOutcome::Fail;
            artifact.error_category = Some("empty_terminal_answer".into());
            return finish(&mut artifact, &BraveRoundtripRecord::default());
        };
        let matched = answer.contains("http://") || answer.contains("https://");
        let record = BraveRoundtripRecord {
            terminal_answer: Some(answer),
            terminal_citation_matched: Some(matched),
            ..BraveRoundtripRecord::default()
        };
        if !matched {
            artifact.outcome = LiveOutcome::Fail;
            artifact.error_category = Some("terminal_no_citation".into());
            artifact.diagnostic = Some("terminal answer cited no URL".into());
            return finish(&mut artifact, &record);
        }
        artifact.outcome = LiveOutcome::Ok;
        return finish(&mut artifact, &record);
    }
    if turn.tool_name.as_deref() == Some("web_search") {
        artifact.outcome = LiveOutcome::Fail;
        artifact.error_category = Some("search_not_executed".into());
        artifact.diagnostic = Some(
            "model emitted function_call web_search; Vellum must intercept it as web_search_call"
                .into(),
        );
        return finish(&mut artifact, &BraveRoundtripRecord::default());
    }
    if turn.tool_name.as_deref() != Some("web_search_call") {
        artifact.outcome = LiveOutcome::Fail;
        artifact.error_category = Some("model_did_not_call_search".into());
        artifact.diagnostic = Some("model completed the turn without a web_search_call".into());
        return finish(&mut artifact, &BraveRoundtripRecord::default());
    }
    let (Some(call_id), Some(arguments), Some(query)) = (
        turn.call_id.clone(),
        turn.arguments.clone(),
        turn.query.clone(),
    ) else {
        artifact.kind = Some("web_search".into());
        artifact.outcome = LiveOutcome::Fail;
        artifact.error_category = Some("missing_call_id".into());
        artifact.diagnostic =
            Some("web_search call missing its call id or complete arguments".into());
        return finish(&mut artifact, &BraveRoundtripRecord::default());
    };
    artifact.kind = Some("web_search".into());

    let search_body = json!({
        "model": model,
        "commands": { "search_query": [{ "q": query }] }
    });
    let response = http_client()?
        .post(format!("http://{}/v1/alpha/search", session.addr))
        .header(BOUNDARY_KEY_HEADER, &session.boundary)
        .json(&search_body)
        .send()
        .await
        .map_err(|error| AppError::Message(format!("Brave search hop: {error}")))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let detail = match response.text().await {
            Ok(text) => bounded_text(&text, 600),
            Err(_) => "(no body)".to_string(),
        };
        eprintln!("live: Brave search hop rejected status={status}: {detail}");
        artifact.outcome = LiveOutcome::Fail;
        artifact.upstream_status = Some(status);
        artifact.error_category = if status == 503 {
            Some("search_disabled".into())
        } else {
            Some(classify_http_status(status).into())
        };
        artifact.diagnostic = Some(detail);
        return finish(&mut artifact, &BraveRoundtripRecord::default());
    }
    let hits = match response.json::<Value>().await {
        Ok(value) => brave_text_results(&value),
        Err(error) => {
            artifact.outcome = LiveOutcome::Fail;
            artifact.error_category = Some(format!("search_response_parse: {error}"));
            return finish(&mut artifact, &BraveRoundtripRecord::default());
        }
    };
    let mut record = BraveRoundtripRecord {
        search_result_count: Some(hits.len() as u64),
        first_result_url: hits.first().map(|hit| hit.url.clone()),
        ..BraveRoundtripRecord::default()
    };
    if hits.is_empty() {
        artifact.outcome = LiveOutcome::Fail;
        artifact.error_category = Some("empty_search_results".into());
        return finish(&mut artifact, &record);
    }

    // Second turn: replay the assistant `function_call`, feed the bounded
    // results back through `function_call_output` on the same call id, and
    // require a terminal answer that cites an actual returned URL.
    let output = render_brave_results(&hits);
    let urls: Vec<String> = hits.iter().map(|hit| hit.url.clone()).collect();
    let terminal =
        brave_terminal_turn(session, &model, "web_search", &call_id, &arguments, &output).await?;
    if !(200..300).contains(&terminal.status) {
        artifact.outcome = LiveOutcome::Fail;
        artifact.upstream_status = Some(terminal.status);
        artifact.error_category = Some(classify_http_status(terminal.status).into());
        artifact.diagnostic = Some(format!("terminal turn HTTP {}", terminal.status));
        return finish(&mut artifact, &record);
    }
    if terminal.terminal.as_deref() == Some("response.failed") {
        artifact.outcome = LiveOutcome::Fail;
        artifact.error_category = Some("terminal_response_failed".into());
        return finish(&mut artifact, &record);
    }
    if terminal.terminal.is_none() {
        artifact.outcome = LiveOutcome::Timeout;
        artifact.error_category = Some("terminal_turn_timeout".into());
        return finish(&mut artifact, &record);
    }
    let Some(answer) = terminal.answer.clone() else {
        artifact.outcome = LiveOutcome::Fail;
        artifact.error_category = Some("empty_terminal_answer".into());
        return finish(&mut artifact, &record);
    };
    record.terminal_answer = Some(answer.clone());
    let matched = terminal_cites_any_url(&answer, &urls);
    record.terminal_citation_matched = Some(matched);
    if !matched {
        artifact.outcome = LiveOutcome::Fail;
        artifact.error_category = Some("terminal_no_citation".into());
        artifact.diagnostic = Some("terminal answer cited none of the returned URLs".into());
        return finish(&mut artifact, &record);
    }
    artifact.outcome = LiveOutcome::Ok;
    finish(&mut artifact, &record)
}

async fn run_qwen(session: &LiveSession, options: &LiveRunOptions) -> AppResult<Vec<PathBuf>> {
    let models = list_models(session).await?;
    let ids = catalog_ids(&models);
    let desktop_qwen: Vec<String> = session
        .desktop
        .active_model_routes()
        .into_iter()
        .filter(|route| {
            looks_like_qwen_38(&route.catalog_id)
                || looks_like_qwen_38(&route.upstream_model)
                || looks_like_qwen_36(&route.catalog_id)
                || looks_like_qwen_36(&route.upstream_model)
        })
        .map(|route| route.catalog_id)
        .collect();
    let requested = std::env::var("VELLUM_LIVE_QWEN_MODEL")
        .ok()
        .or_else(|| prefer_qwen_38(&ids))
        .or_else(|| prefer_qwen_38(&desktop_qwen))
        .ok_or_else(|| AppError::Message("no Qwen model is selected or advertised".into()))?;
    if looks_like_qwen_38(&requested) {
        refuse_qwen_standin(&requested, &requested)
            .map_err(|error| AppError::Message(error.to_string()))?;
    }
    if !ids.is_empty() {
        if let Err(error) = select_qwen_from_models(&requested, &ids) {
            write_env_failure(&options.artifact_dir, "qwen", &error.to_string());
            return Err(AppError::Message(error.to_string()));
        }
    }
    let selected = requested.clone();
    eprintln!("live: Qwen selected catalog_id={selected}");
    if looks_like_qwen_38(&requested) && looks_like_qwen_36(&selected) {
        return Err(AppError::Message(
            "qwen3.6 must not stand in for Qwen 3.8".into(),
        ));
    }
    let view = session
        .desktop
        .active_model_routes()
        .into_iter()
        .find(|route| route.catalog_id == selected || route.upstream_model == selected)
        .ok_or_else(|| AppError::Message(format!("selected Qwen {selected} has no route")))?;
    let desktop_route = session
        .state
        .routes()
        .into_iter()
        .find(|route| route.id == view.route_id)
        .ok_or_else(|| AppError::Message(format!("Qwen route {} missing", view.route_id)))?;
    let secret = session
        .desktop
        .credentials()
        .await
        .get_secret(&desktop_route.id)
        .await
        .map_err(AppError::Message)?
        .ok_or_else(|| AppError::Message(format!("no credential for {}", desktop_route.id)))?;
    let mut written = Vec::new();
    let input = official_live_marker_input("VELLUM_QWEN_OK");
    let mut direct_ttfts = Vec::new();
    let mut proxy_ttfts = Vec::new();
    let ab_turns = std::env::var("VELLUM_LIVE_AB_TURNS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(3)
        .max(1);
    for path in [LivePath::Direct, LivePath::Proxy] {
        for turn in 0..ab_turns {
            let marker = format!("{input} turn-{turn}");
            eprintln!("live: Qwen {:?} turn {turn}", path);
            let artifact = match path {
                LivePath::Proxy => {
                    post_responses_sse(
                        session,
                        &view.route_id,
                        &selected,
                        "responses",
                        path,
                        LiveKind::Sse,
                        json!({
                            "model": selected,
                            "store": false,
                            "stream": true,
                            "tool_choice": "none",
                            "tools": [],
                            "input": [{
                                "type": "message",
                                "role": "user",
                                "content": [{ "type": "input_text", "text": marker }]
                            }]
                        }),
                        None,
                    )
                    .await?
                }
                LivePath::Direct => {
                    qwen_direct_chat_sse(
                        &desktop_route.base_url,
                        &view.upstream_model,
                        &secret,
                        &view.route_id,
                        &selected,
                        &marker,
                    )
                    .await
                }
            };
            if path == LivePath::Direct {
                direct_ttfts.extend(artifact.downstream_first_frame_ms);
            } else {
                proxy_ttfts.extend(artifact.downstream_first_frame_ms);
            }
            written.push(
                write_redacted_artifact(
                    &options.artifact_dir,
                    &format!(
                        "qwen-{}-{}-{turn}.json",
                        selected.replace([':', '/'], "_"),
                        match path {
                            LivePath::Direct => "direct",
                            LivePath::Proxy => "proxy",
                        }
                    ),
                    &artifact,
                )
                .map_err(|error| AppError::Message(error.to_string()))?,
            );
        }
    }
    // Plan §3/C Qwen gate: median first-frame as TTFT, direct vs proxy, with
    // the proxy budget max(250 ms, direct x 20%). The median, not the min,
    // is the fair warm measurement.
    if !direct_ttfts.is_empty() && !proxy_ttfts.is_empty() {
        let direct = median_u64(&direct_ttfts);
        let proxy = median_u64(&proxy_ttfts);
        let verdict = classify_proxy_delay(direct, proxy, Some(direct), Some(proxy));
        let summary = json!({
            "direct_ttft_median_ms": direct,
            "proxy_ttft_median_ms": proxy,
            "direct_first_frames_ms": direct_ttfts,
            "proxy_first_frames_ms": proxy_ttfts,
            "extra_delay_budget_ms": extra_delay_budget_ms(direct),
            "verdict": verdict,
            "catalog_model": selected,
            "note": if matches!(verdict, vellum_proxy_runtime::TimingVerdict::WithinBudget) {
                "proxy extra delay within budget"
            } else {
                "see verdict"
            }
        });
        let path = options.artifact_dir.join("qwen-ttft.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&summary).unwrap_or_else(|_| "{}".into()),
        )
        .ok();
        written.push(path);
    }
    let continuation = post_responses_sse(
        session,
        &view.route_id,
        &selected,
        "responses",
        LivePath::Proxy,
        LiveKind::Continuation,
        json!({
            "model": selected,
            "store": false,
            "stream": true,
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "Remember token=VELLUM_QWEN_CONT" }]
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_live_qwen",
                    "output": "tool-output-ok"
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "Reply with exactly VELLUM_QWEN_CONT and nothing else." }]
                }
            ]
        }),
        None,
    )
    .await?;
    written.push(
        write_redacted_artifact(
            &options.artifact_dir,
            "qwen-continuation.json",
            &continuation,
        )
        .map_err(|error| AppError::Message(error.to_string()))?,
    );
    Ok(written)
}

fn select_opencode_caps(caps: &[OpenCodeModelCaps], max_models: usize) -> Vec<OpenCodeModelCaps> {
    let mut selected = Vec::new();
    if let Some(streaming) = caps.iter().find(|cap| cap.streaming) {
        selected.push(streaming.clone());
    }
    if let Some(buffered) = caps.iter().find(|cap| !cap.streaming) {
        if !selected
            .iter()
            .any(|cap| cap.catalog_id == buffered.catalog_id)
        {
            selected.push(buffered.clone());
        }
    }
    for cap in caps {
        if selected.len() >= max_models {
            break;
        }
        if !selected
            .iter()
            .any(|seen| seen.catalog_id == cap.catalog_id)
        {
            selected.push(cap.clone());
        }
    }
    selected.truncate(max_models);
    selected
}

fn is_opencode_route(route: &crate::model::Route) -> bool {
    route.name.to_ascii_lowercase().contains("opencode") || route.base_url.contains("opencode.ai")
}

/// Model-level catalog action from the per-kind live evidence table (each
/// entry already includes single retries: `provider_unavailable` retried once
/// after cooldown, `no_delta` retried buffered). Never rewrites a wire
/// capability: it only records what the catalog should advertise.
fn opencode_model_action(kinds: &[Value]) -> (String, String) {
    if kinds.is_empty() {
        return (
            "hide_from_active_catalog".to_string(),
            "no kind produced evidence".to_string(),
        );
    }
    let ok_kinds = kinds
        .iter()
        .filter(|result| result["outcome"].as_str() == Some("Ok"))
        .count();
    let any_incremental = kinds.iter().any(|result| {
        result["quality"]
            .as_str()
            .is_some_and(|quality| quality == "incremental")
    });
    let failing = kinds
        .iter()
        .filter(|result| result["outcome"].as_str() != Some("Ok"))
        .map(|result| {
            format!(
                "{}={}",
                result["kind"].as_str().unwrap_or_default(),
                result["outcome"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    if ok_kinds == kinds.len() {
        if any_incremental {
            (
                "keep_model_streaming".to_string(),
                "all kinds ok with an incremental stream".to_string(),
            )
        } else {
            (
                "keep_model_with_streaming_false".to_string(),
                "all kinds ok but no incremental stream".to_string(),
            )
        }
    } else if failing.is_empty() {
        (
            "hide_from_active_catalog".to_string(),
            "no kind succeeded".to_string(),
        )
    } else {
        (
            "hide_from_active_catalog".to_string(),
            format!("kinds failed: {failing}"),
        )
    }
}

/// Cooldown before the single `provider_unavailable` retry, milliseconds.
fn retry_cooldown_ms() -> u64 {
    std::env::var("VELLUM_LIVE_RETRY_COOLDOWN_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(20_000)
        .clamp(500, 5 * 60 * 1000)
}

async fn run_opencode(session: &LiveSession, options: &LiveRunOptions) -> AppResult<Vec<PathBuf>> {
    let models = list_models(session).await?;
    let ids = catalog_ids(&models);
    let routes = session.state.routes();
    let model_routes = session.state.model_routes();
    let mut caps = Vec::new();
    for route in routes
        .iter()
        .filter(|route| route.enabled && is_opencode_route(route))
    {
        let selected = route
            .selected_models
            .clone()
            .unwrap_or_else(|| route.models.clone());
        for upstream in selected {
            let catalog_id = model_routes
                .iter()
                .find(|model| model.route_id == route.id && model.upstream_model == upstream)
                .map(|model| model.catalog_id.clone())
                .unwrap_or_else(|| upstream.clone());
            if !ids.is_empty() && !ids.iter().any(|id| id == &catalog_id || id == &upstream) {
                continue;
            }
            let capability = route
                .model_capabilities
                .iter()
                .find(|capability| capability.model == upstream);
            let wire = capability
                .and_then(|capability| capability.wire)
                .unwrap_or(route.wire);
            let streaming = capability
                .and_then(|capability| capability.streaming)
                .unwrap_or(route.streaming);
            let tools = capability
                .and_then(|capability| capability.tool_calling)
                .unwrap_or(false);
            caps.push(OpenCodeModelCaps {
                catalog_id,
                wire: match wire {
                    WireFormat::Chat => "chat".into(),
                    WireFormat::Responses => "responses".into(),
                },
                streaming,
                tools,
                enabled: true,
            });
        }
    }
    if caps.is_empty() {
        write_env_failure(
            &options.artifact_dir,
            "opencode",
            "no enabled OpenCode models in /models",
        );
        return Err(AppError::Message(
            "no enabled OpenCode models in /models".into(),
        ));
    }
    let mut written = Vec::new();
    let mut saw_chat = false;
    let mut saw_responses = false;
    let max_models = std::env::var("VELLUM_LIVE_OPENCODE_MAX")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(usize::MAX)
        .max(1);
    let selected = select_opencode_caps(&caps, max_models);
    let mut projected_nonstreaming = Vec::new();
    let mut catalog_decisions = Vec::new();
    for cap in &selected {
        let Some(plan) = plan_opencode_model(cap) else {
            continue;
        };
        if plan.wire.contains("chat") {
            saw_chat = true;
        }
        if plan.wire.contains("responses") {
            saw_responses = true;
        }
        let mut kind_results: Vec<Value> = Vec::new();
        for kind in &plan.kinds {
            // Use this model's own capability. A non-streaming model must not
            // inherit SSE from another model; a streaming model keeps SSE for
            // marker/tool/continuation as well as the dedicated SSE kind.
            let stream = cap.streaming;
            let body = json!({
                "model": plan.catalog_id,
                "store": false,
                "stream": stream,
                "tool_choice": "none",
                "tools": [],
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{
                        "type": "input_text",
                        "text": official_live_marker_input("VELLUM_OPENCODE_OK")
                    }]
                }]
            });
            let artifact = post_responses_sse(
                session,
                "opencode",
                &plan.catalog_id,
                &plan.wire,
                LivePath::Proxy,
                *kind,
                body.clone(),
                None,
            )
            .await?;
            if cap.streaming
                && artifact
                    .stream_quality
                    .is_some_and(|quality| !streaming_qualifies(quality))
            {
                projected_nonstreaming.push(json!({
                    "catalog_model": plan.catalog_id,
                    "wire": plan.wire,
                    "declared_streaming": true,
                    "stream_quality": artifact.stream_quality,
                    "projected_streaming": false,
                    "kind": format!("{kind:?}").to_ascii_lowercase(),
                }));
            }
            written.push(
                write_redacted_artifact(
                    &options.artifact_dir,
                    &format!(
                        "opencode-{}-{:?}.json",
                        plan.catalog_id.replace([':', '/'], "_"),
                        kind
                    )
                    .to_ascii_lowercase(),
                    &artifact,
                )
                .map_err(|error| AppError::Message(error.to_string()))?,
            );
            // Decision pass per kind: a `provider_unavailable` is retried
            // exactly once after a cooldown; a `no_delta` stream gets a
            // buffered (`stream: false`) retry. Neither rewrites the wire
            // capability; the model-level decision table records the action.
            let mut buffered_quality = None;
            let mut buffered_outcome = None;
            let mut effective_outcome = artifact.outcome;
            let mut effective_quality = artifact.stream_quality;
            if artifact.outcome == LiveOutcome::Fail
                && artifact.error_category.as_deref() == Some("provider_unavailable")
            {
                let cooldown = Duration::from_millis(retry_cooldown_ms());
                eprintln!(
                    "live: opencode {} {:?} provider_unavailable; single retry after {:?}",
                    plan.catalog_id, kind, cooldown
                );
                tokio::time::sleep(cooldown).await;
                let retry = post_responses_sse(
                    session,
                    "opencode",
                    &plan.catalog_id,
                    &plan.wire,
                    LivePath::Proxy,
                    *kind,
                    body.clone(),
                    None,
                )
                .await?;
                written.push(
                    write_redacted_artifact(
                        &options.artifact_dir,
                        &format!(
                            "opencode-{}-{:?}-retry.json",
                            plan.catalog_id.replace([':', '/'], "_"),
                            kind
                        )
                        .to_ascii_lowercase(),
                        &retry,
                    )
                    .map_err(|error| AppError::Message(error.to_string()))?,
                );
                effective_outcome = retry.outcome;
                effective_quality = retry.stream_quality;
            } else if artifact.outcome == LiveOutcome::Ok
                && matches!(artifact.stream_quality, Some(StreamQuality::NoDelta))
            {
                let buffered = post_responses_sse(
                    session,
                    "opencode",
                    &plan.catalog_id,
                    &plan.wire,
                    LivePath::Proxy,
                    *kind,
                    json!({
                        "model": plan.catalog_id,
                        "store": false,
                        "stream": false,
                        "tool_choice": "none",
                        "tools": [],
                        "input": [{
                            "type": "message",
                            "role": "user",
                            "content": [{
                                "type": "input_text",
                                "text": official_live_marker_input("VELLUM_OPENCODE_OK")
                            }]
                        }]
                    }),
                    None,
                )
                .await?;
                written.push(
                    write_redacted_artifact(
                        &options.artifact_dir,
                        &format!(
                            "opencode-{}-{:?}-buffered.json",
                            plan.catalog_id.replace([':', '/'], "_"),
                            kind
                        )
                        .to_ascii_lowercase(),
                        &buffered,
                    )
                    .map_err(|error| AppError::Message(error.to_string()))?,
                );
                buffered_quality = buffered.stream_quality;
                buffered_outcome = Some(buffered.outcome);
                if buffered.outcome == LiveOutcome::Ok {
                    effective_outcome = LiveOutcome::Ok;
                    effective_quality = buffered.stream_quality;
                }
            }
            kind_results.push(json!({
                "kind": format!("{kind:?}").to_ascii_lowercase(),
                "streaming_requested": stream,
                "outcome": format!("{:?}", effective_outcome),
                "quality": effective_quality.map(|quality| quality.as_token()),
                "retried_after_cooldown": artifact.error_category.as_deref() == Some("provider_unavailable"),
                "buffered_retried": buffered_quality.is_some(),
                "buffered_outcome": buffered_outcome.map(|outcome| format!("{outcome:?}")),
                "buffered_quality": buffered_quality.map(|quality| quality.as_token()),
            }));
        }
        let (action, reason) = opencode_model_action(&kind_results);
        catalog_decisions.push(json!({
            "catalog_model": plan.catalog_id,
            "wire": plan.wire,
            "declared_streaming": cap.streaming,
            "tools": cap.tools,
            "action": action,
            "reason": reason,
            "kinds": kind_results,
        }));
    }
    if !catalog_decisions.is_empty() {
        let path = options.artifact_dir.join("opencode-catalog-decisions.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "note": "model-level catalog actions from live evidence; never rewrites wire capability. keep_model_streaming: incremental observed. keep_model_with_streaming_false: usable buffered/end-flush only. hide_from_active_catalog: every kind ultimately failed (provider_unavailable retried once after cooldown; no_delta retried buffered).",
                "retry_cooldown_ms": retry_cooldown_ms(),
                "models": catalog_decisions,
            }))
            .unwrap_or_else(|_| "{}".into()),
        )
        .map_err(|error| AppError::Message(format!(
            "write opencode catalog decisions: {error}"
        )))?;
        written.push(path);
    }
    if !projected_nonstreaming.is_empty() {
        let path = options
            .artifact_dir
            .join("opencode-streaming-projection.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "note": "routes that declared streaming but whose live stream quality was not incremental are projected to streaming=false",
                "projections": projected_nonstreaming,
            }))
            .unwrap_or_else(|_| "{}".into()),
        )
        .map_err(|error| AppError::Message(format!("write opencode projection: {error}")))?;
        written.push(path);
    }
    if !saw_chat {
        let _ = std::fs::write(
            options.artifact_dir.join("opencode-chat-na.json"),
            json!({"status":"N/A","reason":"no Chat model in catalog"}).to_string(),
        );
    }
    if !saw_responses {
        let _ = std::fs::write(
            options.artifact_dir.join("opencode-responses-na.json"),
            json!({"status":"N/A","reason":"no Responses model in catalog"}).to_string(),
        );
    }
    Ok(written)
}

async fn run_official(session: &LiveSession, options: &LiveRunOptions) -> AppResult<Vec<PathBuf>> {
    let model = resolve_official_live_model(options.official_model_env.as_deref())
        .map_err(|error| AppError::Message(error.to_string()))?;
    let catalog = session
        .desktop
        .active_model_routes()
        .into_iter()
        .find(|route| {
            route.route_id == "openai-official" && official_catalog_is_luna(&route.catalog_id)
        })
        .or_else(|| {
            session
                .desktop
                .active_model_routes()
                .into_iter()
                .find(|route| {
                    route.route_id == "openai-official"
                        && official_catalog_is_luna(&route.upstream_model)
                })
        });
    let catalog_id = catalog
        .as_ref()
        .map(|route| route.catalog_id.clone())
        .unwrap_or_else(|| model.to_string());
    if !official_catalog_is_luna(&catalog_id) && catalog_id != model {
        return Err(AppError::Message(format!(
            "Official live model must be {PINNED_OFFICIAL_MODEL}, catalog has {catalog_id}"
        )));
    }
    let campaign_dir = campaign_budget_dir();
    let auth = session
        .state
        .codex_oauth()
        .valid_default_auth()
        .await
        .map_err(|error| AppError::Message(error.to_string()))?
        .ok_or_else(|| AppError::Message("no managed Official OAuth account".into()))?;
    let account_hash_value = account_hash(&auth.account_id);
    let input = official_live_marker_input("VELLUM_LUNA_1");
    let body = official_live_create_body(model, &input);
    let http_body = official_live_http_body(model, &input);
    let mut direct_headers = official_live_metadata_headers();
    direct_headers.push((
        "authorization".into(),
        format!("Bearer {}", auth.access_token),
    ));
    direct_headers.push(("chatgpt-account-id".into(), auth.account_id.clone()));
    let mut proxy_headers = official_live_metadata_headers();
    proxy_headers.push((
        "authorization".into(),
        format!("Bearer {}", auth.access_token),
    ));
    proxy_headers.push(("chatgpt-account-id".into(), auth.account_id.clone()));
    assert_official_request_parity(&body, &body, &direct_headers, &proxy_headers)
        .map_err(|error| AppError::Message(error.to_string()))?;
    let _ = http_body;

    let idle_gap = Duration::from_secs(AB_IDLE_GAP_SECS);
    let turn_deadline = Duration::from_millis(OFFICIAL_FIRST_FRAME_DEADLINE_MS);
    let mut written = Vec::new();

    // ---- Direct: one socket, Cold -> Warm -> Idle ----
    let (mut direct_transport, direct_connect_ms) =
        official_direct_connect(&direct_headers).await?;
    let direct_config = AbRunConfig {
        route: "openai-official",
        catalog_model: &catalog_id,
        model,
        path: LivePath::Direct,
        account_id: Some(account_hash_value.as_str()),
        idle_gap,
        turn_deadline,
    };
    let direct_ledger = |_request_id: &str, observed: Option<u64>| LedgerFrame {
        upstream_first_frame_ms: observed,
        downstream_first_frame_ms: observed,
        connection_id: None,
        usage_record_count: 0,
    };
    let mut admit = || -> Result<u32, LiveAttributionError> {
        // Atomic load -> admit -> temp-file+rename persist, so a concurrent
        // gateway or a crashed process can never double-spend or tear the
        // ledger.
        OfficialTurnBudget::admit_and_persist(&campaign_dir)
    };
    let direct = run_ab_stage_sequence(
        &mut direct_transport,
        direct_connect_ms,
        &direct_config,
        &mut admit,
        direct_ledger,
    )
    .await;
    let _ = direct_transport.close().await;
    let direct_stopped = direct
        .last()
        .map(|artifact| should_stop_official_luna(artifact.error_category.as_deref(), None))
        .unwrap_or(false);
    written.extend(write_ab_run_artifacts(
        &options.artifact_dir,
        LivePath::Direct,
        &direct,
    )?);
    if direct_stopped {
        let reason = direct
            .last()
            .and_then(|artifact| artifact.error_category.clone())
            .unwrap_or_else(|| "unattributed".into());
        let _ = OfficialTurnBudget::stop_and_persist(&campaign_dir, &reason);
        let path = write_redacted_artifact(
            &options.artifact_dir,
            "luna-stop.json",
            direct.last().expect("stopped artifact"),
        )
        .map_err(|error| AppError::Message(error.to_string()))?;
        written.push(path);
        return Err(AppError::Message(
            "Official Luna stopped after 429/quota/unattributed transport".into(),
        ));
    }

    // ---- Proxy: one socket, Cold -> Warm -> Idle ----
    let (mut proxy_transport, proxy_connect_ms, proxy_request_id) =
        official_proxy_connect(session).await?;
    let proxy_config = AbRunConfig {
        route: "openai-official",
        catalog_model: &catalog_id,
        model,
        path: LivePath::Proxy,
        account_id: Some(account_hash_value.as_str()),
        idle_gap,
        turn_deadline,
    };
    let expected_rows = std::cell::Cell::new(0_u64);
    let proxy_ledger = move |_request_id: &str, _observed: Option<u64>| {
        let expected = expected_rows.get() + 1;
        expected_rows.set(expected);
        desktop_ledger_frame(session, &proxy_request_id, expected)
    };
    let mut admit = || -> Result<u32, LiveAttributionError> {
        OfficialTurnBudget::admit_and_persist(&campaign_dir)
    };
    let proxy = run_ab_stage_sequence(
        &mut proxy_transport,
        proxy_connect_ms,
        &proxy_config,
        &mut admit,
        proxy_ledger,
    )
    .await;
    let _ = proxy_transport.close().await;
    let proxy_stopped = proxy
        .last()
        .map(|artifact| should_stop_official_luna(artifact.error_category.as_deref(), None))
        .unwrap_or(false);
    written.extend(write_ab_run_artifacts(
        &options.artifact_dir,
        LivePath::Proxy,
        &proxy,
    )?);
    if proxy_stopped {
        let reason = proxy
            .last()
            .and_then(|artifact| artifact.error_category.clone())
            .unwrap_or_else(|| "unattributed".into());
        let _ = OfficialTurnBudget::stop_and_persist(&campaign_dir, &reason);
        let path = write_redacted_artifact(
            &options.artifact_dir,
            "luna-stop.json",
            proxy.last().expect("stopped artifact"),
        )
        .map_err(|error| AppError::Message(error.to_string()))?;
        written.push(path);
        return Err(AppError::Message(
            "Official Luna stopped after 429/quota/unattributed transport".into(),
        ));
    }
    written.push(write_official_ab_attribution(
        &options.artifact_dir,
        &direct,
        &proxy,
    )?);
    Ok(written)
}

/// Open the single direct-to-OpenAI WebSocket used by the Direct A/B side.
/// A connect timeout is enforced here so no detached connect task survives.
async fn official_direct_connect(headers: &[(String, String)]) -> AppResult<(WsAbTransport, u64)> {
    let mut request = OFFICIAL_DIRECT_WSS
        .into_client_request()
        .map_err(|error| AppError::Message(format!("official direct request: {error}")))?;
    for (name, value) in headers {
        if let (Ok(header_name), Ok(header_value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            request.headers_mut().insert(header_name, header_value);
        }
    }
    WsAbTransport::connect(request, Duration::from_secs(30))
        .await
        .map_err(|error| AppError::Message(format!("official direct connect: {error}")))
}

async fn official_proxy_connect(session: &LiveSession) -> AppResult<(WsAbTransport, u64, String)> {
    let started = Instant::now();
    // One id for the whole connection's lifetime (WebSocket carries no
    // per-frame headers). Every `response.create` turn on this socket is
    // recorded under it; the proxy ledger closure reads it once per turn,
    // sequentially, so each lookup sees exactly that turn's own usage row.
    let request_id = format!("live-{}", ulid::Ulid::new());
    let mut request = format!("ws://{}/v1/responses", session.addr)
        .into_client_request()
        .map_err(|error| AppError::Message(format!("official proxy request: {error}")))?;
    request.headers_mut().insert(
        BOUNDARY_KEY_HEADER,
        session
            .boundary
            .parse()
            .map_err(|error| AppError::Message(format!("boundary header: {error}")))?,
    );
    request.headers_mut().insert(
        "x-request-id",
        request_id
            .parse()
            .map_err(|error| AppError::Message(format!("x-request-id header: {error}")))?,
    );
    for (name, value) in official_live_metadata_headers() {
        request.headers_mut().insert(
            HeaderName::from_bytes(name.as_bytes())
                .map_err(|error| AppError::Message(format!("header {name}: {error}")))?,
            HeaderValue::from_str(&value)
                .map_err(|error| AppError::Message(format!("header value {name}: {error}")))?,
        );
    }
    let (transport, _) = WsAbTransport::connect(request, Duration::from_secs(30))
        .await
        .map_err(|error| AppError::Message(format!("official proxy upgrade: {error}")))?;
    Ok((transport, started.elapsed().as_millis() as u64, request_id))
}

pub fn live_artifact_json(artifact: &LiveRequestArtifact) -> AppResult<Value> {
    artifact_to_redacted_json(artifact).map_err(|error| AppError::Message(error.to_string()))
}

// --- Direct-vs-Proxy A/B attribution harness ------------------------------
//
// Everything below drives the Cold/Warm/Idle sequence against an
// `AbTransport`, which is a connection abstraction, not a network call.
// `WsAbTransport` wraps a real tokio-tungstenite WebSocket for the live
// Official path; `MockAbTransport` (test module) exercises the same driver
// in-process without any network. The ledger closure is supplied per side:
// Direct has no Vellum bridge so upstream == downstream == the observed
// first frame, while Proxy reads the real two-stage split out of the
// Desktop runtime's usage store.

/// One message read off an A/B connection. `Timeout` is a real, distinct
/// outcome (no message arrived within the budget) — never conflated with
/// `Closed` or with a transport error.
#[derive(Debug, Clone)]
pub enum AbMessage {
    Application(Value),
    Ping,
    Pong,
    Closed,
    Timeout,
}

/// Seam between the Cold/Warm/Idle driver and an actual socket. A single
/// `AbTransport` instance is reused across all three stages, which is what
/// proves same-connection reuse: the driver never constructs a second one.
/// Used generically only (never as `dyn AbTransport`), so the missing
/// `Send` auto-bound on the desugared future is fine here.
#[allow(async_fn_in_trait)]
pub trait AbTransport {
    async fn send_create(&mut self, body: Value) -> Result<(), String>;
    async fn recv(&mut self, timeout: Duration) -> Result<AbMessage, String>;
}

/// Real WebSocket `AbTransport` used by the live Official run. Both the
/// direct-to-OpenAI connection and the Desktop-bridge proxy connection are
/// plain tokio-tungstenite WebSockets, so one wrapper serves both.
struct WsAbTransport {
    client: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl WsAbTransport {
    /// Connect and report the handshake cost. `connect_async` has no
    /// internal deadline, so the caller's connect timeout is enforced here
    /// and no connect task is left running after it fires.
    async fn connect<R>(request: R, timeout: Duration) -> Result<(Self, u64), String>
    where
        R: tokio_tungstenite::tungstenite::client::IntoClientRequest + Unpin,
    {
        let started = Instant::now();
        let connected = tokio::time::timeout(timeout, tokio_tungstenite::connect_async(request))
            .await
            .map_err(|_| "official WebSocket connect timed out".to_string())?;
        let client = connected
            .map_err(|error| format!("official WebSocket upgrade: {error}"))?
            .0;
        let connect_ms = started.elapsed().as_millis() as u64;
        Ok((Self { client }, connect_ms))
    }

    async fn close(&mut self) -> Result<(), String> {
        self.client
            .close(None)
            .await
            .map_err(|error| error.to_string())
    }
}

impl AbTransport for WsAbTransport {
    async fn send_create(&mut self, body: Value) -> Result<(), String> {
        self.client
            .send(Message::Text(body.to_string().into()))
            .await
            .map_err(|error| error.to_string())
    }

    async fn recv(&mut self, timeout: Duration) -> Result<AbMessage, String> {
        match tokio::time::timeout(timeout, self.client.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let value = serde_json::from_str(&text)
                    .map_err(|error| format!("decode WebSocket application frame: {error}"))?;
                Ok(AbMessage::Application(value))
            }
            Ok(Some(Ok(Message::Binary(bytes)))) => {
                let value = serde_json::from_slice(&bytes)
                    .map_err(|error| format!("decode WebSocket application frame: {error}"))?;
                Ok(AbMessage::Application(value))
            }
            Ok(Some(Ok(Message::Ping(_)))) => Ok(AbMessage::Ping),
            Ok(Some(Ok(Message::Pong(_)))) => Ok(AbMessage::Pong),
            Ok(Some(Ok(Message::Close(_)))) => Ok(AbMessage::Closed),
            Ok(Some(Ok(Message::Frame(_)))) => {
                tokio::task::yield_now().await;
                Ok(AbMessage::Ping)
            }
            Ok(Some(Err(error))) => Err(error.to_string()),
            Ok(None) => Ok(AbMessage::Closed),
            Err(_) => Ok(AbMessage::Timeout),
        }
    }
}

/// Read the Desktop runtime's own usage-ledger frame for one proxy
/// connection. The runtime writes each turn's row synchronously right after
/// forwarding that turn's `response.completed` downstream, but the harness
/// is a separate task; poll briefly so a Proxy artifact never reads a stale
/// (previous stage's) row and reports a fake bridge delay. `expected_rows`
/// is how many rows this connection should have by now (1 for Cold, 2 for
/// Warm, 3 for Idle), so each stage waits for its own new row instead of
/// returning the previous stage's timing.
fn desktop_ledger_frame(
    session: &LiveSession,
    request_id: &str,
    expected_rows: u64,
) -> LedgerFrame {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let records = session
            .desktop
            .proxy_runtime()
            .usage_records()
            .unwrap_or_default();
        let frame = ledger_frame_for_request(&records, request_id);
        if frame.usage_record_count >= expected_rows || std::time::Instant::now() >= deadline {
            return frame;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Campaign-global budget directory, independent of the per-run artifact
/// directory so candidate / installed / retry all share one Luna ledger and
/// switching `--artifact-dir` can never reset it. Falls back to the default
/// artifact dir when the production data dir is unavailable (tests). Shared
/// with the eval gateway so the SWE evaluator spends the same ledger.
pub fn campaign_budget_dir() -> PathBuf {
    production_vellum_data_dir()
        .map(|root| root.join("live-campaign"))
        .unwrap_or_else(|| PathBuf::from("target/vellum-live"))
}

pub struct AbRunConfig<'a> {
    pub route: &'a str,
    pub catalog_model: &'a str,
    pub model: &'a str,
    pub path: LivePath,
    pub account_id: Option<&'a str>,
    pub idle_gap: Duration,
    pub turn_deadline: Duration,
}

struct AbTurnCollected {
    frame_count: u64,
    delta_count: u64,
    output_delta_count: u64,
    reasoning_delta_count: u64,
    first_output_delta_ms: Option<u64>,
    first_reasoning_delta_ms: Option<u64>,
    first_frame_ms: Option<u64>,
    completed_ms: u64,
    outcome: LiveOutcome,
    error_category: Option<String>,
    status: Option<u16>,
    request_id: Option<String>,
}

async fn run_ab_turn<T: AbTransport>(
    transport: &mut T,
    body: &Value,
    deadline: Duration,
) -> AbTurnCollected {
    let started = Instant::now();
    if let Err(error) = transport.send_create(body.clone()).await {
        return AbTurnCollected {
            frame_count: 0,
            delta_count: 0,
            output_delta_count: 0,
            reasoning_delta_count: 0,
            first_output_delta_ms: None,
            first_reasoning_delta_ms: None,
            first_frame_ms: None,
            completed_ms: started.elapsed().as_millis() as u64,
            outcome: LiveOutcome::Fail,
            error_category: Some(error),
            status: None,
            request_id: None,
        };
    }
    let mut frame_count = 0_u64;
    let mut delta_count = 0_u64;
    let mut output_delta_count = 0_u64;
    let mut reasoning_delta_count = 0_u64;
    let mut first_output_delta_ms = None;
    let mut first_reasoning_delta_ms = None;
    let mut first_frame_ms = None;
    let mut request_id = None;
    loop {
        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return AbTurnCollected {
                frame_count,
                delta_count,
                output_delta_count,
                reasoning_delta_count,
                first_output_delta_ms,
                first_reasoning_delta_ms,
                first_frame_ms,
                completed_ms: started.elapsed().as_millis() as u64,
                outcome: LiveOutcome::Timeout,
                error_category: Some("first_frame_timeout".into()),
                status: None,
                request_id,
            };
        }
        let message = match transport.recv(remaining).await {
            Ok(message) => message,
            Err(error) => {
                return AbTurnCollected {
                    frame_count,
                    delta_count,
                    output_delta_count,
                    reasoning_delta_count,
                    first_output_delta_ms,
                    first_reasoning_delta_ms,
                    first_frame_ms,
                    completed_ms: started.elapsed().as_millis() as u64,
                    outcome: LiveOutcome::Fail,
                    error_category: Some(error),
                    status: None,
                    request_id,
                };
            }
        };
        match message {
            AbMessage::Ping | AbMessage::Pong => continue,
            AbMessage::Timeout => {
                return AbTurnCollected {
                    frame_count,
                    delta_count,
                    output_delta_count,
                    reasoning_delta_count,
                    first_output_delta_ms,
                    first_reasoning_delta_ms,
                    first_frame_ms,
                    completed_ms: started.elapsed().as_millis() as u64,
                    outcome: LiveOutcome::Timeout,
                    error_category: Some("first_frame_timeout".into()),
                    status: None,
                    request_id,
                };
            }
            AbMessage::Closed => {
                return AbTurnCollected {
                    frame_count,
                    delta_count,
                    output_delta_count,
                    reasoning_delta_count,
                    first_output_delta_ms,
                    first_reasoning_delta_ms,
                    first_frame_ms,
                    completed_ms: started.elapsed().as_millis() as u64,
                    outcome: LiveOutcome::Fail,
                    error_category: Some("socket_closed".into()),
                    status: None,
                    request_id,
                };
            }
            AbMessage::Application(value) => {
                frame_count += 1;
                if first_frame_ms.is_none() {
                    first_frame_ms = Some(started.elapsed().as_millis() as u64);
                }
                let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
                // Per-turn stream quality: output deltas (`output_text.delta` /
                // function-arguments) qualify, reasoning deltas are a separate
                // channel and never substitute for text/tool streaming.
                if is_output_delta(kind) {
                    delta_count += 1;
                    output_delta_count += 1;
                    first_output_delta_ms.get_or_insert(started.elapsed().as_millis() as u64);
                } else if is_reasoning_delta(kind) {
                    delta_count += 1;
                    reasoning_delta_count += 1;
                    first_reasoning_delta_ms.get_or_insert(started.elapsed().as_millis() as u64);
                }
                if let Some(id) = value.pointer("/response/id").and_then(Value::as_str) {
                    request_id = Some(id.to_string());
                }
                let status = value
                    .get("status")
                    .and_then(Value::as_u64)
                    .map(|s| s as u16);
                if kind == "response.completed" {
                    return AbTurnCollected {
                        frame_count,
                        delta_count,
                        output_delta_count,
                        reasoning_delta_count,
                        first_output_delta_ms,
                        first_reasoning_delta_ms,
                        first_frame_ms,
                        completed_ms: started.elapsed().as_millis() as u64,
                        outcome: LiveOutcome::Ok,
                        error_category: None,
                        status,
                        request_id,
                    };
                }
                if kind == "response.failed" || status == Some(429) {
                    let outcome = if status == Some(429) {
                        LiveOutcome::Quota
                    } else {
                        LiveOutcome::Fail
                    };
                    let category = if status == Some(429) {
                        "provider_quota".to_string()
                    } else {
                        value
                            .pointer("/error/category")
                            .and_then(Value::as_str)
                            .unwrap_or("provider_protocol")
                            .to_string()
                    };
                    return AbTurnCollected {
                        frame_count,
                        delta_count,
                        output_delta_count,
                        reasoning_delta_count,
                        first_output_delta_ms,
                        first_reasoning_delta_ms,
                        first_frame_ms,
                        completed_ms: started.elapsed().as_millis() as u64,
                        outcome,
                        error_category: Some(category),
                        status,
                        request_id,
                    };
                }
            }
        }
    }
}

/// Hold the socket open for `idle_gap`, correctly consuming Ping/Pong (and
/// any stray application frame) without treating either as an error or as
/// ending the wait early. Only a real close is a failure.
async fn wait_idle_processing_control_frames<T: AbTransport>(
    transport: &mut T,
    idle_gap: Duration,
) -> Result<(), String> {
    let started = Instant::now();
    loop {
        let remaining = idle_gap.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(());
        }
        match transport.recv(remaining).await? {
            AbMessage::Ping | AbMessage::Pong | AbMessage::Timeout | AbMessage::Application(_) => {
                continue;
            }
            AbMessage::Closed => return Err("socket closed during idle wait".into()),
        }
    }
}

/// Drive Cold -> Warm -> Idle on one already-connected `transport`. Stops
/// immediately (does not send the next stage's create) on 429/quota or an
/// unattributable failure, per `should_stop_official_luna`. `admit` is called
/// once per stage *before* that stage's create is sent, so the campaign cap
/// rejects a surplus send before it leaves the harness; the `ledger` closure
/// receives the observed first-frame time (the harness clock) so a Direct
/// connection can honestly report upstream == downstream (no bridge to
/// measure) while a Proxy connection reads the real two-stage split.
pub async fn run_ab_stage_sequence<T: AbTransport>(
    transport: &mut T,
    connection_ms: u64,
    config: &AbRunConfig<'_>,
    mut admit: impl FnMut() -> Result<u32, LiveAttributionError>,
    ledger: impl Fn(&str, Option<u64>) -> LedgerFrame,
) -> Vec<LiveRequestArtifact> {
    let mut results = Vec::new();
    for stage in [AbStage::Cold, AbStage::Warm, AbStage::Idle] {
        if stage == AbStage::Idle {
            if let Err(error) =
                wait_idle_processing_control_frames(transport, config.idle_gap).await
            {
                let mut artifact = new_live_artifact(
                    config.route,
                    config.catalog_model,
                    "responses",
                    config.path,
                    config.account_id,
                );
                artifact.stage = Some(stage);
                artifact.connection_ms = Some(connection_ms);
                artifact.outcome = LiveOutcome::Fail;
                artifact.error_category = Some(error);
                results.push(artifact);
                break;
            }
        }
        if let Err(error) = admit() {
            let mut artifact = new_live_artifact(
                config.route,
                config.catalog_model,
                "responses",
                config.path,
                config.account_id,
            );
            artifact.stage = Some(stage);
            artifact.connection_ms = Some(connection_ms);
            artifact.outcome = LiveOutcome::Quota;
            artifact.error_category = Some(error.to_string());
            results.push(artifact);
            break;
        }
        let marker = format!("VELLUM_LUNA_AB_{stage:?}");
        let body = official_live_create_body(config.model, &official_live_marker_input(&marker));
        let collected = run_ab_turn(transport, &body, config.turn_deadline).await;

        let mut artifact = new_live_artifact(
            config.route,
            config.catalog_model,
            "responses",
            config.path,
            config.account_id,
        );
        artifact.stage = Some(stage);
        artifact.kind = Some("marker".into());
        artifact.connection_ms = Some(connection_ms);
        artifact.turn_first_frame_ms = collected.first_frame_ms;
        artifact.frame_count = collected.frame_count;
        artifact.delta_count = collected.delta_count;
        artifact.output_delta_count = collected.output_delta_count;
        artifact.reasoning_delta_count = collected.reasoning_delta_count;
        artifact.first_output_delta_ms = collected.first_output_delta_ms;
        artifact.first_reasoning_delta_ms = collected.first_reasoning_delta_ms;
        artifact.completed_ms = Some(collected.completed_ms);
        artifact.outcome = collected.outcome;
        artifact.error_category = collected.error_category.clone();
        artifact.request_id = collected.request_id.clone();
        if artifact.outcome == LiveOutcome::Ok {
            let quality = classify_stream_quality(&StreamQualityInput {
                streaming_requested: true,
                capability_streaming: true,
                streamed: true,
                output_delta_count: collected.output_delta_count,
                first_output_delta_ms: collected.first_output_delta_ms,
                tool_delta_count: 0,
                first_tool_delta_ms: None,
                terminal_ms: artifact.completed_ms,
                reasoning_delta_count: collected.reasoning_delta_count,
                first_reasoning_delta_ms: collected.first_reasoning_delta_ms,
            });
            artifact.stream_quality = Some(quality);
            // Official / Responses stays strict: `end_flush` and `no_delta`
            // turns are protocol failures even though the transport reached
            // `response.completed`.
            if !streaming_qualifies(quality) {
                artifact.outcome = LiveOutcome::Fail;
                artifact.error_category = Some(match quality {
                    StreamQuality::NoDelta => "no_stream_delta".into(),
                    _ => "end_of_stream_flush".into(),
                });
            }
        }
        match collected.request_id.as_deref() {
            Some(request_id) => {
                let ledger_frame = ledger(request_id, collected.first_frame_ms);
                apply_ledger_frame(&mut artifact, &ledger_frame);
            }
            None if stage == AbStage::Cold => {
                artifact.cold_total_first_frame_ms =
                    cold_total_first_frame_ms(artifact.connection_ms, artifact.turn_first_frame_ms);
            }
            None => {}
        }

        let should_stop =
            should_stop_official_luna(artifact.error_category.as_deref(), collected.status);
        results.push(artifact);
        if should_stop {
            break;
        }
    }
    results
}

fn ab_stage_timing(
    artifacts: &[LiveRequestArtifact],
    stage: AbStage,
) -> vellum_proxy_runtime::AbStageTiming {
    use vellum_proxy_runtime::AbStageTiming;
    let Some(artifact) = artifacts
        .iter()
        .find(|artifact| artifact.stage == Some(stage))
    else {
        return AbStageTiming::fail();
    };
    if artifact.outcome != LiveOutcome::Ok {
        return AbStageTiming::fail();
    }
    let ms = match stage {
        AbStage::Cold => artifact.cold_total_first_frame_ms,
        AbStage::Warm | AbStage::Idle => artifact.turn_first_frame_ms,
    };
    match ms {
        Some(ms) => AbStageTiming::ok(ms),
        None => AbStageTiming::fail(),
    }
}

/// Write one side's (Direct or Proxy) three stage artifacts as
/// `official-{direct,proxy}-{1,2,3}.json`, in stage order.
pub fn write_ab_run_artifacts(
    artifact_dir: &Path,
    path: LivePath,
    artifacts: &[LiveRequestArtifact],
) -> AppResult<Vec<PathBuf>> {
    let label = match path {
        LivePath::Direct => "direct",
        LivePath::Proxy => "proxy",
    };
    let mut written = Vec::new();
    for (index, artifact) in artifacts.iter().enumerate() {
        let name = format!("official-{label}-{}.json", index + 1);
        written.push(
            write_redacted_artifact(artifact_dir, &name, artifact)
                .map_err(|error| AppError::Message(error.to_string()))?,
        );
    }
    Ok(written)
}

/// Build and write `official-attribution.json`, applying the Direct/Proxy
/// Cold/Warm/Idle classification table to all 6 runs.
pub fn write_official_ab_attribution(
    artifact_dir: &Path,
    direct: &[LiveRequestArtifact],
    proxy: &[LiveRequestArtifact],
) -> AppResult<PathBuf> {
    use vellum_proxy_runtime::AbComparisonInput;
    let input = AbComparisonInput {
        direct_cold: ab_stage_timing(direct, AbStage::Cold),
        direct_warm: ab_stage_timing(direct, AbStage::Warm),
        direct_idle: ab_stage_timing(direct, AbStage::Idle),
        proxy_cold: ab_stage_timing(proxy, AbStage::Cold),
        proxy_warm: ab_stage_timing(proxy, AbStage::Warm),
        proxy_idle: ab_stage_timing(proxy, AbStage::Idle),
    };
    let verdict = vellum_proxy_runtime::classify_official_ab(&input);
    let direct_json = direct
        .iter()
        .map(live_artifact_json)
        .collect::<AppResult<Vec<_>>>()?;
    let proxy_json = proxy
        .iter()
        .map(live_artifact_json)
        .collect::<AppResult<Vec<_>>>()?;
    let summary = json!({
        "verdict": verdict,
        "budget_cold_extra_ms": input.direct_cold.ms.map(extra_delay_budget_ms),
        "budget_warm_extra_ms": input.direct_warm.ms.map(extra_delay_budget_ms),
        "direct": direct_json,
        "proxy": proxy_json,
    });
    let path = artifact_dir.join("official-attribution.json");
    let text = serde_json::to_string_pretty(&summary)
        .map_err(|error| AppError::Message(format!("serialize official attribution: {error}")))?;
    std::fs::write(&path, text)
        .map_err(|error| AppError::Message(format!("write {}: {error}", path.display())))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_live_args_defaults_to_all_candidate() {
        let parsed = parse_live_args(&[]).unwrap();
        assert_eq!(parsed.route, LiveRouteFilter::All);
        assert_eq!(parsed.phase, LivePhase::Candidate);
    }

    #[test]
    fn parse_live_args_rejects_unknown_route() {
        let error = parse_live_args(&["--route".into(), "doctor".into()]).unwrap_err();
        assert!(error.to_string().contains("unknown live route"));
    }

    #[test]
    fn parse_live_args_rejects_installed_phase() {
        let error = parse_live_args(&["--phase".into(), "installed".into()]).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("not a valid acceptance mode"), "{text}");
        assert!(text.contains("Installed Gate D"), "{text}");
        assert!(!text.contains("expected candidate|installed"), "{text}");
    }

    #[test]
    fn env_failure_log_names_the_route() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_env_failure(dir.path(), "grok", "DesktopCredentials missing");
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("route=grok"));
        assert!(text.contains("blocked=true"));
        assert!(!text.contains("sk-"));
    }

    #[test]
    fn official_env_pin_is_consulted_before_send() {
        resolve_official_live_model(Some("gpt-5.6-luna")).unwrap();
        assert!(resolve_official_live_model(Some("gpt-5.4-mini")).is_err());
    }

    #[test]
    fn grok_attribution_end_flush_on_both_lanes_is_provider_behavior() {
        let direct = vec![
            Some(StreamQuality::EndFlush),
            Some(StreamQuality::EndFlush),
            Some(StreamQuality::EndFlush),
        ];
        let proxy = vec![
            Some(StreamQuality::EndFlush),
            Some(StreamQuality::EndFlush),
            Some(StreamQuality::Incremental),
        ];
        assert_eq!(
            grok_attribution_verdict(&direct, &proxy),
            "provider_behavior"
        );
    }

    #[test]
    fn grok_attribution_direct_incremental_with_proxy_flush_is_a_regression() {
        let direct = vec![
            Some(StreamQuality::Incremental),
            Some(StreamQuality::Incremental),
            Some(StreamQuality::EndFlush),
        ];
        let proxy = vec![
            Some(StreamQuality::EndFlush),
            Some(StreamQuality::EndFlush),
            Some(StreamQuality::EndFlush),
        ];
        assert_eq!(
            grok_attribution_verdict(&direct, &proxy),
            "vellum_regression"
        );
    }

    #[test]
    fn brave_text_results_collects_bounded_title_url_snippet_entries() {
        let value = json!({
            "encrypted_output": null,
            "output": "rendered",
            "results": [
                {"type": "image_result", "ref_id": "turn0search0", "image_url": "https://img/1"},
                {
                    "type": "text_result",
                    "ref_id": "turn0search1",
                    "url": "https://a.example/1",
                    "title": "First",
                    "snippet": "first snippet"
                },
                {
                    "type": "text_result",
                    "ref_id": "turn0search2",
                    "url": "https://a.example/2",
                    "title": "Second"
                }
            ]
        });
        let hits = brave_text_results(&value);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://a.example/1");
        assert_eq!(hits[0].title.as_deref(), Some("First"));
        assert_eq!(hits[0].snippet.as_deref(), Some("first snippet"));
        assert_eq!(hits[1].url, "https://a.example/2");
        assert!(hits[1].snippet.is_none());
        // Missing URL or non-text results never contribute a hit.
        assert!(brave_text_results(&json!({})).is_empty());
        assert!(brave_text_results(&json!({ "results": [] })).is_empty());
        assert!(brave_text_results(&json!({
            "results": [{"type": "text_result", "title": "no url"}]
        }))
        .is_empty());
    }

    #[test]
    fn render_brave_results_preserves_every_url_verbatim() {
        let hits = vec![
            BraveSearchHit {
                url: "https://a.example/1".to_string(),
                title: Some("First".to_string()),
                snippet: Some("snippet one".to_string()),
            },
            BraveSearchHit {
                url: "https://a.example/2".to_string(),
                title: None,
                snippet: None,
            },
        ];
        let rendered = render_brave_results(&hits);
        assert!(rendered.contains("https://a.example/1"));
        assert!(rendered.contains("https://a.example/2"));
        assert!(rendered.contains("First"));
        assert!(rendered.contains("snippet one"));
        assert!(rendered.contains("(untitled)"));
    }

    #[test]
    fn terminal_citation_match_requires_an_actually_returned_url() {
        let urls = vec![
            "https://a.example/1".to_string(),
            "https://a.example/2".to_string(),
        ];
        assert!(terminal_cites_any_url(
            "see https://a.example/1 for details",
            &urls
        ));
        assert!(!terminal_cites_any_url(
            "see https://elsewhere.example/x",
            &urls
        ));
        assert!(!terminal_cites_any_url("no citation at all", &urls));
    }

    #[test]
    fn function_call_tool_name_only_accepts_a_function_call_item() {
        assert_eq!(
            function_call_tool_name(&json!({"item": {"type": "web_search_call"}})),
            Some("web_search_call".to_string())
        );
        assert_eq!(
            function_call_tool_name(
                &json!({"item": {"type": "function_call", "name": "web_search"}})
            ),
            Some("web_search".to_string())
        );
        assert_eq!(
            function_call_tool_name(&json!({"item": {"type": "message", "name": "web_search"}})),
            None
        );
        assert_eq!(
            function_call_tool_name(&json!({"item": {"type": "function_call"}})),
            None
        );
        assert_eq!(function_call_tool_name(&json!({})), None);
    }

    #[test]
    fn function_call_query_extracts_only_a_present_string_query() {
        assert_eq!(
            function_call_query(r#"{"query":"rust release notes"}"#),
            Some("rust release notes".to_string())
        );
        assert_eq!(function_call_query(r#"{"query":""}"#), None);
        assert_eq!(function_call_query("not json"), None);
        assert_eq!(function_call_query(r#"{"arg": 1}"#), None);
    }

    #[test]
    fn function_call_call_id_prefers_the_explicit_item_call_id() {
        assert_eq!(
            function_call_call_id(&json!({"item": {"call_id": "call_ws1", "id": "call_ws1"}})),
            Some("call_ws1".to_string())
        );
        assert_eq!(
            function_call_call_id(&json!({"item": {"id": "call_ws2"}})),
            Some("call_ws2".to_string())
        );
        assert_eq!(
            function_call_call_id(&json!({"item_id": "call_ws3"})),
            Some("call_ws3".to_string())
        );
        assert_eq!(function_call_call_id(&json!({})), None);
        assert_eq!(function_call_call_id(&json!({"item_id": ""})), None);
    }

    #[test]
    fn terminal_answer_from_sse_keeps_only_output_text_deltas_and_bounds() {
        let raw = [
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"The latest release is 1.8. \"}",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"see https://a.example/1\"}",
            "event: response.reasoning_summary_text.delta\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"must be ignored\"}",
            "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"ignored too\"}",
            "event: response.completed\ndata: {\"type\":\"response.completed\"}",
        ]
        .join("\n\n");
        let answer = terminal_answer_from_sse(&raw);
        assert!(answer.contains("https://a.example/1"));
        assert!(!answer.contains("must be ignored"));
        assert!(!answer.contains("ignored too"));

        let long = format!(
            "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{}\"}}",
            "x".repeat(TERMINAL_ANSWER_MAX_CHARS + 100)
        );
        assert_eq!(
            terminal_answer_from_sse(&long).chars().count(),
            TERMINAL_ANSWER_MAX_CHARS
        );
        assert_eq!(terminal_answer_from_sse(""), "");
    }

    #[test]
    fn brave_terminal_body_replays_the_call_and_output_without_query() {
        let body = brave_terminal_body(
            "grok-cli/4.6",
            "web_search",
            "call_ws1",
            r#"{"query":"rust release notes"}"#,
            "1. First\n   https://a.example/1\n",
        );
        assert_eq!(body["model"], "grok-cli/4.6");
        assert_eq!(body["stream"], true);
        // The assistant function_call is replayed first, so the follow-up tool
        // output is not orphaned.
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][0]["name"], "web_search");
        assert_eq!(body["input"][0]["call_id"], "call_ws1");
        assert_eq!(
            body["input"][0]["arguments"],
            r#"{"query":"rust release notes"}"#
        );
        // Then the output on the same call id, then the terminal prompt.
        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["input"][1]["call_id"], "call_ws1");
        assert!(body["input"][1]["output"]
            .as_str()
            .unwrap()
            .contains("https://a.example/1"));
        assert_eq!(body["input"][2]["type"], "message");
        // The replayed assistant call keeps its arguments verbatim (a faithful
        // replay), but the tool output and the terminal prompt never echo the
        // original query — only the public result facts (title/URL/snippet).
        assert!(body["input"][0]["arguments"]
            .as_str()
            .unwrap()
            .contains("rust release notes"));
        assert!(!body["input"][1]["output"]
            .as_str()
            .unwrap()
            .contains("rust release notes"));
        assert!(!body["input"][2]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("rust release notes"));
        assert!(body["input"][1]["output"]
            .as_str()
            .unwrap()
            .contains("https://a.example/1"));
    }

    #[test]
    fn brave_roundtrip_artifact_redacts_key_and_bounds_answer_and_records_brave_facts() {
        let dir = tempfile::tempdir().unwrap();
        let mut artifact = new_live_artifact(
            "grok-cli",
            "grok-cli/4.6",
            "responses",
            LivePath::Proxy,
            None,
        );
        artifact.kind = Some("web_search".into());
        artifact.outcome = LiveOutcome::Ok;
        artifact.frame_count = 1;
        // A verbose reply that survives bounding while carrying a
        // provider-key-shaped token must have that token scrubbed before it is
        // stored, while the first URL survives.
        let terminal = format!(
            "The top source is https://a.example/1. credential sk-ABC123secret {}",
            "y".repeat(TERMINAL_ANSWER_MAX_CHARS * 2)
        );
        let record = BraveRoundtripRecord {
            search_result_count: Some(3),
            first_result_url: Some("https://a.example/1".to_string()),
            terminal_answer: Some(terminal),
            terminal_citation_matched: Some(true),
        };
        let path =
            write_brave_roundtrip_artifact(dir.path(), "grok-cli/4.6", &artifact, &record).unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["search_result_count"], 3);
        assert_eq!(value["first_result_url"], "https://a.example/1");
        assert_eq!(value["terminal_citation_matched"], true);
        let answer = value["terminal_answer"].as_str().unwrap();
        assert!(answer.contains("https://a.example/1"));
        // Bounded: the oversized reply was truncated to the cap.
        assert!(answer.chars().count() <= TERMINAL_ANSWER_MAX_CHARS);
        assert!(!text.contains("sk-ABC123secret"));
        assert!(text.contains("sk-[REDACTED]"));
        // The first URL lives in `first_result_url`, never in `request_id`.
        assert!(artifact.request_id.is_none());
    }

    #[test]
    fn grok_attribution_both_lanes_incremental_means_the_case_is_closed() {
        let direct = vec![
            Some(StreamQuality::Incremental),
            Some(StreamQuality::Incremental),
            Some(StreamQuality::Incremental),
        ];
        let proxy = vec![
            Some(StreamQuality::Incremental),
            Some(StreamQuality::Incremental),
            Some(StreamQuality::Incremental),
        ];
        assert_eq!(
            grok_attribution_verdict(&direct, &proxy),
            "both_lanes_incremental"
        );
    }

    #[test]
    fn grok_attribution_provider_native_flush_with_incremental_proxy_is_not_a_regression() {
        let direct = vec![
            Some(StreamQuality::EndFlush),
            Some(StreamQuality::EndFlush),
            Some(StreamQuality::EndFlush),
        ];
        let proxy = vec![
            Some(StreamQuality::Incremental),
            Some(StreamQuality::Incremental),
            Some(StreamQuality::Incremental),
        ];
        assert_eq!(
            grok_attribution_verdict(&direct, &proxy),
            "no_vellum_regression"
        );
    }

    #[test]
    fn grok_attribution_split_or_provider_error_is_inconclusive() {
        // A lane that failed before any quality was classified (provider
        // error) is never attributed, regardless of the other lane.
        assert_eq!(
            grok_attribution_verdict(
                &[
                    Some(StreamQuality::Incremental),
                    None,
                    Some(StreamQuality::EndFlush)
                ],
                &[
                    Some(StreamQuality::Incremental),
                    Some(StreamQuality::Incremental),
                    Some(StreamQuality::EndFlush)
                ],
            ),
            "inconclusive"
        );
        assert_eq!(grok_attribution_verdict(&[None], &[None]), "inconclusive");
    }

    #[test]
    fn opencode_model_action_keeps_streaming_only_when_incremental_observed() {
        let kinds = vec![
            json!({"kind": "marker", "outcome": "Ok", "quality": "incremental"}),
            json!({"kind": "sse", "outcome": "Ok", "quality": "incremental"}),
        ];
        let (action, _) = opencode_model_action(&kinds);
        assert_eq!(action, "keep_model_streaming");
    }

    #[test]
    fn opencode_model_action_projects_buffered_or_end_flush_to_streaming_false() {
        let kinds = vec![
            json!({"kind": "marker", "outcome": "Ok", "quality": "end_flush"}),
            json!({"kind": "sse", "outcome": "Ok", "quality": "buffered"}),
        ];
        let (action, _) = opencode_model_action(&kinds);
        assert_eq!(action, "keep_model_with_streaming_false");
    }

    #[test]
    fn opencode_model_action_hides_when_any_kind_failed_even_after_retries() {
        let kinds = vec![
            json!({"kind": "marker", "outcome": "Ok", "quality": "incremental"}),
            json!({"kind": "sse", "outcome": "Fail", "quality": Value::Null}),
        ];
        let (action, reason) = opencode_model_action(&kinds);
        assert_eq!(action, "hide_from_active_catalog");
        assert!(reason.contains("sse=Fail"));
        assert_eq!(opencode_model_action(&[]).0, "hide_from_active_catalog");
    }

    #[test]
    fn opencode_selection_covers_streaming_and_buffered_without_mixing() {
        let caps = vec![
            OpenCodeModelCaps {
                catalog_id: "buffered".into(),
                wire: "chat".into(),
                streaming: false,
                tools: true,
                enabled: true,
            },
            OpenCodeModelCaps {
                catalog_id: "streaming".into(),
                wire: "chat".into(),
                streaming: true,
                tools: true,
                enabled: true,
            },
        ];
        let selected = select_opencode_caps(&caps, 2);
        assert_eq!(selected.len(), 2);
        assert!(selected
            .iter()
            .any(|cap| cap.streaming && cap.catalog_id == "streaming"));
        assert!(selected
            .iter()
            .any(|cap| !cap.streaming && cap.catalog_id == "buffered"));
        let streaming = selected.iter().find(|cap| cap.streaming).unwrap();
        let plan = plan_opencode_model(streaming).unwrap();
        assert!(plan.kinds.contains(&LiveKind::Sse));
        let buffered = selected.iter().find(|cap| !cap.streaming).unwrap();
        let buffered_plan = plan_opencode_model(buffered).unwrap();
        assert!(!buffered_plan.kinds.contains(&LiveKind::Sse));
        assert_eq!(buffered_plan.fallback.as_deref(), Some("route_fallback"));
    }

    // --- A/B harness: mock transport + driver ------------------------

    /// A message is only visible once its own turn's `send_create` has gone
    /// out: `buckets[k]` holds messages available while exactly `k` creates
    /// have been sent (i.e. after the k-th send, before the (k+1)-th). This
    /// is what makes it a faithful mock of a real socket instead of a plain
    /// queue — a plain queue would let a later turn's scripted response leak
    /// into an earlier idle-wait purely because both read from one FIFO.
    struct MockAbTransport {
        sent: Vec<Value>,
        buckets: Vec<std::collections::VecDeque<AbMessage>>,
        /// Simulated gap between the first output delta and the frame after it
        /// (e.g. `response.completed`), so a fully-instant mock still classifies
        /// as `incremental` instead of an `end_flush`.
        delta_lead_ms: u64,
        after_output_delta: bool,
    }

    impl MockAbTransport {
        fn new() -> Self {
            Self {
                sent: Vec::new(),
                buckets: vec![std::collections::VecDeque::new()],
                delta_lead_ms: 0,
                after_output_delta: false,
            }
        }

        fn with_delta_lead_ms(mut self, ms: u64) -> Self {
            self.delta_lead_ms = ms;
            self
        }

        /// Queue `message` for delivery once `after_sends` creates have been
        /// sent (1 = after the first turn's create, 2 = after the second, ...).
        fn push_after(&mut self, after_sends: usize, message: AbMessage) -> &mut Self {
            while self.buckets.len() <= after_sends {
                self.buckets.push(std::collections::VecDeque::new());
            }
            self.buckets[after_sends].push_back(message);
            self
        }
    }

    impl AbTransport for MockAbTransport {
        async fn send_create(&mut self, body: Value) -> Result<(), String> {
            self.sent.push(body);
            Ok(())
        }

        async fn recv(&mut self, timeout: Duration) -> Result<AbMessage, String> {
            let bucket_index = self.sent.len().min(self.buckets.len().saturating_sub(1));
            if let Some(message) = self
                .buckets
                .get_mut(bucket_index)
                .and_then(|bucket| bucket.pop_front())
            {
                if self.after_output_delta && self.delta_lead_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(self.delta_lead_ms)).await;
                }
                self.after_output_delta = matches!(
                    &message,
                    AbMessage::Application(value)
                        if is_output_delta(
                            value.get("type").and_then(Value::as_str).unwrap_or("")
                        )
                );
                return Ok(message);
            }
            tokio::time::sleep(timeout.min(Duration::from_millis(5))).await;
            Ok(AbMessage::Timeout)
        }
    }

    fn completed_frame(request_id: &str) -> Value {
        json!({
            "type": "response.completed",
            "response": { "id": request_id, "usage": { "input_tokens": 5, "output_tokens": 3 } }
        })
    }

    /// An output-text delta frame so a mock stream qualifies as incremental
    /// (the Official/responses gate rejects `no_delta` / `end_flush` turns).
    fn output_delta_frame(text: &str) -> Value {
        json!({
            "type": "response.output_text.delta",
            "response": { "id": "unused", "output_index": 0 },
            "delta": text
        })
    }

    fn ab_config(path: LivePath) -> AbRunConfig<'static> {
        AbRunConfig {
            route: "openai-official",
            catalog_model: PINNED_OFFICIAL_MODEL,
            model: PINNED_OFFICIAL_MODEL,
            path,
            account_id: Some("acct_ab_mock_secret"),
            idle_gap: Duration::from_millis(15),
            turn_deadline: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn ab_sequence_runs_cold_warm_idle_on_one_socket_reusing_the_same_transport() {
        let mut transport = MockAbTransport::new().with_delta_lead_ms(60);
        transport
            .push_after(1, AbMessage::Application(output_delta_frame("cold")))
            .push_after(1, AbMessage::Application(completed_frame("req-cold")))
            .push_after(2, AbMessage::Application(output_delta_frame("warm")))
            .push_after(2, AbMessage::Application(completed_frame("req-warm")))
            .push_after(2, AbMessage::Ping)
            .push_after(2, AbMessage::Pong)
            .push_after(3, AbMessage::Application(output_delta_frame("idle")))
            .push_after(3, AbMessage::Application(completed_frame("req-idle")));

        let mut ledger = std::collections::HashMap::new();
        ledger.insert(
            "req-cold".to_string(),
            LedgerFrame {
                upstream_first_frame_ms: Some(40),
                downstream_first_frame_ms: Some(55),
                connection_id: Some("conn-1".into()),
                usage_record_count: 1,
            },
        );
        ledger.insert(
            "req-warm".to_string(),
            LedgerFrame {
                upstream_first_frame_ms: Some(10),
                downstream_first_frame_ms: Some(14),
                connection_id: Some("conn-1".into()),
                usage_record_count: 1,
            },
        );
        ledger.insert(
            "req-idle".to_string(),
            LedgerFrame {
                upstream_first_frame_ms: Some(12),
                downstream_first_frame_ms: Some(18),
                connection_id: Some("conn-1".into()),
                usage_record_count: 1,
            },
        );

        let config = ab_config(LivePath::Proxy);
        let results = run_ab_stage_sequence(
            &mut transport,
            33,
            &config,
            || Ok(1),
            |request_id, _observed| ledger.get(request_id).cloned().unwrap_or_default(),
        )
        .await;

        assert_eq!(results.len(), 3, "all three stages must run");
        assert_eq!(
            transport.sent.len(),
            3,
            "same socket must carry all three creates, no reconnect"
        );
        assert_eq!(results[0].stage, Some(AbStage::Cold));
        assert_eq!(results[1].stage, Some(AbStage::Warm));
        assert_eq!(results[2].stage, Some(AbStage::Idle));
        assert!(results
            .iter()
            .all(|artifact| artifact.outcome == LiveOutcome::Ok));

        let cold = &results[0];
        assert_eq!(cold.connection_ms, Some(33));
        assert_eq!(cold.connection_id.as_deref(), Some("conn-1"));
        let observed = cold.turn_first_frame_ms.unwrap();
        let upstream = cold.upstream_first_frame_ms.unwrap();
        let downstream = cold.downstream_first_frame_ms.unwrap();
        // Three genuinely distinct numbers: the harness's own clock, and
        // the ledger's two separate pipeline-stage timestamps. None may be
        // a duplicate of another.
        assert_ne!(upstream, downstream);
        assert_ne!(observed, upstream);
        assert_ne!(observed, downstream);
        assert_eq!(cold.bridge_delay_ms, Some(15));
        assert_eq!(cold.cold_total_first_frame_ms, Some(33 + observed));

        assert_eq!(results[1].cold_total_first_frame_ms, None);
        assert_eq!(results[2].cold_total_first_frame_ms, None);
    }

    #[tokio::test]
    async fn idle_wait_consumes_ping_pong_without_ending_early_or_erroring() {
        let mut transport = MockAbTransport::new();
        transport
            .push_after(0, AbMessage::Ping)
            .push_after(0, AbMessage::Pong)
            .push_after(0, AbMessage::Ping);
        let result =
            wait_idle_processing_control_frames(&mut transport, Duration::from_millis(15)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn idle_wait_fails_if_the_socket_closes() {
        let mut transport = MockAbTransport::new();
        transport.push_after(0, AbMessage::Closed);
        let result =
            wait_idle_processing_control_frames(&mut transport, Duration::from_millis(50)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn sequence_stops_immediately_on_429_and_never_sends_the_idle_stage() {
        let mut transport = MockAbTransport::new().with_delta_lead_ms(60);
        transport
            .push_after(1, AbMessage::Application(output_delta_frame("cold")))
            .push_after(1, AbMessage::Application(completed_frame("req-cold")))
            .push_after(
                2,
                AbMessage::Application(
                    json!({"type": "response.failed", "status": 429, "response": {"id": "req-warm"}}),
                ),
            );
        let config = ab_config(LivePath::Direct);
        let results = run_ab_stage_sequence(
            &mut transport,
            10,
            &config,
            || Ok(1),
            |_, _| LedgerFrame::default(),
        )
        .await;

        assert_eq!(results.len(), 2, "idle stage must not run after a 429");
        assert_eq!(results[1].outcome, LiveOutcome::Quota);
        assert_eq!(results[1].error_category.as_deref(), Some("provider_quota"));
        assert_eq!(
            transport.sent.len(),
            2,
            "the idle create must never be sent"
        );
    }

    #[tokio::test]
    async fn sequence_stops_when_failure_cannot_be_attributed() {
        let mut transport = MockAbTransport::new().with_delta_lead_ms(60);
        transport
            .push_after(1, AbMessage::Application(output_delta_frame("cold")))
            .push_after(1, AbMessage::Application(completed_frame("req-cold")))
            .push_after(
                2,
                AbMessage::Application(json!({
                    "type": "response.failed",
                    "response": {"id": "req-warm"},
                    "error": {"category": "unattributed"}
                })),
            );
        let config = ab_config(LivePath::Proxy);
        let results = run_ab_stage_sequence(
            &mut transport,
            10,
            &config,
            || Ok(1),
            |_, _| LedgerFrame::default(),
        )
        .await;

        assert_eq!(
            results.len(),
            2,
            "unattributable failure must stop the sequence"
        );
        assert_eq!(results[1].error_category.as_deref(), Some("unattributed"));
        assert_eq!(transport.sent.len(), 2);
    }

    #[tokio::test]
    async fn official_ab_end_to_end_writes_six_run_artifacts_and_one_attribution_with_no_secrets() {
        let dir = tempfile::tempdir().unwrap();

        let mut direct_transport = MockAbTransport::new().with_delta_lead_ms(60);
        for (turn, id) in ["d-cold", "d-warm", "d-idle"].into_iter().enumerate() {
            direct_transport
                .push_after(
                    turn + 1,
                    AbMessage::Application(output_delta_frame(&format!("direct-{id}"))),
                )
                .push_after(turn + 1, AbMessage::Application(completed_frame(id)));
        }
        let direct_config = ab_config(LivePath::Direct);
        let direct = run_ab_stage_sequence(
            &mut direct_transport,
            20,
            &direct_config,
            || Ok(1),
            |request_id, _observed| LedgerFrame {
                upstream_first_frame_ms: Some(110),
                downstream_first_frame_ms: Some(125),
                connection_id: Some(format!("conn-direct-{request_id}")),
                usage_record_count: 1,
            },
        )
        .await;

        let mut proxy_transport = MockAbTransport::new().with_delta_lead_ms(60);
        for (turn, id) in ["p-cold", "p-warm", "p-idle"].into_iter().enumerate() {
            proxy_transport
                .push_after(
                    turn + 1,
                    AbMessage::Application(output_delta_frame(&format!("proxy-{id}"))),
                )
                .push_after(turn + 1, AbMessage::Application(completed_frame(id)));
        }
        let proxy_config = ab_config(LivePath::Proxy);
        let proxy = run_ab_stage_sequence(
            &mut proxy_transport,
            25,
            &proxy_config,
            || Ok(1),
            |request_id, _observed| LedgerFrame {
                upstream_first_frame_ms: Some(210),
                downstream_first_frame_ms: Some(228),
                connection_id: Some(format!("conn-proxy-{request_id}")),
                usage_record_count: 1,
            },
        )
        .await;

        assert_eq!(direct.len(), 3);
        assert_eq!(proxy.len(), 3);

        let mut written = write_ab_run_artifacts(dir.path(), LivePath::Direct, &direct).unwrap();
        written.extend(write_ab_run_artifacts(dir.path(), LivePath::Proxy, &proxy).unwrap());
        written.push(write_official_ab_attribution(dir.path(), &direct, &proxy).unwrap());

        let names: Vec<String> = written
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        for expected in [
            "official-direct-1.json",
            "official-direct-2.json",
            "official-direct-3.json",
            "official-proxy-1.json",
            "official-proxy-2.json",
            "official-proxy-3.json",
            "official-attribution.json",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }

        let mut combined = String::new();
        for path in &written {
            combined.push_str(&std::fs::read_to_string(path).unwrap());
        }
        assert!(
            !combined.contains('@'),
            "no artifact may contain an email/account marker"
        );
        assert!(!combined.contains("acct_ab_mock_secret"));
        assert!(!combined.contains("sk-"));

        let attribution: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("official-attribution.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            attribution["verdict"],
            "official_issue_closed_no_vellum_problem"
        );

        let first_direct: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("official-direct-1.json")).unwrap(),
        )
        .unwrap();
        for field in [
            "artifact_commit",
            "binary_commit",
            "binary_sha256",
            "route",
            "catalog_model",
            "connection_id",
            "connection_ms",
            "turn_first_frame_ms",
            "cold_total_first_frame_ms",
            "upstream_first_frame_ms",
            "downstream_first_frame_ms",
            "bridge_delay_ms",
            "frame_count",
            "delta_count",
            "completed_ms",
            "usage_record_count",
            "outcome",
            "account_hash",
        ] {
            assert!(first_direct.get(field).is_some(), "missing {field}");
        }
    }

    #[tokio::test]
    async fn campaign_cap_exceeded_stops_before_the_next_create_is_sent() {
        let mut transport = MockAbTransport::new().with_delta_lead_ms(60);
        transport
            .push_after(1, AbMessage::Application(output_delta_frame("cold")))
            .push_after(1, AbMessage::Application(completed_frame("req-cold")))
            .push_after(2, AbMessage::Application(output_delta_frame("warm")))
            .push_after(2, AbMessage::Application(completed_frame("req-warm")))
            .push_after(3, AbMessage::Application(output_delta_frame("idle")))
            .push_after(3, AbMessage::Application(completed_frame("req-idle")));
        let config = ab_config(LivePath::Proxy);
        // The cap admits the first stage and rejects the second: the driver
        // must stop without sending the warm create.
        let mut admits = 0_u32;
        let results = run_ab_stage_sequence(
            &mut transport,
            10,
            &config,
            || {
                admits += 1;
                if admits > 1 {
                    Err(LiveAttributionError::OfficialTurnCapExceeded { spent: 1, cap: 1 })
                } else {
                    Ok(1)
                }
            },
            |_, _| LedgerFrame::default(),
        )
        .await;

        assert_eq!(
            results.len(),
            2,
            "warm/idle must not run after the cap rejects admit"
        );
        assert_eq!(results[1].outcome, LiveOutcome::Quota);
        assert_eq!(
            transport.sent.len(),
            1,
            "the warm create must never be sent"
        );
    }

    #[test]
    fn campaign_budget_dir_is_stable_and_independent_of_the_artifact_dir() {
        let artifact = PathBuf::from("target/vellum-live-candidate");
        let other_artifact = PathBuf::from("target/vellum-live-installed");
        let budget_dir = campaign_budget_dir();
        assert_ne!(budget_dir, artifact);
        assert_ne!(budget_dir, other_artifact);
        // The production data dir is fixed per machine; the budget must not
        // move when --artifact-dir changes.
        assert!(production_vellum_data_dir()
            .map(|root| budget_dir.starts_with(&root))
            .unwrap_or(true));
    }
}
