//! Protocol error codes and payloads.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteErrorCode {
    UnsupportedProtocol,
    Unauthenticated,
    DeviceRevoked,
    WriterLeaseRequired,
    WriterLeaseConflict,
    ThreadNotFound,
    ThreadNotLoaded,
    UpstreamUnavailable,
    UpstreamIncompatible,
    UpstreamOverloaded,
    StaleUpstreamEpoch,
    ApprovalOrphaned,
    CommandConflict,
    ReplayGap,
    SnapshotRequired,
    InvalidWorkspace,
    InternalError,
}

impl RemoteErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedProtocol => "unsupported_protocol",
            Self::Unauthenticated => "unauthenticated",
            Self::DeviceRevoked => "device_revoked",
            Self::WriterLeaseRequired => "writer_lease_required",
            Self::WriterLeaseConflict => "writer_lease_conflict",
            Self::ThreadNotFound => "thread_not_found",
            Self::ThreadNotLoaded => "thread_not_loaded",
            Self::UpstreamUnavailable => "upstream_unavailable",
            Self::UpstreamIncompatible => "upstream_incompatible",
            Self::UpstreamOverloaded => "upstream_overloaded",
            Self::StaleUpstreamEpoch => "stale_upstream_epoch",
            Self::ApprovalOrphaned => "approval_orphaned",
            Self::CommandConflict => "command_conflict",
            Self::ReplayGap => "replay_gap",
            Self::SnapshotRequired => "snapshot_required",
            Self::InvalidWorkspace => "invalid_workspace",
            Self::InternalError => "internal_error",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteError {
    pub code: RemoteErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(default)]
    pub details: Value,
}

impl RemoteError {
    pub fn new(code: RemoteErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
            details: Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_roundtrip() {
        let error = RemoteError::new(
            RemoteErrorCode::WriterLeaseRequired,
            "This device does not hold the writer lease.",
            true,
        );
        let json = serde_json::to_string(&error).unwrap();
        assert!(json.contains("writer_lease_required"));
        let decoded: RemoteError = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, error);
    }
}
