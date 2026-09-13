//! Atomic remote-agent binary update and rollback.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentUpdateResult {
    pub installed_path: String,
    pub rollback_path: String,
    pub sha256: String,
    pub restart_required: bool,
}

pub fn install_current(staged: &Path, expected_sha256: &str) -> Result<AgentUpdateResult, String> {
    let target = std::env::current_exe()
        .map_err(|error| format!("resolve current agent executable: {error}"))?;
    install_binary(staged, &target, expected_sha256)
}

pub fn rollback_current() -> Result<AgentUpdateResult, String> {
    let target = std::env::current_exe()
        .map_err(|error| format!("resolve current agent executable: {error}"))?;
    rollback_binary(&target)
}

pub(crate) fn install_binary(
    staged: &Path,
    target: &Path,
    expected_sha256: &str,
) -> Result<AgentUpdateResult, String> {
    reject_symlink(staged)?;
    if !staged.is_file() {
        return Err(format!(
            "staged agent binary is not a file: {}",
            staged.display()
        ));
    }
    let payload = std::fs::metadata(staged)
        .map(|meta| meta.len())
        .unwrap_or(0);
    crate::space::gate_replace(
        crate::space::free_bytes_for_path(target.parent().unwrap_or(target)),
        payload,
    )?;
    let digest = sha256_file(staged)?;
    let expected = expected_sha256
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(expected_sha256.trim());
    if digest != expected {
        return Err(format!(
            "AgentDigestMismatch: expected {expected}, resolved {digest}"
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| "agent executable has no parent".to_string())?;
    let temporary = parent.join(format!(".vellum-agent-update-{}", ulid::Ulid::new()));
    copy_private_executable(staged, &temporary)?;
    let rollback = rollback_path(target);
    if rollback.exists() {
        fs::remove_file(&rollback)
            .map_err(|error| format!("remove previous rollback binary: {error}"))?;
    }
    fs::rename(target, &rollback)
        .map_err(|error| format!("stage current agent for rollback: {error}"))?;
    if let Err(error) = fs::rename(&temporary, target) {
        let rollback_error = fs::rename(&rollback, target).err();
        return Err(format!(
            "publish agent update failed: {error}; rollback={rollback_error:?}"
        ));
    }
    sync_parent(parent)?;
    Ok(AgentUpdateResult {
        installed_path: target.to_string_lossy().to_string(),
        rollback_path: rollback.to_string_lossy().to_string(),
        sha256: digest,
        restart_required: true,
    })
}

pub(crate) fn rollback_binary(target: &Path) -> Result<AgentUpdateResult, String> {
    let rollback = rollback_path(target);
    reject_symlink(&rollback)?;
    if !rollback.is_file() {
        return Err("AgentRollbackUnavailable: no rollback binary exists".into());
    }
    let failed = target.with_extension(format!("failed-{}", ulid::Ulid::new()));
    fs::rename(target, &failed).map_err(|error| format!("preserve failed agent: {error}"))?;
    if let Err(error) = fs::rename(&rollback, target) {
        let _ = fs::rename(&failed, target);
        return Err(format!("restore rollback agent failed: {error}"));
    }
    let _ = fs::remove_file(failed);
    if let Some(parent) = target.parent() {
        sync_parent(parent)?;
    }
    Ok(AgentUpdateResult {
        installed_path: target.to_string_lossy().to_string(),
        rollback_path: rollback.to_string_lossy().to_string(),
        sha256: sha256_file(target)?,
        restart_required: true,
    })
}

fn rollback_path(target: &Path) -> PathBuf {
    target.with_extension("vellum-rollback")
}

pub fn copy_private_executable(source: &Path, target: &Path) -> Result<(), String> {
    let mut input =
        fs::File::open(source).map_err(|error| format!("open staged agent: {error}"))?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o755);
    }
    let mut output = options
        .open(target)
        .map_err(|error| format!("create temporary agent: {error}"))?;
    std::io::copy(&mut input, &mut output)
        .and_then(|_| output.flush())
        .and_then(|_| output.sync_all())
        .map_err(|error| format!("copy staged agent: {error}"))?;
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file =
        fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn reject_symlink(path: &Path) -> Result<(), String> {
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(format!("refusing symlink agent path: {}", path.display()));
    }
    Ok(())
}

fn sync_parent(_path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    fs::File::open(_path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync agent directory: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_is_digest_pinned_and_rollback_restores_previous_binary() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("agent");
        let staged = temp.path().join("agent-new");
        fs::write(&target, b"old").unwrap();
        fs::write(&staged, b"new").unwrap();
        let digest = sha256_file(&staged).unwrap();
        assert!(install_binary(&staged, &target, "wrong").is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");

        let result = install_binary(&staged, &target, &digest).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert!(Path::new(&result.rollback_path).is_file());
        rollback_binary(&target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"old");
    }
}
