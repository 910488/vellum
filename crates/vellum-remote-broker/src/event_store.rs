//! Durable event persistence for remote sessions.

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use vellum_remote_protocol::{ThreadEvent, ThreadRuntimeStatus};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Message(String),
}

#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub seq: u64,
    pub thread_seq: u64,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub upstream_epoch: u64,
    pub method: String,
    pub occurred_at: DateTime<Utc>,
    pub payload: Value,
}

/// Durable projection fields updated with events and recovery outcomes.
#[derive(Debug, Clone)]
pub struct ThreadProjectionUpdate {
    pub status: ThreadRuntimeStatus,
    pub loaded: bool,
    /// `Some(None)` clears the active turn; `None` leaves it unchanged.
    pub active_turn_id: Option<Option<String>>,
    pub upstream_epoch: Option<u64>,
}

#[derive(Clone)]
pub struct EventStore {
    conn: Arc<Mutex<Connection>>,
}

impl EventStore {
    pub fn from_shared(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Convenience helper for unit tests. Production code must open via BrokerDatabase.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let db = crate::db::BrokerDatabase::open(path)?;
        Ok(Self::from_shared(db.connection()))
    }

    pub fn ensure_thread(
        &self,
        thread_id: &str,
        cwd: Option<&str>,
        status: ThreadRuntimeStatus,
    ) -> Result<(), StoreError> {
        let conn = self.conn.lock().expect("event store poisoned");
        Self::ensure_thread_on(&conn, thread_id, cwd, status)
    }

    pub fn update_thread_projection(
        &self,
        thread_id: &str,
        update: &ThreadProjectionUpdate,
    ) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();
        let conn = self.conn.lock().expect("event store poisoned");
        Self::ensure_thread_on(&conn, thread_id, None, update.status)?;
        match &update.active_turn_id {
            Some(Some(turn_id)) => {
                conn.execute(
                    "UPDATE remote_threads
                     SET latest_status = ?1,
                         loaded = ?2,
                         active_turn_id = ?3,
                         last_upstream_epoch = COALESCE(?4, last_upstream_epoch),
                         updated_at_ms = ?5
                     WHERE thread_id = ?6",
                    params![
                        status_name(update.status),
                        if update.loaded { 1 } else { 0 },
                        turn_id,
                        update.upstream_epoch.map(|v| v as i64),
                        now,
                        thread_id
                    ],
                )?;
            }
            Some(None) => {
                conn.execute(
                    "UPDATE remote_threads
                     SET latest_status = ?1,
                         loaded = ?2,
                         active_turn_id = NULL,
                         last_upstream_epoch = COALESCE(?3, last_upstream_epoch),
                         updated_at_ms = ?4
                     WHERE thread_id = ?5",
                    params![
                        status_name(update.status),
                        if update.loaded { 1 } else { 0 },
                        update.upstream_epoch.map(|v| v as i64),
                        now,
                        thread_id
                    ],
                )?;
            }
            None => {
                conn.execute(
                    "UPDATE remote_threads
                     SET latest_status = ?1,
                         loaded = ?2,
                         last_upstream_epoch = COALESCE(?3, last_upstream_epoch),
                         updated_at_ms = ?4
                     WHERE thread_id = ?5",
                    params![
                        status_name(update.status),
                        if update.loaded { 1 } else { 0 },
                        update.upstream_epoch.map(|v| v as i64),
                        now,
                        thread_id
                    ],
                )?;
            }
        }
        Ok(())
    }

    pub fn ensure_epoch(&self, epoch: u64, codex_version: Option<&str>) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();
        let conn = self.conn.lock().expect("event store poisoned");
        conn.execute(
            "INSERT INTO upstream_epochs(
                epoch, started_at_ms, ended_at_ms, end_reason, codex_version,
                codex_home, socket_path, server_identity_json, compatibility_state
             ) VALUES(?1, ?2, NULL, NULL, ?3, NULL, NULL, NULL, 'compatible')
             ON CONFLICT(epoch) DO NOTHING",
            params![epoch as i64, now, codex_version],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_event(
        &self,
        thread_id: &str,
        upstream_epoch: u64,
        method: &str,
        payload: Value,
        turn_id: Option<&str>,
        item_id: Option<&str>,
        occurred_at: DateTime<Utc>,
        projection: &ThreadProjectionUpdate,
    ) -> Result<StoredEvent, StoreError> {
        let received_at = Utc::now();
        let payload_bytes = serde_json::to_vec(&payload)?;
        let digest = hex::encode(Sha256::digest(&payload_bytes));
        let conn = self.conn.lock().expect("event store poisoned");
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO upstream_epochs(
                epoch, started_at_ms, ended_at_ms, end_reason, codex_version,
                codex_home, socket_path, server_identity_json, compatibility_state
             ) VALUES(?1, ?2, NULL, NULL, NULL, NULL, NULL, NULL, 'compatible')
             ON CONFLICT(epoch) DO NOTHING",
            params![upstream_epoch as i64, received_at.timestamp_millis()],
        )?;
        Self::ensure_thread_on(&tx, thread_id, None, projection.status)?;
        let next_thread_seq: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(thread_seq), 0) + 1 FROM remote_events WHERE thread_id = ?1",
                params![thread_id],
                |row| row.get(0),
            )
            .unwrap_or(1);
        tx.execute(
            "INSERT INTO remote_events(
                thread_id, turn_id, item_id, upstream_epoch, method,
                occurred_at_ms, received_at_ms, payload_ciphertext, payload_sha256, flags, thread_seq
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10)",
            params![
                thread_id,
                turn_id,
                item_id,
                upstream_epoch as i64,
                method,
                occurred_at.timestamp_millis(),
                received_at.timestamp_millis(),
                payload_bytes,
                digest,
                next_thread_seq
            ],
        )?;
        let seq = tx.last_insert_rowid() as u64;
        match &projection.active_turn_id {
            Some(None) => {
                tx.execute(
                    "UPDATE remote_threads
                     SET last_event_seq = ?1,
                         last_upstream_epoch = ?2,
                         latest_status = ?3,
                         loaded = ?4,
                         updated_at_ms = ?5,
                         active_turn_id = NULL
                     WHERE thread_id = ?6",
                    params![
                        next_thread_seq,
                        upstream_epoch as i64,
                        status_name(projection.status),
                        if projection.loaded { 1 } else { 0 },
                        received_at.timestamp_millis(),
                        thread_id
                    ],
                )?;
            }
            Some(Some(turn)) => {
                tx.execute(
                    "UPDATE remote_threads
                     SET last_event_seq = ?1,
                         last_upstream_epoch = ?2,
                         latest_status = ?3,
                         loaded = ?4,
                         updated_at_ms = ?5,
                         active_turn_id = ?6
                     WHERE thread_id = ?7",
                    params![
                        next_thread_seq,
                        upstream_epoch as i64,
                        status_name(projection.status),
                        if projection.loaded { 1 } else { 0 },
                        received_at.timestamp_millis(),
                        turn,
                        thread_id
                    ],
                )?;
            }
            None => {
                tx.execute(
                    "UPDATE remote_threads
                     SET last_event_seq = ?1,
                         last_upstream_epoch = ?2,
                         latest_status = ?3,
                         loaded = ?4,
                         updated_at_ms = ?5,
                         active_turn_id = COALESCE(?6, active_turn_id)
                     WHERE thread_id = ?7",
                    params![
                        next_thread_seq,
                        upstream_epoch as i64,
                        status_name(projection.status),
                        if projection.loaded { 1 } else { 0 },
                        received_at.timestamp_millis(),
                        turn_id,
                        thread_id
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(StoredEvent {
            seq,
            thread_seq: next_thread_seq as u64,
            thread_id: thread_id.to_string(),
            turn_id: turn_id.map(str::to_string),
            item_id: item_id.map(str::to_string),
            upstream_epoch,
            method: method.to_string(),
            occurred_at,
            payload,
        })
    }

    pub fn events_after(
        &self,
        thread_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let conn = self.conn.lock().expect("event store poisoned");
        let mut stmt = conn.prepare(
            "SELECT seq, COALESCE(thread_seq, seq) as thread_seq, thread_id, turn_id, item_id,
                    upstream_epoch, method, occurred_at_ms, payload_ciphertext
             FROM remote_events
             WHERE thread_id = ?1 AND COALESCE(thread_seq, seq) > ?2
             ORDER BY COALESCE(thread_seq, seq) ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![thread_id, after_seq as i64, limit as i64], |row| {
            let payload_bytes: Vec<u8> = row.get(8)?;
            let payload: Value = serde_json::from_slice(&payload_bytes).unwrap_or(Value::Null);
            let occurred_ms: i64 = row.get(7)?;
            Ok(StoredEvent {
                seq: row.get::<_, i64>(0)? as u64,
                thread_seq: row.get::<_, i64>(1)? as u64,
                thread_id: row.get(2)?,
                turn_id: row.get(3)?,
                item_id: row.get(4)?,
                upstream_epoch: row.get::<_, i64>(5)? as u64,
                method: row.get(6)?,
                occurred_at: DateTime::from_timestamp_millis(occurred_ms).unwrap_or_else(Utc::now),
                payload,
            })
        })?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }

    /// Return the complete consolidated thread timeline in chronological
    /// order. High-volume delta notifications stay in the durable event log
    /// but reconnect snapshots need only native turn boundaries and typed item
    /// start/completion records, matching the information in `thread/read`.
    pub fn session_timeline_events(&self, thread_id: &str) -> Result<Vec<StoredEvent>, StoreError> {
        let conn = self.conn.lock().expect("event store poisoned");
        let mut stmt = conn.prepare(
            "SELECT seq, COALESCE(thread_seq, seq) as thread_seq, thread_id, turn_id, item_id,
                    upstream_epoch, method, occurred_at_ms, payload_ciphertext
             FROM remote_events
             WHERE thread_id = ?1
               AND method IN ('turn/started', 'turn/completed', 'item/started', 'item/completed')
             ORDER BY COALESCE(thread_seq, seq) ASC",
        )?;
        let rows = stmt.query_map(params![thread_id], |row| {
            let payload_bytes: Vec<u8> = row.get(8)?;
            let payload: Value = serde_json::from_slice(&payload_bytes).unwrap_or(Value::Null);
            let occurred_ms: i64 = row.get(7)?;
            Ok(StoredEvent {
                seq: row.get::<_, i64>(0)? as u64,
                thread_seq: row.get::<_, i64>(1)? as u64,
                thread_id: row.get(2)?,
                turn_id: row.get(3)?,
                item_id: row.get(4)?,
                upstream_epoch: row.get::<_, i64>(5)? as u64,
                method: row.get(6)?,
                occurred_at: DateTime::from_timestamp_millis(occurred_ms).unwrap_or_else(Utc::now),
                payload,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn latest_seq(&self, thread_id: &str) -> Result<u64, StoreError> {
        let conn = self.conn.lock().expect("event store poisoned");
        let seq: Option<i64> = conn
            .query_row(
                "SELECT last_event_seq FROM remote_threads WHERE thread_id = ?1",
                params![thread_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(seq) = seq {
            return Ok(seq as u64);
        }
        let max_seq: Option<i64> = conn
            .query_row(
                "SELECT MAX(COALESCE(thread_seq, seq)) FROM remote_events WHERE thread_id = ?1",
                params![thread_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(max_seq.unwrap_or(0) as u64)
    }

    pub fn earliest_seq(&self, thread_id: &str) -> Result<Option<u64>, StoreError> {
        let conn = self.conn.lock().expect("event store poisoned");
        let seq: Option<i64> = conn
            .query_row(
                "SELECT MIN(COALESCE(thread_seq, seq)) FROM remote_events WHERE thread_id = ?1",
                params![thread_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(seq.map(|value| value as u64))
    }

    pub fn set_ack(
        &self,
        device_id: &str,
        thread_id: &str,
        through_seq: u64,
    ) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();
        let conn = self.conn.lock().expect("event store poisoned");
        conn.execute(
            "INSERT INTO client_thread_acks(device_id, thread_id, through_seq, updated_at_ms)
             VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(device_id, thread_id) DO UPDATE SET
                through_seq = CASE
                    WHEN excluded.through_seq > client_thread_acks.through_seq THEN excluded.through_seq
                    ELSE client_thread_acks.through_seq
                END,
                updated_at_ms = excluded.updated_at_ms",
            params![device_id, thread_id, through_seq as i64, now],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn store_command(
        &self,
        idempotency_key: &str,
        device_id: &str,
        thread_id: Option<&str>,
        command_kind: &str,
        command: &Value,
        state: &str,
        result: Option<&Value>,
        error: Option<&Value>,
    ) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();
        let command_bytes = serde_json::to_vec(command)?;
        let result_bytes = result.map(serde_json::to_vec).transpose()?;
        let error_bytes = error.map(serde_json::to_vec).transpose()?;
        let conn = self.conn.lock().expect("event store poisoned");
        let digest = hex::encode(Sha256::digest(&command_bytes));
        conn.execute(
            "INSERT INTO client_commands(
                idempotency_key, device_id, thread_id, command_kind, command_ciphertext,
                state, result_ciphertext, error_ciphertext, created_at_ms, updated_at_ms, command_sha256
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)
             ON CONFLICT(idempotency_key) DO UPDATE SET
                state = excluded.state,
                result_ciphertext = excluded.result_ciphertext,
                error_ciphertext = excluded.error_ciphertext,
                updated_at_ms = excluded.updated_at_ms,
                command_sha256 = COALESCE(excluded.command_sha256, client_commands.command_sha256)",
            params![
                idempotency_key,
                device_id,
                thread_id,
                command_kind,
                command_bytes,
                state,
                result_bytes,
                error_bytes,
                now,
                digest
            ],
        )?;
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    pub fn get_command(
        &self,
        idempotency_key: &str,
    ) -> Result<
        Option<(
            String,
            Option<Value>,
            Option<Value>,
            Option<String>,
            Option<String>,
            Option<String>,
        )>,
        StoreError,
    > {
        let conn = self.conn.lock().expect("event store poisoned");
        let row = conn
            .query_row(
                "SELECT state, result_ciphertext, error_ciphertext, command_kind, device_id, command_sha256
                 FROM client_commands WHERE idempotency_key = ?1",
                params![idempotency_key],
                |row| {
                    let state: String = row.get(0)?;
                    let result_bytes: Option<Vec<u8>> = row.get(1)?;
                    let error_bytes: Option<Vec<u8>> = row.get(2)?;
                    let command_kind: String = row.get(3)?;
                    let device_id: String = row.get(4)?;
                    let command_sha256: Option<String> = row.get(5)?;
                    Ok((state, result_bytes, error_bytes, command_kind, device_id, command_sha256))
                },
            )
            .optional()?;
        Ok(match row {
            Some((state, result_bytes, error_bytes, command_kind, device_id, command_sha256)) => {
                Some((
                    state,
                    result_bytes
                        .map(|bytes| serde_json::from_slice(&bytes))
                        .transpose()?,
                    error_bytes
                        .map(|bytes| serde_json::from_slice(&bytes))
                        .transpose()?,
                    Some(command_kind),
                    Some(device_id),
                    command_sha256,
                ))
            }
            None => None,
        })
    }

    pub fn mark_processing_indeterminate(&self) -> Result<u64, StoreError> {
        let conn = self.conn.lock().expect("event store poisoned");
        let now = Utc::now().timestamp_millis();
        let changed = conn.execute(
            "UPDATE client_commands
             SET state = 'indeterminate', updated_at_ms = ?1
             WHERE state = 'processing'
               AND command_kind IN ('thread.start', 'turn.start', 'turn.interrupt', 'approval.respond')",
            params![now],
        )?;
        Ok(changed as u64)
    }

    pub fn list_threads(&self) -> Result<Vec<(String, String, u64)>, StoreError> {
        let conn = self.conn.lock().expect("event store poisoned");
        let mut stmt = conn.prepare(
            "SELECT thread_id, latest_status, last_event_seq
             FROM remote_threads
             ORDER BY updated_at_ms DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u64,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    #[allow(clippy::type_complexity)]
    pub fn thread_projection(
        &self,
        thread_id: &str,
    ) -> Result<Option<(ThreadRuntimeStatus, bool, Option<String>, u64)>, StoreError> {
        let conn = self.conn.lock().expect("event store poisoned");
        let row = conn
            .query_row(
                "SELECT latest_status, loaded, active_turn_id, last_event_seq
                 FROM remote_threads WHERE thread_id = ?1",
                params![thread_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)? as u64,
                    ))
                },
            )
            .optional()?;
        Ok(row.map(|(status, loaded, active_turn_id, last_event_seq)| {
            (
                parse_status(&status),
                loaded != 0,
                active_turn_id,
                last_event_seq,
            )
        }))
    }

    fn ensure_thread_on(
        conn: &Connection,
        thread_id: &str,
        cwd: Option<&str>,
        status: ThreadRuntimeStatus,
    ) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO remote_threads(
                thread_id, cwd, title, source_kind, latest_status, loaded,
                active_turn_id, last_upstream_epoch, last_event_seq, last_snapshot_seq,
                created_at_ms, updated_at_ms, metadata_json
             ) VALUES(?1, ?2, NULL, 'remote', ?3, 0, NULL, NULL, 0, NULL, ?4, ?4, NULL)
             ON CONFLICT(thread_id) DO UPDATE SET
                cwd = COALESCE(excluded.cwd, remote_threads.cwd),
                updated_at_ms = excluded.updated_at_ms",
            params![thread_id, cwd, status_name(status), now],
        )?;
        Ok(())
    }
}

impl StoredEvent {
    pub fn into_protocol(self) -> ThreadEvent {
        ThreadEvent {
            // Downstream cursor is per-thread.
            seq: self.thread_seq,
            thread_id: self.thread_id,
            upstream_epoch: self.upstream_epoch,
            method: self.method,
            occurred_at: self.occurred_at,
            turn_id: self.turn_id,
            item_id: self.item_id,
            data: self.payload,
        }
    }
}

fn status_name(status: ThreadRuntimeStatus) -> &'static str {
    match status {
        ThreadRuntimeStatus::Unknown => "unknown",
        ThreadRuntimeStatus::StoredNotLoaded => "stored_not_loaded",
        ThreadRuntimeStatus::Loading => "loading",
        ThreadRuntimeStatus::Idle => "idle",
        ThreadRuntimeStatus::Running => "running",
        ThreadRuntimeStatus::WaitingForApproval => "waiting_for_approval",
        ThreadRuntimeStatus::Interrupted => "interrupted",
        ThreadRuntimeStatus::Completed => "completed",
        ThreadRuntimeStatus::Failed => "failed",
        ThreadRuntimeStatus::Orphaned => "orphaned",
        ThreadRuntimeStatus::Indeterminate => "indeterminate",
    }
}

fn parse_status(status: &str) -> ThreadRuntimeStatus {
    match status {
        "unknown" => ThreadRuntimeStatus::Unknown,
        "stored_not_loaded" => ThreadRuntimeStatus::StoredNotLoaded,
        "loading" => ThreadRuntimeStatus::Loading,
        "idle" => ThreadRuntimeStatus::Idle,
        "running" => ThreadRuntimeStatus::Running,
        "waiting_for_approval" => ThreadRuntimeStatus::WaitingForApproval,
        "interrupted" => ThreadRuntimeStatus::Interrupted,
        "completed" => ThreadRuntimeStatus::Completed,
        "failed" => ThreadRuntimeStatus::Failed,
        "orphaned" => ThreadRuntimeStatus::Orphaned,
        "indeterminate" => ThreadRuntimeStatus::Indeterminate,
        _ => ThreadRuntimeStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn projection(status: ThreadRuntimeStatus, turn: Option<&str>) -> ThreadProjectionUpdate {
        ThreadProjectionUpdate {
            status,
            loaded: true,
            active_turn_id: Some(turn.map(str::to_string)),
            upstream_epoch: Some(1),
        }
    }

    #[test]
    fn append_and_replay_events_are_ordered() {
        let dir = tempdir().unwrap();
        let store = EventStore::open(&dir.path().join("remote.sqlite3")).unwrap();
        store
            .ensure_thread("t1", Some("/tmp/project"), ThreadRuntimeStatus::Idle)
            .unwrap();
        let first = store
            .append_event(
                "t1",
                1,
                "item/agentMessage/delta",
                serde_json::json!({"delta": "a"}),
                Some("turn-1"),
                Some("item-1"),
                Utc::now(),
                &projection(ThreadRuntimeStatus::Running, Some("turn-1")),
            )
            .unwrap();
        let second = store
            .append_event(
                "t1",
                1,
                "item/agentMessage/delta",
                serde_json::json!({"delta": "b"}),
                Some("turn-1"),
                Some("item-1"),
                Utc::now(),
                &projection(ThreadRuntimeStatus::Running, Some("turn-1")),
            )
            .unwrap();
        assert_eq!(first.thread_seq + 1, second.thread_seq);
        let replay = store.events_after("t1", first.thread_seq, 10).unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].thread_seq, second.thread_seq);
    }

    #[test]
    fn session_timeline_keeps_complete_items_and_drops_stream_deltas() {
        let dir = tempdir().unwrap();
        let store = EventStore::open(&dir.path().join("remote.sqlite3")).unwrap();
        store
            .ensure_thread("t1", None, ThreadRuntimeStatus::Idle)
            .unwrap();
        for (index, method) in [
            "turn/started",
            "item/started",
            "item/commandExecution/outputDelta",
            "item/completed",
            "turn/completed",
        ]
        .into_iter()
        .enumerate()
        {
            store
                .append_event(
                    "t1",
                    1,
                    method,
                    serde_json::json!({"item":{"type":"commandExecution","durationMs":index}}),
                    Some("turn-1"),
                    Some(&format!("item-{index}")),
                    Utc::now(),
                    &projection(ThreadRuntimeStatus::Running, Some("turn-1")),
                )
                .unwrap();
        }
        let timeline = store.session_timeline_events("t1").unwrap();
        assert_eq!(timeline.len(), 4);
        assert_eq!(timeline.first().unwrap().method, "turn/started");
        assert_eq!(timeline.last().unwrap().method, "turn/completed");
        assert!(!timeline.iter().any(|event| event.method.contains("Delta")));
    }

    #[test]
    fn running_thread_projection_is_persisted_for_restart_selection() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("remote.sqlite3");
        {
            let store = EventStore::open(&path).unwrap();
            store
                .ensure_thread("t-run", Some("/tmp/project"), ThreadRuntimeStatus::Idle)
                .unwrap();
            store
                .append_event(
                    "t-run",
                    1,
                    "turn/started",
                    serde_json::json!({"threadId":"t-run","turnId":"turn-9"}),
                    Some("turn-9"),
                    None,
                    Utc::now(),
                    &projection(ThreadRuntimeStatus::Running, Some("turn-9")),
                )
                .unwrap();
        }
        let store = EventStore::open(&path).unwrap();
        let threads = store.list_threads().unwrap();
        let row = threads.iter().find(|(id, _, _)| id == "t-run").unwrap();
        assert_eq!(row.1, "running");
        let proj = store.thread_projection("t-run").unwrap().unwrap();
        assert_eq!(proj.0, ThreadRuntimeStatus::Running);
        assert_eq!(proj.2.as_deref(), Some("turn-9"));
    }

    #[test]
    fn turn_completed_clears_active_turn_id() {
        let dir = tempdir().unwrap();
        let store = EventStore::open(&dir.path().join("remote.sqlite3")).unwrap();
        store
            .ensure_thread("t1", None, ThreadRuntimeStatus::Idle)
            .unwrap();
        store
            .append_event(
                "t1",
                1,
                "turn/started",
                serde_json::json!({"turnId":"turn-1"}),
                Some("turn-1"),
                None,
                Utc::now(),
                &projection(ThreadRuntimeStatus::Running, Some("turn-1")),
            )
            .unwrap();
        store
            .append_event(
                "t1",
                1,
                "turn/completed",
                serde_json::json!({"turn":{"status":"completed"}}),
                Some("turn-1"),
                None,
                Utc::now(),
                &projection(ThreadRuntimeStatus::Completed, None),
            )
            .unwrap();
        let proj = store.thread_projection("t1").unwrap().unwrap();
        assert_eq!(proj.0, ThreadRuntimeStatus::Completed);
        assert!(proj.2.is_none());
    }

    #[test]
    fn completed_thread_is_not_selected_for_recovery() {
        let dir = tempdir().unwrap();
        let store = EventStore::open(&dir.path().join("remote.sqlite3")).unwrap();
        store.ensure_epoch(1, Some("test")).unwrap();
        store
            .ensure_thread("done", None, ThreadRuntimeStatus::Idle)
            .unwrap();
        store
            .update_thread_projection(
                "done",
                &ThreadProjectionUpdate {
                    status: ThreadRuntimeStatus::Completed,
                    loaded: true,
                    active_turn_id: Some(None),
                    upstream_epoch: Some(1),
                },
            )
            .unwrap();
        store
            .ensure_thread("run", None, ThreadRuntimeStatus::Idle)
            .unwrap();
        store
            .update_thread_projection(
                "run",
                &ThreadProjectionUpdate {
                    status: ThreadRuntimeStatus::Running,
                    loaded: true,
                    active_turn_id: Some(Some("turn-x".into())),
                    upstream_epoch: Some(1),
                },
            )
            .unwrap();
        let recover = store
            .list_threads()
            .unwrap()
            .into_iter()
            .filter(|(_, status, _)| {
                matches!(
                    status.as_str(),
                    "running" | "waiting_for_approval" | "indeterminate" | "loading" | "orphaned"
                )
            })
            .map(|(id, _, _)| id)
            .collect::<Vec<_>>();
        assert_eq!(recover, vec!["run".to_string()]);
    }
}
