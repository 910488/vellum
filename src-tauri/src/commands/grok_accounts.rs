use crate::error::{AppError, AppResult};
use crate::grok_accounts::{GrokAccountStatus, GrokLoginStatus};
use crate::grok_auth::GrokModelCatalog;
use crate::model::{ProviderKind, QuotaSnapshot};
use crate::state::AppState;
use tauri::State;

#[tauri::command]
pub async fn get_grok_account_status(state: State<'_, AppState>) -> AppResult<GrokAccountStatus> {
    Ok(state.grok_accounts().status())
}

#[tauri::command]
pub async fn start_grok_account_login(state: State<'_, AppState>) -> AppResult<GrokLoginStatus> {
    state.grok_accounts().start_login().await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn poll_grok_account_login(
    login_id: String,
    state: State<'_, AppState>,
) -> AppResult<GrokLoginStatus> {
    state.grok_accounts().poll_login(login_id.trim()).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn cancel_grok_account_login(
    login_id: String,
    state: State<'_, AppState>,
) -> AppResult<GrokLoginStatus> {
    state.grok_accounts().cancel_login(login_id.trim()).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn set_default_grok_account(
    account_id: String,
    state: State<'_, AppState>,
) -> AppResult<GrokAccountStatus> {
    state.grok_accounts().set_default(account_id.trim())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn refresh_grok_account(
    account_id: String,
    state: State<'_, AppState>,
) -> AppResult<GrokAccountStatus> {
    let manager = state.grok_accounts();
    manager.refresh_account(account_id.trim()).await?;
    Ok(manager.status())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn remove_grok_account(
    account_id: String,
    state: State<'_, AppState>,
) -> AppResult<GrokAccountStatus> {
    state.grok_accounts().remove_account(account_id.trim())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn get_grok_account_quota(
    account_id: String,
    route_id: Option<String>,
    force_refresh: Option<bool>,
    state: State<'_, AppState>,
) -> AppResult<Vec<QuotaSnapshot>> {
    let manager = state.grok_accounts();
    let account_id = account_id.trim();
    let home = manager.account_home(account_id)?;
    let route = match route_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        Some(route_id) => state
            .routes()
            .into_iter()
            .find(|route| route.id == route_id),
        None => state
            .routes()
            .into_iter()
            .find(|route| route.provider_kind == ProviderKind::GrokCli),
    }
    .ok_or_else(|| AppError::Message("Grok route is not configured".into()))?;
    if route.provider_kind != ProviderKind::GrokCli {
        return Err(AppError::Message(
            "quota route is not a Grok Build route".into(),
        ));
    }
    let service = state.quota_service();
    let route_id = route.id.clone();
    let account_id = account_id.to_string();
    let force_refresh = force_refresh.unwrap_or(false);
    let quota = tokio::task::spawn_blocking(move || {
        service.get_for_account(&route_id, &account_id, &home, force_refresh)
    })
    .await
    .map_err(|error| AppError::Message(format!("Grok quota task failed: {error}")))??;
    Ok(vec![quota])
}

#[tauri::command(rename_all = "camelCase")]
pub async fn refresh_grok_model_catalog(
    route_id: String,
    state: State<'_, AppState>,
) -> AppResult<GrokModelCatalog> {
    let route = state
        .routes()
        .into_iter()
        .find(|route| route.id == route_id)
        .ok_or_else(|| AppError::RouteNotFound(route_id.clone()))?;
    if route.provider_kind != ProviderKind::GrokCli {
        return Err(AppError::Message(
            "model discovery is available only for Grok Build routes".into(),
        ));
    }
    let (_, home) = state.grok_accounts().default_account_home()?;
    let catalog = crate::grok_auth::discover_models(&home).await?;
    if !state.replace_grok_model_catalog(
        &route.id,
        catalog.models.clone(),
        catalog.default_model.as_deref(),
    ) {
        return Err(AppError::Message(
            "the Grok route changed while its model catalog was being refreshed".into(),
        ));
    }
    if state.proxy_status().running {
        state.refresh_active_route_models(&route.id);
    }
    crate::commands::overview::refresh_catalog_if_running(&state)?;
    Ok(catalog)
}
