//! M31: desktop-side pinned Codex installation.
//!
//! Flow: verify that the resource manifest matches the hash embedded into the
//! Vellum binary at compile time, resolve the bundled artifact for the host
//! architecture, verify its SHA-256, stage it over SSH stdin, then ask the
//! agent to perform the digest-verified atomic install.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};
use crate::remote::agent_client::RemoteAgentClient;
use crate::remote::process::background_command;
use crate::state::AppState;

const RELEASE_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexArtifact {
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactSet {
    #[serde(rename = "linux-x64")]
    pub linux_x64: CodexArtifact,
    #[serde(rename = "linux-arm64")]
    pub linux_arm64: CodexArtifact,
}

/// Component pin with per-arch artifacts (agent / broker self-update).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ComponentManifestEntry {
    pub version: String,
    pub artifacts: ArtifactSet,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProxyManifestEntry {
    pub image: String,
    pub artifacts: ArtifactSet,
}

/// Canonical release schema consumed by bootstrap, install and update.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseManifest {
    pub schema_version: u32,
    pub release_version: String,
    pub codex: CodexManifestEntry,
    pub agent: ComponentManifestEntry,
    pub broker: ComponentManifestEntry,
    pub proxy: ProxyManifestEntry,
    pub protocol_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexManifestEntry {
    pub pinned_version: String,
    pub compatible_range: String,
    pub artifacts: ArtifactSet,
}

/// Renderer-safe release readiness. `ready` means the resource manifest is
/// the exact manifest embedded into this binary and every artifact declaration
/// passed schema validation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteReleaseStatus {
    pub ready: bool,
    pub trust: String,
    pub release_version: Option<String>,
    pub codex_version: Option<String>,
    pub agent_version: Option<String>,
    pub broker_version: Option<String>,
    pub proxy_image: Option<String>,
    pub proxy_digest: Option<String>,
    pub detail: String,
}

impl ReleaseManifest {
    fn validate(&self) -> Result<(), String> {
        if self.schema_version != RELEASE_SCHEMA_VERSION {
            return Err(format!(
                "ReleaseManifestSchemaUnsupported: expected {RELEASE_SCHEMA_VERSION}, got {}",
                self.schema_version
            ));
        }
        if self.release_version.trim().is_empty() || self.protocol_version == 0 {
            return Err("ReleaseManifestInvalid: releaseVersion/protocolVersion missing".into());
        }
        for artifact in [
            &self.codex.artifacts.linux_x64,
            &self.codex.artifacts.linux_arm64,
            &self.agent.artifacts.linux_x64,
            &self.agent.artifacts.linux_arm64,
            &self.broker.artifacts.linux_x64,
            &self.broker.artifacts.linux_arm64,
        ] {
            if artifact.url.trim().is_empty() || !valid_sha256(&artifact.sha256) {
                return Err("ReleaseManifestInvalid: artifact URL/digest is invalid".into());
            }
        }
        for artifact in [
            &self.proxy.artifacts.linux_x64,
            &self.proxy.artifacts.linux_arm64,
        ] {
            if artifact.url.trim().is_empty() || !valid_sha256(&artifact.sha256) {
                return Err("ReleaseManifestInvalid: proxy artifact is invalid".into());
            }
        }
        if self.proxy.image.trim().is_empty() {
            return Err("ReleaseManifestInvalid: proxy image is missing".into());
        }
        Ok(())
    }

    pub(crate) fn resource_digest(&self, relative: &str) -> Option<&str> {
        match relative.replace('\\', "/").as_str() {
            "linux-amd64/vellum-remote-agent" => Some(&self.agent.artifacts.linux_x64.sha256),
            "linux-arm64/vellum-remote-agent" => Some(&self.agent.artifacts.linux_arm64.sha256),
            "linux-amd64/vellum-remote-broker" => Some(&self.broker.artifacts.linux_x64.sha256),
            "linux-arm64/vellum-remote-broker" => Some(&self.broker.artifacts.linux_arm64.sha256),
            _ => None,
        }
    }
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn release_resource_roots() -> Vec<PathBuf> {
    crate::install_paths::remote_resource_roots()
}

fn manifest_source() -> AppResult<Vec<u8>> {
    let root = release_resource_roots()
        .into_iter()
        .find(|root| root.join("manifest.json").is_file())
        .ok_or_else(|| {
            AppError::Message(
                "ReleaseManifestUnavailable: bundled resources/remote/manifest.json is missing"
                    .into(),
            )
        })?;
    std::fs::read(root.join("manifest.json"))
        .map_err(|error| AppError::Message(format!("read release manifest: {error}")))
}

fn verify_embedded_manifest(raw: &[u8]) -> AppResult<()> {
    let expected = option_env!("VELLUM_BUNDLED_MANIFEST_SHA256").ok_or_else(|| {
        AppError::Message(
            "ReleaseManifestUnavailable: this build does not embed a remote bundle".into(),
        )
    })?;
    let actual = hex::encode(Sha256::digest(raw));
    if actual != expected {
        return Err(AppError::Message(format!(
            "ReleaseManifestDigestMismatch: expected {expected}, got {actual}"
        )));
    }
    Ok(())
}

/// Load the canonical manifest and verify it against the hash compiled into
/// the executable. Runtime environment variables cannot replace this source.
pub fn load_verified_manifest() -> AppResult<ReleaseManifest> {
    let raw = manifest_source()?;
    verify_embedded_manifest(&raw)?;
    let manifest: ReleaseManifest = serde_json::from_slice(&raw)
        .map_err(|error| AppError::Message(format!("invalid release manifest: {error}")))?;
    manifest.validate().map_err(AppError::Message)?;
    Ok(manifest)
}

/// Report whether production install/update operations can safely proceed.
/// This deliberately converts verification failures into a renderer-visible
/// blocked state instead of making the whole Remote screen fail to load.
pub fn release_status() -> RemoteReleaseStatus {
    match load_verified_manifest() {
        Ok(manifest) => RemoteReleaseStatus {
            ready: true,
            trust: "bundled".into(),
            release_version: Some(manifest.release_version),
            codex_version: Some(manifest.codex.pinned_version),
            agent_version: Some(manifest.agent.version),
            broker_version: Some(manifest.broker.version),
            proxy_image: Some(manifest.proxy.image),
            proxy_digest: None,
            detail: "Ready".into(),
        },
        Err(error) => RemoteReleaseStatus {
            ready: false,
            trust: "bundled".into(),
            release_version: None,
            codex_version: None,
            agent_version: None,
            broker_version: None,
            proxy_image: None,
            proxy_digest: None,
            detail: error.to_string(),
        },
    }
}

fn artifact_for_arch<'a>(artifacts: &'a ArtifactSet, arch: &str) -> AppResult<&'a CodexArtifact> {
    match arch {
        "aarch64" | "arm64" => Ok(&artifacts.linux_arm64),
        "x86_64" | "amd64" => Ok(&artifacts.linux_x64),
        other => Err(AppError::Message(format!(
            "ReleaseArtifactUnsupportedArch: {other}"
        ))),
    }
}

/// Resolve a compile-time-verified bundled component artifact before the Agent exists.
/// The component name is owned by Desktop code, never renderer input.
pub(crate) fn component_artifact_for_arch(
    manifest: &ReleaseManifest,
    component: &str,
    arch: &str,
) -> AppResult<CodexArtifact> {
    let artifacts = match component {
        "vellum-remote-agent" => &manifest.agent.artifacts,
        "vellum-remote-broker" => &manifest.broker.artifacts,
        _ => {
            return Err(AppError::Message(format!(
                "ReleaseArtifactUnknownComponent: {component}"
            )))
        }
    };
    artifact_for_arch(artifacts, arch).cloned()
}

fn select_artifact<'a>(manifest: &'a ReleaseManifest, arch: &str) -> AppResult<&'a CodexArtifact> {
    artifact_for_arch(&manifest.codex.artifacts, arch)
}

pub(crate) fn proxy_artifact_for_arch<'a>(
    manifest: &'a ReleaseManifest,
    arch: &str,
) -> AppResult<&'a CodexArtifact> {
    artifact_for_arch(&manifest.proxy.artifacts, arch)
}

fn sha256_file(path: &Path) -> AppResult<String> {
    let bytes = std::fs::read(path)
        .map_err(|error| AppError::Message(format!("read artifact {}: {error}", path.display())))?;
    let digest = Sha256::digest(&bytes);
    Ok(hex::encode(digest))
}

fn bundled_artifact(relative: &str) -> AppResult<PathBuf> {
    if relative.is_empty()
        || relative.contains("..")
        || relative.starts_with('/')
        || relative.starts_with('\\')
        || relative.contains(':')
    {
        return Err(AppError::Message("ReleaseArtifactBundlePathUnsafe".into()));
    }
    release_resource_roots()
        .into_iter()
        .map(|root| root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)))
        .find(|path| path.is_file())
        .ok_or_else(|| AppError::Message(format!("ReleaseArtifactMissing: bundle://{relative}")))
}

/// Resolve a bundled artifact or download an explicitly HTTPS artifact, then
/// verify its SHA-256 before any remote mutation.
pub(crate) fn download_artifact(artifact: &CodexArtifact) -> AppResult<PathBuf> {
    let staged = std::env::temp_dir().join(format!("vellum-codex-artifact-{}", ulid::Ulid::new()));
    let url = artifact.url.trim();
    if !url.starts_with("bundle://") && !url.starts_with("https://") {
        return Err(AppError::Message(
            "ReleaseArtifactUrlUnsafe: only bundle:// or https:// is allowed".into(),
        ));
    }
    let result = if let Some(relative) = url.strip_prefix("bundle://") {
        let source = bundled_artifact(relative)?;
        std::fs::copy(&source, &staged)
            .map(|_| ())
            .map_err(|error| {
                AppError::Message(format!("copy artifact {}: {error}", source.display()))
            })
    } else {
        let output = background_command("curl")
            .args([
                "-fsSL",
                "--proto",
                "=https",
                "--tlsv1.2",
                "--fail",
                url,
                "-o",
            ])
            .arg(&staged)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| AppError::Message(format!("curl spawn failed: {error}")))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(AppError::Message(format!(
                "artifact download failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    };
    if let Err(error) = result {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    let digest = sha256_file(&staged)?;
    let expected = artifact
        .sha256
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(&artifact.sha256);
    if digest != expected {
        let _ = std::fs::remove_file(&staged);
        return Err(AppError::Message(format!(
            "ReleaseArtifactDigestMismatch: expected {expected}, resolved {digest}"
        )));
    }
    Ok(staged)
}

/// Orchestrate a pinned Codex install on the remote host.
pub fn install_pinned_codex(
    state: &AppState,
    host_id: &str,
    operation_id: &str,
) -> AppResult<serde_json::Value> {
    let manifest = load_verified_manifest()?;
    let target = crate::remote::RemoteHostManager::resolve_target(state, host_id)?;
    let client = RemoteAgentClient::new(target.clone());
    let inventory = client.host_inventory_v2()?;
    let arch = inventory
        .pointer("/system/arch")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| AppError::Message("host inventory missing system.arch".into()))?;
    let artifact = select_artifact(&manifest, arch)?;

    // Download + local digest verification.
    let local_staged = download_artifact(artifact)?;
    let result = (|| -> AppResult<serde_json::Value> {
        let bytes = std::fs::read(&local_staged)
            .map_err(|error| AppError::Message(format!("read staged artifact: {error}")))?;
        let remote_path = format!("/tmp/vellum-codex-stage-{}.bin", ulid::Ulid::new());
        client.stage_artifact(&remote_path, &bytes)?;
        crate::remote::provision_remote_boundary_key(
            &client,
            &state.data_root(),
            host_id,
            operation_id,
        )?;
        let install = client.codex_install_pinned(
            operation_id,
            &remote_path,
            &artifact.sha256,
            &manifest.codex.pinned_version,
        );
        client.remove_remote_file(&remote_path);
        install
    })();
    let _ = std::fs::remove_file(&local_staged);
    let result = result?;
    crate::remote::confirm_remote_boundary_key_consumers(
        &client,
        &state.data_root(),
        host_id,
        &[crate::proxy::BoundaryKeyConsumer::NativeCodex],
        false,
    )?;
    Ok(result)
}

/// M30/M31: update the remote agent to the manifest-pinned version. Reports
/// component drift (agent / proxy) and performs the digest-verified agent
/// self-update when the running version differs.
pub fn update_remote_components(
    state: &AppState,
    host_id: &str,
    operation_id: &str,
) -> AppResult<serde_json::Value> {
    let manifest = load_verified_manifest()?;
    let target = crate::remote::RemoteHostManager::resolve_target(state, host_id)?;
    let client = RemoteAgentClient::new(target.clone());
    let inventory = client.host_inventory_v2()?;
    let arch = inventory
        .pointer("/system/arch")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| AppError::Message("host inventory missing system.arch".into()))?;
    let running_agent = inventory
        .pointer("/agentVersion")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let running_proxy_image = inventory
        .pointer("/proxy/image")
        .and_then(serde_json::Value::as_str);

    let mut report = serde_json::json!({
        "hostId": host_id,
        "agent": {
            "desired": manifest.agent.version,
            "running": running_agent,
            "current": running_agent == manifest.agent.version,
        },
        "broker": {
            "desired": manifest.broker.version,
            "verifiable": false,
            "note": "broker update RPC is not implemented; broker remains diagnostic-only"
        },
        "proxy": {
            "desiredImage": manifest.proxy.image,
            "runningImage": running_proxy_image,
            "current": running_proxy_image == Some(manifest.proxy.image.as_str()),
            "note": "proxy image switches on the next deployment apply"
        },
        "steps": [],
    });
    let steps = report
        .get_mut("steps")
        .and_then(serde_json::Value::as_array_mut)
        .expect("steps array");

    if running_agent == manifest.agent.version {
        steps.push("agent.alreadyCurrent".into());
        steps.push("broker.notUpdated".into());
        steps.push("proxy.updatedOnNextApply".into());
        return Ok(report);
    }
    let artifact = artifact_for_arch(&manifest.agent.artifacts, arch)?;
    let local_staged = download_artifact(artifact)?;
    let update_result = (|| -> AppResult<serde_json::Value> {
        let bytes = std::fs::read(&local_staged)
            .map_err(|error| AppError::Message(format!("read agent artifact: {error}")))?;
        let remote_path = format!("/tmp/vellum-agent-stage-{}.bin", ulid::Ulid::new());
        client.stage_artifact(&remote_path, &bytes)?;
        let update = client.agent_update(operation_id, &remote_path, &artifact.sha256);
        client.remove_remote_file(&remote_path);
        update
    })();
    let _ = std::fs::remove_file(&local_staged);
    let update = update_result?;
    steps.push("agent.updated".into());
    steps.push("broker.notUpdated".into());
    steps.push("proxy.updatedOnNextApply".into());
    report["update"] = update;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_fixture() -> ReleaseManifest {
        ReleaseManifest {
            schema_version: 3,
            release_version: "0.2.2".into(),
            codex: CodexManifestEntry {
                pinned_version: "0.147.0".into(),
                compatible_range: "0.147.x".into(),
                artifacts: ArtifactSet {
                    linux_x64: CodexArtifact {
                        url: "https://example.invalid/x64".into(),
                        sha256: "a".repeat(64),
                    },
                    linux_arm64: CodexArtifact {
                        url: "https://example.invalid/arm64".into(),
                        sha256: "b".repeat(64),
                    },
                },
            },
            agent: ComponentManifestEntry {
                version: "0.1.0".into(),
                artifacts: ArtifactSet {
                    linux_x64: CodexArtifact {
                        url: "https://example.invalid/agent-x64".into(),
                        sha256: "c".repeat(64),
                    },
                    linux_arm64: CodexArtifact {
                        url: "https://example.invalid/agent-arm64".into(),
                        sha256: "c".repeat(64),
                    },
                },
            },
            broker: ComponentManifestEntry {
                version: "0.1.0".into(),
                artifacts: ArtifactSet {
                    linux_x64: CodexArtifact {
                        url: "https://example.invalid/broker-x64".into(),
                        sha256: "d".repeat(64),
                    },
                    linux_arm64: CodexArtifact {
                        url: "https://example.invalid/broker-arm64".into(),
                        sha256: "d".repeat(64),
                    },
                },
            },
            proxy: ProxyManifestEntry {
                image: "vellum-proxy:0.2.2".into(),
                artifacts: ArtifactSet {
                    linux_x64: CodexArtifact {
                        url: "bundle://linux-amd64/proxy-image.tar".into(),
                        sha256: "e".repeat(64),
                    },
                    linux_arm64: CodexArtifact {
                        url: "bundle://linux-arm64/proxy-image.tar".into(),
                        sha256: "f".repeat(64),
                    },
                },
            },
            protocol_version: 1,
        }
    }

    #[test]
    fn canonical_manifest_validates_and_maps_bootstrap_resources() {
        let manifest = manifest_fixture();
        assert!(manifest.validate().is_ok());
        assert_eq!(
            manifest.resource_digest("linux-arm64/vellum-remote-agent"),
            Some("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc")
        );
        let mut malformed = manifest;
        malformed.schema_version = 1;
        assert_eq!(
            malformed.validate(),
            Err("ReleaseManifestSchemaUnsupported: expected 3, got 1".into())
        );
    }

    #[test]
    fn artifact_selection_maps_arch() {
        let manifest = manifest_fixture();
        assert_eq!(
            select_artifact(&manifest, "aarch64").unwrap().sha256,
            "b".repeat(64)
        );
        assert_eq!(
            select_artifact(&manifest, "x86_64").unwrap().url,
            "https://example.invalid/x64"
        );
        assert!(select_artifact(&manifest, "riscv64").is_err());
        assert_eq!(
            component_artifact_for_arch(&manifest, "vellum-remote-agent", "arm64")
                .unwrap()
                .sha256,
            "c".repeat(64)
        );
        assert!(component_artifact_for_arch(&manifest, "renderer-value", "arm64").is_err());
    }

    #[test]
    fn sha256_file_matches_digest() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("artifact.bin");
        std::fs::write(&path, b"payload").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            hex::encode(Sha256::digest(b"payload"))
        );
    }

    #[test]
    fn boundary_key_provisioning_precedes_the_first_install_pinned_rpc_in_source_order() {
        let source = include_str!("pinned_install.rs");
        let provision_at = source
            .find("provision_remote_boundary_key(")
            .expect("install_pinned_codex() must call provision_remote_boundary_key");
        let rpc_at = source
            .find("client.codex_install_pinned(")
            .expect("install_pinned_codex() must call codex_install_pinned");
        let confirm_at = source
            .find("confirm_remote_boundary_key_consumers(")
            .expect("install_pinned_codex() must confirm any started native instance");
        assert!(
            provision_at < rpc_at,
            "boundary-key provisioning must run before the first codex.installPinned RPC"
        );
        assert!(
            rpc_at < confirm_at,
            "native confirmation must follow install"
        );
    }
}
