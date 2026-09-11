//! Non-streaming response normalization (M3C).
//!
//! The exact decode+narrow step Desktop's production `forward_response`
//! applies to a buffered provider response before it reaches Codex. The
//! `responses_*` parity fixtures freeze these semantics; this module is the
//! shared-runtime implementation those fixtures will guard once the handler
//! body is migrated.
//!
//! Order matters and is fixed here: Chat wire is adapted to Responses first,
//! then custom/namespace tool calls are restored to their Codex-native forms,
//! then third-party readable reasoning is folded into summaries (and opaque
//! ciphertext is dropped), then messages that still lack a phase get one
//! (`final_answer`, or `commentary` when the turn contains tool calls).

use serde_json::Value;

use crate::adapter::{
    chat_response_to_responses_with_context, CodexToolContext, NamespaceToolContext,
};
use crate::route::RuntimeWireFormat;

/// Decode and normalize one non-streaming upstream response.
///
/// `request` is the original client request (used to rebuild the
/// custom/namespace tool contexts), `wire` decides whether the body is a
/// Chat completion needing the Responses adapter or an already-Responses
/// body that passes through, and `model` names the upstream model.
pub fn normalize_non_streaming_response(
    request: &Value,
    wire: RuntimeWireFormat,
    model: &str,
    body: Value,
) -> Result<Value, String> {
    let tool_context = CodexToolContext::from_request(request);
    let mut normalized = match wire {
        RuntimeWireFormat::Responses => body,
        RuntimeWireFormat::Chat => {
            chat_response_to_responses_with_context(&body, model, &tool_context)?
        }
    };
    tool_context.restore_response_tools(&mut normalized);
    NamespaceToolContext::from_request(request).restore_response(&mut normalized);
    normalize_third_party_readable_reasoning(&mut normalized);
    assign_response_message_phases(&mut normalized);
    Ok(normalized)
}

fn response_contains_function_call(response: &Value) -> bool {
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

/// Assign `phase` to every message that lacks one: `final_answer`, or
/// `commentary` when the turn contains a tool call.
fn assign_response_message_phases(response: &mut Value) {
    let phase = if response_contains_function_call(response) {
        "commentary"
    } else {
        "final_answer"
    };
    let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) else {
        return;
    };
    for item in output {
        let Some(object) = item.as_object_mut() else {
            continue;
        };
        if object.get("type").and_then(Value::as_str) == Some("message")
            && object.get("phase").is_none_or(Value::is_null)
        {
            object.insert("phase".into(), Value::String(phase.into()));
        }
    }
}

/// Fold third-party readable reasoning (`reasoning_content` / `content`) into
/// a `summary`, and strip opaque provider ciphertext and raw reasoning text
/// from reasoning items so nothing provider-private reaches Codex history.
fn normalize_third_party_readable_reasoning(value: &mut Value) {
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
                            serde_json::json!([{"type": "summary_text", "text": readable}]),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn responses_passthrough_strips_ciphertext_and_assigns_final_answer() {
        let request = json!({"model": "m", "input": "hi"});
        let body = json!({
            "id": "resp_1",
            "object": "response",
            "status": "completed",
            "output": [
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "encrypted_content": "opaque",
                    "summary": [{"type": "summary_text", "text": "thought"}]
                },
                {
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "hi back", "annotations": []}]
                }
            ],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        });
        let normalized =
            normalize_non_streaming_response(&request, RuntimeWireFormat::Responses, "m", body)
                .unwrap();
        assert!(!normalized.to_string().contains("encrypted_content"));
        assert!(!normalized.to_string().contains("opaque"));
        assert_eq!(normalized["output"][1]["phase"], "final_answer");
    }

    #[test]
    fn responses_multi_tool_turn_is_preserved_and_phase_is_commentary() {
        let request = json!({
            "model": "m",
            "input": "check both cities",
            "tools": [{"type": "function", "name": "get_weather"}]
        });
        let body = json!({
            "id": "resp_1",
            "object": "response",
            "status": "completed",
            "output": [
                {"type": "function_call", "id": "fc_1", "call_id": "call_a", "name": "get_weather", "arguments": "{\"city\":\"Taipei\"}", "status": "completed"},
                {"type": "function_call", "id": "fc_2", "call_id": "call_b", "name": "get_weather", "arguments": "{\"city\":\"Tokyo\"}", "status": "completed"},
                {"type": "function_call_output", "call_id": "call_a", "output": "{\"temp\":32}"},
                {"type": "function_call_output", "call_id": "call_b", "output": "{\"temp\":28}"},
                {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "both warm", "annotations": []}]}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 8, "total_tokens": 18}
        });
        let normalized =
            normalize_non_streaming_response(&request, RuntimeWireFormat::Responses, "m", body)
                .unwrap();
        let output = normalized["output"].as_array().unwrap();
        assert_eq!(output[0]["call_id"], "call_a");
        assert_eq!(output[1]["call_id"], "call_b");
        assert_eq!(output[2]["type"], "function_call_output");
        assert_eq!(output[4]["phase"], "commentary");
    }

    #[test]
    fn chat_wire_is_adapted_to_responses() {
        let request = json!({"model": "m", "input": "hi"});
        let body = json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "hi from chat"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
        });
        let normalized =
            normalize_non_streaming_response(&request, RuntimeWireFormat::Chat, "m", body).unwrap();
        assert_eq!(normalized["object"], "response");
        assert_eq!(normalized["output"][0]["type"], "message");
        assert_eq!(normalized["output"][0]["phase"], "final_answer");
        assert_eq!(normalized["usage"]["input_tokens"], 1);
    }

    #[test]
    fn readable_reasoning_content_is_folded_into_summary() {
        let request = json!({"model": "m"});
        let body = json!({
            "id": "resp_1",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "reasoning",
                "id": "rs_1",
                "reasoning_content": "provider thought",
                "content": "raw hidden"
            }]
        });
        let normalized =
            normalize_non_streaming_response(&request, RuntimeWireFormat::Responses, "m", body)
                .unwrap();
        let reasoning = &normalized["output"][0];
        assert_eq!(reasoning["summary"][0]["text"], "provider thought");
        assert!(reasoning.get("reasoning_content").is_none());
        assert!(reasoning.get("content").is_none());
    }

    #[test]
    fn custom_tool_calls_are_restored_for_codex_replay() {
        let request = json!({
            "model": "m",
            "tools": [{"type": "custom", "name": "apply_patch"}]
        });
        let body = json!({
            "id": "resp_1",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_patch",
                "name": "apply_patch",
                "arguments": "{\"patch\":\"@@ -1 +1 @@\"}",
                "status": "completed"
            }]
        });
        let normalized =
            normalize_non_streaming_response(&request, RuntimeWireFormat::Responses, "m", body)
                .unwrap();
        let call = &normalized["output"][0];
        assert_eq!(call["type"], "custom_tool_call");
        assert!(call.get("arguments").is_none());
        assert!(call["input"].as_str().unwrap().contains("-1 +1"));
    }

    #[test]
    fn declared_tool_search_is_restored_from_a_compatible_function_call() {
        let request = json!({
            "model": "m",
            "tools": [{"type": "tool_search", "execution": "client"}]
        });
        let body = json!({
            "id": "resp_1",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "function_call",
                "id": "fc_provider",
                "call_id": "call_search",
                "name": "tool_search",
                "arguments": "{\"query\":\"spawn subagent\",\"limit\":5}",
                "status": "completed"
            }]
        });

        let normalized =
            normalize_non_streaming_response(&request, RuntimeWireFormat::Responses, "m", body)
                .unwrap();
        let call = &normalized["output"][0];
        assert_eq!(call["type"], "tool_search_call");
        assert!(call["id"].as_str().unwrap().starts_with("tsc_vellum_"));
        assert_eq!(call["call_id"], "call_search");
        assert_eq!(call["arguments"]["query"], "spawn subagent");
        assert_eq!(call["arguments"]["limit"], 5);
        assert_eq!(call["execution"], "client");
        assert!(call.get("name").is_none());
    }

    #[test]
    fn undeclared_function_named_tool_search_is_not_retyped() {
        let request = json!({"model": "m", "tools": []});
        let body = json!({
            "id": "resp_1",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "function_call",
                "id": "fc_provider",
                "call_id": "call_search",
                "name": "tool_search",
                "arguments": "{}",
                "status": "completed"
            }]
        });

        let normalized =
            normalize_non_streaming_response(&request, RuntimeWireFormat::Responses, "m", body)
                .unwrap();
        assert_eq!(normalized["output"][0]["type"], "function_call");
        assert_eq!(normalized["output"][0]["name"], "tool_search");
    }
}
