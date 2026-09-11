use crate::budget::{grok_model_cache_window, resolve, BudgetInputs};
use crate::error::{AppError, AppResult};
use crate::model::{ContextBudget, ModelRoute};
use crate::state::AppState;
use tauri::State;

/// doc/04：真接線時 model_cache / catalog 改讀磁碟，其餘不動。
fn inputs_for(state: &AppState, route: &crate::model::Route, model: &ModelRoute) -> BudgetInputs {
    BudgetInputs {
        override_tokens: state
            .budget_override(&model.catalog_id)
            .or_else(|| state.budget_override(&route.id)),
        model_cache: if route.provider_kind == crate::model::ProviderKind::GrokCli {
            grok_model_cache_window(&model.upstream_model)
        } else {
            None
        },
        catalog: model.context_window,
        effective_percent: Some(95),
    }
}

pub fn resolve_model_budget(
    state: &AppState,
    route: &crate::model::Route,
    model: &ModelRoute,
) -> ContextBudget {
    let mut budget = resolve(
        &route.id,
        &model.upstream_model,
        inputs_for(state, route, model),
    );
    budget.catalog_id = model.catalog_id.clone();
    budget
}

#[tauri::command]
pub fn list_model_routes(state: State<'_, AppState>) -> AppResult<Vec<ModelRoute>> {
    Ok(state.model_routes())
}

#[tauri::command]
pub fn list_review_model_routes(state: State<'_, AppState>) -> AppResult<Vec<ModelRoute>> {
    Ok(state.review_model_routes())
}

#[tauri::command(rename_all = "camelCase")]
pub fn get_context_budget(
    catalog_id: String,
    state: State<'_, AppState>,
) -> AppResult<ContextBudget> {
    let (route, model) = state
        .route_for_catalog_model(&catalog_id)
        .ok_or_else(|| AppError::RouteNotFound(catalog_id.clone()))?;
    Ok(resolve_model_budget(&state, &route, &model))
}

#[tauri::command(rename_all = "camelCase")]
pub fn set_budget_override(
    catalog_id: String,
    tokens: Option<u64>,
    state: State<'_, AppState>,
) -> AppResult<ContextBudget> {
    if state.route_for_catalog_model(&catalog_id).is_none() {
        return Err(AppError::RouteNotFound(catalog_id));
    }
    state.set_budget_override(&catalog_id, tokens);
    crate::commands::overview::refresh_catalog_if_running(&state)?;
    let (route, model) = state
        .route_for_catalog_model(&catalog_id)
        .ok_or_else(|| AppError::RouteNotFound(catalog_id.clone()))?;
    Ok(resolve_model_budget(&state, &route, &model))
}
