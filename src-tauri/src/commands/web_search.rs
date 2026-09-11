use crate::error::{AppError, AppResult};
use crate::web_search::{
    SearchCommands, SearchEngine, SearchRequest, WebSearchSettings, BRAVE_SEARCH_CREDENTIAL_ID,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchSettingsView {
    pub settings: WebSearchSettings,
    pub has_brave_api_key: bool,
    /// Set once, the first time settings are read after a startup migration
    /// auto-disabled search for lack of a Brave key (legacy
    /// duckduckgo/searxng settings). `None` on every ordinary read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_notice: Option<crate::model::RuntimeNotice>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetWebSearchSettingsRequest {
    pub settings: WebSearchSettings,
    #[serde(default)]
    pub brave_api_key: Option<String>,
    #[serde(default)]
    pub clear_brave_api_key: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchProbeResult {
    pub output: String,
    pub result_count: usize,
}

#[tauri::command]
pub fn get_web_search_settings(
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<WebSearchSettingsView> {
    let root = state.data_root();
    Ok(WebSearchSettingsView {
        settings: state.web_search_settings(),
        has_brave_api_key: crate::credentials::load(&root, BRAVE_SEARCH_CREDENTIAL_ID)?.is_some(),
        migration_notice: state.take_web_search_migration_notice(),
    })
}

#[tauri::command(rename_all = "camelCase")]
pub fn set_web_search_settings(
    request: SetWebSearchSettingsRequest,
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<WebSearchSettingsView> {
    let root = state.data_root();
    // Brave is the only backend: saving with search enabled but no usable key
    // (about to be stored, or already on disk) must be rejected outright
    // rather than silently persisting a configuration that fails closed on
    // every request. This must be resolved against the key state this save
    // is *about* to produce, not the state on disk right now.
    let will_have_brave_key = if request.clear_brave_api_key {
        false
    } else if request
        .brave_api_key
        .as_deref()
        .map(str::trim)
        .is_some_and(|secret| !secret.is_empty())
    {
        true
    } else {
        crate::credentials::load(&root, BRAVE_SEARCH_CREDENTIAL_ID)?.is_some()
    };
    validate_settings(&request.settings, will_have_brave_key)?;
    if request.clear_brave_api_key {
        crate::credentials::remove(&root, BRAVE_SEARCH_CREDENTIAL_ID)?;
    } else if let Some(secret) = request
        .brave_api_key
        .as_deref()
        .map(str::trim)
        .filter(|secret| !secret.is_empty())
    {
        crate::credentials::save(&root, BRAVE_SEARCH_CREDENTIAL_ID, secret)?;
    }
    state.set_web_search_settings(request.settings);
    get_web_search_settings(state)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn probe_web_search(
    query: Option<String>,
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<WebSearchProbeResult> {
    let mut settings = state.web_search_settings();
    settings.brave_api_key =
        crate::credentials::load(&state.data_root(), BRAVE_SEARCH_CREDENTIAL_ID)?;
    if !settings.enabled || !settings.mode.allows_search() {
        return Err(AppError::Message(
            "third-party web search must be enabled before probing".into(),
        ));
    }
    let engine = SearchEngine::new(settings)?;
    let request = SearchRequest {
        commands: Some(SearchCommands {
            search_query: Some(vec![json!({
                "q": query.as_deref().unwrap_or("OpenAI Codex")
            })]),
            response_length: Some(json!("short")),
            ..SearchCommands::default()
        }),
        ..SearchRequest::default()
    };
    let response = engine.run(&request).await?;
    Ok(WebSearchProbeResult {
        output: response.output,
        result_count: response.results.len(),
    })
}

/// `will_have_brave_key` reflects the key state this save is about to leave
/// behind (see `set_web_search_settings`), not necessarily what's on disk
/// right now.
fn validate_settings(settings: &WebSearchSettings, will_have_brave_key: bool) -> AppResult<()> {
    if settings.search_context_size.trim().is_empty() {
        return Err(AppError::Message(
            "search context size cannot be empty".into(),
        ));
    }
    if settings.enabled && !will_have_brave_key {
        return Err(AppError::Message(
            "web search is enabled but no Brave Search API key is configured".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_enabled_search_without_a_brave_key() {
        let settings = WebSearchSettings {
            enabled: true,
            ..WebSearchSettings::default()
        };
        assert!(validate_settings(&settings, false).is_err());
    }

    #[test]
    fn allows_enabled_search_with_a_brave_key() {
        let settings = WebSearchSettings {
            enabled: true,
            ..WebSearchSettings::default()
        };
        assert!(validate_settings(&settings, true).is_ok());
    }

    #[test]
    fn allows_disabled_search_without_a_brave_key() {
        // Search left disabled must never be blocked by a missing key: this
        // must not affect proxy readiness/startup when search is simply off.
        let settings = WebSearchSettings::default();
        assert!(!settings.enabled);
        assert!(validate_settings(&settings, false).is_ok());
    }
}
