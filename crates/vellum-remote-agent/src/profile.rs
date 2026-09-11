//! Isolated Codex managed profiles and durable three-way configuration leases.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml::Value as TomlValue;

use crate::protocol::ProxyStatusView;
use crate::state::{AgentPaths, AgentStateStore};

// The three `model_*` global-override keys are cleared (never set to a
// value) while Vellum runs — see `desired_values` and Desktop's
// `GLOBAL_COMPACTION_OVERRIDE_KEYS` in `src-tauri/src/codex.rs` for why:
// bundled Codex applies them as global overrides *on top of* every model's
// own catalog entry, which would defeat the catalog's per-model auto-compact
// contract (most dangerously for Grok/disabled routes) if left in place.
// `pub` so Desktop's `remote/deployment.rs::plan` can assert its own
// user-facing `managed_changes` list stays exactly in sync with this one
// instead of maintaining a second hand-copied list that can silently drift
// (see the "Deployment plan omits managed compaction keys" finding).
pub const MANAGED_PATHS: &[&str] = &[
    "model_provider",
    "openai_base_url",
    "model_catalog_json",
    "model_providers",
    "model_context_window",
    "model_auto_compact_token_limit",
    "model_auto_compact_token_limit_scope",
    "features.standalone_web_search",
    "features.remote_compaction_v2",
    "features.auto_compaction",
    "features.enable_request_compression",
    "features.image_generation",
    "cli_auth_credentials_store",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ProfileMode {
    Managed,
    AdoptExisting,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileRecord {
    pub profile_id: String,
    pub mode: ProfileMode,
    pub codex_home: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InjectionPlan {
    pub profile: ProfileRecord,
    pub proxy_base_url: String,
    pub restart_required: bool,
    pub managed_changes: Vec<ManagedChange>,
    pub conflicts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedChange {
    pub path: String,
    pub before: Option<TomlValue>,
    pub after: Option<TomlValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigLease {
    schema_version: u32,
    lease_id: String,
    host_id: String,
    profile_id: String,
    mode: ProfileMode,
    codex_home: String,
    created_at: DateTime<Utc>,
    proxy_install_id: String,
    proxy_host_port: u16,
    original_sha256: String,
    applied_sha256: String,
    #[serde(default)]
    catalog_original_exists: bool,
    #[serde(default)]
    catalog_original_sha256: Option<String>,
    #[serde(default)]
    catalog_applied_sha256: Option<String>,
    #[serde(default)]
    codex_version: Option<String>,
    original_managed_values: BTreeMap<String, Option<TomlValue>>,
    applied_managed_values: BTreeMap<String, Option<TomlValue>>,
    state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RestoreResult {
    pub status: String,
    pub safe_fields_restored: Vec<String>,
    pub conflicts: Vec<ManagedChange>,
    pub manual_action_required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedRuntimeStatus {
    pub profile_id: String,
    pub app_server_unit: String,
    pub broker_unit: String,
    pub app_server_active: bool,
    pub broker_active: bool,
    pub socket_path: String,
    pub broker_port: u16,
    pub ready: bool,
}

#[derive(Clone)]
pub struct ProfileManager {
    store: AgentStateStore,
}

impl ProfileManager {
    pub fn new(store: AgentStateStore) -> Self {
        Self { store }
    }

    pub fn aggregate_statuses(&self) -> Result<Vec<serde_json::Value>, String> {
        let root = &self.store.paths().profiles_dir;
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut profile_ids = fs::read_dir(root)
            .map_err(|error| format!("failed reading managed profiles: {error}"))?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().join("profile.json").is_file())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>();
        profile_ids.sort();
        let mut statuses = Vec::with_capacity(profile_ids.len());
        for profile_id in profile_ids {
            let profile = self.inspect(&profile_id)?;
            let lease = self.lease_status(&profile_id)?;
            let runtime = self.runtime_status(&profile_id, None)?;
            statuses.push(serde_json::json!({
                "profile": profile,
                "lease": lease,
                "runtime": runtime,
            }));
        }
        Ok(statuses)
    }

    pub fn create_managed(
        &self,
        profile_id: &str,
        auth_json: Option<&str>,
    ) -> Result<ProfileRecord, String> {
        validate_id(profile_id)?;
        let profile_root = self.store.paths().profiles_dir.join(profile_id);
        reject_symlink(&profile_root)?;
        let codex_home = profile_root.join("codex-home");
        self.create(profile_id, ProfileMode::Managed, codex_home, auth_json)
    }

    pub fn adopt_existing(
        &self,
        profile_id: &str,
        codex_home: &Path,
        explicit_adopt: bool,
    ) -> Result<ProfileRecord, String> {
        if !explicit_adopt {
            return Err("ExplicitAdoptRequired: existing profiles are never auto-adopted".into());
        }
        validate_id(profile_id)?;
        let canonical = codex_home
            .canonicalize()
            .map_err(|error| format!("invalid existing CODEX_HOME: {error}"))?;
        verify_adopt_path(&canonical)?;
        self.create(profile_id, ProfileMode::AdoptExisting, canonical, None)
    }

    fn create(
        &self,
        profile_id: &str,
        mode: ProfileMode,
        codex_home: PathBuf,
        auth_json: Option<&str>,
    ) -> Result<ProfileRecord, String> {
        self.store.paths().ensure()?;
        fs::create_dir_all(&codex_home)
            .map_err(|error| format!("failed creating managed CODEX_HOME: {error}"))?;
        reject_symlink(&codex_home)?;
        set_private_dir(&codex_home)?;
        if let Some(auth) = auth_json {
            let _: serde_json::Value = serde_json::from_str(auth)
                .map_err(|error| format!("invalid auth JSON: {error}"))?;
            atomic_write(&codex_home.join("auth.json"), auth.as_bytes(), 0o600)?;
        }
        let record = ProfileRecord {
            profile_id: profile_id.into(),
            mode,
            codex_home: codex_home.to_string_lossy().to_string(),
            created_at: Utc::now(),
        };
        atomic_json(&self.profile_path(profile_id), &record, 0o600)?;
        Ok(record)
    }

    pub fn inspect(&self, profile_id: &str) -> Result<ProfileRecord, String> {
        validate_id(profile_id)?;
        read_json(&self.profile_path(profile_id), "profile")
    }

    pub fn plan_injection(
        &self,
        profile_id: &str,
        proxy: &ProxyStatusView,
    ) -> Result<InjectionPlan, String> {
        require_healthy_proxy(proxy)?;
        let profile = self.inspect(profile_id)?;
        let current = read_toml(&PathBuf::from(&profile.codex_home).join("config.toml"))?;
        let desired = desired_values(&profile, proxy)?;
        let managed_changes = MANAGED_PATHS
            .iter()
            .map(|path| ManagedChange {
                path: (*path).into(),
                before: get_path(&current, path),
                after: desired.get(*path).cloned().flatten(),
            })
            .collect();
        let restart_required = codex_home_has_active_process(&PathBuf::from(&profile.codex_home));
        Ok(InjectionPlan {
            profile,
            proxy_base_url: proxy_base_url(proxy)?,
            restart_required,
            managed_changes,
            conflicts: Vec::new(),
        })
    }

    pub fn inject(
        &self,
        profile_id: &str,
        proxy: &ProxyStatusView,
        catalog_json: &str,
    ) -> Result<InjectionPlan, String> {
        let plan = self.plan_injection(profile_id, proxy)?;
        let profile_home = PathBuf::from(&plan.profile.codex_home);
        let config_path = profile_home.join("config.toml");
        let catalog_path = profile_home.join("vellum-model-catalog.json");
        let _: serde_json::Value = serde_json::from_str(catalog_json)
            .map_err(|error| format!("invalid model catalog JSON: {error}"))?;
        let original_catalog = fs::read(&catalog_path).ok();
        if let Some(bytes) = original_catalog.as_deref() {
            atomic_write(&self.catalog_backup_path(profile_id), bytes, 0o600)?;
        }
        atomic_write(&catalog_path, catalog_json.as_bytes(), 0o600)?;

        let original_raw = read_text(&config_path)?;
        let mut applied = parse_toml(&original_raw)?;
        let desired = desired_values(&plan.profile, proxy)?;
        let original_values = MANAGED_PATHS
            .iter()
            .map(|path| ((*path).into(), get_path(&applied, path)))
            .collect();
        for (path, value) in &desired {
            set_path(&mut applied, path, value.clone());
        }
        let applied_raw = toml::to_string_pretty(&applied).map_err(|error| error.to_string())?;
        let lease_id = ulid::Ulid::new().to_string();
        let mut lease = ConfigLease {
            schema_version: 1,
            lease_id: lease_id.clone(),
            host_id: self.store.load()?.host_id,
            profile_id: profile_id.into(),
            mode: plan.profile.mode.clone(),
            codex_home: plan.profile.codex_home.clone(),
            created_at: Utc::now(),
            proxy_install_id: proxy
                .install_id
                .clone()
                .ok_or_else(|| "healthy proxy missing install id".to_string())?,
            proxy_host_port: proxy
                .host_port
                .ok_or_else(|| "healthy proxy missing port".to_string())?,
            original_sha256: hash(&original_raw),
            applied_sha256: hash(&applied_raw),
            catalog_original_exists: original_catalog.is_some(),
            catalog_original_sha256: original_catalog.as_deref().map(hash_bytes),
            catalog_applied_sha256: Some(hash_bytes(catalog_json.as_bytes())),
            codex_version: crate::host_probe::probe_host().codex_version,
            original_managed_values: original_values,
            applied_managed_values: desired,
            state: "leasePrepared".into(),
        };
        let lease_path = self.lease_path(profile_id);
        atomic_json(&lease_path, &lease, 0o600)?;
        atomic_write(&config_path, applied_raw.as_bytes(), 0o600)?;
        let verified = read_toml(&config_path)?;
        if !lease
            .applied_managed_values
            .iter()
            .all(|(path, expected)| get_path(&verified, path) == *expected)
        {
            return Err("InjectionVerificationFailed: managed fields differ after write".into());
        }
        lease.state = if plan.restart_required {
            "restartRequired"
        } else {
            "active"
        }
        .into();
        atomic_json(&lease_path, &lease, 0o600)?;
        Ok(plan)
    }

    pub fn restore(&self, profile_id: &str) -> Result<RestoreResult, String> {
        let lease_path = self.lease_path(profile_id);
        let mut lease: ConfigLease = read_json(&lease_path, "lease")?;
        if lease.profile_id != profile_id {
            return Err("LeaseProfileMismatch".into());
        }
        let config_path = PathBuf::from(&lease.codex_home).join("config.toml");
        let mut current = read_toml(&config_path)?;
        let mut restored = Vec::new();
        let mut conflicts = Vec::new();
        for path in MANAGED_PATHS {
            let now = get_path(&current, path);
            let applied = lease.applied_managed_values.get(*path).cloned().flatten();
            let original = lease.original_managed_values.get(*path).cloned().flatten();
            if now == applied {
                set_path(&mut current, path, original.clone());
                restored.push((*path).to_string());
            } else if now != original {
                conflicts.push(ManagedChange {
                    path: (*path).into(),
                    before: original,
                    after: now,
                });
            }
        }
        let encoded = toml::to_string_pretty(&current).map_err(|error| error.to_string())?;
        atomic_write(&config_path, encoded.as_bytes(), 0o600)?;
        let catalog_path = PathBuf::from(&lease.codex_home).join("vellum-model-catalog.json");
        let current_catalog = fs::read(&catalog_path).ok();
        let current_catalog_hash = current_catalog.as_deref().map(hash_bytes);
        if current_catalog_hash == lease.catalog_applied_sha256 {
            if lease.catalog_original_exists {
                let backup = fs::read(self.catalog_backup_path(profile_id))
                    .map_err(|error| format!("catalog backup missing: {error}"))?;
                if Some(hash_bytes(&backup)) != lease.catalog_original_sha256 {
                    return Err("CatalogBackupHashMismatch".into());
                }
                atomic_write(&catalog_path, &backup, 0o600)?;
            } else {
                match fs::remove_file(&catalog_path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(format!("remove managed catalog: {error}")),
                }
            }
            restored.push("model_catalog_json_file".into());
        } else if current_catalog_hash != lease.catalog_original_sha256 {
            conflicts.push(ManagedChange {
                path: "model_catalog_json_file".into(),
                before: lease.catalog_original_sha256.clone().map(TomlValue::String),
                after: current_catalog_hash.map(TomlValue::String),
            });
        }
        let manual = !conflicts.is_empty();
        lease.state = if manual { "configConflict" } else { "restored" }.into();
        atomic_json(&lease_path, &lease, 0o600)?;
        Ok(RestoreResult {
            status: lease.state,
            safe_fields_restored: restored,
            conflicts,
            manual_action_required: manual,
        })
    }

    pub fn lease_status(&self, profile_id: &str) -> Result<serde_json::Value, String> {
        validate_id(profile_id)?;
        let path = self.lease_path(profile_id);
        if !path.exists() {
            return Ok(serde_json::json!({"present": false}));
        }
        let mut lease: ConfigLease = read_json(&path, "lease")?;
        if lease.state == "leasePrepared" {
            let current = read_text(&PathBuf::from(&lease.codex_home).join("config.toml"))?;
            lease.state = if hash(&current) == lease.applied_sha256 {
                "active".into()
            } else if hash(&current) == lease.original_sha256 {
                "leasePrepared".into()
            } else {
                "recoveryRequired".into()
            };
            atomic_json(&path, &lease, 0o600)?;
        }
        Ok(serde_json::json!({
            "present": true,
            "leaseId": lease.lease_id,
            "profileId": lease.profile_id,
            "state": lease.state,
            "catalogHash": lease.catalog_applied_sha256,
        }))
    }

    pub fn activate_after_restart(&self, profile_id: &str) -> Result<(), String> {
        validate_id(profile_id)?;
        let path = self.lease_path(profile_id);
        if !path.exists() {
            return Ok(());
        }
        let mut lease: ConfigLease = read_json(&path, "lease")?;
        if lease.state == "active" {
            return Ok(());
        }
        if lease.state != "restartRequired" {
            return Ok(());
        }
        let home = PathBuf::from(&lease.codex_home);
        let current = read_toml(&home.join("config.toml"))?;
        if !lease
            .applied_managed_values
            .iter()
            .all(|(path, expected)| get_path(&current, path) == *expected)
        {
            return Err("RestartActivationFailed: managed config changed before restart".into());
        }
        let catalog = fs::read(home.join("vellum-model-catalog.json"))
            .map_err(|error| format!("RestartActivationFailed: catalog unavailable: {error}"))?;
        if Some(hash_bytes(&catalog)) != lease.catalog_applied_sha256 {
            return Err("RestartActivationFailed: managed catalog changed before restart".into());
        }
        lease.state = "active".into();
        atomic_json(&path, &lease, 0o600)
    }

    pub fn start_managed(
        &self,
        profile_id: &str,
        broker_port: Option<u16>,
    ) -> Result<ManagedRuntimeStatus, String> {
        let profile = self.inspect(profile_id)?;
        let lease: ConfigLease = read_json(&self.lease_path(profile_id), "lease")?;
        if lease.state != "active" {
            return Err(format!(
                "InjectionNotActive: lease state is {}",
                lease.state
            ));
        }
        let home = PathBuf::from(&profile.codex_home);
        let socket = managed_socket_path(&home, profile_id)?;
        let run_dir = socket
            .parent()
            .ok_or_else(|| "managed socket has no parent".to_string())?;
        fs::create_dir_all(run_dir).map_err(|e| e.to_string())?;
        set_private_dir(run_dir)?;
        let port = broker_port
            .or_else(|| configured_broker_port(&PathBuf::from(&profile.codex_home)))
            .unwrap_or(45100);
        let codex = crate::host_probe::probe_host()
            .codex_binary
            .ok_or_else(|| "CodexBinaryMissing".to_string())?;
        let app_unit = app_unit(profile_id);
        let broker_unit = broker_unit(profile_id);
        // Tear down any leftover broker unit so a previous diagnostic spawn
        // cannot keep an unauthenticated listener around after repair.
        stop_unit(&broker_unit)?;
        stop_unit(&app_unit)?;
        match fs::remove_file(&socket) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("failed removing stale app-server socket: {error}")),
        }
        // This path launches the app-server directly via `systemd-run`,
        // bypassing Codex's own `app-server daemon` control commands (see
        // `native_codex.rs::run_daemon_command`) entirely — a separate,
        // earlier lifecycle mechanism from the M31 native-daemon path. It
        // still needs the same `VELLUM_BOUNDARY_KEY` env var Codex's
        // `env_http_headers` resolves the boundary header from
        // (`desired_values` above), or every request from a Codex started
        // this way is rejected by the proxy's unconditional boundary check.
        //
        // The secret itself must never appear in `systemd-run`'s own argv or
        // in the transient unit's stored environment metadata (both
        // inspectable via `ps`/`systemctl show`/the journal) — so this does
        // not use `--setenv=VELLUM_BOUNDARY_KEY=<value>` the way
        // `CODEX_HOME` is passed below. Instead it uses systemd's
        // `LoadCredential=`, which exposes the *file* at
        // `$CREDENTIALS_DIRECTORY/<id>` to the unit — a private, per-unit
        // path only that unit's own user can read — and a tiny inline
        // `sh -c` wrapper reads that file, exports it as an env var, and
        // `exec`s the real Codex binary. Only the credential *file path* is
        // ever visible in process listings, never the secret value. We
        // already have this exact file on disk from
        // `configuration::put_credential`, so no separate credential file is
        // written here.
        //
        // This lease is already confirmed `active` above (the profile's
        // `config.toml` currently carries the `env_http_headers` reference),
        // so a missing boundary key here is a real problem, not a benign
        // "Vellum was never configured for this Codex instance" case — fail
        // the whole start rather than silently launching a daemon that can
        // never successfully call the proxy.
        let boundary_key_path = self
            .store
            .paths()
            .secrets_dir
            .join(vellum_proxy_runtime::BOUNDARY_CREDENTIAL_ID);
        if !boundary_key_path.is_file() {
            return Err(format!(
                "BoundaryKeyMissing: {} is not provisioned; this profile's lease is active and \
                 requires it (see configuration::put_credential)",
                boundary_key_path.display()
            ));
        }
        const CREDENTIAL_ID: &str = "vellum-boundary-key";
        let systemd_run_args = [
            "--user".to_string(),
            format!("--unit={app_unit}"),
            "--property=Restart=on-failure".to_string(),
            format!("--setenv=CODEX_HOME={}", home.to_string_lossy()),
            format!(
                "--property=LoadCredential={CREDENTIAL_ID}:{}",
                boundary_key_path.display()
            ),
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "export {}=\"$(cat \"$CREDENTIALS_DIRECTORY/{CREDENTIAL_ID}\")\"; exec \"$@\"",
                vellum_proxy_runtime::BOUNDARY_KEY_ENV_VAR
            ),
            "sh".to_string(),
            codex.clone(),
            "app-server".to_string(),
            "--listen".to_string(),
            format!("unix://{}", socket.to_string_lossy()),
        ];
        run_checked(
            Command::new("systemd-run").args(&systemd_run_args),
            "start managed Codex app-server",
        )?;
        wait_for(|| socket.exists(), "Codex app-server socket")?;

        #[cfg(feature = "diagnostic-broker")]
        spawn_diagnostic_broker(profile_id, &home, &codex, &socket, port, &broker_unit)?;

        self.runtime_status(profile_id, Some(port))
    }

    pub fn stop_managed(&self, profile_id: &str) -> Result<ManagedRuntimeStatus, String> {
        validate_id(profile_id)?;
        let prior = self.runtime_status(profile_id, None)?;
        stop_unit(&prior.broker_unit)?;
        stop_unit(&prior.app_server_unit)?;
        self.runtime_status(profile_id, Some(prior.broker_port))
    }

    pub fn runtime_status(
        &self,
        profile_id: &str,
        broker_port: Option<u16>,
    ) -> Result<ManagedRuntimeStatus, String> {
        let profile = self.inspect(profile_id)?;
        let home = PathBuf::from(profile.codex_home);
        let socket = managed_socket_path(&home, profile_id)?;
        let app_server_unit = app_unit(profile_id);
        let broker_unit = broker_unit(profile_id);
        let app_server_active = unit_active(&app_server_unit);
        let broker_active = unit_active(&broker_unit);
        let port = broker_port
            .or_else(|| configured_broker_port(&home))
            .unwrap_or(45100);
        Ok(ManagedRuntimeStatus {
            profile_id: profile_id.into(),
            app_server_unit,
            broker_unit,
            app_server_active,
            broker_active,
            socket_path: socket.to_string_lossy().to_string(),
            broker_port: port,
            ready: managed_runtime_is_ready(app_server_active, socket.exists()),
        })
    }

    fn profile_path(&self, id: &str) -> PathBuf {
        self.store
            .paths()
            .profiles_dir
            .join(id)
            .join("profile.json")
    }
    fn lease_path(&self, id: &str) -> PathBuf {
        self.store.paths().leases_dir.join(format!("{id}.json"))
    }
    fn catalog_backup_path(&self, id: &str) -> PathBuf {
        self.store
            .paths()
            .leases_dir
            .join(format!("{id}.catalog.original"))
    }
}

/// Whether `profile_id` currently has a Vellum-authored config injection
/// applied — i.e. `config.toml` was written by `inject()` and has not yet
/// been cleaned up by `restore()`. Any lease still on disk means the
/// `[model_providers.vellum]` table (and therefore the boundary-key
/// requirement) is present in that profile's `config.toml`, regardless of
/// the lease's exact verification state (mirrors Desktop's
/// `has_active_lease` in `src-tauri/src/codex.rs`).
///
/// Used by `native_codex.rs`'s daemon-lifecycle commands to decide whether a
/// missing boundary key is a real problem (the profile expects it) or
/// benign (this Codex instance was never pointed at Vellum at all).
pub fn profile_lease_is_active(paths: &AgentPaths, profile_id: &str) -> bool {
    paths.leases_dir.join(format!("{profile_id}.json")).exists()
}

fn app_unit(profile_id: &str) -> String {
    format!("vellum-codex-{profile_id}.service")
}
fn broker_unit(profile_id: &str) -> String {
    format!("vellum-broker-{profile_id}.service")
}

/// Production readiness is the Codex app-server only. The broker listener is
/// not part of the live Remote path (ADR-001) and must not keep `ready` false
/// after a successful `start_managed`.
pub(crate) fn managed_runtime_is_ready(app_server_active: bool, socket_exists: bool) -> bool {
    app_server_active && socket_exists
}

/// Compiled into the agent only with `--features diagnostic-broker`. Default
/// release builds do not contain this spawn path.
pub const DIAGNOSTIC_BROKER_IN_BINARY: bool = cfg!(feature = "diagnostic-broker");

#[cfg(feature = "diagnostic-broker")]
fn spawn_diagnostic_broker(
    profile_id: &str,
    home: &Path,
    codex: &str,
    socket: &Path,
    port: u16,
    broker_unit: &str,
) -> Result<(), String> {
    let broker_config = home.join("broker.toml");
    let broker_binary =
        std::env::var("VELLUM_REMOTE_BROKER_BIN").unwrap_or_else(|_| "vellum-remote-broker".into());
    let data_dir = home.join("broker-data");
    let allowed_root = dirs::home_dir().unwrap_or_else(|| home.to_path_buf());
    let config = format!(
        "broker_id = \"vellum-{profile_id}\"\ndata_dir = {:?}\nlisten_addr = \"127.0.0.1:{port}\"\ncodex_binary = {:?}\ncodex_home = {:?}\napp_server_socket = {:?}\nallowed_versions = []\nallowed_roots = [{:?}]\nrequire_auth = false\nlocal_only = true\nwriter_lease_ttl_secs = 30\nwriter_lease_heartbeat_secs = 10\nwriter_lease_disconnect_grace_secs = 15\n",
        data_dir.to_string_lossy(),
        codex,
        home.to_string_lossy(),
        socket.to_string_lossy(),
        allowed_root.to_string_lossy()
    );
    let _: toml::Value = config
        .parse()
        .map_err(|e| format!("generated broker config invalid: {e}"))?;
    atomic_write(&broker_config, config.as_bytes(), 0o600)?;
    run_checked(
        Command::new("systemd-run").args([
            "--user",
            &format!("--unit={broker_unit}"),
            "--property=Restart=on-failure",
            &broker_binary,
            &broker_config.to_string_lossy(),
        ]),
        "start managed remote broker",
    )?;
    wait_for(
        || std::net::TcpStream::connect(("127.0.0.1", port)).is_ok(),
        "remote broker listener",
    )
}

/// Linux limits Unix-domain socket paths to roughly 108 bytes. Managed
/// CODEX_HOME paths can be arbitrarily deep, so sockets live in the user's
/// short runtime directory and are keyed by the full profile identity.
pub(crate) fn managed_socket_path(home: &Path, profile_id: &str) -> Result<PathBuf, String> {
    let (uid, _) = crate::permissions::host_runtime_identity()?;
    let runtime_root = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{uid}")));
    let mut hasher = Sha256::new();
    hasher.update(home.to_string_lossy().as_bytes());
    hasher.update([0]);
    hasher.update(profile_id.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    Ok(runtime_root
        .join("vellum-remote")
        .join(format!("{}.sock", &digest[..20])))
}
fn unit_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", unit])
        .status()
        .is_ok_and(|s| s.success())
}
fn stop_unit(unit: &str) -> Result<(), String> {
    let status = Command::new("systemctl")
        .args(["--user", "stop", unit])
        .status();
    match status {
        Ok(_) => {
            let _ = Command::new("systemctl")
                .args(["--user", "reset-failed", unit])
                .status();
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err("systemctl is required for managed profiles".into())
        }
        Err(e) => Err(e.to_string()),
    }
}
fn run_checked(command: &mut Command, label: &str) -> Result<(), String> {
    let output = command.output().map_err(|e| format!("{label}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{label} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}
fn wait_for(mut ready: impl FnMut() -> bool, label: &str) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if ready() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(format!("{label} did not become ready"))
}

fn codex_home_has_active_process(codex_home: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        let expected = format!("CODEX_HOME={}", codex_home.to_string_lossy());
        let Ok(entries) = fs::read_dir("/proc") else {
            return false;
        };
        for entry in entries.flatten().filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .chars()
                .all(|c| c.is_ascii_digit())
        }) {
            let root = entry.path();
            let Ok(command) = fs::read(root.join("cmdline")) else {
                continue;
            };
            if !String::from_utf8_lossy(&command).contains("codex") {
                continue;
            }
            if let Ok(environment) = fs::read(root.join("environ")) {
                if environment
                    .split(|byte| *byte == 0)
                    .any(|item| item == expected.as_bytes())
                {
                    return true;
                }
            }
        }
    }
    let _ = codex_home;
    false
}

fn reject_symlink(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "PathAliasRejected: {} is a symlink",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("failed inspecting {}: {error}", path.display())),
    }
}

fn verify_adopt_path(codex_home: &Path) -> Result<(), String> {
    reject_symlink(codex_home)?;
    reject_symlink(&codex_home.join("config.toml"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let expected_uid = crate::permissions::host_runtime_identity()?.0;
        let metadata = fs::metadata(codex_home).map_err(|error| error.to_string())?;
        if metadata.uid() != expected_uid {
            return Err(format!(
                "OwnershipMismatch: {} belongs to uid {}, expected {}",
                codex_home.display(),
                metadata.uid(),
                expected_uid
            ));
        }
    }
    Ok(())
}

/// Provider id Vellum manages remotely. Matches Desktop's `VELLUM_PROVIDER_NAME`
/// (see `src-tauri/src/codex.rs`) — kept as a distinct local constant since
/// this crate does not depend on the `vellum` binary crate.
const REMOTE_PROVIDER_ID: &str = "vellum";

/// Build the `[model_providers.vellum]` table the remote-managed Codex routes
/// through.
///
/// Mirrors Desktop's `write_vellum_provider` (`src-tauri/src/codex.rs`):
/// `name = "OpenAI"` is what Codex's `supports_remote_compaction()` gates
/// real auto-compact scheduling on, keyed on display name rather than
/// provider id or any capability flag. Unlike Desktop, the boundary key
/// itself never appears in this table — `env_http_headers` carries only the
/// *environment variable name* Codex resolves the header value from at
/// request time, so the secret never rides along in the deployment plan, the
/// public diff, or the injection lease JSON this table's value ends up
/// serialized into (see `BOUNDARY_KEY_ENV_VAR`).
fn model_providers_table(proxy: &ProxyStatusView) -> Result<TomlValue, String> {
    let mut provider = toml::map::Map::new();
    provider.insert("name".into(), TomlValue::String("OpenAI".into()));
    provider.insert("base_url".into(), TomlValue::String(proxy_base_url(proxy)?));
    provider.insert("wire_api".into(), TomlValue::String("responses".into()));
    provider.insert("requires_openai_auth".into(), TomlValue::Boolean(true));
    provider.insert("supports_websockets".into(), TomlValue::Boolean(true));
    let mut headers = toml::map::Map::new();
    headers.insert(
        vellum_proxy_runtime::BOUNDARY_KEY_HEADER.into(),
        TomlValue::String(vellum_proxy_runtime::BOUNDARY_KEY_ENV_VAR.into()),
    );
    provider.insert("env_http_headers".into(), TomlValue::Table(headers));
    let mut providers = toml::map::Map::new();
    providers.insert(REMOTE_PROVIDER_ID.into(), TomlValue::Table(provider));
    Ok(TomlValue::Table(providers))
}

fn desired_values(
    profile: &ProfileRecord,
    proxy: &ProxyStatusView,
) -> Result<BTreeMap<String, Option<TomlValue>>, String> {
    let catalog = PathBuf::from(&profile.codex_home)
        .join("vellum-model-catalog.json")
        .to_string_lossy()
        .to_string();
    Ok(BTreeMap::from([
        (
            "model_provider".into(),
            Some(TomlValue::String(REMOTE_PROVIDER_ID.into())),
        ),
        // The built-in OpenAI provider cannot carry the boundary key — same
        // reasoning as Desktop's `relinquish_legacy_openai_base_url`. Routing
        // goes through `model_providers.vellum` instead.
        ("openai_base_url".into(), None),
        (
            "model_catalog_json".into(),
            Some(TomlValue::String(catalog)),
        ),
        (
            "model_providers".into(),
            Some(model_providers_table(proxy)?),
        ),
        // Bundled Codex applies these three as *global* overrides on top of
        // every model's own catalog entry — never a value Vellum sets, only
        // ever cleared while it runs (see the `MANAGED_PATHS` doc comment).
        ("model_context_window".into(), None),
        ("model_auto_compact_token_limit".into(), None),
        ("model_auto_compact_token_limit_scope".into(), None),
        (
            "features.standalone_web_search".into(),
            Some(TomlValue::Boolean(true)),
        ),
        // Requires the `name = "OpenAI"` capability gate above. See
        // `write_vellum_provider` in `src-tauri/src/codex.rs` for why all
        // three are pinned explicitly rather than left to Codex's defaults.
        (
            "features.remote_compaction_v2".into(),
            Some(TomlValue::Boolean(true)),
        ),
        // The real scheduler gate: without this, Codex's automatic
        // compaction pass never runs regardless of any threshold.
        (
            "features.auto_compaction".into(),
            Some(TomlValue::Boolean(true)),
        ),
        (
            "features.enable_request_compression".into(),
            Some(TomlValue::Boolean(false)),
        ),
        (
            "features.image_generation".into(),
            Some(TomlValue::Boolean(false)),
        ),
        (
            "cli_auth_credentials_store".into(),
            Some(TomlValue::String("file".into())),
        ),
    ]))
}

fn require_healthy_proxy(proxy: &ProxyStatusView) -> Result<(), String> {
    if !proxy.present
        || !proxy.running
        || !proxy.ready
        || proxy.install_id.is_none()
        || proxy.host_port.is_none()
    {
        return Err("ProxyUnhealthy: injection requires the expected ready managed proxy".into());
    }
    Ok(())
}

fn proxy_base_url(proxy: &ProxyStatusView) -> Result<String, String> {
    Ok(format!(
        "http://127.0.0.1:{}/v1",
        proxy
            .host_port
            .ok_or_else(|| "proxy port missing".to_string())?
    ))
}

fn configured_broker_port(codex_home: &Path) -> Option<u16> {
    let raw = fs::read_to_string(codex_home.join("broker.toml")).ok()?;
    let config = raw.parse::<TomlValue>().ok()?;
    let listen = config.get("listen_addr")?.as_str()?;
    listen.rsplit_once(':')?.1.parse().ok()
}
fn hash(raw: &str) -> String {
    hex::encode(Sha256::digest(raw.as_bytes()))
}
fn hash_bytes(raw: &[u8]) -> String {
    hex::encode(Sha256::digest(raw))
}
fn read_text(path: &Path) -> Result<String, String> {
    match fs::read_to_string(path) {
        Ok(v) => Ok(v),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("failed reading {}: {e}", path.display())),
    }
}
fn parse_toml(raw: &str) -> Result<TomlValue, String> {
    if raw.trim().is_empty() {
        Ok(TomlValue::Table(Default::default()))
    } else {
        raw.parse()
            .map_err(|error| format!("invalid config.toml: {error}"))
    }
}
fn read_toml(path: &Path) -> Result<TomlValue, String> {
    parse_toml(&read_text(path)?)
}

fn get_path(root: &TomlValue, path: &str) -> Option<TomlValue> {
    path.split('.')
        .try_fold(root, |value, key| value.as_table()?.get(key))
        .cloned()
}

fn set_path(root: &mut TomlValue, path: &str, value: Option<TomlValue>) {
    let parts = path.split('.').collect::<Vec<_>>();
    let mut table = root.as_table_mut().expect("root config is table");
    for key in &parts[..parts.len() - 1] {
        table
            .entry((*key).to_string())
            .or_insert_with(|| TomlValue::Table(Default::default()));
        table = table
            .get_mut(*key)
            .and_then(TomlValue::as_table_mut)
            .expect("managed path table");
    }
    let key = parts[parts.len() - 1];
    match value {
        Some(value) => {
            table.insert(key.into(), value);
        }
        None => {
            table.remove(key);
        }
    }
}

fn validate_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        Err("invalid profile id".into())
    } else {
        Ok(())
    }
}
fn set_private_dir(_path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    Ok(())
}
fn atomic_json<T: Serialize>(path: &Path, value: &T, mode: u32) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    atomic_write(path, &bytes, mode)
}
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path, label: &str) -> Result<T, String> {
    let raw = fs::read(path).map_err(|e| format!("failed reading {label}: {e}"))?;
    serde_json::from_slice(&raw).map_err(|e| format!("{label} is corrupt: {e}"))
}
fn atomic_write(path: &Path, bytes: &[u8], _mode: u32) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
    let mut opts = OpenOptions::new();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(_mode);
    }
    let mut file = opts.open(&tmp).map_err(|e| e.to_string())?;
    file.write_all(bytes).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AgentPaths;

    fn proxy() -> ProxyStatusView {
        ProxyStatusView {
            present: true,
            running: true,
            ready: true,
            install_id: Some("install-1".into()),
            host_port: Some(15721),
            ..Default::default()
        }
    }

    #[test]
    fn ready_is_app_server_and_socket_only() {
        assert!(managed_runtime_is_ready(true, true));
        assert!(!managed_runtime_is_ready(false, true));
        assert!(!managed_runtime_is_ready(true, false));
        assert!(!managed_runtime_is_ready(false, false));
    }

    #[test]
    fn ready_does_not_consult_broker_listener_state() {
        // Production `runtime_status` no longer ANDs broker_active or TCP
        // :45100 into ready. The helper takes only app-server facts so a
        // start_managed that skipped the broker still reports ready.
        assert!(managed_runtime_is_ready(true, true));
        // Compile-time gate (kept as a const assertion: the release agent
        // must not compile the managed-broker spawn path).
        const _: () = assert!(!DIAGNOSTIC_BROKER_IN_BINARY);
    }

    #[test]
    fn managed_socket_stays_below_linux_sun_len_for_deep_profiles() {
        let home = PathBuf::from("/home/vellum-test/.local/state/vellum-smoke/manager")
            .join("a".repeat(80))
            .join("profiles")
            .join("b".repeat(80))
            .join("codex-home");
        let socket = managed_socket_path(&home, &format!("m16-{}", "c".repeat(80))).unwrap();
        assert!(socket.to_string_lossy().len() < 100, "{}", socket.display());
        assert!(!socket.starts_with(&home));
    }

    #[test]
    fn managed_profile_is_isolated_and_three_way_restore_preserves_edits() {
        let temp = tempfile::tempdir().unwrap();
        let store = AgentStateStore::new(AgentPaths::from_root(temp.path().join("state")));
        let manager = ProfileManager::new(store);
        let profile = manager.create_managed("profile-1", None).unwrap();
        assert!(PathBuf::from(&profile.codex_home).starts_with(temp.path().join("state/profiles")));
        let config = PathBuf::from(&profile.codex_home).join("config.toml");
        fs::write(&config, "approval_policy = \"on-request\"\n").unwrap();
        manager
            .inject("profile-1", &proxy(), "{\"models\":[]}")
            .unwrap();
        let mut current = read_toml(&config).unwrap();
        set_path(
            &mut current,
            "approval_policy",
            Some(TomlValue::String("never".into())),
        );
        fs::write(&config, toml::to_string_pretty(&current).unwrap()).unwrap();
        let restored = manager.restore("profile-1").unwrap();
        assert!(!restored.manual_action_required);
        let value = read_toml(&config).unwrap();
        assert_eq!(
            get_path(&value, "approval_policy"),
            Some(TomlValue::String("never".into()))
        );
        assert_eq!(get_path(&value, "openai_base_url"), None);
    }

    /// Codex's `supports_remote_compaction()` gates the real `compaction_trigger`
    /// round-trip on provider *display name*, not provider id or capability
    /// flags — the remote-managed profile must present `name = "OpenAI"` under
    /// provider id `vellum` for the same reason Desktop's `write_vellum_provider`
    /// does (see `src-tauri/src/codex.rs`). The boundary key itself must never
    /// land in the injected config: only the *env var name* it will be read
    /// from at request time.
    #[test]
    fn injection_presents_the_openai_capability_name_without_embedding_the_boundary_secret() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProfileManager::new(AgentStateStore::new(AgentPaths::from_root(
            temp.path().join("state"),
        )));
        let profile = manager.create_managed("profile-1", None).unwrap();
        manager
            .inject("profile-1", &proxy(), "{\"models\":[]}")
            .unwrap();

        let config = read_toml(&PathBuf::from(&profile.codex_home).join("config.toml")).unwrap();
        assert_eq!(
            get_path(&config, "model_provider"),
            Some(TomlValue::String("vellum".into()))
        );
        assert_eq!(get_path(&config, "openai_base_url"), None);
        let provider = get_path(&config, "model_providers")
            .and_then(|value| value.as_table().cloned())
            .and_then(|table| table.get("vellum").cloned())
            .expect("model_providers.vellum must be present");
        assert_eq!(
            provider.get("name").and_then(TomlValue::as_str),
            Some("OpenAI")
        );
        let header_env_var = provider
            .get("env_http_headers")
            .and_then(TomlValue::as_table)
            .and_then(|headers| headers.get(vellum_proxy_runtime::BOUNDARY_KEY_HEADER))
            .and_then(TomlValue::as_str);
        assert_eq!(
            header_env_var,
            Some(vellum_proxy_runtime::BOUNDARY_KEY_ENV_VAR)
        );
        let rendered = toml::to_string_pretty(&config).unwrap();
        assert!(
            !rendered.contains("http_headers") || rendered.contains("env_http_headers"),
            "the provider table must reference an env var, never a literal header value"
        );

        assert_eq!(
            get_path(&config, "features.remote_compaction_v2"),
            Some(TomlValue::Boolean(true))
        );
        assert_eq!(
            get_path(&config, "features.enable_request_compression"),
            Some(TomlValue::Boolean(false))
        );
        assert_eq!(
            get_path(&config, "features.image_generation"),
            Some(TomlValue::Boolean(false))
        );
        assert_eq!(
            get_path(&config, "features.auto_compaction"),
            Some(TomlValue::Boolean(true)),
            "without this, Codex's automatic compaction pass never runs at all"
        );

        // Restore must clean up every key this injection introduced, not just
        // the ones present before Vellum's boundary-key work.
        manager.restore("profile-1").unwrap();
        let restored = read_toml(&PathBuf::from(&profile.codex_home).join("config.toml")).unwrap();
        assert_eq!(get_path(&restored, "model_provider"), None);
        assert_eq!(get_path(&restored, "model_providers"), None);
        assert_eq!(get_path(&restored, "features.remote_compaction_v2"), None);
        assert_eq!(get_path(&restored, "features.auto_compaction"), None);
    }

    /// Bundled Codex applies `model_context_window` and
    /// `model_auto_compact_token_limit` as *global* overrides on top of
    /// every model's own catalog entry. A pre-existing user value here would
    /// silently defeat the catalog's per-model contract — most dangerously
    /// for Grok/disabled routes, whose catalog entry deliberately omits
    /// `context_window` so Codex's 90% fallback never schedules. Injection
    /// must clear all three while active and restore exactly what the user
    /// had on stop.
    #[test]
    fn injection_clears_conflicting_global_compaction_overrides_and_restore_returns_them() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProfileManager::new(AgentStateStore::new(AgentPaths::from_root(
            temp.path().join("state"),
        )));
        let profile = manager.create_managed("profile-1", None).unwrap();
        let config = PathBuf::from(&profile.codex_home).join("config.toml");
        fs::write(
            &config,
            "model_provider = \"openai\"\n\
             model_context_window = 400000\n\
             model_auto_compact_token_limit = 350000\n\
             model_auto_compact_token_limit_scope = \"body_after_prefix\"\n",
        )
        .unwrap();

        manager
            .inject("profile-1", &proxy(), "{\"models\":[]}")
            .unwrap();
        let injected = read_toml(&config).unwrap();
        assert_eq!(get_path(&injected, "model_context_window"), None);
        assert_eq!(get_path(&injected, "model_auto_compact_token_limit"), None);
        assert_eq!(
            get_path(&injected, "model_auto_compact_token_limit_scope"),
            None
        );

        manager.restore("profile-1").unwrap();
        let restored = read_toml(&config).unwrap();
        assert_eq!(
            get_path(&restored, "model_context_window"),
            Some(TomlValue::Integer(400_000))
        );
        assert_eq!(
            get_path(&restored, "model_auto_compact_token_limit"),
            Some(TomlValue::Integer(350_000))
        );
        assert_eq!(
            get_path(&restored, "model_auto_compact_token_limit_scope"),
            Some(TomlValue::String("body_after_prefix".into()))
        );
    }

    #[test]
    fn successful_native_restart_activates_a_verified_lease() {
        let temp = tempfile::tempdir().unwrap();
        let store = AgentStateStore::new(AgentPaths::from_root(temp.path().join("state")));
        let manager = ProfileManager::new(store);
        manager.create_managed("profile-1", None).unwrap();
        manager
            .inject("profile-1", &proxy(), "{\"models\":[]}")
            .unwrap();
        let lease_path = manager.lease_path("profile-1");
        let mut lease: ConfigLease = read_json(&lease_path, "lease").unwrap();
        lease.state = "restartRequired".into();
        atomic_json(&lease_path, &lease, 0o600).unwrap();
        assert_eq!(
            manager.lease_status("profile-1").unwrap()["state"],
            "restartRequired"
        );
        manager.activate_after_restart("profile-1").unwrap();
        assert_eq!(
            manager.lease_status("profile-1").unwrap()["state"],
            "active"
        );
    }

    #[test]
    fn managed_field_edit_reports_conflict_without_overwrite() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProfileManager::new(AgentStateStore::new(AgentPaths::from_root(
            temp.path().join("state"),
        )));
        let profile = manager.create_managed("profile-1", None).unwrap();
        manager.inject("profile-1", &proxy(), "{}").unwrap();
        let config = PathBuf::from(profile.codex_home).join("config.toml");
        let mut current = read_toml(&config).unwrap();
        set_path(
            &mut current,
            "openai_base_url",
            Some(TomlValue::String("http://127.0.0.1:9999/v1".into())),
        );
        fs::write(&config, toml::to_string_pretty(&current).unwrap()).unwrap();
        let result = manager.restore("profile-1").unwrap();
        assert!(result.manual_action_required);
        assert_eq!(
            get_path(&read_toml(&config).unwrap(), "openai_base_url"),
            Some(TomlValue::String("http://127.0.0.1:9999/v1".into()))
        );
    }

    #[test]
    fn injection_fails_closed_before_any_config_write_when_proxy_unhealthy() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProfileManager::new(AgentStateStore::new(AgentPaths::from_root(
            temp.path().join("state"),
        )));
        let profile = manager.create_managed("profile-1", None).unwrap();
        let err = manager
            .inject("profile-1", &ProxyStatusView::default(), "{}")
            .unwrap_err();
        assert!(err.contains("ProxyUnhealthy"));
        assert!(!PathBuf::from(profile.codex_home)
            .join("config.toml")
            .exists());
    }

    #[test]
    fn reboot_reconciliation_recovers_the_persisted_broker_port() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("broker.toml"),
            "listen_addr = \"127.0.0.1:46234\"\n",
        )
        .unwrap();
        assert_eq!(configured_broker_port(temp.path()), Some(46234));
    }

    #[test]
    fn adopt_existing_is_explicit_and_uses_three_way_restore() {
        let temp = tempfile::tempdir().unwrap();
        let existing = temp.path().join("production-codex-home");
        fs::create_dir_all(&existing).unwrap();
        fs::write(
            existing.join("config.toml"),
            "approval_policy = \"on-request\"\n",
        )
        .unwrap();
        let manager = ProfileManager::new(AgentStateStore::new(AgentPaths::from_root(
            temp.path().join("state"),
        )));
        assert!(manager
            .adopt_existing("adopted", &existing, false)
            .unwrap_err()
            .contains("ExplicitAdoptRequired"));
        manager.adopt_existing("adopted", &existing, true).unwrap();
        manager.inject("adopted", &proxy(), "{}").unwrap();
        let mut current = read_toml(&existing.join("config.toml")).unwrap();
        set_path(
            &mut current,
            "approval_policy",
            Some(TomlValue::String("never".into())),
        );
        fs::write(
            existing.join("config.toml"),
            toml::to_string_pretty(&current).unwrap(),
        )
        .unwrap();
        let result = manager.restore("adopted").unwrap();
        assert!(!result.manual_action_required);
        assert_eq!(
            get_path(
                &read_toml(&existing.join("config.toml")).unwrap(),
                "approval_policy"
            ),
            Some(TomlValue::String("never".into()))
        );
    }
}
