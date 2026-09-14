use crate::catalog::write_catalog_with_model_routes_and_search_and_compaction;
use crate::codex::{
    apply_proxy_config, has_active_lease, reconcile_lease, restore_proxy_config, CodexPaths,
};
use crate::error::{AppError, AppResult};
use crate::model::{CatalogStatus, ProxyPhase, ProxyStatus, RestoreResult};
use crate::proxy::serve_local_proxy;
use crate::proxy_prepare::{DesktopPrepareHost, PrepareHost, PrepareSnapshot};
use crate::state::AppState;
#[cfg(test)]
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::State;

const PROXY_PORT: u16 = 15721;

#[cfg(test)]
type TestStartOverrides = Mutex<HashMap<PathBuf, (Arc<dyn PrepareHost>, u16)>>;

#[cfg(test)]
fn test_start_overrides() -> &'static TestStartOverrides {
    static CELL: OnceLock<TestStartOverrides> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn set_test_start_override(root: PathBuf, host: Arc<dyn PrepareHost>, port: u16) {
    test_start_overrides()
        .lock()
        .expect("test start override poisoned")
        .insert(root, (host, port));
}

#[cfg(test)]
pub(crate) fn clear_test_start_override(root: &std::path::Path) {
    test_start_overrides()
        .lock()
        .expect("test start override poisoned")
        .remove(root);
}

fn update_codex_restart_requirement(state: &AppState, process_identity: Option<String>) {
    if let Some(process_identity) = process_identity {
        state.mark_restart_required_for_process(
            crate::model::RuntimeNotice::new("codexRunning"),
            process_identity,
        );
    } else {
        state.clear_restart_required();
    }
}

#[tauri::command]
pub fn get_proxy_status(state: State<'_, AppState>) -> AppResult<ProxyStatus> {
    Ok(state.proxy_status())
}

#[tauri::command]
pub fn get_catalog_status(state: State<'_, AppState>) -> AppResult<CatalogStatus> {
    let proxy = state.proxy_status();
    let injected_model_ids = if proxy.running {
        let paths = CodexPaths::discover(&state.data_root());
        std::fs::read(&paths.catalog)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| {
                value
                    .get("models")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
            })
            .unwrap_or_default()
            .into_iter()
            .filter_map(|model| {
                model
                    .get("slug")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(CatalogStatus {
        proxy_running: proxy.running,
        injected_model_ids,
    })
}

#[tauri::command]
pub async fn start_proxy(state: State<'_, AppState>) -> AppResult<ProxyStatus> {
    start_proxy_inner(&state).await
}

pub(crate) async fn start_proxy_inner(state: &AppState) -> AppResult<ProxyStatus> {
    #[cfg(test)]
    {
        let root = state.data_root();
        let override_host = test_start_overrides()
            .lock()
            .expect("test start override poisoned")
            .get(&root)
            .cloned();
        if let Some((host, port)) = override_host {
            return start_proxy_with_host(state, host.as_ref(), port).await;
        }
    }
    let state = state.clone();
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let base_url = format!("http://127.0.0.1:{PROXY_PORT}/v1");
        let host = DesktopPrepareHost {
            state: &state,
            base_url,
        };
        runtime.block_on(start_proxy_with_host(&state, &host, PROXY_PORT))
    })
    .await
    .map_err(|error| AppError::Message(format!("proxy start worker failed: {error}")))?
}

/// Shared Start path. Concurrent callers join the in-flight generation instead
/// of bumping a second one. `listen_port` is the product port in production
/// and `0` in tests so the user's Desktop listener is not stolen.
pub(crate) async fn start_proxy_with_host<H: PrepareHost + ?Sized>(
    state: &AppState,
    host: &H,
    listen_port: u16,
) -> AppResult<ProxyStatus> {
    if state.proxy_status().running {
        return Ok(state.proxy_status());
    }
    let generation_id = {
        let _admit = state.lifecycle_lock().lock().await;
        if state.proxy_status().running {
            return Ok(state.proxy_status());
        }
        match state.proxy_status().phase {
            ProxyPhase::Preparing | ProxyPhase::Starting => state.proxy_status().generation,
            _ => {
                let generation = state.bump_proxy_generation();
                let _ = state.apply_status_if_generation(generation.id, |status| {
                    status.phase = ProxyPhase::Preparing;
                    status.running = false;
                    status.last_error = None;
                    status.stage = Some("discovery".into());
                    status.stage_elapsed_ms = Some(0);
                });
                generation.id
            }
        }
    };
    let live = state.generation_controller().current();
    if live.id != generation_id {
        return Err(AppError::Message("user stopped proxy".into()));
    }
    let cancel = live.subscribe();
    let prepared = {
        let mut coordinator = state.prepare_coordinator().lock().await;
        match coordinator
            .prepare_with_progress(host, cancel, |stage, elapsed| {
                state.set_proxy_stage(generation_id, stage, elapsed);
            })
            .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let _ = state.apply_status_if_generation(generation_id, |status| {
                    status.phase = ProxyPhase::Failed;
                    status.last_error = Some(error.to_string());
                    status.stage = Some("prepare".into());
                });
                return Err(error);
            }
        }
    };
    if !state.is_generation_current(generation_id) {
        return Err(AppError::Message("user stopped proxy".into()));
    }
    let _gate = state.lifecycle_lock().lock().await;
    if !state.is_generation_current(generation_id) {
        return Err(AppError::Message("user stopped proxy".into()));
    }
    if state.proxy_status().running {
        return Ok(state.proxy_status());
    }
    let result = start_proxy_transaction_on(state, &prepared, generation_id, listen_port).await;
    if let Err(error) = &result {
        state.apply_status_if_generation(generation_id, |status| {
            status.phase = ProxyPhase::Failed;
            status.running = false;
            status.last_error = Some(error.to_string());
        });
    }
    result
}

/// Lease, bind, readiness, then diversion. Does not re-run schema / capability
/// / catalog prepare when `prepared` is still valid.
async fn start_proxy_transaction_on(
    state: &AppState,
    prepared: &PrepareSnapshot,
    generation: u64,
    listen_port: u16,
) -> AppResult<ProxyStatus> {
    if !state.is_generation_current(generation) {
        return Err(AppError::Message("user stopped proxy".into()));
    }
    if prepared.schema_ran && prepared.capability_ran && prepared.catalog_ran {
        // Transaction path: those stages already ran (or were reused).
        log::info!(
            "[Proxy] start transaction after prepare schema_ok={} capability_ok={}",
            prepared.schema_ok,
            prepared.capability_ok
        );
    }
    let _ = state.apply_status_if_generation(generation, |status| {
        status.phase = ProxyPhase::Starting;
        status.stage = Some("lease".into());
    });
    let started = Instant::now();
    let paths = CodexPaths::discover(&state.data_root());
    reconcile_lease(&paths)?;
    let lease_ms = started.elapsed().as_millis() as u64;
    state.set_proxy_stage(generation, "lease", lease_ms);
    if !state.is_generation_current(generation) {
        return Err(AppError::Message("user stopped proxy".into()));
    }

    let bind_started = Instant::now();
    let boundary_key = crate::proxy::ensure_boundary_key(&state.data_root())?;
    let boundary_key_value = boundary_key.expose_for_storage().to_string();
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), listen_port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| {
            AppError::Message(format!(
                "ProxyPortOwnershipConflict: 本機代理埠 {listen_port} 已被其他程序佔用：{error}{}",
                leftover_vellum_host_detail()
                    .map(|detail| format!(" leftover Vellum host still running: {detail}"))
                    .unwrap_or_default()
            ))
        })?;
    let bound_port = listener
        .local_addr()
        .map(|addr| addr.port())
        .unwrap_or(listen_port);
    let base_url = format!("http://127.0.0.1:{bound_port}/v1");
    let bind_ms = bind_started.elapsed().as_millis() as u64;
    state.set_proxy_stage(generation, "bind", bind_ms);

    let catalog_started = Instant::now();
    crate::runtime::snapshot_catalog(&state.data_root(), &paths.catalog)?;
    // Commit from current settings under the lifecycle lock. A file left by
    // an earlier prepare is not proof that its routes/search settings match.
    {
        write_catalog_with_model_routes_and_search_and_compaction(
            &paths.catalog,
            &state.routes(),
            &state.model_routes(),
            Some(&paths.models_cache),
            state
                .web_search_settings()
                .with_stored_brave_key(&state.data_root())
                .advertised(),
            None,
        )?;
    }
    if let Ok(bytes) = std::fs::read(&paths.catalog) {
        state.set_active_catalog_version(Some(crate::runtime::catalog_id(&bytes)));
    }
    state.activate_proxy_routes();
    let catalog_ms = catalog_started.elapsed().as_millis() as u64;
    state.set_proxy_stage(generation, "catalog_commit", catalog_ms);

    state.set_draining(false);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let owned = (*state).clone();
    let handle = tokio::spawn(async move {
        if let Err(error) =
            serve_local_proxy(owned.clone(), listener, boundary_key, shutdown_rx).await
        {
            log::error!("[Proxy] {error}");
            if owned.is_generation_current(generation) {
                owned.set_proxy_running(
                    false,
                    None,
                    has_active_lease(&CodexPaths::discover(&owned.data_root())),
                    Some(error.to_string()),
                );
            }
        } else if owned.is_generation_current(generation) {
            owned.set_proxy_running(
                false,
                None,
                has_active_lease(&CodexPaths::discover(&owned.data_root())),
                None,
            );
        }
    });

    let ready_started = Instant::now();
    state.set_proxy_stage(generation, "readiness", 0);
    #[cfg(test)]
    let forced_ready_failure = state.data_root().join("force-ready-failure").exists();
    #[cfg(not(test))]
    let forced_ready_failure = false;
    if forced_ready_failure || !self_check_ready(bound_port, &boundary_key_value).await {
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(Duration::from_millis(400), handle).await;
        let _ = restore_proxy_config(&paths);
        state.clear_active_proxy_routes();
        let _ = state.apply_status_if_generation(generation, |status| {
            status.phase = ProxyPhase::Failed;
            status.running = false;
            status.last_error = Some(
                "ProxyBoundaryAuthenticationFailed: proxy started but did not answer its own \
                 authenticated readiness check; diversion settings were not committed"
                    .into(),
            );
        });
        return Err(AppError::Message(
            "ProxyBoundaryAuthenticationFailed: proxy started but did not answer its own \
             authenticated readiness check; diversion settings were not committed"
                .into(),
        ));
    }
    let ready_ms = ready_started.elapsed().as_millis() as u64;
    state.set_proxy_stage(generation, "readiness", ready_ms);

    let divert_started = Instant::now();
    if let Err(error) = apply_proxy_config(
        &paths,
        &base_url,
        &state.subagent_settings(),
        boundary_key_value.as_str(),
        bound_port,
    ) {
        let _ = shutdown_tx.send(());
        let _ = tokio::time::timeout(Duration::from_millis(400), handle).await;
        let _ = restore_proxy_config(&paths);
        state.clear_active_proxy_routes();
        return Err(error);
    }
    let divert_ms = divert_started.elapsed().as_millis() as u64;
    state.set_proxy_stage(generation, "diversion", divert_ms);
    #[cfg(test)]
    eprintln!(
        "[start-tx] lease={lease_ms}ms bind={bind_ms}ms catalog_commit={catalog_ms}ms readiness={ready_ms}ms diversion={divert_ms}ms"
    );
    update_codex_restart_requirement(state, crate::commands::runtime::codex_process_identity());

    if !state.is_generation_current(generation) {
        let _ = shutdown_tx.send(());
        let _ = restore_proxy_config(&paths);
        state.clear_active_proxy_routes();
        return Err(AppError::Message("user stopped proxy".into()));
    }

    state.install_proxy_shutdown(shutdown_tx);
    state.set_proxy_running(
        true,
        Some(paths.catalog.to_string_lossy().to_string()),
        true,
        None,
    );
    state.install_proxy_server_task(tauri::async_runtime::JoinHandle::Tokio(handle));
    if !prepared.schema_ok {
        state.record_live_applied(
            crate::model::RuntimeNotice::new("enhancedDesktopRuntimeNotArmed").with(
                "detail",
                "schema prepare did not verify; Enhanced is not armed",
            ),
        );
    } else {
        // Enhanced arm is warning-not-fatal and may spawn/process-probe. Keep it
        // off the authenticated-readiness + diversion return path so Start can
        // meet 500ms after prepare. Stop bumps generation first, so a late arm
        // must not write over a newer lifecycle. Schema failure must not arm.
        let arm_state = (*state).clone();
        tokio::task::spawn_blocking(move || {
            arm_enhanced_runtime_for_proxy(&arm_state, generation);
        });
    }
    Ok(state.proxy_status())
}

fn update_codex_restart_after_stop(state: &AppState, process_identity: Option<String>) {
    if let Some(process_identity) = process_identity {
        state.mark_restart_required_for_process(
            crate::model::RuntimeNotice::new("proxyStoppedCodexRestartRequired"),
            process_identity,
        );
    } else {
        state.clear_restart_required();
    }
}

/// Arms the Enhanced launch behind a running Proxy, or gives it up cleanly.
///
/// This used to take the whole Proxy down with it, on the reasoning that Proxy
/// and bridge launch ownership are one transaction. The ownership part is real;
/// the conclusion was not. Enhanced only decides *which Codex runtime executes*
/// a third-party thread — the Proxy decides whether that thread reaches its
/// provider at all. Tying the second to the first meant a stale bridge path
/// cost the user every third-party model, to protect a change that would only
/// have altered how those models ran.
///
/// So a failure here is reported, not propagated. What must not be skipped is
/// the release: leaving `CODEX_CLI_PATH` pointing at a launch we could not
/// prepare is the half-state the teardown was actually guarding, and releasing
/// it is enough to rule that out.
fn enhanced_commit_guard(
    state: &AppState,
    generation: u64,
) -> Result<tokio::sync::MutexGuard<'_, ()>, crate::enhanced_runtime::DesktopRuntimeManagerError> {
    let guard = state.lifecycle_lock().blocking_lock();
    if !state.is_generation_current(generation) || !state.proxy_status().running {
        return Err(
            crate::enhanced_runtime::DesktopRuntimeManagerError::DesktopProbe(
                "user stopped proxy".into(),
            ),
        );
    }
    Ok(guard)
}

fn arm_enhanced_runtime_for_proxy(state: &AppState, generation: u64) {
    if !state.is_generation_current(generation) {
        return;
    }
    let armed = crate::enhanced_runtime::desktop_manager::sync_proxy_desktop_launch(
        &state.data_root(),
        &state.routes(),
        &state.model_routes(),
        || enhanced_commit_guard(state, generation),
    );
    // Notices and failure cleanup belong to the same generation too. In
    // particular, an old failure must not release a newer start's lease.
    let Ok(_guard) = enhanced_commit_guard(state, generation) else {
        return;
    };
    match armed {
        Ok(launch) => {
            if let Some(process_identity) = launch
                .is_some()
                .then(crate::commands::runtime::codex_process_identity)
                .flatten()
            {
                state.mark_restart_required_for_process(
                    crate::model::RuntimeNotice::new("enhancedDesktopRuntimeChanged"),
                    process_identity,
                );
            }
        }
        Err(error) => {
            let _ = crate::enhanced_runtime::release_desktop_launch(&state.data_root());
            state.record_live_applied(
                crate::model::RuntimeNotice::new("enhancedDesktopRuntimeNotArmed")
                    .with("detail", error.to_string()),
            );
        }
    }
}

fn leftover_vellum_host_detail() -> Option<String> {
    let peers = crate::enhanced_runtime::process_info::leftover_vellum_hosts();
    if peers.is_empty() {
        None
    } else {
        Some(crate::enhanced_runtime::process_info::format_process_images(&peers))
    }
}

/// Poll this process's own proxy with the key it just wrote into the Codex
/// config, bounded so a genuinely stuck listener still fails `start_proxy`
/// instead of hanging the command.
///
/// Uses a raw HTTP/1.1 GET over TCP so start does not pay TLS client
/// construction on the transaction path.
async fn self_check_ready(port: u16, boundary_key: &str) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let request = format!(
        "GET /readyz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{}: {boundary_key}\r\nConnection: close\r\n\r\n",
        vellum_proxy_runtime::BOUNDARY_KEY_HEADER
    );
    loop {
        if ready_over_tcp(port, &request).await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn ready_over_tcp(port: u16, request: &str) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let connect = tokio::time::timeout(
        Duration::from_millis(50),
        tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
    )
    .await;
    let Ok(Ok(mut stream)) = connect else {
        return false;
    };
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }
    let mut buf = [0u8; 96];
    let read = tokio::time::timeout(Duration::from_millis(50), stream.read(&mut buf)).await;
    match read {
        Ok(Ok(n)) if n > 0 => {
            let head = std::str::from_utf8(&buf[..n]).unwrap_or("");
            head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200")
        }
        _ => false,
    }
}

const PROXY_STOP_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(400);

/// Immediate stop: cancel the current generation, restore Vellum-owned
/// settings, and schedule bounded history maintenance off the button path.
pub(crate) async fn stop_proxy_gracefully(state: &AppState) -> AppResult<()> {
    let _gate = state.lifecycle_lock().lock().await;
    stop_proxy_locked(state).await
}

async fn stop_proxy_locked(state: &AppState) -> AppResult<()> {
    let generation = state.bump_proxy_generation();
    let _ = state.apply_status_if_generation(generation.id, |status| {
        status.phase = ProxyPhase::Stopping;
        status.stage = Some("cancel".into());
    });
    let _outcome = state
        .stop_proxy_and_wait_idle(PROXY_STOP_IDLE_TIMEOUT)
        .await;
    state.schedule_stopped_maintenance(generation.id);

    let paths = CodexPaths::discover(&state.data_root());
    // Config restoration and bridge disarming are independent shared-state
    // repairs. Always attempt both, so one failure cannot strand the other.
    // Only Vellum-owned settings and the launch lease are restored; the
    // user's native Codex daemon is not killed.
    let restore_result = restore_proxy_config(&paths);
    let bridge_release = crate::enhanced_runtime::release_desktop_launch(&state.data_root());
    let result = match (restore_result, bridge_release) {
        (Ok(_), Ok(_)) => Ok(()),
        (Err(config), Ok(_)) => Err(config),
        (Ok(_), Err(bridge)) => Err(AppError::Message(bridge.to_string())),
        (Err(config), Err(bridge)) => Err(AppError::Message(format!(
            "proxy restore failed: {config}; Enhanced bridge release also failed: {bridge}"
        ))),
    };
    if state.is_generation_current(generation.id) {
        state.set_proxy_running(
            false,
            None,
            has_active_lease(&paths),
            result.as_ref().err().map(ToString::to_string),
        );
        state.clear_active_proxy_routes();
        state.set_active_catalog_version(None);
        state.set_draining(false);
        update_codex_restart_after_stop(state, crate::commands::runtime::codex_process_identity());
    }
    result
}

#[tauri::command]
pub async fn stop_proxy_and_restore(state: State<'_, AppState>) -> AppResult<ProxyStatus> {
    stop_proxy_gracefully(&state).await?;
    Ok(state.proxy_status())
}

#[tauri::command]
pub async fn repair_codex_config(state: State<'_, AppState>) -> AppResult<RestoreResult> {
    let _gate = state.lifecycle_lock().lock().await;
    let was_running = state.proxy_status().running;
    let paths = CodexPaths::discover(&state.data_root());
    let had_catalog = paths.catalog.exists();
    // Drain + wait for idle before restore so active streams cannot write
    // history after config/catalog mutation begins.
    stop_proxy_locked(&state).await?;
    // Idempotent: stop_proxy_gracefully already restored once.
    let restored = restore_proxy_config(&paths)?;
    let mut cleared = Vec::new();
    if was_running {
        cleared.push("已停止 Vellum 本機 Proxy".into());
    }
    if restored || was_running {
        cleared.push("已還原 Codex provider 與代理網址".into());
        cleared.push("已還原 Codex 原生 model_providers 設定".into());
        cleared.push("已移除 Codex 的 Vellum 模型型錄參照".into());
    }
    if had_catalog {
        cleared.push("已刪除 Vellum 產生的模型型錄檔".into());
    }
    Ok(RestoreResult {
        status: state.proxy_status(),
        changed: restored || had_catalog || was_running,
        cleared,
        preserved: vec![
            "ChatGPT／Codex 登入憑證".into(),
            "聊天紀錄與工作階段".into(),
            "專案分組與工作區資料".into(),
        ],
    })
}

#[tauri::command]
pub async fn exit_vellum(app: tauri::AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    let _ = stop_proxy_gracefully(&state).await;
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_is_only_required_when_codex_was_already_running() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());

        update_codex_restart_requirement(&state, None);
        let stopped = state.runtime_status();
        assert!(!stopped.restart_required);
        assert!(stopped.live_applied.is_empty());

        update_codex_restart_requirement(&state, Some("100:1234".into()));
        let running = state.runtime_status();
        assert!(running.restart_required);
        assert!(running
            .restart_reasons
            .iter()
            .any(|item| item.code == "codexRunning"));
    }

    #[tokio::test]
    async fn stop_does_not_vacuum_on_the_button_path() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let before = state.history_store().storage_telemetry().unwrap();
        stop_proxy_gracefully(&state).await.unwrap();
        let after = state.history_store().storage_telemetry().unwrap();
        assert_eq!(before.last_vacuum_at, after.last_vacuum_at);
        assert_eq!(before.last_maintenance_at, after.last_maintenance_at);
        let status = state.proxy_status();
        assert!(!status.running);
        assert_eq!(status.phase, ProxyPhase::Stopped);
    }

    #[test]
    fn stale_generation_results_are_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        let first = state.bump_proxy_generation();
        let _second = state.bump_proxy_generation();
        assert!(!state.apply_status_if_generation(first.id, |status| {
            status.running = true;
            status.phase = ProxyPhase::Running;
        }));
        let status = state.proxy_status();
        assert!(!status.running);
        assert_ne!(status.phase, ProxyPhase::Running);
        assert!(!status.running);
    }

    #[test]
    fn status_exposes_every_lifecycle_phase_and_generation_lock() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_test_fixtures(temp.path().to_path_buf());
        let first = state.bump_proxy_generation();
        assert!(first.id > 0);
        assert_eq!(state.proxy_status().generation, first.id);
        assert!(state.proxy_status().operation_id.is_some());

        for phase in [
            ProxyPhase::Preparing,
            ProxyPhase::Starting,
            ProxyPhase::Running,
            ProxyPhase::Stopping,
            ProxyPhase::Stopped,
            ProxyPhase::Failed,
        ] {
            assert!(state.apply_status_if_generation(first.id, |status| {
                status.phase = phase;
                status.running = phase == ProxyPhase::Running;
            }));
            let status = state.proxy_status();
            assert_eq!(status.phase, phase);
            assert_eq!(status.running, phase == ProxyPhase::Running);
        }

        let second = state.bump_proxy_generation();
        assert_ne!(second.id, first.id);
        assert!(!state.apply_status_if_generation(first.id, |status| {
            status.phase = ProxyPhase::Running;
            status.running = true;
        }));
        let locked = state.proxy_status();
        assert_eq!(locked.generation, second.id);
        assert_ne!(locked.phase, ProxyPhase::Running);
        assert!(!locked.running);
    }

    #[test]
    fn proxy_status_keeps_running_beside_phase() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());
        state.set_proxy_running(true, Some("catalog".into()), true, None);
        let status = state.proxy_status();
        assert!(status.running);
        assert_eq!(status.phase, ProxyPhase::Running);
        state.set_proxy_running(false, None, false, Some("boom".into()));
        let failed = state.proxy_status();
        assert!(!failed.running);
        assert_eq!(failed.phase, ProxyPhase::Failed);
    }

    #[test]
    fn stopping_proxy_keeps_restart_required_until_live_codex_reloads() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_data_dir(temp.path().to_path_buf());

        update_codex_restart_after_stop(&state, Some("100:1234".into()));
        let running = state.runtime_status();
        assert!(running.restart_required);
        assert!(running
            .restart_reasons
            .iter()
            .any(|item| item.code == "proxyStoppedCodexRestartRequired"));

        update_codex_restart_after_stop(&state, None);
        assert!(!state.runtime_status().restart_required);
    }

    fn prepared_snapshot(catalog_path: &std::path::Path) -> PrepareSnapshot {
        std::fs::write(catalog_path, "{\"models\":[]}").unwrap();
        PrepareSnapshot {
            executable: None,
            lockfile: None,
            packaged_lockfile_digest: None,
            protocol_version: vellum_proxy_runtime::PROXY_RUNTIME_VERSION.to_string(),
            settings_fingerprint: "test".into(),
            capability_ok: true,
            schema_ok: true,
            schema_ran: true,
            capability_ran: true,
            catalog_ran: true,
            stages: vec![crate::proxy_prepare::PrepareStage {
                name: "schema",
                elapsed_ms: 1,
            }],
        }
    }

    fn isolated_state() -> (tempfile::TempDir, AppState) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("codex-home")).unwrap();
        std::fs::write(
            root.join("codex-home").join("config.toml"),
            "# user config\n",
        )
        .unwrap();
        let state = AppState::with_test_fixtures(root.to_path_buf());
        (temp, state)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopped_generation_cannot_commit_enhanced_or_release_new_launch() {
        let (_temp, state) = isolated_state();
        let old = state.bump_proxy_generation();
        state.set_proxy_running(true, None, true, None);
        let (prepared_tx, prepared_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let worker_state = state.clone();
        let worker = tokio::task::spawn_blocking(move || {
            prepared_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
            enhanced_commit_guard(&worker_state, old.id).is_ok()
        });
        prepared_rx.await.unwrap();
        stop_proxy_gracefully(&state).await.unwrap();
        let new = state.bump_proxy_generation();
        state.set_proxy_running(true, None, true, None);
        resume_tx.send(()).unwrap();
        assert!(
            !worker.await.unwrap(),
            "old verification must not commit after Stop/Start"
        );
        assert_eq!(state.proxy_status().generation, new.id);
        assert!(state.proxy_status().running);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn occupied_port_finishes_start_as_failed() {
        let (_temp, state) = isolated_state();
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let host = crate::proxy_prepare::CountingPrepareHost::new(&state.data_root());
        let error = start_proxy_with_host(&state, &host, port)
            .await
            .expect_err("occupied port");
        assert!(error.to_string().contains("ProxyPortOwnershipConflict"));
        let status = state.proxy_status();
        assert_eq!(status.phase, ProxyPhase::Failed);
        assert!(!status.running);
        assert!(status
            .last_error
            .unwrap()
            .contains("ProxyPortOwnershipConflict"));
    }

    struct SlowPrepareHost {
        inner: crate::proxy_prepare::CountingPrepareHost,
        delay: std::time::Duration,
    }

    impl PrepareHost for SlowPrepareHost {
        fn executable_path(&self) -> Option<std::path::PathBuf> {
            self.inner.executable_path()
        }
        fn lockfile_path(&self) -> Option<std::path::PathBuf> {
            self.inner.lockfile_path()
        }
        fn protocol_version(&self) -> String {
            self.inner.protocol_version()
        }
        fn settings_fingerprint(&self) -> String {
            self.inner.settings_fingerprint()
        }
        fn discover_capability(&self) -> Result<(), String> {
            std::thread::sleep(self.delay);
            self.inner.discover_capability()
        }
        fn probe_schema(&self) -> Result<(), String> {
            self.inner.probe_schema()
        }
        fn precompute_catalog(&self) -> Result<(), String> {
            self.inner.precompute_catalog()
        }
    }

    fn percentile(sorted: &[u128], p: f64) -> u128 {
        if sorted.is_empty() {
            return 0;
        }
        let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    /// Prepared start/stop against the real bind + serve + readiness +
    /// diversion path. Schema/capability/catalog prepare is not on this
    /// path. Isolated `codex-home` under data_root so the user's `~/.codex`
    /// is not touched. Eleven runs so p50/p95/p99/max/fail-rate are real.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prepared_start_and_stop_twice_under_500ms() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("codex-home")).unwrap();
        std::fs::write(root.join("codex-home").join("config.toml"), "").unwrap();
        let state = AppState::with_test_fixtures(root.to_path_buf());
        let catalog = root.join("vellum-model-catalog.json");
        let prepared = prepared_snapshot(&catalog);

        let mut timings = Vec::new();
        let mut start_fails = 0u32;
        let mut stop_fails = 0u32;
        const RUNS: u32 = 11;
        for run in 1..=RUNS {
            let generation = state.bump_proxy_generation();
            let started = Instant::now();
            let status = start_proxy_transaction_on(&state, &prepared, generation.id, 0)
                .await
                .unwrap_or_else(|error| panic!("run {run} start failed: {error}"));
            let start_ms = started.elapsed().as_millis();
            assert!(status.running, "run {run} should be running");
            assert_eq!(status.phase, ProxyPhase::Running);
            assert!(
                status.stage.as_deref() != Some("schema"),
                "start transaction must not report a schema stage"
            );
            eprintln!(
                "prepared-start run={run} start_ms={start_ms} stage={:?} stage_ms={:?}",
                status.stage, status.stage_elapsed_ms
            );

            let stopping = Instant::now();
            stop_proxy_gracefully(&state).await.unwrap();
            let stop_ms = stopping.elapsed().as_millis();
            assert!(!state.proxy_status().running);
            if start_ms >= 500 {
                start_fails += 1;
            }
            if stop_ms >= 500 {
                stop_fails += 1;
            }
            // Debug timings are diagnostic samples only. Optimizer, LTO and
            // stripped release code are part of the 500ms acceptance target.
            #[cfg(not(debug_assertions))]
            assert!(
                start_ms < 500,
                "run {run} prepared start {start_ms}ms exceeds 500ms (stages {:?})",
                status.stage
            );
            #[cfg(not(debug_assertions))]
            assert!(stop_ms < 500, "run {run} stop {stop_ms}ms exceeds 500ms");
            timings.push((run, start_ms, stop_ms));
        }
        let mut starts: Vec<u128> = timings.iter().map(|(_, s, _)| *s).collect();
        let mut stops: Vec<u128> = timings.iter().map(|(_, _, s)| *s).collect();
        starts.sort_unstable();
        stops.sort_unstable();
        eprintln!(
            "prepared-start percentiles n={} p50={} p95={} p99={} max={} fail_rate={}/{}",
            starts.len(),
            percentile(&starts, 50.0),
            percentile(&starts, 95.0),
            percentile(&starts, 99.0),
            starts.last().copied().unwrap_or(0),
            start_fails,
            RUNS
        );
        eprintln!(
            "prepared-stop percentiles n={} p50={} p95={} p99={} max={} fail_rate={}/{}",
            stops.len(),
            percentile(&stops, 50.0),
            percentile(&stops, 95.0),
            percentile(&stops, 99.0),
            stops.last().copied().unwrap_or(0),
            stop_fails,
            RUNS
        );
        for (run, start_ms, stop_ms) in timings {
            eprintln!("prepared-start-stop run={run} start_ms={start_ms} stop_ms={stop_ms}");
        }
    }

    #[test]
    fn lifecycle_watch_receives_generation_keyed_status() {
        let temp = tempfile::tempdir().unwrap();
        let state = AppState::with_test_fixtures(temp.path().to_path_buf());
        let rx = state.subscribe_proxy_lifecycle();
        let first = state.bump_proxy_generation();
        assert_eq!(rx.borrow().generation, first.id);
        assert!(state.apply_status_if_generation(first.id, |status| {
            status.phase = ProxyPhase::Preparing;
            status.stage = Some("discovery".into());
        }));
        assert_eq!(rx.borrow().phase, ProxyPhase::Preparing);
        assert_eq!(rx.borrow().stage.as_deref(), Some("discovery"));
        let second = state.bump_proxy_generation();
        assert_eq!(rx.borrow().generation, second.id);
        assert!(rx.borrow().stage.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn readiness_failure_does_not_apply_diversion() {
        let (_temp, state) = isolated_state();
        let catalog = state.data_root().join("vellum-model-catalog.json");
        let prepared = prepared_snapshot(&catalog);
        let config_path = state.data_root().join("codex-home").join("config.toml");
        let before = std::fs::read_to_string(&config_path).unwrap();
        std::fs::write(state.data_root().join("force-ready-failure"), b"fail").unwrap();
        let generation = state.bump_proxy_generation();
        let error = start_proxy_transaction_on(&state, &prepared, generation.id, 0)
            .await
            .expect_err("readiness must fail");
        assert!(
            error
                .to_string()
                .contains("ProxyBoundaryAuthenticationFailed"),
            "{error}"
        );
        let after = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(
            after, before,
            "readiness failure must restore_proxy_config / never apply_proxy_config"
        );
        assert!(
            !after.contains("model_providers.vellum"),
            "readiness failure must not write the Vellum provider"
        );
        assert!(!state.proxy_status().running);
        assert_eq!(state.proxy_status().phase, ProxyPhase::Failed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schema_not_ok_leaves_proxy_running_without_arming_enhanced() {
        let (_temp, state) = isolated_state();
        let catalog = state.data_root().join("vellum-model-catalog.json");
        let mut prepared = prepared_snapshot(&catalog);
        prepared.schema_ok = false;
        let generation = state.bump_proxy_generation();
        let status = start_proxy_transaction_on(&state, &prepared, generation.id, 0)
            .await
            .expect("proxy may start when schema is unverified");
        assert!(status.running);
        assert_eq!(status.phase, ProxyPhase::Running);
        let runtime = state.runtime_status();
        assert!(
            runtime
                .live_applied
                .iter()
                .any(|notice| notice.code == "enhancedDesktopRuntimeNotArmed"),
            "schema failure must report Enhanced not armed: {:?}",
            runtime.live_applied
        );
        assert!(
            !runtime
                .restart_reasons
                .iter()
                .any(|notice| notice.code == "enhancedDesktopRuntimeChanged"),
            "schema failure must not report Enhanced armed"
        );
        stop_proxy_gracefully(&state).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_double_start_joins_one_generation() {
        let (_temp, state) = isolated_state();
        let root = state.data_root();
        let host: std::sync::Arc<dyn PrepareHost> = std::sync::Arc::new(SlowPrepareHost {
            inner: crate::proxy_prepare::CountingPrepareHost::new(&root),
            delay: std::time::Duration::from_millis(200),
        });
        set_test_start_override(root.clone(), std::sync::Arc::clone(&host), 0);
        let first = {
            let state = state.clone();
            tokio::spawn(async move { start_proxy_inner(&state).await })
        };
        let second = {
            let state = state.clone();
            tokio::spawn(async move { start_proxy_inner(&state).await })
        };
        let first = first.await.unwrap().expect("first start");
        let second = second.await.unwrap().expect("second start");
        assert!(first.running);
        assert!(second.running);
        assert_eq!(first.generation, second.generation);
        assert_eq!(state.proxy_status().phase, ProxyPhase::Running);
        assert_eq!(state.proxy_status().generation, first.generation);
        stop_proxy_gracefully(&state).await.unwrap();
        clear_test_start_override(&root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_while_preparing_does_not_leave_mixed_phase() {
        let (_temp, state) = isolated_state();
        let root = state.data_root();
        let host: std::sync::Arc<dyn PrepareHost> = std::sync::Arc::new(SlowPrepareHost {
            inner: crate::proxy_prepare::CountingPrepareHost::new(&root),
            delay: std::time::Duration::from_millis(800),
        });
        set_test_start_override(root.clone(), host, 0);
        let starting = {
            let state = state.clone();
            tokio::spawn(async move { start_proxy_inner(&state).await })
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let status = state.proxy_status();
            if status.phase == ProxyPhase::Preparing {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("start never reached preparing: {status:?}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        stop_proxy_gracefully(&state).await.unwrap();
        let start_result = starting.await.unwrap();
        assert!(
            start_result.is_err(),
            "in-flight prepare must fail after stop: {start_result:?}"
        );
        let status = state.proxy_status();
        assert!(!status.running);
        assert_eq!(status.phase, ProxyPhase::Stopped);
        assert!(
            status.generation > 0,
            "stop must own the current generation"
        );
        clear_test_start_override(&root);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_while_starting_does_not_mix_generation() {
        let (_temp, state) = isolated_state();
        let root = state.data_root();
        let host: std::sync::Arc<dyn PrepareHost> = std::sync::Arc::new(SlowPrepareHost {
            inner: crate::proxy_prepare::CountingPrepareHost::new(&root),
            delay: std::time::Duration::from_millis(50),
        });
        set_test_start_override(root.clone(), std::sync::Arc::clone(&host), 0);
        let first = {
            let state = state.clone();
            tokio::spawn(async move { start_proxy_inner(&state).await })
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let phase = state.proxy_status().phase;
            if matches!(phase, ProxyPhase::Preparing | ProxyPhase::Starting) {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("first start never left stopped");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let second = start_proxy_inner(&state)
            .await
            .expect("overlapping start must join");
        let first = first.await.unwrap().expect("first start");
        assert_eq!(first.generation, second.generation);
        assert!(second.running);
        assert_eq!(state.proxy_status().phase, ProxyPhase::Running);
        stop_proxy_gracefully(&state).await.unwrap();
        clear_test_start_override(&root);
    }
}
