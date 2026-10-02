//! Pinned macOS composer repair. Only an owned copy is edited and re-signed.
use super::steer_repair::{archive_at, digest, replace_once, Archive};
use serde_json::Value;
use std::fs;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const VERSION: &str = "26.930.21537-arm64";
const ASSET: &str = "app-primary-c9f7ac16cee9.js";
const ORIGINAL_EXE: &str = "ac53f00ea78b96fd4e4aaca08f3acffce2657007c97965b86ef6ac28203c6665";
const REPAIRED_EXE: &str = "2cdedf0aa83957ed2893fb0182e463654045cff29334888093118f332102f242";
const FRAMEWORK: &str =
    "Contents/Frameworks/Codex Framework.framework/Versions/Current/Codex Framework";
const ORIGINAL_FRAMEWORK: &str = "9ad6b60505129bd1a866ca8e1d8882a0787c5e3a86f86b30e03487e31062ac70";
const REPAIRED_FRAMEWORK: &str = "c658821e6a4232ae8542dc272380fccf795b50b9d0d5b052e219b71100367477";
const INTEGRITY_SENTINEL: &[u8] = b"AGbevlPCksUGKNL8TSn7wGmJEuJsXb2A";
const ORIGINAL_HEADER: &str = "3db6024d6e1e2f6248f55e46b91be74b23cad2bc1f5406d4646ddafe11db3aef";
const ORIGINAL_RENDERER: &str = "ddd19cf4305f13cc6df9c2b3d83c7fd5c898f893e18c89d56ed7b064dd8835da";
const REPAIRED_HEADER: &str = "2dcd3ab5eba743a3bc24ebe35a20929629b1734c8cfd183dda772efe4dc55e88";
const REPAIRED_RENDERER: &str = "b395304f631ea9339ff570e7331715ad7af5ff8074c4d1d0db37ea8ab678381f";
const GATE: &str = "fn=X(mP)&&bt===`local`";
const DISABLE: &str = "Wn=Ae||St||rt&&it||vt||Ot||rn?.isLoading===!0||wt||fn";
fn marker_path(bundle: &Path) -> PathBuf {
    bundle.with_extension("vellum-send-repair.json")
}

fn invalid(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

pub fn bundle_path(executable: &Path) -> Option<&Path> {
    if executable.file_name()? != "ChatGPT"
        || executable.parent()?.file_name()? != "MacOS"
        || executable.parent()?.parent()?.file_name()? != "Contents"
    {
        return None;
    }
    let bundle = executable.parent()?.parent()?.parent()?;
    (bundle.file_name()? == "ChatGPT.app").then_some(bundle)
}

fn command_ok(command: &mut Command) -> io::Result<()> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(invalid(String::from_utf8_lossy(&output.stderr)));
    }
    Ok(())
}

fn plist_hash(bundle: &Path) -> io::Result<String> {
    let output = Command::new("/usr/bin/plutil")
        .args(["-extract", "ElectronAsarIntegrity", "json", "-o", "-"])
        .arg(bundle.join("Contents/Info.plist"))
        .output()?;
    if !output.status.success() {
        return Err(invalid("Missing macOS ASAR integrity"));
    }
    let value: Value = serde_json::from_slice(&output.stdout).map_err(invalid)?;
    if value["Resources/app.asar"]["algorithm"] != "SHA256" {
        return Err(invalid("Unsupported macOS ASAR integrity"));
    }
    value["Resources/app.asar"]["hash"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("Missing macOS ASAR hash"))
}

fn signed_bundle(bundle: &Path) -> io::Result<()> {
    command_ok(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(bundle),
    )
}

fn validate_original(executable: &Path) -> io::Result<Archive> {
    let bundle = bundle_path(executable).ok_or_else(|| invalid("Unsupported macOS bundle"))?;
    if digest(&fs::read(executable)?) != ORIGINAL_EXE
        || digest(&fs::read(bundle.join(FRAMEWORK))?) != ORIGINAL_FRAMEWORK
        || plist_hash(bundle)? != ORIGINAL_HEADER
    {
        return Err(invalid("Unsupported macOS Codex build"));
    }
    signed_bundle(bundle)?;
    let data = archive_at(&bundle.join("Contents/Resources/app.asar"), ASSET)?;
    if digest(&data.raw_header) != ORIGINAL_HEADER || digest(&data.renderer) != ORIGINAL_RENDERER {
        return Err(invalid("Unsupported macOS Codex resources"));
    }
    Ok(data)
}

fn patched_renderer(source: &[u8]) -> io::Result<Vec<u8>> {
    if digest(source) != ORIGINAL_RENDERER {
        return Err(invalid("Unsupported macOS composer"));
    }
    // Preserve the query subscription and the Reserve hook. Only their send
    // disable results change; empty-message/upload/loading guards stay intact.
    let gate = format!("{:<width$}", "fn=(X(mP),!1)", width = GATE.len());
    let result = replace_once(source, GATE, &gate)?;
    let result = replace_once(&result, DISABLE, &DISABLE.replace("||wt", "    "))?;
    if digest(&result) != REPAIRED_RENDERER {
        return Err(invalid("macOS renderer patch mismatch"));
    }
    Ok(result)
}

// The ad-hoc signature is deterministic for this audited bundle. Verify the
// pinned signed executable, resource seal and exact renderer, not provenance alone.
fn verify_copy(executable: &Path) -> bool {
    let result = (|| -> io::Result<bool> {
        let bundle = bundle_path(executable).ok_or_else(|| invalid("Invalid copy path"))?;
        let marker: Value =
            serde_json::from_slice(&fs::read(marker_path(bundle))?).map_err(invalid)?;
        if marker["version"] != VERSION
            || digest(&fs::read(executable)?) != REPAIRED_EXE
            || digest(&fs::read(bundle.join(FRAMEWORK))?) != REPAIRED_FRAMEWORK
            || plist_hash(bundle)? != REPAIRED_HEADER
        {
            return Ok(false);
        }
        signed_bundle(bundle)?;
        let data = archive_at(&bundle.join("Contents/Resources/app.asar"), ASSET)?;
        Ok(digest(&data.raw_header) == REPAIRED_HEADER
            && digest(&data.renderer) == REPAIRED_RENDERER)
    })();
    result.unwrap_or(false)
}

pub fn is_verified_copy(executable: &Path) -> bool {
    let Some(bundle) = bundle_path(executable) else {
        return false;
    };
    // Avoid hashing and codesign for every unrelated process during discovery.
    if !marker_path(bundle).is_file() {
        return false;
    }
    let Some(stamp) = copy_stamp(executable) else {
        return false;
    };
    let cache = VERIFIED_COPIES.get_or_init(Default::default);
    if let Some((cached, verdict)) = cache.lock().unwrap().get(executable) {
        if *cached == stamp {
            return *verdict;
        }
    }
    let verdict = verify_copy(executable);
    cache
        .lock()
        .unwrap()
        .insert(executable.to_path_buf(), (stamp, verdict));
    verdict
}

type CopyStamp = [(u64, std::time::SystemTime); 5];
static VERIFIED_COPIES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, (CopyStamp, bool)>>,
> = std::sync::OnceLock::new();

fn copy_stamp(executable: &Path) -> Option<CopyStamp> {
    let bundle = bundle_path(executable)?;
    let stamp = |path: &Path| {
        let metadata = fs::metadata(path).ok()?;
        Some((metadata.len(), metadata.modified().ok()?))
    };
    Some([
        stamp(executable)?,
        stamp(&bundle.join("Contents/Resources/app.asar"))?,
        stamp(&bundle.join("Contents/Info.plist"))?,
        stamp(&marker_path(bundle))?,
        stamp(&bundle.join(FRAMEWORK))?,
    ])
}

pub fn prepare(root: &Path, executable: &Path) -> io::Result<PathBuf> {
    let mut data = validate_original(executable)?;
    let source = fs::canonicalize(bundle_path(executable).unwrap())?;
    let parent = root.join("enhanced-runtime/send-repair-macos");
    fs::create_dir_all(&parent)?;
    let parent = fs::canonicalize(parent)?;
    if parent.starts_with(&source) || source.starts_with(&parent) {
        return Err(invalid("Repair must be separate from the installed app"));
    }
    let destination = parent.join(VERSION);
    let entry = destination.join("ChatGPT.app/Contents/MacOS/ChatGPT");
    if destination.exists() {
        if is_verified_copy(&entry) {
            return Ok(entry);
        }
        return Err(invalid(
            "Existing macOS repair copy is invalid; not overwritten",
        ));
    }
    let temporary = tempfile::Builder::new()
        .prefix("send-stage-")
        .tempdir_in(&parent)?;
    let bundle = temporary.path().join("ChatGPT.app");
    // ditto preserves the framework symlinks and executable permissions.
    command_ok(Command::new("/usr/bin/ditto").arg(&source).arg(&bundle))?;
    let renderer = patched_renderer(&data.renderer)?;
    let item = &mut data.header["files"]["webview"]["files"]["assets"]["files"][ASSET];
    item["integrity"]["hash"] = Value::String(REPAIRED_RENDERER.into());
    item["integrity"]["blocks"] = serde_json::json!([REPAIRED_RENDERER]);
    let header = serde_json::to_vec(&data.header).map_err(invalid)?;
    if header.len() != data.raw_header.len() || digest(&header) != REPAIRED_HEADER {
        return Err(invalid("macOS archive header mismatch"));
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(bundle.join("Contents/Resources/app.asar"))?;
    file.seek(SeekFrom::Start(16))?;
    file.write_all(&header)?;
    file.seek(SeekFrom::Start(data.offset))?;
    file.write_all(&renderer)?;
    file.sync_all()?;
    drop(file);
    let integrity =
        serde_json::json!({"Resources/app.asar":{"algorithm":"SHA256","hash":REPAIRED_HEADER}});
    command_ok(
        Command::new("/usr/bin/plutil")
            .args(["-replace", "ElectronAsarIntegrity", "-json"])
            .arg(integrity.to_string())
            .arg(bundle.join("Contents/Info.plist")),
    )?;
    patch_framework(&bundle)?;
    // Team-bound entitlements cannot be issued by a local ad-hoc signer.
    // Preserve Electron's runtime permissions; remove identity-only grants.
    let entitlements = local_entitlements(executable, temporary.path())?;
    command_ok(
        Command::new("/usr/bin/codesign")
            .args(["--force", "--deep", "--sign", "-", "--entitlements"])
            .arg(entitlements)
            .arg("--preserve-metadata=flags,runtime")
            .arg(&bundle),
    )?;
    signed_bundle(&bundle)?;
    if digest(&fs::read(bundle.join("Contents/MacOS/ChatGPT"))?) != REPAIRED_EXE
        || digest(&fs::read(bundle.join(FRAMEWORK))?) != REPAIRED_FRAMEWORK
    {
        return Err(invalid(format!(
            "macOS signed copy mismatch: executable={}, framework={}",
            digest(&fs::read(bundle.join("Contents/MacOS/ChatGPT"))?),
            digest(&fs::read(bundle.join(FRAMEWORK))?)
        )));
    }
    let marker = serde_json::json!({"version":VERSION,"sourceApp":source,
        "executableHash":digest(&fs::read(bundle.join("Contents/MacOS/ChatGPT"))?),
        "scope":"composer-quota-disable-only"});
    // Keep provenance beside the bundle, outside the app's resource seal.
    super::atomic::write_atomic(
        &marker_path(&bundle),
        &serde_json::to_vec(&marker).map_err(invalid)?,
    )?;
    if !is_verified_copy(&bundle.join("Contents/MacOS/ChatGPT")) {
        return Err(invalid("macOS repair copy verification failed"));
    }
    fs::rename(temporary.path(), &destination)?;
    Ok(entry)
}

fn patch_framework(bundle: &Path) -> io::Result<()> {
    use sha2::{Digest, Sha256};
    let path = bundle.join(FRAMEWORK);
    let mut bytes = fs::read(&path)?;
    if digest(&bytes) != ORIGINAL_FRAMEWORK {
        return Err(invalid("Unsupported Electron Framework"));
    }
    let matches: Vec<_> = bytes
        .windows(INTEGRITY_SENTINEL.len())
        .enumerate()
        .filter_map(|(offset, value)| (value == INTEGRITY_SENTINEL).then_some(offset))
        .collect();
    if matches.len() != 1 {
        return Err(invalid("Ambiguous Framework integrity slot"));
    }
    let offset = matches[0] + INTEGRITY_SENTINEL.len();
    let old = Sha256::digest(format!("Resources/app.asarSHA256{ORIGINAL_HEADER}").as_bytes());
    let new = Sha256::digest(format!("Resources/app.asarSHA256{REPAIRED_HEADER}").as_bytes());
    if bytes.get(offset..offset + 2) != Some(&[1, 1])
        || bytes.get(offset + 2..offset + 34) != Some(old.as_slice())
    {
        return Err(invalid("Unsupported Framework integrity digest"));
    }
    bytes[offset + 2..offset + 34].copy_from_slice(&new);
    fs::write(path, bytes)
}

fn local_entitlements(executable: &Path, stage: &Path) -> io::Result<PathBuf> {
    let output = Command::new("/usr/bin/codesign")
        .args(["-d", "--entitlements", ":-"])
        .arg(executable)
        .output()?;
    if !output.status.success() {
        return Err(invalid("Cannot read Electron entitlements"));
    }
    let path = stage.join("entitlements.plist");
    fs::write(&path, output.stdout)?;
    command_ok(
        Command::new("/usr/bin/plutil")
            .args(["-convert", "json"])
            .arg(&path),
    )?;
    let mut value: Value = serde_json::from_slice(&fs::read(&path)?).map_err(invalid)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid("Invalid Electron entitlements"))?;
    for key in [
        "com.apple.application-identifier",
        "com.apple.developer.team-identifier",
        "com.apple.developer.aps-environment",
        "com.apple.security.application-groups",
        "keychain-access-groups",
    ] {
        object.remove(key);
    }
    object.insert(
        "com.apple.security.cs.disable-library-validation".into(),
        Value::Bool(true),
    );
    fs::write(&path, serde_json::to_vec(&value).map_err(invalid)?)?;
    command_ok(
        Command::new("/usr/bin/plutil")
            .args(["-convert", "xml1"])
            .arg(&path),
    )?;
    Ok(path)
}

pub fn launch_executable(root: &Path, current: &Path) -> PathBuf {
    let source = if is_verified_copy(current) {
        bundle_path(current)
            .and_then(|bundle| fs::read(marker_path(bundle)).ok())
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| value["sourceApp"].as_str().map(PathBuf::from))
            .map(|bundle| bundle.join("Contents/MacOS/ChatGPT"))
            .filter(|path| path.is_file())
            .unwrap_or_else(|| current.to_path_buf())
    } else {
        current.to_path_buf()
    };
    if source == current && is_verified_copy(current) {
        return source;
    }
    match prepare(root, &source) {
        Ok(copy) => copy,
        Err(error) => {
            log::warn!(
                "[CodexSendRepair] macOS repair unavailable; keeping installed app: {error}"
            );
            source
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrelated_bundles_and_cli_workers_are_rejected() {
        assert!(bundle_path(Path::new(
            "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT"
        ))
        .is_some());
        assert!(bundle_path(Path::new("/Applications/Other.app/Contents/MacOS/ChatGPT")).is_none());
        assert!(bundle_path(Path::new(
            "/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex"
        ))
        .is_none());
        assert!(patched_renderer(b"unknown").is_err());
    }

    #[test]
    fn unsupported_app_does_not_block_restart() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("ChatGPT.app/Contents/MacOS/ChatGPT");
        fs::create_dir_all(app.parent().unwrap()).unwrap();
        fs::write(&app, b"unknown").unwrap();
        assert_eq!(launch_executable(root.path(), &app), app);
        assert!(!root.path().join("enhanced-runtime").exists());
    }

    #[test]
    #[ignore = "requires the audited installed arm64 macOS app"]
    fn installed_app_copy_verifies_and_keeps_original_untouched() {
        let root = PathBuf::from(std::env::var("VELLUM_MAC_SEND_TEST_ROOT").unwrap());
        let app = Path::new("/Applications/ChatGPT.app/Contents/MacOS/ChatGPT");
        let repaired = prepare(&root, app).unwrap();
        assert_ne!(repaired, app);
        assert!(is_verified_copy(&repaired));
        assert_eq!(prepare(&root, app).unwrap(), repaired);
        assert_eq!(launch_executable(&root, &repaired), repaired);
        assert!(validate_original(app).is_ok());
        if let Ok(output) = std::env::var("VELLUM_SEND_REPAIR_RENDERER_OUTPUT") {
            let data = archive_at(
                &bundle_path(&repaired)
                    .unwrap()
                    .join("Contents/Resources/app.asar"),
                ASSET,
            )
            .unwrap();
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output)
                .unwrap();
            output.write_all(&data.renderer).unwrap();
        }
    }
}
