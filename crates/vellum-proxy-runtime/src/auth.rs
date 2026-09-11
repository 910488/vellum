//! Request-level authorization, resolved exactly once per request (M3C).
//!
//! M3A validated a route's auth posture but threw the result away, so the
//! dispatch seam could only re-read live credentials — a request-level TOCTOU
//! window: look up the credential, do adapter work, look up again, and a
//! rotation in between would send one secret to a call the adapter prepared
//! for another. M3C replaces that with [`ResolvedAuth`]: one resolve, one
//! consumed value. The upstream header set is derived from the resolved value,
//! never from a second provider read.
//!
//! `None` is the route's *explicit* "no auth" posture. It is never inferred:
//! a Bearer route that cannot resolve its credential fails closed instead of
//! degrading into an anonymous request.

use serde_json::Value;

use crate::credentials::CredentialProvider;
use crate::error::RuntimeError;
use crate::grok_session::GrokSessionRegistry;
use crate::history::HistoryStore;
use crate::official_auth::{OfficialAuthDecision, OfficialAuthProvider};
use crate::request::RuntimeRequest;
use crate::route::{RuntimeAccessMode, RuntimeAuthKind, RuntimeModelRoute};

/// The authorization posture for one request, resolved once and then consumed
/// by dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedAuth {
    /// The route explicitly needs no auth. No credential header is injected.
    None,
    /// Static secret from the credential store (`authorization: Bearer ..`).
    Bearer(String),
    /// Official native Codex login: forward the incoming request's own
    /// authorization/account values verbatim. `None` means the incoming
    /// request carried none.
    OfficialPreserveIncoming {
        authorization: Option<String>,
        account: Option<String>,
    },
    /// Vellum-managed ChatGPT account (`authorization: Bearer ..` +
    /// `ChatGPT-Account-Id`).
    OfficialManaged {
        token: String,
        account_id: Option<String>,
        selection_revision: Option<u64>,
        selection_verified: bool,
    },
    /// Grok CLI session: bearer token plus the `x-grok-*` header set.
    ///
    /// `session_id`, `request_id`, and `turn_index` are resolved through the
    /// shared [`crate::grok_session::GrokSessionRegistry`]: the session is
    /// stable across a conversation (including across a restart, seeded from
    /// durable history), a retry of the same request body reuses the same
    /// `request_id`/`turn_index` instead of advancing, and `turn_index` is
    /// only `None` for a compaction/auxiliary call (not exercised by this
    /// resolver yet — every current caller is a normal turn).
    GrokSession {
        token: String,
        request_id: String,
        session_id: String,
        turn_index: Option<u32>,
        client_version: Option<String>,
        agent_id: Option<String>,
        user_id: Option<String>,
    },
}

impl ResolvedAuth {
    /// Resolve the authorization posture for `route` against the request
    /// context, exactly once. Callers must use the returned value for the
    /// whole request; never re-query the providers.
    pub async fn resolve(
        route: &RuntimeModelRoute,
        request: &RuntimeRequest,
        credentials: &dyn CredentialProvider,
        official_auth: &dyn OfficialAuthProvider,
        grok_sessions: &GrokSessionRegistry,
        history: Option<&dyn HistoryStore>,
    ) -> Result<Self, RuntimeError> {
        if route.effective_provider_profile().is_some() {
            let mode = route
                .effective_access_mode()
                .unwrap_or(RuntimeAccessMode::Credentialed);
            let stored = match (mode, route.credential_id.as_deref()) {
                (RuntimeAccessMode::Credentialed, Some(id)) => credentials
                    .get_secret(id)
                    .await
                    .map_err(RuntimeError::AuthenticationFailed)?,
                _ => None,
            };
            let token = crate::opencode::authorization_token(mode, stored.as_deref())
                .map_err(RuntimeError::CredentialMissing)?;
            return Ok(Self::Bearer(token));
        }
        match route.auth_kind {
            RuntimeAuthKind::None => Ok(Self::None),
            RuntimeAuthKind::Bearer => {
                let secret = resolve_bearer_secret(route, credentials).await?;
                Ok(Self::Bearer(secret))
            }
            RuntimeAuthKind::ChatGpt => {
                let requested_account = request.metadata.review_official_account_id.as_deref();
                let decision = official_auth
                    .authorize_as(&route.route_id, requested_account)
                    .await
                    .map_err(RuntimeError::AuthenticationFailed)?;
                match decision {
                    OfficialAuthDecision::PreserveIncoming => {
                        // A review that named a billing account cannot fall
                        // back to the incoming request's own authorization:
                        // that is the *user's* account, which is exactly the
                        // one they moved the review off.
                        if let Some(account) = requested_account {
                            return Err(RuntimeError::AuthenticationFailed(format!(
                                "Auto Review is set to bill account {account}, but this host has                                  no managed ChatGPT account for it"
                            )));
                        }
                        Ok(Self::OfficialPreserveIncoming {
                            authorization: request.incoming_auth.authorization.clone(),
                            account: request.incoming_auth.openai_account.clone(),
                        })
                    }
                    OfficialAuthDecision::Managed(auth) => {
                        // A provider that cannot select an account answers
                        // with whichever one it has. Compare rather than
                        // trust: billing a review to an account the user did
                        // not choose is a silent failure, and a silent
                        // billing failure is worse than a loud one.
                        if let Some(account) = requested_account {
                            match auth.account_id.as_deref() {
                                Some(actual) if actual == account => {}
                                Some(actual) => {
                                    return Err(RuntimeError::AuthenticationFailed(format!(
                                        "Auto Review is set to bill account {account}, but this                                          host authorized {actual}"
                                    )))
                                }
                                None => {
                                    return Err(RuntimeError::AuthenticationFailed(format!(
                                        "Auto Review is set to bill account {account}, but the                                          authorization returned no account identity"
                                    )))
                                }
                            }
                        }
                        Ok(Self::OfficialManaged {
                            token: auth.access_token,
                            account_id: auth.account_id,
                            selection_revision: auth.selection_revision,
                            selection_verified: auth.selection_verified,
                        })
                    }
                }
            }
            RuntimeAuthKind::GrokSession => {
                let secret = resolve_bearer_secret(route, credentials).await?;
                let secret = parse_grok_credential(&secret)?;
                let identity =
                    grok_sessions.resolve_turn(&request.body, history, &route.route_id, false)?;
                Ok(Self::GrokSession {
                    token: secret.access_token,
                    request_id: identity.request_id,
                    session_id: identity.session_id,
                    turn_index: identity.turn_index.map(|index| index as u32),
                    client_version: secret.client_version,
                    agent_id: secret.agent_id,
                    user_id: secret.user_id,
                })
            }
        }
    }

    /// Resolve the authorization posture for a Grok Build compaction /
    /// canonical-summarizer auxiliary call: same conversation session as the
    /// route's normal turns, but never a normal turn itself. `identity_seed`
    /// should be the auxiliary call's own outgoing body (not the enclosing
    /// turn's `request.body`) so that several distinct auxiliary calls on one
    /// turn (e.g. one per compaction chunk) get distinct request ids while a
    /// retry of the identical auxiliary body reuses the same one.
    ///
    /// Non-Grok routes have no auxiliary-vs-turn distinction to make, so this
    /// delegates straight to [`Self::resolve`] for every other auth kind.
    pub async fn resolve_for_compaction(
        route: &RuntimeModelRoute,
        request: &RuntimeRequest,
        credentials: &dyn CredentialProvider,
        official_auth: &dyn OfficialAuthProvider,
        grok_sessions: &GrokSessionRegistry,
        history: Option<&dyn HistoryStore>,
        identity_seed: &Value,
    ) -> Result<Self, RuntimeError> {
        match route.auth_kind {
            RuntimeAuthKind::GrokSession => {
                let secret = resolve_bearer_secret(route, credentials).await?;
                let secret = parse_grok_credential(&secret)?;
                let session_id = grok_sessions.resolve_session(&request.body, history)?;
                let identity = grok_sessions.compaction_identity(&session_id, identity_seed)?;
                Ok(Self::GrokSession {
                    token: secret.access_token,
                    request_id: identity.request_id,
                    session_id: identity.session_id,
                    turn_index: identity.turn_index.map(|index| index as u32),
                    client_version: secret.client_version,
                    agent_id: secret.agent_id,
                    user_id: secret.user_id,
                })
            }
            _ => {
                Self::resolve(
                    route,
                    request,
                    credentials,
                    official_auth,
                    grok_sessions,
                    history,
                )
                .await
            }
        }
    }

    /// The upstream headers this posture injects. `upstream_model` is the
    /// model the upstream actually sees (Grok's override header).
    pub fn upstream_headers(&self, upstream_model: &str) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        match self {
            Self::None => {}
            Self::Bearer(secret) => {
                headers.push(("authorization".into(), format!("Bearer {secret}")));
            }
            Self::OfficialPreserveIncoming {
                authorization,
                account,
            } => {
                if let Some(authorization) = authorization {
                    headers.push(("authorization".into(), authorization.clone()));
                }
                if let Some(account) = account {
                    headers.push(("openai-account".into(), account.clone()));
                }
            }
            Self::OfficialManaged {
                token, account_id, ..
            } => {
                headers.push(("authorization".into(), format!("Bearer {token}")));
                if let Some(account_id) = account_id {
                    headers.push(("ChatGPT-Account-Id".into(), account_id.clone()));
                }
            }
            Self::GrokSession {
                token,
                request_id,
                session_id,
                turn_index,
                client_version,
                agent_id,
                user_id,
            } => {
                headers.push(("authorization".into(), format!("Bearer {token}")));
                // The header set mirrors Desktop's GrokCLI dispatch: selecting
                // the authenticated headless sampler rather than leaving the
                // private endpoint to infer a client mode.
                headers.push(("x-xai-token-auth".into(), "xai-grok-cli".into()));
                headers.push((
                    "x-authenticateresponse".into(),
                    "authenticate-response".into(),
                ));
                headers.push(("x-grok-client-mode".into(), "headless".into()));
                headers.push(("x-grok-req-id".into(), request_id.clone()));
                headers.push(("x-grok-model-override".into(), upstream_model.into()));
                headers.push(("x-grok-session-id".into(), session_id.clone()));
                headers.push(("x-grok-conv-id".into(), session_id.clone()));
                headers.push(("x-grok-doom-loop-check".into(), "true".into()));
                headers.push(("x-grok-client-identifier".into(), "grok-shell".into()));
                if let Some(client_version) = client_version {
                    headers.push(("x-grok-client-version".into(), client_version.clone()));
                }
                if let Some(agent_id) = agent_id {
                    headers.push(("x-grok-agent-id".into(), agent_id.clone()));
                }
                if let Some(user_id) = user_id {
                    headers.push(("x-grok-user-id".into(), user_id.clone()));
                }
                if let Some(turn_index) = turn_index {
                    headers.push(("x-grok-turn-idx".into(), turn_index.to_string()));
                }
            }
        }
        headers
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrokCredentialSecret {
    #[serde(alias = "access_token")]
    access_token: String,
    #[serde(default, alias = "client_version")]
    client_version: Option<String>,
    #[serde(default, alias = "agent_id")]
    agent_id: Option<String>,
    #[serde(default, alias = "user_id")]
    user_id: Option<String>,
}

fn parse_grok_credential(secret: &str) -> Result<GrokCredentialSecret, RuntimeError> {
    if !secret.trim_start().starts_with('{') {
        return Ok(GrokCredentialSecret {
            access_token: secret.to_string(),
            client_version: None,
            agent_id: None,
            user_id: None,
        });
    }
    let parsed: GrokCredentialSecret = serde_json::from_str(secret).map_err(|_| {
        RuntimeError::AuthenticationFailed("invalid Grok credential metadata".into())
    })?;
    if parsed.access_token.trim().is_empty() {
        return Err(RuntimeError::AuthenticationFailed(
            "Grok credential access token is empty".into(),
        ));
    }
    Ok(parsed)
}

/// A Bearer-ish route (Bearer / GrokSession) names a credential. Missing
/// credential reference or missing secret is a configuration error that fails
/// closed — it must never become an anonymous upstream request.
async fn resolve_bearer_secret(
    route: &RuntimeModelRoute,
    credentials: &dyn CredentialProvider,
) -> Result<String, RuntimeError> {
    let credential_id = route.credential_id.as_deref().ok_or_else(|| {
        RuntimeError::CredentialMissing(format!(
            "bearer route `{}` has no credential reference",
            route.route_id
        ))
    })?;
    let secret = credentials
        .get_secret(credential_id)
        .await
        .map_err(|message| {
            RuntimeError::Internal(format!("credential lookup failed: {message}"))
        })?;
    secret.ok_or_else(|| {
        RuntimeError::CredentialMissing(format!(
            "credential `{credential_id}` is not provisioned for route `{}`",
            route.route_id
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::MemoryCredentialProvider;
    use crate::environment::{
        ExecutionEnvironment, ExecutorCapability, RuntimeAmpersandSemantics, RuntimePathStyle,
        RuntimePlatform, RuntimeShellKind,
    };
    use crate::request::{IncomingAuthContext, RequestMetadata, RuntimeEndpoint};
    use crate::route::{
        RuntimeCompactionCapabilities, RuntimeProviderKind, RuntimeReasoningCapabilities,
        RuntimeToolCapabilities, RuntimeWireFormat,
    };

    fn request_with_auth(body: serde_json::Value, incoming: IncomingAuthContext) -> RuntimeRequest {
        RuntimeRequest {
            body,
            endpoint: RuntimeEndpoint::Responses,
            incoming_auth: incoming,
            execution_environment: ExecutionEnvironment {
                platform: RuntimePlatform::Linux,
                shell: RuntimeShellKind::Bash,
                shell_version: None,
                supports_and_and: true,
                has_unix_utilities: true,
                path_style: RuntimePathStyle::Posix,
                ampersand_semantics: RuntimeAmpersandSemantics::PosixBackground,
                verified_capabilities: vec![ExecutorCapability::ArgvSafeExec],
            },
            metadata: RequestMetadata {
                request_id: "req-auth".into(),
                received_at_ms: 0,
                review_run_id: None,
                review_role: None,
                primary_failure_reason: None,
                connection_id: None,
                ..Default::default()
            },
        }
    }

    fn route(auth_kind: RuntimeAuthKind, credential_id: Option<String>) -> RuntimeModelRoute {
        RuntimeModelRoute {
            route_id: "route-auth".into(),
            catalog_id: "vlm-auth".into(),
            name: "Auth".into(),
            base_url: "http://127.0.0.1:0/v1".into(),
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            auth_kind,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: false,
            streaming: true,
            reasoning: true,
            vision: false,
            upstream_model: "auth-model".into(),
            context_window: Some(128_000),
            reasoning_capabilities: RuntimeReasoningCapabilities::default(),
            compaction_capabilities: RuntimeCompactionCapabilities::default(),
            compaction_policy: crate::config::RuntimeCompactionPolicy::default(),
            tool_capabilities: RuntimeToolCapabilities::default(),
            credential_id,
            insecure_http_policy: crate::outbound::InsecureHttpPolicy::Deny,
            provider_profile: None,
            access_mode: None,
            chat_capabilities: crate::route::RuntimeChatCapabilities::default(),
        }
    }

    #[tokio::test]
    async fn none_needs_no_credential_and_adds_no_header() {
        let auth = ResolvedAuth::resolve(
            &route(RuntimeAuthKind::None, None),
            &request_with_auth(
                serde_json::json!({"model": "m"}),
                IncomingAuthContext::default(),
            ),
            &MemoryCredentialProvider::new(),
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(auth, ResolvedAuth::None);
        assert!(auth.upstream_headers("m").is_empty());
    }

    #[tokio::test]
    async fn bearer_requires_a_provisioned_secret() {
        let route = route(RuntimeAuthKind::Bearer, Some("key-1".into()));
        let empty = MemoryCredentialProvider::new();
        let error = ResolvedAuth::resolve(
            &route,
            &request_with_auth(
                serde_json::json!({"model": "m"}),
                IncomingAuthContext::default(),
            ),
            &empty,
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, RuntimeError::CredentialMissing(_)));

        let store = MemoryCredentialProvider::new();
        store.insert("key-1", "sk-secret");
        let auth = ResolvedAuth::resolve(
            &route,
            &request_with_auth(
                serde_json::json!({"model": "m"}),
                IncomingAuthContext::default(),
            ),
            &store,
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            auth.upstream_headers("m"),
            vec![("authorization".to_string(), "Bearer sk-secret".to_string())]
        );
    }

    #[tokio::test]
    async fn bearer_without_a_credential_reference_fails_closed() {
        let route = route(RuntimeAuthKind::Bearer, None);
        let error = ResolvedAuth::resolve(
            &route,
            &request_with_auth(
                serde_json::json!({"model": "m"}),
                IncomingAuthContext::default(),
            ),
            &MemoryCredentialProvider::new(),
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, RuntimeError::CredentialMissing(_)));
    }

    struct ManagedOfficial;

    #[async_trait::async_trait]
    impl crate::official_auth::OfficialAuthProvider for ManagedOfficial {
        async fn authorize(
            &self,
            _route_id: &str,
        ) -> Result<crate::official_auth::OfficialAuthDecision, String> {
            Ok(crate::official_auth::OfficialAuthDecision::Managed(
                crate::official_auth::OfficialAuthorization {
                    access_token: "managed-token".into(),
                    account_id: Some("acct-1".into()),
                    selection_revision: Some(0),
                    selection_verified: true,
                },
            ))
        }

        async fn refresh_after_rejection(
            &self,
            _route_id: &str,
            _rejected: &crate::official_auth::OfficialAuthorization,
        ) -> Result<crate::official_auth::OfficialAuthorization, String> {
            Err("unused".into())
        }
    }

    struct PreserveIncomingOfficial;

    #[async_trait::async_trait]
    impl crate::official_auth::OfficialAuthProvider for PreserveIncomingOfficial {
        async fn authorize(
            &self,
            _route_id: &str,
        ) -> Result<crate::official_auth::OfficialAuthDecision, String> {
            Ok(crate::official_auth::OfficialAuthDecision::PreserveIncoming)
        }

        async fn refresh_after_rejection(
            &self,
            _route_id: &str,
            _rejected: &crate::official_auth::OfficialAuthorization,
        ) -> Result<crate::official_auth::OfficialAuthorization, String> {
            Err("unused".into())
        }
    }

    /// A host that really can pick between managed accounts. Only Desktop
    /// can today; this stands in for it.
    struct MultiAccountOfficial;

    #[async_trait::async_trait]
    impl crate::official_auth::OfficialAuthProvider for MultiAccountOfficial {
        async fn authorize(
            &self,
            _route_id: &str,
        ) -> Result<crate::official_auth::OfficialAuthDecision, String> {
            Ok(crate::official_auth::OfficialAuthDecision::Managed(
                crate::official_auth::OfficialAuthorization {
                    access_token: "default-token".into(),
                    account_id: Some("acct-1".into()),
                    selection_revision: Some(0),
                    selection_verified: true,
                },
            ))
        }

        async fn authorize_as(
            &self,
            route_id: &str,
            account_id: Option<&str>,
        ) -> Result<crate::official_auth::OfficialAuthDecision, String> {
            let Some(account_id) = account_id else {
                return self.authorize(route_id).await;
            };
            if account_id != "acct-2" {
                return Err(format!("no such account: {account_id}"));
            }
            Ok(crate::official_auth::OfficialAuthDecision::Managed(
                crate::official_auth::OfficialAuthorization {
                    access_token: "review-token".into(),
                    account_id: Some("acct-2".into()),
                    selection_revision: Some(0),
                    selection_verified: true,
                },
            ))
        }

        async fn refresh_after_rejection(
            &self,
            _route_id: &str,
            _rejected: &crate::official_auth::OfficialAuthorization,
        ) -> Result<crate::official_auth::OfficialAuthorization, String> {
            Err("unused".into())
        }
    }

    fn review_billed_to(account: &str) -> RuntimeRequest {
        let mut request = request_with_auth(
            serde_json::json!({"model": "codex-auto-review"}),
            IncomingAuthContext {
                authorization: Some("Bearer callers-own".into()),
                openai_account: Some("acct-1".into()),
            },
        );
        request.metadata.review_official_account_id = Some(account.into());
        request
    }

    #[tokio::test]
    async fn a_review_is_billed_to_the_account_auto_review_names() {
        let auth = ResolvedAuth::resolve(
            &route(RuntimeAuthKind::ChatGpt, None),
            &review_billed_to("acct-2"),
            &MemoryCredentialProvider::new(),
            &MultiAccountOfficial,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            auth,
            ResolvedAuth::OfficialManaged {
                token: "review-token".into(),
                account_id: Some("acct-2".into()),
                selection_revision: Some(0),
                selection_verified: true,
            }
        );
    }

    /// `ManagedOfficial` never overrides `authorize_as`, so it answers with
    /// the only account it has -- exactly what every single-grant host does.
    /// The review must fail rather than quietly charge that account: a
    /// silent billing failure is the one outcome the user cannot notice.
    #[tokio::test]
    async fn a_review_is_refused_when_the_host_cannot_bill_the_named_account() {
        let error = ResolvedAuth::resolve(
            &route(RuntimeAuthKind::ChatGpt, None),
            &review_billed_to("acct-2"),
            &MemoryCredentialProvider::new(),
            &ManagedOfficial,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("acct-2"), "{message}");
        assert!(message.contains("acct-1"), "{message}");
    }

    /// The account the review was moved *off* is the caller's own, so
    /// preserving the incoming authorization would bill precisely the
    /// account the setting exists to avoid.
    #[tokio::test]
    async fn a_review_that_names_an_account_never_falls_back_to_the_caller() {
        let error = ResolvedAuth::resolve(
            &route(RuntimeAuthKind::ChatGpt, None),
            &review_billed_to("acct-2"),
            &MemoryCredentialProvider::new(),
            &PreserveIncomingOfficial,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("acct-2"), "{error}");
    }

    /// The whole feature is opt-in: a turn that names no account resolves
    /// exactly as it did before the field existed.
    #[tokio::test]
    async fn a_turn_that_names_no_account_is_unchanged() {
        let auth = ResolvedAuth::resolve(
            &route(RuntimeAuthKind::ChatGpt, None),
            &request_with_auth(
                serde_json::json!({"model": "m"}),
                IncomingAuthContext::default(),
            ),
            &MemoryCredentialProvider::new(),
            &MultiAccountOfficial,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            auth,
            ResolvedAuth::OfficialManaged {
                token: "default-token".into(),
                account_id: Some("acct-1".into()),
                selection_revision: Some(0),
                selection_verified: true,
            }
        );
    }

    #[tokio::test]
    async fn official_managed_applies_bearer_and_account_header() {
        let route = route(RuntimeAuthKind::ChatGpt, None);
        let auth = ResolvedAuth::resolve(
            &route,
            &request_with_auth(
                serde_json::json!({"model": "m"}),
                IncomingAuthContext::default(),
            ),
            &MemoryCredentialProvider::new(),
            &ManagedOfficial,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            auth.upstream_headers("m"),
            vec![
                (
                    "authorization".to_string(),
                    "Bearer managed-token".to_string()
                ),
                ("ChatGPT-Account-Id".to_string(), "acct-1".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn official_preserve_incoming_forwards_the_incoming_authorization_verbatim() {
        let route = route(RuntimeAuthKind::ChatGpt, None);
        let incoming = IncomingAuthContext {
            authorization: Some("Bearer codex-login-token".into()),
            openai_account: Some("incoming-account".into()),
        };
        let auth = ResolvedAuth::resolve(
            &route,
            &request_with_auth(serde_json::json!({"model": "m"}), incoming),
            &MemoryCredentialProvider::new(),
            &PreserveIncomingOfficial,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            auth.upstream_headers("m"),
            vec![
                (
                    "authorization".to_string(),
                    "Bearer codex-login-token".to_string()
                ),
                ("openai-account".to_string(), "incoming-account".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn grok_session_applies_the_x_grok_header_set() {
        let route = route(RuntimeAuthKind::GrokSession, Some("grok-key".into()));
        let store = MemoryCredentialProvider::new();
        store.insert("grok-key", "grok-token");
        let auth = ResolvedAuth::resolve(
            &route,
            &request_with_auth(
                serde_json::json!({"model": "m", "session": "sess-1"}),
                IncomingAuthContext::default(),
            ),
            &store,
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap();
        let headers = auth.upstream_headers("grok-model");
        assert_eq!(
            headers[0],
            ("authorization".to_string(), "Bearer grok-token".to_string())
        );
        let named = |name: &str| {
            headers
                .iter()
                .find(|(header, _)| header == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| panic!("missing {name} header"))
        };
        // The session identity is a hash of the raw conversation identifier
        // (`crate::grok_session::conversation_key_from_raw`), matching
        // Desktop's established `conversation_key` convention — never the
        // raw client-supplied value on the wire.
        let expected_session = crate::grok_session::conversation_key_from_raw(Some("sess-1"))
            .expect("non-empty session hashes");
        assert_eq!(named("x-xai-token-auth"), "xai-grok-cli");
        assert_eq!(named("x-grok-client-mode"), "headless");
        assert_eq!(named("x-grok-session-id"), expected_session);
        assert_eq!(named("x-grok-conv-id"), expected_session);
        assert_eq!(named("x-grok-model-override"), "grok-model");
        assert_eq!(named("x-grok-req-id").len(), "vellum-".len() + 32);
        assert_eq!(named("x-grok-turn-idx"), "0");
    }

    #[tokio::test]
    async fn grok_session_turn_index_persists_and_increments_across_calls_on_one_registry() {
        // The registry (not `ResolvedAuth::resolve` itself) is what makes
        // turn_index stable across a conversation: two turns on the same
        // registry and the same session must see 0, then 1 — never both 0
        // (the old M3C stub's stateless behavior).
        let route = route(RuntimeAuthKind::GrokSession, Some("grok-key".into()));
        let store = MemoryCredentialProvider::new();
        store.insert("grok-key", "grok-token");
        let registry = GrokSessionRegistry::new();
        let turn_idx = |auth: &ResolvedAuth| {
            auth.upstream_headers("grok-model")
                .into_iter()
                .find(|(name, _)| name == "x-grok-turn-idx")
                .map(|(_, value)| value)
        };

        let first = ResolvedAuth::resolve(
            &route,
            &request_with_auth(
                serde_json::json!({"model": "m", "session": "sess-2", "input": "one"}),
                IncomingAuthContext::default(),
            ),
            &store,
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &registry,
            None,
        )
        .await
        .unwrap();
        let second = ResolvedAuth::resolve(
            &route,
            &request_with_auth(
                serde_json::json!({"model": "m", "session": "sess-2", "input": "two"}),
                IncomingAuthContext::default(),
            ),
            &store,
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &registry,
            None,
        )
        .await
        .unwrap();
        assert_eq!(turn_idx(&first), Some("0".to_string()));
        assert_eq!(turn_idx(&second), Some("1".to_string()));

        // A retry of the exact same body reuses the same identity instead of
        // advancing the counter again.
        let retry = ResolvedAuth::resolve(
            &route,
            &request_with_auth(
                serde_json::json!({"model": "m", "session": "sess-2", "input": "two"}),
                IncomingAuthContext::default(),
            ),
            &store,
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &registry,
            None,
        )
        .await
        .unwrap();
        assert_eq!(turn_idx(&retry), Some("1".to_string()));
    }

    #[tokio::test]
    async fn grok_structured_credential_carries_authenticated_client_identity() {
        let route = route(RuntimeAuthKind::GrokSession, Some("grok-key".into()));
        let store = MemoryCredentialProvider::new();
        store.insert(
            "grok-key",
            r#"{"accessToken":"grok-token","clientVersion":"0.2.112","agentId":"agent-1","userId":"user-1"}"#,
        );
        let auth = ResolvedAuth::resolve(
            &route,
            &request_with_auth(
                serde_json::json!({"model": "m"}),
                IncomingAuthContext::default(),
            ),
            &store,
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap();
        let headers = auth.upstream_headers("grok-model");
        for (name, expected) in [
            ("x-grok-client-version", "0.2.112"),
            ("x-grok-agent-id", "agent-1"),
            ("x-grok-user-id", "user-1"),
        ] {
            assert_eq!(
                headers.iter().find(|(header, _)| header == name).unwrap().1,
                expected
            );
        }
    }

    #[tokio::test]
    async fn compaction_reuses_the_conversation_session_and_omits_turn_index() {
        let route = route(RuntimeAuthKind::GrokSession, Some("grok-key".into()));
        let store = MemoryCredentialProvider::new();
        store.insert("grok-key", "grok-token");
        let registry = GrokSessionRegistry::new();
        let turn_request = request_with_auth(
            serde_json::json!({"model": "m", "session": "sess-compact", "input": "hello"}),
            IncomingAuthContext::default(),
        );

        // The normal turn establishes the conversation's session identity.
        let turn_auth = ResolvedAuth::resolve(
            &route,
            &turn_request,
            &store,
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &registry,
            None,
        )
        .await
        .unwrap();
        let ResolvedAuth::GrokSession {
            session_id: turn_session,
            ..
        } = &turn_auth
        else {
            panic!("expected GrokSession auth");
        };

        // Compaction on the same conversation must reuse that session, never
        // invent a fresh one, and never carry a turn index.
        let compaction_auth = ResolvedAuth::resolve_for_compaction(
            &route,
            &turn_request,
            &store,
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &registry,
            None,
            &serde_json::json!({"purpose": "compaction-checkpoint"}),
        )
        .await
        .unwrap();
        let ResolvedAuth::GrokSession {
            session_id: compaction_session,
            request_id,
            turn_index,
            ..
        } = &compaction_auth
        else {
            panic!("expected GrokSession auth");
        };
        assert_eq!(compaction_session, turn_session);
        assert_ne!(
            compaction_session, "vellum",
            "must never fall back to a fixed placeholder session"
        );
        assert!(
            turn_index.is_none(),
            "compaction must never carry x-grok-turn-idx"
        );
        assert!(
            request_id.starts_with("xai-compact-"),
            "compaction must use a distinct request identity, got {request_id}"
        );
        assert!(compaction_auth
            .upstream_headers("m")
            .iter()
            .all(|(name, _)| name != "x-grok-turn-idx"));
    }

    #[tokio::test]
    async fn compaction_retry_of_the_same_auxiliary_body_reuses_the_same_request_id() {
        let route = route(RuntimeAuthKind::GrokSession, Some("grok-key".into()));
        let store = MemoryCredentialProvider::new();
        store.insert("grok-key", "grok-token");
        let registry = GrokSessionRegistry::new();
        let turn_request = request_with_auth(
            serde_json::json!({"model": "m", "session": "sess-retry"}),
            IncomingAuthContext::default(),
        );
        let seed = serde_json::json!({"chunk": "same content"});

        let first = ResolvedAuth::resolve_for_compaction(
            &route,
            &turn_request,
            &store,
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &registry,
            None,
            &seed,
        )
        .await
        .unwrap();
        let retry = ResolvedAuth::resolve_for_compaction(
            &route,
            &turn_request,
            &store,
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &registry,
            None,
            &seed,
        )
        .await
        .unwrap();
        let request_id = |auth: &ResolvedAuth| match auth {
            ResolvedAuth::GrokSession { request_id, .. } => request_id.clone(),
            _ => panic!("expected GrokSession auth"),
        };
        assert_eq!(request_id(&first), request_id(&retry));
    }

    #[tokio::test]
    async fn compaction_resolve_delegates_to_resolve_for_non_grok_routes() {
        let route = route(RuntimeAuthKind::Bearer, Some("key-1".into()));
        let store = MemoryCredentialProvider::new();
        store.insert("key-1", "sk-secret");
        let auth = ResolvedAuth::resolve_for_compaction(
            &route,
            &request_with_auth(
                serde_json::json!({"model": "m"}),
                IncomingAuthContext::default(),
            ),
            &store,
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &GrokSessionRegistry::new(),
            None,
            &serde_json::json!({"chunk": 1}),
        )
        .await
        .unwrap();
        assert_eq!(
            auth.upstream_headers("m"),
            vec![("authorization".to_string(), "Bearer sk-secret".to_string())]
        );
    }

    #[tokio::test]
    async fn grok_session_fails_closed_without_a_provisioned_secret() {
        let route = route(RuntimeAuthKind::GrokSession, Some("grok-key".into()));
        let error = ResolvedAuth::resolve(
            &route,
            &request_with_auth(
                serde_json::json!({"model": "m"}),
                IncomingAuthContext::default(),
            ),
            &MemoryCredentialProvider::new(),
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, RuntimeError::CredentialMissing(_)));
    }

    #[tokio::test]
    async fn opencode_zen_free_models_use_the_public_token_even_when_a_key_is_stored() {
        let mut zen = route(RuntimeAuthKind::Bearer, Some("zen-key".into()));
        zen.base_url = crate::opencode::OPENCODE_ZEN_BASE_URL.into();
        zen.upstream_model = "mimo-v2.5-free".into();
        zen.provider_profile = Some(crate::route::RuntimeProviderProfile::OpenCodeZen);
        zen.access_mode = Some(RuntimeAccessMode::AnonymousFree);
        let store = MemoryCredentialProvider::new();
        store.insert("zen-key", "sk-private");
        let auth = ResolvedAuth::resolve(
            &zen,
            &request_with_auth(
                serde_json::json!({"model": "mimo-v2.5-free"}),
                IncomingAuthContext::default(),
            ),
            &store,
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            auth.upstream_headers("mimo-v2.5-free"),
            vec![("authorization".to_string(), "Bearer public".to_string())]
        );
    }

    #[tokio::test]
    async fn opencode_paid_and_go_models_use_the_stored_api_key() {
        let mut paid = route(RuntimeAuthKind::Bearer, Some("zen-key".into()));
        paid.base_url = crate::opencode::OPENCODE_ZEN_BASE_URL.into();
        paid.upstream_model = "glm-5.2".into();
        paid.provider_profile = Some(crate::route::RuntimeProviderProfile::OpenCodeZen);
        paid.access_mode = Some(RuntimeAccessMode::Credentialed);
        let store = MemoryCredentialProvider::new();
        store.insert("zen-key", "sk-private");
        let auth = ResolvedAuth::resolve(
            &paid,
            &request_with_auth(
                serde_json::json!({"model": "glm-5.2"}),
                IncomingAuthContext::default(),
            ),
            &store,
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            auth.upstream_headers("glm-5.2"),
            vec![("authorization".to_string(), "Bearer sk-private".to_string())]
        );

        let mut go = route(RuntimeAuthKind::Bearer, Some("go-key".into()));
        go.base_url = crate::opencode::OPENCODE_GO_BASE_URL.into();
        go.upstream_model = "mimo-v2.5".into();
        go.provider_profile = Some(crate::route::RuntimeProviderProfile::OpenCodeGo);
        go.access_mode = Some(RuntimeAccessMode::Credentialed);
        store.insert("go-key", "sk-go");
        let auth = ResolvedAuth::resolve(
            &go,
            &request_with_auth(
                serde_json::json!({"model": "mimo-v2.5"}),
                IncomingAuthContext::default(),
            ),
            &store,
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            auth.upstream_headers("mimo-v2.5"),
            vec![("authorization".to_string(), "Bearer sk-go".to_string())]
        );
    }

    #[tokio::test]
    async fn unknown_zen_models_are_not_guessed_free_from_a_suffix() {
        let mut unknown = route(RuntimeAuthKind::Bearer, Some("zen-key".into()));
        unknown.base_url = crate::opencode::OPENCODE_ZEN_BASE_URL.into();
        unknown.upstream_model = "mystery-free".into();
        let store = MemoryCredentialProvider::new();
        store.insert("zen-key", "sk-private");
        let auth = ResolvedAuth::resolve(
            &unknown,
            &request_with_auth(
                serde_json::json!({"model": "mystery-free"}),
                IncomingAuthContext::default(),
            ),
            &store,
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            auth.upstream_headers("mystery-free"),
            vec![("authorization".to_string(), "Bearer sk-private".to_string())]
        );
    }

    #[tokio::test]
    async fn non_opencode_openai_compatible_routes_keep_bearer_auth() {
        let route = route(RuntimeAuthKind::Bearer, Some("key-1".into()));
        let store = MemoryCredentialProvider::new();
        store.insert("key-1", "sk-secret");
        let auth = ResolvedAuth::resolve(
            &route,
            &request_with_auth(
                serde_json::json!({"model": "m"}),
                IncomingAuthContext::default(),
            ),
            &store,
            &crate::official_auth::UnconfiguredOfficialAuthProvider,
            &GrokSessionRegistry::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            auth.upstream_headers("m"),
            vec![("authorization".to_string(), "Bearer sk-secret".to_string())]
        );
    }
}
