//! Harness-profile-aware request translation shared by Desktop and the
//! headless daemon (plan M3B).
//!
//! Everything here is driven by explicit inputs — the resolved
//! [`HarnessProfile`], the route fields translation actually reads
//! ([`ProfileAdapterRoute`]), the executor contract ([`ExecutionEnvironment`]),
//! and the catalog entry. The caller resolves all of them; this module never
//! reads the environment or decides a capability itself, so Desktop and the
//! daemon translate identical requests identically.
//!
//! The resolved profile is the single authority for the whole translation: it
//! is resolved once by the caller and passed down into tool translation,
//! prompt generation, and history translation, so no downstream branch can
//! quietly decide a different environment from the one the catalog described.
//! Delegation availability is *derived* from that same profile's
//! `multi_agent_policy`, never accepted as a second caller-supplied fact, so
//! the tool surface and the reasoning gate cannot disagree.

use crate::compaction::{is_canonical_checkpoint_item, is_summary_item};
use crate::environment::ExecutionEnvironment;
use crate::harness::snapshot::{ModelVisibleHarnessSnapshot, ToolSnapshot, UnsupportedTool};
use crate::harness::tools::{self as harness_tools, BuiltinDecision};
use crate::harness::{HarnessProfile, MultiAgentPolicy, PatchContract, ToolHistoryContract};
use crate::replay::{
    apply_checkpoint_resume_tail_to_projection, apply_neutral_user_bridge_to_projection,
    chat_transcript_diagnostics_with_provenance, check_chat_transcript_invariants_with_provenance,
    should_replay_reasoning, ChatMessageProvenance, ChatProjection, HarnessTranscriptDiagnostics,
    ReplayContext, CHAT_CHECKPOINT_PREFACE,
};
use crate::route::{
    RuntimeChatCapabilities, RuntimeProviderKind, RuntimeReasoningEffortTransport,
    RuntimeWireFormat,
};
use base64::Engine as _;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Cursor;

const CUSTOM_TOOL_INPUT_FIELD: &str = "input";
const MAX_COMPRESSIBLE_IMAGE_BASE64_BYTES: usize = 64 * 1024 * 1024;

/// The route fields the profile-aware translation actually reads.
///
/// Deliberately narrower than [`crate::route::RuntimeModelRoute`]: translation
/// only needs the upstream model name, reasoning and vision flags, the
/// provider kind, the wire format, and the base URL. It must NOT carry execution authority —
/// a credential reference, compaction, or tool capabilities live on the full
/// route and are resolved separately when a request is actually executed
/// (M3C onward). Callers that only translate (Desktop's request adapter today,
/// a daemon translating a snapshot) build this projection; they cannot be
/// tempted to feed it to a router as a full [`crate::route::RuntimeModelRoute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileAdapterRoute {
    pub upstream_model: String,
    pub reasoning: bool,
    pub vision: bool,
    pub provider_kind: RuntimeProviderKind,
    pub wire: RuntimeWireFormat,
    pub base_url: String,
    pub chat_capabilities: RuntimeChatCapabilities,
    pub tool_capabilities: crate::route::RuntimeToolCapabilities,
    /// How this route's verified Effort transport wants
    /// `reasoning.effort` relocated before the request leaves this process —
    /// see `apply_reasoning_effort_transport`. Read from
    /// `RuntimeModelRoute::reasoning_capabilities`, never re-derived here.
    pub reasoning_effort_transport: RuntimeReasoningEffortTransport,
}

impl From<&crate::route::RuntimeModelRoute> for ProfileAdapterRoute {
    fn from(route: &crate::route::RuntimeModelRoute) -> Self {
        Self {
            upstream_model: route.upstream_model.clone(),
            reasoning: route.reasoning,
            vision: route.vision,
            provider_kind: route.provider_kind,
            wire: route.wire,
            base_url: route.base_url.clone(),
            chat_capabilities: route.chat_capabilities.clone(),
            tool_capabilities: route.tool_capabilities.clone(),
            reasoning_effort_transport: route.reasoning_capabilities.reasoning_effort_transport,
        }
    }
}

/// Arguments for a translated custom tool call, using whichever field the
/// tool's model-visible contract declares.
fn custom_tool_arguments(name: &str, input: Value) -> String {
    let field = if harness_tools::is_apply_patch(name) {
        harness_tools::APPLY_PATCH_FIELD
    } else {
        CUSTOM_TOOL_INPUT_FIELD
    };
    json!({ field: input }).to_string()
}

/// Translate one Codex request for an upstream provider.
///
/// Issue #6 review, P0-1: the resolved [`HarnessProfile`] is the single
/// authority for this whole function. It is resolved once, here, and passed
/// down into tool translation, prompt generation, and history translation, so
/// no downstream branch can quietly decide a different environment from the
/// one the catalog described.
///
/// `catalog_entry` is the entry Codex will actually read for this model. When
/// supplied it is verified against the resolved profile and a mismatch is an
/// **error**, not a log line: a catalog that advertises a capability the
/// runtime cannot execute is the precise failure this harness exists to
/// prevent, and forwarding the request anyway would defeat the check. Its
/// `base_instructions` is also the exact string the request-time prompt
/// replacement matches against (never a live probe of this process's shell).
///
/// Delegation availability is derived from `profile.multi_agent_policy`, the
/// same fact the tool surface derives from, so a stale catalog or a
/// hand-written request can still ask for a delegating effort and the request
/// path refuses it — while never admitting a policy/bool pair that disagrees.
///
/// `web_search_enabled` decides whether the local `web_search` compatibility
/// wrapper (Vellum's own Brave-backed tool, executed locally — distinct from
/// a provider's *native* search passthrough, which this flag never touches)
/// stays on the outgoing tool list for a third-party route. The caller
/// resolves this the same way [`crate::snapshot::WebSearchPolicySnapshot`] is
/// populated (search enabled AND a usable backend key configured); it is
/// never re-derived here.
/// Chat-wire preparation carries provenance-backed diagnostics so production
/// exec can record hashed occurrence IDs without recomputing from bare
/// messages (which would drop provenance at the exec boundary).
#[derive(Debug, Clone)]
pub struct PreparedUpstreamRequest {
    pub body: Value,
    pub chat_diagnostics: Option<HarnessTranscriptDiagnostics>,
    pub chat_provenance: Vec<ChatMessageProvenance>,
}

pub fn prepare_upstream_request_with_environment(
    original: &Value,
    route: &ProfileAdapterRoute,
    profile: &HarnessProfile,
    environment: &ExecutionEnvironment,
    catalog_entry: Option<&Value>,
    web_search_enabled: bool,
) -> Result<Value, String> {
    prepare_upstream_request_with_replay(
        original,
        route,
        profile,
        environment,
        catalog_entry,
        web_search_enabled,
        &ReplayContext::default(),
    )
}

pub fn prepare_upstream_request_with_replay(
    original: &Value,
    route: &ProfileAdapterRoute,
    profile: &HarnessProfile,
    environment: &ExecutionEnvironment,
    catalog_entry: Option<&Value>,
    web_search_enabled: bool,
    replay: &ReplayContext,
) -> Result<Value, String> {
    Ok(prepare_upstream_request_details(
        original,
        route,
        profile,
        environment,
        catalog_entry,
        web_search_enabled,
        replay,
    )?
    .body)
}

pub fn prepare_upstream_request_details(
    original: &Value,
    route: &ProfileAdapterRoute,
    profile: &HarnessProfile,
    environment: &ExecutionEnvironment,
    catalog_entry: Option<&Value>,
    web_search_enabled: bool,
    replay: &ReplayContext,
) -> Result<PreparedUpstreamRequest, String> {
    if let Some(entry) = catalog_entry {
        profile
            .verify_catalog_entry(entry)
            .map_err(|error| error.to_string())?;
    }
    // Issue #6: `ultra` is maximum reasoning *with automatic task delegation*.
    // The catalog no longer advertises it without a delegation runtime, but a
    // stale catalog or a hand-written request can still ask for it, so the
    // request path refuses it too rather than forwarding a mode whose contract
    // this route cannot honour. Availability is the profile's own
    // `multi_agent_policy` — the same authority the tool surface reads — so
    // the two can never disagree about whether delegation exists.
    let delegation_available = profile.multi_agent_policy == MultiAgentPolicy::VellumDelegated;
    if profile.kind.is_translated() && !delegation_available {
        if let Some(effort) = original
            .get("reasoning")
            .and_then(|reasoning| reasoning.get("effort"))
            .and_then(Value::as_str)
        {
            if crate::harness::multi_agent::is_delegating_effort(effort) {
                return Err(format!(
                    "reasoning effort `{effort}` requires a verified delegation runtime, which this route does not have; choose a lower effort"
                ));
            }
        }
    }
    // Issue #3 / #4: Official is semantic passthrough — exact model mapping only.
    // Do not sanitize/strip reasoning, rewrite item IDs, or hydrate here.
    if !profile.kind.is_translated() {
        return Ok(PreparedUpstreamRequest {
            body: crate::adapter::prepare_openai_official_native(original, &route.upstream_model),
            chat_diagnostics: None,
            chat_provenance: Vec::new(),
        });
    }
    let mut canonical = original.clone();
    // Native multi-agent V2 delivers the child's real assignment inside an
    // `agent_message`, including a locally readable `encrypted_content` part.
    // Materialize that one collaboration item before the generic cross-realm
    // scrub removes every field with that name. All remaining opaque provider
    // state is still stripped below and can never reach a translated route.
    materialize_portable_agent_messages(&mut canonical);
    if route.reasoning {
        // OpenAI Official returned above and remains byte-for-byte passthrough.
        // Every third-party provider uses the provider-neutral readable
        // reasoning summary. Opaque ciphertext must never enter Codex history.
        crate::adapter::strip_opaque_reasoning_keep_summary(&mut canonical);
    } else {
        crate::adapter::strip_cross_realm_fields(&mut canonical);
    }
    canonical["model"] = Value::String(route.upstream_model.clone());
    // Captured before any wire-specific reshaping below, so a later Grok/Chat
    // normalization pass touching `reasoning` can never change which value
    // `apply_reasoning_effort_transport` relocates. Codex always sends this
    // as a nested Responses-shaped object regardless of the route's actual
    // upstream wire.
    let canonical_effort = canonical
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
        .map(str::to_string);
    if route.provider_kind == RuntimeProviderKind::GrokCli {
        normalize_grok_responses_request(&mut canonical, profile, web_search_enabled)?;
    } else if route.provider_kind == RuntimeProviderKind::OpenAiCompatible
        && route.wire == RuntimeWireFormat::Responses
    {
        normalize_compatible_responses_request(&mut canonical, profile, web_search_enabled)?;
    }
    // Issue #6 review, P0-2: the tool-aware prompt has to reach the model.
    // Generating it for a snapshot and shipping the catalog-time baseline
    // meant the recorded prompt hash described a prompt nobody received.
    // Applied before the Chat conversion so `instructions` becomes the system
    // message on that path rather than needing a second, divergent write.
    let (tools, _) = model_visible_tools(&canonical, route.wire, profile);
    let catalog_baseline = catalog_entry
        .and_then(|entry| entry.get("base_instructions"))
        .and_then(Value::as_str);
    apply_request_time_instructions(
        &mut canonical,
        profile,
        &tools,
        environment,
        catalog_baseline,
    );
    if route.wire == RuntimeWireFormat::Responses {
        // A Responses-wire body already carries `reasoning.effort` in its
        // native, standard location — that needs no relocation for
        // `ResponsesObject`/`None`. `ProviderSpecific` is the one case a
        // standards-shaped Responses body still cannot reach: a Jinja
        // chat-template deployment (llama.cpp, vLLM) that reads
        // `chat_template_kwargs.reasoning_effort` regardless of which wire
        // shape the surrounding request otherwise uses — see
        // `apply_reasoning_effort_transport`.
        if let (Some(effort), RuntimeReasoningEffortTransport::ProviderSpecific) = (
            canonical_effort.as_deref(),
            route.reasoning_effort_transport,
        ) {
            apply_reasoning_effort_transport(
                &mut canonical,
                effort,
                RuntimeReasoningEffortTransport::ProviderSpecific,
            );
        }
        return Ok(PreparedUpstreamRequest {
            body: canonical,
            chat_diagnostics: None,
            chat_provenance: Vec::new(),
        });
    }
    // Some reasoning-capable Chat providers (including OpenCode DeepSeek
    // Thinking) reject `tool_choice: "required"`. The converter adds that
    // value as a Vellum heuristic for workspace-action prompts, so suppress
    // only that inferred value on reasoning routes. An explicit caller choice
    // remains authoritative and is forwarded unchanged.
    let suppress_inferred_required_tool_choice = route.reasoning
        && has_auto_or_missing_tool_choice(&canonical)
        && should_require_initial_workspace_tool(&canonical);
    let (mut chat, projection) = responses_to_chat_projection(
        &canonical,
        profile,
        &route.chat_capabilities,
        replay,
        route.vision,
    )?;
    check_chat_transcript_invariants_with_provenance(
        &projection.messages,
        &projection.provenance,
        &route.chat_capabilities,
        replay,
    )?;
    let chat_diagnostics = chat_transcript_diagnostics_with_provenance(
        &projection.messages,
        &projection.provenance,
        replay,
    );
    let chat_provenance = projection.provenance.clone();
    if suppress_inferred_required_tool_choice
        && chat.get("tool_choice").and_then(Value::as_str) == Some("required")
    {
        chat.as_object_mut()
            .expect("Responses-to-Chat conversion returns an object")
            .remove("tool_choice");
    }
    apply_provider_chat_options(route, &mut chat);
    if let Some(effort) = canonical_effort.as_deref() {
        apply_reasoning_effort_transport(&mut chat, effort, route.reasoning_effort_transport);
    }
    fit_chat_images_to_provider_request_budget(route, &mut chat);
    Ok(PreparedUpstreamRequest {
        body: chat,
        chat_diagnostics: Some(chat_diagnostics),
        chat_provenance,
    })
}

/// Console Go applies a 4.5 MiB limit to the complete JSON request. Codex's
/// `view_image` result is a data URL, so one tall PNG can exceed that limit
/// even though Vellum's inbound safety bound is intentionally much larger.
/// Re-encode only when the complete translated request exceeds that hard cap.
/// Unsupported images remain intact. If the best-effort result still cannot
/// fit, let the provider return its canonical error instead of inventing a
/// local category.
fn fit_chat_images_to_provider_request_budget(route: &ProfileAdapterRoute, body: &mut Value) {
    if !crate::opencode::is_opencode_go_endpoint(&route.base_url) {
        return;
    }
    fit_data_images_to_request_budget(
        body,
        crate::opencode::OPENCODE_GO_REQUEST_BODY_BYTES,
        crate::opencode::OPENCODE_GO_REQUEST_TARGET_BYTES,
    );
}

fn fit_data_images_to_request_budget(body: &mut Value, max_bytes: usize, target_bytes: usize) {
    debug_assert!(target_bytes < max_bytes);
    if encoded_json_len(body) <= max_bytes {
        return;
    }

    // One pass normally handles a large view_image result. Later passes keep
    // multi-image requests bounded without penalizing ordinary screenshots.
    for (long_edge, quality) in [(2048, 85), (1536, 78), (1024, 72), (768, 65)] {
        rewrite_data_images(body, long_edge, quality);
        if encoded_json_len(body) <= target_bytes {
            return;
        }
    }
}

fn encoded_json_len(body: &Value) -> usize {
    serde_json::to_vec(body).map_or(usize::MAX, |encoded| encoded.len())
}

fn rewrite_data_images(value: &mut Value, max_long_edge: u32, quality: u8) {
    match value {
        Value::String(url) if url.starts_with("data:image/") => {
            if let Some(compressed) = compress_data_image(url, max_long_edge, quality) {
                *url = compressed;
            }
        }
        Value::Array(values) => {
            for value in values {
                rewrite_data_images(value, max_long_edge, quality);
            }
        }
        Value::Object(object) => {
            for value in object.values_mut() {
                rewrite_data_images(value, max_long_edge, quality);
            }
        }
        _ => {}
    }
}

fn compress_data_image(url: &str, max_long_edge: u32, quality: u8) -> Option<String> {
    let (metadata, encoded) = url.split_once(',')?;
    if !metadata.ends_with(";base64") || encoded.len() > MAX_COMPRESSIBLE_IMAGE_BASE64_BYTES {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(32_768);
    limits.max_image_height = Some(32_768);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let resized = if image.width().max(image.height()) > max_long_edge {
        image.resize(max_long_edge, max_long_edge, FilterType::Lanczos3)
    } else {
        image
    };
    let rgba = resized.to_rgba8();
    let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
    for (target, source) in rgb.pixels_mut().zip(rgba.pixels()) {
        let alpha = u16::from(source[3]);
        for channel in 0..3 {
            target[channel] =
                ((u16::from(source[channel]) * alpha + 255 * (255 - alpha) + 127) / 255) as u8;
        }
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, quality)
        .encode_image(&rgb)
        .ok()?;
    let compressed = format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(jpeg)
    );
    (compressed.len() < url.len()).then_some(compressed)
}

/// Replace the catalog-time baseline instructions with the tool-aware ones.
///
/// Only the baseline is replaced: Codex appends the user's own instructions
/// (AGENTS.md and friends) to `base_instructions`, and overwriting the whole
/// field would silently discard them.
///
/// `environment` is the executor contract the model's tools will run under,
/// and `catalog_baseline` is the exact `base_instructions` the catalog shipped
/// for this route — the string Codex's incoming `instructions` was built from.
/// The comparison never derives a second baseline from this process's own
/// shell: the proxy host can differ from the Codex executor (e.g. a Linux
/// container under a Windows proxy), and matching against a live host probe
/// could replace the wrong baseline and ship a contradictory shell contract
/// (the old contract plus the new one).
fn apply_request_time_instructions(
    body: &mut Value,
    profile: &HarnessProfile,
    tools: &[ToolSnapshot],
    environment: &ExecutionEnvironment,
    catalog_baseline: Option<&str>,
) {
    let capabilities = crate::harness::shell::TerminalCapabilities::from(environment);
    let baseline = crate::harness::prompt::instructions(profile, &capabilities, &[]);
    let parallel_guidance = std::env::var("VELLUM_EVAL_PARALLEL_GUIDANCE")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "on" | "true"
            )
        })
        .unwrap_or(false);
    let action_bias_guidance = !std::env::var("VELLUM_EVAL_ACTION_BIAS_GUIDANCE")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "off" | "false"
            )
        })
        .unwrap_or(false);
    let tool_aware = crate::harness::prompt::instructions_with_options(
        profile,
        &capabilities,
        tools,
        parallel_guidance,
        action_bias_guidance,
    );
    if baseline == tool_aware {
        return;
    }
    let Some(object) = body.as_object_mut() else {
        return;
    };
    let updated = match (
        object.get("instructions").and_then(Value::as_str),
        catalog_baseline,
    ) {
        (Some(existing), _) if existing.contains(&tool_aware) => return,
        (Some(existing), Some(catalog_baseline)) if existing.contains(catalog_baseline) => {
            existing.replacen(catalog_baseline, &tool_aware, 1)
        }
        // The catalog on disk predates this harness, or Codex composed the
        // instructions differently. Append rather than overwrite so the
        // contract still reaches the model without dropping anything.
        (Some(existing), _) => format!("{existing}\n\n{tool_aware}"),
        (None, _) => tool_aware,
    };
    object.insert("instructions".into(), Value::String(updated));
}

/// The instructions the upstream provider will actually receive.
///
/// On the Chat wire `responses_to_chat_with_profile` has already folded them
/// into the leading system message, so the snapshot reads them back from there
/// instead of re-deriving them.
fn outgoing_instructions(body: &Value, wire: RuntimeWireFormat) -> String {
    match wire {
        RuntimeWireFormat::Responses => body
            .get("instructions")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        RuntimeWireFormat::Chat => body
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|message| message.get("role").and_then(Value::as_str) == Some("system"))
            .and_then(|message| message.get("content").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string(),
    }
}

/// The tool surface as the model will see it, plus what was deliberately
/// dropped. Shared by prompt generation and the harness snapshot so the two
/// can never describe different tool lists.
fn model_visible_tools(
    body: &Value,
    wire: RuntimeWireFormat,
    profile: &HarnessProfile,
) -> (Vec<ToolSnapshot>, Vec<UnsupportedTool>) {
    let mut tools = Vec::new();
    let mut unsupported = Vec::new();
    for tool in body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let origin = tool
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("function")
            .to_string();
        if !matches!(
            origin.as_str(),
            "function" | "custom" | "namespace" | "tool_search"
        ) {
            if let BuiltinDecision::Unsupported { reason } = harness_tools::builtin_tool(&origin) {
                unsupported.push(UnsupportedTool {
                    name: origin.clone(),
                    reason,
                });
                continue;
            }
        }
        // Both translators are identity on an already-translated function
        // tool, so this is correct whether the body has been normalized yet.
        let translated = if wire == RuntimeWireFormat::Chat {
            chat_tools(tool, profile)
        } else {
            grok_function_tools(tool, profile)
        };
        for entry in translated {
            let function = entry.get("function").unwrap_or(&entry);
            let Some(name) = function.get("name").and_then(Value::as_str) else {
                continue;
            };
            tools.push(ToolSnapshot::new(
                name,
                origin.clone(),
                harness_tools::is_mutating_tool(name),
                function
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"})),
            ));
        }
    }
    (tools, unsupported)
}

/// Provider Chat options applied after the Responses→Chat conversion.
fn apply_provider_chat_options(route: &ProfileAdapterRoute, body: &mut Value) {
    let nvidia_nim = route
        .base_url
        .to_ascii_lowercase()
        .contains("integrate.api.nvidia.com");
    if nvidia_nim {
        // `prompt_cache_key` belongs to OpenAI's request surface. NVIDIA NIM
        // rejects it instead of ignoring unknown parameters, so never forward
        // it across this provider boundary.
        if let Some(object) = body.as_object_mut() {
            object.remove("prompt_cache_key");
        }
    }
    let lower_model = route.upstream_model.to_ascii_lowercase();
    if !nvidia_nim || !(lower_model.contains("nemotron") || lower_model.contains("reasoning")) {
        return;
    }
    let Some(object) = body.as_object_mut() else {
        return;
    };
    object.entry("temperature").or_insert_with(|| json!(1));
    object.entry("top_p").or_insert_with(|| json!(0.95));
    object
        .entry("chat_template_kwargs")
        .or_insert_with(|| json!({"enable_thinking": true}));
    let budget = object
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(16_384)
        .clamp(1, 16_384);
    object
        .entry("reasoning_budget")
        .or_insert_with(|| json!(budget));
}

/// Inserts `patch`'s own keys into `target[key]` (creating the object if
/// absent) without clobbering sibling keys already there. Needed because
/// `chat_template_kwargs` is not this function's exclusive territory:
/// `apply_provider_chat_options` above already seeds it with
/// `enable_thinking` for NVIDIA NIM reasoning models, and a client's own
/// request may carry other entries in it too. Replacing the object outright
/// would silently discard whichever of those got there first.
fn merge_object_field(target: &mut Value, key: &str, patch: Value) {
    let Some(object) = target.as_object_mut() else {
        return;
    };
    match object.get_mut(key) {
        Some(existing) if existing.is_object() => {
            if let (Some(existing_map), Some(patch_map)) =
                (existing.as_object_mut(), patch.as_object())
            {
                for (field, value) in patch_map {
                    existing_map.insert(field.clone(), value.clone());
                }
            }
        }
        _ => {
            object.insert(key.to_string(), patch);
        }
    }
}

/// Relocates a verified Effort value into wherever `transport` says this
/// route's reviewer/model actually reads it, before the request leaves this
/// process. Codex always sends `reasoning.effort` in the standard
/// Responses-shaped location; `ResponsesObject`/`ChatObject` re-assert that
/// same nested shape (a no-op on Responses wire, the standard location on
/// Chat wire), `ChatField` writes the flat OpenAI-Chat-style
/// `reasoning_effort` field, and `ProviderSpecific` merges into
/// `chat_template_kwargs.reasoning_effort` — the convention llama.cpp and
/// vLLM use to pass a value through to a custom Jinja chat template's own
/// `reasoning_effort` variable, for deployments unreachable through either
/// standard encoding (see `probe::apply_effort_field`, the probe-side
/// counterpart this mirrors). `ProviderSpecific` also strips the standard
/// `reasoning.effort` field it relocated out of (via
/// `strip_relocated_effort_field`), so the outbound body carries exactly one
/// Effort encoding instead of one the deployment reads and one it silently
/// ignores; `reasoning.summary` and any other sibling under `reasoning`
/// survive untouched. `None` — never probed, or confirmed unsupported —
/// leaves the body untouched.
pub(crate) fn apply_reasoning_effort_transport(
    body: &mut Value,
    effort: &str,
    transport: RuntimeReasoningEffortTransport,
) {
    match transport {
        RuntimeReasoningEffortTransport::ResponsesObject
        | RuntimeReasoningEffortTransport::ChatObject => {
            if let Some(object) = body.as_object_mut() {
                object.insert("reasoning".into(), json!({"effort": effort}));
            }
        }
        RuntimeReasoningEffortTransport::ChatField => {
            if let Some(object) = body.as_object_mut() {
                object.insert("reasoning_effort".into(), json!(effort));
            }
        }
        RuntimeReasoningEffortTransport::ProviderSpecific => {
            merge_object_field(
                body,
                "chat_template_kwargs",
                json!({"reasoning_effort": effort}),
            );
            strip_relocated_effort_field(body);
        }
        RuntimeReasoningEffortTransport::None => {}
    }
}

/// Removes the standard-location Effort representation once
/// `ProviderSpecific` has relocated it into `chat_template_kwargs`, so the
/// upstream deployment never sees both a `reasoning.effort` field it ignores
/// and the `chat_template_kwargs.reasoning_effort` field its Jinja template
/// actually reads. Only the `effort` key is removed — `reasoning.summary`
/// and any other sibling field is preserved — and the now-possibly-empty
/// `reasoning` object itself is dropped only if stripping `effort` left it
/// with nothing else in it. The flat Chat-style `reasoning_effort` key is
/// removed too, defensively: `apply_reasoning_effort_transport` never writes
/// it and ProviderSpecific, but a client-supplied request could already
/// carry it.
fn strip_relocated_effort_field(body: &mut Value) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    object.remove("reasoning_effort");
    let mut reasoning_now_empty = false;
    if let Some(reasoning) = object.get_mut("reasoning").and_then(Value::as_object_mut) {
        reasoning.remove("effort");
        reasoning_now_empty = reasoning.is_empty();
    }
    if reasoning_now_empty {
        object.remove("reasoning");
    }
}

/// Fields forwarded on the Grok Responses wire (issue #4 / endpoint inventory).
/// Grok continuity uses stable session headers plus readable reasoning
/// checkpoints, not OpenAI `previous_response_id` or foreign ciphertext.
/// Keep the sampling knobs that Grok Build's ModelInput accepts.
const GROK_REQUEST_FIELDS: &[&str] = &[
    "model",
    "instructions",
    "input",
    "tools",
    "tool_choice",
    "stream",
    "temperature",
    "top_p",
    "max_output_tokens",
    "reasoning",
    "text",
    "metadata",
    "parallel_tool_calls",
    "include",
    "prompt_cache_key",
    "store",
    "user",
];

/// Grok Build exposes a Responses-shaped endpoint, but its `ModelInput` is
/// intentionally narrower than Codex Desktop's private Responses dialect.
/// Normalize the private tool/history variants before forwarding rather than
/// sending the Codex body verbatim and relying on Grok to deserialize it.
///
/// `web_search_enabled` gates Vellum's local `web_search` compatibility
/// wrapper only. Console Go reserves the *name* `web_search` for its own
/// upstream facility regardless of this flag, so the wrapper is always
/// distinguishable and never collides with a native Grok search tool — this
/// function has no native-search passthrough to protect.
pub fn normalize_grok_responses_request(
    body: &mut Value,
    profile: &HarnessProfile,
    web_search_enabled: bool,
) -> Result<(), String> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| "Grok request body must be a JSON object".to_string())?;

    object.retain(|key, _| GROK_REQUEST_FIELDS.contains(&key.as_str()));
    object.remove("previous_response_id");

    let mut tools = normalize_responses_input_and_tools(object, profile)?;
    // Codex's local web-search compatibility wrapper is executed by Vellum,
    // not forwarded as an upstream custom function. Offer it only when the
    // caller resolved a usable local search backend (Brave key configured
    // and search enabled); otherwise strip it so the model is never told
    // about a tool that would fail closed on every call.
    if !web_search_enabled {
        tools.retain(|tool| function_tool_name(tool) != Some("web_search"));
    }
    if !tools.is_empty() {
        object.insert("tools".into(), Value::Array(tools));
        if let Some(choice) = object.get_mut("tool_choice") {
            normalize_grok_tool_choice(choice);
        }
    } else {
        object.remove("tool_choice");
        object.remove("parallel_tool_calls");
    }

    // Effort strings are capability-probed per model and must reach the
    // upstream unchanged. Silent coercion makes the Codex selector lie about
    // the level the provider actually receives.
    Ok(())
}

/// OpenAI-compatible Responses servers frequently implement only the public
/// Responses item variants. Codex Desktop also sends private history/tool
/// variants such as `additional_tools` and `local_shell_call`; normalize those
/// before they reach providers such as llama.cpp.
///
/// `web_search_enabled` gates Vellum's local `web_search` compatibility
/// wrapper only — see [`normalize_grok_responses_request`]'s doc for the same
/// contract. Official's native search passthrough never goes through this
/// OpenAI-compatible normalizer, so there is nothing here for it to affect.
pub fn normalize_compatible_responses_request(
    body: &mut Value,
    profile: &HarnessProfile,
    web_search_enabled: bool,
) -> Result<(), String> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| "OpenAI-compatible Responses body must be a JSON object".to_string())?;
    object.remove("previous_response_id");
    // Codex 0.147 added private client telemetry that is not part of the
    // public Responses schema. Strict compatible endpoints (Console Go in
    // particular) reject the whole request when it leaks through.
    object.remove("client_metadata");
    let mut tools = normalize_responses_input_and_tools(object, profile)?;
    normalize_compatible_function_call_outputs(object);
    if !web_search_enabled {
        tools.retain(|tool| function_tool_name(tool) != Some("web_search"));
    }
    if tools.is_empty() {
        object.remove("tools");
        object.remove("tool_choice");
        object.remove("parallel_tool_calls");
    } else {
        object.insert("tools".into(), Value::Array(tools));
        if let Some(choice) = object.get_mut("tool_choice") {
            normalize_grok_tool_choice(choice);
        }
    }
    // Some OpenAI-compatible Responses endpoints deserialize message items
    // through their stricter Chat template. Codex may rehydrate `developer`
    // messages after user/assistant history, but templates such as llama.cpp's
    // Ornith parser accept exactly one system message and require it to be the
    // first message. Fold every system/developer item into the canonical
    // top-level `instructions` field instead of merely renaming its role in
    // place. Official OpenAI routes never enter this compatibility transform.
    fold_compatible_responses_instructions(object);
    Ok(())
}

/// Lower rich function results to the portable subset implemented by strict
/// OpenAI-compatible Responses servers.
///
/// OpenAI permits text, image, and file content inside a function result, but
/// several compatible servers deserialize that field as text-only. Preserve
/// the completed tool pair with a string result and make every omitted rich
/// part explicit. A user-declared vision flag is not proof that the compatible
/// server loaded its multimodal projector, so this path never relocates the
/// image into another upstream field. Official routes never call this
/// compatibility transform.
fn normalize_compatible_function_call_outputs(object: &mut serde_json::Map<String, Value>) {
    let Some(Value::Array(items)) = object.get_mut("input") else {
        return;
    };

    let mut normalized = Vec::with_capacity(items.len());
    for item in std::mem::take(items) {
        let is_function_output =
            item.get("type").and_then(Value::as_str) == Some("function_call_output");
        if !is_function_output {
            normalized.push(item);
            continue;
        }

        let Value::Object(mut output_item) = item else {
            normalized.push(item);
            continue;
        };
        let output = output_item.remove("output").unwrap_or(Value::Null);
        let text = portable_compatible_tool_output(output);
        output_item.insert("output".into(), Value::String(text));
        normalized.push(Value::Object(output_item));
    }
    *items = normalized;
}

fn portable_compatible_tool_output(output: Value) -> String {
    let Value::Array(parts) = output else {
        return match output {
            Value::String(text) => text,
            Value::Null => String::new(),
            other => other.to_string(),
        };
    };

    let mut text = Vec::new();
    let mut unavailable_images = 0usize;
    let mut unavailable_files = 0usize;
    let mut unsupported_parts = 0usize;

    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("input_text" | "text") => {
                if let Some(value) = part
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    text.push(value.to_string());
                }
            }
            Some("input_image" | "image") => {
                unavailable_images += 1;
            }
            Some("input_file" | "file") => unavailable_files += 1,
            _ => {
                if let Some(value) = part
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    text.push(value.to_string());
                } else {
                    unsupported_parts += 1;
                }
            }
        }
    }

    if unavailable_images > 0 {
        text.push(format!(
            "[Tool returned {unavailable_images} image(s) that this route cannot transport.]"
        ));
    }
    if unavailable_files > 0 {
        text.push(format!(
            "[Tool returned {unavailable_files} file(s) that this compatible route cannot transport.]"
        ));
    }
    if unsupported_parts > 0 {
        text.push(format!(
            "[Tool returned {unsupported_parts} unsupported content part(s).]"
        ));
    }
    if text.is_empty() {
        text.push("[Tool completed without textual output.]".to_string());
    }
    text.join("\n")
}

fn fold_compatible_responses_instructions(object: &mut serde_json::Map<String, Value>) {
    let Some(Value::Array(items)) = object.get_mut("input") else {
        return;
    };

    let mut folded = Vec::new();
    items.retain(|item| {
        if matches!(
            item.get("role").and_then(Value::as_str),
            Some("system" | "developer")
        ) {
            let text = item
                .get("content")
                .map(crate::adapter::content_to_plain_string)
                .unwrap_or_default();
            if !text.trim().is_empty() {
                folded.push(text);
            }
            false
        } else {
            true
        }
    });
    if folded.is_empty() {
        return;
    }

    let mut instructions = object
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .into_iter()
        .collect::<Vec<_>>();
    instructions.extend(folded);
    object.insert(
        "instructions".into(),
        Value::String(instructions.join("\n\n")),
    );
}

fn normalize_responses_input_and_tools(
    object: &mut serde_json::Map<String, Value>,
    profile: &HarnessProfile,
) -> Result<Vec<Value>, String> {
    let mut embedded_tools = Vec::new();
    if let Some(Value::Array(items)) = object.get_mut("input") {
        let mut normalized = Vec::with_capacity(items.len());
        for item in std::mem::take(items) {
            if let Some(item) = normalize_grok_input_item(item, &mut embedded_tools, profile)? {
                normalized.push(item);
            }
        }
        *items = normalized;
    }

    // Top-level declarations are canonical. Embedded `additional_tools`
    // declarations only fill names that are not already present.  Do the
    // precedence merge *after* normalizing namespace/custom tools, otherwise
    // the same Codex tool (for example `_fetch`) can look structurally
    // different and be rejected even though the current top-level schema is
    // authoritative.
    let top_level_tools = object
        .remove("tools")
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    // Codex Desktop can publish a free-form custom tool and a generated
    // function compatibility wrapper with the same name in the same current
    // snapshot. The custom declaration is the executable contract; the
    // wrapper is only for clients that cannot represent custom tools. Keep the
    // custom declaration before converting everything to function tools, or
    // `_fetch` becomes two conflicting function schemas on third-party routes.
    let custom_names = top_level_tools
        .iter()
        .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("custom"))
        .filter_map(function_tool_name)
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let mut expanded_top_level = top_level_tools
        .iter()
        .filter(|tool| tool.get("type").and_then(Value::as_str) != Some("custom"))
        .flat_map(|tool| grok_function_tools(tool, profile))
        .filter(|tool| function_tool_name(tool).is_none_or(|name| !custom_names.contains(name)))
        .collect::<Vec<_>>();
    expanded_top_level.extend(
        top_level_tools
            .iter()
            .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("custom"))
            .flat_map(|tool| grok_function_tools(tool, profile)),
    );
    let mut normalized = deduplicate_function_tools(expanded_top_level, ToolShape::Responses)?;
    let canonical_names = normalized
        .iter()
        .filter_map(function_tool_name)
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let embedded = embedded_tools
        .iter()
        .flat_map(|tool| grok_function_tools(tool, profile))
        .filter(|tool| function_tool_name(tool).is_none_or(|name| !canonical_names.contains(name)))
        .collect::<Vec<_>>();
    normalized.extend(deduplicate_function_tools(embedded, ToolShape::Responses)?);
    Ok(apply_tool_mode(normalized, profile))
}

/// Nested tools stay exactly the runtimes Codex already has — Vellum expands a
/// plan into ordinary Codex tool calls — so nothing here advertises an
/// unexecutable delegation surface.
fn apply_tool_mode(mut surface: Vec<Value>, profile: &HarnessProfile) -> Vec<Value> {
    // Delegation is advertised only once its runtime has been proven; an
    // unverified runtime contributes nothing rather than a disabled-looking
    // tool, because a tool the model can see is a tool the model will try.
    if profile.multi_agent_policy == MultiAgentPolicy::VellumDelegated {
        if let Some(namespace) = crate::harness::multi_agent::delegation_namespace(true) {
            surface.extend(grok_function_tools(&namespace, profile));
        }
    }
    surface
}

fn function_tool_name(tool: &Value) -> Option<&str> {
    tool.get("name")
        .or_else(|| tool.get("function").and_then(|value| value.get("name")))
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
}

fn normalize_grok_tool_choice(choice: &mut Value) {
    let Some(object) = choice.as_object_mut() else {
        return;
    };
    match object.get("type").and_then(Value::as_str) {
        Some("namespace") => {
            let namespace = object
                .get("namespace")
                .or_else(|| object.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("namespace");
            let name = object
                .get("function")
                .or_else(|| object.get("tool"))
                .and_then(Value::as_str)
                .unwrap_or("tool");
            *choice = json!({"type": "function", "name": format!("{namespace}__{name}")});
        }
        Some("custom") => {
            let name = object
                .get("name")
                .cloned()
                .unwrap_or_else(|| json!("custom_tool"));
            *choice = json!({"type": "function", "name": name});
        }
        Some("tool_search") => {
            *choice = json!({"type": "function", "name": "tool_search"});
        }
        Some(kind) if kind != "function" => {
            *choice = Value::String("auto".into());
        }
        _ => {}
    }
}

fn strip_internal_fields(object: &mut serde_json::Map<String, Value>) {
    object.retain(|key, _| !key.starts_with("internal_"));
}

fn strip_vellum_checkpoint_metadata(object: &mut serde_json::Map<String, Value>) {
    if let Some(meta) = object.get_mut("metadata").and_then(Value::as_object_mut) {
        let is_vellum = meta.get("vellum_checkpoint").and_then(Value::as_str) == Some("canonical")
            || meta.contains_key("checkpoint");
        if is_vellum {
            meta.remove("vellum_checkpoint");
            meta.remove("checkpoint");
            meta.remove("schema_version");
            meta.remove("checkpoint_schema_version");
            meta.remove("continuity_kind");
            meta.remove("retained_tail_count");
            meta.remove("source_hash");
            meta.remove("checkpoint_hash");
            if meta.is_empty() {
                object.remove("metadata");
            }
        }
    }
}

fn normalize_grok_input_item(
    item: Value,
    embedded_tools: &mut Vec<Value>,
    profile: &HarnessProfile,
) -> Result<Option<Value>, String> {
    let Value::Object(mut object) = item else {
        return Ok(Some(item));
    };
    strip_internal_fields(&mut object);
    strip_vellum_checkpoint_metadata(&mut object);
    let item_type = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message")
        .to_string();

    match item_type.as_str() {
        "additional_tools" => {
            if let Some(tools) = object.get("tools").and_then(Value::as_array) {
                embedded_tools.extend(tools.iter().cloned());
            }
            Ok(None)
        }
        "reasoning" => Ok(sanitize_grok_reasoning_item(object)),
        "item_reference" => Ok(None),
        "compaction" => Err(
            "unmaterialized compaction item reached Grok transform; refusing to drop context"
                .to_string(),
        ),
        "local_shell_call" => {
            let call_id = object
                .get("call_id")
                .or_else(|| object.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("call_shell");
            // Issue #6 Phase 4: under a lossless history contract the argv
            // array stays an array. Flattening it with `join(" ")` is not
            // reversible — `["git", "-C", "C:\\path with space", "status"]`
            // becomes a line that re-parses as five arguments — and every such
            // history item then teaches the model that quoting is optional.
            let arguments = crate::harness::shell::shell_call_arguments(object.get("action"));
            Ok(Some(json!({
                "type": "function_call",
                "name": "shell",
                "call_id": call_id,
                "arguments": arguments.to_string()
            })))
        }
        "local_shell_call_output" => Ok(Some(json!({
            "type": "function_call_output",
            "call_id": object.get("call_id").cloned().unwrap_or_else(|| json!("call_shell")),
            "output": object.get("output").cloned().unwrap_or_else(|| json!(""))
        }))),
        "custom_tool_call" => {
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("custom_tool")
                .to_string();
            let input = object.get("input").cloned().unwrap_or_else(|| json!(""));
            Ok(Some(json!({
                "type": "function_call",
                "name": name,
                "call_id": object.get("call_id").or_else(|| object.get("id")).cloned()
                    .unwrap_or_else(|| json!("call_custom")),
                "arguments": custom_tool_arguments(&name, input)
            })))
        }
        "tool_search_call" => Ok(Some(json!({
            "type": "function_call",
            "name": "tool_search",
            "call_id": object.get("call_id").or_else(|| object.get("id")).cloned()
                .unwrap_or_else(|| json!("call_tool_search")),
            "arguments": object.get("arguments").cloned().unwrap_or_else(|| json!({})).to_string()
        }))),
        "custom_tool_call_output" | "tool_search_output" => Ok(Some(json!({
            "type": "function_call_output",
            "call_id": object.get("call_id").cloned().unwrap_or_else(|| json!("call_tool")),
            "output": object.get("output").or_else(|| object.get("tools")).cloned()
                .unwrap_or(Value::Null).to_string()
        }))),
        // Issue #6 §G: these private Codex items have no Grok ModelInput
        // equivalent. Under a lossless history contract they are marked as
        // explicitly unsupported rather than having the raw private object
        // poured into the transcript — dumping it is still lossy, it is not a
        // structured tool history, and it ships Codex internals to a
        // third-party provider.
        "web_search_call" | "computer_call" | "computer_call_output" => Ok(Some(
            unsupported_history_marker(&item_type, profile, &object),
        )),
        "function_call" => {
            // Fold `namespace` into the flattened name so the third-party
            // provider sees the same name it was advertised (`web__run`).
            // The NamespaceToolContext restores `namespace` on the response.
            if let (Some(namespace), Some(name)) = (
                object.get("namespace").and_then(Value::as_str),
                object.get("name").and_then(Value::as_str),
            ) {
                object.insert(
                    "name".into(),
                    json!(harness_tools::flatten_namespace_name(namespace, name)),
                );
            }
            object.remove("namespace");
            Ok(Some(Value::Object(object)))
        }
        "message" | "function_call_output" => Ok(Some(Value::Object(object))),
        // Native multi-agent V2 delegation, which translated routes now
        // advertise, carries every parent<->child utterance as `agent_message`.
        // Dropping those to a "content omitted" marker is what a route without
        // sub-agents should do; on a route that has them it deletes the only
        // record of what the children actually reported back. Native V2 stores
        // its locally-created tool payload in an `encrypted_content` part even
        // though that field contains the plaintext collaboration result at this
        // boundary. Carry supported parts as ordinary text and preserve the
        // author/recipient pair that gives the result meaning.
        // An `agent_message` is input from another actor to the model running
        // this thread. Project it as a user turn, not as this model's previous
        // assistant output. The latter makes a newly spawned child's first
        // request system+assistant-only; the Chat invariant then rejects the
        // real assignment before it can reach the provider.
        "agent_message" => Ok(Some(
            agent_message_as_text(&object)
                .map(|text| {
                    json!({
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": text}]
                    })
                })
                .unwrap_or_else(|| unsupported_history_marker(&item_type, profile, &object)),
        )),
        // Unknown private Codex items cannot be represented by Grok ModelInput.
        // Keep a textual history marker instead of forwarding an invalid enum.
        _ => Ok(Some(unsupported_history_marker(
            &item_type, profile, &object,
        ))),
    }
}

/// Portable text for a native multi-agent V2 `agent_message`.
///
/// `InterAgentCommunication::new_encrypted` uses the misleadingly named
/// `encrypted_content` field for the local collaboration tool's message string;
/// it is not ciphertext on this request boundary. This projection is narrowly
/// scoped to `agent_message`: opaque reasoning and compaction items remain
/// unrepresentable on translated routes.
fn agent_message_as_text(object: &serde_json::Map<String, Value>) -> Option<String> {
    let parts = object.get("content")?.as_array()?;
    if parts.is_empty() {
        return None;
    }
    let mut text_parts = Vec::with_capacity(parts.len());
    for part in parts {
        let text = match part.get("type").and_then(Value::as_str) {
            Some("input_text") | Some("output_text") | Some("text") => {
                part.get("text").and_then(Value::as_str)?
            }
            Some("encrypted_content") => part.get("encrypted_content").and_then(Value::as_str)?,
            _ => return None,
        };
        text_parts.push(text);
    }
    let body = text_parts.join("\n");
    if body.trim().is_empty() {
        return None;
    }
    let author = object.get("author").and_then(Value::as_str).unwrap_or("");
    let recipient = object
        .get("recipient")
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(
        match (author.trim().is_empty(), recipient.trim().is_empty()) {
            (false, false) => format!("[agent {author} -> {recipient}] {body}"),
            (false, true) => format!("[agent {author}] {body}"),
            _ => body,
        },
    )
}

/// Replace readable native collaboration messages with portable user turns.
///
/// This must run before cross-realm stripping: the native V2 collaboration
/// protocol stores its plaintext assignment in a content part named
/// `encrypted_content`. The narrow item-type check keeps opaque reasoning and
/// compaction ciphertext subject to the normal translated-route scrub.
fn materialize_portable_agent_messages(value: &mut Value) {
    let Some(items) = value.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items {
        let Some(object) = item.as_object() else {
            continue;
        };
        if object.get("type").and_then(Value::as_str) != Some("agent_message") {
            continue;
        }
        let Some(text) = agent_message_as_text(object) else {
            continue;
        };
        *item = json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}]
        });
    }
}

/// History placeholder for a Codex item this route cannot represent.
///
/// `LosslessTranslated` (issue #6 §G) means: say plainly that the item is
/// missing and say nothing about its contents. The older behaviour embedded
/// the raw private object, which was still lossy, was not structured tool
/// history, and forwarded Codex internals to a third-party provider.
fn unsupported_history_marker(
    item_type: &str,
    profile: &HarnessProfile,
    object: &serde_json::Map<String, Value>,
) -> Value {
    let text = match profile.history_contract {
        ToolHistoryContract::LosslessTranslated => format!(
            "[unsupported on this route: a previous Codex `{item_type}` is omitted from history. Do not assume its result; re-derive anything you need.]"
        ),
        ToolHistoryContract::Native => format!(
            "[Previous Codex {item_type}; history only] {}",
            Value::Object(object.clone())
        ),
    };
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text}]
    })
}

fn nonempty_json_string(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
}

fn reasoning_has_summary_text(summary: Option<&Value>) -> bool {
    summary.and_then(Value::as_array).is_some_and(|parts| {
        parts
            .iter()
            .any(|part| nonempty_json_string(part.get("text")))
    })
}

fn sanitize_grok_reasoning_item(mut object: serde_json::Map<String, Value>) -> Option<Value> {
    strip_internal_fields(&mut object);
    object.remove("reasoning_content");
    object.remove("content");
    object.remove("encrypted_content");
    let has_summary = reasoning_has_summary_text(object.get("summary"));
    if !has_summary {
        return None;
    }

    let mut sanitized = serde_json::Map::new();
    sanitized.insert("type".into(), Value::String("reasoning".into()));
    for key in ["id", "summary"] {
        if let Some(value) = object.remove(key) {
            if key != "id" || nonempty_json_string(Some(&value)) {
                sanitized.insert(key.to_string(), value);
            }
        }
    }
    Some(Value::Object(sanitized))
}

/// Translate one Codex tool declaration into its Grok/Responses function form,
/// governed by the resolved profile's patch contract. Shared by request
/// normalization and by hosts that need the model-visible surface of a single
/// tool (Desktop tests, a daemon applying its own tool policy).
/// Coerce a tool's parameter schema into one with an **object** root, or
/// report that it cannot be.
///
/// Codex publishes MCP tools verbatim, and an MCP server is free to declare a
/// union at the root of a tool's input schema.
/// `mcp__codex_app__automation_update` does exactly that. Providers do not
/// accept it: Grok Build answers the whole request with
/// `400 tool parameter root must be an object type (root schema is an
/// anyOf/oneOf union with a non-object branch)`. That is the important part --
/// one tool it cannot read fails *every* turn on the route, not just calls to
/// that tool, so forwarding the union unchanged takes the provider down
/// entirely. The tool is also injected by tool search rather than always
/// present, which is why a route can work for weeks and then break on the
/// first turn that mentions automations.
///
/// The union is collapsed to the object branches it does contain, because a
/// permissive superset still lets the model call the tool and the server still
/// validates the real arguments. `required` is intersected rather than unioned:
/// a field only some branches demand is not required by the merged shape, and
/// claiming otherwise would reject calls the server would have accepted.
///
/// `None` means the schema has no object branch at all. The caller drops that
/// one tool -- already an expected outcome for tools a route cannot express --
/// which costs the model one tool instead of the entire conversation.
fn object_rooted_tool_parameters(schema: &Value) -> Option<Value> {
    let Some(fields) = schema.as_object() else {
        // A bare `true`/`null` schema constrains nothing; the empty object
        // schema says the same thing in a shape every provider accepts.
        return Some(json!({"type": "object"}));
    };

    let union = fields
        .get("anyOf")
        .or_else(|| fields.get("oneOf"))
        .and_then(Value::as_array);
    let Some(branches) = union else {
        if declares_object_type(schema) {
            return Some(schema.clone());
        }
        // No union and no object marker: a scalar or array root, which has no
        // object reading. `{}` (no type, no properties) is the one benign case
        // and is handled by `declares_object_type`.
        return None;
    };

    let root_is_object = declares_object_type(schema);
    let objects = branches
        .iter()
        .filter(|branch| declares_object_type(branch))
        .collect::<Vec<_>>();
    if objects.is_empty() && !root_is_object {
        return None;
    }

    // A schema can declare `type: object` and still carry an anyOf/oneOf at
    // its root. Grok rejects that union when even one branch is non-object,
    // despite the outer type making that branch semantically unreachable.
    // Start from the outer object constraints when present, then collapse the
    // surviving object branches into a portable object-only superset.
    let mut merged = if root_is_object {
        fields.clone()
    } else {
        serde_json::Map::new()
    };
    merged.remove("anyOf");
    merged.remove("oneOf");
    merged.insert("type".into(), json!("object"));
    // Sibling annotations at the root outlive the collapse.
    for key in ["description", "title", "$defs", "definitions"] {
        if let Some(value) = fields.get(key) {
            merged.insert(key.to_string(), value.clone());
        }
    }

    let mut properties = merged
        .remove("properties")
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let root_required = merged
        .remove("required")
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|name| name.as_str().map(str::to_string))
        .collect::<Vec<_>>();
    let mut required: Option<Vec<String>> = None;
    for branch in objects {
        if let Some(branch_properties) = branch.get("properties").and_then(Value::as_object) {
            for (name, value) in branch_properties {
                // First branch wins a name collision, so the merge is stable
                // for a given tool declaration rather than order-dependent on
                // whatever the map iteration happened to yield.
                properties
                    .entry(name.clone())
                    .or_insert_with(|| value.clone());
            }
        }
        let branch_required = branch
            .get("required")
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        required = Some(match required {
            None => branch_required,
            Some(current) => current
                .into_iter()
                .filter(|name| branch_required.contains(name))
                .collect(),
        });
    }
    merged.insert("properties".into(), Value::Object(properties));
    let mut required = required.unwrap_or_default();
    for name in root_required {
        if !required.contains(&name) {
            required.push(name);
        }
    }
    if !required.is_empty() {
        merged.insert("required".into(), json!(required));
    }
    Some(Value::Object(merged))
}

/// Whether a schema reads as an object. `type` may be absent on a schema that
/// only lists `properties`, and may be a list (`["object", "null"]`) rather
/// than a string.
fn declares_object_type(schema: &Value) -> bool {
    let Some(fields) = schema.as_object() else {
        return false;
    };
    match fields.get("type") {
        Some(Value::String(name)) => name == "object",
        Some(Value::Array(names)) => names.iter().any(|name| name.as_str() == Some("object")),
        Some(_) => false,
        // No `type` at all: an object if it describes properties, and also for
        // the empty schema `{}`, which constrains nothing and is accepted
        // everywhere as an object root.
        None => {
            fields.contains_key("properties")
                || !(fields.contains_key("anyOf")
                    || fields.contains_key("oneOf")
                    || fields.contains_key("items"))
        }
    }
}

pub fn grok_function_tools(tool: &Value, profile: &HarnessProfile) -> Vec<Value> {
    match tool.get("type").and_then(Value::as_str) {
        Some("function") => {
            let source = tool.get("function").unwrap_or(tool);
            let declared = source
                .get("parameters")
                .or_else(|| source.get("input_schema"))
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            // A tool whose root schema has no object reading is dropped rather
            // than forwarded: the provider would reject the whole request.
            let Some(parameters) = object_rooted_tool_parameters(&declared) else {
                return Vec::new();
            };
            vec![json!({
                "type": "function",
                "name": source.get("name").cloned().unwrap_or_else(|| json!("unnamed_tool")),
                "description": source.get("description").cloned().unwrap_or_else(|| json!("")),
                "parameters": parameters
            })]
        }
        Some("namespace") => {
            let namespace = tool
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("namespace");
            let namespace_description = tool.get("description").and_then(Value::as_str);
            tool.get("tools")
                .or_else(|| tool.get("functions"))
                .and_then(Value::as_array)
                .map(|children| {
                    children
                        .iter()
                        .flat_map(|child| grok_function_tools(child, profile))
                        .map(|mut child| {
                            if let Some(object) = child.as_object_mut() {
                                let name = object
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("tool")
                                    .to_string();
                                object.insert(
                                    "name".into(),
                                    json!(harness_tools::flatten_namespace_name(namespace, &name)),
                                );
                                // Issue #6 §E: flattening the name throws away
                                // the grouping the native surface carried. Fold
                                // the namespace guidance into each child so the
                                // organisation survives the translation.
                                let child_description =
                                    object.get("description").and_then(Value::as_str);
                                let description = harness_tools::namespaced_description(
                                    namespace,
                                    namespace_description,
                                    child_description,
                                );
                                object.insert("description".into(), json!(description));
                            }
                            child
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        Some("custom") => {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("custom_tool");
            // Issue #6 Phase 3: `apply_patch` is the authoritative edit API, so
            // it gets an exact closed contract with the full patch grammar in
            // its description rather than an anonymous `{input: string}` blob.
            // Which shape it takes is the resolved profile's decision, not a
            // constant — that is what keeps the catalog, the prompt, and this
            // translation describing one environment.
            if harness_tools::is_apply_patch(name) {
                return match profile.patch_contract {
                    PatchContract::TranslatedExactFunction | PatchContract::NativeFreeform => {
                        vec![harness_tools::apply_patch_function_tool()]
                    }
                    // The prompt will not mention a patch tool either, so the
                    // two cannot disagree about how files get edited.
                    PatchContract::Unsupported => Vec::new(),
                };
            }
            vec![json!({
                "type": "function",
                "name": name,
                "description": tool.get("description").cloned().unwrap_or_else(|| json!("")),
                "parameters": {
                    "type": "object",
                    "properties": {"input": {"type": "string"}},
                    "required": ["input"],
                    "additionalProperties": false
                }
            })]
        }
        Some("tool_search") => vec![json!({
            "type": "function",
            "name": "tool_search",
            "description": "Search for available tools",
            "parameters": tool.get("parameters").cloned()
                .unwrap_or_else(|| json!({"type": "object"}))
        })],
        // Issue #6 §D / Phase 2: a Codex built-in resolves to an exact schema
        // or to nothing. The old permissive `additionalProperties: true`
        // fallback preserved callability and threw away every field name,
        // constraint, and mutation semantic — and a mutating tool the runtime
        // cannot dispatch is worse than an absent one.
        Some(kind) => match harness_tools::builtin_tool(kind) {
            BuiltinDecision::Exact {
                description,
                parameters,
                ..
            } => vec![json!({
                "type": "function",
                "name": kind,
                "description": description,
                "parameters": parameters
            })],
            BuiltinDecision::Unsupported { .. } => Vec::new(),
        },
        None => Vec::new(),
    }
}

#[derive(Debug, Clone, Copy)]
enum ToolShape {
    Responses,
    Chat,
}

fn deduplicate_function_tools(tools: Vec<Value>, shape: ToolShape) -> Result<Vec<Value>, String> {
    let mut seen = std::collections::HashMap::<String, Value>::new();
    let mut output = Vec::with_capacity(tools.len());
    for tool in tools {
        let function = match shape {
            ToolShape::Responses => &tool,
            ToolShape::Chat => tool.get("function").unwrap_or(&tool),
        };
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| "function tool is missing a name".to_string())?;
        let schema = function
            .get("parameters")
            .or_else(|| function.get("input_schema"))
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"}));
        if let Some(previous) = seen.get(name) {
            if previous != &schema {
                let previous_hash = short_json_hash(previous);
                let current_hash = short_json_hash(&schema);
                return Err(format!(
                    "conflicting duplicate function definition '{name}' has different parameter schemas \
                     (wire={shape:?}, first_schema={previous_hash}, second_schema={current_hash})"
                ));
            }
            continue;
        }
        seen.insert(name.to_string(), schema);
        output.push(tool);
    }
    Ok(output)
}

fn short_json_hash(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    Sha256::digest(bytes)
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Convert a canonical Responses body to Chat Completions using the generic
/// Chat profile. Prefer [`responses_to_chat_with_options`] on the live path so
/// the translation is governed by the route's own resolved contract.
pub fn responses_to_chat(body: &Value) -> Result<Value, String> {
    responses_to_chat_with_profile(body, &HarnessProfile::generic(RuntimeWireFormat::Chat))
}

pub fn responses_to_chat_with_profile(
    body: &Value,
    profile: &HarnessProfile,
) -> Result<Value, String> {
    responses_to_chat_with_options(
        body,
        profile,
        &RuntimeChatCapabilities::default(),
        &ReplayContext::default(),
    )
}

pub fn responses_to_chat_with_options(
    body: &Value,
    profile: &HarnessProfile,
    capabilities: &RuntimeChatCapabilities,
    replay: &ReplayContext,
) -> Result<Value, String> {
    Ok(responses_to_chat_projection(body, profile, capabilities, replay, true)?.0)
}

fn responses_to_chat_projection(
    body: &Value,
    profile: &HarnessProfile,
    capabilities: &RuntimeChatCapabilities,
    replay: &ReplayContext,
    vision: bool,
) -> Result<(Value, ChatProjection), String> {
    // Chat-compatible providers must receive the same normalized public Codex
    // dialect as Responses-compatible providers. In particular,
    // `additional_tools` is a private declaration item: it contributes to the
    // tool surface but must never be serialized as a user/developer message.
    // Normalizing the input first also converts private shell/custom history
    // items into their public function-call equivalents before message
    // conversion.
    let mut normalized_body = body.clone();
    let object = normalized_body
        .as_object_mut()
        .ok_or_else(|| "Responses-to-Chat body must be a JSON object".to_string())?;
    let mut embedded_tools = Vec::new();
    let mut input_pairs: Option<Vec<(Value, ChatMessageProvenance)>> = None;
    if let Some(Value::Array(items)) = object.get_mut("input") {
        let original = std::mem::take(items);
        let mut pairs = Vec::with_capacity(original.len());
        for (index, item) in original.into_iter().enumerate() {
            let provenance = replay.provenance_for_item(index);
            if let Some(item) = normalize_grok_input_item(item, &mut embedded_tools, profile)? {
                pairs.push((item, provenance));
            }
        }
        *items = pairs.iter().map(|(item, _)| item.clone()).collect();
        input_pairs = Some(pairs);
    }
    let raw_tools = object
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let custom_names = raw_tools
        .iter()
        .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("custom"))
        .filter_map(function_tool_name)
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let mut tools = raw_tools
        .iter()
        .filter(|tool| tool.get("type").and_then(Value::as_str) != Some("custom"))
        .flat_map(|tool| chat_tools(tool, profile))
        .filter(|tool| function_tool_name(tool).is_none_or(|name| !custom_names.contains(name)))
        .collect::<Vec<_>>();
    tools.extend(
        raw_tools
            .iter()
            .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("custom"))
            .flat_map(|tool| chat_tools(tool, profile)),
    );
    let mut tools = deduplicate_function_tools(tools, ToolShape::Chat)?;
    let canonical_names = tools
        .iter()
        .filter_map(function_tool_name)
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let embedded = embedded_tools
        .iter()
        .flat_map(|tool| chat_tools(tool, profile))
        .filter(|tool| function_tool_name(tool).is_none_or(|name| !canonical_names.contains(name)))
        .collect::<Vec<_>>();
    tools.extend(deduplicate_function_tools(embedded, ToolShape::Chat)?);
    if profile.multi_agent_policy == MultiAgentPolicy::VellumDelegated {
        if let Some(namespace) = crate::harness::multi_agent::delegation_namespace(true) {
            tools.extend(chat_tools(&namespace, profile));
        }
    }
    let tools = deduplicate_function_tools(tools, ToolShape::Chat)?;
    let mut projection = ChatProjection::default();
    let synthetic = ChatMessageProvenance {
        occurrence_id: None,
        from_prefix: false,
    };
    if let Some(instructions) = normalized_body.get("instructions").and_then(Value::as_str) {
        projection.push(
            json!({"role": "system", "content": instructions}),
            synthetic.clone(),
        );
    }
    if !tools.is_empty() {
        projection.push(
            json!({
                "role": "system",
                "content": concat!(
                    "Tool-use continuity for this Codex task:\n",
                    "- Use the native Chat Completions tool_calls field whenever a tool is needed. ",
                    "Never serialize <tool_call>, <think>, tool arguments, function results, ",
                    "tool_call_id markers, or internal reasoning into assistant content.\n",
                    "- If you say you will inspect, clone, create, edit, run, test, or otherwise act ",
                    "on the workspace, issue the corresponding native tool call in the same response.\n",
                    "- Do not end the turn with only a promise, an observation marker, or an ",
                    "announcement of future work.\n",
                    "- After receiving a tool result, either issue the next native tool call or ",
                    "provide a user-visible final answer when the task is complete.\n",
                    "- Continue using tools until the requested task is complete or you are genuinely blocked."
                )
            }),
            synthetic.clone(),
        );
    }
    let replay_reasoning = should_replay_reasoning(capabilities, replay);
    let pairs = match input_pairs {
        Some(pairs) => pairs,
        None => crate::request::request_input_items(&normalized_body)
            .into_iter()
            .enumerate()
            .map(|(index, item)| (item, replay.provenance_for_item(index)))
            .collect(),
    };
    project_chat_items(&mut projection, &pairs, replay_reasoning, vision)?;
    let mut projection = coalesce_adjacent_chat_assistant_messages(projection);
    projection = reconcile_partial_chat_tool_continuations(projection)?;
    projection = collapse_system_messages_to_head(projection);
    apply_neutral_user_bridge_to_projection(&mut projection, capabilities);
    apply_checkpoint_resume_tail_to_projection(&mut projection);
    let messages = projection.messages.clone();
    let mut converted = json!({
        "model": normalized_body.get("model").cloned().unwrap_or(Value::Null),
        "messages": messages,
        "stream": normalized_body.get("stream").and_then(Value::as_bool).unwrap_or(true),
    });
    if !tools.is_empty() {
        converted["tools"] = Value::Array(tools);
        if has_auto_or_missing_tool_choice(&normalized_body)
            && should_require_initial_workspace_tool(&normalized_body)
        {
            converted["tool_choice"] = json!("required");
        } else if let Some(tool_choice) = normalized_body.get("tool_choice") {
            converted["tool_choice"] = normalize_chat_tool_choice(tool_choice);
        }
    }
    // Outside the `tools` block on purpose. A Chat Completions server sends the
    // final usage chunk of a stream only when this asks for it, and whether the
    // turn declared tools has nothing to do with whether its tokens should be
    // counted. Nested here, a tool-less streaming turn — a review leg, a
    // summarization, a provider probe — was billed to Codex as zero, and Codex
    // trusts an explicit zero.
    if converted["stream"] == true {
        converted["stream_options"] = json!({"include_usage": true});
    }
    for key in [
        "temperature",
        "top_p",
        "max_tokens",
        "max_completion_tokens",
        "parallel_tool_calls",
        "prompt_cache_key",
    ] {
        if let Some(value) = normalized_body.get(key) {
            converted[key] = value.clone();
        }
    }
    if converted.get("max_tokens").is_none() {
        if let Some(value) = normalized_body.get("max_output_tokens") {
            converted["max_tokens"] = value.clone();
        }
    }
    Ok((converted, projection))
}

/// A Responses turn can contain several assistant-owned items in sequence:
/// readable reasoning, commentary text, and one or more function calls. Chat
/// Completions represents that same turn as one assistant message. Sending one
/// Chat message per Responses item is not merely redundant: Ollama rejects a
/// request whose tail contains two assistant messages with
/// `Cannot have 2 or more assistant messages at the end of the list`.
///
/// Coalesce only adjacent assistant messages. Tool/user boundaries remain
/// intact, so function-call outputs still pair with the assistant turn that
/// preceded them.
fn coalesce_adjacent_chat_assistant_messages(projection: ChatProjection) -> ChatProjection {
    let mut result = ChatProjection::default();
    for (message, provenance) in projection.into_pairs() {
        let is_assistant = message.get("role").and_then(Value::as_str) == Some("assistant");
        let previous_is_assistant = result.messages.last().is_some_and(|previous| {
            previous.get("role").and_then(Value::as_str) == Some("assistant")
        });
        if is_assistant && previous_is_assistant {
            merge_chat_assistant_message(
                result.messages.last_mut().expect("checked above"),
                message,
            );
        } else {
            result.push(message, provenance);
        }
    }
    result
}

fn merge_chat_assistant_message(target: &mut Value, incoming: Value) {
    let Some(target) = target.as_object_mut() else {
        return;
    };
    let Some(mut incoming) = incoming.as_object().cloned() else {
        return;
    };

    for field in ["content", "reasoning_content", "reasoning"] {
        let incoming_text = incoming
            .remove(field)
            .map(|value| crate::adapter::content_to_plain_string(&value))
            .unwrap_or_default();
        if incoming_text.trim().is_empty() {
            continue;
        }
        let existing = target
            .get(field)
            .map(crate::adapter::content_to_plain_string)
            .unwrap_or_default();
        let combined = if existing.trim().is_empty() {
            incoming_text
        } else {
            format!("{}\n\n{}", existing.trim_end(), incoming_text.trim_start())
        };
        target.insert(field.to_string(), Value::String(combined));
    }

    if let Some(Value::Array(mut calls)) = incoming.remove("tool_calls") {
        let target_calls = target
            .entry("tool_calls".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(target_calls) = target_calls {
            for call in calls.drain(..) {
                let call_id = call.get("id").and_then(Value::as_str);
                if call_id.is_some_and(|id| {
                    target_calls
                        .iter()
                        .any(|existing| existing.get("id").and_then(Value::as_str) == Some(id))
                }) {
                    continue;
                }
                target_calls.push(call);
            }
        }
    }

    // Preserve provider-neutral assistant fields that are not represented by
    // the explicit merge rules above. Existing values remain authoritative.
    for (key, value) in incoming {
        if key != "role" {
            target.entry(key).or_insert(value);
        }
    }
}

/// Codex can report parallel tool results incrementally. A Responses snapshot
/// may therefore contain one assistant item with two tool calls followed by a
/// result for only the tool that has finished so far. Chat Completions rejects
/// that snapshot: every advertised `tool_call` must have an immediately
/// following tool message before the model may continue.
///
/// Keep the completed call/result pairs and defer unresolved calls until they
/// appear with their result in a later full snapshot. Duplicate ids and
/// orphan results fail closed.
fn reconcile_partial_chat_tool_continuations(
    projection: ChatProjection,
) -> Result<ChatProjection, String> {
    let pairs = projection.into_pairs();
    let mut reconciled = ChatProjection::default();
    let mut index = 0;
    while index < pairs.len() {
        let (mut message, provenance) = pairs[index].clone();
        let is_assistant = message.get("role").and_then(Value::as_str) == Some("assistant");
        let has_calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty());
        if !is_assistant || !has_calls {
            reconciled.push(message, provenance);
            index += 1;
            continue;
        }

        let mut boundary = index + 1;
        let mut completed = HashSet::new();
        while boundary < pairs.len()
            && pairs[boundary].0.get("role").and_then(Value::as_str) == Some("tool")
        {
            if let Some(call_id) = pairs[boundary]
                .0
                .get("tool_call_id")
                .and_then(Value::as_str)
            {
                completed.insert(call_id.to_string());
            }
            boundary += 1;
        }

        let mut retained = HashSet::new();
        if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
            let mut seen_ids = HashSet::new();
            for call in calls.iter() {
                let Some(call_id) = call.get("id").and_then(Value::as_str) else {
                    return Err("chat tool pairing: tool call missing id".into());
                };
                if !seen_ids.insert(call_id.to_string()) {
                    return Err(format!(
                        "chat tool pairing: duplicate tool call id `{call_id}`"
                    ));
                }
            }
            calls.retain(|call| {
                let call_id = call.get("id").and_then(Value::as_str);
                let keep = call_id.is_some_and(|call_id| completed.contains(call_id));
                if keep {
                    retained.insert(call_id.expect("checked above").to_string());
                }
                keep
            });
            if calls.is_empty() {
                message
                    .as_object_mut()
                    .expect("assistant message is an object")
                    .remove("tool_calls");
            }
        }
        if retained.is_empty() {
            let object = message
                .as_object_mut()
                .expect("assistant message is an object");
            object.remove("reasoning_content");
            object.remove("reasoning");
        }
        if !retained.is_empty() || assistant_has_non_reasoning_content(&message) {
            reconciled.push(message, provenance);
        }
        for (tool, tool_provenance) in &pairs[index + 1..boundary] {
            if tool
                .get("tool_call_id")
                .and_then(Value::as_str)
                .is_some_and(|call_id| retained.contains(call_id))
            {
                reconciled.push(tool.clone(), tool_provenance.clone());
            }
        }
        index = boundary;
    }
    Ok(reconciled)
}

fn assistant_has_non_reasoning_content(message: &Value) -> bool {
    message
        .get("content")
        .map(crate::adapter::content_to_plain_string)
        .is_some_and(|content| !content.trim().is_empty())
}

fn collapse_system_messages_to_head(projection: ChatProjection) -> ChatProjection {
    let mut system = Vec::new();
    let mut system_provenance = None;
    let mut rest = ChatProjection::default();
    for (message, provenance) in projection.into_pairs() {
        if message.get("role").and_then(Value::as_str) == Some("system") {
            let text = message
                .get("content")
                .map(crate::adapter::content_to_text)
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default();
            if !text.trim().is_empty() {
                if system_provenance.is_none() {
                    system_provenance = Some(provenance);
                }
                system.push(text);
            }
        } else {
            rest.push(message, provenance);
        }
    }
    if system.is_empty() {
        return rest;
    }
    let mut collapsed = ChatProjection::default();
    collapsed.push(
        json!({"role": "system", "content": system.join("\n\n")}),
        system_provenance.unwrap_or(ChatMessageProvenance {
            occurrence_id: None,
            from_prefix: false,
        }),
    );
    for (message, provenance) in rest.into_pairs() {
        collapsed.push(message, provenance);
    }
    collapsed
}

fn has_auto_or_missing_tool_choice(body: &Value) -> bool {
    match body.get("tool_choice") {
        None | Some(Value::Null) => true,
        Some(Value::String(choice)) => choice.eq_ignore_ascii_case("auto"),
        Some(choice) => choice
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|choice| choice.eq_ignore_ascii_case("auto")),
    }
}

fn normalize_chat_tool_choice(choice: &Value) -> Value {
    match choice {
        Value::Object(object) => {
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| choice.pointer("/function/name").and_then(Value::as_str));
            match name {
                Some(name) => json!({"type": "function", "function": {"name": name}}),
                None => json!("auto"),
            }
        }
        _ => choice.clone(),
    }
}

fn should_require_initial_workspace_tool(body: &Value) -> bool {
    let Some(input) = body.get("input") else {
        return false;
    };
    let prompt = match input {
        Value::String(prompt) => prompt.clone(),
        Value::Array(items) => {
            if items.iter().rev().any(|item| {
                matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("function_call_output" | "custom_tool_call_output" | "tool_search_output")
                )
            }) {
                return false;
            }
            items
                .iter()
                .rev()
                .find(|item| item.get("role").and_then(Value::as_str) == Some("user"))
                .and_then(|item| item.get("content"))
                .map(crate::adapter::content_to_text)
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default()
        }
        _ => String::new(),
    };
    let lower = prompt.to_lowercase();
    let action = [
        "implement",
        "create",
        "edit",
        "modify",
        "fix",
        "run",
        "test",
        "inspect",
        "read",
        "investigate",
        "build",
        "commit",
        "apply",
        "delete",
        "remove",
        "update",
        "clone",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || [
            "實作", "实现", "建立", "修改", "修復", "修复", "執行", "执行", "測試", "测试", "檢查",
            "检查", "調查", "调查", "讀取", "读取", "建置", "編譯", "编译", "提交", "刪除", "删除",
            "克隆",
        ]
        .iter()
        .any(|needle| prompt.contains(needle));
    let workspace = [
        "file",
        "repo",
        "repository",
        "project",
        "code",
        "test",
        "build",
        "workspace",
        "directory",
        "path",
        "github",
        ".py",
        ".rs",
        ".ts",
        ".tsx",
        ".js",
        ".json",
        ".toml",
        ".md",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        || [
            "檔案",
            "文件",
            "專案",
            "项目",
            "程式碼",
            "代码",
            "測試",
            "测试",
            "目錄",
            "目录",
            "路徑",
            "工作區",
            "工作区",
            "倉庫",
            "仓库",
        ]
        .iter()
        .any(|needle| prompt.contains(needle));
    action && workspace
}

struct AssistantTurnAcc {
    content: String,
    reasoning: String,
    calls: Vec<Value>,
    provenance: ChatMessageProvenance,
}

fn project_chat_items(
    projection: &mut ChatProjection,
    items: &[(Value, ChatMessageProvenance)],
    replay_reasoning: bool,
    vision: bool,
) -> Result<(), String> {
    let mut turn: Option<AssistantTurnAcc> = None;
    let mut pending_tool_images = Vec::new();
    for (item, provenance) in items {
        let provenance = provenance.clone();
        let item_type = item.get("type").and_then(Value::as_str);
        let is_tool_output = matches!(
            item_type,
            Some("function_call_output" | "custom_tool_call_output" | "tool_search_output")
        );
        if !is_tool_output {
            flush_chat_tool_images(projection, &mut pending_tool_images);
        }
        if (is_canonical_checkpoint_item(item)
            && item_type != Some("compaction")
            && item_type != Some("context_compaction"))
            || is_summary_item(item)
        {
            flush_assistant_turn(projection, turn.take(), replay_reasoning);
            let text = item
                .get("content")
                .map(crate::adapter::content_to_plain_string)
                .unwrap_or_default();
            projection.push(
                json!({
                    "role": "assistant",
                    "content": format!("{CHAT_CHECKPOINT_PREFACE}\n\n{text}")
                }),
                provenance,
            );
            continue;
        }
        match item_type {
            Some("compaction" | "context_compaction" | "compaction_trigger") => {}
            Some("function_call" | "custom_tool_call") => {
                let call = chat_tool_call_value(item, item_type);
                let acc = turn.get_or_insert_with(|| AssistantTurnAcc {
                    content: String::new(),
                    reasoning: String::new(),
                    calls: Vec::new(),
                    provenance: provenance.clone(),
                });
                acc.calls.push(call);
            }
            Some("function_call_output" | "custom_tool_call_output" | "tool_search_output") => {
                flush_assistant_turn(projection, turn.take(), replay_reasoning);
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("call");
                let (content, images) =
                    chat_tool_output(item.get("output").unwrap_or(&Value::Null), call_id, vision)?;
                projection.push(
                    json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": content
                    }),
                    provenance,
                );
                pending_tool_images.extend(images);
            }
            Some("reasoning") => {
                if let Some(summary) = reasoning_summary_text(item) {
                    if replay_reasoning {
                        let acc = turn.get_or_insert_with(|| AssistantTurnAcc {
                            content: String::new(),
                            reasoning: String::new(),
                            calls: Vec::new(),
                            provenance: provenance.clone(),
                        });
                        acc.reasoning = if acc.reasoning.trim().is_empty() {
                            summary
                        } else {
                            format!("{}\n\n{summary}", acc.reasoning.trim_end())
                        };
                    }
                }
            }
            _ => {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                if role == "assistant" {
                    let to_content = crate::adapter::content_to_text;
                    let content = item
                        .get("content")
                        .map(to_content)
                        .unwrap_or_else(|| to_content(item));
                    let text = crate::adapter::content_to_plain_string(&content);
                    let acc = turn.get_or_insert_with(|| AssistantTurnAcc {
                        content: String::new(),
                        reasoning: String::new(),
                        calls: Vec::new(),
                        provenance: provenance.clone(),
                    });
                    if !text.trim().is_empty() {
                        acc.content = if acc.content.trim().is_empty() {
                            text
                        } else {
                            format!("{}\n\n{}", acc.content.trim_end(), text.trim_start())
                        };
                    }
                    continue;
                }
                if matches!(role, "system" | "developer" | "user" | "tool") {
                    flush_assistant_turn(projection, turn.take(), replay_reasoning);
                    let chat_role = if role == "developer" { "system" } else { role };
                    let to_content = if role == "user" {
                        content_to_chat_message_content
                    } else {
                        crate::adapter::content_to_text
                    };
                    let content = item
                        .get("content")
                        .map(to_content)
                        .unwrap_or_else(|| to_content(item));
                    projection.push(json!({"role": chat_role, "content": content}), provenance);
                }
            }
        }
    }
    flush_assistant_turn(projection, turn.take(), replay_reasoning);
    flush_chat_tool_images(projection, &mut pending_tool_images);
    Ok(())
}

/// Chat tool messages are text-only on the strict compatible endpoints Vellum
/// supports. Keep the tool result itself textual, then carry image parts in a
/// multimodal user message after every tool call in the assistant turn has
/// been closed. Deferring the image message is essential for parallel calls:
/// inserting it between two tool results would invalidate their pairing.
fn chat_tool_output(
    output: &Value,
    call_id: &str,
    vision: bool,
) -> Result<(String, Vec<Value>), String> {
    let Value::Array(parts) = output else {
        return Ok((crate::adapter::content_to_plain_string(output), Vec::new()));
    };

    let mut text = Vec::new();
    let mut images = Vec::new();
    for part in parts {
        if is_image_part(part) {
            let Some(url) = responses_image_url(part) else {
                continue;
            };
            let mut image = json!({ "url": url });
            if let Some(detail) = part.get("detail").and_then(Value::as_str) {
                image["detail"] = json!(detail);
            }
            images.push(json!({"type": "image_url", "image_url": image}));
            continue;
        }
        let part_text = crate::adapter::content_to_plain_string(part);
        if !part_text.trim().is_empty() {
            text.push(part_text);
        }
    }

    if !images.is_empty() {
        if !vision {
            return Err(format!(
                "tool `{call_id}` returned image content, but the selected Chat route does not declare vision support"
            ));
        }
        text.push(format!(
            "[Tool `{call_id}` returned {} image(s); image content follows.]",
            images.len()
        ));
    }
    if text.is_empty() {
        text.push("[Tool completed without textual output.]".to_string());
    }
    Ok((text.join("\n"), images))
}

fn flush_chat_tool_images(projection: &mut ChatProjection, images: &mut Vec<Value>) {
    if images.is_empty() {
        return;
    }
    let mut content = Vec::with_capacity(images.len() + 1);
    content.push(json!({
        "type": "text",
        "text": "Image content returned by the preceding tool call(s)."
    }));
    content.append(images);
    projection.push(
        json!({"role": "user", "content": content}),
        ChatMessageProvenance {
            occurrence_id: None,
            from_prefix: false,
        },
    );
}

fn chat_tool_call_value(item: &Value, item_type: Option<&str>) -> Value {
    let call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .cloned()
        .unwrap_or_else(|| json!("call"));
    let arguments = if item_type == Some("custom_tool_call") {
        custom_tool_arguments(
            item.get("name").and_then(Value::as_str).unwrap_or_default(),
            item.get("input").cloned().unwrap_or_else(|| json!("")),
        )
    } else {
        item.get("arguments")
            .and_then(Value::as_str)
            .unwrap_or("{}")
            .to_string()
    };
    json!({
        "id": call_id,
        "type": "function",
        "function": {
            "name": item.get("name").cloned().unwrap_or_else(|| json!("unknown")),
            "arguments": arguments
        }
    })
}

fn reasoning_summary_text(item: &Value) -> Option<String> {
    item.get("summary")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .filter(|text| !text.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|text| !text.trim().is_empty())
        .or_else(|| {
            item.get("content")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .map(str::to_string)
        })
}

fn flush_assistant_turn(
    projection: &mut ChatProjection,
    turn: Option<AssistantTurnAcc>,
    replay_reasoning: bool,
) {
    let Some(turn) = turn else {
        return;
    };
    if turn.calls.is_empty() && turn.content.trim().is_empty() && turn.reasoning.trim().is_empty() {
        return;
    }
    if turn.calls.is_empty() && turn.content.trim().is_empty() {
        // Reasoning without a tool-call owner is not an independent Chat turn.
        return;
    }
    let mut message = json!({
        "role": "assistant",
        "content": if turn.content.trim().is_empty() {
            Value::String(EMPTY_ASSISTANT_CONTENT.to_string())
        } else {
            Value::String(turn.content)
        }
    });
    if !turn.calls.is_empty() {
        message["tool_calls"] = Value::Array(turn.calls);
    }
    if replay_reasoning && !turn.reasoning.trim().is_empty() {
        message["reasoning_content"] = json!(turn.reasoning);
    }
    projection.push(message, turn.provenance);
}

/// What an assistant message carries when it has no user-visible text — a tool
/// call, or retained reasoning.
///
/// OpenAI documents `null` here and vLLM and DeepSeek accept it, but Ollama
/// rejects the whole request with `invalid message content type: <nil>` unless
/// the message also carries `tool_calls`. Measured against a live endpoint:
/// assistant+null with tool_calls is accepted, while assistant+null without
/// them, a `tool` message with null, and a `system` message with null are all
/// HTTP 400. An empty string is accepted everywhere tested, so one rule covers
/// every provider instead of branching on which one is upstream.
const EMPTY_ASSISTANT_CONTENT: &str = "";

fn is_image_part(part: &Value) -> bool {
    matches!(
        part.get("type").and_then(Value::as_str),
        Some("input_image" | "image")
    )
}

/// Pull a usable URL out of a Responses image part.
///
/// Codex sends `image_url` as a bare string; some SDKs send `{"url": ...}`.
/// A part carrying only `file_id` refers to something uploaded on the
/// provider's side, which a third-party endpoint cannot fetch, so it has no
/// Chat equivalent and is left out.
fn responses_image_url(part: &Value) -> Option<String> {
    let url = match part.get("image_url")? {
        Value::String(url) => url.clone(),
        Value::Object(object) => object.get("url").and_then(Value::as_str)?.to_string(),
        _ => return None,
    };
    (!url.trim().is_empty()).then_some(url)
}

/// Build the `content` of a Chat message, keeping any images.
///
/// Flattening to text used to drop image parts silently: they carry no `text`
/// field, so the filter that collects text simply skipped them. The model then
/// answered a question about a picture it never received — and rather than
/// failing, it invented an answer. Only a message that really carries an image
/// becomes the multimodal array form; everything else stays the plain string
/// that providers and the rest of this module expect.
fn content_to_chat_message_content(value: &Value) -> Value {
    let Value::Array(parts) = value else {
        return crate::adapter::content_to_text(value);
    };
    if !parts.iter().any(is_image_part) {
        return crate::adapter::content_to_text(value);
    }

    let mut converted = Vec::with_capacity(parts.len());
    let mut kept_image = false;
    for part in parts {
        if is_image_part(part) {
            let Some(url) = responses_image_url(part) else {
                continue;
            };
            let mut image = json!({ "url": url });
            if let Some(detail) = part.get("detail").and_then(Value::as_str) {
                image["detail"] = json!(detail);
            }
            converted.push(json!({"type": "image_url", "image_url": image}));
            kept_image = true;
            continue;
        }
        if let Some(text) = part
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| part.get("content").and_then(Value::as_str))
            .filter(|text| !text.is_empty())
        {
            converted.push(json!({"type": "text", "text": text}));
        }
    }

    // Nothing survived that needed the richer shape, so do not impose it.
    if !kept_image {
        return crate::adapter::content_to_text(value);
    }
    Value::Array(converted)
}

/// Translate one Codex tool declaration into its Chat Completions function
/// form, governed by the resolved profile's patch contract. The mirror of
/// [`grok_function_tools`] for the Chat wire.
pub fn chat_tools(tool: &Value, profile: &HarnessProfile) -> Vec<Value> {
    match tool.get("type").and_then(Value::as_str) {
        Some("function") => normalize_function_tool(tool).into_iter().collect(),
        Some("custom") => {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("custom_tool");
            if harness_tools::is_apply_patch(name) {
                return match profile.patch_contract {
                    PatchContract::Unsupported => Vec::new(),
                    _ => vec![json!({
                        "type": "function",
                        "function": {
                            "name": harness_tools::APPLY_PATCH_TOOL_NAME,
                            "description": harness_tools::apply_patch_description(),
                            "parameters": harness_tools::apply_patch_parameters()
                        }
                    })],
                };
            }
            vec![json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": tool.get("description").cloned().unwrap_or_else(|| json!(
                        "Codex free-form tool. Preserve the raw input exactly."
                    )),
                    "parameters": {
                        "type": "object",
                        "properties": {
                            CUSTOM_TOOL_INPUT_FIELD: {
                                "type": "string",
                                "description": "Raw input for the original Codex custom tool"
                            }
                        },
                        "required": [CUSTOM_TOOL_INPUT_FIELD],
                        "additionalProperties": false
                    }
                }
            })]
        }
        Some("namespace") => {
            let namespace = tool
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty());
            let namespace_description = tool.get("description").and_then(Value::as_str);
            tool.get("tools")
                .or_else(|| tool.get("functions"))
                .and_then(Value::as_array)
                .map(|children| {
                    children
                        .iter()
                        // A child whose schema has no object root is dropped
                        // here too, not carried into the namespace flattening.
                        .filter_map(normalize_function_tool)
                        .map(|mut child| {
                            // Chat Completions has no namespace tool type. Keep
                            // Codex's namespace in the flattened function name,
                            // matching the Responses/Grok adapter. Without this,
                            // deferred helpers such as `_fetch` from two
                            // namespaces collapse into one ambiguous function.
                            if let (Some(namespace), Some(function)) = (
                                namespace,
                                child.get_mut("function").and_then(Value::as_object_mut),
                            ) {
                                if let Some(name) = function
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                                {
                                    function.insert(
                                        "name".into(),
                                        json!(harness_tools::flatten_namespace_name(
                                            namespace, &name
                                        )),
                                    );
                                }
                                let child_description =
                                    function.get("description").and_then(Value::as_str);
                                let description = harness_tools::namespaced_description(
                                    namespace,
                                    namespace_description,
                                    child_description,
                                );
                                function.insert("description".into(), json!(description));
                            }
                            child
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        Some("tool_search") => vec![json!({
            "type": "function",
            "function": {
                "name": "tool_search",
                "description": "Search for available tools",
                "parameters": tool.get("parameters").cloned()
                    .unwrap_or_else(|| json!({"type": "object"}))
            }
        })],
        // Same exact-or-nothing rule as the Responses path.
        Some(kind) => match harness_tools::builtin_tool(kind) {
            BuiltinDecision::Exact {
                description,
                parameters,
                ..
            } => vec![json!({
                "type": "function",
                "function": {
                    "name": kind,
                    "description": description,
                    "parameters": parameters
                }
            })],
            BuiltinDecision::Unsupported { .. } => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// `None` when the declared parameter schema has no object root the wire can
/// carry; the caller drops that tool. See `object_rooted_tool_parameters`.
fn normalize_function_tool(tool: &Value) -> Option<Value> {
    let source = tool.get("function").unwrap_or(tool);
    let declared = source
        .get("parameters")
        .or_else(|| source.get("input_schema"))
        .cloned()
        .unwrap_or_else(|| json!({"type": "object"}));
    let parameters = object_rooted_tool_parameters(&declared)?;
    Some(json!({
        "type": "function",
        "function": {
            "name": source.get("name").cloned().unwrap_or_else(|| json!("unnamed_tool")),
            "description": source.get("description").cloned().unwrap_or_else(|| json!("")),
            "parameters": parameters
        }
    }))
}

/// Dump exactly what the model can see on this route.
///
/// Issue #6 Phase 0. Takes the **prepared upstream body**, not the incoming
/// Codex request: the point of the snapshot is to record what was actually
/// sent, so the tools are read after translation and the prompt hash is taken
/// over the real outgoing `instructions`. Hashing a locally re-derived prompt
/// would let the trace describe something the model never received.
///
/// Codex builds the native tool router for Official routes, so there the
/// snapshot only reports the catalog contract.
pub fn harness_snapshot_with_environment(
    original: &Value,
    prepared: &Value,
    wire: RuntimeWireFormat,
    profile: &HarnessProfile,
    environment: &ExecutionEnvironment,
    catalog_entry: Option<&Value>,
) -> Result<ModelVisibleHarnessSnapshot, String> {
    if let Some(entry) = catalog_entry {
        profile
            .verify_catalog_entry(entry)
            .map_err(|error| error.to_string())?;
    }
    if !profile.kind.is_translated() {
        return Ok(ModelVisibleHarnessSnapshot::from_official_catalog_entry(
            catalog_entry.unwrap_or(&Value::Null),
        ));
    }

    // Tools and prompt come from the prepared body — that is what was sent.
    // The dropped tools can only come from the original request: by the time
    // the body is prepared they are already gone, which is the point.
    let (tools, _) = model_visible_tools(prepared, wire, profile);
    let (_, unsupported) = model_visible_tools(original, wire, profile);
    let prompt = outgoing_instructions(prepared, wire);
    let provider_capabilities = json!({
        "apply_patch_tool_type": catalog_entry.and_then(|entry| entry.get("apply_patch_tool_type")),
        "web_search_tool_type": catalog_entry.and_then(|entry| entry.get("web_search_tool_type")),
        "input_modalities": catalog_entry.and_then(|entry| entry.get("input_modalities")),
        "supports_parallel_tool_calls": profile.advertises_parallel_tool_calls(),
        "tool_mode": Value::Null,
        "use_responses_lite": profile.advertises_responses_lite(),
        "shell_type": catalog_entry.and_then(|entry| entry.get("shell_type")),
        "context_window": catalog_entry.and_then(|entry| entry.get("context_window")),
        "truncation_policy": catalog_entry.and_then(|entry| entry.get("truncation_policy")),
        "comp_hash": Value::Null,
        "multi_agent_version": Value::Null,
        "supported_reasoning_levels": catalog_entry.and_then(|entry| entry.get("supported_reasoning_levels")),
        "default_reasoning_level": catalog_entry.and_then(|entry| entry.get("default_reasoning_level")),
        "priority": catalog_entry.and_then(|entry| entry.get("priority"))
    });

    let capabilities = crate::harness::shell::TerminalCapabilities::from(environment);
    Ok(ModelVisibleHarnessSnapshot::new(
        *profile,
        catalog_entry.cloned().unwrap_or(Value::Null),
        crate::harness::prompt::prompt_source(profile),
        &prompt,
        tools,
        unsupported,
        Some(capabilities),
        provider_capabilities,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::ExecutionEnvironment;
    use crate::harness::shell::{ShellProbe, TerminalCapabilities};
    use crate::harness::{resolve_with_options, HarnessOptions};

    fn executor_environment() -> ExecutionEnvironment {
        ExecutionEnvironment::from(&TerminalCapabilities::from_probe(&ShellProbe {
            platform: Some(crate::harness::shell::Platform::Linux),
            shell_env: Some("/bin/bash".into()),
            ..ShellProbe::default()
        }))
    }

    fn grok_route() -> ProfileAdapterRoute {
        ProfileAdapterRoute {
            upstream_model: "grok-4.5".into(),
            reasoning: true,
            vision: true,
            provider_kind: RuntimeProviderKind::GrokCli,
            wire: RuntimeWireFormat::Responses,
            base_url: "https://example.test/v1".into(),
            chat_capabilities: RuntimeChatCapabilities::default(),
            tool_capabilities: crate::route::RuntimeToolCapabilities::default(),
            reasoning_effort_transport: RuntimeReasoningEffortTransport::None,
        }
    }

    fn reasoning_chat_route() -> ProfileAdapterRoute {
        ProfileAdapterRoute {
            upstream_model: "deepseek-v4-flash".into(),
            reasoning: true,
            vision: true,
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            wire: RuntimeWireFormat::Chat,
            base_url: "https://opencode.example.test/v1".into(),
            chat_capabilities: RuntimeChatCapabilities::tool_call_bound_reasoning(),
            tool_capabilities: crate::route::RuntimeToolCapabilities::default(),
            reasoning_effort_transport: RuntimeReasoningEffortTransport::None,
        }
    }

    fn ultra_request() -> Value {
        json!({
            "model": "grok-4.5",
            "reasoning": {"effort": "ultra"},
            "input": [{"type": "message", "role": "user", "content": "hi"}]
        })
    }

    /// Issue #6 review, M3B P0-2: delegation availability must be derived from
    /// the resolved profile alone. A separate caller-supplied `bool` let a
    /// caller construct a contradictory surface — `SingleAgent` policy with a
    /// "proven" bool would hide the delegation tools while the reasoning gate
    /// still accepted `ultra`, and the reverse advertised a tool the gate
    /// refused. The request path now reads the same `multi_agent_policy` the
    /// tool surface reads, so the two cannot disagree.
    /// The shape that took Grok Build down: an MCP tool whose root schema is a
    /// union of an object and a non-object. Forwarded as-is it is not one bad
    /// tool, it is `400` on every turn the route serves.
    #[test]
    fn a_union_rooted_tool_schema_is_collapsed_to_its_object_branches() {
        let tool = json!({
            "type": "function",
            "name": "mcp__codex_app__automation_update",
            "description": "manage automations",
            "parameters": {
                "description": "update or delete",
                "anyOf": [
                    {
                        "type": "object",
                        "properties": {"id": {"type": "string"}, "prompt": {"type": "string"}},
                        "required": ["id", "prompt"]
                    },
                    {
                        "type": "object",
                        "properties": {"id": {"type": "string"}, "delete": {"type": "boolean"}},
                        "required": ["id"]
                    },
                    {"type": "string"}
                ]
            }
        });
        let profile = resolve_with_options(
            RuntimeProviderKind::GrokCli,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );
        let tools = grok_function_tools(&tool, &profile);
        assert_eq!(tools.len(), 1);
        let parameters = &tools[0]["parameters"];
        assert_eq!(parameters["type"], "object");
        // Every object branch's fields survive, so the model can still call
        // either form of the tool.
        for field in ["id", "prompt", "delete"] {
            assert!(
                parameters["properties"].get(field).is_some(),
                "{field} missing from {parameters}"
            );
        }
        // `prompt` is required by only one branch, so the merged shape must not
        // demand it -- that would reject a delete the server would accept.
        assert_eq!(parameters["required"], json!(["id"]));
        assert_eq!(parameters["description"], "update or delete");
    }

    /// Codex's generated MCP schema has both an outer object marker and a
    /// union. The old guard returned that schema unchanged as soon as it saw
    /// `type: object`, so the direct helper test passed while the real Grok
    /// request still failed with `invalid_client_tool_schema`.
    #[test]
    fn grok_request_collapses_a_typed_object_union_before_upstream() {
        let mut request = json!({
            "model": "grok-4.6",
            "input": [{"type": "message", "role": "user", "content": "Hi"}],
            "tools": [{
                "type": "function",
                "name": "mcp__codex_app__automation_update",
                "description": "manage automations",
                "parameters": {
                    "type": "object",
                    "properties": {"mode": {"type": "string"}},
                    "required": ["mode"],
                    "anyOf": [
                        {"type": "object", "properties": {"id": {"type": "string"}}},
                        {"type": "object", "properties": {"name": {"type": "string"}}},
                        {"type": "null"}
                    ]
                }
            }]
        });
        let profile = resolve_with_options(
            RuntimeProviderKind::GrokCli,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );

        normalize_grok_responses_request(&mut request, &profile, false).unwrap();

        let parameters = &request["tools"][0]["parameters"];
        assert_eq!(parameters["type"], "object");
        assert!(parameters.get("anyOf").is_none());
        assert!(parameters.get("oneOf").is_none());
        for field in ["mode", "id", "name"] {
            assert!(parameters["properties"].get(field).is_some());
        }
        assert_eq!(parameters["required"], json!(["mode"]));
    }

    #[test]
    fn a_union_rooted_tool_with_no_object_branch_is_dropped_not_forwarded() {
        let tool = json!({
            "type": "function",
            "name": "stringly",
            "parameters": {"oneOf": [{"type": "string"}, {"type": "number"}]}
        });
        let profile = resolve_with_options(
            RuntimeProviderKind::GrokCli,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );
        assert!(grok_function_tools(&tool, &profile).is_empty());
        assert!(chat_tools(&tool, &profile).is_empty());
    }

    /// The common case must not be disturbed: an ordinary object schema is
    /// forwarded byte-for-byte, including keywords this collapse does not model.
    #[test]
    fn an_ordinary_object_schema_is_forwarded_untouched() {
        let parameters = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        });
        let tool = json!({"type": "function", "name": "read", "parameters": parameters});
        let profile = resolve_with_options(
            RuntimeProviderKind::GrokCli,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );
        assert_eq!(
            grok_function_tools(&tool, &profile)[0]["parameters"],
            parameters
        );
        assert_eq!(
            chat_tools(&tool, &profile)[0]["function"]["parameters"],
            parameters
        );
    }

    #[test]
    fn chat_translation_exposes_client_tool_search_as_a_function() {
        let parameters = json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"],
            "additionalProperties": false
        });
        let tool = json!({
            "type": "tool_search",
            "execution": "client",
            "parameters": parameters
        });
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Chat,
            HarnessOptions::default(),
            false,
        );

        let translated = chat_tools(&tool, &profile);
        assert_eq!(translated.len(), 1);
        assert_eq!(translated[0]["function"]["name"], "tool_search");
        assert_eq!(translated[0]["function"]["parameters"], parameters);
    }

    /// A schema that lists `properties` without saying `type`, and the empty
    /// schema, are both already object-rooted; neither may be dropped.
    #[test]
    fn a_schema_without_an_explicit_type_is_still_an_object_root() {
        let profile = resolve_with_options(
            RuntimeProviderKind::GrokCli,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );
        for parameters in [json!({"properties": {"a": {"type": "string"}}}), json!({})] {
            let tool = json!({"type": "function", "name": "t", "parameters": parameters});
            assert_eq!(
                grok_function_tools(&tool, &profile).len(),
                1,
                "dropped {parameters}"
            );
        }
    }

    #[test]
    fn delegation_authority_is_derived_from_the_profile_not_a_separate_bool() {
        let route = grok_route();
        let environment = executor_environment();

        // A wired delegation runtime that has not been *verified* resolves to
        // SingleAgent: no delegation tool surface, and `ultra` refused.
        let single = resolve_with_options(
            RuntimeProviderKind::GrokCli,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            true,
        );
        assert_eq!(single.multi_agent_policy, MultiAgentPolicy::SingleAgent);
        let error = prepare_upstream_request_with_environment(
            &ultra_request(),
            &route,
            &single,
            &environment,
            None,
            false,
        )
        .unwrap_err();
        assert!(
            error.contains("requires a verified delegation runtime"),
            "{error}"
        );

        // The identical request is accepted once the profile itself resolves
        // to VellumDelegated. There is no second switch that can disagree with
        // the tool surface, which is derived from the same policy.
        let delegated = resolve_with_options(
            RuntimeProviderKind::GrokCli,
            RuntimeWireFormat::Responses,
            HarnessOptions {
                delegation_verified: true,
            },
            true,
        );
        assert_eq!(
            delegated.multi_agent_policy,
            MultiAgentPolicy::VellumDelegated
        );
        prepare_upstream_request_with_environment(
            &ultra_request(),
            &route,
            &delegated,
            &environment,
            None,
            false,
        )
        .unwrap();
    }

    /// Issue #6 review, M3B P0-1: the request-time prompt replacement must
    /// match against the *catalog's* `base_instructions`, never a live probe of
    /// this process's shell. The executor contract is the single environment;
    /// a host-derived baseline could replace the wrong text and ship two shell
    /// contracts. A catalog entry that does not carry `base_instructions` falls
    /// back to appending, which still never duplicates a matching baseline.
    #[test]
    fn prompt_replacement_uses_the_catalog_baseline_only() {
        let profile = resolve_with_options(
            RuntimeProviderKind::GrokCli,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );
        let route = grok_route();
        let environment = executor_environment();

        // The catalog was written on a Windows PowerShell host; the executor is
        // the Linux bash contract above. Shipping the catalog's actual baseline
        // (not this process's shell) lets the replacement swap it cleanly.
        let catalog_caps = TerminalCapabilities::from_probe(&ShellProbe {
            platform: Some(crate::harness::shell::Platform::Windows),
            powershell: Some("C:/Windows/System32/powershell.exe".into()),
            powershell_version: Some("5.1.26200.1".into()),
            ..ShellProbe::default()
        });
        let catalog_baseline = crate::harness::prompt::instructions(&profile, &catalog_caps, &[]);
        let catalog_entry = json!({
            "slug": "vlm-grok",
            "base_instructions": catalog_baseline,
            "shell_type": "shell_command",
            "apply_patch_tool_type": "freeform",
            "use_responses_lite": false,
            "supports_parallel_tool_calls": false
        });

        let request = json!({
            "model": "vlm-grok",
            "instructions": format!(
                "{catalog_baseline}\n\n# AGENTS.md\nProject rule: never touch vendor/."
            ),
            "input": "hi",
            "tools": [
                {"type": "custom", "name": "apply_patch"},
                {"type": "shell"}
            ]
        });

        let prepared = prepare_upstream_request_with_environment(
            &request,
            &route,
            &profile,
            &environment,
            Some(&catalog_entry),
            false,
        )
        .unwrap();
        let sent = prepared["instructions"].as_str().unwrap();
        // The bash executor contract replaced the Windows catalog baseline.
        assert!(sent.contains("Tools available in this request:"));
        assert!(sent.contains("use `rg` rather than `grep`"));
        assert!(!sent.contains("no `&&`"));
        assert!(!sent.contains("Select-String"));
        assert!(!sent.contains("Tools available on this route:"));
        // The user's own instructions survive the replacement.
        assert!(sent.contains("Project rule: never touch vendor/."));
    }

    #[test]
    fn chat_handoff_maps_responses_developer_messages_to_system() {
        let converted = responses_to_chat(&json!({
            "model": "deepseek-v4-flash",
            "input": [
                {
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": "preserve the contract"}]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "continue"}]
                }
            ],
            "stream": false
        }))
        .unwrap();

        let messages = converted["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert!(messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("preserve the contract"));
        assert!(messages
            .iter()
            .all(|message| message["role"] != "developer"));
    }

    #[test]
    fn compatible_responses_folds_hydrated_instructions_to_the_system_prefix() {
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );
        let mut request = json!({
            "model": "local-model",
            "instructions": "base harness",
            "client_metadata": {"codex_version": "0.147.0"},
            "tools": [{
                "type": "function",
                "name": "web_search",
                "description": "local search wrapper",
                "parameters": {"type": "object", "properties": {}}
            }],
            "input": [
                {
                    "type": "message",
                    "role": "developer",
                    "content": "original instructions"
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": "first turn"
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": "first reply"
                },
                {
                    "type": "message",
                    "role": "developer",
                    "content": "durable checkpoint"
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": "continue"
                }
            ],
            "stream": true
        });

        normalize_compatible_responses_request(&mut request, &profile, false).unwrap();

        assert!(request.get("client_metadata").is_none());
        assert!(request
            .get("tools")
            .and_then(Value::as_array)
            .is_none_or(|tools| tools
                .iter()
                .all(|tool| function_tool_name(tool) != Some("web_search"))));
        let input = request["input"].as_array().unwrap();
        assert!(input.iter().all(|item| !matches!(
            item.get("role").and_then(Value::as_str),
            Some("developer" | "system")
        )));
        assert_eq!(
            input
                .iter()
                .filter_map(|item| item.get("role").and_then(Value::as_str))
                .collect::<Vec<_>>(),
            ["user", "assistant", "user"]
        );
        assert_eq!(
            request["instructions"],
            "base harness\n\noriginal instructions\n\ndurable checkpoint"
        );
    }

    #[test]
    fn compatible_responses_coalesces_system_and_developer_items_without_reordering_history() {
        let mut object = json!({
            "instructions": "catalog instructions",
            "input": [
                {"type": "message", "role": "user", "content": "first"},
                {"type": "message", "role": "system", "content": "late system"},
                {"type": "message", "role": "assistant", "content": "reply"},
                {"type": "message", "role": "developer", "content": [
                    {"type": "input_text", "text": "checkpoint"}
                ]},
                {"type": "message", "role": "user", "content": "continue"}
            ]
        });
        fold_compatible_responses_instructions(object.as_object_mut().unwrap());

        assert_eq!(
            object["instructions"],
            "catalog instructions\n\nlate system\n\ncheckpoint"
        );
        assert_eq!(
            object["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["role"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["user", "assistant", "user"]
        );
    }

    /// Local `web_search` wrapper request, shared by the Grok and
    /// OpenAI-compatible retention tests below.
    fn web_search_wrapper_request(model: &str) -> Value {
        json!({
            "model": model,
            "tools": [{
                "type": "function",
                "name": "web_search",
                "description": "local search wrapper",
                "parameters": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}
            }],
            "input": [{"type": "message", "role": "user", "content": "search for something"}]
        })
    }

    fn has_web_search_tool(request: &Value) -> bool {
        request
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| {
                tools
                    .iter()
                    .any(|tool| function_tool_name(tool) == Some("web_search"))
            })
    }

    /// Replaces the intent of the rejected commit `529924e` ("Grok Responses
    /// always omits the local web_search wrapper"): that pinned an
    /// unconditional strip as the shipped contract, which meant Brave search
    /// could never actually be offered to a third-party route even when
    /// fully configured. The corrected contract is conditional on
    /// `web_search_enabled` (populated at snapshot-capture time from "search
    /// enabled AND a usable backend key is configured" — see
    /// `crate::snapshot::WebSearchPolicySnapshot`), not always-stripped.
    /// Native multi-agent V2 is advertised to translated routes, so a parent
    /// that delegates gets its children's replies back as `agent_message`
    /// history items. Captured from a real `codex exec` spawn against a
    /// loopback provider, where the item below arrived in the forked child's
    /// history and was being replaced by a "content omitted" marker -- which
    /// is the whole record of what the child said.
    #[test]
    fn native_v2_agent_messages_survive_translation_without_partial_fallback() {
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );

        let mut embedded = Vec::new();
        let readable = normalize_grok_input_item(
            json!({
                "type": "agent_message",
                "id": "amsg_1",
                "author": "selected_provider_child_a",
                "recipient": "root",
                "content": [{"type": "input_text", "text": "CHILD_ROUTE_OK"}]
            }),
            &mut embedded,
            &profile,
        )
        .unwrap()
        .expect("a plaintext agent message must reach the provider");
        assert_eq!(readable["type"], "message");
        assert_eq!(readable["role"], "user");
        assert_eq!(readable["content"][0]["type"], "input_text");
        let text = readable["content"][0]["text"]
            .as_str()
            .expect("translated agent message text");
        assert!(
            text.contains("CHILD_ROUTE_OK"),
            "the child's actual reply must survive: {text}"
        );
        assert!(
            text.contains("selected_provider_child_a") && text.contains("root"),
            "who said it to whom is what makes a delegated reply usable: {text}"
        );
        assert!(
            !text.contains("omitted from history"),
            "a readable agent message must not be reported as unrepresentable: {text}"
        );

        // Real native V2 traffic uses `encrypted_content` for the local tool
        // result string. Both its envelope and child report must reach the
        // parent model.
        let native_v2 = normalize_grok_input_item(
            json!({
                "type": "agent_message",
                "author": "child",
                "recipient": "root",
                "content": [
                    {"type": "input_text", "text": "partial"},
                    {"type": "encrypted_content", "encrypted_content": "CHILD_NATIVE_V2_OK"}
                ]
            }),
            &mut embedded,
            &profile,
        )
        .unwrap()
        .expect("a native V2 agent message must reach the provider");
        let native_v2_text = native_v2["content"][0]["text"]
            .as_str()
            .expect("translated native V2 text");
        assert!(
            native_v2_text.contains("partial") && native_v2_text.contains("CHILD_NATIVE_V2_OK"),
            "the complete native V2 payload must survive: {native_v2_text}"
        );
        assert!(
            !native_v2_text.contains("omitted from history"),
            "a supported native V2 payload must not become a marker: {native_v2_text}"
        );

        let unsupported = normalize_grok_input_item(
            json!({
                "type": "agent_message",
                "author": "child",
                "recipient": "root",
                "content": [
                    {"type": "input_text", "text": "partial"},
                    {"type": "future_private_part", "payload": "opaque"}
                ]
            }),
            &mut embedded,
            &profile,
        )
        .unwrap()
        .expect("an unsupported agent message still occupies a history slot");
        let unsupported_text = unsupported["content"][0]["text"]
            .as_str()
            .expect("marker text");
        assert!(
            unsupported_text.contains("omitted from history")
                && !unsupported_text.contains("partial")
                && !unsupported_text.contains("opaque"),
            "unknown content must fail closed without a partial transcript: {unsupported_text}"
        );
    }

    /// Regression from the real Codex 0.153 child rollout
    /// `01a06b6f-013c-7a80-85ed-4ff62cd57c18`: the child's first request
    /// contained only top-level instructions plus this native assignment.
    /// Exercise the production Responses-to-Chat preparation path because a
    /// one-item normalizer test cannot detect the system+assistant-only shape
    /// that rejected the request before Qwen saw it.
    #[test]
    fn native_v2_child_assignment_is_a_semantic_user_turn_on_chat_wire() {
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Chat,
            HarnessOptions::default(),
            false,
        );
        let route = ProfileAdapterRoute {
            upstream_model: "qwen".into(),
            reasoning: false,
            vision: false,
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            wire: RuntimeWireFormat::Chat,
            base_url: "https://qwen.example.test/v1".into(),
            chat_capabilities: RuntimeChatCapabilities::default(),
            tool_capabilities: crate::route::RuntimeToolCapabilities::default(),
            reasoning_effort_transport: RuntimeReasoningEffortTransport::None,
        };
        let request = json!({
            "model": "qwen",
            "instructions": "You are /root/explore_project, a sub-agent.",
            "input": [
                {
                    "type": "reasoning",
                    "id": "rs_must_remain_private",
                    "encrypted_content": "OFFICIAL_REASONING_CIPHERTEXT_MUST_NOT_LEAK",
                    "summary": []
                },
                {
                    "type": "agent_message",
                    "author": "/root",
                    "recipient": "/root/explore_project",
                    "content": [
                        {
                            "type": "input_text",
                            "text": "Message Type: NEW_TASK\nTask name: explore_project"
                        },
                        {
                            "type": "encrypted_content",
                            "encrypted_content": "你的任務：探索真實專案結構並回報結果。"
                        }
                    ]
                }
            ]
        });

        let prepared = prepare_upstream_request_with_environment(
            &request,
            &route,
            &profile,
            &executor_environment(),
            None,
            false,
        )
        .expect("the real child assignment shape must pass Chat invariants");
        let messages = prepared["messages"].as_array().unwrap();
        let assignment = messages
            .iter()
            .find(|message| message["role"] == "user")
            .expect("the child must receive a semantic user turn");
        let content = assignment["content"].as_str().unwrap();
        assert!(content.contains("Message Type: NEW_TASK"));
        assert!(content.contains("探索真實專案結構"));
        assert!(content.contains("/root -> /root/explore_project"));
        assert!(!prepared.to_string().contains("omitted from history"));
        assert!(!prepared
            .to_string()
            .contains("OFFICIAL_REASONING_CIPHERTEXT_MUST_NOT_LEAK"));
    }

    #[test]
    fn grok_responses_retains_web_search_wrapper_only_when_enabled() {
        let profile = resolve_with_options(
            RuntimeProviderKind::GrokCli,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );

        let mut disabled = web_search_wrapper_request("grok-4.6");
        normalize_grok_responses_request(&mut disabled, &profile, false).unwrap();
        assert!(
            !has_web_search_tool(&disabled),
            "search disabled must strip the wrapper: {disabled}"
        );

        let mut enabled = web_search_wrapper_request("grok-4.6");
        normalize_grok_responses_request(&mut enabled, &profile, true).unwrap();
        assert!(
            has_web_search_tool(&enabled),
            "Brave enabled with a usable key must retain the wrapper: {enabled}"
        );
    }

    /// Same corrected contract on the OpenAI-compatible Responses normalizer
    /// (the other unconditional strip `529924e` also touched).
    #[test]
    fn compatible_responses_retains_web_search_wrapper_only_when_enabled() {
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );

        let mut disabled = web_search_wrapper_request("local-model");
        normalize_compatible_responses_request(&mut disabled, &profile, false).unwrap();
        assert!(
            !has_web_search_tool(&disabled),
            "search disabled must strip the wrapper: {disabled}"
        );

        let mut enabled = web_search_wrapper_request("local-model");
        normalize_compatible_responses_request(&mut enabled, &profile, true).unwrap();
        assert!(
            has_web_search_tool(&enabled),
            "Brave enabled with a usable key must retain the wrapper: {enabled}"
        );
    }

    /// Full-pipeline version of the same contract through
    /// `prepare_upstream_request_with_environment`, the entry point the live
    /// dispatch path (`ProxyRuntime::execute`, `request_grok_summary`) and
    /// Desktop's `adapter.rs` actually call. A missing Brave key must strip
    /// the wrapper even if some caller mistakenly left `enabled: true` on
    /// its own — this flag is the single "usable now" fact, never split back
    /// into an enabled bit and a configured bit that could disagree.
    #[test]
    fn prepare_upstream_request_retains_web_search_only_when_the_flag_says_usable() {
        let environment = executor_environment();
        let grok_profile = resolve_with_options(
            RuntimeProviderKind::GrokCli,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );
        let request = web_search_wrapper_request("grok-4.6");

        let stripped = prepare_upstream_request_with_environment(
            &request,
            &grok_route(),
            &grok_profile,
            &environment,
            None,
            false,
        )
        .unwrap();
        assert!(
            !has_web_search_tool(&stripped),
            "a missing/unusable backend must strip the wrapper even through the full pipeline: {stripped}"
        );

        let retained = prepare_upstream_request_with_environment(
            &request,
            &grok_route(),
            &grok_profile,
            &environment,
            None,
            true,
        )
        .unwrap();
        assert!(
            has_web_search_tool(&retained),
            "a usable backend must retain the wrapper through the full pipeline: {retained}"
        );
    }

    #[test]
    fn qwen_like_chat_marker_request_does_not_bloat_the_upstream_body() {
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Chat,
            HarnessOptions::default(),
            false,
        );
        let route = ProfileAdapterRoute {
            upstream_model: "hf.co/unsloth/Qwen3.8-27B-GGUF:Q4_K_M".into(),
            reasoning: true,
            vision: false,
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            wire: RuntimeWireFormat::Chat,
            base_url: "https://api.provider.example/v1".into(),
            chat_capabilities: RuntimeChatCapabilities::default(),
            tool_capabilities: crate::route::RuntimeToolCapabilities::default(),
            reasoning_effort_transport: RuntimeReasoningEffortTransport::None,
        };
        let request = json!({
            "model": "vlm-qwen38",
            "store": false,
            "stream": true,
            "tool_choice": "none",
            "tools": [],
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "Reply with exactly VELLUM_QWEN_OK and nothing else." }]
            }]
        });
        let prepared = prepare_upstream_request_with_environment(
            &request,
            &route,
            &profile,
            &executor_environment(),
            None,
            false,
        )
        .unwrap();
        let encoded = serde_json::to_vec(&prepared).unwrap();
        assert!(
            encoded.len() < 2048,
            "Qwen marker request bloated to {} bytes: {prepared}",
            encoded.len()
        );
        assert_eq!(prepared["stream"], true);
        assert!(prepared.get("max_tokens").is_none());
        assert!(prepared
            .get("tools")
            .and_then(Value::as_array)
            .is_none_or(|tools| tools.is_empty()));
    }

    #[test]
    fn openai_compatible_routes_do_not_inject_an_output_token_limit() {
        let request = json!({
            "model": "local-model",
            "input": [{"type": "message", "role": "user", "content": "continue"}],
            "stream": true
        });
        let environment = executor_environment();

        let chat_profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Chat,
            HarnessOptions::default(),
            true,
        );
        let chat = prepare_upstream_request_with_environment(
            &request,
            &reasoning_chat_route(),
            &chat_profile,
            &environment,
            None,
            false,
        )
        .unwrap();
        assert!(chat.get("max_tokens").is_none());
        assert!(chat.get("max_completion_tokens").is_none());

        let responses_profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );
        let responses_route = ProfileAdapterRoute {
            upstream_model: "local-model".into(),
            reasoning: false,
            vision: false,
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            wire: RuntimeWireFormat::Responses,
            base_url: "http://127.0.0.1:11434/v1".into(),
            chat_capabilities: RuntimeChatCapabilities::default(),
            tool_capabilities: crate::route::RuntimeToolCapabilities::default(),
            reasoning_effort_transport: RuntimeReasoningEffortTransport::None,
        };
        let responses = prepare_upstream_request_with_environment(
            &request,
            &responses_route,
            &responses_profile,
            &environment,
            None,
            false,
        )
        .unwrap();
        assert!(responses.get("max_output_tokens").is_none());
        assert!(responses.get("max_tokens").is_none());
    }

    #[test]
    fn reasoning_chat_suppresses_only_inferred_required_tool_choice() {
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Chat,
            HarnessOptions::default(),
            true,
        );
        let request = json!({
            "model": "vlm-deepseek",
            "input": "Use shell_command to inspect the repository.",
            "tools": [{
                "type": "function",
                "name": "shell_command",
                "description": "Run a shell command",
                "parameters": {"type": "object"}
            }]
        });
        let prepared = prepare_upstream_request_with_environment(
            &request,
            &reasoning_chat_route(),
            &profile,
            &executor_environment(),
            None,
            false,
        )
        .unwrap();
        assert!(
            prepared.get("tool_choice").is_none(),
            "reasoning providers can reject Vellum's inferred required choice: {prepared}"
        );

        let mut explicit = request;
        explicit["tool_choice"] = json!("required");
        let prepared = prepare_upstream_request_with_environment(
            &explicit,
            &reasoning_chat_route(),
            &profile,
            &executor_environment(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(prepared["tool_choice"], "required");
    }

    #[test]
    fn reasoning_chat_replays_only_completed_parallel_tool_pairs() {
        // Production fixture from task 019feb7f: DeepSeek emitted readable
        // reasoning plus web and shell calls in one turn. Codex reported the
        // shell result while web search was still pending. Advertising both
        // calls to Chat Completions made the next request invalid, while
        // dropping the reasoning made OpenCode thinking mode reject it with
        // "reasoning_content ... must be passed back".
        let converted = responses_to_chat_with_options(
            &json!({
            "model": "deepseek-v4-flash",
            "stream": false,
            "input": [
                {"type": "message", "role": "user", "content": "make a presentation"},
                {"type": "reasoning", "summary": [{
                    "type": "summary_text",
                    "text": "Research the topic and inspect the presentation guidance."
                }]},
                {"type": "function_call", "call_id": "call_web", "name": "web_search", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_shell", "name": "shell_command", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_shell", "output": "guidance loaded"}
            ]
        }),
            &HarnessProfile::generic(RuntimeWireFormat::Chat),
            &RuntimeChatCapabilities::tool_call_bound_reasoning(),
            &ReplayContext::default(),
        )
        .unwrap();

        let messages = converted["messages"].as_array().unwrap();
        let assistant = messages
            .iter()
            .find(|message| message.get("tool_calls").is_some())
            .expect("completed tool call must be replayed");
        assert_eq!(
            assistant["reasoning_content"],
            "Research the topic and inspect the presentation guidance."
        );
        let calls = assistant["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_shell");
        assert!(messages.iter().any(|message| {
            message.get("tool_call_id").and_then(Value::as_str) == Some("call_shell")
        }));
        assert!(!serde_json::to_string(&converted)
            .unwrap()
            .contains("call_web"));
    }

    #[test]
    fn reasoning_chat_keeps_all_parallel_calls_after_all_results_arrive() {
        let converted = responses_to_chat_with_options(
            &json!({
            "model": "deepseek-v4-flash",
            "stream": false,
            "input": [
                {"type": "message", "role": "user", "content": "inspect both"},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "Use both tools."}]},
                {"type": "function_call", "call_id": "call_web", "name": "web_search", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_shell", "name": "shell_command", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_shell", "output": "shell done"},
                {"type": "function_call_output", "call_id": "call_web", "output": "web done"}
            ]
        }),
            &HarnessProfile::generic(RuntimeWireFormat::Chat),
            &RuntimeChatCapabilities::tool_call_bound_reasoning(),
            &ReplayContext::default(),
        )
        .unwrap();

        let messages = converted["messages"].as_array().unwrap();
        let assistant = messages
            .iter()
            .find(|message| message.get("tool_calls").is_some())
            .unwrap();
        assert_eq!(assistant["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(assistant["reasoning_content"], "Use both tools.");
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["role"] == "tool")
                .count(),
            2
        );
    }

    #[test]
    fn vision_chat_route_carries_view_image_output_after_the_tool_result() {
        let image_url = "data:image/png;base64,aW1hZ2U=";
        let request = json!({
            "model": "omen-alpha",
            "stream": false,
            "input": [
                {"type": "message", "role": "user", "content": "Inspect the frame."},
                {"type": "function_call", "call_id": "call_view", "name": "view_image", "arguments": "{}"},
                {
                    "type": "function_call_output",
                    "call_id": "call_view",
                    "output": [{"type": "input_image", "image_url": image_url, "detail": "high"}]
                }
            ]
        });
        let prepared = prepare_upstream_request_with_environment(
            &request,
            &reasoning_chat_route(),
            &HarnessProfile::generic(RuntimeWireFormat::Chat),
            &executor_environment(),
            None,
            false,
        )
        .unwrap();

        let messages = prepared["messages"].as_array().unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|message| message["role"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["user", "assistant", "tool", "user"]
        );
        assert!(messages[2]["content"]
            .as_str()
            .unwrap()
            .contains("image content follows"));
        assert!(!messages[2]["content"].as_str().unwrap().contains(image_url));
        assert_eq!(messages[3]["content"][1]["type"], "image_url");
        assert_eq!(messages[3]["content"][1]["image_url"]["url"], image_url);
        assert_eq!(messages[3]["content"][1]["image_url"]["detail"], "high");
    }

    fn noisy_png_data_url(width: u32, height: u32) -> String {
        use image::ImageEncoder as _;

        let mut state = 0x1234_5678_u32;
        let image = image::RgbImage::from_fn(width, height, |_x, _y| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            image::Rgb([state as u8, (state >> 8) as u8, (state >> 16) as u8])
        });
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(
                image.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png)
        )
    }

    #[test]
    fn oversized_chat_image_is_reencoded_under_the_request_target() {
        let original = noisy_png_data_url(512, 512);
        let mut body = json!({
            "model": "deepseek-v4-pro",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Inspect this image."},
                    {"type": "image_url", "image_url": {"url": original}}
                ]
            }]
        });
        let before = encoded_json_len(&body);
        assert!(before > 450_000, "fixture must exceed its test budget");

        fit_data_images_to_request_budget(&mut body, 450_000, 440_000);

        let after = encoded_json_len(&body);
        assert!(after <= 440_000, "fitted request remained {after} bytes");
        let fitted = body["messages"][0]["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap();
        assert!(fitted.starts_with("data:image/jpeg;base64,"));
    }

    #[test]
    fn image_below_the_provider_limit_remains_byte_identical() {
        let original = noisy_png_data_url(512, 512);
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [{"type": "image_url", "image_url": {"url": original}}]
            }]
        });
        let before = body.clone();

        assert!(encoded_json_len(&body) > 400_000);
        fit_data_images_to_request_budget(&mut body, 2_000_000, 400_000);

        assert_eq!(body, before);
    }

    #[test]
    fn opencode_go_chat_preparation_fits_a_tool_image_below_its_provider_limit() {
        let image_url = noisy_png_data_url(1152, 1152);
        let request = json!({
            "model": "deepseek-v4-pro",
            "stream": true,
            "input": [
                {"type": "message", "role": "user", "content": "Inspect the contact sheet."},
                {"type": "function_call", "call_id": "call_view", "name": "view_image", "arguments": "{}"},
                {
                    "type": "function_call_output",
                    "call_id": "call_view",
                    "output": [{"type": "input_image", "image_url": image_url, "detail": "high"}]
                }
            ]
        });
        let mut route = reasoning_chat_route();
        route.base_url = crate::opencode::OPENCODE_GO_BASE_URL.into();

        let prepared = prepare_upstream_request_with_environment(
            &request,
            &route,
            &HarnessProfile::generic(RuntimeWireFormat::Chat),
            &executor_environment(),
            None,
            false,
        )
        .unwrap();

        let body_bytes = encoded_json_len(&prepared);
        assert!(
            body_bytes <= crate::opencode::OPENCODE_GO_REQUEST_TARGET_BYTES,
            "OpenCode Go request remained {body_bytes} bytes"
        );
        assert!(prepared.to_string().contains("data:image/jpeg;base64,"));
    }

    #[test]
    fn vision_chat_route_closes_parallel_tools_before_carrying_their_images() {
        let converted = responses_to_chat(&json!({
            "model": "omen-alpha",
            "stream": false,
            "input": [
                {"type": "message", "role": "user", "content": "Inspect both."},
                {"type": "function_call", "call_id": "call_a", "name": "view_image", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_b", "name": "view_image", "arguments": "{}"},
                {
                    "type": "function_call_output",
                    "call_id": "call_a",
                    "output": [{"type": "input_image", "image_url": "data:image/png;base64,YQ=="}]
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_b",
                    "output": [{"type": "input_image", "image_url": "data:image/png;base64,Yg=="}]
                }
            ]
        }))
        .unwrap();

        let messages = converted["messages"].as_array().unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|message| message["role"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["user", "assistant", "tool", "tool", "user"]
        );
        assert_eq!(messages[4]["content"].as_array().unwrap().len(), 3);
        check_chat_transcript_invariants_with_provenance(
            messages,
            &[],
            &RuntimeChatCapabilities::default(),
            &ReplayContext::default(),
        )
        .unwrap();
    }

    #[test]
    fn text_only_chat_route_rejects_image_tool_output() {
        let mut route = reasoning_chat_route();
        route.vision = false;
        let error = prepare_upstream_request_with_environment(
            &json!({
                "model": "text-only",
                "input": [
                    {"type": "message", "role": "user", "content": "Inspect."},
                    {"type": "function_call", "call_id": "call_view", "name": "view_image", "arguments": "{}"},
                    {
                        "type": "function_call_output",
                        "call_id": "call_view",
                        "output": [{"type": "input_image", "image_url": "data:image/png;base64,aW1hZ2U="}]
                    }
                ]
            }),
            &route,
            &HarnessProfile::generic(RuntimeWireFormat::Chat),
            &executor_environment(),
            None,
            false,
        )
        .unwrap_err();

        assert!(error.contains("does not declare vision support"), "{error}");
    }

    #[test]
    fn chat_projection_keeps_last_user_query_after_system_and_tool_history() {
        let converted = responses_to_chat(&json!({
            "model": "qwen3.8-27b",
            "instructions": "You are a helpful assistant.",
            "tools": [{
                "type": "function",
                "name": "shell_command",
                "description": "Run a shell command",
                "parameters": {"type": "object"}
            }],
            "input": [
                {"type": "message", "role": "user", "content": "first question"},
                {"type": "function_call", "call_id": "call_1", "name": "shell_command", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "continue"}]},
                {"type": "message", "role": "user", "content": "what is the final answer?"}
            ]
        }))
        .unwrap();
        let messages = converted["messages"].as_array().unwrap();
        let last_user = messages
            .iter()
            .rev()
            .find(|message| message["role"] == "user")
            .expect("Chat projection must keep a last user query");
        assert_eq!(last_user["content"], "what is the final answer?");
        assert!(
            messages.iter().any(|message| message["role"] == "system"),
            "system instructions must still exist: {messages:?}"
        );
        assert!(
            messages.iter().any(|message| message["role"] == "tool"),
            "tool history must still exist: {messages:?}"
        );
        assert_eq!(
            messages.last().and_then(|message| message.get("role")),
            Some(&json!("user")),
            "the last semantic message must be the user query: {messages:?}"
        );
    }

    #[test]
    fn chat_projection_drops_empty_and_thinking_only_assistant_turns() {
        let converted = responses_to_chat_with_options(
            &json!({
                "model": "laguna-s-2.1-free",
                "input": [
                    {"type": "message", "role": "user", "content": "continue"},
                    {"type": "message", "role": "assistant", "content": ""},
                    {"type": "reasoning", "summary": [{"type": "summary_text", "text": "private thinking"}]}
                ]
            }),
            &HarnessProfile::generic(RuntimeWireFormat::Chat),
            &RuntimeChatCapabilities::tool_call_bound_reasoning(),
            &ReplayContext::default(),
        )
        .unwrap();
        let messages = converted["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert!(!serde_json::to_string(&converted)
            .unwrap()
            .contains("private thinking"));
    }

    #[test]
    fn chat_projection_omits_reasoning_only_assistant_when_no_tool_call_completed() {
        let (converted, projection) = responses_to_chat_projection(
            &json!({
                "model": "laguna-s-2.1-free",
                "input": [
                    {"type": "message", "role": "user", "content": "continue"},
                    {"type": "reasoning", "summary": [{"type": "summary_text", "text": "private thinking"}]},
                    {"type": "function_call", "call_id": "call_pending_a", "name": "shell_command", "arguments": "{}"},
                    {"type": "function_call", "call_id": "call_pending_b", "name": "web_search", "arguments": "{}"}
                ]
            }),
            &HarnessProfile::generic(RuntimeWireFormat::Chat),
            &RuntimeChatCapabilities::tool_call_bound_reasoning(),
            &ReplayContext::default(),
            true,
        )
        .unwrap();

        let messages = converted["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(projection.provenance.len(), messages.len());
        assert!(!messages.iter().any(|message| {
            message["role"] == "assistant"
                && (message.get("reasoning_content").is_some()
                    || message.get("reasoning").is_some())
        }));
    }

    #[test]
    fn chat_projection_strips_reasoning_from_visible_content_with_only_pending_calls() {
        let (converted, projection) = responses_to_chat_projection(
            &json!({
                "model": "laguna-s-2.1-free",
                "input": [
                    {"type": "message", "role": "user", "content": "continue"},
                    {"type": "message", "role": "assistant", "content": "I will inspect that now."},
                    {"type": "reasoning", "summary": [{"type": "summary_text", "text": "private thinking"}]},
                    {"type": "function_call", "call_id": "call_pending_a", "name": "shell_command", "arguments": "{}"},
                    {"type": "function_call", "call_id": "call_pending_b", "name": "web_search", "arguments": "{}"}
                ]
            }),
            &HarnessProfile::generic(RuntimeWireFormat::Chat),
            &RuntimeChatCapabilities::tool_call_bound_reasoning(),
            &ReplayContext::default(),
            true,
        )
        .unwrap();

        let messages = converted["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2, "{messages:?}");
        let assistant = messages
            .iter()
            .find(|message| message["role"] == "assistant")
            .expect("visible assistant content must survive");
        assert_eq!(assistant["content"], "I will inspect that now.");
        assert!(assistant.get("tool_calls").is_none());
        assert!(assistant.get("reasoning_content").is_none());
        assert!(assistant.get("reasoning").is_none());
        assert_eq!(projection.provenance.len(), messages.len());
        assert!(!serde_json::to_string(&converted)
            .unwrap()
            .contains("private thinking"));
    }

    #[test]
    fn chat_projection_keeps_tool_tail_by_default() {
        let converted = responses_to_chat(&json!({
            "model": "qwen3.8-27b",
            "instructions": "You are a helpful assistant.",
            "tools": [{
                "type": "function",
                "name": "shell_command",
                "description": "Run a shell command",
                "parameters": {"type": "object"}
            }],
            "input": [
                {"type": "message", "role": "user", "content": "what is the final answer?"},
                {"type": "function_call", "call_id": "call_1", "name": "shell_command", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "42"},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "use the tool result"}]}
            ]
        }))
        .unwrap();
        let messages = converted["messages"].as_array().unwrap();
        assert!(
            messages.iter().any(|message| message["role"] == "tool"),
            "tool history must still exist: {messages:?}"
        );
        let last = messages.last().expect("Chat projection must emit messages");
        assert_eq!(
            last["role"], "tool",
            "default Chat continuation keeps the tool result as the tail: {messages:?}"
        );
        let user_turns = messages
            .iter()
            .filter(|message| message["role"] == "user")
            .collect::<Vec<_>>();
        assert_eq!(user_turns.len(), 1);
        assert_eq!(user_turns[0]["content"], "what is the final answer?");
        assert!(!serde_json::to_string(&converted)
            .unwrap()
            .contains(crate::replay::NEUTRAL_USER_BRIDGE_TEXT));
    }

    fn history_entry(
        id: &str,
        input: Vec<Value>,
        output: Vec<Value>,
    ) -> crate::history::HistoryEntry {
        crate::history::HistoryEntry {
            response_id: id.into(),
            previous_response_id: None,
            route_id: "r".into(),
            continuation_realm: None,
            input_items: input,
            output_items: output,
            created_at: 1,
        }
    }

    fn production_chat_projection(
        body: &mut Value,
        chain: &[crate::history::HistoryEntry],
    ) -> ChatProjection {
        let outcome = crate::replay::hydrate_input_with_mode(
            body,
            chain,
            crate::continuation::ContinuationMode::PortableSemanticReplay,
        );
        let replay = outcome.replay_context();
        responses_to_chat_projection(
            body,
            &HarnessProfile::generic(RuntimeWireFormat::Chat),
            &RuntimeChatCapabilities::default(),
            &replay,
            true,
        )
        .unwrap()
        .1
    }

    fn repaste_user_occurrence(projection: &mut ChatProjection, occurrence_id: &str) {
        let pair = projection
            .messages
            .iter()
            .zip(projection.provenance.iter())
            .find(|(message, provenance)| {
                message.get("role").and_then(Value::as_str) == Some("user")
                    && provenance.occurrence_id.as_deref() == Some(occurrence_id)
            })
            .map(|(message, provenance)| (message.clone(), provenance.clone()));
        if let Some((message, provenance)) = pair {
            projection.push(message, provenance);
        }
    }

    #[test]
    fn production_provenance_fails_closed_when_a_helper_repastes_the_first_user() {
        let chain = [history_entry(
            "resp_1",
            vec![json!({"role": "user", "content": "inspect the repository"})],
            vec![
                json!({
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "shell_command",
                    "arguments": "{}"
                }),
                json!({
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "ok"
                }),
            ],
        )];
        let mut body = json!({
            "model": "laguna-mini",
            "previous_response_id": "resp_1",
            "tools": [{
                "type": "function",
                "name": "shell_command",
                "parameters": {"type": "object"}
            }],
            "input": [{"role": "user", "content": "continue"}]
        });
        let mut projection = production_chat_projection(&mut body, &chain);
        let first_user = projection
            .provenance
            .iter()
            .zip(projection.messages.iter())
            .find(|(provenance, message)| {
                message.get("role").and_then(Value::as_str) == Some("user")
                    && provenance.from_prefix
            })
            .and_then(|(provenance, _)| provenance.occurrence_id.clone())
            .expect("stored user occurrence");
        assert!(
            first_user.starts_with("store:resp_1:input:0"),
            "production identity must be the stored source, got {first_user}"
        );
        repaste_user_occurrence(&mut projection, &first_user);
        let error = check_chat_transcript_invariants_with_provenance(
            &projection.messages,
            &projection.provenance,
            &RuntimeChatCapabilities::default(),
            &ReplayContext::default(),
        )
        .unwrap_err();
        assert!(
            error.contains(&first_user),
            "dispatch-time invariant must fail closed on a cloned occurrence: {error}"
        );
    }

    #[test]
    fn production_provenance_fails_closed_when_a_middle_user_is_repasted() {
        let chain = [history_entry(
            "resp_1",
            vec![
                json!({"role": "user", "content": "first"}),
                json!({"role": "assistant", "content": "ack"}),
                json!({"role": "user", "content": "middle task"}),
            ],
            vec![
                json!({
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "shell_command",
                    "arguments": "{}"
                }),
                json!({
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "ok"
                }),
            ],
        )];
        let mut body = json!({
            "model": "laguna-mini",
            "previous_response_id": "resp_1",
            "tools": [{
                "type": "function",
                "name": "shell_command",
                "parameters": {"type": "object"}
            }],
            "input": [{"role": "user", "content": "continue"}]
        });
        let mut projection = production_chat_projection(&mut body, &chain);
        let middle = crate::replay::stored_item_identity("resp_1", "input", 2);
        repaste_user_occurrence(&mut projection, &middle);
        let error = check_chat_transcript_invariants_with_provenance(
            &projection.messages,
            &projection.provenance,
            &RuntimeChatCapabilities::default(),
            &ReplayContext::default(),
        )
        .unwrap_err();
        assert!(error.contains(&middle), "{error}");
    }

    #[test]
    fn production_provenance_keeps_two_real_same_text_user_turns() {
        let chain = [history_entry(
            "resp_1",
            vec![json!({"role": "user", "content": "same text"})],
            vec![json!({"role": "assistant", "content": "ack"})],
        )];
        let mut body = json!({
            "model": "laguna-mini",
            "previous_response_id": "resp_1",
            "input": [{"role": "user", "content": "same text"}]
        });
        let projection = production_chat_projection(&mut body, &chain);
        let user_ids: Vec<_> = projection
            .messages
            .iter()
            .zip(projection.provenance.iter())
            .filter(|(message, _)| message.get("role").and_then(Value::as_str) == Some("user"))
            .map(|(_, provenance)| provenance.occurrence_id.clone().unwrap())
            .collect();
        assert_eq!(user_ids.len(), 2);
        assert_ne!(user_ids[0], user_ids[1]);
        check_chat_transcript_invariants_with_provenance(
            &projection.messages,
            &projection.provenance,
            &RuntimeChatCapabilities::default(),
            &ReplayContext::default(),
        )
        .unwrap();
    }

    #[test]
    fn qwen_capability_uses_one_neutral_bridge_without_copying_the_query() {
        let converted = responses_to_chat_with_options(
            &json!({
                "model": "qwen3.8-27b",
                "instructions": "You are a helpful assistant.",
                "tools": [{
                    "type": "function",
                    "name": "shell_command",
                    "description": "Run a shell command",
                    "parameters": {"type": "object"}
                }],
                "input": [
                    {"type": "message", "role": "user", "content": "what is the final answer?"},
                    {"type": "function_call", "call_id": "call_1", "name": "shell_command", "arguments": "{}"},
                    {"type": "function_call_output", "call_id": "call_1", "output": "42"}
                ]
            }),
            &HarnessProfile::generic(RuntimeWireFormat::Chat),
            &RuntimeChatCapabilities::qwen_bridge(),
            &ReplayContext::default(),
        )
        .unwrap();
        let messages = converted["messages"].as_array().unwrap();
        let last = messages.last().unwrap();
        assert_eq!(last["role"], "user");
        assert_eq!(last["content"], crate::replay::NEUTRAL_USER_BRIDGE_TEXT);
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["content"] == crate::replay::NEUTRAL_USER_BRIDGE_TEXT)
                .count(),
            1
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["role"] == "user"
                    && message["content"] == "what is the final answer?")
                .count(),
            1
        );
    }

    /// The exact tail Codex's own local compaction writes: the summary is the
    /// last item of the replacement history, carries the shared summary
    /// prefix, and nothing follows it.
    fn native_local_compaction_summary(text: &str) -> Value {
        json!({
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": format!("{}\n{text}", crate::compaction::OFFICIAL_SUMMARY_PREFIX)
            }],
            "internal_chat_message_metadata_passthrough": {
                "content_item_kinds": ["compaction.summary"]
            }
        })
    }

    fn resume_continuations(messages: &[Value]) -> usize {
        messages
            .iter()
            .filter(|message| {
                message.get("role").and_then(Value::as_str) == Some("user")
                    && crate::replay::is_checkpoint_resume_text(
                        &crate::adapter::content_to_plain_string(
                            message.get("content").unwrap_or(&Value::Null),
                        ),
                    )
            })
            .count()
    }

    #[test]
    fn native_local_compaction_summary_tail_gets_one_resume_continuation() {
        let converted = responses_to_chat(&json!({
            "model": "vlm-qwen",
            "stream": false,
            "input": [
                {"role": "user", "content": "install the extension on the emulator"},
                native_local_compaction_summary("{\"next\":\"push the unpacked folder\"}")
            ]
        }))
        .unwrap();
        let messages = converted["messages"].as_array().unwrap();
        let checkpoint = messages
            .iter()
            .find(|message| {
                crate::adapter::content_to_plain_string(
                    message.get("content").unwrap_or(&Value::Null),
                )
                .starts_with(CHAT_CHECKPOINT_PREFACE)
            })
            .expect("the summary is projected as a checkpoint");
        assert_eq!(checkpoint["role"], "assistant");
        assert_eq!(resume_continuations(messages), 1);
        let last = messages.last().unwrap();
        assert_eq!(last["role"], "user");
        assert_eq!(last["content"], crate::replay::CHAT_CHECKPOINT_RESUME_TEXT);
        // The continuation is fixed text: the original task is never resent.
        assert!(!last["content"]
            .as_str()
            .unwrap()
            .contains("install the extension"));
    }

    /// The turn Codex sends immediately after a mid-turn local compaction:
    /// the replacement history replaces the tool work that came before it, and
    /// the summary is the whole suffix. This runs the production hydrate →
    /// project → dispatch-invariant path, not the converter alone.
    #[test]
    fn production_path_resumes_after_a_mid_turn_local_compaction() {
        let chain = [history_entry(
            "resp_1",
            vec![json!({"role": "user", "content": "install the extension on the emulator"})],
            vec![
                json!({
                    "type": "function_call",
                    "call_id": "c1",
                    "name": "shell_command",
                    "arguments": "{}"
                }),
                json!({"type": "function_call_output", "call_id": "c1", "output": "device offline"}),
            ],
        )];
        let mut body = json!({
            "model": "vlm-qwen",
            "previous_response_id": "resp_1",
            "tools": [{
                "type": "function",
                "name": "shell_command",
                "parameters": {"type": "object"}
            }],
            "input": [native_local_compaction_summary("{\"next\":\"wait for the device\"}")]
        });
        let projection = production_chat_projection(&mut body, &chain);
        check_chat_transcript_invariants_with_provenance(
            &projection.messages,
            &projection.provenance,
            &RuntimeChatCapabilities::default(),
            &ReplayContext::default(),
        )
        .unwrap();
        assert_eq!(resume_continuations(&projection.messages), 1);
        let last = projection.messages.last().unwrap();
        assert_eq!(last["role"], "user");
        assert_eq!(last["content"], crate::replay::CHAT_CHECKPOINT_RESUME_TEXT);
        let checkpoint_index = projection
            .messages
            .iter()
            .position(crate::replay::is_chat_checkpoint_message)
            .expect("the summary is projected as a checkpoint");
        assert_eq!(checkpoint_index, projection.messages.len() - 2);
    }

    #[test]
    fn a_summary_already_followed_by_a_user_turn_gets_no_resume_continuation() {
        let converted = responses_to_chat(&json!({
            "model": "vlm-qwen",
            "input": [
                {"role": "user", "content": "install the extension"},
                native_local_compaction_summary("{\"next\":\"push\"}"),
                {"role": "user", "content": "actually check the emulator first"}
            ]
        }))
        .unwrap();
        let messages = converted["messages"].as_array().unwrap();
        assert_eq!(resume_continuations(messages), 0);
        assert_eq!(messages.last().unwrap()["role"], "user");
        assert_eq!(
            messages.last().unwrap()["content"],
            "actually check the emulator first"
        );
    }

    #[test]
    fn a_summary_followed_by_a_tool_result_gets_no_resume_continuation() {
        let converted = responses_to_chat(&json!({
            "model": "vlm-qwen",
            "input": [
                native_local_compaction_summary("{\"next\":\"push\"}"),
                {
                    "type": "function_call",
                    "call_id": "c1",
                    "name": "shell_command",
                    "arguments": "{}"
                },
                {"type": "function_call_output", "call_id": "c1", "output": "ok"}
            ]
        }))
        .unwrap();
        let messages = converted["messages"].as_array().unwrap();
        assert_eq!(resume_continuations(messages), 0);
        assert_eq!(messages.last().unwrap()["role"], "tool");
    }

    #[test]
    fn an_ordinary_assistant_tail_is_not_mistaken_for_a_checkpoint() {
        let converted = responses_to_chat(&json!({
            "model": "vlm-qwen",
            "input": [
                {"role": "user", "content": "summarize the plan"},
                {
                    "type": "message",
                    "role": "assistant",
                    "content": "Here is the plan, as a conversation checkpoint of sorts."
                }
            ]
        }))
        .unwrap();
        let messages = converted["messages"].as_array().unwrap();
        assert_eq!(resume_continuations(messages), 0);
        assert_eq!(messages.last().unwrap()["role"], "assistant");
    }

    #[test]
    fn the_resume_continuation_is_appended_at_most_once() {
        let mut projection = ChatProjection::default();
        projection.push(
            json!({
                "role": "assistant",
                "content": format!("{CHAT_CHECKPOINT_PREFACE}\n\nsummary")
            }),
            ChatMessageProvenance {
                occurrence_id: None,
                from_prefix: false,
            },
        );
        apply_checkpoint_resume_tail_to_projection(&mut projection);
        apply_checkpoint_resume_tail_to_projection(&mut projection);
        assert_eq!(resume_continuations(&projection.messages), 1);
        assert_eq!(projection.messages.len(), 2);
    }

    #[test]
    fn dispatch_fails_closed_when_a_checkpoint_is_the_last_message() {
        let messages = vec![
            json!({"role": "user", "content": "install the extension"}),
            json!({
                "role": "assistant",
                "content": format!("{CHAT_CHECKPOINT_PREFACE}\n\nsummary")
            }),
        ];
        let error = check_chat_transcript_invariants_with_provenance(
            &messages,
            &[],
            &RuntimeChatCapabilities::default(),
            &ReplayContext::default(),
        )
        .unwrap_err();
        assert!(
            error.contains("checkpoint must not be the last message"),
            "{error}"
        );
    }

    #[test]
    fn canonical_checkpoint_is_assistant_owned_not_a_user_query() {
        let converted = responses_to_chat(&json!({
            "model": "laguna-mini",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{
                        "type": "input_text",
                        "text": format!(
                            "{}\n{}",
                            crate::compaction::OFFICIAL_SUMMARY_PREFIX,
                            "{\"goal\":\"keep going\"}"
                        )
                    }],
                    "metadata": {"vellum_checkpoint": "canonical"}
                },
                {
                    "type": "function_call",
                    "call_id": "c1",
                    "name": "shell_command",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "c1",
                    "output": "ok"
                }
            ]
        }))
        .unwrap();
        let messages = converted["messages"].as_array().unwrap();
        let checkpoints = messages
            .iter()
            .filter(|message| {
                crate::adapter::content_to_plain_string(
                    message.get("content").unwrap_or(&Value::Null),
                )
                .starts_with(CHAT_CHECKPOINT_PREFACE)
            })
            .collect::<Vec<_>>();
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0]["role"], "assistant");
        assert_eq!(messages.last().unwrap()["role"], "tool");
        assert_eq!(resume_continuations(messages), 0);
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["role"] == "user")
                .count(),
            0
        );
    }

    #[test]
    fn reasoning_after_function_call_binds_to_the_same_assistant() {
        let converted = responses_to_chat_with_options(
            &json!({
                "model": "deepseek-v4-flash",
                "input": [
                    {"type": "message", "role": "user", "content": "continue"},
                    {"type": "function_call", "call_id": "c1", "name": "shell_command", "arguments": "{}"},
                    {"type": "reasoning", "summary": [{"type": "summary_text", "text": "after the call"}]},
                    {"type": "function_call_output", "call_id": "c1", "output": "ok"}
                ]
            }),
            &HarnessProfile::generic(RuntimeWireFormat::Chat),
            &RuntimeChatCapabilities::tool_call_bound_reasoning(),
            &ReplayContext::default(),
        )
        .unwrap();
        let messages = converted["messages"].as_array().unwrap();
        let assistant = messages
            .iter()
            .find(|message| message.get("tool_calls").is_some())
            .unwrap();
        assert_eq!(assistant["reasoning_content"], "after the call");
        assert_eq!(assistant["tool_calls"][0]["id"], "c1");
        let next = &messages[messages
            .iter()
            .position(|message| message.get("tool_calls").is_some())
            .unwrap()
            + 1];
        assert_eq!(next["role"], "tool");
        assert_eq!(next["tool_call_id"], "c1");
    }

    #[test]
    fn reasoning_does_not_replay_across_route_switch() {
        let converted = responses_to_chat_with_options(
            &json!({
                "model": "deepseek-v4-flash",
                "input": [
                    {"type": "message", "role": "user", "content": "continue"},
                    {"type": "reasoning", "summary": [{"type": "summary_text", "text": "secret prior reasoning"}]},
                    {"type": "function_call", "call_id": "c1", "name": "shell_command", "arguments": "{}"},
                    {"type": "function_call_output", "call_id": "c1", "output": "ok"}
                ]
            }),
            &HarnessProfile::generic(RuntimeWireFormat::Chat),
            &RuntimeChatCapabilities::tool_call_bound_reasoning(),
            &ReplayContext {
                same_route: false,
                allow_reasoning_replay: false,
                ..ReplayContext::default()
            },
        )
        .unwrap();
        let wire = serde_json::to_string(&converted).unwrap();
        assert!(
            !wire.contains("secret prior reasoning"),
            "route switch must not replay readable reasoning: {wire}"
        );
    }

    fn effort_request(effort: &str) -> Value {
        json!({
            "model": "vlm-test",
            "reasoning": {"effort": effort},
            "input": [{"type": "message", "role": "user", "content": "hi"}]
        })
    }

    fn responses_route_with_transport(
        transport: RuntimeReasoningEffortTransport,
    ) -> ProfileAdapterRoute {
        ProfileAdapterRoute {
            upstream_model: "qwen3.8-27b-q4k".into(),
            reasoning: true,
            vision: false,
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            wire: RuntimeWireFormat::Responses,
            base_url: "http://127.0.0.1:8080/v1".into(),
            chat_capabilities: RuntimeChatCapabilities::default(),
            tool_capabilities: crate::route::RuntimeToolCapabilities::default(),
            reasoning_effort_transport: transport,
        }
    }

    fn chat_route_with_transport(
        transport: RuntimeReasoningEffortTransport,
    ) -> ProfileAdapterRoute {
        ProfileAdapterRoute {
            upstream_model: "deepseek-v4-flash".into(),
            reasoning: true,
            vision: true,
            provider_kind: RuntimeProviderKind::OpenAiCompatible,
            wire: RuntimeWireFormat::Chat,
            base_url: "https://opencode.example.test/v1".into(),
            chat_capabilities: RuntimeChatCapabilities::tool_call_bound_reasoning(),
            tool_capabilities: crate::route::RuntimeToolCapabilities::default(),
            reasoning_effort_transport: transport,
        }
    }

    #[test]
    fn responses_object_transport_leaves_the_already_native_effort_field_untouched() {
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );
        let prepared = prepare_upstream_request_with_environment(
            &effort_request("high"),
            &responses_route_with_transport(RuntimeReasoningEffortTransport::ResponsesObject),
            &profile,
            &executor_environment(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(prepared["reasoning"]["effort"], "high");
        assert!(prepared.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn provider_specific_transport_on_responses_wire_merges_into_chat_template_kwargs() {
        // The real llama.cpp/hoi case: wire is Responses, but the verified
        // transport is ProviderSpecific because the deployment's Jinja
        // template only reads `chat_template_kwargs.reasoning_effort`.
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );
        let prepared = prepare_upstream_request_with_environment(
            &effort_request("xhigh"),
            &responses_route_with_transport(RuntimeReasoningEffortTransport::ProviderSpecific),
            &profile,
            &executor_environment(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            prepared["chat_template_kwargs"]["reasoning_effort"],
            "xhigh"
        );
        // The native Responses field is removed, not left in place --
        // ProviderSpecific relocates the value, it does not duplicate it.
        // Two disagreeing encodings on the wire is worse than one.
        assert!(
            prepared.get("reasoning").is_none() || prepared["reasoning"].get("effort").is_none(),
            "ProviderSpecific must not leave a redundant reasoning.effort behind: {prepared}"
        );
    }

    #[test]
    fn provider_specific_transport_strips_only_effort_and_keeps_reasoning_summary() {
        // `reasoning.summary` is a real, independent Codex request field --
        // relocating `effort` into chat_template_kwargs must not take it out
        // with the rest of the `reasoning` object.
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );
        let mut request = effort_request("high");
        request["reasoning"]["summary"] = json!("detailed");
        let prepared = prepare_upstream_request_with_environment(
            &request,
            &responses_route_with_transport(RuntimeReasoningEffortTransport::ProviderSpecific),
            &profile,
            &executor_environment(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(prepared["chat_template_kwargs"]["reasoning_effort"], "high");
        assert!(prepared["reasoning"].get("effort").is_none());
        assert_eq!(prepared["reasoning"]["summary"], "detailed");
    }

    #[test]
    fn chat_field_transport_writes_the_flat_reasoning_effort_key() {
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Chat,
            HarnessOptions::default(),
            false,
        );
        let prepared = prepare_upstream_request_with_environment(
            &effort_request("low"),
            &chat_route_with_transport(RuntimeReasoningEffortTransport::ChatField),
            &profile,
            &executor_environment(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(prepared["reasoning_effort"], "low");
        assert!(prepared.get("chat_template_kwargs").is_none());
        assert!(
            prepared.get("reasoning").is_none() || prepared["reasoning"].get("effort").is_none(),
            "ChatField must not also carry the nested reasoning.effort shape: {prepared}"
        );
    }

    #[test]
    fn chat_object_transport_writes_the_nested_reasoning_object() {
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Chat,
            HarnessOptions::default(),
            false,
        );
        let prepared = prepare_upstream_request_with_environment(
            &effort_request("medium"),
            &chat_route_with_transport(RuntimeReasoningEffortTransport::ChatObject),
            &profile,
            &executor_environment(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(prepared["reasoning"]["effort"], "medium");
        assert!(prepared.get("reasoning_effort").is_none());
        assert!(
            prepared.get("chat_template_kwargs").is_none(),
            "ChatObject must not also carry the ProviderSpecific shape: {prepared}"
        );
    }

    #[test]
    fn provider_specific_transport_on_chat_wire_merges_into_chat_template_kwargs() {
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Chat,
            HarnessOptions::default(),
            false,
        );
        let prepared = prepare_upstream_request_with_environment(
            &effort_request("xhigh"),
            &chat_route_with_transport(RuntimeReasoningEffortTransport::ProviderSpecific),
            &profile,
            &executor_environment(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            prepared["chat_template_kwargs"]["reasoning_effort"],
            "xhigh"
        );
        assert!(prepared.get("reasoning_effort").is_none());
        assert!(
            prepared.get("reasoning").is_none() || prepared["reasoning"].get("effort").is_none(),
            "ProviderSpecific on Chat wire must not also carry the Responses shape: {prepared}"
        );
    }

    #[test]
    fn provider_specific_merge_never_clobbers_sibling_chat_template_kwargs_entries() {
        // `apply_provider_chat_options` already seeds `chat_template_kwargs`
        // with `enable_thinking` for NVIDIA NIM reasoning models -- the
        // Effort projection must land as a sibling key, not replace the
        // object outright.
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Chat,
            HarnessOptions::default(),
            false,
        );
        let mut route =
            chat_route_with_transport(RuntimeReasoningEffortTransport::ProviderSpecific);
        route.upstream_model = "nemotron-reasoning".into();
        route.base_url = "https://integrate.api.nvidia.com/v1".into();
        let prepared = prepare_upstream_request_with_environment(
            &effort_request("high"),
            &route,
            &profile,
            &executor_environment(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(prepared["chat_template_kwargs"]["reasoning_effort"], "high");
        assert_eq!(prepared["chat_template_kwargs"]["enable_thinking"], true);
    }

    #[test]
    fn none_transport_leaves_the_body_without_any_effort_field() {
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Chat,
            HarnessOptions::default(),
            false,
        );
        let prepared = prepare_upstream_request_with_environment(
            &effort_request("high"),
            &chat_route_with_transport(RuntimeReasoningEffortTransport::None),
            &profile,
            &executor_environment(),
            None,
            false,
        )
        .unwrap();
        assert!(prepared.get("reasoning_effort").is_none());
        assert!(prepared.get("chat_template_kwargs").is_none());
        assert!(
            prepared.get("reasoning").is_none()
                || prepared["reasoning"].get("effort").is_none(),
            "an unsupported transport must not leak the Responses-shaped field onto Chat wire: {prepared}"
        );
    }

    #[test]
    fn no_effort_in_the_request_leaves_provider_specific_routes_untouched() {
        // A request with no `reasoning` at all (Guardian/no-reasoning
        // requests) must not fabricate a `chat_template_kwargs` entry just
        // because the route's transport is ProviderSpecific.
        let profile = resolve_with_options(
            RuntimeProviderKind::OpenAiCompatible,
            RuntimeWireFormat::Chat,
            HarnessOptions::default(),
            false,
        );
        let request = json!({
            "model": "vlm-test",
            "input": [{"type": "message", "role": "user", "content": "hi"}]
        });
        let prepared = prepare_upstream_request_with_environment(
            &request,
            &chat_route_with_transport(RuntimeReasoningEffortTransport::ProviderSpecific),
            &profile,
            &executor_environment(),
            None,
            false,
        )
        .unwrap();
        assert!(prepared.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn chat_adapter_strips_vellum_checkpoint_metadata_from_upstream_messages() {
        let converted = responses_to_chat_with_options(
            &json!({
                "model": "deepseek-v4-flash",
                "input": [
                    {
                        "type": "message",
                        "role": "developer",
                        "content": format!("{}

## Current Goal
Keep going", crate::compaction::OFFICIAL_SUMMARY_PREFIX),
                        "metadata": {
                            "vellum_checkpoint": "canonical",
                            "schema_version": 2,
                            "checkpoint": {"task": {"currentGoal": "Keep going"}}
                        }
                    },
                    {
                        "type": "function_call",
                        "call_id": "c1",
                        "name": "shell_command",
                        "arguments": "{}"
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "c1",
                        "output": "ok"
                    }
                ]
            }),
            &HarnessProfile::generic(RuntimeWireFormat::Chat),
            &RuntimeChatCapabilities::default(),
            &ReplayContext::default(),
        )
        .unwrap();

        let messages = converted["messages"].as_array().unwrap();
        for msg in messages {
            assert!(
                msg.get("metadata").is_none(),
                "Chat messages must never contain metadata: {:?}",
                msg
            );
        }
        // Developer summary is mapped to system/user message with markdown content
        assert!(messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("Keep going"));
    }

    #[test]
    fn grok_responses_adapter_strips_vellum_checkpoint_metadata() {
        let mut request = json!({
            "model": "grok-4.5",
            "input": [
                {
                    "type": "message",
                    "role": "developer",
                    "content": "Keep going",
                    "metadata": {
                        "vellum_checkpoint": "canonical",
                        "schema_version": 2,
                        "checkpoint": {"task": {"currentGoal": "Keep going"}}
                    }
                },
                {
                    "type": "function_call",
                    "call_id": "c1",
                    "name": "shell_command",
                    "arguments": "{}"
                }
            ]
        });
        let profile = HarnessProfile::grok_sol_translated_direct();
        normalize_grok_responses_request(&mut request, &profile, false).unwrap();
        let items = request["input"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert!(
            items[0].get("metadata").is_none(),
            "Grok Responses input must not leak checkpoint metadata: {:?}",
            items[0]
        );
    }

    #[test]
    fn compatible_responses_adapter_strips_vellum_checkpoint_metadata() {
        let mut request = json!({
            "model": "glm-5.2",
            "input": [
                {
                    "type": "message",
                    "role": "developer",
                    "content": "Keep going",
                    "metadata": {
                        "vellum_checkpoint": "canonical",
                        "schema_version": 2,
                        "checkpoint": {"task": {"currentGoal": "Keep going"}}
                    }
                },
                {
                    "type": "function_call",
                    "call_id": "c1",
                    "name": "shell_command",
                    "arguments": "{}"
                }
            ]
        });
        let profile = HarnessProfile::generic(RuntimeWireFormat::Responses);
        normalize_compatible_responses_request(&mut request, &profile, false).unwrap();
        // developer message was folded into top-level instructions, input only has function_call
        let items = request["input"].as_array().unwrap();
        for item in items {
            assert!(
                item.get("metadata").is_none(),
                "Compatible Responses item must not leak metadata: {:?}",
                item
            );
        }
        assert!(request["instructions"]
            .as_str()
            .unwrap()
            .contains("Keep going"));
    }

    #[test]
    fn compatible_responses_lowers_rich_tool_images_to_bounded_text() {
        let image_url = "data:image/png;base64,iVBORw0KGgo=";
        let mut request = json!({
            "model": "qwen",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_view_image",
                    "name": "view_image",
                    "arguments": "{}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_view_image",
                    "output": [
                        {"type": "input_text", "text": "Screenshot captured."},
                        {"type": "input_image", "image_url": image_url, "detail": "high"}
                    ]
                }
            ]
        });
        let profile = HarnessProfile::generic(RuntimeWireFormat::Responses);

        normalize_compatible_responses_request(&mut request, &profile, false).unwrap();

        let input = request["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[1]["type"], "function_call_output");
        let output = input[1]["output"].as_str().unwrap();
        assert!(output.contains("Screenshot captured."));
        assert!(output.contains("this route cannot transport"));
        assert!(!request.to_string().contains(image_url));
        assert!(input
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .all(|item| item["output"].is_string()));
    }

    #[test]
    fn compatible_responses_removes_image_payload_from_provider_request() {
        let image_url = "data:image/png;base64,secret-image-payload";
        let mut request = json!({
            "model": "text-only",
            "input": [{
                "type": "function_call_output",
                "call_id": "call_view_image",
                "output": [{"type": "input_image", "image_url": image_url}]
            }]
        });
        let profile = HarnessProfile::generic(RuntimeWireFormat::Responses);

        normalize_compatible_responses_request(&mut request, &profile, false).unwrap();

        let input = request["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "function_call_output");
        assert!(input[0]["output"]
            .as_str()
            .unwrap()
            .contains("this route cannot transport"));
        assert!(!request.to_string().contains("secret-image-payload"));
    }

    #[test]
    fn compatible_responses_leaves_plain_text_tool_outputs_unchanged() {
        let mut request = json!({
            "model": "qwen",
            "input": [{
                "type": "function_call_output",
                "call_id": "call_shell",
                "output": "exit code: 0"
            }]
        });
        let profile = HarnessProfile::generic(RuntimeWireFormat::Responses);

        normalize_compatible_responses_request(&mut request, &profile, false).unwrap();

        assert_eq!(request["input"][0]["output"], "exit code: 0");
        assert_eq!(request["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn glm_empty_tool_result_encodes_as_empty_string_not_null() {
        for output in [json!(null), json!("")] {
            let mut request = json!({
                "model": "glm-4.6",
                "input": [{
                    "type": "function_call_output",
                    "call_id": "call_empty",
                    "output": output
                }]
            });
            let profile = HarnessProfile::generic(RuntimeWireFormat::Responses);
            normalize_compatible_responses_request(&mut request, &profile, false).unwrap();
            assert_eq!(request["input"][0]["output"], "");
            assert!(request["input"][0]["output"].is_string());
        }

        let mut empty_array = json!({
            "model": "glm-4.6",
            "input": [{
                "type": "function_call_output",
                "call_id": "call_empty_parts",
                "output": []
            }]
        });
        let profile = HarnessProfile::generic(RuntimeWireFormat::Responses);
        normalize_compatible_responses_request(&mut empty_array, &profile, false).unwrap();
        assert!(empty_array["input"][0]["output"].is_string());
        assert_ne!(empty_array["input"][0]["output"], Value::Null);
        assert_eq!(empty_array["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn official_responses_preserves_rich_tool_outputs_verbatim() {
        let request = json!({
            "model": "gpt-test",
            "input": [{
                "type": "function_call_output",
                "call_id": "call_view_image",
                "output": [{
                    "type": "input_image",
                    "image_url": "data:image/png;base64,official-payload"
                }]
            }]
        });
        let route = ProfileAdapterRoute {
            upstream_model: "gpt-test".into(),
            reasoning: true,
            vision: true,
            provider_kind: RuntimeProviderKind::Official,
            wire: RuntimeWireFormat::Responses,
            base_url: "https://api.openai.com/v1".into(),
            chat_capabilities: RuntimeChatCapabilities::default(),
            tool_capabilities: crate::route::RuntimeToolCapabilities::default(),
            reasoning_effort_transport: RuntimeReasoningEffortTransport::None,
        };

        let prepared = prepare_upstream_request_with_environment(
            &request,
            &route,
            &HarnessProfile::official_native(),
            &executor_environment(),
            None,
            false,
        )
        .unwrap();

        assert_eq!(prepared, request);
    }

    #[test]
    fn non_vellum_metadata_survives_grok_adapter() {
        let mut request = json!({
            "model": "grok-4.5",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": "Hello",
                    "metadata": {
                        "custom_field": "preserved",
                        "trace_id": "abc-456"
                    }
                }
            ]
        });
        let profile = HarnessProfile::grok_sol_translated_direct();
        normalize_grok_responses_request(&mut request, &profile, false).unwrap();
        let items = request["input"].as_array().unwrap();
        assert_eq!(items[0]["metadata"]["custom_field"], "preserved");
        assert_eq!(items[0]["metadata"]["trace_id"], "abc-456");
    }

    #[test]
    fn grok_adapter_strips_internal_efficiency_recovery_metadata_from_outbound_messages() {
        let route = grok_route();
        let profile = resolve_with_options(
            RuntimeProviderKind::GrokCli,
            RuntimeWireFormat::Responses,
            HarnessOptions::default(),
            false,
        );
        let synthetic_item = json!({
            "type": "message",
            "role": "developer",
            "id": "synthetic:task_efficiency_recovery:1",
            "content": "[Vellum execution checkpoint: research sprawl]",
            "internal_efficiency_recovery": {
                "recovery_count": 1,
                "request_index": 13,
                "cumulative_input_tokens": 80_000
            }
        });
        let request = json!({
            "model": "grok-4.5",
            "input": [synthetic_item]
        });
        let prepared = prepare_upstream_request_with_environment(
            &request,
            &route,
            &profile,
            &executor_environment(),
            None,
            false,
        )
        .unwrap();

        let input_items = prepared.get("input").and_then(Value::as_array).unwrap();
        assert_eq!(input_items.len(), 1);
        let out_msg = &input_items[0];
        assert_eq!(
            out_msg.get("content").and_then(Value::as_str),
            Some("[Vellum execution checkpoint: research sprawl]")
        );
        assert!(out_msg.get("internal_efficiency_recovery").is_none());
        assert!(out_msg.get("metadata").is_none());
    }
}
