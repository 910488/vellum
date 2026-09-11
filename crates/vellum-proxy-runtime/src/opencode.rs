//! OpenCode Zen / OpenCode Go compatibility.
//!
//! Vellum never downloads, launches, or talks to the OpenCode CLI. `public`
//! is the fixed free-access identity the OpenCode app sends; it is not a
//! second secret derived from a private API key.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::diagnostics::bounded_text;
use crate::grok_session::conversation_key_from_request;
use crate::route::{RuntimeAccessMode, RuntimeProviderProfile};

pub const OPENCODE_ZEN_BASE_URL: &str = "https://opencode.ai/zen/v1";
pub const OPENCODE_GO_BASE_URL: &str = "https://opencode.ai/zen/go/v1";
pub const OPENCODE_PUBLIC_TOKEN: &str = "public";
pub const OPENCODE_CLIENT: &str = "vellum";
pub const DEFAULT_QUOTA_RETRY_AFTER_SECS: u64 = 60;
pub const MAX_QUOTA_RETRY_AFTER_SECS: u64 = 24 * 60 * 60;
/// Console Go rejects request bodies above 4.5 MiB. Keep translated Chat
/// requests below 4 MiB so JSON/tool metadata still has bounded headroom.
pub const OPENCODE_GO_REQUEST_BODY_BYTES: usize = 9 * 1024 * 1024 / 2;
pub const OPENCODE_GO_REQUEST_TARGET_BYTES: usize = 4 * 1024 * 1024;
const QUOTA_DIAGNOSTIC_LIMIT: usize = 240;

/// Official Zen catalog rows confirmed free. Unknown ids are never inferred
/// from a `-free` suffix or from `big-pickle`-style aliases outside this list.
const CONFIRMED_FREE_ZEN_MODELS: &[&str] = &[
    "big-pickle",
    "deepseek-v4-flash-free",
    "glm-4.7-free",
    "glm-5-free",
    "grok-code",
    "hy3-free",
    "hy3-preview-free",
    "kimi-k2.5-free",
    "laguna-s-2.1-free",
    "ling-2.6-flash-free",
    "ling-3.0-flash-fin-free",
    "ling-3.0-flash-free",
    "longcat-2.0-free",
    "mimo-v2-flash-free",
    "mimo-v2-omni-free",
    "mimo-v2-pro-free",
    "mimo-v2.5-free",
    "minimax-m2.1-free",
    "minimax-m2.5-free",
    "minimax-m3-free",
    "muse-spark-1.2-contributor-free",
    "muse-spark-1.3-contributor-free",
    "nemotron-3-super-free",
    "nemotron-3-ultra-free",
    "nemotron-3.5-lightning-free",
    "north-mini-code-free",
    "qwen3.6-plus-free",
    "ring-2.6-1t-free",
    "trinity-large-preview-free",
    "x-preview-f-free",
];

/// Exact OpenCode catalog roots. Other OpenAI-compatible URLs, including
/// lookalike hosts and extra path segments, stay generic.
pub fn infer_provider_profile(base_url: &str) -> Option<RuntimeProviderProfile> {
    let url = url::Url::parse(&normalize_opencode_base_url(base_url)).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    if !url
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("opencode.ai"))
    {
        return None;
    }
    match url.path().trim_end_matches('/') {
        "/zen/v1" => Some(RuntimeProviderProfile::OpenCodeZen),
        "/zen/go/v1" => Some(RuntimeProviderProfile::OpenCodeGo),
        _ => None,
    }
}

pub fn is_opencode_endpoint(base_url: &str) -> bool {
    infer_provider_profile(base_url).is_some()
}

pub fn is_opencode_zen_endpoint(base_url: &str) -> bool {
    infer_provider_profile(base_url) == Some(RuntimeProviderProfile::OpenCodeZen)
}

pub fn is_opencode_go_endpoint(base_url: &str) -> bool {
    infer_provider_profile(base_url) == Some(RuntimeProviderProfile::OpenCodeGo)
}

/// Strip catalog/wire suffixes so a pasted `/models` URL still matches the
/// exact Zen/Go roots. Does not rewrite unrelated OpenAI-compatible paths
/// into OpenCode.
pub fn normalize_opencode_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim();
    let Ok(mut url) = url::Url::parse(trimmed) else {
        return trimmed.trim_end_matches('/').to_string();
    };
    if let Some(host) = url.host_str().map(|host| host.to_ascii_lowercase()) {
        let _ = url.set_host(Some(&host));
    }
    url.set_query(None);
    url.set_fragment(None);
    let mut path = url.path().trim_end_matches('/').to_string();
    for suffix in ["/models", "/chat/completions", "/responses"] {
        if path.len() > suffix.len() && path.to_ascii_lowercase().ends_with(suffix) {
            path.truncate(path.len() - suffix.len());
            path = path.trim_end_matches('/').to_string();
        }
    }
    url.set_path(&path);
    let mut normalized = url.to_string();
    if normalized.ends_with('/') {
        normalized.pop();
    }
    normalized
}

pub fn confirmed_free_zen_model(model: &str) -> bool {
    CONFIRMED_FREE_ZEN_MODELS
        .iter()
        .any(|id| id.eq_ignore_ascii_case(model))
}

/// Official display names for Zen catalog ids whose id is not itself
/// readable. Zen's `/models` response carries no name field at all (just
/// `id`, `object`, `created`, `owned_by`), so an id like `x-preview-f-free`
/// would otherwise show up in the Codex model picker exactly as-is. Sourced
/// from OpenCode's own catalog UI, the same way `CONFIRMED_FREE_ZEN_MODELS`
/// is -- never guessed from the id string.
const CONFIRMED_ZEN_MODEL_DISPLAY_NAMES: &[(&str, &str)] = &[("x-preview-f-free", "Ox Alpha Free")];

pub fn confirmed_zen_model_display_name(model: &str) -> Option<&'static str> {
    CONFIRMED_ZEN_MODEL_DISPLAY_NAMES
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(model))
        .map(|(_, name)| *name)
}

/// Wire for an OpenCode catalog model. GPT/Grok stay on Responses; Claude,
/// Gemini and Qwen 3.5+ native protocols are unsupported; everything else
/// (including MiMo) is Chat Completions.
pub fn wire_for_model(model: &str) -> Option<&'static str> {
    let model = model.to_ascii_lowercase();
    if model.starts_with("claude-")
        || model.starts_with("gemini-")
        || model.starts_with("qwen3.5-")
        || model.starts_with("qwen3.6-")
        || model.starts_with("qwen3.7-")
        || model.starts_with("qwen3.8-")
    {
        return None;
    }
    if model.starts_with("gpt-") || model.starts_with("grok-") || model.starts_with("muse-spark-") {
        Some("responses")
    } else {
        Some("chat")
    }
}

pub fn default_access_mode(profile: RuntimeProviderProfile, model: &str) -> RuntimeAccessMode {
    match profile {
        RuntimeProviderProfile::OpenCodeZen if confirmed_free_zen_model(model) => {
            RuntimeAccessMode::AnonymousFree
        }
        RuntimeProviderProfile::OpenCodeZen | RuntimeProviderProfile::OpenCodeGo => {
            RuntimeAccessMode::Credentialed
        }
    }
}

/// Persist profile/access mode for an exact Zen/Go URL. Unknown models stay
/// credentialed; they are never guessed free from a name suffix.
pub fn migrate_route_profile(
    base_url: &str,
    upstream_model: &str,
    provider_profile: &mut Option<RuntimeProviderProfile>,
    access_mode: &mut Option<RuntimeAccessMode>,
) -> bool {
    let inferred = infer_provider_profile(base_url);
    let mut changed = false;
    if provider_profile.is_none() {
        if let Some(profile) = inferred {
            *provider_profile = Some(profile);
            changed = true;
        }
    }
    if access_mode.is_none() {
        if let Some(profile) = *provider_profile {
            *access_mode = Some(default_access_mode(profile, upstream_model));
            changed = true;
        }
    }
    changed
}

pub fn authorization_token(
    access_mode: RuntimeAccessMode,
    stored_secret: Option<&str>,
) -> Result<String, String> {
    match access_mode {
        RuntimeAccessMode::AnonymousFree => Ok(OPENCODE_PUBLIC_TOKEN.to_string()),
        RuntimeAccessMode::Credentialed => stored_secret
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| "OpenCode credentialed models require an API key".into()),
    }
}

/// SHA-256 conversation identity from the existing conversation-key/history
/// mechanism. Never the raw Codex thread id or prompt cache key.
pub fn session_identity(request: &Value) -> Option<String> {
    conversation_key_from_request(request)
}

/// Stable per-Codex-request identity: session + exact upstream body. A retry
/// of the same body reuses the id; a later turn with a different body does not.
pub fn request_identity(session: &str, upstream_body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session.as_bytes());
    hasher.update([0]);
    hasher.update(upstream_body);
    format!("{:x}", hasher.finalize())
}

pub fn user_agent() -> String {
    format!("opencode/vellum-{}", crate::PROXY_RUNTIME_VERSION)
}

pub fn identity_headers(request: &Value, upstream_body: &[u8]) -> Vec<(String, String)> {
    let session = session_identity(request);
    let request_id = request_identity(
        session.as_deref().unwrap_or("vellum-opencode"),
        upstream_body,
    );
    let mut headers = vec![
        ("user-agent".into(), user_agent()),
        ("x-opencode-client".into(), OPENCODE_CLIENT.into()),
        ("x-opencode-request".into(), request_id),
    ];
    if let Some(session) = session {
        headers.push(("x-opencode-session".into(), session));
    }
    headers
}

/// Remove OpenAI Responses-only and conversation-identity fields from an
/// already-translated Chat (or compatible Responses) body.
pub fn strip_openai_only_fields(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    for key in [
        "previous_response_id",
        "prompt_cache_key",
        "store",
        "include",
        "truncation",
        "service_tier",
        "prompt_cache_retention",
        "client_metadata",
        "safety_identifier",
        "context_management",
        "reasoning_encrypt",
    ] {
        object.remove(key);
    }
}

pub fn parse_retry_after(headers: &[(String, String)]) -> Option<u64> {
    let value = headers.iter().find_map(|(name, value)| {
        name.eq_ignore_ascii_case("retry-after")
            .then_some(value.as_str())
    })?;
    let trimmed = value.trim();
    if let Ok(seconds) = trimmed.parse::<u64>() {
        return Some(seconds.clamp(1, MAX_QUOTA_RETRY_AFTER_SECS));
    }
    let deadline = chrono::DateTime::parse_from_rfc2822(trimmed).ok()?;
    let seconds = deadline
        .with_timezone(&chrono::Utc)
        .signed_duration_since(chrono::Utc::now())
        .num_seconds();
    Some((seconds.max(1) as u64).min(MAX_QUOTA_RETRY_AFTER_SECS))
}

pub fn quota_diagnostic(message: &str) -> String {
    bounded_text(message, QUOTA_DIAGNOSTIC_LIMIT)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaCooldownEntry {
    pub retry_after_secs: u64,
    pub diagnostic: String,
    pub until: Instant,
}

impl QuotaCooldownEntry {
    pub fn remaining_secs(&self) -> u64 {
        self.until
            .saturating_duration_since(Instant::now())
            .as_secs()
            .max(1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CooldownKey {
    route_id: String,
    model: String,
}

#[derive(Debug, Default)]
pub struct OpenCodeQuotaCooldown {
    entries: Mutex<HashMap<CooldownKey, QuotaCooldownEntry>>,
}

impl OpenCodeQuotaCooldown {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, route_id: &str, model: &str) -> Option<QuotaCooldownEntry> {
        let key = CooldownKey {
            route_id: route_id.to_string(),
            model: model.to_ascii_lowercase(),
        };
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = entries.get(&key)?;
        if Instant::now() >= entry.until {
            entries.remove(&key);
            return None;
        }
        Some(entry.clone())
    }

    pub fn record(
        &self,
        route_id: &str,
        model: &str,
        retry_after_secs: u64,
        diagnostic: impl Into<String>,
    ) {
        let secs = retry_after_secs.clamp(1, MAX_QUOTA_RETRY_AFTER_SECS);
        let key = CooldownKey {
            route_id: route_id.to_string(),
            model: model.to_ascii_lowercase(),
        };
        let entry = QuotaCooldownEntry {
            retry_after_secs: secs,
            diagnostic: quota_diagnostic(&entry_or_default(diagnostic.into())),
            until: Instant::now() + Duration::from_secs(secs),
        };
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(key, entry);
    }

    pub fn expire(&self, route_id: &str, model: &str) {
        let key = CooldownKey {
            route_id: route_id.to_string(),
            model: model.to_ascii_lowercase(),
        };
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&key);
    }
}

fn entry_or_default(diagnostic: String) -> String {
    if diagnostic.trim().is_empty() {
        "provider quota exceeded".into()
    } else {
        diagnostic
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn infers_only_the_exact_zen_and_go_roots() {
        assert_eq!(
            infer_provider_profile(OPENCODE_ZEN_BASE_URL),
            Some(RuntimeProviderProfile::OpenCodeZen)
        );
        assert_eq!(
            infer_provider_profile("https://opencode.ai/zen/v1/"),
            Some(RuntimeProviderProfile::OpenCodeZen)
        );
        assert_eq!(
            infer_provider_profile("https://opencode.ai/zen/v1/models"),
            Some(RuntimeProviderProfile::OpenCodeZen)
        );
        assert_eq!(
            infer_provider_profile(OPENCODE_GO_BASE_URL),
            Some(RuntimeProviderProfile::OpenCodeGo)
        );
        assert_eq!(
            infer_provider_profile("https://opencode.ai/zen/go/v1/"),
            Some(RuntimeProviderProfile::OpenCodeGo)
        );
        assert_eq!(infer_provider_profile("https://example.test/zen/v1"), None);
        assert_eq!(infer_provider_profile("http://opencode.ai/zen/v1"), None);
        assert_eq!(
            infer_provider_profile("https://opencode.ai:8443/zen/v1"),
            None
        );
        assert_eq!(
            infer_provider_profile("https://user@opencode.ai/zen/v1"),
            None
        );
        assert_eq!(infer_provider_profile("https://opencode.ai/v1"), None);
        assert_eq!(infer_provider_profile("https://api.openai.com/v1"), None);
    }

    #[test]
    fn migrates_exact_urls_and_leaves_other_openai_compatible_routes() {
        let mut profile = None;
        let mut access = None;
        assert!(migrate_route_profile(
            OPENCODE_ZEN_BASE_URL,
            "mimo-v2.5-free",
            &mut profile,
            &mut access
        ));
        assert_eq!(profile, Some(RuntimeProviderProfile::OpenCodeZen));
        assert_eq!(access, Some(RuntimeAccessMode::AnonymousFree));

        let mut profile = None;
        let mut access = None;
        assert!(migrate_route_profile(
            OPENCODE_GO_BASE_URL,
            "mimo-v2.5",
            &mut profile,
            &mut access
        ));
        assert_eq!(profile, Some(RuntimeProviderProfile::OpenCodeGo));
        assert_eq!(access, Some(RuntimeAccessMode::Credentialed));

        let mut profile = None;
        let mut access = None;
        assert!(!migrate_route_profile(
            "https://api.provider.example/v1",
            "qwen3.6",
            &mut profile,
            &mut access
        ));
        assert_eq!(profile, None);
        assert_eq!(access, None);
    }

    #[test]
    fn unknown_models_are_not_guessed_free_from_a_suffix() {
        assert!(confirmed_free_zen_model("mimo-v2.5-free"));
        assert!(confirmed_free_zen_model("big-pickle"));
        assert!(!confirmed_free_zen_model("mystery-free"));
        assert!(!confirmed_free_zen_model("gpt-5-nano"));
        assert!(!confirmed_free_zen_model("mimo-v2.5"));
        assert_eq!(
            default_access_mode(RuntimeProviderProfile::OpenCodeZen, "mystery-free"),
            RuntimeAccessMode::Credentialed
        );
        assert_eq!(
            default_access_mode(RuntimeProviderProfile::OpenCodeGo, "mimo-v2.5-free"),
            RuntimeAccessMode::Credentialed
        );
    }

    #[test]
    fn public_token_is_fixed_and_never_derived_from_a_private_key() {
        assert_eq!(
            authorization_token(RuntimeAccessMode::AnonymousFree, Some("sk-private")).unwrap(),
            OPENCODE_PUBLIC_TOKEN
        );
        assert_eq!(
            authorization_token(RuntimeAccessMode::AnonymousFree, None).unwrap(),
            OPENCODE_PUBLIC_TOKEN
        );
        assert_eq!(
            authorization_token(RuntimeAccessMode::Credentialed, Some("sk-private")).unwrap(),
            "sk-private"
        );
        assert!(authorization_token(RuntimeAccessMode::Credentialed, None).is_err());
        assert!(authorization_token(RuntimeAccessMode::Credentialed, Some("  ")).is_err());
    }

    #[test]
    fn mimo_stays_on_chat_completions() {
        assert_eq!(wire_for_model("mimo-v2.5-free"), Some("chat"));
        assert_eq!(wire_for_model("gpt-5.4-mini"), Some("responses"));
        assert_eq!(wire_for_model("claude-opus-5"), None);
    }

    #[test]
    fn session_is_stable_across_turns_and_request_id_follows_the_body() {
        let turn_one = json!({
            "model": "mimo-v2.5-free",
            "prompt_cache_key": "thread-abc",
            "input": "one"
        });
        let turn_two = json!({
            "model": "mimo-v2.5-free",
            "prompt_cache_key": "thread-abc",
            "input": "two"
        });
        let other = json!({
            "model": "mimo-v2.5-free",
            "prompt_cache_key": "thread-xyz",
            "input": "one"
        });
        let session = session_identity(&turn_one).unwrap();
        assert_eq!(Some(session.clone()), session_identity(&turn_two));
        assert_ne!(Some(session.clone()), session_identity(&other));
        assert!(!session.contains("thread-abc"));
        assert_eq!(session.len(), 64);

        let body_a = br#"{"model":"mimo-v2.5-free"}"#;
        let body_b = br#"{"model":"mimo-v2.5-free","messages":[]}"#;
        let request_a = request_identity(&session, body_a);
        assert_eq!(request_a, request_identity(&session, body_a));
        assert_ne!(request_a, request_identity(&session, body_b));
    }

    #[test]
    fn requests_without_conversation_identity_do_not_share_a_fixed_session() {
        let headers = identity_headers(&json!({"model": "mimo-v2.5-free"}), b"{}");
        assert!(!headers.iter().any(|(name, _)| name == "x-opencode-session"));
        assert!(headers.iter().any(|(name, _)| name == "x-opencode-request"));
    }

    #[test]
    fn retry_after_supports_seconds_dates_and_clamps_extreme_values() {
        assert_eq!(
            parse_retry_after(&[("retry-after".into(), u64::MAX.to_string())]),
            Some(MAX_QUOTA_RETRY_AFTER_SECS)
        );
        assert_eq!(
            parse_retry_after(&[("Retry-After".into(), "Wed, 21 Oct 2037 07:28:00 GMT".into(),)]),
            Some(MAX_QUOTA_RETRY_AFTER_SECS)
        );
    }

    #[test]
    fn identity_headers_are_transparent_and_redacted() {
        let request = json!({
            "prompt_cache_key": "codex-thread-secret",
            "metadata": {"thread_id": "thread_raw"}
        });
        let headers = identity_headers(&request, b"{}");
        let map: HashMap<_, _> = headers.into_iter().collect();
        assert_eq!(
            map.get("user-agent").map(String::as_str),
            Some(user_agent().as_str())
        );
        assert!(map["user-agent"].starts_with("opencode/vellum-"));
        assert_eq!(
            map.get("x-opencode-client").map(String::as_str),
            Some("vellum")
        );
        assert!(!map["x-opencode-session"].contains("codex-thread-secret"));
        assert!(!map["x-opencode-session"].contains("thread_raw"));
        assert!(!map["x-opencode-request"].contains("codex-thread-secret"));
    }

    #[test]
    fn cooldown_is_per_route_and_model() {
        let store = OpenCodeQuotaCooldown::new();
        store.record("route-a", "mimo-v2.5-free", 30, "rate limited");
        assert!(store.get("route-a", "mimo-v2.5-free").is_some());
        assert!(store.get("route-a", "big-pickle").is_none());
        assert!(store.get("route-b", "mimo-v2.5-free").is_none());
        store.expire("route-a", "mimo-v2.5-free");
        assert!(store.get("route-a", "mimo-v2.5-free").is_none());
    }

    #[test]
    fn strip_removes_responses_only_identity_fields() {
        let mut body = json!({
            "model": "mimo-v2.5-free",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [],
            "stream": true,
            "prompt_cache_key": "codex-thread-secret",
            "previous_response_id": "resp_raw",
            "store": false,
            "include": ["reasoning.encrypted_content"]
        });
        strip_openai_only_fields(&mut body);
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("store").is_none());
        assert!(body.get("include").is_none());
        assert_eq!(body["model"], "mimo-v2.5-free");
        assert_eq!(body["stream"], true);
        assert!(body.get("messages").is_some());
        assert!(body.get("tools").is_some());
    }

    #[test]
    fn confirmed_zen_model_display_name_only_covers_the_confirmed_table() {
        assert_eq!(
            confirmed_zen_model_display_name("x-preview-f-free"),
            Some("Ox Alpha Free")
        );
        // Case-insensitive, matching confirmed_free_zen_model's own lookup.
        assert_eq!(
            confirmed_zen_model_display_name("X-Preview-F-Free"),
            Some("Ox Alpha Free")
        );
        // An id not in the table is never guessed at -- None shows the
        // upstream id unchanged, same as any other unmapped model.
        assert_eq!(confirmed_zen_model_display_name("big-pickle"), None);
        assert_eq!(confirmed_zen_model_display_name("not-a-real-model"), None);
    }
}
