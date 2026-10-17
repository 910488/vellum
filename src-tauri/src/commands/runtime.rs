use crate::enhanced_runtime::{
    adopted_by_codex_desktop, BridgeAttestationV1, BridgeLifecycle, DesktopRuntimeLaunch,
};
use crate::error::{AppError, AppResult};
use crate::model::{CatalogVersion, RestartResult, RuntimeNotice, RuntimeStatus};
use crate::state::AppState;
use std::path::PathBuf;
use std::time::Duration;
use tauri::{Manager, State};

/// How long a packaged Codex Desktop gets to start, adopt the bridge, and let
/// both children finish `initialize`. A cold start of a packaged app on Windows
/// is routinely slower than the old "did a new PID appear" check assumed.
const BRIDGE_READY_TIMEOUT: Duration = Duration::from_secs(90);
const BRIDGE_POLL_INTERVAL: Duration = Duration::from_millis(250);
static CODEX_RESTART_FLIGHT: tokio::sync::Mutex<
    Option<tokio::sync::watch::Receiver<RestartFlight>>,
> = tokio::sync::Mutex::const_new(None);

#[derive(Clone)]
enum RestartFlight {
    Pending,
    Done(std::sync::Arc<Result<ManagedRestart, String>>),
}

#[derive(Debug)]
struct CodexLaunchTarget {
    pid: u32,
    #[cfg(target_os = "macos")]
    started_at: Option<String>,
    executable: PathBuf,
    app_id: Option<String>,
}

#[tauri::command]
pub async fn get_runtime_status(state: State<'_, AppState>) -> AppResult<RuntimeStatus> {
    let state = state.inner().clone();
    off_main_thread(move || {
        state.reconcile_codex_restart(codex_process_identity());
        state.runtime_status()
    })
    .await
}

/// Runs `work` off the thread that draws the window.
///
/// Tauri executes a synchronous command on the main thread, and reading this
/// runtime's status is not cheap work: verifying the settings re-hashes the
/// Enhanced core, which is a third of a gigabyte. Doing that on the main thread
/// froze the whole window — and because the status is polled, it froze on every
/// poll, not only while injecting.
///
/// `async` alone would already move the command off the main thread; the
/// blocking pool is what keeps a second of hashing from also stalling every
/// other async task sharing that worker.
async fn off_main_thread<T, F>(work: F) -> AppResult<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| AppError::Message(error.to_string()))
}

#[tauri::command]
pub async fn get_enhanced_desktop_runtime_status(
    state: State<'_, AppState>,
) -> AppResult<crate::enhanced_runtime::DesktopRuntimeStatus> {
    let status = read_desktop_runtime_status(&state).await?;
    repair_superseded_launch(&state, &status);
    restore_cleared_lease(&state, &status);
    Ok(status)
}

/// Puts back a `CODEX_CLI_PATH` that something cleared under a held lease.
///
/// The live launch never notices — Desktop read the variable when it started —
/// so this used to surface only as a restart badge, and following it dropped
/// Desktop onto native Codex. Re-acquiring needs no restart and no consent: an
/// absent value has no other owner. A foreign value is left alone.
fn restore_cleared_lease(state: &AppState, status: &crate::enhanced_runtime::DesktopRuntimeStatus) {
    if !status.enabled
        || status.environment_state != "leaseValueMissing"
        || !state.proxy_status().running
    {
        return;
    }
    let state = state.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // Launch publication owns the lease; let it finish and retry next poll.
        let Ok(_guard) = state.lifecycle_lock().try_lock() else {
            return;
        };
        let data_root = state.data_root();
        let Some(lease) = crate::enhanced_runtime::env_lease::EnvironmentLease::read(&data_root)
        else {
            return;
        };
        match crate::enhanced_runtime::env_lease::acquire(
            &data_root,
            &lease.applied_value,
            &lease.launch_id,
        ) {
            Ok(_) => log::warn!(
                "[EnvLease] CODEX_CLI_PATH was cleared under lease launch={} acquired_at={}; restored at {}",
                lease.launch_id,
                lease.acquired_at,
                chrono::Utc::now().timestamp()
            ),
            Err(error) => log::warn!(
                "[EnvLease] CODEX_CLI_PATH cleared under lease launch={}; restore failed: {error}",
                lease.launch_id
            ),
        }
    });
}

async fn read_desktop_runtime_status(
    state: &AppState,
) -> AppResult<crate::enhanced_runtime::DesktopRuntimeStatus> {
    let data_root = state.data_root();
    let state = state.clone();
    off_main_thread(move || {
        // Serialize observation and reconciliation with launch publication.
        // A poll of the previous launch must not clear a newer restart notice.
        // A managed restart can hold this lock for 90 seconds. Keep polling
        // responsive; reconcile on the next poll if publication is in flight.
        let guard = state.lifecycle_lock().try_lock();
        let status = crate::enhanced_runtime::desktop_runtime_status(&data_root);
        if guard.is_ok() {
            state.reconcile_enhanced_adoption(
                (status.ready || status.bridge_protection_active) && !status.restart_required,
            );
        }
        status
    })
    .await
}

/// Rebuilds Proxy launch state after Codex Desktop replaces its core.
///
/// Reported as a blocker this was a set of instructions — quit Codex Desktop,
/// stop the Proxy, start the Proxy, launch Codex Desktop — to be carried out
/// in that order, about an update the user never asked for and a stored path
/// Vellum had already corrected on its own. Every step of it is
/// A Proxy restart re-runs the launch transaction against Desktop's newly
/// discovered core. Codex still holds the old child processes, so the runtime
/// status remains restart-required until Codex itself restarts.
///
/// It is bounded on three sides:
/// [`superseded_launch_repair`] says whether this launch may be rebuilt at
/// all, [`AppState::claim_launch_repair`] gives each launch exactly one
/// attempt, and the repair refuses outright while Codex or Proxy is mid-turn —
/// the refusal hands the claim back, because a repair
/// deferred until the turn ends is the behaviour we want, not a spent attempt.
///
/// Detached rather than awaited, because the caller is a poll. `refresh` in
/// the shell awaits this status alongside three others before it renders any
/// of them, and a managed restart waits up to `BRIDGE_READY_TIMEOUT` for the
/// new bridge — so awaiting it here would freeze the whole window for a minute
/// and a half to report a repair the next poll can report by itself. The
/// screen already says a repair is under way, off `launch_core_drift`.
fn repair_superseded_launch(
    state: &AppState,
    status: &crate::enhanced_runtime::DesktopRuntimeStatus,
) {
    let proxy_running = state.proxy_status().running;
    let Some(launch_id) =
        crate::enhanced_runtime::superseded_launch_repair(status, proxy_running).map(str::to_owned)
    else {
        return;
    };
    if !state.claim_launch_repair(&launch_id) {
        return;
    }
    if let Some(process_identity) = codex_process_identity() {
        state.mark_restart_required_for_process(
            RuntimeNotice::new("enhancedDesktopRuntimeChanged"),
            process_identity,
        );
    }
    let state = state.clone();
    tauri::async_runtime::spawn(async move {
        // Close admission before checking for idle, otherwise a request can
        // enter between the check and the Proxy lifecycle lock and be cancelled by
        // what is meant to be a non-disruptive repair.
        state.set_draining(true);
        let bridge = crate::enhanced_runtime::live_bridge_attestation(&state.data_root());
        if state.active_requests() != 0 || codex_turn_refusal(bridge.as_ref(), false).is_some() {
            state.set_draining(false);
            state.release_launch_repair();
            return;
        }
        let repair = async {
            crate::commands::proxy::stop_proxy_gracefully(&state).await?;
            crate::commands::proxy::start_proxy_inner(&state).await?;
            Ok::<_, AppError>(())
        }
        .await;
        let notice = match repair {
            Ok(()) => RuntimeNotice::new("enhancedLaunchCoreRepaired").with("launchId", launch_id),
            Err(error) => RuntimeNotice::new("enhancedLaunchCoreRepairFailed")
                .with("reason", error.to_string()),
        };
        state.record_live_applied(notice);
    });
}

/// Turns Enhanced off and hands `CODEX_CLI_PATH` back to whoever had it.
#[tauri::command]
pub async fn get_enhanced_runtime_overview(
    state: State<'_, AppState>,
) -> AppResult<crate::enhanced_runtime::DesktopRuntimeStatus> {
    get_enhanced_desktop_runtime_status(state).await
}

#[tauri::command]
pub async fn list_enhanced_runtime_sessions(
    state: State<'_, AppState>,
) -> AppResult<crate::enhanced_runtime::observations::RuntimeObservations> {
    let root = state.data_root();
    off_main_thread(move || crate::enhanced_runtime::observations::read(&root)).await
}

#[tauri::command]
pub async fn get_enhanced_remote_control_status(
    state: State<'_, AppState>,
) -> AppResult<crate::enhanced_runtime::observations::RemoteObservation> {
    let root = state.data_root();
    off_main_thread(move || crate::enhanced_runtime::observations::read(&root).remote).await
}

#[tauri::command]
pub async fn recheck_enhanced_runtime_compatibility(
    state: State<'_, AppState>,
) -> AppResult<crate::enhanced_runtime::DesktopRuntimeStatus> {
    crate::enhanced_runtime::desktop_manager::invalidate_protocol_cache();
    get_enhanced_desktop_runtime_status(state).await
}

#[tauri::command]
pub async fn export_enhanced_runtime_diagnostics(state: State<'_, AppState>) -> AppResult<PathBuf> {
    let root = state.data_root();
    off_main_thread(move || {
        crate::enhanced_runtime::observations::export(&root)
            .map_err(|error| AppError::Message(error.to_string()))
    })
    .await?
}

/// Exports every retained text log owned by Vellum into one bounded, redacted
/// ZIP. Configuration, credentials, conversation history, and SQLite stores
/// are deliberately outside the log allowlist in `support_bundle`.
#[tauri::command]
pub async fn export_vellum_logs(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> AppResult<PathBuf> {
    let data_root = state.data_root();
    let usage = state.usage_store();
    let app_log_root = app.path().app_log_dir().ok();
    let destination = app
        .path()
        .download_dir()
        .unwrap_or_else(|_| data_root.join("support-bundles"));
    off_main_thread(move || {
        let diagnostics = usage.diagnostic_log_for_support(32 * 1024 * 1024)?;
        crate::support_bundle::export(
            &data_root,
            app_log_root.as_deref(),
            &destination,
            Some(diagnostics),
        )
        .map_err(|error| AppError::Message(error.to_string()))
    })
    .await?
}

/// Releases only the launch lease; session ownership is unchanged.
#[tauri::command]
pub async fn disable_enhanced_desktop_runtime(
    state: State<'_, AppState>,
) -> AppResult<crate::enhanced_runtime::DesktopRuntimeStatus> {
    let data_root = state.data_root();
    let status = off_main_thread(move || {
        crate::enhanced_runtime::disable_desktop_runtime(&data_root)
            .map_err(|error| AppError::Message(error.to_string()))?;
        Ok::<_, AppError>(crate::enhanced_runtime::desktop_runtime_status(&data_root))
    })
    .await??;
    if let Some(process_identity) = codex_process_identity() {
        state.mark_restart_required_for_process(
            crate::model::RuntimeNotice::new("enhancedDesktopRuntimeChanged"),
            process_identity,
        );
    }
    Ok(status)
}

/// Runs the installed-Desktop qualification and stores its result.
#[tauri::command]
pub async fn run_enhanced_installed_gate(
    state: State<'_, AppState>,
) -> AppResult<crate::enhanced_runtime::qualification::QualificationResult> {
    crate::eval::enhanced_gate::installed_mode::run_from_desktop(&state).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn configure_enhanced_desktop_runtime(
    official_codex_executable: PathBuf,
    enabled: bool,
    state: State<'_, AppState>,
) -> AppResult<crate::enhanced_runtime::DesktopRuntimeStatus> {
    // Read the catalog on this thread; the closure below outlives the borrow.
    let data_root = state.data_root();
    let routes = state.routes();
    let models = state.model_routes();
    let proxy_running = state.proxy_status().running;
    let status = off_main_thread(move || {
        crate::enhanced_runtime::configure_desktop_runtime(
            &data_root,
            official_codex_executable,
            enabled,
        )
        .map_err(|error| AppError::Message(error.to_string()))?;
        crate::enhanced_runtime::sync_desktop_launch(&data_root, &routes, &models, proxy_running)
            .map_err(|error| AppError::Message(error.to_string()))?;
        Ok::<_, AppError>(crate::enhanced_runtime::desktop_runtime_status(&data_root))
    })
    .await??;
    if let Some(process_identity) = codex_process_identity() {
        state.mark_restart_required_for_process(
            crate::model::RuntimeNotice::new("enhancedDesktopRuntimeChanged"),
            process_identity,
        );
    }
    Ok(status)
}

#[tauri::command]
pub async fn begin_graceful_drain(state: State<'_, AppState>) -> AppResult<RuntimeStatus> {
    state.set_draining(true);
    for _ in 0..100 {
        if state.active_requests() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Ok(state.runtime_status())
}

#[tauri::command]
pub fn cancel_graceful_drain(state: State<'_, AppState>) -> AppResult<RuntimeStatus> {
    state.set_draining(false);
    Ok(state.runtime_status())
}

#[tauri::command]
pub fn list_catalog_versions(state: State<'_, AppState>) -> AppResult<Vec<CatalogVersion>> {
    crate::runtime::list_catalog_versions(&state.data_root())
}

#[tauri::command(rename_all = "camelCase")]
pub fn rollback_catalog_version(
    version_id: String,
    state: State<'_, AppState>,
) -> AppResult<RuntimeStatus> {
    let paths = crate::codex::CodexPaths::discover(&state.data_root());
    crate::runtime::snapshot_catalog(&state.data_root(), &paths.catalog)?;
    crate::runtime::rollback_catalog(&state.data_root(), &version_id, &paths.catalog)?;
    state.set_active_catalog_version(Some(version_id));
    let notice = crate::model::RuntimeNotice::new("catalogRestored");
    match codex_process_identity() {
        Some(identity) => state.mark_restart_required_for_process(notice, identity),
        None => {
            state.mark_restart_required(notice);
            state.set_restart_process_identity(None);
        }
    }
    Ok(state.runtime_status())
}

#[tauri::command]
pub async fn restart_codex_safely(
    state: State<'_, AppState>,
    force: Option<bool>,
) -> AppResult<RestartResult> {
    let force = force.unwrap_or(false);
    let outcome = shared_restart_codex(&state, force).await?;
    Ok(RestartResult {
        restarted: outcome.restarted,
        notice: outcome.notice,
    })
}

/// What a managed restart actually observed. The bridge attestation is kept so
/// the installed qualification can assert on the same evidence the UI shows.
#[derive(Clone)]
pub(crate) struct ManagedRestart {
    pub restarted: bool,
    pub notice: RuntimeNotice,
    pub launch: Option<DesktopRuntimeLaunch>,
    pub attestation: Option<BridgeAttestationV1>,
}

impl ManagedRestart {
    fn refused(notice: RuntimeNotice) -> Self {
        Self {
            restarted: false,
            notice,
            launch: None,
            attestation: None,
        }
    }
}

/// Should a managed restart stop because Codex Desktop is mid-turn?
///
/// Split out from the restart itself so the answer can be tested. Three
/// separate things have to be true before this refuses, and each one is a way
/// the guard could otherwise lock the user out of their own restart button:
/// there is an attestation, the bridge that wrote it is still alive
/// (`live_bridge_attestation` checks that), and the user has not already been
/// told and answered.
pub(crate) fn codex_turn_refusal(
    attestation: Option<&BridgeAttestationV1>,
    force: bool,
) -> Option<RuntimeNotice> {
    if force {
        return None;
    }
    let attestation = attestation?;
    if !attestation.has_open_turn() {
        return None;
    }
    Some(
        RuntimeNotice::new("restartBlockedByCodexTurn")
            .with("count", attestation.open_turns.to_string()),
    )
}

/// Stops Codex Desktop and starts it again.
///
/// When Enhanced is enabled, "started again" is not "a new Codex PID appeared".
/// A new PID proves the app relaunched; it says nothing about which runtime the
/// app is using, and that gap is exactly how Enhanced could report ready while
/// every turn ran on Official Codex. So the Enhanced path waits for the bridge
/// belonging to *this* launch id to attest that it is ready, and reports
/// `EnhancedDesktopBridgeNotObserved` if it never does.
async fn shared_restart_codex(state: &AppState, force: bool) -> AppResult<ManagedRestart> {
    let mut slot = CODEX_RESTART_FLIGHT.lock().await;
    let mut rx = if let Some(rx) = slot.as_ref() {
        rx.clone()
    } else {
        let (tx, rx) = tokio::sync::watch::channel(RestartFlight::Pending);
        *slot = Some(rx.clone());
        let state = state.clone();
        tauri::async_runtime::spawn(async move {
            // A flight that never ends is a button that never answers again:
            // every later click joins it. Bound it, and undo the drain a
            // dropped restart would otherwise leave behind.
            let result = match tokio::time::timeout(
                RESTART_FLIGHT_TIMEOUT,
                restart_codex_managed(&state, force),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    state.set_draining(false);
                    log::warn!(
                        "[Restart] managed restart exceeded {}s and was abandoned",
                        RESTART_FLIGHT_TIMEOUT.as_secs()
                    );
                    Ok(ManagedRestart::refused(RuntimeNotice::new(
                        "restartTimedOut",
                    )))
                }
            };
            let encoded = RestartFlight::Done(std::sync::Arc::new(match result {
                Ok(value) => Ok(value),
                Err(error) => Err(error.to_string()),
            }));
            let _ = tx.send(encoded);
            *CODEX_RESTART_FLIGHT.lock().await = None;
        });
        rx
    };
    drop(slot);
    loop {
        let snapshot = rx.borrow().clone();
        match snapshot {
            RestartFlight::Done(result) => match result.as_ref() {
                Ok(value) => return Ok(value.clone()),
                Err(error) => return Err(AppError::Message(error.clone())),
            },
            RestartFlight::Pending => {
                if rx.changed().await.is_err() {
                    return Err(AppError::Message("restart flight dropped".into()));
                }
            }
        }
    }
}

/// Upper bound for one managed restart: drain, stop, relaunch, and the bridge
/// readiness wait together.
const RESTART_FLIGHT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
/// How long a restart waits for a Proxy start/stop to release the lifecycle.
const RESTART_LIFECYCLE_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

pub(crate) async fn restart_codex_managed(
    state: &AppState,
    force: bool,
) -> AppResult<ManagedRestart> {
    log::info!("[Restart] managed restart requested force={force}");
    let outcome = restart_codex_managed_inner(state, force).await;
    match &outcome {
        Ok(result) => log::info!(
            "[Restart] managed restart finished restarted={} notice={}",
            result.restarted,
            result.notice.code
        ),
        Err(error) => log::warn!("[Restart] managed restart failed: {error}"),
    }
    outcome
}

async fn restart_codex_managed_inner(state: &AppState, force: bool) -> AppResult<ManagedRestart> {
    let Ok(_lifecycle) =
        tokio::time::timeout(RESTART_LIFECYCLE_WAIT, state.lifecycle_lock().lock()).await
    else {
        return Ok(ManagedRestart::refused(RuntimeNotice::new(
            "restartBlockedByLifecycle",
        )));
    };
    state.set_draining(true);
    for _ in 0..100 {
        if state.active_requests() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // The loop above drains Vellum. It says nothing about Codex.
    //
    // `active_requests` counts Proxy traffic, and a Codex Desktop turn on the
    // Official plane never touches the Proxy — so the answer was "idle" while
    // Codex was mid-turn, and restarting on it destroyed a thread Codex had not
    // written anywhere yet. The bridge is the only process that sees those
    // turns, so it is the one to ask.
    if let Some(notice) = codex_turn_refusal(
        crate::enhanced_runtime::live_bridge_attestation(&state.data_root()).as_ref(),
        force,
    ) {
        state.set_draining(false);
        return Ok(ManagedRestart::refused(notice));
    }

    let active = state.active_requests();
    let process = discover_codex_process()?;
    let executable = process.as_ref().map(|target| target.executable.as_path());
    #[cfg(target_os = "windows")]
    let app_id = discover_codex_app_id();
    let precondition = crate::runtime::restart_precondition(active, executable);
    #[cfg(target_os = "windows")]
    let precondition = match precondition {
        Err(_) if active == 0 && process.is_none() && app_id.is_some() => Ok(()),
        other => other,
    };
    if let Err(notice) = precondition {
        state.set_draining(false);
        return Ok(ManagedRestart::refused(notice));
    }
    let previous_identity = process.as_ref().map(launch_target_identity);
    #[cfg(not(target_os = "windows"))]
    let target = process.expect("precondition verified process");
    #[cfg(target_os = "windows")]
    let target = {
        let mut target = process.unwrap_or_else(|| aumid_only_target(app_id.clone().unwrap()));
        target.app_id = app_id;
        target
    };

    // Prepare and verify before changing launch leases or stopping the app.
    // Keep the original target/PID for the stop; only alter the next launch.
    #[cfg(target_os = "windows")]
    let next_target = {
        let root = state.data_root();
        let current = target.executable.clone();
        let next = match off_main_thread(move || {
            if current.as_os_str().is_empty() {
                current
            } else {
                crate::enhanced_runtime::steer_repair::launch_executable(&root, &current)
            }
        })
        .await
        {
            Ok(executable) => executable,
            Err(error) => {
                state.set_draining(false);
                return Err(error);
            }
        };
        CodexLaunchTarget {
            pid: target.pid,
            executable: next,
            app_id: target.app_id.clone(),
        }
    };
    #[cfg(target_os = "windows")]
    let launch_target = &next_target;
    #[cfg(target_os = "macos")]
    let next_target = {
        let root = state.data_root();
        let current = target.executable.clone();
        let next = match off_main_thread(move || {
            crate::enhanced_runtime::macos_send_repair::launch_executable(&root, &current)
        })
        .await
        {
            Ok(executable) => executable,
            Err(error) => {
                state.set_draining(false);
                return Err(error);
            }
        };
        CodexLaunchTarget {
            pid: target.pid,
            started_at: target.started_at.clone(),
            executable: next,
            app_id: target.app_id.clone(),
        }
    };
    #[cfg(target_os = "macos")]
    let launch_target = &next_target;
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let launch_target = &target;

    // The launch lease is a Proxy transaction, not a restart side-effect.
    // Preparing it here while the Proxy is stopped would leave CODEX_CLI_PATH
    // armed for the next Desktop launch with no data plane behind it.
    if crate::updates::next_start_should_apply_pending(&state.data_root()) {
        crate::updates::arm_core_pending_apply();
    }
    let launch = match crate::enhanced_runtime::sync_desktop_launch(
        &state.data_root(),
        &state.routes(),
        &state.model_routes(),
        state.proxy_status().running,
    ) {
        Ok(launch) => launch,
        Err(error) => {
            state.set_draining(false);
            return Err(AppError::Message(error.to_string()));
        }
    };

    #[cfg(target_os = "windows")]
    let stop_attestation = crate::enhanced_runtime::live_bridge_attestation(&state.data_root());
    #[cfg(target_os = "windows")]
    if target.pid != 0 {
        // Launch preparation can take time; check again immediately before
        // sending WM_CLOSE so a newly opened Official turn is also protected.
        if let Some(notice) = codex_turn_refusal(stop_attestation.as_ref(), force) {
            state.set_draining(false);
            return Ok(ManagedRestart::refused(notice));
        }
    }
    let stopped = match if target.pid == 0 {
        Ok(StopObservation::not_running())
    } else {
        #[cfg(target_os = "windows")]
        let result = stop_windows_codex_and_wait(
            &target,
            force,
            &state.data_root(),
            stop_attestation.as_ref(),
        )
        .await;
        #[cfg(not(target_os = "windows"))]
        let result = stop_codex_and_wait(&target, force).await;
        result
    } {
        Ok(stopped) => stopped,
        Err(error) => {
            state.set_draining(false);
            return Err(AppError::Message(format!("無法關閉 Codex 程序：{error}")));
        }
    };
    log::info!(
        "[Restart] stop method={} exited={} exit_code={:?} force_used={}",
        stopped.stop_method,
        stopped.exited,
        stopped.exit_code,
        stopped.force_used
    );
    if !stopped.exited {
        state.set_draining(false);
        return Ok(ManagedRestart::refused(stopped.notice()));
    }

    #[cfg(target_os = "macos")]
    if let Err(error) = quiesce_macos_codex_relaunches(target.pid).await {
        state.set_draining(false);
        return Err(AppError::Message(format!(
            "無法穩定關閉自動重新啟動的 Codex 程序：{error}"
        )));
    }

    if let Err(error) = launch_codex(launch_target, launch.as_ref()) {
        state.set_draining(false);
        return Err(AppError::Message(format!("無法重新啟動 Codex：{error}")));
    }

    let Some(launch) = launch else {
        // No Enhanced runtime configured, so a new Codex process is the whole
        // claim being made and a new PID is the right evidence for it.
        for _ in 0..50 {
            if codex_process_identity().is_some_and(|identity| Some(identity) != previous_identity)
            {
                state.clear_restart_required();
                state.set_draining(false);
                return Ok(ManagedRestart {
                    restarted: true,
                    notice: stopped.annotate(RuntimeNotice::new("restartSucceeded")),
                    launch: None,
                    attestation: None,
                });
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        state.set_draining(false);
        return Ok(ManagedRestart::refused(
            stopped.annotate(RuntimeNotice::new("restartNotDetected")),
        ));
    };

    let observed = await_bridge_ready(&launch).await;
    state.set_draining(false);
    match observed {
        BridgeObservation::Ready(attestation) => {
            let _ = crate::updates::conclude_pending_core(
                &state.data_root(),
                core_promote_input(&state.data_root(), &attestation, &launch.launch_id, true),
            );
            state.clear_restart_required();
            Ok(ManagedRestart {
                restarted: true,
                notice: stopped.annotate(
                    RuntimeNotice::new("enhancedDesktopBridgeReady")
                        .with("launchId", launch.launch_id.clone()),
                ),
                launch: Some(launch),
                attestation: Some(attestation),
            })
        }
        BridgeObservation::Failed(attestation) => {
            let _ = crate::updates::conclude_pending_core(
                &state.data_root(),
                core_promote_input(&state.data_root(), &attestation, &launch.launch_id, false),
            );
            let reason = attestation
                .failure_reason
                .clone()
                .unwrap_or_else(|| attestation.state.as_str().to_string());
            Ok(ManagedRestart {
                restarted: false,
                notice: stopped.annotate(
                    RuntimeNotice::new("enhancedDesktopBridgeFailed").with("reason", reason),
                ),
                launch: Some(launch),
                attestation: Some(attestation),
            })
        }
        BridgeObservation::NotObserved(attestation) => Ok(ManagedRestart {
            restarted: false,
            notice: stopped.annotate(
                RuntimeNotice::new("enhancedDesktopBridgeNotObserved")
                    .with("launchId", launch.launch_id.clone()),
            ),
            launch: Some(launch),
            attestation,
        }),
    }
}

pub(crate) enum BridgeObservation {
    Ready(BridgeAttestationV1),
    Failed(BridgeAttestationV1),
    NotObserved(Option<BridgeAttestationV1>),
}

fn core_promote_input(
    data_root: &std::path::Path,
    attestation: &BridgeAttestationV1,
    launch_id: &str,
    ready: bool,
) -> crate::updates::CorePromoteInput {
    let pending_verified = crate::updates::pending_core(data_root)
        .is_some_and(|pending| crate::updates::signed_core_digest(data_root, &pending.digest));
    crate::updates::CorePromoteInput {
        pending_verified,
        bridge_compatible: if ready {
            attestation.is_active_for(launch_id)
        } else {
            attestation.matches_launch(launch_id)
        },
        official_protocol_ok: attestation.official.initialized && !attestation.official.exited,
        attestation_ok: ready,
        user_turn_accepted: attestation.has_open_turn() || attestation.session_features_applied > 0,
    }
}

async fn await_bridge_ready(launch: &DesktopRuntimeLaunch) -> BridgeObservation {
    let deadline = std::time::Instant::now() + BRIDGE_READY_TIMEOUT;
    let mut last = None;
    while std::time::Instant::now() < deadline {
        if let Ok(attestation) = BridgeAttestationV1::read(&launch.attestation_path) {
            if attestation.matches_launch(&launch.launch_id) {
                if attestation.is_active_for(&launch.launch_id)
                    && adopted_by_codex_desktop(&attestation)
                {
                    return BridgeObservation::Ready(attestation);
                }
                if attestation.state == BridgeLifecycle::Failed {
                    return BridgeObservation::Failed(attestation);
                }
                last = Some(attestation);
            }
        }
        tokio::time::sleep(BRIDGE_POLL_INTERVAL).await;
    }
    BridgeObservation::NotObserved(last)
}

#[cfg(target_os = "windows")]
fn discover_codex_process() -> AppResult<Option<CodexLaunchTarget>> {
    let candidates = codex_process_snapshot()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(pid, parent, path)| {
            (pid != std::process::id()
                && path.is_absolute()
                && path.exists()
                && is_codex_desktop_executable(&path))
            .then_some((pid, parent, path))
        })
        .collect::<Vec<_>>();
    Ok(
        select_codex_desktop_root(candidates).map(|(pid, executable)| CodexLaunchTarget {
            pid,
            #[cfg(target_os = "macos")]
            started_at: None,
            executable,
            app_id: None,
        }),
    )
}

#[cfg(target_os = "windows")]
fn is_codex_desktop_executable(path: &std::path::Path) -> bool {
    let normalized = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    let known_install = normalized.contains("\\windowsapps\\openai.codex_")
        || normalized.contains("\\programs\\openai\\codex\\");
    (known_install
        && (normalized.ends_with("\\chatgpt.exe") || normalized.ends_with("\\codex.exe")))
        || crate::enhanced_runtime::steer_repair::is_verified_copy(path)
}

#[cfg(target_os = "windows")]
fn select_codex_desktop_root(candidates: Vec<(u32, u32, PathBuf)>) -> Option<(u32, PathBuf)> {
    use std::collections::HashSet;

    let pids = candidates
        .iter()
        .map(|(pid, _, _)| *pid)
        .collect::<HashSet<_>>();
    candidates
        .into_iter()
        .filter(|(_, parent, _)| !pids.contains(parent))
        .max_by_key(|(pid, _, _)| *pid)
        .map(|(pid, _, path)| (pid, path))
}

#[cfg(target_os = "windows")]
fn codex_process_snapshot() -> Option<Vec<(u32, u32, PathBuf)>> {
    windows_process_snapshot(true)
}

#[cfg(target_os = "windows")]
fn windows_process_snapshot(query_desktop_paths: bool) -> Option<Vec<(u32, u32, PathBuf)>> {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut processes = Vec::new();
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let mut path = PathBuf::new();
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|value| *value == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                if query_desktop_paths
                    && (name.eq_ignore_ascii_case("ChatGPT.exe")
                        || name.eq_ignore_ascii_case("Codex.exe"))
                {
                    let process =
                        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID);
                    if !process.is_null() {
                        let mut buffer = vec![0_u16; 32_768];
                        let mut length = buffer.len() as u32;
                        if QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length)
                            != 0
                        {
                            path =
                                PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]));
                        }
                        CloseHandle(process);
                    }
                }
                // Retain even inaccessible processes: ancestry must not depend
                // on whether their executable path can be queried.
                processes.push((entry.th32ProcessID, entry.th32ParentProcessID, path));
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        Some(processes)
    }
}

#[cfg(not(target_os = "windows"))]
fn discover_codex_process() -> AppResult<Option<CodexLaunchTarget>> {
    #[cfg(target_os = "macos")]
    {
        let mut pgrep = crate::process::background_command("pgrep");
        pgrep.args([
            "-f",
            "(ChatGPT\\.app/Contents/MacOS/ChatGPT|Codex\\.app/Contents/MacOS/Codex)",
        ]);
        let output = pgrep
            .output()
            .map_err(|error| AppError::Message(format!("cannot inspect Codex: {error}")))?;
        if output.status.success() {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let Ok(pid) = line.trim().parse::<u32>() else {
                    continue;
                };
                if pid == std::process::id() {
                    continue;
                }
                let mut ps = crate::process::background_command("ps");
                ps.args(["-p", &pid.to_string(), "-o", "comm="]);
                let executable = ps
                    .output()
                    .ok()
                    .filter(|result| result.status.success())
                    .and_then(|result| {
                        let value = String::from_utf8_lossy(&result.stdout).trim().to_string();
                        let path = PathBuf::from(value);
                        path.is_absolute().then_some(path)
                    })
                    .filter(|path| path.exists())
                    .or_else(|| {
                        crate::install_paths::macos_codex_desktop_executable_candidates(
                            dirs::home_dir().as_deref(),
                        )
                        .into_iter()
                        .find(|path| path.exists())
                    });
                if let Some(executable) = executable {
                    let started_at = process_start_time(pid);
                    return Ok(Some(CodexLaunchTarget {
                        pid,
                        started_at,
                        executable,
                        app_id: Some("com.openai.codex".to_string()),
                    }));
                }
            }
        }
    }
    Ok(None)
}

#[cfg(target_os = "windows")]
fn discover_codex_app_id() -> Option<String> {
    let script = "Get-StartApps | Select-Object -ExpandProperty AppID";
    let mut command = crate::process::background_command("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|value| is_codex_app_id(value))
        .map(str::to_owned)
}

#[cfg(any(target_os = "windows", test))]
fn is_codex_app_id(value: &str) -> bool {
    value
        .strip_prefix("OpenAI.Codex_")
        .and_then(|value| value.strip_suffix("!App"))
        .is_some_and(|publisher| {
            publisher.len() == 13
                && publisher
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// PID zero and an empty executable represent an installed, stopped MSIX app.
#[cfg(any(target_os = "windows", test))]
fn aumid_only_target(app_id: String) -> CodexLaunchTarget {
    CodexLaunchTarget {
        pid: 0,
        #[cfg(target_os = "macos")]
        started_at: None,
        executable: PathBuf::new(),
        app_id: Some(app_id),
    }
}

// The durable user environment lease lets later user launches keep Enhanced,
// but the managed restart also binds the verified bridge directly. GUI launch
// environments can lag or drop a broadcast on both supported desktop systems;
// a restart must not claim success based on that ambient state alone.
#[cfg(target_os = "windows")]
fn launch_codex(
    target: &CodexLaunchTarget,
    launch: Option<&DesktopRuntimeLaunch>,
) -> std::io::Result<()> {
    let (program, args, bridge) = windows_launch_spec(target, launch);
    let mut command = std::process::Command::new(program);
    command.args(args);
    if let Some(bridge) = bridge {
        command.env(crate::enhanced_runtime::env_lease::CODEX_CLI_PATH, bridge);
    }
    if let Some(profile) =
        crate::enhanced_runtime::steer_repair::user_data_override(&target.executable)
    {
        command.env("CODEX_ELECTRON_USER_DATA_PATH", profile);
    }
    command.spawn()?;
    Ok(())
}

#[cfg(any(target_os = "windows", test))]
fn windows_launch_spec(
    target: &CodexLaunchTarget,
    launch: Option<&DesktopRuntimeLaunch>,
) -> (PathBuf, Vec<String>, Option<PathBuf>) {
    if use_direct_desktop_launch(&target.executable, launch.is_some()) {
        return (
            target.executable.clone(),
            Vec::new(),
            launch.map(|launch| launch.bridge_executable.clone()),
        );
    }
    let (program, args) =
        crate::runtime::codex_launch_spec(target.app_id.as_deref(), &target.executable);
    (program, args, None)
}

#[cfg(any(target_os = "windows", test))]
pub(crate) fn use_direct_desktop_launch(executable: &std::path::Path, has_bridge: bool) -> bool {
    // A stopped MSIX app uses AUMID activation and the HKCU lease prepared by
    // sync_desktop_launch. await_bridge_ready still verifies actual adoption.
    !executable.as_os_str().is_empty()
        && (has_bridge || crate::enhanced_runtime::steer_repair::is_verified_copy(executable))
}

#[cfg(target_os = "macos")]
fn launch_codex(
    target: &CodexLaunchTarget,
    launch: Option<&DesktopRuntimeLaunch>,
) -> std::io::Result<()> {
    if crate::enhanced_runtime::macos_send_repair::is_verified_copy(&target.executable) {
        let bundle = crate::enhanced_runtime::macos_send_repair::bundle_path(&target.executable)
            .expect("verified macOS repair bundle");
        std::process::Command::new("open")
            .args(macos_repaired_open_args(bundle, launch))
            .spawn()?;
    } else if let Some(app_id) = target.app_id.as_deref() {
        std::process::Command::new("open")
            .args(macos_open_args(app_id, launch))
            .spawn()?;
    } else {
        let mut command = std::process::Command::new(&target.executable);
        if let Some(launch) = launch {
            command.env(
                crate::enhanced_runtime::env_lease::CODEX_CLI_PATH,
                &launch.bridge_executable,
            );
        }
        command.spawn()?;
    }
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn launch_codex(
    target: &CodexLaunchTarget,
    _launch: Option<&DesktopRuntimeLaunch>,
) -> std::io::Result<()> {
    std::process::Command::new(&target.executable).spawn()?;
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn macos_open_args(app_id: &str, launch: Option<&DesktopRuntimeLaunch>) -> Vec<std::ffi::OsString> {
    let mut args = vec!["-b".into(), app_id.into()];
    if let Some(launch) = launch {
        args.push("--env".into());
        args.push(
            format!(
                "{}={}",
                crate::enhanced_runtime::env_lease::CODEX_CLI_PATH,
                launch.bridge_executable.display()
            )
            .into(),
        );
    }
    args
}

#[cfg(any(target_os = "macos", test))]
fn macos_repaired_open_args(
    bundle: &std::path::Path,
    launch: Option<&DesktopRuntimeLaunch>,
) -> Vec<std::ffi::OsString> {
    let mut args = vec!["-a".into(), bundle.as_os_str().to_owned()];
    if let Some(launch) = launch {
        args.push("--env".into());
        args.push(
            format!(
                "{}={}",
                crate::enhanced_runtime::env_lease::CODEX_CLI_PATH,
                launch.bridge_executable.display()
            )
            .into(),
        );
    }
    args
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StopObservation {
    pub exited: bool,
    pub exit_code: Option<i32>,
    pub stderr: String,
    pub force_used: bool,
    pub can_force: bool,
    pub stop_method: &'static str,
}

impl StopObservation {
    fn not_running() -> Self {
        Self {
            exited: true,
            exit_code: None,
            stderr: String::new(),
            force_used: false,
            can_force: false,
            stop_method: "notRunning",
        }
    }

    fn annotate(&self, notice: RuntimeNotice) -> RuntimeNotice {
        let mut notice = notice.with("stopMethod", self.stop_method);
        if let Some(code) = self.exit_code {
            notice = notice.with("exitCode", code.to_string());
        }
        if self.force_used {
            notice.with("forceUsed", "true")
        } else {
            notice
        }
    }

    pub fn notice(&self) -> RuntimeNotice {
        let notice = if self.exit_code.is_some() {
            RuntimeNotice::new("restartProcessStillRunning")
        } else {
            RuntimeNotice::new("restartProcessCloseTimedOut")
                .with("seconds", if self.force_used { "12" } else { "10" })
        }
        .with("stderr", truncate_stop_stderr(&self.stderr))
        .with("canForce", self.can_force.to_string());
        self.annotate(notice)
    }
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, PartialEq, Eq)]
enum OpenProcessFailure {
    Abort,
    Gone,
    Skip,
}

/// Only an unobservable original root prevents shutdown from starting.
/// Later failures must not abandon a tree whose windows may already be closing.
#[cfg(any(target_os = "windows", test))]
fn open_process_failure(is_root: bool, attempt: usize, code: Option<i32>) -> OpenProcessFailure {
    if is_root && attempt == 0 {
        OpenProcessFailure::Abort
    } else if code == Some(87) {
        // ERROR_INVALID_PARAMETER: the snapshot raced with process exit.
        OpenProcessFailure::Gone
    } else {
        OpenProcessFailure::Skip
    }
}

fn truncate_stop_stderr(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.chars().count() <= 400 {
        trimmed.to_string()
    } else {
        trimmed.chars().take(400).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopAction {
    Done,
    ForceKill,
    Refuse { can_force: bool },
}

/// Escalation requires explicit consent or a live bridge proving zero open turns.
/// A graceful-close timeout alone provides no evidence that Desktop is idle.
pub(crate) fn next_stop_action(
    force: bool,
    open_turns: Option<u32>,
    still_running: bool,
) -> StopAction {
    if !still_running {
        return StopAction::Done;
    }
    if force || open_turns == Some(0) {
        return StopAction::ForceKill;
    }
    StopAction::Refuse { can_force: true }
}

#[cfg(any(not(target_os = "windows"), test))]
pub(crate) fn stop_observation_from_output(
    output: &std::process::Output,
    still_running: bool,
    force_used: bool,
) -> StopObservation {
    StopObservation {
        exited: !still_running,
        exit_code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        force_used,
        // Keep the existing macOS refusal UI; only Windows adds timeout consent.
        can_force: cfg!(target_os = "windows") && !force_used && still_running,
        stop_method: if force_used { "forced" } else { "graceful" },
    }
}

#[cfg(any(target_os = "windows", test))]
fn descendant_pids(root: u32, processes: &[(u32, u32, PathBuf)]) -> Vec<u32> {
    let mut tree = vec![root];
    loop {
        let previous = tree.len();
        for (pid, parent, _) in processes {
            if tree.contains(parent) && !tree.contains(pid) {
                tree.push(*pid);
            }
        }
        if tree.len() == previous {
            return tree;
        }
    }
}

#[cfg(target_os = "windows")]
async fn stop_windows_codex_and_wait(
    target: &CodexLaunchTarget,
    force: bool,
    data_root: &std::path::Path,
    initial_attestation: Option<&BridgeAttestationV1>,
) -> std::io::Result<StopObservation> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_TERMINATE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE,
    };

    unsafe extern "system" fn close_window(window: HWND, context: LPARAM) -> i32 {
        let pids = &*(context as *const Vec<u32>);
        let mut pid = 0;
        GetWindowThreadProcessId(window, &mut pid);
        if pids.contains(&pid) && IsWindowVisible(window) != 0 {
            PostMessageW(window, WM_CLOSE, 0, 0);
        }
        1
    }

    fn exit_code(handle: &OwnedHandle) -> std::io::Result<Option<i32>> {
        let mut code = 0;
        if unsafe { GetExitCodeProcess(handle.as_raw_handle(), &mut code) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((code != 259).then_some(code as i32))
    }

    // Hold handles throughout shutdown: even if Windows reuses a PID, force
    // escalation can only terminate the processes we actually observed.
    let mut handles: Vec<(u32, OwnedHandle)> = Vec::new();
    let mut reported_failures = std::collections::HashSet::new();
    // WM_CLOSE can stop the bridge before Desktop's remaining helpers exit.
    // Preserve the live evidence collected before sending the first close.
    let mut observation = StopObservation::not_running();
    observation.stop_method = "graceful";
    for attempt in 0..=120 {
        let snapshot = windows_process_snapshot(false).ok_or_else(std::io::Error::last_os_error)?;
        let roots = if attempt == 0 {
            vec![target.pid]
        } else {
            let mut roots = Vec::new();
            for (pid, handle) in &handles {
                if exit_code(handle)?.is_none() {
                    roots.push(*pid);
                }
            }
            roots
        };
        for root in roots {
            for pid in descendant_pids(root, &snapshot) {
                if handles.iter().any(|(known, _)| *known == pid) {
                    continue;
                }
                let raw = unsafe {
                    OpenProcess(
                        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                        0,
                        pid,
                    )
                };
                if raw.is_null() {
                    let error = std::io::Error::last_os_error();
                    match open_process_failure(pid == target.pid, attempt, error.raw_os_error()) {
                        OpenProcessFailure::Abort => return Err(error),
                        OpenProcessFailure::Gone => {}
                        OpenProcessFailure::Skip => {
                            if reported_failures.insert(pid) {
                                observation.stderr = truncate_stop_stderr(&format!(
                                    "{} OpenProcess PID {pid} skipped: {error};",
                                    observation.stderr
                                ));
                            }
                        }
                    }
                    continue;
                }
                handles.push((pid, unsafe { OwnedHandle::from_raw_handle(raw) }));
            }
        }
        let mut alive = Vec::new();
        for (pid, handle) in &handles {
            match exit_code(handle)? {
                None => alive.push(*pid),
                Some(code) if *pid == target.pid => observation.exit_code = Some(code),
                Some(_) => {}
            }
        }
        if alive.is_empty() {
            observation.exited = true;
            return Ok(observation);
        }
        observation.exited = false;
        if attempt < 100 {
            // Include windows belonging to children created during shutdown.
            unsafe {
                EnumWindows(Some(close_window), &alive as *const Vec<u32> as LPARAM);
            }
        } else if attempt == 100 {
            let attestation = crate::enhanced_runtime::live_bridge_attestation(data_root);
            match next_stop_action(
                force,
                attestation
                    .as_ref()
                    .or(initial_attestation)
                    .map(|a| a.open_turns),
                true,
            ) {
                StopAction::Refuse { can_force } => {
                    observation.can_force = can_force;
                    return Ok(observation);
                }
                StopAction::ForceKill => {
                    observation.force_used = true;
                    observation.stop_method = "forced";
                }
                StopAction::Done => unreachable!(),
            }
        }
        if observation.force_used {
            // Children first; retained handles also cover orphaned children
            // after the original root has already exited gracefully.
            for (pid, handle) in handles.iter().rev() {
                if alive.contains(pid)
                    && unsafe { TerminateProcess(handle.as_raw_handle(), 1) } == 0
                {
                    observation.stderr = truncate_stop_stderr(&format!(
                        "{} TerminateProcess PID {pid}: {};",
                        observation.stderr,
                        std::io::Error::last_os_error()
                    ));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(observation)
}

#[cfg(not(target_os = "windows"))]
fn stop_codex(target: &CodexLaunchTarget) -> std::io::Result<std::process::Output> {
    let mut command = crate::process::background_command("kill");
    command.args(["-TERM", &target.pid.to_string()]);
    command.output()
}

#[cfg(not(target_os = "windows"))]
async fn stop_codex_and_wait(
    target: &CodexLaunchTarget,
    force: bool,
) -> std::io::Result<StopObservation> {
    let output = stop_codex(target)?;
    let _ = output.status.success();

    #[cfg(target_os = "macos")]
    {
        for _ in 0..50 {
            if !process_is_alive(target.pid) {
                return Ok(stop_observation_from_output(&output, false, false));
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        match next_stop_action(force, None, true) {
            StopAction::Done => Ok(stop_observation_from_output(&output, false, false)),
            StopAction::Refuse { .. } => Ok(stop_observation_from_output(&output, true, false)),
            StopAction::ForceKill => {
                let mut command = crate::process::background_command("kill");
                command.args(["-KILL", &target.pid.to_string()]);
                let forced = command.output()?;
                for _ in 0..20 {
                    if !process_is_alive(target.pid) {
                        return Ok(stop_observation_from_output(&forced, false, true));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Ok(stop_observation_from_output(
                    &forced,
                    process_is_alive(target.pid),
                    true,
                ))
            }
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        if wait_for_codex_pid_exit(target.pid, 50).await {
            return Ok(stop_observation_from_output(&output, false, false));
        }
        Ok(stop_observation_from_output(&output, true, false))
    }
}

/// LaunchServices may recreate an Electron app between the PID we stopped and
/// our explicit `open`. `open --env` only applies to a process it launches, so
/// accepting that replacement would repeat the exact stale-bridge failure the
/// managed restart is meant to repair. Require a short quiet window, bounded
/// by a hard deadline, before issuing the authoritative launch.
#[cfg(target_os = "macos")]
async fn quiesce_macos_codex_relaunches(original_pid: u32) -> std::io::Result<()> {
    const QUIET_WINDOW: Duration = Duration::from_millis(500);
    const HARD_TIMEOUT: Duration = Duration::from_secs(10);

    let deadline = std::time::Instant::now() + HARD_TIMEOUT;
    let mut quiet_since = std::time::Instant::now();
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Codex kept relaunching during the managed restart",
            ));
        }
        let replacement = discover_codex_process()
            .map_err(|error| std::io::Error::other(error.to_string()))?
            .filter(|target| target.pid != original_pid);
        if let Some(replacement) = replacement {
            let stopped = stop_codex_and_wait(&replacement, true).await?;
            if !stopped.exited {
                return Err(std::io::Error::other(
                    "replacement Codex process did not exit",
                ));
            }
            quiet_since = std::time::Instant::now();
            continue;
        }
        if quiet_since.elapsed() >= QUIET_WINDOW {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
async fn wait_for_codex_pid_exit(pid: u32, attempts: usize) -> bool {
    for _ in 0..attempts {
        if !crate::enhanced_runtime::process_info::pid_is_alive(pid) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    false
}

#[cfg(target_os = "macos")]
fn process_is_alive(pid: u32) -> bool {
    let mut command = crate::process::background_command("kill");
    command.args(["-0", &pid.to_string()]);
    command.status().is_ok_and(|status| status.success())
}

#[cfg(target_os = "macos")]
fn process_start_time(pid: u32) -> Option<String> {
    let mut command = crate::process::background_command("ps");
    command.args(["-p", &pid.to_string(), "-o", "lstart="]);
    command
        .output()
        .ok()
        .and_then(|output| {
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        })
        .filter(|value| !value.is_empty())
}

fn launch_target_identity(_target: &CodexLaunchTarget) -> String {
    #[cfg(target_os = "windows")]
    {
        let started = crate::enhanced_runtime::process_info::started_at(_target.pid)
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|time| time.as_nanos().to_string())
            .unwrap_or_else(|| "unknown".into());
        format!("pid:{};started:{started}", _target.pid)
    }
    #[cfg(target_os = "macos")]
    {
        format!(
            "pid:{};started:{}",
            _target.pid,
            _target.started_at.as_deref().unwrap_or("unknown")
        )
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        format!("pid:{}", _target.pid)
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn codex_process_identity() -> Option<String> {
    discover_codex_process()
        .ok()
        .flatten()
        .map(|target| launch_target_identity(&target))
}

#[cfg(target_os = "macos")]
pub(crate) fn codex_process_identity() -> Option<String> {
    discover_codex_process()
        .ok()
        .flatten()
        .map(|target| launch_target_identity(&target))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub(crate) fn codex_process_identity() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "windows")]
    use super::{is_codex_desktop_executable, select_codex_desktop_root};
    #[cfg(target_os = "windows")]
    use std::path::Path;

    #[cfg(target_os = "windows")]
    #[test]
    fn packaged_codex_executable_is_recognized_without_accepting_cli_workers() {
        assert!(is_codex_desktop_executable(Path::new(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.818.2441.0_x64__2p2nqs\Codex.exe"
        )));
        assert!(is_codex_desktop_executable(Path::new(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.700.1.0_x64__2p2nqs\ChatGPT.exe"
        )));
        assert!(!is_codex_desktop_executable(Path::new(
            r"C:\Tools\Unrelated\ChatGPT.exe"
        )));
        assert!(!is_codex_desktop_executable(Path::new(
            r"C:\Users\me\AppData\Local\OpenAI\Codex\bin\hash\codex.exe"
        )));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn desktop_identity_uses_the_mixed_process_tree_root() {
        let root = Path::new(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.818.2441.0_x64__2p2nqs\app\ChatGPT.exe",
        )
        .to_path_buf();
        let utility = Path::new(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.818.2441.0_x64__2p2nqs\app\ChatGPT.exe",
        )
        .to_path_buf();
        let worker = Path::new(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.818.2441.0_x64__2p2nqs\app\resources\codex.exe",
        )
        .to_path_buf();
        assert_eq!(
            select_codex_desktop_root(vec![
                (100, 50, root.clone()),
                (101, 100, utility),
                (102, 101, worker),
            ]),
            Some((100, root))
        );
    }
}

#[cfg(test)]
mod restart_guard_tests {
    use super::*;
    use crate::enhanced_runtime::{BridgeLifecycle, ChildAttestation};

    #[test]
    fn open_process_failures_only_abort_for_the_initial_root() {
        use OpenProcessFailure::{Abort, Gone, Skip};
        for code in [Some(5), Some(87), Some(6), None] {
            assert_eq!(open_process_failure(true, 0, code), Abort);
            for (is_root, attempt) in [(false, 0), (false, 1), (true, 1), (false, 100)] {
                assert_eq!(
                    open_process_failure(is_root, attempt, code),
                    if code == Some(87) { Gone } else { Skip }
                );
            }
        }
    }

    #[test]
    fn close_timeout_without_exit_code_reports_seconds_and_consent() {
        let mut observation = StopObservation::not_running();
        observation.exited = false;
        observation.stop_method = "graceful";
        observation.can_force = true;
        observation.stderr = "OpenProcess PID 123 skipped: Access denied;".into();
        let notice = observation.notice();
        assert_eq!(notice.code, "restartProcessCloseTimedOut");
        assert_eq!(notice.params["seconds"], "10");
        assert_eq!(notice.params["canForce"], "true");
        assert_eq!(notice.params["stopMethod"], "graceful");
        assert!(notice.params["stderr"].contains("PID 123"));
        assert!(!notice.params.contains_key("exitCode"));

        observation.force_used = true;
        observation.stop_method = "forced";
        observation.can_force = false;
        let notice = observation.notice();
        assert_eq!(notice.code, "restartProcessCloseTimedOut");
        assert_eq!(notice.params["seconds"], "12");
        assert_eq!(notice.params["forceUsed"], "true");
        assert_eq!(notice.params["canForce"], "false");
        assert!(!notice.params.contains_key("exitCode"));

        observation.exit_code = Some(1);
        let notice = observation.notice();
        assert_eq!(notice.code, "restartProcessStillRunning");
        assert_eq!(notice.params["exitCode"], "1");
        assert!(!notice.params.contains_key("seconds"));
    }

    fn attestation(open_turns: u32) -> BridgeAttestationV1 {
        BridgeAttestationV1 {
            schema_version: 1,
            launch_id: "launch".into(),
            bridge_pid: std::process::id(),
            parent_pid: 0,
            parent_executable: None,
            started_at: 0,
            updated_at: 0,
            state: BridgeLifecycle::Ready,
            official: ChildAttestation::default(),
            enhanced: ChildAttestation::default(),
            model_provider_map_sha256: String::new(),
            binding_db: std::path::PathBuf::new(),
            binding_db_identity: String::new(),
            enhanced_identity: None,
            session_features_applied: 0,
            open_turns,
            failure_reason: None,
        }
    }

    /// The 2026-09-02 loss: a thread created 83 seconds earlier, on the
    /// Official plane, so Vellum's own counter read zero and the restart went
    /// ahead before Codex had written the thread anywhere.
    #[test]
    fn a_turn_in_flight_stops_the_restart_and_says_how_many() {
        let notice = codex_turn_refusal(Some(&attestation(1)), false).expect("refused");
        assert_eq!(notice.code, "restartBlockedByCodexTurn");
        assert_eq!(notice.params.get("count").map(String::as_str), Some("1"));
    }

    #[test]
    fn an_idle_bridge_does_not_stop_anything() {
        assert!(codex_turn_refusal(Some(&attestation(0)), false).is_none());
    }

    /// Enhanced switched off, or no bridge running: there is nothing to ask,
    /// and "nothing to ask" must never mean "refuse".
    #[test]
    fn no_bridge_is_not_a_reason_to_refuse() {
        assert!(codex_turn_refusal(None, false).is_none());
    }

    /// Being told is the point; being unable to proceed afterwards is not.
    #[test]
    fn the_user_can_overrule_the_guard() {
        assert!(codex_turn_refusal(Some(&attestation(3)), true).is_none());
    }

    #[test]
    fn macos_managed_launch_binds_the_verified_bridge_explicitly() {
        let launch = DesktopRuntimeLaunch {
            bridge_executable: PathBuf::from("/Applications/Vellum.app/Contents/Resources/bridge"),
            launch_id: "launch-1".into(),
            manifest_path: PathBuf::from("/tmp/launch.json"),
            attestation_path: PathBuf::from("/tmp/attestation.json"),
        };
        let args = macos_open_args("com.openai.codex", Some(&launch));
        assert_eq!(
            args,
            vec![
                std::ffi::OsString::from("-b"),
                std::ffi::OsString::from("com.openai.codex"),
                std::ffi::OsString::from("--env"),
                std::ffi::OsString::from(
                    "CODEX_CLI_PATH=/Applications/Vellum.app/Contents/Resources/bridge"
                ),
            ]
        );
        let bundle = std::path::Path::new("/tmp/repair with spaces/ChatGPT.app");
        let repaired = macos_repaired_open_args(bundle, Some(&launch));
        assert_eq!(
            &repaired[..2],
            &[
                std::ffi::OsString::from("-a"),
                bundle.as_os_str().to_owned()
            ]
        );
        assert_eq!(&repaired[2..], &args[2..]);
        assert_eq!(macos_repaired_open_args(bundle, None), repaired[..2]);
    }

    #[test]
    fn windows_managed_launch_binds_the_verified_bridge_explicitly() {
        let target = CodexLaunchTarget {
            pid: 42,
            #[cfg(target_os = "macos")]
            started_at: Some("1".into()),
            executable: PathBuf::from(r"C:\Program Files\WindowsApps\ChatGPT.exe"),
            app_id: Some("OpenAI.Codex_123!Codex".into()),
        };
        let launch = DesktopRuntimeLaunch {
            bridge_executable: PathBuf::from(r"E:\Vellum\binaries\bridge.exe"),
            launch_id: "launch-1".into(),
            manifest_path: PathBuf::from(r"E:\Vellum\launch.json"),
            attestation_path: PathBuf::from(r"E:\Vellum\attestation.json"),
        };
        let (program, args, bridge) = windows_launch_spec(&target, Some(&launch));
        assert_eq!(program, target.executable);
        assert!(args.is_empty());
        assert_eq!(bridge.as_deref(), Some(launch.bridge_executable.as_path()));

        let (program, args, bridge) = windows_launch_spec(&target, None);
        assert_eq!(program, PathBuf::from("explorer.exe"));
        assert_eq!(args, vec![r"shell:AppsFolder\OpenAI.Codex_123!Codex"]);
        assert_eq!(bridge, None);
    }

    #[test]
    fn stop_decision_truth_table() {
        use StopAction::{Done, ForceKill, Refuse};
        for (force, open_turns, still_running, expected) in [
            (false, None, false, Done),
            (false, None, true, Refuse { can_force: true }),
            (false, Some(0), false, Done),
            (false, Some(0), true, ForceKill),
            (false, Some(1), false, Done),
            (false, Some(1), true, Refuse { can_force: true }),
            (false, Some(3), false, Done),
            (false, Some(3), true, Refuse { can_force: true }),
            (true, None, false, Done),
            (true, None, true, ForceKill),
            (true, Some(0), false, Done),
            (true, Some(0), true, ForceKill),
            (true, Some(1), false, Done),
            (true, Some(1), true, ForceKill),
            (true, Some(3), false, Done),
            (true, Some(3), true, ForceKill),
        ] {
            assert_eq!(
                next_stop_action(force, open_turns, still_running),
                expected,
                "force={force}, turns={open_turns:?}, running={still_running}"
            );
        }
    }

    #[test]
    fn aumid_matching_excludes_other_openai_apps() {
        assert!(is_codex_app_id("OpenAI.Codex_2p2nqsd0c76g0!App"));
        for value in [
            "",
            "OpenAI.CodexThirdPartyAPI_2p2nqsd0c76g0!App",
            "OpenAI.ChatGPT_2p2nqsd0c76g0!App",
            "OpenAI.Codex_!App",
            "OpenAI.Codex_2p2nqsd0c76g0!Other",
            "OpenAI.Codex_2p2nqsd0c76g0!App extra",
        ] {
            assert!(!is_codex_app_id(value), "{value}");
        }
    }

    #[test]
    fn stopped_msix_launch_uses_aumid_even_with_enhanced_lease() {
        let target = aumid_only_target("OpenAI.Codex_2p2nqsd0c76g0!App".into());
        let launch = DesktopRuntimeLaunch {
            bridge_executable: PathBuf::from("bridge.exe"),
            launch_id: "launch".into(),
            manifest_path: PathBuf::from("launch.json"),
            attestation_path: PathBuf::from("attestation.json"),
        };
        for launch in [None, Some(&launch)] {
            let (program, args, bridge) = windows_launch_spec(&target, launch);
            assert_eq!(program, PathBuf::from("explorer.exe"));
            assert_eq!(args, [r"shell:AppsFolder\OpenAI.Codex_2p2nqsd0c76g0!App"]);
            assert!(bridge.is_none());
        }
    }

    #[test]
    fn process_tree_includes_orphans_helpers_and_unordered_descendants() {
        let rows = [(4, 3), (99, 98), (3, 2), (2, 1), (1, 0)]
            .map(|(pid, parent)| (pid, parent, PathBuf::new()));
        let mut pids = descendant_pids(1, &rows);
        pids.sort();
        assert_eq!(pids, [1, 2, 3, 4]);
    }

    #[test]
    fn stop_failure_records_the_real_exit_code_and_stderr() {
        let output = std::process::Output {
            status: dummy_exit(128),
            stdout: Vec::new(),
            stderr: b"taskkill: Access denied.\n".to_vec(),
        };
        let observation = stop_observation_from_output(&output, true, false);
        assert!(!observation.exited);
        assert_eq!(observation.exit_code, Some(128));
        let notice = observation.notice();
        assert_eq!(notice.code, "restartProcessStillRunning");
        assert_eq!(
            notice.params.get("exitCode").map(String::as_str),
            Some("128")
        );
        assert!(notice
            .params
            .get("stderr")
            .is_some_and(|value| value.contains("Access denied")));
        assert_eq!(
            notice.params.get("stopMethod").map(String::as_str),
            Some("graceful")
        );
        assert_eq!(
            notice.params.get("canForce").map(String::as_str),
            Some(if cfg!(target_os = "windows") {
                "true"
            } else {
                "false"
            })
        );
        assert!(!notice.params.contains_key("forceUsed"));

        let forced = stop_observation_from_output(&output, true, true).notice();
        assert_eq!(
            forced.params.get("stopMethod").map(String::as_str),
            Some("forced")
        );
        assert_eq!(
            forced.params.get("forceUsed").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            forced.params.get("canForce").map(String::as_str),
            Some("false")
        );
        let skipped =
            StopObservation::not_running().annotate(RuntimeNotice::new("restartSucceeded"));
        assert_eq!(
            skipped.params.get("stopMethod").map(String::as_str),
            Some("notRunning")
        );
    }

    fn dummy_exit(code: i32) -> std::process::ExitStatus {
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(code as u32)
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(code << 8)
        }
    }
}
