//! App-wide panic visibility.
//!
//! With `panic = "abort"` gone, a panic inside a tokio worker task used to be
//! the loudest failure mode in the product: Windows Error Reporting recorded
//! `0xC0000409` + a fixed offset, which is what made the web-search crashes
//! locatable at all. Unwinding changes that calculus: a panic in proxy
//! streaming / adapter / compaction / sse code now just kills one tokio task
//! with no WER entry, and Rust's default hook writes to stderr, which has no
//! destination in a Windows GUI-subsystem binary. The hook installed here
//! routes every panic through the app logger (persisted by tauri-plugin-log)
//! first and then re-runs the default hook for debuggers/CI.

use std::any::Any;

/// Install the app-wide panic hook. Call once at process start.
pub fn install() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!(
            "[Vellum] panic at {}: {}",
            info.location()
                .map(|location| location.to_string())
                .unwrap_or_else(|| "unknown".into()),
            payload_text(info.payload())
        );
        default_hook(info);
    }));
}

/// Best-effort human-readable text for a panic payload: `&str` and `String`
/// payloads are kept verbatim, anything else gets a generic label. Shared by
/// the panic hook and the web-search engine's `catch_unwind` error envelope.
pub fn payload_text(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_text_extracts_str_and_string_payloads() {
        let caught = std::panic::catch_unwind(|| panic!("literal boom")).unwrap_err();
        assert_eq!(payload_text(&*caught), "literal boom");

        let caught = std::panic::catch_unwind(|| panic!("boom {}", 42)).unwrap_err();
        assert_eq!(payload_text(&*caught), "boom 42");
    }
}
