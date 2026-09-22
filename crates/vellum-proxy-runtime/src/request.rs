//! Explicit request context (plan §25.4).
//!
//! Provider logic must never see an Axum `Request`: a raw HTTP request would
//! let the provider layer forward unrelated headers and couple execution to
//! the boundary. [`RuntimeRequest`] carries exactly what execution needs —
//! the parsed body, which endpoint it arrived on, the incoming authorization
//! context (enough for Official `PreserveIncoming` passthrough, never a full
//! header dump), the executor environment that will run the model's tools, and
//! request metadata for diagnostics. The HTTP boundary builds one of these per
//! request; the runtime only consumes the typed value.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::codex_metadata::{CodexCapabilityProfile, CodexIdentityTrust, CodexTurnIdentity};
use crate::environment::ExecutionEnvironment;
use crate::subagent_graph::CodexThreadKey;

/// Which proxy endpoint a request arrived on. Kept separate from the body so
/// a request can never masquerade as a different dialect than the route it
/// actually hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeEndpoint {
    Responses,
    ResponsesStream,
    ChatCompletions,
    Compact,
    Search,
}

/// The authorization context of the *incoming* request. Only the fields that
/// are meaningful to forward are captured — never a full header dump, so the
/// runtime cannot accidentally leak unrelated headers upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IncomingAuthContext {
    /// `authorization` header value, if present. Used verbatim only for
    /// Official `PreserveIncoming` passthrough (native Codex login).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<String>,
    /// `openai-account` header carried by native Codex login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_account: Option<String>,
}

/// Per-request metadata for diagnostics and telemetry. Never carries secrets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestMetadata {
    pub request_id: String,
    pub received_at_ms: u64,
    /// Auto Review run id when this request is a guardian execution (M9);
    /// used for review usage attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_run_id: Option<String>,
    /// `"primary"` or `"fallback"` when this request is one leg of a
    /// guardian dispatch; `None` for every non-guardian request. Threaded
    /// into the [`crate::usage::UsageRecord`] this exchange produces so
    /// review usage stats can tell primary and fallback attempts apart
    /// instead of collapsing them into whichever ran last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_role: Option<String>,
    /// Proxy-internal marker for a request already projected as one Guardian
    /// reviewer leg. This is distinct from `review_role`: inner attempt usage
    /// deliberately leaves attribution unset until the winning leg is known.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub guardian_dispatch: bool,
    /// A short, human-readable reason the primary reviewer was unavailable,
    /// set only on the fallback leg of a guardian dispatch. Deliberately
    /// bounded (a route name plus a status/category, never the raw upstream
    /// error body) so a verbose or adversarial provider response can never
    /// balloon a usage record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_failure_reason: Option<String>,
    /// WebSocket connection identity for this exchange, assigned once per
    /// client socket by the Responses WebSocket bridge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    /// Normalized Codex turn and subagent identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_identity: Option<CodexTurnIdentity>,
    /// Resolved Codex capability profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_capabilities: Option<CodexCapabilityProfile>,
    /// Proxy-private escape hatch used by the WebSocket dispatcher when it
    /// can prove that a `previous_response_id` belongs to the Official
    /// account on the segment being replaced. The shared execute path then
    /// hydrates portable history instead of forwarding that account-scoped
    /// id to the newly selected account.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force_portable_official_handoff: bool,
    /// The managed ChatGPT account this request must bill to, set only on
    /// the legs of a guardian dispatch whose Auto Review policy names a
    /// review-specific account. An account id, never a token -- the same
    /// identifier the accounts list already shows in the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_official_account_id: Option<String>,
}

impl RequestMetadata {
    pub fn codex_thread_key(&self) -> Option<CodexThreadKey> {
        let id = self.codex_identity.as_ref()?;
        if id.trust == CodexIdentityTrust::Conflict {
            return None;
        }
        let session_id = id.session_id.clone()?;
        let thread_id = id.thread_id.clone()?;
        Some(CodexThreadKey::new(session_id, thread_id))
    }

    pub fn has_exact_codex_parent(&self) -> bool {
        let Some(id) = self.codex_identity.as_ref() else {
            return false;
        };
        (id.trust == CodexIdentityTrust::Exact || id.trust == CodexIdentityTrust::Structured)
            && id.thread_id.is_some()
            && id.parent_thread_id.is_some()
            && id.trust != CodexIdentityTrust::Conflict
    }

    pub fn codex_identity_conflicted(&self) -> bool {
        self.codex_identity
            .as_ref()
            .map(|id| id.trust == CodexIdentityTrust::Conflict)
            .unwrap_or(false)
    }
}

/// The full context one request executes against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeRequest {
    /// The parsed request body (already decoded and size-limited at the
    /// boundary).
    pub body: Value,
    pub endpoint: RuntimeEndpoint,
    pub incoming_auth: IncomingAuthContext,
    /// The executor that will actually run this model's tools on this host.
    /// Explicit input, never derived from the proxy container.
    pub execution_environment: ExecutionEnvironment,
    pub metadata: RequestMetadata,
}

/// Walk a request's `input` into a flat list of items (oldest to newest).
pub fn request_input_items(request: &Value) -> Vec<Value> {
    request
        .get("input")
        .cloned()
        .map(input_value_items)
        .unwrap_or_default()
}

/// Normalize one `input` value into items (the cc-switch convention).
pub fn input_value_items(input: Value) -> Vec<Value> {
    match input {
        Value::Array(items) => items,
        Value::Object(object) => vec![Value::Object(object)],
        Value::String(text) => vec![serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": text }],
        })],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_metadata::{CodexIdentitySource, CodexOpaqueId};
    use crate::environment::ExecutionEnvironment;

    #[test]
    fn a_request_can_be_built_and_serialized_for_telemetry() {
        let request = RuntimeRequest {
            body: serde_json::json!({"model": "vlm-grok", "input": "hi"}),
            endpoint: RuntimeEndpoint::Responses,
            incoming_auth: IncomingAuthContext {
                authorization: Some("Bearer x".into()),
                openai_account: None,
            },
            execution_environment: ExecutionEnvironment::posix_reference(),
            metadata: RequestMetadata {
                request_id: "req-1".into(),
                received_at_ms: 1_700_000_000_000,
                review_run_id: None,
                review_role: None,
                guardian_dispatch: false,
                primary_failure_reason: None,
                connection_id: None,
                codex_identity: None,
                codex_capabilities: None,
                force_portable_official_handoff: false,
                review_official_account_id: None,
            },
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["endpoint"], "responses");
        assert_eq!(encoded["incomingAuth"]["authorization"], "Bearer x");
        let decoded: RuntimeRequest = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn request_round_trip_preserves_optional_codex_identity() {
        let identity = CodexTurnIdentity {
            session_id: Some(CodexOpaqueId::new("session_id", "s_123").unwrap()),
            thread_id: Some(CodexOpaqueId::new("thread_id", "t_456").unwrap()),
            parent_thread_id: Some(CodexOpaqueId::new("parent_thread_id", "t_parent").unwrap()),
            source: CodexIdentitySource::TurnMetadataHeader,
            trust: CodexIdentityTrust::Exact,
            ..Default::default()
        };

        let metadata = RequestMetadata {
            request_id: "req-2".into(),
            received_at_ms: 1_700_000_000_000,
            review_run_id: None,
            review_role: None,
            guardian_dispatch: false,
            primary_failure_reason: None,
            connection_id: None,
            codex_identity: Some(identity),
            codex_capabilities: None,
            force_portable_official_handoff: false,
            review_official_account_id: None,
        };

        assert!(metadata.has_exact_codex_parent());
        let key = metadata.codex_thread_key().unwrap();
        assert_eq!(key.session_id.as_str(), "s_123");
        assert_eq!(key.thread_id.as_str(), "t_456");

        let encoded = serde_json::to_value(&metadata).unwrap();
        let decoded: RequestMetadata = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, metadata);
    }

    #[test]
    fn legacy_request_metadata_without_codex_fields_deserializes() {
        let legacy_json = serde_json::json!({
            "requestId": "req-legacy",
            "receivedAtMs": 1700000000000u64,
        });

        let decoded: RequestMetadata = serde_json::from_value(legacy_json).unwrap();
        assert_eq!(decoded.request_id, "req-legacy");
        assert!(decoded.codex_identity.is_none());
        assert!(decoded.codex_capabilities.is_none());
        assert!(decoded.codex_thread_key().is_none());
        assert!(!decoded.has_exact_codex_parent());
    }

    #[test]
    fn empty_auth_context_serializes_without_noise() {
        let context = IncomingAuthContext::default();
        let encoded = serde_json::to_value(&context).unwrap();
        assert!(encoded.get("authorization").is_none());
        assert!(encoded.get("openaiAccount").is_none());
    }
}
