//! Resolve local Codex runtime identity before connecting to app-server.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::transport::{AppServerError, ServerIdentity};
use crate::config::BrokerConfig;

#[derive(Debug, Clone)]
pub struct ResolvedCodexRuntime {
    pub binary_path: PathBuf,
    pub binary_version: String,
    pub socket_path: PathBuf,
    pub codex_home: PathBuf,
}

impl ResolvedCodexRuntime {
    pub fn resolve(config: &BrokerConfig) -> Result<Self, AppServerError> {
        let binary_path = config.codex_binary.clone();
        if !binary_path.exists() {
            return Err(AppServerError::Transport(format!(
                "codex binary not found at {}",
                binary_path.display()
            )));
        }
        let binary_version = read_codex_version(&binary_path)?;
        Ok(Self {
            binary_path,
            binary_version,
            socket_path: config.app_server_socket.clone(),
            codex_home: config.codex_home.clone(),
        })
    }

    pub fn into_identity(self) -> ServerIdentity {
        ServerIdentity {
            name: "codex-app-server".into(),
            version: self.binary_version,
            platform: None,
            platform_family: None,
            platform_os: None,
            codex_home: Some(self.codex_home.display().to_string()),
        }
    }
}

pub fn read_codex_version(binary: &Path) -> Result<String, AppServerError> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|error| {
            AppServerError::Transport(format!(
                "failed to execute {} --version: {error}",
                binary.display()
            ))
        })?;
    if !output.status.success() {
        return Err(AppServerError::Transport(format!(
            "{} --version exited with {}",
            binary.display(),
            output.status
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if !stdout.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    parse_version_text(&text).ok_or_else(|| {
        AppServerError::Protocol(format!(
            "unable to parse codex version from output: {}",
            text.trim()
        ))
    })
}

pub fn parse_version_text(text: &str) -> Option<String> {
    // Accept forms like:
    // - codex-cli 0.146.1
    // - 0.146.1
    // - codex 0.146.1 (abc123)
    for token in text.split_whitespace() {
        let cleaned = token.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        if cleaned.split('.').count() >= 2
            && cleaned.chars().all(|c| c.is_ascii_digit() || c == '.')
            && cleaned.chars().any(|c| c.is_ascii_digit())
        {
            return Some(cleaned.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::parse_version_text;

    #[test]
    fn parses_codex_version_output() {
        assert_eq!(
            parse_version_text("codex-cli 0.146.1").as_deref(),
            Some("0.146.1")
        );
        assert_eq!(parse_version_text("0.146.1").as_deref(), Some("0.146.1"));
        assert_eq!(
            parse_version_text("codex 0.146.1 (deadbeef)").as_deref(),
            Some("0.146.1")
        );
    }
}
