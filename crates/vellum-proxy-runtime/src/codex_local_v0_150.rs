//! Codex Local Compact 0.150 — Vellum embedded port.
//!
//! Ported from OpenAI Codex `rust-v0.150.0-alpha.8`
//! (commit `fcbdb57851be70192fd0c21faa9e529146e93ff1`, Apache-2.0).
//! See `third_party/codex-0.150/PROVENANCE.md` for the vendored file
//! inventory, SHA-256 digests, and the statement of modification.
//!
//! This module is the *semantic* half of the engine: pure functions over
//! `serde_json::Value` conversation items. The single upstream summarizer call
//! and the record journal live in the execution layer (`exec.rs`), which must
//! route that call to the session's own provider, model, auth, and verified
//! reasoning effort.
//!
//! What upstream does, preserved here in behavior:
//!
//! * The compaction prompt is the fixed upstream `prompt.md`; the summary is
//!   prefixed with the fixed upstream `summary_prefix.md`.
//! * The summary body is the **last assistant message** produced by the
//!   summarizer turn — nothing is parsed out of it, and no JSON is requested.
//! * Replacement history is the most recent real user messages under a
//!   20,000-token budget (emitted oldest-first), followed by the prefixed
//!   summary as the final item.
//! * Prior summary messages are never re-selected as user messages.
//! * On a context-window error the summarizer input drops its **oldest** item
//!   and retries, preserving the prefix cache; the source record is untouched.
//!
//! What Vellum adds and upstream does not have: cross-provider sanitization of
//! the summarizer input (see `crate::compaction::sanitize_portable_history`),
//! applied by the caller before `build_summarizer_input`.

use serde_json::Value;

use crate::compaction::{classify_item, is_summary_item, item_text, ItemRole};

/// Stable wire/API identifier for this engine.
pub const ENGINE_ID: &str = "codex_local_v0_150";

/// Human-facing engine label.
pub const ENGINE_LABEL: &str = "Codex Local Compact 0.150 (embedded)";

/// Upstream provenance recorded on every compaction record.
pub const ENGINE_PROVENANCE: &str = "openai-codex@rust-v0.150.0-alpha.8+fcbdb578";

/// Marker prefix for records materialized by this engine. Materialization must
/// only accept this exact prefix; `vcomp1.` (Canonical) is not readable here.
pub const COMPACTION_MARKER_PREFIX: &str = "vcompact.codex0150.";

/// Upstream `codex-rs/prompts/templates/compact/prompt.md`, vendored verbatim.
pub const SUMMARIZATION_PROMPT: &str =
    include_str!("../../../third_party/codex-0.150/templates/compact/prompt.md");

/// Upstream `codex-rs/prompts/templates/compact/summary_prefix.md`, vendored
/// verbatim. Byte-identical to the historical `OFFICIAL_SUMMARY_PREFIX`.
pub const SUMMARY_PREFIX: &str =
    include_str!("../../../third_party/codex-0.150/templates/compact/summary_prefix.md");

/// Upstream `COMPACT_USER_MESSAGE_MAX_TOKENS`.
pub const COMPACT_USER_MESSAGE_MAX_TOKENS: usize = 20_000;

/// Upstream `APPROX_BYTES_PER_TOKEN` (`codex-rs/utils/string/src/truncate.rs`).
const APPROX_BYTES_PER_TOKEN: usize = 4;

/// Port of upstream `approx_token_count`: bytes divided by 4, rounded up.
pub fn approx_token_count(text: &str) -> usize {
    let len = text.len();
    len.saturating_add(APPROX_BYTES_PER_TOKEN.saturating_sub(1)) / APPROX_BYTES_PER_TOKEN
}

/// Port of upstream `approx_bytes_for_tokens`.
pub fn approx_bytes_for_tokens(tokens: usize) -> usize {
    tokens.saturating_mul(APPROX_BYTES_PER_TOKEN)
}

/// Port of upstream `approx_tokens_from_byte_count`.
fn approx_tokens_from_byte_count(bytes: usize) -> u64 {
    let bytes_u64 = bytes as u64;
    bytes_u64.saturating_add((APPROX_BYTES_PER_TOKEN as u64).saturating_sub(1))
        / (APPROX_BYTES_PER_TOKEN as u64)
}

/// Port of upstream `split_budget`.
fn split_budget(budget: usize) -> (usize, usize) {
    let left = budget / 2;
    (left, budget - left)
}

/// Port of upstream `split_string`. Splits on UTF-8 character boundaries so a
/// truncated message is never invalid UTF-8.
fn split_string(s: &str, beginning_bytes: usize, end_bytes: usize) -> (usize, &str, &str) {
    if s.is_empty() {
        return (0, "", "");
    }

    let len = s.len();
    let tail_start_target = len.saturating_sub(end_bytes);
    let mut prefix_end = 0usize;
    let mut suffix_start = len;
    let mut removed_chars = 0usize;
    let mut suffix_started = false;

    for (idx, ch) in s.char_indices() {
        let char_end = idx + ch.len_utf8();
        if char_end <= beginning_bytes {
            prefix_end = char_end;
            continue;
        }

        if idx >= tail_start_target {
            if !suffix_started {
                suffix_start = idx;
                suffix_started = true;
            }
            continue;
        }

        removed_chars = removed_chars.saturating_add(1);
    }

    if suffix_start < prefix_end {
        suffix_start = prefix_end;
    }

    (removed_chars, &s[..prefix_end], &s[suffix_start..])
}

/// Port of upstream `truncate_with_byte_estimate` in token mode.
fn truncate_with_token_estimate(s: &str, max_bytes: usize) -> String {
    if s.is_empty() {
        return String::new();
    }

    if max_bytes == 0 {
        let removed = approx_tokens_from_byte_count(s.len());
        return format!("…{removed} tokens truncated…");
    }

    if s.len() <= max_bytes {
        return s.to_string();
    }

    let total_bytes = s.len();
    let (left_budget, right_budget) = split_budget(max_bytes);
    let (_removed_chars, left, right) = split_string(s, left_budget, right_budget);
    let removed = approx_tokens_from_byte_count(total_bytes.saturating_sub(max_bytes));
    let marker = format!("…{removed} tokens truncated…");

    let mut out = String::with_capacity(left.len() + marker.len() + right.len() + 1);
    out.push_str(left);
    out.push_str(&marker);
    out.push_str(right);
    out
}

/// Port of upstream `truncate_text(_, TruncationPolicy::Tokens(max_tokens))`.
pub fn truncate_text_tokens(s: &str, max_tokens: usize) -> String {
    if s.is_empty() {
        return String::new();
    }
    if max_tokens > 0 && s.len() <= approx_bytes_for_tokens(max_tokens) {
        return s.to_string();
    }
    truncate_with_token_estimate(s, approx_bytes_for_tokens(max_tokens))
}

/// Port of upstream `is_summary_message`. A replacement summary begins with the
/// prefix followed by a newline; such an item is never re-selected as a user
/// message on a later compaction.
pub fn is_summary_message(message: &str) -> bool {
    message.starts_with(&format!("{}\n", SUMMARY_PREFIX.trim_end_matches('\n')))
}

/// A real user message eligible for the replacement-history budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactedUserMessage {
    pub message: String,
}

/// Port of upstream `compacted_user_message`: keep `role: "user"` items whose
/// text is not itself a prior compaction summary.
fn compacted_user_message(item: &Value) -> Option<CompactedUserMessage> {
    if !matches!(classify_item(item), ItemRole::User) {
        return None;
    }
    if is_summary_item(item) {
        return None;
    }
    let message = item_text(item)?;
    if is_summary_message(&message) {
        return None;
    }
    Some(CompactedUserMessage { message })
}

/// Port of upstream `collect_annotated_user_messages`.
pub fn collect_user_messages(items: &[Value]) -> Vec<CompactedUserMessage> {
    items.iter().filter_map(compacted_user_message).collect()
}

/// The single summarizer request payload: the sanitized conversation followed
/// by the fixed upstream compaction prompt as a user turn.
///
/// The caller is responsible for having already run
/// `crate::compaction::sanitize_portable_history` over `sanitized_history`.
pub fn build_summarizer_input(sanitized_history: &[Value]) -> Vec<Value> {
    let mut input = sanitized_history.to_vec();
    input.push(serde_json::json!({
        "type": "message",
        "role": "user",
        "content": [{ "type": "input_text", "text": SUMMARIZATION_PROMPT }],
    }));
    input
}

/// Port of upstream `ContextWindowExceeded` handling: drop the **oldest**
/// summarizer input item and retry, preserving the prefix cache. Returns
/// `false` when only one item remains, at which point upstream surfaces the
/// context-window error rather than retrying again.
///
/// This mutates only the summarizer input. The caller's source record must not
/// be rewritten.
pub fn remove_oldest_summarizer_item(input: &mut Vec<Value>) -> bool {
    if input.len() > 1 {
        input.remove(0);
        true
    } else {
        false
    }
}

/// Port of upstream `format!("{SUMMARY_PREFIX}\n{summary_suffix}")`, where
/// `summary_suffix` is the last assistant message of the summarizer turn.
pub fn summary_text(last_assistant_message: &str) -> String {
    format!(
        "{}\n{}",
        SUMMARY_PREFIX.trim_end_matches('\n'),
        last_assistant_message
    )
}

/// Build the summary item appended last in replacement history.
pub fn summary_item(summary_text: &str) -> Value {
    serde_json::json!({
        "type": "message",
        "role": "user",
        "content": [{ "type": "input_text", "text": summary_text }],
    })
}

/// Port of upstream `build_compacted_history_with_limit`.
///
/// Walks user messages newest-first spending a `max_tokens` budget; the first
/// message that does not fit is truncated to the remaining budget and ends
/// selection. The selection is then reversed back to chronological order and
/// the prefixed summary is appended last.
pub fn build_compacted_history_with_limit(
    user_messages: &[CompactedUserMessage],
    summary_text: &str,
    max_tokens: usize,
) -> Vec<Value> {
    let mut selected: Vec<CompactedUserMessage> = Vec::new();
    if max_tokens > 0 {
        let mut remaining = max_tokens;
        for message in user_messages.iter().rev() {
            if remaining == 0 {
                break;
            }
            let tokens = approx_token_count(&message.message);
            if tokens <= remaining {
                selected.push(message.clone());
                remaining = remaining.saturating_sub(tokens);
            } else {
                selected.push(CompactedUserMessage {
                    message: truncate_text_tokens(&message.message, remaining),
                });
                break;
            }
        }
        selected.reverse();
    }

    let mut history: Vec<Value> = selected
        .iter()
        .map(|message| {
            serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": message.message }],
            })
        })
        .collect();
    history.push(summary_item(summary_text));
    history
}

/// Port of upstream `build_compacted_history` at the default budget.
pub fn build_compacted_history(
    user_messages: &[CompactedUserMessage],
    summary_text: &str,
) -> Vec<Value> {
    build_compacted_history_with_limit(user_messages, summary_text, COMPACT_USER_MESSAGE_MAX_TOKENS)
}

/// End-to-end replacement assembly once the summarizer has returned its last
/// assistant message. `source` is the sanitized pre-compaction history.
pub fn build_replacement_items(source: &[Value], last_assistant_message: &str) -> Vec<Value> {
    let summary = summary_text(last_assistant_message);
    let user_messages = collect_user_messages(source);
    build_compacted_history(&user_messages, &summary)
}

/// Classified failure of one compaction attempt. Every variant is reported
/// truthfully to the caller; none of them silently reroute to another model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionFailure {
    /// Transport/connection failure talking to the session's provider.
    Transport(String),
    /// The provider or Vellum deadline elapsed.
    Timeout,
    /// Provider rejected the request for quota/billing reasons.
    Quota(String),
    /// The summarizer returned no usable assistant message.
    EmptySummary,
    /// Input no longer fits even after dropping every droppable item.
    ContextExhausted,
    /// Retries were spent without a successful attempt.
    RetryExhausted(String),
}

impl CompactionFailure {
    /// Stable diagnostic code recorded on the compaction record.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Transport(_) => "transport",
            Self::Timeout => "timeout",
            Self::Quota(_) => "quota",
            Self::EmptySummary => "empty_summary",
            Self::ContextExhausted => "context_exhausted",
            Self::RetryExhausted(_) => "retry_exhausted",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Value {
        serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": text }],
        })
    }

    fn assistant(text: &str) -> Value {
        serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }],
        })
    }

    #[test]
    fn vendored_prompt_matches_upstream_tag() {
        // Upstream templates/compact/prompt.md at rust-v0.150.0-alpha.8.
        assert!(
            SUMMARIZATION_PROMPT.starts_with("You are performing a CONTEXT CHECKPOINT COMPACTION.")
        );
        assert!(SUMMARIZATION_PROMPT.contains("Current progress and key decisions made"));
        assert!(SUMMARIZATION_PROMPT.contains("What remains to be done (clear next steps)"));
    }

    #[test]
    fn summary_prefix_matches_upstream_tag() {
        assert_eq!(
            SUMMARY_PREFIX.trim_end_matches('\n'),
            "Another language model started to solve this problem and produced a summary of its \
             thinking process. You also have access to the state of the tools that were used by \
             that language model. Use this to build on the work that has already been done and \
             avoid duplicating work. Here is the summary produced by the other language model, \
             use the information in this summary to assist with your own analysis:"
        );
    }

    #[test]
    fn approx_token_count_matches_upstream_rounding() {
        assert_eq!(approx_token_count(""), 0);
        assert_eq!(approx_token_count("a"), 1);
        assert_eq!(approx_token_count("abcd"), 1);
        assert_eq!(approx_token_count("abcde"), 2);
    }

    #[test]
    fn summarizer_input_appends_fixed_prompt_last() {
        let history = vec![user("hello"), assistant("hi")];
        let input = build_summarizer_input(&history);
        assert_eq!(input.len(), 3);
        assert_eq!(
            input[2]["content"][0]["text"].as_str().unwrap(),
            SUMMARIZATION_PROMPT
        );
        assert_eq!(input[2]["role"].as_str().unwrap(), "user");
    }

    #[test]
    fn replacement_keeps_user_messages_in_order_then_summary_last() {
        let source = vec![user("first"), assistant("noise"), user("second")];
        let items = build_replacement_items(&source, "SUMMARY BODY");
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["content"][0]["text"].as_str().unwrap(), "first");
        assert_eq!(items[1]["content"][0]["text"].as_str().unwrap(), "second");
        let last = items[2]["content"][0]["text"].as_str().unwrap();
        assert!(last.starts_with(SUMMARY_PREFIX.trim_end_matches('\n')));
        assert!(last.ends_with("SUMMARY BODY"));
    }

    #[test]
    fn assistant_messages_are_excluded_from_replacement() {
        let source = vec![assistant("only assistant")];
        let items = build_replacement_items(&source, "S");
        assert_eq!(items.len(), 1, "summary only");
    }

    #[test]
    fn prior_summary_is_not_reselected_as_user_message() {
        let prior = summary_text("older summary");
        let source = vec![user(&prior), user("real")];
        let messages = collect_user_messages(&source);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message, "real");
    }

    #[test]
    fn budget_selects_newest_first_and_truncates_the_boundary_message() {
        let old = "o".repeat(40); // 10 tokens
        let new = "n".repeat(40); // 10 tokens
        let messages = vec![
            CompactedUserMessage { message: old },
            CompactedUserMessage {
                message: new.clone(),
            },
        ];
        // Budget fits the newest message plus half of the older one.
        let items = build_compacted_history_with_limit(&messages, "S", 15);
        assert_eq!(items.len(), 3, "two user messages plus summary");
        let kept_old = items[0]["content"][0]["text"].as_str().unwrap();
        assert!(
            kept_old.contains("tokens truncated"),
            "older message truncated"
        );
        assert_eq!(items[1]["content"][0]["text"].as_str().unwrap(), new);
    }

    #[test]
    fn zero_budget_drops_every_user_message() {
        let messages = vec![CompactedUserMessage {
            message: "x".into(),
        }];
        let items = build_compacted_history_with_limit(&messages, "S", 0);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn truncation_stays_on_utf8_boundaries() {
        let text = "漢字".repeat(200); // 3 bytes per char
        let truncated = truncate_text_tokens(&text, 20);
        assert!(!truncated.is_empty());
        assert!(!truncated.contains('\u{FFFD}'));
    }

    #[test]
    fn context_window_retry_drops_oldest_then_stops_at_one() {
        let mut input = vec![user("a"), user("b"), user("c")];
        assert!(remove_oldest_summarizer_item(&mut input));
        assert_eq!(input[0]["content"][0]["text"].as_str().unwrap(), "b");
        assert!(remove_oldest_summarizer_item(&mut input));
        assert_eq!(input.len(), 1);
        assert!(
            !remove_oldest_summarizer_item(&mut input),
            "single remaining item surfaces the error instead of retrying"
        );
        assert_eq!(input.len(), 1);
    }

    #[test]
    fn failure_codes_are_stable() {
        assert_eq!(CompactionFailure::Timeout.code(), "timeout");
        assert_eq!(CompactionFailure::EmptySummary.code(), "empty_summary");
        assert_eq!(
            CompactionFailure::ContextExhausted.code(),
            "context_exhausted"
        );
    }
}
