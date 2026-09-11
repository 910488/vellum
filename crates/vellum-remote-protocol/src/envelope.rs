//! Envelope wrapper for every downstream message.

use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;

use crate::version::PROTOCOL_VERSION;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Envelope<T> {
    pub protocol_version: u32,
    pub message_id: String,
    #[serde(rename = "type")]
    pub r#type: String,
    pub sent_at: DateTime<Utc>,
    pub payload: T,
}

impl Envelope<Value> {
    pub fn into_typed<T: DeserializeOwned>(self) -> Result<Envelope<T>, serde_json::Error> {
        Ok(Envelope {
            protocol_version: self.protocol_version,
            message_id: self.message_id,
            r#type: self.r#type,
            sent_at: self.sent_at,
            payload: serde_json::from_value(self.payload)?,
        })
    }
}

impl<T: Serialize> Envelope<T> {
    pub fn new(message_type: impl Into<String>, message_id: impl Into<String>, payload: T) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            message_id: message_id.into(),
            r#type: message_type.into(),
            sent_at: Utc::now(),
            payload,
        }
    }

    pub fn to_value_envelope(&self) -> Result<Envelope<Value>, serde_json::Error> {
        Ok(Envelope {
            protocol_version: self.protocol_version,
            message_id: self.message_id.clone(),
            r#type: self.r#type.clone(),
            sent_at: self.sent_at,
            payload: serde_json::to_value(&self.payload)?,
        })
    }
}

/// Canonical downstream message types.
pub mod message_type {
    pub const CLIENT_HELLO: &str = "client.hello";
    pub const SERVER_WELCOME: &str = "server.welcome";
    pub const THREAD_SUBSCRIBE: &str = "thread.subscribe";
    pub const THREAD_SNAPSHOT: &str = "thread.snapshot";
    pub const THREAD_EVENT: &str = "thread.event";
    pub const THREAD_ACK: &str = "thread.ack";
    pub const THREAD_COMMAND: &str = "thread.command";
    pub const THREAD_COMMAND_RESULT: &str = "thread.commandResult";
    pub const WRITER_HEARTBEAT: &str = "writer.heartbeat";
    pub const WRITER_CHANGED: &str = "writer.changed";
    pub const SERVER_ERROR: &str = "server.error";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_roundtrip_preserves_unknown_payload_fields() {
        let raw = json!({
            "protocolVersion": 1,
            "messageId": "hello-1",
            "type": "client.hello",
            "sentAt": "2026-08-07T00:00:00Z",
            "payload": {
                "deviceId": "windows-desktop-01",
                "clientVersion": "0.2.0",
                "futureField": true
            }
        });
        let envelope: Envelope<Value> = serde_json::from_value(raw).unwrap();
        assert_eq!(envelope.protocol_version, 1);
        assert_eq!(envelope.payload["futureField"], true);
        let encoded = serde_json::to_value(&envelope).unwrap();
        assert_eq!(encoded["payload"]["futureField"], true);
    }
}
