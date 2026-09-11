//! The Enhanced Runtime end-to-end integration gate.
//!
//! Two layers, because two different lies are possible.
//!
//! `--mode bridge` runs non-interactively in CI. It starts the real bridge
//! binary against real App Server children and a scripted local provider, and
//! answers: does routing, binding, resume, and every Enhanced port behave, over
//! the wire, the way the design says?
//!
//! `--mode installed` runs before a release. It restarts the actual packaged
//! Codex Desktop through Vellum's managed restart and answers the question the
//! bridge gate cannot: is the app the user launches actually going through this
//! bridge? Anything less than a matching ready attestation is reported as
//! `EnhancedDesktopBridgeNotObserved` — never as "restart succeeded".

pub mod bridge_mode;
pub mod child;
pub mod installed_mode;
pub mod live_mode;
pub mod live_provider;
pub mod preflight;
pub mod provider;
pub mod scenarios;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use vellum_enhanced_codex::hash_identifier;

use crate::error::{AppError, AppResult};

pub const GATE_REPORT_SCHEMA_VERSION: u32 = 1;
const QUALIFICATIONS_DIR: &str = "enhanced-runtime/qualifications";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateMode {
    Bridge,
    Installed,
    /// The bridge gate's runtime with a real provider on the far end.
    Live,
}

impl GateMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "bridge" => Some(Self::Bridge),
            "installed" => Some(Self::Installed),
            "live" => Some(Self::Live),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bridge => "bridge",
            Self::Installed => "installed",
            Self::Live => "live",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaseResult {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

impl CaseResult {
    pub fn new(name: &str, passed: bool, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed,
            detail: detail.into(),
        }
    }
}

/// Identity of a binary the gate exercised. Path and hash only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BinaryIdentity {
    pub path: PathBuf,
    pub sha256: String,
}

impl BinaryIdentity {
    pub fn of(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            sha256: crate::enhanced_runtime::launch_manifest::sha256_file(path)
                .unwrap_or_else(|_| "sha256:unavailable".into()),
        }
    }
}

/// A durable thread binding, with the thread id hashed. The report is written
/// to a shared location, so no conversation identifier leaves in the clear.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedBinding {
    pub thread_id_hash: String,
    pub plane: String,
    pub provider_id: String,
    pub model_id: String,
    pub runtime_digest: String,
}

impl ObservedBinding {
    pub fn from_binding(binding: &crate::enhanced_runtime::ThreadRuntimeBinding) -> Self {
        Self {
            thread_id_hash: hash_identifier(&binding.thread_id),
            plane: binding.plane.as_str().to_string(),
            provider_id: binding.provider_id.clone(),
            model_id: binding.model_id.clone(),
            runtime_digest: binding.runtime_digest.clone(),
        }
    }
}

/// Counters that must be zero. Each one names a way the runtime could look
/// healthy while doing the wrong thing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HardGates {
    pub duplicate_logical_tool_execution: u64,
    pub synthetic_false_success: u64,
    pub unbounded_overflow_retry: u64,
    pub automatic_continuation_over_max: u64,
    pub official_runtime_mutated: u64,
    pub enhanced_fallback_to_official: u64,
    pub remote_compaction_requests: u64,
    pub enhanced_notification_leaked_to_desktop: u64,
    pub legacy_vellum_harness_mutation: u64,
}

impl HardGates {
    pub fn failures(&self) -> Vec<String> {
        let mut failures = Vec::new();
        for (name, count) in [
            (
                "duplicateLogicalToolExecution",
                self.duplicate_logical_tool_execution,
            ),
            ("syntheticFalseSuccess", self.synthetic_false_success),
            ("unboundedOverflowRetry", self.unbounded_overflow_retry),
            (
                "automaticContinuationOverMax",
                self.automatic_continuation_over_max,
            ),
            ("officialRuntimeMutated", self.official_runtime_mutated),
            (
                "enhancedFallbackToOfficial",
                self.enhanced_fallback_to_official,
            ),
            ("remoteCompactionRequests", self.remote_compaction_requests),
            (
                "enhancedNotificationLeakedToDesktop",
                self.enhanced_notification_leaked_to_desktop,
            ),
            (
                "legacyVellumHarnessMutation",
                self.legacy_vellum_harness_mutation,
            ),
        ] {
            if count > 0 {
                failures.push(format!("hard gate {name} = {count}, expected 0"));
            }
        }
        failures
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateReport {
    pub schema_version: u32,
    pub run_id: String,
    pub mode: String,
    pub started_at: i64,
    pub finished_at: i64,
    pub passed: bool,
    pub execution_profile: String,
    pub promotion_eligible: bool,
    pub promotion_ready: bool,
    pub launch_id: Option<String>,
    pub bridge: Option<BinaryIdentity>,
    pub official: Option<BinaryIdentity>,
    pub enhanced: Option<BinaryIdentity>,
    pub attestation_state: Option<String>,
    pub bridge_pid: Option<u32>,
    pub official_child_pid: Option<u32>,
    pub enhanced_child_pid: Option<u32>,
    pub enhanced_identity: Option<crate::enhanced_runtime::EnhancedRuntimeIdentity>,
    /// Number of sessions that reported the effective hook profile from the
    /// Enhanced agent-loop state. Zero means process identity is known but no
    /// session-level application has been observed yet.
    #[serde(default)]
    pub session_features_applied: u64,
    pub bindings: Vec<ObservedBinding>,
    pub enhanced_event_counts: BTreeMap<String, u64>,
    pub provider_requests_by_model: BTreeMap<String, u64>,
    pub cases: Vec<CaseResult>,
    pub hard_gates: HardGates,
    pub failures: Vec<String>,
    /// Which real endpoint answered, when one did. Absent for scripted runs.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub live_endpoint: Option<String>,
    /// Every real exchange, so a reader can check a claim against what the
    /// server actually said rather than take the case detail on trust.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub live_exchanges: Vec<live_provider::LiveExchange>,
}

impl GateReport {
    pub fn new(run_id: String, mode: GateMode, started_at: i64) -> Self {
        Self {
            schema_version: GATE_REPORT_SCHEMA_VERSION,
            run_id,
            mode: mode.as_str().to_string(),
            started_at,
            finished_at: started_at,
            passed: false,
            execution_profile: match mode {
                GateMode::Bridge => "surrogate-children".into(),
                GateMode::Installed => "installed-desktop".into(),
                GateMode::Live => "live-provider".into(),
            },
            promotion_eligible: mode == GateMode::Installed,
            promotion_ready: false,
            launch_id: None,
            bridge: None,
            official: None,
            enhanced: None,
            attestation_state: None,
            bridge_pid: None,
            official_child_pid: None,
            enhanced_child_pid: None,
            enhanced_identity: None,
            session_features_applied: 0,
            bindings: Vec::new(),
            enhanced_event_counts: BTreeMap::new(),
            provider_requests_by_model: BTreeMap::new(),
            cases: Vec::new(),
            hard_gates: HardGates::default(),
            failures: Vec::new(),
            live_endpoint: None,
            live_exchanges: Vec::new(),
        }
    }

    pub fn record(&mut self, case: CaseResult) {
        if !case.passed {
            self.failures
                .push(format!("{}: {}", case.name, case.detail));
        }
        self.cases.push(case);
    }

    pub fn finish(&mut self, finished_at: i64) {
        self.finished_at = finished_at;
        self.failures.extend(self.hard_gates.failures());
        self.passed = self.failures.is_empty() && self.cases.iter().all(|case| case.passed);
        self.promotion_ready = self.passed && self.promotion_eligible;
    }

    /// The report is a release artifact that gets attached to tickets, so the
    /// contents are checked rather than trusted.
    pub fn assert_no_sensitive_content(&self) -> Result<(), String> {
        let encoded = serde_json::to_string(self).map_err(|error| error.to_string())?;
        let lowered = encoded.to_ascii_lowercase();
        for needle in [
            "authorization",
            "api_key",
            "apikey",
            "bearer ",
            "\"prompt\"",
            "tool_output",
            "sk-",
        ] {
            if lowered.contains(needle) {
                return Err(format!("gate report contains `{needle}`"));
            }
        }
        Ok(())
    }
}

pub fn run_root(report_root: Option<&Path>, run_id: &str) -> PathBuf {
    report_root
        .map(Path::to_path_buf)
        .unwrap_or_else(|| crate::state::app_data_dir().join(QUALIFICATIONS_DIR))
        .join(run_id)
}

pub fn write_report(run_root: &Path, report: &GateReport) -> AppResult<PathBuf> {
    report
        .assert_no_sensitive_content()
        .map_err(AppError::Message)?;
    std::fs::create_dir_all(run_root)
        .map_err(|error| AppError::Message(format!("create {}: {error}", run_root.display())))?;
    let path = run_root.join("report.json");
    let bytes = serde_json::to_vec_pretty(report)
        .map_err(|error| AppError::Message(format!("encode gate report: {error}")))?;
    std::fs::write(&path, bytes)
        .map_err(|error| AppError::Message(format!("write {}: {error}", path.display())))?;
    Ok(path)
}

pub fn new_run_id(mode: GateMode) -> String {
    format!("{}-{}", mode.as_str(), ulid::Ulid::new())
}

/// `vellum-eval enhanced-integration-gate --mode bridge|installed`
pub async fn run(args: &[String]) -> AppResult<()> {
    let mode = value_after(args, "--mode")
        .as_deref()
        .and_then(GateMode::parse)
        .ok_or_else(|| {
            AppError::Message("--mode must be `bridge`, `installed` or `live`".into())
        })?;
    let report_root = value_after(args, "--report-root").map(PathBuf::from);
    let run_id = new_run_id(mode);
    let run_root = run_root(report_root.as_deref(), &run_id);
    std::fs::create_dir_all(&run_root)
        .map_err(|error| AppError::Message(format!("create {}: {error}", run_root.display())))?;

    let report = match mode {
        GateMode::Bridge => bridge_mode::run(&run_id, &run_root, args).await?,
        GateMode::Installed => installed_mode::run(&run_id, &run_root, args).await?,
        GateMode::Live => live_mode::run(&run_id, &run_root, args).await?,
    };
    let path = write_report(&run_root, &report)?;

    for case in &report.cases {
        println!(
            "{} {:<40} {}",
            if case.passed { "PASS" } else { "FAIL" },
            case.name,
            case.detail
        );
    }
    println!("Report: {}", path.display());
    if report.promotion_ready {
        println!("Enhanced integration gate ({}): GO", report.mode);
        Ok(())
    } else if report.passed {
        println!(
            "Enhanced integration gate ({}): COMPONENT PASS; promotion NO-GO ({})",
            report.mode, report.execution_profile
        );
        Ok(())
    } else {
        for failure in &report.failures {
            println!("  - {failure}");
        }
        Err(AppError::Message(format!(
            "enhanced integration gate ({}) failed with {} blocker(s); report: {}",
            report.mode,
            report.failures.len(),
            path.display()
        )))
    }
}

pub(crate) fn value_after(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|argument| argument == flag)
        .and_then(|index| args.get(index + 1))
        .filter(|value| !value.starts_with("--"))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse_and_unknown_modes_are_refused() {
        assert_eq!(GateMode::parse("bridge"), Some(GateMode::Bridge));
        assert_eq!(GateMode::parse("Installed"), Some(GateMode::Installed));
        assert_eq!(GateMode::parse("desktop"), None);
    }

    #[test]
    fn a_failing_hard_gate_fails_the_report_even_when_every_case_passed() {
        let mut report = GateReport::new("run-1".into(), GateMode::Bridge, 1);
        report.record(CaseResult::new("routing", true, "ok"));
        report.hard_gates.duplicate_logical_tool_execution = 1;
        report.finish(2);
        assert!(!report.passed);
        assert!(report.failures[0].contains("duplicateLogicalToolExecution"));
    }

    #[test]
    fn surrogate_bridge_pass_is_not_promotion_ready() {
        let mut report = GateReport::new("run-1".into(), GateMode::Bridge, 1);
        report.record(CaseResult::new("routing", true, "ok"));
        report.finish(2);
        assert!(report.passed);
        assert!(!report.promotion_eligible);
        assert!(!report.promotion_ready);
    }

    #[test]
    fn installed_pass_is_promotion_ready() {
        let mut report = GateReport::new("run-1".into(), GateMode::Installed, 1);
        report.record(CaseResult::new("adoption", true, "ok"));
        report.finish(2);
        assert!(report.passed);
        assert!(report.promotion_eligible);
        assert!(report.promotion_ready);
    }

    #[test]
    fn thread_identifiers_are_hashed_before_they_reach_the_report() {
        let binding = crate::enhanced_runtime::ThreadRuntimeBinding::new(
            "thread-secret",
            crate::enhanced_runtime::ExecutionPlane::EnhancedCodex,
            "sha256:digest",
            "qwen",
            "qwen3-coder",
            "native-1",
            1,
        );
        let observed = ObservedBinding::from_binding(&binding);
        assert!(observed.thread_id_hash.starts_with("sha256:"));
        assert!(!observed.thread_id_hash.contains("thread-secret"));
    }

    #[test]
    fn a_report_carrying_an_authorization_header_is_refused() {
        let mut report = GateReport::new("run-1".into(), GateMode::Bridge, 1);
        report.record(CaseResult::new(
            "leak",
            true,
            "Authorization: Bearer abc123",
        ));
        report.finish(2);
        assert!(report.assert_no_sensitive_content().is_err());
    }
}
