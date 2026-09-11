//! SSE（Server-Sent Events）解析工具（M5 extraction）。
//!
//! 實作與測試已移入 shared `vellum-proxy-runtime::sse`；Desktop 保留這個
//! 薄 re-export 模組，讓既有 call site（`crate::sse::...`）繼續走同一份
//! 共用程式碼，不複製第二份實作。

pub use vellum_proxy_runtime::sse::{append_utf8_safe, strip_sse_field, take_sse_block};
