//! 前後端共用的資料形狀。
//!
//! 這裡的每個 struct 都對應 `src/types.ts` 的一個 interface。
//! 改動任一邊都要同步另一邊 —— 目前靠 `tests/contract.rs` 的欄位檢查擋住漏改。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use vellum_proxy_runtime::InsecureHttpPolicy;
pub use vellum_proxy_runtime::{ContinuationTail, ReasoningReplay, RuntimeChatCapabilities};

// Re-export remote desktop-facing types from the shared protocol/client layer so
// UI contract code can keep importing through `model` if needed later.
pub use crate::remote::client::ConnectionState as RemoteConnectionStatus;
pub use crate::remote::local_cache::CachedHost as RemoteHost;
pub use crate::remote::reducer::RemoteThreadState as RemoteThreadSummary;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WireFormat {
    Responses,
    Chat,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum ProviderKind {
    Official,
    #[default]
    OpenAiCompatible,
    GrokCli,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum AuthKind {
    ChatGpt,
    Bearer,
    GrokSession,
    #[default]
    None,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AccessMode {
    AnonymousFree,
    Credentialed,
}

impl From<AccessMode> for vellum_proxy_runtime::RuntimeAccessMode {
    fn from(value: AccessMode) -> Self {
        match value {
            AccessMode::AnonymousFree => vellum_proxy_runtime::RuntimeAccessMode::AnonymousFree,
            AccessMode::Credentialed => vellum_proxy_runtime::RuntimeAccessMode::Credentialed,
        }
    }
}

/// 上下文視窗數值的來源，決定 UI 亮起哪一層。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BudgetSource {
    Override,
    ModelCache,
    Catalog,
    Fallback,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum CatalogScope {
    #[default]
    All,
    FreeOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Route {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub wire: WireFormat,
    pub is_current: bool,
    /// 上游是否自己記住上一輪。false 代表要本地補歷史。
    pub server_side_resume: bool,
    pub streaming: bool,
    pub reasoning: bool,
    #[serde(default)]
    pub provider_kind: ProviderKind,
    #[serde(default)]
    pub auth_kind: AuthKind,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub models: Vec<String>,
    /// Models selected for exposure in Codex. `None` preserves legacy
    /// configurations where every discovered model was exposed.
    #[serde(default)]
    pub selected_models: Option<Vec<String>>,
    #[serde(default)]
    pub context_window: Option<u64>,
    /// Capabilities verified for individual upstream models. Older settings
    /// files omit this field and continue to use the provider-level fallback.
    #[serde(default)]
    pub model_capabilities: Vec<ModelCapability>,
    /// Third-party plaintext HTTP exemption. Omitted in older settings files
    /// so they load as `Deny`. Official routes ignore this field.
    #[serde(default)]
    pub insecure_http_policy: InsecureHttpPolicy,
    #[serde(default)]
    pub catalog_scope: CatalogScope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRouteInput {
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub wire: WireFormat,
    pub streaming: bool,
    pub reasoning: bool,
    pub server_side_resume: bool,
    pub provider_kind: Option<ProviderKind>,
    pub api_key: Option<String>,
    pub models: Option<Vec<String>>,
    pub selected_models: Option<Vec<String>>,
    pub context_window: Option<u64>,
    #[serde(default)]
    pub model_capabilities: Vec<ModelCapability>,
    #[serde(default)]
    pub catalog_scope: Option<CatalogScope>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRoute {
    pub catalog_id: String,
    pub display_name: String,
    pub route_id: String,
    pub upstream_model: String,
    pub context_window: Option<u64>,
    pub wire: WireFormat,
    pub reasoning: bool,
    pub streaming: bool,
    /// Drives `input_modalities` in the catalog Codex reads, which is what makes
    /// Codex offer or refuse image attachment for this model.
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub reasoning_efforts: Vec<String>,
    #[serde(default)]
    pub default_reasoning_effort: Option<String>,
    #[serde(default)]
    pub reasoning_effort_transport: ReasoningEffortTransport,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffortTransport {
    ResponsesObject,
    ChatField,
    ChatObject,
    /// `chat_template_kwargs.reasoning_effort` — the convention llama.cpp and
    /// vLLM use to pass a value through to a custom Jinja chat template's own
    /// `reasoning_effort` variable, for deployments whose template reads that
    /// variable but is not reachable through either OpenAI-dialect encoding
    /// above. Tried on both wires: the field sits outside the Chat/Responses
    /// request shape either way. See `probe::apply_effort_field`.
    ProviderSpecific,
    #[default]
    None,
}

/// Outcome of the last Effort probe attempt for one model, kept distinct from
/// `reasoning_efforts.is_empty()` so the UI (and `catalog.rs`'s resolver) can
/// tell "never probed" apart from "probed, and the provider genuinely has no
/// discrete levels" apart from "probed, but the run was inconclusive and
/// should be retried" — collapsing all three into one empty list is what let
/// a rate-limited or not-yet-run probe render as "Automatic" in the UI.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EffortProbeStatus {
    /// No Effort probe has ever completed for this model.
    #[default]
    NotProbed,
    /// The probe ran and determined Effort control does not apply here (the
    /// model does not support reasoning, or the harness/tool-calling probe
    /// this depends on did not verify).
    NotApplicable,
    /// The probe ran, reliably distinguished real levels from an invalid
    /// one, and found at least one accepted level. See `reasoning_efforts`.
    Supported,
    /// The probe ran, reliably distinguished real levels from an invalid
    /// one, and found none accepted.
    Unsupported,
    /// The probe attempted to run but could not reach a reliable verdict —
    /// a 401/403/429/5xx/timeout on a candidate request, or the provider
    /// accepting a deliberately invalid Effort value (proving it ignores the
    /// field rather than validating it). Never treated as "no levels"; the
    /// UI should offer a retry and `effort_probe_version` is left unset so a
    /// later re-probe is not silently skipped as already-done.
    Indeterminate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelCapability {
    pub model: String,
    pub context_window: Option<u64>,
    pub wire: Option<WireFormat>,
    pub streaming: Option<bool>,
    pub reasoning: Option<bool>,
    /// Whether the upstream returns a typed function/tool call that Codex can
    /// execute. Merely accepting a `tools` field is not sufficient: some
    /// vLLM deployments render `<tool_call>` as ordinary assistant text when
    /// no tool-call parser is configured.
    #[serde(default)]
    pub tool_calling: Option<bool>,
    /// Version of the Codex-dialect compatibility probe that produced this
    /// capability. `None` identifies legacy minimal-probe results.
    #[serde(default)]
    pub probe_version: Option<u32>,
    /// Machine-readable reason a capability probe could not be completed.
    /// This is deliberately separate from `tool_calling = Some(false)`: an
    /// account-level opt-in or access policy does not prove that the model is
    /// text-only.
    #[serde(default)]
    pub probe_issue: Option<String>,
    /// What to call this model in the Codex picker, when the upstream id is
    /// unreadable (`hf.co/unsloth/Qwen3.6-35B-A3B-GGUF:UD-Q4_K_M`). Display
    /// only — the upstream id is still what gets sent, and it is half of
    /// `stable_catalog_id`, so renaming must never touch it. `None` shows the
    /// upstream id unchanged.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Whether this model accepts image input, declared by the user rather than
    /// probed. There is no reliable status-code test: a text-only server happily
    /// returns 200 for a request carrying an image and lets the model invent an
    /// answer, so acceptance proves nothing. Being user-declared, it is read
    /// without the `probe_version` gate the probed fields use — a re-probe must
    /// not silently drop what the user set.
    #[serde(default)]
    pub vision: Option<bool>,
    /// Whether the model accepts OpenAI-style retained reasoning context.
    #[serde(default)]
    pub supports_persisted_reasoning: Option<bool>,
    /// Whether ordinary `/responses` accepts server-side compaction.
    #[serde(default)]
    pub supports_server_side_compaction: Option<bool>,
    /// Whether `/responses/compact` is available for this model.
    #[serde(default)]
    pub supports_standalone_compaction: Option<bool>,
    /// Compact threshold in tokens advertised by the model / probe.
    #[serde(default)]
    pub compact_threshold_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_efforts: Vec<String>,
    #[serde(default)]
    pub default_reasoning_effort: Option<String>,
    #[serde(default)]
    pub reasoning_effort_transport: ReasoningEffortTransport,
    /// `Some` only when `effort_probe_status` is `NotApplicable`, `Supported`,
    /// or `Unsupported` — a genuinely determinable outcome. Left `None` on
    /// `Indeterminate` so a stale version never suppresses a retry.
    #[serde(default)]
    pub effort_probe_version: Option<u32>,
    #[serde(default)]
    pub effort_probed_at: Option<i64>,
    #[serde(default)]
    pub effort_probe_status: EffortProbeStatus,
    /// Machine-readable reason the Effort probe landed on `Indeterminate`
    /// (e.g. `provider_quota`, `provider_error`, `timeout`,
    /// `provider_ignores_unknown_effort`). `None` for every other status.
    #[serde(default)]
    pub effort_probe_issue: Option<String>,
    /// Whether this model is free to use, per the provider's own catalog
    /// metadata (currently populated for OpenCode Zen/Go only). `None` means
    /// the provider has no free/paid distinction Vellum knows about, not
    /// "known to be paid" — the UI should not render a badge either way.
    #[serde(default)]
    pub free: Option<bool>,
    /// The provider has retired this model. Sourced from the same bundled
    /// registry as `free`, and orthogonal to it: `deepseek-v4-flash-free`
    /// still costs nothing and is still deprecated. A free-tier catalog has
    /// to read both, or it keeps offering models the provider has withdrawn.
    /// `None` means unknown, never "current".
    #[serde(default)]
    pub deprecated: Option<bool>,
    /// OpenCode authentication mode persisted per model. Official catalog
    /// free Zen models are `anonymousFree`; paid, Go, and unknown models are
    /// `credentialed`. Omitted in older settings files.
    #[serde(default)]
    pub access_mode: Option<AccessMode>,
    /// Chat continuation capabilities. Older settings omit this and load as
    /// native tool-result tail with no reasoning replay. Probe/live tests may
    /// persist a neutral user bridge; the runtime never infers one from a name.
    #[serde(default, skip_serializing_if = "RuntimeChatCapabilities::is_default")]
    pub chat_capabilities: RuntimeChatCapabilities,
    /// Detailed diagnostic probe attempts recorded during probing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub probe_attempts: Vec<ProbeAttempt>,
    /// Whether the most recent capability probe for this model failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_probe_failed: Option<bool>,
    /// Timestamp when this model's capabilities were last probed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_probed_at: Option<i64>,
}

/// One individual probe request/stage attempt and its diagnostic outcome.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProbeAttempt {
    pub stage: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire: Option<WireFormat>,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub duration_ms: u64,
    #[serde(default)]
    pub timeout: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_response_kind: Option<String>,
}

/// How Vellum should request retained private reasoning across turns.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningContextMode {
    /// Leave the caller's `reasoning` object untouched.
    #[default]
    Passthrough,
    Auto,
    CurrentTurn,
    AllTurns,
}

/// Which compaction backend should drive context management for a request.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompactionBackend {
    #[default]
    None,
    ServerSide,
    Standalone,
    LocalStructured,
}

/// Resolved continuity capabilities for a route/model pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ContextCapabilities {
    pub supports_persisted_reasoning: bool,
    pub supports_server_side_compaction: bool,
    pub supports_standalone_compaction: bool,
    pub preserves_reasoning_in_compaction: bool,
    pub compaction_backend: CompactionBackend,
    pub compact_threshold_tokens: Option<u64>,
    pub requires_same_account_realm: bool,
}

impl ContextCapabilities {
    /// Derive continuity capabilities from provider kind and optional probe data.
    pub fn resolve(
        provider_kind: ProviderKind,
        upstream_model: &str,
        model_capability: Option<&ModelCapability>,
        context_window: Option<u64>,
    ) -> Self {
        // Official defaults are intentionally conservative unless probe/catalog
        // evidence opts a model into the newer retained-reasoning surfaces.
        // Blanket-true injection caused 400s on older Official models.
        let mut caps = match provider_kind {
            ProviderKind::Official => {
                let known_modern = official_model_supports_modern_continuity(upstream_model);
                Self {
                    supports_persisted_reasoning: known_modern,
                    supports_server_side_compaction: known_modern,
                    supports_standalone_compaction: true,
                    preserves_reasoning_in_compaction: known_modern,
                    compaction_backend: if known_modern {
                        CompactionBackend::ServerSide
                    } else {
                        CompactionBackend::Standalone
                    },
                    compact_threshold_tokens: context_window.map(|window| {
                        // Keep a comfortable margin for output + tool results.
                        ((window as f64) * 0.78) as u64
                    }),
                    requires_same_account_realm: true,
                }
            }
            ProviderKind::GrokCli => Self {
                supports_persisted_reasoning: true,
                supports_server_side_compaction: false,
                supports_standalone_compaction: false,
                preserves_reasoning_in_compaction: true,
                compaction_backend: CompactionBackend::LocalStructured,
                compact_threshold_tokens: context_window
                    .map(|window| ((window as f64) * 0.8) as u64),
                requires_same_account_realm: true,
            },
            ProviderKind::OpenAiCompatible => Self {
                supports_persisted_reasoning: false,
                supports_server_side_compaction: false,
                supports_standalone_compaction: false,
                preserves_reasoning_in_compaction: false,
                compaction_backend: CompactionBackend::LocalStructured,
                compact_threshold_tokens: context_window
                    .map(|window| ((window as f64) * 0.8) as u64),
                requires_same_account_realm: false,
            },
        };
        if let Some(capability) = model_capability {
            if official_model_supports_modern_continuity(&capability.model)
                && provider_kind == ProviderKind::Official
            {
                caps.supports_persisted_reasoning = true;
                caps.supports_server_side_compaction = true;
                caps.preserves_reasoning_in_compaction = true;
                caps.compaction_backend = CompactionBackend::ServerSide;
            }
            if let Some(value) = capability.supports_persisted_reasoning {
                caps.supports_persisted_reasoning = value;
            }
            if let Some(value) = capability.supports_server_side_compaction {
                caps.supports_server_side_compaction = value;
                if value {
                    caps.compaction_backend = CompactionBackend::ServerSide;
                } else if caps.compaction_backend == CompactionBackend::ServerSide {
                    caps.compaction_backend = CompactionBackend::Standalone;
                }
            }
            if let Some(value) = capability.supports_standalone_compaction {
                caps.supports_standalone_compaction = value;
            }
            if let Some(value) = capability.compact_threshold_tokens {
                caps.compact_threshold_tokens = Some(value);
            }
        }
        caps
    }
}

/// Models known to accept `reasoning.context` / server-side `context_management`.
/// Unknown Official models stay on standalone compact + local hydrate defaults.
fn official_model_supports_modern_continuity(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains("gpt-5")
        || lower.contains("o3")
        || lower.contains("o4")
        || lower.contains("codex")
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProxyPhase {
    Preparing,
    Starting,
    Running,
    Stopping,
    #[default]
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyStatus {
    pub running: bool,
    pub base_url: String,
    pub catalog_path: Option<String>,
    pub codex_managed: bool,
    /// 上游或伺服器任務真的壞掉時的原文。內容來自外部，翻不了。
    pub last_error: Option<String>,
    /// 需要使用者知道、但不是故障的狀態（例如上次沒還原的 Codex 設定）。
    /// 只有代號，句子由前端用當前語言組。
    pub notice: Option<RuntimeNotice>,
    /// Lifecycle phase. `running` stays as the boolean compatibility field.
    #[serde(default)]
    pub phase: ProxyPhase,
    #[serde(default)]
    pub operation_id: Option<String>,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub stage: Option<String>,
    #[serde(default)]
    pub stage_elapsed_ms: Option<u64>,
}

/// Generation-keyed Desktop lifecycle event. The UI applies only the current
/// generation and must not wait for the status poll to show prepare/start/stop
/// stages.
pub const PROXY_LIFECYCLE_EVENT: &str = "proxy://lifecycle";

impl Default for ProxyStatus {
    fn default() -> Self {
        Self {
            running: false,
            base_url: "http://127.0.0.1:15721/v1".into(),
            catalog_path: None,
            codex_managed: false,
            last_error: None,
            notice: None,
            phase: ProxyPhase::Stopped,
            operation_id: None,
            generation: 0,
            stage: None,
            stage_elapsed_ms: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogStatus {
    pub proxy_running: bool,
    pub injected_model_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreResult {
    pub status: ProxyStatus,
    pub changed: bool,
    pub cleared: Vec<String>,
    pub preserved: Vec<String>,
}

/// 後端要說給人聽的一句話。後端只給代號與參數，句子由前端用當前語言組。
///
/// 之前這裡是 `Vec<String>`，字面就是繁體中文，換介面語言換不掉 ——
/// 「已偵測到 Codex 重新啟動」會原樣出現在英文與日文介面裡。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeNotice {
    /// i18n 鍵的尾段，例如 `codexRestartDetected`。
    pub code: String,
    /// 插值參數。沒有參數時是空的物件，不是 null。
    pub params: BTreeMap<String, String>,
}

impl RuntimeNotice {
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            params: BTreeMap::new(),
        }
    }

    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.params.insert(key.into(), value.into());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatus {
    pub proxy_running: bool,
    pub codex_managed: bool,
    pub active_requests: u64,
    pub draining: bool,
    pub restart_required: bool,
    pub restart_reasons: Vec<RuntimeNotice>,
    pub live_applied: Vec<RuntimeNotice>,
    pub active_catalog_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogVersion {
    pub id: String,
    pub created_at: i64,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestartResult {
    pub restarted: bool,
    pub notice: RuntimeNotice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextBudget {
    pub route_id: String,
    pub catalog_id: String,
    pub model: String,
    pub source: BudgetSource,
    pub context_window: u64,
    pub effective_percent: u32,
    pub effective_window: u64,
    pub override_tokens: Option<u64>,
    pub compact_threshold_percent: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatus {
    pub id: String,
    pub label: Option<String>,
    pub route_id: String,
    pub provider: String,
    pub model: String,
    pub used_tokens: u64,
    pub window_tokens: u64,
    pub compact_threshold_percent: u32,
    pub last_activity_at: i64,
    /// Execution plane that owns this conversation's compaction lifecycle.
    pub core: Option<String>,
}

/// 額度視窗的長度單位。後端不組人看的字串 —— 一旦組了，那句話就是
/// 後端的語言，前端換語言也換不掉它。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QuotaPeriodUnit {
    Hour,
    Day,
    Week,
    Month,
    /// 上游沒有給可辨識的視窗長度。
    Unspecified,
}

/// 額度視窗長度。`amount` 只有在單位需要數量時才有值（5 小時、30 天）；
/// 週與月是命名視窗，數量沒有意義。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaPeriod {
    pub unit: QuotaPeriodUnit,
    pub amount: Option<i64>,
}

impl QuotaPeriod {
    pub fn named(unit: QuotaPeriodUnit) -> Self {
        Self { unit, amount: None }
    }

    pub fn counted(unit: QuotaPeriodUnit, amount: i64) -> Self {
        Self {
            unit,
            amount: Some(amount),
        }
    }

    pub fn unspecified() -> Self {
        Self::named(QuotaPeriodUnit::Unspecified)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSnapshot {
    pub route_id: String,
    pub used_percent: f64,
    pub period: QuotaPeriod,
    pub reset_at: Option<String>,
    pub tier: Option<String>,
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    pub used_tokens: u64,
    pub window_tokens: u64,
    pub turns: u32,
    pub provider_total_tokens: u64,
    pub trend: Vec<f64>,
    pub compacted: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    Warning,
    Info,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub id: String,
    pub severity: Severity,
    pub title: String,
    pub location: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewSettings {
    pub on_edit: bool,
    pub before_send: bool,
    pub before_compact: bool,
    #[serde(default)]
    pub route_id: String,
    pub model: String,
    #[serde(default)]
    pub policy: Option<ReviewPolicy>,
    #[serde(default)]
    pub fallback_catalog_id: Option<String>,
    /// Which ChatGPT account pays for a review that runs on the Official
    /// plane. `None` follows the account the user's own turns use, which is
    /// what every saved config before this field meant. Stores an account
    /// id only -- the credential itself stays in the OAuth store.
    #[serde(default)]
    pub official_account_id: Option<String>,
}

/// Result of a Guardian settings save (M9 protocol fix). `settings` is the
/// canonical, validated value actually persisted; `localApplied` documents
/// the guarantee that the local proxy's *very next* request already reads it
/// (no restart needed — see `ReviewSettingsSource` in the shared runtime);
/// `remoteHostsPendingReapply` counts registered remote hosts whose deployed
/// Auto Review policy now differs from this local value, so the Settings UI
/// can point the user at Remote Manager instead of implying remote hosts
/// were silently touched.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewSettingsUpdate {
    pub settings: ReviewSettings,
    pub local_applied: bool,
    pub remote_hosts_pending_reapply: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReviewPolicy {
    Always,
    Failover,
}

/// Which sub-agent model default Vellum should manage in Codex.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SubagentMode {
    /// Leave Codex's own sub-agent defaults untouched (the parent model keeps
    /// deciding what a spawned agent runs on).
    #[default]
    Inherit,
    /// Write the selected Provider/model/effort as Codex's sub-agent defaults.
    Custom,
}

/// Vellum-managed defaults for Codex native sub-agents.
///
/// Codex resolves each spawned agent independently: an explicit spawn value,
/// then `[agents]` defaults, then the parent's value. Vellum only manages the
/// `[agents].default_subagent_model` and
/// `[agents].default_subagent_reasoning_effort` keys, so `inherit` keeps the
/// existing Codex behaviour byte-for-byte and `custom` only adds defaults that
/// an explicit spawn request can still override.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct SubagentSettings {
    #[serde(default)]
    pub mode: SubagentMode,
    /// Provider route the default model belongs to. The Provider itself is
    /// derived from this route so a Provider/model mismatch can never be
    /// persisted.
    #[serde(default)]
    pub route_id: Option<String>,
    /// Stable catalog id written to `agents.default_subagent_model`.
    #[serde(default)]
    pub catalog_id: Option<String>,
    /// Reasoning effort written to `agents.default_subagent_reasoning_effort`.
    /// `None` means automatic: omit the key so the selected model uses its own
    /// default effort.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

/// Runtime support for Codex's native `[agents]` default-model keys.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubagentCapability {
    pub supported: bool,
    /// User-facing Desktop app build (for example `26.818.61809`).
    pub desktop_version: Option<String>,
    /// Bundled core version used only for capability verification. The UI
    /// must not present this as the user's installed CLI version.
    pub runtime_version: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewProviderStat {
    pub route_id: String,
    pub provider: String,
    pub model: String,
    pub primary_runs: u64,
    pub fallback_runs: u64,
    pub failed_runs: u64,
    pub last_used_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewStats {
    pub total_runs: u64,
    pub fallback_runs: u64,
    pub providers: Vec<ReviewProviderStat>,
    pub active_route_id: Option<String>,
    pub active_model: Option<String>,
    pub active_is_fallback: bool,
    pub active_reason: Option<String>,
}

impl Default for ReviewSettings {
    fn default() -> Self {
        Self {
            on_edit: false,
            // 歷史壓掉就回不來，這是最後能攔住錯誤的時間點。
            before_send: true,
            before_compact: true,
            route_id: String::new(),
            model: String::new(),
            policy: None,
            fallback_catalog_id: None,
            official_account_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSegment {
    /// Stable semantic key used by the renderer. `label` is presentation only.
    #[serde(default)]
    pub kind: String,
    pub label: String,
    pub before: u32,
    pub after: u32,
    pub tone: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionPreview {
    pub before_tokens: u64,
    pub after_tokens: u64,
    #[serde(default = "default_true")]
    pub after_tokens_exact: bool,
    pub segments: Vec<CompactionSegment>,
    pub keep_recent_turns: u32,
    /// `official_canonical`, `local_canonical`, or `pending`.
    #[serde(default)]
    pub engine: String,
    #[serde(default)]
    pub readable_replay_tokens: u64,
    #[serde(default)]
    pub cross_session_tokens: u64,
    #[serde(default)]
    pub cross_session_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResult {
    pub reachable: bool,
    pub wire: Option<WireFormat>,
    pub models: Vec<String>,
    pub context_window: Option<u64>,
    pub streaming: bool,
    pub reasoning: bool,
    pub server_side_resume: bool,
    #[serde(default)]
    pub model_capabilities: Vec<ModelCapability>,
    /// 探不到、需要人填的欄位名，對應前端的 FIELD_LABEL。
    pub needs_input: Vec<String>,
    /// Optional stream-quality observation from a live run. When present, the
    /// persisted `streaming` flag is projected through
    /// `probe_streaming_projection` instead of trusting `streaming` blindly:
    /// only `incremental` keeps `streaming=true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_quality: Option<String>,
}

/// One model's re-probe failure, surfaced individually rather than folded
/// into a single provider-level error string — a Provider re-probe touching
/// several selected models must let the caller see *which* ones failed and
/// why, not just that "something" did.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelProbeError {
    pub model: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default)]
    pub timeout: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<u64>,
}

/// Result of a Provider-level re-probe (`reprobe_route_capabilities`).
///
/// Deliberately not pass/fail: refreshing the `/models` catalog and
/// verifying the currently-selected models' capabilities are two operations
/// with independent success/failure, and a partial failure (one selected
/// model's endpoint timed out) must not read as "re-probe failed" when every
/// other selected model verified fine. Only a catalog refresh failure, a
/// credential error, or total unreachability fails the whole command (see
/// `reprobe_route_capabilities`); everything else lands here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct RouteReprobeReport {
    /// Models found in this refresh of the upstream `/models` catalog.
    pub discovered: u32,
    /// Models this re-probe actually attempted to verify — the route's
    /// current `selectedModels`, intersected with `discovered` (a selected
    /// model no longer offered upstream is `skipped`, not `targeted`).
    pub targeted: u32,
    pub succeeded: u32,
    pub failed: u32,
    /// Targeted models that could not be probed at all this round (removed
    /// from the upstream catalog since being selected, or otherwise
    /// unresolvable) — distinct from `failed`, which means a probe request
    /// for that model actually ran and did not succeed.
    pub skipped: u32,
    #[serde(default)]
    pub errors: Vec<ModelProbeError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct HistoryStorageTelemetry {
    pub logical_live_bytes: u64,
    pub main_db_physical_bytes: u64,
    pub wal_bytes: u64,
    pub compaction_journal_bytes: u64,
    pub history_truncated_without_compaction: u64,
    pub history_truncated_items: u64,
    pub last_maintenance_at: Option<i64>,
    pub last_vacuum_at: Option<i64>,
    pub last_journal_retention_deleted: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub pooled: bool,
    pub connections: u32,
    pub first_byte_ms: Option<u32>,
    pub reasoning_visible: bool,
    pub history_retention_days: u32,
    /// Separated history storage metrics (issue #1 DoD).
    #[serde(default)]
    pub history_storage: Option<HistoryStorageTelemetry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Overview {
    pub route: Option<Route>,
    pub last_successful_route: Option<RouteTelemetry>,
    pub quota: Option<QuotaSnapshot>,
    pub usage: ContextUsage,
    pub health: Health,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RouteTelemetry {
    pub route_id: String,
    pub provider: String,
    pub model: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModelStatus {
    pub catalog_id: String,
    pub display_name: String,
    pub upstream_model: String,
    pub context_window: Option<u64>,
    pub effective_window: u64,
    pub reasoning: bool,
    pub streaming: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderOverview {
    pub route: Route,
    pub applied_to_running_proxy: bool,
    pub quota: Option<QuotaSnapshot>,
    pub quota_windows: Vec<QuotaSnapshot>,
    pub quota_error: Option<String>,
    pub models: Vec<ProviderModelStatus>,
    pub latest_input_tokens: u64,
    pub turns: u32,
    pub first_byte_ms: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestLogEntry {
    pub id: i64,
    pub route_id: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub status: u16,
    pub error: Option<String>,
    pub duration_ms: u64,
    pub first_byte_ms: Option<u64>,
    pub created_at: i64,
    pub connection_id: Option<String>,
    /// Stream-quality classification, `None` when not observed. Transport
    /// outcome (`status`) and quality stay separate.
    pub stream_quality: Option<String>,
    pub first_output_delta_ms: Option<u64>,
    pub first_reasoning_delta_ms: Option<u64>,
    pub output_delta_count: u64,
    pub reasoning_delta_count: u64,
    /// Stable short SHA-256 identity of the Vellum request id, when one was
    /// assigned. Lets the Log screen correlate a parent request row to its
    /// spawned subagent runs without ever exposing the raw request id to the
    /// client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_account_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_account_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsage {
    pub provider: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub failed_requests: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionLogEntry {
    pub id: i64,
    pub created_at: i64,
    pub engine: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_before: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_after: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_model_visible_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement_model_visible_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement_durable_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality_outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_generation: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items_before: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items_after: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold_percent: Option<u32>,
    /// The durable journal id this checkpoint materializes from — absent for
    /// a stateless standalone compact that was never journaled (see
    /// `execute_compact` in `vellum-proxy-runtime`). Never fabricated: this
    /// mirrors the underlying diagnostic exactly, including its absence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentLogEntry {
    pub id: i64,
    pub created_at: i64,
    pub kind: String,
    pub call_id: Option<String>,
    pub parent_request_id: Option<String>,
    pub child_request_id: Option<String>,
    pub child_model: Option<String>,
    pub link_method: Option<String>,
    pub link_confidence: Option<String>,
}

/// One spawned child agent's whole life. Native V2 is aggregated from the
/// authoritative `SubagentGraphLinked` edge and the child's actual
/// `request_usage`; the translated compatibility path uses its raw
/// `SpawnRequested` / `ChildTurn` / `SpawnCompleted` events.
///
/// This is deliberately not a passthrough of `SpawnCompleted`: that event
/// only proves the parent got *a* tool result back, not that the child
/// request behind it actually succeeded. `state` is the honest answer;
/// `Completed` is reserved for runs where the joined child outcome itself
/// says success.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubagentRunState {
    /// Only `SpawnRequested` observed so far.
    Requested,
    /// A child request started (linked via `ChildTurn`) but has not reached
    /// a terminal outcome yet.
    Running,
    /// `SpawnCompleted` observed AND the joined child request's own
    /// `request_usage` row reports success.
    Completed,
    /// The joined child outcome is a provider/protocol error, or
    /// `SpawnCompleted` never arrived within the correlation window.
    Failed,
    /// The joined child outcome is `client_cancel` / `client_disconnect`.
    Cancelled,
    /// Two or more spawns were pending at once with no confident way to tell
    /// which child request answered which `call_id` — never silently
    /// resolved to a guess.
    Ambiguous,
    /// A child request (or completion) was observed but could not be
    /// confidently correlated back to any `call_id`.
    Unlinked,
}

/// How the child request id attached to this run was established.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubagentLinkConfidence {
    /// Exact match (content hash of the spawned prompt).
    Exact,
    /// A weaker heuristic matched (configured model / solitary time window).
    Heuristic,
    /// No child request could be confidently attached.
    Unlinked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentRun {
    /// Absent only for orphan child turns that never matched any `call_id`
    /// (state is always `Unlinked` in that case).
    pub call_id: Option<String>,
    pub state: SubagentRunState,
    /// Raw Vellum request ids. Kept only for internal joins against
    /// `request_usage`; never serialized to the client.
    #[serde(skip)]
    pub parent_request_id: Option<String>,
    #[serde(skip)]
    pub child_request_id: Option<String>,
    /// Stable short SHA-256 identities of the parent/child request, for UI
    /// display and cross-referencing. Never the raw request id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_request_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_request_hash: Option<String>,
    /// `request_usage.id` of the parent/child row — the local navigation
    /// anchor the Log screen scrolls to. Absent when the row has not landed
    /// or no confident link exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_usage_entry_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_usage_entry_id: Option<i64>,
    pub route_id: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub link_confidence: SubagentLinkConfidence,
    pub requested_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<i64>,
    /// The child's own execution time (preferred) or, failing that, the
    /// parent-observed round trip from `SpawnCompleted`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// The child's own recorded outcome (`success`, `provider_failure`,
    /// `protocol_failure`, `client_cancel`, `client_disconnect`) when a
    /// confident join was possible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestLogRouteOption {
    pub route_id: String,
    pub provider: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestLog {
    pub entries: Vec<RequestLogEntry>,
    pub providers: Vec<ProviderUsage>,
    /// Rows matching the current filter, not the page length.
    #[serde(default)]
    pub entry_total: u32,
    /// Actual entry OFFSET applied after focus / clamp.
    #[serde(default)]
    pub entry_offset: u32,
    #[serde(default)]
    pub compaction_total: u32,
    #[serde(default)]
    pub subagent_total: u32,
    /// Distinct routes present in the ledger, for the Log filter menu.
    #[serde(default)]
    pub entry_routes: Vec<RequestLogRouteOption>,
    #[serde(default)]
    pub compaction_events: Vec<CompactionLogEntry>,
    /// Raw per-event feed (one row per `SpawnRequested`/`ChildTurn`/
    /// `SpawnCompleted`). Kept for lower-level inspection; the Log screen
    /// renders [`Self::subagent_runs`] instead.
    #[serde(default)]
    pub subagent_events: Vec<SubagentLogEntry>,
    /// One row per unique native graph edge or compatibility spawn, joined
    /// against `request_usage` for the real child route/outcome.
    #[serde(default)]
    pub subagent_runs: Vec<SubagentRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageActivityDay {
    pub date: String,
    pub tokens: u64,
    pub requests: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageProviderTotal {
    pub route_id: String,
    pub provider: String,
    pub tokens: u64,
    pub account_count: u32,
    /// `codex_profile` is authoritative ChatGPT profile data. `proxy` is
    /// counted from one upstream response recorded by Vellum.
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageActivity {
    pub days: Vec<UsageActivityDay>,
    pub total_tokens: u64,
    pub peak_tokens: u64,
    pub longest_task_duration_ms: u64,
    pub current_streak_days: u32,
    pub longest_streak_days: u32,
    pub providers: Vec<UsageProviderTotal>,
    pub official_source: String,
    pub warning: Option<RuntimeNotice>,
}
