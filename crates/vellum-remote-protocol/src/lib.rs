//! Shared Vellum Remote Protocol types.
//!
//! Pure serde types only — no Tokio, SQLite, or Tauri dependencies.

pub mod approval;
pub mod command;
pub mod envelope;
pub mod error;
pub mod event;
pub mod snapshot;
pub mod version;

pub use approval::*;
pub use command::*;
pub use envelope::*;
pub use error::*;
pub use event::*;
pub use snapshot::*;
pub use version::*;
