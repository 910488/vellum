//! Desktop authority adapters for the shared proxy runtime.

use crate::model::{AuthKind, ProviderKind, ReviewPolicy};
use crate::state::AppState;
use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use vellum_proxy_runtime::config::RuntimeCompactionPolicy;
use vellum_proxy_runtime::history::{
    HistoryEntry as RuntimeHistoryEntry, HistoryStore as RuntimeHistoryStore,
};
use vellum_proxy_runtime::route::{
    grok_responses_readiness, projected_wire, ResolvedRoute, RouteCatalog,
    RuntimeCompactionCapabilities, RuntimeModelRoute, RuntimeReasoningCapabilities,
    RuntimeToolCapabilities,
};
use vellum_proxy_runtime::search::{
    SearchEngine as RuntimeSearchEngine, SearchRequest as RuntimeSearchRequest,
    SearchResponse as RuntimeSearchResponse, SearchResult as RuntimeSearchResult,
};
use vellum_proxy_runtime::usage::{
    UsageRecord as RuntimeUsageRecord, UsageStore as RuntimeUsageStore,
    UsageSummary as RuntimeUsageSummary,
};
use vellum_proxy_runtime::{
    CredentialProvider, ExecutionEnvironment, ModelRouteView, OfficialAuthDecision,
    OfficialAuthProvider, OfficialAuthorization, ProxyRuntime, ProxyRuntimeIdentity,
    ProxyRuntimeState, RequestGuard, RequestLifecycle, RuntimeDiagnostics,
};

#[derive(Clone)]
struct DesktopCatalog {
    state: AppState,
}

impl DesktopCatalog {
    // Deliberately uncached: `ProxyRuntime` calls `RuntimeSnapshot::capture_from`
    // exactly once per request and freezes that result for the request's
    // lifetime (see `crates/vellum-proxy-runtime/src/exec.rs`). Caching a
    // snapshot here on top of that would make hot-refresh (catalog edits
    // while the app is running) invisible until restart.
    fn snapshot(&self) -> Option<DesktopCatalogSnapshot> {
        desktop_catalog_snapshot(&self.state).ok()
    }
}

struct DesktopCatalogSnapshot {
    routes: Vec<RuntimeModelRoute>,
    review_ids: HashSet<String>,
    entries: HashMap<String, Value>,
}
impl RouteCatalog for DesktopCatalog {
    fn active_models(&self) -> Vec<RuntimeModelRoute> {
        self.snapshot()
            .map(|catalog| catalog.routes)
            .unwrap_or_default()
    }
    fn resolve_model(&self, id: &str) -> Option<ResolvedRoute> {
        self.snapshot()?
            .routes
            .into_iter()
            .find(|r| r.catalog_id == id)
            .map(|route| ResolvedRoute { route })
    }
    fn resolve_review_model(&self, id: &str) -> Option<ResolvedRoute> {
        let snapshot = self.snapshot()?;
        snapshot
            .routes
            .iter()
            .find(|r| r.catalog_id == id && snapshot.review_ids.contains(id))
            .cloned()
            .map(|route| ResolvedRoute { route })
            .or_else(|| self.resolve_model(id))
    }
    fn catalog_entry(&self, id: &str) -> Option<Value> {
        self.snapshot()?.entries.get(id).cloned()
    }
    fn route_entries(&self) -> Vec<(RuntimeModelRoute, Option<Value>)> {
        let Some(snapshot) = self.snapshot() else {
            return Vec::new();
        };
        let DesktopCatalogSnapshot {
            routes, entries, ..
        } = snapshot;
        routes
            .into_iter()
            .map(|route| {
                let entry = entries.get(&route.catalog_id).cloned();
                (route, entry)
            })
            .collect()
    }
}

fn grok_structured_secret(
    home: &std::path::Path,
    access_token: &str,
    user_id: Option<&str>,
) -> String {
    serde_json::json!({
        "access_token": access_token,
        "client_version": crate::grok_auth::client_version(home)
            .or_else(|| crate::grok_auth::cli_version(home)),
        "agent_id": crate::grok_auth::agent_id(home),
        "user_id": user_id,
    })
    .to_string()
}

struct DesktopCredentials {
    state: AppState,
}
#[async_trait]
impl CredentialProvider for DesktopCredentials {
    async fn get_secret(&self, id: &str) -> Result<Option<String>, String> {
        if self
            .state
            .routes()
            .iter()
            .any(|route| route.id == id && route.provider_kind == ProviderKind::GrokCli)
        {
            return self
                .state
                .grok_accounts()
                .resolve_default()
                .await
                .and_then(|(account_id, credential)| {
                    let home = self.state.grok_accounts().account_home(&account_id)?;
                    Ok(Some(grok_structured_secret(
                        &home,
                        credential.access_token.as_str(),
                        credential.user_id.as_deref(),
                    )))
                })
                .map_err(|e| e.to_string());
        }
        crate::credentials::load(&self.state.data_root(), id).map_err(|e| e.to_string())
    }
    async fn list_ids(&self) -> Result<Vec<String>, String> {
        let mut ids = Vec::new();
        for route in self.state.routes() {
            if self.get_secret(&route.id).await?.is_some() {
                ids.push(route.id);
            }
        }
        ids.sort();
        Ok(ids)
    }
}

struct DesktopOfficialAuth {
    state: AppState,
}
#[async_trait]
impl OfficialAuthProvider for DesktopOfficialAuth {
    async fn authorize(&self, _route_id: &str) -> Result<OfficialAuthDecision, String> {
        self.state
            .codex_oauth()
            .valid_default_auth()
            .await
            .map(|auth| match auth {
                Some(auth) => OfficialAuthDecision::Managed(OfficialAuthorization {
                    access_token: auth.access_token,
                    account_id: Some(auth.account_id),
                    selection_revision: Some(auth.selection_revision),
                    selection_verified: auth.selection_verified,
                }),
                None => OfficialAuthDecision::PreserveIncoming,
            })
            .map_err(|e| e.to_string())
    }
    /// Desktop is the only host that can actually hold several managed
    /// ChatGPT accounts at once, so it is the only implementor that
    /// overrides this. `valid_auth_for` fails loudly when the named account
    /// is gone (removed, or signed out) rather than quietly falling back to
    /// the default -- the caller checks the returned identity anyway, but
    /// failing here gives the user the account id in the message.
    ///
    /// The selection revision and verification bit are passed through as the
    /// account manager reports them. They describe the *selection pointer*,
    /// which this path deliberately bypasses; they are carried only so a
    /// mid-turn refresh can prove it did not drift (see
    /// `official_refresh_matches_snapshot`), and `refresh_after_rejection`
    /// already refreshes by `rejected.account_id`, which is this account.
    async fn authorize_as(
        &self,
        route_id: &str,
        account_id: Option<&str>,
    ) -> Result<OfficialAuthDecision, String> {
        let Some(account_id) = account_id else {
            return self.authorize(route_id).await;
        };
        self.state
            .codex_oauth()
            .valid_auth_for(account_id)
            .await
            .map(|auth| {
                OfficialAuthDecision::Managed(OfficialAuthorization {
                    access_token: auth.access_token,
                    account_id: Some(auth.account_id),
                    selection_revision: Some(auth.selection_revision),
                    selection_verified: auth.selection_verified,
                })
            })
            .map_err(|e| e.to_string())
    }
    async fn refresh_after_rejection(
        &self,
        _route_id: &str,
        rejected: &OfficialAuthorization,
    ) -> Result<OfficialAuthorization, String> {
        let account = rejected
            .account_id
            .as_deref()
            .ok_or_else(|| "managed Official auth has no account id".to_string())?;
        self.state
            .codex_oauth()
            .refresh_after_rejection(account, &rejected.access_token)
            .await
            .map(|auth| OfficialAuthorization {
                access_token: auth.access_token,
                account_id: Some(auth.account_id),
                // Refresh is part of the already-resolved turn.  Preserve
                // that turn's immutable selection snapshot even if the UI
                // selected another account while the refresh was in flight;
                // the manager's current revision belongs to a later turn.
                selection_revision: rejected.selection_revision,
                selection_verified: rejected.selection_verified && auth.selection_verified,
            })
            .map_err(|e| e.to_string())
    }
}

struct DesktopLifecycle {
    state: AppState,
}
impl RequestLifecycle for DesktopLifecycle {
    fn begin(&self) -> Result<RequestGuard, String> {
        self.state
            .try_begin_request()
            .map(RequestGuard::from_owned)
            .map_err(|e| e.to_string())
    }
}

struct DesktopHistory {
    inner: Arc<crate::history::HistoryStore>,
}
impl RuntimeHistoryStore for DesktopHistory {
    fn record_local_compaction(
        &self,
        record: vellum_proxy_runtime::history::LocalCompactionRecordV1,
    ) -> Result<(), String> {
        record.validate()?;
        self.inner
            .save_canonical_compaction_with_meta(
                &record.compaction_id,
                crate::history::CompactionJournalKind::Local,
                record.source_items,
                record.replacement_items,
                None,
                "runtime",
                None,
                None,
                None,
                crate::history::CompactionSaveMeta {
                    engine_id: Some(record.engine_id),
                    engine_provenance: record.engine_provenance,
                    route_id: Some(record.route_id),
                    upstream_model: Some(record.upstream_model),
                    tokens_before: Some(record.tokens_before),
                    tokens_after: Some(record.tokens_after),
                    elapsed_ms: Some(record.elapsed_ms),
                    generation: record.generation,
                    source_head_response_id: Some(record.response_id),
                    checkpoint_schema_version: Some(record.schema_version),
                    source_hash: Some(record.source_hash),
                    checkpoint_hash: Some(record.replacement_hash),
                    ..Default::default()
                },
            )
            .map_err(|error| error.to_string())
    }

    fn recovery_snapshot(
        &self,
        conversation_key: &str,
    ) -> Result<
        Option<vellum_proxy_runtime::recovery_snapshot::ConversationRecoverySnapshotV1>,
        String,
    > {
        let Some(blob) = self
            .inner
            .recovery_snapshot_blob(conversation_key)
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        // A row that no longer decodes is treated as absent rather than fatal:
        // damaged recovery bookkeeping must not block the conversation.
        Ok(serde_json::from_slice(&blob).ok())
    }

    fn put_recovery_snapshot(
        &self,
        snapshot: &vellum_proxy_runtime::recovery_snapshot::ConversationRecoverySnapshotV1,
    ) -> Result<(), String> {
        snapshot.validate()?;
        let blob = serde_json::to_vec(snapshot)
            .map_err(|error| format!("encode recovery snapshot: {error}"))?;
        self.inner
            .put_recovery_snapshot_blob(&snapshot.conversation_key, &blob, snapshot.updated_at)
            .map_err(|error| error.to_string())
    }

    fn record_compaction(
        &self,
        response_id: &str,
        compaction_id: &str,
        generation: u32,
        items: Vec<Value>,
    ) -> Result<(), String> {
        let source_items = items.clone();
        let source_hash = vellum_proxy_runtime::history::hash_items(&source_items);
        let checkpoint_hash = vellum_proxy_runtime::history::hash_items(&items);
        self.record_compaction_record(vellum_proxy_runtime::history::CompactionJournalRecord {
            response_id: response_id.to_string(),
            compaction_id: compaction_id.to_string(),
            generation,
            engine_id: vellum_proxy_runtime::codex_local_v0_150::ENGINE_ID.to_string(),
            engine_provenance: Some(
                vellum_proxy_runtime::codex_local_v0_150::ENGINE_PROVENANCE.to_string(),
            ),
            route_id: String::new(),
            upstream_model: String::new(),
            tokens_before: 0,
            tokens_after: 0,
            elapsed_ms: 0,
            schema_version: vellum_proxy_runtime::history::LOCAL_COMPACTION_RECORD_SCHEMA_V1,
            source_items,
            canonical_items: items,
            source_hash,
            checkpoint_hash,
            provider_owned: false,
        })
    }

    fn record_compaction_record(
        &self,
        record: vellum_proxy_runtime::history::CompactionJournalRecord,
    ) -> Result<(), String> {
        let strategy = if record.schema_version == 2 {
            Some("canonical_v2".to_string())
        } else {
            Some("canonical_v1".to_string())
        };
        let continuity_kind = if record.schema_version == 2 {
            Some("semantic".to_string())
        } else {
            None
        };
        // A compaction the provider performed is not a Vellum one, and the
        // journal kind is what every reader downstream branches on. Stamping
        // it `Local` is why the Context screen described OpenAI's compactions
        // as 舊版 Vellum 壓縮紀錄 and tried to show a plaintext "after" that
        // does not exist.
        let kind = if record.provider_owned {
            crate::history::CompactionJournalKind::Official
        } else {
            crate::history::CompactionJournalKind::Local
        };

        self.inner
            .save_canonical_compaction_with_meta(
                &record.compaction_id,
                kind,
                record.source_items,
                record.canonical_items,
                None,
                "runtime",
                None,
                None,
                None,
                crate::history::CompactionSaveMeta {
                    engine_id: Some(record.engine_id),
                    engine_provenance: record.engine_provenance,
                    route_id: (!record.route_id.is_empty()).then_some(record.route_id),
                    upstream_model: (!record.upstream_model.is_empty())
                        .then_some(record.upstream_model),
                    tokens_before: Some(record.tokens_before),
                    tokens_after: Some(record.tokens_after),
                    elapsed_ms: Some(record.elapsed_ms),
                    generation: record.generation,
                    source_head_response_id: Some(record.response_id),
                    strategy,
                    continuity_kind,
                    checkpoint_schema_version: Some(record.schema_version),
                    source_hash: (!record.source_hash.is_empty()).then_some(record.source_hash),
                    checkpoint_hash: (!record.checkpoint_hash.is_empty())
                        .then_some(record.checkpoint_hash),
                    ..Default::default()
                },
            )
            .map_err(|e| e.to_string())
    }
    fn compaction_ids_for_response(&self, id: &str) -> Result<Vec<String>, String> {
        self.inner
            .compaction_ids_for_response(id)
            .map_err(|e| e.to_string())
    }
    fn compaction_generation(&self, id: &str) -> Result<Option<u32>, String> {
        self.inner
            .get_compaction(id)
            .map(|j| j.map(|j| j.generation))
            .map_err(|e| e.to_string())
    }
    fn journal_canonical_items(&self, response_id: &str) -> Result<Option<Vec<Value>>, String> {
        let ids = self
            .inner
            .compaction_ids_for_response(response_id)
            .map_err(|e| e.to_string())?;
        let Some(id) = ids.last() else {
            return Ok(None);
        };
        self.inner
            .get_compaction(id)
            .map(|j| j.map(|j| j.canonical_items))
            .map_err(|e| e.to_string())
    }
    fn journal_record_for_compaction(
        &self,
        compaction_id: &str,
    ) -> Result<Option<vellum_proxy_runtime::history::CompactionJournal>, String> {
        self.inner
            .get_compaction(compaction_id)
            .map(|journal| {
                journal.map(|j| {
                    let canonical_items = j
                        .portable_items
                        .filter(|items| !items.is_empty())
                        .unwrap_or(j.canonical_items);
                    let response_id = j
                        .source_head_response_id
                        .unwrap_or_else(|| j.compaction_id.clone());
                    vellum_proxy_runtime::history::CompactionJournal {
                        compaction_id: j.compaction_id,
                        // A stored row with no engine id predates the
                        // switchover; keep it visibly not-current so
                        // materialization refuses it rather than installing
                        // items from a schema that no longer exists.
                        engine_id: j
                            .engine_id
                            .unwrap_or_else(|| "vellum_canonical_retired".to_string()),
                        engine_provenance: j.engine_provenance,
                        route_id: j.route_id.unwrap_or_default(),
                        upstream_model: j.upstream_model.unwrap_or_default(),
                        tokens_before: j.tokens_before.unwrap_or(0),
                        tokens_after: j.tokens_after.unwrap_or(0),
                        elapsed_ms: j.elapsed_ms.unwrap_or(0),
                        response_id,
                        generation: j.generation,
                        canonical_items,
                        source_items: j.source_items,
                        schema_version: j.checkpoint_schema_version.unwrap_or(1),
                        source_hash: j.source_hash.unwrap_or_default(),
                        checkpoint_hash: j.checkpoint_hash.unwrap_or_default(),
                        created_at: j.created_at,
                    }
                })
            })
            .map_err(|e| e.to_string())
    }
    fn canonical_items_for_compaction(
        &self,
        compaction_id: &str,
    ) -> Result<Option<Vec<Value>>, String> {
        self.inner
            .get_compaction(compaction_id)
            .map(|journal| {
                journal.map(|journal| {
                    journal
                        .portable_items
                        .filter(|items| !items.is_empty())
                        .unwrap_or(journal.canonical_items)
                })
            })
            .map_err(|error| error.to_string())
    }
    fn record_exchange(
        &self,
        request: &Value,
        response: &Value,
        route_id: &str,
    ) -> Result<bool, String> {
        let saved = self
            .inner
            .record_exchange(request, response)
            .map_err(|e| e.to_string())?;
        if saved {
            if let Some(id) = response.get("id").and_then(Value::as_str) {
                self.inner
                    .mark_response_route(id, route_id)
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(saved)
    }
    fn get_chain(&self, head: &str) -> Result<Vec<RuntimeHistoryEntry>, String> {
        self.inner
            .get_chain(head, usize::MAX)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|e| {
                let continuation_realm = self
                    .inner
                    .response_realm_fingerprint(&e.response_id)
                    .map_err(|e| e.to_string())?;
                let route_id = self
                    .inner
                    .response_route(&e.response_id)
                    .map_err(|e| e.to_string())?
                    .unwrap_or_default();
                Ok(RuntimeHistoryEntry {
                    response_id: e.response_id,
                    previous_response_id: e.previous_response_id,
                    route_id,
                    continuation_realm,
                    input_items: e.input_items,
                    output_items: e.output_items,
                    created_at: e.created_at,
                })
            })
            .collect()
    }
    /// Grok session identity (`crate::grok_session::GrokSessionRegistry`)
    /// restart recovery: an explicit conversation key overrides
    /// re-derivation from the request body, so a session resolved from a
    /// caller-supplied hint (not one of the body's own identity fields)
    /// still records under one durable conversation.
    fn record_exchange_with_conversation_key(
        &self,
        request: &Value,
        response: &Value,
        route_id: &str,
        conversation_key: Option<&str>,
    ) -> Result<bool, String> {
        let saved = self
            .inner
            .record_exchange_with_conversation_key(request, response, conversation_key)
            .map_err(|e| e.to_string())?;
        if saved {
            if let Some(id) = response.get("id").and_then(Value::as_str) {
                self.inner
                    .mark_response_route(id, route_id)
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(saved)
    }
    fn record_exchange_with_context(
        &self,
        request: &Value,
        response: &Value,
        route_id: &str,
        conversation_key: Option<&str>,
        continuation_realm: Option<&str>,
    ) -> Result<bool, String> {
        let saved = self
            .inner
            .record_exchange_with_conversation_key(request, response, conversation_key)
            .map_err(|e| e.to_string())?;
        if saved {
            if let Some(id) = response.get("id").and_then(Value::as_str) {
                self.inner
                    .mark_response_context_full(id, route_id, None, continuation_realm, None)
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(saved)
    }
    fn response_continuation_realm(&self, response_id: &str) -> Result<Option<String>, String> {
        self.inner
            .response_realm_fingerprint(response_id)
            .map_err(|e| e.to_string())
    }
    fn response_conversation_key(&self, response_id: &str) -> Result<Option<String>, String> {
        self.inner
            .response_conversation_key(response_id)
            .map_err(|e| e.to_string())
    }
    fn conversation_route_exchange_count(
        &self,
        conversation_key: &str,
        route_id: &str,
    ) -> Result<u64, String> {
        self.inner
            .conversation_route_exchange_count(conversation_key, route_id)
            .map_err(|e| e.to_string())
    }
}

struct DesktopUsage {
    inner: Arc<crate::usage::UsageStore>,
    records: Mutex<Vec<RuntimeUsageRecord>>,
}
impl RuntimeUsageStore for DesktopUsage {
    fn record(&self, value: &RuntimeUsageRecord) -> Result<(), String> {
        let agent_attribution_json = value
            .agent_attribution
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| format!("serialize Codex agent attribution: {error}"))?;
        self.inner
            .record_with_agent_attribution(
                &crate::usage::UsageRecord {
                    route_id: value.route_id.clone(),
                    provider: value.provider.clone(),
                    model: value.model.clone(),
                    input_tokens: value.input_tokens,
                    output_tokens: value.output_tokens,
                    cached_input_tokens: value.cached_input_tokens,
                    reasoning_tokens: value.reasoning_tokens,
                    tool_calls: value.tool_calls,
                    compaction_tokens: value.compaction_tokens,
                    review_tokens: value.review_tokens,
                    status: value.status,
                    error: value.error.clone(),
                    duration_ms: value.duration_ms,
                    first_byte_ms: value.first_byte_ms,
                    review_run_id: value.review_run_id.clone(),
                    review_role: value.review_role.clone(),
                    review_reason: value.review_reason.clone(),
                    request_id: value.request_id.clone(),
                    connection_id: value.connection_id.clone(),
                    conversation_identity: value.conversation_identity.clone(),
                    first_event_ms: value.first_event_ms,
                    first_downstream_frame_ms: value.first_downstream_frame_ms,
                    outcome: value.outcome.clone(),
                    error_category: value.error_category.clone(),
                    stage_times_ms: value
                        .stage_times_ms
                        .as_ref()
                        .and_then(|value| serde_json::to_string(value).ok()),
                    stream_quality: value.stream_quality.clone(),
                    first_output_delta_ms: value.first_output_delta_ms,
                    first_reasoning_delta_ms: value.first_reasoning_delta_ms,
                    output_delta_count: value.output_delta_count,
                    reasoning_delta_count: value.reasoning_delta_count,
                    control_account_hash: value.control_account_hash.clone(),
                    execution_account_hash: value.execution_account_hash.clone(),
                    selection_revision: value.selection_revision,
                    auth_mode: value.auth_mode.clone(),
                    provider_profile: value.provider_profile.clone(),
                    upstream_attempted: value.upstream_attempted,
                    retry_after: value.retry_after,
                },
                agent_attribution_json.as_deref(),
            )
            .map_err(|e| e.to_string())?;
        self.records
            .lock()
            .map_err(|_| "usage cache poisoned".to_string())?
            .push(value.clone());
        Ok(())
    }
    fn summary(&self, id: &str) -> Result<RuntimeUsageSummary, String> {
        self.inner
            .summary(id)
            .map(|s| RuntimeUsageSummary {
                latest_input_tokens: s.latest_input_tokens,
                turns: s.turns,
                total_tokens: s.total_tokens,
                latest_first_byte_ms: s.latest_first_byte_ms,
            })
            .map_err(|e| e.to_string())
    }
    fn records(&self) -> Result<Vec<RuntimeUsageRecord>, String> {
        self.records
            .lock()
            .map(|r| r.clone())
            .map_err(|_| "usage cache poisoned".to_string())
    }

    fn mark_first_downstream_frame(&self, request_id: &str, elapsed_ms: u64) -> Result<(), String> {
        self.inner
            .mark_first_downstream_frame(request_id, elapsed_ms)
            .map_err(|error| error.to_string())?;
        let mut records = self
            .records
            .lock()
            .map_err(|_| "usage cache poisoned".to_string())?;
        if let Some(record) = records
            .iter_mut()
            .rev()
            .find(|record| record.request_id.as_deref() == Some(request_id))
        {
            if record.first_downstream_frame_ms.is_none() {
                record.first_downstream_frame_ms = Some(elapsed_ms);
            }
        }
        Ok(())
    }
}

struct DesktopSearch {
    engine: crate::web_search::SearchEngine,
}
#[async_trait]
impl RuntimeSearchEngine for DesktopSearch {
    async fn run(&self, request: &RuntimeSearchRequest) -> Result<RuntimeSearchResponse, String> {
        let commands = request
            .commands
            .as_ref()
            .map(|c| crate::web_search::SearchCommands {
                search_query: c.search_query.clone(),
                image_query: c.image_query.clone(),
                open: c.open.clone(),
                click: c.click.clone(),
                find: c.find.clone(),
                screenshot: c.screenshot.clone(),
                finance: c.finance.clone(),
                weather: c.weather.clone(),
                sports: c.sports.clone(),
                time: c.time.clone(),
                response_length: c.response_length.clone(),
            });
        let request = crate::web_search::SearchRequest {
            id: request.id.clone(),
            model: request.model.clone(),
            reasoning: request.reasoning.clone(),
            input: request.input.clone(),
            commands,
            settings: request.settings.clone(),
            max_output_tokens: request.max_output_tokens,
        };
        self.engine
            .run(&request)
            .await
            .map(|r| RuntimeSearchResponse {
                encrypted_output: r.encrypted_output,
                output: r.output,
                results: r
                    .results
                    .into_iter()
                    .map(|i| RuntimeSearchResult {
                        kind: i.kind,
                        ref_id: i.ref_id,
                        url: i.url,
                        title: i.title,
                        snippet: i.snippet,
                        image_url: i.image_url,
                        width: i.width,
                        height: i.height,
                        pageno: i.pageno,
                    })
                    .collect(),
            })
            .map_err(|e| e.to_string())
    }
}

/// Reads `AppState`'s Auto Review settings fresh on every call. This is the
/// fix for the bug where the shared proxy agent copied `ReviewSettings` once
/// at `DesktopProxyRuntimeState::new()` and never looked again: a policy
/// edit in the UI had no effect on an already-running local proxy until it
/// restarted. `AppState::review_settings()` already reads through to the
/// persisted store on every call, so this is a thin typed adapter, not a
/// cache.
struct DesktopReviewSettingsSource {
    state: AppState,
}

impl vellum_proxy_runtime::ReviewSettingsSource for DesktopReviewSettingsSource {
    fn current(&self) -> vellum_proxy_runtime::ReviewSettings {
        runtime_review_settings(self.state.review_settings())
    }
}

#[derive(Clone)]
pub struct DesktopProxyRuntimeState {
    identity: ProxyRuntimeIdentity,
    state: AppState,
    credentials: Arc<dyn CredentialProvider>,
    runtime: Arc<ProxyRuntime>,
    environment: ExecutionEnvironment,
}
impl DesktopProxyRuntimeState {
    pub fn new(state: AppState) -> Result<Self, String> {
        #[cfg(test)]
        let constructed = std::time::Instant::now();
        let snapshot = desktop_catalog_snapshot(&state)?;
        #[cfg(test)]
        eprintln!(
            "[runtime-new] catalog_snapshot {}ms",
            constructed.elapsed().as_millis()
        );
        let config_hash = desktop_runtime_config_hash(&snapshot);
        let credentials: Arc<dyn CredentialProvider> = Arc::new(DesktopCredentials {
            state: state.clone(),
        });
        let mut search_settings = state.web_search_settings();
        search_settings.brave_api_key = crate::credentials::load(
            &state.data_root(),
            crate::web_search::BRAVE_SEARCH_CREDENTIAL_ID,
        )
        .map_err(|e| e.to_string())?;
        // The local `web_search` compatibility wrapper is only worth
        // advertising to a model when it would actually resolve to Brave:
        // the master switch is on AND a usable Brave key is configured.
        // Offering the wrapper with no usable backend behind it would let
        // the model call a tool that fails closed on every invocation. This
        // is the exact "search enabled and Brave key present" fact
        // `SearchEngine::new(search_settings)` below is about to consume, so
        // it must be captured before `search_settings` moves into that call.
        let web_search_wrapper_enabled =
            search_settings.enabled && search_settings.brave_api_key.is_some();
        let environment = ExecutionEnvironment::from(crate::harness::shell::detected());
        #[cfg(test)]
        eprintln!(
            "[runtime-new] before ProxyRuntime::new {}ms",
            constructed.elapsed().as_millis()
        );
        let mut runtime = ProxyRuntime::new(
            Arc::new(DesktopCatalog {
                state: state.clone(),
            }),
            Arc::clone(&credentials),
            Arc::new(DesktopOfficialAuth {
                state: state.clone(),
            }),
            Arc::new(DesktopLifecycle {
                state: state.clone(),
            }),
            Arc::new(vellum_proxy_runtime::ReqwestTransport::new()),
        )
        .with_history_store(Arc::new(DesktopHistory {
            inner: state.history_store(),
        }))
        .with_usage_store(Arc::new(DesktopUsage {
            inner: state.usage_store(),
            records: Mutex::new(Vec::new()),
        }))
        .with_diagnostics_sink(Arc::new(
            crate::diagnostics_store::DesktopDiagnosticsSink::new(state.usage_store()),
        ))
        .with_search_engine(Arc::new(DesktopSearch {
            engine: crate::web_search::SearchEngine::new(search_settings)
                .map_err(|e| e.to_string())?,
        }))
        .with_web_search_wrapper_enabled(web_search_wrapper_enabled)
        .with_review_settings_source(Arc::new(DesktopReviewSettingsSource {
            state: state.clone(),
        }))
        .with_diagnostic_config_hash(config_hash.clone())
        .with_harness_environment(
            crate::harness::harness_options_from_env(),
            crate::harness::multi_agent::RUNTIME_WIRED,
        );
        if state.eval_recovery_enabled() {
            runtime = runtime.with_task_recovery_policies(
                vellum_proxy_runtime::task_stall::TaskStallPolicy::recover(),
                vellum_proxy_runtime::task_efficiency::TaskEfficiencyPolicy::recover(),
            );
        }
        #[cfg(test)]
        eprintln!(
            "[runtime-new] total {}ms",
            constructed.elapsed().as_millis()
        );
        Ok(Self {
            identity: ProxyRuntimeIdentity {
                install_id: "desktop".into(),
                host_id: "desktop-local".into(),
                image_version: env!("CARGO_PKG_VERSION").into(),
                config_hash,
                ..Default::default()
            }
            .with_process_identity(),
            state,
            credentials,
            runtime: Arc::new(runtime),
            environment,
        })
    }

    pub fn proxy_runtime_handle(&self) -> Arc<ProxyRuntime> {
        Arc::clone(&self.runtime)
    }
}

#[async_trait]
impl ProxyRuntimeState for DesktopProxyRuntimeState {
    fn identity(&self) -> ProxyRuntimeIdentity {
        self.identity.clone()
    }
    fn active_model_routes(&self) -> Vec<ModelRouteView> {
        desktop_model_views(&self.state).unwrap_or_default()
    }
    fn route_for_model(&self, id: &str) -> Option<ModelRouteView> {
        self.active_model_routes()
            .into_iter()
            .find(|route| route.catalog_id == id)
    }
    fn diagnostics(&self) -> RuntimeDiagnostics {
        let snapshot = desktop_catalog_snapshot(&self.state);
        let grok_ready = snapshot
            .as_ref()
            .map(|catalog| grok_responses_readiness(&catalog.routes))
            .unwrap_or_else(|error| Err(error.clone()));
        let persist_error = self.state.grok_rewrite_error();
        let mut notes = Vec::new();
        if let Err(error) = snapshot.as_ref() {
            notes.push(error.clone());
        }
        if let Err(error) = &grok_ready {
            notes.push(error.clone());
        }
        if let Some(error) = persist_error.clone() {
            notes.push(error);
        }
        let config_valid = snapshot.is_ok() && grok_ready.is_ok() && persist_error.is_none();
        RuntimeDiagnostics {
            ok: true,
            ready: config_valid,
            identity: self.identity(),
            model_count: self.active_model_routes().len(),
            listener_ready: true,
            config_valid,
            secrets_readable: true,
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
        self.environment.clone()
    }
}

#[cfg(test)]
thread_local! {
    // Per-thread, not a shared `static`, because cargo test runs each test
    // function on its own thread in parallel; a process-wide counter would
    // pick up builds from unrelated tests running concurrently.
    static DESKTOP_CATALOG_SNAPSHOT_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn desktop_catalog_snapshot(state: &AppState) -> Result<DesktopCatalogSnapshot, String> {
    #[cfg(test)]
    DESKTOP_CATALOG_SNAPSHOT_BUILDS.with(|count| count.set(count.get() + 1));
    let mut all = state.active_model_routes();
    let main_ids = all
        .iter()
        .map(|model| model.catalog_id.clone())
        .collect::<HashSet<_>>();
    let review = state.active_review_model_routes();
    let review_ids = review
        .iter()
        .filter(|model| !main_ids.contains(&model.catalog_id))
        .map(|m| m.catalog_id.clone())
        .collect::<HashSet<_>>();
    all.extend(review);
    let mut seen = HashSet::new();
    all.retain(|m| seen.insert(m.catalog_id.clone()));
    // Resolve each model's route exactly once from the same active-route
    // snapshot. The previous per-model `route_for_active_*` path re-read the
    // official catalog file on every call, which turned a 150-model catalog
    // into ~750ms of file I/O per request.
    let active_routes = state.active_routes();
    let mut desktop_routes = Vec::new();
    for model in &all {
        let Some(route) = active_routes
            .iter()
            .find(|route| route.id == model.route_id)
            .cloned()
        else {
            continue;
        };
        if !desktop_routes
            .iter()
            .any(|r: &crate::model::Route| r.id == route.id)
        {
            desktop_routes.push(route);
        }
    }
    let paths = crate::codex::CodexPaths::discover(&state.data_root());
    let official = crate::catalog::read_official_catalog(&paths.models_cache);
    // Same projection start_proxy writes: publish context capacity for the
    // Desktop meter and Enhanced native local compaction, but no legacy
    // Vellum-authored compact threshold.
    let json = crate::catalog::catalog_json_with_official_and_compaction(
        &desktop_routes,
        official.as_ref(),
        None,
    );
    let entries = json
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            e.get("slug")
                .and_then(Value::as_str)
                .map(|id| (id.into(), e.clone()))
        })
        .collect();
    let routes = all
        .into_iter()
        .map(|model| {
            let route = active_routes
                .iter()
                .find(|route| route.id == model.route_id)
                .expect("route checked");
            runtime_route(state, route, &model)
        })
        .collect();
    Ok(DesktopCatalogSnapshot {
        routes,
        review_ids,
        entries,
    })
}

fn desktop_runtime_config_hash(snapshot: &DesktopCatalogSnapshot) -> String {
    let mut routes = snapshot.routes.clone();
    routes.sort_by(|left, right| {
        left.catalog_id
            .cmp(&right.catalog_id)
            .then(left.route_id.cmp(&right.route_id))
    });
    let mut review_ids = snapshot.review_ids.iter().cloned().collect::<Vec<_>>();
    review_ids.sort();
    let mut entries = snapshot
        .entries
        .iter()
        .map(|(id, entry)| (id.clone(), entry.clone()))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let canonical = serde_json::json!({
        "routes": routes,
        "reviewIds": review_ids,
        "entries": entries,
    });
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    format!("{:x}", Sha256::digest(bytes))
}

fn desktop_model_views(state: &AppState) -> Result<Vec<ModelRouteView>, String> {
    let catalog = desktop_catalog_snapshot(state)?;
    Ok(catalog
        .routes
        .iter()
        .filter(|route| !catalog.review_ids.contains(&route.catalog_id))
        .map(|route| ModelRouteView {
            catalog_id: route.catalog_id.clone(),
            route_id: route.route_id.clone(),
            upstream_model: route.upstream_model.clone(),
            context_window: route.context_window,
            owned_by: Some(route.name.clone()),
        })
        .collect())
}

pub(crate) fn to_runtime_compaction_policy(
    policy: &crate::policy::ResolvedCompactionPolicy,
    grok_threshold_percent: Option<u32>,
) -> RuntimeCompactionPolicy {
    RuntimeCompactionPolicy {
        threshold_percent: policy.threshold_percent,
        output_reserve_tokens: policy.output_reserve_tokens,
        tool_reserve_tokens: policy.tool_reserve_tokens,
        grok_threshold_percent,
    }
}

pub(crate) fn runtime_route(
    state: &AppState,
    route: &crate::model::Route,
    model: &crate::model::ModelRoute,
) -> RuntimeModelRoute {
    let grok_cli = route.provider_kind == ProviderKind::GrokCli;
    let cap = route
        .model_capabilities
        .iter()
        .find(|c| c.model.eq_ignore_ascii_case(&model.upstream_model));
    let context = crate::model::ContextCapabilities::resolve(
        route.provider_kind,
        &model.upstream_model,
        cap,
        model.context_window,
    );
    let policy = state.resolve_compaction_policy(route, &model.upstream_model, false);
    let provider_profile = (route.provider_kind == ProviderKind::OpenAiCompatible)
        .then(|| vellum_proxy_runtime::infer_provider_profile(&route.base_url))
        .flatten();
    RuntimeModelRoute {
        route_id: route.id.clone(),
        catalog_id: model.catalog_id.clone(),
        name: route.name.clone(),
        base_url: if grok_cli {
            "https://cli-chat-proxy.grok.com/v1".into()
        } else {
            route.base_url.clone()
        },
        provider_kind: route.provider_kind.into(),
        auth_kind: route.auth_kind.into(),
        wire: projected_wire(route.provider_kind.into(), model.wire.into()),
        server_side_resume: route.server_side_resume,
        streaming: model.streaming,
        reasoning: model.reasoning,
        vision: model.vision,
        upstream_model: model.upstream_model.clone(),
        context_window: model.context_window,
        reasoning_capabilities: RuntimeReasoningCapabilities {
            supports_persisted_reasoning: context.supports_persisted_reasoning,
            preserves_reasoning_in_compaction: context.preserves_reasoning_in_compaction,
            reasoning_efforts: model.reasoning_efforts.clone(),
            default_reasoning_effort: model.default_reasoning_effort.clone(),
            reasoning_effort_transport: model.reasoning_effort_transport.into(),
        },
        compaction_capabilities: RuntimeCompactionCapabilities {
            supports_server_side_compaction: context.supports_server_side_compaction,
            supports_standalone_compaction: context.supports_standalone_compaction,
            compact_threshold_tokens: context.compact_threshold_tokens,
        },
        compaction_policy: to_runtime_compaction_policy(
            &policy,
            (route.provider_kind == ProviderKind::GrokCli).then_some(policy.threshold_percent),
        ),
        tool_capabilities: RuntimeToolCapabilities {
            tool_calling: cap.and_then(|c| c.tool_calling).unwrap_or(true),
        },
        credential_id: matches!(route.auth_kind, AuthKind::Bearer | AuthKind::GrokSession)
            .then(|| route.id.clone()),
        insecure_http_policy: route.insecure_http_policy,
        provider_profile,
        access_mode: cap
            .and_then(|capability| capability.access_mode.map(Into::into))
            .or_else(|| {
                provider_profile.map(|profile| {
                    vellum_proxy_runtime::opencode::default_access_mode(
                        profile,
                        &model.upstream_model,
                    )
                })
            }),
        chat_capabilities: cap
            .map(|capability| {
                capability.chat_capabilities.clone().migrate_legacy(
                    model.wire == crate::model::WireFormat::Chat,
                    model.reasoning || capability.reasoning.unwrap_or(false),
                    capability.probe_version,
                )
            })
            .unwrap_or_else(|| {
                vellum_proxy_runtime::RuntimeChatCapabilities::default().migrate_legacy(
                    model.wire == crate::model::WireFormat::Chat,
                    model.reasoning,
                    None,
                )
            }),
    }
}
pub(crate) fn runtime_review_settings(
    s: crate::model::ReviewSettings,
) -> vellum_proxy_runtime::ReviewSettings {
    vellum_proxy_runtime::ReviewSettings {
        on_edit: s.on_edit,
        before_send: s.before_send,
        before_compact: s.before_compact,
        route_id: s.route_id,
        model: s.model,
        policy: s.policy.map(|p| match p {
            ReviewPolicy::Always => vellum_proxy_runtime::ReviewPolicy::Always,
            ReviewPolicy::Failover => vellum_proxy_runtime::ReviewPolicy::Failover,
        }),
        fallback_catalog_id: s.fallback_catalog_id,
        official_account_id: s.official_account_id,
    }
}

pub use vellum_proxy_runtime::{ProxyRuntimeConfig, PROXY_RUNTIME_VERSION};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CreateRouteInput, ProbeResult, WireFormat};

    #[test]
    fn desktop_history_resolves_runtime_compaction_by_id() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let history = DesktopHistory {
            inner: state.history_store(),
        };
        let items = vec![serde_json::json!({
            "type": "message",
            "role": "user",
            "content": "canonical checkpoint"
        })];
        RuntimeHistoryStore::record_compaction(
            &history,
            "resp_source",
            "cmp_vellum_bridge_test",
            1,
            items.clone(),
        )
        .unwrap();
        assert_eq!(
            RuntimeHistoryStore::canonical_items_for_compaction(&history, "cmp_vellum_bridge_test")
                .unwrap(),
            Some(items)
        );
    }

    #[test]
    fn desktop_history_persists_official_continuation_realm() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let history = DesktopHistory {
            inner: state.history_store(),
        };
        let request = serde_json::json!({
            "model": "vellum-official",
            "input": [{"role": "user", "content": "hello"}]
        });
        let response = serde_json::json!({
            "id": "resp_realm_test",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "hi"}]
            }]
        });

        assert!(RuntimeHistoryStore::record_exchange_with_context(
            &history,
            &request,
            &response,
            "official",
            Some("codex:test-session:test-thread"),
            Some("opaque-realm-fingerprint"),
        )
        .unwrap());
        assert_eq!(
            RuntimeHistoryStore::response_continuation_realm(&history, "resp_realm_test")
                .unwrap()
                .as_deref(),
            Some("opaque-realm-fingerprint")
        );
        let chain = RuntimeHistoryStore::get_chain(&history, "resp_realm_test").unwrap();
        assert_eq!(
            chain[0].continuation_realm.as_deref(),
            Some("opaque-realm-fingerprint")
        );
    }

    #[test]
    fn desktop_history_journal_record_for_compaction_and_standalone_replay_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let history = DesktopHistory {
            inner: state.history_store(),
        };

        // 1. Save canonical Gen 2 journal via record_compaction_record:
        // response_id is resp_post_4, compaction_id is cmp_gen2, generation is 2.
        let record = vellum_proxy_runtime::history::CompactionJournalRecord {
            engine_id: "codex_local_v0_150".to_string(),
            engine_provenance: None,
            route_id: String::new(),
            upstream_model: String::new(),
            tokens_before: 0,
            tokens_after: 0,
            elapsed_ms: 0,
            response_id: "resp_post_4".to_string(),
            compaction_id: "cmp_gen2".to_string(),
            generation: 2,
            schema_version: 2,
            source_items: vec![
                serde_json::json!({"type": "message", "role": "user", "content": "hello"}),
            ],
            canonical_items: vec![
                serde_json::json!({
                    "type": "message",
                    "role": "assistant",
                    "content": "summary gen 2",
                    "metadata": {
                        "checkpoint": {
                            "generation": 2,
                            "schema_version": 2
                        }
                    }
                }),
                serde_json::json!({
                    "type": "function_call",
                    "call_id": "call_patch_1",
                    "name": "apply_patch"
                }),
            ],
            source_hash: "hash_src".to_string(),
            checkpoint_hash: "hash_cp".to_string(),
            provider_owned: false,
        };
        RuntimeHistoryStore::record_compaction_record(&history, record).unwrap();

        // 2. DesktopHistory.journal_record_for_compaction("cmp_gen2") MUST resolve:
        let journal = RuntimeHistoryStore::journal_record_for_compaction(&history, "cmp_gen2")
            .unwrap()
            .expect("DesktopHistory must resolve journal_record_for_compaction for cmp_gen2");
        assert_eq!(journal.compaction_id, "cmp_gen2");
        assert_eq!(journal.generation, 2);
        assert_eq!(journal.schema_version, 2);
        assert_eq!(journal.canonical_items.len(), 2);
        assert_eq!(journal.response_id, "resp_post_4");

        // 3. Record a post-Gen2 exchange whose previous_response_id links directly to cmp_gen2:
        let req_after = serde_json::json!({
            "previous_response_id": "cmp_gen2",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_search_5",
                    "name": "exec_command",
                    "arguments": "{\"command\": \"Select-String -Pattern CIPHERTEXT_RECOVERY_EXACT -Path .\"}"
                }
            ]
        });
        let resp_after = serde_json::json!({
            "id": "resp_post_5",
            "output": [
                {
                    "type": "function_call_output",
                    "call_id": "call_search_5",
                    "output": "",
                    "status": 0
                }
            ]
        });
        assert!(RuntimeHistoryStore::record_exchange(
            &history,
            &req_after,
            &resp_after,
            "vlm-test"
        )
        .unwrap());

        // 4. DesktopHistory.compacted_replay_with_provenance("resp_post_5") MUST resolve
        // the Gen 2 canonical prefix and identify parent_compaction_id as cmp_gen2:
        let replay = RuntimeHistoryStore::compacted_replay_with_provenance(&history, "resp_post_5")
            .unwrap()
            .expect(
                "DesktopHistory must resolve compacted_replay_with_provenance across Gen2->Gen3",
            );
        assert_eq!(replay.parent_compaction_id.as_deref(), Some("cmp_gen2"));
        assert_eq!(replay.canonical_prefix_len, 2);
        // 2 canonical items + 1 req item + 1 resp item = 4 items
        assert_eq!(replay.items.len(), 4);

        // 5. DesktopHistory.compacted_replay_parent("resp_post_5") MUST return cmp_gen2:
        assert_eq!(
            RuntimeHistoryStore::compacted_replay_parent(&history, "resp_post_5").unwrap(),
            Some("cmp_gen2".to_string())
        );

        // 6. DesktopHistory.compacted_replay_items("resp_post_5") MUST match replay.items:
        let replay_items = RuntimeHistoryStore::compacted_replay_items(&history, "resp_post_5")
            .unwrap()
            .expect("DesktopHistory must resolve compacted_replay_items");
        assert_eq!(replay_items.len(), 4);
    }

    #[test]
    fn e806_route_runtime_config_is_bearer_chat_and_stateless() {
        // e806 is a remote OpenAI-compatible API. Its runtime route must keep
        // `openAiCompatible + bearer + chat` with `server_side_resume=false`
        // and a credential reference equal to the immutable route id `cc`.
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
                api_key: Some("sk-e806-redacted".into()),
                models: Some(vec!["qwen3.6".into()]),
                selected_models: Some(vec!["qwen3.6".into()]),
                context_window: Some(131_072),
                model_capabilities: Vec::new(),
                catalog_scope: None,
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::Bearer,
        );
        let route = routes
            .iter()
            .find(|route| route.id == "cc")
            .expect("route id must stay a slug of the name");
        let model = state
            .model_routes()
            .into_iter()
            .find(|model| model.route_id == route.id)
            .expect("created model route");
        let runtime = runtime_route(&state, route, &model);
        assert_eq!(
            runtime.provider_kind,
            vellum_proxy_runtime::route::RuntimeProviderKind::OpenAiCompatible
        );
        assert_eq!(
            runtime.auth_kind,
            vellum_proxy_runtime::route::RuntimeAuthKind::Bearer
        );
        assert_eq!(
            runtime.wire,
            vellum_proxy_runtime::route::RuntimeWireFormat::Chat
        );
        assert!(!runtime.server_side_resume, "e806 must be stateless");
        assert_eq!(runtime.credential_id.as_deref(), Some("cc"));
        assert_eq!(runtime.base_url, "https://api.provider.example/v1");
        assert_eq!(runtime.upstream_model, "qwen3.6");
        assert_eq!(
            vellum_proxy_runtime::compaction_engine::resolve_compaction_engine(&runtime),
            vellum_proxy_runtime::compaction_engine::ResolvedCompactionEngine::CodexLocalV0150,
            "production third-party routes must use the embedded Codex 0.150 engine"
        );
    }

    #[test]
    fn production_official_route_remains_provider_native() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let official = state
            .routes()
            .into_iter()
            .find(|route| route.provider_kind == ProviderKind::Official)
            .expect("seeded Official route");
        let model = state
            .model_routes()
            .into_iter()
            .find(|model| model.route_id == official.id)
            .expect("seeded Official model");

        let runtime = runtime_route(&state, &official, &model);

        // Official compacts natively upstream and must never resolve to a
        // Vellum-local engine.
        let engine = vellum_proxy_runtime::compaction_engine::resolve_compaction_engine(&runtime);
        assert_eq!(
            engine,
            vellum_proxy_runtime::compaction_engine::ResolvedCompactionEngine::OfficialNative
        );
        assert!(!engine.is_vellum_local());
    }

    #[test]
    fn desktop_runtime_catalog_observes_hot_refreshed_model_selection() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        state.create_route(
            CreateRouteInput {
                name: "cc".into(),
                base_url: "https://api.provider.example/v1".into(),
                model: "qwen-35b".into(),
                wire: WireFormat::Responses,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(ProviderKind::OpenAiCompatible),
                api_key: Some("sk-test-redacted".into()),
                models: Some(vec!["qwen-35b".into()]),
                selected_models: Some(vec!["qwen-35b".into()]),
                context_window: Some(131_072),
                model_capabilities: Vec::new(),
                catalog_scope: None,
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::Bearer,
        );
        state.activate_proxy_routes();
        let catalog = DesktopCatalog {
            state: state.clone(),
        };
        let muse_catalog_id = crate::catalog::stable_catalog_id("cc", "muse-glimmer:latest");
        let initial = catalog
            .active_models()
            .into_iter()
            .filter(|route| route.route_id == "cc")
            .collect::<Vec<_>>();
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].upstream_model, "qwen-35b");
        assert!(catalog.resolve_model(&muse_catalog_id).is_none());

        assert!(state.replace_route_probe_result(
            "cc",
            &ProbeResult {
                reachable: true,
                wire: Some(WireFormat::Responses),
                models: vec!["qwen-35b".into(), "muse-glimmer:latest".into()],
                context_window: Some(131_072),
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                model_capabilities: Vec::new(),
                stream_quality: None,
                needs_input: Vec::new(),
            },
        ));
        assert!(state.set_route_models("cc", vec!["muse-glimmer:latest".into()]));
        state.refresh_active_route_models("cc");

        let active = catalog
            .resolve_model(&muse_catalog_id)
            .expect("the already-running runtime catalog must see the new model")
            .route;
        assert_eq!(active.upstream_model, "muse-glimmer:latest");
        assert_eq!(
            active.wire,
            vellum_proxy_runtime::route::RuntimeWireFormat::Responses
        );
    }

    #[test]
    fn desktop_route_entries_builds_the_snapshot_once_per_request_not_once_per_model() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        state.create_route(
            CreateRouteInput {
                name: "cc1".into(),
                base_url: "https://api.provider.example/v1".into(),
                model: "qwen-35b".into(),
                wire: WireFormat::Responses,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(ProviderKind::OpenAiCompatible),
                api_key: Some("sk-test-redacted-1".into()),
                models: Some(vec!["qwen-35b".into()]),
                selected_models: Some(vec!["qwen-35b".into()]),
                context_window: Some(131_072),
                model_capabilities: Vec::new(),
                catalog_scope: None,
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::Bearer,
        );
        state.create_route(
            CreateRouteInput {
                name: "cc2".into(),
                base_url: "https://backup.provider.example/v1".into(),
                model: "qwen-70b".into(),
                wire: WireFormat::Responses,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(ProviderKind::OpenAiCompatible),
                api_key: Some("sk-test-redacted-2".into()),
                models: Some(vec!["qwen-70b".into()]),
                selected_models: Some(vec!["qwen-70b".into()]),
                context_window: Some(131_072),
                model_capabilities: Vec::new(),
                catalog_scope: None,
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::Bearer,
        );
        state.activate_proxy_routes();
        let catalog = DesktopCatalog {
            state: state.clone(),
        };

        DESKTOP_CATALOG_SNAPSHOT_BUILDS.with(|count| count.set(0));
        let entries = catalog.route_entries();
        let seen = entries
            .iter()
            .filter(|(route, _)| route.route_id == "cc1" || route.route_id == "cc2")
            .count();
        assert_eq!(
            seen, 2,
            "both models must resolve from the same pass: {entries:?}"
        );
        assert_eq!(
            DESKTOP_CATALOG_SNAPSHOT_BUILDS.with(|count| count.get()),
            1,
            "route_entries must build the Desktop catalog snapshot once per request, \
             not once per model"
        );
    }

    #[test]
    fn desktop_catalog_snapshot_freezes_an_in_flight_request_and_refreshes_the_next_one() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        state.create_route(
            CreateRouteInput {
                name: "cc".into(),
                base_url: "https://api.provider.example/v1".into(),
                model: "qwen-35b".into(),
                wire: WireFormat::Responses,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(ProviderKind::OpenAiCompatible),
                api_key: Some("sk-test-redacted".into()),
                models: Some(vec!["qwen-35b".into()]),
                selected_models: Some(vec!["qwen-35b".into()]),
                context_window: Some(131_072),
                model_capabilities: Vec::new(),
                catalog_scope: None,
            },
            ProviderKind::OpenAiCompatible,
            AuthKind::Bearer,
        );
        state.activate_proxy_routes();
        let catalog = DesktopCatalog {
            state: state.clone(),
        };
        let muse_catalog_id = crate::catalog::stable_catalog_id("cc", "muse-glimmer:latest");

        // The equivalent of ProxyRuntime::execute's `RuntimeSnapshot::capture_from`
        // call at the start of a request.
        let in_flight = vellum_proxy_runtime::RuntimeSnapshot::capture_from(&catalog);
        assert!(in_flight.resolve_model(&muse_catalog_id).is_none());

        assert!(state.replace_route_probe_result(
            "cc",
            &ProbeResult {
                reachable: true,
                wire: Some(WireFormat::Responses),
                models: vec!["qwen-35b".into(), "muse-glimmer:latest".into()],
                context_window: Some(131_072),
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                model_capabilities: Vec::new(),
                stream_quality: None,
                needs_input: Vec::new(),
            },
        ));
        assert!(state.set_route_models("cc", vec!["muse-glimmer:latest".into()]));
        state.refresh_active_route_models("cc");

        assert!(
            in_flight.resolve_model(&muse_catalog_id).is_none(),
            "a snapshot already captured for an in-flight request must not see a \
             concurrent catalog change"
        );

        let next = vellum_proxy_runtime::RuntimeSnapshot::capture_from(&catalog);
        assert!(
            next.resolve_model(&muse_catalog_id).is_some(),
            "the next request's fresh capture must observe the hot refresh"
        );
    }

    #[test]
    fn desktop_config_hash_covers_the_live_catalog_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        state.activate_proxy_routes();
        let snapshot = desktop_catalog_snapshot(&state).unwrap();
        assert!(
            !snapshot.routes.is_empty(),
            "activated seed routes must appear in the Desktop snapshot"
        );
        let first_hash = desktop_runtime_config_hash(&snapshot);
        let mut mutated = snapshot;
        mutated.routes[0].upstream_model = "mutated-upstream".into();
        let second_hash = desktop_runtime_config_hash(&mutated);
        assert_ne!(first_hash, second_hash);
        assert_eq!(first_hash.len(), 64);

        let desktop = DesktopProxyRuntimeState::new(state).unwrap();
        assert_ne!(desktop.identity().config_hash, "desktop-live-snapshot");
        assert_eq!(desktop.identity().config_hash, first_hash);
        assert_eq!(desktop.runtime.diagnostic_config_hash(), first_hash);
    }

    #[test]
    fn desktop_projects_grok_as_responses_and_fails_closed_on_persist_error() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let grok = state
            .routes()
            .into_iter()
            .find(|route| route.provider_kind == ProviderKind::GrokCli)
            .expect("seeded Grok route");
        let model = state
            .model_routes()
            .into_iter()
            .find(|model| model.route_id == grok.id)
            .expect("seeded Grok model");
        let runtime = runtime_route(&state, &grok, &model);
        assert_eq!(
            runtime.wire,
            vellum_proxy_runtime::route::RuntimeWireFormat::Responses
        );

        state.set_grok_rewrite_error(Some(
            "Grok Responses rewrite could not be persisted: disk full".into(),
        ));
        let desktop = DesktopProxyRuntimeState::new(state).unwrap();
        let diagnostics = desktop.diagnostics();
        assert!(!diagnostics.ready);
        assert!(!diagnostics.config_valid);
        assert!(
            diagnostics
                .notes
                .iter()
                .any(|note| note.contains("could not be persisted")),
            "{:?}",
            diagnostics.notes
        );
    }

    #[test]
    fn grok_structured_secret_includes_account_home_metadata() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("version.json"), r#"{"version":"1.0.4"}"#).unwrap();
        std::fs::write(temp.path().join("agent_id"), "agent-live-test\n").unwrap();
        let secret: Value = serde_json::from_str(&grok_structured_secret(
            temp.path(),
            "tok-live",
            Some("user-live"),
        ))
        .unwrap();
        assert_eq!(secret["access_token"], "tok-live");
        assert_eq!(secret["client_version"], "1.0.4");
        assert_eq!(secret["agent_id"], "agent-live-test");
        assert_eq!(secret["user_id"], "user-live");
    }

    fn live_visible_text(response: &Value) -> String {
        if let Some(text) = response.get("output_text").and_then(Value::as_str) {
            if !text.trim().is_empty() {
                return text.to_string();
            }
        }
        response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| item.get("content"))
            .flat_map(|content| match content {
                Value::String(text) => vec![text.clone()],
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|part| {
                        part.get("text")
                            .or_else(|| part.get("input_text"))
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .collect(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>()
            .join("")
    }

    #[allow(dead_code)]
    fn live_runtime_request(
        model: &str,
        marker: &str,
        stream: bool,
        max_output_tokens: Option<u64>,
    ) -> vellum_proxy_runtime::RuntimeRequest {
        let mut body = serde_json::json!({
            "model": model,
            "stream": stream,
            "store": false,
            "tool_choice": "none",
            "tools": [],
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": format!("Reply with exactly {marker} and nothing else.")
                }]
            }]
        });
        if let Some(limit) = max_output_tokens {
            body["max_output_tokens"] = serde_json::json!(limit);
        }
        vellum_proxy_runtime::RuntimeRequest {
            body,
            endpoint: vellum_proxy_runtime::RuntimeEndpoint::Responses,
            incoming_auth: vellum_proxy_runtime::IncomingAuthContext::default(),
            execution_environment: vellum_proxy_runtime::ExecutionEnvironment::posix_reference(),
            metadata: vellum_proxy_runtime::RequestMetadata {
                request_id: format!("req_live_{model}"),
                received_at_ms: 0,
                review_run_id: None,
                review_role: None,
                primary_failure_reason: None,
                connection_id: None,
                ..Default::default()
            },
        }
    }

    fn live_redact(value: &str) -> String {
        vellum_proxy_runtime::hash_text(value)
    }

    fn copy_if_exists(src: &std::path::Path, dst: &std::path::Path) {
        if !src.is_file() {
            return;
        }
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::copy(src, dst);
    }

    fn stage_live_desktop_state() -> (tempfile::TempDir, AppState) {
        let src = dirs::data_local_dir()
            .expect("local app data")
            .join("vellum");
        let temp = tempfile::tempdir().unwrap();
        let dst = temp.path();
        copy_if_exists(&src.join("settings.json"), &dst.join("settings.json"));
        copy_if_exists(&src.join("vellum-key.dpapi"), &dst.join("vellum-key.dpapi"));
        copy_if_exists(
            &src.join("vellum-key.protection"),
            &dst.join("vellum-key.protection"),
        );
        copy_if_exists(
            &src.join("codex_oauth_accounts.json"),
            &dst.join("codex_oauth_accounts.json"),
        );
        copy_if_exists(
            &src.join("grok_accounts").join("accounts.json"),
            &dst.join("grok_accounts").join("accounts.json"),
        );
        let cred_src = src.join("credentials");
        if cred_src.is_dir() {
            let cred_dst = dst.join("credentials");
            std::fs::create_dir_all(&cred_dst).unwrap();
            for entry in std::fs::read_dir(cred_src).unwrap() {
                let entry = entry.unwrap();
                if entry.path().is_file() {
                    let _ = std::fs::copy(entry.path(), cred_dst.join(entry.file_name()));
                }
            }
        }
        let state = AppState::with_data_dir(dst.to_path_buf());
        state.activate_proxy_routes();
        (temp, state)
    }

    async fn collect_ws_turn(
        client: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        deadline: std::time::Duration,
    ) -> Result<(std::time::Duration, String), String> {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;
        let started = std::time::Instant::now();
        let mut first_frame = None;
        let mut delta = String::new();
        let wait = tokio::time::timeout(deadline, async {
            while let Some(message) = client.next().await {
                let message = message.map_err(|error| error.to_string())?;
                let text = match message {
                    Message::Text(text) => text.to_string(),
                    Message::Binary(bytes) => {
                        String::from_utf8(bytes.to_vec()).map_err(|error| error.to_string())?
                    }
                    Message::Ping(_) | Message::Pong(_) => continue,
                    Message::Close(_) => return Err("upstream closed".into()),
                    Message::Frame(_) => continue,
                };
                first_frame.get_or_insert(started.elapsed());
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    if let Some(kind) = value.get("type").and_then(Value::as_str) {
                        eprintln!("official ws frame type={kind}");
                    }
                    if value.get("type").and_then(Value::as_str)
                        == Some("response.output_text.delta")
                    {
                        if let Some(piece) = value.get("delta").and_then(Value::as_str) {
                            delta.push_str(piece);
                        }
                    }
                    if value.get("type").and_then(Value::as_str) == Some("response.completed") {
                        let text = value
                            .get("response")
                            .map(live_visible_text)
                            .unwrap_or_default();
                        let visible = if text.trim().is_empty() { delta } else { text };
                        return Ok((first_frame.unwrap_or(started.elapsed()), visible));
                    }
                    if value.get("type").and_then(Value::as_str) == Some("response.failed") {
                        return Err(value
                            .pointer("/response/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("response.failed")
                            .to_string());
                    }
                }
            }
            Err("socket ended before response.completed".into())
        })
        .await
        .map_err(|_| "timed out waiting for Official WebSocket turn".to_string())?;
        wait
    }

    fn official_create_frame(model: &str, marker: &str) -> String {
        vellum_proxy_runtime::official_live_create_body(
            model,
            &vellum_proxy_runtime::official_live_marker_input(marker),
        )
        .to_string()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn live_vellum_grok_and_official_accounts() {
        if std::env::var("VELLUM_LIVE_VELLUM_ACCOUNTS").ok().as_deref() != Some("1") {
            eprintln!("skip live Grok/Official: VELLUM_LIVE_VELLUM_ACCOUNTS is unset");
            return;
        }
        let (_temp, state) = stage_live_desktop_state();
        let mut review = state.review_settings();
        review.before_send = false;
        review.on_edit = false;
        review.before_compact = false;
        state.set_review_settings(review).unwrap();
        let desktop = DesktopProxyRuntimeState::new(state.clone()).unwrap();
        let routes = desktop.active_model_routes();
        let grok = routes
            .iter()
            .find(|route| route.route_id == "grok-cli")
            .unwrap_or_else(|| panic!("no Grok catalog route in {routes:?}"));
        let official = routes
            .iter()
            .find(|route| {
                route.route_id == "openai-official"
                    && vellum_proxy_runtime::official_catalog_is_luna(&route.catalog_id)
            })
            .or_else(|| {
                routes.iter().find(|route| {
                    route.route_id == "openai-official"
                        && vellum_proxy_runtime::official_catalog_is_luna(&route.upstream_model)
                })
            })
            .unwrap_or_else(|| panic!("no Official Luna catalog route in {routes:?}"));

        let secret = desktop
            .credentials()
            .await
            .get_secret("grok-cli")
            .await
            .expect("DesktopCredentials lookup")
            .expect("DesktopCredentials must return a Grok secret");
        let parsed: Value = serde_json::from_str(&secret).expect("DesktopCredentials Grok JSON");
        assert!(
            parsed["client_version"]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
            "DesktopCredentials must include client_version"
        );
        assert!(
            parsed["access_token"]
                .as_str()
                .is_some_and(|value| value.len() > 20),
            "DesktopCredentials must include an access token"
        );
        eprintln!(
            "grok desktop_secret client_version_set=true user_id_set={} token_len={} catalog={}",
            parsed["user_id"].as_str().is_some(),
            parsed["access_token"].as_str().map(str::len).unwrap_or(0),
            grok.catalog_id
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let boundary = vellum_proxy_runtime::BoundaryKey::generate().unwrap();
        let app = vellum_proxy_runtime::build_headless_router(
            std::sync::Arc::new(desktop.clone()),
            vellum_proxy_runtime::InboundAccessPolicy::authenticated(
                vellum_proxy_runtime::BOUNDARY_CREDENTIAL_ID,
                boundary.clone(),
                addr.port(),
            ),
        );
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let ready = reqwest::Client::new()
            .get(format!("http://{addr}/readyz"))
            .header(
                vellum_proxy_runtime::BOUNDARY_KEY_HEADER,
                boundary.expose_for_storage(),
            )
            .send()
            .await
            .expect("readyz through Desktop router");
        assert!(
            ready.status().is_success(),
            "Desktop router /readyz failed: {}",
            ready.status()
        );
        eprintln!("desktop router ready");

        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::Message;
        let mut request = format!("ws://{addr}/v1/responses")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            vellum_proxy_runtime::BOUNDARY_KEY_HEADER,
            boundary.expose_for_storage().parse().unwrap(),
        );
        request
            .headers_mut()
            .insert("openai-beta", "responses=experimental".parse().unwrap());
        request
            .headers_mut()
            .insert("x-codex-turn-metadata", "live-smoke".parse().unwrap());
        let (mut client, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        eprintln!("desktop GET /v1/responses upgraded");
        client
            .send(Message::Text(
                official_create_frame(&official.catalog_id, "VELLUM_GPT_WS_1").into(),
            ))
            .await
            .unwrap();
        let official_ws = collect_ws_turn(&mut client, std::time::Duration::from_secs(20)).await;
        let first_frame = match &official_ws {
            Ok((first_ttft, _)) => vellum_proxy_runtime::evaluate_first_frame(
                vellum_proxy_runtime::FirstFrameObservation::Observed {
                    elapsed_ms: first_ttft.as_millis() as u64,
                },
            ),
            Err(_) => vellum_proxy_runtime::evaluate_first_frame(
                vellum_proxy_runtime::FirstFrameObservation::TimedOut {
                    deadline_ms: 20_000,
                },
            ),
        };
        first_frame.expect("Official first-frame timeout is a failure, not a skipped PASS");
        let (first_ttft, first_text) = official_ws.expect("official first WS turn");
        assert!(
            first_text.contains("VELLUM_GPT_WS_1"),
            "official first WS turn missing marker in {first_text}"
        );
        client
            .send(Message::Text(
                official_create_frame(&official.catalog_id, "VELLUM_GPT_WS_2").into(),
            ))
            .await
            .unwrap();
        let (_second_ttft, second_text) =
            collect_ws_turn(&mut client, std::time::Duration::from_secs(45))
                .await
                .expect("official second WS turn");
        assert!(
            second_text.contains("VELLUM_GPT_WS_2"),
            "official second WS turn missing marker in {second_text}"
        );
        let records = desktop.proxy_runtime().usage_records().unwrap();
        let official_rows: Vec<_> = records
            .iter()
            .filter(|record| record.route_id == official.route_id)
            .collect();
        assert!(
            official_rows.len() >= 2,
            "official WS must record both turns: {official_rows:?}"
        );
        let connection_ids: Vec<_> = official_rows
            .iter()
            .filter_map(|record| record.connection_id.as_deref())
            .collect();
        assert!(
            connection_ids.len() >= 2 && connection_ids.windows(2).all(|pair| pair[0] == pair[1]),
            "official turns must share one connection_id: {connection_ids:?}"
        );
        eprintln!(
            "official native ws reuse connection={} turns={} proxy_first_ttft_ms={}",
            live_redact(connection_ids[0]),
            official_rows.len(),
            first_ttft.as_millis()
        );
        let _ = client.close(None).await;

        // Direct Official WSS always runs, even if the proxy turns above failed.
        if let Ok(Some(auth)) = state.codex_oauth().valid_default_auth().await {
            let mut direct = "wss://chatgpt.com/backend-api/codex/responses"
                .into_client_request()
                .unwrap();
            direct.headers_mut().insert(
                "authorization",
                format!("Bearer {}", auth.access_token).parse().unwrap(),
            );
            direct
                .headers_mut()
                .insert("chatgpt-account-id", auth.account_id.parse().unwrap());
            direct
                .headers_mut()
                .insert("openai-beta", "responses=experimental".parse().unwrap());
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                tokio_tungstenite::connect_async(direct),
            )
            .await
            {
                Ok(Ok((mut upstream, _))) => {
                    let _ = upstream
                        .send(Message::Text(
                            official_create_frame(
                                vellum_proxy_runtime::PINNED_OFFICIAL_MODEL,
                                "VELLUM_GPT_DIRECT",
                            )
                            .into(),
                        ))
                        .await;
                    match collect_ws_turn(&mut upstream, std::time::Duration::from_secs(30)).await {
                        Ok((direct_ttft, _)) => {
                            eprintln!(
                                "official ws direct_ttft_ms={} proxy_minus_direct_ms={}",
                                direct_ttft.as_millis(),
                                first_ttft
                                    .as_millis()
                                    .saturating_sub(direct_ttft.as_millis())
                            );
                        }
                        Err(error) => {
                            let _ = vellum_proxy_runtime::evaluate_first_frame(
                                vellum_proxy_runtime::FirstFrameObservation::TimedOut {
                                    deadline_ms: 30_000,
                                },
                            );
                            panic!(
                                "official direct WSS first-frame failed: {}",
                                live_redact(&error)
                            );
                        }
                    }
                    let _ = upstream.close(None).await;
                }
                Ok(Err(error)) => {
                    panic!("official direct WSS connect failed: {error}");
                }
                Err(_) => panic!("official direct WSS connect timed out"),
            }
        }

        server.abort();
    }
}
