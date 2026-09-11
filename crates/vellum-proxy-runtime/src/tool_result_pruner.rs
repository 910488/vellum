//! Pressure-gated Tool Result Pruner.
//!
//! Unlike ingress truncation (which breaks first-read evidence comprehension),
//! this pruner activates ONLY when model-visible context pressure exceeds a
//! configured threshold. It keeps durable raw history completely intact,
//! rewriting only the visible request surface.
//!
//! Preserves tool exchange identity (call_id / occurrence_id) and ensures
//! pruned replacements are never folded as new tool results.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::compaction::estimate_tokens;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultPrunePolicy {
    pub enabled: bool,
    /// Ratio of context window usage that triggers pruning (e.g. 0.75 = 75%).
    pub trigger_pressure_ratio: f64,
    /// Minimum string length of a tool result to be eligible for pruning.
    pub max_result_chars: usize,
    /// Number of characters to retain from the beginning of the result.
    pub head_chars: usize,
    /// Number of characters to retain from the end of the result.
    pub tail_chars: usize,
}

impl Default for ToolResultPrunePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger_pressure_ratio: 0.75,
            max_result_chars: 4000,
            head_chars: 1200,
            tail_chars: 1200,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PruneReport {
    pub pruned_results_count: usize,
    pub original_estimated_tokens: u64,
    pub post_prune_estimated_tokens: u64,
    pub chars_removed: usize,
}

/// Safely slices a string slice up to `max_bytes` without breaking UTF-8 char boundaries.
fn safe_head(text: &str, max_chars: usize) -> &str {
    for (count, (byte_idx, _)) in text.char_indices().enumerate() {
        if count == max_chars {
            return &text[..byte_idx];
        }
    }
    text
}

/// Safely slices the last `max_chars` of a string without breaking UTF-8 char boundaries.
fn safe_tail(text: &str, max_chars: usize) -> &str {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text;
    }
    let skip = char_count - max_chars;
    for (count, (byte_idx, _)) in text.char_indices().enumerate() {
        if count == skip {
            return &text[byte_idx..];
        }
    }
    text
}

/// Prune only text-bearing leaves of a tool-result payload. Rich content
/// arrays keep their block shape and all non-text blocks/identity metadata.
fn prune_text_payload(value: &mut Value, policy: &ToolResultPrunePolicy, min_len: usize) -> usize {
    match value {
        Value::String(text) => {
            let char_count = text.chars().count();
            if char_count <= min_len {
                return 0;
            }
            let head = safe_head(text, policy.head_chars);
            let tail = safe_tail(text, policy.tail_chars);
            let omitted = char_count.saturating_sub(policy.head_chars + policy.tail_chars);
            let marker = format!(
                "\n... [tool result truncated for context efficiency: {omitted} chars omitted] ...\n"
            );
            *text = format!("{head}{marker}{tail}");
            omitted
        }
        Value::Array(blocks) => blocks
            .iter_mut()
            .map(|block| match block {
                Value::String(_) => prune_text_payload(block, policy, min_len),
                Value::Object(object) => {
                    let text_bearing = object
                        .get("type")
                        .and_then(Value::as_str)
                        .map(|kind| matches!(kind, "text" | "input_text" | "output_text"))
                        .unwrap_or_else(|| object.contains_key("text"));
                    if text_bearing {
                        object
                            .get_mut("text")
                            .map(|text| prune_text_payload(text, policy, min_len))
                            .unwrap_or(0)
                    } else {
                        0
                    }
                }
                _ => 0,
            })
            .sum(),
        _ => 0,
    }
}

/// Estimates total prompt surface tokens for an entire request payload:
/// input items + instructions + tools.
pub fn estimate_request_tokens(body: &Value) -> u64 {
    let input_tokens = body
        .get("input")
        .and_then(Value::as_array)
        .map(|items| estimate_tokens(items))
        .unwrap_or(0);
    let instruction_tokens = body
        .get("instructions")
        .map(crate::compaction::estimate_json_tokens)
        .unwrap_or(0);
    let tool_tokens = body
        .get("tools")
        .map(crate::compaction::estimate_json_tokens)
        .unwrap_or(0);
    input_tokens
        .saturating_add(instruction_tokens)
        .saturating_add(tool_tokens)
}

/// Inspects visible history items and conditionally prunes oversized tool results
/// if and only if estimated prompt surface pressure exceeds `policy.trigger_pressure_ratio`.
pub fn prune_tool_results_under_pressure_with_estimate(
    items: &mut [Value],
    total_request_tokens: u64,
    context_window: u64,
    policy: &ToolResultPrunePolicy,
) -> PruneReport {
    if !policy.enabled || items.is_empty() || context_window == 0 {
        let tokens = estimate_tokens(items);
        return PruneReport {
            pruned_results_count: 0,
            original_estimated_tokens: tokens,
            post_prune_estimated_tokens: tokens,
            chars_removed: 0,
        };
    }

    let pressure_ratio = (total_request_tokens as f64) / (context_window as f64);

    // Strict invariant: below pressure threshold -> ZERO visible rewrite
    if pressure_ratio < policy.trigger_pressure_ratio {
        let original_tokens = estimate_tokens(items);
        return PruneReport {
            pruned_results_count: 0,
            original_estimated_tokens: original_tokens,
            post_prune_estimated_tokens: original_tokens,
            chars_removed: 0,
        };
    }

    let original_tokens = estimate_tokens(items);
    let min_len = policy
        .max_result_chars
        .max(policy.head_chars + policy.tail_chars + 64);
    let mut pruned_count = 0;
    let mut chars_removed = 0;

    for item in items.iter_mut() {
        // Handle function_call_output
        if item.get("type").and_then(Value::as_str) == Some("function_call_output") {
            if let Some(output_val) = item.get_mut("output") {
                let removed = prune_text_payload(output_val, policy, min_len);
                if removed > 0 {
                    chars_removed += removed;
                    pruned_count += 1;
                }
            }
        } else if item.get("role").and_then(Value::as_str) == Some("tool") {
            // Handle Chat-style tool messages
            if let Some(content_val) = item.get_mut("content") {
                let removed = prune_text_payload(content_val, policy, min_len);
                if removed > 0 {
                    chars_removed += removed;
                    pruned_count += 1;
                }
            }
        }
    }

    let post_tokens = estimate_tokens(items);
    PruneReport {
        pruned_results_count: pruned_count,
        original_estimated_tokens: original_tokens,
        post_prune_estimated_tokens: post_tokens,
        chars_removed,
    }
}

pub fn prune_tool_results_under_pressure(
    items: &mut [Value],
    context_window: u64,
    policy: &ToolResultPrunePolicy,
) -> PruneReport {
    let tokens = estimate_tokens(items);
    prune_tool_results_under_pressure_with_estimate(items, tokens, context_window, policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn below_pressure_ratio_does_not_modify_items() {
        let mut items = vec![json!({
            "type": "function_call_output",
            "call_id": "call_1",
            "output": "A".repeat(10_000)
        })];

        let policy = ToolResultPrunePolicy {
            enabled: true,
            trigger_pressure_ratio: 0.80,
            max_result_chars: 4000,
            head_chars: 1000,
            tail_chars: 1000,
        };

        // Context window is large (100k tokens), 10k chars is ~2.5k tokens = 2.5% pressure
        let report = prune_tool_results_under_pressure(&mut items, 100_000, &policy);
        assert_eq!(report.pruned_results_count, 0);
        assert_eq!(report.chars_removed, 0);
        assert_eq!(items[0]["output"].as_str().unwrap().len(), 10_000);
    }

    #[test]
    fn above_pressure_ratio_prunes_oversized_tool_output() {
        let mut items = vec![json!({
            "type": "function_call_output",
            "call_id": "call_1",
            "output": format!("BEGINNING_{}_ENDING", "MIDDLE_DATA_".repeat(1000))
        })];

        let policy = ToolResultPrunePolicy {
            enabled: true,
            trigger_pressure_ratio: 0.50,
            max_result_chars: 1000,
            head_chars: 100,
            tail_chars: 100,
        };

        // Context window is small (2000 tokens), output is ~12k chars = ~3000 tokens = >100% pressure
        let report = prune_tool_results_under_pressure(&mut items, 2000, &policy);
        assert_eq!(report.pruned_results_count, 1);
        assert!(report.chars_removed > 0);
        assert!(report.post_prune_estimated_tokens < report.original_estimated_tokens);

        let output = items[0]["output"].as_str().unwrap();
        assert!(output.starts_with("BEGINNING_"));
        assert!(output.ends_with("ENDING"));
        assert!(output.contains("[tool result truncated for context efficiency:"));
        // Call ID is preserved intact!
        assert_eq!(items[0]["call_id"].as_str().unwrap(), "call_1");
    }

    #[test]
    fn disabled_policy_never_prunes() {
        let mut items = vec![json!({
            "type": "function_call_output",
            "call_id": "call_1",
            "output": "X".repeat(20_000)
        })];

        let policy = ToolResultPrunePolicy {
            enabled: false,
            ..Default::default()
        };

        let report = prune_tool_results_under_pressure(&mut items, 1000, &policy);
        assert_eq!(report.pruned_results_count, 0);
        assert_eq!(items[0]["output"].as_str().unwrap().len(), 20_000);
    }

    #[test]
    fn utf8_multibyte_safe_slicing() {
        // Multi-byte CJK characters: each character is 3 bytes in UTF-8
        let cjk = "你好世界測試程式碼".repeat(500);
        let mut items = vec![json!({
            "type": "function_call_output",
            "call_id": "call_cjk",
            "output": cjk
        })];

        let policy = ToolResultPrunePolicy {
            enabled: true,
            trigger_pressure_ratio: 0.10,
            max_result_chars: 500,
            head_chars: 50,
            tail_chars: 50,
        };

        let report = prune_tool_results_under_pressure(&mut items, 1000, &policy);
        assert_eq!(report.pruned_results_count, 1);
        let output = items[0]["output"].as_str().unwrap();
        assert!(output.contains("chars omitted"));
    }

    #[test]
    fn rich_tool_content_prunes_text_blocks_without_changing_identity_or_shape() {
        let mut items = vec![json!({
            "role": "tool",
            "tool_call_id": "call_rich",
            "content": [
                {"type": "text", "text": format!("HEAD{}TAIL", "x".repeat(5000))},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,abc"}},
                {"type": "output_text", "text": "short"}
            ]
        })];
        let policy = ToolResultPrunePolicy {
            trigger_pressure_ratio: 0.01,
            max_result_chars: 500,
            head_chars: 50,
            tail_chars: 50,
            ..Default::default()
        };

        let report = prune_tool_results_under_pressure(&mut items, 100, &policy);

        assert_eq!(report.pruned_results_count, 1);
        assert_eq!(items[0]["tool_call_id"], "call_rich");
        assert!(items[0]["content"].is_array());
        assert!(items[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("tool result truncated"));
        assert_eq!(items[0]["content"][1]["type"], "image_url");
        assert_eq!(items[0]["content"][2]["text"], "short");
    }
}
