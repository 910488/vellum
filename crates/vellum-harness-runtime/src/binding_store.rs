use std::{path::Path, sync::Mutex};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use vellum_harness_protocol::{HarnessId, HarnessSessionBinding};

#[derive(Debug, thiserror::Error)]
pub enum HarnessBindingStoreError {
    #[error("binding database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("binding timestamp is invalid: {0}")]
    Timestamp(String),
}

/// Durable index only. It contains no transcript and is never a source for
/// recreating a native session after that runtime says the session is gone.
pub struct HarnessBindingStore {
    connection: Mutex<Connection>,
}
impl HarnessBindingStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, HarnessBindingStoreError> {
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS harness_session_bindings (
                ui_thread_id TEXT PRIMARY KEY NOT NULL,
                harness_id TEXT NOT NULL,
                runtime_instance_id TEXT NOT NULL,
                native_session_id TEXT NOT NULL,
                workspace TEXT NOT NULL,
                capability_snapshot_hash TEXT NOT NULL,
                created_at TEXT NOT NULL,
                last_seen_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS ix_harness_binding_native
               ON harness_session_bindings(harness_id, native_session_id);",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }
    pub fn upsert(&self, binding: &HarnessSessionBinding) -> Result<(), HarnessBindingStoreError> {
        self.connection.lock().expect("binding db poisoned").execute(
            "INSERT INTO harness_session_bindings (ui_thread_id,harness_id,runtime_instance_id,native_session_id,workspace,capability_snapshot_hash,created_at,last_seen_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(ui_thread_id) DO UPDATE SET harness_id=excluded.harness_id,runtime_instance_id=excluded.runtime_instance_id,native_session_id=excluded.native_session_id,workspace=excluded.workspace,capability_snapshot_hash=excluded.capability_snapshot_hash,last_seen_at=excluded.last_seen_at",
            params![binding.ui_thread_id, binding.harness_id.0, binding.runtime_instance_id, binding.native_session_id, binding.workspace.to_string_lossy(), binding.capability_snapshot_hash, binding.created_at.to_rfc3339(), binding.last_seen_at.to_rfc3339()],
        )?;
        Ok(())
    }
    pub fn get(
        &self,
        thread_id: &str,
    ) -> Result<Option<HarnessSessionBinding>, HarnessBindingStoreError> {
        let connection = self.connection.lock().expect("binding db poisoned");
        connection.query_row(
            "SELECT ui_thread_id,harness_id,runtime_instance_id,native_session_id,workspace,capability_snapshot_hash,created_at,last_seen_at FROM harness_session_bindings WHERE ui_thread_id=?1",
            [thread_id], decode,
        ).optional().map_err(Into::into)
    }
    pub fn touch(
        &self,
        thread_id: &str,
        at: DateTime<Utc>,
    ) -> Result<(), HarnessBindingStoreError> {
        self.connection
            .lock()
            .expect("binding db poisoned")
            .execute(
                "UPDATE harness_session_bindings SET last_seen_at=?2 WHERE ui_thread_id=?1",
                params![thread_id, at.to_rfc3339()],
            )?;
        Ok(())
    }
    pub fn remove(&self, thread_id: &str) -> Result<(), HarnessBindingStoreError> {
        self.connection
            .lock()
            .expect("binding db poisoned")
            .execute(
                "DELETE FROM harness_session_bindings WHERE ui_thread_id=?1",
                [thread_id],
            )?;
        Ok(())
    }
}
fn decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<HarnessSessionBinding> {
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
    Ok(HarnessSessionBinding {
        ui_thread_id: row.get(0)?,
        harness_id: HarnessId(row.get(1)?),
        runtime_instance_id: row.get(2)?,
        native_session_id: row.get(3)?,
        workspace: row.get::<_, String>(4)?.into(),
        capability_snapshot_hash: row.get(5)?,
        created_at: parse(6)?,
        last_seen_at: parse(7)?,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binding_round_trip_keeps_only_binding_data() {
        let temp = tempfile::tempdir().unwrap();
        let store = HarnessBindingStore::open(temp.path().join("bindings.sqlite")).unwrap();
        let now = Utc::now();
        let binding = HarnessSessionBinding {
            ui_thread_id: "ui-1".into(),
            harness_id: HarnessId("grok-build".into()),
            runtime_instance_id: "runtime-1".into(),
            native_session_id: "native-1".into(),
            workspace: temp.path().into(),
            capability_snapshot_hash: "sha256:caps".into(),
            created_at: now,
            last_seen_at: now,
        };
        store.upsert(&binding).unwrap();
        assert_eq!(
            store.get("ui-1").unwrap().unwrap().native_session_id,
            "native-1"
        );
        store.remove("ui-1").unwrap();
        assert!(store.get("ui-1").unwrap().is_none());
    }
}
