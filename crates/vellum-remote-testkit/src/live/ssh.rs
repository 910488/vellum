//! Thin SSH/SCP helpers for driving Jetson live smoke from a Windows host.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};

use super::config::LiveSmokeError;

#[derive(Debug)]
pub struct SshTunnelGuard {
    child: Child,
    #[allow(dead_code)]
    pub local_port: u16,
    #[allow(dead_code)]
    pub remote_port: u16,
}

impl SshTunnelGuard {
    pub async fn open(
        ssh_host: &str,
        local_port: u16,
        remote_port: u16,
    ) -> Result<Self, LiveSmokeError> {
        let mut child = Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-N",
                "-L",
                &format!("{local_port}:127.0.0.1:{remote_port}"),
                ssh_host,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| LiveSmokeError::Ssh(format!("failed to spawn ssh tunnel: {error}")))?;

        // Give the tunnel a moment to bind.
        tokio::time::sleep(Duration::from_millis(400)).await;
        if let Some(status) = child.try_wait().map_err(LiveSmokeError::Io)? {
            let stderr = if let Some(mut err) = child.stderr.take() {
                use tokio::io::AsyncReadExt;
                let mut buf = String::new();
                let _ = err.read_to_string(&mut buf).await;
                buf
            } else {
                String::new()
            };
            return Err(LiveSmokeError::Ssh(format!(
                "ssh tunnel exited early with {status}: {stderr}"
            )));
        }

        Ok(Self {
            child,
            local_port,
            remote_port,
        })
    }
}

impl Drop for SshTunnelGuard {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

pub async fn scp_to_remote(
    local: &Path,
    ssh_host: &str,
    remote_path: &str,
) -> Result<(), LiveSmokeError> {
    let target = format!("{ssh_host}:{remote_path}");
    let output = Command::new("scp")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            &local.display().to_string(),
            &target,
        ])
        .output()
        .await
        .map_err(|error| LiveSmokeError::Ssh(format!("scp failed: {error}")))?;
    if !output.status.success() {
        return Err(LiveSmokeError::Ssh(format!(
            "scp failed ({status}): stdout={stdout} stderr={stderr}",
            status = output.status,
            stdout = String::from_utf8_lossy(&output.stdout),
            stderr = String::from_utf8_lossy(&output.stderr),
        )));
    }
    Ok(())
}

pub async fn scp_from_remote(
    ssh_host: &str,
    remote_path: &str,
    local: &Path,
) -> Result<(), LiveSmokeError> {
    if let Some(parent) = local.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let source = format!("{ssh_host}:{remote_path}");
    let output = Command::new("scp")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            &source,
            &local.display().to_string(),
        ])
        .output()
        .await
        .map_err(|error| LiveSmokeError::Ssh(format!("scp pull failed: {error}")))?;
    if !output.status.success() {
        return Err(LiveSmokeError::Ssh(format!(
            "scp pull failed ({status}): stdout={stdout} stderr={stderr}",
            status = output.status,
            stdout = String::from_utf8_lossy(&output.stdout),
            stderr = String::from_utf8_lossy(&output.stderr),
        )));
    }
    Ok(())
}

/// Run a remote bash script via stdin (`bash -s`).
pub async fn ssh_script(ssh_host: &str, script: &str) -> Result<String, LiveSmokeError> {
    use tokio::io::AsyncWriteExt;
    let mut child = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=15",
            ssh_host,
            "bash -s",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| LiveSmokeError::Ssh(format!("ssh script spawn failed: {error}")))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| LiveSmokeError::Ssh("missing ssh stdin".into()))?;
        stdin
            .write_all(script.as_bytes())
            .await
            .map_err(|error| LiveSmokeError::Ssh(format!("ssh stdin write failed: {error}")))?;
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|error| LiveSmokeError::Ssh(format!("ssh script wait failed: {error}")))?;
    if !output.status.success() {
        return Err(LiveSmokeError::Ssh(format!(
            "ssh script failed ({status}): stdout={stdout} stderr={stderr}",
            status = output.status,
            stdout = String::from_utf8_lossy(&output.stdout),
            stderr = String::from_utf8_lossy(&output.stderr),
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}
