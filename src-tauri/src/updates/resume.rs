//! A Desktop update restarts Vellum, and nothing starts the Proxy on an
//! ordinary launch. When the Proxy was serving Codex before the restart, the
//! exiting process leaves this marker so the relaunched build picks it back up.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// An installer that never relaunched Vellum must not start the Proxy on
/// whatever launch comes next, possibly days later.
const FRESH_FOR: Duration = Duration::from_secs(30 * 60);

fn marker(data_root: &Path) -> PathBuf {
    data_root.join("updates").join("resume-proxy")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

pub fn remember_running_proxy(data_root: &Path) {
    if let Err(error) = super::write_atomic(&marker(data_root), now_secs().to_string().as_bytes()) {
        log::warn!("[Updates] cannot remember the running proxy across the restart: {error}");
    }
}

/// Consumes the marker; true when it was left by a restart moments ago.
pub fn take_proxy_resume(data_root: &Path) -> bool {
    take_at(data_root, now_secs())
}

fn take_at(data_root: &Path, now: u64) -> bool {
    let path = marker(data_root);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let _ = std::fs::remove_file(&path);
    text.trim()
        .parse::<u64>()
        .is_ok_and(|written| now.saturating_sub(written) <= FRESH_FOR.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_marker_resumes_once() {
        let root = tempfile::tempdir().unwrap();
        assert!(!take_proxy_resume(root.path()));
        remember_running_proxy(root.path());
        assert!(take_proxy_resume(root.path()));
        assert!(!take_proxy_resume(root.path()));
    }

    #[test]
    fn a_marker_from_an_old_restart_is_dropped() {
        let root = tempfile::tempdir().unwrap();
        remember_running_proxy(root.path());
        assert!(!take_at(root.path(), now_secs() + FRESH_FOR.as_secs() + 1));
        assert!(!marker(root.path()).exists());
    }
}
