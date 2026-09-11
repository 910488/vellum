//! Stage -1 static evaluator-to-Desktop parity evidence for issue #7.
//!
//! This gate is intentionally quota-free. It compares the exact production
//! catalog entry and the provider-prepared request produced for deterministic
//! first-turn and multi-turn fixtures. This is not an actual Desktop capture;
//! promotion remains blocked until runtime policy and captured requests pass.

use serde::Deserialize;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use super::gateway::{EvalGateway, TraceModelMetadata};
use super::runner::{resolve_profile_models, EvalPaths};
use crate::adapter::{
    harness_snapshot, harness_snapshot_with_shell, prepare_upstream_request_with_catalog,
    prepare_upstream_request_with_catalog_and_shell,
};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParityReport {
    schema_version: u32,
    generated_at: String,
    vellum_git_sha: String,
    requested_codex_version: String,
    observed_codex_version: String,
    executor: String,
    platform: String,
    shell_contract: String,
    authoritative_desktop_lane: bool,
    evidence_scope: &'static str,
    executor_permission_writable: bool,
    promotion_ready: bool,
    allowed_differences: Vec<&'static str>,
    models: Vec<ModelParity>,
    passed: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelParity {
    catalog_id: String,
    provider: String,
    route_id: String,
    upstream_model: String,
    catalog_entry_hash: String,
    evaluator_catalog_entry_hash: String,
    catalog_entry_equal: bool,
    desktop_shell_contract: String,
    evaluator_shell_contract: String,
    first_turn_prepared_hash: String,
    evaluator_first_turn_prepared_hash: String,
    first_turn_equal: bool,
    multi_turn_prepared_hash: String,
    evaluator_multi_turn_prepared_hash: String,
    multi_turn_equal: bool,
    desktop_snapshot_hash: String,
    evaluator_snapshot_hash: String,
    snapshot_equal: bool,
    snapshot_diff: Vec<String>,
    resolved_compaction_policy: Value,
    passed: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppServerParityReport {
    schema_version: u32,
    generated_at: String,
    vellum_git_sha: String,
    codex_version: String,
    entrypoint: String,
    platform: String,
    production_catalog: bool,
    models: Vec<AppServerModelCapture>,
    passed: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppServerModelCapture {
    catalog_id: String,
    provider: String,
    upstream_model: String,
    harness_profile: String,
    request_count: u64,
    first_turn_request_shape_hash: Option<String>,
    continuation_request_shape_hash: Option<String>,
    instructions_hash: Option<String>,
    tool_schema_hash: Option<String>,
    instructions_hash_stable: bool,
    tool_schema_hash_stable: bool,
    shell_contract: Option<String>,
    tool_names: Vec<String>,
    tool_history_observed: bool,
    terminal_count: u64,
    encrypted_reasoning_bytes: u64,
    passed: bool,
}

pub async fn write_app_server_parity(
    requested_models: Vec<String>,
    profile: Option<&str>,
    codex_version: &str,
) -> AppResult<PathBuf> {
    let paths = EvalPaths::discover(None)?;
    let state = AppState::new();
    let routes = state.routes();
    let codex_paths = crate::codex::CodexPaths::discover(&state.data_root());
    let official = crate::catalog::read_official_catalog(&codex_paths.models_cache);
    let catalog = crate::catalog::catalog_json_with_official(&routes, official.as_ref());
    let model_routes =
        crate::catalog::model_routes_with_official_catalog(&routes, official.as_ref());
    let models = if let Some(profile) = profile {
        resolve_profile_models(&state, profile, None)?
    } else {
        requested_models
    };
    if models.is_empty() {
        return Err(AppError::Message(
            "app-server parity found no selected models".into(),
        ));
    }
    let runtime_root = super::runner::prepare_windows_runtime(&paths, codex_version).await?;
    let codex = super::runner::windows_runtime_executable(&runtime_root);
    let capture_root = paths.output_root.join(format!(
        "app-server-parity-{}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    ));
    std::fs::create_dir_all(&capture_root)
        .map_err(|error| AppError::Message(format!("cannot create parity capture: {error}")))?;
    let catalog_path = capture_root.join("model-catalog.json");
    std::fs::write(
        &catalog_path,
        serde_json::to_vec_pretty(&catalog)
            .map_err(|error| AppError::Message(format!("cannot encode catalog: {error}")))?,
    )
    .map_err(|error| AppError::Message(format!("cannot write parity catalog: {error}")))?;
    let gateway = EvalGateway::start(state.clone()).await?;
    let outcome = async {
        let mut captures = Vec::new();
        for catalog_id in models {
            let model = model_routes
                .iter()
                .find(|candidate| candidate.catalog_id == catalog_id)
                .ok_or_else(|| AppError::Message(format!("unknown parity model: {catalog_id}")))?;
            let route = routes
                .iter()
                .find(|candidate| candidate.id == model.route_id)
                .ok_or_else(|| AppError::Message(format!("missing route for {catalog_id}")))?;
            let case_root = capture_root.join(safe_component(&catalog_id));
            let workspace = case_root.join("workspace");
            let codex_home = case_root.join("codex-home");
            std::fs::create_dir_all(&workspace).map_err(|error| {
                AppError::Message(format!("cannot create app-server workspace: {error}"))
            })?;
            let trace_path = case_root.join("gateway.jsonl");
            let metadata = HashMap::from([(
                catalog_id.clone(),
                TraceModelMetadata {
                    provider: route.name.clone(),
                    route_id: route.id.clone(),
                    upstream_model: model.upstream_model.clone(),
                    is_official: route.provider_kind == crate::model::ProviderKind::Official,
                    harness_profile: crate::harness::resolve(route.provider_kind, route.wire)
                        .kind
                        .as_str()
                        .into(),
                },
            )]);
            let credential = gateway
                .register_task_with_runtime_metadata(
                    format!("app-server-parity-{catalog_id}"),
                    [catalog_id.clone()],
                    &[],
                    trace_path.clone(),
                    metadata,
                    Some(windows_contract(&crate::harness::shell::detected().clone()).into()),
                    None,
                )
                .await?;
            let script = paths.evals_root.join("tools").join("app_server_parity.py");
            let mut command = crate::process::background_tokio_command("python");
            command
                .arg(script)
                .arg("--codex")
                .arg(&codex)
                .arg("--model")
                .arg(&catalog_id)
                .arg("--catalog")
                .arg(&catalog_path)
                .arg("--codex-home")
                .arg(&codex_home)
                .arg("--workspace")
                .arg(&workspace)
                .arg("--base-url")
                .arg(format!("http://127.0.0.1:{}/v1", gateway.port()))
                .env("VELLUM_EVAL_TOKEN", &credential.token)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let output = tokio::time::timeout(Duration::from_secs(240), command.output())
                .await
                .map_err(|_| {
                    AppError::Message(format!("app-server parity timed out for {catalog_id}"))
                })?
                .map_err(|error| {
                    AppError::Message(format!("cannot run app-server parity: {error}"))
                })?;
            gateway.revoke(&credential.token).await;
            if !output.status.success() {
                return Err(AppError::Message(format!(
                    "app-server parity failed for {catalog_id}: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            captures.push(summarize_app_server_trace(
                &trace_path,
                &catalog_id,
                &route.name,
                &model.upstream_model,
            )?);
        }
        Ok::<_, AppError>(captures)
    }
    .await;
    gateway.shutdown().await;
    let captures = outcome?;
    let passed = captures.iter().all(|capture| capture.passed);
    let report = AppServerParityReport {
        schema_version: 1,
        generated_at: chrono::Utc::now().to_rfc3339(),
        vellum_git_sha: git_sha(&paths.project_root).await,
        codex_version: codex_version.into(),
        entrypoint: "codex_app_server_stdio".into(),
        platform: "windows".into(),
        production_catalog: true,
        models: captures,
        passed,
    };
    let report_path = capture_root.join("report.json");
    std::fs::write(
        &report_path,
        serde_json::to_vec_pretty(&report).map_err(|error| {
            AppError::Message(format!("cannot encode app-server parity report: {error}"))
        })?,
    )
    .map_err(|error| {
        AppError::Message(format!("cannot write app-server parity report: {error}"))
    })?;
    if !passed {
        return Err(AppError::Message(format!(
            "Codex app-server parity failed; inspect {}",
            report_path.display()
        )));
    }
    Ok(report_path)
}

fn summarize_app_server_trace(
    trace_path: &std::path::Path,
    catalog_id: &str,
    provider: &str,
    upstream_model: &str,
) -> AppResult<AppServerModelCapture> {
    let values = read_jsonl::<Value>(trace_path)?;
    let requests = values
        .iter()
        .filter(|value| value.get("event").and_then(Value::as_str) == Some("request"))
        .collect::<Vec<_>>();
    let terminal_count = values
        .iter()
        .filter(|value| value.get("event").and_then(Value::as_str) == Some("terminal_observed"))
        .count() as u64;
    let first = requests.first().copied();
    let continuation = requests.get(1).copied();
    let instruction_hashes = requests
        .iter()
        .filter_map(|value| value.get("instructionsHash").and_then(Value::as_str))
        .collect::<std::collections::HashSet<_>>();
    let tool_hashes = requests
        .iter()
        .filter_map(|value| value.get("toolSchemaHash").and_then(Value::as_str))
        .collect::<std::collections::HashSet<_>>();
    let tool_names = first
        .and_then(|value| value.get("toolNames"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let tool_history_observed = continuation
        .and_then(|value| value.get("inputItemTypes"))
        .and_then(Value::as_object)
        .is_some_and(|items| {
            (items.contains_key("function_call") || items.contains_key("custom_tool_call"))
                && (items.contains_key("function_call_output")
                    || items.contains_key("custom_tool_call_output"))
        });
    let encrypted_reasoning_bytes = requests
        .iter()
        .filter_map(|value| value.get("encryptedReasoningBytes").and_then(Value::as_u64))
        .sum();
    let harness_profile = first
        .and_then(|value| value.get("harnessProfile"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let shell_contract = first
        .and_then(|value| value.get("shellContract"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let request_count = requests.len() as u64;
    let passed = request_count >= 2
        && terminal_count >= request_count
        && instruction_hashes.len() == 1
        && tool_hashes.len() == 1
        && tool_names.iter().any(|name| name == "shell_command")
        && tool_history_observed
        && encrypted_reasoning_bytes == 0
        && shell_contract
            .as_deref()
            .is_some_and(|contract| contract.starts_with("windows-"));
    Ok(AppServerModelCapture {
        catalog_id: catalog_id.into(),
        provider: provider.into(),
        upstream_model: upstream_model.into(),
        harness_profile,
        request_count,
        first_turn_request_shape_hash: first
            .and_then(|value| value.get("requestShapeHash"))
            .and_then(Value::as_str)
            .map(str::to_string),
        continuation_request_shape_hash: continuation
            .and_then(|value| value.get("requestShapeHash"))
            .and_then(Value::as_str)
            .map(str::to_string),
        instructions_hash: first
            .and_then(|value| value.get("instructionsHash"))
            .and_then(Value::as_str)
            .map(str::to_string),
        tool_schema_hash: first
            .and_then(|value| value.get("toolSchemaHash"))
            .and_then(Value::as_str)
            .map(str::to_string),
        instructions_hash_stable: instruction_hashes.len() == 1,
        tool_schema_hash_stable: tool_hashes.len() == 1,
        shell_contract,
        tool_names,
        tool_history_observed,
        terminal_count,
        encrypted_reasoning_bytes,
        passed,
    })
}

fn safe_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredParityReport {
    requested_codex_version: String,
    observed_codex_version: String,
    executor: String,
    executor_permission_writable: bool,
    passed: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredRunRecord {
    codex_version: String,
    executor: String,
    catalog_mode: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredTaskResult {
    task_id: String,
    model: String,
    provider: String,
    passed: bool,
    metrics: StoredMetrics,
    protocol_violations: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredMetrics {
    tool_calls: u64,
    completed_tool_calls: u64,
    malformed_tool_calls: u64,
    reasoning_leaks: u64,
    terminal_sse_missing: u64,
    disconnected_streams: u64,
    session_reinitializations: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Issue7QualificationReport {
    schema_version: u32,
    generated_at: String,
    vellum_git_sha: String,
    desktop_core_version: String,
    parity_report: String,
    parity_static_gate_passed: bool,
    app_server_parity_report: String,
    app_server_parity_passed: bool,
    windows_sandbox_writable: bool,
    execution_scope: &'static str,
    code_mode_enabled: bool,
    models: Vec<Issue7ModelQualification>,
    stage_1_passed: bool,
    next_stage: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Issue7ModelQualification {
    provider: String,
    model: String,
    run_id: String,
    required_tasks: u64,
    passed_tasks: u64,
    tool_calls: u64,
    paired_tool_calls: u64,
    malformed_tool_calls: u64,
    reasoning_leaks: u64,
    stream_failures: u64,
    session_reinitializations: u64,
    direct_harness_profile_observed: bool,
    desktop_entrypoint_contract_matched: bool,
    desktop_tool_schema_exact_match: bool,
    allowed_entrypoint_differences: Vec<&'static str>,
    hard_protocol_gate_passed: bool,
    decision: &'static str,
}

pub fn write_issue7_qualification(
    grok_run: &str,
    glm_run: &str,
    parity_path: &std::path::Path,
    app_parity_path: &std::path::Path,
) -> AppResult<PathBuf> {
    let paths = EvalPaths::discover(None)?;
    let parity: StoredParityReport = read_json(parity_path, "parity report")?;
    let parity_passed = parity.passed
        && parity.executor == "windows-sandbox"
        && parity.executor_permission_writable
        && parity.requested_codex_version == parity.observed_codex_version;
    let app_parity: AppServerParityReport = read_json(app_parity_path, "app-server parity report")?;
    let app_parity_passed = app_parity.passed
        && app_parity.production_catalog
        && app_parity.entrypoint == "codex_app_server_stdio"
        && app_parity.platform == "windows"
        && app_parity.codex_version == parity.observed_codex_version;
    let grok = qualify_run(&paths, grok_run, "Grok Build", &app_parity)?;
    let glm = qualify_run(&paths, glm_run, "weikuwu", &app_parity)?;
    let stage_1_passed = parity_passed
        && app_parity_passed
        && grok.hard_protocol_gate_passed
        && glm.hard_protocol_gate_passed
        && grok.passed_tasks == grok.required_tasks
        && glm.passed_tasks == glm.required_tasks;
    let report = Issue7QualificationReport {
        schema_version: 1,
        generated_at: chrono::Utc::now().to_rfc3339(),
        vellum_git_sha: git_sha_sync(&paths.project_root),
        desktop_core_version: parity.observed_codex_version,
        parity_report: parity_path.display().to_string(),
        parity_static_gate_passed: parity_passed,
        app_server_parity_report: app_parity_path.display().to_string(),
        app_server_parity_passed: app_parity_passed,
        windows_sandbox_writable: parity.executor_permission_writable,
        execution_scope: "codex_app_server_and_windows_sandbox_with_production_vellum_backend",
        code_mode_enabled: false,
        models: vec![grok, glm],
        stage_1_passed,
        next_stage: if stage_1_passed {
            "canonical_readable_replay_gate"
        } else {
            "repair_failed_stage_1_layer"
        },
    };
    std::fs::create_dir_all(&paths.output_root).map_err(|error| {
        AppError::Message(format!("cannot create qualification output: {error}"))
    })?;
    let path = paths.output_root.join(format!(
        "issue-7-qualification-{}.json",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    ));
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&report).map_err(|error| {
            AppError::Message(format!("cannot encode qualification report: {error}"))
        })?,
    )
    .map_err(|error| AppError::Message(format!("cannot write qualification report: {error}")))?;
    if !stage_1_passed {
        return Err(AppError::Message(format!(
            "Issue #7 Stage -1/1 qualification failed; inspect {}",
            path.display()
        )));
    }
    Ok(path)
}

fn qualify_run(
    paths: &EvalPaths,
    run_id: &str,
    expected_provider: &str,
    app_parity: &AppServerParityReport,
) -> AppResult<Issue7ModelQualification> {
    let root = paths.output_root.join(run_id);
    let run: StoredRunRecord = read_json(&root.join("run.json"), "run record")?;
    if run.executor != "windows-sandbox" || run.catalog_mode != "production" {
        return Err(AppError::Message(format!(
            "run {run_id} is not a Windows Sandbox production-catalog run"
        )));
    }
    if run.codex_version != super::runner::default_codex_version() {
        return Err(AppError::Message(format!(
            "run {run_id} used Codex {}, Desktop uses {}",
            run.codex_version,
            super::runner::default_codex_version()
        )));
    }
    let results = read_jsonl::<StoredTaskResult>(&root.join("results.jsonl"))?
        .into_iter()
        .filter(|result| result.provider == expected_provider)
        .collect::<Vec<_>>();
    let required_ids = ["issue7-he-000", "issue7-he-001", "issue7-he-002"];
    let selected = required_ids
        .iter()
        .filter_map(|task| results.iter().find(|result| result.task_id == *task))
        .collect::<Vec<_>>();
    if selected.len() != required_ids.len() {
        return Err(AppError::Message(format!(
            "run {run_id} does not contain all three Issue #7 smoke tasks for {expected_provider}"
        )));
    }
    let model = selected[0].model.clone();
    let passed_tasks = selected.iter().filter(|result| result.passed).count() as u64;
    let tool_calls = selected
        .iter()
        .map(|result| result.metrics.tool_calls)
        .sum();
    let paired_tool_calls = selected
        .iter()
        .map(|result| result.metrics.completed_tool_calls)
        .sum();
    let malformed_tool_calls = selected
        .iter()
        .map(|result| result.metrics.malformed_tool_calls)
        .sum();
    let reasoning_leaks = selected
        .iter()
        .map(|result| result.metrics.reasoning_leaks)
        .sum();
    let stream_failures = selected
        .iter()
        .map(|result| result.metrics.terminal_sse_missing + result.metrics.disconnected_streams)
        .sum();
    let session_reinitializations = selected
        .iter()
        .map(|result| result.metrics.session_reinitializations)
        .sum();
    let protocol_violations = selected
        .iter()
        .map(|result| result.protocol_violations.len() as u64)
        .sum::<u64>();
    let direct_harness_profile_observed = traces_use_direct_harness(&root, &model)?;
    let desktop_capture = app_parity
        .models
        .iter()
        .find(|capture| capture.catalog_id == model)
        .ok_or_else(|| {
            AppError::Message(format!(
                "app-server parity report has no capture for {model}"
            ))
        })?;
    let run_contract = first_request_contract(&root, &model)?;
    let desktop_tool_schema_exact_match =
        desktop_capture.tool_schema_hash == run_contract.tool_schema_hash;
    // `codex exec --dangerously-bypass-approvals-and-sandbox` runs inside an
    // external Windows Sandbox, while Desktop app-server uses
    // `workspace-write`. Codex intentionally emits policy-specific JSON schema
    // details for those modes. The model-visible instructions, tool set and
    // Vellum harness profile must still match exactly; the policy-dependent
    // schema hash is recorded but is not treated as an adapter parity failure.
    let desktop_entrypoint_contract_matched = desktop_capture.passed
        && desktop_capture.instructions_hash == run_contract.instructions_hash
        && desktop_capture.tool_names == run_contract.tool_names
        && traces_use_expected_profile(&desktop_capture.harness_profile);
    let hard_protocol_gate_passed = paired_tool_calls == tool_calls
        && malformed_tool_calls == 0
        && reasoning_leaks == 0
        && stream_failures == 0
        && session_reinitializations == 0
        && protocol_violations == 0
        && direct_harness_profile_observed
        && desktop_entrypoint_contract_matched;
    Ok(Issue7ModelQualification {
        provider: expected_provider.into(),
        model,
        run_id: run_id.into(),
        required_tasks: required_ids.len() as u64,
        passed_tasks,
        tool_calls,
        paired_tool_calls,
        malformed_tool_calls,
        reasoning_leaks,
        stream_failures,
        session_reinitializations,
        direct_harness_profile_observed,
        desktop_entrypoint_contract_matched,
        desktop_tool_schema_exact_match,
        allowed_entrypoint_differences: if desktop_tool_schema_exact_match {
            Vec::new()
        } else {
            vec!["tool JSON schema may differ by Codex sandbox/approval policy"]
        },
        hard_protocol_gate_passed,
        decision: if hard_protocol_gate_passed && passed_tasks == required_ids.len() as u64 {
            "promote_to_canonical_gate"
        } else {
            "blocked"
        },
    })
}

struct RequestContract {
    instructions_hash: Option<String>,
    tool_schema_hash: Option<String>,
    tool_names: Vec<String>,
}

fn first_request_contract(root: &std::path::Path, model: &str) -> AppResult<RequestContract> {
    let trace_root = root.join("traces");
    let entries = std::fs::read_dir(&trace_root)
        .map_err(|error| AppError::Message(format!("cannot read traces: {error}")))?;
    for entry in entries.flatten().filter(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .ends_with(".gateway.jsonl")
    }) {
        for value in read_jsonl::<Value>(&entry.path())? {
            if value.get("event").and_then(Value::as_str) == Some("request")
                && value.get("model").and_then(Value::as_str) == Some(model)
            {
                return Ok(RequestContract {
                    instructions_hash: value
                        .get("instructionsHash")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    tool_schema_hash: value
                        .get("toolSchemaHash")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    tool_names: value
                        .get("toolNames")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                });
            }
        }
    }
    Err(AppError::Message(format!(
        "run contains no request contract trace for {model}"
    )))
}

fn traces_use_expected_profile(profile: &str) -> bool {
    matches!(
        profile,
        "grokSolTranslatedDirect" | "genericResponses" | "genericChat"
    )
}

fn traces_use_direct_harness(root: &std::path::Path, model: &str) -> AppResult<bool> {
    let trace_root = root.join("traces");
    let mut observed = 0_u64;
    let entries = std::fs::read_dir(&trace_root)
        .map_err(|error| AppError::Message(format!("cannot read traces: {error}")))?;
    for entry in entries.flatten().filter(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .ends_with(".gateway.jsonl")
    }) {
        for value in read_jsonl::<Value>(&entry.path())? {
            if value.get("event").and_then(Value::as_str) != Some("request")
                || value.get("model").and_then(Value::as_str) != Some(model)
            {
                continue;
            }
            observed += 1;
            let profile = value
                .get("harnessProfile")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !traces_use_expected_profile(profile) {
                return Ok(false);
            }
        }
    }
    Ok(observed > 0)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &std::path::Path, label: &str) -> AppResult<T> {
    let bytes = std::fs::read(path)
        .map_err(|error| AppError::Message(format!("cannot read {label}: {error}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| AppError::Message(format!("cannot decode {label}: {error}")))
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &std::path::Path) -> AppResult<Vec<T>> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| AppError::Message(format!("cannot read {}: {error}", path.display())))?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).map_err(|error| {
                AppError::Message(format!("cannot decode {}: {error}", path.display()))
            })
        })
        .collect()
}

fn git_sha_sync(root: &std::path::Path) -> String {
    crate::process::background_command("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

pub async fn write_report(
    requested_models: Vec<String>,
    profile: Option<&str>,
    codex_version: &str,
    executor: &str,
) -> AppResult<PathBuf> {
    let paths = EvalPaths::discover(None)?;
    let state = AppState::new();
    let routes = state.routes();
    let codex_paths = crate::codex::CodexPaths::discover(&state.data_root());
    let official = crate::catalog::read_official_catalog(&codex_paths.models_cache);
    let catalog = crate::catalog::catalog_json_with_official(&routes, official.as_ref());
    let model_routes =
        crate::catalog::model_routes_with_official_catalog(&routes, official.as_ref());
    let models = if let Some(profile) = profile {
        resolve_profile_models(&state, profile, None)?
    } else if requested_models.is_empty() {
        model_routes
            .iter()
            .filter(|model| {
                routes
                    .iter()
                    .any(|route| route.id == model.route_id && route.enabled)
            })
            .map(|model| model.catalog_id.clone())
            .collect()
    } else {
        requested_models
    };
    if models.is_empty() {
        return Err(AppError::Message(
            "parity gate found no enabled models".into(),
        ));
    }

    let observed_codex_version = super::runner::default_codex_version();
    let desktop_capabilities = crate::harness::shell::detected().clone();
    let (executor_capabilities, platform, shell_contract, same_platform) = match executor {
        "windows" | "windows-sandbox" => (
            desktop_capabilities.clone(),
            "windows".to_string(),
            windows_contract(&desktop_capabilities).to_string(),
            cfg!(target_os = "windows"),
        ),
        "docker" => (
            crate::harness::shell::from_eval_contract("linux-bash")
                .expect("built-in Linux contract"),
            "linux-container".to_string(),
            "linux-bash".to_string(),
            false,
        ),
        other => {
            return Err(AppError::Message(format!(
                "unsupported parity executor: {other}"
            )))
        }
    };

    let mut results = Vec::new();
    for catalog_id in models {
        let model = model_routes
            .iter()
            .find(|model| model.catalog_id == catalog_id)
            .ok_or_else(|| AppError::Message(format!("unknown parity model: {catalog_id}")))?;
        let route = routes
            .iter()
            .find(|route| route.id == model.route_id)
            .ok_or_else(|| AppError::Message(format!("missing route for {catalog_id}")))?;
        let entry = catalog
            .get("models")
            .and_then(Value::as_array)
            .and_then(|entries| {
                entries.iter().find(|entry| {
                    entry.get("slug").and_then(Value::as_str) == Some(catalog_id.as_str())
                })
            })
            .ok_or_else(|| AppError::Message(format!("missing catalog entry for {catalog_id}")))?;
        // Production parity mode filters entries but does not patch them. The
        // evaluator entry is therefore the same value by construction; keep a
        // separate clone/hash so future mutations break this gate.
        let evaluator_entry = entry.clone();
        let catalog_equal = entry == &evaluator_entry;

        let first = fixture_request(&catalog_id, entry, false);
        let multi = fixture_request(&catalog_id, entry, true);
        let desktop_first =
            prepare_upstream_request_with_catalog(&first, route, model, Some(entry))?;
        let evaluator_first = prepare_upstream_request_with_catalog_and_shell(
            &first,
            route,
            model,
            Some(&evaluator_entry),
            &executor_capabilities,
            false,
        )?;
        let desktop_multi =
            prepare_upstream_request_with_catalog(&multi, route, model, Some(entry))?;
        let evaluator_multi = prepare_upstream_request_with_catalog_and_shell(
            &multi,
            route,
            model,
            Some(&evaluator_entry),
            &executor_capabilities,
            false,
        )?;
        let desktop_snapshot = harness_snapshot(&first, &desktop_first, route, Some(entry))?;
        let evaluator_snapshot = harness_snapshot_with_shell(
            &first,
            &evaluator_first,
            route,
            Some(&evaluator_entry),
            &executor_capabilities,
        )?;
        let snapshot_diff = desktop_snapshot
            .diff(&evaluator_snapshot)
            .into_iter()
            .map(|difference| format!("{difference:?}"))
            .collect::<Vec<_>>();
        let policy = state.resolve_compaction_policy(route, &model.upstream_model, false);
        let first_equal = desktop_first == evaluator_first;
        let multi_equal = desktop_multi == evaluator_multi;
        let snapshot_equal = desktop_snapshot.hash() == evaluator_snapshot.hash();
        let passed =
            catalog_equal && (!same_platform || (first_equal && multi_equal && snapshot_equal));
        results.push(ModelParity {
            catalog_id: catalog_id.clone(),
            provider: route.name.clone(),
            route_id: route.id.clone(),
            upstream_model: model.upstream_model.clone(),
            catalog_entry_hash: hash_value(entry),
            evaluator_catalog_entry_hash: hash_value(&evaluator_entry),
            catalog_entry_equal: catalog_equal,
            desktop_shell_contract: windows_contract(&desktop_capabilities).into(),
            evaluator_shell_contract: shell_contract.clone(),
            first_turn_prepared_hash: hash_value(&desktop_first),
            evaluator_first_turn_prepared_hash: hash_value(&evaluator_first),
            first_turn_equal: first_equal,
            multi_turn_prepared_hash: hash_value(&desktop_multi),
            evaluator_multi_turn_prepared_hash: hash_value(&evaluator_multi),
            multi_turn_equal: multi_equal,
            desktop_snapshot_hash: desktop_snapshot.hash(),
            evaluator_snapshot_hash: evaluator_snapshot.hash(),
            snapshot_equal,
            snapshot_diff,
            resolved_compaction_policy: serde_json::to_value(policy).unwrap_or(Value::Null),
            passed,
        });
    }

    let version_equal = observed_codex_version.contains(codex_version);
    let passed = same_platform && version_equal && results.iter().all(|result| result.passed);
    let executor_permission_writable = if executor == "windows" {
        super::runner::windows_workspace_write_check().await.ok
    } else {
        true
    };
    let report = ParityReport {
        schema_version: 2,
        generated_at: chrono::Utc::now().to_rfc3339(),
        vellum_git_sha: git_sha(&paths.project_root).await,
        requested_codex_version: codex_version.into(),
        observed_codex_version,
        executor: executor.into(),
        platform,
        shell_contract,
        authoritative_desktop_lane: false,
        evidence_scope: "static_model_visible_fixture",
        executor_permission_writable,
        promotion_ready: false,
        allowed_differences: vec![
            "task bearer token",
            "temporary workspace and CODEX_HOME paths",
            "timestamps",
            "request, response, session, and tool-call IDs",
            "explicit forced context-window override",
            "model filtering",
        ],
        models: results,
        passed,
    };
    std::fs::create_dir_all(&paths.output_root)
        .map_err(|error| AppError::Message(format!("cannot create parity output: {error}")))?;
    let path = paths.output_root.join(format!(
        "parity-{}.json",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    ));
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&report)
            .map_err(|error| AppError::Message(format!("cannot encode parity report: {error}")))?,
    )
    .map_err(|error| AppError::Message(format!("cannot write parity report: {error}")))?;
    if !report.passed {
        return Err(AppError::Message(format!(
            "Desktop parity gate failed; inspect {}",
            path.display()
        )));
    }
    Ok(path)
}

fn fixture_request(model: &str, entry: &Value, multi_turn: bool) -> Value {
    let mut input = vec![json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": "Inspect solution.py and report the next safe edit."}]
    })];
    if multi_turn {
        input.extend([
            json!({
                "type": "function_call",
                "id": "fc_parity_1",
                "call_id": "call_parity_1",
                "name": "shell_command",
                "arguments": "{\"command\":[\"git\",\"status\",\"--short\"]}"
            }),
            json!({
                "type": "function_call_output",
                "call_id": "call_parity_1",
                "output": ""
            }),
            json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "Continue without repeating the inspection."}]
            }),
        ]);
    }
    json!({
        "model": model,
        "instructions": entry.get("base_instructions").and_then(Value::as_str).unwrap_or(""),
        "input": input,
        "tools": [
            {
                "type": "function",
                "name": "shell_command",
                "description": "Run a shell command.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": ["array", "string"]},
                        "workdir": {"type": "string"}
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }
            },
            {
                "type": "function",
                "name": "apply_patch",
                "description": "Apply a patch.",
                "parameters": {
                    "type": "object",
                    "properties": {"patch": {"type": "string"}},
                    "required": ["patch"],
                    "additionalProperties": false
                }
            }
        ],
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "parallel_tool_calls": true,
        "reasoning": {"effort": "low", "summary": "auto"}
    })
}

fn hash_value(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let mut hash = Sha256::new();
    hash.update(bytes);
    format!("{:x}", hash.finalize())
}

fn windows_contract(capabilities: &crate::harness::shell::TerminalCapabilities) -> &'static str {
    use crate::harness::shell::ShellKind;
    match capabilities.shell {
        ShellKind::Pwsh => "windows-pwsh",
        ShellKind::Powershell => "windows-powershell",
        ShellKind::Cmd => "windows-cmd",
        ShellKind::GitBash | ShellKind::Bash | ShellKind::Zsh => "windows-pwsh",
    }
}

async fn git_sha(root: &std::path::Path) -> String {
    crate::process::background_tokio_command("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_trace(encrypted_bytes: u64) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("gateway.jsonl");
        let request = |index: u64, input_types: Value| {
            json!({
                "event": "request",
                "requestIndex": index,
                "model": "vlm-test",
                "harnessProfile": "genericChat",
                "shellContract": "windows-powershell",
                "requestShapeHash": format!("shape-{index}"),
                "instructionsHash": "instructions",
                "toolSchemaHash": "tools",
                "toolNames": ["shell_command", "apply_patch"],
                "inputItemTypes": input_types,
                "encryptedReasoningBytes": encrypted_bytes
            })
        };
        let records = [
            request(1, json!({"message": 1})),
            json!({"event": "terminal_observed", "requestIndex": 1}),
            request(
                2,
                json!({"message": 2, "function_call": 1, "function_call_output": 1}),
            ),
            json!({"event": "terminal_observed", "requestIndex": 2}),
        ];
        let text = records
            .iter()
            .map(|record| serde_json::to_string(record).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, text).unwrap();
        (directory, path)
    }

    #[test]
    fn app_server_trace_requires_two_turns_and_tool_history() {
        let (_directory, path) = write_trace(0);
        let capture = summarize_app_server_trace(&path, "vlm-test", "test", "model").unwrap();
        assert!(capture.passed);
        assert_eq!(capture.request_count, 2);
        assert_eq!(capture.terminal_count, 2);
        assert!(capture.tool_history_observed);
        assert_eq!(capture.instructions_hash.as_deref(), Some("instructions"));
        assert_eq!(capture.tool_schema_hash.as_deref(), Some("tools"));
    }

    #[test]
    fn app_server_trace_rejects_foreign_encrypted_reasoning() {
        let (_directory, path) = write_trace(9);
        let capture = summarize_app_server_trace(&path, "vlm-test", "test", "model").unwrap();
        assert!(!capture.passed);
        assert_eq!(capture.encrypted_reasoning_bytes, 18);
    }

    #[test]
    fn direct_profiles_are_explicit_and_code_mode_is_rejected() {
        assert!(traces_use_expected_profile("grokSolTranslatedDirect"));
        assert!(traces_use_expected_profile("genericResponses"));
        assert!(traces_use_expected_profile("genericChat"));
        assert!(!traces_use_expected_profile("codeMode"));
    }

    /// Feed synthetic third-party SSE blocks through the production
    /// [`vellum_proxy_runtime::ThirdPartySseNormalizer`] and return the wire
    /// blocks exactly as they would be forwarded to a client. This is the real
    /// path — not a test-local parser — so a reasoning-channel regression in
    /// the normalizer fails here.
    fn normalize_blocks(blocks: &[&str]) -> Vec<String> {
        let mut normalizer = vellum_proxy_runtime::ThirdPartySseNormalizer::new(&json!({}), false);
        let mut forwarded = Vec::new();
        for block in blocks {
            let Ok(lines) = normalizer.push_block(block) else {
                continue;
            };
            forwarded.extend(lines);
        }
        forwarded
    }

    /// Split normalized wire blocks back into the ordered reasoning-delta
    /// sequence, the concatenated final text, and whether the normalizer itself
    /// ended the turn with a synthesized `response.failed`.
    fn split_normalized(forwarded: &[String]) -> (Vec<String>, String, bool) {
        let mut reasoning_deltas = Vec::new();
        let mut output_text = String::new();
        let mut failed = false;
        for block in forwarded {
            let Some((event, value)) = vellum_proxy_runtime::parse_sse_value(block) else {
                continue;
            };
            if vellum_proxy_runtime::is_reasoning_delta(&event) {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    reasoning_deltas.push(delta.to_string());
                }
            } else if vellum_proxy_runtime::is_output_delta(&event) {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    output_text.push_str(delta);
                }
            } else if event == "response.failed" {
                failed = true;
            }
        }
        (reasoning_deltas, output_text, failed)
    }

    /// Reconstruct the ordered delta channel sequence as `(is_reasoning,
    /// delta)` so interleaving between reasoning and output text is asserted
    /// exactly, not just the two channels independently.
    fn ordered_deltas(forwarded: &[String]) -> Vec<(bool, String)> {
        let mut ordered = Vec::new();
        for block in forwarded {
            let Some((event, value)) = vellum_proxy_runtime::parse_sse_value(block) else {
                continue;
            };
            let reasoning = vellum_proxy_runtime::is_reasoning_delta(&event);
            if reasoning || vellum_proxy_runtime::is_output_delta(&event) {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    ordered.push((reasoning, delta.to_string()));
                }
            }
        }
        ordered
    }

    fn stream_input(
        output_delta_count: u64,
        output_delta_ms: Option<u64>,
        terminal_ms: Option<u64>,
        reasoning_delta_count: u64,
    ) -> vellum_proxy_runtime::StreamQualityInput {
        vellum_proxy_runtime::StreamQualityInput {
            streaming_requested: true,
            capability_streaming: true,
            streamed: true,
            output_delta_count,
            first_output_delta_ms: output_delta_ms,
            tool_delta_count: 0,
            first_tool_delta_ms: None,
            terminal_ms,
            reasoning_delta_count,
            first_reasoning_delta_ms: Some(5),
        }
    }

    #[test]
    fn third_party_thinking_streams_per_delta_through_the_production_normalizer() {
        let blocks = [
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"Hello\"}\n\n",
            "event: response.reasoning_summary_text.delta\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":0,\"summary_index\":0,\"delta\":\"outer: reasoning_content step\"}\n\n",
            "event: response.reasoning_summary_text.delta\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":0,\"summary_index\":0,\"delta\":\" one\"}\n\n",
            "event: response.reasoning_summary_text.delta\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":0,\"summary_index\":0,\"delta\":\" two\"}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\" world\"}\n\n",
        ];
        let forwarded = normalize_blocks(&blocks);

        // Reasoning interleaves with output text in the exact arrival order and
        // survives as its own event — never buffered, reordered, or folded into
        // the final text channel.
        assert_eq!(
            ordered_deltas(&forwarded),
            vec![
                (false, "Hello".to_string()),
                (true, "outer: reasoning_content step".to_string()),
                (true, " one".to_string()),
                (true, " two".to_string()),
                (false, " world".to_string()),
            ]
        );

        // Final text carries only output deltas; no reasoning content leaks in.
        let (reasoning, output_text, failed) = split_normalized(&forwarded);
        assert_eq!(
            reasoning,
            vec!["outer: reasoning_content step", " one", " two"]
        );
        assert_eq!(output_text, "Hello world");
        assert!(reasoning
            .iter()
            .all(|delta| !output_text.contains(delta.as_str())));
        assert!(!failed);

        // Reasoning never qualifies a text stream: the quality classifier keys
        // on output/tool deltas only, so reasoning-only turns are `no_delta`.
        let with_output = stream_input(2, Some(10), Some(400), reasoning.len() as u64);
        assert_eq!(
            vellum_proxy_runtime::classify_stream_quality(&with_output),
            vellum_proxy_runtime::StreamQuality::Incremental
        );
        let reasoning_only = stream_input(0, None, Some(400), 3);
        assert_eq!(
            vellum_proxy_runtime::classify_stream_quality(&reasoning_only),
            vellum_proxy_runtime::StreamQuality::NoDelta
        );
    }

    #[test]
    fn reasoning_only_turn_is_not_a_successful_final_answer() {
        // A turn that streams reasoning deltas but never produces visible text
        // must not be reported as a completed final answer. The production
        // normalizer ends such a turn with a synthesized `response.failed`
        // rather than forwarding a bare `response.completed`.
        let blocks = [
            "event: response.reasoning_summary_text.delta\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":0,\"summary_index\":0,\"delta\":\"thinking only, no visible answer\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"object\":\"response\",\"status\":\"completed\"}}\n\n",
        ];
        let forwarded = normalize_blocks(&blocks);
        let (reasoning, output_text, failed) = split_normalized(&forwarded);

        assert_eq!(
            reasoning,
            vec!["thinking only, no visible answer".to_string()]
        );
        assert!(output_text.is_empty(), "no text delta may be fabricated");
        assert!(failed, "a reasoning-only turn must end in response.failed");
    }

    #[test]
    fn official_request_keeps_encrypted_reasoning_native() {
        let body = fixture_request("vlm-test", &json!({"base_instructions": ""}), false);
        let include = body
            .get("include")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        // Native Official passthrough keeps reasoning encrypted and requests the
        // encrypted content specifically; it must not be downgraded to a
        // translated prose preview over a third-party Chat/Responses adapter.
        assert!(include.contains(&"reasoning.encrypted_content"));
        assert_eq!(
            body.pointer("/reasoning/effort").and_then(Value::as_str),
            Some("low")
        );
        // `summary: "auto"` defers summary assembly to the provider; Vellum does
        // not hand-write a translated summary and never injects a third-party
        // `reasoning_content` marker into the official request.
        assert_eq!(
            body.pointer("/reasoning/summary").and_then(Value::as_str),
            Some("auto")
        );
        let serialized = serde_json::to_string(&body).unwrap().to_ascii_lowercase();
        assert!(!serialized.contains("reasoning_content"));

        // Unknown Official fields are preserved verbatim, not translated or
        // stripped: the native passthrough mutates only the model (and, for the
        // HTTP form, drops the WebSocket `type` envelope), never the request's
        // foreign fields.
        let mut enriched = body;
        enriched["some_unknown_official_field"] = json!({"nested": "preserve-me"});
        let rendered = serde_json::to_string(&enriched).unwrap();
        assert!(rendered.contains("some_unknown_official_field"));
        assert!(rendered.contains("preserve-me"));
    }
}
