//! Startup verification and best-effort cleanup of retired installer images.
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const SIDECARS: [&str; 3] = [
    "vellum-codex-app-server.exe",
    "vellum-enhanced-codex.exe",
    "vellum-codex-relay.exe",
];

pub fn verify_bridge(path: Option<&Path>, expected: Option<&str>) -> Result<(), String> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let path = path.ok_or_else(|| format!("bundled bridge missing; expected {expected}"))?;
    let bytes =
        std::fs::read(path).map_err(|error| format!("bridge {}: {error}", path.display()))?;
    let actual = format!("sha256:{:x}", Sha256::digest(bytes));
    if actual != expected {
        return Err(format!(
            "bridge {}: expected {expected}, got {actual}",
            path.display()
        ));
    }
    Ok(())
}

pub fn cleanup_retired_images(directory: &Path) {
    // No recursion or wildcard deletion outside known install directories.
    for dir in [directory.to_path_buf(), directory.join("binaries")] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if SIDECARS.iter().any(|base| {
                name.starts_with(&format!("{base}.old-"))
                    && entry.path().with_file_name(base).is_file()
            }) && entry.file_type().is_ok_and(|kind| kind.is_file())
            {
                let _ = std::fs::remove_file(entry.path()); // Running images remain until a later startup.
            }
        }
    }
}

pub fn installed_bridge_integrity() -> Result<(), String> {
    let path: Option<PathBuf> =
        crate::enhanced_runtime::desktop_manager::packaged_bridge_executable();
    verify_bridge(
        path.as_deref(),
        crate::enhanced_runtime::desktop_manager::bundled_bridge_sha256(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_bridge_and_missing_bridge_fail_even_if_desktop_version_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge.exe");
        std::fs::write(&path, b"old").unwrap();
        let expected = format!("sha256:{:x}", Sha256::digest(b"new"));
        assert!(verify_bridge(Some(&path), Some(&expected))
            .unwrap_err()
            .contains("expected"));
        assert!(verify_bridge(None, Some(&expected)).is_err());
        std::fs::write(&path, b"new").unwrap();
        assert!(verify_bridge(Some(&path), Some(&expected)).is_ok());
    }
    #[test]
    fn cleanup_only_removes_known_retired_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("vellum-codex-app-server.exe.old-1-2");
        let keep = dir.path().join("unrelated.exe.old-1-2");
        std::fs::write(&old, b"old").unwrap();
        std::fs::write(&keep, b"keep").unwrap();
        // An unreplaced image may be the user's only remaining core.
        cleanup_retired_images(dir.path());
        assert!(old.exists());
        std::fs::write(dir.path().join("vellum-codex-app-server.exe"), b"new").unwrap();
        cleanup_retired_images(dir.path());
        assert!(!old.exists());
        assert!(keep.exists());
    }
}
