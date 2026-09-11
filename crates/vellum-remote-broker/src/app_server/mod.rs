//! Upstream Codex app-server adapter.

pub mod adapter;
pub mod compatibility;
pub mod jsonrpc;
pub mod runtime;
pub mod supervisor;
pub mod transport;
pub mod unix_ws;

pub use adapter::AppServerAdapter;
pub use supervisor::UpstreamSupervisor;
pub use transport::{
    AppServerError, AppServerTransport, ServerIdentity, UpstreamConnectionState, UpstreamMessage,
};
pub use unix_ws::UnixWsAppServerTransport;
