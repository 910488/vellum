use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct EnhancedRuntimeObservation {
    pub enhanced_commit: Option<String>,
    pub runtime_digest: Option<String>,
    pub feature_profile: Option<String>,
    pub qwen_tool_reliability: Option<bool>,
    pub deepseek_context_recovery: Option<bool>,
    pub qwen_bounded_continuation: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct EventMetrics {
    pub session_id: Option<String>,
    pub tool_calls: u64,
    pub completed_tool_calls: u64,
    /// Tool executions that reached a successful terminal state. This is
    /// intentionally separate from `completed_tool_calls`: Codex emits
    /// `item.completed` for policy-declined shell commands as well.
    pub successful_tool_calls: u64,
    pub failed_tool_calls: u64,
    /// Tool calls rejected by the local Codex approval/exec policy before the
    /// provider task could observe their result.
    pub rejected_tool_calls: u64,
    pub malformed_tool_calls: u64,
    pub duplicate_commands: u64,
    pub reasoning_leaks: u64,
    pub retries: u64,
    pub compactions: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cached_tokens: u64,
    pub tool_duration_ms: u64,
    pub repeated_file_changes: u64,
    pub terminal_sse_missing: u64,
    pub disconnected_streams: u64,
    pub session_reinitializations: u64,
    pub errors: Vec<String>,
    pub commands: Vec<String>,
    pub changed_files: Vec<String>,
    /// Observed inside the running Enhanced process. These are never inferred
    /// from the requested CLI profile or the model name.
    pub enhanced_runtime: Option<EnhancedRuntimeObservation>,
    pub enhanced_event_counts: BTreeMap<String, u64>,
}

pub fn parse_codex_jsonl(text: &str) -> EventMetrics {
    let mut result = EventMetrics::default();
    let mut command_counts = HashMap::<String, u64>::new();
    let mut file_change_counts = HashMap::<String, u64>::new();
    for line in text.lines() {
        let Some(value) = parse_codex_jsonl_line(line) else {
            continue;
        };
        if result.session_id.is_none() {
            result.session_id = find_string(
                &value,
                &["thread_id", "threadId", "session_id", "sessionId"],
            );
        }
        let raw_event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        match value.get("method").and_then(Value::as_str) {
            Some("vellum/enhancedRuntimeIdentity") => {
                let params = value.get("params").unwrap_or(&Value::Null);
                let ports = params.get("ports").unwrap_or(&Value::Null);
                result.enhanced_runtime = Some(EnhancedRuntimeObservation {
                    enhanced_commit: params
                        .get("enhancedCommit")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    runtime_digest: params
                        .get("runtimeDigest")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    feature_profile: params
                        .get("featureProfile")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    qwen_tool_reliability: ports
                        .get("qwenToolReliability")
                        .and_then(Value::as_bool),
                    deepseek_context_recovery: ports
                        .get("deepseekContextRecovery")
                        .and_then(Value::as_bool),
                    qwen_bounded_continuation: ports
                        .get("qwenBoundedContinuation")
                        .and_then(Value::as_bool),
                });
            }
            Some("vellum/enhancedEvent") => {
                if let Some(name) = value
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .filter(|name| name.starts_with("enhanced."))
                {
                    *result
                        .enhanced_event_counts
                        .entry(name.to_string())
                        .or_default() += 1;
                }
            }
            _ => {}
        }
        // `codex exec --json` writes public item.started/item.completed events
        // to stdout, while the durable rollout stores the same activity as
        // response_item payloads. Windows Sandbox can be terminated before a
        // background PowerShell job flushes stdout, so accept the durable
        // representation as a telemetry fallback instead of reporting a
        // misleading 0-token/0-tool run.
        let item = value.get("item").or_else(|| {
            (raw_event_type == "response_item")
                .then(|| value.get("payload"))
                .flatten()
        });
        let item_type = item
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let event_type = match (raw_event_type, item_type) {
            ("response_item", "function_call" | "custom_tool_call" | "mcp_tool_call") => {
                "item.started"
            }
            (
                "response_item",
                "function_call_output" | "custom_tool_call_output" | "mcp_tool_call_output",
            ) => "item.completed",
            _ => raw_event_type,
        };
        let metric_item_type = match item_type {
            "function_call_output" => "function_call",
            "custom_tool_call_output" => "custom_tool_call",
            "mcp_tool_call_output" => "mcp_tool_call",
            other => other,
        };
        if event_type == "item.started"
            && matches!(
                metric_item_type,
                "command_execution"
                    | "mcp_tool_call"
                    | "function_call"
                    | "custom_tool_call"
                    | "file_change"
            )
        {
            result.tool_calls += 1;
        }
        if event_type == "item.completed"
            && matches!(
                metric_item_type,
                "command_execution"
                    | "mcp_tool_call"
                    | "function_call"
                    | "custom_tool_call"
                    | "file_change"
            )
        {
            result.completed_tool_calls += 1;
            let item = item.unwrap_or(&Value::Null);
            let status = item
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("completed");
            let exit_code = item.get("exit_code").and_then(Value::as_i64);
            if matches!(status, "declined" | "rejected") {
                result.rejected_tool_calls += 1;
                result.failed_tool_calls += 1;
            } else if status == "failed" || exit_code.is_some_and(|code| code != 0) {
                result.failed_tool_calls += 1;
            } else {
                result.successful_tool_calls += 1;
            }
        }
        if event_type == "item.failed"
            && matches!(
                metric_item_type,
                "command_execution"
                    | "mcp_tool_call"
                    | "function_call"
                    | "custom_tool_call"
                    | "file_change"
            )
        {
            result.completed_tool_calls += 1;
            result.failed_tool_calls += 1;
        }
        if event_type.contains("retry") || contains_text(&value, "reconnecting") {
            result.retries += 1;
        }
        // Count one completed history rewrite, not every lifecycle event. A
        // single Codex compaction may emit both `item.started(type=compact)`
        // and a terminal event; counting both lets a one-compaction run pass
        // the double-compaction gate.
        let completed_compaction = raw_event_type == "compacted"
            || raw_event_type == "compaction.completed"
            || (event_type == "item.completed"
                && metric_item_type.to_ascii_lowercase().contains("compact"));
        if completed_compaction {
            result.compactions += 1;
        }
        if event_type == "error" || event_type.ends_with(".failed") {
            if let Some(error) = find_string(&value, &["message", "error"]) {
                result.errors.push(error);
            }
        }
        // Codex emits the same command on both item.started and
        // item.completed. Count the execution once from its start event;
        // otherwise every successful command is falsely reported as a
        // duplicate.
        if event_type == "item.started" && item_type == "command_execution" {
            if let Some(command) =
                find_string(item.unwrap_or(&Value::Null), &["command", "cmd", "input"])
            {
                *command_counts.entry(command.clone()).or_default() += 1;
                result.commands.push(command);
            }
        }
        if event_type == "item.completed"
            && matches!(
                metric_item_type,
                "command_execution"
                    | "mcp_tool_call"
                    | "function_call"
                    | "custom_tool_call"
                    | "file_change"
            )
        {
            result.tool_duration_ms = result.tool_duration_ms.saturating_add(
                find_number(item.unwrap_or(&Value::Null), &["duration_ms", "durationMs"])
                    .unwrap_or(0),
            );
        }
        if event_type == "item.completed" && item_type == "file_change" {
            if let Some(path) = find_string(
                item.unwrap_or(&Value::Null),
                &["path", "file_path", "filePath"],
            ) {
                *file_change_counts.entry(path.clone()).or_default() += 1;
                result.changed_files.push(path);
            }
        }
        if matches!(
            metric_item_type,
            "function_call" | "custom_tool_call" | "mcp_tool_call"
        ) && (event_type == "item.started" || item_type == metric_item_type)
            && !tool_arguments_valid(item.unwrap_or(&Value::Null), metric_item_type)
        {
            result.malformed_tool_calls += 1;
        }
        // Only assistant-visible message output can be a reasoning leak.
        // Tool results, prompts, diagnostics and compacted history often
        // legitimately mention these sentinels and must not fail the model.
        let assistant_message = (raw_event_type == "item.completed"
            && item_type == "agent_message")
            || (raw_event_type == "response_item"
                && (item_type == "agent_message"
                    || (item_type == "message"
                        && item
                            .and_then(|item| item.get("role"))
                            .and_then(Value::as_str)
                            == Some("assistant"))));
        if assistant_message && item.is_some_and(assistant_message_contains_reasoning_marker) {
            result.reasoning_leaks += 1;
        }
        collect_usage(&value, &mut result);
    }
    result.duplicate_commands = command_counts
        .values()
        .map(|count| count.saturating_sub(1))
        .sum();
    result.repeated_file_changes = file_change_counts
        .values()
        .map(|count| count.saturating_sub(1))
        .sum();
    result
}

fn assistant_message_contains_reasoning_marker(value: &Value) -> bool {
    match value {
        Value::String(text) => {
            let lower = text.to_ascii_lowercase();
            lower.contains("<think>")
                || lower.contains("</think>")
                || lower.contains("<tool_call>")
                || lower.contains("reasoning_content")
        }
        Value::Array(values) => values
            .iter()
            .any(assistant_message_contains_reasoning_marker),
        Value::Object(values) => values.iter().any(|(key, value)| {
            key.eq_ignore_ascii_case("reasoning_content")
                || assistant_message_contains_reasoning_marker(value)
        }),
        _ => false,
    }
}

/// Codex JSONL can contain an otherwise-valid public event whose captured
/// Windows command output includes a raw path escape such as `\vellum`. The
/// JSON producer should have emitted `\\vellum`; dropping the whole terminal
/// event makes the evaluator falsely report an unpaired tool call. Repair only
/// invalid escapes *inside JSON strings*, then require serde_json to validate
/// the complete line. Structural corruption still fails closed.
fn parse_codex_jsonl_line(line: &str) -> Option<Value> {
    serde_json::from_str::<Value>(line).ok().or_else(|| {
        let repaired = repair_invalid_json_string_escapes(line)?;
        serde_json::from_str::<Value>(&repaired).ok()
    })
}

fn repair_invalid_json_string_escapes(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut repaired = Vec::with_capacity(bytes.len().saturating_add(8));
    let mut in_string = false;
    let mut changed = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'"' {
            in_string = !in_string;
            repaired.push(byte);
            index += 1;
            continue;
        }
        if in_string && byte == b'\\' {
            let valid_escape = bytes.get(index + 1).is_some_and(|next| {
                matches!(
                    *next,
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' | b'u'
                )
            });
            if !valid_escape {
                repaired.push(b'\\');
                changed = true;
                repaired.push(byte);
                index += 1;
                continue;
            }
            repaired.push(byte);
            repaired.push(bytes[index + 1]);
            index += 2;
            continue;
        }
        repaired.push(byte);
        index += 1;
    }
    changed.then(|| String::from_utf8_lossy(&repaired).into_owned())
}

fn collect_usage(value: &Value, metrics: &mut EventMetrics) {
    let Some(object) = value.as_object() else {
        if let Some(array) = value.as_array() {
            for value in array {
                collect_usage(value, metrics);
            }
        }
        return;
    };
    let looks_like_usage = object.keys().any(|key| {
        matches!(
            key.as_str(),
            "input_tokens"
                | "inputTokens"
                | "output_tokens"
                | "outputTokens"
                | "cached_input_tokens"
                | "cachedInputTokens"
        )
    });
    if looks_like_usage {
        metrics.input_tokens = metrics
            .input_tokens
            .max(number(object, &["input_tokens", "inputTokens"]));
        metrics.output_tokens = metrics
            .output_tokens
            .max(number(object, &["output_tokens", "outputTokens"]));
        metrics.reasoning_tokens = metrics
            .reasoning_tokens
            .max(number(object, &["reasoning_tokens", "reasoningTokens"]));
        metrics.cached_tokens = metrics.cached_tokens.max(number(
            object,
            &["cached_input_tokens", "cachedInputTokens"],
        ));
    }
    for child in object.values() {
        collect_usage(child, metrics);
    }
}

fn number(object: &serde_json::Map<String, Value>, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_u64))
        .unwrap_or(0)
}

fn contains_text(value: &Value, needle: &str) -> bool {
    serde_json::to_string(value)
        .map(|text| text.to_ascii_lowercase().contains(needle))
        .unwrap_or(false)
}

fn find_string(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(object) => {
            for key in keys {
                if let Some(value) = object.get(*key) {
                    if let Some(text) = value.as_str() {
                        return Some(text.to_string());
                    }
                    if let Some(message) = value.get("message").and_then(Value::as_str) {
                        return Some(message.to_string());
                    }
                }
            }
            object.values().find_map(|child| find_string(child, keys))
        }
        Value::Array(array) => array.iter().find_map(|child| find_string(child, keys)),
        _ => None,
    }
}

fn find_number(value: &Value, keys: &[&str]) -> Option<u64> {
    match value {
        Value::Object(object) => {
            for key in keys {
                if let Some(number) = object.get(*key).and_then(Value::as_u64) {
                    return Some(number);
                }
            }
            object.values().find_map(|child| find_number(child, keys))
        }
        Value::Array(array) => array.iter().find_map(|child| find_number(child, keys)),
        _ => None,
    }
}

fn tool_arguments_valid(item: &Value, item_type: &str) -> bool {
    let Some(arguments) = item.get("arguments").or_else(|| item.get("input")) else {
        return false;
    };
    match arguments {
        Value::Object(_) | Value::Array(_) => true,
        Value::String(_) if item_type == "custom_tool_call" => true,
        Value::String(text) => serde_json::from_str::<Value>(text)
            .map(|value| value.is_object() || value.is_array())
            .unwrap_or(false),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_codex_compact_events() {
        let input = concat!(
            "{\"type\":\"item.started\",\"item\":{\"type\":\"compact\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"compact\"}}\n",
        );
        let metrics = parse_codex_jsonl(input);
        assert_eq!(metrics.compactions, 1);
    }

    #[test]
    fn counts_two_completed_compactions_as_two() {
        let input = concat!(
            "{\"type\":\"compaction.completed\"}\n",
            "{\"type\":\"compaction.completed\"}\n",
        );
        assert_eq!(parse_codex_jsonl(input).compactions, 2);
    }

    #[test]
    fn parses_session_tools_usage_and_duplicates() {
        let input = concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"thread-1\"}\n",
            "{\"type\":\"item.started\",\"item\":{\"type\":\"command_execution\",\"command\":\"cargo test\"}}\n",
            "{\"type\":\"item.started\",\"item\":{\"type\":\"command_execution\",\"command\":\"cargo test\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\",\"duration_ms\":42}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"file_change\",\"path\":\"src/lib.rs\",\"duration_ms\":8}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"file_change\",\"path\":\"src/lib.rs\",\"duration_ms\":9}}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":120,\"output_tokens\":30,\"cached_input_tokens\":20}}\n"
        );
        let parsed = parse_codex_jsonl(input);
        assert_eq!(parsed.session_id.as_deref(), Some("thread-1"));
        assert_eq!(parsed.tool_calls, 2);
        assert_eq!(parsed.duplicate_commands, 1);
        assert_eq!(parsed.input_tokens, 120);
        assert_eq!(parsed.cached_tokens, 20);
        assert_eq!(parsed.tool_duration_ms, 59);
        assert_eq!(parsed.repeated_file_changes, 1);
    }

    #[test]
    fn detects_reasoning_leaks() {
        let parsed = parse_codex_jsonl(
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"<think>secret</think>\"}}",
        );
        assert_eq!(parsed.reasoning_leaks, 1);
    }

    #[test]
    fn ignores_reasoning_markers_in_tool_output_and_compaction_payloads() {
        let parsed = parse_codex_jsonl(concat!(
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\",\"aggregated_output\":\"fixture contains <think> and <tool_call>\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"output\":\"reasoning_content\"}}\n",
            "{\"type\":\"compaction.completed\",\"replacement\":\"<think> historical fixture\"}\n"
        ));
        assert_eq!(parsed.reasoning_leaks, 0);
    }

    #[test]
    fn detects_reasoning_leak_in_durable_assistant_message() {
        let parsed = parse_codex_jsonl(
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"<tool_call>bad</tool_call>\"}]}}",
        );
        assert_eq!(parsed.reasoning_leaks, 1);
    }

    #[test]
    fn parses_durable_rollout_tool_events_when_exec_stdout_is_unavailable() {
        let parsed = parse_codex_jsonl(concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"session-rollout\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"shell_command\",\"arguments\":\"{\\\"command\\\":\\\"Get-Content gate.py\\\"}\"}}\n",
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"output\":\"Exit code: 0\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":120,\"output_tokens\":30,\"cached_input_tokens\":20}}}}\n"
        ));
        assert_eq!(parsed.session_id.as_deref(), Some("session-rollout"));
        assert_eq!(parsed.tool_calls, 1);
        assert_eq!(parsed.completed_tool_calls, 1);
        assert_eq!(parsed.successful_tool_calls, 1);
        assert_eq!(parsed.input_tokens, 120);
        assert_eq!(parsed.output_tokens, 30);
        assert_eq!(parsed.cached_tokens, 20);
    }

    #[test]
    fn command_start_and_completion_are_one_execution() {
        let parsed = parse_codex_jsonl(concat!(
            "{\"type\":\"item.started\",\"item\":{\"type\":\"command_execution\",\"command\":\"cargo test\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\",\"command\":\"cargo test\",\"exit_code\":0}}\n"
        ));
        assert_eq!(parsed.tool_calls, 1);
        assert_eq!(parsed.successful_tool_calls, 1);
        assert_eq!(parsed.failed_tool_calls, 0);
        assert_eq!(parsed.commands, vec!["cargo test"]);
        assert_eq!(parsed.duplicate_commands, 0);
    }

    #[test]
    fn policy_decline_is_not_counted_as_a_successful_tool_execution() {
        let parsed = parse_codex_jsonl(concat!(
            "{\"type\":\"item.started\",\"item\":{\"type\":\"command_execution\",\"command\":\"Get-ChildItem | Select-Object Name\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\",\"status\":\"declined\",\"exit_code\":-1}}\n"
        ));
        assert_eq!(parsed.tool_calls, 1);
        assert_eq!(parsed.completed_tool_calls, 1);
        assert_eq!(parsed.successful_tool_calls, 0);
        assert_eq!(parsed.failed_tool_calls, 1);
        assert_eq!(parsed.rejected_tool_calls, 1);
    }

    #[test]
    fn invalid_windows_path_escape_does_not_drop_tool_terminal() {
        let parsed = parse_codex_jsonl(concat!(
            r#"{"type":"item.started","item":{"id":"item_21","type":"command_execution","command":"Get-Content case.wsb","status":"in_progress"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"item_21","type":"command_execution","aggregated_output":"HostFolder=C:\vellum\target","exit_code":0,"status":"completed"}}"#
        ));
        assert_eq!(parsed.tool_calls, 1);
        assert_eq!(parsed.completed_tool_calls, 1);
        assert_eq!(parsed.successful_tool_calls, 1);
    }

    #[test]
    fn accepts_structured_and_custom_tool_arguments() {
        let parsed = parse_codex_jsonl(concat!(
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"function_call\",\"arguments\":{\"path\":\"a\"}}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"mcp_tool_call\",\"arguments\":\"{\\\"q\\\":1}\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"custom_tool_call\",\"input\":\"plain DSL\"}}\n"
        ));
        assert_eq!(parsed.malformed_tool_calls, 0);

        let malformed = parse_codex_jsonl(
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"function_call\",\"arguments\":\"not-json\"}}",
        );
        assert_eq!(malformed.malformed_tool_calls, 1);
    }

    #[test]
    fn parses_observed_enhanced_identity_and_events() {
        let parsed = parse_codex_jsonl(concat!(
            "{\"method\":\"vellum/enhancedRuntimeIdentity\",\"params\":{\"enhancedCommit\":\"abc\",\"runtimeDigest\":\"sha256:def\",\"featureProfile\":\"E1\",\"ports\":{\"qwenToolReliability\":true,\"deepseekContextRecovery\":false,\"qwenBoundedContinuation\":false}}}\n",
            "{\"method\":\"vellum/enhancedEvent\",\"params\":{\"name\":\"enhanced.tool.duplicate_suppressed\",\"fields\":{}}}\n",
            "{\"method\":\"vellum/enhancedEvent\",\"params\":{\"name\":\"enhanced.tool.duplicate_suppressed\",\"fields\":{}}}\n"
        ));
        let runtime = parsed.enhanced_runtime.unwrap();
        assert_eq!(runtime.feature_profile.as_deref(), Some("E1"));
        assert_eq!(runtime.qwen_tool_reliability, Some(true));
        assert_eq!(
            parsed.enhanced_event_counts["enhanced.tool.duplicate_suppressed"],
            2
        );
    }
}
