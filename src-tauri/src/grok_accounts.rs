use crate::error::{AppError, AppResult};
use crate::grok_auth::{self, GrokCredential};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tokio::process::Child;
use tokio::sync::Mutex;

const STORE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum GrokAccountSource {
    External,
    Managed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GrokAccount {
    pub account_id: String,
    pub email: Option<String>,
    pub authenticated_at: i64,
    pub is_default: bool,
    pub source: GrokAccountSource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GrokAccountStatus {
    pub authenticated: bool,
    pub default_account_id: Option<String>,
    pub accounts: Vec<GrokAccount>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GrokLoginStatus {
    pub login_id: String,
    pub state: GrokLoginState,
    pub account: Option<GrokAccount>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GrokLoginState {
    Waiting,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AccountMetadata {
    account_id: String,
    email: Option<String>,
    authenticated_at: i64,
    source: GrokAccountSource,
    profile_key: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AccountStore {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    accounts: HashMap<String, AccountMetadata>,
    #[serde(default)]
    default_account_id: Option<String>,
    #[serde(default)]
    ignored_external_account_id: Option<String>,
}

struct PendingLogin {
    child: Child,
    staging_home: PathBuf,
}

pub struct GrokAccountManager {
    root: PathBuf,
    external_home: PathBuf,
    store: RwLock<AccountStore>,
    pending: Mutex<HashMap<String, PendingLogin>>,
    refresh_locks: RwLock<HashMap<String, Arc<Mutex<()>>>>,
}

impl GrokAccountManager {
    pub fn new(data_root: PathBuf) -> Self {
        let root = data_root.join("grok_accounts");
        let external_home = grok_auth::grok_home();
        let mut store = load_store(&root).unwrap_or_default();
        store.version = STORE_VERSION;
        discover_external(&external_home, &mut store);
        normalize_default(&mut store);
        let manager = Self {
            root,
            external_home,
            store: RwLock::new(store),
            pending: Mutex::new(HashMap::new()),
            refresh_locks: RwLock::new(HashMap::new()),
        };
        let _ = manager.persist();
        manager
    }

    pub fn status(&self) -> GrokAccountStatus {
        let store = self.store.read().expect("grok account store poisoned");
        let mut accounts = store
            .accounts
            .values()
            .map(|account| GrokAccount {
                account_id: account.account_id.clone(),
                email: account.email.clone(),
                authenticated_at: account.authenticated_at,
                is_default: store.default_account_id.as_deref()
                    == Some(account.account_id.as_str()),
                source: account.source,
            })
            .collect::<Vec<_>>();
        accounts.sort_by(|left, right| {
            right
                .is_default
                .cmp(&left.is_default)
                .then_with(|| right.authenticated_at.cmp(&left.authenticated_at))
        });
        GrokAccountStatus {
            authenticated: !accounts.is_empty(),
            default_account_id: store.default_account_id.clone(),
            accounts,
        }
    }

    pub fn peek_default_account_id(&self) -> Option<String> {
        self.store
            .read()
            .ok()
            .and_then(|store| store.default_account_id.clone())
    }

    pub fn find_account_id(&self, selector: &str) -> Option<String> {
        let store = self.store.read().ok()?;
        let trimmed = selector.trim();
        if store.accounts.contains_key(trimmed) {
            return Some(trimmed.to_string());
        }
        store.accounts.values().find_map(|account| {
            if account
                .email
                .as_deref()
                .is_some_and(|email| email.eq_ignore_ascii_case(trimmed))
            {
                Some(account.account_id.clone())
            } else {
                None
            }
        })
    }

    pub fn account_home(&self, account_id: &str) -> AppResult<PathBuf> {
        let store = self.store.read().expect("grok account store poisoned");
        let account = store
            .accounts
            .get(account_id)
            .ok_or_else(|| AppError::Message(format!("Grok account not found: {account_id}")))?;
        Ok(match account.source {
            GrokAccountSource::External => self.external_home.clone(),
            GrokAccountSource::Managed => {
                self.root
                    .join("profiles")
                    .join(account.profile_key.as_deref().ok_or_else(|| {
                        AppError::Message("managed Grok account has no profile key".into())
                    })?)
            }
        })
    }

    pub fn default_account_home(&self) -> AppResult<(String, PathBuf)> {
        let account_id = self
            .peek_default_account_id()
            .ok_or_else(|| AppError::Message("Grok is not signed in".into()))?;
        let home = self.account_home(&account_id)?;
        Ok((account_id, home))
    }

    pub async fn resolve_default(&self) -> AppResult<(String, GrokCredential)> {
        let (account_id, home) = self.default_account_home()?;
        let credential = self.resolve_for_home(&account_id, &home).await?;
        Ok((account_id, credential))
    }

    pub async fn refresh_account(&self, account_id: &str) -> AppResult<()> {
        let home = self.account_home(account_id)?;
        let lock = self.refresh_lock(account_id);
        let _guard = lock.lock().await;
        grok_auth::refresh_home(&home).await?;
        if let Some((credential, _)) = grok_auth::read_credential(&home)? {
            {
                let mut store = self.store.write().expect("grok account store poisoned");
                if let Some(account) = store.accounts.get_mut(account_id) {
                    account.email = credential.email.or_else(|| account.email.clone());
                    account.authenticated_at = chrono::Utc::now().timestamp();
                }
            }
            self.persist()?;
        }
        Ok(())
    }

    async fn resolve_for_home(&self, account_id: &str, home: &Path) -> AppResult<GrokCredential> {
        match grok_auth::read_credential(home)? {
            Some((credential, expires_at))
                if expires_at.is_none_or(|expires| {
                    chrono::Utc::now() + chrono::Duration::seconds(60) < expires
                }) =>
            {
                Ok(credential)
            }
            _ => {
                let lock = self.refresh_lock(account_id);
                let _guard = lock.lock().await;
                grok_auth::refresh_home(home).await?;
                grok_auth::read_credential(home)?
                    .map(|(credential, _)| credential)
                    .ok_or_else(|| {
                        AppError::Message("Grok login did not provide a credential".into())
                    })
            }
        }
    }

    fn refresh_lock(&self, account_id: &str) -> Arc<Mutex<()>> {
        if let Some(lock) = self
            .refresh_locks
            .read()
            .expect("grok refresh locks poisoned")
            .get(account_id)
        {
            return Arc::clone(lock);
        }
        let mut locks = self
            .refresh_locks
            .write()
            .expect("grok refresh locks poisoned");
        Arc::clone(
            locks
                .entry(account_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    pub async fn start_login(&self) -> AppResult<GrokLoginStatus> {
        if !grok_auth::is_cli_installed() {
            return Err(AppError::Message("Grok CLI is not installed".into()));
        }
        let login_id = random_id("grok-login")?;
        let staging_home = self.root.join("staging").join(&login_id);
        std::fs::create_dir_all(&staging_home).map_err(|error| {
            AppError::Message(format!("create isolated Grok login profile: {error}"))
        })?;
        secure_profile_directory(&staging_home)?;
        let mut command = crate::process::background_tokio_command(grok_auth::grok_executable(
            &self.external_home,
        ));
        command
            .args(["login", "--oauth"])
            .env("GROK_HOME", &staging_home)
            .env("GROK_DISABLE_AUTOUPDATER", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let child = command.spawn().map_err(|error| {
            let _ = std::fs::remove_dir_all(&staging_home);
            AppError::Message(format!("start Grok browser login: {error}"))
        })?;
        self.pending.lock().await.insert(
            login_id.clone(),
            PendingLogin {
                child,
                staging_home,
            },
        );
        Ok(GrokLoginStatus {
            login_id,
            state: GrokLoginState::Waiting,
            account: None,
            error: None,
        })
    }

    pub async fn poll_login(&self, login_id: &str) -> AppResult<GrokLoginStatus> {
        let mut pending = self.pending.lock().await;
        let Some(login) = pending.get_mut(login_id) else {
            return Err(AppError::Message("Grok login request was not found".into()));
        };
        let Some(status) = login
            .child
            .try_wait()
            .map_err(|error| AppError::Message(format!("poll Grok login process: {error}")))?
        else {
            return Ok(GrokLoginStatus {
                login_id: login_id.to_string(),
                state: GrokLoginState::Waiting,
                account: None,
                error: None,
            });
        };
        let completed = pending
            .remove(login_id)
            .expect("pending Grok login disappeared");
        if !status.success() {
            let _ = std::fs::remove_dir_all(&completed.staging_home);
            return Ok(GrokLoginStatus {
                login_id: login_id.to_string(),
                state: GrokLoginState::Failed,
                account: None,
                error: Some(format!("Grok login exited with {status}")),
            });
        }
        let account = self.adopt_login(&completed.staging_home)?;
        Ok(GrokLoginStatus {
            login_id: login_id.to_string(),
            state: GrokLoginState::Complete,
            account: Some(account),
            error: None,
        })
    }

    pub async fn cancel_login(&self, login_id: &str) -> AppResult<GrokLoginStatus> {
        let mut pending = self.pending.lock().await;
        let Some(mut login) = pending.remove(login_id) else {
            return Err(AppError::Message("Grok login request was not found".into()));
        };
        let _ = login.child.kill().await;
        let _ = login.child.wait().await;
        let _ = std::fs::remove_dir_all(&login.staging_home);
        Ok(GrokLoginStatus {
            login_id: login_id.to_string(),
            state: GrokLoginState::Cancelled,
            account: None,
            error: None,
        })
    }

    pub fn set_default(&self, account_id: &str) -> AppResult<GrokAccountStatus> {
        {
            let mut store = self.store.write().expect("grok account store poisoned");
            if !store.accounts.contains_key(account_id) {
                return Err(AppError::Message(format!(
                    "Grok account not found: {account_id}"
                )));
            }
            store.default_account_id = Some(account_id.to_string());
        }
        self.persist()?;
        Ok(self.status())
    }

    pub fn remove_account(&self, account_id: &str) -> AppResult<GrokAccountStatus> {
        let removed = {
            let mut store = self.store.write().expect("grok account store poisoned");
            let removed = store.accounts.remove(account_id).ok_or_else(|| {
                AppError::Message(format!("Grok account not found: {account_id}"))
            })?;
            if removed.source == GrokAccountSource::External {
                store.ignored_external_account_id = Some(account_id.to_string());
            }
            if store.default_account_id.as_deref() == Some(account_id) {
                store.default_account_id = None;
                normalize_default(&mut store);
            }
            removed
        };
        if removed.source == GrokAccountSource::Managed {
            if let Some(profile_key) = removed.profile_key {
                let profile = self.root.join("profiles").join(profile_key);
                ensure_child_path(&self.root, &profile)?;
                if profile.exists() {
                    std::fs::remove_dir_all(&profile).map_err(|error| {
                        AppError::Message(format!("remove Grok account profile: {error}"))
                    })?;
                }
            }
        }
        self.refresh_locks
            .write()
            .expect("grok refresh locks poisoned")
            .remove(account_id);
        self.persist()?;
        Ok(self.status())
    }

    fn adopt_login(&self, staging_home: &Path) -> AppResult<GrokAccount> {
        let (credential, _) = grok_auth::read_credential(staging_home)?
            .ok_or_else(|| AppError::Message("Grok login completed without auth.json".into()))?;
        let identity = credential
            .user_id
            .as_deref()
            .or(credential.email.as_deref())
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| AppError::Message("Grok login did not identify the account".into()))?;
        let account_id = stable_account_id(identity);
        let profile_key = profile_key(&account_id);
        let profile = self.root.join("profiles").join(&profile_key);
        std::fs::create_dir_all(self.root.join("profiles"))
            .map_err(|error| AppError::Message(format!("create Grok profiles: {error}")))?;
        if profile.exists() {
            let source = staging_home.join("auth.json");
            let target = profile.join("auth.json");
            std::fs::create_dir_all(&profile)
                .map_err(|error| AppError::Message(format!("open Grok profile: {error}")))?;
            std::fs::copy(source, target)
                .map_err(|error| AppError::Message(format!("update Grok account: {error}")))?;
            let _ = std::fs::remove_dir_all(staging_home);
        } else {
            std::fs::rename(staging_home, &profile)
                .map_err(|error| AppError::Message(format!("adopt Grok profile: {error}")))?;
        }
        secure_profile_directory(&profile)?;
        let authenticated_at = chrono::Utc::now().timestamp();
        {
            let mut store = self.store.write().expect("grok account store poisoned");
            store.accounts.insert(
                account_id.clone(),
                AccountMetadata {
                    account_id: account_id.clone(),
                    email: credential.email.clone(),
                    authenticated_at,
                    source: GrokAccountSource::Managed,
                    profile_key: Some(profile_key),
                },
            );
            if store.default_account_id.is_none() {
                store.default_account_id = Some(account_id.clone());
            }
        }
        self.persist()?;
        Ok(self
            .status()
            .accounts
            .into_iter()
            .find(|account| account.account_id == account_id)
            .expect("adopted Grok account missing"))
    }

    fn persist(&self) -> AppResult<()> {
        std::fs::create_dir_all(&self.root)
            .map_err(|error| AppError::Message(format!("create Grok account store: {error}")))?;
        let store = self
            .store
            .read()
            .expect("grok account store poisoned")
            .clone();
        let bytes = serde_json::to_vec_pretty(&store)
            .map_err(|error| AppError::Message(format!("encode Grok accounts: {error}")))?;
        let path = self.root.join("accounts.json");
        let temporary = self.root.join("accounts.json.tmp");
        std::fs::write(&temporary, bytes)
            .map_err(|error| AppError::Message(format!("write Grok accounts: {error}")))?;
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|error| AppError::Message(format!("replace Grok accounts: {error}")))?;
        }
        std::fs::rename(temporary, path)
            .map_err(|error| AppError::Message(format!("commit Grok accounts: {error}")))
    }
}

fn discover_external(home: &Path, store: &mut AccountStore) {
    let Some((credential, _)) = grok_auth::read_credential(home).ok().flatten() else {
        return;
    };
    let Some(identity) = credential
        .user_id
        .as_deref()
        .or(credential.email.as_deref())
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    let account_id = stable_account_id(identity);
    if store.ignored_external_account_id.as_deref() == Some(account_id.as_str()) {
        return;
    }
    let authenticated_at = store
        .accounts
        .get(&account_id)
        .filter(|account| account.source == GrokAccountSource::External)
        .map(|account| account.authenticated_at)
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    store.accounts.insert(
        account_id.clone(),
        AccountMetadata {
            account_id,
            email: credential.email,
            authenticated_at,
            source: GrokAccountSource::External,
            profile_key: None,
        },
    );
}

fn normalize_default(store: &mut AccountStore) {
    if store
        .default_account_id
        .as_ref()
        .is_some_and(|id| store.accounts.contains_key(id))
    {
        return;
    }
    store.default_account_id = store
        .accounts
        .values()
        .max_by_key(|account| account.authenticated_at)
        .map(|account| account.account_id.clone());
}

fn load_store(root: &Path) -> AppResult<AccountStore> {
    let path = root.join("accounts.json");
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AccountStore::default());
        }
        Err(error) => return Err(AppError::Message(format!("read Grok accounts: {error}"))),
    };
    serde_json::from_slice(&bytes)
        .map_err(|error| AppError::Message(format!("decode Grok accounts: {error}")))
}

fn stable_account_id(user_id: &str) -> String {
    let digest = Sha256::digest(user_id.as_bytes());
    format!("grok-{}", hex_prefix(&digest, 12))
}

fn profile_key(account_id: &str) -> String {
    let digest = Sha256::digest(account_id.as_bytes());
    hex_prefix(&digest, 16)
}

fn hex_prefix(bytes: &[u8], count: usize) -> String {
    bytes
        .iter()
        .take(count)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn random_id(prefix: &str) -> AppResult<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| AppError::Message(format!("generate Grok login id: {error}")))?;
    Ok(format!("{prefix}-{}", hex_prefix(&bytes, bytes.len())))
}

fn ensure_child_path(root: &Path, target: &Path) -> AppResult<()> {
    let root = root
        .canonicalize()
        .map_err(|error| AppError::Message(format!("resolve Grok account root: {error}")))?;
    let parent = target.parent().unwrap_or(target);
    let parent = parent
        .canonicalize()
        .map_err(|error| AppError::Message(format!("resolve Grok account profile: {error}")))?;
    if !parent.starts_with(&root) {
        return Err(AppError::Message(
            "refusing to remove a Grok profile outside Vellum data".into(),
        ));
    }
    Ok(())
}

fn secure_profile_directory(path: &Path) -> AppResult<()> {
    // Windows profiles inherit the user-only ACL of Vellum's LocalAppData
    // directory. Unix needs an explicit owner-only mode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| AppError::Message(format!("secure Grok profile: {error}")))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_account_removal_never_deletes_external_home() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let external = temp.path().join("external");
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(external.join("sentinel"), "keep").unwrap();
        let manager = GrokAccountManager {
            root: data.join("grok_accounts"),
            external_home: external.clone(),
            store: RwLock::new(AccountStore {
                version: STORE_VERSION,
                accounts: HashMap::from([(
                    "grok-external".into(),
                    AccountMetadata {
                        account_id: "grok-external".into(),
                        email: Some("user@example.com".into()),
                        authenticated_at: 1,
                        source: GrokAccountSource::External,
                        profile_key: None,
                    },
                )]),
                default_account_id: Some("grok-external".into()),
                ignored_external_account_id: None,
            }),
            pending: Mutex::new(HashMap::new()),
            refresh_locks: RwLock::new(HashMap::new()),
        };
        manager.remove_account("grok-external").unwrap();
        assert!(external.join("sentinel").exists());
    }

    #[test]
    fn default_switch_is_persisted_and_hot() {
        let temp = tempfile::tempdir().unwrap();
        let manager = GrokAccountManager {
            root: temp.path().join("grok_accounts"),
            external_home: temp.path().join("external"),
            store: RwLock::new(AccountStore {
                version: STORE_VERSION,
                accounts: ["one", "two"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, id)| {
                        (
                            id.into(),
                            AccountMetadata {
                                account_id: id.into(),
                                email: None,
                                authenticated_at: index as i64,
                                source: GrokAccountSource::Managed,
                                profile_key: Some(id.into()),
                            },
                        )
                    })
                    .collect(),
                default_account_id: Some("one".into()),
                ignored_external_account_id: None,
            }),
            pending: Mutex::new(HashMap::new()),
            refresh_locks: RwLock::new(HashMap::new()),
        };
        let status = manager.set_default("two").unwrap();
        assert_eq!(status.default_account_id.as_deref(), Some("two"));
        assert_eq!(manager.peek_default_account_id().as_deref(), Some("two"));
    }

    #[tokio::test]
    async fn in_flight_resolution_keeps_the_account_selected_at_request_start() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("grok_accounts");
        for (profile, token, user_id) in [
            ("one-profile", "token-one", "user-one"),
            ("two-profile", "token-two", "user-two"),
        ] {
            let home = root.join("profiles").join(profile);
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(
                home.join("auth.json"),
                serde_json::json!({
                    "session": {
                        "access_token": token,
                        "user_id": user_id,
                        "expires_at": "2099-01-01T00:00:00Z"
                    }
                })
                .to_string(),
            )
            .unwrap();
        }
        let manager = GrokAccountManager {
            root,
            external_home: temp.path().join("external"),
            store: RwLock::new(AccountStore {
                version: STORE_VERSION,
                accounts: HashMap::from([
                    (
                        "one".into(),
                        AccountMetadata {
                            account_id: "one".into(),
                            email: None,
                            authenticated_at: 1,
                            source: GrokAccountSource::Managed,
                            profile_key: Some("one-profile".into()),
                        },
                    ),
                    (
                        "two".into(),
                        AccountMetadata {
                            account_id: "two".into(),
                            email: None,
                            authenticated_at: 2,
                            source: GrokAccountSource::Managed,
                            profile_key: Some("two-profile".into()),
                        },
                    ),
                ]),
                default_account_id: Some("one".into()),
                ignored_external_account_id: None,
            }),
            pending: Mutex::new(HashMap::new()),
            refresh_locks: RwLock::new(HashMap::new()),
        };

        let (first_id, first_credential) = manager.resolve_default().await.unwrap();
        manager.set_default("two").unwrap();
        let (second_id, second_credential) = manager.resolve_default().await.unwrap();

        assert_eq!(first_id, "one");
        assert_eq!(first_credential.access_token.as_str(), "token-one");
        assert_eq!(second_id, "two");
        assert_eq!(second_credential.access_token.as_str(), "token-two");
    }

    #[test]
    fn repeated_managed_login_updates_one_profile_and_remove_deletes_only_that_profile() {
        let temp = tempfile::tempdir().unwrap();
        let manager = GrokAccountManager {
            root: temp.path().join("grok_accounts"),
            external_home: temp.path().join("external"),
            store: RwLock::new(AccountStore::default()),
            pending: Mutex::new(HashMap::new()),
            refresh_locks: RwLock::new(HashMap::new()),
        };

        let write_staging = |name: &str, token: &str| {
            let staging = manager.root.join("staging").join(name);
            std::fs::create_dir_all(&staging).unwrap();
            std::fs::write(
                staging.join("auth.json"),
                serde_json::json!({
                    "session": {
                        "access_token": token,
                        "user_id": "same-user",
                        "email": "same@example.com",
                        "expires_at": "2099-01-01T00:00:00Z"
                    }
                })
                .to_string(),
            )
            .unwrap();
            staging
        };

        let first = manager
            .adopt_login(&write_staging("first", "token-one"))
            .unwrap();
        let second = manager
            .adopt_login(&write_staging("second", "token-two"))
            .unwrap();
        assert_eq!(first.account_id, second.account_id);
        assert_eq!(manager.status().accounts.len(), 1);
        let home = manager.account_home(&second.account_id).unwrap();
        let (credential, _) = grok_auth::read_credential(&home).unwrap().unwrap();
        assert_eq!(credential.access_token.as_str(), "token-two");

        manager.remove_account(&second.account_id).unwrap();
        assert!(!home.exists());
        assert!(manager.root.exists());
    }

    #[test]
    fn test_find_account_id_matches_id_and_email() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("grok_accounts");
        let manager = GrokAccountManager::new(root);

        let staging = temp.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(
            staging.join("auth.json"),
            serde_json::json!({
                "session": {
                    "access_token": "token-test",
                    "user_id": "user-12345",
                    "email": "Tester@Example.COM",
                    "expires_at": "2099-01-01T00:00:00Z"
                }
            })
            .to_string(),
        )
        .unwrap();

        let info = manager.adopt_login(&staging).unwrap();
        assert_eq!(
            manager.find_account_id(&info.account_id),
            Some(info.account_id.clone())
        );
        assert_eq!(
            manager.find_account_id("tester@example.com"),
            Some(info.account_id.clone())
        );
        assert_eq!(
            manager.find_account_id("TESTER@EXAMPLE.COM"),
            Some(info.account_id.clone())
        );
        assert_eq!(manager.find_account_id("nonexistent@example.com"), None);
    }
}
