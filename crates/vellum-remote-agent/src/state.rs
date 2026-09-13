use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::operations::OperationJournal;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InstallRecord {
    pub install_id: String,
    pub host_id: String,
    pub image: String,
    pub image_digest: Option<String>,
    pub config_hash: String,
    pub host_port: u16,
    pub updated_at: DateTime<Utc>,
    /// Absent on records written before protocol 4; those are Docker installs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_backend: Option<String>,
    /// SHA-256 of the native `vellum-proxy-daemon` binary. Never a fake image name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_executable_digest: Option<String>,
}

impl InstallRecord {
    pub fn backend(&self) -> crate::platform::ProxyBackend {
        crate::platform::ProxyBackend::parse(self.proxy_backend.as_deref())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentPersistedState {
    pub host_id: String,
    pub install: Option<InstallRecord>,
}

#[derive(Debug, Clone)]
pub struct AgentPaths {
    pub root: PathBuf,
    pub agent_dir: PathBuf,
    pub state_path: PathBuf,
    pub install_path: PathBuf,
    pub operations_dir: PathBuf,
    pub mutation_lock: PathBuf,
    pub proxy_config_dir: PathBuf,
    pub proxy_data_dir: PathBuf,
    pub proxy_history_dir: PathBuf,
    pub proxy_logs_dir: PathBuf,
    /// Writable, host-private OAuth state consumed only by the remote proxy.
    /// This is deliberately outside CODEX_HOME so execution-account changes
    /// cannot alter the Remote-control daemon identity.
    pub official_accounts_dir: PathBuf,
    pub secrets_dir: PathBuf,
    pub profiles_dir: PathBuf,
    pub leases_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub codex_accounts_dir: PathBuf,
}

impl AgentPaths {
    pub fn discover() -> Self {
        if let Ok(root) = std::env::var("VELLUM_REMOTE_STATE_ROOT") {
            let root = PathBuf::from(root);
            if !root.as_os_str().is_empty() {
                return Self::from_root(root);
            }
        }
        let root = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("vellum-remote");
        Self::from_root(root)
    }

    pub fn from_root(root: PathBuf) -> Self {
        let agent_dir = root.join("agent");
        Self {
            state_path: agent_dir.join("state.json"),
            install_path: agent_dir.join("install.json"),
            operations_dir: agent_dir.join("operations"),
            mutation_lock: agent_dir.join("mutation.lock"),
            proxy_config_dir: root.join("proxy").join("config"),
            proxy_data_dir: root.join("proxy").join("data"),
            proxy_history_dir: root.join("proxy").join("history"),
            proxy_logs_dir: root.join("proxy").join("logs"),
            official_accounts_dir: root.join("proxy").join("data").join("official-auth"),
            secrets_dir: root.join("secrets"),
            profiles_dir: root.join("profiles"),
            leases_dir: root.join("leases"),
            logs_dir: root.join("logs"),
            codex_accounts_dir: root.join("codex-accounts"),
            agent_dir,
            root,
        }
    }

    pub fn ensure(&self) -> Result<(), String> {
        for dir in [
            &self.root,
            &self.agent_dir,
            &self.operations_dir,
            &self.proxy_config_dir,
            &self.proxy_data_dir,
            &self.proxy_history_dir,
            &self.proxy_logs_dir,
            &self.official_accounts_dir,
            &self.secrets_dir,
            &self.profiles_dir,
            &self.leases_dir,
            &self.logs_dir,
            &self.codex_accounts_dir,
        ] {
            fs::create_dir_all(dir)
                .map_err(|error| format!("failed creating {}: {error}", dir.display()))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct AgentStateStore {
    paths: AgentPaths,
}

impl AgentStateStore {
    pub fn new(paths: AgentPaths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &AgentPaths {
        &self.paths
    }

    pub fn operations(&self) -> OperationJournal {
        OperationJournal::new(self.paths.operations_dir.clone())
    }

    pub fn load(&self) -> Result<AgentPersistedState, String> {
        if !self.paths.state_path.exists() {
            return Ok(AgentPersistedState {
                host_id: default_host_id(&self.paths.root),
                ..AgentPersistedState::default()
            });
        }
        let raw = fs::read_to_string(&self.paths.state_path)
            .map_err(|error| format!("failed reading agent state: {error}"))?;
        // Tolerate legacy fields (last_operation_*) by deserializing into a
        // value first and only keeping the current schema keys.
        let mut value: serde_json::Value =
            serde_json::from_str(&raw).map_err(|error| format!("invalid agent state: {error}"))?;
        if let Some(obj) = value.as_object_mut() {
            obj.remove("lastOperationId");
            obj.remove("lastOperationResult");
            obj.remove("last_operation_id");
            obj.remove("last_operation_result");
        }
        serde_json::from_value(value).map_err(|error| format!("invalid agent state: {error}"))
    }

    pub fn save(&self, state: &AgentPersistedState) -> Result<(), String> {
        self.paths.ensure()?;
        let raw = serde_json::to_string_pretty(state)
            .map_err(|error| format!("failed encoding agent state: {error}"))?;
        atomic_write(&self.paths.state_path, raw.as_bytes())?;
        if let Some(install) = &state.install {
            let install_raw = serde_json::to_string_pretty(install)
                .map_err(|error| format!("failed encoding install record: {error}"))?;
            atomic_write(&self.paths.install_path, install_raw.as_bytes())?;
        }
        Ok(())
    }
}

fn default_host_id(root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(root.to_string_lossy().as_bytes());
    format!("host-{}", &hex::encode(hasher.finalize())[..12])
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(|error| format!("failed writing {}: {error}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|error| {
        format!(
            "failed renaming {} -> {}: {error}",
            tmp.display(),
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmarked_install_records_default_to_docker_and_never_invent_a_native_image() {
        let raw = r#"{
            "installId": "install-1",
            "hostId": "host-1",
            "image": "vellum-proxy:0.2.2",
            "imageDigest": "sha256:abc",
            "configHash": "cfg",
            "hostPort": 15721,
            "updatedAt": "2026-01-01T00:00:00Z"
        }"#;
        let record: InstallRecord = serde_json::from_str(raw).unwrap();
        assert_eq!(record.backend(), crate::platform::ProxyBackend::Docker);
        assert!(record.native_executable_digest.is_none());
        assert_eq!(record.image, "vellum-proxy:0.2.2");

        let native = InstallRecord {
            install_id: "install-2".into(),
            host_id: "host-1".into(),
            image: String::new(),
            image_digest: None,
            config_hash: "cfg".into(),
            host_port: 15722,
            updated_at: Utc::now(),
            proxy_backend: Some("native".into()),
            native_executable_digest: Some("a".repeat(64)),
        };
        let encoded = serde_json::to_value(&native).unwrap();
        assert_eq!(encoded["proxyBackend"], "native");
        assert_eq!(encoded["image"], "");
        assert_eq!(encoded["nativeExecutableDigest"], "a".repeat(64));
        assert_ne!(encoded["image"], "vellum-proxy:native");
        assert_eq!(native.backend(), crate::platform::ProxyBackend::Native);
    }
}
