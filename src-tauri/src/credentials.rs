use crate::crypto::JournalCipher;
use crate::error::{AppError, AppResult};
use std::path::{Path, PathBuf};

pub fn save(root: &Path, route_id: &str, secret: &str) -> AppResult<()> {
    let cipher = JournalCipher::load_or_create(root)?;
    let encrypted = cipher.seal(secret.as_bytes(), route_id.as_bytes())?;
    let directory = root.join("credentials");
    std::fs::create_dir_all(&directory)
        .map_err(|error| AppError::Message(format!("無法建立憑證目錄：{error}")))?;
    let path = directory.join(file_name(route_id));
    let tmp = path.with_extension("bin.tmp");
    std::fs::write(&tmp, encrypted)
        .map_err(|error| AppError::Message(format!("無法寫入憑證：{error}")))?;
    std::fs::rename(&tmp, path).map_err(|error| AppError::Message(format!("無法替換憑證：{error}")))
}

pub fn load(root: &Path, route_id: &str) -> AppResult<Option<String>> {
    let path = root.join("credentials").join(file_name(route_id));
    let encrypted = match std::fs::read(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(AppError::Message(format!("無法讀取憑證：{error}")));
        }
    };
    let cipher = JournalCipher::load_or_create(root)?;
    let plaintext = cipher.open(&encrypted, route_id.as_bytes())?;
    String::from_utf8(plaintext)
        .map(Some)
        .map_err(|error| AppError::Message(format!("憑證不是有效 UTF-8：{error}")))
}

pub fn remove(root: &Path, route_id: &str) -> AppResult<()> {
    let path = root.join("credentials").join(file_name(route_id));
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::Message(format!("無法移除憑證：{error}"))),
    }
}

/// Credentials staged for a one-time master-key migration (see
/// `crypto::JournalCipher`'s env-key → platform-key recovery). Nothing is
/// written to disk until every entry above has been decrypted under the old
/// key, re-encrypted under the new key, and had that round trip verified —
/// see [`prepare_migration`] / [`commit_migration`].
pub struct StagedCredentialMigration {
    entries: Vec<(PathBuf, Vec<u8>)>,
}

/// Decrypt every stored credential under `old`, re-encrypt under `new`, and
/// verify each round trip — entirely in memory, no disk writes. On any
/// failure, returns an error and leaves every credential file untouched.
pub fn prepare_migration(
    root: &Path,
    old: &JournalCipher,
    new: &JournalCipher,
) -> AppResult<StagedCredentialMigration> {
    let directory = root.join("credentials");
    if !directory.exists() {
        return Ok(StagedCredentialMigration {
            entries: Vec::new(),
        });
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&directory)
        .map_err(|error| AppError::Message(format!("read credentials directory: {error}")))?
    {
        let path = entry
            .map_err(|error| AppError::Message(format!("read credential entry: {error}")))?
            .path();
        if path.extension().and_then(|value| value.to_str()) != Some("bin") {
            continue;
        }
        let file_stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_string();
        let encrypted = std::fs::read(&path).map_err(|error| {
            AppError::Message(format!("read credential for migration: {error}"))
        })?;
        let (plaintext, route_id) = open_stored_file(old, &encrypted, &file_stem)?;
        let re_encrypted = new.seal(&plaintext, route_id.as_bytes())?;
        if new.open(&re_encrypted, route_id.as_bytes())? != plaintext {
            return Err(AppError::Message(format!(
                "credential re-encryption verification failed for {route_id}; original data was not modified"
            )));
        }
        entries.push((path, re_encrypted));
    }
    Ok(StagedCredentialMigration { entries })
}

/// Commit a [`StagedCredentialMigration`] prepared by [`prepare_migration`].
/// Every entry was already decrypted, re-encrypted, and verified before this
/// is called, so the only remaining failure mode is an I/O error while
/// writing — each file is replaced with a write-then-rename so a crash
/// mid-commit leaves individual files either fully old or fully new, never
/// truncated.
pub fn commit_migration(staged: StagedCredentialMigration) -> AppResult<()> {
    for (path, data) in staged.entries {
        let tmp = path.with_extension("bin.tmp");
        std::fs::write(&tmp, &data)
            .map_err(|error| AppError::Message(format!("write migrated credential: {error}")))?;
        std::fs::rename(&tmp, &path)
            .map_err(|error| AppError::Message(format!("commit migrated credential: {error}")))?;
    }
    Ok(())
}

fn file_name(route_id: &str) -> String {
    let safe: String = route_id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .collect();
    format!("{}.bin", if safe.is_empty() { "route" } else { &safe })
}

/// Open a credential discovered by enumerating the credential directory.
///
/// Most persisted IDs are already filename-safe, so their stem is also the
/// AES-GCM AAD. Boundary credentials are the exception: their reserved ID
/// starts and ends with underscores, which `file_name` intentionally removes.
/// Older releases therefore wrote a safe filename while sealing with the
/// original reserved ID. Try the literal stem first so a normal provider with
/// a similar name remains unambiguous, then recover the reserved prefix for
/// local, remote, and sync-marker boundary credentials.
pub(crate) fn open_stored_file(
    cipher: &JournalCipher,
    encrypted: &[u8],
    file_stem: &str,
) -> AppResult<(Vec<u8>, String)> {
    match cipher.open(encrypted, file_stem.as_bytes()) {
        Ok(plaintext) => Ok((plaintext, file_stem.to_string())),
        Err(primary_error) => {
            const SAFE_BOUNDARY_PREFIX: &str = "vellumproxyboundary";
            let Some(suffix) = file_stem.strip_prefix(SAFE_BOUNDARY_PREFIX) else {
                return Err(primary_error);
            };
            let route_id = format!("{}{}", vellum_proxy_runtime::BOUNDARY_CREDENTIAL_ID, suffix);
            cipher
                .open(encrypted, route_id.as_bytes())
                .map(|plaintext| (plaintext, route_id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_round_trip_is_encrypted() {
        let temp = tempfile::tempdir().unwrap();
        save(temp.path(), "route-1", "secret-token").unwrap();
        let bytes = std::fs::read(temp.path().join("credentials/route-1.bin")).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("secret-token"));
        assert_eq!(
            load(temp.path(), "route-1").unwrap().as_deref(),
            Some("secret-token")
        );
    }

    #[test]
    fn enumerated_credentials_recover_boundary_aad_without_rewriting_provider_ids() {
        let cipher = JournalCipher::from_key(vec![7; crate::crypto::KEY_BYTES], "test").unwrap();
        for route_id in [
            vellum_proxy_runtime::BOUNDARY_CREDENTIAL_ID.to_string(),
            format!(
                "{}remote-0123456789abcdef",
                vellum_proxy_runtime::BOUNDARY_CREDENTIAL_ID
            ),
            "opencode-zen".to_string(),
        ] {
            let encrypted = cipher.seal(b"secret", route_id.as_bytes()).unwrap();
            let stem = file_name(&route_id).trim_end_matches(".bin").to_string();
            let (plaintext, recovered_id) = open_stored_file(&cipher, &encrypted, &stem).unwrap();
            assert_eq!(plaintext, b"secret");
            assert_eq!(recovered_id, route_id);
        }
    }
}
