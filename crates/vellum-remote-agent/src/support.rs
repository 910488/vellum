//! Redacted support-bundle generation.

use std::fs::{self, OpenOptions};
use std::io::Read;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::state::AgentPaths;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SupportBundleResult {
    pub path: String,
    pub sha256: String,
    pub redacted: bool,
}

pub fn create_support_bundle(
    paths: &AgentPaths,
    host_status: Value,
) -> Result<SupportBundleResult, String> {
    paths.ensure()?;
    let document = json!({
        "schemaVersion": 1,
        "createdAt": Utc::now(),
        "redaction": {
            "secrets": "excluded",
            "authorizationHeaders": "excluded",
            "authJson": "excluded"
        },
        "hostStatus": host_status,
        "filesystem": {
            "stateRoot": paths.root,
            "proxyConfigPresent": paths.proxy_config_dir.join("proxy.toml").is_file(),
            "credentialFileCount": count_regular_files(&paths.secrets_dir)?,
            "profileCount": count_profile_records(&paths.profiles_dir)?,
            "leaseCount": count_regular_files(&paths.leases_dir)?
        }
    });
    let status_bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| format!("encode support bundle: {error}"))?;
    let path = paths
        .logs_dir
        .join(format!("support-bundle-{}.tar", ulid::Ulid::new()));
    let operation_summary = operation_summary(paths)?;
    let manifest = serde_json::to_vec_pretty(&json!({
        "schemaVersion": 2,
        "format": "sanitized-tar",
        "includesTranscript": false,
        "includesSecrets": false,
        "files": ["status.json", "operations.json"]
    }))
    .map_err(|error| error.to_string())?;
    atomic_private_archive(
        &path,
        &[
            ("manifest.json", manifest.as_slice()),
            ("status.json", status_bytes.as_slice()),
            ("operations.json", operation_summary.as_slice()),
        ],
    )?;
    let mut archive = fs::File::open(&path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    archive
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let sha256 = hex::encode(Sha256::digest(&bytes));
    Ok(SupportBundleResult {
        path: path.to_string_lossy().to_string(),
        sha256,
        redacted: true,
    })
}

fn operation_summary(paths: &AgentPaths) -> Result<Vec<u8>, String> {
    let mut summaries = Vec::new();
    if paths.operations_dir.is_dir() {
        for entry in fs::read_dir(&paths.operations_dir)
            .map_err(|error| format!("read operations: {error}"))?
            .filter_map(Result::ok)
        {
            let Ok(raw) = fs::read(entry.path()) else {
                continue;
            };
            let Ok(value) = serde_json::from_slice::<Value>(&raw) else {
                continue;
            };
            summaries.push(json!({
                "operationId": value.get("operationId"),
                "method": value.get("method"),
                "state": value.get("state"),
                "createdAt": value.get("createdAt"),
                "completedAt": value.get("completedAt"),
                "hasError": value.get("error").is_some_and(|item| !item.is_null())
            }));
        }
    }
    serde_json::to_vec_pretty(&summaries).map_err(|error| error.to_string())
}

fn atomic_private_archive(path: &std::path::Path, files: &[(&str, &[u8])]) -> Result<(), String> {
    let temporary = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    let mut archive = tar::Builder::new(file);
    for (name, bytes) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        archive
            .append_data(&mut header, name, *bytes)
            .map_err(|error| error.to_string())?;
    }
    archive.finish().map_err(|error| error.to_string())?;
    drop(archive);
    fs::rename(&temporary, path).map_err(|error| error.to_string())
}

fn count_regular_files(path: &std::path::Path) -> Result<usize, String> {
    if !path.exists() {
        return Ok(0);
    }
    Ok(fs::read_dir(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_file())
        .count())
}

fn count_profile_records(path: &std::path::Path) -> Result<usize, String> {
    if !path.exists() {
        return Ok(0);
    }
    Ok(fs::read_dir(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("profile.json").is_file())
        .count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_exports_normally_for_a_legacy_or_broken_persisted_config() {
        // Before `public_status` was made lenient, `host.status` hard-failed
        // against a schema-1/broken config -- and since `SupportBundle`'s RPC
        // handler builds its `host_status` argument by calling `host_status`
        // first, that failure took the support bundle down with it too,
        // exactly when a stuck deployment needed it most. This bundle
        // embeds only the bounded state/schemaVersion/issue shape
        // (`PublicConfigurationStatus`), never the raw persisted TOML.
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        paths.ensure().unwrap();
        let legacy_status = json!({
            "hostId": "host-a",
            "configuration": {
                "present": true,
                "state": "repairRequired",
                "schemaVersion": 2,
                "requiresReconfigure": true,
                "issue": "missingRequiredField",
                "configHash": null,
                "credentialRefs": [],
                "credentialsReady": false
            }
        });
        let result = create_support_bundle(&paths, legacy_status).unwrap();
        let bytes = fs::read(result.path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("repairRequired"));
        assert!(text.contains("missingRequiredField"));
        assert!(result.redacted);
    }

    #[test]
    fn bundle_reports_secret_count_but_never_secret_contents() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        paths.ensure().unwrap();
        fs::write(paths.secrets_dir.join("provider-a"), "do-not-leak").unwrap();
        let result = create_support_bundle(&paths, json!({"hostId": "host-a"})).unwrap();
        let bytes = fs::read(result.path).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("do-not-leak"));
        assert!(result.redacted);
    }
}
