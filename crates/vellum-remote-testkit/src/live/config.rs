//! Live smoke configuration and hard isolation guards.

use std::env;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Hard-blocked production paths. Live smoke must never touch these.
pub const PRODUCTION_CODEX_HOME: &str = "/home/vellum-test/.codex";
pub const PRODUCTION_APP_SERVER_CONTROL_SOCK: &str =
    "/home/vellum-test/.codex/app-server-control/app-server-control.sock";
pub const PRODUCTION_DESKTOP_SSH_WS_SOCK: &str =
    "/home/vellum-test/.codex/app-server-control/desktop-ssh-websocket-v0.sock";

/// Jetson native Codex binary (aarch64 musl). Shared executable only.
pub const DEFAULT_CODEX_NATIVE: &str = "/home/vellum-test/.local/lib/node_modules/@openai/codex/node_modules/@openai/codex-linux-arm64/vendor/aarch64-unknown-linux-musl/bin/codex";

/// Production auth/config sources that may be *copied* into isolated home.
pub const PRODUCTION_AUTH_JSON: &str = "/home/vellum-test/.codex/auth.json";
pub const PRODUCTION_CONFIG_TOML: &str = "/home/vellum-test/.codex/config.toml";

#[derive(Debug, Error)]
pub enum LiveSmokeError {
    #[error("{0}")]
    Message(String),
    #[error("isolation guard failed: {0}")]
    Isolation(String),
    #[error("ssh error: {0}")]
    Ssh(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("timeout: {0}")]
    Timeout(String),
}

#[derive(Debug, Clone)]
pub struct LiveSmokeConfig {
    pub ssh_host: String,
    pub remote_user_home: PathBuf,
    pub codex_native: PathBuf,
    pub production_codex_home: PathBuf,
    pub production_auth: PathBuf,
    pub production_config: PathBuf,
    pub remote_state_root: PathBuf,
    pub remote_runtime_root: PathBuf,
    pub local_artifact_root: PathBuf,
    pub allowed_versions: Vec<String>,
    pub broker_listen_port: u16,
    pub local_forward_port: u16,
}

impl LiveSmokeConfig {
    pub fn from_env() -> Result<Self, LiveSmokeError> {
        let enabled = env::var("VELLUM_LIVE_SMOKE")
            .map(|v| matches!(v.as_str(), "1" | "YES" | "yes" | "true" | "TRUE"))
            .unwrap_or(false);
        if !enabled {
            return Err(LiveSmokeError::Message(
                "set VELLUM_LIVE_SMOKE=YES to enable isolated Jetson live smoke".into(),
            ));
        }

        let ssh_host = env::var("VELLUM_LIVE_SSH").map_err(|_| {
            LiveSmokeError::Message(
                "VELLUM_LIVE_SSH is required (e.g. example-host or tester@192.0.2.10)".into(),
            )
        })?;

        let remote_user_home = PathBuf::from(
            env::var("VELLUM_LIVE_REMOTE_HOME").unwrap_or_else(|_| "/home/vellum-test".into()),
        );
        let codex_native = PathBuf::from(
            env::var("VELLUM_LIVE_CODEX_NATIVE").unwrap_or_else(|_| DEFAULT_CODEX_NATIVE.into()),
        );
        let production_codex_home = PathBuf::from(
            env::var("VELLUM_LIVE_PRODUCTION_CODEX_HOME")
                .unwrap_or_else(|_| PRODUCTION_CODEX_HOME.into()),
        );
        let production_auth = PathBuf::from(
            env::var("VELLUM_LIVE_PRODUCTION_AUTH").unwrap_or_else(|_| PRODUCTION_AUTH_JSON.into()),
        );
        let production_config = PathBuf::from(
            env::var("VELLUM_LIVE_PRODUCTION_CONFIG")
                .unwrap_or_else(|_| PRODUCTION_CONFIG_TOML.into()),
        );
        let remote_state_root =
            PathBuf::from(env::var("VELLUM_LIVE_STATE_ROOT").unwrap_or_else(|_| {
                remote_user_home
                    .join(".local/state/vellum-smoke")
                    .display()
                    .to_string()
            }));
        // Prefer caller override. Fixture will re-resolve via remote XDG_RUNTIME_DIR when possible.
        let remote_runtime_root = PathBuf::from(
            env::var("VELLUM_LIVE_RUNTIME_ROOT")
                .unwrap_or_else(|_| "/run/user/1000/vellum-smoke".into()),
        );
        let local_artifact_root = PathBuf::from(
            env::var("VELLUM_LIVE_ARTIFACT_ROOT").unwrap_or_else(|_| "target/live-smoke".into()),
        );
        let allowed_versions = env::var("VELLUM_LIVE_ALLOWED_VERSIONS")
            .unwrap_or_else(|_| "0.146.0".into())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        if allowed_versions.is_empty() {
            return Err(LiveSmokeError::Message(
                "VELLUM_LIVE_ALLOWED_VERSIONS resolved empty".into(),
            ));
        }

        let broker_listen_port = env::var("VELLUM_LIVE_BROKER_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(45110);
        let local_forward_port = env::var("VELLUM_LIVE_LOCAL_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(45110);

        let cfg = Self {
            ssh_host,
            remote_user_home,
            codex_native,
            production_codex_home,
            production_auth,
            production_config,
            remote_state_root,
            remote_runtime_root,
            local_artifact_root,
            allowed_versions,
            broker_listen_port,
            local_forward_port,
        };
        cfg.assert_isolation_sources()?;
        Ok(cfg)
    }

    pub fn assert_isolation_sources(&self) -> Result<(), LiveSmokeError> {
        // Source auth/config may live under production home; runtime paths must not.
        if self.codex_native.as_os_str().is_empty() {
            return Err(LiveSmokeError::Isolation(
                "codex_native path is empty".into(),
            ));
        }
        Ok(())
    }

    pub fn assert_smoke_paths(
        &self,
        smoke_codex_home: &Path,
        socket_path: &Path,
        workspace: &Path,
        broker_data_dir: &Path,
    ) -> Result<(), LiveSmokeError> {
        for (label, path) in [
            ("smoke_codex_home", smoke_codex_home),
            ("socket_path", socket_path),
            ("workspace", workspace),
            ("broker_data_dir", broker_data_dir),
        ] {
            if path.starts_with(&self.production_codex_home)
                || path.starts_with(PRODUCTION_CODEX_HOME)
            {
                return Err(LiveSmokeError::Isolation(format!(
                    "{label} must not be under production CODEX_HOME: {}",
                    path.display()
                )));
            }
        }

        let socket = socket_path.to_string_lossy();
        if socket.contains("app-server-control.sock")
            || socket.contains("desktop-ssh-websocket-v0.sock")
            || socket_path.starts_with(PRODUCTION_APP_SERVER_CONTROL_SOCK)
            || socket_path.starts_with(PRODUCTION_DESKTOP_SSH_WS_SOCK)
            || socket_path.starts_with(Path::new(PRODUCTION_CODEX_HOME).join("app-server-control"))
        {
            return Err(LiveSmokeError::Isolation(format!(
                "socket_path must not use production app-server sockets: {}",
                socket_path.display()
            )));
        }

        if smoke_codex_home == Path::new(PRODUCTION_CODEX_HOME)
            || smoke_codex_home == self.production_codex_home
        {
            return Err(LiveSmokeError::Isolation(
                "smoke CODEX_HOME equals production CODEX_HOME".into(),
            ));
        }
        Ok(())
    }
}
