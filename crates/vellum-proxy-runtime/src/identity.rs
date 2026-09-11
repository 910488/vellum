//! On-disk artifact identity for prepare-result reuse.
//!
//! A prepare cache may be reused only when the live file still proves it is
//! the same artifact. Path, version text, or a previously stored hash are
//! not enough on their own: this type re-stats the file and, when the OS
//! identity is incomplete or has changed, re-hashes with a streaming reader.

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const HASH_CHUNK: usize = 64 * 1024;

/// Streaming SHA-256 of a file. Never loads the whole artifact into memory.
pub fn sha256_file_streaming(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Captured identity of one file that a prepare result is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactIdentity {
    pub path: PathBuf,
    pub len: u64,
    pub modified: Option<SystemTime>,
    /// Windows file index or Unix (dev, ino) packed for equality checks.
    pub os_id: Option<u128>,
    pub changed_at: Option<(i64, i64)>,
    pub digest: String,
}

impl ArtifactIdentity {
    /// Capture path metadata and a streaming digest. Fails if the file cannot
    /// be read: an unreadable artifact is not reusable.
    pub fn capture(path: &Path) -> io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", path.display()),
            ));
        }
        let digest = sha256_file_streaming(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            len: meta.len(),
            modified: meta.modified().ok(),
            os_id: os_file_id(&meta),
            changed_at: change_time(&meta),
            digest,
        })
    }

    /// Re-stat the path. Returns true only when the live file is still the
    /// captured artifact.
    ///
    /// Matching length + mtime + OS file id is accepted as proof of identity
    /// (no re-hash). Any missing or changed field forces a streaming re-hash
    /// and digest comparison. A vanished or unreadable file is not a match.
    pub fn still_matches(&self) -> bool {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return false;
        };
        if !meta.is_file() || meta.len() != self.len {
            return false;
        }
        let modified = meta.modified().ok();
        let os_id = os_file_id(&meta);
        let changed_at = change_time(&meta);
        let metadata_complete = modified.is_some() && os_id.is_some() && changed_at.is_some();
        if metadata_complete
            && modified == self.modified
            && os_id == self.os_id
            && changed_at == self.changed_at
        {
            return true;
        }
        sha256_file_streaming(&self.path)
            .ok()
            .is_some_and(|digest| digest == self.digest)
    }
}

fn change_time(meta: &std::fs::Metadata) -> Option<(i64, i64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((meta.ctime(), meta.ctime_nsec()))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

fn os_file_id(meta: &std::fs::Metadata) -> Option<u128> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(((meta.dev() as u128) << 64) | (meta.ino() as u128))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn capture_then_unchanged_file_still_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact.bin");
        std::fs::write(&path, b"same-bytes").unwrap();
        let identity = ArtifactIdentity::capture(&path).unwrap();
        assert!(identity.still_matches());
        assert_eq!(identity.len, 10);
        assert_eq!(identity.digest, sha256_file_streaming(&path).unwrap());
    }

    #[test]
    fn rewritten_bytes_fail_identity_even_at_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact.bin");
        std::fs::write(&path, b"original").unwrap();
        let identity = ArtifactIdentity::capture(&path).unwrap();
        std::fs::write(&path, b"changed!").unwrap();
        assert!(!identity.still_matches());
    }

    #[test]
    fn missing_file_is_not_reusable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gone.bin");
        std::fs::write(&path, b"temp").unwrap();
        let identity = ArtifactIdentity::capture(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(!identity.still_matches());
    }

    #[test]
    fn rewriting_same_length_and_restoring_mtime_does_not_reuse_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifact.bin");
        std::fs::write(&path, b"original").unwrap();
        let identity = ArtifactIdentity::capture(&path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&path, b"changed!").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(identity.modified.unwrap())
            .unwrap();
        assert!(!identity.still_matches());
    }

    #[test]
    fn path_alone_does_not_make_a_different_file_valid() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("a.bin");
        let second = dir.path().join("b.bin");
        std::fs::write(&first, b"alpha").unwrap();
        std::fs::write(&second, b"beta").unwrap();
        let identity = ArtifactIdentity::capture(&first).unwrap();
        // Swap contents at the captured path.
        std::fs::copy(&second, &first).unwrap();
        assert!(!identity.still_matches());
    }

    #[test]
    fn streaming_hash_matches_whole_file_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        let mut file = File::create(&path).unwrap();
        let payload = vec![0x5Au8; 200_000];
        file.write_all(&payload).unwrap();
        drop(file);
        let streamed = sha256_file_streaming(&path).unwrap();
        let whole = format!("{:x}", Sha256::digest(&payload));
        assert_eq!(streamed, whole);
    }
}
