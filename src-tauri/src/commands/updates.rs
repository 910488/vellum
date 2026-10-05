use crate::error::{AppError, AppResult};
use crate::model::UPDATES_PROGRESS_EVENT;
use crate::state::AppState;
use crate::updates::{
    UpdateComponent, UpdateOperation, UpdatePhase, UpdatePreferences, UpdateStatusSnapshot,
};
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

/// Installs the staged Desktop update now: the same teardown as the tray's
/// Exit, then the installer, then this process exits. The installer relaunches
/// Vellum, and that launch confirms the new version.
///
/// Refused while a request or Codex turn is in flight, because the teardown
/// stops the proxy under it. If the installer cannot be started, a proxy that
/// was running is started again and Vellum stays open.
#[tauri::command]
pub async fn restart_to_apply_desktop_update(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> AppResult<UpdateOperation> {
    let state = (*state).clone();
    let desktop = crate::updates::get_status(&state).desktop;
    if !matches!(
        desktop.phase,
        UpdatePhase::Staged | UpdatePhase::WaitingForRestart
    ) {
        return Err(AppError::Message("nothingStaged".into()));
    }
    let activity = crate::updates::desktop_apply_input(&state, false);
    if activity.proxy_active_requests > 0 {
        return Err(AppError::Message("proxyBusy".into()));
    }
    if activity.core_in_progress {
        return Err(AppError::Message("coreBusy".into()));
    }

    let proxy_was_running = state.proxy_status().running;
    let applied = crate::shutdown_for_exit(&state, true).await;
    if let Some(Ok(op)) = &applied {
        if op.phase == UpdatePhase::Applying {
            emit_progress(&app, op);
            // Let the reply reach the window before the process goes away.
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                handle.exit(0);
            });
            return Ok(op.clone());
        }
    }

    if proxy_was_running {
        if let Err(error) = crate::commands::proxy::start_proxy_inner(&state).await {
            log::warn!("[Updates] proxy did not restart after a failed desktop apply: {error}");
        }
    }
    match applied {
        Some(Err(error)) => Err(error),
        _ => {
            let desktop = crate::updates::get_status(&state).desktop;
            Err(AppError::Message(
                desktop
                    .failure_reason
                    .unwrap_or_else(|| desktop.phase.as_str().to_string()),
            ))
        }
    }
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
