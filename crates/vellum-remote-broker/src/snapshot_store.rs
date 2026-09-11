//! Snapshot persistence and selection.

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use vellum_remote_protocol::ThreadSnapshot;

use crate::event_store::StoreError;

#[derive(Clone)]
pub struct SnapshotStore {
    conn: Arc<Mutex<Connection>>,
}

impl SnapshotStore {
    pub fn from_shared(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub fn open_with_path(path: &std::path::Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn save_snapshot(&self, snapshot: &ThreadSnapshot) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(snapshot)?;
        let digest = hex::encode(Sha256::digest(&bytes));
        let now = Utc::now().timestamp_millis();
        let conn = self.conn.lock().expect("snapshot store poisoned");
        conn.execute(
            "INSERT INTO remote_snapshots(
                thread_id, snapshot_seq, created_at_ms, payload_ciphertext, payload_sha256
             ) VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(thread_id, snapshot_seq) DO UPDATE SET
                created_at_ms = excluded.created_at_ms,
                payload_ciphertext = excluded.payload_ciphertext,
                payload_sha256 = excluded.payload_sha256",
            params![
                snapshot.thread_id,
                snapshot.snapshot_seq as i64,
                now,
                bytes,
                digest
            ],
        )?;
        conn.execute(
            "UPDATE remote_threads
             SET last_snapshot_seq = ?1, updated_at_ms = ?2
             WHERE thread_id = ?3",
            params![snapshot.snapshot_seq as i64, now, snapshot.thread_id],
        )?;
        Ok(())
    }

    pub fn latest_snapshot(&self, thread_id: &str) -> Result<Option<ThreadSnapshot>, StoreError> {
        let conn = self.conn.lock().expect("snapshot store poisoned");
        let row = conn
            .query_row(
                "SELECT payload_ciphertext
                 FROM remote_snapshots
                 WHERE thread_id = ?1
                 ORDER BY snapshot_seq DESC
                 LIMIT 1",
                params![thread_id],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        Ok(match row {
            Some(bytes) => Some(serde_json::from_slice(&bytes)?),
            None => None,
        })
    }

    pub fn build_fallback_snapshot(
        thread_id: &str,
        snapshot_seq: u64,
        status: vellum_remote_protocol::ThreadRuntimeStatus,
        loaded: bool,
        pending_approvals: Vec<vellum_remote_protocol::PendingApproval>,
        writer_lease: Option<vellum_remote_protocol::WriterLeaseView>,
    ) -> ThreadSnapshot {
        ThreadSnapshot {
            thread_id: thread_id.to_string(),
            snapshot_seq,
            status,
            loaded,
            active_turn: None,
            pending_approvals,
            items: Vec::new(),
            usage: None,
            writer_lease,
        }
    }

    pub fn raw_json(snapshot: &ThreadSnapshot) -> Result<Value, StoreError> {
        Ok(serde_json::to_value(snapshot)?)
    }
}
