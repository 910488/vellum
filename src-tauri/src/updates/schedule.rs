//! A Desktop restart that waits for running work instead of refusing it.
//!
//! "Restart and update" used to fail with `proxyBusy` / `coreBusy` while a
//! Codex turn was running, which left the user to keep pressing the button.
//! Now the press is remembered: a watcher restarts Vellum once the proxy and
//! the Enhanced core have stayed quiet for a few polls in a row. A single
//! quiet poll is not enough, because a turn goes quiet between its requests.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::state::AppState;

const POLL: Duration = Duration::from_secs(2);
/// Ten seconds of no proxy request and no open core turn.
const QUIET_POLLS: u32 = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RestartSchedule {
    pub version: Option<String>,
    /// Codex turns the Enhanced core reports open right now.
    pub open_turns: u32,
    /// Requests the proxy is serving right now.
    pub active_requests: u64,
    pub since: i64,
}

struct Pending {
    id: u64,
    version: Option<String>,
    since: i64,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn work(state: &AppState) -> (u32, u64) {
    let evidence = super::live_idle_evidence(state, false);
    (evidence.open_turns, state.active_requests())
}

/// The waiting restart, with the work it is waiting for counted live.
pub fn restart_schedule(state: &AppState) -> Option<RestartSchedule> {
    let (version, since) = {
        let pending = PENDING.lock().expect("restart schedule poisoned");
        let pending = pending.as_ref()?;
        (pending.version.clone(), pending.since)
    };
    let (open_turns, active_requests) = work(state);
    Some(RestartSchedule {
        version,
        open_turns,
        active_requests,
        since,
    })
}

/// Remembers the restart and runs `restart` once Vellum has been quiet for
/// [`QUIET_POLLS`] polls. Scheduling again replaces the earlier schedule.
pub fn schedule_restart<F, Fut>(state: AppState, version: Option<String>, restart: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    *PENDING.lock().expect("restart schedule poisoned") = Some(Pending {
        id,
        version,
        since: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    });
    log::info!("[Updates] desktop restart scheduled until running work finishes");
    tauri::async_runtime::spawn(async move {
        let mut quiet = Quiet::default();
        loop {
            tokio::time::sleep(POLL).await;
            if !is_current(id) {
                return;
            }
            let (open_turns, active_requests) = work(&state);
            if quiet.observe(open_turns == 0 && active_requests == 0) {
                if take(id) {
                    log::info!("[Updates] scheduled desktop restart is starting");
                    restart().await;
                }
                return;
            }
        }
    });
}

/// Returns whether a restart was waiting.
pub fn cancel_scheduled_restart() -> bool {
    let cancelled = PENDING
        .lock()
        .expect("restart schedule poisoned")
        .take()
        .is_some();
    if cancelled {
        log::info!("[Updates] scheduled desktop restart cancelled");
    }
    cancelled
}

fn is_current(id: u64) -> bool {
    PENDING
        .lock()
        .expect("restart schedule poisoned")
        .as_ref()
        .is_some_and(|pending| pending.id == id)
}

fn take(id: u64) -> bool {
    let mut pending = PENDING.lock().expect("restart schedule poisoned");
    if pending.as_ref().is_some_and(|item| item.id == id) {
        *pending = None;
        true
    } else {
        false
    }
}

#[derive(Default)]
struct Quiet(u32);

impl Quiet {
    /// True once the last [`QUIET_POLLS`] observations were all idle.
    fn observe(&mut self, idle: bool) -> bool {
        self.0 = if idle { self.0 + 1 } else { 0 };
        self.0 >= QUIET_POLLS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gap_between_requests_does_not_count_as_finished() {
        let mut quiet = Quiet::default();
        for _ in 0..QUIET_POLLS - 1 {
            assert!(!quiet.observe(true));
        }
        assert!(
            !quiet.observe(false),
            "work came back before the window closed"
        );
        for _ in 0..QUIET_POLLS - 1 {
            assert!(!quiet.observe(true));
        }
        assert!(quiet.observe(true));
    }
}
