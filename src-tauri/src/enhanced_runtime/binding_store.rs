use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

use super::{ExecutionPlane, ThreadRuntimeBinding, BINDING_VERSION};

/// Durable index only. Resume uses the stored runtime digest; the store never
/// replays a transcript onto a different execution plane.
pub struct ThreadRuntimeBindingStore {
    connection: Mutex<Connection>,
}

impl ThreadRuntimeBindingStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BindingStoreError> {
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS thread_runtime_bindings (
                thread_id TEXT PRIMARY KEY NOT NULL,
                binding_version INTEGER NOT NULL,
                plane TEXT NOT NULL,
                runtime_digest TEXT NOT NULL,
                provider_id TEXT NOT NULL,
                model_id TEXT NOT NULL,
                native_thread_id TEXT NOT NULL,
                created_at INTEGER NOT NULL
             );",
        )?;
        drop_retired_protocol_hash(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn insert_immutable(
        &self,
        binding: &ThreadRuntimeBinding,
    ) -> Result<(), BindingStoreError> {
        if binding.binding_version != BINDING_VERSION {
            return Err(BindingStoreError::UnsupportedVersion(
                binding.binding_version,
            ));
        }
        if let Some(existing) = self.get(&binding.thread_id)? {
            if existing.binding_version != binding.binding_version
                || existing.plane != binding.plane
                || existing.runtime_digest != binding.runtime_digest
                || existing.provider_id != binding.provider_id
                || existing.model_id != binding.model_id
                || existing.native_thread_id != binding.native_thread_id
            {
                return Err(BindingStoreError::Immutable {
                    thread_id: binding.thread_id.clone(),
                });
            }
            return Ok(());
        }
        self.connection
            .lock()
            .expect("binding db poisoned")
            .execute(
                "INSERT INTO thread_runtime_bindings (
                thread_id, binding_version, plane, runtime_digest,
                provider_id, model_id, native_thread_id, created_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    binding.thread_id,
                    binding.binding_version,
                    binding.plane.as_str(),
                    binding.runtime_digest,
                    binding.provider_id,
                    binding.model_id,
                    binding.native_thread_id,
                    binding.created_at
                ],
            )?;
        Ok(())
    }

    pub fn get(&self, thread_id: &str) -> Result<Option<ThreadRuntimeBinding>, BindingStoreError> {
        let connection = self.connection.lock().expect("binding db poisoned");
        connection
            .query_row(
                "SELECT thread_id, binding_version, plane, runtime_digest,
                        provider_id, model_id, native_thread_id, created_at
                 FROM thread_runtime_bindings WHERE thread_id=?1",
                [thread_id],
                decode,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Return the durable execution-plane index. This is metadata only: the
    /// bridge uses it to seed its observation surface after a restart and
    /// never treats it as a transcript or a live connection.
    pub fn list(&self) -> Result<Vec<ThreadRuntimeBinding>, BindingStoreError> {
        let connection = self.connection.lock().expect("binding db poisoned");
        let mut statement = connection.prepare(
            "SELECT thread_id, binding_version, plane, runtime_digest,
                    provider_id, model_id, native_thread_id, created_at
             FROM thread_runtime_bindings ORDER BY created_at DESC",
        )?;
        let bindings = statement
            .query_map([], decode)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(bindings)
    }
}

fn decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadRuntimeBinding> {
    let plane_raw: String = row.get(2)?;
    let plane = ExecutionPlane::parse(&plane_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown execution plane {plane_raw}"),
            )),
        )
    })?;
    Ok(ThreadRuntimeBinding {
        thread_id: row.get(0)?,
        binding_version: row.get(1)?,
        plane,
        runtime_digest: row.get(3)?,
        provider_id: row.get(4)?,
        model_id: row.get(5)?,
        native_thread_id: row.get(6)?,
        created_at: row.get(7)?,
    })
}

/// Databases written before the app-server protocol pin was retired carry a
/// `NOT NULL` column nothing supplies any more, which would fail every insert.
/// Dropping it is safe: the value was a constant, so no row held information.
fn drop_retired_protocol_hash(connection: &Connection) -> Result<(), BindingStoreError> {
    let present = connection
        .prepare("SELECT 1 FROM pragma_table_info('thread_runtime_bindings') WHERE name='app_server_protocol_hash'")?
        .exists([])?;
    if present {
        connection.execute_batch(
            "ALTER TABLE thread_runtime_bindings DROP COLUMN app_server_protocol_hash;",
        )?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum BindingStoreError {
    #[error("thread runtime binding database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("unsupported thread runtime binding version {0}")]
    UnsupportedVersion(u16),
    #[error("thread {thread_id} runtime binding is immutable; create a new thread to change execution plane")]
    Immutable { thread_id: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_is_immutable_after_insert() {
        let temp = tempfile::tempdir().unwrap();
        let store = ThreadRuntimeBindingStore::open(temp.path().join("b.sqlite")).unwrap();
        let binding = ThreadRuntimeBinding::new(
            "t1",
            ExecutionPlane::EnhancedCodex,
            "digest-a",
            "qwen",
            "qwen3-coder",
            "native-1",
            10,
        );
        store.insert_immutable(&binding).unwrap();
        store.insert_immutable(&binding).unwrap();
        let mut switched = binding.clone();
        switched.plane = ExecutionPlane::OfficialCodex;
        switched.runtime_digest = "digest-official".into();
        assert!(store.insert_immutable(&switched).is_err());
        let mut model_switch = binding.clone();
        model_switch.model_id = "other-model".into();
        assert!(store.insert_immutable(&model_switch).is_err());
        let loaded = store.get("t1").unwrap().unwrap();
        assert_eq!(loaded.runtime_digest, "digest-a");
        assert_eq!(loaded.plane, ExecutionPlane::EnhancedCodex);
        assert_eq!(loaded.model_id, "qwen3-coder");
        assert_eq!(store.list().unwrap(), vec![binding]);
    }

    /* 之前的資料庫帶著 app_server_protocol_hash NOT NULL。欄位退休以後沒有
    人再供這個值，所以升級上來的使用者第一次寫 binding 就會失敗 —— 那是
    建立執行緒的路徑，壞掉不會有第二次機會。 */
    #[test]
    fn a_database_written_before_the_pin_was_retired_still_takes_writes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("legacy.sqlite");
        let legacy = Connection::open(&path).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE thread_runtime_bindings (
                    thread_id TEXT PRIMARY KEY NOT NULL,
                    binding_version INTEGER NOT NULL,
                    plane TEXT NOT NULL,
                    runtime_digest TEXT NOT NULL,
                    app_server_protocol_hash TEXT NOT NULL,
                    provider_id TEXT NOT NULL,
                    model_id TEXT NOT NULL,
                    native_thread_id TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                 );
                 INSERT INTO thread_runtime_bindings VALUES
                    ('old', 1, 'enhanced-codex', 'digest-a', 'sha256:pin',
                     'qwen', 'qwen3-coder', 'native-0', 1);",
            )
            .unwrap();
        drop(legacy);

        let store = ThreadRuntimeBindingStore::open(&path).unwrap();
        // 既有的 binding 要讀得回來，不是被遷移掉。
        let loaded = store.get("old").unwrap().unwrap();
        assert_eq!(loaded.runtime_digest, "digest-a");
        assert_eq!(loaded.model_id, "qwen3-coder");
        // 而且新的執行緒要寫得進去。
        store
            .insert_immutable(&ThreadRuntimeBinding::new(
                "new",
                ExecutionPlane::EnhancedCodex,
                "digest-a",
                "qwen",
                "qwen3-coder",
                "native-1",
                2,
            ))
            .unwrap();
        assert!(store.get("new").unwrap().is_some());
    }
}
