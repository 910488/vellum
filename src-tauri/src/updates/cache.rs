//! Versioned download cache: `.partial` resume, same-volume atomic rename,
//! zip-slip / symlink / capacity guards.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::manifest::verify_asset_sha256;
use super::write_atomic;

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("disk space is insufficient: need {needed} bytes")]
    DiskFull { needed: u64 },
    #[error("archive entry escapes the destination: {0}")]
    ZipSlip(String),
    #[error("archive contains a forbidden link: {0}")]
    IllegalLink(String),
    #[error("archive exceeds declared capacity ({0} bytes)")]
    OverCapacity(u64),
    #[error("{0}")]
    Io(String),
    #[error("asset hash mismatch")]
    BadHash,
}

impl From<std::io::Error> for CacheError {
    fn from(error: std::io::Error) -> Self {
        if error.raw_os_error() == Some(28) || error.kind() == std::io::ErrorKind::StorageFull {
            return CacheError::DiskFull { needed: 0 };
        }
        CacheError::Io(error.to_string())
    }
}

pub trait DiskSpace {
    fn available_bytes(&self, path: &Path) -> u64;
}

pub struct HostDisk;

impl DiskSpace for HostDisk {
    fn available_bytes(&self, path: &Path) -> u64 {
        host_available_bytes(path)
    }
}

fn host_available_bytes(path: &Path) -> u64 {
    // stage_bytes checks capacity before create_dir_all, so `path` often does
    // not exist yet. Query the nearest existing ancestor; a missing path must
    // not look like a full disk (0) or unlimited (u64::MAX).
    let dir = existing_dir(path);
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut free_to_caller = 0u64;
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free_to_caller,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            0
        } else {
            free_to_caller
        }
    }
    #[cfg(not(windows))]
    {
        let c_path = std::ffi::CString::new(dir.to_string_lossy().as_bytes()).ok();
        let Some(c_path) = c_path else {
            return 0;
        };
        let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) };
        if ok != 0 {
            0
        } else {
            (stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64)
        }
    }
}

fn existing_dir(path: &Path) -> PathBuf {
    let mut current = if path.is_file() {
        path.parent().unwrap_or(path).to_path_buf()
    } else {
        path.to_path_buf()
    };
    loop {
        if current.exists() {
            return current;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return std::env::temp_dir(),
        }
    }
}

pub fn component_cache(root: &Path, component: &str, version: &str) -> PathBuf {
    root.join("updates")
        .join("cache")
        .join(component)
        .join(version)
}

/// Write `bytes` into `dest` via `dest.partial`, resuming if the partial
/// already holds a prefix of the same content.
pub fn stage_bytes(
    dest: &Path,
    bytes: &[u8],
    expected_sha256: &str,
    expected_size: u64,
    disk: &dyn DiskSpace,
) -> Result<PathBuf, CacheError> {
    if expected_size > 0 && bytes.len() as u64 != expected_size {
        return Err(CacheError::Io(format!(
            "declared size {expected_size} != {}",
            bytes.len()
        )));
    }
    if disk.available_bytes(dest) < bytes.len() as u64 {
        return Err(CacheError::DiskFull {
            needed: bytes.len() as u64,
        });
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let partial = dest.with_extension("partial");
    let mut already = 0u64;
    if partial.exists() {
        already = fs::metadata(&partial)?.len();
        if already as usize > bytes.len() {
            fs::remove_file(&partial)?;
            already = 0;
        } else if already as usize <= bytes.len()
            && fs::read(&partial)
                .ok()
                .is_some_and(|prefix| prefix == bytes[..already as usize])
        {
            // resume
        } else {
            fs::remove_file(&partial)?;
            already = 0;
        }
    }
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&partial)?;
        file.seek(SeekFrom::Start(already))?;
        file.write_all(&bytes[already as usize..])?;
        file.sync_all()?;
    }
    verify_asset_sha256(bytes, expected_sha256).map_err(|_| CacheError::BadHash)?;
    atomic_replace(&partial, dest)?;
    Ok(dest.to_path_buf())
}

pub fn atomic_replace(from: &Path, to: &Path) -> Result<(), CacheError> {
    if to.exists() {
        let _ = fs::remove_file(to);
    }
    fs::rename(from, to).map_err(CacheError::from)
}

/// Crash-safe tree switch: `current`, `candidate`, `previous`.
/// After any crash the tree is a complete old or a complete new version.
pub fn commit_candidate(slot_root: &Path) -> Result<(), CacheError> {
    let current = slot_root.join("current");
    let candidate = slot_root.join("candidate");
    let previous = slot_root.join("previous");
    let marker = slot_root.join("switch.json");
    if !candidate.join(".complete").exists() {
        return Err(CacheError::Io("candidate is incomplete".into()));
    }
    write_atomic(&marker, br#"{"phase":"renamingCurrent"}"#)?;
    if current.exists() {
        if previous.exists() {
            fs::remove_dir_all(&previous)?;
        }
        fs::rename(&current, &previous)?;
    }
    write_atomic(&marker, br#"{"phase":"renamingCandidate"}"#)?;
    fs::rename(&candidate, &current)?;
    write_atomic(&marker, br#"{"phase":"done"}"#)?;
    Ok(())
}

pub fn recover_switch(slot_root: &Path) -> Result<(), CacheError> {
    let current = slot_root.join("current");
    let candidate = slot_root.join("candidate");
    let marker: serde_json::Value = std::fs::read(slot_root.join("switch.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| serde_json::json!({"phase":"done"}));
    let phase = marker
        .get("phase")
        .and_then(|value| value.as_str())
        .unwrap_or("done");
    match phase {
        "renamingCurrent" | "renamingCandidate"
            if !current.exists() && candidate.join(".complete").exists() =>
        {
            fs::rename(&candidate, &current)?;
        }
        _ => {}
    }
    // Never leave a mix: incomplete candidate is discarded.
    if candidate.exists() && !candidate.join(".complete").exists() {
        fs::remove_dir_all(&candidate)?;
    }
    Ok(())
}

pub fn mark_complete(dir: &Path) -> Result<(), CacheError> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join(".complete"), b"ok")?;
    Ok(())
}

pub fn extract_verified_archive(
    archive: &Path,
    dest: &Path,
    declared_uncompressed: u64,
) -> Result<(), CacheError> {
    fs::create_dir_all(dest)?;
    let canonical_dest = fs::canonicalize(dest)?;
    let lower = archive.to_string_lossy().to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        return extract_tar_gz(archive, dest, &canonical_dest, declared_uncompressed);
    }
    let file = File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file).map_err(|error| CacheError::Io(error.to_string()))?;
    let mut written = 0u64;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|error| CacheError::Io(error.to_string()))?;
        let name = entry.name().to_string();
        if name.contains('\0') {
            return Err(CacheError::ZipSlip(name));
        }
        let raw = PathBuf::from(&name);
        if raw.is_absolute()
            || raw
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(CacheError::ZipSlip(name));
        }
        if entry.is_symlink() {
            return Err(CacheError::IllegalLink(name));
        }
        let out = dest.join(&raw);
        let parent = out.parent().unwrap_or(dest);
        fs::create_dir_all(parent)?;
        let canonical_parent = fs::canonicalize(parent)?;
        if !canonical_parent.starts_with(&canonical_dest) {
            return Err(CacheError::ZipSlip(name));
        }
        if entry.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        written = written.saturating_add(entry.size());
        if declared_uncompressed > 0 && written > declared_uncompressed {
            return Err(CacheError::OverCapacity(declared_uncompressed));
        }
        let mut target = File::create(&out)?;
        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .map_err(|error| CacheError::Io(error.to_string()))?;
        target.write_all(&buf)?;
    }
    Ok(())
}

fn extract_tar_gz(
    archive: &Path,
    dest: &Path,
    canonical_dest: &Path,
    declared_uncompressed: u64,
) -> Result<(), CacheError> {
    let file = File::open(archive)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    let mut written = 0u64;
    let entries = tar
        .entries()
        .map_err(|error| CacheError::Io(error.to_string()))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| CacheError::Io(error.to_string()))?;
        let raw = entry
            .path()
            .map_err(|error| CacheError::Io(error.to_string()))?
            .into_owned();
        let name = raw.to_string_lossy().to_string();
        if raw.is_absolute()
            || raw.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            return Err(CacheError::ZipSlip(name));
        }
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            return Err(CacheError::IllegalLink(name));
        }
        let out = dest.join(&raw);
        if kind.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        if !kind.is_file() {
            return Err(CacheError::IllegalLink(name));
        }
        let parent = out.parent().unwrap_or(dest);
        fs::create_dir_all(parent)?;
        let canonical_parent = fs::canonicalize(parent)?;
        if !canonical_parent.starts_with(canonical_dest) {
            return Err(CacheError::ZipSlip(name));
        }
        let size = entry.size();
        written = written.saturating_add(size);
        if declared_uncompressed > 0 && written > declared_uncompressed {
            return Err(CacheError::OverCapacity(declared_uncompressed));
        }
        let mut target = File::create(&out)?;
        std::io::copy(&mut entry, &mut target)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    struct FullDisk;
    impl DiskSpace for FullDisk {
        fn available_bytes(&self, _path: &Path) -> u64 {
            0
        }
    }

    struct Plenty;
    impl DiskSpace for Plenty {
        fn available_bytes(&self, _path: &Path) -> u64 {
            u64::MAX
        }
    }

    #[test]
    fn host_disk_reports_real_free_space_not_unlimited() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("cache").join("not-created-yet.bin");
        let available = HostDisk.available_bytes(&missing);
        assert_ne!(
            available,
            u64::MAX,
            "HostDisk must query the OS; u64::MAX would never refuse a full disk"
        );
        assert!(
            available > 0,
            "missing dest paths must still resolve to an existing volume, got {available}"
        );
        let bytes = b"hello";
        let hash = hex::encode(Sha256::digest(bytes));
        stage_bytes(&missing, bytes, &hash, bytes.len() as u64, &HostDisk).unwrap();
        assert!(missing.exists());
    }

    #[test]
    fn disk_full_is_refused_before_write() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("asset.bin");
        let bytes = b"hello";
        let hash = hex::encode(Sha256::digest(bytes));
        let err = stage_bytes(&dest, bytes, &hash, bytes.len() as u64, &FullDisk).unwrap_err();
        assert!(matches!(err, CacheError::DiskFull { .. }));
        assert!(!dest.exists());
    }

    #[test]
    fn partial_resume_appends() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("asset.bin");
        let bytes = b"hello world";
        let hash = hex::encode(Sha256::digest(bytes));
        let partial = dest.with_extension("partial");
        fs::write(&partial, &bytes[..5]).unwrap();
        stage_bytes(&dest, bytes, &hash, bytes.len() as u64, &Plenty).unwrap();
        super::super::manifest::verify_asset_file(&dest, &hash).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), bytes);
        assert!(!partial.exists());
    }

    #[test]
    fn zip_slip_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("bad.zip");
        {
            let file = File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("../escape.txt", options).unwrap();
            std::io::Write::write_all(&mut zip, b"nope").unwrap();
            zip.finish().unwrap();
        }
        let dest = dir.path().join("out");
        let err = extract_verified_archive(&archive, &dest, 1024).unwrap_err();
        assert!(matches!(err, CacheError::ZipSlip(_)));
    }

    #[test]
    fn crash_mid_switch_recovers_complete_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("current")).unwrap();
        fs::write(root.join("current").join("app"), b"old").unwrap();
        mark_complete(&root.join("current")).unwrap();
        fs::create_dir_all(root.join("candidate")).unwrap();
        fs::write(root.join("candidate").join("app"), b"new").unwrap();
        mark_complete(&root.join("candidate")).unwrap();
        // Simulate crash after current → previous.
        fs::rename(root.join("current"), root.join("previous")).unwrap();
        write_atomic(
            &root.join("switch.json"),
            br#"{"phase":"renamingCandidate"}"#,
        )
        .unwrap();
        recover_switch(root).unwrap();
        assert_eq!(fs::read(root.join("current").join("app")).unwrap(), b"new");
        // A second, clean switch must still leave only complete trees.
        fs::create_dir_all(root.join("candidate")).unwrap();
        fs::write(root.join("candidate").join("app"), b"newer").unwrap();
        mark_complete(&root.join("candidate")).unwrap();
        commit_candidate(root).unwrap();
        assert_eq!(
            fs::read(root.join("current").join("app")).unwrap(),
            b"newer"
        );
        assert_eq!(fs::read(root.join("previous").join("app")).unwrap(), b"new");
    }

    #[test]
    fn incomplete_candidate_is_discarded_on_recover() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("current")).unwrap();
        fs::write(root.join("current").join("app"), b"old").unwrap();
        mark_complete(&root.join("current")).unwrap();
        fs::create_dir_all(root.join("candidate")).unwrap();
        fs::write(root.join("candidate").join("app"), b"partial").unwrap();
        recover_switch(root).unwrap();
        assert_eq!(fs::read(root.join("current").join("app")).unwrap(), b"old");
        assert!(!root.join("candidate").exists());
    }
}
