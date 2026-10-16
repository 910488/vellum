//! Agent-owned Linux proxy supervision when there is no systemd user manager.
//! PID files authorize only an exact uid, executable, argv and start-time match.

use crate::native_proxy::{LaunchAgentSpec, LaunchdJobStatus, ProxyServiceClient};
use crate::state::AgentPaths;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub uid: u32,
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub start_time: String,
    pub boot_id: String,
}

#[derive(Debug, Clone)]
pub struct ProcessSpec {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub state_root: PathBuf,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
}

pub trait ManagedProcessClient: Send + Sync {
    fn uid(&self) -> u32;
    fn inspect(&self, pid: u32) -> Result<Option<ProcessIdentity>, String>;
    fn spawn_detached(&self, spec: &ProcessSpec) -> Result<ProcessIdentity, String>;
    fn terminate(&self, identity: &ProcessIdentity) -> Result<(), String>;
}

pub struct LinuxProcessClient;

#[cfg(target_os = "linux")]
impl ManagedProcessClient for LinuxProcessClient {
    fn uid(&self) -> u32 {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() }
    }

    fn inspect(&self, pid: u32) -> Result<Option<ProcessIdentity>, String> {
        if pid == 0 || pid > i32::MAX as u32 {
            return Err("OnDemandPidInvalid".into());
        }
        let root = PathBuf::from(format!("/proc/{pid}"));
        let result = (|| {
            let stat = match std::fs::read_to_string(root.join("stat")) {
                Ok(stat) => stat,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(format!("OnDemandProcessProbeFailed: {error}")),
            };
            let fields: Vec<_> = stat
                .rsplit_once(')')
                .ok_or("OnDemandProcessStatInvalid")?
                .1
                .split_whitespace()
                .collect();
            if fields.first() == Some(&"Z") {
                return Ok(None);
            }
            let start_time = fields
                .get(19)
                .ok_or("OnDemandProcessStatInvalid")?
                .to_string();
            let status = std::fs::read_to_string(root.join("status")).map_err(|e| e.to_string())?;
            let uid = status
                .lines()
                .find_map(|line| line.strip_prefix("Uid:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|uid| uid.parse().ok())
                .ok_or("OnDemandProcessUidInvalid")?;
            let executable = std::fs::read_link(root.join("exe")).map_err(|e| e.to_string())?;
            let cmdline = std::fs::read(root.join("cmdline")).map_err(|e| e.to_string())?;
            let mut argv = cmdline
                .split(|byte| *byte == 0)
                .filter(|arg| !arg.is_empty());
            argv.next();
            let args = argv
                .map(|arg| String::from_utf8(arg.to_vec()).map_err(|e| e.to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .map_err(|e| e.to_string())?
                .trim()
                .to_string();
            if boot_id.is_empty() {
                return Err("OnDemandBootIdUnavailable".into());
            }
            Ok(Some(ProcessIdentity {
                pid,
                uid,
                executable,
                args,
                start_time,
                boot_id,
            }))
        })();
        // Exit/reap can race a multi-file procfs observation. Disappearance
        // is a dead process, while permission or identity errors fail closed.
        if result.is_err() {
            match std::fs::read_to_string(root.join("stat")) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Ok(stat)
                    if stat.rsplit_once(')').is_some_and(|(_, fields)| {
                        fields.split_whitespace().next() == Some("Z")
                    }) =>
                {
                    return Ok(None)
                }
                _ => {}
            }
        }
        result
    }

    fn spawn_detached(&self, spec: &ProcessSpec) -> Result<ProcessIdentity, String> {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let mut command = Command::new(&spec.executable);
        command
            .args(&spec.args)
            .env("VELLUM_REMOTE_STATE_ROOT", &spec.state_root)
            .stdin(Stdio::null())
            .stdout(private_log(&spec.stdout)?)
            .stderr(private_log(&spec.stderr)?);
        // SAFETY: setsid is async-signal-safe and the closure captures no state.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("OnDemandSpawnFailed: {e}"))?;
        let pid = child.id();
        for _ in 0..50 {
            if let Ok(Some(identity)) = self.inspect(pid) {
                if matches_spec(&identity, spec, self.uid()) {
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                    return Ok(identity);
                }
            }
            if child.try_wait().map_err(|e| e.to_string())?.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        Err("OnDemandSpawnIdentityUnavailable".into())
    }

    fn terminate(&self, identity: &ProcessIdentity) -> Result<(), String> {
        if self.inspect(identity.pid)?.as_ref() != Some(identity) || identity.uid != self.uid() {
            return Err("OnDemandProcessIdentityMismatch: refusing to signal process".into());
        }
        // SAFETY: signal only the positive PID just verified against its record.
        if unsafe { libc::kill(identity.pid as libc::pid_t, libc::SIGTERM) } != 0 {
            return Err(format!(
                "OnDemandStopFailed: {}",
                std::io::Error::last_os_error()
            ));
        }
        for _ in 0..100 {
            if self.inspect(identity.pid)?.as_ref() != Some(identity) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Err("OnDemandStopTimeout: process did not exit after SIGTERM".into())
    }
}

#[cfg(not(target_os = "linux"))]
impl ManagedProcessClient for LinuxProcessClient {
    fn uid(&self) -> u32 {
        0
    }
    fn inspect(&self, _pid: u32) -> Result<Option<ProcessIdentity>, String> {
        Err("OnDemandLinuxOnly".into())
    }
    fn spawn_detached(&self, _spec: &ProcessSpec) -> Result<ProcessIdentity, String> {
        Err("OnDemandLinuxOnly".into())
    }
    fn terminate(&self, _identity: &ProcessIdentity) -> Result<(), String> {
        Err("OnDemandLinuxOnly".into())
    }
}

#[cfg(target_os = "linux")]
fn private_log(path: &Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| e.to_string())
}

fn matches_spec(identity: &ProcessIdentity, spec: &ProcessSpec, uid: u32) -> bool {
    identity.pid > 0
        && identity.pid <= i32::MAX as u32
        && identity.uid == uid
        && identity.executable == spec.executable
        && identity.args == spec.args
        && !identity.start_time.is_empty()
        && !identity.boot_id.is_empty()
}

fn read_record(path: &Path) -> Result<Option<ProcessIdentity>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| format!("OnDemandPidRecordInvalid: {e}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("OnDemandPidRecordUnreadable: {error}")),
    }
}

fn private_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    crate::configuration::atomic_write(
        path,
        &serde_json::to_vec(value).map_err(|e| e.to_string())?,
        0o600,
    )
}

fn recorded_process(
    client: &dyn ManagedProcessClient,
    spec: &ProcessSpec,
    path: &Path,
) -> Result<Option<ProcessIdentity>, String> {
    let Some(record) = read_record(path)? else {
        return Ok(None);
    };
    if !matches_spec(&record, spec, client.uid()) {
        return Err("OnDemandProcessRecordMismatch".into());
    }
    match client.inspect(record.pid)? {
        None => Ok(None),
        Some(live) if live == record => Ok(Some(live)),
        // PID reuse is not authority to adopt or stop a different process.
        Some(_) => Ok(None),
    }
}

fn ensure_process(
    client: &dyn ManagedProcessClient,
    spec: &ProcessSpec,
    path: &Path,
) -> Result<ProcessIdentity, String> {
    if let Some(identity) = recorded_process(client, spec, path)? {
        return Ok(identity);
    }
    let identity = client.spawn_detached(spec)?;
    if !matches_spec(&identity, spec, client.uid()) {
        return Err("OnDemandSpawnIdentityMismatch".into());
    }
    if let Err(error) = private_json(path, &identity) {
        client.terminate(&identity)?;
        return Err(error);
    }
    Ok(identity)
}

pub struct OnDemandClient {
    paths: AgentPaths,
    client: Arc<dyn ManagedProcessClient>,
    probes: Arc<dyn ProxyServiceClient>,
}

impl OnDemandClient {
    pub fn new(paths: AgentPaths) -> Self {
        Self {
            paths,
            client: Arc::new(LinuxProcessClient),
            probes: Arc::new(crate::native_proxy::SystemdUserClient::default()),
        }
    }

    fn spec_path(&self) -> PathBuf {
        self.paths.agent_dir.join("on-demand-proxy.json")
    }
    fn enabled_path(&self) -> PathBuf {
        self.paths.agent_dir.join("on-demand-enabled")
    }
    fn pid_path(&self) -> PathBuf {
        self.paths.agent_dir.join("on-demand-proxy.pid.json")
    }

    fn proxy_spec(&self) -> Result<ProcessSpec, String> {
        let spec: LaunchAgentSpec =
            serde_json::from_slice(&std::fs::read(self.spec_path()).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        let executable = std::fs::canonicalize(crate::native_proxy::native_proxy_bin(&self.paths))
            .map_err(|e| e.to_string())?;
        let (stdout, stderr) = crate::native_proxy::native_log_paths(&self.paths);
        let listen: std::net::SocketAddr =
            spec.listen.parse().map_err(|_| "OnDemandListenInvalid")?;
        if spec.executable != crate::native_proxy::native_proxy_bin(&self.paths)
            || spec.config != self.paths.proxy_config_dir.join("proxy.toml")
            || spec.state_root != self.paths.root
            || spec.stdout_log != stdout
            || spec.stderr_log != stderr
            || listen.ip() != std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
            || listen.port() == 0
        {
            return Err("OnDemandServiceSpecMismatch".into());
        }
        Ok(ProcessSpec {
            executable,
            args: vec![
                "serve".into(),
                "--config".into(),
                spec.config.to_string_lossy().into_owned(),
                "--listen".into(),
                spec.listen,
            ],
            state_root: self.paths.root.clone(),
            stdout: spec.stdout_log,
            stderr: spec.stderr_log,
        })
    }

    fn supervisor_spec(&self) -> Result<ProcessSpec, String> {
        Ok(ProcessSpec {
            executable: std::fs::canonicalize(std::env::current_exe().map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?,
            args: vec![
                "--state-root".into(),
                self.paths.root.to_string_lossy().into_owned(),
                "supervise".into(),
            ],
            state_root: self.paths.root.clone(),
            stdout: self.paths.logs_dir.join("supervisor.out.log"),
            stderr: self.paths.logs_dir.join("supervisor.err.log"),
        })
    }

    fn ensure_supervisor(&self) -> Result<(), String> {
        let spec = self.supervisor_spec()?;
        ensure_process(
            self.client.as_ref(),
            &spec,
            &self.paths.agent_dir.join("supervisor.pid.json"),
        )?;
        Ok(())
    }

    pub fn supervisor_ready(&self) -> Result<bool, String> {
        if !self.enabled_path().is_file() {
            return Ok(false);
        }
        Ok(recorded_process(
            self.client.as_ref(),
            &self.supervisor_spec()?,
            &self.paths.agent_dir.join("supervisor.pid.json"),
        )?
        .is_some())
    }

    /// Called by the dedicated SSH wrapper. Does not enable a stopped install.
    pub fn ensure_enabled(&self) -> Result<(), String> {
        let _guard = crate::mutation_lock::HostMutationGuard::acquire(&self.paths.mutation_lock)?;
        if !self.enabled_path().is_file() {
            return Ok(());
        }
        let _launch = crate::mutation_lock::HostMutationGuard::acquire(
            self.paths.agent_dir.join("on-demand-launch.lock"),
        )?;
        ensure_process(self.client.as_ref(), &self.proxy_spec()?, &self.pid_path())?;
        self.ensure_supervisor()
    }

    pub fn supervise(&self) -> Result<(), String> {
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let _singleton = crate::mutation_lock::HostMutationGuard::acquire_timeout(
            self.paths.agent_dir.join("supervisor.lock"),
            std::time::Duration::from_secs(10),
        )?;
        loop {
            #[cfg(target_os = "linux")]
            if std::fs::read_link("/proc/self/exe").map_err(|error| error.to_string())?
                != executable
            {
                return Ok(());
            }
            #[cfg(not(target_os = "linux"))]
            let _ = &executable;
            // Serialize every tick with stop/update/restore, including the
            // enabled check, so an explicit stop cannot be undone by a tick.
            let guard =
                crate::mutation_lock::HostMutationGuard::acquire(&self.paths.mutation_lock)?;
            if !self.enabled_path().is_file() {
                return Ok(());
            }
            if let Err(error) =
                ensure_process(self.client.as_ref(), &self.proxy_spec()?, &self.pid_path())
            {
                eprintln!("{error}");
            }
            if crate::profile::profile_lease_is_active(
                &self.paths,
                crate::native_codex::NATIVE_PROFILE_ID,
            ) {
                match crate::native_codex::discover_native() {
                    Ok(native) if !native.daemon_running => {
                        if let Err(error) = crate::native_codex::bootstrap_native(&self.paths) {
                            eprintln!("{error}");
                        }
                    }
                    Ok(_) => {} // App-owned or healthy daemons retain authority.
                    Err(error) => eprintln!("{error}"),
                }
            }
            drop(guard);
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }
}

impl ProxyServiceClient for OnDemandClient {
    fn service_path(&self, _directory: &Path) -> PathBuf {
        self.spec_path()
    }
    fn write_service(&self, spec: &LaunchAgentSpec, _directory: &Path) -> Result<PathBuf, String> {
        private_json(&self.spec_path(), spec)?;
        Ok(self.spec_path())
    }
    fn default_port(&self) -> u16 {
        crate::platform::LINUX_PROXY_PORT
    }
    fn restart_after_update(&self, job: &LaunchdJobStatus) -> bool {
        job.running
    }
    fn load_agent(&self, _domain: &str, _file: &Path) -> Result<(), String> {
        let _guard = crate::mutation_lock::HostMutationGuard::acquire(
            self.paths.agent_dir.join("on-demand-launch.lock"),
        )?;
        private_json(&self.enabled_path(), &true)?;
        ensure_process(self.client.as_ref(), &self.proxy_spec()?, &self.pid_path())?;
        self.ensure_supervisor()
    }
    fn unload_agent(&self, _domain: &str, _label: &str) -> Result<(), String> {
        match std::fs::remove_file(self.enabled_path()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        let supervisor_record = self.paths.agent_dir.join("supervisor.pid.json");
        if let Some(identity) = recorded_process(
            self.client.as_ref(),
            &self.supervisor_spec()?,
            &supervisor_record,
        )? {
            self.client.terminate(&identity)?;
        }
        let _ = std::fs::remove_file(supervisor_record);
        if !self.spec_path().is_file() {
            return Ok(());
        }
        if let Some(identity) =
            recorded_process(self.client.as_ref(), &self.proxy_spec()?, &self.pid_path())?
        {
            self.client.terminate(&identity)?;
        }
        let _ = std::fs::remove_file(self.pid_path());
        Ok(())
    }
    fn inspect_agent(&self, _domain: &str, _label: &str) -> Result<LaunchdJobStatus, String> {
        if !self.spec_path().is_file() {
            return Ok(LaunchdJobStatus::default());
        }
        let identity =
            recorded_process(self.client.as_ref(), &self.proxy_spec()?, &self.pid_path())?;
        Ok(LaunchdJobStatus {
            loaded: self.enabled_path().is_file(),
            running: identity.is_some(),
            pid: identity.as_ref().map(|p| p.pid),
            program: identity.map(|p| p.executable.to_string_lossy().into_owned()),
        })
    }
    fn stop_before_replace(&self) -> bool {
        true
    }
    fn inspect_readyz(&self, port: u16, key: &str) -> Result<bool, String> {
        self.probes.inspect_readyz(port, key)
    }
    fn port_in_use(&self, port: u16) -> Result<bool, String> {
        self.probes.port_in_use(port)
    }
    fn read_logs(
        &self,
        stdout: &Path,
        stderr: &Path,
        max_bytes: usize,
    ) -> Result<crate::protocol::ProxyLogsView, String> {
        let mut logs = crate::native_proxy::read_launchd_log_files(stdout, stderr, max_bytes)?;
        logs.source = "agentSupervisor".into();
        Ok(logs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeProcesses {
        live: Mutex<BTreeMap<u32, ProcessIdentity>>,
        spawned: Mutex<Vec<ProcessSpec>>,
        stopped: Mutex<Vec<u32>>,
    }

    impl ManagedProcessClient for FakeProcesses {
        fn uid(&self) -> u32 {
            1000
        }
        fn inspect(&self, pid: u32) -> Result<Option<ProcessIdentity>, String> {
            Ok(self.live.lock().unwrap().get(&pid).cloned())
        }
        fn spawn_detached(&self, spec: &ProcessSpec) -> Result<ProcessIdentity, String> {
            let mut spawned = self.spawned.lock().unwrap();
            let pid = 4000 + spawned.len() as u32;
            let identity = ProcessIdentity {
                pid,
                uid: self.uid(),
                executable: spec.executable.clone(),
                args: spec.args.clone(),
                start_time: format!("start-{pid}"),
                boot_id: "boot-1".into(),
            };
            spawned.push(spec.clone());
            self.live.lock().unwrap().insert(pid, identity.clone());
            Ok(identity)
        }
        fn terminate(&self, identity: &ProcessIdentity) -> Result<(), String> {
            assert_eq!(self.inspect(identity.pid)?.as_ref(), Some(identity));
            self.live.lock().unwrap().remove(&identity.pid);
            self.stopped.lock().unwrap().push(identity.pid);
            Ok(())
        }
    }

    fn fixture() -> (tempfile::TempDir, OnDemandClient, Arc<FakeProcesses>) {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        paths.ensure().unwrap();
        std::fs::create_dir_all(paths.root.join("bin")).unwrap();
        let executable = crate::native_proxy::native_proxy_bin(&paths);
        std::fs::write(&executable, b"fake proxy").unwrap();
        let fake = Arc::new(FakeProcesses::default());
        let service = OnDemandClient {
            paths: paths.clone(),
            client: fake.clone(),
            probes: Arc::new(crate::native_proxy::MemoryLaunchd::default()),
        };
        let spec = LaunchAgentSpec::new(
            executable,
            paths.proxy_config_dir.join("proxy.toml"),
            "127.0.0.1:15721",
            &paths.logs_dir,
            paths.root.clone(),
        );
        service.write_service(&spec, temp.path()).unwrap();
        (temp, service, fake)
    }

    #[test]
    fn reconnect_does_not_enable_a_stopped_install() {
        let (_temp, service, fake) = fixture();
        service.ensure_enabled().unwrap();
        assert!(fake.spawned.lock().unwrap().is_empty());
        assert!(!service.supervisor_ready().unwrap());
    }

    #[test]
    fn load_and_reconnect_are_idempotent_and_recover_dead_processes() {
        let (temp, service, fake) = fixture();
        service.load_agent("user", temp.path()).unwrap();
        assert_eq!(fake.spawned.lock().unwrap().len(), 2);
        assert!(service.supervisor_ready().unwrap());
        service.ensure_enabled().unwrap();
        assert_eq!(fake.spawned.lock().unwrap().len(), 2);
        fake.live.lock().unwrap().clear();
        service.ensure_enabled().unwrap();
        assert_eq!(fake.spawned.lock().unwrap().len(), 4);
        assert!(service.inspect_agent("user", "proxy").unwrap().running);
        service.unload_agent("user", "proxy").unwrap();
        assert_eq!(fake.stopped.lock().unwrap().len(), 2);
        service.ensure_enabled().unwrap();
        assert_eq!(fake.spawned.lock().unwrap().len(), 4);
        assert!(!service.inspect_agent("user", "proxy").unwrap().running);
    }

    #[test]
    fn proxy_update_stops_exact_processes_before_replacing_binary_then_resumes() {
        let (temp, service, fake) = fixture();
        let store = crate::state::AgentStateStore::new(service.paths.clone());
        let staged = temp.path().join("new-proxy");
        std::fs::write(&staged, b"new fake daemon").unwrap();
        let digest = crate::update::sha256_file(&staged).unwrap();
        let service = Arc::new(service);
        let manager = crate::proxy::ProxyManager::with_native(
            store,
            service.clone(),
            temp.path().to_path_buf(),
            "user",
        );
        manager.install("initial", "", None).unwrap();
        service.load_agent("user", temp.path()).unwrap();
        assert!(manager
            .update(
                "invalid-digest",
                staged.to_str().unwrap(),
                Some("bad-digest".into())
            )
            .is_err());
        assert!(fake.stopped.lock().unwrap().is_empty());
        manager
            .update("update", staged.to_str().unwrap(), Some(digest))
            .unwrap();
        assert_eq!(fake.stopped.lock().unwrap().len(), 2);
        assert_eq!(fake.spawned.lock().unwrap().len(), 4);
        assert!(service.supervisor_ready().unwrap());
        assert!(service.inspect_agent("user", "proxy").unwrap().running);
    }

    #[test]
    fn reused_pid_is_never_adopted_or_signaled() {
        let (temp, service, fake) = fixture();
        service.load_agent("user", temp.path()).unwrap();
        let record = read_record(&service.pid_path()).unwrap().unwrap();
        for mutation in ["uid", "executable", "args", "start", "boot"] {
            let mut foreign = record.clone();
            match mutation {
                "uid" => foreign.uid += 1,
                "executable" => foreign.executable = PathBuf::from("/unmanaged"),
                "args" => foreign.args.push("unmanaged".into()),
                "start" => foreign.start_time = "new-start".into(),
                "boot" => foreign.boot_id = "another-boot".into(),
                _ => unreachable!(),
            }
            fake.live.lock().unwrap().insert(record.pid, foreign);
            assert!(!service.inspect_agent("user", "proxy").unwrap().running);
        }
        service.unload_agent("user", "proxy").unwrap();
        assert!(!fake.stopped.lock().unwrap().contains(&record.pid));
        assert!(fake.live.lock().unwrap().contains_key(&record.pid));
    }

    #[test]
    fn malformed_record_and_unexpected_service_paths_fail_closed() {
        let (_temp, service, fake) = fixture();
        std::fs::write(service.pid_path(), b"partial record").unwrap();
        assert!(service
            .inspect_agent("user", "proxy")
            .unwrap_err()
            .contains("PidRecordInvalid"));
        assert!(fake.spawned.lock().unwrap().is_empty());
        let mut spec: LaunchAgentSpec =
            serde_json::from_slice(&std::fs::read(service.spec_path()).unwrap()).unwrap();
        spec.listen = "0.0.0.0:15721".into();
        private_json(&service.spec_path(), &spec).unwrap();
        assert!(service
            .proxy_spec()
            .unwrap_err()
            .contains("ServiceSpecMismatch"));
        spec.listen = "127.0.0.1:15721".into();
        spec.stderr_log = PathBuf::from("/foreign/output");
        private_json(&service.spec_path(), &spec).unwrap();
        assert!(service
            .proxy_spec()
            .unwrap_err()
            .contains("ServiceSpecMismatch"));
    }
}
