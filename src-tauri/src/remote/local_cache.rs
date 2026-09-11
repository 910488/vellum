//! Local non-secret remote cache for host metadata and ack positions.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedHost {
    pub id: String,
    pub name: String,
    pub ssh_alias: String,
    pub broker_url: String,
    pub device_id: String,
    #[serde(default)]
    pub device_token: Option<String>,
    #[serde(default)]
    pub last_thread_id: Option<String>,
    #[serde(default)]
    pub last_ack_seq: u64,
    #[serde(default = "default_cursor_scheme")]
    pub cursor_scheme: String,
}

const ACTIVE_HOST_KEY: &str = "activeHostId";

fn default_cursor_scheme() -> String {
    vellum_remote_protocol::CURSOR_SCHEME.to_string()
}

pub struct RemoteLocalCache {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl RemoteLocalCache {
    pub fn open(path: impl Into<PathBuf>) -> AppResult<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| AppError::Message(error.to_string()))?;
        }
        let conn = Connection::open(&path).map_err(|error| AppError::Message(error.to_string()))?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS remote_meta (
                key TEXT PRIMARY KEY,
                value TEXT
            );
            CREATE TABLE IF NOT EXISTS remote_hosts (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                ssh_alias TEXT NOT NULL,
                broker_url TEXT NOT NULL,
                device_id TEXT NOT NULL,
                device_token TEXT,
                last_thread_id TEXT,
                last_ack_seq INTEGER NOT NULL DEFAULT 0,
                cursor_scheme TEXT NOT NULL DEFAULT 'thread-seq-v1'
            );
            ",
        )
        .map_err(|error| AppError::Message(error.to_string()))?;
        let _ = conn.execute("ALTER TABLE remote_hosts ADD COLUMN device_token TEXT", []);
        let _ = conn.execute(
            "ALTER TABLE remote_hosts ADD COLUMN cursor_scheme TEXT NOT NULL DEFAULT 'thread-seq-v1'",
            [],
        );
        Ok(Self {
            conn: Mutex::new(conn),
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list_hosts(&self) -> AppResult<Vec<CachedHost>> {
        let conn = self.conn.lock().expect("remote cache poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT id, name, ssh_alias, broker_url, device_id, device_token, last_thread_id, last_ack_seq, cursor_scheme
                 FROM remote_hosts
                 ORDER BY name ASC",
            )
            .map_err(|error| AppError::Message(error.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(CachedHost {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    ssh_alias: row.get(2)?,
                    broker_url: row.get(3)?,
                    device_id: row.get(4)?,
                    device_token: row.get(5)?,
                    last_thread_id: row.get(6)?,
                    last_ack_seq: row.get::<_, i64>(7)? as u64,
                    cursor_scheme: row
                        .get::<_, String>(8)
                        .unwrap_or_else(|_| default_cursor_scheme()),
                })
            })
            .map_err(|error| AppError::Message(error.to_string()))?;
        let mut hosts = Vec::new();
        for row in rows {
            hosts.push(row.map_err(|error| AppError::Message(error.to_string()))?);
        }
        Ok(hosts)
    }

    pub fn upsert_host(&self, host: &CachedHost) -> AppResult<()> {
        let conn = self.conn.lock().expect("remote cache poisoned");
        conn.execute(
            "INSERT INTO remote_hosts(
                id, name, ssh_alias, broker_url, device_id, device_token, last_thread_id, last_ack_seq, cursor_scheme
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                ssh_alias = excluded.ssh_alias,
                broker_url = excluded.broker_url,
                device_id = excluded.device_id,
                device_token = excluded.device_token,
                last_thread_id = excluded.last_thread_id,
                last_ack_seq = excluded.last_ack_seq,
                cursor_scheme = excluded.cursor_scheme",
            params![
                host.id,
                host.name,
                host.ssh_alias,
                host.broker_url,
                host.device_id,
                host.device_token,
                host.last_thread_id,
                host.last_ack_seq as i64,
                host.cursor_scheme
            ],
        )
        .map_err(|error| AppError::Message(error.to_string()))?;
        Ok(())
    }

    pub fn remove_host(&self, host_id: &str) -> AppResult<()> {
        let conn = self.conn.lock().expect("remote cache poisoned");
        conn.execute("DELETE FROM remote_hosts WHERE id = ?1", params![host_id])
            .map_err(|error| AppError::Message(error.to_string()))?;
        conn.execute(
            "DELETE FROM remote_meta WHERE key = ?1 AND value = ?2",
            params![ACTIVE_HOST_KEY, host_id],
        )
        .map_err(|error| AppError::Message(error.to_string()))?;
        Ok(())
    }

    /// The host Remote Manager currently has selected, or `None` when nothing
    /// is selected or the selection named a host that has since been removed.
    ///
    /// Read before any push that follows a Desktop-side change: an account
    /// switch reaches this one host and no other, so an unreachable machine
    /// somewhere in the inventory can never hold up the switch itself, and no
    /// host the user is not looking at changes identity behind their back.
    /// Resolving through `remote_hosts` rather than trusting the stored id is
    /// what keeps a stale selection from naming a host that no longer exists.
    pub fn active_host(&self) -> AppResult<Option<String>> {
        let conn = self.conn.lock().expect("remote cache poisoned");
        Ok(conn
            .query_row(
                "SELECT meta.value FROM remote_meta AS meta
                 JOIN remote_hosts AS host ON host.id = meta.value
                 WHERE meta.key = ?1",
                params![ACTIVE_HOST_KEY],
                |row| row.get(0),
            )
            .ok())
    }

    pub fn set_active_host(&self, host_id: Option<&str>) -> AppResult<()> {
        let conn = self.conn.lock().expect("remote cache poisoned");
        match host_id {
            Some(host_id) => conn.execute(
                "INSERT INTO remote_meta(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![ACTIVE_HOST_KEY, host_id],
            ),
            None => conn.execute(
                "DELETE FROM remote_meta WHERE key = ?1",
                params![ACTIVE_HOST_KEY],
            ),
        }
        .map_err(|error| AppError::Message(error.to_string()))?;
        Ok(())
    }

    pub fn invalidate_ack_if_cursor_scheme_mismatch(
        &self,
        host_id: &str,
        expected_scheme: &str,
    ) -> AppResult<bool> {
        let conn = self.conn.lock().expect("remote cache poisoned");
        let current: Option<String> = conn
            .query_row(
                "SELECT cursor_scheme FROM remote_hosts WHERE id = ?1",
                params![host_id],
                |row| row.get(0),
            )
            .ok();
        if current.as_deref().unwrap_or(expected_scheme) == expected_scheme {
            return Ok(false);
        }
        conn.execute(
            "UPDATE remote_hosts
             SET last_ack_seq = 0,
                 cursor_scheme = ?1
             WHERE id = ?2",
            params![expected_scheme, host_id],
        )
        .map_err(|error| AppError::Message(error.to_string()))?;
        Ok(true)
    }

    pub fn update_ack(&self, host_id: &str, thread_id: &str, through_seq: u64) -> AppResult<()> {
        let conn = self.conn.lock().expect("remote cache poisoned");
        conn.execute(
            "UPDATE remote_hosts
             SET last_thread_id = ?1,
                 last_ack_seq = CASE
                    WHEN ?2 > last_ack_seq THEN ?2
                    ELSE last_ack_seq
                 END
             WHERE id = ?3",
            params![thread_id, through_seq as i64, host_id],
        )
        .map_err(|error| AppError::Message(error.to_string()))?;
        Ok(())
    }
}
