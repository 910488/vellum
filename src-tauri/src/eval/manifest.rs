use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::Component;
use std::path::Path;

use super::artifacts::{is_archive_local_tag, is_immutable_oci_reference};
use crate::error::{AppError, AppResult};

fn default_repeat() -> u32 {
    1
}

fn default_timeout() -> u64 {
    900
}

fn default_cpu() -> f32 {
    2.0
}

fn default_memory() -> u64 {
    4096
}

fn default_pids() -> u32 {
    256
}

fn default_disk() -> u64 {
    1024
}

fn default_input_tokens() -> u64 {
    200_000
}

fn default_output_tokens() -> u64 {
    30_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuiteManifest {
    pub schema_version: u32,
    pub name: String,
    pub version: String,
    #[serde(default = "default_repeat")]
    pub default_repeat: u32,
    /// Explicit Enhanced Codex ablation profile. Never inferred from a model id.
    #[serde(default)]
    pub ablation_profile: Option<String>,
    pub tasks: Vec<EvalTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskCategory {
    ShortFix,
    MultiFile,
    ToolRecovery,
    LongContext,
    ModelSwitch,
    ProtocolReplay,
    Continuity,
    SweBench,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskSource {
    HumanEval {
        task_id: String,
    },
    Mbpp {
        task_id: String,
        #[serde(default)]
        multi_file: bool,
    },
    Fixture {
        path: String,
    },
    Git {
        repository: String,
        commit: String,
        #[serde(default)]
        subdirectory: Option<String>,
    },
    SweBench {
        instance_id: String,
        dataset_revision: String,
        repository: String,
        commit: String,
        image_digest: String,
        bundle_sha256: String,
        bundle_url: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalTask {
    pub id: String,
    pub category: TaskCategory,
    #[serde(default)]
    pub tags: Vec<String>,
    pub source: TaskSource,
    pub prompt: String,
    /// Optional Codex reasoning effort for the initial turn. The evaluator
    /// passes the exact value through `model_reasoning_effort`; it never maps
    /// or silently downgrades provider effort names.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub setup: Vec<String>,
    pub verification: Vec<String>,
    #[serde(default)]
    pub phases: Vec<TaskPhase>,
    #[serde(default)]
    pub faults: Vec<FaultSpec>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub forced_context_window: Option<u64>,
    /// Optional eval-only scheduler threshold. This lets a focused compaction
    /// gate keep a realistic model context window while deterministically
    /// arming compaction before an efficient model finishes the task.
    #[serde(default)]
    pub forced_auto_compact_token_limit: Option<u64>,
    #[serde(default)]
    pub expected_harness: ExpectedHarnessEvents,
    #[serde(default)]
    pub grader_image: Option<String>,
    /// Optional GitHub Release image archive. A SWE task must use exactly one
    /// of: an immutable OCI `graderImage`, or this archive plus a
    /// config-digest local tag. The two modes must not be combined.
    #[serde(default)]
    pub grader_image_archive: Option<GraderImageArchive>,
    #[serde(default)]
    pub continuity_checkpoints: Vec<ContinuityCheckpoint>,
    #[serde(default)]
    pub transition_mode: TransitionMode,
    #[serde(default)]
    pub limits: TaskLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GraderImageArchive {
    pub url: String,
    pub sha256: String,
    pub compression: ArchiveCompression,
    pub size_bytes: u64,
    pub uncompressed_size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ArchiveCompression {
    Gzip,
}

impl GraderImageArchive {
    fn validate(&self, task_id: &str) -> AppResult<()> {
        if !self.url.starts_with("https://") {
            return Err(AppError::Message(format!(
                "eval task {task_id} graderImageArchive.url must be HTTPS"
            )));
        }
        if self.sha256.len() != 64
            || !self
                .sha256
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        {
            return Err(AppError::Message(format!(
                "eval task {task_id} graderImageArchive.sha256 must be 64 hex characters"
            )));
        }
        if self.size_bytes == 0 || self.uncompressed_size_bytes == 0 {
            return Err(AppError::Message(format!(
                "eval task {task_id} graderImageArchive sizes must be positive"
            )));
        }
        if self.uncompressed_size_bytes < self.size_bytes {
            return Err(AppError::Message(format!(
                "eval task {task_id} graderImageArchive uncompressed size is smaller than the compressed archive"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweGraderMode<'a> {
    Oci {
        image: &'a str,
        config_digest: &'a str,
    },
    Archive {
        tag: &'a str,
        archive: &'a GraderImageArchive,
        config_digest: &'a str,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectedHarnessEvents {
    #[serde(default)]
    pub min_tool_calls: u64,
    #[serde(default)]
    pub min_compactions: u64,
    #[serde(default)]
    pub require_terminal_sse: bool,
    #[serde(default)]
    pub require_session_resume: bool,
    #[serde(default)]
    pub require_model_switch: bool,
    /// Minimum counts for observed `vellum/enhancedEvent` notification names.
    /// The evaluator never treats a requested ablation profile as evidence.
    #[serde(default)]
    pub min_enhanced_events: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuityCheckpoint {
    pub path: String,
    pub contains: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionMode {
    #[default]
    Auto,
    SameModel,
    OrderedPair,
    RoundTrip,
    AllRoundTrips,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskPhase {
    pub prompt: String,
    #[serde(default)]
    pub model: PhaseModel,
    /// Per-phase override used by model-switch continuation tests.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseModel {
    #[default]
    Current,
    Next,
    Named(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskLimits {
    #[serde(default = "default_cpu")]
    pub cpu: f32,
    #[serde(default = "default_memory")]
    pub memory_mb: u64,
    #[serde(default = "default_pids")]
    pub pids: u32,
    #[serde(default = "default_disk")]
    pub disk_mb: u64,
    #[serde(default = "default_input_tokens")]
    pub max_input_tokens: u64,
    #[serde(default = "default_output_tokens")]
    pub max_output_tokens: u64,
}

impl Default for TaskLimits {
    fn default() -> Self {
        Self {
            cpu: default_cpu(),
            memory_mb: default_memory(),
            pids: default_pids(),
            disk_mb: default_disk(),
            max_input_tokens: default_input_tokens(),
            max_output_tokens: default_output_tokens(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultKind {
    Http429,
    Http500,
    Http524,
    MalformedJson,
    CompactEmpty,
    CompactNonJson,
    TruncateSse,
    DropCompleted,
    DuplicateDelta,
    FragmentSse,
    /// Replay one completed function-call SSE item with the exact same call
    /// id/name/arguments so the real agent loop must suppress the duplicate.
    ReplayToolCall,
    /// Return a deterministic context-window rejection. Enhanced context
    /// recovery may prune once and retry the same provider turn.
    ContextLengthExceeded,
    /// A valid provider stop with no further output. Used after a successful
    /// `update_plan` call to test whether pending deterministic work receives
    /// one bounded continuation turn.
    NormalStop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FaultSpec {
    pub at_request: u32,
    pub kind: FaultKind,
    /// Eval-only persistent rejection rule. Starting at atRequest, every
    /// request above this total estimate receives the configured fault.
    #[serde(default)]
    pub while_total_estimated_tokens_above: Option<u64>,
}

impl SuiteManifest {
    pub fn load(path: &Path) -> AppResult<Self> {
        let bytes = std::fs::read(path).map_err(|error| {
            AppError::Message(format!(
                "cannot read eval suite {}: {error}",
                path.display()
            ))
        })?;
        let suite: Self = serde_json::from_slice(&bytes).map_err(|error| {
            AppError::Message(format!("invalid eval suite {}: {error}", path.display()))
        })?;
        suite.validate()?;
        Ok(suite)
    }

    pub fn validate(&self) -> AppResult<()> {
        if self.schema_version != 1 {
            return Err(AppError::Message(format!(
                "unsupported eval suite schema version {}",
                self.schema_version
            )));
        }
        if !safe_identifier(&self.name) || self.version.trim().is_empty() {
            return Err(AppError::Message(
                "eval suite name and version are required".into(),
            ));
        }
        if self.tasks.is_empty() {
            return Err(AppError::Message("eval suite has no tasks".into()));
        }
        let mut ids = HashSet::new();
        for task in &self.tasks {
            task.validate()?;
            if !ids.insert(task.id.to_ascii_lowercase()) {
                return Err(AppError::Message(format!(
                    "duplicate eval task id: {}",
                    task.id
                )));
            }
        }
        Ok(())
    }
}

impl EvalTask {
    fn validate(&self) -> AppResult<()> {
        if !safe_identifier(&self.id) || self.prompt.trim().is_empty() {
            return Err(AppError::Message(
                "eval task id and prompt are required".into(),
            ));
        }
        if self.verification.is_empty()
            || self.verification.iter().any(|item| item.trim().is_empty())
        {
            return Err(AppError::Message(format!(
                "eval task {} requires non-empty verification commands",
                self.id
            )));
        }
        if self
            .expected_harness
            .min_enhanced_events
            .iter()
            .any(|(name, count)| {
                !name.starts_with("enhanced.") || name.trim().len() != name.len() || *count == 0
            })
        {
            return Err(AppError::Message(format!(
                "eval task {} has an invalid minEnhancedEvents entry",
                self.id
            )));
        }
        if self.timeout_seconds == 0
            || self.limits.cpu <= 0.0
            || self.limits.memory_mb < 128
            || self.limits.pids == 0
            || self.limits.disk_mb < 16
            || self.limits.max_input_tokens == 0
            || self.limits.max_output_tokens == 0
        {
            return Err(AppError::Message(format!(
                "eval task {} has invalid resource limits",
                self.id
            )));
        }
        if let Some(limit) = self.forced_auto_compact_token_limit {
            if limit == 0
                || self
                    .forced_context_window
                    .is_none_or(|context_window| limit >= context_window)
            {
                return Err(AppError::Message(format!(
                    "eval task {} requires forcedAutoCompactTokenLimit to be positive and smaller than forcedContextWindow",
                    self.id
                )));
            }
        }
        if self.tags.iter().any(|tag| !safe_identifier(tag)) {
            return Err(AppError::Message(format!(
                "eval task {} contains an unsafe tag",
                self.id
            )));
        }
        if self.continuity_checkpoints.iter().any(|checkpoint| {
            !safe_relative_path(&checkpoint.path) || checkpoint.contains.trim().is_empty()
        }) {
            return Err(AppError::Message(format!(
                "eval task {} contains an invalid continuity checkpoint",
                self.id
            )));
        }
        if self
            .grader_image
            .as_deref()
            .is_some_and(|image| image.trim().is_empty() || image.chars().any(char::is_whitespace))
        {
            return Err(AppError::Message(format!(
                "eval task {} contains an invalid grader image",
                self.id
            )));
        }
        let mut requests = HashSet::new();
        for fault in &self.faults {
            if fault.at_request == 0 || !requests.insert(fault.at_request) {
                return Err(AppError::Message(format!(
                    "eval task {} has invalid or duplicate fault request index",
                    self.id
                )));
            }
            if fault
                .while_total_estimated_tokens_above
                .is_some_and(|limit| limit == 0)
                || (fault.while_total_estimated_tokens_above.is_some()
                    && !matches!(fault.kind, FaultKind::ContextLengthExceeded))
            {
                return Err(AppError::Message(format!(
                    "eval task {} has an invalid persistent context fault rule",
                    self.id
                )));
            }
        }
        match &self.source {
            TaskSource::HumanEval { task_id } | TaskSource::Mbpp { task_id, .. }
                if task_id.trim().is_empty() =>
            {
                Err(AppError::Message(format!(
                    "eval task {} has an empty dataset task id",
                    self.id
                )))
            }
            TaskSource::Fixture { path } if !safe_relative_path(path) => {
                Err(AppError::Message(format!(
                    "eval task {} has an unsafe fixture path",
                    self.id
                )))
            }
            TaskSource::Git {
                repository,
                commit,
                subdirectory,
            } if !(repository.starts_with("https://")
                && commit.len() == 40
                && commit.chars().all(|character| character.is_ascii_hexdigit())
                && subdirectory
                    .as_deref()
                    .map(safe_relative_path)
                    .unwrap_or(true)) =>
            {
                Err(AppError::Message(format!(
                    "eval task {} requires an HTTPS repository, full commit SHA, and safe subdirectory",
                    self.id
                )))
            }
            TaskSource::SweBench {
                instance_id,
                dataset_revision,
                repository,
                commit,
                image_digest,
                bundle_sha256,
                bundle_url,
            } => {
                if !safe_identifier(instance_id)
                    || !safe_identifier(dataset_revision)
                    || !repository.starts_with("https://")
                    || commit.len() != 40
                    || !commit
                        .chars()
                        .all(|character| character.is_ascii_hexdigit())
                    || !image_digest.starts_with("sha256:")
                    || image_digest.len() != 71
                    || !image_digest[7..]
                        .chars()
                        .all(|character| character.is_ascii_hexdigit())
                    || bundle_sha256.len() != 64
                    || !bundle_sha256
                        .chars()
                        .all(|character| character.is_ascii_hexdigit())
                    || !bundle_url.starts_with("https://")
                {
                    return Err(AppError::Message(format!(
                        "eval task {} contains invalid pinned SWE-bench metadata",
                        self.id
                    )));
                }
                self.swe_grader_mode().map(|_| ())
            }
            _ => Ok(()),
        }
    }

    pub fn swe_grader_mode(&self) -> AppResult<SweGraderMode<'_>> {
        let TaskSource::SweBench { image_digest, .. } = &self.source else {
            return Err(AppError::Message(format!(
                "eval task {} is not a SWE-bench task",
                self.id
            )));
        };
        let oci = self
            .grader_image
            .as_deref()
            .filter(|image| is_immutable_oci_reference(image));
        match (oci, self.grader_image_archive.as_ref()) {
            (Some(_), Some(_)) => Err(AppError::Message(format!(
                "eval task {} must not combine an OCI graderImage with graderImageArchive",
                self.id
            ))),
            (Some(image), None) => Ok(SweGraderMode::Oci {
                image,
                config_digest: image_digest,
            }),
            (None, Some(archive)) => {
                archive.validate(&self.id)?;
                let Some(tag) = self.grader_image.as_deref() else {
                    return Err(AppError::Message(format!(
                        "eval task {} archive mode requires a config-digest local graderImage tag",
                        self.id
                    )));
                };
                if tag.ends_with(":latest") || tag.contains(":latest@") {
                    return Err(AppError::Message(format!(
                        "eval task {} forbids :latest; archive graderImage must be a cfg-<digest> local tag",
                        self.id
                    )));
                }
                if !is_archive_local_tag(tag, image_digest) {
                    return Err(AppError::Message(format!(
                        "eval task {} graderImage must be a safe local tag ending in :cfg-<12 hex of image_digest>",
                        self.id
                    )));
                }
                Ok(SweGraderMode::Archive {
                    tag,
                    archive,
                    config_digest: image_digest,
                })
            }
            (None, None) => Err(AppError::Message(format!(
                "eval task {} requires an immutable OCI graderImage or a complete graderImageArchive",
                self.id
            ))),
        }
    }
}

fn safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

fn safe_relative_path(value: &str) -> bool {
    let path = Path::new(value);
    !value.trim().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_suite() -> SuiteManifest {
        SuiteManifest {
            schema_version: 1,
            name: "smoke".into(),
            version: "1".into(),
            default_repeat: 1,
            ablation_profile: None,
            tasks: vec![EvalTask {
                id: "task-1".into(),
                category: TaskCategory::ShortFix,
                tags: vec!["smoke".into()],
                source: TaskSource::HumanEval {
                    task_id: "HumanEval/0".into(),
                },
                prompt: "Implement the function.".into(),
                reasoning_effort: None,
                setup: Vec::new(),
                verification: vec!["python /hidden/grader.py".into()],
                phases: Vec::new(),
                faults: Vec::new(),
                timeout_seconds: 30,
                forced_context_window: None,
                forced_auto_compact_token_limit: None,
                expected_harness: ExpectedHarnessEvents::default(),
                grader_image: None,
                grader_image_archive: None,
                continuity_checkpoints: Vec::new(),
                transition_mode: TransitionMode::Auto,
                limits: TaskLimits::default(),
            }],
        }
    }

    #[test]
    fn accepts_valid_manifest() {
        valid_suite().validate().unwrap();
    }

    #[test]
    fn rejects_duplicate_ids() {
        let mut suite = valid_suite();
        suite.tasks.push(suite.tasks[0].clone());
        assert!(suite
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate"));
    }

    #[test]
    fn rejects_duplicate_fault_indices() {
        let mut suite = valid_suite();
        suite.tasks[0].faults = vec![
            FaultSpec {
                at_request: 1,
                kind: FaultKind::Http429,
                while_total_estimated_tokens_above: None,
            },
            FaultSpec {
                at_request: 1,
                kind: FaultKind::Http500,
                while_total_estimated_tokens_above: None,
            },
        ];
        assert!(suite.validate().is_err());
    }

    #[test]
    fn validates_persistent_context_fault_rules() {
        let mut suite = valid_suite();
        suite.tasks[0].faults = vec![FaultSpec {
            at_request: 2,
            kind: FaultKind::ContextLengthExceeded,
            while_total_estimated_tokens_above: Some(10_000),
        }];
        suite.validate().unwrap();

        suite.tasks[0].faults[0].while_total_estimated_tokens_above = Some(0);
        assert!(suite.validate().is_err());

        suite.tasks[0].faults[0].while_total_estimated_tokens_above = Some(10_000);
        suite.tasks[0].faults[0].kind = FaultKind::Http429;
        assert!(suite.validate().is_err());
    }

    #[test]
    fn rejects_paths_that_escape_eval_roots() {
        let mut suite = valid_suite();
        suite.tasks[0].id = "../escape".into();
        assert!(suite.validate().is_err());

        let mut suite = valid_suite();
        suite.tasks[0].source = TaskSource::Fixture {
            path: "../../secret".into(),
        };
        assert!(suite.validate().is_err());
    }

    fn valid_swe_task() -> EvalTask {
        EvalTask {
            id: "swe-psf__requests-1142".into(),
            category: TaskCategory::SweBench,
            tags: vec!["swe-bench".into()],
            source: TaskSource::SweBench {
                instance_id: "psf__requests-1142".into(),
                dataset_revision: "verified-parquet-a45b1fe4".into(),
                repository: "https://github.com/psf/requests.git".into(),
                commit: "22623bd8c265b78b161542663ee980738441c307".into(),
                image_digest: "sha256:64ac3483a165db2706f67b59e064095d0646e4db397d1573443b307c45763d1b"
                    .into(),
                bundle_sha256: "073c9c6b4957da5aaad21f24d1e5666da00a1e707eeb6e848536a0d9f635d929"
                    .into(),
                bundle_url: "https://github.com/910488/vellum/releases/download/swebench-grader-v1/psf__requests-1142.tar".into(),
            },
            prompt: "Fix the request.".into(),
            reasoning_effort: None,
            setup: Vec::new(),
            verification: vec!["bash /hidden/eval.sh".into()],
            phases: Vec::new(),
            faults: Vec::new(),
            timeout_seconds: 2700,
            forced_context_window: None,
            forced_auto_compact_token_limit: None,
            expected_harness: ExpectedHarnessEvents::default(),
            grader_image: Some(
                "ghcr.io/910488/vellum/swe-grader-psf-requests-1142@sha256:3f35c591fdd9e120548fb9aa14cc1d1bc572e87385b5ee8eb32e7f506dfb4313".into(),
            ),
            grader_image_archive: None,
            continuity_checkpoints: Vec::new(),
            transition_mode: TransitionMode::Auto,
            limits: TaskLimits::default(),
        }
    }

    fn valid_archive() -> GraderImageArchive {
        GraderImageArchive {
            url: "https://github.com/910488/vellum/releases/download/swebench-verified12-a45b1fe4-deadbeef/psf__requests-1142.image.tar.gz".into(),
            sha256: "aa".repeat(32),
            compression: ArchiveCompression::Gzip,
            size_bytes: 100,
            uncompressed_size_bytes: 200,
        }
    }

    #[test]
    fn swe_task_accepts_oci_or_archive_but_not_both_or_neither() {
        let mut suite = valid_suite();
        suite.tasks[0] = valid_swe_task();
        suite.validate().unwrap();

        suite.tasks[0].grader_image = None;
        assert!(suite
            .validate()
            .unwrap_err()
            .to_string()
            .contains("immutable OCI graderImage or a complete graderImageArchive"));

        suite.tasks[0].grader_image = Some("sweb.eval.x86_64.psf__requests-1142:latest".into());
        assert!(suite
            .validate()
            .unwrap_err()
            .to_string()
            .contains("immutable OCI graderImage or a complete graderImageArchive"));

        suite.tasks[0] = valid_swe_task();
        suite.tasks[0].grader_image_archive = Some(valid_archive());
        assert!(suite
            .validate()
            .unwrap_err()
            .to_string()
            .contains("must not combine"));

        suite.tasks[0].grader_image =
            Some("sweb.eval.x86_64.psf__requests-1142:cfg-64ac3483a165".into());
        suite.validate().unwrap();
    }

    #[test]
    fn archive_mode_rejects_latest_bad_hash_and_invalid_sizes() {
        let mut suite = valid_suite();
        suite.tasks[0] = valid_swe_task();
        suite.tasks[0].grader_image = Some("sweb.eval.x86_64.psf__requests-1142:latest".into());
        suite.tasks[0].grader_image_archive = Some(valid_archive());
        assert!(suite
            .validate()
            .unwrap_err()
            .to_string()
            .contains(":latest"));

        suite.tasks[0].grader_image =
            Some("sweb.eval.x86_64.psf__requests-1142:cfg-ffffffffffff".into());
        assert!(suite
            .validate()
            .unwrap_err()
            .to_string()
            .contains("cfg-<12 hex"));

        suite.tasks[0].grader_image =
            Some("sweb.eval.x86_64.psf__requests-1142:cfg-64ac3483a165".into());
        suite.tasks[0].grader_image_archive = Some(GraderImageArchive {
            url: "http://example.invalid/image.tar.gz".into(),
            ..valid_archive()
        });
        assert!(suite.validate().unwrap_err().to_string().contains("HTTPS"));

        suite.tasks[0].grader_image_archive = Some(GraderImageArchive {
            sha256: "xyz".into(),
            ..valid_archive()
        });
        assert!(suite.validate().unwrap_err().to_string().contains("64 hex"));

        suite.tasks[0].grader_image_archive = Some(GraderImageArchive {
            size_bytes: 200,
            uncompressed_size_bytes: 100,
            ..valid_archive()
        });
        assert!(suite
            .validate()
            .unwrap_err()
            .to_string()
            .contains("uncompressed size"));
    }
}
