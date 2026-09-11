//! Protocol versioning helpers.

/// Current Vellum Remote Protocol major version.
/// v2: ThreadEvent.seq / last_ack_seq / snapshotSeq are per-thread cursors.
pub const PROTOCOL_VERSION: u32 = 2;

/// Cursor scheme advertised by broker welcome / used by desktop cache invalidation.
pub const CURSOR_SCHEME: &str = "thread-seq-v1";

/// Returns true when a peer protocol version is acceptable for this build.
pub fn is_supported(protocol_version: u32) -> bool {
    protocol_version == PROTOCOL_VERSION || protocol_version == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_and_legacy_v1_are_supported() {
        assert!(is_supported(PROTOCOL_VERSION));
        assert!(is_supported(1));
        assert!(!is_supported(0));
        assert!(!is_supported(PROTOCOL_VERSION + 1));
    }
}
