//! Executable digests the bridge already computed on an earlier launch.
//!
//! Codex Desktop starts the bridge on every launch, and the bridge checks about
//! 1 GB of executables before its first core may start: Desktop's shipped core,
//! the pinned Official core, the Enhanced core and the relay. Desktop's sidebar
//! does not wait for that. On Codex Desktop 26.1103 its project list is read
//! once, before the signed-in account is known, so a slow first `initialize`
//! leaves the sidebar empty until a project is added again.
//!
//! A digest is reused only for a file NTFS reports unchanged since it was
//! hashed: same volume, same file id, same update sequence number (USN), same
//! length and same last write time. NTFS gives a file a new USN on every write,
//! truncation, rename or attribute change, and user-mode code cannot set it, so
//! any change to the bytes means the file is hashed again. A volume without a
//! change journal reports no USN and is always hashed, as is every file on
//! other platforms.
//!
//! The store sits beside the launch manifest. Whoever can write it can already
//! write the manifest that names these executables and their digests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::atomic::write_atomic;

pub const ARTIFACT_DIGESTS_FILE: &str = "artifact-digests.json";
const SCHEMA_VERSION: u32 = 1;
const MAX_ENTRIES: usize = 32;

static STORE: OnceLock<PathBuf> = OnceLock::new();
static WRITE: Mutex<()> = Mutex::new(());

/// Lets [`super::launch_manifest::sha256_file`] reuse and record digests in
/// `path`. Only the bridge calls this; everywhere else every file is hashed.
pub(crate) fn remember_in(path: PathBuf) {
    let _ = STORE.set(path);
}

/// What the file system says about one version of a file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileVersion {
    volume: u64,
    file_id: String,
    usn: i64,
    len: u64,
    modified_nanos: u128,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Store {
    schema_version: u32,
    entries: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    version: FileVersion,
    digest: String,
}

/// The current version of `path`, when the store is in use and the file
/// system can vouch for it.
pub(crate) fn version(path: &Path, len: u64, modified: Option<SystemTime>) -> Option<FileVersion> {
    STORE.get()?;
    version_of(path, len, modified)
}

fn current_version(path: &Path) -> Option<FileVersion> {
    let metadata = std::fs::metadata(path).ok()?;
    version_of(path, metadata.len(), metadata.modified().ok())
}

fn version_of(path: &Path, len: u64, modified: Option<SystemTime>) -> Option<FileVersion> {
    let modified_nanos = modified?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let (volume, file_id, usn) = platform::identity(path)?;
    Some(FileVersion {
        volume,
        file_id,
        usn,
        len,
        modified_nanos,
    })
}

/// The digest recorded for exactly this version of `canonical`.
pub(crate) fn recall(canonical: &Path, version: &FileVersion) -> Option<String> {
    recall_from(STORE.get()?, canonical, version)
}

/// Records `digest` for `version`, provided the file still is that version:
/// a write during the hash would otherwise be remembered as the old bytes.
pub(crate) fn remember(canonical: &Path, path: &Path, version: FileVersion, digest: &str) {
    if let Some(location) = STORE.get() {
        remember_into(location, canonical, path, version, digest);
    }
}

fn recall_from(location: &Path, canonical: &Path, version: &FileVersion) -> Option<String> {
    let store = read(location)?;
    let entry = store.entries.get(&key(canonical))?;
    (entry.version == *version).then(|| entry.digest.clone())
}

fn remember_into(
    location: &Path,
    canonical: &Path,
    path: &Path,
    version: FileVersion,
    digest: &str,
) {
    if current_version(path).as_ref() != Some(&version) {
        return;
    }
    let Ok(_guard) = WRITE.lock() else {
        return;
    };
    let mut store = read(location).unwrap_or_default();
    store.schema_version = SCHEMA_VERSION;
    store.entries.insert(
        key(canonical),
        Entry {
            version,
            digest: digest.to_owned(),
        },
    );
    while store.entries.len() > MAX_ENTRIES {
        let Some(first) = store.entries.keys().next().cloned() else {
            break;
        };
        store.entries.remove(&first);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&store) {
        let _ = write_atomic(location, &bytes);
    }
}

fn key(canonical: &Path) -> String {
    canonical.to_string_lossy().into_owned()
}

fn read(location: &Path) -> Option<Store> {
    let store: Store = serde_json::from_slice(&std::fs::read(location).ok()?).ok()?;
    (store.schema_version == SCHEMA_VERSION).then_some(store)
}

#[cfg(windows)]
mod platform {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };
    use windows_sys::Win32::System::Ioctl::{FSCTL_READ_FILE_USN_DATA, READ_FILE_USN_DATA};
    use windows_sys::Win32::System::IO::DeviceIoControl;

    const FILE_READ_ATTRIBUTES: u32 = 0x0080;
    const SHARE_ALL: u32 = 0x0001 | 0x0002 | 0x0004;

    /// (volume serial, 128-bit file id, USN), or `None` when the volume keeps
    /// no change journal.
    pub(super) fn identity(path: &Path) -> Option<(u64, String, i64)> {
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(SHARE_ALL)
            .open(path)
            .ok()?;
        let handle = file.as_raw_handle();
        // SAFETY: `handle` is open for the duration of both calls and every
        // buffer is sized by what is passed with it.
        unsafe {
            let mut id: FILE_ID_INFO = std::mem::zeroed();
            if GetFileInformationByHandleEx(
                handle,
                FileIdInfo,
                (&mut id as *mut FILE_ID_INFO).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            ) == 0
            {
                return None;
            }
            let request = READ_FILE_USN_DATA {
                MinMajorVersion: 2,
                MaxMajorVersion: 3,
            };
            let mut record = [0_u64; 128];
            let mut returned = 0_u32;
            if DeviceIoControl(
                handle,
                FSCTL_READ_FILE_USN_DATA,
                (&request as *const READ_FILE_USN_DATA).cast(),
                std::mem::size_of::<READ_FILE_USN_DATA>() as u32,
                record.as_mut_ptr().cast(),
                std::mem::size_of_val(&record) as u32,
                &mut returned,
                std::ptr::null_mut(),
            ) == 0
            {
                return None;
            }
            let bytes = std::slice::from_raw_parts(record.as_ptr().cast::<u8>(), returned as usize);
            // USN_RECORD_V2 keeps 64-bit file references, V3 (ReFS) 128-bit ones.
            let offset = match u16::from_le_bytes(bytes.get(4..6)?.try_into().ok()?) {
                2 => 24,
                3 => 40,
                _ => return None,
            };
            let usn = i64::from_le_bytes(bytes.get(offset..offset + 8)?.try_into().ok()?);
            (usn > 0).then(|| {
                (
                    id.VolumeSerialNumber,
                    hex::encode(id.FileId.Identifier),
                    usn,
                )
            })
        }
    }
}

#[cfg(not(windows))]
mod platform {
    pub(super) fn identity(_path: &std::path::Path) -> Option<(u64, String, i64)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_is_reused_only_for_the_version_it_was_computed_from() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join(ARTIFACT_DIGESTS_FILE);
        let path = temp.path().join("core.exe");
        std::fs::write(&path, b"one").unwrap();
        let canonical = std::fs::canonicalize(&path).unwrap();
        let Some(first) = current_version(&path) else {
            // No change journal here (or not Windows): nothing is ever reused.
            assert!(recall_from(&store, &canonical, &FileVersion::default()).is_none());
            return;
        };
        remember_into(&store, &canonical, &path, first.clone(), "sha256:one");
        assert_eq!(
            recall_from(&store, &canonical, &first).as_deref(),
            Some("sha256:one")
        );

        // Same length, and the old timestamp put back: only the USN tells.
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, b"two").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let second = current_version(&path).unwrap();
        assert_eq!(second.len, first.len);
        assert_eq!(second.modified_nanos, first.modified_nanos);
        assert_ne!(second.usn, first.usn);
        assert!(recall_from(&store, &canonical, &second).is_none());
    }

    #[test]
    fn a_version_that_changed_during_the_hash_is_not_recorded() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join(ARTIFACT_DIGESTS_FILE);
        let path = temp.path().join("core.exe");
        std::fs::write(&path, b"one").unwrap();
        let canonical = std::fs::canonicalize(&path).unwrap();
        let Some(before) = current_version(&path) else {
            return;
        };
        std::fs::write(&path, b"two").unwrap();
        remember_into(&store, &canonical, &path, before.clone(), "sha256:one");
        assert!(recall_from(&store, &canonical, &before).is_none());
        assert!(!store.exists());
    }
}
