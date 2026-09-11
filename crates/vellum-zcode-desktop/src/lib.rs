//! Control channel, durable thread binding, and lifecycle types for the
//! experimental ZCode Desktop host tap.
//!
//! Desktop keeps authentication and CAPTCHA. This crate talks only to the tap
//! control pipe; it never stores runtime headers or forges `headersApplied`.

mod binding;
mod channel;
mod discovery;
mod error;
mod host;
pub mod protocol;

pub use binding::{ZcodeBindingStore, ZcodeThreadBinding};
pub use channel::{ControlClient, ControlEvent};
pub use discovery::{
    discover, discover_filtered, listen_path_for_pid, newest, require_newest, require_qualified,
    DiscoverFilter, TapListenRecord,
};
pub use error::ZcodeDesktopError;
pub use host::{default_log_dir, ZcodeDesktopHost};
pub use protocol::{
    ArtifactPin, AssistantCompletedEvent, BindParams, BindResult, HelloResult, LifecycleEvent,
    SessionAnnouncedEvent, TapLifecycle, ToolCompletedEvent, ToolLifecycleEvent, TurnCancelParams,
    TurnCompletedEvent, TurnDeltaEvent, TurnOutcome, TurnStartParams, TurnStartResult,
    TurnStartedEvent, TurnUsageEvent, ZcodeArtifact, CONTROL_PROTOCOL_VERSION,
};

pub fn fingerprint_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;
