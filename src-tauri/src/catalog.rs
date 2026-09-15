use crate::error::{AppError, AppResult};
use crate::harness::{self, HarnessProfile};
use crate::model::{ModelRoute, ProviderKind, ReasoningEffortTransport, Route};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use vellum_proxy_runtime::config::RuntimeCompactionPolicy;

/// Per-model Codex-native auto-compact policy, keyed by catalog slug
/// (`ModelRoute::catalog_id`). Built by callers that have `AppState` (the
/// only place the resolved global/route compaction policy lives) and passed
/// through to catalog generation so `auto_compact_token_limit` reflects the
/// same threshold Vellum would otherwise gate on. Absent entries (including
/// every Grok route in this pass — see docs/protocol-source-of-truth.md)
/// leave the field unset, matching today's stripped-on-projection behavior.
pub type CompactionPolicyBySlug = HashMap<String, RuntimeCompactionPolicy>;

pub(crate) const DEFAULT_CONTEXT_WINDOW: u64 = 121_600;

/// Codex's official catalog can opt a model into a private Responses-Lite tool
/// surface. Chat-compatible third-party routes cannot consume that transport.
/// If these fields are inherited from the official template, Codex omits its
/// normal tool definitions and the upstream model starts printing fake
/// `<tool_call>` text instead of returning native Chat `tool_calls`.
fn apply_chat_proxy_catalog_profile(object: &mut serde_json::Map<String, Value>) {
    // Keep the normal function-tool surface without opting the route into
    // OpenAI's private Responses-Lite transport. `tool_mode` is optional in
    // the current schema, so omitting it is safer than inventing semantics.
    object.remove("tool_mode");
    object.insert("use_responses_lite".into(), json!(false));
}

/// Resolve the harness contract for a generated entry. Catalog generation and
/// the request adapter must reach this through the same [`harness::resolve`],
/// or the model-visible surface and the runtime drift apart again.
fn profile_for(routes: &[Route], model: &ModelRoute) -> HarnessProfile {
    routes
        .iter()
        .find(|route| route.id == model.route_id)
        .map(|route| harness::resolve(route.provider_kind, route.wire))
        .unwrap_or_else(|| HarnessProfile::generic(model.wire.into()))
}

/// Fill the structural fields required by the current Codex model-catalog
/// schema without advertising capabilities that a third-party route has not
/// proved.  Codex rejects the *entire* configuration when one model omits a
/// required field, which in turn can make Windows sandbox onboarding appear
/// even though the user's `[windows]` setting is valid.
fn apply_codex_catalog_schema_defaults(
    object: &mut serde_json::Map<String, Value>,
    profile: &HarnessProfile,
) {
    object.insert("shell_type".into(), json!("shell_command"));
    object.insert("include_skills_usage_instructions".into(), json!(false));
    object.insert("supports_reasoning_summaries".into(), json!(false));
    object.insert("default_reasoning_summary".into(), json!("none"));
    object.insert("support_verbosity".into(), json!(true));
    object.insert("default_verbosity".into(), json!("low"));
    object.insert("apply_patch_tool_type".into(), json!("freeform"));
    object.insert("web_search_tool_type".into(), json!("text_and_image"));
    object.insert(
        "truncation_policy".into(),
        json!({"mode": "tokens", "limit": 10_000}),
    );
    // Driven by the resolved profile rather than hardcoded, so there is a
    // single place that decides what a route advertises. These particular
    // flags switch Codex's *private* request and runtime behaviour, not a
    // prompt feature: setting them without the matching runtime hides normal
    // tools and sends an unsupported dialect upstream (issue #6, "do not spoof
    // Sol private flags"). Sol parity is reached through the prompt, the exact
    // tool contracts, and the execution policy instead.
    object.insert(
        "supports_parallel_tool_calls".into(),
        json!(profile.advertises_parallel_tool_calls()),
    );
    object.insert(
        "use_responses_lite".into(),
        json!(profile.advertises_responses_lite()),
    );
    object.insert("supports_image_detail_original".into(), json!(false));
    object.insert("experimental_supported_tools".into(), json!([]));
    // This is Codex's client-side deferred tool-discovery capability, not the
    // optional Vellum/Brave web-search setting. The shared adapters preserve
    // `tool_search` calls on every translated route.
    object.insert("supports_search_tool".into(), json!(true));
    object.remove("comp_hash");
    object.remove("tool_mode");
    // Native multi-agent V2 is executed by the qualified Enhanced Codex host;
    // child requests still traverse this entry's ordinary provider adapter.
    object.insert("multi_agent_version".into(), json!("v2"));
}

const NATIVE_SUBAGENT_CONTRACT: &str = r#"

Native subagents are provided by the Codex host. When the user explicitly asks
for subagents, use the native collaboration functions. If they are deferred,
discover them with tool search, then invoke the exact returned function schema.
Never type a collaboration function name into the shell. Task APIs such as
create_thread and fork_thread are not substitutes for a native subagent. Give
each child a bounded task, retain its returned id, wait for it, and synthesize
its result. A child executes its assigned bounded task directly and must not
spawn another child unless that assigned task explicitly requires nested
delegation. If the native function cannot be loaded or invoked, report that
capability failure directly instead of silently changing mechanisms.
"#;

/// Official-only / provider-specific fields that must never ride along when a
/// third-party model is materialised. Cloning a full official ModelInfo and
/// patching a few keys leaks compaction thresholds, capability hashes, and
/// transport flags that only make sense for ChatGPT-hosted models.
const OFFICIAL_ONLY_CATALOG_FIELDS: &[&str] = &[
    "comp_hash",
    "auto_compact_token_limit",
    "truncation_policy",
    "supports_reasoning_summaries",
    "supports_parallel_tool_calls",
    "service_tiers",
    "additional_speed_tiers",
    "experimental_supported_tools",
    "tool_mode",
    "use_responses_lite",
    "availability_nux",
    "upgrade",
    "supported_in_api",
    "visibility",
    "prefer_websockets",
    "priority",
    "slug",
    "display_name",
    "description",
    "context_window",
    "max_context_window",
    "effective_context_window_percent",
    "supported_reasoning_levels",
    "default_reasoning_level",
];

/// Whether the caller has opted into per-model compaction-aware catalog
/// generation, and if so, this model's resolved policy (if any).
///
/// `NotOptedIn` covers Desktop's Enhanced Runtime projection as well as tests,
/// eval fixtures, and callers re-deriving an already-known catalog through
/// `catalog_json`/`catalog_json_with_official` without a compaction map
/// (`compaction_policies: None`). That path publishes `context_window` for
/// Codex's usage meter and native local compaction, while never publishing a
/// Vellum-authored `auto_compact_token_limit`.
///
/// `OptedIn(policy)` covers remote-managed Codex catalog projection. Codex
/// derives its own 90%-of-`context_window` auto-compact threshold whenever
/// `auto_compact_token_limit` is absent but the model still advertises a
/// `context_window` — there is no schema field that lets a model opt out of
/// scheduling while keeping its context window visible. So a model with no
/// policy in the map (for example a remote Grok route) or an
/// explicitly disabled one omits `context_window`/`max_context_window`
/// alongside the limit, or Codex would schedule against its own fallback
/// threshold instead. The route's own context window is unaffected: only
/// what remote Codex's catalog advertises for that model's auto-compact meter
/// is withheld. Desktop does not use this mode because its non-OpenAI Vellum
/// provider selects native local compaction.
#[derive(Clone, Copy)]
enum CompactionProjection<'a> {
    NotOptedIn,
    OptedIn(Option<&'a RuntimeCompactionPolicy>),
}

impl CompactionProjection<'_> {
    fn from_map<'a>(
        compaction_policies: Option<&'a CompactionPolicyBySlug>,
        catalog_id: &str,
    ) -> CompactionProjection<'a> {
        match compaction_policies {
            Some(map) => CompactionProjection::OptedIn(map.get(catalog_id)),
            None => CompactionProjection::NotOptedIn,
        }
    }

    /// Resolves to `(publish_context_window, auto_compact_token_limit)`.
    fn resolve(self, context: u64) -> (bool, Option<u64>) {
        match self {
            CompactionProjection::NotOptedIn => (true, None),
            CompactionProjection::OptedIn(policy) => {
                let limit = policy.and_then(|policy| policy.auto_compact_token_limit(context));
                (limit.is_some(), limit)
            }
        }
    }
}

fn apply_context_and_compact_limit(
    object: &mut serde_json::Map<String, Value>,
    context: u64,
    projection: CompactionProjection<'_>,
) {
    let (publish_context, limit) = projection.resolve(context);
    if publish_context {
        object.insert("context_window".into(), json!(context));
        object.insert("max_context_window".into(), json!(context));
        object.insert("effective_context_window_percent".into(), json!(95));
    } else {
        object.remove("context_window");
        object.remove("max_context_window");
        object.remove("effective_context_window_percent");
    }
    match limit {
        Some(limit) => {
            object.insert("auto_compact_token_limit".into(), json!(limit));
        }
        None => {
            object.remove("auto_compact_token_limit");
        }
    }
}

/// Codex reads this list to decide whether the attachment button is offered for
/// a model. Omitting `image` is what produces "此模型不支援圖像輸入".
fn input_modalities(vision: bool) -> Value {
    if vision {
        json!(["text", "image"])
    } else {
        json!(["text"])
    }
}

/// Codex renders reasoning levels as a left-to-right strength slider in the
/// order supplied by the catalog. Provider catalogs are not ordered
/// consistently (Grok reports strongest first), so normalize presentation
/// order without changing the provider's explicit default or the value sent
/// upstream.
fn ordered_reasoning_efforts(efforts: &[String]) -> Vec<String> {
    fn rank(effort: &str) -> u8 {
        match effort.to_ascii_lowercase().as_str() {
            "none" => 0,
            "minimal" => 1,
            "low" => 2,
            "medium" => 3,
            "high" => 4,
            "xhigh" => 5,
            "ultra" => 6,
            _ => u8::MAX,
        }
    }

    let mut ordered = efforts.to_vec();
    ordered.sort_by_key(|effort| rank(effort));
    ordered
}

/// Choose a useful default when a successful capability probe did not name
/// one. Falling back to the first display-ordered item selected `none` for
/// the common `[none, minimal, low, medium, ...]` surface, which made two
/// otherwise identical routes behave differently depending on whether Codex
/// happened to retain an older per-thread effort selection.
fn default_reasoning_effort(model: &ModelRoute) -> &str {
    if let Some(explicit) = model.default_reasoning_effort.as_deref() {
        return explicit;
    }

    model
        .reasoning_efforts
        .iter()
        .find(|effort| effort.eq_ignore_ascii_case("medium"))
        .or_else(|| {
            ordered_reasoning_efforts(&model.reasoning_efforts)
                .into_iter()
                .find(|effort| !effort.eq_ignore_ascii_case("none"))
                .and_then(|selected| {
                    model
                        .reasoning_efforts
                        .iter()
                        .find(|effort| effort.eq_ignore_ascii_case(&selected))
                })
        })
        .or_else(|| model.reasoning_efforts.first())
        .map(String::as_str)
        .unwrap_or("none")
}

fn third_party_catalog_entry(
    model: &ModelRoute,
    priority: usize,
    profile: &HarnessProfile,
    compaction_projection: CompactionProjection<'_>,
) -> Value {
    let context = model
        .context_window
        .filter(|window| *window > 0)
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    // Issue #6 Phase 1: the prompt is generated from the resolved profile and
    // the shell that was actually detected. It used to be copied from
    // `models.first()` — whichever model Codex happened to list first — which
    // made the instructions depend on OpenAI's catalog ordering and describe a
    // native environment this route does not provide.
    let mut base_instructions =
        harness::prompt::instructions(profile, harness::shell::detected(), &[]);
    base_instructions.push_str(NATIVE_SUBAGENT_CONTRACT);
    let mut entry = json!({
        "slug": model.catalog_id,
        "display_name": model.display_name,
        "description": model.display_name,
        "priority": priority,
        "base_instructions": base_instructions,
        "model_messages": {
            "instructions_template": base_instructions,
            "instructions_variables": {
                "personality_default": "",
                "personality_friendly": "",
                "personality_pragmatic": ""
            }
        },
        // Text-only unless the user declared this model vision-capable. Never
        // copy Official modalities — this list is built, not inherited.
        "input_modalities": input_modalities(model.vision),
        "supported_reasoning_levels": ordered_reasoning_efforts(&model.reasoning_efforts).iter().map(|effort| {
            json!({"effort": effort, "description": effort})
        }).collect::<Vec<_>>(),
        "default_reasoning_level": default_reasoning_effort(model),
        "prefer_websockets": false,
        "additional_speed_tiers": [],
        "service_tiers": [],
        "availability_nux": null,
        "upgrade": null,
        "visibility": "list",
        "supported_in_api": true
    });
    let object = entry
        .as_object_mut()
        .expect("third-party catalog entry must be an object");
    // Re-assert after the structural build so a residual key from any future
    // template path cannot reintroduce Official modalities.
    object.insert("input_modalities".into(), input_modalities(model.vision));
    // Explicit allowlist builder: never leave residual official-only keys even
    // if a future template path copies more than the structural set above.
    for key in OFFICIAL_ONLY_CATALOG_FIELDS {
        // Keep the keys we intentionally set on this entry.
        if matches!(
            *key,
            "slug"
                | "display_name"
                | "description"
                | "context_window"
                | "max_context_window"
                | "effective_context_window_percent"
                | "priority"
                | "supported_reasoning_levels"
                | "default_reasoning_level"
                | "prefer_websockets"
                | "service_tiers"
                | "additional_speed_tiers"
                | "availability_nux"
                | "upgrade"
                | "visibility"
                | "supported_in_api"
        ) {
            continue;
        }
        object.remove(*key);
    }
    // Always force the third-party defaults for promotional / transport metadata.
    object.insert("prefer_websockets".into(), json!(false));
    object.insert("additional_speed_tiers".into(), json!([]));
    object.insert("service_tiers".into(), json!([]));
    object.insert("availability_nux".into(), Value::Null);
    object.insert("upgrade".into(), Value::Null);
    object.insert("visibility".into(), json!("list"));
    object.insert("supported_in_api".into(), json!(true));
    // Codex-native auto-compact scheduling (see
    // docs/protocol-source-of-truth.md): Vellum computes an absolute token
    // limit from the route's Canonical threshold/reserves and publishes it
    // per-model, rather than rewriting requests itself once Codex crosses
    // the line. See `CompactionProjection` for what gets omitted and why.
    if let (_, Some(limit)) = compaction_projection.resolve(context) {
        log::info!(
            "[Compaction] catalog auto_compact_token_limit published: model={} limit={limit} context_window={context}",
            model.catalog_id
        );
    }
    apply_context_and_compact_limit(object, context, compaction_projection);
    apply_codex_catalog_schema_defaults(object, profile);
    if model.wire == crate::model::WireFormat::Chat {
        apply_chat_proxy_catalog_profile(object);
    }
    entry
}

pub fn model_routes(routes: &[Route]) -> Vec<ModelRoute> {
    model_routes_with_official_catalog(routes, None)
}

/// Build the routing table from configured providers while importing every
/// model from Codex's own cache for the official provider.  Keeping the
/// official entries native is important: Vellum must not replace Codex's
/// current prompt/tool metadata with a hand-maintained approximation.
pub fn model_routes_with_official_catalog(
    routes: &[Route],
    official_catalog: Option<&Value>,
) -> Vec<ModelRoute> {
    let mut result = Vec::new();
    let delegation_available = crate::harness::multi_agent::delegation_available();
    for route in routes.iter().filter(|route| route.enabled) {
        let models = if route.provider_kind == ProviderKind::Official {
            let cached = official_model_ids(official_catalog);
            if cached.is_empty() {
                configured_models(route)
            } else {
                cached
            }
        } else {
            configured_models(route)
        };
        for model in models {
            let official = route.provider_kind == ProviderKind::Official;
            let capability = route.model_capabilities.iter().find(|capability| {
                capability.model.eq_ignore_ascii_case(&model)
                    && capability.probe_version == Some(crate::probe::HARNESS_PROBE_VERSION)
            });
            // Both are user-declared rather than probed, so they are read
            // without the probe-version gate above: a stale or absent probe
            // must not discard something the user set by hand.
            let declared = route
                .model_capabilities
                .iter()
                .find(|capability| capability.model.eq_ignore_ascii_case(&model));
            let vision = declared
                .and_then(|capability| capability.vision)
                .unwrap_or(false);
            let model_label = declared
                .and_then(|capability| capability.display_name.as_deref())
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .unwrap_or(model.as_str())
                .to_string();
            // Codex depends on typed function calls. A provider that merely
            // accepts a tool schema but renders `<tool_call>` as assistant
            // text is usable as a chat endpoint, not as a Codex model. Keep it
            // out of the picker rather than advertising a route that cannot
            // execute the native harness.
            if !official
                && capability.is_some_and(|capability| capability.tool_calling == Some(false))
            {
                continue;
            }
            let verified_effort_capability = capability.filter(|capability| {
                capability.effort_probe_version == Some(crate::probe::EFFORT_PROBE_VERSION)
            });
            let context_window = if official {
                official_context_window(official_catalog, &model)
                    .or_else(|| route.context_window.filter(|window| *window > 0))
            } else {
                capability
                    .and_then(|capability| capability.context_window)
                    .filter(|window| *window > 0)
                    .or_else(|| route.context_window.filter(|window| *window > 0))
            };
            let catalog_id = if official {
                model.clone()
            } else {
                stable_catalog_id(&route.id, &model)
            };
            let (provider_efforts, provider_default_effort) = if official {
                official_reasoning_efforts(official_catalog, &model)
            } else if route.provider_kind == ProviderKind::GrokCli {
                crate::budget::grok_model_cache_efforts(&model)
            } else {
                (Vec::new(), None)
            };
            let resolved_efforts = if official {
                provider_efforts.clone()
            } else {
                verified_effort_capability
                    .map(|capability| capability.reasoning_efforts.clone())
                    .filter(|levels| !levels.is_empty())
                    .unwrap_or_else(|| provider_efforts.clone())
            };
            let resolved_default = if official {
                provider_default_effort.clone()
            } else {
                verified_effort_capability
                    .and_then(|capability| capability.default_reasoning_effort.clone())
                    .or_else(|| provider_default_effort.clone())
            };
            // Issue #6: `ultra` is maximum reasoning *with automatic task
            // delegation*. Grok's own cache and the effort probe can both
            // report it, and until this gate existed it reached the catalog
            // unchecked — Codex would offer the model a mode with no
            // delegation runtime behind it. Applied here, when the ModelRoute
            // is built, so every consumer sees the gated list.
            let (reasoning_efforts, default_reasoning_effort) = if official {
                (resolved_efforts, resolved_default)
            } else {
                crate::harness::multi_agent::gate_reasoning_levels(
                    &resolved_efforts,
                    resolved_default,
                    delegation_available,
                )
            };
            result.push(ModelRoute {
                catalog_id,
                display_name: if official {
                    official_display_name(official_catalog, &model).unwrap_or_else(|| model.clone())
                } else {
                    format!("[{}] {model_label}", route.name)
                },
                route_id: route.id.clone(),
                upstream_model: model,
                context_window,
                wire: if route.provider_kind == ProviderKind::GrokCli {
                    crate::model::WireFormat::Responses
                } else {
                    capability
                        .and_then(|capability| capability.wire)
                        .unwrap_or(route.wire)
                },
                vision,
                reasoning: capability
                    .and_then(|capability| capability.reasoning)
                    .unwrap_or(route.reasoning),
                streaming: capability
                    .and_then(|capability| capability.streaming)
                    .unwrap_or(route.streaming),
                reasoning_efforts,
                default_reasoning_effort,
                reasoning_effort_transport: verified_effort_capability
                    .map(|capability| capability.reasoning_effort_transport)
                    .filter(|transport| *transport != ReasoningEffortTransport::None)
                    .unwrap_or_else(|| {
                        if route.provider_kind == ProviderKind::GrokCli {
                            ReasoningEffortTransport::ResponsesObject
                        } else {
                            ReasoningEffortTransport::None
                        }
                    }),
            });
        }
    }
    result
}

fn official_reasoning_efforts(
    catalog: Option<&Value>,
    slug: &str,
) -> (Vec<String>, Option<String>) {
    let model = catalog
        .and_then(|value| value.get("models"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|model| model.get("slug").and_then(Value::as_str) == Some(slug));
    let levels = model
        .and_then(|model| model.get("supported_reasoning_levels"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| {
            value
                .as_str()
                .or_else(|| {
                    value
                        .get("effort")
                        .or_else(|| value.get("value"))
                        .or_else(|| value.get("id"))
                        .and_then(Value::as_str)
                })
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    let default = model
        .and_then(|model| model.get("default_reasoning_level"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|candidate| levels.iter().any(|level| level == candidate));
    (levels, default)
}

/// Auto Review is configured independently from the models exposed in the
/// Codex picker. A reviewer may therefore use any discovered model belonging
/// to an enabled provider without adding that model to the main picker.
pub fn review_model_routes_with_official_catalog(
    routes: &[Route],
    official_catalog: Option<&Value>,
) -> Vec<ModelRoute> {
    let mut review_routes = routes.to_vec();
    for route in &mut review_routes {
        if route.provider_kind != ProviderKind::Official {
            route.selected_models = None;
        }
    }
    model_routes_with_official_catalog(&review_routes, official_catalog)
}

fn official_context_window(catalog: Option<&Value>, slug: &str) -> Option<u64> {
    catalog
        .and_then(|value| value.get("models"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|model| model.get("slug").and_then(Value::as_str) == Some(slug))
        .and_then(|model| {
            ["context_window", "max_context_window", "max_context_length"]
                .into_iter()
                .find_map(|key| model.get(key).and_then(Value::as_u64))
        })
}

fn official_display_name(catalog: Option<&Value>, slug: &str) -> Option<String> {
    catalog
        .and_then(|value| value.get("models"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|model| model.get("slug").and_then(Value::as_str) == Some(slug))
        .and_then(|model| model.get("display_name").and_then(Value::as_str))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn configured_models(route: &Route) -> Vec<String> {
    if let Some(selected) = route
        .selected_models
        .as_ref()
        .filter(|models| !models.is_empty())
    {
        selected.clone()
    } else if route.models.is_empty() {
        vec![route.model.clone()]
    } else {
        route.models.clone()
    }
}

fn official_model_ids(catalog: Option<&Value>) -> Vec<String> {
    catalog
        .and_then(|value| value.get("models"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| model.get("slug").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

pub fn stable_catalog_id(route_id: &str, model: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(route_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(model.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("vlm-{}-{}", &digest[..10], slug(model))
}

fn slug(value: &str) -> String {
    let mut out = String::new();
    let mut separator = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            separator = false;
        } else if !separator {
            out.push('-');
            separator = true;
        }
    }
    out.trim_matches('-').chars().take(40).collect()
}

pub fn catalog_json(routes: &[Route]) -> Value {
    catalog_json_with_official(routes, None)
}

pub fn catalog_json_with_official(routes: &[Route], official_catalog: Option<&Value>) -> Value {
    catalog_json_with_official_and_compaction(routes, official_catalog, None)
}

/// Same as [`catalog_json_with_official`], but projects `auto_compact_token_limit`
/// onto third-party entries from `compaction_policies` (keyed by catalog slug).
/// The only real "publish to Codex" call sites (local proxy startup, the
/// runtime HTTP catalog snapshot, remote desired-state) should call this with
/// a resolved policy map; everything else (tests, eval fixtures) keeps the
/// `None` behavior above unchanged.
pub fn catalog_json_with_official_and_compaction(
    routes: &[Route],
    official_catalog: Option<&Value>,
    compaction_policies: Option<&CompactionPolicyBySlug>,
) -> Value {
    let mut output = official_catalog.cloned().unwrap_or_else(|| json!({}));
    let mut models = official_catalog
        .and_then(|value| value.get("models"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut seen = models
        .iter()
        .filter_map(|model| model.get("slug").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<HashSet<_>>();

    let generated = model_routes_with_official_catalog(routes, official_catalog)
        .into_iter()
        .filter(|model| !seen.contains(&model.catalog_id))
        .enumerate()
        .map(|(priority, model)| {
            let profile = profile_for(routes, &model);
            let projection = CompactionProjection::from_map(compaction_policies, &model.catalog_id);
            third_party_catalog_entry(&model, 1000 + priority, &profile, projection)
        })
        .collect::<Vec<_>>();
    for model in generated {
        if let Some(slug) = model.get("slug").and_then(Value::as_str) {
            seen.insert(slug.to_string());
        }
        models.push(model);
    }
    output
        .as_object_mut()
        .expect("catalog root must be an object")
        .insert("models".into(), Value::Array(models));
    output
}

const CODEX_REQUIRED_INSTRUCTIONS: &str =
    "Official native catalog entry. Instructions are owned by the Codex Official catalog.";

/// Codex CLI rejects the whole catalog when any active entry lacks a field
/// required by the host's catalog schema. Official cache rows can come from a
/// different Codex build, so fill conservative compatibility defaults without
/// replacing fields the Official catalog already supplied.
pub(crate) fn ensure_catalog_ready_for_codex(catalog: &mut Value) -> AppResult<()> {
    let models = catalog
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| AppError::Message("model catalog is missing models[]".into()))?;
    for (index, model) in models.iter_mut().enumerate() {
        let slug = model
            .get("slug")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if slug.is_empty() {
            return Err(AppError::Message(format!(
                "model catalog entry {index} is missing slug; refusing to apply"
            )));
        }
        let existing = model
            .get("base_instructions")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if existing.is_empty() {
            let object = model.as_object_mut().ok_or_else(|| {
                AppError::Message(format!(
                    "model catalog entry {slug} is not an object; refusing to apply"
                ))
            })?;
            object.insert(
                "base_instructions".into(),
                Value::String(CODEX_REQUIRED_INSTRUCTIONS.into()),
            );
        }
        let object = model.as_object_mut().ok_or_else(|| {
            AppError::Message(format!(
                "model catalog entry {slug} is not an object; refusing to apply"
            ))
        })?;
        object
            .entry("shell_type")
            .or_insert_with(|| json!("shell_command"));
        object
            .entry("supports_parallel_tool_calls")
            .or_insert_with(|| json!(false));
        let filled = model
            .get("base_instructions")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if filled.is_empty() {
            return Err(AppError::Message(format!(
                "model catalog entry {slug} has empty base_instructions; refusing to apply"
            )));
        }
    }
    Ok(())
}

pub fn read_official_catalog(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.get("models").and_then(Value::as_array)?;
    Some(value)
}

pub fn write_catalog(
    path: &Path,
    routes: &[Route],
    official_cache_path: Option<&Path>,
) -> AppResult<()> {
    write_catalog_with_search(path, routes, official_cache_path, false)
}

fn write_catalog_with_search(
    path: &Path,
    routes: &[Route],
    official_cache_path: Option<&Path>,
    _search_tool_available: bool,
) -> AppResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Message("模型型錄路徑沒有父目錄".into()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| AppError::Message(format!("無法建立模型型錄目錄：{error}")))?;
    let tmp = path.with_extension("json.tmp");
    let official = official_cache_path.and_then(read_official_catalog);
    let mut catalog = catalog_json_with_official(routes, official.as_ref());
    ensure_catalog_ready_for_codex(&mut catalog)?;
    let bytes = serde_json::to_vec_pretty(&catalog)
        .map_err(|error| AppError::Message(format!("模型型錄序列化失敗：{error}")))?;
    std::fs::write(&tmp, bytes)
        .map_err(|error| AppError::Message(format!("模型型錄寫入失敗：{error}")))?;
    std::fs::rename(&tmp, path)
        .map_err(|error| AppError::Message(format!("模型型錄替換失敗：{error}")))?;
    Ok(())
}

pub fn write_catalog_with_model_routes(
    path: &Path,
    routes: &[Route],
    model_routes: &[ModelRoute],
    official_cache_path: Option<&Path>,
) -> AppResult<()> {
    write_catalog_with_model_routes_and_search(
        path,
        routes,
        model_routes,
        official_cache_path,
        false,
    )
}

pub fn write_catalog_with_model_routes_and_search(
    path: &Path,
    routes: &[Route],
    model_routes: &[ModelRoute],
    official_cache_path: Option<&Path>,
    search_tool_available: bool,
) -> AppResult<()> {
    write_catalog_with_model_routes_and_search_and_compaction(
        path,
        routes,
        model_routes,
        official_cache_path,
        search_tool_available,
        None,
    )
}

/// Same as [`write_catalog_with_model_routes_and_search`], but projects
/// `auto_compact_token_limit` from `compaction_policies` (keyed by catalog
/// slug) onto every third-party entry it writes or patches.
pub fn write_catalog_with_model_routes_and_search_and_compaction(
    path: &Path,
    routes: &[Route],
    model_routes: &[ModelRoute],
    official_cache_path: Option<&Path>,
    _search_tool_available: bool,
    compaction_policies: Option<&CompactionPolicyBySlug>,
) -> AppResult<()> {
    let official = official_cache_path.and_then(read_official_catalog);
    let official_slugs = official
        .as_ref()
        .and_then(|value| value.get("models"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| model.get("slug").and_then(Value::as_str).map(str::to_owned))
        .collect::<HashSet<_>>();
    let mut value =
        catalog_json_with_official_and_compaction(routes, official.as_ref(), compaction_policies);
    let models = value
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .expect("catalog models must be an array");
    let mut seen = models
        .iter()
        .filter_map(|model| model.get("slug").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<HashSet<_>>();
    for (priority, model) in model_routes.iter().enumerate() {
        let context = model
            .context_window
            .filter(|window| *window > 0)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        let projection = CompactionProjection::from_map(compaction_policies, &model.catalog_id);
        // Never mutate Official cache entries: context windows and percentages
        // must round-trip byte-for-byte from Codex's own catalog.
        if official_slugs.contains(&model.catalog_id) {
            continue;
        }
        if let Some(existing) = models.iter_mut().find(|entry| {
            entry.get("slug").and_then(Value::as_str) == Some(model.catalog_id.as_str())
        }) {
            // ModelRoute is authoritative for third-party context overrides.
            // The generated entry's limit was computed against the old
            // context window; recompute so it stays consistent with the
            // override instead of silently going stale.
            if let Some(object) = existing.as_object_mut() {
                apply_context_and_compact_limit(object, context, projection);
            }
            continue;
        }
        seen.insert(model.catalog_id.clone());
        let profile = profile_for(routes, model);
        let entry = third_party_catalog_entry(model, 2000 + priority, &profile, projection);
        models.push(entry);
    }
    ensure_catalog_ready_for_codex(&mut value)?;
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Message("模型型錄路徑沒有父目錄".into()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| AppError::Message(format!("無法建立模型型錄目錄：{error}")))?;
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(&value)
        .map_err(|error| AppError::Message(format!("模型型錄序列化失敗：{error}")))?;
    std::fs::write(&tmp, bytes)
        .map_err(|error| AppError::Message(format!("模型型錄寫入失敗：{error}")))?;
    std::fs::rename(&tmp, path)
        .map_err(|error| AppError::Message(format!("模型型錄替換失敗：{error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AuthKind, ModelCapability, ReasoningEffortTransport, WireFormat};

    fn route(kind: ProviderKind, id: &str, model: &str) -> Route {
        Route {
            id: id.into(),
            name: id.into(),
            base_url: "https://example.test".into(),
            model: model.into(),
            wire: WireFormat::Responses,
            is_current: false,
            server_side_resume: false,
            streaming: true,
            reasoning: true,
            provider_kind: kind,
            auth_kind: AuthKind::None,
            enabled: true,
            models: vec![model.into()],
            selected_models: None,
            context_window: None,
            model_capabilities: Vec::new(),
            insecure_http_policy: Default::default(),
            catalog_scope: Default::default(),
        }
    }

    #[test]
    fn third_party_ids_are_stable_and_namespaced() {
        let first = stable_catalog_id("weikuwu", "GLM-5.2");
        assert_eq!(first, stable_catalog_id("weikuwu", "GLM-5.2"));
        assert!(first.starts_with("vlm-"));
        assert_ne!(first, stable_catalog_id("other", "GLM-5.2"));
    }

    #[test]
    fn grok_efforts_are_advertised_from_lightest_to_strongest() {
        let mut provider = route(ProviderKind::GrokCli, "grok-cli", "grok-4.6");
        provider.model_capabilities = vec![ModelCapability {
            model: "grok-4.6".into(),
            reasoning_efforts: vec!["xhigh".into(), "high".into(), "medium".into(), "low".into()],
            default_reasoning_effort: Some("high".into()),
            reasoning_effort_transport: ReasoningEffortTransport::ResponsesObject,
            probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
            effort_probe_version: Some(crate::probe::EFFORT_PROBE_VERSION),
            ..Default::default()
        }];

        let value = catalog_json(&[provider]);
        let entry = &value["models"][0];
        let advertised = entry["supported_reasoning_levels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|level| level["effort"].as_str().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(advertised, vec!["low", "medium", "high", "xhigh"]);
        assert_eq!(entry["default_reasoning_level"], "high");
    }

    #[test]
    fn missing_reasoning_default_prefers_medium_over_none() {
        let mut provider = route(ProviderKind::OpenAiCompatible, "third", "thinking-model");
        provider.model_capabilities = vec![ModelCapability {
            model: "thinking-model".into(),
            reasoning_efforts: vec![
                "none".into(),
                "minimal".into(),
                "low".into(),
                "medium".into(),
                "high".into(),
            ],
            default_reasoning_effort: None,
            reasoning_effort_transport: ReasoningEffortTransport::ChatField,
            probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
            effort_probe_version: Some(crate::probe::EFFORT_PROBE_VERSION),
            ..Default::default()
        }];

        let value = catalog_json(&[provider]);
        assert_eq!(value["models"][0]["default_reasoning_level"], "medium");
    }

    #[test]
    fn missing_reasoning_default_uses_lightest_non_none_level() {
        let mut provider = route(ProviderKind::OpenAiCompatible, "third", "thinking-model");
        provider.model_capabilities = vec![ModelCapability {
            model: "thinking-model".into(),
            reasoning_efforts: vec!["none".into(), "high".into(), "minimal".into()],
            default_reasoning_effort: None,
            reasoning_effort_transport: ReasoningEffortTransport::ChatField,
            probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
            effort_probe_version: Some(crate::probe::EFFORT_PROBE_VERSION),
            ..Default::default()
        }];

        let value = catalog_json(&[provider]);
        assert_eq!(value["models"][0]["default_reasoning_level"], "minimal");
    }

    #[test]
    fn third_party_catalog_uses_per_model_capabilities() {
        let mut route = route(ProviderKind::OpenAiCompatible, "mixed", "model-a");
        route.models = vec!["model-a".into(), "model-b".into()];
        route.selected_models = Some(route.models.clone());
        route.context_window = Some(8_000);
        route.model_capabilities = vec![
            ModelCapability {
                model: "model-a".into(),
                context_window: Some(32_000),
                wire: Some(WireFormat::Chat),
                streaming: Some(true),
                reasoning: Some(false),
                probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                ..Default::default()
            },
            ModelCapability {
                model: "model-b".into(),
                context_window: Some(200_000),
                wire: Some(WireFormat::Responses),
                streaming: Some(false),
                reasoning: Some(true),
                probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                reasoning_efforts: vec!["minimal".into(), "xhigh".into()],
                default_reasoning_effort: Some("xhigh".into()),
                reasoning_effort_transport: ReasoningEffortTransport::ResponsesObject,
                effort_probe_version: Some(crate::probe::EFFORT_PROBE_VERSION),
                ..Default::default()
            },
        ];
        let models = model_routes(&[route]);
        assert_eq!(models[0].context_window, Some(32_000));
        assert_eq!(models[0].wire, WireFormat::Chat);
        assert!(models[0].streaming);
        assert!(!models[0].reasoning);
        assert_eq!(models[1].context_window, Some(200_000));
        assert_eq!(models[1].reasoning_efforts, vec!["minimal", "xhigh"]);
        assert_eq!(models[1].default_reasoning_effort.as_deref(), Some("xhigh"));
        assert_eq!(models[1].wire, WireFormat::Responses);
        assert!(!models[1].streaming);
        assert!(models[1].reasoning);
    }

    #[test]
    fn zero_context_windows_fall_through_instead_of_publishing_zero() {
        let mut route = route(ProviderKind::OpenAiCompatible, "third", "model-a");
        route.context_window = Some(64_000);
        route.model_capabilities = vec![ModelCapability {
            model: "model-a".into(),
            context_window: Some(0),
            probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
            ..Default::default()
        }];

        let models = model_routes(std::slice::from_ref(&route));
        assert_eq!(models[0].context_window, Some(64_000));

        route.context_window = Some(0);
        let value = catalog_json(&[route]);
        assert_eq!(value["models"][0]["context_window"], DEFAULT_CONTEXT_WINDOW);
        assert_eq!(
            value["models"][0]["max_context_window"],
            DEFAULT_CONTEXT_WINDOW
        );
    }

    #[test]
    fn opencode_capability_wins_over_route_default_for_all_wire_streaming_pairs() {
        let mut route = route(ProviderKind::OpenAiCompatible, "opencode", "fallback");
        route.wire = WireFormat::Chat;
        route.streaming = true;
        route.models = vec![
            "chat-stream".into(),
            "chat-buffered".into(),
            "resp-stream".into(),
            "resp-buffered".into(),
            "fallback".into(),
        ];
        route.selected_models = Some(route.models.clone());
        route.model_capabilities = vec![
            ModelCapability {
                model: "chat-stream".into(),
                wire: Some(WireFormat::Chat),
                streaming: Some(true),
                probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                ..Default::default()
            },
            ModelCapability {
                model: "chat-buffered".into(),
                wire: Some(WireFormat::Chat),
                streaming: Some(false),
                probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                ..Default::default()
            },
            ModelCapability {
                model: "resp-stream".into(),
                wire: Some(WireFormat::Responses),
                streaming: Some(true),
                probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                ..Default::default()
            },
            ModelCapability {
                model: "resp-buffered".into(),
                wire: Some(WireFormat::Responses),
                streaming: Some(false),
                probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
                ..Default::default()
            },
        ];
        let models = model_routes(&[route]);
        let by_name = |needle: &str| {
            models
                .iter()
                .find(|model| model.upstream_model == needle)
                .unwrap()
        };
        assert_eq!(by_name("chat-stream").wire, WireFormat::Chat);
        assert!(by_name("chat-stream").streaming);
        assert_eq!(by_name("chat-buffered").wire, WireFormat::Chat);
        assert!(!by_name("chat-buffered").streaming);
        assert_eq!(by_name("resp-stream").wire, WireFormat::Responses);
        assert!(by_name("resp-stream").streaming);
        assert_eq!(by_name("resp-buffered").wire, WireFormat::Responses);
        assert!(!by_name("resp-buffered").streaming);
        assert_eq!(
            by_name("fallback").wire,
            WireFormat::Chat,
            "missing capability must use the route default"
        );
        assert!(by_name("fallback").streaming);
    }

    #[test]
    fn grok_catalog_projection_cannot_be_flipped_to_chat() {
        let mut grok = route(ProviderKind::GrokCli, "grok-cli", "grok-4.6");
        grok.wire = WireFormat::Chat;
        grok.model_capabilities = vec![ModelCapability {
            model: "grok-4.6".into(),
            wire: Some(WireFormat::Chat),
            streaming: Some(true),
            ..Default::default()
        }];
        let models = model_routes(&[grok]);
        assert_eq!(models[0].wire, WireFormat::Responses);
    }

    #[test]
    fn official_models_keep_native_id() {
        let routes = model_routes(&[route(ProviderKind::Official, "official", "gpt-5.6-sol")]);
        assert_eq!(routes[0].catalog_id, "gpt-5.6-sol");
    }

    #[test]
    fn writer_fills_missing_official_base_instructions_and_refuses_empty_slug() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("catalog.json");
        let official_cache = temp.path().join("official.json");
        std::fs::write(
            &official_cache,
            serde_json::to_vec(&json!({
                "models": [{
                    "slug": "gpt-5.6-luna",
                    "display_name": "GPT-5.6-Luna",
                    "context_window": 272000
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let grok = route(ProviderKind::GrokCli, "grok-cli", "grok-4.6");
        let models = model_routes(std::slice::from_ref(&grok));
        write_catalog_with_model_routes(&path, &[grok], &models, Some(&official_cache)).unwrap();
        let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let luna = value["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["slug"] == "gpt-5.6-luna")
            .unwrap();
        assert_eq!(
            luna["base_instructions"].as_str().unwrap(),
            CODEX_REQUIRED_INSTRUCTIONS
        );
        assert_eq!(luna["context_window"], 272000);
        assert_eq!(luna["shell_type"], "shell_command");
        assert_eq!(luna["supports_parallel_tool_calls"], false);

        let bad = temp.path().join("bad-official.json");
        std::fs::write(
            &bad,
            serde_json::to_vec(&json!({
                "models": [{ "display_name": "no slug" }]
            }))
            .unwrap(),
        )
        .unwrap();
        let grok = route(ProviderKind::GrokCli, "grok-cli", "grok-4.6");
        let models = model_routes(std::slice::from_ref(&grok));
        let error = write_catalog_with_model_routes(
            &temp.path().join("rejected.json"),
            &[grok],
            &models,
            Some(&bad),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("missing slug"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn catalog_contains_required_agent_fields() {
        let value = catalog_json(&[route(ProviderKind::OpenAiCompatible, "third", "GLM-5.2")]);
        let model = &value["models"][0];
        assert!(model["base_instructions"].is_string());
        assert!(model["model_messages"].is_object());
        assert_eq!(model["prefer_websockets"], false);
        assert_eq!(model["visibility"], "list");
        assert_eq!(model["supported_in_api"], true);
    }

    #[test]
    fn enabled_grok_is_materialized_in_the_codex_catalog() {
        let grok = route(ProviderKind::GrokCli, "grok-cli", "grok-4.5");
        let value = catalog_json(&[grok]);
        let model = &value["models"][0];
        assert_eq!(model["display_name"], "[grok-cli] grok-4.5");
        assert!(model["slug"].as_str().unwrap().contains("grok-4-5"));
        assert_eq!(model["visibility"], "list");
    }

    #[test]
    fn writer_applies_context_override_to_an_already_materialized_model() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("catalog.json");
        let third = route(ProviderKind::OpenAiCompatible, "third", "local-model");
        let mut models = model_routes(std::slice::from_ref(&third));
        assert_eq!(models.len(), 1);
        models[0].context_window = Some(300_000);
        let catalog_id = models[0].catalog_id.clone();
        let mut policies = CompactionPolicyBySlug::new();
        policies.insert(catalog_id.clone(), RuntimeCompactionPolicy::default());

        write_catalog_with_model_routes_and_search_and_compaction(
            &path,
            &[third],
            &models,
            None,
            false,
            Some(&policies),
        )
        .unwrap();

        let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let entry = value["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["slug"] == catalog_id)
            .unwrap();
        assert_eq!(entry["context_window"], 300_000);
        assert_eq!(entry["max_context_window"], 300_000);
        assert_eq!(
            entry["supports_search_tool"], true,
            "disabling Brave search must not disable deferred tool discovery"
        );
    }

    /// Grok is manual-`/compact`-only (see docs/protocol-source-of-truth.md):
    /// no `compaction_policies` entry is ever built for it. Codex derives its
    /// own 90%-of-`context_window` auto-compact threshold whenever a model
    /// still advertises a context window, even with no explicit limit — so a
    /// context override reaching the catalog for Grok would silently turn
    /// scheduling back on. The context override must be dropped along with
    /// the limit, not just the limit.
    #[test]
    fn writer_omits_context_window_for_a_grok_entry_despite_a_route_context_override() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("catalog.json");
        let grok = route(ProviderKind::GrokCli, "grok-cli", "grok-4.5");
        let mut models = model_routes(std::slice::from_ref(&grok));
        assert_eq!(models.len(), 1);
        models[0].context_window = Some(300_000);
        let catalog_id = models[0].catalog_id.clone();
        // Remote's projection never inserts a Grok entry (see
        // docs/protocol-source-of-truth.md) — an opted-in, Grok-less map is
        // what the one remaining opted-in call site produces. Desktop opts out
        // entirely and is covered by the `NotOptedIn` cases above.
        let policies = CompactionPolicyBySlug::new();

        write_catalog_with_model_routes_and_search_and_compaction(
            &path,
            &[grok],
            &models,
            None,
            false,
            Some(&policies),
        )
        .unwrap();

        let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let entry = value["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["slug"] == catalog_id)
            .unwrap();
        assert!(entry.get("context_window").is_none());
        assert!(entry.get("max_context_window").is_none());
        assert!(entry.get("auto_compact_token_limit").is_none());
    }

    #[test]
    fn third_party_catalog_only_contains_selected_models() {
        let mut provider = route(ProviderKind::OpenAiCompatible, "weikuwu", "GLM-5.2");
        provider.models = vec![
            "GLM-5.2".into(),
            "DeepSeek-V4-Flash".into(),
            "MiMo-V2.5-Pro".into(),
        ];
        provider.selected_models = Some(vec!["GLM-5.2".into(), "MiMo-V2.5-Pro".into()]);

        let routes = model_routes(&[provider]);
        assert_eq!(
            routes
                .iter()
                .map(|route| route.upstream_model.as_str())
                .collect::<Vec<_>>(),
            vec!["GLM-5.2", "MiMo-V2.5-Pro"]
        );
    }

    #[test]
    fn auto_review_can_use_a_model_hidden_from_the_codex_picker() {
        let mut provider = route(ProviderKind::OpenAiCompatible, "weikuwu", "GLM-5.2");
        provider.models = vec!["GLM-5.2".into(), "nemotron-3-ultra".into()];
        provider.selected_models = Some(vec!["GLM-5.2".into()]);

        let picker = model_routes(&[provider.clone()]);
        assert_eq!(picker.len(), 1);
        assert_eq!(picker[0].upstream_model, "GLM-5.2");

        let reviewers = review_model_routes_with_official_catalog(&[provider], None);
        assert_eq!(reviewers.len(), 2);
        assert!(reviewers
            .iter()
            .any(|model| model.upstream_model == "nemotron-3-ultra"));
    }

    #[test]
    fn preserves_official_entries_and_uses_them_as_third_party_template() {
        let official = json!({
            "fetched_at": 123,
            "models": [{
                "slug": "gpt-current",
                "display_name": "GPT Current",
                "context_window": 272000,
                "base_instructions": "official instructions",
                "model_messages": {"instructions_template": "official template"},
                "tool_mode": "code",
                "use_responses_lite": true,
                "prefer_websockets": true,
                "comp_hash": "official-comp-hash",
                "auto_compact_token_limit": 200000,
                "truncation_policy": {"type": "tokens", "limit": 1000},
                "supports_reasoning_summaries": true,
                "supports_parallel_tool_calls": true,
                "experimental_supported_tools": ["apply_patch"],
                "availability_nux": {"message": "GPT-5.6 Sol launch"},
                "upgrade": {"target": "gpt-next"}
            }]
        });
        let mut third = route(ProviderKind::OpenAiCompatible, "third", "GLM-5.2");
        third.wire = WireFormat::Chat;
        let routes = vec![
            route(ProviderKind::Official, "official", "stale-local-id"),
            third,
        ];
        let value = catalog_json_with_official(&routes, Some(&official));
        let models = value["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["slug"], "gpt-current");
        assert_eq!(models[0]["prefer_websockets"], true);
        assert_eq!(models[0]["tool_mode"], "code");
        assert_eq!(models[0]["use_responses_lite"], true);
        assert_eq!(models[0]["comp_hash"], "official-comp-hash");
        // Issue #6 Phase 1: the third-party prompt is generated from the
        // resolved harness profile, never inherited from whichever model is
        // first in Codex's catalog. Official instructions stay on Official.
        let third_party_prompt = models[1]["base_instructions"].as_str().unwrap();
        assert_ne!(third_party_prompt, "official instructions");
        assert!(third_party_prompt.starts_with(crate::harness::prompt::BASE_IDENTITY));
        assert!(third_party_prompt.contains("apply_patch"));
        assert_eq!(
            models[1]["model_messages"]["instructions_template"],
            models[1]["base_instructions"]
        );
        assert_eq!(models[1]["prefer_websockets"], false);
        assert!(models[1].get("tool_mode").is_none());
        assert_eq!(models[1]["use_responses_lite"], false);
        assert!(models[1].get("comp_hash").is_none());
        assert!(models[1].get("auto_compact_token_limit").is_none());
        assert_eq!(models[1]["truncation_policy"]["mode"], "tokens");
        assert_eq!(models[1]["supports_reasoning_summaries"], false);
        assert_eq!(models[1]["supports_parallel_tool_calls"], false);
        assert_eq!(models[1]["experimental_supported_tools"], json!([]));
        assert_eq!(models[1]["shell_type"], "shell_command");
        // Third-party modalities are text-only by default; never image copy.
        assert_eq!(models[1]["input_modalities"], json!(["text"]));
        assert_eq!(
            models[0]["availability_nux"]["message"],
            "GPT-5.6 Sol launch"
        );
        assert!(models[1]["availability_nux"].is_null());
        assert!(models[1]["upgrade"].is_null());
        assert_eq!(value["fetched_at"], 123);
    }

    #[test]
    fn writer_does_not_mutate_official_catalog_entries() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("catalog.json");
        let official_cache = temp.path().join("models_cache.json");
        let official = json!({
            "models": [{
                "slug": "gpt-official",
                "display_name": "Official Native",
                "context_window": 272000,
                "max_context_window": 300000,
                "effective_context_window_percent": 88,
                "input_modalities": ["text", "image"],
                "comp_hash": "keep-me",
                "base_instructions": "official"
            }]
        });
        std::fs::write(
            &official_cache,
            serde_json::to_vec_pretty(&official).unwrap(),
        )
        .unwrap();
        let third = route(ProviderKind::OpenAiCompatible, "third", "local-model");
        let mut models = model_routes_with_official_catalog(
            &[
                route(ProviderKind::Official, "official", "stale"),
                third.clone(),
            ],
            Some(&official),
        );
        // Even if ModelRoute carries a different window, Official must not change.
        if let Some(official_route) = models.iter_mut().find(|m| m.catalog_id == "gpt-official") {
            official_route.context_window = Some(1);
        }
        write_catalog_with_model_routes(
            &path,
            &[route(ProviderKind::Official, "official", "stale"), third],
            &models,
            Some(&official_cache),
        )
        .unwrap();
        let written: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let entry = written["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["slug"] == "gpt-official")
            .unwrap();
        assert_eq!(entry["context_window"], 272000);
        assert_eq!(entry["max_context_window"], 300000);
        assert_eq!(entry["effective_context_window_percent"], 88);
        assert_eq!(entry["input_modalities"], json!(["text", "image"]));
        assert_eq!(entry["comp_hash"], "keep-me");
    }

    #[test]
    fn renaming_changes_the_label_but_never_the_catalog_id() {
        // `[cc] hf.co/unsloth/Qwen3.6-...` -> `[806] Qwen3.6-...`, without the
        // id moving. The id is derived from route id + upstream model, so if a
        // rename shifted it, Codex would be left holding an id nothing resolves
        // and every rename would behave like delete-and-recreate.
        let mut route = route(
            ProviderKind::OpenAiCompatible,
            "cc",
            "hf.co/unsloth/qwen3.6",
        );
        let original = catalog_json(&[route.clone()]);
        let before = &original["models"].as_array().unwrap()[0];
        let id_before = before["slug"].as_str().unwrap().to_string();
        assert_eq!(before["display_name"], "[cc] hf.co/unsloth/qwen3.6");

        route.name = "806".into();
        route.model_capabilities = vec![ModelCapability {
            model: "hf.co/unsloth/qwen3.6".into(),
            display_name: Some("Qwen3.6-35B-A3B".into()),
            ..Default::default()
        }];
        let value = catalog_json(&[route]);
        let after = &value["models"].as_array().unwrap()[0];

        assert_eq!(after["display_name"], "[806] Qwen3.6-35B-A3B");
        assert_eq!(after["slug"].as_str().unwrap(), id_before);
    }

    #[test]
    fn a_blank_model_alias_falls_back_to_the_upstream_id() {
        let mut route = route(ProviderKind::OpenAiCompatible, "cc", "qwen-local");
        route.model_capabilities = vec![ModelCapability {
            model: "qwen-local".into(),
            display_name: Some("   ".into()),
            ..Default::default()
        }];
        let value = catalog_json(&[route]);
        assert_eq!(
            value["models"].as_array().unwrap()[0]["display_name"],
            "[cc] qwen-local"
        );
    }

    #[test]
    fn declared_vision_reaches_the_catalog_codex_reads() {
        // Without `image` in input_modalities Codex refuses the attachment with
        // "此模型不支援圖像輸入", which is what made vision-capable third-party
        // models look broken.
        let mut with_vision = route(ProviderKind::OpenAiCompatible, "third", "qwen-vl");
        with_vision.model_capabilities = vec![ModelCapability {
            model: "qwen-vl".into(),
            vision: Some(true),
            ..Default::default()
        }];
        let value = catalog_json(&[with_vision]);
        let entry = &value["models"].as_array().unwrap()[0];
        assert_eq!(entry["input_modalities"], json!(["text", "image"]));

        // Default stays text-only: nothing is advertised that was not declared.
        let plain = catalog_json(&[route(ProviderKind::OpenAiCompatible, "third", "text-model")]);
        assert_eq!(
            plain["models"].as_array().unwrap()[0]["input_modalities"],
            json!(["text"])
        );
    }

    #[test]
    fn declared_vision_survives_a_capability_from_an_older_probe_version() {
        // The switch is user-declared, so it must not be gated on the probe
        // version the way probed fields are — a stale probe entry would
        // otherwise silently drop it.
        let mut route = route(ProviderKind::OpenAiCompatible, "third", "qwen-vl");
        route.model_capabilities = vec![ModelCapability {
            model: "qwen-vl".into(),
            probe_version: Some(1),
            vision: Some(true),
            ..Default::default()
        }];
        let value = catalog_json(&[route]);
        let entry = &value["models"].as_array().unwrap()[0];
        assert_eq!(entry["input_modalities"], json!(["text", "image"]));
    }

    #[test]
    fn third_party_does_not_inherit_official_input_modalities() {
        let official = json!({
            "models": [{
                "slug": "gpt-vision",
                "input_modalities": ["text", "image", "audio"],
                "base_instructions": "official"
            }]
        });
        let value = catalog_json_with_official(
            &[route(ProviderKind::OpenAiCompatible, "third", "local")],
            Some(&official),
        );
        let third = value["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["slug"] != "gpt-vision")
            .unwrap();
        assert_eq!(third["input_modalities"], json!(["text"]));
    }

    #[test]
    fn third_party_catalog_does_not_clone_official_model_specific_metadata() {
        let official = json!({
            "models": [{
                "slug": "gpt-official",
                "comp_hash": "abc123",
                "auto_compact_token_limit": 180_000,
                "base_instructions": "shared structural prompt"
            }]
        });
        let value = catalog_json_with_official(
            &[route(
                ProviderKind::OpenAiCompatible,
                "third",
                "local-model",
            )],
            Some(&official),
        );
        let third = value["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["slug"] != "gpt-official")
            .unwrap();
        assert_ne!(third["base_instructions"], "shared structural prompt");
        assert!(third.get("comp_hash").is_none());
        assert!(third.get("auto_compact_token_limit").is_none());
    }

    #[test]
    fn third_party_catalog_omits_auto_compact_token_limit_without_a_policy() {
        // `catalog_json`/`catalog_json_with_official` (no compaction map) must
        // keep today's behavior: every existing caller (tests, eval fixtures,
        // commands that only re-derive an already-known catalog) is unaffected
        // until it opts in via `catalog_json_with_official_and_compaction`.
        let value = catalog_json(&[route(
            ProviderKind::OpenAiCompatible,
            "third",
            "local-model",
        )]);
        assert!(value["models"][0].get("auto_compact_token_limit").is_none());
        assert!(
            value["models"][0].get("context_window").is_some(),
            "a caller that never opted into compaction_policies must keep \
             publishing context_window exactly as before"
        );
    }

    #[test]
    fn third_party_catalog_projects_auto_compact_token_limit_from_policy() {
        let third = route(ProviderKind::OpenAiCompatible, "third", "local-model");
        let catalog_id = model_routes_with_official_catalog(std::slice::from_ref(&third), None)[0]
            .catalog_id
            .clone();
        let mut policies = CompactionPolicyBySlug::new();
        policies.insert(
            catalog_id.clone(),
            RuntimeCompactionPolicy {
                threshold_percent: 60,
                output_reserve_tokens: 1_000,
                tool_reserve_tokens: 2_000,
                grok_threshold_percent: None,
            },
        );
        let value = catalog_json_with_official_and_compaction(&[third], None, Some(&policies));
        let entry = value["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["slug"] == catalog_id)
            .unwrap();
        let context = entry["context_window"].as_u64().unwrap();
        let expected = context * 60 / 100 - 3_000;
        assert_eq!(entry["auto_compact_token_limit"], json!(expected));
    }

    #[test]
    fn official_catalog_never_receives_a_projected_auto_compact_token_limit() {
        // Official is native passthrough; Vellum must never author this field
        // for an Official entry even if a policy happens to be keyed by its
        // slug (it never should be, but the strip must not depend on that).
        let official = json!({
            "models": [{
                "slug": "gpt-official",
                "auto_compact_token_limit": 180_000,
                "base_instructions": "official"
            }]
        });
        let mut policies = CompactionPolicyBySlug::new();
        policies.insert("gpt-official".into(), RuntimeCompactionPolicy::default());
        let value =
            catalog_json_with_official_and_compaction(&[], Some(&official), Some(&policies));
        assert_eq!(value["models"][0]["auto_compact_token_limit"], 180_000);
    }

    #[test]
    fn third_party_catalog_contains_current_codex_required_schema_fields() {
        let value = catalog_json(&[route(
            ProviderKind::OpenAiCompatible,
            "third",
            "local-model",
        )]);
        let entry = &value["models"][0];
        for key in [
            "shell_type",
            "include_skills_usage_instructions",
            "default_reasoning_summary",
            "support_verbosity",
            "default_verbosity",
            "apply_patch_tool_type",
            "web_search_tool_type",
            "truncation_policy",
            "supports_parallel_tool_calls",
            "supports_image_detail_original",
            "experimental_supported_tools",
            "supports_search_tool",
            "use_responses_lite",
            "multi_agent_version",
        ] {
            assert!(entry.get(key).is_some(), "missing required field {key}");
        }
        assert_eq!(entry["shell_type"], "shell_command");
        assert_eq!(entry["truncation_policy"]["mode"], "tokens");
        assert_eq!(entry["input_modalities"], json!(["text"]));
        assert_eq!(entry["supports_image_detail_original"], false);
        assert_eq!(entry["supports_search_tool"], true);
        assert_eq!(entry["multi_agent_version"], "v2");
    }

    #[test]
    fn third_party_deferred_tool_search_does_not_follow_brave_availability() {
        let grok = route(ProviderKind::GrokCli, "grok-cli", "grok-4.6");
        let catalog = catalog_json(&[grok]);
        assert_eq!(catalog["models"][0]["supports_search_tool"], true);
    }

    /// Issue #6 §B: the third-party prompt used to be `models.first()`, so it
    /// changed whenever OpenAI reordered its catalog. It must now depend only
    /// on the resolved harness profile.
    #[test]
    fn third_party_prompt_does_not_depend_on_official_catalog_ordering() {
        let third = route(ProviderKind::GrokCli, "grok-cli", "grok-4.5");
        let prompt_with = |models: Value| {
            let official = json!({"models": models});
            let value = catalog_json_with_official(std::slice::from_ref(&third), Some(&official));
            value["models"]
                .as_array()
                .unwrap()
                .iter()
                .find(|model| model["slug"].as_str().unwrap().starts_with("vlm-"))
                .unwrap()["base_instructions"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let sol_first = prompt_with(json!([
            {"slug": "gpt-5.6-sol", "base_instructions": "sol instructions", "priority": 1},
            {"slug": "gpt-5.6-luna", "base_instructions": "luna instructions", "priority": 3}
        ]));
        let luna_first = prompt_with(json!([
            {"slug": "gpt-5.6-luna", "base_instructions": "luna instructions", "priority": 3},
            {"slug": "gpt-5.6-sol", "base_instructions": "sol instructions", "priority": 1}
        ]));
        let no_official = prompt_with(json!([]));
        assert_eq!(sol_first, luna_first);
        assert_eq!(sol_first, no_official);
        assert!(!sol_first.contains("sol instructions"));
        assert!(!sol_first.contains("luna instructions"));
        assert!(sol_first.contains("apply_patch"));
        assert!(sol_first.contains("exact returned function schema"));
        assert!(sol_first.contains("create_thread and fork_thread are not substitutes"));
        assert!(sol_first.contains("must not\nspawn another child"));
    }

    /// Issue #6 DoD: catalog generation and the request adapter must resolve
    /// the same profile, and the generated entry must satisfy it.
    #[test]
    fn generated_entries_satisfy_the_profile_the_adapter_resolves() {
        let routes = vec![
            route(ProviderKind::GrokCli, "grok-cli", "grok-4.5"),
            route(ProviderKind::OpenAiCompatible, "compat", "GLM-5.2"),
        ];
        let value = catalog_json(&routes);
        let models = value["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        for (entry, source) in models.iter().zip(&routes) {
            let adapter_profile = crate::harness::resolve(source.provider_kind, source.wire);
            let catalog_profile =
                profile_for(&routes, &model_routes(std::slice::from_ref(source))[0]);
            HarnessProfile::assert_consistent(&catalog_profile, &adapter_profile).unwrap();
            adapter_profile.verify_catalog_entry(entry).unwrap();
            // Provider-native Sol flags stay off; host-native V2 is explicit.
            assert_eq!(entry["use_responses_lite"], false);
            assert_eq!(entry["supports_parallel_tool_calls"], false);
            assert!(entry.get("tool_mode").is_none());
            assert_eq!(entry["multi_agent_version"], "v2");
        }
    }

    /// Issue #6: `ultra` is maximum reasoning *with automatic task delegation*.
    /// Grok's own effort cache can report it, and before this gate it reached
    /// the catalog unchecked — Codex would have offered a mode with no
    /// delegation runtime behind it.
    #[test]
    fn ultra_never_reaches_a_third_party_catalog_without_a_delegation_runtime() {
        assert!(
            !crate::harness::multi_agent::delegation_available(),
            "this test describes the ungated default"
        );
        let mut provider = route(ProviderKind::OpenAiCompatible, "third", "model-a");
        provider.model_capabilities = vec![ModelCapability {
            model: "model-a".into(),
            reasoning_efforts: vec!["low".into(), "medium".into(), "high".into(), "ultra".into()],
            default_reasoning_effort: Some("ultra".into()),
            reasoning_effort_transport: ReasoningEffortTransport::ResponsesObject,
            probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
            effort_probe_version: Some(crate::probe::EFFORT_PROBE_VERSION),
            ..Default::default()
        }];

        let models = model_routes(std::slice::from_ref(&provider));
        assert_eq!(models[0].reasoning_efforts, vec!["low", "medium", "high"]);
        // A default of `ultra` falls back to the strongest permitted level
        // rather than advertising a mode with no runtime.
        assert_eq!(models[0].default_reasoning_effort.as_deref(), Some("high"));

        let value = catalog_json(&[provider]);
        let entry = &value["models"][0];
        let advertised = entry["supported_reasoning_levels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|level| level["effort"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(!advertised.contains(&"ultra"));
        assert_eq!(entry["default_reasoning_level"], "high");
    }

    /// The delegation gate must leave the provider default unresolved so the
    /// catalog can apply its balanced fallback consistently.
    #[test]
    fn the_delegation_gate_leaves_a_missing_default_to_the_catalog_fallback() {
        let mut provider = route(ProviderKind::OpenAiCompatible, "third", "model-a");
        provider.model_capabilities = vec![ModelCapability {
            model: "model-a".into(),
            reasoning_efforts: vec!["high".into(), "medium".into()],
            default_reasoning_effort: None,
            reasoning_effort_transport: ReasoningEffortTransport::ResponsesObject,
            probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
            effort_probe_version: Some(crate::probe::EFFORT_PROBE_VERSION),
            ..Default::default()
        }];

        let models = model_routes(std::slice::from_ref(&provider));
        assert_eq!(models[0].reasoning_efforts, vec!["high", "medium"]);
        assert_eq!(models[0].default_reasoning_effort, None);

        // The catalog's fallback prefers medium when the provider omitted a
        // default, regardless of the provider's capability-list ordering.
        let value = catalog_json(&[provider]);
        assert_eq!(value["models"][0]["default_reasoning_level"], "medium");
    }

    #[test]
    fn official_reasoning_levels_are_never_rewritten_by_the_delegation_gate() {
        // Official models run Codex's own delegation runtime; Vellum does not
        // translate them and must not edit what their catalog declares.
        let official = json!({
            "models": [{
                "slug": "gpt-5.6-sol",
                "supported_reasoning_levels": [{"effort": "high"}, {"effort": "ultra"}],
                "default_reasoning_level": "ultra"
            }]
        });
        let routes = model_routes_with_official_catalog(
            &[route(ProviderKind::Official, "official", "stale")],
            Some(&official),
        );
        assert_eq!(routes[0].reasoning_efforts, vec!["high", "ultra"]);
        assert_eq!(routes[0].default_reasoning_effort.as_deref(), Some("ultra"));
    }

    /// An OpenCode Zen route that happens to expose a model literally named
    /// `gpt-5.4-mini` must stay a genuine third-party `OpenAiCompatible`
    /// entry: its catalog id, Effort levels/default, and wire never come
    /// from the Official catalog entry of the same name, even when both are
    /// configured side by side (Official gates every Official-only field on
    /// `route.provider_kind == Official`, never on the model string).
    #[test]
    fn opencode_zen_gpt_named_model_never_inherits_official_catalog_data() {
        let official = json!({
            "models": [{
                "slug": "gpt-5.4-mini",
                "display_name": "GPT-5.4 mini (Official)",
                "context_window": 400_000,
                "supported_reasoning_levels": [{"effort": "medium"}, {"effort": "high"}],
                "default_reasoning_level": "high"
            }]
        });
        let mut zen_route = route(
            ProviderKind::OpenAiCompatible,
            "opencode-zen",
            "gpt-5.4-mini",
        );
        zen_route.base_url = crate::probe::OPENCODE_ZEN_BASE_URL.into();
        zen_route.models = vec!["gpt-5.4-mini".into()];
        zen_route.selected_models = Some(vec!["gpt-5.4-mini".into()]);
        zen_route.context_window = Some(128_000);
        zen_route.model_capabilities = vec![ModelCapability {
            model: "gpt-5.4-mini".into(),
            context_window: Some(128_000),
            wire: Some(WireFormat::Responses),
            streaming: Some(true),
            reasoning: Some(true),
            tool_calling: Some(true),
            probe_version: Some(crate::probe::HARNESS_PROBE_VERSION),
            // This route's own verified Effort levels -- deliberately
            // disjoint from the Official entry's, so any leak is obvious.
            reasoning_efforts: vec!["low".into()],
            default_reasoning_effort: Some("low".into()),
            reasoning_effort_transport: ReasoningEffortTransport::ChatField,
            effort_probe_version: Some(crate::probe::EFFORT_PROBE_VERSION),
            ..Default::default()
        }];

        let routes = model_routes_with_official_catalog(
            &[
                route(ProviderKind::Official, "official", "gpt-5.4-mini"),
                zen_route,
            ],
            Some(&official),
        );

        let official_entry = routes
            .iter()
            .find(|entry| entry.route_id == "official")
            .unwrap();
        let zen_entry = routes
            .iter()
            .find(|entry| entry.route_id == "opencode-zen")
            .unwrap();

        // Distinct, namespaced catalog ids -- never collapsed into one.
        assert_eq!(official_entry.catalog_id, "gpt-5.4-mini");
        assert_ne!(zen_entry.catalog_id, official_entry.catalog_id);

        assert_eq!(official_entry.reasoning_efforts, vec!["medium", "high"]);
        assert_eq!(official_entry.context_window, Some(400_000));

        assert_eq!(zen_entry.reasoning_efforts, vec!["low"]);
        assert_eq!(zen_entry.default_reasoning_effort.as_deref(), Some("low"));
        assert_eq!(zen_entry.context_window, Some(128_000));
        assert_eq!(zen_entry.wire, WireFormat::Responses);
    }

    #[test]
    fn official_routing_uses_every_cached_model() {
        let official = json!({
            "models": [
                {"slug": "gpt-a", "display_name": "GPT A", "context_window": 272000},
                {"slug": "gpt-b", "context_window": 128000}
            ]
        });
        let routes = model_routes_with_official_catalog(
            &[route(ProviderKind::Official, "official", "stale-local-id")],
            Some(&official),
        );
        assert_eq!(
            routes
                .iter()
                .map(|route| route.catalog_id.as_str())
                .collect::<Vec<_>>(),
            vec!["gpt-a", "gpt-b"]
        );
        assert_eq!(routes[0].context_window, Some(272_000));
        assert_eq!(routes[1].context_window, Some(128_000));
        assert_eq!(routes[0].display_name, "GPT A");
        assert_eq!(routes[1].display_name, "gpt-b");
    }
}
