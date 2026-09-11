//! Harness-aware prompt generation.
//!
//! M3 (refactor(proxy-runtime): move harness contract to runtime): the pure
//! implementation now lives in `vellum-proxy-runtime::harness::prompt`,
//! shared with the headless daemon. This module re-exports that single type
//! set so every existing `crate::harness::prompt::*` call site in this crate
//! resolves exactly as before.

pub use vellum_proxy_runtime::harness::prompt::*;
