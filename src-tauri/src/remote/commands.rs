//! Tauri commands for remote broker control-plane wiring.

use crate::error::AppResult;
use crate::state::AppState;
use sha2::{Digest, Sha256};

#[tauri::command]
pub fn get_remote_manager_feature_flags(state: tauri::State<'_, AppState>) -> serde_json::Value {
    let enabled = std::env::var("VELLUM_NATIVE_CODEX_REMOTE_MANAGER")
        .map(|value| !matches!(value.trim(), "0" | "false" | "off"))
        .unwrap_or(true);
    serde_json::json!({
        "nativeCodexRemoteManager": enabled,
        "legacyBrokerHostsDropped": state.remote().dropped_legacy_broker_hosts(),
    })
}

#[tauri::command]
pub fn get_remote_release_status() -> crate::remote::pinned_install::RemoteReleaseStatus {
    crate::remote::pinned_install::release_status()
}

#[tauri::command]
pub async fn get_desktop_codex_compatibility(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<crate::remote::desktop_codex::DesktopCodexCompatibilityStatus> {
    let owned = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        tauri::async_runtime::block_on(crate::remote::desktop_codex::compatibility_status(
            &owned, &host_id,
        ))
    })
    .await
    .map_err(|error| {
        crate::error::AppError::Message(format!("Desktop Codex compatibility task failed: {error}"))
    })?
}

#[tauri::command]
pub fn update_remote_codex_for_desktop(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<crate::remote::RemoteOperationProgress> {
    let operation_id = format!("desktop-codex-sync-{}", ulid::Ulid::new());
    let progress = crate::remote::RemoteOperationProgress::queued(
        operation_id.clone(),
        host_id.clone(),
        "desktopCodexSync",
    );
    crate::remote::operation::save(&state.data_root(), &progress)?;
    let owned = state.inner().clone();
    let task_operation_id = operation_id.clone();
    tauri::async_runtime::spawn(async move {
        let mut running = progress;
        running.phase = "resolvingDesktopCodex".into();
        running.percent = 10;
        running.message = Some("Resolving the Desktop Codex protocol identity".into());
        running.updated_at = chrono::Utc::now();
        let _ = crate::remote::operation::save(&owned.data_root(), &running);
        match crate::remote::desktop_codex::update_remote_for_desktop(
            &owned,
            &host_id,
            &task_operation_id,
        )
        .await
        {
            Ok(result) => {
                running.phase = "verified".into();
                running.percent = 100;
                running.state = "completed".into();
                running.message = Some("Remote Codex now matches the Desktop protocol".into());
                running.result = Some(result);
            }
            Err(error) => {
                running.phase = "failed".into();
                running.state = "failed".into();
                running.message = Some(error.to_string());
            }
        }
        running.updated_at = chrono::Utc::now();
        let _ = crate::remote::operation::save(&owned.data_root(), &running);
    });
    crate::remote::operation::load(&state.data_root(), &operation_id)
}

#[tauri::command]
pub async fn discover_remote_connections(
    state: tauri::State<'_, AppState>,
) -> AppResult<Vec<crate::remote::RemoteHostCandidate>> {
    let owned = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let cached = owned.remote().list_hosts()?;
        let candidates = crate::remote::discovery::discover(&cached)?;
        for candidate in &candidates {
            owned.remote().import_discovered_host(candidate)?;
        }
        Ok(candidates)
    })
    .await
    .map_err(|error| {
        crate::error::AppError::Message(format!("remote discovery task failed: {error}"))
    })?
}

#[tauri::command]
pub async fn inspect_remote_host(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<crate::remote::RemoteHostAggregateStatus> {
    let owned = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        crate::remote::RemoteHostManager::aggregate_status(&owned, &host_id)
    })
    .await
    .map_err(|error| {
        crate::error::AppError::Message(format!("remote inspection task failed: {error}"))
    })?
}

#[tauri::command]
pub async fn bootstrap_remote_host(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<crate::remote::RemoteOperationProgress> {
    let operation_id = format!("one-click-bootstrap-{}", ulid::Ulid::new());
    let progress = crate::remote::RemoteOperationProgress::queued(
        operation_id.clone(),
        host_id.clone(),
        "oneClickBootstrap",
    );
    crate::remote::operation::save(&state.data_root(), &progress)?;
    let owned = state.inner().clone();
    tauri::async_runtime::spawn(async move {
        let mut running = progress;
        let progress_root = owned.data_root();
        let result = crate::remote::bootstrap::one_click_bootstrap(
            &owned,
            &host_id,
            |phase, percent, message| {
                running.phase = phase.into();
                running.percent = percent;
                running.message = Some(message.into());
                running.updated_at = chrono::Utc::now();
                let _ = crate::remote::operation::save(&progress_root, &running);
            },
        )
        .await;
        match result {
            Ok(result) => {
                running.phase = "verified".into();
                running.percent = 100;
                running.state = "completed".into();
                running.message = Some("Remote host is ready for native Codex projects".into());
                running.result = serde_json::to_value(result).ok();
            }
            Err(error) => {
                running.phase = "failed".into();
                running.state = "failed".into();
                running.message = Some(error.to_string());
            }
        }
        running.updated_at = chrono::Utc::now();
        let _ = crate::remote::operation::save(&progress_root, &running);
    });
    crate::remote::operation::load(&state.data_root(), &operation_id)
}

#[tauri::command]
pub fn plan_remote_deployment(
    state: tauri::State<'_, AppState>,
    host_id: String,
    selection: crate::remote::RemoteModelSelection,
) -> AppResult<crate::remote::RemoteDeploymentPlan> {
    crate::remote::deployment::plan(&state, &host_id, selection)
}

/// Rebuild a plan from this host's own last-applied model selection and
/// policy overrides (`RemoteHostDesiredState`), instead of the renderer
/// resupplying a `RemoteModelSelection` — the exact shape "reapply desired
/// state" needs, and the only path that will not reset a host's own
/// `autoReviewEnabled`/compaction/web-search overrides back to their
/// defaults the way calling `plan_remote_deployment` with a fresh, empty
/// `RemotePolicyOverrides` would.
#[tauri::command]
pub fn reapply_remote_deployment(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<crate::remote::RemoteDeploymentPlan> {
    crate::remote::deployment::reapply_desired_state(&state, &host_id)
}

#[tauri::command]
pub fn apply_remote_deployment(
    state: tauri::State<'_, AppState>,
    host_id: String,
    plan_id: String,
) -> AppResult<crate::remote::RemoteOperationProgress> {
    let operation_id = ulid::Ulid::new().to_string();
    let progress = crate::remote::RemoteOperationProgress::queued(
        operation_id.clone(),
        host_id.clone(),
        "applyDeployment",
    );
    crate::remote::operation::save(&state.data_root(), &progress)?;
    let owned = state.inner().clone();
    tauri::async_runtime::spawn(async move {
        let mut running = progress;
        running.phase = "applying".into();
        running.percent = 10;
        running.updated_at = chrono::Utc::now();
        let _ = crate::remote::operation::save(&owned.data_root(), &running);
        match crate::remote::deployment::apply(&owned, &host_id, &plan_id).await {
            Ok(result) => {
                running.phase = "verified".into();
                running.percent = 100;
                running.state = "completed".into();
                running.result = serde_json::to_value(result).ok();
            }
            Err(error) => {
                running.phase = "failed".into();
                running.state = "failed".into();
                running.message = Some(error.to_string());
            }
        }
        running.updated_at = chrono::Utc::now();
        let _ = crate::remote::operation::save(&owned.data_root(), &running);
    });
    crate::remote::operation::load(&state.data_root(), &operation_id)
}

#[tauri::command]
pub fn get_remote_operation(
    state: tauri::State<'_, AppState>,
    operation_id: String,
) -> AppResult<crate::remote::RemoteOperationProgress> {
    crate::remote::operation::load(&state.data_root(), &operation_id)
}

fn restart_remote_native_codex_inner(
    state: &AppState,
    host_id: &str,
    operation_id: &str,
) -> AppResult<serde_json::Value> {
    let target = crate::remote::RemoteHostManager::resolve_target(state, host_id)?;
    let client = crate::remote::RemoteAgentClient::new(target);
    let status = client.host_status().ok();
    let session = client.codex_session_status(None).ok();
    crate::remote::observation::require_idle_for_destructive_op(
        session.as_ref(),
        status.as_ref().and_then(|value| value.get("nativeCodex")),
    )
    .map_err(|code| {
        crate::error::AppError::Message(format!("NativeRestartBlocked: {code}"))
    })?;
    let provisioned = crate::remote::provision_remote_boundary_key(
        &client,
        &state.data_root(),
        host_id,
        operation_id,
    )?;
    let result = if provisioned.native_codex_restarted {
        // Provisioning already restarted native Codex to converge a
        // boundary key that was missing or stale -- issuing the user's
        // requested restart again here would just double the downtime for
        // no further effect. Report the state that restart just produced.
        client.codex_discover_native()?
    } else {
        client.codex_restart_native(operation_id)?
    };
    crate::remote::confirm_remote_boundary_key_consumers(
        &client,
        &state.data_root(),
        host_id,
        &[crate::proxy::BoundaryKeyConsumer::NativeCodex],
        true,
    )?;
    Ok(result)
}

#[tauri::command]
pub fn restart_remote_native_codex(
    state: tauri::State<'_, AppState>,
    host_id: String,
    operation_id: String,
) -> AppResult<serde_json::Value> {
    restart_remote_native_codex_inner(&state, &host_id, &operation_id)
}

#[tauri::command]
pub fn stop_remote_app_owned_codex(
    state: tauri::State<'_, AppState>,
    host_id: String,
    operation_id: String,
) -> AppResult<serde_json::Value> {
    let target = crate::remote::RemoteHostManager::resolve_target(&state, &host_id)?;
    crate::remote::RemoteAgentClient::new(target).codex_stop_app_owned(&operation_id)
}

#[tauri::command]
pub async fn restore_remote_host(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<crate::remote::RemoteOperationProgress> {
    let operation_id = format!("one-click-restore-{}", ulid::Ulid::new());
    let progress = crate::remote::RemoteOperationProgress::queued(
        operation_id.clone(),
        host_id.clone(),
        "oneClickRestore",
    );
    crate::remote::operation::save(&state.data_root(), &progress)?;
    let owned = state.inner().clone();
    let task_operation_id = operation_id.clone();
    tauri::async_runtime::spawn(async move {
        let mut running = progress;
        let progress_root = owned.data_root();
        let result = crate::remote::restore::one_click_restore(
            &owned,
            &host_id,
            &task_operation_id,
            |phase, percent, message| {
                running.phase = phase.into();
                running.percent = percent;
                running.message = Some(message.into());
                running.updated_at = chrono::Utc::now();
                let _ = crate::remote::operation::save(&progress_root, &running);
            },
        );
        match result {
            Ok(result) => {
                running.phase = "restored".into();
                running.percent = 100;
                running.state = "completed".into();
                running.message = Some("Vellum managed routing has been safely withdrawn".into());
                running.result = serde_json::to_value(result).ok();
            }
            Err(error) => {
                running.phase = "failed".into();
                running.state = "failed".into();
                running.message = Some(error.to_string());
            }
        }
        running.updated_at = chrono::Utc::now();
        let _ = crate::remote::operation::save(&progress_root, &running);
    });
    crate::remote::operation::load(&state.data_root(), &operation_id)
}

#[tauri::command]
pub fn start_remote_grok_login(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<serde_json::Value> {
    let target = crate::remote::RemoteHostManager::resolve_target(&state, &host_id)?;
    crate::remote::RemoteAgentClient::new(target)
        .grok_start_login(&format!("grok-login-{}", ulid::Ulid::new()))
}

#[tauri::command]
pub fn poll_remote_grok_login(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<serde_json::Value> {
    let target = crate::remote::RemoteHostManager::resolve_target(&state, &host_id)?;
    crate::remote::RemoteAgentClient::new(target).grok_poll_login()
}

#[tauri::command]
pub fn refresh_remote_grok_login(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<serde_json::Value> {
    let target = crate::remote::RemoteHostManager::resolve_target(&state, &host_id)?;
    crate::remote::RemoteAgentClient::new(target)
        .grok_refresh(&format!("grok-refresh-{}", ulid::Ulid::new()))
}

#[tauri::command]
pub fn cancel_remote_grok_login(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<serde_json::Value> {
    let target = crate::remote::RemoteHostManager::resolve_target(&state, &host_id)?;
    crate::remote::RemoteAgentClient::new(target)
        .grok_cancel_login(&format!("grok-cancel-{}", ulid::Ulid::new()))
}

/// Which Desktop account a remote account command acts on. Omitting it keeps
/// the original single-account behaviour — Desktop's current default — so the
/// one-account-at-a-time repair path stays exactly as it was; passing one is
/// what lets first-run setup walk the whole account list.
fn requested_account_id(
    state: &tauri::State<'_, AppState>,
    account_id: Option<String>,
) -> AppResult<String> {
    let requested = account_id
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty());
    if let Some(requested) = requested {
        // A renderer-supplied id must be one Desktop actually holds. Otherwise
        // this command would pair a remote host against an account that has no
        // grant on this machine, which no later switch could ever activate.
        if !state
            .codex_oauth()
            .peek_accounts()
            .iter()
            .any(|(id, _, _)| *id == requested)
            && super::desktop_control_account_id(state.inner()).as_deref()
                != Some(requested.as_str())
        {
            return Err(crate::error::AppError::Message(
                "desktopOfficialAccountUnknown".into(),
            ));
        }
        return Ok(requested);
    }
    super::desktop_control_account_id(state.inner())
        .ok_or_else(|| crate::error::AppError::Message("desktopOfficialAccountMissing".into()))
}

/// Remember which host Remote Manager is showing. An account switch pushes to
/// this host and no other; see `RemoteLocalCache::active_host`.
#[tauri::command(rename_all = "camelCase")]
pub fn set_active_remote_host(
    state: tauri::State<'_, AppState>,
    host_id: Option<String>,
) -> AppResult<()> {
    state.remote().set_active_host(host_id.as_deref())
}

/// One row per Desktop ChatGPT account, saying whether this host has its own
/// grant for it and which one the daemon is using right now.
///
/// Each row costs an SSH round trip, because the agent answers about one
/// account at a time. That is deliberate: nothing here needs a new agent RPC,
/// so this works against every agent already deployed, and the cost is only
/// paid when the account panel is open.
#[tauri::command(rename_all = "camelCase")]
pub fn list_remote_codex_account_pairings(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<serde_json::Value> {
    let mut accounts = state.codex_oauth().peek_accounts();
    let default_account_id = super::desktop_control_account_id(state.inner());
    if let Some(native) = default_account_id.as_ref() {
        if !accounts.iter().any(|(account, _, _)| account == native) {
            accounts.push((native.clone(), None, None));
        }
    }
    accounts.sort_by(|left, right| left.0.cmp(&right.0));
    let target = crate::remote::RemoteHostManager::resolve_target(&state, &host_id)?;
    let client = crate::remote::RemoteAgentClient::new(target);
    let mut rows = Vec::with_capacity(accounts.len());
    for (account_id, email, workspace_name) in accounts {
        // One unreachable or unpairable account must not blank the whole
        // panel: report it on its own row and keep going.
        let status = client.codex_account_status(Some(&account_id));
        let (paired, active, detail) = match &status {
            Ok(value) => (
                value.get("paired").and_then(serde_json::Value::as_bool) == Some(true),
                value.get("active").and_then(serde_json::Value::as_bool) == Some(true),
                None,
            ),
            Err(error) => (false, false, Some(error.to_string())),
        };
        rows.push(serde_json::json!({
            "accountId": account_id,
            "email": email,
            "workspaceName": workspace_name,
            "isDesktopDefault": default_account_id.as_deref() == Some(account_id.as_str()),
            "paired": paired,
            "active": active,
            "detail": detail,
        }));
    }
    Ok(serde_json::Value::Array(rows))
}

#[tauri::command(rename_all = "camelCase")]
pub fn start_remote_codex_account_login(
    state: tauri::State<'_, AppState>,
    host_id: String,
    account_id: Option<String>,
) -> AppResult<serde_json::Value> {
    let account_id = requested_account_id(&state, account_id)?;
    let target = crate::remote::RemoteHostManager::resolve_target(&state, &host_id)?;
    crate::remote::RemoteAgentClient::new(target).codex_account_login_start(
        &format!("codex-account-login-{}", ulid::Ulid::new()),
        &account_id,
    )
}

#[tauri::command]
pub fn poll_remote_codex_account_login(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<serde_json::Value> {
    let target = crate::remote::RemoteHostManager::resolve_target(&state, &host_id)?;
    crate::remote::RemoteAgentClient::new(target).codex_account_login_poll()
}

#[tauri::command(rename_all = "camelCase")]
pub fn activate_remote_codex_account(
    state: tauri::State<'_, AppState>,
    host_id: String,
    account_id: Option<String>,
) -> AppResult<serde_json::Value> {
    let account_id = requested_account_id(&state, account_id)?;
    let target = crate::remote::RemoteHostManager::resolve_target(&state, &host_id)?;
    let result = crate::remote::RemoteAgentClient::new(target).codex_account_activate(
        &format!("codex-account-activate-{}", ulid::Ulid::new()),
        &account_id,
    );
    if result.is_ok() {
        state.remote().invalidate_snapshot(&host_id);
    }
    result
}

/// Pair a phone with the remote daemon's control identity. Official Remote
/// requires Desktop and phone to use the same ChatGPT account/workspace.
#[tauri::command]
pub fn start_remote_control_pairing(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<serde_json::Value> {
    let account_id = super::desktop_control_account_id(state.inner())
        .ok_or_else(|| crate::error::AppError::Message("desktopOfficialAccountMissing".into()))?;
    let expected_hash = format!("{:x}", Sha256::digest(account_id.as_bytes()));
    let target = crate::remote::RemoteHostManager::resolve_target(&state, &host_id)?;
    crate::remote::RemoteAgentClient::new(target).codex_remote_control_pair_start(&expected_hash)
}

#[tauri::command]
pub async fn list_remote_official_execution_accounts(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<serde_json::Value> {
    let owned = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let target = crate::remote::RemoteHostManager::resolve_target(&owned, &host_id)?;
        crate::remote::RemoteAgentClient::new(target).proxy_official_account_list()
    })
    .await
    .map_err(|error| {
        crate::error::AppError::Message(format!("remote account list task failed: {error}"))
    })?
}

#[tauri::command]
pub fn start_remote_official_execution_account_login(
    state: tauri::State<'_, AppState>,
    host_id: String,
    display_name: String,
) -> AppResult<serde_json::Value> {
    let target = crate::remote::RemoteHostManager::resolve_target(&state, &host_id)?;
    crate::remote::RemoteAgentClient::new(target).proxy_official_account_login_start(
        &format!("official-execution-login-{}", ulid::Ulid::new()),
        &display_name,
    )
}

#[tauri::command]
pub fn poll_remote_official_execution_account_login(
    state: tauri::State<'_, AppState>,
    host_id: String,
    login_id: String,
) -> AppResult<serde_json::Value> {
    let target = crate::remote::RemoteHostManager::resolve_target(&state, &host_id)?;
    crate::remote::RemoteAgentClient::new(target).proxy_official_account_login_poll(&login_id)
}

#[tauri::command]
pub fn select_remote_official_execution_account(
    state: tauri::State<'_, AppState>,
    host_id: String,
    account_id_hash: String,
) -> AppResult<serde_json::Value> {
    let target = crate::remote::RemoteHostManager::resolve_target(&state, &host_id)?;
    crate::remote::RemoteAgentClient::new(target).proxy_official_account_select(
        &format!("official-execution-select-{}", ulid::Ulid::new()),
        &account_id_hash,
    )
}

#[tauri::command]
pub fn remove_remote_official_execution_account(
    state: tauri::State<'_, AppState>,
    host_id: String,
    account_id_hash: String,
) -> AppResult<serde_json::Value> {
    let target = crate::remote::RemoteHostManager::resolve_target(&state, &host_id)?;
    crate::remote::RemoteAgentClient::new(target).proxy_official_account_remove(
        &format!("official-execution-remove-{}", ulid::Ulid::new()),
        &account_id_hash,
    )
}

#[tauri::command]
pub fn remote_agent_host_status(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<serde_json::Value> {
    crate::remote::RemoteHostManager::probe_agent(&state, &host_id)
}

#[tauri::command]
pub async fn remote_host_aggregate_status(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<crate::remote::RemoteHostAggregateStatus> {
    let owned = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        crate::remote::RemoteHostManager::aggregate_status(&owned, &host_id)
    })
    .await
    .map_err(|error| {
        crate::error::AppError::Message(format!("remote status task failed: {error}"))
    })?
}

#[tauri::command]
pub fn remote_proxy_start(
    state: tauri::State<'_, AppState>,
    host_id: String,
    operation_id: String,
    host_port: Option<u16>,
    image: Option<String>,
) -> AppResult<serde_json::Value> {
    crate::remote::RemoteHostManager::start_proxy(
        &state,
        &host_id,
        &operation_id,
        host_port,
        image.as_deref(),
    )
}

#[tauri::command]
pub fn remote_proxy_install(
    state: tauri::State<'_, AppState>,
    host_id: String,
    operation_id: String,
    image: String,
    image_digest: Option<String>,
) -> AppResult<serde_json::Value> {
    crate::remote::RemoteHostManager::install_proxy(
        &state,
        &host_id,
        &operation_id,
        &image,
        image_digest.as_deref(),
    )
}

#[tauri::command]
pub fn remote_proxy_configure(
    state: tauri::State<'_, AppState>,
    host_id: String,
    operation_id: String,
    config_toml: String,
) -> AppResult<serde_json::Value> {
    crate::remote::RemoteHostManager::configure_proxy(&state, &host_id, &operation_id, &config_toml)
}

#[tauri::command]
pub fn remote_proxy_stop(
    state: tauri::State<'_, AppState>,
    host_id: String,
    operation_id: String,
) -> AppResult<serde_json::Value> {
    crate::remote::RemoteHostManager::stop_proxy(&state, &host_id, &operation_id)
}

#[tauri::command]
pub fn remote_manager_repair(
    state: tauri::State<'_, AppState>,
    host_id: String,
    operation_id: String,
) -> AppResult<serde_json::Value> {
    crate::remote::RemoteHostManager::repair(&state, &host_id, &operation_id)
}

#[tauri::command]
pub fn remote_manager_support_bundle(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<serde_json::Value> {
    crate::remote::RemoteHostManager::support_bundle(&state, &host_id)
}

#[tauri::command]
pub fn install_remote_pinned_codex(
    state: tauri::State<'_, AppState>,
    host_id: String,
    operation_id: String,
) -> AppResult<serde_json::Value> {
    crate::remote::pinned_install::install_pinned_codex(&state, &host_id, &operation_id)
}

#[tauri::command]
pub fn update_remote_components(
    state: tauri::State<'_, AppState>,
    host_id: String,
    operation_id: String,
) -> AppResult<serde_json::Value> {
    crate::remote::pinned_install::update_remote_components(&state, &host_id, &operation_id)
}

#[tauri::command]
pub fn get_remote_desired_state(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<crate::remote::RemoteHostDesiredState> {
    crate::remote::desired_state::load(&state.data_root(), &host_id)
}

#[tauri::command]
pub async fn remote_session_summary(
    state: tauri::State<'_, AppState>,
    host_id: String,
) -> AppResult<crate::remote::RemoteSessionSummary> {
    let owned = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        crate::remote::session_summary::summarize(&owned, &host_id)
    })
    .await
    .map_err(|error| {
        crate::error::AppError::Message(format!("remote session summary task failed: {error}"))
    })?
}

#[cfg(test)]
mod tests {
    #[test]
    fn boundary_key_provisioning_precedes_the_manual_restart_rpc_in_source_order() {
        let source = include_str!("commands.rs");
        let gate_at = source
            .find("require_idle_for_destructive_op(")
            .expect("restart_remote_native_codex_inner() must gate incomplete observation");
        let provision_at = source
            .find("provision_remote_boundary_key(")
            .expect("restart_remote_native_codex_inner() must call provision_remote_boundary_key");
        let rpc_at = source
            .find("client.codex_restart_native(")
            .expect("restart_remote_native_codex_inner() must call codex_restart_native");
        let confirm_at = source
            .find("confirm_remote_boundary_key_consumers(")
            .expect("manual restart must confirm the resulting native instance");
        assert!(
            gate_at < provision_at,
            "observation gate must run before restart mutates the host"
        );
        assert!(
            provision_at < rpc_at,
            "boundary-key provisioning must run before the manual codex.restartNative RPC"
        );
        assert!(
            rpc_at < confirm_at,
            "native confirmation must follow restart"
        );
    }
}
