use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{HarnessErrorInfo, HarnessId};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessEventEnvelope {
    pub seq: u64,
    pub harness_id: HarnessId,
    pub thread_id: String,
    pub native_session_id: String,
    pub native_event_id: Option<String>,
    pub occurred_at: DateTime<Utc>,
    pub event: HarnessEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
pub enum HarnessEvent {
    SessionState(SessionStateEvent),
    AssistantMessage(MessageEvent),
    AssistantDelta(MessageDeltaEvent),
    Reasoning(ReasoningEvent),
    ToolCallStarted(ToolCallEvent),
    ToolCallUpdated(ToolCallEvent),
    ToolCallCompleted(ToolResultEvent),
    PermissionRequested(PermissionRequestEvent),
    PlanUpdated(PlanEvent),
    UsageUpdated(UsageEvent),
    CompactionStarted(CompactionEvent),
    CompactionCompleted(CompactionEvent),
    SubagentStarted(SubagentEvent),
    SubagentUpdated(SubagentEvent),
    SubagentCompleted(SubagentEvent),
    TurnStarted(TurnEvent),
    TurnCompleted(TurnEvent),
    Error(HarnessErrorEvent),
    NativeExtension(NativeExtensionEvent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStateEvent {
    pub state: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEvent {
    pub message_id: Option<String>,
    pub text: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageDeltaEvent {
    pub message_id: Option<String>,
    pub delta: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEvent {
    pub text: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallEvent {
    pub tool_call_id: String,
    pub name: String,
    pub arguments: Option<Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultEvent {
    pub tool_call_id: String,
    /// The tool that ran. Carried on completion too, so a consumer that only
    /// sees the result does not have to display the call id as a name.
    pub name: Option<String>,
    pub result: Value,
    pub is_error: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequestEvent {
    pub native_request_id: String,
    pub description: String,
    pub payload: Value,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanEvent {
    pub plan: Value,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageEvent {
    pub usage: Value,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEvent {
    pub detail: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentEvent {
    pub parent_session_id: String,
    pub native_subagent_id: String,
    pub name: Option<String>,
    pub status: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnEvent {
    pub turn_id: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessErrorEvent {
    pub error: HarnessErrorInfo,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeExtensionEvent {
    pub namespace: String,
    pub event_type: String,
    pub payload: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn native_extension_round_trips_without_loss() {
        let event = HarnessEvent::NativeExtension(NativeExtensionEvent {
            namespace: "xai".into(),
            event_type: "doom_loop_check".into(),
            payload: json!({"nested": [1, true]}),
        });
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(
            serde_json::from_str::<HarnessEvent>(&encoded).unwrap(),
            event
        );
    }
}
