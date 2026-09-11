//! Pure request/response translation helpers shared by Desktop and the
//! headless daemon.
//!
//! Everything in this module is a `Value`-in/`Value`-out (or
//! `String`-in/`String`-out) pure function or a context struct built purely
//! from request JSON: no `AppState`, no live shell detection, and no
//! delegation-runtime checks. Harness-profile-aware translation
//! (`prepare_upstream_request`, `responses_to_chat`, Grok normalization,
//! shell/tool-catalog snapshots, etc.) lives in `profile_adapter`, which is
//! driven by the resolved [`crate::harness::HarnessProfile`], the addressed
//! route, the executor contract, and proven delegation availability — all
//! resolved by the caller so Desktop and the daemon translate identically.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// Apply-patch tool contract and namespace-name flattening.
//
// Moved from `src-tauri/src/harness/tools.rs`. That file also carries a large
// Desktop-side subsystem (built-in tool schemas, shell-dependent snapshots)
// which stays there; only the pure patch-envelope parser and the namespace
// name-flattening helper needed by the contexts below move here.
// ---------------------------------------------------------------------------

pub const APPLY_PATCH_TOOL_NAME: &str = "apply_patch";

pub fn is_apply_patch(name: &str) -> bool {
    name == APPLY_PATCH_TOOL_NAME
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeKind {
    Add,
    Update,
    Delete,
}

impl FileChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub kind: FileChangeKind,
    pub added_lines: usize,
    pub removed_lines: usize,
}

/// Structured summary of a patch, kept so a successful apply reports real file
/// changes rather than an opaque "ok".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PatchSummary {
    pub changes: Vec<FileChange>,
}

impl PatchSummary {
    pub fn to_json(&self) -> Value {
        json!({
            "changes": self.changes.iter().map(|change| json!({
                "path": change.path,
                "kind": change.kind.as_str(),
                "addedLines": change.added_lines,
                "removedLines": change.removed_lines
            })).collect::<Vec<_>>()
        })
    }
}

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";

/// Normalize the one delimiter variant repeatedly emitted by Grok's function
/// transport (`*** Begin Patch ***` / `*** End Patch ***`). The extra trailing
/// stars are presentation fencing, not patch semantics. No hunk, path, or
/// content line is modified, and every other malformed shape remains for the
/// native Codex parser to reject.
pub fn normalize_patch_delimiters(patch: &str) -> String {
    let had_trailing_newline = patch.ends_with('\n');
    let mut lines = patch.lines().map(str::to_string).collect::<Vec<_>>();
    if let Some(first) = lines.iter_mut().find(|line| !line.trim().is_empty()) {
        if first.trim() == "*** Begin Patch ***" {
            *first = BEGIN.to_string();
        }
    }
    if let Some(last) = lines.iter_mut().rfind(|line| !line.trim().is_empty()) {
        if last.trim() == "*** End Patch ***" {
            *last = END.to_string();
        }
    }
    let mut normalized = lines.join("\n");
    if had_trailing_newline {
        normalized.push('\n');
    }
    normalized
}

/// Validate the patch envelope and report a *specific* parser error.
///
/// **Diagnostic, not authoritative.** Patches are applied by Codex's own
/// apply_patch runtime, which owns permission calculation, diff tracking, and
/// the error text the model actually receives. This checker runs alongside it
/// (see `CodexToolContext::restore_output_item`) so a malformed patch is
/// visible in Vellum's log with a line number rather than only as a downstream
/// failure. Apart from the separately documented delimiter normalization, it
/// does not gate, rewrite, or reject anything, and
/// [`PatchSummary`] is not yet consumed by any caller — it is the shape a
/// Vellum-side patch runtime would report.
pub fn validate_patch(patch: &str) -> Result<PatchSummary, String> {
    let lines: Vec<&str> = patch.lines().collect();
    let first = lines
        .iter()
        .position(|line| !line.trim().is_empty())
        .ok_or_else(|| {
            "patch is empty; expected a document starting with `*** Begin Patch`".to_string()
        })?;
    if lines[first].trim_end() != BEGIN {
        return Err(format!(
            "patch must start with `{BEGIN}`, found `{}`",
            lines[first].trim()
        ));
    }
    let last = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .unwrap_or(first);
    if lines[last].trim_end() != END {
        return Err(format!(
            "patch must end with `{END}`, found `{}`",
            lines[last].trim()
        ));
    }

    let mut summary = PatchSummary::default();
    let mut current: Option<FileChange> = None;
    for (offset, line) in lines[first + 1..last].iter().enumerate() {
        let number = first + offset + 2; // 1-based line number in the document
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            push_change(
                &mut summary,
                &mut current,
                path,
                FileChangeKind::Update,
                number,
            )?;
        } else if let Some(path) = line.strip_prefix("*** Add File: ") {
            push_change(
                &mut summary,
                &mut current,
                path,
                FileChangeKind::Add,
                number,
            )?;
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            push_change(
                &mut summary,
                &mut current,
                path,
                FileChangeKind::Delete,
                number,
            )?;
        } else if line.starts_with("***") {
            return Err(format!(
                "line {number}: unknown patch section `{}`; expected `*** Add File:`, `*** Update File:`, `*** Delete File:`, or `{END}`",
                line.trim()
            ));
        } else {
            let Some(change) = current.as_mut() else {
                return Err(format!(
                    "line {number}: content before any `*** Add File:` / `*** Update File:` / `*** Delete File:` section"
                ));
            };
            match change.kind {
                FileChangeKind::Delete => {
                    if !line.trim().is_empty() {
                        return Err(format!(
                            "line {number}: `*** Delete File:` sections take no body, found `{}`",
                            line.trim()
                        ));
                    }
                }
                FileChangeKind::Add => {
                    if let Some(rest) = line.strip_prefix('+') {
                        let _ = rest;
                        change.added_lines += 1;
                    } else if !line.trim().is_empty() {
                        return Err(format!(
                            "line {number}: every line of an `*** Add File:` section must start with `+`, found `{}`",
                            line.trim()
                        ));
                    }
                }
                FileChangeKind::Update => match line.chars().next() {
                    Some('+') => change.added_lines += 1,
                    Some('-') => change.removed_lines += 1,
                    Some('@') | Some(' ') | None => {}
                    Some(_) => {
                        return Err(format!(
                            "line {number}: update hunk lines must start with `+`, `-`, `@@`, or a space, found `{}`",
                            line.trim()
                        ));
                    }
                },
            }
        }
    }
    if let Some(change) = current.take() {
        validate_change(&change)?;
        summary.changes.push(change);
    }
    if summary.changes.is_empty() {
        return Err(format!(
            "patch contains no file sections between `{BEGIN}` and `{END}`"
        ));
    }
    Ok(summary)
}

fn push_change(
    summary: &mut PatchSummary,
    current: &mut Option<FileChange>,
    path: &str,
    kind: FileChangeKind,
    number: usize,
) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err(format!("line {number}: file section has an empty path"));
    }
    if let Some(previous) = current.take() {
        validate_change(&previous)?;
        summary.changes.push(previous);
    }
    *current = Some(FileChange {
        path: path.trim().to_string(),
        kind,
        added_lines: 0,
        removed_lines: 0,
    });
    Ok(())
}

fn validate_change(change: &FileChange) -> Result<(), String> {
    match change.kind {
        FileChangeKind::Add if change.added_lines == 0 => Err(format!(
            "`*** Add File: {}` has no `+` content lines; an empty new file must still declare its content",
            change.path
        )),
        FileChangeKind::Update if change.added_lines == 0 && change.removed_lines == 0 => {
            Err(format!(
                "`*** Update File: {}` contains no `+` or `-` lines, so it would not change anything",
                change.path
            ))
        }
        _ => Ok(()),
    }
}

/// Flattened name for a namespaced tool. Chat Completions and Grok have no
/// namespace tool type, so the namespace has to survive inside the name.
pub fn flatten_namespace_name(namespace: &str, name: &str) -> String {
    format!("{namespace}__{name}")
}

// ---------------------------------------------------------------------------
// Codex custom-tool and namespace-tool contexts (moved from
// `src-tauri/src/adapter.rs`).
// ---------------------------------------------------------------------------

const CUSTOM_TOOL_INPUT_FIELD: &str = "input";
const APPLY_PATCH_FIELD: &str = "patch";

/// Remembers which Codex tools were originally declared as free-form custom
/// tools. Chat-compatible providers can only see them as JSON functions, but
/// Codex Desktop must receive `custom_tool_call` items again to rebuild native
/// edit/patch cards after the app restarts.
#[derive(Debug, Clone, Default)]
pub struct CodexToolContext {
    custom_tools: HashSet<String>,
    tool_search: bool,
}

impl CodexToolContext {
    pub fn from_request(request: &Value) -> Self {
        let mut context = Self::default();
        if let Some(tools) = request.get("tools").and_then(Value::as_array) {
            for tool in tools {
                match tool.get("type").and_then(Value::as_str) {
                    Some("custom") => {
                        if let Some(name) = tool.get("name").and_then(Value::as_str) {
                            context.custom_tools.insert(name.to_string());
                        }
                    }
                    Some("tool_search") => context.tool_search = true,
                    _ => {}
                }
            }
        }
        context
    }

    pub fn is_custom(&self, name: &str) -> bool {
        self.custom_tools.contains(name)
    }

    pub fn is_tool_search(&self, name: &str) -> bool {
        self.tool_search && name == "tool_search"
    }

    pub fn restore_response_tools(&self, response: &mut Value) {
        let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) else {
            return;
        };
        for item in output {
            self.restore_output_item(item);
        }
    }

    pub fn restore_output_item(&self, item: &mut Value) -> bool {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return false;
        }
        let Some(name) = item.get("name").and_then(Value::as_str) else {
            return false;
        };
        let is_tool_search = self.is_tool_search(name);
        if !self.is_custom(name) && !is_tool_search {
            return false;
        }
        if is_tool_search {
            let arguments = match item.get("arguments") {
                Some(Value::String(arguments)) if arguments.trim().is_empty() => json!({}),
                Some(Value::String(arguments)) => serde_json::from_str(arguments)
                    .unwrap_or_else(|_| Value::String(arguments.clone())),
                Some(arguments) => arguments.clone(),
                None => json!({}),
            };
            let Some(object) = item.as_object_mut() else {
                return false;
            };
            object.insert("type".into(), json!("tool_search_call"));
            normalize_official_item_id(object);
            object.remove("name");
            object.insert("arguments".into(), arguments);
            object.insert("execution".into(), json!("client"));
            return true;
        }
        let mut input = custom_tool_input(
            item.get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        // Diagnostic only. Codex's native apply_patch parser stays the
        // authority — it is what actually applies the patch and produces the
        // error the model sees. Checking here just makes a malformed patch
        // visible in Vellum's own log with a line number, instead of surfacing
        // only as an opaque downstream failure.
        if is_apply_patch(name) {
            input = normalize_patch_delimiters(&input);
            if let Err(problem) = validate_patch(&input) {
                log::warn!("[Harness] model produced a malformed apply_patch: {problem}");
            }
        }
        let Some(object) = item.as_object_mut() else {
            return false;
        };
        object.insert("type".into(), json!("custom_tool_call"));
        normalize_official_item_id(object);
        object.remove("arguments");
        object.insert("input".into(), json!(input));
        true
    }
}

/// Recover the free-form body of a translated custom tool call.
///
/// Generic custom tools travel as `{"input": "..."}`. `apply_patch` gets its
/// own exact contract (issue #6 Phase 3) and travels as `{"patch": "..."}`, so
/// both field names are accepted here; the two never collide, and reading both
/// keeps older histories replayable after the contract change.
pub fn custom_tool_input(arguments: &str) -> String {
    if arguments.trim().is_empty() {
        return String::new();
    }
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get(CUSTOM_TOOL_INPUT_FIELD)
                .or_else(|| value.get(APPLY_PATCH_FIELD))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| arguments.to_string())
}

/// Remembers which Codex tools were declared as namespace groups (e.g.
/// `web.run`). Chat/Responses-compatible providers have no native namespace
/// type, so the namespace is folded into a flattened function name
/// (`web__run`). This context restores the original `{namespace, name}` form
/// on the response so Codex rebuilds the native `web.run` tool call and its
/// WebSearch task item.
///
/// Bidirectional mapping: request `namespace:web/tools:[run]` -> advertised
/// `web__run` -> response `function_call{name: web__run}` -> restored
/// `function_call{namespace: web, name: run}`.
#[derive(Debug, Clone, Default)]
pub struct NamespaceToolContext {
    /// flattened name (`web__run`) -> (namespace, child name)
    flattened: HashSet<(String, String)>,
}

impl NamespaceToolContext {
    pub fn from_request(request: &Value) -> Self {
        let mut context = Self::default();
        if let Some(tools) = request.get("tools").and_then(Value::as_array) {
            for tool in tools {
                if tool.get("type").and_then(Value::as_str) == Some("namespace") {
                    let namespace = tool
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("namespace")
                        .to_string();
                    if let Some(children) = tool.get("tools").and_then(Value::as_array) {
                        for child in children {
                            if let Some(name) = child.get("name").and_then(Value::as_str) {
                                context
                                    .flattened
                                    .insert((namespace.clone(), name.to_string()));
                            }
                        }
                    }
                }
            }
        }
        context
    }

    pub fn is_empty(&self) -> bool {
        self.flattened.is_empty()
    }

    /// If `name` matches a flattened namespace tool (`namespace__child`),
    /// return the split `(namespace, child)`.
    pub fn split(&self, name: &str) -> Option<(&str, &str)> {
        for (namespace, child) in &self.flattened {
            let flattened = flatten_namespace_name(namespace, child);
            if flattened == name {
                return Some((namespace.as_str(), child.as_str()));
            }
        }
        None
    }

    /// Restore the `namespace` field on a `function_call` whose flattened name
    /// maps to a known namespace tool. Returns true when the item was changed.
    pub fn restore_item(&self, item: &mut Value) -> bool {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return false;
        }
        let name = match item.get("name").and_then(Value::as_str) {
            Some(name) => name.to_string(),
            None => return false,
        };
        let Some((namespace, child)) = self.split(&name) else {
            return false;
        };
        let object = match item.as_object_mut() {
            Some(object) => object,
            None => return false,
        };
        object.insert("namespace".into(), json!(namespace));
        object.insert("name".into(), json!(child));
        true
    }

    /// Restore every namespace function call in a response's `output` array.
    pub fn restore_response(&self, response: &mut Value) {
        let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) else {
            return;
        };
        for item in output {
            self.restore_item(item);
        }
    }
}

/// Official HTTP + WebSocket shared preparer: only exact upstream model mapping.
/// No local hydrate, materialize, sanitize, or ID rewrite (issue #4 comment).
pub fn prepare_openai_official_native(original: &Value, upstream_model: &str) -> Value {
    let mut body = original.clone();
    if let Some(object) = body.as_object_mut() {
        object.insert("model".into(), Value::String(upstream_model.to_string()));
    }
    body
}

pub fn strip_cross_realm_fields(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("reasoning_content");
            object.remove("encrypted_content");
            for child in object.values_mut() {
                strip_cross_realm_fields(child);
            }
        }
        Value::Array(values) => {
            values.retain(|child| child.get("type").and_then(Value::as_str) != Some("reasoning"));
            for child in values {
                strip_cross_realm_fields(child);
            }
        }
        _ => {}
    }
}

fn nonempty_json_string(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
}

fn reasoning_has_summary_text(summary: Option<&Value>) -> bool {
    summary.and_then(Value::as_array).is_some_and(|parts| {
        parts
            .iter()
            .any(|part| nonempty_json_string(part.get("text")))
    })
}

/// Remove provider-owned ciphertext while preserving a readable reasoning
/// summary. This is the portable baseline for unprobed transitions. Empty
/// reasoning items are removed rather than sending malformed placeholders.
pub fn strip_opaque_reasoning_keep_summary(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("reasoning")
                && !reasoning_has_summary_text(object.get("summary"))
            {
                let readable = object
                    .get("reasoning_content")
                    .or_else(|| object.get("content"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string);
                if let Some(readable) = readable {
                    object.insert(
                        "summary".into(),
                        json!([{"type": "summary_text", "text": readable}]),
                    );
                }
            }
            object.remove("encrypted_content");
            object.remove("reasoning_content");
            if object.get("type").and_then(Value::as_str) == Some("reasoning") {
                object.remove("content");
            }
            for child in object.values_mut() {
                strip_opaque_reasoning_keep_summary(child);
            }
        }
        Value::Array(values) => {
            for child in values.iter_mut() {
                strip_opaque_reasoning_keep_summary(child);
            }
            values.retain(|child| {
                if child.get("type").and_then(Value::as_str) != Some("reasoning") {
                    return true;
                }
                reasoning_has_summary_text(child.get("summary"))
                    || nonempty_json_string(child.get("content"))
            });
        }
        _ => {}
    }
}

/// Remove provider-owned reasoning while preserving opaque official
/// compaction state. A model switch can carry both in the same full Desktop
/// snapshot: reasoning belongs to the provider that produced the turn, while
/// an earlier official compaction remains valid when returning to OpenAI.
pub fn strip_cross_realm_fields_for_official(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("reasoning_content");
            for child in object.values_mut() {
                strip_cross_realm_fields_for_official(child);
            }
        }
        Value::Array(values) => {
            values.retain(|child| child.get("type").and_then(Value::as_str) != Some("reasoning"));
            for child in values {
                strip_cross_realm_fields_for_official(child);
            }
        }
        _ => {}
    }
}

/// Official encrypted reasoning belongs to the ChatGPT account/realm that
/// produced it. Preserve canonical encrypted items, but remove third-party
/// reasoning summaries and Chat-only `reasoning_content` fields before a
/// provider switch. This avoids both unknown-parameter and decrypt/verify
/// failures without discarding visible assistant/tool history.
pub fn sanitize_for_official(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("reasoning_content");
            normalize_official_item_id(object);
            for child in object.values_mut() {
                sanitize_for_official(child);
            }
        }
        Value::Array(values) => {
            values.retain(|child| {
                child.get("type").and_then(Value::as_str) != Some("reasoning")
                    || (child
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id.starts_with("rs_"))
                        && child
                            .get("encrypted_content")
                            .and_then(Value::as_str)
                            .is_some_and(|content| !content.trim().is_empty()))
            });
            for child in values {
                sanitize_for_official(child);
            }
        }
        _ => {}
    }
}

/// True when a Desktop snapshot contains an output item synthesized by one of
/// Vellum's third-party adapters. Codex can replay a complete input snapshot
/// without `previous_response_id`, so route-history alone cannot always prove
/// that an Official request is a provider handoff.
pub fn contains_vellum_synthetic_item(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.starts_with("resp_vellum_"))
                || object.values().any(contains_vellum_synthetic_item)
        }
        Value::Array(values) => values.iter().any(contains_vellum_synthetic_item),
        _ => false,
    }
}

fn normalize_official_item_id(object: &mut serde_json::Map<String, Value>) {
    let Some(item_type) = object.get("type").and_then(Value::as_str) else {
        return;
    };
    let expected_prefix = match item_type {
        "message" => Some("msg_"),
        "function_call" => Some("fc_"),
        "reasoning" => Some("rs_"),
        "custom_tool_call" => Some("ctc_"),
        "tool_search_call" => Some("tsc_"),
        _ => None,
    };
    let Some(id) = object.get("id").and_then(Value::as_str) else {
        return;
    };
    let Some(expected_prefix) = expected_prefix else {
        // Output items are linked to their calls through `call_id`. A
        // third-party item ID has no portable meaning in the official realm.
        if id.starts_with("resp_vellum_") || id.starts_with("call_") {
            object.remove("id");
        }
        return;
    };
    object.insert(
        "id".into(),
        Value::String(normalized_item_id(expected_prefix, id)),
    );
}

fn normalized_item_id(expected_prefix: &str, source: &str) -> String {
    if source.starts_with(expected_prefix) {
        return source.to_string();
    }
    let digest = Sha256::digest(source.as_bytes());
    format!(
        "{expected_prefix}vellum_{}",
        digest[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

/// Read the reasoning text off a Chat message or streaming delta.
///
/// OpenAI-compatible providers disagree on the field name: vLLM and DeepSeek
/// send `reasoning_content`, Ollama sends `reasoning`. Accepting only one of
/// them discarded every thinking token from the other in silence. A model that
/// reasons for a thousand tokens before its first word then looked like a dead
/// stream, and when its token budget ran out during that reasoning the turn
/// produced nothing at all. The capability probe already checks both spellings;
/// this keeps the request path honest about the same thing.
pub fn chat_reasoning_text(object: &Value) -> Option<&str> {
    ["reasoning_content", "reasoning"]
        .into_iter()
        .find_map(|key| object.get(key).and_then(Value::as_str))
        .filter(|text| !text.is_empty())
}

pub fn content_to_text(value: &Value) -> Value {
    match value {
        Value::String(_) => value.clone(),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| part.get("content").and_then(Value::as_str))
                })
                .collect::<Vec<_>>()
                .join("\n");
            Value::String(text)
        }
        Value::Object(object) => object
            .get("text")
            .cloned()
            .unwrap_or_else(|| Value::String(value.to_string())),
        _ => Value::String(value.to_string()),
    }
}

pub fn content_to_plain_string(value: &Value) -> String {
    match content_to_text(value) {
        Value::String(text) => text,
        other => other.to_string(),
    }
}

pub fn chat_response_to_responses(body: &Value, model: &str) -> Result<Value, String> {
    chat_response_to_responses_with_context(body, model, &CodexToolContext::default())
}

pub fn chat_response_to_responses_with_context(
    body: &Value,
    model: &str,
    tool_context: &CodexToolContext,
) -> Result<Value, String> {
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| "Chat 回應沒有 choices[0]".to_string())?;
    let message = choice
        .get("message")
        .ok_or_else(|| "Chat 回應沒有 message".to_string())?;
    let response_id = body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("resp_vellum");
    let mut output = Vec::new();
    let has_tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty());

    if let Some(reasoning) = chat_reasoning_text(message) {
        output.push(json!({
            "type": "reasoning",
            "id": format!("{response_id}_reasoning"),
            "summary": [{"type": "summary_text", "text": reasoning}]
        }));
    }
    if let Some(content) = message.get("content").filter(|value| !value.is_null()) {
        // OpenAI-compatible Chat providers are inconsistent here: some return
        // a string, while others return Responses-style content parts. Codex
        // Guardian still requires the exact JSON verdict after normalization,
        // so normalize the shape without weakening the verdict validator.
        let text = sanitize_model_content(&content_to_plain_string(content));
        if !text.is_empty() {
            output.push(json!({
                "type": "message",
                "id": format!("{response_id}_message"),
                "role": "assistant",
                "status": "completed",
                "phase": if has_tool_calls { "commentary" } else { "final_answer" },
                "content": [{"type": "output_text", "text": text, "annotations": []}]
            }));
        }
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .unwrap_or("call");
            let arguments = tool_arguments_text(call.pointer("/function/arguments"));
            let mut item = json!({
                "type": "function_call",
                "id": normalized_item_id("fc_", call_id),
                "call_id": call_id,
                "name": call.pointer("/function/name").cloned().unwrap_or_else(|| json!("unknown")),
                // llama.cpp can optionally return tool arguments as an object
                // (`--tool-args-object`) while OpenAI-compatible clients and
                // Codex require the field to be a JSON string.
                "arguments": arguments,
                "status": "completed"
            });
            tool_context.restore_output_item(&mut item);
            output.push(item);
        }
    }
    let has_actionable_output = output.iter().any(|item| {
        matches!(
            item.get("type").and_then(Value::as_str),
            Some("message" | "function_call" | "custom_tool_call" | "tool_search_call")
        )
    });
    if !has_actionable_output {
        let mut response = json!({
            "id": response_id,
            "object": "response",
            "created_at": body.get("created").cloned().unwrap_or_else(|| json!(0)),
            "status": "failed",
            "model": model,
            "output": output,
            "error": {
                "type": "empty_completion",
                "message": "Upstream model ended without a native tool call or user-visible answer"
            },
            "store": false
        });
        insert_normalized_usage(&mut response, body.get("usage"));
        return Ok(response);
    }
    let mut response = json!({
        "id": response_id,
        "object": "response",
        "created_at": body.get("created").cloned().unwrap_or_else(|| json!(0)),
        "status": "completed",
        "model": model,
        "output": output,
        "store": false
    });
    insert_normalized_usage(&mut response, body.get("usage"));
    Ok(response)
}

fn tool_arguments_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(arguments)) => arguments.clone(),
        Some(Value::Null) | None => "{}".to_string(),
        Some(arguments) => serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string()),
    }
}

fn normalized_usage(usage: Option<&Value>) -> Option<Value> {
    let usage = usage.filter(|usage| !usage.is_null())?;
    let input_tokens = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input_tokens.saturating_add(output_tokens));
    let cached_tokens = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .or_else(|| usage.pointer("/input_tokens_details/cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning_tokens = usage
        .pointer("/completion_tokens_details/reasoning_tokens")
        .or_else(|| usage.pointer("/output_tokens_details/reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(json!({
        "input_tokens": input_tokens,
        "input_tokens_details": {"cached_tokens": cached_tokens},
        "output_tokens": output_tokens,
        "output_tokens_details": {"reasoning_tokens": reasoning_tokens},
        "total_tokens": total_tokens
    }))
}

fn insert_normalized_usage(response: &mut Value, usage: Option<&Value>) {
    let Some(usage) = normalized_usage(usage) else {
        return;
    };
    if let Some(object) = response.as_object_mut() {
        object.insert("usage".into(), usage);
    }
}

pub struct ChatSseAdapter {
    response_id: String,
    model: String,
    sequence: u64,
    started: bool,
    finished: bool,
    terminal_seen: bool,
    message_added: bool,
    reasoning_added: bool,
    reasoning_index: Option<usize>,
    message_index: Option<usize>,
    next_output_index: usize,
    text: String,
    reasoning: String,
    usage: Option<Value>,
    content_filter: ModelContentFilter,
    tool_context: CodexToolContext,
    namespace_context: NamespaceToolContext,
    tool_calls: std::collections::BTreeMap<usize, StreamToolCall>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum HiddenContent {
    #[default]
    Visible,
    Think,
    ToolCall,
}

/// Some Chat-compatible reasoning models occasionally serialize their private
/// scratchpad or tool protocol into `delta.content` instead of the native
/// `reasoning_content` / `tool_calls` fields. Codex treats `delta.content` as
/// user-visible answer text, so letting those tags through exposes chain of
/// thought and raw tool JSON in the conversation.
///
/// The request-side protocol reminder prevents the common case. This stateful
/// filter is the fail-closed boundary for providers that ignore it. It is
/// deliberately streaming-aware so a tag split across SSE chunks cannot leak.
#[derive(Debug, Clone, Default)]
struct ModelContentFilter {
    pending: String,
    hidden: HiddenContent,
}

impl ModelContentFilter {
    const TAGS: [&'static str; 8] = [
        "<think>",
        "</think>",
        "<tool_call>",
        "</tool_call>",
        "<function_results>",
        "</function_results>",
        "<function_result>",
        "</function_result>",
    ];

    fn push(&mut self, chunk: &str) -> String {
        self.pending.push_str(chunk);
        self.drain(false)
    }

    fn finish(&mut self) -> String {
        self.drain(true)
    }

    fn drain(&mut self, finishing: bool) -> String {
        let mut visible = String::new();
        loop {
            match self.hidden {
                HiddenContent::Visible => {
                    let next = Self::next_tag(&self.pending);
                    if let Some((index, tag)) = next {
                        visible.push_str(&self.pending[..index]);
                        self.pending.drain(..index + tag.len());
                        self.hidden = match tag {
                            "<think>" => HiddenContent::Think,
                            "<tool_call>" | "<function_results>" | "<function_result>" => {
                                HiddenContent::ToolCall
                            }
                            _ => HiddenContent::Visible,
                        };
                        continue;
                    }
                    let keep = if finishing {
                        0
                    } else {
                        Self::partial_tag_suffix(&self.pending)
                    };
                    let emit = self.pending.len().saturating_sub(keep);
                    visible.push_str(&self.pending[..emit]);
                    self.pending.drain(..emit);
                    break;
                }
                HiddenContent::Think => {
                    if let Some((index, tag)) = Self::next_of(
                        &self.pending,
                        &[
                            "<tool_call>",
                            "<function_results>",
                            "<function_result>",
                            "</think>",
                        ],
                    ) {
                        self.pending.drain(..index + tag.len());
                        self.hidden = if matches!(
                            tag,
                            "<tool_call>" | "<function_results>" | "<function_result>"
                        ) {
                            HiddenContent::ToolCall
                        } else {
                            HiddenContent::Visible
                        };
                        continue;
                    }
                    if finishing {
                        self.pending.clear();
                    } else {
                        Self::discard_hidden_prefix(&mut self.pending);
                    }
                    break;
                }
                HiddenContent::ToolCall => {
                    if let Some((index, tag)) = Self::next_of(
                        &self.pending,
                        &[
                            "</tool_call>",
                            "</function_results>",
                            "</function_result>",
                            "</think>",
                            "<tool_call>",
                            "<function_results>",
                            "<function_result>",
                        ],
                    ) {
                        self.pending.drain(..index + tag.len());
                        self.hidden = if matches!(
                            tag,
                            "<tool_call>" | "<function_results>" | "<function_result>"
                        ) {
                            HiddenContent::ToolCall
                        } else {
                            HiddenContent::Visible
                        };
                        continue;
                    }
                    if finishing {
                        self.pending.clear();
                    } else {
                        Self::discard_hidden_prefix(&mut self.pending);
                    }
                    break;
                }
            }
        }
        visible
    }

    fn next_tag(text: &str) -> Option<(usize, &'static str)> {
        Self::next_of(text, &Self::TAGS)
    }

    fn next_of(text: &str, tags: &[&'static str]) -> Option<(usize, &'static str)> {
        tags.iter()
            .filter_map(|tag| text.find(tag).map(|index| (index, *tag)))
            .min_by_key(|(index, _)| *index)
    }

    fn partial_tag_suffix(text: &str) -> usize {
        let bytes = text.as_bytes();
        let max = Self::TAGS
            .iter()
            .map(|tag| tag.len().saturating_sub(1))
            .max()
            .unwrap_or(0)
            .min(bytes.len());
        (1..=max)
            .rev()
            .find(|length| {
                let suffix = &bytes[bytes.len() - length..];
                Self::TAGS
                    .iter()
                    .any(|tag| tag.as_bytes().starts_with(suffix))
            })
            .unwrap_or(0)
    }

    fn discard_hidden_prefix(text: &mut String) {
        let keep = Self::partial_tag_suffix(text);
        let discard = text.len().saturating_sub(keep);
        text.drain(..discard);
    }
}

fn sanitize_model_content(text: &str) -> String {
    let mut filter = ModelContentFilter::default();
    let mut visible = filter.push(text);
    visible.push_str(&filter.finish());
    visible
}

#[derive(Debug, Clone, Default)]
struct StreamToolCall {
    id: String,
    name: String,
    arguments: String,
    output_index: usize,
    added: bool,
}

impl ChatSseAdapter {
    pub fn new(model: impl Into<String>) -> Self {
        Self::new_with_context(
            model,
            CodexToolContext::default(),
            NamespaceToolContext::default(),
        )
    }

    pub fn new_with_request(model: impl Into<String>, request: &Value) -> Self {
        Self::new_with_context(
            model,
            CodexToolContext::from_request(request),
            NamespaceToolContext::from_request(request),
        )
    }

    fn new_with_context(
        model: impl Into<String>,
        tool_context: CodexToolContext,
        namespace_context: NamespaceToolContext,
    ) -> Self {
        Self {
            response_id: format!("resp_vellum_{}", now_millis()),
            model: model.into(),
            sequence: 0,
            started: false,
            finished: false,
            terminal_seen: false,
            message_added: false,
            reasoning_added: false,
            reasoning_index: None,
            message_index: None,
            next_output_index: 0,
            text: String::new(),
            reasoning: String::new(),
            usage: None,
            content_filter: ModelContentFilter::default(),
            tool_context,
            namespace_context,
            tool_calls: std::collections::BTreeMap::new(),
        }
    }

    pub fn push_data(&mut self, data: &str) -> Vec<String> {
        if self.finished {
            return Vec::new();
        }
        if data.trim() == "[DONE]" {
            self.finished = true;
            return self.finish();
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            self.usage = normalized_usage(Some(usage));
        }
        let delta = value
            .pointer("/choices/0/delta")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let mut events = self.ensure_started();
        if let Some(reasoning) = chat_reasoning_text(&delta) {
            self.reasoning.push_str(reasoning);
            if !self.reasoning_added {
                self.reasoning_added = true;
                let output_index = self.next_output_index;
                self.next_output_index += 1;
                self.reasoning_index = Some(output_index);
                events.push(self.event(json!({
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": {
                        "type": "reasoning",
                        "id": format!("{}_reasoning", self.response_id),
                        "summary": []
                    }
                })));
                events.push(self.event(json!({
                    "type": "response.reasoning_summary_part.added",
                    "output_index": output_index,
                    "summary_index": 0,
                    "part": {
                        "type": "summary_text",
                        "text": ""
                    }
                })));
            }
            // Forward the provider's own reasoning chunk verbatim, in the
            // order it arrived. OpenCode's own integration streams full
            // reasoning directly, and third-party Chat providers exposing
            // `reasoning_content`/`reasoning` deserve the same fidelity: no
            // re-chunking, deduping, or compressing into a preview.
            events.push(self.event(json!({
                "type": "response.reasoning_summary_text.delta",
                "output_index": self.reasoning_index.unwrap_or(0),
                "summary_index": 0,
                "delta": reasoning
            })));
        }
        if let Some(content) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            let content = self.content_filter.push(content);
            if !content.is_empty() {
                self.text.push_str(&content);
                if !self.message_added {
                    self.message_added = true;
                    let output_index = self.next_output_index;
                    self.next_output_index += 1;
                    self.message_index = Some(output_index);
                    events.push(self.event(json!({
                        "type": "response.output_item.added",
                        "output_index": output_index,
                        "item": {
                            "type": "message",
                            "id": format!("{}_message", self.response_id),
                            "role": "assistant",
                            "status": "in_progress",
                            "content": []
                        }
                    })));
                    events.push(self.event(json!({
                        "type": "response.content_part.added",
                        "output_index": output_index,
                        "content_index": 0,
                        "part": {
                            "type": "output_text",
                            "text": "",
                            "annotations": []
                        }
                    })));
                }
                events.push(self.event(json!({
                    "type": "response.output_text.delta",
                    "output_index": self.message_index.unwrap_or(0),
                    "content_index": 0,
                    "delta": content
                })));
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let id = call.get("id").and_then(Value::as_str).unwrap_or("");
                let name = call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let arguments = tool_arguments_text(call.pointer("/function/arguments"));
                let mut added_event = None;
                let mut delta_event = None;
                {
                    let entry = self.tool_calls.entry(index).or_insert_with(|| {
                        let output_index = self.next_output_index;
                        self.next_output_index += 1;
                        StreamToolCall {
                            output_index,
                            ..StreamToolCall::default()
                        }
                    });
                    if !id.is_empty() {
                        entry.id = id.to_string();
                    }
                    if !name.is_empty() {
                        entry.name.push_str(name);
                    }
                    if !entry.added {
                        entry.added = true;
                        added_event = Some((
                            entry.output_index,
                            if entry.id.is_empty() {
                                format!("call_{}", index)
                            } else {
                                entry.id.clone()
                            },
                            entry.name.clone(),
                        ));
                    }
                    if !arguments.is_empty() {
                        entry.arguments.push_str(&arguments);
                        delta_event = Some((
                            entry.output_index,
                            if entry.id.is_empty() {
                                format!("call_{index}")
                            } else {
                                entry.id.clone()
                            },
                            arguments,
                        ));
                    }
                }
                if let Some((output_index, call_id, call_name)) = added_event {
                    let item_prefix = if self.tool_context.is_custom(&call_name) {
                        "ctc_"
                    } else if self.tool_context.is_tool_search(&call_name) {
                        "tsc_"
                    } else {
                        "fc_"
                    };
                    let item_id = normalized_item_id(item_prefix, &call_id);
                    let mut item = json!({
                        "type": "function_call",
                        "id": item_id,
                        "call_id": call_id,
                        "name": if call_name.is_empty() { "unknown" } else { &call_name },
                        "arguments": "",
                        "status": "in_progress"
                    });
                    self.tool_context.restore_output_item(&mut item);
                    events.push(self.event(json!({
                        "type": "response.output_item.added",
                        "output_index": output_index,
                        "item": item
                    })));
                }
                if let Some((output_index, item_id, delta)) = delta_event {
                    let translated_native = self.tool_calls.get(&index).is_some_and(|call| {
                        self.tool_context.is_custom(&call.name)
                            || self.tool_context.is_tool_search(&call.name)
                    });
                    if !translated_native {
                        events.push(self.event(json!({
                            "type": "response.function_call_arguments.delta",
                            "item_id": item_id,
                            "output_index": output_index,
                            "delta": delta
                        })));
                    }
                }
            }
        }
        // OpenAI's Chat SSE convention ends with `data: [DONE]`, but several
        // compatible servers close immediately after the final choice chunk.
        // A non-null finish_reason is itself an explicit terminal frame, so
        // remember it and let `finish_if_terminal` complete the projection when
        // the body ends, instead of reporting a false disconnect. A genuinely
        // truncated stream still has no finish_reason and continues to fail
        // closed in `chat_stream`.
        //
        // Remember, do not finish here. `include_usage` puts the token counts
        // in their own trailing chunk with an empty `choices` array, which by
        // the spec arrives *after* the one carrying finish_reason. Completing
        // on sight of the reason latched the adapter shut one frame too early
        // and threw that chunk away, so every provider that follows the spec
        // was reported to Codex as a turn that cost zero tokens.
        if value
            .pointer("/choices/0/finish_reason")
            .is_some_and(|reason| !reason.is_null())
        {
            self.terminal_seen = true;
        }
        events
    }

    /// Complete a stream whose body ended without `[DONE]`, but only if the
    /// model actually said it was done. Returns nothing for a truncated stream,
    /// which `chat_stream` still reports as a disconnect.
    pub fn finish_if_terminal(&mut self) -> Vec<String> {
        if self.finished || !self.terminal_seen {
            return Vec::new();
        }
        self.finished = true;
        self.finish()
    }

    fn ensure_started(&mut self) -> Vec<String> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        let created = self.event(json!({
            "type": "response.created",
            "response": {
                "id": self.response_id,
                "object": "response",
                "status": "in_progress",
                "model": self.model,
                "created_at": now_unix_seconds(),
                "output": []
            }
        }));
        let in_progress = self.event(json!({
            "type": "response.in_progress",
            "response": {
                "id": self.response_id,
                "object": "response",
                "status": "in_progress",
                "model": self.model,
                "created_at": now_unix_seconds(),
                "output": []
            }
        }));
        vec![created, in_progress]
    }

    fn finish(&mut self) -> Vec<String> {
        let mut events = self.ensure_started();
        let trailing = self.content_filter.finish();
        if !trailing.is_empty() {
            self.text.push_str(&trailing);
            if !self.message_added {
                self.message_added = true;
                let output_index = self.next_output_index;
                self.next_output_index += 1;
                self.message_index = Some(output_index);
                events.push(self.event(json!({
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": {
                        "type": "message",
                        "id": format!("{}_message", self.response_id),
                        "role": "assistant",
                        "status": "in_progress",
                        "content": []
                    }
                })));
                events.push(self.event(json!({
                    "type": "response.content_part.added",
                    "output_index": output_index,
                    "content_index": 0,
                    "part": {
                        "type": "output_text",
                        "text": "",
                        "annotations": []
                    }
                })));
            }
            events.push(self.event(json!({
                "type": "response.output_text.delta",
                "output_index": self.message_index.unwrap_or(0),
                "content_index": 0,
                "delta": trailing
            })));
        }
        if let Some(output_index) = self.reasoning_index {
            // The terminal reasoning item is exactly the concatenation of
            // every delta already streamed above — never a re-derived or
            // re-truncated summary, even past historical preview lengths.
            events.push(self.event(json!({
                "type": "response.reasoning_summary_text.done",
                "output_index": output_index,
                "summary_index": 0,
                "text": self.reasoning
            })));
            events.push(self.event(json!({
                "type": "response.reasoning_summary_part.done",
                "output_index": output_index,
                "summary_index": 0,
                "part": {
                    "type": "summary_text",
                    "text": self.reasoning
                }
            })));
            events.push(self.event(json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": {
                    "type": "reasoning",
                    "id": format!("{}_reasoning", self.response_id),
                    "summary": [{"type": "summary_text", "text": self.reasoning}]
                }
            })));
        }
        if let Some(output_index) = self.message_index {
            let phase = if self.tool_calls.is_empty() {
                "final_answer"
            } else {
                "commentary"
            };
            events.push(self.event(json!({
                "type": "response.output_text.done",
                "output_index": output_index,
                "content_index": 0,
                "text": self.text
            })));
            events.push(self.event(json!({
                "type": "response.content_part.done",
                "output_index": output_index,
                "content_index": 0,
                "part": {
                    "type": "output_text",
                    "text": self.text,
                    "annotations": []
                }
            })));
            events.push(self.event(json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": {
                    "type": "message",
                    "id": format!("{}_message", self.response_id),
                    "role": "assistant",
                    "status": "completed",
                    "phase": phase,
                    "content": [{
                        "type": "output_text",
                        "text": self.text,
                        "annotations": []
                    }]
                }
            })));
        }
        let calls = self.tool_calls.values().cloned().collect::<Vec<_>>();
        for call in calls {
            let call_id = if call.id.is_empty() {
                format!("call_{}", call.output_index)
            } else {
                call.id
            };
            let custom = self.tool_context.is_custom(&call.name);
            let tool_search = self.tool_context.is_tool_search(&call.name);
            let item_id = normalized_item_id(
                if custom {
                    "ctc_"
                } else if tool_search {
                    "tsc_"
                } else {
                    "fc_"
                },
                &call_id,
            );
            if custom {
                let input = custom_tool_input(&call.arguments);
                if !input.is_empty() {
                    events.push(self.event(json!({
                        "type": "response.custom_tool_call_input.delta",
                        "item_id": item_id,
                        "output_index": call.output_index,
                        "delta": input
                    })));
                }
                events.push(self.event(json!({
                    "type": "response.custom_tool_call_input.done",
                    "item_id": item_id,
                    "output_index": call.output_index,
                    "input": input
                })));
                events.push(self.event(json!({
                    "type": "response.output_item.done",
                    "output_index": call.output_index,
                    "item": {
                        "type": "custom_tool_call",
                        "id": item_id,
                        "call_id": call_id,
                        "name": if call.name.is_empty() { "unknown" } else { &call.name },
                        "input": input,
                        "status": "completed"
                    }
                })));
            } else if tool_search {
                let mut item = json!({
                    "type": "function_call",
                    "id": item_id,
                    "call_id": call_id,
                    "name": "tool_search",
                    "arguments": call.arguments,
                    "status": "completed"
                });
                self.tool_context.restore_output_item(&mut item);
                events.push(self.event(json!({
                    "type": "response.output_item.done",
                    "output_index": call.output_index,
                    "item": item
                })));
            } else {
                events.push(self.event(json!({
                    "type": "response.function_call_arguments.done",
                    "item_id": item_id,
                    "output_index": call.output_index,
                    "arguments": call.arguments
                })));
                // Build the function_call item mutably so a flattened
                // namespace name (`web__run`) can be restored to
                // `{namespace: "web", name: "run"}` for Codex.
                let mut item = json!({
                    "type": "function_call",
                    "id": item_id,
                    "call_id": call_id,
                    "name": if call.name.is_empty() { "unknown" } else { &call.name },
                    "arguments": call.arguments,
                    "status": "completed"
                });
                self.namespace_context.restore_item(&mut item);
                events.push(self.event(json!({
                    "type": "response.output_item.done",
                    "output_index": call.output_index,
                    "item": item
                })));
            }
        }
        if self.text.trim().is_empty() && self.tool_calls.is_empty() {
            let mut response = json!({
                "id": self.response_id,
                "object": "response",
                "status": "failed",
                "model": self.model,
                "output": [],
                "error": {
                    "type": "empty_completion",
                    "message": "Upstream model ended without a native tool call or user-visible answer"
                }
            });
            if let (Some(object), Some(usage)) = (response.as_object_mut(), self.usage.as_ref()) {
                object.insert("usage".into(), usage.clone());
            }
            events.push(self.event(json!({
                "type": "response.failed",
                "response": response
            })));
            return events;
        }
        let response = self.completed_response();
        events.push(self.event(json!({
            "type": "response.completed",
            "response": response
        })));
        events
    }

    /// Full readable reasoning is kept for replay/history, but the client
    /// should only receive the same concise progress summary as the stream.
    pub fn history_response(&self) -> Value {
        let mut response = self.completed_response();
        if let Some(reasoning) = response
            .get_mut("output")
            .and_then(Value::as_array_mut)
            .and_then(|output| {
                output
                    .iter_mut()
                    .find(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
            })
            .and_then(Value::as_object_mut)
        {
            reasoning.insert(
                "summary".into(),
                json!([{"type": "summary_text", "text": self.reasoning}]),
            );
        }
        response
    }

    pub fn completed_response(&self) -> Value {
        let mut output = Vec::new();
        if !self.reasoning.is_empty() {
            output.push(json!({
                "type": "reasoning",
                "id": format!("{}_reasoning", self.response_id),
                "summary": [{"type": "summary_text", "text": self.reasoning}]
            }));
        }
        if !self.text.is_empty() {
            let phase = if self.tool_calls.is_empty() {
                "final_answer"
            } else {
                "commentary"
            };
            output.push(json!({
                "type": "message",
                "id": format!("{}_message", self.response_id),
                "role": "assistant",
                "status": "completed",
                "phase": phase,
                "content": [{"type": "output_text", "text": self.text, "annotations": []}]
            }));
        }
        for (index, call) in &self.tool_calls {
            let call_id = if call.id.is_empty() {
                format!("call_{index}")
            } else {
                call.id.clone()
            };
            let custom = self.tool_context.is_custom(&call.name);
            let tool_search = self.tool_context.is_tool_search(&call.name);
            let item_id = normalized_item_id(
                if custom {
                    "ctc_"
                } else if tool_search {
                    "tsc_"
                } else {
                    "fc_"
                },
                &call_id,
            );
            if custom {
                output.push(json!({
                    "type": "custom_tool_call",
                    "id": item_id,
                    "call_id": call_id,
                    "name": if call.name.is_empty() { "unknown" } else { &call.name },
                    "input": custom_tool_input(&call.arguments),
                    "status": "completed"
                }));
            } else {
                let mut item = json!({
                    "type": "function_call",
                    "id": item_id,
                    "call_id": call_id,
                    "name": if call.name.is_empty() { "unknown" } else { &call.name },
                    "arguments": call.arguments,
                    "status": "completed"
                });
                self.tool_context.restore_output_item(&mut item);
                self.namespace_context.restore_item(&mut item);
                output.push(item);
            }
        }
        let mut response = json!({
            "id": self.response_id,
            "object": "response",
            "status": "completed",
            "model": self.model,
            "created_at": now_unix_seconds(),
            "output": output,
            "store": false
        });
        if let (Some(object), Some(usage)) = (response.as_object_mut(), self.usage.as_ref()) {
            object.insert("usage".into(), usage.clone());
        }
        response
    }

    fn event(&mut self, mut value: Value) -> String {
        if let Value::Object(ref mut object) = value {
            object.insert("sequence_number".into(), json!(self.sequence));
        }
        self.sequence += 1;
        format!(
            "event: {}\ndata: {}\n\n",
            value["type"].as_str().unwrap_or("message"),
            value
        )
    }
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn now_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_response_without_provider_usage_does_not_invent_zero_usage() {
        let response = chat_response_to_responses(
            &json!({
                "id": "chat_1",
                "choices": [{"message": {"content": "done"}}]
            }),
            "third-party",
        )
        .unwrap();

        assert!(response.get("usage").is_none());
    }

    /// The exact frame order an OpenAI-compatible server sends when
    /// `stream_options.include_usage` was requested: the token counts arrive in
    /// their own chunk, after the one carrying `finish_reason`, with `choices`
    /// empty. Completing the projection on sight of the reason discarded that
    /// chunk and billed the whole turn as zero -- which is what Codex Desktop
    /// was showing as `0 token / 0 token`.
    #[test]
    fn a_trailing_usage_chunk_is_counted_even_though_it_follows_finish_reason() {
        let mut adapter = ChatSseAdapter::new("third-party");
        adapter.push_data(&json!({"choices": [{"delta": {"content": "4"}}]}).to_string());
        adapter
            .push_data(&json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}).to_string());
        adapter.push_data(
            &json!({
                "choices": [],
                "usage": {"prompt_tokens": 1339, "completion_tokens": 59, "total_tokens": 1398}
            })
            .to_string(),
        );
        adapter.push_data("[DONE]");

        let usage = adapter.completed_response();
        let usage = usage.get("usage").expect("provider usage kept");
        assert_eq!(usage["input_tokens"], 1339);
        assert_eq!(usage["output_tokens"], 59);
        assert_eq!(usage["total_tokens"], 1398);
    }

    /// A server that closes straight after the final choice chunk still
    /// completes -- that is what the finish_reason latch was added for, and
    /// deferring the completion must not give the false disconnect back.
    #[test]
    fn a_body_that_ends_after_finish_reason_completes_without_done() {
        let mut adapter = ChatSseAdapter::new("third-party");
        adapter.push_data(
            &json!({"choices": [{"delta": {"content": "4"}, "finish_reason": "stop"}]}).to_string(),
        );

        let events = adapter.finish_if_terminal();
        assert!(events
            .iter()
            .any(|event| event.contains("response.completed")));
        // Once emitted, it is not emitted twice.
        assert!(adapter.finish_if_terminal().is_empty());
    }

    /// A stream cut off mid-answer has no finish_reason, so nothing is
    /// synthesized and `chat_stream` still reports the disconnect.
    #[test]
    fn a_truncated_body_is_not_completed_for_the_model() {
        let mut adapter = ChatSseAdapter::new("third-party");
        adapter.push_data(&json!({"choices": [{"delta": {"content": "4"}}]}).to_string());

        assert!(adapter.finish_if_terminal().is_empty());
    }

    #[test]
    fn chat_stream_without_provider_usage_does_not_invent_zero_usage() {
        let mut adapter = ChatSseAdapter::new("third-party");
        adapter.push_data(
            &json!({
                "choices": [{
                    "delta": {"content": "done"},
                    "finish_reason": "stop"
                }]
            })
            .to_string(),
        );

        assert!(adapter.completed_response().get("usage").is_none());
    }

    #[test]
    fn chat_provider_usage_is_still_normalized_when_present() {
        let response = chat_response_to_responses(
            &json!({
                "id": "chat_1",
                "choices": [{"message": {"content": "done"}}],
                "usage": {
                    "prompt_tokens": 120,
                    "completion_tokens": 8,
                    "total_tokens": 128
                }
            }),
            "third-party",
        )
        .unwrap();

        assert_eq!(response["usage"]["input_tokens"], 120);
        assert_eq!(response["usage"]["output_tokens"], 8);
        assert_eq!(response["usage"]["total_tokens"], 128);
    }

    #[test]
    fn chat_finish_reason_is_terminal_when_compatible_server_omits_done() {
        let mut adapter = ChatSseAdapter::new("third-party");
        let events = adapter.push_data(
            &json!({
                "choices": [{
                    "delta": {"content": "The screenshot shows a file-in-use error."},
                    "finish_reason": "stop"
                }]
            })
            .to_string(),
        );
        // The reason alone no longer completes the turn: a trailing
        // `include_usage` chunk is still allowed to arrive. The completion is
        // emitted when the body actually ends.
        assert!(!events
            .iter()
            .any(|event| event.contains("event: response.completed")));
        let events = adapter.finish_if_terminal();
        assert!(events
            .iter()
            .any(|event| event.contains("event: response.completed")));
        assert_eq!(
            adapter.completed_response()["output"][0]["content"][0]["text"],
            "The screenshot shows a file-in-use error."
        );
        assert!(adapter.push_data("[DONE]").is_empty());
    }

    /// Reasoning chunks A -> B -> C from a Chat-wire provider must forward as
    /// three ordered, verbatim deltas (no re-chunking, no dedup, no preview
    /// compression), and the terminal summary must equal their exact
    /// concatenation even though it is well past the old 120-char preview
    /// bound.
    #[test]
    fn chat_reasoning_stream_forwards_every_chunk_verbatim_and_in_order() {
        let a = "Investigating the adapter before editing. ".repeat(4);
        let b = "Reading another file with more private scratchpad. ".repeat(4);
        let c = "Concluding the change is safe.".to_string();
        let mut adapter = ChatSseAdapter::new("third-party");
        let mut events = adapter
            .push_data(&json!({"choices": [{"delta": {"reasoning_content": a}}]}).to_string());
        events.extend(
            adapter
                .push_data(&json!({"choices": [{"delta": {"reasoning_content": b}}]}).to_string()),
        );
        events.extend(
            adapter
                .push_data(&json!({"choices": [{"delta": {"reasoning_content": c}}]}).to_string()),
        );
        events.extend(
            adapter.push_data(&json!({"choices": [{"delta": {"content": "Done."}}]}).to_string()),
        );
        events.extend(adapter.push_data("[DONE]"));

        // Every reasoning chunk arrives as its own ordered delta carrying the
        // exact provider text, not a merged/compressed rewrite.
        let deltas = events
            .iter()
            .filter(|event| event.contains("event: response.reasoning_summary_text.delta"))
            .map(|event| {
                let data = event
                    .lines()
                    .find(|line| line.starts_with("data: "))
                    .unwrap();
                let value: Value = serde_json::from_str(data.trim_start_matches("data: ")).unwrap();
                value["delta"].as_str().unwrap().to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(deltas, vec![a.clone(), b.clone(), c.clone()]);

        let full = format!("{a}{b}{c}");
        assert!(
            full.chars().count() > 120,
            "fixture must exceed the old preview bound"
        );

        let client = adapter.completed_response();
        assert_eq!(client["output"][0]["summary"][0]["text"], full);
        let history = adapter.history_response();
        assert_eq!(history["output"][0]["summary"][0]["text"], full);

        // Reasoning never leaks into the final-answer text stream.
        assert_eq!(client["output"][1]["content"][0]["text"], "Done.");
        let output_text_deltas = events
            .iter()
            .filter(|event| event.contains("event: response.output_text.delta"))
            .count();
        assert_eq!(output_text_deltas, 1);
        for event in &events {
            if event.contains("event: response.output_text.delta") {
                assert!(!event.contains(&a) && !event.contains(&b) && !event.contains(&c));
            }
        }
    }

    /// Round-trip a namespaced tool (`web.run`) through the flattened-name
    /// contract: `split()` must recover the original `(namespace, child)`
    /// pair, and `restore_item`/`restore_response` must put `namespace`/
    /// `name` back onto a `function_call` output item that only carries the
    /// flattened name.
    #[test]
    fn namespace_tool_context_round_trips_flattened_calls() {
        let request = json!({
            "tools": [
                {
                    "type": "namespace",
                    "name": "web",
                    "tools": [{"type": "function", "name": "run"}]
                }
            ]
        });
        let context = NamespaceToolContext::from_request(&request);
        assert!(!context.is_empty());

        let flattened = flatten_namespace_name("web", "run");
        assert_eq!(flattened, "web__run");
        assert_eq!(context.split(&flattened), Some(("web", "run")));
        assert_eq!(context.split("web__other"), None);

        let mut item = json!({
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": flattened,
            "arguments": "{}",
            "status": "completed"
        });
        assert!(context.restore_item(&mut item));
        assert_eq!(item["namespace"], "web");
        assert_eq!(item["name"], "run");

        let mut response = json!({
            "output": [
                {
                    "type": "function_call",
                    "id": "fc_2",
                    "call_id": "call_2",
                    "name": "web__run",
                    "arguments": "{}",
                    "status": "completed"
                }
            ]
        });
        context.restore_response(&mut response);
        assert_eq!(response["output"][0]["namespace"], "web");
        assert_eq!(response["output"][0]["name"], "run");
    }

    #[test]
    fn chat_stream_restores_declared_tool_search_as_a_native_call() {
        let request = json!({
            "tools": [{"type": "tool_search", "execution": "client"}]
        });
        let mut adapter = ChatSseAdapter::new_with_request("third-party", &request);
        let mut events = adapter.push_data(
            &json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_search",
                            "function": {
                                "name": "tool_search",
                                "arguments": "{\"query\":\"spawn subagent\"}"
                            }
                        }]
                    }
                }]
            })
            .to_string(),
        );
        events.extend(adapter.push_data("[DONE]"));
        let transcript = events.join("");

        assert!(transcript.contains("\"type\":\"tool_search_call\""));
        assert!(transcript.contains("\"execution\":\"client\""));
        assert!(transcript.contains("\"query\":\"spawn subagent\""));
        assert!(!transcript.contains("response.function_call_arguments.delta"));
        assert!(!transcript.contains("response.function_call_arguments.done"));
        assert!(!transcript.contains("\"name\":\"tool_search\""));

        let completed = adapter.completed_response();
        assert_eq!(completed["output"][0]["type"], "tool_search_call");
        assert_eq!(completed["output"][0]["call_id"], "call_search");
    }
}
