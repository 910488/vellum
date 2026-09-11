//! Harness-neutral event -> Codex App Server output.
//!
//! The second of the two mapping stages. Adapters produce neutral events; only
//! this stage knows what the Codex UI expects. Every method, item type and
//! status named here comes from the pinned schema and is verified by
//! `tests/schema_conformance.rs`.
//!
//! Two consequences of speaking real Codex protocol rather than a convenient
//! one:
//!
//! * A permission request is a *server request*, not a notification, so it is
//!   emitted as [`CodexServerMessage::Request`] and the UI answers with a
//!   JSON-RPC response.
//! * Some neutral events have no Codex equivalent at all — a mid-flight tool
//!   update, a provider-specific extension. Those map to zero Codex messages.
//!   They are not lost: the journal keeps the neutral event, and runtime
//!   subscribers still see it. Inventing a notification the UI will never
//!   understand would be worse than saying nothing.

use serde_json::{json, Value};
use vellum_harness_protocol::{HarnessEvent, HarnessEventEnvelope};

use crate::methods::{item, notify, request, tool_status};

#[derive(Debug, Clone, PartialEq)]
pub struct CodexServerNotification {
    pub method: String,
    pub params: Value,
}

/// Anything the facade sends towards the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum CodexServerMessage {
    Notification(CodexServerNotification),
    /// A server-originated request the UI must answer. `id` is the JSON-RPC id
    /// the facade will correlate the response against.
    Request {
        id: String,
        method: String,
        params: Value,
    },
}

impl CodexServerMessage {
    pub fn as_notification(&self) -> Option<&CodexServerNotification> {
        match self {
            Self::Notification(notification) => Some(notification),
            Self::Request { .. } => None,
        }
    }

    pub fn method(&self) -> &str {
        match self {
            Self::Notification(notification) => &notification.method,
            Self::Request { method, .. } => method,
        }
    }
}

/// What the mapper needs beyond the envelope itself.
#[derive(Debug, Clone, Default)]
pub struct UiMapContext {
    /// The turn currently in flight, tracked by the facade's event pump.
    pub turn_id: Option<String>,
    /// Vellum-issued permission id, present only for a permission request.
    pub ui_request_id: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodexMapError {
    #[error("event cannot be represented to the Codex UI: {0}")]
    Unrepresentable(String),
}

pub trait CodexUiEventMapper: Send + Sync {
    fn map_event(
        &self,
        envelope: &HarnessEventEnvelope,
        context: &UiMapContext,
    ) -> Result<Vec<CodexServerMessage>, CodexMapError>;
}

#[derive(Debug, Default, Clone)]
pub struct DefaultCodexUiEventMapper;

impl CodexUiEventMapper for DefaultCodexUiEventMapper {
    fn map_event(
        &self,
        envelope: &HarnessEventEnvelope,
        context: &UiMapContext,
    ) -> Result<Vec<CodexServerMessage>, CodexMapError> {
        let thread_id = envelope.thread_id.as_str();
        let turn_id = context.turn_id.clone().unwrap_or_default();
        let notify = |method: &str, params: Value| {
            vec![CodexServerMessage::Notification(CodexServerNotification {
                method: method.to_owned(),
                params,
            })]
        };
        let started = |item: Value| {
            notify(
                notify::ITEM_STARTED,
                json!({
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "item": item,
                    "startedAtMs": envelope.occurred_at.timestamp_millis()
                }),
            )
        };
        let completed = |item: Value| {
            notify(
                notify::ITEM_COMPLETED,
                json!({
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "item": item,
                    "completedAtMs": envelope.occurred_at.timestamp_millis()
                }),
            )
        };

        Ok(match &envelope.event {
            HarnessEvent::TurnStarted(turn) => notify(
                notify::TURN_STARTED,
                json!({
                    "threadId": thread_id,
                    "turn": {"id": turn.turn_id.clone().unwrap_or_else(|| turn_id.clone())}
                }),
            ),
            HarnessEvent::TurnCompleted(turn) => notify(
                notify::TURN_COMPLETED,
                json!({
                    "threadId": thread_id,
                    "turn": {"id": turn.turn_id.clone().unwrap_or_else(|| turn_id.clone())}
                }),
            ),
            HarnessEvent::AssistantDelta(message) => notify(
                notify::AGENT_MESSAGE_DELTA,
                json!({
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "itemId": item_id(message.message_id.as_deref(), "agentMessage", envelope.seq),
                    "delta": message.delta
                }),
            ),
            HarnessEvent::AssistantMessage(message) => completed(json!({
                "type": item::AGENT_MESSAGE,
                "id": item_id(message.message_id.as_deref(), "agentMessage", envelope.seq),
                "text": message.text
            })),
            HarnessEvent::Reasoning(reasoning) => notify(
                notify::REASONING_TEXT_DELTA,
                json!({
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "itemId": item_id(None, "reasoning", envelope.seq),
                    "contentIndex": 0,
                    "delta": reasoning.text
                }),
            ),
            HarnessEvent::ToolCallStarted(call) => started(json!({
                "type": item::DYNAMIC_TOOL_CALL,
                "id": call.tool_call_id,
                "tool": call.name,
                "arguments": call.arguments.clone().unwrap_or(Value::Null),
                "status": tool_status::IN_PROGRESS
            })),
            // Codex has no mid-flight item update: a tool call is reported
            // started, then completed. The neutral event is kept in the journal
            // rather than forced into a notification that does not exist.
            HarnessEvent::ToolCallUpdated(_) => Vec::new(),
            HarnessEvent::ToolCallCompleted(result) => completed(json!({
                "type": item::DYNAMIC_TOOL_CALL,
                "id": result.tool_call_id,
                // Falls back to the call id only when the harness did not name
                // the tool on completion; never a fabricated name.
                "tool": result.name.clone().unwrap_or_else(|| result.tool_call_id.clone()),
                "arguments": Value::Null,
                "status": if result.is_error { tool_status::FAILED } else { tool_status::COMPLETED },
                "success": !result.is_error,
                "contentItems": tool_output(&result.result)
            })),
            HarnessEvent::PermissionRequested(permission) => {
                let ui_request_id = context.ui_request_id.clone().ok_or_else(|| {
                    CodexMapError::Unrepresentable(
                        "permission request reached the UI without a registered binding".into(),
                    )
                })?;
                vec![CodexServerMessage::Request {
                    id: ui_request_id,
                    method: request::PERMISSIONS_REQUEST_APPROVAL.into(),
                    params: json!({
                        "threadId": thread_id,
                        "turnId": turn_id,
                        "itemId": permission.native_request_id,
                        "cwd": permission.payload.get("cwd").cloned().unwrap_or(Value::Null),
                        "permissions": permission.payload.get("permissions").cloned().unwrap_or(Value::Null),
                        "reason": permission.description,
                        "startedAtMs": envelope.occurred_at.timestamp_millis()
                    }),
                }]
            }
            HarnessEvent::PlanUpdated(plan) => notify(
                notify::PLAN_UPDATED,
                json!({"threadId": thread_id, "turnId": turn_id, "plan": plan.plan}),
            ),
            HarnessEvent::UsageUpdated(usage) => notify(
                notify::TOKEN_USAGE_UPDATED,
                json!({"threadId": thread_id, "usage": usage.usage}),
            ),
            // Codex announces a completed compaction; a start has no
            // notification of its own.
            HarnessEvent::CompactionStarted(_) => Vec::new(),
            HarnessEvent::CompactionCompleted(_) => {
                notify(notify::THREAD_COMPACTED, json!({"threadId": thread_id}))
            }
            HarnessEvent::SubagentStarted(subagent) => started(subagent_item(subagent)),
            HarnessEvent::SubagentCompleted(subagent) => completed(subagent_item(subagent)),
            // As with tool updates, there is no item-updated notification.
            HarnessEvent::SubagentUpdated(_) => Vec::new(),
            HarnessEvent::Error(error) => notify(
                notify::ERROR,
                json!({"threadId": thread_id, "message": error.error.message}),
            ),
            // No Codex equivalent exists. Preserved in the journal and on the
            // neutral event stream instead of being invented into protocol.
            HarnessEvent::SessionState(_) | HarnessEvent::NativeExtension(_) => Vec::new(),
        })
    }
}

fn subagent_item(subagent: &vellum_harness_protocol::SubagentEvent) -> Value {
    json!({
        "type": item::SUB_AGENT_ACTIVITY,
        "id": subagent.native_subagent_id,
        "agentThreadId": subagent.native_subagent_id,
        "agentPath": subagent.name.clone().unwrap_or_default(),
        "kind": subagent.status
    })
}

/// Codex expects tool output as content items; a bare JSON result is wrapped.
fn tool_output(result: &Value) -> Value {
    match result {
        Value::Null => Value::Null,
        Value::String(text) => json!([{"type": "text", "text": text}]),
        other => json!([{"type": "text", "text": other.to_string()}]),
    }
}

fn item_id(native: Option<&str>, prefix: &str, seq: u64) -> String {
    native
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{prefix}-{seq}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use vellum_harness_protocol::{
        HarnessId, MessageDeltaEvent, NativeExtensionEvent, PermissionRequestEvent, ToolCallEvent,
        ToolResultEvent,
    };

    fn envelope(event: HarnessEvent) -> HarnessEventEnvelope {
        HarnessEventEnvelope {
            seq: 3,
            harness_id: HarnessId("grok-build".into()),
            thread_id: "ui-1".into(),
            native_session_id: "native-1".into(),
            native_event_id: None,
            occurred_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            event,
        }
    }

    fn context() -> UiMapContext {
        UiMapContext {
            turn_id: Some("turn-1".into()),
            ui_request_id: None,
        }
    }

    fn map(event: HarnessEvent, context: &UiMapContext) -> Vec<CodexServerMessage> {
        DefaultCodexUiEventMapper
            .map_event(&envelope(event), context)
            .unwrap()
    }

    #[test]
    fn assistant_deltas_use_the_real_agent_message_delta_notification() {
        let mapped = map(
            HarnessEvent::AssistantDelta(MessageDeltaEvent {
                message_id: Some("m-1".into()),
                delta: "hello".into(),
            }),
            &context(),
        );
        let notification = mapped[0].as_notification().unwrap();
        assert_eq!(notification.method, "item/agentMessage/delta");
        assert_eq!(notification.params["threadId"], json!("ui-1"));
        assert_eq!(notification.params["turnId"], json!("turn-1"));
        assert_eq!(notification.params["itemId"], json!("m-1"));
        assert_eq!(notification.params["delta"], json!("hello"));
    }

    #[test]
    fn a_tool_call_becomes_a_dynamic_tool_call_item_pair() {
        let start = map(
            HarnessEvent::ToolCallStarted(ToolCallEvent {
                tool_call_id: "call-1".into(),
                name: "read_file".into(),
                arguments: Some(json!({"path": "a.rs"})),
            }),
            &context(),
        );
        let started = start[0].as_notification().unwrap();
        assert_eq!(started.method, "item/started");
        assert_eq!(started.params["item"]["type"], json!("dynamicToolCall"));
        assert_eq!(started.params["item"]["status"], json!("inProgress"));
        assert_eq!(started.params["item"]["tool"], json!("read_file"));

        let end = map(
            HarnessEvent::ToolCallCompleted(ToolResultEvent {
                tool_call_id: "call-1".into(),
                name: Some("read_file".into()),
                result: json!("done"),
                is_error: false,
            }),
            &context(),
        );
        let finished = end[0].as_notification().unwrap();
        assert_eq!(finished.method, "item/completed");
        assert_eq!(finished.params["item"]["status"], json!("completed"));
        // The tool is named, not identified by its opaque call id.
        assert_eq!(finished.params["item"]["tool"], json!("read_file"));
        assert_eq!(finished.params["item"]["success"], json!(true));

        let failed = map(
            HarnessEvent::ToolCallCompleted(ToolResultEvent {
                tool_call_id: "call-1".into(),
                name: Some("read_file".into()),
                result: json!("boom"),
                is_error: true,
            }),
            &context(),
        );
        assert_eq!(
            failed[0].as_notification().unwrap().params["item"]["status"],
            json!("failed")
        );
    }

    #[test]
    fn a_permission_is_a_server_request_and_hides_the_native_id() {
        let event = HarnessEvent::PermissionRequested(PermissionRequestEvent {
            native_request_id: "call-7".into(),
            description: "write file".into(),
            payload: json!({"cwd": "C:/workspace"}),
        });
        let mapped = map(
            event.clone(),
            &UiMapContext {
                turn_id: Some("turn-1".into()),
                ui_request_id: Some("perm_public".into()),
            },
        );
        match &mapped[0] {
            CodexServerMessage::Request { id, method, params } => {
                assert_eq!(id, "perm_public");
                assert_eq!(method, "item/permissions/requestApproval");
                assert_eq!(params["reason"], json!("write file"));
                assert_eq!(params["threadId"], json!("ui-1"));
            }
            other => panic!("permission must be a server request, got {other:?}"),
        }

        // Without a registered binding there is no id the UI could answer with.
        assert!(DefaultCodexUiEventMapper
            .map_event(&envelope(event), &context())
            .is_err());
    }

    #[test]
    fn events_with_no_codex_equivalent_map_to_nothing_rather_than_invented_protocol() {
        for event in [
            HarnessEvent::NativeExtension(NativeExtensionEvent {
                namespace: "xai".into(),
                event_type: "xai/doom_loop_check".into(),
                payload: json!({"repeats": 2}),
            }),
            HarnessEvent::ToolCallUpdated(ToolCallEvent {
                tool_call_id: "call-1".into(),
                name: "read_file".into(),
                arguments: None,
            }),
        ] {
            assert!(
                map(event.clone(), &context()).is_empty(),
                "{event:?} must not be forced into a Codex notification"
            );
        }
    }
}
