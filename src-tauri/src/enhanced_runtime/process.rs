use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use vellum_enhanced_codex::EnhancedRuntimeFeatures;

use super::{ExecutionPlane, RuntimeRegistry, RuntimeRegistryError, ThreadRuntimeBinding};

pub const CODEX_HOME_DIR: &str = "codex-home";
const ENHANCED_DEBUG_LOG_RELATIVE_PATH: &str = "log/enhanced-events.jsonl";
const ENHANCED_DEBUG_LOG_MAX_BYTES: u64 = 8 * 1024 * 1024;
const ENHANCED_DEBUG_LOG_RETAINED_FILES: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLaunchSpec {
    pub plane: ExecutionPlane,
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub codex_home: PathBuf,
    pub digest: String,
    pub features: EnhancedRuntimeFeatures,
}

pub struct RuntimeProcessLauncher;

impl RuntimeProcessLauncher {
    pub fn spec_for_binding(
        registry: &RuntimeRegistry,
        binding: &ThreadRuntimeBinding,
        data_root: &Path,
    ) -> Result<RuntimeLaunchSpec, EnhancedProcessError> {
        let install = registry.require_digest(binding.plane, &binding.runtime_digest)?;
        if !install.executable.is_absolute() {
            return Err(EnhancedProcessError::ExecutableInvalid {
                plane: binding.plane,
                path: install.executable.clone(),
            });
        }
        if !install.executable.exists() {
            return Err(EnhancedProcessError::LaunchFailed {
                plane: binding.plane,
                path: install.executable.clone(),
            });
        }
        if let Some(expected) = install.artifact_sha256.as_deref() {
            let actual = sha256_file(&install.executable)?;
            if actual != expected && format!("sha256:{actual}") != expected {
                return Err(EnhancedProcessError::ArtifactMismatch {
                    expected: expected.to_string(),
                    actual: format!("sha256:{actual}"),
                });
            }
        }
        if binding.plane == ExecutionPlane::EnhancedCodex && install.manifest.is_none() {
            return Err(EnhancedProcessError::MissingEnhancedManifest);
        }
        let features = install
            .manifest
            .as_ref()
            .map(|manifest| manifest.features.clone().into())
            .unwrap_or_else(EnhancedRuntimeFeatures::all_off);
        let codex_home = data_root
            .join("runtimes")
            .join(binding.plane.as_str())
            .join(sanitize_digest(&binding.runtime_digest))
            .join(CODEX_HOME_DIR);
        std::fs::create_dir_all(&codex_home)
            .map_err(|error| EnhancedProcessError::Io(format!("create CODEX_HOME: {error}")))?;
        write_enhanced_runtime_config(
            &codex_home,
            binding,
            features,
            install
                .manifest
                .as_ref()
                .map(|manifest| manifest.enhanced_commit.as_str()),
        )?;
        let mut env = BTreeMap::new();
        env.insert(
            "CODEX_HOME".into(),
            codex_home.to_string_lossy().into_owned(),
        );
        env.insert(
            "VELLUM_EXECUTION_PLANE".into(),
            binding.plane.as_str().to_string(),
        );
        env.insert(
            "VELLUM_RUNTIME_DIGEST".into(),
            binding.runtime_digest.clone(),
        );
        Ok(RuntimeLaunchSpec {
            plane: binding.plane,
            executable: install.executable.clone(),
            args: vec!["app-server".into()],
            env,
            codex_home,
            digest: binding.runtime_digest.clone(),
            features,
        })
    }
}

pub fn write_enhanced_runtime_config(
    codex_home: &Path,
    binding: &ThreadRuntimeBinding,
    compiled_feature_defaults: EnhancedRuntimeFeatures,
    enhanced_commit: Option<&str>,
) -> Result<(), EnhancedProcessError> {
    let debug_log = (binding.plane == ExecutionPlane::EnhancedCodex).then(|| {
        serde_json::json!({
            "enabled": true,
            "path": ENHANCED_DEBUG_LOG_RELATIVE_PATH,
            "maxBytes": ENHANCED_DEBUG_LOG_MAX_BYTES,
            "retainedFiles": ENHANCED_DEBUG_LOG_RETAINED_FILES,
            "queueCapacity": 2048,
            "flushIntervalMs": 250,
        })
    });
    let payload = serde_json::json!({
        "runtimeDigest": binding.runtime_digest,
        "enhancedCodexCommit": enhanced_commit,
        "executionPlane": binding.plane.as_str(),
        "compiledFeatureDefaults": compiled_feature_defaults,
        "debugLog": debug_log,
    });
    std::fs::write(
        codex_home.join("enhanced-runtime.json"),
        serde_json::to_vec_pretty(&payload).map_err(|error| {
            EnhancedProcessError::Io(format!("encode enhanced-runtime.json: {error}"))
        })?,
    )
    .map_err(|error| EnhancedProcessError::Io(format!("write enhanced-runtime.json: {error}")))
}

fn sha256_file(path: &Path) -> Result<String, EnhancedProcessError> {
    let bytes = std::fs::read(path)
        .map_err(|error| EnhancedProcessError::Io(format!("hash runtime binary: {error}")))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn sanitize_digest(digest: &str) -> String {
    digest
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[derive(Debug, thiserror::Error)]
pub enum EnhancedProcessError {
    #[error("{plane:?} executable is invalid: {}", path.display())]
    ExecutableInvalid {
        plane: ExecutionPlane,
        path: PathBuf,
    },
    #[error("{plane:?} failed to start from {}; silent fallback is forbidden", path.display())]
    LaunchFailed {
        plane: ExecutionPlane,
        path: PathBuf,
    },
    #[error(transparent)]
    Registry(#[from] RuntimeRegistryError),
    #[error("enhanced runtime manifest is required to launch Enhanced Codex")]
    MissingEnhancedManifest,
    #[error("enhanced binary artifact hash mismatch: expected {expected}, got {actual}")]
    ArtifactMismatch { expected: String, actual: String },
    #[error("{0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enhanced_runtime::{
        ExecutionPlane, RuntimeInstall, RuntimeRegistry, ThreadRuntimeBinding,
    };
    use std::fs;

    #[test]
    fn missing_enhanced_binary_is_explicit_and_uses_isolated_home() {
        let temp = tempfile::tempdir().unwrap();
        let official = temp.path().join("official.exe");
        fs::write(&official, []).unwrap();
        let mut registry = RuntimeRegistry::new();
        registry
            .register(RuntimeInstall {
                plane: ExecutionPlane::OfficialCodex,
                executable: official,
                digest: "official-digest".into(),
                artifact_sha256: None,
                manifest: None,
            })
            .unwrap();
        let binding = ThreadRuntimeBinding::new(
            "t1",
            ExecutionPlane::EnhancedCodex,
            "missing-digest",
            "qwen",
            "qwen3-coder",
            "native-1",
            1,
        );
        let error =
            RuntimeProcessLauncher::spec_for_binding(&registry, &binding, temp.path()).unwrap_err();
        assert!(matches!(
            error,
            EnhancedProcessError::Registry(RuntimeRegistryError::RuntimeMissing { .. })
                | EnhancedProcessError::Registry(
                    RuntimeRegistryError::BoundRuntimeUnavailable { .. }
                )
        ));
    }

    #[test]
    fn official_launch_uses_per_runtime_codex_home() {
        let temp = tempfile::tempdir().unwrap();
        let official = temp.path().join("official.exe");
        fs::write(&official, []).unwrap();
        let mut registry = RuntimeRegistry::new();
        registry
            .register(RuntimeInstall {
                plane: ExecutionPlane::OfficialCodex,
                executable: official.clone(),
                digest: "official-digest".into(),
                artifact_sha256: None,
                manifest: None,
            })
            .unwrap();
        let binding = ThreadRuntimeBinding::new(
            "t1",
            ExecutionPlane::OfficialCodex,
            "official-digest",
            "openai-official",
            "gpt-5.4",
            "native-1",
            1,
        );
        let spec =
            RuntimeProcessLauncher::spec_for_binding(&registry, &binding, temp.path()).unwrap();
        assert_eq!(spec.executable, official);
        assert!(spec.codex_home.ends_with(CODEX_HOME_DIR));
        assert!(spec
            .env
            .get("CODEX_HOME")
            .unwrap()
            .contains("official-codex"));
        let config: serde_json::Value = serde_json::from_slice(
            &fs::read(spec.codex_home.join("enhanced-runtime.json")).unwrap(),
        )
        .unwrap();
        assert!(config["debugLog"].is_null());
    }

    #[test]
    fn enhanced_runtime_config_enables_bounded_debug_log() {
        let temp = tempfile::tempdir().unwrap();
        let binding = ThreadRuntimeBinding::new(
            "t1",
            ExecutionPlane::EnhancedCodex,
            "enhanced-digest",
            "qwen",
            "qwen3-coder",
            "native-1",
            1,
        );
        write_enhanced_runtime_config(
            temp.path(),
            &binding,
            EnhancedRuntimeFeatures::all_on(),
            Some("enhanced-commit"),
        )
        .unwrap();

        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(temp.path().join("enhanced-runtime.json")).unwrap())
                .unwrap();
        assert_eq!(
            config["compiledFeatureDefaults"],
            serde_json::json!({
                "qwenToolReliability": true,
                "deepseekContextRecovery": true,
                "qwenBoundedContinuation": true,
            })
        );
        assert_eq!(config["debugLog"]["enabled"], true);
        assert_eq!(config["debugLog"]["path"], ENHANCED_DEBUG_LOG_RELATIVE_PATH);
        assert_eq!(config["debugLog"]["maxBytes"], ENHANCED_DEBUG_LOG_MAX_BYTES);
        assert_eq!(
            config["debugLog"]["retainedFiles"],
            ENHANCED_DEBUG_LOG_RETAINED_FILES
        );
    }
}
