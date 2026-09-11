use std::collections::HashMap;
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use vellum_enhanced_codex::EnhancedRuntimeLockFile;

use super::{EnhancedRuntimeManifest, ExecutionPlane, ManifestError, RuntimeKind};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RuntimeKey {
    pub plane: ExecutionPlane,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInstall {
    pub plane: ExecutionPlane,
    pub executable: PathBuf,
    pub digest: String,
    pub artifact_sha256: Option<String>,
    pub manifest: Option<EnhancedRuntimeManifest>,
}

#[derive(Debug, Default)]
pub struct RuntimeRegistry {
    installs: HashMap<RuntimeKey, RuntimeInstall>,
    defaults: HashMap<ExecutionPlane, String>,
}

impl RuntimeRegistry {
    pub fn new() -> Self {
        Self {
            installs: HashMap::new(),
            defaults: HashMap::new(),
        }
    }

    pub fn register(&mut self, install: RuntimeInstall) -> Result<(), RuntimeRegistryError> {
        self.register_with_default(install, false)
    }

    pub fn register_default(
        &mut self,
        install: RuntimeInstall,
    ) -> Result<(), RuntimeRegistryError> {
        self.register_with_default(install, true)
    }

    pub fn register_verified(
        &mut self,
        install: RuntimeInstall,
        lock: &EnhancedRuntimeLockFile,
        promote: bool,
    ) -> Result<(), RuntimeRegistryError> {
        let Some(manifest) = install.manifest.as_ref() else {
            return Err(RuntimeRegistryError::MissingManifest);
        };
        manifest.verify_against_lock(lock)?;
        self.register_with_default(install, promote)
    }

    fn register_with_default(
        &mut self,
        install: RuntimeInstall,
        promote: bool,
    ) -> Result<(), RuntimeRegistryError> {
        if install.plane == ExecutionPlane::EnhancedCodex {
            let Some(manifest) = install.manifest.as_ref() else {
                return Err(RuntimeRegistryError::MissingManifest);
            };
            if manifest.runtime_kind != RuntimeKind::EnhancedCodex {
                return Err(RuntimeRegistryError::KindMismatch);
            }
            if manifest.runtime_digest()? != install.digest {
                return Err(RuntimeRegistryError::DigestMismatch);
            }
            if let Some(artifact) = install.artifact_sha256.as_deref() {
                if artifact != manifest.artifact_sha256 {
                    return Err(RuntimeRegistryError::ArtifactMismatch {
                        expected: manifest.artifact_sha256.clone(),
                        actual: artifact.to_string(),
                    });
                }
            }
            if install.executable.exists() {
                let hashed = sha256_file(&install.executable)?;
                let expected = manifest
                    .artifact_sha256
                    .strip_prefix("sha256:")
                    .unwrap_or(&manifest.artifact_sha256);
                if hashed != expected {
                    return Err(RuntimeRegistryError::ArtifactMismatch {
                        expected: manifest.artifact_sha256.clone(),
                        actual: format!("sha256:{hashed}"),
                    });
                }
            }
        }
        let key = RuntimeKey {
            plane: install.plane,
            digest: install.digest.clone(),
        };
        let plane = install.plane;
        let digest = install.digest.clone();
        self.installs.insert(key, install);
        if promote || !self.defaults.contains_key(&plane) {
            self.defaults.insert(plane, digest);
        }
        Ok(())
    }

    pub fn set_default(
        &mut self,
        plane: ExecutionPlane,
        digest: &str,
    ) -> Result<(), RuntimeRegistryError> {
        self.require_digest(plane, digest)?;
        self.defaults.insert(plane, digest.to_string());
        Ok(())
    }

    pub fn get(&self, plane: ExecutionPlane) -> Option<&RuntimeInstall> {
        let digest = self.defaults.get(&plane)?;
        self.installs.get(&RuntimeKey {
            plane,
            digest: digest.clone(),
        })
    }

    pub fn require(&self, plane: ExecutionPlane) -> Result<&RuntimeInstall, RuntimeRegistryError> {
        self.get(plane)
            .ok_or(RuntimeRegistryError::RuntimeMissing { plane })
    }

    pub fn require_digest(
        &self,
        plane: ExecutionPlane,
        digest: &str,
    ) -> Result<&RuntimeInstall, RuntimeRegistryError> {
        self.installs
            .get(&RuntimeKey {
                plane,
                digest: digest.to_string(),
            })
            .ok_or_else(|| RuntimeRegistryError::BoundRuntimeUnavailable {
                plane,
                expected: digest.to_string(),
                actual: self
                    .defaults
                    .get(&plane)
                    .cloned()
                    .unwrap_or_else(|| "<none>".into()),
            })
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RuntimeRegistryError {
    #[error("enhanced runtime manifest is required")]
    MissingManifest,
    #[error("runtime kind does not match the execution plane")]
    KindMismatch,
    #[error("runtime digest does not match the verified manifest")]
    DigestMismatch,
    #[error("enhanced binary artifact hash mismatch: expected {expected}, got {actual}")]
    ArtifactMismatch { expected: String, actual: String },
    #[error("cannot hash runtime binary: {0}")]
    Io(String),
    #[error("execution plane {plane:?} is not installed")]
    RuntimeMissing { plane: ExecutionPlane },
    #[error(
        "bound {plane:?} runtime digest {expected} is not installed (default {actual}); silent fallback is forbidden"
    )]
    BoundRuntimeUnavailable {
        plane: ExecutionPlane,
        expected: String,
        actual: String,
    },
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

fn sha256_file(path: &PathBuf) -> Result<String, RuntimeRegistryError> {
    let bytes = std::fs::read(path).map_err(|error| RuntimeRegistryError::Io(error.to_string()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn official(digest: &str) -> RuntimeInstall {
        RuntimeInstall {
            plane: ExecutionPlane::OfficialCodex,
            executable: PathBuf::from(format!("C:/codex/{digest}.exe")),
            digest: digest.into(),
            artifact_sha256: None,
            manifest: None,
        }
    }

    #[test]
    fn keeps_previous_digest_after_a_new_default_is_registered() {
        let mut registry = RuntimeRegistry::new();
        registry.register_default(official("digest-a")).unwrap();
        registry.register(official("digest-b")).unwrap();
        assert_eq!(
            registry
                .require(ExecutionPlane::OfficialCodex)
                .unwrap()
                .digest,
            "digest-a"
        );
        assert_eq!(
            registry
                .require_digest(ExecutionPlane::OfficialCodex, "digest-b")
                .unwrap()
                .digest,
            "digest-b"
        );
        registry
            .set_default(ExecutionPlane::OfficialCodex, "digest-b")
            .unwrap();
        assert_eq!(
            registry
                .require(ExecutionPlane::OfficialCodex)
                .unwrap()
                .digest,
            "digest-b"
        );
        assert_eq!(
            registry
                .require_digest(ExecutionPlane::OfficialCodex, "digest-a")
                .unwrap()
                .digest,
            "digest-a"
        );
    }
}
