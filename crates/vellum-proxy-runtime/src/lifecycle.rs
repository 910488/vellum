//! Request lifecycle port (plan §4.4).
//!
//! Desktop's real implementation is `AppState`'s existing
//! `try_begin_request()`/draining/idle accounting. The daemon's real
//! implementation is the atomic counter below. Neither is wired into the
//! request path yet — this only fixes the contract so `proxy.stop` can later
//! drain in-flight requests before tearing down the container without the
//! daemon reimplementing Desktop's accounting from scratch.
//!
//! Admission and the draining flag live in one `AtomicUsize` word, checked
//! and updated by a single compare-exchange, so there is one linearization
//! point for "am I admitted" instead of two independently-timed atomics.
//! Two separate atomics (an admitted-count and a draining bool) invite a
//! window where a thread reads `draining == false`, a concurrent
//! `begin_drain()` sets it and observes `active == 0`, and only then does
//! the first thread's increment land — a request admitted after the drain
//! was believed complete.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::watch;

/// One proxy serve generation. Start, Stop, and Repair share this counter so
/// a stale task cannot write a newer generation's status.
#[derive(Debug, Clone)]
pub struct ProxyGeneration {
    pub id: u64,
    cancel: watch::Sender<bool>,
}

impl ProxyGeneration {
    fn new(id: u64) -> Self {
        let (cancel, _) = watch::channel(false);
        Self { id, cancel }
    }

    pub fn cancel(&self) {
        self.cancel.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.cancel.borrow()
    }

    pub fn cancelled(&self) -> impl std::future::Future<Output = ()> + Send {
        let mut rx = self.cancel.subscribe();
        async move {
            if *rx.borrow() {
                return;
            }
            let _ = rx.changed().await;
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.cancel.subscribe()
    }
}

/// Process-wide current generation. Bumping cancels the previous generation.
#[derive(Debug)]
pub struct GenerationController {
    current: AtomicU64,
    slot: std::sync::Mutex<ProxyGeneration>,
}

impl Default for GenerationController {
    fn default() -> Self {
        Self {
            current: AtomicU64::new(0),
            slot: std::sync::Mutex::new(ProxyGeneration::new(0)),
        }
    }
}

impl GenerationController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn current_id(&self) -> u64 {
        self.current.load(Ordering::SeqCst)
    }

    pub fn current(&self) -> ProxyGeneration {
        self.slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Cancel the live generation, mint the next one, and return it.
    pub fn bump(&self) -> ProxyGeneration {
        let next_id = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        let next = ProxyGeneration::new(next_id);
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slot.cancel();
        *slot = next.clone();
        next
    }

    pub fn cancel_current(&self) {
        self.current().cancel();
    }

    /// True when `id` is still the live generation.
    pub fn is_current(&self, id: u64) -> bool {
        self.current_id() == id
    }
}

const DRAINING: usize = 1 << (usize::BITS - 1);

/// Held for the lifetime of one in-flight request. Dropping it always
/// releases the slot, including on panic/early return.
pub struct RequestGuard {
    release: Option<Box<dyn FnOnce() + Send>>,
}

impl RequestGuard {
    /// Adapt an authority-owned guard into the shared runtime lifecycle.
    /// The captured value is released exactly when this runtime guard drops.
    pub fn from_owned<T: Send + 'static>(guard: T) -> Self {
        Self {
            release: Some(Box::new(move || drop(guard))),
        }
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

pub trait RequestLifecycle: Send + Sync {
    /// Reserves a slot for one request. Fails once draining has begun so a
    /// stop can wait for `active_count() == 0` without racing new admissions.
    fn begin(&self) -> Result<RequestGuard, String>;
    /// In-flight request count. Default 0 for hosts that do not expose it.
    fn active_count(&self) -> usize {
        0
    }
}

#[derive(Default)]
pub struct CountingRequestLifecycle {
    state: Arc<AtomicUsize>,
}

impl CountingRequestLifecycle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn active_count(&self) -> usize {
        self.state.load(Ordering::SeqCst) & !DRAINING
    }

    /// Refuses new `begin()` calls; in-flight requests already holding a
    /// guard are unaffected and still drain normally on drop. Once this
    /// returns, no `begin()` call anywhere can succeed again — the flag is
    /// part of the same word every admission CAS reads, so there is no call
    /// that can be "in flight" across the boundary.
    pub fn begin_drain(&self) {
        self.state.fetch_or(DRAINING, Ordering::SeqCst);
    }
}

impl RequestLifecycle for CountingRequestLifecycle {
    fn active_count(&self) -> usize {
        CountingRequestLifecycle::active_count(self)
    }

    fn begin(&self) -> Result<RequestGuard, String> {
        loop {
            let current = self.state.load(Ordering::SeqCst);
            if current & DRAINING != 0 {
                return Err("proxy is draining, not accepting new requests".into());
            }
            match self.state.compare_exchange(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(_) => continue,
            }
        }
        let state = Arc::clone(&self.state);
        Ok(RequestGuard {
            release: Some(Box::new(move || {
                state.fetch_sub(1, Ordering::SeqCst);
            })),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn bump_cancels_the_previous_generation_and_ignores_stale_ids() {
        let controller = GenerationController::new();
        let first = controller.bump();
        assert_eq!(first.id, 1);
        assert!(!first.is_cancelled());
        let second = controller.bump();
        assert_eq!(second.id, 2);
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
        assert!(controller.is_current(2));
        assert!(!controller.is_current(1));
    }

    #[test]
    fn guard_drop_releases_the_slot() {
        let lifecycle = CountingRequestLifecycle::new();
        let guard = lifecycle.begin().unwrap();
        assert_eq!(lifecycle.active_count(), 1);
        drop(guard);
        assert_eq!(lifecycle.active_count(), 0);
    }

    #[test]
    fn draining_refuses_new_admissions_but_not_existing_guards() {
        let lifecycle = CountingRequestLifecycle::new();
        let guard = lifecycle.begin().unwrap();
        lifecycle.begin_drain();
        assert!(lifecycle.begin().is_err());
        assert_eq!(lifecycle.active_count(), 1);
        drop(guard);
        assert_eq!(lifecycle.active_count(), 0);
    }

    /// The exact scenario the two-atomic version could get wrong: once
    /// `begin_drain()` has returned to its caller, every `begin()` call any
    /// other thread makes afterward — no matter how many — must be refused.
    #[test]
    fn no_admission_after_begin_drain_returns() {
        let lifecycle = Arc::new(CountingRequestLifecycle::new());
        lifecycle.begin_drain();

        let handles: Vec<_> = (0..64)
            .map(|_| {
                let lifecycle = Arc::clone(&lifecycle);
                thread::spawn(move || lifecycle.begin().is_err())
            })
            .collect();
        for handle in handles {
            assert!(
                handle.join().unwrap(),
                "begin() succeeded after begin_drain() had already returned"
            );
        }
    }

    /// Hammers `begin()`/drop() from several threads while another thread
    /// drains mid-flight, repeated many times to shake out ordering-dependent
    /// bugs. Every admitted guard must eventually be released and the count
    /// must land on exactly zero — a stray double-admission or a lost
    /// decrement would leave it non-zero.
    #[test]
    fn concurrent_admission_and_drain_never_leaks_or_double_admits() {
        for _ in 0..200 {
            let lifecycle = Arc::new(CountingRequestLifecycle::new());
            let stop = Arc::new(AtomicBool::new(false));

            let workers: Vec<_> = (0..8)
                .map(|_| {
                    let lifecycle = Arc::clone(&lifecycle);
                    let stop = Arc::clone(&stop);
                    thread::spawn(move || {
                        while !stop.load(Ordering::SeqCst) {
                            if let Ok(guard) = lifecycle.begin() {
                                drop(guard);
                            }
                        }
                    })
                })
                .collect();

            thread::sleep(Duration::from_micros(200));
            lifecycle.begin_drain();
            stop.store(true, Ordering::SeqCst);
            for worker in workers {
                worker.join().unwrap();
            }
            assert_eq!(lifecycle.active_count(), 0);
        }
    }
}
