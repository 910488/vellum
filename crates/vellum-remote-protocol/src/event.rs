//! Downstream event and control-plane payloads.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::approval::PendingApproval;
use crate::command::{ThreadCommandRequest, ThreadCommandResult};
use crate::error::RemoteError;
use crate::snapshot::{ThreadSnapshot, WriterLeaseView};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    #[serde(default)]
    pub approvals: bool,
    #[serde(default)]
    pub artifacts: bool,
    #[serde(default)]
    pub writer_lease: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ResumeHint {
    pub thread_id: String,
    pub last_ack_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClientHello {
    pub device_id: String,
    pub client_version: String,
    #[serde(default)]
    pub device_token: Option<String>,
    #[serde(default)]
    pub resume: Option<ResumeHint>,
    #[serde(default)]
    pub capabilities: ClientCapabilities,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AuthState {
    Authenticated,
    PairingRequired,
    Revoked,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum UpstreamState {
    Ready,
    Connecting,
    Recovering,
    Incompatible,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamInfo {
    pub state: UpstreamState,
    pub epoch: u64,
    #[serde(default)]
    pub codex_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServerWelcome {
    pub broker_id: String,
    pub broker_version: String,
    pub protocol_version: u32,
    /// Semantic scheme for lastAckSeq / snapshotSeq / ThreadEvent.seq.
    #[serde(default = "default_cursor_scheme")]
    pub cursor_scheme: String,
    pub server_time: DateTime<Utc>,
    pub auth_state: AuthState,
    pub upstream: UpstreamInfo,
}

fn default_cursor_scheme() -> String {
    crate::version::CURSOR_SCHEME.to_string()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SubscriptionMode {
    Writer,
    Observer,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSubscribe {
    pub thread_id: String,
    pub last_ack_seq: u64,
    pub mode: SubscriptionMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadEvent {
    pub seq: u64,
    pub thread_id: String,
    pub upstream_epoch: u64,
    pub method: String,
    pub occurred_at: DateTime<Utc>,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub item_id: Option<String>,
    #[serde(default)]
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ThreadAck {
    pub thread_id: String,
    pub through_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WriterHeartbeat {
    pub thread_id: String,
    pub lease_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WriterChanged {
    pub thread_id: String,
    pub lease: Option<WriterLeaseView>,
}

/// Typed view of all known downstream messages after envelope decoding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload", rename_all = "camelCase")]
pub enum DownstreamMessage {
    #[serde(rename = "client.hello")]
    ClientHello(ClientHello),
    #[serde(rename = "server.welcome")]
    ServerWelcome(ServerWelcome),
    #[serde(rename = "thread.subscribe")]
    ThreadSubscribe(ThreadSubscribe),
    #[serde(rename = "thread.snapshot")]
    ThreadSnapshot(ThreadSnapshot),
    #[serde(rename = "thread.event")]
    ThreadEvent(ThreadEvent),
    #[serde(rename = "thread.ack")]
    ThreadAck(ThreadAck),
    #[serde(rename = "thread.command")]
    ThreadCommand(ThreadCommandRequest),
    #[serde(rename = "thread.commandResult")]
    ThreadCommandResult(ThreadCommandResult),
    #[serde(rename = "writer.heartbeat")]
    WriterHeartbeat(WriterHeartbeat),
    #[serde(rename = "writer.changed")]
    WriterChanged(WriterChanged),
    #[serde(rename = "server.error")]
    ServerError(RemoteError),
    #[serde(rename = "approval.pending")]
    ApprovalPending(PendingApproval),
}
