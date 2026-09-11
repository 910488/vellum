use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};

use crate::ZcodeDesktopError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZcodeThreadBinding {
    pub vellum_thread_id: String,
    pub zcode_session_id: String,
    pub workspace: String,
    pub artifact_sha256: String,
    pub runtime_instance_id: String,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

pub struct ZcodeBindingStore {
    connection: Mutex<Connection>,
}

impl ZcodeBindingStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ZcodeDesktopError> {
        let connection = Connection::open(path).map_err(db)?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS zcode_thread_bindings (
                    vellum_thread_id TEXT PRIMARY KEY NOT NULL,
                    zcode_session_id TEXT NOT NULL,
                    workspace TEXT NOT NULL,
                    artifact_sha256 TEXT NOT NULL,
                    runtime_instance_id TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    last_seen_at TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS ix_zcode_binding_session
                   ON zcode_thread_bindings(zcode_session_id);",
            )
            .map_err(db)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn upsert(&self, binding: &ZcodeThreadBinding) -> Result<(), ZcodeDesktopError> {
        self.connection
            .lock()
            .expect("binding db poisoned")
            .execute(
                "INSERT INTO zcode_thread_bindings (
                vellum_thread_id, zcode_session_id, workspace, artifact_sha256,
                runtime_instance_id, created_at, last_seen_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(vellum_thread_id) DO UPDATE SET
                zcode_session_id=excluded.zcode_session_id,
                workspace=excluded.workspace,
                artifact_sha256=excluded.artifact_sha256,
                runtime_instance_id=excluded.runtime_instance_id,
                last_seen_at=excluded.last_seen_at",
                params![
                    binding.vellum_thread_id,
                    binding.zcode_session_id,
                    binding.workspace,
                    binding.artifact_sha256,
                    binding.runtime_instance_id,
                    binding.created_at.to_rfc3339(),
                    binding.last_seen_at.to_rfc3339()
                ],
            )
            .map_err(db)?;
        Ok(())
    }

    pub fn get(
        &self,
        vellum_thread_id: &str,
    ) -> Result<Option<ZcodeThreadBinding>, ZcodeDesktopError> {
        let connection = self.connection.lock().expect("binding db poisoned");
        connection
            .query_row(
                "SELECT vellum_thread_id, zcode_session_id, workspace, artifact_sha256,
                        runtime_instance_id, created_at, last_seen_at
                 FROM zcode_thread_bindings WHERE vellum_thread_id=?1",
                [vellum_thread_id],
                decode,
            )
            .optional()
            .map_err(db)
    }

    pub fn remove(&self, vellum_thread_id: &str) -> Result<(), ZcodeDesktopError> {
        self.connection
            .lock()
            .expect("binding db poisoned")
            .execute(
                "DELETE FROM zcode_thread_bindings WHERE vellum_thread_id=?1",
                [vellum_thread_id],
            )
            .map_err(db)?;
        Ok(())
    }
}

fn decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<ZcodeThreadBinding> {
    let parse = |index| -> rusqlite::Result<DateTime<Utc>> {
        let raw: String = row.get(index)?;
        DateTime::parse_from_rfc3339(&raw)
            .map(|time| time.with_timezone(&Utc))
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
    };
    Ok(ZcodeThreadBinding {
        vellum_thread_id: row.get(0)?,
        zcode_session_id: row.get(1)?,
        workspace: row.get(2)?,
        artifact_sha256: row.get(3)?,
        runtime_instance_id: row.get(4)?,
        created_at: parse(5)?,
        last_seen_at: parse(6)?,
    })
}

fn db(error: rusqlite::Error) -> ZcodeDesktopError {
    ZcodeDesktopError::Binding(error.to_string())
}
