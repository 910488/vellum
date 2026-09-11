//! Transport trait for the official Codex app-server.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::jsonrpc::JsonRpcId;

#[derive(Debug, Error)]
pub enum AppServerError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("upstream overloaded")]
    Overloaded,
    #[error("incompatible upstream: {0}")]
    Incompatible(String),
    #[error("request timed out")]
    Timeout,
    #[error("upstream not ready")]
    NotReady,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum UpstreamConnectionState {
    Stopped,
    Connecting,
    Initializing,
    Ready,
    Recovering,
    Incompatible,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerIdentity {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub platform_family: Option<String>,
    #[serde(default)]
    pub platform_os: Option<String>,
    #[serde(default)]
    pub codex_home: Option<String>,
}

/// Upstream frames always carry the connection-captured epoch.
/// Never re-read transport.epoch() at dispatch time.
#[derive(Debug, Clone)]
pub enum UpstreamMessage {
    Notification {
        epoch: u64,
        method: String,
        params: Value,
    },
    ServerRequest {
        epoch: u64,
        id: JsonRpcId,
        method: String,
        params: Value,
    },
    Disconnected {
        epoch: u64,
        reason: String,
    },
    Ready {
        epoch: u64,
        identity: ServerIdentity,
    },
}

#[async_trait]
pub trait AppServerTransport: Send + Sync {
    async fn initialize(&self) -> Result<ServerIdentity, AppServerError>;
    async fn call(&self, method: &str, params: Value) -> Result<Value, AppServerError>;
    async fn notify(&self, method: &str, params: Value) -> Result<(), AppServerError>;
    async fn respond(&self, id: JsonRpcId, result: Value) -> Result<(), AppServerError>;
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<UpstreamMessage>;
    fn epoch(&self) -> u64;
    fn connection_state(&self) -> UpstreamConnectionState;
    fn is_ready(&self) -> bool {
        self.connection_state() == UpstreamConnectionState::Ready
    }
    fn reader_alive(&self) -> bool {
        true
    }
    fn writer_alive(&self) -> bool {
        true
    }
}
