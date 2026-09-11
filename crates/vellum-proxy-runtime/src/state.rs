use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{ProxyRuntimeConfig, ProxyRuntimeIdentity};
use crate::credentials::{CredentialProvider, MemoryCredentialProvider};
use crate::diagnostics::RuntimeDiagnostics;
use crate::environment::ExecutionEnvironment;
use crate::exec::ProxyRuntime;
use crate::history::FileHistoryStore;
use crate::lifecycle::CountingRequestLifecycle;
use crate::official_auth::FileManagedOfficialAuthProvider;
use crate::route::{ResolvedRoute, RouteCatalog, RuntimeModelRoute};
use crate::snapshot::RuntimeRouteSnapshot;
use crate::transport::{ReqwestTransport, UpstreamTransport};
use crate::usage::FileUsageStore;

/// The wire view of a route (`/v1/models`). Derived from the route config; it
/// is deliberately thin and is never the execution authority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelRouteView {
    pub catalog_id: String,
    pub route_id: String,
    pub upstream_model: String,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub owned_by: Option<String>,
}

#[async_trait]
pub trait ProxyRuntimeState: Send + Sync + 'static {
    fn identity(&self) -> ProxyRuntimeIdentity;
    fn active_model_routes(&self) -> Vec<ModelRouteView>;
    fn route_for_model(&self, catalog_id: &str) -> Option<ModelRouteView>;
    fn diagnostics(&self) -> RuntimeDiagnostics;
    async fn credentials(&self) -> Arc<dyn CredentialProvider>;
    /// The real execution engine. Boundary handlers dispatch through this;
    /// there is no separate mock response path anymore.
    fn proxy_runtime(&self) -> Arc<ProxyRuntime>;
    /// The explicit executor contract for this host's Codex tools, resolved
    /// from configuration/state — never derived by the proxy process itself
    /// (plan §25.2). The responses boundary forwards it into every
    /// [`RuntimeRequest`](crate::request::RuntimeRequest).
    fn execution_environment(&self) -> ExecutionEnvironment;
    /// The credential ID this proxy's boundary key is stored under. Defaults to
    /// the reserved ID so an embedding host that has not overridden it still
    /// resolves a key rather than silently skipping the guard.
    fn inbound_credential_id(&self) -> String {
        crate::inbound::BOUNDARY_CREDENTIAL_ID.to_string()
    }
}

/// Route catalog backed by the static `[[models]]` config. Each configured
/// route carries the catalog entry the config shipped (falling back to the
/// honest route-only projection when none is present).
#[derive(Debug, Clone)]
pub struct StaticRouteCatalog {
    route_snapshots: Vec<RuntimeRouteSnapshot>,
}

impl StaticRouteCatalog {
    pub fn from_config(config: &ProxyRuntimeConfig) -> Self {
        let route_snapshots = config
            .models
            .iter()
            .map(|route_config| {
                let route = route_config.to_route();
                let catalog_entry = route_config
                    .catalog_entry
                    .clone()
                    .unwrap_or_else(|| crate::snapshot::route_projection(&route));
                RuntimeRouteSnapshot::new(route, catalog_entry)
            })
            .collect();
        Self { route_snapshots }
    }

    pub fn route_snapshots(&self) -> &[RuntimeRouteSnapshot] {
        &self.route_snapshots
    }
}

impl RouteCatalog for StaticRouteCatalog {
    fn active_models(&self) -> Vec<RuntimeModelRoute> {
        self.route_snapshots
            .iter()
            .map(|snapshot| snapshot.route.clone())
            .collect()
    }

    fn resolve_model(&self, catalog_id: &str) -> Option<ResolvedRoute> {
        self.route_snapshots
            .iter()
            .find(|snapshot| snapshot.route.catalog_id == catalog_id)
            .map(|snapshot| ResolvedRoute {
                route: snapshot.route.clone(),
            })
    }

    fn resolve_review_model(&self, catalog_id: &str) -> Option<ResolvedRoute> {
        self.resolve_model(catalog_id)
    }

    fn catalog_entry(&self, catalog_id: &str) -> Option<Value> {
        self.route_snapshots
            .iter()
            .find(|snapshot| snapshot.route.catalog_id == catalog_id)
            .map(|snapshot| snapshot.catalog_entry.clone())
    }
}

#[derive(Clone)]
pub struct StaticProxyState {
    config: Arc<ProxyRuntimeConfig>,
    credentials: Arc<dyn CredentialProvider>,
    listener_ready: bool,
    config_valid: bool,
    secrets_readable: bool,
    runtime: Arc<ProxyRuntime>,
}

impl StaticProxyState {
    /// Build the state and its real execution engine over the production
    /// transport (real HTTP).
    pub fn from_config(config: ProxyRuntimeConfig) -> Result<Self, String> {
        Self::from_config_with_transport_and_durability(
            config,
            Arc::new(ReqwestTransport::new()),
            !cfg!(test),
            None,
        )
    }

    /// Build the state with a caller-supplied transport (tests, parity runs).
    pub fn from_config_with_transport(
        config: ProxyRuntimeConfig,
        transport: Arc<dyn UpstreamTransport>,
    ) -> Result<Self, String> {
        Self::from_config_with_transport_and_durability(config, transport, false, None)
    }

    /// Same as [`Self::from_config`], with an explicit diagnostic sink so
    /// tests can observe `WebSocketClosed.frames_out`.
    pub fn from_config_with_diagnostics(
        config: ProxyRuntimeConfig,
        diagnostics: Arc<dyn crate::diagnostics::DiagnosticsSink>,
    ) -> Result<Self, String> {
        Self::from_config_with_transport_and_durability(
            config,
            Arc::new(ReqwestTransport::new()),
            false,
            Some(diagnostics),
        )
    }

    fn from_config_with_transport_and_durability(
        mut config: ProxyRuntimeConfig,
        transport: Arc<dyn UpstreamTransport>,
        durable: bool,
        diagnostics: Option<Arc<dyn crate::diagnostics::DiagnosticsSink>>,
    ) -> Result<Self, String> {
        config.enforce_grok_responses();
        config.identity = config.identity.clone().with_process_identity();
        config.validate()?;
        if durable {
            config.ensure_dirs()?;
        }
        let credentials: Arc<dyn CredentialProvider> = if let Some(dir) = &config.credentials_dir {
            Arc::new(crate::credentials::FileCredentialProvider::new(dir.clone()))
        } else {
            Arc::new(MemoryCredentialProvider::new())
        };
        let catalog = Arc::new(StaticRouteCatalog::from_config(&config));
        let credential_refs = config
            .models
            .iter()
            .filter(|route| route.provider_kind != crate::route::RuntimeProviderKind::Official)
            .filter_map(|route| route.credential_id.as_deref())
            .collect::<Vec<_>>();
        let official_credentials_readable = config
            .models
            .iter()
            .filter(|route| route.provider_kind == crate::route::RuntimeProviderKind::Official)
            .filter_map(|route| route.credential_id.as_deref())
            .all(|credential_id| {
                let root = config.data_dir.join("official-auth");
                if credential_id == crate::official_auth::SELECTED_OFFICIAL_CREDENTIAL_ID {
                    let selected_path = root.join("selected.json");
                    if !selected_path.exists() {
                        // No execution account B selected: preserve the
                        // incoming control account A authorization.
                        true
                    } else {
                        std::fs::read(&selected_path)
                            .ok()
                            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                            .and_then(|selected| {
                                selected
                                    .get("accountIdHash")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                            })
                            .is_some_and(|hash| {
                                hash.len() == 64
                                    && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                                    && root.join("grants").join(format!("{hash}.json")).is_file()
                            })
                    }
                } else {
                    root.join("grants")
                        .join(format!("{credential_id}.json"))
                        .is_file()
                }
            });
        let file_secrets_readable = config.credentials_dir.as_ref().is_some_and(|dir| {
            credential_refs.iter().all(|id| {
                let path = dir.join(id);
                path.is_file() && std::fs::File::open(path).is_ok()
            })
        });
        let secrets_readable = (credential_refs.is_empty() || file_secrets_readable)
            && official_credentials_readable
            && (!config.require_secrets || config.credentials_dir.is_some());
        let official_routes = config
            .models
            .iter()
            .filter(|route| route.provider_kind == crate::route::RuntimeProviderKind::Official)
            .map(|route| (route.route_id.clone(), route.credential_id.clone()))
            .collect::<Vec<_>>();
        let official_auth: Arc<dyn crate::official_auth::OfficialAuthProvider> =
            Arc::new(FileManagedOfficialAuthProvider::new(
                config.data_dir.join("official-auth"),
                official_routes,
            ));
        let mut runtime = ProxyRuntime::new(
            catalog,
            Arc::clone(&credentials),
            official_auth,
            Arc::new(CountingRequestLifecycle::new()),
            transport,
        )
        .with_resource_policy(crate::resource::ResourcePolicy::from_config(
            &config.resource,
        ))
        .with_review_config(config.review.clone())
        .with_diagnostic_config_hash(config.identity.config_hash.clone());
        if let Some(diagnostics) = diagnostics {
            runtime = runtime.with_diagnostics_sink(diagnostics);
        }
        if durable {
            let history = Arc::new(FileHistoryStore::open(
                config.history_dir.join("history.jsonl"),
            )?);
            let usage = Arc::new(FileUsageStore::open(config.data_dir.join("usage.jsonl"))?);
            runtime = runtime.with_history_store(history).with_usage_store(usage);
        }
        let config_valid = config.grok_rewrite_error.is_none();
        Ok(Self {
            config: Arc::new(config),
            credentials,
            listener_ready: false,
            config_valid,
            secrets_readable,
            runtime: Arc::new(runtime),
        })
    }

    pub fn mark_listener_ready(&mut self) {
        self.listener_ready = true;
    }

    pub fn set_secrets_readable(&mut self, value: bool) {
        self.secrets_readable = value;
    }

    pub fn config(&self) -> &ProxyRuntimeConfig {
        &self.config
    }
}

#[async_trait]
impl ProxyRuntimeState for StaticProxyState {
    fn identity(&self) -> ProxyRuntimeIdentity {
        self.config.identity.clone()
    }

    fn active_model_routes(&self) -> Vec<ModelRouteView> {
        self.config
            .models
            .iter()
            .map(|route| ModelRouteView {
                catalog_id: route.catalog_id.clone(),
                route_id: route.route_id.clone(),
                upstream_model: route.upstream_model.clone(),
                context_window: route.context_window,
                owned_by: Some(route.name.clone()),
            })
            .collect()
    }

    fn route_for_model(&self, catalog_id: &str) -> Option<ModelRouteView> {
        self.config
            .models
            .iter()
            .find(|route| route.catalog_id == catalog_id)
            .map(|route| ModelRouteView {
                catalog_id: route.catalog_id.clone(),
                route_id: route.route_id.clone(),
                upstream_model: route.upstream_model.clone(),
                context_window: route.context_window,
                owned_by: Some(route.name.clone()),
            })
    }

    fn diagnostics(&self) -> RuntimeDiagnostics {
        // When require_secrets is true, secrets_readable already gates readiness.
        // When false, secrets may be optional placeholders but still must be readable
        // enough for the daemon to start without I/O errors.
        let grok_ready = crate::route::grok_responses_readiness(&self.config.models);
        let mut notes = Vec::new();
        if let Err(error) = &grok_ready {
            notes.push(error.clone());
        }
        if let Some(error) = &self.config.grok_rewrite_error {
            notes.push(error.clone());
        }
        let ready = self.config_valid
            && self.listener_ready
            && self.secrets_readable
            && grok_ready.is_ok()
            && self.config.grok_rewrite_error.is_none();
        RuntimeDiagnostics {
            ok: true,
            ready,
            identity: self.identity(),
            model_count: self.config.models.len(),
            listener_ready: self.listener_ready,
            config_valid: self.config_valid,
            secrets_readable: self.secrets_readable,
            notes,
        }
    }

    async fn credentials(&self) -> Arc<dyn CredentialProvider> {
        Arc::clone(&self.credentials)
    }

    fn proxy_runtime(&self) -> Arc<ProxyRuntime> {
        Arc::clone(&self.runtime)
    }

    fn execution_environment(&self) -> ExecutionEnvironment {
        self.config.execution_environment.clone()
    }

    fn inbound_credential_id(&self) -> String {
        self.config.inbound_access.credential_id.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_daemon_state_opens_stores_and_fails_closed_on_corruption() {
        let temp = tempfile::tempdir().unwrap();
        let config = ProxyRuntimeConfig {
            data_dir: temp.path().join("data"),
            history_dir: temp.path().join("history"),
            log_dir: temp.path().join("logs"),
            ..ProxyRuntimeConfig::default()
        };
        StaticProxyState::from_config_with_transport_and_durability(
            config.clone(),
            Arc::new(ReqwestTransport::new()),
            true,
            None,
        )
        .unwrap();
        assert!(config.data_dir.join("usage.jsonl").is_file());
        assert!(config.history_dir.is_dir());

        std::fs::write(config.data_dir.join("usage.jsonl"), "corrupt\n").unwrap();
        assert!(StaticProxyState::from_config_with_transport_and_durability(
            config,
            Arc::new(ReqwestTransport::new()),
            true,
            None,
        )
        .is_err());
    }

    #[test]
    fn grok_persist_failure_fails_readiness_and_config_validity() {
        let mut config = ProxyRuntimeConfig::default();
        config.models.push(crate::config::RuntimeRouteConfig {
            route_id: "grok-cli".into(),
            catalog_id: "vlm-grok".into(),
            name: "Grok Build".into(),
            base_url: "https://cli-chat-proxy.grok.com/v1".into(),
            provider_kind: crate::route::RuntimeProviderKind::GrokCli,
            auth_kind: crate::route::RuntimeAuthKind::GrokSession,
            wire: crate::route::RuntimeWireFormat::Responses,
            server_side_resume: false,
            streaming: true,
            reasoning: true,
            vision: false,
            upstream_model: "grok-4.5".into(),
            context_window: None,
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: Default::default(),
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: None,
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        });
        config.grok_rewrite_error =
            Some("Grok Responses rewrite could not be persisted: disk full".into());
        let mut state =
            StaticProxyState::from_config_with_transport(config, Arc::new(ReqwestTransport::new()))
                .unwrap();
        state.mark_listener_ready();
        let diagnostics = state.diagnostics();
        assert!(!diagnostics.config_valid);
        assert!(!diagnostics.ready);
        assert!(
            diagnostics
                .notes
                .iter()
                .any(|note| note.contains("could not be persisted")),
            "{:?}",
            diagnostics.notes
        );
    }
}
