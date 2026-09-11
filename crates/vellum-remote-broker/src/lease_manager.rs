//! Writer lease acquisition and heartbeat.

use chrono::{Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::{Arc, Mutex};
use ulid::Ulid;
use vellum_remote_protocol::WriterLeaseView;

use crate::event_store::StoreError;

#[derive(Clone)]
pub struct LeaseManager {
    conn: Arc<Mutex<Connection>>,
    ttl_secs: i64,
}

impl LeaseManager {
    pub fn from_shared(conn: Arc<Mutex<Connection>>, ttl_secs: u64) -> Self {
        Self {
            conn,
            ttl_secs: ttl_secs as i64,
        }
    }

    pub fn ensure_device(&self, device_id: &str, display_name: &str) -> Result<(), StoreError> {
        let now = Utc::now().timestamp_millis();
        let conn = self.conn.lock().expect("lease manager poisoned");
        conn.execute(
            "INSERT INTO client_devices(
                device_id, display_name, public_key, token_hash, capabilities_json,
                created_at_ms, last_seen_at_ms, revoked_at_ms
             ) VALUES(?1, ?2, NULL, NULL, NULL, ?3, ?3, NULL)
             ON CONFLICT(device_id) DO UPDATE SET
                display_name = excluded.display_name,
                last_seen_at_ms = excluded.last_seen_at_ms",
            params![device_id, display_name, now],
        )?;
        Ok(())
    }

    pub fn acquire(
        &self,
        thread_id: &str,
        device_id: &str,
    ) -> Result<Result<WriterLeaseView, WriterLeaseView>, StoreError> {
        let now = Utc::now();
        let expires = now + Duration::seconds(self.ttl_secs);
        let lease_id = format!("lease_{}", Ulid::new());
        let conn = self.conn.lock().expect("lease manager poisoned");
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM writer_leases WHERE expires_at_ms < ?1",
            params![now.timestamp_millis()],
        )?;
        let existing = tx
            .query_row(
                "SELECT lease_id, device_id, expires_at_ms, generation
                 FROM writer_leases WHERE thread_id = ?1",
                params![thread_id],
                |row| {
                    Ok(WriterLeaseView {
                        lease_id: row.get(0)?,
                        holder_device_id: row.get(1)?,
                        expires_at: chrono::DateTime::from_timestamp_millis(row.get(2)?)
                            .unwrap_or(now),
                        generation: row.get::<_, i64>(3)? as u64,
                    })
                },
            )
            .optional()?;
        if let Some(holder) = existing {
            if holder.holder_device_id != device_id {
                tx.commit()?;
                return Ok(Err(holder));
            }
            tx.execute(
                "UPDATE writer_leases
                 SET expires_at_ms = ?1
                 WHERE thread_id = ?2",
                params![expires.timestamp_millis(), thread_id],
            )?;
            tx.commit()?;
            return Ok(Ok(WriterLeaseView {
                lease_id: holder.lease_id,
                holder_device_id: device_id.to_string(),
                expires_at: expires,
                generation: holder.generation,
            }));
        }

        tx.execute(
            "INSERT INTO writer_leases(
                thread_id, lease_id, device_id, acquired_at_ms, expires_at_ms, generation
             ) VALUES(?1, ?2, ?3, ?4, ?5, 1)",
            params![
                thread_id,
                lease_id,
                device_id,
                now.timestamp_millis(),
                expires.timestamp_millis()
            ],
        )?;
        tx.commit()?;
        Ok(Ok(WriterLeaseView {
            lease_id,
            holder_device_id: device_id.to_string(),
            expires_at: expires,
            generation: 1,
        }))
    }

    pub fn heartbeat(
        &self,
        thread_id: &str,
        device_id: &str,
        lease_id: &str,
    ) -> Result<Option<WriterLeaseView>, StoreError> {
        let now = Utc::now();
        let expires = now + Duration::seconds(self.ttl_secs);
        let conn = self.conn.lock().expect("lease manager poisoned");
        let changed = conn.execute(
            "UPDATE writer_leases
             SET expires_at_ms = ?1
             WHERE thread_id = ?2 AND device_id = ?3 AND lease_id = ?4",
            params![expires.timestamp_millis(), thread_id, device_id, lease_id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        // Read generation while still holding the same connection lock to avoid
        // re-entrant Mutex deadlock via current_generation().
        let generation = conn
            .query_row(
                "SELECT generation FROM writer_leases WHERE thread_id = ?1",
                params![thread_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .map(|value| value as u64)
            .unwrap_or(1);
        Ok(Some(WriterLeaseView {
            lease_id: lease_id.to_string(),
            holder_device_id: device_id.to_string(),
            expires_at: expires,
            generation,
        }))
    }

    pub fn current(&self, thread_id: &str) -> Result<Option<WriterLeaseView>, StoreError> {
        let now = Utc::now().timestamp_millis();
        let conn = self.conn.lock().expect("lease manager poisoned");
        conn.execute(
            "DELETE FROM writer_leases WHERE expires_at_ms < ?1",
            params![now],
        )?;
        let row = conn
            .query_row(
                "SELECT lease_id, device_id, expires_at_ms, generation
                 FROM writer_leases WHERE thread_id = ?1",
                params![thread_id],
                |row| {
                    Ok(WriterLeaseView {
                        lease_id: row.get(0)?,
                        holder_device_id: row.get(1)?,
                        expires_at: chrono::DateTime::from_timestamp_millis(row.get(2)?)
                            .unwrap_or_else(Utc::now),
                        generation: row.get::<_, i64>(3)? as u64,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn release(
        &self,
        thread_id: &str,
        device_id: &str,
        lease_id: &str,
    ) -> Result<bool, StoreError> {
        let conn = self.conn.lock().expect("lease manager poisoned");
        let changed = conn.execute(
            "DELETE FROM writer_leases
             WHERE thread_id = ?1 AND device_id = ?2 AND lease_id = ?3",
            params![thread_id, device_id, lease_id],
        )?;
        Ok(changed > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::sync::{Arc, Mutex};

    fn open_manager() -> LeaseManager {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA busy_timeout = 1000;
            ",
        )
        .unwrap();
        conn.execute_batch(include_str!("../migrations/0001_remote_core.sql"))
            .unwrap();
        conn.execute_batch(include_str!("../migrations/0002_writer_leases.sql"))
            .unwrap();
        LeaseManager::from_shared(Arc::new(Mutex::new(conn)), 30)
    }

    #[test]
    fn acquire_free_lease_and_heartbeat() {
        let mgr = open_manager();
        mgr.ensure_device("dev-a", "Device A").unwrap();
        let first = mgr.acquire("thr_1", "dev-a").unwrap().unwrap();
        assert_eq!(first.holder_device_id, "dev-a");
        let hb = mgr
            .heartbeat("thr_1", "dev-a", &first.lease_id)
            .unwrap()
            .unwrap();
        assert_eq!(hb.lease_id, first.lease_id);
        assert!(hb.expires_at >= first.expires_at);
    }

    #[test]
    fn acquire_conflict_from_other_device() {
        let mgr = open_manager();
        mgr.ensure_device("dev-a", "A").unwrap();
        mgr.ensure_device("dev-b", "B").unwrap();
        let _ = mgr.acquire("thr_1", "dev-a").unwrap().unwrap();
        let conflict = mgr.acquire("thr_1", "dev-b").unwrap();
        assert!(conflict.is_err());
        let holder = conflict.err().unwrap();
        assert_eq!(holder.holder_device_id, "dev-a");
    }
}
