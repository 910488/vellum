//! Private, durable Docker-to-native rollback snapshot. Never included in an
//! RPC response or support archive; credential bytes stay under secrets_dir.

use crate::state::{AgentPaths, InstallRecord};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub install: InstallRecord,
    pub config: Vec<u8>,
    pub credentials: BTreeMap<String, Vec<u8>>,
    /// Written before each stop, so interruption between Docker stop and its
    /// reply can be recovered by re-observing the exact previously-live ID.
    pub stopped_ids: Vec<String>,
    #[serde(default)]
    pub rolled_back: bool,
}

fn path(paths: &AgentPaths) -> std::path::PathBuf {
    paths
        .secrets_dir
        .join("proxy-migration-rollback/snapshot.json")
}

pub(crate) fn load(paths: &AgentPaths) -> Result<Option<Snapshot>, String> {
    let path = path(paths);
    if let Some(parent) = path.parent() {
        crate::update::reject_symlink(parent)?;
    }
    crate::update::reject_symlink(&path)?;
    match std::fs::read(&path) {
        Ok(bytes) if bytes.len() <= MAX_SNAPSHOT_BYTES => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| "ProxyMigrationSnapshotInvalid".into()),
        Ok(_) => Err("ProxyMigrationSnapshotTooLarge".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("ProxyMigrationSnapshotReadFailed: {error}")),
    }
}

pub(crate) fn save(paths: &AgentPaths, snapshot: &Snapshot) -> Result<(), String> {
    let path = path(paths);
    let parent = path.parent().ok_or("ProxyMigrationSnapshotPathInvalid")?;
    crate::update::reject_symlink(parent)?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    let bytes = serde_json::to_vec(snapshot).map_err(|_| "ProxyMigrationSnapshotEncodeFailed")?;
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err("ProxyMigrationSnapshotTooLarge".into());
    }
    crate::configuration::atomic_write(&path, &bytes, 0o600)
}

pub(crate) fn clear(paths: &AgentPaths) -> Result<(), String> {
    if let Some(parent) = path(paths).parent() {
        crate::update::reject_symlink(parent)?;
    }
    match std::fs::remove_file(path(paths)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("ProxyMigrationSnapshotRemoveFailed: {error}")),
    }
}

fn private_bytes(path: &Path) -> Result<Vec<u8>, String> {
    crate::update::reject_symlink(path)?;
    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_SNAPSHOT_BYTES as u64 {
        return Err("ProxyMigrationSnapshotFileInvalid".into());
    }
    std::fs::read(path).map_err(|error| error.to_string())
}

pub(crate) fn capture(paths: &AgentPaths, install: InstallRecord) -> Result<Snapshot, String> {
    let config = private_bytes(&paths.proxy_config_dir.join("proxy.toml"))?;
    let mut credentials = BTreeMap::new();
    let mut size = config.len();
    for entry in std::fs::read_dir(&paths.secrets_dir).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            continue;
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "ProxyMigrationCredentialNameInvalid")?;
        if !credential_name(&name) {
            continue;
        }
        let bytes = private_bytes(&entry.path())?;
        size = size
            .checked_add(bytes.len())
            .ok_or("ProxyMigrationSnapshotTooLarge")?;
        if size > MAX_SNAPSHOT_BYTES {
            return Err("ProxyMigrationSnapshotTooLarge".into());
        }
        credentials.insert(name, bytes);
    }
    Ok(Snapshot {
        install,
        config,
        credentials,
        stopped_ids: Vec::new(),
        rolled_back: false,
    })
}

fn credential_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        && !matches!(name, "." | "..")
}

pub(crate) fn restore_files(paths: &AgentPaths, snapshot: &Snapshot) -> Result<(), String> {
    // Do not delete newly added or unknown files. The old config references
    // only its original credential IDs, which are restored byte-for-byte.
    for (name, bytes) in &snapshot.credentials {
        if !credential_name(name) {
            return Err("ProxyMigrationCredentialNameInvalid".into());
        }
        crate::update::reject_symlink(&paths.secrets_dir.join(name))?;
        crate::configuration::atomic_write(&paths.secrets_dir.join(name), bytes, 0o600)?;
    }
    crate::update::reject_symlink(&paths.proxy_config_dir.join("proxy.toml"))?;
    crate::configuration::atomic_write(
        &paths.proxy_config_dir.join("proxy.toml"),
        &snapshot.config,
        0o600,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restoration_rejects_path_traversal_without_writing_outside_secrets() {
        assert!(!credential_name("../outside.json"));
        assert!(!credential_name("..\\outside.json"));
        assert!(!credential_name("C:outside.json"));
        assert!(credential_name("provider-a.json"));
        assert!(credential_name("provider-credential"));
        assert!(credential_name("__vellum_proxy_boundary__"));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_parent_symlink_is_rejected_for_read_and_clear() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("agent"));
        paths.ensure().unwrap();
        let other = temp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&other, paths.secrets_dir.join("proxy-migration-rollback"))
            .unwrap();
        assert!(load(&paths).is_err());
        assert!(clear(&paths).is_err());
        assert!(std::fs::read_dir(other).unwrap().next().is_none());
    }
}
