//! Failure budget and strategy switching.
//!
//! Issue #6 Phase 5. In the 2026-08-02 session roughly a quarter of all
//! commands exited non-zero, and the same broken PowerShell composition was
//! retried unchanged more than twice. Nothing in the harness noticed. This
//! ledger makes repetition a *checkable* condition instead of something the
//! model has to remember across a compaction boundary.

use super::shell::ShellKind;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureCategory {
    Syntax,
    Quoting,
    MissingTool,
    Path,
    Patch,
    Typecheck,
    Test,
    Build,
    Timeout,
    Unknown,
}

impl FailureCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::Quoting => "quoting",
            Self::MissingTool => "missing-tool",
            Self::Path => "path",
            Self::Patch => "patch",
            Self::Typecheck => "typecheck",
            Self::Test => "test",
            Self::Build => "build",
            Self::Timeout => "timeout",
            Self::Unknown => "unknown",
        }
    }

    /// Categories that mean "the command never ran as written". Retrying the
    /// same text cannot help; only a different composition can.
    pub fn is_composition_error(self) -> bool {
        matches!(
            self,
            Self::Syntax | Self::Quoting | Self::MissingTool | Self::Path
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolFailure {
    pub step_id: String,
    pub normalized_command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub category: FailureCategory,
    pub attempted_strategy: String,
}

/// What the runtime observed for one command, before classification.
#[derive(Debug, Clone, Default)]
pub struct FailureSignal<'a> {
    pub command: &'a [String],
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stderr: &'a str,
}

/// Stable identity for "the same command". Argument boundaries are preserved
/// with a unit separator so `git commit -m "a b"` never collides with
/// `git commit -m a b`.
pub fn normalize_command(command: &[String]) -> String {
    command
        .iter()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    let lowered = haystack.to_ascii_lowercase();
    needles
        .iter()
        .any(|needle| lowered.contains(&needle.to_ascii_lowercase()))
}

/// Classify a failure. Error text wins over the command name: a `tsc` run that
/// died because `tsc` is not installed is a missing-tool problem, not a type
/// error, and the two call for completely different next moves.
pub fn categorize(signal: &FailureSignal<'_>) -> FailureCategory {
    if signal.timed_out {
        return FailureCategory::Timeout;
    }
    let stderr = signal.stderr;
    if contains_any(
        stderr,
        &[
            "is not recognized as the name of a cmdlet",
            "command not found",
            "CommandNotFoundException",
            "No such file or directory: ",
            "is not recognized as an internal or external command",
        ],
    ) {
        return FailureCategory::MissingTool;
    }
    if contains_any(
        stderr,
        &[
            "TerminatorExpectedAtEndOfString",
            "unterminated quoted string",
            "unexpected EOF while looking for matching",
            "UnexpectedToken",
            "missing closing",
        ],
    ) {
        return FailureCategory::Quoting;
    }
    if contains_any(
        stderr,
        &[
            "ParserError",
            "SyntaxError",
            "parse error",
            "Unexpected token",
            "The token '&&' is not a valid statement separator",
        ],
    ) {
        return FailureCategory::Syntax;
    }
    if contains_any(
        stderr,
        &[
            "Cannot find path",
            "ENOENT",
            "No such file or directory",
            "ItemNotFoundException",
            "PathNotFound",
        ],
    ) {
        return FailureCategory::Path;
    }
    if contains_any(stderr, &["*** Begin Patch", "patch", "apply_patch"]) {
        return FailureCategory::Patch;
    }
    let command = signal.command.join(" ").to_ascii_lowercase();
    if contains_any(&command, &["typecheck", "tsc", "cargo check", "type-check"]) {
        return FailureCategory::Typecheck;
    }
    if contains_any(&command, &["test", "vitest", "jest", "pytest"]) {
        return FailureCategory::Test;
    }
    if contains_any(&command, &["build", "cargo build", "msbuild"]) {
        return FailureCategory::Build;
    }
    FailureCategory::Unknown
}

/// Whether a proposed attempt may run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryVerdict {
    Allowed,
    /// The circuit breaker is open. `reason` is written back to the model so
    /// it changes approach rather than guessing why the call was refused.
    Blocked {
        reason: String,
    },
}

impl RetryVerdict {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Blocked { reason } => Some(reason),
            Self::Allowed => None,
        }
    }
}

/// Threshold above which an execution checkpoint must be written before a
/// context compaction, so the failure history is not summarised away.
pub const CHECKPOINT_NONZERO_RATE: f64 = 0.15;

/// Consecutive failures on one step with an unchanged strategy before the
/// breaker opens. Two failures are information; a third is a loop.
pub const MAX_SAME_STRATEGY_ATTEMPTS: usize = 2;

#[derive(Debug, Clone, Default)]
pub struct FailureLedger {
    failures: Vec<ToolFailure>,
    /// Consecutive failures per step, reset by a success on that step.
    consecutive: BTreeMap<String, Vec<ToolFailure>>,
    succeeded_commands: BTreeMap<String, usize>,
    /// Commands whose inputs changed since their last success, so a re-run is
    /// justified again.
    invalidated_commands: BTreeSet<String>,
    temp_scripts: BTreeSet<String>,
    removed_temp_scripts: BTreeSet<String>,
    total_commands: usize,
    nonzero_commands: usize,
}

impl FailureLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_success(&mut self, step_id: &str, command: &[String]) {
        self.total_commands += 1;
        let normalized = normalize_command(command);
        *self
            .succeeded_commands
            .entry(normalized.clone())
            .or_insert(0) += 1;
        self.invalidated_commands.remove(&normalized);
        self.consecutive.remove(step_id);
    }

    pub fn record_failure(
        &mut self,
        step_id: &str,
        signal: &FailureSignal<'_>,
        attempted_strategy: &str,
    ) -> ToolFailure {
        self.total_commands += 1;
        self.nonzero_commands += 1;
        let failure = ToolFailure {
            step_id: step_id.to_string(),
            normalized_command: normalize_command(signal.command),
            exit_code: signal.exit_code,
            category: categorize(signal),
            attempted_strategy: attempted_strategy.to_string(),
        };
        self.consecutive
            .entry(step_id.to_string())
            .or_default()
            .push(failure.clone());
        self.failures.push(failure.clone());
        failure
    }

    /// Mark a previously successful verification as stale — its inputs
    /// changed, so running it again carries new information.
    pub fn invalidate(&mut self, command: &[String]) {
        self.invalidated_commands.insert(normalize_command(command));
    }

    /// Invalidate every recorded verification. Used after an edit lands.
    pub fn invalidate_all_verifications(&mut self) {
        for command in self.succeeded_commands.keys() {
            self.invalidated_commands.insert(command.clone());
        }
    }

    pub fn record_temp_script(&mut self, path: &str) {
        self.temp_scripts.insert(path.to_string());
    }

    pub fn record_temp_script_removed(&mut self, path: &str) {
        self.removed_temp_scripts.insert(path.to_string());
    }

    /// Temporary scripts that were created and never cleaned up.
    pub fn leaked_temp_scripts(&self) -> Vec<&str> {
        self.temp_scripts
            .difference(&self.removed_temp_scripts)
            .map(String::as_str)
            .collect()
    }

    pub fn temp_script_count(&self) -> usize {
        self.temp_scripts.len()
    }

    pub fn failures(&self) -> &[ToolFailure] {
        &self.failures
    }

    pub fn nonzero_rate(&self) -> f64 {
        if self.total_commands == 0 {
            return 0.0;
        }
        self.nonzero_commands as f64 / self.total_commands as f64
    }

    pub fn should_checkpoint_before_compaction(&self) -> bool {
        self.nonzero_rate() > CHECKPOINT_NONZERO_RATE
    }

    pub fn category_counts(&self) -> BTreeMap<FailureCategory, usize> {
        let mut counts = BTreeMap::new();
        for failure in &self.failures {
            *counts.entry(failure.category).or_insert(0) += 1;
        }
        counts
    }

    /// The circuit breaker.
    ///
    /// Blocks a third consecutive attempt at one step under an unchanged
    /// strategy, and blocks re-running a verification that already passed and
    /// whose inputs have not changed.
    pub fn verdict(
        &self,
        step_id: &str,
        command: &[String],
        proposed_strategy: &str,
    ) -> RetryVerdict {
        let normalized = normalize_command(command);
        if let Some(previous) = self.consecutive.get(step_id) {
            let same_strategy = previous
                .iter()
                .rev()
                .take_while(|failure| failure.attempted_strategy == proposed_strategy)
                .count();
            if same_strategy >= MAX_SAME_STRATEGY_ATTEMPTS {
                let categories = previous
                    .iter()
                    .rev()
                    .take(same_strategy)
                    .map(|failure| failure.category.as_str())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(", ");
                return RetryVerdict::Blocked {
                    reason: format!(
                        "step `{step_id}` already failed {same_strategy} times with strategy `{proposed_strategy}` ({categories}); change strategy before retrying, and state what you changed"
                    ),
                };
            }
        }
        if self.succeeded_commands.contains_key(&normalized)
            && !self.invalidated_commands.contains(&normalized)
        {
            return RetryVerdict::Blocked {
                reason: format!(
                    "`{}` already passed and nothing it depends on has changed since; re-running it adds no information",
                    command.join(" ")
                ),
            };
        }
        RetryVerdict::Allowed
    }

    /// Strategies this step may no longer use. Surfaced in the checkpoint so
    /// the constraint survives compaction.
    pub fn forbidden_retries(&self) -> Vec<Value> {
        self.consecutive
            .iter()
            .filter_map(|(step_id, failures)| {
                let last = failures.last()?;
                let repeated = failures
                    .iter()
                    .rev()
                    .take_while(|failure| failure.attempted_strategy == last.attempted_strategy)
                    .count();
                (repeated >= MAX_SAME_STRATEGY_ATTEMPTS).then(|| {
                    json!({
                        "stepId": step_id,
                        "forbiddenStrategy": last.attempted_strategy,
                        "attempts": repeated,
                        "lastCategory": last.category.as_str()
                    })
                })
            })
            .collect()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "totalCommands": self.total_commands,
            "nonzeroCommands": self.nonzero_commands,
            "nonzeroRate": self.nonzero_rate(),
            "categories": self
                .category_counts()
                .into_iter()
                .map(|(category, count)| (category.as_str().to_string(), json!(count)))
                .collect::<serde_json::Map<_, _>>(),
            "forbiddenRetries": self.forbidden_retries(),
            "tempScriptsCreated": self.temp_script_count(),
            "tempScriptsLeaked": self.leaked_temp_scripts(),
        })
    }
}

/// A bulk edit that broke parsing or type checking must not be pushed forward.
/// Returns the instruction the runtime sends back with the failure.
pub fn bulk_edit_rollback_guidance(category: FailureCategory, shell: ShellKind) -> Option<String> {
    if !matches!(
        category,
        FailureCategory::Typecheck | FailureCategory::Build | FailureCategory::Syntax
    ) {
        return None;
    }
    let _ = shell;
    Some(
        "This batch left the tree unparseable. Revert this batch before doing anything else, then redo the change file by file with exact anchored edits, checking each file after it is written."
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    fn signal<'a>(command: &'a [String], stderr: &'a str) -> FailureSignal<'a> {
        FailureSignal {
            command,
            exit_code: Some(1),
            timed_out: false,
            stderr,
        }
    }

    #[test]
    fn third_attempt_with_an_unchanged_strategy_is_blocked() {
        let mut ledger = FailureLedger::new();
        let command = argv(["pwsh", "-c", "a && b"].as_ref());
        for _ in 0..2 {
            ledger.record_failure(
                "migrate-context",
                &signal(
                    &command,
                    "ParserError: The token '&&' is not a valid statement separator",
                ),
                "powershell-chain",
            );
        }
        let verdict = ledger.verdict("migrate-context", &command, "powershell-chain");
        assert!(!verdict.is_allowed());
        let reason = verdict.reason().unwrap();
        assert!(reason.contains("change strategy"));
        assert!(reason.contains("syntax"));

        // A genuinely different approach is allowed immediately.
        assert!(ledger
            .verdict("migrate-context", &command, "single-statement")
            .is_allowed());
        // A success on that step closes the breaker.
        ledger.record_success("migrate-context", &argv(["pwsh", "-c", "a"].as_ref()));
        assert!(ledger
            .verdict("migrate-context", &command, "powershell-chain")
            .is_allowed());
    }

    #[test]
    fn passing_verification_is_not_rerun_until_its_inputs_change() {
        let mut ledger = FailureLedger::new();
        let typecheck = argv(["pnpm", "typecheck"].as_ref());
        ledger.record_success("verify", &typecheck);
        let blocked = ledger.verdict("verify", &typecheck, "rerun");
        assert!(!blocked.is_allowed());
        assert!(blocked.reason().unwrap().contains("already passed"));

        ledger.invalidate_all_verifications();
        assert!(ledger.verdict("verify", &typecheck, "rerun").is_allowed());
    }

    #[test]
    fn categories_read_the_error_before_the_command_name() {
        let typecheck = argv(["pnpm", "typecheck"].as_ref());
        assert_eq!(
            categorize(&signal(&typecheck, "tsc: command not found")),
            FailureCategory::MissingTool
        );
        assert_eq!(
            categorize(&signal(&typecheck, "src/a.ts(3,1): error TS2304")),
            FailureCategory::Typecheck
        );
        assert_eq!(
            categorize(&FailureSignal {
                command: &typecheck,
                exit_code: None,
                timed_out: true,
                stderr: "",
            }),
            FailureCategory::Timeout
        );
        assert_eq!(
            categorize(&signal(
                &argv(["powershell", "-c", "echo 'x"].as_ref()),
                "TerminatorExpectedAtEndOfString"
            )),
            FailureCategory::Quoting
        );
        assert_eq!(
            categorize(&signal(
                &argv(["cat", "missing.ts"].as_ref()),
                "Cannot find path 'missing.ts'"
            )),
            FailureCategory::Path
        );
        assert!(FailureCategory::Quoting.is_composition_error());
        assert!(!FailureCategory::Test.is_composition_error());
    }

    #[test]
    fn normalized_commands_keep_argument_boundaries() {
        assert_ne!(
            normalize_command(&argv(["git", "commit", "-m", "a b"].as_ref())),
            normalize_command(&argv(["git", "commit", "-m", "a", "b"].as_ref()))
        );
        assert_eq!(
            normalize_command(&argv([" git ", "status"].as_ref())),
            normalize_command(&argv(["git", "status"].as_ref()))
        );
    }

    #[test]
    fn checkpoint_triggers_above_the_nonzero_budget() {
        let mut ledger = FailureLedger::new();
        let command = argv(["git", "status"].as_ref());
        for _ in 0..5 {
            ledger.record_success("step", &command);
            ledger.invalidate_all_verifications();
        }
        assert_eq!(ledger.nonzero_rate(), 0.0);
        assert!(!ledger.should_checkpoint_before_compaction());
        // 1 of 6 commands non-zero is over the 15% budget.
        ledger.record_failure("step", &signal(&command, "boom"), "first");
        assert!(ledger.nonzero_rate() > CHECKPOINT_NONZERO_RATE);
        assert!(ledger.should_checkpoint_before_compaction());
    }

    #[test]
    fn temp_scripts_are_tracked_until_they_are_removed() {
        let mut ledger = FailureLedger::new();
        ledger.record_temp_script("scratch/migrate.py");
        ledger.record_temp_script("scratch/fix.ps1");
        assert_eq!(ledger.temp_script_count(), 2);
        ledger.record_temp_script_removed("scratch/migrate.py");
        assert_eq!(ledger.leaked_temp_scripts(), vec!["scratch/fix.ps1"]);
    }

    #[test]
    fn forbidden_retries_survive_into_the_checkpoint_payload() {
        let mut ledger = FailureLedger::new();
        let command = argv(["pwsh", "-c", "bulk"].as_ref());
        for _ in 0..2 {
            ledger.record_failure("bulk", &signal(&command, "ParserError"), "regex-bulk");
        }
        let payload = ledger.to_json();
        assert_eq!(payload["forbiddenRetries"][0]["stepId"], "bulk");
        assert_eq!(
            payload["forbiddenRetries"][0]["forbiddenStrategy"],
            "regex-bulk"
        );
        assert_eq!(payload["categories"]["syntax"], 2);
    }

    #[test]
    fn broken_bulk_edits_are_told_to_roll_back_and_go_per_file() {
        let guidance =
            bulk_edit_rollback_guidance(FailureCategory::Typecheck, ShellKind::Powershell).unwrap();
        assert!(guidance.contains("Revert this batch"));
        assert!(guidance.contains("file by file"));
        assert!(
            bulk_edit_rollback_guidance(FailureCategory::Test, ShellKind::Powershell).is_none()
        );
    }
}
