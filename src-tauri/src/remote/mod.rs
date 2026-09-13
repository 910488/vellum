//! Desktop-side Remote Broker client wiring.
//!
//! This module intentionally stops at backend/Tauri command surfaces. React UI
//! is out of scope for the first wiring pass.

pub mod agent_client;
pub mod bootstrap;
pub mod boundary_key;
pub mod client;
pub mod commands;
pub mod connection_manager;
pub mod deployment;
pub mod desired_state;
pub mod desktop_codex;
pub mod digest;
pub mod discovery;
pub mod host_manager;
pub mod local_cache;
pub mod observation;
pub mod operation;
pub mod pinned_install;
pub mod platform;
mod process;
pub mod reducer;
pub mod restore;
pub mod session_summary;
pub mod ssh_isolation;
pub mod ssh_trust;

pub use agent_client::{RemoteAgentClient, ResolvedAgentTarget};
pub(crate) use boundary_key::confirm_remote_boundary_key_consumers;
pub use boundary_key::{
    provision_remote_boundary_key, provision_remote_boundary_key_without_native_restart,
};
pub use connection_manager::RemoteClientManager;
pub use deployment::{
    RemoteApplyResult, RemoteConfigDrift, RemoteDeploymentPlan, RemoteDiffEntry,
    RemoteModelSelection, RemoteQualifiedCapability,
};
pub use desired_state::RemoteHostDesiredState;
pub use discovery::RemoteHostCandidate;
pub use host_manager::{RemoteHostAggregateStatus, RemoteHostManager};
pub use local_cache::RemoteLocalCache;
pub use operation::RemoteOperationProgress;
pub use session_summary::{RemoteSessionSummary, RemoteThreadSummary};

/// The Remote-control identity follows the account Codex Desktop is actually
/// using. Vellum's managed OAuth default is only a fallback for installations
/// whose native auth file does not expose an account identity.
pub(crate) fn desktop_control_account_id(state: &crate::state::AppState) -> Option<String> {
    let paths = crate::codex::CodexPaths::discover(&state.data_root());
    crate::codex_oauth::native_codex_account_id(&paths.auth)
        .or_else(|| state.codex_oauth().peek_default_account_id())
}
