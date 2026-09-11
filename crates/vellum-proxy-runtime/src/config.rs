use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::environment::ExecutionEnvironment;
use crate::route::{
    RuntimeAuthKind, RuntimeChatCapabilities, RuntimeCompactionCapabilities, RuntimeModelRoute,
    RuntimeProviderKind, RuntimeReasoningCapabilities, RuntimeToolCapabilities, RuntimeWireFormat,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProxyRuntimeIdentity {
    pub install_id: String,
    pub host_id: String,
    pub image_version: String,
    pub config_hash: String,
    /// Package/product version embedded at build time. Empty in older configs.
    #[serde(default)]
    pub version: String,
    /// Full git commit the binary was built from. Empty in older configs.
    #[serde(default)]
    pub commit: String,
    /// SHA-256 of the running executable. Filled lazily for `/version` and
    /// live attribution — never on `/readyz` or proxy bind.
    #[serde(default)]
    pub binary_hash: String,
}

impl Default for ProxyRuntimeIdentity {
    fn default() -> Self {
        Self {
            install_id: "local-dev".into(),
            host_id: "local".into(),
            image_version: crate::PROXY_RUNTIME_VERSION.into(),
            config_hash: "unset".into(),
            version: crate::PROXY_RUNTIME_VERSION.into(),
            commit: build_commit().into(),
            binary_hash: String::new(),
        }
    }
}

/// Git commit baked in by `build.rs`, or `unknown` for an uninstrumented tree.
pub fn build_commit() -> &'static str {
    option_env!("VELLUM_GIT_COMMIT").unwrap_or("unknown")
}

/// Product version baked in by the desktop/daemon build, falling back to the
/// runtime crate version.
pub fn build_version() -> &'static str {
    option_env!("VELLUM_BUILD_VERSION").unwrap_or(crate::PROXY_RUNTIME_VERSION)
}

static PROCESS_BINARY_HASH: OnceLock<String> = OnceLock::new();

fn hash_current_binary() -> String {
    let Ok(exe) = std::env::current_exe() else {
        return "unavailable".into();
    };
    crate::identity::sha256_file_streaming(&exe).unwrap_or_else(|_| "unavailable".into())
}

/// SHA-256 of the current process image. Best-effort: missing or unreadable
/// executables yield `"unavailable"`. Hashes with a streaming reader so a
/// large binary is not copied into a single buffer.
///
/// Computed once, lazily. Must not run on `/readyz` or the start
/// transaction: hashing the Desktop or test binary is hundreds of
/// milliseconds to seconds.
pub fn current_binary_sha256() -> String {
    PROCESS_BINARY_HASH.get_or_init(hash_current_binary).clone()
}

impl ProxyRuntimeIdentity {
    /// Fill compile-time version and commit when the stored identity left
    /// them empty. Does **not** hash the process image: that belongs on
    /// `/version` and live attribution, not bind or `/readyz`.
    pub fn with_process_identity(mut self) -> Self {
        if self.version.trim().is_empty() {
            self.version = build_version().into();
        }
        if self.commit.trim().is_empty() {
            self.commit = build_commit().into();
        }
        self
    }

    /// Fill `binary_hash` from the process image. Call from diagnostic
    /// surfaces, never from readiness.
    pub fn with_binary_hash(mut self) -> Self {
        if self.binary_hash.trim().is_empty() {
            self.binary_hash = current_binary_sha256();
        }
        self
    }
}

/// One fully-specified route the static (headless) config can ship. Field set
/// mirrors `RuntimeModelRoute` plus an optional `catalog_entry` so the
/// request-time prompt baseline and verification authority come from the
/// config, not from a synthetic projection in the execution engine.
///
/// Deliberately a flat struct (no `#[serde(flatten)]`): TOML's flatten support
/// is unreliable across serde versions, and an explicit shape keeps the wire
/// contract readable. Every field except the route's identity has a default so
/// a minimal `[[models]]` entry stays valid.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeRouteConfig {
    pub route_id: String,
    pub catalog_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub provider_kind: RuntimeProviderKind,
    #[serde(default)]
    pub auth_kind: RuntimeAuthKind,
    #[serde(default)]
    pub wire: RuntimeWireFormat,
    #[serde(default)]
    pub server_side_resume: bool,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub vision: bool,
    pub upstream_model: String,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub reasoning_capabilities: RuntimeReasoningCapabilities,
    #[serde(default)]
    pub compaction_capabilities: RuntimeCompactionCapabilities,
    /// Vellum Canonical auto-compact policy (plan M6). Mirrors Desktop's
    /// `SessionCompactionPolicy` fields that drive the threshold gate.
    #[serde(default)]
    pub compaction_policy: RuntimeCompactionPolicy,
    #[serde(default)]
    pub tool_capabilities: RuntimeToolCapabilities,
    #[serde(default)]
    pub credential_id: Option<String>,
    /// The model-visible catalog entry for this route (with `base_instructions`
    /// when the host has one). `None` falls back to the honest route-only
    /// projection.
    #[serde(default)]
    pub catalog_entry: Option<Value>,
    #[serde(default)]
    pub chat_capabilities: RuntimeChatCapabilities,
    /// The route owner's plaintext-HTTP exemption, as Desktop resolved it.
    /// Older configs omit it and load as `Deny`, which is the behaviour they
    /// already had: before this field existed `to_route` hard-coded `Deny`,
    /// so a remote host silently refused a LAN endpoint its Desktop was
    /// happily reaching.
    #[serde(default)]
    pub insecure_http_policy: crate::outbound::InsecureHttpPolicy,
    /// The OpenCode access mode the probe persisted for this exact model.
    /// `None` means "infer from the profile", which is what older configs do
    /// and what `to_route` used to do unconditionally -- overriding a probed
    /// `anonymousFree` back to `credentialed` on every remote deployment.
    #[serde(default)]
    pub access_mode: Option<crate::route::RuntimeAccessMode>,
}

/// The Vellum Canonical auto-compact policy for one route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeCompactionPolicy {
    /// Window percent that triggers compaction (Desktop default 60).
    #[serde(default = "default_compaction_threshold_percent")]
    pub threshold_percent: u32,
    #[serde(default)]
    pub output_reserve_tokens: u64,
    #[serde(default)]
    pub tool_reserve_tokens: u64,
    /// Grok Native auto-compact threshold override (None = Desktop default
    /// 85%).
    #[serde(default)]
    pub grok_threshold_percent: Option<u32>,
}

impl Default for RuntimeCompactionPolicy {
    fn default() -> Self {
        Self {
            threshold_percent: 60,
            output_reserve_tokens: 0,
            tool_reserve_tokens: 0,
            grok_threshold_percent: None,
        }
    }
}

fn default_compaction_threshold_percent() -> u32 {
    60
}

impl RuntimeCompactionPolicy {
    /// Codex-native auto-compact catalog limit for a model with the given
    /// context window, derived from this route's Canonical threshold and
    /// reserves. `None` when auto-compaction is disabled for the route, so
    /// callers omit `auto_compact_token_limit` from the catalog entry rather
    /// than publish a value Codex should not act on.
    ///
    /// Uses `threshold_percent` only — `grok_threshold_percent` governs the
    /// Grok-native runtime auto-compact gate, not catalog projection; Grok
    /// routes do not get a projected `auto_compact_token_limit` in this pass.
    pub fn auto_compact_token_limit(&self, context_window: u64) -> Option<u64> {
        let threshold = context_window.saturating_mul(self.threshold_percent as u64) / 100;
        let reserve = self
            .output_reserve_tokens
            .saturating_add(self.tool_reserve_tokens);
        Some(threshold.saturating_sub(reserve).max(1))
    }
}

impl RuntimeRouteConfig {
    pub fn to_route(&self) -> RuntimeModelRoute {
        let provider_profile = (self.provider_kind == RuntimeProviderKind::OpenAiCompatible)
            .then(|| crate::opencode::infer_provider_profile(&self.base_url))
            .flatten();
        let compaction_policy = self.compaction_policy.clone();
        if self.provider_kind == RuntimeProviderKind::Official {
            // Official Responses owns its native compaction contract. A
            // migrated/missing canonicalEngine value must never opt it into
            // Vellum's canonical engine.
        }
        RuntimeModelRoute {
            route_id: self.route_id.clone(),
            catalog_id: self.catalog_id.clone(),
            name: self.name.clone(),
            base_url: self.base_url.clone(),
            provider_kind: self.provider_kind,
            auth_kind: self.auth_kind,
            wire: crate::route::projected_wire(self.provider_kind, self.wire),
            server_side_resume: self.server_side_resume,
            streaming: self.streaming,
            reasoning: self.reasoning,
            vision: self.vision,
            upstream_model: self.upstream_model.clone(),
            context_window: self.context_window,
            reasoning_capabilities: self.reasoning_capabilities.clone(),
            compaction_capabilities: self.compaction_capabilities.clone(),
            compaction_policy,
            tool_capabilities: self.tool_capabilities.clone(),
            credential_id: self.credential_id.clone(),
            insecure_http_policy: self.insecure_http_policy,
            provider_profile,
            // Inference is the fallback, not the answer. A config that
            // carries what the probe found must not have it re-guessed from
            // the model name here.
            access_mode: self.access_mode.or_else(|| {
                provider_profile.map(|profile| {
                    crate::opencode::default_access_mode(profile, &self.upstream_model)
                })
            }),
            chat_capabilities: self.chat_capabilities.clone(),
        }
    }
}

/// How this proxy admits inbound callers.
///
/// There is no permissive variant. The only knob is *where* the boundary key
/// lives — the local install keeps it in the Vellum credential store, Remote
/// mounts it under `/run/secrets` — because a proxy that cannot find its key
/// must fail to start rather than fall back to admitting everyone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InboundAccessConfig {
    /// The credential ID holding the boundary key. Normally the reserved
    /// [`crate::inbound::BOUNDARY_CREDENTIAL_ID`].
    pub credential_id: String,
}

impl Default for InboundAccessConfig {
    fn default() -> Self {
        Self {
            credential_id: crate::inbound::BOUNDARY_CREDENTIAL_ID.to_string(),
        }
    }
}

/// Optional lower bounds for [`crate::resource::ResourcePolicy`].
///
/// Every field is independently optional. Missing fields keep the production
/// default. Values above the production default are clamped down at
/// construction time — a provider response can never raise these.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResourceLimitsConfig {
    pub buffered_byte_budget: Option<usize>,
    pub active_http_requests: Option<usize>,
    pub third_party_streams: Option<usize>,
    pub official_ws_slots: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyRuntimeConfig {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    pub history_dir: PathBuf,
    pub log_dir: PathBuf,
    pub model_catalog_path: Option<PathBuf>,
    pub credentials_dir: Option<PathBuf>,
    pub identity: ProxyRuntimeIdentity,
    #[serde(default)]
    pub models: Vec<RuntimeRouteConfig>,
    #[serde(default)]
    pub require_secrets: bool,
    #[serde(default)]
    pub strict_upstream: bool,
    /// Auto Review / Guardian policy (plan M9). The Desktop UI configures
    /// policy; the runtime executes it.
    #[serde(default)]
    pub review: crate::review::ReviewSettings,
    /// The explicit executor contract for the host that will actually run
    /// this proxy's Codex tools (plan §25.2/§23.6). The headless daemon must
    /// never guess it from the container it happens to live in, so there is no
    /// `#[serde(default)]`: a config that forgets to resolve the real executor
    /// fails closed at load time instead of silently advertising a reference.
    /// `Default` keeps the POSIX reference only for dev/test construction.
    pub execution_environment: ExecutionEnvironment,
    /// How inbound callers are admitted (V-01/V-02). Like the executor
    /// contract, this has no `#[serde(default)]`: a config that omits it fails
    /// to load rather than starting a proxy that answers to anyone who can
    /// reach the port.
    pub inbound_access: InboundAccessConfig,
    /// Optional lower resource limits (V-03). Absent fields keep production
    /// defaults; nothing can raise them above those defaults.
    #[serde(default)]
    pub resource: ResourceLimitsConfig,
    /// Set when a Grok Chat → Responses rewrite could not be persisted.
    /// Readiness and config validity must fail closed rather than start on
    /// an in-memory-only rewrite.
    #[serde(skip)]
    pub grok_rewrite_error: Option<String>,
}

impl Default for ProxyRuntimeConfig {
    fn default() -> Self {
        Self {
            schema_version: default_schema_version(),
            listen: "127.0.0.1:15721".parse().expect("static addr"),
            data_dir: PathBuf::from("./data"),
            history_dir: PathBuf::from("./history"),
            log_dir: PathBuf::from("./logs"),
            model_catalog_path: None,
            credentials_dir: None,
            identity: ProxyRuntimeIdentity::default(),
            models: Vec::new(),
            require_secrets: false,
            strict_upstream: false,
            review: crate::review::ReviewSettings::default(),
            execution_environment: ExecutionEnvironment::posix_reference(),
            inbound_access: InboundAccessConfig::default(),
            resource: ResourceLimitsConfig::default(),
            grok_rewrite_error: None,
        }
    }
}

impl ProxyRuntimeConfig {
    pub fn from_toml_str(raw: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(raw)
    }

    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|error| format!("failed to read proxy config {}: {error}", path.display()))?;
        let mut config =
            Self::from_toml_str(&raw).map_err(|error| format!("invalid proxy config: {error}"))?;
        if config.enforce_grok_responses() {
            if let Err(error) = persist_toml_atomically(path, &config) {
                config.grok_rewrite_error = Some(format!(
                    "Grok Responses rewrite could not be persisted: {error}"
                ));
            }
        }
        Ok(config)
    }

    /// Rewrite Grok Chat settings to Responses. Returns true when the
    /// in-memory config changed and should be persisted.
    pub fn enforce_grok_responses(&mut self) -> bool {
        crate::route::rewrite_grok_chat_routes(&mut self.models)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != default_schema_version() {
            return Err(format!(
                "unsupported proxy config schema_version {}; expected {}",
                self.schema_version,
                default_schema_version()
            ));
        }
        if self.identity.install_id.trim().is_empty() {
            return Err("identity.install_id must not be empty".into());
        }
        if self.identity.host_id.trim().is_empty() {
            return Err("identity.host_id must not be empty".into());
        }
        if self.inbound_access.credential_id.trim().is_empty() {
            return Err("inbound_access.credential_id must not be empty".into());
        }
        Ok(())
    }

    pub fn ensure_dirs(&self) -> Result<(), String> {
        for dir in [&self.data_dir, &self.history_dir, &self.log_dir] {
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
        }
        Ok(())
    }
}

/// Bumped to 2 by the inbound boundary guard: a version-1 config has no
/// `inbound_access`, and silently accepting one would start an unauthenticated
/// proxy. The version check refuses it so the Remote upgrade is atomic.
fn default_schema_version() -> u32 {
    2
}

fn persist_toml_atomically(
    path: &std::path::Path,
    config: &ProxyRuntimeConfig,
) -> Result<(), String> {
    let encoded = toml::to_string_pretty(config)
        .map_err(|error| format!("encode rewritten proxy config: {error}"))?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, encoded)
        .map_err(|error| format!("write rewritten proxy config {}: {error}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|error| format!("persist rewritten proxy config {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_process_identity_does_not_hash_the_executable() {
        let started = std::time::Instant::now();
        let identity = ProxyRuntimeIdentity::default().with_process_identity();
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "process identity on the start path hashed the executable ({:?})",
            elapsed
        );
        assert!(
            identity.binary_hash.is_empty(),
            "binary_hash must stay empty until /version or live attribution"
        );
        assert!(!identity.commit.is_empty());
        assert!(!identity.version.is_empty());
    }

    #[test]
    fn a_legacy_canonical_engine_setting_is_ignored_on_load() {
        // Canonical is retired and compaction is no longer selectable, so a
        // config written by an older build must load cleanly with the legacy
        // key simply dropped — never rejected, and never honoured.
        let migrated: RuntimeCompactionPolicy =
            serde_json::from_value(serde_json::json!({"canonicalEngine": "v1"}))
                .expect("a legacy canonicalEngine key must not fail the load");
        assert_eq!(migrated, RuntimeCompactionPolicy::default());

        let reserialized =
            serde_json::to_value(&migrated).expect("re-serialize the migrated policy");
        assert!(
            reserialized.get("canonicalEngine").is_none(),
            "the retired key must not be written back out: {reserialized}"
        );
    }

    #[test]
    fn an_official_route_resolves_to_provider_native_compaction() {
        let config = RuntimeRouteConfig {
            route_id: "official".into(),
            catalog_id: "official:gpt".into(),
            name: "OpenAI Official".into(),
            base_url: "https://api.openai.com/v1".into(),
            provider_kind: RuntimeProviderKind::Official,
            auth_kind: RuntimeAuthKind::ChatGpt,
            wire: RuntimeWireFormat::Responses,
            server_side_resume: true,
            streaming: true,
            reasoning: true,
            vision: true,
            upstream_model: "gpt".into(),
            context_window: Some(100_000),
            reasoning_capabilities: Default::default(),
            compaction_capabilities: Default::default(),
            compaction_policy: RuntimeCompactionPolicy::default(),
            tool_capabilities: Default::default(),
            credential_id: None,
            catalog_entry: None,
            chat_capabilities: Default::default(),
            insecure_http_policy: Default::default(),
            access_mode: None,
        };

        // Official compacts natively upstream; Vellum must never resolve it
        // to a local engine.
        let engine = crate::compaction_engine::resolve_compaction_engine(&config.to_route());
        assert_eq!(
            engine,
            crate::compaction_engine::ResolvedCompactionEngine::OfficialNative
        );
        assert!(!engine.is_vellum_local());
    }

    #[test]
    fn auto_compact_token_limit_applies_percent_and_reserves() {
        let policy = RuntimeCompactionPolicy {
            threshold_percent: 60,
            output_reserve_tokens: 4_096,
            tool_reserve_tokens: 8_192,
            grok_threshold_percent: None,
        };
        // 200_000 * 60% = 120_000; minus (4_096 + 8_192) reserve = 107_712.
        assert_eq!(policy.auto_compact_token_limit(200_000), Some(107_712));
    }

    #[test]
    fn auto_compact_token_limit_ignores_grok_threshold_override() {
        let mut policy = RuntimeCompactionPolicy::default();
        policy.threshold_percent = 60;
        policy.grok_threshold_percent = Some(85);
        // Must use threshold_percent, not grok_threshold_percent, for catalog
        // projection — Grok stays on its own native runtime gate.
        assert_eq!(
            policy.auto_compact_token_limit(100_000),
            RuntimeCompactionPolicy {
                grok_threshold_percent: None,
                ..policy.clone()
            }
            .auto_compact_token_limit(100_000)
        );
    }

    #[test]
    fn auto_compact_token_limit_saturates_when_reserves_exceed_threshold() {
        let policy = RuntimeCompactionPolicy {
            threshold_percent: 10,
            output_reserve_tokens: 50_000,
            tool_reserve_tokens: 50_000,
            grok_threshold_percent: None,
        };
        // 100_000 * 10% = 10_000, reserve is 100_000 — must floor at 1, not
        // underflow or return 0 (Codex treats 0 as "compact immediately").
        assert_eq!(policy.auto_compact_token_limit(100_000), Some(1));
    }

    #[test]
    fn auto_compact_token_limit_handles_zero_context_window() {
        let policy = RuntimeCompactionPolicy::default();
        assert_eq!(policy.auto_compact_token_limit(0), Some(1));
    }

    /// A minimal-but-valid config body in the daemon's wire shape, with or
    /// without the `[execution_environment]` table.
    fn config_toml(executor_env: &str) -> String {
        format!(
            "listen = \"0.0.0.0:15721\"\n\
data_dir = \"/var/lib/vellum/data\"\n\
history_dir = \"/var/lib/vellum/history\"\n\
log_dir = \"/var/log/vellum\"\n\
credentials_dir = \"/run/secrets\"\n\
require_secrets = false\n\
strict_upstream = false\n\
\n\
[inbound_access]\n\
credentialId = \"__vellum_proxy_boundary__\"\n\
\n\
[identity]\n\
install_id = \"install-1\"\n\
host_id = \"host-1\"\n\
image_version = \"0.1.0\"\n\
config_hash = \"abc\"\n\
\n\
[[models]]\n\
route_id = \"mock\"\n\
catalog_id = \"vellum-mock\"\n\
name = \"Vellum Mock\"\n\
base_url = \"http://127.0.0.1:1/v1\"\n\
provider_kind = \"openAiCompatible\"\n\
auth_kind = \"none\"\n\
wire = \"responses\"\n\
server_side_resume = false\n\
streaming = true\n\
reasoning = false\n\
vision = false\n\
upstream_model = \"mock-1\"\n\
context_window = 128000\n\
{executor_env}"
        )
    }

    /// Plan §23.4 / reviewer P0: a config that omits the executor contract
    /// must fail closed. Without `#[serde(default)]`, the missing field is a
    /// hard load error — the daemon never silently falls back to the POSIX
    /// reference on the production path.
    #[test]
    fn config_without_executor_environment_fails_closed() {
        let error = ProxyRuntimeConfig::from_toml_str(&config_toml("")).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("execution_environment"),
            "the load error must name the missing executor contract: {message}"
        );
    }

    /// V-01: a config that omits `inbound_access` must not load. Accepting it
    /// would start a proxy that answers to anyone who can reach the port.
    #[test]
    fn config_without_inbound_access_fails_closed() {
        let raw = config_toml(
            "\n\
[execution_environment]\n\
platform = \"linux\"\n\
shell = \"bash\"\n\
supportsAndAnd = true\n\
hasUnixUtilities = true\n\
pathStyle = \"posix\"\n\
ampersandSemantics = \"posix-background\"\n",
        )
        .replace(
            "[inbound_access]\ncredentialId = \"__vellum_proxy_boundary__\"\n\n",
            "",
        );
        let message = ProxyRuntimeConfig::from_toml_str(&raw)
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("inbound_access"),
            "the load error must name the missing access policy: {message}"
        );
    }

    /// A pre-guard (schema 1) config is refused rather than silently upgraded.
    /// The Remote rollout replaces config and image together; there is no
    /// unauthenticated compatibility window to fall back into.
    #[test]
    fn a_pre_boundary_schema_version_is_refused() {
        let config = ProxyRuntimeConfig {
            schema_version: 1,
            ..ProxyRuntimeConfig::default()
        };
        let message = config.validate().unwrap_err();
        assert!(
            message.contains("schema_version 1") && message.contains("expected 2"),
            "the error must name both versions so the UI can offer an update: {message}"
        );
    }

    #[test]
    fn an_empty_boundary_credential_id_is_refused() {
        let config = ProxyRuntimeConfig {
            inbound_access: InboundAccessConfig {
                credential_id: "  ".into(),
            },
            ..ProxyRuntimeConfig::default()
        };
        assert!(config.validate().is_err());
    }

    /// The default remote-agent / example config shape (explicit Linux/Bash
    /// executor) parses and round-trips exactly.
    #[test]
    fn config_with_explicit_linux_bash_executor_round_trips() {
        let raw = config_toml(
            "\n\
[execution_environment]\n\
platform = \"linux\"\n\
shell = \"bash\"\n\
supportsAndAnd = true\n\
hasUnixUtilities = true\n\
pathStyle = \"posix\"\n\
ampersandSemantics = \"posix-background\"\n",
        );
        let config = ProxyRuntimeConfig::from_toml_str(&raw).expect("explicit config must load");
        assert_eq!(
            config.execution_environment,
            ExecutionEnvironment::posix_reference(),
            "the explicit Linux/Bash table is the reference contract, not a guess"
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn grok_chat_rewrite_persist_failure_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.toml");
        let raw = format!(
            "{}\n\
[[models]]\n\
route_id = \"grok-cli\"\n\
catalog_id = \"vlm-grok\"\n\
name = \"Grok Build\"\n\
base_url = \"https://cli-chat-proxy.grok.com/v1\"\n\
provider_kind = \"grokCli\"\n\
auth_kind = \"grokSession\"\n\
wire = \"chat\"\n\
server_side_resume = false\n\
streaming = true\n\
reasoning = true\n\
vision = false\n\
upstream_model = \"grok-4.5\"\n",
            config_toml(
                "\n\
[execution_environment]\n\
platform = \"linux\"\n\
shell = \"bash\"\n\
supportsAndAnd = true\n\
hasUnixUtilities = true\n\
pathStyle = \"posix\"\n\
ampersandSemantics = \"posix-background\"\n",
            )
        );
        std::fs::write(&path, raw).unwrap();
        std::fs::create_dir(path.with_extension("toml.tmp")).unwrap();
        let config = ProxyRuntimeConfig::load(&path).expect("in-memory rewrite must still load");
        assert!(
            config.models.iter().any(|route| route.provider_kind
                == crate::route::RuntimeProviderKind::GrokCli
                && route.wire == crate::route::RuntimeWireFormat::Responses),
            "failed persist must not leave the in-memory Grok route on Chat"
        );
        let error = config
            .grok_rewrite_error
            .as_deref()
            .expect("persist failure must be recorded");
        assert!(
            error.contains("Grok Responses rewrite could not be persisted"),
            "{error}"
        );
    }
}
