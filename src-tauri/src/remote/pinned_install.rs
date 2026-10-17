//! M31: desktop-side pinned Codex installation.
//!
//! Flow: verify that the resource manifest matches the hash embedded into the
//! Vellum binary at compile time, resolve the pinned artifact for the host
//! architecture, verify its SHA-256 and complete runtime layout, stage the
//! archive over SSH stdin, then ask the
//! agent to perform the digest-verified atomic install.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};
use crate::remote::agent_client::RemoteAgentClient;
use crate::remote::process::background_command;
use crate::state::AppState;

const RELEASE_SCHEMA_VERSION: u32 = 5;
const LEGACY_RELEASE_SCHEMA_VERSION: u32 = 3;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexArtifact {
    pub url: String,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub size: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub uncompressed_size: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

const MAX_CODEX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactSet {
    #[serde(rename = "linux-x64")]
    pub linux_x64: CodexArtifact,
    #[serde(rename = "linux-arm64")]
    pub linux_arm64: CodexArtifact,
    #[serde(
        rename = "darwin-arm64",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub darwin_arm64: Option<CodexArtifact>,
}

/// Component pin with per-arch artifacts (agent / broker self-update).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ComponentManifestEntry {
    pub version: String,
    pub artifacts: ArtifactSet,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProxyManifestEntry {
    #[serde(default)]
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
    #[serde(default)]
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
        if self.schema_version != RELEASE_SCHEMA_VERSION
            && self.schema_version != 4
            && self.schema_version != LEGACY_RELEASE_SCHEMA_VERSION
        {
            return Err(format!(
                "ReleaseManifestSchemaUnsupported: expected 3, 4 or {RELEASE_SCHEMA_VERSION}, got {}",
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
        if self.schema_version < 5 {
            for artifact in [
                &self.broker.artifacts.linux_x64,
                &self.broker.artifacts.linux_arm64,
            ] {
                if artifact.url.trim().is_empty() || !valid_sha256(&artifact.sha256) {
                    return Err("ReleaseManifestInvalid: broker artifact is invalid".into());
                }
            }
        }
        if self.schema_version < 5 && self.proxy.image.trim().is_empty() {
            return Err("ReleaseManifestInvalid: proxy image is missing".into());
        }
        if self.schema_version == 5 {
            if self.protocol_version != 5 {
                return Err("ReleaseManifestInvalid: schema 5 requires agent protocol 5".into());
            }
            for (artifact, relative) in [
                (
                    &self.proxy.artifacts.linux_x64,
                    "linux-amd64/vellum-proxy-daemon",
                ),
                (
                    &self.proxy.artifacts.linux_arm64,
                    "linux-arm64/vellum-proxy-daemon",
                ),
            ] {
                if artifact.url != format!("bundle://{relative}") {
                    return Err(
                        "ReleaseManifestInvalid: schema 5 requires native Linux proxy artifacts"
                            .into(),
                    );
                }
            }
        }
        if self.schema_version == 4
            || self.codex.artifacts.darwin_arm64.is_some()
            || self.agent.artifacts.darwin_arm64.is_some()
            || self.proxy.artifacts.darwin_arm64.is_some()
        {
            for artifact in [
                self.codex.artifacts.darwin_arm64.as_ref(),
                self.agent.artifacts.darwin_arm64.as_ref(),
                self.proxy.artifacts.darwin_arm64.as_ref(),
            ] {
                let artifact = artifact.ok_or_else(|| {
                    "ReleaseManifestInvalid: schema 4 requires Darwin ARM64 artifacts".to_string()
                })?;
                if artifact.url.trim().is_empty() || !valid_sha256(&artifact.sha256) {
                    return Err("ReleaseManifestInvalid: Darwin ARM64 artifact is invalid".into());
                }
            }
        }
        Ok(())
    }

    pub(crate) fn resource_digest(&self, relative: &str) -> Option<&str> {
        match relative.replace('\\', "/").as_str() {
            "linux-amd64/vellum-remote-agent" => Some(&self.agent.artifacts.linux_x64.sha256),
            "linux-arm64/vellum-remote-agent" => Some(&self.agent.artifacts.linux_arm64.sha256),
            "darwin-arm64/vellum-remote-agent" => self
                .agent
                .artifacts
                .darwin_arm64
                .as_ref()
                .map(|item| item.sha256.as_str()),
            "linux-amd64/vellum-proxy-daemon" if self.schema_version == 5 => {
                Some(&self.proxy.artifacts.linux_x64.sha256)
            }
            "linux-arm64/vellum-proxy-daemon" if self.schema_version == 5 => {
                Some(&self.proxy.artifacts.linux_arm64.sha256)
            }
            "linux-amd64/vellum-remote-broker" if self.schema_version < 5 => {
                Some(&self.broker.artifacts.linux_x64.sha256)
            }
            "linux-arm64/vellum-remote-broker" if self.schema_version < 5 => {
                Some(&self.broker.artifacts.linux_arm64.sha256)
            }
            "darwin-arm64/vellum-proxy-daemon" => self
                .proxy
                .artifacts
                .darwin_arm64
                .as_ref()
                .map(|item| item.sha256.as_str()),
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
            broker_version: (!manifest.broker.version.is_empty())
                .then_some(manifest.broker.version),
            proxy_image: (!manifest.proxy.image.is_empty()).then_some(manifest.proxy.image),
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
    artifact_for_os_arch(artifacts, "linux", arch)
}

pub(crate) fn artifact_for_os_arch<'a>(
    artifacts: &'a ArtifactSet,
    os: &str,
    arch: &str,
) -> AppResult<&'a CodexArtifact> {
    let platform = crate::remote::platform::RemotePlatform::from_os_arch(os, arch)
        .map_err(|error| AppError::Message(format!("{}: {}", error.code, error.message)))?;
    match platform {
        crate::remote::platform::RemotePlatform::LinuxAmd64 => Ok(&artifacts.linux_x64),
        crate::remote::platform::RemotePlatform::LinuxArm64 => Ok(&artifacts.linux_arm64),
        crate::remote::platform::RemotePlatform::DarwinArm64 => artifacts
            .darwin_arm64
            .as_ref()
            .ok_or_else(|| AppError::Message("ReleaseArtifactMissingDarwinArm64".into())),
    }
}

/// Resolve a compile-time-verified bundled component artifact before the Agent exists.
/// The component name is owned by Desktop code, never renderer input.
#[cfg(test)]
pub(crate) fn component_artifact_for_arch(
    manifest: &ReleaseManifest,
    component: &str,
    arch: &str,
) -> AppResult<CodexArtifact> {
    component_artifact_for_os_arch(manifest, component, "linux", arch)
}

pub(crate) fn component_artifact_for_os_arch(
    manifest: &ReleaseManifest,
    component: &str,
    os: &str,
    arch: &str,
) -> AppResult<CodexArtifact> {
    let artifacts = match component {
        "vellum-remote-agent" => &manifest.agent.artifacts,
        "vellum-remote-broker" if manifest.schema_version >= 5 => {
            return Err(AppError::Message(
                "ReleaseDiagnosticBrokerNotBundled".into(),
            ));
        }
        "vellum-remote-broker" => &manifest.broker.artifacts,
        "vellum-proxy-daemon"
            if os.eq_ignore_ascii_case("linux") && manifest.schema_version < 5 =>
        {
            return Err(AppError::Message(
                "ReleaseNativeLinuxProxyRequiresSchema5".into(),
            ));
        }
        "vellum-proxy-daemon" => &manifest.proxy.artifacts,
        _ => {
            return Err(AppError::Message(format!(
                "ReleaseArtifactUnknownComponent: {component}"
            )))
        }
    };
    artifact_for_os_arch(artifacts, os, arch).cloned()
}

#[cfg(test)]
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
    let mut file = std::fs::File::open(path)
        .map_err(|error| AppError::Message(format!("read artifact {}: {error}", path.display())))?;
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest).map_err(|error| AppError::Message(error.to_string()))?;
    Ok(hex::encode(digest.finalize()))
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

/// Inspect a signed bundle asset without staging, downloading or remote writes.
pub(crate) fn inspect_bundled_artifact(artifact: &CodexArtifact) -> AppResult<u64> {
    let relative = artifact.url.strip_prefix("bundle://").ok_or_else(|| {
        AppError::Message("bundledReleaseUnavailable: artifact is not bundled".into())
    })?;
    let path = bundled_artifact(relative)?;
    inspect_artifact_file(&path, &artifact.sha256)
}

pub(crate) fn inspect_bundled_codex_package(artifact: &CodexArtifact) -> AppResult<u64> {
    let relative = artifact.url.strip_prefix("bundle://").ok_or_else(|| {
        AppError::Message("bundledReleaseUnavailable: Codex package is not bundled".into())
    })?;
    let path = bundled_artifact(relative)?;
    vellum_codex_package::verify_archive(&path).map_err(AppError::Message)
}

/// Preflight remains read-only and offline. HTTPS packages carry build-time
/// sizes in the same manifest whose digest is embedded into Desktop.
pub(crate) fn inspect_codex_package_sizes(artifact: &CodexArtifact) -> AppResult<(u64, u64)> {
    if artifact.url.starts_with("bundle://") {
        return Ok((
            inspect_bundled_artifact(artifact)?,
            inspect_bundled_codex_package(artifact)?,
        ));
    }
    if !artifact.url.starts_with("https://")
        || artifact.size == 0
        || artifact.size > MAX_CODEX_ARCHIVE_BYTES
        || artifact.uncompressed_size == 0
        || artifact.uncompressed_size > 1024 * 1024 * 1024
    {
        return Err(AppError::Message(
            "ReleaseManifestInvalid: HTTPS Codex package needs signed archive and expanded sizes"
                .into(),
        ));
    }
    Ok((artifact.size, artifact.uncompressed_size))
}

fn verify_codex_package(path: &Path, artifact: &CodexArtifact) -> AppResult<()> {
    let size = inspect_artifact_file(path, &artifact.sha256).map_err(|error|
        AppError::Message(format!("CodexPackageVerificationFailed: {error}; retry deployment to download a verified package")))?;
    if size > MAX_CODEX_ARCHIVE_BYTES || (artifact.size != 0 && size != artifact.size) {
        return Err(AppError::Message(
            "CodexPackageVerificationFailed: archive size mismatch".into(),
        ));
    }
    let expanded = vellum_codex_package::verify_archive(path)
        .map_err(|error| AppError::Message(format!("CodexPackageVerificationFailed: {error}")))?;
    if artifact.uncompressed_size != 0 && expanded > artifact.uncompressed_size {
        return Err(AppError::Message(
            "CodexPackageVerificationFailed: expanded size exceeds signed reserve".into(),
        ));
    }
    Ok(())
}

async fn cached_codex_package_with<F, Fut>(
    root: &Path,
    artifact: &CodexArtifact,
    download: F,
) -> AppResult<PathBuf>
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: std::future::Future<Output = AppResult<()>>,
{
    let digest = artifact
        .sha256
        .strip_prefix("sha256:")
        .unwrap_or(&artifact.sha256)
        .to_ascii_lowercase();
    if !valid_sha256(&digest) {
        return Err(AppError::Message(
            "ReleaseManifestInvalid: invalid Codex package digest".into(),
        ));
    }
    let dir = root.join("remote-cache").join("codex");
    std::fs::create_dir_all(&dir).map_err(|error| AppError::Message(error.to_string()))?;
    let cached = dir.join(format!("{digest}.tar.gz"));
    if cached.exists() {
        match verify_codex_package(&cached, artifact) {
            Ok(()) => return Ok(cached),
            Err(error) => {
                std::fs::remove_file(&cached).map_err(|remove| {
                    AppError::Message(format!(
                        "CodexPackageVerificationFailed: {error}; remove corrupt cache: {remove}"
                    ))
                })?;
            }
        }
    }
    // A cancelled/failed transfer leaves no cache entry. Each retry gets its
    // own temporary file, and only a fully verified archive is published.
    let staged = tempfile::NamedTempFile::new_in(&dir)
        .map_err(|error| AppError::Message(error.to_string()))?;
    download(staged.path().to_path_buf()).await?;
    verify_codex_package(staged.path(), artifact)?;
    match staged.persist_noclobber(&cached) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            if let Err(error) = verify_codex_package(&cached, artifact) {
                let _ = std::fs::remove_file(&cached);
                return Err(error);
            }
        }
        Err(error) => return Err(AppError::Message(format!("cache Codex package: {error}"))),
    }
    Ok(cached)
}

async fn cached_codex_package(
    root: &Path,
    artifact: &CodexArtifact,
    progress: &mut impl FnMut(u64, Option<u64>),
) -> AppResult<PathBuf> {
    cached_codex_package_with(root, artifact, |path| async move {
        if let Some(relative) = artifact.url.strip_prefix("bundle://") {
            let source = bundled_artifact(relative)?;
            std::fs::copy(source, path).map_err(|error| AppError::Message(error.to_string()))?;
            return Ok(());
        }
        inspect_codex_package_sizes(artifact)?;
        let result = async {
            let mut response = super::desktop_codex::http_client()?.get(&artifact.url).send().await
                .map_err(|error| AppError::Message(error.to_string()))?
                .error_for_status().map_err(|error| AppError::Message(error.to_string()))?;
            if response.content_length().is_some_and(|size| size > MAX_CODEX_ARCHIVE_BYTES) {
                return Err(AppError::Message("archive too large".into()));
            }
            let mut output = std::fs::File::create(path).map_err(|error| AppError::Message(error.to_string()))?;
            let mut bytes = 0u64;
            let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
            progress(0, Some(artifact.size));
            while let Some(chunk) = response.chunk().await.map_err(|error| AppError::Message(error.to_string()))? {
                bytes += chunk.len() as u64;
                if bytes > MAX_CODEX_ARCHIVE_BYTES || bytes > artifact.size { return Err(AppError::Message("archive exceeds signed size".into())); }
                output.write_all(&chunk).map_err(|error| AppError::Message(error.to_string()))?;
                if last.elapsed() >= std::time::Duration::from_millis(500) || bytes == artifact.size {
                    progress(bytes, Some(artifact.size));
                    last = std::time::Instant::now();
                }
            }
            output.sync_all().map_err(|error| AppError::Message(error.to_string()))?;
            Ok(())
        }.await;
        result.map_err(|error: AppError| AppError::Message(format!("CodexPackageDownloadFailed: {error}; check access to GitHub OpenAI releases and retry deployment")))
    }).await
}

fn inspect_artifact_file(path: &Path, expected: &str) -> AppResult<u64> {
    let expected = expected.strip_prefix("sha256:").unwrap_or(expected);
    if sha256_file(path)? != expected {
        return Err(AppError::Message(
            "ReleaseArtifactDigestMismatch: bundled artifact failed verification".into(),
        ));
    }
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|error| AppError::Message(error.to_string()))
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
    tauri::async_runtime::block_on(install_pinned_codex_with_progress(
        state,
        host_id,
        operation_id,
        |_, _| {},
    ))
}

pub(crate) async fn install_pinned_codex_with_progress(
    state: &AppState,
    host_id: &str,
    operation_id: &str,
    mut progress: impl FnMut(u64, Option<u64>),
) -> AppResult<serde_json::Value> {
    let manifest = load_verified_manifest()?;
    let target = crate::remote::RemoteHostManager::resolve_target(state, host_id)?;
    let client = RemoteAgentClient::new(target.clone());
    let inventory = client.host_inventory_v2()?;
    let artifact = codex_artifact_from_inventory(&manifest, &inventory)?;

    // Download + local digest verification.
    let local_staged = cached_codex_package(&state.data_root(), artifact, &mut progress).await?;
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

fn codex_artifact_from_inventory<'a>(
    manifest: &'a ReleaseManifest,
    inventory: &serde_json::Value,
) -> AppResult<&'a CodexArtifact> {
    let arch = inventory
        .pointer("/system/arch")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| AppError::Message("host inventory missing system.arch".into()))?;
    let os = inventory
        .pointer("/system/os")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| AppError::Message("host inventory missing system.os".into()))?;
    artifact_for_os_arch(&manifest.codex.artifacts, os, arch)
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
    let status = client.host_status().ok();
    let session = client.codex_session_status(None).ok();
    crate::remote::observation::require_idle_for_destructive_op(
        session.as_ref(),
        status.as_ref().and_then(|value| value.get("nativeCodex")),
    )
    .map_err(|code| AppError::Message(format!("ComponentUpdateBlocked: {code}")))?;
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
    let os = inventory
        .pointer("/system/os")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("linux");
    let proxy_backend = inventory
        .pointer("/proxyBackend")
        .or_else(|| inventory.pointer("/proxy/proxyBackend"))
        .and_then(serde_json::Value::as_str);
    let running_native_digest = inventory
        .pointer("/proxy/nativeExecutableDigest")
        .and_then(serde_json::Value::as_str);
    let native = proxy_backend == Some("native");
    let native_proxy_artifact = if native {
        Some(component_artifact_for_os_arch(
            &manifest,
            "vellum-proxy-daemon",
            os,
            arch,
        )?)
    } else {
        None
    };
    let native_proxy_stale = native_proxy_artifact.as_ref().is_some_and(|artifact| {
        native_proxy_component_update_needed(proxy_backend, running_native_digest, &artifact.sha256)
    });

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
            "proxyBackend": proxy_backend,
            "runningNativeDigest": running_native_digest,
            "current": if native {
                !native_proxy_stale
            } else {
                running_proxy_image == Some(manifest.proxy.image.as_str())
            },
            "note": if native {
                "native proxy updates through proxy.update and service manager reload"
            } else {
                "proxy image switches on the next deployment apply"
            },
        },
        "steps": [],
    });
    let agent_current = running_agent == manifest.agent.version;
    if agent_current && !native_proxy_stale {
        let steps = report
            .get_mut("steps")
            .and_then(serde_json::Value::as_array_mut)
            .expect("steps array");
        steps.push("agent.alreadyCurrent".into());
        steps.push("broker.notUpdated".into());
        steps.push(
            if native {
                "proxy.alreadyCurrent"
            } else {
                "proxy.updatedOnNextApply"
            }
            .into(),
        );
        return Ok(report);
    }
    let mut agent_update = None;
    let mut proxy_update = None;
    {
        let steps = report
            .get_mut("steps")
            .and_then(serde_json::Value::as_array_mut)
            .expect("steps array");
        if !agent_current {
            let artifact = artifact_for_os_arch(&manifest.agent.artifacts, os, arch)?;
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
            agent_update = Some(update_result?);
            steps.push("agent.updated".into());
        } else {
            steps.push("agent.alreadyCurrent".into());
        }
        steps.push("broker.notUpdated".into());
        if native_proxy_stale {
            let artifact = native_proxy_artifact.expect("native proxy artifact");
            let local_staged = download_artifact(&artifact)?;
            let update_result = (|| -> AppResult<serde_json::Value> {
                let bytes = std::fs::read(&local_staged).map_err(|error| {
                    AppError::Message(format!("read native proxy artifact: {error}"))
                })?;
                let remote_path = format!("/tmp/vellum-proxy-stage-{}.bin", ulid::Ulid::new());
                client.stage_artifact(&remote_path, &bytes)?;
                let update = client.proxy_update(
                    &format!("{operation_id}-proxy"),
                    &remote_path,
                    Some(&artifact.sha256),
                );
                client.remove_remote_file(&remote_path);
                update
            })();
            let _ = std::fs::remove_file(&local_staged);
            proxy_update = Some(update_result?);
            steps.push("proxy.updatedNative".into());
        } else if native {
            steps.push("proxy.alreadyCurrent".into());
        } else {
            steps.push("proxy.updatedOnNextApply".into());
        }
    }
    if let Some(update) = agent_update {
        report["update"] = update;
    }
    if let Some(update) = proxy_update {
        report["proxyUpdate"] = update;
    }
    Ok(report)
}

pub(crate) fn native_proxy_component_update_needed(
    proxy_backend: Option<&str>,
    running_digest: Option<&str>,
    desired_digest: &str,
) -> bool {
    proxy_backend == Some("native")
        && !desired_digest.is_empty()
        && !running_digest.is_some_and(|digest| digest.eq_ignore_ascii_case(desired_digest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package_fixture(path: &Path, omit_helper: bool) -> CodexArtifact {
        let gzip = flate2::write::GzEncoder::new(
            std::fs::File::create(path).unwrap(),
            flate2::Compression::default(),
        );
        let mut archive = tar::Builder::new(gzip);
        for dir in ["bin", "codex-resources", "codex-path"] {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_mode(0o755);
            header.set_cksum();
            archive
                .append_data(&mut header, dir, std::io::empty())
                .unwrap();
        }
        for (name, body) in [
            ("bin/codex", "runtime"),
            ("bin/codex-code-mode-host", "helper"),
            ("codex-path/rg", "rg"),
            ("codex-resources/bwrap", "sandbox"),
            (
                "codex-package.json",
                r#"{"layoutVersion":1,"version":"0.147.0","target":"x86_64-unknown-linux-musl","entrypoint":"bin/codex","resourcesDir":"codex-resources","pathDir":"codex-path"}"#,
            ),
        ] {
            if omit_helper && name == "bin/codex-code-mode-host" {
                continue;
            }
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            archive
                .append_data(&mut header, name, body.as_bytes())
                .unwrap();
        }
        archive.into_inner().unwrap().finish().unwrap();
        CodexArtifact { url: "https://github.com/openai/codex/releases/download/rust-v0.147.0/codex-package-x86_64-unknown-linux-musl.tar.gz".into(), sha256: sha256_file(path).unwrap(), size: std::fs::metadata(path).unwrap().len(), uncompressed_size: 4096 }
    }

    #[tokio::test]
    async fn codex_cache_verifies_once_per_use_and_reuses_without_downloading() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.tgz");
        let artifact = package_fixture(&source, false);
        let cached = cached_codex_package_with(temp.path(), &artifact, |dest| async {
            std::fs::copy(&source, dest).unwrap();
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(
            cached.file_name().unwrap().to_str().unwrap(),
            format!("{}.tar.gz", artifact.sha256)
        );
        let reused = cached_codex_package_with(temp.path(), &artifact, |_| async {
            panic!("cache hit must not fetch")
        })
        .await
        .unwrap();
        assert_eq!(cached, reused);
        std::fs::write(&cached, "corrupt").unwrap();
        let downloads = std::cell::Cell::new(0);
        let repaired = cached_codex_package_with(temp.path(), &artifact, |dest| async {
            downloads.set(downloads.get() + 1);
            assert!(
                !cached.exists(),
                "corrupt entry must be removed before fetching"
            );
            std::fs::copy(&source, dest).unwrap();
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(downloads.get(), 1);
        assert_eq!(repaired, cached);
        verify_codex_package(&repaired, &artifact).unwrap();
    }

    #[tokio::test]
    async fn failed_or_invalid_package_never_enters_cache_and_deployment_can_retry() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.tgz");
        let artifact = package_fixture(&source, false);
        let error = cached_codex_package_with(temp.path(), &artifact, |dest| async {
            std::fs::write(dest, "interrupted").unwrap();
            Err(AppError::Message(
                "CodexPackageDownloadFailed: retry deployment".into(),
            ))
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("CodexPackageDownloadFailed"));
        assert_eq!(
            std::fs::read_dir(temp.path().join("remote-cache/codex"))
                .unwrap()
                .count(),
            0
        );
        assert!(
            cached_codex_package_with(temp.path(), &artifact, |dest| async {
                std::fs::write(dest, "wrong digest").unwrap();
                Ok(())
            })
            .await
            .is_err()
        );
        let incomplete = package_fixture(&source, true);
        let error = cached_codex_package_with(temp.path(), &incomplete, |dest| async {
            std::fs::copy(&source, dest).unwrap();
            Ok(())
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("CodexPackageVerificationFailed"));
        assert_eq!(
            std::fs::read_dir(temp.path().join("remote-cache/codex"))
                .unwrap()
                .count(),
            0
        );
        package_fixture(&source, false);
        cached_codex_package_with(temp.path(), &artifact, |dest| async {
            std::fs::copy(&source, dest).unwrap();
            Ok(())
        })
        .await
        .unwrap();
    }

    #[test]
    fn https_package_preflight_uses_signed_sizes_without_fetching() {
        let mut artifact = CodexArtifact {
            url: "https://example.invalid/codex-package.tar.gz".into(),
            sha256: "a".repeat(64),
            size: 100,
            uncompressed_size: 1000,
        };
        assert_eq!(inspect_codex_package_sizes(&artifact).unwrap(), (100, 1000));
        artifact.uncompressed_size = 0;
        assert!(inspect_codex_package_sizes(&artifact).is_err());
        artifact.url = "http://example.invalid/package".into();
        assert!(inspect_codex_package_sizes(&artifact).is_err());
    }

    #[test]
    fn local_artifact_inspection_checks_digest_before_deployment_and_never_stages() {
        let temp = tempfile::tempdir().unwrap();
        let artifact = temp.path().join("artifact");
        std::fs::write(&artifact, b"signed bytes").unwrap();
        let digest = sha256_file(&artifact).unwrap();
        assert_eq!(inspect_artifact_file(&artifact, &digest).unwrap(), 12);
        assert!(inspect_artifact_file(&artifact, "wrong")
            .unwrap_err()
            .to_string()
            .contains("ReleaseArtifactDigestMismatch"));
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
        assert!(inspect_bundled_artifact(&CodexArtifact {
            url: "https://example.invalid/artifact".into(),
            sha256: digest,
            ..CodexArtifact::default()
        })
        .unwrap_err()
        .to_string()
        .contains("bundledReleaseUnavailable"));
    }

    #[test]
    fn schema5_native_linux_manifest_omits_broker_and_image_and_fails_closed_on_archives() {
        let mut value = serde_json::to_value(manifest_fixture()).unwrap();
        value["schemaVersion"] = 5.into();
        value["protocolVersion"] = 5.into();
        value.as_object_mut().unwrap().remove("broker");
        value["proxy"].as_object_mut().unwrap().remove("image");
        value["proxy"]["artifacts"]["linux-x64"]["url"] =
            "bundle://linux-amd64/vellum-proxy-daemon".into();
        value["proxy"]["artifacts"]["linux-arm64"]["url"] =
            "bundle://linux-arm64/vellum-proxy-daemon".into();
        let manifest: ReleaseManifest = serde_json::from_value(value.clone()).unwrap();
        assert!(manifest.validate().is_ok());
        assert!(manifest.broker.version.is_empty());
        assert!(manifest.proxy.image.is_empty());
        assert_eq!(
            manifest.resource_digest("linux-amd64/vellum-proxy-daemon"),
            Some("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee")
        );
        assert!(manifest
            .resource_digest("linux-amd64/vellum-remote-broker")
            .is_none());
        assert!(
            component_artifact_for_os_arch(&manifest, "vellum-proxy-daemon", "linux", "arm64")
                .is_ok()
        );
        value["proxy"]["artifacts"]["linux-x64"]["url"] =
            "bundle://linux-amd64/proxy-image.tar".into();
        assert!(serde_json::from_value::<ReleaseManifest>(value)
            .unwrap()
            .validate()
            .is_err());
        let mut invalid_digest = manifest.clone();
        invalid_digest.proxy.artifacts.linux_x64.sha256 = "bad".into();
        assert!(invalid_digest.validate().is_err());
        let mut invalid_protocol = manifest;
        invalid_protocol.protocol_version = 4;
        assert!(invalid_protocol.validate().is_err());
        let legacy = manifest_fixture();
        assert!(
            component_artifact_for_os_arch(&legacy, "vellum-proxy-daemon", "linux", "x86_64")
                .is_err()
        );
    }

    #[test]
    fn schema4_remains_readable_with_darwin_artifacts() {
        let mut manifest = manifest_fixture();
        manifest.schema_version = 4;
        manifest.protocol_version = 4;
        manifest.codex.artifacts.darwin_arm64 = Some(CodexArtifact {
            url: "bundle://darwin-arm64/codex".into(),
            sha256: "a".repeat(64),
            ..CodexArtifact::default()
        });
        manifest.agent.artifacts.darwin_arm64 = Some(CodexArtifact {
            url: "bundle://darwin-arm64/vellum-remote-agent".into(),
            sha256: "b".repeat(64),
            ..CodexArtifact::default()
        });
        manifest.proxy.artifacts.darwin_arm64 = Some(CodexArtifact {
            url: "bundle://darwin-arm64/vellum-proxy-daemon".into(),
            sha256: "c".repeat(64),
            ..CodexArtifact::default()
        });
        let decoded: ReleaseManifest =
            serde_json::from_slice(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(decoded.validate().is_ok());
        assert_eq!(
            component_artifact_for_os_arch(&decoded, "vellum-proxy-daemon", "darwin", "arm64")
                .unwrap()
                .sha256,
            "c".repeat(64)
        );
    }

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
                        ..CodexArtifact::default()
                    },
                    linux_arm64: CodexArtifact {
                        url: "https://example.invalid/arm64".into(),
                        sha256: "b".repeat(64),
                        ..CodexArtifact::default()
                    },
                    darwin_arm64: None,
                },
            },
            agent: ComponentManifestEntry {
                version: "0.1.0".into(),
                artifacts: ArtifactSet {
                    linux_x64: CodexArtifact {
                        url: "https://example.invalid/agent-x64".into(),
                        sha256: "c".repeat(64),
                        ..CodexArtifact::default()
                    },
                    linux_arm64: CodexArtifact {
                        url: "https://example.invalid/agent-arm64".into(),
                        sha256: "c".repeat(64),
                        ..CodexArtifact::default()
                    },
                    darwin_arm64: None,
                },
            },
            broker: ComponentManifestEntry {
                version: "0.1.0".into(),
                artifacts: ArtifactSet {
                    linux_x64: CodexArtifact {
                        url: "https://example.invalid/broker-x64".into(),
                        sha256: "d".repeat(64),
                        ..CodexArtifact::default()
                    },
                    linux_arm64: CodexArtifact {
                        url: "https://example.invalid/broker-arm64".into(),
                        sha256: "d".repeat(64),
                        ..CodexArtifact::default()
                    },
                    darwin_arm64: None,
                },
            },
            proxy: ProxyManifestEntry {
                image: "vellum-proxy:0.2.2".into(),
                artifacts: ArtifactSet {
                    linux_x64: CodexArtifact {
                        url: "bundle://linux-amd64/proxy-image.tar".into(),
                        sha256: "e".repeat(64),
                        ..CodexArtifact::default()
                    },
                    linux_arm64: CodexArtifact {
                        url: "bundle://linux-arm64/proxy-image.tar".into(),
                        sha256: "f".repeat(64),
                        ..CodexArtifact::default()
                    },
                    darwin_arm64: None,
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
            Err("ReleaseManifestSchemaUnsupported: expected 3, 4 or 5, got 1".into())
        );
        let mut schema3 = manifest_fixture();
        schema3.schema_version = 3;
        assert!(schema3.validate().is_ok());
        let mut schema4 = manifest_fixture();
        schema4.schema_version = 4;
        assert_eq!(
            schema4.validate(),
            Err("ReleaseManifestInvalid: schema 4 requires Darwin ARM64 artifacts".into())
        );
    }

    #[test]
    fn os_plus_arch_picks_darwin_arm64_and_rejects_intel_mac() {
        let mut manifest = manifest_fixture();
        manifest.codex.artifacts.darwin_arm64 = Some(CodexArtifact {
            url: "bundle://darwin-arm64/codex".into(),
            sha256: "1".repeat(64),
            ..CodexArtifact::default()
        });
        assert_eq!(
            artifact_for_os_arch(&manifest.codex.artifacts, "darwin", "arm64")
                .unwrap()
                .url,
            "bundle://darwin-arm64/codex"
        );
        assert!(artifact_for_os_arch(&manifest.codex.artifacts, "darwin", "x86_64").is_err());
        assert!(artifact_for_os_arch(&manifest.codex.artifacts, "linux", "amd64").is_ok());

        let inventory = serde_json::json!({
            "system": {"os": "Darwin", "arch": "arm64"}
        });
        assert_eq!(
            codex_artifact_from_inventory(&manifest, &inventory)
                .unwrap()
                .url,
            "bundle://darwin-arm64/codex"
        );
        assert!(codex_artifact_from_inventory(
            &manifest,
            &serde_json::json!({"system": {"arch": "arm64"}})
        )
        .is_err());
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
    fn update_remote_components_gates_incomplete_observation_before_replace() {
        let source = include_str!("pinned_install.rs");
        let fn_at = source
            .find("pub fn update_remote_components(")
            .expect("update_remote_components");
        let body = &source[fn_at..];
        let gate_at = body
            .find("require_idle_for_destructive_op(")
            .expect("update must call require_idle_for_destructive_op");
        let agent_at = body.find("client.agent_update(");
        let proxy_at = body.find("client.proxy_update(");
        assert!(
            agent_at.is_none_or(|at| gate_at < at),
            "observation gate must precede agent.update"
        );
        assert!(
            proxy_at.is_none_or(|at| gate_at < at),
            "observation gate must precede proxy.update"
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

    #[test]
    fn native_proxy_component_update_is_digest_gated_and_legacy_docker_stays_next_apply() {
        let digest = "a".repeat(64);
        let other = "b".repeat(64);
        assert!(native_proxy_component_update_needed(
            Some("native"),
            None,
            &digest
        ));
        assert!(native_proxy_component_update_needed(
            Some("native"),
            Some(other.as_str()),
            &digest
        ));
        assert!(!native_proxy_component_update_needed(
            Some("native"),
            Some(&digest),
            &digest
        ));
        assert!(!native_proxy_component_update_needed(
            Some("docker"),
            None,
            &digest
        ));
        assert!(!native_proxy_component_update_needed(
            Some("native"),
            None,
            ""
        ));
    }
}
