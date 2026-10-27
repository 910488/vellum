//! Claude Desktop **local Code** inference. This is a separate, native
//! Messages data plane; none of the Codex replay/compaction adapters apply.
pub mod certificate;
mod proxy;
pub mod settings;
mod usage;
pub use usage::{TokenUsage, UsageObservation, UsageWindowObservation};

pub use proxy::{
    AccountToken, ClaudeAccounts, ClaudeCodeProxy, CodeRequestRecord, CodeRouteStatus,
    CodeUsageRecord, ProxyScope,
};

/// Same form as the Codex `execution_account_hash` in `request_usage`, so
/// one query can follow either provider's account.
pub fn account_hash(account_id: &str) -> String {
    format!(
        "sha256:{}",
        crate::request::OfficialSocketAffinity::hash_account(account_id)
    )
}
