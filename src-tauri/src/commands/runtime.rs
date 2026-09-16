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
pub fn get_runtime_status(state: State<'_, AppState>) -> AppResult<RuntimeStatus> {
    state.reconcile_codex_restart(codex_process_identity());
    Ok(state.runtime_status())
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
    Ok(status)
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
            state.reconcile_enhanced_adoption(status.ready && !status.restart_required);
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
    state.mark_restart_required(crate::model::RuntimeNotice::new("catalogRestored"));
    state.set_restart_process_identity(codex_process_identity());
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
            let result = restart_codex_managed(&state, force).await;
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

pub(crate) async fn restart_codex_managed(
    state: &AppState,
    force: bool,
) -> AppResult<ManagedRestart> {
    let _lifecycle = state.lifecycle_lock().lock().await;
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
    if let Err(notice) = crate::runtime::restart_precondition(active, executable) {
        state.set_draining(false);
        return Ok(ManagedRestart::refused(notice));
    }
    let target = process.expect("precondition verified process");
    #[cfg(target_os = "windows")]
    let target = {
        let mut target = target;
        target.app_id = discover_codex_app_id();
        target
    };
    let previous_identity = launch_target_identity(&target);

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

    let stopped = match stop_codex_and_wait(&target, force).await {
        Ok(stopped) => stopped,
        Err(error) => {
            state.set_draining(false);
            return Err(AppError::Message(format!("無法關閉 Codex 程序：{error}")));
        }
    };
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

    if let Err(error) = launch_codex(&target, launch.as_ref()) {
        state.set_draining(false);
        return Err(AppError::Message(format!("無法重新啟動 Codex：{error}")));
    }

    let Some(launch) = launch else {
        // No Enhanced runtime configured, so a new Codex process is the whole
        // claim being made and a new PID is the right evidence for it.
        for _ in 0..50 {
            if codex_process_identity().is_some_and(|identity| identity != previous_identity) {
                state.clear_restart_required();
                state.set_draining(false);
                return Ok(ManagedRestart {
                    restarted: true,
                    notice: RuntimeNotice::new("restartSucceeded"),
                    launch: None,
                    attestation: None,
                });
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        state.set_draining(false);
        return Ok(ManagedRestart::refused(RuntimeNotice::new(
            "restartNotDetected",
        )));
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
                notice: RuntimeNotice::new("enhancedDesktopBridgeReady")
                    .with("launchId", launch.launch_id.clone()),
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
                notice: RuntimeNotice::new("enhancedDesktopBridgeFailed").with("reason", reason),
                launch: Some(launch),
                attestation: Some(attestation),
            })
        }
        BridgeObservation::NotObserved(attestation) => Ok(ManagedRestart {
            restarted: false,
            notice: RuntimeNotice::new("enhancedDesktopBridgeNotObserved")
                .with("launchId", launch.launch_id.clone()),
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
    known_install && (normalized.ends_with("\\chatgpt.exe") || normalized.ends_with("\\codex.exe"))
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
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|value| *value == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                if name.eq_ignore_ascii_case("ChatGPT.exe")
                    || name.eq_ignore_ascii_case("Codex.exe")
                {
                    let process =
                        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID);
                    if !process.is_null() {
                        let mut buffer = vec![0_u16; 32_768];
                        let mut length = buffer.len() as u32;
                        if QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length)
                            != 0
                        {
                            processes.push((
                                entry.th32ProcessID,
                                entry.th32ParentProcessID,
                                PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize])),
                            ));
                        }
                        CloseHandle(process);
                    }
                }
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
    let script = "Get-StartApps | Where-Object { $_.Name -match 'Codex|ChatGPT' -or $_.AppID -match 'OpenAI.Codex' } | Select-Object -First 1 -ExpandProperty AppID";
    let mut command = crate::process::background_command("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    let output = command.output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
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
    command.spawn()?;
    Ok(())
}

#[cfg(any(target_os = "windows", test))]
fn windows_launch_spec(
    target: &CodexLaunchTarget,
    launch: Option<&DesktopRuntimeLaunch>,
) -> (PathBuf, Vec<String>, Option<PathBuf>) {
    if let Some(launch) = launch {
        return (
            target.executable.clone(),
            Vec::new(),
            Some(launch.bridge_executable.clone()),
        );
    }
    let (program, args) =
        crate::runtime::codex_launch_spec(target.app_id.as_deref(), &target.executable);
    (program, args, None)
}

#[cfg(target_os = "macos")]
fn launch_codex(
    target: &CodexLaunchTarget,
    launch: Option<&DesktopRuntimeLaunch>,
) -> std::io::Result<()> {
    if let Some(app_id) = target.app_id.as_deref() {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StopObservation {
    pub exited: bool,
    pub exit_code: Option<i32>,
    pub stderr: String,
    pub force_used: bool,
}

impl StopObservation {
    pub fn notice(&self) -> RuntimeNotice {
        let mut notice = RuntimeNotice::new("restartProcessStillRunning")
            .with(
                "exitCode",
                self.exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "none".into()),
            )
            .with("stderr", truncate_stop_stderr(&self.stderr));
        if self.force_used {
            notice = notice.with("forceUsed", "true");
        }
        notice
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
    Refuse,
}

/// Force-kill is only allowed when the user overrode the turn guard.
/// A packaged Desktop surviving a graceful taskkill is not idle evidence.
pub(crate) fn next_stop_action(force: bool, still_running: bool) -> StopAction {
    if !still_running {
        return StopAction::Done;
    }
    if force {
        return StopAction::ForceKill;
    }
    StopAction::Refuse
}

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
    }
}

#[cfg(target_os = "windows")]
fn stop_codex(target: &CodexLaunchTarget) -> std::io::Result<std::process::Output> {
    let mut command = crate::process::background_command("taskkill");
    command.args(["/PID", &target.pid.to_string(), "/T"]);
    command.output()
}

#[cfg(target_os = "windows")]
fn force_stop_codex(target: &CodexLaunchTarget) -> std::io::Result<std::process::Output> {
    let mut command = crate::process::background_command("taskkill");
    command.args(["/PID", &target.pid.to_string(), "/T", "/F"]);
    command.output()
}

#[cfg(not(target_os = "windows"))]
fn stop_codex(target: &CodexLaunchTarget) -> std::io::Result<std::process::Output> {
    let mut command = crate::process::background_command("kill");
    command.args(["-TERM", &target.pid.to_string()]);
    command.output()
}

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
        match next_stop_action(force, true) {
            StopAction::Done => Ok(stop_observation_from_output(&output, false, false)),
            StopAction::Refuse => Ok(stop_observation_from_output(&output, true, false)),
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

    #[cfg(target_os = "windows")]
    {
        if wait_for_codex_pid_exit(target.pid, 50).await {
            return Ok(stop_observation_from_output(&output, false, false));
        }
        match next_stop_action(force, true) {
            StopAction::Done => Ok(stop_observation_from_output(&output, false, false)),
            StopAction::Refuse => Ok(stop_observation_from_output(&output, true, false)),
            StopAction::ForceKill => {
                let forced = force_stop_codex(target)?;
                let still = !wait_for_codex_pid_exit(target.pid, 20).await;
                Ok(stop_observation_from_output(&forced, still, true))
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

#[cfg(not(target_os = "macos"))]
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
        format!("pid:{}", _target.pid)
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
        .map(|target| format!("pid:{}", target.pid))
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
    fn graceful_stop_does_not_force_kill_without_user_override() {
        assert_eq!(next_stop_action(false, true), StopAction::Refuse);
        assert_eq!(next_stop_action(true, true), StopAction::ForceKill);
        assert_eq!(next_stop_action(false, false), StopAction::Done);
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
