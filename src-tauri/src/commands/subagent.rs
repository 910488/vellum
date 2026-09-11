//! Vellum-managed defaults for Codex native sub-agents.
//!
//! Codex already resolves sub-agent defaults itself (explicit spawn value →
//! `[agents]` defaults → parent value); Vellum only manages the `[agents]`
//! default keys while its Proxy lease is active.

use crate::codex::CodexPaths;
use crate::error::AppResult;
use crate::model::{SubagentCapability, SubagentSettings};
use crate::state::AppState;
use tauri::State;

#[tauri::command]
pub fn get_subagent_settings(state: State<'_, AppState>) -> AppResult<SubagentSettings> {
    Ok(state.subagent_settings())
}

#[tauri::command]
pub fn get_subagent_capability() -> AppResult<SubagentCapability> {
    Ok(crate::codex::native_subagent_capability())
}

#[tauri::command]
pub fn set_subagent_settings(
    settings: SubagentSettings,
    state: State<'_, AppState>,
) -> AppResult<SubagentSettings> {
    crate::codex::ensure_native_subagent_supported(&settings)?;
    let previous = state.subagent_settings();
    state.set_subagent_settings(settings.clone())?;
    // While the Proxy is running the managed Codex config is live: write the
    // `[agents]` defaults immediately so the next spawned agent picks them up
    // without a restart. When it is not running the settings are persisted and
    // applied by the next Proxy start.
    if state.proxy_status().running {
        let paths = CodexPaths::discover(&state.data_root());
        if let Err(error) = crate::codex::apply_subagent_defaults(&paths, &settings) {
            // A failed hot update must not leave Vellum's persisted settings
            // ahead of the live Codex config: roll back both so the next Proxy
            // start cannot silently apply a change the UI reported as failed.
            let _ = crate::codex::apply_subagent_defaults(&paths, &previous);
            let _ = state.set_subagent_settings(previous);
            return Err(error);
        }
    }
    Ok(settings)
}
