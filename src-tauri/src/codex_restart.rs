//! Persist only the contract a particular Desktop process could have loaded.
//! Never persist the config itself (its provider headers contain secrets).
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ProxyContract {
    config_digest: String,
    catalog: serde_json::Value,
}

impl ProxyContract {
    pub(crate) fn from_bytes(config: &str, catalog: &[u8]) -> Option<Self> {
        let config: toml::Value = toml::from_str(config).ok()?;
        let mut managed = serde_json::Map::new();
        for key in [
            "model_provider",
            "model_catalog_json",
            "openai_base_url",
            "model_context_window",
            "model_auto_compact_token_limit",
            "features",
            "agents",
        ] {
            if let Some(value) = config.get(key) {
                managed.insert(key.into(), serde_json::to_value(value).ok()?);
            }
        }
        for id in ["vellum", "vellum-official"] {
            if let Some(value) = config
                .get("model_providers")
                .and_then(|providers| providers.get(id))
            {
                managed.insert(format!("provider:{id}"), serde_json::to_value(value).ok()?);
            }
        }
        use sha2::{Digest, Sha256};
        let config_digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&managed).ok()?));
        let catalog: serde_json::Value = serde_json::from_slice(catalog).ok()?;
        catalog.get("models")?.as_array()?;
        Some(Self {
            config_digest,
            catalog,
        })
    }

    pub(crate) fn read(paths: &crate::codex::CodexPaths) -> Option<Self> {
        Self::from_bytes(
            &std::fs::read_to_string(&paths.config).ok()?,
            &std::fs::read(&paths.catalog).ok()?,
        )
    }

    pub(crate) fn observed_before_start(
        paths: &crate::codex::CodexPaths,
        root: &Path,
        started: Option<std::time::SystemTime>,
    ) -> Option<(Self, bool)> {
        let started = started?;
        let before_start = |path: &Path| {
            std::fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| modified <= started)
        };
        let config_loaded = before_start(&paths.config);
        let config = std::fs::read_to_string(&paths.config).ok()?;
        if before_start(&paths.catalog) {
            return Self::read(paths).map(|contract| (contract, config_loaded));
        }
        // A later fetched_at does not prove a later model contract. Use the
        // most recent retained catalog that existed at process startup.
        let mut versions = crate::runtime::list_catalog_versions(root).ok()?;
        versions.sort_by_key(|version| std::cmp::Reverse(version.created_at));
        versions.into_iter().find_map(|version| {
            let path = Path::new(&version.path);
            before_start(path)
                .then(|| {
                    std::fs::read(path)
                        .ok()
                        .and_then(|bytes| Self::from_bytes(&config, &bytes))
                        .map(|contract| (contract, config_loaded))
                })
                .flatten()
        })
    }

    pub(crate) fn matches(&self, other: &Self) -> bool {
        self.config_digest == other.config_digest
            && !crate::runtime::catalog_requires_restart(
                &serde_json::to_vec(&self.catalog).expect("catalog JSON"),
                &serde_json::to_vec(&other.catalog).expect("catalog JSON"),
            )
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ProcessBaseline {
    pub process_identity: String,
    pub contract: Option<ProxyContract>,
    pub contract_loaded: bool,
}

impl ProcessBaseline {
    pub(crate) fn capture(
        root: &Path,
        identity: String,
        before: Option<ProxyContract>,
        contract_loaded: bool,
    ) -> Self {
        let path = root.join("codex-process-contract.json");
        if let Some(previous) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
        {
            if previous.process_identity == identity {
                return previous;
            }
        }
        // Unknown first-takeover state stays unknown across a Vellum restart;
        // the newly written config is not evidence the old process loaded it.
        let baseline = Self {
            process_identity: identity,
            contract: before,
            contract_loaded,
        };
        baseline.persist(root);
        baseline
    }

    pub(crate) fn persist(&self, root: &Path) {
        if let Err(error) = crate::updates::write_atomic(
            &root.join("codex-process-contract.json"),
            &serde_json::to_vec(self).expect("contract JSON"),
        ) {
            log::warn!("[Restart] cannot persist process contract: {error}");
        }
    }

    pub(crate) fn matches(
        &self,
        identity: &str,
        current: Option<&ProxyContract>,
        successful_request: bool,
    ) -> bool {
        self.process_identity == identity
            && (self.contract_loaded || successful_request)
            && self
                .contract
                .as_ref()
                .zip(current)
                .is_some_and(|(before, after)| before.matches(after))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn contract(port: u16, fetched: &str, context: u64) -> ProxyContract {
        ProxyContract::from_bytes(&format!("model_provider = 'vellum'\nmodel_catalog_json = 'catalog.json'\n[model_providers.vellum]\nbase_url = 'http://127.0.0.1:{port}/v1'\n[model_providers.vellum.http_headers]\nx-vellum-boundary-key = 'synthetic-private-value'\n"), &serde_json::to_vec(&serde_json::json!({"fetched_at":fetched,"models":[{"slug":"sol","context_window":context}]})).unwrap()).unwrap()
    }
    #[test]
    fn loaded_contract_ignores_timestamp_but_detects_port_and_model_changes() {
        let dir = tempfile::tempdir().unwrap();
        let baseline = ProcessBaseline::capture(
            dir.path(),
            "pid:1;started:100".into(),
            Some(contract(15721, "old", 128000)),
            true,
        );
        assert!(baseline.matches(
            "pid:1;started:100",
            Some(&contract(15721, "new", 128000)),
            false
        ));
        assert!(!baseline.matches(
            "pid:1;started:100",
            Some(&contract(15722, "new", 128000)),
            true
        ));
        assert!(!baseline.matches(
            "pid:1;started:100",
            Some(&contract(15721, "new", 256000)),
            true
        ));
        assert!(!baseline.matches(
            "pid:1;started:200",
            Some(&contract(15721, "new", 128000)),
            true
        ));
        assert!(
            !std::fs::read_to_string(dir.path().join("codex-process-contract.json"))
                .unwrap()
                .contains("synthetic-private-value")
        );
    }
    #[test]
    fn first_takeover_unknown_baseline_is_not_replaced_by_the_written_config() {
        let dir = tempfile::tempdir().unwrap();
        ProcessBaseline::capture(dir.path(), "pid:1".into(), None, false);
        let baseline = ProcessBaseline::capture(
            dir.path(),
            "pid:1".into(),
            Some(contract(15721, "new", 128000)),
            true,
        );
        assert!(!baseline.matches("pid:1", Some(&contract(15721, "new", 128000)), true));
        let new_process = ProcessBaseline::capture(
            dir.path(),
            "pid:2".into(),
            Some(contract(15721, "new", 128000)),
            true,
        );
        assert!(new_process.matches("pid:2", Some(&contract(15721, "new", 128000)), false));
    }
    #[test]
    fn successful_request_can_confirm_an_unchanged_but_recently_written_contract() {
        let dir = tempfile::tempdir().unwrap();
        let baseline = ProcessBaseline::capture(
            dir.path(),
            "pid:1".into(),
            Some(contract(15721, "old", 128000)),
            false,
        );
        assert!(!baseline.matches("pid:1", Some(&contract(15721, "new", 128000)), false));
        assert!(baseline.matches("pid:1", Some(&contract(15721, "new", 128000)), true));
    }

    #[test]
    fn recent_catalog_refresh_uses_the_contract_that_predates_process_start() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::codex::CodexPaths {
            config: dir.path().join("config.toml"),
            auth: dir.path().join("unused-auth"),
            models_cache: dir.path().join("unused-cache"),
            catalog: dir.path().join("catalog.json"),
            lease: dir.path().join("unused-lease"),
        };
        std::fs::write(&paths.config, "model_provider='vellum'").unwrap();
        let old = br#"{"fetched_at":"old","models":[{"slug":"sol"}]}"#;
        std::fs::write(&paths.catalog, old).unwrap();
        let history = crate::runtime::snapshot_catalog(dir.path(), &paths.catalog)
            .unwrap()
            .unwrap();
        let at = |seconds| std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds);
        for path in [&paths.config, &paths.catalog, Path::new(&history.path)] {
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(at(100))
                .unwrap();
        }
        std::fs::write(
            &paths.catalog,
            br#"{"fetched_at":"new","models":[{"slug":"sol"}]}"#,
        )
        .unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&paths.catalog)
            .unwrap()
            .set_modified(at(300))
            .unwrap();
        let (previous, loaded) =
            ProxyContract::observed_before_start(&paths, dir.path(), Some(at(200))).unwrap();
        assert!(loaded);
        assert!(previous.matches(&ProxyContract::read(&paths).unwrap()));
        std::fs::write(&paths.catalog, br#"{"models":[{"slug":"changed"}]}"#).unwrap();
        assert!(!previous.matches(&ProxyContract::read(&paths).unwrap()));
        std::fs::OpenOptions::new()
            .write(true)
            .open(&paths.config)
            .unwrap()
            .set_modified(at(300))
            .unwrap();
        let (_, loaded) =
            ProxyContract::observed_before_start(&paths, dir.path(), Some(at(200))).unwrap();
        assert!(
            !loaded,
            "recent config requires a successful unchanged request"
        );
    }
}
