//! Re-exports the Desktop-wide background-process policy for existing
//! `crate::remote::process::*` call sites. The policy itself now lives in
//! `crate::process` since it covers every non-interactive helper Vellum
//! spawns, not just Remote Manager's `ssh`/`curl` calls — see that module's
//! doc comment for the full rationale.

#[allow(unused_imports)]
// re-exported for API completeness even though remote/* only uses the std variant today
pub use crate::process::{background_command, background_tokio_command};
