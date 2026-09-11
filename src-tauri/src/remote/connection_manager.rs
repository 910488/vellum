//! Multi-host remote host cache owned by AppState.
//!
//! This previously also owned live WebSocket connections to
//! `vellum-remote-broker` hosts (`RemoteClient`), plus thread/turn/writer
//! command plumbing. That surface had zero callers from the shipped Desktop
//! frontend (`src/lib/api.ts`) — the live Remote flow talks to the native
//! remote agent over SSH via `RemoteAgentClient`, not the broker protocol —
//! and was removed as dead code in the Stage G security-hardening pass. See
//! `docs/adr/ADR-001-codex-host-native.md`: "The Broker is legacy/diagnostic-
//! only and is not part of the production session data path."
//!
//! What remains is the non-secret host cache (`CachedHost`) that native
//! discovery/bootstrap (`discovery.rs`, `bootstrap.rs`, `deployment.rs`,
//! `restore.rs`) still reads and writes through `list_hosts` /
//! `import_discovered_host`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::AppResult;
use crate::remote::discovery::RemoteHostCandidate;
use crate::remote::local_cache::{CachedHost, RemoteLocalCache};

/// M35: how long one `host.managerSnapshot` result stays usable without a
/// fresh SSH round trip. Short enough that a deliberate "refresh" a few
/// seconds later still sees live data; long enough that the handful of
/// Tauri commands one Remote Manager page load fires within the same tick
/// (`inspect_remote_host`, `get_desktop_codex_compatibility`,
/// `get_remote_session_summary`) share a single probe instead of each
/// opening their own SSH process.
const SNAPSHOT_CACHE_TTL: Duration = Duration::from_millis(1500);

#[derive(Clone)]
struct CachedSnapshot {
    value: Value,
    fetched_at: Instant,
    generation: u64,
}

pub struct RemoteClientManager {
    cache: RemoteLocalCache,
    dropped_legacy_broker_hosts: Vec<String>,
    snapshot_cache: Mutex<HashMap<String, CachedSnapshot>>,
    /// Per-host single-flight lock so concurrent callers for the *same*
    /// host serialize onto one SSH process instead of racing separate ones;
    /// callers for different hosts never block each other.
    snapshot_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    snapshot_generation: AtomicU64,
}

impl RemoteClientManager {
    pub fn open(data_root: PathBuf) -> AppResult<Self> {
        let cache = RemoteLocalCache::open(data_root.join("remote-cache.sqlite3"))?;
        let dropped_legacy_broker_hosts = match cache.list_hosts() {
            Ok(hosts) => drop_stale_legacy_broker_hosts(&cache, &hosts),
            Err(_) => Vec::new(),
        };
        Ok(Self {
            cache,
            dropped_legacy_broker_hosts,
            snapshot_cache: Mutex::new(HashMap::new()),
            snapshot_locks: Mutex::new(HashMap::new()),
            snapshot_generation: AtomicU64::new(0),
        })
    }

    /// M35: at most one live `host.managerSnapshot` SSH round trip per host
    /// within [`SNAPSHOT_CACHE_TTL`]. A cache hit returns immediately; a
    /// miss acquires a per-host lock (so concurrent callers for the same
    /// host serialize instead of each spawning their own SSH process,
    /// rather than each running `fetch` independently), re-checks the cache
    /// once the lock is held (the caller that was ahead of us may have just
    /// populated it), and only then calls `fetch`. `fetch` should perform
    /// the actual `RemoteAgentClient::host_manager_snapshot` RPC; it is
    /// passed in rather than done here so this module never needs to know
    /// about `RemoteAgentClient` or SSH.
    pub fn host_manager_snapshot(
        &self,
        host_id: &str,
        fetch: impl FnOnce() -> AppResult<Value>,
    ) -> AppResult<(Value, u64)> {
        if let Some(entry) = self.fresh_snapshot(host_id) {
            return Ok((entry.value, entry.generation));
        }
        let lock = {
            let mut locks = self.snapshot_locks.lock().expect("state poisoned");
            Arc::clone(
                locks
                    .entry(host_id.to_string())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let _guard = lock.lock().expect("state poisoned");
        if let Some(entry) = self.fresh_snapshot(host_id) {
            return Ok((entry.value, entry.generation));
        }
        let value = fetch()?;
        let generation = self.snapshot_generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.snapshot_cache.lock().expect("state poisoned").insert(
            host_id.to_string(),
            CachedSnapshot {
                value: value.clone(),
                fetched_at: Instant::now(),
                generation,
            },
        );
        Ok((value, generation))
    }

    /// Whatever `host_manager_snapshot` last cached for this host. TTL only
    /// decides when to refresh; it never erases the last-good value.
    pub fn cached_snapshot(&self, host_id: &str) -> Option<Value> {
        self.snapshot_cache
            .lock()
            .expect("state poisoned")
            .get(host_id)
            .map(|entry| entry.value.clone())
    }

    fn fresh_snapshot(&self, host_id: &str) -> Option<CachedSnapshot> {
        let cache = self.snapshot_cache.lock().expect("state poisoned");
        let entry = cache.get(host_id)?;
        if entry.fetched_at.elapsed() < SNAPSHOT_CACHE_TTL {
            Some(entry.clone())
        } else {
            None
        }
    }

    /// Force the next `host_manager_snapshot` call for this host to hit the
    /// network — call after any mutation (start/stop/install/repair/...) so
    /// the UI never shows pre-mutation state for up to `SNAPSHOT_CACHE_TTL`.
    pub fn invalidate_snapshot(&self, host_id: &str) {
        self.snapshot_cache
            .lock()
            .expect("state poisoned")
            .remove(host_id);
    }

    /// Remove all per-host snapshot state when a host itself is deleted.
    pub fn forget_snapshot_host(&self, host_id: &str) {
        self.invalidate_snapshot(host_id);
        self.snapshot_locks
            .lock()
            .expect("state poisoned")
            .remove(host_id);
    }

    /// Display names of leftover broker-paired cache rows dropped on open.
    /// The UI shows an unsupported-migration notice when this is non-empty.
    pub fn dropped_legacy_broker_hosts(&self) -> &[String] {
        &self.dropped_legacy_broker_hosts
    }

    pub fn list_hosts(&self) -> AppResult<Vec<CachedHost>> {
        self.cache.list_hosts()
    }

    pub fn active_host(&self) -> AppResult<Option<String>> {
        self.cache.active_host()
    }

    pub fn set_active_host(&self, host_id: Option<&str>) -> AppResult<()> {
        self.cache.set_active_host(host_id)
    }

    /// Persist only the non-secret mapping needed by the native Remote
    /// Manager.  Broker pairing remains a separate legacy action.
    pub fn import_discovered_host(&self, candidate: &RemoteHostCandidate) -> AppResult<CachedHost> {
        validate_ssh_alias(&candidate.ssh_alias)?;
        if let Some(existing) = self
            .cache
            .list_hosts()?
            .into_iter()
            .find(|host| host.id == candidate.vellum_host_id)
        {
            return Ok(existing);
        }
        let host = CachedHost {
            id: candidate.vellum_host_id.clone(),
            name: candidate.display_name.clone(),
            ssh_alias: candidate.ssh_alias.clone(),
            broker_url: "ws://127.0.0.1:45100/ws".into(),
            device_id: format!("native-{}", candidate.vellum_host_id),
            device_token: None,
            last_thread_id: None,
            last_ack_seq: 0,
            cursor_scheme: vellum_remote_protocol::CURSOR_SCHEME.to_string(),
        };
        self.cache.upsert_host(&host)?;
        Ok(host)
    }
}

/// Legacy on-disk state: the now-removed `remote_add_host` command let a
/// `CachedHost` be created with a user-supplied `broker_url` pointing at a
/// paired `vellum-remote-broker` instance. There is no command left that can
/// act on such a row (connect/disconnect/list_threads/etc. are gone), and the
/// frontend never rendered `broker_url` distinctly from the native-discovery
/// default (`ws://127.0.0.1:45100/ws`). Drop any stray legacy rows on load so
/// they don't linger silently; this is a no-op for hosts created only through
/// native discovery.
fn is_legacy_paired_broker_url(broker_url: &str) -> bool {
    const NATIVE_DISCOVERY_BROKER_URL: &str = "ws://127.0.0.1:45100/ws";
    let trimmed = broker_url.trim();
    !trimmed.is_empty() && trimmed != NATIVE_DISCOVERY_BROKER_URL
}

fn drop_stale_legacy_broker_hosts(cache: &RemoteLocalCache, hosts: &[CachedHost]) -> Vec<String> {
    let mut dropped = Vec::new();
    for host in hosts {
        if !is_legacy_paired_broker_url(&host.broker_url) {
            continue;
        }
        log::warn!(
            "remote host {} ({}) has a legacy paired broker_url ({}) with no remaining broker command surface; dropping the cached row",
            host.id,
            host.name,
            host.broker_url
        );
        if cache.remove_host(&host.id).is_ok() {
            dropped.push(host.name.clone());
        }
    }
    dropped
}

fn validate_ssh_alias(alias: &str) -> AppResult<()> {
    let alias = alias.trim();
    if alias.is_empty() {
        return Err(crate::error::AppError::Message(
            "ssh_alias must not be empty".into(),
        ));
    }
    if alias.starts_with('-') {
        return Err(crate::error::AppError::Message(
            "ssh_alias must not start with '-' (could be parsed as an ssh option)".into(),
        ));
    }
    if alias.chars().any(|c| c == '\0' || c == '\n' || c == '\r') {
        return Err(crate::error::AppError::Message(
            "ssh_alias must not contain NUL or newline characters".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: &str, name: &str, broker_url: &str) -> CachedHost {
        CachedHost {
            id: id.into(),
            name: name.into(),
            ssh_alias: name.into(),
            broker_url: broker_url.into(),
            device_id: format!("native-{id}"),
            device_token: None,
            last_thread_id: None,
            last_ack_seq: 0,
            cursor_scheme: vellum_remote_protocol::CURSOR_SCHEME.to_string(),
        }
    }

    #[test]
    fn open_drops_legacy_paired_broker_hosts_and_keeps_native_discovery_rows() {
        let temp = tempfile::tempdir().unwrap();
        let cache = RemoteLocalCache::open(temp.path().join("remote-cache.sqlite3")).unwrap();
        cache
            .upsert_host(&host("native", "Jetson", "ws://127.0.0.1:45100/ws"))
            .unwrap();
        cache
            .upsert_host(&host("legacy", "Old Broker Box", "ws://10.0.0.8:45100/ws"))
            .unwrap();
        drop(cache);

        let manager = RemoteClientManager::open(temp.path().to_path_buf()).unwrap();
        let remaining: Vec<_> = manager
            .list_hosts()
            .unwrap()
            .into_iter()
            .map(|host| host.id)
            .collect();
        assert_eq!(remaining, vec!["native".to_string()]);
        assert_eq!(
            manager.dropped_legacy_broker_hosts(),
            &["Old Broker Box".to_string()]
        );
    }

    /// A Desktop account switch follows exactly one host, so a selection that
    /// no longer names a live host must read as "no host" rather than as an id
    /// to push to. Removing the host is the ordinary way that happens; a cache
    /// carried over from a previous install is the other.
    #[test]
    fn the_active_host_never_outlives_the_host_it_names() {
        let temp = tempfile::tempdir().unwrap();
        let cache = RemoteLocalCache::open(temp.path().join("remote-cache.sqlite3")).unwrap();
        assert_eq!(cache.active_host().unwrap(), None);

        cache
            .upsert_host(&host("jetson", "Jetson", "ws://127.0.0.1:45100/ws"))
            .unwrap();
        cache.set_active_host(Some("jetson")).unwrap();
        assert_eq!(cache.active_host().unwrap(), Some("jetson".to_string()));

        // Selecting a host that was never imported must not resolve either.
        cache.set_active_host(Some("ghost")).unwrap();
        assert_eq!(cache.active_host().unwrap(), None);

        cache.set_active_host(Some("jetson")).unwrap();
        cache.remove_host("jetson").unwrap();
        assert_eq!(cache.active_host().unwrap(), None);
    }

    #[test]
    fn clearing_the_selection_leaves_no_host_to_push_to() {
        let temp = tempfile::tempdir().unwrap();
        let cache = RemoteLocalCache::open(temp.path().join("remote-cache.sqlite3")).unwrap();
        cache
            .upsert_host(&host("jetson", "Jetson", "ws://127.0.0.1:45100/ws"))
            .unwrap();
        cache.set_active_host(Some("jetson")).unwrap();
        cache.set_active_host(None).unwrap();
        assert_eq!(cache.active_host().unwrap(), None);
    }

    #[test]
    fn empty_broker_url_is_not_treated_as_a_legacy_pairing() {
        assert!(!is_legacy_paired_broker_url(""));
        assert!(!is_legacy_paired_broker_url("ws://127.0.0.1:45100/ws"));
        assert!(is_legacy_paired_broker_url("ws://192.168.1.9:45100/ws"));
    }

    fn manager() -> RemoteClientManager {
        let temp = tempfile::tempdir().unwrap();
        RemoteClientManager::open(temp.path().to_path_buf()).unwrap()
    }

    #[test]
    fn a_second_call_within_the_ttl_never_invokes_fetch_again() {
        let manager = manager();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let fetch = || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({"probe": calls.load(Ordering::SeqCst)}))
        };

        let (first, generation1) = manager.host_manager_snapshot("host-a", fetch).unwrap();
        let (second, generation2) = manager.host_manager_snapshot("host-a", fetch).unwrap();

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second call must be a cache hit"
        );
        assert_eq!(first, second);
        assert_eq!(generation1, generation2);
    }

    #[test]
    fn different_hosts_never_share_a_cache_entry() {
        let manager = manager();
        let (a, _) = manager
            .host_manager_snapshot("host-a", || Ok(serde_json::json!({"host": "a"})))
            .unwrap();
        let (b, _) = manager
            .host_manager_snapshot("host-b", || Ok(serde_json::json!({"host": "b"})))
            .unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn invalidate_forces_the_next_call_to_fetch_again() {
        let manager = manager();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let fetch = || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({"probe": calls.load(Ordering::SeqCst)}))
        };

        manager.host_manager_snapshot("host-a", fetch).unwrap();
        manager.invalidate_snapshot("host-a");
        manager.host_manager_snapshot("host-a", fetch).unwrap();

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "invalidate must force a fresh fetch"
        );
    }

    #[test]
    fn a_failed_fetch_is_never_cached() {
        let manager = manager();
        let result = manager.host_manager_snapshot("host-a", || {
            Err::<Value, _>(crate::error::AppError::Message("agent unreachable".into()))
        });
        assert!(result.is_err());
        assert!(
            manager.cached_snapshot("host-a").is_none(),
            "a failed probe must not poison the cache with nothing, and a later successful \
             probe must be free to populate it"
        );
    }

    #[test]
    fn cached_snapshot_reads_without_ever_calling_fetch() {
        let manager = manager();
        assert!(manager.cached_snapshot("host-a").is_none());
        manager
            .host_manager_snapshot("host-a", || Ok(serde_json::json!({"session": null})))
            .unwrap();
        assert_eq!(
            manager.cached_snapshot("host-a"),
            Some(serde_json::json!({"session": null}))
        );
    }

    #[test]
    fn forgetting_a_host_removes_cache_and_single_flight_state() {
        let manager = manager();
        manager
            .host_manager_snapshot("host-a", || Ok(serde_json::json!({"ok": true})))
            .unwrap();
        assert!(manager
            .snapshot_locks
            .lock()
            .unwrap()
            .contains_key("host-a"));

        manager.forget_snapshot_host("host-a");

        assert!(manager.cached_snapshot("host-a").is_none());
        assert!(!manager
            .snapshot_locks
            .lock()
            .unwrap()
            .contains_key("host-a"));
    }

    #[test]
    fn concurrent_probes_for_the_same_host_collapse_into_one_fetch() {
        let manager = std::sync::Arc::new(manager());
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let manager = std::sync::Arc::clone(&manager);
                let calls = std::sync::Arc::clone(&calls);
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    manager
                        .host_manager_snapshot("host-a", || {
                            calls.fetch_add(1, Ordering::SeqCst);
                            // Give any racing caller time to reach the lock
                            // while this one holds it, so a bug that fails
                            // to serialize would show up as > 1 fetch.
                            std::thread::sleep(Duration::from_millis(20));
                            Ok(serde_json::json!({"probe": true}))
                        })
                        .unwrap()
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "four concurrent probes for the same host must collapse into one SSH-equivalent fetch"
        );
    }
}
