use super::{
    ExecutionPlane, RuntimeInstall, RuntimeRegistry, RuntimeRegistryError, ThreadRuntimeBinding,
    BINDING_VERSION,
};

/// Trusted provider identities. Routing uses provider_id, never a model name.
#[derive(Debug, Clone, Default)]
pub struct TrustedProviderSet {
    official: Vec<String>,
    third_party: Vec<String>,
}

impl TrustedProviderSet {
    pub fn new(official: Vec<String>, third_party: Vec<String>) -> Self {
        Self {
            official,
            third_party,
        }
    }

    pub fn classify(&self, provider_id: &str) -> Result<ProviderClass, RouterError> {
        if provider_id.trim().is_empty() {
            return Err(RouterError::UntrustedProvider {
                provider_id: provider_id.to_string(),
            });
        }
        if provider_id == "openai"
            || provider_id == "openai-official"
            || self.official.iter().any(|id| id == provider_id)
        {
            return Ok(ProviderClass::Official);
        }
        if self.third_party.iter().any(|id| id == provider_id) {
            return Ok(ProviderClass::TrustedThirdParty);
        }
        Err(RouterError::UntrustedProvider {
            provider_id: provider_id.to_string(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderClass {
    Official,
    TrustedThirdParty,
}

#[derive(Debug, Clone)]
pub struct RuntimeRouter {
    pub providers: TrustedProviderSet,
}

impl RuntimeRouter {
    pub fn new(providers: TrustedProviderSet) -> Self {
        Self { providers }
    }

    pub fn plane_for_provider(&self, provider_id: &str) -> Result<ExecutionPlane, RouterError> {
        match self.providers.classify(provider_id)? {
            ProviderClass::Official => Ok(ExecutionPlane::OfficialCodex),
            ProviderClass::TrustedThirdParty => Ok(ExecutionPlane::EnhancedCodex),
        }
    }

    pub fn bind_new_thread(
        &self,
        registry: &RuntimeRegistry,
        thread_id: &str,
        provider_id: &str,
        model_id: &str,
        native_thread_id: &str,
        created_at: i64,
    ) -> Result<ThreadRuntimeBinding, RouterError> {
        let plane = self.plane_for_provider(provider_id)?;
        let install = registry.require(plane)?;
        Ok(ThreadRuntimeBinding::new(
            thread_id,
            plane,
            install.digest.clone(),
            provider_id,
            model_id,
            native_thread_id,
            created_at,
        ))
    }

    pub fn resume<'a>(
        &self,
        registry: &'a RuntimeRegistry,
        binding: &ThreadRuntimeBinding,
    ) -> Result<&'a RuntimeInstall, RouterError> {
        if binding.binding_version != BINDING_VERSION {
            return Err(RouterError::UnsupportedBindingVersion(
                binding.binding_version,
            ));
        }
        Ok(registry.require_digest(binding.plane, &binding.runtime_digest)?)
    }

    pub fn same_runtime_for_control_plane(
        &self,
        binding: &ThreadRuntimeBinding,
        plane: ExecutionPlane,
        digest: &str,
    ) -> Result<(), RouterError> {
        if binding.plane != plane || binding.runtime_digest != digest {
            return Err(RouterError::PlaneSwitchForbidden {
                thread_id: binding.thread_id.clone(),
                bound: binding.plane,
                requested: plane,
            });
        }
        Ok(())
    }

    pub fn reject_plane_switch(
        existing: &ThreadRuntimeBinding,
        requested: ExecutionPlane,
    ) -> Result<(), RouterError> {
        if existing.plane == requested {
            Ok(())
        } else {
            Err(RouterError::PlaneSwitchForbidden {
                thread_id: existing.thread_id.clone(),
                bound: existing.plane,
                requested,
            })
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RouterError {
    #[error("provider {provider_id} is not in the trusted provider set")]
    UntrustedProvider { provider_id: String },
    #[error("thread {thread_id} is bound to {bound:?}; switching to {requested:?} requires a new thread")]
    PlaneSwitchForbidden {
        thread_id: String,
        bound: ExecutionPlane,
        requested: ExecutionPlane,
    },
    #[error("unsupported thread runtime binding version {0}")]
    UnsupportedBindingVersion(u16),
    #[error(transparent)]
    Registry(#[from] RuntimeRegistryError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enhanced_runtime::{EnhancedRuntimeManifest, RuntimeInstall, FEATURE_DEFAULTS_MVP};
    use std::path::PathBuf;
    use vellum_enhanced_codex::EnhancedRuntimeLockFile;

    fn complete_lock() -> EnhancedRuntimeLockFile {
        let mut lock = EnhancedRuntimeLockFile::mvp_pins();
        lock.enhanced_codex_commit = Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into());
        lock.artifact_sha256 =
            Some("sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into());
        lock.target_triple = Some("x86_64-pc-windows-msvc".into());
        lock
    }

    fn registry() -> RuntimeRegistry {
        let lock = complete_lock();
        let manifest = EnhancedRuntimeManifest::from_lock(
            &lock,
            FEATURE_DEFAULTS_MVP,
            "x86_64-pc-windows-msvc",
        )
        .unwrap();
        let digest = manifest.runtime_digest().unwrap();
        let mut registry = RuntimeRegistry::new();
        registry
            .register(RuntimeInstall {
                plane: ExecutionPlane::OfficialCodex,
                executable: PathBuf::from("C:/codex/official.exe"),
                digest: "official-digest".into(),
                artifact_sha256: None,
                manifest: None,
            })
            .unwrap();
        registry
            .register(RuntimeInstall {
                plane: ExecutionPlane::EnhancedCodex,
                executable: PathBuf::from("C:/codex/enhanced.exe"),
                digest: digest.clone(),
                artifact_sha256: lock.artifact_sha256.clone(),
                manifest: Some(manifest),
            })
            .unwrap();
        registry
    }

    fn router() -> RuntimeRouter {
        RuntimeRouter::new(TrustedProviderSet::new(
            vec!["openai-official".into()],
            vec!["qwen".into(), "deepseek".into(), "grok".into()],
        ))
    }

    #[test]
    fn routes_by_trusted_provider_id_not_model_name() {
        let router = router();
        assert_eq!(
            router.plane_for_provider("openai-official").unwrap(),
            ExecutionPlane::OfficialCodex
        );
        assert_eq!(
            router.plane_for_provider("qwen").unwrap(),
            ExecutionPlane::EnhancedCodex
        );
        assert!(router.plane_for_provider("gpt-5.4").is_err());
        assert!(router.plane_for_provider("qwen3-coder").is_err());
    }

    #[test]
    fn resume_keeps_runtime_digest_and_forbids_fallback() {
        let router = router();
        let registry = registry();
        let binding = router
            .bind_new_thread(&registry, "t1", "qwen", "qwen3-coder", "native-1", 1)
            .unwrap();
        assert_eq!(binding.plane, ExecutionPlane::EnhancedCodex);
        let resumed = router.resume(&registry, &binding).unwrap();
        assert_eq!(resumed.digest, binding.runtime_digest);
        assert!(
            RuntimeRouter::reject_plane_switch(&binding, ExecutionPlane::OfficialCodex).is_err()
        );
    }

    #[test]
    fn missing_enhanced_runtime_does_not_fall_back() {
        let router = router();
        let mut registry = RuntimeRegistry::new();
        registry
            .register(RuntimeInstall {
                plane: ExecutionPlane::OfficialCodex,
                executable: PathBuf::from("C:/codex/official.exe"),
                digest: "official-digest".into(),
                artifact_sha256: None,
                manifest: None,
            })
            .unwrap();
        let error = router
            .bind_new_thread(&registry, "t1", "qwen", "qwen3-coder", "native-1", 1)
            .unwrap_err();
        assert!(matches!(
            error,
            RouterError::Registry(RuntimeRegistryError::RuntimeMissing {
                plane: ExecutionPlane::EnhancedCodex
            })
        ));
    }
}
