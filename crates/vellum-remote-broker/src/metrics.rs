//! Lightweight in-process metrics counters.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct BrokerMetrics {
    pub connected_clients: AtomicU64,
    pub active_threads: AtomicU64,
    pub active_turns: AtomicU64,
    pub pending_approvals: AtomicU64,
    pub event_seq: AtomicU64,
    pub reconnect_total: AtomicU64,
    pub upstream_epoch: AtomicU64,
    pub upstream_disconnect_total: AtomicU64,
    pub replay_events_total: AtomicU64,
    pub snapshot_total: AtomicU64,
    pub command_idempotency_hits_total: AtomicU64,
    pub orphaned_approvals_total: AtomicU64,
}

impl BrokerMetrics {
    pub fn snapshot_json(&self) -> serde_json::Value {
        serde_json::json!({
            "vellum_remote_connected_clients": self.connected_clients.load(Ordering::Relaxed),
            "vellum_remote_active_threads": self.active_threads.load(Ordering::Relaxed),
            "vellum_remote_active_turns": self.active_turns.load(Ordering::Relaxed),
            "vellum_remote_pending_approvals": self.pending_approvals.load(Ordering::Relaxed),
            "vellum_remote_event_seq": self.event_seq.load(Ordering::Relaxed),
            "vellum_remote_reconnect_total": self.reconnect_total.load(Ordering::Relaxed),
            "vellum_remote_upstream_epoch": self.upstream_epoch.load(Ordering::Relaxed),
            "vellum_remote_upstream_disconnect_total": self.upstream_disconnect_total.load(Ordering::Relaxed),
            "vellum_remote_replay_events_total": self.replay_events_total.load(Ordering::Relaxed),
            "vellum_remote_snapshot_total": self.snapshot_total.load(Ordering::Relaxed),
            "vellum_remote_command_idempotency_hits_total": self.command_idempotency_hits_total.load(Ordering::Relaxed),
            "vellum_remote_orphaned_approvals_total": self.orphaned_approvals_total.load(Ordering::Relaxed),
        })
    }
}
