//! 探測指令（doc/03）。
//!
//! 指令只搬資料／轉錯誤。真實探測邏輯在 `crate::probe`。

use crate::error::AppResult;
use crate::model::{ModelCapability, ProbeResult, RouteReprobeReport};
use crate::probe::{
    default_client, discover_endpoint_with_client, probe_endpoint_with_client,
    probe_model_capability_with_client, probe_model_capability_with_client_and_existing,
    reprobe_selected_models_with_client,
};

#[tauri::command(rename_all = "camelCase")]
pub async fn discover_endpoint_models(
    endpoint: String,
    api_key: Option<String>,
) -> AppResult<ProbeResult> {
    let client = default_client()?;
    discover_endpoint_with_client(
        &client,
        &endpoint,
        api_key.as_deref().filter(|value| !value.trim().is_empty()),
    )
    .await
}

/// doc/03：探測一個端點。發極小的試探請求，判定 wire / 模型 / 能力。
///
/// `endpoint` 可以是根 URL 或已帶 `/v1`；這裡會正規化。
#[tauri::command(rename_all = "camelCase")]
pub async fn probe_endpoint(endpoint: String, api_key: Option<String>) -> AppResult<ProbeResult> {
    let client = default_client()?;
    probe_endpoint_with_client(
        &client,
        &endpoint,
        api_key.as_deref().filter(|value| !value.trim().is_empty()),
    )
    .await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn probe_endpoint_model(
    endpoint: String,
    api_key: Option<String>,
    model: String,
) -> AppResult<ModelCapability> {
    let client = default_client()?;
    probe_model_capability_with_client(
        &client,
        &endpoint,
        api_key.as_deref().filter(|value| !value.trim().is_empty()),
        &model,
    )
    .await
}

/// Re-probe a route: refresh its `/models` catalog (cheap, no inference —
/// see `discover_endpoint_with_client`), then live-verify only the models
/// currently in `selectedModels`. A marketplace catalog can list dozens of
/// models the route never actually exposes to Codex (OpenCode Zen: 64+); a
/// route rarely selects more than a handful, so scoping live verification to
/// that handful is what keeps this affordable regardless of catalog size.
/// Unselected/unverified models keep their placeholder or prior capability —
/// see `AppState::merge_route_probe_result` — and each targeted model's own
/// verification is a separate, independently reportable request. Only a
/// catalog-refresh failure (unreachable endpoint, bad credential) fails the
/// whole command; a targeted model's own probe failing is reported in
/// `RouteReprobeReport`, not returned as `Err`.
#[tauri::command(rename_all = "camelCase")]
pub async fn reprobe_route_capabilities(
    route_id: String,
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<RouteReprobeReport> {
    let route = state
        .routes()
        .into_iter()
        .find(|route| route.id == route_id)
        .ok_or_else(|| crate::error::AppError::RouteNotFound(route_id.clone()))?;
    if route.provider_kind != crate::model::ProviderKind::OpenAiCompatible {
        return Err(crate::error::AppError::Message(
            "manual endpoint capability probing is available for OpenAI-compatible providers"
                .into(),
        ));
    }
    let key = crate::credentials::load(&state.data_root(), &route.id)?;
    let client = default_client()?;
    let selected = route
        .selected_models
        .clone()
        .unwrap_or_else(|| route.models.clone());
    let (discovery, verified, report) = reprobe_selected_models_with_client(
        &client,
        &route.base_url,
        key.as_deref(),
        &selected,
        &route.model_capabilities,
        route.wire,
    )
    .await?;

    state.merge_route_probe_result(&route.id, &discovery, verified);
    // A re-probe can add or drop models from the routing table — a model whose
    // tool protocol now verifies becomes routable, one that no longer verifies
    // is filtered out. The running proxy routes against a snapshot taken when it
    // started, so publishing the new catalog without refreshing that snapshot
    // let Codex request a model the proxy could not resolve: HTTP 422,
    // `找不到線路 <catalog id>`.
    if state.proxy_status().running {
        state.refresh_active_route_models(&route.id);
    }
    crate::commands::overview::refresh_catalog_if_running(&state)?;

    Ok(report)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn reprobe_route_model_capability(
    route_id: String,
    model: String,
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<ModelCapability> {
    let route = state
        .routes()
        .into_iter()
        .find(|route| route.id == route_id)
        .ok_or_else(|| crate::error::AppError::RouteNotFound(route_id.clone()))?;
    if route.provider_kind != crate::model::ProviderKind::OpenAiCompatible {
        return Err(crate::error::AppError::Message(
            "model capability probing is available for OpenAI-compatible providers".into(),
        ));
    }
    if !route
        .models
        .iter()
        .any(|available| available.eq_ignore_ascii_case(&model))
    {
        return Err(crate::error::AppError::Message(format!(
            "model '{model}' is not present in this provider catalog"
        )));
    }
    let key = crate::credentials::load(&state.data_root(), &route.id)?;
    let client = default_client()?;
    let existing = route
        .model_capabilities
        .iter()
        .find(|capability| capability.model.eq_ignore_ascii_case(&model))
        .map(|capability| {
            capability.chat_capabilities.clone().migrate_legacy(
                matches!(capability.wire, Some(crate::model::WireFormat::Chat))
                    || route.wire == crate::model::WireFormat::Chat,
                capability.reasoning.unwrap_or(false),
                capability.probe_version,
            )
        })
        .unwrap_or_default();
    let capability = probe_model_capability_with_client_and_existing(
        &client,
        &route.base_url,
        key.as_deref(),
        &model,
        &existing,
    )
    .await?;
    if !state.replace_route_model_capability(&route.id, capability.clone()) {
        return Err(crate::error::AppError::Message(
            "the provider or model changed while capability verification was running".into(),
        ));
    }
    if state.proxy_status().running {
        state.refresh_active_route_models(&route.id);
    }
    crate::commands::overview::refresh_catalog_if_running(&state)?;
    Ok(capability)
}
