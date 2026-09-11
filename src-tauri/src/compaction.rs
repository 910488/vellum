//! 上下文壓縮（doc/07）+ issue #4 Canonical / Grok Native helpers。
//!
//! 三段式：系統指示完整保留 / 早期往返摘要 / 近期 N 輪逐字保留。
//! 壓縮必須讓使用者看得到「保留了什麼、摘要了什麼」——所以核心產物是
//! 前後對照的 `CompactionPreview`，不是設定選項。
//!
//! 切分是純函式（`segment_items`、`build_preview`），可單測；
//! 真正的摘要生成（調模型）在審查章節接，這裡先產可還原的對照數字。
//!
//! Issue #4 adds:
//! - [`CanonicalCheckpoint`] schema + validation for Vellum Canonical
//! - [`sanitize_portable_history`] so third-party compactors never see opaque state
//! - [`GrokNativeCompactionConfig`] mirroring Grok Build session thresholds
//! - explicit ban on silent rolling truncation on the happy path

use crate::model::{CompactionPreview, CompactionSegment};
use crate::policy::{
    ContinuityKind, GROK_NATIVE_AUTO_COMPACT_THRESHOLD_PERCENT, GROK_NATIVE_TWO_PASS_DEFAULT,
};
use serde::{Deserialize, Serialize};

/// 預設保留的最後幾「輪」（一輪 = user + assistant）。doc/07 起始值 8。
pub const DEFAULT_KEEP_RECENT_TURNS: u32 = 8;
pub const COMPACT_USER_MESSAGE_MAX_TOKENS: usize = 20_000;
pub const OFFICIAL_SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

/// 各段的視覺色票（與 CSS 變數同名，doc/07）。
pub const TONE_SYSTEM: &str = "var(--haze)";
pub const TONE_SUMMARY: &str = "var(--coral)";
pub const TONE_RECENT: &str = "var(--honey)";
pub const TONE_TOOL: &str = "var(--lavender)";

/// 一個對話 item 的角色判定。系統指示 / 使用者 / 助理 / 工具。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemRole {
    System,
    User,
    Assistant,
    Tool,
    Other,
}

/// 從一個 Responses/Chat item 推斷角色（純函式）。
pub fn classify_item(item: &serde_json::Value) -> ItemRole {
    // Responses wire：item.type 或 role
    let item_type = item.get("type").and_then(serde_json::Value::as_str);
    let role = item.get("role").and_then(serde_json::Value::as_str);
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

/// 三段切分結果。before = 原始佔比，after = 壓縮後佔比（相對單位）。
#[derive(Debug, Clone, PartialEq)]
pub struct Segmentation {
    /// 系統指示（完整保留）。
    pub system_count: usize,
    /// 早期往返（要摘要）。
    pub early_count: usize,
    /// 近期往返（逐字保留）。
    pub recent_count: usize,
    /// 一輪定義為相鄰的 user+assistant（或單獨的 assistant）。
    /// recent_turns 是實際保留的輪數（可能 < 預設，因為總共沒那麼多）。
    pub recent_turns: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompactionParts {
    pub system: Vec<serde_json::Value>,
    pub early: Vec<serde_json::Value>,
    pub recent: Vec<serde_json::Value>,
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

/// Issue #4 portable semantic checkpoint produced by Vellum Canonical.
///
/// Continuity is always semantic — never claims provider-private reasoning
/// was retained. `retained_tail_items` must already be sanitized.
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
    pub retained_tail_items: Vec<serde_json::Value>,
    /// Always `semantic` for Canonical checkpoints.
    #[serde(default = "canonical_continuity_kind")]
    pub continuity_kind: ContinuityKind,
}

fn canonical_continuity_kind() -> ContinuityKind {
    ContinuityKind::Semantic
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

// ---------------------------------------------------------------------------
// Tool-result pruning（Grok Build `[compaction.pruning]` 的移植）
// ---------------------------------------------------------------------------

/// Grok Build `[compaction.pruning] keep_last_n_turns = 3`.
pub const PRUNING_KEEP_LAST_N_TURNS: usize = 3;
/// Grok Build `[compaction.pruning] soft_trim_threshold = 4000`.
pub const PRUNING_SOFT_TRIM_THRESHOLD: usize = 4_000;
/// Grok Build `[compaction.pruning] soft_trim_head = 1500`.
pub const PRUNING_SOFT_TRIM_HEAD: usize = 1_500;
/// Grok Build `[compaction.pruning] soft_trim_tail = 1500`.
pub const PRUNING_SOFT_TRIM_TAIL: usize = 1_500;
/// Grok Build `[compaction.pruning] hard_clear_age_turns = 10`.
pub const PRUNING_HARD_CLEAR_AGE_TURNS: usize = 10;

/// Tool-result pruning knobs, mirroring Grok Build's `[compaction.pruning]`.
///
/// This is **not** the banned happy-path rolling truncation: no conversation
/// item is ever dropped, and the ordering and the call/output pairing are
/// untouched. Only the *body* of an old tool result shrinks, which is the one
/// part of the window that grows without bound in a tool-heavy session and
/// that the model no longer reads once it has moved on.
///
/// Durability is unaffected — pruning applies to the outbound upstream body
/// only. `original_request` still carries the verbatim result into history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultPruningConfig {
    pub enabled: bool,
    pub keep_last_n_turns: usize,
    pub soft_trim_threshold: usize,
    pub soft_trim_head: usize,
    pub soft_trim_tail: usize,
    pub hard_clear_age_turns: usize,
}

impl Default for ToolResultPruningConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            keep_last_n_turns: PRUNING_KEEP_LAST_N_TURNS,
            soft_trim_threshold: PRUNING_SOFT_TRIM_THRESHOLD,
            soft_trim_head: PRUNING_SOFT_TRIM_HEAD,
            soft_trim_tail: PRUNING_SOFT_TRIM_TAIL,
            hard_clear_age_turns: PRUNING_HARD_CLEAR_AGE_TURNS,
        }
    }
}

impl ToolResultPruningConfig {
    /// `VELLUM_TOOL_RESULT_PRUNING=0` (also `false`/`off`/`no`) disables pruning
    /// without a rebuild. It is an operational kill switch, and the lever the
    /// live A/B test uses to measure what pruning actually saves upstream.
    pub fn from_env() -> Self {
        let mut config = Self::default();
        if let Ok(raw) = std::env::var("VELLUM_TOOL_RESULT_PRUNING") {
            if matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            ) {
                config.enabled = false;
            }
        }
        config
    }
}

/// What a pruning pass actually did, for logging and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruningOutcome {
    pub soft_trimmed: usize,
    pub hard_cleared: usize,
    /// Characters removed from the outbound body.
    pub chars_removed: usize,
}

impl PruningOutcome {
    pub fn touched_any(&self) -> bool {
        self.soft_trimmed > 0 || self.hard_cleared > 0
    }
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// Keep the head and tail, elide the middle. Slices on char boundaries so
/// multi-byte text (the whole CJK corpus this app is written for) cannot panic
/// or emit invalid UTF-8.
fn soft_trim(text: &str, head: usize, tail: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= head.saturating_add(tail) {
        return text.to_string();
    }
    let removed = chars.len() - head - tail;
    let head_text: String = chars[..head].iter().collect();
    let tail_text: String = chars[chars.len() - tail..].iter().collect();
    format!("{head_text}\n\n[… {removed} characters elided by Vellum tool-result pruning …]\n\n{tail_text}")
}

fn hard_clear_placeholder(original_chars: usize, age_turns: usize) -> String {
    format!(
        "[tool result cleared by Vellum tool-result pruning — {original_chars} characters, \
         {age_turns} turns old]"
    )
}

/// Is this item a user message (a turn boundary) in either wire format?
fn is_user_turn_boundary(item: &serde_json::Value) -> bool {
    item.get("role").and_then(serde_json::Value::as_str) == Some("user")
}

/// Is this a tool result carrying a prunable string body, in either wire format?
///
/// Chat-wire tool messages are `{"role": "tool", "content": "…"}`; Responses-wire
/// results are matched by [`is_tool_output`].
fn prunable_body_field(item: &serde_json::Value) -> Option<&'static str> {
    if is_tool_output(item) && item.get("output").is_some_and(serde_json::Value::is_string) {
        return Some("output");
    }
    if item.get("role").and_then(serde_json::Value::as_str) == Some("tool")
        && item
            .get("content")
            .is_some_and(serde_json::Value::is_string)
    {
        return Some("content");
    }
    None
}

/// Shrink the bodies of old tool results in an outbound request, in place.
///
/// Accepts either wire format: a Responses `input` array or a Chat `messages`
/// array. Age is measured in user turns, so an entire tool loop inside one user
/// turn prunes identically on every round — the prompt prefix stays byte-stable
/// across the rounds that dominate a tool-heavy session.
pub fn prune_tool_results(
    request: &mut serde_json::Value,
    config: &ToolResultPruningConfig,
) -> PruningOutcome {
    let mut outcome = PruningOutcome::default();
    if !config.enabled {
        return outcome;
    }
    let Some(key) = ["input", "messages"]
        .into_iter()
        .find(|key| request.get(key).is_some_and(serde_json::Value::is_array))
    else {
        return outcome;
    };
    let Some(items) = request
        .get_mut(key)
        .and_then(serde_json::Value::as_array_mut)
    else {
        return outcome;
    };

    let total_turns = items
        .iter()
        .filter(|item| is_user_turn_boundary(item))
        .count();
    // Nothing can be older than keep_last_n_turns yet.
    if total_turns <= config.keep_last_n_turns {
        return outcome;
    }

    let mut seen_users = 0usize;
    for item in items.iter_mut() {
        if is_user_turn_boundary(item) {
            seen_users += 1;
            continue;
        }
        let Some(field) = prunable_body_field(item) else {
            continue;
        };
        let age_turns = total_turns.saturating_sub(seen_users);
        if age_turns < config.keep_last_n_turns {
            continue;
        }
        let Some(body) = item.get(field).and_then(serde_json::Value::as_str) else {
            continue;
        };
        let original_chars = char_len(body);
        let replacement = if age_turns >= config.hard_clear_age_turns {
            outcome.hard_cleared += 1;
            hard_clear_placeholder(original_chars, age_turns)
        } else if original_chars > config.soft_trim_threshold {
            outcome.soft_trimmed += 1;
            soft_trim(body, config.soft_trim_head, config.soft_trim_tail)
        } else {
            continue;
        };
        let saved = original_chars.saturating_sub(char_len(&replacement));
        // A placeholder longer than the result it replaces is not a saving.
        if saved == 0 {
            if age_turns >= config.hard_clear_age_turns {
                outcome.hard_cleared -= 1;
            } else {
                outcome.soft_trimmed -= 1;
            }
            continue;
        }
        outcome.chars_removed += saved;
        item[field] = serde_json::Value::String(replacement);
    }
    outcome
}

/// Grok Build session-style native compaction knobs (not OpenAI compact API).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GrokNativeCompactionConfig {
    /// Mirrors `[session] auto_compact_threshold_percent`.
    pub auto_compact_threshold_percent: u32,
    /// Mirrors `[features] two_pass_compaction`.
    pub two_pass_compaction: bool,
    /// When true, Vellum must not auto-compact on the Grok native path.
    pub disabled: bool,
}

impl Default for GrokNativeCompactionConfig {
    fn default() -> Self {
        Self {
            auto_compact_threshold_percent: GROK_NATIVE_AUTO_COMPACT_THRESHOLD_PERCENT,
            two_pass_compaction: GROK_NATIVE_TWO_PASS_DEFAULT,
            disabled: false,
        }
    }
}

/// Result of a Grok-native compaction decision (no OpenAI `/responses/compact`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GrokNativeCompactionDecision {
    pub should_compact: bool,
    pub threshold_percent: u32,
    pub two_pass: bool,
    pub continuity_kind: ContinuityKind,
    pub reason: String,
}

impl GrokNativeCompactionConfig {
    pub fn decide(
        &self,
        active_context_tokens: u64,
        pending_tokens: u64,
        window_tokens: u64,
    ) -> GrokNativeCompactionDecision {
        let should = crate::policy::should_compact_grok_native(
            active_context_tokens,
            pending_tokens,
            window_tokens,
            self.auto_compact_threshold_percent,
            self.disabled,
        );
        GrokNativeCompactionDecision {
            should_compact: should,
            threshold_percent: self.auto_compact_threshold_percent,
            two_pass: self.two_pass_compaction,
            continuity_kind: ContinuityKind::Native,
            reason: if self.disabled {
                "grok_native_compaction_disabled".into()
            } else if should {
                "grok_native_threshold_exceeded".into()
            } else {
                "grok_native_below_threshold".into()
            },
        }
    }
}

/// Grok Build's native compaction is a client-side session operation. It uses
/// the configured inference backend (`/responses`, `/chat/completions`, or
/// `/messages`) and does not call an OpenAI-style `/responses/compact`
/// endpoint. Keep the prompt version explicit so endpoint inventory changes
/// cannot silently change the journal semantics.
pub const GROK_BUILD_COMPACTION_PROMPT_VERSION: &str = "grok-build-0.2.112-nine-section";

/// Faithful, bounded form of Grok Build's nine-section successor prompt.
///
/// The wording and section contract mirror
/// `xai-grok-shell/session/helpers/session_compact.rs`. Vellum deliberately
/// does not request tools during this auxiliary inference call.
pub fn grok_build_compaction_prompt() -> String {
    r#"Your task is to produce a faithful, concise summary of the conversation so far so that a successor assistant can continue the work seamlessly after the earlier turns are discarded. The successor will see the user's original query plus this summary. Capture the user's explicit requests, recent actions, key technical details, file paths, commands, configuration, architectural decisions, errors, and remaining work. Prefer tight prose and short references over long verbatim dumps.

CRITICAL: If earlier turns contain a prior compaction summary, treat it as authoritative for the early history and carry its still-relevant information forward.

Think privately before writing. Output only one <summary>...</summary> block with every numbered heading:
1. Primary Request and Intent
2. Key Technical Concepts
3. Files and Code Sections
4. Errors and Fixes
5. Problem Solving
6. All User Messages
7. Pending Tasks
8. Current Work
9. Optional Next Step

Do not call tools. Do not emit analysis outside the summary block."#
        .to_string()
}

/// Remove private drafting/control markup from a Grok Build compaction sample.
pub fn normalize_grok_build_summary(raw: &str) -> Option<String> {
    let without_tokens = ["<|eos|>", "<|endoftext|>", "<|im_end|>"]
        .iter()
        .fold(raw.to_string(), |text, token| text.replace(token, ""));
    let trimmed = without_tokens.trim();
    let extracted = trimmed
        .find("<summary>")
        .and_then(|start| {
            trimmed.rfind("</summary>").and_then(|end| {
                (end > start).then(|| trimmed[start + "<summary>".len()..end].trim().to_string())
            })
        })
        .unwrap_or_else(|| {
            let mut value = trimmed.to_string();
            while let Some(start) = value.find("<analysis>") {
                let Some(relative_end) = value[start..].find("</analysis>") else {
                    value.truncate(start);
                    break;
                };
                let end = start + relative_end + "</analysis>".len();
                value.replace_range(start..end, "");
            }
            value.trim().to_string()
        });
    (extracted.chars().count() >= 500).then_some(extracted)
}

/// Build the same successor-facing order used by Grok Build:
/// recent real-user turn and its tool tail first, then the compaction summary.
///
/// Codex re-injects its system instructions and tools on the next request, so
/// this window contains only Responses input items. The recent suffix is kept
/// byte-for-byte (including same-realm encrypted reasoning); cross-realm
/// replay must use the journal's separately sanitized portable window.
pub fn build_grok_native_window(
    source: &[serde_json::Value],
    summary: &str,
    max_tail_tokens: u64,
) -> Vec<serde_json::Value> {
    let source = source
        .iter()
        .filter(|item| {
            item.get("type").and_then(serde_json::Value::as_str) != Some("compaction_trigger")
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut window = bound_retained_tail(&source, 1, max_tail_tokens);
    window.push(serde_json::json!({
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": format!(
                "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\nSummary:\n{summary}"
            )
        }]
    }));
    window
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

fn is_opaque_field_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    OPAQUE_FIELD_KEYS
        .iter()
        .any(|blocked| lower == *blocked || lower.contains("encrypted"))
}

fn find_opaque_field_path(value: &serde_json::Value, path: &str) -> Option<(String, String)> {
    match value {
        serde_json::Value::Array(values) => values
            .iter()
            .enumerate()
            .find_map(|(index, child)| find_opaque_field_path(child, &format!("{path}[{index}]"))),
        serde_json::Value::Object(object) => {
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
pub fn strip_opaque_fields(mut value: serde_json::Value) -> serde_json::Value {
    match &mut value {
        serde_json::Value::Array(values) => {
            for child in values.iter_mut() {
                *child = strip_opaque_fields(std::mem::take(child));
            }
        }
        serde_json::Value::Object(object) => {
            object.retain(|key, _| !is_opaque_field_key(key));
            // Drop entire reasoning/compaction opaque items that only carried ciphertext.
            let item_type = object
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if matches!(item_type, "reasoning" | "compaction") {
                let has_visible = object.contains_key("summary")
                    || object.contains_key("content")
                    || object.contains_key("text");
                if !has_visible {
                    return serde_json::Value::Null;
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

/// Build portable visible history for Vellum Canonical / cross-provider fork.
///
/// Removes encrypted reasoning, auth tokens, provider-private compaction items,
/// and system/developer instructions that Codex re-injects itself.
pub fn sanitize_portable_history(items: &[serde_json::Value]) -> Vec<serde_json::Value> {
    items
        .iter()
        .filter(|item| {
            !matches!(classify_item(item), ItemRole::System)
                && !is_summary_item(item)
                && !matches!(
                    item.get("type").and_then(serde_json::Value::as_str),
                    Some("compaction" | "compaction_trigger")
                )
        })
        .cloned()
        .map(strip_opaque_fields)
        .filter(|value| !value.is_null())
        .map(strip_images)
        .collect()
}

/// Validate a Canonical checkpoint: required fields + continuity_kind=semantic.
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
    // Retained tail must not reintroduce opaque state.
    for (index, item) in checkpoint.retained_tail_items.iter().enumerate() {
        if let Some((key, path)) =
            find_opaque_field_path(item, &format!("retained_tail_items[{index}]"))
        {
            return Err(format!(
                "canonical {path} still contains opaque field `{key}`"
            ));
        }
    }
    validate_retained_tool_pairing(&checkpoint.retained_tail_items)?;
    Ok(())
}

/// A canonical tail is durable completed history. Unlike the live pending
/// suffix, it must never contain only one side of a structured tool exchange.
/// Persisting an orphan call makes every later Chat-compatible continuation
/// fail before reaching the model; persisting an orphan output loses the action
/// that produced the result. Built-in web search is excluded because providers
/// may represent it as a self-contained call item without a separate output.
fn validate_retained_tool_pairing(items: &[serde_json::Value]) -> Result<(), String> {
    let mut pending = std::collections::HashMap::<String, usize>::new();
    for (index, item) in items.iter().enumerate() {
        let item_type = item
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let requires_output = matches!(
            item_type,
            "function_call"
                | "local_shell_call"
                | "custom_tool_call"
                | "tool_search_call"
                | "computer_call"
        );
        if requires_output {
            let id = tool_pair_id(item).ok_or_else(|| {
                format!("canonical retained tool call at index {index} has no call_id")
            })?;
            *pending.entry(id.to_string()).or_insert(0) += 1;
            continue;
        }
        if !is_tool_output(item) {
            continue;
        }
        let id = tool_pair_id(item).ok_or_else(|| {
            format!("canonical retained tool output at index {index} has no call_id")
        })?;
        let Some(count) = pending.get_mut(id) else {
            return Err(format!(
                "canonical retained tool output `{id}` at index {index} has no preceding call"
            ));
        };
        *count -= 1;
        if *count == 0 {
            pending.remove(id);
        }
    }
    if pending.is_empty() {
        return Ok(());
    }
    let mut ids = pending.into_keys().collect::<Vec<_>>();
    ids.sort();
    Err(format!(
        "canonical retained tool call(s) have no matching output: {}",
        ids.join(", ")
    ))
}

/// Lift a usable [`StructuredSummary`] into the issue #4 Canonical schema.
pub fn canonical_checkpoint_from_summary(
    summary: &StructuredSummary,
    retained_tail_items: Vec<serde_json::Value>,
) -> Result<CanonicalCheckpoint, String> {
    if !summary.is_usable() {
        return Err("structured summary is not usable for a canonical checkpoint".into());
    }
    let exact_next_step = summary
        .next_steps
        .iter()
        .find(|step| !step.trim().is_empty() && !is_transient_runtime_claim(step))
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
    let transient_blockers = summary
        .blocked
        .iter()
        .chain(summary.unresolved.iter())
        .filter(|value| is_transient_runtime_claim(value))
        .cloned()
        .collect::<Vec<_>>();
    let checkpoint = CanonicalCheckpoint {
        goal: bounded_text(&summary.goal, 2_048),
        constraints: bounded_strings(&summary.constraints, 8, 384),
        decisions: bounded_strings(&summary.decisions, 8, 384),
        discovered_facts,
        failed_attempts: {
            let mut failed = bounded_strings(&summary.errors, 8, 384);
            failed.extend(bounded_strings(&transient_blockers, 8, 384));
            failed.truncate(12);
            failed
        },
        files_changed,
        tool_state: Vec::new(),
        pending_tasks: bounded_strings(&summary.in_progress, 8, 384),
        unresolved_blockers: {
            let mut blocked = bounded_strings(&summary.blocked, 8, 384);
            blocked.extend(bounded_strings(&summary.unresolved, 8, 384));
            blocked.retain(|value| !is_transient_runtime_claim(value));
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

/// Do not promote a one-turn harness limitation into durable task state.
fn is_transient_runtime_claim(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    [
        "cannot access local files",
        "unable to access local files",
        "cannot run git",
        "unable to run git",
        "tools are unavailable",
        "tool is unavailable",
        "restart this task",
        "restart the task",
        "relaunch the task",
        "無法存取本地檔案",
        "無法執行 git",
        "工具不可用",
        "重新啟動這個 task",
        "重新啟動任務",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
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

/// Hard cap for retained recent tail tokens on Vellum Canonical (issue #4 B-1).
pub const CANONICAL_MAX_TAIL_TOKENS: u64 = 12_000;

/// Call/output ids that must stay paired in a retained tail.
fn tool_pair_id(item: &serde_json::Value) -> Option<&str> {
    item.get("call_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| item.get("id").and_then(serde_json::Value::as_str))
}

/// Tool call kinds recognized by the Codex adapter + compaction (comment 5149622796 P1-2).
pub fn is_tool_call(item: &serde_json::Value) -> bool {
    matches!(
        item.get("type").and_then(serde_json::Value::as_str),
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
pub fn is_tool_output(item: &serde_json::Value) -> bool {
    matches!(
        item.get("type").and_then(serde_json::Value::as_str),
        Some(
            "function_call_output"
                | "local_shell_call_output"
                | "custom_tool_call_output"
                | "tool_search_output"
                | "computer_call_output"
                // Some providers emit web_search results as call items only; keep
                // a dedicated output type if present in history.
                | "web_search_call_output"
        )
    )
}

/// Expand a recent slice so every tool call keeps its matching output (and vice versa)
/// when the pair exists in `full`. Uses **multiset / newest-first occurrence** matching
/// so duplicate identical messages only pull as many occurrences as appear in `recent`
/// (issue #4 comment 5144958590 P1-4).
///
/// Pair completion is **occurrence-scoped**: a reused `call_id` in an older exchange is
/// not pulled in when only the newer occurrence is selected (comment 5149433523 P1-2).
pub fn enforce_tool_call_output_pairing(
    full: &[serde_json::Value],
    recent: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut recent_remaining: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for item in recent {
        let key = serde_json::to_string(item).unwrap_or_default();
        *recent_remaining.entry(key).or_insert(0) += 1;
    }
    // Mark the *newest* matching occurrences (walk full reverse) for multiset membership.
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
    // Expand full positional spans (control/assistant rows between call and
    // output stay in the window — comment 5149622796 P1-3).
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

/// Occurrence-scoped tool exchange spans: each call pairs only with outputs of the
/// same `call_id` that appear before the next call with that id (comment 5149433523).
/// Overlapping parallel spans are merged.
pub fn tool_exchange_spans(items: &[serde_json::Value]) -> Vec<(usize, usize)> {
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

/// Atomic retained-tail unit: a contiguous **source slice** in exact original
/// order (issue #4 comment 5146162101). Parallel tool spans merge into one batch.
#[derive(Debug, Clone, PartialEq)]
pub struct TailUnit {
    /// Exact copy of `source[start..end)` preserving interleaving and order.
    pub items: Vec<serde_json::Value>,
}

impl TailUnit {
    pub fn tokens(&self) -> u64 {
        estimate_tokens(&self.items)
    }

    pub fn into_items(self) -> Vec<serde_json::Value> {
        self.items
    }

    pub fn is_orphan_output_only(&self) -> bool {
        self.items.len() == 1 && is_tool_output(&self.items[0])
    }

    pub fn starts_with_tool_output(&self) -> bool {
        self.items.first().is_some_and(is_tool_output)
    }
}

/// Flatten packed units back to items (must equal source when packing is complete).
pub fn flatten_tail_units(units: &[TailUnit]) -> Vec<serde_json::Value> {
    units
        .iter()
        .flat_map(|unit| unit.items.iter().cloned())
        .collect()
}

/// Pack items into order-preserving atomic batches.
///
/// Each tool call forms an **occurrence-scoped** span through matching outputs
/// before the next same-`call_id` call. Overlapping parallel spans merge so
/// layouts like `[call A, call B, out A, out B]` stay one contiguous source
/// slice without reordering (comments 5146162101 B-1, 5149433523 P1-2).
pub fn pack_tail_units(items: &[serde_json::Value]) -> Vec<TailUnit> {
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let merged = tool_exchange_spans(items);
    // Emit units covering 0..n without holes or reordering.
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
        // Single-item unit (message, orphan output, unpaired control).
        units.push(TailUnit {
            items: vec![items[i].clone()],
        });
        i += 1;
    }
    units
}

/// Start index of the retained recent tail as a **true contiguous source suffix**
/// (`portable_items[start..]`). Issue #4 comment 5149622796 B-1.
pub fn retained_tail_start_index(
    portable_items: &[serde_json::Value],
    keep_recent_turns: u32,
    max_tail_tokens: u64,
) -> usize {
    let n = portable_items.len();
    if n == 0 {
        return 0;
    }
    let max_tail_tokens = max_tail_tokens.max(256);

    // Turn-based recent window start (user-turn indices).
    let user_starts: Vec<usize> = portable_items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| matches!(classify_item(item), ItemRole::User).then_some(index))
        .collect();
    let keep = keep_recent_turns as usize;
    let mut window_start = if user_starts.is_empty() {
        // Pure tool / control sequences: consider the full source.
        0
    } else if user_starts.len() <= keep {
        user_starts[0]
    } else {
        user_starts[user_starts.len() - keep]
    };

    // Expand left so a tool span straddling the turn boundary stays whole.
    for &(s, e) in &tool_exchange_spans(portable_items) {
        if s < window_start && e > window_start {
            window_start = s;
        }
    }

    // Pack the candidate window and keep a contiguous unit suffix under the cap.
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
    // Drop leading orphan-output units (still a newer contiguous suffix).
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
/// Guarantees: `estimate_tokens(tail) <= max_tail_tokens` (or empty), never
/// reorders items, never begins with tool output after selection, and retained
/// units form a **contiguous recent source suffix** (`source[k..]`).
pub fn bound_retained_tail(
    portable_items: &[serde_json::Value],
    keep_recent_turns: u32,
    max_tail_tokens: u64,
) -> Vec<serde_json::Value> {
    let start = retained_tail_start_index(portable_items, keep_recent_turns, max_tail_tokens);
    let out = portable_items[start..].to_vec();
    let max_tail_tokens = max_tail_tokens.max(256);
    debug_assert!(estimate_tokens(&out) <= max_tail_tokens || out.is_empty());
    debug_assert!(
        out.first().map(|i| !is_tool_output(i)).unwrap_or(true),
        "retained tail must not start with tool output"
    );
    out
}

/// Split request input into prior active history vs pending current-turn items.
/// Pending = last non-summary user message + everything after it (tools, etc.),
/// plus immediately preceding model-switch markers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActivePendingSplit {
    pub active_history: Vec<serde_json::Value>,
    pub pending_items: Vec<serde_json::Value>,
}

fn is_model_switch_marker(item: &serde_json::Value) -> bool {
    let role = item.get("role").and_then(serde_json::Value::as_str);
    if !matches!(role, Some("developer" | "system")) {
        return false;
    }
    let text = item_text(item).unwrap_or_default();
    text.contains("<model_switch>")
}

fn is_pending_user(item: &serde_json::Value) -> bool {
    item.get("role").and_then(serde_json::Value::as_str) == Some("user") && !is_summary_item(item)
}

/// Split active (compactable prior history) from pending current-turn items.
pub fn split_active_and_pending(items: &[serde_json::Value]) -> ActivePendingSplit {
    if items.is_empty() {
        return ActivePendingSplit::default();
    }
    // Ignore trailing compaction_trigger when locating the pending user.
    let searchable: Vec<(usize, &serde_json::Value)> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.get("type").and_then(serde_json::Value::as_str) != Some("compaction_trigger")
        })
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
///
/// Both slices are **positional source cuts** (comment 5149622796 B-1):
/// `early == source[..k]`, `tail == source[k..]`. Duplicate payloads (e.g. two
/// `"Continue"` turns) keep chronology — early is never rebuilt by consuming
/// the earliest equal JSON multiset membership of the tail.
pub fn split_canonical_source(
    portable_items: &[serde_json::Value],
    keep_recent_turns: u32,
    max_tail_tokens: u64,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let start = retained_tail_start_index(portable_items, keep_recent_turns, max_tail_tokens);
    let early = portable_items[..start].to_vec();
    let tail = portable_items[start..].to_vec();
    debug_assert_eq!(early.len() + tail.len(), portable_items.len());
    (early, tail)
}

/// Issue #4 B-1 shrink gate: the installed canonical window must be strictly
/// smaller than the portable source window.
///
/// Output/tool reserves are already accounted for by
/// the retired Canonical scheduler. Adding them again here compares
/// future capacity against historical input and makes every small forced-window
/// compaction mathematically impossible whenever reserves exceed source tokens.
pub fn canonical_replacement_frees_context(
    source_tokens: u64,
    replacement_tokens: u64,
    _output_reserve_tokens: u64,
    _tool_reserve_tokens: u64,
) -> bool {
    source_tokens > 0 && replacement_tokens < source_tokens
}

/// Install a validated Canonical checkpoint as portable conversation items.
/// Order: **summary → retained active tail** (pending user/tool appended by caller).
/// Summary payload omits the retained tail so tokens are not double-counted.
pub fn install_canonical_checkpoint_items(
    checkpoint: &CanonicalCheckpoint,
) -> Vec<serde_json::Value> {
    let mut summary_only = checkpoint.clone();
    summary_only.retained_tail_items = Vec::new();
    let mut items = vec![serde_json::json!({
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": format!(
                "{}\n\
                 This is durable historical task state, not a new user request. The latest \
                 user message after this checkpoint has priority over `exactNextStep`. Runtime \
                 capabilities, available tools, permissions, network access, and transient \
                 service availability must be determined from the current request; never infer \
                 that they remain unavailable from an earlier error or blocker.\n{}",
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

/// Happy-path rolling truncation is forbidden (issue #4). Emergency truncation
/// is only legal when policy explicitly allows it after compaction failure.
pub fn may_silently_roll_truncate() -> bool {
    false
}

/// 把 items 陣列切成三段（純函式）。
///
/// - 系統段：所有 system/developer items。
/// - 近期段：最後 `keep_recent_turns` 輪（一輪 = 一個 user 後續跟著 assistant/tool，
///   直到下一個 user）。
/// - 早期段：系統與近期之間的全部。
pub fn segment_items(items: &[serde_json::Value], keep_recent_turns: u32) -> Segmentation {
    let system_count = items
        .iter()
        .filter(|i| matches!(classify_item(i), ItemRole::System))
        .count();

    // 找最後 keep_recent_turns 個「user 起頭的輪」的起點索引。
    let mut user_starts: Vec<usize> = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        if matches!(classify_item(item), ItemRole::User) {
            user_starts.push(idx);
        }
    }
    let keep = keep_recent_turns as usize;
    let recent_start = if user_starts.is_empty() {
        items.len()
    } else if user_starts.len() <= keep {
        // 全部都是「近期」——沒有早期可摘要。
        user_starts[0]
    } else {
        user_starts[user_starts.len() - keep]
    };

    // recent 段：recent_start..len（但不含系統段）。
    let recent_count = items[recent_start..]
        .iter()
        .filter(|i| !matches!(classify_item(i), ItemRole::System))
        .count();

    let early_count = items
        .iter()
        .enumerate()
        .filter(|(idx, i)| {
            *idx >= system_count.min(items.len()).min(items.len())
                && *idx < recent_start
                && !matches!(classify_item(i), ItemRole::System)
        })
        .count();

    // 實際保留的輪數（user 數，取使用者的與 keep 的較小值）。
    let recent_turns = user_starts.len().min(keep).min(u32::MAX as usize) as u32;

    Segmentation {
        system_count,
        early_count,
        recent_count,
        recent_turns,
    }
}

pub fn split_items(items: &[serde_json::Value], keep_recent_turns: u32) -> CompactionParts {
    let user_starts = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| matches!(classify_item(item), ItemRole::User).then_some(index))
        .collect::<Vec<_>>();
    let keep = keep_recent_turns as usize;
    let recent_start = if user_starts.is_empty() {
        items.len()
    } else if user_starts.len() <= keep {
        user_starts[0]
    } else {
        user_starts[user_starts.len() - keep]
    };
    let mut parts = CompactionParts {
        system: Vec::new(),
        early: Vec::new(),
        recent: Vec::new(),
    };
    for (index, item) in items.iter().cloned().enumerate() {
        if matches!(classify_item(&item), ItemRole::System) {
            parts.system.push(item);
        } else if index >= recent_start {
            parts.recent.push(item);
        } else {
            parts.early.push(item);
        }
    }
    parts
}

pub fn summary_prompt(early: &[serde_json::Value]) -> String {
    format!(
        "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary \
         for another LLM that will resume the task. Preserve facts and exact file paths, \
         symbols, commands, test outcomes, errors, constraints, user preferences, key \
         decisions, rejected approaches, and the next executable step. Do not invent completion. \
         Do not infer persistent tool, filesystem, network, sandbox, authentication, or service \
         limitations from a transient failure in one turn. Record such incidents only in errors; \
         put an item in blocked/unresolved only when it is still a current, durable blocker. The \
         successor model's current tool definitions and newest user message are authoritative and \
         supersede stale execution-environment observations and next steps. Output only strict \
         JSON using exactly these camelCase keys: \
         goal (string), acceptanceCriteria, constraints, userPreferences, done, inProgress, \
         blocked, decisions, changedFiles, relevantFiles, commands, tests, unresolved, errors, \
         criticalContext, references, nextSteps (arrays of strings). Be concise but make the \
         handoff sufficient to continue without re-exploring the codebase.\n\nConversation:\n{}",
        serde_json::to_string(early).unwrap_or_else(|_| "[]".into())
    )
}

pub fn parse_structured_summary(value: &serde_json::Value) -> Option<StructuredSummary> {
    let direct = if value.is_object() {
        value.clone()
    } else {
        return None;
    };
    serde_json::from_value(direct).ok().or_else(|| {
        value
            .get("choices")
            .and_then(serde_json::Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.pointer("/message/content"))
            .and_then(serde_json::Value::as_str)
            .and_then(|content| serde_json::from_str(content).ok())
    })
}

/// Parse a structured checkpoint from model text while still requiring a
/// syntactically valid JSON object. Reasoning models commonly wrap the final
/// object in a markdown fence, a short preface, or a `<think>` block.
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

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(unfenced) {
        if let Some(summary) = parse_structured_summary(&value) {
            return Some(summary);
        }
    }

    // Find balanced JSON objects outside quoted strings. Try every complete
    // object because a reasoning preamble may itself contain brace examples.
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
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) {
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

/// Split a large transcript into bounded prompts. Whole items are preserved
/// whenever possible. A single oversized tool result is represented by a
/// head/tail excerpt rather than making every compaction request exceed the
/// selected summarizer's context window.
pub fn chunk_compaction_items(
    items: &[serde_json::Value],
    max_tokens: usize,
) -> Vec<Vec<serde_json::Value>> {
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

fn oversized_item_excerpt(item: &serde_json::Value, max_tokens: usize) -> serde_json::Value {
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
            // Later chunks contain the most recent task state.
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

pub fn summary_item(summary: &StructuredSummary) -> serde_json::Value {
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

pub fn is_summary_item(item: &serde_json::Value) -> bool {
    item.get("role").and_then(serde_json::Value::as_str) == Some("user")
        && item_text(item).is_some_and(|text| text.starts_with(OFFICIAL_SUMMARY_PREFIX))
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

/// Build a Codex-compatible compact endpoint output without forging an
/// OpenAI-owned encrypted `compaction` item. Codex will re-inject its current
/// canonical initial context after receiving these ResponseItems.
pub fn build_remote_compact_output(
    history: &[serde_json::Value],
    summary: &StructuredSummary,
) -> Vec<serde_json::Value> {
    let mut output = collect_recent_user_messages(history, COMPACT_USER_MESSAGE_MAX_TOKENS);
    output.push(summary_item(summary));
    output
}

/// Build the conversation transcript that may be summarized by a third-party
/// model. Canonical system/developer instructions are re-injected by Codex and
/// must not be copied into a provider-owned checkpoint.
pub fn compactable_history(items: &[serde_json::Value]) -> Vec<serde_json::Value> {
    items
        .iter()
        .filter(|item| {
            !matches!(classify_item(item), ItemRole::System)
                && !is_summary_item(item)
                && !matches!(
                    item.get("type").and_then(serde_json::Value::as_str),
                    Some("reasoning" | "compaction" | "compaction_trigger")
                )
        })
        .cloned()
        .map(strip_provider_owned_content)
        .collect()
}

pub fn collect_recent_user_messages(
    items: &[serde_json::Value],
    max_tokens: usize,
) -> Vec<serde_json::Value> {
    if max_tokens == 0 {
        return Vec::new();
    }
    let mut remaining = max_tokens;
    let mut selected = Vec::new();
    for item in items.iter().rev() {
        if item.get("role").and_then(serde_json::Value::as_str) != Some("user")
            || is_summary_item(item)
        {
            continue;
        }
        let sanitized = strip_images(item.clone());
        let tokens = estimate_tokens(std::slice::from_ref(&sanitized)) as usize;
        if tokens <= remaining {
            selected.push(sanitized);
            remaining = remaining.saturating_sub(tokens);
        } else {
            break;
        }
    }
    selected.reverse();
    selected
}

fn strip_images(mut value: serde_json::Value) -> serde_json::Value {
    match &mut value {
        serde_json::Value::Array(values) => {
            for child in values.iter_mut() {
                *child = strip_images(std::mem::take(child));
            }
        }
        serde_json::Value::Object(object) => {
            if matches!(
                object.get("type").and_then(serde_json::Value::as_str),
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

fn strip_provider_owned_content(mut value: serde_json::Value) -> serde_json::Value {
    match &mut value {
        serde_json::Value::Array(values) => {
            for child in values.iter_mut() {
                *child = strip_provider_owned_content(std::mem::take(child));
            }
        }
        serde_json::Value::Object(object) => {
            object.remove("encrypted_content");
            object.remove("reasoning_content");
            for child in object.values_mut() {
                *child = strip_provider_owned_content(std::mem::take(child));
            }
        }
        _ => {}
    }
    strip_images(value)
}

fn item_text(item: &serde_json::Value) -> Option<String> {
    match item.get("content") {
        Some(serde_json::Value::String(text)) => Some(text.clone()),
        Some(serde_json::Value::Array(parts)) => Some(
            parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(serde_json::Value::as_str)
                        .or_else(|| part.get("content").and_then(serde_json::Value::as_str))
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

/// 粗估 token 數（與 history 一致：4 bytes/token）。
pub fn estimate_tokens(items: &[serde_json::Value]) -> u64 {
    let bytes: usize = items
        .iter()
        .map(|i| serde_json::to_vec(i).map(|b| b.len()).unwrap_or(0))
        .sum();
    (bytes / 4).max(1) as u64
}

/// Estimate tokens for an arbitrary JSON value (instructions, tools schemas, …).
pub fn estimate_json_tokens(value: &serde_json::Value) -> u64 {
    if value.is_null() {
        return 0;
    }
    let bytes = serde_json::to_vec(value).map(|b| b.len()).unwrap_or(0);
    if bytes == 0 {
        return 0;
    }
    (bytes / 4).max(1) as u64
}

/// Context contributors outside bare `input` history that still consume the
/// model window (issue #4 comment 5149433523 P1-3): top-level `instructions`
/// and `tools` / function schemas.
pub fn estimate_non_input_context_tokens(request: &serde_json::Value) -> u64 {
    let mut total = 0u64;
    if let Some(instructions) = request.get("instructions") {
        total = total.saturating_add(estimate_json_tokens(instructions));
    }
    if let Some(tools) = request.get("tools") {
        total = total.saturating_add(estimate_json_tokens(tools));
    }
    total
}

/// True when the item is a tool call or output (shared classifier for pairing,
/// pending accounting, and orphan guards — comment 5149622796 P1-2).
pub fn is_pending_tool_item(item: &serde_json::Value) -> bool {
    is_tool_call(item) || is_tool_output(item)
}

/// 從切分結果組 CompactionPreview（純函式）。
///
/// before/after 是「比例式」的數字（供前端堆疊色條 flexGrow 用），
/// 不是絕對 token。比例直觀：系統不變、近期不變、早期大幅縮小（摘要）。
/// 真實的摘要會讓早期段 after 更小，這裡先用經驗比值（約 1/5）。
pub fn build_preview(seg: &Segmentation, total_tokens: u64) -> CompactionPreview {
    // 估算各段 token（用 item 數佔比粗分）。
    let total_items = seg.system_count + seg.early_count + seg.recent_count;
    let total_items = total_items.max(1) as f64;
    let sys_frac = seg.system_count as f64 / total_items;
    let early_frac = seg.early_count as f64 / total_items;
    let recent_frac = seg.recent_count as f64 / total_items;

    let sys_tokens = (total_tokens as f64 * sys_frac).round() as u64;
    let early_tokens = (total_tokens as f64 * early_frac).round() as u64;
    let recent_tokens = (total_tokens as f64 * recent_frac).round() as u64;

    // before/after 用「同一把尺的絕對量」——系統與近期前後不變，
    // 只有早期縮小。這樣前端堆疊色條 flexGrow 直接吃數字時，
    // 系統／近期那兩條寬度真的不動（doc/07「完整保留」的視覺承諾）。
    // 單位是相對量（可 > 100）；前端只看比例，不看絕對值。
    let sys_val = sys_tokens.max(1) as u32;
    let early_before_val = early_tokens.max(1) as u32;
    let recent_val = recent_tokens.max(1) as u32;
    // 早期摘要成約 1/5（經驗值，doc/07）。
    let early_after_val = ((early_tokens as f64) / 5.0).round().max(1.0) as u32;

    let before_sum = sys_tokens + early_tokens + recent_tokens;
    let after_sum = sys_tokens + early_after_val as u64 + recent_tokens;

    CompactionPreview {
        before_tokens: before_sum,
        after_tokens: after_sum,
        after_tokens_exact: true,
        keep_recent_turns: seg.recent_turns,
        segments: vec![
            CompactionSegment {
                kind: "retained_context".into(),
                label: "系統與指示".into(),
                before: sys_val,
                after: sys_val,
                tone: TONE_SYSTEM.into(),
            },
            CompactionSegment {
                kind: "canonical_checkpoint".into(),
                label: "早期往返 → 摘要".into(),
                before: early_before_val,
                after: early_after_val,
                tone: TONE_SUMMARY.into(),
            },
            CompactionSegment {
                kind: "retained_context".into(),
                label: "近期往返".into(),
                before: recent_val,
                after: recent_val,
                tone: TONE_RECENT.into(),
            },
        ],
        engine: "pending".into(),
        readable_replay_tokens: 0,
        cross_session_tokens: 0,
        cross_session_available: false,
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct RoleTokens {
    canonical_checkpoint: u64,
    readable_reasoning: u64,
    tool_state: u64,
    retained_context: u64,
}

impl RoleTokens {
    fn total(self) -> u64 {
        self.canonical_checkpoint
            .saturating_add(self.readable_reasoning)
            .saturating_add(self.tool_state)
            .saturating_add(self.retained_context)
    }
}

fn is_canonical_checkpoint_item(item: &serde_json::Value) -> bool {
    item.get("metadata")
        .and_then(|value| value.get("vellum_checkpoint"))
        .and_then(serde_json::Value::as_str)
        == Some("canonical")
        || is_summary_item(item)
        || item.get("type").and_then(serde_json::Value::as_str) == Some("compaction")
}

fn is_readable_reasoning_item(item: &serde_json::Value) -> bool {
    item.get("type").and_then(serde_json::Value::as_str) == Some("reasoning")
        && (item.get("summary").is_some()
            || item.get("content").is_some()
            || item.get("reasoning_content").is_some())
}

fn observable_preview_item(mut item: serde_json::Value) -> serde_json::Value {
    match &mut item {
        serde_json::Value::Array(values) => {
            for value in values {
                *value = observable_preview_item(std::mem::take(value));
            }
        }
        serde_json::Value::Object(object) => {
            object.remove("encrypted_content");
            for value in object.values_mut() {
                *value = observable_preview_item(std::mem::take(value));
            }
        }
        _ => {}
    }
    item
}

fn role_tokens(items: &[serde_json::Value]) -> RoleTokens {
    let mut result = RoleTokens::default();
    for item in items {
        // Ciphertext byte length is not a token count. Keep the existence of
        // opaque state in detail telemetry, but classify only observable text.
        let observable = observable_preview_item(item.clone());
        let tokens = estimate_tokens(std::slice::from_ref(&observable));
        if is_canonical_checkpoint_item(item) {
            result.canonical_checkpoint = result.canonical_checkpoint.saturating_add(tokens);
            continue;
        }
        if is_readable_reasoning_item(item) {
            result.readable_reasoning = result.readable_reasoning.saturating_add(tokens);
            continue;
        }
        let item_type = item.get("type").and_then(serde_json::Value::as_str);
        let is_tool_definition = item_type == Some("additional_tools")
            || (item.get("role").and_then(serde_json::Value::as_str) == Some("developer")
                && item.get("tools").is_some());
        if is_tool_definition {
            result.tool_state = result.tool_state.saturating_add(tokens);
            continue;
        }
        match classify_item(item) {
            ItemRole::Tool => result.tool_state = result.tool_state.saturating_add(tokens),
            ItemRole::System | ItemRole::User | ItemRole::Assistant | ItemRole::Other => {
                result.retained_context = result.retained_context.saturating_add(tokens)
            }
        }
    }
    result
}

fn segment(kind: &str, label: &str, before: u64, after: u64, tone: &str) -> CompactionSegment {
    CompactionSegment {
        kind: kind.into(),
        label: label.into(),
        before: before.min(u32::MAX as u64) as u32,
        after: after.min(u32::MAX as u64) as u32,
        tone: tone.into(),
    }
}

fn role_preview(
    before: RoleTokens,
    after: RoleTokens,
    keep_recent_turns: u32,
) -> CompactionPreview {
    CompactionPreview {
        before_tokens: before.total(),
        after_tokens: after.total(),
        after_tokens_exact: true,
        keep_recent_turns,
        segments: vec![
            segment(
                "canonical_checkpoint",
                "Canonical 檢查點",
                before.canonical_checkpoint,
                after.canonical_checkpoint,
                TONE_SUMMARY,
            ),
            segment(
                "readable_reasoning",
                "Readable Reasoning Replay",
                before.readable_reasoning,
                after.readable_reasoning,
                TONE_SYSTEM,
            ),
            segment(
                "tool_state",
                "工具續作狀態",
                before.tool_state,
                after.tool_state,
                TONE_TOOL,
            ),
            segment(
                "retained_context",
                "保留的對話脈絡",
                before.retained_context,
                after.retained_context,
                TONE_RECENT,
            ),
        ],
        engine: "pending".into(),
        readable_replay_tokens: after.readable_reasoning,
        cross_session_tokens: 0,
        cross_session_available: false,
    }
}

/// Preview the actual context composition instead of estimating each segment
/// from item counts. This keeps small but important system/tool sections
/// visible and gives the renderer stable values for every legend entry.
pub fn build_item_preview(
    items: &[serde_json::Value],
    keep_recent_turns: u32,
) -> CompactionPreview {
    let before = role_tokens(items);
    let parts = split_items(items, keep_recent_turns);
    let mut preserved = parts.system;
    preserved.extend(parts.recent);
    let mut after = role_tokens(&preserved);
    // The early conversation becomes one structured user checkpoint.
    if !parts.early.is_empty() {
        after.canonical_checkpoint = after
            .canonical_checkpoint
            .saturating_add(estimate_tokens(&parts.early).saturating_div(5).max(1));
    }
    role_preview(before, after, keep_recent_turns)
}

/// Build a semantic preview from a completed canonical journal.
pub fn build_item_comparison_preview(
    original: &[serde_json::Value],
    replacement: &[serde_json::Value],
    keep_recent_turns: u32,
) -> CompactionPreview {
    let before = role_tokens(original);
    let after = role_tokens(replacement);
    role_preview(before, after, keep_recent_turns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(t: &str) -> serde_json::Value {
        json!({"role": "user", "content": t})
    }
    fn assistant(t: &str) -> serde_json::Value {
        json!({"role": "assistant", "content": t})
    }
    fn system(t: &str) -> serde_json::Value {
        json!({"role": "system", "content": t})
    }

    // ---- tool-result pruning（Grok Build [compaction.pruning] 的移植）----

    fn tool_output(call_id: &str, chars: usize) -> serde_json::Value {
        json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": "x".repeat(chars)
        })
    }

    /// `turns` 輪對話，每輪一個 user + 一個大工具結果。
    fn conversation_with_tool_results(turns: usize, chars: usize) -> serde_json::Value {
        let mut input = vec![system("instructions")];
        for turn in 0..turns {
            input.push(user(&format!("turn {turn}")));
            input.push(tool_output(&format!("call_{turn}"), chars));
        }
        json!({"input": input})
    }

    fn body_at(request: &serde_json::Value, index: usize) -> String {
        request["input"][index]["output"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn pruning_leaves_the_most_recent_turns_verbatim() {
        let mut request = conversation_with_tool_results(12, 9_000);
        let outcome = prune_tool_results(&mut request, &ToolResultPruningConfig::default());
        assert!(outcome.touched_any());
        // 最後 3 輪（age 0/1/2）一個字都不能動。
        for turn in 9..12 {
            let index = 1 + turn * 2 + 1;
            assert_eq!(
                body_at(&request, index).chars().count(),
                9_000,
                "turn {turn} must stay verbatim"
            );
        }
    }

    #[test]
    fn pruning_soft_trims_the_middle_and_hard_clears_the_oldest() {
        let mut request = conversation_with_tool_results(12, 9_000);
        let config = ToolResultPruningConfig::default();
        let outcome = prune_tool_results(&mut request, &config);

        // turn 0 的 age = 12 >= hard_clear_age_turns(10) → placeholder。
        let oldest = body_at(&request, 2);
        assert!(oldest.contains("cleared by Vellum"), "{oldest}");
        assert!(oldest.chars().count() < 200);

        // turn 5 的 age = 7 → 介於 keep(3) 與 hard(10) 之間 → 頭尾各留 1500。
        let middle = body_at(&request, 1 + 5 * 2 + 1);
        assert!(middle.contains("elided by Vellum"), "{middle}");
        assert_eq!(
            middle.chars().count(),
            1_500
                + 1_500
                + char_len("\n\n[… 6000 characters elided by Vellum tool-result pruning …]\n\n")
        );

        assert_eq!(outcome.hard_cleared, 2, "turns 0-1 are 12 and 11 turns old");
        assert_eq!(
            outcome.soft_trimmed, 7,
            "turns 2-8 sit between the two ages"
        );
        // 9 prunable results × 9,000 chars = 81,000 in scope; the 3 newest turns
        // (27,000 chars) stay verbatim. Removing >55k of the 81k is the point.
        assert!(
            outcome.chars_removed > 55_000,
            "expected a large saving, got {}",
            outcome.chars_removed
        );
    }

    #[test]
    fn pruning_is_a_no_op_for_short_sessions() {
        let mut request = conversation_with_tool_results(3, 9_000);
        let before = request.clone();
        let outcome = prune_tool_results(&mut request, &ToolResultPruningConfig::default());
        assert!(!outcome.touched_any());
        assert_eq!(request, before);
    }

    #[test]
    fn pruning_never_drops_reorders_or_unpairs_items() {
        let mut request = conversation_with_tool_results(12, 9_000);
        let before = request.clone();
        prune_tool_results(&mut request, &ToolResultPruningConfig::default());

        let before_items = before["input"].as_array().unwrap();
        let after_items = request["input"].as_array().unwrap();
        assert_eq!(before_items.len(), after_items.len());
        for (before_item, after_item) in before_items.iter().zip(after_items) {
            assert_eq!(before_item["type"], after_item["type"]);
            assert_eq!(before_item["role"], after_item["role"]);
            // call_id must survive so tool call/output pairing is intact.
            assert_eq!(before_item["call_id"], after_item["call_id"]);
        }
    }

    /* 一個 user turn 裡的 tool loop 每一輪都會重送整個 context。若同一輪內
    修剪結果會漂移，prefix cache 每一次都失效——那正是要省的東西。 */
    #[test]
    fn pruning_is_stable_across_rounds_within_one_user_turn() {
        let mut first = conversation_with_tool_results(12, 9_000);
        prune_tool_results(&mut first, &ToolResultPruningConfig::default());

        // 同一輪再多一次工具往返（沒有新的 user 訊息）。
        let mut second = conversation_with_tool_results(12, 9_000);
        second["input"]
            .as_array_mut()
            .unwrap()
            .push(tool_output("call_extra", 200));
        prune_tool_results(&mut second, &ToolResultPruningConfig::default());

        let first_items = first["input"].as_array().unwrap();
        let second_items = second["input"].as_array().unwrap();
        assert_eq!(
            first_items,
            &second_items[..first_items.len()],
            "the shared prefix must be byte-identical between rounds"
        );
    }

    #[test]
    fn pruning_handles_chat_wire_tool_messages() {
        let mut input = vec![system("instructions")];
        for turn in 0..12 {
            input.push(user(&format!("turn {turn}")));
            input.push(json!({"role": "tool", "content": "y".repeat(9_000)}));
        }
        let mut request = json!({"messages": input});
        let outcome = prune_tool_results(&mut request, &ToolResultPruningConfig::default());
        assert!(outcome.touched_any());
        assert!(request["messages"][2]["content"]
            .as_str()
            .unwrap()
            .contains("cleared by Vellum"));
    }

    #[test]
    fn pruning_does_not_corrupt_multibyte_tool_results() {
        let mut input = vec![system("instructions")];
        for turn in 0..12 {
            input.push(user(&format!("turn {turn}")));
            input.push(json!({
                "type": "function_call_output",
                "call_id": format!("call_{turn}"),
                // 每個字元 3 bytes——用 byte index 切就會 panic。
                "output": "檔".repeat(9_000)
            }));
        }
        let mut request = json!({"input": input});
        prune_tool_results(&mut request, &ToolResultPruningConfig::default());
        let middle = body_at(&request, 1 + 5 * 2 + 1);
        assert!(middle.starts_with(&"檔".repeat(1_500)));
        assert!(middle.ends_with(&"檔".repeat(1_500)));
    }

    #[test]
    fn pruning_skips_non_string_and_already_small_bodies() {
        let mut input = vec![system("instructions")];
        for turn in 0..12 {
            input.push(user(&format!("turn {turn}")));
        }
        // 結構化 output 不能被改成字串。
        input.push(json!({
            "type": "function_call_output",
            "call_id": "structured",
            "output": {"kind": "image", "data": "…"}
        }));
        let mut request = json!({"input": input});
        let before = request.clone();
        let outcome = prune_tool_results(&mut request, &ToolResultPruningConfig::default());
        assert_eq!(outcome.soft_trimmed, 0);
        assert_eq!(request, before);
    }

    #[test]
    fn pruning_can_be_disabled() {
        let mut request = conversation_with_tool_results(12, 9_000);
        let before = request.clone();
        let config = ToolResultPruningConfig {
            enabled: false,
            ..ToolResultPruningConfig::default()
        };
        assert!(!prune_tool_results(&mut request, &config).touched_any());
        assert_eq!(request, before);
    }

    #[test]
    fn classify_responds_roles() {
        assert_eq!(classify_item(&system("s")), ItemRole::System);
        assert_eq!(classify_item(&user("u")), ItemRole::User);
        assert_eq!(classify_item(&assistant("a")), ItemRole::Assistant);
        assert_eq!(
            classify_item(&json!({"type": "function_call"})),
            ItemRole::Assistant
        );
        assert_eq!(
            classify_item(&json!({"type": "function_call_output"})),
            ItemRole::Tool
        );
    }

    #[test]
    fn segment_splits_three_parts() {
        let items = vec![
            system("sys"),
            user("1"),
            assistant("1"),
            user("2"),
            assistant("2"),
            user("3"),
            assistant("3"),
        ];
        let seg = segment_items(&items, 1);
        assert_eq!(seg.system_count, 1);
        // keep=1 → 最後 1 個 user 起頭的輪是「近期」。
        assert!(seg.recent_count >= 2); // user3 + assistant3
        assert!(seg.early_count >= 2); // user1+asst1 至少
    }

    #[test]
    fn segment_all_recent_when_few_turns() {
        let items = vec![user("1"), assistant("1")];
        let seg = segment_items(&items, 8);
        assert_eq!(seg.system_count, 0);
        assert_eq!(seg.early_count, 0);
        assert_eq!(seg.recent_count, 2);
        assert_eq!(seg.recent_turns, 1);
    }

    #[test]
    fn segment_system_kept_separately() {
        let items = vec![system("s"), developer("d"), user("1"), assistant("1")];
        let seg = segment_items(&items, 8);
        assert_eq!(seg.system_count, 2);
    }

    fn developer(t: &str) -> serde_json::Value {
        json!({"role": "developer", "content": t})
    }

    #[test]
    fn preview_system_and_recent_unchanged() {
        let items = vec![
            system("sys"),
            user("1"),
            assistant("1"),
            user("2"),
            assistant("2"),
            user("3"),
            assistant("3"),
        ];
        let seg = segment_items(&items, 1);
        let preview = build_preview(&seg, 10_000);
        // 系統段 before == after（完整保留）。
        let sys_seg = &preview.segments[0];
        assert_eq!(sys_seg.before, sys_seg.after);
        assert_eq!(sys_seg.tone, TONE_SYSTEM);
        // 近期段 before == after。
        let recent_seg = &preview.segments[2];
        assert_eq!(recent_seg.before, recent_seg.after);
        assert_eq!(recent_seg.tone, TONE_RECENT);
    }

    #[test]
    fn preview_early_shrinks() {
        let items = vec![
            system("s"),
            user("1"),
            assistant("1"),
            user("2"),
            assistant("2"),
            user("3"),
            assistant("3"),
            user("4"),
            assistant("4"),
            user("5"),
            assistant("5"),
        ];
        let seg = segment_items(&items, 1);
        let preview = build_preview(&seg, 10_000);
        let early = &preview.segments[1];
        assert!(
            early.after < early.before,
            "早期段壓縮後應更小：before={} after={}",
            early.before,
            early.after
        );
        assert_eq!(early.tone, TONE_SUMMARY);
    }

    #[test]
    fn preview_after_less_than_before() {
        let items = vec![
            system("s"),
            user("1"),
            assistant("1"),
            user("2"),
            assistant("2"),
            user("3"),
            assistant("3"),
        ];
        let seg = segment_items(&items, 1);
        let preview = build_preview(&seg, 10_000);
        assert!(
            preview.after_tokens < preview.before_tokens,
            "壓縮後 token 應少於壓縮前"
        );
    }

    #[test]
    fn preview_keep_recent_turns_recorded() {
        let items = vec![
            user("1"),
            assistant("1"),
            user("2"),
            assistant("2"),
            user("3"),
            assistant("3"),
        ];
        let seg = segment_items(&items, 2);
        let preview = build_preview(&seg, 1_000);
        assert_eq!(preview.keep_recent_turns, 2);
    }

    #[test]
    fn estimate_tokens_positive() {
        assert!(estimate_tokens(&[user("hi")]) >= 1);
    }

    #[test]
    fn role_preview_exposes_canonical_replay_context_states_with_real_tokens() {
        let original = vec![
            json!({"type": "message", "role": "developer", "content": "system prompt"}),
            json!({"type": "additional_tools", "role": "developer", "tools": [{"name": "shell"}]}),
            user("implement the task"),
            assistant("working"),
            json!({"type": "function_call_output", "output": "tool result"}),
            user("continue"),
        ];
        let replacement = vec![user("continue"), user("structured checkpoint summary")];
        let preview = build_item_comparison_preview(&original, &replacement, 8);
        assert_eq!(
            preview
                .segments
                .iter()
                .map(|segment| segment.label.as_str())
                .collect::<Vec<_>>(),
            vec![
                "Canonical 檢查點",
                "Readable Reasoning Replay",
                "工具續作狀態",
                "保留的對話脈絡"
            ]
        );
        assert_eq!(preview.segments[0].before, 0);
        assert!(preview.segments[2].before > preview.segments[2].after);
        assert!(preview.segments[3].before > preview.segments[3].after);
        assert!(preview.after_tokens > 0);
    }

    #[test]
    fn canonical_preview_classifies_checkpoint_and_readable_reasoning_separately() {
        let checkpoint = summary_item(&StructuredSummary {
            goal: "Continue the implementation".into(),
            decisions: vec!["Keep the public API stable".into()],
            next_steps: vec!["Run the focused tests".into()],
            ..StructuredSummary::default()
        });
        let reasoning = json!({
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": "The parser is the root cause."}]
        });
        let preview = build_item_comparison_preview(
            &[user("investigate"), assistant("working")],
            &[checkpoint, reasoning],
            8,
        );
        let checkpoint = preview
            .segments
            .iter()
            .find(|segment| segment.kind == "canonical_checkpoint")
            .unwrap();
        let replay = preview
            .segments
            .iter()
            .find(|segment| segment.kind == "readable_reasoning")
            .unwrap();
        assert!(checkpoint.after > 0);
        assert!(replay.after > 0);
        assert_eq!(preview.readable_replay_tokens, u64::from(replay.after));
    }

    #[test]
    fn preview_does_not_treat_opaque_ciphertext_bytes_as_reasoning_tokens() {
        let small = json!({
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": "Keep this decision."}],
            "encrypted_content": "short"
        });
        let large = json!({
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": "Keep this decision."}],
            "encrypted_content": "x".repeat(100_000)
        });
        let small_tokens = role_tokens(&[small]).readable_reasoning;
        let large_tokens = role_tokens(&[large]).readable_reasoning;
        assert_eq!(small_tokens, large_tokens);
    }

    #[test]
    fn structured_summary_text_accepts_reasoning_wrappers_but_requires_json() {
        let wrapped = r#"<think>I should preserve the exact state.</think>
Here is the checkpoint:
```json
{"goal":"Continue implementation","done":["Read the code"],"nextSteps":["Run tests"]}
```"#;
        let summary = parse_structured_summary_text(wrapped).unwrap();
        assert_eq!(summary.goal, "Continue implementation");
        assert_eq!(summary.next_steps, ["Run tests"]);
        assert!(parse_structured_summary_text("The task is mostly complete.").is_none());
    }

    #[test]
    fn long_compaction_input_is_chunked_and_oversized_items_are_bounded() {
        let items = vec![
            user(&"a".repeat(8_000)),
            assistant(&"b".repeat(8_000)),
            user(&"c".repeat(20_000)),
        ];
        let chunks = chunk_compaction_items(&items, 1_500);
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().all(|chunk| estimate_tokens(chunk) <= 1_600));
    }

    #[test]
    fn chunk_summaries_merge_in_order_and_deduplicate_state() {
        let first = StructuredSummary {
            goal: "Initial goal".into(),
            done: vec!["Read code".into()],
            relevant_files: vec!["src/a.rs".into()],
            ..StructuredSummary::default()
        };
        let second = StructuredSummary {
            goal: "Current goal".into(),
            done: vec!["Read code".into(), "Made patch".into()],
            next_steps: vec!["Run tests".into()],
            ..StructuredSummary::default()
        };
        let merged = merge_structured_summaries(&[first, second]);
        assert_eq!(merged.goal, "Current goal");
        assert_eq!(merged.done, ["Read code", "Made patch"]);
        assert_eq!(merged.next_steps, ["Run tests"]);
    }

    fn useful_summary() -> StructuredSummary {
        serde_json::from_value(json!({
            "goal": "Finish the proxy",
            "done": ["Located the route"],
            "nextSteps": ["Run cargo test"]
        }))
        .unwrap()
    }

    #[test]
    fn remote_output_uses_official_handoff_shape_and_drops_stale_context() {
        let old_summary = summary_item(&useful_summary());
        let history = vec![
            system("stale system"),
            old_summary,
            user("original requirement"),
            assistant("working"),
            json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_image", "image_url": "data:image/png;base64,huge"}]
            }),
        ];
        let output = build_remote_compact_output(&history, &useful_summary());
        let encoded = serde_json::to_string(&output).unwrap();
        assert!(output.iter().all(|item| item["role"] == "user"));
        assert_eq!(
            output.iter().filter(|item| is_summary_item(item)).count(),
            1
        );
        assert!(!encoded.contains("stale system"));
        assert!(!encoded.contains("data:image"));
        assert!(encoded.contains("Image omitted during compaction"));
        assert!(is_summary_item(output.last().unwrap()));
    }

    #[test]
    fn summary_source_excludes_canonical_and_provider_owned_context() {
        let history = vec![
            system("OLD_STALE_INSTRUCTION_DO_NOT_REPLAY"),
            user("keep this requirement"),
            json!({
                "type": "reasoning",
                "encrypted_content": "foreign ciphertext",
                "summary": [{"type": "summary_text", "text": "private reasoning"}]
            }),
            json!({
                "type": "message",
                "role": "assistant",
                "reasoning_content": "provider scratchpad",
                "content": [{"type": "output_text", "text": "keep this result"}]
            }),
        ];
        let source = compactable_history(&history);
        let encoded = serde_json::to_string(&source).unwrap();
        assert!(encoded.contains("keep this requirement"));
        assert!(encoded.contains("keep this result"));
        assert!(!encoded.contains("OLD_STALE_INSTRUCTION_DO_NOT_REPLAY"));
        assert!(!encoded.contains("foreign ciphertext"));
        assert!(!encoded.contains("provider scratchpad"));
        assert!(!encoded.contains("private reasoning"));
    }

    #[test]
    fn empty_summary_is_rejected_before_checkpoint_installation() {
        let empty: StructuredSummary = serde_json::from_value(json!({})).unwrap();
        assert!(!empty.is_usable());
        assert!(useful_summary().is_usable());
    }

    #[test]
    fn repeated_compaction_replaces_instead_of_nesting_old_summary() {
        let first = build_remote_compact_output(&[user("request")], &useful_summary());
        let second = build_remote_compact_output(&first, &useful_summary());
        assert_eq!(
            second.iter().filter(|item| is_summary_item(item)).count(),
            1
        );
        assert_eq!(
            second
                .iter()
                .filter(|item| item["role"] == "user" && !is_summary_item(item))
                .count(),
            1
        );
    }

    #[test]
    fn portable_sanitizer_strips_encrypted_and_auth_fields() {
        let history = vec![
            system("do not send"),
            user("visible requirement"),
            json!({
                "type": "reasoning",
                "encrypted_content": "grok-opaque-state",
                "summary": [{"type": "summary_text", "text": "private"}]
            }),
            json!({
                "type": "message",
                "role": "assistant",
                "content": "ok",
                "api_key": "secret-token",
                "access_token": "bearer-xyz"
            }),
            json!({
                "type": "compaction",
                "id": "cmp_1",
                "encrypted_content": "opaque-compaction"
            }),
        ];
        let portable = sanitize_portable_history(&history);
        let encoded = serde_json::to_string(&portable).unwrap();
        assert!(encoded.contains("visible requirement"));
        assert!(!encoded.contains("grok-opaque-state"));
        assert!(!encoded.contains("secret-token"));
        assert!(!encoded.contains("bearer-xyz"));
        assert!(!encoded.contains("opaque-compaction"));
        assert!(!encoded.contains("do not send"));
    }

    #[test]
    fn canonical_checkpoint_requires_semantic_and_next_step() {
        let summary = useful_summary();
        let checkpoint = canonical_checkpoint_from_summary(
            &summary,
            vec![
                user("latest intent"),
                json!({
                    "type": "reasoning",
                    "encrypted_content": "must-be-stripped"
                }),
            ],
        )
        .unwrap();
        assert_eq!(checkpoint.continuity_kind, ContinuityKind::Semantic);
        assert!(!checkpoint.exact_next_step.is_empty());
        assert!(validate_canonical_checkpoint(&checkpoint).is_ok());
        let installed = install_canonical_checkpoint_items(&checkpoint);
        let encoded = serde_json::to_string(&installed).unwrap();
        assert!(encoded.contains("semantic"));
        assert!(!encoded.contains("must-be-stripped"));

        let mut bad = checkpoint.clone();
        bad.continuity_kind = ContinuityKind::Native;
        assert!(validate_canonical_checkpoint(&bad).is_err());
    }

    #[test]
    fn canonical_checkpoint_rejects_orphaned_tool_history() {
        let orphan_call = canonical_checkpoint_from_summary(
            &useful_summary(),
            vec![json!({
                "type": "function_call",
                "call_id": "call_real_shape",
                "name": "exec_command",
                "arguments": "{}"
            })],
        )
        .unwrap_err();
        assert!(orphan_call.contains("no matching output"), "{orphan_call}");

        let orphan_output = canonical_checkpoint_from_summary(
            &useful_summary(),
            vec![json!({
                "type": "function_call_output",
                "call_id": "call_real_shape",
                "output": "done"
            })],
        )
        .unwrap_err();
        assert!(
            orphan_output.contains("no preceding call"),
            "{orphan_output}"
        );

        let paired = canonical_checkpoint_from_summary(
            &useful_summary(),
            vec![
                json!({
                    "type": "function_call",
                    "call_id": "call_real_shape",
                    "name": "exec_command",
                    "arguments": "{}"
                }),
                json!({
                    "type": "function_call_output",
                    "call_id": "call_real_shape",
                    "output": "done"
                }),
            ],
        )
        .unwrap();
        assert!(validate_canonical_checkpoint(&paired).is_ok());
    }

    #[test]
    fn canonical_checkpoint_does_not_promote_transient_tool_failure() {
        let mut summary = useful_summary();
        summary.blocked = vec![
            "Cannot access local files in this turn; restart the task".into(),
            "Waiting for the user to choose an API contract".into(),
        ];
        summary.next_steps = vec![
            "Restart this task because tools are unavailable".into(),
            "Inspect the existing diff and continue the implementation".into(),
        ];
        let checkpoint = canonical_checkpoint_from_summary(&summary, Vec::new()).unwrap();
        assert_eq!(
            checkpoint.unresolved_blockers,
            vec!["Waiting for the user to choose an API contract"]
        );
        assert!(checkpoint
            .failed_attempts
            .iter()
            .any(|value| value.contains("Cannot access local files")));
        assert_eq!(
            checkpoint.exact_next_step,
            "Inspect the existing diff and continue the implementation"
        );
    }

    #[test]
    fn installed_checkpoint_makes_current_tools_and_user_message_authoritative() {
        let checkpoint = canonical_checkpoint_from_summary(
            &useful_summary(),
            vec![user("newest retained user intent")],
        )
        .unwrap();
        let encoded =
            serde_json::to_string(&install_canonical_checkpoint_items(&checkpoint)).unwrap();
        assert!(encoded.contains("latest user message"));
        assert!(encoded.contains("current request"));
        assert!(encoded.contains("newest retained user intent"));
    }

    #[test]
    fn canonical_validation_ignores_opaque_field_names_in_visible_text() {
        let summary = useful_summary();
        let visible_diagnostic =
            "The upstream error mentioned reasoning_content and encrypted_content as field names.";
        let checkpoint = canonical_checkpoint_from_summary(
            &summary,
            vec![json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": visible_diagnostic}]
            })],
        )
        .unwrap();

        assert!(validate_canonical_checkpoint(&checkpoint).is_ok());
        assert!(serde_json::to_string(&checkpoint.retained_tail_items)
            .unwrap()
            .contains(visible_diagnostic));
    }

    #[test]
    fn canonical_validation_rejects_nested_opaque_keys_with_an_exact_path() {
        let mut checkpoint =
            canonical_checkpoint_from_summary(&useful_summary(), vec![user("latest intent")])
                .unwrap();
        checkpoint.retained_tail_items.push(json!({
            "type": "message",
            "role": "assistant",
            "metadata": {"reasoning_content": "provider scratchpad"},
            "content": "visible answer"
        }));

        let error = validate_canonical_checkpoint(&checkpoint).unwrap_err();
        assert!(error.contains("retained_tail_items[1].metadata.reasoning_content"));
        assert!(error.contains("opaque field `reasoning_content`"));
    }

    #[test]
    fn canonical_checkpoint_bounds_repetitive_model_summary() {
        let repeated = (0..200)
            .map(|index| format!("Context record {index:04}: {}", "x".repeat(1_000)))
            .collect::<Vec<_>>();
        let summary = StructuredSummary {
            goal: "g".repeat(8_000),
            constraints: repeated.clone(),
            decisions: repeated.clone(),
            done: repeated.clone(),
            critical_context: repeated,
            next_steps: vec!["continue".into()],
            ..StructuredSummary::default()
        };
        let checkpoint = canonical_checkpoint_from_summary(&summary, Vec::new()).unwrap();
        assert!(checkpoint.goal.chars().count() <= 2_049);
        assert_eq!(checkpoint.constraints.len(), 8);
        assert_eq!(checkpoint.decisions.len(), 8);
        assert!(checkpoint
            .constraints
            .iter()
            .all(|value| value.chars().count() <= 385));
        assert!(estimate_tokens(&install_canonical_checkpoint_items(&checkpoint)) < 10_000);
    }

    #[test]
    fn canonical_shrink_gate_does_not_double_count_future_reserves() {
        assert!(canonical_replacement_frees_context(
            10_000, 2_000, 4_096, 8_192
        ));
        assert!(!canonical_replacement_frees_context(
            2_000, 2_000, 4_096, 8_192
        ));
    }

    #[test]
    fn grok_native_config_mirrors_build_defaults_not_openai_compact() {
        let config = GrokNativeCompactionConfig::default();
        assert_eq!(config.auto_compact_threshold_percent, 85);
        assert!(!config.two_pass_compaction);
        let decision = config.decide(90_000, 0, 100_000);
        assert!(decision.should_compact);
        assert_eq!(decision.continuity_kind, ContinuityKind::Native);
        assert!(!may_silently_roll_truncate());
    }

    #[test]
    fn grok_build_prompt_has_versioned_nine_section_contract() {
        let prompt = grok_build_compaction_prompt();
        assert_eq!(
            GROK_BUILD_COMPACTION_PROMPT_VERSION,
            "grok-build-0.2.112-nine-section"
        );
        for section in 1..=9 {
            assert!(prompt.contains(&format!("{section}.")));
        }
        assert!(prompt.contains("<summary>"));
        assert!(!prompt.contains("/responses/compact"));
    }

    #[test]
    fn grok_build_summary_strips_private_control_markup_and_rejects_degenerate_output() {
        let body = format!(
            "<analysis>private draft</analysis><summary>{}</summary><|eos|>",
            "durable implementation state ".repeat(30)
        );
        let normalized = normalize_grok_build_summary(&body).unwrap();
        assert!(!normalized.contains("private draft"));
        assert!(!normalized.contains("<|eos|>"));
        assert!(normalize_grok_build_summary("<summary>too short</summary>").is_none());
    }

    #[test]
    fn grok_native_window_keeps_recent_tool_pair_then_successor_summary() {
        let source = vec![
            user("old request"),
            assistant("old answer"),
            user("current request"),
            json!({
                "type": "function_call",
                "call_id": "call_current",
                "name": "read_file",
                "arguments": "{}"
            }),
            json!({
                "type": "function_call_output",
                "call_id": "call_current",
                "output": "current tool result"
            }),
            json!({"type": "compaction_trigger"}),
        ];
        let summary = "durable state ".repeat(50);
        let window = build_grok_native_window(&source, &summary, 50_000);
        let encoded = serde_json::to_string(&window).unwrap();
        assert!(!encoded.contains("old request"));
        assert!(encoded.contains("current request"));
        assert!(encoded.contains("call_current"));
        assert!(encoded.contains("current tool result"));
        assert!(!encoded.contains("compaction_trigger"));
        assert!(encoded.contains("This session is being continued"));
        assert!(encoded.contains("durable state"));
    }

    #[test]
    fn split_canonical_source_summarizes_early_and_bounds_tail_with_tool_pairs() {
        let mut items = Vec::new();
        for i in 0..20 {
            items.push(user(&format!("early-turn-{i}-{}", "x".repeat(200))));
            items.push(assistant(&format!("early-reply-{i}")));
        }
        items.push(user("latest intent"));
        items.push(json!({
            "type": "function_call",
            "call_id": "call_pair",
            "name": "read",
            "arguments": "{}"
        }));
        items.push(json!({
            "type": "function_call_output",
            "call_id": "call_pair",
            "output": "tool result body"
        }));
        let (early, tail) = split_canonical_source(&items, 2, 4_000);
        assert!(!early.is_empty());
        // Every source item lands in early ∪ tail (no silent drop).
        assert_eq!(
            early.len() + tail.len(),
            items.len(),
            "early={} tail={} source={}",
            early.len(),
            tail.len(),
            items.len()
        );
        let early_text = serde_json::to_string(&early).unwrap();
        let tail_text = serde_json::to_string(&tail).unwrap();
        // Earliest turns land in early, not as full raw tail.
        assert!(early_text.contains("early-turn-0") || early_text.contains("early-turn-1"));
        assert!(tail_text.contains("latest intent") || tail_text.contains("call_pair"));
        // Tool pairing intact in tail when present.
        if tail_text.contains("call_pair") {
            assert!(tail_text.contains("function_call"));
            assert!(tail_text.contains("function_call_output"));
        }
        let summary = useful_summary();
        let checkpoint = canonical_checkpoint_from_summary(&summary, tail).unwrap();
        let installed = install_canonical_checkpoint_items(&checkpoint);
        let source_tokens = estimate_tokens(&items);
        let replacement_tokens = estimate_tokens(&installed);
        assert!(
            canonical_replacement_frees_context(source_tokens, replacement_tokens, 100, 100),
            "source={source_tokens} replacement={replacement_tokens}"
        );
        assert!(!canonical_replacement_frees_context(
            replacement_tokens,
            source_tokens,
            0,
            0
        ));
    }

    /// keep_recent volume exceeds max_tail_tokens so bound_retained_tail drops
    /// oldest "recent" items — those must still appear in early, not vanish.
    #[test]
    fn pack_tail_units_parallel_and_reverse_output_order() {
        let items = vec![
            json!({"type": "function_call", "call_id": "A", "name": "a", "arguments": "{}"}),
            json!({"type": "function_call", "call_id": "B", "name": "b", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "A", "output": "oa"}),
            json!({"type": "function_call_output", "call_id": "B", "output": "ob"}),
        ];
        let units = pack_tail_units(&items);
        // Overlapping parallel spans merge into one order-preserving batch.
        assert_eq!(units.len(), 1);
        assert_eq!(flatten_tail_units(&units), items);
        let rev = vec![
            json!({"type": "function_call", "call_id": "A", "name": "a", "arguments": "{}"}),
            json!({"type": "function_call", "call_id": "B", "name": "b", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "B", "output": "ob"}),
            json!({"type": "function_call_output", "call_id": "A", "output": "oa"}),
        ];
        assert_eq!(flatten_tail_units(&pack_tail_units(&rev)), rev);
        assert_eq!(pack_tail_units(&rev).len(), 1);
    }

    #[test]
    fn enforce_pairing_uses_multiset_newest_occurrences() {
        let full = vec![
            user("Continue"),
            assistant("old"),
            user("Continue"),
            assistant("new"),
        ];
        // Recent window has a single "Continue" user — only the newest match.
        let recent = vec![user("Continue"), assistant("new")];
        let expanded = enforce_tool_call_output_pairing(&full, &recent);
        let users: Vec<_> = expanded
            .iter()
            .filter(|i| i.get("role").and_then(|r| r.as_str()) == Some("user"))
            .collect();
        assert_eq!(users.len(), 1, "multiset must not pull both Continues");
    }

    /// Duplicate "Continue" payloads: early/tail are true source prefix/suffix.
    #[test]
    fn split_canonical_source_is_positional_for_duplicate_continue_payloads() {
        let source = vec![
            user("Continue"),
            assistant("old answer"),
            user("Continue"),
            assistant("new answer"),
        ];
        let (early, tail) = split_canonical_source(&source, 1, 50_000);
        assert_eq!(
            early,
            source[..2],
            "early must be the source prefix, not multiset-eaten oldest Continue"
        );
        assert_eq!(tail, source[2..], "tail must be the newest source suffix");
        assert_eq!(
            [early.as_slice(), tail.as_slice()].concat(),
            source,
            "early ‖ tail must reconstruct the full source"
        );
        // Bound API agrees with the same start index.
        let start = retained_tail_start_index(&source, 1, 50_000);
        assert_eq!(start, 2);
        assert_eq!(bound_retained_tail(&source, 1, 50_000), source[start..]);
    }

    #[test]
    fn enforce_pairing_keeps_full_span_including_control_between_call_and_output() {
        let full = vec![
            user("before"),
            json!({"type": "function_call", "call_id": "X", "name": "read", "arguments": "{}"}),
            json!({"role": "assistant", "content": "control-between"}),
            json!({"type": "function_call_output", "call_id": "X", "output": "out"}),
        ];
        // Recent only has the output (as if turn boundary cut mid-span).
        let recent = vec![json!({
            "type": "function_call_output",
            "call_id": "X",
            "output": "out"
        })];
        let expanded = enforce_tool_call_output_pairing(&full, &recent);
        let text = serde_json::to_string(&expanded).unwrap();
        assert!(
            text.contains("function_call"),
            "call must be pulled: {text}"
        );
        assert!(
            text.contains("control-between"),
            "full positional span must keep control item: {text}"
        );
        assert!(text.contains("out"));
    }

    #[test]
    fn computer_and_tool_search_kinds_are_tools_for_pairing_and_pending() {
        let computer_call = json!({
            "type": "computer_call",
            "call_id": "c1",
            "action": {"type": "click"}
        });
        let computer_out = json!({
            "type": "computer_call_output",
            "call_id": "c1",
            "output": "screen"
        });
        let search_call = json!({
            "type": "tool_search_call",
            "call_id": "s1",
            "query": "docs"
        });
        let search_out = json!({
            "type": "tool_search_output",
            "call_id": "s1",
            "output": "hits"
        });
        let web = json!({"type": "web_search_call", "call_id": "w1", "query": "x"});
        assert!(is_tool_call(&computer_call) && is_tool_output(&computer_out));
        assert!(is_tool_call(&search_call) && is_tool_output(&search_out));
        assert!(is_tool_call(&web));
        assert!(is_pending_tool_item(&computer_call) && is_pending_tool_item(&computer_out));
        let items = vec![computer_call, computer_out.clone()];
        assert_eq!(flatten_tail_units(&pack_tail_units(&items)), items);
        let expanded = enforce_tool_call_output_pairing(&items, &[computer_out]);
        assert_eq!(expanded.len(), 2, "computer call+output must pair");
    }

    /// Reused call_id: only the newest occurrence is pulled into the recent pair window.
    #[test]
    fn enforce_pairing_is_occurrence_scoped_for_reused_call_id() {
        let full = vec![
            json!({"type": "function_call", "call_id": "A", "name": "read", "arguments": "{\"old\":true}"}),
            json!({"type": "function_call_output", "call_id": "A", "output": "old-out"}),
            user("later turn"),
            json!({"type": "function_call", "call_id": "A", "name": "read", "arguments": "{\"new\":true}"}),
            json!({"type": "function_call_output", "call_id": "A", "output": "new-out"}),
        ];
        let recent = vec![
            json!({"type": "function_call", "call_id": "A", "name": "read", "arguments": "{\"new\":true}"}),
            json!({"type": "function_call_output", "call_id": "A", "output": "new-out"}),
        ];
        let expanded = enforce_tool_call_output_pairing(&full, &recent);
        let text = serde_json::to_string(&expanded).unwrap();
        assert!(text.contains("new-out"));
        assert!(
            !text.contains("old-out"),
            "older same call_id exchange must not be pulled: {text}"
        );
        assert_eq!(
            flatten_tail_units(&pack_tail_units(&expanded)),
            expanded,
            "retained pairing window must order-preserve"
        );
        // Pack of full history keeps two separate occurrence units for same id.
        let units = pack_tail_units(&full);
        assert!(
            units.len() >= 3,
            "old exchange, user, new exchange should be separate units: {units:?}"
        );
    }

    #[test]
    fn non_input_context_tokens_count_instructions_and_tools() {
        let bare = json!({
            "input": [{"role": "user", "content": "hi"}]
        });
        assert_eq!(estimate_non_input_context_tokens(&bare), 0);
        let fat = json!({
            "instructions": "X".repeat(4_000),
            "tools": [{
                "type": "function",
                "name": "big",
                "parameters": {"description": "Y".repeat(4_000)}
            }],
            "input": [{"role": "user", "content": "hi"}]
        });
        let overhead = estimate_non_input_context_tokens(&fat);
        assert!(
            overhead > estimate_tokens(&[json!({"role": "user", "content": "hi"})]),
            "instructions+tools must dominate bare input: overhead={overhead}"
        );
        assert!(is_pending_tool_item(&json!({
            "type": "custom_tool_call",
            "call_id": "c1",
            "name": "shell"
        })));
        assert!(is_pending_tool_item(&json!({
            "type": "custom_tool_call_output",
            "call_id": "c1",
            "output": "ok"
        })));
    }

    #[test]
    fn split_canonical_source_never_silently_drops_when_tail_token_bound_cuts() {
        let mut items = Vec::new();
        // Many large recent turns: keep_recent_turns=8 keeps all as "recent",
        // but max_tail_tokens forces bound_retained_tail to drop oldest of them.
        for i in 0..12 {
            items.push(user(&format!("turn-{i}-{}", "X".repeat(800))));
            items.push(assistant(&format!("reply-{i}-{}", "Y".repeat(400))));
        }
        items.push(json!({
            "type": "function_call",
            "call_id": "call_bound",
            "name": "read",
            "arguments": "{}"
        }));
        items.push(json!({
            "type": "function_call_output",
            "call_id": "call_bound",
            "output": "paired tool output"
        }));

        let max_tail = 1_500u64;
        // Prove the recent slice alone exceeds the tail budget so the bound path runs.
        let recent_only = split_items(&items, 8).recent;
        assert!(
            estimate_tokens(&recent_only) > max_tail,
            "fixture must exceed max_tail_tokens so bound_retained_tail drops items"
        );

        let (early, tail) = split_canonical_source(&items, 8, max_tail);
        assert!(
            !early.is_empty(),
            "dropped-from-tail items must move into early"
        );
        assert!(!tail.is_empty());
        assert!(estimate_tokens(&tail) <= max_tail.max(256));

        // Partition integrity: every source item is in early ∪ tail exactly once.
        assert_eq!(
            early.len() + tail.len(),
            items.len(),
            "silent drop: early={} tail={} source={}",
            early.len(),
            tail.len(),
            items.len()
        );
        let mut early_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut tail_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for item in &early {
            *early_counts
                .entry(serde_json::to_string(item).unwrap_or_default())
                .or_insert(0) += 1;
        }
        for item in &tail {
            *tail_counts
                .entry(serde_json::to_string(item).unwrap_or_default())
                .or_insert(0) += 1;
        }
        for item in &items {
            let key = serde_json::to_string(item).unwrap_or_default();
            let in_early = early_counts.get(&key).copied().unwrap_or(0);
            let in_tail = tail_counts.get(&key).copied().unwrap_or(0);
            assert_eq!(
                in_early + in_tail,
                1,
                "item must appear exactly once across early∪tail: {key}"
            );
            if in_early > 0 {
                *early_counts.get_mut(&key).unwrap() -= 1;
            } else {
                *tail_counts.get_mut(&key).unwrap() -= 1;
            }
        }

        // Oldest recent content that was token-bound out of the tail is in early.
        let early_text = serde_json::to_string(&early).unwrap();
        assert!(
            early_text.contains("turn-0-") || early_text.contains("turn-1-"),
            "token-bound-out recent items must be summarized via early"
        );
        // Newest tail keeps tool pairing when present.
        let tail_text = serde_json::to_string(&tail).unwrap();
        if tail_text.contains("call_bound") {
            assert!(tail_text.contains("function_call"));
            assert!(tail_text.contains("function_call_output"));
        }
    }
}
