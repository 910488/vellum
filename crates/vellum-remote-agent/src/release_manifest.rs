//! Pinned release manifest for remote component installation (M31).
//!
//! The Desktop verifies the manifest embedded into the local installer and
//! stages a digest-pinned artifact; the agent independently verifies the
//! selected artifact digest before the atomic install.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexArtifact {
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodexManifestEntry {
    pub pinned_version: String,
    /// Compatible range for the *running* Codex version, e.g. `0.147.x`.
    pub compatible_range: String,
    pub artifacts: ArtifactSet,
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
    pub digest: String,
}

/// Canonical release schema shared with the Desktop.
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

/// Resolve the artifact for a host architecture. The agent reports
/// `std::env::consts::ARCH` (`aarch64` / `x86_64`) in its inventory.
pub fn select_artifact<'a>(
    manifest: &'a ReleaseManifest,
    arch: &str,
) -> Result<&'a CodexArtifact, String> {
    match arch {
        "aarch64" | "arm64" => Ok(&manifest.codex.artifacts.linux_arm64),
        "x86_64" | "amd64" => Ok(&manifest.codex.artifacts.linux_x64),
        other => Err(format!("CodexArtifactUnsupportedArch: {other}")),
    }
}

/// Numeric dot-segment version compare (`0.147.0` vs `0.147.2`).
pub fn compare_versions(left: &str, right: &str) -> Option<Ordering> {
    let parse = |value: &str| -> Option<Vec<u64>> {
        let candidate = value
            .split_whitespace()
            .find(|part| part.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
            .unwrap_or(value.trim());
        let core = candidate.split(['-', '+']).next().unwrap_or(candidate);
        let segments: Vec<u64> = core
            .split('.')
            .map(|segment| segment.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()?;
        if segments.is_empty() {
            return None;
        }
        Some(segments)
    };
    let left = parse(left)?;
    let right = parse(right)?;
    for (a, b) in left.iter().zip(right.iter()) {
        match a.cmp(b) {
            Ordering::Equal => {}
            ordering => return Some(ordering),
        }
    }
    Some(left.len().cmp(&right.len()))
}

/// Range syntax: `0.147.x` (prefix), `0.147.0` (exact), or `>=0.140,<0.148`
/// (comma-separated comparators).
pub fn version_in_range(version: &str, range: &str) -> bool {
    let range = range.trim();
    if range.is_empty() {
        return false;
    }
    if range.contains(',') {
        return range.split(',').all(|part| version_in_range(version, part));
    }
    if let Some(min) = range.strip_prefix(">=") {
        return compare_versions(version, min).is_some_and(|ordering| ordering != Ordering::Less);
    }
    if let Some(max) = range.strip_prefix('<') {
        return compare_versions(version, max).is_some_and(|ordering| ordering == Ordering::Less);
    }
    let range_token = range
        .split_whitespace()
        .find(|part| part.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
        .unwrap_or(range);
    let normalized_range = range_token.split('+').next().unwrap_or(range_token);
    let version_token = version
        .split_whitespace()
        .find(|part| part.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
        .unwrap_or(version);
    let normalized_version = version_token.split('+').next().unwrap_or(version_token);
    if normalized_range.ends_with(".x") || normalized_range.ends_with('*') {
        let prefix = normalized_range
            .trim_end_matches('*')
            .trim_end_matches(".x");
        return normalized_version.starts_with(&format!("{prefix}."));
    }
    normalized_version == normalized_range
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn manifest_parses_from_json_and_selects_arch() -> Result<(), String> {
        let value = json!({
            "schemaVersion": 2,
            "releaseVersion": "v0.1.2",
            "codex": {
                "pinnedVersion": "0.147.0",
                "compatibleRange": "0.147.x",
                "artifacts": {
                    "linux-x64": {"url": "https://example.invalid/x64", "sha256": "a".repeat(64)},
                    "linux-arm64": {"url": "https://example.invalid/arm64", "sha256": "b".repeat(64)}
                }
            },
            "agent": {
                "version": "0.1.0",
                "artifacts": {
                    "linux-x64": {"url": "https://example.invalid/agent-x64", "sha256": "c".repeat(64)},
                    "linux-arm64": {"url": "https://example.invalid/agent-arm64", "sha256": "c".repeat(64)}
                }
            },
            "broker": {
                "version": "0.1.0",
                "artifacts": {
                    "linux-x64": {"url": "https://example.invalid/broker-x64", "sha256": "d".repeat(64)},
                    "linux-arm64": {"url": "https://example.invalid/broker-arm64", "sha256": "d".repeat(64)}
                }
            },
            "proxy": {
                "image": "ghcr.io/910488/vellum/vellum-proxy:v0.1.2",
                "digest": "sha256:".to_string() + &"e".repeat(64)
            },
            "protocolVersion": 1
        });
        let manifest: ReleaseManifest = serde_json::from_value(value).unwrap();
        assert_eq!(
            select_artifact(&manifest, "aarch64")?.sha256,
            "b".repeat(64)
        );
        assert_eq!(
            select_artifact(&manifest, "x86_64")?.url,
            "https://example.invalid/x64"
        );
        assert!(select_artifact(&manifest, "riscv64").is_err());
        Ok(())
    }

    #[test]
    fn version_compare_and_ranges() {
        assert_eq!(compare_versions("0.147.0", "0.147.2"), Some(Ordering::Less));
        assert_eq!(
            compare_versions("0.147.2", "0.147.0"),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_versions("0.147.0", "0.147.0"),
            Some(Ordering::Equal)
        );
        assert_eq!(compare_versions("0.147.0", "0.148"), Some(Ordering::Less));
        assert_eq!(compare_versions("0.9", "0.10"), Some(Ordering::Less));
        assert_eq!(compare_versions("garbage", "0.1.0"), None);

        assert!(version_in_range("0.147.3", "0.147.x"));
        assert!(!version_in_range("0.148.0", "0.147.x"));
        assert!(version_in_range("0.147.0", "0.147.0"));
        assert!(version_in_range(
            "codex-cli 0.147.0-alpha.6.5",
            "0.147.0-alpha.6.5"
        ));
        assert!(!version_in_range("codex-cli 0.147.0", "0.147.0-alpha.6.5"));
        assert!(!version_in_range("codex-cli 0.147.0-alpha.6.5", "0.147.0"));
        assert!(version_in_range("0.146.0", ">=0.140,<0.148"));
        assert!(!version_in_range("0.148.0", ">=0.140,<0.148"));
        assert!(!version_in_range("0.147.0", ""));
    }
}
