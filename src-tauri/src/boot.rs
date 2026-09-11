//! Persistent process-boot telemetry.
//!
//! The desktop process can die without a graceful shutdown (for example the
//! old `panic = "abort"` web-search crashes). Keeping a small `boot.json` in
//! the data root lets the UI show "this process has restarted N times / last
//! started at ..." so restarts are visible instead of looking like the app
//! just silently dropped state (e.g. a vanished web-search ref store).

use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BootTelemetry {
    /// Total number of process starts observed, including this one.
    pub boot_count: u64,
    /// Unix seconds when the current process started.
    pub started_at: u64,
    /// Unix seconds when the previous process started, if any.
    pub previous_started_at: Option<u64>,
    /// OS process id of the current run.
    pub pid: u32,
}

pub fn boot_file(data_root: &Path) -> PathBuf {
    data_root.join("boot.json")
}

/// Record a process start and return the updated telemetry. Corrupt or
/// unreadable state is treated as a fresh first boot rather than an error:
/// telemetry must never block app startup.
pub fn record_boot(data_root: &Path) -> BootTelemetry {
    let previous = load(data_root).ok().flatten();
    let started_at = now_secs();
    let next = BootTelemetry {
        boot_count: previous.as_ref().map_or(0, |previous| previous.boot_count) + 1,
        started_at,
        previous_started_at: previous.map(|previous| previous.started_at),
        pid: std::process::id(),
    };
    let _ = write(data_root, &next);
    next
}

pub fn load(data_root: &Path) -> AppResult<Option<BootTelemetry>> {
    let path = boot_file(data_root);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(AppError::Message(format!("read boot telemetry: {error}")));
        }
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| AppError::Message(format!("parse boot telemetry: {error}")))
}

fn write(data_root: &Path, value: &BootTelemetry) -> AppResult<()> {
    let path = boot_file(data_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| AppError::Message(format!("create boot telemetry dir: {error}")))?;
    }
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| AppError::Message(format!("serialize boot telemetry: {error}")))?;
    let tmp = data_root.join("boot.json.tmp");
    std::fs::write(&tmp, &bytes)
        .map_err(|error| AppError::Message(format!("write boot telemetry: {error}")))?;
    std::fs::rename(&tmp, &path)
        .map_err(|error| AppError::Message(format!("commit boot telemetry: {error}")))?;
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_boot_increments_and_preserves_previous_start() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        let first = record_boot(root);
        assert_eq!(first.boot_count, 1);
        assert!(first.previous_started_at.is_none());
        assert_eq!(first.pid, std::process::id());

        let second = record_boot(root);
        assert_eq!(second.boot_count, 2);
        assert_eq!(second.previous_started_at, Some(first.started_at));

        assert_eq!(load(root).unwrap(), Some(second));
    }

    #[test]
    fn missing_or_corrupt_state_starts_fresh() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        assert_eq!(load(root).unwrap(), None);

        std::fs::write(boot_file(root), b"not json").unwrap();
        assert!(load(root).is_err());
        let boot = record_boot(root);
        assert_eq!(boot.boot_count, 1);
    }
}
