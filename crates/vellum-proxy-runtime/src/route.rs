//! Route/model contracts for the shared proxy runtime (plan §4.1).
//!
//! `ModelRouteView` (see `state.rs`) is deliberately thin — it only carries
//! what the M0 scaffold's mock response path needs. Executing the real
//! production pipeline needs the same route shape Desktop's `Route`/
//! `ModelRoute` pair carries (provider kind, wire format, reasoning/
//! compaction/tool capabilities, credential reference). These types define
//! that shape inside the runtime crate so `vellum-proxy-runtime` never has to
//! depend on the Desktop crate to describe a route.
//!
//! No caller wires these to Desktop's real route store yet — that mapping is
//! extraction work for later milestones. This module only fixes the contract
//! so downstream extraction has a stable target.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeWireFormat {
    #[default]
    Responses,
    Chat,
}

impl RuntimeWireFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeWireFormat::Responses => "responses",
            RuntimeWireFormat::Chat => "chat",
        }
    }
}

/// Grok seed, stored routes, and model capabilities are Responses-only.
/// Catalog refresh and probe results must not project Chat back onto a Grok
/// route. Other providers keep the declared wire (OpenCode mixed catalogs
/// still resolve per-model capability at the Desktop catalog layer).
pub fn projected_wire(
    provider_kind: RuntimeProviderKind,
    wire: RuntimeWireFormat,
) -> RuntimeWireFormat {
    if provider_kind == RuntimeProviderKind::GrokCli {
        RuntimeWireFormat::Responses
    } else {
        wire
    }
}

/// Rewrite every Grok Chat setting to Responses. Returns true when a value
/// changed so the caller can persist atomically.
pub fn rewrite_grok_chat_routes<T>(routes: &mut [T]) -> bool
where
    T: GrokWireRoute,
{
    let mut changed = false;
    for route in routes {
        if route.provider_kind() != RuntimeProviderKind::GrokCli {
            continue;
        }
        if route.wire() != RuntimeWireFormat::Responses {
            route.set_wire(RuntimeWireFormat::Responses);
            changed = true;
        }
        if route.rewrite_grok_capabilities() {
            changed = true;
        }
    }
    changed
}

pub trait GrokWireRoute {
    fn provider_kind(&self) -> RuntimeProviderKind;
    fn wire(&self) -> RuntimeWireFormat;
    fn set_wire(&mut self, wire: RuntimeWireFormat);
    fn rewrite_grok_capabilities(&mut self) -> bool {
        false
    }
}

impl GrokWireRoute for crate::config::RuntimeRouteConfig {
    fn provider_kind(&self) -> RuntimeProviderKind {
        self.provider_kind
    }
    fn wire(&self) -> RuntimeWireFormat {
        self.wire
    }
    fn set_wire(&mut self, wire: RuntimeWireFormat) {
        self.wire = wire;
    }
}

impl GrokWireRoute for RuntimeModelRoute {
    fn provider_kind(&self) -> RuntimeProviderKind {
        self.provider_kind
    }
    fn wire(&self) -> RuntimeWireFormat {
        self.wire
    }
    fn set_wire(&mut self, wire: RuntimeWireFormat) {
        self.wire = wire;
    }
}

/// Remaining Grok Chat routes after projection. Empty means readiness may
/// proceed; a leftover Chat route is a silent-start failure.
pub fn grok_chat_route_ids<T: GrokWireRoute>(routes: &[T]) -> Vec<String> {
    routes
        .iter()
        .filter(|route| {
            route.provider_kind() == RuntimeProviderKind::GrokCli
                && route.wire() != RuntimeWireFormat::Responses
        })
        .map(|route| route.wire().as_str().to_string())
        .collect()
}

pub fn grok_responses_readiness<T: GrokWireRoute>(routes: &[T]) -> Result<(), String> {
    let leftover = routes
        .iter()
        .filter(|route| {
            route.provider_kind() == RuntimeProviderKind::GrokCli
                && route.wire() != RuntimeWireFormat::Responses
        })
        .count();
    if leftover == 0 {
        Ok(())
    } else {
        Err(format!(
            "Grok routes must use the Responses wire; {leftover} Chat setting(s) remain"
        ))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeProviderKind {
    Official,
    #[default]
    OpenAiCompatible,
    GrokCli,
}

impl RuntimeProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeProviderKind::Official => "official",
            RuntimeProviderKind::OpenAiCompatible => "openAiCompatible",
            RuntimeProviderKind::GrokCli => "grokCli",
        }
    }
}

/// Compatibility profile layered on an OpenAI-compatible route. Inferred only
/// for the exact OpenCode Zen / Go catalog roots; other providers stay `None`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeProviderProfile {
    OpenCodeZen,
    OpenCodeGo,
}

impl RuntimeProviderProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenCodeZen => "openCodeZen",
            Self::OpenCodeGo => "openCodeGo",
        }
    }
}

/// How an OpenCode model authenticates. Persisted per model; unknown models
/// stay credentialed and are never guessed free from a name suffix.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeAccessMode {
    AnonymousFree,
    Credentialed,
}

impl RuntimeAccessMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnonymousFree => "anonymousFree",
            Self::Credentialed => "credentialed",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeAuthKind {
    ChatGpt,
    Bearer,
    GrokSession,
    #[default]
    None,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeReasoningEffortTransport {
    ResponsesObject,
    ChatField,
    ChatObject,
    ProviderSpecific,
    #[default]
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct RuntimeReasoningCapabilities {
    pub supports_persisted_reasoning: bool,
    pub preserves_reasoning_in_compaction: bool,
    pub reasoning_efforts: Vec<String>,
    pub default_reasoning_effort: Option<String>,
    pub reasoning_effort_transport: RuntimeReasoningEffortTransport,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct RuntimeCompactionCapabilities {
    pub supports_server_side_compaction: bool,
    pub supports_standalone_compaction: bool,
    pub compact_threshold_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct RuntimeToolCapabilities {
    pub tool_calling: bool,
}

/// How a Chat Completions route continues a tool-result tail.
///
/// Unknown routes stay on the OpenAI-compatible default: the last tool
/// message is a valid tail. Only a probe or live test that observed a
/// provider rejecting that shape may persist [`NeutralUserBridge`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum ContinuationTail {
    #[default]
    NativeToolResult,
    NeutralUserBridge,
}

/// How readable reasoning is replayed on the Chat wire.
///
/// Default is no replay. [`ToolCallBound`] attaches reasoning only to the
/// assistant message that owns the matching tool call; it never creates an
/// independent user/system turn and never copies another turn's reasoning.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum ReasoningReplay {
    #[default]
    None,
    ToolCallBound,
}

/// Chat-wire continuation capabilities. Older configs omit this object and
/// deserialize as the OpenAI-compatible default. The runtime never infers
/// a bridge from a model name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeChatCapabilities {
    #[serde(default)]
    pub continuation_tail: ContinuationTail,
    #[serde(default)]
    pub reasoning_replay: ReasoningReplay,
}

/// Probe version that persists [`RuntimeChatCapabilities`] instead of relying
/// on the legacy `reasoning=true` Chat migration.
pub const CHAT_CAPABILITY_PROBE_VERSION: u32 = 5;

impl RuntimeChatCapabilities {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Older Chat+reasoning routes required `reasoning_content` on tool
    /// continuation. Settings that predate capability persistence inherit
    /// [`ReasoningReplay::ToolCallBound`]. New probes only persist that value
    /// after a successful round-trip observation.
    pub fn migrate_legacy(
        self,
        chat_wire: bool,
        reasoning: bool,
        probe_version: Option<u32>,
    ) -> Self {
        if probe_version.is_some_and(|version| version >= CHAT_CAPABILITY_PROBE_VERSION) {
            return self;
        }
        if chat_wire && reasoning && self.reasoning_replay == ReasoningReplay::None {
            Self {
                continuation_tail: self.continuation_tail,
                reasoning_replay: ReasoningReplay::ToolCallBound,
            }
        } else {
            self
        }
    }

    pub fn native_tool_result() -> Self {
        Self::default()
    }

    pub fn qwen_bridge() -> Self {
        Self {
            continuation_tail: ContinuationTail::NeutralUserBridge,
            reasoning_replay: ReasoningReplay::None,
        }
    }

    pub fn tool_call_bound_reasoning() -> Self {
        Self {
            continuation_tail: ContinuationTail::NativeToolResult,
            reasoning_replay: ReasoningReplay::ToolCallBound,
        }
    }
}

/// One catalog entry Codex can address: a route paired with one of its
/// upstream models, carrying every field the production pipeline needs to
/// adapt, route, and authorize a request. Field set mirrors plan §4.1.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeModelRoute {
    pub route_id: String,
    pub catalog_id: String,
    pub name: String,
    pub base_url: String,
    pub provider_kind: RuntimeProviderKind,
    pub auth_kind: RuntimeAuthKind,
    pub wire: RuntimeWireFormat,
    pub server_side_resume: bool,
    pub streaming: bool,
    pub reasoning: bool,
    pub vision: bool,
    pub upstream_model: String,
    pub context_window: Option<u64>,
    pub reasoning_capabilities: RuntimeReasoningCapabilities,
    pub compaction_capabilities: RuntimeCompactionCapabilities,
    /// Vellum Canonical auto-compact policy (plan M6).
    pub compaction_policy: crate::config::RuntimeCompactionPolicy,
    pub tool_capabilities: RuntimeToolCapabilities,
    /// Reference into `CredentialProvider`, never a plaintext secret.
    pub credential_id: Option<String>,
    /// Third-party plaintext HTTP exemption. Official OpenAI Responses ignores
    /// this field — that path never runs the outbound validator. Older configs
    /// that omit it deserialize as `Deny`.
    #[serde(default)]
    pub insecure_http_policy: crate::outbound::InsecureHttpPolicy,
    /// OpenCode Zen / Go profile. `None` for every other provider. Older
    /// configs omit it; request time still infers the exact catalog URLs.
    #[serde(default)]
    pub provider_profile: Option<RuntimeProviderProfile>,
    /// Per-model OpenCode access mode. `None` means "infer from the official
    /// free catalog"; unknown models infer as credentialed.
    #[serde(default)]
    pub access_mode: Option<RuntimeAccessMode>,
    /// Chat continuation capabilities. Older configs omit this and load as
    /// `nativeToolResult` + `none`. Unknown routes must not be guessed into
    /// a trailing-user bridge.
    #[serde(default, skip_serializing_if = "RuntimeChatCapabilities::is_default")]
    pub chat_capabilities: RuntimeChatCapabilities,
}

impl RuntimeModelRoute {
    pub fn effective_provider_profile(&self) -> Option<RuntimeProviderProfile> {
        if self.provider_kind != RuntimeProviderKind::OpenAiCompatible {
            return None;
        }
        let inferred = crate::opencode::infer_provider_profile(&self.base_url);
        match (self.provider_profile, inferred) {
            (Some(stored), Some(current)) if stored == current => Some(stored),
            (None, current) => current,
            _ => None,
        }
    }

    pub fn effective_access_mode(&self) -> Option<RuntimeAccessMode> {
        if let Some(mode) = self.access_mode {
            return Some(mode);
        }
        self.effective_provider_profile()
            .map(|profile| crate::opencode::default_access_mode(profile, &self.upstream_model))
    }
}

/// A route resolved for one request. Kept as its own type (rather than reusing
/// `RuntimeModelRoute` directly) so later milestones can attach per-request
/// resolution context (e.g. review fallback chain) without widening the
/// catalog-wide route shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedRoute {
    pub route: RuntimeModelRoute,
}

/// Port the runtime uses to ask "what can I route to" without depending on
/// Desktop's `AppState` or the daemon's static config directly.
pub trait RouteCatalog: Send + Sync {
    fn active_models(&self) -> Vec<RuntimeModelRoute>;
    fn resolve_model(&self, catalog_id: &str) -> Option<ResolvedRoute>;
    /// Resolves the model an auto-review turn should run against, which may
    /// differ from the conversation's primary route (plan §9, M9).
    fn resolve_review_model(&self, catalog_id: &str) -> Option<ResolvedRoute>;
    /// The model-visible catalog entry for a route — the exact entry Codex
    /// will read for this model, including `base_instructions`. This is the
    /// request-time authority the adapter verifies against and replaces the
    /// prompt baseline from. A catalog that does not carry an explicit entry
    /// may return `None`; callers fall back to the honest route-only
    /// projection, never a guessed-at private surface.
    fn catalog_entry(&self, catalog_id: &str) -> Option<Value> {
        let _ = catalog_id;
        None
    }

    /// One pass over routes plus their catalog entries. Desktop's snapshot is
    /// expensive; `RuntimeSnapshot::capture_from` must not rebuild it once per
    /// model. The default walks `active_models` / `catalog_entry`.
    fn route_entries(&self) -> Vec<(RuntimeModelRoute, Option<Value>)> {
        self.active_models()
            .into_iter()
            .map(|route| {
                let entry = self.catalog_entry(&route.catalog_id);
                (route, entry)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedCatalog {
        routes: Vec<RuntimeModelRoute>,
    }

    impl RouteCatalog for FixedCatalog {
        fn active_models(&self) -> Vec<RuntimeModelRoute> {
            self.routes.clone()
        }

        fn resolve_model(&self, catalog_id: &str) -> Option<ResolvedRoute> {
            self.routes
                .iter()
                .find(|route| route.catalog_id == catalog_id)
                .cloned()
                .map(|route| ResolvedRoute { route })
        }

        fn resolve_review_model(&self, catalog_id: &str) -> Option<ResolvedRoute> {
            self.resolve_model(catalog_id)
        }
    }

    fn sample_route() -> RuntimeModelRoute {
        RuntimeModelRoute {
            route_id: "route-1".into(),
            catalog_id: "vlm-sample".into(),
            name: "Sample".into(),
            base_url: "http://127.0.0.1:0/v1".into(),
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            auth_kind: RuntimeAuthKind::Bearer,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: false,
            streaming: true,
            reasoning: true,
            vision: false,
            upstream_model: "sample-model".into(),
            context_window: Some(128_000),
            reasoning_capabilities: RuntimeReasoningCapabilities::default(),
            compaction_capabilities: RuntimeCompactionCapabilities::default(),
            compaction_policy: crate::config::RuntimeCompactionPolicy::default(),
            tool_capabilities: RuntimeToolCapabilities::default(),
            credential_id: Some("sample-credential".into()),
            insecure_http_policy: crate::outbound::InsecureHttpPolicy::Deny,
            provider_profile: None,
            access_mode: None,
            chat_capabilities: RuntimeChatCapabilities::default(),
        }
    }

    #[test]
    fn resolve_model_finds_active_route_by_catalog_id() {
        let catalog = FixedCatalog {
            routes: vec![sample_route()],
        };
        let resolved = catalog.resolve_model("vlm-sample").expect("route present");
        assert_eq!(resolved.route.upstream_model, "sample-model");
        assert!(catalog.resolve_model("missing").is_none());
    }

    #[test]
    fn credential_id_is_a_reference_not_a_secret() {
        let route = sample_route();
        assert_eq!(route.credential_id.as_deref(), Some("sample-credential"));
    }

    #[test]
    fn legacy_chat_reasoning_routes_migrate_to_tool_call_bound() {
        let migrated = RuntimeChatCapabilities::default().migrate_legacy(true, true, Some(4));
        assert_eq!(migrated.reasoning_replay, ReasoningReplay::ToolCallBound);
        let probed = RuntimeChatCapabilities::default().migrate_legacy(
            true,
            true,
            Some(CHAT_CAPABILITY_PROBE_VERSION),
        );
        assert_eq!(probed.reasoning_replay, ReasoningReplay::None);
    }

    #[test]
    fn grok_chat_routes_are_rewritten_to_responses() {
        let mut route = sample_route();
        route.provider_kind = RuntimeProviderKind::GrokCli;
        route.wire = RuntimeWireFormat::Chat;
        assert!(rewrite_grok_chat_routes(std::slice::from_mut(&mut route)));
        assert_eq!(route.wire, RuntimeWireFormat::Responses);
        assert_eq!(
            projected_wire(RuntimeProviderKind::GrokCli, RuntimeWireFormat::Chat),
            RuntimeWireFormat::Responses
        );
        assert!(grok_responses_readiness(std::slice::from_ref(&route)).is_ok());
    }

    #[test]
    fn grok_readiness_fails_when_chat_remains() {
        let mut route = sample_route();
        route.provider_kind = RuntimeProviderKind::GrokCli;
        route.wire = RuntimeWireFormat::Chat;
        let error = grok_responses_readiness(std::slice::from_ref(&route)).unwrap_err();
        assert!(error.contains("Responses"), "{error}");
    }

    #[test]
    fn opencode_compatible_chat_wire_is_not_rewritten() {
        let mut route = sample_route();
        route.provider_kind = RuntimeProviderKind::OpenAiCompatible;
        route.wire = RuntimeWireFormat::Chat;
        assert!(!rewrite_grok_chat_routes(std::slice::from_mut(&mut route)));
        assert_eq!(route.wire, RuntimeWireFormat::Chat);
        assert_eq!(
            projected_wire(
                RuntimeProviderKind::OpenAiCompatible,
                RuntimeWireFormat::Chat
            ),
            RuntimeWireFormat::Chat
        );
    }

    #[test]
    fn opencode_profile_requires_an_openai_compatible_route_and_exact_origin() {
        let mut route = sample_route();
        route.base_url = crate::opencode::OPENCODE_ZEN_BASE_URL.into();
        route.provider_kind = RuntimeProviderKind::Official;
        route.provider_profile = Some(RuntimeProviderProfile::OpenCodeZen);
        assert_eq!(route.effective_provider_profile(), None);

        route.provider_kind = RuntimeProviderKind::OpenAiCompatible;
        assert_eq!(
            route.effective_provider_profile(),
            Some(RuntimeProviderProfile::OpenCodeZen)
        );
        route.base_url = "https://example.test/zen/v1".into();
        assert_eq!(route.effective_provider_profile(), None);
    }
}
