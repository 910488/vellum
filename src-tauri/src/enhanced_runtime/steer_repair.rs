//! Automatic pinned Windows Desktop composer repair for the quota pool.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const ASSET: &str = "app-primary-84ad97f06929.js";
const ORIGINAL_RENDERER: &str = "533eaa44435adf88ae2c7f5e61aac747db1ad1deafd2d02bdaf2606a37e80f7b";
const REPAIRED_RENDERER: &str = "a0453190941b2bd418e281fd327b9e0e2146062bb92dfb99785b882b3b026bf4";
const ORIGINAL_HEADER: &str = "d7da3129304d1f77bb8e4825709942bc7573786b67a21da1e9ee96f3370777a8";
const REPAIRED_HEADER: &str = "1d753bc87addb8a1d8420517393869bbfaafa5faa1f81ddd090bbf56cd4e4468";
const ORIGINAL_EXE: &str = "709a4d9b88cafc7f78e43aab7965b0de9f6279884036e909c47c00a89b48c463";
const REPAIRED_EXE: &str = "f91440bd7826e8ec1400fc4fd10b91a6a2effcf36aa5eef1810c7dc3c11134b9";
const GATE: &str = "fn=q(rP)&&bt===`local`";
const DISABLE: &str = "Gn=je||St||rt&&it||vt||Ot||rn?.isLoading===!0||wt||fn";

fn invalid(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn copy_directory(root: &Path) -> PathBuf {
    root.join("enhanced-runtime/steer-repair/26.928.2636")
}

struct Archive {
    header: Value,
    raw_header: Vec<u8>,
    renderer: Vec<u8>,
    offset: u64,
}

fn archive(path: &Path) -> io::Result<Archive> {
    let mut file = fs::File::open(path)?;
    let mut pre = [0_u8; 16];
    file.read_exact(&mut pre)?;
    let header_size = u32::from_le_bytes(pre[12..16].try_into().unwrap()) as usize;
    if header_size > 32 * 1024 * 1024 {
        return Err(invalid("Invalid Desktop archive header size"));
    }
    let mut raw_header = vec![0; header_size];
    file.read_exact(&mut raw_header)?;
    let header: Value = serde_json::from_slice(&raw_header).map_err(invalid)?;
    let entry = &header["files"]["webview"]["files"]["assets"]["files"][ASSET];
    let size = entry["size"]
        .as_u64()
        .filter(|size| *size <= 16 * 1024 * 1024)
        .ok_or_else(|| invalid("Unsupported Desktop renderer size"))?;
    if entry["unpacked"].as_bool() == Some(true) || entry["integrity"]["algorithm"] != "SHA256" {
        return Err(invalid("Unsupported Desktop archive layout"));
    }
    let relative = entry["offset"]
        .as_str()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| invalid("Invalid Desktop renderer offset"))?;
    let offset = (8 + u64::from(u32::from_le_bytes(pre[4..8].try_into().unwrap())))
        .checked_add(relative)
        .ok_or_else(|| invalid("Invalid Desktop renderer offset"))?;
    file.seek(SeekFrom::Start(offset))?;
    let mut renderer = vec![0; size as usize];
    file.read_exact(&mut renderer)?;
    Ok(Archive {
        header,
        raw_header,
        renderer,
        offset,
    })
}

fn replace_once(bytes: &[u8], original: &str, replacement: &str) -> io::Result<Vec<u8>> {
    if original.len() != replacement.len() {
        return Err(invalid("Desktop patch must preserve file offsets"));
    }
    let matches = bytes
        .windows(original.len())
        .enumerate()
        .filter_map(|(offset, value)| (value == original.as_bytes()).then_some(offset))
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(invalid("Desktop patch target must occur exactly once"));
    }
    let mut result = bytes.to_vec();
    result[matches[0]..matches[0] + original.len()].copy_from_slice(replacement.as_bytes());
    Ok(result)
}

fn patch_renderer(bytes: &[u8], expected_hash: &str) -> io::Result<Vec<u8>> {
    if digest(bytes) != expected_hash {
        return Err(invalid(
            "This Codex Desktop version does not support Steer repair",
        ));
    }
    // Both hooks still run; only their composer quota-disable results change.
    let repaired_gate = format!("{:<width$}", "fn=(q(rP),!1)", width = GATE.len());
    let renderer = replace_once(bytes, GATE, &repaired_gate)?;
    replace_once(&renderer, DISABLE, &DISABLE.replace("||wt", "    "))
}

fn validate_original(executable: &Path) -> io::Result<Archive> {
    if digest(&fs::read(executable)?) != ORIGINAL_EXE {
        return Err(invalid(
            "This Codex Desktop version does not support Steer repair",
        ));
    }
    let source = archive(
        &executable
            .parent()
            .ok_or_else(|| invalid("Missing app directory"))?
            .join("resources/app.asar"),
    )?;
    if digest(&source.raw_header) != ORIGINAL_HEADER
        || digest(&source.renderer) != ORIGINAL_RENDERER
    {
        return Err(invalid("Unsupported Codex Desktop resources"));
    }
    Ok(source)
}

pub fn is_verified_copy(executable: &Path) -> bool {
    let result = (|| -> io::Result<bool> {
        if digest(&fs::read(executable)?) != REPAIRED_EXE {
            return Ok(false);
        }
        let data = archive(
            &executable
                .parent()
                .ok_or_else(|| invalid("Missing app directory"))?
                .join("resources/app.asar"),
        )?;
        Ok(digest(&data.raw_header) == REPAIRED_HEADER
            && digest(&data.renderer) == REPAIRED_RENDERER)
    })();
    result.unwrap_or(false)
}

fn original_executable(current: &Path) -> io::Result<PathBuf> {
    if is_verified_copy(current) {
        let marker = current.parent().unwrap().join("vellum-steer-repair.json");
        let metadata: Value = serde_json::from_slice(&fs::read(marker)?).map_err(invalid)?;
        let source = metadata["sourceApp"]
            .as_str()
            .map(PathBuf::from)
            .ok_or_else(|| invalid("Repair copy has no original app location"))?
            .join("ChatGPT.exe");
        validate_original(&source)?;
        return Ok(source);
    }
    if current.exists() {
        let native = current.parent().unwrap().join("ChatGPT.exe");
        if current
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("Codex.exe"))
            && validate_original(&native).is_ok()
        {
            return Ok(native);
        }
        return Ok(current.to_path_buf());
    }
    Err(invalid("Open Codex Desktop first"))
}

fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let destination = target.join(entry.file_name());
        if kind.is_symlink() {
            return Err(invalid("App copy contains a symbolic link"));
        }
        if kind.is_dir() {
            fs::create_dir(&destination)?;
            copy_tree(&entry.path(), &destination)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), destination)?;
        } else {
            return Err(invalid("Unsupported app copy entry"));
        }
    }
    Ok(())
}

pub fn prepare(root: &Path, source_executable: &Path) -> io::Result<PathBuf> {
    let mut data = validate_original(source_executable)?;
    let destination = copy_directory(root);
    let executable = destination.join("ChatGPT.exe");
    if destination.exists() {
        if is_verified_copy(&executable) {
            return Ok(executable);
        }
        return Err(invalid(
            "Existing Steer repair copy failed verification; it was not overwritten",
        ));
    }
    let source = fs::canonicalize(source_executable.parent().unwrap())?;
    fs::create_dir_all(destination.parent().unwrap())?;
    let parent = fs::canonicalize(destination.parent().unwrap())?;
    if parent.starts_with(&source) || source.starts_with(&parent) {
        return Err(invalid("Repair copy must be separate from installed app"));
    }
    // TempDir owns only the newly created directory beneath this verified root.
    let temporary = tempfile::Builder::new()
        .prefix("steer-stage-")
        .tempdir_in(&parent)?;
    if !temporary.path().starts_with(&parent) {
        return Err(invalid("Invalid staging location"));
    }
    copy_tree(&source, temporary.path())?;
    let renderer = patch_renderer(&data.renderer, ORIGINAL_RENDERER)?;
    if digest(&renderer) != REPAIRED_RENDERER {
        return Err(invalid("Renderer patch verification failed"));
    }
    let entry = &mut data.header["files"]["webview"]["files"]["assets"]["files"][ASSET];
    entry["integrity"]["hash"] = Value::String(REPAIRED_RENDERER.into());
    entry["integrity"]["blocks"] = serde_json::json!([REPAIRED_RENDERER]);
    let header = serde_json::to_vec(&data.header).map_err(invalid)?;
    if header.len() != data.raw_header.len() || digest(&header) != REPAIRED_HEADER {
        return Err(invalid("Archive header patch verification failed"));
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(temporary.path().join("resources/app.asar"))?;
    file.seek(SeekFrom::Start(16))?;
    file.write_all(&header)?;
    file.seek(SeekFrom::Start(data.offset))?;
    file.write_all(&renderer)?;
    file.sync_all()?;
    drop(file);
    let binary = replace_once(
        &fs::read(source_executable)?,
        ORIGINAL_HEADER,
        REPAIRED_HEADER,
    )?;
    if digest(&binary) != REPAIRED_EXE {
        return Err(invalid("Executable patch verification failed"));
    }
    fs::write(temporary.path().join("ChatGPT.exe"), binary)?;
    let marker = serde_json::json!({"sourceApp": source, "scope": "composer-quota-disable-only"});
    super::atomic::write_atomic(
        &temporary.path().join("vellum-steer-repair.json"),
        &serde_json::to_vec(&marker).map_err(invalid)?,
    )?;
    if !is_verified_copy(&temporary.path().join("ChatGPT.exe")) {
        return Err(invalid("Steer repair copy failed verification"));
    }
    fs::rename(temporary.path(), &destination)?;
    Ok(executable)
}

/// Resolve before stopping Desktop, so incompatibility cannot strand the user.
pub fn launch_executable(root: &Path, current: &Path, managed_pool: bool) -> io::Result<PathBuf> {
    let source = original_executable(current)?;
    if cfg!(target_os = "windows") && managed_pool && validate_original(&source).is_ok() {
        prepare(root, &source)
    } else {
        Ok(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_composer_gates_change_without_moving_bytes_or_removing_hooks() {
        let source = format!("before;{GATE};let {DISABLE};after");
        let repaired = patch_renderer(source.as_bytes(), &digest(source.as_bytes())).unwrap();
        assert_eq!(repaired.len(), source.len());
        let repaired = String::from_utf8(repaired).unwrap();
        assert!(repaired.contains("fn=(q(rP),!1)"));
        assert!(repaired.contains("Gn=je||St||rt&&it||vt||Ot||rn?.isLoading===!0    ||fn"));
        assert!(patch_renderer(source.as_bytes(), ORIGINAL_RENDERER).is_err());
    }

    #[test]
    fn corrupt_or_ambiguous_targets_are_rejected() {
        assert!(replace_once(b"abcabc", "abc", "def").is_err());
        assert!(replace_once(b"abc", "abc", "longer").is_err());
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("ChatGPT.exe"), "unknown").unwrap();
        assert!(!is_verified_copy(&root.path().join("ChatGPT.exe")));
        assert!(prepare(root.path(), &root.path().join("ChatGPT.exe")).is_err());
        assert!(!copy_directory(root.path()).exists());
    }

    #[test]
    fn unsupported_desktop_remains_unchanged_without_creating_a_copy() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("ChatGPT.exe");
        fs::write(&source, "unknown build").unwrap();
        assert_eq!(
            launch_executable(root.path(), &source, true).unwrap(),
            source
        );
        assert!(!copy_directory(root.path()).exists());
    }

    #[test]
    #[ignore = "requires the pinned installed Windows Codex app"]
    fn installed_source_matches_script_and_native_copy_round_trips() {
        let source =
            std::env::var("VELLUM_STEER_REPAIR_SOURCE").expect("set VELLUM_STEER_REPAIR_SOURCE");
        let root = tempfile::tempdir().unwrap();
        let source = fs::canonicalize(PathBuf::from(source).join("ChatGPT.exe")).unwrap();
        let repaired = launch_executable(root.path(), &source, true).unwrap();
        assert!(is_verified_copy(&repaired));
        assert_eq!(prepare(root.path(), &source).unwrap(), repaired);
        // Legacy disabled preferences cannot disable the default pool repair.
        fs::write(
            root.path().join("enhanced-runtime/steer-repair.json"),
            r#"{"enabled":false}"#,
        )
        .unwrap();
        assert_eq!(
            launch_executable(root.path(), &source, true).unwrap(),
            repaired
        );
        assert_eq!(
            launch_executable(root.path(), &repaired, false).unwrap(),
            source
        );
        assert_eq!(
            launch_executable(root.path(), &repaired, true).unwrap(),
            repaired
        );
        assert!(validate_original(&source).is_ok());
        fs::write(repaired, "corrupt").unwrap();
        assert!(prepare(root.path(), &source).is_err());
        assert!(launch_executable(root.path(), &source, true).is_err());
    }
}
