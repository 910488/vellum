//! GitHub Releases listing. `/latest` is never used; the client filters
//! tags and picks the highest compatible signed manifest.

use std::collections::HashMap;
#[cfg(test)]
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use super::UpdateComponent;

#[derive(Debug, Clone, Deserialize)]
pub struct GithubRelease {
    pub tag_name: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubAsset {
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub browser_download_url: String,
}

#[derive(Debug, Clone)]
pub struct ListedReleases {
    pub etag: Option<String>,
    pub not_modified: bool,
    pub rate_limited: bool,
    pub retry_after_secs: u64,
    pub releases: Vec<GithubRelease>,
}

pub trait ReleaseSource: Send + Sync {
    fn list_releases(&self, repo: &str, etag: Option<&str>) -> Result<ListedReleases, String>;
    fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, String>;
}

#[derive(Default)]
pub struct GithubSource {
    etags: Mutex<HashMap<String, String>>,
}

impl GithubSource {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ReleaseSource for GithubSource {
    fn list_releases(&self, repo: &str, etag: Option<&str>) -> Result<ListedReleases, String> {
        if !crate::updates::live_auto_update_enabled() {
            return Ok(ListedReleases {
                etag: etag.map(str::to_string),
                not_modified: false,
                rate_limited: false,
                retry_after_secs: 0,
                releases: Vec::new(),
            });
        }
        let url = format!("https://api.github.com/repos/{repo}/releases?per_page=30");
        let agent = reqwest::blocking::Client::builder()
            .user_agent("vellum-updater")
            .build()
            .map_err(|error| error.to_string())?;
        let mut request = agent
            .get(&url)
            .header("Accept", "application/vnd.github+json");
        if let Some(etag) = etag {
            request = request.header("If-None-Match", etag);
        }
        let response = request.send().map_err(|error| error.to_string())?;
        if response.status().as_u16() == 304 {
            return Ok(ListedReleases {
                etag: etag.map(str::to_string),
                not_modified: true,
                rate_limited: false,
                retry_after_secs: 0,
                releases: Vec::new(),
            });
        }
        if response.status().as_u16() == 403 || response.status().as_u16() == 429 {
            let retry = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok())
                .unwrap_or(60);
            return Ok(ListedReleases {
                etag: None,
                not_modified: false,
                rate_limited: true,
                retry_after_secs: retry,
                releases: Vec::new(),
            });
        }
        if !response.status().is_success() {
            return Err(format!("github releases HTTP {}", response.status()));
        }
        let etag = response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        if let Some(etag) = etag.clone() {
            self.etags
                .lock()
                .expect("etag map")
                .insert(repo.to_string(), etag);
        }
        let body = response.text().map_err(|error| error.to_string())?;
        let releases = parse_github_releases(&body)?;
        Ok(ListedReleases {
            etag,
            not_modified: false,
            rate_limited: false,
            retry_after_secs: 0,
            releases,
        })
    }

    fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
        if !crate::updates::live_auto_update_enabled() {
            return Err("live auto-update is disabled".into());
        }
        let agent = reqwest::blocking::Client::builder()
            .user_agent("vellum-updater")
            .build()
            .map_err(|error| error.to_string())?;
        let response = agent.get(url).send().map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("download HTTP {}", response.status()));
        }
        response
            .bytes()
            .map(|b| b.to_vec())
            .map_err(|error| error.to_string())
    }
}

pub fn parse_github_releases(json: &str) -> Result<Vec<GithubRelease>, String> {
    serde_json::from_str(json).map_err(|error| error.to_string())
}

pub fn matching_tags(
    releases: &[GithubRelease],
    component: UpdateComponent,
) -> Vec<&GithubRelease> {
    let prefix = component.tag_prefix();
    releases
        .iter()
        .filter(|release| !release.draft && release.tag_name.starts_with(prefix))
        .collect()
}

/// Fixture source used by tests and the local unsigned-stand-in path.
pub struct MemorySource {
    pub releases: HashMap<String, Vec<u8>>,
    pub listed: ListedReleases,
}

impl ReleaseSource for MemorySource {
    fn list_releases(&self, _repo: &str, _etag: Option<&str>) -> Result<ListedReleases, String> {
        Ok(self.listed.clone())
    }

    fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>, String> {
        self.releases
            .get(url)
            .cloned()
            .ok_or_else(|| format!("missing fixture {url}"))
    }
}

#[cfg(test)]
pub fn write_fixture_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)
}

pub type SharedSource = Arc<dyn ReleaseSource>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drafts_are_ignored_and_latest_is_not_special() {
        let json = r#"
        [
          {"tag_name":"desktop-v0.1.0","prerelease":false,"draft":false,"published_at":"2026-01-01T00:00:00Z","assets":[]},
          {"tag_name":"desktop-v0.3.0","prerelease":false,"draft":true,"published_at":"2026-09-01T00:00:00Z","assets":[]},
          {"tag_name":"v9.9.9","prerelease":false,"draft":false,"assets":[]},
          {"tag_name":"desktop-v0.2.0","prerelease":false,"draft":false,"published_at":"2026-08-01T00:00:00Z","assets":[]}
        ]
        "#;
        let releases = parse_github_releases(json).unwrap();
        let tags: Vec<_> = matching_tags(&releases, UpdateComponent::Desktop)
            .into_iter()
            .map(|r| r.tag_name.as_str())
            .collect();
        assert_eq!(tags, ["desktop-v0.1.0", "desktop-v0.2.0"]);
    }

    #[test]
    fn fixture_bytes_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asset.bin");
        write_fixture_file(&path, b"ok").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"ok");
    }
}
