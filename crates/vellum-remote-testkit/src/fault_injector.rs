//! Fault injection knobs for reconnect and ordering tests.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct FaultInjector {
    pub drop_next_event: AtomicBool,
    pub duplicate_next_event: AtomicBool,
    pub force_disconnect: AtomicBool,
    pub injected_faults: AtomicU64,
}

impl FaultInjector {
    pub fn arm_drop(&self) {
        self.drop_next_event.store(true, Ordering::SeqCst);
    }

    pub fn arm_duplicate(&self) {
        self.duplicate_next_event.store(true, Ordering::SeqCst);
    }

    pub fn arm_disconnect(&self) {
        self.force_disconnect.store(true, Ordering::SeqCst);
    }

    pub fn should_drop(&self) -> bool {
        if self.drop_next_event.swap(false, Ordering::SeqCst) {
            self.injected_faults.fetch_add(1, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    pub fn should_duplicate(&self) -> bool {
        if self.duplicate_next_event.swap(false, Ordering::SeqCst) {
            self.injected_faults.fetch_add(1, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    pub fn should_disconnect(&self) -> bool {
        if self.force_disconnect.swap(false, Ordering::SeqCst) {
            self.injected_faults.fetch_add(1, Ordering::SeqCst);
            true
        } else {
            false
        }
    }
}
