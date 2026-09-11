//! Vellum Canonical v2 deterministic tool semantics (spec sections 9–16).
//!
//! Extracts deterministic facts (commands, exit codes, test states, file changes,
//! and pending tool calls) from portable history and paired tool exchanges.
//! This state is constructed purely by deterministic code and is never left
//! to the LLM to guess.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::tool_output_normalizer::{normalize_output_text, normalize_tool_output};
use crate::trajectory::ToolExchange;

/// Stable sha256 identity helper.
fn sha256_hex(payload: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(payload.as_bytes()))
}

/// Normalize a command string for state keying (collapsing whitespace).
pub fn normalize_command_key(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Normalized category for a shell or process command invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    Test,
    Build,
    Format,
    Lint,
    Git,
    Search,
    Read,
    Write,
    Process,
    Other,
}

/// Outcome of a search or existence command invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchOutcome {
    Match,
    NoMatch,
    Error,
    Unknown,
}

/// Deterministically classify search outcome from command, output text, and exit code.
pub fn classify_search_outcome(
    command: &str,
    output: &str,
    exit_code: Option<i32>,
) -> SearchOutcome {
    let lower_cmd = command.to_ascii_lowercase();
    let output = normalize_output_text(output);
    let lower_out = output.to_ascii_lowercase();

    // Check exit codes
    if let Some(code) = exit_code {
        if code != 0 {
            // grep / rg exit code 1 means 0 lines matched (successful negative search)
            if (lower_cmd.starts_with("grep")
                || lower_cmd.starts_with("rg")
                || lower_cmd.starts_with("ripgrep"))
                && code == 1
            {
                return SearchOutcome::NoMatch;
            }
            return SearchOutcome::Error;
        }
    }

    if lower_out.contains("cannot find")
        || lower_out.contains("error:")
        || lower_out.contains("exception:")
        || lower_out.contains("command failed")
    {
        return SearchOutcome::Error;
    }

    // PowerShell Test-Path returns True or False
    if lower_cmd.contains("test-path") {
        if lower_out
            .lines()
            .any(|l| l.trim().eq_ignore_ascii_case("true"))
            || lower_out.trim().ends_with("true")
        {
            return SearchOutcome::Match;
        }
        if lower_out
            .lines()
            .any(|l| l.trim().eq_ignore_ascii_case("false"))
            || lower_out.trim().ends_with("false")
        {
            return SearchOutcome::NoMatch;
        }
    }

    if output.trim().is_empty() {
        return SearchOutcome::NoMatch;
    }

    SearchOutcome::Match
}

/// A deterministic observation of an executed shell or tool command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandInvocation {
    pub source_index: usize,
    pub call_id: Option<String>,
    pub command: String,
    pub kind: CommandKind,
    pub exit_code: Option<i32>,
    pub output_hash: Option<String>,
    #[serde(default)]
    pub search_outcome: Option<SearchOutcome>,
}

/// Classify a raw shell command line into a semantic [`CommandKind`].
pub fn classify_command(command: &str) -> CommandKind {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return CommandKind::Other;
    }

    // Tokenize command tokens (basic whitespace split, stripping leading env vars).
    let tokens: Vec<&str> = trimmed
        .split_whitespace()
        .skip_while(|tok| tok.contains('=') && !tok.starts_with('-'))
        .collect();

    let lower_trimmed = trimmed.to_ascii_lowercase();
    if lower_trimmed.contains("| select-string") || lower_trimmed.contains("| sls") {
        return CommandKind::Search;
    }

    let first = tokens.first().copied().unwrap_or("");
    let second = tokens.get(1).copied().unwrap_or("");
    let first_lower = first.to_ascii_lowercase();

    match first_lower.as_str() {
        "get-childitem" | "gci" | "dir" => {
            if lower_trimmed.contains("-filter")
                || lower_trimmed.contains("-include")
                || lower_trimmed.contains("-recurse")
            {
                return CommandKind::Search;
            } else {
                return CommandKind::Read;
            }
        }
        "select-string" | "sls" => return CommandKind::Search,
        "get-content" | "gc" | "type" => return CommandKind::Read,
        "test-path" | "resolve-path" => return CommandKind::Read,
        "set-content" | "add-content" | "new-item" | "remove-item" => return CommandKind::Write,
        _ => {}
    }

    match first {
        "cargo" => match second {
            "test" | "t" | "nextest" => CommandKind::Test,
            "build" | "b" | "check" | "c" => CommandKind::Build,
            "fmt" => CommandKind::Format,
            "clippy" => CommandKind::Lint,
            _ => CommandKind::Other,
        },
        "pytest" | "ctest" | "jest" | "vitest" | "mocha" => CommandKind::Test,
        "python" | "python3" => {
            if second == "-m" && tokens.get(2).copied() == Some("pytest") {
                CommandKind::Test
            } else {
                CommandKind::Other
            }
        }
        "pnpm" | "npm" | "yarn" | "bun" => {
            let sub = if second == "run" {
                tokens.get(2).copied().unwrap_or("")
            } else {
                second
            };
            match sub {
                "test" | "t" => CommandKind::Test,
                "build" | "compile" => CommandKind::Build,
                "fmt" | "format" => CommandKind::Format,
                "lint" => CommandKind::Lint,
                _ => CommandKind::Other,
            }
        }
        "go" => match second {
            "test" => CommandKind::Test,
            "build" => CommandKind::Build,
            "fmt" => CommandKind::Format,
            "vet" => CommandKind::Lint,
            _ => CommandKind::Other,
        },
        "dotnet" => match second {
            "test" => CommandKind::Test,
            "build" => CommandKind::Build,
            "format" => CommandKind::Format,
            _ => CommandKind::Other,
        },
        "rustfmt" | "prettier" | "black" | "gofmt" | "goimports" | "clang-format" => {
            CommandKind::Format
        }
        "ruff" => match second {
            "format" => CommandKind::Format,
            "check" => CommandKind::Lint,
            _ => CommandKind::Other,
        },
        "eslint" | "flake8" | "pylint" | "golangci-lint" => CommandKind::Lint,
        "git" => CommandKind::Git,
        "rg" | "grep" | "ripgrep" | "find" | "fd" | "ag" | "ack" => CommandKind::Search,
        "cat" | "head" | "tail" | "sed" | "awk" | "less" | "more" | "bat" => CommandKind::Read,
        "cp" | "mv" | "rm" | "mkdir" | "touch" => CommandKind::Write,
        "ps" | "kill" | "pkill" | "top" | "htop" => CommandKind::Process,
        "make" | "ninja" | "cmake" => CommandKind::Build,
        _ => CommandKind::Other,
    }
}

/// Extract an integer exit code from structured or formatted tool output.
/// Never guesses 0 from prose text like "passed" without explicit code/status.
pub fn extract_exit_code(output: &Value) -> Option<i32> {
    if let Some(code) = output.get("exit_code").and_then(Value::as_i64) {
        return Some(code as i32);
    }
    if let Some(code) = output.get("exitCode").and_then(Value::as_i64) {
        return Some(code as i32);
    }
    if let Some(code) = output.get("code").and_then(Value::as_i64) {
        return Some(code as i32);
    }
    if let Some(code) = output.get("status").and_then(Value::as_i64) {
        return Some(code as i32);
    }

    // Check embedded exit code string patterns in stdout/output
    let text = match output.get("output") {
        Some(Value::String(s)) => Some(s.as_str()),
        _ => match output.get("text") {
            Some(Value::String(s)) => Some(s.as_str()),
            _ => None,
        },
    };

    if let Some(text) = text {
        for line in text.lines().rev() {
            let line = line.trim();
            let lower = line.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("exit code:") {
                if let Ok(code) = rest.trim().parse::<i32>() {
                    return Some(code);
                }
            }
            if let Some(rest) = lower.strip_prefix("exit code ") {
                if let Ok(code) = rest.trim().parse::<i32>() {
                    return Some(code);
                }
            }
            if let Some(rest) = lower.strip_prefix("process exited with code") {
                if let Ok(code) = rest.trim().parse::<i32>() {
                    return Some(code);
                }
            }
            if let Some(rest) = lower.strip_prefix("the command exited with code ") {
                let code_str = rest.trim().trim_end_matches('.');
                if let Ok(code) = code_str.parse::<i32>() {
                    return Some(code);
                }
            }
        }
    }

    None
}

/// Extract command string from a tool call item.
pub fn extract_command_string(call: &Value) -> Option<String> {
    let name = call.get("name").and_then(Value::as_str).unwrap_or("");
    let call_type = call.get("type").and_then(Value::as_str).unwrap_or("");

    let args = match call.get("arguments") {
        Some(Value::String(raw)) => {
            serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.clone()))
        }
        Some(val) => val.clone(),
        None => Value::Null,
    };

    if call_type == "local_shell_call" {
        if let Some(cmd) = call.get("command").and_then(Value::as_str) {
            return Some(cmd.to_string());
        }
    }

    // Check common argument field names
    if let Value::Object(map) = &args {
        for key in &[
            "cmd",
            "command",
            "CommandLine",
            "command_line",
            "script",
            "input",
        ] {
            if let Some(Value::String(cmd)) = map.get(*key) {
                return Some(cmd.clone());
            }
        }
    } else if let Value::String(cmd) = &args {
        return Some(cmd.clone());
    }

    // If tool name itself indicates shell invocation
    if matches!(
        name,
        "exec_command"
            | "run_command"
            | "execute_command"
            | "bash"
            | "shell"
            | "terminal"
            | "local_shell_call"
    ) {
        if let Some(cmd) = call.get("command").and_then(Value::as_str) {
            return Some(cmd.to_string());
        }
    }

    None
}

/// Extract textual output from a tool output item.
pub fn extract_tool_output_text(output: &Value) -> Option<String> {
    if let Some(s) = output.get("output").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    if let Some(s) = output.get("text").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    if let Some(s) = output.get("content").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    if let Some(arr) = output.get("content").and_then(Value::as_array) {
        let mut buf = String::new();
        for part in arr {
            if let Some(txt) = part.get("text").and_then(Value::as_str) {
                buf.push_str(txt);
            }
        }
        if !buf.is_empty() {
            return Some(buf);
        }
    }
    None
}

/// Extract a [`CommandInvocation`] from a tool call and its matching output slice.
pub fn extract_command_invocation(
    source_index: usize,
    call: &Value,
    outputs: &[&Value],
) -> Option<CommandInvocation> {
    let command = extract_command_string(call)?;
    let call_id = call
        .get("call_id")
        .or_else(|| call.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let kind = classify_command(&command);

    let exit_code = outputs.iter().find_map(|out| extract_exit_code(out));

    let normalized = normalize_tool_output(outputs);
    let output_hash = Some(sha256_hex(&normalized.text));
    let combined_output = if normalized.text.is_empty() {
        String::new()
    } else {
        normalized.text.clone()
    };
    let exit_code = exit_code.or(normalized.exit_code);

    let search_outcome = if matches!(kind, CommandKind::Search)
        || (matches!(kind, CommandKind::Read) && command.to_ascii_lowercase().contains("test-path"))
    {
        Some(classify_search_outcome(
            &command,
            &combined_output,
            exit_code,
        ))
    } else {
        None
    };

    Some(CommandInvocation {
        source_index,
        call_id,
        command,
        kind,
        exit_code,
        output_hash,
        search_outcome,
    })
}

/// Status of a test execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    Passed,
    Failed,
    Unknown,
}

/// A deterministic observation of test execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestState {
    pub command: String,
    pub status: TestStatus,
    pub exit_code: Option<i32>,
    pub result_hash: Option<String>,
    pub summary: String,
}

/// Type of file operation observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOperation {
    Created,
    Modified,
    Deleted,
    Unknown,
}

/// Provenance of the observed file change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileEvidenceSource {
    Patch,
    StructuredTool,
    GitDiff,
    Other,
}

/// A deterministic observation of a file change in the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedFileChange {
    pub path: String,
    pub operation: FileOperation,
    pub source: FileEvidenceSource,
}

/// A tool invocation that has not completed with an output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingToolCall {
    pub call_id: Option<String>,
    pub tool_name: String,
    pub source_index: usize,
}

/// Full deterministic workspace state extracted from execution history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedWorkspaceState {
    pub changed_files: Vec<ObservedFileChange>,
    pub tests: Vec<TestState>,
    pub commands: Vec<CommandInvocation>,
    pub pending_tool_calls: Vec<PendingToolCall>,
    #[serde(default)]
    pub last_progress_source_index: Option<usize>,
}

/// Stable workspace-mutation identity: canonical targets plus payload digest.
/// Excludes source index, call id, and timestamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationIdentity {
    pub targets: Vec<String>,
    pub payload_digest: String,
}

/// Parse tool-call arguments from `arguments` or custom-tool `input`.
pub fn parse_tool_call_arguments(call: &Value) -> Value {
    match call.get("arguments") {
        Some(Value::String(raw)) => {
            serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.clone()))
        }
        Some(val) => val.clone(),
        None => match call.get("input") {
            Some(Value::String(raw)) => {
                serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.clone()))
            }
            Some(val) => val.clone(),
            None => Value::Null,
        },
    }
}

fn string_arg(args: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(Value::String(s)) = args.get(*key) {
            if !s.is_empty() {
                return Some(s.clone());
            }
        }
    }
    None
}

fn patch_header_paths(patch: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in patch.lines() {
        let trimmed = line.trim();
        for prefix in ["*** Update File:", "*** Add File:", "*** Delete File:"] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let path = rest.trim();
                if !path.is_empty() {
                    out.push(path.to_string());
                }
            }
        }
    }
    out
}

fn mutation_payload_text(name: &str, args: &Value) -> String {
    match name {
        "apply_patch" | "patch" => {
            let raw = args
                .get("patch")
                .or_else(|| args.get("input"))
                .and_then(Value::as_str)
                .or_else(|| args.as_str())
                .unwrap_or("");
            crate::adapter::normalize_patch_delimiters(raw)
                .replace("\r\n", "\n")
                .trim()
                .to_string()
        }
        "write_to_file" | "create_file" => {
            let path = string_arg(args, &["TargetFile", "target_file", "path"]).unwrap_or_default();
            let content = string_arg(
                args,
                &[
                    "contents",
                    "content",
                    "CodeContent",
                    "code_content",
                    "code",
                    "text",
                ],
            )
            .unwrap_or_default();
            format!("{path}\n{content}")
        }
        _ => {
            let path = string_arg(args, &["TargetFile", "target_file", "path"]).unwrap_or_default();
            let old = string_arg(args, &["old_string", "oldString", "OldString", "old"])
                .unwrap_or_default();
            let new = string_arg(args, &["new_string", "newString", "NewString", "new"])
                .unwrap_or_default();
            let content = string_arg(
                args,
                &[
                    "contents",
                    "content",
                    "CodeContent",
                    "code_content",
                    "code",
                    "text",
                ],
            )
            .unwrap_or_default();
            format!("{path}\n{old}\n{new}\n{content}")
        }
    }
}

/// Identity for dedicated mutation tools (`apply_patch`, `write_to_file`, `edit_file`).
pub fn mutation_identity_from_call(call: &Value) -> Option<MutationIdentity> {
    let name = call.get("name").and_then(Value::as_str).unwrap_or("");
    if !matches!(
        name,
        "apply_patch"
            | "patch"
            | "write_to_file"
            | "create_file"
            | "edit_file"
            | "replace_file_content"
            | "modify_file"
    ) {
        return None;
    }
    let args = parse_tool_call_arguments(call);
    let payload = mutation_payload_text(name, &args);
    let mut targets: Vec<String> = extract_file_changes_from_call(call)
        .into_iter()
        .map(|change| change.path)
        .collect();
    if matches!(name, "apply_patch" | "patch") {
        for path in patch_header_paths(&payload) {
            if !targets.iter().any(|existing| existing == &path) {
                targets.push(path);
            }
        }
    } else if let Some(path) = string_arg(&args, &["TargetFile", "target_file", "path"]) {
        if !targets.iter().any(|existing| existing == &path) {
            targets.push(path);
        }
    }
    targets.sort();
    targets.dedup();
    Some(MutationIdentity {
        targets,
        payload_digest: sha256_hex(&payload),
    })
}

/// Extract structured file changes from tool calls (e.g. `write_to_file`, `replace_file_content`, `apply_patch`).
fn extract_file_changes_from_call(call: &Value) -> Vec<ObservedFileChange> {
    let name = call.get("name").and_then(Value::as_str).unwrap_or("");
    let args = parse_tool_call_arguments(call);

    let mut changes = Vec::new();

    match name {
        "write_to_file" | "create_file" => {
            let path = args
                .get("TargetFile")
                .or_else(|| args.get("target_file"))
                .or_else(|| args.get("path"))
                .and_then(Value::as_str);
            let overwrite = args
                .get("Overwrite")
                .or_else(|| args.get("overwrite"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let Some(path) = path {
                changes.push(ObservedFileChange {
                    path: path.to_string(),
                    operation: if overwrite {
                        FileOperation::Modified
                    } else {
                        FileOperation::Created
                    },
                    source: FileEvidenceSource::StructuredTool,
                });
            }
        }
        "replace_file_content" | "edit_file" | "modify_file" => {
            let path = args
                .get("TargetFile")
                .or_else(|| args.get("target_file"))
                .or_else(|| args.get("path"))
                .and_then(Value::as_str);
            if let Some(path) = path {
                changes.push(ObservedFileChange {
                    path: path.to_string(),
                    operation: FileOperation::Modified,
                    source: FileEvidenceSource::StructuredTool,
                });
            }
        }
        "apply_patch" | "patch" => {
            let patch_raw = args
                .get("patch")
                .or_else(|| args.get("input"))
                .and_then(Value::as_str)
                .or_else(|| args.as_str());

            if let Some(patch_str) = patch_raw {
                let normalized = crate::adapter::normalize_patch_delimiters(patch_str);
                if let Ok(summary) = crate::adapter::validate_patch(&normalized) {
                    for change in summary.changes {
                        let op = match change.kind {
                            crate::adapter::FileChangeKind::Add => FileOperation::Created,
                            crate::adapter::FileChangeKind::Update => FileOperation::Modified,
                            crate::adapter::FileChangeKind::Delete => FileOperation::Deleted,
                        };
                        changes.push(ObservedFileChange {
                            path: change.path,
                            operation: op,
                            source: FileEvidenceSource::Patch,
                        });
                    }
                } else {
                    // Fallback to standard diff parser
                    for line in patch_str.lines() {
                        if let Some(file) = line.strip_prefix("+++ b/") {
                            changes.push(ObservedFileChange {
                                path: file.trim().to_string(),
                                operation: FileOperation::Modified,
                                source: FileEvidenceSource::Patch,
                            });
                        } else if let Some(file) = line.strip_prefix("--- a/") {
                            if !changes.iter().any(|c| c.path == file.trim()) {
                                changes.push(ObservedFileChange {
                                    path: file.trim().to_string(),
                                    operation: FileOperation::Modified,
                                    source: FileEvidenceSource::Patch,
                                });
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }

    changes
}

/// Classified outcome of a workspace mutation attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationOutcome {
    ConfirmedSuccess,
    ConfirmedFailure,
    Unknown,
}

/// Classify mutation tool outcome from tool name and output items (spec section 24).
/// Requires positive proof of success (e.g. exit code 0 or explicit success message)
/// to promote an attempted change to ConfirmedSuccess.
pub fn classify_mutation_outcome(tool_name: &str, output_items: &[&Value]) -> MutationOutcome {
    if output_items.is_empty() {
        return MutationOutcome::Unknown;
    }

    let mut has_failure = false;
    let mut has_success = false;

    for out in output_items {
        // 1. Structured exit code
        if let Some(code) = extract_exit_code(out) {
            if code != 0 {
                return MutationOutcome::ConfirmedFailure;
            } else {
                has_success = true;
            }
        }

        // 2. Structured status / error
        if let Some(status) = out.get("status").and_then(Value::as_str) {
            let lower = status.trim().to_ascii_lowercase();
            if lower == "error" || lower == "failed" || lower == "failure" {
                return MutationOutcome::ConfirmedFailure;
            }
            if lower == "success" || lower == "ok" || lower == "completed" {
                has_success = true;
            }
        }
        if out.get("error").is_some() && !out.get("error").unwrap().is_null() {
            return MutationOutcome::ConfirmedFailure;
        }

        // 3. Text output analysis
        let text = out
            .get("output")
            .or_else(|| out.get("text"))
            .or_else(|| out.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("");

        let text_lower = text.to_ascii_lowercase();
        let trimmed_lower = text_lower.trim();

        // Check failure patterns first (including "unsuccessful", "failed", "error", etc.)
        if trimmed_lower.contains("unsuccessful")
            || trimmed_lower.contains("not successful")
            || trimmed_lower.contains("invalid context")
            || trimmed_lower.contains("patch failed")
            || trimmed_lower.contains("failed to apply patch")
            || trimmed_lower.contains("error:")
            || trimmed_lower.contains("permission denied")
            || trimmed_lower.contains("failed to write")
            || trimmed_lower.contains("failed to create")
            || trimmed_lower.contains("failed to edit")
            || trimmed_lower.contains("no such file or directory")
            || trimmed_lower.contains("file not found")
            || trimmed_lower.contains("read-only file system")
            || trimmed_lower.contains("operation not permitted")
            || trimmed_lower.contains("cannot open file")
            || trimmed_lower.contains("panic:")
            || trimmed_lower.contains("rejected chunk")
            || trimmed_lower.contains("hunk failed")
        {
            has_failure = true;
        }

        // Positive success patterns: exact adapter-specific phrases, never bare `contains("success")`
        if trimmed_lower == "ok"
            || trimmed_lower == "success"
            || trimmed_lower == "completed"
            || trimmed_lower.starts_with("success:")
            || trimmed_lower.starts_with("successfully applied")
            || trimmed_lower.starts_with("applied patch successfully")
            || trimmed_lower.starts_with("patch applied successfully")
            || trimmed_lower.starts_with("file created successfully")
            || trimmed_lower.starts_with("file written successfully")
            || trimmed_lower.starts_with("file updated successfully")
            || trimmed_lower.starts_with("edited successfully")
            || trimmed_lower.starts_with("created ")
            || trimmed_lower.contains("0 errors, 0 warnings")
            || (tool_name == "apply_patch"
                && (trimmed_lower == "patch applied"
                    || trimmed_lower.starts_with("patch applied successfully")
                    || trimmed_lower.starts_with("applied successfully")))
            || (tool_name == "write_to_file"
                && (trimmed_lower.starts_with("file created")
                    || trimmed_lower.starts_with("file written")
                    || trimmed_lower.starts_with("written to")))
        {
            has_success = true;
        }
    }

    if has_failure {
        MutationOutcome::ConfirmedFailure
    } else if has_success {
        MutationOutcome::ConfirmedSuccess
    } else {
        MutationOutcome::Unknown
    }
}

/// Extract deterministic workspace state from items and paired tool exchanges.
pub fn extract_observed_workspace_state(
    items: &[Value],
    exchanges: &[ToolExchange],
) -> ObservedWorkspaceState {
    let mut commands = Vec::new();
    let mut tests_map: std::collections::BTreeMap<String, TestState> =
        std::collections::BTreeMap::new();
    let mut changed_files_map: std::collections::BTreeMap<String, ObservedFileChange> =
        std::collections::BTreeMap::new();
    let mut pending_tool_calls = Vec::new();
    let mut last_progress_source_index: Option<usize> = None;

    for exchange in exchanges {
        let call_item = &items[exchange.call_source_index];
        let output_items: Vec<&Value> = exchange
            .output_source_indices
            .iter()
            .map(|&idx| &items[idx])
            .collect();

        // 1. Pending tool calls
        if !exchange.completed {
            pending_tool_calls.push(PendingToolCall {
                call_id: exchange.call_id.clone(),
                tool_name: exchange.tool_name.clone(),
                source_index: exchange.call_source_index,
            });
        }

        // 2. File changes - only promote if matching output exists and confirms success
        let attempted_changes = extract_file_changes_from_call(call_item);
        if !attempted_changes.is_empty() && exchange.completed {
            let outcome = classify_mutation_outcome(&exchange.tool_name, &output_items);
            if outcome == MutationOutcome::ConfirmedSuccess {
                last_progress_source_index = Some(
                    last_progress_source_index
                        .map(|idx: usize| idx.max(exchange.call_source_index))
                        .unwrap_or(exchange.call_source_index),
                );
                for change in attempted_changes {
                    changed_files_map.insert(change.path.clone(), change);
                }
            }
        }

        // 3. Command invocations
        if let Some(invocation) =
            extract_command_invocation(exchange.call_source_index, call_item, &output_items)
        {
            // If this command is a test and has completed, update test state
            if invocation.kind == CommandKind::Test && exchange.completed {
                last_progress_source_index = Some(
                    last_progress_source_index
                        .map(|idx: usize| idx.max(exchange.call_source_index))
                        .unwrap_or(exchange.call_source_index),
                );
                let status = match invocation.exit_code {
                    Some(0) => TestStatus::Passed,
                    Some(_) => TestStatus::Failed,
                    None => TestStatus::Unknown,
                };
                let summary = format!(
                    "{} (exit code: {:?})",
                    invocation.command, invocation.exit_code
                );
                let key = normalize_command_key(&invocation.command);
                tests_map.insert(
                    key,
                    TestState {
                        command: invocation.command.clone(),
                        status,
                        exit_code: invocation.exit_code,
                        result_hash: invocation.output_hash.clone(),
                        summary,
                    },
                );
            }
            commands.push(invocation);
        }
    }

    ObservedWorkspaceState {
        changed_files: changed_files_map.into_values().collect(),
        tests: tests_map.into_values().collect(),
        commands,
        pending_tool_calls,
        last_progress_source_index,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_classify_powershell_commands() {
        assert_eq!(
            classify_command("Get-ChildItem -Recurse -Filter DECISIONS.md"),
            CommandKind::Search
        );
        assert_eq!(
            classify_command("Get-Content DECISIONS.md"),
            CommandKind::Read
        );
        assert_eq!(
            classify_command("Select-String -Pattern CIPHERTEXT_RECOVERY_EXACT"),
            CommandKind::Search
        );
        assert_eq!(
            classify_command("Test-Path DECISIONS.md"),
            CommandKind::Read
        );
        assert_eq!(
            classify_command("Set-Content -Path out.txt -Value test"),
            CommandKind::Write
        );
        assert_eq!(
            classify_command("Get-ChildItem | Select-String pattern"),
            CommandKind::Search
        );
    }

    #[test]
    fn test_classify_search_outcome() {
        assert_eq!(
            classify_search_outcome("Select-String -Pattern foo", "", Some(0)),
            SearchOutcome::NoMatch
        );
        assert_eq!(
            classify_search_outcome("Select-String -Pattern foo", "foo.txt:1:foo", Some(0)),
            SearchOutcome::Match
        );
        assert_eq!(
            classify_search_outcome("Test-Path foo.txt", "False", Some(0)),
            SearchOutcome::NoMatch
        );
        assert_eq!(
            classify_search_outcome("Test-Path foo.txt", "True", Some(0)),
            SearchOutcome::Match
        );
        assert_eq!(
            classify_search_outcome("rg foo", "", Some(1)),
            SearchOutcome::NoMatch
        );
        assert_eq!(
            classify_search_outcome("rg foo", "error: path not found", Some(2)),
            SearchOutcome::Error
        );
        assert_eq!(
            classify_search_outcome(
                "Select-String -Path . -Pattern foo",
                "Chunk ID: 123\nWall time: 0.1 seconds\nProcess exited with code 0\nFinal output:\n",
                Some(0),
            ),
            SearchOutcome::NoMatch,
            "an empty PowerShell payload must not be confused with the Codex exec envelope",
        );
        assert_eq!(
            classify_search_outcome(
                "Select-String -Path . -Pattern foo",
                "Chunk ID: 123\r\nProcess exited with code 0\r\nFinal output:\r\n",
                Some(0),
            ),
            SearchOutcome::NoMatch,
        );
        assert_eq!(
            classify_search_outcome(
                "Select-String -Path . -Pattern foo",
                "Chunk ID: 123\nProcess exited with code 0\nFinal output:\nfoo.txt:1:foo",
                Some(0),
            ),
            SearchOutcome::Match,
        );
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn command_classification_recognizes_core_tools() {
        assert_eq!(classify_command("cargo test --lib"), CommandKind::Test);
        assert_eq!(classify_command("pytest -k test_foo"), CommandKind::Test);
        assert_eq!(
            classify_command("python -m pytest evals/"),
            CommandKind::Test
        );
        assert_eq!(classify_command("pnpm test"), CommandKind::Test);
        assert_eq!(classify_command("go test ./..."), CommandKind::Test);
        assert_eq!(classify_command("dotnet test"), CommandKind::Test);

        assert_eq!(
            classify_command("cargo build --release"),
            CommandKind::Build
        );
        assert_eq!(classify_command("pnpm build"), CommandKind::Build);

        assert_eq!(classify_command("cargo fmt --check"), CommandKind::Format);
        assert_eq!(classify_command("prettier --write ."), CommandKind::Format);
        assert_eq!(classify_command("ruff format"), CommandKind::Format);

        assert_eq!(classify_command("cargo clippy"), CommandKind::Lint);
        assert_eq!(classify_command("eslint src/"), CommandKind::Lint);

        assert_eq!(classify_command("git status"), CommandKind::Git);
        assert_eq!(classify_command("git diff HEAD~1"), CommandKind::Git);

        assert_eq!(classify_command("rg 'fn test'"), CommandKind::Search);
        assert_eq!(classify_command("cat Cargo.toml"), CommandKind::Read);
        assert_eq!(classify_command("mkdir -p src/"), CommandKind::Write);
        assert_eq!(classify_command("ps aux"), CommandKind::Process);
    }

    #[test]
    fn extract_exit_code_reads_structured_and_string_patterns() {
        assert_eq!(extract_exit_code(&json!({"exit_code": 0})), Some(0));
        assert_eq!(extract_exit_code(&json!({"code": 101})), Some(101));
        assert_eq!(
            extract_exit_code(&json!({"output": "The command exited with code 0."})),
            Some(0)
        );
        assert_eq!(
            extract_exit_code(&json!({"output": "build failed\nexit code: 1"})),
            Some(1)
        );
        // Prose saying "all tests passed" without exit code is None
        assert_eq!(
            extract_exit_code(&json!({"output": "all 42 tests passed in 0.5s"})),
            None
        );
    }

    #[test]
    fn extract_file_changes_reads_edit_tools() {
        let call = json!({
            "type": "function_call",
            "name": "write_to_file",
            "arguments": serde_json::to_string(&json!({
                "TargetFile": "/app/src/main.rs",
                "Overwrite": true
            })).unwrap()
        });
        let changes = extract_file_changes_from_call(&call);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "/app/src/main.rs");
        assert_eq!(changes[0].operation, FileOperation::Modified);
    }

    #[test]
    fn observed_workspace_state_tracks_latest_test_status() {
        let items = vec![
            json!({
                "type": "function_call",
                "call_id": "c1",
                "name": "exec_command",
                "arguments": serde_json::to_string(&json!({ "cmd": "cargo test foo" })).unwrap()
            }),
            json!({
                "type": "function_call_output",
                "call_id": "c1",
                "output": "exit code: 1"
            }),
            json!({
                "type": "function_call",
                "call_id": "c2",
                "name": "exec_command",
                "arguments": serde_json::to_string(&json!({ "cmd": "cargo test foo" })).unwrap()
            }),
            json!({
                "type": "function_call_output",
                "call_id": "c2",
                "output": "The command exited with code 0."
            }),
        ];
        let exchanges = vec![
            ToolExchange {
                call_source_index: 0,
                output_source_indices: vec![1],
                call_id: Some("c1".into()),
                tool_name: "exec_command".into(),
                call_fingerprint: "fp1".into(),
                output_fingerprint: Some("out1".into()),
                completed: true,
            },
            ToolExchange {
                call_source_index: 2,
                output_source_indices: vec![3],
                call_id: Some("c2".into()),
                tool_name: "exec_command".into(),
                call_fingerprint: "fp2".into(),
                output_fingerprint: Some("out2".into()),
                completed: true,
            },
        ];

        let state = extract_observed_workspace_state(&items, &exchanges);
        assert_eq!(state.tests.len(), 1);
        assert_eq!(state.tests[0].command, "cargo test foo");
        assert_eq!(state.tests[0].status, TestStatus::Passed);
        assert_eq!(state.tests[0].exit_code, Some(0));
    }

    #[test]
    fn codex_apply_patch_populates_changed_files() {
        let patch_content =
            "*** Begin Patch\n*** Update File: src/main.rs\n-old line\n+new line\n*** End Patch\n";
        let call = json!({
            "type": "function_call",
            "name": "apply_patch",
            "arguments": serde_json::to_string(&json!({
                "patch": patch_content
            })).unwrap()
        });

        let changes = extract_file_changes_from_call(&call);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "src/main.rs");
        assert_eq!(changes[0].operation, FileOperation::Modified);
        assert_eq!(changes[0].source, FileEvidenceSource::Patch);
    }

    #[test]
    fn failed_apply_patch_does_not_modify_workspace_state() {
        let items = vec![
            json!({
                "type": "function_call",
                "call_id": "c_patch",
                "name": "apply_patch",
                "arguments": serde_json::to_string(&json!({
                    "patch": "*** Begin Patch\n*** Update File: src/bad.rs\n-old\n+new\n*** End Patch\n"
                })).unwrap()
            }),
            json!({
                "type": "function_call_output",
                "call_id": "c_patch",
                "output": "Invalid Context: hunk #1 failed @ line 10"
            }),
        ];
        let exchanges = vec![ToolExchange {
            call_source_index: 0,
            output_source_indices: vec![1],
            call_id: Some("c_patch".into()),
            tool_name: "apply_patch".into(),
            call_fingerprint: "fp".into(),
            output_fingerprint: Some("out".into()),
            completed: true,
        }];

        let ws = extract_observed_workspace_state(&items, &exchanges);
        assert!(
            ws.changed_files.is_empty(),
            "failed patch must not register file change in workspace"
        );
    }

    #[test]
    fn failed_write_tool_does_not_mark_file_created() {
        let items = vec![
            json!({
                "type": "function_call",
                "call_id": "c_write",
                "name": "write_to_file",
                "arguments": serde_json::to_string(&json!({
                    "TargetFile": "/root/readonly.rs",
                    "CodeContent": "fn main() {}"
                })).unwrap()
            }),
            json!({
                "type": "function_call_output",
                "call_id": "c_write",
                "output": "Error: Permission denied (os error 13)",
                "exit_code": 1
            }),
        ];
        let exchanges = vec![ToolExchange {
            call_source_index: 0,
            output_source_indices: vec![1],
            call_id: Some("c_write".into()),
            tool_name: "write_to_file".into(),
            call_fingerprint: "fp".into(),
            output_fingerprint: Some("out".into()),
            completed: true,
        }];

        let ws = extract_observed_workspace_state(&items, &exchanges);
        assert!(
            ws.changed_files.is_empty(),
            "failed write tool must not register file change"
        );
    }

    #[test]
    fn parallel_calls_workspace_extracts_both() {
        let items = vec![
            json!({
                "type": "function_call",
                "call_id": "c1",
                "name": "write_to_file",
                "arguments": serde_json::to_string(&json!({
                    "TargetFile": "src/a.rs",
                    "CodeContent": "fn a() {}"
                })).unwrap()
            }),
            json!({
                "type": "function_call",
                "call_id": "c2",
                "name": "write_to_file",
                "arguments": serde_json::to_string(&json!({
                    "TargetFile": "src/b.rs",
                    "CodeContent": "fn b() {}"
                })).unwrap()
            }),
            json!({
                "type": "function_call_output",
                "call_id": "c1",
                "output": "Created src/a.rs",
                "exit_code": 0
            }),
            json!({
                "type": "function_call_output",
                "call_id": "c2",
                "output": "Created src/b.rs",
                "exit_code": 0
            }),
        ];
        let exchanges = vec![
            ToolExchange {
                call_source_index: 0,
                output_source_indices: vec![2],
                call_id: Some("c1".into()),
                tool_name: "write_to_file".into(),
                call_fingerprint: "fp1".into(),
                output_fingerprint: Some("out1".into()),
                completed: true,
            },
            ToolExchange {
                call_source_index: 1,
                output_source_indices: vec![3],
                call_id: Some("c2".into()),
                tool_name: "write_to_file".into(),
                call_fingerprint: "fp2".into(),
                output_fingerprint: Some("out2".into()),
                completed: true,
            },
        ];

        let ws = extract_observed_workspace_state(&items, &exchanges);
        assert_eq!(ws.changed_files.len(), 2);
        assert!(ws.changed_files.iter().any(|f| f.path == "src/a.rs"));
        assert!(ws.changed_files.iter().any(|f| f.path == "src/b.rs"));
    }

    #[test]
    fn mutation_without_positive_success_is_unknown() {
        let items = vec![
            json!({
                "type": "function_call",
                "call_id": "c_unknown",
                "name": "apply_patch",
                "arguments": "*** patch content ***"
            }),
            json!({
                "type": "function_call_output",
                "call_id": "c_unknown",
                "output": "unrecognized ambiguous status message"
            }),
        ];
        let exchanges = vec![ToolExchange {
            call_source_index: 0,
            output_source_indices: vec![1],
            call_id: Some("c_unknown".into()),
            tool_name: "apply_patch".into(),
            call_fingerprint: "fp".into(),
            output_fingerprint: Some("out".into()),
            completed: true,
        }];

        let ws = extract_observed_workspace_state(&items, &exchanges);
        assert!(
            ws.changed_files.is_empty(),
            "ambiguous mutation output without positive confirmation must not register file change"
        );
    }

    #[test]
    fn mutation_unsuccessful_is_not_confirmed_success() {
        let out_item = json!({
            "type": "function_call_output",
            "output": "Patch unsuccessful: rejected chunk #2"
        });
        let outcome = classify_mutation_outcome("apply_patch", &[&out_item]);
        assert_ne!(
            outcome,
            MutationOutcome::ConfirmedSuccess,
            "unsuccessful output must not be classified as ConfirmedSuccess"
        );
        assert_eq!(outcome, MutationOutcome::ConfirmedFailure);
    }

    #[test]
    fn mutation_identity_uses_targets_and_payload_digest() {
        let patch_a = json!({
            "name": "apply_patch",
            "arguments": "*** Begin Patch\n*** Update File: src/a.rs\n+fn a() {}\n*** End Patch"
        });
        let patch_b = json!({
            "name": "apply_patch",
            "arguments": "*** Begin Patch\n*** Update File: src/b.rs\n+fn b() {}\n*** End Patch"
        });
        let a = mutation_identity_from_call(&patch_a).unwrap();
        let b = mutation_identity_from_call(&patch_b).unwrap();
        assert_eq!(a.targets, vec!["src/a.rs".to_string()]);
        assert_eq!(b.targets, vec!["src/b.rs".to_string()]);
        assert_ne!(a.payload_digest, b.payload_digest);
        let a_again = mutation_identity_from_call(&patch_a).unwrap();
        assert_eq!(a.payload_digest, a_again.payload_digest);

        let write = json!({
            "name": "write_to_file",
            "arguments": {"path": "src/a.rs", "contents": "fn a() {}"}
        });
        let write_id = mutation_identity_from_call(&write).unwrap();
        assert_eq!(write_id.targets, vec!["src/a.rs".to_string()]);
        let write_same_path_new_body = json!({
            "name": "write_to_file",
            "arguments": {"path": "src/a.rs", "contents": "fn a2() {}"}
        });
        let write2 = mutation_identity_from_call(&write_same_path_new_body).unwrap();
        assert_ne!(write_id.payload_digest, write2.payload_digest);

        let edit = json!({
            "name": "edit_file",
            "arguments": {
                "path": "src/a.rs",
                "old_string": "fn a() {}",
                "new_string": "fn a2() {}"
            }
        });
        let edit_id = mutation_identity_from_call(&edit).unwrap();
        assert_eq!(edit_id.targets, vec!["src/a.rs".to_string()]);
        assert!(!edit_id.payload_digest.is_empty());
    }

    #[test]
    fn empty_patch_output_is_unknown() {
        let out_item = json!({
            "type": "function_call_output",
            "output": ""
        });
        let outcome = classify_mutation_outcome("apply_patch", &[&out_item]);
        assert_eq!(
            outcome,
            MutationOutcome::Unknown,
            "empty output without explicit exit code or success string must be Unknown"
        );
    }

    #[test]
    fn test_normalized_command_key_collapses_whitespace() {
        assert_eq!(normalize_command_key("cargo test foo"), "cargo test foo");
        assert_eq!(
            normalize_command_key("cargo   test   foo"),
            "cargo test foo"
        );
        assert_eq!(
            normalize_command_key("  cargo\ttest\nfoo  "),
            "cargo test foo"
        );

        let items = vec![
            json!({"type": "function_call", "call_id": "c1", "name": "exec_command", "arguments": "{\"command\": \"cargo test foo\"}"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": "Exit code: 1\nfailed test"}),
            json!({"type": "function_call", "call_id": "c2", "name": "exec_command", "arguments": "{\"command\": \"cargo   test foo\"}"}),
            json!({"type": "function_call_output", "call_id": "c2", "output": "Exit code: 0\npassed test"}),
        ];
        let exchanges = vec![
            ToolExchange {
                call_source_index: 0,
                output_source_indices: vec![1],
                call_id: Some("c1".into()),
                tool_name: "exec_command".into(),
                call_fingerprint: "fp1".into(),
                output_fingerprint: Some("out1".into()),
                completed: true,
            },
            ToolExchange {
                call_source_index: 2,
                output_source_indices: vec![3],
                call_id: Some("c2".into()),
                tool_name: "exec_command".into(),
                call_fingerprint: "fp2".into(),
                output_fingerprint: Some("out2".into()),
                completed: true,
            },
        ];
        let ws = extract_observed_workspace_state(&items, &exchanges);
        assert_eq!(
            ws.tests.len(),
            1,
            "whitespace-differing test commands must normalize to single entry"
        );
        assert_eq!(
            ws.tests[0].status,
            TestStatus::Passed,
            "latest test run must win"
        );
    }

    #[test]
    fn test_incomplete_test_command_is_not_counted_as_progress() {
        let items = vec![
            json!({"type": "function_call", "call_id": "c1", "name": "exec_command", "arguments": "{\"command\": \"cargo test\"}"}),
        ];
        let exchanges = vec![ToolExchange {
            call_source_index: 0,
            output_source_indices: vec![],
            call_id: Some("c1".into()),
            tool_name: "exec_command".into(),
            call_fingerprint: "fp1".into(),
            output_fingerprint: None,
            completed: false,
        }];
        let ws = extract_observed_workspace_state(&items, &exchanges);
        assert!(
            ws.tests.is_empty(),
            "incomplete test call must not produce TestState"
        );
        assert_eq!(
            ws.last_progress_source_index, None,
            "incomplete test call must not be counted as progress"
        );
        assert_eq!(
            ws.pending_tool_calls.len(),
            1,
            "incomplete test call must be recorded in pending_tool_calls"
        );
    }
}
