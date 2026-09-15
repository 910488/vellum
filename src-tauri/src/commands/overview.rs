//! 概覽指令（doc/01）。
//!
//! quota 欄位已接線（doc/08）：透過 ACP 查 Grok 週/月額度。
//! ACP 查詢走 stdio、阻塞 2–3 秒，所以指令是 async + spawn_blocking。
//! 前端全是 async/await，同步 → async 不影響 UI。

use crate::budget::{grok_model_cache_window, resolve, BudgetInputs};
use crate::error::{AppError, AppResult};
use crate::history::DEFAULT_RETENTION_DAYS;
use crate::model::*;
use crate::state::AppState;
use tauri::State;

#[tauri::command]
pub fn list_routes(state: State<'_, AppState>) -> AppResult<Vec<Route>> {
    Ok(state.routes())
}

#[tauri::command(rename_all = "camelCase")]
pub fn select_route(route_id: String, state: State<'_, AppState>) -> AppResult<()> {
    if state.select_route(&route_id) {
        Ok(())
    } else {
        Err(AppError::RouteNotFound(route_id))
    }
}

/// doc/03：從探測結果建立新路線並設為使用中。回傳更新後的完整線路清單。
#[tauri::command(rename_all = "camelCase")]
pub fn create_route(input: CreateRouteInput, state: State<'_, AppState>) -> AppResult<Vec<Route>> {
    create_route_inner(input, &state)
}

fn create_route_inner(mut input: CreateRouteInput, state: &AppState) -> AppResult<Vec<Route>> {
    let provider_kind = input
        .provider_kind
        .unwrap_or(ProviderKind::OpenAiCompatible);
    if provider_kind == ProviderKind::OpenAiCompatible {
        input.base_url = crate::probe::normalize_provider_base_url(&input.base_url);
    }
    let auth_kind = match provider_kind {
        ProviderKind::Official => AuthKind::ChatGpt,
        ProviderKind::GrokCli => AuthKind::GrokSession,
        ProviderKind::OpenAiCompatible
            if input
                .api_key
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()) =>
        {
            AuthKind::Bearer
        }
        ProviderKind::OpenAiCompatible => AuthKind::None,
    };
    let api_key = input
        .api_key
        .clone()
        .filter(|value| !value.trim().is_empty());
    let previous_current = state.current_route().map(|route| route.id);
    let routes = state.create_route(input, provider_kind, auth_kind);
    if let (Some(route), Some(secret)) = (routes.iter().find(|route| route.is_current), api_key) {
        if let Err(error) = crate::credentials::save(&state.data_root(), &route.id, secret.trim()) {
            let _ = state.delete_route(&route.id);
            if let Some(previous) = previous_current {
                let _ = state.select_route(&previous);
            }
            return Err(error);
        }
    }
    refresh_catalog_if_running(state)?;
    Ok(routes)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn get_request_log(
    limit: Option<u32>,
    offset: Option<u32>,
    failed_only: Option<bool>,
    route_id: Option<String>,
    focus_entry_id: Option<i64>,
    state: State<'_, AppState>,
) -> AppResult<RequestLog> {
    let owned = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let event_limit = 500;
        let mut log = owned
            .usage_store()
            .request_log_page(&crate::usage::RequestLogPage {
                limit: limit.unwrap_or(20),
                offset: offset.unwrap_or(0),
                failed_only: failed_only.unwrap_or(false),
                route_id,
                event_limit,
                focus_entry_id,
            })?;
        let names = owned.route_display_names();
        for entry in &mut log.entries {
            resolve_provider_name(&names, &entry.route_id, &mut entry.provider);
        }
        for route in &mut log.entry_routes {
            resolve_provider_name(&names, &route.route_id, &mut route.provider);
        }
        log.compaction_events.extend(
            crate::codex::read_recent_compaction_events(event_limit as usize)
                .into_iter()
                .map(|event| crate::model::CompactionLogEntry {
                    id: event.event_id,
                    created_at: event.created_at,
                    engine: "codex_client".into(),
                    outcome: "compacted".into(),
                    reason: event.label,
                    tokens_before: event.tokens_before,
                    tokens_after: event.tokens_after,
                    source_model_visible_tokens: event.tokens_before,
                    replacement_model_visible_tokens: event.tokens_after,
                    replacement_durable_tokens: event.tokens_after,
                    quality_outcome: Some("compacted".into()),
                    candidate_generation: None,
                    fallback_reason: None,
                    // Parsed straight out of Codex's own rollout text, not
                    // from a Vellum diagnostic payload — none of the
                    // Canonical/journal detail below has a meaning here.
                    items_before: None,
                    items_after: None,
                    window: None,
                    active_tokens: None,
                    threshold_percent: None,
                    checkpoint_id: None,
                    generation: None,
                }),
        );
        log.compaction_events
            .sort_by_key(|event| std::cmp::Reverse((event.created_at, event.id)));
        log.compaction_events.truncate(event_limit as usize);
        log.compaction_total = log.compaction_events.len() as u32;
        Ok(log)
    })
    .await
    .map_err(|error| AppError::Message(format!("request log task failed: {error}")))?
}

/// Persistent process-boot telemetry so the UI can show whether this
/// desktop process restarted since the last observed failure. Read errors are
/// swallowed to `None` — telemetry must never make the Log page fail.
#[tauri::command]
pub fn get_boot_telemetry(state: State<'_, AppState>) -> Option<crate::boot::BootTelemetry> {
    crate::boot::load(&state.data_root()).ok().flatten()
}

/// 用線路 ID 換回現在的顯示名稱。
///
/// 用量列裡存的名稱是當下的快照 —— 供應商改名（或 Vellum 自己換掉內建
/// 名稱）之後，那些列還是舊名字。線路已經被刪掉時沒有更好的來源，
/// 就留著存下來的那個。
pub fn resolve_provider_name(
    names: &std::collections::HashMap<String, String>,
    route_id: &str,
    provider: &mut String,
) {
    if let Some(name) = names.get(route_id) {
        provider.clone_from(name);
    }
}

#[tauri::command]
pub fn get_sessions(state: State<'_, AppState>) -> AppResult<Vec<SessionStatus>> {
    let routes = state.routes();
    let models = state.model_routes();
    // A bug in one stored row (bad blob, malformed JSON) must not crash the
    // whole sessions list — contain the panic here and report it as a clear
    // history error instead of an opaque crashed command.
    let indexed_sessions = crate::codex::read_session_labels();
    let snapshots = crate::history::catch_history_panic(std::panic::AssertUnwindSafe(|| {
        state.history_store().session_snapshots()
    }))?
    .into_iter()
    // A Codex session can own several internal threads: sub-agents, Guardian
    // reviews, and other delegated work. They need distinct history keys for
    // continuation, but they are not separate conversations the user opened.
    // Filtering before rollout lookup also prevents the root rollout from
    // matching an arbitrary child through its shared session-id alias.
    .filter(|snapshot| is_user_facing_session_key(&snapshot.conversation_key, &indexed_sessions))
    .collect::<Vec<_>>();
    let keys = snapshots
        .iter()
        .map(|snapshot| snapshot.conversation_key.clone())
        .collect::<std::collections::HashSet<_>>();
    let runtime = crate::codex::read_session_runtime(&keys);
    let execution_planes =
        crate::enhanced_runtime::desktop_manager::thread_execution_planes(&state.data_root());
    Ok(snapshots
        .into_iter()
        .filter_map(|snapshot| {
            let route = routes.iter().find(|route| route.id == snapshot.route_id)?;
            let session_runtime = runtime.get(&snapshot.conversation_key);
            let runtime_model = session_runtime
                .and_then(|runtime| runtime.model.as_deref())
                .filter(|model| !model.trim().is_empty());
            let recorded_model = snapshot
                .model
                .as_deref()
                .filter(|model| !model.trim().is_empty());
            let model = models
                .iter()
                .find(|model| {
                    model.route_id == route.id
                        && runtime_model.is_some_and(|name| {
                            model.catalog_id.eq_ignore_ascii_case(name)
                                || model.upstream_model.eq_ignore_ascii_case(name)
                        })
                })
                .or_else(|| {
                    models.iter().find(|model| {
                        model.route_id == route.id
                            && recorded_model
                                .is_some_and(|name| model.upstream_model.eq_ignore_ascii_case(name))
                    })
                });
            let model_name = model
                .map(|model| model.upstream_model.as_str())
                .or(runtime_model)
                .or(recorded_model)
                .unwrap_or(&route.model);
            let (configured_window, compact_threshold_percent) = model
                .map(|model| {
                    let budget =
                        crate::commands::budget::resolve_model_budget(&state, route, model);
                    (budget.effective_window, budget.compact_threshold_percent)
                })
                .unwrap_or((
                    route
                        .context_window
                        .unwrap_or(crate::budget::FALLBACK_TOKENS),
                    crate::budget::DEFAULT_COMPACT_THRESHOLD,
                ));
            // Releases before the Auto Review isolation fix persisted every
            // Guardian assessment as a synthetic response-history session.
            // Such rows have no matching Codex rollout runtime and use the
            // configured reviewer route/model.  Hide only that legacy shape;
            // real Codex sessions still have a rollout runtime entry.
            if is_legacy_review_snapshot(
                &snapshot.route_id,
                model_name,
                session_runtime,
                &state.review_settings(),
            ) {
                return None;
            }
            let window_tokens = session_runtime
                .and_then(|runtime| runtime.window_tokens)
                .unwrap_or(configured_window);
            let estimated_tokens = snapshot
                .payload_bytes
                .div_ceil(crate::history::BYTES_PER_TOKEN as u64)
                .min(window_tokens);
            Some(SessionStatus {
                label: session_runtime.and_then(|runtime| runtime.label.clone()),
                id: snapshot.conversation_key.clone(),
                route_id: route.id.clone(),
                provider: route.name.clone(),
                model: model_name.to_string(),
                used_tokens: session_runtime
                    .and_then(|runtime| runtime.used_tokens)
                    .unwrap_or(estimated_tokens),
                window_tokens,
                compact_threshold_percent,
                last_activity_at: snapshot.last_activity_at,
                core: if route.provider_kind == ProviderKind::Official {
                    Some("official".into())
                } else {
                    execution_planes
                        .get(&snapshot.conversation_key)
                        .map(|plane| match plane {
                            crate::enhanced_runtime::ExecutionPlane::OfficialCodex => "official",
                            crate::enhanced_runtime::ExecutionPlane::EnhancedCodex => "enhanced",
                        })
                        .map(str::to_string)
                },
            })
        })
        .collect())
}

/// User-facing session lists contain root Codex threads, not the internal
/// child threads that execute underneath them.
///
/// Current Codex identities are `codex:<session_id>:<thread_id>`. A root turn
/// normally repeats the same id in both positions; a child keeps the root
/// session id and receives its own thread id. An independently visible fork
/// can also have different ids, so retain it when Codex put that thread in its
/// user-facing session index. Older hashed and foreign keys cannot be
/// classified from their shape, so retain them rather than hiding real data.
fn is_user_facing_session_key(
    key: &str,
    indexed_sessions: &std::collections::HashMap<String, String>,
) -> bool {
    let mut parts = key.trim().split(':');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(prefix), Some(session_id), Some(thread_id), None)
            if prefix.eq_ignore_ascii_case("codex") =>
        {
            session_id.eq_ignore_ascii_case(thread_id)
                || indexed_sessions.contains_key(&thread_id.to_ascii_lowercase())
        }
        _ => true,
    }
}

fn is_legacy_review_snapshot(
    route_id: &str,
    model: &str,
    runtime: Option<&crate::codex::CodexSessionRuntime>,
    review: &ReviewSettings,
) -> bool {
    let has_codex_runtime = runtime.is_some_and(|runtime| {
        runtime.label.is_some() || runtime.used_tokens.is_some() || runtime.window_tokens.is_some()
    });
    !has_codex_runtime
        && !review.route_id.is_empty()
        && route_id == review.route_id
        && model.eq_ignore_ascii_case(&review.model)
}

#[tauri::command]
pub async fn get_usage_activity(state: State<'_, AppState>) -> AppResult<UsageActivity> {
    let official_routes = state
        .routes()
        .into_iter()
        .filter(|route| route.provider_kind == ProviderKind::Official)
        .collect::<Vec<_>>();
    let official_route_ids = official_routes
        .iter()
        .map(|route| route.id.clone())
        .collect::<Vec<_>>();
    let official_name = official_routes
        .first()
        .map(|route| route.name.clone())
        .unwrap_or_else(|| crate::state::OFFICIAL_ROUTE_NAME.into());

    let manager = state.codex_oauth();
    let accounts = manager.status().await.accounts;
    let mut profiles = Vec::with_capacity(accounts.len());
    let mut failed_accounts = 0_usize;
    for account in &accounts {
        match query_managed_profile(&manager, &account.account_id).await {
            Ok(profile) => profiles.push(profile),
            Err(_) => failed_accounts += 1,
        }
    }
    let warning = if accounts.is_empty() {
        Some(RuntimeNotice::new("usageOAuthNotConfigured"))
    } else if failed_accounts > 0 {
        Some(
            RuntimeNotice::new("usageProfileUnavailable")
                .with("count", failed_accounts.to_string()),
        )
    } else {
        None
    };

    // Once the authoritative profile is available, local OpenAI requests must
    // be excluded or every official request would be counted twice.
    let excluded = if profiles.is_empty() {
        &[]
    } else {
        official_route_ids.as_slice()
    };
    let local = state.usage_store().activity(excluded)?;
    let mut activity = build_usage_activity(
        profiles,
        local,
        official_route_ids.first().cloned(),
        official_name,
        warning,
    );
    let names = state.route_display_names();
    for provider in &mut activity.providers {
        resolve_provider_name(&names, &provider.route_id, &mut provider.provider);
    }
    Ok(activity)
}

async fn query_managed_profile(
    manager: &crate::codex_oauth::CodexOAuthManager,
    account_id: &str,
) -> Result<crate::codex_profile::CodexProfileUsage, String> {
    let auth = manager
        .valid_auth_for(account_id)
        .await
        .map_err(|error| error.to_string())?;
    match crate::codex_profile::query(&auth.access_token, &auth.account_id, false).await {
        Ok(profile) => Ok(profile),
        Err(crate::codex_profile::CodexProfileError::Unauthorized) => {
            let refreshed = manager
                .refresh_after_rejection(&auth.credential_id, &auth.access_token)
                .await
                .map_err(|error| error.to_string())?;
            crate::codex_profile::query(&refreshed.access_token, &refreshed.account_id, true)
                .await
                .map_err(|error| error.to_string())
        }
        Err(error) => Err(error.to_string()),
    }
}

fn build_usage_activity(
    profiles: Vec<crate::codex_profile::CodexProfileUsage>,
    local: crate::usage::LocalUsageActivity,
    official_route_id: Option<String>,
    official_name: String,
    warning: Option<RuntimeNotice>,
) -> UsageActivity {
    let mut days = std::collections::BTreeMap::<String, (u64, u64)>::new();
    let mut providers = Vec::new();
    let mut total_tokens = 0_u64;
    let mut longest_task_duration_ms = local.longest_request_ms;
    let mut authoritative_peak = 0_u64;
    let official_source;
    let official_streaks;

    if !profiles.is_empty() {
        official_source = "codex_profile".to_owned();
        let account_count = u32::try_from(profiles.len()).unwrap_or(u32::MAX);
        let mut official_total = 0_u64;
        let mut official_current = 0_u32;
        let mut official_longest = 0_u32;
        for profile in profiles {
            official_total = official_total.saturating_add(profile.lifetime_tokens);
            authoritative_peak = authoritative_peak.max(profile.peak_daily_tokens);
            longest_task_duration_ms =
                longest_task_duration_ms.max(profile.longest_task_duration_ms);
            official_current = official_current.max(profile.current_streak_days);
            official_longest = official_longest.max(profile.longest_streak_days);
            for day in profile.daily_usage {
                let item = days.entry(day.date).or_default();
                item.0 = item.0.saturating_add(day.tokens);
            }
        }
        total_tokens = total_tokens.saturating_add(official_total);
        official_streaks = Some((official_current, official_longest));
        providers.push(UsageProviderTotal {
            route_id: official_route_id.unwrap_or_else(|| "openai-official".into()),
            provider: official_name,
            tokens: official_total,
            account_count,
            source: "codex_profile".into(),
        });
    } else {
        official_source = "proxy_fallback".to_owned();
        official_streaks = None;
    }

    for day in local.days {
        let item = days.entry(day.date).or_default();
        item.0 = item.0.saturating_add(day.tokens);
        item.1 = item.1.saturating_add(day.requests);
    }
    for provider in local.providers {
        total_tokens = total_tokens.saturating_add(provider.tokens);
        providers.push(UsageProviderTotal {
            route_id: provider.route_id,
            provider: provider.provider,
            tokens: provider.tokens,
            account_count: 0,
            source: "proxy".into(),
        });
    }
    providers.sort_by(|left, right| {
        right
            .tokens
            .cmp(&left.tokens)
            .then_with(|| left.provider.cmp(&right.provider))
    });

    let days = days
        .into_iter()
        .map(|(date, (tokens, requests))| UsageActivityDay {
            date,
            tokens,
            requests,
        })
        .collect::<Vec<_>>();
    let peak_tokens = authoritative_peak.max(days.iter().map(|day| day.tokens).max().unwrap_or(0));
    let (computed_current, computed_longest) = usage_streaks(&days);
    let (official_current, official_longest) = official_streaks.unwrap_or((0, 0));
    let current_streak_days = official_current.max(computed_current);
    let longest_streak_days = official_longest.max(computed_longest);

    UsageActivity {
        days,
        total_tokens,
        peak_tokens,
        longest_task_duration_ms,
        current_streak_days,
        longest_streak_days,
        providers,
        official_source,
        warning,
    }
}

fn usage_streaks(days: &[UsageActivityDay]) -> (u32, u32) {
    if days.is_empty() {
        return (0, 0);
    }
    let active = days
        .iter()
        .filter(|day| day.tokens > 0)
        .map(|day| day.date.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut cursor = chrono::Local::now().date_naive();
    let mut current = 0_u32;
    while active.contains(cursor.format("%Y-%m-%d").to_string().as_str()) {
        current = current.saturating_add(1);
        cursor -= chrono::Duration::days(1);
    }
    let mut longest = 0_u32;
    let mut run = 0_u32;
    let mut previous = None;
    for day in days.iter().filter(|day| day.tokens > 0) {
        let Ok(date) = chrono::NaiveDate::parse_from_str(&day.date, "%Y-%m-%d") else {
            continue;
        };
        run = if previous
            .is_some_and(|previous| date.signed_duration_since(previous).num_days() == 1)
        {
            run.saturating_add(1)
        } else {
            1
        };
        longest = longest.max(run);
        previous = Some(date);
    }
    (current, longest)
}

pub(crate) fn refresh_catalog_if_running(state: &AppState) -> AppResult<()> {
    if state.proxy_status().running {
        let paths = crate::codex::CodexPaths::discover(&state.data_root());
        let previous = crate::runtime::snapshot_catalog(&state.data_root(), &paths.catalog)?;
        // Provider add/remove/enable changes are deliberately deferred until
        // the proxy restarts. Publish the same immutable route snapshot the
        // listener is actually serving; publishing `state.routes()` here can
        // expose a freshly configured model in Codex before the runtime can
        // resolve it, producing a misleading `model not configured` failure.
        let active_routes = state.active_routes();
        let active_models = state.active_model_routes();
        // Must stay identical to what start_proxy writes, or a
        // Provider/probe/budget/Grok account change would republish a
        // different catalog than the one the running proxy was started with.
        // Match startup: expose context capacity for the Desktop meter and
        // Enhanced native local compaction, without publishing a legacy
        // Vellum compaction schedule.
        crate::catalog::write_catalog_with_model_routes_and_search_and_compaction(
            &paths.catalog,
            &active_routes,
            &active_models,
            Some(&paths.models_cache),
            state
                .web_search_settings()
                .with_stored_brave_key(&state.data_root())
                .advertised(),
            None,
        )?;
        let current = std::fs::read(&paths.catalog)
            .map_err(|error| AppError::Message(format!("無法讀取更新後的模型型錄：{error}")))?;
        let previous_bytes = previous
            .as_ref()
            .map(|version| std::fs::read(&version.path))
            .transpose()
            .map_err(|error| AppError::Message(format!("read previous catalog: {error}")))?;
        let catalog_changed = previous_bytes.as_deref().is_none_or(|bytes| {
            crate::runtime::catalog_requires_restart(bytes, &current)
        });
        if catalog_changed {
            log::info!(
                "[Restart] catalog contract changed previous={} current={}",
                previous.as_ref().map_or("missing", |version| version.id.as_str()),
                crate::runtime::catalog_id(&current)
            );
            let reason = crate::model::RuntimeNotice::new("routesAndCatalogUpdated");
            if let Some(identity) = crate::commands::runtime::codex_process_identity() {
                state.mark_restart_required_for_process(reason, identity);
            } else {
                state.mark_restart_required(reason);
            }
        }
    }
    Ok(())
}

#[tauri::command(rename_all = "camelCase")]
pub fn set_route_enabled(
    route_id: String,
    enabled: bool,
    state: State<'_, AppState>,
) -> AppResult<Vec<Route>> {
    if !state.set_route_enabled(&route_id, enabled) {
        return Err(AppError::RouteNotFound(route_id));
    }
    refresh_catalog_if_running(&state)?;
    Ok(state.routes())
}

/// Opt a route into plaintext HTTP beyond loopback (see
/// `InsecureHttpPolicy`) — most commonly `allowPrivateNetwork` for a
/// self-hosted OpenAI-compatible server on a LAN or Tailscale address, which
/// the outbound admission check otherwise refuses at request time.
fn set_route_insecure_http_policy_inner(
    state: &AppState,
    route_id: &str,
    policy: InsecureHttpPolicy,
) -> AppResult<Vec<Route>> {
    if !state.set_route_insecure_http_policy(route_id, policy) {
        return Err(AppError::RouteNotFound(route_id.to_string()));
    }
    if state.proxy_status().running {
        state.refresh_active_route_models(route_id);
    }
    refresh_catalog_if_running(state)?;
    Ok(state.routes())
}

#[tauri::command(rename_all = "camelCase")]
pub fn set_route_insecure_http_policy(
    route_id: String,
    policy: InsecureHttpPolicy,
    state: State<'_, AppState>,
) -> AppResult<Vec<Route>> {
    set_route_insecure_http_policy_inner(&state, &route_id, policy)
}

#[tauri::command(rename_all = "camelCase")]
pub fn set_route_models(
    route_id: String,
    models: Vec<String>,
    state: State<'_, AppState>,
) -> AppResult<Vec<Route>> {
    if !state.set_route_models(&route_id, models) {
        return Err(AppError::Message(
            "至少選擇一個此 Provider 提供的模型。".into(),
        ));
    }
    if state.proxy_status().running {
        state.refresh_active_route_models(&route_id);
    }
    refresh_catalog_if_running(&state)?;
    Ok(state.routes())
}

/// Rename a Provider. Display only — the route id, and therefore every catalog
/// id on the route, is left alone so Codex keeps resolving what it already has.
#[tauri::command(rename_all = "camelCase")]
pub fn set_route_name(
    route_id: String,
    name: String,
    state: State<'_, AppState>,
) -> AppResult<Vec<Route>> {
    if !state.set_route_name(&route_id, &name) {
        return Err(AppError::Message(
            "找不到這個 Provider，或名稱不能是空的。".into(),
        ));
    }
    republish_route(&state, &route_id)?;
    Ok(state.routes())
}

/// Rename one model for display. An empty name restores the upstream id.
#[tauri::command(rename_all = "camelCase")]
pub fn set_model_display_name(
    route_id: String,
    model: String,
    name: String,
    state: State<'_, AppState>,
) -> AppResult<Vec<Route>> {
    if !state.set_model_display_name(&route_id, &model, &name) {
        return Err(AppError::Message(
            "找不到這個 Provider 的模型，或這個 Provider 不支援手動設定。".into(),
        ));
    }
    republish_route(&state, &route_id)?;
    Ok(state.routes())
}

/// Push a route change to both places it has to land: the catalog Codex reads,
/// and the snapshot the running proxy routes against. Refreshing only the
/// catalog lets Codex ask for something the proxy cannot resolve.
fn republish_route(state: &AppState, route_id: &str) -> AppResult<()> {
    if state.proxy_status().running {
        state.refresh_active_route_models(route_id);
    }
    refresh_catalog_if_running(state)
}

#[tauri::command(rename_all = "camelCase")]
pub fn set_model_vision(
    route_id: String,
    model: String,
    vision: bool,
    state: State<'_, AppState>,
) -> AppResult<Vec<Route>> {
    if !state.set_model_vision(&route_id, &model, vision) {
        return Err(AppError::Message(
            "找不到這個 Provider 的模型，或這個 Provider 不支援手動設定。".into(),
        ));
    }
    republish_route(&state, &route_id)?;
    Ok(state.routes())
}

#[tauri::command(rename_all = "camelCase")]
pub fn delete_route(route_id: String, state: State<'_, AppState>) -> AppResult<Vec<Route>> {
    let route = state
        .routes()
        .into_iter()
        .find(|route| route.id == route_id)
        .ok_or_else(|| AppError::RouteNotFound(route_id.clone()))?;
    if route.provider_kind == ProviderKind::Official {
        return Err(AppError::Message(
            "不能刪除 OpenAI 官方線路；可停用代理以完全還原。".into(),
        ));
    }
    if !state.delete_route(&route_id) {
        return Err(AppError::RouteNotFound(route_id));
    }
    crate::credentials::remove(&state.data_root(), &route.id)?;
    refresh_catalog_if_running(&state)?;
    Ok(state.routes())
}

/// doc/01：概覽。quota 走快取（不強制重查），其餘欄位仍為種子。
#[tauri::command]
pub async fn get_overview(
    force_refresh: Option<bool>,
    state: State<'_, AppState>,
) -> AppResult<Overview> {
    build_overview(&state, force_refresh.unwrap_or(false)).await
}

#[tauri::command]
pub async fn get_provider_overviews(
    force_refresh: Option<bool>,
    state: State<'_, AppState>,
) -> AppResult<Vec<ProviderOverview>> {
    let routes = state.routes();
    let paths = crate::codex::CodexPaths::discover(&state.data_root());
    let official_catalog = crate::catalog::read_official_catalog(&paths.models_cache);
    let active_route_ids = state
        .active_model_routes()
        .into_iter()
        .map(|model| model.route_id)
        .collect::<std::collections::HashSet<_>>();
    let usage_store = state.usage_store();
    let mut result = Vec::with_capacity(routes.len());

    for route in routes {
        let mut route_for_catalog = route.clone();
        route_for_catalog.enabled = true;
        let route_models = crate::catalog::model_routes_with_official_catalog(
            &[route_for_catalog],
            official_catalog.as_ref(),
        );
        let (quota_windows, quota_error) = match route.provider_kind {
            ProviderKind::Official => {
                openai_quota_windows(&state, &route.id, force_refresh.unwrap_or(false)).await
            }
            ProviderKind::GrokCli => {
                grok_quota_windows(&state, &route.id, force_refresh.unwrap_or(false)).await
            }
            ProviderKind::OpenAiCompatible => (Vec::new(), None),
        };
        let quota = quota_windows.first().cloned();
        let usage = usage_store.summary(&route.id)?;
        let provider_models = route_models
            .iter()
            .map(|model| provider_model_status(&state, &route, model))
            .collect::<Vec<_>>();
        result.push(ProviderOverview {
            applied_to_running_proxy: active_route_ids.contains(&route.id),
            route,
            quota,
            quota_windows,
            quota_error,
            models: provider_models,
            latest_input_tokens: usage.latest_input_tokens,
            turns: usage.turns,
            first_byte_ms: usage.latest_first_byte_ms,
        });
    }
    result.sort_by(|left, right| {
        right
            .route
            .enabled
            .cmp(&left.route.enabled)
            .then_with(|| left.route.name.cmp(&right.route.name))
    });
    Ok(result)
}

fn provider_model_status(
    state: &AppState,
    route: &Route,
    model: &ModelRoute,
) -> ProviderModelStatus {
    let inputs = BudgetInputs {
        override_tokens: state
            .budget_override(&model.catalog_id)
            .or_else(|| state.budget_override(&route.id)),
        model_cache: if route.provider_kind == ProviderKind::GrokCli {
            grok_model_cache_window(&model.upstream_model)
        } else {
            None
        },
        catalog: model.context_window,
        effective_percent: Some(95),
    };
    let effective_window = resolve(&model.route_id, &model.upstream_model, inputs).effective_window;
    ProviderModelStatus {
        catalog_id: model.catalog_id.clone(),
        display_name: model.display_name.clone(),
        upstream_model: model.upstream_model.clone(),
        context_window: model.context_window,
        effective_window,
        reasoning: model.reasoning,
        streaming: model.streaming,
    }
}

/// doc/08：強制重查額度（跳過快取），回完整 overview。
#[tauri::command(rename_all = "camelCase")]
pub async fn refresh_quota(route_id: String, state: State<'_, AppState>) -> AppResult<Overview> {
    if !state.routes().iter().any(|r| r.id == route_id) {
        return Err(AppError::RouteNotFound(route_id));
    }
    // route_id 只用來驗證存在；quota 仍以「目前線路」為來源（與 get_overview 一致）。
    build_overview(&state, true).await
}

/// 組 overview。quota 查詢可能阻塞，用 spawn_blocking 搬到 blocking 執行緒。
///
/// wiring 狀態：
/// - window_tokens → 真實預算解析（doc/04）
/// - history_retention_days → 真實常數（doc/05）
/// - reasoning_visible → 線路設定（doc/03）
/// - 其餘（used_tokens / trend / first_byte_ms）仍為種子，待代理轉發接上後產生。
async fn build_overview(state: &AppState, force_refresh: bool) -> AppResult<Overview> {
    let route = state.current_route();

    let quota = match &route {
        Some(route) if route.provider_kind == ProviderKind::Official => {
            openai_quota_windows(state, &route.id, force_refresh)
                .await
                .0
                .into_iter()
                .next()
        }
        Some(route) if route.provider_kind == ProviderKind::GrokCli => {
            grok_quota_windows(state, &route.id, force_refresh)
                .await
                .0
                .into_iter()
                .next()
        }
        _ => None,
    };

    // doc/04：用真實預算解析器算有效視窗。沒有線路時退保底值。
    let window_tokens = match &route {
        Some(r) => {
            let inputs = BudgetInputs {
                override_tokens: state.budget_override(&r.id),
                model_cache: if r.provider_kind == ProviderKind::GrokCli {
                    grok_model_cache_window(&r.model)
                } else {
                    None
                },
                catalog: r.context_window,
                effective_percent: Some(95),
            };
            resolve(&r.id, &r.model, inputs).effective_window
        }
        None => 121_600,
    };

    let reasoning_visible = route.as_ref().is_some_and(|r| r.reasoning);
    let usage_summary = match &route {
        Some(route) => state.usage_store().summary(&route.id)?,
        None => crate::usage::UsageSummary::default(),
    };

    Ok(Overview {
        route,
        last_successful_route: state.usage_store().latest_successful_route()?.map(
            |mut telemetry| {
                resolve_provider_name(
                    &state.route_display_names(),
                    &telemetry.route_id,
                    &mut telemetry.provider,
                );
                telemetry
            },
        ),
        quota,
        usage: ContextUsage {
            used_tokens: usage_summary.latest_input_tokens,
            window_tokens,
            turns: usage_summary.turns,
            provider_total_tokens: usage_summary.total_tokens,
            trend: usage_summary.trend,
            compacted: false,
        },
        health: Health {
            pooled: true,
            connections: 1,
            first_byte_ms: usage_summary.latest_first_byte_ms,
            reasoning_visible,
            history_retention_days: DEFAULT_RETENTION_DAYS,
            history_storage: crate::history::catch_history_panic(std::panic::AssertUnwindSafe(
                || state.history_store().storage_telemetry(),
            ))
            .ok()
            .map(|telemetry| crate::model::HistoryStorageTelemetry {
                logical_live_bytes: telemetry.logical_live_bytes,
                main_db_physical_bytes: telemetry.main_db_physical_bytes,
                wal_bytes: telemetry.wal_bytes,
                compaction_journal_bytes: telemetry.compaction_journal_bytes,
                history_truncated_without_compaction: telemetry
                    .history_truncated_without_compaction,
                history_truncated_items: telemetry.history_truncated_items,
                last_maintenance_at: telemetry.last_maintenance_at,
                last_vacuum_at: telemetry.last_vacuum_at,
                last_journal_retention_deleted: telemetry.last_journal_retention_deleted,
            }),
        },
        findings: state.findings(),
    })
}

async fn openai_quota_windows(
    state: &AppState,
    route_id: &str,
    force_refresh: bool,
) -> (Vec<QuotaSnapshot>, Option<String>) {
    let manager = state.codex_oauth();
    let auth = match manager.valid_default_auth().await {
        Ok(Some(auth)) => auth,
        Ok(None) => return (Vec::new(), Some("尚未透過 Vellum 登入 OpenAI OAuth".into())),
        Err(error) => return (Vec::new(), Some(format!("OpenAI OAuth 無法使用：{error}"))),
    };
    match crate::codex_quota::query(
        &auth.access_token,
        &auth.account_id,
        route_id,
        force_refresh,
    )
    .await
    {
        Ok(windows) => (windows, None),
        Err(crate::codex_quota::CodexQuotaError::Unauthorized) => {
            let refreshed = match manager
                .refresh_after_rejection(&auth.credential_id, &auth.access_token)
                .await
            {
                Ok(refreshed) => refreshed,
                Err(error) => return (Vec::new(), Some(format!("OpenAI OAuth 刷新失敗：{error}"))),
            };
            match crate::codex_quota::query(
                &refreshed.access_token,
                &refreshed.account_id,
                route_id,
                true,
            )
            .await
            {
                Ok(windows) => (windows, None),
                Err(error) => (Vec::new(), Some(format!("OpenAI 額度查詢失敗：{error}"))),
            }
        }
        Err(error) => (Vec::new(), Some(format!("OpenAI 額度查詢失敗：{error}"))),
    }
}

async fn grok_quota_windows(
    state: &AppState,
    route_id: &str,
    force_refresh: bool,
) -> (Vec<QuotaSnapshot>, Option<String>) {
    let (account_id, home) = match state.grok_accounts().default_account_home() {
        Ok(account) => account,
        Err(error) => return (Vec::new(), Some(error.to_string())),
    };
    let service = state.quota_service();
    let route_id = route_id.to_string();
    let result = tokio::task::spawn_blocking(move || {
        service.get_for_account(&route_id, &account_id, &home, force_refresh)
    })
    .await;
    match result {
        Ok(Ok(quota)) => (vec![quota], None),
        Ok(Err(error)) => (Vec::new(), Some(error.to_string())),
        Err(error) => (Vec::new(), Some(format!("Grok 額度查詢工作失敗：{error}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_route_input(name: &str, model: &str) -> CreateRouteInput {
        CreateRouteInput {
            name: name.into(),
            base_url: "http://host.test/v1".into(),
            model: model.into(),
            wire: WireFormat::Responses,
            streaming: true,
            reasoning: true,
            server_side_resume: false,
            provider_kind: Some(ProviderKind::OpenAiCompatible),
            api_key: None,
            models: Some(vec![model.into()]),
            selected_models: Some(vec![model.into()]),
            context_window: Some(128_000),
            model_capabilities: vec![ModelCapability {
                model: model.into(),
                wire: Some(WireFormat::Responses),
                streaming: Some(true),
                reasoning: Some(true),
                tool_calling: Some(true),
                probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                ..Default::default()
            }],
            catalog_scope: None,
        }
    }

    #[test]
    fn bearer_route_creation_provisions_credentials_for_all_compatible_providers() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_test_fixtures(temp.path().to_path_buf());

        for (name, model) in [("806", "qwen"), ("OpenCode Zen", "glm-5")] {
            let mut input = local_route_input(name, model);
            input.api_key = Some(format!("{name}-secret"));
            let routes = create_route_inner(input, &state).unwrap();
            let route = routes.iter().find(|route| route.name == name).unwrap();
            assert_eq!(route.auth_kind, AuthKind::Bearer);
            assert_eq!(
                crate::credentials::load(temp.path(), &route.id)
                    .unwrap()
                    .as_deref(),
                Some(format!("{name}-secret").as_str())
            );
        }
    }

    #[test]
    fn failed_credential_save_does_not_leave_an_unprovisioned_bearer_route() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_test_fixtures(temp.path().to_path_buf());
        let previous = state.current_route().unwrap().id;
        std::fs::write(temp.path().join("credentials"), b"not-a-directory").unwrap();
        let mut input = local_route_input("806", "qwen");
        input.api_key = Some("test-secret".into());

        assert!(create_route_inner(input, &state).is_err());
        assert!(state.routes().iter().all(|route| route.name != "806"));
        assert_eq!(state.current_route().unwrap().id, previous);
    }

    /// The only way a user can escape "plaintext HTTP to private-network
    /// host `X` requires this route's insecureHttpPolicy to be set to
    /// allowPrivateNetwork" (the outbound admission error a self-hosted
    /// LAN/Tailscale provider hits by default) is this command — a fresh
    /// route always starts `Deny` (`state.rs::create_route`) and nothing
    /// else in the app ever changes it.
    #[test]
    fn set_route_insecure_http_policy_persists_and_reaches_a_running_proxys_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_test_fixtures(temp.path().to_path_buf());
        state.activate_proxy_routes();
        state.set_proxy_running(true, None, true, None);

        let routes = state.create_route(
            local_route_input("Lan", "local-model"),
            ProviderKind::OpenAiCompatible,
            AuthKind::None,
        );
        let route = routes.iter().find(|route| route.name == "Lan").unwrap();
        assert_eq!(route.insecure_http_policy, InsecureHttpPolicy::Deny);

        let updated = set_route_insecure_http_policy_inner(
            &state,
            &route.id,
            InsecureHttpPolicy::AllowPrivateNetwork,
        )
        .unwrap();
        let updated_route = updated
            .iter()
            .find(|updated| updated.id == route.id)
            .unwrap();
        assert_eq!(
            updated_route.insecure_http_policy,
            InsecureHttpPolicy::AllowPrivateNetwork
        );

        // Persisted, not just held in memory.
        let reloaded = AppState::with_test_fixtures(temp.path().to_path_buf());
        let reloaded_route = reloaded
            .routes()
            .into_iter()
            .find(|route| route.id == updated_route.id)
            .unwrap();
        assert_eq!(
            reloaded_route.insecure_http_policy,
            InsecureHttpPolicy::AllowPrivateNetwork
        );
    }

    #[test]
    fn set_route_insecure_http_policy_rejects_an_unknown_route() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_test_fixtures(temp.path().to_path_buf());
        let error = set_route_insecure_http_policy_inner(
            &state,
            "no-such-route",
            InsecureHttpPolicy::AllowPrivateNetwork,
        )
        .unwrap_err();
        assert!(matches!(error, AppError::RouteNotFound(_)));
    }

    #[test]
    fn running_catalog_never_advertises_a_provider_pending_proxy_restart() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_test_fixtures(temp.path().to_path_buf());
        state.activate_proxy_routes();
        state.set_proxy_running(true, None, true, None);

        let routes = state.create_route(
            local_route_input("Hoi", "ornith-1.5-35b-iq4xs"),
            ProviderKind::OpenAiCompatible,
            AuthKind::None,
        );
        let route = routes.iter().find(|route| route.name == "Hoi").unwrap();
        let catalog_id = crate::catalog::stable_catalog_id(&route.id, "ornith-1.5-35b-iq4xs");
        assert!(state
            .configured_route_for_catalog_model(&catalog_id)
            .is_some());
        assert!(state.route_for_active_catalog_model(&catalog_id).is_none());

        refresh_catalog_if_running(&state).unwrap();
        let catalog: serde_json::Value = serde_json::from_slice(
            &std::fs::read(temp.path().join("vellum-model-catalog.json")).unwrap(),
        )
        .unwrap();
        assert!(catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .all(|model| model["slug"] != catalog_id));

        // A proxy restart promotes the configured snapshot; the same catalog
        // refresh now advertises a model the listener can actually resolve.
        state.activate_proxy_routes();
        refresh_catalog_if_running(&state).unwrap();
        let catalog: serde_json::Value = serde_json::from_slice(
            &std::fs::read(temp.path().join("vellum-model-catalog.json")).unwrap(),
        )
        .unwrap();
        assert!(catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["slug"] == catalog_id));
        assert!(state.route_for_active_catalog_model(&catalog_id).is_some());
    }

    /// Desktop exposes context capacity so Codex can render usage and Enhanced
    /// can derive its native local-compaction threshold. Vellum still must not
    /// project a legacy `auto_compact_token_limit` of its own.
    #[test]
    fn refresh_catalog_if_running_advertises_context_without_legacy_compact_limit() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_test_fixtures(temp.path().to_path_buf());
        state.activate_proxy_routes();
        state.set_proxy_running(true, None, true, None);

        let routes = state.create_route(
            local_route_input("Hoi", "ornith-1.5-35b-iq4xs"),
            ProviderKind::OpenAiCompatible,
            AuthKind::None,
        );
        let route = routes.iter().find(|route| route.name == "Hoi").unwrap();
        let expected_context_window = route.context_window.expect("fixture context window");
        state.activate_proxy_routes();
        let catalog_id = crate::catalog::stable_catalog_id(&route.id, "ornith-1.5-35b-iq4xs");

        refresh_catalog_if_running(&state).unwrap();
        let catalog: serde_json::Value = serde_json::from_slice(
            &std::fs::read(temp.path().join("vellum-model-catalog.json")).unwrap(),
        )
        .unwrap();
        let entry = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["slug"] == catalog_id)
            .expect("refreshed catalog must contain the active model");
        assert!(
            entry.get("auto_compact_token_limit").is_none(),
            "Vellum must not project an auto-compact schedule any more: {entry}"
        );
        assert_eq!(entry["context_window"], expected_context_window);
        assert_eq!(entry["max_context_window"], expected_context_window);
    }

    #[test]
    fn unchanged_catalog_refresh_does_not_require_another_restart() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_test_fixtures(temp.path().to_path_buf());
        state.activate_proxy_routes();
        state.set_proxy_running(true, None, true, None);

        refresh_catalog_if_running(&state).unwrap();
        assert!(state.runtime_status().restart_required);

        state.clear_restart_required();
        refresh_catalog_if_running(&state).unwrap();

        assert!(!state.runtime_status().restart_required);
    }

    #[test]
    fn cache_only_catalog_refresh_does_not_recreate_restart_notice() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_test_fixtures(temp.path().to_path_buf());
        state.activate_proxy_routes();
        state.set_proxy_running(true, None, true, None);
        refresh_catalog_if_running(&state).unwrap();
        state.clear_restart_required();
        let paths = crate::codex::CodexPaths::discover(&state.data_root());
        let mut catalog: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&paths.catalog).unwrap()).unwrap();
        for key in ["fetched_at", "etag", "client_version"] {
            catalog[key] = "cache-only-change".into();
        }
        std::fs::write(&paths.catalog, serde_json::to_vec(&catalog).unwrap()).unwrap();
        refresh_catalog_if_running(&state).unwrap();
        assert!(!state.runtime_status().restart_required);
        // A real model-contract change must still require reloading Codex.
        catalog["models"][0]["context_window"] = serde_json::json!(42);
        std::fs::write(&paths.catalog, serde_json::to_vec(&catalog).unwrap()).unwrap();
        refresh_catalog_if_running(&state).unwrap();
        assert!(state.runtime_status().restart_required);
    }

    /* 用量列裡的供應商名稱是寫入當下的快照。線路還在就用現在的名字 ——
    否則狀態列會一直顯示改名前的那個，換介面語言也換不掉。 */
    #[test]
    fn stored_provider_names_follow_the_route_rename() {
        let names = std::collections::HashMap::from([(
            "openai-official".to_string(),
            crate::state::OFFICIAL_ROUTE_NAME.to_string(),
        )]);

        let mut stale = "OpenAI 官方".to_string();
        resolve_provider_name(&names, "openai-official", &mut stale);
        assert_eq!(stale, crate::state::OFFICIAL_ROUTE_NAME);

        // 線路已被刪除時沒有更好的來源，存下來的名稱就是最後依據。
        let mut deleted = "退租的供應商".to_string();
        resolve_provider_name(&names, "gone", &mut deleted);
        assert_eq!(deleted, "退租的供應商");
    }

    #[test]
    fn legacy_auto_review_snapshot_is_not_a_work_session() {
        let review = ReviewSettings {
            route_id: "weikuwu".into(),
            model: "nemotron-3-ultra".into(),
            ..ReviewSettings::default()
        };
        assert!(is_legacy_review_snapshot(
            "weikuwu",
            "nemotron-3-ultra",
            None,
            &review
        ));

        let runtime = crate::codex::CodexSessionRuntime {
            label: Some("real task".into()),
            ..crate::codex::CodexSessionRuntime::default()
        };
        assert!(!is_legacy_review_snapshot(
            "weikuwu",
            "nemotron-3-ultra",
            Some(&runtime),
            &review
        ));
        assert!(!is_legacy_review_snapshot(
            "weikuwu", "GLM-5.2", None, &review
        ));
    }

    #[test]
    fn session_list_keeps_roots_and_excludes_internal_codex_threads() {
        let root = "01a07fd9-07bd-74a2-9460-78197c223522";
        let review = "01a07fd9-8fd1-7900-a329-c5f428356705";
        let visible_fork = "01a07fe1-fca5-7232-a8c1-ce2b900c961f";
        let indexed = std::collections::HashMap::from([(
            visible_fork.to_string(),
            "visible fork".to_string(),
        )]);
        assert!(is_user_facing_session_key(
            &format!("codex:{root}:{root}"),
            &indexed
        ));
        assert!(!is_user_facing_session_key(
            &format!("codex:{root}:{review}"),
            &indexed
        ));
        assert!(is_user_facing_session_key(
            &format!("CODEX:{root}:{visible_fork}"),
            &indexed
        ));

        // Legacy Vellum rows are SHA256 keys, and non-Codex harnesses may use
        // their own identifiers. Their shape cannot prove they are children.
        assert!(is_user_facing_session_key(
            "7a2f862b62d9a42a297777385727e4225d9216cf514a0824f3f559e820f0c52",
            &indexed
        ));
        assert!(is_user_facing_session_key("zcode:workspace-a", &indexed));
    }

    #[test]
    fn official_profile_replaces_local_openai_and_combines_third_party_usage() {
        let activity = build_usage_activity(
            vec![
                crate::codex_profile::CodexProfileUsage {
                    daily_usage: vec![
                        crate::codex_profile::CodexProfileDay {
                            date: "2026-07-19".into(),
                            tokens: 324_455_603,
                        },
                        crate::codex_profile::CodexProfileDay {
                            date: "2026-07-20".into(),
                            tokens: 23_619_995,
                        },
                    ],
                    lifetime_tokens: 4_085_208_823,
                    peak_daily_tokens: 324_455_603,
                    current_streak_days: 0,
                    longest_streak_days: 92,
                    longest_task_duration_ms: 6_692_000,
                },
                crate::codex_profile::CodexProfileUsage {
                    daily_usage: vec![crate::codex_profile::CodexProfileDay {
                        date: "2026-07-20".into(),
                        tokens: 10_000,
                    }],
                    lifetime_tokens: 50_000,
                    peak_daily_tokens: 10_000,
                    current_streak_days: 1,
                    longest_streak_days: 3,
                    longest_task_duration_ms: 1_000,
                },
            ],
            crate::usage::LocalUsageActivity {
                days: vec![crate::usage::LocalUsageDay {
                    date: "2026-07-20".into(),
                    route_id: "grok".into(),
                    provider: "Grok Build".into(),
                    tokens: 900,
                    requests: 2,
                }],
                providers: vec![crate::usage::LocalProviderTotal {
                    route_id: "grok".into(),
                    provider: "Grok Build".into(),
                    tokens: 900,
                }],
                longest_request_ms: 2_000,
            },
            Some("openai".into()),
            crate::state::OFFICIAL_ROUTE_NAME.into(),
            None,
        );

        assert_eq!(activity.total_tokens, 4_085_259_723);
        assert_eq!(activity.peak_tokens, 324_455_603);
        assert_eq!(activity.days[1].tokens, 23_630_895);
        assert_eq!(activity.longest_task_duration_ms, 6_692_000);
        assert_eq!(activity.providers.len(), 2);
        assert_eq!(activity.providers[0].source, "codex_profile");
        assert_eq!(activity.providers[0].account_count, 2);
        assert_eq!(activity.official_source, "codex_profile");
    }
}
