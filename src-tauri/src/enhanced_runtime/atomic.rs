//! Atomic file replacement for the control files the bridge and Vellum share.
//!
//! A half-written launch manifest or attestation is worse than a missing one:
//! the reader would fail closed on a parse error and blame the runtime rather
//! than the write. Every writer here renames a fully written temp file into
//! place instead.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// A temporary next to `path` that no other writer in any process shares.
///
/// A fixed name (`config.toml.tmp`) or one keyed only by pid lets two threads
/// writing the same file interleave: one renames the other's half-written
/// temporary into place, or finds its own already moved away.
pub fn unique_temporary(path: &Path) -> std::path::PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("vellum"),
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} has no parent directory", path.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let temporary = unique_temporary(path);
    {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    // Windows rename fails when the destination exists; replace explicitly.
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(_) if path.exists() => {
            let result =
                std::fs::remove_file(path).and_then(|()| std::fs::rename(&temporary, path));
            if result.is_err() {
                let _ = std::fs::remove_file(&temporary);
            }
            result
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}

pub fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_an_existing_file_without_leaving_a_temp_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested").join("manifest.json");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let leftovers = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn concurrent_writers_to_one_file_never_publish_a_torn_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let writers: Vec<_> = (0..8u8)
            .map(|n| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let body = vec![b'a' + n; 64 * 1024];
                    (0..20).all(|_| write_atomic(&path, &body).is_ok())
                })
            })
            .collect();
        assert!(writers.into_iter().all(|writer| writer.join().unwrap()));
        let published = std::fs::read(&path).unwrap();
        assert_eq!(published.len(), 64 * 1024);
        assert!(published.iter().all(|byte| *byte == published[0]));
    }
}
