//! Automatic pinned Windows Desktop send-button repair for managed launches.
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

struct RepairProfile {
    version: &'static str,
    asset: &'static str,
    original_renderer: &'static str,
    repaired_renderer: &'static str,
    original_header: &'static str,
    repaired_header: &'static str,
    original_exe: &'static str,
    repaired_exe: &'static str,
    gate: &'static str,
    repaired_gate: &'static str,
    disable: &'static str,
    reserve_gate: &'static str,
}

const PROFILES: &[RepairProfile] = &[
    RepairProfile {
        version: "26.928.2636",
        asset: ASSET,
        original_renderer: ORIGINAL_RENDERER,
        repaired_renderer: REPAIRED_RENDERER,
        original_header: ORIGINAL_HEADER,
        repaired_header: REPAIRED_HEADER,
        original_exe: ORIGINAL_EXE,
        repaired_exe: REPAIRED_EXE,
        gate: GATE,
        repaired_gate: "fn=(q(rP),!1)",
        disable: DISABLE,
        reserve_gate: "||wt",
    },
    RepairProfile {
        version: "26.928.4866",
        asset: "app-primary-2b539a729a98.js",
        original_renderer: "2c5259d8c72b2b7f3c437c372bdbad340357156036ca02d6e92caa5c08e55b29",
        repaired_renderer: "ac000108ac31d987767b119f67ad95ddee7689fdbf98e5f9ecc740972cab76c9",
        original_header: "1e596e41423edb250a8a489763865064cff2d59a96a636758acd0df9fc63d0ba",
        repaired_header: "1d25de483c53f80708f3447c6c3c455aa04dbe1d24fcb06f877d83a6f8f69dbb",
        original_exe: "c11cdd4ed0e0f25eddc1d54035e7b87932f07ac6b011ced04d424e368fedb9a0",
        repaired_exe: "f287488cfa8b455cbf13a89ee520808eb863b2dfad8f9dc03081ef6561c9de10",
        gate: "dn=Y(lP)&&yt===`local`",
        repaired_gate: "dn=(Y(lP),!1)",
        disable: "Un=je||xt||nt&&rt||_t||Dt||tn?.isLoading===!0||Ct||dn",
        reserve_gate: "||Ct",
    },
    RepairProfile {
        version: "26.930.2377",
        asset: "app-primary-85e5c56f1696.js",
        original_renderer: "f9f2825579ca55161694bd6e6aa7b84800fa39813de93b6caab74fa73d6ff98a",
        repaired_renderer: "3786ba570864f9e64d8274a370a256f1d25630252eba806e1707bef7469005f4",
        original_header: "cc672940d88b7bf98e0b72497cbe2389330b3832f0cb49536ef23041e21a33e3",
        repaired_header: "9a61219c3013cde90d710deacf016eb5ecfda5f8abe37edd051826b12615c3f0",
        original_exe: "27d4a13c2557cfb9b5d3360b0977828103b774b87295198abc7b901d4c223325",
        repaired_exe: "8ccc70db766e02540d1956766eca6fa031b9aac16e0ea532ee754e07846304ee",
        // fn: native login quota gate (mP requires rate_limit.allowed===false).
        // wt: the gpt-reserve atom's hardBlocked. Same roles as 26.928.
        gate: "fn=X(mP)&&bt===`local`",
        repaired_gate: "fn=(X(mP),!1)",
        disable: "Wn=Ae||St||rt&&it||vt||Ot||rn?.isLoading===!0||wt||fn",
        reserve_gate: "||wt",
    },
];

fn invalid(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn copy_directory(root: &Path, profile: &RepairProfile) -> PathBuf {
    root.join("enhanced-runtime/steer-repair")
        .join(profile.version)
}

struct Archive {
    header: Value,
    raw_header: Vec<u8>,
    renderer: Vec<u8>,
    offset: u64,
}

fn archive(path: &Path, profile: &RepairProfile) -> io::Result<Archive> {
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
    let entry = &header["files"]["webview"]["files"]["assets"]["files"][profile.asset];
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

fn patch_renderer(
    bytes: &[u8],
    expected_hash: &str,
    profile: &RepairProfile,
) -> io::Result<Vec<u8>> {
    if digest(bytes) != expected_hash {
        return Err(invalid(
            "This Codex Desktop version does not support send-button repair",
        ));
    }
    // Both hooks still run; only their composer quota-disable results change.
    let repaired_gate = format!(
        "{:<width$}",
        profile.repaired_gate,
        width = profile.gate.len()
    );
    let renderer = replace_once(bytes, profile.gate, &repaired_gate)?;
    replace_once(
        &renderer,
        profile.disable,
        &profile.disable.replace(profile.reserve_gate, "    "),
    )
}

fn validate_original(executable: &Path) -> io::Result<(Archive, &'static RepairProfile)> {
    let hash = digest(&fs::read(executable)?);
    let profile = PROFILES
        .iter()
        .find(|profile| profile.original_exe == hash)
        .ok_or_else(|| invalid("This Codex Desktop version does not support send-button repair"))?;
    let source = archive(
        &executable
            .parent()
            .ok_or_else(|| invalid("Missing app directory"))?
            .join("resources/app.asar"),
        profile,
    )?;
    if digest(&source.raw_header) != profile.original_header
        || digest(&source.renderer) != profile.original_renderer
    {
        return Err(invalid("Unsupported Codex Desktop resources"));
    }
    Ok((source, profile))
}

/// Size and mtime of the executable and its archive. Electron runs a dozen
/// ChatGPT.exe processes and status polling discovers them all, so a verdict
/// is reused until either file changes instead of re-hashing ~20MB each time.
type CopyStamp = [(u64, std::time::SystemTime); 2];
static VERIFIED_COPIES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, (CopyStamp, bool)>>,
> = std::sync::OnceLock::new();

fn copy_stamp(executable: &Path) -> Option<CopyStamp> {
    let stamp = |path: &Path| {
        let metadata = fs::metadata(path).ok()?;
        Some((metadata.len(), metadata.modified().ok()?))
    };
    Some([
        stamp(executable)?,
        stamp(&executable.parent()?.join("resources/app.asar"))?,
    ])
}

pub fn is_verified_copy(executable: &Path) -> bool {
    // Only the native Desktop entry can be a repair copy. In particular, do
    // not hash every CLI worker named Codex.exe during process discovery.
    if !executable
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("ChatGPT.exe"))
    {
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

fn verify_copy(executable: &Path) -> bool {
    let result = (|| -> io::Result<bool> {
        let hash = digest(&fs::read(executable)?);
        let Some(profile) = PROFILES.iter().find(|profile| profile.repaired_exe == hash) else {
            return Ok(false);
        };
        let data = archive(
            &executable
                .parent()
                .ok_or_else(|| invalid("Missing app directory"))?
                .join("resources/app.asar"),
            profile,
        )?;
        Ok(digest(&data.raw_header) == profile.repaired_header
            && digest(&data.renderer) == profile.repaired_renderer)
    })();
    result.unwrap_or(false)
}

/// Where the installed Store app keeps its Electron profile.
///
/// MSIX virtualizes the packaged app's %APPDATA% writes into the package's
/// LocalCache. A copy runs without package identity and would otherwise start
/// on an empty %APPDATA%\Codex. Desktop honours CODEX_ELECTRON_USER_DATA_PATH,
/// so point the copy back at the installed app's profile.
pub fn user_data_override(executable: &Path) -> Option<PathBuf> {
    if !is_verified_copy(executable) {
        return None;
    }
    let marker = fs::read(executable.parent()?.join("vellum-steer-repair.json")).ok()?;
    let metadata: Value = serde_json::from_slice(&marker).ok()?;
    let family = package_family(Path::new(metadata["sourceApp"].as_str()?))?;
    let profile = dirs::data_local_dir()?
        .join("Packages")
        .join(family)
        .join("LocalCache/Roaming/Codex");
    profile.is_dir().then_some(profile)
}

/// `…\WindowsApps\OpenAI.Codex_26.930.2377.0_x64__2p2nqsd0c76g0\app` →
/// `OpenAI.Codex_2p2nqsd0c76g0`. Non-Store installs have no package family.
fn package_family(source_app: &Path) -> Option<String> {
    source_app.components().find_map(|component| {
        let name = component.as_os_str().to_str()?;
        let (full_name, publisher) = name.split_once("__")?;
        let (package, _) = full_name.split_once('_')?;
        (package.eq_ignore_ascii_case("OpenAI.Codex") && !publisher.is_empty())
            .then(|| format!("{package}_{publisher}"))
    })
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
        // An in-place update leaves an unsupported build at the same path; it
        // still launches, unrepaired. A removed install (Store update) errors.
        if !source.is_file() {
            return Err(invalid("Repair copy's original app is no longer installed"));
        }
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
    let (mut data, profile) = validate_original(source_executable)?;
    let destination = copy_directory(root, profile);
    let executable = destination.join("ChatGPT.exe");
    if destination.exists() {
        if is_verified_copy(&executable) && digest(&fs::read(&executable)?) == profile.repaired_exe
        {
            return Ok(executable);
        }
        return Err(invalid(
            "Existing send-button repair copy failed verification; it was not overwritten",
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
    let renderer = patch_renderer(&data.renderer, profile.original_renderer, profile)?;
    if digest(&renderer) != profile.repaired_renderer {
        return Err(invalid("Renderer patch verification failed"));
    }
    let entry = &mut data.header["files"]["webview"]["files"]["assets"]["files"][profile.asset];
    entry["integrity"]["hash"] = Value::String(profile.repaired_renderer.into());
    entry["integrity"]["blocks"] = serde_json::json!([profile.repaired_renderer]);
    let header = serde_json::to_vec(&data.header).map_err(invalid)?;
    if header.len() != data.raw_header.len() || digest(&header) != profile.repaired_header {
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
        profile.original_header,
        profile.repaired_header,
    )?;
    if digest(&binary) != profile.repaired_exe {
        return Err(invalid("Executable patch verification failed"));
    }
    fs::write(temporary.path().join("ChatGPT.exe"), binary)?;
    let marker = serde_json::json!({"sourceApp": source, "scope": "composer-quota-disable-only"});
    super::atomic::write_atomic(
        &temporary.path().join("vellum-steer-repair.json"),
        &serde_json::to_vec(&marker).map_err(invalid)?,
    )?;
    if !is_verified_copy(&temporary.path().join("ChatGPT.exe")) {
        return Err(invalid("Send-button repair copy failed verification"));
    }
    fs::rename(temporary.path(), &destination)?;
    Ok(executable)
}

/// Resolve before stopping Desktop, so incompatibility cannot strand the user.
///
/// The repair is optional: it never fails a restart. Any problem falls back
/// to the unmodified app, or to the running copy when the app it came from
/// is gone, since that copy is verified and still runs.
pub fn launch_executable(root: &Path, current: &Path) -> PathBuf {
    let source = match original_executable(current) {
        Ok(source) => source,
        Err(error) => {
            log::warn!("[CodexSendRepair] keeping the current Desktop executable: {error}");
            return current.to_path_buf();
        }
    };
    if !cfg!(target_os = "windows") {
        return source;
    }
    if validate_original(&source).is_err() {
        log::warn!("[CodexSendRepair] unsupported Desktop build: general-send quota gates were not repaired");
        return source;
    }
    match prepare(root, &source) {
        Ok(repaired) => repaired,
        Err(error) => {
            log::warn!("[CodexSendRepair] repair copy unavailable, launching the unmodified app: {error}");
            source
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_composer_gates_change_without_moving_bytes_or_removing_hooks() {
        for profile in PROFILES {
            let source = format!("before;{};let {};after", profile.gate, profile.disable);
            let repaired =
                patch_renderer(source.as_bytes(), &digest(source.as_bytes()), profile).unwrap();
            assert_eq!(repaired.len(), source.len());
            let repaired = String::from_utf8(repaired).unwrap();
            assert!(repaired.contains(profile.repaired_gate));
            assert!(repaired.contains(&profile.disable.replace(profile.reserve_gate, "    ")));
            assert!(patch_renderer(source.as_bytes(), profile.original_renderer, profile).is_err());
        }
    }

    #[test]
    fn corrupt_or_ambiguous_targets_are_rejected() {
        assert!(replace_once(b"abcabc", "abc", "def").is_err());
        assert!(replace_once(b"abc", "abc", "longer").is_err());
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("ChatGPT.exe"), "unknown").unwrap();
        assert!(!is_verified_copy(&root.path().join("ChatGPT.exe")));
        assert!(prepare(root.path(), &root.path().join("ChatGPT.exe")).is_err());
        assert!(!root.path().join("enhanced-runtime/steer-repair").exists());
    }

    #[test]
    fn unsupported_desktop_remains_unchanged_without_creating_a_copy() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("ChatGPT.exe");
        fs::write(&source, "unknown build").unwrap();
        assert_eq!(launch_executable(root.path(), &source), source);
        assert!(!root.path().join("enhanced-runtime/steer-repair").exists());
    }

    #[test]
    fn package_family_comes_from_the_store_install_directory() {
        assert_eq!(
            package_family(Path::new(
                r"\\?\C:\Program Files\WindowsApps\OpenAI.Codex_26.930.2377.0_x64__2p2nqsd0c76g0\app"
            ))
            .as_deref(),
            Some("OpenAI.Codex_2p2nqsd0c76g0")
        );
        assert_eq!(
            package_family(Path::new(r"C:\Users\a\AppData\Local\Programs\OpenAI\Codex")),
            None
        );
    }

    #[test]
    fn verification_is_reused_until_the_files_change() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("ChatGPT.exe");
        fs::create_dir_all(root.path().join("resources")).unwrap();
        fs::write(&executable, "unknown").unwrap();
        fs::write(root.path().join("resources/app.asar"), "x").unwrap();
        assert!(!is_verified_copy(&executable));
        // Seed a different verdict: the next call must come from the cache.
        let stamp = copy_stamp(&executable).unwrap();
        VERIFIED_COPIES
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .insert(executable.clone(), (stamp, true));
        assert!(is_verified_copy(&executable));
        fs::write(&executable, "changed size").unwrap();
        assert!(!is_verified_copy(&executable));
        assert!(user_data_override(&executable).is_none());
    }

    #[test]
    fn a_missing_executable_never_fails_the_restart() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("gone/ChatGPT.exe");
        assert_eq!(launch_executable(root.path(), &missing), missing);
        assert!(!root.path().join("enhanced-runtime/steer-repair").exists());
    }

    #[test]
    #[ignore = "requires the pinned installed Windows Codex app"]
    fn installed_source_general_send_repair_is_independent_of_pool_and_proxy() {
        let source =
            std::env::var("VELLUM_STEER_REPAIR_SOURCE").expect("set VELLUM_STEER_REPAIR_SOURCE");
        let root = tempfile::tempdir().unwrap();
        let source = fs::canonicalize(PathBuf::from(source).join("ChatGPT.exe")).unwrap();
        let repaired = launch_executable(root.path(), &source);
        assert!(is_verified_copy(&repaired));
        assert_eq!(prepare(root.path(), &source).unwrap(), repaired);
        // No pool/proxy settings or bridge are needed to preserve general send.
        assert!(crate::commands::runtime::use_direct_desktop_launch(
            &repaired, false
        ));
        assert!(!crate::commands::runtime::use_direct_desktop_launch(
            &source, false
        ));
        // Legacy disabled preferences cannot disable the default send repair.
        fs::write(
            root.path().join("enhanced-runtime/steer-repair.json"),
            r#"{"enabled":false}"#,
        )
        .unwrap();
        assert_eq!(launch_executable(root.path(), &source), repaired);
        assert_eq!(launch_executable(root.path(), &repaired), repaired);
        assert!(validate_original(&source).is_ok());
        if let Ok(path) = std::env::var("VELLUM_SEND_REPAIR_RENDERER_OUTPUT") {
            let (_, profile) = validate_original(&source).unwrap();
            let data = archive(
                &repaired.parent().unwrap().join("resources/app.asar"),
                profile,
            )
            .unwrap();
            // Export only static renderer bytes for the ordinary-send DOM regression.
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .unwrap();
            output.write_all(&data.renderer).unwrap();
        }
        fs::write(repaired, "corrupt").unwrap();
        assert!(prepare(root.path(), &source).is_err());
        // A corrupt copy is left alone, and the restart falls back to the app.
        assert_eq!(launch_executable(root.path(), &source), source);
    }
}
