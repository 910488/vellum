//! Verified Codex runtime packages. Never flatten the official package layout:
//! InstallContext locates helpers and sandbox resources relative to bin/codex.
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

const MAX_FILES: usize = 4096;
const MAX_UNPACKED_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimePackageStatus {
    pub complete: bool,
    pub missing: Vec<String>,
}

fn private_mode(path: &Path, executable: bool) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if executable { 0o700 } else { 0o600 }),
        )
        .map_err(|error| error.to_string())?;
    }
    let _ = (path, executable);
    Ok(())
}

fn regular_file(path: &Path, executable: bool) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        if !metadata.is_file() || metadata.len() == 0 {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if executable && metadata.permissions().mode() & 0o100 == 0 {
                return false;
            }
        }
        let _ = executable;
        true
    })
}

/// Also used by handoff preflight: a legacy single binary is present but incomplete.
pub fn inspect(root: &Path) -> RuntimePackageStatus {
    let mut missing = Vec::new();
    for name in ["bin", "codex-resources", "codex-path"] {
        if !fs::symlink_metadata(root.join(name)).is_ok_and(|m| m.is_dir()) {
            missing.push(name.into());
        }
    }
    let metadata = fs::read(root.join("codex-package.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let valid_metadata = metadata.as_ref().is_some_and(|m| {
        m["layoutVersion"] == 1
            && m["entrypoint"] == "bin/codex"
            && m["resourcesDir"] == "codex-resources"
            && m["pathDir"] == "codex-path"
            && m["version"].as_str().is_some_and(|v| !v.is_empty())
            && matches!(
                m["target"].as_str(),
                Some(
                    "x86_64-unknown-linux-musl"
                        | "aarch64-unknown-linux-musl"
                        | "x86_64-apple-darwin"
                        | "aarch64-apple-darwin"
                )
            )
    });
    if !valid_metadata || !regular_file(&root.join("codex-package.json"), false) {
        missing.push("codex-package.json (valid layout/version/target)".into());
    }
    let mut required = vec!["bin/codex", "bin/codex-code-mode-host", "codex-path/rg"];
    if metadata
        .as_ref()
        .and_then(|m| m["target"].as_str())
        .is_some_and(|t| t.contains("linux"))
    {
        required.push("codex-resources/bwrap");
    }
    for name in required {
        if !regular_file(&root.join(name), true) {
            missing.push(name.into());
        }
    }
    RuntimePackageStatus {
        complete: missing.is_empty(),
        missing,
    }
}

pub fn require_complete(root: &Path) -> Result<(), String> {
    let status = inspect(root);
    if status.complete {
        Ok(())
    } else {
        Err(format!("CodexRuntimeIncomplete: redeploy the complete Codex runtime package; missing or invalid: {}", status.missing.join(", ")))
    }
}

pub fn inspect_installed(root: &Path) -> RuntimePackageStatus {
    let mut status = inspect(root);
    let entrypoint = root.join("codex");
    let correct_entrypoint = entrypoint.is_file();
    #[cfg(unix)]
    let correct_entrypoint = correct_entrypoint
        && fs::canonicalize(&entrypoint)
            .ok()
            .zip(fs::canonicalize(root.join("bin/codex")).ok())
            .is_some_and(|(actual, expected)| actual == expected);
    if !correct_entrypoint {
        status.complete = false;
        status
            .missing
            .push("codex -> bin/codex (daemon entrypoint)".into());
    }
    status
}

/// Extract only regular files/directories into a new private staging directory.
/// Links, traversal, duplicate paths, special files and oversized archives fail closed.
pub fn unpack(archive: &Path, destination: &Path) -> Result<u64, String> {
    if let Ok(metadata) = fs::symlink_metadata(destination) {
        if !metadata.is_dir()
            || fs::read_dir(destination)
                .map_err(|e| e.to_string())?
                .next()
                .is_some()
        {
            return Err("CodexPackageStagingNotEmpty: use a new private staging directory".into());
        }
    } else {
        fs::create_dir_all(destination).map_err(|e| e.to_string())?;
    }
    private_mode(destination, true)?;
    let input = fs::File::open(archive).map_err(|e| format!("CodexPackageArchiveInvalid: {e}"))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(input));
    let mut paths = BTreeSet::new();
    let mut total = 0_u64;
    let entries = tar
        .entries()
        .map_err(|e| format!("CodexPackageArchiveInvalid: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("CodexPackageArchiveInvalid: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("CodexPackageArchiveInvalid: {e}"))?
            .into_owned();
        let safe = path
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
            && !path.to_string_lossy().contains(['\\', ':']);
        let normalized: std::path::PathBuf = path
            .components()
            .filter_map(|part| match part {
                Component::Normal(name) => Some(name),
                _ => None,
            })
            .collect();
        if safe
            && normalized.as_os_str().is_empty()
            && entry.header().entry_type().is_dir()
            && entry.size() == 0
        {
            continue;
        }
        if !safe
            || normalized.as_os_str().is_empty()
            || normalized == Path::new("codex")
            || !paths.insert(normalized.clone())
            || paths.len() > MAX_FILES
        {
            return Err(format!("CodexPackageArchiveUnsafe: {}", path.display()));
        }
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err(format!(
                "CodexPackageArchiveUnsafe: links/special files are not allowed: {}",
                path.display()
            ));
        }
        let output = destination.join(normalized);
        if kind.is_dir() {
            if entry.size() != 0 {
                return Err("CodexPackageArchiveUnsafe: directory has a data payload".into());
            }
            fs::create_dir_all(&output).map_err(|e| e.to_string())?;
            private_mode(&output, true)?;
        } else {
            let size = entry.size();
            total = total
                .checked_add(size)
                .filter(|n| *n <= MAX_UNPACKED_BYTES)
                .ok_or_else(|| "CodexPackageArchiveTooLarge".to_string())?;
            fs::create_dir_all(output.parent().ok_or("CodexPackageArchiveUnsafe")?)
                .map_err(|e| e.to_string())?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&output)
                .map_err(|e| e.to_string())?;
            let written = std::io::copy(&mut entry.by_ref().take(size + 1), &mut file)
                .map_err(|e| e.to_string())?;
            if written != size {
                return Err("CodexPackageArchiveTruncated".into());
            }
            private_mode(&output, entry.header().mode().unwrap_or(0) & 0o111 != 0)?;
        }
    }
    require_complete(destination)?;
    Ok(total)
}

/// Desktop validates the package before staging it over SSH; never executes it.
pub fn verify_archive(archive: &Path) -> Result<u64, String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    unpack(archive, directory.path())
}

pub(crate) fn link_daemon_entrypoint(root: &Path) -> Result<(), String> {
    #[cfg(unix)]
    std::os::unix::fs::symlink("bin/codex", root.join("codex")).map_err(|e| e.to_string())?;
    // Windows is only a unit-test host for these Unix runtime packages. A hard
    // link exercises the atomic directory swap without requiring symlink rights.
    #[cfg(not(unix))]
    fs::hard_link(root.join("bin/codex"), root.join("codex")).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn test_archive(path: &Path, payload: &str, omit: Option<&str>) {
    let file = fs::File::create(path).unwrap();
    let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut archive = tar::Builder::new(gzip);
    for dir in ["bin", "codex-resources", "codex-path"] {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, dir, std::io::empty())
            .unwrap();
    }
    let metadata = r#"{"layoutVersion":1,"version":"0.147.0-alpha.6.6","target":"x86_64-unknown-linux-musl","entrypoint":"bin/codex","resourcesDir":"codex-resources","pathDir":"codex-path"}"#;
    for (name, body) in [
        ("bin/codex", payload),
        ("bin/codex-code-mode-host", "helper"),
        ("codex-path/rg", "rg"),
        ("codex-resources/bwrap", "bwrap"),
        ("codex-resources/zsh/bin/zsh", "zsh"),
        ("codex-package.json", metadata),
    ] {
        if omit == Some(name) {
            continue;
        }
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, name, body.as_bytes())
            .unwrap();
    }
    archive.into_inner().unwrap().finish().unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn complete_package_keeps_helpers_resources_and_daemon_path() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("package.tgz");
        test_archive(&archive, "codex", None);
        let root = temp.path().join("unpack");
        unpack(&archive, &root).unwrap();
        link_daemon_entrypoint(&root).unwrap();
        assert!(inspect_installed(&root).complete);
        assert_eq!(fs::read(root.join("codex")).unwrap(), b"codex");
        assert_eq!(
            fs::read(root.join("codex-resources/zsh/bin/zsh")).unwrap(),
            b"zsh"
        );
        #[cfg(unix)]
        {
            assert_eq!(
                fs::read_link(root.join("codex")).unwrap(),
                Path::new("bin/codex")
            );
            assert_eq!(
                fs::canonicalize(root.join("codex")).unwrap(),
                fs::canonicalize(root.join("bin/codex")).unwrap()
            );
        }
    }
    #[test]
    fn missing_helper_and_legacy_binary_are_incomplete() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("codex"), "legacy").unwrap();
        assert!(!inspect(temp.path()).complete);
        let archive = temp.path().join("package.tgz");
        test_archive(&archive, "codex", Some("bin/codex-code-mode-host"));
        let error = verify_archive(&archive).unwrap_err();
        assert!(error.contains("CodexRuntimeIncomplete"));
        assert!(error.contains("bin/codex-code-mode-host"));
    }
    #[test]
    fn archive_links_cannot_escape_the_staging_directory() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("unsafe.tgz");
        let gzip = flate2::write::GzEncoder::new(
            fs::File::create(&path).unwrap(),
            flate2::Compression::default(),
        );
        let mut archive = tar::Builder::new(gzip);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_link_name("../../outside").unwrap();
        header.set_cksum();
        archive
            .append_data(&mut header, "bin", std::io::empty())
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();
        assert!(verify_archive(&path)
            .unwrap_err()
            .contains("CodexPackageArchiveUnsafe"));
        assert!(!temp.path().join("outside").exists());
    }
}
