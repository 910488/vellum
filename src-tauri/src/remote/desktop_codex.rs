//! Desktop Codex protocol identity and official Linux runtime synchronization.
//!
//! The Desktop-bundled core is the authority. A remote binary is never chosen
//! by `latest`: Vellum resolves the exact OpenAI release tag, requires the
//! publisher-provided GitHub asset digest, stages the binary, and lets the
//! Remote Agent run its protocol probe before an atomic swap.

use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{AppError, AppResult};
use crate::remote::agent_client::RemoteAgentClient;
use crate::remote::process::background_command;
use crate::state::AppState;

const MIN_DYNAMIC_UPDATE_AGENT_PROTOCOL: u64 = 3;
const MAX_CODEX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DesktopCodexIdentity {
    pub version: String,
    pub schema_sha256: String,
    pub binary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DesktopCodexCompatibilityStatus {
    pub state: String,
    pub desktop_version: Option<String>,
    pub desktop_schema_sha256: Option<String>,
    pub remote_version: Option<String>,
    pub required_remote_version: Option<String>,
    pub remote_arch: Option<String>,
    pub agent_protocol: Option<u64>,
    pub can_update: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct QualificationRecord {
    schema_version: u32,
    host_id: String,
    desktop_version: String,
    desktop_schema_sha256: String,
    remote_version: String,
    remote_binary_sha256: String,
    qualified_at: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubAsset {
    name: String,
    digest: Option<String>,
    browser_download_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OfficialCodexArchive {
    version: String,
    asset_name: String,
    binary_name: String,
    url: String,
    sha256: String,
}

pub async fn compatibility_status(
    state: &AppState,
    host_id: &str,
) -> AppResult<DesktopCodexCompatibilityStatus> {
    let identity = match desktop_identity() {
        Ok(identity) => identity,
        Err(error) => {
            return Ok(DesktopCodexCompatibilityStatus {
                state: "desktopUnavailable".into(),
                desktop_version: None,
                desktop_schema_sha256: None,
                remote_version: None,
                required_remote_version: None,
                remote_arch: None,
                agent_protocol: None,
                can_update: false,
                detail: error.to_string(),
            })
        }
    };
    let target = crate::remote::RemoteHostManager::resolve_target(state, host_id)?;
    let client = RemoteAgentClient::new(target);
    // M35: share the cached host.managerSnapshot with the other Remote
    // Manager probes this refresh cycle instead of a second independent
    // `host.inventoryV2` SSH call. Falls back to the plain RPC for older
    // agents that don't support the snapshot method yet. This is a
    // read-only status display, not the mutation path (see
    // `update_remote_for_desktop` below), so a briefly-cached inventory is
    // fine here.
    let expected_account_id = super::desktop_control_account_id(state);
    let inventory = match state.remote().host_manager_snapshot(host_id, || {
        client.host_manager_snapshot(expected_account_id.as_deref(), None)
    }) {
        Ok((snapshot, _generation)) => snapshot
            .get("inventory")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        Err(_) => client.host_inventory_v2()?,
    };
    let remote_version = inventory
        .pointer("/codex/version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let arch = inventory
        .pointer("/system/arch")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let agent_protocol = inventory
        .get("agentProtocol")
        .and_then(serde_json::Value::as_u64);
    let installation = client
        .codex_verify_installation(Some(&identity.version))
        .ok();
    let live_binary_sha256 = installation
        .as_ref()
        .and_then(|value| value.get("sha256"))
        .and_then(serde_json::Value::as_str);
    let record = load_record(&state.data_root(), host_id).ok().flatten();
    let qualified = record.as_ref().is_some_and(|record| {
        record.desktop_version == identity.version
            && record.desktop_schema_sha256 == identity.schema_sha256
            && live_binary_sha256 == Some(record.remote_binary_sha256.as_str())
            && remote_version.as_deref().and_then(version_token)
                == Some(record.remote_version.as_str())
    });
    if qualified {
        return Ok(DesktopCodexCompatibilityStatus {
            state: "current".into(),
            desktop_version: Some(identity.version.clone()),
            desktop_schema_sha256: Some(identity.schema_sha256),
            remote_version,
            required_remote_version: Some(identity.version),
            remote_arch: arch,
            agent_protocol,
            can_update: false,
            detail: "Remote Codex passed the current Desktop protocol qualification".into(),
        });
    }
    if agent_protocol.unwrap_or_default() < MIN_DYNAMIC_UPDATE_AGENT_PROTOCOL {
        return Ok(DesktopCodexCompatibilityStatus {
            state: "agentUpdateRequired".into(),
            desktop_version: Some(identity.version.clone()),
            desktop_schema_sha256: Some(identity.schema_sha256),
            remote_version,
            required_remote_version: Some(identity.version),
            remote_arch: arch,
            agent_protocol,
            can_update: false,
            detail: format!(
                "Remote Agent protocol {} cannot perform a staged Desktop protocol qualification; update the Agent first",
                agent_protocol.unwrap_or_default()
            ),
        });
    }
    let Some(arch) = arch else {
        return Ok(DesktopCodexCompatibilityStatus {
            state: "unavailable".into(),
            desktop_version: Some(identity.version.clone()),
            desktop_schema_sha256: Some(identity.schema_sha256),
            remote_version,
            required_remote_version: Some(identity.version),
            remote_arch: None,
            agent_protocol,
            can_update: false,
            detail: "Remote architecture is unavailable".into(),
        });
    };
    match resolve_official_archive(&identity.version, &arch).await {
        Ok(_) => Ok(DesktopCodexCompatibilityStatus {
            state: if remote_version.as_deref().and_then(version_token)
                == Some(identity.version.as_str())
            {
                "qualificationRequired".into()
            } else {
                "updateAvailable".into()
            },
            desktop_version: Some(identity.version.clone()),
            desktop_schema_sha256: Some(identity.schema_sha256),
            remote_version,
            required_remote_version: Some(identity.version),
            remote_arch: Some(arch),
            agent_protocol,
            can_update: true,
            detail: "An exact OpenAI Linux release is available for protocol qualification".into(),
        }),
        Err(error) => Ok(DesktopCodexCompatibilityStatus {
            state: "unavailable".into(),
            desktop_version: Some(identity.version.clone()),
            desktop_schema_sha256: Some(identity.schema_sha256),
            remote_version,
            required_remote_version: Some(identity.version),
            remote_arch: Some(arch),
            agent_protocol,
            can_update: false,
            detail: error.to_string(),
        }),
    }
}

pub async fn update_remote_for_desktop(
    state: &AppState,
    host_id: &str,
    operation_id: &str,
) -> AppResult<serde_json::Value> {
    let identity = desktop_identity()?;
    let target = crate::remote::RemoteHostManager::resolve_target(state, host_id)?;
    let client = RemoteAgentClient::new(target);
    let inventory = client.host_inventory_v2()?;
    let agent_protocol = inventory
        .get("agentProtocol")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    if agent_protocol < MIN_DYNAMIC_UPDATE_AGENT_PROTOCOL {
        return Err(AppError::Message(format!(
            "RemoteCodexAgentUpdateRequired: protocol {agent_protocol}; expected at least {MIN_DYNAMIC_UPDATE_AGENT_PROTOCOL}"
        )));
    }
    let arch = inventory
        .pointer("/system/arch")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| AppError::Message("remote inventory missing system.arch".into()))?;
    let archive = resolve_official_archive(&identity.version, arch).await?;
    let extracted = download_and_extract(&archive).await?;
    let binary_sha256 = sha256_file(extracted.path())?;
    let bytes = fs::read(extracted.path())
        .map_err(|error| AppError::Message(format!("read extracted Codex binary: {error}")))?;
    let remote_path = format!("/tmp/vellum-codex-desktop-sync-{}.bin", ulid::Ulid::new());
    crate::remote::provision_remote_boundary_key_without_native_restart(
        &client,
        &state.data_root(),
        host_id,
        operation_id,
    )?;
    let install = (|| -> AppResult<serde_json::Value> {
        client.stage_artifact(&remote_path, &bytes)?;
        client.codex_install_pinned(
            operation_id,
            &remote_path,
            &binary_sha256,
            &identity.version,
        )
    })();
    client.remove_remote_file(&remote_path);
    let install = install?;
    let boundary_key = crate::proxy::ensure_remote_boundary_key(&state.data_root(), host_id)?;
    let restart =
        client.codex_restart_native(&crate::remote::boundary_key::restart_operation_id(
            operation_id,
            crate::proxy::BoundaryKeyConsumer::NativeCodex,
            &boundary_key,
        ))?;
    crate::remote::confirm_remote_boundary_key_consumers(
        &client,
        &state.data_root(),
        host_id,
        &[crate::proxy::BoundaryKeyConsumer::NativeCodex],
        true,
    )?;
    save_record(
        &state.data_root(),
        &QualificationRecord {
            schema_version: 1,
            host_id: host_id.into(),
            desktop_version: identity.version.clone(),
            desktop_schema_sha256: identity.schema_sha256.clone(),
            remote_version: identity.version.clone(),
            remote_binary_sha256: binary_sha256.clone(),
            qualified_at: chrono::Utc::now().to_rfc3339(),
        },
    )?;
    Ok(serde_json::json!({
        "hostId": host_id,
        "desktopVersion": identity.version,
        "desktopSchemaSha256": identity.schema_sha256,
        "remoteBinarySha256": binary_sha256,
        "source": "openai/codex GitHub release",
        "install": install,
        "restart": restart,
    }))
}

/// Binary identity used to key the desktop identity/schema cache below:
/// path, size, and mtime. Cheap to `stat()` on every call, and changes
/// exactly when the bundled Codex core is actually replaced (an app
/// update), so a cache hit here is always correct — never a staleness
/// tradeoff — unlike a TTL-based cache would be.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DesktopBinaryIdentity {
    path: PathBuf,
    len: u64,
    modified: std::time::SystemTime,
}

static DESKTOP_IDENTITY_CACHE: std::sync::OnceLock<
    std::sync::Mutex<Option<(DesktopBinaryIdentity, DesktopCodexIdentity)>>,
> = std::sync::OnceLock::new();

/// Probes the bundled Codex core's version and schema hash. Both require
/// spawning a subprocess (`--version`, and `app-server generate-json-schema`
/// which writes real schema files to a temp dir and hashes them back) — real
/// work callers like `compatibility_status` used to repeat on every refresh.
/// Cached by [`DesktopBinaryIdentity`]: unless the bundled binary itself
/// changed (an app update), the previous probe result is still correct.
pub fn desktop_identity() -> AppResult<DesktopCodexIdentity> {
    let binary = desktop_binary().ok_or_else(|| {
        AppError::Message("DesktopCodexUnavailable: bundled Codex core was not found".into())
    })?;
    let metadata = fs::metadata(&binary)
        .map_err(|error| AppError::Message(format!("stat bundled Codex core: {error}")))?;
    let modified = metadata
        .modified()
        .map_err(|error| AppError::Message(format!("stat bundled Codex core: {error}")))?;
    let key = DesktopBinaryIdentity {
        path: binary.clone(),
        len: metadata.len(),
        modified,
    };

    let cache = DESKTOP_IDENTITY_CACHE.get_or_init(|| std::sync::Mutex::new(None));
    if let Some((cached_key, cached_identity)) = cache.lock().expect("state poisoned").as_ref() {
        if cached_key == &key {
            return Ok(cached_identity.clone());
        }
    }

    let output = background_command(&binary)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            AppError::Message(format!("Desktop Codex version probe failed: {error}"))
        })?;
    if !output.status.success() {
        return Err(AppError::Message(
            "Desktop Codex version probe returned a failure".into(),
        ));
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let version = version_token(&text)
        .filter(|version| valid_version(version))
        .ok_or_else(|| AppError::Message("Desktop Codex reported an invalid version".into()))?
        .to_string();
    let schema_sha256 = desktop_schema_hash(&binary)?;
    let identity = DesktopCodexIdentity {
        version,
        schema_sha256,
        binary: binary.to_string_lossy().to_string(),
    };
    *cache.lock().expect("state poisoned") = Some((key, identity.clone()));
    Ok(identity)
}

#[cfg(target_os = "windows")]
fn desktop_binary() -> Option<PathBuf> {
    let root = dirs::data_local_dir()?
        .join("OpenAI")
        .join("Codex")
        .join("bin");
    newest_codex_binary(&root, "codex.exe")
}

#[cfg(target_os = "macos")]
fn desktop_binary() -> Option<PathBuf> {
    crate::install_paths::macos_codex_desktop_cli_candidates(dirs::home_dir().as_deref())
        .into_iter()
        .find(|path| path.is_file())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn desktop_binary() -> Option<PathBuf> {
    None
}

#[cfg(target_os = "windows")]
fn newest_codex_binary(root: &Path, name: &str) -> Option<PathBuf> {
    let mut candidates = fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path().join(name);
            let modified = path.metadata().ok()?.modified().ok()?;
            path.is_file().then_some((modified, path))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    candidates.into_iter().next().map(|(_, path)| path)
}

fn desktop_schema_hash(binary: &Path) -> AppResult<String> {
    let output = tempfile::tempdir()
        .map_err(|error| AppError::Message(format!("create schema probe directory: {error}")))?;
    let status = background_command(binary)
        .args(["app-server", "generate-json-schema", "--out"])
        .arg(output.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .map_err(|error| AppError::Message(format!("Desktop schema probe failed: {error}")))?;
    if !status.success() {
        return Err(AppError::Message(format!(
            "DesktopCodexSchemaUnavailable: app-server schema generation returned {status}"
        )));
    }
    let mut files = Vec::new();
    collect_files(output.path(), output.path(), &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    if files.is_empty() {
        return Err(AppError::Message(
            "DesktopCodexSchemaUnavailable: schema output was empty".into(),
        ));
    }
    let mut hasher = Sha256::new();
    for (relative, path) in files {
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hasher.update(fs::read(path).map_err(|error| AppError::Message(error.to_string()))?);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn collect_files(root: &Path, current: &Path, files: &mut Vec<(String, PathBuf)>) -> AppResult<()> {
    for entry in fs::read_dir(current).map_err(|error| AppError::Message(error.to_string()))? {
        let entry = entry.map_err(|error| AppError::Message(error.to_string()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, files)?;
        } else if path.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| AppError::Message(error.to_string()))?
                .to_string_lossy()
                .replace('\\', "/");
            files.push((relative, path));
        }
    }
    Ok(())
}

async fn resolve_official_archive(version: &str, arch: &str) -> AppResult<OfficialCodexArchive> {
    if !valid_version(version) {
        return Err(AppError::Message("DesktopCodexVersionUnsafe".into()));
    }
    let target = match arch {
        "aarch64" | "arm64" => "aarch64-unknown-linux-musl",
        "x86_64" | "amd64" => "x86_64-unknown-linux-musl",
        other => {
            return Err(AppError::Message(format!(
                "DesktopCodexUnsupportedRemoteArch: {other}"
            )))
        }
    };
    let asset_name = format!("codex-{target}.tar.gz");
    let api_url =
        format!("https://api.github.com/repos/openai/codex/releases/tags/rust-v{version}");
    let response = http_client()?.get(api_url).send().await.map_err(|error| {
        AppError::Message(format!("OpenAI Codex release lookup failed: {error}"))
    })?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(AppError::Message(format!(
            "DesktopCodexReleaseUnavailable: OpenAI has not published Linux artifacts for {version}"
        )));
    }
    let response = response.error_for_status().map_err(|error| {
        AppError::Message(format!("OpenAI Codex release lookup failed: {error}"))
    })?;
    let release: GitHubRelease = response
        .json()
        .await
        .map_err(|error| AppError::Message(format!("invalid OpenAI release metadata: {error}")))?;
    select_official_archive(version, &asset_name, release)
}

fn select_official_archive(
    version: &str,
    asset_name: &str,
    release: GitHubRelease,
) -> AppResult<OfficialCodexArchive> {
    let expected_tag = format!("rust-v{version}");
    if release.tag_name != expected_tag {
        return Err(AppError::Message("DesktopCodexReleaseTagMismatch".into()));
    }
    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name == asset_name)
        .ok_or_else(|| {
            AppError::Message(format!(
                "DesktopCodexArtifactUnavailable: {asset_name} is missing"
            ))
        })?;
    let digest = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| valid_sha256(digest))
        .ok_or_else(|| AppError::Message("DesktopCodexArtifactDigestMissing".into()))?;
    let expected_prefix =
        format!("https://github.com/openai/codex/releases/download/{expected_tag}/");
    if asset.browser_download_url != format!("{expected_prefix}{asset_name}") {
        return Err(AppError::Message("DesktopCodexArtifactUrlMismatch".into()));
    }
    Ok(OfficialCodexArchive {
        version: version.into(),
        binary_name: asset_name.trim_end_matches(".tar.gz").into(),
        asset_name: asset_name.into(),
        url: asset.browser_download_url,
        sha256: digest.into(),
    })
}

async fn download_and_extract(
    archive: &OfficialCodexArchive,
) -> AppResult<tempfile::NamedTempFile> {
    let response = http_client()?
        .get(&archive.url)
        .send()
        .await
        .map_err(|error| AppError::Message(format!("Codex archive download failed: {error}")))?
        .error_for_status()
        .map_err(|error| AppError::Message(format!("Codex archive download failed: {error}")))?;
    if response
        .content_length()
        .is_some_and(|size| size > MAX_CODEX_ARCHIVE_BYTES as u64)
    {
        return Err(AppError::Message("DesktopCodexArtifactTooLarge".into()));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| AppError::Message(format!("read Codex archive: {error}")))?;
    if bytes.len() > MAX_CODEX_ARCHIVE_BYTES {
        return Err(AppError::Message("DesktopCodexArtifactTooLarge".into()));
    }
    let resolved = hex::encode(Sha256::digest(&bytes));
    if resolved != archive.sha256 {
        return Err(AppError::Message(format!(
            "DesktopCodexArtifactDigestMismatch: expected {}, resolved {resolved}",
            archive.sha256
        )));
    }
    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut tar = tar::Archive::new(decoder);
    let mut output = tempfile::NamedTempFile::new()
        .map_err(|error| AppError::Message(format!("create Codex staging file: {error}")))?;
    let mut found = false;
    for entry in tar
        .entries()
        .map_err(|error| AppError::Message(format!("read Codex archive: {error}")))?
    {
        let mut entry = entry
            .map_err(|error| AppError::Message(format!("read Codex archive entry: {error}")))?;
        let path = entry
            .path()
            .map_err(|error| AppError::Message(format!("read Codex archive path: {error}")))?;
        if path == Path::new(&archive.binary_name) {
            if found || !entry.header().entry_type().is_file() {
                return Err(AppError::Message("DesktopCodexArchiveInvalid".into()));
            }
            std::io::copy(&mut entry, &mut output)
                .map_err(|error| AppError::Message(format!("extract Codex binary: {error}")))?;
            found = true;
        }
    }
    if !found {
        return Err(AppError::Message(format!(
            "DesktopCodexArchiveMissingBinary: {}",
            archive.binary_name
        )));
    }
    output
        .flush()
        .map_err(|error| AppError::Message(format!("flush Codex binary: {error}")))?;
    Ok(output)
}

fn http_client() -> AppResult<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("vellum-desktop-codex-compatibility")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|error| AppError::Message(format!("build update client: {error}")))
}

fn record_path(root: &Path, host_id: &str) -> PathBuf {
    let id = hex::encode(Sha256::digest(host_id.as_bytes()));
    root.join("remote-codex-qualifications")
        .join(format!("{id}.json"))
}

fn load_record(root: &Path, host_id: &str) -> AppResult<Option<QualificationRecord>> {
    let path = record_path(root, host_id);
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| AppError::Message(format!("invalid qualification record: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::Message(format!(
            "read qualification record: {error}"
        ))),
    }
}

fn save_record(root: &Path, record: &QualificationRecord) -> AppResult<()> {
    let path = record_path(root, &record.host_id);
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Message("qualification record has no parent".into()))?;
    fs::create_dir_all(parent)
        .map_err(|error| AppError::Message(format!("create qualification directory: {error}")))?;
    fs::write(
        &path,
        serde_json::to_vec_pretty(record).map_err(|error| AppError::Message(error.to_string()))?,
    )
    .map_err(|error| AppError::Message(format!("write qualification record: {error}")))
}

fn sha256_file(path: &Path) -> AppResult<String> {
    let mut file = fs::File::open(path)
        .map_err(|error| AppError::Message(format!("open Codex binary: {error}")))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| AppError::Message(format!("hash Codex binary: {error}")))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn version_token(value: &str) -> Option<&str> {
    value.split_whitespace().find(|part| {
        part.chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
    })
}

fn valid_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_selection_requires_exact_tag_asset_url_and_publisher_digest() {
        let version = "0.147.0-alpha.6.5";
        let name = "codex-aarch64-unknown-linux-musl.tar.gz";
        let url =
            format!("https://github.com/openai/codex/releases/download/rust-v{version}/{name}");
        let release = GitHubRelease {
            tag_name: format!("rust-v{version}"),
            assets: vec![GitHubAsset {
                name: name.into(),
                digest: Some(format!("sha256:{}", "a".repeat(64))),
                browser_download_url: url.clone(),
            }],
        };
        let selected = select_official_archive(version, name, release).unwrap();
        assert_eq!(selected.url, url);
        assert_eq!(selected.sha256, "a".repeat(64));
        assert_eq!(selected.binary_name, "codex-aarch64-unknown-linux-musl");
    }

    #[test]
    fn release_selection_rejects_redirected_or_digestless_assets() {
        let release = GitHubRelease {
            tag_name: "rust-v0.147.0".into(),
            assets: vec![GitHubAsset {
                name: "codex-x86_64-unknown-linux-musl.tar.gz".into(),
                digest: None,
                browser_download_url: "https://example.invalid/codex.tar.gz".into(),
            }],
        };
        assert!(select_official_archive(
            "0.147.0",
            "codex-x86_64-unknown-linux-musl.tar.gz",
            release
        )
        .is_err());
    }

    #[test]
    fn version_tokens_preserve_prerelease_identity() {
        assert_eq!(
            version_token("codex-cli 0.147.0-alpha.6.5"),
            Some("0.147.0-alpha.6.5")
        );
        assert!(valid_version("0.147.0-alpha.6.5"));
        assert!(!valid_version("../../0.147.0"));
    }

    #[test]
    fn qualification_record_can_be_replaced_after_a_desktop_update() {
        let root = tempfile::tempdir().unwrap();
        let mut record = QualificationRecord {
            schema_version: 1,
            host_id: "jetson".into(),
            desktop_version: "0.147.0-alpha.6.5".into(),
            desktop_schema_sha256: "a".repeat(64),
            remote_version: "0.147.0-alpha.6.5".into(),
            remote_binary_sha256: "b".repeat(64),
            qualified_at: "2026-08-14T00:00:00Z".into(),
        };
        save_record(root.path(), &record).unwrap();
        record.desktop_version = "0.147.0-alpha.6.6".into();
        save_record(root.path(), &record).unwrap();

        assert_eq!(
            load_record(root.path(), "jetson")
                .unwrap()
                .unwrap()
                .desktop_version,
            "0.147.0-alpha.6.6"
        );
    }

    #[test]
    fn boundary_key_provisioning_precedes_install_and_restart_rpcs_in_source_order() {
        let source = include_str!("desktop_codex.rs");
        let provision_at = source
            .find("provision_remote_boundary_key_without_native_restart(")
            .expect("update_remote_for_desktop() must call provision_remote_boundary_key");
        let install_at = source
            .find("client.codex_install_pinned(")
            .expect("update_remote_for_desktop() must call codex_install_pinned");
        let restart_at = source
            .find("client.codex_restart_native(")
            .expect("update_remote_for_desktop() must call codex_restart_native");
        let confirm_at = source
            .find("confirm_remote_boundary_key_consumers(")
            .expect("dynamic sync must confirm the restarted native instance");
        assert!(
            provision_at < install_at,
            "boundary-key provisioning must run before codex.installPinned"
        );
        assert!(
            provision_at < restart_at,
            "boundary-key provisioning must run before codex.restartNative"
        );
        assert!(
            restart_at < confirm_at,
            "native confirmation must follow restart"
        );
    }
}
