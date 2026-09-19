//! 應用狀態。
//!
//! 種子資料形狀已是最終形狀，doc/03 起的各章節會把每個欄位換成真的來源，
//! UI 一行都不用改。quota 欄位已接線（doc/08）：透過 ACP 查 Grok 額度。

use crate::error::{AppError, AppResult};
use crate::history::{CompactionEngine, HistoryStore, DEFAULT_RETENTION_DAYS};
use crate::model::*;
use crate::policy::{
    resolve_route_policy, PolicyResolutionInput, ResolvedCompactionPolicy, SessionCompactionPolicy,
};
use crate::quota::QuotaService;
use crate::remote::RemoteClientManager;
use crate::usage::UsageStore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

type LifecycleSink = Arc<dyn Fn(ProxyStatus) + Send + Sync>;

/// Vellum 在使用者資料目錄下的子目錄名（跨平台資料，不是快取）。
const DATA_SUBDIR: &str = "vellum";

/// 回傳 Vellum 專屬的資料目錄。
///
/// Windows：`%LOCALAPPDATA%\vellum`（roaming 會跟著帳號走，不適合放可能變大的 JSON）。
/// 測試一律傳 temp dir 進來，不靠這個函式。
///
/// 例外是 `ssh_trust::runtime_data_root()`：走 SSH 的呼叫點手上只有 client，
/// 沒有 `AppState`，所以它固定讀這裡。`tests/remote_dev_host.rs` 需要知道那是
/// 哪個目錄才能把 loopback 假主機的 host key 確認寫對地方，因此這個函式是
/// 公開的——它只回傳路徑，不做任何事。
pub fn app_data_dir() -> PathBuf {
    let base = dirs::data_local_dir().unwrap_or_else(std::env::temp_dir);
    base.join(DATA_SUBDIR)
}

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Mutex<Inner>>,
    data_root: PathBuf,
    eval_mode: bool,
    codex_oauth: Arc<crate::codex_oauth::CodexOAuthManager>,
    grok_accounts: Arc<crate::grok_accounts::GrokAccountManager>,
    /// 額度服務獨立持有，用 Arc 包起來——async 指令可 clone 一份搬進
    /// `spawn_blocking`，不卡 Mutex 也不卡 Tauri State 的生命週期。
    /// QuotaService 內部自己用 Mutex 保護快取，所以 Arc 就夠。
    quota: Arc<QuotaService>,
    usage: Arc<UsageStore>,
    history: Arc<HistoryStore>,
    proxy: Arc<Mutex<ProxyControl>>,
    generation: Arc<vellum_proxy_runtime::GenerationController>,
    lifecycle: Arc<tokio::sync::Mutex<()>>,
    prepare: Arc<tokio::sync::Mutex<crate::proxy_prepare::PrepareCoordinator>>,
    remote: Arc<RemoteClientManager>,
    grok_rewrite_error: Arc<Mutex<Option<String>>>,
    eval_canonical_engine:
        Arc<Mutex<Option<vellum_proxy_runtime::compaction::CanonicalEngineVersion>>>,
    eval_recovery_enabled: Arc<Mutex<bool>>,
    /// Eval-only per-route compaction overrides.
    ///
    /// Deliberately not part of the persisted `Inner`: the product has no
    /// compaction strategy to configure any more, but an A/B run still has to
    /// be able to reproduce a historical Canonical V1/V2 baseline to compare
    /// against. Writes are refused outside eval mode, so this cannot become a
    /// back door that reinstates Canonical on a real install.
    eval_route_compaction_policies: Arc<Mutex<HashMap<String, SessionCompactionPolicy>>>,
    /// Set once at startup when a legacy `duckduckgo`/`searxng` web search
    /// configuration was normalized to Brave-only and had to be auto-disabled
    /// for lack of a Brave key (see `migrate_web_search_settings`). Surfaced
    /// through `get_web_search_settings` so Settings can explain the change
    /// instead of leaving search silently off.
    web_search_migration_notice: Arc<Mutex<Option<RuntimeNotice>>>,
    lifecycle_tx: tokio::sync::watch::Sender<ProxyStatus>,
    lifecycle_sink: Arc<Mutex<Option<LifecycleSink>>>,
}

struct ProxyControl {
    status: ProxyStatus,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    /// Join handle for the `serve_local_proxy` task. Taken and awaited during
    /// graceful stop so history maintenance runs only after the server exits.
    server_task: Option<tauri::async_runtime::JoinHandle<()>>,
    runtime: Option<std::sync::Arc<vellum_proxy_runtime::ProxyRuntime>>,
    active_routes: Vec<Route>,
    active_model_routes: Vec<ModelRoute>,
    active_requests: u64,
    draining: bool,
    restart_reasons: Vec<RuntimeNotice>,
    restart_process_identity: Option<String>,
    /// The launch id of a superseded live launch this session already spent
    /// its one automatic repair on. See [`AppState::claim_launch_repair`].
    repaired_launch_id: Option<String>,
    live_applied: Vec<RuntimeNotice>,
    active_catalog_version: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Inner {
    #[serde(default = "seed_routes")]
    routes: Vec<Route>,
    #[serde(default)]
    review: ReviewSettings,
    /// route_id → 手動覆寫的 token 數。
    #[serde(default)]
    overrides: HashMap<String, u64>,
    #[serde(default)]
    findings: Vec<Finding>,
    /// Third-party (non-OpenAI) web search configuration exposed at
    /// `POST /v1/alpha/search`. Default for a fresh install: OFF.
    #[serde(default)]
    web_search: crate::web_search::WebSearchSettings,
    /// Vellum-managed defaults for Codex native sub-agents. `inherit` keeps
    /// Codex's own behaviour; `custom` writes `[agents]` defaults on the next
    /// Proxy apply and hot-updates them while the Proxy is running.
    #[serde(default)]
    subagent: SubagentSettings,
}

pub struct ActiveRequestGuard {
    proxy: Arc<Mutex<ProxyControl>>,
}

/// Result of waiting for the proxy to become idle enough for stopped-state
/// history maintenance (issue #1 timeout fail-closed invariant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyStopOutcome {
    /// Active request guards are zero **and** the server task exited cleanly
    /// before the deadline. WAL/VACUUM maintenance is safe.
    Idle,
    /// Deadline elapsed while one or more request guards were still held.
    TimedOut { active_requests: u64 },
    /// Server task did not finish before the deadline and was aborted.
    ServerTaskTimedOut { active_requests: u64 },
    /// Server task join failed after shutdown.
    ServerTaskFailed {
        active_requests: u64,
        message: String,
    },
}

impl ProxyStopOutcome {
    pub fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        let mut proxy = self.proxy.lock().expect("proxy state poisoned");
        proxy.active_requests = proxy.active_requests.saturating_sub(1);
    }
}

impl AppState {
    /// 正式入口：用使用者資料目錄當落盤根。
    pub fn new() -> Self {
        Self::try_new().expect("failed to initialize Vellum application state")
    }

    pub fn try_new() -> AppResult<Self> {
        Self::try_with_data_dir(app_data_dir())
    }

    /// 測試 / 自訂入口：顯式指定落盤根目錄。
    pub fn with_data_dir(root: PathBuf) -> Self {
        Self::try_with_data_dir(root).expect("failed to initialize Vellum application state")
    }

    pub fn try_with_data_dir(root: PathBuf) -> AppResult<Self> {
        Self::try_assemble(root, |root| {
            HistoryStore::open(root.join("history.sqlite3"), root)
        })
    }

    /// Isolated test state: history is sealed with a per-call fixture key
    /// so gateway tests never read `VELLUM_MASTER_KEY` and do not need the
    /// process-wide crypto env lock across `.await`.
    #[cfg(test)]
    pub fn with_test_fixtures(root: PathBuf) -> Self {
        let cipher = crate::crypto::JournalCipher::from_key(
            vec![0x51; crate::crypto::KEY_BYTES],
            "test-fixture",
        )
        .expect("test fixture master key");
        Self::try_assemble(root, move |root| {
            HistoryStore::open_with_cipher(root.join("history.sqlite3"), cipher)
        })
        .expect("failed to initialize isolated test AppState")
    }

    fn try_assemble(
        root: PathBuf,
        open_history: impl FnOnce(&std::path::Path) -> AppResult<HistoryStore>,
    ) -> AppResult<Self> {
        let lease_active = root == app_data_dir()
            && crate::codex::has_active_lease(&crate::codex::CodexPaths::discover(&root));
        let mut inner = load_inner(&root).unwrap_or_else(|| Inner {
            routes: seed_routes(),
            review: ReviewSettings {
                route_id: "grok-cli".to_string(),
                model: "grok-4.5".to_string(),
                ..ReviewSettings::default()
            },
            overrides: HashMap::new(),
            findings: Vec::new(),
            web_search: crate::web_search::WebSearchSettings::default(),
            subagent: SubagentSettings::default(),
        });
        let migrated = migrate_seeded_route_names(&mut inner.routes)
            | migrate_opencode_access_modes(&mut inner.routes)
            | migrate_opencode_catalog_scopes(&mut inner.routes);
        let outbound_disabled = disable_noncompliant_outbound_routes(&mut inner.routes);
        // Brave is now the only web search backend. A settings file written
        // before this change may still carry `enabled: true` from a
        // duckduckgo/searxng configuration (those fields are simply dropped
        // by serde on load, since they no longer exist on the struct). That
        // collapses to one rule regardless of which backend was previously
        // selected: search stays enabled only if a usable Brave key is
        // already on disk; otherwise it must be turned off explicitly here,
        // never left silently pointed at a backend that no longer exists.
        let has_brave_key =
            crate::credentials::load(&root, crate::web_search::BRAVE_SEARCH_CREDENTIAL_ID)
                .ok()
                .flatten()
                .is_some();
        let web_search_migration_notice =
            migrate_web_search_settings(&mut inner.web_search, has_brave_key);
        let mut grok_rewrite_error = None;
        if migrated || outbound_disabled || web_search_migration_notice.is_some() {
            if let Err(error) = try_persist_inner(&root, &inner) {
                log::warn!("[State] 設定落盤失敗：{error}");
                if migrated {
                    grok_rewrite_error = Some(format!(
                        "Grok Responses rewrite could not be persisted: {error}"
                    ));
                }
            }
        }
        let usage = UsageStore::open(root.join("usage.sqlite3"))?;
        let history = open_history(&root)?;
        if let Err(error) = history.evict_older_than_days(DEFAULT_RETENTION_DAYS) {
            log::warn!("[History] startup retention cleanup failed: {error}");
        }
        let remote = RemoteClientManager::open(root.clone())
            .expect("failed to initialize Vellum remote client cache");
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
            data_root: root.clone(),
            eval_mode: false,
            codex_oauth: Arc::new(crate::codex_oauth::CodexOAuthManager::new(root.clone())),
            grok_accounts: Arc::new(crate::grok_accounts::GrokAccountManager::new(root.clone())),
            quota: Arc::new(QuotaService::with_grok(root)),
            usage: Arc::new(usage),
            history: Arc::new(history),
            remote: Arc::new(remote),
            grok_rewrite_error: Arc::new(Mutex::new(grok_rewrite_error)),
            eval_canonical_engine: Arc::new(Mutex::new(None)),
            eval_recovery_enabled: Arc::new(Mutex::new(false)),
            eval_route_compaction_policies: Arc::new(Mutex::new(HashMap::new())),
            web_search_migration_notice: Arc::new(Mutex::new(web_search_migration_notice)),
            generation: Arc::new(vellum_proxy_runtime::GenerationController::new()),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
            prepare: Arc::new(tokio::sync::Mutex::new(
                crate::proxy_prepare::PrepareCoordinator::new(),
            )),
            lifecycle_tx: tokio::sync::watch::channel(ProxyStatus::default()).0,
            lifecycle_sink: Arc::new(Mutex::new(None)),
            proxy: Arc::new(Mutex::new(ProxyControl {
                status: ProxyStatus {
                    running: false,
                    base_url: "http://127.0.0.1:15721/v1".into(),
                    catalog_path: None,
                    codex_managed: lease_active,
                    last_error: None,
                    // 上次結束時沒有還原 Codex 設定（當掉或被強制關閉），
                    // 所以 config.toml 現在還指著 Vellum。不是錯誤，是狀態。
                    notice: lease_active
                        .then(|| RuntimeNotice::new("codexConfigStillPointedAtVellum")),
                    ..Default::default()
                },
                shutdown: None,
                server_task: None,
                runtime: None,
                active_routes: Vec::new(),
                active_model_routes: Vec::new(),
                active_requests: 0,
                draining: false,
                restart_reasons: Vec::new(),
                restart_process_identity: None,
                repaired_launch_id: None,
                live_applied: Vec::new(),
                active_catalog_version: None,
            })),
        })
    }

    /// Creates an isolated runtime state for the developer evaluation runner.
    ///
    /// Provider routing is copied exactly (so catalog IDs remain stable), but
    /// history, usage, OAuth metadata, and other mutable state live under the
    /// supplied temporary directory. This prevents benchmark traffic from
    /// changing the desktop application's statistics or conversation journal.
    pub(crate) fn with_eval_routes(root: PathBuf, routes: Vec<Route>) -> Self {
        let mut state = Self::with_data_dir(root);
        state.eval_mode = true;
        {
            let mut inner = state.inner.lock().expect("state poisoned");
            inner.routes = routes;
            persist_inner(&state.data_root, &inner);
        }
        state
    }

    #[allow(dead_code)]
    pub(crate) fn eval_mode(&self) -> bool {
        self.eval_mode
    }

    pub(crate) fn set_eval_compaction_engine(&self, engine: CompactionEngine) {
        self.history.set_eval_compaction_engine(engine);
    }

    pub(crate) fn set_eval_canonical_engine(
        &self,
        engine: Option<vellum_proxy_runtime::compaction::CanonicalEngineVersion>,
    ) {
        if self.eval_mode {
            let mut guard = self
                .eval_canonical_engine
                .lock()
                .expect("eval_canonical_engine poisoned");
            *guard = engine;
        }
    }

    /// Force a compaction policy onto one route for an eval run. A no-op
    /// outside eval mode — production resolution has no override layer.
    pub(crate) fn set_eval_route_compaction_policy(
        &self,
        route_id: &str,
        policy: Option<SessionCompactionPolicy>,
    ) {
        if !self.eval_mode {
            return;
        }
        let mut guard = self
            .eval_route_compaction_policies
            .lock()
            .expect("eval_route_compaction_policies poisoned");
        match policy {
            Some(policy) => {
                guard.insert(route_id.to_string(), policy);
            }
            None => {
                guard.remove(route_id);
            }
        }
    }

    fn eval_route_compaction_policy(&self, route_id: &str) -> Option<SessionCompactionPolicy> {
        if !self.eval_mode {
            return None;
        }
        self.eval_route_compaction_policies
            .lock()
            .ok()
            .and_then(|guard| guard.get(route_id).cloned())
    }

    #[cfg(test)]
    pub(crate) fn eval_canonical_engine(
        &self,
    ) -> Option<vellum_proxy_runtime::compaction::CanonicalEngineVersion> {
        if self.eval_mode {
            self.eval_canonical_engine
                .lock()
                .ok()
                .and_then(|guard| *guard)
        } else {
            None
        }
    }

    pub(crate) fn set_eval_recovery_enabled(&self, enabled: bool) {
        if self.eval_mode {
            *self
                .eval_recovery_enabled
                .lock()
                .expect("eval_recovery_enabled poisoned") = enabled;
        }
    }

    pub(crate) fn eval_recovery_enabled(&self) -> bool {
        self.eval_mode
            && self
                .eval_recovery_enabled
                .lock()
                .map(|guard| *guard)
                .unwrap_or(false)
    }

    pub(crate) fn is_eval_mode(&self) -> bool {
        self.eval_mode
    }

    #[cfg(test)]
    pub(crate) fn runtime_canonical_engine(
        &self,
    ) -> Option<vellum_proxy_runtime::compaction::CanonicalEngineVersion> {
        self.eval_canonical_engine()
    }

    /// Evaluation-only state that keeps mutable history/statistics isolated
    /// while borrowing the host OAuth manager for an explicitly requested
    /// official control model. Provider credentials never enter the agent
    /// container or the temporary eval directory.
    pub(crate) fn with_eval_routes_and_oauth(
        root: PathBuf,
        routes: Vec<Route>,
        codex_oauth: Arc<crate::codex_oauth::CodexOAuthManager>,
    ) -> Self {
        let mut state = Self::with_eval_routes(root, routes);
        state.codex_oauth = codex_oauth;
        state
    }

    pub fn routes(&self) -> Vec<Route> {
        let routes = self.inner.lock().expect("state poisoned").routes.clone();
        visible_routes(routes, grok_cli_available_for_runtime())
    }

    pub fn current_route(&self) -> Option<Route> {
        self.routes().into_iter().find(|route| route.is_current)
    }

    /// 線路 ID → 目前的顯示名稱。
    ///
    /// 用量資料庫每一列都存了當下的供應商名稱，那是歷史快照：改名之後
    /// 舊列還是舊名字，狀態列與統計表就會同時出現同一家的兩個名字。
    /// 讀出來的時候一律用 ID 換回現在的名稱，存的那份只當線路已被刪除
    /// 時的最後依據。
    pub fn route_display_names(&self) -> HashMap<String, String> {
        self.inner
            .lock()
            .expect("state poisoned")
            .routes
            .iter()
            .map(|route| (route.id.clone(), route.name.clone()))
            .collect()
    }

    pub fn select_route(&self, route_id: &str) -> bool {
        let mut guard = self.inner.lock().expect("state poisoned");
        let Some(selected) = guard
            .routes
            .iter()
            .find(|route| route.id == route_id && route.enabled)
        else {
            return false;
        };
        if selected.provider_kind == ProviderKind::GrokCli && !grok_cli_available_for_runtime() {
            return false;
        }
        for route in guard.routes.iter_mut() {
            route.is_current = route.id == route_id;
        }
        persist_inner(&self.data_root, &guard);
        true
    }

    pub fn set_route_enabled(&self, route_id: &str, enabled: bool) -> bool {
        let mut guard = self.inner.lock().expect("state poisoned");
        let Some(route) = guard.routes.iter_mut().find(|route| route.id == route_id) else {
            return false;
        };
        route.enabled = enabled;
        if !enabled && route.is_current {
            route.is_current = false;
            if let Some(fallback) = guard.routes.iter_mut().find(|route| route.enabled) {
                fallback.is_current = true;
            }
        }
        persist_inner(&self.data_root, &guard);
        true
    }

    /// Third-party plaintext HTTP exemption, per route. Official routes
    /// ignore this field entirely (see `Route::insecure_http_policy`), so
    /// setting it there is accepted as a no-op rather than an error — the
    /// caller does not need to special-case Official just to skip the call.
    pub fn set_route_insecure_http_policy(
        &self,
        route_id: &str,
        policy: crate::model::InsecureHttpPolicy,
    ) -> bool {
        let mut guard = self.inner.lock().expect("state poisoned");
        let Some(route) = guard.routes.iter_mut().find(|route| route.id == route_id) else {
            return false;
        };
        route.insecure_http_policy = policy;
        persist_inner(&self.data_root, &guard);
        true
    }

    pub fn set_route_models(&self, route_id: &str, selected: Vec<String>) -> bool {
        let mut guard = self.inner.lock().expect("state poisoned");
        let Some(route) = guard.routes.iter_mut().find(|route| route.id == route_id) else {
            return false;
        };
        if route.provider_kind == ProviderKind::Official || selected.is_empty() {
            return false;
        }
        let mut normalized = Vec::new();
        for model in selected {
            if route
                .models
                .iter()
                .any(|available| available.eq_ignore_ascii_case(&model))
                && !normalized
                    .iter()
                    .any(|chosen: &String| chosen.eq_ignore_ascii_case(&model))
            {
                normalized.push(model);
            }
        }
        if normalized.is_empty() {
            return false;
        }
        if !normalized
            .iter()
            .any(|model| model.eq_ignore_ascii_case(&route.model))
        {
            route.model = normalized[0].clone();
        }
        route.selected_models = Some(normalized);
        persist_inner(&self.data_root, &guard);
        true
    }

    /// Replace a Grok Build route's available catalog from `grok models`.
    /// Existing selections remain enabled and the CLI default is added so a
    /// newly released model is immediately available without disconnecting an
    /// older model that an existing session still references.
    pub fn replace_grok_model_catalog(
        &self,
        route_id: &str,
        models: Vec<String>,
        default_model: Option<&str>,
    ) -> bool {
        if models.is_empty() {
            return false;
        }
        let mut guard = self.inner.lock().expect("state poisoned");
        let Some(route) = guard.routes.iter_mut().find(|route| route.id == route_id) else {
            return false;
        };
        if route.provider_kind != ProviderKind::GrokCli {
            return false;
        }
        let mut available = Vec::new();
        for model in models {
            let model = model.trim();
            if !model.is_empty()
                && !available
                    .iter()
                    .any(|known: &String| known.eq_ignore_ascii_case(model))
            {
                available.push(model.to_string());
            }
        }
        if available.is_empty() {
            return false;
        }
        let default = default_model
            .and_then(|default| {
                available
                    .iter()
                    .find(|model| model.eq_ignore_ascii_case(default))
            })
            .cloned()
            .unwrap_or_else(|| available[0].clone());
        let mut selected = route.selected_models.clone().unwrap_or_default();
        selected.retain(|selected| {
            available
                .iter()
                .any(|model| model.eq_ignore_ascii_case(selected))
        });
        if !selected
            .iter()
            .any(|model| model.eq_ignore_ascii_case(&default))
        {
            selected.push(default.clone());
        }
        route.model_capabilities.retain(|capability| {
            available
                .iter()
                .any(|model| model.eq_ignore_ascii_case(&capability.model))
        });
        route.models = available;
        route.selected_models = Some(selected);
        route.model = default;
        persist_inner(&self.data_root, &guard);
        true
    }

    /// Rename a Provider for display.
    ///
    /// Deliberately does not touch `route.id`. The id is half of
    /// `stable_catalog_id`, so changing it would rewrite every catalog id on
    /// the route and strand any model Codex already knows about — a rename
    /// would silently behave like delete-and-recreate.
    pub fn set_route_name(&self, route_id: &str, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        let mut guard = self.inner.lock().expect("state poisoned");
        let Some(route) = guard.routes.iter_mut().find(|route| route.id == route_id) else {
            return false;
        };
        if route.name == name {
            return true;
        }
        route.name = name.to_string();
        persist_inner(&self.data_root, &guard);
        true
    }

    /// Rename one upstream model for display.
    ///
    /// Stored on the model's capability entry, creating a bare one when the
    /// model has never been probed. Passing an empty name clears the alias and
    /// restores the upstream id. The upstream id itself is never touched: it is
    /// what gets sent to the provider and the other half of the catalog id.
    pub fn set_model_display_name(&self, route_id: &str, model: &str, name: &str) -> bool {
        let name = name.trim();
        let alias = (!name.is_empty()).then(|| name.to_string());
        let mut guard = self.inner.lock().expect("state poisoned");
        let Some(route) = guard.routes.iter_mut().find(|route| route.id == route_id) else {
            return false;
        };
        if route.provider_kind == ProviderKind::Official {
            return false;
        }
        if !route
            .models
            .iter()
            .any(|available| available.eq_ignore_ascii_case(model))
        {
            return false;
        }
        match route
            .model_capabilities
            .iter_mut()
            .find(|capability| capability.model.eq_ignore_ascii_case(model))
        {
            Some(capability) => capability.display_name = alias,
            None => route.model_capabilities.push(ModelCapability {
                model: model.to_string(),
                display_name: alias,
                ..Default::default()
            }),
        }
        persist_inner(&self.data_root, &guard);
        true
    }

    /// Declare whether an upstream model accepts image input.
    ///
    /// Stored on the model's capability entry, creating a bare one if the model
    /// has never been probed — the switch has to survive on a route whose probe
    /// results are missing or from an older probe version.
    pub fn set_model_vision(&self, route_id: &str, model: &str, vision: bool) -> bool {
        let mut guard = self.inner.lock().expect("state poisoned");
        let Some(route) = guard.routes.iter_mut().find(|route| route.id == route_id) else {
            return false;
        };
        if route.provider_kind == ProviderKind::Official {
            return false;
        }
        if !route
            .models
            .iter()
            .any(|available| available.eq_ignore_ascii_case(model))
        {
            return false;
        }
        match route
            .model_capabilities
            .iter_mut()
            .find(|capability| capability.model.eq_ignore_ascii_case(model))
        {
            Some(capability) => capability.vision = Some(vision),
            None => route.model_capabilities.push(ModelCapability {
                model: model.to_string(),
                vision: Some(vision),
                ..Default::default()
            }),
        }
        persist_inner(&self.data_root, &guard);
        true
    }

    /// Promote a successful full Codex request into a current dialect probe.
    /// This migrates legacy minimal-probe results without trusting them: the
    /// capability is recorded only after an actual Desktop-shaped request has
    /// succeeded, including a one-time safe Responses-to-Chat retry.
    pub fn record_runtime_model_capability(
        &self,
        route_id: &str,
        model: &str,
        wire: WireFormat,
    ) -> bool {
        let mut guard = self.inner.lock().expect("state poisoned");
        let Some(route) = guard.routes.iter_mut().find(|route| route.id == route_id) else {
            return false;
        };
        if route.provider_kind != ProviderKind::OpenAiCompatible {
            return false;
        }
        if let Some(capability) = route
            .model_capabilities
            .iter_mut()
            .find(|capability| capability.model.eq_ignore_ascii_case(model))
        {
            if capability.probe_version == Some(crate::probe::HARNESS_PROBE_VERSION)
                && capability.wire == Some(wire)
                && capability.tool_calling == Some(true)
                && capability.probe_issue.is_none()
            {
                return false;
            }
            capability.wire = Some(wire);
            capability.streaming = Some(true);
            capability.reasoning = Some(route.reasoning);
            capability.tool_calling = Some(true);
            capability.probe_version = Some(crate::probe::HARNESS_PROBE_VERSION);
            capability.probe_issue = None;
        } else {
            route.model_capabilities.push(ModelCapability {
                model: model.to_string(),
                context_window: route.context_window,
                wire: Some(wire),
                streaming: Some(true),
                reasoning: Some(route.reasoning),
                tool_calling: Some(true),
                probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                ..Default::default()
            });
        }
        persist_inner(&self.data_root, &guard);
        drop(guard);
        if self.proxy_status().running {
            self.activate_proxy_routes();
        }
        true
    }

    pub fn delete_route(&self, route_id: &str) -> bool {
        let mut guard = self.inner.lock().expect("state poisoned");
        let Some(index) = guard.routes.iter().position(|route| route.id == route_id) else {
            return false;
        };
        if guard.routes[index].provider_kind == ProviderKind::Official {
            return false;
        }
        let was_current = guard.routes[index].is_current;
        guard.routes.remove(index);
        guard.overrides.remove(route_id);
        if was_current {
            if let Some(fallback) = guard.routes.iter_mut().find(|route| route.enabled) {
                fallback.is_current = true;
            }
        }
        persist_inner(&self.data_root, &guard);
        true
    }

    /// doc/03：從探測結果建立新路線並設為使用中（建立並啟用）。
    /// 回傳更新後的完整線路清單，前端可直接用來刷新。
    pub fn create_route(
        &self,
        input: CreateRouteInput,
        provider_kind: ProviderKind,
        auth_kind: AuthKind,
    ) -> Vec<Route> {
        let mut guard = self.inner.lock().expect("state poisoned");
        let id = unique_slug(&input.name, &guard.routes);
        // Keep endpoint aliases canonical at the state boundary as well as in
        // the Tauri command. Eval/import callers invoke AppState directly;
        // without this guard an Ollama `/api/generate` URL would later become
        // `/api/generate/v1/...` when the proxy builds the upstream endpoint.
        let base_url = if provider_kind == ProviderKind::OpenAiCompatible {
            crate::probe::normalize_provider_base_url(&input.base_url)
        } else {
            input.base_url.clone()
        };
        let mut available_models = input
            .models
            .clone()
            .filter(|models| !models.is_empty())
            .unwrap_or_else(|| vec![input.model.clone()]);
        available_models.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
        let selected_models = if provider_kind == ProviderKind::Official {
            None
        } else {
            let mut selected = input
                .selected_models
                .clone()
                .unwrap_or_else(|| vec![input.model.clone()]);
            selected.retain(|model| {
                available_models
                    .iter()
                    .any(|available| available.eq_ignore_ascii_case(model))
            });
            selected.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
            if selected.is_empty() {
                selected.push(available_models[0].clone());
            }
            Some(selected)
        };
        let default_model = selected_models
            .as_ref()
            .and_then(|models| {
                models
                    .iter()
                    .find(|model| model.eq_ignore_ascii_case(&input.model))
                    .cloned()
                    .or_else(|| models.first().cloned())
            })
            .unwrap_or_else(|| input.model.clone());
        for r in guard.routes.iter_mut() {
            r.is_current = false;
        }
        let enabled = !vellum_proxy_runtime::outbound::stored_route_fails_admission(
            provider_kind == ProviderKind::Official,
            &base_url,
            !matches!(auth_kind, AuthKind::None),
            crate::model::InsecureHttpPolicy::Deny,
        );
        guard.routes.push(Route {
            id,
            name: input.name,
            base_url,
            models: available_models,
            selected_models,
            model: default_model,
            wire: input.wire,
            is_current: true,
            server_side_resume: input.server_side_resume,
            streaming: input.streaming,
            reasoning: input.reasoning,
            provider_kind,
            auth_kind,
            enabled,
            context_window: input.context_window,
            model_capabilities: input.model_capabilities,
            insecure_http_policy: crate::model::InsecureHttpPolicy::Deny,
            catalog_scope: input.catalog_scope.unwrap_or_default(),
        });
        persist_inner(&self.data_root, &guard);
        drop(guard);
        self.routes()
    }

    pub fn review_settings(&self) -> ReviewSettings {
        let mut settings = { self.inner.lock().expect("state poisoned").review.clone() };
        let routes = self.routes();
        if !settings.route_id.is_empty()
            && !routes.iter().any(|route| route.id == settings.route_id)
        {
            settings.route_id.clear();
        }
        // Migrate settings written before provider selection was explicit.
        if settings.route_id.is_empty() && !settings.model.is_empty() {
            let mut matches = routes.into_iter().filter(|route| {
                route
                    .models
                    .iter()
                    .any(|model| model.eq_ignore_ascii_case(&settings.model))
                    || route.model.eq_ignore_ascii_case(&settings.model)
            });
            if let Some(route) = matches.next().filter(|_| matches.next().is_none()) {
                settings.route_id = route.id;
            }
        }
        settings
    }

    /// Validate then atomically commit new Auto Review settings.
    ///
    /// `Always`/`Failover` must resolve to an enabled review-capable model
    /// (and, for `Failover`, an existing fallback on a different Provider)
    /// *before* anything is written — a half-valid save that only fails at
    /// the next Guardian request would surface as a confusing runtime error
    /// far from the settings screen that caused it. Skipped when none of
    /// `on_edit`/`before_send`/`before_compact` are set: Guardian can never
    /// dispatch in that state, so there is nothing to validate eagerly, and
    /// a user turning the feature off entirely must not be blocked by a
    /// stale or never-configured route/model.
    pub fn set_review_settings(&self, settings: ReviewSettings) -> AppResult<ReviewSettings> {
        if settings.on_edit || settings.before_send || settings.before_compact {
            let paths = crate::codex::CodexPaths::discover(&self.data_root);
            let official_catalog = crate::catalog::read_official_catalog(&paths.models_cache);
            let models = crate::catalog::review_model_routes_with_official_catalog(
                &self.routes(),
                official_catalog.as_ref(),
            );
            crate::review::resolve_guardian_route_plan(&settings, &models)?;
        }
        let mut guard = self.inner.lock().expect("state poisoned");
        guard.review = settings;
        persist_inner(&self.data_root, &guard);
        drop(guard);
        Ok(self.review_settings())
    }

    /// Read-only snapshot of the third-party web search configuration.
    pub fn web_search_settings(&self) -> crate::web_search::WebSearchSettings {
        self.inner
            .lock()
            .expect("state poisoned")
            .web_search
            .clone()
    }

    /// Replace the third-party web search configuration and persist.
    pub fn set_web_search_settings(&self, settings: crate::web_search::WebSearchSettings) {
        let mut guard = self.inner.lock().expect("state poisoned");
        guard.web_search = settings;
        persist_inner(&self.data_root, &guard);
    }

    pub fn subagent_settings(&self) -> SubagentSettings {
        self.inner.lock().expect("state poisoned").subagent.clone()
    }

    /// Validate and persist sub-agent defaults.
    ///
    /// `custom` requires an enabled Provider and a catalog model belonging to
    /// it; an explicit reasoning effort must be verifiably supported by that
    /// model. Stale selections are rejected at save time rather than silently
    /// remapped, and `inherit` never touches Codex's own defaults.
    pub fn set_subagent_settings(&self, settings: SubagentSettings) -> AppResult<()> {
        if settings.mode == SubagentMode::Custom {
            let route_id = settings
                .route_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| AppError::Message("子代理預設需要選擇 Provider".into()))?;
            let catalog_id = settings
                .catalog_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| AppError::Message("子代理預設需要選擇模型".into()))?;
            let routes = self.routes();
            let route = routes
                .iter()
                .find(|route| route.id == route_id)
                .ok_or_else(|| {
                    AppError::Message(format!("Provider `{route_id}` 不存在或已停用"))
                })?;
            if !route.enabled {
                return Err(AppError::Message(format!(
                    "Provider `{}` 已停用，無法作為子代理預設",
                    route.name
                )));
            }
            let model = self
                .model_routes()
                .into_iter()
                .find(|model| model.catalog_id == catalog_id && model.route_id == route_id)
                .ok_or_else(|| {
                    AppError::Message(format!(
                        "模型 `{catalog_id}` 不存在於 Provider `{route_id}` 的可用清單"
                    ))
                })?;
            if let Some(effort) = settings
                .reasoning_effort
                .as_deref()
                .filter(|effort| !effort.is_empty())
            {
                if !model.reasoning_efforts.iter().any(|known| known == effort) {
                    return Err(AppError::Message(format!(
                        "模型 `{}` 未驗證支援 reasoning effort `{effort}`",
                        model.upstream_model
                    )));
                }
            }
        }
        let mut guard = self.inner.lock().expect("state poisoned");
        guard.subagent = settings;
        persist_inner(&self.data_root, &guard);
        Ok(())
    }

    /// Resolve who owns compaction for a route/model pair.
    pub fn resolve_compaction_policy(
        &self,
        route: &Route,
        upstream_model: &str,
        is_review: bool,
    ) -> ResolvedCompactionPolicy {
        self.resolve_compaction_policy_for_session(route, upstream_model, None, is_review)
    }

    /// Resolve with an optional session id.
    ///
    /// There are no stored overrides any more: the compaction strategy is not
    /// a Vellum setting, it follows from the provider's execution plane. The
    /// session id is kept because callers still key telemetry by it.
    pub fn resolve_compaction_policy_for_session(
        &self,
        route: &Route,
        upstream_model: &str,
        _session_id: Option<&str>,
        is_review: bool,
    ) -> ResolvedCompactionPolicy {
        let mut resolved = resolve_route_policy(
            route,
            upstream_model,
            &PolicyResolutionInput {
                route_default: self.eval_route_compaction_policy(&route.id),
                is_review,
            },
        );
        if route.provider_kind == ProviderKind::Official {
            return resolved;
        }
        if let Some(effort) = resolved.resolved_reasoning_effort.clone() {
            let target_route = resolved
                .resolved_compactor_route_id
                .as_deref()
                .unwrap_or(&route.id);
            let target_model = resolved
                .resolved_compactor_model
                .as_deref()
                .unwrap_or(upstream_model);
            let verified = self.model_routes().into_iter().find(|candidate| {
                candidate.route_id == target_route
                    && (candidate.upstream_model.eq_ignore_ascii_case(target_model)
                        || candidate.catalog_id.eq_ignore_ascii_case(target_model))
            });
            let supported = verified
                .as_ref()
                .is_some_and(|candidate| candidate.reasoning_efforts.iter().any(|v| v == &effort));
            if !supported {
                resolved.resolved_reasoning_effort = None;
                resolved.reasoning_effort_source = "automatic_fallback".into();
                resolved.reasoning_effort_fallback_reason = Some(format!(
                    "reasoning effort `{effort}` is not verified for compactor model `{target_model}`"
                ));
            }
        }
        resolved
    }

    pub fn budget_override(&self, route_id: &str) -> Option<u64> {
        self.inner
            .lock()
            .expect("state poisoned")
            .overrides
            .get(route_id)
            .copied()
    }

    pub fn set_budget_override(&self, route_id: &str, tokens: Option<u64>) {
        let mut guard = self.inner.lock().expect("state poisoned");
        match tokens.filter(|t| *t > 0) {
            Some(t) => {
                guard.overrides.insert(route_id.to_string(), t);
            }
            None => {
                guard.overrides.remove(route_id);
            }
        }
        persist_inner(&self.data_root, &guard);
        drop(guard);

        // The context window is part of both the live proxy routing snapshot and
        // the Codex model catalog. Keep the live snapshot in sync immediately;
        // the command layer rewrites the on-disk catalog when the proxy is
        // running so Codex can pick it up after restart.
        if self.proxy_status().running {
            self.activate_proxy_routes();
        }
    }

    /// 額度服務的 Arc 柄。clone 後可搬進 blocking task。
    pub fn quota_service(&self) -> Arc<QuotaService> {
        Arc::clone(&self.quota)
    }

    pub fn grok_accounts(&self) -> Arc<crate::grok_accounts::GrokAccountManager> {
        Arc::clone(&self.grok_accounts)
    }

    pub fn grok_rewrite_error(&self) -> Option<String> {
        self.grok_rewrite_error
            .lock()
            .ok()
            .and_then(|error| error.clone())
    }

    #[cfg(test)]
    pub(crate) fn set_grok_rewrite_error(&self, error: Option<String>) {
        *self
            .grok_rewrite_error
            .lock()
            .expect("grok rewrite error lock poisoned") = error;
    }

    /// One-shot notice from the startup Brave-only web search migration (see
    /// `migrate_web_search_settings`). `get_web_search_settings` surfaces
    /// this so Settings can explain why search was turned off instead of
    /// leaving the user to notice a silently-flipped toggle. Consumed on
    /// first read so it does not nag on every later settings fetch.
    pub fn take_web_search_migration_notice(&self) -> Option<RuntimeNotice> {
        self.web_search_migration_notice
            .lock()
            .ok()
            .and_then(|mut notice| notice.take())
    }

    /// Shared usage connection. Schema migration and SQLite setup are run once
    /// when AppState is created instead of on every renderer poll.
    pub fn usage_store(&self) -> Arc<UsageStore> {
        Arc::clone(&self.usage)
    }

    pub fn history_store(&self) -> Arc<HistoryStore> {
        Arc::clone(&self.history)
    }

    pub fn remote(&self) -> Arc<RemoteClientManager> {
        Arc::clone(&self.remote)
    }

    pub fn findings(&self) -> Vec<Finding> {
        self.inner.lock().expect("state poisoned").findings.clone()
    }

    pub fn set_findings(&self, findings: Vec<Finding>) {
        let mut guard = self.inner.lock().expect("state poisoned");
        guard.findings = findings;
        persist_inner(&self.data_root, &guard);
    }

    pub fn model_routes(&self) -> Vec<ModelRoute> {
        let paths = crate::codex::CodexPaths::discover(&self.data_root);
        let official = crate::catalog::read_official_catalog(&paths.models_cache);
        let routes = self.routes();
        let mut models =
            crate::catalog::model_routes_with_official_catalog(&routes, official.as_ref());
        let overrides = {
            let inner = self.inner.lock().expect("state poisoned");
            inner.overrides.clone()
        };
        for model in &mut models {
            if let Some(tokens) = overrides
                .get(&model.catalog_id)
                .or_else(|| overrides.get(&model.route_id))
                .copied()
                .filter(|tokens| *tokens > 0)
            {
                model.context_window = Some(tokens);
            }
        }
        models
    }

    pub fn review_model_routes(&self) -> Vec<ModelRoute> {
        let paths = crate::codex::CodexPaths::discover(&self.data_root);
        let official = crate::catalog::read_official_catalog(&paths.models_cache);
        crate::catalog::review_model_routes_with_official_catalog(&self.routes(), official.as_ref())
    }

    pub fn route_for_catalog_model(&self, catalog_id: &str) -> Option<(Route, ModelRoute)> {
        let model = self
            .model_routes()
            .into_iter()
            .find(|model| model.catalog_id == catalog_id)?;
        let route = self
            .routes()
            .into_iter()
            .find(|route| route.id == model.route_id)?;
        Some((route, model))
    }

    /// 建立 Proxy 啟動時的不可變路由快照。一般線路的啟用／停用只會修改
    /// 下一次啟動所使用的設定，不會改變正在服務中的 Proxy。
    pub fn activate_proxy_routes(&self) {
        let routes = self
            .routes()
            .into_iter()
            .filter(|route| route.enabled)
            .collect::<Vec<_>>();
        let active_ids = routes
            .iter()
            .map(|route| route.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let models = self
            .model_routes()
            .into_iter()
            .filter(|model| active_ids.contains(model.route_id.as_str()))
            .collect();
        let mut proxy = self.proxy.lock().expect("proxy state poisoned");
        proxy.active_routes = routes;
        proxy.active_model_routes = models;
    }

    pub fn clear_active_proxy_routes(&self) {
        let mut proxy = self.proxy.lock().expect("proxy state poisoned");
        proxy.active_routes.clear();
        proxy.active_model_routes.clear();
    }

    pub fn active_model_routes(&self) -> Vec<ModelRoute> {
        self.proxy
            .lock()
            .expect("proxy state poisoned")
            .active_model_routes
            .clone()
    }

    /// Snapshot of the routes the proxy is currently serving. Used by the
    /// runtime catalog to resolve every active model against one shared
    /// route snapshot instead of re-reading the official catalog per model.
    pub fn active_routes(&self) -> Vec<Route> {
        self.proxy
            .lock()
            .expect("proxy state poisoned")
            .active_routes
            .clone()
    }

    pub fn active_review_model_routes(&self) -> Vec<ModelRoute> {
        let routes = self
            .proxy
            .lock()
            .expect("proxy state poisoned")
            .active_routes
            .clone();
        let paths = crate::codex::CodexPaths::discover(&self.data_root);
        let official = crate::catalog::read_official_catalog(&paths.models_cache);
        crate::catalog::review_model_routes_with_official_catalog(&routes, official.as_ref())
    }

    pub fn refresh_active_route_models(&self, route_id: &str) {
        let configured_route = self
            .routes()
            .into_iter()
            .find(|route| route.id == route_id && route.enabled);
        let configured_models = self
            .model_routes()
            .into_iter()
            .filter(|model| model.route_id == route_id)
            .collect::<Vec<_>>();
        let mut proxy = self.proxy.lock().expect("proxy state poisoned");
        let Some(route) = configured_route else {
            return;
        };
        let Some(active_route) = proxy
            .active_routes
            .iter_mut()
            .find(|active| active.id == route_id)
        else {
            return;
        };
        *active_route = route;
        proxy
            .active_model_routes
            .retain(|model| model.route_id != route_id);
        proxy.active_model_routes.extend(configured_models);
    }

    pub fn replace_route_probe_result(
        &self,
        route_id: &str,
        result: &crate::model::ProbeResult,
    ) -> bool {
        let mut guard = self.inner.lock().expect("state poisoned");
        let Some(route) = guard.routes.iter_mut().find(|route| route.id == route_id) else {
            return false;
        };
        if let Some(wire) = result.wire {
            if route.provider_kind != ProviderKind::GrokCli {
                route.wire = wire;
            }
        }
        let streaming_timed_out = result.model_capabilities.iter().any(|capability| {
            capability.streaming.is_none() && capability.probe_issue.as_deref() == Some("timeout")
        });
        if !streaming_timed_out || result.streaming {
            // A live-run stream-quality observation overrides the header-level
            // probe verdict: only `incremental` may persist `streaming=true`
            // (end-flush/buffered routes project to `streaming=false` so Codex
            // stops expecting incremental replies). `None` keeps the probe's
            // own verdict unchanged.
            route.streaming = match result.stream_quality.as_deref() {
                Some("incremental") => vellum_proxy_runtime::probe_streaming_projection(
                    vellum_proxy_runtime::StreamQuality::Incremental,
                ),
                Some("end_flush") => vellum_proxy_runtime::probe_streaming_projection(
                    vellum_proxy_runtime::StreamQuality::EndFlush,
                ),
                Some("buffered") => vellum_proxy_runtime::probe_streaming_projection(
                    vellum_proxy_runtime::StreamQuality::Buffered,
                ),
                Some(_) => vellum_proxy_runtime::probe_streaming_projection(
                    vellum_proxy_runtime::StreamQuality::NoDelta,
                ),
                None => result.streaming,
            };
        }
        route.reasoning = result.reasoning;
        route.server_side_resume = result.server_side_resume;
        if !result.models.is_empty() {
            route.models = result.models.clone();
            if route.provider_kind != ProviderKind::Official {
                let mut selected = route.selected_models.clone().unwrap_or_default();
                selected.retain(|selected_model| {
                    route
                        .models
                        .iter()
                        .any(|model| model.eq_ignore_ascii_case(selected_model))
                });
                if selected.is_empty() {
                    selected.push(route.models[0].clone());
                }
                if !selected
                    .iter()
                    .any(|model| model.eq_ignore_ascii_case(&route.model))
                {
                    route.model = selected[0].clone();
                }
                route.selected_models = Some(selected);
            }
        }
        route.context_window = result.context_window.or(route.context_window);
        route.model_capabilities = result.model_capabilities.clone();
        persist_inner(&self.data_root, &guard);
        drop(guard);
        if self.proxy_status().running {
            self.activate_proxy_routes();
        }
        true
    }

    /// Merge one Provider-level re-probe round into a route's model
    /// capabilities, instead of `replace_route_probe_result`'s full
    /// overwrite.
    ///
    /// `discovery` is a catalog-only refresh (no live verification -- see
    /// `discover_endpoint_with_client`): it determines which models exist
    /// upstream right now (so a model genuinely removed upstream is dropped,
    /// same as the full-replace path) and supplies an unprobed placeholder
    /// for any model that has never been verified. `verified` is this
    /// round's live results for exactly the targeted (selected ∩ discovered)
    /// models -- a Provider re-probe that verifies 2 of a route's 5 selected
    /// models must not blank out the other 3's, or any unselected model's,
    /// last-known-good capability. A model this round tried to verify and
    /// failed simply keeps whatever it already had (its entry is absent from
    /// `verified`, so the existing route entry -- or the discovery
    /// placeholder if it had none yet -- passes through unchanged).
    ///
    /// User-declared `display_name`/`vision`, and OpenCode's own `free`/
    /// `access_mode` catalog metadata, always carry forward from whatever
    /// the model's prior entry had: `probe_model_capability`'s live-verify
    /// path has no way to know the user's display name or vision choice,
    /// and does not populate OpenCode-specific metadata at all, so a naive
    /// overwrite would silently erase both on every successful re-probe.
    pub fn merge_route_probe_result(
        &self,
        route_id: &str,
        discovery: &crate::model::ProbeResult,
        verified: Vec<ModelCapability>,
    ) -> bool {
        let mut guard = self.inner.lock().expect("state poisoned");
        let Some(route) = guard.routes.iter_mut().find(|route| route.id == route_id) else {
            return false;
        };
        let is_free_only = route.catalog_scope == crate::model::CatalogScope::FreeOnly;
        let mut discovered_models = discovery.models.clone();
        let mut discovered_capabilities = discovery.model_capabilities.clone();
        if is_free_only {
            let on_free_tier = |model: &str| {
                crate::probe::opencode_zen_catalog(&route.base_url)
                    .map(|catalog| crate::probe::opencode_zen_free_tier_model(catalog, model))
                    .unwrap_or_else(|| {
                        vellum_proxy_runtime::opencode::confirmed_free_zen_model(model)
                    })
            };
            discovered_models.retain(|model| on_free_tier(model));
            // A capability's own `free` flag is not enough on its own: a
            // retired model still costs nothing, and reading only the price
            // is what kept `deepseek-v4-flash-free` in the catalog after
            // OpenCode withdrew it.
            discovered_capabilities.retain(|cap| {
                cap.deprecated != Some(true) && (cap.free == Some(true) || on_free_tier(&cap.model))
            });
        }
        if !discovered_models.is_empty() {
            route.models = discovered_models;
            if route.provider_kind != ProviderKind::Official {
                let mut selected = route.selected_models.clone().unwrap_or_default();
                selected.retain(|selected_model| {
                    route
                        .models
                        .iter()
                        .any(|model| model.eq_ignore_ascii_case(selected_model))
                });
                if selected.is_empty() && !route.models.is_empty() {
                    selected.push(route.models[0].clone());
                }
                if !selected
                    .iter()
                    .any(|model| model.eq_ignore_ascii_case(&route.model))
                    && !selected.is_empty()
                {
                    route.model = selected[0].clone();
                }
                route.selected_models = Some(selected);
            }
        }
        route.context_window = discovery.context_window.or(route.context_window);
        let verified_by_model: HashMap<String, ModelCapability> = verified
            .into_iter()
            .map(|capability| (capability.model.to_ascii_lowercase(), capability))
            .collect();
        route.model_capabilities = discovered_capabilities
            .iter()
            .map(|placeholder| {
                let key = placeholder.model.to_ascii_lowercase();
                let existing = route
                    .model_capabilities
                    .iter()
                    .find(|capability| capability.model.eq_ignore_ascii_case(&placeholder.model))
                    .cloned();
                let mut resolved = match (existing.as_ref(), verified_by_model.get(&key)) {
                    (Some(existing), Some(fresh)) => {
                        merge_capability_probe_update(existing, fresh, Some(placeholder))
                    }
                    (None, Some(fresh)) => {
                        merge_capability_probe_update(placeholder, fresh, Some(placeholder))
                    }
                    (Some(existing), None) => existing.clone(),
                    (None, None) => placeholder.clone(),
                };
                // A model verified for the very first time has no `existing`
                // persisted row yet, and the live-verify path never sets
                // `display_name` itself (see the test comment below) -- so
                // without this fallback, a name the discovery placeholder
                // already carried (e.g. OpenCode Zen's catalog-sourced
                // display names) would be silently dropped on exactly the
                // first pass that both discovers and verifies a model.
                let prior_display_name = existing
                    .as_ref()
                    .and_then(|e| e.display_name.clone())
                    .or_else(|| placeholder.display_name.clone());
                let prior_vision = existing.as_ref().and_then(|e| e.vision);
                let prior_free = existing.as_ref().and_then(|e| e.free).or(placeholder.free);
                let prior_access_mode = existing
                    .as_ref()
                    .and_then(|e| e.access_mode)
                    .or(placeholder.access_mode);
                resolved.display_name = prior_display_name.or(resolved.display_name);
                resolved.vision = prior_vision.or(resolved.vision);
                resolved.free = resolved.free.or(prior_free);
                resolved.access_mode = resolved.access_mode.or(prior_access_mode);
                resolved
            })
            .collect();
        persist_inner(&self.data_root, &guard);
        drop(guard);
        if self.proxy_status().running {
            self.activate_proxy_routes();
        }
        true
    }

    pub fn replace_route_model_capability(
        &self,
        route_id: &str,
        capability: ModelCapability,
    ) -> bool {
        let mut guard = self.inner.lock().expect("state poisoned");
        let Some(route) = guard.routes.iter_mut().find(|route| route.id == route_id) else {
            return false;
        };
        if !route
            .models
            .iter()
            .any(|model| model.eq_ignore_ascii_case(&capability.model))
        {
            return false;
        }
        if let Some(existing) = route
            .model_capabilities
            .iter_mut()
            .find(|existing| existing.model.eq_ignore_ascii_case(&capability.model))
        {
            *existing = merge_capability_probe_update(existing, &capability, None);
        } else {
            route.model_capabilities.push(capability);
        }
        persist_inner(&self.data_root, &guard);
        true
    }

    pub fn route_for_active_catalog_model(&self, catalog_id: &str) -> Option<(Route, ModelRoute)> {
        let proxy = self.proxy.lock().expect("proxy state poisoned");
        let model = proxy
            .active_model_routes
            .iter()
            .find(|model| model.catalog_id == catalog_id)?
            .clone();
        let route = proxy
            .active_routes
            .iter()
            .find(|route| route.id == model.route_id)?
            .clone();
        Some((route, model))
    }

    /// Resolve a catalog id against the *configured* routes instead of the
    /// running snapshot.
    ///
    /// Only for diagnosing a failed lookup: it tells "this Provider is waiting
    /// for a proxy restart" apart from "this model does not exist anywhere",
    /// which are the same opaque error otherwise. Never route on this — the
    /// snapshot is what the proxy is actually serving.
    pub fn configured_route_for_catalog_model(
        &self,
        catalog_id: &str,
    ) -> Option<(Route, ModelRoute)> {
        let routes = self.routes();
        let model = crate::catalog::model_routes(&routes)
            .into_iter()
            .find(|model| model.catalog_id == catalog_id)?;
        let route = routes
            .into_iter()
            .find(|route| route.id == model.route_id)?;
        Some((route, model))
    }

    pub fn route_for_active_review_model(&self, catalog_id: &str) -> Option<(Route, ModelRoute)> {
        let model = self
            .active_review_model_routes()
            .into_iter()
            .find(|model| model.catalog_id == catalog_id)?;
        let route = self
            .proxy
            .lock()
            .expect("proxy state poisoned")
            .active_routes
            .iter()
            .find(|route| route.id == model.route_id)?
            .clone();
        Some((route, model))
    }

    pub fn data_root(&self) -> PathBuf {
        self.data_root.clone()
    }

    pub fn codex_oauth(&self) -> Arc<crate::codex_oauth::CodexOAuthManager> {
        Arc::clone(&self.codex_oauth)
    }

    pub fn proxy_status(&self) -> ProxyStatus {
        self.proxy
            .lock()
            .expect("proxy state poisoned")
            .status
            .clone()
    }

    pub fn set_proxy_running(
        &self,
        running: bool,
        catalog_path: Option<String>,
        codex_managed: bool,
        last_error: Option<String>,
    ) {
        {
            let mut proxy = self.proxy.lock().expect("proxy state poisoned");
            proxy.status.running = running;
            proxy.status.catalog_path = catalog_path;
            proxy.status.codex_managed = codex_managed;
            proxy.status.phase = if running {
                crate::model::ProxyPhase::Running
            } else if last_error.is_some() {
                crate::model::ProxyPhase::Failed
            } else {
                crate::model::ProxyPhase::Stopped
            };
            proxy.status.last_error = last_error;
            // 啟動或停止都讓開機時那則「上次沒還原」失效：代理跑起來之後
            // 那份設定就是這次的，停下來之後它已經被還原了。
            proxy.status.notice = None;
        }
        self.publish_proxy_status();
    }

    pub fn lifecycle_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.lifecycle
    }

    pub fn prepare_coordinator(
        &self,
    ) -> &tokio::sync::Mutex<crate::proxy_prepare::PrepareCoordinator> {
        &self.prepare
    }

    pub fn generation_controller(&self) -> &vellum_proxy_runtime::GenerationController {
        &self.generation
    }

    pub fn bump_proxy_generation(&self) -> vellum_proxy_runtime::ProxyGeneration {
        let gen = self.generation.bump();
        {
            let mut proxy = self.proxy.lock().expect("proxy state poisoned");
            proxy.status.generation = gen.id;
            proxy.status.operation_id = Some(format!("op_{}", gen.id));
            proxy.status.stage = None;
            proxy.status.stage_elapsed_ms = None;
        }
        self.publish_proxy_status();
        gen
    }

    pub fn is_generation_current(&self, id: u64) -> bool {
        self.generation.is_current(id)
    }

    pub fn apply_status_if_generation(
        &self,
        generation: u64,
        apply: impl FnOnce(&mut ProxyStatus),
    ) -> bool {
        if !self.generation.is_current(generation) {
            return false;
        }
        {
            let mut proxy = self.proxy.lock().expect("proxy state poisoned");
            if proxy.status.generation != generation {
                return false;
            }
            apply(&mut proxy.status);
        }
        self.publish_proxy_status();
        true
    }

    /// Wire a UI sink after the Tauri app exists. Tests use the watch
    /// subscription and do not need a sink.
    pub fn attach_lifecycle_emitter(&self, sink: impl Fn(ProxyStatus) + Send + Sync + 'static) {
        *self.lifecycle_sink.lock().expect("lifecycle sink poisoned") = Some(Arc::new(sink));
        self.publish_proxy_status();
    }

    pub fn subscribe_proxy_lifecycle(&self) -> tokio::sync::watch::Receiver<ProxyStatus> {
        self.lifecycle_tx.subscribe()
    }

    fn publish_proxy_status(&self) {
        let status = self.proxy_status();
        self.lifecycle_tx.send_replace(status.clone());
        if let Ok(guard) = self.lifecycle_sink.lock() {
            if let Some(sink) = guard.as_ref() {
                sink(status);
            }
        }
    }

    pub fn set_proxy_stage(&self, generation: u64, stage: &str, elapsed_ms: u64) {
        let _ = self.apply_status_if_generation(generation, |status| {
            status.stage = Some(stage.to_string());
            status.stage_elapsed_ms = Some(elapsed_ms);
        });
    }

    pub fn install_proxy_runtime(
        &self,
        runtime: std::sync::Arc<vellum_proxy_runtime::ProxyRuntime>,
    ) {
        self.proxy.lock().expect("proxy state poisoned").runtime = Some(runtime);
    }

    pub fn take_proxy_runtime(&self) -> Option<std::sync::Arc<vellum_proxy_runtime::ProxyRuntime>> {
        self.proxy
            .lock()
            .expect("proxy state poisoned")
            .runtime
            .take()
    }

    /// VACUUM / checkpoint after Stop, never on the button completion path.
    pub fn schedule_stopped_maintenance(&self, generation: u64) {
        let state = self.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let Ok(_gate) = state.lifecycle.clone().try_lock_owned() else {
                return;
            };
            if !state.is_generation_current(generation) || state.proxy_status().running {
                return;
            }
            if state.active_requests() != 0 {
                return;
            }
            let store = state.history_store();
            let _ = tokio::task::spawn_blocking(move || {
                let _gate = _gate;
                crate::history::catch_history_panic(std::panic::AssertUnwindSafe(|| {
                    store.run_stopped_state_maintenance(crate::history::DEFAULT_RETENTION_DAYS)
                }))
            })
            .await;
        });
    }

    pub fn install_proxy_shutdown(&self, sender: tokio::sync::oneshot::Sender<()>) {
        self.proxy.lock().expect("proxy state poisoned").shutdown = Some(sender);
    }

    pub fn install_proxy_server_task(&self, handle: tauri::async_runtime::JoinHandle<()>) {
        let mut proxy = self.proxy.lock().expect("proxy state poisoned");
        // Abort any previous task that was not cleaned up (should be rare).
        if let Some(previous) = proxy.server_task.take() {
            previous.abort();
        }
        proxy.server_task = Some(handle);
    }

    pub fn take_proxy_server_task(&self) -> Option<tauri::async_runtime::JoinHandle<()>> {
        self.proxy
            .lock()
            .expect("proxy state poisoned")
            .server_task
            .take()
    }

    pub fn stop_proxy_signal(&self) {
        if let Some(sender) = self
            .proxy
            .lock()
            .expect("proxy state poisoned")
            .shutdown
            .take()
        {
            let _ = sender.send(());
        }
    }

    /// Cancel the current generation immediately: refuse new work, cancel
    /// in-flight HTTP/WebSocket/stream executions, signal the server, and
    /// abort it if it does not exit within a short bound. Does not wait for
    /// a 30s drain and does not run VACUUM.
    pub async fn stop_proxy_and_wait_idle(&self, timeout: std::time::Duration) -> ProxyStopOutcome {
        self.set_draining(true);
        if let Some(runtime) = self.take_proxy_runtime() {
            let _ = runtime.cancel_all_in_flight_for_stop();
        }
        let was_running = self.proxy_status().running;
        let server_task = self.take_proxy_server_task();
        self.stop_proxy_signal();
        if !was_running && server_task.is_none() {
            return ProxyStopOutcome::Idle;
        }

        let hard_cap = timeout.min(std::time::Duration::from_millis(400));
        let deadline = std::time::Instant::now() + hard_cap;
        let mut server_task_timed_out = false;
        let mut server_task_failed: Option<String> = None;
        if let Some(handle) = server_task {
            while !handle.inner().is_finished() {
                if std::time::Instant::now() >= deadline {
                    log::warn!("[Proxy] aborting server task after immediate stop bound");
                    handle.abort();
                    server_task_timed_out = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            match handle.await {
                Ok(()) => {
                    if !server_task_timed_out {
                        log::info!("[Proxy] server task exited after cancel");
                    }
                }
                Err(error) => {
                    let message = error.to_string();
                    log::warn!("[Proxy] server task join error: {message}");
                    if !server_task_timed_out {
                        server_task_failed = Some(message);
                    }
                }
            }
        }

        let remaining = self.active_requests();
        if server_task_timed_out {
            ProxyStopOutcome::ServerTaskTimedOut {
                active_requests: remaining,
            }
        } else if let Some(message) = server_task_failed {
            ProxyStopOutcome::ServerTaskFailed {
                active_requests: remaining,
                message,
            }
        } else {
            // In-flight guards may still exist until Drop; stop does not wait.
            ProxyStopOutcome::Idle
        }
    }

    pub fn try_begin_request(&self) -> crate::error::AppResult<ActiveRequestGuard> {
        let mut proxy = self.proxy.lock().expect("proxy state poisoned");
        if proxy.draining {
            return Err(crate::error::AppError::Message(
                "Vellum 正在等待現有請求結束，暫不接受新請求".into(),
            ));
        }
        proxy.active_requests += 1;
        Ok(ActiveRequestGuard {
            proxy: Arc::clone(&self.proxy),
        })
    }

    pub fn set_draining(&self, draining: bool) {
        self.proxy.lock().expect("proxy state poisoned").draining = draining;
    }

    pub fn active_requests(&self) -> u64 {
        self.proxy
            .lock()
            .expect("proxy state poisoned")
            .active_requests
    }

    pub fn mark_restart_required(&self, reason: RuntimeNotice) {
        let mut proxy = self.proxy.lock().expect("proxy state poisoned");
        if !proxy.restart_reasons.contains(&reason) {
            log::info!("[Restart] reason added code={}", reason.code);
            proxy.restart_reasons.push(reason);
        }
    }

    /// Records a restart reason together with the Codex instance that still
    /// has the previous configuration loaded. Keeping both writes under one
    /// lock prevents a status poll from observing an unreconcilable reason
    /// with no process identity.
    pub fn mark_restart_required_for_process(
        &self,
        reason: RuntimeNotice,
        process_identity: String,
    ) {
        let mut proxy = self.proxy.lock().expect("proxy state poisoned");
        if !proxy.restart_reasons.contains(&reason) {
            log::info!("[Restart] reason added code={} process_bound=true", reason.code);
            proxy.restart_reasons.push(reason);
        }
        proxy.restart_process_identity = Some(process_identity);
    }

    /// A verified live launch satisfies only the Enhanced adoption notice.
    /// Catalog/config changes still require their own restart acknowledgement.
    pub fn reconcile_enhanced_adoption(&self, adopted: bool) {
        if !adopted {
            return;
        }
        let mut proxy = self.proxy.lock().expect("proxy state poisoned");
        let before = proxy.restart_reasons.len();
        proxy
            .restart_reasons
            .retain(|reason| reason.code != "enhancedDesktopRuntimeChanged");
        if proxy.restart_reasons.len() != before {
            log::info!("[Restart] Enhanced launch adoption verified; cleared runtime change notice");
        }
        if proxy.restart_reasons.is_empty() {
            proxy.restart_process_identity = None;
        }
    }

    pub fn set_restart_process_identity(&self, identity: Option<String>) {
        self.proxy
            .lock()
            .expect("proxy state poisoned")
            .restart_process_identity = identity;
    }

    pub fn reconcile_codex_restart(&self, current_identity: Option<String>) {
        let mut proxy = self.proxy.lock().expect("proxy state poisoned");
        if proxy.restart_reasons.is_empty() {
            proxy.restart_process_identity = None;
            return;
        }
        let Some(previous) = proxy.restart_process_identity.as_deref() else {
            return;
        };
        let Some(current) = current_identity.as_deref() else {
            // Codex is between stop and start. Keep waiting for a new instance.
            return;
        };
        if current != previous {
            proxy.restart_reasons.clear();
            proxy.restart_process_identity = None;
            let notice = RuntimeNotice::new("codexRestartDetected");
            if !proxy.live_applied.contains(&notice) {
                proxy.live_applied.push(notice);
            }
        }
    }

    /// Takes the single automatic repair this session owes `launch_id`.
    ///
    /// Returns true to exactly one caller per launch id. Status is polled, and
    /// a repair takes long enough for several more polls to arrive while it
    /// runs, so "have we already started one" has to be answered and recorded
    /// under the same lock — two concurrent polls both seeing an unrepaired
    /// launch would both stop Codex Desktop.
    ///
    /// One attempt is also the whole budget, not a first try. A repair that
    /// ran and left the drift in place has found something a second identical
    /// restart will not fix, and the failure mode of retrying is restarting
    /// the user's editor on every poll forever. The blocker is still on the
    /// screen; from there it is a person's call.
    pub fn claim_launch_repair(&self, launch_id: &str) -> bool {
        let mut proxy = self.proxy.lock().expect("proxy state poisoned");
        if proxy.repaired_launch_id.as_deref() == Some(launch_id) {
            return false;
        }
        proxy.repaired_launch_id = Some(launch_id.to_string());
        true
    }

    /// Hands the claim back, for a repair that never ran.
    ///
    /// A managed restart that refused because Codex was mid-turn has not spent
    /// anything: nothing was stopped and nothing was rebuilt. Keeping the
    /// claim there would mean one badly-timed poll — one that happened to land
    /// while the user was waiting on a turn — permanently disabled the repair
    /// for that launch.
    pub fn release_launch_repair(&self) {
        self.proxy
            .lock()
            .expect("proxy state poisoned")
            .repaired_launch_id = None;
    }

    pub fn record_live_applied(&self, action: RuntimeNotice) {
        let mut proxy = self.proxy.lock().expect("proxy state poisoned");
        if !proxy.live_applied.contains(&action) {
            proxy.live_applied.push(action);
        }
    }

    pub fn clear_restart_required(&self) {
        let mut proxy = self.proxy.lock().expect("proxy state poisoned");
        proxy.restart_reasons.clear();
        proxy.restart_process_identity = None;
    }

    pub fn set_active_catalog_version(&self, version: Option<String>) {
        self.proxy
            .lock()
            .expect("proxy state poisoned")
            .active_catalog_version = version;
    }

    pub fn runtime_status(&self) -> RuntimeStatus {
        let proxy = self.proxy.lock().expect("proxy state poisoned");
        RuntimeStatus {
            proxy_running: proxy.status.running,
            codex_managed: proxy.status.codex_managed,
            active_requests: proxy.active_requests,
            draining: proxy.draining,
            restart_required: !proxy.restart_reasons.is_empty(),
            restart_reasons: proxy.restart_reasons.clone(),
            live_applied: proxy.live_applied.clone(),
            active_catalog_version: proxy.active_catalog_version.clone(),
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// 內建線路的顯示名稱。品牌名不翻譯，但也不能夾雜某一種語言的修飾語 ——
/// 「OpenAI 官方」在英文與日文介面裡是換不掉的中文。
pub const OFFICIAL_ROUTE_NAME: &str = "OpenAI";

/// 舊版種下的中文名稱是使用者資料，重新種一次不會動到既有安裝。
/// 只在名稱仍是舊的預設值時改寫 —— 使用者自己改過的名字不動。
/// Existing non-compliant third-party routes stay stored. Public HTTP with a
/// credential (and any other admission refusal except DNS) is marked disabled
/// until the owner opts in. Official routes are never gated here.
fn disable_noncompliant_outbound_routes(routes: &mut [Route]) -> bool {
    let mut changed = false;
    for route in routes {
        if !route.enabled {
            continue;
        }
        if vellum_proxy_runtime::outbound::stored_route_fails_admission(
            route.provider_kind == ProviderKind::Official,
            &route.base_url,
            !matches!(route.auth_kind, AuthKind::None),
            route.insecure_http_policy,
        ) {
            route.enabled = false;
            changed = true;
        }
    }
    changed
}

fn migrate_opencode_access_modes(routes: &mut [Route]) -> bool {
    let mut changed = false;
    for route in routes {
        if route.provider_kind != crate::model::ProviderKind::OpenAiCompatible {
            continue;
        }
        let Some(profile) = vellum_proxy_runtime::infer_provider_profile(&route.base_url) else {
            continue;
        };
        for capability in &mut route.model_capabilities {
            if capability.access_mode.is_none() {
                capability.access_mode = Some(
                    match vellum_proxy_runtime::opencode::default_access_mode(
                        profile,
                        &capability.model,
                    ) {
                        vellum_proxy_runtime::RuntimeAccessMode::AnonymousFree => {
                            crate::model::AccessMode::AnonymousFree
                        }
                        vellum_proxy_runtime::RuntimeAccessMode::Credentialed => {
                            crate::model::AccessMode::Credentialed
                        }
                    },
                );
                changed = true;
            }
        }
    }
    changed
}

/// Merge the latest diagnostic result without throwing away a capability that
/// was previously proven usable. Probe status/diagnostics and newly discovered
/// context always advance; user/catalog-owned metadata always survives.
fn merge_capability_probe_update(
    existing: &ModelCapability,
    fresh: &ModelCapability,
    placeholder: Option<&ModelCapability>,
) -> ModelCapability {
    let keep_last_known_good = fresh.last_probe_failed == Some(true)
        && existing.tool_calling == Some(true)
        && existing.wire.is_some();
    let mut merged = if keep_last_known_good {
        existing.clone()
    } else {
        fresh.clone()
    };

    merged.context_window = fresh
        .context_window
        .or_else(|| placeholder.and_then(|value| value.context_window))
        .or(existing.context_window);
    merged.probe_issue = fresh.probe_issue.clone();
    merged.probe_attempts = fresh.probe_attempts.clone();
    merged.last_probe_failed = fresh.last_probe_failed;
    merged.last_probed_at = fresh.last_probed_at;

    merged.display_name = existing
        .display_name
        .clone()
        .or_else(|| placeholder.and_then(|value| value.display_name.clone()))
        .or(merged.display_name);
    merged.vision = existing
        .vision
        .or_else(|| placeholder.and_then(|value| value.vision))
        .or(merged.vision);
    merged.free = fresh
        .free
        .or(existing.free)
        .or_else(|| placeholder.and_then(|value| value.free));
    merged.access_mode = fresh
        .access_mode
        .or(existing.access_mode)
        .or_else(|| placeholder.and_then(|value| value.access_mode));
    // Freshest wins: a model retired since the last probe must be able to
    // become deprecated, so this is not `existing`-first like the others.
    merged.deprecated = fresh
        .deprecated
        .or(existing.deprecated)
        .or_else(|| placeholder.and_then(|value| value.deprecated));
    merged
}

/// Records `freeOnly` on OpenCode routes written before the field existed, and
/// keeps a `freeOnly` route's stored catalog consistent with that scope.
///
/// This runs on *every* settings load, so it must only ever label a route as
/// what it already is. It used to also impose the scope: any keyless OpenCode
/// route was forced to `freeOnly` and its paid models deleted from `models`,
/// `model_capabilities` and `selected_models`. That defeated the documented
/// escape hatch -- adding the provider by hand through the generic Add
/// Provider flow, which offers the full catalog -- because the next load
/// silently reverted the choice and destroyed the discovered models, and
/// re-probing could not restore them (`merge_route_probe_result` filters on
/// the same scope). The user's only way out was to invent an API key.
fn migrate_opencode_catalog_scopes(routes: &mut [Route]) -> bool {
    let mut changed = false;
    for route in routes {
        if route.provider_kind != crate::model::ProviderKind::OpenAiCompatible {
            continue;
        }
        if !crate::probe::is_opencode_zen_endpoint(&route.base_url) {
            continue;
        }
        let catalog = crate::probe::opencode_zen_catalog(&route.base_url);
        let is_free = |model: &str| {
            catalog
                .map(|catalog| crate::probe::opencode_zen_free_tier_model(catalog, model))
                .unwrap_or_else(|| vellum_proxy_runtime::opencode::confirmed_free_zen_model(model))
        };
        // Label, never impose. A keyless route whose catalog is already all
        // free models is a `freeOnly` route that predates the field, and
        // writing the scope down changes nothing the user can see. A keyless
        // route that carries paid models was asked for that way, and is left
        // alone -- those models fail at call time with a clear "requires an
        // API key", which is a better answer than deleting them.
        if route.auth_kind == crate::model::AuthKind::None
            && route.catalog_scope != crate::model::CatalogScope::FreeOnly
            && !route.models.is_empty()
            && route.models.iter().all(|model| is_free(model))
        {
            route.catalog_scope = crate::model::CatalogScope::FreeOnly;
            changed = true;
        }
        if route.catalog_scope == crate::model::CatalogScope::FreeOnly {
            // Pruning must never empty a route. `opencode_zen_known_free`
            // answers `Some(false)` for every OpenCode **Go** model, and the
            // endpoint test above matches Go as well, so a Go route that lost
            // its API key would otherwise have its entire catalog deleted and
            // then be left with nothing to select.
            if !route.models.iter().any(|model| is_free(model)) {
                continue;
            }
            let before = (
                route.models.clone(),
                route.selected_models.clone(),
                route.model.clone(),
                route.model_capabilities.len(),
            );
            route.models.retain(|model| is_free(model));
            route
                .model_capabilities
                .retain(|capability| is_free(&capability.model));
            if let Some(selected) = &mut route.selected_models {
                selected.retain(|model| is_free(model));
                if selected.is_empty() {
                    if let Some(first) = route.models.first() {
                        selected.push(first.clone());
                    }
                }
            }
            let selected = route.selected_models.as_deref().unwrap_or(&route.models);
            if !selected
                .iter()
                .any(|model| model.eq_ignore_ascii_case(&route.model))
            {
                if let Some(first) = selected.first() {
                    route.model = first.clone();
                }
            }
            if before
                != (
                    route.models.clone(),
                    route.selected_models.clone(),
                    route.model.clone(),
                    route.model_capabilities.len(),
                )
            {
                changed = true;
            }
        }
    }
    changed
}

fn migrate_seeded_route_names(routes: &mut [Route]) -> bool {
    let mut changed = false;
    for route in routes {
        if route.id == "openai-official" && route.name == "OpenAI 官方" {
            route.name = OFFICIAL_ROUTE_NAME.into();
            changed = true;
        }
        if route.provider_kind == ProviderKind::GrokCli {
            if route.id == "grok-cli"
                && route.base_url.trim_end_matches('/') == "https://cli-chat-proxy.grok.com"
            {
                route.base_url = "https://cli-chat-proxy.grok.com/v1".into();
                changed = true;
            }
            // Grok Build speaks Responses at `{base}/responses`. An earlier
            // migration rewrote this to Chat, which sent `/chat/completions`
            // and the upstream answered HTTP 426 with an empty Codex turn.
            if route.wire != WireFormat::Responses {
                route.wire = WireFormat::Responses;
                changed = true;
            }
            for capability in &mut route.model_capabilities {
                if capability.wire != Some(WireFormat::Responses) {
                    capability.wire = Some(WireFormat::Responses);
                    changed = true;
                }
            }
        }
    }
    changed
}

/// Brave is now the only web search backend. A settings file written before
/// this change may still carry `enabled: true` from a duckduckgo/searxng
/// configuration; those backend-selection fields no longer exist on
/// `WebSearchSettings` and are simply dropped by serde on load (unknown
/// fields are ignored, not rejected). That collapses migration to one rule
/// regardless of which backend was previously selected: search must not stay
/// enabled without a Brave key actually configured, because there is no
/// other backend left to silently answer the query. A user who was already
/// enabled *and* already has a Brave key keeps working exactly as before —
/// they're just pointed at Brave now instead of whatever they'd picked.
fn migrate_web_search_settings(
    web_search: &mut crate::web_search::WebSearchSettings,
    has_brave_key: bool,
) -> Option<RuntimeNotice> {
    if web_search.enabled && !has_brave_key {
        web_search.enabled = false;
        return Some(RuntimeNotice::new("webSearchDisabledMissingBraveKey"));
    }
    None
}

fn seed_routes() -> Vec<Route> {
    vec![
        Route {
            id: "openai-official".into(),
            name: OFFICIAL_ROUTE_NAME.into(),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            model: "gpt-5.6-sol".into(),
            wire: WireFormat::Responses,
            is_current: true,
            server_side_resume: true,
            streaming: true,
            reasoning: true,
            provider_kind: ProviderKind::Official,
            auth_kind: AuthKind::ChatGpt,
            enabled: true,
            // 實際清單永遠取自 Codex models_cache.json，避免 Vellum
            // 用內建常數覆蓋官方目前可用的模型。
            models: Vec::new(),
            selected_models: None,
            context_window: None,
            model_capabilities: Vec::new(),
            insecure_http_policy: Default::default(),
            catalog_scope: Default::default(),
        },
        Route {
            id: "grok-cli".into(),
            name: "Grok Build".into(),
            base_url: "https://cli-chat-proxy.grok.com/v1".into(),
            model: "grok-4.5".into(),
            wire: WireFormat::Responses,
            is_current: false,
            server_side_resume: false,
            streaming: true,
            reasoning: true,
            provider_kind: ProviderKind::GrokCli,
            auth_kind: AuthKind::GrokSession,
            enabled: true,
            models: vec!["grok-4.5".into()],
            selected_models: Some(vec!["grok-4.5".into()]),
            context_window: Some(500_000),
            model_capabilities: Vec::new(),
            insecure_http_policy: Default::default(),
            catalog_scope: Default::default(),
        },
    ]
}

fn grok_cli_available_for_runtime() -> bool {
    #[cfg(test)]
    {
        true
    }
    #[cfg(not(test))]
    {
        crate::grok_auth::is_cli_installed()
    }
}

fn visible_routes(routes: Vec<Route>, grok_cli_available: bool) -> Vec<Route> {
    let mut visible = routes
        .into_iter()
        .filter(|route| route.provider_kind != ProviderKind::GrokCli || grok_cli_available)
        .collect::<Vec<_>>();
    if visible.iter().any(|route| route.is_current) {
        return visible;
    }
    let fallback = visible
        .iter()
        .position(|route| route.enabled && route.provider_kind == ProviderKind::Official)
        .or_else(|| visible.iter().position(|route| route.enabled));
    if let Some(index) = fallback {
        visible[index].is_current = true;
    }
    visible
}

/// 從名稱產生 slug 當 route id。只保留 ASCII 英數，其餘轉減號。
/// 名稱全空白時用 "route"。與現有 id 重複時加數字後綴。
fn unique_slug(name: &str, existing: &[Route]) -> String {
    let base: String = name
        .trim()
        .to_lowercase()
        .chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() {
                Some(c)
            } else if matches!(c, ' ' | '-' | '_') {
                Some('-')
            } else {
                None
            }
        })
        .collect();
    let base = if base.is_empty() {
        "route".to_string()
    } else {
        base
    };
    let mut candidate = base.clone();
    let mut n = 2;
    while existing.iter().any(|r| r.id == candidate) {
        candidate = format!("{base}-{n}");
        n += 1;
    }
    candidate
}

fn settings_path(root: &std::path::Path) -> PathBuf {
    root.join("settings.json")
}

fn load_inner(root: &std::path::Path) -> Option<Inner> {
    let bytes = std::fs::read(settings_path(root)).ok()?;
    let mut value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            log::warn!("[State] 忽略損壞的設定檔：{error}");
            return None;
        }
    };
    // Migration: `ReviewPolicy::Follow` was removed. A settings file written
    // before this had to have persisted the literal string `"follow"` here,
    // which no longer deserializes -- and since `Inner` deserializes as one
    // JSON blob, that one bad field would otherwise fail the whole file and
    // silently reseed every route/override/finding, not just Guardian
    // policy. Clearing it lets `#[serde(default)]` fall through to `None`,
    // which resolves to `Always` (see `effective_review_policy`).
    if value.pointer("/review/policy").and_then(|v| v.as_str()) == Some("follow") {
        if let Some(review) = value.get_mut("review").and_then(|v| v.as_object_mut()) {
            review.remove("policy");
        }
    }
    match serde_json::from_value(value) {
        Ok(inner) => Some(inner),
        Err(error) => {
            log::warn!("[State] 忽略損壞的設定檔：{error}");
            None
        }
    }
}

fn persist_inner(root: &std::path::Path, inner: &Inner) {
    if let Err(error) = try_persist_inner(root, inner) {
        log::warn!("[State] 設定落盤失敗：{error}");
    }
}

fn try_persist_inner(root: &std::path::Path, inner: &Inner) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|error| error.to_string())?;
    let path = settings_path(root);
    let tmp = root.join("settings.json.tmp");
    let mut stored = inner.clone();
    migrate_seeded_route_names(&mut stored.routes);
    migrate_opencode_access_modes(&mut stored.routes);
    let bytes = serde_json::to_vec_pretty(&stored).map_err(|error| error.to_string())?;
    std::fs::write(&tmp, bytes).map_err(|error| error.to_string())?;
    std::fs::rename(tmp, path).map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_engine_selection_is_eval_only() {
        // Production selects no engine at all, and cannot be pushed onto one:
        // the eval-only setter is dropped outside eval mode. Eval preserves an
        // explicit empty/V1/V2 selection so baselines stay reproducible.
        let production_dir = tempfile::tempdir().unwrap();
        let production = AppState::with_data_dir(production_dir.path().to_path_buf());
        production.set_eval_canonical_engine(Some(
            vellum_proxy_runtime::compaction::CanonicalEngineVersion::V2,
        ));
        assert_eq!(production.eval_canonical_engine(), None);
        assert_eq!(production.runtime_canonical_engine(), None);

        let eval_dir = tempfile::tempdir().unwrap();
        let eval_state = AppState::with_eval_routes(eval_dir.path().to_path_buf(), seed_routes());
        assert_eq!(eval_state.eval_canonical_engine(), None);
        assert_eq!(eval_state.runtime_canonical_engine(), None);
        eval_state.set_eval_canonical_engine(Some(
            vellum_proxy_runtime::compaction::CanonicalEngineVersion::V1,
        ));
        assert_eq!(
            eval_state.eval_canonical_engine(),
            Some(vellum_proxy_runtime::compaction::CanonicalEngineVersion::V1)
        );
        assert_eq!(
            eval_state.runtime_canonical_engine(),
            Some(vellum_proxy_runtime::compaction::CanonicalEngineVersion::V1)
        );
    }

    #[test]
    fn recovery_override_is_eval_only() {
        let production_dir = tempfile::tempdir().unwrap();
        let production = AppState::with_data_dir(production_dir.path().to_path_buf());
        production.set_eval_recovery_enabled(true);
        assert!(!production.eval_recovery_enabled());

        let eval_dir = tempfile::tempdir().unwrap();
        let eval_state = AppState::with_eval_routes(eval_dir.path().to_path_buf(), seed_routes());
        assert!(!eval_state.eval_recovery_enabled());
        eval_state.set_eval_recovery_enabled(true);
        assert!(eval_state.eval_recovery_enabled());
    }

    #[test]
    fn grok_cli_is_locked_to_responses_and_chat_settings_are_migrated() {
        let mut routes = seed_routes();
        let grok = routes.iter().find(|route| route.id == "grok-cli").unwrap();
        assert_eq!(grok.wire, WireFormat::Responses);

        routes
            .iter_mut()
            .find(|route| route.id == "grok-cli")
            .unwrap()
            .wire = WireFormat::Chat;
        routes
            .iter_mut()
            .find(|route| route.id == "grok-cli")
            .unwrap()
            .model_capabilities
            .push(crate::model::ModelCapability {
                model: "grok-4.6".into(),
                wire: Some(WireFormat::Chat),
                ..Default::default()
            });
        assert!(migrate_seeded_route_names(&mut routes));
        let grok = routes.iter().find(|route| route.id == "grok-cli").unwrap();
        assert_eq!(grok.wire, WireFormat::Responses);
        assert_eq!(grok.model_capabilities[0].wire, Some(WireFormat::Responses));
        assert!(!migrate_seeded_route_names(&mut routes));
    }

    #[test]
    fn grok_chat_settings_are_rewritten_and_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut inner = Inner {
            routes: seed_routes(),
            review: ReviewSettings::default(),
            overrides: HashMap::new(),
            findings: Vec::new(),
            web_search: crate::web_search::WebSearchSettings::default(),
            subagent: SubagentSettings::default(),
        };
        inner
            .routes
            .iter_mut()
            .find(|route| route.provider_kind == ProviderKind::GrokCli)
            .unwrap()
            .wire = WireFormat::Chat;
        persist_inner(dir.path(), &inner);
        let reopened = load_inner(dir.path()).unwrap();
        let grok = reopened
            .routes
            .iter()
            .find(|route| route.provider_kind == ProviderKind::GrokCli)
            .unwrap();
        assert_eq!(grok.wire, WireFormat::Responses);
        let state = AppState::with_data_dir(dir.path().to_path_buf());
        let grok = state
            .routes()
            .into_iter()
            .find(|route| route.provider_kind == ProviderKind::GrokCli)
            .unwrap();
        assert_eq!(grok.wire, WireFormat::Responses);
    }

    #[test]
    fn grok_route_is_hidden_when_cli_is_not_installed() {
        let mut routes = seed_routes();
        routes
            .iter_mut()
            .for_each(|route| route.is_current = route.provider_kind == ProviderKind::GrokCli);

        let visible = visible_routes(routes, false);

        assert!(visible
            .iter()
            .all(|route| route.provider_kind != ProviderKind::GrokCli));
        assert_eq!(
            visible
                .iter()
                .find(|route| route.is_current)
                .map(|route| route.id.as_str()),
            Some("openai-official")
        );
    }

    #[test]
    fn settings_survive_reopen_without_touching_user_home() {
        let temp = tempfile::tempdir().unwrap();
        {
            let state = AppState::with_data_dir(temp.path().to_path_buf());
            state.set_budget_override("openai-official", Some(42_000));
            state
                .set_review_settings(ReviewSettings {
                    before_send: false,
                    before_compact: false,
                    ..ReviewSettings::default()
                })
                .unwrap();
        }
        let reopened = AppState::with_data_dir(temp.path().to_path_buf());
        assert_eq!(reopened.budget_override("openai-official"), Some(42_000));
        assert!(!reopened.review_settings().before_send);
    }

    /// Reproduces the Auto Review breakage from a real installation, using that
    /// machine's actual providers: an OpenCode Zen route offering the free
    /// models, and a separate local Qwen route.
    ///
    /// Two distinct faults were stacked in the persisted settings, and the first
    /// one hid the second:
    ///
    /// 1. `route_id` named a provider that no longer exists (`opencode-zen-2`
    ///    after the route had been recreated as `opencode-zen`). Resolution
    ///    failed before any reviewer was contacted, which is why the symptom was
    ///    "Auto Review does nothing" rather than a bad review.
    /// 2. Once the route id is recovered, primary and fallback both land on
    ///    OpenCode Zen — `mimo-v2.5-free` and `nemotron-3.5-lightning-free` are
    ///    the same provider — so Failover is still refused. A fallback on the
    ///    same provider is not a fallback.
    ///
    /// Moving the fallback to the Qwen route is what actually makes it run.
    #[test]
    fn auto_review_recovers_a_renamed_provider_but_still_requires_two_of_them() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        state.create_route(
            CreateRouteInput {
                name: "OpenCode Zen".into(),
                base_url: "https://opencode.invalid/v1".into(),
                model: "big-pickle".into(),
                wire: WireFormat::Responses,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(ProviderKind::OpenAiCompatible),
                api_key: None,
                models: Some(vec![
                    "big-pickle".into(),
                    "mimo-v2.5-free".into(),
                    "nemotron-3.5-lightning-free".into(),
                ]),
                selected_models: Some(vec![
                    "big-pickle".into(),
                    "mimo-v2.5-free".into(),
                    "nemotron-3.5-lightning-free".into(),
                ]),
                context_window: Some(128_000),
                model_capabilities: Vec::new(),
                catalog_scope: None,
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::None,
        );
        state.create_route(
            CreateRouteInput {
                name: "Qwen".into(),
                base_url: "http://127.0.0.1:8062/v1".into(),
                model: "qwen".into(),
                wire: WireFormat::Responses,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(ProviderKind::OpenAiCompatible),
                api_key: None,
                models: Some(vec!["qwen".into()]),
                selected_models: Some(vec!["qwen".into()]),
                context_window: Some(128_000),
                model_capabilities: Vec::new(),
                catalog_scope: None,
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::None,
        );
        let routes = state.routes();
        let opencode = routes
            .iter()
            .find(|route| route.name == "OpenCode Zen")
            .expect("opencode route");
        let qwen = routes
            .iter()
            .find(|route| route.name == "Qwen")
            .expect("qwen route");
        let models = state.model_routes();
        let catalog_of = |upstream: &str| {
            models
                .iter()
                .find(|model| model.upstream_model == upstream)
                .unwrap_or_else(|| panic!("no catalog entry for {upstream}"))
                .catalog_id
                .clone()
        };

        // Fault 1, as persisted on the real machine.
        let stale = ReviewSettings {
            before_send: true,
            route_id: format!("{}-2", opencode.id),
            model: "mimo-v2.5-free".into(),
            policy: Some(ReviewPolicy::Failover),
            fallback_catalog_id: Some(catalog_of("nemotron-3.5-lightning-free")),
            ..ReviewSettings::default()
        };
        {
            let mut guard = state.inner.lock().expect("state poisoned");
            guard.review = stale.clone();
        }
        let recovered = state.review_settings();
        assert_eq!(
            recovered.route_id, opencode.id,
            "a renamed provider must be recovered from its unambiguous model"
        );

        // Fault 2 is only visible once fault 1 is out of the way.
        let same_provider = crate::review::resolve_guardian_route_plan(&recovered, &models)
            .expect_err("a fallback on the primary's own provider is not a fallback");
        assert!(
            same_provider.to_string().contains("不同 Provider"),
            "the error must name the real cause: {same_provider}"
        );

        // OpenCode primary, Qwen fallback: the configuration that actually runs.
        let working = ReviewSettings {
            fallback_catalog_id: Some(catalog_of("qwen")),
            ..recovered
        };
        let plan = crate::review::resolve_guardian_route_plan(&working, &models).unwrap();
        assert_eq!(plan.primary_catalog_id, catalog_of("mimo-v2.5-free"));
        assert_eq!(plan.fallback_catalog_id, Some(catalog_of("qwen")));
        let plan_routes = |catalog: &str| {
            models
                .iter()
                .find(|model| model.catalog_id == catalog)
                .map(|model| model.route_id.clone())
                .expect("planned catalog id must belong to a route")
        };
        assert_eq!(plan_routes(&plan.primary_catalog_id), opencode.id);
        assert_eq!(
            plan_routes(plan.fallback_catalog_id.as_deref().unwrap()),
            qwen.id
        );
    }

    /// `Always`/`Failover` must resolve to an enabled review-capable model
    /// *before* anything is written; an unresolvable selection is rejected
    /// rather than persisted and only failing later at the next Guardian
    /// request.
    #[test]
    fn review_settings_always_rejects_a_route_that_does_not_resolve() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let error = state
            .set_review_settings(ReviewSettings {
                before_send: true,
                route_id: "no-such-route".into(),
                model: "no-such-model".into(),
                policy: Some(ReviewPolicy::Always),
                fallback_catalog_id: None,
                ..ReviewSettings::default()
            })
            .unwrap_err();
        assert!(format!("{error}").contains("找不到可用的主要模型"));
        // The rejected save must not have touched the persisted settings.
        assert_eq!(state.review_settings().policy, None);
    }

    /// `Failover` additionally requires an existing fallback on a *different*
    /// Provider — the same invariant the runtime enforces per-request
    /// (`resolve_guardian_route_plan`), just checked eagerly at save time.
    #[test]
    fn review_settings_failover_rejects_a_same_provider_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        state.create_route(
            CreateRouteInput {
                name: "Reviewer Provider".into(),
                base_url: "http://reviewer.test/v1".into(),
                model: "review-model-a".into(),
                wire: WireFormat::Chat,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(ProviderKind::OpenAiCompatible),
                api_key: None,
                models: Some(vec!["review-model-a".into(), "review-model-b".into()]),
                selected_models: Some(vec!["review-model-a".into(), "review-model-b".into()]),
                context_window: Some(128_000),
                model_capabilities: Vec::new(),
                catalog_scope: None,
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::None,
        );
        let route_id = state
            .routes()
            .into_iter()
            .find(|route| route.name == "Reviewer Provider")
            .unwrap()
            .id;
        let models = state.model_routes();
        let primary = models
            .iter()
            .find(|model| model.upstream_model == "review-model-a")
            .unwrap();
        let same_provider_fallback = models
            .iter()
            .find(|model| model.upstream_model == "review-model-b")
            .unwrap();
        let error = state
            .set_review_settings(ReviewSettings {
                before_send: true,
                route_id: route_id.clone(),
                model: primary.catalog_id.clone(),
                policy: Some(ReviewPolicy::Failover),
                fallback_catalog_id: Some(same_provider_fallback.catalog_id.clone()),
                ..ReviewSettings::default()
            })
            .unwrap_err();
        assert!(format!("{error}").contains("不同 Provider"));
    }

    #[test]
    fn subagent_settings_default_to_inherit_and_persist_across_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let defaults = state.subagent_settings();
        assert_eq!(defaults.mode, SubagentMode::Inherit);
        assert_eq!(defaults.route_id, None);
        assert_eq!(defaults.catalog_id, None);
        assert_eq!(defaults.reasoning_effort, None);

        let official = state
            .model_routes()
            .into_iter()
            .find(|model| model.route_id == "openai-official")
            .expect("seeded official model");
        let custom = SubagentSettings {
            mode: SubagentMode::Custom,
            route_id: Some(official.route_id.clone()),
            catalog_id: Some(official.catalog_id.clone()),
            reasoning_effort: None,
        };
        state.set_subagent_settings(custom.clone()).unwrap();
        assert_eq!(state.subagent_settings(), custom);

        let reopened = AppState::with_data_dir(temp.path().to_path_buf());
        assert_eq!(reopened.subagent_settings(), custom);
    }

    #[test]
    fn subagent_custom_rejects_unknown_disabled_or_unsupported_selections() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let official = state
            .model_routes()
            .into_iter()
            .find(|model| model.route_id == "openai-official")
            .expect("seeded official model");

        // Unknown Provider.
        let error = state
            .set_subagent_settings(SubagentSettings {
                mode: SubagentMode::Custom,
                route_id: Some("missing-provider".into()),
                catalog_id: Some(official.catalog_id.clone()),
                reasoning_effort: None,
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("不存在或已停用"), "{error}");

        // A catalog model that does not belong to the selected Provider.
        let error = state
            .set_subagent_settings(SubagentSettings {
                mode: SubagentMode::Custom,
                route_id: Some(official.route_id.clone()),
                catalog_id: Some("grok-cli:grok-4.5".into()),
                reasoning_effort: None,
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("不存在於 Provider"), "{error}");

        // Disabled Provider.
        assert!(state.set_route_enabled("openai-official", false));
        let error = state
            .set_subagent_settings(SubagentSettings {
                mode: SubagentMode::Custom,
                route_id: Some(official.route_id.clone()),
                catalog_id: Some(official.catalog_id.clone()),
                reasoning_effort: None,
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("已停用"), "{error}");
        assert!(state.set_route_enabled("openai-official", true));

        // An effort the model has not verified.
        let error = state
            .set_subagent_settings(SubagentSettings {
                mode: SubagentMode::Custom,
                route_id: Some(official.route_id.clone()),
                catalog_id: Some(official.catalog_id.clone()),
                reasoning_effort: Some("definitely-not-supported".into()),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("未驗證支援"), "{error}");
    }

    #[test]
    fn subagent_settings_migrate_to_inherit_from_old_settings_file() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("settings.json"),
            serde_json::json!({
                "routes": [],
                "review": {
                    "onEdit": false,
                    "beforeSend": true,
                    "beforeCompact": true,
                    "routeId": "",
                    "model": ""
                }
            })
            .to_string(),
        )
        .unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        assert_eq!(state.subagent_settings().mode, SubagentMode::Inherit);
        assert_eq!(state.subagent_settings().catalog_id, None);
        assert_eq!(state.subagent_settings().reasoning_effort, None);
    }

    /// Legacy shape from before Brave-only consolidation: an explicit
    /// `backends`/`searxngUrl` selection. Those fields no longer exist on
    /// `WebSearchSettings` and are simply dropped by serde on load (unknown
    /// fields are ignored, not rejected) — this is exactly what a real
    /// pre-consolidation `settings.json` looks like on disk.
    fn legacy_web_search_settings_json(enabled: bool) -> serde_json::Value {
        serde_json::json!({
            "routes": [],
            // `Inner`'s field is `web_search` (no rename_all on `Inner` itself);
            // only the nested `WebSearchSettings` fields are camelCase.
            "web_search": {
                "enabled": enabled,
                "mode": "live",
                "backends": ["duckduckgo"],
                "searxngUrl": null,
                "domainPolicy": { "allow": [], "block": [] },
                "searchContextSize": "medium"
            }
        })
    }

    #[test]
    fn startup_migration_disables_web_search_when_no_brave_key_is_configured() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("settings.json"),
            legacy_web_search_settings_json(true).to_string(),
        )
        .unwrap();
        // No Brave credential saved: the legacy duckduckgo selection cannot
        // be normalized to a working Brave configuration, so search must be
        // auto-disabled rather than left silently pointed at a backend that
        // no longer exists.
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        assert!(!state.web_search_settings().enabled);
        let notice = state
            .take_web_search_migration_notice()
            .expect("auto-disabling search must produce an explanatory notice");
        assert_eq!(notice.code, "webSearchDisabledMissingBraveKey");
        // The notice is one-shot: a second read must not repeat it.
        assert!(state.take_web_search_migration_notice().is_none());
        // The correction is persisted, not just held in memory, so a restart
        // does not resurrect the disabled-but-still-enabled configuration.
        let reopened = AppState::with_data_dir(temp.path().to_path_buf());
        assert!(!reopened.web_search_settings().enabled);
    }

    #[test]
    fn startup_migration_keeps_web_search_enabled_when_a_brave_key_is_already_configured() {
        let temp = tempfile::tempdir().unwrap();
        crate::credentials::save(
            temp.path(),
            crate::web_search::BRAVE_SEARCH_CREDENTIAL_ID,
            "brave-test-key",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("settings.json"),
            legacy_web_search_settings_json(true).to_string(),
        )
        .unwrap();
        // A usable Brave key is already on disk: the legacy duckduckgo
        // selection is simply repointed at Brave, search stays on, and there
        // is nothing to explain to the user.
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        assert!(state.web_search_settings().enabled);
        assert!(state.take_web_search_migration_notice().is_none());
    }

    #[test]
    fn startup_migration_does_not_touch_already_disabled_search() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("settings.json"),
            legacy_web_search_settings_json(false).to_string(),
        )
        .unwrap();
        // Search left disabled must never be blocked or flagged by a missing
        // key: this must not affect overall proxy readiness/startup.
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        assert!(!state.web_search_settings().enabled);
        assert!(state.take_web_search_migration_notice().is_none());
    }

    #[test]
    fn disabling_current_route_selects_an_enabled_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        assert!(state.set_route_enabled("openai-official", false));
        assert_eq!(state.current_route().unwrap().id, "grok-cli");
    }

    #[test]
    fn official_route_cannot_be_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        assert!(!state.delete_route("openai-official"));
        assert!(state
            .routes()
            .iter()
            .any(|route| route.id == "openai-official"));
    }

    #[test]
    fn graceful_drain_rejects_new_requests_and_guard_releases_count() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let guard = state.try_begin_request().unwrap();
        assert_eq!(state.active_requests(), 1);
        drop(guard);
        assert_eq!(state.active_requests(), 0);
        state.set_draining(true);
        assert!(state.try_begin_request().is_err());
    }

    #[tokio::test]
    async fn stop_proxy_and_wait_idle_drains_then_clears_server_task() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        state.set_proxy_running(true, None, true, None);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        state.install_proxy_shutdown(shutdown_tx);
        let handle = tauri::async_runtime::spawn(async move {
            let _ = shutdown_rx.await;
            // Simulate a short handler wind-down after axum graceful shutdown.
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        });
        state.install_proxy_server_task(handle);

        let guard = state.try_begin_request().unwrap();
        let state_clone = state.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            drop(guard);
            // After drop, idle wait should proceed.
            let _ = state_clone;
        });

        let started = std::time::Instant::now();
        let outcome = state
            .stop_proxy_and_wait_idle(std::time::Duration::from_secs(2))
            .await;
        assert_eq!(outcome, ProxyStopOutcome::Idle);
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        assert!(state.take_proxy_server_task().is_none());
        // Draining remains true until the caller finishes restore.
        assert!(state.try_begin_request().is_err());
        state.set_draining(false);
    }

    #[tokio::test]
    async fn stop_proxy_timeout_is_fail_closed_for_maintenance() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        state.set_proxy_running(true, None, true, None);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        state.install_proxy_shutdown(shutdown_tx);
        let handle = tauri::async_runtime::spawn(async move {
            let _ = shutdown_rx.await;
        });
        state.install_proxy_server_task(handle);

        // Hold a request guard past a very short deadline so wait cannot claim Idle.
        let guard = state.try_begin_request().unwrap();
        assert_eq!(state.active_requests(), 1);

        let before = state.history_store().storage_telemetry().unwrap();
        let started = std::time::Instant::now();
        let outcome = state
            .stop_proxy_and_wait_idle(std::time::Duration::from_millis(80))
            .await;
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        // Immediate stop does not wait for the held guard and does not VACUUM.
        assert!(matches!(
            outcome,
            ProxyStopOutcome::Idle
                | ProxyStopOutcome::TimedOut { .. }
                | ProxyStopOutcome::ServerTaskTimedOut { .. }
                | ProxyStopOutcome::ServerTaskFailed { .. }
        ));
        let after = state.history_store().storage_telemetry().unwrap();
        assert_eq!(before.last_maintenance_at, after.last_maintenance_at);
        assert_eq!(before.last_vacuum_at, after.last_vacuum_at);

        drop(guard);
        state.set_draining(false);
    }

    #[tokio::test]
    async fn stop_proxy_graceful_path_runs_maintenance_only_when_idle() {
        use crate::history::DEFAULT_RETENTION_DAYS;

        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        // Idle stop with no server task → Idle → maintenance allowed.
        let outcome = state
            .stop_proxy_and_wait_idle(std::time::Duration::from_millis(200))
            .await;
        assert_eq!(outcome, ProxyStopOutcome::Idle);
        let report = state
            .history_store()
            .run_stopped_state_maintenance(DEFAULT_RETENTION_DAYS)
            .unwrap();
        assert!(report.telemetry.last_maintenance_at.is_some());
        state.set_draining(false);
    }

    #[test]
    fn runtime_status_separates_live_changes_from_restart_required_changes() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        state.record_live_applied(RuntimeNotice::new("routeHotSwapped"));
        state.mark_restart_required(RuntimeNotice::new("catalogUpdated"));

        let status = state.runtime_status();
        assert!(status.restart_required);
        assert_eq!(status.live_applied, [RuntimeNotice::new("routeHotSwapped")]);
        assert_eq!(
            status.restart_reasons,
            [RuntimeNotice::new("catalogUpdated")]
        );

        state.clear_restart_required();
        let status = state.runtime_status();
        assert!(!status.restart_required);
        assert_eq!(status.live_applied, [RuntimeNotice::new("routeHotSwapped")]);
        assert!(status.restart_reasons.is_empty());
    }

    /// The claim is what keeps an automatic Codex Desktop restart from being
    /// driven by a polling loop. Status is read every few seconds, and a
    /// restart outlives several of those reads, so the second poll must not be
    /// able to stop the app again while the first repair is still running --
    /// nor may a repair that ran and failed be retried until the user gives up
    /// on the editor. Only a repair that never happened hands the claim back.
    #[test]
    fn a_superseded_launch_gets_one_automatic_repair_and_not_one_per_poll() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());

        assert!(state.claim_launch_repair("01LAUNCH"));
        assert!(!state.claim_launch_repair("01LAUNCH"));
        assert!(!state.claim_launch_repair("01LAUNCH"));

        // A restart that Codex refused mid-turn spent nothing, so the same
        // launch is repairable again once the turn ends.
        state.release_launch_repair();
        assert!(state.claim_launch_repair("01LAUNCH"));

        // A repaired launch is replaced by a new one, and Desktop can update
        // itself again under that. Each launch carries its own attempt.
        assert!(state.claim_launch_repair("02LAUNCH"));
        assert!(!state.claim_launch_repair("02LAUNCH"));
    }

    #[test]
    fn restart_requirement_clears_only_after_a_new_codex_process_instance() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        state.mark_restart_required_for_process(
            RuntimeNotice::new("catalogUpdated"),
            "100:1000".into(),
        );

        state.reconcile_codex_restart(Some("100:1000".into()));
        assert!(state.runtime_status().restart_required);

        state.reconcile_codex_restart(None);
        assert!(state.runtime_status().restart_required);

        state.reconcile_codex_restart(Some("200:2000".into()));
        let status = state.runtime_status();
        assert!(!status.restart_required);
        assert!(status
            .live_applied
            .iter()
            .any(|notice| notice.code == "codexRestartDetected"));
    }

    #[test]
    fn enhanced_adoption_clears_delayed_start_notice_without_another_restart() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        state.mark_restart_required_for_process(
            RuntimeNotice::new("enhancedDesktopRuntimeChanged"),
            "100:1000".into(),
        );
        state.reconcile_enhanced_adoption(false);
        assert!(state.runtime_status().restart_required);
        state.reconcile_enhanced_adoption(true);
        assert!(!state.runtime_status().restart_required);
        state.reconcile_codex_restart(Some("100:1000".into()));
        assert!(!state.runtime_status().restart_required);
    }

    #[test]
    fn enhanced_adoption_preserves_catalog_restart_and_process_binding() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        for code in ["enhancedDesktopRuntimeChanged", "catalogUpdated"] {
            state.mark_restart_required_for_process(RuntimeNotice::new(code), "100:1000".into());
        }
        state.reconcile_enhanced_adoption(true);
        assert_eq!(
            state.runtime_status().restart_reasons,
            vec![RuntimeNotice::new("catalogUpdated")]
        );
        state.reconcile_codex_restart(Some("200:2000".into()));
        assert!(!state.runtime_status().restart_required);
    }

    #[test]
    fn route_enable_changes_only_apply_to_the_next_proxy_activation() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        state.activate_proxy_routes();
        assert!(state
            .active_model_routes()
            .iter()
            .any(|model| model.route_id == "grok-cli"));

        assert!(state.set_route_enabled("grok-cli", false));
        assert!(!state.select_route("grok-cli"));
        assert!(state
            .active_model_routes()
            .iter()
            .any(|model| model.route_id == "grok-cli"));

        state.activate_proxy_routes();
        assert!(!state
            .active_model_routes()
            .iter()
            .any(|model| model.route_id == "grok-cli"));
    }

    #[test]
    fn selected_provider_models_persist_and_hot_refresh_active_routing() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        {
            let mut inner = state.inner.lock().unwrap();
            let route = inner
                .routes
                .iter_mut()
                .find(|route| route.id == "grok-cli")
                .unwrap();
            route.models = vec!["grok-4.5".into(), "grok-fast".into()];
            route.selected_models = None;
        }
        state.activate_proxy_routes();
        assert!(state.set_route_models("grok-cli", vec!["grok-fast".into()]));
        state.refresh_active_route_models("grok-cli");

        let active = state
            .active_model_routes()
            .into_iter()
            .filter(|model| model.route_id == "grok-cli")
            .collect::<Vec<_>>();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].upstream_model, "grok-fast");

        let reopened = AppState::with_data_dir(temp.path().to_path_buf());
        let route = reopened
            .routes()
            .into_iter()
            .find(|route| route.id == "grok-cli")
            .unwrap();
        assert_eq!(route.selected_models, Some(vec!["grok-fast".into()]));
        assert_eq!(route.model, "grok-fast");
    }

    #[test]
    fn grok_catalog_refresh_adds_cli_default_without_dropping_existing_selection() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        assert!(state.replace_grok_model_catalog(
            "grok-cli",
            vec!["grok-4.6".into(), "grok-4.5".into()],
            Some("grok-4.6"),
        ));
        let route = state
            .routes()
            .into_iter()
            .find(|route| route.id == "grok-cli")
            .unwrap();
        assert_eq!(route.models, vec!["grok-4.6", "grok-4.5"]);
        assert_eq!(route.model, "grok-4.6");
        assert_eq!(
            route.selected_models,
            Some(vec!["grok-4.5".into(), "grok-4.6".into()])
        );
    }

    /// A capability edit can make a model routable that was not routable when
    /// the proxy started, because `model_routes` filters out models whose Codex
    /// tool protocol did not verify. Publishing the new catalog without
    /// refreshing the proxy's snapshot let Codex request a model the proxy could
    /// not resolve — HTTP 422 `找不到線路 <catalog id>`. Anything that mutates
    /// capabilities and republishes the catalog has to do both.
    #[test]
    fn a_capability_change_is_not_routable_until_the_active_snapshot_refreshes() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        {
            let mut inner = state.inner.lock().unwrap();
            inner.routes.push(Route {
                id: "local".into(),
                name: "local".into(),
                base_url: "http://host.test/v1".into(),
                model: "qwen-local".into(),
                wire: WireFormat::Chat,
                is_current: false,
                server_side_resume: false,
                streaming: true,
                reasoning: true,
                provider_kind: ProviderKind::OpenAiCompatible,
                auth_kind: AuthKind::None,
                enabled: true,
                models: vec!["qwen-local".into()],
                selected_models: None,
                context_window: Some(128_000),
                model_capabilities: vec![ModelCapability {
                    model: "qwen-local".into(),
                    wire: Some(WireFormat::Chat),
                    tool_calling: Some(false),
                    probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                    ..Default::default()
                }],
                insecure_http_policy: Default::default(),
                catalog_scope: Default::default(),
            });
        }
        state.activate_proxy_routes();

        let catalog_id = crate::catalog::stable_catalog_id("local", "qwen-local");
        assert!(
            state.route_for_active_catalog_model(&catalog_id).is_none(),
            "a model whose tool protocol failed must not be routable"
        );

        // The re-probe now verifies the tool protocol.
        {
            let mut inner = state.inner.lock().unwrap();
            let route = inner
                .routes
                .iter_mut()
                .find(|route| route.id == "local")
                .unwrap();
            route.model_capabilities[0].tool_calling = Some(true);
        }

        // Catalog generation already sees it, so Codex would be told about it...
        assert!(crate::catalog::model_routes(&state.routes())
            .iter()
            .any(|model| model.catalog_id == catalog_id));
        // ...while the proxy still cannot route it.
        assert!(
            state.route_for_active_catalog_model(&catalog_id).is_none(),
            "the snapshot is stale until it is refreshed"
        );

        state.refresh_active_route_models("local");
        assert!(
            state.route_for_active_catalog_model(&catalog_id).is_some(),
            "refreshing the snapshot must make the model routable"
        );
    }

    /// Adding, enabling or deleting a Provider is deferred to the next proxy
    /// activation on purpose, but the catalog is published immediately — so
    /// Codex can ask for a model the running proxy does not serve yet. That has
    /// to read as "restart the proxy", not as an unknown catalog hash.
    #[test]
    fn a_provider_awaiting_activation_is_distinguishable_from_an_unknown_model() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        state.activate_proxy_routes();

        state.create_route(
            CreateRouteInput {
                name: "Local".into(),
                base_url: "http://host.test/v1".into(),
                model: "qwen-local".into(),
                wire: WireFormat::Chat,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(ProviderKind::OpenAiCompatible),
                api_key: None,
                models: Some(vec!["qwen-local".into()]),
                selected_models: Some(vec!["qwen-local".into()]),
                context_window: Some(128_000),
                model_capabilities: vec![ModelCapability {
                    model: "qwen-local".into(),
                    wire: Some(WireFormat::Chat),
                    tool_calling: Some(true),
                    probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                    ..Default::default()
                }],
                catalog_scope: None,
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::None,
        );
        let route_id = state
            .routes()
            .into_iter()
            .find(|route| route.name == "Local")
            .unwrap()
            .id;
        let catalog_id = crate::catalog::stable_catalog_id(&route_id, "qwen-local");

        // The proxy has not been restarted, so it cannot route this yet...
        assert!(state.route_for_active_catalog_model(&catalog_id).is_none());
        // ...but it is configured, which is what makes the message actionable.
        let (route, model) = state
            .configured_route_for_catalog_model(&catalog_id)
            .expect("a configured provider must be recognisable while pending");
        assert_eq!(route.name, "Local");
        assert_eq!(model.upstream_model, "qwen-local");

        // A genuinely unknown id stays unknown.
        assert!(state
            .configured_route_for_catalog_model("vlm-0000000000-nope")
            .is_none());

        // After activation it routes normally.
        state.activate_proxy_routes();
        assert!(state.route_for_active_catalog_model(&catalog_id).is_some());
    }

    #[test]
    fn direct_route_creation_normalizes_an_ollama_native_endpoint() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let routes = state.create_route(
            CreateRouteInput {
                name: "Local Ollama".into(),
                base_url: "http://host.test:11434/api/generate".into(),
                model: "qwen-local".into(),
                wire: WireFormat::Chat,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(ProviderKind::OpenAiCompatible),
                api_key: None,
                models: Some(vec!["qwen-local".into()]),
                selected_models: Some(vec!["qwen-local".into()]),
                context_window: Some(131_072),
                model_capabilities: Vec::new(),
                catalog_scope: None,
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::None,
        );
        let route = routes
            .iter()
            .find(|route| route.name == "Local Ollama")
            .unwrap();
        assert_eq!(route.base_url, "http://host.test:11434/v1");
    }

    #[test]
    fn renaming_a_route_keeps_id_catalog_credential_and_realm_identity() {
        // The existing `cc` route must keep its immutable internal id when the
        // display name becomes "Ollama API (e806)": catalog ids, credential
        // lookup and the continuation realm are all derived from the id /
        // base_url, never from the display name.
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let routes = state.create_route(
            CreateRouteInput {
                name: "cc".into(),
                base_url: "https://api.provider.example/v1".into(),
                model: "qwen3.6".into(),
                wire: WireFormat::Chat,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(ProviderKind::OpenAiCompatible),
                api_key: None,
                models: Some(vec!["qwen3.6".into()]),
                selected_models: Some(vec!["qwen3.6".into()]),
                context_window: Some(131_072),
                model_capabilities: Vec::new(),
                catalog_scope: None,
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::None,
        );
        let before = routes
            .iter()
            .find(|route| route.name == "cc")
            .cloned()
            .expect("created route");
        let catalog_id_before = crate::catalog::stable_catalog_id(&before.id, "qwen3.6");
        let realm_before = crate::continuation::compute_route_realm_fingerprint(&before, None);

        assert!(state.set_route_name(&before.id, "Ollama API (e806)"));

        let after = state
            .routes()
            .into_iter()
            .find(|route| route.id == before.id)
            .expect("rename must not move the route");
        assert_eq!(after.id, before.id);
        assert_eq!(after.name, "Ollama API (e806)");
        assert_eq!(
            crate::catalog::stable_catalog_id(&after.id, "qwen3.6"),
            catalog_id_before
        );
        assert_eq!(
            crate::continuation::compute_route_realm_fingerprint(&after, None),
            realm_before
        );
    }

    #[test]
    fn reprobe_reconciles_a_changed_vllm_deployment_and_active_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let routes = state.create_route(
            CreateRouteInput {
                name: "vLLM".into(),
                base_url: "https://example.test/v1".into(),
                model: "old/model".into(),
                wire: WireFormat::Responses,
                streaming: true,
                reasoning: false,
                server_side_resume: false,
                provider_kind: Some(ProviderKind::OpenAiCompatible),
                api_key: None,
                models: Some(vec!["old/model".into()]),
                selected_models: Some(vec!["old/model".into()]),
                context_window: None,
                model_capabilities: Vec::new(),
                catalog_scope: None,
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::None,
        );
        let route_id = routes
            .iter()
            .find(|route| route.name == "vLLM")
            .unwrap()
            .id
            .clone();
        state.set_proxy_running(true, None, true, None);
        state.activate_proxy_routes();
        let old_catalog = crate::catalog::stable_catalog_id(&route_id, "old/model");
        assert!(state.route_for_active_catalog_model(&old_catalog).is_some());

        state.replace_route_probe_result(
            &route_id,
            &crate::model::ProbeResult {
                reachable: true,
                wire: Some(WireFormat::Responses),
                models: vec!["new/model".into()],
                context_window: Some(131_072),
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                model_capabilities: vec![ModelCapability {
                    model: "new/model".into(),
                    context_window: Some(131_072),
                    wire: Some(WireFormat::Responses),
                    streaming: Some(true),
                    reasoning: Some(true),
                    tool_calling: Some(true),
                    probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                    ..Default::default()
                }],
                stream_quality: None,
                needs_input: Vec::new(),
            },
        );

        let route = state
            .routes()
            .into_iter()
            .find(|route| route.id == route_id)
            .unwrap();
        assert_eq!(route.model, "new/model");
        assert_eq!(route.selected_models, Some(vec!["new/model".into()]));
        assert!(state.route_for_active_catalog_model(&old_catalog).is_none());
        let new_catalog = crate::catalog::stable_catalog_id(&route.id, "new/model");
        assert!(state.route_for_active_catalog_model(&new_catalog).is_some());
    }

    #[test]
    fn focused_model_probe_updates_only_that_model_and_persists() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let routes = state.create_route(
            CreateRouteInput {
                name: "OpenCode Zen".into(),
                base_url: crate::probe::OPENCODE_ZEN_BASE_URL.into(),
                model: "big-pickle".into(),
                wire: WireFormat::Chat,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(ProviderKind::OpenAiCompatible),
                api_key: None,
                models: Some(vec!["big-pickle".into(), "gpt-5.4-mini".into()]),
                selected_models: Some(vec!["big-pickle".into()]),
                context_window: None,
                model_capabilities: vec![
                    ModelCapability {
                        model: "big-pickle".into(),
                        wire: Some(WireFormat::Chat),
                        tool_calling: Some(true),
                        probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                        ..Default::default()
                    },
                    ModelCapability {
                        model: "gpt-5.4-mini".into(),
                        wire: Some(WireFormat::Responses),
                        ..Default::default()
                    },
                ],
                catalog_scope: None,
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::Bearer,
        );
        let route_id = routes
            .iter()
            .find(|route| route.name == "OpenCode Zen")
            .unwrap()
            .id
            .clone();

        assert!(state.replace_route_model_capability(
            &route_id,
            ModelCapability {
                model: "gpt-5.4-mini".into(),
                wire: Some(WireFormat::Responses),
                streaming: Some(true),
                tool_calling: Some(true),
                probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                ..Default::default()
            },
        ));

        let reopened = AppState::with_data_dir(temp.path().to_path_buf());
        let route = reopened
            .routes()
            .into_iter()
            .find(|route| route.id == route_id)
            .unwrap();
        let verified = route
            .model_capabilities
            .iter()
            .find(|capability| capability.model == "gpt-5.4-mini")
            .unwrap();
        assert_eq!(verified.wire, Some(WireFormat::Responses));
        assert_eq!(verified.tool_calling, Some(true));
        assert_eq!(
            verified.probe_version,
            Some(crate::probe::HARNESS_PROBE_VERSION)
        );
        assert!(route.model_capabilities.iter().any(|capability| {
            capability.model == "big-pickle" && capability.tool_calling == Some(true)
        }));
    }

    fn zen_route_for_merge_tests(state: &AppState, models: Vec<&str>) -> String {
        let models: Vec<String> = models.into_iter().map(str::to_string).collect();
        let routes = state.create_route(
            CreateRouteInput {
                name: "OpenCode Zen".into(),
                base_url: crate::probe::OPENCODE_ZEN_BASE_URL.into(),
                model: models[0].clone(),
                wire: WireFormat::Chat,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(ProviderKind::OpenAiCompatible),
                api_key: None,
                models: Some(models.clone()),
                selected_models: Some(models),
                context_window: None,
                model_capabilities: Vec::new(),
                catalog_scope: None,
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::Bearer,
        );
        routes
            .iter()
            .find(|route| route.name == "OpenCode Zen")
            .unwrap()
            .id
            .clone()
    }

    fn placeholder_capability(model: &str) -> ModelCapability {
        ModelCapability {
            model: model.into(),
            wire: Some(WireFormat::Chat),
            probe_version: None,
            ..Default::default()
        }
    }

    /// The bug this whole merge path exists to fix: a Provider-level
    /// re-probe that only actually verified some of a route's selected
    /// models (the rest were not targeted this round, or their probe
    /// failed) must never blank out the models it did not touch.
    #[test]
    fn merge_route_probe_result_keeps_last_known_good_for_untargeted_models() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let route_id = zen_route_for_merge_tests(&state, vec!["big-pickle", "glm-5.2"]);
        // Seed both models as already verified, including a live Effort
        // probe on "glm-5.2".
        assert!(state.replace_route_model_capability(
            &route_id,
            ModelCapability {
                model: "big-pickle".into(),
                wire: Some(WireFormat::Chat),
                tool_calling: Some(true),
                probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                ..Default::default()
            },
        ));
        assert!(state.replace_route_model_capability(
            &route_id,
            ModelCapability {
                model: "glm-5.2".into(),
                wire: Some(WireFormat::Chat),
                tool_calling: Some(true),
                probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                reasoning_efforts: vec!["low".into(), "high".into()],
                effort_probe_version: Some(crate::probe::EFFORT_PROBE_VERSION),
                effort_probe_status: crate::model::EffortProbeStatus::Supported,
                ..Default::default()
            },
        ));

        // A fresh catalog discovery finds a brand-new third model too. This
        // round's live verification only targeted "big-pickle" (the other
        // two were not selected, or their probe simply was not attempted).
        let discovery = crate::model::ProbeResult {
            reachable: true,
            wire: None,
            models: vec!["big-pickle".into(), "glm-5.2".into(), "new-model".into()],
            context_window: None,
            streaming: false,
            reasoning: false,
            server_side_resume: false,
            model_capabilities: vec![
                placeholder_capability("big-pickle"),
                placeholder_capability("glm-5.2"),
                placeholder_capability("new-model"),
            ],
            stream_quality: None,
            needs_input: Vec::new(),
        };
        let freshly_verified = vec![ModelCapability {
            model: "big-pickle".into(),
            wire: Some(WireFormat::Chat),
            tool_calling: Some(true),
            probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
            reasoning_efforts: vec!["medium".into()],
            effort_probe_version: Some(crate::probe::EFFORT_PROBE_VERSION),
            effort_probe_status: crate::model::EffortProbeStatus::Supported,
            ..Default::default()
        }];
        assert!(state.merge_route_probe_result(&route_id, &discovery, freshly_verified));

        let route = state
            .routes()
            .into_iter()
            .find(|route| route.id == route_id)
            .unwrap();
        let big_pickle = route
            .model_capabilities
            .iter()
            .find(|capability| capability.model == "big-pickle")
            .unwrap();
        assert_eq!(big_pickle.reasoning_efforts, vec!["medium".to_string()]);

        // "glm-5.2" was not targeted this round -- its previously-verified
        // Effort data must survive untouched, not revert to an unprobed
        // placeholder.
        let glm = route
            .model_capabilities
            .iter()
            .find(|capability| capability.model == "glm-5.2")
            .unwrap();
        assert_eq!(
            glm.reasoning_efforts,
            vec!["low".to_string(), "high".to_string()]
        );
        assert_eq!(
            glm.effort_probe_status,
            crate::model::EffortProbeStatus::Supported
        );
        assert_eq!(
            glm.effort_probe_version,
            Some(crate::probe::EFFORT_PROBE_VERSION)
        );

        // The newly-discovered model gets an unprobed placeholder, not an
        // error and not omission.
        assert!(route.model_capabilities.iter().any(
            |capability| capability.model == "new-model" && capability.probe_version.is_none()
        ));
    }

    #[test]
    fn merge_route_probe_result_drops_a_model_removed_from_the_upstream_catalog() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let route_id = zen_route_for_merge_tests(&state, vec!["big-pickle", "retired-model"]);
        assert!(state.replace_route_model_capability(
            &route_id,
            ModelCapability {
                model: "retired-model".into(),
                wire: Some(WireFormat::Chat),
                tool_calling: Some(true),
                probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                ..Default::default()
            },
        ));

        let discovery = crate::model::ProbeResult {
            reachable: true,
            wire: None,
            models: vec!["big-pickle".into()],
            context_window: None,
            streaming: false,
            reasoning: false,
            server_side_resume: false,
            model_capabilities: vec![placeholder_capability("big-pickle")],
            stream_quality: None,
            needs_input: Vec::new(),
        };
        assert!(state.merge_route_probe_result(&route_id, &discovery, Vec::new()));

        let route = state
            .routes()
            .into_iter()
            .find(|route| route.id == route_id)
            .unwrap();
        assert!(!route
            .model_capabilities
            .iter()
            .any(|capability| capability.model == "retired-model"));
    }

    #[test]
    fn merge_route_probe_result_preserves_user_declared_display_name_vision_and_free_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let route_id = zen_route_for_merge_tests(&state, vec!["big-pickle"]);
        assert!(state.replace_route_model_capability(
            &route_id,
            ModelCapability {
                model: "big-pickle".into(),
                wire: Some(WireFormat::Chat),
                tool_calling: Some(true),
                probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                display_name: Some("My Pickle".into()),
                vision: Some(true),
                free: Some(true),
                access_mode: Some(crate::model::AccessMode::AnonymousFree),
                ..Default::default()
            },
        ));

        let discovery = crate::model::ProbeResult {
            reachable: true,
            wire: None,
            models: vec!["big-pickle".into()],
            context_window: None,
            streaming: false,
            reasoning: false,
            server_side_resume: false,
            model_capabilities: vec![placeholder_capability("big-pickle")],
            stream_quality: None,
            needs_input: Vec::new(),
        };
        // The live-verify path never sets display_name/vision/free/access_mode
        // itself -- exactly what `probe_model_capability` actually produces.
        let freshly_verified = vec![ModelCapability {
            model: "big-pickle".into(),
            wire: Some(WireFormat::Chat),
            tool_calling: Some(true),
            probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
            ..Default::default()
        }];
        assert!(state.merge_route_probe_result(&route_id, &discovery, freshly_verified));

        let route = state
            .routes()
            .into_iter()
            .find(|route| route.id == route_id)
            .unwrap();
        let big_pickle = route
            .model_capabilities
            .iter()
            .find(|capability| capability.model == "big-pickle")
            .unwrap();
        assert_eq!(big_pickle.display_name.as_deref(), Some("My Pickle"));
        assert_eq!(big_pickle.vision, Some(true));
        assert_eq!(big_pickle.free, Some(true));
        assert_eq!(
            big_pickle.access_mode,
            Some(crate::model::AccessMode::AnonymousFree)
        );
        // The fresh verification's own fields still apply.
        assert_eq!(big_pickle.tool_calling, Some(true));
    }

    #[test]
    fn merge_route_probe_result_keeps_a_first_time_discovered_display_name_verification_never_sets()
    {
        // The Ox Alpha Free case: a model verified for the very first time
        // has no persisted `existing` row yet, but its discovery placeholder
        // already carries a catalog-sourced display name (OpenCode Zen's
        // `x-preview-f-free` -> "Ox Alpha Free" -- id unreadable, real name
        // known statically). The live-verify path never sets display_name
        // itself, so without falling back to the placeholder, this name
        // would be dropped on exactly the pass that first discovers and
        // verifies the model together.
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let route_id = zen_route_for_merge_tests(&state, vec!["x-preview-f-free"]);

        let discovery = crate::model::ProbeResult {
            reachable: true,
            wire: None,
            models: vec!["x-preview-f-free".into()],
            context_window: None,
            streaming: false,
            reasoning: false,
            server_side_resume: false,
            model_capabilities: vec![ModelCapability {
                display_name: Some("Ox Alpha Free".into()),
                ..placeholder_capability("x-preview-f-free")
            }],
            stream_quality: None,
            needs_input: Vec::new(),
        };
        let freshly_verified = vec![ModelCapability {
            model: "x-preview-f-free".into(),
            wire: Some(WireFormat::Chat),
            tool_calling: Some(true),
            probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
            ..Default::default()
        }];
        assert!(state.merge_route_probe_result(&route_id, &discovery, freshly_verified));

        let route = state
            .routes()
            .into_iter()
            .find(|route| route.id == route_id)
            .unwrap();
        let ox = route
            .model_capabilities
            .iter()
            .find(|capability| capability.model == "x-preview-f-free")
            .unwrap();
        assert_eq!(ox.display_name.as_deref(), Some("Ox Alpha Free"));
        // Routing/persistence still key off the raw upstream id, never the
        // display name.
        assert_eq!(ox.model, "x-preview-f-free");
        assert_eq!(ox.tool_calling, Some(true));

        // A second re-probe round (no display_name in discovery this time,
        // matching a route whose catalog cache was already warm) must not
        // regress the name back to nothing.
        let quiet_discovery = crate::model::ProbeResult {
            model_capabilities: vec![placeholder_capability("x-preview-f-free")],
            ..discovery
        };
        let reverified = vec![ModelCapability {
            model: "x-preview-f-free".into(),
            wire: Some(WireFormat::Chat),
            tool_calling: Some(true),
            probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
            ..Default::default()
        }];
        assert!(state.merge_route_probe_result(&route_id, &quiet_discovery, reverified));
        let route = state
            .routes()
            .into_iter()
            .find(|route| route.id == route_id)
            .unwrap();
        let ox = route
            .model_capabilities
            .iter()
            .find(|capability| capability.model == "x-preview-f-free")
            .unwrap();
        assert_eq!(ox.display_name.as_deref(), Some("Ox Alpha Free"));
    }

    #[test]
    fn failed_focused_probe_keeps_last_known_good_but_advances_context_and_diagnostic() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let routes = state.create_route(
            CreateRouteInput {
                name: "806".into(),
                base_url: "http://127.0.0.1:8000/v1".into(),
                model: "qwen".into(),
                wire: WireFormat::Responses,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(ProviderKind::OpenAiCompatible),
                api_key: None,
                models: Some(vec!["qwen".into()]),
                selected_models: Some(vec!["qwen".into()]),
                context_window: Some(98_048),
                model_capabilities: vec![ModelCapability {
                    model: "qwen".into(),
                    context_window: Some(98_048),
                    wire: Some(WireFormat::Responses),
                    tool_calling: Some(true),
                    probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                    reasoning_efforts: vec!["none".into(), "low".into(), "high".into()],
                    effort_probe_status: crate::model::EffortProbeStatus::Supported,
                    ..Default::default()
                }],
                catalog_scope: None,
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::None,
        );
        let route_id = routes
            .iter()
            .find(|route| route.name == "806")
            .unwrap()
            .id
            .clone();

        assert!(state.replace_route_model_capability(
            &route_id,
            ModelCapability {
                model: "qwen".into(),
                context_window: Some(196_608),
                wire: Some(WireFormat::Responses),
                tool_calling: Some(false),
                probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                probe_issue: Some("tool_call_missing".into()),
                probe_attempts: vec![crate::model::ProbeAttempt {
                    stage: "typedTool".into(),
                    outcome: "tool_call_missing".into(),
                    message: Some("plain text instead of a typed tool call".into()),
                    ..Default::default()
                }],
                last_probe_failed: Some(true),
                last_probed_at: Some(123),
                ..Default::default()
            },
        ));

        let route = state
            .routes()
            .into_iter()
            .find(|route| route.id == route_id)
            .unwrap();
        let qwen = route
            .model_capabilities
            .iter()
            .find(|capability| capability.model == "qwen")
            .unwrap();
        assert_eq!(qwen.context_window, Some(196_608));
        assert_eq!(qwen.tool_calling, Some(true));
        assert_eq!(qwen.wire, Some(WireFormat::Responses));
        assert_eq!(qwen.last_probe_failed, Some(true));
        assert_eq!(qwen.probe_issue.as_deref(), Some("tool_call_missing"));
        assert_eq!(qwen.reasoning_efforts, vec!["none", "low", "high"]);
        assert_eq!(qwen.probe_attempts[0].stage, "typedTool");
    }

    /// A keyless route carrying a paid model was *asked* for that way -- the
    /// generic Add Provider flow offers the full catalog on purpose. The
    /// migration runs on every load, so imposing `freeOnly` here deleted that
    /// choice, and the models with it, every time the app started.
    #[test]
    fn a_keyless_route_that_carries_paid_models_keeps_them_and_its_scope() {
        let mut routes = vec![zen_route(
            CatalogScope::All,
            AuthKind::None,
            vec!["gpt-5.6-sol".into(), "big-pickle".into()],
        )];
        assert!(!migrate_opencode_catalog_scopes(&mut routes));
        assert_eq!(routes[0].catalog_scope, CatalogScope::All);
        assert_eq!(routes[0].models, vec!["gpt-5.6-sol", "big-pickle"]);
    }

    /// Labelling a route that is already free-only is safe: it writes down
    /// what is true and removes nothing.
    #[test]
    fn a_keyless_route_that_is_already_free_only_is_labelled_without_losing_models() {
        let mut routes = vec![zen_route(
            CatalogScope::All,
            AuthKind::None,
            vec!["big-pickle".into(), "mimo-v2.5-free".into()],
        )];
        assert!(migrate_opencode_catalog_scopes(&mut routes));
        assert_eq!(routes[0].catalog_scope, CatalogScope::FreeOnly);
        assert_eq!(routes[0].models, vec!["big-pickle", "mimo-v2.5-free"]);
    }

    /// `opencode_zen_known_free` answers `Some(false)` for every OpenCode Go
    /// model, and the endpoint test matches Go too. Pruning a Go route would
    /// therefore delete its entire catalog and leave nothing selectable.
    #[test]
    fn a_free_only_go_route_is_never_pruned_to_an_empty_catalog() {
        let mut route = zen_route(
            CatalogScope::FreeOnly,
            AuthKind::None,
            vec!["deepseek-v4-pro".into(), "glm-5".into()],
        );
        route.base_url = "https://opencode.ai/zen/go/v1".into();
        let mut routes = vec![route];
        migrate_opencode_catalog_scopes(&mut routes);
        assert_eq!(routes[0].models, vec!["deepseek-v4-pro", "glm-5"]);
        assert!(routes[0]
            .selected_models
            .as_ref()
            .is_some_and(|s| !s.is_empty()));
    }

    fn zen_route(scope: CatalogScope, auth_kind: AuthKind, models: Vec<String>) -> Route {
        Route {
            model: models[0].clone(),
            model_capabilities: models
                .iter()
                .map(|model| ModelCapability {
                    model: model.clone(),
                    ..Default::default()
                })
                .collect(),
            selected_models: Some(models.clone()),
            models,
            auth_kind,
            catalog_scope: scope,
            ..zen_route_shell()
        }
    }

    fn zen_route_shell() -> Route {
        Route {
            id: "zen".into(),
            name: "OpenCode Zen".into(),
            base_url: crate::probe::OPENCODE_ZEN_BASE_URL.into(),
            model: String::new(),
            wire: WireFormat::Chat,
            is_current: false,
            server_side_resume: false,
            streaming: true,
            reasoning: true,
            provider_kind: ProviderKind::OpenAiCompatible,
            auth_kind: AuthKind::None,
            enabled: true,
            models: Vec::new(),
            selected_models: None,
            context_window: None,
            model_capabilities: Vec::new(),
            insecure_http_policy: Default::default(),
            catalog_scope: CatalogScope::All,
        }
    }

    /// An **explicitly** free-only route still has every catalog surface
    /// pruned together, and its default model repaired if the prune removed
    /// it. Only the imposing of that scope on a route that never asked for it
    /// was dropped; enforcing a scope the route really carries is unchanged.
    #[test]
    fn an_explicitly_free_only_route_filters_every_catalog_surface_and_repairs_default() {
        let mut routes = vec![Route {
            id: "opencode-zen".into(),
            name: "OpenCode Zen".into(),
            base_url: crate::probe::OPENCODE_ZEN_BASE_URL.into(),
            model: "gpt-5.6-sol".into(),
            wire: WireFormat::Chat,
            is_current: false,
            server_side_resume: false,
            streaming: true,
            reasoning: true,
            provider_kind: ProviderKind::OpenAiCompatible,
            auth_kind: AuthKind::None,
            enabled: true,
            models: vec!["gpt-5.6-sol".into(), "big-pickle".into()],
            selected_models: Some(vec!["gpt-5.6-sol".into()]),
            context_window: None,
            model_capabilities: vec![
                ModelCapability {
                    model: "gpt-5.6-sol".into(),
                    free: Some(false),
                    ..Default::default()
                },
                ModelCapability {
                    model: "big-pickle".into(),
                    free: Some(true),
                    ..Default::default()
                },
            ],
            insecure_http_policy: Default::default(),
            catalog_scope: CatalogScope::FreeOnly,
        }];

        assert!(migrate_opencode_catalog_scopes(&mut routes));
        let route = &routes[0];
        assert_eq!(route.catalog_scope, CatalogScope::FreeOnly);
        assert_eq!(route.models, vec!["big-pickle"]);
        assert_eq!(route.selected_models, Some(vec!["big-pickle".into()]));
        assert_eq!(route.model, "big-pickle");
        assert_eq!(route.model_capabilities.len(), 1);
        assert_eq!(route.model_capabilities[0].model, "big-pickle");
    }

    #[test]
    fn successful_runtime_request_upgrades_stale_probe_version_and_wire() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let routes = state.create_route(
            CreateRouteInput {
                name: "Local llama.cpp".into(),
                base_url: "http://127.0.0.1:8000/v1".into(),
                model: "Qwen3-Coder-Next".into(),
                wire: WireFormat::Responses,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(ProviderKind::OpenAiCompatible),
                api_key: None,
                models: Some(vec!["Qwen3-Coder-Next".into()]),
                selected_models: Some(vec!["Qwen3-Coder-Next".into()]),
                context_window: Some(204_800),
                model_capabilities: vec![ModelCapability {
                    model: "Qwen3-Coder-Next".into(),
                    context_window: Some(204_800),
                    wire: Some(WireFormat::Responses),
                    streaming: Some(true),
                    reasoning: Some(true),
                    probe_version: None,
                    ..Default::default()
                }],
                catalog_scope: None,
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::None,
        );
        let route_id = routes
            .iter()
            .find(|route| route.name == "Local llama.cpp")
            .unwrap()
            .id
            .clone();
        assert!(state.record_runtime_model_capability(
            &route_id,
            "Qwen3-Coder-Next",
            WireFormat::Chat
        ));
        let route = state
            .routes()
            .into_iter()
            .find(|route| route.id == route_id)
            .unwrap();
        let capability = route
            .model_capabilities
            .iter()
            .find(|capability| capability.model == "Qwen3-Coder-Next")
            .unwrap();
        assert_eq!(capability.wire, Some(WireFormat::Chat));
        assert_eq!(
            capability.probe_version,
            Some(crate::probe::HARNESS_PROBE_VERSION)
        );
    }

    #[test]
    fn model_context_override_updates_routes_persists_and_can_be_cleared() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let grok = state
            .model_routes()
            .into_iter()
            .find(|model| model.route_id == "grok-cli")
            .expect("seeded Grok model");
        let original = grok.context_window;

        state.set_budget_override(&grok.catalog_id, Some(300_000));
        assert_eq!(
            state
                .route_for_catalog_model(&grok.catalog_id)
                .unwrap()
                .1
                .context_window,
            Some(300_000)
        );

        let reopened = AppState::with_data_dir(temp.path().to_path_buf());
        assert_eq!(
            reopened
                .route_for_catalog_model(&grok.catalog_id)
                .unwrap()
                .1
                .context_window,
            Some(300_000)
        );

        reopened.set_budget_override(&grok.catalog_id, None);
        assert_eq!(
            reopened
                .route_for_catalog_model(&grok.catalog_id)
                .unwrap()
                .1
                .context_window,
            original
        );
    }

    /// The override that keeps eval baselines reproducible must not be a way
    /// back to Canonical on a real install: outside eval mode the write is
    /// dropped and resolution falls through to the built-in gateway default.
    #[test]
    fn route_compaction_override_is_refused_outside_eval_mode() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        state.set_eval_route_compaction_policy(
            "grok-cli",
            Some(crate::policy::SessionCompactionPolicy {
                strategy: crate::policy::CompactionStrategy::VellumCanonical {
                    compactor: crate::policy::CompactorSelector::SessionModel,
                },
                ..Default::default()
            }),
        );
        let route = state
            .routes()
            .into_iter()
            .find(|route| route.id == "grok-cli")
            .unwrap();
        let resolved = state.resolve_compaction_policy(&route, "grok-4.5", false);
        assert_eq!(
            resolved.strategy,
            crate::policy::CompactionStrategy::Disabled
        );
        assert_eq!(
            resolved.pipeline,
            crate::policy::RequestPipeline::ThirdPartyGateway
        );
        assert_eq!(state.runtime_canonical_engine(), None);
    }

    /// Reasoning effort still has to be verified against the model that will
    /// actually run the compaction — which now only happens on an eval
    /// baseline, the sole caller that can still select Canonical.
    #[test]
    fn stale_compaction_effort_falls_back_to_automatic_with_a_reason() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_eval_routes(temp.path().to_path_buf(), seed_routes());
        state.set_eval_route_compaction_policy(
            "grok-cli",
            Some(crate::policy::SessionCompactionPolicy {
                strategy: crate::policy::CompactionStrategy::VellumCanonical {
                    compactor: crate::policy::CompactorSelector::SessionModel,
                },
                reasoning_effort: Some("definitely-not-supported".into()),
                ..Default::default()
            }),
        );
        let route = state
            .routes()
            .into_iter()
            .find(|route| route.id == "grok-cli")
            .unwrap();
        let resolved = state.resolve_compaction_policy_for_session(&route, "grok-4.5", None, false);
        assert_eq!(
            resolved.pipeline,
            crate::policy::RequestPipeline::VellumCanonical
        );
        assert_eq!(resolved.resolved_reasoning_effort, None);
        assert_eq!(resolved.reasoning_effort_source, "automatic_fallback");
        assert!(resolved
            .reasoning_effort_fallback_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("not verified")));
    }
}
