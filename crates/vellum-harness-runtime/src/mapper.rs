//! Native protocol -> harness-neutral event mapping.
//!
//! This is deliberately a separate stage from the UI mapping. A native
//! notification becomes zero or more [`HarnessEvent`]s and never a Codex
//! notification directly, so replacing the UI does not require rewriting every
//! adapter.
//!
//! Anything the mapper does not recognise is preserved verbatim as a
//! [`NativeExtensionEvent`] rather than dropped.

use serde_json::Value;
use vellum_harness_protocol::{
    CompactionEvent, HarnessErrorCategory, HarnessErrorEvent, HarnessErrorInfo, HarnessEvent,
    MessageDeltaEvent, MessageEvent, NativeExtensionEvent, PermissionRequestEvent, PlanEvent,
    ReasoningEvent, SessionStateEvent, SubagentEvent, ToolCallEvent, ToolResultEvent, TurnEvent,
    UsageEvent,
};

/// Context a mapper needs that the native payload does not carry itself.
#[derive(Debug, Clone)]
pub struct EventMapContext {
    pub native_session_id: String,
    /// Namespace for native events with no neutral equivalent, e.g. `xai`.
    pub extension_namespace: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HarnessMapError {
    #[error("native payload is not a JSON object: {0}")]
    NotAnObject(String),
}

pub trait NativeEventMapper: Send + Sync {
    fn map_notification(
        &self,
        method: &str,
        params: &Value,
        context: &EventMapContext,
    ) -> Result<Vec<HarnessEvent>, HarnessMapError>;
}

/// Maps the ACP surface shared by Grok Build, Qwen Code and DeepSeek Harness.
#[derive(Debug, Default, Clone)]
pub struct AcpEventMapper;

impl AcpEventMapper {
    /// A payload without a matching session id is never mapped into that
    /// session; cross-session leakage is a correctness bug, not a UI filter.
    pub fn belongs_to_session(params: &Value, native_session_id: &str) -> bool {
        params
            .get("sessionId")
            .and_then(Value::as_str)
            .is_some_and(|id| id == native_session_id)
    }
}

impl NativeEventMapper for AcpEventMapper {
    fn map_notification(
        &self,
        method: &str,
        params: &Value,
        context: &EventMapContext,
    ) -> Result<Vec<HarnessEvent>, HarnessMapError> {
        if !params.is_object() {
            return Err(HarnessMapError::NotAnObject(params.to_string()));
        }
        match method {
            "session/update" => Ok(map_session_update(params, context, method)),
            "session/request_permission" => Ok(vec![HarnessEvent::PermissionRequested(
                PermissionRequestEvent {
                    native_request_id: params
                        .get("toolCall")
                        .and_then(|call| call.get("toolCallId"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    description: params
                        .get("toolCall")
                        .and_then(|call| call.get("title"))
                        .and_then(Value::as_str)
                        .unwrap_or("permission requested")
                        .to_owned(),
                    payload: params.clone(),
                },
            )]),
            "session/error" => Ok(vec![HarnessEvent::Error(HarnessErrorEvent {
                error: HarnessErrorInfo {
                    category: HarnessErrorCategory::Provider,
                    message: params
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("native harness reported an error")
                        .to_owned(),
                    diagnostic: Some(params.to_string()),
                },
            })]),
            _ => Ok(preserve(params, context, method)),
        }
    }
}

fn map_session_update(
    params: &Value,
    context: &EventMapContext,
    method: &str,
) -> Vec<HarnessEvent> {
    let Some(update) = params.get("update") else {
        return preserve(params, context, method);
    };
    match update.get("sessionUpdate").and_then(Value::as_str) {
        Some("agent_message_chunk") => text_of(update)
            .map(|delta| {
                vec![HarnessEvent::AssistantDelta(MessageDeltaEvent {
                    message_id: string_of(update, "messageId"),
                    delta,
                })]
            })
            .unwrap_or_else(|| preserve(params, context, method)),
        Some("agent_message") => text_of(update)
            .map(|text| {
                vec![HarnessEvent::AssistantMessage(MessageEvent {
                    message_id: string_of(update, "messageId"),
                    text,
                })]
            })
            .unwrap_or_else(|| preserve(params, context, method)),
        Some("agent_thought_chunk") => text_of(update)
            .map(|text| vec![HarnessEvent::Reasoning(ReasoningEvent { text })])
            .unwrap_or_else(|| preserve(params, context, method)),
        Some("tool_call") => vec![HarnessEvent::ToolCallStarted(ToolCallEvent {
            tool_call_id: string_of(update, "toolCallId").unwrap_or_default(),
            name: string_of(update, "title")
                .or_else(|| string_of(update, "kind"))
                .unwrap_or_else(|| "tool".into()),
            arguments: update.get("rawInput").cloned(),
        })],
        Some("tool_call_update") => {
            let tool_call_id = string_of(update, "toolCallId").unwrap_or_default();
            match update.get("status").and_then(Value::as_str) {
                Some(status @ ("completed" | "failed")) => {
                    vec![HarnessEvent::ToolCallCompleted(ToolResultEvent {
                        tool_call_id,
                        name: string_of(update, "title"),
                        result: update
                            .get("rawOutput")
                            .or_else(|| update.get("content"))
                            .cloned()
                            .unwrap_or(Value::Null),
                        is_error: status == "failed",
                    })]
                }
                _ => vec![HarnessEvent::ToolCallUpdated(ToolCallEvent {
                    tool_call_id,
                    name: string_of(update, "title").unwrap_or_else(|| "tool".into()),
                    arguments: update.get("rawInput").cloned(),
                })],
            }
        }
        Some("plan") => vec![HarnessEvent::PlanUpdated(PlanEvent {
            plan: update.get("entries").cloned().unwrap_or(Value::Null),
        })],
        Some("usage" | "context_usage") => vec![HarnessEvent::UsageUpdated(UsageEvent {
            usage: update.clone(),
        })],
        Some("turn_started") => vec![HarnessEvent::TurnStarted(TurnEvent {
            turn_id: string_of(update, "turnId"),
        })],
        Some("turn_completed") => vec![HarnessEvent::TurnCompleted(TurnEvent {
            turn_id: string_of(update, "turnId"),
        })],
        Some("compaction_started") => vec![HarnessEvent::CompactionStarted(CompactionEvent {
            detail: string_of(update, "detail"),
        })],
        Some("compaction_completed") => vec![HarnessEvent::CompactionCompleted(CompactionEvent {
            detail: string_of(update, "detail"),
        })],
        Some(subagent @ ("subagent_started" | "subagent_update" | "subagent_completed")) => {
            let event = SubagentEvent {
                parent_session_id: context.native_session_id.clone(),
                native_subagent_id: string_of(update, "subagentId").unwrap_or_default(),
                name: string_of(update, "name"),
                status: string_of(update, "status").unwrap_or_else(|| subagent.into()),
            };
            vec![match subagent {
                "subagent_started" => HarnessEvent::SubagentStarted(event),
                "subagent_completed" => HarnessEvent::SubagentCompleted(event),
                _ => HarnessEvent::SubagentUpdated(event),
            }]
        }
        Some("current_mode_update") => vec![HarnessEvent::SessionState(SessionStateEvent {
            state: string_of(update, "currentModeId").unwrap_or_else(|| "unknown".into()),
        })],
        _ => preserve(params, context, method),
    }
}

fn preserve(params: &Value, context: &EventMapContext, method: &str) -> Vec<HarnessEvent> {
    vec![HarnessEvent::NativeExtension(NativeExtensionEvent {
        namespace: context.extension_namespace.clone(),
        event_type: method.to_owned(),
        payload: params.clone(),
    })]
}

fn string_of(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn text_of(update: &Value) -> Option<String> {
    update
        .get("content")
        .and_then(|content| content.get("text"))
        .or_else(|| update.get("text"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context() -> EventMapContext {
        EventMapContext {
            native_session_id: "native-a".into(),
            extension_namespace: "xai".into(),
        }
    }

    fn map(method: &str, params: Value) -> Vec<HarnessEvent> {
        AcpEventMapper
            .map_notification(method, &params, &context())
            .unwrap()
    }

    fn update(body: Value) -> Value {
        json!({"sessionId": "native-a", "update": body})
    }

    #[test]
    fn maps_the_full_turn_lifecycle_of_a_recorded_acp_session() {
        assert!(matches!(
            map(
                "session/update",
                update(json!({"sessionUpdate":"turn_started","turnId":"t-1"}))
            )
            .as_slice(),
            [HarnessEvent::TurnStarted(TurnEvent { turn_id })] if turn_id.as_deref() == Some("t-1")
        ));
        assert!(matches!(
            map("session/update", update(json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}))).as_slice(),
            [HarnessEvent::Reasoning(ReasoningEvent { text })] if text == "thinking"
        ));
        assert!(matches!(
            map("session/update", update(json!({"sessionUpdate":"tool_call","toolCallId":"call-1","title":"read_file","rawInput":{"path":"a.rs"}}))).as_slice(),
            [HarnessEvent::ToolCallStarted(ToolCallEvent { tool_call_id, name, .. })] if tool_call_id == "call-1" && name == "read_file"
        ));
        assert!(matches!(
            map("session/update", update(json!({"sessionUpdate":"tool_call_update","toolCallId":"call-1","status":"in_progress"}))).as_slice(),
            [HarnessEvent::ToolCallUpdated(_)]
        ));
        assert!(matches!(
            map("session/update", update(json!({"sessionUpdate":"tool_call_update","toolCallId":"call-1","status":"completed","rawOutput":{"ok":true}}))).as_slice(),
            [HarnessEvent::ToolCallCompleted(ToolResultEvent { is_error: false, .. })]
        ));
        assert!(matches!(
            map("session/update", update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}}))).as_slice(),
            [HarnessEvent::AssistantDelta(MessageDeltaEvent { delta, .. })] if delta == "done"
        ));
        assert!(matches!(
            map(
                "session/update",
                update(json!({"sessionUpdate":"turn_completed","turnId":"t-1"}))
            )
            .as_slice(),
            [HarnessEvent::TurnCompleted(_)]
        ));
    }

    #[test]
    fn failed_tool_calls_keep_their_error_flag() {
        assert!(matches!(
            map("session/update", update(json!({"sessionUpdate":"tool_call_update","toolCallId":"call-9","status":"failed","rawOutput":"boom"}))).as_slice(),
            [HarnessEvent::ToolCallCompleted(ToolResultEvent { is_error: true, .. })]
        ));
    }

    #[test]
    fn permission_requests_carry_the_native_correlation_id() {
        let events = map(
            "session/request_permission",
            json!({"sessionId":"native-a","toolCall":{"toolCallId":"call-7","title":"write file"}}),
        );
        assert!(matches!(
            events.as_slice(),
            [HarnessEvent::PermissionRequested(PermissionRequestEvent { native_request_id, description, .. })]
                if native_request_id == "call-7" && description == "write file"
        ));
    }

    #[test]
    fn unknown_native_events_are_preserved_rather_than_dropped() {
        let payload = json!({"sessionId":"native-a","goal":"ship it","nested":[1,2,3]});
        let events = map("xai/goal_revision", payload.clone());
        assert!(matches!(
            events.as_slice(),
            [HarnessEvent::NativeExtension(NativeExtensionEvent { namespace, event_type, payload: kept })]
                if namespace == "xai" && event_type == "xai/goal_revision" && kept == &payload
        ));
    }

    #[test]
    fn unknown_session_update_kinds_are_preserved_with_their_whole_payload() {
        let params = update(json!({"sessionUpdate":"available_commands_update","commands":["/x"]}));
        assert!(matches!(
            map("session/update", params.clone()).as_slice(),
            [HarnessEvent::NativeExtension(NativeExtensionEvent { payload, .. })] if payload == &params
        ));
    }

    #[test]
    fn payloads_for_another_session_are_never_claimed_by_this_one() {
        let other =
            json!({"sessionId":"native-b","update":{"sessionUpdate":"agent_message_chunk"}});
        assert!(!AcpEventMapper::belongs_to_session(&other, "native-a"));
        assert!(AcpEventMapper::belongs_to_session(
            &json!({"sessionId":"native-a"}),
            "native-a"
        ));
        assert!(!AcpEventMapper::belongs_to_session(&json!({}), "native-a"));
    }

    #[test]
    fn non_object_payloads_are_a_protocol_error_not_a_silent_drop() {
        assert!(AcpEventMapper
            .map_notification("session/update", &json!("nope"), &context())
            .is_err());
    }
}
