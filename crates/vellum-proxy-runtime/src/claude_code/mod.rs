//! Claude Desktop **local Code** inference. This is a separate, native
//! Messages data plane; none of the Codex replay/compaction adapters apply.
pub mod certificate;
mod proxy;
pub mod settings;
mod usage;
pub use usage::{UsageObservation, UsageWindowObservation};

pub use proxy::{
    AccountToken, ClaudeAccounts, ClaudeCodeProxy, CodeRequestRecord, CodeRouteStatus, ProxyScope,
};
