//! Remote-owned Grok credential refresh for detached operation.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::configuration::{put_credential, remove_credential};
use crate::state::AgentPaths;

const TIMER: &str = "vellum-grok-refresh.timer";
const LOGIN_META: &str = "device-login.json";
const LOGIN_LOG: &str = "device-login.log";
#[cfg(target_os = "linux")]
const SERVICE: &str = "vellum-grok-refresh.service";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteGrokStatus {
    pub configured: bool,
    pub credential_id: String,
    pub account: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_refresh_at: Option<DateTime<Utc>>,
    pub detached_qualified: bool,
    pub refresh_timer_active: bool,
    pub login_pending: bool,
    pub verification_uri: Option<String>,
    pub user_code: Option<String>,
    pub login_started_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceLogin {
    pid: u32,
    started_at: DateTime<Utc>,
}

pub fn start_login(paths: &AgentPaths) -> Result<RemoteGrokStatus, String> {
    let account_dir = account_dir(paths);
    fs::create_dir_all(&account_dir).map_err(|error| error.to_string())?;
    set_private_dir(&account_dir)?;
    if account_dir.join("auth.json").is_file() {
        return status(paths);
    }
    if let Ok(login) = read_device_login(&account_dir) {
        if pid_alive(login.pid) {
            return status(paths);
        }
        remove_login_files(&account_dir);
    }
    let binary = ensure_grok_binary()?;
    let log_path = account_dir.join(LOGIN_LOG);
    let log = private_log(&log_path)?;
    let stderr = log.try_clone().map_err(|error| error.to_string())?;
    let mut command = Command::new(binary);
    command
        .args(["login", "--device-auth"])
        .env("GROK_HOME", &account_dir)
        .env("GROK_DISABLE_AUTOUPDATER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .spawn()
        .map_err(|error| format!("GrokDeviceLoginStartFailed: {error}"))?;
    atomic_json(
        &account_dir.join(LOGIN_META),
        &DeviceLogin {
            pid: child.id(),
            started_at: Utc::now(),
        },
    )?;
    std::thread::sleep(std::time::Duration::from_millis(400));
    status(paths)
}

pub fn poll_login(paths: &AgentPaths) -> Result<RemoteGrokStatus, String> {
    let account_dir = account_dir(paths);
    if account_dir.join("auth.json").is_file() {
        finalize_device_login(paths)?;
    }
    status(paths)
}

pub fn cancel_login(paths: &AgentPaths) -> Result<RemoteGrokStatus, String> {
    let account_dir = account_dir(paths);
    if let Ok(login) = read_device_login(&account_dir) {
        if pid_alive(login.pid) {
            let _ = Command::new("kill")
                .args(["-TERM", "--", &login.pid.to_string()])
                .status();
        }
    }
    remove_login_files(&account_dir);
    status(paths)
}

pub fn install_account(
    paths: &AgentPaths,
    credential_id: &str,
    auth_json: &str,
    version_json: Option<&str>,
    agent_id: Option<&str>,
) -> Result<RemoteGrokStatus, String> {
    let value: Value =
        serde_json::from_str(auth_json).map_err(|error| format!("InvalidGrokAuth: {error}"))?;
    let account_dir = account_dir(paths);
    fs::create_dir_all(&account_dir).map_err(|error| error.to_string())?;
    set_private_dir(&account_dir)?;
    atomic_write(&account_dir.join("auth.json"), auth_json.as_bytes(), 0o600)?;
    if let Some(version) = version_json {
        let _: Value = serde_json::from_str(version)
            .map_err(|error| format!("InvalidGrokVersion: {error}"))?;
        atomic_write(&account_dir.join("version.json"), version.as_bytes(), 0o600)?;
    }
    if let Some(agent) = agent_id.map(str::trim).filter(|value| !value.is_empty()) {
        atomic_write(&account_dir.join("agent_id"), agent.as_bytes(), 0o600)?;
    }
    let detached = contains_refresh_token(&value) && grok_binary().is_some();
    rotate_proxy_secret(paths, credential_id, &value)?;
    let metadata = Metadata {
        credential_id: credential_id.into(),
        last_refresh_at: Some(Utc::now()),
        detached_qualified: detached,
        last_error: None,
    };
    atomic_json(&account_dir.join("metadata.json"), &metadata)?;
    if detached {
        install_refresh_timer()?;
    }
    status(paths)
}

pub fn refresh(paths: &AgentPaths) -> Result<RemoteGrokStatus, String> {
    let account_dir = account_dir(paths);
    let mut metadata = read_metadata(&account_dir)?;
    let binary = grok_binary().ok_or_else(|| "GrokCliMissing".to_string())?;
    let output = Command::new(binary)
        .arg("models")
        .env("GROK_HOME", &account_dir)
        .env("GROK_DISABLE_AUTOUPDATER", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|error| format!("Grok refresh failed to start: {error}"))?;
    if !output.status.success() {
        metadata.last_error = Some(String::from_utf8_lossy(&output.stderr).trim().to_string());
        atomic_json(&account_dir.join("metadata.json"), &metadata)?;
        return Err(format!(
            "GrokRefreshFailed: {}",
            metadata.last_error.as_deref().unwrap_or("unknown error")
        ));
    }
    let value: Value = serde_json::from_slice(
        &fs::read(account_dir.join("auth.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    rotate_proxy_secret(paths, &metadata.credential_id, &value)?;
    metadata.last_refresh_at = Some(Utc::now());
    metadata.detached_qualified = contains_refresh_token(&value);
    metadata.last_error = None;
    atomic_json(&account_dir.join("metadata.json"), &metadata)?;
    status(paths)
}

pub fn status(paths: &AgentPaths) -> Result<RemoteGrokStatus, String> {
    let account_dir = account_dir(paths);
    if !account_dir.join("auth.json").is_file() {
        return Ok(RemoteGrokStatus {
            configured: false,
            credential_id: "grok-cli".into(),
            account: None,
            expires_at: None,
            last_refresh_at: None,
            detached_qualified: false,
            refresh_timer_active: false,
            login_pending: read_device_login(&account_dir).is_ok_and(|login| pid_alive(login.pid)),
            verification_uri: login_prompt(&account_dir).0,
            user_code: login_prompt(&account_dir).1,
            login_started_at: read_device_login(&account_dir)
                .ok()
                .map(|login| login.started_at),
            error: None,
        });
    }
    let value: Value = serde_json::from_slice(
        &fs::read(account_dir.join("auth.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let candidate = best_credential(&value).ok_or_else(|| "GrokCredentialMissing".to_string())?;
    let metadata = read_metadata(&account_dir)?;
    Ok(RemoteGrokStatus {
        configured: true,
        credential_id: metadata.credential_id,
        account: candidate.user_id.or(candidate.email),
        expires_at: candidate.expires_at,
        last_refresh_at: metadata.last_refresh_at,
        detached_qualified: metadata.detached_qualified && contains_refresh_token(&value),
        refresh_timer_active: timer_active(),
        login_pending: false,
        verification_uri: None,
        user_code: None,
        login_started_at: None,
        error: metadata.last_error,
    })
}

fn finalize_device_login(paths: &AgentPaths) -> Result<(), String> {
    let account_dir = account_dir(paths);
    let value: Value = serde_json::from_slice(
        &fs::read(account_dir.join("auth.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    rotate_proxy_secret(paths, "grok-cli", &value)?;
    let detached = contains_refresh_token(&value) && grok_binary().is_some();
    atomic_json(
        &account_dir.join("metadata.json"),
        &Metadata {
            credential_id: "grok-cli".into(),
            last_refresh_at: Some(Utc::now()),
            detached_qualified: detached,
            last_error: None,
        },
    )?;
    if detached {
        install_refresh_timer()?;
    }
    remove_login_files(&account_dir);
    Ok(())
}

fn private_log(path: &Path) -> Result<std::fs::File, String> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|error| error.to_string())
}

fn read_device_login(account_dir: &Path) -> Result<DeviceLogin, String> {
    serde_json::from_slice(
        &fs::read(account_dir.join(LOGIN_META)).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn remove_login_files(account_dir: &Path) {
    let _ = fs::remove_file(account_dir.join(LOGIN_META));
    let _ = fs::remove_file(account_dir.join(LOGIN_LOG));
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        PathBuf::from(format!("/proc/{pid}")).is_dir()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

fn login_prompt(account_dir: &Path) -> (Option<String>, Option<String>) {
    let raw = fs::read_to_string(account_dir.join(LOGIN_LOG)).unwrap_or_default();
    let uri = raw
        .split_whitespace()
        .map(|part| part.trim_matches(|ch: char| "()[]{}<>,.;\"'".contains(ch)))
        .find(|part| part.starts_with("https://") || part.starts_with("http://"))
        .map(str::to_owned);
    let code = raw
        .split_whitespace()
        .map(|part| part.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-'))
        .find(|part| {
            (5..=20).contains(&part.len())
                && part.contains('-')
                && part
                    .chars()
                    .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '-')
        })
        .map(str::to_owned);
    (uri, code)
}

fn ensure_grok_binary() -> Result<PathBuf, String> {
    if let Some(binary) = grok_binary() {
        return Ok(binary);
    }
    let download = Command::new("curl")
        .args([
            "-fsSL",
            "--proto",
            "=https",
            "--tlsv1.2",
            "https://x.ai/cli/install.sh",
        ])
        .output()
        .map_err(|error| format!("GrokInstallerDownloadFailed: {error}"))?;
    if !download.status.success() || download.stdout.is_empty() {
        return Err(format!(
            "GrokInstallerDownloadFailed: {}",
            String::from_utf8_lossy(&download.stderr).trim()
        ));
    }
    let mut child = Command::new("bash")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("GrokInstallerStartFailed: {error}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "GrokInstallerStdinUnavailable".to_string())?
        .write_all(&download.stdout)
        .map_err(|error| format!("GrokInstallerWriteFailed: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("GrokInstallerWaitFailed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "GrokInstallerFailed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    grok_binary().ok_or_else(|| "GrokInstallerMissingBinary".to_string())
}

pub fn remove(paths: &AgentPaths) -> Result<RemoteGrokStatus, String> {
    let credential_id = read_metadata(&account_dir(paths))
        .map(|metadata| metadata.credential_id)
        .unwrap_or_else(|_| "grok-cli".into());
    disable_refresh_timer();
    match fs::remove_dir_all(account_dir(paths)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    remove_credential(paths, &credential_id)?;
    status(paths)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    credential_id: String,
    last_refresh_at: Option<DateTime<Utc>>,
    detached_qualified: bool,
    last_error: Option<String>,
}

#[derive(Debug, Clone)]
struct CredentialCandidate {
    access_token: String,
    user_id: Option<String>,
    email: Option<String>,
    expires_at: Option<DateTime<Utc>>,
}

fn rotate_proxy_secret(
    paths: &AgentPaths,
    credential_id: &str,
    value: &Value,
) -> Result<(), String> {
    let candidate = best_credential(value).ok_or_else(|| "GrokCredentialMissing".to_string())?;
    let account_dir = account_dir(paths);
    let client_version = fs::read_to_string(account_dir.join("version.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| {
            value
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .or_else(grok_cli_version)
        .ok_or_else(|| "GrokClientVersionMissing".to_string())?;
    let agent_id = fs::read_to_string(account_dir.join("agent_id"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let user_id = candidate
        .user_id
        .clone()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "GrokUserIdentityMissing".to_string())?;
    let mut secret = serde_json::json!({
        "accessToken": candidate.access_token,
        "clientVersion": client_version,
        "userId": user_id,
    });
    if let Some(agent_id) = agent_id {
        secret["agentId"] = Value::String(agent_id);
    }
    let secret = serde_json::to_string(&secret).map_err(|error| error.to_string())?;
    put_credential(paths, credential_id, &secret)?;
    Ok(())
}

fn best_credential(value: &Value) -> Option<CredentialCandidate> {
    let mut output = Vec::new();
    collect_credentials(value, &mut output);
    output.sort_by_key(|candidate| candidate.expires_at);
    output.pop()
}

fn collect_credentials(value: &Value, output: &mut Vec<CredentialCandidate>) {
    match value {
        Value::Object(object) => {
            if let Some(token) = object
                .get("access_token")
                .or_else(|| object.get("key"))
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty())
            {
                output.push(CredentialCandidate {
                    access_token: token.into(),
                    user_id: object
                        .get("user_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    email: object
                        .get("email")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    expires_at: object
                        .get("expires_at")
                        .and_then(Value::as_str)
                        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
                        .map(|value| value.with_timezone(&Utc)),
                });
            }
            for child in object.values() {
                collect_credentials(child, output);
            }
        }
        Value::Array(values) => values
            .iter()
            .for_each(|child| collect_credentials(child, output)),
        _ => {}
    }
}

fn contains_refresh_token(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            (matches!(key.as_str(), "refresh_token" | "refreshToken")
                && value.as_str().is_some_and(|token| !token.is_empty()))
                || contains_refresh_token(value)
        }),
        Value::Array(values) => values.iter().any(contains_refresh_token),
        _ => false,
    }
}

fn account_dir(paths: &AgentPaths) -> PathBuf {
    paths.root.join("grok").join("default")
}

fn read_metadata(account_dir: &Path) -> Result<Metadata, String> {
    serde_json::from_slice(
        &fs::read(account_dir.join("metadata.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn grok_binary() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let managed = home.join(".grok/bin/grok");
    if managed.is_file() {
        return Some(managed);
    }
    let output = Command::new("sh")
        .args(["-lc", "command -v grok"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
}

fn grok_cli_version() -> Option<String> {
    let output = Command::new(grok_binary()?)
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .find(|part| {
            part.chars().next().is_some_and(|ch| ch.is_ascii_digit())
                && part.contains('.')
                && part.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
        })
        .map(str::to_owned)
}

#[cfg(target_os = "linux")]
fn install_refresh_timer() -> Result<(), String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory missing".to_string())?;
    let unit_dir = home.join(".config/systemd/user");
    fs::create_dir_all(&unit_dir).map_err(|error| error.to_string())?;
    atomic_write(
        &unit_dir.join(SERVICE),
        b"[Unit]\nDescription=Refresh Vellum remote Grok credential\n\n[Service]\nType=oneshot\nExecStart=%h/.local/bin/vellum-remote-agent refresh-grok\n",
        0o600,
    )?;
    atomic_write(
        &unit_dir.join(TIMER),
        b"[Unit]\nDescription=Refresh Vellum remote Grok credential\n\n[Timer]\nOnBootSec=2m\nOnUnitActiveSec=15m\nPersistent=true\n\n[Install]\nWantedBy=timers.target\n",
        0o600,
    )?;
    let reload = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let enable = Command::new("systemctl")
        .args(["--user", "enable", "--now", TIMER])
        .status();
    if reload.is_ok_and(|status| status.success()) && enable.is_ok_and(|status| status.success()) {
        Ok(())
    } else {
        Err("GrokRefreshTimerInstallFailed".into())
    }
}

#[cfg(not(target_os = "linux"))]
fn install_refresh_timer() -> Result<(), String> {
    Err("Grok refresh timer requires Linux systemd".into())
}

fn timer_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", TIMER])
        .status()
        .is_ok_and(|status| status.success())
}

fn disable_refresh_timer() {
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", TIMER])
        .status();
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    atomic_write(
        path,
        &serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?,
        0o600,
    )
}

fn atomic_write(path: &Path, bytes: &[u8], _mode: u32) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(_mode);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| error.to_string())?;
    fs::rename(temporary, path).map_err(|error| error.to_string())
}

fn set_private_dir(_path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_never_serializes_tokens() {
        let value = serde_json::json!({"session":{"access_token":"secret","refresh_token":"refresh","user_id":"u"}});
        assert!(contains_refresh_token(&value));
        let candidate = best_credential(&value).unwrap();
        assert_eq!(candidate.user_id.as_deref(), Some("u"));
        let status = RemoteGrokStatus {
            configured: true,
            credential_id: "grok-cli".into(),
            account: Some("u".into()),
            expires_at: None,
            last_refresh_at: None,
            detached_qualified: true,
            refresh_timer_active: true,
            login_pending: false,
            verification_uri: None,
            user_code: None,
            login_started_at: None,
            error: None,
        };
        let public = serde_json::to_string(&status).unwrap();
        assert!(!public.contains("secret"));
        assert!(!public.contains("\"refresh\""));
    }

    #[test]
    fn device_prompt_parser_returns_only_url_and_short_code() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join(LOGIN_LOG),
            "Open https://auth.x.ai/device and enter ABCD-EFGH\n",
        )
        .unwrap();
        let (uri, code) = login_prompt(temp.path());
        assert_eq!(uri.as_deref(), Some("https://auth.x.ai/device"));
        assert_eq!(code.as_deref(), Some("ABCD-EFGH"));
    }
}
