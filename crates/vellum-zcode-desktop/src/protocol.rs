use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CONTROL_PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ZcodeArtifact {
    pub cjs_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactPin {
    pub cjs_sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TapLifecycle {
    Ready,
    CaptchaWaiting,
    AppServerRestart,
    Exited,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HelloParams {
    pub protocol_version: u32,
    pub client: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_cjs_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HelloResult {
    pub protocol_version: u32,
    pub artifact: ZcodeArtifact,
    pub pid: u32,
    pub inner_pid: Option<u32>,
    pub sessions: Vec<String>,
    pub lifecycle: TapLifecycle,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BindParams {
    pub vellum_thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BindResult {
    pub vellum_thread_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    pub vellum_thread_id: String,
    pub vellum_turn_id: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartResult {
    pub vellum_turn_id: String,
    pub session_id: String,
    pub native_request_id: String,
    pub input_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartedEvent {
    pub vellum_turn_id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_turn_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TurnCancelParams {
    pub vellum_thread_id: String,
    pub vellum_turn_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StatusResult {
    pub lifecycle: TapLifecycle,
    pub sessions: Vec<String>,
    pub inflight_turns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TurnDeltaEvent {
    pub vellum_turn_id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AssistantCompletedEvent {
    pub vellum_turn_id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TurnUsageEvent {
    pub vellum_turn_id: String,
    pub session_id: String,
    pub usage: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolLifecycleEvent {
    pub vellum_turn_id: String,
    pub session_id: String,
    pub tool_call_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCompletedEvent {
    pub vellum_turn_id: String,
    pub session_id: String,
    pub tool_call_id: String,
    pub name: String,
    pub result: Value,
    pub is_error: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TurnOutcome {
    Completed,
    Failed,
    Cancelled,
    Timeout,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedEvent {
    pub vellum_turn_id: String,
    pub session_id: String,
    pub outcome: TurnOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionAnnouncedEvent {
    pub session_id: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleEvent {
    pub state: TapLifecycle,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ControlErrorBody {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlFrame {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ControlErrorBody>,
}

impl ControlFrame {
    pub fn request(id: impl Into<Value>, method: &str, params: Value) -> Self {
        Self {
            id: Some(id.into()),
            method: Some(method.to_owned()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    pub fn result(id: Value, result: Value) -> Self {
        Self {
            id: Some(id),
            method: None,
            params: None,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<Value>, code: &str, message: impl Into<String>) -> Self {
        Self {
            id,
            method: None,
            params: None,
            result: None,
            error: Some(ControlErrorBody {
                code: code.to_owned(),
                message: message.into(),
                data: None,
            }),
        }
    }

    pub fn event(method: &str, params: Value) -> Self {
        Self {
            id: None,
            method: Some(method.to_owned()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    pub fn is_event(&self) -> bool {
        self.id.is_none() && self.method.is_some()
    }
}

pub const METHOD_HELLO: &str = "hello";
pub const METHOD_BIND: &str = "bind";
pub const METHOD_TURN_START: &str = "turn/start";
pub const METHOD_TURN_CANCEL: &str = "turn/cancel";
pub const METHOD_STATUS: &str = "status";
pub const EVENT_TURN_DELTA: &str = "turn/delta";
pub const EVENT_TURN_STARTED: &str = "turn/started";
pub const EVENT_ASSISTANT_COMPLETED: &str = "assistant/completed";
pub const EVENT_TURN_USAGE: &str = "turn/usage";
pub const EVENT_TOOL_STARTED: &str = "tool/started";
pub const EVENT_TOOL_UPDATED: &str = "tool/updated";
pub const EVENT_TOOL_COMPLETED: &str = "tool/completed";
pub const EVENT_TURN_COMPLETED: &str = "turn/completed";
pub const EVENT_SESSION_ANNOUNCED: &str = "session/announced";
pub const EVENT_LIFECYCLE: &str = "lifecycle";
