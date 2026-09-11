//! Client command payloads.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::approval::ApprovalDecision;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UserInput {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default, flatten)]
    pub extra: Value,
}

impl UserInput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            kind: "text".into(),
            text: Some(text.into()),
            extra: Value::Object(Default::default()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct TurnSettings {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub approval_policy: Option<String>,
    #[serde(default)]
    pub extra: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ThreadCommand {
    #[serde(rename = "thread.start")]
    ThreadStart {
        cwd: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        approval_policy: Option<String>,
    },
    #[serde(rename = "thread.resume")]
    ThreadResume { thread_id: String },
    #[serde(rename = "turn.start")]
    TurnStart {
        input: Vec<UserInput>,
        #[serde(default)]
        settings: TurnSettings,
    },
    #[serde(rename = "turn.steer")]
    TurnSteer {
        expected_turn_id: String,
        client_user_message_id: String,
        input: Vec<UserInput>,
    },
    #[serde(rename = "turn.interrupt")]
    TurnInterrupt { turn_id: String },
    #[serde(rename = "approval.respond")]
    ApprovalRespond {
        approval_token: String,
        decision: ApprovalDecision,
    },
    #[serde(rename = "thread.archive")]
    ThreadArchive,
    #[serde(rename = "writer.acquire")]
    WriterAcquire,
    #[serde(rename = "writer.release")]
    WriterRelease { lease_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCommandRequest {
    pub idempotency_key: String,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub expected_writer_lease_id: Option<String>,
    pub command: ThreadCommand,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CommandStatus {
    Accepted,
    Processing,
    Completed,
    Failed,
    /// Side-effectful command may already have been accepted upstream, but the
    /// broker lost the outcome. Clients must reconcile and must never auto-resend.
    Indeterminate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCommandResult {
    pub idempotency_key: String,
    pub status: CommandStatus,
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub error: Option<crate::error::RemoteError>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_kind_tags_use_dotted_names() {
        let command = ThreadCommand::TurnStart {
            input: vec![UserInput::text("run tests")],
            settings: TurnSettings::default(),
        };
        let json = serde_json::to_value(&command).unwrap();
        assert_eq!(json["kind"], "turn.start");
    }
}
