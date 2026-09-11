//! Broker configuration.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BrokerConfig {
    pub broker_id: String,
    pub data_dir: PathBuf,
    pub listen_addr: SocketAddr,
    pub codex_binary: PathBuf,
    pub codex_home: PathBuf,
    pub app_server_socket: PathBuf,
    pub allowed_versions: Vec<String>,
    pub allowed_roots: Vec<PathBuf>,
    /// MVP: SSH tunnel + local_only is the supported path.
    /// `require_auth=true` currently only checks non-empty deviceToken and still
    /// blocks thread commands; pairing/hash verification is not complete.
    pub require_auth: bool,
    pub local_only: bool,
    pub writer_lease_ttl_secs: u64,
    pub writer_lease_heartbeat_secs: u64,
    pub writer_lease_disconnect_grace_secs: u64,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let share = home.join(".local").join("share").join("vellum");
        Self {
            broker_id: "vellum-remote-broker".into(),
            data_dir: share.join("remote"),
            listen_addr: "127.0.0.1:45100".parse().expect("valid loopback addr"),
            codex_binary: home
                .join(".codex")
                .join("packages")
                .join("standalone")
                .join("current")
                .join("codex"),
            codex_home: share.join("codex-home"),
            app_server_socket: share.join("run").join("codex-app-server.sock"),
            allowed_versions: vec!["0.146.1".into()],
            allowed_roots: vec![home.join("projects")],
            require_auth: false,
            local_only: true,
            writer_lease_ttl_secs: 30,
            writer_lease_heartbeat_secs: 10,
            writer_lease_disconnect_grace_secs: 15,
        }
    }
}

impl BrokerConfig {
    pub fn load_or_default(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        if path.exists() {
            let text = fs::read_to_string(path)?;
            let config: Self = toml::from_str(&text)?;
            return Ok(config);
        }
        let config = Self::default();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, toml::to_string_pretty(&config)?)?;
        Ok(config)
    }

    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.data_dir)?;
        fs::create_dir_all(&self.codex_home)?;
        if let Some(parent) = self.app_server_socket.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::create_dir_all(self.data_dir.join("diagnostics"))?;
        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("remote.sqlite3")
    }
}
