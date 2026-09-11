use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use tokio::{
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    time::timeout,
};
use uuid::Uuid;

use crate::HarnessError;
use vellum_harness_protocol::HarnessErrorCategory;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RuntimeInstanceKey(pub String);

#[derive(Debug, Clone)]
pub struct HarnessLaunchSpec {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub inherit_env: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessProcessState {
    Starting,
    Ready,
    Stopping,
    Exited,
    Crashed,
}

#[derive(Debug)]
pub struct ManagedHarnessProcess {
    pub key: RuntimeInstanceKey,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
    child: Child,
    pub state: HarnessProcessState,
}

impl ManagedHarnessProcess {
    pub async fn graceful_shutdown(&mut self, deadline: Duration) -> Result<(), HarnessError> {
        self.state = HarnessProcessState::Stopping;
        match timeout(deadline, self.child.wait()).await {
            Ok(Ok(_)) => {
                self.state = HarnessProcessState::Exited;
                Ok(())
            }
            Ok(Err(error)) => Err(process_error(error)),
            Err(_) => {
                self.child.kill().await.map_err(process_error)?;
                let _ = self.child.wait().await;
                self.state = HarnessProcessState::Exited;
                Ok(())
            }
        }
    }
}

pub struct NativeHarnessSupervisor {
    processes: Mutex<HashMap<RuntimeInstanceKey, Arc<Mutex<ManagedHarnessProcess>>>>,
}
impl Default for NativeHarnessSupervisor {
    fn default() -> Self {
        Self {
            processes: Mutex::new(HashMap::new()),
        }
    }
}
impl NativeHarnessSupervisor {
    pub async fn spawn(
        &self,
        spec: HarnessLaunchSpec,
    ) -> Result<Arc<Mutex<ManagedHarnessProcess>>, HarnessError> {
        if !spec.cwd.is_dir() {
            return Err(HarnessError::Categorized {
                category: HarnessErrorCategory::RuntimeUnavailable,
                message: format!("harness cwd does not exist: {}", spec.cwd.display()),
            });
        }
        let mut command = Command::new(&spec.executable);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if !spec.inherit_env {
            command.env_clear();
        }
        command.envs(&spec.env);
        let mut child = command.spawn().map_err(process_error)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| unavailable("harness stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| unavailable("harness stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| unavailable("harness stderr unavailable"))?;
        let key = RuntimeInstanceKey(Uuid::new_v4().to_string());
        let process = Arc::new(Mutex::new(ManagedHarnessProcess {
            key: key.clone(),
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr: Some(stderr),
            child,
            state: HarnessProcessState::Ready,
        }));
        self.processes
            .lock()
            .await
            .insert(key, Arc::clone(&process));
        Ok(process)
    }
    pub async fn remove(&self, key: &RuntimeInstanceKey) {
        self.processes.lock().await.remove(key);
    }
}
/// Whether a launch spec's executable can actually be run, without running it.
///
/// A probe for a harness the user has not installed should cost a filesystem
/// lookup, not a spawned process and a shutdown deadline.
pub fn executable_is_available(executable: &std::path::Path) -> bool {
    let has_directory = executable.components().count() > 1;
    if has_directory {
        return executable.is_file();
    }
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| extension.to_lowercase())
            .collect()
    } else {
        Vec::new()
    };
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| {
        let candidate = directory.join(executable);
        if candidate.is_file() {
            return true;
        }
        extensions.iter().any(|extension| {
            let mut with_extension = candidate.clone().into_os_string();
            with_extension.push(extension);
            std::path::Path::new(&with_extension).is_file()
        })
    })
}

fn unavailable(message: &str) -> HarnessError {
    HarnessError::Categorized {
        category: HarnessErrorCategory::RuntimeUnavailable,
        message: message.into(),
    }
}
fn process_error(error: std::io::Error) -> HarnessError {
    HarnessError::Categorized {
        category: HarnessErrorCategory::RuntimeCrashed,
        message: error.to_string(),
    }
}
