//! 對稱加密：AES-256-GCM，金鑰由 Windows DPAPI 包住（doc/03 憑證存放、doc/05 落盤）。
//!
//! 從 cc-switch 的 `proxy/compaction/crypto.rs` 移植，拿掉對 cc-switch 設定層的依賴。
//! byte layout 維持不變（與舊落盤相容）：nonce(12) || tag(16) || ciphertext。

use crate::error::{AppError, AppResult};
use aes_gcm::{
    aead::{Aead, Payload},
    Aes256Gcm, KeyInit, Nonce,
};
use hmac::{Hmac, Mac};
#[cfg(target_os = "macos")]
use sha2::Digest;
use sha2::Sha256;
#[cfg(windows)]
use std::io::Read;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
use zeroize::Zeroizing;

pub const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
const ENV_KEY: &str = "VELLUM_MASTER_KEY";
#[cfg(test)]
thread_local! {
    /// Per-test override for the process-wide master-key environment variable.
    /// Crypto tests use this so the parallel test runner never exposes other
    /// AppState instances to a temporary migration key.
    static TEST_ENV_KEY: std::cell::RefCell<Option<Option<String>>> = const {
        std::cell::RefCell::new(None)
    };
}
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "com.vellum.desktop.master-key";
/// Records which key currently protects the data under `root`, so a future
/// launch can tell "this was encrypted under an env-var key that a packaged
/// build no longer trusts by default" from "this has always been on the
/// platform key store". See [`load_or_create`]'s migration path.
const PROTECTION_MARKER_FILE: &str = "vellum-key.protection";

/// AES-256-GCM with a platform secure master key.
pub struct JournalCipher {
    key: Zeroizing<Vec<u8>>,
    protection: String,
}

impl std::fmt::Debug for JournalCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalCipher")
            .field("protection", &self.protection)
            .finish_non_exhaustive()
    }
}

impl JournalCipher {
    /// 載入或建立一把金鑰。
    ///
    /// 正式封裝的 release build 永遠優先 DPAPI（Windows）／Keychain
    /// （macOS），完全不看 `VELLUM_MASTER_KEY`——任何能在這個使用者底下
    /// 設環境變數的東西（另一個以同帳號跑的程式、被動過手腳的啟動器、設錯
    /// 的捷徑）否則都能替換掉信任的金鑰，讓 DPAPI 的 OS 層保護形同虛設。
    /// 環境變數只在測試／`debug_assertions` 開發建置下生效
    /// （見 [`Self::allow_environment_key`]）。
    ///
    /// 若偵測到既有落盤資料是舊版在信任環境變數時建立的（見保護標記檔），
    /// 會嘗試一次性遷移到平台金鑰（見 [`Self::migrate_from_environment_if_needed`]）。
    pub fn load_or_create(root: &Path) -> AppResult<Self> {
        Self::load_or_create_with_mode(root, Self::allow_environment_key())
    }

    /// 是否為允許 `VELLUM_MASTER_KEY` 的開發／測試建置。封裝好的 release
    /// build 這裡永遠是 `false`。
    fn allow_environment_key() -> bool {
        cfg!(any(test, debug_assertions))
    }

    /// [`load_or_create`] 的可測試核心：`allow_env_key` 模擬「這是開發／測試
    /// 建置（信任 `VELLUM_MASTER_KEY`）」還是「封裝好的 release build（永遠
    /// 不讀）」，讓單測能在同一個 `cfg(test)` 二進位裡驗證兩種行為，不受
    /// 「測試永遠是 debug_assertions」這個事實影響。
    fn load_or_create_with_mode(root: &Path, allow_env_key: bool) -> AppResult<Self> {
        if allow_env_key {
            if let Some(key) = explicit_key()? {
                let cipher = Self::from_key(key, "environment")?;
                record_protection_marker(root, "environment")?;
                return Ok(cipher);
            }
        }

        let platform = Self::load_platform_key(root)?;
        Self::migrate_from_environment_if_needed(root, &platform)?;
        record_protection_marker(root, platform.protection())?;
        Ok(platform)
    }

    fn load_platform_key(root: &Path) -> AppResult<Self> {
        #[cfg(windows)]
        {
            let key_path = root.join("vellum-key.dpapi");
            let key = load_or_create_dpapi_key(&key_path)?;
            Self::from_key(key, "dpapi-current-user")
        }

        #[cfg(not(windows))]
        {
            #[cfg(target_os = "macos")]
            {
                let key = load_or_create_keychain_key(root)?;
                Self::from_key(key, "macos-keychain")
            }

            #[cfg(not(target_os = "macos"))]
            {
                let _ = root;
                Err(AppError::Message(format!(
                    "No secure key store on this platform; set {ENV_KEY} to a {}-byte hex string",
                    KEY_BYTES
                )))
            }
        }
    }

    /// One-time recovery for the master-key priority bug this module used to
    /// have: if the protection marker says existing data was encrypted under
    /// `VELLUM_MASTER_KEY` and that variable still happens to be set,
    /// decrypt the credentials store and history journal with it and
    /// re-encrypt everything under the now-loaded platform key. Every row is
    /// decrypted, re-encrypted, and verified in memory *before* anything is
    /// written to disk (see `credentials::prepare_migration` /
    /// `history::prepare_journal_migration`); nothing is committed, and the
    /// marker is not touched, unless every step above succeeds.
    ///
    /// If the marker says migration is needed but the env var is no longer
    /// set, we cannot recover the old key at all — fail closed with a clear
    /// error rather than silently switching to the platform key (which would
    /// make the existing data permanently undecryptable).
    fn migrate_from_environment_if_needed(root: &Path, platform: &JournalCipher) -> AppResult<()> {
        if read_protection_marker(root).as_deref() != Some("environment") {
            return Ok(());
        }
        let Some(old_key) = explicit_key()? else {
            return Err(AppError::Message(format!(
                "existing Vellum data is still encrypted under {ENV_KEY}, which packaged builds no longer trust automatically; set {ENV_KEY} to the same value one more time to let Vellum migrate it to the platform key store, then unset it"
            )));
        };
        let old_cipher = Self::from_key(old_key, "environment")?;

        // Stage both migrations (decrypt + re-encrypt + verify, no disk
        // writes) before committing either, so a failure partway through
        // never leaves one store migrated and the other not.
        let staged_credentials =
            crate::credentials::prepare_migration(root, &old_cipher, platform)?;
        let history_db_path = root.join("history.sqlite3");
        let staged_history =
            crate::history::prepare_journal_migration(&history_db_path, &old_cipher, platform)?;

        crate::credentials::commit_migration(staged_credentials)?;
        crate::history::commit_journal_migration(staged_history)?;
        // Marker is recorded by the caller (`load_or_create_with_mode`)
        // once this returns `Ok`; leaving it as "environment" here would
        // make a partial failure after this point look already-migrated.
        log::warn!(
            "[Crypto] migrated Vellum's encrypted data from an environment-variable master key to the platform key store ({})",
            platform.protection()
        );
        Ok(())
    }

    /// 直接用給定的金鑰（測試专用；正式流程走 load_or_create）。
    pub fn from_key(key: Vec<u8>, protection: &str) -> AppResult<Self> {
        if key.len() != KEY_BYTES {
            return Err(AppError::Message(format!(
                "master key must be exactly {KEY_BYTES} bytes"
            )));
        }
        Ok(Self {
            key: Zeroizing::new(key),
            protection: protection.to_string(),
        })
    }

    pub fn protection(&self) -> &str {
        &self.protection
    }

    /// 穩定指紋，用來偵測金鑰是否換過（換了舊落盤就解不開）。
    pub fn fingerprint(&self, value: &[u8]) -> AppResult<String> {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(self.key.as_slice())
            .map_err(|_| AppError::Message("invalid fingerprint key".into()))?;
        mac.update(value);
        Ok(format!("{:x}", mac.finalize().into_bytes()))
    }

    /// AES-256-GCM，輸出 layout：nonce(12) || tag(16) || ciphertext。
    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> AppResult<Vec<u8>> {
        let cipher = Aes256Gcm::new_from_slice(self.key.as_slice())
            .map_err(|_| AppError::Message("invalid master key".into()))?;
        let mut nonce = [0u8; NONCE_BYTES];
        getrandom::fill(&mut nonce)
            .map_err(|e| AppError::Message(format!("cannot generate nonce: {e}")))?;
        let encrypted = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| AppError::Message("cannot encrypt payload".into()))?;
        if encrypted.len() < TAG_BYTES {
            return Err(AppError::Message("encrypted payload is truncated".into()));
        }
        // aes-gcm 輸出 ciphertext||tag；我們重排成 nonce||tag||ciphertext 以相容舊落盤。
        let split = encrypted.len() - TAG_BYTES;
        let mut output = Vec::with_capacity(NONCE_BYTES + encrypted.len());
        output.extend_from_slice(&nonce);
        output.extend_from_slice(&encrypted[split..]);
        output.extend_from_slice(&encrypted[..split]);
        Ok(output)
    }

    pub fn open(&self, blob: &[u8], aad: &[u8]) -> AppResult<Vec<u8>> {
        if blob.len() < NONCE_BYTES + TAG_BYTES + 1 {
            return Err(AppError::Message("encrypted payload is truncated".into()));
        }
        let (nonce, remainder) = blob.split_at(NONCE_BYTES);
        let (tag, ciphertext) = remainder.split_at(TAG_BYTES);
        let mut aes_layout = Vec::with_capacity(ciphertext.len() + TAG_BYTES);
        aes_layout.extend_from_slice(ciphertext);
        aes_layout.extend_from_slice(tag);
        let cipher = Aes256Gcm::new_from_slice(self.key.as_slice())
            .map_err(|_| AppError::Message("invalid master key".into()))?;
        cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: &aes_layout,
                    aad,
                },
            )
            .map_err(|_| {
                AppError::Message(
                    "payload authentication failed; original data was not modified".into(),
                )
            })
    }
}

fn protection_marker_path(root: &Path) -> std::path::PathBuf {
    root.join(PROTECTION_MARKER_FILE)
}

fn read_protection_marker(root: &Path) -> Option<String> {
    std::fs::read_to_string(protection_marker_path(root))
        .ok()
        .map(|value| value.trim().to_string())
}

fn record_protection_marker(root: &Path, protection: &str) -> AppResult<()> {
    // load_or_create runs on essentially every credential read/write, not
    // just startup — skip the write+rename when the marker already says
    // what we're about to write.
    if read_protection_marker(root).as_deref() == Some(protection) {
        return Ok(());
    }
    if let Some(parent) = protection_marker_path(root).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| AppError::Message(format!("create key root directory: {error}")))?;
    }
    let path = protection_marker_path(root);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, protection)
        .map_err(|error| AppError::Message(format!("write protection marker: {error}")))?;
    std::fs::rename(&tmp, &path)
        .map_err(|error| AppError::Message(format!("commit protection marker: {error}")))?;
    Ok(())
}

fn explicit_key() -> AppResult<Option<Vec<u8>>> {
    #[cfg(test)]
    let test_override = TEST_ENV_KEY.with(|value| value.borrow().clone());
    #[cfg(not(test))]
    let test_override: Option<Option<String>> = None;
    let raw = match test_override {
        Some(value) => value,
        None => std::env::var(ENV_KEY)
            .ok()
            .filter(|value| !value.is_empty()),
    };
    let Some(raw) = raw else {
        return Ok(None);
    };
    parse_explicit_key(&raw).map(Some)
}

fn parse_explicit_key(raw: &str) -> AppResult<Vec<u8>> {
    let hex: String = raw
        .chars()
        .filter(|c| *c != '_' && !c.is_whitespace())
        .collect();
    if hex.len() != KEY_BYTES * 2 {
        return Err(AppError::Message(format!(
            "{ENV_KEY} must be {} hex characters (got {})",
            KEY_BYTES * 2,
            hex.len()
        )));
    }
    let mut key = Vec::with_capacity(KEY_BYTES);
    let bytes = hex.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        key.push((hi << 4) | lo);
        i += 2;
    }
    Ok(key)
}

fn hex_nibble(b: u8) -> AppResult<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(AppError::Message(format!("invalid hex character: {b:?}"))),
    }
}

#[cfg(target_os = "macos")]
#[allow(clippy::question_mark)]
fn load_or_create_keychain_key(root: &Path) -> AppResult<Vec<u8>> {
    let account = keychain_account(root);
    if let Some(value) = keychain_get(&account)? {
        let key = parse_hex_key(&value)?;
        validate_existing_credentials(root, &key)?;
        return Ok(key);
    }

    let legacy_path = root.join("vellum-master-key");
    let test_key_path = root.join(".vellum-test-master-key");
    let test_file_key_allowed = cfg!(test)
        || (cfg!(debug_assertions)
            && std::env::var("VELLUM_TEST_FILE_KEY").as_deref() == Ok("integration-only"));
    if test_file_key_allowed {
        if let Ok(value) = std::fs::read(&test_key_path) {
            if value.len() == KEY_BYTES {
                validate_existing_credentials(root, &value)?;
                return Ok(value);
            }
        }
    }
    let key = if legacy_path.exists() {
        let value = std::fs::read(&legacy_path)
            .map_err(|error| AppError::Message(format!("read legacy master key: {error}")))?;
        if value.len() != KEY_BYTES {
            return Err(AppError::Message(
                "legacy master key has an invalid length; existing data was not modified".into(),
            ));
        }
        value
    } else {
        let mut value = vec![0u8; KEY_BYTES];
        getrandom::fill(&mut value)
            .map_err(|error| AppError::Message(format!("cannot generate master key: {error}")))?;
        value
    };

    validate_existing_credentials(root, &key)?;
    if let Err(error) = keychain_put(&account, &hex_key(&key)) {
        // Unit tests execute in a non-interactive SSH/CI session where the
        // user's login keychain is intentionally unavailable.  Fresh test
        // roots contain no user data, so an ephemeral key is safe and keeps
        // the rest of the state/history tests platform-independent.  Never
        // use this escape hatch for a real profile or a migration that has
        // existing encrypted data: production remains fail-closed.
        if test_file_key_allowed && !legacy_path.exists() && !has_existing_credentials(root)? {
            let _ = std::fs::write(&test_key_path, &key);
            return Ok(key);
        }
        return Err(error);
    }
    let stored = keychain_get(&account)?.ok_or_else(|| {
        AppError::Message(
            "macOS Keychain write could not be verified; existing data was not modified".into(),
        )
    })?;
    if parse_hex_key(&stored)? != key {
        return Err(AppError::Message(
            "macOS Keychain verification failed; existing data was not modified".into(),
        ));
    }
    if legacy_path.exists() {
        std::fs::remove_file(&legacy_path).map_err(|error| {
            AppError::Message(format!(
                "Keychain migration succeeded but legacy key cleanup failed: {error}"
            ))
        })?;
    }
    Ok(key)
}

#[cfg(target_os = "macos")]
fn keychain_account(root: &Path) -> String {
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    format!(
        "vellum-{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

#[cfg(target_os = "macos")]
fn keychain_get(account: &str) -> AppResult<Option<String>> {
    let output = Command::new("security")
        .args([
            "find-generic-password",
            "-a",
            account,
            "-s",
            KEYCHAIN_SERVICE,
            "-w",
            &keychain_path(),
        ])
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| AppError::Message(format!("run macOS Keychain lookup: {error}")))?;
    if output.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("could not be found") || stderr.contains("not found") {
        return Ok(None);
    }
    Err(AppError::Message(format!(
        "macOS Keychain lookup failed: {}",
        stderr.trim()
    )))
}

#[cfg(target_os = "macos")]
fn keychain_put(account: &str, value: &str) -> AppResult<()> {
    let output = Command::new("security")
        .args([
            "add-generic-password",
            "-U",
            "-a",
            account,
            "-s",
            KEYCHAIN_SERVICE,
            "-w",
            value,
            &keychain_path(),
        ])
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| AppError::Message(format!("run macOS Keychain write: {error}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(AppError::Message(format!(
            "macOS Keychain write failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

#[cfg(target_os = "macos")]
fn keychain_path() -> String {
    std::env::var_os("HOME")
        .map(|home| {
            Path::new(&home)
                .join("Library/Keychains/login.keychain-db")
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| "login.keychain-db".to_string())
}

#[cfg(target_os = "macos")]
fn parse_hex_key(raw: &str) -> AppResult<Vec<u8>> {
    parse_explicit_key(raw)
}

#[cfg(target_os = "macos")]
fn hex_key(key: &[u8]) -> String {
    key.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(target_os = "macos")]
fn validate_existing_credentials(root: &Path, key: &[u8]) -> AppResult<()> {
    let directory = root.join("credentials");
    if !directory.exists() {
        return Ok(());
    }
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
            .unwrap_or_default();
        let cipher = JournalCipher::from_key(key.to_vec(), "migration-validation")?;
        let encrypted = std::fs::read(&path)
            .map_err(|error| AppError::Message(format!("read existing credential: {error}")))?;
        crate::credentials::open_stored_file(&cipher, &encrypted, file_stem).map_err(|error| {
            AppError::Message(format!(
                "existing credential validation failed; existing data was not modified: {error}"
            ))
        })?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn has_existing_credentials(root: &Path) -> AppResult<bool> {
    let directory = root.join("credentials");
    if !directory.exists() {
        return Ok(false);
    }
    Ok(std::fs::read_dir(&directory)
        .map_err(|error| AppError::Message(format!("read credentials directory: {error}")))?
        .filter_map(Result::ok)
        .any(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("bin")))
}

#[cfg(windows)]
fn load_or_create_dpapi_key(path: &Path) -> AppResult<Vec<u8>> {
    if let Ok(mut file) = std::fs::File::open(path) {
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .map_err(|e| AppError::Message(format!("read dpapi key: {e}")))?;
        let plain = dpapi_unprotect(&buf)?;
        if plain.len() == KEY_BYTES {
            return Ok(plain);
        }
    }
    let mut key = vec![0u8; KEY_BYTES];
    getrandom::fill(&mut key)
        .map_err(|e| AppError::Message(format!("cannot generate key: {e}")))?;
    let sealed = dpapi_protect(&key)?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, &sealed)
        .map_err(|e| AppError::Message(format!("write dpapi key: {e}")))?;
    Ok(key)
}

#[cfg(windows)]
fn dpapi_protect(plain: &[u8]) -> AppResult<Vec<u8>> {
    use std::ptr;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let input_blob = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe {
        CryptProtectData(
            &input_blob,
            ptr::null(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(AppError::Message(format!(
            "DPAPI CryptProtectData failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        let _ = LocalFree(output.pbData.cast());
    }
    Ok(bytes)
}

#[cfg(windows)]
fn dpapi_unprotect(sealed: &[u8]) -> AppResult<Vec<u8>> {
    use std::ptr;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let input_blob = CRYPT_INTEGER_BLOB {
        cbData: sealed.len() as u32,
        pbData: sealed.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe {
        CryptUnprotectData(
            &input_blob,
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(AppError::Message(format!(
            "DPAPI CryptUnprotectData failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        let _ = LocalFree(output.pbData.cast());
    }
    Ok(bytes)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let c = JournalCipher::from_key(vec![7u8; KEY_BYTES], "test").unwrap();
        let msg = b"hello world";
        let blob = c.seal(msg, b"aad").unwrap();
        assert_ne!(&blob[..], msg);
        let opened = c.open(&blob, b"aad").unwrap();
        assert_eq!(opened, msg);
    }

    #[test]
    fn wrong_aad_fails() {
        let c = JournalCipher::from_key(vec![3u8; KEY_BYTES], "test").unwrap();
        let blob = c.seal(b"secret", b"right").unwrap();
        assert!(c.open(&blob, b"wrong").is_err());
    }

    #[test]
    fn truncated_blob_rejected() {
        let c = JournalCipher::from_key(vec![1u8; KEY_BYTES], "test").unwrap();
        assert!(c.open(&[0u8; 5], b"").is_err());
    }

    #[test]
    fn nonce_is_random_per_seal() {
        let c = JournalCipher::from_key(vec![9u8; KEY_BYTES], "test").unwrap();
        let a = c.seal(b"same", b"").unwrap();
        let b = c.seal(b"same", b"").unwrap();
        assert_ne!(a, b, "nonce must differ between calls");
    }

    #[test]
    fn wrong_key_length_rejected() {
        assert!(JournalCipher::from_key(vec![0u8; 10], "test").is_err());
    }

    #[test]
    fn fingerprint_is_stable() {
        let c = JournalCipher::from_key(vec![5u8; KEY_BYTES], "test").unwrap();
        assert_eq!(c.fingerprint(b"x").unwrap(), c.fingerprint(b"x").unwrap());
    }

    #[test]
    fn tampered_ciphertext_fails_closed() {
        let c = JournalCipher::from_key(vec![9u8; KEY_BYTES], "test").unwrap();
        let mut blob = c.seal(b"retained", b"aad").unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0x40;
        let err = c.open(&blob, b"aad").unwrap_err().to_string();
        assert!(err.contains("authentication failed"));
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_round_trip_and_corruption_detection() {
        let plaintext = b"vellum-dpapi-test";
        let protected = dpapi_protect(plaintext).expect("protect with current Windows user");
        assert_ne!(protected, plaintext);
        assert_eq!(
            dpapi_unprotect(&protected).expect("unprotect with current Windows user"),
            plaintext
        );
        let mut corrupt = protected;
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0x5a;
        assert!(dpapi_unprotect(&corrupt).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_key_file_survives_reload_without_plaintext() {
        let root = tempfile::TempDir::new().unwrap();
        let path = root.path().join("vellum-key.dpapi");
        let first = load_or_create_dpapi_key(&path).unwrap();
        let disk = std::fs::read(&path).unwrap();
        assert!(!disk.windows(first.len()).any(|window| window == first));
        let second = load_or_create_dpapi_key(&path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn env_key_parses_hex() {
        assert_eq!(
            parse_explicit_key(&"ab".repeat(KEY_BYTES)).unwrap().len(),
            KEY_BYTES
        );
        assert!(parse_explicit_key("tooshort").is_err());
    }

    fn with_test_env_key<T>(value: Option<String>, run: impl FnOnce() -> T) -> T {
        TEST_ENV_KEY.with(|slot| {
            let previous = slot.replace(Some(value));
            let result = run();
            slot.replace(previous);
            result
        })
    }

    /// Debug/test builds must still honor `VELLUM_MASTER_KEY` — existing
    /// test infra (and local dev reproducibility) depends on it.
    #[test]
    fn dev_mode_still_honors_env_key() {
        let root = tempfile::TempDir::new().unwrap();
        let result = with_test_env_key(Some("44".repeat(KEY_BYTES)), || {
            JournalCipher::load_or_create_with_mode(root.path(), true)
        });
        assert_eq!(result.unwrap().protection(), "environment");
    }

    /// Packaged-release mode (`allow_env_key = false`) must ignore
    /// `VELLUM_MASTER_KEY` even when it is set, and fall back to DPAPI.
    #[cfg(windows)]
    #[test]
    fn packaged_mode_ignores_env_key_and_uses_dpapi() {
        let root = tempfile::TempDir::new().unwrap();
        let result = with_test_env_key(Some("33".repeat(KEY_BYTES)), || {
            JournalCipher::load_or_create_with_mode(root.path(), false)
        });
        assert_eq!(result.unwrap().protection(), "dpapi-current-user");
    }

    /// End-to-end: data encrypted under an env key by a dev build gets
    /// migrated to the platform key the first time a packaged-mode load sees
    /// it (with the env var still present for that one-time window), and
    /// remains readable afterward with the env var gone entirely.
    #[cfg(windows)]
    #[test]
    fn migration_moves_data_from_env_key_to_platform_key() {
        let root = tempfile::TempDir::new().unwrap();
        with_test_env_key(Some("11".repeat(KEY_BYTES)), || {
            // Old (buggy-priority) build: env key wins, credential saved under it.
            let env_cipher = JournalCipher::load_or_create_with_mode(root.path(), true).unwrap();
            assert_eq!(env_cipher.protection(), "environment");
            crate::credentials::save(root.path(), "route-1", "top-secret").unwrap();

            // Packaged build sees the same env var still set (the one-time
            // migration window) and must move the data to the platform key.
            let migrated = JournalCipher::load_or_create_with_mode(root.path(), false).unwrap();
            assert_eq!(migrated.protection(), "dpapi-current-user");
        });
        assert_eq!(
            read_protection_marker(root.path()).as_deref(),
            Some("dpapi-current-user")
        );

        // Env var goes away entirely — packaged mode must still read the
        // migrated credential using only the platform key.
        with_test_env_key(None, || {
            assert_eq!(
                crate::credentials::load(root.path(), "route-1")
                    .unwrap()
                    .as_deref(),
                Some("top-secret")
            );
        });
    }

    /// If the marker says migration is needed but the env var is gone
    /// (analogous to the platform key store being unavailable partway
    /// through: either way, one of the two keys required to complete the
    /// migration can't be obtained), fail closed with a clear error and
    /// leave the marker exactly as it was — never silently treat the data as
    /// migrated.
    #[cfg(windows)]
    #[test]
    fn migration_fails_closed_without_corrupting_data_when_old_key_is_unavailable() {
        let root = tempfile::TempDir::new().unwrap();
        with_test_env_key(Some("22".repeat(KEY_BYTES)), || {
            let env_cipher = JournalCipher::load_or_create_with_mode(root.path(), true).unwrap();
            assert_eq!(env_cipher.protection(), "environment");
            crate::credentials::save(root.path(), "route-1", "still-secret").unwrap();
        });

        // Simulate the env var having become unavailable before a packaged
        // build ever got a chance to migrate.
        let result = with_test_env_key(None, || {
            JournalCipher::load_or_create_with_mode(root.path(), false)
        });
        assert!(result.is_err());
        assert_eq!(
            read_protection_marker(root.path()).as_deref(),
            Some("environment"),
            "marker must not advance past a failed migration"
        );
        // Fails closed rather than silently reading garbage under the wrong key.
        with_test_env_key(None, || {
            assert!(crate::credentials::load(root.path(), "route-1").is_err());
        });
    }
}
