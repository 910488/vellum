//! Durable proxy configuration and credential-reference storage.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use vellum_proxy_runtime::{ProxyRuntimeConfig, SELECTED_OFFICIAL_CREDENTIAL_ID};

use crate::permissions::prepare_proxy_mounts;
use crate::state::AgentPaths;

const MAX_CONFIG_LOAD_ERROR_BYTES: usize = 1_024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigureResult {
    pub config_hash: String,
    pub config_path: String,
    pub credential_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CredentialStatus {
    pub credential_id: String,
    pub present: bool,
}

/// The schema version this build's runtime and daemon actually enforce
/// (`vellum_proxy_runtime::config`'s own `default_schema_version`, which is
/// private to that crate). Duplicated as a literal rather than re-derived
/// from a default-constructed config so the comparison stays a plain integer
/// check independent of `ProxyRuntimeConfig::default()`'s other fields.
const CURRENT_SCHEMA_VERSION: u32 = 2;

/// Bounded, content-free machine codes for `PublicConfigurationStatus.issue`.
/// Never the raw parse/validation error string -- that can echo back
/// arbitrary bytes from the persisted file.
mod issue_code {
    pub const MALFORMED_TOML: &str = "malformedToml";
    pub const MISSING_REQUIRED_FIELD: &str = "missingRequiredField";
    pub const VALIDATION_FAILED: &str = "validationFailed";
    pub const FUTURE_SCHEMA_VERSION: &str = "futureSchemaVersion";
    pub const IO_ERROR: &str = "ioError";
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ConfigurationState {
    /// No persisted config file at all -- a fresh host, or one never
    /// deployed to.
    Missing,
    /// Schema 2, strictly parses, and validates. The only state the daemon
    /// or `proxy.configure` will actually load.
    Current,
    /// Schema 1, or no `schema_version` key at all (schema 1 predates the
    /// field). A known-shape config a normal deployment can safely replace.
    UpgradeRequired,
    /// Tagged schema 2 but the file is corrupt, missing a required field, or
    /// fails `ProxyRuntimeConfig::validate()`. Also known-safe to replace --
    /// this host was never running on it either.
    RepairRequired,
    /// `schema_version` above what this build supports. Never auto-replaced:
    /// an older Desktop must not clobber a config a newer one understands.
    Incompatible,
    /// The file exists but couldn't be read (permissions, I/O error, not
    /// valid UTF-8). Distinct from `RepairRequired`: this may not be safe to
    /// overwrite blindly, so it blocks deployment instead of triggering one.
    Unreadable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PublicConfigurationStatus {
    pub present: bool,
    pub state: ConfigurationState,
    pub schema_version: Option<u32>,
    pub requires_reconfigure: bool,
    pub issue: Option<String>,
    pub config_hash: Option<String>,
    pub credential_refs: Vec<String>,
    pub credentials_ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_error: Option<String>,
}

/// The result of leniently inspecting a persisted config's *shape*, without
/// ever treating an old or broken one as loadable. Only `Current` carries a
/// fully strict-parsed, `validate()`-passed config -- every other state
/// exists so the caller can classify *why* it isn't one, never so a
/// half-trusted value can leak into credential/route resolution.
struct ConfigInspection {
    state: ConfigurationState,
    schema_version: Option<u32>,
    issue: Option<&'static str>,
    /// The underlying failure in the words the parser used.
    ///
    /// `issue` classifies; this says which field or version was actually
    /// wrong. A caller looking at `RepairRequired` still has to be told
    /// *what* to repair, and "inbound_access is missing" is the sentence that
    /// does that -- the code alone sends someone back to the file to guess.
    detail: Option<String>,
    valid_config: Option<ProxyRuntimeConfig>,
}

/// Read-only classification of a persisted config's on-disk shape. This is
/// deliberately independent of `ProxyRuntimeConfig::from_toml_str`, which
/// requires every schema-2 field (`inbound_access` above all) to be present
/// and errors out otherwise -- exactly the behavior that must stay strict for
/// the daemon and `proxy.configure`, and exactly what made `host.status` fail
/// closed (as an RPC error, not an observable state) against a schema-1
/// config with no `inbound_access` table at all.
fn inspect_persisted_config(raw: &str) -> ConfigInspection {
    let generic: toml::Value = match toml::from_str(raw) {
        Ok(value) => value,
        Err(error) => {
            return ConfigInspection {
                state: ConfigurationState::RepairRequired,
                schema_version: None,
                issue: Some(issue_code::MALFORMED_TOML),
                detail: Some(format!("invalid persisted proxy config: {error}")),
                valid_config: None,
            };
        }
    };
    let schema_version_value = generic.get("schema_version");
    let schema_version = match schema_version_value {
        None => None,
        Some(value) => match value.as_integer() {
            Some(version) if version >= 0 => Some(version as u32),
            _ => {
                return ConfigInspection {
                    state: ConfigurationState::RepairRequired,
                    schema_version: None,
                    issue: Some(issue_code::MALFORMED_TOML),
                    detail: Some("schema_version is not a non-negative integer".to_string()),
                    valid_config: None,
                };
            }
        },
    };
    // Schema 1 predates the `schema_version` field entirely -- an absent key
    // is schema 1's own shape, not ambiguity.
    let effective_version = schema_version.unwrap_or(1);
    if effective_version < CURRENT_SCHEMA_VERSION {
        return ConfigInspection {
            state: ConfigurationState::UpgradeRequired,
            schema_version,
            issue: None,
            detail: Some(format!(
                "persisted proxy config is schema_version {effective_version}; this build requires {CURRENT_SCHEMA_VERSION}"
            )),
            valid_config: None,
        };
    }
    if effective_version > CURRENT_SCHEMA_VERSION {
        return ConfigInspection {
            state: ConfigurationState::Incompatible,
            schema_version,
            issue: Some(issue_code::FUTURE_SCHEMA_VERSION),
            detail: Some(format!(
                "persisted proxy config is schema_version {effective_version}; this build supports {CURRENT_SCHEMA_VERSION}"
            )),
            valid_config: None,
        };
    }
    match ProxyRuntimeConfig::from_toml_str(raw) {
        Ok(config) => match config.validate() {
            Ok(()) => ConfigInspection {
                state: ConfigurationState::Current,
                schema_version: Some(effective_version),
                issue: None,
                detail: None,
                valid_config: Some(config),
            },
            Err(error) => ConfigInspection {
                state: ConfigurationState::RepairRequired,
                schema_version: Some(effective_version),
                issue: Some(issue_code::VALIDATION_FAILED),
                detail: Some(format!("invalid persisted proxy config: {error}")),
                valid_config: None,
            },
        },
        Err(error) => ConfigInspection {
            state: ConfigurationState::RepairRequired,
            schema_version: Some(effective_version),
            issue: Some(issue_code::MISSING_REQUIRED_FIELD),
            detail: Some(format!("invalid persisted proxy config: {error}")),
            valid_config: None,
        },
    }
}

/// Never fails on an old, corrupt, or otherwise unloadable persisted
/// config -- only a state a caller (Remote Manager plan/apply, `host.status`)
/// can react to. The daemon and `proxy.configure` remain the only paths that
/// actually load and enforce a config; this function only ever *observes*.
pub fn public_status(paths: &AgentPaths) -> Result<PublicConfigurationStatus, String> {
    let path = paths.proxy_config_dir.join("proxy.toml");
    if !path.exists() {
        return Ok(PublicConfigurationStatus {
            present: false,
            state: ConfigurationState::Missing,
            schema_version: None,
            requires_reconfigure: true,
            issue: None,
            config_hash: None,
            credential_refs: Vec::new(),
            credentials_ready: false,
            load_error: None,
        });
    }
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) => {
            return Ok(PublicConfigurationStatus {
                present: true,
                state: ConfigurationState::Unreadable,
                schema_version: None,
                requires_reconfigure: true,
                issue: Some(issue_code::IO_ERROR.to_string()),
                config_hash: None,
                credential_refs: Vec::new(),
                credentials_ready: false,
                load_error: Some(bounded_config_load_error(format!(
                    "cannot read persisted proxy config: {error}"
                ))),
            });
        }
    };
    let inspection = inspect_persisted_config(&raw);
    let (config_hash, credential_refs, credentials_ready) = match &inspection.valid_config {
        Some(config) => {
            let mut refs = config
                .models
                .iter()
                .filter_map(|route| route.credential_id.clone())
                .collect::<Vec<_>>();
            refs.sort();
            refs.dedup();
            let providers_ready = refs.iter().all(|credential_id| {
                if credential_id == SELECTED_OFFICIAL_CREDENTIAL_ID {
                    // This is an Official-account selector, not a secret file.
                    // With no selected execution account, the proxy deliberately
                    // preserves the Codex control account's request authorization.
                    crate::official_account::selected_credential_ready(paths)
                } else {
                    credential_path(paths, credential_id).is_file()
                }
            });
            // Provider credentials are necessary but not sufficient: a
            // config with every provider key present but no boundary key
            // provisioned would otherwise read as "ready" while the proxy
            // still can't start with inbound authentication enforced.
            let boundary_ready =
                credential_path(paths, &config.inbound_access.credential_id).is_file();
            (
                Some(config.identity.config_hash.clone()),
                refs,
                providers_ready && boundary_ready,
            )
        }
        None => (None, Vec::new(), false),
    };
    Ok(PublicConfigurationStatus {
        present: true,
        requires_reconfigure: inspection.state != ConfigurationState::Current,
        state: inspection.state,
        schema_version: inspection.schema_version,
        issue: inspection.issue.map(str::to_string),
        load_error: inspection.detail.map(bounded_config_load_error),
        config_hash,
        credential_refs,
        credentials_ready,
    })
}

pub fn configure(paths: &AgentPaths, raw: &str) -> Result<ConfigureResult, String> {
    prepare_proxy_mounts(paths)?;
    let mut config = ProxyRuntimeConfig::from_toml_str(raw)
        .map_err(|error| format!("InvalidProxyConfig: {error}"))?;
    config.validate()?;

    let mut refs = config
        .models
        .iter()
        .filter_map(|route| route.credential_id.clone())
        .collect::<Vec<_>>();
    refs.sort();
    refs.dedup();
    for credential_id in &refs {
        validate_id(credential_id)?;
    }

    // Hash a stable serialization with the hash field blanked, then persist
    // that exact resolved identity. Secrets are referenced, never embedded.
    config.identity.config_hash.clear();
    let canonical = toml::to_string_pretty(&config)
        .map_err(|error| format!("failed encoding proxy config: {error}"))?;
    let config_hash = hex::encode(Sha256::digest(canonical.as_bytes()));
    config.identity.config_hash = config_hash.clone();
    let encoded = toml::to_string_pretty(&config)
        .map_err(|error| format!("failed encoding proxy config: {error}"))?;
    let path = paths.proxy_config_dir.join("proxy.toml");
    atomic_write(&path, encoded.as_bytes(), 0o600)?;

    Ok(ConfigureResult {
        config_hash,
        config_path: path.to_string_lossy().to_string(),
        credential_refs: refs,
    })
}

pub fn put_credential(
    paths: &AgentPaths,
    credential_id: &str,
    secret: &str,
) -> Result<CredentialStatus, String> {
    validate_id(credential_id)?;
    prepare_proxy_mounts(paths)?;
    if secret.is_empty() {
        return Err("credential secret must not be empty".into());
    }
    atomic_write(
        &credential_path(paths, credential_id),
        secret.as_bytes(),
        0o600,
    )?;
    Ok(CredentialStatus {
        credential_id: credential_id.into(),
        present: true,
    })
}

pub fn remove_credential(
    paths: &AgentPaths,
    credential_id: &str,
) -> Result<CredentialStatus, String> {
    validate_id(credential_id)?;
    match fs::remove_file(credential_path(paths, credential_id)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("failed removing credential: {error}")),
    }
    Ok(CredentialStatus {
        credential_id: credential_id.into(),
        present: false,
    })
}

pub fn credential_status(
    paths: &AgentPaths,
    credential_id: &str,
) -> Result<CredentialStatus, String> {
    validate_id(credential_id)?;
    Ok(CredentialStatus {
        credential_id: credential_id.into(),
        present: credential_path(paths, credential_id).is_file(),
    })
}

/// Read this host's proxy boundary key from the agent's own secret store.
///
/// The agent is what provisions the key and what mounts `/run/secrets` into the
/// proxy container, so it is the one caller that can legitimately present it to
/// the guarded endpoints when probing readiness.
pub fn read_boundary_key(paths: &AgentPaths) -> Result<String, String> {
    let path = credential_path(paths, vellum_proxy_runtime::BOUNDARY_CREDENTIAL_ID);
    let raw = std::fs::read_to_string(&path).map_err(|error| {
        format!(
            "proxy boundary key is not provisioned at {}: {error}",
            path.display()
        )
    })?;
    Ok(raw.trim_end_matches(['\r', '\n']).to_string())
}

/// Best-effort variant for Codex daemon-lifecycle call sites that must not
/// fail the whole operation over a boundary key that legitimately has not
/// been provisioned yet (e.g. before the proxy has ever been configured on
/// this host). This crate has no logging dependency and its stdout is the
/// agent's own JSON-RPC-shaped wire protocol (see `main.rs`), so silently
/// discarding the read error would leave no trace at all of a Codex daemon
/// that started without the credential it needs for every subsequent
/// request — this prints to stderr instead, which is safe from the wire
/// protocol and picked up by whatever supervises the agent process (journal,
/// systemd, etc.).
pub fn read_boundary_key_or_warn(paths: &AgentPaths, context: &str) -> Option<String> {
    match read_boundary_key(paths) {
        Ok(key) => Some(key),
        Err(error) => {
            eprintln!("vellum-remote-agent: {context}: {error}");
            None
        }
    }
}

fn credential_path(paths: &AgentPaths, credential_id: &str) -> std::path::PathBuf {
    paths.secrets_dir.join(credential_id)
}

fn bounded_config_load_error(message: String) -> String {
    if message.len() <= MAX_CONFIG_LOAD_ERROR_BYTES {
        return message;
    }
    let mut end = MAX_CONFIG_LOAD_ERROR_BYTES - '…'.len_utf8();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &message[..end])
}

fn validate_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err("invalid credential id".into());
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8], _mode: u32) -> Result<(), String> {
    atomic_write_with(path, _mode, |file| file.write_all(bytes))
}

fn atomic_write_with(
    path: &Path,
    _mode: u32,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "path has no parent".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed creating {}: {error}", parent.display()))?;
    let tmp = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(_mode);
    }
    let mut file = options
        .open(&tmp)
        .map_err(|error| format!("failed creating {}: {error}", tmp.display()))?;
    if let Err(error) = write(&mut file) {
        drop(file);
        let _ = fs::remove_file(&tmp);
        return Err(format!("failed writing {}: {error}", tmp.display()));
    }
    file.sync_all()
        .map_err(|error| format!("failed syncing {}: {error}", tmp.display()))?;
    drop(file);
    fs::rename(&tmp, path)
        .map_err(|error| format!("failed replacing {}: {error}", path.display()))?;
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|error| format!("failed syncing {}: {error}", parent.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> String {
        r#"schema_version = 2
listen = "0.0.0.0:15721"
data_dir = "/var/lib/vellum/data"
history_dir = "/var/lib/vellum/history"
log_dir = "/var/log/vellum"
credentials_dir = "/run/secrets"
require_secrets = true
strict_upstream = true

[inbound_access]
credentialId = "__vellum_proxy_boundary__"

[identity]
install_id = "install-1"
host_id = "host-1"
image_version = "0.1.0"
config_hash = "pending"

[[models]]
route_id = "route-1"
catalog_id = "model-1"
upstream_model = "upstream-1"
credential_id = "provider-main"

[execution_environment]
platform = "linux"
shell = "bash"
supportsAndAnd = true
hasUnixUtilities = true
pathStyle = "posix"
ampersandSemantics = "posix-background"
"#
        .into()
    }

    /// A real schema-1 config: no `schema_version` key, no `inbound_access`
    /// table -- what a host deployed before the boundary guard actually has
    /// on disk.
    fn schema1_config() -> String {
        r#"listen = "0.0.0.0:15721"
data_dir = "/var/lib/vellum/data"
history_dir = "/var/lib/vellum/history"
log_dir = "/var/log/vellum"
credentials_dir = "/run/secrets"
require_secrets = true
strict_upstream = true

[identity]
install_id = "install-1"
host_id = "host-1"
image_version = "0.1.0"
config_hash = "pending"

[[models]]
route_id = "route-1"
catalog_id = "model-1"
upstream_model = "upstream-1"
credential_id = "provider-main"

[execution_environment]
platform = "linux"
shell = "bash"
supportsAndAnd = true
hasUnixUtilities = true
pathStyle = "posix"
ampersandSemantics = "posix-background"
"#
        .into()
    }

    fn write_persisted_config(paths: &AgentPaths, raw: &str) {
        fs::create_dir_all(&paths.proxy_config_dir).unwrap();
        fs::write(paths.proxy_config_dir.join("proxy.toml"), raw).unwrap();
    }

    #[test]
    fn public_status_reports_missing_when_no_file_exists() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        let status = public_status(&paths).unwrap();
        assert_eq!(status.state, ConfigurationState::Missing);
        assert!(!status.present);
        assert!(status.requires_reconfigure);
        assert_eq!(status.schema_version, None);
        assert_eq!(status.issue, None);
    }

    #[test]
    fn public_status_observes_a_schema1_config_instead_of_erroring() {
        // The bug this whole module exists to fix: `host.status` used to
        // hard-fail (`Result::Err`, not an observable state) against a
        // schema-1 config with no `inbound_access` table, because the old
        // `public_status` called the same strict parser the daemon uses.
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        write_persisted_config(&paths, &schema1_config());

        let status = public_status(&paths).unwrap();
        assert_eq!(status.state, ConfigurationState::UpgradeRequired);
        assert!(status.present);
        assert!(status.requires_reconfigure);
        assert_eq!(status.schema_version, None);
        assert_eq!(status.issue, None);
        // An old config can never be trusted for credential resolution.
        assert!(status.credential_refs.is_empty());
        assert!(!status.credentials_ready);

        // The strict path the daemon and `proxy.configure` actually use must
        // still refuse this exact file.
        assert!(ProxyRuntimeConfig::from_toml_str(&schema1_config()).is_err());
        assert!(configure(&paths, &schema1_config()).is_err());
    }

    #[test]
    fn public_status_treats_an_explicit_schema_1_tag_the_same_as_unversioned() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        let tagged = format!("schema_version = 1\n{}", schema1_config());
        write_persisted_config(&paths, &tagged);

        let status = public_status(&paths).unwrap();
        assert_eq!(status.state, ConfigurationState::UpgradeRequired);
        assert_eq!(status.schema_version, Some(1));
    }

    #[test]
    fn public_status_reports_current_for_a_valid_schema2_config() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        write_persisted_config(&paths, &config());
        put_credential(&paths, "provider-main", "very-secret").unwrap();
        put_credential(&paths, "__vellum_proxy_boundary__", "boundary-secret").unwrap();

        let status = public_status(&paths).unwrap();
        assert_eq!(status.state, ConfigurationState::Current);
        assert!(!status.requires_reconfigure);
        assert_eq!(status.schema_version, Some(2));
        assert_eq!(status.issue, None);
        assert_eq!(status.credential_refs, vec!["provider-main".to_string()]);
        assert!(status.credentials_ready);
    }

    #[test]
    fn official_selector_without_an_execution_override_is_ready() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        let config = config().replace(
            "credential_id = \"provider-main\"",
            "credential_id = \"official-selected\"",
        );
        write_persisted_config(&paths, &config);
        put_credential(&paths, "__vellum_proxy_boundary__", "boundary-secret").unwrap();

        let status = public_status(&paths).unwrap();
        assert_eq!(
            status.credential_refs,
            vec!["official-selected".to_string()]
        );
        assert!(status.credentials_ready);
    }

    #[test]
    fn public_status_is_not_ready_when_the_boundary_key_is_missing() {
        // The bugfix: every provider credential can be present while the
        // proxy still cannot start with inbound authentication enforced,
        // because credentialsReady never looked at inbound_access at all.
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        write_persisted_config(&paths, &config());
        put_credential(&paths, "provider-main", "very-secret").unwrap();
        // No `__vellum_proxy_boundary__` credential provisioned.

        let status = public_status(&paths).unwrap();
        assert_eq!(status.state, ConfigurationState::Current);
        assert!(!status.credentials_ready);
    }

    #[test]
    fn public_status_reports_repair_required_for_a_schema2_config_missing_a_field() {
        let broken = config().replace(
            "[inbound_access]\ncredentialId = \"__vellum_proxy_boundary__\"\n\n",
            "",
        );
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        write_persisted_config(&paths, &broken);

        let status = public_status(&paths).unwrap();
        assert_eq!(status.state, ConfigurationState::RepairRequired);
        assert!(status.requires_reconfigure);
        assert_eq!(status.schema_version, Some(2));
        assert_eq!(status.issue.as_deref(), Some("missingRequiredField"));
        assert!(
            status.issue.unwrap().len() < 64,
            "issue must be a short code"
        );
    }

    #[test]
    fn public_status_reports_repair_required_for_a_schema2_config_failing_validation() {
        let broken = config().replace("install_id = \"install-1\"", "install_id = \"\"");
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        write_persisted_config(&paths, &broken);

        let status = public_status(&paths).unwrap();
        assert_eq!(status.state, ConfigurationState::RepairRequired);
        assert_eq!(status.issue.as_deref(), Some("validationFailed"));
    }

    #[test]
    fn public_status_reports_repair_required_for_corrupted_toml() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        write_persisted_config(&paths, "this is not [ valid toml at all");

        let status = public_status(&paths).unwrap();
        assert_eq!(status.state, ConfigurationState::RepairRequired);
        assert_eq!(status.schema_version, None);
        assert_eq!(status.issue.as_deref(), Some("malformedToml"));
        // Never echoes the raw file content back as the issue code.
        assert!(!status.issue.unwrap().contains("this is not"));
    }

    #[test]
    fn public_status_reports_incompatible_for_a_future_schema_version() {
        let future = format!("schema_version = 99\n{}", schema1_config());
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        write_persisted_config(&paths, &future);

        let status = public_status(&paths).unwrap();
        assert_eq!(status.state, ConfigurationState::Incompatible);
        assert!(status.requires_reconfigure);
        assert_eq!(status.schema_version, Some(99));
        assert_eq!(status.issue.as_deref(), Some("futureSchemaVersion"));
    }

    #[test]
    fn public_status_never_leaks_the_config_body_into_the_issue_code() {
        for raw in [
            "this is not [ valid toml at all with a secret sk-abc123",
            "schema_version = \"not-an-integer\"",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let paths = AgentPaths::from_root(temp.path().to_path_buf());
            write_persisted_config(&paths, raw);
            let status = public_status(&paths).unwrap();
            let issue = status.issue.unwrap();
            assert!(issue.len() < 64);
            assert!(!issue.contains("sk-abc123"));
            assert!(issue.chars().all(|c| c.is_ascii_alphanumeric()));
        }
    }

    #[test]
    fn configuration_is_stable_and_contains_only_credential_refs() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        put_credential(&paths, "provider-main", "very-secret").unwrap();
        let first = configure(&paths, &config()).unwrap();
        let second = configure(&paths, &config()).unwrap();
        assert_eq!(first.config_hash, second.config_hash);
        let persisted = fs::read_to_string(first.config_path).unwrap();
        assert!(persisted.contains("provider-main"));
        assert!(!persisted.contains("very-secret"));
        assert!(credential_status(&paths, "provider-main").unwrap().present);
    }

    #[test]
    fn credential_delete_is_idempotent_and_id_is_confined() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        assert!(put_credential(&paths, "../escape", "secret").is_err());
        remove_credential(&paths, "safe").unwrap();
        remove_credential(&paths, "safe").unwrap();
    }

    #[test]
    fn failed_atomic_write_preserves_previous_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("proxy.toml");
        fs::write(&path, "previous").unwrap();
        let error = atomic_write_with(&path, 0o600, |file| {
            file.write_all(b"partial")?;
            Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "synthetic disk full",
            ))
        })
        .unwrap_err();
        assert!(error.contains("synthetic disk full"));
        assert_eq!(fs::read_to_string(path).unwrap(), "previous");
    }

    #[test]
    fn status_reports_legacy_config_without_blocking_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        prepare_proxy_mounts(&paths).unwrap();
        let legacy = config()
            .replace("schema_version = 2", "schema_version = 1")
            .replace(
                "[inbound_access]\ncredentialId = \"__vellum_proxy_boundary__\"\n\n",
                "",
            );
        fs::write(paths.proxy_config_dir.join("proxy.toml"), legacy).unwrap();

        let status = public_status(&paths).expect("status must remain available for recovery");
        assert!(status.present);
        assert!(status.requires_reconfigure);
        assert_eq!(status.config_hash, None);
        assert!(status.credential_refs.is_empty());
        assert!(!status.credentials_ready);
        // The diagnostic names schema 1, not the missing `inbound_access`
        // table. That is the better answer, and deliberately so: a schema-1
        // config has no boundary key *because* it predates the field, so
        // reporting the absent table reports a symptom and sends the reader to
        // patch a file that needs replacing. The classification is the cause.
        assert_eq!(status.state, ConfigurationState::UpgradeRequired);
        assert!(
            status
                .load_error
                .as_deref()
                .is_some_and(|message| message.contains("schema_version 1")),
            "the diagnostic must still say what is wrong, not only that something is"
        );
    }

    #[test]
    fn status_reports_unsupported_schema_without_treating_it_as_valid() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AgentPaths::from_root(temp.path().to_path_buf());
        prepare_proxy_mounts(&paths).unwrap();
        fs::write(
            paths.proxy_config_dir.join("proxy.toml"),
            config().replace("schema_version = 2", "schema_version = 1"),
        )
        .unwrap();

        let status = public_status(&paths).expect("status must remain available for recovery");
        assert!(status.requires_reconfigure);
        assert!(status
            .load_error
            .as_deref()
            .is_some_and(|message| message.contains("schema_version 1")));
    }

    #[test]
    fn invalid_config_status_diagnostic_is_utf8_safe_and_bounded() {
        let message = format!("invalid persisted proxy config: {}", "錯".repeat(2_000));
        let bounded = bounded_config_load_error(message);
        assert!(bounded.len() <= MAX_CONFIG_LOAD_ERROR_BYTES);
        assert!(bounded.ends_with('…'));
    }
}
