use crate::claude_code::CodeStatus;
use crate::state::AppState;
use tauri::State;

#[tauri::command]
pub async fn claude_code_prepare_certificate(
    state: State<'_, AppState>,
) -> Result<CodeStatus, String> {
    state.claude_code().prepare_certificate().await?;
    state.claude_code().status().await
}
#[tauri::command(rename_all = "camelCase")]
pub async fn claude_code_trust_certificate(
    expected_sha256: String,
    state: State<'_, AppState>,
) -> Result<CodeStatus, String> {
    state
        .claude_code()
        .trust_certificate(expected_sha256)
        .await?;
    state.claude_code().status().await
}
#[tauri::command]
pub async fn claude_code_restore_trust(state: State<'_, AppState>) -> Result<CodeStatus, String> {
    state.claude_code().restore_trust().await?;
    state.claude_code().status().await
}

#[tauri::command]
pub async fn claude_code_status(state: State<'_, AppState>) -> Result<CodeStatus, String> {
    state.claude_code().status().await
}
#[tauri::command(rename_all = "camelCase")]
pub async fn claude_code_start(
    project: String,
    state: State<'_, AppState>,
) -> Result<CodeStatus, String> {
    state.claude_code().start(project.into()).await?;
    state.claude_code().status().await
}
#[tauri::command(rename_all = "camelCase")]
pub async fn claude_code_stop(
    force: bool,
    state: State<'_, AppState>,
) -> Result<CodeStatus, String> {
    state.claude_code().stop(force).await?;
    state.claude_code().status().await
}
#[tauri::command(rename_all = "camelCase")]
pub async fn claude_code_select_account(
    account_id: Option<String>,
    state: State<'_, AppState>,
) -> Result<CodeStatus, String> {
    state.claude_code().accounts.select(account_id).await?;
    state.claude_code().status().await
}
#[tauri::command]
pub async fn claude_code_begin_login(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let manager = state.claude_code();
    let url = manager.accounts.begin().await?;
    if app.opener().open_url(url, None::<&str>).is_err() {
        manager.accounts.cancel_login().await;
        return Err("Cannot open Claude authorization in the browser".into());
    }
    Ok(())
}
#[tauri::command]
pub async fn claude_code_cancel_login(state: State<'_, AppState>) -> Result<(), String> {
    state.claude_code().accounts.cancel_login().await;
    Ok(())
}
#[tauri::command(rename_all = "camelCase")]
pub async fn claude_code_remove_account(
    account_id: String,
    state: State<'_, AppState>,
) -> Result<CodeStatus, String> {
    state.claude_code().accounts.remove(&account_id).await?;
    state.claude_code().status().await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn claude_code_refresh_usage(
    account_id: String,
    state: State<'_, AppState>,
) -> Result<CodeStatus, String> {
    state
        .claude_code()
        .accounts
        .refresh_usage(&account_id)
        .await?;
    state.claude_code().status().await
}
