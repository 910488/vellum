//! Continuation mode resolution for retained reasoning and history hydrate.
//!
//! Official routes can resume via `previous_response_id` only when store
//! semantics, realm, and capabilities all allow it. Stateless / ZDR requests
//! replay prior output from local history. Only OpenAI Official may replay
//! provider-owned encrypted reasoning; third-party routes use readable state.

use crate::history::realm_fingerprint;
use crate::model::{ContextCapabilities, ProviderKind, Route};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// How the next request should continue prior conversation state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationMode {
    /// Keep `previous_response_id`; provider holds the transcript.
    ServerPreviousResponseId,
    /// OpenAI Official stateless replay. Expand prior output from local history
    /// and preserve official encrypted reasoning byte-for-byte.
    StatelessEncryptedReplay,
    /// Cross-realm or portable path: strip opaque state and use semantic window.
    PortableSemanticReplay,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinuationDecision {
    pub mode: ContinuationMode,
    pub same_route: bool,
    pub same_realm: bool,
    pub store_enabled: bool,
    pub realm_fingerprint: Option<String>,
    pub previous_realm_fingerprint: Option<String>,
}

/// Normalize a model id into a coarse family for continuity checks.
/// Prefer the last path/component so `openai/gpt-5.6-sol` → `gpt-5`, not `openai`.
pub fn model_family(model: &str) -> String {
    let lower = model.trim().to_ascii_lowercase();
    let base = lower
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(&lower)
        .split('@')
        .next()
        .unwrap_or(&lower)
        .trim();
    // gpt-5.6-sol / gpt-5.4-mini → gpt-5; grok-4.5 → grok-4
    let parts = base.split('-').collect::<Vec<_>>();
    if parts.len() >= 2 && !parts[0].chars().any(|c| c.is_ascii_digit()) {
        let major = parts[1]
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect::<String>();
        if !major.is_empty() {
            let major_token: String = major.chars().take_while(|c| *c != '.').collect();
            return format!("{}-{}", parts[0], major_token);
        }
    }
    parts.first().copied().unwrap_or(base).to_string()
}

/// Runtime context collected while preparing a request and used when the
/// terminal response is durably committed.
#[derive(Debug, Clone, Default)]
pub struct ContinuationExecution {
    pub mode: Option<ContinuationMode>,
    pub realm_fingerprint: Option<String>,
    pub model_family: String,
    pub account_subject: Option<String>,
    /// Compaction checkpoints actually consumed by this turn.
    pub consumed_compaction_ids: Vec<String>,
    /// Logical input after a consumed checkpoint has been materialized.
    ///
    /// The client-facing request can contain only the new turn while the
    /// upstream request contains `canonical window + post-checkpoint tail`.
    /// Persisting this snapshot lets durable replay recover the exact boundary
    /// instead of trying to infer it from the original client payload.
    pub durable_compaction_input_items: Option<Vec<Value>>,
    /// Resolved conversation identity shared with history / SessionStatus /
    /// Grok runtime (issue #4 comment 5146162101). Overrides request re-hash.
    pub conversation_key: Option<String>,
}

impl ContinuationExecution {
    pub fn push_compaction_id(&mut self, id: impl Into<String>) {
        let id = id.into();
        if !id.is_empty()
            && !self
                .consumed_compaction_ids
                .iter()
                .any(|existing| existing == &id)
        {
            self.consumed_compaction_ids.push(id);
        }
    }

    /// Merge another execution into this one without dropping consumed checkpoint IDs
    /// (issue #4 comment 5144558715: hydrate must not wipe auto-compact IDs).
    pub fn merge_from(&mut self, other: ContinuationExecution) {
        for id in other.consumed_compaction_ids {
            self.push_compaction_id(id);
        }
        if self.durable_compaction_input_items.is_none() {
            self.durable_compaction_input_items = other.durable_compaction_input_items;
        }
        if self.mode.is_none() {
            self.mode = other.mode;
        }
        if self.realm_fingerprint.is_none() {
            self.realm_fingerprint = other.realm_fingerprint;
        }
        if self.model_family.is_empty() && !other.model_family.is_empty() {
            self.model_family = other.model_family;
        }
        if self.account_subject.is_none() {
            self.account_subject = other.account_subject;
        }
        if self.conversation_key.is_none() {
            self.conversation_key = other.conversation_key;
        }
    }
}

/// `store: false` or explicit ZDR markers force local replay.
pub fn request_disables_server_store(request: &Value) -> bool {
    if request
        .get("store")
        .and_then(Value::as_bool)
        .is_some_and(|store| !store)
    {
        return true;
    }
    // Common ZDR / privacy markers used by Codex and compatible clients.
    if request
        .get("metadata")
        .and_then(Value::as_object)
        .is_some_and(|meta| {
            meta.get("zdr").and_then(Value::as_bool).unwrap_or(false)
                || meta
                    .get("zero_data_retention")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                || meta
                    .get("stateless")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        })
    {
        return true;
    }
    false
}

/// Ensure Official stateless replay requests ask for encrypted reasoning content.
pub fn ensure_encrypted_reasoning_include(request: &mut Value) {
    let Some(object) = request.as_object_mut() else {
        return;
    };
    let include = object.entry("include").or_insert_with(|| json!([]));
    let Some(array) = include.as_array_mut() else {
        *include = json!(["reasoning.encrypted_content"]);
        return;
    };
    let already = array.iter().any(|value| {
        value
            .as_str()
            .is_some_and(|item| item == "reasoning.encrypted_content")
    });
    if !already {
        array.push(json!("reasoning.encrypted_content"));
    }
}

pub fn compute_route_realm_fingerprint(route: &Route, account_subject: Option<&str>) -> String {
    let auth = match route.auth_kind {
        crate::model::AuthKind::ChatGpt => "chatgpt",
        crate::model::AuthKind::Bearer => "bearer",
        crate::model::AuthKind::GrokSession => "grok_session",
        crate::model::AuthKind::None => "none",
    };
    let family = match route.provider_kind {
        ProviderKind::Official => "official",
        ProviderKind::GrokCli => "grok_cli",
        ProviderKind::OpenAiCompatible => "openai_compatible",
    };
    realm_fingerprint(
        family,
        &route.base_url,
        auth,
        account_subject.unwrap_or("anonymous"),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn resolve_continuation(
    route: &Route,
    model: &str,
    capabilities: &ContextCapabilities,
    request: &Value,
    previous_route_id: Option<&str>,
    previous_realm_fingerprint: Option<&str>,
    previous_model: Option<&str>,
    current_realm_fingerprint: Option<&str>,
) -> ContinuationDecision {
    let store_enabled = !request_disables_server_store(request);
    let same_route = previous_route_id
        .map(|previous| previous == route.id)
        .unwrap_or(true);
    let same_realm = match (previous_realm_fingerprint, current_realm_fingerprint) {
        (Some(previous), Some(current)) => previous == current,
        // Unknown prior realm: only treat as same when route matches and we
        // are not requiring account isolation evidence we do not have.
        (None, _) => same_route && !capabilities.requires_same_account_realm,
        (Some(_), None) => false,
    };
    // Model-family names are telemetry only and never grant opaque replay.
    let _ = (model, previous_model);

    let mode = if !same_route || !same_realm {
        ContinuationMode::PortableSemanticReplay
    } else if route.provider_kind == ProviderKind::Official
        && route.server_side_resume
        && store_enabled
        && capabilities.supports_persisted_reasoning
        && same_realm
    {
        ContinuationMode::ServerPreviousResponseId
    } else if route.provider_kind == ProviderKind::Official {
        ContinuationMode::StatelessEncryptedReplay
    } else {
        ContinuationMode::PortableSemanticReplay
    };

    ContinuationDecision {
        mode,
        same_route,
        same_realm,
        store_enabled,
        realm_fingerprint: current_realm_fingerprint.map(str::to_string),
        previous_realm_fingerprint: previous_realm_fingerprint.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AuthKind, WireFormat};

    fn official_route() -> Route {
        Route {
            id: "openai-official".into(),
            name: "OpenAI Official".into(),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            model: "gpt-5.6-sol".into(),
            wire: WireFormat::Responses,
            is_current: true,
            server_side_resume: true,
            streaming: true,
            reasoning: true,
            provider_kind: ProviderKind::Official,
            auth_kind: AuthKind::ChatGpt,
            enabled: true,
            models: vec!["gpt-5.6-sol".into()],
            selected_models: None,
            context_window: Some(272_000),
            model_capabilities: Vec::new(),
            insecure_http_policy: Default::default(),
            catalog_scope: Default::default(),
        }
    }

    #[test]
    fn store_false_forces_stateless_replay_on_official() {
        let route = official_route();
        let caps = ContextCapabilities::resolve(
            ProviderKind::Official,
            "gpt-5.6-sol",
            None,
            Some(272_000),
        );
        let decision = resolve_continuation(
            &route,
            "gpt-5.6-sol",
            &caps,
            &json!({"store": false, "previous_response_id": "resp_1"}),
            Some("openai-official"),
            Some("fp-a"),
            Some("gpt-5.6-sol"),
            Some("fp-a"),
        );
        assert_eq!(decision.mode, ContinuationMode::StatelessEncryptedReplay);
        assert!(!decision.store_enabled);
    }

    #[test]
    fn same_realm_official_uses_server_resume() {
        let route = official_route();
        let caps = ContextCapabilities::resolve(
            ProviderKind::Official,
            "gpt-5.6-sol",
            None,
            Some(272_000),
        );
        let decision = resolve_continuation(
            &route,
            "gpt-5.6-sol",
            &caps,
            &json!({"previous_response_id": "resp_1"}),
            Some("openai-official"),
            Some("fp-a"),
            Some("gpt-5.6-sol"),
            Some("fp-a"),
        );
        assert_eq!(decision.mode, ContinuationMode::ServerPreviousResponseId);
    }

    #[test]
    fn account_realm_change_is_portable() {
        let route = official_route();
        let caps = ContextCapabilities::resolve(
            ProviderKind::Official,
            "gpt-5.6-sol",
            None,
            Some(272_000),
        );
        let decision = resolve_continuation(
            &route,
            "gpt-5.6-sol",
            &caps,
            &json!({"previous_response_id": "resp_1"}),
            Some("openai-official"),
            Some("fp-old"),
            Some("gpt-5.6-sol"),
            Some("fp-new"),
        );
        assert_eq!(decision.mode, ContinuationMode::PortableSemanticReplay);
        assert!(!decision.same_realm);
    }

    #[test]
    fn third_party_same_route_still_uses_readable_semantic_replay() {
        let mut route = official_route();
        route.id = "grok".into();
        route.provider_kind = ProviderKind::GrokCli;
        route.auth_kind = AuthKind::GrokSession;
        route.server_side_resume = false;
        let caps =
            ContextCapabilities::resolve(ProviderKind::GrokCli, "grok-4.5", None, Some(200_000));
        let decision = resolve_continuation(
            &route,
            "grok-4.5",
            &caps,
            &json!({"store": false, "previous_response_id": "resp_1"}),
            Some("grok"),
            Some("fp-a"),
            Some("grok-4.5"),
            Some("fp-a"),
        );
        assert_eq!(decision.mode, ContinuationMode::PortableSemanticReplay);
    }

    #[test]
    fn model_family_helper_keeps_major_token() {
        assert_eq!(model_family("gpt-5.6-sol"), "gpt-5");
        assert_eq!(model_family("grok-4.5"), "grok-4");
        assert_eq!(model_family("openai/gpt-5.6-sol"), "gpt-5");
        assert_eq!(model_family("provider:grok-4.5"), "grok-4");
    }

    #[test]
    fn ensure_include_adds_encrypted_reasoning_once() {
        let mut request = json!({"include": ["file_search_call.results"]});
        ensure_encrypted_reasoning_include(&mut request);
        ensure_encrypted_reasoning_include(&mut request);
        let include = request["include"].as_array().unwrap();
        assert_eq!(
            include
                .iter()
                .filter(|value| value.as_str() == Some("reasoning.encrypted_content"))
                .count(),
            1
        );
    }
}
