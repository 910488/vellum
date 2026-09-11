//! Command routing helpers shared by gateway and actors.

use serde_json::{json, Value};
use vellum_remote_protocol::{
    CommandStatus, RemoteError, RemoteErrorCode, ThreadCommand, ThreadCommandResult,
};

pub fn command_kind(command: &ThreadCommand) -> &'static str {
    match command {
        ThreadCommand::ThreadStart { .. } => "thread.start",
        ThreadCommand::ThreadResume { .. } => "thread.resume",
        ThreadCommand::TurnStart { .. } => "turn.start",
        ThreadCommand::TurnSteer { .. } => "turn.steer",
        ThreadCommand::TurnInterrupt { .. } => "turn.interrupt",
        ThreadCommand::ApprovalRespond { .. } => "approval.respond",
        ThreadCommand::ThreadArchive => "thread.archive",
        ThreadCommand::WriterAcquire => "writer.acquire",
        ThreadCommand::WriterRelease { .. } => "writer.release",
    }
}

pub fn requires_writer(command: &ThreadCommand) -> bool {
    matches!(
        command,
        ThreadCommand::TurnStart { .. }
            | ThreadCommand::TurnSteer { .. }
            | ThreadCommand::TurnInterrupt { .. }
            | ThreadCommand::ThreadArchive
            | ThreadCommand::ThreadResume { .. }
    )
}

pub fn accepted(idempotency_key: impl Into<String>, result: Value) -> ThreadCommandResult {
    ThreadCommandResult {
        idempotency_key: idempotency_key.into(),
        status: CommandStatus::Accepted,
        result,
        error: None,
    }
}

pub fn completed(idempotency_key: impl Into<String>, result: Value) -> ThreadCommandResult {
    ThreadCommandResult {
        idempotency_key: idempotency_key.into(),
        status: CommandStatus::Completed,
        result,
        error: None,
    }
}

pub fn failed(idempotency_key: impl Into<String>, error: RemoteError) -> ThreadCommandResult {
    ThreadCommandResult {
        idempotency_key: idempotency_key.into(),
        status: CommandStatus::Failed,
        result: Value::Null,
        error: Some(error),
    }
}

pub fn processing(idempotency_key: impl Into<String>) -> ThreadCommandResult {
    ThreadCommandResult {
        idempotency_key: idempotency_key.into(),
        status: CommandStatus::Processing,
        result: json!({}),
        error: None,
    }
}

pub fn indeterminate(idempotency_key: impl Into<String>) -> ThreadCommandResult {
    ThreadCommandResult {
        idempotency_key: idempotency_key.into(),
        status: CommandStatus::Indeterminate,
        result: json!({"needsReconcile": true}),
        error: Some(RemoteError::new(
            RemoteErrorCode::UpstreamUnavailable,
            "command outcome is indeterminate; reconcile before retrying",
            true,
        )),
    }
}

pub fn lease_required(idempotency_key: impl Into<String>) -> ThreadCommandResult {
    failed(
        idempotency_key,
        RemoteError::new(
            RemoteErrorCode::WriterLeaseRequired,
            "This device does not hold the writer lease.",
            true,
        ),
    )
}
