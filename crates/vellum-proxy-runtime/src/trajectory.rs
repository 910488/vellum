//! Vellum Canonical v2 trajectory analysis (spec sections 5–8).
//!
//! Pure functions over portable history items: normalize items into
//! fingerprint-bearing [`TrajectoryEvent`]s, pair them into [`ToolExchange`]s
//! via the shared compaction pairing semantics, detect repeated exchange /
//! sequence loop patterns, and turn the resulting [`TrajectoryAnalysis`] into
//! a loop-guard decision. Events never retain raw prompt or tool-output text.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::compaction::{
    estimate_json_tokens, is_canonical_checkpoint_item, is_model_switch_marker, is_tool_call,
    is_tool_output,
};

/// Default lower bound for repeated-sequence detection.
const DEFAULT_MIN_SEQUENCE_LEN: usize = 2;

/// Stable `"sha256:<hex>"` identity for a fingerprint payload.
fn sha256_hex(payload: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(payload.as_bytes()))
}

/// Error returned by trajectory normalization. Provenance mismatches are
/// surfaced instead of being masked with synthesized identities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrajectoryError {
    /// `items.len() != identities.len()`; no fallback identity may be invented.
    IdentityMismatch {
        /// Number of source items.
        item_count: usize,
        /// Number of supplied occurrence identities.
        identity_count: usize,
    },
}

impl std::fmt::Display for TrajectoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrajectoryError::IdentityMismatch {
                item_count,
                identity_count,
            } => write!(
                f,
                "trajectory identity mismatch: {item_count} items but {identity_count} identities"
            ),
        }
    }
}

impl std::error::Error for TrajectoryError {}

/// Kind of a normalized trajectory event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrajectoryEventKind {
    /// User-visible message item.
    UserMessage,
    /// Assistant message item.
    AssistantMessage,
    /// Tool invocation item.
    ToolCall,
    /// Tool result item.
    ToolOutput,
    /// Developer/system `<model_switch>` marker.
    ModelSwitch,
    /// Vellum canonical compaction checkpoint.
    CanonicalCheckpoint,
    /// Anything else.
    Other,
}

/// Normalized per-item trajectory event. Stores metadata and fingerprints
/// only; raw prompt/output text, credentials, and provider-private reasoning
/// are never retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrajectoryEvent {
    /// Index into the source item slice.
    pub source_index: usize,

    /// Occurrence identity from `ReplayContext.item_identities`.
    pub occurrence_id: String,

    pub kind: TrajectoryEventKind,

    pub role: Option<String>,

    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,

    pub tool_call_fingerprint: Option<String>,
    pub tool_output_fingerprint: Option<String>,

    pub estimated_tokens: u64,
}

/// One paired tool invocation plus its outputs, in source order.
#[derive(Debug, Clone)]
pub struct ToolExchange {
    /// Source index of the tool call item.
    pub call_source_index: usize,
    /// Source indices of every output inside the exchange span.
    pub output_source_indices: Vec<usize>,

    pub call_id: Option<String>,
    pub tool_name: String,

    pub call_fingerprint: String,
    pub output_fingerprint: Option<String>,

    pub completed: bool,
}

/// Confidence attached to a detected repetition pattern. Only `High`
/// patterns drive loop-guard intervention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopConfidence {
    Low,
    Medium,
    High,
}

/// A detected repetition pattern over tool exchanges.
#[derive(Debug, Clone)]
pub struct LoopPattern {
    /// Stable sha256 identity over the repeated fingerprint sequence.
    pub pattern_hash: String,

    /// Number of exchanges in one period of the repetition.
    pub sequence_length: usize,
    /// Number of consecutive repetitions observed.
    pub repeat_count: usize,

    /// Inclusive exchange index bounds of the observed run.
    pub first_exchange_index: usize,
    pub last_exchange_index: usize,

    /// True when every output fingerprint inside one period is equal.
    pub identical_outputs: bool,

    pub confidence: LoopConfidence,
}

/// Aggregate result of trajectory analysis.
#[derive(Debug, Clone, Default)]
pub struct TrajectoryAnalysis {
    pub events: Vec<TrajectoryEvent>,
    pub exchanges: Vec<ToolExchange>,
    pub repeated_exchanges: Vec<LoopPattern>,
    pub repeated_sequences: Vec<LoopPattern>,
    pub repeated_investigations: Option<LoopPattern>,
    /// Historical maximum run of consecutive ToolCall/ToolOutput events (for telemetry).
    pub max_tool_only_event_streak: usize,
    /// Current suffix run of consecutive ToolCall/ToolOutput events.
    pub current_tool_only_event_streak: usize,
    /// Current suffix run of consecutive ToolExchanges without intervening user/assistant messages.
    pub current_tool_exchange_streak: usize,
    /// Backward-compatible alias for current tool exchange streak.
    pub tool_only_streak: usize,
    /// Largest repeat count across all detected patterns.
    pub max_repeat_count: usize,
}

/// Tunables for the loop guard (spec 8.1).
#[derive(Debug, Clone)]
pub struct LoopGuardPolicy {
    /// Repeats of one identical exchange before it becomes a High loop.
    pub same_exchange_repeat_threshold: usize,
    /// Repeats of a repeated block before it becomes a High loop.
    pub sequence_repeat_threshold: usize,
    /// Upper bound on the block length considered for sequence detection.
    pub max_sequence_len: usize,

    /// Number of recovery injections allowed before aborting.
    pub recovery_attempts: u8,

    /// Hard cap on consecutive tool-only events regardless of fingerprints.
    pub absolute_tool_only_limit: usize,
}

impl Default for LoopGuardPolicy {
    fn default() -> Self {
        Self {
            same_exchange_repeat_threshold: 3,
            sequence_repeat_threshold: 3,
            max_sequence_len: 8,
            recovery_attempts: 1,
            absolute_tool_only_limit: 24,
        }
    }
}

/// Outcome of the loop guard for a turn.
#[derive(Debug, Clone, PartialEq)]
pub enum LoopGuardDecision {
    /// Proceed normally.
    Allow,
    /// Inject a recovery message describing how to break the loop.
    Recover {
        /// Hash of the offending pattern.
        pattern_hash: String,
        /// Recovery guidance injected into the next request.
        message: String,
    },
    /// The loop survived recovery. Disable tools for one last model turn so
    /// it can report the bounded failure to the user instead of disappearing
    /// behind a transport error.
    Finalize {
        /// Hash of the offending pattern.
        pattern_hash: String,
        /// Finalization guidance injected into the tool-disabled request.
        message: String,
    },
    /// Stop the session; the loop survived recovery attempts.
    Abort {
        /// Stable machine-readable failure category.
        error_code: &'static str,
        /// Bounded human-facing diagnostic.
        message: String,
    },
}

/// Error code used when the loop guard aborts a session.
pub const TOOL_LOOP_LIMIT_ERROR_CODE: &str = "vellum_tool_loop_limit";

const RECOVERY_MESSAGE: &str = "The last tool sequence repeated three times with identical \
results and produced no new information.\n\n\
Do not repeat these tool calls.\n\n\
Summarize what has already been learned and either:\n\
1. choose a materially different action,\n\
2. modify the implementation,\n\
3. run a different verification, or\n\
4. finish the task if the investigation is complete.";

const FINALIZATION_MESSAGE: &str = "The same tool sequence persisted after a recovery warning, so \
Vellum has disabled all tools for this final response. Do not claim the requested work succeeded. \
Briefly tell the user what was established, why the repeated tool path could not complete the \
request, and the smallest concrete action needed to unblock or retry it. Return a final text \
response now; do not request or simulate another tool call.";

fn tool_pair_identity(item: &Value) -> Option<String> {
    item.get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn classify_event_kind(item: &Value) -> TrajectoryEventKind {
    let role = item.get("role").and_then(Value::as_str);
    if role == Some("user") {
        TrajectoryEventKind::UserMessage
    } else if role == Some("assistant") {
        TrajectoryEventKind::AssistantMessage
    } else if is_tool_call(item) {
        TrajectoryEventKind::ToolCall
    } else if is_tool_output(item) {
        TrajectoryEventKind::ToolOutput
    } else if is_model_switch_marker(item) {
        TrajectoryEventKind::ModelSwitch
    } else if is_canonical_checkpoint_item(item) {
        TrajectoryEventKind::CanonicalCheckpoint
    } else {
        TrajectoryEventKind::Other
    }
}

/// Normalize portable history items into one fingerprint-bearing event per
/// item. Every event takes its occurrence id from `identities`; a length
/// mismatch is an error, never silently repaired.
pub fn normalize_trajectory(
    items: &[Value],
    identities: &[String],
) -> Result<Vec<TrajectoryEvent>, TrajectoryError> {
    if items.len() != identities.len() {
        return Err(TrajectoryError::IdentityMismatch {
            item_count: items.len(),
            identity_count: identities.len(),
        });
    }
    Ok(items
        .iter()
        .zip(identities)
        .enumerate()
        .map(|(source_index, (item, identity))| TrajectoryEvent {
            source_index,
            occurrence_id: identity.clone(),
            kind: classify_event_kind(item),
            role: item.get("role").and_then(Value::as_str).map(str::to_string),
            tool_call_id: tool_pair_identity(item),
            tool_name: item.get("name").and_then(Value::as_str).map(str::to_string),
            tool_call_fingerprint: tool_call_fingerprint(item),
            tool_output_fingerprint: tool_output_fingerprint(item),
            estimated_tokens: estimate_json_tokens(item),
        })
        .collect())
}

/// Replace CRLF with LF and trim trailing whitespace per line inside a string.
fn normalize_string_text(raw: &str) -> String {
    let unified = raw.replace("\r\n", "\n");
    let mut out = String::with_capacity(unified.len());
    for (index, line) in unified.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end_matches([' ', '\t']));
    }
    out
}

/// Remove ANSI CSI (`ESC[...final`) and OSC (`ESC]...(BEL | ESC\)`) sequences.
fn strip_ansi_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(current) = chars.next() {
        if current != '\x1b' {
            out.push(current);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(next) = chars.next() {
                    if next == '\x07' {
                        break;
                    }
                    if next == '\x1b' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Recursively sort object keys and normalize whitespace inside strings for
/// stable fingerprinting. Never drops keys or alters non-string values.
pub fn canonicalize_json_for_fingerprint(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(canonicalize_json_for_fingerprint)
                .collect(),
        ),
        Value::Object(object) => {
            let sorted: BTreeMap<&str, &Value> = object
                .iter()
                .map(|(key, val)| (key.as_str(), val))
                .collect();
            let mut canonical = Map::new();
            for (key, val) in sorted {
                canonical.insert(key.to_string(), canonicalize_json_for_fingerprint(val));
            }
            Value::Object(canonical)
        }
        Value::String(text) => Value::String(normalize_string_text(text)),
        other => other.clone(),
    }
}

/// Fingerprint a tool call item as `SHA256(name + "\n" + canonical_args)`.
/// Returns `None` for anything that is not a tool call.
pub fn tool_call_fingerprint(item: &Value) -> Option<String> {
    if !is_tool_call(item) {
        return None;
    }
    let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
    let arguments = match item.get("arguments") {
        Some(Value::String(raw)) => {
            serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.clone()))
        }
        Some(value) => value.clone(),
        None => serde_json::json!({}),
    };
    let canonical = canonicalize_json_for_fingerprint(&arguments);
    Some(sha256_hex(&format!("{name}\n{canonical}")))
}

/// Apply CRLF/ANSI normalization to every string leaf so the normalization
/// survives JSON serialization (control characters would otherwise be
/// escaped before the textual normalizers could see them).
fn normalize_payload_strings(value: &mut Value) {
    match value {
        Value::String(text) => *text = normalize_string_text(&strip_ansi_escapes(text)),
        Value::Array(values) => values.iter_mut().for_each(normalize_payload_strings),
        Value::Object(map) => map.values_mut().for_each(normalize_payload_strings),
        _ => {}
    }
}

/// Fingerprint a tool output item after CRLF, ANSI, whitespace, and structured
/// field normalization. Retains structured status/exit codes so status transitions
/// (e.g. exit 1 -> exit 0) alter the fingerprint. Returns `None` for non-outputs.
pub fn tool_output_fingerprint(item: &Value) -> Option<String> {
    if !is_tool_output(item) {
        return None;
    }
    let mut reduced = item.clone();
    if let Value::Object(map) = &mut reduced {
        map.remove("type");
        map.remove("call_id");
        map.remove("id");
    }
    normalize_payload_strings(&mut reduced);
    let canonical = canonicalize_json_for_fingerprint(&reduced);
    let serialized = serde_json::to_string(&canonical).unwrap_or_default();
    Some(sha256_hex(&serialized))
}

/// Pair tool calls with their outputs using occurrence-scoped matching, correctly
/// handling parallel tool calls, multi-output responses, and duplicate call_ids.
pub fn build_tool_exchanges(items: &[Value], _events: &[TrajectoryEvent]) -> Vec<ToolExchange> {
    occurrence_tool_exchanges(items)
}

/// Occurrence-scoped tool exchange pairing.
pub fn occurrence_tool_exchanges(items: &[Value]) -> Vec<ToolExchange> {
    let mut exchanges = Vec::new();
    let n = items.len();

    for i in 0..n {
        if !is_tool_call(&items[i]) {
            continue;
        }
        let call_item = &items[i];
        let call_id = tool_pair_identity(call_item);

        let mut output_indices = Vec::new();
        for (j, item) in items.iter().enumerate().take(n).skip(i + 1) {
            if is_tool_call(item) {
                // If another call with the same ID appears, stop matching outputs for this occurrence
                if let Some(ref cid) = call_id {
                    if tool_pair_identity(item).as_deref() == Some(cid.as_str()) {
                        break;
                    }
                }
                continue;
            }
            if is_tool_output(item) {
                let out_id = tool_pair_identity(item);
                if let Some(ref cid) = call_id {
                    if out_id.as_deref() == Some(cid.as_str()) {
                        output_indices.push(j);
                    }
                } else if out_id.is_none() {
                    output_indices.push(j);
                    // For un-identified tools, take the immediate output
                    break;
                }
            }
        }

        let fps: Vec<String> = output_indices
            .iter()
            .filter_map(|&idx| tool_output_fingerprint(&items[idx]))
            .collect();
        let output_fingerprint = if fps.is_empty() {
            None
        } else if fps.len() == 1 {
            Some(fps[0].clone())
        } else {
            Some(sha256_hex(&fps.join("\n")))
        };
        let completed = !output_indices.is_empty();

        exchanges.push(ToolExchange {
            call_source_index: i,
            output_source_indices: output_indices,
            call_id,
            tool_name: call_item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string(),
            call_fingerprint: tool_call_fingerprint(call_item).unwrap_or_else(|| sha256_hex("{}")),
            output_fingerprint,
            completed,
        });
    }

    exchanges
}

/// Authoritative validation report on tool call and output pairing in an item sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPairingReport {
    /// Total tool calls found.
    pub total_calls: usize,
    /// Total tool outputs found.
    pub total_outputs: usize,
    /// Source indices of tool outputs that do not match any previous tool call occurrence.
    pub orphan_outputs: Vec<usize>,
    /// Source indices of tool calls that have no matching output.
    pub incomplete_calls: Vec<usize>,
}

/// Perform authoritative occurrence-scoped pairing analysis on a sequence of items.
pub fn analyze_tool_pairing(items: &[Value]) -> ToolPairingReport {
    let exchanges = occurrence_tool_exchanges(items);
    let mut matched_outputs = std::collections::HashSet::new();
    let mut incomplete_calls = Vec::new();

    for ex in &exchanges {
        if ex.output_source_indices.is_empty() {
            incomplete_calls.push(ex.call_source_index);
        } else {
            for &out_idx in &ex.output_source_indices {
                matched_outputs.insert(out_idx);
            }
        }
    }

    let mut orphan_outputs = Vec::new();
    let mut total_outputs = 0;
    let mut total_calls = 0;

    for (idx, item) in items.iter().enumerate() {
        if is_tool_call(item) {
            total_calls += 1;
        } else if is_tool_output(item) {
            total_outputs += 1;
            if !matched_outputs.contains(&idx) {
                orphan_outputs.push(idx);
            }
        }
    }

    ToolPairingReport {
        total_calls,
        total_outputs,
        orphan_outputs,
        incomplete_calls,
    }
}

/// Combined call + output identity for sequence comparison.
fn exchange_unit_fingerprint(exchange: &ToolExchange) -> String {
    format!(
        "{}\u{1f}{}",
        exchange.call_fingerprint,
        exchange.output_fingerprint.as_deref().unwrap_or("-")
    )
}

fn sequence_pattern_hash(exchanges: &[ToolExchange]) -> String {
    let joined = exchanges
        .iter()
        .map(exchange_unit_fingerprint)
        .collect::<Vec<_>>()
        .join("\n");
    sha256_hex(&joined)
}

/// Detect runs of consecutive identical completed exchanges (same call
/// fingerprint, equal `Some` output fingerprint). Returns the largest (last
/// such) run once it reaches `min_repeats`; only High confidence results.
pub fn detect_repeated_exchange(
    exchanges: &[ToolExchange],
    min_repeats: usize,
) -> Option<LoopPattern> {
    let mut best: Option<(usize, usize, usize)> = None;
    let mut index = 0usize;
    while index < exchanges.len() {
        let Some(output_fp) = exchanges[index].output_fingerprint.clone() else {
            index += 1;
            continue;
        };
        let call_fp = exchanges[index].call_fingerprint.as_str();
        let mut end = index + 1;
        while end < exchanges.len()
            && exchanges[end].call_fingerprint == call_fp
            && exchanges[end].output_fingerprint.as_deref() == Some(output_fp.as_str())
        {
            end += 1;
        }
        let count = end - index;
        let replaces = best.is_none_or(|(top, _, _)| count >= top);
        if replaces {
            best = Some((count, index, end - 1));
        }
        index = end;
    }
    best.and_then(|(repeat_count, first, last)| {
        (repeat_count >= min_repeats).then(|| LoopPattern {
            pattern_hash: sequence_pattern_hash(&exchanges[first..first + 1]),
            sequence_length: 1,
            repeat_count,
            first_exchange_index: first,
            last_exchange_index: last,
            identical_outputs: true,
            confidence: LoopConfidence::High,
        })
    })
}

/// Detect consecutive repeated blocks of exchange fingerprints, longest-first
/// within `[min_sequence_len, max_sequence_len]`. Blocks repeating at least
/// `min_repeats` times yield High confidence when their exchanges are completed with outputs.
pub fn detect_repeated_sequence(
    exchanges: &[ToolExchange],
    min_sequence_len: usize,
    max_sequence_len: usize,
    min_repeats: usize,
) -> Vec<LoopPattern> {
    let total = exchanges.len();
    let units: Vec<String> = exchanges.iter().map(exchange_unit_fingerprint).collect();
    let min_len = min_sequence_len.max(1);
    let max_len = max_sequence_len.max(min_len);
    let mut patterns = Vec::new();
    let mut covered: Vec<(usize, usize)> = Vec::new();

    for length in (min_len..=max_len).rev() {
        let mut start = 0usize;
        while start + length <= total {
            if covered
                .iter()
                .any(|&(begin, stop)| start > begin && start < stop)
            {
                start += 1;
                continue;
            }
            let block = &units[start..start + length];
            let mut repeats = 1usize;
            while start + (repeats + 1) * length <= total
                && units[start + repeats * length..start + (repeats + 1) * length] == *block
            {
                repeats += 1;
            }
            if repeats >= 2 && repeats >= min_repeats {
                let end = start + repeats * length;
                let all_have_outputs = exchanges[start..start + length]
                    .iter()
                    .all(|exchange| exchange.output_fingerprint.is_some() && exchange.completed);
                patterns.push(LoopPattern {
                    pattern_hash: sequence_pattern_hash(&exchanges[start..start + length]),
                    sequence_length: length,
                    repeat_count: repeats,
                    first_exchange_index: start,
                    last_exchange_index: end - 1,
                    identical_outputs: all_have_outputs,
                    confidence: if all_have_outputs {
                        LoopConfidence::High
                    } else {
                        LoopConfidence::Medium
                    },
                });
                covered.push((start, end));
                start = end;
            } else {
                start += 1;
            }
        }
    }

    patterns.sort_by_key(|pattern| std::cmp::Reverse(pattern.sequence_length));
    patterns
}

/// Run the full pipeline: normalize, pair, detect, and compute streaks.
/// Detect consecutive suffix tool exchanges performing equivalent searches on the same target
/// with zero information gain (all NoMatch, no workspace mutations).
pub fn detect_semantic_investigation_loop(
    items: &[Value],
    exchanges: &[ToolExchange],
    min_repeats: usize,
) -> Option<LoopPattern> {
    if exchanges.len() < min_repeats {
        return None;
    }

    // 1. Extract task literals from user items in items
    let mut user_text = String::new();
    for item in items {
        if matches!(classify_event_kind(item), TrajectoryEventKind::UserMessage) {
            if let Some(txt) = crate::compaction::item_text(item) {
                user_text.push_str(&txt);
                user_text.push(' ');
            }
        }
    }
    let task_literals = crate::investigation::extract_task_literals(&user_text);

    // 2. Classify exchanges from the end
    let mut consecutive_count = 0usize;
    let mut current_target: Option<String> = None;

    for ex in exchanges.iter().rev() {
        let call_item = items.get(ex.call_source_index);
        let Some(call_val) = call_item else {
            break;
        };
        let output_refs: Vec<&Value> = ex
            .output_source_indices
            .iter()
            .filter_map(|&idx| items.get(idx))
            .collect();
        let cmd = crate::tool_semantics::extract_command_invocation(
            ex.call_source_index,
            call_val,
            &output_refs,
        );
        let Some(inv) = cmd else {
            break;
        };

        // Workspace write or test execution breaks negative investigation loop
        if matches!(
            inv.kind,
            crate::tool_semantics::CommandKind::Write
                | crate::tool_semantics::CommandKind::Test
                | crate::tool_semantics::CommandKind::Build
        ) {
            break;
        }

        let targets =
            crate::investigation::extract_investigation_targets(&inv.command, &task_literals);
        if targets.is_empty() {
            break;
        }

        // Determine output text
        let mut output_text = String::new();
        for &out_idx in &ex.output_source_indices {
            if let Some(out_val) = items.get(out_idx) {
                if let Some(text) = out_val
                    .get("output")
                    .or_else(|| out_val.get("text"))
                    .and_then(Value::as_str)
                {
                    output_text.push_str(text);
                }
            }
        }

        let outcome = crate::tool_semantics::classify_search_outcome(
            &inv.command,
            &output_text,
            inv.exit_code,
        );
        let transcript_echo = outcome == crate::tool_semantics::SearchOutcome::Match
            && crate::investigation::is_self_transcript_search(&inv.command);
        if outcome != crate::tool_semantics::SearchOutcome::NoMatch && !transcript_echo {
            // Match or Error breaks negative investigation loop
            break;
        }

        let target = &targets[0];
        if let Some(ref ct) = current_target {
            if !ct.eq_ignore_ascii_case(target) {
                break;
            }
        } else {
            current_target = Some(target.clone());
        }
        consecutive_count += 1;
    }

    if let Some(target) = current_target {
        if consecutive_count >= min_repeats {
            let last_idx = exchanges.len() - 1;
            let first_idx = exchanges.len() - consecutive_count;
            return Some(LoopPattern {
                pattern_hash: format!("semantic_investigation:{target}"),
                sequence_length: 1,
                repeat_count: consecutive_count,
                first_exchange_index: first_idx,
                last_exchange_index: last_idx,
                identical_outputs: true,
                confidence: LoopConfidence::High,
            });
        }
    }

    None
}

pub fn analyze_trajectory(
    items: &[Value],
    identities: &[String],
    policy: &LoopGuardPolicy,
) -> Result<TrajectoryAnalysis, TrajectoryError> {
    let events = normalize_trajectory(items, identities)?;
    let exchanges = build_tool_exchanges(items, &events);
    let repeated_exchanges: Vec<LoopPattern> =
        detect_repeated_exchange(&exchanges, policy.same_exchange_repeat_threshold)
            .into_iter()
            .collect();
    let repeated_sequences = detect_repeated_sequence(
        &exchanges,
        DEFAULT_MIN_SEQUENCE_LEN,
        policy.max_sequence_len,
        policy.sequence_repeat_threshold,
    );
    let repeated_investigations = detect_semantic_investigation_loop(
        items,
        &exchanges,
        policy.same_exchange_repeat_threshold,
    );

    let mut max_tool_only_event_streak = 0usize;
    let mut current_tool_only_event_streak = 0usize;
    for event in &events {
        if matches!(
            event.kind,
            TrajectoryEventKind::ToolCall | TrajectoryEventKind::ToolOutput
        ) {
            current_tool_only_event_streak += 1;
            max_tool_only_event_streak =
                max_tool_only_event_streak.max(current_tool_only_event_streak);
        } else {
            current_tool_only_event_streak = 0;
        }
    }

    // Suffix tool exchange streak: count consecutive tool calls from the end until a message/non-tool event
    let mut current_tool_exchange_streak = 0usize;
    for event in events.iter().rev() {
        if matches!(event.kind, TrajectoryEventKind::ToolCall) {
            current_tool_exchange_streak += 1;
        } else if matches!(event.kind, TrajectoryEventKind::ToolOutput) {
            // output is paired with call, continue scanning backward
        } else {
            break;
        }
    }

    let max_repeat_count = repeated_exchanges
        .iter()
        .chain(repeated_sequences.iter())
        .map(|pattern| pattern.repeat_count)
        .max()
        .unwrap_or(0);

    Ok(TrajectoryAnalysis {
        events,
        exchanges,
        repeated_exchanges,
        repeated_sequences,
        repeated_investigations,
        max_tool_only_event_streak,
        current_tool_only_event_streak,
        current_tool_exchange_streak,
        tool_only_streak: current_tool_exchange_streak,
        max_repeat_count,
    })
}

/// Recovery tracking state for an active loop pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopRecoveryState {
    pub pattern_hash: String,
    pub recovery_count: u8,
    pub recovery_exchange_count: usize,
    #[serde(default)]
    pub finalization_count: u8,
}

/// Authoritative candidate selection for loop guard intervention.
/// Selects ONLY suffix-scoped High-confidence patterns (ending at the very end of current exchanges).
pub fn select_loop_guard_candidate(analysis: &TrajectoryAnalysis) -> Option<&LoopPattern> {
    analysis
        .repeated_exchanges
        .iter()
        .chain(analysis.repeated_sequences.iter())
        .chain(analysis.repeated_investigations.iter())
        .filter(|pattern| {
            // Must be active suffix: the pattern reaches the very end of the current exchanges
            pattern.last_exchange_index + 1 == analysis.exchanges.len()
        })
        .max_by_key(|pattern| {
            let conf_val = match pattern.confidence {
                LoopConfidence::Low => 1usize,
                LoopConfidence::Medium => 2,
                LoopConfidence::High => 3,
            };
            (conf_val, pattern.repeat_count)
        })
}

/// Decide whether the turn may proceed, needs a recovery injection, or must
/// abort. Absolute tool-only streak violations abort immediately; otherwise
/// only active suffix High-confidence patterns intervene, recovering until
/// `policy.recovery_attempts` is exhausted.
pub fn decide_loop_guard(
    analysis: &TrajectoryAnalysis,
    recovery_state: Option<&LoopRecoveryState>,
    policy: &LoopGuardPolicy,
) -> LoopGuardDecision {
    decide_loop_guard_scoped(analysis, recovery_state, policy, true)
}

/// Layer 0 structural guard. When `include_semantic_investigation` is false,
/// investigation no-progress is owned by the shared ledger reducer instead.
pub fn decide_loop_guard_scoped(
    analysis: &TrajectoryAnalysis,
    recovery_state: Option<&LoopRecoveryState>,
    policy: &LoopGuardPolicy,
    include_semantic_investigation: bool,
) -> LoopGuardDecision {
    if analysis.current_tool_exchange_streak > policy.absolute_tool_only_limit {
        if recovery_state.is_none_or(|state| state.finalization_count == 0) {
            return LoopGuardDecision::Finalize {
                pattern_hash: "absolute_tool_only_limit".to_string(),
                message: FINALIZATION_MESSAGE.to_string(),
            };
        }
        return LoopGuardDecision::Abort {
            error_code: TOOL_LOOP_LIMIT_ERROR_CODE,
            message: format!(
                "tool-only streak {} exceeded absolute limit {}; halting before unbounded tool use",
                analysis.current_tool_exchange_streak, policy.absolute_tool_only_limit
            ),
        };
    }

    let candidate = if include_semantic_investigation {
        select_loop_guard_candidate(analysis)
    } else {
        analysis
            .repeated_exchanges
            .iter()
            .chain(analysis.repeated_sequences.iter())
            .filter(|pattern| pattern.last_exchange_index + 1 == analysis.exchanges.len())
            .max_by_key(|pattern| {
                let conf_val = match pattern.confidence {
                    LoopConfidence::Low => 1usize,
                    LoopConfidence::Medium => 2,
                    LoopConfidence::High => 3,
                };
                (conf_val, pattern.repeat_count)
            })
    };

    match candidate {
        Some(pattern) if pattern.confidence == LoopConfidence::High => {
            let recovery_count = if let Some(state) = recovery_state {
                if state.pattern_hash == pattern.pattern_hash {
                    state.recovery_count
                } else {
                    0
                }
            } else {
                0
            };

            let message = if let Some(target) =
                pattern.pattern_hash.strip_prefix("semantic_investigation:")
            {
                format!(
                    "Repeated investigation of {target} has produced no new evidence. Do not repeat equivalent searches unless new evidence appears. Use the established task requirements and current workspace state to choose a materially different action, such as editing, testing, or completing the task."
                )
            } else {
                RECOVERY_MESSAGE.to_string()
            };

            if recovery_count < policy.recovery_attempts {
                LoopGuardDecision::Recover {
                    pattern_hash: pattern.pattern_hash.clone(),
                    message,
                }
            } else if recovery_state.is_none_or(|state| state.finalization_count == 0) {
                LoopGuardDecision::Finalize {
                    pattern_hash: pattern.pattern_hash.clone(),
                    message: FINALIZATION_MESSAGE.to_string(),
                }
            } else {
                LoopGuardDecision::Abort {
                    error_code: TOOL_LOOP_LIMIT_ERROR_CODE,
                    message: format!(
                        "repeated tool loop {} persisted after {recovery_count} recovery attempt(s)",
                        pattern.pattern_hash
                    ),
                }
            }
        }
        _ => LoopGuardDecision::Allow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn identities(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("req:{index}")).collect()
    }

    fn call_item(id: &str, command: &str) -> Value {
        json!({
            "type": "function_call",
            "call_id": id,
            "name": "exec_command",
            "arguments": serde_json::to_string(&json!({ "cmd": command })).unwrap()
        })
    }

    fn output_item(id: &str, text: &str) -> Value {
        json!({
            "type": "function_call_output",
            "call_id": id,
            "output": text
        })
    }

    fn exchange_pair(command: &str, result: &str, offset: usize) -> Vec<Value> {
        let id = format!("call_{offset}");
        vec![call_item(&id, command), output_item(&id, result)]
    }

    #[test]
    fn identity_mismatch_is_an_error_not_a_fallback() {
        let items = vec![
            json!({"type": "message", "role": "user", "content": "hi"}),
            json!({"type": "message", "role": "user", "content": "again"}),
        ];
        assert_eq!(
            normalize_trajectory(&items, &vec!["req:0".to_string()]),
            Err(TrajectoryError::IdentityMismatch {
                item_count: 2,
                identity_count: 1
            })
        );
    }

    #[test]
    fn call_fingerprint_is_stable_across_argument_key_order() {
        let a = json!({
            "type": "function_call",
            "call_id": "c1",
            "name": "exec_command",
            "arguments": "{\"cmd\":\"ls\",\"cwd\":\"/tmp\"}"
        });
        let b = json!({
            "type": "function_call",
            "call_id": "c2",
            "name": "exec_command",
            "arguments": "{\"cwd\":\"/tmp\",\"cmd\":\"ls\"}"
        });
        assert_eq!(tool_call_fingerprint(&a), tool_call_fingerprint(&b));

        let unordered = json!({"cwd": "/tmp\n ", "cmd": [1, 2, {"z": 1, "a": 2}]});
        let ordered = json!({"cmd": [1, 2, {"a": 2, "z": 1}], "cwd": "/tmp\n"});
        assert_eq!(
            canonicalize_json_for_fingerprint(&unordered),
            canonicalize_json_for_fingerprint(&ordered)
        );
    }

    #[test]
    fn different_commands_produce_different_fingerprints() {
        let a = call_item("c1", "codex features list");
        let b = call_item("c2", "codex features list --enable foo");
        assert_ne!(tool_call_fingerprint(&a), tool_call_fingerprint(&b));
    }

    #[test]
    fn output_fingerprint_ignores_ansi_coloring_and_crlf() {
        let plain = output_item("c1", "error: build failed\r\nexit code 101");
        let noisy = output_item("c2", "\x1b[31merror\x1b[0m: build failed\nexit code 101");
        assert_eq!(
            tool_output_fingerprint(&plain),
            tool_output_fingerprint(&noisy)
        );
    }

    #[test]
    fn single_identical_exchange_loops_at_three_repeats() {
        let mut items = vec![json!({"type": "message", "role": "user", "content": "go"})];
        for offset in 0..3 {
            items.extend(exchange_pair("same-cmd", "same-result", offset));
        }
        let events = normalize_trajectory(&items, &identities(items.len())).unwrap();
        let exchanges = build_tool_exchanges(&items, &events);
        assert_eq!(exchanges.len(), 3);
        assert!(exchanges.iter().all(|exchange| exchange.completed));

        let pattern = detect_repeated_exchange(&exchanges, 3).expect("loop expected");
        assert_eq!(pattern.repeat_count, 3);
        assert_eq!(pattern.sequence_length, 1);
        assert_eq!(pattern.confidence, LoopConfidence::High);
        assert!(pattern.identical_outputs);

        assert!(detect_repeated_exchange(&exchanges, 4).is_none());
    }

    #[test]
    fn abcd_x3_sequence_is_detected_longest_first() {
        let commands = ["cmd-a", "cmd-b", "cmd-c", "cmd-d"];
        let mut items = Vec::new();
        let mut offset = 0usize;
        for _ in 0..3 {
            for command in commands {
                items.extend(exchange_pair(command, "stable output", offset));
                offset += 1;
            }
        }
        let events = normalize_trajectory(&items, &identities(items.len())).unwrap();
        let exchanges = build_tool_exchanges(&items, &events);
        assert_eq!(exchanges.len(), 12);

        let patterns = detect_repeated_sequence(&exchanges, 2, 8, 3);
        let best = patterns
            .iter()
            .find(|pattern| pattern.sequence_length == 4)
            .expect("A-B-C-D x3 expected");
        assert_eq!(best.repeat_count, 3);
        assert_eq!(best.first_exchange_index, 0);
        assert_eq!(best.last_exchange_index, 11);
        assert_eq!(best.confidence, LoopConfidence::High);
        assert!(best.identical_outputs);
        assert!(patterns.iter().all(|p| p.sequence_length <= 4));
    }

    #[test]
    fn loop_guard_recovers_then_finalizes_once_before_aborting() {
        let mut items = vec![json!({"type": "message", "role": "user", "content": "go"})];
        for offset in 0..3 {
            items.extend(exchange_pair("stuck-cmd", "stuck-result", offset));
        }
        let policy = LoopGuardPolicy::default();
        let analysis = analyze_trajectory(&items, &identities(items.len()), &policy).unwrap();

        let pattern_hash = analysis.repeated_exchanges[0].pattern_hash.clone();

        match decide_loop_guard(&analysis, None, &policy) {
            LoopGuardDecision::Recover {
                pattern_hash: ref phash,
                message,
            } => {
                assert_eq!(phash, &pattern_hash);
                assert!(message.contains("Do not repeat these tool calls."));
                assert!(message.contains("4. finish the task"));
            }
            other => panic!("expected Recover, got {other:?}"),
        }

        let state_recovered = LoopRecoveryState {
            pattern_hash: pattern_hash.clone(),
            recovery_count: 1,
            recovery_exchange_count: analysis.exchanges.len(),
            finalization_count: 0,
        };

        match decide_loop_guard(&analysis, Some(&state_recovered), &policy) {
            LoopGuardDecision::Finalize {
                pattern_hash: ref phash,
                message,
            } => {
                assert_eq!(phash, &pattern_hash);
                assert!(message.contains("disabled all tools"));
            }
            other => panic!("expected Finalize, got {other:?}"),
        }

        let state_finalized = LoopRecoveryState {
            finalization_count: 1,
            ..state_recovered
        };
        match decide_loop_guard(&analysis, Some(&state_finalized), &policy) {
            LoopGuardDecision::Abort { error_code, .. } => {
                assert_eq!(error_code, "vellum_tool_loop_limit");
            }
            other => panic!("expected Abort, got {other:?}"),
        }
    }

    #[test]
    fn clean_analysis_allows_the_turn() {
        let items = vec![
            json!({"type": "message", "role": "user", "content": "go"}),
            json!({"type": "message", "role": "assistant", "content": "ok"}),
        ];
        let policy = LoopGuardPolicy::default();
        let analysis = analyze_trajectory(&items, &identities(items.len()), &policy).unwrap();
        assert_eq!(
            decide_loop_guard(&analysis, None, &policy),
            LoopGuardDecision::Allow
        );
    }

    #[test]
    fn historical_loop_does_not_trigger_current_guard() {
        let mut items = vec![json!({"type": "message", "role": "user", "content": "go"})];
        // Turn 1..3: Loop occurred in early history
        for offset in 0..3 {
            items.extend(exchange_pair("stuck-cmd", "stuck-result", offset));
        }
        // Turn 4..5: Agent broke loop and made progress
        items.extend(exchange_pair("edit-code", "diff applied", 10));
        items.extend(exchange_pair("cargo-test", "test passed", 11));

        let policy = LoopGuardPolicy::default();
        let analysis = analyze_trajectory(&items, &identities(items.len()), &policy).unwrap();

        // Even if loop pattern exists in early history, it's NOT at the suffix
        assert_eq!(
            decide_loop_guard(&analysis, None, &policy),
            LoopGuardDecision::Allow
        );
    }

    #[test]
    fn loop_recovery_then_progress_does_not_abort() {
        let mut items = vec![json!({"type": "message", "role": "user", "content": "go"})];
        for offset in 0..3 {
            items.extend(exchange_pair("stuck-cmd", "stuck-result", offset));
        }
        let policy = LoopGuardPolicy::default();
        let analysis_1 = analyze_trajectory(&items, &identities(items.len()), &policy).unwrap();
        let pattern_hash = analysis_1.repeated_exchanges[0].pattern_hash.clone();

        let recovery_state = LoopRecoveryState {
            pattern_hash,
            recovery_count: 1,
            recovery_exchange_count: analysis_1.exchanges.len(),
            finalization_count: 0,
        };

        // Model received guidance and took different actions
        items.extend(exchange_pair("read-file", "content", 10));
        items.extend(exchange_pair("write-file", "updated", 11));

        let analysis_2 = analyze_trajectory(&items, &identities(items.len()), &policy).unwrap();
        assert_eq!(
            decide_loop_guard(&analysis_2, Some(&recovery_state), &policy),
            LoopGuardDecision::Allow
        );
    }

    #[test]
    fn same_stdout_different_exit_code_has_different_fingerprint() {
        let out_fail = json!({
            "type": "function_call_output",
            "call_id": "c1",
            "output": "running test suite\nfailed",
            "exit_code": 1
        });
        let out_pass = json!({
            "type": "function_call_output",
            "call_id": "c1",
            "output": "running test suite\nfailed", // same stdout
            "exit_code": 0
        });

        let fp1 = tool_output_fingerprint(&out_fail).unwrap();
        let fp2 = tool_output_fingerprint(&out_pass).unwrap();
        assert_ne!(
            fp1, fp2,
            "different exit code must produce distinct fingerprint"
        );
    }

    #[test]
    fn parallel_calls_preserve_every_call_and_output() {
        let items = vec![
            json!({"type": "function_call", "call_id": "c_a", "name": "tool_a", "arguments": "{}"}),
            json!({"type": "function_call", "call_id": "c_b", "name": "tool_b", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "c_a", "output": "res_a"}),
            json!({"type": "function_call_output", "call_id": "c_b", "output": "res_b"}),
        ];

        let exchanges = occurrence_tool_exchanges(&items);
        assert_eq!(exchanges.len(), 2);
        assert_eq!(exchanges[0].tool_name, "tool_a");
        assert_eq!(exchanges[0].call_source_index, 0);
        assert_eq!(exchanges[0].output_source_indices, vec![2]);

        assert_eq!(exchanges[1].tool_name, "tool_b");
        assert_eq!(exchanges[1].call_source_index, 1);
        assert_eq!(exchanges[1].output_source_indices, vec![3]);

        let report = analyze_tool_pairing(&items);
        assert_eq!(report.total_calls, 2);
        assert_eq!(report.total_outputs, 2);
        assert!(report.orphan_outputs.is_empty());
        assert!(report.incomplete_calls.is_empty());
    }

    #[test]
    fn multi_output_call_pairs_all_outputs_without_orphan() {
        let items = vec![
            json!({"type": "function_call", "call_id": "c_multi", "name": "tool_stream", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "c_multi", "output": "chunk1"}),
            json!({"type": "function_call_output", "call_id": "c_multi", "output": "chunk2"}),
        ];

        let exchanges = occurrence_tool_exchanges(&items);
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].output_source_indices, vec![1, 2]);

        let report = analyze_tool_pairing(&items);
        assert_eq!(report.total_calls, 1);
        assert_eq!(report.total_outputs, 2);
        assert!(report.orphan_outputs.is_empty());
    }

    #[test]
    fn duplicate_call_id_occurrences_pair_correctly() {
        let items = vec![
            json!({"type": "function_call", "call_id": "dup", "name": "t1", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "dup", "output": "out1"}),
            json!({"type": "function_call", "call_id": "dup", "name": "t2", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "dup", "output": "out2"}),
        ];

        let exchanges = occurrence_tool_exchanges(&items);
        assert_eq!(exchanges.len(), 2);
        assert_eq!(exchanges[0].call_source_index, 0);
        assert_eq!(exchanges[0].output_source_indices, vec![1]);
        assert_eq!(exchanges[1].call_source_index, 2);
        assert_eq!(exchanges[1].output_source_indices, vec![3]);

        let report = analyze_tool_pairing(&items);
        assert!(report.orphan_outputs.is_empty());
        assert!(report.incomplete_calls.is_empty());
    }

    #[test]
    fn tool_only_streak_counts_consecutive_tool_events() {
        let mut items = vec![
            json!({"type": "message", "role": "user", "content": "go"}),
            json!({"type": "message", "role": "assistant", "content": "working"}),
        ];
        for offset in 0..2 {
            items.extend(exchange_pair("first-phase", "out", offset));
        }
        items.push(json!({"type": "message", "role": "assistant", "content": "midpoint"}));
        for offset in 10..13 {
            items.extend(exchange_pair("second-phase", "out", offset));
        }
        let policy = LoopGuardPolicy::default();
        let analysis = analyze_trajectory(&items, &identities(items.len()), &policy).unwrap();
        assert_eq!(analysis.current_tool_exchange_streak, 3);
        assert_eq!(analysis.max_tool_only_event_streak, 6);
        assert_eq!(analysis.events[0].kind, TrajectoryEventKind::UserMessage);
        assert_eq!(
            analysis.events[1].kind,
            TrajectoryEventKind::AssistantMessage
        );
    }

    #[test]
    fn historical_absolute_streak_does_not_abort_after_progress() {
        let mut items =
            vec![json!({"type": "message", "role": "user", "content": "investigate bug"})];
        // 30 consecutive tool exchanges earlier in the session
        for i in 0..30 {
            items.extend(exchange_pair(&format!("cmd_{i}"), "output", i));
        }
        // Assistant made progress and user replied
        items.push(json!({"type": "message", "role": "assistant", "content": "Found the root cause in auth.rs"}));
        items.push(json!({"type": "message", "role": "user", "content": "Please apply the fix."}));

        // Followed by 2 normal tool exchanges
        items.extend(exchange_pair("apply_patch", "ok", 100));
        items.extend(exchange_pair("cargo_test", "passed", 101));

        let policy = LoopGuardPolicy {
            absolute_tool_only_limit: 24,
            ..Default::default()
        };
        let analysis = analyze_trajectory(&items, &identities(items.len()), &policy).unwrap();
        assert_eq!(analysis.max_tool_only_event_streak, 60); // historical max
        assert_eq!(analysis.current_tool_exchange_streak, 2); // current suffix streak

        let decision = decide_loop_guard(&analysis, None, &policy);
        assert_eq!(
            decision,
            LoopGuardDecision::Allow,
            "historical streak must not abort after forward progress"
        );
    }

    #[test]
    fn current_suffix_tool_streak_hits_absolute_limit() {
        let mut items =
            vec![json!({"type": "message", "role": "user", "content": "run unbounded tasks"})];
        // 25 consecutive tool exchanges in current suffix
        for i in 0..25 {
            items.extend(exchange_pair(&format!("distinct_cmd_{i}"), "output", i));
        }

        let policy = LoopGuardPolicy {
            absolute_tool_only_limit: 24,
            ..Default::default()
        };
        let analysis = analyze_trajectory(&items, &identities(items.len()), &policy).unwrap();
        assert_eq!(analysis.current_tool_exchange_streak, 25);

        let decision = decide_loop_guard(&analysis, None, &policy);
        match decision {
            LoopGuardDecision::Finalize { pattern_hash, .. } => {
                assert_eq!(pattern_hash, "absolute_tool_only_limit");
            }
            other => panic!("expected Finalize, got {:?}", other),
        }
    }

    #[test]
    fn same_call_different_output_is_new_loop_pattern() {
        let items_a = vec![
            json!({"type": "function_call", "call_id": "c1", "name": "get_status", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": "status: pending"}),
            json!({"type": "function_call", "call_id": "c2", "name": "get_status", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "c2", "output": "status: pending"}),
            json!({"type": "function_call", "call_id": "c3", "name": "get_status", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "c3", "output": "status: pending"}),
        ];

        let items_b = vec![
            json!({"type": "function_call", "call_id": "c1", "name": "get_status", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": "status: failed"}),
            json!({"type": "function_call", "call_id": "c2", "name": "get_status", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "c2", "output": "status: failed"}),
            json!({"type": "function_call", "call_id": "c3", "name": "get_status", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "c3", "output": "status: failed"}),
        ];

        let policy = LoopGuardPolicy::default();
        let analysis_a = analyze_trajectory(&items_a, &identities(items_a.len()), &policy).unwrap();
        let analysis_b = analyze_trajectory(&items_b, &identities(items_b.len()), &policy).unwrap();

        let pattern_a = analysis_a.repeated_exchanges.first().unwrap();
        let pattern_b = analysis_b.repeated_exchanges.first().unwrap();

        assert_ne!(
            pattern_a.pattern_hash, pattern_b.pattern_hash,
            "different outputs must produce distinct pattern hashes"
        );
    }

    #[test]
    fn sequence_distinct_outputs_x3_is_high() {
        let mut items = vec![json!({"type": "message", "role": "user", "content": "run task"})];
        // Cycle of 4 distinct tools with distinct outputs: A, B, C, D
        for cycle in 0..3 {
            items.extend(exchange_pair("step-A", "output-alpha", cycle * 4));
            items.extend(exchange_pair("step-B", "output-beta", cycle * 4 + 1));
            items.extend(exchange_pair("step-C", "output-gamma", cycle * 4 + 2));
            items.extend(exchange_pair("step-D", "output-delta", cycle * 4 + 3));
        }

        let policy = LoopGuardPolicy::default();
        let analysis = analyze_trajectory(&items, &identities(items.len()), &policy).unwrap();
        assert_eq!(analysis.repeated_sequences.len(), 1);
        let seq = &analysis.repeated_sequences[0];
        assert_eq!(seq.sequence_length, 4);
        assert_eq!(seq.repeat_count, 3);
        assert_eq!(seq.confidence, LoopConfidence::High);
    }

    #[test]
    fn semantic_equivalent_no_progress_loop_triggers_recovery() {
        let items = vec![
            json!({"type": "message", "role": "user", "content": "Find CIPHERTEXT_RECOVERY_EXACT in project"}),
            json!({"type": "function_call", "call_id": "c1", "name": "exec_command", "arguments": "{\"command\": \"Get-ChildItem -Recurse -Filter CIPHERTEXT_RECOVERY_EXACT\"}"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": ""}),
            json!({"type": "function_call", "call_id": "c2", "name": "exec_command", "arguments": "{\"command\": \"Select-String -Pattern CIPHERTEXT_RECOVERY_EXACT\"}"}),
            json!({"type": "function_call_output", "call_id": "c2", "output": ""}),
            json!({"type": "function_call", "call_id": "c3", "name": "exec_command", "arguments": "{\"command\": \"python -c \\\"print('search')\\\" CIPHERTEXT_RECOVERY_EXACT\"}"}),
            json!({"type": "function_call_output", "call_id": "c3", "output": ""}),
        ];

        let policy = LoopGuardPolicy::default();
        let analysis = analyze_trajectory(&items, &identities(items.len()), &policy).unwrap();
        assert!(
            analysis.repeated_investigations.is_some(),
            "Must detect semantic investigation loop across 3 distinct search commands"
        );
        let inv_pattern = analysis.repeated_investigations.as_ref().unwrap();
        assert_eq!(inv_pattern.repeat_count, 3);
        assert_eq!(inv_pattern.confidence, LoopConfidence::High);

        let decision = decide_loop_guard(&analysis, None, &policy);
        match decision {
            LoopGuardDecision::Recover {
                message,
                pattern_hash,
            } => {
                assert!(pattern_hash.contains("CIPHERTEXT_RECOVERY_EXACT"));
                assert!(message.contains("CIPHERTEXT_RECOVERY_EXACT"));
                assert!(message.contains("Do not repeat equivalent searches"));
            }
            other => panic!("Expected Recover decision, got {:?}", other),
        }
    }

    #[test]
    fn legitimate_search_narrowing_does_not_trigger_loop() {
        let items = vec![
            json!({"type": "message", "role": "user", "content": "Find CIPHERTEXT_RECOVERY_EXACT in project"}),
            json!({"type": "function_call", "call_id": "c1", "name": "exec_command", "arguments": "{\"command\": \"Select-String -Pattern CIPHERTEXT_RECOVERY_EXACT\"}"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": "src/lib.rs:1:CIPHERTEXT_RECOVERY_EXACT"}),
            json!({"type": "function_call", "call_id": "c2", "name": "exec_command", "arguments": "{\"command\": \"Select-String -Path src/*.rs -Pattern CIPHERTEXT_RECOVERY_EXACT\"}"}),
            json!({"type": "function_call_output", "call_id": "c2", "output": "src/lib.rs:1:CIPHERTEXT_RECOVERY_EXACT"}),
            json!({"type": "function_call", "call_id": "c3", "name": "exec_command", "arguments": "{\"command\": \"Select-String -Path tests/*.rs -Pattern CIPHERTEXT_RECOVERY_EXACT\"}"}),
            json!({"type": "function_call_output", "call_id": "c3", "output": "tests/test.rs:1:CIPHERTEXT_RECOVERY_EXACT"}),
        ];

        let policy = LoopGuardPolicy::default();
        let analysis = analyze_trajectory(&items, &identities(items.len()), &policy).unwrap();
        assert!(
            analysis.repeated_investigations.is_none(),
            "Matches must not be flagged as negative investigation loops"
        );
        let decision = decide_loop_guard(&analysis, None, &policy);
        assert_eq!(decision, LoopGuardDecision::Allow);
    }

    #[test]
    fn repeated_rollout_transcript_echo_triggers_no_progress_recovery() {
        let items = vec![
            json!({"type": "message", "role": "user", "content": "Find CIPHERTEXT_RECOVERY_EXACT in project"}),
            json!({"type": "function_call", "call_id": "c1", "name": "exec_command", "arguments": "{\"command\": \"Select-String -Path $env:CODEX_HOME\\\\sessions\\\\rollout-a.jsonl -Pattern CIPHERTEXT_RECOVERY_EXACT\"}"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": "prompt:CIPHERTEXT_RECOVERY_EXACT"}),
            json!({"type": "function_call", "call_id": "c2", "name": "exec_command", "arguments": "{\"command\": \"Select-String -Path $env:CODEX_HOME\\\\sessions\\\\rollout-b.jsonl -Pattern CIPHERTEXT_RECOVERY_EXACT\"}"}),
            json!({"type": "function_call_output", "call_id": "c2", "output": "prompt:CIPHERTEXT_RECOVERY_EXACT"}),
            json!({"type": "function_call", "call_id": "c3", "name": "exec_command", "arguments": "{\"command\": \"Select-String -Path $env:CODEX_HOME\\\\sessions\\\\rollout-c.jsonl -Pattern CIPHERTEXT_RECOVERY_EXACT\"}"}),
            json!({"type": "function_call_output", "call_id": "c3", "output": "prompt:CIPHERTEXT_RECOVERY_EXACT"}),
        ];

        let policy = LoopGuardPolicy::default();
        let analysis = analyze_trajectory(&items, &identities(items.len()), &policy).unwrap();
        assert!(analysis.repeated_investigations.is_some());
    }

    #[test]
    fn user_message_content_array_triggers_semantic_investigation_loop_recovery() {
        // User message with Responses-style content parts array
        let items = vec![
            json!({
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "Please preserve CIPHERTEXT_RECOVERY_EXACT in DECISIONS.md"}
                ]
            }),
            json!({"type": "function_call", "call_id": "c1", "name": "exec_command", "arguments": "{\"command\": \"Get-ChildItem -Recurse -Filter CIPHERTEXT_RECOVERY_EXACT\"}"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": "", "status": 0}),
            json!({"type": "function_call", "call_id": "c2", "name": "exec_command", "arguments": "{\"command\": \"Select-String -Pattern CIPHERTEXT_RECOVERY_EXACT\"}"}),
            json!({"type": "function_call_output", "call_id": "c2", "output": "", "status": 0}),
            json!({"type": "function_call", "call_id": "c3", "name": "exec_command", "arguments": "{\"command\": \"python -c \\\"print('search')\\\" CIPHERTEXT_RECOVERY_EXACT\"}"}),
            json!({"type": "function_call_output", "call_id": "c3", "output": "", "status": 0}),
        ];

        let policy = LoopGuardPolicy::default();
        let analysis = analyze_trajectory(&items, &identities(items.len()), &policy).unwrap();
        assert!(
            analysis.repeated_investigations.is_some(),
            "Must extract task literal from content array and detect semantic investigation loop"
        );
        let decision = decide_loop_guard(&analysis, None, &policy);
        match decision {
            LoopGuardDecision::Recover {
                message,
                pattern_hash,
            } => {
                assert!(pattern_hash.contains("CIPHERTEXT_RECOVERY_EXACT"));
                assert!(message.contains("CIPHERTEXT_RECOVERY_EXACT"));
            }
            other => panic!("Expected Recover decision, got {:?}", other),
        }
    }
}
