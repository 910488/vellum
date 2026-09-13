//! Native macOS proxy backend: `vellum-proxy-daemon serve` under LaunchAgent.
//!
//! Production talks to `launchctl` through [`LaunchdClient`]. Unit tests inject
//! a memory client so Windows can drive the same install/start/stop/status
//! path without a live launchd.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::protocol::{ProxyLogsView, ProxyStatusView};
use crate::state::{AgentPaths, AgentStateStore, InstallRecord};

pub const LAUNCH_AGENT_LABEL: &str = "com.vellum.remote.proxy";
pub const THROTTLE_INTERVAL_SECS: u32 = 10;
pub const NATIVE_LISTEN_HOST: &str = "127.0.0.1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchAgentSpec {
    pub label: String,
    pub executable: PathBuf,
    pub config: PathBuf,
    pub listen: String,
    pub stdout_log: PathBuf,
    pub stderr_log: PathBuf,
    pub state_root: PathBuf,
    pub throttle_interval_secs: u32,
}

impl LaunchAgentSpec {
    pub fn new(
        executable: PathBuf,
        config: PathBuf,
        listen: impl Into<String>,
        logs_dir: &Path,
        state_root: PathBuf,
    ) -> Self {
        Self {
            label: LAUNCH_AGENT_LABEL.into(),
            executable,
            config,
            listen: listen.into(),
            stdout_log: logs_dir.join("proxy.launchd.out.log"),
            stderr_log: logs_dir.join("proxy.launchd.err.log"),
            state_root,
            throttle_interval_secs: THROTTLE_INTERVAL_SECS,
        }
    }

    pub fn plist_path(launch_agents_dir: &Path) -> PathBuf {
        launch_agents_dir.join(format!("{LAUNCH_AGENT_LABEL}.plist"))
    }

    pub fn to_plist(&self) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{executable}</string>
    <string>serve</string>
    <string>--config</string>
    <string>{config}</string>
    <string>--listen</string>
    <string>{listen}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>{throttle}</integer>
  <key>StandardOutPath</key>
  <string>{stdout}</string>
  <key>StandardErrorPath</key>
  <string>{stderr}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>VELLUM_REMOTE_STATE_ROOT</key>
    <string>{state}</string>
  </dict>
</dict>
</plist>
"#,
            label = xml_escape(&self.label),
            executable = xml_escape(&self.executable.to_string_lossy()),
            config = xml_escape(&self.config.to_string_lossy()),
            listen = xml_escape(&self.listen),
            throttle = self.throttle_interval_secs,
            stdout = xml_escape(&self.stdout_log.to_string_lossy()),
            stderr = xml_escape(&self.stderr_log.to_string_lossy()),
            state = xml_escape(&self.state_root.to_string_lossy()),
        )
    }
}

pub fn launch_agents_dir(home: &Path) -> PathBuf {
    home.join("Library").join("LaunchAgents")
}

pub fn default_launch_agents_dir(paths: &AgentPaths) -> PathBuf {
    dirs::home_dir()
        .map(|home| launch_agents_dir(&home))
        .unwrap_or_else(|| paths.root.join("LaunchAgents"))
}

pub fn native_listen(port: u16) -> String {
    format!("{NATIVE_LISTEN_HOST}:{port}")
}

pub fn native_proxy_bin(paths: &AgentPaths) -> PathBuf {
    paths.root.join("bin").join("vellum-proxy-daemon")
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LaunchdJobStatus {
    pub loaded: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub program: Option<String>,
}

pub fn parse_launchctl_print(output: &str) -> LaunchdJobStatus {
    let missing = output.contains("Could not find service")
        || output.contains("Could not find domain")
        || output.trim().is_empty();
    if missing {
        return LaunchdJobStatus::default();
    }
    let running = output
        .lines()
        .any(|line| line.trim() == "state = running" || line.trim().starts_with("state = running"));
    let pid = output.lines().find_map(|line| {
        let line = line.trim();
        let value = line.strip_prefix("pid = ")?;
        value.trim().parse().ok()
    });
    let program = output.lines().find_map(|line| {
        let line = line.trim();
        let value = line.strip_prefix("program = ")?;
        Some(value.trim().to_string())
    });
    LaunchdJobStatus {
        loaded: true,
        running,
        pid,
        program,
    }
}

pub fn gui_domain_from_uid_output(stdout: &str) -> Result<String, String> {
    let uid = stdout.trim();
    if uid.is_empty() || !uid.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(format!("LaunchdUidInvalid: {uid}"));
    }
    Ok(format!("gui/{uid}"))
}

/// Port already bound by something that is not the managed proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortConflict {
    pub listen: String,
    pub code: &'static str,
}

pub fn port_conflict(listen: &str, in_use_by_managed: bool) -> Option<PortConflict> {
    if in_use_by_managed {
        None
    } else {
        Some(PortConflict {
            listen: listen.into(),
            code: "proxyPortConflict",
        })
    }
}

pub trait LaunchdClient: Send + Sync {
    fn load_agent(&self, domain: &str, plist: &Path) -> Result<(), String>;
    fn unload_agent(&self, domain: &str, label: &str) -> Result<(), String>;
    fn inspect_agent(&self, domain: &str, label: &str) -> Result<LaunchdJobStatus, String>;
    fn inspect_readyz(&self, host_port: u16, boundary_key: &str) -> Result<bool, String>;
    fn port_in_use(&self, host_port: u16) -> Result<bool, String>;
    fn read_logs(
        &self,
        stdout_log: &Path,
        stderr_log: &Path,
        max_bytes: usize,
    ) -> Result<ProxyLogsView, String>;
}

pub struct ProcessLaunchdClient;

impl ProcessLaunchdClient {
    pub fn domain() -> Result<String, String> {
        let output = Command::new("id")
            .arg("-u")
            .output()
            .map_err(|error| format!("LaunchdUidProbeFailed: {error}"))?;
        if !output.status.success() {
            return Err("LaunchdUidProbeFailed".into());
        }
        gui_domain_from_uid_output(&String::from_utf8_lossy(&output.stdout))
    }
}

impl LaunchdClient for ProcessLaunchdClient {
    fn load_agent(&self, domain: &str, plist: &Path) -> Result<(), String> {
        let plist_s = plist.to_string_lossy();
        let bootstrap = Command::new("launchctl")
            .args(["bootstrap", domain, plist_s.as_ref()])
            .output()
            .map_err(|error| format!("launchctl bootstrap failed to start: {error}"))?;
        if !bootstrap.status.success() {
            let stderr = String::from_utf8_lossy(&bootstrap.stderr);
            if !stderr.contains("already loaded") && !stderr.contains("service already loaded") {
                let load = Command::new("launchctl")
                    .args(["load", "-w", plist_s.as_ref()])
                    .output()
                    .map_err(|error| format!("launchctl load failed to start: {error}"))?;
                if !load.status.success() {
                    return Err(format!(
                        "LaunchAgentLoadFailed: {}",
                        String::from_utf8_lossy(&load.stderr).trim()
                    ));
                }
            }
        }
        let target = format!("{domain}/{LAUNCH_AGENT_LABEL}");
        let _ = Command::new("launchctl")
            .args(["kickstart", "-k", &target])
            .output();
        Ok(())
    }

    fn unload_agent(&self, domain: &str, label: &str) -> Result<(), String> {
        let target = format!("{domain}/{label}");
        let bootout = Command::new("launchctl")
            .args(["bootout", &target])
            .output()
            .map_err(|error| format!("launchctl bootout failed to start: {error}"))?;
        if bootout.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&bootout.stderr);
        if stderr.contains("No such process") || stderr.contains("Could not find") {
            return Ok(());
        }
        Err(format!("LaunchAgentUnloadFailed: {}", stderr.trim()))
    }

    fn inspect_agent(&self, domain: &str, label: &str) -> Result<LaunchdJobStatus, String> {
        let target = format!("{domain}/{label}");
        let output = Command::new("launchctl")
            .args(["print", &target])
            .output()
            .map_err(|error| format!("launchctl print failed to start: {error}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(parse_launchctl_print(&format!("{stdout}{stderr}")))
    }

    fn inspect_readyz(&self, host_port: u16, boundary_key: &str) -> Result<bool, String> {
        let url = format!("http://{NATIVE_LISTEN_HOST}:{host_port}/readyz");
        let header = format!(
            "{}: {boundary_key}",
            vellum_proxy_runtime::BOUNDARY_KEY_HEADER
        );
        let output = Command::new("curl")
            .args(["-sS", "-H", header.as_str(), url.as_str()])
            .output()
            .map_err(|error| format!("native readyz probe failed: {error}"))?;
        if !output.status.success() {
            return Ok(false);
        }
        let body: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("native readyz returned invalid JSON: {error}"))?;
        Ok(body
            .get("ready")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false))
    }

    fn port_in_use(&self, host_port: u16) -> Result<bool, String> {
        let output = Command::new("lsof")
            .args([
                "-nP",
                &format!("-iTCP:{host_port}"),
                "-sTCP:LISTEN",
            ])
            .output()
            .map_err(|error| format!("lsof failed to start: {error}"))?;
        Ok(output.status.success() && !output.stdout.is_empty())
    }

    fn read_logs(
        &self,
        stdout_log: &Path,
        stderr_log: &Path,
        max_bytes: usize,
    ) -> Result<ProxyLogsView, String> {
        read_launchd_log_files(stdout_log, stderr_log, max_bytes)
    }
}

#[derive(Debug, Default)]
pub struct MemoryLaunchd {
    pub loaded: Mutex<bool>,
    pub running: Mutex<bool>,
    pub pid: Mutex<Option<u32>>,
    pub ready: Mutex<bool>,
    pub loads: Mutex<Vec<PathBuf>>,
    pub unloads: Mutex<Vec<String>>,
    pub port_busy: Mutex<bool>,
    pub stdout_log: Mutex<String>,
    pub stderr_log: Mutex<String>,
    pub log_reads: Mutex<u32>,
}

impl LaunchdClient for MemoryLaunchd {
    fn load_agent(&self, _domain: &str, plist: &Path) -> Result<(), String> {
        self.loads.lock().unwrap().push(plist.to_path_buf());
        *self.loaded.lock().unwrap() = true;
        *self.running.lock().unwrap() = true;
        if self.pid.lock().unwrap().is_none() {
            *self.pid.lock().unwrap() = Some(4242);
        }
        Ok(())
    }

    fn unload_agent(&self, _domain: &str, label: &str) -> Result<(), String> {
        self.unloads.lock().unwrap().push(label.to_string());
        *self.loaded.lock().unwrap() = false;
        *self.running.lock().unwrap() = false;
        *self.pid.lock().unwrap() = None;
        *self.ready.lock().unwrap() = false;
        Ok(())
    }

    fn inspect_agent(&self, _domain: &str, _label: &str) -> Result<LaunchdJobStatus, String> {
        Ok(LaunchdJobStatus {
            loaded: *self.loaded.lock().unwrap(),
            running: *self.running.lock().unwrap(),
            pid: *self.pid.lock().unwrap(),
            program: None,
        })
    }

    fn inspect_readyz(&self, _host_port: u16, _boundary_key: &str) -> Result<bool, String> {
        Ok(*self.ready.lock().unwrap())
    }

    fn port_in_use(&self, _host_port: u16) -> Result<bool, String> {
        Ok(*self.port_busy.lock().unwrap())
    }

    fn read_logs(
        &self,
        _stdout_log: &Path,
        _stderr_log: &Path,
        _max_bytes: usize,
    ) -> Result<ProxyLogsView, String> {
        *self.log_reads.lock().unwrap() += 1;
        Ok(ProxyLogsView {
            source: "launchd".into(),
            stdout: self.stdout_log.lock().unwrap().clone(),
            stderr: self.stderr_log.lock().unwrap().clone(),
            truncated: false,
        })
    }
}

pub fn native_status_view(
    install: Option<&InstallRecord>,
    binary_present: bool,
    plist_present: bool,
    job: &LaunchdJobStatus,
    ready: bool,
    last_error: Option<String>,
) -> ProxyStatusView {
    ProxyStatusView {
        present: install.is_some() && (binary_present || plist_present || job.loaded),
        running: job.running,
        ready: job.running && ready,
        container_id: job.pid.map(|pid| pid.to_string()),
        image: None,
        image_digest: None,
        host_port: install.map(|item| item.host_port),
        install_id: install.map(|item| item.install_id.clone()),
        config_hash: install.map(|item| item.config_hash.clone()),
        last_error,
        proxy_backend: Some(crate::platform::PROXY_BACKEND_NATIVE.into()),
        native_executable_digest: install.and_then(|item| item.native_executable_digest.clone()),
    }
}

pub fn write_launch_agent_plist(
    spec: &LaunchAgentSpec,
    launch_agents_dir: &Path,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(launch_agents_dir)
        .map_err(|error| format!("LaunchAgentDirCreateFailed: {error}"))?;
    let path = LaunchAgentSpec::plist_path(launch_agents_dir);
    let plist = spec.to_plist();
    if !plist.contains("<string>serve</string>")
        || !plist.contains("RunAtLoad")
        || !spec.executable.is_absolute()
    {
        return Err("NativeProxyPlistInvalid: require absolute exec, serve, RunAtLoad".into());
    }
    std::fs::write(&path, plist.as_bytes())
        .map_err(|error| format!("LaunchAgentWriteFailed: {error}"))?;
    Ok(path)
}

pub fn observe_native(
    store: &AgentStateStore,
    launchd: &dyn LaunchdClient,
    launch_agents_dir: &Path,
    domain: &str,
    boundary_key: &str,
) -> Result<ProxyStatusView, String> {
    let state = store.load()?;
    let binary = native_proxy_bin(store.paths());
    let plist = LaunchAgentSpec::plist_path(launch_agents_dir);
    let job = launchd.inspect_agent(domain, LAUNCH_AGENT_LABEL)?;
    let port = state
        .install
        .as_ref()
        .map(|item| item.host_port)
        .unwrap_or(crate::platform::DARWIN_PROXY_PORT);
    let listen = native_listen(port);
    if !job.running {
        if launchd.port_in_use(port)? {
            if let Some(conflict) = port_conflict(&listen, false) {
                return Ok(native_status_view(
                    state.install.as_ref(),
                    binary.is_file(),
                    plist.is_file(),
                    &job,
                    false,
                    Some(conflict.code.into()),
                ));
            }
        }
        return Ok(native_status_view(
            state.install.as_ref(),
            binary.is_file(),
            plist.is_file(),
            &job,
            false,
            None,
        ));
    }
    let ready = launchd.inspect_readyz(port, boundary_key).unwrap_or(false);
    Ok(native_status_view(
        state.install.as_ref(),
        binary.is_file(),
        plist.is_file(),
        &job,
        ready,
        None,
    ))
}

pub fn start_native(
    store: &AgentStateStore,
    launchd: &dyn LaunchdClient,
    launch_agents_dir: &Path,
    domain: &str,
    host_port: Option<u16>,
    boundary_key: &str,
) -> Result<ProxyStatusView, String> {
    let mut state = store.load()?;
    let install = state
        .install
        .as_ref()
        .ok_or_else(|| "NativeProxyInstallMissing: install the native proxy first".to_string())?;
    let port = host_port.unwrap_or(install.host_port);
    let listen = native_listen(port);
    let job = launchd.inspect_agent(domain, LAUNCH_AGENT_LABEL)?;
    if launchd.port_in_use(port)? && !job.running {
        if let Some(conflict) = port_conflict(&listen, false) {
            return Err(format!(
                "{}: {listen} is already in use",
                conflict.code
            ));
        }
    }
    let paths = store.paths();
    let executable = native_proxy_bin(paths);
    if !executable.is_file() {
        return Err(format!(
            "NativeProxyBinaryMissing: {}",
            executable.display()
        ));
    }
    if !executable.is_absolute() {
        return Err("NativeProxyBinaryMustBeAbsolute".into());
    }
    let spec = LaunchAgentSpec::new(
        executable,
        paths.proxy_config_dir.join("proxy.toml"),
        listen,
        &paths.logs_dir,
        paths.root.clone(),
    );
    let plist = write_launch_agent_plist(&spec, launch_agents_dir)?;
    launchd.load_agent(domain, &plist)?;
    if let Some(install) = state.install.as_mut() {
        install.host_port = port;
        install.updated_at = chrono::Utc::now();
    }
    store.save(&state)?;
    observe_native(store, launchd, launch_agents_dir, domain, boundary_key)
}

pub fn stop_native(
    store: &AgentStateStore,
    launchd: &dyn LaunchdClient,
    launch_agents_dir: &Path,
    domain: &str,
    boundary_key: &str,
) -> Result<ProxyStatusView, String> {
    launchd.unload_agent(domain, LAUNCH_AGENT_LABEL)?;
    observe_native(store, launchd, launch_agents_dir, domain, boundary_key)
}

pub fn digest_native_binary(path: &Path) -> Result<String, String> {
    crate::update::sha256_file(path)
}

pub fn native_log_paths(paths: &AgentPaths) -> (PathBuf, PathBuf) {
    (
        paths.logs_dir.join("proxy.launchd.out.log"),
        paths.logs_dir.join("proxy.launchd.err.log"),
    )
}

pub fn read_launchd_log_files(
    stdout_log: &Path,
    stderr_log: &Path,
    max_bytes: usize,
) -> Result<ProxyLogsView, String> {
    let (stdout, trunc_out) = tail_file(stdout_log, max_bytes)?;
    let (stderr, trunc_err) = tail_file(stderr_log, max_bytes)?;
    Ok(ProxyLogsView {
        source: "launchd".into(),
        stdout,
        stderr,
        truncated: trunc_out || trunc_err,
    })
}

fn tail_file(path: &Path, max_bytes: usize) -> Result<(String, bool), String> {
    if !path.is_file() {
        return Ok((String::new(), false));
    }
    let data = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    if data.len() <= max_bytes {
        return Ok((String::from_utf8_lossy(&data).into_owned(), false));
    }
    let start = data.len().saturating_sub(max_bytes);
    Ok((
        String::from_utf8_lossy(&data[start..]).into_owned(),
        true,
    ))
}

pub fn logs_native(
    store: &AgentStateStore,
    launchd: &dyn LaunchdClient,
    max_bytes: usize,
) -> Result<ProxyLogsView, String> {
    let (stdout, stderr) = native_log_paths(store.paths());
    launchd.read_logs(&stdout, &stderr, max_bytes.max(1))
}

pub fn reload_native_if_loaded(
    store: &AgentStateStore,
    launchd: &dyn LaunchdClient,
    launch_agents_dir: &Path,
    domain: &str,
    boundary_key: &str,
) -> Result<ProxyStatusView, String> {
    let job = launchd.inspect_agent(domain, LAUNCH_AGENT_LABEL)?;
    if job.loaded || job.running {
        launchd.unload_agent(domain, LAUNCH_AGENT_LABEL)?;
        return start_native(
            store,
            launchd,
            launch_agents_dir,
            domain,
            None,
            boundary_key,
        );
    }
    observe_native(store, launchd, launch_agents_dir, domain, boundary_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AgentPaths;
    use chrono::Utc;

    #[test]
    fn launch_agent_plist_uses_absolute_exec_run_at_load_and_throttle() {
        let spec = LaunchAgentSpec::new(
            PathBuf::from("/Users/joshhuang/.local/bin/vellum-proxy-daemon"),
            PathBuf::from(
                "/Users/joshhuang/Library/Application Support/vellum-remote/proxy/config/config.toml",
            ),
            "127.0.0.1:15722",
            Path::new("/Users/joshhuang/Library/Application Support/vellum-remote/logs"),
            PathBuf::from("/Users/joshhuang/Library/Application Support/vellum-remote"),
        );
        let plist = spec.to_plist();
        assert!(plist.contains("<string>com.vellum.remote.proxy</string>"));
        assert!(plist.contains(
            "<string>/Users/joshhuang/.local/bin/vellum-proxy-daemon</string>"
        ));
        assert!(plist.contains("<string>serve</string>"));
        assert!(plist.contains("<string>127.0.0.1:15722</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(plist.contains("<key>SuccessfulExit</key>\n    <false/>"));
        assert!(plist.contains("<integer>10</integer>"));
        assert!(plist.contains("VELLUM_REMOTE_STATE_ROOT"));
        assert!(!plist.contains("/proc"));
        assert!(!plist.contains("docker"));
        let path = LaunchAgentSpec::plist_path(Path::new(
            "/Users/joshhuang/Library/LaunchAgents",
        ));
        assert_eq!(
            path,
            Path::new("/Users/joshhuang/Library/LaunchAgents/com.vellum.remote.proxy.plist")
        );
    }

    #[test]
    fn busy_unmanaged_port_is_an_explicit_conflict() {
        assert_eq!(
            port_conflict("127.0.0.1:15722", false)
                .unwrap()
                .code,
            "proxyPortConflict"
        );
        assert!(port_conflict("127.0.0.1:15722", true).is_none());
    }

    #[test]
    fn launchctl_print_sample_is_running_and_missing_is_not() {
        let sample = "\
gui/501/com.vellum.remote.proxy = {
	active count = 1
	path = /Users/joshhuang/Library/LaunchAgents/com.vellum.remote.proxy.plist
	state = running
	program = /Users/joshhuang/.vellum-remote/bin/vellum-proxy-daemon
	pid = 18841
}
";
        let job = parse_launchctl_print(sample);
        assert!(job.loaded && job.running);
        assert_eq!(job.pid, Some(18841));
        assert_eq!(
            job.program.as_deref(),
            Some("/Users/joshhuang/.vellum-remote/bin/vellum-proxy-daemon")
        );
        let missing = parse_launchctl_print(
            "Could not find service \"com.vellum.remote.proxy\" in domain for handle 501",
        );
        assert!(!missing.loaded && !missing.running && missing.pid.is_none());
    }

    #[test]
    fn gui_domain_is_gui_slash_uid() {
        assert_eq!(gui_domain_from_uid_output("501\n").unwrap(), "gui/501");
        assert!(gui_domain_from_uid_output("n/a").is_err());
    }

    #[test]
    fn start_stop_status_go_through_launchd_and_persist_15722() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        paths.ensure().unwrap();
        let bin = native_proxy_bin(&paths);
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"fake-daemon").unwrap();
        let digest = digest_native_binary(&bin).unwrap();
        let store = AgentStateStore::new(paths.clone());
        let mut state = store.load().unwrap();
        state.install = Some(InstallRecord {
            install_id: "install-n".into(),
            host_id: state.host_id.clone(),
            image: String::new(),
            image_digest: None,
            config_hash: "cfg".into(),
            host_port: crate::platform::DARWIN_PROXY_PORT,
            updated_at: Utc::now(),
            proxy_backend: Some(crate::platform::PROXY_BACKEND_NATIVE.into()),
            native_executable_digest: Some(digest.clone()),
        });
        store.save(&state).unwrap();

        let agents = temp.path().join("Library").join("LaunchAgents");
        let launchd = MemoryLaunchd {
            ready: Mutex::new(true),
            ..MemoryLaunchd::default()
        };
        let before = observe_native(&store, &launchd, &agents, "gui/501", "key").unwrap();
        assert!(!before.running);
        assert_eq!(before.host_port, Some(15722));
        assert!(before.image.is_none() || before.image.as_deref() == Some(""));

        let started =
            start_native(&store, &launchd, &agents, "gui/501", None, "key").unwrap();
        assert!(started.running);
        assert!(started.ready);
        assert_eq!(started.host_port, Some(15722));
        assert_eq!(started.native_executable_digest.as_deref(), Some(digest.as_str()));
        let loads = launchd.loads.lock().unwrap().clone();
        assert_eq!(loads.len(), 1);
        assert_eq!(
            loads[0],
            LaunchAgentSpec::plist_path(&agents)
        );
        let plist = std::fs::read_to_string(&loads[0]).unwrap();
        assert!(plist.contains("<string>serve</string>"));
        assert!(plist.contains("<string>127.0.0.1:15722</string>"));
        assert!(plist.contains(&bin.to_string_lossy().to_string()));
        assert!(plist.contains("RunAtLoad"));

        let stopped = stop_native(&store, &launchd, &agents, "gui/501", "key").unwrap();
        assert!(!stopped.running);
        assert!(!stopped.ready);
        assert!(LaunchAgentSpec::plist_path(&agents).is_file());
        assert_eq!(launchd.unloads.lock().unwrap().as_slice(), [LAUNCH_AGENT_LABEL]);
        assert_eq!(store.load().unwrap().install.unwrap().host_port, 15722);
    }

    #[test]
    fn start_refuses_an_unmanaged_busy_port() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        paths.ensure().unwrap();
        let bin = native_proxy_bin(&paths);
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, b"fake-daemon").unwrap();
        let store = AgentStateStore::new(paths);
        let mut state = store.load().unwrap();
        state.install = Some(InstallRecord {
            install_id: "install-n".into(),
            host_id: state.host_id.clone(),
            image: String::new(),
            image_digest: None,
            config_hash: "cfg".into(),
            host_port: 15722,
            updated_at: Utc::now(),
            proxy_backend: Some("native".into()),
            native_executable_digest: Some("a".repeat(64)),
        });
        store.save(&state).unwrap();
        let launchd = MemoryLaunchd {
            port_busy: Mutex::new(true),
            ..MemoryLaunchd::default()
        };
        let error = start_native(
            &store,
            &launchd,
            &temp.path().join("LaunchAgents"),
            "gui/501",
            None,
            "",
        )
        .unwrap_err();
        assert!(error.contains("proxyPortConflict"));
        assert!(launchd.loads.lock().unwrap().is_empty());
    }

    #[test]
    fn logs_go_through_launchd_not_docker_files() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        paths.ensure().unwrap();
        let store = AgentStateStore::new(paths);
        let launchd = MemoryLaunchd {
            stdout_log: Mutex::new("launchd stdout".into()),
            stderr_log: Mutex::new("launchd stderr".into()),
            ..MemoryLaunchd::default()
        };
        let logs = logs_native(&store, &launchd, 1024).unwrap();
        assert_eq!(logs.source, "launchd");
        assert_eq!(logs.stdout, "launchd stdout");
        assert_eq!(logs.stderr, "launchd stderr");
        assert_eq!(*launchd.log_reads.lock().unwrap(), 1);
    }
}
