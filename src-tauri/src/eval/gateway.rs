use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use base64::Engine as _;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{oneshot, RwLock};

use super::manifest::{FaultKind, FaultSpec};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Every SWE task may consume at most this many `/v1/responses` turns.
/// Requests beyond the cap are refused before they are forwarded. For Luna,
/// this also preserves the campaign worst case: 6 A/B + 6 installed A/B +
/// 30 SWE = 42/50, with 8 turns in reserve.
pub const SWE_MAX_TURNS_PER_TASK: u32 = 30;

/// Shared admission state for every attempt of one SWE case. The evaluator
/// may retry a transient provider failure once; a fresh credential must not
/// reset either the hard turn budget or trace request numbering.
#[derive(Clone)]
pub struct SweTurnBudget {
    cap: u32,
    turns: Arc<AtomicU32>,
    request_count: Arc<AtomicU32>,
}

impl SweTurnBudget {
    pub fn new(cap: u32) -> Self {
        Self {
            cap,
            turns: Arc::new(AtomicU32::new(0)),
            request_count: Arc::new(AtomicU32::new(0)),
        }
    }
}

/// Token circuit breaker for evaluations.
/// Tracks cumulative gross input tokens across non-compaction `/v1/responses` requests.
/// Once spent + projected input tokens exceed `gross_input_limit`, subsequent requests are refused
/// before dispatch (returning HTTP 429 `swe_token_limit`).
#[derive(Clone)]
pub struct EvalTokenBudget {
    pub gross_input_limit: u64,
    pub cumulative_input_tokens: Arc<AtomicU64>,
    reserved_input_tokens: Arc<AtomicU64>,
}

impl EvalTokenBudget {
    pub fn new(gross_input_limit: u64) -> Self {
        Self {
            gross_input_limit,
            cumulative_input_tokens: Arc::new(AtomicU64::new(0)),
            reserved_input_tokens: Arc::new(AtomicU64::new(0)),
        }
    }

    fn try_reserve(&self, projected: u64) -> Result<(), (u64, u64)> {
        let mut spent = self.cumulative_input_tokens.load(Ordering::SeqCst);
        let result = self.reserved_input_tokens.fetch_update(
            Ordering::SeqCst,
            Ordering::SeqCst,
            |reserved| {
                spent = self.cumulative_input_tokens.load(Ordering::SeqCst);
                (spent.saturating_add(reserved).saturating_add(projected) <= self.gross_input_limit)
                    .then_some(reserved.saturating_add(projected))
            },
        );
        result.map(|_| ()).map_err(|reserved| (spent, reserved))
    }

    fn release_reservation(&self, projected: u64) {
        self.reserved_input_tokens
            .fetch_sub(projected, Ordering::SeqCst);
    }

    fn finalize_reservation(&self, projected: u64, actual: Option<u64>) {
        // No usage event (JSON response, disconnect, or send error) is
        // charged conservatively at the pre-dispatch estimate so the hard
        // cap cannot be bypassed by a response that omits SSE usage.
        self.cumulative_input_tokens
            .fetch_add(actual.unwrap_or(projected), Ordering::SeqCst);
        self.release_reservation(projected);
    }
}

struct EvalTokenReservation {
    budget: EvalTokenBudget,
    projected: u64,
    dispatched: bool,
    settled: bool,
}

impl EvalTokenReservation {
    fn new(budget: EvalTokenBudget, projected: u64) -> Self {
        Self {
            budget,
            projected,
            dispatched: false,
            settled: false,
        }
    }

    fn mark_dispatched(&mut self) {
        self.dispatched = true;
    }

    fn release(mut self) {
        self.budget.release_reservation(self.projected);
        self.settled = true;
    }

    fn finalize(mut self, actual: Option<u64>) {
        self.budget.finalize_reservation(self.projected, actual);
        self.settled = true;
    }
}

impl Drop for EvalTokenReservation {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        if self.dispatched {
            self.budget.finalize_reservation(self.projected, None);
        } else {
            self.budget.release_reservation(self.projected);
        }
        self.settled = true;
    }
}

pub type SweTokenBudget = EvalTokenBudget;

#[derive(Clone)]
struct GatewayState {
    backend: String,
    /// The boundary key the backend proxy was started with. The gateway is the
    /// only legitimate caller of that listener, so it presents the key the same
    /// way Codex does ??which also keeps evals exercising the real guard.
    boundary_key: String,
    client: reqwest::Client,
    tasks: Arc<RwLock<HashMap<String, Arc<TaskAccess>>>>,
    /// Shared Luna campaign ledger directory. Tests inject a tempdir.
    campaign_dir: PathBuf,
    run_official_turn_cap: Option<u32>,
    run_official_turns_spent: Arc<AtomicU32>,
    usage: Arc<crate::usage::UsageStore>,
}

struct TaskAccess {
    task_id: String,
    allowed_models: HashSet<String>,
    faults: HashMap<u32, FaultKind>,
    persistent_faults: Vec<FaultSpec>,
    request_count: Arc<AtomicU32>,
    trace_path: PathBuf,
    model_metadata: HashMap<String, TraceModelMetadata>,
    shell_contract: Option<String>,
    /// Luna SWE tasks spend the campaign ledger: every `/v1/responses` turn
    /// admits atomically before forwarding, the per-task turn cap is enforced
    /// before the send, and 429/quota stops the campaign.
    luna_swe: bool,
    swe_turn_budget: Option<SweTurnBudget>,
    swe_token_budget: Option<SweTokenBudget>,
    diagnostic_cursor: i64,
    request_namespace: String,
}

#[derive(Debug, Clone)]
pub struct TaskCredential {
    pub token: String,
    request_namespace: String,
}

impl TaskCredential {
    pub fn runtime_request_id(&self, request_index: u32) -> String {
        format!(
            "vellum-eval-{}-request-{request_index}",
            self.request_namespace
        )
    }

    pub fn runtime_request_id_hash(&self, request_index: u32) -> String {
        full_hash_identifier(&self.runtime_request_id(request_index))
    }
}

#[derive(Debug, Clone, Default)]
pub struct TraceModelMetadata {
    pub provider: String,
    pub route_id: String,
    pub upstream_model: String,
    /// Authoritative route classification from `ProviderKind`; never infer
    /// Official traffic from display names or model-name substrings.
    pub is_official: bool,
    /// Resolved harness contract for this model (issue #6 Phase 0). Recorded
    /// per request so an A/B run can attribute a change in behaviour to the
    /// harness variant rather than guessing.
    pub harness_profile: String,
}

pub fn is_official_route_metadata(meta: &TraceModelMetadata) -> bool {
    meta.is_official
}

pub struct EvalGateway {
    public_address: SocketAddr,
    tasks: Arc<RwLock<HashMap<String, Arc<TaskAccess>>>>,
    public_shutdown: Option<oneshot::Sender<()>>,
    backend_shutdown: Option<oneshot::Sender<()>>,
    public_task: tokio::task::JoinHandle<()>,
    backend_task: tokio::task::JoinHandle<()>,
    campaign_dir: PathBuf,
    run_official_turns_spent: Arc<AtomicU32>,
    usage: Arc<crate::usage::UsageStore>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TraceRecord<'a> {
    event: &'a str,
    timestamp_ms: i64,
    task_id: &'a str,
    request_index: u32,
    method: &'a str,
    path: &'a str,
    model: Option<&'a str>,
    provider: Option<&'a str>,
    route_id: Option<&'a str>,
    upstream_model: Option<&'a str>,
    official_route: Option<bool>,
    harness_profile: Option<&'a str>,
    shell_contract: Option<&'a str>,
    request_shape_hash: Option<String>,
    instructions_hash: Option<String>,
    instructions_mark_plan_mode: bool,
    instructions_mark_default_mode: bool,
    reasoning_effort: Option<String>,
    compaction: bool,
    status: u16,
    fault: Option<&'a str>,
    fault_rule: Option<String>,
    fault_threshold_tokens: Option<u64>,
    session_hash: Option<String>,
    input_item_types: BTreeMap<String, u64>,
    tool_schema_hash: Option<String>,
    tool_names: Vec<String>,
    tool_count: u64,
    malformed_tool_arguments: u64,
    reasoning_items: u64,
    encrypted_reasoning_bytes: u64,
    encrypted_reasoning_hashes: Vec<String>,
    grok_turn_index: Option<String>,
    request_model_visible_tokens: Option<u64>,
    request_input_tokens: Option<u64>,
    request_instruction_tokens: Option<u64>,
    request_tool_schema_tokens: Option<u64>,
    request_total_estimated_tokens: Option<u64>,
    terminal_sse: Option<bool>,
    sse_event_count: Option<u64>,
    response_bytes: Option<u64>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    disconnected: Option<bool>,
    lifecycle: Option<&'a str>,
}

impl EvalGateway {
    pub async fn start(state: AppState) -> AppResult<Self> {
        Self::start_with_campaign_dir(state, super::live::campaign_budget_dir()).await
    }

    pub async fn start_with_turn_budget(
        state: AppState,
        max_official_turns: Option<u32>,
        initial_official_turns: u32,
    ) -> AppResult<Self> {
        Self::start_configured(
            state,
            super::live::campaign_budget_dir(),
            None,
            max_official_turns,
            initial_official_turns,
        )
        .await
    }

    /// `start` with an explicit campaign ledger directory (tests inject a
    /// tempdir so Luna-gate assertions never touch the real user ledger).
    pub async fn start_with_campaign_dir(
        state: AppState,
        campaign_dir: PathBuf,
    ) -> AppResult<Self> {
        Self::start_configured(state, campaign_dir, None, None, 0).await
    }

    /// Test entry: inject ledger path and a fixture boundary key so the
    /// async test never creates credentials through `JournalCipher` and
    /// never holds the process-wide `VELLUM_MASTER_KEY` lock across await.
    #[cfg(test)]
    pub async fn start_for_test(
        state: AppState,
        campaign_dir: PathBuf,
        boundary_key: vellum_proxy_runtime::BoundaryKey,
    ) -> AppResult<Self> {
        Self::start_configured(state, campaign_dir, Some(boundary_key), None, 0).await
    }

    #[cfg(test)]
    pub async fn start_for_test_with_turn_budget(
        state: AppState,
        campaign_dir: PathBuf,
        boundary_key: vellum_proxy_runtime::BoundaryKey,
        max_official_turns: Option<u32>,
    ) -> AppResult<Self> {
        Self::start_configured(
            state,
            campaign_dir,
            Some(boundary_key),
            max_official_turns,
            0,
        )
        .await
    }

    async fn start_configured(
        state: AppState,
        campaign_dir: PathBuf,
        injected_boundary_key: Option<vellum_proxy_runtime::BoundaryKey>,
        run_official_turn_cap: Option<u32>,
        initial_official_turns: u32,
    ) -> AppResult<Self> {
        state.activate_proxy_routes();
        let usage = state.usage_store();
        let backend_listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|error| {
                AppError::Message(format!("cannot bind eval backend proxy: {error}"))
            })?;
        let backend_address = backend_listener.local_addr().map_err(|error| {
            AppError::Message(format!("cannot inspect eval backend address: {error}"))
        })?;
        let (backend_tx, backend_rx) = oneshot::channel();
        let boundary_key = match injected_boundary_key {
            Some(key) => key,
            None => crate::proxy::ensure_boundary_key(&state.data_root())?,
        };
        let backend_boundary_key = boundary_key.clone();
        let backend_task = tokio::spawn(async move {
            if let Err(error) = crate::proxy::serve_local_proxy(
                state,
                backend_listener,
                backend_boundary_key,
                backend_rx,
            )
            .await
            {
                log::error!("[Eval] backend proxy stopped: {error}");
            }
        });

        let tasks = Arc::new(RwLock::new(HashMap::new()));
        let run_official_turns_spent = Arc::new(AtomicU32::new(initial_official_turns));
        let gateway_state = Arc::new(GatewayState {
            backend: format!("http://{backend_address}"),
            boundary_key: boundary_key.expose_for_storage().to_string(),
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(650))
                .build()
                .map_err(|error| {
                    AppError::Message(format!("cannot create eval gateway client: {error}"))
                })?,
            tasks: Arc::clone(&tasks),
            campaign_dir: campaign_dir.clone(),
            run_official_turn_cap,
            run_official_turns_spent: Arc::clone(&run_official_turns_spent),
            usage: Arc::clone(&usage),
        });
        let public_listener = tokio::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .map_err(|error| AppError::Message(format!("cannot bind eval gateway: {error}")))?;
        let public_address = public_listener.local_addr().map_err(|error| {
            AppError::Message(format!("cannot inspect eval gateway address: {error}"))
        })?;
        let app = Router::new()
            .route("/v1/models", any(forward))
            .route("/v1/responses", any(forward))
            .route("/v1/responses/compact", any(forward))
            .fallback(reject_path)
            .with_state(gateway_state);
        let (public_tx, public_rx) = oneshot::channel();
        let public_task = tokio::spawn(async move {
            if let Err(error) = axum::serve(public_listener, app)
                .with_graceful_shutdown(async {
                    let _ = public_rx.await;
                })
                .await
            {
                log::error!("[Eval] public gateway stopped: {error}");
            }
        });

        Ok(Self {
            public_address,
            tasks,
            public_shutdown: Some(public_tx),
            backend_shutdown: Some(backend_tx),
            public_task,
            backend_task,
            campaign_dir,
            run_official_turns_spent,
            usage,
        })
    }

    pub fn port(&self) -> u16 {
        self.public_address.port()
    }

    pub fn official_turns_spent(&self) -> u32 {
        self.run_official_turns_spent.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub async fn register_task(
        &self,
        task_id: impl Into<String>,
        models: impl IntoIterator<Item = String>,
        faults: &[FaultSpec],
        trace_path: PathBuf,
    ) -> AppResult<TaskCredential> {
        self.register_task_with_runtime_metadata(
            task_id,
            models,
            faults,
            trace_path,
            HashMap::new(),
            None,
            Some(SweTurnBudget::new(SWE_MAX_TURNS_PER_TASK)),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn register_task_with_runtime_metadata(
        &self,
        task_id: impl Into<String>,
        models: impl IntoIterator<Item = String>,
        faults: &[FaultSpec],
        trace_path: PathBuf,
        model_metadata: HashMap<String, TraceModelMetadata>,
        shell_contract: Option<String>,
        swe_turn_budget: Option<SweTurnBudget>,
    ) -> AppResult<TaskCredential> {
        self.register_task_with_budgets(
            task_id,
            models,
            faults,
            trace_path,
            model_metadata,
            shell_contract,
            swe_turn_budget,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn register_task_with_budgets(
        &self,
        task_id: impl Into<String>,
        models: impl IntoIterator<Item = String>,
        faults: &[FaultSpec],
        trace_path: PathBuf,
        model_metadata: HashMap<String, TraceModelMetadata>,
        shell_contract: Option<String>,
        swe_turn_budget: Option<SweTurnBudget>,
        swe_token_budget: Option<SweTokenBudget>,
    ) -> AppResult<TaskCredential> {
        let task_id = task_id.into();
        let allowed_models = models
            .into_iter()
            .map(|model| model.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        if allowed_models.is_empty() {
            return Err(AppError::Message(
                "eval task must allow at least one model".into(),
            ));
        }
        let luna_swe = allowed_models
            .iter()
            .any(|model| vellum_proxy_runtime::official_catalog_is_luna(model));
        if luna_swe {
            // Pre-start gate: never launch a Luna SWE task when the campaign
            // ledger has less than 30% remaining (plan §3.3). The ledger is
            // the one shared across candidate/installed/retry/SWE runs.
            let budget = vellum_proxy_runtime::OfficialTurnBudget::load(&self.campaign_dir)
                .map_err(|error| AppError::Message(error.to_string()))?;
            let remaining_percent =
                (budget.remaining() as f64) * 100.0 / (budget.cap.max(1) as f64);
            if !vellum_proxy_runtime::luna_quota_allows_swe(remaining_percent) {
                return Err(AppError::Message(format!(
                    "Luna SWE blocked before start: remaining campaign quota {remaining_percent:.0}% is below the {:.0}% floor; refusing to launch",
                    vellum_proxy_runtime::LUNA_SWE_MIN_REMAINING_PERCENT
                )));
            }
        }
        let mut random = [0_u8; 32];
        getrandom::fill(&mut random)
            .map_err(|error| AppError::Message(format!("cannot create eval token: {error}")))?;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
        let request_namespace = hash_identifier(&token);
        let access = Arc::new(TaskAccess {
            task_id: task_id.clone(),
            allowed_models,
            faults: faults
                .iter()
                .filter(|fault| fault.while_total_estimated_tokens_above.is_none())
                .map(|fault| (fault.at_request, fault.kind.clone()))
                .collect(),
            persistent_faults: faults
                .iter()
                .filter(|fault| fault.while_total_estimated_tokens_above.is_some())
                .cloned()
                .collect(),
            request_count: swe_turn_budget
                .as_ref()
                .map(|budget| Arc::clone(&budget.request_count))
                .unwrap_or_else(|| Arc::new(AtomicU32::new(0))),
            trace_path,
            model_metadata: model_metadata
                .into_iter()
                .map(|(model, metadata)| (model.to_ascii_lowercase(), metadata))
                .collect(),
            shell_contract,
            luna_swe,
            swe_turn_budget,
            swe_token_budget,
            diagnostic_cursor: self.usage.diagnostic_cursor()?,
            request_namespace: request_namespace.clone(),
        });
        self.tasks.write().await.insert(token.clone(), access);
        Ok(TaskCredential {
            token,
            request_namespace,
        })
    }

    pub async fn revoke(&self, token: &str) {
        self.tasks.write().await.remove(token);
    }

    pub async fn shutdown(mut self) {
        if let Some(sender) = self.public_shutdown.take() {
            let _ = sender.send(());
        }
        if let Some(sender) = self.backend_shutdown.take() {
            let _ = sender.send(());
        }
        let _ = self.public_task.await;
        let _ = self.backend_task.await;
    }
}

async fn reject_path() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": {"message": "eval gateway path is not allowed"}})),
    )
        .into_response()
}

async fn forward(State(state): State<Arc<GatewayState>>, request: Request) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let method_allowed = if path == "/v1/models" {
        method == axum::http::Method::GET
    } else {
        method == axum::http::Method::POST
    };
    if !method_allowed {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let token = bearer_token(request.headers());
    let access = match token {
        Some(token) => state.tasks.read().await.get(token).cloned(),
        None => None,
    };
    let Some(access) = access else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": {"message": "invalid or expired eval credential"}})),
        )
            .into_response();
    };
    // Catalog discovery must not shift deterministic fault indices. Fault N
    // always means the Nth Responses/compact request, independent of whether a
    // particular Codex version calls `/v1/models`.
    let request_index = if path == "/v1/models" {
        0
    } else {
        access.request_count.fetch_add(1, Ordering::SeqCst) + 1
    };
    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(body) => body,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": format!("cannot read eval request: {error}")}})),
            )
                .into_response()
        }
    };
    let parsed = if body.is_empty() {
        None
    } else {
        serde_json::from_slice::<Value>(&body).ok()
    };
    let model = parsed
        .as_ref()
        .and_then(|value| value.get("model"))
        .and_then(Value::as_str);
    let compaction =
        path == "/v1/responses/compact" || parsed.as_ref().is_some_and(has_compaction_trigger);
    let mut request_diagnostics = request_diagnostics(parsed.as_ref(), &parts.headers);
    if path != "/v1/models"
        && !model
            .map(|model| access.allowed_models.contains(&model.to_ascii_lowercase()))
            .unwrap_or(false)
    {
        trace(
            &access,
            request_index,
            method.as_str(),
            &path,
            model,
            compaction,
            403,
            Some("model_not_allowed"),
            &request_diagnostics,
        );
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": {"message": "model is not allowed for this eval task"}})),
        )
            .into_response();
    }

    if path == "/v1/responses" && !compaction {
        let watchdog_candidate = match state
            .usage
            .task_stall_diagnostics_after(access.diagnostic_cursor)
        {
            Ok(diagnostics) => {
                let request_hashes = (1..request_index)
                    .map(|index| {
                        full_hash_identifier(&format!(
                            "vellum-eval-{}-request-{index}",
                            access.request_namespace
                        ))
                    })
                    .collect::<HashSet<_>>();
                diagnostics
                    .iter()
                    .rev()
                    .find_map(|diagnostic| {
                        diagnostic
                            .request_id_hash
                            .as_ref()
                            .filter(|hash| request_hashes.contains(*hash))
                            .map(|_| diagnostic.clone())
                    })
                    .filter(|diagnostic| diagnostic.watchdog_candidate || diagnostic.watchdog_stop)
            }
            Err(error) => {
                trace(
                    &access,
                    request_index,
                    method.as_str(),
                    &path,
                    model,
                    compaction,
                    500,
                    Some("task_stall_watchdog_observation_failed"),
                    &request_diagnostics,
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": {"message": format!(
                        "eval task-stall watchdog observation failed: {error}"
                    )}})),
                )
                    .into_response();
            }
        };
        if let Some(candidate) = watchdog_candidate {
            let terminal = vellum_proxy_runtime::task_stall::TaskStallTerminalDiagnostic {
                request_id_hash: full_hash_identifier(&format!(
                    "vellum-eval-{}-request-{request_index}",
                    access.request_namespace
                )),
                request_index: Some(request_index),
                task_revision: candidate.task_revision,
                recovery_count: candidate.recovery_injected_count,
                post_recovery_no_progress: candidate.post_recovery_no_progress,
                category: "continued_after_tool_disabled_finalization".into(),
            };
            if let Err(error) = state.usage.record_diagnostic_event(
                &vellum_proxy_runtime::DiagnosticEvent::TaskStallTerminal(terminal),
            ) {
                trace(
                    &access,
                    request_index,
                    method.as_str(),
                    &path,
                    model,
                    compaction,
                    500,
                    Some("task_stall_terminal_record_failed"),
                    &request_diagnostics,
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": {"message": format!(
                        "eval task-stall terminal recording failed: {error}"
                    )}})),
                )
                    .into_response();
            }
            trace(
                &access,
                request_index,
                method.as_str(),
                &path,
                model,
                compaction,
                409,
                Some("model_no_progress_after_recovery"),
                &request_diagnostics,
            );
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": {
                    "message": "model_no_progress_after_recovery: eval stopped before another provider turn"
                }})),
            )
                .into_response();
        }
    }

    let persistent_fault = access.persistent_faults.iter().find(|fault| {
        persistent_fault_matches(
            fault,
            request_index,
            request_diagnostics.request_total_estimated_tokens,
        )
    });
    if let Some(rule) = persistent_fault {
        request_diagnostics.fault_rule = Some("while_total_estimated_tokens_above".into());
        request_diagnostics.fault_threshold_tokens = rule.while_total_estimated_tokens_above;
        if let Some(response) = immediate_fault(&rule.kind) {
            let status = response.status().as_u16();
            trace(
                &access,
                request_index,
                method.as_str(),
                &path,
                model,
                compaction,
                status,
                Some(fault_name(&rule.kind)),
                &request_diagnostics,
            );
            return response;
        }
    }

    let fault = access.faults.get(&request_index);
    if let Some(kind) = fault {
        if let Some(response) = immediate_fault(kind) {
            let status = response.status().as_u16();
            trace(
                &access,
                request_index,
                method.as_str(),
                &path,
                model,
                compaction,
                status,
                Some(fault_name(kind)),
                &request_diagnostics,
            );
            return response;
        }
        if compaction {
            if let Some(response) = compact_fault(kind) {
                let status = response.status().as_u16();
                trace(
                    &access,
                    request_index,
                    method.as_str(),
                    &path,
                    model,
                    compaction,
                    status,
                    Some(fault_name(kind)),
                    &request_diagnostics,
                );
                return response;
            }
        }
    }

    if path == "/v1/models" {
        let models = access
            .allowed_models
            .iter()
            .map(|model| json!({"id": model, "object": "model", "owned_by": "vellum-eval"}))
            .collect::<Vec<_>>();
        trace(
            &access,
            request_index,
            method.as_str(),
            &path,
            None,
            false,
            200,
            None,
            &request_diagnostics,
        );
        return Json(json!({"object": "list", "data": models})).into_response();
    }

    // ---- SWE hard gate & Official turn budget: admit before every real send ----
    // The per-task cap applies to every provider. Official routes additionally spend
    // the single-run turn cap (if specified) and the shared campaign ledger atomically.
    // Compact requests are auxiliary (`/v1/responses/compact`) and never count as turns.
    let mut token_reservation: Option<EvalTokenReservation> = None;
    if path == "/v1/responses" && !compaction {
        if let Some(token_budget) = access.swe_token_budget.as_ref() {
            let projected = request_diagnostics
                .request_total_estimated_tokens
                .unwrap_or(0);
            if let Err((spent, reserved)) = token_budget.try_reserve(projected) {
                trace(
                    &access,
                    request_index,
                    method.as_str(),
                    &path,
                    model,
                    compaction,
                    429,
                    Some("swe_token_limit"),
                    &request_diagnostics,
                );
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(
                        json!({"error": {"message":
                            format!("Eval token limit exceeded (spent {spent} + reserved {reserved} + projected {projected} > {}); request refused before dispatch", token_budget.gross_input_limit)}}),
                    ),
                )
                    .into_response();
            }
            token_reservation = Some(EvalTokenReservation::new(token_budget.clone(), projected));
        }
        if let Some(budget) = access.swe_turn_budget.as_ref() {
            let turn = budget.turns.fetch_add(1, Ordering::SeqCst) + 1;
            if turn > budget.cap {
                if let Some(reservation) = token_reservation.take() {
                    reservation.release();
                }
                trace(
                    &access,
                    request_index,
                    method.as_str(),
                    &path,
                    model,
                    compaction,
                    429,
                    Some("swe_turn_cap"),
                    &request_diagnostics,
                );
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(
                        json!({"error": {"message":
                            format!("SWE turn cap exceeded ({turn} > {}); request refused before send", budget.cap)}}),
                    ),
                )
                    .into_response();
            }
        }
        let is_official_turn = access.luna_swe
            || model.is_some_and(|m| {
                access
                    .model_metadata
                    .get(&m.to_ascii_lowercase())
                    .is_some_and(is_official_route_metadata)
                    || vellum_proxy_runtime::official_catalog_is_luna(m)
            });
        if is_official_turn {
            if let Some(cap) = state.run_official_turn_cap {
                let update_res = state.run_official_turns_spent.fetch_update(
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                    |curr| {
                        if curr < cap {
                            Some(curr + 1)
                        } else {
                            None
                        }
                    },
                );
                if update_res.is_err() {
                    if let Some(reservation) = token_reservation.take() {
                        reservation.release();
                    }
                    let spent = state.run_official_turns_spent.load(Ordering::SeqCst);
                    trace(
                        &access,
                        request_index,
                        method.as_str(),
                        &path,
                        model,
                        compaction,
                        429,
                        Some("single_run_official_cap"),
                        &request_diagnostics,
                    );
                    return (
                        StatusCode::TOO_MANY_REQUESTS,
                        Json(json!({"error": {"message": format!("Single-run Official turn limit reached ({spent} >= {cap}); request refused before send")}})).into_response(),
                    )
                        .into_response();
                }
            }
            if let Err(error) =
                vellum_proxy_runtime::OfficialTurnBudget::admit_and_persist(&state.campaign_dir)
            {
                if let Some(reservation) = token_reservation.take() {
                    reservation.release();
                }
                if state.run_official_turn_cap.is_some() {
                    state
                        .run_official_turns_spent
                        .fetch_sub(1, Ordering::SeqCst);
                }
                trace(
                    &access,
                    request_index,
                    method.as_str(),
                    &path,
                    model,
                    compaction,
                    429,
                    Some("luna_campaign_quota"),
                    &request_diagnostics,
                );
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({"error": {"message": error.to_string()}})).into_response(),
                )
                    .into_response();
            }
            if state.run_official_turn_cap.is_none() {
                state
                    .run_official_turns_spent
                    .fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    let url = format!("{}{}", state.backend, path);
    let mut builder = state.client.request(
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::POST),
        url,
    );
    for (name, value) in &parts.headers {
        if matches!(
            name.as_str(),
            "host"
                | "content-length"
                | "authorization"
                | "cookie"
                | "connection"
                | "x-request-id"
                | "x-vellum-eval-shell-contract"
        ) {
            continue;
        }
        builder = builder.header(name, value);
    }
    if let Some(contract) = access.shell_contract.as_deref() {
        builder = builder.header("x-vellum-eval-shell-contract", contract);
    }
    builder = builder.header(
        "x-request-id",
        format!(
            "vellum-eval-{}-request-{request_index}",
            access.request_namespace
        ),
    );
    builder = builder.header(
        vellum_proxy_runtime::BOUNDARY_KEY_HEADER,
        &state.boundary_key,
    );
    if let Some(reservation) = token_reservation.as_mut() {
        reservation.mark_dispatched();
    }
    let upstream = match builder.body(body).send().await {
        Ok(response) => response,
        Err(error) => {
            if let Some(reservation) = token_reservation.take() {
                reservation.finalize(None);
            }
            trace(
                &access,
                request_index,
                method.as_str(),
                &path,
                model,
                compaction,
                502,
                Some("gateway_upstream"),
                &request_diagnostics,
            );
            return (
                StatusCode::BAD_GATEWAY,
                Json(
                    json!({"error": {"message": format!("eval gateway upstream failed: {error}")}}),
                ),
            )
                .into_response();
        }
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .cloned();
    let fault_name_value = fault.map(fault_name);
    trace(
        &access,
        request_index,
        method.as_str(),
        &path,
        model,
        compaction,
        status.as_u16(),
        fault_name_value,
        &request_diagnostics,
    );
    if access.luna_swe && status == StatusCode::TOO_MANY_REQUESTS {
        let _ = vellum_proxy_runtime::OfficialTurnBudget::stop_and_persist(
            &state.campaign_dir,
            "provider_quota",
        );
    }

    if matches!(fault, Some(FaultKind::MalformedJson)) {
        if let Some(reservation) = token_reservation.take() {
            reservation.finalize(None);
        }
        return (
            status,
            [(header::CONTENT_TYPE, "application/json")],
            "{malformed",
        )
            .into_response();
    }
    let mutate_sse = fault.cloned();
    let stream_access = Arc::clone(&access);
    let stream_path = path.clone();
    let stream_model = model.map(str::to_string);
    let stream_diagnostics = request_diagnostics.clone();
    let stream = async_stream::stream! {
        let mut source = upstream.bytes_stream();
        let mut luna_stopped = false;
        let mut lifecycle = StreamLifecycleGuard::new(
            Arc::clone(&stream_access),
            request_index,
            stream_path.clone(),
            stream_model.clone(),
            compaction,
            status.as_u16(),
            fault_name_value.map(str::to_string),
            stream_diagnostics.clone(),
            token_reservation.take(),
        );
        let mut sse_tail = String::new();
        let mut replay_source = Vec::new();
        while let Some(item) = source.next().await {
            match item {
                Ok(bytes) => {
                    lifecycle.response_bytes += bytes.len() as u64;
                    if matches!(mutate_sse, Some(FaultKind::TruncateSse))
                        && lifecycle.response_bytes > 256
                    {
                        lifecycle.disconnected = true;
                        break;
                    }
                    let mut output = bytes.to_vec();
                    let output_text = String::from_utf8_lossy(&output);
                    if stream_access.luna_swe && !luna_stopped {
                        // A `response.failed` carrying a quota / unattributed
                        // category stops the whole Luna campaign immediately.
                        for needle in ["\"provider_quota\"", "\"unattributed\"", "\"quota\""] {
                            if output_text.contains(needle) {
                                let _ = vellum_proxy_runtime::OfficialTurnBudget::stop_and_persist(
                                    &state.campaign_dir,
                                    "provider_quota",
                                );
                                luna_stopped = true;
                                break;
                            }
                        }
                    }
                    lifecycle.sse_event_count += output_text.matches("event:").count() as u64;
                    lifecycle.observe_sse(&output_text);
                    let combined = format!("{sse_tail}{output_text}");
                    let observed_terminal = combined.contains("response.completed")
                        || combined.contains("response.failed");
                    if observed_terminal && !lifecycle.terminal_sse {
                        lifecycle.terminal_sse = true;
                        lifecycle.trace("terminal_observed", "terminal_observed");
                    }
                    sse_tail = combined
                        .chars()
                        .rev()
                        .take(96)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect();
                    if matches!(mutate_sse, Some(FaultKind::DropCompleted))
                        && output_text.contains("response.completed")
                    {
                        lifecycle.terminal_sse = false;
                        continue;
                    }
                    if matches!(mutate_sse, Some(FaultKind::ReplayToolCall)) {
                        replay_source.extend_from_slice(&output);
                        continue;
                    }
                    if matches!(mutate_sse, Some(FaultKind::DuplicateDelta))
                        && String::from_utf8_lossy(&output).contains(".delta")
                    {
                        let duplicate = output.clone();
                        output.extend_from_slice(&duplicate);
                    }
                    if matches!(mutate_sse, Some(FaultKind::FragmentSse)) {
                        for fragment in output.chunks(7) {
                            yield Ok::<Bytes, Infallible>(Bytes::copy_from_slice(fragment));
                        }
                    } else {
                        yield Ok::<Bytes, Infallible>(Bytes::from(output));
                    }
                }
                Err(_) => {
                    lifecycle.disconnected = true;
                    break;
                }
            }
        }
        if matches!(mutate_sse, Some(FaultKind::ReplayToolCall)) {
            let replayed = replay_completed_function_call(&replay_source);
            yield Ok::<Bytes, Infallible>(Bytes::from(replayed));
        }
        lifecycle.finish();
    };
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    if let Some(value) = content_type {
        if let Ok(value) = HeaderValue::from_bytes(value.as_bytes()) {
            response.headers_mut().insert(header::CONTENT_TYPE, value);
        }
    }
    response
}

fn immediate_fault(kind: &FaultKind) -> Option<Response> {
    if matches!(kind, FaultKind::NormalStop) {
        let body = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_eval_normal_stop\",\"object\":\"response\",\"status\":\"in_progress\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_eval_normal_stop\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":0,\"output_tokens\":0,\"total_tokens\":0}}}\n\n"
        );
        return Some(
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream")],
                body,
            )
                .into_response(),
        );
    }
    if matches!(kind, FaultKind::ContextLengthExceeded) {
        return Some(
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {
                    "message": "deterministic eval context window exceeded",
                    "type": "invalid_request_error",
                    "code": "context_length_exceeded"
                }})),
            )
                .into_response(),
        );
    }
    let status = match kind {
        FaultKind::Http429 => StatusCode::TOO_MANY_REQUESTS,
        FaultKind::Http500 => StatusCode::INTERNAL_SERVER_ERROR,
        FaultKind::Http524 => StatusCode::from_u16(524).expect("524 is valid"),
        _ => return None,
    };
    let mut response = (
        status,
        Json(json!({"error": {
            "message": format!("deterministic eval fault: {}", fault_name(kind)),
            "type": if matches!(kind, FaultKind::Http429) {
                "rate_limit_error"
            } else {
                "vellum_eval_fault"
            },
            "code": if matches!(kind, FaultKind::Http429) {
                "rate_limit_exceeded"
            } else {
                fault_name(kind)
            }
        }})),
    )
        .into_response();
    if matches!(kind, FaultKind::Http429) {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        response.headers_mut().insert(
            HeaderName::from_static("x-should-retry"),
            HeaderValue::from_static("true"),
        );
    }
    Some(response)
}

fn compact_fault(kind: &FaultKind) -> Option<Response> {
    match kind {
        FaultKind::CompactEmpty => Some(
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                r#"{"id":"eval-compact-empty","object":"response","output":[]}"#,
            )
                .into_response(),
        ),
        FaultKind::CompactNonJson => Some(
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/plain")],
                "deterministic non-JSON compact response",
            )
                .into_response(),
        ),
        _ => None,
    }
}

fn fault_name(kind: &FaultKind) -> &'static str {
    match kind {
        FaultKind::Http429 => "http_429",
        FaultKind::Http500 => "http_500",
        FaultKind::Http524 => "http_524",
        FaultKind::MalformedJson => "malformed_json",
        FaultKind::CompactEmpty => "compact_empty",
        FaultKind::CompactNonJson => "compact_non_json",
        FaultKind::TruncateSse => "truncate_sse",
        FaultKind::DropCompleted => "drop_completed",
        FaultKind::DuplicateDelta => "duplicate_delta",
        FaultKind::FragmentSse => "fragment_sse",
        FaultKind::ReplayToolCall => "replay_tool_call",
        FaultKind::ContextLengthExceeded => "context_length_exceeded",
        FaultKind::NormalStop => "normal_stop",
    }
}

fn persistent_fault_matches(
    fault: &FaultSpec,
    request_index: u32,
    total_estimated_tokens: Option<u64>,
) -> bool {
    request_index >= fault.at_request
        && fault
            .while_total_estimated_tokens_above
            .zip(total_estimated_tokens)
            .is_some_and(|(limit, actual)| actual > limit)
}

fn replay_completed_function_call(bytes: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(bytes);
    let mut output = String::with_capacity(text.len().saturating_add(512));
    let mut replayed = false;
    for block in text.split_inclusive("\n\n") {
        output.push_str(block);
        if !replayed
            && block.contains("response.output_item.done")
            && block.contains("\"type\":\"function_call\"")
        {
            output.push_str(block);
            replayed = true;
        }
    }
    output.into_bytes()
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

#[allow(clippy::too_many_arguments)]
fn trace(
    access: &TaskAccess,
    request_index: u32,
    method: &str,
    path: &str,
    model: Option<&str>,
    compaction: bool,
    status: u16,
    fault: Option<&str>,
    diagnostics: &RequestDiagnostics,
) {
    let metadata = model.and_then(|model| access.model_metadata.get(&model.to_ascii_lowercase()));
    let record = TraceRecord {
        event: "request",
        timestamp_ms: chrono::Utc::now().timestamp_millis(),
        task_id: &access.task_id,
        request_index,
        method,
        path,
        model,
        provider: metadata.map(|value| value.provider.as_str()),
        route_id: metadata.map(|value| value.route_id.as_str()),
        upstream_model: metadata.map(|value| value.upstream_model.as_str()),
        official_route: metadata.map(|value| value.is_official),
        harness_profile: metadata
            .map(|value| value.harness_profile.as_str())
            .filter(|profile| !profile.is_empty()),
        shell_contract: access.shell_contract.as_deref(),
        request_shape_hash: diagnostics.request_shape_hash.clone(),
        instructions_hash: diagnostics.instructions_hash.clone(),
        instructions_mark_plan_mode: diagnostics.instructions_mark_plan_mode,
        instructions_mark_default_mode: diagnostics.instructions_mark_default_mode,
        reasoning_effort: diagnostics.reasoning_effort.clone(),
        compaction,
        status,
        fault,
        fault_rule: diagnostics.fault_rule.clone(),
        fault_threshold_tokens: diagnostics.fault_threshold_tokens,
        session_hash: diagnostics.session_hash.clone(),
        input_item_types: diagnostics.input_item_types.clone(),
        tool_schema_hash: diagnostics.tool_schema_hash.clone(),
        tool_names: diagnostics.tool_names.clone(),
        tool_count: diagnostics.tool_count,
        malformed_tool_arguments: diagnostics.malformed_tool_arguments,
        reasoning_items: diagnostics.reasoning_items,
        encrypted_reasoning_bytes: diagnostics.encrypted_reasoning_bytes,
        encrypted_reasoning_hashes: diagnostics.encrypted_reasoning_hashes.clone(),
        grok_turn_index: diagnostics.grok_turn_index.clone(),
        request_model_visible_tokens: diagnostics.request_model_visible_tokens,
        request_input_tokens: diagnostics.request_input_tokens,
        request_instruction_tokens: diagnostics.request_instruction_tokens,
        request_tool_schema_tokens: diagnostics.request_tool_schema_tokens,
        request_total_estimated_tokens: diagnostics.request_total_estimated_tokens,
        terminal_sse: None,
        sse_event_count: None,
        response_bytes: None,
        input_tokens: None,
        output_tokens: None,
        disconnected: None,
        lifecycle: None,
    };
    if let Err(error) = append_jsonl(&access.trace_path, &record) {
        log::warn!("[Eval] cannot append gateway trace: {error}");
    }
}

#[derive(Debug, Clone, Default)]
struct RequestDiagnostics {
    request_shape_hash: Option<String>,
    instructions_hash: Option<String>,
    instructions_mark_plan_mode: bool,
    instructions_mark_default_mode: bool,
    reasoning_effort: Option<String>,
    session_hash: Option<String>,
    input_item_types: BTreeMap<String, u64>,
    tool_schema_hash: Option<String>,
    tool_names: Vec<String>,
    tool_count: u64,
    malformed_tool_arguments: u64,
    reasoning_items: u64,
    encrypted_reasoning_bytes: u64,
    encrypted_reasoning_hashes: Vec<String>,
    grok_turn_index: Option<String>,
    request_model_visible_tokens: Option<u64>,
    request_input_tokens: Option<u64>,
    request_instruction_tokens: Option<u64>,
    request_tool_schema_tokens: Option<u64>,
    request_total_estimated_tokens: Option<u64>,
    fault_rule: Option<String>,
    fault_threshold_tokens: Option<u64>,
}

struct StreamLifecycleGuard {
    access: Arc<TaskAccess>,
    request_index: u32,
    path: String,
    model: Option<String>,
    compaction: bool,
    status: u16,
    fault: Option<String>,
    diagnostics: RequestDiagnostics,
    terminal_sse: bool,
    sse_event_count: u64,
    response_bytes: u64,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    sse_buffer: String,
    disconnected: bool,
    finished: bool,
    token_reservation: Option<EvalTokenReservation>,
}

impl StreamLifecycleGuard {
    #[allow(clippy::too_many_arguments)]
    fn new(
        access: Arc<TaskAccess>,
        request_index: u32,
        path: String,
        model: Option<String>,
        compaction: bool,
        status: u16,
        fault: Option<String>,
        diagnostics: RequestDiagnostics,
        token_reservation: Option<EvalTokenReservation>,
    ) -> Self {
        Self {
            access,
            request_index,
            path,
            model,
            compaction,
            status,
            fault,
            diagnostics,
            terminal_sse: false,
            sse_event_count: 0,
            response_bytes: 0,
            input_tokens: None,
            output_tokens: None,
            sse_buffer: String::new(),
            disconnected: false,
            finished: false,
            token_reservation,
        }
    }

    fn trace(&self, event: &str, lifecycle: &str) {
        trace_stream_event(
            &self.access,
            event,
            self.request_index,
            &self.path,
            self.model.as_deref(),
            self.compaction,
            self.status,
            self.fault.as_deref(),
            &self.diagnostics,
            self.terminal_sse,
            self.sse_event_count,
            self.response_bytes,
            self.input_tokens,
            self.output_tokens,
            self.disconnected,
            lifecycle,
        );
    }

    fn finish(&mut self) {
        self.finalize_token_reservation();
        self.trace("stream_end", "upstream_eof");
        self.finished = true;
    }

    fn finalize_token_reservation(&mut self) {
        if let Some(reservation) = self.token_reservation.take() {
            reservation.finalize(self.input_tokens);
        }
    }

    fn observe_sse(&mut self, text: &str) {
        self.sse_buffer.push_str(text);
        while let Some((end, delimiter_len)) = next_sse_frame(&self.sse_buffer) {
            let frame = self.sse_buffer[..end].to_string();
            self.sse_buffer.drain(..end + delimiter_len);
            let data = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
                .collect::<Vec<_>>()
                .join("\n");
            let Ok(value) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            if let Some((input, output)) = response_usage(&value) {
                let prev_input = self.input_tokens.unwrap_or(0);
                self.input_tokens = Some(prev_input.max(input));
                self.output_tokens = Some(self.output_tokens.unwrap_or(0).max(output));
            }
        }
        if self.sse_buffer.len() > 2 * 1024 * 1024 {
            self.sse_buffer.clear();
        }
    }
}

fn next_sse_frame(buffer: &str) -> Option<(usize, usize)> {
    let lf = buffer.find("\n\n").map(|index| (index, 2));
    let crlf = buffer.find("\r\n\r\n").map(|index| (index, 4));
    match (lf, crlf) {
        (Some(left), Some(right)) => Some(if left.0 <= right.0 { left } else { right }),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn response_usage(value: &Value) -> Option<(u64, u64)> {
    let usage = value
        .pointer("/response/usage")
        .or_else(|| value.get("usage"))?;
    let input = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .and_then(Value::as_u64)?;
    let output = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some((input, output))
}

impl Drop for StreamLifecycleGuard {
    fn drop(&mut self) {
        self.finalize_token_reservation();
        if !self.finished {
            let lifecycle = if self.terminal_sse {
                "client_closed_after_terminal"
            } else if self.disconnected {
                "upstream_disconnected_before_terminal"
            } else {
                "client_closed_before_terminal"
            };
            self.trace("stream_dropped", lifecycle);
        }
    }
}

fn request_diagnostics(value: Option<&Value>, headers: &HeaderMap) -> RequestDiagnostics {
    let mut diagnostics = RequestDiagnostics {
        grok_turn_index: headers
            .get("x-grok-turn-idx")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
        ..RequestDiagnostics::default()
    };
    let Some(value) = value else {
        return diagnostics;
    };
    diagnostics.request_shape_hash = Some(crate::harness::snapshot::hash_value(value));
    diagnostics.reasoning_effort = value
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .or_else(|| value.get("reasoning_effort").and_then(Value::as_str))
        .map(str::to_string);
    let mut est_instruction = 0u64;
    if let Some(instructions) = value.get("instructions").and_then(Value::as_str) {
        let lower = instructions.to_ascii_lowercase();
        diagnostics.instructions_hash = Some(hash_identifier(instructions));
        diagnostics.instructions_mark_plan_mode = lower.contains("plan mode");
        diagnostics.instructions_mark_default_mode = lower.contains("default mode");
        est_instruction = vellum_proxy_runtime::compaction::estimate_tokens(&[
            serde_json::json!({"content": instructions}),
        ]);
        diagnostics.request_instruction_tokens = Some(est_instruction);
    }
    diagnostics.session_hash = [
        "previous_response_id",
        "conversation_id",
        "prompt_cache_key",
    ]
    .iter()
    .find_map(|key| value.get(*key).and_then(Value::as_str))
    .map(hash_identifier);
    let mut est_input = 0u64;
    if let Some(items) = value.get("input").and_then(Value::as_array) {
        let input_tokens = vellum_proxy_runtime::compaction::estimate_tokens(items);
        est_input = input_tokens;
        diagnostics.request_input_tokens = Some(input_tokens);
        diagnostics.request_model_visible_tokens =
            Some(vellum_proxy_runtime::compaction::estimate_tokens(
                &vellum_proxy_runtime::context_projection::project_model_visible_items(items),
            ));
        for item in items {
            let item_type = item
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            *diagnostics
                .input_item_types
                .entry(item_type.to_string())
                .or_default() += 1;
            if item_type == "reasoning" {
                diagnostics.reasoning_items += 1;
                if let Some(ciphertext) = item.get("encrypted_content").and_then(Value::as_str) {
                    diagnostics.encrypted_reasoning_bytes += ciphertext.len() as u64;
                    diagnostics
                        .encrypted_reasoning_hashes
                        .push(hash_identifier(ciphertext));
                }
            }
            if matches!(item_type, "function_call" | "custom_tool_call") {
                if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                    if serde_json::from_str::<Value>(arguments).is_err() {
                        diagnostics.malformed_tool_arguments += 1;
                    }
                }
            }
        }
    }
    if let Some(tools) = value.get("tools").and_then(Value::as_array) {
        diagnostics.tool_count = tools.len() as u64;
        diagnostics.tool_names = tools
            .iter()
            .filter_map(|tool| {
                tool.get("name")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        tool.get("function")
                            .and_then(|function| function.get("name"))
                            .and_then(Value::as_str)
                    })
                    .map(str::to_string)
            })
            .collect();
        // Canonicalized so the hash is an identity for the tool *contract*
        // (issue #6 Phase 0). Hashing the raw serialization made key order and
        // declaration order look like schema changes, which is exactly the
        // noise that made the translated surface hard to diff against Sol's.
        diagnostics.tool_schema_hash = Some(crate::harness::snapshot::hash_value(&Value::Array(
            tools.clone(),
        )));
        let est_tools = vellum_proxy_runtime::compaction::estimate_tokens(tools);
        diagnostics.request_tool_schema_tokens = Some(est_tools);
    }
    let est_tools = diagnostics.request_tool_schema_tokens.unwrap_or(0);
    diagnostics.request_total_estimated_tokens = Some(est_input + est_instruction + est_tools);
    diagnostics
}

fn hash_identifier(value: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(value.as_bytes()));
    digest[..16].to_string()
}

fn full_hash_identifier(value: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(value.as_bytes()))
}

#[allow(clippy::too_many_arguments)]
fn trace_stream_event(
    access: &TaskAccess,
    event: &str,
    request_index: u32,
    path: &str,
    model: Option<&str>,
    compaction: bool,
    status: u16,
    fault: Option<&str>,
    diagnostics: &RequestDiagnostics,
    terminal_sse: bool,
    sse_event_count: u64,
    response_bytes: u64,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    disconnected: bool,
    lifecycle: &str,
) {
    let metadata = model.and_then(|model| access.model_metadata.get(&model.to_ascii_lowercase()));
    let record = TraceRecord {
        event,
        timestamp_ms: chrono::Utc::now().timestamp_millis(),
        task_id: &access.task_id,
        request_index,
        method: "POST",
        path,
        model,
        provider: metadata.map(|value| value.provider.as_str()),
        route_id: metadata.map(|value| value.route_id.as_str()),
        upstream_model: metadata.map(|value| value.upstream_model.as_str()),
        official_route: metadata.map(|value| value.is_official),
        harness_profile: metadata
            .map(|value| value.harness_profile.as_str())
            .filter(|profile| !profile.is_empty()),
        shell_contract: access.shell_contract.as_deref(),
        request_shape_hash: diagnostics.request_shape_hash.clone(),
        instructions_hash: diagnostics.instructions_hash.clone(),
        instructions_mark_plan_mode: diagnostics.instructions_mark_plan_mode,
        instructions_mark_default_mode: diagnostics.instructions_mark_default_mode,
        reasoning_effort: diagnostics.reasoning_effort.clone(),
        compaction,
        status,
        fault,
        fault_rule: diagnostics.fault_rule.clone(),
        fault_threshold_tokens: diagnostics.fault_threshold_tokens,
        session_hash: diagnostics.session_hash.clone(),
        input_item_types: BTreeMap::new(),
        tool_schema_hash: None,
        tool_names: Vec::new(),
        tool_count: 0,
        malformed_tool_arguments: 0,
        reasoning_items: diagnostics.reasoning_items,
        encrypted_reasoning_bytes: diagnostics.encrypted_reasoning_bytes,
        encrypted_reasoning_hashes: diagnostics.encrypted_reasoning_hashes.clone(),
        grok_turn_index: diagnostics.grok_turn_index.clone(),
        request_model_visible_tokens: diagnostics.request_model_visible_tokens,
        request_input_tokens: diagnostics.request_input_tokens,
        request_instruction_tokens: diagnostics.request_instruction_tokens,
        request_tool_schema_tokens: diagnostics.request_tool_schema_tokens,
        request_total_estimated_tokens: diagnostics.request_total_estimated_tokens,
        terminal_sse: Some(terminal_sse),
        sse_event_count: Some(sse_event_count),
        response_bytes: Some(response_bytes),
        input_tokens,
        output_tokens,
        disconnected: Some(disconnected),
        lifecycle: Some(lifecycle),
    };
    if let Err(error) = append_jsonl(&access.trace_path, &record) {
        log::warn!("[Eval] cannot append gateway stream trace: {error}");
    }
}

fn has_compaction_trigger(value: &Value) -> bool {
    value
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction_trigger"))
        })
}

fn append_jsonl(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")
}

pub fn redact_text(input: &str) -> String {
    input
        .split_inclusive('\n')
        .map(|line| {
            let (content, newline) = line
                .strip_suffix('\n')
                .map(|content| (content, "\n"))
                .unwrap_or((line, ""));
            if let Ok(mut value) = serde_json::from_str::<Value>(content) {
                redact_json(&mut value);
                format!(
                    "{}{newline}",
                    serde_json::to_string(&value).unwrap_or_else(|_| "{}".into())
                )
            } else {
                format!("{}{newline}", redact_plain_text(content))
            }
        })
        .collect()
}

pub fn redact_text_with_secrets(input: &str, secrets: &[&str]) -> String {
    let mut output = redact_text(input);
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        output = output.replace(secret, "[REDACTED]");
    }
    output
}

fn redact_json(value: &mut Value) {
    redact_json_inner(value, false);
}

fn redact_json_inner(value: &mut Value, inside_reasoning: bool) {
    match value {
        Value::Object(object) => {
            let object_is_reasoning = inside_reasoning
                || object
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| {
                        kind == "reasoning" || kind == "reasoning_content" || kind == "summary_text"
                    });
            for (key, value) in object {
                let normalized = key.to_ascii_lowercase().replace('-', "_");
                if matches!(
                    normalized.as_str(),
                    "authorization"
                        | "proxy_authorization"
                        | "cookie"
                        | "set_cookie"
                        | "api_key"
                        | "apikey"
                        | "access_token"
                        | "refresh_token"
                        | "id_token"
                        | "client_secret"
                        | "x_api_key"
                        | "x_xai_token"
                        | "reasoning_content"
                        | "encrypted_content"
                ) {
                    *value = Value::String("[REDACTED]".into());
                } else if object_is_reasoning
                    && matches!(normalized.as_str(), "text" | "content" | "summary")
                {
                    *value = Value::String("[REASONING_REDACTED]".into());
                } else {
                    redact_json_inner(value, object_is_reasoning);
                }
            }
        }
        Value::Array(array) => {
            for value in array {
                redact_json_inner(value, inside_reasoning);
            }
        }
        Value::String(text) => {
            *text = if inside_reasoning {
                "[REASONING_REDACTED]".into()
            } else {
                redact_plain_text(text)
            }
        }
        _ => {}
    }
}

fn redact_plain_text(input: &str) -> String {
    let mut output = input.to_string();
    loop {
        let lower = output.to_ascii_lowercase();
        let Some(start) = lower.find("<think>") else {
            break;
        };
        let end = lower[start + 7..]
            .find("</think>")
            .map(|offset| start + 7 + offset + 8)
            .unwrap_or(output.len());
        output.replace_range(start..end, "[REASONING_REDACTED]");
    }
    for marker in [
        "authorization:",
        "\"authorization\":",
        "cookie:",
        "\"cookie\":",
        "bearer ",
        "api_key=",
        "api-key=",
        "access_token=",
        "refresh_token=",
        "client_secret=",
    ] {
        let mut cursor = 0;
        loop {
            let lower = output.to_ascii_lowercase();
            let Some(relative) = lower[cursor..].find(marker) else {
                break;
            };
            let marker_index = cursor + relative;
            let mut start = marker_index + marker.len();
            while output[start..]
                .chars()
                .next()
                .map(char::is_whitespace)
                .unwrap_or(false)
                && !output[start..].starts_with('\n')
                && !output[start..].starts_with('\r')
            {
                start += output[start..].chars().next().unwrap().len_utf8();
            }
            let end = output[start..]
                .find(['\r', '\n', '"', '\'', ',', '}'])
                .map(|offset| start + offset)
                .unwrap_or(output.len());
            if end > start {
                output.replace_range(start..end, "[REDACTED]");
                cursor = start + "[REDACTED]".len();
            } else {
                cursor = start;
            }
            if cursor >= output.len() {
                break;
            }
        }
    }
    for prefix in ["nvapi-", "sk-", "xai-"] {
        let mut cursor = 0;
        loop {
            let lower = output.to_ascii_lowercase();
            let Some(relative) = lower[cursor..].find(prefix) else {
                break;
            };
            let start = cursor + relative;
            let has_token_boundary = start == 0
                || output[..start]
                    .chars()
                    .next_back()
                    .map(|character| {
                        !character.is_ascii_alphanumeric() && !matches!(character, '_' | '-')
                    })
                    .unwrap_or(true);
            if !has_token_boundary {
                cursor = start + prefix.len();
                continue;
            }
            let end = output[start..]
                .find(|character: char| {
                    character.is_whitespace()
                        || matches!(character, '"' | '\'' | ',' | '}' | ']' | '\\')
                })
                .map(|offset| start + offset)
                .unwrap_or(output.len());
            output.replace_range(start..end, "[REDACTED]");
            cursor = start + "[REDACTED]".len();
        }
    }
    output
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]

    use super::*;

    #[test]
    fn redacts_credentials() {
        let text = concat!(
            "Authorization: Bearer secret-token\n",
            "Cookie: session=abc\n",
            "api_key=nvapi-secret value\n",
            "{\"access_token\":\"oauth-secret\",\"nested\":{\"client_secret\":\"client-value\"}}\n",
            "{\"message\":\"provider rejected nvapi-raw-secret\"}"
        );
        let redacted = redact_text(text);
        assert!(!redacted.contains("secret-token"));
        assert!(!redacted.contains("session=abc"));
        assert!(!redacted.contains("nvapi-secret"));
        assert!(!redacted.contains("oauth-secret"));
        assert!(!redacted.contains("client-value"));
        assert!(!redacted.contains("nvapi-raw-secret"));
        assert!(redacted.contains("[REDACTED]"));
        assert_eq!(
            redact_text_with_secrets("opaque task-token", &["task-token"]),
            "opaque [REDACTED]"
        );
        let reasoning = redact_text(
            "{\"type\":\"reasoning\",\"encrypted_content\":\"opaque\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"private chain\"}]}",
        );
        assert!(!reasoning.contains("opaque"));
        assert!(!reasoning.contains("private chain"));
        assert!(reasoning.contains("[REASONING_REDACTED]"));
    }

    #[test]
    fn immediate_faults_are_deterministic() {
        assert_eq!(
            immediate_fault(&FaultKind::Http429).unwrap().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert!(immediate_fault(&FaultKind::DropCompleted).is_none());
        assert!(immediate_fault(&FaultKind::CompactEmpty).is_none());
        assert_eq!(
            compact_fault(&FaultKind::CompactEmpty).unwrap().status(),
            StatusCode::OK
        );
        assert!(compact_fault(&FaultKind::Http500).is_none());
        assert_eq!(fault_name(&FaultKind::FragmentSse), "fragment_sse");
        assert_eq!(
            immediate_fault(&FaultKind::ContextLengthExceeded)
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            immediate_fault(&FaultKind::NormalStop).unwrap().status(),
            StatusCode::OK
        );
    }

    #[test]
    fn persistent_context_fault_rejects_until_total_estimate_shrinks() {
        let fault = FaultSpec {
            at_request: 2,
            kind: FaultKind::ContextLengthExceeded,
            while_total_estimated_tokens_above: Some(10_000),
        };
        assert!(!persistent_fault_matches(&fault, 1, Some(20_000)));
        assert!(persistent_fault_matches(&fault, 2, Some(20_000)));
        assert!(persistent_fault_matches(&fault, 3, Some(10_001)));
        assert!(!persistent_fault_matches(&fault, 4, Some(10_000)));
        assert!(!persistent_fault_matches(&fault, 5, Some(9_999)));
        assert!(!persistent_fault_matches(&fault, 6, None));
    }

    #[test]
    fn replay_fault_duplicates_only_completed_function_call_item() {
        let input = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call-1\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\"}\n\n"
        );
        let replayed = String::from_utf8(replay_completed_function_call(input.as_bytes())).unwrap();
        assert_eq!(replayed.matches("\"call_id\":\"call-1\"").count(), 2);
        assert_eq!(replayed.matches("event: response.completed").count(), 1);
    }

    #[test]
    fn terminal_sse_usage_accepts_responses_and_chat_shapes() {
        assert_eq!(
            response_usage(&json!({
                "type": "response.completed",
                "response": {"usage": {"input_tokens": 120, "output_tokens": 30}}
            })),
            Some((120, 30))
        );
        assert_eq!(
            response_usage(&json!({
                "usage": {"prompt_tokens": 9, "completion_tokens": 4}
            })),
            Some((9, 4))
        );
        assert_eq!(
            next_sse_frame("event: x\r\ndata: {}\r\n\r\nrest"),
            Some((18, 4))
        );
    }

    #[test]
    fn reasoning_trace_keeps_only_ciphertext_size_and_hash() {
        let ciphertext = "opaque-provider-secret";
        let diagnostics = request_diagnostics(
            Some(&json!({
                "input": [{
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "private"}],
                    "encrypted_content": ciphertext
                }]
            })),
            &HeaderMap::new(),
        );
        assert_eq!(diagnostics.reasoning_items, 1);
        assert_eq!(
            diagnostics.encrypted_reasoning_bytes,
            ciphertext.len() as u64
        );
        assert_eq!(
            diagnostics.encrypted_reasoning_hashes,
            vec![hash_identifier(ciphertext)]
        );
        let encoded = serde_json::to_string(&diagnostics.encrypted_reasoning_hashes).unwrap();
        assert!(!encoded.contains(ciphertext));
        assert!(!encoded.contains("private"));
    }

    fn test_gateway_state(temp: &tempfile::TempDir) -> AppState {
        AppState::with_test_fixtures(temp.path().into())
    }

    fn test_boundary_key() -> vellum_proxy_runtime::BoundaryKey {
        vellum_proxy_runtime::BoundaryKey::generate().expect("fixture boundary key")
    }

    #[tokio::test]
    async fn gateway_requires_token_and_enforces_model_allowlist() {
        let temp = tempfile::tempdir().unwrap();
        let campaign = temp.path().join("campaign");
        let gateway =
            EvalGateway::start_for_test(test_gateway_state(&temp), campaign, test_boundary_key())
                .await
                .unwrap();
        let credential = gateway
            .register_task(
                "case-1",
                ["allowed-model".to_string()],
                &[],
                temp.path().join("trace.jsonl"),
            )
            .await
            .unwrap();
        let client = reqwest::Client::new();
        let root = format!("http://127.0.0.1:{}", gateway.port());
        let unauthorized = client
            .get(format!("{root}/v1/models"))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
        let models: Value = client
            .get(format!("{root}/v1/models"))
            .bearer_auth(&credential.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(models["data"][0]["id"], "allowed-model");
        let forbidden = client
            .post(format!("{root}/v1/responses"))
            .bearer_auth(&credential.token)
            .json(&json!({"model": "other-model", "input": "hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(forbidden.status(), reqwest::StatusCode::FORBIDDEN);
        gateway.shutdown().await;
    }

    #[tokio::test]
    async fn registered_fault_is_applied_at_exact_request() {
        let temp = tempfile::tempdir().unwrap();
        let campaign = temp.path().join("campaign");
        let gateway =
            EvalGateway::start_for_test(test_gateway_state(&temp), campaign, test_boundary_key())
                .await
                .unwrap();
        let credential = gateway
            .register_task(
                "case-fault",
                ["allowed-model".to_string()],
                &[FaultSpec {
                    at_request: 1,
                    kind: FaultKind::Http429,
                    while_total_estimated_tokens_above: None,
                }],
                temp.path().join("trace.jsonl"),
            )
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}/v1/responses", gateway.port()))
            .bearer_auth(&credential.token)
            .json(&json!({"model": "allowed-model", "input": "hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        gateway.shutdown().await;
    }

    #[tokio::test]
    async fn task_stall_watchdog_refuses_the_next_provider_turn() {
        let temp = tempfile::tempdir().unwrap();
        let app_state = test_gateway_state(&temp);
        let usage = app_state.usage_store();
        let gateway = EvalGateway::start_for_test(
            app_state,
            temp.path().join("campaign"),
            test_boundary_key(),
        )
        .await
        .unwrap();
        let credential = gateway
            .register_task(
                "case-stall-watchdog",
                ["allowed-model".to_string()],
                &[FaultSpec {
                    at_request: 1,
                    kind: FaultKind::Http500,
                    while_total_estimated_tokens_above: None,
                }],
                temp.path().join("trace.jsonl"),
            )
            .await
            .unwrap();
        let client = reqwest::Client::new();
        let root = format!("http://127.0.0.1:{}", gateway.port());
        let first = client
            .post(format!("{root}/v1/responses"))
            .bearer_auth(&credential.token)
            .json(&json!({"model": "allowed-model", "input": "hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), reqwest::StatusCode::INTERNAL_SERVER_ERROR);

        let mut stall = vellum_proxy_runtime::task_stall::TaskStallState::default();
        stall.recovery_total = 1;
        stall.epoch_recovery_level = 1;
        stall.recovery_state = vellum_proxy_runtime::task_stall::StallRecoveryState::Warned;
        stall.tool_results_since_progress = 24;
        stall.post_recovery_tool_results_without_progress = 8;
        usage
            .record_diagnostic_event(&vellum_proxy_runtime::DiagnosticEvent::TaskStall(
                vellum_proxy_runtime::task_stall::TaskStallDiagnostic::from_state(
                    &stall,
                    &vellum_proxy_runtime::task_stall::TaskStallPolicy::recover(),
                    4,
                    true,
                    Some(credential.runtime_request_id_hash(1)),
                ),
            ))
            .unwrap();
        let terminal_cursor = usage.diagnostic_cursor().unwrap();

        let stopped = client
            .post(format!("{root}/v1/responses"))
            .bearer_auth(&credential.token)
            .json(&json!({"model": "allowed-model", "input": "continue"}))
            .send()
            .await
            .unwrap();
        assert_eq!(stopped.status(), reqwest::StatusCode::CONFLICT);
        assert!(stopped
            .text()
            .await
            .unwrap()
            .contains("model_no_progress_after_recovery"));
        let terminals = usage.task_stall_terminals_after(terminal_cursor).unwrap();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].request_index, Some(2));
        gateway.shutdown().await;
    }

    #[tokio::test]
    async fn luna_task_registration_is_blocked_below_thirty_percent_remaining() {
        let temp = tempfile::tempdir().unwrap();
        let campaign = temp.path().join("live-campaign");
        vellum_proxy_runtime::OfficialTurnBudget {
            spent: (vellum_proxy_runtime::OFFICIAL_LIVE_TURN_CAP as f64 * 0.72).ceil() as u32,
            cap: vellum_proxy_runtime::OFFICIAL_LIVE_TURN_CAP,
            stopped: None,
        }
        .save(&campaign)
        .unwrap();
        let gateway = EvalGateway::start_for_test(
            test_gateway_state(&temp),
            campaign.clone(),
            test_boundary_key(),
        )
        .await
        .unwrap();
        let error = gateway
            .register_task(
                "luna-swe-blocked",
                [vellum_proxy_runtime::PINNED_OFFICIAL_MODEL.to_string()],
                &[],
                temp.path().join("trace.jsonl"),
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("Luna SWE blocked"),
            "28% remaining must refuse to launch Luna SWE: {error}"
        );
        // A non-Luna task is unaffected by the ledger.
        gateway
            .register_task(
                "plain-task",
                ["allowed-model".to_string()],
                &[],
                temp.path().join("trace2.jsonl"),
            )
            .await
            .unwrap();
        gateway.shutdown().await;
    }

    #[tokio::test]
    async fn luna_swe_turns_admit_the_campaign_ledger_before_forwarding() {
        let temp = tempfile::tempdir().unwrap();
        let campaign = temp.path().join("live-campaign");
        let gateway = EvalGateway::start_for_test(
            test_gateway_state(&temp),
            campaign.clone(),
            test_boundary_key(),
        )
        .await
        .unwrap();
        let credential = gateway
            .register_task(
                "luna-swe-admit",
                [vellum_proxy_runtime::PINNED_OFFICIAL_MODEL.to_string()],
                &[],
                temp.path().join("trace.jsonl"),
            )
            .await
            .unwrap();
        let client = reqwest::Client::new();
        let root = format!("http://127.0.0.1:{}", gateway.port());
        for _ in 0..3 {
            let response = client
                .post(format!("{root}/v1/responses"))
                .bearer_auth(&credential.token)
                .json(&json!({
                    "model": vellum_proxy_runtime::PINNED_OFFICIAL_MODEL,
                    "input": "hi"
                }))
                .send()
                .await
                .unwrap();
            // The backend has no route for Luna, but the admit already
            // happened before the forward — that is the point.
            assert_ne!(response.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        }
        let budget = vellum_proxy_runtime::OfficialTurnBudget::load(&campaign).unwrap();
        assert_eq!(
            budget.spent, 3,
            "each forwarded Luna SWE turn must spend the shared ledger"
        );
        gateway.shutdown().await;
    }

    #[tokio::test]
    async fn luna_swe_turn_cap_refuses_the_thirty_first_request_before_send() {
        let temp = tempfile::tempdir().unwrap();
        let campaign = temp.path().join("live-campaign");
        let gateway = EvalGateway::start_for_test(
            test_gateway_state(&temp),
            campaign.clone(),
            test_boundary_key(),
        )
        .await
        .unwrap();
        let credential = gateway
            .register_task(
                "luna-swe-cap",
                [vellum_proxy_runtime::PINNED_OFFICIAL_MODEL.to_string()],
                &[],
                temp.path().join("trace.jsonl"),
            )
            .await
            .unwrap();
        let client = reqwest::Client::new();
        let root = format!("http://127.0.0.1:{}", gateway.port());
        for turn in 1..=SWE_MAX_TURNS_PER_TASK {
            let response = client
                .post(format!("{root}/v1/responses"))
                .bearer_auth(&credential.token)
                .json(&json!({
                    "model": vellum_proxy_runtime::PINNED_OFFICIAL_MODEL,
                    "input": "hi"
                }))
                .send()
                .await
                .unwrap();
            assert_ne!(
                response.status(),
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                "turn {turn} is within the per-task cap and must be forwarded"
            );
        }
        let refused = client
            .post(format!("{root}/v1/responses"))
            .bearer_auth(&credential.token)
            .json(&json!({
                "model": vellum_proxy_runtime::PINNED_OFFICIAL_MODEL,
                "input": "hi"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            refused.status(),
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "the 31st Luna SWE turn must be refused before the send"
        );
        let body: Value = refused.json().await.unwrap();
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("turn cap exceeded")),
            "{body}"
        );
        let budget = vellum_proxy_runtime::OfficialTurnBudget::load(&campaign).unwrap();
        assert_eq!(
            budget.spent, 30,
            "the refused turn must never spend the ledger"
        );
        gateway.shutdown().await;
    }

    #[tokio::test]
    async fn non_luna_swe_turn_cap_also_refuses_the_thirty_first_request() {
        let temp = tempfile::tempdir().unwrap();
        let gateway = EvalGateway::start_for_test(
            test_gateway_state(&temp),
            temp.path().join("campaign"),
            test_boundary_key(),
        )
        .await
        .unwrap();
        let credential = gateway
            .register_task(
                "grok-swe-cap",
                ["allowed-model".to_string()],
                &[],
                temp.path().join("trace.jsonl"),
            )
            .await
            .unwrap();
        let client = reqwest::Client::new();
        let root = format!("http://127.0.0.1:{}", gateway.port());
        for turn in 1..=SWE_MAX_TURNS_PER_TASK {
            let response = client
                .post(format!("{root}/v1/responses"))
                .bearer_auth(&credential.token)
                .json(&json!({"model": "allowed-model", "input": "hi"}))
                .send()
                .await
                .unwrap();
            assert_ne!(
                response.status(),
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                "non-Luna turn {turn} is within the shared SWE cap"
            );
        }
        let compact = client
            .post(format!("{root}/v1/responses"))
            .bearer_auth(&credential.token)
            .json(&json!({
                "model": "allowed-model",
                "input": [{"type": "compaction_trigger"}]
            }))
            .send()
            .await
            .unwrap();
        assert_ne!(
            compact.status(),
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "compaction is auxiliary and must not spend a model turn"
        );
        let refused = client
            .post(format!("{root}/v1/responses"))
            .bearer_auth(&credential.token)
            .json(&json!({"model": "allowed-model", "input": "hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        let body: Value = refused.json().await.unwrap();
        assert!(body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("SWE turn cap exceeded")));
        gateway.shutdown().await;
    }

    #[tokio::test]
    async fn swe_retry_credentials_share_one_turn_cap() {
        let temp = tempfile::tempdir().unwrap();
        let gateway = EvalGateway::start_for_test(
            test_gateway_state(&temp),
            temp.path().join("campaign"),
            test_boundary_key(),
        )
        .await
        .unwrap();
        let budget = SweTurnBudget::new(2);
        let first = gateway
            .register_task_with_runtime_metadata(
                "swe-retry",
                ["allowed-model".to_string()],
                &[],
                temp.path().join("trace.jsonl"),
                HashMap::new(),
                None,
                Some(budget.clone()),
            )
            .await
            .unwrap();
        let second = gateway
            .register_task_with_runtime_metadata(
                "swe-retry",
                ["allowed-model".to_string()],
                &[],
                temp.path().join("trace.jsonl"),
                HashMap::new(),
                None,
                Some(budget),
            )
            .await
            .unwrap();
        let client = reqwest::Client::new();
        let root = format!("http://127.0.0.1:{}", gateway.port());
        for credential in [&first, &second] {
            let response = client
                .post(format!("{root}/v1/responses"))
                .bearer_auth(&credential.token)
                .json(&json!({"model": "allowed-model", "input": "hi"}))
                .send()
                .await
                .unwrap();
            assert_ne!(response.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        }
        let refused = client
            .post(format!("{root}/v1/responses"))
            .bearer_auth(&second.token)
            .json(&json!({"model": "allowed-model", "input": "hi"}))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);

        let trace = std::fs::read_to_string(temp.path().join("trace.jsonl")).unwrap();
        assert!(trace.contains(r#""requestIndex":1"#));
        assert!(trace.contains(r#""requestIndex":2"#));
        assert!(trace.contains(r#""requestIndex":3"#));
        gateway.shutdown().await;
    }

    #[tokio::test]
    async fn single_run_official_turn_cap_refuses_traffic() {
        let temp = tempfile::tempdir().unwrap();
        let ledger_dir = temp.path().join("ledger");
        let gateway = EvalGateway::start_for_test_with_turn_budget(
            test_gateway_state(&temp),
            ledger_dir,
            test_boundary_key(),
            Some(1),
        )
        .await
        .unwrap();

        let mut model_metadata = HashMap::new();
        model_metadata.insert(
            "luna-official".to_string(),
            TraceModelMetadata {
                provider: "official".into(),
                route_id: "official".into(),
                upstream_model: "gpt-5.6-luna".into(),
                is_official: true,
                harness_profile: "openai".into(),
            },
        );

        let credential = gateway
            .register_task_with_runtime_metadata(
                "official-task",
                ["luna-official".to_string()],
                &[],
                temp.path().join("trace.jsonl"),
                model_metadata,
                None,
                None,
            )
            .await
            .unwrap();

        let client = reqwest::Client::new();
        let root = format!("http://127.0.0.1:{}", gateway.port());

        // First request is admitted and incremented (turns_spent = 1)
        let _ = client
            .post(format!("{root}/v1/responses"))
            .bearer_auth(&credential.token)
            .json(&json!({"model": "luna-official", "input": "turn 1"}))
            .send()
            .await;

        assert_eq!(gateway.official_turns_spent(), 1);

        // Second request exceeds single-run cap of 1 -> returns 429
        let refused = client
            .post(format!("{root}/v1/responses"))
            .bearer_auth(&credential.token)
            .json(&json!({"model": "luna-official", "input": "turn 2"}))
            .send()
            .await
            .unwrap();

        assert_eq!(refused.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        let body: Value = refused.json().await.unwrap();
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Single-run Official turn limit reached"));
        gateway.shutdown().await;
    }

    #[tokio::test]
    async fn concurrent_official_turn_admission_never_exceeds_cap() {
        let temp = tempfile::tempdir().unwrap();
        let ledger_dir = temp.path().join("ledger");
        let gateway = EvalGateway::start_for_test_with_turn_budget(
            test_gateway_state(&temp),
            ledger_dir,
            test_boundary_key(),
            Some(2),
        )
        .await
        .unwrap();

        let mut model_metadata = HashMap::new();
        model_metadata.insert(
            "gpt-5".to_string(),
            TraceModelMetadata {
                provider: "OpenAI Official".into(),
                route_id: "openai_official".into(),
                upstream_model: "gpt-5-codex".into(),
                is_official: true,
                harness_profile: "openai".into(),
            },
        );

        let credential = gateway
            .register_task_with_runtime_metadata(
                "concurrent-official-task",
                ["gpt-5".to_string()],
                &[],
                temp.path().join("trace.jsonl"),
                model_metadata,
                None,
                None,
            )
            .await
            .unwrap();

        let client = reqwest::Client::new();
        let root = format!("http://127.0.0.1:{}", gateway.port());

        // Spawn 10 concurrent requests
        let mut handles = Vec::new();
        for i in 0..10 {
            let client = client.clone();
            let root = root.clone();
            let token = credential.token.clone();
            handles.push(tokio::spawn(async move {
                client
                    .post(format!("{root}/v1/responses"))
                    .bearer_auth(&token)
                    .json(&json!({"model": "gpt-5", "input": format!("turn {i}")}))
                    .send()
                    .await
            }));
        }

        let mut success_count = 0;
        let mut rate_limited_count = 0;
        for handle in handles {
            if let Ok(Ok(response)) = handle.await {
                if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    rate_limited_count += 1;
                } else {
                    success_count += 1;
                }
            }
        }

        assert_eq!(
            success_count, 2,
            "exactly 2 concurrent requests must be admitted"
        );
        assert_eq!(
            rate_limited_count, 8,
            "exactly 8 concurrent requests must be rejected with 429"
        );
        assert_eq!(gateway.official_turns_spent(), 2);
        gateway.shutdown().await;
    }

    #[test]
    fn authoritative_route_metadata_identifies_official_routes() {
        let meta1 = TraceModelMetadata {
            provider: "OpenAI Official".into(),
            route_id: "openai_official".into(),
            upstream_model: "gpt-5-codex".into(),
            is_official: true,
            harness_profile: "openai".into(),
        };
        assert!(is_official_route_metadata(&meta1));

        let meta2 = TraceModelMetadata {
            provider: "Grok".into(),
            route_id: "grok-direct".into(),
            upstream_model: "grok-3".into(),
            is_official: false,
            harness_profile: "grok".into(),
        };
        assert!(!is_official_route_metadata(&meta2));

        let meta3 = TraceModelMetadata {
            provider: "Luna".into(),
            route_id: "luna".into(),
            upstream_model: "gpt-5.6-luna".into(),
            is_official: true,
            harness_profile: "openai".into(),
        };
        assert!(is_official_route_metadata(&meta3));
    }

    #[tokio::test]
    async fn test_eval_token_budget_proactive_hard_stop() {
        let temp = tempfile::tempdir().unwrap();
        let ledger_dir = temp.path().join("ledger");
        let gateway =
            EvalGateway::start_for_test(test_gateway_state(&temp), ledger_dir, test_boundary_key())
                .await
                .unwrap();

        let mut model_metadata = HashMap::new();
        model_metadata.insert(
            "gpt-5".to_string(),
            TraceModelMetadata {
                provider: "OpenAI Official".into(),
                route_id: "openai_official".into(),
                upstream_model: "gpt-5-codex".into(),
                is_official: true,
                harness_profile: "openai".into(),
            },
        );

        let token_budget = EvalTokenBudget::new(50_000);
        // Pre-spend 35_000 tokens
        token_budget
            .cumulative_input_tokens
            .store(35_000, Ordering::SeqCst);

        let credential = gateway
            .register_task_with_budgets(
                "token-budget-test",
                ["gpt-5".to_string()],
                &[],
                temp.path().join("trace.jsonl"),
                model_metadata,
                None,
                None,
                Some(token_budget.clone()),
            )
            .await
            .unwrap();

        let client = reqwest::Client::new();

        // Send a request with ~80,000 characters of prompt (> 20,000 estimated tokens).
        // Since spent (35,000) + projected (>20,000) > limit (50,000),
        // the proactive gate must refuse the request before dispatch with HTTP 429!
        let large_content = "X".repeat(80_000);
        let resp = client
            .post(format!("http://127.0.0.1:{}/v1/responses", gateway.port()))
            .bearer_auth(&credential.token)
            .json(&serde_json::json!({
                "model": "gpt-5",
                "input": [{"type": "message", "role": "user", "content": large_content}]
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        let body: Value = resp.json().await.unwrap();
        let err_msg = body["error"]["message"].as_str().unwrap();
        assert!(
            err_msg.contains("Eval token limit exceeded"),
            "Error message must indicate limit exceeded: {}",
            err_msg
        );
        assert!(
            err_msg.contains("request refused before dispatch"),
            "Error message must specify refused before dispatch: {}",
            err_msg
        );

        gateway.shutdown().await;
    }

    #[tokio::test]
    async fn concurrent_eval_token_reservations_cannot_cross_cap() {
        let budget = EvalTokenBudget::new(1_000);
        let mut joins = Vec::new();
        for _ in 0..10 {
            let budget = budget.clone();
            joins.push(tokio::spawn(async move { budget.try_reserve(300).is_ok() }));
        }
        let mut admitted = 0;
        for join in joins {
            admitted += usize::from(join.await.unwrap());
        }

        assert_eq!(admitted, 3);
        assert_eq!(budget.reserved_input_tokens.load(Ordering::SeqCst), 900);
        assert_eq!(budget.cumulative_input_tokens.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn response_without_usage_is_charged_at_projected_reservation() {
        let budget = EvalTokenBudget::new(1_000);
        budget.try_reserve(400).unwrap();

        budget.finalize_reservation(400, None);

        assert_eq!(budget.reserved_input_tokens.load(Ordering::SeqCst), 0);
        assert_eq!(budget.cumulative_input_tokens.load(Ordering::SeqCst), 400);
        assert!(budget.try_reserve(700).is_err());
    }

    #[test]
    fn dropped_reservation_releases_before_dispatch_and_charges_after_dispatch() {
        let budget = EvalTokenBudget::new(1_000);
        budget.try_reserve(300).unwrap();
        drop(EvalTokenReservation::new(budget.clone(), 300));
        assert_eq!(budget.reserved_input_tokens.load(Ordering::SeqCst), 0);
        assert_eq!(budget.cumulative_input_tokens.load(Ordering::SeqCst), 0);

        budget.try_reserve(400).unwrap();
        let mut dispatched = EvalTokenReservation::new(budget.clone(), 400);
        dispatched.mark_dispatched();
        drop(dispatched);
        assert_eq!(budget.reserved_input_tokens.load(Ordering::SeqCst), 0);
        assert_eq!(budget.cumulative_input_tokens.load(Ordering::SeqCst), 400);
    }
}
