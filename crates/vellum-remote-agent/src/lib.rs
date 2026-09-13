//! Vellum remote host lifecycle agent.
//!
//! Owns install/update, proxy container lifecycle, host probes and repair.
//! Does not own broker sessions, approvals, or model request adaptation.

pub mod configuration;
pub mod docker;
pub mod grok;
pub mod host_probe;
pub mod mobile_account;
pub mod mutation_lock;
pub mod native_account;
pub mod native_codex;
pub mod native_proxy;
pub mod native_session;
pub mod official_account;
pub mod operations;
pub mod permissions;
pub mod platform;
pub mod process_identity;
pub mod profile;
pub mod protocol;
pub mod proxy;
pub mod release_manifest;
pub mod space;
pub mod ssh_launcher;
pub mod state;
pub mod support;
pub mod update;

pub use native_codex::{
    daemon_reconcile_commands, install_pinned_codex, reconcile_durable_service,
    update_pinned_codex, verify_installation, CodexCliLauncherStatus, CodexInstallationResult,
    CodexInstallationStatus, ServiceReconcileResult,
};
pub use native_session::{query_session_status, NativeSessionStatus, NativeThreadStatus};
pub use protocol::{
    AgentError, AgentRequest, AgentResponse, CodexInventory, DockerInventory, HostBlocker,
    HostCapabilities, HostInventoryV2, HostStatus, OperationResult, ProxyLogsView, ProxyStatusView,
    SystemInventory,
};
pub use proxy::{ProxyManager, ProxyStartRequest};
pub use state::{AgentPaths, AgentStateStore, InstallRecord};

pub const AGENT_PROTOCOL_VERSION: u32 = 4;
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
