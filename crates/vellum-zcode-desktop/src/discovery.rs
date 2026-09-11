use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::protocol::{ZcodeArtifact, CONTROL_PROTOCOL_VERSION};
use crate::ZcodeDesktopError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TapListenRecord {
    pub schema_version: u32,
    pub protocol_version: u32,
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inner_pid: Option<u32>,
    pub pipe: String,
    pub artifact: ZcodeArtifact,
    pub started_at: String,
}

pub fn listen_path_for_pid(log_dir: &Path, pid: u32) -> PathBuf {
    log_dir.join(format!("tap-{pid}.listen.json"))
}

#[derive(Debug, Clone, Default)]
pub struct DiscoverFilter {
    pub require_cjs_sha256: Option<String>,
    pub require_live_pid: bool,
}

pub fn discover(log_dir: impl AsRef<Path>) -> Result<Vec<TapListenRecord>, ZcodeDesktopError> {
    discover_filtered(log_dir, DiscoverFilter::default())
}

pub fn discover_filtered(
    log_dir: impl AsRef<Path>,
    filter: DiscoverFilter,
) -> Result<Vec<TapListenRecord>, ZcodeDesktopError> {
    let log_dir = log_dir.as_ref();
    if !log_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in fs::read_dir(log_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("tap-") || !name.ends_with(".listen.json") {
            continue;
        }
        let raw = fs::read_to_string(entry.path())?;
        let record: TapListenRecord = serde_json::from_str(&raw)?;
        if record.protocol_version != CONTROL_PROTOCOL_VERSION {
            continue;
        }
        if let Some(expected) = filter.require_cjs_sha256.as_deref() {
            if record.artifact.cjs_sha256 != expected {
                continue;
            }
        }
        if filter.require_live_pid && !pid_is_alive(record.pid) {
            continue;
        }
        let expected_pid = name
            .trim_start_matches("tap-")
            .trim_end_matches(".listen.json")
            .parse::<u32>()
            .ok();
        if expected_pid.is_some_and(|pid| pid != record.pid) {
            continue;
        }
        records.push(record);
    }
    records.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    Ok(records)
}

pub fn newest(log_dir: impl AsRef<Path>) -> Result<Option<TapListenRecord>, ZcodeDesktopError> {
    Ok(discover(log_dir)?.into_iter().next())
}

pub fn require_newest(log_dir: impl AsRef<Path>) -> Result<TapListenRecord, ZcodeDesktopError> {
    newest(log_dir)?.ok_or(ZcodeDesktopError::TapUnavailable)
}

pub fn require_qualified(
    log_dir: impl AsRef<Path>,
    cjs_sha256: &str,
) -> Result<TapListenRecord, ZcodeDesktopError> {
    discover_filtered(
        log_dir,
        DiscoverFilter {
            require_cjs_sha256: Some(cjs_sha256.to_string()),
            require_live_pid: true,
        },
    )?
    .into_iter()
    .next()
    .ok_or(ZcodeDesktopError::TapUnavailable)
}

fn pid_is_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/NH", "/FI", &format!("PID eq {pid}")])
            .output()
            .map(|output| {
                let text = String::from_utf8_lossy(&output.stdout);
                text.contains(&pid.to_string())
            })
            .unwrap_or(false)
    }
    #[cfg(unix)]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
        false
    }
}
