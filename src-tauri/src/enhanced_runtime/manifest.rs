use serde::{Deserialize, Serialize};
use vellum_enhanced_codex::{
    compute_runtime_digest, DigestError, DigestInputs, EnhancedRuntimeFeatures,
    EnhancedRuntimeLockFile,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeKind {
    OfficialCodex,
    EnhancedCodex,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct FeatureFlags {
    pub qwen_tool_reliability: bool,
    pub deepseek_context_recovery: bool,
    pub qwen_bounded_continuation: bool,
    #[serde(default)]
    pub repetition_notice: bool,
    #[serde(default)]
    pub intent_continuation: bool,
}

impl From<FeatureFlags> for EnhancedRuntimeFeatures {
    fn from(value: FeatureFlags) -> Self {
        Self {
            qwen_tool_reliability: value.qwen_tool_reliability,
            deepseek_context_recovery: value.deepseek_context_recovery,
            qwen_bounded_continuation: value.qwen_bounded_continuation,
            repetition_notice: value.repetition_notice,
            intent_continuation: value.intent_continuation,
        }
    }
}

impl From<EnhancedRuntimeFeatures> for FeatureFlags {
    fn from(value: EnhancedRuntimeFeatures) -> Self {
        Self {
            qwen_tool_reliability: value.qwen_tool_reliability,
            deepseek_context_recovery: value.deepseek_context_recovery,
            qwen_bounded_continuation: value.qwen_bounded_continuation,
            repetition_notice: value.repetition_notice,
            intent_continuation: value.intent_continuation,
        }
    }
}

/// Feature defaults baked into an Enhanced Runtime release. Eval still selects
/// an ablation profile per run; this is the binary's compiled default.
/// New experiments stay off so E5 cannot smuggle them.
pub const FEATURE_DEFAULTS_MVP: EnhancedRuntimeFeatures = EnhancedRuntimeFeatures {
    qwen_tool_reliability: true,
    deepseek_context_recovery: true,
    qwen_bounded_continuation: true,
    repetition_notice: false,
    intent_continuation: false,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnhancedRuntimeManifest {
    pub schema_version: u32,
    pub runtime_kind: RuntimeKind,
    pub codex_upstream_commit: String,
    pub enhanced_commit: String,
    pub qwen_source_commit: String,
    pub deepseek_source_commit: String,
    pub features: FeatureFlags,
    pub build_profile: String,
    pub build_target: String,
    pub artifact_sha256: String,
}

impl EnhancedRuntimeManifest {
    pub fn from_lock(
        lock: &EnhancedRuntimeLockFile,
        features: EnhancedRuntimeFeatures,
        build_target: impl Into<String>,
    ) -> Result<Self, ManifestError> {
        let build_target = build_target.into();
        let enhanced = lock
            .enhanced_codex_commit
            .clone()
            .filter(|value| !value.is_empty())
            .ok_or(ManifestError::IncompleteIdentity(
                "enhancedCodexCommit".into(),
            ))?;
        let artifact = lock
            .artifact_for_target(&build_target)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or(ManifestError::IncompleteIdentity("artifactSha256".into()))?;
        Ok(Self {
            schema_version: 1,
            runtime_kind: RuntimeKind::EnhancedCodex,
            codex_upstream_commit: lock.codex_upstream_commit.clone(),
            enhanced_commit: enhanced,
            qwen_source_commit: lock.qwen_code_source_commit.clone(),
            deepseek_source_commit: lock.deepseek_harness_source_commit.clone(),
            features: features.into(),
            build_profile: lock.build_profile.clone(),
            build_target,
            artifact_sha256: artifact,
        })
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        let manifest = serde_json::from_slice::<Self>(bytes)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.schema_version != 1 {
            return Err(ManifestError::UnsupportedSchema(self.schema_version));
        }
        Ok(())
    }

    pub fn verify_against_lock(&self, lock: &EnhancedRuntimeLockFile) -> Result<(), ManifestError> {
        lock.validate()
            .map_err(|error| ManifestError::Lock(error.to_string()))?;
        self.validate()?;
        if self.codex_upstream_commit != lock.codex_upstream_commit
            || Some(self.enhanced_commit.as_str()) != lock.enhanced_codex_commit.as_deref()
            || self.qwen_source_commit != lock.qwen_code_source_commit
            || self.deepseek_source_commit != lock.deepseek_harness_source_commit
            || Some(self.artifact_sha256.as_str()) != lock.artifact_for_target(&self.build_target)
            || self.build_profile != lock.build_profile
        {
            return Err(ManifestError::LockMismatch);
        }
        Ok(())
    }

    pub fn runtime_digest(&self) -> Result<String, ManifestError> {
        Ok(compute_runtime_digest(&DigestInputs {
            codex_upstream_commit: self.codex_upstream_commit.clone(),
            enhanced_codex_commit: self.enhanced_commit.clone(),
            qwen_source_commit: self.qwen_source_commit.clone(),
            deepseek_source_commit: self.deepseek_source_commit.clone(),
            feature_defaults: self.features.clone().into(),
            build_profile: self.build_profile.clone(),
            target_triple: self.build_target.clone(),
            artifact_sha256: self.artifact_sha256.clone(),
        })?)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("runtime manifest JSON is invalid: {0}")]
    Json(String),
    #[error("unsupported runtime manifest schema version: {0}")]
    UnsupportedSchema(u32),
    #[error("runtime manifest does not match enhanced-runtime.lock.json")]
    LockMismatch,
    #[error("runtime identity is incomplete: {0}")]
    IncompleteIdentity(String),
    #[error("lock file is invalid: {0}")]
    Lock(String),
    #[error(transparent)]
    Digest(#[from] DigestError),
}

impl From<serde_json::Error> for ManifestError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_lock() -> EnhancedRuntimeLockFile {
        let mut lock = EnhancedRuntimeLockFile::mvp_pins();
        lock.enhanced_codex_commit = Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into());
        lock.artifact_sha256 =
            Some("sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into());
        lock.target_triple = Some("x86_64-pc-windows-msvc".into());
        lock
    }

    #[test]
    fn from_lock_requires_a_pinned_target_triple() {
        let lock = complete_lock();
        let matching = EnhancedRuntimeManifest::from_lock(
            &lock,
            FEATURE_DEFAULTS_MVP,
            "x86_64-pc-windows-msvc",
        )
        .unwrap();
        matching.verify_against_lock(&lock).unwrap();

        assert!(matches!(
            EnhancedRuntimeManifest::from_lock(&lock, FEATURE_DEFAULTS_MVP, "aarch64-apple-darwin"),
            Err(ManifestError::IncompleteIdentity(_))
        ));
    }

    #[test]
    fn desktop_release_enables_all_qualified_enhanced_features() {
        assert_eq!(FEATURE_DEFAULTS_MVP, EnhancedRuntimeFeatures::all_on());
    }
}
