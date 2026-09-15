use crate::error::{AppError, AppResult};
use crate::model::{CatalogVersion, RuntimeNotice};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const MAX_CATALOG_VERSIONS: usize = 5;

pub fn catalog_id(bytes: &[u8]) -> String {
    let digest = format!("{:x}", Sha256::digest(bytes));
    digest[..12].to_string()
}

/// Cache bookkeeping does not change the model contract loaded by Codex.
/// Keep every other field (including unknown future fields) significant.
pub fn catalog_requires_restart(previous: &[u8], current: &[u8]) -> bool {
    fn contract(bytes: &[u8]) -> Option<serde_json::Value> {
        let mut value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        let object = value.as_object_mut()?;
        object.get("models")?.as_array()?;
        for key in ["fetched_at", "etag", "client_version"] {
            object.remove(key);
        }
        Some(value)
    }
    match (contract(previous), contract(current)) {
        (Some(previous), Some(current)) => previous != current,
        _ => true,
    }
}

pub fn snapshot_catalog(root: &Path, catalog: &Path) -> AppResult<Option<CatalogVersion>> {
    let bytes = match std::fs::read(catalog) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(AppError::Message(format!("無法讀取目前模型型錄：{error}")));
        }
    };
    let id = catalog_id(&bytes);
    let history = root.join("catalog-history");
    std::fs::create_dir_all(&history)
        .map_err(|error| AppError::Message(format!("無法建立型錄歷史目錄：{error}")))?;
    let path = history.join(format!("{id}.json"));
    if !path.exists() {
        std::fs::write(&path, bytes)
            .map_err(|error| AppError::Message(format!("無法保存型錄版本：{error}")))?;
    }
    prune_catalog_versions(&history, MAX_CATALOG_VERSIONS)?;
    Ok(Some(CatalogVersion {
        id,
        created_at: modified_timestamp(&path),
        path: path.to_string_lossy().to_string(),
    }))
}

pub fn list_catalog_versions(root: &Path) -> AppResult<Vec<CatalogVersion>> {
    let history = root.join("catalog-history");
    let mut versions = read_versions(&history)?;
    versions.sort_by_key(|version| std::cmp::Reverse(version.created_at));
    Ok(versions)
}

pub fn rollback_catalog(root: &Path, id: &str, target: &Path) -> AppResult<()> {
    let source = root.join("catalog-history").join(format!("{id}.json"));
    if !source.exists() {
        return Err(AppError::Message(format!("找不到型錄版本 {id}")));
    }
    let bytes = std::fs::read(&source)
        .map_err(|error| AppError::Message(format!("無法讀取型錄版本：{error}")))?;
    let tmp = target.with_extension("rollback.tmp");
    std::fs::write(&tmp, bytes)
        .map_err(|error| AppError::Message(format!("無法寫入回滾型錄：{error}")))?;
    std::fs::rename(&tmp, target)
        .map_err(|error| AppError::Message(format!("無法套用回滾型錄：{error}")))
}

fn prune_catalog_versions(history: &Path, keep: usize) -> AppResult<()> {
    let mut versions = read_versions(history)?;
    versions.sort_by_key(|version| std::cmp::Reverse(version.created_at));
    for version in versions.into_iter().skip(keep) {
        std::fs::remove_file(&version.path)
            .map_err(|error| AppError::Message(format!("無法清理舊型錄版本：{error}")))?;
    }
    Ok(())
}

fn read_versions(history: &Path) -> AppResult<Vec<CatalogVersion>> {
    let entries = match std::fs::read_dir(history) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(AppError::Message(format!("無法讀取型錄歷史：{error}")));
        }
    };
    Ok(entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .filter_map(version_from_path)
        .collect())
}

fn version_from_path(path: PathBuf) -> Option<CatalogVersion> {
    Some(CatalogVersion {
        id: path.file_stem()?.to_str()?.to_string(),
        created_at: modified_timestamp(&path),
        path: path.to_string_lossy().to_string(),
    })
}

fn modified_timestamp(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

pub fn restart_precondition(
    active_requests: u64,
    executable: Option<&Path>,
) -> Result<(), RuntimeNotice> {
    if active_requests > 0 {
        return Err(RuntimeNotice::new("restartBlockedByActiveRequests")
            .with("count", active_requests.to_string()));
    }
    let Some(executable) = executable else {
        return Err(RuntimeNotice::new("restartExecutableMissing"));
    };
    if !executable.is_absolute() || !executable.exists() {
        return Err(RuntimeNotice::new("restartExecutableInvalid"));
    }
    Ok(())
}

pub fn codex_launch_spec(app_id: Option<&str>, executable: &Path) -> (PathBuf, Vec<String>) {
    match app_id.filter(|value| !value.trim().is_empty()) {
        Some(app_id) => (
            PathBuf::from("explorer.exe"),
            vec![format!(r"shell:AppsFolder\{app_id}")],
        ),
        None => (executable.to_path_buf(), Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_restart_ignores_cache_metadata_but_preserves_model_changes() {
        let original = serde_json::json!({
            "fetched_at": "old", "etag": "old", "client_version": "old",
            "models": [{"slug": "qwen", "context_window": 128000,
                "default_reasoning_level": "medium"}]
        });
        let mut refreshed = original.clone();
        for key in ["fetched_at", "etag", "client_version"] {
            refreshed[key] = "new".into();
        }
        let bytes = serde_json::to_vec(&original).unwrap();
        let encode = |value: &serde_json::Value| serde_json::to_vec_pretty(value).unwrap();
        assert!(!catalog_requires_restart(&bytes, &encode(&refreshed)));
        for (key, value) in [
            ("context_window", serde_json::json!(256000)),
            ("default_reasoning_level", serde_json::json!("high")),
            ("new_runtime_field", serde_json::json!(true)),
        ] {
            let mut changed = refreshed.clone();
            changed["models"][0][key] = value;
            assert!(catalog_requires_restart(&bytes, &encode(&changed)));
        }
        refreshed["models"] = serde_json::json!([]);
        assert!(catalog_requires_restart(&bytes, &encode(&refreshed)));
        assert!(catalog_requires_restart(b"invalid", &bytes));
        assert!(catalog_requires_restart(b"{}", &bytes));
    }

    #[test]
    fn catalog_snapshots_are_deduplicated_and_rollback_is_exact() {
        let temp = tempfile::tempdir().unwrap();
        let catalog = temp.path().join("catalog.json");
        std::fs::write(&catalog, b"{\"models\":[1]}").unwrap();
        let first = snapshot_catalog(temp.path(), &catalog).unwrap().unwrap();
        let duplicate = snapshot_catalog(temp.path(), &catalog).unwrap().unwrap();
        assert_eq!(first.id, duplicate.id);
        std::fs::write(&catalog, b"{\"models\":[2]}").unwrap();
        snapshot_catalog(temp.path(), &catalog).unwrap();
        rollback_catalog(temp.path(), &first.id, &catalog).unwrap();
        assert_eq!(std::fs::read(catalog).unwrap(), b"{\"models\":[1]}");
    }

    #[test]
    fn restart_is_refused_with_active_requests_or_unverified_path() {
        assert_eq!(
            restart_precondition(1, None).unwrap_err().code,
            "restartBlockedByActiveRequests"
        );
        assert_eq!(
            restart_precondition(0, None).unwrap_err().code,
            "restartExecutableMissing"
        );
    }

    #[test]
    fn restart_accepts_an_existing_absolute_executable() {
        let current = std::env::current_exe().unwrap();
        assert!(restart_precondition(0, Some(&current)).is_ok());
    }

    #[test]
    fn packaged_codex_restart_prefers_registered_app_id() {
        let executable = Path::new(r"C:\Program Files\WindowsApps\OpenAI.Codex\ChatGPT.exe");
        let (program, args) = codex_launch_spec(Some("OpenAI.CodexThirdPartyAPI"), executable);
        assert_eq!(program, PathBuf::from("explorer.exe"));
        assert_eq!(
            args,
            [r"shell:AppsFolder\OpenAI.CodexThirdPartyAPI".to_string()]
        );
    }

    #[test]
    fn unpackaged_codex_restart_falls_back_to_discovered_executable() {
        let executable = Path::new(r"C:\Tools\Codex\ChatGPT.exe");
        let (program, args) = codex_launch_spec(None, executable);
        assert_eq!(program, executable);
        assert!(args.is_empty());
    }
}
