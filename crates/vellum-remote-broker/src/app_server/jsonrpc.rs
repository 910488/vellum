//! Minimal JSON-RPC helpers for Codex app-server wire format.
//!
//! Official app-server uses JSON-RPC 2.0 semantics but omits the `jsonrpc`
//! field on the wire. Keep wire types free of that field.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(i64),
    String(String),
}

impl From<u64> for JsonRpcId {
    fn from(value: u64) -> Self {
        Self::Number(value as i64)
    }
}

impl From<i64> for JsonRpcId {
    fn from(value: i64) -> Self {
        Self::Number(value)
    }
}

impl From<String> for JsonRpcId {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl JsonRpcId {
    pub fn to_value(&self) -> Value {
        match self {
            Self::Number(n) => Value::from(*n),
            Self::String(s) => Value::String(s.clone()),
        }
    }

    pub fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Number(n) => n.as_i64().map(Self::Number),
            Value::String(s) => Some(Self::String(s.clone())),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRequest {
    pub id: JsonRpcId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientNotification {
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerRequest {
    pub id: JsonRpcId,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerResponse {
    pub id: JsonRpcId,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientResponse {
    pub id: JsonRpcId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IncomingMessage {
    Response(ServerResponse),
    Request(ServerRequest),
    Notification(ClientNotification),
}

/// Classify an app-server frame by required JSON-RPC fields.
///
/// Untagged serde is unsafe here: a server request has `id` + `method` and would
/// also deserialize as a response with optional `result`/`error` missing.
pub fn classify_incoming(value: Value) -> Result<IncomingMessage, String> {
    let has_id = value.get("id").is_some();
    let has_method = value.get("method").is_some();
    let has_result = value.get("result").is_some();
    let has_error = value.get("error").is_some();

    match (has_id, has_method, has_result || has_error) {
        (true, true, _) => {
            let request: ServerRequest = serde_json::from_value(value)
                .map_err(|error| format!("invalid server request: {error}"))?;
            Ok(IncomingMessage::Request(request))
        }
        (false, true, _) => {
            let notification: ClientNotification = serde_json::from_value(value)
                .map_err(|error| format!("invalid notification: {error}"))?;
            Ok(IncomingMessage::Notification(notification))
        }
        (true, false, true) => {
            let response: ServerResponse = serde_json::from_value(value)
                .map_err(|error| format!("invalid response: {error}"))?;
            Ok(IncomingMessage::Response(response))
        }
        _ => Err("unclassifiable app-server message".into()),
    }
}

pub fn classify_incoming_str(text: &str) -> Result<IncomingMessage, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("invalid upstream json: {error}"))?;
    classify_incoming(value)
}

impl ClientRequest {
    pub fn new(id: impl Into<JsonRpcId>, method: impl Into<String>, params: Value) -> Self {
        Self {
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

impl ClientNotification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            method: method.into(),
            params,
        }
    }
}

impl ClientResponse {
    pub fn result(id: JsonRpcId, result: Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_numeric_server_request_frame() {
        let message = classify_incoming(json!({
            "id": 61,
            "method": "item/commandExecution/requestApproval",
            "params": { "threadId": "thr_123", "command": "ls" }
        }))
        .unwrap();
        match message {
            IncomingMessage::Request(request) => {
                assert_eq!(request.id, JsonRpcId::Number(61));
                assert_eq!(request.method, "item/commandExecution/requestApproval");
                assert_eq!(
                    request.params.get("threadId").and_then(Value::as_str),
                    Some("thr_123")
                );
            }
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[test]
    fn parses_string_server_request_frame() {
        let message = classify_incoming(json!({
            "id": "req-9",
            "method": "item/fileChange/requestApproval",
            "params": { "threadId": "thr_abc" }
        }))
        .unwrap();
        match message {
            IncomingMessage::Request(request) => {
                assert_eq!(request.id, JsonRpcId::String("req-9".into()));
            }
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[test]
    fn does_not_parse_server_request_as_response() {
        let message = classify_incoming(json!({
            "id": 61,
            "method": "item/commandExecution/requestApproval",
            "params": { "threadId": "thr_123" }
        }))
        .unwrap();
        assert!(matches!(message, IncomingMessage::Request(_)));
        assert!(!matches!(message, IncomingMessage::Response(_)));
    }

    #[test]
    fn parses_response_and_notification() {
        let response = classify_incoming(json!({
            "id": 3,
            "result": { "ok": true }
        }))
        .unwrap();
        assert!(matches!(response, IncomingMessage::Response(_)));

        let note = classify_incoming(json!({
            "method": "turn/started",
            "params": { "threadId": "thr_1", "turnId": "turn_1" }
        }))
        .unwrap();
        assert!(matches!(note, IncomingMessage::Notification(_)));
    }
}
