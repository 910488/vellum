//! Desktop [`DiagnosticsSink`] backed by the usage sqlite database.
//!
//! The shared runtime emits structured [`DiagnosticEvent`] values through a
//! trait seam; the headless daemon may project them to JSONL, while Desktop
//! writes them into `diagnostic_events` for the two SQL acceptance queries.

use std::sync::{Arc, Mutex};

use vellum_proxy_runtime::{
    strip_t3_fields, DetailLevel, DiagnosticEvent, DiagnosticsSink, T3CaptureSession,
};

/// Fire-and-forget sink over the desktop usage database. Record failures are
/// downgraded to a warning so a persistence problem can never affect a request.
///
/// T3 is process-local: the capture session lives only in this sink, so a
/// restart returns to T1. While T3 is enabled, [`T3CaptureSession`] applies
/// the shipped 15-minute / 200 MB bounds.
pub struct DesktopDiagnosticsSink {
    usage: Arc<crate::usage::UsageStore>,
    t3: Mutex<T3CaptureSession>,
}

impl DesktopDiagnosticsSink {
    pub fn new(usage: Arc<crate::usage::UsageStore>) -> Self {
        Self {
            usage,
            t3: Mutex::new(T3CaptureSession::default()),
        }
    }

    /// Opt in to T3 for this process only. Forgotten on drop / restart.
    pub fn enable_t3(&self) {
        self.t3
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .enable_t3();
    }
}

impl DiagnosticsSink for DesktopDiagnosticsSink {
    fn record(&self, mut event: DiagnosticEvent) {
        let encoded = serde_json::to_string(&event).unwrap_or_default();
        let level = {
            let mut t3 = self
                .t3
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            t3.record_bytes(encoded.len() as u64);
            t3.detail_level()
        };
        if level != DetailLevel::T3FullContext {
            strip_t3_fields(&mut event);
        }
        if let Err(error) = self.usage.record_diagnostic_event(&event) {
            log::warn!("[Diagnostics] failed to record diagnostic event: {error}");
        }
    }

    fn detail_level(&self) -> DetailLevel {
        self.t3
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .detail_level()
    }
}
