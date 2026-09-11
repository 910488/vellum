//! Shared SSE streaming machinery (plan M5).
//!
//! Moved verbatim from Desktop's `src-tauri/src/proxy.rs` so Desktop and the
//! headless runtime normalize third-party streams through one implementation:
//! incremental SSE parsing, provider replay heuristics, control-token
//! stripping, readable-reasoning normalization, message phase assignment,
//! tool-call delta accumulation, completion tracking, and the failure event
//! envelope. Desktop keeps thin re-exports at its call sites.

use serde_json::{json, Value};

use crate::adapter::{custom_tool_input, CodexToolContext, NamespaceToolContext};
use crate::sse::{append_utf8_safe, strip_sse_field, take_sse_block};

/// Upper bound on provider-supplied text quoted back in a protocol diagnostic.
/// A provider must never be able to grow an error message without limit, and
/// the diagnostic is only ever a shape description — never a payload body.
const MAX_PROTOCOL_DIAGNOSTIC_CHARS: usize = 160;

/// A known Responses protocol event arrived with a payload shape the protocol
/// does not define.
///
/// Known events are never forwarded as opaque passthrough. Doing so hands the
/// provider a way to reach handlers that assume the documented shape, which is
/// how a two-line SSE frame used to panic the streaming task. The caller
/// decides how to surface this: a canonical `provider_protocol` error while no
/// bytes have reached the client yet, or a single bounded terminal failure
/// event once the stream is already underway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseProtocolError {
    event: String,
    detail: String,
}

impl SseProtocolError {
    fn new(event: &str, detail: impl Into<String>) -> Self {
        Self {
            event: bounded_diagnostic(event),
            detail: bounded_diagnostic(&detail.into()),
        }
    }

    /// The protocol event name that failed validation.
    pub fn event(&self) -> &str {
        &self.event
    }

    /// The shape problem, already bounded. Never contains a payload body.
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl std::fmt::Display for SseProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "upstream sent a malformed `{}` event: {}",
            self.event, self.detail
        )
    }
}

impl std::error::Error for SseProtocolError {}

/// Truncate provider-influenced text to a fixed budget, on a char boundary.
fn bounded_diagnostic(text: &str) -> String {
    let single_line = text.replace(['\n', '\r'], " ");
    if single_line.chars().count() <= MAX_PROTOCOL_DIAGNOSTIC_CHARS {
        return single_line;
    }
    let mut clipped = single_line
        .chars()
        .take(MAX_PROTOCOL_DIAGNOSTIC_CHARS.saturating_sub(1))
        .collect::<String>();
    clipped.push('…');
    clipped
}

/// The JSON type name of a value, for shape diagnostics.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// True when the event name belongs to the Responses SSE protocol this
/// normalizer interprets. Everything else is provider-specific and keeps its
/// existing passthrough behavior — the protocol reserves the `response.`
/// namespace and the bare `error` event, so nothing outside it is ours to
/// validate.
fn is_known_protocol_event(event: &str) -> bool {
    event.starts_with("response.") || event == "error"
}

/// Validate a known protocol event's payload *before* any handler reads a
/// field from it.
///
/// This checks exactly the shapes the normalizer depends on: the payload
/// itself, the nested containers it rewrites in place, the strings it previews
/// or forwards, and the integers it uses as identity for replay suppression and
/// per-item state. A provider that violates any of them is a protocol error,
/// not an opaque event.
fn validate_known_sse_event(event: &str, value: &Value) -> Result<(), SseProtocolError> {
    if !is_known_protocol_event(event) {
        return Ok(());
    }
    let Some(object) = value.as_object() else {
        return Err(SseProtocolError::new(
            event,
            format!(
                "payload is {} where the protocol defines an object",
                json_kind(value)
            ),
        ));
    };

    // Containers the normalizer rewrites in place (`item`, `part`, `response`).
    // An absent or null container is legal — the handlers already skip it — but
    // a scalar or array in its place is not.
    for field in ["item", "part", "response"] {
        if let Some(nested) = object.get(field) {
            if !nested.is_object() && !nested.is_null() {
                return Err(SseProtocolError::new(
                    event,
                    format!(
                        "`{field}` is {} where the protocol defines an object",
                        json_kind(nested)
                    ),
                ));
            }
        }
    }

    // Text the normalizer abbreviates or forwards verbatim. A non-string here
    // would silently degrade to an empty preview and leak the unabbreviated
    // value onward instead.
    for field in ["delta", "text", "arguments", "input"] {
        if let Some(nested) = object.get(field) {
            if !nested.is_string() && !nested.is_null() {
                return Err(SseProtocolError::new(
                    event,
                    format!(
                        "`{field}` is {} where the protocol defines a string",
                        json_kind(nested)
                    ),
                ));
            }
        }
    }

    // `sequence_number` is the protocol's event identity, and the only thing
    // that suppresses a replayed frame. A float or string there does not fail
    // loudly on its own — it just disables deduplication — so reject it here.
    // The per-item indices address normalizer state; a bad one silently folds
    // distinct items onto index 0.
    for field in [
        "sequence_number",
        "output_index",
        "content_index",
        "summary_index",
    ] {
        if let Some(nested) = object.get(field) {
            if !nested.is_null() && nested.as_u64().is_none() {
                return Err(SseProtocolError::new(
                    event,
                    format!(
                        "`{field}` is {} where the protocol defines a non-negative integer",
                        json_kind(nested)
                    ),
                ));
            }
        }
    }

    Ok(())
}

/// The client-visible failure envelope for stream-level errors. Matches
/// Desktop's `failed_sse_event` byte-for-byte.
pub fn failed_sse_event(message: &str) -> String {
    failed_sse_event_inner(None, message)
}

/// The same envelope, carrying a canonical [`crate::error::RuntimeError`]
/// category.
///
/// A streaming response commits its status line and headers before the first
/// upstream block is read, so a mid-stream failure can no longer be answered
/// with the HTTP error envelope. Attaching the category here keeps the client
/// on the same taxonomy the boundary would have used, instead of degrading a
/// protocol failure into an unclassified stream error.
pub fn failed_sse_event_with_category(category: &str, message: &str) -> String {
    failed_sse_event_inner(Some(category), message)
}

fn failed_sse_event_inner(category: Option<&str>, message: &str) -> String {
    let mut error = json!({
        "type": "vellum_upstream_stream_error",
        "message": message
    });
    if let (Some(category), Some(object)) = (category, error.as_object_mut()) {
        object.insert("category".into(), json!(category));
    }
    let value = json!({
        "type": "response.failed",
        "sequence_number": 0,
        "response": {
            "object": "response",
            "status": "failed",
            "error": error
        }
    });
    format!("event: response.failed\ndata: {value}\n\n")
}

/// Normalize a third-party SSE block that failed to parse as a JSON protocol
/// event. Provider control tokens are stripped; blocks left with no `data`
/// line are dropped.
pub fn sanitize_unparsed_sse_block(block: &str) -> Vec<String> {
    let cleaned = strip_provider_control_tokens_from_text(block);
    if cleaned == block {
        return vec![format!("{block}\n\n")];
    }
    let has_nonempty_data = cleaned
        .lines()
        .filter_map(|line| strip_sse_field(line, "data"))
        .any(|data| !data.trim().is_empty());
    if has_nonempty_data {
        vec![format!("{cleaned}\n\n")]
    } else {
        Vec::new()
    }
}

/// Parse one SSE block into `(event name, data value)`. The event name comes
/// from the `event:` line, falling back to the payload's `type` field and
/// finally `message`.
pub fn parse_sse_value(block: &str) -> Option<(String, Value)> {
    let event = block
        .lines()
        .find_map(|line| strip_sse_field(line, "event"))
        .map(str::to_owned);
    let data = block
        .lines()
        .find_map(|line| strip_sse_field(line, "data"))?;
    let value = serde_json::from_str::<Value>(data).ok()?;
    let event = event
        .or_else(|| value.get("type").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "message".into());
    Some((event, value))
}

/// Serialize one protocol event back to the wire format
/// `event: <name>\ndata: <json>\n\n`.
pub fn format_sse_value(event: &str, value: Value) -> String {
    format!("event: {event}\ndata: {value}\n\n")
}

pub fn sse_value_contains_function_call(value: &Value) -> bool {
    matches!(
        value.pointer("/item/type").and_then(Value::as_str),
        Some("function_call" | "custom_tool_call" | "tool_search_call")
    ) || value
        .pointer("/response/output")
        .and_then(Value::as_array)
        .is_some_and(|output| {
            output.iter().any(|item| {
                matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("function_call" | "custom_tool_call" | "tool_search_call")
                )
            })
        })
}

pub fn response_contains_function_call(response: &Value) -> bool {
    response
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|output| {
            output.iter().any(|item| {
                matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("function_call" | "custom_tool_call" | "tool_search_call")
                )
            })
        })
}

pub fn response_contains_visible_text(response: &Value) -> bool {
    response
        .get("output")
        .and_then(Value::as_array)
        .is_some_and(|output| {
            output.iter().any(|item| {
                item.get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        parts.iter().any(|part| {
                            part.get("text")
                                .and_then(Value::as_str)
                                .is_some_and(|text| !text.trim().is_empty())
                        })
                    })
            })
        })
}

pub fn set_message_phase_at_pointer(value: &mut Value, pointer: &str, phase: &str) {
    let Some(message) = value.pointer_mut(pointer).and_then(Value::as_object_mut) else {
        return;
    };
    if message.get("type").and_then(Value::as_str) == Some("message")
        && message.get("phase").is_none_or(Value::is_null)
    {
        message.insert("phase".into(), json!(phase));
    }
}

pub fn assign_response_message_phases_with(response: &mut Value, phase: &str) {
    let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) else {
        return;
    };
    for item in output {
        if item.get("type").and_then(Value::as_str) == Some("message")
            && item.get("phase").is_none_or(Value::is_null)
        {
            // `get` only answers on an object, so this always matches; keeping
            // it fallible means no provider payload can reach an `expect`.
            if let Some(object) = item.as_object_mut() {
                object.insert("phase".into(), json!(phase));
            }
        }
    }
}

pub fn assign_response_message_phases(response: &mut Value) {
    let phase = if response_contains_function_call(response) {
        "commentary"
    } else {
        "final_answer"
    };
    assign_response_message_phases_with(response, phase);
}

/// Third-party providers may deliver reasoning as raw provider text or as a
/// readable summary. Normalize every reasoning object to the readable
/// `summary` form Codex understands and never forward opaque ciphertext.
pub fn normalize_third_party_readable_reasoning(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("reasoning") {
                let readable = object
                    .get("reasoning_content")
                    .or_else(|| object.get("content"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string);
                let has_summary =
                    object
                        .get("summary")
                        .and_then(Value::as_array)
                        .is_some_and(|parts| {
                            parts.iter().any(|part| {
                                part.get("text")
                                    .and_then(Value::as_str)
                                    .is_some_and(|text| !text.trim().is_empty())
                            })
                        });
                if !has_summary {
                    if let Some(readable) = readable {
                        object.insert(
                            "summary".into(),
                            json!([{"type": "summary_text", "text": readable}]),
                        );
                    }
                }
                object.remove("encrypted_content");
                object.remove("content");
            }
            object.remove("reasoning_content");
            for child in object.values_mut() {
                normalize_third_party_readable_reasoning(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                normalize_third_party_readable_reasoning(child);
            }
        }
        _ => {}
    }
}

pub fn strip_provider_control_tokens(value: &mut Value) {
    strip_provider_control_tokens_with_options(value, false);
}

pub fn strip_provider_control_tokens_with_options(
    value: &mut Value,
    collapse_adjacent_replays: bool,
) {
    match value {
        Value::Object(object) => {
            let is_output_text_event = matches!(
                object.get("type").and_then(Value::as_str),
                Some("response.output_text.delta" | "response.output_text.done")
            );
            let is_output_text_item =
                object.get("type").and_then(Value::as_str) == Some("output_text");
            if is_output_text_event {
                if let Some(Value::String(text)) = object.get_mut("delta") {
                    *text = strip_provider_control_tokens_from_text_with_options(
                        text,
                        collapse_adjacent_replays,
                    );
                }
                if let Some(Value::String(text)) = object.get_mut("text") {
                    *text = strip_provider_control_tokens_from_text_with_options(
                        text,
                        collapse_adjacent_replays,
                    );
                }
            }
            if is_output_text_item {
                if let Some(Value::String(text)) = object.get_mut("text") {
                    *text = strip_provider_control_tokens_from_text_with_options(
                        text,
                        collapse_adjacent_replays,
                    );
                }
            }
            for child in object.values_mut() {
                strip_provider_control_tokens_with_options(child, collapse_adjacent_replays);
            }
        }
        Value::Array(values) => {
            for child in values {
                strip_provider_control_tokens_with_options(child, collapse_adjacent_replays);
            }
        }
        _ => {}
    }
}

fn strip_provider_control_tokens_from_text(text: &str) -> String {
    strip_provider_control_tokens_from_text_with_options(text, false)
}

fn strip_provider_control_tokens_from_text_with_options(
    text: &str,
    collapse_adjacent_replays: bool,
) -> String {
    const TOKENS: [&str; 3] = ["<|eos|>", "<|endoftext|>", "<|im_end|>"];
    let cleaned = TOKENS.iter().fold(text.to_string(), |cleaned, token| {
        cleaned.replace(token, "")
    });
    if collapse_adjacent_replays {
        collapse_adjacent_provider_replay_lines(&cleaned)
    } else {
        cleaned
    }
}

/// Some third-party Responses implementations replay a complete commentary
/// line after an intervening metadata event. The stream-level sequence check
/// cannot identify that replay when the provider assigns a fresh sequence
/// number. Collapse only adjacent, exact, non-trivial lines so ordinary token
/// repetition and intentionally short formatting remain untouched.
pub fn collapse_adjacent_provider_replay_lines(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut previous_nonempty: Option<String> = None;

    for segment in text.split_inclusive('\n') {
        let line = segment.trim_end_matches(['\r', '\n']);
        let normalized = line.trim();
        let replayed = normalized.chars().count() >= 8
            && previous_nonempty
                .as_deref()
                .is_some_and(|previous| previous == normalized);
        if !replayed {
            output.push_str(segment);
        }
        if !normalized.is_empty() {
            previous_nonempty = Some(normalized.to_string());
        }
    }

    output
}

pub fn terminal_error_from_sse(text: &str) -> Option<String> {
    let mut tracker = SseCompletionTracker::default();
    tracker.feed_text(text);
    tracker.terminal_error()
}

pub fn completed_response_from_sse(text: &str) -> Option<Value> {
    let mut tracker = SseCompletionTracker::default();
    tracker.feed_text(text);
    tracker.completed_response()
}

pub fn sse_event_is_response_completed(event: &str) -> bool {
    event.contains("event: response.completed")
        || event
            .lines()
            .find_map(|line| strip_sse_field(line, "data"))
            .and_then(|data| serde_json::from_str::<Value>(data).ok())
            .and_then(|value| {
                value
                    .get("type")
                    .and_then(Value::as_str)
                    .map(|kind| kind == "response.completed")
            })
            .unwrap_or(false)
}

/// Incremental SSE completion tracker. Each block is parsed once; completed
/// output items accumulate without rescanning the full transcript.
#[derive(Default)]
pub struct SseCompletionTracker {
    utf8: String,
    remainder: Vec<u8>,
    completed_items: Vec<Value>,
    completed_response: Option<Value>,
    terminal_error: Option<String>,
    just_completed: bool,
}

impl SseCompletionTracker {
    pub fn push_chunk(&mut self, bytes: &[u8]) -> Option<Value> {
        self.just_completed = false;
        append_utf8_safe(&mut self.utf8, &mut self.remainder, bytes);
        while let Some(block) = take_sse_block(&mut self.utf8) {
            self.feed_block(&block);
        }
        if self.just_completed {
            self.completed_response()
        } else {
            None
        }
    }

    pub fn feed_text(&mut self, text: &str) {
        self.just_completed = false;
        self.utf8.push_str(text);
        while let Some(block) = take_sse_block(&mut self.utf8) {
            self.feed_block(&block);
        }
    }

    fn feed_block(&mut self, block: &str) {
        let Some(data) = block.lines().find_map(|line| strip_sse_field(line, "data")) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("response.output_item.done") => {
                if let Some(item) = value.get("item").cloned() {
                    self.completed_items.push(item);
                }
            }
            Some("response.completed") => {
                self.completed_response = value.get("response").cloned();
                self.just_completed = true;
            }
            Some("response.failed" | "error") => {
                self.terminal_error = value
                    .pointer("/response/error/message")
                    .or_else(|| value.pointer("/error/message"))
                    .or_else(|| value.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            _ => {}
        }
    }

    pub fn completed_response(&self) -> Option<Value> {
        let mut last = self.completed_response.clone()?;
        let output = last
            .as_object_mut()?
            .entry("output")
            .or_insert_with(|| json!([]))
            .as_array_mut()?;
        for item in &self.completed_items {
            let item_id = item.get("id").and_then(Value::as_str);
            let duplicate = output.iter().any(|existing| {
                item_id
                    .zip(existing.get("id").and_then(Value::as_str))
                    .is_some_and(|(left, right)| left == right)
                    || existing == item
            });
            if !duplicate {
                output.push(item.clone());
            }
        }
        Some(last)
    }

    pub fn terminal_error(&self) -> Option<String> {
        self.terminal_error.clone()
    }
}

#[derive(Default)]
struct NativeCustomCall {
    item_id: String,
    arguments: String,
}

/// Third-party Responses SSE normalizer: replay dedup, control-token
/// stripping, tool-call delta accumulation, readable reasoning, and message
/// phase assignment. Mirrors Desktop's `ThirdPartySseNormalizer` exactly.
#[derive(Default)]
pub struct ThirdPartySseNormalizer {
    pending_message_done: Vec<Value>,
    saw_function_call: bool,
    saw_visible_text: bool,
    /// When true, apply Grok-specific payload-equality replay heuristics for
    /// text deltas without sequence numbers. Non-Grok providers must keep
    /// legitimate repeated content (identical deltas with distinct sequence
    /// numbers, repeated code lines, etc.).
    payload_replay_heuristics: bool,
    seen_sequence_numbers: std::collections::HashSet<(String, u64)>,
    last_text_deltas: std::collections::HashMap<(usize, usize), String>,
    /// Every reasoning delta is forwarded to the client verbatim as it
    /// arrives (OpenCode parity). This still tracks each output item's full
    /// terminal summary so the durable continuation journal can be kept in
    /// sync even if a caller reconstructs the response from a partial
    /// transcript.
    completed_reasoning_summaries: std::collections::HashMap<String, Value>,
    /// Grok-only telemetry: count of payload-equality drops without sequence IDs.
    payload_replay_drops: u64,
    /// Private signals returned when `x-grok-doom-loop-check` is enabled.
    /// They are consumed by Vellum and must never leak into the Codex stream.
    grok_doom_loop_triggers: Vec<String>,
    tool_context: CodexToolContext,
    namespace_context: NamespaceToolContext,
    custom_calls: std::collections::HashMap<usize, NativeCustomCall>,
    tool_search_calls: std::collections::HashSet<usize>,
}

impl ThirdPartySseNormalizer {
    pub fn new(request: &Value, payload_replay_heuristics: bool) -> Self {
        Self {
            tool_context: CodexToolContext::from_request(request),
            namespace_context: NamespaceToolContext::from_request(request),
            payload_replay_heuristics,
            ..Self::default()
        }
    }

    /// Normalize one upstream SSE block.
    ///
    /// `Err` means a *known* protocol event arrived with a shape the protocol
    /// does not define. It is never a passthrough case: the caller must end the
    /// stream rather than forward the frame.
    pub fn push_block(&mut self, block: &str) -> Result<Vec<String>, SseProtocolError> {
        let (event, mut value) = match parse_sse_value(block) {
            Some(parsed) => parsed,
            None => {
                // Unparsed blocks are not protocol events. Never drop them by
                // payload equality ??that rewrites legitimate repeated content
                // for every provider. Sequence identity only applies to parsed
                // JSON events below.
                return Ok(sanitize_unparsed_sse_block(block));
            }
        };
        // Shape first, fields second. Every read below this line is on a
        // payload whose structure the protocol guarantees.
        validate_known_sse_event(&event, &value)?;
        self.capture_grok_doom_loop_triggers(&value);
        if event == "response.doom_loop_check"
            || value.get("type").and_then(Value::as_str) == Some("response.doom_loop_check")
        {
            // This is a Grok-private diagnostic event, not part of the OpenAI
            // Responses protocol understood by Codex.
            return Ok(Vec::new());
        }
        // The terminal response repeats the same private diagnostic as a
        // belt-and-braces copy. Preserve the terminal event but remove the
        // provider-only field before forwarding it to Codex.
        if let Some(response) = value.get_mut("response").and_then(Value::as_object_mut) {
            response.remove("doom_loop_check");
        }
        // A provider or intermediary may replay the last SSE frame while
        // recovering a streaming connection. Sequence numbers identify the
        // same protocol event unambiguously; forwarding it twice makes Codex
        // render duplicated text or execute the same tool delta twice.
        if let Some(sequence) = value.get("sequence_number").and_then(Value::as_u64) {
            if !self.seen_sequence_numbers.insert((event.clone(), sequence)) {
                return Ok(Vec::new());
            }
        }
        normalize_third_party_readable_reasoning(&mut value);
        // `response.reasoning_summary_text.delta`, `.done`, and
        // `response.reasoning_summary_part.done` are protocol events the
        // upstream itself emits for its documented reasoning channel (never
        // derived from opaque/pattern-matched content). Forward them
        // verbatim, in order, with no buffering, dedup, or preview
        // compression — OpenCode parity for third-party reasoning streams.
        if event == "response.output_item.done"
            && value.pointer("/item/type").and_then(Value::as_str) == Some("reasoning")
        {
            if let (Some(id), Some(summary)) = (
                value.pointer("/item/id").and_then(Value::as_str),
                value.pointer("/item/summary").cloned(),
            ) {
                self.completed_reasoning_summaries
                    .insert(id.to_string(), summary);
            }
        }
        strip_provider_control_tokens_with_options(&mut value, self.payload_replay_heuristics);
        let output_index = value
            .get("output_index")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let content_index = value
            .get("content_index")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        if event == "response.output_text.delta" {
            let delta = value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if delta.trim().is_empty() {
                return Ok(Vec::new());
            }
            let key = (output_index, content_index);
            let has_sequence = value
                .get("sequence_number")
                .and_then(Value::as_u64)
                .is_some();
            // Protocol identity is sequence_number. Payload equality alone is
            // not identity: models may emit the same text twice legitimately.
            // Only Grok (payload_replay_heuristics) may drop identical deltas
            // when the provider omits sequence numbers during reconnect replay.
            if self.payload_replay_heuristics
                && !has_sequence
                && self
                    .last_text_deltas
                    .get(&key)
                    .is_some_and(|previous| previous == delta)
            {
                // Explicit Grok-only heuristic: no sequence number means the
                // provider may be replaying. Track drops for telemetry.
                self.payload_replay_drops = self.payload_replay_drops.saturating_add(1);
                log::debug!(
                    "[SSE] grok payload-replay heuristic dropped identical delta (drops={})",
                    self.payload_replay_drops
                );
                return Ok(Vec::new());
            }
            self.last_text_deltas.insert(key, delta.to_string());
        }
        if matches!(
            event.as_str(),
            "response.output_text.delta" | "response.output_text.done"
        ) && value
            .get("delta")
            .or_else(|| value.get("text"))
            .and_then(Value::as_str)
            .is_some_and(|text| !text.trim().is_empty())
        {
            self.saw_visible_text = true;
        }

        if matches!(
            event.as_str(),
            "response.output_item.added" | "response.output_item.done"
        ) {
            if let Some(item) = value.get_mut("item") {
                if item
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|parts| {
                        parts.iter().any(|part| {
                            part.get("text")
                                .and_then(Value::as_str)
                                .is_some_and(|text| !text.trim().is_empty())
                        })
                    })
                {
                    self.saw_visible_text = true;
                }
                let item_id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if self.tool_context.restore_output_item(item) {
                    if item.get("type").and_then(Value::as_str) == Some("tool_search_call") {
                        self.tool_search_calls.insert(output_index);
                    } else {
                        let state = self.custom_calls.entry(output_index).or_default();
                        state.item_id = item_id;
                        if event == "response.output_item.done"
                            && item
                                .get("input")
                                .and_then(Value::as_str)
                                .is_none_or(str::is_empty)
                            && !state.arguments.is_empty()
                        {
                            // `restore_output_item` only returns true after it has
                            // itself rewritten the item as an object.
                            if let Some(object) = item.as_object_mut() {
                                object.insert(
                                    "input".into(),
                                    json!(custom_tool_input(&state.arguments)),
                                );
                            }
                        }
                    }
                }
                self.namespace_context.restore_item(item);
            }
        }

        if event == "response.function_call_arguments.delta"
            && self.tool_search_calls.contains(&output_index)
        {
            return Ok(Vec::new());
        }

        if event == "response.function_call_arguments.done"
            && self.tool_search_calls.contains(&output_index)
        {
            return Ok(Vec::new());
        }

        if event == "response.function_call_arguments.delta"
            && self.custom_calls.contains_key(&output_index)
        {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                self.custom_calls
                    .entry(output_index)
                    .or_default()
                    .arguments
                    .push_str(delta);
            }
            return Ok(Vec::new());
        }

        if event == "response.function_call_arguments.done"
            && self.custom_calls.contains_key(&output_index)
        {
            let state = self.custom_calls.entry(output_index).or_default();
            if let Some(arguments) = value.get("arguments").and_then(Value::as_str) {
                state.arguments = arguments.to_string();
            }
            let input = custom_tool_input(&state.arguments);
            let item_id = state.item_id.clone();
            let mut output = Vec::new();
            if !input.is_empty() {
                output.push(format_sse_value(
                    "response.custom_tool_call_input.delta",
                    json!({
                        "type": "response.custom_tool_call_input.delta",
                        "item_id": item_id,
                        "output_index": output_index,
                        "delta": input
                    }),
                ));
            }
            output.push(format_sse_value(
                "response.custom_tool_call_input.done",
                json!({
                    "type": "response.custom_tool_call_input.done",
                    "item_id": item_id,
                    "output_index": output_index,
                    "input": input
                }),
            ));
            return Ok(output);
        }

        if sse_value_contains_function_call(&value) {
            self.saw_function_call = true;
        }
        if event == "response.output_item.done"
            && value.pointer("/item/type").and_then(Value::as_str) == Some("message")
        {
            self.pending_message_done.push(value);
            return Ok(Vec::new());
        }
        if event == "response.completed" {
            if let Some(response) = value.get_mut("response") {
                capture_response_reasoning(response, &mut self.completed_reasoning_summaries);
                self.tool_context.restore_response_tools(response);
                self.namespace_context.restore_response(response);
                if response_contains_visible_text(response) {
                    self.saw_visible_text = true;
                }
            }
            if value
                .get("response")
                .is_some_and(response_contains_function_call)
            {
                self.saw_function_call = true;
            }
            if !self.saw_visible_text && !self.saw_function_call {
                return Ok(vec![failed_sse_event(
                    "Upstream model ended with only a provider control token; retry the turn",
                )]);
            }
            let phase = if self.saw_function_call {
                "commentary"
            } else {
                "final_answer"
            };
            let mut output = self
                .pending_message_done
                .drain(..)
                .map(|mut pending| {
                    set_message_phase_at_pointer(&mut pending, "/item", phase);
                    format_sse_value("response.output_item.done", pending)
                })
                .collect::<Vec<_>>();
            if let Some(response) = value.get_mut("response") {
                assign_response_message_phases_with(response, phase);
            }
            output.push(format_sse_value(&event, value));
            return Ok(output);
        }
        Ok(vec![format_sse_value(&event, value)])
    }

    fn capture_grok_doom_loop_triggers(&mut self, value: &Value) {
        let triggers = value
            .pointer("/doom_loop_check/triggers")
            .or_else(|| value.pointer("/response/doom_loop_check/triggers"))
            .and_then(Value::as_array);
        let Some(triggers) = triggers else {
            return;
        };
        for trigger in triggers.iter().filter_map(Value::as_str) {
            if !self
                .grok_doom_loop_triggers
                .iter()
                .any(|known| known == trigger)
            {
                self.grok_doom_loop_triggers.push(trigger.to_string());
            }
        }
    }

    pub fn take_confident_doom_loop_trigger(&mut self) -> Option<String> {
        let index = self.grok_doom_loop_triggers.iter().position(|trigger| {
            let Some(rest) = trigger.strip_prefix("tail_repetition:") else {
                return false;
            };
            let Some((threshold, channel)) = rest.split_once('@') else {
                return false;
            };
            channel == "thinking"
                && threshold
                    .parse::<u32>()
                    .is_ok_and(|threshold| threshold <= 8)
        })?;
        Some(self.grok_doom_loop_triggers.remove(index))
    }

    /// Restore the provider's complete readable summary only for Vellum's
    /// durable replay journal. Client-facing events remain abbreviated.
    pub fn restore_full_reasoning_for_history(&self, response: &mut Value) {
        let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) else {
            return;
        };
        for item in output {
            let Some(id) = item.get("id").and_then(Value::as_str).map(str::to_string) else {
                continue;
            };
            let Some(summary) = self.completed_reasoning_summaries.get(&id) else {
                continue;
            };
            if let Some(object) = item.as_object_mut() {
                object.insert("summary".into(), summary.clone());
            }
        }
    }

    pub fn finish(&mut self) -> Vec<String> {
        let phase = if self.saw_function_call {
            "commentary"
        } else {
            "final_answer"
        };
        self.pending_message_done
            .drain(..)
            .map(|mut value| {
                set_message_phase_at_pointer(&mut value, "/item", phase);
                format_sse_value("response.output_item.done", value)
            })
            .collect()
    }
}

/// Record each reasoning output item's full terminal summary in `completed`
/// for the durable continuation journal. The item itself is left untouched —
/// the client-facing summary is already the full, unabbreviated text.
fn capture_response_reasoning(
    response: &mut Value,
    completed: &mut std::collections::HashMap<String, Value>,
) {
    let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) else {
        return;
    };
    for item in output {
        if item.get("type").and_then(Value::as_str) != Some("reasoning") {
            continue;
        }
        if let (Some(id), Some(summary)) = (
            item.get("id").and_then(Value::as_str),
            item.get("summary").cloned(),
        ) {
            completed.insert(id.to_string(), summary);
        }
    }
}

#[cfg(test)]
mod reasoning_preview_tests {
    use super::*;

    fn reasoning_delta(delta: &str) -> String {
        format_sse_value(
            "response.reasoning_summary_text.delta",
            json!({
                "type": "response.reasoning_summary_text.delta",
                "output_index": 0,
                "summary_index": 0,
                "delta": delta
            }),
        )
    }

    /// Reasoning chunks A -> B -> C from a compatible-Responses upstream must
    /// forward as three ordered, verbatim deltas: no buffering into a single
    /// progress sentence, no dropped chunks, no rewritten text.
    #[test]
    fn responses_reasoning_forwards_every_delta_verbatim_and_in_order() {
        let mut normalizer = ThirdPartySseNormalizer::new(&json!({}), false);
        let mut output = Vec::new();
        let chunks = [
            "Checking",
            " the",
            " mobile",
            " rendering.",
            " Private scratchpad.",
        ];
        for chunk in chunks {
            output.extend(
                normalizer
                    .push_block(&reasoning_delta(chunk))
                    .expect("well-formed"),
            );
        }
        assert_eq!(output.len(), chunks.len());
        for (block, chunk) in output.iter().zip(chunks.iter()) {
            assert!(
                block.contains(&format!("\"delta\":\"{chunk}\"")),
                "delta {chunk:?} was not forwarded verbatim: {block}"
            );
        }
    }

    #[test]
    fn terminal_response_keeps_the_full_reasoning_summary_for_client_and_history() {
        let full = "Checking the mobile rendering. Private scratchpad details.";
        let mut normalizer = ThirdPartySseNormalizer::new(&json!({}), false);
        let event = format_sse_value(
            "response.completed",
            json!({
                "type": "response.completed",
                "response": {
                    "output": [
                        {
                            "type": "reasoning",
                            "id": "reasoning_1",
                            "summary": [{"type": "summary_text", "text": full}]
                        },
                        {
                            "type": "message",
                            "id": "message_1",
                            "content": [{"type": "output_text", "text": "Done."}]
                        }
                    ]
                }
            }),
        );
        let transcript = normalizer.push_block(&event).expect("well-formed").join("");
        // The terminal item the client sees already carries the exact full
        // text — nothing here is truncated into a preview.
        assert!(transcript.contains(full));

        let mut history = json!({
            "output": [{
                "type": "reasoning",
                "id": "reasoning_1",
                "summary": [{"type": "summary_text", "text": full}]
            }]
        });
        normalizer.restore_full_reasoning_for_history(&mut history);
        assert_eq!(history["output"][0]["summary"][0]["text"], full);
    }

    #[test]
    fn completed_stream_preserves_real_provider_usage() {
        let mut normalizer = ThirdPartySseNormalizer::new(&json!({"input": "large"}), false);
        let event = format_sse_value(
            "response.completed",
            json!({
                "type": "response.completed",
                "response": {
                    "output": [{
                        "type": "message",
                        "content": [{"type": "output_text", "text": "done"}]
                    }],
                    "usage": {"input_tokens": 40, "output_tokens": 2, "total_tokens": 42}
                }
            }),
        );

        let transcript = normalizer.push_block(&event).unwrap().join("");
        let response = completed_response_from_sse(&transcript).unwrap();
        assert_eq!(response["usage"]["input_tokens"], 40);
        assert_eq!(response["usage"]["output_tokens"], 2);
        assert_eq!(response["usage"]["total_tokens"], 42);
    }

    #[test]
    fn compatible_responses_stream_restores_client_tool_search_without_function_deltas() {
        let request = json!({
            "tools": [{"type": "tool_search", "execution": "client"}]
        });
        let mut normalizer = ThirdPartySseNormalizer::new(&request, false);

        let added = format_sse_value(
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "id": "fc_provider",
                    "call_id": "call_search",
                    "name": "tool_search",
                    "arguments": "",
                    "status": "in_progress"
                }
            }),
        );
        let added = normalizer.push_block(&added).unwrap().join("");
        assert!(added.contains("\"type\":\"tool_search_call\""));
        assert!(added.contains("\"execution\":\"client\""));
        assert!(!added.contains("\"name\":\"tool_search\""));

        for event in [
            format_sse_value(
                "response.function_call_arguments.delta",
                json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": "fc_provider",
                    "output_index": 0,
                    "delta": "{\"query\":\"spawn"
                }),
            ),
            format_sse_value(
                "response.function_call_arguments.done",
                json!({
                    "type": "response.function_call_arguments.done",
                    "item_id": "fc_provider",
                    "output_index": 0,
                    "arguments": "{\"query\":\"spawn subagent\"}"
                }),
            ),
        ] {
            assert!(normalizer.push_block(&event).unwrap().is_empty());
        }

        let done = format_sse_value(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "id": "fc_provider",
                    "call_id": "call_search",
                    "name": "tool_search",
                    "arguments": "{\"query\":\"spawn subagent\"}",
                    "status": "completed"
                }
            }),
        );
        let done = normalizer.push_block(&done).unwrap().join("");
        assert!(done.contains("\"type\":\"tool_search_call\""));
        assert!(done.contains("\"query\":\"spawn subagent\""));
        assert!(!done.contains("response.function_call_arguments.done"));
    }
}
