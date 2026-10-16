//! Native proxy lifecycle under launchd (macOS) or systemd user (Linux).
//!
//! [`ProxyServiceClient`] keeps install/start/stop/status independent of the
//! host service manager. Unit tests inject memory clients and command runners.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::protocol::{ProxyLogsView, ProxyStatusView};
use crate::state::{AgentPaths, AgentStateStore, InstallRecord};

pub const LAUNCH_AGENT_LABEL: &str = "com.vellum.remote.proxy";
pub const THROTTLE_INTERVAL_SECS: u32 = 10;
pub const NATIVE_LISTEN_HOST: &str = "127.0.0.1";
pub const AUTO_PROXY_PORT_END: u16 = 15799;

/// Read-only candidate selection. A managed listener keeps its installed
/// port; no probe ever stops, signals or adopts an unrelated listener.
pub fn select_native_port(
    client: &dyn ProxyServiceClient,
    preferred: u16,
    managed_port: Option<u16>,
) -> Result<u16, String> {
    if let Some(port) = managed_port {
        if port == 0 {
            return Err("NativeProxyPortInvalid: managed port must be nonzero".into());
        }
        return Ok(port);
    }
    if preferred == 0 {
        return Err("NativeProxyPortInvalid: port must be nonzero".into());
    }
    if !client.port_in_use(preferred)? {
        return Ok(preferred);
    }
    for port in crate::platform::LINUX_PROXY_PORT..=AUTO_PROXY_PORT_END {
        if port != preferred && !client.port_in_use(port)? {
            return Ok(port);
        }
    }
    Err(format!(
        "proxyPortConflict: no free loopback port in {}-{AUTO_PROXY_PORT_END}",
        crate::platform::LINUX_PROXY_PORT
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

pub trait ProxyServiceClient: Send + Sync {
    fn stop_before_replace(&self) -> bool {
        false
    }
    fn service_path(&self, directory: &Path) -> PathBuf {
        LaunchAgentSpec::plist_path(directory)
    }
    fn write_service(&self, spec: &LaunchAgentSpec, directory: &Path) -> Result<PathBuf, String> {
        write_launch_agent_plist(spec, directory)
    }
    fn default_port(&self) -> u16 {
        crate::platform::DARWIN_PROXY_PORT
    }
    fn restart_after_update(&self, job: &LaunchdJobStatus) -> bool {
        job.loaded || job.running
    }
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

// Preserve the public name for existing launchd test clients.
pub use ProxyServiceClient as LaunchdClient;

pub struct ProcessLaunchdClient;

pub const SYSTEMD_PROXY_UNIT: &str = "vellum-proxy.service";

pub fn systemd_user_dir(home: &Path) -> PathBuf {
    home.join(".config/systemd/user")
}

fn systemd_quote(value: &str, exec: bool) -> Result<String, String> {
    if value.contains(['\n', '\r', '\0']) {
        return Err("NativeProxyUnitInvalid: control character in service argument".into());
    }
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    let escaped = if exec {
        escaped.replace('$', "$$")
    } else {
        escaped
    };
    Ok(format!("\"{escaped}\""))
}

pub fn systemd_unit(spec: &LaunchAgentSpec) -> Result<String, String> {
    if !spec.executable.is_absolute()
        || !spec.config.is_absolute()
        || !spec.state_root.is_absolute()
    {
        return Err("NativeProxyUnitInvalid: service paths must be absolute".into());
    }
    let executable = systemd_quote(&spec.executable.to_string_lossy(), true)?;
    let config = systemd_quote(&spec.config.to_string_lossy(), true)?;
    let listen = systemd_quote(&spec.listen, true)?;
    let environment = systemd_quote(
        &format!("VELLUM_REMOTE_STATE_ROOT={}", spec.state_root.display()),
        false,
    )?;
    Ok(format!(
        "[Unit]\nDescription=Vellum remote proxy\n\n[Service]\nType=simple\nExecStart={executable} serve --config {config} --listen {listen}\nEnvironment={environment}\nRestart=on-failure\nRestartSec=10\nUMask=0077\nStandardOutput=journal\nStandardError=journal\n\n[Install]\nWantedBy=default.target\n"
    ))
}

#[derive(Debug, Default, Clone)]
pub struct ServiceCommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Injectable command boundary: tests never invoke the host service manager.
pub trait ServiceCommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[&str]) -> Result<ServiceCommandOutput, String>;
}

pub struct ProcessServiceCommandRunner;

impl ServiceCommandRunner for ProcessServiceCommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> Result<ServiceCommandOutput, String> {
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|error| format!("NativeProxyServiceCommandFailed: {program}: {error}"))?;
        Ok(ServiceCommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

pub struct SystemdUserClient {
    runner: std::sync::Arc<dyn ServiceCommandRunner>,
}

impl Default for SystemdUserClient {
    fn default() -> Self {
        Self::with_runner(std::sync::Arc::new(ProcessServiceCommandRunner))
    }
}

impl SystemdUserClient {
    pub fn with_runner(runner: std::sync::Arc<dyn ServiceCommandRunner>) -> Self {
        Self { runner }
    }

    fn checked(&self, program: &str, args: &[&str]) -> Result<ServiceCommandOutput, String> {
        let output = self.runner.run(program, args)?;
        if !output.success {
            return Err(format!(
                "NativeProxyServiceFailed: {program} {}: {}",
                args.join(" "),
                output.stderr.trim()
            ));
        }
        Ok(output)
    }
}

pub fn parse_systemd_show(output: &str) -> LaunchdJobStatus {
    let property = |name: &str| output.lines().find_map(|line| line.strip_prefix(name));
    let loaded = property("LoadState=") == Some("loaded");
    let pid = property("MainPID=")
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid > 0);
    LaunchdJobStatus {
        loaded,
        running: loaded && property("ActiveState=") == Some("active") && pid.is_some(),
        pid,
        program: None,
    }
}

impl ProxyServiceClient for SystemdUserClient {
    fn restart_after_update(&self, job: &LaunchdJobStatus) -> bool {
        // LoadState=loaded only means the unit file exists. A stopped service
        // must stay stopped until configure/credentials have converged.
        job.running
    }
    fn service_path(&self, directory: &Path) -> PathBuf {
        directory.join(SYSTEMD_PROXY_UNIT)
    }

    fn write_service(&self, spec: &LaunchAgentSpec, directory: &Path) -> Result<PathBuf, String> {
        let contents = systemd_unit(spec)?;
        std::fs::create_dir_all(directory)
            .map_err(|error| format!("NativeProxyUnitWriteFailed: {error}"))?;
        let path = self.service_path(directory);
        std::fs::write(&path, contents)
            .map_err(|error| format!("NativeProxyUnitWriteFailed: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| format!("NativeProxyUnitWriteFailed: {error}"))?;
        }
        Ok(path)
    }

    fn default_port(&self) -> u16 {
        crate::platform::LINUX_PROXY_PORT
    }

    fn load_agent(&self, _domain: &str, _unit: &Path) -> Result<(), String> {
        self.checked("systemctl", &["--user", "daemon-reload"])?;
        self.checked(
            "systemctl",
            &["--user", "enable", "--now", SYSTEMD_PROXY_UNIT],
        )?;
        Ok(())
    }

    fn unload_agent(&self, domain: &str, label: &str) -> Result<(), String> {
        if !self.inspect_agent(domain, label)?.loaded {
            return Ok(());
        }
        self.checked(
            "systemctl",
            &["--user", "disable", "--now", SYSTEMD_PROXY_UNIT],
        )?;
        Ok(())
    }

    fn inspect_agent(&self, _domain: &str, _label: &str) -> Result<LaunchdJobStatus, String> {
        let output = self.runner.run(
            "systemctl",
            &[
                "--user",
                "show",
                SYSTEMD_PROXY_UNIT,
                "--property=LoadState,ActiveState,MainPID",
                "--no-pager",
            ],
        )?;
        // A missing unit is observable; an unavailable user bus is an error.
        if !output.success
            && !output
                .stdout
                .lines()
                .any(|line| line == "LoadState=not-found")
        {
            return Err(format!(
                "NativeProxyServiceProbeFailed: {}",
                output.stderr.trim()
            ));
        }
        Ok(parse_systemd_show(&output.stdout))
    }

    fn inspect_readyz(&self, port: u16, key: &str) -> Result<bool, String> {
        ProcessLaunchdClient.inspect_readyz(port, key)
    }

    fn port_in_use(&self, port: u16) -> Result<bool, String> {
        match std::net::TcpListener::bind((NATIVE_LISTEN_HOST, port)) {
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => Ok(true),
            Err(error) => Err(format!("NativeProxyPortProbeFailed: {error}")),
        }
    }

    fn read_logs(
        &self,
        _stdout: &Path,
        _stderr: &Path,
        max_bytes: usize,
    ) -> Result<ProxyLogsView, String> {
        let output = self.checked(
            "journalctl",
            &[
                "--user",
                "--unit",
                SYSTEMD_PROXY_UNIT,
                "--no-pager",
                "--output=cat",
                "--lines=200",
            ],
        )?;
        let bytes = output.stdout.as_bytes();
        let start = bytes.len().saturating_sub(max_bytes);
        Ok(ProxyLogsView {
            source: "systemdUser".into(),
            stdout: String::from_utf8_lossy(&bytes[start..]).into_owned(),
            stderr: String::new(),
            truncated: start > 0,
        })
    }
}

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
            .args(["-nP", &format!("-iTCP:{host_port}"), "-sTCP:LISTEN"])
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
    pub systemd: bool,
    pub loaded: Mutex<bool>,
    pub running: Mutex<bool>,
    pub pid: Mutex<Option<u32>>,
    pub ready: Mutex<bool>,
    pub loads: Mutex<Vec<PathBuf>>,
    pub unloads: Mutex<Vec<String>>,
    pub port_busy: Mutex<bool>,
    pub busy_ports: Mutex<std::collections::BTreeSet<u16>>,
    pub stdout_log: Mutex<String>,
    pub stderr_log: Mutex<String>,
    pub log_reads: Mutex<u32>,
}

impl LaunchdClient for MemoryLaunchd {
    fn restart_after_update(&self, job: &LaunchdJobStatus) -> bool {
        if self.systemd {
            job.running
        } else {
            job.loaded || job.running
        }
    }
    fn service_path(&self, directory: &Path) -> PathBuf {
        if self.systemd {
            directory.join(SYSTEMD_PROXY_UNIT)
        } else {
            LaunchAgentSpec::plist_path(directory)
        }
    }
    fn write_service(&self, spec: &LaunchAgentSpec, directory: &Path) -> Result<PathBuf, String> {
        if self.systemd {
            SystemdUserClient::default().write_service(spec, directory)
        } else {
            write_launch_agent_plist(spec, directory)
        }
    }
    fn default_port(&self) -> u16 {
        if self.systemd {
            crate::platform::LINUX_PROXY_PORT
        } else {
            crate::platform::DARWIN_PROXY_PORT
        }
    }
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

    fn port_in_use(&self, host_port: u16) -> Result<bool, String> {
        Ok(*self.port_busy.lock().unwrap() || self.busy_ports.lock().unwrap().contains(&host_port))
    }

    fn read_logs(
        &self,
        _stdout_log: &Path,
        _stderr_log: &Path,
        _max_bytes: usize,
    ) -> Result<ProxyLogsView, String> {
        *self.log_reads.lock().unwrap() += 1;
        Ok(ProxyLogsView {
            source: if self.systemd {
                "systemdUser"
            } else {
                "launchd"
            }
            .into(),
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
    let plist = launchd.service_path(launch_agents_dir);
    let job = launchd.inspect_agent(domain, LAUNCH_AGENT_LABEL)?;
    let port = state
        .install
        .as_ref()
        .map(|item| item.host_port)
        .unwrap_or_else(|| launchd.default_port());
    if !job.running {
        if let Err(error) = select_native_port(launchd, port, None) {
            if error.starts_with("proxyPortConflict:") {
                return Ok(native_status_view(
                    state.install.as_ref(),
                    binary.is_file(),
                    plist.is_file(),
                    &job,
                    false,
                    Some("proxyPortConflict".into()),
                ));
            }
            return Err(error);
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
    let job = launchd.inspect_agent(domain, LAUNCH_AGENT_LABEL)?;
    if job.running {
        return observe_native(store, launchd, launch_agents_dir, domain, boundary_key);
    }
    let port = select_native_port(launchd, host_port.unwrap_or(install.host_port), None)?;
    let listen = native_listen(port);
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
    if let Some(config_hash) = crate::configuration::configure_native_listen(paths, port)? {
        if let Some(install) = state.install.as_mut() {
            install.config_hash = config_hash;
        }
    }
    let spec = LaunchAgentSpec::new(
        executable,
        paths.proxy_config_dir.join("proxy.toml"),
        listen,
        &paths.logs_dir,
        paths.root.clone(),
    );
    let plist = launchd.write_service(&spec, launch_agents_dir)?;
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
    Ok((String::from_utf8_lossy(&data[start..]).into_owned(), true))
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
    if launchd.restart_after_update(&job) {
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

    #[test]
    fn auto_port_selection_skips_busy_ports_and_only_conflicts_when_exhausted() {
        let service = MemoryLaunchd::default();
        assert_eq!(select_native_port(&service, 15721, None).unwrap(), 15721);
        service
            .busy_ports
            .lock()
            .unwrap()
            .extend([15721, 15722, 15723]);
        assert_eq!(select_native_port(&service, 15721, None).unwrap(), 15724);
        assert_eq!(select_native_port(&service, 15780, None).unwrap(), 15780);
        service.busy_ports.lock().unwrap().extend(15721..15799);
        assert_eq!(select_native_port(&service, 15721, None).unwrap(), 15799);
        *service.port_busy.lock().unwrap() = true;
        assert_eq!(
            select_native_port(&service, 15721, Some(15770)).unwrap(),
            15770
        );
        assert!(select_native_port(&service, 15721, None)
            .unwrap_err()
            .starts_with("proxyPortConflict:"));
        assert!(select_native_port(&service, 0, None)
            .unwrap_err()
            .starts_with("NativeProxyPortInvalid:"));
    }

    #[test]
    fn plan_start_race_reselects_and_persists_the_actual_port_and_config() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().join("state"));
        paths.ensure().unwrap();
        let binary = native_proxy_bin(&paths);
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, b"fake-daemon").unwrap();
        let store = AgentStateStore::new(paths.clone());
        let service = std::sync::Arc::new(MemoryLaunchd {
            systemd: true,
            ready: Mutex::new(true),
            ..Default::default()
        });
        let manager = crate::proxy::ProxyManager::with_native(
            store.clone(),
            service.clone(),
            temp.path().join("units"),
            "user",
        );
        manager.install("install", "", None).unwrap();
        let before = std::fs::read(&paths.state_path).unwrap();
        service.busy_ports.lock().unwrap().extend([15721, 15722]);
        let planned = manager.select_port(None).unwrap();
        assert_eq!(planned, 15723);
        assert_eq!(
            std::fs::read(&paths.state_path).unwrap(),
            before,
            "planning is read-only"
        );
        service.busy_ports.lock().unwrap().insert(planned);
        let started = manager
            .start(crate::proxy::ProxyStartRequest {
                operation_id: "start".into(),
                host_port: Some(planned),
                image: None,
            })
            .unwrap();
        let view: ProxyStatusView = serde_json::from_value(started.detail).unwrap();
        assert_eq!(view.host_port, Some(15724));
        let install = store.load().unwrap().install.unwrap();
        assert_eq!(install.host_port, 15724);
        let config = vellum_proxy_runtime::ProxyRuntimeConfig::from_toml_str(
            &std::fs::read_to_string(paths.proxy_config_dir.join("proxy.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(config.listen.to_string(), "127.0.0.1:15724");
        assert_eq!(config.identity.config_hash, install.config_hash);
        assert_eq!(
            manager.select_port(Some(15721)).unwrap(),
            15724,
            "keep a running managed listener"
        );
        assert_eq!(
            manager
                .start(crate::proxy::ProxyStartRequest {
                    operation_id: "start-again".into(),
                    host_port: Some(15721),
                    image: None
                })
                .unwrap()
                .detail["hostPort"],
            15724
        );
        // This is the exact provider table used for Codex catalog injection;
        // test it without invoking host probes or a real service manager.
        let providers = crate::profile::model_providers_table(&view).unwrap();
        assert_eq!(
            providers["vellum"]["base_url"].as_str(),
            Some("http://127.0.0.1:15724/v1")
        );
        assert_eq!(
            config.models[0].base_url, "http://127.0.0.1:1/v1",
            "upstream endpoints must remain unchanged"
        );
    }
    use crate::state::AgentPaths;
    use chrono::Utc;

    #[derive(Default)]
    struct FakeServiceCommands {
        calls: Mutex<Vec<String>>,
        outputs: Mutex<std::collections::VecDeque<ServiceCommandOutput>>,
    }

    impl ServiceCommandRunner for FakeServiceCommands {
        fn run(&self, program: &str, args: &[&str]) -> Result<ServiceCommandOutput, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
            Ok(self
                .outputs
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected service command"))
        }
    }

    fn command_output(success: bool, stdout: &str, stderr: &str) -> ServiceCommandOutput {
        ServiceCommandOutput {
            success,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn systemd_unit_quotes_paths_and_rejects_injected_directives() {
        let temp = tempfile::tempdir().unwrap();
        let mut spec = LaunchAgentSpec::new(
            temp.path().join("space $HOME %i/bin/proxy"),
            temp.path().join("config/proxy.toml"),
            "127.0.0.1:15721",
            temp.path(),
            temp.path().to_path_buf(),
        );
        let unit = systemd_unit(&spec).unwrap();
        assert!(unit.contains("$$HOME %%i"));
        assert!(unit.contains("serve --config \""));
        assert!(unit.contains("--listen \"127.0.0.1:15721\""));
        assert!(unit.contains("Restart=on-failure\nRestartSec=10\nUMask=0077"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(!unit.contains("docker"));
        spec.listen = "127.0.0.1:15721\nExecStart=/bad".into();
        assert!(systemd_unit(&spec)
            .unwrap_err()
            .contains("control character"));
    }

    #[test]
    fn systemd_start_status_stop_and_logs_use_injected_commands() {
        let runner = std::sync::Arc::new(FakeServiceCommands::default());
        runner.outputs.lock().unwrap().extend([
            command_output(true, "", ""), // daemon-reload
            command_output(true, "", ""), // enable --now
            command_output(
                true,
                "LoadState=loaded\nActiveState=active\nMainPID=123\n",
                "",
            ),
            command_output(
                true,
                "LoadState=loaded\nActiveState=active\nMainPID=123\n",
                "",
            ),
            command_output(true, "", ""), // disable --now
            command_output(true, "log line", ""),
        ]);
        let client = SystemdUserClient::with_runner(runner.clone());
        client.load_agent("user", Path::new("ignored")).unwrap();
        let job = client.inspect_agent("user", LAUNCH_AGENT_LABEL).unwrap();
        assert!(job.loaded && job.running);
        assert_eq!(job.pid, Some(123));
        client.unload_agent("user", LAUNCH_AGENT_LABEL).unwrap();
        let logs = client
            .read_logs(Path::new("ignored"), Path::new("ignored"), 4)
            .unwrap();
        assert_eq!(logs.source, "systemdUser");
        assert_eq!(logs.stdout, "line");
        assert!(logs.truncated);
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls[0], "systemctl --user daemon-reload");
        assert_eq!(
            calls[1],
            "systemctl --user enable --now vellum-proxy.service"
        );
        assert_eq!(
            calls[4],
            "systemctl --user disable --now vellum-proxy.service"
        );
        assert!(calls[5].starts_with("journalctl --user --unit vellum-proxy.service"));
    }

    #[test]
    fn systemd_missing_unit_is_idle_but_unavailable_bus_and_enable_failure_are_errors() {
        let runner = std::sync::Arc::new(FakeServiceCommands::default());
        runner.outputs.lock().unwrap().extend([
            command_output(
                false,
                "LoadState=not-found\nActiveState=inactive\nMainPID=0\n",
                "",
            ),
            command_output(false, "", "Failed to connect to bus"),
            command_output(true, "", ""),
            command_output(false, "", "permission denied"),
        ]);
        let client = SystemdUserClient::with_runner(runner);
        assert_eq!(
            client.inspect_agent("user", "").unwrap(),
            LaunchdJobStatus::default()
        );
        assert!(client
            .inspect_agent("user", "")
            .unwrap_err()
            .contains("Failed to connect to bus"));
        assert!(client
            .load_agent("user", Path::new("unit"))
            .unwrap_err()
            .contains("permission denied"));
        let stopped = parse_systemd_show("LoadState=loaded\nActiveState=inactive\nMainPID=0\n");
        assert!(!stopped.running);
        assert!(!client.restart_after_update(&stopped));
        assert!(ProcessLaunchdClient.restart_after_update(&stopped));
        let failed = parse_systemd_show("LoadState=loaded\nActiveState=failed\nMainPID=123\n");
        assert!(!failed.running);
    }

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
        assert!(plist.contains("<string>/Users/joshhuang/.local/bin/vellum-proxy-daemon</string>"));
        assert!(plist.contains("<string>serve</string>"));
        assert!(plist.contains("<string>127.0.0.1:15722</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(plist.contains("<key>SuccessfulExit</key>\n    <false/>"));
        assert!(plist.contains("<integer>10</integer>"));
        assert!(plist.contains("VELLUM_REMOTE_STATE_ROOT"));
        assert!(!plist.contains("/proc"));
        assert!(!plist.contains("docker"));
        let path = LaunchAgentSpec::plist_path(Path::new("/Users/joshhuang/Library/LaunchAgents"));
        assert_eq!(
            path,
            Path::new("/Users/joshhuang/Library/LaunchAgents/com.vellum.remote.proxy.plist")
        );
    }

    #[test]
    fn busy_unmanaged_port_is_an_explicit_conflict() {
        assert_eq!(
            port_conflict("127.0.0.1:15722", false).unwrap().code,
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

        let started = start_native(&store, &launchd, &agents, "gui/501", None, "key").unwrap();
        assert!(started.running);
        assert!(started.ready);
        assert_eq!(started.host_port, Some(15722));
        assert_eq!(
            started.native_executable_digest.as_deref(),
            Some(digest.as_str())
        );
        let loads = launchd.loads.lock().unwrap().clone();
        assert_eq!(loads.len(), 1);
        assert_eq!(loads[0], LaunchAgentSpec::plist_path(&agents));
        let plist = std::fs::read_to_string(&loads[0]).unwrap();
        assert!(plist.contains("<string>serve</string>"));
        assert!(plist.contains("<string>127.0.0.1:15722</string>"));
        assert!(plist.contains(&bin.to_string_lossy().to_string()));
        assert!(plist.contains("RunAtLoad"));

        let stopped = stop_native(&store, &launchd, &agents, "gui/501", "key").unwrap();
        assert!(!stopped.running);
        assert!(!stopped.ready);
        assert!(LaunchAgentSpec::plist_path(&agents).is_file());
        assert_eq!(
            launchd.unloads.lock().unwrap().as_slice(),
            [LAUNCH_AGENT_LABEL]
        );
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
