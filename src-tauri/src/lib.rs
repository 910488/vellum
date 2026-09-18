//! Vellum 本機指令層。
//!
//! 模組分工（doc/00 原則）：指令層只搬資料、轉錯誤；商業邏輯放在旁邊的純模組。

pub mod adapter;
pub mod boot;
pub mod budget;
pub mod catalog;
pub mod codex;
pub mod codex_oauth;
pub mod codex_profile;
pub mod codex_quota;
pub mod commands;
pub mod compaction;
pub mod continuation;
pub mod credentials;
pub mod crypto;
pub mod diagnostics_store;
pub mod enhanced_runtime;
pub mod error;
pub mod eval;
pub mod grok_accounts;
pub mod grok_auth;
pub mod harness;
pub mod history;
pub mod install_paths;
pub mod loop_guard;
pub mod model;
pub mod panic_hook;
pub mod policy;
pub mod probe;
pub mod process;
pub mod proxy;
pub mod proxy_prepare;
pub mod proxy_runtime_bridge;
pub mod quota;
pub mod remote;
pub mod review;
pub mod runtime;
pub mod sse;
pub mod state;
pub mod support_bundle;
pub mod thread_runtime_binding;
pub mod trace;
pub mod updates;
pub mod usage;
pub mod web_search;
pub mod web_search_pdf;

use state::AppState;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
use tauri::Manager;

fn show_main_window(app: &tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    if let Err(error) = app.set_activation_policy(tauri::ActivationPolicy::Regular) {
        log::warn!("[Window] cannot restore macOS Dock activation policy: {error}");
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn toggle_proxy_from_tray(app: &tauri::AppHandle) {
    let state = (*app.state::<AppState>()).clone();
    tauri::async_runtime::spawn(async move {
        let result = if state.proxy_status().running {
            crate::commands::proxy::stop_proxy_gracefully(&state).await
        } else {
            crate::commands::proxy::start_proxy_inner(&state)
                .await
                .map(|_| ())
        };
        if let Err(error) = result {
            log::error!("[Tray] cannot toggle proxy: {error}");
        }
    });
}

fn shutdown_proxy_and_restore(app: &tauri::AppHandle, apply_desktop_update: bool) {
    let state = app.state::<AppState>().clone();
    // Exit, tray Stop, and the explicit Stop command share one teardown. This
    // is also where the Enhanced CODEX_CLI_PATH lease is released; keeping a
    // duplicate exit-only restore path previously left the bridge armed.
    tauri::async_runtime::block_on(async move {
        if let Err(error) = crate::commands::proxy::stop_proxy_gracefully(&state).await {
            log::error!("[Codex] 結束前還原 Proxy/Enhanced 啟動接管失敗：{error}");
        }
        // Hiding the main window leaves this process alive, so an installer
        // may only start for the tray's explicit process exit.
        if apply_desktop_update {
            if let Err(error) = crate::updates::apply_staged_on_exit(&state) {
                log::error!("[Updates] exit-time desktop apply failed: {error}");
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    crate::panic_hook::install();
    let builder = tauri::Builder::default();
    // Two Vellum processes managing the same Codex home is exactly the
    // scenario the lease-owner reconcile in `codex.rs` has to detect after
    // the fact; refusing the second launch outright is cheaper and cannot
    // race. A second launch focuses the existing window instead of starting
    // a competing instance.
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.unminimize();
            let _ = window.set_focus();
        }
    }));
    builder
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .build(),
        )
        .setup(|app| {
            let handle = app.handle().clone();
            app.state::<AppState>()
                .attach_lifecycle_emitter(move |status| {
                    use tauri::Emitter;
                    let _ = handle.emit(crate::model::PROXY_LIFECYCLE_EVENT, &status);
                });
            // Always start with the full workspace available. The config flag
            // handles normal launches; this runtime call also covers window
            // state restored by the platform between app versions.
            if let Some(window) = app.get_webview_window("main") {
                if let Err(error) = window.maximize() {
                    log::warn!("[Window] cannot maximize the main window at launch: {error}");
                }
            }
            let data_root = app.state::<AppState>().data_root();
            crate::updates::spawn_auto_check(app.handle().clone(), data_root.clone());
            let boot = crate::boot::record_boot(&data_root);
            crate::trace::install(&data_root);
            log::info!(
                "[Vellum] process started: boot={} pid={} started_at={} previous_started_at={:?}",
                boot.boot_count,
                boot.pid,
                boot.started_at,
                boot.previous_started_at
            );
            if let Some(icon) = app.default_window_icon().cloned() {
                let show =
                    tauri::menu::MenuItem::with_id(app, "show", "顯示 Vellum", true, None::<&str>)?;
                let toggle_proxy = tauri::menu::MenuItem::with_id(
                    app,
                    "toggle_proxy",
                    "啟動／停止 Proxy",
                    true,
                    None::<&str>,
                )?;
                let exit =
                    tauri::menu::MenuItem::with_id(app, "exit", "結束 Vellum", true, None::<&str>)?;
                let menu = tauri::menu::Menu::with_items(app, &[&show, &toggle_proxy, &exit])?;
                tauri::tray::TrayIconBuilder::new()
                    .tooltip("Vellum")
                    .icon(icon)
                    .menu(&menu)
                    .on_menu_event(|app, event| match event.id().as_ref() {
                        "show" => show_main_window(app),
                        "toggle_proxy" => toggle_proxy_from_tray(app),
                        "exit" => {
                            shutdown_proxy_and_restore(app, true);
                            app.exit(0);
                        }
                        _ => {}
                    })
                    // Only a completed left click restores the window. The old
                    // catch-all handler also ran for right-click/menu/hover
                    // events, stealing focus and repeatedly closing/reopening
                    // the native context menu.
                    .on_tray_icon_event(|tray, event| {
                        if matches!(
                            event,
                            TrayIconEvent::Click {
                                button: MouseButton::Left,
                                button_state: MouseButtonState::Up,
                                ..
                            }
                        ) {
                            show_main_window(tray.app_handle());
                        }
                    })
                    // Windows keeps the normal context menu on right click;
                    // left click is reserved for restoring the window above.
                    .show_menu_on_left_click(false)
                    .build(app)?;
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // The close button is a data-plane stop, not a cosmetic hide.
            // Leaving the Proxy alive behind a hidden tray window made users
            // reasonably believe Vellum was closed while Codex config and an
            // old account connection were still managed. Keep the tray process
            // available, but restore Codex and release the Enhanced launch
            // lease before the window disappears.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                shutdown_proxy_and_restore(window.app_handle(), false);
                let _ = window.hide();
                #[cfg(target_os = "macos")]
                if let Err(error) = window
                    .app_handle()
                    .set_activation_policy(tauri::ActivationPolicy::Accessory)
                {
                    log::warn!("[Window] cannot hide macOS Dock icon: {error}");
                }
            }
        })
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::get_overview,
            commands::get_provider_overviews,
            commands::get_request_log,
            commands::get_boot_telemetry,
            commands::get_usage_activity,
            commands::get_sessions,
            commands::list_routes,
            commands::select_route,
            commands::set_route_enabled,
            commands::set_route_insecure_http_policy,
            commands::set_route_models,
            commands::set_model_vision,
            commands::set_route_name,
            commands::set_model_display_name,
            commands::delete_route,
            commands::create_route,
            commands::refresh_quota,
            commands::discover_endpoint_models,
            commands::refresh_opencode_model_catalogs,
            commands::probe_endpoint,
            commands::probe_endpoint_model,
            commands::reprobe_route_capabilities,
            commands::reprobe_route_model_capability,
            commands::get_context_budget,
            commands::set_budget_override,
            commands::list_model_routes,
            commands::list_review_model_routes,
            commands::get_review_settings,
            commands::set_review_settings,
            commands::get_review_stats,
            commands::get_subagent_settings,
            commands::set_subagent_settings,
            commands::get_compaction_preview,
            commands::get_compaction_detail,
            commands::get_compaction_snapshot,
            commands::get_compaction_handoffs,
            commands::get_compaction_transcript,
            commands::run_review,
            commands::get_proxy_status,
            commands::get_catalog_status,
            commands::start_proxy,
            commands::stop_proxy_and_restore,
            commands::repair_codex_config,
            commands::get_codex_oauth_status,
            commands::get_codex_quota_pool,
            commands::set_codex_quota_pool,
            commands::start_codex_oauth_login,
            commands::poll_codex_oauth_login,
            commands::set_default_codex_oauth_account,
            commands::remove_codex_oauth_account,
            commands::logout_codex_oauth,
            commands::refresh_codex_oauth,
            commands::get_codex_oauth_reset_credits,
            commands::consume_codex_oauth_reset,
            commands::get_codex_oauth_account_quota,
            commands::trigger_codex_oauth_five_hour_window,
            commands::get_grok_account_status,
            commands::start_grok_account_login,
            commands::poll_grok_account_login,
            commands::cancel_grok_account_login,
            commands::set_default_grok_account,
            commands::refresh_grok_account,
            commands::remove_grok_account,
            commands::get_grok_account_quota,
            commands::refresh_grok_model_catalog,
            commands::exit_vellum,
            commands::get_runtime_status,
            commands::begin_graceful_drain,
            commands::cancel_graceful_drain,
            commands::list_catalog_versions,
            commands::rollback_catalog_version,
            commands::restart_codex_safely,
            commands::get_update_status,
            commands::check_updates,
            commands::set_update_preferences,
            commands::download_update,
            commands::apply_update,
            commands::cancel_update_download,
            commands::rollback_update,
            commands::set_remote_update_policy,
            commands::get_enhanced_desktop_runtime_status,
            commands::get_enhanced_runtime_overview,
            commands::list_enhanced_runtime_sessions,
            commands::get_enhanced_remote_control_status,
            commands::recheck_enhanced_runtime_compatibility,
            commands::export_enhanced_runtime_diagnostics,
            commands::export_vellum_logs,
            commands::configure_enhanced_desktop_runtime,
            commands::disable_enhanced_desktop_runtime,
            commands::run_enhanced_installed_gate,
            commands::get_web_search_settings,
            commands::set_web_search_settings,
            commands::probe_web_search,
            commands::get_subagent_capability,
            remote::commands::get_remote_manager_feature_flags,
            remote::commands::get_remote_release_status,
            remote::commands::get_desktop_codex_compatibility,
            remote::commands::update_remote_codex_for_desktop,
            remote::commands::discover_remote_connections,
            remote::commands::inspect_remote_host,
            remote::commands::bootstrap_remote_host,
            remote::commands::plan_remote_deployment,
            remote::commands::reapply_remote_deployment,
            remote::commands::apply_remote_deployment,
            remote::commands::get_remote_operation,
            remote::commands::restart_remote_native_codex,
            remote::commands::stop_remote_app_owned_codex,
            remote::commands::restore_remote_host,
            remote::commands::start_remote_grok_login,
            remote::commands::poll_remote_grok_login,
            remote::commands::refresh_remote_grok_login,
            remote::commands::cancel_remote_grok_login,
            remote::commands::set_active_remote_host,
            remote::commands::list_remote_codex_account_pairings,
            remote::commands::start_remote_codex_account_login,
            remote::commands::poll_remote_codex_account_login,
            remote::commands::activate_remote_codex_account,
            remote::commands::start_remote_control_pairing,
            remote::commands::list_remote_official_execution_accounts,
            remote::commands::start_remote_official_execution_account_login,
            remote::commands::poll_remote_official_execution_account_login,
            remote::commands::select_remote_official_execution_account,
            remote::commands::remove_remote_official_execution_account,
            remote::commands::remote_agent_host_status,
            remote::commands::remote_host_aggregate_status,
            remote::commands::remote_manager_repair,
            remote::commands::remote_manager_support_bundle,
            remote::commands::install_remote_pinned_codex,
            remote::commands::update_remote_components,
            remote::commands::get_remote_desired_state,
            remote::commands::remote_session_summary,
            remote::ssh_trust::remote_ssh_trust_status,
            remote::ssh_trust::remote_ssh_fetch_fingerprint,
            remote::ssh_trust::remote_ssh_confirm_fingerprint,
        ])
        .run(tauri::generate_context!())
        .expect("error while running vellum");
}
