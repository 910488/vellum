//! Continuation mode resolution for the shared runtime (plan M4).
//!
//! Mirrors Desktop's `continuation.rs` for the route shape this crate
//! actually carries ([`RuntimeModelRoute`]): same-route Official requests stay
//! native because the Codex harness owns their persisted/stateless state
//! representation. Route switches and third-party routes use readable state.
//!
//! M4 first-commit scope: the decision is same-route based (a route switch
//! forces portable replay) and realm/account fingerprinting is deferred
//! until credentials are wired into the runtime — the fixtures are
//! single-route, so the observable decision is identical to Desktop's.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::route::{RuntimeModelRoute, RuntimeProviderKind};

/// How the next request should continue prior conversation state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationMode {
    /// Keep `previous_response_id`; the provider holds the transcript.
    ServerPreviousResponseId,
    /// OpenAI Official stateless replay: expand prior output from local
    /// history and preserve official encrypted reasoning byte-for-byte.
    StatelessEncryptedReplay,
    /// Cross-realm or portable path: strip opaque state and use the semantic
    /// window (the third-party contract the parity fixtures freeze).
    PortableSemanticReplay,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinuationDecision {
    pub mode: ContinuationMode,
    pub same_route: bool,
    pub store_enabled: bool,
}

/// Detect storage-disabled markers for third-party replay policy and
/// diagnostics. Same-route Official dispatch still preserves them natively.
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

/// Ensure Official stateless replay requests ask for encrypted reasoning
/// content (mirrors Desktop).
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

/// Resolve the continuation mode for one request against one route.
/// `previous_route_id` comes from the stored chain; unknown history means
/// "same route" (a first turn with `previous_response_id` still resolves,
/// and the caller fails closed when no chain exists for replay modes).
pub fn resolve_continuation(
    route: &RuntimeModelRoute,
    request: &Value,
    previous_route_id: Option<&str>,
) -> ContinuationDecision {
    let store_enabled = !request_disables_server_store(request);
    let same_route = previous_route_id
        .map(|previous| previous == route.route_id)
        .unwrap_or(true);
    let mode = if same_route && route.provider_kind == RuntimeProviderKind::Official {
        ContinuationMode::ServerPreviousResponseId
    } else {
        ContinuationMode::PortableSemanticReplay
    };
    ContinuationDecision {
        mode,
        same_route,
        store_enabled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::{
        RuntimeAuthKind, RuntimeCompactionCapabilities, RuntimeReasoningCapabilities,
        RuntimeToolCapabilities, RuntimeWireFormat,
    };

    fn route(provider: RuntimeProviderKind) -> RuntimeModelRoute {
        RuntimeModelRoute {
            route_id: "parity".into(),
            catalog_id: "parity-model".into(),
            name: "Test".into(),
            base_url: "http://127.0.0.1:0/v1".into(),
            provider_kind: provider,
            auth_kind: RuntimeAuthKind::None,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: true,
            streaming: true,
            reasoning: true,
            vision: false,
            upstream_model: "test-model".into(),
            context_window: Some(128_000),
            reasoning_capabilities: RuntimeReasoningCapabilities {
                supports_persisted_reasoning: true,
                ..RuntimeReasoningCapabilities::default()
            },
            compaction_capabilities: RuntimeCompactionCapabilities::default(),
            compaction_policy: crate::config::RuntimeCompactionPolicy::default(),
            tool_capabilities: RuntimeToolCapabilities::default(),
            credential_id: None,
            insecure_http_policy: crate::outbound::InsecureHttpPolicy::Deny,
            provider_profile: None,
            access_mode: None,
            chat_capabilities: crate::route::RuntimeChatCapabilities::default(),
        }
    }

    #[test]
    fn third_party_route_is_portable_semantic_replay_even_with_store_enabled() {
        let decision = resolve_continuation(
            &route(RuntimeProviderKind::OpenAiCompatible),
            &json!({"previous_response_id": "resp_1", "input": "hi"}),
            Some("parity"),
        );
        assert_eq!(decision.mode, ContinuationMode::PortableSemanticReplay);
        assert!(decision.same_route);
        assert!(decision.store_enabled);
    }

    #[test]
    fn same_route_official_uses_server_resume() {
        let decision = resolve_continuation(
            &route(RuntimeProviderKind::Official),
            &json!({"previous_response_id": "resp_1", "input": "hi"}),
            Some("parity"),
        );
        assert_eq!(decision.mode, ContinuationMode::ServerPreviousResponseId);
        assert!(decision.same_route);
    }

    #[test]
    fn store_false_official_stays_native() {
        let decision = resolve_continuation(
            &route(RuntimeProviderKind::Official),
            &json!({"store": false, "previous_response_id": "resp_1", "input": "hi"}),
            Some("parity"),
        );
        assert_eq!(decision.mode, ContinuationMode::ServerPreviousResponseId);
        assert!(!decision.store_enabled);
    }

    #[test]
    fn official_without_persisted_reasoning_stays_native() {
        let mut route = route(RuntimeProviderKind::Official);
        route.reasoning_capabilities.supports_persisted_reasoning = false;
        let decision = resolve_continuation(
            &route,
            &json!({"previous_response_id": "resp_1", "input": "hi"}),
            Some("parity"),
        );
        assert_eq!(decision.mode, ContinuationMode::ServerPreviousResponseId);
    }

    #[test]
    fn route_switch_forces_portable_replay() {
        let decision = resolve_continuation(
            &route(RuntimeProviderKind::Official),
            &json!({"previous_response_id": "resp_1", "input": "hi"}),
            Some("some-other-route"),
        );
        assert_eq!(decision.mode, ContinuationMode::PortableSemanticReplay);
        assert!(!decision.same_route);
    }

    #[test]
    fn store_false_is_detected_from_store_field() {
        assert!(request_disables_server_store(&json!({"store": false})));
        assert!(!request_disables_server_store(&json!({"store": true})));
        assert!(!request_disables_server_store(&json!({"input": "hi"})));
    }

    #[test]
    fn zdr_metadata_markers_force_local_replay() {
        for marker in ["zdr", "zero_data_retention", "stateless"] {
            assert!(
                request_disables_server_store(&json!({"metadata": {marker: true}})),
                "metadata.{marker} must disable the server store"
            );
        }
        assert!(!request_disables_server_store(
            &json!({"metadata": {"zdr": false}})
        ));
    }

    #[test]
    fn ensure_encrypted_reasoning_include_is_idempotent() {
        let mut request = json!({"input": "hi"});
        ensure_encrypted_reasoning_include(&mut request);
        assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
        ensure_encrypted_reasoning_include(&mut request);
        assert_eq!(request["include"].as_array().unwrap().len(), 1);
    }
}
