//! Isolated Jetson live-smoke helpers.
//!
//! Design rules:
//! - share only the Codex *native binary*
//! - never touch production `~/.codex` runtime state or production sockets
//! - every run gets a unique RUN_ID with isolated CODEX_HOME / broker DB / workspace

mod broker_client;
mod config;
mod fixture;
mod ssh;
mod trace;

pub use broker_client::{LiveBrokerClient, RecoveredThreadState};
pub use config::{LiveSmokeConfig, LiveSmokeError};
pub use fixture::{
    isolation_guard_summary, resolve_local_broker_binary, JetsonLiveFixture, SmokeUnit,
    SmokeUnitKind,
};
pub use ssh::SshTunnelGuard;
pub use trace::LiveSmokeTrace;
