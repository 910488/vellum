//! The App Server child the bridge-mode gate runs on both planes.
//!
//! It is a real stdio App Server peer: the bridge cannot tell it apart from
//! Codex, because it is spoken to over the same protocol on the same pipes. Its
//! agent loop is deliberately small — one provider call, tools, stop — but the
//! three Enhanced ports it runs are the *actual* portable modules that are meant
//! to land in the Codex fork, not reimplementations. That is what makes the port
//! behaviour cases evidence rather than theatre.
//!
//! On the Official plane every port is off, so this child is an E0 baseline and
//! must behave exactly like an unmodified runtime.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use vellum_enhanced_codex::{event_notification, identity_notification};
use vellum_enhanced_codex::{
    AdmitDecision, CompactDecision, ContentBlock, ContinuationDecision, EnhancedEvent,
    EnhancedRuntimeFeatures, EnhancedTurnHooks, HookDecision, MemoryTelemetry, ModelVisibleSurface,
    OverflowDecision, SurfaceItem, ToolCallLedger, ToolCallResolution, ToolResultPrunePolicy,
    TurnStopContext, UnfinishedSignal,
};

use super::provider::THREAD_HEADER;
use super::scenarios::{
    GateCatalog, GateModel, GATE_CATALOG_ENV, GATE_PROVIDER_URL_ENV, GATE_WORKSPACE_ENV,
};

const THREAD_STORE_FILE: &str = "gate-threads.json";
const SIDE_EFFECT_LOG: &str = "side-effects.log";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plane {
    Official,
    Enhanced,
}

impl Plane {
    fn from_env() -> Self {
        match std::env::var("VELLUM_EXECUTION_PLANE").as_deref() {
            Ok("enhanced-codex") => Self::Enhanced,
            _ => Self::Official,
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Official => "official",
            Self::Enhanced => "enhanced",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadRecord {
    model: String,
    model_provider: String,
    runtime_digest: String,
    items: Vec<SurfaceItem>,
    generation: u64,
    ledger: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadStore {
    next_thread: u64,
    threads: HashMap<String, ThreadRecord>,
}

struct GateChild {
    plane: Plane,
    runtime_digest: String,
    enhanced_commit: String,
    features: EnhancedRuntimeFeatures,
    feature_profile: String,
    catalog: GateCatalog,
    provider_url: String,
    workspace: PathBuf,
    store_path: PathBuf,
    store: Mutex<ThreadStore>,
    out: Mutex<std::io::Stdout>,
    cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
    steers: Mutex<HashMap<String, Arc<AtomicBool>>>,
    running_turns: Mutex<HashMap<String, u64>>,
    runtime: tokio::runtime::Runtime,
}

pub fn run() -> std::io::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if !args.iter().any(|arg| arg == "app-server") {
        // Mirrors Codex: a non-`app-server` invocation is a one-shot command.
        println!("vellum-codex-gate-child 1.0.0");
        return Ok(());
    }
    let child = Arc::new(GateChild::from_env()?);
    if child.plane == Plane::Enhanced {
        child.announce_identity();
    }
    let stdin = std::io::stdin();
    let mut handles = Vec::new();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(handle) = child.dispatch(value) {
            handles.push(handle);
        }
    }
    for handle in handles {
        let _ = handle.join();
    }
    Ok(())
}

impl GateChild {
    fn from_env() -> std::io::Result<Self> {
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .ok_or_else(|| std::io::Error::other("CODEX_HOME is required"))?;
        let catalog_path = std::env::var_os(GATE_CATALOG_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| GateCatalog::path_in(&codex_home));
        let catalog = GateCatalog::read(&catalog_path)?;
        let provider_url = std::env::var(GATE_PROVIDER_URL_ENV)
            .map_err(|_| std::io::Error::other(format!("{GATE_PROVIDER_URL_ENV} is required")))?;
        let workspace = std::env::var_os(GATE_WORKSPACE_ENV)
            .map(PathBuf::from)
            .ok_or_else(|| std::io::Error::other(format!("{GATE_WORKSPACE_ENV} is required")))?;
        std::fs::create_dir_all(&workspace)?;
        let plane = Plane::from_env();
        let features = match plane {
            // The Official plane is the untouched baseline. No port may run
            // here, whatever the launch manifest says.
            Plane::Official => EnhancedRuntimeFeatures::all_off(),
            Plane::Enhanced => parse_enhanced_features(
                std::env::var("VELLUM_ENHANCED_FEATURE_PROFILE").ok().as_deref(),
            )?,
        };
        let store_path = codex_home.join(THREAD_STORE_FILE);
        let store = std::fs::read(&store_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ThreadStore>(&bytes).ok())
            .unwrap_or_default();
        Ok(Self {
            plane,
            runtime_digest: std::env::var("VELLUM_RUNTIME_DIGEST").unwrap_or_default(),
            enhanced_commit: std::env::var("VELLUM_ENHANCED_COMMIT").unwrap_or_default(),
            feature_profile: profile_label(features),
            features,
            catalog,
            provider_url,
            workspace,
            store_path,
            store: Mutex::new(store),
            out: Mutex::new(std::io::stdout()),
            cancels: Mutex::new(HashMap::new()),
            steers: Mutex::new(HashMap::new()),
            running_turns: Mutex::new(HashMap::new()),
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
        })
    }

    fn send(&self, value: &Value) {
        let mut out = self.out.lock().expect("stdout poisoned");
        let _ = serde_json::to_writer(&mut *out, value);
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }

    fn announce_identity(&self) {
        // Built by the portable crate, so this child cannot drift from the
        // shape the Enhanced fork will actually emit.
        self.send(&identity_notification(
            &self.enhanced_commit,
            &self.runtime_digest,
            &self.feature_profile,
            self.features,
        ));
    }

    fn emit_enhanced_events(&self, events: &[EnhancedEvent]) {
        if self.plane != Plane::Enhanced {
            return;
        }
        for event in events {
            self.send(&event_notification(event));
        }
    }

    fn persist(&self) {
        let store = self.store.lock().expect("thread store poisoned");
        if let Ok(bytes) = serde_json::to_vec_pretty(&*store) {
            let _ = std::fs::write(&self.store_path, bytes);
        }
    }

    fn dispatch(self: &Arc<Self>, value: Value) -> Option<std::thread::JoinHandle<()>> {
        let method = value.get("method").and_then(Value::as_str)?.to_string();
        let id = value.get("id").cloned();
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        match method.as_str() {
            "initialize" => {
                if let Some(id) = id {
                    self.send(&json!({
                        "id": id,
                        "result": {
                            "userAgent": format!("vellum-codex-gate-child/{}", self.plane.prefix())
                        }
                    }));
                }
                None
            }
            "initialized" => None,
            "thread/start" => {
                self.reply(id, self.start_thread(&params, None));
                None
            }
            "thread/resume" => {
                let thread_id = params
                    .get("threadId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let known = self
                    .store
                    .lock()
                    .expect("thread store poisoned")
                    .threads
                    .contains_key(&thread_id);
                let result = if known {
                    Ok(json!({"thread": {"id": thread_id}}))
                } else {
                    self.start_thread(&params, Some(thread_id))
                };
                self.reply(id, result);
                None
            }
            "thread/fork" => {
                self.reply(id, self.fork_thread(&params));
                None
            }
            "turn/interrupt" => {
                if let Some(thread_id) = params.get("threadId").and_then(Value::as_str) {
                    if let Some(flag) = self
                        .cancels
                        .lock()
                        .expect("cancel table poisoned")
                        .get(thread_id)
                    {
                        flag.store(true, Ordering::SeqCst);
                    }
                }
                self.reply(id, Ok(json!({"interrupted": true})));
                None
            }
            // The gate's stand-in for the user typing while a turn runs. Real
            // Desktop expresses this as a new turn on a live thread; the gate
            // needs it as its own signal so the assertion is not a race.
            "turn/steer" => {
                if let Some(thread_id) = params.get("threadId").and_then(Value::as_str) {
                    if let Some(flag) = self
                        .steers
                        .lock()
                        .expect("steer table poisoned")
                        .get(thread_id)
                    {
                        flag.store(true, Ordering::SeqCst);
                    }
                }
                self.reply(id, Ok(json!({"steered": true})));
                None
            }
            "turn/create" => {
                let thread_id = params
                    .get("threadId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                // A second turn arriving while one is in flight is a user
                // steering the conversation; the running turn must stop
                // continuing on its own.
                let already_running = self
                    .running_turns
                    .lock()
                    .expect("turn table poisoned")
                    .contains_key(&thread_id);
                if already_running {
                    if let Some(flag) = self
                        .steers
                        .lock()
                        .expect("steer table poisoned")
                        .get(&thread_id)
                    {
                        flag.store(true, Ordering::SeqCst);
                    }
                }
                let child = Arc::clone(self);
                Some(std::thread::spawn(move || {
                    child.run_turn(id, thread_id, params);
                }))
            }
            _ => {
                if id.is_some() {
                    self.reply(id, Ok(json!({})));
                }
                None
            }
        }
    }

    fn reply(&self, id: Option<Value>, result: Result<Value, String>) {
        let Some(id) = id else { return };
        match result {
            Ok(result) => self.send(&json!({"id": id, "result": result})),
            Err(message) => {
                self.send(&json!({"id": id, "error": {"code": -32000, "message": message}}))
            }
        }
    }

    fn start_thread(&self, params: &Value, requested_id: Option<String>) -> Result<Value, String> {
        let model = params
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if self.catalog.get(&model).is_none() {
            return Err(format!("model {model} is not in the gate catalog"));
        }
        let model_provider = params
            .get("modelProvider")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut store = self.store.lock().expect("thread store poisoned");
        let thread_id = requested_id.unwrap_or_else(|| {
            store.next_thread += 1;
            format!("gate-{}-thread-{}", self.plane.prefix(), store.next_thread)
        });
        store.threads.insert(
            thread_id.clone(),
            ThreadRecord {
                model,
                model_provider,
                runtime_digest: self.runtime_digest.clone(),
                items: Vec::new(),
                generation: 1,
                ledger: ToolCallLedger::new().to_durable_json(),
            },
        );
        drop(store);
        self.persist();
        Ok(json!({"thread": {"id": thread_id}}))
    }

    fn fork_thread(&self, params: &Value) -> Result<Value, String> {
        let source = params
            .get("threadId")
            .and_then(Value::as_str)
            .ok_or_else(|| "thread/fork requires threadId".to_string())?;
        let mut store = self.store.lock().expect("thread store poisoned");
        let record = store
            .threads
            .get(source)
            .cloned()
            .ok_or_else(|| format!("unknown thread {source}"))?;
        store.next_thread += 1;
        let thread_id = format!("gate-{}-thread-{}", self.plane.prefix(), store.next_thread);
        store.threads.insert(thread_id.clone(), record);
        drop(store);
        self.persist();
        Ok(json!({"thread": {"id": thread_id}}))
    }

    fn run_turn(&self, id: Option<Value>, thread_id: String, params: Value) {
        let cancel = Arc::new(AtomicBool::new(false));
        let steer = Arc::new(AtomicBool::new(false));
        self.cancels
            .lock()
            .expect("cancel table poisoned")
            .insert(thread_id.clone(), Arc::clone(&cancel));
        self.steers
            .lock()
            .expect("steer table poisoned")
            .insert(thread_id.clone(), Arc::clone(&steer));
        *self
            .running_turns
            .lock()
            .expect("turn table poisoned")
            .entry(thread_id.clone())
            .or_insert(0) += 1;

        let outcome = self.execute(&thread_id, &params, &cancel, &steer);

        self.running_turns
            .lock()
            .expect("turn table poisoned")
            .remove(&thread_id);
        self.cancels
            .lock()
            .expect("cancel table poisoned")
            .remove(&thread_id);
        self.steers
            .lock()
            .expect("steer table poisoned")
            .remove(&thread_id);

        match outcome {
            Ok(turn) => {
                self.send(&json!({
                    "method": "turn/completed",
                    "params": {"threadId": thread_id, "status": turn["status"].clone()}
                }));
                self.reply(id, Ok(json!({"turn": turn})));
            }
            Err(message) => self.reply(id, Err(message)),
        }
    }

    fn execute(
        &self,
        thread_id: &str,
        params: &Value,
        cancel: &AtomicBool,
        steer: &AtomicBool,
    ) -> Result<Value, String> {
        let (model, mut surface, ledger_json) = {
            let store = self.store.lock().expect("thread store poisoned");
            let record = store
                .threads
                .get(thread_id)
                .ok_or_else(|| format!("unknown thread {thread_id}"))?;
            (
                record.model.clone(),
                ModelVisibleSurface {
                    item_token_estimates: Vec::new(),
                    items: record.items.clone(),
                    generation: record.generation,
                },
                record.ledger.clone(),
            )
        };
        let entry: GateModel = *self
            .catalog
            .get(&model)
            .ok_or_else(|| format!("model {model} is not in the gate catalog"))?;

        let mut hooks = EnhancedTurnHooks::new(self.features);
        let _ = hooks.restore_ledger(&ledger_json);
        hooks.prune_policy = ToolResultPrunePolicy {
            trigger_ratio: 0.25,
            min_text_bytes: 512,
            keep_head_bytes: 256,
            keep_tail_bytes: 256,
        };
        hooks.on_new_user_input();
        let mut telemetry = MemoryTelemetry::default();

        if let Some(text) = params.get("input").and_then(Value::as_str) {
            surface.items.push(SurfaceItem::User { text: text.into() });
        } else {
            surface.items.push(SurfaceItem::User {
                text: "start".into(),
            });
        }

        let mut counters = TurnCounters::default();
        let mut status = "completed";
        let mut error_message: Option<String> = None;

        loop {
            if cancel.load(Ordering::SeqCst) {
                status = "cancelled";
                break;
            }
            // Context pressure: prune, re-measure, and only then consider a
            // local compaction. Nothing here reaches the provider.
            if let HookDecision::Handled(plan) = hooks.plan_pressure(
                &surface,
                entry.context_window,
                entry.compact_threshold,
                &mut telemetry,
            ) {
                if plan.prune.rewritten {
                    counters.pruned_chars += plan.prune.bytes_removed;
                    surface = plan.prune.surface.clone();
                    surface.generation += 1;
                }
                counters.compaction_avoided |= plan.compaction_avoided;
                if plan.compact == CompactDecision::RunNativeCodexLocalCompact {
                    surface = local_compact(&surface);
                    counters.local_compactions += 1;
                }
                self.emit_enhanced_events(&telemetry.events);
                telemetry.events.clear();
            }

            let before = surface.clone();
            let response = self.call_provider(thread_id, &model, &surface)?;
            counters.provider_requests += 1;

            if let Some(provider_error) = response.error {
                if provider_error.kind != "context_length_exceeded" {
                    status = "failed";
                    error_message = Some(provider_error.message);
                    break;
                }
                // The provider has just contradicted our own size estimate, so
                // recovery prunes unconditionally rather than waiting for the
                // pressure ratio that already said the surface was fine.
                let pressure_policy = hooks.prune_policy;
                hooks.prune_policy = ToolResultPrunePolicy {
                    trigger_ratio: 0.0,
                    ..pressure_policy
                };
                let after = match hooks.plan_pressure(
                    &surface,
                    entry.context_window,
                    entry.compact_threshold,
                    &mut telemetry,
                ) {
                    HookDecision::Handled(plan) => {
                        let mut after = plan.prune.surface.clone();
                        after.generation = before.generation + 1;
                        after
                    }
                    HookDecision::DeferToUpstream => before.clone(),
                };
                hooks.prune_policy = pressure_policy;
                telemetry.events.clear();
                match hooks.plan_overflow(
                    &before,
                    &after,
                    cancel.load(Ordering::SeqCst),
                    &mut telemetry,
                ) {
                    HookDecision::Handled(plan) => {
                        self.emit_enhanced_events(&telemetry.events);
                        telemetry.events.clear();
                        match plan.decision {
                            OverflowDecision::Retry { retry_index } => {
                                counters.overflow_retries =
                                    counters.overflow_retries.max(retry_index as u64);
                                surface = after;
                                continue;
                            }
                            OverflowDecision::PreserveOriginalError => {
                                status = "failed";
                                error_message = Some(provider_error.message);
                                break;
                            }
                            OverflowDecision::Cancelled => {
                                status = "cancelled";
                                break;
                            }
                        }
                    }
                    HookDecision::DeferToUpstream => {
                        status = "failed";
                        error_message = Some(provider_error.message);
                        break;
                    }
                }
            }

            let mut executed_tool = false;
            for item in &response.output {
                match item {
                    OutputItem::Message { text } => {
                        surface
                            .items
                            .push(SurfaceItem::Assistant { text: text.clone() });
                    }
                    OutputItem::FunctionCall {
                        call_id,
                        name,
                        arguments,
                    } => {
                        executed_tool = true;
                        surface.items.push(SurfaceItem::ToolCall {
                            call_id: call_id.clone(),
                            tool_name: name.clone(),
                            tool_type: "function".into(),
                            arguments: arguments.to_string(),
                        });
                        let identity = ToolCallLedger::identity(call_id, name, arguments);
                        let decision = hooks.admit_tool_call(identity, &mut telemetry);
                        self.emit_enhanced_events(&telemetry.events);
                        telemetry.events.clear();
                        let text = match decision {
                            HookDecision::DeferToUpstream => {
                                counters.tool_executions += 1;
                                self.run_tool(name, arguments)
                            }
                            HookDecision::Handled(AdmitDecision::Execute) => {
                                counters.tool_executions += 1;
                                let output = self.run_tool(name, arguments);
                                hooks.mark_tool_resolution(call_id, ToolCallResolution::Executed);
                                output
                            }
                            HookDecision::Handled(AdmitDecision::SuppressDuplicate { message }) => {
                                counters.synthetic_duplicates += 1;
                                let synthetic =
                                    ToolCallLedger::synthetic_duplicate_result(&message);
                                hooks.mark_tool_resolution(
                                    call_id,
                                    ToolCallResolution::SyntheticDuplicate,
                                );
                                // Never a success payload: the model has to see
                                // that the call did not run.
                                format!(
                                    "{{\"ok\":{},\"kind\":\"{:?}\",\"message\":{}}}",
                                    synthetic.success(),
                                    synthetic.kind,
                                    Value::String(synthetic.message.clone())
                                )
                            }
                            HookDecision::Handled(AdmitDecision::FailClosed { message }) => {
                                counters.fail_closed += 1;
                                hooks.mark_tool_resolution(call_id, ToolCallResolution::Failed);
                                format!("{{\"ok\":false,\"message\":{}}}", Value::String(message))
                            }
                        };
                        surface.items.push(SurfaceItem::ToolResult {
                            call_id: call_id.clone(),
                            tool_name: name.clone(),
                            tool_type: "function".into(),
                            blocks: vec![ContentBlock {
                                kind: "text".into(),
                                text: Some(text),
                            }],
                        });
                    }
                }
            }

            if response.unfinished {
                let context = TurnStopContext {
                    cancelled: cancel.load(Ordering::SeqCst),
                    user_steer_pending: steer.load(Ordering::SeqCst),
                    unfinished: vec![UnfinishedSignal::StructuredPendingWork],
                    ..TurnStopContext::default()
                };
                match hooks.on_turn_stop(&context, &mut telemetry) {
                    HookDecision::Handled(plan) => {
                        self.emit_enhanced_events(&telemetry.events);
                        telemetry.events.clear();
                        match plan.decision {
                            ContinuationDecision::Continue { index } => {
                                if let Some(reserved) = plan.reservation {
                                    let _ = hooks.commit_continuation(reserved);
                                }
                                counters.continuations = counters.continuations.max(index as u64);
                                surface.items.push(SurfaceItem::User {
                                    text: vellum_enhanced_codex::CONTINUE_NUDGE.into(),
                                });
                                continue;
                            }
                            ContinuationDecision::StopCancelled => {
                                status = "cancelled";
                                break;
                            }
                            ContinuationDecision::StopUserSteer => {
                                status = "steered";
                                break;
                            }
                            ContinuationDecision::StopBudgetExhausted => {
                                counters.continuation_budget_exhausted = true;
                                break;
                            }
                            ContinuationDecision::AllowStop => break,
                        }
                    }
                    // Without the port an unfinished turn just stops, which is
                    // exactly the unmodified behaviour the E0 lane must show.
                    HookDecision::DeferToUpstream => break,
                }
            }

            if !executed_tool {
                break;
            }
        }

        {
            let mut store = self.store.lock().expect("thread store poisoned");
            if let Some(record) = store.threads.get_mut(thread_id) {
                record.items = surface.items.clone();
                record.generation = surface.generation;
                record.ledger = hooks.ledger.to_durable_json();
            }
        }
        self.persist();

        Ok(json!({
            "status": status,
            "error": error_message,
            "runtimeDigest": self.runtime_digest,
            "featureProfile": self.feature_profile,
            "providerRequests": counters.provider_requests,
            "toolExecutions": counters.tool_executions,
            "syntheticDuplicates": counters.synthetic_duplicates,
            "failClosed": counters.fail_closed,
            "continuations": counters.continuations,
            "continuationBudgetExhausted": counters.continuation_budget_exhausted,
            "overflowRetries": counters.overflow_retries,
            "prunedChars": counters.pruned_chars,
            "compactionAvoided": counters.compaction_avoided,
            "localCompactions": counters.local_compactions,
            "toolPairsIntact": surface.tool_pairs_intact()
        }))
    }

    fn run_tool(&self, name: &str, arguments: &Value) -> String {
        match name {
            "workspace_append" => {
                let line = arguments
                    .get("line")
                    .and_then(Value::as_str)
                    .unwrap_or("side-effect");
                let path = self.workspace.join(SIDE_EFFECT_LOG);
                let result = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .and_then(|mut file| writeln!(file, "{line}"));
                match result {
                    Ok(()) => "{\"ok\":true}".into(),
                    Err(error) => format!("{{\"ok\":false,\"message\":\"{error}\"}}"),
                }
            }
            "read_large" => {
                let bytes = arguments
                    .get("bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(1_024)
                    .min(1_000_000) as usize;
                "x".repeat(bytes)
            }
            other => format!("{{\"ok\":false,\"message\":\"unknown tool {other}\"}}"),
        }
    }

    fn call_provider(
        &self,
        thread_id: &str,
        model: &str,
        surface: &ModelVisibleSurface,
    ) -> Result<ProviderResponse, String> {
        let body = json!({
            "model": model,
            "stream": false,
            "input": surface.items.iter().map(provider_item).collect::<Vec<_>>()
        });
        let url = format!("{}/v1/responses", self.provider_url);
        let thread = thread_id.to_string();
        self.runtime.block_on(async move {
            let client = reqwest::Client::new();
            let response = client
                .post(url)
                .header(THREAD_HEADER, thread)
                .json(&body)
                .send()
                .await
                .map_err(|error| format!("provider request failed: {error}"))?;
            let value = response
                .json::<Value>()
                .await
                .map_err(|error| format!("provider response was not JSON: {error}"))?;
            Ok(ProviderResponse::parse(&value))
        })
    }
}

fn parse_enhanced_features(value: Option<&str>) -> std::io::Result<EnhancedRuntimeFeatures> {
    let Some(value) = value else {
        return Ok(EnhancedRuntimeFeatures::all_on());
    };
    serde_json::from_str(value).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("VELLUM_ENHANCED_FEATURE_PROFILE is invalid: {error}"),
        )
    })
}

#[derive(Debug, Default)]
struct TurnCounters {
    provider_requests: u64,
    tool_executions: u64,
    synthetic_duplicates: u64,
    fail_closed: u64,
    continuations: u64,
    continuation_budget_exhausted: bool,
    overflow_retries: u64,
    pruned_chars: u64,
    compaction_avoided: bool,
    local_compactions: u64,
}

#[derive(Debug)]
struct ProviderError {
    kind: String,
    message: String,
}

#[derive(Debug)]
enum OutputItem {
    Message {
        text: String,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: Value,
    },
}

#[derive(Debug)]
struct ProviderResponse {
    output: Vec<OutputItem>,
    unfinished: bool,
    error: Option<ProviderError>,
}

impl ProviderResponse {
    fn parse(value: &Value) -> Self {
        if let Some(error) = value.get("error") {
            return Self {
                output: Vec::new(),
                unfinished: false,
                error: Some(ProviderError {
                    kind: error
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("provider_error")
                        .to_string(),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("provider error")
                        .to_string(),
                }),
            };
        }
        let output = value
            .get("output")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(parse_output_item).collect())
            .unwrap_or_default();
        Self {
            output,
            unfinished: value.get("status").and_then(Value::as_str) == Some("incomplete"),
            error: None,
        }
    }
}

fn parse_output_item(value: &Value) -> Option<OutputItem> {
    match value.get("type").and_then(Value::as_str)? {
        "message" => Some(OutputItem::Message {
            text: value
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        "function_call" => Some(OutputItem::FunctionCall {
            call_id: value.get("call_id").and_then(Value::as_str)?.to_string(),
            name: value.get("name").and_then(Value::as_str)?.to_string(),
            arguments: value
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|text| serde_json::from_str(text).ok())
                .unwrap_or(Value::Null),
        }),
        _ => None,
    }
}

fn provider_item(item: &SurfaceItem) -> Value {
    match item {
        SurfaceItem::User { text } => json!({"type": "message", "role": "user", "text": text}),
        SurfaceItem::System { text } => json!({"type": "message", "role": "system", "text": text}),
        SurfaceItem::Developer { text } => {
            json!({"type": "message", "role": "developer", "text": text})
        }
        SurfaceItem::Assistant { text } => {
            json!({"type": "message", "role": "assistant", "text": text})
        }
        SurfaceItem::ToolCall {
            call_id,
            tool_name,
            arguments,
            ..
        } => json!({
            "type": "function_call",
            "call_id": call_id,
            "name": tool_name,
            "arguments": arguments
        }),
        SurfaceItem::ToolResult {
            call_id, blocks, ..
        } => json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": blocks
                .iter()
                .filter_map(|block| block.text.clone())
                .collect::<Vec<_>>()
                .join("")
        }),
        SurfaceItem::Control { key, value } => {
            json!({"type": "control", "key": key, "value": value})
        }
    }
}

/// Local compaction: the runtime rewrites its own surface and says nothing to
/// the provider about it. A remote `compaction_trigger` would be a Vellum
/// control plane, which is exactly what this runtime no longer has.
fn local_compact(surface: &ModelVisibleSurface) -> ModelVisibleSurface {
    let keep_from = surface.items.len().saturating_sub(2);
    let mut items = vec![SurfaceItem::Control {
        key: "local_compaction".into(),
        value: format!("{} earlier items summarized locally", keep_from),
    }];
    items.extend(surface.items[keep_from..].iter().cloned());
    ModelVisibleSurface {
        item_token_estimates: Vec::new(),
        items,
        generation: surface.generation + 1,
    }
}

fn profile_label(features: EnhancedRuntimeFeatures) -> String {
    for profile in [
        vellum_enhanced_codex::AblationProfile::E0,
        vellum_enhanced_codex::AblationProfile::E1,
        vellum_enhanced_codex::AblationProfile::E2,
        vellum_enhanced_codex::AblationProfile::E3,
        vellum_enhanced_codex::AblationProfile::E4,
        vellum_enhanced_codex::AblationProfile::E5,
    ] {
        if profile.features() == features {
            return profile.as_str().to_string();
        }
    }
    "custom".into()
}

/// The gate child binary lives in this crate; expose the path helper so the
/// gate can find whichever build profile produced the current test run.
pub fn gate_child_executable() -> Option<PathBuf> {
    let current = std::env::current_exe().ok()?;
    let mut directory = current.parent()?.to_path_buf();
    let name = format!("vellum-codex-gate-child{}", std::env::consts::EXE_SUFFIX);
    for _ in 0..3 {
        let candidate = directory.join(&name);
        if candidate.is_file() {
            return Some(candidate);
        }
        // `cargo test` runs binaries from `target/<profile>/deps`.
        directory = directory.parent()?.to_path_buf();
    }
    None
}

pub fn workspace_side_effect_log(workspace: &Path) -> PathBuf {
    workspace.join(SIDE_EFFECT_LOG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_compaction_keeps_the_tail_and_advances_the_generation() {
        let surface = ModelVisibleSurface {
            item_token_estimates: Vec::new(),
            generation: 3,
            items: vec![
                SurfaceItem::User { text: "a".into() },
                SurfaceItem::Assistant { text: "b".into() },
                SurfaceItem::User { text: "c".into() },
                SurfaceItem::Assistant { text: "d".into() },
            ],
        };
        let compacted = local_compact(&surface);
        assert_eq!(compacted.generation, 4);
        assert_eq!(compacted.items.len(), 3);
        assert!(matches!(compacted.items[0], SurfaceItem::Control { .. }));
    }

    #[test]
    fn a_provider_request_never_carries_a_compaction_trigger() {
        let surface = ModelVisibleSurface {
            item_token_estimates: Vec::new(),
            generation: 1,
            items: vec![SurfaceItem::User { text: "hi".into() }],
        };
        let body = json!({
            "model": "m",
            "stream": false,
            "input": surface.items.iter().map(provider_item).collect::<Vec<_>>()
        });
        let encoded = serde_json::to_string(&body).unwrap();
        assert!(!encoded.contains("compaction_trigger"));
        assert!(!encoded.contains("canonical"));
    }

    #[test]
    fn feature_profiles_are_labelled_from_flags_not_model_names() {
        assert_eq!(profile_label(EnhancedRuntimeFeatures::all_off()), "E0");
        assert_eq!(profile_label(EnhancedRuntimeFeatures::all_on()), "E5");
    }

    #[test]
    fn enhanced_feature_profile_defaults_only_when_absent() {
        assert_eq!(
            parse_enhanced_features(None).unwrap(),
            EnhancedRuntimeFeatures::all_on()
        );
        let explicit = parse_enhanced_features(Some(
            r#"{"qwenToolReliability":false,"deepseekContextRecovery":false,"qwenBoundedContinuation":false}"#,
        ))
        .unwrap();
        assert_eq!(explicit, EnhancedRuntimeFeatures::all_off());
    }

    #[test]
    fn enhanced_feature_profile_rejects_invalid_or_unknown_flags() {
        assert!(parse_enhanced_features(Some("not-json")).is_err());
        assert!(parse_enhanced_features(Some(r#"{"mysteryFlag":true}"#)).is_err());
    }
}
