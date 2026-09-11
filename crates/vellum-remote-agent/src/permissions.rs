//! Host filesystem permission policy for managed proxy mounts.
//!
//! Bind-mount deployments run the proxy container as the host agent effective
//! UID/GID so private host-owned config/secrets (0700/0600) remain readable
//! and writable data/history/logs remain usable without chown to a fixed
//! container identity. The image still defaults to USER 65532 for standalone
//! runs without bind mounts.

use std::path::Path;
#[cfg(unix)]
use std::process::Command;

use crate::state::AgentPaths;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedMounts {
    pub uid: u32,
    pub gid: u32,
    pub user: String,
}

/// Ensure managed directories exist with host-private ownership semantics.
///
/// - config/secrets: 0700 host-private (mounted :ro)
/// - data/history/logs: 0700 host-private writable mounts
/// - runtime user: host effective uid:gid for bind-mount deployments
pub fn prepare_proxy_mounts(paths: &AgentPaths) -> Result<PreparedMounts, String> {
    paths.ensure()?;

    let identity = host_runtime_identity()?;

    set_dir_mode(&paths.proxy_config_dir, 0o700)?;
    set_dir_mode(&paths.secrets_dir, 0o700)?;
    set_dir_mode(&paths.proxy_data_dir, 0o700)?;
    set_dir_mode(&paths.proxy_history_dir, 0o700)?;
    set_dir_mode(&paths.proxy_logs_dir, 0o700)?;
    set_dir_mode(&paths.official_accounts_dir, 0o700)?;
    set_dir_mode(&paths.logs_dir, 0o700)?;

    ensure_private_dir(&paths.proxy_config_dir)?;
    ensure_private_dir(&paths.secrets_dir)?;
    ensure_private_dir(&paths.proxy_data_dir)?;
    ensure_private_dir(&paths.proxy_history_dir)?;
    ensure_private_dir(&paths.proxy_logs_dir)?;
    ensure_private_dir(&paths.official_accounts_dir)?;

    Ok(PreparedMounts {
        uid: identity.0,
        gid: identity.1,
        user: format!("{}:{}", identity.0, identity.1),
    })
}

/// Host effective identity used for docker --user on bind-mounted hosts.
pub fn host_runtime_identity() -> Result<(u32, u32), String> {
    #[cfg(unix)]
    {
        let uid = parse_id_output(
            Command::new("id")
                .arg("-u")
                .output()
                .map_err(|e| e.to_string())?,
        )?;
        let gid = parse_id_output(
            Command::new("id")
                .arg("-g")
                .output()
                .map_err(|e| e.to_string())?,
        )?;
        Ok((uid, gid))
    }
    #[cfg(not(unix))]
    {
        // Windows only builds/tests the agent; production remote hosts are Linux.
        Ok((1000, 1000))
    }
}

#[cfg(unix)]
fn parse_id_output(output: std::process::Output) -> Result<u32, String> {
    if !output.status.success() {
        return Err(format!(
            "id failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|error| format!("invalid id output: {error}"))
}

fn set_dir_mode(path: &Path, mode: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(mode);
        fs::set_permissions(path, perms)
            .map_err(|error| format!("failed chmod {:o} on {}: {error}", mode, path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

fn ensure_private_dir(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::metadata(path)
            .map_err(|error| format!("failed stat {}: {error}", path.display()))?;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "{} must be private (0700); found mode {:o}",
                path.display(),
                mode
            ));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AgentPaths;
    use tempfile::tempdir;

    #[test]
    fn prepare_proxy_mounts_uses_host_identity_and_private_dirs() {
        let temp = tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        let prepared = prepare_proxy_mounts(&paths).unwrap();
        let (uid, gid) = host_runtime_identity().unwrap();
        assert_eq!(prepared.uid, uid);
        assert_eq!(prepared.gid, gid);
        assert_eq!(prepared.user, format!("{uid}:{gid}"));
        assert!(paths.proxy_data_dir.is_dir());
        assert!(paths.proxy_config_dir.is_dir());
        assert!(paths.secrets_dir.is_dir());

        #[cfg(unix)]
        {
            use std::fs;
            use std::os::unix::fs::PermissionsExt;
            for dir in [
                &paths.proxy_config_dir,
                &paths.secrets_dir,
                &paths.proxy_data_dir,
                &paths.proxy_history_dir,
                &paths.proxy_logs_dir,
                &paths.official_accounts_dir,
            ] {
                let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o700, "{} mode", dir.display());
            }
        }
    }
}
