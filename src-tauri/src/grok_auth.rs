use crate::error::{AppError, AppResult};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};
use url::Url;
use zeroize::Zeroizing;

/// The exact host Grok CLI's own transport (and Vellum's hardcoded built-in
/// `grok-cli` route) talks to. `models_cache.json` metadata is never used to
/// route a request — Vellum's route keeps its own hardcoded base URL — but a
/// cache entry naming any other host is untrusted and its model is rejected;
/// treating an unexpected host as trustworthy metadata would let a
/// compromised or malformed cache file smuggle in a model Vellum never
/// verified against xAI's real Responses transport.
pub const GROK_CLI_PROXY_HOST: &str = "cli-chat-proxy.grok.com";
pub const GROK_CLI_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";

/// The only Grok models Vellum currently ships verified Responses support
/// for. `grok models`/`models_cache.json` may list others (older or newer
/// releases, Chat-only variants, internal previews); anything outside this
/// set is excluded from the published catalog with a diagnostic, never
/// silently promoted.
const VERIFIED_GROK_MODELS: &[&str] = &["grok-4.6", "grok-4.5"];

/// One Grok model's validated Responses-wire capability, resolved from
/// `models_cache.json` (the CLI's own capability authority — never inferred
/// from the `grok models` text listing alone).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GrokModelCapability {
    pub model: String,
    pub display_name: String,
    pub context_window: Option<u64>,
    pub reasoning_efforts: Vec<String>,
    pub default_reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GrokModelCatalog {
    pub models: Vec<String>,
    pub default_model: Option<String>,
    /// Validated capability metadata for each published model, resolved
    /// against `models_cache.json`. Empty for catalogs built before this
    /// validation existed (never required — callers that only need model
    /// ids keep working against `models`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<GrokModelCapability>,
    /// Human-readable reasons a candidate from `grok models`' own listing
    /// was excluded from `models` (unknown backend, non-xAI host, hidden,
    /// unsupported, or simply not one of the verified 4.6/4.5 models).
    /// Fail-closed means visibly excluded with a reason here, never a
    /// silent drop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
}

impl GrokModelCatalog {
    #[cfg(test)]
    fn from_cli_list(models: Vec<String>, default_model: Option<String>) -> Self {
        Self {
            models,
            default_model,
            ..Default::default()
        }
    }
}

pub struct GrokCredential {
    pub access_token: Zeroizing<String>,
    pub user_id: Option<String>,
    pub email: Option<String>,
}

type CredentialCandidate = (
    String,
    Option<DateTime<Utc>>,
    Option<String>,
    Option<String>,
);

pub async fn resolve() -> AppResult<GrokCredential> {
    let root = grok_home();
    match read_credential(&root)? {
        Some((credential, expires_at))
            if expires_at.is_none_or(|expires| Utc::now() + Duration::seconds(60) < expires) =>
        {
            Ok(credential)
        }
        _ => {
            refresh_home(&root).await?;
            read_credential(&root)?
                .map(|(credential, _)| credential)
                .ok_or_else(|| {
                    AppError::Message("找不到有效 Grok 登入。請先執行 `grok login`。".into())
                })
        }
    }
}

pub fn grok_home() -> PathBuf {
    std::env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".grok")))
        .unwrap_or_else(|| std::env::temp_dir().join(".grok"))
}

pub fn grok_executable(root: &Path) -> PathBuf {
    let binary = if cfg!(windows) { "grok.exe" } else { "grok" };
    let local = root.join("bin").join(binary);
    if local.exists() {
        return local;
    }
    // Managed accounts intentionally use an isolated GROK_HOME and therefore
    // do not contain a second CLI binary. Reuse the user's official Grok CLI
    // executable while pointing its mutable profile at the isolated home.
    let external = dirs::home_dir()
        .map(|home| home.join(".grok").join("bin").join(binary))
        .filter(|path| path.is_file());
    external.unwrap_or_else(|| PathBuf::from(binary))
}

pub fn is_cli_installed() -> bool {
    is_cli_installed_with_path(&grok_home(), std::env::var_os("PATH").as_deref())
}

fn is_cli_installed_with_path(root: &Path, path: Option<&std::ffi::OsStr>) -> bool {
    let binary = if cfg!(windows) { "grok.exe" } else { "grok" };
    if root.join("bin").join(binary).is_file() {
        return true;
    }
    path.into_iter()
        .flat_map(std::env::split_paths)
        .any(|directory| directory.join(binary).is_file())
}

pub fn client_version(root: &Path) -> Option<String> {
    let value = std::fs::read_to_string(root.join("version.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())?;
    value
        .get("version")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .map(str::to_string)
}

pub fn cli_version(root: &Path) -> Option<String> {
    let output = crate::process::background_command(grok_executable(root))
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .find(|part| {
            part.chars().next().is_some_and(|ch| ch.is_ascii_digit())
                && part.contains('.')
                && part.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
        })
        .map(str::to_owned)
}

pub fn agent_id(root: &Path) -> Option<String> {
    std::fs::read_to_string(root.join("agent_id"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Ask the installed official Grok Build CLI for its live model catalog.
/// This intentionally uses the selected account's isolated GROK_HOME so the
/// result matches the credential Vellum will use for inference.
pub async fn discover_models(root: &Path) -> AppResult<GrokModelCatalog> {
    let mut command = crate::process::background_tokio_command(grok_executable(root));
    command
        .arg("models")
        .env("GROK_HOME", root)
        .env("GROK_DISABLE_AUTOUPDATER", "1")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .map_err(|_| AppError::Message("Grok model discovery timed out".into()))?
        .map_err(|error| AppError::Message(format!("start `grok models`: {error}")))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(AppError::Message(format!(
            "`grok models` failed: {}",
            detail.trim().chars().take(512).collect::<String>()
        )));
    }
    let cli = parse_models_output(&String::from_utf8_lossy(&output.stdout))?;
    apply_models_cache(cli, &root.join("models_cache.json"))
}

/// Merge the CLI's own available-model listing with `models_cache.json` —
/// the transport authority for which of those models are real, verified,
/// Responses-capable Grok models. A caller (`refresh_grok_model_catalog`)
/// that receives `Err` here must leave whatever catalog it already has in
/// place rather than clearing it: refresh failure keeps the last-known-good
/// catalog, it never empties the runtime's view of what Grok models exist.
pub fn apply_models_cache(cli: GrokModelCatalog, cache_path: &Path) -> AppResult<GrokModelCatalog> {
    let text = std::fs::read_to_string(cache_path).map_err(|error| {
        AppError::Message(format!(
            "Grok models_cache.json is missing after `grok models`: {}: {error}",
            cache_path.display()
        ))
    })?;
    let cache: Value = serde_json::from_str(&text).map_err(|error| {
        AppError::Message(format!("Grok models_cache.json is not valid JSON: {error}"))
    })?;
    merge_cli_list_with_cache(cli, &cache)
}

/// Validate every model the CLI reports against `models_cache.json` and
/// publish only what both agree on. Each candidate that the cache cannot
/// vouch for (unknown/missing `api_backend`, a base URL that is not
/// `https://cli-chat-proxy.grok.com/v1`, `hidden`, `supported_in_api: false`,
/// or simply not one of the verified 4.6/4.5 models) is rejected — excluded
/// with a diagnostic explaining why, never silently dropped. The whole
/// refresh fails (fail closed) only when nothing survives validation.
pub fn merge_cli_list_with_cache(
    cli: GrokModelCatalog,
    cache: &Value,
) -> AppResult<GrokModelCatalog> {
    let (cache_entries, mut diagnostics) = parse_models_cache(cache)?;
    let mut entries = Vec::new();
    let mut models = Vec::new();
    for id in &cli.models {
        if !VERIFIED_GROK_MODELS
            .iter()
            .any(|verified| id.eq_ignore_ascii_case(verified))
        {
            diagnostics.push(format!(
                "skipped Grok model `{id}`: not one of the verified Grok Responses models \
                 ({})",
                VERIFIED_GROK_MODELS.join(", ")
            ));
            continue;
        }
        match cache_entries
            .iter()
            .find(|entry| entry.model.eq_ignore_ascii_case(id))
        {
            Some(entry) => {
                if !models
                    .iter()
                    .any(|known: &String| known.eq_ignore_ascii_case(&entry.model))
                {
                    models.push(entry.model.clone());
                    entries.push(entry.clone());
                }
            }
            None => {
                diagnostics.push(format!(
                    "skipped Grok model `{id}`: models_cache.json has no validated transport \
                     metadata for it"
                ));
            }
        }
    }
    if models.is_empty() {
        return Err(AppError::Message(format!(
            "Grok models_cache.json published no verified Grok 4.6/4.5 Responses models: {}",
            diagnostics.join("; ")
        )));
    }
    let default_model = cli
        .default_model
        .and_then(|default| {
            models
                .iter()
                .find(|model| model.eq_ignore_ascii_case(&default))
                .cloned()
        })
        .or_else(|| models.first().cloned());
    Ok(GrokModelCatalog {
        models,
        default_model,
        entries,
        diagnostics,
    })
}

/// Parse and validate a Grok CLI `models_cache.json` document into
/// per-model capability rows plus diagnostics for every row that failed
/// validation. A row that fails validation (unknown backend, wrong host,
/// missing id, ...) is excluded with a diagnostic — this function itself
/// only fails closed (returns `Err`) when the document's own shape is
/// unusable (not an object, no `models` map).
fn parse_models_cache(cache: &Value) -> AppResult<(Vec<GrokModelCapability>, Vec<String>)> {
    let object = cache
        .as_object()
        .ok_or_else(|| AppError::Message("Grok models_cache.json must be a JSON object".into()))?;
    let Some(models_value) = object.get("models") else {
        return Err(AppError::Message(
            "Grok models_cache.json is missing the models map".into(),
        ));
    };
    let mut diagnostics = Vec::new();
    let mut entries = Vec::new();
    match models_value {
        Value::Object(map) => {
            for (key, value) in map {
                match parse_cache_model_entry(key, value) {
                    Ok(Some(entry)) => entries.push(entry),
                    Ok(None) => {}
                    Err(error) => diagnostics.push(error),
                }
            }
        }
        Value::Array(list) => {
            for value in list {
                let key = value
                    .get("id")
                    .or_else(|| value.get("model"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match parse_cache_model_entry(key, value) {
                    Ok(Some(entry)) => entries.push(entry),
                    Ok(None) => {}
                    Err(error) => diagnostics.push(error),
                }
            }
        }
        _ => {
            return Err(AppError::Message(
                "Grok models_cache.json models must be an object or array".into(),
            ));
        }
    }
    Ok((entries, diagnostics))
}

/// Validate one `models_cache.json` model row. `Ok(None)` means the row is
/// explicitly hidden/unsupported and is dropped without a diagnostic (that
/// is the cache's own stated intent, not a validation failure); `Err`
/// carries a human-readable reason for every other rejection (unknown
/// backend, non-xAI host, missing id, missing metadata) — always visible to
/// the caller, never silent.
fn parse_cache_model_entry(
    key: &str,
    value: &Value,
) -> Result<Option<GrokModelCapability>, String> {
    let info = value.get("info").unwrap_or(value);
    let model = info
        .get("id")
        .or_else(|| info.get("model"))
        .or_else(|| value.get("id"))
        .or_else(|| value.get("model"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .or_else(|| {
            let trimmed = key.trim();
            (!trimmed.is_empty()).then_some(trimmed)
        })
        .ok_or_else(|| "skipped Grok cache row: missing model ID".to_string())?
        .to_string();
    let hidden = info.get("hidden").and_then(Value::as_bool).unwrap_or(false);
    let supported_in_api = info
        .get("supported_in_api")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if hidden {
        return Err(format!(
            "skipped Grok model `{model}`: models_cache.json marks it hidden"
        ));
    }
    if !supported_in_api {
        return Err(format!(
            "skipped Grok model `{model}`: models_cache.json marks supported_in_api=false"
        ));
    }
    let backend = info
        .get("api_backend")
        .or_else(|| info.get("backend"))
        .or_else(|| value.get("api_backend"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|backend| !backend.is_empty());
    match backend {
        Some("responses") => {}
        Some(other) => {
            return Err(format!(
                "skipped Grok model `{model}`: unknown api_backend `{other}`"
            ));
        }
        None => {
            return Err(format!(
                "skipped Grok model `{model}`: models_cache.json is missing api_backend"
            ));
        }
    }
    let raw_base = info
        .get("base_url")
        .or_else(|| info.get("baseUrl"))
        .or_else(|| value.get("base_url"))
        .or_else(|| value.get("api_base_url"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| {
            format!("skipped Grok model `{model}`: models_cache.json is missing base_url")
        })?;
    validate_grok_base_url(raw_base)
        .map_err(|error| format!("skipped Grok model `{model}`: {error}"))?;
    let display_name = info
        .get("name")
        .or_else(|| info.get("display_name"))
        .or_else(|| info.get("displayName"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&model)
        .to_string();
    let context_window = info
        .get("context_window")
        .or_else(|| info.get("contextWindow"))
        .or_else(|| info.get("max_context_length"))
        .and_then(Value::as_u64)
        .filter(|window| *window > 0);
    let (reasoning_efforts, default_reasoning_effort) = parse_cache_reasoning(info);
    Ok(Some(GrokModelCapability {
        model,
        display_name,
        context_window,
        reasoning_efforts,
        default_reasoning_effort,
    }))
}

fn parse_cache_reasoning(info: &Value) -> (Vec<String>, Option<String>) {
    let mut levels = Vec::new();
    if let Some(entries) = info.get("reasoning_efforts").and_then(Value::as_array) {
        for entry in entries {
            let level = entry
                .get("value")
                .or_else(|| entry.get("id"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|level| !level.is_empty());
            if let Some(level) = level {
                if !levels.iter().any(|known: &String| known == level) {
                    levels.push(level.to_string());
                }
            }
        }
    }
    let flagged_default = info
        .get("reasoning_efforts")
        .and_then(Value::as_array)
        .and_then(|entries| {
            entries.iter().find_map(|entry| {
                entry
                    .get("default")
                    .and_then(Value::as_bool)
                    .filter(|is_default| *is_default)
                    .and_then(|_| {
                        entry
                            .get("value")
                            .or_else(|| entry.get("id"))
                            .and_then(Value::as_str)
                    })
                    .map(str::to_owned)
            })
        });
    let default = info
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or(flagged_default)
        .filter(|candidate| levels.iter().any(|level| level == candidate));
    (levels, default)
}

/// Validate a Grok CLI cache base URL. Only HTTPS
/// `cli-chat-proxy.grok.com` is accepted — the one host Vellum's built-in
/// `grok-cli` route actually talks to. Any other host (including a
/// plausible-looking but different xAI domain) fails closed: this function
/// never routes a request, so accepting it would only mean trusting
/// metadata Vellum has no evidence actually came from the real transport.
fn validate_grok_base_url(raw: &str) -> Result<(), String> {
    let url = Url::parse(raw).map_err(|error| format!("invalid Grok base URL `{raw}`: {error}"))?;
    if url.scheme() != "https" {
        return Err(format!(
            "Grok base URL must be HTTPS, got `{}`",
            url.scheme()
        ));
    }
    if url.host_str() != Some(GROK_CLI_PROXY_HOST) {
        return Err(format!(
            "Grok base URL host must be `{GROK_CLI_PROXY_HOST}`, got `{}`",
            url.host_str().unwrap_or("")
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Grok base URL must not embed credentials".into());
    }
    if url.port().is_some() {
        return Err("Grok base URL must use the default HTTPS port".into());
    }
    Ok(())
}

fn parse_models_output(output: &str) -> AppResult<GrokModelCatalog> {
    let clean = strip_ansi(output);
    let default_model = clean.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Default model:")
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_string)
    });
    let mut in_models = false;
    let mut models = Vec::new();
    for line in clean.lines() {
        let line = line.trim();
        if line == "Available models:" {
            in_models = true;
            continue;
        }
        if !in_models {
            continue;
        }
        let Some(candidate) = line.strip_prefix("* ").or_else(|| line.strip_prefix("- ")) else {
            continue;
        };
        let model = candidate
            .strip_suffix(" (default)")
            .unwrap_or(candidate)
            .trim();
        if !model.is_empty()
            && !models
                .iter()
                .any(|known: &String| known.eq_ignore_ascii_case(model))
        {
            models.push(model.to_string());
        }
    }
    if let Some(default) = default_model.as_deref() {
        if !models
            .iter()
            .any(|model| model.eq_ignore_ascii_case(default))
        {
            models.insert(0, default.to_string());
        }
    }
    if models.is_empty() {
        return Err(AppError::Message(
            "`grok models` returned no available models".into(),
        ));
    }
    Ok(GrokModelCatalog {
        models,
        default_model,
        ..Default::default()
    })
}

fn strip_ansi(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for code in chars.by_ref() {
                if ('@'..='~').contains(&code) {
                    break;
                }
            }
        } else {
            output.push(ch);
        }
    }
    output
}

pub(crate) fn read_credential(
    root: &Path,
) -> AppResult<Option<(GrokCredential, Option<DateTime<Utc>>)>> {
    let path = root.join("auth.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(AppError::Message(format!(
                "無法讀取 Grok 登入檔 {}：{error}",
                path.display()
            )));
        }
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| AppError::Message(format!("Grok auth.json 格式錯誤：{error}")))?;
    let mut entries = Vec::new();
    collect_credentials(&value, &mut entries);
    entries.sort_by_key(|(_, expires, _, _)| *expires);
    Ok(entries.pop().map(|(token, expires_at, user_id, email)| {
        (
            GrokCredential {
                access_token: Zeroizing::new(token),
                user_id,
                email,
            },
            expires_at,
        )
    }))
}

fn collect_credentials(value: &Value, output: &mut Vec<CredentialCandidate>) {
    match value {
        Value::Object(object) => {
            if let Some(token) = object
                .get("access_token")
                .or_else(|| object.get("key"))
                .and_then(Value::as_str)
                .filter(|token| !token.trim().is_empty())
            {
                let expires_at = object
                    .get("expires_at")
                    .and_then(Value::as_str)
                    .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                    .map(|value| value.with_timezone(&Utc));
                if expires_at.is_none_or(|expires| expires > Utc::now()) {
                    output.push((
                        token.to_string(),
                        expires_at,
                        object
                            .get("user_id")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        object
                            .get("email")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    ));
                }
            }
            for child in object.values() {
                collect_credentials(child, output);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_credentials(child, output);
            }
        }
        _ => {}
    }
}

pub(crate) async fn refresh_home(root: &Path) -> AppResult<()> {
    let mut command = crate::process::background_tokio_command(grok_executable(root));
    command
        .arg("models")
        .env("GROK_HOME", root)
        .env("GROK_DISABLE_AUTOUPDATER", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .map_err(|_| AppError::Message("Grok 登入刷新逾時；請執行 `grok login`。".into()))?
        .map_err(|error| AppError::Message(format!("無法執行 `grok models`：{error}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(AppError::Message(format!(
            "Grok 登入刷新失敗：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_live_grok_models_output_and_default() {
        let catalog = parse_models_output(
            "You are logged in with grok.com.\n\nDefault model: grok-4.6\n\nAvailable models:\n  * grok-4.6 (default)\n  - grok-4.5\n",
        )
        .unwrap();
        assert_eq!(catalog.default_model.as_deref(), Some("grok-4.6"));
        assert_eq!(catalog.models, vec!["grok-4.6", "grok-4.5"]);
    }

    #[test]
    fn recursively_finds_session_credentials() {
        let value = serde_json::json!({
            "accounts": [{"session": {"access_token": "token", "user_id": "u"}}]
        });
        let mut entries = Vec::new();
        collect_credentials(&value, &mut entries);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "token");
        assert_eq!(entries[0].2.as_deref(), Some("u"));
    }

    #[test]
    fn reads_client_identity_from_grok_home() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("version.json"), r#"{"version":"0.2.112"}"#).unwrap();
        std::fs::write(temp.path().join("agent_id"), " agent-1 \n").unwrap();
        assert_eq!(client_version(temp.path()).as_deref(), Some("0.2.112"));
        assert_eq!(agent_id(temp.path()).as_deref(), Some("agent-1"));
    }

    #[test]
    fn detects_grok_cli_in_home_or_path() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let path_dir = temp.path().join("path");
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::create_dir_all(&path_dir).unwrap();
        let binary = if cfg!(windows) { "grok.exe" } else { "grok" };

        assert!(!is_cli_installed_with_path(&home, None));

        std::fs::write(path_dir.join(binary), b"test").unwrap();
        let path = std::env::join_paths([&path_dir]).unwrap();
        assert!(is_cli_installed_with_path(&home, Some(path.as_os_str())));

        std::fs::remove_file(path_dir.join(binary)).unwrap();
        std::fs::write(home.join("bin").join(binary), b"test").unwrap();
        assert!(is_cli_installed_with_path(&home, None));
    }

    fn official_cache() -> Value {
        serde_json::json!({
            "models": {
                "grok-4.6": {
                    "info": {
                        "id": "grok-4.6",
                        "base_url": "https://cli-chat-proxy.grok.com/v1",
                        "name": "Grok 4.6",
                        "api_backend": "responses",
                        "context_window": 500000,
                        "hidden": false,
                        "supported_in_api": true,
                        "reasoning_effort": "high",
                        "reasoning_efforts": [
                            {"id": "high", "value": "high", "default": true},
                            {"id": "medium", "value": "medium", "default": false}
                        ]
                    }
                },
                "grok-4.5": {
                    "info": {
                        "id": "grok-4.5",
                        "base_url": "https://cli-chat-proxy.grok.com/v1",
                        "name": "Grok 4.5",
                        "api_backend": "responses",
                        "context_window": 500000,
                        "hidden": false,
                        "supported_in_api": true,
                        "reasoning_efforts": [
                            {"id": "high", "value": "high", "default": true}
                        ]
                    }
                }
            }
        })
    }

    #[test]
    fn grok_46_and_45_are_published_with_cache_capabilities() {
        let cli = GrokModelCatalog::from_cli_list(
            vec!["grok-4.6".into(), "grok-4.5".into()],
            Some("grok-4.6".into()),
        );
        let catalog = merge_cli_list_with_cache(cli, &official_cache()).unwrap();
        assert_eq!(catalog.models, vec!["grok-4.6", "grok-4.5"]);
        assert_eq!(catalog.default_model.as_deref(), Some("grok-4.6"));
        let grok46 = catalog
            .entries
            .iter()
            .find(|entry| entry.model == "grok-4.6")
            .unwrap();
        assert_eq!(grok46.display_name, "Grok 4.6");
        assert_eq!(grok46.context_window, Some(500_000));
        assert_eq!(grok46.reasoning_efforts, vec!["high", "medium"]);
        assert_eq!(grok46.default_reasoning_effort.as_deref(), Some("high"));
        assert!(catalog.diagnostics.is_empty());
    }

    #[test]
    fn publishes_only_verified_grok_46_45_nothing_else() {
        // A CLI listing that also names a model outside the verified set
        // (e.g. a future/preview release) must never surface it, even if
        // models_cache.json would otherwise validate it cleanly.
        let mut cache = official_cache();
        cache["models"]["grok-5-preview"] = serde_json::json!({
            "info": {
                "id": "grok-5-preview",
                "base_url": "https://cli-chat-proxy.grok.com/v1",
                "api_backend": "responses",
                "hidden": false,
                "supported_in_api": true
            }
        });
        let cli = GrokModelCatalog::from_cli_list(
            vec!["grok-4.6".into(), "grok-5-preview".into()],
            Some("grok-4.6".into()),
        );
        let catalog = merge_cli_list_with_cache(cli, &cache).unwrap();
        assert_eq!(catalog.models, vec!["grok-4.6"]);
        assert!(catalog
            .diagnostics
            .iter()
            .any(|item| item.contains("grok-5-preview") && item.contains("verified")));
    }

    #[test]
    fn hidden_and_unsupported_models_fail_closed_and_are_excluded() {
        let mut cache = official_cache();
        cache["models"]["grok-4.6"]["info"]["hidden"] = serde_json::json!(true);
        let cli = GrokModelCatalog::from_cli_list(
            vec!["grok-4.6".into(), "grok-4.5".into()],
            Some("grok-4.5".into()),
        );
        let catalog = merge_cli_list_with_cache(cli, &cache).unwrap();
        assert_eq!(catalog.models, vec!["grok-4.5"]);
        assert!(catalog
            .diagnostics
            .iter()
            .any(|item| item.contains("grok-4.6") && item.contains("hidden")));
    }

    #[test]
    fn unknown_backend_and_non_xai_host_fail_closed() {
        assert!(parse_cache_model_entry(
            "grok-4.6",
            &serde_json::json!({
                "info": {
                    "id": "grok-4.6",
                    "base_url": "https://cli-chat-proxy.grok.com/v1",
                    "api_backend": "anthropic"
                }
            }),
        )
        .unwrap_err()
        .contains("unknown api_backend"));

        assert!(parse_cache_model_entry(
            "grok-4.6",
            &serde_json::json!({
                "info": {
                    "id": "grok-4.6",
                    "base_url": "https://cli-chat-proxy.grok.com/v1"
                }
            }),
        )
        .unwrap_err()
        .contains("missing api_backend"));

        // `api.x.ai` is a real xAI-owned domain, but it is not the Grok CLI
        // transport host Vellum's route actually talks to — still rejected.
        assert!(validate_grok_base_url("https://api.x.ai/v1")
            .unwrap_err()
            .contains("cli-chat-proxy.grok.com"));
        assert!(validate_grok_base_url("http://cli-chat-proxy.grok.com/v1")
            .unwrap_err()
            .contains("HTTPS"));
    }

    #[test]
    fn unknown_backend_model_is_excluded_but_the_verified_sibling_still_publishes() {
        let mut cache = official_cache();
        cache["models"]["grok-4.6"]["info"]["api_backend"] = serde_json::json!("chat");
        let cli = GrokModelCatalog::from_cli_list(
            vec!["grok-4.6".into(), "grok-4.5".into()],
            Some("grok-4.5".into()),
        );
        let catalog = merge_cli_list_with_cache(cli, &cache).unwrap();
        assert_eq!(catalog.models, vec!["grok-4.5"]);
        assert!(catalog
            .diagnostics
            .iter()
            .any(|item| item.contains("grok-4.6") && item.contains("unknown api_backend")));
    }

    #[test]
    fn nothing_validating_fails_the_whole_refresh_closed() {
        // When every candidate is rejected, the refresh itself must fail
        // (never publish an empty catalog) so the caller keeps whatever
        // catalog it already has.
        let mut cache = official_cache();
        cache["models"]["grok-4.6"]["info"]["base_url"] =
            serde_json::json!("https://evil.example/v1");
        cache["models"]["grok-4.5"]["info"]["base_url"] =
            serde_json::json!("https://evil.example/v1");
        let cli = GrokModelCatalog::from_cli_list(
            vec!["grok-4.6".into(), "grok-4.5".into()],
            Some("grok-4.6".into()),
        );
        let error = merge_cli_list_with_cache(cli, &cache)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no verified Grok 4.6/4.5"));
    }

    #[test]
    fn a_refresh_failure_never_replaces_the_last_known_good_catalog() {
        // `apply_models_cache` (and therefore `discover_models`) surfaces a
        // failure without ever constructing a replacement catalog value —
        // there is nothing for a caller to accidentally adopt in place of
        // its last-known-good state.
        let temp = tempfile::tempdir().unwrap();
        let cache_path = temp.path().join("models_cache.json");
        // No models_cache.json written: simulates a `grok models` run that
        // succeeded (so `parse_models_output` returns Ok) but whose
        // transport metadata never landed on disk.
        let cli = GrokModelCatalog::from_cli_list(vec!["grok-4.6".into()], Some("grok-4.6".into()));
        let error = apply_models_cache(cli, &cache_path)
            .unwrap_err()
            .to_string();
        assert!(error.contains("models_cache.json is missing"));
    }

    #[test]
    fn malformed_cache_document_fails_closed_not_empty() {
        let error = merge_cli_list_with_cache(
            GrokModelCatalog::from_cli_list(vec!["grok-4.6".into()], None),
            &serde_json::json!([1, 2, 3]),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("must be a JSON object"));
    }
}
