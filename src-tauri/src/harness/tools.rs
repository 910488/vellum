//! Exact translated tool contracts.
//!
//! M3 (refactor(proxy-runtime): move harness contract to runtime): the pure
//! implementation now lives in `vellum-proxy-runtime::harness::tools`, shared
//! with the headless daemon. This module re-exports that single type set so
//! every existing `crate::harness::tools::*` call site in this crate resolves
//! exactly as before.

pub use vellum_proxy_runtime::harness::tools::*;
