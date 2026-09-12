//! Stages a signed Enhanced Core archive as an immutable runnable tree.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use super::cache::{commit_candidate, extract_verified_archive, mark_complete};
use super::core_slots::{CoreSlot, CoreSlotFile};
use super::manifest::{verify_asset_file, CoreManifest, CoreTarget};

pub fn stage_verified_core(
    root: &Path,
    archive: &Path,
    version: &str,
    manifest: &CoreManifest,
) -> Result<CoreSlot, String> {
    let target = manifest
        .target(
            super::manifest::current_platform(),
            super::manifest::current_arch(),
        )
        .ok_or_else(|| "signed core manifest has no target for this machine".to_string())?;
    if archive.file_name().and_then(|name| name.to_str()) != Some(target.asset.as_str()) {
        return Err("downloaded core archive does not match its signed target".into());
    }
    let slot_root = root.join("updates").join("core").join(version);
    let candidate = slot_root.join("candidate");
    if candidate.exists() {
        fs::remove_dir_all(&candidate).map_err(|error| error.to_string())?;
    }
    fs::create_dir_all(&candidate).map_err(|error| error.to_string())?;

    let staged = (|| {
        extract_verified_archive(archive, &candidate, target.uncompressed_size)
            .map_err(|error| error.to_string())?;
        verify_runnable_tree(&candidate, target)?;
        mark_executables(&candidate, target)?;
        mark_complete(&candidate).map_err(|error| error.to_string())?;
        commit_candidate(&slot_root).map_err(|error| error.to_string())?;
        build_slot(&slot_root.join("current"), version, manifest, target)
    })();

    if staged.is_err() && candidate.exists() {
        let _ = fs::remove_dir_all(&candidate);
    }
    staged
}

fn verify_runnable_tree(root: &Path, target: &CoreTarget) -> Result<(), String> {
    let expected = target
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<HashMap<_, _>>();
    let mut observed = HashSet::new();
    collect_files(root, root, &mut |relative, path| {
        let Some(file) = expected.get(relative.as_str()) else {
            return Err(format!("core archive contains unsigned file {relative}"));
        };
        let size = fs::metadata(path).map_err(|error| error.to_string())?.len();
        if size != file.size {
            return Err(format!(
                "core file {relative} size {size} does not match signed size {}",
                file.size
            ));
        }
        verify_asset_file(path, &file.sha256).map_err(|error| error.to_string())?;
        observed.insert(relative);
        Ok(())
    })?;
    for path in expected.keys() {
        if !observed.contains(*path) {
            return Err(format!("core archive is missing signed file {path}"));
        }
    }
    Ok(())
}

fn collect_files(
    root: &Path,
    dir: &Path,
    visit: &mut impl FnMut(String, &Path) -> Result<(), String>,
) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "core tree contains forbidden link {}",
                path.display()
            ));
        }
        if metadata.is_dir() {
            collect_files(root, &path, visit)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            visit(relative, &path)?;
        } else {
            return Err(format!(
                "core tree contains unsupported entry {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn mark_executables(root: &Path, target: &CoreTarget) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for file in target.files.iter().filter(|file| file.executable) {
            let path = root.join(&file.path);
            let mut permissions = fs::metadata(&path)
                .map_err(|error| error.to_string())?
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).map_err(|error| error.to_string())?;
        }
    }
    #[cfg(not(unix))]
    let _ = (root, target);
    Ok(())
}

fn build_slot(
    current: &Path,
    version: &str,
    manifest: &CoreManifest,
    target: &CoreTarget,
) -> Result<CoreSlot, String> {
    let files = target
        .files
        .iter()
        .map(|file| CoreSlotFile {
            path: current.join(&file.path),
            size: file.size,
            sha256: file.sha256.clone(),
        })
        .collect::<Vec<_>>();
    let executable = current.join(&target.executable);
    let digest = target
        .files
        .iter()
        .find(|file| file.path == target.executable)
        .map(|file| file.sha256.clone())
        .ok_or_else(|| "signed executable is absent from the core file list".to_string())?;
    let protocol = if target.protocol_schema_sha256.is_empty() {
        manifest.protocol_schema_sha256.clone()
    } else {
        target.protocol_schema_sha256.clone()
    };
    Ok(CoreSlot {
        version: version.to_string(),
        digest,
        protocol,
        protocol_compat: manifest.protocol_compat.0.clone(),
        path: executable,
        helpers: target
            .helpers
            .iter()
            .map(|path| current.join(path))
            .collect(),
        files,
        enhanced_commit: manifest.enhanced_commit.clone(),
        feature_profile: manifest.feature_profile.clone(),
        launch_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updates::manifest::{CompatRange, CoreFile};
    use sha2::{Digest, Sha256};
    use std::io::Write;

    fn digest(bytes: &[u8]) -> String {
        format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
    }

    fn manifest(extra: bool) -> CoreManifest {
        let mut files = vec![
            CoreFile {
                path: "codex.exe".into(),
                size: 4,
                sha256: digest(b"core"),
                executable: true,
            },
            CoreFile {
                path: "codex-command-runner.exe".into(),
                size: 6,
                sha256: digest(b"helper"),
                executable: true,
            },
        ];
        if extra {
            files.push(CoreFile {
                path: "unsigned.txt".into(),
                size: 1,
                sha256: digest(b"x"),
                executable: false,
            });
        }
        CoreManifest {
            upstream_commit: "a".repeat(40),
            enhanced_commit: "b".repeat(40),
            feature_profile: "test".into(),
            protocol_schema_sha256: digest(b"schema"),
            protocol_compat: CompatRange::new(">=1 <2"),
            targets: vec![CoreTarget {
                platform: super::super::manifest::current_platform().into(),
                arch: super::super::manifest::current_arch().into(),
                asset: "core.zip".into(),
                executable: "codex.exe".into(),
                helpers: vec!["codex-command-runner.exe".into()],
                uncompressed_size: 11,
                protocol_schema_sha256: digest(b"probed-schema"),
                files,
            }],
        }
    }

    fn write_zip(path: &Path, extra: bool) {
        let file = fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        for (name, bytes) in [
            ("codex.exe", b"core".as_slice()),
            ("codex-command-runner.exe", b"helper".as_slice()),
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        if extra {
            zip.start_file("unsigned.txt", options).unwrap();
            zip.write_all(b"x").unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn stages_only_an_exact_signed_tree() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("core.zip");
        write_zip(&archive, false);
        let slot = stage_verified_core(dir.path(), &archive, "1.0.0", &manifest(false)).unwrap();
        assert_eq!(fs::read(slot.path).unwrap(), b"core");
        assert_eq!(slot.helpers.len(), 1);
        assert_eq!(slot.files.len(), 2);
    }

    #[test]
    fn rejects_unsigned_archive_entries() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("core.zip");
        write_zip(&archive, true);
        let error =
            stage_verified_core(dir.path(), &archive, "1.0.0", &manifest(false)).unwrap_err();
        assert!(error.contains("unsigned file"));
    }

    #[test]
    fn a_staged_tree_is_rehashed_before_it_is_trusted_again() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("core.zip");
        write_zip(&archive, false);
        let slot = stage_verified_core(dir.path(), &archive, "1.0.0", &manifest(false)).unwrap();
        let digest = slot.digest.clone();
        super::super::core_slots::set_pending(dir.path(), slot.clone()).unwrap();
        assert!(
            !super::super::core_slots::signed_core_digest(dir.path(), &digest),
            "a staged tree without the original signed blob is not a signed identity"
        );
        fs::write(&slot.helpers[0], b"tampered").unwrap();
        assert!(!super::super::core_slots::signed_core_digest(
            dir.path(),
            &digest
        ));
    }
}
