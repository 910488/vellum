use std::collections::BTreeMap;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Default non-root identity baked into deploy/proxy/Dockerfile for standalone
/// container runs (no bind mounts). Bind-mount deployments override --user with
/// the host agent effective UID/GID instead of forcing this fixed identity.
pub const PROXY_IMAGE_DEFAULT_UID: u32 = 65532;
pub const PROXY_IMAGE_DEFAULT_GID: u32 = 65532;
pub const PROXY_IMAGE_DEFAULT_USER: &str = "65532:65532";

/// Default bound for long-running docker pulls while holding the mutation lock.
pub const DOCKER_PULL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedContainerLabels {
    pub host_id: String,
    pub install_id: String,
    pub config_hash: String,
    pub image_version: String,
}

impl ManagedContainerLabels {
    pub fn as_map(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        map.insert("io.vellum.managed".into(), "true".into());
        map.insert("io.vellum.component".into(), "proxy".into());
        map.insert("io.vellum.host-id".into(), self.host_id.clone());
        map.insert("io.vellum.install-id".into(), self.install_id.clone());
        map.insert("io.vellum.config-hash".into(), self.config_hash.clone());
        map.insert("io.vellum.image-version".into(), self.image_version.clone());
        map
    }

    pub fn matches(&self, labels: &BTreeMap<String, String>) -> bool {
        labels.get("io.vellum.managed").map(String::as_str) == Some("true")
            && labels.get("io.vellum.component").map(String::as_str) == Some("proxy")
            && labels.get("io.vellum.host-id") == Some(&self.host_id)
            && labels.get("io.vellum.install-id") == Some(&self.install_id)
            && labels.get("io.vellum.config-hash") == Some(&self.config_hash)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContainerSummary {
    pub id: String,
    pub name: String,
    pub image: String,
    pub running: bool,
    pub labels: BTreeMap<String, String>,
    pub host_port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageIdentity {
    /// Repository/manifest digest from RepoDigests (`sha256:...`), if any.
    pub repo_digest: Option<String>,
    /// Docker image config id (`.Id`), never used as `repo@sha256`.
    pub image_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImage {
    /// Original caller-supplied reference (tag or digest).
    pub source: String,
    /// Runtime reference for `docker run`.
    /// - registry: `repo@sha256:<manifest>`
    /// - local tag without RepoDigest: original tag (e.g. `vellum-proxy:local`)
    pub run_ref: String,
    /// Immutable repository/manifest digest when available.
    pub repo_digest: Option<String>,
    /// Local image id for drift detection; not a repo pin.
    pub image_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyzProbe {
    pub ready: bool,
    pub install_id: Option<String>,
    pub config_hash: Option<String>,
}

pub trait DockerClient: Send + Sync {
    fn list_proxy_containers(&self) -> Result<Vec<ContainerSummary>, String>;
    fn run_proxy_container(&self, spec: &RunProxySpec) -> Result<ContainerSummary, String>;
    fn stop_container(&self, id: &str) -> Result<(), String>;
    fn remove_container(&self, id: &str) -> Result<(), String>;
    fn inspect_readyz(&self, host_port: u16, boundary_key: &str) -> Result<bool, String>;
    /// Verify the Responses endpoint accepts a WebSocket upgrade. Codex may
    /// select this transport for the first turn even when later turns use
    /// SSE, so HTTP-only readiness is not sufficient for native parity.
    fn inspect_responses_websocket(
        &self,
        host_port: u16,
        boundary_key: &str,
    ) -> Result<bool, String>;
    fn inspect_readyz_identity(
        &self,
        host_port: u16,
        boundary_key: &str,
    ) -> Result<ReadyzProbe, String> {
        Ok(ReadyzProbe {
            ready: self.inspect_readyz(host_port, boundary_key)?,
            install_id: None,
            config_hash: None,
        })
    }

    fn pull_image(&self, image: &str) -> Result<(), String>;
    /// Inspect image identity. RepoDigest and Image ID are distinct.
    fn inspect_image_identity(&self, image: &str) -> Result<ImageIdentity, String>;
    fn load_image_archive(
        &self,
        _archive: &std::path::Path,
        _image: &str,
    ) -> Result<ImageIdentity, String> {
        Err("docker image archive loading is unsupported".into())
    }
}

#[derive(Debug, Clone)]
pub struct RunProxySpec {
    pub image: String,
    pub name: String,
    pub host_port: u16,
    pub labels: ManagedContainerLabels,
    pub config_dir: String,
    pub data_dir: String,
    pub history_dir: String,
    pub logs_dir: String,
    pub secrets_dir: String,
    pub user: String,
}

#[derive(Debug, Default)]
pub struct ProcessDockerClient;

impl ProcessDockerClient {
    pub fn new() -> Self {
        Self
    }

    fn run(args: &[&str]) -> Result<Output, String> {
        Command::new("docker")
            .args(args)
            .output()
            .map_err(|error| format!("failed invoking docker: {error}"))
    }

    fn run_timeout(args: &[&str], timeout: Duration) -> Result<Output, String> {
        // docker CLI itself does not expose a portable pull timeout flag across
        // all hosts; bound the agent-side wait so Desktop RPCs fail closed.
        let mut child = Command::new("docker")
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|error| format!("failed invoking docker: {error}"))?;
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_status)) => {
                    return child
                        .wait_with_output()
                        .map_err(|error| format!("failed collecting docker output: {error}"));
                }
                Ok(None) => {
                    if started.elapsed() > timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!(
                            "docker command timed out after {}s: {}",
                            timeout.as_secs(),
                            args.join(" ")
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(format!("failed waiting for docker: {error}")),
            }
        }
    }
}

impl DockerClient for ProcessDockerClient {
    fn list_proxy_containers(&self) -> Result<Vec<ContainerSummary>, String> {
        let output = Self::run(&[
            "ps",
            "-a",
            "--filter",
            "label=io.vellum.managed=true",
            "--filter",
            "label=io.vellum.component=proxy",
            "--format",
            "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Label \"io.vellum.host-id\"}}\t{{.Label \"io.vellum.install-id\"}}\t{{.Label \"io.vellum.config-hash\"}}\t{{.Label \"io.vellum.image-version\"}}\t{{.Ports}}",
        ])?;
        if !output.status.success() {
            return Err(format!(
                "docker ps failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let mut out = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if line.trim().is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 9 {
                continue;
            }
            let mut labels = BTreeMap::new();
            labels.insert("io.vellum.managed".into(), "true".into());
            labels.insert("io.vellum.component".into(), "proxy".into());
            labels.insert("io.vellum.host-id".into(), parts[4].to_string());
            labels.insert("io.vellum.install-id".into(), parts[5].to_string());
            labels.insert("io.vellum.config-hash".into(), parts[6].to_string());
            labels.insert("io.vellum.image-version".into(), parts[7].to_string());
            out.push(ContainerSummary {
                id: parts[0].to_string(),
                name: parts[1].to_string(),
                image: parts[2].to_string(),
                running: parts[3].to_lowercase().starts_with("up"),
                labels,
                host_port: parse_host_port(parts[8]),
            });
        }
        Ok(out)
    }

    fn run_proxy_container(&self, spec: &RunProxySpec) -> Result<ContainerSummary, String> {
        let mut args = vec![
            "run".into(),
            "-d".into(),
            "--restart".into(),
            "unless-stopped".into(),
            "--name".into(),
            spec.name.clone(),
            "--user".into(),
            spec.user.clone(),
            "-p".into(),
            format!("127.0.0.1:{}:15721", spec.host_port),
            "-v".into(),
            format!("{}:/etc/vellum:ro", spec.config_dir),
            "-v".into(),
            format!("{}:/var/lib/vellum/data", spec.data_dir),
            "-v".into(),
            format!("{}:/var/lib/vellum/history", spec.history_dir),
            "-v".into(),
            format!("{}:/var/log/vellum", spec.logs_dir),
            "-v".into(),
            format!("{}:/run/secrets:ro", spec.secrets_dir),
        ];
        for (key, value) in spec.labels.as_map() {
            args.push("--label".into());
            args.push(format!("{key}={value}"));
        }
        args.push(spec.image.clone());
        args.push("serve".into());
        args.push("--config".into());
        args.push("/etc/vellum/proxy.toml".into());
        args.push("--listen".into());
        args.push("0.0.0.0:15721".into());

        let str_args = args.iter().map(String::as_str).collect::<Vec<_>>();
        let output = Self::run(&str_args)?;
        if !output.status.success() {
            return Err(format!(
                "docker run failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(ContainerSummary {
            id,
            name: spec.name.clone(),
            image: spec.image.clone(),
            running: true,
            labels: spec.labels.as_map(),
            host_port: Some(spec.host_port),
        })
    }

    fn stop_container(&self, id: &str) -> Result<(), String> {
        let output = Self::run(&["stop", id])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "docker stop failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }

    fn remove_container(&self, id: &str) -> Result<(), String> {
        let output = Self::run(&["rm", "-f", id])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "docker rm failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }

    fn inspect_readyz(&self, host_port: u16, boundary_key: &str) -> Result<bool, String> {
        Ok(self.inspect_readyz_identity(host_port, boundary_key)?.ready)
    }

    fn inspect_readyz_identity(
        &self,
        host_port: u16,
        boundary_key: &str,
    ) -> Result<ReadyzProbe, String> {
        // `/readyz` is behind the boundary guard like every other endpoint, so
        // the agent's own probe presents the key it provisioned. Passed via
        // `-H` rather than the URL: a header does not reach the process list
        // the way a query string would.
        let url = format!("http://127.0.0.1:{host_port}/readyz");
        let header = format!(
            "{}: {boundary_key}",
            vellum_proxy_runtime::BOUNDARY_KEY_HEADER
        );
        let output = Command::new("curl")
            .args(["-sS", "-H", header.as_str(), url.as_str()])
            .output()
            .map_err(|error| format!("readyz probe failed: {error}"))?;
        if !output.status.success() {
            return Ok(ReadyzProbe {
                ready: false,
                install_id: None,
                config_hash: None,
            });
        }
        let body: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("readyz returned invalid JSON: {error}"))?;
        Ok(ReadyzProbe {
            ready: body
                .get("ready")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            install_id: body
                .pointer("/identity/install_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            config_hash: body
                .pointer("/identity/config_hash")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        })
    }

    fn inspect_responses_websocket(
        &self,
        host_port: u16,
        boundary_key: &str,
    ) -> Result<bool, String> {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let timeout = Duration::from_secs(3);
        let mut stream = TcpStream::connect_timeout(
            &format!("127.0.0.1:{host_port}")
                .parse()
                .map_err(|error| format!("invalid proxy probe address: {error}"))?,
            timeout,
        )
        .map_err(|error| format!("responses websocket connect failed: {error}"))?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|error| format!("responses websocket read timeout: {error}"))?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(|error| format!("responses websocket write timeout: {error}"))?;
        // The upgrade is behind the boundary guard like every other endpoint.
        let request = format!(
            "GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{host_port}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{key_header}: {boundary_key}\r\n\r\n",
            key_header = vellum_proxy_runtime::BOUNDARY_KEY_HEADER,
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|error| format!("responses websocket probe write failed: {error}"))?;
        let mut response = [0u8; 1024];
        let count = stream
            .read(&mut response)
            .map_err(|error| format!("responses websocket probe read failed: {error}"))?;
        let head = String::from_utf8_lossy(&response[..count]);
        Ok(head.starts_with("HTTP/1.1 101 ") || head.starts_with("HTTP/1.0 101 "))
    }

    fn pull_image(&self, image: &str) -> Result<(), String> {
        if image_ref_is_local_only(image) {
            // Local tags / already-digest-pinned images do not require a registry pull.
            return Ok(());
        }
        let output = Self::run_timeout(&["pull", image], DOCKER_PULL_TIMEOUT)?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "docker pull failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }

    fn inspect_image_identity(&self, image: &str) -> Result<ImageIdentity, String> {
        if let Some(digest) = digest_from_ref(image) {
            return Ok(ImageIdentity {
                repo_digest: Some(digest),
                image_id: None,
            });
        }
        let repo_output = Self::run(&[
            "image",
            "inspect",
            image,
            "--format",
            "{{if .RepoDigests}}{{index .RepoDigests 0}}{{end}}",
        ])?;
        if !repo_output.status.success() {
            return Err(format!(
                "docker image inspect failed: {}",
                String::from_utf8_lossy(&repo_output.stderr)
            ));
        }
        let id_output = Self::run(&["image", "inspect", image, "--format", "{{.Id}}"])?;
        if !id_output.status.success() {
            return Err(format!(
                "docker image inspect failed: {}",
                String::from_utf8_lossy(&id_output.stderr)
            ));
        }
        let repo_raw = String::from_utf8_lossy(&repo_output.stdout)
            .trim()
            .to_string();
        let id_raw = String::from_utf8_lossy(&id_output.stdout)
            .trim()
            .to_string();
        Ok(ImageIdentity {
            repo_digest: normalize_digest_field(&repo_raw),
            image_id: normalize_digest_field(&id_raw),
        })
    }

    fn load_image_archive(
        &self,
        archive: &std::path::Path,
        image: &str,
    ) -> Result<ImageIdentity, String> {
        let archive = archive
            .to_str()
            .ok_or_else(|| "proxy image archive path is not UTF-8".to_string())?;
        let output = Self::run_timeout(&["load", "--input", archive], Duration::from_secs(300))?;
        if !output.status.success() {
            return Err(format!(
                "docker load failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        self.inspect_image_identity(image)
    }
}

pub fn resolve_image_with(
    docker: &dyn DockerClient,
    image: &str,
    expected_digest: Option<&str>,
) -> Result<ResolvedImage, String> {
    let expected = expected_digest
        .map(normalize_digest)
        .transpose()?
        .filter(|value| !value.is_empty());

    // If the caller already passed repo@sha256, honor and verify it.
    if let Some(digest) = digest_from_ref(image) {
        if let Some(expected) = expected.as_ref() {
            if &digest != expected {
                return Err(format!(
                    "ImageDigestMismatch: expected {expected}, got {digest}"
                ));
            }
        }
        return Ok(ResolvedImage {
            source: image.to_string(),
            run_ref: image.to_string(),
            repo_digest: Some(digest),
            image_id: None,
        });
    }

    docker.pull_image(image)?;
    let identity = docker.inspect_image_identity(image)?;

    if let Some(expected) = expected.as_ref() {
        match identity.repo_digest.as_ref() {
            Some(actual) if actual == expected => {}
            Some(actual) => {
                return Err(format!(
                    "ImageDigestMismatch: expected {expected}, got {actual}"
                ));
            }
            None if image_ref_is_local_only(image) => {
                // Local builds have no RepoDigest. Do not invent one from Image ID.
                // Expected digest is only meaningful for registry-backed images.
                return Err(
                    "ImageDigestMismatch: expected repo digest provided but local image has no RepoDigest"
                        .into(),
                );
            }
            None => {
                return Err(
                    "ImageDigestMismatch: expected digest provided but image has no RepoDigest"
                        .into(),
                );
            }
        }
    }

    // Only pin with repository/manifest digests. Never synthesize repo@image-id.
    let run_ref = match identity.repo_digest.as_ref() {
        Some(digest) => pin_image_ref(image, digest),
        None => image.to_string(),
    };
    Ok(ResolvedImage {
        source: image.to_string(),
        run_ref,
        repo_digest: identity.repo_digest,
        image_id: identity.image_id,
    })
}

pub fn pin_image_ref(image: &str, digest: &str) -> String {
    if image.contains('@') {
        return image.to_string();
    }
    let repo = image
        .rsplit_once(':')
        .map(|(repo, _tag)| repo)
        .unwrap_or(image);
    format!("{repo}@{digest}")
}

pub fn digest_from_ref(image: &str) -> Option<String> {
    let (_, digest) = image.split_once('@')?;
    normalize_digest(digest).ok()
}

pub fn normalize_digest(raw: &str) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Err("image digest must not be empty".into());
    }
    if let Some(hex) = value.strip_prefix("sha256:") {
        if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(format!("sha256:{hex}"));
        }
        return Err(format!("invalid sha256 digest: {value}"));
    }
    if value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(format!("sha256:{value}"));
    }
    // Accept docker image ids like sha256: short only as opaque when already prefixed.
    if value.starts_with("sha256:") {
        return Ok(value.to_string());
    }
    Err(format!("unsupported image digest format: {value}"))
}

fn normalize_digest_field(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    if let Some((_, digest)) = value.split_once('@') {
        return normalize_digest(digest).ok();
    }
    if value.starts_with("sha256:") {
        return normalize_digest(value).ok();
    }
    None
}

fn image_ref_is_local_only(image: &str) -> bool {
    image.contains('@')
        || image.starts_with("vellum-proxy:")
        || image.ends_with(":local")
        || image.contains("localhost/")
}

fn parse_host_port(ports: &str) -> Option<u16> {
    let marker = "127.0.0.1:";
    let idx = ports.find(marker)?;
    let rest = &ports[idx + marker.len()..];
    let port = rest.split(['-', '>', '/', ' ']).next()?;
    port.parse().ok()
}

#[derive(Debug, Default)]
pub struct FakeDockerClient {
    pub containers: std::sync::Mutex<Vec<ContainerSummary>>,
    pub readyz: std::sync::Mutex<bool>,
    pub readyz_install_id: std::sync::Mutex<Option<String>>,
    pub readyz_config_hash: std::sync::Mutex<Option<String>>,
    pub responses_websocket: std::sync::Mutex<bool>,
    pub pulled: std::sync::Mutex<Vec<String>>,
    /// RepoDigests map (manifest digests only).
    pub repo_digests: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
    /// Image config ids (`.Id`); never treated as RepoDigest.
    pub image_ids: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
    pub last_run_image: std::sync::Mutex<Option<String>>,
    pub last_run_user: std::sync::Mutex<Option<String>>,
}

impl DockerClient for FakeDockerClient {
    fn list_proxy_containers(&self) -> Result<Vec<ContainerSummary>, String> {
        Ok(self.containers.lock().expect("poisoned").clone())
    }

    fn run_proxy_container(&self, spec: &RunProxySpec) -> Result<ContainerSummary, String> {
        *self.last_run_image.lock().expect("poisoned") = Some(spec.image.clone());
        *self.last_run_user.lock().expect("poisoned") = Some(spec.user.clone());
        let summary = ContainerSummary {
            id: format!("ctr-{}", ulid::Ulid::new()),
            name: spec.name.clone(),
            image: spec.image.clone(),
            running: true,
            labels: spec.labels.as_map(),
            host_port: Some(spec.host_port),
        };
        self.containers
            .lock()
            .expect("poisoned")
            .push(summary.clone());
        Ok(summary)
    }

    fn stop_container(&self, id: &str) -> Result<(), String> {
        let mut guard = self.containers.lock().expect("poisoned");
        if let Some(item) = guard.iter_mut().find(|c| c.id == id) {
            item.running = false;
            Ok(())
        } else {
            Err(format!("container not found: {id}"))
        }
    }

    fn remove_container(&self, id: &str) -> Result<(), String> {
        let mut guard = self.containers.lock().expect("poisoned");
        let before = guard.len();
        guard.retain(|c| c.id != id);
        if guard.len() == before {
            Err(format!("container not found: {id}"))
        } else {
            Ok(())
        }
    }

    fn inspect_readyz(&self, _host_port: u16, _boundary_key: &str) -> Result<bool, String> {
        Ok(*self.readyz.lock().expect("poisoned"))
    }

    fn inspect_readyz_identity(
        &self,
        _host_port: u16,
        _boundary_key: &str,
    ) -> Result<ReadyzProbe, String> {
        Ok(ReadyzProbe {
            ready: *self.readyz.lock().expect("poisoned"),
            install_id: self.readyz_install_id.lock().expect("poisoned").clone(),
            config_hash: self.readyz_config_hash.lock().expect("poisoned").clone(),
        })
    }

    fn inspect_responses_websocket(
        &self,
        _host_port: u16,
        _boundary_key: &str,
    ) -> Result<bool, String> {
        Ok(*self.responses_websocket.lock().expect("poisoned"))
    }

    fn pull_image(&self, image: &str) -> Result<(), String> {
        self.pulled
            .lock()
            .expect("poisoned")
            .push(image.to_string());
        Ok(())
    }

    fn inspect_image_identity(&self, image: &str) -> Result<ImageIdentity, String> {
        if let Some(digest) = digest_from_ref(image) {
            return Ok(ImageIdentity {
                repo_digest: Some(digest),
                image_id: None,
            });
        }
        Ok(ImageIdentity {
            repo_digest: self
                .repo_digests
                .lock()
                .expect("poisoned")
                .get(image)
                .cloned(),
            image_id: self.image_ids.lock().expect("poisoned").get(image).cloned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_match_requires_identity() {
        let labels = ManagedContainerLabels {
            host_id: "h1".into(),
            install_id: "i1".into(),
            config_hash: "c1".into(),
            image_version: "v1".into(),
        };
        let mut map = labels.as_map();
        assert!(labels.matches(&map));
        map.insert("io.vellum.install-id".into(), "other".into());
        assert!(!labels.matches(&map));
    }

    #[test]
    fn pin_image_ref_replaces_tag() {
        assert_eq!(
            pin_image_ref("ghcr.io/acme/proxy:1.2.3", "sha256:abc"),
            "ghcr.io/acme/proxy@sha256:abc"
        );
    }

    #[test]
    fn resolve_rejects_digest_mismatch() {
        let docker = FakeDockerClient::default();
        docker.repo_digests.lock().unwrap().insert(
            "proxy:1".into(),
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        );
        let err = resolve_image_with(
            &docker,
            "proxy:1",
            Some("sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        )
        .unwrap_err();
        assert!(err.contains("ImageDigestMismatch"));
    }

    #[test]
    fn local_image_without_repo_digest_keeps_tag_run_ref() {
        let docker = FakeDockerClient::default();
        docker.image_ids.lock().unwrap().insert(
            "vellum-proxy:local".into(),
            "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into(),
        );
        let resolved = resolve_image_with(&docker, "vellum-proxy:local", None).unwrap();
        assert_eq!(resolved.run_ref, "vellum-proxy:local");
        assert!(resolved.repo_digest.is_none());
        assert_eq!(
            resolved.image_id.as_deref(),
            Some("sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc")
        );
    }

    #[test]
    fn registry_image_pins_repo_digest() {
        let docker = FakeDockerClient::default();
        docker.repo_digests.lock().unwrap().insert(
            "ghcr.io/acme/proxy:1.0".into(),
            "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".into(),
        );
        docker.image_ids.lock().unwrap().insert(
            "ghcr.io/acme/proxy:1.0".into(),
            "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into(),
        );
        let resolved = resolve_image_with(&docker, "ghcr.io/acme/proxy:1.0", None).unwrap();
        assert_eq!(
            resolved.run_ref,
            "ghcr.io/acme/proxy@sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
        );
        assert_eq!(
            resolved.repo_digest.as_deref(),
            Some("sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd")
        );
    }
}
