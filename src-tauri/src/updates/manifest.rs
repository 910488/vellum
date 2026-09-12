//! Signed update manifest. Signature is over the raw JSON bytes; SHA-256 of
//! each asset is checked only after the signature verifies.

use std::path::Path;

use ed25519_dalek::{Signature, Verifier};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::trust::TrustStore;
use super::UpdateComponent;

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateManifest {
    pub schema_version: u32,
    pub component: UpdateComponent,
    pub version: String,
    pub source_commit: String,
    pub release_tag: String,
    pub sequence: u64,
    pub key_id: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub release_notes: Option<String>,
    pub min_desktop_version: String,
    pub bridge_api_compat: CompatRange,
    pub remote_protocol_compat: CompatRange,
    #[serde(default)]
    pub data_format: DataFormat,
    pub assets: Vec<AssetRef>,
    #[serde(default)]
    pub core: Option<CoreManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DataFormat {
    #[serde(default = "truth")]
    pub compatible: bool,
    #[serde(default)]
    pub irreversible_migration: bool,
    #[serde(default = "truth")]
    pub rollback_compatible: bool,
}

fn truth() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetRef {
    pub platform: String,
    pub arch: String,
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoreManifest {
    pub upstream_commit: String,
    pub feature_profile: String,
    pub helpers: Vec<String>,
    pub protocol_schema_sha256: String,
    pub protocol_compat: CompatRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CompatRange(pub String);

impl CompatRange {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn matches(&self, version: &str) -> bool {
        range_matches(&self.0, version)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("update manifest is not valid JSON: {0}")]
    Json(String),
    #[error("update manifest schema {0} is not supported")]
    Schema(u32),
    #[error("update manifest is missing required fields")]
    Incomplete,
    #[error("update signature is missing or not 64 bytes")]
    SignatureLength,
    #[error("no trusted key with id {0}")]
    UnknownKey(String),
    #[error("update signature verification failed")]
    BadSignature,
    #[error("asset {0} SHA-256 mismatch")]
    BadHash(String),
    #[error("asset {0} is missing")]
    MissingAsset(String),
}

/// Verify Ed25519 over `raw` then parse. Hashing the JSON is not trust.
pub fn verify_signed_manifest(
    raw: &[u8],
    signature: &[u8],
    trust: &TrustStore,
) -> Result<UpdateManifest, ManifestError> {
    if signature.len() != 64 {
        return Err(ManifestError::SignatureLength);
    }
    let parsed: UpdateManifest =
        serde_json::from_slice(raw).map_err(|error| ManifestError::Json(error.to_string()))?;
    if parsed.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(ManifestError::Schema(parsed.schema_version));
    }
    if parsed.version.trim().is_empty()
        || parsed.release_tag.trim().is_empty()
        || parsed.assets.is_empty()
        || parsed.key_id.trim().is_empty()
    {
        return Err(ManifestError::Incomplete);
    }
    let Some(key) = trust.key_for(&parsed.key_id) else {
        return Err(ManifestError::UnknownKey(parsed.key_id));
    };
    let sig_bytes: [u8; 64] = signature
        .try_into()
        .map_err(|_| ManifestError::SignatureLength)?;
    let signature = Signature::from_bytes(&sig_bytes);
    key.verify(raw, &signature)
        .map_err(|_| ManifestError::BadSignature)?;
    Ok(parsed)
}

/// Asset integrity after a signed manifest has already been accepted.
pub fn verify_asset_sha256(bytes: &[u8], expected: &str) -> Result<(), ManifestError> {
    let expected = expected
        .strip_prefix("sha256:")
        .unwrap_or(expected)
        .to_ascii_lowercase();
    let actual = hex::encode(Sha256::digest(bytes));
    if actual != expected {
        return Err(ManifestError::BadHash(actual));
    }
    Ok(())
}

pub fn verify_asset_file(path: &Path, expected: &str) -> Result<(), ManifestError> {
    let bytes =
        std::fs::read(path).map_err(|_| ManifestError::MissingAsset(path.display().to_string()))?;
    verify_asset_sha256(&bytes, expected)
}

pub fn current_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

pub fn current_arch() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "unknown"
    }
}

pub fn sign_raw(raw: &[u8], signing: &ed25519_dalek::SigningKey) -> [u8; 64] {
    use ed25519_dalek::Signer;
    signing.sign(raw).to_bytes()
}

fn range_matches(spec: &str, version: &str) -> bool {
    let Some(ver) = parse_version(version) else {
        return false;
    };
    let spec = spec.trim();
    if spec == "*" || spec.is_empty() {
        return true;
    }
    spec.split(|ch: char| ch == ',' || ch.is_whitespace())
        .filter(|part| !part.is_empty())
        .all(|part| {
            let part = part.trim();
            if let Some(rest) = part.strip_prefix(">=") {
                parse_version(rest)
                    .map(|bound| ver >= bound)
                    .unwrap_or(false)
            } else if let Some(rest) = part.strip_prefix("<=") {
                parse_version(rest)
                    .map(|bound| ver <= bound)
                    .unwrap_or(false)
            } else if let Some(rest) = part.strip_prefix('<') {
                parse_version(rest)
                    .map(|bound| ver < bound)
                    .unwrap_or(false)
            } else if let Some(rest) = part.strip_prefix('>') {
                parse_version(rest)
                    .map(|bound| ver > bound)
                    .unwrap_or(false)
            } else if let Some(rest) = part.strip_prefix('=') {
                parse_version(rest)
                    .map(|bound| ver == bound)
                    .unwrap_or(false)
            } else {
                parse_version(part)
                    .map(|bound| ver == bound)
                    .unwrap_or(false)
            }
        })
}

pub(crate) fn normalize_semver(value: &str) -> String {
    let value = value.trim().trim_start_matches('v');
    let parts: Vec<&str> = value.split('.').collect();
    match parts.len() {
        1 => format!("{}.0.0", parts[0]),
        2 => format!("{}.{}.0", parts[0], parts[1]),
        _ => value.to_string(),
    }
}

pub fn parse_version(value: &str) -> Option<semver::Version> {
    semver::Version::parse(&normalize_semver(value)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updates::trust::TrustStore;
    use ed25519_dalek::SigningKey;

    fn sample_manifest(key_id: &str) -> (Vec<u8>, UpdateManifest) {
        let manifest = UpdateManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            component: UpdateComponent::Desktop,
            version: "0.3.0".into(),
            source_commit: "abc".into(),
            release_tag: "desktop-v0.3.0".into(),
            sequence: 3,
            key_id: key_id.into(),
            prerelease: false,
            release_notes: Some("notes".into()),
            min_desktop_version: "0.2.0".into(),
            bridge_api_compat: CompatRange::new(">=1.0.0 <2.0.0"),
            remote_protocol_compat: CompatRange::new(">=3 <5"),
            data_format: DataFormat::default(),
            assets: vec![AssetRef {
                platform: "windows".into(),
                arch: "x64".into(),
                name: "Vellum_0.3.0_x64-setup.exe".into(),
                size: 4,
                sha256: hex::encode(Sha256::digest(b"nsis")),
            }],
            core: None,
        };
        let raw = serde_json::to_vec(&manifest).unwrap();
        (raw, manifest)
    }

    #[test]
    fn signed_manifest_round_trip() {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let trust = TrustStore::from_key("k1", signing.verifying_key());
        let (raw, expected) = sample_manifest("k1");
        let sig = sign_raw(&raw, &signing);
        let parsed = verify_signed_manifest(&raw, &sig, &trust).unwrap();
        assert_eq!(parsed.version, expected.version);
    }

    #[test]
    fn bad_signature_is_rejected_even_when_json_parses() {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let other = SigningKey::from_bytes(&[9u8; 32]);
        let trust = TrustStore::from_key("k1", signing.verifying_key());
        let (raw, _) = sample_manifest("k1");
        let sig = sign_raw(&raw, &other);
        match verify_signed_manifest(&raw, &sig, &trust) {
            Err(ManifestError::BadSignature) => {}
            other => panic!("expected BadSignature, got {other:?}"),
        }
    }

    #[test]
    fn hash_is_not_source_trust() {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let trust = TrustStore::from_key("k1", signing.verifying_key());
        let (raw, _) = sample_manifest("k1");
        // Correct SHA-256 of the JSON must not admit an unsigned manifest.
        let _json_hash = hex::encode(Sha256::digest(&raw));
        assert!(matches!(
            verify_signed_manifest(&raw, &[], &trust),
            Err(ManifestError::SignatureLength)
        ));
        assert!(matches!(
            verify_signed_manifest(&raw, &[0u8; 64], &trust),
            Err(ManifestError::BadSignature)
        ));
    }

    #[test]
    fn asset_hash_mismatch_is_reported() {
        assert!(verify_asset_sha256(b"abc", "00").is_err());
        verify_asset_sha256(b"abc", &hex::encode(Sha256::digest(b"abc"))).unwrap();
    }

    #[test]
    fn compat_range_accepts_compound_bounds() {
        let range = CompatRange::new(">=1.0.0 <2.0.0");
        assert!(range.matches("1.4.2"));
        assert!(!range.matches("2.0.0"));
        assert!(!range.matches("0.9.9"));
    }
}
