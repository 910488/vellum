//! Renderer-safe production deployment planning.
//!
//! The renderer selects stable catalog ids and policy overrides. Raw proxy
//! TOML, catalog JSON and provider credentials are produced and retained on
//! the Rust side only.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use vellum_proxy_runtime::config::{
    ProxyRuntimeConfig, ProxyRuntimeIdentity, RuntimeCompactionPolicy, RuntimeRouteConfig,
};
use vellum_proxy_runtime::{ExecutionEnvironment, SELECTED_OFFICIAL_CREDENTIAL_ID};

use crate::error::{AppError, AppResult};
use crate::model::{AuthKind, ProviderKind};
use crate::remote::{RemoteAgentClient, RemoteHostManager};
use crate::state::AppState;

const PLAN_TTL_MINUTES: i64 = 30;

/// Remote keeps Canonical compaction; the local Desktop path does not.
///
/// That is not an inconsistency, it is the difference between the two
/// deployments. On Desktop a thread executes inside a Codex runtime that owns
/// its own compaction — Official natively, Enhanced through its local
/// compaction and context recovery — so Vellum compacting as well would be a
/// second compactor on the same context. A remote host has no such runtime:
/// it serves a plain Codex over the network, and if the remote runtime does
/// not compact, nothing does.
///
/// These values are the remote runtime's own defaults, deliberately built here
/// rather than resolved from a Vellum-wide policy — there is no such policy any
/// more, and reintroducing one to feed Remote is what this indirection avoids.
const REMOTE_THRESHOLD_PERCENT: u32 = 85;
const REMOTE_OUTPUT_RESERVE_TOKENS: u64 = 4_096;
const REMOTE_TOOL_RESERVE_TOKENS: u64 = 8_192;

fn remote_compaction_policy(provider_kind: ProviderKind) -> RuntimeCompactionPolicy {
    // Which engine runs is no longer part of the policy: it is a pure function
    // of the route's provider kind, so Official stays native passthrough and
    // every third-party route compacts with the embedded Codex 0.150 engine
    // without anything here having to say so. What remains is the threshold
    // this deployment publishes.
    RuntimeCompactionPolicy {
        threshold_percent: REMOTE_THRESHOLD_PERCENT,
        output_reserve_tokens: REMOTE_OUTPUT_RESERVE_TOKENS,
        tool_reserve_tokens: REMOTE_TOOL_RESERVE_TOKENS,
        // Grok gates on its own native runtime threshold.
        grok_threshold_percent: (provider_kind == ProviderKind::GrokCli)
            .then_some(REMOTE_THRESHOLD_PERCENT),
    }
}

/// Per-model `auto_compact_token_limit` projection for the remote catalog.
///
/// Official is native passthrough and Grok keeps its own native runtime gate,
/// so neither gets a projected limit — a slug with no entry is what
/// `third_party_catalog_entry` reads as "leave the field unset".
fn remote_compaction_policies_by_slug(
    routes: &[crate::model::Route],
    models: &[crate::model::ModelRoute],
) -> crate::catalog::CompactionPolicyBySlug {
    let mut policies = crate::catalog::CompactionPolicyBySlug::new();
    for model in models {
        let Some(route) = routes.iter().find(|route| route.id == model.route_id) else {
            continue;
        };
        if matches!(
            route.provider_kind,
            ProviderKind::GrokCli | ProviderKind::Official
        ) {
            continue;
        }
        policies.insert(
            model.catalog_id.clone(),
            remote_compaction_policy(route.provider_kind),
        );
    }
    policies
}
const RETIRED_REMOTE_ROUTE_IDS: &[&str] = &["weikuwu"];

/// The `config.toml` keys `vellum-remote-agent` actually manages on the
/// remote host during profile injection, surfaced here purely for display in
/// the deployment plan the renderer shows before apply. This must stay
/// identical (content and order) to `vellum_remote_agent::profile::MANAGED_PATHS`
/// — that constant is the actual three-way-lease source of truth; this one
/// exists only so Desktop doesn't need `vellum-remote-agent` as a normal
/// dependency to describe what it does. A regression test
/// (`managed_changes_stays_in_sync_with_the_agents_managed_paths`) asserts
/// the two are exactly equal, so a change to one without the other fails
/// closed instead of silently drifting the review screen out of sync with
/// what the agent's three-way lease actually covers.
const MANAGED_CHANGES: &[&str] = &[
    "model_provider",
    "openai_base_url",
    "model_catalog_json",
    "model_providers",
    "model_context_window",
    "model_auto_compact_token_limit",
    "model_auto_compact_token_limit_scope",
    "features.standalone_web_search",
    "features.remote_compaction_v2",
    "features.auto_compaction",
    "features.enable_request_compression",
    "features.image_generation",
    "cli_auth_credentials_store",
];

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemotePolicyOverrides {
    pub compaction_threshold_percent: Option<u32>,
    pub auto_review_enabled: Option<bool>,
    pub standalone_web_search: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RemoteModelSelection {
    #[serde(default)]
    pub catalog_ids: Vec<String>,
    #[serde(default)]
    pub policy: RemotePolicyOverrides,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteConfigDrift {
    pub desired_config_hash: String,
    pub remote_config_hash: Option<String>,
    pub config_changed: bool,
    pub desired_catalog_hash: String,
    pub remote_catalog_hash: Option<String>,
    pub catalog_changed: bool,
    /// Fingerprint of the Auto Review policy this plan carries (after any
    /// per-host `autoReviewEnabled` override). Desktop-side only — the
    /// remote agent does not expose a review-specific hash, so this is
    /// compared against this host's own last-*applied* fingerprint
    /// (`RemoteHostDesiredState::applied_review_policy_fingerprint`), not
    /// against anything the remote reports.
    pub review_policy_fingerprint: String,
    /// True when `review_policy_fingerprint` differs from what this host's
    /// last successful `apply()` used — i.e. local Auto Review settings
    /// changed since this host was last applied. Lets the Remote Manager UI
    /// show "Auto Review settings pending reapply" as a distinct reason from
    /// a generic `config_changed`.
    pub review_policy_changed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDeploymentPlan {
    pub plan_id: String,
    pub host_id: String,
    pub state: String,
    /// Monotonic per-host plan revision (M32).
    pub desired_revision: u64,
    /// Revision last observed as applied on the host (M32).
    pub observed_revision: u64,
    pub selected_catalog_ids: Vec<String>,
    pub selected_models: Vec<RemoteModelOption>,
    pub credential_requirements: Vec<RemoteCredentialRequirement>,
    pub drift: RemoteConfigDrift,
    /// Structured diff this plan applies versus the observed state (M32).
    pub public_diff: Vec<RemoteDiffEntry>,
    /// Capabilities this plan relies on, per selected model (M32).
    pub qualified_capabilities: Vec<RemoteQualifiedCapability>,
    /// What a restore would roll back (M32).
    pub rollback_summary: String,
    /// Content fingerprint of the desired configuration (M32).
    pub plan_hash: String,
    pub managed_changes: Vec<String>,
    pub restart_required: bool,
    pub blocked_reasons: Vec<String>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDiffEntry {
    pub path: String,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteQualifiedCapability {
    pub catalog_id: String,
    pub route_id: String,
    pub upstream_model: String,
    pub tool_calling: bool,
    pub probe_version: Option<String>,
    pub vision: bool,
    pub reasoning: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteModelOption {
    pub catalog_id: String,
    pub display_name: String,
    pub route_id: String,
    pub upstream_model: String,
    pub selected: bool,
    pub mandatory: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCredentialRequirement {
    pub credential_id: String,
    pub kind: String,
    pub available_on_desktop: bool,
    pub detached_qualified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredDeploymentPlan {
    public: RemoteDeploymentPlan,
    config_toml: String,
    catalog_json: String,
    image: String,
    route_ids: Vec<String>,
    /// The policy overrides this plan was built with, so a later `apply()`
    /// can persist them into `RemoteHostDesiredState` without needing the
    /// caller to resupply the original selection.
    #[serde(default)]
    policy_overrides: RemotePolicyOverrides,
}

fn bundled_proxy_image() -> AppResult<String> {
    #[cfg(test)]
    {
        // Unit tests deliberately avoid requiring a signed release bundle,
        // but the ignored live lane must exercise the exact image pinned by
        // the bundled manifest. Otherwise plan() stores `:local` and apply()
        // rejects its own plan as a manifest change before deployment.
        if std::env::var_os("VELLUM_LIVE_SSH_ALIAS").is_some() {
            return Ok(crate::remote::pinned_install::load_verified_manifest()?
                .proxy
                .image);
        }
        Ok("vellum-proxy:local".into())
    }
    #[cfg(not(test))]
    {
        Ok(crate::remote::pinned_install::load_verified_manifest()?
            .proxy
            .image)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteApplyResult {
    pub operation_id: String,
    pub completed_steps: Vec<String>,
    pub proxy: Value,
    pub native_adopt: Value,
    pub native_runtime: Value,
    pub state: String,
}

/// Drop `fetched_at` from a catalog bound for a remote host.
///
/// It records when *this Desktop* last polled the Official catalog, which is
/// a fact about Desktop, not about the catalog, and is meaningless on the
/// host. Leaving it in made the deployed catalog change every time the local
/// Codex refreshed its cache, even when nothing about the catalog moved --
/// the sibling `etag` says so directly, and was observed unchanged across
/// exactly such a refresh. Since the Agent records `catalogHash` over the
/// bytes it wrote, a new timestamp read as catalog drift, and catalog drift
/// makes the next apply re-adopt the profile and restart the native daemon,
/// dropping whatever session that host was carrying.
///
/// `etag` and `client_version` stay: the first is a content identity and the
/// second changes only when Codex itself does, both of which are reasons a
/// host genuinely should be re-applied. And the field is already optional in
/// this projection -- a Desktop with no Official cache emits a catalog
/// without it at all.
fn strip_desktop_poll_time(catalog: &mut Value) {
    if let Some(object) = catalog.as_object_mut() {
        object.remove("fetched_at");
    }
}

/// Whether `apply` has to (re)install the proxy image, read from the Agent's
/// `host.status`.
///
/// Which image a host is installed from is recorded on the **install**, not
/// on the running container. The Agent runs the container by a digest-pinned
/// reference, so `docker ps` -- and therefore `proxy.image` -- reports an
/// image *id* there (`ed4425383089`), never the tag a plan names
/// (`vellum-proxy:0.2.3-42169033ae28`). Comparing `proxy.image` to the
/// desired tag could not match on any host, so this was unconditionally
/// true: every apply to an already-converged host re-pushed the image,
/// stopped the proxy, temporarily restored the native lease and restarted the
/// daemon -- dropping whatever session that host was carrying, which is the
/// exact outcome docs/remote-acceptance.md 6 forbids.
///
/// Pure and host-agnostic, because the response shape is the whole question
/// and it should be answerable without a live agent.
fn proxy_install_needed(agent_status: &Value, desired_image: &str) -> bool {
    let present = agent_status
        .pointer("/proxy/present")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let installed = agent_status
        .pointer("/install/image")
        .and_then(Value::as_str);
    !present || installed != Some(desired_image)
}

/// Blocked reasons the observed `configuration.state` alone dictates.
/// `upgradeRequired`/`repairRequired` are deliberately absent here -- a
/// known-shape old or broken config is exactly what a normal deployment must
/// be able to converge, not block on. `missing`/`current` never block either.
/// Pure and host-agnostic so the state->reason mapping can be verified
/// without a live agent connection.
fn configuration_blocked_reasons(configuration_state: Option<&str>) -> Vec<String> {
    match configuration_state {
        Some("incompatible") => vec!["proxyConfigurationSchemaTooNew".into()],
        Some("unreadable") => vec!["proxyConfigurationUnreadable".into()],
        _ => Vec::new(),
    }
}

/// Whether the observed `configuration.state` alone is enough to force a
/// reconfigure, independent of the config-hash comparison. `upgradeRequired`/
/// `repairRequired` configs never carry a `configHash` (only `current` does),
/// so the hash comparison alone already forces this -- this exists as an
/// explicit, independently-verifiable second signal rather than a hidden
/// coupling to that invariant.
fn configuration_state_forces_reconfigure(configuration_state: Option<&str>) -> bool {
    matches!(
        configuration_state,
        Some("upgradeRequired" | "repairRequired")
    )
}

fn credential_is_detached_qualified(
    auth_kind: AuthKind,
    available_on_desktop: bool,
    agent_status: Option<&Value>,
) -> bool {
    match auth_kind {
        // apply() stages a desktop Bearer secret into the detached proxy, so
        // availability is qualification—not an error indicator.
        AuthKind::Bearer => available_on_desktop,
        AuthKind::GrokSession => agent_status
            .and_then(|status| status.pointer("/grok/detachedQualified"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        _ => true,
    }
}

pub fn plan(
    state: &AppState,
    host_id: &str,
    selection: RemoteModelSelection,
) -> AppResult<RemoteDeploymentPlan> {
    RemoteHostManager::resolve_target(state, host_id)?;
    let proxy_image = bundled_proxy_image()?;
    let routes = state.routes();
    let all_models = state.model_routes();
    let codex_paths = crate::codex::CodexPaths::discover(&state.data_root());
    let official_catalog = crate::catalog::read_official_catalog(&codex_paths.models_cache);
    let hidden_official_models = hidden_official_model_ids(official_catalog.as_ref());
    let allowed = routes
        .iter()
        .filter(|route| route.enabled && !RETIRED_REMOTE_ROUTE_IDS.contains(&route.id.as_str()))
        .map(|route| route.id.as_str())
        .collect::<BTreeSet<_>>();
    let available = all_models
        .iter()
        .filter(|model| allowed.contains(model.route_id.as_str()))
        .filter(|model| {
            !routes.iter().any(|route| {
                route.id == model.route_id
                    && route.provider_kind == ProviderKind::Official
                    && hidden_official_models.contains(&model.catalog_id)
            })
        })
        .filter(|model| remote_model_is_qualified(state, model))
        .cloned()
        .collect::<Vec<_>>();
    let requested = selection
        .catalog_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let use_defaults = requested.is_empty();
    let selected = available
        .iter()
        .filter(|model| {
            let official = routes.iter().any(|route| {
                route.id == model.route_id && route.provider_kind == ProviderKind::Official
            });
            official || use_defaults || requested.contains(model.catalog_id.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();

    let mut blocked = Vec::new();
    for unknown in requested.iter().filter(|id| {
        !available
            .iter()
            .any(|model| model.catalog_id.as_str() == **id)
    }) {
        blocked.push(format!("unknownOrUnqualifiedModel:{unknown}"));
    }
    if selected.is_empty() {
        blocked.push("noModelsSelected".into());
    }

    let selected_ids = selected
        .iter()
        .map(|model| model.catalog_id.clone())
        .collect::<BTreeSet<_>>();
    let selected_route_ids = selected
        .iter()
        .map(|model| model.route_id.clone())
        .collect::<BTreeSet<_>>();
    let selected_routes = routes
        .iter()
        .filter(|route| selected_route_ids.contains(&route.id))
        .cloned()
        .collect::<Vec<_>>();

    // Remote per-host `compaction_threshold_percent` overrides must be applied
    // before catalog generation, so the projected `auto_compact_token_limit`
    // matches the threshold the deployed runtime will actually gate on (see
    // the per-model override applied to `runtime.compaction_policy` below).
    // Invalid thresholds are left unapplied here — the per-model loop below
    // still records `invalidCompactionThreshold` in `blocked`, which fails
    // the plan regardless of what the catalog says.
    let mut compaction_policies = remote_compaction_policies_by_slug(&selected_routes, &selected);
    if let Some(threshold) = selection.policy.compaction_threshold_percent {
        if (10..=95).contains(&threshold) {
            for policy in compaction_policies.values_mut() {
                policy.threshold_percent = threshold;
            }
        }
    }
    let mut catalog = crate::catalog::catalog_json_with_official_and_compaction(
        &selected_routes,
        official_catalog.as_ref(),
        Some(&compaction_policies),
    );
    if let Some(models) = catalog.get_mut("models").and_then(Value::as_array_mut) {
        models.retain(|entry| {
            entry
                .get("slug")
                .and_then(Value::as_str)
                .is_some_and(|slug| selected_ids.contains(slug))
        });
    }
    crate::catalog::ensure_catalog_ready_for_codex(&mut catalog)?;
    strip_desktop_poll_time(&mut catalog);
    let catalog_json = serde_json::to_string_pretty(&catalog)
        .map_err(|error| AppError::Message(error.to_string()))?;
    let catalog_hash = hash(&catalog_json);
    let entries = catalog
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            entry
                .get("slug")
                .and_then(Value::as_str)
                .map(|slug| (slug.to_string(), entry.clone()))
        })
        .collect::<BTreeMap<_, _>>();

    let mut runtime_models = Vec::new();
    for model in &selected {
        let route = selected_routes
            .iter()
            .find(|route| route.id == model.route_id)
            .expect("selected route checked");
        let mut runtime = crate::proxy_runtime_bridge::runtime_route(state, route, model);
        // `runtime_route` resolves the local Desktop policy, where nothing
        // compacts. The remote runtime is the compactor on its own host, so the
        // policy is replaced here rather than inherited.
        runtime.compaction_policy = remote_compaction_policy(route.provider_kind);
        if route.provider_kind == ProviderKind::Official {
            // Stable indirection: absent selection preserves control account
            // A; selecting B changes only the private pointer on the remote
            // host, without a proxy config or Remote daemon restart.
            runtime.credential_id = Some(SELECTED_OFFICIAL_CREDENTIAL_ID.to_string());
        }
        if let Some(threshold) = selection.policy.compaction_threshold_percent {
            if !(10..=95).contains(&threshold) {
                blocked.push("invalidCompactionThreshold".into());
            } else {
                runtime.compaction_policy.threshold_percent = threshold;
            }
        }
        runtime_models.push(RuntimeRouteConfig {
            route_id: runtime.route_id,
            catalog_id: runtime.catalog_id.clone(),
            name: runtime.name,
            base_url: runtime.base_url,
            provider_kind: runtime.provider_kind,
            auth_kind: runtime.auth_kind,
            wire: runtime.wire,
            server_side_resume: runtime.server_side_resume,
            streaming: runtime.streaming,
            reasoning: runtime.reasoning,
            vision: runtime.vision,
            upstream_model: runtime.upstream_model,
            context_window: runtime.context_window,
            reasoning_capabilities: runtime.reasoning_capabilities,
            compaction_capabilities: runtime.compaction_capabilities,
            compaction_policy: runtime.compaction_policy,
            tool_capabilities: runtime.tool_capabilities,
            credential_id: runtime.credential_id,
            catalog_entry: entries
                .get(&runtime.catalog_id)
                .cloned()
                .map(toml_safe_json),
            chat_capabilities: runtime.chat_capabilities,
            // Both are resolved from Desktop state the host does not have --
            // a per-route setting and a per-model probe result -- so they
            // have to travel in the config or the host re-derives something
            // else. `deployment_desktop_parity` is what says so.
            insecure_http_policy: runtime.insecure_http_policy,
            access_mode: runtime.access_mode,
        });
    }

    let mut review = remote_review_settings(state.review_settings());
    if selection.policy.auto_review_enabled == Some(false) {
        review.on_edit = false;
        review.before_send = false;
        review.before_compact = false;
    }
    // Desktop-side fingerprint of the *effective* review policy this plan
    // carries (post-override), independent of the opaque whole-config hash
    // — see `RemoteConfigDrift::review_policy_fingerprint`. Shared with
    // `count_hosts_pending_review_reapply` so the per-host drift flag and
    // the aggregate pending count returned from a Settings save can never
    // disagree.
    let review_policy_fingerprint =
        review_policy_fingerprint_for(&review, selection.policy.auto_review_enabled);
    let mut config = ProxyRuntimeConfig {
        listen: "0.0.0.0:15721".parse().expect("static address"),
        data_dir: PathBuf::from("/var/lib/vellum/data"),
        history_dir: PathBuf::from("/var/lib/vellum/history"),
        log_dir: PathBuf::from("/var/log/vellum"),
        model_catalog_path: Some(PathBuf::from("/etc/vellum/model-catalog.json")),
        credentials_dir: Some(PathBuf::from("/run/secrets")),
        identity: ProxyRuntimeIdentity {
            install_id: "resolved-by-agent".into(),
            host_id: host_id.into(),
            image_version: "resolved-by-agent".into(),
            config_hash: String::new(),
            ..Default::default()
        },
        models: runtime_models,
        require_secrets: true,
        strict_upstream: true,
        review,
        execution_environment: ExecutionEnvironment::posix_reference(),
        ..ProxyRuntimeConfig::default()
    };
    let canonical = toml_edit::ser::to_string_pretty(&config)
        .map_err(|error| AppError::Message(format!("encode remote config failed: {error}")))?;
    let config_hash = hash(&canonical);
    config.identity.config_hash = config_hash.clone();
    let config_toml = toml_edit::ser::to_string_pretty(&config)
        .map_err(|error| AppError::Message(format!("encode remote config failed: {error}")))?;

    let target = RemoteHostManager::resolve_target(state, host_id)?;
    let agent_status = RemoteAgentClient::new(target).host_status();
    let (remote_config_hash, remote_catalog_hash, lease_state, native) = match &agent_status {
        Ok(status) => (
            status
                .pointer("/configuration/configHash")
                .and_then(Value::as_str)
                .map(str::to_owned),
            status
                .get("managedProfiles")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|profile| {
                    profile
                        .pointer("/profile/profileId")
                        .and_then(Value::as_str)
                        == Some("codex-app-native")
                })
                .and_then(|profile| profile.pointer("/lease/catalogHash"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            status
                .get("managedProfiles")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|profile| {
                    profile
                        .pointer("/profile/profileId")
                        .and_then(Value::as_str)
                        == Some("codex-app-native")
                })
                .and_then(|profile| profile.pointer("/lease/state"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            status.get("nativeCodex").cloned(),
        ),
        Err(error) => {
            blocked.push(format!("agentUnavailable:{error}"));
            (None, None, None, None)
        }
    };
    let desired_remote_config_hash = agent_status
        .as_ref()
        .ok()
        .and_then(|status| {
            let mut resolved = config.clone();
            resolved.identity.host_id = status.get("hostId")?.as_str()?.to_owned();
            resolved.identity.install_id =
                status.pointer("/install/installId")?.as_str()?.to_owned();
            resolved.identity.image_version =
                status.pointer("/install/image")?.as_str()?.to_owned();
            resolved.identity.config_hash.clear();
            toml::to_string_pretty(&resolved).ok().map(|raw| hash(&raw))
        })
        .unwrap_or_else(|| config_hash.clone());
    let official_selected = selected_routes
        .iter()
        .any(|route| route.provider_kind == ProviderKind::Official);
    if official_selected {
        match super::desktop_control_account_id(state) {
            None => blocked.push("desktopOfficialAccountMissing".into()),
            Some(account_id) => {
                if let Ok(target) = RemoteHostManager::resolve_target(state, host_id) {
                    match RemoteAgentClient::new(target).codex_account_status(Some(&account_id)) {
                        Ok(status)
                            if status.get("state").and_then(Value::as_str)
                                == Some("synchronized") => {}
                        Ok(status) => blocked.push(format!(
                            "officialAccount{}:{}",
                            match status.get("state").and_then(Value::as_str) {
                                Some("activationRequired") => "ActivationRequired",
                                _ => "PairingRequired",
                            },
                            account_id
                        )),
                        Err(error) => {
                            blocked.push(format!("officialAccountStatusUnavailable:{error}"))
                        }
                    }
                }
            }
        }
    }
    // Persisted-config state (schema 1/repairable/incompatible/unreadable),
    // never the strict-parse error `agentUnavailable` used to be for a
    // schema-1 host. `upgradeRequired`/`repairRequired` already force
    // `config_changed` below via the hash mismatch alone -- `configHash` is
    // only ever populated for `current` -- but stating it explicitly keeps
    // that guarantee from depending on `public_status` never changing what
    // it reports for those states. `incompatible` and `unreadable` block
    // instead: a newer schema than this build understands must never be
    // silently overwritten, and an unreadable file may not be safe to
    // replace at all (a permissions problem, not a stale one).
    let configuration_state = agent_status
        .as_ref()
        .ok()
        .and_then(|status| status.pointer("/configuration/state"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    blocked.extend(configuration_blocked_reasons(
        configuration_state.as_deref(),
    ));
    let config_changed = remote_config_hash.as_deref() != Some(desired_remote_config_hash.as_str())
        || configuration_state_forces_reconfigure(configuration_state.as_deref());
    let catalog_changed = remote_catalog_hash.as_deref() != Some(catalog_hash.as_str())
        || !matches!(lease_state.as_deref(), Some("active" | "restartRequired"));
    let restart_required =
        config_changed || catalog_changed || lease_state.as_deref() == Some("restartRequired");
    if native
        .as_ref()
        .and_then(|value| value.get("compatible"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        blocked.push("versionMismatch".into());
    }
    if native
        .as_ref()
        .and_then(|value| value.get("daemonRunning"))
        .and_then(Value::as_bool)
        == Some(true)
        && native
            .as_ref()
            .and_then(|value| value.get("restartSafe"))
            .and_then(Value::as_bool)
            != Some(true)
    {
        blocked.push("nativeDaemonAppOwned:runBootstrapAfterDisconnect".into());
    }

    let credential_requirements = selected_routes
        .iter()
        .filter(|route| matches!(route.auth_kind, AuthKind::Bearer | AuthKind::GrokSession))
        .map(|route| {
            let available = match route.auth_kind {
                AuthKind::Bearer => crate::credentials::load(&state.data_root(), &route.id)
                    .ok()
                    .flatten()
                    .is_some(),
                AuthKind::GrokSession => true,
                _ => true,
            };
            if !available {
                blocked.push(format!("credentialMissing:{}", route.id));
            }
            RemoteCredentialRequirement {
                credential_id: route.id.clone(),
                kind: match route.auth_kind {
                    AuthKind::GrokSession => "grokSession",
                    _ => "bearer",
                }
                .into(),
                available_on_desktop: available,
                detached_qualified: credential_is_detached_qualified(
                    route.auth_kind,
                    available,
                    agent_status.as_ref().ok(),
                ),
            }
        })
        .collect::<Vec<_>>();

    blocked.sort();
    blocked.dedup();
    let plan_id = ulid::Ulid::new().to_string();
    let expires_at = Utc::now() + Duration::minutes(PLAN_TTL_MINUTES);
    let desired_revision = super::desired_state::next_revision(&state.data_root(), host_id)?;
    let previous_desired_state = super::desired_state::load(&state.data_root(), host_id)?;
    let observed_revision = previous_desired_state.observed_revision;
    // Gated on `observed_revision > 0`: a host that has never been applied
    // has nothing to "reapply" yet (the general blocked/needs-configure
    // state already covers it), so the review-specific signal stays quiet
    // there instead of adding a redundant badge. For a host that *has* been
    // applied, a missing `applied_review_policy_fingerprint` means either a
    // pre-M9 apply (the field did not exist yet) or some other gap in our
    // own bookkeeping — either way, unknown must read as "pending reapply",
    // never be defaulted to "no drift".
    let review_policy_changed = observed_revision > 0
        && previous_desired_state
            .applied_review_policy_fingerprint
            .as_deref()
            != Some(review_policy_fingerprint.as_str());
    let qualified_capabilities = selected
        .iter()
        .map(|model| {
            let route = selected_routes
                .iter()
                .find(|route| route.id == model.route_id)
                .expect("selected route checked");
            let capability = if matches!(
                route.provider_kind,
                ProviderKind::GrokCli | ProviderKind::Official
            ) {
                None
            } else {
                route
                    .model_capabilities
                    .iter()
                    .find(|capability| capability.model.eq_ignore_ascii_case(&model.upstream_model))
            };
            RemoteQualifiedCapability {
                catalog_id: model.catalog_id.clone(),
                route_id: model.route_id.clone(),
                upstream_model: model.upstream_model.clone(),
                tool_calling: capability
                    .map(|cap| cap.tool_calling == Some(true))
                    .unwrap_or(matches!(
                        route.provider_kind,
                        ProviderKind::GrokCli | ProviderKind::Official
                    )),
                probe_version: capability
                    .and_then(|cap| cap.probe_version)
                    .map(|version| version.to_string()),
                vision: capability
                    .map(|cap| cap.vision == Some(true))
                    .unwrap_or(false),
                reasoning: capability
                    .map(|cap| cap.reasoning == Some(true))
                    .unwrap_or(false),
            }
        })
        .collect::<Vec<_>>();
    let plan_hash = plan_fingerprint(
        &selected_ids,
        &desired_remote_config_hash,
        &catalog_hash,
        &proxy_image,
    );
    let rollback_summary = "restore Vellum-managed proxy config, model catalog and credentials; user-modified conflicts are preserved".to_string();
    let public_diff = vec![
        RemoteDiffEntry {
            path: "model_catalog".into(),
            old_value: remote_catalog_hash.clone(),
            new_value: Some(catalog_hash.clone()),
        },
        RemoteDiffEntry {
            path: "proxy_config".into(),
            old_value: remote_config_hash.clone(),
            new_value: Some(desired_remote_config_hash.clone()),
        },
    ];
    let public = RemoteDeploymentPlan {
        plan_id: plan_id.clone(),
        host_id: host_id.into(),
        state: if blocked.is_empty() {
            "readyToApply"
        } else {
            "blocked"
        }
        .into(),
        desired_revision,
        observed_revision,
        selected_catalog_ids: selected
            .iter()
            .map(|model| model.catalog_id.clone())
            .collect(),
        selected_models: available
            .iter()
            .map(|model| RemoteModelOption {
                catalog_id: model.catalog_id.clone(),
                display_name: model.display_name.clone(),
                route_id: model.route_id.clone(),
                upstream_model: model.upstream_model.clone(),
                selected: selected_ids.contains(&model.catalog_id),
                mandatory: routes.iter().any(|route| {
                    route.id == model.route_id && route.provider_kind == ProviderKind::Official
                }),
            })
            .collect(),
        credential_requirements,
        drift: RemoteConfigDrift {
            desired_config_hash: desired_remote_config_hash,
            remote_config_hash: remote_config_hash.clone(),
            config_changed,
            desired_catalog_hash: catalog_hash,
            remote_catalog_hash,
            catalog_changed,
            review_policy_fingerprint: review_policy_fingerprint.clone(),
            review_policy_changed,
        },
        public_diff,
        qualified_capabilities,
        rollback_summary,
        plan_hash,
        managed_changes: MANAGED_CHANGES
            .iter()
            .map(|key| (*key).to_string())
            .collect(),
        restart_required,
        blocked_reasons: blocked,
        expires_at,
    };
    super::desired_state::save(
        &state.data_root(),
        &super::desired_state::RemoteHostDesiredState {
            host_id: host_id.into(),
            desired_revision,
            observed_revision,
            last_plan_id: Some(plan_id.clone()),
            selected_catalog_ids: selected_ids.iter().cloned().collect(),
            config_hash: Some(public.drift.desired_config_hash.clone()),
            catalog_hash: Some(public.drift.desired_catalog_hash.clone()),
            policy_overrides: selection.policy.clone(),
            review_policy_fingerprint: Some(review_policy_fingerprint),
            applied_review_policy_fingerprint: previous_desired_state
                .applied_review_policy_fingerprint,
        },
    )?;
    let stored = StoredDeploymentPlan {
        public: public.clone(),
        config_toml,
        catalog_json,
        image: proxy_image,
        route_ids: selected_route_ids.into_iter().collect(),
        policy_overrides: selection.policy,
    };
    store_plan(&state.data_root(), &stored)?;
    Ok(public)
}

/// Codex's official cache contains internal routing aliases and system-only
/// models alongside picker models. Respect the cache's visibility contract so
/// remote injection does not turn hidden implementation details into choices.
fn hidden_official_model_ids(catalog: Option<&Value>) -> BTreeSet<String> {
    catalog
        .and_then(|value| value.get("models"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|model| model.get("visibility").and_then(Value::as_str) == Some("hide"))
        .filter_map(|model| model.get("slug").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

/// Content fingerprint of the desired configuration: stable across plans
/// with identical selection and resolved hashes, so the UI can detect that a
/// previously applied plan is still current without comparing secrets.
fn plan_fingerprint(
    selected_ids: &BTreeSet<String>,
    config_hash: &str,
    catalog_hash: &str,
    image: &str,
) -> String {
    let payload = serde_json::json!({
        "selectedCatalogIds": selected_ids,
        "configHash": config_hash,
        "catalogHash": catalog_hash,
        "image": image,
    });
    hash(&serde_json::to_string(&payload).expect("fingerprint serialization cannot fail"))
}

/// Re-plan from the persisted per-host desired state (M30/M32 "Reapply
/// desired state"): the fresh plan uses the stored model selection and
/// exposes the current diff versus the observed host.
pub fn reapply_desired_state(state: &AppState, host_id: &str) -> AppResult<RemoteDeploymentPlan> {
    let desired = super::desired_state::load(&state.data_root(), host_id)?;
    plan(
        state,
        host_id,
        RemoteModelSelection {
            catalog_ids: desired.selected_catalog_ids,
            // Reuse this host's own last policy overrides
            // (`autoReviewEnabled`, compaction threshold, standalone web
            // search) instead of `RemotePolicyOverrides::default()` — a
            // reapply must restore the host's known state, not silently
            // reset every override it was deliberately given.
            policy: desired.policy_overrides,
        },
    )
}

/// Fingerprint of the Auto Review policy a host actually runs, shared
/// verbatim by [`plan`] and [`count_hosts_pending_review_reapply`] so the
/// per-host `RemoteConfigDrift::review_policy_changed` flag and the
/// aggregate `remoteHostsPendingReapply` count a Settings save reports can
/// never disagree.
///
/// When Auto Review is disabled for this host (`auto_review_enabled ==
/// Some(false)`), the fingerprint is a fixed canonical value — Auto
/// Review's own default `ReviewSettings`, not `review` with just its three
/// trigger flags zeroed. Every other field (`route_id`, `model`, `policy`,
/// `fallback_catalog_id`) is unreachable on a host that never triggers
/// Guardian, so a local change to which Provider/model Auto Review would
/// otherwise use must never register as drift on a host where it can never
/// run.
/// The Auto Review policy as a *remote* host would run it. Shared by
/// [`plan`] (what actually ships) and
/// [`count_hosts_pending_review_reapply`] (what is compared), because a
/// field that one strips and the other hashes makes the two disagree
/// forever -- the pending count would never clear.
///
/// `official_account_id` is the only field dropped. It is a local identity,
/// not policy: a remote host resolves Official auth from its own grant
/// under its own data mount -- one grant per host per account -- so this
/// machine's account id would name an account that host may not hold, and
/// every review there would fail the identity check instead of running.
/// Which model reviews is policy and does travel; who pays for it does not.
fn remote_review_settings(
    settings: crate::model::ReviewSettings,
) -> vellum_proxy_runtime::ReviewSettings {
    let mut review = crate::proxy_runtime_bridge::runtime_review_settings(settings);
    review.official_account_id = None;
    review
}

fn review_policy_fingerprint_for(
    review: &vellum_proxy_runtime::ReviewSettings,
    auto_review_enabled_override: Option<bool>,
) -> String {
    let canonical = if auto_review_enabled_override == Some(false) {
        vellum_proxy_runtime::ReviewSettings::default()
    } else {
        review.clone()
    };
    hash(&serde_json::to_string(&canonical).unwrap_or_default())
}

/// How many registered remote hosts have Auto Review settings pending
/// reapply, given the *current* local `ReviewSettings`.
///
/// Deliberately local-only: it never contacts a remote agent (unlike
/// [`plan`]), so a Settings save is never blocked on a flaky or unreachable
/// SSH host. Each host's own last policy override
/// (`RemoteHostDesiredState::policy_overrides`) is honored, so a host with
/// Auto Review deliberately disabled is not counted as drifted just because
/// local settings changed.
pub fn count_hosts_pending_review_reapply(state: &AppState) -> usize {
    let settings = state.review_settings();
    let Ok(hosts) = super::host_manager::RemoteHostManager::list_hosts(state) else {
        return 0;
    };
    hosts
        .into_iter()
        .filter(|host| {
            let desired = match super::desired_state::load(&state.data_root(), &host.id) {
                Ok(desired) => desired,
                Err(_) => return false,
            };
            // Mirrors `plan`'s `review_policy_changed` gate exactly: a host
            // that has never been *applied* has nothing to reapply yet, no
            // matter whether it has already been merely planned (`plan`
            // always records a `review_policy_fingerprint`, so checking that
            // field alone here would disagree with `plan`'s own
            // `observed_revision > 0` gate on a planned-but-not-yet-applied
            // host).
            if desired.observed_revision == 0 {
                return false;
            }
            let review = remote_review_settings(settings.clone());
            let fingerprint = review_policy_fingerprint_for(
                &review,
                desired.policy_overrides.auto_review_enabled,
            );
            desired.applied_review_policy_fingerprint.as_deref() != Some(fingerprint.as_str())
        })
        .count()
}

/// Only models verified by the *current* Codex-dialect probe with typed tool
/// support may be injected into the remote Codex catalog.
///
/// This is deliberately fail-closed: a missing capability entry (unknown),
/// a legacy or stale `probe_version`, and an explicit `tool_calling=false`
/// are all excluded. A model that merely answers chat text is not Codex-ready
/// and must never reach the remote catalog, even when the user asked for it
/// by catalog id. Official and Grok models are exempt: their typed tool
/// protocols are native Codex contracts, not third-party probe results.
fn remote_model_is_qualified(state: &AppState, model: &crate::model::ModelRoute) -> bool {
    let Some(route) = state
        .routes()
        .into_iter()
        .find(|route| route.id == model.route_id)
    else {
        return false;
    };
    if matches!(
        route.provider_kind,
        ProviderKind::GrokCli | ProviderKind::Official
    ) {
        return true;
    }
    let Some(capability) = route
        .model_capabilities
        .iter()
        .find(|capability| capability.model.eq_ignore_ascii_case(&model.upstream_model))
    else {
        return false;
    };
    capability.probe_version == Some(crate::probe::HARNESS_PROBE_VERSION)
        && capability.tool_calling == Some(true)
}

pub async fn apply(state: &AppState, host_id: &str, plan_id: &str) -> AppResult<RemoteApplyResult> {
    let stored = load_plan(&state.data_root(), plan_id)?;
    if stored.public.host_id != host_id {
        return Err(AppError::Message("DeploymentPlanHostMismatch".into()));
    }
    if stored.public.expires_at < Utc::now() {
        return Err(AppError::Message("DeploymentPlanExpired".into()));
    }
    if !stored.public.blocked_reasons.is_empty() {
        return Err(AppError::Message(format!(
            "DeploymentPlanBlocked: {}",
            stored.public.blocked_reasons.join(", ")
        )));
    }
    let client = RemoteAgentClient::new(RemoteHostManager::resolve_target(state, host_id)?);
    let operation_id = format!("native-{plan_id}");
    let mut steps = Vec::new();
    client.agent_version()?;
    client.codex_discover_native()?;
    steps.push("compatibility.verified".into());

    let agent_status = client.host_status()?;
    let status = agent_status
        .get("proxy")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let running = status
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let needs_install = proxy_install_needed(&agent_status, &stored.image);
    // `proxy.install` lays down the image's bootstrap/mock configuration.
    // Even when the previously running install already matched the desired
    // hash, an image replacement must re-apply the desired configuration or
    // the new container will start with only `vellum-mock` configured.
    let needs_configure = stored.public.drift.config_changed || needs_install;
    let needs_proxy_mutation = needs_install || needs_configure;
    let active_native_lease = agent_status
        .get("managedProfiles")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|profile| {
            profile
                .pointer("/profile/profileId")
                .and_then(Value::as_str)
                == Some("codex-app-native")
                && matches!(
                    profile.pointer("/lease/state").and_then(Value::as_str),
                    Some("active" | "restartRequired" | "recoveryRequired")
                )
        });
    if needs_proxy_mutation && active_native_lease {
        client.codex_restore_native(&format!("{operation_id}-restore-for-config"))?;
        steps.push("nativeAdopt.temporarilyRestored".into());
    }
    if running && needs_proxy_mutation {
        client.proxy_stop(&format!("{operation_id}-stop-for-config"))?;
        steps.push("proxy.stoppedForConfiguration".into());
    }
    if needs_install {
        let manifest = crate::remote::pinned_install::load_verified_manifest()?;
        if manifest.proxy.image != stored.image {
            return Err(AppError::Message("DeploymentProxyManifestChanged".into()));
        }
        let inventory = client.host_inventory_v2()?;
        let arch = inventory
            .pointer("/system/arch")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::Message("host inventory missing system.arch".into()))?;
        let artifact = crate::remote::pinned_install::proxy_artifact_for_arch(&manifest, arch)?;
        let staged = crate::remote::pinned_install::download_artifact(artifact)?;
        let bytes = fs::read(&staged)
            .map_err(|error| AppError::Message(format!("read proxy image archive: {error}")))?;
        let remote_path = format!("/tmp/vellum-proxy-stage-{}.tar", ulid::Ulid::new());
        let load_result = (|| -> AppResult<()> {
            client.stage_artifact(&remote_path, &bytes)?;
            client.proxy_load_image(
                &format!("{operation_id}-load-image"),
                &remote_path,
                &artifact.sha256,
                &stored.image,
            )?;
            Ok(())
        })();
        client.remove_remote_file(&remote_path);
        let _ = fs::remove_file(&staged);
        load_result?;
        steps.push("proxy.imageLoaded".into());
        client.proxy_install(&format!("{operation_id}-install"), &stored.image, None)?;
        steps.push("proxy.installed".into());
    }

    // The remote proxy refuses to start without its boundary key, so this has
    // to land before `proxy.configure`/`proxy.start` — and before the Codex
    // config on the host is pointed at it. Shared with every other Desktop
    // entry point that can start/install/restart the remote daemon (see
    // `crate::remote::provision_remote_boundary_key`), so a legacy host with
    // an active lease but a missing/stale key gets repaired here too.
    crate::remote::provision_remote_boundary_key(
        &client,
        &state.data_root(),
        &stored.public.host_id,
        &operation_id,
    )?;
    steps.push("credentials.boundaryReady".into());

    for route_id in &stored.route_ids {
        let Some(route) = state
            .routes()
            .into_iter()
            .find(|route| route.id == *route_id)
        else {
            return Err(AppError::Message(format!("route disappeared: {route_id}")));
        };
        let secret = match route.auth_kind {
            AuthKind::Bearer => crate::credentials::load(&state.data_root(), route_id)?
                .ok_or_else(|| AppError::Message(format!("credential missing: {route_id}")))?,
            AuthKind::GrokSession => {
                if client
                    .grok_status()
                    .ok()
                    .and_then(|status| status.get("detachedQualified").and_then(Value::as_bool))
                    .unwrap_or(false)
                {
                    continue;
                }
                let (account_id, credential) = state.grok_accounts().resolve_default().await?;
                super::host_manager::grok_remote_credential_secret_for_deployment(
                    credential,
                    &state.grok_accounts().account_home(&account_id)?,
                    &crate::grok_auth::grok_home(),
                )?
            }
            _ => continue,
        };
        client.credential_put(
            &format!("{operation_id}-credential-{route_id}"),
            route_id,
            &secret,
        )?;
    }
    steps.push("credentials.ready".into());
    if needs_configure {
        client.proxy_configure(&format!("{operation_id}-configure"), &stored.config_toml)?;
        steps.push("proxy.configured".into());
    }
    let proxy = if !running || needs_proxy_mutation {
        let value = client.proxy_start(&format!("{operation_id}-start"), Some(15721), None)?;
        steps.push("proxy.ready".into());
        value
    } else {
        steps.push("proxy.alreadyReady".into());
        status
    };
    let needs_native_apply = needs_proxy_mutation
        || stored.public.drift.catalog_changed
        || stored.public.restart_required;
    let (native_adopt, native_runtime) = if needs_native_apply {
        client.codex_plan_native_adopt()?;
        steps.push("nativeAdopt.planned".into());
        let adopt = client
            .codex_apply_native_adopt(&format!("{operation_id}-adopt"), &stored.catalog_json)?;
        steps.push("nativeAdopt.applied".into());
        let runtime = client.codex_restart_native(&format!("{operation_id}-restart"))?;
        steps.push("nativeDaemon.restarted".into());
        (adopt, runtime)
    } else {
        steps.push("nativeAdopt.alreadyActive".into());
        (
            serde_json::json!({"status": "alreadyActive"}),
            client.codex_discover_native()?,
        )
    };
    crate::remote::confirm_remote_boundary_key_consumers(
        &client,
        &state.data_root(),
        host_id,
        &[
            crate::proxy::BoundaryKeyConsumer::Proxy,
            crate::proxy::BoundaryKeyConsumer::NativeCodex,
        ],
        true,
    )?;
    // A schema-1/broken config was exactly why deployment couldn't reach this
    // far in the first place -- confirm the persisted config this apply just
    // wrote is actually load-bearing before reporting success, rather than
    // trusting that `proxy.configure` returning Ok implies it. Any mismatch
    // here is a real bug in this apply, not a state the caller should silently
    // paper over.
    if needs_configure {
        let final_status = client.host_status()?;
        let final_state = final_status
            .pointer("/configuration/state")
            .and_then(Value::as_str);
        if final_state != Some("current") {
            return Err(AppError::Message(format!(
                "DeploymentConfigurationNotCurrentAfterApply: {}",
                final_state.unwrap_or("unknown")
            )));
        }
        let final_schema_version = final_status.pointer("/configuration/schemaVersion");
        if final_schema_version != Some(&Value::from(2u32)) {
            return Err(AppError::Message(format!(
                "DeploymentConfigurationSchemaMismatchAfterApply: {final_schema_version:?}"
            )));
        }
    }
    super::desired_state::save(
        &state.data_root(),
        &super::desired_state::RemoteHostDesiredState {
            host_id: host_id.into(),
            desired_revision: stored.public.desired_revision,
            observed_revision: stored.public.desired_revision,
            last_plan_id: Some(plan_id.into()),
            selected_catalog_ids: stored.public.selected_catalog_ids.clone(),
            config_hash: Some(stored.public.drift.desired_config_hash.clone()),
            catalog_hash: Some(stored.public.drift.desired_catalog_hash.clone()),
            policy_overrides: stored.policy_overrides.clone(),
            review_policy_fingerprint: Some(stored.public.drift.review_policy_fingerprint.clone()),
            // This apply just succeeded, so desired and applied agree.
            applied_review_policy_fingerprint: Some(
                stored.public.drift.review_policy_fingerprint.clone(),
            ),
        },
    )?;
    Ok(RemoteApplyResult {
        operation_id,
        completed_steps: steps,
        proxy,
        native_adopt,
        native_runtime,
        state: "nativeActive".into(),
    })
}

fn plan_dir(root: &Path) -> PathBuf {
    root.join("remote-deployment-plans")
}

fn store_plan(root: &Path, plan: &StoredDeploymentPlan) -> AppResult<()> {
    let dir = plan_dir(root);
    fs::create_dir_all(&dir).map_err(|error| AppError::Message(error.to_string()))?;
    let path = dir.join(format!("{}.json", plan.public.plan_id));
    let bytes =
        serde_json::to_vec_pretty(plan).map_err(|error| AppError::Message(error.to_string()))?;
    atomic_write(&path, &bytes)
}

fn load_plan(root: &Path, plan_id: &str) -> AppResult<StoredDeploymentPlan> {
    if plan_id.is_empty()
        || !plan_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err(AppError::Message("invalid plan id".into()));
    }
    let bytes = fs::read(plan_dir(root).join(format!("{plan_id}.json")))
        .map_err(|error| AppError::Message(format!("deployment plan not found: {error}")))?;
    serde_json::from_slice(&bytes).map_err(|error| AppError::Message(error.to_string()))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> AppResult<()> {
    let tmp = path.with_extension(format!("tmp-{}", ulid::Ulid::new()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    let mut file = options
        .open(&tmp)
        .map_err(|error| AppError::Message(error.to_string()))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| AppError::Message(error.to_string()))?;
    fs::rename(&tmp, path).map_err(|error| AppError::Message(error.to_string()))
}

fn hash(raw: &str) -> String {
    hex::encode(Sha256::digest(raw.as_bytes()))
}

fn toml_safe_json(mut value: Value) -> Value {
    match &mut value {
        Value::Object(object) => {
            object.retain(|_, child| !child.is_null());
            for child in object.values_mut() {
                *child = toml_safe_json(child.take());
            }
        }
        Value::Array(values) => {
            values.retain(|child| !child.is_null());
            for child in values {
                *child = toml_safe_json(child.take());
            }
        }
        _ => {}
    }
    value
}

/// Remote/Desktop runtime-route parity, kept in its own file because it is a
/// suite rather than a case: a route matrix, both ends' answers, and a
/// field-by-field comparison. Declared here so it can reach `load_plan` and
/// the rest of this module's private surface.
#[cfg(test)]
#[path = "deployment_desktop_parity.rs"]
mod desktop_parity;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_bearer_secret_qualifies_the_detached_deployment() {
        assert!(credential_is_detached_qualified(
            AuthKind::Bearer,
            true,
            None
        ));
        assert!(!credential_is_detached_qualified(
            AuthKind::Bearer,
            false,
            Some(&serde_json::json!({"grok": {"detachedQualified": true}}))
        ));
    }

    /// Two catalogs that differ only in when Desktop last polled must reach
    /// the host as the same bytes, or the Agent's `catalogHash` moves and the
    /// next apply restarts a daemon that had nothing to pick up.
    #[test]
    fn a_refreshed_official_cache_is_not_catalog_drift_on_its_own() {
        let of = |fetched: &str| {
            serde_json::json!({
                "fetched_at": fetched,
                "etag": "W/\"1e10c2927ad7b0d7cddc841252b75cb1\"",
                "client_version": "0.146.0",
                "models": [{"slug": "vlm-1", "display_name": "one"}],
            })
        };
        let mut earlier = of("2026-09-04T09:18:56.437633Z");
        let mut later = of("2026-09-04T09:19:01.959763Z");
        assert_ne!(earlier, later);
        strip_desktop_poll_time(&mut earlier);
        strip_desktop_poll_time(&mut later);
        assert_eq!(
            earlier, later,
            "only the poll time differed, so the deployed catalog must not"
        );
        assert!(earlier.get("fetched_at").is_none());
        assert_eq!(
            earlier.get("etag"),
            Some(&serde_json::json!("W/\"1e10c2927ad7b0d7cddc841252b75cb1\"")),
            "the content identity must survive; it is a real change signal"
        );
        assert!(
            earlier.get("client_version").is_some(),
            "the Codex version must survive; it is a real change signal"
        );
    }

    /// The status shape a converged host really returns, copied from a dev
    /// host after a successful apply. `proxy.image` is the image id docker
    /// prints for a digest-pinned container; `install.image` is the reference
    /// the Agent recorded. Reading the first one made every apply reinstall.
    #[test]
    fn a_converged_host_does_not_need_the_proxy_image_installed_again() {
        let status = serde_json::json!({
            "proxy": {
                "present": true,
                "running": true,
                "ready": true,
                "containerId": "601b2eb22787",
                "image": "ed4425383089",
                "imageDigest": "sha256:ed44253830892682fa10b47e22e580ee88b727238a22dd591c4b57ae75e1ec23",
            },
            "install": {
                "installId": "install-01M1NTY78B19ZJEG6FHHYSVB79",
                "image": "vellum-proxy:0.2.3-42169033ae28",
            },
        });
        assert!(!proxy_install_needed(
            &status,
            "vellum-proxy:0.2.3-42169033ae28"
        ));
        assert!(
            proxy_install_needed(&status, "vellum-proxy:0.2.3-somethingelse"),
            "a different desired image must still install"
        );
    }

    #[test]
    fn an_absent_or_unrecorded_proxy_still_needs_installing() {
        assert!(proxy_install_needed(
            &serde_json::json!({"proxy": {"present": false}, "install": {"image": "vellum-proxy:x"}}),
            "vellum-proxy:x"
        ));
        // A running container the Agent has no install record for is not
        // evidence of anything; installing is the safe answer.
        assert!(proxy_install_needed(
            &serde_json::json!({"proxy": {"present": true, "image": "abc123"}}),
            "vellum-proxy:x"
        ));
    }

    #[test]
    fn upgrade_and_repair_required_states_never_block_but_do_force_reconfigure() {
        // A known-shape old or broken config is exactly what a normal
        // deployment must safely converge, not stall on.
        for state in ["upgradeRequired", "repairRequired"] {
            assert!(
                configuration_blocked_reasons(Some(state)).is_empty(),
                "{state} must not block deployment"
            );
            assert!(
                configuration_state_forces_reconfigure(Some(state)),
                "{state} must force a reconfigure"
            );
        }
    }

    #[test]
    fn incompatible_and_unreadable_states_block_with_specific_reasons() {
        assert_eq!(
            configuration_blocked_reasons(Some("incompatible")),
            vec!["proxyConfigurationSchemaTooNew".to_string()]
        );
        assert_eq!(
            configuration_blocked_reasons(Some("unreadable")),
            vec!["proxyConfigurationUnreadable".to_string()]
        );
        // Blocking must never look like "just reconfigure it" -- a future
        // schema must never be silently overwritten by an older Desktop, and
        // an unreadable file might be a permissions problem, not a stale one.
        assert!(!configuration_state_forces_reconfigure(Some(
            "incompatible"
        )));
        assert!(!configuration_state_forces_reconfigure(Some("unreadable")));
    }

    #[test]
    fn missing_current_and_unknown_states_neither_block_nor_force_reconfigure() {
        for state in [Some("missing"), Some("current"), None] {
            assert!(configuration_blocked_reasons(state).is_empty());
            assert!(!configuration_state_forces_reconfigure(state));
        }
    }

    #[test]
    fn stored_plan_keeps_raw_payload_out_of_public_contract() {
        let public = RemoteDeploymentPlan {
            plan_id: "p1".into(),
            host_id: "h1".into(),
            state: "readyToApply".into(),
            desired_revision: 1,
            observed_revision: 0,
            selected_catalog_ids: vec![],
            selected_models: vec![],
            credential_requirements: vec![],
            drift: RemoteConfigDrift {
                desired_config_hash: "a".into(),
                remote_config_hash: None,
                config_changed: true,
                desired_catalog_hash: "b".into(),
                remote_catalog_hash: None,
                catalog_changed: true,
                review_policy_fingerprint: "review-fp".into(),
                review_policy_changed: false,
            },
            public_diff: vec![],
            qualified_capabilities: vec![],
            rollback_summary: "restore Vellum-managed fields".into(),
            plan_hash: "plan-hash".into(),
            managed_changes: vec![],
            restart_required: false,
            blocked_reasons: vec![],
            expires_at: Utc::now(),
        };
        let value = serde_json::to_value(&public).unwrap();
        assert!(value.get("configToml").is_none());
        assert!(value.get("catalogJson").is_none());
        assert!(value.get("credentials").is_none());
    }

    #[test]
    fn plan_fingerprint_is_stable_for_identical_selection() {
        let mut selected = BTreeSet::new();
        selected.insert("qwen3.6".to_string());
        let first = plan_fingerprint(&selected, "cfg-a", "cat-b", "vellum-proxy:local");
        let second = plan_fingerprint(&selected, "cfg-a", "cat-b", "vellum-proxy:local");
        assert_eq!(first, second);
        assert_eq!(first.len(), 64);

        let changed = plan_fingerprint(&selected, "cfg-a", "cat-c", "vellum-proxy:local");
        assert_ne!(first, changed, "catalog change must alter the fingerprint");

        let mut other = BTreeSet::new();
        other.insert("other-model".to_string());
        let different = plan_fingerprint(&other, "cfg-a", "cat-b", "vellum-proxy:local");
        assert_ne!(
            first, different,
            "selection change must alter the fingerprint"
        );
    }

    #[test]
    fn hidden_official_models_are_not_remote_picker_candidates() {
        let catalog = serde_json::json!({
            "models": [
                {"slug": "gpt-5.6-sol", "visibility": "list"},
                {"slug": "gpt-5.6-sol-wm", "visibility": "hide"},
                {"slug": "codex-auto-review", "visibility": "hide"}
            ]
        });
        let hidden = hidden_official_model_ids(Some(&catalog));
        assert_eq!(
            hidden,
            BTreeSet::from([
                "codex-auto-review".to_string(),
                "gpt-5.6-sol-wm".to_string()
            ])
        );
        assert!(!hidden.contains("gpt-5.6-sol"));
    }

    #[test]
    fn e806_selection_fails_closed_on_missing_credential_and_excludes_secrets_and_weikuwu() {
        // Gate 1 planner contract: selecting the e806 route (id `cc`) must
        // surface `credentialMissing:cc` when the desktop store has no secret,
        // the public plan JSON must never contain a token, and the retired
        // `weikuwu` route must never appear in the model list or catalog.
        // The e806 model is current-probe qualified (typed tools verified), so
        // the only blocker left is the missing credential.
        let temp = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::with_data_dir(temp.path().to_path_buf());
        let qualified = || crate::model::ModelCapability {
            model: "qwen3.6".into(),
            tool_calling: Some(true),
            probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
            ..Default::default()
        };
        state.create_route(
            crate::model::CreateRouteInput {
                name: "cc".into(),
                base_url: "https://api.provider.example/v1".into(),
                model: "qwen3.6".into(),
                wire: crate::model::WireFormat::Chat,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(crate::model::ProviderKind::OpenAiCompatible),
                api_key: Some("sk-e806-redacted".into()),
                models: Some(vec!["qwen3.6".into()]),
                selected_models: Some(vec!["qwen3.6".into()]),
                context_window: Some(131_072),
                model_capabilities: vec![qualified()],
                catalog_scope: None,
            },
            crate::model::ProviderKind::OpenAiCompatible,
            crate::model::AuthKind::Bearer,
        );
        state.create_route(
            crate::model::CreateRouteInput {
                name: "weikuwu".into(),
                base_url: "https://weikuwu.invalid/v1".into(),
                model: "wu-model".into(),
                wire: crate::model::WireFormat::Chat,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(crate::model::ProviderKind::OpenAiCompatible),
                api_key: Some("sk-weikuwu-redacted".into()),
                models: Some(vec!["wu-model".into()]),
                selected_models: Some(vec!["wu-model".into()]),
                context_window: Some(131_072),
                model_capabilities: Vec::new(),
                catalog_scope: None,
            },
            crate::model::ProviderKind::OpenAiCompatible,
            crate::model::AuthKind::Bearer,
        );
        state
            .remote()
            .import_discovered_host(&crate::remote::discovery::RemoteHostCandidate {
                codex_host_id: None,
                vellum_host_id: "host-e806-test".into(),
                display_name: "e806 test".into(),
                ssh_alias: "127.0.0.1".into(),
                hostname: Some("127.0.0.1".into()),
                user: None,
                port: Some(1),
                source: "openSsh".into(),
                validated: false,
                validation_error: None,
            })
            .unwrap();

        let planned = plan(&state, "host-e806-test", RemoteModelSelection::default())
            .expect("planning must succeed even when the agent is unreachable");
        let reasons = planned.blocked_reasons.clone();
        assert!(
            reasons
                .iter()
                .any(|reason| reason == "credentialMissing:cc"),
            "e806 selection must gate on the missing cc credential: {reasons:?}"
        );
        assert!(
            reasons
                .iter()
                .any(|reason| reason.starts_with("agentUnavailable")),
            "unreachable agent must be reported, not guessed: {reasons:?}"
        );
        assert!(planned
            .credential_requirements
            .iter()
            .any(|requirement| requirement.credential_id == "cc"
                && requirement.kind == "bearer"
                && !requirement.available_on_desktop));
        assert!(planned
            .selected_models
            .iter()
            .all(|model| model.route_id != "weikuwu"));
        assert!(planned
            .selected_models
            .iter()
            .any(|model| model.route_id == "cc"));

        let public_json = serde_json::to_string(&planned).unwrap();
        assert!(
            !public_json.contains("sk-e806"),
            "public plan leaked a secret"
        );
        assert!(
            !public_json.to_ascii_lowercase().contains("weikuwu"),
            "public plan must never mention the retired route"
        );
    }

    /// The deployment plan's `managed_changes` is a display-only description
    /// of what the agent's three-way lease actually covers — a second,
    /// hand-maintained copy of `vellum_remote_agent::profile::MANAGED_PATHS`
    /// (Desktop doesn't depend on that crate in production, only in tests).
    /// If the two ever diverge, the review screen a user sees before
    /// applying a deployment plan understates or overstates what Vellum will
    /// actually touch on the remote host — exactly the kind of drift a
    /// prior review caught (the plan omitted the three global compaction
    /// keys and `features.auto_compaction` after P2 added them to the
    /// agent's managed set). Exact equality, not just "each is a superset of
    /// the other", so neither an added key nor a removed one can slip past
    /// silently in either direction.
    #[test]
    fn managed_changes_stays_in_sync_with_the_agents_managed_paths() {
        assert_eq!(
            MANAGED_CHANGES,
            vellum_remote_agent::profile::MANAGED_PATHS,
            "remote/deployment.rs::MANAGED_CHANGES has drifted from \
             vellum_remote_agent::profile::MANAGED_PATHS — update both together"
        );
    }

    #[test]
    fn remote_plan_carries_compaction_and_review_policy_into_the_runtime_config() {
        // M33 parity: the remote deployment config must be the same runtime
        // configuration surface as local (compaction policy + review settings
        // resolved by the shared proxy_runtime_bridge builders), so
        // compact / Auto Review / usage behave identically on the remote host.
        let temp = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::with_data_dir(temp.path().to_path_buf());
        let qualified = crate::model::ModelCapability {
            model: "parity-model".into(),
            tool_calling: Some(true),
            probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
            ..Default::default()
        };
        state.create_route(
            crate::model::CreateRouteInput {
                name: "parity".into(),
                base_url: "https://parity.invalid/v1".into(),
                model: "parity-model".into(),
                wire: crate::model::WireFormat::Chat,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(crate::model::ProviderKind::OpenAiCompatible),
                api_key: None,
                models: Some(vec!["parity-model".into()]),
                selected_models: Some(vec!["parity-model".into()]),
                context_window: Some(131_072),
                model_capabilities: vec![qualified],
                catalog_scope: None,
            },
            crate::model::ProviderKind::OpenAiCompatible,
            crate::model::AuthKind::None,
        );
        state
            .remote()
            .import_discovered_host(&crate::remote::discovery::RemoteHostCandidate {
                codex_host_id: None,
                vellum_host_id: "host-parity".into(),
                display_name: "parity".into(),
                ssh_alias: "127.0.0.1".into(),
                hostname: Some("127.0.0.1".into()),
                user: None,
                port: Some(1),
                source: "openSsh".into(),
                validated: false,
                validation_error: None,
            })
            .unwrap();

        let planned = plan(
            &state,
            "host-parity",
            RemoteModelSelection {
                catalog_ids: vec![],
                policy: RemotePolicyOverrides {
                    compaction_threshold_percent: Some(70),
                    auto_review_enabled: Some(false),
                    standalone_web_search: None,
                },
            },
        )
        .expect("planning must succeed");
        assert!(
            planned
                .blocked_reasons
                .iter()
                .all(|reason| reason.starts_with("agentUnavailable")
                    || reason.starts_with("officialAccountStatusUnavailable")
                    || reason == "desktopOfficialAccountMissing"),
            "only unreachable-agent, unavailable account status, and missing Desktop account gates may block: {:?}",
            planned.blocked_reasons
        );

        let stored = load_plan(&state.data_root(), &planned.plan_id).unwrap();
        let remote_runtime =
            vellum_proxy_runtime::ProxyRuntimeConfig::from_toml_str(&stored.config_toml)
                .map_err(|error| format!("stored remote config must be valid: {error}"))
                .unwrap();
        let parity_route = remote_runtime
            .models
            .iter()
            .find(|route| route.route_id == "parity")
            .expect("remote runtime config must carry the parity route");
        assert_eq!(
            parity_route.compaction_policy.threshold_percent, 70,
            "compaction policy must reach the remote runtime config"
        );
        let catalog: Value = serde_json::from_str(&stored.catalog_json).unwrap();
        let entry = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["slug"] == parity_route.catalog_id)
            .expect("catalog must contain the parity model");
        let context = entry["context_window"].as_u64().unwrap();
        assert!(entry["supports_parallel_tool_calls"].is_boolean());
        let expected_reserve = parity_route.compaction_policy.output_reserve_tokens
            + parity_route.compaction_policy.tool_reserve_tokens;
        assert_eq!(
            entry["auto_compact_token_limit"].as_u64(),
            Some(context * 70 / 100 - expected_reserve),
            "the 70% remote override must reach the projected catalog limit \
             the same way it reaches the runtime config, not stay stuck at \
             whatever the desktop default would have computed"
        );
        assert!(
            !remote_runtime.review.on_edit
                && !remote_runtime.review.before_send
                && !remote_runtime.review.before_compact,
            "auto-review disable must reach the remote runtime config"
        );
        assert!(remote_runtime
            .models
            .iter()
            .filter(|route| {
                route.provider_kind == vellum_proxy_runtime::RuntimeProviderKind::Official
            })
            .all(|route| {
                route.credential_id.as_deref()
                    == Some(vellum_proxy_runtime::SELECTED_OFFICIAL_CREDENTIAL_ID)
            }));
    }

    /// A reapply must restore this host's *own* last policy overrides, not
    /// reset them to `RemotePolicyOverrides::default()`. Before this fix,
    /// `reapply_desired_state` always planned with
    /// `RemotePolicyOverrides::default()`, so a host with Auto Review
    /// deliberately disabled would have it silently re-enabled on the very
    /// next reapply.
    #[test]
    fn reapply_desired_state_preserves_this_hosts_policy_overrides() {
        let temp = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::with_data_dir(temp.path().to_path_buf());
        let qualified = crate::model::ModelCapability {
            model: "parity-model".into(),
            tool_calling: Some(true),
            probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
            ..Default::default()
        };
        state.create_route(
            crate::model::CreateRouteInput {
                name: "parity".into(),
                base_url: "https://parity.invalid/v1".into(),
                model: "parity-model".into(),
                wire: crate::model::WireFormat::Chat,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(crate::model::ProviderKind::OpenAiCompatible),
                api_key: None,
                models: Some(vec!["parity-model".into()]),
                selected_models: Some(vec!["parity-model".into()]),
                context_window: Some(131_072),
                model_capabilities: vec![qualified],
                catalog_scope: None,
            },
            crate::model::ProviderKind::OpenAiCompatible,
            crate::model::AuthKind::None,
        );
        state
            .remote()
            .import_discovered_host(&crate::remote::discovery::RemoteHostCandidate {
                codex_host_id: None,
                vellum_host_id: "host-parity".into(),
                display_name: "parity".into(),
                ssh_alias: "127.0.0.1".into(),
                hostname: Some("127.0.0.1".into()),
                user: None,
                port: Some(1),
                source: "openSsh".into(),
                validated: false,
                validation_error: None,
            })
            .unwrap();

        let first = plan(
            &state,
            "host-parity",
            RemoteModelSelection {
                catalog_ids: vec![],
                policy: RemotePolicyOverrides {
                    compaction_threshold_percent: None,
                    auto_review_enabled: Some(false),
                    standalone_web_search: None,
                },
            },
        )
        .expect("first plan must succeed");
        assert!(
            !first.drift.review_policy_changed,
            "a host that has never been applied has nothing to reapply yet -- \
             the general blocked/needs-configure state already covers it, so \
             the review-specific signal must stay quiet instead of adding a \
             redundant badge"
        );

        let reapplied = reapply_desired_state(&state, "host-parity").expect("reapply must succeed");
        let stored = load_plan(&state.data_root(), &reapplied.plan_id).unwrap();
        let remote_runtime =
            vellum_proxy_runtime::ProxyRuntimeConfig::from_toml_str(&stored.config_toml)
                .expect("stored remote config must be valid");
        assert!(
            !remote_runtime.review.before_send,
            "reapply must keep this host's Auto Review disabled, not silently \
             reset it to RemotePolicyOverrides::default()"
        );
        assert_eq!(
            reapplied.drift.review_policy_fingerprint, first.drift.review_policy_fingerprint,
            "the reapplied plan carries the identical effective review policy"
        );
    }

    /// A host applied by a pre-M9 build has `observed_revision > 0` (it has
    /// genuinely been deployed) but no `applied_review_policy_fingerprint`
    /// (the field did not exist yet). Unknown must read as "pending
    /// reapply", never be defaulted to "no drift" -- the desktop cannot
    /// prove the remote's actual review policy matches, so silence would be
    /// a false negative.
    #[test]
    fn a_legacy_applied_host_without_a_recorded_fingerprint_shows_pending_not_no_drift() {
        let temp = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::with_data_dir(temp.path().to_path_buf());
        state
            .remote()
            .import_discovered_host(&crate::remote::discovery::RemoteHostCandidate {
                codex_host_id: None,
                vellum_host_id: "host-legacy".into(),
                display_name: "legacy".into(),
                ssh_alias: "127.0.0.1".into(),
                hostname: Some("127.0.0.1".into()),
                user: None,
                port: Some(1),
                source: "openSsh".into(),
                validated: false,
                validation_error: None,
            })
            .unwrap();
        // Simulate a pre-M9 apply: observed_revision advanced, but no
        // review policy fingerprint was ever recorded.
        super::super::desired_state::save(
            &state.data_root(),
            &super::super::desired_state::RemoteHostDesiredState {
                host_id: "host-legacy".into(),
                desired_revision: 1,
                observed_revision: 1,
                ..Default::default()
            },
        )
        .unwrap();

        let planned = plan(
            &state,
            "host-legacy",
            RemoteModelSelection {
                catalog_ids: vec![],
                policy: RemotePolicyOverrides::default(),
            },
        )
        .expect("plan must succeed");
        assert!(
            planned.drift.review_policy_changed,
            "a legacy applied host with no recorded fingerprint must be \
             treated as pending reapply, not silently as no drift"
        );
    }

    fn state_with_registered_host(host_id: &str) -> (tempfile::TempDir, crate::state::AppState) {
        let temp = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::with_data_dir(temp.path().to_path_buf());
        state
            .remote()
            .import_discovered_host(&crate::remote::discovery::RemoteHostCandidate {
                codex_host_id: None,
                vellum_host_id: host_id.into(),
                display_name: host_id.into(),
                ssh_alias: "127.0.0.1".into(),
                hostname: Some("127.0.0.1".into()),
                user: None,
                port: Some(1),
                source: "openSsh".into(),
                validated: false,
                validation_error: None,
            })
            .unwrap();
        (temp, state)
    }

    /// Regression matrix for `count_hosts_pending_review_reapply`: it must
    /// agree with `plan`'s own `review_policy_changed` gate exactly (the
    /// same `observed_revision > 0` condition, the same canonical-when-
    /// disabled fingerprint), or a Settings save's `remoteHostsPendingReapply`
    /// count and an individual host's plan-card drift badge could disagree.
    #[test]
    fn count_hosts_pending_review_reapply_matches_plans_drift_gate_case_by_case() {
        // Registered but never planned: no desired state file exists at
        // all, so `observed_revision` defaults to 0 -- not counted.
        let (_temp, state) = state_with_registered_host("never-planned");
        assert_eq!(count_hosts_pending_review_reapply(&state), 0);

        // Planned, never applied: `plan` always records a
        // `review_policy_fingerprint`, but `observed_revision` stays 0 until
        // an actual `apply` succeeds -- still not counted.
        let (_temp, state) = state_with_registered_host("planned-not-applied");
        super::super::desired_state::save(
            &state.data_root(),
            &super::super::desired_state::RemoteHostDesiredState {
                host_id: "planned-not-applied".into(),
                desired_revision: 1,
                observed_revision: 0,
                review_policy_fingerprint: Some("plan-only-fp".into()),
                applied_review_policy_fingerprint: None,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(count_hosts_pending_review_reapply(&state), 0);

        // Legacy applied host, no recorded fingerprint: unknown must read
        // as pending, never as "no drift".
        let (_temp, state) = state_with_registered_host("legacy-applied");
        super::super::desired_state::save(
            &state.data_root(),
            &super::super::desired_state::RemoteHostDesiredState {
                host_id: "legacy-applied".into(),
                desired_revision: 1,
                observed_revision: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(count_hosts_pending_review_reapply(&state), 1);

        // Picking a local ChatGPT account to bill reviews to is not a
        // policy change for any remote host -- that host bills its own
        // grant either way -- so an applied host must stay settled instead
        // of being reported as pending forever.
        let (_temp, state) = state_with_registered_host("applied-billing-account");
        let settled =
            review_policy_fingerprint_for(&remote_review_settings(state.review_settings()), None);
        super::super::desired_state::save(
            &state.data_root(),
            &super::super::desired_state::RemoteHostDesiredState {
                host_id: "applied-billing-account".into(),
                desired_revision: 1,
                observed_revision: 1,
                review_policy_fingerprint: Some(settled.clone()),
                applied_review_policy_fingerprint: Some(settled),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(count_hosts_pending_review_reapply(&state), 0);
        let mut billed = state.review_settings();
        billed.official_account_id = Some("acct-review-only".into());
        state.set_review_settings(billed).unwrap();
        assert_eq!(count_hosts_pending_review_reapply(&state), 0);
        // ...and the id itself never reaches the projection a host runs.
        assert_eq!(
            remote_review_settings(state.review_settings()).official_account_id,
            None
        );

        // Applied with a fingerprint that matches current local settings
        // exactly: no drift.
        let (_temp, state) = state_with_registered_host("applied-matching");
        let current_review = remote_review_settings(state.review_settings());
        let matching_fingerprint = review_policy_fingerprint_for(&current_review, None);
        super::super::desired_state::save(
            &state.data_root(),
            &super::super::desired_state::RemoteHostDesiredState {
                host_id: "applied-matching".into(),
                desired_revision: 1,
                observed_revision: 1,
                review_policy_fingerprint: Some(matching_fingerprint.clone()),
                applied_review_policy_fingerprint: Some(matching_fingerprint),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(count_hosts_pending_review_reapply(&state), 0);

        // Applied, then local policy changed since: the recorded fingerprint
        // no longer matches what would be planned now -- counted.
        let (_temp, state) = state_with_registered_host("applied-then-changed");
        super::super::desired_state::save(
            &state.data_root(),
            &super::super::desired_state::RemoteHostDesiredState {
                host_id: "applied-then-changed".into(),
                desired_revision: 1,
                observed_revision: 1,
                review_policy_fingerprint: Some("stale-fp-before-local-change".into()),
                applied_review_policy_fingerprint: Some("stale-fp-before-local-change".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let changed_routes = state.create_route(
            crate::model::CreateRouteInput {
                name: "Changed Route".into(),
                base_url: "https://changed-route.invalid/v1".into(),
                model: "changed-model".into(),
                wire: crate::model::WireFormat::Responses,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(crate::model::ProviderKind::OpenAiCompatible),
                api_key: None,
                models: Some(vec!["changed-model".into()]),
                selected_models: Some(vec!["changed-model".into()]),
                context_window: Some(128_000),
                model_capabilities: Vec::new(),
                catalog_scope: None,
            },
            crate::model::ProviderKind::OpenAiCompatible,
            crate::model::AuthKind::None,
        );
        let changed_route_id = changed_routes
            .iter()
            .find(|route| route.name == "Changed Route")
            .unwrap()
            .id
            .clone();
        state
            .set_review_settings(crate::model::ReviewSettings {
                on_edit: false,
                before_send: true,
                before_compact: false,
                route_id: changed_route_id,
                model: "changed-model".into(),
                policy: Some(crate::model::ReviewPolicy::Always),
                fallback_catalog_id: None,
                official_account_id: None,
            })
            .unwrap();
        assert_eq!(count_hosts_pending_review_reapply(&state), 1);

        // This host has Auto Review explicitly disabled
        // (`autoReviewEnabled: false`): its applied fingerprint is the fixed
        // canonical "disabled" value. A local Auto Review policy change
        // must never register as drift here -- Guardian can never trigger
        // on this host regardless of which Provider/model local settings
        // would otherwise select.
        let (_temp, state) = state_with_registered_host("remote-disabled");
        let canonical_disabled_fingerprint = review_policy_fingerprint_for(
            &vellum_proxy_runtime::ReviewSettings::default(),
            Some(false),
        );
        super::super::desired_state::save(
            &state.data_root(),
            &super::super::desired_state::RemoteHostDesiredState {
                host_id: "remote-disabled".into(),
                desired_revision: 1,
                observed_revision: 1,
                review_policy_fingerprint: Some(canonical_disabled_fingerprint.clone()),
                applied_review_policy_fingerprint: Some(canonical_disabled_fingerprint),
                policy_overrides: RemotePolicyOverrides {
                    compaction_threshold_percent: None,
                    auto_review_enabled: Some(false),
                    standalone_web_search: None,
                },
                ..Default::default()
            },
        )
        .unwrap();
        // A real, resolvable route/model: `set_review_settings` now always
        // eagerly validates (no policy is exempt from it any more), so the
        // point this proves is narrower than before -- any local change to
        // which Provider/model Auto Review would use is still irrelevant on
        // a host where Guardian never runs (`auto_review_enabled: false`
        // above pins its fingerprint to the fixed canonical "disabled"
        // value regardless).
        let routes = state.create_route(
            crate::model::CreateRouteInput {
                name: "Some Other Route".into(),
                base_url: "https://some-other-route.invalid/v1".into(),
                model: "some-other-model".into(),
                wire: crate::model::WireFormat::Responses,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(crate::model::ProviderKind::OpenAiCompatible),
                api_key: None,
                models: Some(vec!["some-other-model".into()]),
                selected_models: Some(vec!["some-other-model".into()]),
                context_window: Some(128_000),
                model_capabilities: Vec::new(),
                catalog_scope: None,
            },
            crate::model::ProviderKind::OpenAiCompatible,
            crate::model::AuthKind::None,
        );
        let some_other_route_id = routes
            .iter()
            .find(|route| route.name == "Some Other Route")
            .unwrap()
            .id
            .clone();
        state
            .set_review_settings(crate::model::ReviewSettings {
                on_edit: true,
                before_send: true,
                before_compact: true,
                route_id: some_other_route_id,
                model: "some-other-model".into(),
                policy: Some(crate::model::ReviewPolicy::Always),
                fallback_catalog_id: None,
                official_account_id: None,
            })
            .unwrap();
        assert_eq!(count_hosts_pending_review_reapply(&state), 0);
    }

    #[test]
    fn remote_deployment_only_selects_current_probe_typed_tool_models() {
        // Qualification matrix: unknown capability, stale probe version,
        // explicit tool_calling=false and current-probe tool_calling=true.
        // Only the last one may enter the deployment model list and catalog.
        let temp = tempfile::tempdir().unwrap();
        let state = crate::state::AppState::with_data_dir(temp.path().to_path_buf());
        let make_route =
            |name: &str, model: &str, capability: Option<crate::model::ModelCapability>| {
                state.create_route(
                    crate::model::CreateRouteInput {
                        name: name.into(),
                        base_url: format!("https://{name}.invalid/v1"),
                        model: model.into(),
                        wire: crate::model::WireFormat::Chat,
                        streaming: true,
                        reasoning: true,
                        server_side_resume: false,
                        provider_kind: Some(crate::model::ProviderKind::OpenAiCompatible),
                        api_key: None,
                        models: Some(vec![model.into()]),
                        selected_models: Some(vec![model.into()]),
                        context_window: Some(131_072),
                        model_capabilities: capability.into_iter().collect(),
                        catalog_scope: None,
                    },
                    crate::model::ProviderKind::OpenAiCompatible,
                    crate::model::AuthKind::None,
                );
            };
        make_route("unknown", "unknown-model", None);
        make_route(
            "stale",
            "stale-model",
            Some(crate::model::ModelCapability {
                model: "stale-model".into(),
                tool_calling: Some(true),
                probe_version: Some(crate::probe::HARNESS_PROBE_VERSION - 1),
                ..Default::default()
            }),
        );
        make_route(
            "chatonly",
            "chatonly-model",
            Some(crate::model::ModelCapability {
                model: "chatonly-model".into(),
                tool_calling: Some(false),
                probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                ..Default::default()
            }),
        );
        make_route(
            "qualified",
            "qualified-model",
            Some(crate::model::ModelCapability {
                model: "qualified-model".into(),
                tool_calling: Some(true),
                probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                ..Default::default()
            }),
        );
        // Grok is exempt from the probe gate: its typed tool protocol is the
        // official CLI contract, verified by the detached GrokSession.
        state.create_route(
            crate::model::CreateRouteInput {
                name: "grok-cli".into(),
                base_url: "https://cli-chat-proxy.grok.com/v1".into(),
                model: "grok-4.5".into(),
                wire: crate::model::WireFormat::Chat,
                streaming: true,
                reasoning: true,
                server_side_resume: false,
                provider_kind: Some(crate::model::ProviderKind::GrokCli),
                api_key: None,
                models: Some(vec!["grok-4.5".into()]),
                selected_models: Some(vec!["grok-4.5".into()]),
                context_window: Some(200_000),
                model_capabilities: Vec::new(),
                catalog_scope: None,
            },
            crate::model::ProviderKind::GrokCli,
            crate::model::AuthKind::GrokSession,
        );
        state
            .remote()
            .import_discovered_host(&crate::remote::discovery::RemoteHostCandidate {
                codex_host_id: None,
                vellum_host_id: "host-qualification".into(),
                display_name: "qualification".into(),
                ssh_alias: "127.0.0.1".into(),
                hostname: Some("127.0.0.1".into()),
                user: None,
                port: Some(1),
                source: "openSsh".into(),
                validated: false,
                validation_error: None,
            })
            .unwrap();

        let planned = plan(
            &state,
            "host-qualification",
            RemoteModelSelection::default(),
        )
        .expect("planning must succeed even when the agent is unreachable");
        let listed = planned
            .selected_models
            .iter()
            .map(|model| model.route_id.as_str())
            .collect::<Vec<_>>();
        assert!(
            !listed.contains(&"unknown")
                && !listed.contains(&"stale")
                && !listed.contains(&"chatonly"),
            "unqualified models must never be listed for deployment: {listed:?}"
        );
        assert!(
            listed.contains(&"qualified"),
            "current-probe typed-tool model must be listed: {listed:?}"
        );
        assert!(
            listed.contains(&"grok-cli"),
            "Grok official CLI route must stay deployable: {listed:?}"
        );
        assert!(
            planned.selected_models.iter().any(|model| model.mandatory),
            "official models must be listed and selected as mandatory"
        );
        assert!(planned.selected_models.iter().all(|model| {
            model.mandatory
                || (model.catalog_id.starts_with("vlm-")
                    && !model.catalog_id.contains("unknown")
                    && !model.catalog_id.contains("stale")
                    && !model.catalog_id.contains("chatonly"))
        }));
        // Requesting an unqualified model by catalog id must fail closed with
        // the explicit unknownOrUnqualifiedModel blocker.
        let unqualified_id = state
            .model_routes()
            .into_iter()
            .find(|model| model.route_id == "stale")
            .map(|model| model.catalog_id)
            .expect("stale route model");
        let blocked = plan(
            &state,
            "host-qualification",
            RemoteModelSelection {
                catalog_ids: vec![unqualified_id.clone()],
                ..RemoteModelSelection::default()
            },
        )
        .expect("planning must succeed even when the agent is unreachable");
        assert!(
            blocked
                .blocked_reasons
                .iter()
                .any(|reason| reason == &format!("unknownOrUnqualifiedModel:{unqualified_id}")),
            "requesting a stale model must fail closed: {:?}",
            blocked.blocked_reasons
        );
        assert!(
            blocked
                .selected_models
                .iter()
                .filter(|model| !model.mandatory)
                .all(|model| !model.selected),
            "only mandatory official models may remain selected: {:?}",
            blocked.selected_catalog_ids
        );
        assert!(
            blocked
                .selected_models
                .iter()
                .all(|model| model.mandatory || !model.selected),
            "the stale model must not be marked selected"
        );
    }

    #[test]
    #[ignore = "exports secret-free inputs for the isolated Jetson live-smoke harness"]
    fn export_isolated_live_smoke_inputs() {
        assert_eq!(
            std::env::var("VELLUM_LIVE_EXPORT_INPUTS").as_deref(),
            Ok("YES"),
            "set VELLUM_LIVE_EXPORT_INPUTS=YES explicitly"
        );
        let plan_path = std::path::PathBuf::from(
            std::env::var("VELLUM_LIVE_STORED_PLAN").expect("stored deployment plan path"),
        );
        let output_dir = std::path::PathBuf::from(
            std::env::var("VELLUM_LIVE_EXPORT_DIR").expect("live export directory"),
        );
        let repository_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("repository root");
        let allowed_root = repository_root.join("target/live-smoke");
        std::fs::create_dir_all(&output_dir).expect("create live export directory");
        let resolved_output = output_dir
            .canonicalize()
            .expect("canonical export directory");
        let resolved_allowed = allowed_root
            .canonicalize()
            .expect("canonical target/live-smoke directory");
        assert!(
            resolved_output.starts_with(&resolved_allowed),
            "live inputs must stay under {}",
            resolved_allowed.display()
        );

        let stored: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&plan_path).expect("read stored deployment plan"),
        )
        .expect("parse stored deployment plan");
        let raw_config = stored
            .get("configToml")
            .and_then(serde_json::Value::as_str)
            .expect("stored configToml");
        let mut config = raw_config.to_string();
        if let Ok(compaction_model) = std::env::var("VELLUM_LIVE_COMPACTION_MODEL") {
            let threshold_percent = std::env::var("VELLUM_LIVE_COMPACTION_THRESHOLD_PERCENT")
                .unwrap_or_else(|_| "1".into())
                .parse::<u32>()
                .expect("valid live compaction threshold percent");
            assert!((1..=100).contains(&threshold_percent));
            let mut runtime = vellum_proxy_runtime::ProxyRuntimeConfig::from_toml_str(&config)
                .expect("parse live proxy config");
            let route = runtime
                .models
                .iter_mut()
                .find(|route| route.catalog_id == compaction_model)
                .expect("live compaction model exists in stored plan");
            route.compaction_policy.threshold_percent = threshold_percent;
            config = toml::to_string_pretty(&runtime).expect("encode live proxy config");
        }
        let catalog = stored
            .get("catalogJson")
            .and_then(serde_json::Value::as_str)
            .expect("stored catalogJson");
        assert!(
            !config.to_ascii_lowercase().contains("weikuwu")
                && !catalog.to_ascii_lowercase().contains("weikuwu"),
            "retired route must not enter live inputs"
        );
        let credentials = stored
            .get("routeIds")
            .and_then(serde_json::Value::as_array)
            .expect("stored routeIds")
            .iter()
            .map(|route| {
                serde_json::json!({
                    "credentialId": route.as_str().expect("route id")
                })
            })
            .collect::<Vec<_>>();
        assert!(
            credentials.iter().all(|credential| {
                credential
                    .get("credentialId")
                    .and_then(serde_json::Value::as_str)
                    != Some("weikuwu")
            }),
            "retired credential must not enter live inputs"
        );

        std::fs::write(resolved_output.join("proxy.toml"), &config)
            .expect("write live proxy config");
        std::fs::write(resolved_output.join("catalog.json"), catalog).expect("write live catalog");
        std::fs::write(
            resolved_output.join("credentials.json"),
            serde_json::to_vec_pretty(&credentials).expect("encode credential metadata"),
        )
        .expect("write live credential metadata");
        if let Ok(bytes) = std::env::var("VELLUM_LIVE_COMPACTION_PROMPT_BYTES") {
            let bytes = bytes
                .parse::<usize>()
                .expect("valid compaction prompt size");
            assert!((16_000..=2_000_000).contains(&bytes));
            let mut prompt = String::from(
                "Remember checkpoint marker VELLUM_COMPACT_CHECKPOINT. Read the numbered public fixture records and acknowledge the checkpoint when finished.\n",
            );
            let mut index = 0usize;
            while prompt.len() < bytes {
                prompt.push_str(&format!(
                    "Record {index:06}: alpha beta gamma delta epsilon zeta eta theta iota kappa lambda; retain only the checkpoint marker.\n"
                ));
                index += 1;
            }
            prompt.truncate(bytes);
            std::fs::write(resolved_output.join("compaction-prompt.txt"), prompt)
                .expect("write live compaction prompt");
        }
        println!(
            "exported isolated live inputs: {} routes (no credential plaintext)",
            credentials.len()
        );
    }

    #[tokio::test]
    #[ignore = "plans/applies against an explicitly selected live SSH host"]
    async fn live_native_deployment() {
        let alias = std::env::var("VELLUM_LIVE_SSH_ALIAS").expect("live SSH alias");
        let state = AppState::new();
        let candidates =
            crate::remote::discovery::discover(&state.remote().list_hosts().unwrap()).unwrap();
        let candidate = candidates
            .into_iter()
            .find(|candidate| candidate.ssh_alias == alias)
            .expect("configured Codex/OpenSSH host");
        state.remote().import_discovered_host(&candidate).unwrap();
        let planned = plan(
            &state,
            &candidate.vellum_host_id,
            RemoteModelSelection::default(),
        )
        .expect("live deployment plan");
        println!("{}", serde_json::to_string_pretty(&planned).unwrap());
        if std::env::var("VELLUM_LIVE_APPLY").as_deref() == Ok("1") {
            assert!(
                planned.blocked_reasons.is_empty(),
                "{:?}",
                planned.blocked_reasons
            );
            let applied = apply(&state, &candidate.vellum_host_id, &planned.plan_id)
                .await
                .expect("live native deployment apply");
            assert_eq!(applied.state, "nativeActive");
        }
    }

    #[test]
    fn boundary_key_provisioning_precedes_proxy_configure_start_adopt_and_restart_in_source_order()
    {
        let source = include_str!("deployment.rs");
        let provision_at = source
            .find("provision_remote_boundary_key(")
            .expect("apply() must call provision_remote_boundary_key");
        let confirm_at = source
            .find("confirm_remote_boundary_key_consumers(")
            .expect("apply() must confirm resulting consumer identities");
        for (label, marker) in [
            ("proxy.configure", "client.proxy_configure("),
            ("proxy.start", "client.proxy_start("),
            ("native adopt", ".codex_apply_native_adopt("),
            ("native restart", "client.codex_restart_native("),
        ] {
            let rpc_at = source
                .find(marker)
                .unwrap_or_else(|| panic!("apply() must call {label}"));
            assert!(
                provision_at < rpc_at,
                "boundary-key provisioning must run before {label}"
            );
            assert!(
                rpc_at < confirm_at,
                "boundary-key consumer confirmation must run after {label}"
            );
        }
    }
}
