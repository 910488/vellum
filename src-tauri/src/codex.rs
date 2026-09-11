use crate::error::{AppError, AppResult};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, Read, Seek, SeekFrom},
    sync::{Mutex, OnceLock},
    time::SystemTime,
};
use toml_edit::{value, DocumentMut, Item, Table};

// `model_providers` is included because apply_proxy_config removes reserved
// provider tables to keep Codex from resolving the proxy through stale custom
// definitions. `openai_base_url` remains listed so leases written by versions
// that pointed it at the authenticated proxy can still restore it safely.
// The three `model_*` global-override keys are listed so Vellum can clear
// them while it runs: bundled Codex applies `model_context_window` and
// `model_auto_compact_token_limit` as global overrides *on top of* the
// per-model catalog entry, so a pre-existing user value here would silently
// override the catalog's per-model context capacity and runtime-owned compact
// threshold. Vellum never writes a value for any of the three; it only ever
// removes them while active and restores exactly what the user had (or leaves
// them absent) on stop — see
// `clear_conflicting_global_compaction_keys`.
const MANAGED_KEYS: &[&str] = &[
    "model_provider",
    "openai_base_url",
    "model_catalog_json",
    "model_providers",
    "model_context_window",
    "model_auto_compact_token_limit",
    "model_auto_compact_token_limit_scope",
];
const MANAGED_FEATURE_KEYS: &[&str] = &[
    "standalone_web_search",
    "remote_compaction_v2",
    "enable_request_compression",
    "image_generation",
];
/// Keys under `[agents]` that Vellum manages for native Codex sub-agents.
/// Only these two nested keys are written and restored; everything else in
/// `[agents]` (enabled, thread caps, custom agent files, …) is left alone.
const MANAGED_AGENT_KEYS: &[&str] = &[
    "default_subagent_model",
    "default_subagent_reasoning_effort",
];
pub const MIN_NATIVE_SUBAGENT_DEFAULTS_VERSION: &str = "0.147.0";

#[derive(Debug, Clone)]
pub struct CodexPaths {
    pub config: PathBuf,
    pub auth: PathBuf,
    pub models_cache: PathBuf,
    pub catalog: PathBuf,
    pub lease: PathBuf,
}

/// Legacy (pre-v2) lease shape. Only ever *read*: a lease in this shape has no
/// owner information, so [`reconcile_lease`] can never confirm who -- if
/// anyone -- is still managing it. Read for migration only.
#[derive(Debug, Serialize, Deserialize)]
struct LegacyConfigLease {
    original_config: String,
    applied_config: String,
}

/// Identifies the process that wrote a [`ConfigLeaseV2`], so a later launch
/// can tell a crashed owner (safe to reclaim) from one still running (must
/// not touch the config out from under it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseOwner {
    pub install_id: String,
    pub instance_id: String,
    pub pid: u32,
    /// Best-effort OS process creation timestamp, opaque and
    /// platform-specific. Its only use is equality: if the recorded pid is
    /// alive but its start time no longer matches, the pid was recycled by an
    /// unrelated process and the lease is actually orphaned.
    pub process_start_time: Option<String>,
    pub proxy_port: u16,
    pub proxy_identity_nonce: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConfigLeaseV2 {
    schema_version: u32,
    owner: LeaseOwner,
    original_config: String,
    applied_config: String,
    original_hash: String,
    applied_hash: String,
    boundary_credential_id: String,
    created_at: String,
}

const LEASE_SCHEMA_VERSION: u32 = 2;

fn hash_config(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// What a fresh reconcile pass found on disk, resolved down to what the
/// caller should do next. Building a new [`ConfigLeaseV2`] is always safe
/// after this returns `Ok`; nothing here fabricates ownership.
enum StoredLease {
    V2(Box<ConfigLeaseV2>),
    /// Pre-v2 lease: usable for restore (it still carries the two config
    /// strings), but never for an owner-liveness decision.
    Legacy {
        original_config: String,
        applied_config: String,
    },
}

fn read_stored_lease(paths: &CodexPaths) -> AppResult<Option<StoredLease>> {
    if !paths.lease.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&paths.lease)
        .map_err(|error| AppError::Message(format!("無法讀取 Codex lease：{error}")))?;
    if let Ok(v2) = serde_json::from_slice::<ConfigLeaseV2>(&bytes) {
        return Ok(Some(StoredLease::V2(Box::new(v2))));
    }
    let legacy: LegacyConfigLease = serde_json::from_slice(&bytes)
        .map_err(|error| AppError::Message(format!("Codex lease 已損壞：{error}")))?;
    Ok(Some(StoredLease::Legacy {
        original_config: legacy.original_config,
        applied_config: legacy.applied_config,
    }))
}

impl StoredLease {
    fn original_config(&self) -> &str {
        match self {
            Self::V2(lease) => &lease.original_config,
            Self::Legacy {
                original_config, ..
            } => original_config,
        }
    }

    fn applied_config(&self) -> &str {
        match self {
            Self::V2(lease) => &lease.applied_config,
            Self::Legacy { applied_config, .. } => applied_config,
        }
    }
}

/// Build the [`LeaseOwner`] record for *this* process, right before it writes
/// a fresh lease. `proxy_identity_nonce` is drawn fresh per call: it is not a
/// secret, only a per-boot tag so a later reconcile can tell "the process
/// still holding `proxy_port` really is the one that wrote this lease" from
/// "something else happens to hold that port now".
fn build_lease_owner(port: u16) -> LeaseOwner {
    let pid = std::process::id();
    LeaseOwner {
        install_id: "desktop".into(),
        instance_id: ulid::Ulid::new().to_string(),
        pid,
        process_start_time: process_start_time(pid),
        proxy_port: port,
        proxy_identity_nonce: ulid::Ulid::new().to_string(),
    }
}

/// Whether the process identified by `pid` is currently alive. Used only to
/// tell a crashed lease owner from one still running; never a security
/// boundary, since a recycled pid can produce a false positive (mitigated by
/// also checking [`process_start_time`]).
#[cfg(target_os = "windows")]
fn process_is_alive(pid: u32) -> bool {
    windows_process_snapshot()
        .is_some_and(|processes| processes.iter().any(|(entry, _)| *entry == pid))
}

#[cfg(target_os = "windows")]
fn process_start_time(pid: u32) -> Option<String> {
    // ToolHelp process entries do not carry a creation timestamp; shelling out
    // to PowerShell mirrors the macOS `ps -o lstart=` fallback below rather
    // than pulling in the extra windows-sys threading feature for
    // `GetProcessTimes`.
    let mut command = crate::process::background_command("powershell");
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &format!("(Get-Process -Id {pid} -ErrorAction Stop).StartTime.Ticks"),
    ]);
    command.output().ok().and_then(|output| {
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

#[cfg(target_os = "windows")]
fn windows_process_snapshot() -> Option<Vec<(u32, u32)>> {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut processes = Vec::new();
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                processes.push((entry.th32ProcessID, entry.th32ParentProcessID));
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        Some(processes)
    }
}

#[cfg(not(target_os = "windows"))]
fn process_is_alive(pid: u32) -> bool {
    let mut command = std::process::Command::new("kill");
    command.args(["-0", &pid.to_string()]);
    command.status().is_ok_and(|status| status.success())
}

#[cfg(not(target_os = "windows"))]
fn process_start_time(pid: u32) -> Option<String> {
    let mut command = std::process::Command::new("ps");
    command.args(["-p", &pid.to_string(), "-o", "lstart="]);
    command.output().ok().and_then(|output| {
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

/// Confirm the process holding a lease's recorded `pid` is still that same
/// process, not an unrelated one that reused the pid. Missing start-time data
/// on either side (platform without a reliable probe) falls back to the
/// liveness check alone rather than blocking recovery on data nothing can
/// supply.
fn owner_still_alive(owner: &LeaseOwner) -> bool {
    if !process_is_alive(owner.pid) {
        return false;
    }
    match (&owner.process_start_time, process_start_time(owner.pid)) {
        (Some(recorded), Some(current)) => *recorded == current,
        _ => true,
    }
}

/// Resolve any lease left on disk before Vellum touches the Codex config or
/// writes a new one. Runs first in the start sequence, ahead of the boundary
/// key and the protocol probe, because it is the cheapest check and the one
/// most likely to explain a confusing prior state.
///
/// A dead owner's lease is left in place (not rewritten) so its
/// `original_config` -- the true pre-Vellum baseline -- survives the crash;
/// [`apply_proxy_config`] already skips writing a new lease when one exists.
/// Its `owner` is reclaimed separately so a *third* crash in a row still
/// finds a live-looking owner to compare against instead of drifting back to
/// stale process data forever.
pub fn reconcile_lease(paths: &CodexPaths) -> AppResult<()> {
    let Some(stored) = read_stored_lease(paths)? else {
        return Ok(());
    };
    let current = read_config(&paths.config)?;

    if current == stored.original_config() {
        // Either we (or the user) already reverted everything this lease
        // covered; there is nothing left to manage or restore.
        std::fs::remove_file(&paths.lease)
            .map_err(|error| AppError::Message(format!("無法移除 Codex lease：{error}")))?;
        remove_managed_catalog(paths)?;
        return Ok(());
    }

    match stored {
        StoredLease::Legacy { .. } => Err(AppError::Message(format!(
            "ConfigLeaseConflict: {} 是舊格式 lease，沒有記錄擁有者，Vellum 不會自動採用或還原。\
             請先確認 Codex 沒有正在使用中的工作階段，再手動刪除該檔案讓 Vellum 重新接管，\
             或手動還原 {} 後再啟動 Proxy。",
            paths.lease.display(),
            paths.config.display()
        ))),
        StoredLease::V2(lease) => {
            if owner_still_alive(&lease.owner) {
                Err(AppError::Message(format!(
                    "ConfigLeaseConflict: {} 顯示 pid {}{} 仍在管理 Codex 設定。\
                     如果該程序已經不是這個 Vellum，請先結束它再啟動 Proxy；\
                     不要刪除 lease 來搶同一個 config.toml。",
                    paths.lease.display(),
                    lease.owner.pid,
                    crate::enhanced_runtime::process_info::executable_of(lease.owner.pid)
                        .map(|path| format!(" ({})", path.display()))
                        .unwrap_or_default()
                )))
            } else {
                let mut reclaimed = lease;
                reclaimed.owner = build_lease_owner(reclaimed.owner.proxy_port);
                write_json_atomic(&paths.lease, &reclaimed)
            }
        }
    }
}

impl CodexPaths {
    pub fn discover(data_root: &Path) -> Self {
        let codex_home = {
            #[cfg(test)]
            {
                let nested = data_root.join("codex-home");
                if nested.is_dir() {
                    nested
                } else {
                    Self::env_codex_home()
                }
            }
            #[cfg(not(test))]
            {
                Self::env_codex_home()
            }
        };
        Self {
            config: codex_home.join("config.toml"),
            auth: codex_home.join("auth.json"),
            models_cache: codex_home.join("models_cache.json"),
            catalog: data_root.join("vellum-model-catalog.json"),
            lease: data_root.join("codex-config-lease.json"),
        }
    }

    fn env_codex_home() -> PathBuf {
        std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
            .unwrap_or_else(|| std::env::temp_dir().join(".codex"))
    }
}

fn parse_codex_version(text: &str) -> Option<(u64, u64, u64)> {
    text.split_whitespace().find_map(|token| {
        let mut parts = token
            .trim_start_matches('v')
            .split(|character: char| !character.is_ascii_digit() && character != '.');
        let version = parts.next()?;
        let mut numbers = version.split('.');
        let major = numbers.next()?.parse().ok()?;
        let minor = numbers.next()?.parse().ok()?;
        let patch = numbers.next()?.parse().ok()?;
        Some((major, minor, patch))
    })
}

fn push_unique_codex_candidate(candidates: &mut Vec<PathBuf>, candidate: PathBuf) {
    let duplicate = candidates.iter().any(|existing| {
        #[cfg(windows)]
        {
            existing
                .to_string_lossy()
                .eq_ignore_ascii_case(&candidate.to_string_lossy())
        }
        #[cfg(not(windows))]
        {
            existing == &candidate
        }
    });
    if !duplicate {
        candidates.push(candidate);
    }
}

/// Codex Desktop extracts its current native CLI below
/// `%LOCALAPPDATA%\OpenAI\Codex\bin\<content-hash>\codex.exe`. The content
/// hash changes across Desktop updates, so discovery must enumerate the
/// versioned children instead of persisting one resolved path.
#[cfg(windows)]
fn desktop_runtime_codex_candidates(local_app_data: &Path) -> Vec<PathBuf> {
    let runtime_root = local_app_data.join("OpenAI/Codex/bin");
    let mut discovered = std::fs::read_dir(&runtime_root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let executable = entry.path().join("codex.exe");
            executable.is_file().then(|| {
                let modified = executable
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok();
                (modified, executable)
            })
        })
        .collect::<Vec<_>>();
    discovered.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    discovered
        .into_iter()
        .map(|(_, executable)| executable)
        .collect()
}

fn local_codex_candidates_from(
    override_candidate: Option<PathBuf>,
    local_app_data: Option<&Path>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(binary) = override_candidate {
        push_unique_codex_candidate(&mut candidates, binary);
    }
    #[cfg(windows)]
    if let Some(local) = local_app_data {
        // The content-addressed runtime belongs to the currently installed
        // Desktop app. Prefer it over the old standalone CLI path: otherwise
        // an abandoned 0.x CLI can incorrectly gate features that the active
        // Desktop runtime already supports.
        for executable in desktop_runtime_codex_candidates(local) {
            push_unique_codex_candidate(&mut candidates, executable);
        }
        push_unique_codex_candidate(
            &mut candidates,
            local.join("Programs/OpenAI/Codex/bin/codex.exe"),
        );
    }
    #[cfg(not(windows))]
    let _ = local_app_data;
    // A macOS GUI app inherits launchd's PATH, which has neither Homebrew nor
    // Codex on it, so a bare `codex` fails with "No such file or directory"
    // even with Codex Desktop installed. Only existing files are probed;
    // `codex_not_found_detail` names the rest when none of them is there.
    #[cfg(target_os = "macos")]
    for executable in crate::install_paths::macos_codex_cli_candidates(dirs::home_dir().as_deref())
    {
        if executable.is_file() {
            push_unique_codex_candidate(&mut candidates, executable);
        }
    }
    push_unique_codex_candidate(&mut candidates, PathBuf::from("codex"));
    candidates
}

/// Every place a Codex CLI was looked for on this platform, existing or not,
/// for a message that has to say why none was found.
fn codex_search_locations() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        crate::install_paths::macos_codex_cli_candidates(dirs::home_dir().as_deref())
    }
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|local| {
                vec![
                    local.join("OpenAI/Codex/bin/<version>/codex.exe"),
                    local.join("Programs/OpenAI/Codex/bin/codex.exe"),
                ]
            })
            .unwrap_or_default()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Vec::new()
    }
}

/// "No such file or directory" names neither the file nor the fix.
fn codex_not_found_detail() -> String {
    let mut searched = codex_search_locations()
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    searched.push("PATH 上的 codex".into());
    format!(
        "找不到 Codex CLI；請確認已安裝 Codex Desktop，或以 VELLUM_CODEX_BIN 指定路徑。已找過：{}",
        searched.join("、")
    )
}

pub fn local_codex_candidates() -> Vec<PathBuf> {
    local_codex_candidates_from(
        std::env::var_os("VELLUM_CODEX_BIN").map(PathBuf::from),
        std::env::var_os("LOCALAPPDATA").as_deref().map(Path::new),
    )
}

pub fn native_subagent_capability() -> crate::model::SubagentCapability {
    let minimum = parse_codex_version(MIN_NATIVE_SUBAGENT_DEFAULTS_VERSION)
        .expect("native sub-agent minimum version is valid");
    let mut failures = Vec::new();
    for candidate in local_codex_candidates() {
        match crate::process::background_command(&candidate)
            .arg("--version")
            .output()
        {
            Ok(output) if output.status.success() => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let reported = format!("{stdout} {stderr}");
                let Some(version) = parse_codex_version(&reported) else {
                    failures.push(format!(
                        "{} returned an unrecognized version",
                        candidate.display()
                    ));
                    continue;
                };
                let version_text = format!("{}.{}.{}", version.0, version.1, version.2);
                let supported = version >= minimum;
                let desktop_version = installed_desktop_app_version();
                return crate::model::SubagentCapability {
                    supported,
                    desktop_version: desktop_version.clone(),
                    runtime_version: Some(version_text.clone()),
                    detail: if supported {
                        match desktop_version {
                            Some(desktop) => format!(
                                "Codex Desktop {desktop} runtime supports native sub-agent model defaults"
                            ),
                            None => "Codex Desktop runtime supports native sub-agent model defaults"
                                .into(),
                        }
                    } else {
                        format!(
                            "Codex Desktop runtime {version_text} does not support native sub-agent model defaults; update the Desktop app (runtime {} or newer is required)",
                            MIN_NATIVE_SUBAGENT_DEFAULTS_VERSION
                        )
                    },
                };
            }
            Ok(output) => failures.push(format!(
                "{} --version exited with {}",
                candidate.display(),
                output.status
            )),
            Err(error) => failures.push(format!("{}: {error}", candidate.display())),
        }
    }
    crate::model::SubagentCapability {
        supported: false,
        desktop_version: installed_desktop_app_version(),
        runtime_version: None,
        detail: format!(
            "Codex Desktop runtime was not found or could not be queried ({})",
            failures.join("; ")
        ),
    }
}

#[cfg(target_os = "windows")]
fn installed_desktop_app_version() -> Option<String> {
    let registry = dirs::data_local_dir()?
        .join("OpenAI")
        .join("Codex")
        .join("chrome-native-hosts-v2.json");
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(registry).ok()?).ok()?;
    value
        .get("entries")?
        .as_array()?
        .iter()
        .filter(|entry| {
            entry
                .pointer("/paths/resourcesPath")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|path| Path::new(path).is_dir())
        })
        .max_by_key(|entry| {
            entry
                .get("updatedAt")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
        })
        .and_then(|entry| entry.get("appVersion"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

#[cfg(target_os = "macos")]
fn installed_desktop_app_version() -> Option<String> {
    crate::install_paths::macos_codex_app_candidates(dirs::home_dir().as_deref())
        .into_iter()
        .map(|app| app.join("Contents").join("Info.plist"))
        .find_map(|plist| {
            if !plist.is_file() {
                return None;
            }
            crate::process::background_command("plutil")
                .args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
                .arg(&plist)
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn installed_desktop_app_version() -> Option<String> {
    None
}

pub fn ensure_native_subagent_supported(
    settings: &crate::model::SubagentSettings,
) -> AppResult<()> {
    if settings.mode == crate::model::SubagentMode::Inherit {
        return Ok(());
    }
    let capability = native_subagent_capability();
    if capability.supported {
        Ok(())
    } else {
        Err(AppError::Message(format!(
            "nativeSubagentUnsupported: {}",
            capability.detail
        )))
    }
}

/// Whether the installed Codex accepts the custom-provider shape Vellum needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VellumProviderCapability {
    pub supported: bool,
    pub codex_version: Option<String>,
    pub detail: String,
}

/// Ask the installed Codex to load a config containing exactly the provider
/// table Vellum is about to write.
///
/// This runs *before* the user's own config is touched. The proxy will refuse
/// every request that arrives without the boundary key, so a Codex that cannot
/// carry the header would be left unable to talk to Vellum at all — writing the
/// config first and finding out afterwards would strand the user with a broken
/// setup and no obvious way back.
///
/// The probe uses a throwaway `CODEX_HOME`, so a rejection costs the user
/// nothing. It proves the config parses; it cannot prove the header is actually
/// transmitted, which is what the authenticated readiness check after startup
/// is for.
pub fn vellum_provider_capability(proxy_base_url: &str) -> VellumProviderCapability {
    let probe_home = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(error) => {
            return VellumProviderCapability {
                supported: false,
                codex_version: None,
                detail: format!("無法建立協議探測目錄：{error}"),
            };
        }
    };
    let mut document = DocumentMut::new();
    document["model_provider"] = value(VELLUM_PROVIDER_NAME);
    // A placeholder key: the probe is about the config shape, and the real key
    // must not be written anywhere outside the user's protected config.
    if let Err(error) = write_vellum_provider(&mut document, proxy_base_url, &"0".repeat(64)) {
        return VellumProviderCapability {
            supported: false,
            codex_version: None,
            detail: error.to_string(),
        };
    }
    if let Err(error) = std::fs::write(probe_home.path().join("config.toml"), document.to_string())
    {
        return VellumProviderCapability {
            supported: false,
            codex_version: None,
            detail: format!("無法寫入協議探測設定：{error}"),
        };
    }

    let mut failures = Vec::new();
    // Whether any candidate exists at all; "not found" everywhere is reported
    // as a missing install rather than as a protocol problem.
    let mut found = false;
    for candidate in local_codex_candidates() {
        match crate::process::background_command(&candidate)
            .env("CODEX_HOME", probe_home.path())
            .arg("--version")
            .output()
        {
            Ok(output) if output.status.success() => {
                let reported = format!(
                    "{} {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                let version = parse_codex_version(&reported)
                    .map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"));
                return VellumProviderCapability {
                    supported: true,
                    codex_version: version.clone(),
                    detail: match version {
                        Some(version) => {
                            format!("Codex {version} 接受 Vellum custom provider 設定")
                        }
                        None => "Codex 接受 Vellum custom provider 設定".into(),
                    },
                };
            }
            Ok(output) => {
                found = true;
                failures.push(format!(
                    "{} 以 {} 結束：{}",
                    candidate.display(),
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ))
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                found = true;
                failures.push(format!("{}：{error}", candidate.display()))
            }
            Err(_) => {}
        }
    }
    VellumProviderCapability {
        supported: false,
        codex_version: None,
        detail: if found {
            failures.join("; ")
        } else {
            format!("{CODEX_NOT_FOUND}: {}", codex_not_found_detail())
        },
    }
}

/// Prefix of a capability detail saying no Codex CLI could be started at all,
/// which is a different problem from one that ran and refused the provider.
const CODEX_NOT_FOUND: &str = "CodexDesktopNotFound";

/// Fail closed when the installed Codex cannot carry the boundary key.
pub fn ensure_vellum_provider_supported(proxy_base_url: &str) -> AppResult<()> {
    let capability = vellum_provider_capability(proxy_base_url);
    if capability.supported {
        Ok(())
    } else if capability.detail.starts_with(CODEX_NOT_FOUND) {
        Err(AppError::Message(capability.detail))
    } else {
        Err(AppError::Message(format!(
            "CodexDesktopProtocolMismatch: {}",
            capability.detail
        )))
    }
}

/// Codex Desktop 維護的輕量聊天室索引。只讀名稱與 ID，不碰對話內容。
/// 回傳 key 使用與 history.rs 相同的 SHA256，因而不用把原始 thread ID
/// 寫入 Vellum 的工作階段資料庫。
pub fn read_session_labels() -> HashMap<String, String> {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .unwrap_or_else(|| std::env::temp_dir().join(".codex"));
    read_session_labels_from_codex_home(&codex_home)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexSessionRuntime {
    pub label: Option<String>,
    /// Model recorded by Codex for the latest turn in this rollout. This is
    /// the session's actual selection; the route's current default may have
    /// changed since the turn ran.
    pub model: Option<String>,
    pub used_tokens: Option<u64>,
    pub window_tokens: Option<u64>,
    pub(crate) rollout_modified_at: Option<std::time::SystemTime>,
}

/// A client-side compaction observed in Codex's rollout. Codex performs some
/// automatic compactions locally, outside the proxied Responses exchange, so
/// these events cannot be reconstructed from Vellum's provider journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexCompactionEvent {
    pub created_at: i64,
    pub event_id: i64,
    /// The Codex thread this compaction happened in, as the rollout file
    /// names it — a bare uuid. It is deliberately *not* a conversation key:
    /// the same conversation is called three different things depending on
    /// who is naming it, so binding an event to a session goes through
    /// [`conversation_key_matches`] rather than string equality.
    pub thread_id: Option<String>,
    pub label: Option<String>,
    pub tokens_before: Option<u64>,
    pub tokens_after: Option<u64>,
    /// Verbatim replacement summary persisted by current Codex rollouts.
    /// Official compactions leave this empty and persist only opaque state.
    pub replacement_text: Option<String>,
}

#[derive(Debug, Default)]
struct RolloutScan {
    length: u64,
    trailing: Vec<u8>,
    last_tokens: Option<u64>,
    pending_event: Option<usize>,
    events: Vec<CodexCompactionEvent>,
}

#[derive(Debug, Default)]
struct CompactionScanCache {
    files: HashMap<PathBuf, RolloutScan>,
}

static COMPACTION_SCAN_CACHE: OnceLock<Mutex<CompactionScanCache>> = OnceLock::new();

/// Read recent Codex client compactions without repeatedly walking rollout
/// contents. The first visit is bounded to the tail of the 16 most recently
/// modified rollouts; later visits read only bytes appended to those files.
pub fn read_recent_compaction_events(limit: usize) -> Vec<CodexCompactionEvent> {
    const MAX_ROLLOUTS: usize = 16;
    const INITIAL_TAIL_BYTES: u64 = 8 * 1024 * 1024;

    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .unwrap_or_else(|| std::env::temp_dir().join(".codex"));
    let labels = read_session_labels_from_codex_home(&codex_home);
    let mut candidates = Vec::new();
    for root in [
        codex_home.join("sessions"),
        codex_home.join("archived_sessions"),
    ] {
        collect_rollout_files(&root, &mut candidates);
    }
    candidates.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    candidates.truncate(MAX_ROLLOUTS);

    let cache = COMPACTION_SCAN_CACHE.get_or_init(|| Mutex::new(CompactionScanCache::default()));
    let Ok(mut cache) = cache.lock() else {
        return Vec::new();
    };
    for (path, _) in &candidates {
        let Ok(mut file) = std::fs::File::open(path) else {
            continue;
        };
        let Ok(length) = file.metadata().map(|metadata| metadata.len()) else {
            continue;
        };
        let scan = cache.files.entry(path.clone()).or_default();
        if length < scan.length {
            *scan = RolloutScan::default();
        }
        let first_read = scan.length == 0;
        let start = if first_read {
            length.saturating_sub(INITIAL_TAIL_BYTES)
        } else {
            scan.length
        };
        if length == start || file.seek(SeekFrom::Start(start)).is_err() {
            continue;
        }
        let mut appended = Vec::with_capacity((length - start).min(INITIAL_TAIL_BYTES) as usize);
        if file.read_to_end(&mut appended).is_err() {
            continue;
        }
        scan.length = length;
        if first_read && start > 0 {
            if let Some(newline) = appended.iter().position(|byte| *byte == b'\n') {
                appended.drain(..=newline);
            } else {
                continue;
            }
        }
        scan.trailing.extend_from_slice(&appended);
        let complete_len = scan
            .trailing
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        if complete_len == 0 {
            continue;
        }
        let complete = scan.trailing.drain(..complete_len).collect::<Vec<_>>();
        let thread_id = thread_id_from_rollout_path(path);
        let label = thread_id.as_ref().and_then(|id| labels.get(id)).cloned();
        parse_compaction_lines(&complete, thread_id, label, scan);
    }

    let mut events = candidates
        .iter()
        .filter_map(|(path, _)| cache.files.get(path))
        .flat_map(|scan| scan.events.iter().cloned())
        .collect::<Vec<_>>();
    events.sort_by_key(|event| std::cmp::Reverse((event.created_at, event.event_id)));
    events.truncate(limit.max(1));
    events
}

fn collect_rollout_files(directory: &Path, result: &mut Vec<(PathBuf, SystemTime)>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollout_files(&path, result);
        } else if thread_id_from_rollout_path(&path).is_some() {
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            result.push((path, modified));
        }
    }
}

fn parse_compaction_lines(
    bytes: &[u8],
    thread_id: Option<String>,
    label: Option<String>,
    scan: &mut RolloutScan,
) {
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        let timestamp_ms = value
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .and_then(|timestamp| chrono::DateTime::parse_from_rfc3339(timestamp).ok())
            .map(|timestamp| timestamp.timestamp_millis());
        let kind = value.get("type").and_then(serde_json::Value::as_str);
        let payload_kind = value
            .pointer("/payload/type")
            .and_then(serde_json::Value::as_str);

        if payload_kind == Some("token_count") {
            let tokens = value
                .pointer("/payload/info/last_token_usage/total_tokens")
                .and_then(serde_json::Value::as_u64)
                .filter(|tokens| *tokens > 0);
            if let Some(tokens) = tokens {
                if let Some(index) = scan.pending_event.take() {
                    if let Some(event) = scan.events.get_mut(index) {
                        event.tokens_after = Some(tokens);
                    }
                }
                scan.last_tokens = Some(tokens);
            }
            continue;
        }

        if kind != Some("compacted") && payload_kind != Some("context_compacted") {
            continue;
        }
        let Some(timestamp_ms) = timestamp_ms else {
            continue;
        };
        let duplicate = scan
            .events
            .last()
            .is_some_and(|event| (timestamp_ms / 1000 - event.created_at).abs() <= 2);
        if duplicate {
            if scan
                .events
                .last()
                .is_some_and(|event| event.replacement_text.is_none())
            {
                scan.events.last_mut().unwrap().replacement_text = value
                    .pointer("/payload/message")
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                    .map(str::to_string);
            }
            continue;
        }
        scan.events.push(CodexCompactionEvent {
            created_at: timestamp_ms / 1000,
            event_id: timestamp_ms,
            thread_id: thread_id.clone(),
            label: label.clone(),
            tokens_before: scan.last_tokens,
            tokens_after: None,
            replacement_text: value
                .pointer("/payload/message")
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .map(str::to_string),
        });
        scan.pending_event = Some(scan.events.len() - 1);
    }
}

pub fn read_session_runtime(
    conversation_keys: &HashSet<String>,
) -> HashMap<String, CodexSessionRuntime> {
    if conversation_keys.is_empty() {
        return HashMap::new();
    }
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .unwrap_or_else(|| std::env::temp_dir().join(".codex"));
    let labels = read_session_labels_from_codex_home(&codex_home);
    let mut result = conversation_keys
        .iter()
        .map(|key| {
            (
                key.clone(),
                CodexSessionRuntime {
                    label: conversation_key_aliases(key)
                        .into_iter()
                        .find_map(|alias| labels.get(&alias).cloned()),
                    ..CodexSessionRuntime::default()
                },
            )
        })
        .collect::<HashMap<_, _>>();
    for root in [
        codex_home.join("sessions"),
        codex_home.join("archived_sessions"),
    ] {
        visit_session_files(&root, conversation_keys, &mut result);
    }
    result
}

fn visit_session_files(
    directory: &Path,
    wanted: &HashSet<String>,
    result: &mut HashMap<String, CodexSessionRuntime>,
) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_session_files(&path, wanted, result);
            continue;
        }
        let Some(thread_id) = thread_id_from_rollout_path(&path) else {
            continue;
        };
        // A rollout file knows only its own uuid, so the wanted key has to be
        // found from that end — see `conversation_key_matches`.
        let Some(key) = wanted
            .iter()
            .find(|key| conversation_key_matches(key, &thread_id))
            .cloned()
        else {
            continue;
        };
        let runtime = result.entry(key).or_default();
        if runtime.label.is_none() {
            runtime.label = first_meaningful_user_message(&path);
        }
        let modified_at = path.metadata().and_then(|meta| meta.modified()).ok();
        let is_newer_rollout = match (modified_at, runtime.rollout_modified_at) {
            (Some(candidate), Some(current)) => candidate >= current,
            (Some(_), None) | (None, None) => true,
            (None, Some(_)) => false,
        };
        if is_newer_rollout {
            runtime.model = latest_turn_model(&path);
            if let Some((used, window)) = latest_token_count(&path) {
                runtime.used_tokens = Some(used);
                runtime.window_tokens = Some(window);
            }
            runtime.rollout_modified_at = modified_at;
        }
    }
}

/// Every name one conversation answers to on this machine.
///
/// Two generations of identity meet in the Context screen. Vellum's own
/// history hashes the upstream identifier, so its keys are bare sha256 hex.
/// The proxy runtime instead records the identifier Codex itself sends,
/// `codex:<session>:<thread>`, verbatim — and that is what a live install is
/// full of. The rollout files and `session_index.jsonl` on disk know neither
/// form; they are keyed by the bare uuid.
///
/// Matching on one form only is what left every conversation unnamed: the
/// label index was looked up by sha256 while the keys coming in were
/// `codex:…`, so no title ever resolved and every row in the list read
/// `#codex` — the first six characters of a prefix they all share.
///
/// Returns the key itself first, then the uuid segments, so a caller that
/// stops at the first hit prefers the exact name it was given.
pub fn conversation_key_aliases(key: &str) -> Vec<String> {
    let key = key.trim();
    if key.is_empty() {
        return Vec::new();
    }
    let mut aliases = vec![key.to_ascii_lowercase()];
    for segment in key.split(':').skip(1) {
        let segment = segment.trim().to_ascii_lowercase();
        if !segment.is_empty() && !aliases.contains(&segment) {
            aliases.push(segment);
        }
    }
    aliases
}

/// Does `candidate` — a bare thread uuid read off disk — name the same
/// conversation as `key`?
///
/// The candidate is also compared in hashed form, because a key written
/// before the runtime started sending `codex:…` identities *is* the hash.
pub fn conversation_key_matches(key: &str, candidate: &str) -> bool {
    let candidate = candidate.trim().to_ascii_lowercase();
    if candidate.is_empty() {
        return false;
    }
    let hashed = format!("{:x}", Sha256::digest(candidate.as_bytes()));
    conversation_key_aliases(key)
        .into_iter()
        .any(|alias| alias == candidate || alias == hashed)
}

fn thread_id_from_rollout_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let candidate = stem.get(stem.len().checked_sub(36)?..)?;
    let bytes = candidate.as_bytes();
    if bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes.get(index) == Some(&b'-'))
    {
        Some(candidate.to_ascii_lowercase())
    } else {
        None
    }
}

fn latest_token_count(path: &Path) -> Option<(u64, u64)> {
    const TAIL_BYTES: u64 = 2 * 1024 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut bytes = Vec::with_capacity(length.min(TAIL_BYTES) as usize);
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    text.lines().rev().find_map(|line| {
        if !line.contains("\"type\":\"token_count\"") {
            return None;
        }
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        let used = value
            .pointer("/payload/info/last_token_usage/total_tokens")
            .and_then(serde_json::Value::as_u64)?;
        let window = value
            .pointer("/payload/info/model_context_window")
            .and_then(serde_json::Value::as_u64)?;
        (used > 0 && window > 0).then_some((used, window))
    })
}

/// Read the model from the latest `turn_context` in a rollout. Context rows
/// must use this per-session fact instead of the route's mutable default.
fn latest_turn_model(path: &Path) -> Option<String> {
    const TAIL_BYTES: u64 = 2 * 1024 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut bytes = Vec::with_capacity(length.min(TAIL_BYTES) as usize);
    file.read_to_end(&mut bytes).ok()?;
    String::from_utf8_lossy(&bytes)
        .lines()
        .rev()
        .find_map(|line| {
            if !line.contains("\"type\":\"turn_context\"") {
                return None;
            }
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            value
                .pointer("/payload/model")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(str::to_string)
        })
}

fn first_meaningful_user_message(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .take(5_000)
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(&line).ok()?;
            let message = if value
                .pointer("/payload/type")
                .and_then(serde_json::Value::as_str)
                == Some("user_message")
            {
                value.pointer("/payload/message")?.as_str()?.to_string()
            } else if value.get("type").and_then(serde_json::Value::as_str) == Some("response_item")
                && value
                    .pointer("/payload/type")
                    .and_then(serde_json::Value::as_str)
                    == Some("message")
                && value
                    .pointer("/payload/role")
                    .and_then(serde_json::Value::as_str)
                    == Some("user")
            {
                value
                    .pointer("/payload/content")?
                    .as_array()?
                    .iter()
                    .filter(|item| {
                        item.get("type").and_then(serde_json::Value::as_str) == Some("input_text")
                    })
                    .filter_map(|item| item.get("text").and_then(serde_json::Value::as_str))
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                return None;
            };
            let message = message.trim();
            let compact = message.split_whitespace().collect::<Vec<_>>().join(" ");
            let lower = compact.to_ascii_lowercase();
            (!compact.is_empty()
                && !matches!(lower.as_str(), "hi" | "hello" | "嗨" | "你好")
                && compact.chars().count() >= 6)
                .then_some(compact)
        })
        .next()
}

fn insert_session_label(labels: &mut HashMap<String, String>, id: &str, title: &str) {
    let id = id.trim().to_ascii_lowercase();
    let title = title.trim();
    if id.is_empty() || title.is_empty() {
        return;
    }
    let hashed = format!("{:x}", Sha256::digest(id.as_bytes()));
    labels.insert(id, title.to_string());
    labels.insert(hashed, title.to_string());
}

fn read_session_labels_from_codex_home(codex_home: &Path) -> HashMap<String, String> {
    // `session_index.jsonl` is retained as a compatibility fallback, but
    // current Codex Desktop writes titles and user renames to `state_*.sqlite`.
    let mut labels = read_session_labels_from(&codex_home.join("session_index.jsonl"));
    let mut databases = std::fs::read_dir(codex_home)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            (name.starts_with("state_") && name.ends_with(".sqlite"))
                .then(|| (path, entry.metadata().and_then(|meta| meta.modified()).ok()))
        })
        .collect::<Vec<_>>();
    databases.sort_by_key(|(_, modified)| *modified);
    for (path, _) in databases {
        read_session_labels_from_sqlite(&path, &mut labels);
    }
    labels
}

fn read_session_labels_from_sqlite(path: &Path, labels: &mut HashMap<String, String>) {
    let Ok(connection) = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return;
    };
    // `name` is the explicit user-facing rename in recent schemas. Older
    // state databases only have `title`, so retry the compatible projection.
    let queries = [
        "SELECT id, COALESCE(NULLIF(TRIM(name), ''), NULLIF(TRIM(title), '')) FROM threads",
        "SELECT id, NULLIF(TRIM(title), '') FROM threads",
    ];
    for query in queries {
        let Ok(mut statement) = connection.prepare(query) else {
            continue;
        };
        let Ok(rows) = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        }) else {
            return;
        };
        for row in rows.flatten() {
            if let (id, Some(title)) = row {
                insert_session_label(labels, &id, &title);
            }
        }
        return;
    }
}

fn read_session_labels_from(path: &Path) -> HashMap<String, String> {
    let Ok(file) = std::fs::File::open(path) else {
        return HashMap::new();
    };
    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(&line).ok()?;
            let id = value.get("id")?.as_str()?.trim();
            let title = value.get("thread_name")?.as_str()?.trim();
            if id.is_empty() || title.is_empty() {
                return None;
            }
            Some((id.to_ascii_lowercase(), title.to_string()))
        })
        .fold(HashMap::new(), |mut labels, (id, title)| {
            insert_session_label(&mut labels, &id, &title);
            labels
        })
}

/// The reserved provider name Vellum manages in the user's Codex config.
pub const VELLUM_PROVIDER_NAME: &str = "vellum";
/// The second reserved provider name, used only by Official models.
///
/// Official and third-party models reach the same proxy listener but are not
/// the same kind of provider to Codex, and one table cannot be both. See
/// [`write_vellum_official_provider`].
pub const VELLUM_OFFICIAL_PROVIDER_NAME: &str = "vellum-official";
const OPENAI_THREAD_RECOVERY_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Write the `[model_providers.vellum]` table Codex routes through.
///
/// Vellum no longer just points `openai_base_url` at the proxy. The proxy now
/// requires a boundary key on every request, and `openai_base_url` has nowhere
/// to carry one — a custom provider does, via `http_headers`.
///
/// `requires_openai_auth` keeps ChatGPT OAuth working and
/// `supports_websockets` keeps the native Official WebSocket session, so this
/// stays a transport change and not a downgrade of either.
///
/// The provider display name deliberately remains non-OpenAI. Bundled Codex
/// reads `ModelProviderInfo::is_openai()` -- a comparison against the literal
/// name `"OpenAI"` -- as the remote-compaction capability gate, and as the
/// gate on image generation, native web search, and history notes. None of
/// those hold for a third-party route, so this table must not claim the
/// OpenAI identity. Official models therefore do not use this table; they use
/// [`write_vellum_official_provider`], which does.
fn write_vellum_provider(
    document: &mut DocumentMut,
    proxy_base_url: &str,
    boundary_key: &str,
) -> AppResult<()> {
    if !document
        .get("model_providers")
        .is_some_and(|item| item.is_table())
    {
        document["model_providers"] = Item::Table(Table::new());
    }
    let providers = document["model_providers"]
        .as_table_mut()
        .ok_or_else(|| AppError::Message("model_providers 不是資料表".into()))?;
    providers.set_implicit(true);

    let mut provider = Table::new();
    provider["name"] = value("Vellum");
    provider["base_url"] = value(proxy_base_url.trim_end_matches('/'));
    provider["wire_api"] = value("responses");
    provider["requires_openai_auth"] = value(true);
    provider["supports_websockets"] = value(true);

    let mut headers = Table::new();
    headers[vellum_proxy_runtime::BOUNDARY_KEY_HEADER] = value(boundary_key);
    provider["http_headers"] = Item::Table(headers);

    providers.insert(VELLUM_PROVIDER_NAME, Item::Table(provider));
    Ok(())
}

/// Write the `[model_providers.vellum-official]` table Official models route
/// through.
///
/// Official models are OpenAI models. They reach the same proxy listener as
/// every other route -- that is how Vellum applies the ChatGPT account the
/// user selected, and how Official turns stay visible to usage, quota, and the
/// log -- but to Codex they are not a third-party provider, and one table
/// cannot say both things: `is_openai()` compares the display name against the
/// literal `"OpenAI"`, so a single name would decide remote compaction, image
/// generation, native web search, and history notes for every model sharing
/// the table.
///
/// Splitting the tables lets each answer be true. This one is named `"OpenAI"`
/// because the model on the other side of the hop is OpenAI's; `vellum` stays
/// non-OpenAI because its models are not. The base URL is still the local
/// listener, so `supports_codex_backend_routes()` -- which additionally
/// requires a `/backend-api/codex` base URL -- stays false, and Codex does not
/// send ChatGPT backend routes to a gateway that has none.
///
/// The Enhanced model map binds Official catalog slugs to this table by name;
/// see `TrustedModelProviderMap::from_catalog`.
fn write_vellum_official_provider(
    document: &mut DocumentMut,
    proxy_base_url: &str,
    boundary_key: &str,
) -> AppResult<()> {
    if !document
        .get("model_providers")
        .is_some_and(|item| item.is_table())
    {
        document["model_providers"] = Item::Table(Table::new());
    }
    let providers = document["model_providers"]
        .as_table_mut()
        .ok_or_else(|| AppError::Message("model_providers 不是資料表".into()))?;
    providers.set_implicit(true);

    let mut provider = Table::new();
    provider["name"] = value("OpenAI");
    provider["base_url"] = value(proxy_base_url.trim_end_matches('/'));
    provider["wire_api"] = value("responses");
    provider["requires_openai_auth"] = value(true);
    provider["supports_websockets"] = value(true);

    let mut headers = Table::new();
    headers[vellum_proxy_runtime::BOUNDARY_KEY_HEADER] = value(boundary_key);
    provider["http_headers"] = Item::Table(headers);

    providers.insert(VELLUM_OFFICIAL_PROVIDER_NAME, Item::Table(provider));
    Ok(())
}

/// The three global `model_*` overrides bundled Codex applies on top of the
/// active model's catalog entry, superseding it: `model_context_window`
/// overrides the catalog's `context_window`, and `model_auto_compact_token_limit`
/// (with `model_auto_compact_token_limit_scope`) overrides the catalog's
/// `auto_compact_token_limit`. Vellum's per-model catalog contract only holds
/// if none of these three are set, so Vellum clears them unconditionally while
/// it runs rather than setting a global value that would replace every model's
/// own context window and runtime-owned compact threshold.
const GLOBAL_COMPACTION_OVERRIDE_KEYS: &[&str] = &[
    "model_context_window",
    "model_auto_compact_token_limit",
    "model_auto_compact_token_limit_scope",
];

fn clear_conflicting_global_compaction_keys(document: &mut DocumentMut) {
    let table = document.as_table_mut();
    for key in GLOBAL_COMPACTION_OVERRIDE_KEYS {
        table.remove(key);
    }
}

/// Keep rollout files recorded with a Vellum provider id loadable after the
/// local proxy is stopped. Codex Desktop resolves the provider id stored in
/// `session_meta` before it lets the user change models; removing the provider
/// table therefore makes an otherwise intact thread impossible to open.
///
/// Both reserved names get an alias, because both appear in `session_meta`:
/// third-party threads record `vellum` and Official threads record
/// `vellum-official`. Each alias carries no Vellum boundary credential and
/// points directly at the native ChatGPT Codex endpoint. The Official alias
/// keeps the `"OpenAI"` display name it had while leased, so a recovered
/// Official thread keeps the capabilities it was written with. The top-level
/// provider is still restored to the user's original value, so only
/// Vellum-authored threads select these entries.
fn write_openai_thread_recovery_provider(
    document: &mut DocumentMut,
    reserved_ids: &[&str],
) -> AppResult<()> {
    if reserved_ids.is_empty() {
        return Ok(());
    }
    if !document
        .get("model_providers")
        .is_some_and(|item| item.is_table())
    {
        document["model_providers"] = Item::Table(Table::new());
    }
    let providers = document["model_providers"]
        .as_table_mut()
        .ok_or_else(|| AppError::Message("model_providers 不是資料表".into()))?;
    providers.set_implicit(true);

    for (id, name) in [
        (
            VELLUM_PROVIDER_NAME,
            "Vellum thread recovery (OpenAI direct)",
        ),
        (VELLUM_OFFICIAL_PROVIDER_NAME, "OpenAI"),
    ]
    .into_iter()
    .filter(|(id, _)| reserved_ids.contains(id))
    {
        let mut provider = Table::new();
        provider["name"] = value(name);
        provider["base_url"] = value(OPENAI_THREAD_RECOVERY_BASE_URL);
        provider["wire_api"] = value("responses");
        provider["requires_openai_auth"] = value(true);
        provider["supports_websockets"] = value(true);
        provider["supports_standalone_web_search"] = value(true);
        providers.insert(id, Item::Table(provider));
    }
    Ok(())
}

/// Remove from a *baseline* candidate everything Vellum can prove it wrote
/// itself. Returns whether anything was removed.
///
/// The baseline is the config Codex had before Vellum touched it, and restore
/// writes it back verbatim. `apply_proxy_config` derives it from the lease when
/// one exists and otherwise from the config file as it stands — and that second
/// path is how a Vellum value gets laundered into a user setting. A restore
/// only reverts a key when `current == applied`, so a partial restore (or a
/// crash before restore) leaves Vellum's provider tables and sub-agent default
/// behind; the next `apply` finds no lease, snapshots that file, and from then
/// on believes Codex always looked like this.
///
/// Two costs, both observed. "Keep Desktop's existing settings" stops meaning
/// Codex's own default and starts meaning whichever third-party model Vellum
/// last pinned. And uninstalling *writes that value back*, leaving a Codex
/// config naming a model id that only existed inside Vellum.
///
/// Only provable evidence is scrubbed: a provider table Vellum defines, a
/// pointer at Vellum's own listener or catalog file, or a `vlm-`-prefixed
/// catalog id, which `catalog.rs` generates and no user can have typed.
fn scrub_vellum_from_baseline(
    document: &mut DocumentMut,
    paths: &CodexPaths,
    proxy_base_url: &str,
) -> bool {
    let mut changed = false;
    let reserved = [VELLUM_PROVIDER_NAME, VELLUM_OFFICIAL_PROVIDER_NAME];

    if document
        .get("model_provider")
        .and_then(Item::as_str)
        .is_some_and(|provider| reserved.contains(&provider))
    {
        document.as_table_mut().remove("model_provider");
        changed = true;
    }
    if let Some(providers) = document
        .get_mut("model_providers")
        .and_then(Item::as_table_mut)
    {
        for id in reserved {
            if providers.remove(id).is_some() {
                changed = true;
            }
        }
        if providers.is_empty() {
            document.as_table_mut().remove("model_providers");
        }
    }
    let catalog = path_string(&paths.catalog).ok();
    for (key, vellum_value) in [
        ("model_catalog_json", catalog.as_deref()),
        ("openai_base_url", Some(proxy_base_url)),
    ] {
        let Some(vellum_value) = vellum_value else {
            continue;
        };
        if document
            .get(key)
            .and_then(Item::as_str)
            .is_some_and(|value| value == vellum_value)
        {
            document.as_table_mut().remove(key);
            changed = true;
        }
    }
    for key in MANAGED_AGENT_KEYS {
        if agent_key(document, key)
            .and_then(Item::as_str)
            .is_some_and(|value| value.starts_with("vlm-"))
        {
            remove_agent_key(document, key);
            changed = true;
        }
    }
    changed
}

pub fn apply_proxy_config(
    paths: &CodexPaths,
    proxy_base_url: &str,
    subagent: &crate::model::SubagentSettings,
    boundary_key: &str,
    port: u16,
) -> AppResult<()> {
    let current = read_config(&paths.config)?;
    let stored = read_stored_lease(paths)?;
    let original = stored
        .as_ref()
        .map(StoredLease::original_config)
        .unwrap_or(&current)
        .to_string();
    let mut original_doc = parse_config(&original)?;
    // Runs against the lease's stored baseline too, not only a fresh snapshot:
    // an install that already laundered one is repaired the next time Vellum
    // applies its config, and the corrected baseline is written back below.
    let baseline_repaired = scrub_vellum_from_baseline(&mut original_doc, paths, proxy_base_url);
    let original = if baseline_repaired {
        original_doc.to_string()
    } else {
        original
    };
    let mut document = parse_config(&current)?;

    // Before boundary authentication, Vellum also pointed the built-in OpenAI
    // provider at this listener through `openai_base_url`. That provider cannot
    // attach `x-vellum-boundary-key`, so persisted OpenAI threads reached the
    // proxy without the credential and failed with 401. Relinquish only the
    // exact value recorded in the prior lease; a user edit made while Vellum
    // was active remains protected by the same three-way rule used on restore.
    if let Some(previous_applied) = stored
        .as_ref()
        .map(StoredLease::applied_config)
        .map(parse_config)
        .transpose()?
    {
        relinquish_legacy_openai_base_url(&mut document, &original_doc, &previous_applied);
    }
    document["model_provider"] = value(VELLUM_PROVIDER_NAME);
    document["model_catalog_json"] = value(path_string(&paths.catalog)?);
    // `standalone_web_search` is a Codex feature flag under `[features]`, not
    // a top-level config key. Enabling it lets Codex register the native
    // `web.run` executor and send `/alpha/search` to Vellum. Vellum still
    // removes that tool from third-party model requests while its own master
    // switch is off, so a fresh install remains opt-in.
    set_feature(&mut document, "standalone_web_search", true);
    // Preserve Codex's native remote-compaction transport for Official models,
    // which Enhanced binds to the `vellum-official` provider. The proxy
    // forwards an Official `/responses/compact` upstream unchanged. The
    // non-OpenAI `vellum` provider does not satisfy the capability gate and
    // therefore uses native local compaction for third-party routes.
    set_feature(&mut document, "remote_compaction_v2", true);
    // `auto_compaction` is not a feature flag Codex knows. 0.153 answers
    // `features enable auto_compaction` with "Unknown feature flag", and
    // writing it under `[features]` therefore configures nothing. Scheduling
    // comes from the catalog instead: `remote_compaction_v2` above carries an
    // Official trigger to Vellum, and a third-party model that advertises a
    // `context_window` with no `auto_compact_token_limit` makes Codex derive
    // its own threshold and compact the thread locally, which is what the
    // Enhanced runtime is there to do.
    set_feature(&mut document, "enable_request_compression", false);
    set_feature(&mut document, "image_generation", false);
    clear_conflicting_global_compaction_keys(&mut document);
    remove_reserved_provider_tables(&mut document);
    write_vellum_provider(&mut document, proxy_base_url, boundary_key)?;
    write_vellum_official_provider(&mut document, proxy_base_url, boundary_key)?;
    apply_subagent_keys(&mut document, subagent, Some(&original_doc));
    let applied = document.to_string();

    // `reconcile_lease` runs before this, on every start, and clears out any
    // lease it could not confirm was still safe to hold -- so a lease still on
    // disk here has already been vetted (fresh, or a dead owner just
    // reclaimed by this process). Never overwritten: the `original_config` it
    // holds is the true pre-Vellum baseline, and a second `apply` in the same
    // session must not shift that baseline to Vellum's own prior output.
    if !paths.lease.exists() {
        write_json_atomic(
            &paths.lease,
            &ConfigLeaseV2 {
                schema_version: LEASE_SCHEMA_VERSION,
                owner: build_lease_owner(port),
                original_hash: hash_config(&original),
                applied_hash: hash_config(&applied),
                original_config: original,
                applied_config: applied.clone(),
                boundary_credential_id: vellum_proxy_runtime::BOUNDARY_CREDENTIAL_ID.into(),
                created_at: chrono::Utc::now().to_rfc3339(),
            },
        )?;
    } else if baseline_repaired {
        if let Some(stored) = read_stored_lease(paths)? {
            log::warn!(
                "[Codex] lease baseline carried Vellum's own writes; repaired so restore                  returns Codex to its real pre-Vellum config"
            );
            let repaired = match stored {
                StoredLease::V2(lease) => ConfigLeaseV2 {
                    original_hash: hash_config(&original),
                    applied_hash: hash_config(&applied),
                    original_config: original,
                    applied_config: applied.clone(),
                    ..*lease
                },
                StoredLease::Legacy { .. } => ConfigLeaseV2 {
                    schema_version: LEASE_SCHEMA_VERSION,
                    owner: build_lease_owner(port),
                    original_hash: hash_config(&original),
                    applied_hash: hash_config(&applied),
                    original_config: original,
                    applied_config: applied.clone(),
                    boundary_credential_id: vellum_proxy_runtime::BOUNDARY_CREDENTIAL_ID.into(),
                    created_at: chrono::Utc::now().to_rfc3339(),
                },
            };
            write_json_atomic(&paths.lease, &repaired)?;
        }
    }
    write_text_atomic(&paths.config, &applied)
}

fn relinquish_legacy_openai_base_url(
    current: &mut DocumentMut,
    original: &DocumentMut,
    previous_applied: &DocumentMut,
) {
    const KEY: &str = "openai_base_url";
    if !items_equal(current.get(KEY), previous_applied.get(KEY)) {
        return;
    }
    match original.get(KEY) {
        Some(item) => current[KEY] = item.clone(),
        None => {
            current.as_table_mut().remove(KEY);
        }
    }
}

/// Hot-update the `[agents]` sub-agent defaults in a managed Codex config.
///
/// No-op while Vellum has no active lease: the settings are only persisted in
/// Vellum and are applied by the next [`apply_proxy_config`]. When a lease is
/// active the live config and the lease's `applied_config` are both updated,
/// so a later restore still only reverts keys Vellum actually wrote.
pub fn apply_subagent_defaults(
    paths: &CodexPaths,
    subagent: &crate::model::SubagentSettings,
) -> AppResult<()> {
    let Some(stored) = read_stored_lease(paths)? else {
        return Ok(());
    };
    let original = parse_config(stored.original_config())?;
    let current = read_config(&paths.config)?;
    let mut document = parse_config(&current)?;
    apply_subagent_keys(&mut document, subagent, Some(&original));
    write_text_atomic(&paths.config, &document.to_string())?;

    let mut applied_doc = parse_config(stored.applied_config())?;
    apply_subagent_keys(&mut applied_doc, subagent, Some(&original));
    let applied = applied_doc.to_string();
    match stored {
        StoredLease::V2(mut lease) => {
            lease.applied_hash = hash_config(&applied);
            lease.applied_config = applied;
            write_json_atomic(&paths.lease, &lease)
        }
        StoredLease::Legacy {
            original_config, ..
        } => write_json_atomic(
            &paths.lease,
            &LegacyConfigLease {
                original_config,
                applied_config: applied,
            },
        ),
    }
}

pub fn restore_proxy_config(paths: &CodexPaths) -> AppResult<bool> {
    let Some(stored) = read_stored_lease(paths)? else {
        remove_managed_catalog(paths)?;
        return Ok(false);
    };
    let current = read_config(&paths.config)?;
    let mut current_doc = parse_config(&current)?;
    let original_doc = parse_config(stored.original_config())?;
    let applied_doc = parse_config(stored.applied_config())?;

    // Vellum-created rollouts persist the provider id in `session_meta`. Newer
    // Codex Desktop builds fail the entire thread load when that id no longer
    // exists, before the user can switch back to OpenAI. Replace only the exact
    // provider entry Vellum wrote; a pre-existing or user-edited `vellum`
    // provider remains protected by the normal three-way lease rules.
    let recoverable_providers = [VELLUM_PROVIDER_NAME, VELLUM_OFFICIAL_PROVIDER_NAME]
        .into_iter()
        .filter(|id| {
            model_provider_item(&original_doc, id).is_none()
                && items_equal(
                    model_provider_item(&current_doc, id),
                    model_provider_item(&applied_doc, id),
                )
        })
        .collect::<Vec<_>>();

    for key in MANAGED_KEYS {
        if items_equal(current_doc.get(key), applied_doc.get(key)) {
            match original_doc.get(key) {
                Some(item) => {
                    current_doc[key] = item.clone();
                }
                None => {
                    current_doc.as_table_mut().remove(key);
                }
            }
        }
    }
    for key in MANAGED_FEATURE_KEYS {
        if items_equal(
            feature_item(&current_doc, key),
            feature_item(&applied_doc, key),
        ) {
            match feature_item(&original_doc, key) {
                Some(item) => set_feature_item(&mut current_doc, key, item.clone()),
                None => remove_feature(&mut current_doc, key),
            }
        }
    }
    for key in MANAGED_AGENT_KEYS {
        if items_equal(agent_key(&current_doc, key), agent_key(&applied_doc, key)) {
            match agent_key(&original_doc, key) {
                Some(item) => set_agent_key(&mut current_doc, key, item.clone()),
                None => remove_agent_key(&mut current_doc, key),
            }
        }
    }
    write_openai_thread_recovery_provider(&mut current_doc, &recoverable_providers)?;
    write_text_atomic(&paths.config, &current_doc.to_string())?;
    std::fs::remove_file(&paths.lease)
        .map_err(|error| AppError::Message(format!("無法移除 Codex lease：{error}")))?;
    remove_managed_catalog(paths)?;
    Ok(true)
}

fn remove_managed_catalog(paths: &CodexPaths) -> AppResult<()> {
    match std::fs::remove_file(&paths.catalog) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::Message(format!(
            "無法移除 Vellum 模型型錄：{error}"
        ))),
    }
}

pub fn has_active_lease(paths: &CodexPaths) -> bool {
    paths.lease.exists()
}

fn read_config(path: &Path) -> AppResult<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(AppError::Message(format!(
            "無法讀取 Codex config.toml：{error}"
        ))),
    }
}

fn parse_config(text: &str) -> AppResult<DocumentMut> {
    text.parse::<DocumentMut>()
        .map_err(|error| AppError::Message(format!("Codex config.toml 格式錯誤：{error}")))
}

fn remove_reserved_provider_tables(document: &mut DocumentMut) {
    let Some(providers) = document
        .get_mut("model_providers")
        .and_then(Item::as_table_mut)
    else {
        return;
    };
    for reserved in [
        "amazon-bedrock",
        "openai",
        "ollama",
        "lmstudio",
        "oss",
        "ollama-chat",
    ] {
        providers.remove(reserved);
    }
    if providers.is_empty() {
        document.as_table_mut().remove("model_providers");
    }
}

fn items_equal(left: Option<&Item>, right: Option<&Item>) -> bool {
    match (left, right) {
        (None, None) => true,
        // `model_providers` is normally an implicit parent table. Rendering an
        // implicit table item by itself can produce an empty string even when
        // its child provider tables differ, so compare each item inside a
        // temporary explicit parent that serializes the complete subtree.
        (Some(left), Some(right)) if left.is_table() || right.is_table() => {
            comparable_item(left) == comparable_item(right)
        }
        (Some(left), Some(right)) => left.to_string() == right.to_string(),
        _ => false,
    }
}

fn comparable_item(item: &Item) -> String {
    let mut item = item.clone();
    if let Some(table) = item.as_table_mut() {
        table.set_implicit(false);
    }
    let mut document = DocumentMut::new();
    document["managed"] = item;
    document.to_string()
}

fn model_provider_item<'a>(document: &'a DocumentMut, id: &str) -> Option<&'a Item> {
    document
        .get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(id))
}

fn feature_item<'a>(document: &'a DocumentMut, key: &str) -> Option<&'a Item> {
    document
        .get("features")
        .and_then(Item::as_table)
        .and_then(|features| features.get(key))
}

fn ensure_features(document: &mut DocumentMut) -> &mut Table {
    if !document.get("features").is_some_and(|item| item.is_table()) {
        document["features"] = Item::Table(Table::new());
    }
    document["features"]
        .as_table_mut()
        .expect("features was just materialized as a table")
}

fn set_feature(document: &mut DocumentMut, key: &str, enabled: bool) {
    ensure_features(document).insert(key, value(enabled));
}

fn set_feature_item(document: &mut DocumentMut, key: &str, item: Item) {
    ensure_features(document).insert(key, item);
}

fn remove_feature(document: &mut DocumentMut, key: &str) {
    let remove_table = document
        .get_mut("features")
        .and_then(Item::as_table_mut)
        .map(|features| {
            features.remove(key);
            features.is_empty()
        })
        .unwrap_or(false);
    if remove_table {
        document.as_table_mut().remove("features");
    }
}

fn agent_key<'a>(document: &'a DocumentMut, key: &str) -> Option<&'a Item> {
    document
        .get("agents")
        .and_then(Item::as_table)
        .and_then(|agents| agents.get(key))
}

fn set_agent_key(document: &mut DocumentMut, key: &str, item: Item) {
    if !document.get("agents").is_some_and(|item| item.is_table()) {
        document["agents"] = Item::Table(Table::new());
    }
    document["agents"]
        .as_table_mut()
        .expect("agents was just materialized as a table")
        .insert(key, item);
}

fn remove_agent_key(document: &mut DocumentMut, key: &str) {
    let remove_table = document
        .get_mut("agents")
        .and_then(Item::as_table_mut)
        .map(|agents| {
            agents.remove(key);
            agents.is_empty()
        })
        .unwrap_or(false);
    if remove_table {
        document.as_table_mut().remove("agents");
    }
}

fn apply_subagent_keys(
    document: &mut DocumentMut,
    settings: &crate::model::SubagentSettings,
    original: Option<&DocumentMut>,
) {
    use crate::model::SubagentMode;
    match settings.mode {
        SubagentMode::Inherit => {
            for key in MANAGED_AGENT_KEYS {
                // Inherit means Vellum does not manage these keys: restore the
                // values Codex had when the lease started instead of removing
                // them, so a user's own sub-agent defaults keep working while
                // Vellum is active. Custom-to-inherit therefore relinquishes
                // Vellum's written values and returns to the original ones.
                match original.and_then(|doc| agent_key(doc, key)) {
                    Some(item) => set_agent_key(document, key, item.clone()),
                    None => remove_agent_key(document, key),
                }
            }
        }
        SubagentMode::Custom => {
            match settings.catalog_id.as_deref().filter(|id| !id.is_empty()) {
                Some(model) => set_agent_key(document, "default_subagent_model", value(model)),
                None => remove_agent_key(document, "default_subagent_model"),
            }
            match settings
                .reasoning_effort
                .as_deref()
                .filter(|effort| !effort.is_empty())
            {
                Some(effort) => {
                    set_agent_key(document, "default_subagent_reasoning_effort", value(effort))
                }
                None => remove_agent_key(document, "default_subagent_reasoning_effort"),
            }
        }
    }
}

fn path_string(path: &Path) -> AppResult<String> {
    if !path.is_absolute() {
        return Err(AppError::Message(format!(
            "Codex 模型型錄必須使用絕對路徑：{}",
            path.display()
        )));
    }
    Ok(path.to_string_lossy().replace('\\', "/"))
}

fn write_text_atomic(path: &Path, content: &str) -> AppResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Message("設定路徑沒有父目錄".into()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| AppError::Message(format!("無法建立設定目錄：{error}")))?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, content)
        .map_err(|error| AppError::Message(format!("無法寫入暫存設定：{error}")))?;
    // The config now carries the proxy boundary key, so it is a secret file.
    // Tightened on the temp file, before the rename publishes it.
    restrict_to_current_user(&tmp)?;
    std::fs::rename(&tmp, path)
        .map_err(|error| AppError::Message(format!("無法原子替換設定：{error}")))
}

/// Restrict a file to the current user.
///
/// Unix takes `0600` directly. Windows has no mode bits, so this narrows the
/// ACL to the file's owner: an inherited ACL from the user profile is usually
/// already user-only, but "usually" is not a property a file holding a
/// credential should rely on.
fn restrict_to_current_user(path: &Path) -> AppResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| AppError::Message(format!("無法限制設定檔權限：{error}")))?;
    }
    #[cfg(windows)]
    {
        // `icacls /inheritance:r` drops inherited entries, then the owner is
        // granted full control explicitly. Failure is reported rather than
        // ignored: silently leaving a broad ACL on a file holding the boundary
        // key would defeat the point of writing it there.
        let output = crate::process::background_command("icacls")
            .arg(path)
            .args(["/inheritance:r", "/grant:r"])
            .arg(format!("{}:F", current_windows_user()?))
            .output()
            .map_err(|error| AppError::Message(format!("無法設定設定檔 ACL：{error}")))?;
        if !output.status.success() {
            return Err(AppError::Message(format!(
                "無法設定設定檔 ACL：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }
    let _ = path;
    Ok(())
}

#[cfg(windows)]
fn current_windows_user() -> AppResult<String> {
    let domain = std::env::var("USERDOMAIN").ok();
    let user = std::env::var("USERNAME")
        .map_err(|_| AppError::Message("無法取得目前 Windows 使用者".into()))?;
    Ok(match domain {
        Some(domain) if !domain.trim().is_empty() => format!("{domain}\\{user}"),
        _ => user,
    })
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> AppResult<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| AppError::Message(format!("lease 序列化失敗：{error}")))?;
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Message("lease 路徑沒有父目錄".into()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| AppError::Message(format!("無法建立 lease 目錄：{error}")))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)
        .map_err(|error| AppError::Message(format!("無法寫入 lease：{error}")))?;
    std::fs::rename(&tmp, path)
        .map_err(|error| AppError::Message(format!("無法替換 lease：{error}")))
}

#[cfg(test)]
mod tests {
    /// Shape-valid stand-in; these tests assert on config structure, not on
    /// the key's value.
    const TEST_BOUNDARY_KEY: &str =
        "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

    use super::*;

    #[test]
    fn parses_codex_cli_versions_for_native_subagent_gate() {
        assert_eq!(parse_codex_version("codex-cli 0.147.0"), Some((0, 147, 0)));
        assert_eq!(parse_codex_version("codex 1.2.3-beta.1"), Some((1, 2, 3)));
        assert_eq!(parse_codex_version("not-a-version"), None);
    }

    #[cfg(windows)]
    #[test]
    fn prefers_content_addressed_desktop_runtime_over_legacy_cli() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp
            .path()
            .join("OpenAI/Codex/bin/54ee14df1f760d5e/codex.exe");
        std::fs::create_dir_all(runtime.parent().unwrap()).unwrap();
        std::fs::write(&runtime, b"runtime").unwrap();

        let candidates = local_codex_candidates_from(None, Some(temp.path()));

        assert_eq!(candidates[0], runtime);
        assert_eq!(
            candidates[1],
            temp.path().join("Programs/OpenAI/Codex/bin/codex.exe")
        );
        assert_eq!(candidates.last(), Some(&PathBuf::from("codex")));
    }

    #[test]
    fn codex_candidate_discovery_deduplicates_an_explicit_path_candidate() {
        // On macOS the installed Desktop's CLI is also a candidate when present,
        // so assert on the explicit one rather than on the whole list.
        let candidates = local_codex_candidates_from(Some(PathBuf::from("codex")), None);
        assert_eq!(candidates.first(), Some(&PathBuf::from("codex")));
        assert_eq!(
            candidates
                .iter()
                .filter(|candidate| **candidate == PathBuf::from("codex"))
                .count(),
            1
        );
    }

    /// The 0.2.6 macOS report: "CodexDesktopProtocolMismatch: codex: No such
    /// file or directory". Nothing had run, so nothing had mismatched.
    #[test]
    fn a_missing_codex_says_where_it_looked_and_how_to_fix_it() {
        let detail = codex_not_found_detail();
        assert!(detail.contains("VELLUM_CODEX_BIN"));
        assert!(detail.contains("PATH"));
        for location in codex_search_locations() {
            assert!(detail.contains(&location.display().to_string()));
        }
    }
    use crate::model::{SubagentMode, SubagentSettings};

    fn paths(root: &Path) -> CodexPaths {
        CodexPaths {
            config: root.join(".codex/config.toml"),
            auth: root.join(".codex/auth.json"),
            models_cache: root.join(".codex/models_cache.json"),
            catalog: root.join("data/catalog.json"),
            lease: root.join("data/lease.json"),
        }
    }

    /// A pid essentially guaranteed to be dead on any platform this runs on:
    /// large enough that it is outside the range in active use, and if it
    /// somehow collided with a live process, [`owner_still_alive`]'s
    /// start-time comparison still would not match a freshly recorded `None`.
    const DEAD_PID: u32 = u32::MAX - 7;

    #[test]
    fn reconcile_with_no_lease_is_a_clean_noop() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(&paths.config, "model_provider = \"openai\"\n").unwrap();
        reconcile_lease(&paths).unwrap();
        assert!(!paths.lease.exists());
    }

    #[test]
    fn reconcile_cleans_up_a_lease_already_fully_reverted() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::create_dir_all(paths.catalog.parent().unwrap()).unwrap();
        let original = "model_provider = \"openai\"\n";
        std::fs::write(&paths.config, original).unwrap();
        std::fs::write(&paths.catalog, "{}").unwrap();
        write_json_atomic(
            &paths.lease,
            &ConfigLeaseV2 {
                schema_version: LEASE_SCHEMA_VERSION,
                owner: build_lease_owner(15721),
                original_hash: hash_config(original),
                applied_hash: hash_config("model_provider = \"vellum\"\n"),
                original_config: original.into(),
                applied_config: "model_provider = \"vellum\"\n".into(),
                boundary_credential_id: "__vellum_proxy_boundary__".into(),
                created_at: chrono::Utc::now().to_rfc3339(),
            },
        )
        .unwrap();

        // The config on disk now equals the lease's recorded `original` --
        // as if something already restored it -- so nothing is left to manage.
        reconcile_lease(&paths).unwrap();
        assert!(!paths.lease.exists());
        assert!(!paths.catalog.exists());
    }

    #[test]
    fn reconcile_reclaims_a_lease_whose_owner_is_dead() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        let original = "model_provider = \"openai\"\n";
        std::fs::write(&paths.config, "model_provider = \"vellum\"\n").unwrap();
        let mut dead_owner = build_lease_owner(15721);
        dead_owner.pid = DEAD_PID;
        dead_owner.process_start_time = Some("definitely-not-a-real-timestamp".into());
        write_json_atomic(
            &paths.lease,
            &ConfigLeaseV2 {
                schema_version: LEASE_SCHEMA_VERSION,
                owner: dead_owner,
                original_hash: hash_config(original),
                applied_hash: hash_config("model_provider = \"vellum\"\n"),
                original_config: original.into(),
                applied_config: "model_provider = \"vellum\"\n".into(),
                boundary_credential_id: "__vellum_proxy_boundary__".into(),
                created_at: chrono::Utc::now().to_rfc3339(),
            },
        )
        .unwrap();

        reconcile_lease(&paths).unwrap();

        // The lease survives -- its `original_config` is still the true
        // pre-Vellum baseline -- but ownership moved to this process.
        assert!(paths.lease.exists());
        let reclaimed: ConfigLeaseV2 =
            serde_json::from_slice(&std::fs::read(&paths.lease).unwrap()).unwrap();
        assert_eq!(reclaimed.owner.pid, std::process::id());
        assert_eq!(reclaimed.original_config, original);
    }

    #[test]
    fn reconcile_refuses_a_lease_whose_owner_is_still_alive() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        let original = "model_provider = \"openai\"\n";
        std::fs::write(&paths.config, "model_provider = \"vellum\"\n").unwrap();
        // This test process is unambiguously alive, and a matching start time
        // is not required to confirm it -- `owner_still_alive` only demands
        // the two agree when both are present.
        let live_owner = build_lease_owner(15721);
        write_json_atomic(
            &paths.lease,
            &ConfigLeaseV2 {
                schema_version: LEASE_SCHEMA_VERSION,
                owner: live_owner,
                original_hash: hash_config(original),
                applied_hash: hash_config("model_provider = \"vellum\"\n"),
                original_config: original.into(),
                applied_config: "model_provider = \"vellum\"\n".into(),
                boundary_credential_id: "__vellum_proxy_boundary__".into(),
                created_at: chrono::Utc::now().to_rfc3339(),
            },
        )
        .unwrap();

        let error = reconcile_lease(&paths).unwrap_err();
        assert!(error.to_string().contains("ConfigLeaseConflict"));
        // Refusing to act must not have touched the config or the lease.
        assert!(paths.lease.exists());
        assert_eq!(
            std::fs::read_to_string(&paths.config).unwrap(),
            "model_provider = \"vellum\"\n"
        );
    }

    #[test]
    fn reconcile_refuses_a_legacy_lease_with_no_recorded_owner() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(&paths.config, "model_provider = \"vellum\"\n").unwrap();
        write_json_atomic(
            &paths.lease,
            &LegacyConfigLease {
                original_config: "model_provider = \"openai\"\n".into(),
                applied_config: "model_provider = \"vellum\"\n".into(),
            },
        )
        .unwrap();

        let error = reconcile_lease(&paths).unwrap_err();
        assert!(error.to_string().contains("ConfigLeaseConflict"));
        assert!(paths.lease.exists());
    }

    /// V-01: the proxy now requires a boundary key on every request, and
    /// `openai_base_url` has nowhere to carry one. Codex must be routed through
    /// a custom provider whose `http_headers` presents it — while keeping the
    /// two capabilities that would otherwise be lost in the move: ChatGPT OAuth
    /// and the native Official WebSocket session.
    #[test]
    fn the_managed_provider_carries_the_boundary_key_oauth_and_websockets() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(&paths.config, "model_provider = \"openai\"\n").unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();

        let applied = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            applied["model_provider"].as_str(),
            Some(VELLUM_PROVIDER_NAME)
        );
        assert!(
            applied.get("openai_base_url").is_none(),
            "the built-in OpenAI provider cannot carry the boundary key and must not point at the proxy"
        );
        let provider = &applied["model_providers"][VELLUM_PROVIDER_NAME];
        assert_eq!(
            provider["name"].as_str(),
            Some("Vellum"),
            "third-party Enhanced threads must use Codex native local compaction"
        );
        assert_eq!(
            provider["base_url"].as_str(),
            Some("http://127.0.0.1:15721/v1")
        );
        assert_eq!(provider["wire_api"].as_str(), Some("responses"));
        assert_eq!(
            provider["requires_openai_auth"].as_bool(),
            Some(true),
            "ChatGPT OAuth must survive the move to a custom provider"
        );
        assert_eq!(
            provider["supports_websockets"].as_bool(),
            Some(true),
            "the native Official WebSocket session must survive the move"
        );
        assert_eq!(
            provider["http_headers"][vellum_proxy_runtime::BOUNDARY_KEY_HEADER].as_str(),
            Some(TEST_BOUNDARY_KEY)
        );
    }

    /// Official models are OpenAI models reached through a local hop, so their
    /// table claims the OpenAI identity that gates remote compaction, image
    /// generation, native web search, and history notes -- while still pointing
    /// at the proxy, which is the only place Vellum can apply the selected
    /// ChatGPT account.
    #[test]
    fn the_official_provider_is_openai_by_name_and_still_points_at_the_proxy() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.config,
            "model_provider = \"openai\"
",
        )
        .unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();

        let applied = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        let official = &applied["model_providers"][VELLUM_OFFICIAL_PROVIDER_NAME];
        assert_eq!(
            official["name"].as_str(),
            Some("OpenAI"),
            "Codex gates remote compaction on this exact literal"
        );
        assert_eq!(
            official["base_url"].as_str(),
            Some("http://127.0.0.1:15721/v1"),
            "an Official turn that skips the proxy also skips the account switch"
        );
        assert!(
            !official["base_url"]
                .as_str()
                .unwrap()
                .ends_with("/backend-api/codex"),
            "supports_codex_backend_routes() must stay false: the proxy has no ChatGPT backend routes"
        );
        assert_eq!(official["wire_api"].as_str(), Some("responses"));
        assert_eq!(official["requires_openai_auth"].as_bool(), Some(true));
        assert_eq!(official["supports_websockets"].as_bool(), Some(true));
        assert_eq!(
            official["http_headers"][vellum_proxy_runtime::BOUNDARY_KEY_HEADER].as_str(),
            Some(TEST_BOUNDARY_KEY)
        );
        assert_ne!(
            applied["model_providers"][VELLUM_PROVIDER_NAME]["name"].as_str(),
            official["name"].as_str(),
            "one table cannot answer for both third-party and Official models"
        );
    }

    /// Keep unsupported optional transports pinned off independently of the
    /// provider identity so a future Codex default cannot enable them on the
    /// Vellum boundary.
    #[test]
    fn applying_proxy_pins_unsupported_optional_transports_off() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(&paths.config, "model_provider = \"openai\"\n").unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();

        let applied = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            applied["features"]["remote_compaction_v2"].as_bool(),
            Some(true)
        );
        assert_eq!(
            applied["features"]["enable_request_compression"].as_bool(),
            Some(false),
            "Vellum's inbound contract only accepts identity encoding"
        );
        assert_eq!(
            applied["features"]["image_generation"].as_bool(),
            Some(false),
            "this third-party path does not expose the backend image extension"
        );
    }

    /// A user who had their own value for one of the newly-managed feature
    /// keys before Vellum ever started gets that exact value back on stop —
    /// same three-way rule as `standalone_web_search` already followed.
    #[test]
    fn stopping_proxy_restores_the_users_prior_feature_flag_values() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.config,
            "model_provider = \"openai\"\n[features]\nenable_request_compression = true\nother = true\n",
        )
        .unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();
        assert!(restore_proxy_config(&paths).unwrap());

        let restored = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            restored["features"]["enable_request_compression"].as_bool(),
            Some(true),
            "the user's own pre-Vellum value must survive the round trip"
        );
        assert_eq!(restored["features"]["other"].as_bool(), Some(true));
        assert!(
            feature_item(&restored, "remote_compaction_v2").is_none(),
            "a key Vellum introduced (absent before) must be removed, not defaulted"
        );
        assert!(feature_item(&restored, "image_generation").is_none());
    }

    /// Codex has no `auto_compaction` feature flag -- 0.153 rejects the name
    /// outright. Scheduling comes from the catalog, so writing the key would
    /// leave an inert entry in the user's config that Vellum then has to lease
    /// and restore for no gain.
    #[test]
    fn applying_proxy_never_writes_the_unknown_auto_compaction_feature() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(&paths.config, "model_provider = \"openai\"\n").unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();

        let applied = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        // Scheduling comes from the catalog -- `auto_compact_token_limit` for
        // a remote-managed Codex, the model's own `context_window` for the
        // local one -- plus `remote_compaction_v2` for the Official trigger.
        assert!(
            feature_item(&applied, "auto_compaction").is_none(),
            "auto_compaction must not be written: {applied}"
        );
        assert_eq!(
            applied["features"]["remote_compaction_v2"].as_bool(),
            Some(true),
            "the trigger must still reach Vellum"
        );
    }

    /// Bundled Codex applies `model_context_window` and
    /// `model_auto_compact_token_limit` as *global* overrides on top of
    /// every model's own catalog entry. A pre-existing user value here would
    /// silently defeat the catalog's per-model context capacity and
    /// runtime-owned compact threshold. Vellum must clear all three while it
    /// runs, never write a value of its own, and restore the user's exact
    /// prior values on stop.
    #[test]
    fn applying_proxy_clears_conflicting_global_compaction_overrides() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.config,
            "model_provider = \"openai\"\n\
             model_context_window = 400000\n\
             model_auto_compact_token_limit = 350000\n\
             model_auto_compact_token_limit_scope = \"body_after_prefix\"\n",
        )
        .unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();

        let applied = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert!(applied.get("model_context_window").is_none());
        assert!(applied.get("model_auto_compact_token_limit").is_none());
        assert!(applied
            .get("model_auto_compact_token_limit_scope")
            .is_none());

        assert!(restore_proxy_config(&paths).unwrap());
        let restored = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(restored["model_context_window"].as_integer(), Some(400_000));
        assert_eq!(
            restored["model_auto_compact_token_limit"].as_integer(),
            Some(350_000)
        );
        assert_eq!(
            restored["model_auto_compact_token_limit_scope"].as_str(),
            Some("body_after_prefix")
        );
    }

    /// When the user never had these keys set, restore must leave them
    /// absent rather than defaulting them to anything.
    #[test]
    fn restoring_leaves_global_compaction_overrides_absent_when_the_user_never_set_them() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(&paths.config, "model_provider = \"openai\"\n").unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();
        assert!(restore_proxy_config(&paths).unwrap());

        let restored = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert!(restored.get("model_context_window").is_none());
        assert!(restored.get("model_auto_compact_token_limit").is_none());
        assert!(restored
            .get("model_auto_compact_token_limit_scope")
            .is_none());
    }

    #[test]
    fn applying_proxy_preserves_a_user_openai_base_url() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.config,
            "model_provider = \"openai\"\nopenai_base_url = \"https://user.example/v1\"\n",
        )
        .unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();

        let applied = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            applied["openai_base_url"].as_str(),
            Some("https://user.example/v1")
        );
    }

    #[test]
    fn applying_proxy_migrates_the_loopback_url_from_an_existing_lease() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        let original = "model_provider = \"openai\"\n";
        let previous_applied = concat!(
            "model_provider = \"vellum\"\n",
            "openai_base_url = \"http://127.0.0.1:15721/v1\"\n"
        );
        std::fs::write(&paths.config, previous_applied).unwrap();
        write_json_atomic(
            &paths.lease,
            &ConfigLeaseV2 {
                schema_version: LEASE_SCHEMA_VERSION,
                owner: build_lease_owner(15721),
                original_hash: hash_config(original),
                applied_hash: hash_config(previous_applied),
                original_config: original.into(),
                applied_config: previous_applied.into(),
                boundary_credential_id: vellum_proxy_runtime::BOUNDARY_CREDENTIAL_ID.into(),
                created_at: chrono::Utc::now().to_rfc3339(),
            },
        )
        .unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();

        let applied = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert!(applied.get("openai_base_url").is_none());
        assert_eq!(
            applied["model_providers"][VELLUM_PROVIDER_NAME]["http_headers"]
                [vellum_proxy_runtime::BOUNDARY_KEY_HEADER]
                .as_str(),
            Some(TEST_BOUNDARY_KEY)
        );

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();
        let reapplied = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert!(reapplied.get("openai_base_url").is_none());
    }

    #[test]
    fn applying_proxy_preserves_an_openai_url_edited_after_the_lease() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        let original = "model_provider = \"openai\"\n";
        let previous_applied = concat!(
            "model_provider = \"vellum\"\n",
            "openai_base_url = \"http://127.0.0.1:15721/v1\"\n"
        );
        std::fs::write(
            &paths.config,
            "model_provider = \"vellum\"\nopenai_base_url = \"https://edited.example/v1\"\n",
        )
        .unwrap();
        write_json_atomic(
            &paths.lease,
            &ConfigLeaseV2 {
                schema_version: LEASE_SCHEMA_VERSION,
                owner: build_lease_owner(15721),
                original_hash: hash_config(original),
                applied_hash: hash_config(previous_applied),
                original_config: original.into(),
                applied_config: previous_applied.into(),
                boundary_credential_id: vellum_proxy_runtime::BOUNDARY_CREDENTIAL_ID.into(),
                created_at: chrono::Utc::now().to_rfc3339(),
            },
        )
        .unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();

        let applied = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            applied["openai_base_url"].as_str(),
            Some("https://edited.example/v1")
        );
    }

    /// Restore must remove the live proxy endpoint and boundary key while
    /// retaining a credential-free direct-OpenAI alias for rollout files that
    /// persist `model_provider = "vellum"`.
    #[test]
    fn restoring_replaces_the_managed_provider_with_a_direct_thread_recovery_alias() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.config,
            "model_provider = \"openai\"\n[model_providers.custom]\nname = \"keep\"\n",
        )
        .unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();
        assert!(restore_proxy_config(&paths).unwrap());

        let restored = std::fs::read_to_string(&paths.config).unwrap();
        assert!(
            !restored.contains(TEST_BOUNDARY_KEY),
            "the boundary key must not be left in a restored config: {restored}"
        );
        let restored_doc = restored.parse::<DocumentMut>().unwrap();
        let recovery = &restored_doc["model_providers"][VELLUM_PROVIDER_NAME];
        assert_eq!(
            recovery["base_url"].as_str(),
            Some(OPENAI_THREAD_RECOVERY_BASE_URL)
        );
        assert_eq!(recovery["requires_openai_auth"].as_bool(), Some(true));
        assert_eq!(recovery["supports_websockets"].as_bool(), Some(true));
        assert!(recovery.get("http_headers").is_none());
        // Official threads record `vellum-official` in `session_meta`, so that
        // id needs an alias too or those threads become unopenable. It keeps
        // the OpenAI display name it was written under.
        let official_recovery = &restored_doc["model_providers"][VELLUM_OFFICIAL_PROVIDER_NAME];
        assert_eq!(official_recovery["name"].as_str(), Some("OpenAI"));
        assert_eq!(
            official_recovery["base_url"].as_str(),
            Some(OPENAI_THREAD_RECOVERY_BASE_URL)
        );
        assert!(official_recovery.get("http_headers").is_none());
        assert!(
            restored.contains("[model_providers.custom]"),
            "the user's own provider must be preserved: {restored}"
        );
        assert!(restored.contains("model_provider = \"openai\""));
    }

    #[test]
    fn restore_preserves_a_user_edited_vellum_provider_instead_of_overwriting_it() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(&paths.config, "model_provider = \"openai\"\n").unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();
        let mut edited = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        edited["model_providers"][VELLUM_PROVIDER_NAME]["base_url"] =
            value("https://user.example/v1");
        std::fs::write(&paths.config, edited.to_string()).unwrap();

        assert!(restore_proxy_config(&paths).unwrap());
        let restored = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            restored["model_providers"][VELLUM_PROVIDER_NAME]["base_url"].as_str(),
            Some("https://user.example/v1")
        );
    }

    #[test]
    fn apply_and_restore_preserves_auth_and_unmanaged_fields() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.config,
            "model_provider = \"openai\"\napproval_policy = \"on-request\"\ndisable_response_storage = false\n",
        )
        .unwrap();
        std::fs::write(&paths.auth, b"do-not-touch").unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();
        let mut current = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            current["features"]["standalone_web_search"].as_bool(),
            Some(true)
        );
        current["approval_policy"] = value("never");
        std::fs::write(&paths.config, current.to_string()).unwrap();

        assert!(restore_proxy_config(&paths).unwrap());
        let restored = std::fs::read_to_string(&paths.config).unwrap();
        assert!(restored.contains("model_provider = \"openai\""));
        assert!(restored.contains("approval_policy = \"never\""));
        assert!(restored.contains("disable_response_storage = false"));
        assert!(!restored.contains("openai_base_url"));
        let restored_doc = restored.parse::<DocumentMut>().unwrap();
        assert!(feature_item(&restored_doc, "standalone_web_search").is_none());
        assert_eq!(std::fs::read(&paths.auth).unwrap(), b"do-not-touch");
    }

    #[test]
    fn restore_preserves_existing_standalone_search_feature_value() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.config,
            "model_provider = \"openai\"\n[features]\nstandalone_web_search = false\nother = true\n",
        )
        .unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();
        assert!(restore_proxy_config(&paths).unwrap());
        let restored = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            restored["features"]["standalone_web_search"].as_bool(),
            Some(false)
        );
        assert_eq!(restored["features"]["other"].as_bool(), Some(true));
    }

    fn custom_subagent(model: &str, effort: Option<&str>) -> SubagentSettings {
        SubagentSettings {
            mode: SubagentMode::Custom,
            route_id: Some("grok-cli".into()),
            catalog_id: Some(model.into()),
            reasoning_effort: effort.map(str::to_string),
        }
    }

    #[test]
    fn custom_subagent_defaults_are_written_and_restored_without_touching_other_agents_keys() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.config,
            "model_provider = \"openai\"\n[agents]\nenabled = true\nmax_concurrent_threads_per_session = 6\n",
        )
        .unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &custom_subagent("vlm-test-model", Some("high")),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();
        let applied = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            applied["agents"]["default_subagent_model"].as_str(),
            Some("vlm-test-model")
        );
        assert_eq!(
            applied["agents"]["default_subagent_reasoning_effort"].as_str(),
            Some("high")
        );
        assert_eq!(applied["agents"]["enabled"].as_bool(), Some(true));
        assert_eq!(
            applied["agents"]["max_concurrent_threads_per_session"].as_integer(),
            Some(6)
        );

        // The user edits an unrelated `[agents]` key while Vellum is running;
        // restore keeps that edit and only removes the keys Vellum wrote.
        let mut edited = applied.clone();
        edited["agents"]["enabled"] = value(false);
        std::fs::write(&paths.config, edited.to_string()).unwrap();

        assert!(restore_proxy_config(&paths).unwrap());
        let restored = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(restored["agents"]["enabled"].as_bool(), Some(false));
        assert!(agent_key(&restored, "default_subagent_model").is_none());
        assert!(agent_key(&restored, "default_subagent_reasoning_effort").is_none());
    }

    /// The laundering path, end to end. A crash or a partial restore leaves
    /// Vellum's provider tables and sub-agent default in `config.toml` with no
    /// lease beside them; the next apply must not adopt that file as Codex's
    /// pre-Vellum baseline, or "keep Desktop's existing settings" starts
    /// meaning "keep whatever Vellum last pinned" and uninstalling writes a
    /// `vlm-` model id back into the user's config.
    #[test]
    fn a_config_still_carrying_vellum_is_not_adopted_as_the_baseline() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();

        // Round one leaves the config written; then the lease disappears.
        std::fs::write(
            &paths.config,
            "model_provider = \"openai\"
",
        )
        .unwrap();
        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &custom_subagent("vlm-beb60d2887-qwen", None),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();
        std::fs::remove_file(&paths.lease).unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();

        let lease: ConfigLeaseV2 =
            serde_json::from_slice(&std::fs::read(&paths.lease).unwrap()).unwrap();
        let baseline = lease.original_config.parse::<DocumentMut>().unwrap();
        assert!(agent_key(&baseline, "default_subagent_model").is_none());
        assert!(model_provider_item(&baseline, VELLUM_PROVIDER_NAME).is_none());
        assert!(model_provider_item(&baseline, VELLUM_OFFICIAL_PROVIDER_NAME).is_none());
        assert_ne!(
            baseline.get("model_provider").and_then(Item::as_str),
            Some(VELLUM_PROVIDER_NAME)
        );

        // And restoring from it leaves nothing of Vellum's behind.
        restore_proxy_config(&paths).unwrap();
        let restored = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert!(agent_key(&restored, "default_subagent_model").is_none());
    }

    /// An install that already has a laundered lease on disk repairs it on the
    /// next apply rather than waiting for a reinstall.
    #[test]
    fn an_already_laundered_lease_is_repaired_in_place() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.config,
            "model_provider = \"openai\"
",
        )
        .unwrap();
        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();

        // Rewrite the lease the way an older Vellum left it: the baseline is a
        // config that still names Vellum's provider and sub-agent default.
        let mut lease: ConfigLeaseV2 =
            serde_json::from_slice(&std::fs::read(&paths.lease).unwrap()).unwrap();
        let dirty = format!(
            "{}
[agents]
default_subagent_model = \"vlm-beb60d2887-qwen\"
",
            lease.applied_config
        );
        lease.original_hash = hash_config(&dirty);
        lease.original_config = dirty;
        write_json_atomic(&paths.lease, &lease).unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();

        let lease: ConfigLeaseV2 =
            serde_json::from_slice(&std::fs::read(&paths.lease).unwrap()).unwrap();
        let baseline = lease.original_config.parse::<DocumentMut>().unwrap();
        assert!(agent_key(&baseline, "default_subagent_model").is_none());
        assert!(model_provider_item(&baseline, VELLUM_PROVIDER_NAME).is_none());
        assert_eq!(lease.original_hash, hash_config(&lease.original_config));
    }

    #[test]
    fn apply_subagent_defaults_hot_updates_live_config_and_lease() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(&paths.config, "model_provider = \"openai\"\n").unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();
        apply_subagent_defaults(&paths, &custom_subagent("vlm-x", Some("low"))).unwrap();

        let live = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            live["agents"]["default_subagent_model"].as_str(),
            Some("vlm-x")
        );
        assert_eq!(
            live["agents"]["default_subagent_reasoning_effort"].as_str(),
            Some("low")
        );

        // The lease's applied_config must agree, so a later restore removes
        // exactly the keys Vellum wrote.
        let lease: ConfigLeaseV2 =
            serde_json::from_slice(&std::fs::read(&paths.lease).unwrap()).unwrap();
        let applied = lease.applied_config.parse::<DocumentMut>().unwrap();
        assert_eq!(
            applied["agents"]["default_subagent_model"].as_str(),
            Some("vlm-x")
        );

        // Switching back to inherit removes both keys from config and lease.
        apply_subagent_defaults(&paths, &SubagentSettings::default()).unwrap();
        let live = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert!(live.get("agents").is_none());
        let lease: ConfigLeaseV2 =
            serde_json::from_slice(&std::fs::read(&paths.lease).unwrap()).unwrap();
        assert!(lease
            .applied_config
            .parse::<DocumentMut>()
            .unwrap()
            .get("agents")
            .is_none());

        assert!(restore_proxy_config(&paths).unwrap());
        let restored = std::fs::read_to_string(&paths.config).unwrap();
        assert!(restored.contains("model_provider = \"openai\""));
        assert!(!restored.contains("[agents]"));
    }

    #[test]
    fn inherit_preserves_preexisting_subagent_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.config,
            "model_provider = \"openai\"\n[agents]\nenabled = true\ndefault_subagent_model = \"original-model\"\ndefault_subagent_reasoning_effort = \"low\"\n",
        )
        .unwrap();

        // Vellum starts in inherit mode: Codex's own sub-agent defaults stay in
        // the live config instead of being deleted for the lease duration.
        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();
        let live = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(live["agents"]["enabled"].as_bool(), Some(true));
        assert_eq!(
            live["agents"]["default_subagent_model"].as_str(),
            Some("original-model")
        );
        assert_eq!(
            live["agents"]["default_subagent_reasoning_effort"].as_str(),
            Some("low")
        );

        // Custom writes Vellum's defaults; switching back to inherit restores
        // the pre-existing values rather than removing them.
        apply_subagent_defaults(&paths, &custom_subagent("vlm-x", Some("medium"))).unwrap();
        apply_subagent_defaults(&paths, &SubagentSettings::default()).unwrap();
        let live = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(live["agents"]["enabled"].as_bool(), Some(true));
        assert_eq!(
            live["agents"]["default_subagent_model"].as_str(),
            Some("original-model")
        );
        assert_eq!(
            live["agents"]["default_subagent_reasoning_effort"].as_str(),
            Some("low")
        );

        // Restore keeps both the original defaults and the user's other keys.
        assert!(restore_proxy_config(&paths).unwrap());
        let restored = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(restored["agents"]["enabled"].as_bool(), Some(true));
        assert_eq!(
            restored["agents"]["default_subagent_model"].as_str(),
            Some("original-model")
        );
        assert_eq!(
            restored["agents"]["default_subagent_reasoning_effort"].as_str(),
            Some("low")
        );
    }

    #[test]
    fn restore_returns_original_subagent_defaults_when_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.config,
            "model_provider = \"openai\"\n[agents]\ndefault_subagent_model = \"original-model\"\n",
        )
        .unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &custom_subagent("vlm-vellum", None),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();
        assert!(restore_proxy_config(&paths).unwrap());
        let restored = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            restored["agents"]["default_subagent_model"].as_str(),
            Some("original-model")
        );
        assert!(agent_key(&restored, "default_subagent_reasoning_effort").is_none());
    }

    #[test]
    fn restore_preserves_user_edited_subagent_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(&paths.config, "model_provider = \"openai\"\n").unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &custom_subagent("vlm-vellum", None),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();
        let mut edited = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        edited["agents"]["default_subagent_model"] = value("user-chosen-model");
        std::fs::write(&paths.config, edited.to_string()).unwrap();

        assert!(restore_proxy_config(&paths).unwrap());
        let restored = std::fs::read_to_string(&paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        // The user's own sub-agent default is kept because Vellum's key no
        // longer matches the applied value.
        assert_eq!(
            restored["agents"]["default_subagent_model"].as_str(),
            Some("user-chosen-model")
        );
    }

    #[test]
    fn restore_recovers_reserved_provider_tables_and_preserves_codex_data() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        std::fs::create_dir_all(paths.config.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.config,
            "model_provider = \"openai\"\n[model_providers.openai]\nname = \"native\"\n[model_providers.custom]\nname = \"keep\"\n",
        )
        .unwrap();
        let sessions = temp.path().join(".codex/sessions/thread.jsonl");
        let projects = temp.path().join(".codex/projects/index.json");
        std::fs::create_dir_all(sessions.parent().unwrap()).unwrap();
        std::fs::create_dir_all(projects.parent().unwrap()).unwrap();
        std::fs::write(&sessions, b"conversation").unwrap();
        std::fs::write(&projects, b"projects").unwrap();

        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();
        std::fs::write(&paths.catalog, b"{\"models\":[]}").unwrap();
        assert!(restore_proxy_config(&paths).unwrap());

        let restored = std::fs::read_to_string(&paths.config).unwrap();
        assert!(restored.contains("[model_providers.openai]"));
        assert!(restored.contains("[model_providers.custom]"));
        assert!(!paths.catalog.exists());
        assert_eq!(std::fs::read(sessions).unwrap(), b"conversation");
        assert_eq!(std::fs::read(projects).unwrap(), b"projects");
    }

    #[test]
    fn apply_uses_an_absolute_catalog_path() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        apply_proxy_config(
            &paths,
            "http://127.0.0.1:15721/v1",
            &SubagentSettings::default(),
            TEST_BOUNDARY_KEY,
            15721,
        )
        .unwrap();
        let document = std::fs::read_to_string(paths.config)
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        let catalog = document["model_catalog_json"].as_str().unwrap();
        assert!(Path::new(catalog).is_absolute());
    }

    #[test]
    fn session_index_maps_thread_ids_to_hashed_human_labels() {
        let temp = tempfile::tempdir().unwrap();
        let index = temp.path().join("session_index.jsonl");
        std::fs::write(
            &index,
            "{\"id\":\"thread-1\",\"thread_name\":\"調查 Vellum 工作階段\"}\n\
             {\"id\":\"thread-2\",\"thread_name\":\"\"}\n",
        )
        .unwrap();
        let labels = read_session_labels_from(&index);
        // Both the raw id and its hash resolve, so one map serves a live
        // `codex:<session>:<thread>` key and a legacy hashed one.
        let hashed = format!("{:x}", Sha256::digest(b"thread-1"));
        for key in ["thread-1", hashed.as_str()] {
            assert_eq!(
                labels.get(key).map(String::as_str),
                Some("調查 Vellum 工作階段"),
                "{key}"
            );
        }
        // The empty title is still dropped rather than stored as a name.
        assert_eq!(labels.len(), 2);
    }

    #[test]
    fn current_codex_state_database_overrides_the_legacy_session_index() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("session_index.jsonl"),
            "{\"id\":\"thread-1\",\"thread_name\":\"stale title\"}\n",
        )
        .unwrap();
        let database = temp.path().join("state_5.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT NOT NULL, name TEXT);\
                 INSERT INTO threads VALUES ('thread-1', 'generated title', 'user rename');\
                 INSERT INTO threads VALUES ('thread-2', 'current title', NULL);",
            )
            .unwrap();
        drop(connection);

        let labels = read_session_labels_from_codex_home(temp.path());
        assert_eq!(
            labels.get("thread-1").map(String::as_str),
            Some("user rename")
        );
        assert_eq!(
            labels.get("thread-2").map(String::as_str),
            Some("current title")
        );
        let hashed = format!("{:x}", Sha256::digest(b"thread-2"));
        assert_eq!(
            labels.get(&hashed).map(String::as_str),
            Some("current title")
        );
    }

    #[test]
    fn rollout_runtime_uses_latest_turn_tokens_not_cumulative_usage() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp
            .path()
            .join("rollout-2026-07-29T00-00-00-019fa918-d38c-74e0-948a-b725d20f46b0.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"Hi\"}}\n",
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"vlm-old-model\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"調查上下文 token 計算\"}}\n",
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.6-sol\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"total_tokens\":13575844},\"last_token_usage\":{\"total_tokens\":49843},\"model_context_window\":258400}}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"total_tokens\":14000000},\"last_token_usage\":{\"total_tokens\":51234},\"model_context_window\":258400}}}\n"
            ),
        )
        .unwrap();
        assert_eq!(latest_token_count(&path), Some((51_234, 258_400)));
        assert_eq!(latest_turn_model(&path).as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(
            first_meaningful_user_message(&path).as_deref(),
            Some("調查上下文 token 計算")
        );
        assert_eq!(
            thread_id_from_rollout_path(&path).as_deref(),
            Some("019fa918-d38c-74e0-948a-b725d20f46b0")
        );
    }

    #[test]
    fn rollout_title_fallback_reads_current_response_item_user_messages() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"developer\",\"content\":[{\"type\":\"input_text\",\"text\":\"not the title\"}]}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"調查目前 Vellum 的架構\"}]}}\n"
            ),
        )
        .unwrap();
        assert_eq!(
            first_meaningful_user_message(&path).as_deref(),
            Some("調查目前 Vellum 的架構")
        );
    }

    #[test]
    fn runtime_conversation_keys_resolve_to_their_thread_uuid() {
        // What a live install actually stores. Both segments name the same
        // conversation; a sub-thread differs only in the second.
        let key = "codex:01a079b3-33f3-7bb3-a4c9-e60261a5267d:01a07b65-9b60-7551-9015-68594782955b";
        assert!(conversation_key_matches(
            key,
            "01a079b3-33f3-7bb3-a4c9-e60261a5267d"
        ));
        assert!(conversation_key_matches(
            key,
            "01a07b65-9b60-7551-9015-68594782955b"
        ));
        assert!(!conversation_key_matches(
            key,
            "01a0756d-5919-77e0-9271-e847988268df"
        ));
        // The scheme itself must never be an alias: it is shared by every
        // conversation, so matching on it would bind them all together.
        assert!(!conversation_key_matches(key, "codex"));
    }

    #[test]
    fn legacy_hashed_conversation_keys_still_match_their_thread() {
        let thread = "01a079b3-33f3-7bb3-a4c9-e60261a5267d";
        let hashed = format!("{:x}", Sha256::digest(thread.as_bytes()));
        assert!(conversation_key_matches(&hashed, thread));
        assert!(!conversation_key_matches(
            &hashed,
            "01a0756d-5919-77e0-9271-e847988268df"
        ));
    }

    #[test]
    fn compaction_parser_deduplicates_rollout_pair_and_tracks_token_transition() {
        let lines = concat!(
            "{\"timestamp\":\"2026-08-20T12:02:06.604Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"total_tokens\":224440}}}}\n",
            "{\"timestamp\":\"2026-08-20T12:02:06.622Z\",\"type\":\"compacted\",\"payload\":{\"message\":\"  summary\\n\"}}\n",
            "{\"timestamp\":\"2026-08-20T12:02:06.640Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"total_tokens\":13097}}}}\n",
            "{\"timestamp\":\"2026-08-20T12:02:06.658Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"context_compacted\"}}\n",
        );
        let mut scan = RolloutScan::default();
        parse_compaction_lines(
            lines.as_bytes(),
            Some("01a079b3-33f3-7bb3-a4c9-e60261a5267d".into()),
            Some("Fix Vellum".into()),
            &mut scan,
        );

        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].tokens_before, Some(224_440));
        assert_eq!(scan.events[0].tokens_after, Some(13_097));
        assert_eq!(scan.events[0].label.as_deref(), Some("Fix Vellum"));
        assert_eq!(
            scan.events[0].replacement_text.as_deref(),
            Some("  summary\n")
        );
    }

    #[test]
    fn compaction_parser_accepts_context_compacted_without_large_summary_record() {
        let lines = concat!(
            "{\"timestamp\":\"2026-08-20T12:02:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"total_tokens\":200000}}}}\n",
            "{\"timestamp\":\"2026-08-20T12:02:01Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"context_compacted\"}}\n",
            "{\"timestamp\":\"2026-08-20T12:02:02Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"total_tokens\":12000}}}}\n",
        );
        let mut scan = RolloutScan::default();
        parse_compaction_lines(lines.as_bytes(), None, None, &mut scan);

        assert_eq!(scan.events.len(), 1);
        assert_eq!(scan.events[0].tokens_before, Some(200_000));
        assert_eq!(scan.events[0].tokens_after, Some(12_000));
    }
}
