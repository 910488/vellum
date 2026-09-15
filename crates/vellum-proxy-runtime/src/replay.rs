//! Proxy-private replay provenance, Chat transcript invariants, and
//! content-free harness diagnostics.
//!
//! Provenance stays on the Rust call chain. It is never written into request
//! JSON, so Official passthrough and third-party upstream bodies cannot leak
//! it. Overlap skip follows the OpenCodex safety gate: a stored suffix is
//! dropped from prepend only when it fully matches a stored entry, crosses a
//! provider-output boundary, and carries a provider-issued item id. Identical
//! user-message content alone never dedupes.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::ops::Range;

use crate::compaction::{
    is_canonical_checkpoint_item, is_model_switch_marker, is_summary_item, is_tool_call,
    is_tool_output,
};
use crate::continuation::ContinuationMode;
use crate::diagnostics::hash_text;
use crate::history::{flatten_chain_items, HistoryEntry};
use crate::request::input_value_items;
use crate::route::{ContinuationTail, ReasoningReplay, RuntimeChatCapabilities};

/// Fixed Chat user-bridge used only when [`ContinuationTail::NeutralUserBridge`]
/// is set. The original user task is never copied into this message.
pub const NEUTRAL_USER_BRIDGE_TEXT: &str = "Continue from the previous tool result. The original user task has not been resent or changed.";

/// Preface for a canonical checkpoint projected onto the Chat wire. The
/// internal Responses checkpoint stays a user-role summary; Chat must not
/// present it as a new user command.
pub const CHAT_CHECKPOINT_PREFACE: &str = "Conversation checkpoint; not a new user request";

/// Fixed Chat continuation appended only when a checkpoint is the last
/// message.
///
/// Codex's own local compaction ends the replacement history with the summary
/// itself, so nothing follows the checkpoint on the next turn. Projecting that
/// checkpoint as assistant-owned — correct while a tool result or a user turn
/// follows it — then closes the Chat transcript on an assistant message, which
/// an OpenAI-compatible backend reads as a prefill to be continued rather than
/// as context: the observed result is the checkpoint echoed back verbatim for
/// one completion token, with no tool call, which Codex accepts as a finished
/// turn. This message restores the "keep working" tail without copying the
/// original user task, which is never resent.
pub const CHAT_CHECKPOINT_RESUME_TEXT: &str = "Continue the task described in the conversation checkpoint above, starting from where it stopped. The checkpoint is context, not a new user request, and the original user task has not been resent or changed.";

/// Result of hydrating stored history into a request body.
///
/// Fields describe the hydrated `input` array after prepend/overlap skip.
/// They are never serialized onto the upstream request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydrationOutcome {
    pub replayed_prefix_len: usize,
    pub current_suffix_range: Range<usize>,
    pub continuation_mode: ContinuationMode,
    pub client_carried_full_history: bool,
    pub overlap_skipped: usize,
    pub same_route: bool,
    /// Parallel to the hydrated `input` array. Stored items use
    /// `store:{response_id}:{input|output}:{index}`; the current suffix uses
    /// `req:{request_index}`. Never written into request JSON.
    pub item_identities: Vec<String>,
}

impl HydrationOutcome {
    pub fn native_passthrough(input_len: usize) -> Self {
        Self {
            replayed_prefix_len: 0,
            current_suffix_range: 0..input_len,
            continuation_mode: ContinuationMode::ServerPreviousResponseId,
            client_carried_full_history: false,
            overlap_skipped: 0,
            same_route: true,
            item_identities: Vec::new(),
        }
    }

    pub fn first_turn(input_len: usize) -> Self {
        Self {
            replayed_prefix_len: 0,
            current_suffix_range: 0..input_len,
            continuation_mode: ContinuationMode::PortableSemanticReplay,
            client_carried_full_history: false,
            overlap_skipped: 0,
            same_route: true,
            item_identities: (0..input_len).map(request_item_identity).collect(),
        }
    }

    pub fn replay_context(&self) -> ReplayContext {
        ReplayContext {
            prefix_len: self.replayed_prefix_len,
            suffix_start: self.current_suffix_range.start,
            suffix_end: self.current_suffix_range.end,
            continuation_mode: self.continuation_mode,
            client_carried_full_history: self.client_carried_full_history,
            overlap_skipped: self.overlap_skipped,
            allow_reasoning_replay: self.same_route,
            compaction_repairs: 0,
            same_route: self.same_route,
            item_identities: self.item_identities.clone(),
        }
    }
}

/// Per-request Chat conversion provenance. Never a wire field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayContext {
    pub prefix_len: usize,
    pub suffix_start: usize,
    pub suffix_end: usize,
    pub continuation_mode: ContinuationMode,
    pub client_carried_full_history: bool,
    pub overlap_skipped: usize,
    /// False on a route/model/credential switch so readable reasoning from
    /// the previous realm is not replayed.
    pub allow_reasoning_replay: bool,
    pub compaction_repairs: usize,
    pub same_route: bool,
    pub item_identities: Vec<String>,
}

impl Default for ReplayContext {
    fn default() -> Self {
        Self {
            prefix_len: 0,
            suffix_start: 0,
            suffix_end: usize::MAX,
            continuation_mode: ContinuationMode::PortableSemanticReplay,
            client_carried_full_history: false,
            overlap_skipped: 0,
            allow_reasoning_replay: true,
            compaction_repairs: 0,
            same_route: true,
            item_identities: Vec::new(),
        }
    }
}

impl ReplayContext {
    pub fn provenance_for_item(&self, index: usize) -> ChatMessageProvenance {
        ChatMessageProvenance {
            occurrence_id: Some(
                self.item_identities
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| request_item_identity(index)),
            ),
            from_prefix: index < self.prefix_len,
        }
    }

    pub fn suffix_items<'a>(&self, items: &'a [Value]) -> &'a [Value] {
        let start = self.suffix_start.min(items.len());
        let end = self.suffix_end.min(items.len()).max(start);
        &items[start..end]
    }

    pub fn prefix_items<'a>(&self, items: &'a [Value]) -> &'a [Value] {
        let end = self.prefix_len.min(items.len());
        &items[..end]
    }
}

/// Append a synthetic item (such as a loop recovery developer guidance message) to the input array
/// while keeping the replay item identities and suffix range in exact lockstep.
pub fn append_synthetic_replay_item(
    body: &mut Value,
    replay: &mut ReplayContext,
    item: Value,
    identity: String,
) -> Result<(), String> {
    let input_arr = body
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "request body is missing input array".to_string())?;
    input_arr.push(item);
    replay.item_identities.push(identity);
    if replay.suffix_end != usize::MAX {
        replay.suffix_end += 1;
    }
    if input_arr.len() != replay.item_identities.len() {
        return Err(format!(
            "input array length ({}) does not match replay item identities length ({})",
            input_arr.len(),
            replay.item_identities.len()
        ));
    }
    Ok(())
}

/// Content-free transcript counts emitted before dispatch.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct HarnessTranscriptDiagnostics {
    /// Number of Chat messages after all Responses-to-Chat normalization.
    pub message_count: u64,
    /// A bounded role-only summary such as `system>user>assistant>tool`.
    /// It intentionally contains no message content.
    pub role_sequence: String,
    pub empty_messages: u64,
    pub tool_pairs: u64,
    pub orphan_tool_results: u64,
    pub duplicate_tool_ids: u64,
    pub first_role: Option<String>,
    pub last_role: Option<String>,
    pub body_bytes: Option<u64>,
    /// Serialized bytes occupied by the final Chat `messages` array.
    pub message_bytes: Option<u64>,
    /// Serialized bytes occupied by system messages inside `messages`.
    pub system_message_bytes: Option<u64>,
    /// Number of function declarations in the final Chat `tools` array.
    pub tool_count: u64,
    /// Serialized bytes occupied by the final Chat `tools` array.
    pub tool_bytes: Option<u64>,
    pub model: Option<String>,
    pub upstream_status: Option<u16>,
    pub replay_prefix_items: u64,
    pub current_suffix_items: u64,
    pub user_messages: u64,
    pub assistant_messages: u64,
    pub tool_messages: u64,
    pub system_messages: u64,
    pub tool_calls: u64,
    pub summaries: u64,
    pub overlap_skips: u64,
    pub neutral_bridges: u64,
    /// Fixed continuations appended because the transcript ended on a
    /// compaction checkpoint. Expected to be 0 or 1.
    pub checkpoint_resumes: u64,
    pub compaction_repairs: u64,
    /// Hashes of user-turn *occurrence ids* (never prompt text). Detects the
    /// same stored occurrence being serialized twice.
    pub user_turn_hashes: Vec<String>,
}

/// Proxy-private Chat projection provenance. Never written into request JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessageProvenance {
    pub occurrence_id: Option<String>,
    pub from_prefix: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatProjection {
    pub messages: Vec<Value>,
    pub provenance: Vec<ChatMessageProvenance>,
}

impl ChatProjection {
    pub fn push(&mut self, value: Value, provenance: ChatMessageProvenance) {
        self.messages.push(value);
        self.provenance.push(provenance);
    }

    pub fn into_messages(self) -> Vec<Value> {
        self.messages
    }

    pub fn into_pairs(self) -> Vec<(Value, ChatMessageProvenance)> {
        let mut provenance = self.provenance.into_iter();
        self.messages
            .into_iter()
            .map(|message| {
                (
                    message,
                    provenance.next().unwrap_or(ChatMessageProvenance {
                        occurrence_id: None,
                        from_prefix: false,
                    }),
                )
            })
            .collect()
    }

    pub fn from_pairs(pairs: Vec<(Value, ChatMessageProvenance)>) -> Self {
        let mut projection = Self::default();
        for (message, provenance) in pairs {
            projection.push(message, provenance);
        }
        projection
    }

    pub fn from_messages(messages: Vec<Value>) -> Self {
        let provenance = vec![
            ChatMessageProvenance {
                occurrence_id: None,
                from_prefix: false,
            };
            messages.len()
        ];
        Self {
            messages,
            provenance,
        }
    }
}

pub fn stored_item_identity(response_id: &str, section: &str, index: usize) -> String {
    format!("store:{response_id}:{section}:{index}")
}

pub fn request_item_identity(index: usize) -> String {
    format!("req:{index}")
}

/// Stable identity for a journal-materialized checkpoint item.
pub fn journal_item_identity(compaction_id: &str, index: usize) -> String {
    format!("journal:{compaction_id}:{index}")
}

/// Stable identity for a locally repaired canonical checkpoint item.
pub fn checkpoint_item_identity(compaction_id: &str, index: usize) -> String {
    format!("checkpoint:{compaction_id}:{index}")
}

pub fn assert_input_identity_len(input_len: usize, identities: &[String]) -> Result<(), String> {
    if input_len != identities.len() {
        Err(format!(
            "internal protocol error: input length {input_len} != item_identities {}",
            identities.len()
        ))
    } else {
        Ok(())
    }
}

pub fn seed_request_identities(input_len: usize) -> Vec<String> {
    (0..input_len).map(request_item_identity).collect()
}

pub fn occurrence_id_for_item(_item: &Value, input_index: usize) -> String {
    request_item_identity(input_index)
}

fn flatten_chain_identities(chain: &[HistoryEntry]) -> Vec<String> {
    let mut out = Vec::new();
    for entry in chain {
        for index in 0..entry.input_items.len() {
            out.push(stored_item_identity(&entry.response_id, "input", index));
        }
        for index in 0..entry.output_items.len() {
            out.push(stored_item_identity(&entry.response_id, "output", index));
        }
    }
    out
}

/// Strip `previous_response_id` and replay the stored chain into `body.input`.
///
/// Safe overlap skip only fires when the client's prefix fully matches a
/// stored suffix that includes provider output and a provider-issued id.
pub fn hydrate_input_with_mode(
    body: &mut Value,
    chain: &[HistoryEntry],
    mode: ContinuationMode,
) -> HydrationOutcome {
    let had_previous = body
        .get("previous_response_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty());
    if had_previous {
        if let Some(object) = body.as_object_mut() {
            object.remove("previous_response_id");
        }
    }
    let Some(input) = body.get_mut("input") else {
        return HydrationOutcome {
            replayed_prefix_len: 0,
            current_suffix_range: 0..0,
            continuation_mode: mode,
            client_carried_full_history: false,
            overlap_skipped: 0,
            same_route: true,
            item_identities: Vec::new(),
        };
    };
    let original = std::mem::take(input);
    let current_items = input_value_items(original);
    let current_identities: Vec<String> = (0..current_items.len())
        .map(request_item_identity)
        .collect();
    if chain.is_empty() {
        let len = current_items.len();
        *input = restore_input(current_items);
        return HydrationOutcome {
            replayed_prefix_len: 0,
            current_suffix_range: 0..len,
            continuation_mode: mode,
            client_carried_full_history: false,
            overlap_skipped: 0,
            same_route: true,
            item_identities: current_identities,
        };
    }

    let stored = flatten_chain_items(chain, &[]);
    let stored_identities = flatten_chain_identities(chain);
    let overlap = safe_overlap_len(chain, &stored, &current_items);
    let client_carried_full_history = overlap == stored.len() && !stored.is_empty();
    let prepended = if overlap == 0 {
        stored
    } else {
        stored[..stored.len() - overlap].to_vec()
    };
    let prepended_identities = if overlap == 0 {
        stored_identities
    } else {
        stored_identities[..stored_identities.len() - overlap].to_vec()
    };
    let prefix_len = prepended.len() + overlap;
    let mut hydrated = prepended;
    hydrated.extend(current_items);
    let mut item_identities = prepended_identities;
    item_identities.extend(current_identities);
    let total = hydrated.len();
    let suffix_start = prefix_len.min(total);
    *input = Value::Array(hydrated);
    HydrationOutcome {
        replayed_prefix_len: prefix_len,
        current_suffix_range: suffix_start..total,
        continuation_mode: mode,
        client_carried_full_history,
        overlap_skipped: overlap,
        same_route: true,
        item_identities,
    }
}

fn restore_input(current_items: Vec<Value>) -> Value {
    if current_items.len() == 1 {
        current_items.into_iter().next().unwrap_or(Value::Null)
    } else {
        Value::Array(current_items)
    }
}

fn safe_overlap_len(chain: &[HistoryEntry], stored: &[Value], current: &[Value]) -> usize {
    let max = stored.len().min(current.len());
    for k in (1..=max).rev() {
        if stored[stored.len() - k..] != current[..k] {
            continue;
        }
        if !overlap_includes_full_entry_output(chain, stored.len() - k, stored.len()) {
            continue;
        }
        let overlap = &stored[stored.len() - k..];
        if !overlap.iter().any(has_provider_issued_id) {
            continue;
        }
        if overlap.iter().all(is_user_message_item) {
            continue;
        }
        return k;
    }
    0
}

fn overlap_includes_full_entry_output(chain: &[HistoryEntry], start: usize, end: usize) -> bool {
    let mut idx = 0usize;
    for entry in chain {
        let input_end = idx + entry.input_items.len();
        let entry_end = input_end + entry.output_items.len();
        if !entry.output_items.is_empty() && input_end >= start && entry_end <= end {
            return true;
        }
        idx = entry_end;
    }
    false
}

fn has_provider_issued_id(item: &Value) -> bool {
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| item.get("call_id").and_then(Value::as_str))
        .unwrap_or("");
    if id.trim().is_empty() {
        return false;
    }
    !(id.starts_with("resp_vellum_")
        || id.starts_with("cmp_vellum_")
        || id.starts_with("cmp_local_")
        || id.starts_with(LEGACY_CANONICAL_MARKER_PREFIX)
        || id.starts_with(CODEX_LOCAL_V0_150_MARKER_PREFIX))
}

fn is_user_message_item(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("user")
        && !is_tool_call(item)
        && !is_tool_output(item)
}

/// How a compaction-shaped item must be handled. Journal materialization
/// runs first; only [`Self::ProvenClientLocal`] is eligible for the local
/// repair overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionDisposition {
    VellumJournal,
    ProvenClientLocal,
    ProviderOwnedOpaque,
    Unknown,
}

pub fn classify_compaction_item(item: &Value) -> Option<CompactionDisposition> {
    match item.get("type").and_then(Value::as_str) {
        Some("context_compaction") => Some(CompactionDisposition::ProvenClientLocal),
        Some("compaction") => Some(if is_vellum_local_compaction(item) {
            CompactionDisposition::VellumJournal
        } else if is_provider_owned_opaque_compaction(item) {
            CompactionDisposition::ProviderOwnedOpaque
        } else if is_proven_client_local_compaction(item) {
            CompactionDisposition::ProvenClientLocal
        } else {
            CompactionDisposition::Unknown
        }),
        _ => None,
    }
}

/// True only for a *proven* Codex client-local marker. Non-Vellum
/// `type=compaction` without local-summary evidence is not guessed.
pub fn is_client_local_compaction_marker(item: &Value) -> bool {
    classify_compaction_item(item) == Some(CompactionDisposition::ProvenClientLocal)
}

/// Marker prefix written by the Codex Local Compact 0.150 engine.
///
/// Materialization parses this prefix and only this prefix. The Canonical
/// prefix below is recognized solely so a legacy task can be rejected by name
/// instead of being silently misread as an ordinary item.
pub const CODEX_LOCAL_V0_150_MARKER_PREFIX: &str = "vcompact.codex0150.";

/// Marker prefix written by the retired Canonical engine.
pub const LEGACY_CANONICAL_MARKER_PREFIX: &str = "vcomp1.";

pub fn is_vellum_local_compaction(item: &Value) -> bool {
    let opaque = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .unwrap_or("");
    if opaque.starts_with(LEGACY_CANONICAL_MARKER_PREFIX)
        || opaque.starts_with(CODEX_LOCAL_V0_150_MARKER_PREFIX)
    {
        return true;
    }
    item.get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| id.starts_with("cmp_vellum_") || id.starts_with("cmp_local_"))
}

/// True when the item carries a Codex Local Compact 0.150 marker.
pub fn is_codex_local_v0_150_marker(item: &Value) -> bool {
    item.get("encrypted_content")
        .and_then(Value::as_str)
        .is_some_and(|opaque| opaque.starts_with(CODEX_LOCAL_V0_150_MARKER_PREFIX))
}

/// True when the item carries a retired Canonical marker.
///
/// Such a task cannot be continued: the Canonical checkpoint schema it refers
/// to no longer exists, and dropping the marker to carry on would resume the
/// conversation from a history that never happened. Callers must surface
/// `legacy_canonical_compaction_unsupported` and require a new task.
pub fn is_legacy_canonical_marker(item: &Value) -> bool {
    item.get("encrypted_content")
        .and_then(Value::as_str)
        .is_some_and(|opaque| opaque.starts_with(LEGACY_CANONICAL_MARKER_PREFIX))
}

/// Stable error code for a task that still carries Canonical markers.
pub const LEGACY_CANONICAL_UNSUPPORTED: &str = "legacy_canonical_compaction_unsupported";

/// Reject a history that still carries retired Canonical markers.
pub fn reject_legacy_canonical_history(items: &[Value]) -> Result<(), &'static str> {
    if items.iter().any(is_legacy_canonical_marker) {
        return Err(LEGACY_CANONICAL_UNSUPPORTED);
    }
    Ok(())
}

fn is_provider_owned_opaque_compaction(item: &Value) -> bool {
    let opaque = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .unwrap_or("");
    !opaque.is_empty()
        && !opaque.starts_with(LEGACY_CANONICAL_MARKER_PREFIX)
        && !opaque.starts_with(CODEX_LOCAL_V0_150_MARKER_PREFIX)
}

fn is_proven_client_local_compaction(item: &Value) -> bool {
    if item.get("type").and_then(Value::as_str) == Some("context_compaction") {
        return true;
    }
    if is_vellum_local_compaction(item) || is_provider_owned_opaque_compaction(item) {
        return false;
    }
    embedded_compaction_summary(item).is_some()
}

/// Drop proven client-local markers and *verified* summary replacements only.
/// Ordinary user/developer/assistant messages after a marker are never
/// guessed to be summaries.
pub fn strip_client_local_compaction(items: &[Value]) -> (Vec<Value>, bool, Option<String>) {
    let mut out = Vec::with_capacity(items.len());
    let mut repaired = false;
    let mut discarded_summary: Option<String> = None;
    let mut skip_verified_replacement = false;
    for item in items {
        if skip_verified_replacement {
            skip_verified_replacement = false;
            if is_verified_summary_replacement(item) {
                if discarded_summary.is_none() {
                    discarded_summary = Some(item_plain_text(item));
                }
                repaired = true;
                continue;
            }
            out.push(item.clone());
            continue;
        }
        if is_client_local_compaction_marker(item) {
            repaired = true;
            if let Some(embedded) = embedded_compaction_summary(item) {
                discarded_summary = Some(embedded);
            }
            skip_verified_replacement = true;
            continue;
        }
        out.push(item.clone());
    }
    (out, repaired, discarded_summary)
}

fn embedded_compaction_summary(item: &Value) -> Option<String> {
    item.get("summary")
        .map(crate::adapter::content_to_plain_string)
        .filter(|text| !text.trim().is_empty())
        .or_else(|| {
            item.get("content")
                .map(crate::adapter::content_to_plain_string)
                .filter(|text| !text.trim().is_empty() && is_verified_summary_text(text))
        })
}

fn is_verified_summary_text(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with(crate::compaction::OFFICIAL_SUMMARY_PREFIX)
        || trimmed.starts_with(CHAT_CHECKPOINT_PREFACE)
        || trimmed.contains("This session is being continued from a previous conversation")
}

/// A replacement may be dropped only when it carries a known local-summary
/// shape. Regular messages must remain.
pub fn is_verified_summary_replacement(item: &Value) -> bool {
    if is_tool_call(item) || is_tool_output(item) {
        return false;
    }
    is_canonical_checkpoint_item(item)
        || is_summary_item(item)
        || is_verified_summary_text(&item_plain_text(item))
}

pub fn is_synthetic_bridge_text(text: &str) -> bool {
    text.trim() == NEUTRAL_USER_BRIDGE_TEXT
}

pub fn is_checkpoint_resume_text(text: &str) -> bool {
    text.trim() == CHAT_CHECKPOINT_RESUME_TEXT
}

/// A checkpoint as it appears on the Chat wire: assistant-owned and carrying
/// the preface `project_chat_items` writes.
pub fn is_chat_checkpoint_message(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("assistant")
        && crate::adapter::content_to_plain_string(message.get("content").unwrap_or(&Value::Null))
            .starts_with(CHAT_CHECKPOINT_PREFACE)
}

fn last_semantic_message(messages: &[Value]) -> Option<&Value> {
    messages.iter().rev().find(|message| {
        matches!(
            message.get("role").and_then(Value::as_str),
            Some("user" | "assistant" | "tool")
        )
    })
}

pub fn is_excluded_from_user_query(item: &Value) -> bool {
    is_canonical_checkpoint_item(item)
        || is_summary_item(item)
        || is_model_switch_marker(item)
        || is_client_local_compaction_marker(item)
        || is_synthetic_bridge_text(&item_plain_text(item))
        || is_checkpoint_resume_text(&item_plain_text(item))
        || item_plain_text(item).starts_with(CHAT_CHECKPOINT_PREFACE)
}

/// Last actionable user query inside the current suffix only.
pub fn current_suffix_user_query(items: &[Value], replay: &ReplayContext) -> Option<String> {
    replay.suffix_items(items).iter().rev().find_map(|item| {
        if is_excluded_from_user_query(item) {
            return None;
        }
        let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
        if role != "user" {
            return None;
        }
        let text = item_plain_text(item);
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn item_plain_text(item: &Value) -> String {
    item.get("content")
        .map(crate::adapter::content_to_plain_string)
        .unwrap_or_default()
}

pub fn user_turn_safety_hash(occurrence_id: &str) -> String {
    hash_text(occurrence_id)
}

/// Collect content-free diagnostics from a Chat messages array.
pub fn chat_transcript_diagnostics(
    messages: &[Value],
    replay: &ReplayContext,
) -> HarnessTranscriptDiagnostics {
    chat_transcript_diagnostics_with_provenance(messages, &[], replay)
}

pub fn chat_transcript_diagnostics_with_provenance(
    messages: &[Value],
    provenance: &[ChatMessageProvenance],
    replay: &ReplayContext,
) -> HarnessTranscriptDiagnostics {
    let mut diagnostics = HarnessTranscriptDiagnostics {
        replay_prefix_items: replay.prefix_len as u64,
        current_suffix_items: replay
            .suffix_end
            .saturating_sub(replay.suffix_start)
            .min(u64::MAX as usize) as u64,
        overlap_skips: replay.overlap_skipped as u64,
        compaction_repairs: replay.compaction_repairs as u64,
        ..HarnessTranscriptDiagnostics::default()
    };
    if diagnostics.current_suffix_items == u64::MAX {
        diagnostics.current_suffix_items = 0;
    }
    diagnostics.message_count = messages.len() as u64;
    diagnostics.role_sequence = role_sequence_summary(messages);
    diagnostics.first_role = messages
        .first()
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        .map(str::to_string);
    diagnostics.last_role = messages
        .last()
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let shape = chat_message_shape(messages);
    diagnostics.empty_messages = shape.empty_messages;
    diagnostics.tool_pairs = shape.tool_pairs;
    diagnostics.orphan_tool_results = shape.orphan_tool_results;
    diagnostics.duplicate_tool_ids = shape.duplicate_tool_ids;
    for (index, message) in messages.iter().enumerate() {
        match message.get("role").and_then(Value::as_str) {
            Some("user") => {
                diagnostics.user_messages += 1;
                let text = crate::adapter::content_to_plain_string(
                    message.get("content").unwrap_or(&Value::Null),
                );
                if is_synthetic_bridge_text(&text) {
                    diagnostics.neutral_bridges += 1;
                }
                if is_checkpoint_resume_text(&text) {
                    diagnostics.checkpoint_resumes += 1;
                }
                if let Some(occurrence_id) = provenance
                    .get(index)
                    .and_then(|item| item.occurrence_id.as_deref())
                {
                    diagnostics
                        .user_turn_hashes
                        .push(user_turn_safety_hash(occurrence_id));
                }
            }
            Some("assistant") => {
                diagnostics.assistant_messages += 1;
                if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                    diagnostics.tool_calls += calls.len() as u64;
                }
                let text = crate::adapter::content_to_plain_string(
                    message.get("content").unwrap_or(&Value::Null),
                );
                if text.starts_with(CHAT_CHECKPOINT_PREFACE) {
                    diagnostics.summaries += 1;
                }
            }
            Some("tool") => diagnostics.tool_messages += 1,
            Some("system") => diagnostics.system_messages += 1,
            _ => {}
        }
    }
    diagnostics
}

const ROLE_SEQUENCE_LIMIT: usize = 160;

#[derive(Debug, Clone, Copy, Default)]
struct ChatMessageShape {
    empty_messages: u64,
    tool_pairs: u64,
    orphan_tool_results: u64,
    duplicate_tool_ids: u64,
}

fn role_sequence_summary(messages: &[Value]) -> String {
    let mut summary = String::new();
    for (index, message) in messages.iter().enumerate() {
        if index > 0 {
            summary.push('>');
        }
        let role = message.get("role").and_then(Value::as_str).unwrap_or("?");
        summary.push_str(role);
        if summary.len() >= ROLE_SEQUENCE_LIMIT {
            summary.truncate(ROLE_SEQUENCE_LIMIT.saturating_sub(3));
            summary.push_str("...");
            break;
        }
    }
    summary
}

fn message_has_semantic_content(message: &Value) -> bool {
    let content = message
        .get("content")
        .map(crate::adapter::content_to_plain_string)
        .unwrap_or_default();
    !content.trim().is_empty()
        || message
            .get("reasoning_content")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.trim().is_empty())
        || message
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.trim().is_empty())
        || message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty())
}

fn chat_message_shape(messages: &[Value]) -> ChatMessageShape {
    let mut shape = ChatMessageShape::default();
    let mut open_calls = HashSet::new();
    let mut seen_calls = HashSet::new();
    for message in messages {
        if !message_has_semantic_content(message) {
            shape.empty_messages += 1;
        }
        match message.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let Some(id) = call.get("id").and_then(Value::as_str) else {
                            continue;
                        };
                        if !seen_calls.insert(id.to_string()) {
                            shape.duplicate_tool_ids += 1;
                        }
                        open_calls.insert(id.to_string());
                    }
                }
            }
            Some("tool") => {
                let Some(id) = message.get("tool_call_id").and_then(Value::as_str) else {
                    shape.orphan_tool_results += 1;
                    continue;
                };
                if open_calls.remove(id) {
                    shape.tool_pairs += 1;
                } else {
                    shape.orphan_tool_results += 1;
                }
            }
            _ => {}
        }
    }
    shape.orphan_tool_results += open_calls.len() as u64;
    shape
}

/// Fail closed when the Chat transcript violates pairing, duplicate active
/// user occurrence, multiple active checkpoints, or an unauthorized bridge.
pub fn check_chat_transcript_invariants(
    messages: &[Value],
    capabilities: &RuntimeChatCapabilities,
    replay: &ReplayContext,
) -> Result<(), String> {
    check_chat_transcript_invariants_with_provenance(messages, &[], capabilities, replay)
}

pub fn check_chat_transcript_invariants_with_provenance(
    messages: &[Value],
    provenance: &[ChatMessageProvenance],
    capabilities: &RuntimeChatCapabilities,
    _replay: &ReplayContext,
) -> Result<(), String> {
    check_chat_message_shape(messages)?;
    check_tool_pairing(messages)?;
    check_duplicate_active_user_occurrences(messages, provenance)?;
    check_active_checkpoint_count(messages)?;
    check_checkpoint_is_not_the_tail(messages)?;
    check_unauthorized_bridge(messages, capabilities)?;
    Ok(())
}

fn check_chat_message_shape(messages: &[Value]) -> Result<(), String> {
    if messages.is_empty() {
        return Err("chat shape: messages must not be empty".into());
    }
    let has_semantic_turn = messages.iter().any(|message| {
        matches!(
            message.get("role").and_then(Value::as_str),
            Some("user" | "tool")
        )
    });
    if !has_semantic_turn {
        return Err(
            "chat shape: messages must contain at least one semantic user or tool turn; system-only payloads are rejected"
                .into(),
        );
    }
    for (index, message) in messages.iter().enumerate() {
        if message.get("role").and_then(Value::as_str) == Some("assistant")
            && !message_has_semantic_content(message)
        {
            return Err(format!(
                "chat shape: empty assistant message at index {index}"
            ));
        }
    }
    Ok(())
}

fn check_tool_pairing(messages: &[Value]) -> Result<(), String> {
    let mut open_calls: HashMap<String, usize> = HashMap::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    for (index, message) in messages.iter().enumerate() {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "assistant" => {
                let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
                    continue;
                };
                if !open_calls.is_empty() {
                    return Err(format!(
                        "chat tool pairing: assistant tool calls at message {index} arrived before previous calls were closed"
                    ));
                }
                for call in calls {
                    let Some(id) = call.get("id").and_then(Value::as_str) else {
                        return Err("chat tool pairing: tool call missing id".into());
                    };
                    if !seen_ids.insert(id.to_string()) {
                        return Err(format!("chat tool pairing: duplicate tool call id `{id}`"));
                    }
                    open_calls.insert(id.to_string(), index);
                }
            }
            "tool" => {
                let Some(call_id) = message.get("tool_call_id").and_then(Value::as_str) else {
                    return Err(format!(
                        "chat tool pairing: tool message at {index} missing tool_call_id"
                    ));
                };
                if open_calls.remove(call_id).is_none() {
                    return Err(format!(
                        "chat tool pairing: orphan tool result for `{call_id}` at message {index}"
                    ));
                }
            }
            _ => {
                if !open_calls.is_empty() {
                    return Err(format!(
                        "chat tool pairing: non-tool message at {index} interrupted open tool calls"
                    ));
                }
            }
        }
    }
    if !open_calls.is_empty() {
        let ids = open_calls.keys().cloned().collect::<Vec<_>>().join(",");
        return Err(format!(
            "chat tool pairing: unresolved tool calls remain: {ids}"
        ));
    }
    Ok(())
}

fn check_duplicate_active_user_occurrences(
    messages: &[Value],
    provenance: &[ChatMessageProvenance],
) -> Result<(), String> {
    let mut seen: HashSet<String> = HashSet::new();
    for (index, message) in messages.iter().enumerate() {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let text =
            crate::adapter::content_to_plain_string(message.get("content").unwrap_or(&Value::Null));
        if is_synthetic_bridge_text(&text) || text.starts_with(CHAT_CHECKPOINT_PREFACE) {
            continue;
        }
        let Some(occurrence_id) = provenance
            .get(index)
            .and_then(|item| item.occurrence_id.clone())
        else {
            continue;
        };
        if !seen.insert(occurrence_id.clone()) {
            return Err(format!(
                "chat transcript: the same active user occurrence `{occurrence_id}` was serialized twice"
            ));
        }
    }
    Ok(())
}

fn check_active_checkpoint_count(messages: &[Value]) -> Result<(), String> {
    let checkpoints = messages
        .iter()
        .filter(|message| {
            let text = crate::adapter::content_to_plain_string(
                message.get("content").unwrap_or(&Value::Null),
            );
            text.starts_with(CHAT_CHECKPOINT_PREFACE)
                || text.starts_with(crate::compaction::OFFICIAL_SUMMARY_PREFIX)
        })
        .count();
    if checkpoints > 1 {
        return Err(format!(
            "chat transcript: expected at most one active checkpoint, found {checkpoints}"
        ));
    }
    Ok(())
}

/// A checkpoint is context, so it can never be the message the backend is
/// asked to continue from. Fail closed rather than send a transcript whose
/// tail asks the model to finish writing the summary it was handed.
fn check_checkpoint_is_not_the_tail(messages: &[Value]) -> Result<(), String> {
    if last_semantic_message(messages).is_some_and(is_chat_checkpoint_message) {
        return Err(
            "chat transcript: a conversation checkpoint must not be the last message".into(),
        );
    }
    Ok(())
}

fn check_unauthorized_bridge(
    messages: &[Value],
    capabilities: &RuntimeChatCapabilities,
) -> Result<(), String> {
    let bridges = messages
        .iter()
        .filter(|message| {
            message.get("role").and_then(Value::as_str) == Some("user")
                && is_synthetic_bridge_text(&crate::adapter::content_to_plain_string(
                    message.get("content").unwrap_or(&Value::Null),
                ))
        })
        .count();
    if bridges == 0 {
        return Ok(());
    }
    if capabilities.continuation_tail != ContinuationTail::NeutralUserBridge {
        return Err(
            "chat transcript: unauthorized neutral user bridge on a native-tool-result route"
                .into(),
        );
    }
    if bridges > 1 {
        return Err("chat transcript: more than one neutral user bridge".into());
    }
    Ok(())
}

/// Append a single fixed bridge when the capability requires a user tail and
/// the last semantic message is a tool result. Never copies the original
/// query. At most one bridge per request.
pub fn apply_neutral_user_bridge(
    messages: Vec<Value>,
    capabilities: &RuntimeChatCapabilities,
) -> Vec<Value> {
    let mut projection = ChatProjection::from_messages(messages);
    apply_neutral_user_bridge_to_projection(&mut projection, capabilities);
    projection.into_messages()
}

pub fn apply_neutral_user_bridge_to_projection(
    projection: &mut ChatProjection,
    capabilities: &RuntimeChatCapabilities,
) {
    if capabilities.continuation_tail != ContinuationTail::NeutralUserBridge {
        return;
    }
    if projection.messages.iter().any(|message| {
        message.get("role").and_then(Value::as_str) == Some("user")
            && is_synthetic_bridge_text(&crate::adapter::content_to_plain_string(
                message.get("content").unwrap_or(&Value::Null),
            ))
    }) {
        return;
    }
    let last_semantic = last_semantic_message(&projection.messages);
    let ends_on_tool = last_semantic
        .and_then(|message| message.get("role").and_then(Value::as_str))
        == Some("tool");
    if ends_on_tool {
        projection.push(
            json!({
                "role": "user",
                "content": NEUTRAL_USER_BRIDGE_TEXT
            }),
            ChatMessageProvenance {
                occurrence_id: Some("synthetic:bridge".into()),
                from_prefix: false,
            },
        );
    }
}

/// Append a single fixed continuation when the Chat transcript would otherwise
/// end on a compaction checkpoint.
///
/// Unlike the neutral bridge this is not route-conditional: an assistant tail
/// is a prefill on every Chat Completions backend, so a checkpoint tail is
/// never a shape any provider can be sent. The trigger is narrow on purpose —
/// only the checkpoint this projection itself wrote, never an ordinary
/// assistant turn — and the appended text is fixed, so the original user task
/// is never resent. At most one per request.
pub fn apply_checkpoint_resume_tail_to_projection(projection: &mut ChatProjection) {
    if projection.messages.iter().any(|message| {
        message.get("role").and_then(Value::as_str) == Some("user")
            && is_checkpoint_resume_text(&crate::adapter::content_to_plain_string(
                message.get("content").unwrap_or(&Value::Null),
            ))
    }) {
        return;
    }
    if !last_semantic_message(&projection.messages).is_some_and(is_chat_checkpoint_message) {
        return;
    }
    projection.push(
        json!({
            "role": "user",
            "content": CHAT_CHECKPOINT_RESUME_TEXT
        }),
        ChatMessageProvenance {
            occurrence_id: Some("synthetic:checkpoint-resume".into()),
            from_prefix: false,
        },
    );
}

pub fn should_replay_reasoning(
    capabilities: &RuntimeChatCapabilities,
    replay: &ReplayContext,
) -> bool {
    capabilities.reasoning_replay == ReasoningReplay::ToolCallBound
        && replay.allow_reasoning_replay
        && replay.same_route
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryEntry;
    use serde_json::json;

    fn entry(id: &str, input: Vec<Value>, output: Vec<Value>) -> HistoryEntry {
        HistoryEntry {
            response_id: id.into(),
            previous_response_id: None,
            route_id: "r".into(),
            continuation_realm: None,
            input_items: input,
            output_items: output,
            created_at: 1,
        }
    }

    #[test]
    fn hydrate_prepends_chain_and_keeps_current_suffix() {
        let chain = [entry(
            "resp_1",
            vec![json!({"role": "user", "content": "first"})],
            vec![json!({
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": "ok"
            })],
        )];
        let mut body = json!({
            "previous_response_id": "resp_1",
            "input": [{"role": "user", "content": "second"}]
        });
        let outcome =
            hydrate_input_with_mode(&mut body, &chain, ContinuationMode::PortableSemanticReplay);
        assert!(body.get("previous_response_id").is_none());
        assert_eq!(outcome.replayed_prefix_len, 2);
        assert_eq!(outcome.current_suffix_range, 2..3);
        assert!(!outcome.client_carried_full_history);
        assert_eq!(outcome.overlap_skipped, 0);
        assert_eq!(body["input"].as_array().unwrap().len(), 3);
        assert_eq!(body["input"][2]["content"], "second");
        assert_eq!(
            outcome.item_identities,
            vec![
                stored_item_identity("resp_1", "input", 0),
                stored_item_identity("resp_1", "output", 0),
                request_item_identity(0),
            ]
        );
    }

    #[test]
    fn hydrate_identities_are_source_stable_not_output_index() {
        let chain = [entry(
            "resp_1",
            vec![json!({"role": "user", "content": "same text"})],
            vec![json!({"role": "assistant", "content": "ok"})],
        )];
        let mut body = json!({
            "previous_response_id": "resp_1",
            "input": [{"role": "user", "content": "same text"}]
        });
        let outcome =
            hydrate_input_with_mode(&mut body, &chain, ContinuationMode::PortableSemanticReplay);
        assert_eq!(
            outcome.item_identities[0],
            stored_item_identity("resp_1", "input", 0)
        );
        assert_eq!(outcome.item_identities[2], request_item_identity(0));
        assert_ne!(outcome.item_identities[0], outcome.item_identities[2]);
        let replay = outcome.replay_context();
        let first = replay.provenance_for_item(0);
        let cloned = first.clone();
        let messages = vec![
            json!({"role": "user", "content": "same text"}),
            json!({"role": "assistant", "content": "ok"}),
            json!({"role": "user", "content": "same text"}),
            json!({"role": "user", "content": "same text"}),
        ];
        let provenance = vec![
            first,
            replay.provenance_for_item(1),
            replay.provenance_for_item(2),
            cloned,
        ];
        let error = check_chat_transcript_invariants_with_provenance(
            &messages,
            &provenance,
            &RuntimeChatCapabilities::default(),
            &replay,
        )
        .unwrap_err();
        assert!(error.contains("store:resp_1:input:0"), "{error}");
    }

    #[test]
    fn identical_user_content_does_not_skip_prepend() {
        let chain = [entry(
            "resp_1",
            vec![json!({"role": "user", "content": "same text"})],
            vec![json!({
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": "ok"
            })],
        )];
        let mut body = json!({
            "previous_response_id": "resp_1",
            "input": [{"role": "user", "content": "same text"}]
        });
        let outcome =
            hydrate_input_with_mode(&mut body, &chain, ContinuationMode::PortableSemanticReplay);
        assert_eq!(outcome.overlap_skipped, 0);
        assert_eq!(body["input"].as_array().unwrap().len(), 3);
        assert_eq!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item.get("role").and_then(Value::as_str) == Some("user"))
                .count(),
            2
        );
    }

    #[test]
    fn full_history_with_provider_id_skips_duplicate_prepend() {
        let assistant = json!({
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": "ok"
        });
        let user = json!({"role": "user", "content": "first"});
        let chain = [entry("resp_1", vec![user.clone()], vec![assistant.clone()])];
        let mut body = json!({
            "previous_response_id": "resp_1",
            "input": [user, assistant, {"role": "user", "content": "second"}]
        });
        let outcome =
            hydrate_input_with_mode(&mut body, &chain, ContinuationMode::PortableSemanticReplay);
        assert!(outcome.client_carried_full_history);
        assert_eq!(outcome.overlap_skipped, 2);
        assert_eq!(outcome.replayed_prefix_len, 2);
        assert_eq!(outcome.current_suffix_range, 2..3);
        assert_eq!(body["input"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn current_suffix_user_query_ignores_checkpoint_and_bridge() {
        let items = vec![
            json!({"role": "user", "content": "historical task"}),
            json!({
                "role": "user",
                "content": format!("{}\nsummary", crate::compaction::OFFICIAL_SUMMARY_PREFIX),
                "metadata": {"vellum_checkpoint": "canonical"}
            }),
            json!({"role": "user", "content": NEUTRAL_USER_BRIDGE_TEXT}),
            json!({"role": "user", "content": "new question"}),
        ];
        let replay = ReplayContext {
            prefix_len: 1,
            suffix_start: 1,
            suffix_end: 4,
            ..ReplayContext::default()
        };
        assert_eq!(
            current_suffix_user_query(&items, &replay).as_deref(),
            Some("new question")
        );
    }

    #[test]
    fn chat_shape_rejects_empty_and_system_only_payloads() {
        for messages in [
            Vec::new(),
            vec![json!({"role": "system", "content": "instructions"})],
            vec![
                json!({"role": "system", "content": "instructions"}),
                json!({"role": "assistant", "content": ""}),
            ],
        ] {
            let error = check_chat_transcript_invariants(
                &messages,
                &RuntimeChatCapabilities::default(),
                &ReplayContext::default(),
            )
            .unwrap_err();
            assert!(error.contains("chat shape"), "{error}");
        }
    }

    #[test]
    fn chat_shape_rejects_empty_assistant_and_interleaved_tool_windows() {
        let empty_assistant = vec![
            json!({"role": "user", "content": "task"}),
            json!({"role": "assistant", "content": ""}),
        ];
        let error = check_chat_transcript_invariants(
            &empty_assistant,
            &RuntimeChatCapabilities::default(),
            &ReplayContext::default(),
        )
        .unwrap_err();
        assert!(error.contains("empty assistant"), "{error}");

        let interleaved = vec![
            json!({"role": "user", "content": "task"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "x", "arguments": "{}"}}
            ]}),
            json!({"role": "user", "content": "inserted"}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "ok"}),
        ];
        let error = check_chat_transcript_invariants(
            &interleaved,
            &RuntimeChatCapabilities::default(),
            &ReplayContext::default(),
        )
        .unwrap_err();
        assert!(error.contains("interrupted"), "{error}");
    }

    #[test]
    fn chat_shape_rejects_duplicate_and_orphan_tool_ids() {
        let duplicate = vec![
            json!({"role": "user", "content": "task"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "x", "arguments": "{}"}},
                {"id": "c1", "type": "function", "function": {"name": "y", "arguments": "{}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "ok"}),
        ];
        let error = check_chat_transcript_invariants(
            &duplicate,
            &RuntimeChatCapabilities::default(),
            &ReplayContext::default(),
        )
        .unwrap_err();
        assert!(error.contains("duplicate tool call id"), "{error}");

        let orphan = vec![
            json!({"role": "user", "content": "task"}),
            json!({"role": "tool", "tool_call_id": "missing", "content": "ok"}),
        ];
        let error = check_chat_transcript_invariants(
            &orphan,
            &RuntimeChatCapabilities::default(),
            &ReplayContext::default(),
        )
        .unwrap_err();
        assert!(error.contains("orphan tool result"), "{error}");
    }

    #[test]
    fn chat_diagnostics_are_content_free_and_count_tool_pairs() {
        let messages = vec![
            json!({"role": "system", "content": "private prompt"}),
            json!({"role": "user", "content": "private query"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "x", "arguments": "secret"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "c1", "content": "private result"}),
        ];
        let diagnostics = chat_transcript_diagnostics(&messages, &ReplayContext::default());
        assert_eq!(diagnostics.message_count, 4);
        assert_eq!(diagnostics.role_sequence, "system>user>assistant>tool");
        assert_eq!(diagnostics.tool_pairs, 1);
        assert_eq!(diagnostics.empty_messages, 0);
        assert!(!serde_json::to_string(&diagnostics)
            .unwrap()
            .contains("private"));
    }

    #[test]
    fn invariant_rejects_the_same_occurrence_projected_twice() {
        let messages = vec![
            json!({"role": "user", "content": "do the task"}),
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "x", "arguments": "{}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "ok"}),
            json!({"role": "user", "content": "do the task"}),
        ];
        let provenance = vec![
            ChatMessageProvenance {
                occurrence_id: Some("store:resp_1:input:0".into()),
                from_prefix: true,
            },
            ChatMessageProvenance {
                occurrence_id: Some("store:resp_1:output:0".into()),
                from_prefix: true,
            },
            ChatMessageProvenance {
                occurrence_id: Some("store:resp_1:output:1".into()),
                from_prefix: true,
            },
            ChatMessageProvenance {
                occurrence_id: Some("store:resp_1:input:0".into()),
                from_prefix: true,
            },
        ];
        let error = check_chat_transcript_invariants_with_provenance(
            &messages,
            &provenance,
            &RuntimeChatCapabilities::default(),
            &ReplayContext::default(),
        )
        .unwrap_err();
        assert!(error.contains("store:resp_1:input:0"), "{error}");
    }

    #[test]
    fn invariant_keeps_two_real_user_turns_with_the_same_text() {
        let messages = vec![
            json!({"role": "user", "content": "same text"}),
            json!({"role": "assistant", "content": "ack"}),
            json!({"role": "user", "content": "same text"}),
        ];
        let provenance = vec![
            ChatMessageProvenance {
                occurrence_id: Some("store:resp_1:input:0".into()),
                from_prefix: true,
            },
            ChatMessageProvenance {
                occurrence_id: Some("store:resp_1:output:0".into()),
                from_prefix: true,
            },
            ChatMessageProvenance {
                occurrence_id: Some("req:0".into()),
                from_prefix: false,
            },
        ];
        check_chat_transcript_invariants_with_provenance(
            &messages,
            &provenance,
            &RuntimeChatCapabilities::default(),
            &ReplayContext::default(),
        )
        .unwrap();
    }

    #[test]
    fn invariant_rejects_unauthorized_bridge() {
        let messages = vec![
            json!({"role": "user", "content": "task"}),
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "x", "arguments": "{}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "ok"}),
            json!({"role": "user", "content": NEUTRAL_USER_BRIDGE_TEXT}),
        ];
        let error = check_chat_transcript_invariants(
            &messages,
            &RuntimeChatCapabilities::default(),
            &ReplayContext::default(),
        )
        .unwrap_err();
        assert!(error.contains("unauthorized"), "{error}");
    }

    #[test]
    fn authorized_bridge_is_exactly_one_fixed_string() {
        let messages = vec![
            json!({"role": "user", "content": "task"}),
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "x", "arguments": "{}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "ok"}),
        ];
        let bridged = apply_neutral_user_bridge(messages, &RuntimeChatCapabilities::qwen_bridge());
        assert_eq!(bridged.last().unwrap()["content"], NEUTRAL_USER_BRIDGE_TEXT);
        assert_eq!(
            bridged
                .iter()
                .filter(|message| message["content"] == NEUTRAL_USER_BRIDGE_TEXT)
                .count(),
            1
        );
        assert!(
            !serde_json::to_string(&bridged).unwrap().contains("task")
                || bridged
                    .iter()
                    .filter(|m| m["role"] == "user" && m["content"] == "task")
                    .count()
                    == 1
        );
        check_chat_transcript_invariants(
            &bridged,
            &RuntimeChatCapabilities::qwen_bridge(),
            &ReplayContext::default(),
        )
        .unwrap();
    }

    #[test]
    fn default_capabilities_keep_tool_tail() {
        let messages = vec![
            json!({"role": "user", "content": "task"}),
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "x", "arguments": "{}"}}]
            }),
            json!({"role": "tool", "tool_call_id": "c1", "content": "ok"}),
        ];
        let out = apply_neutral_user_bridge(messages.clone(), &RuntimeChatCapabilities::default());
        assert_eq!(out, messages);
        assert_eq!(out.last().unwrap()["role"], "tool");
    }

    #[test]
    fn strip_client_local_compaction_keeps_ordinary_messages_after_marker() {
        let items = vec![
            json!({"type": "context_compaction", "id": "cc_1"}),
            json!({"role": "developer", "content": "keep this developer note"}),
            json!({"role": "user", "content": "continue the task"}),
        ];
        let (kept, repaired, discarded) = strip_client_local_compaction(&items);
        assert!(repaired);
        assert!(discarded.is_none());
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0]["content"], "keep this developer note");
        assert_eq!(kept[1]["content"], "continue the task");
    }

    #[test]
    fn strip_client_local_compaction_drops_verified_summary_replacement() {
        let items = vec![
            json!({
                "type": "context_compaction",
                "id": "cc_1",
                "summary": "embedded client summary"
            }),
            json!({
                "role": "user",
                "content": format!("{}\nkeep going", crate::compaction::OFFICIAL_SUMMARY_PREFIX),
                "metadata": {"vellum_checkpoint": "canonical"}
            }),
            json!({"role": "user", "content": "continue the task"}),
        ];
        let (kept, repaired, discarded) = strip_client_local_compaction(&items);
        assert!(repaired);
        assert_eq!(discarded.as_deref(), Some("embedded client summary"));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0]["content"], "continue the task");
        assert!(!serde_json::to_string(&kept)
            .unwrap()
            .contains("embedded client summary"));
    }

    #[test]
    fn ten_tool_continuations_keep_one_user_turn() {
        let mut input = vec![json!({"role": "user", "content": "original task"})];
        for i in 0..10 {
            let id = format!("call_{i}");
            input.push(json!({
                "type": "function_call",
                "call_id": id,
                "name": "shell_command",
                "arguments": "{}"
            }));
            input.push(json!({
                "type": "function_call_output",
                "call_id": format!("call_{i}"),
                "output": format!("result {i}")
            }));
        }
        let converted = crate::profile_adapter::responses_to_chat(&json!({
            "model": "laguna-mini",
            "input": input
        }))
        .unwrap();
        let messages = converted["messages"].as_array().unwrap();
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["role"] == "user")
                .count(),
            1
        );
        assert_eq!(messages.last().unwrap()["role"], "tool");
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["role"] == "tool")
                .count(),
            10
        );
    }
}
