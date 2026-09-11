//! Stable error taxonomy for the shared proxy runtime (plan §25.7).
//!
//! These categories are fixed *before* real upstream execution begins so the
//! HTTP boundary, provider adapters, and diagnostics all speak one vocabulary.
//! Everything that fails during request execution maps through here; the HTTP
//! status for each category is centralized in [`RuntimeError::http_status`]
//! so no handler ever re-decides what a category means on the wire.
//!
//! The categories deliberately mirror the milestone map: continuation,
//! compaction, history, and search failures are distinct so M4/M6/M8 can
//! attach their own handling without widening this type again.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

/// One failure mode of the proxy runtime.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    /// The request itself is malformed (missing `model`, bad body, ...).
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// The model-visible surface a catalog entry advertises contradicts the
    /// resolved harness contract. Message text keeps the historic
    /// `harness profile mismatch: ...` form so Desktop-facing behavior is
    /// unchanged by the M3A move.
    #[error("harness profile mismatch: {0}")]
    HarnessMismatch(String),

    /// The request asks for behavior this runtime does not provide
    /// (e.g. `stream: true` before M5). Must never silently downgrade.
    #[error("unsupported mode: {0}")]
    UnsupportedMode(String),

    /// No route resolves the requested model.
    #[error("route not found: {0}")]
    RouteNotFound(String),

    /// The route exists but is not available for execution.
    #[error("route inactive: {0}")]
    RouteInactive(String),

    /// The route needs a credential that is not provisioned.
    #[error("missing credential: {0}")]
    CredentialMissing(String),

    /// Server-side authentication could not be established.
    #[error("authentication failed: {0}")]
    AuthenticationFailed(String),

    /// The provider rejected the request as unauthorized.
    #[error("provider unauthorized: {0}")]
    ProviderUnauthorized(String),

    /// The provider refused the request due to quota / rate limits.
    #[error("provider quota exceeded: {message}")]
    ProviderQuota {
        message: String,
        retry_after_secs: Option<u64>,
        /// `false` when Vellum answered from a local cooldown without calling
        /// the upstream again.
        upstream_attempted: bool,
    },

    /// The provider is reachable but unavailable.
    #[error("provider unavailable: {0}")]
    ProviderUnavailable(String),

    /// The provider returned something the runtime cannot interpret.
    #[error("provider protocol error: {0}")]
    ProviderProtocol(String),

    /// Continuation (server-side resume / local replay) failed.
    #[error("continuation failed: {0}")]
    Continuation(String),

    /// Compaction failed.
    #[error("compaction failed: {0}")]
    Compaction(String),

    /// Durable history persistence / hydration failed.
    #[error("history error: {0}")]
    History(String),

    /// Web search failed.
    #[error("search error: {0}")]
    Search(String),

    /// A server-side tool loop (currently only the third-party `web_search`
    /// loop) hit its per-turn call cap before the model produced a terminal
    /// response. Distinct from [`Self::Search`] (a single search call
    /// failing) so callers can tell "Brave errored" from "the model kept
    /// calling search forever" apart.
    #[error("tool loop limit exceeded: {0}")]
    ToolLoopLimit(String),

    /// Auto Review / Guardian failed.
    #[error("review error: {0}")]
    Review(String),

    /// The proxy is draining and is not admitting new requests.
    #[error("proxy is draining: {0}")]
    LifecycleDraining(String),

    /// The user stopped the proxy while this request was still in flight.
    /// Distinct from a client disconnect and from any provider failure: the
    /// work was interrupted by Vellum's stop, not by the upstream.
    #[error("user stopped proxy: {0}")]
    ProxyStopped(String),

    /// The request used a `Content-Encoding` other than identity.
    ///
    /// Production inbound never decompresses. This is HTTP 415
    /// (`unsupported_media`), not a generic 400, so a compressed body is
    /// refused before any decoder runs.
    #[error("unsupported media: {0}")]
    UnsupportedMedia(String),

    /// The proxy has no remaining admission capacity (HTTP slots, Official
    /// WebSocket slots, third-party stream slots, or the global buffered-byte
    /// budget).
    ///
    /// Maps to HTTP 429 so callers treat it as retryable. Distinct from
    /// [`Self::ProviderQuota`], which is the *upstream* rate limit. Never
    /// rendered as raw Axum/Tower text.
    #[error("resource exhausted: {0}")]
    ResourceExhausted(String),

    /// Anything that should not happen on a valid request path.
    #[error("internal error: {0}")]
    Internal(String),

    /// Upstream accepted the connection but never opened response headers
    /// within the bounded wait (streaming dispatch only). Kept distinct from
    /// [`Self::ProviderUnavailable`] so a stalled header phase is always
    /// visibly tagged in usage/diagnostics as a pre-header failure, never
    /// confused with a slow-but-arriving body (which never hits this path —
    /// only the header-open await is bounded here) or a refused connection.
    #[error("upstream did not open response headers in time: {0}")]
    StreamOpenTimeout(String),

    /// A Guardian review attempt (primary or fallback) did not produce a
    /// complete, valid assessment within its bounded attempt deadline.
    /// Distinct from [`Self::StreamOpenTimeout`], which only bounds the
    /// header-open phase: this covers *any* point an attempt can stall —
    /// before headers, after headers with no body, mid-body on a partial
    /// delta, or fully buffered but past the deadline — because a Guardian
    /// attempt is not "done" until a valid assessment is in hand, not merely
    /// once bytes start arriving. Eligible for Failover fallback (see
    /// `review_failure_allows_fallback`), unlike a genuine content/protocol
    /// failure on a *completed* attempt.
    #[error("guardian review attempt did not complete in time: {0}")]
    ReviewAttemptTimeout(String),
}

impl RuntimeError {
    /// A short, stable category token (used in diagnostics and the wire
    /// envelope). Never changes across versions; the message text may.
    pub fn category(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_request",
            Self::HarnessMismatch(_) => "harness_mismatch",
            Self::UnsupportedMode(_) => "unsupported_mode",
            Self::RouteNotFound(_) => "route_not_found",
            Self::RouteInactive(_) => "route_inactive",
            Self::CredentialMissing(_) => "credential_missing",
            Self::AuthenticationFailed(_) => "authentication_failed",
            Self::ProviderUnauthorized(_) => "provider_unauthorized",
            Self::ProviderQuota { .. } => "provider_quota",
            Self::ProviderUnavailable(_) => "provider_unavailable",
            Self::ProviderProtocol(_) => "provider_protocol",
            Self::Continuation(_) => "continuation",
            Self::Compaction(_) => "compaction",
            Self::History(_) => "history",
            Self::Search(_) => "search",
            Self::ToolLoopLimit(_) => "tool_loop_limit",
            Self::Review(_) => "review",
            Self::LifecycleDraining(_) => "lifecycle_draining",
            Self::ProxyStopped(_) => "proxy_stopped",
            Self::UnsupportedMedia(_) => "unsupported_media",
            Self::ResourceExhausted(_) => "resource_exhausted",
            Self::Internal(_) => "internal",
            Self::StreamOpenTimeout(_) => "stream_open_timeout",
            Self::ReviewAttemptTimeout(_) => "review_attempt_timeout",
        }
    }

    /// The single HTTP mapping every boundary handler uses. Providers are
    /// never allowed to translate categories themselves.
    pub fn http_status(&self) -> StatusCode {
        match self {
            Self::InvalidRequest(_) | Self::HarnessMismatch(_) => StatusCode::BAD_REQUEST,
            Self::UnsupportedMode(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::UnsupportedMedia(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::RouteNotFound(_) | Self::RouteInactive(_) => StatusCode::NOT_FOUND,
            Self::CredentialMissing(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::AuthenticationFailed(_) | Self::ProviderUnauthorized(_) => {
                StatusCode::UNAUTHORIZED
            }
            Self::ProviderQuota { .. } | Self::ResourceExhausted(_) => {
                StatusCode::TOO_MANY_REQUESTS
            }
            Self::ToolLoopLimit(_) => StatusCode::CONFLICT,
            Self::ProviderUnavailable(_) | Self::LifecycleDraining(_) | Self::ProxyStopped(_) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::ProviderProtocol(_) => StatusCode::BAD_GATEWAY,
            Self::Continuation(_)
            | Self::Compaction(_)
            | Self::History(_)
            | Self::Search(_)
            | Self::Review(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::StreamOpenTimeout(_) | Self::ReviewAttemptTimeout(_) => {
                StatusCode::GATEWAY_TIMEOUT
            }
        }
    }

    /// The error envelope for the wire, in the same family as the boundary
    /// envelope in `body.rs` but carrying the stable category token.
    pub fn to_error_payload(&self) -> Value {
        let mut error = json!({
            "type": "vellum_proxy_error",
            "code": self.http_status().as_u16(),
            "category": self.category(),
            "message": self.to_string(),
        });
        if let Self::ProviderQuota {
            retry_after_secs,
            upstream_attempted,
            ..
        } = self
        {
            if let Some(object) = error.as_object_mut() {
                if let Some(secs) = retry_after_secs {
                    object.insert("retry_after".into(), json!(secs));
                }
                object.insert("upstream_attempted".into(), json!(upstream_attempted));
            }
        }
        json!({ "error": error })
    }

    pub fn provider_quota(
        message: impl Into<String>,
        retry_after_secs: Option<u64>,
        upstream_attempted: bool,
    ) -> Self {
        Self::ProviderQuota {
            message: message.into(),
            retry_after_secs,
            upstream_attempted,
        }
    }

    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::ProviderQuota {
                retry_after_secs, ..
            } => *retry_after_secs,
            _ => None,
        }
    }

    pub fn upstream_attempted(&self) -> Option<bool> {
        match self {
            Self::ProviderQuota {
                upstream_attempted, ..
            } => Some(*upstream_attempted),
            _ => None,
        }
    }

    /// Boundary conversion: an `IntoResponse` so any handler can return
    /// `Err(runtime_error)` and get the correct centralized status + envelope.
    pub fn into_error_response(self) -> Response {
        let status = self.http_status();
        (status, Json(self.to_error_payload())).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_errors() -> Vec<RuntimeError> {
        vec![
            RuntimeError::InvalidRequest("bad".into()),
            RuntimeError::HarnessMismatch("model-visible surface".into()),
            RuntimeError::UnsupportedMode("stream".into()),
            RuntimeError::RouteNotFound("m".into()),
            RuntimeError::RouteInactive("m".into()),
            RuntimeError::CredentialMissing("c".into()),
            RuntimeError::AuthenticationFailed("a".into()),
            RuntimeError::ProviderUnauthorized("u".into()),
            RuntimeError::provider_quota("q", None, true),
            RuntimeError::ProviderUnavailable("x".into()),
            RuntimeError::ProviderProtocol("p".into()),
            RuntimeError::Continuation("c".into()),
            RuntimeError::Compaction("c".into()),
            RuntimeError::History("h".into()),
            RuntimeError::Search("s".into()),
            RuntimeError::ToolLoopLimit("web_search".into()),
            RuntimeError::Review("r".into()),
            RuntimeError::LifecycleDraining("d".into()),
            RuntimeError::ProxyStopped("user stopped proxy".into()),
            RuntimeError::UnsupportedMedia("gzip".into()),
            RuntimeError::ResourceExhausted("http_requests".into()),
            RuntimeError::Internal("i".into()),
            RuntimeError::StreamOpenTimeout("30s".into()),
            RuntimeError::ReviewAttemptTimeout("30s".into()),
        ]
    }

    #[test]
    fn every_category_has_a_stable_token_and_maps_to_a_defined_status() {
        for error in all_errors() {
            let category = error.category();
            assert!(!category.is_empty(), "{error:?}");
            let status = error.http_status();
            assert!(
                status.as_u16() >= 400,
                "category {category} must map to a client/server error, got {status}"
            );
            let payload = error.to_error_payload();
            assert_eq!(payload["error"]["category"], category);
            assert_eq!(payload["error"]["code"], status.as_u16());
        }
    }

    #[test]
    fn categories_that_carry_the_same_http_status_are_still_distinct_types() {
        let internal = RuntimeError::Internal("x".into());
        let credential = RuntimeError::CredentialMissing("x".into());
        assert_eq!(internal.http_status(), credential.http_status());
        assert_ne!(internal.category(), credential.category());
    }

    #[test]
    fn error_messages_are_actions_not_type_names() {
        let error = RuntimeError::RouteNotFound("model `gpt-x` is not configured".into());
        assert!(error.to_string().contains("not configured"));
        assert!(error.to_string().contains("gpt-x"));
    }

    #[tokio::test]
    async fn into_error_response_carries_the_centralized_status_and_envelope() {
        let response =
            RuntimeError::provider_quota("rate limited", Some(12), true).into_error_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(payload["error"]["category"], "provider_quota");
        assert_eq!(payload["error"]["code"], 429);
    }

    #[test]
    fn local_tool_loop_limit_is_not_a_retryable_quota_status() {
        let error = RuntimeError::ToolLoopLimit("bounded finalization exhausted".into());
        assert_eq!(error.http_status(), StatusCode::CONFLICT);
        assert_eq!(
            error.to_error_payload()["error"]["category"],
            "tool_loop_limit"
        );
        assert_eq!(error.to_error_payload()["error"]["code"], 409);
    }
}
