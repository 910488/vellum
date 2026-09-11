//! `ConnectionState` — kept only for `model.rs`'s forward-looking re-export
//! (`pub use crate::remote::client::ConnectionState as RemoteConnectionStatus;`).
//!
//! The WebSocket client that used to produce this state (`RemoteClient`,
//! `RemoteEvent`, and the broker-protocol thread/turn/writer command
//! plumbing) had zero callers from the shipped Desktop frontend and was
//! removed as dead code in the Stage G security-hardening pass — see
//! `docs/adr/ADR-001-codex-host-native.md` and
//! `src-tauri/src/remote/connection_manager.rs`.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Authenticating,
    Synchronizing,
    Live,
    Degraded,
    Closing,
}
