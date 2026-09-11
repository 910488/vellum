//! Exclusive host mutation lock for cross-process agent safety.
//!
//! Each Desktop RPC may spawn a separate agent process. All destructive
//! lifecycle mutations share one OS file lock so concurrent SSH retries and
//! overlapping start/stop/update calls are serialized.
//!
//! Long docker pulls still hold this lock; ProcessDockerClient bounds
//! pull waits (see DOCKER_PULL_TIMEOUT) so Desktop RPCs fail closed
//! instead of hanging forever. A non-blocking try-lock path can be
//! added later if soft-busy responses are preferred over queueing.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs2::FileExt;

pub struct HostMutationGuard {
    file: std::fs::File,
    path: PathBuf,
}

impl HostMutationGuard {
    pub fn acquire(lock_path: impl AsRef<Path>) -> Result<Self, String> {
        Self::acquire_timeout(lock_path, Duration::from_secs(30))
    }

    pub fn acquire_timeout(lock_path: impl AsRef<Path>, timeout: Duration) -> Result<Self, String> {
        let path = lock_path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!("failed creating lock parent {}: {error}", parent.display())
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| format!("failed opening mutation lock {}: {error}", path.display()))?;
        let started = Instant::now();
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => break,
                Err(error) if lock_is_busy(&error) => {
                    if started.elapsed() >= timeout {
                        return Err(format!(
                            "mutation lock busy after {} ms: {}",
                            timeout.as_millis(),
                            path.display()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => {
                    return Err(format!(
                        "failed acquiring mutation lock {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        Ok(Self { file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn lock_is_busy(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        // Windows reports LockFileEx contention as ERROR_LOCK_VIOLATION and
        // maps it to ErrorKind::Other on supported Rust versions.
        || cfg!(windows) && error.raw_os_error() == Some(33)
}

impl Drop for HostMutationGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_wait_is_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mutation.lock");
        let _held = HostMutationGuard::acquire(&path).unwrap();
        let started = Instant::now();
        let error = HostMutationGuard::acquire_timeout(&path, Duration::from_millis(40))
            .err()
            .expect("second lock must time out");
        assert!(error.contains("mutation lock busy"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
