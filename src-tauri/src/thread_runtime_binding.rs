//! Durable thread → runtime identity. The binding is an index only: it is not
//! a transcript and cannot reconstruct a Codex session.

pub use crate::enhanced_runtime::{ExecutionPlane, ThreadRuntimeBinding, BINDING_VERSION};
