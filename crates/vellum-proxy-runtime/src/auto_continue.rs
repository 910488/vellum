//! Bounded recovery for a Chat model that ends with only a progress promise.
//!
//! Chat Completions has no native equivalent of Responses' `commentary`
//! phase. A text-only Chat completion is normally a final answer. This guard
//! handles the narrower, observable failure where an agent has just received a
//! tool result and replies only with a short announcement of the next action.

use serde_json::{json, Value};

pub const MAX_AUTO_CONTINUE_RETRIES: u8 = 1;

const MAX_ANNOUNCEMENT_CHARS: usize = 240;

pub const AUTO_CONTINUE_PROMPT: &str = "Your previous response only announced the next action, so the requested task is still in progress. Continue now: issue the next native tool call in this response. If the task is actually complete or genuinely blocked, give the concrete result or blocker instead of another progress announcement.";

pub const EMPTY_CONTINUATION_PROMPT: &str = "The provider returned an empty successful stream after the last tool result. Continue the requested task now: issue the next native tool call in this response. If the task is complete or genuinely blocked, give the concrete result or blocker. Do not return an empty response.";

/// Only tool-result continuations with an executable tool surface qualify.
pub fn request_is_eligible(body: &Value) -> bool {
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    if !has_tools {
        return false;
    }

    crate::request::request_input_items(body)
        .iter()
        .rev()
        .find(|item| item.get("type").and_then(Value::as_str) != Some("reasoning"))
        .and_then(|item| item.get("type").and_then(Value::as_str))
        .is_some_and(|kind| {
            matches!(
                kind,
                "function_call_output" | "custom_tool_call_output" | "tool_search_output"
            )
        })
}

/// Extract a text-only assistant completion. Tool calls or multiple assistant
/// messages are deliberately ineligible because their intent is ambiguous.
pub fn text_only_completion(response: &Value) -> Option<String> {
    let output = response.get("output")?.as_array()?;
    if output.iter().any(|item| {
        matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call" | "custom_tool_call" | "tool_search_call")
        )
    }) {
        return None;
    }
    let mut messages = output
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"));
    let message = messages.next()?;
    if messages.next().is_some() {
        return None;
    }
    let text = message
        .get("content")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<String>();
    (!text.trim().is_empty()).then_some(text)
}

/// Recognize an announcement of future work, not arbitrary short text.
pub fn is_progress_announcement(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_ANNOUNCEMENT_CHARS {
        return false;
    }
    let lower = trimmed.to_lowercase();
    let english = [
        "continuing:",
        "continuing：",
        "continuing ",
        "i'll now ",
        "i will now ",
        "next, i'll ",
        "next, i will ",
        "now i'll ",
        "now i will ",
    ];
    let chinese = [
        "接下來我會",
        "接下来我会",
        "接著我會",
        "接着我会",
        "現在我會",
        "现在我会",
        "我現在會",
        "我现在会",
        "我將繼續",
        "我将继续",
        "繼續：",
        "继续：",
    ];
    english.iter().any(|prefix| lower.starts_with(prefix))
        || chinese.iter().any(|prefix| trimmed.starts_with(prefix))
}

pub fn should_auto_continue(body: &Value, response: &Value, retries: u8) -> Option<String> {
    if retries >= MAX_AUTO_CONTINUE_RETRIES || !request_is_eligible(body) {
        return None;
    }
    let text = text_only_completion(response)?;
    is_progress_announcement(&text).then_some(text)
}

/// Add the discarded progress announcement and a developer correction to the
/// next provider hop. They are dispatch-only context; the terminal exchange
/// persisted for the client remains the successful retry.
pub fn append_retry_context(body: &mut Value, announcement: &str) -> Result<(), String> {
    let input = body
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "auto-continue requires array input".to_string())?;
    input.push(json!({
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "phase": "commentary",
        "content": [{"type": "output_text", "text": announcement, "annotations": []}]
    }));
    input.push(json!({
        "type": "message",
        "role": "developer",
        "content": [{"type": "input_text", "text": AUTO_CONTINUE_PROMPT}]
    }));
    Ok(())
}

/// Perturb a cached or otherwise empty continuation without inventing an
/// assistant message that the provider never produced.
pub fn append_empty_retry_context(body: &mut Value) -> Result<(), String> {
    let input = body
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "empty-continuation recovery requires array input".to_string())?;
    input.push(json!({
        "type": "message",
        "role": "developer",
        "content": [{"type": "input_text", "text": EMPTY_CONTINUATION_PROMPT}]
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eligible_request() -> Value {
        json!({
            "tools": [{"type": "function", "name": "shell"}],
            "input": [
                {"type": "function_call", "call_id": "call_1", "name": "shell", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ssh: Permission denied"}
            ]
        })
    }

    fn completion(text: &str) -> Value {
        json!({
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}]
            }]
        })
    }

    #[test]
    fn captured_omen_completion_requests_one_retry() {
        let text = "Continuing: 偵測遠端 Mac 上的 node/pnpm 安裝位置。";
        assert_eq!(
            should_auto_continue(&eligible_request(), &completion(text), 0),
            Some(text.to_string())
        );
        assert_eq!(
            should_auto_continue(&eligible_request(), &completion(text), 1),
            None,
            "the same bad completion cannot loop forever"
        );
    }

    #[test]
    fn concrete_short_answers_are_not_retried() {
        for text in [
            "Node and pnpm are not installed on the remote Mac.",
            "已確認遠端沒有安裝 Node 與 pnpm。",
            "下一步建議是安裝 pnpm，但本次檢查已完成。",
        ] {
            assert_eq!(
                should_auto_continue(&eligible_request(), &completion(text), 0),
                None
            );
        }
    }

    #[test]
    fn requests_without_tools_or_a_tool_result_are_not_retried() {
        let answer = completion("Continuing: explain the result.");
        assert_eq!(
            should_auto_continue(&json!({"input": []}), &answer, 0),
            None
        );
        assert_eq!(
            should_auto_continue(
                &json!({
                    "tools": [{"type": "function", "name": "shell"}],
                    "input": [{"type": "message", "role": "user", "content": "continue"}]
                }),
                &answer,
                0
            ),
            None
        );
    }

    #[test]
    fn retry_context_keeps_the_announcement_as_commentary() {
        let mut body = eligible_request();
        append_retry_context(&mut body, "Continuing: inspect node.").unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[input.len() - 2]["phase"], "commentary");
        assert_eq!(input.last().unwrap()["role"], "developer");
        assert!(input.last().unwrap().to_string().contains("Continue now"));
    }

    #[test]
    fn empty_retry_context_adds_only_a_developer_correction() {
        let mut body = eligible_request();
        let before = body["input"].as_array().unwrap().len();
        append_empty_retry_context(&mut body).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), before + 1);
        assert_eq!(input.last().unwrap()["role"], "developer");
        assert!(input
            .last()
            .unwrap()
            .to_string()
            .contains("empty successful stream"));
    }
}
