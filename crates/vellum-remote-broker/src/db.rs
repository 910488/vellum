//! Shared SQLite database bootstrap and versioned migrations.
//!
//! All remote stores must share one connection opened through this module.
//! Migrations run exactly once per schema version and never on request hot paths.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};

use crate::event_store::StoreError;

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_remote_core.sql"),
    include_str!("../migrations/0002_writer_leases.sql"),
    include_str!("../migrations/0003_snapshots.sql"),
    include_str!("../migrations/0004_approval_request_id_json.sql"),
    include_str!("../migrations/0005_command_sha256.sql"),
    include_str!("../migrations/0006_thread_seq.sql"),
];

#[derive(Clone)]
pub struct BrokerDatabase {
    conn: Arc<Mutex<Connection>>,
}

impl BrokerDatabase {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| StoreError::Message(error.to_string()))?;
        }
        let conn = Connection::open(path)?;
        configure_connection(&conn)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        configure_connection(&conn)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn connection(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }

    pub fn schema_version(&self) -> Result<u32, StoreError> {
        let conn = self.conn.lock().expect("broker database poisoned");
        read_schema_version(&conn)
    }
}

fn configure_connection(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = FULL;
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 5000;
        PRAGMA wal_autocheckpoint = 1000;
        ",
    )?;
    Ok(())
}

fn migrate(conn: &Connection) -> Result<(), StoreError> {
    // schema_meta is created by migration 0001. Until then version is 0.
    let mut version = read_schema_version(conn).unwrap_or(0);
    while (version as usize) < MIGRATIONS.len() {
        let next = version as usize;
        let tx = conn.unchecked_transaction()?;
        // Some upgrade paths may already contain columns introduced by older
        // hot-path ALTERs (e.g. command_sha256). Ignore duplicate-column noise
        // inside a migration transaction and still advance schema_version.
        match tx.execute_batch(MIGRATIONS[next]) {
            Ok(()) => {}
            Err(error) if is_duplicate_column_error(&error) => {
                log::warn!(
                    "migration {} reported duplicate column; continuing: {error}",
                    next + 1
                );
            }
            Err(error) => return Err(error.into()),
        }
        version = (next as u32) + 1;
        if version >= 1 {
            tx.execute(
                "INSERT INTO schema_meta(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params!["schema_version", version.to_string()],
            )?;
        }
        tx.commit()?;
    }
    Ok(())
}

fn is_duplicate_column_error(error: &rusqlite::Error) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    text.contains("duplicate column") || text.contains("already exists")
}

fn read_schema_version(conn: &Connection) -> Result<u32, StoreError> {
    let table_exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_meta'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !table_exists {
        return Ok(0);
    }
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value.and_then(|raw| raw.parse::<u32>().ok()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn broker_start_fresh_database() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("remote.sqlite3");
        let db = BrokerDatabase::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as u32);
        // Column from migration 0004 must exist.
        let conn = db.connection();
        let guard = conn.lock().unwrap();
        let count: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('pending_server_requests')
                 WHERE name = 'upstream_request_id_json'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn broker_start_existing_v3_database() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("remote.sqlite3");
        {
            let conn = Connection::open(&path).unwrap();
            configure_connection(&conn).unwrap();
            conn.execute_batch(include_str!("../migrations/0001_remote_core.sql"))
                .unwrap();
            conn.execute_batch(include_str!("../migrations/0002_writer_leases.sql"))
                .unwrap();
            conn.execute_batch(include_str!("../migrations/0003_snapshots.sql"))
                .unwrap();
            conn.execute(
                "INSERT INTO schema_meta(key, value) VALUES('schema_version', '3')",
                [],
            )
            .unwrap();
        }
        let db = BrokerDatabase::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as u32);
        let conn = db.connection();
        let guard = conn.lock().unwrap();
        let count: i64 = guard
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('pending_server_requests')
                 WHERE name = 'upstream_request_id_json'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn broker_start_existing_v4_with_preexisting_command_sha256() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("remote.sqlite3");
        {
            let conn = Connection::open(&path).unwrap();
            configure_connection(&conn).unwrap();
            for sql in [
                include_str!("../migrations/0001_remote_core.sql"),
                include_str!("../migrations/0002_writer_leases.sql"),
                include_str!("../migrations/0003_snapshots.sql"),
                include_str!("../migrations/0004_approval_request_id_json.sql"),
            ] {
                conn.execute_batch(sql).unwrap();
            }
            // Simulate 903302c hot-path ALTER before formal migration 0005.
            conn.execute(
                "ALTER TABLE client_commands ADD COLUMN command_sha256 TEXT",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO schema_meta(key, value) VALUES('schema_version', '4')
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [],
            )
            .unwrap();
        }
        let db = BrokerDatabase::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as u32);
    }

    #[test]
    fn v5_to_v6_interleaved_threads_cursor_migration() {
        use rusqlite::params;
        let dir = tempdir().unwrap();
        let path = dir.path().join("remote.sqlite3");
        {
            let conn = Connection::open(&path).unwrap();
            configure_connection(&conn).unwrap();
            for sql in [
                include_str!("../migrations/0001_remote_core.sql"),
                include_str!("../migrations/0002_writer_leases.sql"),
                include_str!("../migrations/0003_snapshots.sql"),
                include_str!("../migrations/0004_approval_request_id_json.sql"),
                include_str!("../migrations/0005_command_sha256.sql"),
            ] {
                conn.execute_batch(sql).unwrap();
            }
            conn.execute(
                "INSERT INTO schema_meta(key, value) VALUES('schema_version', '5')
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO upstream_epochs(epoch, started_at_ms, ended_at_ms, end_reason, codex_version, codex_home, socket_path, server_identity_json, compatibility_state)
                 VALUES(1, 1, NULL, NULL, '0.1', NULL, NULL, NULL, 'compatible')",
                [],
            )
            .unwrap();
            for (thread_id, status, last_seq) in [("A", "running", 3), ("B", "idle", 2)] {
                conn.execute(
                    "INSERT INTO remote_threads(
                        thread_id, cwd, title, source_kind, latest_status, loaded,
                        active_turn_id, last_upstream_epoch, last_event_seq, last_snapshot_seq,
                        created_at_ms, updated_at_ms, metadata_json
                     ) VALUES(?1, '/tmp', NULL, 'remote', ?2, 1, NULL, 1, ?3, ?3, 1, 1, NULL)",
                    params![thread_id, status, last_seq],
                )
                .unwrap();
            }
            // Global seq order: A1, B2, A3
            for (thread_id, method) in [("A", "a1"), ("B", "b1"), ("A", "a2")] {
                conn.execute(
                    "INSERT INTO remote_events(
                        thread_id, turn_id, item_id, upstream_epoch, method,
                        occurred_at_ms, received_at_ms, payload_ciphertext, payload_sha256, flags
                     ) VALUES(?1, NULL, NULL, 1, ?2, 1, 1, X'7B7D', 'x', 0)",
                    params![thread_id, method],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO remote_snapshots(thread_id, snapshot_seq, created_at_ms, payload_ciphertext, payload_sha256)
                 VALUES('A', 3, 1, X'7B7D', 'snap')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO client_thread_acks(device_id, thread_id, through_seq, updated_at_ms)
                 VALUES('dev', 'A', 3, 1)",
                [],
            )
            .unwrap();
        }

        let db = BrokerDatabase::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), MIGRATIONS.len() as u32);
        let conn = db.connection();
        let guard = conn.lock().unwrap();
        let a_last: i64 = guard
            .query_row(
                "SELECT last_event_seq FROM remote_threads WHERE thread_id = 'A'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let b_last: i64 = guard
            .query_row(
                "SELECT last_event_seq FROM remote_threads WHERE thread_id = 'B'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(a_last, 2, "A global 1/3 becomes thread_seq 1/2");
        assert_eq!(b_last, 1, "B global 2 becomes thread_seq 1");
        let snaps: i64 = guard
            .query_row("SELECT COUNT(*) FROM remote_snapshots", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(snaps, 0);
        let acks: i64 = guard
            .query_row("SELECT COUNT(*) FROM client_thread_acks", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(acks, 0);
        let a_max_thread_seq: i64 = guard
            .query_row(
                "SELECT MAX(thread_seq) FROM remote_events WHERE thread_id = 'A'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(a_max_thread_seq, 2);
    }

    #[test]
    fn broker_restart_same_database() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("remote.sqlite3");
        let first = BrokerDatabase::open(&path).unwrap();
        assert_eq!(first.schema_version().unwrap(), MIGRATIONS.len() as u32);
        drop(first);
        let second = BrokerDatabase::open(&path).unwrap();
        assert_eq!(second.schema_version().unwrap(), MIGRATIONS.len() as u32);
        // Opening twice must not fail on ALTER TABLE duplicate column.
        let third = BrokerDatabase::open(&path).unwrap();
        assert_eq!(third.schema_version().unwrap(), MIGRATIONS.len() as u32);
    }
}
