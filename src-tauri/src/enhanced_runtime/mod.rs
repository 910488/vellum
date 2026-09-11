//! Dual Codex runtime routing for Vellum Desktop.
//!
//! Official OpenAI / GPT traffic stays on the unmodified Codex binary.
//! Trusted third-party providers bind to Enhanced Codex. This module must not
//! import Canonical compaction, Action Conversion, loop guard, or any other
//! historical Vellum agent-loop implementation.

pub mod app_server_bridge;
mod atomic;
pub mod attestation;
mod binding_store;
pub mod contracts;
pub(crate) mod desktop_manager;
pub mod env_lease;
pub mod launch_manifest;
mod manifest;
mod model_provider_map;
pub mod observations;
mod process;
pub mod process_info;
pub mod protocol_compat;
pub mod qualification;
mod registry;
mod router;
mod schema_contract;

pub use attestation::{
    AttestationError, AttestationWriter, BridgeAttestationV1, BridgeLifecycle, ChildAttestation,
    EnhancedRuntimeIdentity,
};
pub use binding_store::{BindingStoreError, ThreadRuntimeBindingStore};
pub use desktop_manager::{
    adopted_by_codex_desktop, configure_desktop_runtime, desktop_runtime_status,
    disable_desktop_runtime, live_bridge_attestation, observed_launch, packaged_bridge_executable,
    prepare_desktop_launch, release_desktop_launch, superseded_launch_repair, sync_desktop_launch,
    DesktopLaunchDesire, DesktopRuntimeLaunch, DesktopRuntimeManagerError, DesktopRuntimeStatus,
};
pub use launch_manifest::{
    LaunchManifestError, LaunchManifestV1, RuntimeBinaryIdentity, LAUNCH_MANIFEST_ENV,
};
pub use manifest::{
    EnhancedRuntimeManifest, FeatureFlags, ManifestError, RuntimeKind, FEATURE_DEFAULTS_MVP,
};
pub use model_provider_map::{
    ModelProviderRoute, TrustedModelProviderMap, MODEL_PROVIDER_MAP_SCHEMA_VERSION,
};
pub use process::{
    write_enhanced_runtime_config, EnhancedProcessError, RuntimeLaunchSpec, RuntimeProcessLauncher,
    CODEX_HOME_DIR,
};
pub use process_info::pid_is_alive;
pub use registry::{RuntimeInstall, RuntimeKey, RuntimeRegistry, RuntimeRegistryError};
pub use router::{ProviderClass, RouterError, RuntimeRouter, TrustedProviderSet};

pub const BINDING_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExecutionPlane {
    OfficialCodex,
    EnhancedCodex,
}

impl ExecutionPlane {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OfficialCodex => "official-codex",
            Self::EnhancedCodex => "enhanced-codex",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "official-codex" | "OfficialCodex" => Some(Self::OfficialCodex),
            "enhanced-codex" | "EnhancedCodex" => Some(Self::EnhancedCodex),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRuntimeBinding {
    pub binding_version: u16,
    pub thread_id: String,
    pub plane: ExecutionPlane,
    pub runtime_digest: String,
    pub provider_id: String,
    pub model_id: String,
    pub native_thread_id: String,
    pub created_at: i64,
}

impl ThreadRuntimeBinding {
    pub fn new(
        thread_id: impl Into<String>,
        plane: ExecutionPlane,
        runtime_digest: impl Into<String>,
        provider_id: impl Into<String>,
        model_id: impl Into<String>,
        native_thread_id: impl Into<String>,
        created_at: i64,
    ) -> Self {
        Self {
            binding_version: BINDING_VERSION,
            thread_id: thread_id.into(),
            plane,
            runtime_digest: runtime_digest.into(),
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            native_thread_id: native_thread_id.into(),
            created_at,
        }
    }
}

#[cfg(test)]
mod isolation_tests {
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn enhanced_runtime_does_not_import_legacy_agent_logic() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/enhanced_runtime");
        let forbidden = [
            "crate::compaction",
            "crate::continuation",
            "crate::loop_guard",
            "crate::harness",
            "canonical_v2",
            "action_conversion",
            "semantic_frontier",
            "rolling_local",
            "vellum_proxy_runtime",
        ];
        visit(&root, &forbidden);
    }

    fn visit(dir: &PathBuf, forbidden: &[&str]) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, forbidden);
                continue;
            }
            if path.file_name().and_then(|value| value.to_str()) == Some("mod.rs") {
                continue;
            }
            if path.extension().and_then(|value| value.to_str()) != Some("rs") {
                continue;
            }
            let source = fs::read_to_string(&path).unwrap();
            for needle in forbidden {
                assert!(
                    !source.contains(needle),
                    "{} must not import {needle}",
                    path.display()
                );
            }
        }
    }
}
