use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use toml_edit::{value as toml_value, DocumentMut};

use super::artifacts::{
    config_digest_from_docker_save_manifest, gunzip_limited, install_hashed_download,
    parse_docker_load_output, parse_oci_digest_reference, validate_loaded_images,
    ArchiveAcquisition, BundleDownloadClient,
};
use super::events::{parse_codex_jsonl, EventMetrics};
use super::gateway::{
    redact_text, redact_text_with_secrets, EvalGateway, EvalTokenBudget, SweTokenBudget,
    SweTurnBudget, TraceModelMetadata, SWE_MAX_TURNS_PER_TASK,
};
use super::manifest::{
    EvalTask, FaultKind, FaultSpec, PhaseModel, SuiteManifest, SweGraderMode, TaskCategory,
    TaskSource, TransitionMode,
};
use crate::error::{AppError, AppResult};
use crate::history::CompactionEngine;
use crate::model::{ModelRoute, ProviderKind};
use crate::policy::{CompactionStrategy, SessionCompactionPolicy};
use crate::state::AppState;
use vellum_proxy_runtime::ProxyRuntimeState;

pub const DEFAULT_CODEX_VERSION: &str = "0.142.5";
pub const AGENT_IMAGE_PREFIX: &str = "vellum-eval-agent";
static CONTAINER_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const HUMANEVAL_SHA256: &str = "b796127e635a67f93fb35c04f4cb03cf06f38c8072ee7cee8833d7bee06979ef";
const MBPP_SHA256: &str = "ca95deaa9a01ef0a6f439f88bcf0dd3db3563d22f22aad6cae04ebb9a8d8c8e9";

/// Resolve the evaluator runtime to the Codex core version currently used by
/// Desktop. The model cache is written by the Desktop app-server and is a more
/// reliable source than a separately installed `codex` command on PATH.
/// Headless/CI machines fall back to the evaluator's pinned compatibility
/// version.
pub fn default_codex_version() -> String {
    if let Some(version) = installed_desktop_codex_version() {
        return version;
    }
    let cache = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .map(|root| root.join("models_cache.json"));
    cache
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| {
            value
                .get("client_version")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|version| {
                    !version.is_empty()
                        && version.chars().all(|character| {
                            character.is_ascii_alphanumeric() || ".-+".contains(character)
                        })
                })
                .map(str::to_string)
        })
        .unwrap_or_else(|| DEFAULT_CODEX_VERSION.into())
}

#[cfg(target_os = "windows")]
fn installed_desktop_codex_version() -> Option<String> {
    let root = dirs::data_local_dir()?
        .join("OpenAI")
        .join("Codex")
        .join("bin");
    let mut executables = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path().join("codex.exe");
            let modified = path.metadata().ok()?.modified().ok()?;
            path.is_file().then_some((modified, path))
        })
        .collect::<Vec<_>>();
    executables.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    for (_, executable) in executables {
        let output = crate::process::background_command(executable)
            .arg("--version")
            .output()
            .ok()?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if let Some(version) = text.split_whitespace().find(|part| {
            part.chars()
                .next()
                .is_some_and(|value| value.is_ascii_digit())
        }) {
            return Some(version.to_string());
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
fn installed_desktop_codex_version() -> Option<String> {
    None
}

#[derive(Debug, Clone)]
pub struct EvalPaths {
    pub project_root: PathBuf,
    pub evals_root: PathBuf,
    pub output_root: PathBuf,
    pub prepared_root: PathBuf,
    pub dataset_root: PathBuf,
}

impl EvalPaths {
    pub fn discover(dataset_root: Option<PathBuf>) -> AppResult<Self> {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let project_root = manifest_dir
            .parent()
            .ok_or_else(|| AppError::Message("cannot locate Vellum project root".into()))?
            .to_path_buf();
        let evals_root = project_root.join("evals");
        let output_root = project_root.join("target").join("vellum-evals");
        let prepared_root = output_root.join("prepared");
        let dataset_root =
            dataset_root.unwrap_or_else(|| project_root.join("..").join("public_datasets"));
        Ok(Self {
            project_root,
            evals_root,
            output_root,
            prepared_root,
            dataset_root,
        })
    }

    pub fn suite_path(&self, suite: &str) -> PathBuf {
        let path = PathBuf::from(suite);
        if path.is_file() {
            path
        } else {
            self.evals_root.join("suites").join(format!("{suite}.json"))
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub suite: String,
    pub models: Vec<String>,
    pub profile: Option<String>,
    pub lane: Option<String>,
    pub transition: Option<String>,
    pub repeat: Option<u32>,
    pub dataset_root: Option<PathBuf>,
    pub codex_version: String,
    pub max_tasks: Option<usize>,
    pub max_wall_seconds: Option<u64>,
    pub max_total_tokens: Option<u64>,
    pub natural_context: bool,
    pub resume_run: Option<String>,
    pub task_filter: Option<String>,
    pub category_filter: Option<String>,
    pub provider_filter: Option<String>,
    pub tag_filter: Option<String>,
    pub seed: u64,
    pub control_model: Option<String>,
    pub baseline_run: Option<String>,
    pub compaction_engine: String,
    pub recovery_mode: String,
    pub grok_compaction: String,
    pub schedule: String,
    pub fail_fast: Option<String>,
    /// Agent executor. `windows-sandbox` is the authoritative Desktop
    /// qualification lane; `windows` is host-only diagnostics and `docker`
    /// is a cross-platform synthetic lane.
    pub executor: String,
    /// `production` preserves the Desktop catalog entry byte-for-byte except
    /// for an explicitly recorded context-window override. `legacy` keeps the
    /// old pinned-CLI compatibility mutation for diagnostic runs only.
    pub catalog_mode: String,
    /// Explicit Vellum-managed OpenAI account ID or email for eval traffic.
    /// This never changes the Desktop default account.
    pub oauth_account: Option<String>,
    /// Explicit Vellum-managed Grok account ID or email for eval traffic.
    pub grok_account: Option<String>,
    /// Maximum Official turns allowed for this eval run.
    pub max_official_turns: Option<u32>,
    /// Explicit Enhanced Codex ablation profile. Never inferred from a model id.
    pub ablation_profile: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    pub run_id: String,
    pub status: String,
    #[serde(default)]
    pub pid: Option<u32>,
    pub suite: String,
    pub suite_version: String,
    pub suite_hash: String,
    #[serde(default)]
    pub dataset_hashes: HashMap<String, String>,
    pub vellum_git_sha: String,
    pub vellum_version: String,
    pub codex_version: String,
    #[serde(default = "default_executor_name")]
    pub executor: String,
    #[serde(default = "default_catalog_mode_name")]
    pub catalog_mode: String,
    #[serde(default)]
    pub executor_platform: String,
    #[serde(default)]
    pub shell_contract: String,
    #[serde(default)]
    pub container_image_id: String,
    pub models: Vec<String>,
    #[serde(default)]
    pub oauth_account: Option<String>,
    #[serde(default)]
    pub grok_account: Option<String>,
    #[serde(default)]
    pub max_official_turns: Option<u32>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub lane: Option<String>,
    #[serde(default)]
    pub transition: Option<String>,
    pub repeat: u32,
    pub seed: u64,
    #[serde(default)]
    pub max_tasks: Option<usize>,
    #[serde(default)]
    pub max_wall_seconds: Option<u64>,
    #[serde(default)]
    pub max_total_tokens: Option<u64>,
    #[serde(default)]
    pub natural_context: bool,
    #[serde(default)]
    pub task_filter: Option<String>,
    #[serde(default)]
    pub category_filter: Option<String>,
    #[serde(default)]
    pub provider_filter: Option<String>,
    #[serde(default)]
    pub tag_filter: Option<String>,
    #[serde(default)]
    pub control_model: Option<String>,
    #[serde(default)]
    pub baseline_run: Option<String>,
    #[serde(default = "default_compaction_engine_name")]
    pub compaction_engine: String,
    /// Behavioral guards are explicit in eval artifacts. This run mode is
    /// deliberately independent of the production environment defaults.
    #[serde(default = "default_eval_recovery_mode")]
    pub recovery_mode: String,
    /// Display name injected into Codex `model_providers.vellum_eval.name`.
    /// `"OpenAI"` enables remote compaction; `"Vellum"` forces Codex local.
    #[serde(default = "default_eval_provider_display_name")]
    pub eval_provider_display_name: String,
    #[serde(default = "default_grok_compaction_name")]
    pub grok_compaction: String,
    /// Eval artifact contract version (run.json shape), NOT the Desktop
    /// compaction journal storage schema (see `COMPACTION_JOURNAL_SCHEMA_V3`
    /// in history.rs) and NOT `TaskResult::checkpoint_schema_version`, which
    /// is read from the actual journal row.
    #[serde(default = "default_canonical_contract_version")]
    pub canonical_contract_version: u32,
    #[serde(default = "default_schedule")]
    pub schedule: String,
    #[serde(default)]
    pub fail_fast: Option<String>,
    #[serde(default)]
    pub ablation_profile: Option<String>,
    #[serde(default)]
    pub runtime_digest: Option<String>,
    #[serde(default)]
    pub enhanced_codex_commit: Option<String>,
    #[serde(default)]
    pub enhanced_feature_flags: Option<vellum_enhanced_codex::EnhancedRuntimeFeatures>,
    #[serde(default)]
    pub enhanced_artifact_sha256: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub task_count: usize,
}

/// Structured origin of an evaluation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureOrigin {
    VellumCompaction,
    Provider,
    EvaluatorInfrastructure,
    ResourceBudget,
    #[serde(alias = "model_no_progress_after_recovery")]
    Model,
    Unknown,
}

/// Trigger decision observation for a task phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseTriggerDecision {
    BelowThreshold,
    CompactionObserved,
    AboveThresholdWithoutRequest,
    UnknownMissingTokens,
}

/// Context observation recorded per task phase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhaseContextObservation {
    pub phase: usize,
    pub estimated_context_tokens: u64,
    pub effective_context_window: u64,
    pub auto_compact_token_limit: u64,
    pub compact_requests_before: u64,
    pub compact_requests_after: u64,
    pub trigger_decision: PhaseTriggerDecision,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhaseExecutionObservation {
    #[serde(default = "default_attempt_number")]
    pub attempt: u32,
    pub phase: usize,
    pub model: String,
    pub resumed: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub first_byte_ms: Option<u64>,
    pub terminal_observed: bool,
    pub compaction_delta: u64,
    pub input_token_delta: u64,
    pub output_token_delta: u64,
}

fn default_attempt_number() -> u32 {
    1
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEfficiencyUsageCoverage {
    pub settled_request_count: u64,
    pub zero_usage_request_count: u64,
    pub token_accounting_available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpFaultObservation {
    pub request_index: u64,
    pub fault: String,
    pub message: String,
    pub expected_injection: bool,
    pub recovered: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_total_estimated_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskResult {
    pub run_id: String,
    pub case_id: String,
    pub task_id: String,
    pub category: String,
    pub model: String,
    #[serde(default = "unknown_provider")]
    pub provider: String,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub compaction_mode: Option<String>,
    #[serde(default)]
    pub compact_request_count: u64,
    #[serde(default)]
    pub canonical_hash: Option<String>,
    #[serde(default)]
    pub canonical_schema_version: Option<u32>,
    #[serde(default)]
    pub journal_schema_version: Option<u32>,
    #[serde(default)]
    pub checkpoint_schema_version: Option<u32>,
    #[serde(default)]
    pub canonical_strategy: Option<String>,
    #[serde(default)]
    pub continuity_kind: Option<String>,
    /// Engine the case *requested* via `--compaction-engine` — not proof it ran.
    #[serde(default)]
    pub requested_canonical_engine: Option<String>,
    /// Journal-strategy attribution for Canonical/Official cases.
    /// Prefer [`TaskResult::observed_compaction_engine`] for engine-neutral A/B.
    #[serde(default)]
    pub observed_canonical_engine: Option<String>,
    /// Requested `--compaction-engine` value, copied for engine-neutral reports.
    #[serde(default)]
    pub requested_compaction_engine: Option<String>,
    /// Engine-neutral observation: `vellum_canonical_v2`, `codex_local`,
    /// `remote_v2`, `mixed`, `none`, `unknown`, or `official-native`.
    #[serde(default)]
    pub observed_compaction_engine: Option<String>,
    #[serde(default)]
    pub compaction_engine_matched: Option<bool>,
    #[serde(default)]
    pub codex_compaction_events: u64,
    #[serde(default)]
    pub remote_compact_requests: u64,
    #[serde(default)]
    pub vellum_canonical_journal_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_provider_display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_compaction_model_visible_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_compaction_model_visible_tokens: Option<u64>,
    #[serde(default)]
    pub generation: Option<u32>,
    #[serde(default)]
    pub source_tokens: Option<u64>,
    #[serde(default)]
    pub checkpoint_tokens: Option<u64>,
    #[serde(default)]
    pub semantic_claim_count: Option<usize>,
    #[serde(default)]
    pub grounded_claim_count: Option<usize>,
    #[serde(default)]
    pub rejected_claim_count: Option<usize>,
    #[serde(default)]
    pub grounding_rate: Option<f64>,
    #[serde(default)]
    pub extraction_attempts: Option<usize>,
    #[serde(default)]
    pub fallback_used: Option<bool>,
    #[serde(default)]
    pub repeated_exchanges_collapsed: Option<usize>,
    #[serde(default)]
    pub canonical_kind: Option<String>,
    #[serde(default)]
    pub injected_faults: Vec<String>,
    /// Every gateway HTTP error remains in the report, including declared
    /// fault stimuli that were later recovered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub http_faults: Vec<HttpFaultObservation>,
    pub switched_models: Vec<String>,
    #[serde(default)]
    pub transition: String,
    pub repetition: u32,
    pub passed: bool,
    /// Workspace/grader correctness, independent of latency or token caps.
    #[serde(default)]
    pub task_correctness_passed: bool,
    /// Whether the attempt stayed inside its configured wall/token/disk caps.
    #[serde(default)]
    pub performance_qualified: bool,
    /// Whether transport, tool and recovery protocol observations were valid.
    #[serde(default)]
    pub protocol_qualified: bool,
    pub agent_completed: bool,
    pub timed_out: bool,
    pub limit_exceeded: bool,
    /// The case's configured cumulative input/output token budget, recorded
    /// alongside `metrics.input_tokens`/`output_tokens` so a resumability
    /// check can tell genuine overspend apart from a gateway-side
    /// `swe_token_limit` refusal that never actually settled tokens against
    /// the case (see `result_is_resumable`).
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    pub duration_ms: u64,
    pub first_byte_ms: Option<u64>,
    #[serde(default)]
    pub compaction_recovered: Option<bool>,
    #[serde(default)]
    pub model_switch_recovered: Option<bool>,
    #[serde(default)]
    pub acceptance_passed: u64,
    #[serde(default)]
    pub acceptance_total: u64,
    pub verification_exit_code: Option<i32>,
    pub verification_output: String,
    pub changed_files: Vec<String>,
    pub added_lines: u64,
    pub removed_lines: u64,
    pub metrics: EventMetrics,
    #[serde(default)]
    pub protocol_violations: Vec<String>,
    /// Scenario expectations that were not observed, but are not themselves
    /// wire-protocol failures. For example, a model may stop after one valid
    /// tool call even though the task expected two; that is a capability or
    /// task-completion result, not a broken Codex/Vellum tool contract.
    #[serde(default)]
    pub harness_expectation_misses: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_model_visible_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_model_visible_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_durable_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_visible_compression_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_origin: Option<FailureOrigin>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phase_observations: Vec<PhaseContextObservation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phase_execution_observations: Vec<PhaseExecutionObservation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compaction_attempts: Vec<vellum_proxy_runtime::diagnostics::LocalCompactionAttempt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub semantic_validation_diagnostics:
        Vec<vellum_proxy_runtime::diagnostics::SemanticValidationDiagnostic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_stall_diagnostics: Vec<vellum_proxy_runtime::task_stall::TaskStallDiagnostic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_stall_recoveries: Vec<vellum_proxy_runtime::task_stall::TaskStallRecoveryDiagnostic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_stall_terminals: Vec<vellum_proxy_runtime::task_stall::TaskStallTerminalDiagnostic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_efficiency_diagnostics:
        Vec<vellum_proxy_runtime::task_efficiency::TaskEfficiencyDiagnostic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_efficiency_recoveries:
        Vec<vellum_proxy_runtime::task_efficiency::TaskEfficiencyRecoveryDiagnostic>,
    #[serde(default)]
    pub task_efficiency_usage_coverage: TaskEfficiencyUsageCoverage,
    pub failure_class: Option<String>,
    #[serde(default)]
    pub root_cause: Option<String>,
    #[serde(default)]
    pub supporting_evidence: Vec<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub layers: LayerResults,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerResults {
    pub task_acceptance: bool,
    pub transport_protocol: bool,
    pub tool_protocol: bool,
    pub expected_tool_activity: bool,
    pub compaction_triggered: bool,
    pub canonical_materialized: bool,
    /// Engine-native compaction applied: Canonical journal for Vellum V2,
    /// Codex local history rewrite for `codex-local`.
    #[serde(default)]
    pub compaction_applied: bool,
    pub session_resumed: bool,
    pub continuity_preserved: bool,
    pub resource_budget: bool,
    /// The running process reported the commit, digest, profile, and ports
    /// requested for this case.
    #[serde(default)]
    pub runtime_attribution: bool,
    /// Every mechanism-specific event count declared by the scenario was met.
    #[serde(default)]
    pub mechanism_exercised: bool,
    pub promotion_eligible: bool,
}

fn unknown_provider() -> String {
    "unknown".into()
}

fn default_compaction_engine_name() -> String {
    "codex-local-v0-150".into()
}

fn default_eval_recovery_mode() -> String {
    // Runs created before this field existed inherited the production default,
    // which was shadow. Treating them as recover would make resume metadata
    // lie about the policy that produced their completed attempts.
    "shadow".into()
}

fn default_eval_provider_display_name() -> String {
    "OpenAI".into()
}

pub(super) fn eval_codex_provider_display_name(compaction_engine: &str) -> &'static str {
    if compaction_engine == "codex-local-native-oracle" {
        "Vellum"
    } else {
        "OpenAI"
    }
}

/// Whether Codex should route compaction through Vellum for this eval arm.
///
/// The embedded arm needs the trigger to reach Vellum, so `remote_compaction_v2`
/// stays on. The native-oracle arm measures Codex core's *own* local compact, so
/// the trigger must not be intercepted — leaving this on would silently make the
/// oracle measure Vellum again.
pub(super) fn eval_remote_compaction_v2_enabled(compaction_engine: &str) -> bool {
    compaction_engine != "codex-local-native-oracle"
}

fn default_grok_compaction_name() -> String {
    "desktop".into()
}

pub(super) fn default_executor_name() -> String {
    if cfg!(target_os = "windows") {
        "windows-sandbox".into()
    } else {
        "docker".into()
    }
}

fn default_catalog_mode_name() -> String {
    "production".into()
}

fn default_canonical_contract_version() -> u32 {
    2
}

/// Symmetric, fail-closed check that the journal actually observed after a
/// case matches the compaction engine that was *requested* for it.
///
/// V1 and V2 gates are both enforced — a `canonical-v1` request that landed
/// a V2 journal must fail (not silently pass mislabeled as V1), and vice
/// versa. `canonical-v2` additionally requires a complete audit record
/// (schema version + source/checkpoint hashes), since a V2 case that PASSes
/// with a matching strategy signature but no audit payload produces empty
/// research telemetry (grounding rate, token deltas) rather than a bug.
fn canonical_engine_signature_valid(
    requested: &str,
    journal: Option<&crate::history::CompactionJournal>,
) -> bool {
    match requested {
        "canonical" | "canonical-v1" => journal.is_some_and(|journal| {
            journal.strategy.as_deref() == Some("canonical_v1")
                && journal.checkpoint_schema_version == Some(1)
        }),
        "canonical-v2" => journal.is_some_and(|journal| {
            journal.strategy.as_deref() == Some("canonical_v2")
                && journal.continuity_kind.as_deref() == Some("semantic")
                && journal.checkpoint_schema_version == Some(2)
                && journal
                    .audit
                    .as_ref()
                    .is_some_and(|audit| audit.schema_version == 2)
                && journal.source_hash.is_some()
                && journal.checkpoint_hash.is_some()
        }),
        _ => true,
    }
}

fn observed_compaction_engine(
    requested: &str,
    journal_changed: bool,
    journal_strategy: Option<&str>,
) -> Option<String> {
    if !journal_changed {
        return None;
    }
    if requested == "official-native" {
        // Official compaction is native passthrough. Its separately persisted
        // portable journal may use canonical_v1, but that handoff artifact
        // must not relabel the engine that actually executed.
        return Some("official-native".into());
    }
    journal_strategy.map(str::to_string)
}

/// Engine-neutral observation used by the Canonical V2 vs Codex local A/B.
///
/// Remote compact plus a V2 journal is VellumCanonical V2, not mixed.
/// Codex JSONL compact events that accompany a remote compact request are
/// not counted as Codex local.
fn classify_observed_eval_compaction_engine(
    requested: &str,
    embedded_local_attempt: bool,
    remote_compact_requests: u64,
    codex_compaction_events: u64,
) -> String {
    if requested == "official-native" {
        return if remote_compact_requests > 0 || embedded_local_attempt {
            "official-native".into()
        } else if codex_compaction_events > 0 {
            "mixed".into()
        } else {
            "none".into()
        };
    }
    let saw_remote = remote_compact_requests > 0;
    // JSONL compact events that accompany `/responses/compact` are remote,
    // not Codex local. Local means compact events with no Vellum remote
    // compact request.
    let saw_codex_local = codex_compaction_events > 0 && !saw_remote;
    if embedded_local_attempt && saw_codex_local {
        return "mixed".into();
    }
    if embedded_local_attempt {
        return "codex_local_v0_150".into();
    }
    if saw_codex_local {
        return "codex_local_native_oracle".into();
    }
    if saw_remote {
        return "remote_v2".into();
    }
    "none".into()
}

fn requested_eval_engine_observation(requested: &str) -> &'static str {
    match requested {
        "codex-local-v0-150" => "codex_local_v0_150",
        "codex-local-native-oracle" => "codex_local_native_oracle",
        _ => "unknown",
    }
}

fn compaction_engine_matched(requested: &str, observed: &str) -> bool {
    if observed == "mixed" || observed == "unknown" {
        return false;
    }
    requested_eval_engine_observation(requested) == observed
}

fn default_schedule() -> String {
    "model-major".into()
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug)]
struct CommandOutput {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    first_byte_ms: Option<u64>,
}

pub fn agent_image(codex_version: &str) -> String {
    format!("{AGENT_IMAGE_PREFIX}:{}", codex_version.replace('.', "-"))
}

fn windows_runtime_root(paths: &EvalPaths, codex_version: &str) -> PathBuf {
    paths
        .project_root
        .join("target")
        .join("vellum-eval-runtime")
        .join(format!("codex-{codex_version}"))
}

pub(super) fn windows_runtime_executable(root: &Path) -> PathBuf {
    root.join("node_modules")
        .join("@openai")
        .join("codex-win32-x64")
        .join("vendor")
        .join("x86_64-pc-windows-msvc")
        .join("bin")
        .join("codex.exe")
}

pub(super) async fn prepare_windows_runtime(
    paths: &EvalPaths,
    codex_version: &str,
) -> AppResult<PathBuf> {
    let root = windows_runtime_root(paths, codex_version);
    let executable = windows_runtime_executable(&root);
    if !executable.is_file() {
        std::fs::create_dir_all(&root).map_err(io_error("create Windows eval runtime cache"))?;
        ensure_success(
            run_process(
                if cfg!(target_os = "windows") {
                    "npm.cmd"
                } else {
                    "npm"
                },
                &[
                    "install".into(),
                    "--prefix".into(),
                    root.to_string_lossy().into_owned(),
                    "--no-audit".into(),
                    "--no-fund".into(),
                    format!("@openai/codex@{codex_version}"),
                ],
                Some(&paths.project_root),
                None,
                Duration::from_secs(300),
                &[],
            )
            .await?,
            "install pinned Windows Codex eval runtime",
        )?;
    }
    let version = ensure_success(
        run_process(
            executable.to_string_lossy().as_ref(),
            &["--version".into()],
            None,
            None,
            Duration::from_secs(20),
            &[],
        )
        .await?,
        "verify pinned Windows Codex eval runtime",
    )?;
    if !version.stdout.contains(codex_version) && !version.stderr.contains(codex_version) {
        return Err(AppError::Message(format!(
            "Windows eval runtime does not report Codex {codex_version}: {}{}",
            version.stdout, version.stderr
        )));
    }
    Ok(root)
}

pub async fn doctor(
    paths: &EvalPaths,
    state: &AppState,
    codex_version: &str,
    profile: Option<&str>,
    executor: &str,
    oauth_account: Option<&str>,
    grok_account: Option<&str>,
) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();
    checks.push(
        command_check(
            "Docker CLI",
            "docker",
            &["--version"],
            Duration::from_secs(10),
        )
        .await,
    );
    checks.push(
        command_check(
            "Docker daemon",
            "docker",
            &["info", "--format", "{{.ServerVersion}}"],
            Duration::from_secs(15),
        )
        .await,
    );
    checks.push(disk_check(&paths.output_root).await);
    #[cfg(target_os = "macos")]
    {
        checks.push(macos_codex_app_check());
        checks.push(macos_codex_cli_check(codex_version).await);
        checks.push(macos_keychain_check(&state.data_root()));
        checks.push(macos_architecture_check().await);
        checks.push(
            command_check(
                "Xcode clang",
                "xcrun",
                &["--find", "clang"],
                Duration::from_secs(10),
            )
            .await,
        );
        checks.push(
            command_check(
                "DMG toolchain",
                "hdiutil",
                &["help"],
                Duration::from_secs(10),
            )
            .await,
        );
        checks.push(macos_proxy_port_check(state));
        checks
            .push(command_check("Node.js", "node", &["--version"], Duration::from_secs(10)).await);
        checks.push(command_check("pnpm", "pnpm", &["--version"], Duration::from_secs(10)).await);
    }
    let models = state
        .model_routes()
        .into_iter()
        .filter(|model| {
            state
                .routes()
                .iter()
                .find(|route| route.id == model.route_id)
                .map(|route| route.enabled && route.provider_kind != ProviderKind::Official)
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    checks.push(DoctorCheck {
        name: "Enabled third-party models".into(),
        ok: !models.is_empty(),
        detail: if models.is_empty() {
            "no enabled third-party model is exposed by Vellum".into()
        } else {
            models
                .iter()
                .map(|model| model.catalog_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        },
    });
    checks.extend(credential_checks(state).await);
    if let Some(grok_sel) = grok_account {
        let grok_manager = state.grok_accounts();
        match grok_manager.find_account_id(grok_sel) {
            Some(account_id) => checks.push(DoctorCheck {
                name: "Grok eval account".into(),
                ok: true,
                detail: format!("account {account_id} resolved"),
            }),
            None => checks.push(DoctorCheck {
                name: "Grok eval account".into(),
                ok: false,
                detail: format!("Grok account '{grok_sel}' not found in managed accounts"),
            }),
        }
    }
    let profile_models =
        profile.and_then(|profile| resolve_profile_models(state, profile, None).ok());
    if profile_models
        .as_deref()
        .is_some_and(|models| models_include_official_route(state, models))
    {
        match eval_oauth_manager(state, oauth_account).await {
            Ok(manager) => checks.push(official_oauth_doctor_check(state, &manager).await),
            Err(detail) => checks.push(DoctorCheck {
                name: "Official eval OAuth".into(),
                ok: false,
                detail,
            }),
        }
    }
    checks.extend(provider_probe_checks(state, profile_models.as_deref()).await);
    if let Some(profile) = profile {
        match resolve_profile_models(state, profile, None) {
            Ok(models) => {
                checks.push(DoctorCheck {
                    name: format!("Eval profile: {profile}"),
                    ok: true,
                    detail: models.join(", "),
                });
                let routes = state.routes();
                let model_routes = state.model_routes();
                let stale = models
                    .iter()
                    .filter_map(|catalog_id| {
                        let model = model_routes
                            .iter()
                            .find(|model| model.catalog_id == *catalog_id)?;
                        let route = routes.iter().find(|route| route.id == model.route_id)?;
                        if route.provider_kind == ProviderKind::Official
                            || route.provider_kind == ProviderKind::GrokCli
                        {
                            return None;
                        }
                        let current = route.model_capabilities.iter().any(|capability| {
                            capability.model.eq_ignore_ascii_case(&model.upstream_model)
                                && capability.probe_version
                                    == Some(crate::probe::HARNESS_PROBE_VERSION)
                        });
                        (!current).then(|| format!("{}:{}", route.name, model.upstream_model))
                    })
                    .collect::<Vec<_>>();
                checks.push(DoctorCheck {
                    name: "Codex dialect probe version".into(),
                    // A stale cached probe is advisory here: the quick gate
                    // exercises the real Codex request shape and is the
                    // authoritative compatibility check.
                    ok: true,
                    detail: if stale.is_empty() {
                        format!(
                            "all compatible routes use probe v{}",
                            crate::probe::HARNESS_PROBE_VERSION
                        )
                    } else {
                        format!(
                            "live gate will refresh stale evidence for: {}",
                            stale.join(", ")
                        )
                    },
                });
            }
            Err(error) => checks.push(DoctorCheck {
                name: format!("Eval profile: {profile}"),
                ok: false,
                detail: error.to_string(),
            }),
        }
    }
    checks.push(codex_container_check(codex_version).await);
    if executor == "windows" {
        let runtime = command_check(
            "Windows Codex runtime",
            "codex",
            &["--version"],
            Duration::from_secs(15),
        )
        .await;
        let version_matches = runtime.ok && runtime.detail.contains(codex_version);
        checks.push(DoctorCheck {
            name: runtime.name,
            ok: version_matches,
            detail: if version_matches {
                format!(
                    "{}; shell contract {}",
                    runtime.detail,
                    windows_shell_contract()
                )
            } else {
                format!(
                    "expected Codex {codex_version}; observed {}",
                    runtime.detail
                )
            },
        });
        checks.push(windows_workspace_write_check().await);
    } else if executor == "windows-sandbox" {
        let sandbox = PathBuf::from(std::env::var_os("WINDIR").unwrap_or_default())
            .join("System32")
            .join("WindowsSandbox.exe");
        checks.push(DoctorCheck {
            name: "Windows Sandbox".into(),
            ok: sandbox.is_file(),
            detail: if sandbox.is_file() {
                sandbox.display().to_string()
            } else {
                "enable the Containers-DisposableClientVM optional feature".into()
            },
        });
        let runtime = windows_runtime_root(paths, codex_version);
        let executable = windows_runtime_executable(&runtime);
        checks.push(DoctorCheck {
            name: "Windows Sandbox Codex runtime".into(),
            ok: executable.is_file(),
            detail: if executable.is_file() {
                format!("pinned Codex {codex_version}: {}", executable.display())
            } else {
                format!(
                    "runtime is not prepared; run vellum-eval prepare --suite <suite>: {}",
                    runtime.display()
                )
            },
        });
    }
    match EvalGateway::start(state.clone()).await {
        Ok(gateway) => {
            checks.push(DoctorCheck {
                name: "Ephemeral eval gateway".into(),
                ok: true,
                detail: format!("bound ephemeral port {}", gateway.port()),
            });
            gateway.shutdown().await;
        }
        Err(error) => checks.push(DoctorCheck {
            name: "Ephemeral eval gateway".into(),
            ok: false,
            detail: error.to_string(),
        }),
    }
    checks
}

#[cfg(target_os = "macos")]
fn macos_codex_app_check() -> DoctorCheck {
    let path = macos_codex_app_path();
    DoctorCheck {
        name: "Codex App (macOS)".into(),
        ok: path.is_some(),
        detail: path
            .map(|value| format!("bundle com.openai.codex: {}", value.display()))
            .unwrap_or_else(|| "ChatGPT.app / Codex.app was not found in Applications".into()),
    }
}

#[cfg(target_os = "macos")]
fn macos_codex_app_path() -> Option<PathBuf> {
    crate::install_paths::macos_codex_app_candidates(dirs::home_dir().as_deref())
        .into_iter()
        .find(|candidate| candidate.is_dir())
}

#[cfg(target_os = "macos")]
async fn macos_codex_cli_check(expected_version: &str) -> DoctorCheck {
    let Some(app) = macos_codex_app_path() else {
        return DoctorCheck {
            name: "Bundled Codex CLI".into(),
            ok: false,
            detail: "ChatGPT.app is unavailable".into(),
        };
    };
    let executable = app.join("Contents").join("Resources").join("codex");
    if !executable.is_file() {
        return DoctorCheck {
            name: "Bundled Codex CLI".into(),
            ok: false,
            detail: format!("not found: {}", executable.display()),
        };
    }
    let program = executable.to_string_lossy().into_owned();
    let mut check = command_check(
        "Bundled Codex CLI",
        &program,
        &["--version"],
        Duration::from_secs(15),
    )
    .await;
    if check.ok && !check.detail.contains(expected_version) {
        check.ok = false;
        check.detail = format!(
            "expected Codex {expected_version}; observed {}",
            check.detail
        );
    }
    check
}

#[cfg(target_os = "macos")]
async fn macos_architecture_check() -> DoctorCheck {
    let mut check = command_check(
        "macOS architecture",
        "uname",
        &["-m"],
        Duration::from_secs(10),
    )
    .await;
    if check.ok && check.detail.trim() != "arm64" {
        check.ok = false;
        check.detail = format!("expected arm64; observed {}", check.detail.trim());
    }
    check
}

#[cfg(target_os = "macos")]
fn macos_proxy_port_check(state: &AppState) -> DoctorCheck {
    let running = state.proxy_status().running;
    match std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 15721)) {
        Ok(listener) => {
            drop(listener);
            DoctorCheck {
                name: "Vellum proxy port".into(),
                ok: !running,
                detail: if running {
                    "state reports running, but port 15721 is available".into()
                } else {
                    "127.0.0.1:15721 is available".into()
                },
            }
        }
        Err(error) => DoctorCheck {
            name: "Vellum proxy port".into(),
            ok: running,
            detail: if running {
                "127.0.0.1:15721 is owned by the active Vellum proxy".into()
            } else {
                format!("127.0.0.1:15721 is occupied by another process: {error}")
            },
        },
    }
}

#[cfg(target_os = "macos")]
fn macos_keychain_check(root: &Path) -> DoctorCheck {
    match crate::crypto::JournalCipher::load_or_create(root) {
        Ok(cipher) => DoctorCheck {
            name: "Vellum Keychain".into(),
            ok: cipher.protection() == "macos-keychain",
            detail: cipher.protection().to_string(),
        },
        Err(error) => DoctorCheck {
            name: "Vellum Keychain".into(),
            ok: false,
            detail: error.to_string(),
        },
    }
}

async fn credential_checks(state: &AppState) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();
    for route in state
        .routes()
        .into_iter()
        .filter(|route| route.enabled && route.provider_kind != ProviderKind::Official)
    {
        let result = match route.provider_kind {
            ProviderKind::OpenAiCompatible => {
                match crate::credentials::load(&state.data_root(), &route.id) {
                    Ok(Some(_)) => Ok("encrypted bearer credential is available".to_string()),
                    Ok(None) if route.auth_kind == crate::model::AuthKind::None => {
                        Ok("provider does not require a credential".to_string())
                    }
                    Ok(None) => Err("provider credential is missing".to_string()),
                    Err(error) => Err(error.to_string()),
                }
            }
            ProviderKind::GrokCli => state
                .grok_accounts()
                .resolve_default()
                .await
                .map(|_| "Grok CLI session resolved".to_string())
                .map_err(|error| error.to_string()),
            ProviderKind::Official => continue,
        };
        checks.push(DoctorCheck {
            name: format!("Credential: {}", route.name),
            ok: result.is_ok(),
            detail: result.unwrap_or_else(|error| error),
        });
    }
    checks
}

/// Route facts a live Grok doctor probe must satisfy before it spends quota
/// (replaces `eff9409`'s helper of the same name/shape, updated for the
/// current shared-runtime catalog/session-registry plumbing landed by the
/// two prior pieces of this workstream). Pure and network-free: catalog id
/// resolution and endpoint shape only, so it can be unit-tested without any
/// live credential or connection.
pub(crate) fn grok_runtime_probe_plan(
    state: &AppState,
    route: &crate::model::Route,
    upstream_model: &str,
) -> Result<(String, String), String> {
    let model = state
        .model_routes()
        .into_iter()
        .find(|model| {
            model.route_id == route.id && model.upstream_model.eq_ignore_ascii_case(upstream_model)
        })
        .ok_or_else(|| format!("Grok catalog has no model `{upstream_model}`"))?;
    let runtime = crate::proxy_runtime_bridge::runtime_route(state, route, &model);
    if runtime.wire != vellum_proxy_runtime::RuntimeWireFormat::Responses {
        return Err(format!(
            "Grok model `{}` projected as {} instead of responses",
            runtime.upstream_model,
            runtime.wire.as_str()
        ));
    }
    let endpoint = format!("{}/responses", runtime.base_url.trim_end_matches('/'));
    if endpoint != "https://cli-chat-proxy.grok.com/v1/responses" {
        return Err(format!(
            "Grok endpoint is not the validated Responses URL: {endpoint}"
        ));
    }
    if endpoint.contains("/v1/v1") || endpoint.contains("/responses/responses") {
        return Err(format!("Grok endpoint has a duplicated path: {endpoint}"));
    }
    Ok((model.catalog_id, endpoint))
}

/// Probe one Grok model (4.6 or 4.5, called once per selected model — see
/// the `ProviderKind::GrokCli` target-expansion in `provider_probe_checks`
/// below) through the exact production path: `DesktopProxyRuntimeState` →
/// `ProxyRuntime::execute` → `/responses`. No shortcut/mock of that path —
/// this exercises the same shared catalog snapshot and Grok session registry
/// every live request goes through.
///
/// Only ever called from [`doctor`] (an explicit `vellum-eval doctor`
/// invocation), never from Desktop startup or a readiness check, so it only
/// spends Grok quota when a user (or CI eval run) explicitly asks for a
/// doctor pass.
async fn probe_grok_through_runtime(
    state: &AppState,
    route: &crate::model::Route,
    upstream_model: &str,
) -> DoctorCheck {
    let name = format!("Provider probe: {} ({upstream_model})", route.name);
    let (catalog_id, endpoint) = match grok_runtime_probe_plan(state, route, upstream_model) {
        Ok(plan) => plan,
        Err(detail) => {
            return DoctorCheck {
                name,
                ok: false,
                detail,
            }
        }
    };
    let runtime = match crate::proxy_runtime_bridge::DesktopProxyRuntimeState::new(state.clone()) {
        Ok(runtime) => runtime,
        Err(error) => {
            return DoctorCheck {
                name,
                ok: false,
                detail: format!("cannot start Desktop shared runtime: {error}"),
            }
        }
    };
    let request = vellum_proxy_runtime::RuntimeRequest {
        body: json!({
            "model": catalog_id,
            "input": "Reply with the single word OK.",
            "stream": true,
            "max_output_tokens": 32
        }),
        endpoint: vellum_proxy_runtime::RuntimeEndpoint::Responses,
        incoming_auth: vellum_proxy_runtime::IncomingAuthContext::default(),
        execution_environment: runtime.execution_environment(),
        metadata: vellum_proxy_runtime::RequestMetadata {
            request_id: format!("doctor-grok-{upstream_model}"),
            received_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0),
            ..Default::default()
        },
    };
    let started = Instant::now();
    let execution_id = vellum_proxy_runtime::exec::new_execution_id();
    let outcome = tokio::time::timeout(
        Duration::from_secs(90),
        runtime.proxy_runtime().execute(request, &execution_id),
    )
    .await;
    let elapsed_ms = started.elapsed().as_millis();
    match outcome {
        Err(_) => DoctorCheck {
            name,
            ok: false,
            detail: format!(
                "{catalog_id} → {upstream_model} via {endpoint} timed out after {elapsed_ms}ms waiting for a runtime response"
            ),
        },
        // `RuntimeError`'s Display never carries raw upstream bodies, tokens,
        // or account identifiers (see `crate::error::RuntimeError` and the
        // shared `bounded_provider_diagnostic`/`bounded_text` helpers this
        // whole crate's diagnostics funnel through) — safe to log verbatim.
        Ok(Err(error)) => DoctorCheck {
            name,
            ok: false,
            detail: format!(
                "{catalog_id} → {upstream_model} via {endpoint} failed: {} / {}",
                error.category(),
                error
            ),
        },
        Ok(Ok(response)) => match consume_grok_probe_response(response, started).await {
            Ok((status, first_byte_ms, completed)) => {
                // Usage-model attribution: the terminal event must have been
                // recorded under this exact upstream model/route, not a
                // catalog default or a different route's identity bleeding
                // through.
                let usage = runtime
                    .proxy_runtime()
                    .usage_records()
                    .ok()
                    .and_then(|records| {
                        records.into_iter().rev().find(|record| {
                            record.model.eq_ignore_ascii_case(upstream_model)
                                && record.route_id == route.id
                        })
                    });
                let usage_ok = usage.as_ref().is_some_and(|record| {
                    record.status == 200
                        && !record.model.to_ascii_lowercase().contains("gpt")
                });
                let ok = status == 200 && completed && usage_ok;
                DoctorCheck {
                    name,
                    ok,
                    detail: format!(
                        "{catalog_id} → {upstream_model} via {endpoint} HTTP {status} first_byte_ms={} completed={completed} usage={}",
                        first_byte_ms
                            .map(|ms| ms.to_string())
                            .unwrap_or_else(|| "none".into()),
                        usage
                            .map(|record| format!(
                                "{} {} status={}",
                                record.route_id, record.model, record.status
                            ))
                            .unwrap_or_else(|| "missing".into())
                    ),
                }
            }
            Err(detail) => DoctorCheck {
                name,
                ok: false,
                detail: format!("{catalog_id} → {upstream_model} via {endpoint}: {detail}"),
            },
        },
    }
}

/// Consume one Grok probe response down to (status, first_byte_ms, terminal
/// event observed). A stream-failure excerpt is bounded through
/// `vellum_proxy_runtime::diagnostics::bounded_text` — the same bounded-text
/// helper the rest of this codebase's diagnostics use — rather than an
/// unbounded raw body, and only ever a short SSE `event:`/error fragment,
/// never a token, email, or account identifier.
async fn consume_grok_probe_response(
    response: vellum_proxy_runtime::RuntimeResponse,
    started: Instant,
) -> Result<(u16, Option<u64>, bool), String> {
    use futures_util::StreamExt;
    match response {
        vellum_proxy_runtime::RuntimeResponse::Json(value) => {
            let completed = value.get("status").and_then(Value::as_str) == Some("completed")
                || value.get("output").is_some();
            Ok((200, None, completed))
        }
        vellum_proxy_runtime::RuntimeResponse::Sse(mut stream) => {
            let mut first_byte_ms = None;
            let mut completed = false;
            let mut failed = None;
            while let Some(item) = stream.next().await {
                let bytes = item.map_err(|error| error.to_string())?;
                if first_byte_ms.is_none() {
                    first_byte_ms = Some(started.elapsed().as_millis() as u64);
                }
                let text = String::from_utf8_lossy(&bytes);
                if text.contains("response.completed") {
                    completed = true;
                    break;
                }
                if text.contains("response.failed") {
                    failed = Some(vellum_proxy_runtime::diagnostics::bounded_text(&text, 240));
                    break;
                }
            }
            if let Some(failed) = failed {
                return Err(format!("stream failed: {failed}"));
            }
            if !completed {
                return Err("stream ended without response.completed".into());
            }
            Ok((200, first_byte_ms, completed))
        }
        vellum_proxy_runtime::RuntimeResponse::Raw { status, .. } => {
            Ok((status, None, (200..300).contains(&status)))
        }
    }
}

async fn provider_probe_checks(
    state: &AppState,
    profile_models: Option<&[String]>,
) -> Vec<DoctorCheck> {
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(8))
        // NVIDIA NIM may cold-start hosted deployments before returning SSE
        // headers. Keep the connection timeout strict, but allow enough time
        // for the provider's queue/cold-start phase.
        .timeout(Duration::from_secs(120))
        .user_agent(format!(
            "codex_cli_rs/{} (vellum-harness-eval-doctor)",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return vec![DoctorCheck {
                name: "Provider live probe".into(),
                ok: false,
                detail: error.to_string(),
            }]
        }
    };
    // DesktopCatalog only publishes the proxy-start snapshot. Doctor does not
    // bind a listener, but a live Grok probe still has to execute against the
    // same activated catalog Codex would see.
    state.activate_proxy_routes();
    let routes = state.routes();
    let model_routes = state.model_routes();
    let targets = if let Some(profile_models) = profile_models {
        profile_models
            .iter()
            .filter_map(|catalog_id| {
                let model = model_routes
                    .iter()
                    .find(|model| model.catalog_id == *catalog_id)?;
                let route = routes
                    .iter()
                    .find(|route| route.id == model.route_id)?
                    .clone();
                (route.provider_kind != ProviderKind::Official)
                    .then(|| (route, model.upstream_model.clone()))
            })
            .collect::<Vec<_>>()
    } else {
        let mut targets = Vec::new();
        for route in routes
            .into_iter()
            .filter(|route| route.enabled && route.provider_kind != ProviderKind::Official)
        {
            if route.provider_kind == ProviderKind::GrokCli {
                // Separately validate every selected Grok model (4.6, 4.5,
                // …): each gets its own catalog id, endpoint, and usage
                // attribution check, not one blended credential check for
                // the whole route.
                let selected = route
                    .selected_models
                    .clone()
                    .unwrap_or_else(|| route.models.clone());
                if selected.is_empty() {
                    targets.push((route.clone(), route.model.clone()));
                } else {
                    for model in selected {
                        targets.push((route.clone(), model));
                    }
                }
            } else {
                let model = route
                    .selected_models
                    .as_ref()
                    .unwrap_or(&route.models)
                    .first()
                    .cloned()
                    .unwrap_or_else(|| route.model.clone());
                targets.push((route, model));
            }
        }
        targets
    };
    let futures = targets.into_iter().map(|(route, model)| {
        let client = client.clone();
        let state = state.clone();
        async move {
            if route.provider_kind == ProviderKind::GrokCli {
                return probe_grok_through_runtime(&state, &route, &model).await;
            }
            if model.trim().is_empty() {
                return DoctorCheck {
                    name: format!("Provider probe: {}", route.name),
                    ok: false,
                    detail: "no selected model is available".into(),
                };
            }
            let base = route.base_url.trim_end_matches('/');
            let v1 = if base.ends_with("/v1") {
                base.to_string()
            } else {
                format!("{base}/v1")
            };
            let endpoint = match route.wire {
                crate::model::WireFormat::Responses => format!("{v1}/responses"),
                crate::model::WireFormat::Chat => format!("{v1}/chat/completions"),
            };
            let mut body = match route.wire {
                crate::model::WireFormat::Responses => {
                    crate::probe::build_responses_body("OK", &model)
                }
                crate::model::WireFormat::Chat => crate::probe::build_chat_body("OK", &model),
            };
            let nvidia_nim = prepare_doctor_probe_body(&v1, route.wire, &model, &mut body);
            let secret = crate::credentials::load(&state.data_root(), &route.id)
                .ok()
                .flatten();
            let mut request = client.post(endpoint).json(&body);
            if nvidia_nim {
                request = request.header(reqwest::header::ACCEPT, "text/event-stream");
            }
            if let Some(secret) = secret {
                request = request.bearer_auth(secret);
            }
            match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    let detail = if status.is_success() {
                        format!(
                            "{} accepted a minimal {}{} request",
                            model,
                            if nvidia_nim { "streaming " } else { "" },
                            status
                        )
                    } else {
                        let text = response.text().await.unwrap_or_default();
                        format!("{}: {}", status, redact_text(&text))
                    };
                    DoctorCheck {
                        name: format!("Provider probe: {}", route.name),
                        ok: status.is_success(),
                        detail,
                    }
                }
                Err(error) => DoctorCheck {
                    name: format!("Provider probe: {}", route.name),
                    ok: false,
                    detail: reqwest_error_detail(&error),
                },
            }
        }
    });
    futures_util::future::join_all(futures).await
}

fn models_include_official_route(state: &AppState, models: &[String]) -> bool {
    let routes = state.routes();
    let codex_paths = crate::codex::CodexPaths::discover(&state.data_root());
    let official_catalog = crate::catalog::read_official_catalog(&codex_paths.models_cache);
    crate::catalog::model_routes_with_official_catalog(&routes, official_catalog.as_ref())
        .iter()
        .any(|model| {
            models.contains(&model.catalog_id)
                && routes.iter().any(|route| {
                    route.id == model.route_id && route.provider_kind == ProviderKind::Official
                })
        })
}

async fn eval_oauth_manager(
    state: &AppState,
    selector: Option<&str>,
) -> Result<Arc<crate::codex_oauth::CodexOAuthManager>, String> {
    let Some(selector) = selector.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(state.codex_oauth());
    };
    if selector.eq_ignore_ascii_case("native") {
        let auth_path = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
            .ok_or_else(|| "cannot locate the native Codex home".to_string())?
            .join("auth.json");
        return crate::codex_oauth::CodexOAuthManager::new_with_native_access_token(
            state.data_root(),
            &auth_path,
        )
        .map(Arc::new)
        .map_err(|error| error.to_string());
    }
    let status = state.codex_oauth().status().await;
    let account = status
        .accounts
        .iter()
        .find(|account| {
            account.account_id == selector
                || account
                    .email
                    .as_deref()
                    .is_some_and(|email| email.eq_ignore_ascii_case(selector))
        })
        .ok_or_else(|| format!("Vellum-managed OpenAI account not found: {selector}"))?;
    crate::codex_oauth::CodexOAuthManager::new_with_default_override(
        state.data_root(),
        &account.account_id,
    )
    .map(Arc::new)
    .map_err(|error| error.to_string())
}

async fn official_oauth_preflight(
    state: &AppState,
    manager: &crate::codex_oauth::CodexOAuthManager,
) -> Result<String, String> {
    let auth = manager
        .valid_default_auth()
        .await
        .map_err(|error| format!("OpenAI OAuth refresh failed: {error}"))?
        .ok_or_else(|| {
            "the eval gateway cannot inherit Codex Desktop native auth; sign in to OpenAI OAuth in Vellum before running an Official-model eval"
                .to_string()
        })?;
    let route_id = state
        .routes()
        .into_iter()
        .find(|route| route.provider_kind == ProviderKind::Official)
        .map(|route| route.id)
        .unwrap_or_else(|| "openai-official".into());

    let first =
        crate::codex_quota::query(&auth.access_token, &auth.account_id, &route_id, true).await;
    match first {
        Ok(_) | Err(crate::codex_quota::CodexQuotaError::Parse(_)) => {
            Ok(format!("managed account {} accepted", auth.account_id))
        }
        Err(crate::codex_quota::CodexQuotaError::Unauthorized) => {
            let refreshed = manager
                .refresh_after_rejection(&auth.credential_id, &auth.access_token)
                .await
                .map_err(|error| {
                    format!("OpenAI OAuth was rejected and refresh failed: {error}")
                })?;
            match crate::codex_quota::query(
                &refreshed.access_token,
                &refreshed.account_id,
                &route_id,
                true,
            )
            .await
            {
                Ok(_) | Err(crate::codex_quota::CodexQuotaError::Parse(_)) => Ok(format!(
                    "managed account {} accepted after refresh",
                    refreshed.account_id
                )),
                Err(error) => Err(format!(
                    "OpenAI OAuth remains unusable after refresh: {error}"
                )),
            }
        }
        Err(error) => Err(format!("OpenAI OAuth preflight failed: {error}")),
    }
}

async fn official_oauth_doctor_check(
    state: &AppState,
    manager: &crate::codex_oauth::CodexOAuthManager,
) -> DoctorCheck {
    match official_oauth_preflight(state, manager).await {
        Ok(detail) => DoctorCheck {
            name: "Official eval OAuth".into(),
            ok: true,
            detail,
        },
        Err(detail) => DoctorCheck {
            name: "Official eval OAuth".into(),
            ok: false,
            detail,
        },
    }
}

fn prepare_doctor_probe_body(
    v1: &str,
    wire: crate::model::WireFormat,
    model: &str,
    body: &mut Value,
) -> bool {
    if wire == crate::model::WireFormat::Chat {
        crate::probe::apply_chat_probe_options(v1, model, body);
    }
    let nvidia_nim = v1.to_ascii_lowercase().contains("integrate.api.nvidia.com");
    if nvidia_nim {
        // NIM reasoning/coding models can keep a non-streaming completion
        // open while they generate internal reasoning. Doctor only needs to
        // verify authentication and request acceptance, so use the provider's
        // documented streaming surface and stop after headers.
        body["stream"] = json!(true);
    }
    nvidia_nim
}

fn reqwest_error_detail(error: &reqwest::Error) -> String {
    let mut messages = Vec::new();
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(value) = current {
        let message = value.to_string();
        if messages.last() != Some(&message) {
            messages.push(message);
        }
        current = value.source();
    }
    messages.join(" -> ")
}

async fn codex_container_check(codex_version: &str) -> DoctorCheck {
    let image = agent_image(codex_version);
    let inspected = run_process(
        "docker",
        &[
            "image".into(),
            "inspect".into(),
            "--format".into(),
            "{{.Id}}".into(),
            image.clone(),
        ],
        None,
        None,
        Duration::from_secs(15),
        &[],
    )
    .await;
    match inspected {
        Ok(output) if output.code == Some(0) => {
            let version = run_process(
                "docker",
                &[
                    "run".into(),
                    "--rm".into(),
                    "--network".into(),
                    "none".into(),
                    image,
                    "--version".into(),
                ],
                None,
                None,
                Duration::from_secs(30),
                &[],
            )
            .await;
            match version {
                Ok(output)
                    if output.code == Some(0)
                        && (output.stdout.contains(codex_version)
                            || output.stderr.contains(codex_version)) =>
                {
                    DoctorCheck {
                        name: "Codex CLI container".into(),
                        ok: true,
                        detail: format!("pinned Codex CLI {codex_version} is available"),
                    }
                }
                Ok(output) => DoctorCheck {
                    name: "Codex CLI container".into(),
                    ok: false,
                    detail: format!(
                        "image exists but does not report Codex {codex_version}: {}{}",
                        output.stdout, output.stderr
                    ),
                },
                Err(error) => DoctorCheck {
                    name: "Codex CLI container".into(),
                    ok: false,
                    detail: error.to_string(),
                },
            }
        }
        Ok(output)
            if output.stderr.contains("No such image")
                || output.stderr.contains("does not exist") =>
        {
            DoctorCheck {
                name: "Codex CLI container".into(),
                ok: true,
                detail: format!(
                    "{image} is not built yet; `vellum-eval prepare` will build and verify it"
                ),
            }
        }
        Ok(output) => DoctorCheck {
            name: "Codex CLI container".into(),
            ok: false,
            detail: redact_text(&(output.stderr + &output.stdout)),
        },
        Err(error) => DoctorCheck {
            name: "Codex CLI container".into(),
            ok: false,
            detail: error.to_string(),
        },
    }
}

async fn disk_check(path: &Path) -> DoctorCheck {
    let parent = path
        .ancestors()
        .find(|candidate| candidate.exists())
        .unwrap_or(path);
    #[cfg(target_os = "windows")]
    let (program, args) = (
        "powershell",
        vec![
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$d=Get-PSDrive -Name ([IO.Path]::GetPathRoot((Resolve-Path '.')).Substring(0,1)); [Console]::Write($d.Free)",
        ],
    );
    #[cfg(not(target_os = "windows"))]
    let (program, args) = ("df", vec!["-Pk", "."]);
    let output = run_process(
        program,
        &args
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>(),
        Some(parent),
        None,
        Duration::from_secs(10),
        &[],
    )
    .await;
    match output {
        Ok(output) if output.code == Some(0) => DoctorCheck {
            name: "Disk space".into(),
            ok: true,
            detail: output.stdout.trim().to_string(),
        },
        Ok(output) => DoctorCheck {
            name: "Disk space".into(),
            ok: false,
            detail: output.stderr,
        },
        Err(error) => DoctorCheck {
            name: "Disk space".into(),
            ok: false,
            detail: error.to_string(),
        },
    }
}

async fn command_check(name: &str, program: &str, args: &[&str], timeout: Duration) -> DoctorCheck {
    match run_process(
        program,
        &args
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>(),
        None,
        None,
        timeout,
        &[],
    )
    .await
    {
        Ok(output) if output.code == Some(0) => DoctorCheck {
            name: name.into(),
            ok: true,
            detail: output.stdout.trim().to_string(),
        },
        Ok(output) => DoctorCheck {
            name: name.into(),
            ok: false,
            detail: redact_text(&(output.stderr + &output.stdout)),
        },
        Err(error) => DoctorCheck {
            name: name.into(),
            ok: false,
            detail: error.to_string(),
        },
    }
}

pub async fn prepare(paths: &EvalPaths, suite_name: &str, codex_version: &str) -> AppResult<()> {
    let suite_path = paths.suite_path(suite_name);
    let suite = SuiteManifest::load(&suite_path)?;
    ensure_dataset_files(paths, &suite).await?;
    validate_dataset_files(paths, &suite)?;
    let suite_root = paths.prepared_root.join(&suite.name);
    std::fs::create_dir_all(&suite_root).map_err(io_error("create prepared suite directory"))?;
    let mut swe_graders = Vec::new();
    for task in &suite.tasks {
        materialize_task(paths, &suite_root, task).await?;
        if matches!(task.source, TaskSource::SweBench { .. }) {
            swe_graders.push(ensure_swe_grader(paths, task).await?);
        }
    }
    let dockerfile = paths.evals_root.join("docker").join("Dockerfile.agent");
    let docker_context = paths.evals_root.join("docker");
    ensure_success(
        run_process(
            "docker",
            &[
                "build".into(),
                "--build-arg".into(),
                format!("CODEX_VERSION={codex_version}"),
                "-t".into(),
                agent_image(codex_version),
                "-f".into(),
                docker_path(&dockerfile),
                docker_path(&docker_context),
            ],
            Some(&paths.project_root),
            None,
            Duration::from_secs(1800),
            &[],
        )
        .await?,
        "build Codex eval image",
    )?;
    let version = ensure_success(
        run_process(
            "docker",
            &[
                "run".into(),
                "--rm".into(),
                "--network".into(),
                "none".into(),
                agent_image(codex_version),
                "--version".into(),
            ],
            None,
            None,
            Duration::from_secs(30),
            &[],
        )
        .await?,
        "verify Codex eval image",
    )?;
    if !version.stdout.contains(codex_version) && !version.stderr.contains(codex_version) {
        return Err(AppError::Message(format!(
            "prepared image does not report pinned Codex CLI version {codex_version}: {}{}",
            version.stdout, version.stderr
        )));
    }
    let image_id = ensure_success(
        run_process(
            "docker",
            &[
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Id}}".into(),
                agent_image(codex_version),
            ],
            None,
            None,
            Duration::from_secs(30),
            &[],
        )
        .await?,
        "inspect Codex eval image",
    )?
    .stdout
    .trim()
    .to_string();
    let windows_runtime = if cfg!(target_os = "windows") {
        Some(prepare_windows_runtime(paths, codex_version).await?)
    } else {
        None
    };
    let prepared = json!({
        "suite": suite.name,
        "version": suite.version,
        "suiteHash": sha256_file(&suite_path)?,
        "datasetHashes": dataset_hashes(paths, &suite)?,
        "codexVersion": codex_version,
        "imageId": image_id,
        "preparedAt": chrono::Utc::now().to_rfc3339(),
        "tasks": suite.tasks.len(),
        "windowsRuntime": windows_runtime.map(|path| path.to_string_lossy().into_owned()),
        "sweGraders": swe_graders
    });
    write_json(&suite_root.join("prepared.json"), &prepared)?;
    Ok(())
}

/// Return `image`'s config blob digest, independent of the buildkit driver.
/// Docker-format images expose `config.digest` as a descriptor annotation,
/// but OCI-format images produced by a `docker-container` builder do not;
/// `docker save`'s OCI layout is the one representation that is identical
/// for both, so this falls back to reading the config digest out of the
/// saved archive's `manifest.json`. This is the deterministic content
/// identity for a pinned `image_digest`: `docker image inspect {{.Id}}` (the
/// manifest digest) can differ between builder drivers even when the config
/// and layer blobs are byte-for-byte identical, so it must never be compared
/// against a cross-build pinned digest (mirrors
/// `evals/tools/build_swe_grader.py`'s `image_config_digest`).
async fn image_config_digest(image: &str) -> AppResult<String> {
    let descriptor = run_process(
        "docker",
        &[
            "image".into(),
            "inspect".into(),
            "--format".into(),
            r#"{{ index .Descriptor.Annotations "config.digest" }}"#.into(),
            image.to_string(),
        ],
        None,
        None,
        Duration::from_secs(60),
        &[],
    )
    .await?;
    let trimmed = descriptor.stdout.trim();
    if trimmed.starts_with("sha256:") {
        return Ok(trimmed.to_string());
    }
    let temp =
        tempfile::tempdir().map_err(io_error("create image config digest temp directory"))?;
    let archive = temp.path().join("image.tar");
    ensure_success(
        run_process(
            "docker",
            &[
                "save".into(),
                image.to_string(),
                "-o".into(),
                archive.to_string_lossy().into_owned(),
            ],
            None,
            None,
            Duration::from_secs(300),
            &[],
        )
        .await?,
        "save image for config digest resolution",
    )?;
    let manifest_bytes = {
        use std::io::Read;
        let archive_file =
            std::fs::File::open(&archive).map_err(io_error("open saved image archive"))?;
        let mut archive_reader = tar::Archive::new(archive_file);
        let mut manifest_bytes: Option<Vec<u8>> = None;
        for entry in archive_reader
            .entries()
            .map_err(io_error("inspect saved image archive"))?
        {
            let mut entry = entry.map_err(io_error("read saved image archive entry"))?;
            let path = entry
                .path()
                .map_err(io_error("read saved image archive entry path"))?
                .into_owned();
            if path.to_string_lossy() == "manifest.json" {
                let mut bytes = Vec::new();
                entry
                    .read_to_end(&mut bytes)
                    .map_err(io_error("read saved image manifest.json"))?;
                manifest_bytes = Some(bytes);
                break;
            }
        }
        manifest_bytes.ok_or_else(|| {
            AppError::Message(format!(
                "saved image archive for {image} has no manifest.json"
            ))
        })?
    };
    config_digest_from_docker_save_manifest(&manifest_bytes)
}

async fn ensure_swe_grader(paths: &EvalPaths, task: &EvalTask) -> AppResult<Value> {
    match task.swe_grader_mode()? {
        SweGraderMode::Oci {
            image,
            config_digest,
        } => {
            let source = ensure_swe_grader_image(image, config_digest, &task.id).await?;
            Ok(json!({
                "taskId": task.id,
                "mode": "oci",
                "tag": image,
                "configDigest": config_digest,
                "source": source
            }))
        }
        SweGraderMode::Archive {
            tag,
            archive,
            config_digest,
        } => {
            let source =
                ensure_swe_grader_archive(paths, &task.id, tag, archive, config_digest).await?;
            Ok(json!({
                "taskId": task.id,
                "mode": "archive",
                "tag": tag,
                "configDigest": config_digest,
                "archiveSha256": archive.sha256,
                "source": source
            }))
        }
    }
}

async fn ensure_swe_grader_archive(
    paths: &EvalPaths,
    task_id: &str,
    tag: &str,
    archive: &super::manifest::GraderImageArchive,
    expected_config_digest: &str,
) -> AppResult<&'static str> {
    if image_inspect_ok(tag).await {
        verify_loaded_grader_tag(tag, expected_config_digest, task_id).await?;
        return Ok("local-tag");
    }
    let cache = swe_image_archive_path(paths, &archive.sha256);
    let client = BundleDownloadClient::production(
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(900))
            .build()
            .map_err(|error| AppError::Message(format!("create SWE archive client: {error}")))?,
    );
    let acquisition = client
        .acquire_hashed_archive(
            &archive.url,
            task_id,
            &cache,
            &archive.sha256,
            archive.size_bytes,
            true,
        )
        .await?;
    let tar_path = cache.with_extension("tar");
    let uncompressed = tokio::task::spawn_blocking({
        let cache = cache.clone();
        let tar_path = tar_path.clone();
        let limit = archive.uncompressed_size_bytes;
        move || gunzip_limited(&cache, &tar_path, limit)
    })
    .await
    .map_err(|error| AppError::Message(format!("gzip worker for {task_id}: {error}")))??;
    if uncompressed != archive.uncompressed_size_bytes {
        let _ = std::fs::remove_file(&tar_path);
        return Err(AppError::Message(format!(
            "SWE-bench image archive {task_id} decompressed to {uncompressed} bytes; expected {}",
            archive.uncompressed_size_bytes
        )));
    }
    let load = run_process(
        "docker",
        &[
            "load".into(),
            "--input".into(),
            tar_path.to_string_lossy().into_owned(),
        ],
        None,
        None,
        Duration::from_secs(600),
        &[],
    )
    .await;
    let _ = std::fs::remove_file(&tar_path);
    let output = load?;
    if output.code != Some(0) || output.timed_out {
        return Err(AppError::Message(format!(
            "docker load failed for {task_id}: {}",
            redact_text(&(output.stderr + &output.stdout))
        )));
    }
    let report = parse_docker_load_output(&output.stdout);
    validate_loaded_images(&report, tag)?;
    if !image_inspect_ok(tag).await {
        return Err(AppError::Message(format!(
            "docker load for {task_id} did not leave tag {tag}"
        )));
    }
    verify_loaded_grader_tag(tag, expected_config_digest, task_id).await?;
    Ok(match acquisition {
        ArchiveAcquisition::Cache => "archive-cache",
        ArchiveAcquisition::Download => "archive-download",
    })
}

async fn verify_loaded_grader_tag(
    tag: &str,
    expected_config_digest: &str,
    task_id: &str,
) -> AppResult<()> {
    let resolved_config = image_config_digest(tag).await?;
    if resolved_config != expected_config_digest {
        return Err(AppError::Message(format!(
            "SWE-bench grader image config digest mismatch for {task_id}: expected {expected_config_digest}, got {resolved_config}; the existing local tag was not deleted"
        )));
    }
    let platform = ensure_success(
        run_process(
            "docker",
            &[
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Os}}/{{.Architecture}}".into(),
                tag.to_string(),
            ],
            None,
            None,
            Duration::from_secs(30),
            &[],
        )
        .await?,
        &format!("inspect SWE-bench grader platform {tag}"),
    )?;
    if platform.stdout.trim() != "linux/amd64" {
        return Err(AppError::Message(format!(
            "SWE-bench grader image {tag} for {task_id} must be linux/amd64, got {}",
            platform.stdout.trim()
        )));
    }
    Ok(())
}

fn swe_image_archive_path(paths: &EvalPaths, archive_sha256: &str) -> PathBuf {
    paths
        .dataset_root
        .join("swebench")
        .join("_images")
        .join(format!("{archive_sha256}.tar.gz"))
}

/// Make sure `grader_image` is present locally (pulling the digest-pinned
/// OCI reference when the clean host has no cache), then verify both
/// identities: the OCI manifest digest in `graderImage` and the Docker
/// config digest stored as `image_digest`.
async fn ensure_swe_grader_image(
    grader_image: &str,
    expected_config_digest: &str,
    task_id: &str,
) -> AppResult<&'static str> {
    let parsed = parse_oci_digest_reference(grader_image).ok_or_else(|| {
        AppError::Message(format!(
            "eval task {task_id} graderImage must be an immutable OCI digest reference"
        ))
    })?;
    let local = image_inspect_ok(grader_image).await || image_inspect_ok(&parsed.digest).await;
    if !local {
        pull_grader_image(grader_image, task_id).await?;
    }
    let resolved_manifest = match image_manifest_digest(grader_image).await {
        Ok(digest) => digest,
        Err(_) => image_manifest_digest(&parsed.digest)
            .await
            .map_err(|error| {
                AppError::Message(format!(
                "SWE-bench grader image {grader_image} could not be inspected after pull: {error}"
            ))
            })?,
    };
    if resolved_manifest != parsed.digest {
        return Err(AppError::Message(format!(
            "SWE-bench grader image manifest digest mismatch for {task_id}: expected {}, got {resolved_manifest}",
            parsed.digest
        )));
    }
    let resolved_config = match image_config_digest(grader_image).await {
        Ok(digest) => digest,
        Err(_) => image_config_digest(&parsed.digest).await?,
    };
    if resolved_config != expected_config_digest {
        return Err(AppError::Message(format!(
            "SWE-bench grader image config digest mismatch for {task_id}: expected {expected_config_digest}, got {resolved_config}"
        )));
    }
    Ok(if local { "oci-local" } else { "oci-pull" })
}

async fn image_inspect_ok(image: &str) -> bool {
    run_process(
        "docker",
        &[
            "image".into(),
            "inspect".into(),
            "--format".into(),
            "{{.Id}}".into(),
            image.to_string(),
        ],
        None,
        None,
        Duration::from_secs(30),
        &[],
    )
    .await
    .is_ok_and(|output| output.code == Some(0) && !output.stdout.trim().is_empty())
}

async fn image_manifest_digest(image: &str) -> AppResult<String> {
    let descriptor = run_process(
        "docker",
        &[
            "image".into(),
            "inspect".into(),
            "--format".into(),
            "{{.Descriptor.Digest}}".into(),
            image.to_string(),
        ],
        None,
        None,
        Duration::from_secs(30),
        &[],
    )
    .await?;
    let trimmed = descriptor.stdout.trim();
    if trimmed.starts_with("sha256:") && trimmed.len() == 71 {
        return Ok(trimmed.to_string());
    }
    let image_id = ensure_success(
        run_process(
            "docker",
            &[
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Id}}".into(),
                image.to_string(),
            ],
            None,
            None,
            Duration::from_secs(30),
            &[],
        )
        .await?,
        &format!("inspect SWE-bench grader image {image}"),
    )?;
    let trimmed = image_id.stdout.trim();
    if trimmed.starts_with("sha256:") && trimmed.len() == 71 {
        Ok(trimmed.to_string())
    } else {
        Err(AppError::Message(format!(
            "SWE-bench grader image {image} has no OCI manifest digest"
        )))
    }
}

async fn pull_grader_image(image: &str, task_id: &str) -> AppResult<()> {
    let output = run_process(
        "docker",
        &["pull".into(), image.to_string()],
        None,
        None,
        Duration::from_secs(1800),
        &[],
    )
    .await?;
    if output.code == Some(0) && !output.timed_out {
        return Ok(());
    }
    let detail = redact_text(&(output.stderr + &output.stdout));
    let lower = detail.to_ascii_lowercase();
    if lower.contains("unauthorized")
        || lower.contains("denied")
        || lower.contains("authentication required")
        || lower.contains("no basic auth credentials")
    {
        return Err(AppError::Message(format!(
            "SWE-bench grader image {image} for {task_id} is not present locally and docker pull was unauthorized; log in to ghcr.io (`docker login ghcr.io`) with a token that can read the private package"
        )));
    }
    Err(AppError::Message(format!(
        "SWE-bench grader image {image} for {task_id} is not present locally and docker pull failed: {detail}"
    )))
}

fn validate_dataset_files(paths: &EvalPaths, suite: &SuiteManifest) -> AppResult<()> {
    let needs_humaneval = suite
        .tasks
        .iter()
        .any(|task| matches!(task.source, TaskSource::HumanEval { .. }));
    let needs_mbpp = suite
        .tasks
        .iter()
        .any(|task| matches!(task.source, TaskSource::Mbpp { .. }));
    for (needed, file, expected_hash) in [
        (needs_humaneval, "HumanEval.jsonl.gz", HUMANEVAL_SHA256),
        (needs_mbpp, "sanitized-mbpp.json", MBPP_SHA256),
    ] {
        let path = paths.dataset_root.join(file);
        if needed && !path.is_file() {
            return Err(AppError::Message(format!(
                "required public dataset is missing: {}",
                path.display()
            )));
        }
        if needed && sha256_file(&path)? != expected_hash {
            return Err(AppError::Message(format!(
                "public dataset hash mismatch for {}; remove it and rerun prepare",
                path.display()
            )));
        }
    }
    for task in &suite.tasks {
        if let TaskSource::SweBench {
            instance_id,
            dataset_revision,
            bundle_sha256,
            ..
        } = &task.source
        {
            let path = swe_bundle_path(paths, dataset_revision, instance_id);
            if !path.is_file() {
                return Err(AppError::Message(format!(
                    "required pinned SWE-bench bundle is missing: {}",
                    path.display()
                )));
            }
            if sha256_file(&path)? != *bundle_sha256 {
                return Err(AppError::Message(format!(
                    "SWE-bench bundle hash mismatch for {}; remove it and rerun prepare",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

async fn ensure_dataset_files(paths: &EvalPaths, suite: &SuiteManifest) -> AppResult<()> {
    let needs_humaneval = suite
        .tasks
        .iter()
        .any(|task| matches!(task.source, TaskSource::HumanEval { .. }));
    let needs_mbpp = suite
        .tasks
        .iter()
        .any(|task| matches!(task.source, TaskSource::Mbpp { .. }));
    std::fs::create_dir_all(&paths.dataset_root)
        .map_err(io_error("create public dataset directory"))?;
    for (needed, file, url, expected_hash) in [
        (
            needs_humaneval,
            "HumanEval.jsonl.gz",
            "https://raw.githubusercontent.com/openai/human-eval/master/data/HumanEval.jsonl.gz",
            HUMANEVAL_SHA256,
        ),
        (
            needs_mbpp,
            "sanitized-mbpp.json",
            "https://raw.githubusercontent.com/google-research/google-research/master/mbpp/sanitized-mbpp.json",
            MBPP_SHA256,
        ),
    ] {
        let path = paths.dataset_root.join(file);
        if !needed || path.is_file() {
            continue;
        }
        let response = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|error| AppError::Message(format!("create dataset client: {error}")))?
            .get(url)
            .send()
            .await
            .map_err(|error| AppError::Message(format!("download {file}: {error}")))?
            .error_for_status()
            .map_err(|error| AppError::Message(format!("download {file}: {error}")))?;
        let bytes = response
            .bytes()
            .await
            .map_err(|error| AppError::Message(format!("read downloaded {file}: {error}")))?;
        let actual_hash = format!("{:x}", Sha256::digest(&bytes));
        if actual_hash != expected_hash {
            return Err(AppError::Message(format!(
                "downloaded dataset {file} has unexpected SHA-256 {actual_hash}"
            )));
        }
        let temporary = path.with_extension(format!("download-{}", std::process::id()));
        std::fs::write(&temporary, &bytes).map_err(io_error("write downloaded dataset"))?;
        std::fs::rename(&temporary, &path).map_err(io_error("install downloaded dataset"))?;
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|error| AppError::Message(format!("create SWE-bench client: {error}")))?;
    for task in &suite.tasks {
        if let TaskSource::SweBench {
            instance_id,
            dataset_revision,
            bundle_sha256,
            bundle_url,
            ..
        } = &task.source
        {
            let path = swe_bundle_path(paths, dataset_revision, instance_id);
            if path.is_file() {
                continue;
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(io_error("create SWE-bench dataset directory"))?;
            }
            let downloader = BundleDownloadClient::production(client.clone());
            let temporary = path.with_extension(format!("download-{}", std::process::id()));
            let actual_hash = downloader
                .download_bundle_url(bundle_url, instance_id, &temporary)
                .await?;
            install_hashed_download(&temporary, &path, &actual_hash, bundle_sha256, instance_id)?;
        }
    }
    Ok(())
}

async fn materialize_task(paths: &EvalPaths, suite_root: &Path, task: &EvalTask) -> AppResult<()> {
    let output = suite_root.join(&task.id);
    if output.exists() {
        std::fs::remove_dir_all(&output).map_err(io_error("replace prepared task"))?;
    }
    match &task.source {
        TaskSource::HumanEval { task_id } => {
            materialize_public(paths, "humaneval", task_id, false, &output).await
        }
        TaskSource::Mbpp {
            task_id,
            multi_file,
        } => materialize_public(paths, "mbpp", task_id, *multi_file, &output).await,
        TaskSource::Fixture { path } => {
            let source = paths.evals_root.join("fixtures").join(path);
            copy_tree(&source, &output.join("workspace"))?;
            let hidden = source.join(".hidden");
            if hidden.exists() {
                copy_tree(&hidden, &output.join("hidden"))?;
                let copied_hidden = output.join("workspace").join(".hidden");
                if copied_hidden.exists() {
                    std::fs::remove_dir_all(copied_hidden)
                        .map_err(io_error("remove hidden fixture from workspace"))?;
                }
            } else {
                std::fs::create_dir_all(output.join("hidden"))
                    .map_err(io_error("create empty fixture grader directory"))?;
            }
            Ok(())
        }
        TaskSource::Git {
            repository,
            commit,
            subdirectory,
        } => {
            std::fs::create_dir_all(&output).map_err(io_error("create git task directory"))?;
            ensure_success(
                run_process(
                    "git",
                    &[
                        "clone".into(),
                        "--no-checkout".into(),
                        repository.clone(),
                        docker_path(&output.join("repo")),
                    ],
                    None,
                    None,
                    Duration::from_secs(600),
                    &[],
                )
                .await?,
                "clone eval repository",
            )?;
            ensure_success(
                run_process(
                    "git",
                    &["checkout".into(), "--detach".into(), commit.clone()],
                    Some(&output.join("repo")),
                    None,
                    Duration::from_secs(120),
                    &[],
                )
                .await?,
                "checkout eval repository commit",
            )?;
            let source = subdirectory
                .as_ref()
                .map(|subdirectory| output.join("repo").join(subdirectory))
                .unwrap_or_else(|| output.join("repo"));
            copy_tree(&source, &output.join("workspace"))?;
            std::fs::create_dir_all(output.join("hidden"))
                .map_err(io_error("create git task grader directory"))
        }
        TaskSource::SweBench {
            instance_id,
            dataset_revision,
            image_digest,
            ..
        } => {
            std::fs::create_dir_all(&output)
                .map_err(io_error("create SWE-bench task directory"))?;
            let archive = swe_bundle_path(paths, dataset_revision, instance_id);
            let archive_file =
                std::fs::File::open(&archive).map_err(io_error("open pinned SWE-bench bundle"))?;
            let mut archive_reader = tar::Archive::new(archive_file);
            for entry in archive_reader
                .entries()
                .map_err(io_error("inspect pinned SWE-bench bundle"))?
            {
                let mut entry = entry.map_err(io_error("read pinned SWE-bench entry"))?;
                let path = entry
                    .path()
                    .map_err(io_error("read pinned SWE-bench entry path"))?
                    .into_owned();
                let normalized = path.to_string_lossy().trim_end_matches('/').to_string();
                if !safe_archive_path(&normalized) {
                    return Err(AppError::Message(format!(
                        "SWE-bench bundle {instance_id} contains unsafe archive path {normalized}"
                    )));
                }
                if !entry
                    .unpack_in(&output)
                    .map_err(io_error("extract pinned SWE-bench entry"))?
                {
                    return Err(AppError::Message(format!(
                        "SWE-bench bundle {instance_id} entry escaped the output directory: {normalized}"
                    )));
                }
            }
            if !output.join("workspace").is_dir() || !output.join("hidden").is_dir() {
                return Err(AppError::Message(format!(
                    "SWE-bench bundle {instance_id} must contain workspace/ and hidden/"
                )));
            }
            normalize_shell_script(&output.join("hidden").join("eval.sh"))?;
            // A checked-in focused grader may tighten a broad upstream eval
            // script for a fast provider qualification gate. Its hash is part
            // of `dataset_hashes`, so changing the override invalidates every
            // prepared suite instead of silently changing acceptance.
            let grader_override = swe_grader_override_path(paths, &task.id);
            if grader_override.is_file() {
                let target = output.join("hidden").join("eval.sh");
                std::fs::copy(&grader_override, &target)
                    .map_err(io_error("install pinned SWE-bench grader override"))?;
                normalize_shell_script(&target)?;
            }
            std::fs::write(output.join("grader-image.txt"), image_digest)
                .map_err(io_error("write SWE-bench grader image pin"))
        }
    }
}

fn normalize_shell_script(path: &Path) -> AppResult<()> {
    if !path.is_file() {
        return Ok(());
    }
    let bytes = std::fs::read(path).map_err(io_error("read shell grader"))?;
    if !bytes.windows(2).any(|pair| pair == b"\r\n") {
        return Ok(());
    }
    let normalized = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
    std::fs::write(path, normalized.as_bytes()).map_err(io_error("normalize shell grader"))
}

fn swe_bundle_path(paths: &EvalPaths, revision: &str, instance_id: &str) -> PathBuf {
    paths
        .dataset_root
        .join("swebench")
        .join(revision)
        .join(format!("{instance_id}.tar"))
}

fn swe_grader_override_path(paths: &EvalPaths, task_id: &str) -> PathBuf {
    paths
        .evals_root
        .join("graders")
        .join(format!("{task_id}.sh"))
}

fn safe_archive_path(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && !path.is_absolute()
        && !value.contains('\\')
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

async fn materialize_public(
    paths: &EvalPaths,
    kind: &str,
    task_id: &str,
    multi_file: bool,
    output: &Path,
) -> AppResult<()> {
    let script = paths.evals_root.join("tools").join("materialize.py");
    let mut args = vec![
        docker_path(&script),
        "--dataset-root".into(),
        docker_path(&paths.dataset_root),
        "--kind".into(),
        kind.into(),
        "--task-id".into(),
        task_id.into(),
        "--output".into(),
        docker_path(output),
    ];
    if multi_file {
        args.push("--multi-file".into());
    }
    ensure_success(
        run_process(
            "python",
            &args,
            Some(&paths.project_root),
            None,
            Duration::from_secs(60),
            &[],
        )
        .await?,
        &format!("materialize {kind} {task_id}"),
    )
    .map(|_| ())
}

struct RunLifecycleGuard {
    run_root: PathBuf,
    run_record: RunRecord,
    finalized: bool,
}

impl RunLifecycleGuard {
    fn new(run_root: PathBuf, run_record: RunRecord) -> Self {
        Self {
            run_root,
            run_record,
            finalized: false,
        }
    }

    fn finalize(&mut self, status: &str) -> AppResult<()> {
        self.run_record.status = status.to_string();
        self.run_record.completed_at = Some(chrono::Utc::now().to_rfc3339());
        write_json(&self.run_root.join("run.json"), &self.run_record)?;
        self.finalized = true;
        Ok(())
    }
}

impl Drop for RunLifecycleGuard {
    fn drop(&mut self) {
        if !self.finalized {
            self.run_record.status = "aborted".into();
            self.run_record.completed_at = Some(chrono::Utc::now().to_rfc3339());
            let _ = write_json(&self.run_root.join("run.json"), &self.run_record);
        }
    }
}

pub async fn run(mut options: RunOptions) -> AppResult<PathBuf> {
    let paths = EvalPaths::discover(options.dataset_root.clone())?;
    let suite_path = paths.suite_path(&options.suite);
    let suite = SuiteManifest::load(&suite_path)?;
    if let Some(suite_profile) = suite.ablation_profile.as_deref() {
        match options.ablation_profile.as_deref() {
            None => options.ablation_profile = Some(suite_profile.to_string()),
            Some(cli) if cli.eq_ignore_ascii_case(suite_profile) => {}
            Some(cli) => {
                return Err(AppError::Message(format!(
                    "suite ablation profile {suite_profile} does not match --ablation-profile {cli}"
                )))
            }
        }
    }
    let enhanced_identity = match options.ablation_profile.as_deref() {
        Some(profile) => Some(require_verified_enhanced_runtime(profile, &paths)?),
        None => None,
    };
    let prepared_root = paths.prepared_root.join(&suite.name);
    let prepared_path = prepared_root.join("prepared.json");
    if !prepared_path.is_file() {
        return Err(AppError::Message(format!(
            "suite {} is not prepared; run `vellum-eval prepare --suite {}` first",
            suite.name, suite.name
        )));
    }
    let suite_hash = sha256_file(&suite_path)?;
    let current_dataset_hashes = dataset_hashes(&paths, &suite)?;
    let prepared: Value = serde_json::from_slice(
        &std::fs::read(&prepared_path).map_err(io_error("read prepared suite metadata"))?,
    )
    .map_err(|error| AppError::Message(format!("invalid prepared suite metadata: {error}")))?;
    let prepared_hashes = prepared
        .get("datasetHashes")
        .cloned()
        .and_then(|value| serde_json::from_value::<HashMap<String, String>>(value).ok())
        .unwrap_or_default();
    if prepared.get("suiteHash").and_then(Value::as_str) != Some(suite_hash.as_str())
        || prepared.get("codexVersion").and_then(Value::as_str)
            != Some(options.codex_version.as_str())
        || prepared_hashes != current_dataset_hashes
    {
        return Err(AppError::Message(format!(
            "prepared suite metadata is stale; rerun `vellum-eval prepare --suite {} --codex-version {}`",
            suite.name, options.codex_version
        )));
    }
    let current_image_id = ensure_success(
        run_process(
            "docker",
            &[
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Id}}".into(),
                agent_image(&options.codex_version),
            ],
            None,
            None,
            Duration::from_secs(30),
            &[],
        )
        .await?,
        "inspect prepared Codex eval image",
    )?
    .stdout
    .trim()
    .to_string();
    if prepared.get("imageId").and_then(Value::as_str) != Some(current_image_id.as_str()) {
        return Err(AppError::Message(format!(
            "prepared Docker image changed; rerun `vellum-eval prepare --suite {}`",
            suite.name
        )));
    }
    let state = AppState::new();
    let configured_routes = state.routes();
    let codex_paths = crate::codex::CodexPaths::discover(&state.data_root());
    let official_catalog = crate::catalog::read_official_catalog(&codex_paths.models_cache);
    let base_catalog =
        crate::catalog::catalog_json_with_official(&state.routes(), official_catalog.as_ref());
    let named_phase_models = suite
        .tasks
        .iter()
        .flat_map(|task| task.phases.iter())
        .filter_map(|phase| match &phase.model {
            PhaseModel::Named(model) => Some(model.clone()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let available = crate::catalog::model_routes_with_official_catalog(
        &state.routes(),
        official_catalog.as_ref(),
    )
    .into_iter()
    .filter(|model| {
        configured_routes.iter().any(|route| {
            route.id == model.route_id
                && route.enabled
                && (route.provider_kind != ProviderKind::Official
                    || options.profile.is_some()
                    || options
                        .control_model
                        .as_deref()
                        .is_some_and(|control| control == model.catalog_id)
                    || named_phase_models.contains(&model.catalog_id))
        })
    })
    .map(|model| (model.catalog_id.clone(), model))
    .collect::<HashMap<_, _>>();
    let provider_names = available
        .iter()
        .map(|(catalog_id, model)| {
            let name = configured_routes
                .iter()
                .find(|route| route.id == model.route_id)
                .map(|route| route.name.clone())
                .unwrap_or_else(|| model.route_id.clone());
            (catalog_id.clone(), name)
        })
        .collect::<HashMap<_, _>>();
    let official_models = available
        .iter()
        .filter(|(_, model)| {
            configured_routes.iter().any(|route| {
                route.id == model.route_id && route.provider_kind == ProviderKind::Official
            })
        })
        .map(|(catalog_id, _)| catalog_id.clone())
        .collect::<HashSet<_>>();
    // Issue #6 Phase 0: record which harness contract each model ran under, so
    // an A/B comparison can attribute a change to the variant under test.
    let harness_profiles = available
        .iter()
        .map(|(catalog_id, model)| {
            let profile = configured_routes
                .iter()
                .find(|route| route.id == model.route_id)
                .map(|route| crate::harness::resolve(route.provider_kind, route.wire))
                .unwrap_or_else(|| crate::harness::HarnessProfile::generic(model.wire.into()));
            (catalog_id.clone(), profile.kind.as_str().to_string())
        })
        .collect::<HashMap<_, _>>();
    let mut requested_models = if let Some(profile) = options.profile.as_deref() {
        resolve_profile_models(&state, profile, options.lane.as_deref())?
    } else {
        options.models.clone()
    };
    if let Some(control) = &options.control_model {
        if !requested_models.contains(control) {
            requested_models.push(control.clone());
        }
    }
    if let Some(provider) = &options.provider_filter {
        requested_models.retain(|model| {
            provider_names
                .get(model)
                .is_some_and(|name| name.eq_ignore_ascii_case(provider))
        });
    }
    for model in &requested_models {
        if !available.contains_key(model) {
            return Err(AppError::Message(format!(
                "eval model is not enabled in Vellum: {model}"
            )));
        }
    }
    for model in &named_phase_models {
        if !available.contains_key(model) {
            return Err(AppError::Message(format!(
                "eval phase model is not enabled in Vellum: {model}"
            )));
        }
    }
    if requested_models.is_empty() {
        return Err(AppError::Message(
            "at least one eval model is required".into(),
        ));
    }
    if requested_models.iter().collect::<HashSet<_>>().len() != requested_models.len() {
        return Err(AppError::Message(
            "eval model list contains duplicate catalog IDs".into(),
        ));
    }
    let eval_oauth = eval_oauth_manager(&state, options.oauth_account.as_deref())
        .await
        .map_err(AppError::Message)?;
    let mut credential_models = requested_models.clone();
    credential_models.extend(named_phase_models.iter().cloned());
    let any_model_official = credential_models.iter().any(|catalog_id| {
        available.get(catalog_id).is_some_and(|model| {
            configured_routes.iter().any(|route| {
                route.id == model.route_id && route.provider_kind == ProviderKind::Official
            })
        })
    });

    if matches!(
        options.compaction_engine.as_str(),
        "codex-local-v0-150" | "codex-local-native-oracle"
    ) && any_model_official
    {
        let official_models: Vec<_> = credential_models
            .iter()
            .filter(|catalog_id| {
                available.get(*catalog_id).is_some_and(|model| {
                    configured_routes.iter().any(|route| {
                        route.id == model.route_id && route.provider_kind == ProviderKind::Official
                    })
                })
            })
            .cloned()
            .collect();
        return Err(AppError::Message(format!(
                "compaction-engine '{}' is a third-party A/B arm and cannot be used with Official routes: {}",
                options.compaction_engine,
                official_models.join(", ")
            )));
    }

    if models_include_official_route(&state, &credential_models) {
        official_oauth_preflight(&state, &eval_oauth)
            .await
            .map_err(|error| {
                AppError::Message(format!(
                    "Official-model eval preflight failed before any task was started: {error}"
                ))
            })?;
    }
    if requested_models.len() < 2
        && suite
            .tasks
            .iter()
            .filter(|task| task_matches_options(task, &options))
            .take(options.max_tasks.unwrap_or(usize::MAX))
            .any(|task| {
                matches!(
                    task.transition_mode,
                    TransitionMode::OrderedPair
                        | TransitionMode::RoundTrip
                        | TransitionMode::AllRoundTrips
                ) || task
                    .phases
                    .iter()
                    .any(|phase| matches!(&phase.model, PhaseModel::Next))
            })
    {
        return Err(AppError::Message(format!(
            "suite {} contains model-switch cases and requires at least two models; use the smoke suite for a single-model run",
            suite.name
        )));
    }
    let repeat = options.repeat.unwrap_or(suite.default_repeat).max(1);
    let run_id = options.resume_run.clone().unwrap_or_else(run_id);
    let run_root = paths.output_root.join(&run_id);
    std::fs::create_dir_all(&run_root).map_err(io_error("create eval run directory"))?;
    for directory in ["traces", "patches", "test-output", "cases", "attempts"] {
        std::fs::create_dir_all(run_root.join(directory))
            .map_err(io_error("create eval output directory"))?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(run_root.join("results.jsonl"))
        .map_err(io_error("create eval results file"))?;
    let hashed_oauth_account = options
        .oauth_account
        .as_deref()
        .map(vellum_proxy_runtime::account_hash);
    let hashed_grok_account = options
        .grok_account
        .as_deref()
        .map(vellum_proxy_runtime::account_hash);
    let run_record = if options.resume_run.is_some() && run_root.join("run.json").is_file() {
        let existing = serde_json::from_slice::<RunRecord>(
            &std::fs::read(run_root.join("run.json")).map_err(io_error("read resumed eval run"))?,
        )
        .map_err(|error| AppError::Message(format!("invalid resumed run metadata: {error}")))?;
        if existing.suite != suite.name
            || existing.suite_hash != suite_hash
            || existing.dataset_hashes != current_dataset_hashes
            || existing.models != requested_models
            || existing.oauth_account != hashed_oauth_account
            || existing.grok_account != hashed_grok_account
            || existing.max_official_turns != options.max_official_turns
            || existing.profile != options.profile
            || existing.lane != options.lane
            || existing.transition != options.transition
            || existing.codex_version != options.codex_version
            || existing.executor != options.executor
            || existing.catalog_mode != options.catalog_mode
            || existing.container_image_id != current_image_id
            || existing.repeat != repeat
            || existing.natural_context != options.natural_context
            || existing.seed != options.seed
            || existing.task_filter != options.task_filter
            || existing.category_filter != options.category_filter
            || existing.provider_filter != options.provider_filter
            || existing.tag_filter != options.tag_filter
            || existing.control_model != options.control_model
            || existing.baseline_run != options.baseline_run
            || existing.compaction_engine != options.compaction_engine
            || existing.recovery_mode != options.recovery_mode
            || existing.grok_compaction != options.grok_compaction
            || existing.schedule != options.schedule
            || existing.fail_fast != options.fail_fast
            || existing.ablation_profile != options.ablation_profile
            || existing.runtime_digest
                != enhanced_identity
                    .as_ref()
                    .map(|identity| identity.runtime_digest.clone())
            || existing.enhanced_codex_commit
                != enhanced_identity
                    .as_ref()
                    .map(|identity| identity.enhanced_codex_commit.clone())
            || existing.enhanced_artifact_sha256
                != enhanced_identity
                    .as_ref()
                    .map(|identity| identity.artifact_sha256.clone())
        {
            return Err(AppError::Message(
                "resumed run does not match the requested suite, dataset, models, repeat count, or Codex version".into(),
            ));
        }
        let mut resumed = existing;
        if resumed.status == "running" {
            // A prior process did not terminate cleanly; ensure status transitions back to active running
            log::info!(
                "resuming run {} previously marked as running",
                resumed.run_id
            );
        }
        resumed.status = "running".into();
        resumed.pid = Some(std::process::id());
        resumed.completed_at = None;
        resumed
    } else {
        RunRecord {
            run_id: run_id.clone(),
            status: "running".into(),
            pid: Some(std::process::id()),
            suite: suite.name.clone(),
            suite_version: suite.version.clone(),
            suite_hash: suite_hash.clone(),
            dataset_hashes: current_dataset_hashes.clone(),
            vellum_git_sha: git_sha(&paths.project_root).await,
            vellum_version: env!("CARGO_PKG_VERSION").into(),
            codex_version: options.codex_version.clone(),
            executor: options.executor.clone(),
            catalog_mode: options.catalog_mode.clone(),
            executor_platform: if matches!(options.executor.as_str(), "windows" | "windows-sandbox")
            {
                options.executor.clone()
            } else {
                "linux-container".into()
            },
            shell_contract: if matches!(options.executor.as_str(), "windows" | "windows-sandbox") {
                windows_shell_contract().into()
            } else {
                "linux-bash".into()
            },
            container_image_id: current_image_id,
            models: requested_models.clone(),
            oauth_account: hashed_oauth_account,
            grok_account: hashed_grok_account,
            max_official_turns: options.max_official_turns,
            profile: options.profile.clone(),
            lane: options.lane.clone(),
            transition: options.transition.clone(),
            repeat,
            seed: options.seed,
            max_tasks: options.max_tasks,
            max_wall_seconds: options.max_wall_seconds,
            max_total_tokens: options.max_total_tokens,
            natural_context: options.natural_context,
            task_filter: options.task_filter.clone(),
            category_filter: options.category_filter.clone(),
            provider_filter: options.provider_filter.clone(),
            tag_filter: options.tag_filter.clone(),
            control_model: options.control_model.clone(),
            baseline_run: options.baseline_run.clone(),
            compaction_engine: options.compaction_engine.clone(),
            recovery_mode: options.recovery_mode.clone(),
            eval_provider_display_name: eval_codex_provider_display_name(
                &options.compaction_engine,
            )
            .into(),
            grok_compaction: options.grok_compaction.clone(),
            canonical_contract_version: if options.compaction_engine == "legacy" {
                1
            } else {
                2
            },
            schedule: options.schedule.clone(),
            fail_fast: options.fail_fast.clone(),
            ablation_profile: options.ablation_profile.clone(),
            runtime_digest: enhanced_identity
                .as_ref()
                .map(|identity| identity.runtime_digest.clone()),
            enhanced_codex_commit: enhanced_identity
                .as_ref()
                .map(|identity| identity.enhanced_codex_commit.clone()),
            enhanced_feature_flags: enhanced_identity
                .as_ref()
                .map(|identity| identity.feature_flags),
            enhanced_artifact_sha256: enhanced_identity
                .as_ref()
                .map(|identity| identity.artifact_sha256.clone()),
            started_at: chrono::Utc::now().to_rfc3339(),
            completed_at: None,
            task_count: suite.tasks.len(),
        }
    };
    write_json(&run_root.join("run.json"), &run_record)?;
    let mut lifecycle_guard = RunLifecycleGuard::new(run_root.clone(), run_record);

    let completed = completed_case_ids(&run_root.join("results.jsonl"))?;
    let prior_attempts = case_attempt_counts(&run_root.join("results.jsonl"))?;
    let mut total_tokens_used = tokens_in_results(&run_root.join("results.jsonl"))?;
    let eval_data = tempfile::tempdir().map_err(|error| {
        AppError::Message(format!("cannot create isolated eval state: {error}"))
    })?;
    let mut selected_route_ids = requested_models
        .iter()
        .filter_map(|model| available.get(model))
        .map(|model| model.route_id.clone())
        .collect::<HashSet<_>>();
    for model in suite.tasks.iter().flat_map(|task| {
        task.phases.iter().filter_map(|phase| match &phase.model {
            PhaseModel::Named(model) => Some(model),
            _ => None,
        })
    }) {
        if let Some(model) = available.get(model) {
            selected_route_ids.insert(model.route_id.clone());
        }
    }
    for route in configured_routes
        .iter()
        .filter(|route| selected_route_ids.contains(&route.id))
    {
        if let Some(secret) = crate::credentials::load(&state.data_root(), &route.id)? {
            crate::credentials::save(eval_data.path(), &route.id, &secret)?;
        }
    }
    let includes_grok = configured_routes.iter().any(|route| {
        selected_route_ids.contains(&route.id) && route.provider_kind == ProviderKind::GrokCli
    });
    if includes_grok {
        stage_selected_grok_account(
            &state.grok_accounts(),
            &state.data_root().join("grok_accounts"),
            &eval_data.path().join("grok_accounts"),
            options.grok_account.as_deref(),
        )?;
    }
    let includes_official = credential_models.iter().any(|catalog_id| {
        available.get(catalog_id).is_some_and(|model| {
            configured_routes.iter().any(|route| {
                route.id == model.route_id && route.provider_kind == ProviderKind::Official
            })
        })
    });
    let eval_state = if options.control_model.is_some() || includes_official {
        AppState::with_eval_routes_and_oauth(
            eval_data.path().to_path_buf(),
            configured_routes.clone(),
            eval_oauth,
        )
    } else {
        AppState::with_eval_routes(eval_data.path().to_path_buf(), configured_routes.clone())
    };
    if let Some(grok_sel) = options.grok_account.as_deref() {
        let grok_manager = eval_state.grok_accounts();
        let account_id = grok_manager.find_account_id(grok_sel).ok_or_else(|| {
            AppError::Message(format!(
                "Grok account '{grok_sel}' not found in managed accounts"
            ))
        })?;
        grok_manager.set_default(&account_id)?;
    }
    // The switchover suite has exactly two arms. The embedded arm uses the
    // production route unchanged. The native-oracle arm disables only the
    // Vellum compact endpoint in this isolated eval state so Codex Core's
    // own local compactor can run.
    eval_state.set_eval_canonical_engine(None);
    eval_state.set_eval_compaction_engine(CompactionEngine::Canonical);
    // Cross-provider evals qualify the guard, not merely its diagnostics.
    // Keep production defaults untouched while making the exercised policy
    // explicit and resume-stable in run.json.
    eval_state.set_eval_recovery_enabled(options.recovery_mode == "recover");
    if options.compaction_engine == "codex-local-native-oracle" {
        for route in configured_routes
            .iter()
            .filter(|route| route.provider_kind != ProviderKind::Official)
        {
            eval_state.set_eval_route_compaction_policy(
                &route.id,
                Some(SessionCompactionPolicy {
                    strategy: CompactionStrategy::Disabled,
                    reasoning_effort: None,
                    threshold_percent: Some(85),
                    output_reserve_tokens: Some(4_096),
                    tool_reserve_tokens: Some(8_192),
                    allow_emergency_truncation: false,
                }),
            );
        }
    }
    let initial_official_turns = if options.resume_run.is_some() {
        official_turns_in_run(&run_root)?
    } else {
        0
    };
    let eval_history = eval_state.history_store();
    let eval_usage = eval_state.usage_store();
    let gateway = EvalGateway::start_with_turn_budget(
        eval_state,
        options.max_official_turns,
        initial_official_turns,
    )
    .await?;
    let isolation = match EvalIsolation::start(
        &options.executor,
        &run_id,
        gateway.port(),
        &options.codex_version,
    )
    .await
    {
        Ok(isolation) => isolation,
        Err(error) => {
            gateway.shutdown().await;
            return Err(error);
        }
    };
    let started = Instant::now();
    let mut scheduled = 0_usize;
    let selected_tasks = suite
        .tasks
        .iter()
        .filter(|task| task_matches_options(task, &options))
        .take(options.max_tasks.unwrap_or(usize::MAX))
        .collect::<Vec<_>>();
    if selected_tasks.is_empty() {
        isolation.shutdown().await;
        gateway.shutdown().await;
        return Err(AppError::Message(
            "no eval tasks matched the requested filters".into(),
        ));
    }
    let control_task_ids = sampled_control_tasks(&selected_tasks);
    let mut case_order = Vec::new();
    if options.schedule == "round-robin" {
        for repetition in 1..=repeat {
            for task_index in 0..selected_tasks.len() {
                for model_index in 0..requested_models.len() {
                    case_order.push((model_index, repetition, task_index));
                }
            }
        }
    } else {
        for model_index in 0..requested_models.len() {
            for repetition in 1..=repeat {
                for task_index in 0..selected_tasks.len() {
                    case_order.push((model_index, repetition, task_index));
                }
            }
        }
    }
    let mut quota_stopped = false;
    let mut protocol_failed = false;
    let execution_future = async {
        'cases: for (model_index, repetition, task_index) in case_order {
            let model = &requested_models[model_index];
            let task = selected_tasks[task_index];
            if options.control_model.as_deref() == Some(model.as_str())
                && !control_task_ids.contains(&task.id)
            {
                continue;
            }
            let targets = if options.control_model.as_deref() == Some(model.as_str()) {
                vec![model.as_str()]
            } else {
                switch_targets(task, &requested_models, model)
            };
            for next_model in targets {
                if options
                    .max_wall_seconds
                    .map(|limit| started.elapsed() >= Duration::from_secs(limit))
                    .unwrap_or(false)
                {
                    break 'cases;
                }
                if options
                    .max_total_tokens
                    .map(|limit| total_tokens_used >= limit)
                    .unwrap_or(false)
                {
                    break 'cases;
                }
                if options
                    .max_official_turns
                    .map(|limit| gateway.official_turns_spent() >= limit)
                    .unwrap_or(false)
                {
                    println!(
                        "eval max official turns reached ({} turns); preserving completed cases",
                        gateway.official_turns_spent()
                    );
                    quota_stopped = true;
                    break 'cases;
                }
                let case_id = eval_case_id(&task.id, model, next_model, repetition);
                if completed.contains(&case_id) {
                    continue;
                }
                if options.resume_run.is_some() {
                    if let Some(attempt) = prior_attempts.get(&case_id).copied() {
                        archive_case_outputs_for_resume(&run_root, &case_id, attempt)?;
                        println!(
                            "  resuming invalid case after {attempt} recorded attempt(s); prior artifacts archived"
                        );
                    }
                }
                scheduled += 1;
                println!(
                    "[{scheduled}] {} / {}{} / r{}",
                    task.id,
                    model,
                    if next_model == model {
                        String::new()
                    } else {
                        format!(" -> {next_model}")
                    },
                    repetition
                );
                let swe_turn_budget = matches!(task.category, TaskCategory::SweBench)
                    .then(|| SweTurnBudget::new(SWE_MAX_TURNS_PER_TASK));
                let swe_token_budget = {
                    let env_override = std::env::var("VELLUM_EVAL_MAX_INPUT_TOKENS")
                        .ok()
                        .and_then(|v| v.parse::<u64>().ok());
                    let limit = env_override.unwrap_or(task.limits.max_input_tokens);
                    (limit > 0).then(|| EvalTokenBudget::new(limit))
                };
                let mut result = run_case(
                    &run_root,
                    &run_id,
                    &prepared_root,
                    task,
                    model,
                    next_model,
                    repetition,
                    &available,
                    &provider_names,
                    &official_models,
                    &harness_profiles,
                    &base_catalog,
                    options.natural_context,
                    &gateway,
                    &isolation,
                    &eval_history,
                    &eval_usage,
                    &options.compaction_engine,
                    &options.catalog_mode,
                    swe_turn_budget.clone(),
                    swe_token_budget.clone(),
                    enhanced_identity.as_ref(),
                )
                .await
                .unwrap_or_else(|error| TaskResult {
                    run_id: run_id.clone(),
                    case_id: case_id.clone(),
                    task_id: task.id.clone(),
                    category: category_name(&task.category).into(),
                    model: model.clone(),
                    provider: provider_names
                        .get(model)
                        .cloned()
                        .unwrap_or_else(|| "unknown".into()),
                    context_window: if options.natural_context {
                        available.get(model).and_then(|model| model.context_window)
                    } else {
                        task.forced_context_window
                            .or_else(|| available.get(model).and_then(|model| model.context_window))
                    },
                    compaction_mode: matches!(task.category, TaskCategory::LongContext).then(
                        || {
                            if task.forced_context_window.is_some() && !options.natural_context {
                                "forced".into()
                            } else {
                                "natural".into()
                            }
                        },
                    ),
                    compact_request_count: 0,
                    canonical_hash: None,
                    canonical_schema_version: None,
                    journal_schema_version: None,
                    checkpoint_schema_version: None,
                    canonical_strategy: None,
                    continuity_kind: None,
                    requested_canonical_engine: Some(options.compaction_engine.clone()),
                    observed_canonical_engine: None,
                    requested_compaction_engine: Some(options.compaction_engine.clone()),
                    observed_compaction_engine: None,
                    compaction_engine_matched: None,
                    codex_compaction_events: 0,
                    remote_compact_requests: 0,
                    vellum_canonical_journal_count: 0,
                    catalog_hash: None,
                    eval_provider_display_name: Some(
                        eval_codex_provider_display_name(&options.compaction_engine).into(),
                    ),
                    pre_compaction_model_visible_tokens: None,
                    post_compaction_model_visible_tokens: None,
                    generation: None,
                    source_tokens: None,
                    checkpoint_tokens: None,
                    semantic_claim_count: None,
                    grounded_claim_count: None,
                    rejected_claim_count: None,
                    grounding_rate: None,
                    extraction_attempts: None,
                    fallback_used: None,
                    repeated_exchanges_collapsed: None,
                    canonical_kind: None,
                    injected_faults: task
                        .faults
                        .iter()
                        .map(|fault| fault_kind_name(&fault.kind).into())
                        .collect(),
                    http_faults: Vec::new(),
                    switched_models: (next_model != model)
                        .then(|| next_model.to_string())
                        .into_iter()
                        .collect(),
                    transition: transition_label(model, next_model, &task.transition_mode),
                    repetition,
                    passed: false,
                    task_correctness_passed: false,
                    performance_qualified: false,
                    protocol_qualified: false,
                    agent_completed: false,
                    timed_out: false,
                    limit_exceeded: false,
                    max_input_tokens: Some(
                        swe_token_budget
                            .as_ref()
                            .map(|budget| budget.gross_input_limit)
                            .unwrap_or(task.limits.max_input_tokens),
                    ),
                    max_output_tokens: Some(task.limits.max_output_tokens),
                    duration_ms: 0,
                    first_byte_ms: None,
                    compaction_recovered: matches!(task.category, TaskCategory::LongContext)
                        .then_some(false),
                    model_switch_recovered: matches!(task.category, TaskCategory::ModelSwitch)
                        .then_some(false),
                    acceptance_passed: 0,
                    acceptance_total: task.verification.len() as u64,
                    verification_exit_code: None,
                    verification_output: String::new(),
                    changed_files: Vec::new(),
                    added_lines: 0,
                    removed_lines: 0,
                    metrics: EventMetrics::default(),
                    protocol_violations: vec!["runner_case_failed".into()],
                    harness_expectation_misses: Vec::new(),
                    source_model_visible_tokens: None,
                    replacement_model_visible_tokens: None,
                    replacement_durable_tokens: None,
                    model_visible_compression_ratio: None,
                    source_hash: None,
                    fallback_reason: None,
                    failure_origin: Some(FailureOrigin::EvaluatorInfrastructure),
                    phase_observations: Vec::new(),
                    phase_execution_observations: Vec::new(),
                    compaction_attempts: Vec::new(),
                    semantic_validation_diagnostics: Vec::new(),
                    task_stall_diagnostics: Vec::new(),
                    task_stall_recoveries: Vec::new(),
                    task_stall_terminals: Vec::new(),
                    task_efficiency_diagnostics: Vec::new(),
                    task_efficiency_recoveries: Vec::new(),
                    task_efficiency_usage_coverage: TaskEfficiencyUsageCoverage::default(),
                    failure_class: Some("evaluator_infrastructure".into()),
                    root_cause: Some("evaluator_infrastructure".into()),
                    supporting_evidence: vec![redact_text(&error.to_string())],
                    error: Some(redact_text(&error.to_string())),
                    layers: LayerResults::default(),
                });
                if result.failure_origin == Some(FailureOrigin::Provider)
                    && retryable_provider_failure(&result)
                    && options
                        .max_wall_seconds
                        .map(|limit| {
                            started.elapsed() + Duration::from_secs(task.timeout_seconds.min(30))
                                < Duration::from_secs(limit)
                        })
                        .unwrap_or(true)
                {
                    println!("  transient provider failure; retrying once");
                    if let Ok(mut retry) = run_case(
                        &run_root,
                        &run_id,
                        &prepared_root,
                        task,
                        model,
                        next_model,
                        repetition,
                        &available,
                        &provider_names,
                        &official_models,
                        &harness_profiles,
                        &base_catalog,
                        options.natural_context,
                        &gateway,
                        &isolation,
                        &eval_history,
                        &eval_usage,
                        &options.compaction_engine,
                        &options.catalog_mode,
                        swe_turn_budget.clone(),
                        swe_token_budget.clone(),
                        enhanced_identity.as_ref(),
                    )
                    .await
                    {
                        for phase in &mut retry.phase_execution_observations {
                            phase.attempt = 2;
                        }
                        let mut phase_attempts = result.phase_execution_observations.clone();
                        phase_attempts.extend(retry.phase_execution_observations);
                        retry.phase_execution_observations = phase_attempts;
                        if swe_turn_budget.is_some() {
                            retry.task_efficiency_usage_coverage.settled_request_count = retry
                                .task_efficiency_usage_coverage
                                .settled_request_count
                                .max(result.task_efficiency_usage_coverage.settled_request_count);
                            retry
                                .task_efficiency_usage_coverage
                                .zero_usage_request_count = retry
                                .task_efficiency_usage_coverage
                                .zero_usage_request_count
                                .max(
                                    result
                                        .task_efficiency_usage_coverage
                                        .zero_usage_request_count,
                                );
                        } else {
                            retry.task_efficiency_usage_coverage.settled_request_count = retry
                                .task_efficiency_usage_coverage
                                .settled_request_count
                                .saturating_add(
                                    result.task_efficiency_usage_coverage.settled_request_count,
                                );
                            retry
                                .task_efficiency_usage_coverage
                                .zero_usage_request_count = retry
                                .task_efficiency_usage_coverage
                                .zero_usage_request_count
                                .saturating_add(
                                    result
                                        .task_efficiency_usage_coverage
                                        .zero_usage_request_count,
                                );
                        }
                        retry
                            .task_efficiency_usage_coverage
                            .token_accounting_available =
                            retry.task_efficiency_usage_coverage.settled_request_count > 0
                                && retry
                                    .task_efficiency_usage_coverage
                                    .zero_usage_request_count
                                    == 0;
                        retry.task_efficiency_usage_coverage.unavailable_reason = (!retry
                            .task_efficiency_usage_coverage
                            .token_accounting_available)
                            .then(|| "provider_usage_missing_or_zero".into());
                        merge_attempt_metrics(
                            &mut retry.metrics,
                            &result.metrics,
                            swe_turn_budget.is_some(),
                        );
                        retry.metrics.retries = retry.metrics.retries.saturating_add(1);
                        retry.duration_ms = retry.duration_ms.saturating_add(result.duration_ms);
                        result = retry;
                    }
                }
                if result.failure_class.as_deref() == Some("provider_quota")
                    || result
                        .protocol_violations
                        .iter()
                        .any(|v| v == "luna_campaign_quota" || v == "single_run_official_cap")
                {
                    println!("  provider quota or turn budget reached");
                    quota_stopped = true;
                }
                append_jsonl(&run_root.join("results.jsonl"), &result)?;
                total_tokens_used = total_tokens_used
                    .saturating_add(result.metrics.input_tokens + result.metrics.output_tokens);
                println!(
                    "  {} ({} ms, {} input / {} output tokens)",
                    if result.passed { "PASS" } else { "FAIL" },
                    result.duration_ms,
                    result.metrics.input_tokens,
                    result.metrics.output_tokens
                );
                let current_protocol_failed =
                    result.failure_class.as_deref().is_some_and(|class| {
                        matches!(
                            class,
                            "harness_trigger_config"
                                | "codex_harness_incompatibility"
                                | "vellum_adapter_protocol"
                                | "evaluator_observation"
                                | "wrong_engine"
                                | "mixed_engine"
                        )
                    });
                if current_protocol_failed {
                    protocol_failed = true;
                }
                if quota_stopped {
                    break 'cases;
                }
                if options.fail_fast.as_deref() == Some("protocol") && current_protocol_failed {
                    println!("  fail-fast: protocol layer failed");
                    break 'cases;
                }
            }
        }
        Ok::<(), AppError>(())
    };
    let mut aborted = false;
    let execution = tokio::select! {
        result = execution_future => result,
        _ = tokio::time::sleep(Duration::from_secs(options.max_wall_seconds.unwrap_or(u64::MAX))),
            if options.max_wall_seconds.is_some() => {
                println!("eval wall-time budget reached; preserving completed cases");
                aborted = true;
                Ok(())
            },
        signal = tokio::signal::ctrl_c() => match signal {
            Ok(()) => {
                aborted = true;
                Err(AppError::Message(format!(
                    "eval run interrupted; resume with `vellum-eval matrix --suite {} --models {} --resume {run_id}`",
                    suite.name,
                    options.models.join(",")
                )))
            },
            Err(error) => Err(AppError::Message(format!(
                "cannot monitor eval interrupt signal: {error}"
            ))),
        },
    };
    isolation.shutdown().await;
    gateway.shutdown().await;
    if let Err(error) = execution {
        let final_status = if aborted { "aborted" } else { "failed" };
        let _ = lifecycle_guard.finalize(final_status);
        if run_root.join("results.jsonl").is_file() {
            let baseline = options
                .baseline_run
                .as_deref()
                .map(|id| paths.output_root.join(id));
            let _ = super::report::write_html_report_with_baseline(&run_root, baseline.as_deref());
        }
        return Err(error);
    }
    let expected_cases = requested_models
        .iter()
        .map(|model| {
            selected_tasks
                .iter()
                .filter(|task| {
                    options.control_model.as_deref() != Some(model.as_str())
                        || control_task_ids.contains(&task.id)
                })
                .map(|task| {
                    if options.control_model.as_deref() == Some(model.as_str()) {
                        1
                    } else {
                        switch_targets(task, &requested_models, model).len()
                    }
                })
                .sum::<usize>()
        })
        .sum::<usize>()
        .saturating_mul(repeat as usize);
    let completed_cases = completed_case_ids(&run_root.join("results.jsonl"))?.len();
    let mut final_status = if aborted {
        "aborted"
    } else if quota_stopped {
        "quota_stopped"
    } else if completed_cases >= expected_cases {
        "completed"
    } else {
        "failed"
    };
    let baseline = options
        .baseline_run
        .as_deref()
        .map(|id| paths.output_root.join(id));
    let comparison = baseline
        .as_deref()
        .map(|baseline| super::report::compare_run_roots(&run_root, baseline))
        .transpose()?;
    if comparison
        .as_ref()
        .is_some_and(|comparison| !comparison.pass_rate_gate || !comparison.stable_regression_gate)
    {
        final_status = "regressed";
    }
    lifecycle_guard.finalize(final_status)?;
    if let Some(comparison) = &comparison {
        write_json(&run_root.join("baseline-comparison.json"), comparison)?;
    }
    super::report::write_html_report_with_baseline(&run_root, baseline.as_deref())?;
    if final_status == "regressed" {
        return Err(AppError::Message(format!(
            "eval regression gate failed; inspect {}",
            run_root.join("baseline-comparison.json").display()
        )));
    }
    Ok(run_root)
}

fn task_matches_options(task: &EvalTask, options: &RunOptions) -> bool {
    options
        .task_filter
        .as_deref()
        .map(|value| task.id == value)
        .unwrap_or(true)
        && options
            .category_filter
            .as_deref()
            .map(|value| category_name(&task.category).eq_ignore_ascii_case(value))
            .unwrap_or(true)
        && options
            .tag_filter
            .as_deref()
            .map(|value| task.tags.iter().any(|tag| tag.eq_ignore_ascii_case(value)))
            .unwrap_or(true)
        && transition_matches(task, options.transition.as_deref())
}

fn transition_matches(task: &EvalTask, filter: Option<&str>) -> bool {
    match filter.map(|value| value.to_ascii_lowercase()) {
        None => true,
        Some(value) if value == "directed" => matches!(
            task.transition_mode,
            TransitionMode::SameModel | TransitionMode::OrderedPair
        ),
        Some(value) if value == "all-round-trips" || value == "all_round_trips" => {
            matches!(task.transition_mode, TransitionMode::AllRoundTrips)
        }
        Some(value) if value == "cyclic" || value == "round-trip" => {
            matches!(task.transition_mode, TransitionMode::RoundTrip)
        }
        Some(_) => false,
    }
}

fn sampled_control_tasks(tasks: &[&EvalTask]) -> HashSet<String> {
    let desired = [
        "short-fix",
        "multi-file",
        "tool-recovery",
        "long-context",
        "swe-bench",
    ];
    let mut selected = HashSet::new();
    for category in desired {
        if let Some(task) = tasks
            .iter()
            .find(|task| category_name(&task.category) == category)
        {
            selected.insert(task.id.clone());
        }
    }
    if let Some(second_short) = tasks
        .iter()
        .filter(|task| category_name(&task.category) == "short-fix")
        .nth(1)
    {
        selected.insert(second_short.id.clone());
    }
    if selected.is_empty() {
        if let Some(task) = tasks.first() {
            selected.insert(task.id.clone());
        }
    }
    selected
}

#[derive(Debug, Clone)]
struct EnhancedEvalIdentity {
    runtime_digest: String,
    enhanced_codex_commit: String,
    artifact_sha256: String,
    ablation_profile: String,
    feature_flags: vellum_enhanced_codex::EnhancedRuntimeFeatures,
    executable: PathBuf,
}

fn require_verified_enhanced_runtime(
    profile: &str,
    paths: &EvalPaths,
) -> AppResult<EnhancedEvalIdentity> {
    let parsed = vellum_enhanced_codex::AblationProfile::parse(profile)
        .ok_or_else(|| AppError::Message(format!("invalid ablation profile {profile}")))?;
    let lock_path = paths.project_root.join("enhanced-runtime.lock.json");
    let lock = vellum_enhanced_codex::EnhancedRuntimeLockFile::parse(
        &std::fs::read(&lock_path).map_err(io_error("read enhanced-runtime.lock.json"))?,
    )
    .map_err(|error| AppError::Message(error.to_string()))?;
    let target = env!("VELLUM_BUILD_TARGET");
    if !lock.identity_complete_for_target(target) {
        return Err(AppError::Message(
            format!("ablation profile E0–E5 requires a verified Enhanced Codex runtime artifact for {target} in enhanced-runtime.lock.json before the first task"),
        ));
    }
    let executable = std::env::var_os("VELLUM_ENHANCED_CODEX")
        .map(PathBuf::from)
        .ok_or_else(|| {
            AppError::Message(
                "ablation profile E0–E5 requires VELLUM_ENHANCED_CODEX to point at the verified Enhanced Codex binary".into(),
            )
        })?;
    if !executable.is_file() {
        return Err(AppError::Message(format!(
            "Enhanced Codex binary is missing: {}",
            executable.display()
        )));
    }
    let hashed = sha256_file(&executable)?;
    let expected = lock
        .artifact_for_target(target)
        .unwrap_or_default()
        .trim_start_matches("sha256:");
    if hashed != expected {
        return Err(AppError::Message(format!(
            "Enhanced Codex artifact SHA mismatch: expected sha256:{expected}, got sha256:{hashed}"
        )));
    }
    let digest = lock
        .runtime_digest(crate::enhanced_runtime::FEATURE_DEFAULTS_MVP, target)
        .map_err(|error| AppError::Message(error.to_string()))?;
    Ok(EnhancedEvalIdentity {
        runtime_digest: digest,
        enhanced_codex_commit: lock.enhanced_codex_commit.clone().unwrap_or_default(),
        artifact_sha256: lock
            .artifact_for_target(target)
            .unwrap_or_default()
            .to_string(),
        ablation_profile: parsed.as_str().to_string(),
        feature_flags: parsed.features(),
        executable,
    })
}

fn write_eval_enhanced_runtime_config(
    codex_home: &Path,
    identity: Option<&EnhancedEvalIdentity>,
) -> AppResult<()> {
    let Some(identity) = identity else {
        return Ok(());
    };
    let payload = serde_json::json!({
        "ablationProfile": identity.ablation_profile,
        "runtimeDigest": identity.runtime_digest,
        "enhancedCodexCommit": identity.enhanced_codex_commit,
        "artifactSha256": identity.artifact_sha256,
        "featureFlags": identity.feature_flags,
        "executable": identity.executable,
        "identityComplete": true,
    });
    std::fs::write(
        codex_home.join("enhanced-runtime.json"),
        serde_json::to_vec_pretty(&payload)
            .map_err(|error| AppError::Message(format!("encode enhanced-runtime.json: {error}")))?,
    )
    .map_err(io_error("write enhanced-runtime.json"))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_case(
    run_root: &Path,
    run_id: &str,
    prepared_root: &Path,
    task: &EvalTask,
    model: &str,
    next_model: &str,
    repetition: u32,
    available: &HashMap<String, ModelRoute>,
    provider_names: &HashMap<String, String>,
    official_models: &HashSet<String>,
    harness_profiles: &HashMap<String, String>,
    base_catalog: &Value,
    natural_context: bool,
    gateway: &EvalGateway,
    isolation: &EvalIsolation,
    history: &crate::history::HistoryStore,
    usage: &crate::usage::UsageStore,
    compaction_engine: &str,
    catalog_mode: &str,
    swe_turn_budget: Option<SweTurnBudget>,
    swe_token_budget: Option<SweTokenBudget>,
    enhanced_identity: Option<&EnhancedEvalIdentity>,
) -> AppResult<TaskResult> {
    // Capture before `swe_token_budget` is moved into `register_task_with_budgets`
    // below — this is the limit the gateway's admission gate actually enforces
    // for this case (already resolved against `VELLUM_EVAL_MAX_INPUT_TOKENS`
    // by the caller), which must agree with the final verdict computed later.
    let effective_max_input_tokens = swe_token_budget
        .as_ref()
        .map(|budget| budget.gross_input_limit)
        .unwrap_or(task.limits.max_input_tokens);
    let diagnostic_cursor = usage.diagnostic_cursor()?;
    let journal_before = history
        .latest_compaction()?
        .map(|journal| journal.compaction_id);
    let case_id = eval_case_id(&task.id, model, next_model, repetition);
    // Keep the detailed case ID for reports and trace filenames, but do not
    // embed provider/model slugs in the mutable workspace path. Windows Git
    // creates hook filenames below this directory and otherwise exceeds
    // MAX_PATH for long local-model IDs before Docker even starts.
    let case_root = run_root.join("cases").join(case_work_dir_name(&case_id));
    if case_root.exists() {
        std::fs::remove_dir_all(&case_root).map_err(io_error("replace eval case"))?;
    }
    let workspace = case_root.join("workspace");
    let hidden = case_root.join("hidden");
    let codex_home = case_root.join("codex-home");
    if matches!(task.source, TaskSource::SweBench { .. }) {
        copy_tree_preserving_git(&prepared_root.join(&task.id).join("workspace"), &workspace)?;
    } else {
        copy_tree(&prepared_root.join(&task.id).join("workspace"), &workspace)?;
    }
    copy_tree(&prepared_root.join(&task.id).join("hidden"), &hidden)?;
    std::fs::create_dir_all(&codex_home).map_err(io_error("create isolated CODEX_HOME"))?;
    write_eval_enhanced_runtime_config(&codex_home, enhanced_identity)?;
    if matches!(isolation, EvalIsolation::Windows(_)) {
        write_windows_eval_trust(&codex_home, &workspace)?;
    }
    let initial_prompt = if matches!(task.source, TaskSource::SweBench { .. }) {
        swe_execution_budget_prompt(&task.prompt)
    } else if matches!(task.category, TaskCategory::LongContext) {
        write_long_context_fixture(&workspace)?;
        long_context_execution_prompt(&task.prompt)
    } else {
        task.prompt.clone()
    };
    initialize_git(&workspace).await?;

    let mut allowed = vec![model.to_string()];
    for phase in &task.phases {
        match &phase.model {
            PhaseModel::Next => allowed.push(next_model.to_string()),
            PhaseModel::Named(name) => allowed.push(name.clone()),
            PhaseModel::Current => {}
        }
    }
    allowed.sort();
    allowed.dedup();
    for id in &allowed {
        if !available.contains_key(id) {
            return Err(AppError::Message(format!(
                "task {} references unavailable model {id}",
                task.id
            )));
        }
    }
    let catalog_path = case_root.join("model-catalog.json");
    write_eval_catalog(
        &catalog_path,
        &allowed,
        if natural_context {
            None
        } else {
            task.forced_context_window
        },
        if natural_context {
            None
        } else {
            task.forced_auto_compact_token_limit
        },
        base_catalog,
        catalog_mode,
    )?;
    let trace_path = run_root
        .join("traces")
        .join(format!("{case_id}.gateway.jsonl"));
    if trace_path.is_file() && swe_turn_budget.is_none() {
        std::fs::remove_file(&trace_path).map_err(io_error("replace eval gateway trace"))?;
    }
    let trace_metadata = allowed
        .iter()
        .filter_map(|id| {
            let route = available.get(id)?;
            Some((
                id.to_ascii_lowercase(),
                TraceModelMetadata {
                    provider: provider_names
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| "unknown".into()),
                    route_id: route.route_id.clone(),
                    upstream_model: route.upstream_model.clone(),
                    is_official: official_models.contains(id),
                    harness_profile: harness_profiles.get(id).cloned().unwrap_or_default(),
                },
            ))
        })
        .collect::<HashMap<_, _>>();
    let credential = gateway
        .register_task_with_budgets(
            case_id.clone(),
            allowed.clone(),
            &task.faults,
            trace_path.clone(),
            trace_metadata,
            Some(isolation.shell_contract().to_string()),
            swe_turn_budget,
            swe_token_budget,
        )
        .await?;

    for command in &task.setup {
        let output = isolation
            .run_shell(
                &workspace,
                &codex_home,
                command,
                Duration::from_secs(task.timeout_seconds.min(300)),
                &task.limits,
            )
            .await?;
        ensure_success(output, &format!("setup {}", task.id))?;
    }

    let started = Instant::now();
    let mut metrics = EventMetrics::default();
    let mut phase_observations = Vec::new();
    let mut phase_execution_observations = Vec::new();
    let mut switched_models = Vec::new();
    let mut executed_models = vec![model.to_string()];
    let forced_context_window = (!natural_context)
        .then_some(task.forced_context_window)
        .flatten();
    let forced_auto_compact_token_limit = (!natural_context)
        .then_some(task.forced_auto_compact_token_limit)
        .flatten();
    let (effective_context_window_p0, auto_compact_token_limit_p0) =
        eval_catalog_context_settings(&catalog_path, model)?;

    let (compact_before_p0, max_index_before_p0) = read_gateway_trace(&trace_path)
        .map(|s| (s.compaction_requests, s.max_request_index))
        .unwrap_or((0, 0));
    let initial_started = Instant::now();
    let initial = isolation
        .run_codex(
            &workspace,
            &codex_home,
            &catalog_path,
            model,
            None,
            &initial_prompt,
            &credential.token,
            Duration::from_secs(task.timeout_seconds),
            &task.limits,
            forced_context_window,
            forced_auto_compact_token_limit,
            task.reasoning_effort.as_deref(),
            compaction_engine,
            enhanced_identity.map(|identity| identity.executable.as_path()),
        )
        .await?;
    let first_byte_ms = initial.first_byte_ms;
    let initial_metrics = parse_codex_jsonl(&initial.stdout);
    phase_execution_observations.push(PhaseExecutionObservation {
        attempt: 1,
        phase: 0,
        model: model.to_string(),
        resumed: false,
        exit_code: initial.code,
        timed_out: initial.timed_out,
        duration_ms: initial_started.elapsed().as_millis() as u64,
        first_byte_ms: initial.first_byte_ms,
        terminal_observed: !initial.timed_out && initial.code.is_some(),
        compaction_delta: initial_metrics.compactions,
        input_token_delta: initial_metrics.input_tokens,
        output_token_delta: initial_metrics.output_tokens,
    });
    let mut session_id = initial_metrics.session_id.clone();
    merge_metrics(&mut metrics, initial_metrics);
    write_phase_output(run_root, &case_id, 0, &initial, &credential.token)?;
    let mut agent_completed = initial.code == Some(0) && !initial.timed_out;
    let mut timed_out = initial.timed_out;

    let summary_phase0 = read_gateway_trace(&trace_path).unwrap_or_default();
    let compact_after_phase0 = summary_phase0.compaction_requests;
    let est_tokens_phase0 = summary_phase0
        .request_model_visible_tokens_by_index
        .iter()
        .filter(|(&idx, _)| idx > max_index_before_p0)
        .max_by_key(|(&idx, _)| idx)
        .map(|(_, &tokens)| tokens)
        .unwrap_or(0);

    let trigger_decision_phase0 = classify_phase_trigger_decision_with_codex(
        compact_before_p0,
        compact_after_phase0,
        0,
        metrics.compactions,
        est_tokens_phase0,
        auto_compact_token_limit_p0,
    );
    phase_observations.push(PhaseContextObservation {
        phase: 0,
        estimated_context_tokens: est_tokens_phase0,
        effective_context_window: effective_context_window_p0,
        auto_compact_token_limit: auto_compact_token_limit_p0,
        compact_requests_before: compact_before_p0,
        compact_requests_after: compact_after_phase0,
        trigger_decision: trigger_decision_phase0,
    });

    if !agent_completed && !timed_out && !task.faults.is_empty() {
        for attempt in 1..=2 {
            let Some(thread) = session_id.as_deref() else {
                break;
            };
            let output = isolation
                .run_codex(
                    &workspace,
                    &codex_home,
                    &catalog_path,
                    model,
                    Some(thread),
                    "The previous turn was interrupted by a transient provider or transport error. Continue the same task from the existing workspace state.",
                    &credential.token,
                    Duration::from_secs(task.timeout_seconds),
                    &task.limits,
                    forced_context_window,
                    forced_auto_compact_token_limit,
                    task.reasoning_effort.as_deref(),
                    compaction_engine,
                    enhanced_identity.map(|identity| identity.executable.as_path()),
                )
                .await?;
            let parsed = parse_codex_jsonl(&output.stdout);
            if session_id.is_some()
                && parsed.session_id.is_some()
                && parsed.session_id != session_id
            {
                metrics.session_reinitializations += 1;
            }
            if parsed.session_id.is_some() {
                session_id = parsed.session_id.clone();
            }
            merge_metrics(&mut metrics, parsed);
            metrics.retries = metrics.retries.saturating_add(1);
            write_phase_output(
                run_root,
                &case_id,
                10_000 + attempt,
                &output,
                &credential.token,
            )?;
            agent_completed = output.code == Some(0) && !output.timed_out;
            timed_out |= output.timed_out;
            if agent_completed || timed_out {
                break;
            }
        }
    }

    let mut resumed_phases_observed = 0_usize;
    let mut resumed_phases_completed = 0_usize;
    for (index, phase) in task.phases.iter().enumerate() {
        let Some(thread) = session_id.as_deref() else {
            agent_completed = false;
            metrics
                .errors
                .push("Codex JSONL did not include a resumable session id".into());
            break;
        };
        let phase_model = match &phase.model {
            PhaseModel::Current => model,
            PhaseModel::Next => next_model,
            PhaseModel::Named(name) => name,
        };
        if executed_models
            .last()
            .is_none_or(|previous| previous != phase_model)
        {
            switched_models.push(phase_model.to_string());
        }
        executed_models.push(phase_model.to_string());
        let (effective_context_window_pn, auto_compact_token_limit_pn) =
            eval_catalog_context_settings(&catalog_path, phase_model)?;

        let (compact_before_pn, max_index_before_pn) = read_gateway_trace(&trace_path)
            .map(|s| (s.compaction_requests, s.max_request_index))
            .unwrap_or((0, 0));
        let codex_before_pn = metrics.compactions;
        let input_before_pn = metrics.input_tokens;
        let output_before_pn = metrics.output_tokens;
        let phase_started = Instant::now();
        let output = isolation
            .run_codex(
                &workspace,
                &codex_home,
                &catalog_path,
                phase_model,
                Some(thread),
                &phase.prompt,
                &credential.token,
                Duration::from_secs(task.timeout_seconds),
                &task.limits,
                forced_context_window,
                forced_auto_compact_token_limit,
                phase
                    .reasoning_effort
                    .as_deref()
                    .or(task.reasoning_effort.as_deref()),
                compaction_engine,
                enhanced_identity.map(|identity| identity.executable.as_path()),
            )
            .await?;
        let parsed = parse_codex_jsonl(&output.stdout);
        let phase_resumed = parsed.session_id.as_deref() == Some(thread);
        phase_execution_observations.push(PhaseExecutionObservation {
            attempt: 1,
            phase: index + 1,
            model: phase_model.to_string(),
            resumed: phase_resumed,
            exit_code: output.code,
            timed_out: output.timed_out,
            duration_ms: phase_started.elapsed().as_millis() as u64,
            first_byte_ms: output.first_byte_ms,
            terminal_observed: !output.timed_out && output.code.is_some(),
            compaction_delta: parsed.compactions,
            input_token_delta: parsed.input_tokens.saturating_sub(input_before_pn),
            output_token_delta: parsed.output_tokens.saturating_sub(output_before_pn),
        });
        if phase_resumed {
            resumed_phases_observed += 1;
        }
        if session_id.is_some() && parsed.session_id.is_some() && parsed.session_id != session_id {
            metrics.session_reinitializations += 1;
        }
        if parsed.session_id.is_some() {
            session_id = parsed.session_id.clone();
        }
        merge_metrics(&mut metrics, parsed);
        write_phase_output(run_root, &case_id, index + 1, &output, &credential.token)?;
        let phase_completed = output.code == Some(0) && !output.timed_out;
        if phase_completed {
            resumed_phases_completed += 1;
        }
        let summary_after = read_gateway_trace(&trace_path).unwrap_or_default();
        let compact_after = summary_after.compaction_requests;
        let est_tokens = summary_after
            .request_model_visible_tokens_by_index
            .iter()
            .filter(|(&idx, _)| idx > max_index_before_pn)
            .max_by_key(|(&idx, _)| idx)
            .map(|(_, &tokens)| tokens)
            .unwrap_or(0);

        let trigger_decision = classify_phase_trigger_decision_with_codex(
            compact_before_pn,
            compact_after,
            codex_before_pn,
            metrics.compactions,
            est_tokens,
            auto_compact_token_limit_pn,
        );
        phase_observations.push(PhaseContextObservation {
            phase: index + 1,
            estimated_context_tokens: est_tokens,
            effective_context_window: effective_context_window_pn,
            auto_compact_token_limit: auto_compact_token_limit_pn,
            compact_requests_before: compact_before_pn,
            compact_requests_after: compact_after,
            trigger_decision,
        });
        // A later model is allowed to recover an incomplete or timed-out turn.
        // This is the behavior the continuity suites are intended to qualify;
        // stopping before the handoff only measures the first model's latency.
        agent_completed = phase_completed;
        timed_out |= output.timed_out;
    }
    let gateway_summary = read_gateway_trace(&trace_path)?;
    let task_efficiency_usage_coverage = TaskEfficiencyUsageCoverage {
        settled_request_count: gateway_summary.settled_request_count,
        zero_usage_request_count: gateway_summary.zero_usage_request_count,
        token_accounting_available: gateway_summary.settled_request_count > 0
            && gateway_summary.zero_usage_request_count == 0,
        unavailable_reason: if gateway_summary.settled_request_count == 0 {
            Some("no_settled_provider_request".into())
        } else if gateway_summary.zero_usage_request_count > 0 {
            Some("provider_usage_missing_or_zero".into())
        } else {
            None
        },
    };
    let (engine_neutral_pre_tokens, engine_neutral_post_tokens) =
        engine_neutral_compaction_window(&gateway_summary.request_model_visible_tokens_by_index);
    let catalog_hash = std::fs::read(&catalog_path)
        .ok()
        .map(|bytes| format!("{:x}", Sha256::digest(&bytes)));
    let codex_compaction_events = metrics.compactions;
    let remote_compact_requests = gateway_summary.compaction_requests;
    metrics.input_tokens = metrics.input_tokens.max(gateway_summary.input_tokens);
    metrics.output_tokens = metrics.output_tokens.max(gateway_summary.output_tokens);
    metrics.terminal_sse_missing = gateway_summary.missing_terminal_sse;
    metrics.disconnected_streams = gateway_summary.disconnected_streams;
    metrics.malformed_tool_calls = metrics
        .malformed_tool_calls
        .saturating_add(gateway_summary.malformed_tool_arguments);
    let valid_compaction_responses = gateway_summary.valid_compaction_responses;
    let journal_after = history.latest_compaction()?;
    let journal_changed = journal_after
        .as_ref()
        .is_some_and(|journal| journal_before.as_deref() != Some(journal.compaction_id.as_str()));
    let mut compaction_attempts = usage.local_compaction_attempts_after(diagnostic_cursor)?;
    let semantic_validation_diagnostics =
        usage.semantic_validation_diagnostics_after(diagnostic_cursor)?;
    let max_request_index = u32::try_from(gateway_summary.max_request_index).unwrap_or(u32::MAX);
    let request_hashes = (1..=max_request_index)
        .map(|index| credential.runtime_request_id_hash(index))
        .collect::<HashSet<_>>();
    compaction_attempts.retain(|attempt| {
        attempt
            .request_id_hash
            .as_ref()
            .is_some_and(|hash| request_hashes.contains(hash))
    });
    let mut task_stall_diagnostics = usage.task_stall_diagnostics_after(diagnostic_cursor)?;
    task_stall_diagnostics.retain(|diagnostic| {
        diagnostic
            .request_id_hash
            .as_ref()
            .is_some_and(|hash| request_hashes.contains(hash))
    });
    let mut task_stall_recoveries = usage.task_stall_recoveries_after(diagnostic_cursor)?;
    task_stall_recoveries.retain(|diagnostic| request_hashes.contains(&diagnostic.request_id_hash));
    let mut task_stall_terminals = usage.task_stall_terminals_after(diagnostic_cursor)?;
    task_stall_terminals.retain(|diagnostic| request_hashes.contains(&diagnostic.request_id_hash));
    task_stall_terminals.dedup_by(|left, right| {
        left.task_revision == right.task_revision
            && left.recovery_count == right.recovery_count
            && left.category == right.category
    });
    let mut task_efficiency_diagnostics =
        usage.task_efficiency_diagnostics_after(diagnostic_cursor)?;
    task_efficiency_diagnostics.retain(|diagnostic| {
        diagnostic
            .request_id_hash
            .as_ref()
            .is_some_and(|hash| request_hashes.contains(hash))
    });
    let mut task_efficiency_recoveries =
        usage.task_efficiency_recoveries_after(diagnostic_cursor)?;
    task_efficiency_recoveries
        .retain(|diagnostic| request_hashes.contains(&diagnostic.request_id_hash));
    let task_stall_watchdog_stop = !task_stall_terminals.is_empty();
    let canonical_journal_valid = journal_after.as_ref().is_some_and(|journal| {
        journal_changed
            && journal.schema_version >= 2
            && journal.canonical_available
            && !journal.canonical_items.is_empty()
            && journal.canonical_sha256.len() == 64
    });
    // A declared gateway fault is only recovered when a later request reaches
    // a successful terminal. Preserve every HTTP error as report evidence and
    // fail closed for wrong-position, wrong-kind, or unrecovered injections.
    let http_faults = classify_gateway_http_faults(&gateway_summary, &task.faults);
    let gateway_http_errors = http_faults
        .iter()
        .filter(|observation| !observation.expected_injection || !observation.recovered)
        .map(|observation| observation.message.clone())
        .collect::<Vec<_>>();
    let gateway_http_ok = gateway_http_errors.is_empty();
    metrics.errors.extend(gateway_http_errors.clone());
    redact_event_metrics(&mut metrics, &credential.token);

    let (changed_files, added_lines, removed_lines, patch) = git_diff(&workspace).await?;
    std::fs::write(
        run_root.join("patches").join(format!("{case_id}.patch")),
        redact_text_with_secrets(&patch, &[&credential.token]),
    )
    .map_err(io_error("write eval patch"))?;
    let verification_workspace = case_root.join("verification-workspace");
    copy_tree_preserving_git(&workspace, &verification_workspace)?;

    let mut acceptance_passed = 0_u64;
    let mut verification_exit_code = Some(0);
    let mut verification_timed_out = false;
    let mut verification_output = String::new();
    for (index, command) in task.verification.iter().enumerate() {
        let verification = isolation
            .run_verification(
                &verification_workspace,
                &hidden,
                std::slice::from_ref(command),
                Duration::from_secs(task.timeout_seconds.min(300)),
                &task.limits,
                task.grader_image.as_deref(),
            )
            .await?;
        if verification.code == Some(0) && !verification.timed_out {
            acceptance_passed += 1;
        } else if verification_exit_code == Some(0) {
            verification_exit_code = verification.code;
        }
        verification_timed_out |= verification.timed_out;
        verification_output.push_str(&format!(
            "=== acceptance {}: {} ===\n{}{}\n",
            index + 1,
            command,
            verification.stdout,
            verification.stderr
        ));
    }
    let verification_output = redact_text(&verification_output);
    std::fs::write(
        run_root.join("test-output").join(format!("{case_id}.txt")),
        &verification_output,
    )
    .map_err(io_error("write verification output"))?;
    let workspace_bytes = directory_size(&workspace)?;
    let swe_turn_cap_reached = metrics
        .errors
        .iter()
        .any(|error| error.contains("swe_turn_cap"));
    let swe_token_limit_reached = metrics
        .errors
        .iter()
        .any(|error| error.contains("swe_token_limit"));
    let limit_exceeded = swe_turn_cap_reached
        || swe_token_limit_reached
        || metrics.input_tokens > effective_max_input_tokens
        || metrics.output_tokens > task.limits.max_output_tokens
        || workspace_bytes > task.limits.disk_mb.saturating_mul(1024 * 1024);
    let any_timed_out = timed_out || verification_timed_out;
    let vellum_canonical_journal_count = compaction_attempts
        .iter()
        .filter(|attempt| attempt.replacement_hash.is_some())
        .count() as u64;
    let embedded_local_attempt = compaction_attempts.iter().any(|attempt| {
        attempt.engine_id == vellum_proxy_runtime::codex_local_v0_150::ENGINE_ID
            && attempt.replacement_hash.is_some()
    });
    let observed_engine = classify_observed_eval_compaction_engine(
        compaction_engine,
        embedded_local_attempt,
        remote_compact_requests,
        codex_compaction_events,
    );
    let engine_matched = compaction_engine_matched(compaction_engine, &observed_engine);
    let observed_compaction_count = if compaction_engine == "codex-local-native-oracle" {
        codex_compaction_events
    } else {
        remote_compact_requests.max(codex_compaction_events)
    };
    let compaction_recovered = matches!(task.category, TaskCategory::LongContext).then_some(
        agent_completed
            && if compaction_engine == "codex-local-native-oracle" {
                codex_compaction_events > 0
            } else {
                valid_compaction_responses > 0
            },
    );
    let model_switch_recovered = (matches!(task.category, TaskCategory::ModelSwitch)
        || task.expected_harness.require_model_switch)
        .then_some(agent_completed && !switched_models.is_empty());
    let mut protocol_violations = Vec::new();
    let mut harness_expectation_misses = Vec::new();
    if metrics.reasoning_leaks > 0 {
        protocol_violations.push("reasoning_leak".into());
    }
    if metrics.malformed_tool_calls > 0 {
        protocol_violations.push("malformed_tool_call".into());
    }
    if metrics.terminal_sse_missing > 0 && !any_timed_out {
        protocol_violations.push("missing_terminal_sse".into());
    }
    if metrics.session_reinitializations > 0 {
        protocol_violations.push("session_reinitialized".into());
    }
    if metrics.tool_calls < task.expected_harness.min_tool_calls {
        harness_expectation_misses.push(format!(
            "tool_activity={}/{}",
            metrics.tool_calls, task.expected_harness.min_tool_calls
        ));
    }
    let requires_enhanced_mechanism =
        enhanced_identity.is_some_and(|identity| identity.ablation_profile != "E0");
    if requires_enhanced_mechanism {
        for (name, expected) in &task.expected_harness.min_enhanced_events {
            let observed = metrics
                .enhanced_event_counts
                .get(name)
                .copied()
                .unwrap_or(0);
            if observed < *expected {
                harness_expectation_misses
                    .push(format!("enhanced_event:{name}={observed}/{expected}"));
            }
        }
    }
    let runtime_attribution = match enhanced_identity {
        None => true,
        Some(expected) => metrics.enhanced_runtime.as_ref().is_some_and(|observed| {
            observed.feature_profile.as_deref() == Some(expected.ablation_profile.as_str())
                && observed.runtime_digest.as_deref() == Some(expected.runtime_digest.as_str())
                && observed.enhanced_commit.as_deref()
                    == Some(expected.enhanced_codex_commit.as_str())
                && observed.qwen_tool_reliability
                    == Some(expected.feature_flags.qwen_tool_reliability)
                && observed.deepseek_context_recovery
                    == Some(expected.feature_flags.deepseek_context_recovery)
                && observed.qwen_bounded_continuation
                    == Some(expected.feature_flags.qwen_bounded_continuation)
                && observed.repetition_notice.unwrap_or(false)
                    == expected.feature_flags.repetition_notice
                && observed.intent_continuation.unwrap_or(false)
                    == expected.feature_flags.intent_continuation
        }),
    };
    if !runtime_attribution {
        protocol_violations.push("enhanced_runtime_identity_not_observed".into());
    }
    if observed_compaction_count < task.expected_harness.min_compactions {
        protocol_violations.push("missing_compaction".into());
    }
    if task.expected_harness.require_terminal_sse
        && gateway_summary.terminal_streams == 0
        && !any_timed_out
    {
        protocol_violations.push("terminal_sse_not_observed".into());
    }
    if task.expected_harness.require_session_resume
        && !all_resumes_observed(task.phases.len(), resumed_phases_observed)
    {
        protocol_violations.push("session_resume_not_observed".into());
    }
    if task.expected_harness.require_model_switch && switched_models.is_empty() {
        protocol_violations.push("model_switch_not_observed".into());
    }
    let protocol_error_text = metrics.errors.join(" ").to_ascii_lowercase();
    for (needle, violation) in [
        ("invalid_id_prefix", "invalid_id_prefix"),
        ("encrypted content", "foreign_encrypted_reasoning"),
        ("unmaterialized compaction", "unmaterialized_compaction"),
        ("duplicate function definition", "duplicate_tool_definition"),
        ("malformed tool", "malformed_tool_arguments"),
    ] {
        if protocol_error_text.contains(needle)
            && !protocol_violations.iter().any(|value| value == violation)
        {
            protocol_violations.push(violation.into());
        }
    }
    let mut continuity_preserved = true;
    for checkpoint in &task.continuity_checkpoints {
        let path = workspace.join(&checkpoint.path);
        let retained = std::fs::read_to_string(&path)
            .map(|content| content.contains(&checkpoint.contains))
            .unwrap_or(false);
        if !retained {
            continuity_preserved = false;
            protocol_violations.push(format!("continuity_checkpoint_missing:{}", checkpoint.path));
        }
    }
    let compaction_expected = task.expected_harness.min_compactions > 0;
    if compaction_expected && observed_compaction_count == 0 {
        protocol_violations.push("compaction_not_armed".into());
    }
    if observed_engine == "mixed" {
        protocol_violations.push("mixed_engine".into());
    } else if compaction_expected && !engine_matched && observed_engine != "none" {
        protocol_violations.push("wrong_engine".into());
    }
    let is_canonical_engine = false;
    // Fail-closed both directions: a V1 request that silently ran V2 (or
    // vice versa) must fail the case, not pass mislabeled as the requested
    // engine. A V2-requested case additionally requires a complete audit
    // record — a PASS with a V2 signature but no grounding/token telemetry
    // is not usable research data.
    let engine_signature_valid =
        canonical_engine_signature_valid(compaction_engine, journal_after.as_ref());

    if compaction_expected
        && remote_compact_requests > 0
        && is_canonical_engine
        && !canonical_journal_valid
    {
        protocol_violations.push("canonical_journal_invalid".into());
    }
    if compaction_expected
        && remote_compact_requests > 0
        && is_canonical_engine
        && !engine_signature_valid
    {
        protocol_violations.push("canonical_engine_signature_invalid".into());
    }
    let protocol_recovered = compaction_recovered.unwrap_or(true)
        && model_switch_recovered.unwrap_or(true)
        && protocol_violations.is_empty();
    let task_acceptance = acceptance_passed == task.verification.len() as u64;
    let transport_protocol = gateway_summary.disconnected_streams == 0
        && gateway_http_ok
        && (gateway_summary.missing_terminal_sse == 0 || any_timed_out);
    // `completed_tool_calls` counts both item.completed and item.failed
    // terminals. A policy-declined command is still paired correctly, while
    // policy acceptance remains a separate evaluator layer.
    let tool_protocol = metrics.reasoning_leaks == 0
        && metrics.malformed_tool_calls == 0
        && metrics.completed_tool_calls >= metrics.tool_calls;
    let expected_tool_activity = metrics.tool_calls >= task.expected_harness.min_tool_calls;
    let compaction_triggered = observed_compaction_count > 0;
    let canonical_materialized = if is_canonical_engine {
        compaction_triggered && canonical_journal_valid && engine_signature_valid
    } else if compaction_engine == "codex-local-native-oracle" {
        compaction_triggered && observed_engine == "codex_local_native_oracle"
    } else {
        compaction_triggered && valid_compaction_responses > 0 && journal_changed
    };
    let compaction_applied = if is_canonical_engine {
        canonical_materialized
    } else if compaction_engine == "codex-local-native-oracle" {
        observed_engine == "codex_local_native_oracle"
    } else if compaction_engine == "codex-local-v0-150" {
        observed_engine == "codex_local_v0_150"
    } else {
        compaction_triggered && valid_compaction_responses > 0 && journal_changed
    };
    let session_resumed = all_resumes_observed(task.phases.len(), resumed_phases_observed)
        && session_id.is_some()
        && metrics.session_reinitializations == 0;
    let resource_budget = !any_timed_out && !limit_exceeded;
    let mechanism_exercised = !requires_enhanced_mechanism
        || task
            .expected_harness
            .min_enhanced_events
            .iter()
            .all(|(name, expected)| {
                metrics
                    .enhanced_event_counts
                    .get(name)
                    .copied()
                    .unwrap_or(0)
                    >= *expected
            });
    let task_correctness_passed = task_acceptance && continuity_preserved;
    let protocol_qualified =
        transport_protocol && tool_protocol && protocol_recovered && !task_stall_watchdog_stop;
    let promotion_eligible = task_acceptance
        && transport_protocol
        && tool_protocol
        && expected_tool_activity
        && (!compaction_expected || compaction_applied)
        && (!task.expected_harness.require_session_resume || session_resumed)
        && continuity_preserved
        && resource_budget
        && runtime_attribution
        && mechanism_exercised
        && observed_engine != "mixed"
        && (!compaction_expected || engine_matched);
    let layers = LayerResults {
        task_acceptance,
        transport_protocol,
        tool_protocol,
        expected_tool_activity,
        compaction_triggered,
        canonical_materialized,
        compaction_applied,
        session_resumed,
        continuity_preserved,
        resource_budget,
        runtime_attribution,
        mechanism_exercised,
        promotion_eligible,
    };
    // The CLI's PASS/FAIL must represent the same layered qualification shown
    // in the report. In particular, a grader that accidentally exits zero
    // cannot promote an agent which never exercised the required tools.
    let passed =
        promotion_eligible && agent_completed && protocol_recovered && !task_stall_watchdog_stop;
    let classified_errors = (!metrics.errors.is_empty()).then(|| classify_errors(&metrics.errors));
    let compaction_failure_category = compaction_attempts
        .iter()
        .rev()
        .find_map(|attempt| attempt.failure.clone());
    let failure_evidence = TerminalFailureEvidence {
        passed,
        protocol_violations: protocol_violations.clone(),
        compaction_requests: gateway_summary.compaction_requests,
        valid_compaction_responses,
        request_count: gateway_summary.request_count,
        timed_out,
        verification_timed_out,
        http_errors: gateway_http_errors,
        transport_protocol_ok: transport_protocol,
        tool_protocol_ok: tool_protocol,
        task_acceptance_passed: task_acceptance,
        classified_errors: classified_errors.map(str::to_string),
        compaction_failure_category: compaction_failure_category.clone(),
    };
    let classified_failure_origin = classify_failure_origin(&failure_evidence);
    let resource_budget_exhausted = swe_turn_cap_reached
        || swe_token_limit_reached
        || limit_exceeded
        || (timed_out && gateway_summary.request_count > 0);
    let failure_origin = resolve_failure_origin(
        passed,
        task_stall_watchdog_stop,
        resource_budget_exhausted,
        verification_timed_out,
        classified_failure_origin,
    );

    let engine_void = protocol_violations
        .iter()
        .find(|violation| matches!(violation.as_str(), "wrong_engine" | "mixed_engine"));
    let failure_class = if passed {
        None
    } else if let Some(engine_void) = engine_void {
        Some(engine_void.clone())
    } else if swe_turn_cap_reached || swe_token_limit_reached {
        Some("performance_budget".into())
    } else if metrics.rejected_tool_calls > 0 {
        Some("executor_policy".into())
    } else if task_stall_watchdog_stop {
        Some("model_no_progress_after_recovery".into())
    } else if protocol_violations
        .iter()
        .any(|violation| violation == "compaction_not_armed")
    {
        Some("harness_trigger_config".into())
    } else if limit_exceeded {
        Some("performance_budget".into())
    } else if failure_origin == Some(FailureOrigin::Provider) {
        Some(
            terminal_provider_failure_class(&failure_evidence)
                .unwrap_or("provider_unavailable")
                .into(),
        )
    } else if failure_origin == Some(FailureOrigin::VellumCompaction) {
        Some(compaction_failure_category.unwrap_or_else(|| "vellum_compaction".into()))
    } else if failure_origin == Some(FailureOrigin::EvaluatorInfrastructure) {
        Some("evaluator_infrastructure".into())
    } else if failure_origin == Some(FailureOrigin::Model) {
        Some("model_capability".into())
    } else if timed_out {
        Some("performance_budget".into())
    } else if !protocol_violations.is_empty() {
        Some(
            if protocol_violations.iter().any(|violation| {
                matches!(
                    violation.as_str(),
                    "terminal_sse_not_observed" | "missing_terminal_sse"
                )
            }) {
                "evaluator_observation"
            } else {
                "codex_harness_incompatibility"
            }
            .into(),
        )
    } else if !metrics.errors.is_empty() {
        Some(classify_errors(&metrics.errors).into())
    } else if !task_acceptance && transport_protocol && tool_protocol {
        Some("model_capability".into())
    } else if !agent_completed || !protocol_recovered {
        Some("codex_harness_incompatibility".into())
    } else {
        Some("indeterminate".into())
    };
    let root_cause = failure_class.clone();
    let mut supporting_evidence = vec![
        format!("acceptance={acceptance_passed}/{}", task.verification.len()),
        format!(
            "gateway_http_faults=expected:{} recovered:{} unrecovered:{} unexpected:{}",
            http_faults
                .iter()
                .filter(|observation| observation.expected_injection)
                .count(),
            http_faults
                .iter()
                .filter(|observation| observation.recovered)
                .count(),
            http_faults
                .iter()
                .filter(|observation| { observation.expected_injection && !observation.recovered })
                .count(),
            http_faults
                .iter()
                .filter(|observation| !observation.expected_injection)
                .count(),
        ),
        format!(
            "gateway_requests={} terminal_streams={} disconnected_streams={}",
            gateway_summary.request_count,
            gateway_summary.terminal_streams,
            gateway_summary.disconnected_streams
        ),
        format!(
            "compact_requests={} valid_compact_responses={valid_compaction_responses} \
             requested_engine={compaction_engine} observed_engine={observed_engine} \
             engine_matched={engine_matched} codex_compaction_events={codex_compaction_events} \
             vellum_canonical_journal_count={vellum_canonical_journal_count}",
            gateway_summary.compaction_requests
        ),
        format!(
            "session_resume_observed={}/{} phases_completed={} session_reinitializations={}",
            resumed_phases_observed,
            task.phases.len(),
            resumed_phases_completed,
            metrics.session_reinitializations
        ),
        format!(
            "tokens=input:{} output:{} limits=input:{} output:{}",
            metrics.input_tokens,
            metrics.output_tokens,
            effective_max_input_tokens,
            task.limits.max_output_tokens
        ),
    ];
    if let Some(journal) = journal_after.as_ref().filter(|_| journal_changed) {
        supporting_evidence.push(format!(
            "journal=schema:{} kind:{:?} hash:{}",
            journal.schema_version, journal.kind, journal.canonical_sha256
        ));
    }
    if any_timed_out {
        supporting_evidence.push("case_or_verification_timed_out=true".into());
    }
    if let Some(stall) = task_stall_diagnostics.last() {
        supporting_evidence.push(format!(
            "task_stall=since_progress:{} recovery_count:{} post_recovery:{} candidate:{} terminal:{}",
            stall.tool_results_since_progress,
            stall.recovery_injected_count,
            stall.post_recovery_no_progress,
            stall.watchdog_candidate,
            task_stall_watchdog_stop,
        ));
    }
    supporting_evidence.push(format!(
        "usage_coverage=settled:{} zero:{} available:{}",
        task_efficiency_usage_coverage.settled_request_count,
        task_efficiency_usage_coverage.zero_usage_request_count,
        task_efficiency_usage_coverage.token_accounting_available,
    ));
    if !protocol_violations.is_empty() {
        supporting_evidence.push(format!(
            "protocol_violations={}",
            protocol_violations.join(",")
        ));
    }
    if !harness_expectation_misses.is_empty() {
        supporting_evidence.push(format!(
            "harness_expectation_misses={}",
            harness_expectation_misses.join(",")
        ));
    }
    if metrics.rejected_tool_calls > 0 {
        supporting_evidence.push(format!(
            "executor_policy_rejections={}",
            metrics.rejected_tool_calls
        ));
    }
    supporting_evidence.extend(
        metrics
            .errors
            .iter()
            .take(3)
            .map(|error| format!("error={error}")),
    );
    gateway.revoke(&credential.token).await;
    Ok(TaskResult {
        run_id: run_id.into(),
        case_id,
        task_id: task.id.clone(),
        category: category_name(&task.category).into(),
        model: model.into(),
        provider: provider_names
            .get(model)
            .cloned()
            .unwrap_or_else(|| "unknown".into()),
        context_window: if natural_context {
            available.get(model).and_then(|model| model.context_window)
        } else {
            task.forced_context_window
                .or_else(|| available.get(model).and_then(|model| model.context_window))
        },
        compaction_mode: matches!(task.category, TaskCategory::LongContext).then(|| {
            if task.forced_context_window.is_some() && !natural_context {
                "forced".into()
            } else {
                "natural".into()
            }
        }),
        compact_request_count: remote_compact_requests,
        canonical_hash: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .map(|journal| journal.canonical_sha256.clone())
            .filter(|hash| !hash.is_empty()),
        canonical_schema_version: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|j| j.checkpoint_schema_version)
            .or_else(|| {
                journal_after
                    .as_ref()
                    .filter(|_| journal_changed)
                    .map(|j| j.schema_version)
            }),
        journal_schema_version: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .map(|journal| journal.schema_version),
        checkpoint_schema_version: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|journal| journal.checkpoint_schema_version),
        canonical_strategy: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|journal| journal.strategy.clone()),
        continuity_kind: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|journal| journal.continuity_kind.clone()),
        requested_canonical_engine: Some(compaction_engine.to_string()),
        observed_canonical_engine: observed_compaction_engine(
            compaction_engine,
            journal_changed,
            journal_after
                .as_ref()
                .and_then(|journal| journal.strategy.as_deref()),
        ),
        requested_compaction_engine: Some(compaction_engine.to_string()),
        observed_compaction_engine: Some(observed_engine.clone()),
        compaction_engine_matched: Some(engine_matched),
        codex_compaction_events,
        remote_compact_requests,
        vellum_canonical_journal_count,
        catalog_hash,
        eval_provider_display_name: Some(
            eval_codex_provider_display_name(compaction_engine).into(),
        ),
        // Both engines use the same gateway estimator: the largest request is
        // the pre-compaction surface and the last later request is the
        // observable post-compaction surface. If no later request exists the
        // post value stays absent instead of manufacturing a ratio.
        pre_compaction_model_visible_tokens: engine_neutral_pre_tokens,
        post_compaction_model_visible_tokens: engine_neutral_post_tokens,
        generation: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .map(|journal| journal.generation),
        source_tokens: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|journal| journal.audit.as_ref())
            .map(|audit| audit.source_tokens),
        checkpoint_tokens: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|journal| journal.audit.as_ref())
            .map(|audit| audit.checkpoint_tokens),
        semantic_claim_count: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|journal| journal.audit.as_ref())
            .map(|audit| audit.semantic_claim_count),
        grounded_claim_count: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|journal| journal.audit.as_ref())
            .map(|audit| audit.grounded_claim_count),
        rejected_claim_count: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|journal| journal.audit.as_ref())
            .map(|audit| audit.rejected_claim_count),
        grounding_rate: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|journal| journal.audit.as_ref())
            .map(|audit| {
                if audit.semantic_claim_count == 0 {
                    1.0
                } else {
                    audit.grounded_claim_count as f64 / audit.semantic_claim_count as f64
                }
            }),
        extraction_attempts: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|journal| journal.audit.as_ref())
            .map(|audit| audit.extraction_attempts as usize),
        fallback_used: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|journal| journal.audit.as_ref())
            .map(|audit| audit.fallback_used),
        repeated_exchanges_collapsed: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|journal| journal.audit.as_ref())
            .map(|audit| audit.repeated_exchanges_collapsed),
        canonical_kind: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .map(|journal| format!("{:?}", journal.kind).to_ascii_lowercase()),
        injected_faults: task
            .faults
            .iter()
            .map(|fault| fault_kind_name(&fault.kind).into())
            .collect(),
        http_faults,
        switched_models,
        source_model_visible_tokens: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|j| j.audit.as_ref())
            .and_then(|a| a.source_model_visible_tokens),
        replacement_model_visible_tokens: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|j| j.audit.as_ref())
            .and_then(|a| a.replacement_model_visible_tokens),
        replacement_durable_tokens: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|j| j.audit.as_ref())
            .and_then(|a| a.replacement_durable_tokens),
        model_visible_compression_ratio: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|j| j.audit.as_ref())
            .and_then(|a| a.model_visible_compression_ratio)
            .or_else(|| model_visible_ratio(engine_neutral_pre_tokens, engine_neutral_post_tokens)),
        source_hash: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|j| j.audit.as_ref())
            .map(|a| a.source_hash.clone())
            .or_else(|| {
                journal_after
                    .as_ref()
                    .filter(|_| journal_changed)
                    .and_then(|j| j.source_hash.clone())
            }),
        fallback_reason: journal_after
            .as_ref()
            .filter(|_| journal_changed)
            .and_then(|j| j.audit.as_ref())
            .and_then(|a| a.fallback_reason.as_ref())
            .and_then(|r| serde_json::to_value(r).ok())
            .and_then(|v| v.as_str().map(str::to_string)),
        failure_origin,
        phase_observations,
        phase_execution_observations,
        compaction_attempts,
        semantic_validation_diagnostics,
        task_stall_diagnostics,
        task_stall_recoveries,
        task_stall_terminals,
        task_efficiency_diagnostics,
        task_efficiency_recoveries,
        task_efficiency_usage_coverage,
        transition: executed_models.join(" -> "),
        repetition,
        passed,
        task_correctness_passed,
        performance_qualified: resource_budget,
        protocol_qualified,
        agent_completed,
        timed_out: any_timed_out,
        limit_exceeded,
        max_input_tokens: Some(effective_max_input_tokens),
        max_output_tokens: Some(task.limits.max_output_tokens),
        duration_ms: started.elapsed().as_millis() as u64,
        first_byte_ms,
        compaction_recovered,
        model_switch_recovered,
        acceptance_passed,
        acceptance_total: task.verification.len() as u64,
        verification_exit_code,
        verification_output,
        changed_files,
        added_lines,
        removed_lines,
        metrics,
        protocol_violations,
        harness_expectation_misses,
        failure_class,
        root_cause,
        supporting_evidence,
        error: None,
        layers,
    })
}

pub fn classify_phase_trigger_decision(
    compact_requests_before: u64,
    compact_requests_after: u64,
    estimated_context_tokens: u64,
    auto_compact_token_limit: u64,
) -> PhaseTriggerDecision {
    classify_phase_trigger_decision_with_codex(
        compact_requests_before,
        compact_requests_after,
        0,
        0,
        estimated_context_tokens,
        auto_compact_token_limit,
    )
}

fn classify_phase_trigger_decision_with_codex(
    compact_requests_before: u64,
    compact_requests_after: u64,
    codex_compaction_events_before: u64,
    codex_compaction_events_after: u64,
    estimated_context_tokens: u64,
    auto_compact_token_limit: u64,
) -> PhaseTriggerDecision {
    if compact_requests_after > compact_requests_before
        || codex_compaction_events_after > codex_compaction_events_before
    {
        PhaseTriggerDecision::CompactionObserved
    } else if auto_compact_token_limit > 0 && estimated_context_tokens >= auto_compact_token_limit {
        PhaseTriggerDecision::AboveThresholdWithoutRequest
    } else if estimated_context_tokens > 0 {
        PhaseTriggerDecision::BelowThreshold
    } else {
        PhaseTriggerDecision::UnknownMissingTokens
    }
}

fn engine_neutral_compaction_window(
    request_tokens: &HashMap<u64, u64>,
) -> (Option<u64>, Option<u64>) {
    let mut ordered = request_tokens
        .iter()
        .filter(|(_, tokens)| **tokens > 0)
        .map(|(index, tokens)| (*index, *tokens))
        .collect::<Vec<_>>();
    ordered.sort_by_key(|(index, _)| *index);
    let Some((pre_position, (_, pre_tokens))) = ordered
        .iter()
        .enumerate()
        // `max_by_key` would select the last equal maximum. Reverse the
        // ordering tie-break so a later request remains available as the
        // post-compaction observation.
        .max_by_key(|(position, (_, tokens))| (*tokens, std::cmp::Reverse(*position)))
    else {
        return (None, None);
    };
    let post_tokens = ordered
        .iter()
        .skip(pre_position + 1)
        .next_back()
        .map(|(_, tokens)| *tokens);
    (Some(*pre_tokens), post_tokens)
}

fn model_visible_ratio(pre_tokens: Option<u64>, post_tokens: Option<u64>) -> Option<f64> {
    pre_tokens
        .zip(post_tokens)
        .filter(|(pre, _)| *pre > 0)
        .map(|(pre, post)| post as f64 / pre as f64)
}

#[derive(Debug, Clone, Default)]
pub struct TerminalFailureEvidence {
    pub passed: bool,
    pub protocol_violations: Vec<String>,
    pub compaction_requests: u64,
    pub valid_compaction_responses: u64,
    pub request_count: u64,
    pub timed_out: bool,
    pub verification_timed_out: bool,
    pub http_errors: Vec<String>,
    pub transport_protocol_ok: bool,
    pub tool_protocol_ok: bool,
    pub task_acceptance_passed: bool,
    pub classified_errors: Option<String>,
    pub compaction_failure_category: Option<String>,
}

fn is_deterministic_eval_http_fault(error: &str) -> bool {
    [
        "(http_429)",
        "(http_500)",
        "(http_524)",
        "(swe_turn_cap)",
        "(swe_token_limit)",
        "(single_run_official_cap)",
        "(luna_campaign_quota)",
    ]
    .iter()
    .any(|marker| error.contains(marker))
}

fn terminal_provider_failure_class(evidence: &TerminalFailureEvidence) -> Option<&'static str> {
    evidence.http_errors.iter().find_map(|error| {
        if is_deterministic_eval_http_fault(error) {
            None
        } else if error.contains("429") {
            Some("provider_quota")
        } else if error.contains("stream_open_timeout") {
            Some("stream_open_timeout")
        } else if error.contains("502") || error.contains("provider_protocol") {
            Some("provider_protocol")
        } else if error.contains("503")
            || error.contains("524")
            || error.contains("provider_unavailable")
            || (error.contains("500") && error.contains("(upstream)"))
        {
            Some("provider_unavailable")
        } else {
            None
        }
    })
}

pub fn classify_failure_origin(evidence: &TerminalFailureEvidence) -> Option<FailureOrigin> {
    if evidence.passed {
        return None;
    }

    // 1. Evaluator infrastructure faults FIRST (exact string matches, not loose substring!)
    // "compaction_not_armed" is an evaluator harness expectation failure, NOT a Vellum compaction failure.
    if (evidence.timed_out && evidence.request_count == 0)
        || evidence.verification_timed_out
        || evidence
            .http_errors
            .iter()
            .any(|error| is_deterministic_eval_http_fault(error))
        || evidence.protocol_violations.iter().any(|v| {
            matches!(
                v.as_str(),
                "compaction_not_armed"
                    | "terminal_sse_not_observed"
                    | "missing_terminal_sse"
                    | "runner_case_failed"
                    | "wrong_engine"
                    | "mixed_engine"
            )
        })
    {
        return Some(FailureOrigin::EvaluatorInfrastructure);
    }

    // The runtime's terminal category is authoritative. Provider categories
    // retain their origin; every other compaction category belongs to Vellum.
    if let Some(category) = evidence.compaction_failure_category.as_deref() {
        return Some(
            if matches!(
                category,
                "provider_quota"
                    | "provider_unavailable"
                    | "provider_protocol"
                    | "stream_open_timeout"
            ) {
                FailureOrigin::Provider
            } else {
                FailureOrigin::VellumCompaction
            },
        );
    }

    // 2. Known local Canonical / quality / journal rejection faults
    if evidence.protocol_violations.iter().any(|v| {
        matches!(
            v.as_str(),
            "canonical_journal_invalid"
                | "canonical_engine_signature_invalid"
                | "canonical_compaction_failed"
                | "quality_gate"
        )
    }) {
        return Some(FailureOrigin::VellumCompaction);
    }

    // 3. Structured upstream Provider failure. Injected evaluator faults were
    // consumed above and cannot masquerade as provider instability.
    // Checked BEFORE zero valid compaction fallback so provider 429 on a compact request correctly attributes to Provider!
    if terminal_provider_failure_class(evidence).is_some() {
        return Some(FailureOrigin::Provider);
    }

    // 4. Zero valid compaction responses without provider HTTP error (local silent skip or fault)
    if evidence.compaction_requests > 0 && evidence.valid_compaction_responses == 0 {
        return Some(FailureOrigin::VellumCompaction);
    }

    // 5. Model capability/acceptance failure
    if !evidence.task_acceptance_passed
        && evidence.transport_protocol_ok
        && evidence.tool_protocol_ok
    {
        return Some(FailureOrigin::Model);
    }

    Some(FailureOrigin::Unknown)
}

fn resolve_failure_origin(
    passed: bool,
    task_stall_terminal: bool,
    resource_budget_exhausted: bool,
    verification_timed_out: bool,
    classified: Option<FailureOrigin>,
) -> Option<FailureOrigin> {
    if passed {
        None
    } else if task_stall_terminal {
        Some(FailureOrigin::Model)
    } else if classified == Some(FailureOrigin::Provider) {
        classified
    } else if resource_budget_exhausted && !verification_timed_out {
        Some(FailureOrigin::ResourceBudget)
    } else {
        classified
    }
}

pub fn retryable_provider_failure(result: &TaskResult) -> bool {
    if result.failure_origin != Some(FailureOrigin::Provider) {
        return false;
    }
    // Deny permanent client budget caps / turn caps
    if result.metrics.errors.iter().any(|e| {
        e.contains("swe_turn_cap")
            || e.contains("swe_token_limit")
            || e.contains("campaign_quota")
            || e.contains("budget_exceeded")
            || e.contains("auth_invalid")
    }) {
        return false;
    }
    // Allowlist only structured, explicitly retryable provider classes.
    matches!(
        result.failure_class.as_deref(),
        Some(
            "provider_quota" | "provider_unavailable" | "provider_protocol" | "stream_open_timeout"
        )
    )
}

fn swe_execution_budget_prompt(prompt: &str) -> String {
    format!(
        "{prompt}\n\nEvaluation execution budget: you have at most {SWE_MAX_TURNS_PER_TASK} model turns. Inspect the smallest relevant surface, implement the minimal correct patch, run targeted tests, and finish as soon as the task is verified. Do not restart broad repository exploration after context compaction."
    )
}

fn long_context_execution_prompt(prompt: &str) -> String {
    let read_instruction = if prompt.contains("EVAL_CONTEXT.md") {
        "When you read EVAL_CONTEXT.md, the shell result must return the complete file to your context. Do not request a bounded or truncated preview; if the tool exposes max_output_tokens, set it to at least 12000. A truncation warning does not satisfy the read. Read the file only once."
    } else {
        "Before editing, read EVAL_CONTEXT.md once with a shell command. The shell result must return the complete file to your context. Do not request a bounded or truncated preview; if the tool exposes max_output_tokens, set it to at least 12000. Treat its constraints as authoritative evaluation context and do not read it again after compaction."
    };
    format!("{prompt}\n\nEvaluation trigger contract: {read_instruction}")
}

fn write_long_context_fixture(workspace: &Path) -> AppResult<()> {
    let mut content = String::from(
        "# Long-context evaluation constraints\n\n\
         Preserve the public function name and signature from the starter file. \
         Keep the patch focused, preserve package exports, and verify behavior before finishing. \
         Reading this file once is sufficient; never re-read it after compaction.\n\n",
    );
    let constraints = [
        "Preserve the established public API and exact function signature.",
        "Avoid unrelated changes and keep the patch limited to the requested behavior.",
        "Preserve package exports and existing module boundaries.",
        "Continue from the current workspace state after context compaction.",
        "Run focused behavior checks before reporting completion.",
    ];
    // Keep the fixture large enough to cross the 16k forced-compaction
    // threshold after Codex reads it, but small enough that third-party
    // summarizers do not enter an immediate re-compaction loop.
    for index in 0..450 {
        content.push_str(&format!(
            "- Context record {index:04}: {}\n",
            constraints[index % constraints.len()]
        ));
    }
    std::fs::write(workspace.join("EVAL_CONTEXT.md"), content)
        .map_err(io_error("write long-context fixture"))
}

fn merge_metrics(target: &mut EventMetrics, source: EventMetrics) {
    if source.session_id.is_some() {
        target.session_id = source.session_id;
    }
    target.tool_calls += source.tool_calls;
    target.completed_tool_calls += source.completed_tool_calls;
    target.successful_tool_calls += source.successful_tool_calls;
    target.failed_tool_calls += source.failed_tool_calls;
    target.rejected_tool_calls += source.rejected_tool_calls;
    target.malformed_tool_calls += source.malformed_tool_calls;
    target.duplicate_commands += source.duplicate_commands;
    target.reasoning_leaks += source.reasoning_leaks;
    target.retries += source.retries;
    target.compactions += source.compactions;
    // `codex exec resume --json` reports cumulative thread usage, while tool
    // events only describe the current process. Keep the latest cumulative
    // counters instead of charging every resumed phase again.
    target.input_tokens = target.input_tokens.max(source.input_tokens);
    target.output_tokens = target.output_tokens.max(source.output_tokens);
    target.reasoning_tokens = target.reasoning_tokens.max(source.reasoning_tokens);
    target.cached_tokens = target.cached_tokens.max(source.cached_tokens);
    target.tool_duration_ms += source.tool_duration_ms;
    target.repeated_file_changes += source.repeated_file_changes;
    target.errors.extend(source.errors);
    target.commands.extend(source.commands);
    target.changed_files.extend(source.changed_files);
    if source.enhanced_runtime.is_some() {
        target.enhanced_runtime = source.enhanced_runtime;
    }
    for (name, count) in source.enhanced_event_counts {
        *target.enhanced_event_counts.entry(name).or_default() += count;
    }
}

/// Merge separately-started attempts. Unlike resumed phases, their token
/// counters are independent and must be charged additively.
fn merge_attempt_metrics(
    target: &mut EventMetrics,
    previous: &EventMetrics,
    target_has_cumulative_gateway_usage: bool,
) {
    target.tool_calls = target.tool_calls.saturating_add(previous.tool_calls);
    target.completed_tool_calls = target
        .completed_tool_calls
        .saturating_add(previous.completed_tool_calls);
    target.successful_tool_calls = target
        .successful_tool_calls
        .saturating_add(previous.successful_tool_calls);
    target.failed_tool_calls = target
        .failed_tool_calls
        .saturating_add(previous.failed_tool_calls);
    target.rejected_tool_calls = target
        .rejected_tool_calls
        .saturating_add(previous.rejected_tool_calls);
    target.malformed_tool_calls = if target_has_cumulative_gateway_usage {
        target
            .malformed_tool_calls
            .max(previous.malformed_tool_calls)
    } else {
        target
            .malformed_tool_calls
            .saturating_add(previous.malformed_tool_calls)
    };
    target.duplicate_commands = target
        .duplicate_commands
        .saturating_add(previous.duplicate_commands);
    target.reasoning_leaks = target
        .reasoning_leaks
        .saturating_add(previous.reasoning_leaks);
    target.retries = target.retries.saturating_add(previous.retries);
    target.compactions = if target_has_cumulative_gateway_usage {
        target.compactions.max(previous.compactions)
    } else {
        target.compactions.saturating_add(previous.compactions)
    };
    if target_has_cumulative_gateway_usage {
        target.input_tokens = target.input_tokens.max(previous.input_tokens);
        target.output_tokens = target.output_tokens.max(previous.output_tokens);
    } else {
        target.input_tokens = target.input_tokens.saturating_add(previous.input_tokens);
        target.output_tokens = target.output_tokens.saturating_add(previous.output_tokens);
    }
    target.reasoning_tokens = target
        .reasoning_tokens
        .saturating_add(previous.reasoning_tokens);
    target.cached_tokens = target.cached_tokens.saturating_add(previous.cached_tokens);
    target.tool_duration_ms = target
        .tool_duration_ms
        .saturating_add(previous.tool_duration_ms);
    target.repeated_file_changes = target
        .repeated_file_changes
        .saturating_add(previous.repeated_file_changes);
    if target_has_cumulative_gateway_usage {
        target.terminal_sse_missing = target
            .terminal_sse_missing
            .max(previous.terminal_sse_missing);
        target.disconnected_streams = target
            .disconnected_streams
            .max(previous.disconnected_streams);
    } else {
        target.terminal_sse_missing = target
            .terminal_sse_missing
            .saturating_add(previous.terminal_sse_missing);
        target.disconnected_streams = target
            .disconnected_streams
            .saturating_add(previous.disconnected_streams);
    }
    target.session_reinitializations = target
        .session_reinitializations
        .saturating_add(previous.session_reinitializations);
    if !target_has_cumulative_gateway_usage {
        target.errors.extend(previous.errors.iter().cloned());
    }
    target.commands.extend(previous.commands.iter().cloned());
    target
        .changed_files
        .extend(previous.changed_files.iter().cloned());
    if target.enhanced_runtime.is_none() {
        target.enhanced_runtime = previous.enhanced_runtime.clone();
    }
    for (name, count) in &previous.enhanced_event_counts {
        *target
            .enhanced_event_counts
            .entry(name.clone())
            .or_default() += count;
    }
}

fn redact_event_metrics(metrics: &mut EventMetrics, task_token: &str) {
    for value in metrics.errors.iter_mut().chain(metrics.commands.iter_mut()) {
        *value = redact_text_with_secrets(value, &[task_token]);
    }
}

#[derive(Default, Clone)]
struct GatewayTraceSummary {
    request_count: u64,
    max_request_index: u64,
    compaction_requests: u64,
    valid_compaction_responses: u64,
    terminal_streams: u64,
    missing_terminal_sse: u64,
    disconnected_streams: u64,
    malformed_tool_arguments: u64,
    input_tokens: u64,
    output_tokens: u64,
    settled_request_count: u64,
    zero_usage_request_count: u64,
    http_errors: Vec<String>,
    http_error_details: Vec<GatewayHttpError>,
    terminal_request_indices: HashSet<u64>,
    request_model_visible_tokens_by_index: HashMap<u64, u64>,
    latest_request_model_visible_tokens: u64,
}

#[derive(Clone)]
struct GatewayHttpError {
    request_index: u64,
    fault: String,
    message: String,
    request_total_estimated_tokens: Option<u64>,
}

fn classify_gateway_http_faults(
    summary: &GatewayTraceSummary,
    faults: &[FaultSpec],
) -> Vec<HttpFaultObservation> {
    summary
        .http_error_details
        .iter()
        .map(|error| {
            let expected_injection = faults.iter().any(|expected| {
                let position_matches =
                    expected
                        .while_total_estimated_tokens_above
                        .is_some_and(|threshold| {
                            error.request_index >= u64::from(expected.at_request)
                                && error
                                    .request_total_estimated_tokens
                                    .is_some_and(|actual| actual > threshold)
                        })
                        || (expected.while_total_estimated_tokens_above.is_none()
                            && u64::from(expected.at_request) == error.request_index);
                position_matches && fault_kind_name(&expected.kind) == error.fault
            });
            HttpFaultObservation {
                request_index: error.request_index,
                fault: error.fault.clone(),
                message: error.message.clone(),
                expected_injection,
                recovered: expected_injection
                    && summary
                        .terminal_request_indices
                        .iter()
                        .any(|request_index| *request_index > error.request_index),
                request_total_estimated_tokens: error.request_total_estimated_tokens,
            }
        })
        .collect()
}

fn read_gateway_trace(path: &Path) -> AppResult<GatewayTraceSummary> {
    if !path.is_file() {
        return Ok(GatewayTraceSummary::default());
    }
    let text = std::fs::read_to_string(path).map_err(io_error("read eval gateway trace"))?;
    let mut summary = GatewayTraceSummary::default();
    let mut terminal_request_ids = HashSet::new();
    let mut successful_stream_request_ids = HashSet::new();
    let mut compaction_request_ids = HashSet::new();
    let mut usage_by_request = HashMap::<u64, (u64, u64)>::new();
    for value in text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    {
        let event = value
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or("request");
        let path = value.get("path").and_then(Value::as_str).unwrap_or("");
        let status = value.get("status").and_then(Value::as_u64).unwrap_or(0);
        let compaction = value
            .get("compaction")
            .and_then(Value::as_bool)
            .unwrap_or(path == "/v1/responses/compact");
        let request_index = value
            .get("requestIndex")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        summary.max_request_index = summary.max_request_index.max(request_index);
        if let Some(input) = value.get("inputTokens").and_then(Value::as_u64) {
            let output = value
                .get("outputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let entry = usage_by_request.entry(request_index).or_default();
            entry.0 = entry.0.max(input);
            entry.1 = entry.1.max(output);
        }
        if let Some(mv_tokens) = value
            .get("requestModelVisibleTokens")
            .and_then(Value::as_u64)
        {
            summary
                .request_model_visible_tokens_by_index
                .insert(request_index, mv_tokens);
            summary.latest_request_model_visible_tokens = mv_tokens;
        }
        if event == "request" && compaction {
            compaction_request_ids.insert(request_index);
            summary.compaction_requests += 1;
            if (200..300).contains(&status) && value.get("fault").and_then(Value::as_str).is_none()
            {
                summary.valid_compaction_responses += 1;
            }
        }
        if event == "request" {
            summary.request_count += 1;
            summary.malformed_tool_arguments += value
                .get("malformedToolArguments")
                .and_then(Value::as_u64)
                .unwrap_or(0);
        }
        if event == "terminal_observed"
            || value
                .get("terminalSse")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            terminal_request_ids.insert(request_index);
        }
        if matches!(event, "stream_end" | "stream_dropped") {
            if status < 400 {
                successful_stream_request_ids.insert(request_index);
            }
            if value
                .get("terminalSse")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                terminal_request_ids.insert(request_index);
            }
            if value
                .get("disconnected")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                summary.disconnected_streams += 1;
            }
        }
        if event == "request" && status >= 400 {
            let fault = value
                .get("fault")
                .and_then(Value::as_str)
                .unwrap_or("upstream");
            let message = format!("gateway HTTP {status} at {path} ({fault})");
            summary.http_errors.push(message.clone());
            summary.http_error_details.push(GatewayHttpError {
                request_index,
                fault: fault.to_string(),
                message,
                request_total_estimated_tokens: value
                    .get("requestTotalEstimatedTokens")
                    .and_then(Value::as_u64),
            });
        }
    }
    summary.terminal_request_indices = terminal_request_ids.clone();
    summary.terminal_streams = terminal_request_ids.len() as u64;
    summary.missing_terminal_sse = successful_stream_request_ids
        .difference(&terminal_request_ids)
        .count() as u64;
    let settled_task_requests = successful_stream_request_ids
        .difference(&compaction_request_ids)
        .copied()
        .collect::<HashSet<_>>();
    summary.settled_request_count = settled_task_requests.len() as u64;
    summary.zero_usage_request_count = settled_task_requests
        .iter()
        .filter(|request_index| {
            usage_by_request
                .get(request_index)
                .is_none_or(|(input, output)| *input == 0 && *output == 0)
        })
        .count() as u64;
    for (input, output) in usage_by_request.into_values() {
        summary.input_tokens = summary.input_tokens.saturating_add(input);
        summary.output_tokens = summary.output_tokens.saturating_add(output);
    }
    Ok(summary)
}

fn write_phase_output(
    run_root: &Path,
    case_id: &str,
    phase: usize,
    output: &CommandOutput,
    task_token: &str,
) -> AppResult<()> {
    let trace_root = run_root.join("traces");
    std::fs::write(
        trace_root.join(format!("{case_id}.phase-{phase}.jsonl")),
        redact_text_with_secrets(&output.stdout, &[task_token]),
    )
    .map_err(io_error("write Codex JSONL trace"))?;
    std::fs::write(
        trace_root.join(format!("{case_id}.phase-{phase}.stderr.txt")),
        redact_text_with_secrets(&output.stderr, &[task_token]),
    )
    .map_err(io_error("write Codex stderr trace"))
}

async fn initialize_git(workspace: &Path) -> AppResult<()> {
    let has_existing_repository = workspace.join(".git").is_dir();
    let mut commands = vec![
        vec!["config", "user.email", "vellum-eval@localhost"],
        vec!["config", "user.name", "Vellum Eval"],
        // Docker Desktop exposes a Windows-hosted workspace to the Linux
        // agent with executable permission bits that do not match the pinned
        // repository index. Without this setting every file appears modified
        // after the first tool call, drowning the continuation model in a
        // large mode-only diff and producing false timeout/repetition results.
        // SWE-bench grades file contents, not host filesystem mode emulation.
        vec!["config", "core.fileMode", "false"],
        // The fixture is extracted on Windows before being bind-mounted into
        // Linux. Make Git compare CRLF worktree files against the normalized
        // index exactly as the host checkout does; otherwise generated source
        // trees appear as thousands of content changes inside the agent.
        vec!["config", "core.autocrlf", "true"],
    ];
    if !has_existing_repository {
        commands.insert(0, vec!["init", "-q"]);
    }
    // The official SWE-bench images can contain generated, untracked files
    // (for example build/). They are part of the pinned starting state, not
    // agent output. Snapshot the fully materialized workspace after applying
    // the host-mode normalization so later diff and repetition metrics only
    // describe changes made by the evaluated model.
    commands.push(vec!["add", "-A"]);
    commands.push(vec!["commit", "-q", "--allow-empty", "-m", "eval baseline"]);
    for args in commands {
        ensure_success(
            run_process(
                "git",
                &args
                    .iter()
                    .map(|value| value.to_string())
                    .collect::<Vec<_>>(),
                Some(workspace),
                None,
                Duration::from_secs(30),
                &[],
            )
            .await?,
            "initialize eval git repository",
        )?;
    }
    Ok(())
}

async fn git_diff(workspace: &Path) -> AppResult<(Vec<String>, u64, u64, String)> {
    ensure_success(
        run_process(
            "git",
            &["add".into(), "--intent-to-add".into(), ".".into()],
            Some(workspace),
            None,
            Duration::from_secs(30),
            &[],
        )
        .await?,
        "include untracked eval files in diff",
    )?;
    let patch = run_process(
        "git",
        &["diff".into(), "--binary".into(), "HEAD".into()],
        Some(workspace),
        None,
        Duration::from_secs(30),
        &[],
    )
    .await?;
    let stat = run_process(
        "git",
        &["diff".into(), "--numstat".into(), "HEAD".into()],
        Some(workspace),
        None,
        Duration::from_secs(30),
        &[],
    )
    .await?;
    let mut files = Vec::new();
    let mut added = 0;
    let mut removed = 0;
    for line in stat.stdout.lines() {
        let columns = line.splitn(3, '\t').collect::<Vec<_>>();
        if columns.len() == 3 {
            added += columns[0].parse::<u64>().unwrap_or(0);
            removed += columns[1].parse::<u64>().unwrap_or(0);
            files.push(columns[2].to_string());
        }
    }
    Ok((files, added, removed, patch.stdout))
}

enum EvalIsolation {
    Docker(DockerIsolation),
    Windows(WindowsNativeIsolation),
    WindowsSandbox(Box<WindowsSandboxIsolation>),
}

/// A fresh CODEX_HOME treats an unknown Windows workspace as untrusted and
/// silently narrows `-s workspace-write` to read-only. Trust only the
/// evaluator-owned, per-case workspace; the sandbox still limits writes to
/// that directory. Without this entry the benchmark measures onboarding
/// policy rather than the model or Vellum harness.
fn write_windows_eval_trust(codex_home: &Path, workspace: &Path) -> AppResult<()> {
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let mut document = DocumentMut::new();
    document["projects"][workspace.as_str()]["trust_level"] = toml_value("trusted");
    std::fs::write(codex_home.join("config.toml"), document.to_string())
        .map_err(io_error("write isolated Windows eval trust config"))
}

impl EvalIsolation {
    async fn start(
        executor: &str,
        run_id: &str,
        gateway_port: u16,
        codex_version: &str,
    ) -> AppResult<Self> {
        match executor {
            "docker" => Ok(Self::Docker(
                DockerIsolation::start(run_id, gateway_port, codex_version).await?,
            )),
            "windows" => Ok(Self::Windows(
                WindowsNativeIsolation::start(run_id, gateway_port, codex_version).await?,
            )),
            "windows-sandbox" => Ok(Self::WindowsSandbox(Box::new(
                WindowsSandboxIsolation::start(run_id, gateway_port, codex_version).await?,
            ))),
            other => Err(AppError::Message(format!(
                "unsupported eval executor: {other}"
            ))),
        }
    }

    fn shell_contract(&self) -> &'static str {
        match self {
            Self::Docker(_) => "linux-bash",
            Self::Windows(_) => windows_shell_contract(),
            Self::WindowsSandbox(_) => "windows-powershell",
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_codex(
        &self,
        workspace: &Path,
        codex_home: &Path,
        catalog: &Path,
        model: &str,
        resume: Option<&str>,
        prompt: &str,
        token: &str,
        timeout: Duration,
        limits: &super::manifest::TaskLimits,
        forced_context_window: Option<u64>,
        forced_auto_compact_token_limit: Option<u64>,
        reasoning_effort: Option<&str>,
        compaction_engine: &str,
        codex_executable: Option<&Path>,
    ) -> AppResult<CommandOutput> {
        match self {
            Self::Docker(isolation) => {
                isolation
                    .run_codex(
                        workspace,
                        codex_home,
                        catalog,
                        model,
                        resume,
                        prompt,
                        token,
                        timeout,
                        limits,
                        forced_context_window,
                        forced_auto_compact_token_limit,
                        reasoning_effort,
                        compaction_engine,
                        codex_executable,
                    )
                    .await
            }
            Self::Windows(isolation) => {
                isolation
                    .run_codex(
                        workspace,
                        codex_home,
                        catalog,
                        model,
                        resume,
                        prompt,
                        token,
                        timeout,
                        limits,
                        forced_context_window,
                        forced_auto_compact_token_limit,
                        reasoning_effort,
                        compaction_engine,
                        codex_executable,
                    )
                    .await
            }
            Self::WindowsSandbox(isolation) => {
                isolation
                    .run_codex(
                        workspace,
                        codex_home,
                        catalog,
                        model,
                        resume,
                        prompt,
                        token,
                        timeout,
                        limits,
                        forced_context_window,
                        forced_auto_compact_token_limit,
                        reasoning_effort,
                        compaction_engine,
                        codex_executable,
                    )
                    .await
            }
        }
    }

    async fn run_shell(
        &self,
        workspace: &Path,
        codex_home: &Path,
        command: &str,
        timeout: Duration,
        limits: &super::manifest::TaskLimits,
    ) -> AppResult<CommandOutput> {
        match self {
            Self::Docker(isolation) => {
                isolation
                    .run_shell(workspace, codex_home, command, timeout, limits)
                    .await
            }
            Self::Windows(isolation) => {
                isolation
                    .support
                    .run_shell(workspace, codex_home, command, timeout, limits)
                    .await
            }
            Self::WindowsSandbox(isolation) => {
                isolation
                    .support
                    .run_shell(workspace, codex_home, command, timeout, limits)
                    .await
            }
        }
    }

    async fn run_verification(
        &self,
        workspace: &Path,
        hidden: &Path,
        commands: &[String],
        timeout: Duration,
        limits: &super::manifest::TaskLimits,
        grader_image: Option<&str>,
    ) -> AppResult<CommandOutput> {
        match self {
            Self::Docker(isolation) => {
                isolation
                    .run_verification(workspace, hidden, commands, timeout, limits, grader_image)
                    .await
            }
            Self::Windows(isolation) => {
                isolation
                    .support
                    .run_verification(workspace, hidden, commands, timeout, limits, grader_image)
                    .await
            }
            Self::WindowsSandbox(isolation) => {
                isolation
                    .support
                    .run_verification(workspace, hidden, commands, timeout, limits, grader_image)
                    .await
            }
        }
    }

    async fn shutdown(self) {
        match self {
            Self::Docker(isolation) => isolation.shutdown().await,
            Self::Windows(isolation) => isolation.support.shutdown().await,
            Self::WindowsSandbox(isolation) => {
                let isolation = *isolation;
                isolation.support.shutdown().await;
                let _ = std::fs::remove_dir_all(&isolation.session_marker_root);
            }
        }
    }
}

struct WindowsNativeIsolation {
    gateway_port: u16,
    support: DockerIsolation,
}

impl WindowsNativeIsolation {
    async fn start(run_id: &str, gateway_port: u16, codex_version: &str) -> AppResult<Self> {
        if !cfg!(target_os = "windows") {
            return Err(AppError::Message(
                "the Windows-native eval executor is only available on Windows".into(),
            ));
        }
        let version = ensure_success(
            run_process(
                "codex",
                &["--version".into()],
                None,
                None,
                Duration::from_secs(15),
                &[],
            )
            .await?,
            "inspect Windows Codex CLI",
        )?
        .stdout;
        if !version.contains(codex_version) {
            return Err(AppError::Message(format!(
                "Windows Codex runtime mismatch: expected {codex_version}, observed {}",
                version.trim()
            )));
        }
        let permission = windows_workspace_write_check().await;
        if !permission.ok {
            return Err(AppError::Message(format!(
                "Windows Codex executor cannot qualify writable agent tasks: {}. Use the Docker executor for a synthetic capability run; do not treat it as Desktop qualification.",
                permission.detail
            )));
        }
        // Reuse the Docker support lane only for deterministic setup and the
        // network-disabled hidden grader. The model-facing Codex process below
        // is the installed Windows runtime.
        let support = DockerIsolation::start(run_id, gateway_port, codex_version).await?;
        Ok(Self {
            gateway_port,
            support,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_codex(
        &self,
        workspace: &Path,
        codex_home: &Path,
        catalog: &Path,
        model: &str,
        resume: Option<&str>,
        prompt: &str,
        token: &str,
        timeout: Duration,
        _limits: &super::manifest::TaskLimits,
        forced_context_window: Option<u64>,
        forced_auto_compact_token_limit: Option<u64>,
        reasoning_effort: Option<&str>,
        compaction_engine: &str,
        codex_executable: Option<&Path>,
    ) -> AppResult<CommandOutput> {
        let mut args = vec!["exec".into()];
        args.extend([
            "-m".into(),
            model.into(),
            "-C".into(),
            workspace.to_string_lossy().into_owned(),
            "-s".into(),
            "workspace-write".into(),
            "--strict-config".into(),
            "--json".into(),
            "--disable".into(),
            "apps".into(),
            "--disable".into(),
            "plugins".into(),
            "--disable".into(),
            "remote_plugin".into(),
            "-c".into(),
            "approval_policy=\"never\"".into(),
            "-c".into(),
            "model_provider=\"vellum_eval\"".into(),
            "-c".into(),
            format!(
                "model_providers.vellum_eval.name=\"{}\"",
                eval_codex_provider_display_name(compaction_engine)
            ),
            "-c".into(),
            format!(
                "model_providers.vellum_eval.base_url=\"http://127.0.0.1:{}/v1\"",
                self.gateway_port
            ),
            "-c".into(),
            "model_providers.vellum_eval.env_key=\"OPENAI_API_KEY\"".into(),
            "-c".into(),
            "model_providers.vellum_eval.wire_api=\"responses\"".into(),
            "-c".into(),
            format!(
                "model_catalog_json={}",
                serde_json::to_string(&catalog.to_string_lossy()).unwrap_or_else(|_| "\"\"".into())
            ),
            "-c".into(),
            format!(
                "features.remote_compaction_v2={}",
                eval_remote_compaction_v2_enabled(compaction_engine)
            ),
            "-c".into(),
            "tools.update_plan.enabled=true".into(),
        ]);
        append_forced_compaction_config(
            &mut args,
            forced_context_window,
            forced_auto_compact_token_limit,
        );
        append_reasoning_effort_config(&mut args, reasoning_effort);
        append_exec_prompt_args(&mut args, resume);
        let codex_home_value = codex_home.to_string_lossy().into_owned();
        let enhanced_event_log = codex_home.join("enhanced-events.jsonl");
        let _ = std::fs::remove_file(&enhanced_event_log);
        let enhanced_event_log_value = enhanced_event_log.to_string_lossy().into_owned();
        let enhanced_config = std::fs::read(codex_home.join("enhanced-runtime.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let runtime_digest = enhanced_config
            .as_ref()
            .and_then(|value| value.get("runtimeDigest"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let enhanced_commit = enhanced_config
            .as_ref()
            .and_then(|value| value.get("enhancedCodexCommit"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut env = vec![
            ("CODEX_HOME", codex_home_value.as_str()),
            ("OPENAI_API_KEY", token),
            ("PYTHONIOENCODING", "utf-8"),
            ("PYTHONUTF8", "1"),
            (
                "VELLUM_ENHANCED_EVENT_LOG",
                enhanced_event_log_value.as_str(),
            ),
        ];
        if codex_executable.is_some() {
            env.extend([
                ("VELLUM_EXECUTION_PLANE", "enhanced-codex"),
                ("VELLUM_RUNTIME_DIGEST", runtime_digest.as_str()),
                ("VELLUM_ENHANCED_COMMIT", enhanced_commit.as_str()),
            ]);
        }
        let program = eval_codex_program(codex_executable);
        let mut output = run_process(
            &program,
            &args,
            Some(workspace),
            Some(prompt.as_bytes()),
            timeout,
            &env,
        )
        .await?;
        append_jsonl_file(&mut output.stdout, &enhanced_event_log);
        Ok(output)
    }
}

fn prompt_reports_workspace_write(output: &str) -> bool {
    output.contains("`sandbox_mode` is `workspace-write`")
        || output.contains("sandbox_mode is workspace-write")
        || output.contains("\"sandbox_mode\":\"workspace-write\"")
}

pub(super) async fn windows_workspace_write_check() -> DoctorCheck {
    if !cfg!(target_os = "windows") {
        return DoctorCheck {
            name: "Windows Codex workspace policy".into(),
            ok: false,
            detail: "the Windows executor is unavailable on this platform".into(),
        };
    }
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => {
            return DoctorCheck {
                name: "Windows Codex workspace policy".into(),
                ok: false,
                detail: format!("cannot create isolated policy probe: {error}"),
            };
        }
    };
    let home = temp.path().join("codex-home");
    let workspace = temp.path().join("workspace");
    if let Err(error) = std::fs::create_dir_all(&home)
        .and_then(|_| std::fs::create_dir_all(&workspace))
        .and_then(|_| std::fs::write(home.join("config.toml"), ""))
    {
        return DoctorCheck {
            name: "Windows Codex workspace policy".into(),
            ok: false,
            detail: format!("cannot prepare isolated policy probe: {error}"),
        };
    }
    let home_value = home.to_string_lossy().into_owned();
    let output = run_process(
        "codex",
        &[
            "-C".into(),
            workspace.to_string_lossy().into_owned(),
            "-s".into(),
            "workspace-write".into(),
            "debug".into(),
            "prompt-input".into(),
            "Vellum evaluator permission preflight".into(),
        ],
        Some(&workspace),
        None,
        Duration::from_secs(20),
        &[("CODEX_HOME", home_value.as_str())],
    )
    .await;
    match output {
        Ok(output) if output.code == Some(0) => {
            let writable = prompt_reports_workspace_write(&output.stdout);
            DoctorCheck {
                name: "Windows Codex workspace policy".into(),
                ok: writable,
                detail: if writable {
                    "effective prompt grants workspace-write".into()
                } else if output.stdout.contains("`sandbox_mode` is `read-only`") {
                    "requested workspace-write was narrowed to read-only by the effective Codex permission profile".into()
                } else {
                    "effective prompt did not confirm workspace-write".into()
                },
            }
        }
        Ok(output) => DoctorCheck {
            name: "Windows Codex workspace policy".into(),
            ok: false,
            detail: format!(
                "Codex policy probe failed with {:?}: {}{}",
                output.code, output.stdout, output.stderr
            ),
        },
        Err(error) => DoctorCheck {
            name: "Windows Codex workspace policy".into(),
            ok: false,
            detail: error.to_string(),
        },
    }
}

struct WindowsSandboxIsolation {
    gateway_port: u16,
    session_marker_root: PathBuf,
    runtime_root: PathBuf,
    sandbox_cli: PathBuf,
    python_root: Option<PathBuf>,
    git_root: Option<PathBuf>,
    launch_gate: tokio::sync::Mutex<Option<Instant>>,
    launch_lock_path: PathBuf,
    support: DockerIsolation,
}

/// Windows Sandbox uses one host-compute backend. Serializing launches across
/// evaluator processes prevents two runs from racing session discovery or
/// destabilizing the backend while one VM is still shutting down.
struct WindowsSandboxLaunchGuard {
    file: File,
}

impl Drop for WindowsSandboxLaunchGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

async fn acquire_windows_sandbox_launch_lock(path: &Path) -> AppResult<WindowsSandboxLaunchGuard> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(io_error("create Windows Sandbox lock directory"))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(io_error("open Windows Sandbox launch lock"))?;
        file.lock_exclusive()
            .map_err(io_error("acquire Windows Sandbox launch lock"))?;
        Ok(WindowsSandboxLaunchGuard { file })
    })
    .await
    .map_err(|error| AppError::Message(format!("Windows Sandbox lock worker failed: {error}")))?
}

/// Ensures an evaluator-owned Windows Sandbox is torn down when the async
/// turn is cancelled before `run_codex` reaches its normal cleanup path.
/// Process-wide termination cannot run Rust destructors, so the next
/// evaluator startup also performs a scoped stale-session sweep.
struct WindowsSandboxSessionGuard {
    config_path: PathBuf,
    session_marker_root: PathBuf,
    armed: bool,
}

impl WindowsSandboxSessionGuard {
    fn new(config_path: PathBuf, session_marker_root: PathBuf) -> Self {
        Self {
            config_path,
            session_marker_root,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for WindowsSandboxSessionGuard {
    fn drop(&mut self) {
        if !self.armed || !cfg!(target_os = "windows") {
            return;
        }
        let script =
            windows_sandbox_session_cleanup_script(&self.config_path, &self.session_marker_root);
        let _ = crate::process::background_command("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &script,
            ])
            .spawn();
    }
}

impl WindowsSandboxIsolation {
    async fn start(run_id: &str, gateway_port: u16, codex_version: &str) -> AppResult<Self> {
        if !cfg!(target_os = "windows") {
            return Err(AppError::Message(
                "the Windows Sandbox executor is only available on Windows".into(),
            ));
        }
        // `wsb.exe` is distributed as a per-user WindowsApps alias rather
        // than under System32 on current Windows Sandbox releases.
        let sandbox_cli = PathBuf::from("wsb.exe");
        let paths = EvalPaths::discover(None)?;
        cleanup_stale_eval_windows_sandboxes(&paths.output_root).await;
        let session_marker_root = windows_sandbox_marker_root(&paths.output_root, run_id);
        std::fs::create_dir_all(&session_marker_root)
            .map_err(io_error("create Windows Sandbox session marker directory"))?;
        std::fs::write(
            session_marker_root.join("owner.pid"),
            format!("{}\n", std::process::id()),
        )
        .map_err(io_error("write Windows Sandbox session owner"))?;
        let runtime_root = prepare_windows_runtime(&paths, codex_version).await?;
        let python_root = executable_root("python", 0).await;
        let git_root = executable_root("git", 1).await;
        let support = DockerIsolation::start(run_id, gateway_port, codex_version).await?;
        Ok(Self {
            gateway_port,
            session_marker_root,
            runtime_root,
            sandbox_cli,
            python_root,
            git_root,
            launch_gate: tokio::sync::Mutex::new(None),
            launch_lock_path: windows_sandbox_marker_base(&paths.output_root).join("launch.lock"),
            support,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_codex(
        &self,
        workspace: &Path,
        codex_home: &Path,
        catalog: &Path,
        model: &str,
        resume: Option<&str>,
        prompt: &str,
        token: &str,
        timeout: Duration,
        _limits: &super::manifest::TaskLimits,
        forced_context_window: Option<u64>,
        forced_auto_compact_token_limit: Option<u64>,
        reasoning_effort: Option<&str>,
        compaction_engine: &str,
        codex_executable: Option<&Path>,
    ) -> AppResult<CommandOutput> {
        // Windows Sandbox needs a short host-compute teardown interval after
        // its client exits. Without this gate, an immediately following VM
        // may return exit 0 without booting, which looks like a model-specific
        // 0-token timeout under round-robin scheduling.
        let mut launch_gate = self.launch_gate.lock().await;
        if let Some(previous_exit) = *launch_gate {
            let cooldown = Duration::from_secs(8);
            let elapsed = previous_exit.elapsed();
            if elapsed < cooldown {
                tokio::time::sleep(cooldown - elapsed).await;
            }
        }
        let _cross_process_launch_guard =
            acquire_windows_sandbox_launch_lock(&self.launch_lock_path).await?;
        let case_root = workspace.parent().ok_or_else(|| {
            AppError::Message("Windows Sandbox case has no parent directory".into())
        })?;
        let current = case_root.join(".vellum-current");
        if current.exists() {
            std::fs::remove_dir_all(&current)
                .map_err(io_error("replace Windows Sandbox invocation"))?;
        }
        std::fs::create_dir_all(&current).map_err(io_error("create Windows Sandbox invocation"))?;
        std::fs::write(current.join("prompt.txt"), prompt)
            .map_err(io_error("write Windows Sandbox prompt"))?;
        std::fs::copy(catalog, current.join("model-catalog.json"))
            .map_err(io_error("stage Windows Sandbox model catalog"))?;
        // The artifact goes in exactly as it was verified. The digest the VM
        // re-checks is therefore the pinned artifact's own, not a copy's.
        let enhanced_codex_in_sandbox = if let Some(executable) = codex_executable {
            let dest = current.join("enhanced-codex.exe");
            std::fs::copy(executable, &dest).map_err(io_error(
                "copy verified Enhanced Codex into Windows Sandbox mapping",
            ))?;
            Some(sha256_file(&dest)?)
        } else {
            None
        };
        let rollout_offsets = snapshot_rollout_offsets(codex_home);

        let mut arguments = vec!["'exec'".to_string()];
        arguments.extend([
            "'-m'".into(),
            ps_single_quoted(model),
            "'-C'".into(),
            "'C:\\VellumEval\\workspace'".into(),
            "'--dangerously-bypass-approvals-and-sandbox'".into(),
            "'--skip-git-repo-check'".into(),
            "'--ignore-rules'".into(),
            "'--strict-config'".into(),
            "'--json'".into(),
            "'--disable'".into(),
            "'apps'".into(),
            "'--disable'".into(),
            "'plugins'".into(),
            "'--disable'".into(),
            "'remote_plugin'".into(),
            "'-c'".into(),
            "'model_provider=\"vellum_eval\"'".into(),
            "'-c'".into(),
            ps_single_quoted(&format!(
                "model_providers.vellum_eval.name=\"{}\"",
                eval_codex_provider_display_name(compaction_engine)
            )),
            "'-c'".into(),
            "$baseUrlConfig".into(),
            "'-c'".into(),
            "'model_providers.vellum_eval.env_key=\"OPENAI_API_KEY\"'".into(),
            "'-c'".into(),
            "'model_providers.vellum_eval.wire_api=\"responses\"'".into(),
            "'-c'".into(),
            "'model_catalog_json=\"C:/VellumEval/.vellum-current/model-catalog.json\"'".into(),
            "'-c'".into(),
            ps_single_quoted(&format!(
                "features.remote_compaction_v2={}",
                eval_remote_compaction_v2_enabled(compaction_engine)
            )),
            "'-c'".into(),
            "'tools.update_plan.enabled=true'".into(),
        ]);
        if let Some(window) = forced_context_window {
            let compact_limit =
                forced_auto_compact_token_limit.unwrap_or_else(|| window.saturating_mul(2) / 3);
            arguments.extend([
                "'-c'".into(),
                ps_single_quoted(&format!("model_context_window={window}")),
                "'-c'".into(),
                ps_single_quoted(&format!("model_auto_compact_token_limit={}", compact_limit)),
            ]);
        }
        if let Some(effort) = reasoning_effort {
            arguments.extend([
                "'-c'".into(),
                ps_single_quoted(&format!("model_reasoning_effort=\"{effort}\"")),
            ]);
        }
        append_powershell_exec_prompt_args(&mut arguments, resume);
        let script = format!(
            r#"$ErrorActionPreference = 'Stop'
$utf8 = New-Object System.Text.UTF8Encoding($false)
# Everything below runs as the sandbox logon command. When it throws, the
# session ends immediately, so an exception raised before Codex starts used to
# leave a closed sandbox and three empty files: no stdout, no result, and a
# `stderr.txt` this script had hardcoded to the empty string. The failure was
# indistinguishable from "the model said nothing". Record it instead.
try {{
$hostIp = (Get-NetRoute -DestinationPrefix '0.0.0.0/0' | Sort-Object RouteMetric | Select-Object -First 1).NextHop
if (-not $hostIp) {{ throw 'cannot resolve Windows Sandbox host gateway' }}
Set-NetFirewallProfile -Profile Domain,Public,Private -DefaultOutboundAction Block
New-NetFirewallRule -DisplayName 'Vellum Eval Gateway' -Direction Outbound -Action Allow -RemoteAddress $hostIp -Protocol TCP -RemotePort {port} | Out-Null
$env:OPENAI_API_KEY = {token}
$env:CODEX_HOME = 'C:\VellumEval\codex-home'
$env:VELLUM_ENHANCED_EVENT_LOG = 'C:\VellumEval\.vellum-current\enhanced-events.jsonl'
$enhancedConfigPath = 'C:\VellumEval\codex-home\enhanced-runtime.json'
if (Test-Path $enhancedConfigPath) {{
  $enhancedConfig = Get-Content -Raw $enhancedConfigPath | ConvertFrom-Json
  $env:VELLUM_EXECUTION_PLANE = 'enhanced-codex'
  $env:VELLUM_RUNTIME_DIGEST = $enhancedConfig.runtimeDigest
  $env:VELLUM_ENHANCED_COMMIT = $enhancedConfig.enhancedCodexCommit
}}
$env:PYTHONIOENCODING = 'utf-8'
$env:PYTHONUTF8 = '1'
$toolPaths = @('C:\CodexRuntime\node_modules\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\codex-resources')
if (Test-Path 'C:\Python') {{ $toolPaths += @('C:\Python', 'C:\Python\Scripts') }}
if (Test-Path 'C:\Git') {{ $toolPaths += @('C:\Git\cmd', 'C:\Git\bin') }}
$env:Path = ($toolPaths -join ';') + ';' + $env:Path
{codex_assignment}
$childScript = @'
$ErrorActionPreference = 'Continue'
$codex = '__VELLUM_CODEX__'
$baseUrlConfig = 'model_providers.vellum_eval.base_url="http://__VELLUM_HOST_IP__:{port}/v1"'
$arguments = @(
  {arguments}
)
$prompt = [IO.File]::ReadAllText('C:\VellumEval\.vellum-current\prompt.txt')
$prompt | & $codex @arguments
exit $LASTEXITCODE
'@
$childScript = $childScript.Replace('__VELLUM_HOST_IP__', $hostIp).Replace('__VELLUM_CODEX__', $codex)
$encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($childScript))
$stdoutPath = 'C:\VellumEval\.vellum-current\stdout.jsonl'
$stderrPath = 'C:\VellumEval\.vellum-current\stderr.txt'
$process = Start-Process powershell.exe -ArgumentList @('-NoLogo','-NoProfile','-NonInteractive','-EncodedCommand',$encoded) -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath -WindowStyle Hidden -PassThru
$finished = $process.WaitForExit({timeout_millis})
$timedOut = -not $finished
if ($timedOut) {{
  & taskkill.exe /PID $process.Id /T /F 2>$null | Out-Null
  $process.WaitForExit()
}}
$exitCode = if ($timedOut) {{ -1 }} else {{ $process.ExitCode }}
# An unsigned runtime is refused by code integrity with a message that names
# neither the policy nor the rule. Both are recorded, and nowhere else, so on
# a failed launch they belong beside the child's own stderr.
if ($exitCode -ne 0 -and (Test-Path 'C:\VellumEval\.vellum-current\policy.log')) {{
  $policyText = @('code integrity policy inside the sandbox:')
  $policyText += @(Get-Content 'C:\VellumEval\.vellum-current\policy.log' | ForEach-Object {{ "  $_" }})
  try {{
    $ci = Get-WinEvent -LogName 'Microsoft-Windows-CodeIntegrity/Operational' -MaxEvents 20 -ErrorAction Stop
    $policyText += 'code integrity log:'
    $policyText += @($ci | ForEach-Object {{ "  [$($_.Id)] " + ($_.Message -replace "`r?`n", ' ') }})
  }} catch {{ $policyText += "code integrity log: $($_.Exception.Message)" }}
  [IO.File]::AppendAllText($stderrPath, ($policyText -join "`n") + "`n", $utf8)
}}
[IO.File]::WriteAllText('C:\VellumEval\.vellum-current\result.json', (@{{exitCode=$exitCode;timedOut=$timedOut}} | ConvertTo-Json -Compress), $utf8)
}} catch {{
  $detail = ($_ | Out-String) + "`n" + ($_.ScriptStackTrace | Out-String)
  [IO.File]::WriteAllText('C:\VellumEval\.vellum-current\stderr.txt', $detail, $utf8)
  [IO.File]::WriteAllText('C:\VellumEval\.vellum-current\result.json', (@{{exitCode=125;timedOut=$false;bootstrapFailed=$true}} | ConvertTo-Json -Compress), $utf8)
}}
shutdown.exe /s /t 0 /f
"#,
            port = self.gateway_port,
            token = ps_single_quoted(token),
            arguments = arguments.join(",\n  "),
            timeout_millis = timeout.as_millis().max(1),
            codex_assignment =
                windows_sandbox_codex_assignment(enhanced_codex_in_sandbox.as_deref()),
        );
        std::fs::write(current.join("run.ps1"), script)
            .map_err(io_error("write Windows Sandbox bootstrap"))?;
        // Keep the host mapping manifest outside every guest-visible mount.
        // In particular, never expose case_root: it also contains hidden/
        // and the post-agent verification workspace.
        let config_path = case_root.join(".vellum-sandbox.wsb");
        let config_xml = self.config_xml(
            workspace,
            codex_home,
            &current,
            enhanced_codex_in_sandbox.is_some(),
        )?;
        std::fs::write(&config_path, &config_xml)
            .map_err(io_error("write Windows Sandbox config"))?;
        let mut session_guard =
            WindowsSandboxSessionGuard::new(config_path.clone(), self.session_marker_root.clone());
        // Use the headless Windows Sandbox CLI instead of WindowsSandbox.exe.
        // The GUI launcher creates a RemoteSession process whose localhost
        // gRPC hand-off can remain wedged after a previous VM exits. `wsb
        // start` returns the exact environment id, so ownership and teardown
        // are deterministic and do not depend on process/session discovery.
        let (_, sandbox_id) =
            start_windows_sandbox_cli(&self.sandbox_cli, &config_xml, &self.session_marker_root)
                .await?;
        let launched = execute_windows_sandbox_cli(
            &self.sandbox_cli,
            &sandbox_id,
            r"powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File C:\VellumEval\.vellum-current\run.ps1",
            timeout.saturating_add(Duration::from_secs(90)),
        )
        .await?;
        let result_path = current.join("result.json");
        if let Some(visibility_budget) =
            windows_sandbox_result_visibility_budget(launched.timed_out, result_path.is_file())
        {
            // The guest writes result.json before exiting. Allow only a short
            // mapped-folder visibility delay after a successful `wsb exec`;
            // never add a second task-sized wait after the exec deadline.
            let deadline = Instant::now() + visibility_budget;
            while Instant::now() < deadline && !result_path.is_file() {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
        let result = std::fs::read_to_string(&result_path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        let mut stdout = read_text_file(&current.join("stdout.jsonl"));
        let rollout_delta = read_rollout_delta(codex_home, &rollout_offsets);
        if stdout.trim().is_empty() {
            stdout = rollout_delta;
        } else {
            // `codex exec --json` does not currently expose the completed
            // local-history rewrite on stdout, even though the durable
            // rollout records it as `type:"compacted"`. Append only those
            // terminal compaction events. Appending the whole rollout would
            // double-count tools and usage already present on stdout.
            let completed_compactions = completed_compaction_events(&rollout_delta);
            if !completed_compactions.is_empty() {
                if !stdout.ends_with('\n') {
                    stdout.push('\n');
                }
                stdout.push_str(&completed_compactions);
            }
        }
        let stderr = read_text_file(&current.join("stderr.txt"));
        append_jsonl_file(&mut stdout, &current.join("enhanced-events.jsonl"));
        let code = result
            .as_ref()
            .and_then(|value| value.get("exitCode"))
            .and_then(Value::as_i64)
            .map(|value| value as i32)
            .or(launched.code);
        let timed_out = launched.timed_out
            || result
                .as_ref()
                .and_then(|value| value.get("timedOut"))
                .and_then(Value::as_bool)
                .unwrap_or(result.is_none());
        terminate_windows_sandbox_session(&config_path, &self.session_marker_root).await;
        session_guard.disarm();
        *launch_gate = Some(Instant::now());
        let _ = std::fs::remove_dir_all(&current);
        let _ = std::fs::remove_file(&config_path);
        Ok(CommandOutput {
            code,
            stdout,
            stderr: if stderr.is_empty() {
                launched.stderr
            } else {
                stderr
            },
            timed_out,
            first_byte_ms: launched.first_byte_ms,
        })
    }

    /// `ProtectedClient` raises the sandbox's application-control posture,
    /// and an ablation run turns it off: the pinned Codex runtime is a signed
    /// release and runs either way, while an Enhanced Codex built from source
    /// on this machine is refused outright.
    ///
    /// It is not the only thing in the way -- Smart App Control refuses the
    /// same binary independently, and the bootstrap takes that policy offline
    /// inside the VM. `windows_sandbox_codex_assignment` carries the evidence
    /// for why signing cannot substitute for either.
    ///
    /// Nothing else relaxes: outbound traffic is still blocked except to the
    /// eval gateway, and the mapped folders are unchanged -- in particular
    /// the case root, which holds the hidden graders, stays unmapped.
    fn config_xml(
        &self,
        workspace: &Path,
        codex_home: &Path,
        invocation: &Path,
        unsigned_runtime: bool,
    ) -> AppResult<String> {
        let mut mappings = windows_sandbox_model_mappings(workspace, codex_home, invocation);
        mappings.push(wsb_mapping(&self.runtime_root, "C:\\CodexRuntime", true));
        if let Some(root) = &self.python_root {
            mappings.push(wsb_mapping(root, "C:\\Python", true));
        }
        if let Some(root) = &self.git_root {
            mappings.push(wsb_mapping(root, "C:\\Git", true));
        }
        Ok(format!(
            "<Configuration><VGpu>Disable</VGpu><Networking>Enable</Networking><AudioInput>Disable</AudioInput><VideoInput>Disable</VideoInput><ProtectedClient>{}</ProtectedClient><PrinterRedirection>Disable</PrinterRedirection><ClipboardRedirection>Disable</ClipboardRedirection><MemoryInMB>4096</MemoryInMB><MappedFolders>{}</MappedFolders></Configuration>",
            if unsigned_runtime { "Disable" } else { "Enable" },
            mappings.join("")
        ))
    }
}

fn append_jsonl_file(target: &mut String, path: &Path) {
    let extra = read_text_file(path);
    if extra.trim().is_empty() {
        return;
    }
    if !target.is_empty() && !target.ends_with('\n') {
        target.push('\n');
    }
    target.push_str(&extra);
    if !target.ends_with('\n') {
        target.push('\n');
    }
}

fn windows_sandbox_model_mappings(
    workspace: &Path,
    codex_home: &Path,
    invocation: &Path,
) -> Vec<String> {
    vec![
        wsb_mapping(workspace, "C:\\VellumEval\\workspace", false),
        wsb_mapping(codex_home, "C:\\VellumEval\\codex-home", false),
        wsb_mapping(invocation, "C:\\VellumEval\\.vellum-current", false),
    ]
}

/// Starts a disposable Windows Sandbox without contacting a model Provider.
///
/// The probe proves that the mapped workspace is writable and that the pinned
/// Codex runtime can execute inside the VM. It deliberately performs no Vellum
/// gateway request, so it is safe to run before every live evaluation.
pub async fn windows_sandbox_preflight(paths: &EvalPaths, codex_version: &str) -> DoctorCheck {
    if !cfg!(target_os = "windows") {
        return DoctorCheck {
            name: "Windows Sandbox executor".into(),
            ok: false,
            detail: "available only on Windows".into(),
        };
    }
    let sandbox_cli = PathBuf::from("wsb.exe");
    cleanup_stale_eval_windows_sandboxes(&paths.output_root).await;
    let preflight_owner = format!(
        "preflight-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    );
    let session_marker_root = windows_sandbox_marker_root(&paths.output_root, &preflight_owner);
    if let Err(error) = std::fs::create_dir_all(&session_marker_root) {
        return DoctorCheck {
            name: "Windows Sandbox executor".into(),
            ok: false,
            detail: format!("cannot create session marker directory: {error}"),
        };
    }
    if let Err(error) = std::fs::write(
        session_marker_root.join("owner.pid"),
        format!("{}\n", std::process::id()),
    ) {
        return DoctorCheck {
            name: "Windows Sandbox executor".into(),
            ok: false,
            detail: format!("cannot write session owner: {error}"),
        };
    }
    let runtime_root = match prepare_windows_runtime(paths, codex_version).await {
        Ok(path) => path,
        Err(error) => {
            return DoctorCheck {
                name: "Windows Sandbox executor".into(),
                ok: false,
                detail: error.to_string(),
            };
        }
    };
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    );
    let root = paths.output_root.join("sandbox-preflight").join(nonce);
    let workspace = root.join("workspace");
    let codex_home = root.join("codex-home");
    let invocation = root.join(".vellum-current");
    let hidden = root.join("hidden");
    if let Err(error) = std::fs::create_dir_all(&workspace)
        .and_then(|_| std::fs::create_dir_all(&codex_home))
        .and_then(|_| std::fs::create_dir_all(&invocation))
        .and_then(|_| std::fs::create_dir_all(&hidden))
        .and_then(|_| std::fs::write(hidden.join("grader-canary.txt"), "must-not-be-visible"))
    {
        return DoctorCheck {
            name: "Windows Sandbox executor".into(),
            ok: false,
            detail: format!("cannot create preflight directory: {error}"),
        };
    }
    let script = r#"$ErrorActionPreference = 'Stop'
$utf8 = New-Object System.Text.UTF8Encoding($false)
try {
  $probe = 'C:\VellumEval\workspace\workspace-write.txt'
  [IO.File]::WriteAllText($probe, 'workspace-write-ok', $utf8)
  $hiddenVisible = Test-Path 'C:\VellumEval\hidden'
  $canaryVisible = @(Get-ChildItem -Path 'C:\VellumEval' -Recurse -Force -File -ErrorAction SilentlyContinue | Where-Object Name -eq 'grader-canary.txt').Count -gt 0
  if ($hiddenVisible -or $canaryVisible) { throw 'hidden grader material is visible in the model sandbox' }
  $codex = 'C:\CodexRuntime\node_modules\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\bin\codex.exe'
  if (-not (Test-Path $codex)) { throw 'pinned Codex executable is not visible in the sandbox' }
  $version = (& $codex --version 2>&1 | Out-String).Trim()
  if ($LASTEXITCODE -ne 0) { throw "Codex runtime exited with $LASTEXITCODE" }
  [IO.File]::WriteAllText('C:\VellumEval\.vellum-current\result.json', (@{ok=$true;version=$version;workspaceWrite=(Test-Path $probe);hiddenAbsent=(-not $hiddenVisible -and -not $canaryVisible)} | ConvertTo-Json -Compress), $utf8)
} catch {
  [IO.File]::WriteAllText('C:\VellumEval\.vellum-current\result.json', (@{ok=$false;error=$_.Exception.Message} | ConvertTo-Json -Compress), $utf8)
} finally {
  shutdown.exe /s /t 0 /f
}
"#;
    let script_path = invocation.join("preflight.ps1");
    let config_path = root.join("preflight.wsb");
    let mut mappings = windows_sandbox_model_mappings(&workspace, &codex_home, &invocation);
    mappings.push(wsb_mapping(&runtime_root, "C:\\CodexRuntime", true));
    let config = format!(
        "<Configuration><VGpu>Disable</VGpu><Networking>Disable</Networking><AudioInput>Disable</AudioInput><VideoInput>Disable</VideoInput><ProtectedClient>Enable</ProtectedClient><PrinterRedirection>Disable</PrinterRedirection><ClipboardRedirection>Disable</ClipboardRedirection><MemoryInMB>2048</MemoryInMB><MappedFolders>{}</MappedFolders></Configuration>",
        mappings.join("")
    );
    if let Err(error) =
        std::fs::write(&script_path, script).and_then(|_| std::fs::write(&config_path, &config))
    {
        let _ = std::fs::remove_dir_all(&root);
        return DoctorCheck {
            name: "Windows Sandbox executor".into(),
            ok: false,
            detail: format!("cannot write preflight files: {error}"),
        };
    }
    let mut session_guard =
        WindowsSandboxSessionGuard::new(config_path.clone(), session_marker_root.clone());
    let launch_lock_path = windows_sandbox_marker_base(&paths.output_root).join("launch.lock");
    let _cross_process_launch_guard =
        match acquire_windows_sandbox_launch_lock(&launch_lock_path).await {
            Ok(guard) => guard,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&root);
                let _ = std::fs::remove_dir_all(&session_marker_root);
                return DoctorCheck {
                    name: "Windows Sandbox executor".into(),
                    ok: false,
                    detail: error.to_string(),
                };
            }
        };
    let launched = match start_windows_sandbox_cli(&sandbox_cli, &config, &session_marker_root).await {
        Ok((_, sandbox_id)) => {
            execute_windows_sandbox_cli(
                &sandbox_cli,
                &sandbox_id,
                r"powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File C:\VellumEval\.vellum-current\preflight.ps1",
                Duration::from_secs(90),
            )
            .await
        }
        Err(error) => Err(error),
    };
    let result_path = invocation.join("result.json");
    if launched
        .as_ref()
        .is_ok_and(|output| !output.timed_out && !result_path.is_file())
    {
        // `wsb exec` normally returns after the script exits. Keep only a
        // short mapped-folder visibility allowance; launch/exec failures
        // should surface immediately rather than being hidden by another
        // preflight-sized wait.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !result_path.is_file() {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    let result = std::fs::read_to_string(&result_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    terminate_windows_sandbox_session(&config_path, &session_marker_root).await;
    session_guard.disarm();
    let ok = result
        .as_ref()
        .and_then(|value| value.get("ok"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && result
            .as_ref()
            .and_then(|value| value.get("workspaceWrite"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
        && result
            .as_ref()
            .and_then(|value| value.get("hiddenAbsent"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let detail = if ok {
        format!(
            "writable mapped workspace; hidden grader absent; {}",
            result
                .as_ref()
                .and_then(|value| value.get("version"))
                .and_then(Value::as_str)
                .unwrap_or("pinned Codex runtime available")
        )
    } else if let Some(error) = result
        .as_ref()
        .and_then(|value| value.get("error"))
        .and_then(Value::as_str)
    {
        error.to_string()
    } else {
        match launched {
            Ok(output) => format!(
                "sandbox produced no valid result (exit={:?}, timed_out={}): {}",
                output.code,
                output.timed_out,
                output.stderr.trim()
            ),
            Err(error) => error.to_string(),
        }
    };
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&session_marker_root);
    DoctorCheck {
        name: "Windows Sandbox executor".into(),
        ok,
        detail,
    }
}

fn windows_sandbox_marker_base(output_root: &Path) -> PathBuf {
    output_root.join(".windows-sandbox-sessions")
}

fn windows_sandbox_marker_root(output_root: &Path, owner: &str) -> PathBuf {
    windows_sandbox_marker_base(output_root).join(format!(
        "{}-{}",
        std::process::id(),
        safe_name(owner)
    ))
}

fn windows_sandbox_id_from_start_output(output: &CommandOutput) -> Option<String> {
    let value = serde_json::from_str::<Value>(output.stdout.trim()).ok()?;
    let id = value
        .get("Id")
        .and_then(Value::as_str)?
        .to_ascii_lowercase();
    (id.len() == 36
        && id
            .chars()
            .all(|character| character.is_ascii_hexdigit() || character == '-'))
    .then_some(id)
}

fn windows_sandbox_result_visibility_budget(
    exec_timed_out: bool,
    result_exists: bool,
) -> Option<Duration> {
    (!exec_timed_out && !result_exists).then_some(Duration::from_secs(10))
}

fn mark_owned_windows_sandbox_session(marker_root: &Path, id: &str) -> AppResult<()> {
    std::fs::create_dir_all(marker_root)
        .map_err(io_error("create Windows Sandbox marker directory"))?;
    std::fs::write(
        marker_root.join(format!("{}.owned", id.to_ascii_lowercase())),
        b"vellum-eval\n",
    )
    .map_err(io_error("write Windows Sandbox session marker"))
}

/// Start a headless disposable VM and persist its exact CLI-issued id before
/// returning. One retry is allowed only when no environment was allocated;
/// this covers transient host-compute/gRPC startup failures without ever
/// duplicating a live evaluation attempt.
async fn start_windows_sandbox_cli(
    sandbox_cli: &Path,
    config_xml: &str,
    marker_root: &Path,
) -> AppResult<(CommandOutput, String)> {
    let mut last_failure = None;
    for attempt in 0..2 {
        let baseline_ids = windows_sandbox_ids().await;
        let output = run_process(
            sandbox_cli.to_string_lossy().as_ref(),
            &[
                "start".into(),
                "--config".into(),
                config_xml.into(),
                "--raw".into(),
            ],
            None,
            None,
            Duration::from_secs(30),
            &[],
        )
        .await?;
        let issued_id = if let Some(id) = windows_sandbox_id_from_start_output(&output) {
            Some(id)
        } else {
            // Fail-safe for a CLI version that allocated an environment but
            // returned localized or malformed output. Only a single exact
            // post-launch id is safe to claim.
            let current_ids = windows_sandbox_ids().await;
            let added = current_ids
                .difference(&baseline_ids)
                .cloned()
                .collect::<Vec<_>>();
            (added.len() == 1).then(|| added[0].clone())
        };
        if let Some(id) = issued_id {
            mark_owned_windows_sandbox_session(marker_root, &id)?;
            return Ok((output, id));
        }
        last_failure = Some(output);
        if attempt == 0 {
            // A completed guest can occasionally remain registered long
            // enough for the Store broker to reject every later start with
            // CO_E_APPSINGLEUSE. Stop only ids carrying this run's ownership
            // marker; an unrelated interactive Sandbox must remain untouched.
            for id in &baseline_ids {
                if marker_root.join(format!("{id}.owned")).is_file() {
                    let _ = run_process(
                        sandbox_cli.to_string_lossy().as_ref(),
                        &["stop".into(), "--id".into(), id.clone(), "--raw".into()],
                        None,
                        None,
                        Duration::from_secs(30),
                        &[],
                    )
                    .await;
                }
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    }
    let output = last_failure.expect("Windows Sandbox start loop ran at least once");
    Err(AppError::Message(format!(
        "Windows Sandbox CLI did not allocate an environment (exit={:?}, timed_out={}): {}",
        output.code,
        output.timed_out,
        redact_text(&(output.stderr + &output.stdout))
    )))
}

async fn execute_windows_sandbox_cli(
    sandbox_cli: &Path,
    id: &str,
    command: &str,
    timeout: Duration,
) -> AppResult<CommandOutput> {
    run_process(
        sandbox_cli.to_string_lossy().as_ref(),
        &[
            "exec".into(),
            "--id".into(),
            id.into(),
            "--command".into(),
            command.into(),
            "--run-as".into(),
            "System".into(),
            "--raw".into(),
        ],
        None,
        None,
        timeout,
        &[],
    )
    .await
}

async fn windows_sandbox_ids() -> HashSet<String> {
    if !cfg!(target_os = "windows") {
        return HashSet::new();
    }
    let Ok(output) = run_process(
        "powershell.exe",
        &[
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            "$raw=(& wsb.exe list --raw | Out-String); if($raw){$parsed=$raw | ConvertFrom-Json; @($parsed.WindowsSandboxEnvironments) | ForEach-Object { $_.Id }}".into(),
        ],
        None,
        None,
        Duration::from_secs(10),
        &[],
    )
    .await
    else {
        return HashSet::new();
    };
    output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|id| {
            id.len() == 36
                && id
                    .chars()
                    .all(|character| character.is_ascii_hexdigit() || character == '-')
        })
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Prefer the supported `wsb stop --id` lifecycle, which releases the
/// disposable VM as well as its UI. Process termination is only a fallback
/// for evaluator-owned sessions when the CLI cannot complete.
async fn terminate_windows_sandbox_session(config_path: &Path, marker_root: &Path) {
    if !cfg!(target_os = "windows") {
        return;
    }
    let script = windows_sandbox_session_cleanup_script(config_path, marker_root);
    let _ = run_process(
        "powershell.exe",
        &[
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script,
        ],
        None,
        None,
        Duration::from_secs(15),
        &[],
    )
    .await;
}

fn windows_sandbox_session_cleanup_script(config_path: &Path, marker_root: &Path) -> String {
    let target = ps_single_quoted(&config_path.to_string_lossy());
    let markers = ps_single_quoted(&marker_root.to_string_lossy());
    format!(
        r#"$target={target}
$markers={markers}
if (Test-Path -LiteralPath $markers) {{
  @(Get-ChildItem -LiteralPath $markers -Filter '*.owned' -File -ErrorAction SilentlyContinue) | ForEach-Object {{
    $id=$_.BaseName
    if ($id -match '^[0-9a-fA-F-]{{36}}$') {{ & wsb.exe stop --id $id --raw 2>$null | Out-Null }}
    $deadline=(Get-Date).AddSeconds(20)
    $stillRunning=$true
    while($stillRunning -and (Get-Date) -lt $deadline) {{
      $listed=(& wsb.exe list --raw 2>$null | Out-String)
      $stillRunning=$listed -and $listed.IndexOf($id, [StringComparison]::OrdinalIgnoreCase) -ge 0
      if($stillRunning) {{ Start-Sleep -Milliseconds 250 }}
    }}
    if(-not $stillRunning) {{ Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }}
  }}
}}
$all=@(Get-CimInstance Win32_Process)
function Stop-VellumSandboxTree([uint32]$processId) {{
  @($all | Where-Object {{ $_.ParentProcessId -eq $processId }}) | ForEach-Object {{ Stop-VellumSandboxTree $_.ProcessId }}
  Stop-Process -Id $processId -Force -ErrorAction SilentlyContinue
}}
@($all | Where-Object {{ $_.Name -eq 'WindowsSandboxRemoteSession.exe' -and $_.CommandLine -and $_.CommandLine.IndexOf($target, [StringComparison]::OrdinalIgnoreCase) -ge 0 }}) | ForEach-Object {{ Stop-VellumSandboxTree $_.ProcessId }}"#
    )
}

/// Remove only sessions carrying an evaluator-owned CLI marker. The legacy
/// process sweep is path-scoped to old `.wsb` launches; Store-app broker
/// processes are intentionally left alone because they can be shared or
/// persistent after an environment stops.
async fn cleanup_stale_eval_windows_sandboxes(output_root: &Path) {
    if !cfg!(target_os = "windows") {
        return;
    }
    let script = stale_windows_sandbox_cleanup_script(output_root);
    let _ = run_process(
        "powershell.exe",
        &[
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script,
        ],
        None,
        None,
        Duration::from_secs(15),
        &[],
    )
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
}

fn stale_windows_sandbox_cleanup_script(output_root: &Path) -> String {
    let markers = ps_single_quoted(&windows_sandbox_marker_base(output_root).to_string_lossy());
    format!(
        r#"$markers={markers}
function Stop-VellumOwnedMarkers([string]$directory) {{
  @(Get-ChildItem -LiteralPath $directory -Filter '*.owned' -File -ErrorAction SilentlyContinue) | ForEach-Object {{
    $id=$_.BaseName
    if ($id -match '^[0-9a-fA-F-]{{36}}$') {{ & wsb.exe stop --id $id --raw 2>$null | Out-Null }}
    $deadline=(Get-Date).AddSeconds(20)
    $stillRunning=$true
    while($stillRunning -and (Get-Date) -lt $deadline) {{
      $listed=(& wsb.exe list --raw 2>$null | Out-String)
      $stillRunning=$listed -and $listed.IndexOf($id, [StringComparison]::OrdinalIgnoreCase) -ge 0
      if($stillRunning) {{ Start-Sleep -Milliseconds 250 }}
    }}
    if(-not $stillRunning) {{ Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }}
  }}
}}
if (Test-Path -LiteralPath $markers) {{
  # Legacy markers predate per-process ownership and are therefore stale.
  Stop-VellumOwnedMarkers $markers
  @(Get-ChildItem -LiteralPath $markers -Directory -ErrorAction SilentlyContinue) | ForEach-Object {{
    $ownerFile=Join-Path $_.FullName 'owner.pid'
    $ownerPid=0
    if(Test-Path -LiteralPath $ownerFile) {{ [void][uint32]::TryParse((Get-Content -Raw -LiteralPath $ownerFile).Trim(), [ref]$ownerPid) }}
    $ownerAlive=$ownerPid -gt 0 -and $null -ne (Get-Process -Id $ownerPid -ErrorAction SilentlyContinue)
    if(-not $ownerAlive) {{
      Stop-VellumOwnedMarkers $_.FullName
      Remove-Item -LiteralPath $_.FullName -Recurse -Force -ErrorAction SilentlyContinue
    }}
  }}
}}"#
    )
}

async fn executable_root(name: &str, parent_levels: usize) -> Option<PathBuf> {
    let output = run_process(
        "where.exe",
        &[name.into()],
        None,
        None,
        Duration::from_secs(10),
        &[],
    )
    .await
    .ok()?;
    let mut path = output
        .stdout
        .lines()
        .map(str::trim)
        .map(PathBuf::from)
        .find(|path| path.is_file())?;
    for _ in 0..=parent_levels {
        path = path.parent()?.to_path_buf();
    }
    Some(path)
}

fn ps_single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn wsb_mapping(host: &Path, sandbox: &str, read_only: bool) -> String {
    format!(
        "<MappedFolder><HostFolder>{}</HostFolder><SandboxFolder>{}</SandboxFolder><ReadOnly>{}</ReadOnly></MappedFolder>",
        xml_escape(&host.to_string_lossy()),
        xml_escape(sandbox),
        if read_only { "true" } else { "false" }
    )
}

fn read_text_file(path: &Path) -> String {
    std::fs::read(path)
        .ok()
        .map(|bytes| crate::harness::shell::decode_output(&bytes))
        .unwrap_or_default()
}

fn snapshot_rollout_offsets(codex_home: &Path) -> HashMap<PathBuf, u64> {
    let mut offsets = HashMap::new();
    collect_rollout_files(&codex_home.join("sessions"), &mut |path| {
        let length = std::fs::metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        offsets.insert(path.to_path_buf(), length);
    });
    offsets
}

fn read_rollout_delta(codex_home: &Path, offsets: &HashMap<PathBuf, u64>) -> String {
    let mut chunks = Vec::new();
    collect_rollout_files(&codex_home.join("sessions"), &mut |path| {
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let offset = offsets.get(path).copied().unwrap_or(0) as usize;
        if offset < bytes.len() {
            chunks.push(crate::harness::shell::decode_output(&bytes[offset..]));
        }
    });
    chunks.join("")
}

fn completed_compaction_events(jsonl: &str) -> String {
    let mut events = String::new();
    for line in jsonl.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let raw_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        let item_type = value
            .get("item")
            .or_else(|| value.get("payload"))
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let completed = raw_type == "compacted"
            || raw_type == "compaction.completed"
            || (raw_type == "item.completed" && item_type.to_ascii_lowercase().contains("compact"));
        if completed {
            events.push_str(line);
            events.push('\n');
        }
    }
    events
}

fn collect_rollout_files(root: &Path, visitor: &mut impl FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollout_files(&path, visitor);
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
        {
            visitor(&path);
        }
    }
}

fn windows_shell_contract() -> &'static str {
    use crate::harness::shell::ShellKind;
    match crate::harness::shell::detected().shell {
        ShellKind::Pwsh => "windows-pwsh",
        ShellKind::Powershell => "windows-powershell",
        ShellKind::Cmd => "windows-cmd",
        ShellKind::GitBash => "windows-pwsh",
        ShellKind::Bash | ShellKind::Zsh => "windows-pwsh",
    }
}

struct DockerIsolation {
    network: String,
    relay: String,
    agent_image: String,
    run_label: String,
}

impl DockerIsolation {
    async fn start(run_id: &str, gateway_port: u16, codex_version: &str) -> AppResult<Self> {
        let network = format!("vellum-eval-{}", safe_name(run_id));
        let relay = format!("{network}-gateway");
        let image = agent_image(codex_version);
        let run_label = format!("vellum.eval.run={}", safe_name(run_id));
        ensure_success(
            run_process(
                "docker",
                &[
                    "network".into(),
                    "create".into(),
                    "--internal".into(),
                    network.clone(),
                ],
                None,
                None,
                Duration::from_secs(30),
                &[],
            )
            .await?,
            "create isolated Docker network",
        )?;
        let relay_result = run_process(
            "docker",
            &[
                "run".into(),
                "-d".into(),
                "--name".into(),
                relay.clone(),
                "--label".into(),
                run_label.clone(),
                "--add-host".into(),
                "host.docker.internal:host-gateway".into(),
                "--cap-drop".into(),
                "ALL".into(),
                "--security-opt".into(),
                "no-new-privileges".into(),
                "--entrypoint".into(),
                "/usr/bin/socat".into(),
                image.clone(),
                "TCP-LISTEN:15721,fork,reuseaddr".into(),
                format!("TCP:host.docker.internal:{gateway_port}"),
            ],
            None,
            None,
            Duration::from_secs(60),
            &[],
        )
        .await?;
        if relay_result.code != Some(0) {
            let _ = cleanup_network(&network, &relay).await;
            return Err(AppError::Message(format!(
                "cannot start eval gateway relay: {}",
                relay_result.stderr
            )));
        }
        let connect_result = run_process(
            "docker",
            &[
                "network".into(),
                "connect".into(),
                "--alias".into(),
                "vellum-eval-gateway".into(),
                network.clone(),
                relay.clone(),
            ],
            None,
            None,
            Duration::from_secs(30),
            &[],
        )
        .await
        .and_then(|output| ensure_success(output, "connect eval relay to internal network"));
        if let Err(error) = connect_result {
            let _ = cleanup_network(&network, &relay).await;
            return Err(error);
        }
        Ok(Self {
            network,
            relay,
            agent_image: image,
            run_label,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_codex(
        &self,
        workspace: &Path,
        codex_home: &Path,
        catalog: &Path,
        model: &str,
        resume: Option<&str>,
        prompt: &str,
        token: &str,
        timeout: Duration,
        limits: &super::manifest::TaskLimits,
        forced_context_window: Option<u64>,
        forced_auto_compact_token_limit: Option<u64>,
        reasoning_effort: Option<&str>,
        compaction_engine: &str,
        codex_executable: Option<&Path>,
    ) -> AppResult<CommandOutput> {
        if codex_executable.is_some() {
            return Err(AppError::Message(
                "ablation profile E0–E5 cannot run on the Docker executor: Enhanced Codex requires the verified host or Windows Sandbox artifact, not the image-bundled Codex".into(),
            ));
        }
        let container = eval_container_name("agent");
        let mut args = self.base_args(workspace, codex_home, limits, &container);
        args.extend([
            // The prompt is written to `docker run` stdin below. Without
            // `-i`, Docker closes the container's stdin and Codex reports
            // "No prompt provided via stdin."
            "-i".into(),
            "-e".into(),
            format!("OPENAI_API_KEY={token}"),
            "-v".into(),
            format!("{}:/eval/model-catalog.json:ro", docker_path(catalog)),
            self.agent_image.clone(),
            "exec".into(),
        ]);
        args.extend([
            "-m".into(),
            model.into(),
            "--dangerously-bypass-approvals-and-sandbox".into(),
            "--skip-git-repo-check".into(),
            "--ignore-rules".into(),
            "--strict-config".into(),
            "--json".into(),
            "--disable".into(),
            "apps".into(),
            "--disable".into(),
            "plugins".into(),
            "--disable".into(),
            "remote_plugin".into(),
            "-c".into(),
            "model_provider=\"vellum_eval\"".into(),
            "-c".into(),
            format!(
                "model_providers.vellum_eval.name=\"{}\"",
                eval_codex_provider_display_name(compaction_engine)
            ),
            "-c".into(),
            "model_providers.vellum_eval.base_url=\"http://vellum-eval-gateway:15721/v1\"".into(),
            "-c".into(),
            "model_providers.vellum_eval.env_key=\"OPENAI_API_KEY\"".into(),
            "-c".into(),
            "model_providers.vellum_eval.wire_api=\"responses\"".into(),
            "-c".into(),
            "model_catalog_json=\"/eval/model-catalog.json\"".into(),
            "-c".into(),
            format!(
                "features.remote_compaction_v2={}",
                eval_remote_compaction_v2_enabled(compaction_engine)
            ),
            "-c".into(),
            "tools.update_plan.enabled=true".into(),
        ]);
        append_forced_compaction_config(
            &mut args,
            forced_context_window,
            forced_auto_compact_token_limit,
        );
        append_reasoning_effort_config(&mut args, reasoning_effort);
        append_exec_prompt_args(&mut args, resume);
        let output =
            run_process("docker", &args, None, Some(prompt.as_bytes()), timeout, &[]).await?;
        if output.timed_out {
            remove_container(&container).await;
        }
        Ok(output)
    }

    async fn run_shell(
        &self,
        workspace: &Path,
        codex_home: &Path,
        command: &str,
        timeout: Duration,
        limits: &super::manifest::TaskLimits,
    ) -> AppResult<CommandOutput> {
        let container = eval_container_name("setup");
        let mut args = self.base_args(workspace, codex_home, limits, &container);
        args.extend([
            "--entrypoint".into(),
            "/bin/bash".into(),
            self.agent_image.clone(),
            "-lc".into(),
            command.into(),
        ]);
        let output = run_process("docker", &args, None, None, timeout, &[]).await?;
        if output.timed_out {
            remove_container(&container).await;
        }
        Ok(output)
    }

    async fn run_verification(
        &self,
        workspace: &Path,
        hidden: &Path,
        commands: &[String],
        timeout: Duration,
        limits: &super::manifest::TaskLimits,
        grader_image: Option<&str>,
    ) -> AppResult<CommandOutput> {
        let container = eval_container_name("grader");
        let workdir = if grader_image.is_some() {
            "/testbed"
        } else {
            "/workspace"
        };
        let args = vec![
            "run".into(),
            "--rm".into(),
            "--name".into(),
            container.clone(),
            "--label".into(),
            self.run_label.clone(),
            "--network".into(),
            "none".into(),
            "--read-only".into(),
            "--cap-drop".into(),
            "ALL".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
            "--pids-limit".into(),
            limits.pids.to_string(),
            "--cpus".into(),
            limits.cpu.to_string(),
            "--memory".into(),
            format!("{}m", limits.memory_mb),
            // Git refuses to read a repository-local config until ownership
            // is trusted. Docker Desktop mounts the host workspace with a
            // different numeric owner, so setting core.fileMode only in
            // `.git/config` is insufficient: the agent first sees "dubious
            // ownership", then repeatedly re-investigates an apparently
            // broken repository. Command-scope config is inherited by every
            // shell tool spawned by Codex and does not modify the image or the
            // user's global Git configuration.
            "-e".into(),
            "GIT_CONFIG_COUNT=3".into(),
            "-e".into(),
            "GIT_CONFIG_KEY_0=safe.directory".into(),
            "-e".into(),
            format!("GIT_CONFIG_VALUE_0={workdir}"),
            "-e".into(),
            "GIT_CONFIG_KEY_1=core.fileMode".into(),
            "-e".into(),
            "GIT_CONFIG_VALUE_1=false".into(),
            "-e".into(),
            "GIT_CONFIG_KEY_2=core.autocrlf".into(),
            "-e".into(),
            "GIT_CONFIG_VALUE_2=true".into(),
            "--tmpfs".into(),
            "/tmp:rw,noexec,nosuid,size=64m".into(),
            "-v".into(),
            format!("{}:{workdir}", docker_path(workspace)),
            "-v".into(),
            format!("{}:/hidden:ro", docker_path(hidden)),
            "-w".into(),
            workdir.into(),
            "-e".into(),
            "HOME=/tmp/home".into(),
            "--entrypoint".into(),
            "/bin/sh".into(),
            grader_image.unwrap_or(&self.agent_image).to_string(),
            "-lc".into(),
            format!("mkdir -p /tmp/home && {}", commands.join(" && ")),
        ];
        let output = run_process("docker", &args, None, None, timeout, &[]).await?;
        if output.timed_out {
            remove_container(&container).await;
        }
        Ok(output)
    }

    fn base_args(
        &self,
        workspace: &Path,
        codex_home: &Path,
        limits: &super::manifest::TaskLimits,
        container: &str,
    ) -> Vec<String> {
        vec![
            "run".into(),
            "--rm".into(),
            "--name".into(),
            container.into(),
            "--label".into(),
            self.run_label.clone(),
            "--network".into(),
            self.network.clone(),
            "--read-only".into(),
            "--cap-drop".into(),
            "ALL".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
            "--pids-limit".into(),
            limits.pids.to_string(),
            "--cpus".into(),
            limits.cpu.to_string(),
            "--memory".into(),
            format!("{}m", limits.memory_mb),
            // The Linux agent sees a Windows-owned bind mount. Supply a
            // process-scoped Git configuration so every shell spawned by
            // Codex can inspect the pinned repository without changing any
            // global configuration in the image or on the host.
            "-e".into(),
            "GIT_CONFIG_COUNT=3".into(),
            "-e".into(),
            "GIT_CONFIG_KEY_0=safe.directory".into(),
            "-e".into(),
            "GIT_CONFIG_VALUE_0=/workspace".into(),
            "-e".into(),
            "GIT_CONFIG_KEY_1=core.fileMode".into(),
            "-e".into(),
            "GIT_CONFIG_VALUE_1=false".into(),
            "-e".into(),
            "GIT_CONFIG_KEY_2=core.autocrlf".into(),
            "-e".into(),
            "GIT_CONFIG_VALUE_2=true".into(),
            "--tmpfs".into(),
            "/tmp:rw,nosuid,size=256m".into(),
            "-v".into(),
            format!("{}:/workspace", docker_path(workspace)),
            "-v".into(),
            format!("{}:/codex-home", docker_path(codex_home)),
            "-w".into(),
            "/workspace".into(),
        ]
    }

    async fn shutdown(self) {
        remove_labeled_containers(&self.run_label).await;
        let _ = cleanup_network(&self.network, &self.relay).await;
    }
}

fn append_forced_compaction_config(
    args: &mut Vec<String>,
    context_window: Option<u64>,
    forced_auto_compact_token_limit: Option<u64>,
) {
    if let Some(context_window) = context_window {
        let compact_limit =
            forced_auto_compact_token_limit.unwrap_or_else(|| context_window.saturating_mul(2) / 3);
        args.extend([
            "-c".into(),
            format!("model_context_window={context_window}"),
            "-c".into(),
            format!("model_auto_compact_token_limit={compact_limit}"),
        ]);
    }
}

fn append_reasoning_effort_config(args: &mut Vec<String>, effort: Option<&str>) {
    if let Some(effort) = effort.filter(|value| !value.trim().is_empty()) {
        args.extend(["-c".into(), format!("model_reasoning_effort=\"{effort}\"")]);
    }
}

/// Finish a `codex exec` command after all options shared by fresh and resumed
/// turns have been appended. `resume` is a clap subcommand, so outer options
/// such as `-C` and `--sandbox` must precede it; putting those options after
/// the session id makes current Codex versions reject the invocation.
fn append_exec_prompt_args(args: &mut Vec<String>, resume: Option<&str>) {
    if let Some(thread) = resume {
        args.extend(["resume".into(), thread.into(), "-".into()]);
    } else {
        args.push("-".into());
    }
}

fn append_powershell_exec_prompt_args(args: &mut Vec<String>, resume: Option<&str>) {
    if let Some(thread) = resume {
        args.extend(["'resume'".into(), ps_single_quoted(thread), "'-'".into()]);
    } else {
        args.push("'-'".into());
    }
}

fn eval_container_name(kind: &str) -> String {
    format!(
        "vellum-eval-{kind}-{}-{}",
        std::process::id(),
        CONTAINER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

async fn remove_container(name: &str) {
    let _ = run_process(
        "docker",
        &["rm".into(), "-f".into(), name.into()],
        None,
        None,
        Duration::from_secs(30),
        &[],
    )
    .await;
}

async fn remove_labeled_containers(label: &str) {
    let listed = run_process(
        "docker",
        &[
            "container".into(),
            "ls".into(),
            "-aq".into(),
            "--filter".into(),
            format!("label={label}"),
        ],
        None,
        None,
        Duration::from_secs(30),
        &[],
    )
    .await;
    let Ok(listed) = listed else {
        return;
    };
    for id in listed.stdout.lines().filter(|line| !line.trim().is_empty()) {
        remove_container(id.trim()).await;
    }
}

async fn cleanup_network(network: &str, relay: &str) -> AppResult<()> {
    let _ = run_process(
        "docker",
        &["rm".into(), "-f".into(), relay.into()],
        None,
        None,
        Duration::from_secs(30),
        &[],
    )
    .await;
    let _ = run_process(
        "docker",
        &["network".into(), "rm".into(), network.into()],
        None,
        None,
        Duration::from_secs(30),
        &[],
    )
    .await;
    Ok(())
}

fn write_eval_catalog(
    path: &Path,
    models: &[String],
    forced_context_window: Option<u64>,
    forced_auto_compact_token_limit: Option<u64>,
    base_catalog: &Value,
    catalog_mode: &str,
) -> AppResult<()> {
    let allowed = models.iter().map(String::as_str).collect::<HashSet<_>>();
    let mut catalog = base_catalog.clone();
    let entries = catalog
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| AppError::Message("Vellum model catalog has no models array".into()))?;
    entries.retain(|entry| {
        entry
            .get("slug")
            .and_then(Value::as_str)
            .map(|slug| allowed.contains(slug))
            .unwrap_or(false)
    });
    let found = entries
        .iter()
        .filter_map(|entry| entry.get("slug").and_then(Value::as_str))
        .collect::<HashSet<_>>();
    let missing = allowed.difference(&found).copied().collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(AppError::Message(format!(
            "models are missing from the generated Vellum catalog: {}",
            missing.join(", ")
        )));
    }
    if let Some(context_window) = forced_context_window {
        let compact_limit =
            forced_auto_compact_token_limit.unwrap_or_else(|| context_window.saturating_mul(2) / 3);
        for entry in entries.iter_mut() {
            if let Some(object) = entry.as_object_mut() {
                object.insert("context_window".into(), json!(context_window));
                object.insert("max_context_window".into(), json!(context_window));
                object.insert("effective_context_window_percent".into(), json!(95));
                object.insert("auto_compact_token_limit".into(), json!(compact_limit));
            }
        }
    }
    // Stage -1 parity: the default lane must preserve the exact production
    // entry. Legacy mutation remains available only as an explicitly recorded
    // diagnostic mode for older pinned CLI images.
    if catalog_mode == "legacy" {
        for entry in entries {
            if let Some(object) = entry.as_object_mut() {
                object
                    .entry("supports_reasoning_summaries")
                    .or_insert_with(|| json!(false));
                object
                    .entry("shell_type")
                    .or_insert_with(|| json!("shell_command"));
                object
                    .entry("include_skills_usage_instructions")
                    .or_insert_with(|| json!(false));
                object
                    .entry("default_reasoning_summary")
                    .or_insert_with(|| json!("none"));
                object
                    .entry("support_verbosity")
                    .or_insert_with(|| json!(true));
                object
                    .entry("default_verbosity")
                    .or_insert_with(|| json!("low"));
                object
                    .entry("apply_patch_tool_type")
                    .or_insert_with(|| json!("freeform"));
                object
                    .entry("web_search_tool_type")
                    .or_insert_with(|| json!("text_and_image"));
                object
                    .entry("truncation_policy")
                    .or_insert_with(|| json!({"mode": "tokens", "limit": 10_000}));
                object
                    .entry("supports_parallel_tool_calls")
                    .or_insert_with(|| json!(true));
                object
                    .entry("supports_image_detail_original")
                    .or_insert_with(|| json!(true));
                object.entry("comp_hash").or_insert_with(|| json!("eval"));
                object
                    .entry("experimental_supported_tools")
                    .or_insert_with(|| json!([]));
                object
                    .entry("supports_search_tool")
                    .or_insert_with(|| json!(true));
                object
                    .entry("use_responses_lite")
                    .or_insert_with(|| json!(false));
            }
        }
    }
    write_json(path, &catalog)
}

/// Read the exact context values published to Codex for one eval model. Zero
/// means the catalog intentionally omitted the field; callers must not invent
/// a provider window or threshold in that case.
fn eval_catalog_context_settings(path: &Path, model: &str) -> AppResult<(u64, u64)> {
    let catalog: Value =
        serde_json::from_slice(&std::fs::read(path).map_err(io_error("read eval model catalog"))?)
            .map_err(|error| AppError::Message(format!("decode eval model catalog: {error}")))?;
    let entry = catalog
        .get("models")
        .and_then(Value::as_array)
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry.get("slug").and_then(Value::as_str) == Some(model))
        })
        .ok_or_else(|| {
            AppError::Message(format!(
                "eval model catalog is missing published settings for {model}"
            ))
        })?;
    Ok((
        entry
            .get("context_window")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        entry
            .get("auto_compact_token_limit")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    ))
}

async fn run_process(
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    input: Option<&[u8]>,
    timeout: Duration,
    env: &[(&str, &str)],
) -> AppResult<CommandOutput> {
    const MAX_CAPTURE_BYTES: usize = 64 * 1024 * 1024;
    let started = Instant::now();
    let mut command = crate::process::background_tokio_command(program);
    // A Windows promotion run is frequently launched from Codex Desktop
    // itself. Nested `codex exec` must not inherit the parent Desktop task ID,
    // originator, or permission profile: those variables describe the parent
    // task and previously made the evaluator silently exercise a different
    // client policy. CODEX_HOME and the explicit CLI sandbox remain the sole
    // authority for the child runtime.
    if Path::new(program)
        .file_stem()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.to_ascii_lowercase().contains("codex"))
    {
        for key in [
            "CODEX_THREAD_ID",
            "CODEX_INTERNAL_ORIGINATOR_OVERRIDE",
            "CODEX_PERMISSION_PROFILE",
        ] {
            command.env_remove(key);
        }
        command.env("CODEX_CI", "1");
    }
    command
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .map_err(|error| AppError::Message(format!("cannot run {program}: {error}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::Message(format!("{program} stdout is unavailable")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::Message(format!("{program} stderr is unavailable")))?;
    let stdout_capture = Arc::new(Mutex::new(Vec::new()));
    let stderr_capture = Arc::new(Mutex::new(Vec::new()));
    let first_byte_capture = Arc::new(AtomicU64::new(u64::MAX));
    let mut stdout_task = tokio::spawn(read_limited_shared(
        stdout,
        MAX_CAPTURE_BYTES,
        Arc::clone(&stdout_capture),
        Some((started, Arc::clone(&first_byte_capture))),
    ));
    let mut stderr_task = tokio::spawn(read_limited_shared(
        stderr,
        MAX_CAPTURE_BYTES,
        Arc::clone(&stderr_capture),
        None,
    ));
    if let Some(input) = input {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input).await.map_err(|error| {
                AppError::Message(format!("cannot write {program} stdin: {error}"))
            })?;
        }
    }
    let (code, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(result) => (
            result
                .map_err(|error| AppError::Message(format!("{program} failed: {error}")))?
                .code(),
            false,
        ),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            (None, true)
        }
    };
    // Give readers a short chance to drain bytes the child wrote before it was
    // killed. Aborting immediately races the reader task under a busy test run
    // and can discard the last complete JSONL event. `docker run` descendants
    // may still hold the pipes open, so the drain itself remains bounded.
    if timed_out {
        let drained = tokio::time::timeout(Duration::from_millis(250), async {
            let _ = (&mut stdout_task).await;
            let _ = (&mut stderr_task).await;
        })
        .await
        .is_ok();
        if !drained {
            stdout_task.abort();
            stderr_task.abort();
            let _ = stdout_task.await;
            let _ = stderr_task.await;
        }
    } else {
        stdout_task.await.map_err(|error| {
            AppError::Message(format!("{program} stdout task failed: {error}"))
        })??;
        stderr_task.await.map_err(|error| {
            AppError::Message(format!("{program} stderr task failed: {error}"))
        })??;
    }
    let stdout = stdout_capture
        .lock()
        .expect("stdout capture poisoned")
        .clone();
    let stderr = stderr_capture
        .lock()
        .expect("stderr capture poisoned")
        .clone();
    let first_byte = first_byte_capture.load(Ordering::Relaxed);
    let first_byte_ms = (first_byte != u64::MAX).then_some(first_byte);
    let mut stderr = String::from_utf8_lossy(&stderr).into_owned();
    if timed_out {
        if !stderr.is_empty() {
            stderr.push('\n');
        }
        stderr.push_str(&format!(
            "{program} timed out after {} seconds",
            timeout.as_secs()
        ));
    }
    Ok(CommandOutput {
        code,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr,
        timed_out,
        first_byte_ms,
    })
}

async fn read_limited_shared<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
    output: Arc<Mutex<Vec<u8>>>,
    first_byte: Option<(Instant, Arc<AtomicU64>)>,
) -> AppResult<()> {
    let mut buffer = vec![0_u8; 16 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| AppError::Message(format!("cannot read process output: {error}")))?;
        if read == 0 {
            break;
        }
        if let Some((started, marker)) = &first_byte {
            let _ = marker.compare_exchange(
                u64::MAX,
                started.elapsed().as_millis() as u64,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
        let reached_limit = {
            let mut output = output.lock().expect("process output capture poisoned");
            let remaining = limit.saturating_sub(output.len());
            output.extend_from_slice(&buffer[..read.min(remaining)]);
            output.len() >= limit
        };
        if reached_limit {
            // Continue draining the pipe to avoid deadlocking the child while
            // refusing to allocate unbounded host memory.
            while reader.read(&mut buffer).await.map_err(|error| {
                AppError::Message(format!("cannot drain process output: {error}"))
            })? != 0
            {}
            break;
        }
    }
    Ok(())
}

fn ensure_success(output: CommandOutput, action: &str) -> AppResult<CommandOutput> {
    if output.code == Some(0) && !output.timed_out {
        Ok(output)
    } else {
        Err(AppError::Message(format!(
            "{action} failed: {}",
            redact_text(&(output.stderr + &output.stdout))
        )))
    }
}

fn copy_tree(source: &Path, destination: &Path) -> AppResult<()> {
    copy_tree_internal(source, destination, false)
}

fn copy_tree_preserving_git(source: &Path, destination: &Path) -> AppResult<()> {
    copy_tree_internal(source, destination, true)
}

fn copy_tree_internal(source: &Path, destination: &Path, preserve_git: bool) -> AppResult<()> {
    if !source.is_dir() {
        return Err(AppError::Message(format!(
            "eval source directory does not exist: {}",
            source.display()
        )));
    }
    std::fs::create_dir_all(destination).map_err(io_error("create copied directory"))?;
    for entry in std::fs::read_dir(source).map_err(io_error("read copied directory"))? {
        let entry = entry.map_err(io_error("read copied entry"))?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(io_error("inspect copied entry type"))?;
        if file_type.is_symlink() {
            return Err(AppError::Message(format!(
                "eval sources may not contain symbolic links: {}",
                source_path.display()
            )));
        }
        if file_type.is_dir() {
            if entry.file_name() == ".git" && !preserve_git {
                continue;
            }
            copy_tree_internal(&source_path, &destination_path, preserve_git)?;
        } else if file_type.is_file() {
            std::fs::copy(&source_path, &destination_path).map_err(io_error("copy eval file"))?;
        } else {
            return Err(AppError::Message(format!(
                "unsupported eval source entry: {}",
                source_path.display()
            )));
        }
    }
    Ok(())
}

fn directory_size(path: &Path) -> AppResult<u64> {
    let mut total = 0_u64;
    for entry in std::fs::read_dir(path).map_err(io_error("measure eval workspace"))? {
        let entry = entry.map_err(io_error("measure eval workspace entry"))?;
        let metadata = entry
            .metadata()
            .map_err(io_error("measure eval workspace metadata"))?;
        if metadata.is_dir() {
            total = total.saturating_add(directory_size(&entry.path())?);
        } else {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

fn completed_case_ids(path: &Path) -> AppResult<HashSet<String>> {
    if !path.is_file() {
        return Ok(HashSet::new());
    }
    let text = std::fs::read_to_string(path).map_err(io_error("read eval results"))?;
    let mut latest = HashMap::<String, TaskResult>::new();
    for result in text
        .lines()
        .filter_map(|line| serde_json::from_str::<TaskResult>(line).ok())
    {
        latest.insert(result.case_id.clone(), result);
    }
    Ok(latest
        .into_iter()
        .filter_map(|(case_id, result)| (!result_is_resumable(&result)).then_some(case_id))
        .collect())
}

/// A run-level resume repeats only attempts that cannot produce a valid eval
/// conclusion. Model, Vellum, protocol and permanent budget failures are real
/// outcomes and remain complete. Provider failures and non-deterministic
/// evaluator failures may be repeated after the external condition recovers.
fn result_is_resumable(result: &TaskResult) -> bool {
    if result.passed || !result.injected_faults.is_empty() {
        return false;
    }
    // `swe_token_limit` is a gateway admission gate that refuses a request
    // *before dispatch* whenever spent + in-flight-reserved + projected
    // tokens would exceed the case's budget (see `EvalTokenBudget::try_reserve`
    // in gateway.rs). A refused request never reserves or settles any tokens,
    // so a burst of concurrent retries during a provider 429 storm can trip
    // this gate transiently without the case's real, settled token spend ever
    // exceeding its budget. Only a genuine settled overspend — the same
    // comparison `limit_exceeded` uses at case-completion time — should count
    // as a permanent, non-resumable cap; the bare error string does not.
    let genuine_token_overspend = result
        .max_input_tokens
        .is_some_and(|limit| result.metrics.input_tokens > limit)
        || result
            .max_output_tokens
            .is_some_and(|limit| result.metrics.output_tokens > limit);
    let has_permanent_cap = result.metrics.errors.iter().any(|error| {
        error.contains("swe_turn_cap")
            || error.contains("campaign_quota")
            || error.contains("budget_exceeded")
            || error.contains("single_run_official_cap")
    }) || genuine_token_overspend
        || result.protocol_violations.iter().any(|violation| {
            matches!(
                violation.as_str(),
                "luna_campaign_quota" | "single_run_official_cap"
            )
        });
    if has_permanent_cap {
        return false;
    }
    match result.failure_origin {
        Some(FailureOrigin::Provider) => retryable_provider_failure(result),
        Some(FailureOrigin::EvaluatorInfrastructure) => true,
        _ => false,
    }
}

fn case_attempt_counts(path: &Path) -> AppResult<HashMap<String, usize>> {
    if !path.is_file() {
        return Ok(HashMap::new());
    }
    let text = std::fs::read_to_string(path).map_err(io_error("read eval results"))?;
    let mut counts = HashMap::new();
    for result in text
        .lines()
        .filter_map(|line| serde_json::from_str::<TaskResult>(line).ok())
    {
        *counts.entry(result.case_id).or_insert(0) += 1;
    }
    Ok(counts)
}

fn archive_case_outputs_for_resume(
    run_root: &Path,
    case_id: &str,
    attempt: usize,
) -> AppResult<()> {
    let archive_base = run_root
        .join("attempts")
        .join(case_work_dir_name(case_id))
        .join(format!("attempt-{attempt}"));
    let mut archive_root = archive_base.clone();
    let mut collision = 0_usize;
    while archive_root.exists() {
        collision += 1;
        archive_root =
            archive_base.with_file_name(format!("attempt-{attempt}-interrupted-{collision}"));
    }

    let case_root = run_root.join("cases").join(case_work_dir_name(case_id));
    let mut outputs = Vec::<(PathBuf, &'static str, String)>::new();
    for directory in ["traces", "patches", "test-output"] {
        let source_root = run_root.join(directory);
        if !source_root.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&source_root).map_err(io_error("read eval outputs"))? {
            let entry = entry.map_err(io_error("read eval output entry"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == format!("{case_id}.patch")
                || name == format!("{case_id}.txt")
                || name.starts_with(&format!("{case_id}."))
            {
                outputs.push((entry.path(), directory, name));
            }
        }
    }
    if !case_root.exists() && outputs.is_empty() {
        return Ok(());
    }

    std::fs::create_dir_all(&archive_root).map_err(io_error("create eval attempt archive"))?;
    if case_root.exists() {
        std::fs::rename(&case_root, archive_root.join("case"))
            .map_err(io_error("archive resumed eval case"))?;
    }
    for (source, directory, name) in outputs {
        let destination_root = archive_root.join(directory);
        std::fs::create_dir_all(&destination_root)
            .map_err(io_error("create eval output archive"))?;
        std::fs::rename(source, destination_root.join(name))
            .map_err(io_error("archive resumed eval output"))?;
    }
    Ok(())
}

fn tokens_in_results(path: &Path) -> AppResult<u64> {
    if !path.is_file() {
        return Ok(0);
    }
    let text = std::fs::read_to_string(path).map_err(io_error("read eval token usage"))?;
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str::<TaskResult>(line).ok())
        .map(|result| result.metrics.input_tokens + result.metrics.output_tokens)
        .sum())
}

fn append_jsonl(path: &Path, value: &impl Serialize) -> AppResult<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(io_error("open eval JSONL"))?;
    serde_json::to_writer(&mut file, value)
        .map_err(|error| AppError::Message(format!("serialize eval JSONL: {error}")))?;
    file.write_all(b"\n").map_err(io_error("append eval JSONL"))
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> AppResult<()> {
    std::fs::create_dir_all(dst).map_err(io_error("create destination directory"))?;
    for entry in std::fs::read_dir(src).map_err(io_error("read source directory"))? {
        let entry = entry.map_err(io_error("read source directory entry"))?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else if path.is_file() {
            std::fs::copy(&path, &target).map_err(io_error("copy staged file"))?;
        }
    }
    Ok(())
}

fn stage_selected_grok_account(
    manager: &crate::grok_accounts::GrokAccountManager,
    source_root: &Path,
    destination_root: &Path,
    selector: Option<&str>,
) -> AppResult<()> {
    let account_id = match selector {
        Some(selector) => manager.find_account_id(selector).ok_or_else(|| {
            AppError::Message(format!(
                "Grok account '{selector}' not found in managed accounts"
            ))
        })?,
        None => manager.peek_default_account_id().ok_or_else(|| {
            AppError::Message("Grok is not signed in; no default account can be staged".into())
        })?,
    };
    let account = manager
        .status()
        .accounts
        .into_iter()
        .find(|account| account.account_id == account_id)
        .ok_or_else(|| AppError::Message(format!("Grok account not found: {account_id}")))?;

    std::fs::create_dir_all(destination_root)
        .map_err(io_error("create staged Grok account directory"))?;
    std::fs::copy(
        source_root.join("accounts.json"),
        destination_root.join("accounts.json"),
    )
    .map_err(io_error("copy staged Grok accounts metadata"))?;

    if account.source == crate::grok_accounts::GrokAccountSource::Managed {
        let source_home = manager.account_home(&account_id)?;
        let profile_key = source_home.file_name().ok_or_else(|| {
            AppError::Message(format!(
                "managed Grok account profile has no directory name: {}",
                source_home.display()
            ))
        })?;
        copy_dir_recursive(
            &source_home,
            &destination_root.join("profiles").join(profile_key),
        )?;
    }
    Ok(())
}

pub fn official_turns_in_run(run_root: &Path) -> AppResult<u32> {
    let traces_dir = run_root.join("traces");
    if !traces_dir.is_dir() {
        return Ok(0);
    }
    let mut total_official_turns = 0_u32;
    let entries = std::fs::read_dir(&traces_dir).map_err(io_error("read eval traces directory"))?;
    for entry in entries {
        let entry = entry.map_err(io_error("read eval trace entry"))?;
        let trace_file = entry.path();
        if !trace_file
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".gateway.jsonl"))
        {
            continue;
        }
        let content =
            std::fs::read_to_string(&trace_file).map_err(io_error("read eval gateway trace"))?;
        for line in content.lines() {
            let val = serde_json::from_str::<Value>(line).map_err(|error| {
                AppError::Message(format!(
                    "invalid gateway trace {}: {error}",
                    trace_file.display()
                ))
            })?;
            if val.get("event").and_then(Value::as_str) != Some("request") {
                continue;
            }
            let fault = val.get("fault").and_then(Value::as_str).unwrap_or("");
            // Gateway faults are emitted before Official admission and must not
            // be reconstructed as spent turns on resume.
            if !fault.is_empty() {
                continue;
            }
            // New traces carry the authoritative ProviderKind classification.
            // Exact legacy identifiers keep resume compatible with older runs
            // without reintroducing substring guesses such as "Unofficial".
            let explicitly_official = val.get("officialRoute").and_then(Value::as_bool);
            let legacy_official = || {
                let provider = val.get("provider").and_then(Value::as_str).unwrap_or("");
                let route_id = val.get("routeId").and_then(Value::as_str).unwrap_or("");
                let model = val.get("model").and_then(Value::as_str).unwrap_or("");
                provider.eq_ignore_ascii_case("official")
                    || provider.eq_ignore_ascii_case("openai official")
                    || route_id.eq_ignore_ascii_case("official")
                    || route_id.eq_ignore_ascii_case("openai-official")
                    || route_id.eq_ignore_ascii_case("openai_official")
                    || route_id.eq_ignore_ascii_case("luna")
                    || vellum_proxy_runtime::official_catalog_is_luna(model)
            };
            if explicitly_official.unwrap_or_else(legacy_official) {
                total_official_turns = total_official_turns.saturating_add(1);
            }
        }
    }
    Ok(total_official_turns)
}

fn write_json(path: &Path, value: &impl Serialize) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_error("create JSON directory"))?;
    }
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| AppError::Message(format!("serialize eval JSON: {error}")))?;
    std::fs::write(path, bytes).map_err(io_error("write eval JSON"))
}

fn eval_codex_program(executable: Option<&Path>) -> String {
    executable
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "codex".into())
}

fn windows_sandbox_codex_assignment(expected_sha256: Option<&str>) -> String {
    match expected_sha256 {
        Some(expected) => {
            let expected = expected.trim_start_matches("sha256:");
            format!(
                r#"# Smart App Control is what refuses a locally built runtime, and it is on
# here for a reason that has nothing to do with this artifact: SAC enables
# itself on a clean install, and every Windows Sandbox boot is one. The host
# is an upgraded machine, so the same binary runs there untouched.
#
# SAC judges a file by whether Microsoft's reputation service already knows
# its signer, so signing locally cannot satisfy it. That was measured, not
# assumed: with a self-signed certificate trusted in both `CurrentUser\Root`
# and `LocalMachine\Root`, the sandbox reported `signature: Valid` and blocked
# the launch anyway, naming policy {{0283ac0f-fff1-49ae-ada1-8a933130cad6}}
# `VerifiedAndReputableDesktop` in its code-integrity log.
#
# So the policy comes off instead. It is scoped to this boot of a VM that is
# destroyed at shutdown, and it is the same trade `ProtectedClient` already
# makes. Nothing else relaxes: outbound traffic stays blocked except to the
# eval gateway. What still proves the bytes under test are the verified ones
# is the SHA-256 recomputed below, over the artifact exactly as it was pinned.
$policyNotes = @()
$sacPolicy = '{{0283ac0f-fff1-49ae-ada1-8a933130cad6}}'
$citool = 'C:\Windows\System32\CiTool.exe'
# `CiTool --remove-policy` is refused for a built-in policy (0x80073BC3);
# deleting the compiled policy and refreshing is what takes it offline.
$cip = "C:\Windows\System32\CodeIntegrity\CiPolicies\Active\$sacPolicy.cip"
if (Test-Path $cip) {{
  try {{ Remove-Item -LiteralPath $cip -Force -ErrorAction Stop; $policyNotes += 'smart app control policy: deleted' }}
  catch {{ $policyNotes += "smart app control policy: $($_.Exception.Message)" }}
}} else {{ $policyNotes += 'smart app control policy: not present' }}
try {{
  Set-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy' -Name VerifiedAndReputablePolicyState -Value 0 -Type DWord -ErrorAction Stop
  $policyNotes += 'smart app control state: set to 0'
}} catch {{ $policyNotes += "smart app control state: $($_.Exception.Message)" }}
if (Test-Path $citool) {{
  # Code integrity reloads policy on demand, so this takes effect without the
  # reboot a sandbox cannot perform. CiTool pauses for a keypress; feed it one.
  try {{ $policyNotes += ('refresh: ' + (('' | & $citool --refresh 2>&1 | Out-String).Trim())) }}
  catch {{ $policyNotes += "refresh: $($_.Exception.Message)" }}
}} else {{ $policyNotes += 'CiTool.exe: missing' }}
try {{
  $policyNotes += ('smart app control now: ' + (Get-ItemPropertyValue 'HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy' -Name VerifiedAndReputablePolicyState -ErrorAction Stop))
}} catch {{ $policyNotes += 'smart app control now: not set' }}
[IO.File]::WriteAllLines('C:\VellumEval\.vellum-current\policy.log', $policyNotes, $utf8)
$mapped = 'C:\VellumEval\.vellum-current\enhanced-codex.exe'
$actual = (Get-FileHash -Algorithm SHA256 $mapped).Hash.ToLower()
if ($actual -ne '{expected}') {{ throw "Enhanced Codex artifact SHA mismatch inside sandbox: expected sha256:{expected}, got sha256:$actual" }}
# Run it from the VM's own disk. Paging a 350 MB image in over the mapped
# folder for every process start is what pushed a case past its wall budget.
$codex = 'C:\enhanced-codex.exe'
Copy-Item -LiteralPath $mapped -Destination $codex -Force"#
            )
        }
        None => {
            r"$codex = 'C:\CodexRuntime\node_modules\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\bin\codex.exe'".into()
        }
    }
}

fn sha256_file(path: &Path) -> AppResult<String> {
    let bytes = std::fs::read(path).map_err(io_error("hash eval file"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn dataset_hashes(paths: &EvalPaths, suite: &SuiteManifest) -> AppResult<HashMap<String, String>> {
    let mut hashes = HashMap::new();
    if suite
        .tasks
        .iter()
        .any(|task| matches!(task.source, TaskSource::HumanEval { .. }))
    {
        let path = paths.dataset_root.join("HumanEval.jsonl.gz");
        hashes.insert("HumanEval.jsonl.gz".into(), sha256_file(&path)?);
    }
    if suite
        .tasks
        .iter()
        .any(|task| matches!(task.source, TaskSource::Mbpp { .. }))
    {
        let path = paths.dataset_root.join("sanitized-mbpp.json");
        hashes.insert("sanitized-mbpp.json".into(), sha256_file(&path)?);
    }
    for task in &suite.tasks {
        if let TaskSource::SweBench {
            instance_id,
            dataset_revision,
            ..
        } = &task.source
        {
            let path = swe_bundle_path(paths, dataset_revision, instance_id);
            hashes.insert(
                format!("swebench/{dataset_revision}/{instance_id}.tar"),
                sha256_file(&path)?,
            );
            let grader_override = swe_grader_override_path(paths, &task.id);
            if grader_override.is_file() {
                hashes.insert(
                    format!("graders/{}.sh", task.id),
                    sha256_file(&grader_override)?,
                );
            }
            if let Some(archive) = &task.grader_image_archive {
                let path = swe_image_archive_path(paths, &archive.sha256);
                if path.is_file() {
                    hashes.insert(
                        format!("swebench/_images/{}.tar.gz", archive.sha256),
                        sha256_file(&path)?,
                    );
                }
            }
        }
    }
    Ok(hashes)
}

async fn git_sha(root: &Path) -> String {
    run_process(
        "git",
        &["rev-parse".into(), "HEAD".into()],
        Some(root),
        None,
        Duration::from_secs(10),
        &[],
    )
    .await
    .ok()
    .filter(|output| output.code == Some(0))
    .map(|output| output.stdout.trim().to_string())
    .unwrap_or_else(|| "unknown".into())
}

fn docker_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn safe_name(value: &str) -> String {
    let mut result = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || character == '-' {
            result.push(character.to_ascii_lowercase());
        } else if !result.ends_with('-') {
            result.push('-');
        }
    }
    result.trim_matches('-').chars().take(80).collect()
}

fn run_id() -> String {
    format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S"),
        std::process::id()
    )
}

fn category_name(category: &TaskCategory) -> &'static str {
    match category {
        TaskCategory::ShortFix => "short-fix",
        TaskCategory::MultiFile => "multi-file",
        TaskCategory::ToolRecovery => "tool-recovery",
        TaskCategory::LongContext => "long-context",
        TaskCategory::ModelSwitch => "model-switch",
        TaskCategory::ProtocolReplay => "protocol-replay",
        TaskCategory::Continuity => "continuity",
        TaskCategory::SweBench => "swe-bench",
    }
}

fn all_resumes_observed(expected_phases: usize, observed_phases: usize) -> bool {
    expected_phases > 0 && observed_phases == expected_phases
}

fn fault_kind_name(kind: &FaultKind) -> &'static str {
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

pub(super) fn switch_targets<'a>(
    task: &EvalTask,
    models: &'a [String],
    current: &'a str,
) -> Vec<&'a str> {
    match task.transition_mode {
        TransitionMode::SameModel => return vec![current],
        TransitionMode::OrderedPair => {
            return models
                .iter()
                .map(String::as_str)
                .filter(|model| *model != current)
                .collect()
        }
        TransitionMode::RoundTrip => {
            if let Some(index) = models.iter().position(|model| model == current) {
                return vec![models[(index + 1) % models.len()].as_str()];
            }
            return vec![current];
        }
        TransitionMode::AllRoundTrips => {
            return models
                .iter()
                .map(String::as_str)
                .filter(|model| *model != current)
                .collect()
        }
        TransitionMode::Auto => {}
    }
    let switches_model = task
        .phases
        .iter()
        .any(|phase| matches!(&phase.model, PhaseModel::Next));
    if switches_model {
        models
            .iter()
            .map(String::as_str)
            .filter(|model| *model != current)
            .collect()
    } else {
        vec![current]
    }
}

pub fn resolve_profile_models(
    state: &AppState,
    profile: &str,
    lane: Option<&str>,
) -> AppResult<Vec<String>> {
    if !matches!(
        profile,
        "desktop-six"
            | "canonical-gate-4"
            | "issue-7-required"
            | "deepseek-luna-roundtrip"
            | "official-solo"
    ) {
        return Err(AppError::Message(format!(
            "unknown eval profile: {profile}"
        )));
    }
    let routes = state.routes();
    let codex_paths = crate::codex::CodexPaths::discover(&state.data_root());
    let official_catalog = crate::catalog::read_official_catalog(&codex_paths.models_cache);
    let models =
        crate::catalog::model_routes_with_official_catalog(&routes, official_catalog.as_ref());
    // Qwen3-Coder-Next is temporarily excluded from the live CLI profile.
    // Keep llama.cpp support in the production proxy; only the evaluator
    // scheduling surface is narrowed while its Codex dialect is stabilized.
    let requirements = if profile == "official-solo" {
        // Single-model profile for a pure `--compaction-engine official-native`
        // leg: `official-native` requires every model in the run to be an
        // Official route (exec.rs), and `--control-model` only samples one
        // task per a fixed category list (sampled_control_tasks) rather than
        // running the full matched task set, so neither existing mechanism
        // fits a first-class Official-only baseline arm.
        vec![("openai", "gpt-5.6-luna")]
    } else if profile == "deepseek-luna-roundtrip" {
        vec![
            ("opencode-go", "deepseek-v4-flash"),
            ("openai", "gpt-5.6-luna"),
        ]
    } else if profile == "issue-7-required" {
        vec![("grok", "grok-4.5"), ("weikuwu", "GLM-5.2")]
    } else if profile == "canonical-gate-4" {
        vec![
            ("openai", "gpt-5.4-mini"),
            ("grok", "grok-4.5"),
            ("weikuwu", "GLM-5.2p"),
            ("weikuwu", "GLM-5.2"),
        ]
    } else {
        vec![
            ("openai", "gpt-5.6-sol"),
            ("grok", "grok-4.5"),
            ("weikuwu", "GLM-5.2p"),
            ("weikuwu", "GLM-5.2"),
            ("nvidia", "z-ai/glm-5.2"),
        ]
    };
    let mut resolved = Vec::with_capacity(requirements.len());
    let mut missing = Vec::new();
    for (kind, upstream) in requirements {
        let found = models.iter().find(|model| {
            if !model.upstream_model.eq_ignore_ascii_case(upstream) {
                return false;
            }
            let Some(route) = routes.iter().find(|route| route.id == model.route_id) else {
                return false;
            };
            if !route.enabled {
                return false;
            }
            match kind {
                "openai" => route.provider_kind == ProviderKind::Official,
                "grok" => route.provider_kind == ProviderKind::GrokCli,
                "weikuwu" => route.name.eq_ignore_ascii_case("weikuwu"),
                "nvidia" => route.base_url.contains("integrate.api.nvidia.com"),
                "opencode-go" => route.name.eq_ignore_ascii_case("OpenCode Go"),
                _ => false,
            }
        });
        if let Some(model) = found {
            resolved.push(model.catalog_id.clone());
        } else {
            missing.push(format!("{kind}:{upstream}"));
        }
    }
    if !missing.is_empty() {
        return Err(AppError::Message(format!(
            "{profile} unavailable: {}",
            missing.join(", ")
        )));
    }
    if let Some(lane) = lane {
        let needle = match lane.to_ascii_lowercase().as_str() {
            "grok" => "grok-4.5",
            "glm" => "GLM-5.2",
            value => {
                return Err(AppError::Message(format!(
                    "unknown desktop-six lane: {value}"
                )))
            }
        };
        resolved.retain(|catalog_id| {
            models.iter().any(|model| {
                model.catalog_id == *catalog_id
                    && (model.upstream_model.eq_ignore_ascii_case(needle)
                        || (lane.eq_ignore_ascii_case("glm")
                            && model
                                .upstream_model
                                .to_ascii_lowercase()
                                .contains("glm-5.2")))
            })
        });
    }
    Ok(resolved)
}

fn eval_case_id(task_id: &str, model: &str, next_model: &str, repetition: u32) -> String {
    let transition = if next_model == model {
        String::new()
    } else {
        format!("--to-{}", safe_name(next_model))
    };
    format!(
        "{}--{}{}--r{}",
        safe_name(task_id),
        safe_name(model),
        transition,
        repetition
    )
}

fn case_work_dir_name(case_id: &str) -> String {
    let digest = Sha256::digest(case_id.as_bytes());
    let short_hash = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("case-{short_hash}")
}

fn transition_label(model: &str, next_model: &str, mode: &TransitionMode) -> String {
    if model == next_model {
        format!("{model} -> {model}")
    } else if matches!(
        mode,
        TransitionMode::RoundTrip | TransitionMode::AllRoundTrips
    ) {
        format!("{model} -> {next_model} -> {model}")
    } else {
        format!("{model} -> {next_model}")
    }
}

fn classify_errors(errors: &[String]) -> &'static str {
    let joined = errors.join(" ").to_ascii_lowercase();
    if joined.contains("429") || joined.contains("quota") || joined.contains("limit exhausted") {
        "provider_service"
    } else if joined.contains("vellum_eval_fault")
        || joined.contains("deterministic eval fault")
        || joined.contains("(http_500)")
        || joined.contains("(http_524)")
        || joined.contains("(http_429)")
    {
        "codex_harness_incompatibility"
    } else if joined.contains("timeout")
        || joined.contains("disconnected")
        || joined.contains("connection")
    {
        "provider_or_transport"
    } else if joined.contains("canonical journal")
        || joined.contains("conversion error")
        || joined.contains("format conversion")
        || joined.contains("deserialize")
        || joined.contains("invalid_id_prefix")
        || joined.contains("encrypted content")
        || joined.contains("unmaterialized compaction")
        || joined.contains("duplicate function definition")
        || joined.contains("tool")
        || joined.contains("arguments")
        || joined.contains("compact")
        || joined.contains("context")
    {
        "vellum_adapter_protocol"
    } else if joined.contains("upstream_status")
        || joined.contains("provider:")
        || joined.contains("badrequesterror")
        || joined.contains("http 500")
        || joined.contains("http 502")
        || joined.contains("http 503")
        || joined.contains("http 504")
        || joined.contains("http 524")
    {
        "provider_service"
    } else if joined.contains("empty completion")
        || joined.contains("model returned")
        || joined.contains("refusal")
    {
        "model_capability"
    } else {
        "indeterminate"
    }
}

fn io_error(action: &'static str) -> impl Fn(std::io::Error) -> AppError {
    move |error| AppError::Message(format!("{action}: {error}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]

    use super::*;

    #[test]
    fn legacy_no_progress_origin_deserializes_as_model_origin() {
        let origin: FailureOrigin =
            serde_json::from_str("\"model_no_progress_after_recovery\"").unwrap();
        assert_eq!(origin, FailureOrigin::Model);
        assert_eq!(serde_json::to_string(&origin).unwrap(), "\"model\"");
    }

    fn signature_test_journal(
        strategy: Option<&str>,
        continuity_kind: Option<&str>,
        checkpoint_schema_version: Option<u32>,
    ) -> crate::history::CompactionJournal {
        crate::history::CompactionJournal {
            engine_id: Some("codex_local_v0_150".to_string()),
            engine_provenance: None,
            route_id: None,
            upstream_model: None,
            tokens_before: None,
            tokens_after: None,
            elapsed_ms: None,
            schema_version: 2,
            compaction_id: "cmp_test".into(),
            kind: crate::history::CompactionJournalKind::Local,
            source_items: Vec::new(),
            canonical_items: Vec::new(),
            portable_items: None,
            created_at: 1,
            trigger: "runtime".into(),
            producer_route: None,
            producer_provider: None,
            producer_model: None,
            canonical_sha256: String::new(),
            canonical_available: false,
            realm_fingerprint: None,
            parent_compaction_id: None,
            generation: 1,
            status: crate::history::CheckpointStatus::Provisional,
            continuation_mode: None,
            continuity_kind: continuity_kind.map(str::to_string),
            strategy: strategy.map(str::to_string),
            reasoning_context_requested: None,
            reasoning_context_effective: None,
            contains_encrypted_reasoning: false,
            reasoning_continuity_verified: false,
            source_head_response_id: None,
            verified_followup_response_id: None,
            input_tokens: None,
            output_tokens: None,
            dropped_message_count: None,
            checkpoint_schema_version,
            source_hash: Some("source-hash".into()),
            checkpoint_hash: Some("checkpoint-hash".into()),
            audit: Some(vellum_proxy_runtime::history::CanonicalAuditRecord {
                schema_version: 2,
                source_hash: "source-hash".into(),
                checkpoint_hash: "checkpoint-hash".into(),
                source_item_count: 0,
                source_tokens: 0,
                checkpoint_tokens: 0,
                source_model_visible_tokens: Some(0),
                summary_model_visible_tokens: Some(0),
                tail_model_visible_tokens: Some(0),
                replacement_model_visible_tokens: Some(0),
                replacement_durable_tokens: Some(0),
                model_visible_compression_ratio: Some(1.0),
                semantic_claim_count: 0,
                grounded_claim_count: 0,
                rejected_claim_count: 0,
                prior_checkpoint_used: false,
                prior_checkpoint_hash: None,
                repeated_sequences_detected: 0,
                repeated_exchanges_collapsed: 0,
                soft_trimmed_outputs: 0,
                hard_cleared_outputs: 0,
                extraction_attempts: 0,
                fallback_used: false,
                fallback_reason: None,
            }),
        }
    }

    #[test]
    fn canonical_v1_rejects_v2_journal_signature() {
        // A V1 request that silently ran V2 must fail, not pass mislabeled as V1.
        let journal = signature_test_journal(Some("canonical_v2"), Some("semantic"), Some(2));
        assert!(!canonical_engine_signature_valid(
            "canonical-v1",
            Some(&journal)
        ));
    }

    #[test]
    fn canonical_v2_rejects_v1_journal_signature() {
        // A V2 request that silently ran V1 must fail, not pass mislabeled as V2.
        let journal = signature_test_journal(Some("canonical_v1"), None, Some(1));
        assert!(!canonical_engine_signature_valid(
            "canonical-v2",
            Some(&journal)
        ));
    }

    #[test]
    fn canonical_alias_requires_v1_signature() {
        let matching = signature_test_journal(Some("canonical_v1"), None, Some(1));
        assert!(canonical_engine_signature_valid(
            "canonical",
            Some(&matching)
        ));
        let mismatched = signature_test_journal(Some("canonical_v2"), Some("semantic"), Some(2));
        assert!(!canonical_engine_signature_valid(
            "canonical",
            Some(&mismatched)
        ));
    }

    #[test]
    fn canonical_v2_requires_audit_signature() {
        let mut journal = signature_test_journal(Some("canonical_v2"), Some("semantic"), Some(2));
        journal.audit = None;
        assert!(!canonical_engine_signature_valid(
            "canonical-v2",
            Some(&journal)
        ));

        let mut missing_hash =
            signature_test_journal(Some("canonical_v2"), Some("semantic"), Some(2));
        missing_hash.checkpoint_hash = None;
        assert!(!canonical_engine_signature_valid(
            "canonical-v2",
            Some(&missing_hash)
        ));
    }

    #[test]
    fn canonical_v2_accepts_a_complete_matching_signature() {
        let journal = signature_test_journal(Some("canonical_v2"), Some("semantic"), Some(2));
        assert!(canonical_engine_signature_valid(
            "canonical-v2",
            Some(&journal)
        ));
    }

    #[test]
    fn canonical_engine_signature_check_is_skipped_for_non_canonical_engines() {
        assert!(canonical_engine_signature_valid("legacy", None));
        assert!(canonical_engine_signature_valid("codex-local", None));
    }

    #[test]
    fn official_native_attribution_is_not_relabelled_by_portable_journal_strategy() {
        assert_eq!(
            observed_compaction_engine("official-native", true, Some("canonical_v1")),
            Some("official-native".into())
        );
        assert_eq!(
            observed_compaction_engine("canonical-v2", true, Some("canonical_v2")),
            Some("canonical_v2".into())
        );
        assert_eq!(
            observed_compaction_engine("official-native", false, Some("canonical_v1")),
            None
        );
    }

    #[test]
    fn eval_codex_local_injection_is_not_named_openai() {
        assert_eq!(
            eval_codex_provider_display_name("codex-local-native-oracle"),
            "Vellum"
        );
        // The native-oracle arm must not have its trigger intercepted, or it
        // would measure Vellum instead of Codex core.
        assert!(!eval_remote_compaction_v2_enabled(
            "codex-local-native-oracle"
        ));
        assert!(eval_remote_compaction_v2_enabled("codex-local-v0-150"));
        assert_eq!(
            eval_codex_provider_display_name("codex-local-v0-150"),
            "OpenAI"
        );
    }

    #[test]
    fn observed_eval_engine_classifies_embedded_oracle_remote_and_mixed() {
        assert_eq!(
            classify_observed_eval_compaction_engine("codex-local-v0-150", true, 1, 1),
            "codex_local_v0_150"
        );
        assert_eq!(
            classify_observed_eval_compaction_engine("codex-local-native-oracle", false, 0, 2),
            "codex_local_native_oracle"
        );
        assert_eq!(
            classify_observed_eval_compaction_engine("codex-local-native-oracle", false, 1, 1),
            "remote_v2"
        );
        assert_eq!(
            classify_observed_eval_compaction_engine("codex-local-v0-150", true, 0, 1),
            "mixed"
        );
        assert!(!compaction_engine_matched(
            "codex-local-native-oracle",
            "remote_v2"
        ));
        assert!(compaction_engine_matched(
            "codex-local-v0-150",
            "codex_local_v0_150"
        ));
        assert!(compaction_engine_matched(
            "codex-local-native-oracle",
            "codex_local_native_oracle"
        ));
        assert!(!compaction_engine_matched("codex-local-v0-150", "mixed"));
        assert_eq!(
            classify_observed_eval_compaction_engine("codex-local-v0-150", false, 0, 0),
            "none"
        );
    }

    #[test]
    fn phase_trigger_treats_codex_local_events_as_compaction_observed() {
        let decision = classify_phase_trigger_decision_with_codex(0, 0, 0, 1, 12_000, 10_000);
        assert_eq!(decision, PhaseTriggerDecision::CompactionObserved);
    }

    #[test]
    fn engine_neutral_window_uses_largest_request_and_last_later_request() {
        let requests = HashMap::from([(1, 2_000), (2, 12_000), (3, 3_000), (4, 4_000)]);
        assert_eq!(
            engine_neutral_compaction_window(&requests),
            (Some(12_000), Some(4_000))
        );
    }

    #[test]
    fn engine_neutral_window_refuses_a_ratio_without_a_later_request() {
        let requests = HashMap::from([(1, 2_000), (2, 12_000)]);
        assert_eq!(
            engine_neutral_compaction_window(&requests),
            (Some(12_000), None)
        );
    }

    #[test]
    fn engine_neutral_ratio_uses_the_observed_gateway_window() {
        assert_eq!(model_visible_ratio(Some(12_000), Some(3_000)), Some(0.25));
        assert_eq!(model_visible_ratio(Some(12_000), None), None);
        assert_eq!(model_visible_ratio(Some(0), Some(3_000)), None);
    }

    #[test]
    fn canonical_engine_signature_fails_closed_without_a_journal() {
        assert!(!canonical_engine_signature_valid("canonical-v1", None));
        assert!(!canonical_engine_signature_valid("canonical-v2", None));
    }

    #[test]
    fn safe_names_are_stable_and_portable() {
        assert_eq!(safe_name("GLM-5.2 / 測試"), "glm-5-2");
    }

    #[test]
    fn windows_sandbox_cleanup_is_scoped_and_stops_children_first() {
        let path = Path::new(r"C:\eval's run\case.wsb");
        let markers = Path::new(r"C:\eval's run\.windows-sandbox-sessions");
        let script = windows_sandbox_session_cleanup_script(path, markers);
        assert!(script.contains("wsb.exe stop --id"));
        assert!(script.contains("*.owned"));
        assert!(script.contains("WindowsSandboxRemoteSession.exe"));
        assert!(script.contains("ParentProcessId -eq $processId"));
        assert!(script.contains("Stop-VellumSandboxTree $_.ProcessId"));
        assert!(script.contains(r"C:\eval''s run\case.wsb"));
        assert!(!script.contains("Get-Process WindowsSandbox"));
    }

    #[test]
    fn windows_sandbox_start_output_requires_an_exact_environment_id() {
        let output = CommandOutput {
            code: Some(0),
            stdout: r#"{"Id":"99ec376c-9722-441a-afbe-23554f5076f3"}"#.into(),
            stderr: String::new(),
            timed_out: false,
            first_byte_ms: Some(10),
        };
        assert_eq!(
            windows_sandbox_id_from_start_output(&output).as_deref(),
            Some("99ec376c-9722-441a-afbe-23554f5076f3")
        );
        let malformed = CommandOutput {
            stdout: r#"{"Id":"not-an-environment"}"#.into(),
            ..output
        };
        assert_eq!(windows_sandbox_id_from_start_output(&malformed), None);
    }

    #[test]
    fn windows_sandbox_exec_timeout_never_starts_a_second_task_sized_wait() {
        assert_eq!(windows_sandbox_result_visibility_budget(true, false), None);
        assert_eq!(windows_sandbox_result_visibility_budget(false, true), None);
        assert_eq!(
            windows_sandbox_result_visibility_budget(false, false),
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn windows_sandbox_model_mounts_exclude_hidden_case_material() {
        let case_root = Path::new(r"C:\eval\case");
        let mappings = windows_sandbox_model_mappings(
            &case_root.join("workspace"),
            &case_root.join("codex-home"),
            &case_root.join(".vellum-current"),
        )
        .join("");

        assert_eq!(mappings.matches("<MappedFolder>").count(), 3);
        assert!(mappings.contains(r"<SandboxFolder>C:\VellumEval\workspace</SandboxFolder>"));
        assert!(mappings.contains(r"<SandboxFolder>C:\VellumEval\codex-home</SandboxFolder>"));
        assert!(mappings.contains(r"<SandboxFolder>C:\VellumEval\.vellum-current</SandboxFolder>"));
        assert!(!mappings.contains(r"<HostFolder>C:\eval\case</HostFolder>"));
        assert!(!mappings.to_ascii_lowercase().contains(r"\hidden"));
        assert!(!mappings.contains(r"<SandboxFolder>C:\VellumEval</SandboxFolder>"));
    }

    #[test]
    fn windows_sandbox_markers_are_owner_scoped_and_stale_cleanup_skips_live_owners() {
        let output = Path::new(r"C:\eval-output");
        let first = windows_sandbox_marker_root(output, "run-one");
        let second = windows_sandbox_marker_root(output, "run-two");
        assert_ne!(first, second);
        assert!(first.starts_with(windows_sandbox_marker_base(output)));

        let script = stale_windows_sandbox_cleanup_script(output);
        assert!(script.contains("owner.pid"));
        assert!(script.contains("Get-Process -Id $ownerPid"));
        assert!(script.contains("if(-not $ownerAlive)"));
        assert!(script.contains("Stop-VellumOwnedMarkers $_.FullName"));
        assert!(!script.contains("WindowsSandboxRemoteSession.exe"));
    }

    #[test]
    fn swe_prompt_surfaces_the_hard_turn_budget_without_task_answers() {
        let prompt = swe_execution_budget_prompt("Fix the reported issue.");
        assert!(prompt.contains("at most 30 model turns"));
        assert!(prompt.contains("minimal correct patch"));
        assert!(prompt.contains("Fix the reported issue."));
    }

    #[test]
    fn rejects_unsafe_swe_archive_paths() {
        assert!(safe_archive_path("workspace/src/lib.py"));
        assert!(!safe_archive_path("../secret"));
        assert!(!safe_archive_path("/absolute"));
    }

    #[test]
    fn generates_every_ordered_target_for_switch_tasks() {
        let paths = EvalPaths::discover(None).unwrap();
        let suite = SuiteManifest::load(&paths.suite_path("core-30")).unwrap();
        let task = suite
            .tasks
            .iter()
            .find(|task| matches!(task.category, TaskCategory::ModelSwitch))
            .unwrap();
        let models = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(switch_targets(task, &models, "a"), vec!["b", "c"]);
        assert_eq!(eval_case_id("switch", "a", "c", 2), "switch--a--to-c--r2");
    }

    #[test]
    fn error_classification_separates_transport_and_quota() {
        assert_eq!(classify_errors(&["HTTP 429".into()]), "provider_service");
        assert_eq!(
            classify_errors(&["stream disconnected".into()]),
            "provider_or_transport"
        );
        assert_eq!(
            classify_errors(&["compaction has no canonical journal entry".into()]),
            "vellum_adapter_protocol"
        );
        assert_eq!(
            classify_errors(&["Provider: nvidia; upstream_status: HTTP 400".into()]),
            "provider_service"
        );
        assert_eq!(
            classify_errors(&["gateway HTTP 524 at /responses (upstream)".into()]),
            "provider_service"
        );
        assert_eq!(
            classify_errors(&["gateway HTTP 500 at /responses (http_500)".into()]),
            "codex_harness_incompatibility"
        );
        assert_eq!(
            classify_errors(&["[invalid_id_prefix] expected fc_".into()]),
            "vellum_adapter_protocol"
        );
        assert_eq!(
            classify_errors(&["Duplicate function definition provided: exec".into()]),
            "vellum_adapter_protocol"
        );
    }

    #[test]
    fn nvidia_doctor_probe_uses_streaming_without_foreign_reasoning_options() {
        let mut body = crate::probe::build_chat_body("OK", "z-ai/glm-5.2");
        assert!(prepare_doctor_probe_body(
            "https://integrate.api.nvidia.com/v1",
            crate::model::WireFormat::Chat,
            "z-ai/glm-5.2",
            &mut body,
        ));
        assert_eq!(body["stream"], true);
        assert!(body.get("chat_template_kwargs").is_none());
        assert!(body.get("reasoning_budget").is_none());
    }

    /// Replaces `eff9409`'s test of the same intent, updated for the
    /// `replace_grok_model_catalog` signature 26460a3 landed (catalog
    /// entries are now validated against `models_cache.json` rather than
    /// carrying an explicit capability list at seed time). Pure and
    /// network-free: this validates catalog id resolution and the endpoint
    /// shape for *both* Grok 4.6 and Grok 4.5 without spending any live
    /// Grok quota — the live-network half of the probe
    /// (`probe_grok_through_runtime`) is exercised only by an explicit
    /// `vellum-eval doctor` run against real credentials, never by this
    /// suite.
    #[test]
    fn grok_doctor_probe_plan_requires_validated_responses_endpoint_for_both_models() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        assert!(state.replace_grok_model_catalog(
            "grok-cli",
            vec!["grok-4.6".into(), "grok-4.5".into()],
            Some("grok-4.6"),
        ));
        let route = state
            .routes()
            .into_iter()
            .find(|route| route.id == "grok-cli")
            .unwrap();

        let (catalog, endpoint) = grok_runtime_probe_plan(&state, &route, "grok-4.6").unwrap();
        assert!(catalog.contains("grok-4-6") || catalog.contains("grok-4.6"));
        assert_eq!(endpoint, "https://cli-chat-proxy.grok.com/v1/responses");
        assert!(!endpoint.contains("/v1/v1"));

        let (catalog, endpoint) = grok_runtime_probe_plan(&state, &route, "grok-4.5").unwrap();
        assert!(catalog.contains("grok-4-5") || catalog.contains("grok-4.5"));
        assert_eq!(endpoint, "https://cli-chat-proxy.grok.com/v1/responses");
        assert!(!endpoint.contains("/v1/v1"));
    }

    /// An unresolvable upstream model must fail closed with a diagnostic
    /// naming the model, never silently probe a different catalog entry.
    #[test]
    fn grok_doctor_probe_plan_fails_closed_for_an_unknown_model() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        assert!(state.replace_grok_model_catalog(
            "grok-cli",
            vec!["grok-4.6".into()],
            Some("grok-4.6"),
        ));
        let route = state
            .routes()
            .into_iter()
            .find(|route| route.id == "grok-cli")
            .unwrap();
        let error = grok_runtime_probe_plan(&state, &route, "grok-9-does-not-exist").unwrap_err();
        assert!(error.contains("grok-9-does-not-exist"), "{error}");
    }

    /// The response-consumption half of `probe_grok_through_runtime`,
    /// exercised with synthetic mock SSE bytes — no network, no live
    /// credentials. Validates the terminal-event detection
    /// (`response.completed`) that decides whether the doctor check passes,
    /// mirroring exactly what a real Grok streaming turn through
    /// `DesktopProxyRuntimeState` → `ProxyRuntime` → `/responses` produces.
    #[tokio::test]
    async fn consume_grok_probe_response_detects_the_terminal_completed_event() {
        let chunks: Vec<Result<axum::body::Bytes, vellum_proxy_runtime::RuntimeError>> = vec![
            Ok(axum::body::Bytes::from_static(
                b"event: response.created\ndata: {}\n\n",
            )),
            Ok(axum::body::Bytes::from_static(
                b"event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n",
            )),
        ];
        let stream: futures_util::stream::BoxStream<
            'static,
            Result<axum::body::Bytes, vellum_proxy_runtime::RuntimeError>,
        > = Box::pin(futures_util::stream::iter(chunks));
        let response = vellum_proxy_runtime::RuntimeResponse::Sse(stream);
        let (status, first_byte_ms, completed) =
            consume_grok_probe_response(response, Instant::now())
                .await
                .expect("a completed stream must not error");
        assert_eq!(status, 200);
        assert!(first_byte_ms.is_some());
        assert!(completed);
    }

    /// A stream that ends without `response.completed` must be reported as a
    /// failure, not silently treated as success (the doctor check must never
    /// pass on an ambiguous/incomplete terminal event).
    #[tokio::test]
    async fn consume_grok_probe_response_fails_closed_without_a_terminal_event() {
        let chunks: Vec<Result<axum::body::Bytes, vellum_proxy_runtime::RuntimeError>> = vec![Ok(
            axum::body::Bytes::from_static(b"event: response.created\ndata: {}\n\n"),
        )];
        let stream: futures_util::stream::BoxStream<
            'static,
            Result<axum::body::Bytes, vellum_proxy_runtime::RuntimeError>,
        > = Box::pin(futures_util::stream::iter(chunks));
        let response = vellum_proxy_runtime::RuntimeResponse::Sse(stream);
        let error = consume_grok_probe_response(response, Instant::now())
            .await
            .unwrap_err();
        assert!(error.contains("without response.completed"), "{error}");
    }

    /// A `response.failed` event is reported as an error whose detail is
    /// bounded (never an unbounded raw body dump, and never a token/email —
    /// only the short SSE fragment itself).
    #[tokio::test]
    async fn consume_grok_probe_response_bounds_a_failed_stream_excerpt() {
        let long_detail = "x".repeat(5_000);
        let payload = format!("event: response.failed\ndata: {long_detail}\n\n");
        let chunks: Vec<Result<axum::body::Bytes, vellum_proxy_runtime::RuntimeError>> =
            vec![Ok(axum::body::Bytes::from(payload))];
        let stream: futures_util::stream::BoxStream<
            'static,
            Result<axum::body::Bytes, vellum_proxy_runtime::RuntimeError>,
        > = Box::pin(futures_util::stream::iter(chunks));
        let response = vellum_proxy_runtime::RuntimeResponse::Sse(stream);
        let error = consume_grok_probe_response(response, Instant::now())
            .await
            .unwrap_err();
        assert!(error.starts_with("stream failed: "));
        assert!(
            error.len() < 400,
            "failure detail must be bounded, got {} chars",
            error.len()
        );
    }

    #[test]
    fn gateway_trace_counts_compaction_and_http_errors() {
        let temp = tempfile::tempdir().unwrap();
        let trace = temp.path().join("trace.jsonl");
        std::fs::write(
            &trace,
            concat!(
                "{\"path\":\"/v1/responses/compact\",\"status\":200,\"fault\":null}\n",
                "{\"path\":\"/v1/responses\",\"status\":429,\"fault\":\"http_429\"}\n"
            ),
        )
        .unwrap();
        let summary = read_gateway_trace(&trace).unwrap();
        assert_eq!(summary.request_count, 2);
        assert_eq!(summary.compaction_requests, 1);
        assert_eq!(summary.valid_compaction_responses, 1);
        assert_eq!(summary.http_errors.len(), 1);
        assert!(summary.http_errors[0].contains("429"));
        assert_eq!(summary.http_error_details[0].request_index, 0);
        assert_eq!(summary.http_error_details[0].fault, "http_429");
    }

    #[test]
    fn declared_gateway_fault_is_not_an_unrecovered_transport_error() {
        let summary = GatewayTraceSummary {
            http_error_details: vec![GatewayHttpError {
                request_index: 2,
                fault: "context_length_exceeded".into(),
                message: "gateway HTTP 400 at /v1/responses (context_length_exceeded)".into(),
                request_total_estimated_tokens: Some(12_000),
            }],
            terminal_request_indices: HashSet::from([3]),
            ..GatewayTraceSummary::default()
        };
        let expected = [FaultSpec {
            at_request: 2,
            kind: FaultKind::ContextLengthExceeded,
            while_total_estimated_tokens_above: None,
        }];
        let observations = classify_gateway_http_faults(&summary, &expected);
        assert!(observations[0].expected_injection);
        assert!(observations[0].recovered);

        let unexpected = [FaultSpec {
            at_request: 3,
            kind: FaultKind::ContextLengthExceeded,
            while_total_estimated_tokens_above: None,
        }];
        assert_eq!(
            classify_gateway_http_faults(&summary, &unexpected)
                .iter()
                .filter(|observation| { !observation.expected_injection || !observation.recovered })
                .count(),
            1
        );
    }

    #[test]
    fn declared_gateway_fault_without_later_terminal_is_unrecovered() {
        let summary = GatewayTraceSummary {
            http_error_details: vec![GatewayHttpError {
                request_index: 2,
                fault: "context_length_exceeded".into(),
                message: "gateway HTTP 400 at /v1/responses (context_length_exceeded)".into(),
                request_total_estimated_tokens: Some(12_000),
            }],
            ..GatewayTraceSummary::default()
        };
        let expected = [FaultSpec {
            at_request: 2,
            kind: FaultKind::ContextLengthExceeded,
            while_total_estimated_tokens_above: Some(10_000),
        }];
        let observations = classify_gateway_http_faults(&summary, &expected);
        assert!(observations[0].expected_injection);
        assert!(!observations[0].recovered);
    }

    #[test]
    fn terminal_observation_survives_client_close_after_completed_event() {
        let temp = tempfile::tempdir().unwrap();
        let trace = temp.path().join("trace.jsonl");
        std::fs::write(
            &trace,
            concat!(
                "{\"event\":\"terminal_observed\",\"requestIndex\":7,\"path\":\"/v1/responses\",\"status\":200,\"terminalSse\":true,\"inputTokens\":120,\"outputTokens\":30}\n",
                "{\"event\":\"stream_dropped\",\"requestIndex\":7,\"path\":\"/v1/responses\",\"status\":200,\"terminalSse\":true,\"inputTokens\":120,\"outputTokens\":30,\"lifecycle\":\"client_closed_after_terminal\"}\n"
            ),
        )
        .unwrap();
        let summary = read_gateway_trace(&trace).unwrap();
        assert_eq!(summary.terminal_streams, 1);
        assert_eq!(summary.missing_terminal_sse, 0);
        assert_eq!(summary.input_tokens, 120);
        assert_eq!(summary.output_tokens, 30);
        assert_eq!(summary.settled_request_count, 1);
        assert_eq!(summary.zero_usage_request_count, 0);
    }

    #[test]
    fn gateway_trace_marks_settled_request_without_usage_as_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let trace = temp.path().join("trace.jsonl");
        std::fs::write(
            &trace,
            "{\"event\":\"stream_end\",\"requestIndex\":3,\"path\":\"/v1/responses\",\"status\":200,\"terminalSse\":true}\n",
        )
        .unwrap();
        let summary = read_gateway_trace(&trace).unwrap();
        assert_eq!(summary.settled_request_count, 1);
        assert_eq!(summary.zero_usage_request_count, 1);
        assert_eq!(summary.input_tokens, 0);
    }

    #[test]
    fn task_efficiency_usage_coverage_excludes_compaction_control_requests() {
        let temp = tempfile::tempdir().unwrap();
        let trace = temp.path().join("trace.jsonl");
        std::fs::write(
            &trace,
            concat!(
                "{\"event\":\"request\",\"requestIndex\":3,\"path\":\"/v1/responses/compact\",\"compaction\":true,\"status\":200}\n",
                "{\"event\":\"stream_end\",\"requestIndex\":3,\"path\":\"/v1/responses/compact\",\"compaction\":true,\"status\":200,\"terminalSse\":true}\n",
                "{\"event\":\"stream_end\",\"requestIndex\":4,\"path\":\"/v1/responses\",\"status\":200,\"terminalSse\":true,\"inputTokens\":120,\"outputTokens\":30}\n"
            ),
        )
        .unwrap();
        let summary = read_gateway_trace(&trace).unwrap();
        assert_eq!(summary.settled_request_count, 1);
        assert_eq!(summary.zero_usage_request_count, 0);
    }

    #[test]
    fn resumed_phase_usage_keeps_cumulative_token_totals() {
        let mut combined = EventMetrics {
            input_tokens: 100,
            output_tokens: 20,
            cached_tokens: 60,
            tool_calls: 2,
            ..EventMetrics::default()
        };
        merge_metrics(
            &mut combined,
            EventMetrics {
                input_tokens: 175,
                output_tokens: 35,
                cached_tokens: 120,
                tool_calls: 3,
                ..EventMetrics::default()
            },
        );
        assert_eq!(combined.input_tokens, 175);
        assert_eq!(combined.output_tokens, 35);
        assert_eq!(combined.cached_tokens, 120);
        assert_eq!(combined.tool_calls, 5);
    }

    #[test]
    fn swe_retry_keeps_cumulative_gateway_usage_without_double_charging() {
        let previous = EventMetrics {
            input_tokens: 100,
            output_tokens: 20,
            compactions: 1,
            tool_calls: 2,
            errors: vec!["first".into()],
            ..EventMetrics::default()
        };
        let mut retry = EventMetrics {
            input_tokens: 175,
            output_tokens: 35,
            compactions: 2,
            tool_calls: 3,
            errors: vec!["first".into(), "second".into()],
            ..EventMetrics::default()
        };
        merge_attempt_metrics(&mut retry, &previous, true);
        assert_eq!(retry.input_tokens, 175);
        assert_eq!(retry.output_tokens, 35);
        assert_eq!(retry.compactions, 2);
        assert_eq!(retry.tool_calls, 5);
        assert_eq!(retry.errors, ["first", "second"]);
    }

    #[test]
    fn resume_observation_is_not_conflated_with_turn_completion() {
        assert!(all_resumes_observed(2, 2));
        assert!(!all_resumes_observed(2, 1));
        assert!(!all_resumes_observed(0, 0));
    }

    #[tokio::test]
    async fn eval_git_normalizes_docker_desktop_mode_and_line_endings() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("fixture.txt"), "baseline\n").unwrap();
        initialize_git(temp.path()).await.unwrap();
        let configured = run_process(
            "git",
            &["config".into(), "--bool".into(), "core.fileMode".into()],
            Some(temp.path()),
            None,
            Duration::from_secs(10),
            &[],
        )
        .await
        .unwrap();
        assert_eq!(configured.code, Some(0));
        assert_eq!(configured.stdout.trim(), "false");
        let autocrlf = run_process(
            "git",
            &["config".into(), "--bool".into(), "core.autocrlf".into()],
            Some(temp.path()),
            None,
            Duration::from_secs(10),
            &[],
        )
        .await
        .unwrap();
        assert_eq!(autocrlf.code, Some(0));
        assert_eq!(autocrlf.stdout.trim(), "true");
        std::fs::write(temp.path().join("generated.txt"), "pinned image artifact\n").unwrap();
        initialize_git(temp.path()).await.unwrap();
        let status = run_process(
            "git",
            &["status".into(), "--short".into()],
            Some(temp.path()),
            None,
            Duration::from_secs(10),
            &[],
        )
        .await
        .unwrap();
        assert_eq!(status.code, Some(0));
        assert!(status.stdout.trim().is_empty());
    }

    #[test]
    fn docker_agent_trusts_only_the_mounted_eval_workspace() {
        let isolation = DockerIsolation {
            network: "eval-network".into(),
            relay: "eval-relay".into(),
            agent_image: "eval-agent".into(),
            run_label: "vellum.eval.run=test".into(),
        };
        let args = isolation.base_args(
            Path::new(r"C:\fixture\workspace"),
            Path::new(r"C:\fixture\codex-home"),
            &crate::eval::manifest::TaskLimits::default(),
            "eval-agent-test",
        );
        assert!(args.contains(&"GIT_CONFIG_COUNT=3".into()));
        assert!(args.contains(&"GIT_CONFIG_KEY_0=safe.directory".into()));
        assert!(args.contains(&"GIT_CONFIG_VALUE_0=/workspace".into()));
        assert!(args.contains(&"GIT_CONFIG_KEY_1=core.fileMode".into()));
        assert!(args.contains(&"GIT_CONFIG_VALUE_1=false".into()));
        assert!(args.contains(&"GIT_CONFIG_KEY_2=core.autocrlf".into()));
        assert!(args.contains(&"GIT_CONFIG_VALUE_2=true".into()));
    }

    #[test]
    fn long_context_fixture_is_large_and_contains_stable_constraints() {
        let temp = tempfile::tempdir().unwrap();
        write_long_context_fixture(temp.path()).unwrap();
        let content = std::fs::read_to_string(temp.path().join("EVAL_CONTEXT.md")).unwrap();
        assert!(content.len() > 38_000);
        assert!(content.contains("Context record 0449"));
        assert!(content.contains("Preserve the established public API"));
        assert!(content.contains("never re-read it after compaction"));
    }

    #[test]
    fn long_context_prompt_refuses_a_truncated_tool_preview() {
        let existing = long_context_execution_prompt("Read EVAL_CONTEXT.md exactly once.");
        assert_eq!(existing.matches("EVAL_CONTEXT.md").count(), 2);
        assert!(existing.contains("max_output_tokens"));
        assert!(existing.contains("at least 12000"));
        assert!(existing.contains("truncation warning does not satisfy"));

        let injected = long_context_execution_prompt("Implement the requested change.");
        assert!(injected.contains("Before editing, read EVAL_CONTEXT.md once"));
        assert!(injected.contains("do not read it again after compaction"));
    }

    #[test]
    fn eval_catalog_filters_models_without_dropping_harness_instructions() {
        let temp = tempfile::tempdir().unwrap();
        let base = json!({
            "models": [
                {
                    "slug": "model-a",
                    "base_instructions": "preserve me",
                    "context_window": 200000,
                    "max_context_window": 200000
                },
                {"slug": "model-b", "base_instructions": "other"}
            ]
        });
        let path = temp.path().join("catalog.json");
        write_eval_catalog(
            &path,
            &["model-a".into()],
            Some(16_000),
            None,
            &base,
            "production",
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["models"].as_array().unwrap().len(), 1);
        assert_eq!(value["models"][0]["base_instructions"], "preserve me");
        assert_eq!(value["models"][0]["context_window"], 16_000);
        assert_eq!(value["models"][0]["auto_compact_token_limit"], 10_666);
        assert_eq!(
            eval_catalog_context_settings(&path, "model-a").unwrap(),
            (16_000, 10_666)
        );
        assert!(value["models"][0].get("comp_hash").is_none());
        assert!(value["models"][0]
            .get("supports_reasoning_summaries")
            .is_none());
    }

    #[test]
    fn windows_eval_trust_is_scoped_to_the_case_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        write_windows_eval_trust(&home, &workspace).unwrap();
        let text = std::fs::read_to_string(home.join("config.toml")).unwrap();
        let parsed = text.parse::<DocumentMut>().unwrap();
        let key = workspace
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            parsed["projects"][key.as_str()]["trust_level"].as_str(),
            Some("trusted")
        );
        assert_eq!(parsed["projects"].as_table_like().unwrap().len(), 1);
    }

    #[test]
    fn windows_permission_preflight_requires_effective_workspace_write() {
        assert!(prompt_reports_workspace_write(
            "Filesystem sandboxing defines which files can be read or written. `sandbox_mode` is `workspace-write`."
        ));
        assert!(!prompt_reports_workspace_write(
            "Filesystem sandboxing defines which files can be read or written. `sandbox_mode` is `read-only`."
        ));
    }

    #[test]
    fn windows_sandbox_mapping_is_escaped_and_read_only_is_explicit() {
        let mapping = wsb_mapping(Path::new(r"C:\eval & fixtures"), r"C:\VellumEval", true);
        assert!(mapping.contains("C:\\eval &amp; fixtures"));
        assert!(mapping.contains("<SandboxFolder>C:\\VellumEval</SandboxFolder>"));
        assert!(mapping.contains("<ReadOnly>true</ReadOnly>"));
    }

    #[test]
    fn powershell_single_quote_keeps_tokens_and_paths_literal() {
        assert_eq!(ps_single_quoted("a'b"), "'a''b'");
    }

    #[test]
    fn legacy_eval_catalog_mutation_is_explicit() {
        let temp = tempfile::tempdir().unwrap();
        let base = json!({"models": [{"slug": "model-a"}]});
        let path = temp.path().join("catalog.json");
        write_eval_catalog(&path, &["model-a".into()], None, None, &base, "legacy").unwrap();
        let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(value["models"][0]["comp_hash"], "eval");
        assert_eq!(value["models"][0]["shell_type"], "shell_command");
    }

    #[test]
    fn forced_compaction_uses_codex_runtime_threshold_keys() {
        let mut args = Vec::new();
        append_forced_compaction_config(&mut args, Some(24_000), None);
        assert!(args.contains(&"model_context_window=24000".into()));
        assert!(args.contains(&"model_auto_compact_token_limit=16000".into()));
        assert!(!args
            .iter()
            .any(|value| value.starts_with("auto_compact_token_limit=")));

        let mut focused_args = Vec::new();
        append_forced_compaction_config(&mut focused_args, Some(24_000), Some(10_000));
        assert!(focused_args.contains(&"model_context_window=24000".into()));
        assert!(focused_args.contains(&"model_auto_compact_token_limit=10000".into()));
    }

    #[test]
    fn reasoning_effort_is_forwarded_without_mapping() {
        let mut args = Vec::new();
        append_reasoning_effort_config(&mut args, Some("xhigh"));
        assert_eq!(args, vec!["-c", "model_reasoning_effort=\"xhigh\""]);
        append_reasoning_effort_config(&mut args, None);
        assert_eq!(args.len(), 2);
    }

    #[test]
    fn swe_shell_grader_is_normalized_to_lf() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("eval.sh");
        std::fs::write(&path, b"#!/bin/bash\r\nset -e\r\necho ok\r\n").unwrap();
        normalize_shell_script(&path).unwrap();
        let bytes = std::fs::read(path).unwrap();
        assert!(!bytes.windows(2).any(|pair| pair == b"\r\n"));
        assert_eq!(bytes, b"#!/bin/bash\nset -e\necho ok\n");
    }

    #[test]
    fn case_workspace_directory_is_bounded_for_long_model_slugs() {
        let id = eval_case_id(
            "ollama-long-stream-tool-continuation",
            "vlm-5fbf078a51-hf-co-unsloth-qwen3-6-35b-a3b-gguf-ud-q4",
            "vlm-5fbf078a51-hf-co-unsloth-qwen3-6-35b-a3b-gguf-ud-q4",
            1,
        );
        let directory = case_work_dir_name(&id);
        assert!(directory.starts_with("case-"));
        assert_eq!(directory.len(), 21);
        assert_eq!(directory, case_work_dir_name(&id));
    }

    #[test]
    fn resume_subcommand_follows_all_exec_options() {
        let mut args = vec![
            "exec".into(),
            "-C".into(),
            "C:\\workspace".into(),
            "--json".into(),
        ];
        append_exec_prompt_args(&mut args, Some("session-123"));
        assert_eq!(
            args,
            vec![
                "exec",
                "-C",
                "C:\\workspace",
                "--json",
                "resume",
                "session-123",
                "-"
            ]
        );
    }

    #[test]
    fn powershell_resume_subcommand_follows_all_exec_options() {
        let mut args = vec!["'exec'".into(), "'-C'".into(), "'C:\\workspace'".into()];
        append_powershell_exec_prompt_args(&mut args, Some("session'123"));
        assert_eq!(
            args,
            vec![
                "'exec'",
                "'-C'",
                "'C:\\workspace'",
                "'resume'",
                "'session''123'",
                "'-'"
            ]
        );
    }

    #[test]
    fn rollout_delta_only_returns_events_written_after_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions/2026/08/03");
        std::fs::create_dir_all(&sessions).unwrap();
        let rollout = sessions.join("rollout-test.jsonl");
        std::fs::write(&rollout, "{\"type\":\"before\"}\n").unwrap();
        let offsets = snapshot_rollout_offsets(temp.path());
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap();
        file.write_all(b"{\"type\":\"after\"}\n").unwrap();
        assert_eq!(
            read_rollout_delta(temp.path(), &offsets),
            "{\"type\":\"after\"}\n"
        );
    }

    #[test]
    fn rollout_compaction_filter_keeps_only_completed_history_rewrites() {
        let jsonl = concat!(
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\"}}\n",
            "{\"type\":\"compacted\",\"payload\":{\"message\":\"summary\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"context_compacted\"}}\n",
        );
        assert_eq!(
            completed_compaction_events(jsonl),
            "{\"type\":\"compacted\",\"payload\":{\"message\":\"summary\"}}\n"
        );
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn process_timeout_does_not_wait_for_inherited_output_pipes() {
        let started = Instant::now();
        let output = run_process(
            "powershell",
            &[
                "-NoProfile".into(),
                "-Command".into(),
                "Start-Sleep -Seconds 5".into(),
            ],
            None,
            None,
            Duration::from_millis(100),
            &[],
        )
        .await
        .unwrap();
        assert!(output.timed_out);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn process_timeout_preserves_output_observed_before_kill() {
        let output = run_process(
            "powershell",
            &[
                "-NoProfile".into(),
                "-Command".into(),
                "[Console]::Out.WriteLine('partial-jsonl'); [Console]::Out.Flush(); Start-Sleep -Seconds 20".into(),
            ],
            None,
            None,
            // Windows process startup can be delayed for several seconds
            // while the all-target Gate is linking other binaries. Keep a
            // generous startup window while the long ping still guarantees
            // this exercises the timeout/kill path rather than normal exit.
            Duration::from_secs(5),
            &[],
        )
        .await
        .unwrap();
        assert!(output.timed_out);
        assert!(output.stdout.contains("partial-jsonl"));
    }

    #[test]
    fn test_lifecycle_guard_drop_marks_aborted() {
        let temp = tempfile::tempdir().unwrap();
        let run_root = temp.path().to_path_buf();
        let record = RunRecord {
            run_id: "test-drop-run".into(),
            status: "running".into(),
            pid: Some(1234),
            suite: "test-suite".into(),
            suite_version: "1.0".into(),
            suite_hash: "hash".into(),
            dataset_hashes: HashMap::new(),
            vellum_git_sha: "sha".into(),
            vellum_version: "0.2.3".into(),
            codex_version: "0.1.0".into(),
            executor: "docker".into(),
            catalog_mode: "desktop".into(),
            executor_platform: "linux-container".into(),
            shell_contract: "linux-bash".into(),
            container_image_id: "img".into(),
            models: vec!["model-a".into()],
            oauth_account: None,
            grok_account: None,
            max_official_turns: None,
            profile: None,
            lane: None,
            transition: None,
            repeat: 1,
            seed: 42,
            max_tasks: None,
            max_wall_seconds: None,
            max_total_tokens: None,
            natural_context: false,
            task_filter: None,
            category_filter: None,
            provider_filter: None,
            tag_filter: None,
            control_model: None,
            baseline_run: None,
            compaction_engine: "canonical-v2".into(),
            recovery_mode: "recover".into(),
            eval_provider_display_name: "OpenAI".into(),
            grok_compaction: "desktop".into(),
            canonical_contract_version: 2,
            schedule: "serial".into(),
            fail_fast: None,
            ablation_profile: None,
            runtime_digest: None,
            enhanced_codex_commit: None,
            enhanced_feature_flags: None,
            enhanced_artifact_sha256: None,
            started_at: "2026-08-24T00:00:00Z".into(),
            completed_at: None,
            task_count: 1,
        };
        {
            let _guard = RunLifecycleGuard::new(run_root.clone(), record);
            // Drop without finalize
        }
        let saved: RunRecord =
            serde_json::from_slice(&std::fs::read(run_root.join("run.json")).unwrap()).unwrap();
        assert_eq!(saved.status, "aborted");
        assert!(saved.completed_at.is_some());
    }

    #[test]
    fn test_lifecycle_guard_finalize_sets_status() {
        let temp = tempfile::tempdir().unwrap();
        let run_root = temp.path().to_path_buf();
        let record = RunRecord {
            run_id: "test-finalize-run".into(),
            status: "running".into(),
            pid: Some(1234),
            suite: "test-suite".into(),
            suite_version: "1.0".into(),
            suite_hash: "hash".into(),
            dataset_hashes: HashMap::new(),
            vellum_git_sha: "sha".into(),
            vellum_version: "0.2.3".into(),
            codex_version: "0.1.0".into(),
            executor: "docker".into(),
            catalog_mode: "desktop".into(),
            executor_platform: "linux-container".into(),
            shell_contract: "linux-bash".into(),
            container_image_id: "img".into(),
            models: vec!["model-a".into()],
            oauth_account: None,
            grok_account: None,
            max_official_turns: None,
            profile: None,
            lane: None,
            transition: None,
            repeat: 1,
            seed: 42,
            max_tasks: None,
            max_wall_seconds: None,
            max_total_tokens: None,
            natural_context: false,
            task_filter: None,
            category_filter: None,
            provider_filter: None,
            tag_filter: None,
            control_model: None,
            baseline_run: None,
            compaction_engine: "canonical-v2".into(),
            recovery_mode: "recover".into(),
            eval_provider_display_name: "OpenAI".into(),
            grok_compaction: "desktop".into(),
            canonical_contract_version: 2,
            schedule: "serial".into(),
            fail_fast: None,
            ablation_profile: None,
            runtime_digest: None,
            enhanced_codex_commit: None,
            enhanced_feature_flags: None,
            enhanced_artifact_sha256: None,
            started_at: "2026-08-24T00:00:00Z".into(),
            completed_at: None,
            task_count: 1,
        };
        {
            let mut guard = RunLifecycleGuard::new(run_root.clone(), record);
            guard.finalize("completed").unwrap();
        }
        let saved: RunRecord =
            serde_json::from_slice(&std::fs::read(run_root.join("run.json")).unwrap()).unwrap();
        assert_eq!(saved.status, "completed");
        assert!(saved.completed_at.is_some());
    }

    #[test]
    fn test_run_record_account_hash_redaction() {
        let raw_email = "developer@example.com";
        let hashed = vellum_proxy_runtime::account_hash(raw_email);
        assert_ne!(raw_email, hashed);
        assert!(hashed.starts_with("sha256:"));
        assert_eq!(hashed.len(), 39);
        assert!(!hashed.contains('@'));
    }

    #[test]
    fn test_copy_dir_recursive_and_grok_profile_staging() {
        let temp_src = tempfile::tempdir().unwrap();
        let src_grok = temp_src.path().join("grok_accounts");
        let src_profile = src_grok.join("profiles").join("acc-123");
        std::fs::create_dir_all(&src_profile).unwrap();
        std::fs::write(
            src_grok.join("accounts.json"),
            r#"{
            "version": 1,
            "accounts": {
                "acc-123": {
                    "account_id": "acc-123",
                    "email": "test@example.test",
                    "authenticated_at": 1,
                    "source": "managed",
                    "profile_key": "acc-123"
                },
                "acc-other": {
                    "account_id": "acc-other",
                    "email": "other@example.test",
                    "authenticated_at": 1,
                    "source": "managed",
                    "profile_key": "acc-other"
                }
            },
            "default_account_id": "acc-123"
        }"#,
        )
        .unwrap();
        std::fs::write(
            src_profile.join("auth.json"),
            r#"{"token": "secret-auth-token"}"#,
        )
        .unwrap();
        let other_profile = src_grok.join("profiles").join("acc-other");
        std::fs::create_dir_all(&other_profile).unwrap();
        std::fs::write(
            other_profile.join("auth.json"),
            r#"{"token": "other-secret"}"#,
        )
        .unwrap();

        let temp_dst = tempfile::tempdir().unwrap();
        let dst_grok = temp_dst.path().join("grok_accounts");

        let manager = crate::grok_accounts::GrokAccountManager::new(temp_src.path().to_path_buf());
        stage_selected_grok_account(&manager, &src_grok, &dst_grok, Some("test@example.test"))
            .unwrap();

        assert!(
            dst_grok.join("accounts.json").is_file(),
            "accounts.json must be copied"
        );
        assert!(
            dst_grok
                .join("profiles")
                .join("acc-123")
                .join("auth.json")
                .is_file(),
            "nested auth.json must be copied recursively"
        );
        assert!(
            !dst_grok.join("profiles").join("acc-other").exists(),
            "unselected Grok profiles must not be staged"
        );
        let auth_content =
            std::fs::read_to_string(dst_grok.join("profiles").join("acc-123").join("auth.json"))
                .unwrap();
        assert_eq!(auth_content, r#"{"token": "secret-auth-token"}"#);
    }

    #[test]
    fn test_official_turns_in_run_and_resume_turn_budget() {
        let temp = tempfile::tempdir().unwrap();
        let run_root = temp.path().to_path_buf();
        let traces_dir = run_root.join("traces");
        std::fs::create_dir_all(&traces_dir).unwrap();

        // Case 1 had 4 requests (2 admitted Official, 2 pre-admission faults).
        let trace1 = [
            json!({"event": "request", "provider": "renamed provider", "routeId": "custom", "officialRoute": true, "status": 200}),
            json!({"event": "request", "provider": "OpenAI Official", "routeId": "openai_official", "officialRoute": true, "status": 200}),
            json!({"event": "request", "provider": "OpenAI Official", "routeId": "openai_official", "officialRoute": true, "status": 429, "fault": "single_run_official_cap"}),
            json!({"event": "request", "provider": "OpenAI Official", "routeId": "openai_official", "officialRoute": true, "status": 500, "fault": "http_500"}),
        ];
        let mut trace1_text = String::new();
        for t in &trace1 {
            trace1_text.push_str(&serde_json::to_string(t).unwrap());
            trace1_text.push('\n');
        }
        std::fs::write(traces_dir.join("case_1.gateway.jsonl"), trace1_text).unwrap();

        // Case 2 had 1 official request and 1 third-party request
        let trace2 = [
            json!({"event": "request", "provider": "Luna", "routeId": "luna", "officialRoute": true, "status": 200}),
            json!({"event": "request", "provider": "Unofficial OpenAI Proxy", "routeId": "unofficial-openai", "officialRoute": false, "status": 200}),
        ];
        let mut trace2_text = String::new();
        for t in &trace2 {
            trace2_text.push_str(&serde_json::to_string(t).unwrap());
            trace2_text.push('\n');
        }
        std::fs::write(traces_dir.join("case_2.gateway.jsonl"), trace2_text).unwrap();

        let spent = official_turns_in_run(&run_root).unwrap();
        assert_eq!(
            spent, 3,
            "must count exactly 3 admitted Official turns and exclude all pre-admission faults"
        );
    }

    #[test]
    fn phase_context_observation_trigger_decisions_cover_all_four_variants() {
        // Limit is 10,000 (2/3 of 15,000)
        let limit = 10_000;

        // 1. Below threshold (5,000 < 10,000)
        let d1 = classify_phase_trigger_decision(0, 0, 5000, limit);
        assert_eq!(d1, PhaseTriggerDecision::BelowThreshold);

        // 2. Compaction observed (requests increased from 0 to 1)
        let d2 = classify_phase_trigger_decision(0, 1, 12000, limit);
        assert_eq!(d2, PhaseTriggerDecision::CompactionObserved);

        // 3. Above threshold without compaction request (11,000 >= 10,000 and requests didn't increase)
        let d3 = classify_phase_trigger_decision(1, 1, 11000, limit);
        assert_eq!(d3, PhaseTriggerDecision::AboveThresholdWithoutRequest);

        // 4. Missing tokens (0 tokens recorded)
        let d4 = classify_phase_trigger_decision(1, 1, 0, limit);
        assert_eq!(d4, PhaseTriggerDecision::UnknownMissingTokens);
    }

    #[test]
    fn mixed_error_attribution_prefers_canonical_compaction_failure_over_non_terminal_429_log() {
        let evidence = TerminalFailureEvidence {
            passed: false,
            protocol_violations: vec!["canonical_journal_invalid".into()],
            compaction_requests: 1,
            valid_compaction_responses: 0,
            request_count: 5,
            timed_out: false,
            verification_timed_out: false,
            http_errors: vec!["429 Too Many Requests (non-fatal warning)".into()],
            transport_protocol_ok: true,
            tool_protocol_ok: true,
            task_acceptance_passed: false,
            classified_errors: None,
            compaction_failure_category: None,
        };

        let origin = classify_failure_origin(&evidence);
        assert_eq!(origin, Some(FailureOrigin::VellumCompaction));

        // Create TaskResult and verify retry is rejected
        let mut result = TaskResult::default();
        result.failure_origin = origin;
        assert!(
            !retryable_provider_failure(&result),
            "Vellum compaction failure must never be retried"
        );
    }

    #[test]
    fn compaction_not_armed_is_classified_as_evaluator_infrastructure_not_vellum_compaction() {
        let evidence = TerminalFailureEvidence {
            passed: false,
            protocol_violations: vec!["compaction_not_armed".into()],
            compaction_requests: 0,
            valid_compaction_responses: 0,
            request_count: 5,
            timed_out: false,
            verification_timed_out: false,
            http_errors: Vec::new(),
            transport_protocol_ok: true,
            tool_protocol_ok: true,
            task_acceptance_passed: false,
            classified_errors: None,
            compaction_failure_category: None,
        };

        let origin = classify_failure_origin(&evidence);
        assert_eq!(origin, Some(FailureOrigin::EvaluatorInfrastructure));
    }

    #[test]
    fn resource_budget_is_distinct_and_provider_failure_keeps_precedence() {
        assert_eq!(
            resolve_failure_origin(false, false, true, false, Some(FailureOrigin::Unknown),),
            Some(FailureOrigin::ResourceBudget)
        );
        assert_eq!(
            resolve_failure_origin(false, false, true, false, Some(FailureOrigin::Provider),),
            Some(FailureOrigin::Provider)
        );
        assert_eq!(
            resolve_failure_origin(
                false,
                false,
                true,
                true,
                Some(FailureOrigin::EvaluatorInfrastructure),
            ),
            Some(FailureOrigin::EvaluatorInfrastructure)
        );
    }

    #[test]
    fn genuine_provider_429_terminal_failure_produces_provider_origin_and_permits_retry() {
        let evidence = TerminalFailureEvidence {
            passed: false,
            protocol_violations: Vec::new(),
            compaction_requests: 0,
            valid_compaction_responses: 0,
            request_count: 2,
            timed_out: false,
            verification_timed_out: false,
            http_errors: vec!["429 Too Many Requests".into()],
            transport_protocol_ok: false,
            tool_protocol_ok: true,
            task_acceptance_passed: false,
            classified_errors: Some("provider_service".into()),
            compaction_failure_category: None,
        };

        let origin = classify_failure_origin(&evidence);
        assert_eq!(origin, Some(FailureOrigin::Provider));

        let mut result = TaskResult::default();
        result.failure_origin = origin;
        result.failure_class = Some("provider_quota".into());
        result.error = Some("429 Too Many Requests".into());
        assert!(retryable_provider_failure(&result));

        // But budget / turn cap failures must NOT be retried
        result.metrics.errors = vec!["swe_turn_cap exceeded".into()];
        assert!(
            !retryable_provider_failure(&result),
            "swe_turn_cap must be denied retry"
        );
    }

    #[test]
    fn compaction_request_failing_with_terminal_provider_429_is_classified_as_provider_and_permits_retry(
    ) {
        let evidence = TerminalFailureEvidence {
            passed: false,
            protocol_violations: Vec::new(),
            compaction_requests: 1,
            valid_compaction_responses: 0,
            request_count: 5,
            timed_out: false,
            verification_timed_out: false,
            http_errors: vec!["gateway HTTP 429 at /v1/responses/compact (upstream)".into()],
            transport_protocol_ok: false,
            tool_protocol_ok: true,
            task_acceptance_passed: false,
            classified_errors: Some("provider_quota".into()),
            compaction_failure_category: None,
        };

        let origin = classify_failure_origin(&evidence);
        assert_eq!(
            origin,
            Some(FailureOrigin::Provider),
            "Compaction request failing with upstream 429 must be classified as Provider"
        );

        let mut result = TaskResult::default();
        result.failure_origin = origin;
        result.failure_class = Some("provider_quota".into());
        result.error = Some("429 Too Many Requests".into());
        assert!(
            retryable_provider_failure(&result),
            "Provider 429 on compaction request must permit retry"
        );
    }

    #[test]
    fn deterministic_http_429_is_evaluator_infrastructure_and_never_retried() {
        let evidence = TerminalFailureEvidence {
            passed: false,
            protocol_violations: Vec::new(),
            compaction_requests: 0,
            valid_compaction_responses: 0,
            request_count: 1,
            timed_out: false,
            verification_timed_out: false,
            http_errors: vec!["gateway HTTP 429 at /v1/responses (http_429)".into()],
            transport_protocol_ok: false,
            tool_protocol_ok: true,
            task_acceptance_passed: false,
            classified_errors: Some("provider_quota".into()),
            compaction_failure_category: None,
        };
        let origin = classify_failure_origin(&evidence);
        assert_eq!(origin, Some(FailureOrigin::EvaluatorInfrastructure));
        let mut result = TaskResult::default();
        result.failure_origin = origin;
        result.failure_class = Some("evaluator_infrastructure".into());
        assert!(!retryable_provider_failure(&result));
    }

    #[test]
    fn terminal_compaction_category_wins_over_unrelated_provider_warning() {
        let evidence = TerminalFailureEvidence {
            passed: false,
            protocol_violations: Vec::new(),
            compaction_requests: 1,
            valid_compaction_responses: 0,
            request_count: 4,
            timed_out: false,
            verification_timed_out: false,
            http_errors: vec!["gateway HTTP 429 at /v1/responses (upstream)".into()],
            transport_protocol_ok: false,
            tool_protocol_ok: true,
            task_acceptance_passed: false,
            classified_errors: Some("provider_quota".into()),
            compaction_failure_category: Some("quality_gate".into()),
        };
        assert_eq!(
            classify_failure_origin(&evidence),
            Some(FailureOrigin::VellumCompaction)
        );
    }

    #[test]
    fn run_resume_requeues_invalid_provider_result_until_latest_attempt_completes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("results.jsonl");
        let mut provider_failure = TaskResult::default();
        provider_failure.case_id = "case-provider".into();
        provider_failure.failure_origin = Some(FailureOrigin::Provider);
        provider_failure.failure_class = Some("provider_quota".into());
        provider_failure.metrics.input_tokens = 100;
        append_jsonl(&path, &provider_failure).unwrap();

        assert!(!completed_case_ids(&path).unwrap().contains("case-provider"));
        assert_eq!(case_attempt_counts(&path).unwrap()["case-provider"], 1);

        let mut passed = provider_failure.clone();
        passed.passed = true;
        passed.failure_origin = None;
        passed.failure_class = None;
        passed.metrics.input_tokens = 40;
        append_jsonl(&path, &passed).unwrap();

        assert!(completed_case_ids(&path).unwrap().contains("case-provider"));
        assert_eq!(tokens_in_results(&path).unwrap(), 140);
    }

    #[test]
    fn run_resume_does_not_bypass_permanent_caps_or_injected_faults() {
        // A bare `swe_token_limit` refusal never reserved or settled any
        // tokens against the case (it is refused before dispatch), so
        // without genuine settled overspend it must stay resumable — this
        // is the common shape of a provider 429 storm tripping the gateway's
        // admission gate transiently.
        let mut transient_gate_trip = TaskResult::default();
        transient_gate_trip.failure_origin = Some(FailureOrigin::EvaluatorInfrastructure);
        transient_gate_trip.metrics.errors = vec!["gateway HTTP 429 (swe_token_limit)".into()];
        transient_gate_trip.metrics.input_tokens = 191_274;
        transient_gate_trip.max_input_tokens = Some(200_000);
        assert!(result_is_resumable(&transient_gate_trip));

        // Genuine settled overspend (the case's real cumulative input tokens
        // exceed its configured budget) remains a permanent, non-resumable
        // cap regardless of which error strings are present.
        let mut capped = TaskResult::default();
        capped.failure_origin = Some(FailureOrigin::Provider);
        capped.failure_class = Some("provider_quota".into());
        capped.metrics.errors = vec!["gateway HTTP 429 (swe_token_limit)".into()];
        capped.metrics.input_tokens = 250_000;
        capped.max_input_tokens = Some(200_000);
        assert!(!result_is_resumable(&capped));

        let mut injected = TaskResult::default();
        injected.failure_origin = Some(FailureOrigin::EvaluatorInfrastructure);
        injected.injected_faults = vec!["http_429".into()];
        assert!(!result_is_resumable(&injected));

        let mut crashed = TaskResult::default();
        crashed.failure_origin = Some(FailureOrigin::EvaluatorInfrastructure);
        assert!(result_is_resumable(&crashed));
    }

    #[test]
    fn run_resume_archives_each_interrupted_attempt_without_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let case_id = "case-provider";
        let case_dir = temp.path().join("cases").join(case_work_dir_name(case_id));
        std::fs::create_dir_all(&case_dir).unwrap();
        std::fs::write(case_dir.join("workspace.txt"), "first").unwrap();
        let traces = temp.path().join("traces");
        std::fs::create_dir_all(&traces).unwrap();
        std::fs::write(traces.join(format!("{case_id}.gateway.jsonl")), "first").unwrap();

        archive_case_outputs_for_resume(temp.path(), case_id, 1).unwrap();
        let archive = temp
            .path()
            .join("attempts")
            .join(case_work_dir_name(case_id));
        assert!(archive.join("attempt-1/case/workspace.txt").is_file());

        std::fs::create_dir_all(&case_dir).unwrap();
        std::fs::write(case_dir.join("workspace.txt"), "interrupted").unwrap();
        archive_case_outputs_for_resume(temp.path(), case_id, 1).unwrap();
        assert!(archive
            .join("attempt-1-interrupted-1/case/workspace.txt")
            .is_file());
    }
}
