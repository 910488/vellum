//! Jetson isolated live fixture.
//!
//! Windows host:
//!   cargo test --features live-smoke ...
//! SSH:
//!   provision isolated roots
//!   start transient systemd units for app-server + broker
//!   open local port-forward
//! Drop:
//!   best-effort stop smoke units only

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;
use ulid::Ulid;

use super::broker_client::LiveBrokerClient;
use super::config::{
    LiveSmokeConfig, LiveSmokeError, PRODUCTION_APP_SERVER_CONTROL_SOCK, PRODUCTION_CODEX_HOME,
    PRODUCTION_DESKTOP_SSH_WS_SOCK,
};
use super::ssh::{scp_from_remote, scp_to_remote, ssh_script, SshTunnelGuard};
use super::trace::LiveSmokeTrace;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmokeUnitKind {
    CodexAppServer,
    Broker,
}

impl SmokeUnitKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::CodexAppServer => "vellum-codex-smoke",
            Self::Broker => "vellum-broker-smoke",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SmokeUnit {
    pub kind: SmokeUnitKind,
    pub name: String,
}

impl SmokeUnit {
    fn new(kind: SmokeUnitKind, short_id: &str) -> Self {
        Self {
            kind,
            name: format!("{}-{}.service", kind.prefix(), short_id),
        }
    }

    fn validate(name: &str) -> Result<(), LiveSmokeError> {
        let ok = (name.starts_with("vellum-codex-smoke-")
            || name.starts_with("vellum-broker-smoke-"))
            && name.ends_with(".service")
            && !name.contains("..")
            && !name.contains('/')
            && !name.contains('\\');
        if ok {
            Ok(())
        } else {
            Err(LiveSmokeError::Isolation(format!(
                "refusing to control non-smoke unit: {name}"
            )))
        }
    }
}

fn unix_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

pub struct JetsonLiveFixture {
    pub config: LiveSmokeConfig,
    pub run_id: String,
    pub short_id: String,
    pub remote_root: PathBuf,
    pub codex_home: PathBuf,
    pub socket_path: PathBuf,
    pub workspace: PathBuf,
    pub broker_data_dir: PathBuf,
    pub broker_config_path: PathBuf,
    pub logs_dir: PathBuf,
    pub remote_broker_bin: PathBuf,
    pub app_server_unit: SmokeUnit,
    pub broker_unit: SmokeUnit,
    pub local_artifact_dir: PathBuf,
    pub trace: LiveSmokeTrace,
    tunnel: Option<SshTunnelGuard>,
    cleaned: bool,
}

impl JetsonLiveFixture {
    pub async fn provision() -> Result<Self, LiveSmokeError> {
        let mut config = LiveSmokeConfig::from_env()?;
        if std::env::var("VELLUM_LIVE_RUNTIME_ROOT").is_err() {
            // Resolve remote XDG_RUNTIME_DIR instead of hard-coding uid 1000.
            let script =
                "printf '%s\\n' \"${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/vellum-smoke\"\\n";
            if let Ok(runtime) = ssh_script(&config.ssh_host, script).await {
                let runtime = runtime.trim();
                if !runtime.is_empty() {
                    config.remote_runtime_root = PathBuf::from(runtime);
                }
            }
        }
        let run_id = Ulid::new().to_string();
        let short_id = run_id
            .chars()
            .rev()
            .take(6)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
            .to_lowercase();

        let remote_root = config.remote_state_root.join(&run_id);
        let codex_home = remote_root.join("codex-home");
        let broker_data_dir = remote_root.join("broker");
        let workspace = remote_root.join("workspace");
        let logs_dir = remote_root.join("logs");
        let broker_config_path = remote_root.join("remote-smoke.toml");
        let remote_broker_bin = remote_root.join("bin").join("vellum-remote-broker");
        let socket_path = config.remote_runtime_root.join(&short_id).join("app.sock");
        let local_artifact_dir = config.local_artifact_root.join(&run_id);

        config.assert_smoke_paths(&codex_home, &socket_path, &workspace, &broker_data_dir)?;

        let app_server_unit = SmokeUnit::new(SmokeUnitKind::CodexAppServer, &short_id);
        let broker_unit = SmokeUnit::new(SmokeUnitKind::Broker, &short_id);
        SmokeUnit::validate(&app_server_unit.name)?;
        SmokeUnit::validate(&broker_unit.name)?;

        let mut trace = LiveSmokeTrace::new(&run_id, &short_id, &config.ssh_host);
        trace.local_artifact_dir = Some(local_artifact_dir.clone());
        tokio::fs::create_dir_all(&local_artifact_dir).await?;

        let mut fixture = Self {
            config,
            run_id,
            short_id,
            remote_root,
            codex_home,
            socket_path,
            workspace,
            broker_data_dir,
            broker_config_path,
            logs_dir,
            remote_broker_bin,
            app_server_unit,
            broker_unit,
            local_artifact_dir,
            trace,
            tunnel: None,
            cleaned: false,
        };

        fixture.remote_provision_dirs().await?;
        fixture.remote_copy_auth_config().await?;
        fixture.remote_write_broker_config().await?;
        fixture.remote_write_workspace_helpers().await?;
        fixture.trace.note("provisioned isolated smoke roots");
        Ok(fixture)
    }

    async fn remote_provision_dirs(&self) -> Result<(), LiveSmokeError> {
        let script = format!(
            "set -euo pipefail\n\
PROD_REAL=\"$(realpath -m \"{prod_home}\")\"\n\
SMOKE_REAL=\"$(realpath -m \"{root}\")\"\n\
CODEX_REAL=\"$(realpath -m \"{codex_home}\")\"\n\
WS_REAL=\"$(realpath -m \"{workspace}\")\"\n\
DB_REAL=\"$(realpath -m \"{broker_data}\")\"\n\
SOCK_PARENT_REAL=\"$(realpath -m \"{socket_dir}\")\"\n\
case \"$SMOKE_REAL\" in\n\
  \"$PROD_REAL\"|\"$PROD_REAL\"/*) echo \"smoke root resolves under production CODEX_HOME\" >&2; exit 40 ;;\n\
esac\n\
case \"$CODEX_REAL\" in\n\
  \"$PROD_REAL\"|\"$PROD_REAL\"/*) echo \"smoke CODEX_HOME resolves under production\" >&2; exit 40 ;;\n\
esac\n\
case \"$WS_REAL\" in\n\
  \"$PROD_REAL\"|\"$PROD_REAL\"/*) echo \"workspace resolves under production\" >&2; exit 40 ;;\n\
esac\n\
case \"$DB_REAL\" in\n\
  \"$PROD_REAL\"|\"$PROD_REAL\"/*) echo \"broker data resolves under production\" >&2; exit 40 ;;\n\
esac\n\
case \"$SOCK_PARENT_REAL\" in\n\
  \"$PROD_REAL\"|\"$PROD_REAL\"/*) echo \"socket parent resolves under production\" >&2; exit 40 ;;\n\
esac\n\
if [[ \"{socket}\" == *app-server-control* ]]; then\n\
  echo \"refusing production socket path\" >&2; exit 41\n\
fi\n\
mkdir -p \"{root}/codex-home\" \"{root}/broker\" \"{root}/workspace\" \"{root}/logs\" \"{root}/trace\" \"{root}/bin\" \"{socket_dir}\"\n\
mkdir -p \"{runtime_root}\"\n\
# Re-check after mkdir in case of aliasing.\n\
CODEX_REAL2=\"$(realpath -m \"{codex_home}\")\"\n\
case \"$CODEX_REAL2\" in\n\
  \"$PROD_REAL\"|\"$PROD_REAL\"/*) echo \"post-mkdir CODEX_HOME under production\" >&2; exit 40 ;;\n\
esac\n",
            prod_home = PRODUCTION_CODEX_HOME,
            root = unix_path(&self.remote_root),
            codex_home = unix_path(&self.codex_home),
            workspace = unix_path(&self.workspace),
            broker_data = unix_path(&self.broker_data_dir),
            socket_dir = unix_path(
                self.socket_path
                    .parent()
                    .unwrap_or_else(|| Path::new("/run/user/1000/vellum-smoke"))
            ),
            socket = unix_path(&self.socket_path),
            runtime_root = unix_path(&self.config.remote_runtime_root),
        );
        let _ = ssh_script(&self.config.ssh_host, &script).await?;
        Ok(())
    }

    async fn remote_copy_auth_config(&self) -> Result<(), LiveSmokeError> {
        let script = format!(
            "set -euo pipefail\n\
cp -f \"{auth}\" \"{codex_home}/auth.json\"\n\
cp -f \"{config}\" \"{codex_home}/config.toml\"\n\
chmod 600 \"{codex_home}/auth.json\" \"{codex_home}/config.toml\"\n",
            auth = unix_path(&self.config.production_auth),
            config = unix_path(&self.config.production_config),
            codex_home = unix_path(&self.codex_home),
        );
        let _ = ssh_script(&self.config.ssh_host, &script).await?;
        Ok(())
    }

    async fn remote_write_broker_config(&self) -> Result<(), LiveSmokeError> {
        let allowed = self
            .config
            .allowed_versions
            .iter()
            .map(|version| format!("\"{version}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let body = format!(
            "broker_id = \"vellum-live-smoke-{short}\"\n\
data_dir = \"{data}\"\n\
listen_addr = \"127.0.0.1:{port}\"\n\
codex_binary = \"{binary}\"\n\
codex_home = \"{codex_home}\"\n\
app_server_socket = \"{socket}\"\n\
allowed_versions = [{allowed}]\n\
allowed_roots = [\"{workspace}\"]\n\
require_auth = false\n\
local_only = true\n\
writer_lease_ttl_secs = 30\n\
writer_lease_heartbeat_secs = 10\n\
writer_lease_disconnect_grace_secs = 15\n",
            short = self.short_id,
            data = unix_path(&self.broker_data_dir),
            port = self.config.broker_listen_port,
            binary = unix_path(&self.config.codex_native),
            codex_home = unix_path(&self.codex_home),
            socket = unix_path(&self.socket_path),
            allowed = allowed,
            workspace = unix_path(&self.workspace),
        );
        let local = self.local_artifact_dir.join("remote-smoke.toml");
        tokio::fs::write(&local, body).await?;
        scp_to_remote(
            &local,
            &self.config.ssh_host,
            &unix_path(&self.broker_config_path),
        )
        .await
    }

    async fn remote_write_workspace_helpers(&self) -> Result<(), LiveSmokeError> {
        let script = format!(
            "set -euo pipefail\n\
cat > \"{workspace}/smoke_delay.sh\" <<'EOF'\n\
#!/bin/sh\n\
sleep 8\n\
printf 'VELLUM_DETACH_OK\\n'\n\
EOF\n\
chmod +x \"{workspace}/smoke_delay.sh\"\n\
printf 'vellum live smoke workspace\\n' > \"{workspace}/README.txt\"\n",
            workspace = unix_path(&self.workspace),
        );
        let _ = ssh_script(&self.config.ssh_host, &script).await?;
        Ok(())
    }

    pub async fn upload_broker_binary(&mut self, local_bin: &Path) -> Result<(), LiveSmokeError> {
        if !local_bin.exists() {
            return Err(LiveSmokeError::Message(format!(
                "local broker binary not found: {}",
                local_bin.display()
            )));
        }
        scp_to_remote(
            local_bin,
            &self.config.ssh_host,
            &unix_path(&self.remote_broker_bin),
        )
        .await?;
        let script = format!(
            "set -euo pipefail\nchmod +x \"{bin}\"\nfile \"{bin}\" || true\n",
            bin = unix_path(&self.remote_broker_bin),
        );
        let _ = ssh_script(&self.config.ssh_host, &script).await?;
        self.trace.note(format!(
            "uploaded broker binary to {}",
            self.remote_broker_bin.display()
        ));
        Ok(())
    }

    pub async fn start_app_server(&mut self) -> Result<(), LiveSmokeError> {
        SmokeUnit::validate(&self.app_server_unit.name)?;
        let unit = self.app_server_unit.name.clone();
        let script = format!(
            "set -euo pipefail\n\
rm -f \"{socket}\"\n\
case \"{unit}\" in\n\
  vellum-codex-smoke-*.service) ;;\n\
  *) echo \"bad unit {unit}\" >&2; exit 42 ;;\n\
esac\n\
systemctl --user stop \"{unit}\" 2>/dev/null || true\n\
systemd-run --user \\\n\
  --unit=\"{unit}\" \\\n\
  --property=Restart=no \\\n\
  --property=StandardOutput=append:{log} \\\n\
  --property=StandardError=append:{log} \\\n\
  --setenv=CODEX_HOME={codex_home} \\\n\
  --setenv=RUST_LOG=info \\\n\
  {binary} app-server --listen unix://{socket}\n",
            socket = unix_path(&self.socket_path),
            unit = unit,
            log = unix_path(&self.logs_dir.join("app-server.log")),
            codex_home = unix_path(&self.codex_home),
            binary = unix_path(&self.config.codex_native),
        );
        let _ = ssh_script(&self.config.ssh_host, &script).await?;
        self.wait_remote_path_exists(&self.socket_path, Duration::from_secs(20))
            .await?;
        self.trace.note(format!("app-server unit started: {unit}"));
        Ok(())
    }

    pub async fn start_broker(&mut self) -> Result<(), LiveSmokeError> {
        SmokeUnit::validate(&self.broker_unit.name)?;
        let unit = self.broker_unit.name.clone();
        let script = format!(
            "set -euo pipefail\n\
case \"{unit}\" in\n\
  vellum-broker-smoke-*.service) ;;\n\
  *) echo \"bad unit {unit}\" >&2; exit 42 ;;\n\
esac\n\
systemctl --user stop \"{unit}\" 2>/dev/null || true\n\
systemd-run --user \\\n\
  --unit=\"{unit}\" \\\n\
  --property=Restart=no \\\n\
  --property=StandardOutput=append:{log} \\\n\
  --property=StandardError=append:{log} \\\n\
  --setenv=RUST_LOG=info \\\n\
  {bin} {config}\n\
for i in $(seq 1 60); do\n\
  if curl -fsS \"http://127.0.0.1:{port}/readyz\" >/tmp/vellum-smoke-ready-{short}.json 2>/dev/null; then\n\
    cat /tmp/vellum-smoke-ready-{short}.json\n\
    exit 0\n\
  fi\n\
  sleep 0.25\n\
done\n\
echo \"broker readyz timeout\" >&2\n\
systemctl --user status \"{unit}\" --no-pager || true\n\
tail -n 80 \"{log}\" || true\n\
exit 1\n",
            unit = unit,
            log = unix_path(&self.logs_dir.join("broker.log")),
            bin = unix_path(&self.remote_broker_bin),
            config = unix_path(&self.broker_config_path),
            port = self.config.broker_listen_port,
            short = self.short_id,
        );
        let body = ssh_script(&self.config.ssh_host, &script).await?;
        self.trace.note(format!("broker readyz: {}", body.trim()));
        Ok(())
    }

    pub async fn open_tunnel(&mut self) -> Result<(), LiveSmokeError> {
        // A broker restart invalidates the existing forwarded TCP sessions.
        // Drop the old ssh process before binding the same local port again;
        // attempting the new bind first races with the still-live guard.
        self.tunnel.take();
        tokio::time::sleep(Duration::from_millis(250)).await;
        let tunnel = SshTunnelGuard::open(
            &self.config.ssh_host,
            self.config.local_forward_port,
            self.config.broker_listen_port,
        )
        .await?;
        self.tunnel = Some(tunnel);
        self.trace.note(format!(
            "ssh tunnel 127.0.0.1:{} -> remote 127.0.0.1:{}",
            self.config.local_forward_port, self.config.broker_listen_port
        ));
        Ok(())
    }

    pub async fn connect_client(
        &self,
        device_id: &str,
    ) -> Result<LiveBrokerClient, LiveSmokeError> {
        let url = format!("ws://127.0.0.1:{}/ws", self.config.local_forward_port);
        LiveBrokerClient::connect(&url, device_id).await
    }

    pub async fn restart_broker(&mut self) -> Result<(), LiveSmokeError> {
        self.restart_smoke_unit(&self.broker_unit.clone()).await?;
        let script = format!(
            "set -euo pipefail\n\
for i in $(seq 1 60); do\n\
  if curl -fsS http://127.0.0.1:{}/readyz >/dev/null 2>&1; then exit 0; fi\n\
  sleep 0.25\n\
done\n\
exit 1\n",
            self.config.broker_listen_port
        );
        let _ = ssh_script(&self.config.ssh_host, &script).await?;
        Ok(())
    }

    pub async fn stop_broker(&mut self) -> Result<(), LiveSmokeError> {
        self.stop_smoke_unit(&self.broker_unit.clone()).await
    }

    pub async fn restart_smoke_unit(&self, unit: &SmokeUnit) -> Result<(), LiveSmokeError> {
        SmokeUnit::validate(&unit.name)?;
        let script = format!(
            "set -euo pipefail\n\
case \"{unit}\" in\n\
  vellum-*-smoke-*.service) ;;\n\
  *) echo \"bad unit\" >&2; exit 42 ;;\n\
esac\n\
systemctl --user restart \"{unit}\"\n",
            unit = unit.name,
        );
        let _ = ssh_script(&self.config.ssh_host, &script).await?;
        Ok(())
    }

    pub async fn stop_smoke_unit(&self, unit: &SmokeUnit) -> Result<(), LiveSmokeError> {
        SmokeUnit::validate(&unit.name)?;
        let script = format!(
            "set -euo pipefail\n\
case \"{unit}\" in\n\
  vellum-*-smoke-*.service) ;;\n\
  *) echo \"bad unit\" >&2; exit 42 ;;\n\
esac\n\
systemctl --user stop \"{unit}\" 2>/dev/null || true\n",
            unit = unit.name,
        );
        let _ = ssh_script(&self.config.ssh_host, &script).await?;
        Ok(())
    }

    pub async fn collect_logs(&mut self) -> Result<LiveSmokeTrace, LiveSmokeError> {
        for name in ["app-server.log", "broker.log"] {
            let remote = self.logs_dir.join(name);
            let local = self.local_artifact_dir.join(name);
            let _ = scp_from_remote(&self.config.ssh_host, &unix_path(&remote), &local).await;
        }
        let ready = format!(
            "curl -fsS \"http://127.0.0.1:{port}/readyz\" || true; echo; curl -fsS \"http://127.0.0.1:{port}/version\" || true; echo\n",
            port = self.config.broker_listen_port
        );
        if let Ok(body) = ssh_script(&self.config.ssh_host, &ready).await {
            let path = self.local_artifact_dir.join("readyz-version.txt");
            let _ = tokio::fs::write(&path, body).await;
        }
        self.trace.finished_at = Some(chrono::Utc::now());
        let summary = self.local_artifact_dir.join("summary.json");
        self.trace.write_summary(&summary).await?;
        Ok(self.trace.clone())
    }

    pub async fn codex_version(&self) -> Result<String, LiveSmokeError> {
        let script = format!(
            "set -euo pipefail\n\"{binary}\" --version\n",
            binary = unix_path(&self.config.codex_native),
        );
        let out = ssh_script(&self.config.ssh_host, &script).await?;
        Ok(out.trim().to_string())
    }

    pub async fn generate_schema_artifact(&self) -> Result<(), LiveSmokeError> {
        let out_dir = self.remote_root.join("schema");
        let script = format!(
            "set -euo pipefail\nmkdir -p \"{out}\"\n\"{binary}\" app-server generate-json-schema --out \"{out}\" || true\n",
            out = unix_path(&out_dir),
            binary = unix_path(&self.config.codex_native),
        );
        let _ = ssh_script(&self.config.ssh_host, &script).await;
        Ok(())
    }

    async fn wait_remote_path_exists(
        &self,
        path: &Path,
        timeout: Duration,
    ) -> Result<(), LiveSmokeError> {
        let script = format!(
            "set -euo pipefail\n\
for i in $(seq 1 {iters}); do\n\
  if [ -S \"{path}\" ] || [ -e \"{path}\" ]; then\n\
    exit 0\n\
  fi\n\
  sleep 0.25\n\
done\n\
echo \"path not ready: {path}\" >&2\n\
exit 1\n",
            iters = (timeout.as_millis() / 250).max(1),
            path = unix_path(path),
        );
        let _ = ssh_script(&self.config.ssh_host, &script).await?;
        Ok(())
    }

    pub async fn cleanup(&mut self) -> Result<(), LiveSmokeError> {
        if self.cleaned {
            return Ok(());
        }
        let _ = self.stop_smoke_unit(&self.broker_unit.clone()).await;
        let _ = self.stop_smoke_unit(&self.app_server_unit.clone()).await;
        let script = format!(
            "set -euo pipefail\n\
rm -f \"{socket}\"\n\
if [ \"${{VELLUM_LIVE_CLEAN_ROOTS:-0}}\" = \"1\" ]; then\n\
  case \"{root}\" in\n\
    /home/vellum-test/.local/state/vellum-smoke/*) rm -rf \"{root}\" ;;\n\
  esac\n\
fi\n",
            socket = unix_path(&self.socket_path),
            root = unix_path(&self.remote_root),
        );
        let _ = ssh_script(&self.config.ssh_host, &script).await;
        self.tunnel.take();
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for JetsonLiveFixture {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        let host = self.config.ssh_host.clone();
        for unit in [&self.broker_unit.name, &self.app_server_unit.name] {
            if SmokeUnit::validate(unit).is_err() {
                continue;
            }
            let cmd = format!("systemctl --user stop {unit} 2>/dev/null || true");
            let _ = std::process::Command::new("ssh")
                .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", &host, &cmd])
                .status();
        }
        let socket = unix_path(&self.socket_path);
        if !socket.contains("app-server-control") {
            let cmd = format!("rm -f \"{socket}\"");
            let _ = std::process::Command::new("ssh")
                .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", &host, &cmd])
                .status();
        }
        self.tunnel.take();
        self.cleaned = true;
    }
}

/// Resolve a local broker binary suitable for upload.
/// On Windows this expects a cross-built aarch64-linux binary path via env.
pub fn resolve_local_broker_binary() -> Result<PathBuf, LiveSmokeError> {
    if let Ok(path) = std::env::var("VELLUM_LIVE_BROKER_BIN") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
        return Err(LiveSmokeError::Message(format!(
            "VELLUM_LIVE_BROKER_BIN does not exist: {}",
            path.display()
        )));
    }
    let candidates = [
        PathBuf::from("target/aarch64-unknown-linux-gnu/debug/vellum-remote-broker"),
        PathBuf::from("target/aarch64-unknown-linux-gnu/release/vellum-remote-broker"),
        PathBuf::from("target/aarch64-unknown-linux-musl/debug/vellum-remote-broker"),
        PathBuf::from("target/aarch64-unknown-linux-musl/release/vellum-remote-broker"),
    ];
    for path in candidates {
        if path.exists() {
            return Ok(path);
        }
    }
    Err(LiveSmokeError::Message(
        "no aarch64 broker binary found; set VELLUM_LIVE_BROKER_BIN or cross-build target aarch64-unknown-linux-gnu"
            .into(),
    ))
}

pub fn isolation_guard_summary(fixture: &JetsonLiveFixture) -> serde_json::Value {
    json!({
        "runId": fixture.run_id,
        "shortId": fixture.short_id,
        "codexHome": fixture.codex_home,
        "socketPath": fixture.socket_path,
        "workspace": fixture.workspace,
        "brokerDataDir": fixture.broker_data_dir,
        "appServerUnit": fixture.app_server_unit.name,
        "brokerUnit": fixture.broker_unit.name,
        "blockedProductionCodexHome": PRODUCTION_CODEX_HOME,
        "blockedProductionSockets": [
            PRODUCTION_APP_SERVER_CONTROL_SOCK,
            PRODUCTION_DESKTOP_SSH_WS_SOCK
        ]
    })
}
