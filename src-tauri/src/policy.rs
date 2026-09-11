//! Who owns compaction for a request, and which pipeline the local proxy runs.
//!
//! Vellum has no compaction strategy of its own any more. Compaction belongs to
//! whichever Codex runtime executes the thread: Official Codex compacts the
//! official path natively, and Enhanced Codex owns local compaction and context
//! recovery for third-party Providers. The local proxy is a gateway on that
//! path — it injects auth, maps endpoints, translates Responses/Chat, normalizes
//! SSE, maps provider errors and accounts usage, and nothing else.
//!
//! [`CompactionStrategy::VellumCanonical`] survives for exactly one caller: the
//! eval harness, which has to reproduce historical Canonical V1/V2 baselines to
//! compare against. It is never reachable from a product default — see
//! [`built_in_default`] — and `AppState` only accepts an override in eval mode.
//!
//! Pure resolution only, no I/O.

use crate::model::{ProviderKind, Route};
use serde::{Deserialize, Serialize};

/// How context should be compacted for a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompactionStrategy {
    #[default]
    ProviderNative,
    VellumCanonical {
        #[serde(default)]
        compactor: CompactorSelector,
    },
    Disabled,
}

/// Which model performs Vellum Canonical compaction (eval baselines only).
///
/// The global/explicit compactor selection went out with the settings surface
/// that offered it; eval only ever reproduces a baseline against the session's
/// own model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompactorSelector {
    #[default]
    SessionModel,
}

/// Whether continuity retains provider-private reasoning or only portable semantics.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ContinuityKind {
    #[default]
    Native,
    Semantic,
}

impl ContinuityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Semantic => "semantic",
        }
    }
}

/// Mutually exclusive request handling pipelines (issue #4).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestPipeline {
    #[serde(rename = "open_ai_official_native")]
    OpenAiOfficialNative,
    #[serde(rename = "grok_native")]
    GrokNative,
    #[serde(rename = "vellum_canonical")]
    VellumCanonical,
    /// Third-party traffic that Vellum only gateways. Whichever Codex runtime
    /// is executing the thread owns its own compaction.
    #[serde(rename = "third_party_gateway")]
    ThirdPartyGateway,
    #[serde(rename = "review")]
    Review,
}

/// Session-level override for compaction behavior.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionCompactionPolicy {
    pub strategy: CompactionStrategy,
    /// Exact upstream effort value. None omits the field and lets the provider
    /// choose its default.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub threshold_percent: Option<u32>,
    pub output_reserve_tokens: Option<u64>,
    pub tool_reserve_tokens: Option<u64>,
    /// When true, after compaction exhausts retries, allow emergency truncation
    /// with explicit telemetry (never silent on the happy path).
    #[serde(default)]
    pub allow_emergency_truncation: bool,
}

/// Inputs that influence policy resolution.
#[derive(Debug, Clone, Default)]
pub struct PolicyResolutionInput {
    /// Eval-only per-route override. `AppState` refuses to store one outside
    /// eval mode, so in production this is always `None` and resolution falls
    /// straight through to [`built_in_default`].
    pub route_default: Option<SessionCompactionPolicy>,
    /// True when this request is an isolated Auto Review execution.
    pub is_review: bool,
}

/// Fully resolved policy for a single request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedCompactionPolicy {
    pub strategy: CompactionStrategy,
    pub pipeline: RequestPipeline,
    pub continuity_kind: ContinuityKind,
    pub threshold_percent: u32,
    pub output_reserve_tokens: u64,
    pub tool_reserve_tokens: u64,
    pub allow_emergency_truncation: bool,
    pub resolved_reasoning_effort: Option<String>,
    pub reasoning_effort_source: String,
    #[serde(default)]
    pub reasoning_effort_fallback_reason: Option<String>,
    /// Resolved route/model that should run Canonical compaction, if applicable.
    pub resolved_compactor_route_id: Option<String>,
    pub resolved_compactor_model: Option<String>,
    /// Human-readable reason when the requested strategy was coerced.
    pub coercion_reason: Option<String>,
}

/// Safe defaults from the issue #4 matrix.
pub const DEFAULT_CANONICAL_THRESHOLD_PERCENT: u32 = 85;
pub const DEFAULT_OUTPUT_RESERVE_TOKENS: u64 = 4_096;
pub const DEFAULT_TOOL_RESERVE_TOKENS: u64 = 8_192;
/// Grok Build `[session] auto_compact_threshold_percent = 85`.
pub const GROK_NATIVE_AUTO_COMPACT_THRESHOLD_PERCENT: u32 = 85;
/// Grok Build `[features] two_pass_compaction = false` default.
pub const GROK_NATIVE_TWO_PASS_DEFAULT: bool = false;

fn built_in_default(provider: ProviderKind) -> SessionCompactionPolicy {
    match provider {
        ProviderKind::Official => SessionCompactionPolicy {
            strategy: CompactionStrategy::ProviderNative,
            reasoning_effort: None,
            threshold_percent: None,
            output_reserve_tokens: None,
            tool_reserve_tokens: None,
            allow_emergency_truncation: false,
        },
        // Third-party Providers are gatewayed, not compacted. Whichever Codex
        // runtime executes the thread does that itself; Vellum adding a second
        // compactor on the same context is what this removal is undoing.
        ProviderKind::GrokCli | ProviderKind::OpenAiCompatible => SessionCompactionPolicy {
            strategy: CompactionStrategy::Disabled,
            reasoning_effort: None,
            threshold_percent: None,
            output_reserve_tokens: None,
            tool_reserve_tokens: None,
            allow_emergency_truncation: false,
        },
    }
}

/// Eval override first, otherwise the built-in default for the provider.
fn pick_raw_policy(
    provider: ProviderKind,
    input: &PolicyResolutionInput,
) -> (SessionCompactionPolicy, &'static str) {
    if let Some(policy) = input.route_default.clone() {
        return (policy, "route_default");
    }
    (built_in_default(provider), "built_in_default")
}

/// Coerce a requested strategy to what the provider execution plane allows.
///
/// Only eval requests anything other than the built-in default, but Official
/// stays locked either way: its compaction is the official runtime own, and
/// nothing Vellum-side may replace or disable it.
pub fn validate_strategy_for_provider(
    provider: ProviderKind,
    strategy: CompactionStrategy,
) -> (CompactionStrategy, Option<String>) {
    match provider {
        ProviderKind::Official => match strategy {
            CompactionStrategy::ProviderNative => (strategy, None),
            CompactionStrategy::Disabled => (
                CompactionStrategy::ProviderNative,
                Some(
                    "OpenAI Official cannot disable provider-native compaction; coerced to ProviderNative"
                        .into(),
                ),
            ),
            CompactionStrategy::VellumCanonical { .. } => (
                CompactionStrategy::ProviderNative,
                Some(
                    "OpenAI Official is locked to ProviderNative; Vellum Canonical override rejected"
                        .into(),
                ),
            ),
        },
        ProviderKind::GrokCli => match strategy {
            CompactionStrategy::ProviderNative
            | CompactionStrategy::Disabled
            | CompactionStrategy::VellumCanonical { .. } => (strategy, None),
        },
        ProviderKind::OpenAiCompatible => match strategy {
            CompactionStrategy::ProviderNative => (
                CompactionStrategy::Disabled,
                Some(
                    "no third-party Provider exposes a compaction endpoint Vellum may drive; the executing Codex runtime compacts instead"
                        .into(),
                ),
            ),
            CompactionStrategy::Disabled
            | CompactionStrategy::VellumCanonical { .. } => (strategy, None),
        },
    }
}

fn resolve_compactor_endpoint(
    selector: &CompactorSelector,
    session_route_id: &str,
    session_model: &str,
) -> (Option<String>, Option<String>, Option<String>) {
    match selector {
        CompactorSelector::SessionModel => (
            Some(session_route_id.to_string()),
            Some(session_model.to_string()),
            None,
        ),
    }
}

/// Resolve the full compaction policy for a session/route request.
pub fn resolve_session_policy(
    provider: ProviderKind,
    session_route_id: &str,
    session_model: &str,
    input: &PolicyResolutionInput,
) -> ResolvedCompactionPolicy {
    if input.is_review {
        let (raw, _) = pick_raw_policy(provider, input);
        let threshold = raw
            .threshold_percent
            .unwrap_or(DEFAULT_CANONICAL_THRESHOLD_PERCENT)
            .clamp(1, 100);
        let (route, model, _) = match &raw.strategy {
            CompactionStrategy::VellumCanonical { compactor } => {
                resolve_compactor_endpoint(compactor, session_route_id, session_model)
            }
            _ => (
                Some(session_route_id.to_string()),
                Some(session_model.to_string()),
                None,
            ),
        };
        return ResolvedCompactionPolicy {
            strategy: CompactionStrategy::VellumCanonical {
                compactor: CompactorSelector::SessionModel,
            },
            pipeline: RequestPipeline::Review,
            continuity_kind: ContinuityKind::Semantic,
            threshold_percent: threshold,
            output_reserve_tokens: raw
                .output_reserve_tokens
                .unwrap_or(DEFAULT_OUTPUT_RESERVE_TOKENS),
            tool_reserve_tokens: raw
                .tool_reserve_tokens
                .unwrap_or(DEFAULT_TOOL_RESERVE_TOKENS),
            allow_emergency_truncation: raw.allow_emergency_truncation,
            resolved_reasoning_effort: raw.reasoning_effort.clone(),
            reasoning_effort_source: if raw.reasoning_effort.is_some() {
                "policy".into()
            } else {
                "provider_default".into()
            },
            reasoning_effort_fallback_reason: None,
            resolved_compactor_route_id: route,
            resolved_compactor_model: model,
            coercion_reason: Some(
                "Auto Review always runs on an isolated Review pipeline with semantic continuity"
                    .into(),
            ),
        };
    }

    let (raw, _layer) = pick_raw_policy(provider, input);
    let (strategy, coercion) = validate_strategy_for_provider(provider, raw.strategy.clone());
    let threshold = raw
        .threshold_percent
        .or(match provider {
            ProviderKind::GrokCli => Some(GROK_NATIVE_AUTO_COMPACT_THRESHOLD_PERCENT),
            ProviderKind::OpenAiCompatible => Some(DEFAULT_CANONICAL_THRESHOLD_PERCENT),
            ProviderKind::Official => None,
        })
        .unwrap_or(DEFAULT_CANONICAL_THRESHOLD_PERCENT)
        .clamp(1, 100);

    let (pipeline, continuity_kind, compactor_route, compactor_model, extra_reason) =
        match (&strategy, provider) {
            (CompactionStrategy::ProviderNative, ProviderKind::Official) => (
                RequestPipeline::OpenAiOfficialNative,
                ContinuityKind::Native,
                None,
                None,
                None,
            ),
            // Grok Build performs native compaction as an auxiliary inference
            // turn on its normal endpoint, then installs a local successor
            // window. It does not expose an OpenAI-style compact endpoint.
            (CompactionStrategy::ProviderNative, ProviderKind::GrokCli) => (
                RequestPipeline::GrokNative,
                ContinuityKind::Native,
                None,
                None,
                None,
            ),
            (CompactionStrategy::ProviderNative, ProviderKind::OpenAiCompatible) => {
                // validate_strategy should have coerced this already.
                (
                    RequestPipeline::ThirdPartyGateway,
                    ContinuityKind::Native,
                    None,
                    None,
                    None,
                )
            }
            (CompactionStrategy::VellumCanonical { compactor }, _) => {
                let (route, model, reason) =
                    resolve_compactor_endpoint(compactor, session_route_id, session_model);
                (
                    RequestPipeline::VellumCanonical,
                    ContinuityKind::Semantic,
                    route,
                    model,
                    reason,
                )
            }
            // The default for every third-party Provider: Vellum gateways the
            // request and the executing Codex runtime owns compaction.
            (CompactionStrategy::Disabled, ProviderKind::GrokCli)
            | (CompactionStrategy::Disabled, ProviderKind::OpenAiCompatible) => (
                RequestPipeline::ThirdPartyGateway,
                ContinuityKind::Native,
                None,
                None,
                None,
            ),
            (CompactionStrategy::Disabled, ProviderKind::Official) => (
                RequestPipeline::OpenAiOfficialNative,
                ContinuityKind::Native,
                None,
                None,
                Some("Official compaction cannot be disabled".into()),
            ),
        };

    let mut coercion_reason = coercion;
    if let Some(extra) = extra_reason {
        coercion_reason = Some(match coercion_reason {
            Some(existing) => format!("{existing}; {extra}"),
            None => extra,
        });
    }

    ResolvedCompactionPolicy {
        strategy,
        pipeline,
        continuity_kind,
        threshold_percent: threshold,
        output_reserve_tokens: raw
            .output_reserve_tokens
            .unwrap_or(DEFAULT_OUTPUT_RESERVE_TOKENS),
        tool_reserve_tokens: raw
            .tool_reserve_tokens
            .unwrap_or(DEFAULT_TOOL_RESERVE_TOKENS),
        allow_emergency_truncation: raw.allow_emergency_truncation,
        resolved_reasoning_effort: if provider == ProviderKind::Official {
            None
        } else {
            raw.reasoning_effort.clone()
        },
        reasoning_effort_source: if provider == ProviderKind::Official {
            "official_passthrough".into()
        } else if raw.reasoning_effort.is_some() {
            "policy".into()
        } else {
            "provider_default".into()
        },
        reasoning_effort_fallback_reason: None,
        resolved_compactor_route_id: compactor_route,
        resolved_compactor_model: compactor_model,
        coercion_reason,
    }
}

/// Convenience for route-based resolution without a full session object.
pub fn resolve_route_policy(
    route: &Route,
    upstream_model: &str,
    input: &PolicyResolutionInput,
) -> ResolvedCompactionPolicy {
    resolve_session_policy(route.provider_kind, &route.id, upstream_model, input)
}

/// Grok Native auto-compact decision using Grok Build threshold semantics.
pub fn should_compact_grok_native(
    active_context_tokens: u64,
    pending_tokens: u64,
    window_tokens: u64,
    threshold_percent: u32,
    compaction_disabled: bool,
) -> bool {
    if compaction_disabled || window_tokens == 0 {
        return false;
    }
    let used = active_context_tokens.saturating_add(pending_tokens);
    let threshold = window_tokens.saturating_mul(threshold_percent as u64) / 100;
    used >= threshold
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AuthKind, WireFormat};

    fn route(provider: ProviderKind, id: &str, model: &str) -> Route {
        Route {
            id: id.into(),
            name: id.into(),
            base_url: "https://example.test".into(),
            model: model.into(),
            wire: WireFormat::Responses,
            is_current: true,
            server_side_resume: provider == ProviderKind::Official,
            streaming: true,
            reasoning: true,
            provider_kind: provider,
            auth_kind: match provider {
                ProviderKind::Official => AuthKind::ChatGpt,
                ProviderKind::GrokCli => AuthKind::GrokSession,
                ProviderKind::OpenAiCompatible => AuthKind::Bearer,
            },
            enabled: true,
            models: vec![model.into()],
            selected_models: None,
            context_window: Some(200_000),
            model_capabilities: Vec::new(),
            insecure_http_policy: Default::default(),
            catalog_scope: Default::default(),
        }
    }

    #[test]
    fn official_stays_provider_native() {
        let resolved = resolve_route_policy(
            &route(ProviderKind::Official, "official", "gpt-5.6"),
            "gpt-5.6",
            &PolicyResolutionInput::default(),
        );
        assert_eq!(resolved.strategy, CompactionStrategy::ProviderNative);
        assert_eq!(resolved.pipeline, RequestPipeline::OpenAiOfficialNative);
        assert_eq!(resolved.continuity_kind, ContinuityKind::Native);
        assert_eq!(resolved.resolved_compactor_route_id, None);
    }

    /// Official compaction belongs to the official runtime. Neither an eval
    /// override nor anything else may replace it or switch it off.
    #[test]
    fn official_refuses_canonical_and_refuses_to_be_disabled() {
        for requested in [
            CompactionStrategy::VellumCanonical {
                compactor: CompactorSelector::SessionModel,
            },
            CompactionStrategy::Disabled,
        ] {
            let input = PolicyResolutionInput {
                route_default: Some(SessionCompactionPolicy {
                    strategy: requested,
                    ..Default::default()
                }),
                ..Default::default()
            };
            let resolved = resolve_route_policy(
                &route(ProviderKind::Official, "official", "gpt-5.6"),
                "gpt-5.6",
                &input,
            );
            assert_eq!(resolved.strategy, CompactionStrategy::ProviderNative);
            assert_eq!(resolved.pipeline, RequestPipeline::OpenAiOfficialNative);
            assert!(resolved.coercion_reason.is_some());
        }
    }

    /// The whole point of the removal: no third-party Provider resolves to a
    /// Vellum compactor by default. Vellum gateways the request and whichever
    /// Codex runtime executes the thread compacts it.
    #[test]
    fn third_party_defaults_to_gateway_never_canonical() {
        for provider in [ProviderKind::GrokCli, ProviderKind::OpenAiCompatible] {
            let resolved = resolve_route_policy(
                &route(provider, "third-party", "some-model"),
                "some-model",
                &PolicyResolutionInput::default(),
            );
            assert_eq!(
                resolved.strategy,
                CompactionStrategy::Disabled,
                "{provider:?} must not default to a Vellum compaction strategy"
            );
            assert_eq!(resolved.pipeline, RequestPipeline::ThirdPartyGateway);
            assert_eq!(resolved.continuity_kind, ContinuityKind::Native);
            assert_eq!(resolved.resolved_compactor_route_id, None);
            assert_eq!(resolved.resolved_compactor_model, None);
        }
    }

    /// No third-party Provider exposes a compaction endpoint Vellum could
    /// drive, so ProviderNative can no longer be answered with Canonical —
    /// it collapses to the same gateway as the default.
    #[test]
    fn third_party_provider_native_collapses_to_the_gateway() {
        let input = PolicyResolutionInput {
            route_default: Some(SessionCompactionPolicy {
                strategy: CompactionStrategy::ProviderNative,
                ..Default::default()
            }),
            ..Default::default()
        };
        let resolved = resolve_route_policy(
            &route(ProviderKind::OpenAiCompatible, "third-party", "some-model"),
            "some-model",
            &input,
        );
        assert_eq!(resolved.strategy, CompactionStrategy::Disabled);
        assert_eq!(resolved.pipeline, RequestPipeline::ThirdPartyGateway);
        assert!(resolved.coercion_reason.is_some());
    }

    /// Canonical is reachable only when a caller asks for it by name, which
    /// `AppState` allows in eval mode alone. Baselines have to stay
    /// reproducible; product defaults must not be able to get here.
    #[test]
    fn canonical_survives_only_as_an_explicit_eval_baseline() {
        let input = PolicyResolutionInput {
            route_default: Some(SessionCompactionPolicy {
                strategy: CompactionStrategy::VellumCanonical {
                    compactor: CompactorSelector::SessionModel,
                },
                threshold_percent: Some(70),
                ..Default::default()
            }),
            ..Default::default()
        };
        let resolved = resolve_route_policy(
            &route(ProviderKind::OpenAiCompatible, "third-party", "some-model"),
            "some-model",
            &input,
        );
        assert_eq!(resolved.pipeline, RequestPipeline::VellumCanonical);
        assert_eq!(resolved.continuity_kind, ContinuityKind::Semantic);
        assert_eq!(resolved.threshold_percent, 70);
        assert_eq!(
            resolved.resolved_compactor_route_id.as_deref(),
            Some("third-party")
        );
    }

    #[test]
    fn review_pipeline_is_isolated_semantic() {
        let input = PolicyResolutionInput {
            is_review: true,
            ..Default::default()
        };
        let resolved = resolve_route_policy(
            &route(ProviderKind::Official, "official", "gpt-5.6"),
            "gpt-5.6",
            &input,
        );
        assert_eq!(resolved.pipeline, RequestPipeline::Review);
        assert_eq!(resolved.continuity_kind, ContinuityKind::Semantic);
    }

    /// The public control plane is gone and must not grow back. A command
    /// that lets an install pick a compaction strategy again puts a second
    /// compactor on a context the Codex runtime is already compacting.
    #[test]
    fn no_compaction_policy_command_is_registered() {
        let lib_rs = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"),
        )
        .expect("lib.rs is readable");
        for command in [
            "get_session_compaction_policy",
            "set_session_compaction_policy",
            "get_route_compaction_policy",
            "set_route_compaction_policy",
            "get_global_compaction_policy",
            "set_global_compaction_policy",
            "get_global_compactor",
            "set_global_compactor",
            "resolve_compaction_policy",
            "resolve_auto_compact_token_limit",
            "run_compaction",
            "restore_latest_compaction",
        ] {
            assert!(
                !lib_rs.contains(command),
                "`{command}` is registered again; compaction is the executing                  Codex runtime's, not a Vellum setting"
            );
        }
    }

    #[test]
    fn grok_native_threshold_matches_build_default() {
        assert!(should_compact_grok_native(
            85_000,
            0,
            100_000,
            GROK_NATIVE_AUTO_COMPACT_THRESHOLD_PERCENT,
            false
        ));
        assert!(!should_compact_grok_native(
            85_000,
            0,
            100_000,
            GROK_NATIVE_AUTO_COMPACT_THRESHOLD_PERCENT,
            true
        ));
    }
}
