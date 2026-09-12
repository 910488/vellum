use crate::error::AppResult;
use crate::model::UPDATES_PROGRESS_EVENT;
use crate::state::AppState;
use crate::updates::{UpdateComponent, UpdateOperation, UpdatePreferences, UpdateStatusSnapshot};
use tauri::{Emitter, State};

fn emit_progress(app: &tauri::AppHandle, op: &UpdateOperation) {
    let _ = app.emit(UPDATES_PROGRESS_EVENT, crate::updates::progress_from(op));
}

#[tauri::command]
pub fn get_update_status(state: State<'_, AppState>) -> AppResult<UpdateStatusSnapshot> {
    Ok(crate::updates::get_status(&state))
}

#[tauri::command(rename_all = "camelCase")]
pub async fn check_updates(
    app: tauri::AppHandle,
    component: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<UpdateStatusSnapshot> {
    let parsed = component.as_deref().and_then(UpdateComponent::parse);
    if parsed == Some(UpdateComponent::Core) {
        return Err(crate::error::AppError::Message(
            "Enhanced core update is preview-only until extracted executable and helper hashes are signed"
                .into(),
        ));
    }
    let snapshot = crate::updates::check_updates(&state, parsed).await?;
    let _ = app.emit(UPDATES_PROGRESS_EVENT, &snapshot.attention);
    Ok(snapshot)
}

#[tauri::command(rename_all = "camelCase")]
pub fn set_update_preferences(
    channel: Option<String>,
    auto_check: Option<bool>,
    auto_download: Option<bool>,
    core_idle_handoff: Option<bool>,
    state: State<'_, AppState>,
) -> AppResult<UpdatePreferences> {
    let mut preferences = crate::updates::get_status(&state).preferences;
    if let Some(channel) = channel.as_deref().and_then(crate::updates::Channel::parse) {
        preferences.channel = channel;
    }
    if let Some(auto_check) = auto_check {
        preferences.auto_check = auto_check;
    }
    if let Some(auto_download) = auto_download {
        preferences.auto_download = auto_download;
    }
    if let Some(core_idle_handoff) = core_idle_handoff {
        preferences.core_idle_handoff = core_idle_handoff;
    }
    crate::updates::set_preferences(&state, preferences)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn download_update(
    app: tauri::AppHandle,
    component: String,
    host_id: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<UpdateOperation> {
    let component = UpdateComponent::parse(&component)
        .ok_or_else(|| crate::error::AppError::Message("unknown component".into()))?;
    let op = crate::updates::download_update(&state, component, host_id).await?;
    emit_progress(&app, &op);
    Ok(op)
}

#[tauri::command(rename_all = "camelCase")]
pub fn apply_update(
    app: tauri::AppHandle,
    component: String,
    host_id: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<UpdateOperation> {
    let component = UpdateComponent::parse(&component)
        .ok_or_else(|| crate::error::AppError::Message("unknown component".into()))?;
    let op = crate::updates::apply_update(&state, component, host_id)?;
    emit_progress(&app, &op);
    Ok(op)
}

#[tauri::command(rename_all = "camelCase")]
pub fn cancel_update_download(
    app: tauri::AppHandle,
    component: String,
    state: State<'_, AppState>,
) -> AppResult<UpdateOperation> {
    let component = UpdateComponent::parse(&component)
        .ok_or_else(|| crate::error::AppError::Message("unknown component".into()))?;
    let op = crate::updates::cancel_download(&state, component)?;
    emit_progress(&app, &op);
    Ok(op)
}

#[tauri::command(rename_all = "camelCase")]
pub fn rollback_update(
    app: tauri::AppHandle,
    component: String,
    state: State<'_, AppState>,
) -> AppResult<UpdateOperation> {
    let component = UpdateComponent::parse(&component)
        .ok_or_else(|| crate::error::AppError::Message("unknown component".into()))?;
    let op = crate::updates::rollback_update(&state, component)?;
    emit_progress(&app, &op);
    Ok(op)
}

#[tauri::command(rename_all = "camelCase")]
pub fn set_remote_update_policy(
    host_id: String,
    idle_auto_update: bool,
    state: State<'_, AppState>,
) -> AppResult<crate::updates::RemoteUpdatePolicy> {
    crate::updates::set_remote_policy(
        &state,
        crate::updates::RemoteUpdatePolicy {
            host_id,
            idle_auto_update,
        },
    )
}
