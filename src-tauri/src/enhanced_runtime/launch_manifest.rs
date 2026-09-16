//! The credential-free launch manifest Vellum writes and the bridge reads.
//!
//! Before this file existed the bridge was configured through a dozen
//! `VELLUM_*` environment variables, which only survive if the process that
//! starts Codex Desktop also happens to be Vellum. Codex Desktop starts
//! `CODEX_CLI_PATH` on its own schedule, so the bridge has to be able to
//! configure itself from disk. The manifest is that file: identity only, no
//! credentials, replaced atomically, and stamped with a launch id so an
//! attestation can be matched to the launch that produced it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use vellum_enhanced_codex::field_name_is_forbidden;

use super::atomic::write_atomic;
use super::manifest::FeatureFlags;

pub const LAUNCH_MANIFEST_SCHEMA_VERSION: u32 = 1;
/// Test and qualification override. Production Desktop launches never set it.
pub const LAUNCH_MANIFEST_ENV: &str = "VELLUM_CODEX_LAUNCH_MANIFEST";
const LAUNCH_MANIFEST_FILE: &str = "enhanced-runtime/launch-manifest.json";
pub const ATTESTATION_FILE: &str = "enhanced-runtime/bridge-attestation.json";
pub const QUALIFICATION_JOURNAL_FILE: &str = "enhanced-runtime/qualification-journal.jsonl";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeBinaryIdentity {
    pub executable: PathBuf,
    /// `sha256:<hex>` of the executable at the moment Vellum verified it.
    pub artifact_sha256: String,
    /// Runtime digest recorded in durable thread bindings.
    pub runtime_digest: String,
    pub codex_home: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchManifestV1 {
    pub schema_version: u32,
    pub launch_id: String,
    pub created_at: i64,
    pub official: RuntimeBinaryIdentity,
    pub enhanced: RuntimeBinaryIdentity,
    pub model_provider_map_path: PathBuf,
    pub model_provider_map_sha256: String,
    pub binding_db: PathBuf,
    pub official_provider_ids: Vec<String>,
    pub third_party_provider_ids: Vec<String>,
    pub feature_profile: FeatureFlags,
    pub enhanced_commit: String,
    pub attestation_path: PathBuf,
    pub qualification_journal_path: PathBuf,
    #[serde(default)]
    pub relay: Option<RelayBinaryIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayBinaryIdentity {
    pub executable: PathBuf,
    pub artifact_sha256: String,
}

impl LaunchManifestV1 {
    /// Where the bridge looks when Codex Desktop starts it with none of our
    /// environment. `LAUNCH_MANIFEST_ENV` overrides it for gates and tests.
    pub fn default_path() -> PathBuf {
        std::env::var_os(LAUNCH_MANIFEST_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::state::app_data_dir().join(LAUNCH_MANIFEST_FILE))
    }

    pub fn path_in(data_root: &Path) -> PathBuf {
        data_root.join(LAUNCH_MANIFEST_FILE)
    }

    pub fn read(path: &Path) -> Result<Self, LaunchManifestError> {
        let bytes = std::fs::read(path).map_err(|error| LaunchManifestError::Read {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        let manifest = serde_json::from_slice::<Self>(&bytes)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn write(&self, path: &Path) -> Result<(), LaunchManifestError> {
        self.validate()?;
        write_atomic(path, &serde_json::to_vec_pretty(self)?).map_err(|error| {
            LaunchManifestError::Write {
                path: path.to_path_buf(),
                message: error.to_string(),
            }
        })
    }

    pub fn validate(&self) -> Result<(), LaunchManifestError> {
        if self.schema_version != LAUNCH_MANIFEST_SCHEMA_VERSION {
            return Err(LaunchManifestError::UnsupportedSchema(self.schema_version));
        }
        if self.launch_id.trim().is_empty() {
            return Err(LaunchManifestError::Incomplete("launchId"));
        }
        if self.official_provider_ids.is_empty() {
            return Err(LaunchManifestError::Incomplete("officialProviderIds"));
        }
        if self.third_party_provider_ids.is_empty() {
            return Err(LaunchManifestError::Incomplete("thirdPartyProviderIds"));
        }
        for identity in [&self.official, &self.enhanced] {
            if !identity.executable.is_absolute() {
                return Err(LaunchManifestError::RelativePath(
                    identity.executable.clone(),
                ));
            }
            if !identity.codex_home.is_absolute() {
                return Err(LaunchManifestError::RelativePath(
                    identity.codex_home.clone(),
                ));
            }
            if !identity.artifact_sha256.starts_with("sha256:")
                || identity.runtime_digest.trim().is_empty()
            {
                return Err(LaunchManifestError::Incomplete("runtime identity"));
            }
        }
        self.assert_credential_free()?;
        Ok(())
    }

    /// The manifest lives at a predictable path in the user profile, so it must
    /// never become a place a credential can end up by accident.
    pub fn assert_credential_free(&self) -> Result<(), LaunchManifestError> {
        let value = serde_json::to_value(self)?;
        let mut offending = None;
        walk_keys(&value, &mut |key| {
            if offending.is_none() && field_name_is_forbidden(key) {
                offending = Some(key.to_string());
            }
        });
        match offending {
            Some(key) => Err(LaunchManifestError::CredentialField(key)),
            None => Ok(()),
        }
    }

    /// Recomputes both binary hashes and the model map hash from disk.
    pub fn verify_on_disk(&self) -> Result<(), LaunchManifestError> {
        if let Some(relay) = &self.relay {
            let actual = sha256_file(&relay.executable)?;
            if !relay.executable.is_absolute() || actual != relay.artifact_sha256 {
                return Err(LaunchManifestError::ArtifactMismatch {
                    name: "relay",
                    expected: relay.artifact_sha256.clone(),
                    actual,
                });
            }
        }
        for (name, identity) in [("official", &self.official), ("enhanced", &self.enhanced)] {
            let actual = sha256_file(&identity.executable)?;
            if actual != identity.artifact_sha256 {
                return Err(LaunchManifestError::ArtifactMismatch {
                    name,
                    expected: identity.artifact_sha256.clone(),
                    actual,
                });
            }
        }
        let map = sha256_file(&self.model_provider_map_path)?;
        if map != self.model_provider_map_sha256 {
            return Err(LaunchManifestError::ArtifactMismatch {
                name: "model provider map",
                expected: self.model_provider_map_sha256.clone(),
                actual: map,
            });
        }
        Ok(())
    }
}

fn walk_keys(value: &Value, visit: &mut impl FnMut(&str)) {
    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                visit(key);
                walk_keys(nested, visit);
            }
        }
        Value::Array(items) => {
            for nested in items {
                walk_keys(nested, visit);
            }
        }
        _ => {}
    }
}

#[derive(Clone)]
struct DigestCacheEntry {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
    digest: String,
    checked_at: Instant,
}

static DIGEST_CACHE: OnceLock<Mutex<Vec<DigestCacheEntry>>> = OnceLock::new();

pub fn sha256_file(path: &Path) -> Result<String, LaunchManifestError> {
    let metadata = std::fs::metadata(path).map_err(|error| LaunchManifestError::Read {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let modified = metadata.modified().ok();
    let cache = DIGEST_CACHE.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(entries) = cache.lock() {
        if let Some(entry) = entries.iter().find(|entry| {
            entry.path == canonical
                && entry.len == metadata.len()
                && entry.modified == modified
                && entry.checked_at.elapsed() <= Duration::from_secs(300)
        }) {
            return Ok(entry.digest.clone());
        }
    }

    let mut file = std::fs::File::open(path).map_err(|error| LaunchManifestError::Read {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| LaunchManifestError::Read {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = format!("sha256:{}", hex::encode(hasher.finalize()));
    if let Ok(mut entries) = cache.lock() {
        entries.retain(|entry| entry.path != canonical);
        entries.push(DigestCacheEntry {
            path: canonical,
            len: metadata.len(),
            modified,
            digest: digest.clone(),
            checked_at: Instant::now(),
        });
        if entries.len() > 16 {
            entries.remove(0);
        }
    }
    Ok(digest)
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchManifestError {
    #[error("unsupported launch manifest schema {0}")]
    UnsupportedSchema(u32),
    #[error("launch manifest is missing {0}")]
    Incomplete(&'static str),
    #[error("launch manifest path must be absolute: {}", .0.display())]
    RelativePath(PathBuf),
    #[error("launch manifest must not carry credentials; found field `{0}`")]
    CredentialField(String),
    #[error("{name} hash mismatch: expected {expected}, got {actual}")]
    ArtifactMismatch {
        name: &'static str,
        expected: String,
        actual: String,
    },
    #[error("cannot read {}: {message}", path.display())]
    Read { path: PathBuf, message: String },
    #[error("cannot write {}: {message}", path.display())]
    Write { path: PathBuf, message: String },
    #[error("launch manifest JSON is invalid: {0}")]
    Json(String),
}

impl From<serde_json::Error> for LaunchManifestError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn identity(root: &Path, name: &str) -> RuntimeBinaryIdentity {
        let executable = root.join(format!("{name}.exe"));
        fs::write(&executable, name.as_bytes()).unwrap();
        RuntimeBinaryIdentity {
            artifact_sha256: sha256_file(&executable).unwrap(),
            executable,
            runtime_digest: format!("sha256:{name}-digest"),
            codex_home: root.join(format!("{name}-home")),
        }
    }

    fn fixture(root: &Path) -> LaunchManifestV1 {
        let map_path = root.join("model-provider-map.json");
        fs::write(&map_path, b"{}").unwrap();
        LaunchManifestV1 {
            schema_version: LAUNCH_MANIFEST_SCHEMA_VERSION,
            launch_id: "launch-1".into(),
            created_at: 10,
            official: identity(root, "official"),
            enhanced: identity(root, "enhanced"),
            model_provider_map_sha256: sha256_file(&map_path).unwrap(),
            model_provider_map_path: map_path,
            binding_db: root.join("bindings.sqlite3"),
            official_provider_ids: vec!["openai-official".into()],
            third_party_provider_ids: vec!["qwen".into()],
            feature_profile: super::super::FEATURE_DEFAULTS_MVP.into(),
            enhanced_commit: "c".repeat(40),
            attestation_path: root.join("attestation.json"),
            qualification_journal_path: root.join("journal.jsonl"),
            relay: None,
        }
    }

    #[test]
    fn round_trips_and_verifies_hashes_on_disk() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = fixture(temp.path());
        let path = temp.path().join("launch-manifest.json");
        manifest.write(&path).unwrap();
        let loaded = LaunchManifestV1::read(&path).unwrap();
        assert_eq!(loaded, manifest);
        loaded.verify_on_disk().unwrap();

        fs::write(&loaded.enhanced.executable, b"tampered").unwrap();
        assert!(matches!(
            loaded.verify_on_disk(),
            Err(LaunchManifestError::ArtifactMismatch {
                name: "enhanced",
                ..
            })
        ));
    }

    #[test]
    fn a_credential_shaped_field_never_survives_a_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = fixture(temp.path());
        let mut value = serde_json::to_value(&manifest).unwrap();
        value["authorization"] = Value::String("Bearer redacted".into());
        let path = temp.path().join("tainted.json");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let loaded = LaunchManifestV1::read(&path).unwrap();
        loaded.assert_credential_free().unwrap();
        assert!(!serde_json::to_string(&loaded).unwrap().contains("Bearer"));
    }

    #[test]
    fn relative_paths_and_missing_providers_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let mut manifest = fixture(temp.path());
        manifest.third_party_provider_ids.clear();
        assert!(matches!(
            manifest.validate(),
            Err(LaunchManifestError::Incomplete("thirdPartyProviderIds"))
        ));

        let mut manifest = fixture(temp.path());
        manifest.enhanced.executable = PathBuf::from("enhanced.exe");
        assert!(matches!(
            manifest.validate(),
            Err(LaunchManifestError::RelativePath(_))
        ));
    }
}
