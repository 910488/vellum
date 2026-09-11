//! Clean-host SWE artifact identity: digest-pinned grader images and hashed
//! bundle downloads. Token values and GitHub API bodies never enter error
//! strings; callers persist only the returned SHA-256.

use std::path::Path;

use futures_util::StreamExt;
use reqwest::StatusCode;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::error::{AppError, AppResult};

pub const GITHUB_DOWNLOAD_BASE: &str = "https://github.com";
pub const GITHUB_API_BASE: &str = "https://api.github.com";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciDigestReference {
    pub name: String,
    pub digest: String,
}

/// An immutable OCI reference is `name@sha256:<64 hex>`. Mutable tags such as
/// `:latest` are rejected so a suite cannot pass load on a developer laptop
/// and then fail on a clean host that has no local Docker cache.
pub fn parse_oci_digest_reference(image: &str) -> Option<OciDigestReference> {
    if image.trim().is_empty() || image.chars().any(char::is_whitespace) {
        return None;
    }
    let (name, digest) = image.rsplit_once('@')?;
    if name.is_empty() || !name.contains('/') || name.contains("://") {
        return None;
    }
    let hex = digest.strip_prefix("sha256:")?;
    if hex.len() != 64 || !hex.chars().all(|character| character.is_ascii_hexdigit()) {
        return None;
    }
    Some(OciDigestReference {
        name: name.to_string(),
        digest: digest.to_string(),
    })
}

pub fn is_immutable_oci_reference(image: &str) -> bool {
    parse_oci_digest_reference(image).is_some()
}

/// First 12 hex characters of a `sha256:<64 hex>` config digest.
pub fn config_digest_tag_prefix(image_digest: &str) -> Option<&str> {
    let hex = image_digest.strip_prefix("sha256:")?;
    if hex.len() != 64 || !hex.chars().all(|character| character.is_ascii_hexdigit()) {
        return None;
    }
    Some(&hex[..12])
}

/// Archive-mode local tag: a registry-less name plus `:cfg-<12 hex>`.
/// `:latest` and digest-pinned OCI refs are rejected.
pub fn is_archive_local_tag(image: &str, image_digest: &str) -> bool {
    if image.trim().is_empty()
        || image.chars().any(char::is_whitespace)
        || image.contains('@')
        || image.contains("://")
    {
        return false;
    }
    let Some((name, tag)) = image.rsplit_once(':') else {
        return false;
    };
    if name.is_empty()
        || name.contains(':')
        || !name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return false;
    }
    let Some(prefix) = config_digest_tag_prefix(image_digest) else {
        return false;
    };
    tag == format!("cfg-{prefix}")
}

#[cfg(test)]
pub fn archive_local_tag(instance_id: &str, image_digest: &str) -> Option<String> {
    let prefix = config_digest_tag_prefix(image_digest)?;
    if instance_id.is_empty()
        || !instance_id.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return None;
    }
    Some(format!(
        "sweb.eval.x86_64.{}:cfg-{prefix}",
        instance_id.to_ascii_lowercase()
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadResult {
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveAcquisition {
    Cache,
    Download,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadedImageReport {
    pub tags: Vec<String>,
    pub untagged_ids: Vec<String>,
}

pub fn parse_docker_load_output(stdout: &str) -> LoadedImageReport {
    let mut report = LoadedImageReport::default();
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(tag) = line.strip_prefix("Loaded image: ") {
            if !tag.is_empty() {
                report.tags.push(tag.trim().to_string());
            }
        } else if let Some(id) = line.strip_prefix("Loaded image ID: ") {
            if !id.is_empty() {
                report.untagged_ids.push(id.trim().to_string());
            }
        }
    }
    report
}

pub fn validate_loaded_images(report: &LoadedImageReport, expected_tag: &str) -> AppResult<()> {
    if !report.untagged_ids.is_empty() {
        return Err(AppError::Message(format!(
            "docker load produced untagged image IDs: {}",
            report.untagged_ids.join(", ")
        )));
    }
    if report.tags.is_empty() {
        return Err(AppError::Message(format!(
            "docker load did not produce expected tag {expected_tag}"
        )));
    }
    let extras: Vec<&String> = report
        .tags
        .iter()
        .filter(|tag| tag.as_str() != expected_tag)
        .collect();
    if !extras.is_empty() {
        return Err(AppError::Message(format!(
            "docker load produced undeclared tags: {}",
            extras
                .iter()
                .map(|tag| tag.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if !report.tags.iter().any(|tag| tag == expected_tag) {
        return Err(AppError::Message(format!(
            "docker load did not produce expected tag {expected_tag}"
        )));
    }
    Ok(())
}

/// Decompress gzip `source` to `destination`, failing if output would exceed
/// `max_uncompressed` (zip-bomb guard) or if the decoder is truncated.
pub fn gunzip_limited(source: &Path, destination: &Path, max_uncompressed: u64) -> AppResult<u64> {
    use std::io::{Read, Write};

    let input = std::fs::File::open(source).map_err(|error| {
        AppError::Message(format!("open gzip archive {}: {error}", source.display()))
    })?;
    let mut decoder = flate2::read::GzDecoder::new(input);
    let mut output = std::fs::File::create(destination).map_err(|error| {
        AppError::Message(format!(
            "create decompressed archive {}: {error}",
            destination.display()
        ))
    })?;
    let mut written = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = decoder.read(&mut buffer).map_err(|error| {
            let _ = std::fs::remove_file(destination);
            AppError::Message(format!("gzip decompress failed: {error}"))
        })?;
        if count == 0 {
            break;
        }
        written = written.saturating_add(count as u64);
        if written > max_uncompressed {
            let _ = std::fs::remove_file(destination);
            return Err(AppError::Message(format!(
                "gzip decompressed {written} bytes, exceeding limit {max_uncompressed}"
            )));
        }
        output.write_all(&buffer[..count]).map_err(|error| {
            let _ = std::fs::remove_file(destination);
            AppError::Message(format!("write decompressed archive: {error}"))
        })?;
    }
    output
        .flush()
        .map_err(|error| AppError::Message(format!("flush decompressed archive: {error}")))?;
    Ok(written)
}

/// Config-blob digest from a `docker save` `manifest.json`. This is the
/// driver-independent content identity stored in `image_digest`; it is not
/// the OCI manifest digest pinned in `graderImage`.
pub fn config_digest_from_docker_save_manifest(bytes: &[u8]) -> AppResult<String> {
    let manifest: Value = serde_json::from_slice(bytes).map_err(|error| {
        AppError::Message(format!("cannot parse saved image manifest.json: {error}"))
    })?;
    let config = manifest
        .get(0)
        .and_then(|entry| entry.get("Config"))
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::Message("saved image manifest.json has no Config entry".into()))?;
    let digest = config.strip_prefix("blobs/sha256/").unwrap_or(config);
    Ok(if digest.starts_with("sha256:") {
        digest.to_string()
    } else {
        format!("sha256:{digest}")
    })
}

#[derive(Debug, Clone)]
pub enum GithubTokenSource {
    Default,
    #[cfg(test)]
    Fixed(Option<String>),
}

#[derive(Clone)]
pub struct BundleDownloadClient {
    http: reqwest::Client,
    github_download_base: String,
    github_api_base: String,
    token_source: GithubTokenSource,
}

impl BundleDownloadClient {
    pub fn production(http: reqwest::Client) -> Self {
        Self {
            http,
            github_download_base: GITHUB_DOWNLOAD_BASE.into(),
            github_api_base: GITHUB_API_BASE.into(),
            token_source: GithubTokenSource::Default,
        }
    }

    #[cfg(test)]
    pub fn for_test(
        http: reqwest::Client,
        download_base: impl Into<String>,
        api_base: impl Into<String>,
        token: Option<String>,
    ) -> Self {
        Self {
            http,
            github_download_base: download_base.into(),
            github_api_base: api_base.into(),
            token_source: GithubTokenSource::Fixed(token),
        }
    }

    /// Stream `bundle_url` to `destination` and return the lowercase SHA-256.
    pub async fn download_bundle_url(
        &self,
        bundle_url: &str,
        instance_id: &str,
        destination: &Path,
    ) -> AppResult<String> {
        Ok(self
            .download_url(bundle_url, instance_id, destination, None)
            .await?
            .sha256)
    }

    /// Stream any hashed artifact (bundle or image archive). A GitHub
    /// browser-style release URL that 404s is retried through the REST API.
    /// 429/5xx keep their original HTTP status.
    pub async fn download_url(
        &self,
        url: &str,
        label: &str,
        destination: &Path,
        expected_size: Option<u64>,
    ) -> AppResult<DownloadResult> {
        let direct = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|error| download_error(label, &format!("{error}"), &[]))?;
        let status = direct.status();
        if status.is_success() {
            return stream_response_to_file(direct, destination, label, &[], expected_size).await;
        }
        if status != StatusCode::NOT_FOUND {
            return Err(download_error(
                label,
                &format!("HTTP {}", status.as_u16()),
                &[],
            ));
        }
        let Some((owner, repo, tag, asset_name)) =
            parse_github_release_download_url(url, &self.github_download_base)
        else {
            return Err(download_error(
                label,
                &format!("HTTP {}", status.as_u16()),
                &[],
            ));
        };
        let token = self
            .token_source
            .resolve()
            .await
            .map_err(|error| download_error(label, &error.to_string(), &[]))?;
        let secrets = [token.as_str()];
        let asset_id = self
            .resolve_release_asset_id(&token, &owner, &repo, &tag, &asset_name)
            .await
            .map_err(|error| download_error(label, &error.to_string(), &secrets))?;
        self.download_release_asset(&token, &owner, &repo, asset_id, destination, expected_size)
            .await
            .map_err(|error| download_error(label, &error.to_string(), &secrets))
    }

    pub async fn acquire_hashed_archive(
        &self,
        url: &str,
        label: &str,
        cache_path: &Path,
        expected_sha256: &str,
        expected_size: u64,
        allow_download: bool,
    ) -> AppResult<ArchiveAcquisition> {
        if cache_path.is_file() {
            match hash_and_size(cache_path) {
                Ok((hash, size)) if hash == expected_sha256 && size == expected_size => {
                    return Ok(ArchiveAcquisition::Cache);
                }
                Ok(_) => {
                    let _ = std::fs::remove_file(cache_path);
                    if !allow_download {
                        return Err(AppError::Message(format!(
                            "offline cache for {label} has the wrong SHA-256 or size"
                        )));
                    }
                }
                Err(error) if !allow_download => return Err(error),
                Err(_) => {
                    let _ = std::fs::remove_file(cache_path);
                }
            }
        } else if !allow_download {
            return Err(AppError::Message(format!(
                "offline cache miss for {label}: {}",
                cache_path.display()
            )));
        }
        let temporary = cache_path.with_extension("tar.gz.download");
        let downloaded = self
            .download_url(url, label, &temporary, Some(expected_size))
            .await?;
        if downloaded.sha256 != expected_sha256 || downloaded.size_bytes != expected_size {
            let _ = std::fs::remove_file(&temporary);
            return Err(AppError::Message(format!(
                "downloaded {label} has SHA-256 {} and {} bytes; expected {expected_sha256} / {expected_size}",
                downloaded.sha256, downloaded.size_bytes
            )));
        }
        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                AppError::Message(format!("create artifact cache directory: {error}"))
            })?;
        }
        std::fs::rename(&temporary, cache_path).map_err(|error| {
            AppError::Message(format!("install artifact cache for {label}: {error}"))
        })?;
        Ok(ArchiveAcquisition::Download)
    }

    async fn resolve_release_asset_id(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        tag: &str,
        asset_name: &str,
    ) -> AppResult<u64> {
        let url = format!(
            "{}/repos/{owner}/{repo}/releases/tags/{tag}",
            self.github_api_base.trim_end_matches('/')
        );
        let response = self
            .http
            .get(&url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "vellum-eval")
            .send()
            .await
            .map_err(|error| AppError::Message(format!("resolve GitHub release {tag}: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::Message(format!(
                "resolve GitHub release {owner}/{repo}@{tag}: HTTP {}",
                status.as_u16()
            )));
        }
        let payload: Value = response.json().await.map_err(|error| {
            AppError::Message(format!("parse GitHub release {tag} response: {error}"))
        })?;
        payload
            .get("assets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|asset| asset.get("name").and_then(Value::as_str) == Some(asset_name))
            .and_then(|asset| asset.get("id"))
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                AppError::Message(format!(
                    "GitHub release {owner}/{repo}@{tag} has no asset named {asset_name}"
                ))
            })
    }

    async fn download_release_asset(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        asset_id: u64,
        destination: &Path,
        expected_size: Option<u64>,
    ) -> AppResult<DownloadResult> {
        let url = format!(
            "{}/repos/{owner}/{repo}/releases/assets/{asset_id}",
            self.github_api_base.trim_end_matches('/')
        );
        let response = self
            .http
            .get(&url)
            .bearer_auth(token)
            .header("Accept", "application/octet-stream")
            .header("User-Agent", "vellum-eval")
            .send()
            .await
            .map_err(|error| {
                AppError::Message(format!("download release asset {asset_id}: {error}"))
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::Message(format!(
                "download release asset {asset_id}: HTTP {}",
                status.as_u16()
            )));
        }
        stream_response_to_file(
            response,
            destination,
            &asset_id.to_string(),
            &[token],
            expected_size,
        )
        .await
    }
}

impl GithubTokenSource {
    async fn resolve(&self) -> AppResult<String> {
        match self {
            #[cfg(test)]
            Self::Fixed(Some(token)) => Ok(token.clone()),
            #[cfg(test)]
            Self::Fixed(None) => Err(missing_github_token()),
            Self::Default => resolve_default_github_token().await,
        }
    }
}

pub fn parse_github_release_download_url(
    url: &str,
    download_base: &str,
) -> Option<(String, String, String, String)> {
    let prefix = format!("{}/", download_base.trim_end_matches('/'));
    let rest = url.strip_prefix(&prefix)?;
    let segments: Vec<&str> = rest.split('/').collect();
    if segments.len() != 6 || segments[2] != "releases" || segments[3] != "download" {
        return None;
    }
    if segments.iter().any(|segment| segment.is_empty()) {
        return None;
    }
    Some((
        segments[0].to_string(),
        segments[1].to_string(),
        segments[4].to_string(),
        segments[5].to_string(),
    ))
}

pub fn install_hashed_download(
    temporary: &Path,
    destination: &Path,
    actual_hash: &str,
    expected_hash: &str,
    instance_id: &str,
) -> AppResult<()> {
    if actual_hash != expected_hash {
        let _ = std::fs::remove_file(temporary);
        return Err(AppError::Message(format!(
            "downloaded SWE-bench bundle {instance_id} has unexpected SHA-256 {actual_hash}"
        )));
    }
    std::fs::rename(temporary, destination).map_err(|error| {
        AppError::Message(format!("install SWE-bench bundle {instance_id}: {error}"))
    })
}

fn download_error(instance_id: &str, detail: &str, secrets: &[&str]) -> AppError {
    AppError::Message(format!(
        "download SWE-bench {instance_id}: {}",
        redact_secrets(detail, secrets)
    ))
}

fn redact_secrets(input: &str, secrets: &[&str]) -> String {
    let mut output = input.to_string();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        output = output.replace(secret, "[REDACTED]");
    }
    output
}

fn missing_github_token() -> AppError {
    AppError::Message(
        "no GITHUB_TOKEN/GH_TOKEN and `gh auth token` failed; cannot authenticate to fetch a private-repo release bundle".into(),
    )
}

async fn resolve_default_github_token() -> AppResult<String> {
    for variable in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(token) = std::env::var(variable) {
            if !token.trim().is_empty() {
                return Ok(token);
            }
        }
    }
    let output = crate::process::background_tokio_command("gh")
        .args(["auth", "token"])
        .output()
        .await
        .map_err(|_| missing_github_token())?;
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || token.is_empty() {
        return Err(missing_github_token());
    }
    Ok(token)
}

fn hash_and_size(path: &Path) -> AppResult<(String, u64)> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)
        .map_err(|error| AppError::Message(format!("open cached artifact: {error}")))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| AppError::Message(format!("hash cached artifact: {error}")))?;
        if count == 0 {
            break;
        }
        size += count as u64;
        hasher.update(&buffer[..count]);
    }
    Ok((format!("{:x}", hasher.finalize()), size))
}

async fn stream_response_to_file(
    response: reqwest::Response,
    destination: &Path,
    label: &str,
    secrets: &[&str],
    expected_size: Option<u64>,
) -> AppResult<DownloadResult> {
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|error| {
            AppError::Message(format!("create SWE-bench download directory: {error}"))
        })?;
    }
    let mut file = tokio::fs::File::create(destination)
        .await
        .map_err(|error| {
            AppError::Message(format!("create SWE-bench download {label}: {error}"))
        })?;
    let mut hasher = Sha256::new();
    let mut size_bytes = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            AppError::Message(format!(
                "read SWE-bench {label}: {}",
                redact_secrets(&error.to_string(), secrets)
            ))
        })?;
        size_bytes += chunk.len() as u64;
        if expected_size.is_some_and(|limit| size_bytes > limit) {
            let _ = tokio::fs::remove_file(destination).await;
            return Err(AppError::Message(format!(
                "downloaded {label} exceeded expected size {limit}",
                limit = expected_size.unwrap_or_default()
            )));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(|error| {
            AppError::Message(format!("write SWE-bench download {label}: {error}"))
        })?;
    }
    file.flush()
        .await
        .map_err(|error| AppError::Message(format!("flush SWE-bench download {label}: {error}")))?;
    if let Some(expected) = expected_size {
        if size_bytes != expected {
            let _ = tokio::fs::remove_file(destination).await;
            return Err(AppError::Message(format!(
                "downloaded {label} has {size_bytes} bytes; expected {expected}"
            )));
        }
    }
    Ok(DownloadResult {
        sha256: format!("{:x}", hasher.finalize()),
        size_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::{Path as AxumPath, Request};
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::response::Response;
    use axum::routing::get;
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::oneshot;

    const CONFIG_HEX: &str = "64ac3483a165db2706f67b59e064095d0646e4db397d1573443b307c45763d1b";
    const MANIFEST_HEX: &str = "3f35c591fdd9e120548fb9aa14cc1d1bc572e87385b5ee8eb32e7f506dfb4313";
    const SECRET: &str = "ghp_unit_test_secret_token_do_not_leak";

    fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn immutable_oci_reference_requires_registry_and_digest() {
        assert!(is_immutable_oci_reference(&format!(
            "ghcr.io/910488/vellum/swe-grader-psf-requests-1142@sha256:{MANIFEST_HEX}"
        )));
        assert!(!is_immutable_oci_reference(
            "sweb.eval.x86_64.psf__requests-1142:latest"
        ));
        assert!(!is_immutable_oci_reference("ghcr.io/910488/vellum:latest"));
        assert!(!is_immutable_oci_reference(&format!(
            "ubuntu@sha256:{MANIFEST_HEX}"
        )));
        assert!(!is_immutable_oci_reference(
            "ghcr.io/910488/vellum/swe-grader@sha256:deadbeef"
        ));
    }

    #[test]
    fn config_digest_reads_descriptor_blob_path_and_prefixed_fallback() {
        let blob = format!(r#"[{{"Config":"blobs/sha256/{CONFIG_HEX}"}}]"#);
        assert_eq!(
            config_digest_from_docker_save_manifest(blob.as_bytes()).unwrap(),
            format!("sha256:{CONFIG_HEX}")
        );
        let prefixed = format!(r#"[{{"Config":"sha256:{CONFIG_HEX}"}}]"#);
        assert_eq!(
            config_digest_from_docker_save_manifest(prefixed.as_bytes()).unwrap(),
            format!("sha256:{CONFIG_HEX}")
        );
        let bare = format!(r#"[{{"Config":"{CONFIG_HEX}"}}]"#);
        assert_eq!(
            config_digest_from_docker_save_manifest(bare.as_bytes()).unwrap(),
            format!("sha256:{CONFIG_HEX}")
        );
    }

    #[test]
    fn parse_github_release_url_accepts_only_browser_download_shape() {
        assert_eq!(
            parse_github_release_download_url(
                "https://github.com/910488/vellum/releases/download/swebench-grader-v1/psf__requests-1142.tar",
                GITHUB_DOWNLOAD_BASE,
            ),
            Some((
                "910488".into(),
                "vellum".into(),
                "swebench-grader-v1".into(),
                "psf__requests-1142.tar".into(),
            ))
        );
        assert!(parse_github_release_download_url(
            "https://api.github.com/repos/910488/vellum/releases/assets/1",
            GITHUB_DOWNLOAD_BASE,
        )
        .is_none());
    }

    #[test]
    fn hash_mismatch_deletes_the_temporary_file() {
        let temp = tempfile::tempdir().unwrap();
        let downloaded = temp.path().join("bundle.download");
        let installed = temp.path().join("bundle.tar");
        std::fs::write(&downloaded, b"wrong").unwrap();
        let error =
            install_hashed_download(&downloaded, &installed, "aaa", "bbb", "psf__requests-1142")
                .unwrap_err();
        assert!(error.to_string().contains("unexpected SHA-256 aaa"));
        assert!(!downloaded.exists());
        assert!(!installed.exists());
    }

    #[test]
    fn download_errors_redact_tokens() {
        let error = download_error("psf__requests-1142", SECRET, &[SECRET]);
        let text = error.to_string();
        assert!(!text.contains(SECRET));
        assert!(text.contains("[REDACTED]"));
    }

    struct MockGithub {
        api_hits: AtomicUsize,
        last_authorization: std::sync::Mutex<Option<String>>,
    }

    async fn spawn_mock(state: Arc<MockGithub>) -> (String, oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        let app = Router::new()
            .route("/public/bundle.tar", get(|| async { "public-bundle" }))
            .route(
                "/910488/vellum/releases/download/{tag}/{asset}",
                get(release_download),
            )
            .route(
                "/repos/910488/vellum/releases/tags/{tag}",
                get(release_metadata),
            )
            .route(
                "/repos/910488/vellum/releases/assets/{id}",
                get(release_asset),
            )
            .with_state(state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        (format!("http://{addr}"), tx)
    }

    async fn release_download(
        AxumPath((tag, asset)): AxumPath<(String, String)>,
    ) -> Result<&'static str, StatusCode> {
        match (tag.as_str(), asset.as_str()) {
            ("public-tag", "bundle.tar") => Ok("public-bundle"),
            ("rate-tag", _) => Err(StatusCode::TOO_MANY_REQUESTS),
            ("fail-tag", _) => Err(StatusCode::INTERNAL_SERVER_ERROR),
            _ => Err(StatusCode::NOT_FOUND),
        }
    }

    async fn release_metadata(
        axum::extract::State(state): axum::extract::State<Arc<MockGithub>>,
        AxumPath(tag): AxumPath<String>,
        headers: HeaderMap,
    ) -> Response {
        state.api_hits.fetch_add(1, Ordering::SeqCst);
        *state.last_authorization.lock().unwrap() = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        match tag.as_str() {
            "private-tag" => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"token":"should-not-leak","assets":[{"name":"bundle.tar","id":99}]}"#,
                ))
                .unwrap(),
            "missing-tag" => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"token":"should-not-leak","assets":[{"name":"other.tar","id":1}]}"#,
                ))
                .unwrap(),
            _ => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap(),
        }
    }

    async fn release_asset(
        axum::extract::State(state): axum::extract::State<Arc<MockGithub>>,
        AxumPath(id): AxumPath<String>,
        request: Request,
    ) -> Result<&'static str, StatusCode> {
        *state.last_authorization.lock().unwrap() = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        match id.as_str() {
            "99" => Ok("private-bundle"),
            _ => Err(StatusCode::NOT_FOUND),
        }
    }

    async fn download(
        client: &BundleDownloadClient,
        url: &str,
    ) -> (Result<String, AppError>, Vec<u8>) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bundle.tar");
        let result = client
            .download_bundle_url(url, "psf__requests-1142", &path)
            .await;
        let bytes = std::fs::read(&path).unwrap_or_default();
        (result, bytes)
    }

    #[tokio::test]
    async fn public_direct_download_streams_and_hashes() {
        let state = Arc::new(MockGithub {
            api_hits: AtomicUsize::new(0),
            last_authorization: std::sync::Mutex::new(None),
        });
        let (origin, shutdown) = spawn_mock(Arc::clone(&state)).await;
        let client = BundleDownloadClient::for_test(
            reqwest::Client::new(),
            origin.clone(),
            origin.clone(),
            Some(SECRET.into()),
        );
        let url = format!("{origin}/910488/vellum/releases/download/public-tag/bundle.tar");
        let (result, bytes) = download(&client, &url).await;
        let hash = result.unwrap();
        assert_eq!(bytes, b"public-bundle");
        assert_eq!(hash, sha256_hex(b"public-bundle"));
        assert_eq!(state.api_hits.load(Ordering::SeqCst), 0);
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn private_release_404_falls_back_to_api() {
        let state = Arc::new(MockGithub {
            api_hits: AtomicUsize::new(0),
            last_authorization: std::sync::Mutex::new(None),
        });
        let (origin, shutdown) = spawn_mock(Arc::clone(&state)).await;
        let client = BundleDownloadClient::for_test(
            reqwest::Client::new(),
            origin.clone(),
            origin.clone(),
            Some(SECRET.into()),
        );
        let url = format!("{origin}/910488/vellum/releases/download/private-tag/bundle.tar");
        let (result, bytes) = download(&client, &url).await;
        assert_eq!(bytes, b"private-bundle");
        assert_eq!(result.unwrap(), sha256_hex(b"private-bundle"));
        assert_eq!(state.api_hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            state.last_authorization.lock().unwrap().as_deref(),
            Some("Bearer ghp_unit_test_secret_token_do_not_leak")
        );
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn rate_limit_and_server_errors_do_not_use_private_fallback() {
        let state = Arc::new(MockGithub {
            api_hits: AtomicUsize::new(0),
            last_authorization: std::sync::Mutex::new(None),
        });
        let (origin, shutdown) = spawn_mock(Arc::clone(&state)).await;
        let client = BundleDownloadClient::for_test(
            reqwest::Client::new(),
            origin.clone(),
            origin.clone(),
            Some(SECRET.into()),
        );
        for (tag, status) in [("rate-tag", 429), ("fail-tag", 500)] {
            let url = format!("{origin}/910488/vellum/releases/download/{tag}/bundle.tar");
            let (result, _) = download(&client, &url).await;
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains(&format!("HTTP {status}")),
                "{tag} should keep HTTP {status}: {error}"
            );
            assert!(!error.contains(SECRET), "{error}");
        }
        assert_eq!(state.api_hits.load(Ordering::SeqCst), 0);
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn missing_token_after_private_404_is_explicit() {
        let state = Arc::new(MockGithub {
            api_hits: AtomicUsize::new(0),
            last_authorization: std::sync::Mutex::new(None),
        });
        let (origin, shutdown) = spawn_mock(Arc::clone(&state)).await;
        let client = BundleDownloadClient::for_test(
            reqwest::Client::new(),
            origin.clone(),
            origin.clone(),
            None,
        );
        let url = format!("{origin}/910488/vellum/releases/download/private-tag/bundle.tar");
        let error = download(&client, &url).await.0.unwrap_err().to_string();
        assert!(error.contains("no GITHUB_TOKEN/GH_TOKEN"), "{error}");
        assert_eq!(state.api_hits.load(Ordering::SeqCst), 0);
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn missing_asset_and_api_body_stay_out_of_errors() {
        let state = Arc::new(MockGithub {
            api_hits: AtomicUsize::new(0),
            last_authorization: std::sync::Mutex::new(None),
        });
        let (origin, shutdown) = spawn_mock(Arc::clone(&state)).await;
        let client = BundleDownloadClient::for_test(
            reqwest::Client::new(),
            origin.clone(),
            origin.clone(),
            Some(SECRET.into()),
        );
        let url = format!("{origin}/910488/vellum/releases/download/missing-tag/bundle.tar");
        let error = download(&client, &url).await.0.unwrap_err().to_string();
        assert!(error.contains("has no asset named bundle.tar"), "{error}");
        assert!(!error.contains(SECRET), "{error}");
        assert!(!error.contains("should-not-leak"), "{error}");
        let _ = shutdown.send(());
    }

    #[test]
    fn archive_local_tag_uses_config_digest_prefix() {
        let digest = format!("sha256:{CONFIG_HEX}");
        assert_eq!(
            archive_local_tag("psf__requests-1142", &digest).as_deref(),
            Some("sweb.eval.x86_64.psf__requests-1142:cfg-64ac3483a165")
        );
        assert!(is_archive_local_tag(
            "sweb.eval.x86_64.psf__requests-1142:cfg-64ac3483a165",
            &digest
        ));
        assert!(!is_archive_local_tag(
            "sweb.eval.x86_64.psf__requests-1142:latest",
            &digest
        ));
        assert!(!is_archive_local_tag(
            &format!("ghcr.io/910488/vellum/swe-grader@sha256:{MANIFEST_HEX}"),
            &digest
        ));
    }

    #[test]
    fn docker_load_output_rejects_extra_and_untagged_images() {
        let ok = parse_docker_load_output(
            "Loaded image: sweb.eval.x86_64.psf__requests-1142:cfg-64ac3483a165\n",
        );
        validate_loaded_images(&ok, "sweb.eval.x86_64.psf__requests-1142:cfg-64ac3483a165")
            .unwrap();

        let extra = parse_docker_load_output(
            "Loaded image: sweb.eval.x86_64.psf__requests-1142:cfg-64ac3483a165\nLoaded image: extra:latest\n",
        );
        assert!(validate_loaded_images(
            &extra,
            "sweb.eval.x86_64.psf__requests-1142:cfg-64ac3483a165"
        )
        .unwrap_err()
        .to_string()
        .contains("undeclared tags"));

        let untagged = parse_docker_load_output("Loaded image ID: sha256:deadbeef\n");
        assert!(validate_loaded_images(
            &untagged,
            "sweb.eval.x86_64.psf__requests-1142:cfg-64ac3483a165"
        )
        .unwrap_err()
        .to_string()
        .contains("untagged"));
    }

    #[test]
    fn gunzip_rejects_truncated_input_and_oversize_output() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("truncated.gz");
        std::fs::write(&src, b"\x1f\x8b\x08\x00incomplete").unwrap();
        let dst = temp.path().join("out.tar");
        assert!(gunzip_limited(&src, &dst, 1024)
            .unwrap_err()
            .to_string()
            .contains("gzip decompress failed"));

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &[b'x'; 64]).unwrap();
        let gz = encoder.finish().unwrap();
        let src = temp.path().join("bomb.gz");
        std::fs::write(&src, gz).unwrap();
        assert!(gunzip_limited(&src, &dst, 16)
            .unwrap_err()
            .to_string()
            .contains("exceeding limit"));
        assert!(!dst.exists());
    }

    #[tokio::test]
    async fn size_mismatch_is_rejected_before_cache_install() {
        let state = Arc::new(MockGithub {
            api_hits: AtomicUsize::new(0),
            last_authorization: std::sync::Mutex::new(None),
        });
        let (origin, shutdown) = spawn_mock(Arc::clone(&state)).await;
        let client = BundleDownloadClient::for_test(
            reqwest::Client::new(),
            origin.clone(),
            origin.clone(),
            Some(SECRET.into()),
        );
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("aa".repeat(32) + ".tar.gz");
        let url = format!("{origin}/910488/vellum/releases/download/public-tag/bundle.tar");
        let error = client
            .acquire_hashed_archive(
                &url,
                "psf__requests-1142.image",
                &cache,
                &sha256_hex(b"public-bundle"),
                1,
                true,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("exceeded expected size") || error.contains("bytes"),
            "{error}"
        );
        assert!(!cache.exists());
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn matching_cache_skips_download_and_offline_miss_fails_closed() {
        let state = Arc::new(MockGithub {
            api_hits: AtomicUsize::new(0),
            last_authorization: std::sync::Mutex::new(None),
        });
        let (origin, shutdown) = spawn_mock(Arc::clone(&state)).await;
        let client = BundleDownloadClient::for_test(
            reqwest::Client::new(),
            origin.clone(),
            origin.clone(),
            Some(SECRET.into()),
        );
        let temp = tempfile::tempdir().unwrap();
        let cache = temp
            .path()
            .join(format!("{}.tar.gz", sha256_hex(b"public-bundle")));
        std::fs::write(&cache, b"public-bundle").unwrap();
        let url = format!("{origin}/910488/vellum/releases/download/public-tag/bundle.tar");
        let acquired = client
            .acquire_hashed_archive(
                &url,
                "psf__requests-1142.image",
                &cache,
                &sha256_hex(b"public-bundle"),
                b"public-bundle".len() as u64,
                false,
            )
            .await
            .unwrap();
        assert_eq!(acquired, ArchiveAcquisition::Cache);
        assert_eq!(state.api_hits.load(Ordering::SeqCst), 0);

        let missing = temp.path().join("missing.tar.gz");
        let error = client
            .acquire_hashed_archive(
                &url,
                "psf__requests-1142.image",
                &missing,
                &sha256_hex(b"public-bundle"),
                b"public-bundle".len() as u64,
                false,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("offline cache miss"), "{error}");
        let _ = shutdown.send(());
    }
}
