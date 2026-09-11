//! Shared deterministic tool-output envelope normalization.
//!
//! Strips transport wrappers and volatile metadata. Does not guess
//! investigation questions or treat a non-empty wrapper as progress.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Coarse structured status from a tool payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Success,
    Failed,
    Cancelled,
    Unknown,
}

/// Family of a tool or command error. Distinct from a successful empty result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ErrorFamily {
    PermissionDenied,
    NotFound,
    Locked,
    InvalidObject,
    ParseFailed,
    Timeout,
    Other,
}

/// Envelope-stripped view of one or more tool output payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedToolOutput {
    pub text: String,
    pub exit_code: Option<i32>,
    pub structured_status: Option<ToolStatus>,
    pub error_family: Option<ErrorFamily>,
    pub was_empty_payload: bool,
}

/// Normalize tool output values into a stable payload for downstream classifiers.
pub fn normalize_tool_output(outputs: &[&Value]) -> NormalizedToolOutput {
    let exit_code = outputs
        .iter()
        .find_map(|out| extract_structured_exit_code(out));
    let structured_status = outputs
        .iter()
        .find_map(|out| extract_structured_status(out));

    let mut combined = String::new();
    for out in outputs {
        if let Some(text) = extract_raw_text(out) {
            combined.push_str(&text);
        }
    }

    let stripped = strip_output_envelope(&combined);
    let text = normalize_payload_text(stripped);
    let was_empty_payload = text.trim().is_empty();
    let error_family = classify_error_family_from_parts(&text, exit_code, structured_status);

    NormalizedToolOutput {
        text,
        exit_code,
        structured_status,
        error_family,
        was_empty_payload,
    }
}

/// Normalize a pre-extracted output string (when JSON values are unavailable).
pub fn normalize_output_text(output: &str) -> String {
    normalize_payload_text(strip_output_envelope(output))
}

/// Remove Codex/Vellum exec envelopes, keeping only the command payload.
pub fn strip_output_envelope(output: &str) -> &str {
    const MARKERS: &[&str] = &[
        "\r\nFinal output:\r\n",
        "\r\nOriginal output:\r\n",
        "\nFinal output:\n",
        "\nOriginal output:\n",
        "\r\nFinal output:\n",
        "\nFinal output:\r\n",
        "\r\nOriginal output:\n",
        "\nOriginal output:\r\n",
    ];
    for marker in MARKERS {
        if let Some((_, payload)) = output.rsplit_once(marker) {
            return payload;
        }
    }
    for prefix in ["Final output:", "Original output:", "Output:"] {
        if let Some(rest) = output.strip_prefix(prefix) {
            return rest.trim_start_matches(['\r', '\n']);
        }
        let padded = format!("\n{prefix}\n");
        if let Some((_, payload)) = output.rsplit_once(&padded) {
            return payload;
        }
        let padded_crlf = format!("\r\n{prefix}\r\n");
        if let Some((_, payload)) = output.rsplit_once(&padded_crlf) {
            return payload;
        }
    }
    output
}

fn normalize_payload_text(text: &str) -> String {
    let without_ansi = strip_ansi(text);
    let mut normalized = without_ansi.replace("\r\n", "\n").replace('\r', "\n");
    normalized = strip_transport_metadata_lines(&normalized);
    // Collapse trailing transport-only newlines but keep meaningful inner blanks.
    normalized.trim_end_matches('\n').to_string()
}

fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i += 2;
            while i < bytes.len() {
                let b = bytes[i];
                i += 1;
                if (0x40..=0x7e).contains(&b) {
                    break;
                }
            }
            continue;
        }
        let ch = text[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn strip_transport_metadata_lines(text: &str) -> String {
    let mut kept = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("chunk id:")
            || lower.starts_with("wall time:")
            || lower.starts_with("elapsed:")
            || lower.starts_with("duration:")
            || lower.starts_with("process exited with code")
            || lower.starts_with("the command exited with code")
            || lower.starts_with("exit code:")
            || lower.starts_with("exit code ")
        {
            continue;
        }
        kept.push(line);
    }
    kept.join("\n")
}

fn extract_raw_text(output: &Value) -> Option<String> {
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

fn extract_structured_exit_code(output: &Value) -> Option<i32> {
    for key in ["exit_code", "exitCode", "code"] {
        if let Some(code) = output.get(key).and_then(Value::as_i64) {
            return Some(code as i32);
        }
    }
    if let Some(code) = output.get("status").and_then(Value::as_i64) {
        return Some(code as i32);
    }
    None
}

fn extract_structured_status(output: &Value) -> Option<ToolStatus> {
    match output.get("status") {
        Some(Value::String(s)) => {
            let raw = s.to_ascii_lowercase();
            Some(match raw.as_str() {
                "success" | "ok" | "completed" | "complete" => ToolStatus::Success,
                "failed" | "error" | "failure" => ToolStatus::Failed,
                "cancelled" | "canceled" | "aborted" => ToolStatus::Cancelled,
                _ => ToolStatus::Unknown,
            })
        }
        Some(Value::Bool(true)) => Some(ToolStatus::Success),
        Some(Value::Bool(false)) => Some(ToolStatus::Failed),
        _ => None,
    }
}

/// Classify an error family from already-normalized text and status.
pub fn classify_error_family_from_parts(
    text: &str,
    exit_code: Option<i32>,
    status: Option<ToolStatus>,
) -> Option<ErrorFamily> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("permission denied")
        || lower.contains("access is denied")
        || lower.contains("operation not permitted")
        || lower.contains("eacces")
    {
        return Some(ErrorFamily::PermissionDenied);
    }
    if lower.contains("database is locked")
        || lower.contains("sqlite_busy")
        || lower.contains("resource temporarily unavailable")
        || lower.contains("file is locked")
    {
        return Some(ErrorFamily::Locked);
    }
    if lower.contains("invalid object")
        || lower.contains("bad object")
        || lower.contains("not a git repository")
        || lower.contains("unknown revision")
        || lower.contains("ambiguous argument")
    {
        return Some(ErrorFamily::InvalidObject);
    }
    if lower.contains("syntaxerror")
        || lower.contains("jsondecodeerror")
        || lower.contains("parse error")
        || lower.contains("traceback (most recent call last)")
        || lower.contains("panic:")
    {
        return Some(ErrorFamily::ParseFailed);
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return Some(ErrorFamily::Timeout);
    }
    if lower.contains("cannot find")
        || lower.contains("no such file")
        || lower.contains("not found")
        || lower.contains("cannot find path")
    {
        return Some(ErrorFamily::NotFound);
    }
    if status == Some(ToolStatus::Failed) {
        return Some(ErrorFamily::Other);
    }
    if let Some(code) = exit_code {
        if code != 0 && code != 1 {
            return Some(ErrorFamily::Other);
        }
    }
    if lower.contains("error:") || lower.contains("exception:") || lower.contains("command failed")
    {
        return Some(ErrorFamily::Other);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_final_original_and_output_envelopes() {
        assert_eq!(normalize_output_text("Chunk ID: 1\nFinal output:\n"), "");
        assert_eq!(
            normalize_output_text("Chunk ID: 1\r\nFinal output:\r\n"),
            ""
        );
        assert_eq!(normalize_output_text("Original output:\nhello"), "hello");
        assert_eq!(normalize_output_text("Output:\n\n"), "");
        assert_eq!(
            normalize_output_text("Process exited with code 0\nFinal output:\nfoo.txt:1:foo"),
            "foo.txt:1:foo"
        );
    }

    #[test]
    fn empty_payload_is_flagged() {
        let out = json!({
            "output": "Chunk ID: 123\nWall time: 0.1 seconds\nProcess exited with code 0\nFinal output:\n",
            "exit_code": 0
        });
        let normalized = normalize_tool_output(&[&out]);
        assert!(normalized.was_empty_payload);
        assert_eq!(normalized.text, "");
        assert_eq!(normalized.exit_code, Some(0));
    }

    #[test]
    fn strips_ansi_and_normalizes_crlf() {
        let text = "\u{1b}[31merror\u{1b}[0m\r\nnext";
        assert_eq!(normalize_output_text(text), "error\nnext");
    }

    #[test]
    fn structured_status_and_content_array() {
        let out = json!({
            "status": "success",
            "exit_code": 0,
            "content": [{"type": "output_text", "text": "True"}]
        });
        let normalized = normalize_tool_output(&[&out]);
        assert_eq!(normalized.text, "True");
        assert_eq!(normalized.structured_status, Some(ToolStatus::Success));
        assert_eq!(normalized.exit_code, Some(0));
        assert!(!normalized.was_empty_payload);
    }

    #[test]
    fn classifies_error_families() {
        let denied = json!({"output": "Permission denied: /secret", "exit_code": 13});
        assert_eq!(
            normalize_tool_output(&[&denied]).error_family,
            Some(ErrorFamily::PermissionDenied)
        );
        let locked = json!({"output": "database is locked", "exit_code": 5});
        assert_eq!(
            normalize_tool_output(&[&locked]).error_family,
            Some(ErrorFamily::Locked)
        );
        let parse =
            json!({"output": "Traceback (most recent call last):\nValueError", "exit_code": 1});
        assert_eq!(
            normalize_tool_output(&[&parse]).error_family,
            Some(ErrorFamily::ParseFailed)
        );
    }
}
