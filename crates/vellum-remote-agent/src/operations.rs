//! Durable operation journal for idempotent agent mutations.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OperationState {
    Prepared,
    Executing,
    Completed,
    Failed,
    Indeterminate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OperationRecord {
    pub operation_id: String,
    pub method: String,
    pub request_sha256: String,
    pub state: OperationState,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct OperationJournal {
    root: PathBuf,
}

impl OperationJournal {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn ensure(&self) -> Result<(), String> {
        fs::create_dir_all(&self.root)
            .map_err(|error| format!("failed creating {}: {error}", self.root.display()))
    }

    pub fn path_for(&self, operation_id: &str) -> Result<PathBuf, String> {
        validate_operation_id(operation_id)?;
        Ok(self.root.join(format!("{operation_id}.json")))
    }

    pub fn load(&self, operation_id: &str) -> Result<Option<OperationRecord>, String> {
        let path = self.path_for(operation_id)?;
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&path)
            .map_err(|error| format!("failed reading {}: {error}", path.display()))?;
        let record = serde_json::from_str(&raw)
            .map_err(|error| format!("invalid operation record {}: {error}", path.display()))?;
        Ok(Some(record))
    }

    pub fn save(&self, record: &OperationRecord) -> Result<(), String> {
        self.ensure()?;
        let path = self.path_for(&record.operation_id)?;
        let raw = serde_json::to_string_pretty(record)
            .map_err(|error| format!("failed encoding operation record: {error}"))?;
        atomic_write(&path, raw.as_bytes())
    }

    /// Begin or replay an operation with durable request fingerprinting.
    pub fn begin_or_replay(
        &self,
        operation_id: &str,
        method: &str,
        request: &Value,
    ) -> Result<BeginOutcome, String> {
        let fingerprint = request_fingerprint(request);
        if let Some(existing) = self.load(operation_id)? {
            if existing.request_sha256 != fingerprint || existing.method != method {
                return Err(format!(
                    "OperationConflict: operation_id={operation_id} already used for method={} fingerprint={}",
                    existing.method, existing.request_sha256
                ));
            }
            return match existing.state {
                OperationState::Completed => {
                    if let Some(result) = existing.result.clone() {
                        Ok(BeginOutcome::Replay { result })
                    } else {
                        Err(format!(
                            "completed operation {operation_id} is missing result payload"
                        ))
                    }
                }
                OperationState::Failed => Err(format!(
                    "operation {operation_id} previously failed: {}",
                    existing.error.unwrap_or_else(|| "unknown error".into())
                )),
                OperationState::Prepared
                | OperationState::Executing
                | OperationState::Indeterminate => Ok(BeginOutcome::Resume { record: existing }),
            };
        }

        let record = OperationRecord {
            operation_id: operation_id.to_string(),
            method: method.to_string(),
            request_sha256: fingerprint,
            state: OperationState::Prepared,
            result: None,
            error: None,
            created_at: Utc::now(),
            completed_at: None,
        };
        self.save(&record)?;
        Ok(BeginOutcome::Fresh { record })
    }

    pub fn mark_executing(&self, record: &mut OperationRecord) -> Result<(), String> {
        record.state = OperationState::Executing;
        record.error = None;
        self.save(record)
    }

    pub fn mark_completed(
        &self,
        record: &mut OperationRecord,
        result: Value,
    ) -> Result<(), String> {
        record.state = OperationState::Completed;
        record.result = Some(result);
        record.error = None;
        record.completed_at = Some(Utc::now());
        self.save(record)
    }

    pub fn mark_failed(
        &self,
        record: &mut OperationRecord,
        error: impl Into<String>,
    ) -> Result<(), String> {
        record.state = OperationState::Failed;
        record.error = Some(error.into());
        record.completed_at = Some(Utc::now());
        self.save(record)
    }

    pub fn mark_indeterminate(
        &self,
        record: &mut OperationRecord,
        error: impl Into<String>,
    ) -> Result<(), String> {
        record.state = OperationState::Indeterminate;
        record.error = Some(error.into());
        record.completed_at = Some(Utc::now());
        self.save(record)
    }
}

#[derive(Debug)]
pub enum BeginOutcome {
    Fresh { record: OperationRecord },
    Resume { record: OperationRecord },
    Replay { result: Value },
}

pub fn request_fingerprint(request: &Value) -> String {
    let canonical =
        serde_json::to_vec(request).unwrap_or_else(|_| request.to_string().into_bytes());
    let mut hasher = Sha256::new();
    hasher.update(canonical);
    hex::encode(hasher.finalize())
}

fn validate_operation_id(operation_id: &str) -> Result<(), String> {
    if operation_id.is_empty()
        || operation_id.len() > 128
        || operation_id.contains('/')
        || operation_id.contains('\\')
        || operation_id.contains("..")
        || !operation_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(format!("invalid operation_id: {operation_id}"));
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
    let write_result = (|| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .map_err(|error| format!("failed creating {}: {error}", tmp.display()))?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| format!("failed writing {}: {error}", tmp.display()))
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    fs::rename(&tmp, path).map_err(|error| {
        format!(
            "failed renaming {} -> {}: {error}",
            tmp.display(),
            path.display()
        )
    })?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("failed syncing {}: {error}", parent.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn same_id_same_fingerprint_replays_completed() {
        let temp = tempfile::tempdir().unwrap();
        let journal = OperationJournal::new(temp.path().to_path_buf());
        let request = json!({"image":"a","digest":null});
        let outcome = journal
            .begin_or_replay("op-1", "proxy.install", &request)
            .unwrap();
        let mut record = match outcome {
            BeginOutcome::Fresh { record } => record,
            _ => panic!("expected fresh"),
        };
        journal.mark_executing(&mut record).unwrap();
        journal
            .mark_completed(&mut record, json!({"ok": true}))
            .unwrap();

        match journal
            .begin_or_replay("op-1", "proxy.install", &request)
            .unwrap()
        {
            BeginOutcome::Replay { result } => assert_eq!(result, json!({"ok": true})),
            _ => panic!("expected replay"),
        }
    }

    #[test]
    fn same_id_different_fingerprint_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let journal = OperationJournal::new(temp.path().to_path_buf());
        let first = json!({"image":"a"});
        let second = json!({"image":"b"});
        let mut record = match journal
            .begin_or_replay("op-1", "proxy.install", &first)
            .unwrap()
        {
            BeginOutcome::Fresh { record } => record,
            _ => panic!("expected fresh"),
        };
        journal
            .mark_completed(&mut record, json!({"ok": true}))
            .unwrap();
        let err = journal
            .begin_or_replay("op-1", "proxy.install", &second)
            .unwrap_err();
        assert!(err.contains("OperationConflict"));
    }

    #[test]
    fn executing_record_is_resumed_after_process_interruption() {
        let temp = tempfile::tempdir().unwrap();
        let journal = OperationJournal::new(temp.path().to_path_buf());
        let request = json!({"image":"a"});
        let mut record = match journal
            .begin_or_replay("op-interrupted", "proxy.install", &request)
            .unwrap()
        {
            BeginOutcome::Fresh { record } => record,
            _ => panic!("expected fresh operation"),
        };
        journal.mark_executing(&mut record).unwrap();

        match journal
            .begin_or_replay("op-interrupted", "proxy.install", &request)
            .unwrap()
        {
            BeginOutcome::Resume { record } => {
                assert_eq!(record.state, OperationState::Executing)
            }
            _ => panic!("interrupted operation must resume"),
        }
    }
}
