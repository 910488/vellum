//! Codex App Server V2 facade over the Vellum Harness Manager.
//!
//! Layering, deliberately three stages rather than one translation:
//!
//! ```text
//! native protocol -> vellum-harness-runtime::mapper -> HarnessEvent
//!                 -> vellum-codex-facade::ui_mapper -> Codex notification
//! ```
//!
//! Request and response payloads stay JSON until the pinned schema under
//! `third_party/codex-app-server-schema/<version>` is generated into typed
//! bindings. Vellum does not link `codex-core`; the app-server protocol is the
//! only supported boundary.

pub mod broker;
pub mod facade;
pub mod journal;
pub mod methods;
pub mod permission;
pub mod transport;
pub mod ui_mapper;

pub use broker::{compaction_authority, HarnessBroker, ManagedHarnessBroker};
pub use facade::{CodexAppServerFacade, CodexFacadeError};
pub use journal::{EventJournal, JournalEntry, NOT_RUNTIME_REPLAY_SOURCE};
pub use permission::{PermissionError, PermissionRegistry};
pub use transport::serve;
pub use ui_mapper::{
    CodexMapError, CodexServerMessage, CodexServerNotification, CodexUiEventMapper,
    DefaultCodexUiEventMapper, UiMapContext,
};
