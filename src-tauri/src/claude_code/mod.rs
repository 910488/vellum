//! Local Claude Desktop Code integration; never changes Desktop's login.
mod accounts;
mod recent_projects;
mod trust;
pub use accounts::{AccountGate, AccountView, AccountsView, CodeAccounts};
pub use recent_projects::{recent_projects, RecentProject};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Mutex};
use vellum_proxy_runtime::claude_code::{
    certificate::CodeCertificate, settings::SettingsLease, ClaudeCodeProxy, CodeRouteStatus,
};

const LEASE_ID: &str = "claudecodelease";
const CERT_ID: &str = "claudecodecertificate";
#[derive(Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CodeScope {
    #[default]
    Project,
    User,
}
#[derive(Serialize, Deserialize)]
struct Lease {
    /// Missing in project journals from older versions.
    #[serde(default)]
    scope: CodeScope,
    project: PathBuf,
    settings: PathBuf,
    settings_existed: bool,
    fields: SettingsLease,
    port: u16,
    secret: String,
}
struct Running {
    proxy: Arc<ClaudeCodeProxy>,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    _lease_lock: std::fs::File,
}
#[derive(Default)]
struct Control {
    running: Option<Running>,
    notice: Option<String>,
    last_routing: CodeRouteStatus,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeStatus {
    pub running: bool,
    pub project: Option<String>,
    pub scope: Option<CodeScope>,
    pub port: Option<u16>,
    pub settings_match: bool,
    pub recovery_required: bool,
    pub notice: Option<String>,
    pub routing: CodeRouteStatus,
    pub accounts: AccountsView,
    pub trust: trust::TrustStatus,
}

pub struct CodeManager {
    root: PathBuf,
    user_settings: Option<PathBuf>,
    pub accounts: Arc<CodeAccounts>,
    control: Mutex<Control>,
    trust: Arc<trust::CodeTrust>,
}
impl CodeManager {
    pub fn new(root: PathBuf) -> Arc<Self> {
        Self::with_user_settings(
            root,
            dirs::home_dir().map(|home| home.join(".claude/settings.json")),
        )
    }
    fn with_user_settings(root: PathBuf, user_settings: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            accounts: CodeAccounts::new(root.clone()),
            #[cfg(not(test))]
            trust: Arc::new(trust::CodeTrust::new(root.clone())),
            #[cfg(test)]
            trust: Arc::new(trust::CodeTrust::isolated(root.clone())),
            root,
            user_settings,
            control: Mutex::new(Control::default()),
        })
    }
    pub async fn status(&self) -> Result<CodeStatus, String> {
        let control = self.control.lock().await;
        let lease = self.load_lease()?;
        let settings_match = lease
            .as_ref()
            .is_some_and(|l| read_settings(&l.settings).is_ok_and(|v| l.fields.matches(&v)));
        let running = control
            .running
            .as_ref()
            .is_some_and(|r| !r.task.is_finished());
        let certificate = self.stored_certificate()?;
        let trust = self
            .trust
            .status(certificate.as_ref().map(CodeCertificate::ca_der), running)?;
        Ok(CodeStatus {
            running,
            project: lease
                .as_ref()
                .filter(|l| l.scope == CodeScope::Project)
                .map(|l| l.project.to_string_lossy().into_owned()),
            scope: lease.as_ref().map(|l| l.scope),
            port: lease.as_ref().map(|l| l.port),
            settings_match,
            recovery_required: lease.is_some() && (!running || !settings_match),
            notice: control.notice.clone(),
            routing: control
                .running
                .as_ref()
                .map(|r| r.proxy.status())
                .unwrap_or_else(|| control.last_routing.clone()),
            accounts: self.accounts.view().await?,
            trust,
        })
    }

    /// Prepare public details before the user explicitly approves trust.
    pub async fn prepare_certificate(&self) -> Result<(), String> {
        let control = self.control.lock().await;
        if control.running.is_some() {
            return Err("Stop Code routing before preparing trust".into());
        }
        let _lock = self.manager_lock()?;
        let certificate = self.certificate()?;
        crate::enhanced_runtime::atomic::write_atomic(
            &self.root.join("claude-code/ca.pem"),
            certificate.ca_pem.as_bytes(),
        )
        .map_err(|_| "Cannot publish Code CA certificate".into())
    }

    /// Explicit fingerprint-bound approval; never called as a TLS fallback.
    pub async fn trust_certificate(&self, expected_sha256: String) -> Result<(), String> {
        if !cfg!(windows) {
            return Err("Temporary CA trust is supported only on Windows".into());
        }
        let control = self.control.lock().await;
        if control.running.is_some() {
            return Err("Stop Code routing before changing CA trust".into());
        }
        let _lock = self.manager_lock()?;
        let certificate = self.certificate()?;
        let der = certificate.ca_der().to_vec();
        let trust = self.trust.clone();
        tokio::task::spawn_blocking(move || trust.enable(&der, &expected_sha256))
            .await
            .map_err(|_| "Code CA trust operation failed")?
    }

    pub async fn restore_trust(&self) -> Result<(), String> {
        let control = self.control.lock().await;
        if control.running.is_some() {
            return Err("Stop Code routing before removing CA trust".into());
        }
        let _lock = self.manager_lock()?;
        let trust = self.trust.clone();
        tokio::task::spawn_blocking(move || trust.restore())
            .await
            .map_err(|_| "Code CA trust restoration failed")?
    }

    /// Startup recovery and session-end recovery share the same synchronous
    /// path: native DPAPI + filesystem APIs, never a child process on Windows.
    /// Refuse another live owner's lock; keep the journal on every failure.
    pub fn recover_stale_settings(&self) -> Result<(), String> {
        let mut control = self
            .control
            .try_lock()
            .map_err(|_| "Code routing is busy")?;
        if control.running.is_some() {
            return Err("Code routing is still owned by this process".into());
        }
        if self.load_lease()?.is_none() {
            return Ok(());
        }
        let _lock = self.manager_lock()?;
        let conflicts = self.restore()?;
        control.notice = Some(if conflicts.is_empty() {
            "Recovered Claude Code settings left by the previous Vellum process.".into()
        } else {
            format!(
                "Recovered Claude Code settings; externally edited fields kept: {}",
                conflicts.join(", ")
            )
        });
        Ok(())
    }

    /// Called before the bounded async teardown on WM_ENDSESSION. Restore
    /// diversion first even when unrelated teardown consumes the exit budget.
    pub fn restore_settings_for_session_end(&self) -> Result<(), String> {
        let control = self
            .control
            .try_lock()
            .map_err(|_| "Code routing is busy")?;
        let _lock = if control.running.is_none() {
            Some(self.manager_lock()?)
        } else {
            None
        };
        self.restore().map(|_| ())
    }

    pub async fn start(&self, project: PathBuf) -> Result<(), String> {
        let project =
            std::fs::canonicalize(project).map_err(|_| "Choose an existing project directory")?;
        if !project.is_dir() {
            return Err("Choose a project directory".into());
        }
        self.start_scoped(project, CodeScope::Project).await
    }

    /// Applies to subsequently launched Code sessions reading user settings.
    pub async fn start_user(&self) -> Result<(), String> {
        let settings = self
            .user_settings
            .as_ref()
            .ok_or("Cannot locate Claude user settings")?;
        let directory = settings
            .parent()
            .ok_or("Invalid Claude user settings path")?;
        if directory.is_symlink() || settings.is_symlink() {
            return Err("Refusing linked Claude user settings".into());
        }
        std::fs::create_dir_all(directory)
            .map_err(|_| "Cannot create Claude user settings directory")?;
        let directory = std::fs::canonicalize(directory)
            .map_err(|_| "Cannot resolve Claude user settings directory")?;
        self.start_scoped(directory, CodeScope::User).await
    }

    async fn start_scoped(&self, project: PathBuf, scope: CodeScope) -> Result<(), String> {
        let mut control = self.control.lock().await;
        if control
            .running
            .as_ref()
            .is_some_and(|r| !r.task.is_finished())
        {
            return Err("Claude Desktop Code routing is already running".into());
        }
        let lease_lock = if let Some(finished) = control.running.take() {
            // The finished listener still owns this manager's lock. Reuse it
            // for same-process recovery instead of treating our lock as foreign.
            let _ = finished.stop.send(true);
            let _ = finished.task.await;
            control.last_routing = finished.proxy.status();
            finished._lease_lock
        } else {
            self.manager_lock()?
        };
        let existing = self.load_lease()?;
        if existing
            .as_ref()
            .is_some_and(|l| l.project != project || l.scope != scope)
        {
            return Err("Restore the previous Claude Code settings lease first".into());
        }
        let certificate = self.certificate()?;
        let tls = certificate.server_config()?;
        self.trust.validate_for_routing(certificate.ca_der())?;
        let directory = match scope {
            CodeScope::Project => project.join(".claude"),
            CodeScope::User => project.clone(),
        };
        std::fs::create_dir_all(&directory)
            .map_err(|_| "Cannot create project Claude settings directory")?;
        let canonical_directory = std::fs::canonicalize(&directory)
            .map_err(|_| "Cannot resolve Claude settings directory")?;
        if !canonical_directory.starts_with(&project) {
            return Err("Claude settings directory points outside the project".into());
        }
        let settings = directory.join(match scope {
            CodeScope::Project => "settings.local.json",
            CodeScope::User => self
                .user_settings
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .ok_or("Invalid Claude user settings file name")?,
        });
        if settings.is_symlink() {
            return Err("Refusing to replace linked Claude settings".into());
        }
        // Lease locks serialize Vellum writers. External changes are checked again
        // before publication and restoration merges only fields still owned by us.
        let _lock = self.settings_lock(&project)?;
        let ca_path = self.root.join("claude-code").join("ca.pem");
        crate::enhanced_runtime::atomic::write_atomic(&ca_path, certificate.ca_pem.as_bytes())
            .map_err(|_| "Cannot write Claude Code CA certificate")?;
        let (lease, listener, publication) = if let Some(lease) = existing {
            if !lease.fields.matches(&read_settings(&lease.settings)?) {
                return Err(
                    "Claude Code settings changed; restore them before reconnecting".into(),
                );
            }
            let listener =
                tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, lease.port))
                    .await
                    .map_err(|_| {
                        "Saved Claude Code proxy port is occupied; restore settings first"
                    })?;
            (lease, listener, None)
        } else {
            if scope == CodeScope::Project {
                reject_inherited_settings(&project, self.user_settings.as_deref())?;
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|_| "Cannot bind Claude Code loopback listener")?;
            let port = listener
                .local_addr()
                .map_err(|_| "Cannot read Claude Code listener")?
                .port();
            let secret = accounts::random_secret()?;
            let before = std::fs::read(&settings).ok();
            let mut value = read_settings(&settings)?;
            let fields = SettingsLease::apply(
                &mut value,
                &format!("http://vellum:{secret}@127.0.0.1:{port}"),
                &ca_path.to_string_lossy(),
            )?;
            let lease = Lease {
                scope,
                project: project.clone(),
                settings: settings.clone(),
                settings_existed: settings.exists(),
                fields,
                port,
                secret,
            };
            (lease, listener, Some((before, value)))
        };
        let health_client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .map_err(|_| "Cannot create health client")?;
        let proxy = ClaudeCodeProxy::new(&lease.secret, self.accounts.clone())?;
        let (stop, wait) = watch::channel(false);
        let task = tokio::spawn(proxy.clone().serve(listener, tls, wait));
        // Authenticated health check before reporting a running integration.
        let healthy = health_client
            .get(format!("http://127.0.0.1:{}/readyz", lease.port))
            .header(
                "Proxy-Authorization",
                format!(
                    "Basic {}",
                    base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        format!("vellum:{}", lease.secret)
                    )
                ),
            )
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
        if !healthy {
            let _ = stop.send(true);
            let _ = task.await;
            return Err(
                "Claude Code proxy readiness failed; restore settings before retrying".into(),
            );
        }
        let publish = (|| -> Result<(), String> {
            if let Some((before, value)) = publication {
                if std::fs::read(&settings).ok() != before {
                    return Err("Claude settings changed while enabling; retry".into());
                }
                // Ready first, journal second, diversion last. A crash at any step
                // leaves either untouched settings or a recoverable owned lease.
                self.save_lease(&lease)?;
                if let Err(error) = write_settings(&settings, &value) {
                    let _ = crate::credentials::remove(&self.root, LEASE_ID);
                    return Err(error);
                }
            }
            Ok(())
        })();
        if let Err(error) = publish {
            let _ = stop.send(true);
            let _ = task.await;
            return Err(error);
        }
        control.running = Some(Running {
            proxy,
            stop,
            task,
            _lease_lock: lease_lock,
        });
        control.notice = Some(match scope {
            CodeScope::Project => "Waiting for this project's local Desktop Code request. Resume the original Code session if it still uses its previous network settings.",
            CodeScope::User => "User settings routing enabled. New Desktop Code sessions use this proxy; resume existing sessions to reload settings. CLI requests keep their original authorization.",
        }.into());
        Ok(())
    }

    pub async fn stop(&self, force: bool) -> Result<(), String> {
        let mut control = self.control.lock().await;
        if control.running.is_none() && self.load_lease()?.is_none() {
            if !self.trust.has_lease()? {
                return Ok(());
            }
            let _lock = self.manager_lock()?;
            return self.trust.restore();
        }
        // A second manager/process may inspect status, but must not restore a
        // live manager's diversion or overwrite its single settings lease.
        let _recovery_lock = if control.running.is_none() {
            Some(self.manager_lock()?)
        } else {
            None
        };
        if let Some(running) = control.running.as_ref() {
            if !running.proxy.begin_drain() && !force {
                running.proxy.cancel_drain();
                return Err(
                    "Code work is still active; wait for it to finish or use immediate stop".into(),
                );
            }
        }
        // Restore before closing transport. A restore failure leaves the proxy
        // alive so the user can repair a conflict without a dead endpoint.
        let notice = match self.restore() {
            Ok(notice) => notice,
            Err(error) => {
                if let Some(running) = control.running.as_ref() {
                    running.proxy.cancel_drain();
                }
                return Err(error);
            }
        };
        let _stopped_lock = if let Some(running) = control.running.take() {
            let _ = running.stop.send(true);
            let _ = tokio::time::timeout(Duration::from_secs(3), running.task).await;
            control.last_routing = running.proxy.status();
            // Keep ownership until CA cleanup finishes, not merely until the
            // listener is stopped. Another manager must not start in this gap.
            Some(running._lease_lock)
        } else {
            None
        };
        control.notice = Some(if notice.is_empty() {
            "Settings restored. A running Code process may need to resume before it stops using the old proxy.".into()
        } else {
            format!(
                "Settings restored; externally edited fields kept: {}",
                notice.join(", ")
            )
        });
        // Trust follows settings/transport teardown. A cleanup error retains
        // its separate journal and remains visible even without a project lease.
        let trust = self.trust.clone();
        tokio::task::spawn_blocking(move || trust.restore())
            .await
            .map_err(|_| "Code CA trust restoration failed")??;
        Ok(())
    }

    fn stored_certificate(&self) -> Result<Option<CodeCertificate>, String> {
        crate::credentials::load(&self.root, CERT_ID)
            .map_err(|_| "Cannot decrypt Code certificate")?
            .map(|text| {
                serde_json::from_str(&text).map_err(|_| "Invalid encrypted Code certificate".into())
            })
            .transpose()
    }

    fn certificate(&self) -> Result<CodeCertificate, String> {
        if let Some(text) = crate::credentials::load(&self.root, CERT_ID)
            .map_err(|_| "Cannot decrypt Code certificate")?
        {
            let certificate: CodeCertificate =
                serde_json::from_str(&text).map_err(|_| "Invalid encrypted Code certificate")?;
            if certificate.is_current() {
                return Ok(certificate);
            }
            if self.load_lease()?.is_some() || self.trust.has_lease()? {
                return Err(
                    "Restore Code routing settings before upgrading its certificate".into(),
                );
            }
        }
        let certificate = CodeCertificate::generate()?;
        let text =
            serde_json::to_string(&certificate).map_err(|_| "Cannot encode Code certificate")?;
        crate::credentials::save(&self.root, CERT_ID, &text)
            .map_err(|_| "Cannot save encrypted Code certificate")?;
        Ok(certificate)
    }
    fn load_lease(&self) -> Result<Option<Lease>, String> {
        crate::credentials::load(&self.root, LEASE_ID)
            .map_err(|_| "Cannot decrypt Claude Code settings lease")?
            .map(|text| {
                serde_json::from_str(&text).map_err(|_| "Invalid Claude Code settings lease".into())
            })
            .transpose()
    }
    fn save_lease(&self, lease: &Lease) -> Result<(), String> {
        crate::credentials::save(
            &self.root,
            LEASE_ID,
            &serde_json::to_string(lease).map_err(|_| "Cannot encode Code lease")?,
        )
        .map_err(|_| "Cannot save encrypted Code settings lease".into())
    }
    fn restore(&self) -> Result<Vec<String>, String> {
        let Some(lease) = self.load_lease()? else {
            return Ok(vec![]);
        };
        if lease.settings.is_symlink() {
            return Err("Claude settings became a link; repair manually".into());
        }
        let _lock = self.settings_lock(&lease.project)?;
        if !lease
            .settings
            .try_exists()
            .map_err(|_| "Cannot inspect leased Claude settings")?
        {
            crate::credentials::remove(&self.root, LEASE_ID)
                .map_err(|_| "Cannot remove Claude Code lease")?;
            return Ok(vec![]);
        }
        let parent = lease
            .settings
            .parent()
            .ok_or("Invalid Claude settings path")?;
        if !std::fs::canonicalize(parent)
            .map_err(|_| "Cannot resolve leased Claude settings")?
            .starts_with(&lease.project)
        {
            return Err("Claude settings moved outside the project; repair manually".into());
        }
        let before = std::fs::read(&lease.settings).ok();
        let mut value = read_settings(&lease.settings)?;
        let conflicts = lease.fields.restore(&mut value)?;
        if std::fs::read(&lease.settings).ok() != before {
            return Err("Claude settings changed during restore; retry".into());
        }
        if !lease.settings_existed && value.as_object().is_some_and(|v| v.is_empty()) {
            if lease.settings.exists() {
                std::fs::remove_file(&lease.settings)
                    .map_err(|_| "Cannot remove Vellum-created Claude settings")?;
            }
        } else {
            write_settings(&lease.settings, &value)?;
        }
        crate::credentials::remove(&self.root, LEASE_ID)
            .map_err(|_| "Cannot remove Claude Code lease")?;
        Ok(conflicts)
    }

    fn settings_lock(&self, project: &Path) -> Result<std::fs::File, String> {
        use sha2::{Digest, Sha256};
        let directory = self.root.join("claude-code");
        std::fs::create_dir_all(&directory)
            .map_err(|_| "Cannot create Claude Code control directory")?;
        let name = hex::encode(Sha256::digest(project.to_string_lossy().as_bytes()));
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join(format!("settings-{name}.lock")))
            .map_err(|_| "Cannot lock Claude settings")?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .map_err(|_| "Claude settings are being changed by another Vellum operation")?;
        Ok(lock)
    }

    fn manager_lock(&self) -> Result<std::fs::File, String> {
        let directory = self.root.join("claude-code");
        std::fs::create_dir_all(&directory)
            .map_err(|_| "Cannot create Claude Code control directory")?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("manager.lock"))
            .map_err(|_| "Cannot open Claude Code manager lock")?;
        fs2::FileExt::try_lock_exclusive(&file)
            .map_err(|_| "Another Vellum process owns Claude Code routing")?;
        Ok(file)
    }
}
fn read_settings(path: &Path) -> Result<Value, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| "Claude settings must be valid JSON; existing data was preserved".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(_) => Err("Cannot read Claude settings".into()),
    }
}
fn write_settings(path: &Path, value: &Value) -> Result<(), String> {
    use std::io::Write;
    let bytes = serde_json::to_vec_pretty(value).map_err(|_| "Cannot encode Claude settings")?;
    let parent = path.parent().ok_or("Invalid Claude settings path")?;
    std::fs::create_dir_all(parent).map_err(|_| "Cannot create Claude settings directory")?;
    let temporary = crate::enhanced_runtime::atomic::unique_temporary(path);
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        replace_settings(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|_| "Cannot publish Claude settings".into())
}
#[cfg(windows)]
fn replace_settings(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both paths are NUL-terminated and live for the synchronous call.
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
#[cfg(not(windows))]
fn replace_settings(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}
fn reject_inherited_settings(project: &Path, user_settings: Option<&Path>) -> Result<(), String> {
    let mut paths = vec![project.join(".claude/settings.json")];
    if let Some(path) = user_settings {
        paths.push(path.to_path_buf());
    }
    for path in paths {
        let value = read_settings(&path)?;
        if let Some(env) = value.get("env").and_then(Value::as_object) {
            for key in [
                "https_proxy",
                "HTTPS_PROXY",
                "http_proxy",
                "HTTP_PROXY",
                "no_proxy",
                "NO_PROXY",
                "NODE_EXTRA_CA_CERTS",
                "NODE_TLS_REJECT_UNAUTHORIZED",
            ] {
                if env.contains_key(key) {
                    return Err(format!(
                        "Existing {key} in {} conflicts with Code routing",
                        path.display()
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn fixture() -> (tempfile::TempDir, Arc<CodeManager>, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let manager = CodeManager::with_user_settings(
            dir.path().join("data"),
            Some(dir.path().join("user/settings.json")),
        );
        (dir, manager, project)
    }
    #[tokio::test]
    async fn user_install_restores_only_owned_fields_and_reports_scope() {
        let (_dir, manager, project) = fixture();
        let settings = manager.user_settings.as_ref().unwrap();
        let original = json!({"model":"keep", "env":{"OTHER":"original"}});
        write_settings(settings, &original).unwrap();
        manager.start_user().await.unwrap();
        let status = manager.status().await.unwrap();
        assert!(status.running && status.settings_match);
        assert!(matches!(status.scope, Some(CodeScope::User)));
        assert!(status.project.is_none());
        assert!(!project.join(".claude").exists());
        assert!(manager.start(project).await.is_err());
        let mut value = read_settings(settings).unwrap();
        value["env"]["HTTPS_PROXY"] = json!("http://external-proxy");
        value["permissions"] = json!({"allow":["Read"]});
        write_settings(settings, &value).unwrap();
        assert!(manager.status().await.unwrap().recovery_required);
        manager.stop(false).await.unwrap();
        assert_eq!(
            read_settings(settings).unwrap(),
            json!({"model":"keep", "env":{"OTHER":"original", "HTTPS_PROXY":"http://external-proxy"}, "permissions":{"allow":["Read"]}})
        );
        assert!(manager.load_lease().unwrap().is_none());
    }

    #[tokio::test]
    async fn startup_recovers_user_crash_lease_and_never_restores_a_live_owner() {
        let (_dir, manager, _) = fixture();
        let settings = manager.user_settings.as_ref().unwrap().clone();
        write_settings(&settings, &json!({"model":"original"})).unwrap();
        manager.start_user().await.unwrap();
        let next = CodeManager::with_user_settings(manager.root.clone(), Some(settings.clone()));
        assert!(next.recover_stale_settings().is_err());
        assert!(manager.status().await.unwrap().settings_match);
        drop(manager);
        next.recover_stale_settings().unwrap();
        next.recover_stale_settings().unwrap();
        assert_eq!(
            read_settings(&settings).unwrap(),
            json!({"model":"original"})
        );
        assert!(!next.status().await.unwrap().recovery_required);
    }

    #[tokio::test]
    async fn session_end_synchronously_restores_user_settings_before_transport_closes() {
        let (_dir, manager, _) = fixture();
        manager.start_user().await.unwrap();
        let proxy = manager
            .control
            .lock()
            .await
            .running
            .as_ref()
            .unwrap()
            .proxy
            .clone();
        manager.restore_settings_for_session_end().unwrap();
        assert!(manager.control.lock().await.running.is_some());
        assert!(!manager.user_settings.as_ref().unwrap().exists());
        assert!(manager.load_lease().unwrap().is_none());
        assert!(proxy.begin_drain());
        manager.stop(true).await.unwrap();
    }

    #[tokio::test]
    async fn invalid_user_settings_keep_recovery_journal_and_missing_settings_stay_missing() {
        let (_dir, manager, _) = fixture();
        let settings = manager.user_settings.as_ref().unwrap();
        manager.start_user().await.unwrap();
        std::fs::write(settings, b"invalid json").unwrap();
        assert!(manager.restore_settings_for_session_end().is_err());
        assert!(manager.load_lease().unwrap().is_some());
        std::fs::remove_file(settings).unwrap();
        manager.stop(true).await.unwrap();
        assert!(!settings.exists());
        assert!(manager.load_lease().unwrap().is_none());
    }

    #[tokio::test]
    async fn user_proxy_conflicts_preserve_original_bytes() {
        let (_dir, manager, _) = fixture();
        let settings = manager.user_settings.as_ref().unwrap();
        let bytes = b"{ \"env\": {\"https_proxy\": \"http://company\"} }";
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(settings, bytes).unwrap();
        assert!(manager.start_user().await.is_err());
        assert_eq!(std::fs::read(settings).unwrap(), bytes);
        assert!(manager.load_lease().unwrap().is_none());
    }

    #[tokio::test]
    async fn old_project_journal_defaults_to_project_scope_and_recovers_at_startup() {
        let (_dir, manager, project) = fixture();
        manager.start(project.clone()).await.unwrap();
        let mut old = serde_json::to_value(manager.load_lease().unwrap().unwrap()).unwrap();
        old.as_object_mut().unwrap().remove("scope");
        crate::credentials::save(&manager.root, LEASE_ID, &old.to_string()).unwrap();
        let next =
            CodeManager::with_user_settings(manager.root.clone(), manager.user_settings.clone());
        drop(manager);
        assert!(matches!(
            next.load_lease().unwrap().unwrap().scope,
            CodeScope::Project
        ));
        next.recover_stale_settings().unwrap();
        assert!(!project.join(".claude/settings.local.json").exists());
    }

    #[test]
    fn failed_native_replace_keeps_existing_settings_file_intact() {
        let (dir, _, _) = fixture();
        let settings = dir.path().join("settings.json");
        std::fs::write(&settings, b"original bytes").unwrap();
        assert!(replace_settings(&dir.path().join("missing.tmp"), &settings).is_err());
        assert_eq!(std::fs::read(&settings).unwrap(), b"original bytes");
    }

    #[tokio::test]
    async fn project_install_and_restore_preserve_user_changes_and_never_change_login() {
        let (_dir, manager, project) = fixture();
        manager.start(project.clone()).await.unwrap();
        let status = manager.status().await.unwrap();
        assert!(status.running && status.settings_match);
        assert_eq!(status.routing.desktop_requests, 0);
        let settings = project.join(".claude/settings.local.json");
        let mut value = read_settings(&settings).unwrap();
        assert_eq!(value["env"].as_object().unwrap().len(), 2);
        value["permissions"] = json!({"allow":["Read"]});
        write_settings(&settings, &value).unwrap();
        manager.stop(false).await.unwrap();
        assert_eq!(
            read_settings(&settings).unwrap(),
            json!({"permissions":{"allow":["Read"]}})
        );
        assert!(!manager.status().await.unwrap().recovery_required);
        assert!(!manager.root.join("auth.json").exists());
    }
    #[tokio::test]
    async fn crashed_install_reconnects_with_the_same_ca_port_and_owned_settings() {
        let (_dir, manager, project) = fixture();
        manager.start(project.clone()).await.unwrap();
        let root = manager.root.clone();
        let user_settings = manager.user_settings.clone();
        let port = manager.status().await.unwrap().port.unwrap();
        let ca = std::fs::read(root.join("claude-code/ca.pem")).unwrap();
        drop(manager);
        for _ in 0..100 {
            if let Ok(listener) =
                tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await
            {
                drop(listener);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let manager = CodeManager::with_user_settings(root.clone(), user_settings);
        assert!(manager.status().await.unwrap().recovery_required);
        manager.start(project.clone()).await.unwrap();
        assert_eq!(manager.status().await.unwrap().port, Some(port));
        assert_eq!(std::fs::read(root.join("claude-code/ca.pem")).unwrap(), ca);
        manager.stop(false).await.unwrap();
        assert!(!project.join(".claude/settings.local.json").exists());
    }
    #[tokio::test]
    async fn finished_listener_can_reconnect_in_the_same_manager_without_losing_its_lease() {
        let (_dir, manager, project) = fixture();
        manager.start(project.clone()).await.unwrap();
        let port = manager.status().await.unwrap().port;
        let stop = manager
            .control
            .lock()
            .await
            .running
            .as_ref()
            .unwrap()
            .stop
            .clone();
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if manager
                    .control
                    .lock()
                    .await
                    .running
                    .as_ref()
                    .unwrap()
                    .task
                    .is_finished()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(manager.status().await.unwrap().recovery_required);
        manager.start(project).await.unwrap();
        let status = manager.status().await.unwrap();
        assert!(status.running && status.settings_match);
        assert_eq!(status.port, port);
        manager.stop(false).await.unwrap();
    }
    #[tokio::test]
    async fn inherited_proxy_and_foreign_settings_are_preserved() {
        let (_dir, manager, project) = fixture();
        let user = manager.user_settings.as_ref().unwrap();
        write_settings(user, &json!({"env":{"https_proxy":"http://company"}})).unwrap();
        assert!(manager.start(project.clone()).await.is_err());
        assert!(!project.join(".claude/settings.local.json").exists());
        std::fs::remove_file(user).unwrap();
        manager.start(project.clone()).await.unwrap();
        let settings = project.join(".claude/settings.local.json");
        let mut value = read_settings(&settings).unwrap();
        value["env"]["HTTPS_PROXY"] = json!("http://new-company");
        write_settings(&settings, &value).unwrap();
        assert!(manager.status().await.unwrap().recovery_required);
        manager.stop(false).await.unwrap();
        assert_eq!(
            read_settings(&settings).unwrap(),
            json!({"env":{"HTTPS_PROXY":"http://new-company"}})
        );
    }

    #[tokio::test]
    async fn restore_does_not_recreate_externally_removed_project_settings() {
        let (_dir, manager, project) = fixture();
        manager.start(project.clone()).await.unwrap();
        std::fs::remove_file(project.join(".claude/settings.local.json")).unwrap();
        // Control locks live under Vellum, leaving no extra project files.
        std::fs::remove_dir(project.join(".claude")).unwrap();
        manager.stop(false).await.unwrap();
        assert!(!project.join(".claude").exists());
        assert!(!manager.status().await.unwrap().recovery_required);
    }

    #[tokio::test]
    async fn old_certificate_is_upgraded_only_after_the_project_lease_is_restored() {
        let (_dir, manager, project) = fixture();
        manager.start(project.clone()).await.unwrap();
        let current = manager.certificate().unwrap();
        let mut old = serde_json::to_value(&current).unwrap();
        old.as_object_mut().unwrap().remove("format_version");
        crate::credentials::save(&manager.root, CERT_ID, &old.to_string()).unwrap();
        assert!(manager.certificate().is_err());
        manager.stop(false).await.unwrap();
        let upgraded = manager.certificate().unwrap();
        assert!(upgraded.is_current());
        assert_ne!(upgraded.ca_pem, current.ca_pem);
        assert_eq!(manager.certificate().unwrap().ca_pem, upgraded.ca_pem);
    }
}
