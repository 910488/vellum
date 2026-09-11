//! Vellum Canonical compaction primitives (plan M6).
//!
//! Ported from Desktop's `src-tauri/src/compaction.rs` (issue #4 Canonical /
//! Grok Native helpers) so the shared runtime can materialize local canonical
//! checkpoints with the same decisions as Desktop. Everything here is a pure
//! function over JSON values; the summary upstream call and the journal live
//! in the execution layer.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Default number of recent user turns retained verbatim in a canonical tail.
pub const DEFAULT_KEEP_RECENT_TURNS: u32 = 8;

/// Max user-message tokens collected for the remote compact output.
pub const COMPACT_USER_MESSAGE_MAX_TOKENS: usize = 20_000;

/// Hard cap for retained recent tail tokens on Vellum Canonical (issue #4 B-1).
pub const CANONICAL_MAX_TAIL_TOKENS: u64 = 12_000;

/// Prefix marking a Vellum-injected summary message. Codex re-injects its own
/// canonical initial context; this marker lets history identify the summary.
pub const OFFICIAL_SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

/// How a compacted checkpoint preserves continuity (issue #4): always
/// `Semantic` for Canonical checkpoints, never provider-private reasoning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContinuityKind {
    #[default]
    Native,
    Semantic,
}

impl ContinuityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ContinuityKind::Native => "native",
            ContinuityKind::Semantic => "semantic",
        }
    }
}

/// Canonical Context Engine version (spec 65.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CanonicalEngineVersion {
    V1,
    #[default]
    V2,
}

/// Trigger reason for compaction (spec 64).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTriggerKind {
    SoftThreshold,
    ExplicitClientTrigger,
    ContextExceededRecovery,
}

/// Keys that must never leave Vellum toward a third-party Canonical compactor.
const OPAQUE_FIELD_KEYS: &[&str] = &[
    "encrypted_content",
    "reasoning_content",
    "authorization",
    "api_key",
    "access_token",
    "refresh_token",
    "session_token",
    "xai_token",
    "cookie",
    "set-cookie",
    "comp_hash",
];

/// Check if a JSON item contains any third-party opaque or credential fields.
pub fn contains_opaque_field(item: &Value) -> bool {
    match item {
        Value::Object(map) => {
            for (key, val) in map {
                if OPAQUE_FIELD_KEYS.contains(&key.as_str()) {
                    return true;
                }
                if contains_opaque_field(val) {
                    return true;
                }
            }
            false
        }
        Value::Array(arr) => arr.iter().any(contains_opaque_field),
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemRole {
    System,
    User,
    Assistant,
    Tool,
    Other,
}

/// Classify a Responses/Chat item into a role for compaction splitting.
pub fn classify_item(item: &Value) -> ItemRole {
    let item_type = item.get("type").and_then(Value::as_str);
    let role = item.get("role").and_then(Value::as_str);
    match (item_type, role) {
        (Some("system"), _) | (_, Some("system")) => ItemRole::System,
        (Some("developer"), _) | (_, Some("developer")) => ItemRole::System,
        (_, Some("user")) => ItemRole::User,
        (_, Some("assistant")) => ItemRole::Assistant,
        (Some("function_call"), _) | (Some("local_shell_call"), _) => ItemRole::Assistant,
        (Some("function_call_output"), _) | (Some("local_shell_call_output"), _) => ItemRole::Tool,
        _ => ItemRole::Other,
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StructuredSummary {
    #[serde(default)]
    pub goal: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub user_preferences: Vec<String>,
    #[serde(default)]
    pub done: Vec<String>,
    #[serde(default)]
    pub in_progress: Vec<String>,
    #[serde(default)]
    pub blocked: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub relevant_files: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub tests: Vec<String>,
    #[serde(default)]
    pub unresolved: Vec<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub critical_context: Vec<String>,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub next_steps: Vec<String>,
}

impl StructuredSummary {
    pub fn is_usable(&self) -> bool {
        if self.goal.trim().is_empty() {
            return false;
        }
        [
            &self.acceptance_criteria,
            &self.constraints,
            &self.user_preferences,
            &self.done,
            &self.in_progress,
            &self.blocked,
            &self.decisions,
            &self.changed_files,
            &self.relevant_files,
            &self.commands,
            &self.tests,
            &self.unresolved,
            &self.errors,
            &self.critical_context,
            &self.references,
            &self.next_steps,
        ]
        .into_iter()
        .any(|values| values.iter().any(|value| !value.trim().is_empty()))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeState {
    pub path: String,
    #[serde(default)]
    pub summary: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolState {
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub detail: String,
}

/// Issue #4 portable semantic checkpoint produced by Vellum Canonical.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalCheckpoint {
    pub goal: String,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub discovered_facts: Vec<String>,
    #[serde(default)]
    pub failed_attempts: Vec<String>,
    #[serde(default)]
    pub files_changed: Vec<FileChangeState>,
    #[serde(default)]
    pub tool_state: Vec<ToolState>,
    #[serde(default)]
    pub pending_tasks: Vec<String>,
    #[serde(default)]
    pub unresolved_blockers: Vec<String>,
    pub exact_next_step: String,
    #[serde(default)]
    pub retained_tail_items: Vec<Value>,
    /// Always `semantic` for Canonical checkpoints.
    #[serde(default = "canonical_continuity_kind")]
    pub continuity_kind: ContinuityKind,
}

fn canonical_continuity_kind() -> ContinuityKind {
    ContinuityKind::Semantic
}

fn is_opaque_field_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    OPAQUE_FIELD_KEYS
        .iter()
        .any(|blocked| lower == *blocked || lower.contains("encrypted"))
}

fn find_opaque_field_path(value: &Value, path: &str) -> Option<(String, String)> {
    match value {
        Value::Array(values) => values
            .iter()
            .enumerate()
            .find_map(|(index, child)| find_opaque_field_path(child, &format!("{path}[{index}]"))),
        Value::Object(object) => {
            for (key, child) in object {
                let child_path = format!("{path}.{key}");
                if is_opaque_field_key(key) {
                    return Some((key.clone(), child_path));
                }
                if let Some(found) = find_opaque_field_path(child, &child_path) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

/// Strip provider-private / auth fields from a value tree.
pub fn strip_opaque_fields(mut value: Value) -> Value {
    match &mut value {
        Value::Array(values) => {
            for child in values.iter_mut() {
                *child = strip_opaque_fields(std::mem::take(child));
            }
        }
        Value::Object(object) => {
            object.retain(|key, _| !is_opaque_field_key(key));
            // Drop entire reasoning/compaction opaque items that only carried
            // ciphertext.
            let item_type = object.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(item_type, "reasoning" | "compaction") {
                // Responses producers may persist an optional `content: null`
                // beside a portable reasoning summary.  The field is valid in
                // the local rollout envelope but is rejected by stricter
                // third-party Responses endpoints, which require `content` to
                // be an array when present.  Preserve the summary and omit the
                // absent optional value on the portable wire.
                for key in ["summary", "content", "text"] {
                    if object.get(key).is_some_and(Value::is_null) {
                        object.remove(key);
                    }
                }
                let has_visible = ["summary", "content", "text"].iter().any(|key| {
                    object.get(*key).is_some_and(|value| match value {
                        Value::Null => false,
                        Value::String(text) => !text.trim().is_empty(),
                        Value::Array(values) => !values.is_empty(),
                        Value::Object(values) => !values.is_empty(),
                        _ => true,
                    })
                });
                if !has_visible {
                    return Value::Null;
                }
            }
            for child in object.values_mut() {
                *child = strip_opaque_fields(std::mem::take(child));
            }
        }
        _ => {}
    }
    value
}

pub fn strip_images(mut value: Value) -> Value {
    match &mut value {
        Value::Array(values) => {
            for child in values.iter_mut() {
                *child = strip_images(std::mem::take(child));
            }
        }
        Value::Object(object) => {
            if matches!(
                object.get("type").and_then(Value::as_str),
                Some("input_image" | "image")
            ) {
                return serde_json::json!({
                    "type": "input_text",
                    "text": "[Image omitted during compaction]"
                });
            }
            for child in object.values_mut() {
                *child = strip_images(std::mem::take(child));
            }
        }
        _ => {}
    }
    value
}

pub fn item_text(item: &Value) -> Option<String> {
    match item.get("content") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(parts)) => Some(
            parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| part.get("content").and_then(Value::as_str))
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

/// Portable history item carrying its original occurrence identity and
/// source position. Engine-neutral: used by trajectory and recovery
/// analysis, not by any one compaction engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableItem {
    pub value: Value,
    pub occurrence_id: String,
    pub original_index: usize,
}

/// Build portable visible history with exact occurrence provenance
/// alignment, so an analysis can point back at the item it came from.
pub fn sanitize_portable_history_with_provenance(
    items: &[Value],
    identities: &[String],
) -> Result<Vec<PortableItem>, String> {
    if items.len() != identities.len() {
        return Err(format!(
            "portable history items length ({}) does not match identities length ({})",
            items.len(),
            identities.len()
        ));
    }
    let mut portable = Vec::new();
    for (i, (item, identity)) in items.iter().zip(identities.iter()).enumerate() {
        if matches!(classify_item(item), ItemRole::System)
            || is_summary_item(item)
            || matches!(
                item.get("type").and_then(Value::as_str),
                Some("compaction" | "compaction_trigger")
            )
        {
            continue;
        }
        let stripped = strip_opaque_fields(item.clone());
        if stripped.is_null() {
            continue;
        }
        let cleaned = strip_images(stripped);
        portable.push(PortableItem {
            value: cleaned,
            occurrence_id: identity.clone(),
            original_index: i,
        });
    }
    Ok(portable)
}

/// Build portable visible history for Vellum Canonical / cross-provider fork.
pub fn sanitize_portable_history(items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .filter(|item| {
            !matches!(classify_item(item), ItemRole::System)
                && !is_summary_item(item)
                && !matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("compaction" | "compaction_trigger")
                )
        })
        .cloned()
        .map(strip_opaque_fields)
        .filter(|value| !value.is_null())
        .map(strip_images)
        .collect()
}

/// Build a small, provider-independent semantic checkpoint when the selected
/// model cannot produce the strict JSON summary contract. Remote compaction is
/// an optimization, so a formatting drift or transient summary failure must
/// not turn an otherwise resumable Codex session into HTTP 500.
pub fn fallback_structured_summary(items: &[Value]) -> StructuredSummary {
    let portable = sanitize_portable_history(items);
    let visible = portable
        .iter()
        .filter_map(|item| {
            let text = item_text(item)?;
            let text = bounded_text(&text, 1_024);
            (!text.is_empty()).then(|| (classify_item(item), text))
        })
        .collect::<Vec<_>>();

    let goal = visible
        .iter()
        .find(|(role, _)| *role == ItemRole::User)
        .map(|(_, text)| bounded_text(text, 2_048))
        .or_else(|| visible.first().map(|(_, text)| bounded_text(text, 2_048)))
        .unwrap_or_else(|| "Continue the active task from the retained conversation state.".into());

    let critical_context = visible
        .iter()
        .rev()
        .map(|(role, text)| {
            let label = match role {
                ItemRole::User => "User",
                ItemRole::Assistant => "Assistant",
                ItemRole::Tool => "Tool",
                ItemRole::System => "System",
                ItemRole::Other => "Context",
            };
            format!("{label}: {}", bounded_text(text, 896))
        })
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    StructuredSummary {
        goal,
        critical_context,
        next_steps: vec![
            "Continue from the latest retained user intent and workspace state.".into(),
        ],
        ..StructuredSummary::default()
    }
}

/// Validate a Canonical checkpoint: required fields + continuity=semantic.
pub fn validate_canonical_checkpoint(checkpoint: &CanonicalCheckpoint) -> Result<(), String> {
    if checkpoint.goal.trim().is_empty() {
        return Err("canonical checkpoint missing goal".into());
    }
    if checkpoint.exact_next_step.trim().is_empty() {
        return Err("canonical checkpoint missing exact_next_step".into());
    }
    if checkpoint.continuity_kind != ContinuityKind::Semantic {
        return Err(format!(
            "canonical checkpoint continuity_kind must be semantic, got {:?}",
            checkpoint.continuity_kind
        ));
    }
    for (index, item) in checkpoint.retained_tail_items.iter().enumerate() {
        if let Some((key, path)) =
            find_opaque_field_path(item, &format!("retained_tail_items[{index}]"))
        {
            return Err(format!(
                "canonical {path} still contains opaque field `{key}`"
            ));
        }
    }
    Ok(())
}

fn bounded_text(value: &str, max_chars: usize) -> String {
    let value = value.trim();
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut bounded = value.chars().take(max_chars).collect::<String>();
    bounded.push('…');
    bounded
}

fn bounded_strings(values: &[String], max_items: usize, max_chars: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    values
        .iter()
        .map(|value| bounded_text(value, max_chars))
        .filter(|value| !value.is_empty())
        .filter(|value| seen.insert(value.clone()))
        .take(max_items)
        .collect()
}

/// Lift a usable [`StructuredSummary`] into the issue #4 Canonical schema.
pub fn canonical_checkpoint_from_summary(
    summary: &StructuredSummary,
    retained_tail_items: Vec<Value>,
) -> Result<CanonicalCheckpoint, String> {
    if !summary.is_usable() {
        return Err("structured summary is not usable for a canonical checkpoint".into());
    }
    let exact_next_step = summary
        .next_steps
        .iter()
        .find(|step| !step.trim().is_empty())
        .map(|step| bounded_text(step, 1_024))
        .unwrap_or_else(|| "Continue from the latest retained user intent.".into());
    let files_changed = bounded_strings(&summary.changed_files, 24, 512)
        .iter()
        .map(|path| FileChangeState {
            path: path.clone(),
            summary: String::new(),
        })
        .collect();
    let mut discovered_facts = Vec::new();
    discovered_facts.extend(bounded_strings(&summary.done, 8, 384));
    discovered_facts.extend(bounded_strings(&summary.critical_context, 8, 384));
    discovered_facts.extend(bounded_strings(&summary.tests, 8, 384));
    let checkpoint = CanonicalCheckpoint {
        goal: bounded_text(&summary.goal, 2_048),
        constraints: bounded_strings(&summary.constraints, 8, 384),
        decisions: bounded_strings(&summary.decisions, 8, 384),
        discovered_facts,
        failed_attempts: bounded_strings(&summary.errors, 8, 384),
        files_changed,
        tool_state: Vec::new(),
        pending_tasks: bounded_strings(&summary.in_progress, 8, 384),
        unresolved_blockers: {
            let mut blocked = bounded_strings(&summary.blocked, 8, 384);
            blocked.extend(bounded_strings(&summary.unresolved, 8, 384));
            blocked.truncate(12);
            blocked
        },
        exact_next_step,
        retained_tail_items: sanitize_portable_history(&retained_tail_items),
        continuity_kind: ContinuityKind::Semantic,
    };
    validate_canonical_checkpoint(&checkpoint)?;
    Ok(checkpoint)
}

fn tool_pair_id(item: &Value) -> Option<&str> {
    item.get("call_id")
        .and_then(Value::as_str)
        .or_else(|| item.get("id").and_then(Value::as_str))
}

/// Tool call kinds recognized by the Codex adapter + compaction.
pub fn is_tool_call(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some(
            "function_call"
                | "local_shell_call"
                | "custom_tool_call"
                | "tool_search_call"
                | "computer_call"
                | "web_search_call"
        )
    )
}

/// Matching tool output / result kinds for the same family.
pub fn is_tool_output(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some(
            "function_call_output"
                | "local_shell_call_output"
                | "custom_tool_call_output"
                | "tool_search_output"
                | "computer_call_output"
                | "web_search_call_output"
        )
    )
}

/// Expand a recent slice so every tool call keeps its matching output (and
/// vice versa) when the pair exists in `full`. Uses multiset / newest-first
/// occurrence matching so duplicate identical messages only pull as many
/// occurrences as appear in `recent`.
pub fn enforce_tool_call_output_pairing(full: &[Value], recent: &[Value]) -> Vec<Value> {
    let mut recent_remaining: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for item in recent {
        let key = serde_json::to_string(item).unwrap_or_default();
        *recent_remaining.entry(key).or_insert(0) += 1;
    }
    let mut selected = vec![false; full.len()];
    for (i, item) in full.iter().enumerate().rev() {
        let key = serde_json::to_string(item).unwrap_or_default();
        if let Some(count) = recent_remaining.get_mut(&key) {
            if *count > 0 {
                *count -= 1;
                selected[i] = true;
            }
        }
    }
    let spans = tool_exchange_spans(full);
    for &(start, end) in &spans {
        let span_has_selected_tool = (start..end)
            .any(|i| selected[i] && (is_tool_call(&full[i]) || is_tool_output(&full[i])));
        if !span_has_selected_tool {
            continue;
        }
        for item in selected.iter_mut().take(end).skip(start) {
            *item = true;
        }
    }
    full.iter()
        .enumerate()
        .filter(|(i, _)| selected[*i])
        .map(|(_, item)| item.clone())
        .collect()
}

/// Occurrence-scoped tool exchange spans: each call pairs only with outputs of
/// the same `call_id` before the next call with that id. Overlapping parallel
/// spans are merged.
pub fn tool_exchange_spans(items: &[Value]) -> Vec<(usize, usize)> {
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let mut call_indices_by_id: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    let mut output_indices_by_id: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    for (i, item) in items.iter().enumerate() {
        if let Some(id) = tool_pair_id(item) {
            if is_tool_call(item) {
                call_indices_by_id
                    .entry(id.to_string())
                    .or_default()
                    .push(i);
            } else if is_tool_output(item) {
                output_indices_by_id
                    .entry(id.to_string())
                    .or_default()
                    .push(i);
            }
        }
    }
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if !is_tool_call(item) {
            continue;
        }
        let mut end = i + 1;
        if let Some(id) = tool_pair_id(item) {
            let next_same = call_indices_by_id
                .get(id)
                .and_then(|idxs| idxs.iter().copied().find(|&j| j > i))
                .unwrap_or(n);
            if let Some(outs) = output_indices_by_id.get(id) {
                for &oi in outs {
                    if oi > i && oi < next_same {
                        end = end.max(oi + 1);
                    }
                }
            }
        }
        spans.push((i, end));
    }
    spans.sort_by_key(|(s, _)| *s);
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in spans {
        if let Some((_, m_end)) = merged.last_mut() {
            if start < *m_end {
                *m_end = (*m_end).max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    merged
}

/// Atomic retained-tail unit: a contiguous source slice in exact original
/// order. Parallel tool spans merge into one batch.
#[derive(Debug, Clone, PartialEq)]
pub struct TailUnit {
    pub items: Vec<Value>,
}

impl TailUnit {
    pub fn tokens(&self) -> u64 {
        estimate_tokens(&self.items)
    }

    pub fn into_items(self) -> Vec<Value> {
        self.items
    }

    pub fn is_orphan_output_only(&self) -> bool {
        self.items.len() == 1 && is_tool_output(&self.items[0])
    }

    pub fn starts_with_tool_output(&self) -> bool {
        self.items.first().is_some_and(is_tool_output)
    }
}

/// Flatten packed units back to items (must equal source when packing is
/// complete).
pub fn flatten_tail_units(units: &[TailUnit]) -> Vec<Value> {
    units
        .iter()
        .flat_map(|unit| unit.items.iter().cloned())
        .collect()
}

/// Pack items into order-preserving atomic batches.
pub fn pack_tail_units(items: &[Value]) -> Vec<TailUnit> {
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let merged = tool_exchange_spans(items);
    let mut units = Vec::new();
    let mut i = 0usize;
    let mut m = 0usize;
    while i < n {
        if m < merged.len() && i == merged[m].0 {
            let (s, e) = merged[m];
            units.push(TailUnit {
                items: items[s..e].to_vec(),
            });
            i = e;
            m += 1;
            continue;
        }
        if m < merged.len() && i > merged[m].0 && i < merged[m].1 {
            i = merged[m].1;
            m += 1;
            continue;
        }
        units.push(TailUnit {
            items: vec![items[i].clone()],
        });
        i += 1;
    }
    units
}

/// Start index of the retained recent tail as a true contiguous source suffix
/// (`portable_items[start..]`).
pub fn retained_tail_start_index(
    portable_items: &[Value],
    keep_recent_turns: u32,
    max_tail_tokens: u64,
) -> usize {
    let n = portable_items.len();
    if n == 0 {
        return 0;
    }
    let max_tail_tokens = max_tail_tokens.max(256);

    let user_starts: Vec<usize> = portable_items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| matches!(classify_item(item), ItemRole::User).then_some(index))
        .collect();
    let keep = keep_recent_turns as usize;
    let mut window_start = if user_starts.is_empty() {
        0
    } else if user_starts.len() <= keep {
        user_starts[0]
    } else {
        user_starts[user_starts.len() - keep]
    };

    for &(s, e) in &tool_exchange_spans(portable_items) {
        if s < window_start && e > window_start {
            window_start = s;
        }
    }

    let window = &portable_items[window_start..];
    let units = pack_tail_units(window);
    if units.is_empty() {
        return n;
    }
    let mut kept_from_end = 0usize;
    let mut used = 0u64;
    for unit in units.iter().rev() {
        if kept_from_end == 0 && unit.is_orphan_output_only() {
            break;
        }
        let unit_tokens = unit.tokens();
        if unit_tokens > max_tail_tokens {
            break;
        }
        if used.saturating_add(unit_tokens) > max_tail_tokens {
            break;
        }
        used = used.saturating_add(unit_tokens);
        kept_from_end += 1;
    }
    if kept_from_end == 0 {
        return n;
    }
    let mut first_kept_unit = units.len() - kept_from_end;
    while first_kept_unit < units.len() && units[first_kept_unit].starts_with_tool_output() {
        first_kept_unit += 1;
    }
    if first_kept_unit >= units.len() {
        return n;
    }
    let mut start = window_start;
    for unit in units.iter().take(first_kept_unit) {
        start = start.saturating_add(unit.items.len());
    }
    debug_assert!(start <= n);
    start
}

/// Bound the recent portable tail from the newest atomic unit backward.
pub fn bound_retained_tail(
    portable_items: &[Value],
    keep_recent_turns: u32,
    max_tail_tokens: u64,
) -> Vec<Value> {
    let start = retained_tail_start_index(portable_items, keep_recent_turns, max_tail_tokens);
    portable_items[start..].to_vec()
}

/// Split request input into prior active history vs pending current-turn items.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActivePendingSplit {
    pub active_history: Vec<Value>,
    pub pending_items: Vec<Value>,
}

pub fn is_model_switch_marker(item: &Value) -> bool {
    let role = item.get("role").and_then(Value::as_str);
    if !matches!(role, Some("developer" | "system")) {
        return false;
    }
    item_text(item)
        .unwrap_or_default()
        .contains("<model_switch>")
}

fn is_pending_user(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("user") && !is_summary_item(item)
}

/// Split active (compactable prior history) from pending current-turn items.
pub fn split_active_and_pending(items: &[Value]) -> ActivePendingSplit {
    if items.is_empty() {
        return ActivePendingSplit::default();
    }
    let searchable: Vec<(usize, &Value)> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.get("type").and_then(Value::as_str) != Some("compaction_trigger"))
        .collect();
    let Some(&(last_user_idx, _)) = searchable
        .iter()
        .rev()
        .find(|(_, item)| is_pending_user(item))
    else {
        return ActivePendingSplit {
            active_history: items.to_vec(),
            pending_items: Vec::new(),
        };
    };
    let mut pending_start = last_user_idx;
    while pending_start > 0 && is_model_switch_marker(&items[pending_start - 1]) {
        pending_start -= 1;
    }
    ActivePendingSplit {
        active_history: items[..pending_start].to_vec(),
        pending_items: items[pending_start..].to_vec(),
    }
}

/// Split portable history into early (to summarize) and bounded recent tail.
pub fn split_canonical_source(
    portable_items: &[Value],
    keep_recent_turns: u32,
    max_tail_tokens: u64,
) -> (Vec<Value>, Vec<Value>) {
    let start = retained_tail_start_index(portable_items, keep_recent_turns, max_tail_tokens);
    let early = portable_items[..start].to_vec();
    let tail = portable_items[start..].to_vec();
    debug_assert_eq!(early.len() + tail.len(), portable_items.len());
    (early, tail)
}

/// Issue #4 B-1 shrink gate: the installed canonical window must be strictly
/// smaller than the portable source window.
pub fn canonical_replacement_frees_context(
    source_tokens: u64,
    replacement_tokens: u64,
    _output_reserve_tokens: u64,
    _tool_reserve_tokens: u64,
) -> bool {
    source_tokens > 0 && replacement_tokens < source_tokens
}

/// Install a validated Canonical checkpoint as portable conversation items.
/// Order: summary then retained active tail (pending user/tool appended by
/// caller).
pub fn install_canonical_checkpoint_items(checkpoint: &CanonicalCheckpoint) -> Vec<Value> {
    let mut summary_only = checkpoint.clone();
    summary_only.retained_tail_items = Vec::new();
    let mut items = vec![serde_json::json!({
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": format!(
                "{}\n{}",
                OFFICIAL_SUMMARY_PREFIX,
                serde_json::to_string_pretty(&summary_only).unwrap_or_else(|_| "{}".into())
            )
        }],
        "metadata": {
            "continuity_kind": ContinuityKind::Semantic.as_str(),
            "vellum_checkpoint": "canonical"
        }
    })];
    items.extend(checkpoint.retained_tail_items.clone());
    items
}

/// Ask the model for semantic content only. Vellum owns the stable checkpoint
/// schema and assembles the response below, so provider-specific JSON
/// formatting behavior cannot break compaction.
pub fn summary_prompt(early: &[Value]) -> String {
    format!(
        "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary \
         for another LLM that will resume the task. Preserve facts and exact file paths, \
         symbols, commands, test outcomes, errors, constraints, user preferences, key \
         decisions, rejected approaches, and the next executable step. Do not invent \
         completion. Use concise plain text with clear headings; do not spend tokens formatting \
         JSON because the caller will assemble the final checkpoint schema. Make the handoff \
         sufficient to continue without re-exploring the codebase.\n\nConversation:\n{}",
        serde_json::to_string(early).unwrap_or_else(|_| "[]".into())
    )
}

/// Assemble arbitrary model prose into Vellum's stable checkpoint schema.
/// Existing structured JSON remains accepted for compatibility, but is not a
/// correctness requirement.
pub fn assemble_structured_summary(text: &str, source: &[Value]) -> StructuredSummary {
    if let Some(summary) = parse_structured_summary_text(text).filter(StructuredSummary::is_usable)
    {
        return summary;
    }

    let mut summary = fallback_structured_summary(source);
    let chars = text.trim().chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return summary;
    }
    let selected = if chars.len() <= 3_072 {
        chars
    } else {
        let mut value = chars[..1_472].to_vec();
        value.extend("\n...[summary middle omitted by Vellum]...\n".chars());
        value.extend_from_slice(&chars[chars.len() - 1_472..]);
        value
    };
    summary.critical_context = selected
        .chunks(384)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect();
    summary
}

pub fn parse_structured_summary(value: &Value) -> Option<StructuredSummary> {
    let direct = if value.is_object() {
        value.clone()
    } else {
        return None;
    };
    serde_json::from_value(direct).ok().or_else(|| {
        value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.pointer("/message/content"))
            .and_then(Value::as_str)
            .and_then(|content| serde_json::from_str(content).ok())
    })
}

/// Parse a structured checkpoint from model text while still requiring a
/// syntactically valid JSON object.
pub fn parse_structured_summary_text(text: &str) -> Option<StructuredSummary> {
    let trimmed = text.trim();
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```JSON"))
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .strip_suffix("```")
        .unwrap_or(trimmed)
        .trim();

    if let Ok(value) = serde_json::from_str::<Value>(unfenced) {
        if let Some(summary) = parse_structured_summary(&value) {
            return Some(summary);
        }
    }

    let bytes = trimmed.as_bytes();
    for start in bytes
        .iter()
        .enumerate()
        .filter_map(|(index, byte)| (*byte == b'{').then_some(index))
    {
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        for (offset, byte) in bytes[start..].iter().enumerate() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if *byte == b'\\' {
                    escaped = true;
                } else if *byte == b'"' {
                    in_string = false;
                }
                continue;
            }
            match *byte {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        let candidate = &trimmed[start..start + offset + 1];
                        if let Ok(value) = serde_json::from_str::<Value>(candidate) {
                            if let Some(summary) = parse_structured_summary(&value) {
                                return Some(summary);
                            }
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Split a large transcript into bounded prompts.
pub fn chunk_compaction_items(items: &[Value], max_tokens: usize) -> Vec<Vec<Value>> {
    let max_tokens = max_tokens.max(1);
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut current_tokens = 0usize;

    for item in items {
        let mut item = item.clone();
        let mut item_tokens = estimate_tokens(std::slice::from_ref(&item)) as usize;
        if item_tokens > max_tokens {
            item = oversized_item_excerpt(&item, max_tokens);
            item_tokens = estimate_tokens(std::slice::from_ref(&item)) as usize;
        }
        if !current.is_empty() && current_tokens.saturating_add(item_tokens) > max_tokens {
            chunks.push(std::mem::take(&mut current));
            current_tokens = 0;
        }
        current_tokens = current_tokens.saturating_add(item_tokens);
        current.push(item);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn oversized_item_excerpt(item: &Value, max_tokens: usize) -> Value {
    let serialized = serde_json::to_string(item).unwrap_or_default();
    let max_chars = max_tokens.saturating_mul(4).saturating_sub(256).max(256);
    let chars = serialized.chars().collect::<Vec<_>>();
    let excerpt = if chars.len() <= max_chars {
        serialized
    } else {
        let half = max_chars / 2;
        format!(
            "{}\n...[oversized item truncated for compaction]...\n{}",
            chars[..half].iter().collect::<String>(),
            chars[chars.len() - half..].iter().collect::<String>()
        )
    };
    serde_json::json!({
        "type": "compaction_excerpt",
        "text": excerpt
    })
}

pub fn merge_structured_summaries(summaries: &[StructuredSummary]) -> StructuredSummary {
    let mut merged = StructuredSummary::default();
    for summary in summaries {
        if !summary.goal.trim().is_empty() {
            merged.goal = summary.goal.clone();
        }
        macro_rules! append_unique {
            ($field:ident) => {
                for value in &summary.$field {
                    if !value.trim().is_empty() && !merged.$field.contains(value) {
                        merged.$field.push(value.clone());
                    }
                }
            };
        }
        append_unique!(acceptance_criteria);
        append_unique!(constraints);
        append_unique!(user_preferences);
        append_unique!(done);
        append_unique!(in_progress);
        append_unique!(blocked);
        append_unique!(decisions);
        append_unique!(changed_files);
        append_unique!(relevant_files);
        append_unique!(commands);
        append_unique!(tests);
        append_unique!(unresolved);
        append_unique!(errors);
        append_unique!(critical_context);
        append_unique!(references);
        append_unique!(next_steps);
    }
    merged
}

pub fn summary_item(summary: &StructuredSummary) -> Value {
    serde_json::json!({
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": format!(
                "{}\n{}",
                OFFICIAL_SUMMARY_PREFIX,
                serde_json::to_string_pretty(summary).unwrap_or_else(|_| "{}".into())
            )
        }]
    })
}

pub fn is_summary_item(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("user")
        && item_text(item).is_some_and(|text| text.starts_with(OFFICIAL_SUMMARY_PREFIX))
}

/// Estimate tokens for history items (~4 bytes/token).
pub fn estimate_tokens(items: &[Value]) -> u64 {
    let bytes: usize = items
        .iter()
        .map(|i| serde_json::to_vec(i).map(|b| b.len()).unwrap_or(0))
        .sum();
    (bytes / 4).max(1) as u64
}

/// Estimate tokens for an arbitrary JSON value.
pub fn estimate_json_tokens(value: &Value) -> u64 {
    if value.is_null() {
        return 0;
    }
    let bytes = serde_json::to_vec(value).map(|b| b.len()).unwrap_or(0);
    if bytes == 0 {
        return 0;
    }
    (bytes / 4).max(1) as u64
}

pub fn is_canonical_checkpoint_item(item: &Value) -> bool {
    item.get("metadata")
        .and_then(|value| value.get("vellum_checkpoint"))
        .and_then(Value::as_str)
        == Some("canonical")
        || is_summary_item(item)
        || matches!(
            item.get("type").and_then(Value::as_str),
            Some("compaction" | "context_compaction")
        )
}

/// Rebuild a Canonical checkpoint from durable pre-compact source history.
/// The client-local prose summary is not an input; Vellum owns assembly.
pub fn rebuild_canonical_checkpoint_from_source(source: &[Value]) -> Result<Vec<Value>, String> {
    if source.is_empty() {
        return Err("cannot rebuild a canonical checkpoint from empty source history".into());
    }
    let portable = sanitize_portable_history(source);
    let (early, tail) = split_canonical_source(
        &portable,
        DEFAULT_KEEP_RECENT_TURNS,
        CANONICAL_MAX_TAIL_TOKENS,
    );
    let summary = fallback_structured_summary(&early);
    let mut checkpoint = canonical_checkpoint_from_summary(&summary, tail)?;
    checkpoint.tool_state = extract_tool_state(source);
    validate_canonical_checkpoint(&checkpoint)?;
    Ok(install_canonical_checkpoint_items(&checkpoint))
}

fn extract_tool_state(items: &[Value]) -> Vec<ToolState> {
    let mut states = Vec::new();
    let mut pending: HashMap<String, String> = HashMap::new();
    for item in items {
        if is_tool_call(item) {
            let id = tool_pair_id(item).unwrap_or("").to_string();
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            if !id.is_empty() {
                pending.insert(id, name.clone());
            }
            states.push(ToolState {
                name,
                status: "pending".into(),
                detail: String::new(),
            });
        } else if is_tool_output(item) {
            if let Some(id) = tool_pair_id(item) {
                if let Some(name) = pending.remove(id) {
                    if let Some(state) = states.iter_mut().rev().find(|state| state.name == name) {
                        state.status = "completed".into();
                    }
                }
            }
        }
    }
    states.truncate(24);
    states
}

/// True when the item is a tool call or output (shared classifier for
/// pairing and pending accounting).
pub fn is_pending_tool_item(item: &Value) -> bool {
    is_tool_call(item) || is_tool_output(item)
}

/// Top-level `instructions` / `tools` tokens count toward the real window.
pub fn estimate_non_input_context_tokens(request: &Value) -> u64 {
    let mut total = 0u64;
    if let Some(instructions) = request.get("instructions") {
        total = total.saturating_add(estimate_json_tokens(instructions));
    }
    if let Some(tools) = request.get("tools") {
        total = total.saturating_add(estimate_json_tokens(tools));
    }
    total
}

pub fn has_compaction_trigger(request: &Value) -> bool {
    request
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction_trigger"))
        })
}

/// Estimate whether Canonical compaction should run before the next turn
/// (Desktop `should_compact_canonical`).
pub fn should_compact_canonical(
    active_context_tokens: u64,
    pending_user_tokens: u64,
    pending_tool_tokens: u64,
    output_reserve: u64,
    tool_reserve: u64,
    window_tokens: u64,
    threshold_percent: u32,
) -> bool {
    if window_tokens == 0 {
        return false;
    }
    let used = active_context_tokens
        .saturating_add(pending_user_tokens)
        .saturating_add(pending_tool_tokens)
        .saturating_add(output_reserve)
        .saturating_add(tool_reserve);
    let threshold = window_tokens.saturating_mul(threshold_percent as u64) / 100;
    used >= threshold
}

pub const GROK_BUILD_COMPACTION_PROMPT_VERSION: &str = "grok-build-0.2.112-nine-section";

/// Concatenate the response output text (Desktop `response_output_text`).
pub fn response_output_text(response: &Value) -> String {
    let mut chunks = Vec::new();
    if let Some(text) = response.get("output_text").and_then(Value::as_str) {
        chunks.push(text.to_string());
    }
    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for item in output {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        chunks.push(text.to_string());
                    }
                }
            }
        }
    }
    chunks.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant(text: &str) -> Value {
        json!({
            "type": "message",
            "id": "m",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}]
        })
    }

    #[test]
    fn sanitize_drops_system_items_and_opaque_fields() {
        let items = vec![
            json!({"type": "message", "role": "system", "content": "instructions"}),
            json!({"type": "reasoning", "encrypted_content": "cipher"}),
            json!({"type": "message", "role": "user", "content": "hi"}),
        ];
        let portable = sanitize_portable_history(&items);
        assert_eq!(portable.len(), 1);
        assert_eq!(portable[0]["content"], "hi");
    }

    #[test]
    fn sanitize_drops_reasoning_whose_only_visible_field_is_null() {
        let items = vec![
            json!({
                "type": "reasoning",
                "id": "reasoning_1",
                "content": null,
                "encrypted_content": "provider-private"
            }),
            json!({"type": "message", "role": "user", "content": "continue"}),
        ];

        let portable = sanitize_portable_history(&items);
        assert_eq!(portable, vec![items[1].clone()]);
    }

    #[test]
    fn sanitize_preserves_reasoning_summary_but_omits_null_content() {
        let items = vec![json!({
            "type": "reasoning",
            "id": "reasoning_1",
            "summary": [{
                "type": "summary_text",
                "text": "Keep this portable summary."
            }],
            "content": null,
            "encrypted_content": "provider-private"
        })];

        let portable = sanitize_portable_history(&items);
        assert_eq!(portable.len(), 1);
        assert_eq!(portable[0]["summary"], items[0]["summary"]);
        assert!(portable[0].get("content").is_none());
        assert!(portable[0].get("encrypted_content").is_none());
    }

    #[test]
    fn fallback_summary_preserves_goal_and_recent_visible_context() {
        let items = vec![
            json!({"type": "message", "role": "user", "content": "Fix the failing parser"}),
            json!({"type": "reasoning", "encrypted_content": "secret"}),
            json!({"type": "message", "role": "assistant", "content": "Updated src/parser.rs"}),
            json!({"type": "message", "role": "user", "content": "Run the focused test next"}),
        ];
        let summary = fallback_structured_summary(&items);
        assert_eq!(summary.goal, "Fix the failing parser");
        assert!(summary.is_usable());
        assert!(summary
            .critical_context
            .iter()
            .any(|line| line.contains("Updated src/parser.rs")));
        assert!(summary
            .critical_context
            .iter()
            .all(|line| !line.contains("secret")));
        canonical_checkpoint_from_summary(&summary, Vec::new()).unwrap();
    }

    #[test]
    fn split_canonical_source_is_a_positional_cut() {
        let items: Vec<Value> = (0..10)
            .map(|i| {
                json!({
                    "type": "message",
                    "role": if i % 2 == 0 { "user" } else { "assistant" },
                    "content": format!("item {i}")
                })
            })
            .collect();
        let (early, tail) = split_canonical_source(&items, 2, 256);
        assert_eq!(early.len() + tail.len(), items.len());
        assert!(!tail.is_empty());
        assert_eq!(tail[0]["content"], items[early.len()]["content"]);
    }

    #[test]
    fn checkpoint_from_summary_validates() {
        let summary = StructuredSummary {
            goal: "Build the feature".into(),
            next_steps: vec!["Run the tests".into()],
            done: vec!["Wrote the code".into()],
            ..Default::default()
        };
        let checkpoint =
            canonical_checkpoint_from_summary(&summary, vec![assistant("tail reply")]).unwrap();
        assert_eq!(checkpoint.continuity_kind, ContinuityKind::Semantic);
        assert_eq!(checkpoint.exact_next_step, "Run the tests");
        assert_eq!(checkpoint.retained_tail_items.len(), 1);
        validate_canonical_checkpoint(&checkpoint).unwrap();
    }

    #[test]
    fn parse_structured_summary_text_handles_fences_and_prose() {
        let wrapped = format!(
            "Here is the checkpoint:\n```json\n{}\n```",
            json!({
                "goal": "g",
                "done": ["d"]
            })
        );
        let summary = parse_structured_summary_text(&wrapped).unwrap();
        assert_eq!(summary.goal, "g");
        assert_eq!(summary.done, vec!["d"]);
    }

    #[test]
    fn prose_summary_is_assembled_by_vellum() {
        let source = vec![
            json!({"type": "message", "role": "user", "content": "Fix parser continuity"}),
            json!({"type": "message", "role": "assistant", "content": "Changed src/parser.rs"}),
        ];
        let summary = assemble_structured_summary(
            "Goal\nKeep the parser fix.\n\nNext step\nRun parser_tests.",
            &source,
        );
        assert_eq!(summary.goal, "Fix parser continuity");
        assert!(summary.is_usable());
        assert!(summary
            .critical_context
            .join("\n")
            .contains("Run parser_tests"));
        canonical_checkpoint_from_summary(&summary, Vec::new()).unwrap();
    }

    #[test]
    fn summary_extraction_from_a_responses_completion_is_usable() {
        // The exact shape the `compact_canonical` parity fixture scripts as
        // the summarizer turn: a Responses completion whose output text is
        // the strict-JSON checkpoint.
        let summary_text = json!({
            "goal": "Answer the conversation turns.",
            "done": [
                "Turn 1 answered",
                "Turn 2 answered",
                "Turn 3 answered",
                "Turn 4 answered"
            ],
            "in_progress": ["Turn 7 pending"],
            "decisions": ["Keep replies concise"],
            "next_steps": ["Answer turn 7"],
            "constraints": [],
            "user_preferences": []
        })
        .to_string();
        let response = json!({
            "id": "resp_sum_1",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "message",
                "id": "msg_sum_1",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": summary_text, "annotations": []}]
            }],
            "usage": {"input_tokens": 40, "output_tokens": 30, "total_tokens": 70}
        });
        let text = response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(|item| {
                item.get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<String>();
        let summary = parse_structured_summary_text(&text)
            .expect("scripted summary must parse to a structured checkpoint");
        assert_eq!(summary.goal, "Answer the conversation turns.");
        assert!(summary.is_usable());
    }

    #[test]
    fn tool_pairing_keeps_call_with_output() {
        let full = vec![
            json!({"type": "function_call", "id": "call_1", "call_id": "call_1", "name": "f", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "call_1", "output": "ok"}),
            json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done", "annotations": []}]}),
        ];
        let recent = vec![full[0].clone()];
        let expanded = enforce_tool_call_output_pairing(&full, &recent);
        assert_eq!(expanded.len(), 2);
        assert_eq!(expanded[1]["type"], "function_call_output");
    }

    #[test]
    fn estimate_tokens_uses_4_bytes_per_token() {
        let items = vec![json!({"content": "hello world hello world"})];
        assert!(estimate_tokens(&items) >= 1);
        assert_eq!(estimate_json_tokens(&Value::Null), 0);
    }

    #[test]
    fn rebuild_from_source_discards_client_degenerate_summary() {
        let degenerate = "DEGENERATE client summary that must never go upstream";
        let source = vec![
            json!({"type": "message", "role": "user", "content": "Implement the parser"}),
            json!({
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "shell_command",
                "arguments": "{}"
            }),
            json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "ok"
            }),
            json!({"type": "message", "role": "assistant", "content": "Updated src/parser.rs"}),
        ];
        let rebuilt = rebuild_canonical_checkpoint_from_source(&source).unwrap();
        let serialized = serde_json::to_string(&rebuilt).unwrap();
        assert!(!serialized.contains(degenerate));
        assert!(rebuilt.iter().any(is_canonical_checkpoint_item));
        assert!(rebuilt.iter().any(|item| {
            item.get("metadata")
                .and_then(|value| value.get("vellum_checkpoint"))
                .and_then(Value::as_str)
                == Some("canonical")
        }));
    }
}
