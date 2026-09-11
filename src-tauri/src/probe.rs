//! 線路探測（doc/03）。
//!
//! 使用者只給一個端點，其餘靠探測決定。探測請求必須極小（max_tokens=1、
//! prompt 一個字），因為使用者還沒決定要不要用這條線。
//!
//! 純函式（`build_probe_body`、`infer_wire`、`extract_reasoning`、
//! `merge_context_window`）不碰網路，可單測。`probe_endpoint_with_client`
//! 接收注入的 reqwest client，正式流程用預設 client、測試用 `mockito`/本地伺服器。

use crate::error::{AppError, AppResult};
use crate::model::{
    ContinuationTail, EffortProbeStatus, ModelCapability, ModelProbeError, ProbeResult,
    ReasoningEffortTransport, ReasoningReplay, RouteReprobeReport, RuntimeChatCapabilities,
    WireFormat,
};
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use vellum_proxy_runtime::CHAT_CAPABILITY_PROBE_VERSION;

/// 可達性探測超時。使用者在等，連不上就連不上（doc/03：5–8s）。
///
/// 只用在 `/models`、`/api/tags` 這種列清單的請求——它們不跑推論，正常都在
/// 一秒內回來，所以「超過 8 秒」確實等於「這個端點連不上」。
pub const PROBE_TIMEOUT_SECS: u64 = 8;
/// OpenCode Zen/Go model capabilities are resolved from Vellum's bundled
/// registry. The network request only proves that the catalog endpoint is
/// reachable, so it should not hold the connect UI for the generic endpoint
/// budget when OpenCode is unavailable.
pub const OPENCODE_CATALOG_TIMEOUT_SECS: u64 = 3;
/// Inference budgets for an endpoint that cannot cold-start.
///
/// The generic budgets below are sized for a local single-slot runtime: 90
/// seconds exists because a cold llama.cpp can spend a minute loading weights
/// before the first token, and 45 seconds per tool attempt because attempts on
/// a loaded Ollama box measured 2-19s each. OpenCode Zen and Go are hosted
/// routers with the weights already resident, and nothing about them is cold.
/// Measured against the free tier: time-to-first-byte 0.58-0.78s, and a full
/// 64-token completion from a thinking model 12.3s.
///
/// Spending the local-runtime budget there is not free. Selected-model
/// verification runs one model at a time, so a user who ticks six models pays
/// the worst case six times over, and the worst case was three minutes per
/// model. The retry *count* is unchanged — `tool_choice: "required"` is
/// advisory on a router the same way it is on Ollama, and that is what the
/// retries exist for.
const OPENCODE_INFERENCE_TIMEOUT_SECS: u64 = 30;
const OPENCODE_TOOL_PROBE_TIMEOUT_SECS: u64 = 20;
const OPENCODE_TOOL_PROBE_TOTAL_BUDGET_SECS: u64 = 60;
/// 任何會產生 token 的探測請求的超時。
///
/// 這類請求要等模型真的算出東西：本機大型模型的冷載入可能超過 45 秒。
/// Provider 806 的 27B Qwen 最小 Responses 請求實測約 53 秒才回第一個完整
/// completion；舊的 45 秒上限會在模型正常回應前先把它判為逾時。90 秒仍然有界，
/// 並和串流 TTFB 使用相同的冷啟動預算；不執行推論的 `/models` 仍維持 8 秒。
pub const COMPLETION_PROBE_TIMEOUT_SECS: u64 = 90;
/// Cold-start TTFB budget for a streaming probe. Success is headers or the
/// first legal chunk, not a full `[DONE]`, so this budget is only for
/// time-to-first-byte.
pub const STREAMING_PROBE_TIMEOUT_SECS: u64 = 90;

/// 「極小」探測請求的 max_tokens。
pub const PROBE_MAX_TOKENS: u32 = 1;
const PROBE_USER_MESSAGE: &str = "ping";
// v4 verifies a real typed function call in addition to request acceptance.
// This distinguishes a Codex-capable vLLM/Ollama/llama.cpp deployment from a
// text-only server that merely ignores the `tools` field.
pub const HARNESS_PROBE_VERSION: u32 = CHAT_CAPABILITY_PROBE_VERSION;
const TOOL_PROBE_MAX_TOKENS: u32 = 64;
/// Hybrid reasoning models (Qwen3, DeepSeek-R1 and friends) spend their first
/// tokens thinking, so `TOOL_PROBE_MAX_TOKENS` truncates the response before the
/// tool call is ever emitted — the endpoint then looks chat-only even though it
/// calls tools correctly. Every attempt after the first gets room for a whole
/// think block plus the call; the first stays small so non-reasoning models keep
/// the fast single-request path.
///
/// Sized well above the observed think blocks for this trivial prompt (185-339
/// tokens on Qwen3.6-35B) because the ceiling is nearly free: a model that stops
/// at 250 tokens generates 250 tokens whatever the cap says. It is only paid
/// when a model would otherwise run away, which is exactly the case that used to
/// be misread as "no tool support".
const TOOL_PROBE_REASONING_MAX_TOKENS: u32 = 2048;
/// `tool_choice: "required"` is advisory on several runtimes — Ollama forwards it
/// to the template rather than constraining decoding, so a tool-capable model
/// answers in prose maybe a third of the time. Tool support is an existential
/// property: one typed call proves it, one miss proves nothing. Retry a bounded
/// number of times, but only while the response looks like a model that tried
/// (see `chat_probe_inconclusive`), so a plainly text-only server still fails on
/// the first request.
///
/// Measured decline rate against Ollama was roughly one in three, so three
/// attempts still misread a working endpoint about one run in six. A false
/// negative costs the user the whole provider — the model cannot even be
/// ticked — while an extra attempt costs seconds, so the count is set high and
/// the worst case is bounded by `TOOL_PROBE_TOTAL_BUDGET_SECS` instead.
const TOOL_PROBE_ATTEMPTS: usize = 5;
/// Wall-clock ceiling across all attempts of one harness check. Without it, a
/// slow endpoint that keeps declining could hold the probe for attempts x
/// timeout, which is far longer than anyone will wait on a settings screen.
/// A harness check runs twice per probe (wire detection, then per model), so the
/// probe's worst case is twice this.
/// Sized against a slow local runtime, not a fast hosted one: attempts on a
/// loaded Ollama box measured 2-19s each, so a 45s ceiling silently cut the
/// retries down to two or three and reintroduced the false negative this
/// mechanism exists to prevent.
const TOOL_PROBE_TOTAL_BUDGET_SECS: u64 = 90;
/// Tool probes run only after the initial completion has loaded the model, so
/// keep their per-attempt ceiling at 45 seconds. This preserves bounded retry
/// behavior without cutting off the one request that actually pays cold-start.
const TOOL_PROBE_TIMEOUT_SECS: u64 = 45;
const PROBE_ISSUE_PROVIDER_OPT_IN_REQUIRED: &str = "provider_opt_in_required";
const PROBE_ISSUE_PROVIDER_ACCESS_RESTRICTED: &str = "provider_access_restricted";
const PROBE_ISSUE_TIMEOUT: &str = "timeout";

// Enforced at compile time rather than by a test, because getting these
// relationships wrong is exactly how the probe started reporting a thinking
// endpoint as a broken one.
const _: () = {
    // Inference must never be cut off by the budget meant for list endpoints.
    assert!(COMPLETION_PROBE_TIMEOUT_SECS > PROBE_TIMEOUT_SECS);
    // Both inference entry probes cover the observed cold-start ceiling.
    assert!(STREAMING_PROBE_TIMEOUT_SECS >= COMPLETION_PROBE_TIMEOUT_SECS);
    // The wall clock has to fit more than one full-length attempt, or the retry
    // mechanism silently degrades back to a single try.
    assert!(TOOL_PROBE_TOTAL_BUDGET_SECS >= 2 * TOOL_PROBE_TIMEOUT_SECS);
    // The hosted budgets are shorter, never longer — a value that crept above
    // the generic one would mean OpenCode waits longer than a cold local box.
    assert!(OPENCODE_INFERENCE_TIMEOUT_SECS < STREAMING_PROBE_TIMEOUT_SECS);
    assert!(OPENCODE_TOOL_PROBE_TIMEOUT_SECS < TOOL_PROBE_TIMEOUT_SECS);
    // ...and they keep the same internal relationships, or the retry mechanism
    // degrades to a single attempt exactly where it was tightened.
    assert!(OPENCODE_INFERENCE_TIMEOUT_SECS > OPENCODE_CATALOG_TIMEOUT_SECS);
    assert!(OPENCODE_TOOL_PROBE_TOTAL_BUDGET_SECS >= 2 * OPENCODE_TOOL_PROBE_TIMEOUT_SECS);
};
pub const EFFORT_PROBE_VERSION: u32 = 1;
/// Avoid issuing an unbounded number of paid completion requests when a
/// marketplace endpoint (such as NVIDIA NIM) returns a very large catalog.
pub const MAX_MODEL_CAPABILITY_PROBES: usize = 24;

pub const OPENCODE_ZEN_BASE_URL: &str = "https://opencode.ai/zen/v1";
/// The Go tier is a separate, smaller OpenCode Zen catalog behind its own
/// endpoint — not a filtered view of the full catalog. The official console
/// (`packages/console/app/src/routes/zen/{v1,go/v1}/models.ts` in
/// github.com/anomalyco/opencode) serves `/zen/v1/models` from a "full" model
/// list and `/zen/go/v1/models` from a distinct "lite" one; a Go-subscription
/// key can list the full catalog's `/models` response without error, but the
/// premium models on it (Claude, Gemini, GPT, …) are not part of what that
/// subscription actually grants.
pub const OPENCODE_GO_BASE_URL: &str = "https://opencode.ai/zen/go/v1";

const OLLAMA_NATIVE_SUFFIXES: [&str; 3] = ["/api/generate", "/api/chat", "/api/tags"];

/// Convert a pasted endpoint into the OpenAI-compatible base URL Vellum
/// persists and routes through.
///
/// Ollama documents native endpoints such as `/api/generate`, but Codex needs
/// the richer Responses/Chat tool protocol. Modern Ollama exposes that
/// protocol under `/v1`, so a pasted native endpoint is treated as a discovery
/// alias and routed through the matching `/v1` interface.
pub fn normalize_provider_base_url(base_url: &str) -> String {
    let mut root = base_url.trim().trim_end_matches('/').to_string();
    // Strip discovery aliases and wire suffixes repeatedly so an endpoint like
    // `.../v1/v1/chat/completions` converges to `.../v1` instead of keeping a
    // doubled version prefix that the proxy would later turn into `/v1/v1/...`.
    loop {
        let lower = root.to_ascii_lowercase();
        if let Some(suffix) = OLLAMA_NATIVE_SUFFIXES
            .iter()
            .find(|suffix| lower.ends_with(*suffix))
        {
            root = root[..root.len() - suffix.len()].to_string();
            continue;
        }
        if let Some(suffix) = ["/responses", "/chat/completions", "/models"]
            .iter()
            .find(|suffix| lower.ends_with(&format!("/v1{suffix}")))
        {
            root = root[..root.len() - suffix.len()].to_string();
            continue;
        }
        if lower.ends_with("/v1/v1") {
            root = root[..root.len() - "/v1".len()].to_string();
            continue;
        }
        break;
    }
    let lower = root.to_ascii_lowercase();
    if lower.ends_with("/v1") {
        root
    } else {
        format!("{root}/v1")
    }
}

/// OpenCode Zen exposes one catalog through several provider protocols.  The
/// base URL therefore cannot be assigned one route-wide wire format: GPT/Grok
/// use Responses, the open-source coding models use Chat Completions, while
/// Claude/Qwen and Gemini use Anthropic/Google-native endpoints that Vellum
/// does not translate yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenCodeZenCatalog {
    /// `/zen/v1` — the full catalog (GPT/Claude/Gemini/Grok plus the open
    /// coding models, including the free tier).
    Full,
    /// `/zen/go/v1` — the OpenCode Go subscription's own, smaller catalog.
    /// This is not a filtered view of `Full`: a handful of its models (for
    /// example `hy3`, `mimo-v2-omni`) do not appear in the full catalog at
    /// all, and some ids shared with `Full` carry different numbers there.
    Go,
}

/// Which OpenCode Zen catalog a base URL resolves to, or `None` if it is not
/// an OpenCode Zen endpoint at all.
pub fn opencode_zen_catalog(base_url: &str) -> Option<OpenCodeZenCatalog> {
    let normalized = normalize_provider_base_url(base_url);
    let url = url::Url::parse(&normalized).ok()?;
    if !url
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("opencode.ai"))
    {
        return None;
    }
    match url
        .path()
        .trim_end_matches('/')
        .to_ascii_lowercase()
        .as_str()
    {
        "/zen/v1" => Some(OpenCodeZenCatalog::Full),
        "/zen/go/v1" => Some(OpenCodeZenCatalog::Go),
        _ => None,
    }
}

pub fn is_opencode_zen_endpoint(base_url: &str) -> bool {
    opencode_zen_catalog(base_url).is_some()
}

pub fn opencode_zen_wire_for_model(model: &str) -> Option<WireFormat> {
    match vellum_proxy_runtime::opencode::wire_for_model(model) {
        Some("responses") => Some(WireFormat::Responses),
        Some("chat") => Some(WireFormat::Chat),
        _ => None,
    }
}

/// (model id, context window, reasoning, free, deprecated) — sourced from
/// OpenCode Desktop's own bundled model registry (the same catalog
/// github.com/anomalyco/opencode's console generates `/zen/{v1,go/v1}/models`
/// from), not guessed from a naming convention or a live probe. Zen's
/// `/models` response itself carries no capability fields at all — just id,
/// object, created, owned_by.
type OpenCodeZenModelRow = (&'static str, Option<u64>, bool, bool, bool);

/// The full `/zen/v1` catalog.
const OPENCODE_ZEN_MODEL_METADATA: &[OpenCodeZenModelRow] = &[
    ("big-pickle", Some(200000), true, true, false),
    ("claude-3-5-haiku", Some(200000), false, false, true),
    ("claude-fable-5", Some(1000000), true, false, false),
    ("claude-fable-5-1", Some(1000000), true, false, false),
    ("claude-haiku-4-5", Some(200000), true, false, false),
    ("claude-opus-4-1", Some(200000), true, false, true),
    ("claude-opus-4-5", Some(200000), true, false, false),
    ("claude-opus-4-6", Some(1000000), true, false, false),
    ("claude-opus-4-7", Some(1000000), true, false, false),
    ("claude-opus-4-8", Some(1000000), true, false, false),
    ("claude-opus-5", Some(1000000), true, false, false),
    ("claude-sonnet-4", Some(1000000), true, false, false),
    ("claude-sonnet-4-5", Some(1000000), true, false, false),
    ("claude-sonnet-4-6", Some(1000000), true, false, false),
    ("claude-sonnet-5", Some(1000000), true, false, false),
    ("deepseek-v4-flash", Some(1000000), true, false, false),
    ("deepseek-v4-flash-free", Some(200000), true, true, true),
    (
        "deepseek-v4-flash-vision-exp",
        Some(1000000),
        true,
        false,
        false,
    ),
    ("deepseek-v4-pro", Some(1000000), true, false, false),
    ("gemini-3-flash", Some(1048576), true, false, false),
    ("gemini-3-pro", Some(1048576), true, false, true),
    ("gemini-3.1-pro", Some(1048576), true, false, false),
    ("gemini-3.5-flash", Some(1048576), true, false, false),
    ("gemini-3.5-flash-lite", Some(1048576), true, false, false),
    ("gemini-3.6-flash", Some(1048576), true, false, false),
    ("gemini-3.7-flash", Some(1048576), true, false, false),
    ("gemini-3.8-flash", Some(1048576), true, false, false),
    ("glm-4.6", Some(204800), true, false, true),
    ("glm-4.7", Some(204800), true, false, true),
    ("glm-4.7-free", Some(204800), true, true, true),
    ("glm-5", Some(204800), true, false, false),
    ("glm-5-free", Some(204800), true, true, true),
    ("glm-5.1", Some(204800), true, false, false),
    ("glm-5.2", Some(1000000), true, false, false),
    ("glm-5.3", Some(1000000), true, false, false),
    ("glm-5.3-flash", Some(1000000), true, false, false),
    ("gpt-5", Some(400000), true, false, false),
    ("gpt-5-codex", Some(400000), true, false, false),
    ("gpt-5-nano", Some(400000), true, false, false),
    ("gpt-5.1", Some(400000), true, false, false),
    ("gpt-5.1-codex", Some(400000), true, false, false),
    ("gpt-5.1-codex-max", Some(400000), true, false, false),
    ("gpt-5.1-codex-mini", Some(400000), true, false, false),
    ("gpt-5.2", Some(400000), true, false, false),
    ("gpt-5.2-codex", Some(400000), true, false, false),
    ("gpt-5.3-codex", Some(400000), true, false, false),
    ("gpt-5.3-codex-spark", Some(128000), true, false, false),
    ("gpt-5.4", Some(1050000), true, false, false),
    ("gpt-5.4-mini", Some(400000), true, false, false),
    ("gpt-5.4-nano", Some(400000), true, false, false),
    ("gpt-5.4-pro", Some(1050000), true, false, false),
    ("gpt-5.5", Some(1050000), true, false, false),
    ("gpt-5.5-pro", Some(1050000), true, false, false),
    ("gpt-5.6-luna", Some(1050000), true, false, false),
    ("gpt-5.6-sol", Some(1050000), true, false, false),
    ("gpt-5.6-terra", Some(1050000), true, false, false),
    ("gpt-6-astra", Some(1050000), true, false, false),
    ("grok-4.5", Some(500000), true, false, false),
    ("grok-4.6", Some(500000), true, false, false),
    ("grok-build-0.1", Some(256000), true, false, false),
    ("grok-code", Some(256000), true, true, true),
    ("hy3-free", Some(190000), true, true, true),
    ("hy3-preview-free", Some(256000), true, true, true),
    ("kimi-k2", Some(262144), false, false, true),
    ("kimi-k2-thinking", Some(262144), true, false, true),
    ("kimi-k2.5", Some(262144), true, false, false),
    ("kimi-k2.5-free", Some(262144), true, true, true),
    ("kimi-k2.6", Some(262144), true, false, false),
    ("kimi-k2.7-code", Some(262144), true, false, false),
    ("kimi-k3", Some(1048576), true, false, false),
    ("laguna-s-2.1-free", Some(256000), true, true, true),
    ("ling-2.6-flash-free", Some(262100), false, true, true),
    ("ling-3.0-flash-fin-free", Some(262144), true, true, false),
    ("ling-3.0-flash-free", Some(262144), true, true, true),
    ("longcat-2.0-free", Some(1000000), true, true, true),
    ("mimo-v2-flash-free", Some(262144), true, true, true),
    ("mimo-v2-omni-free", Some(262144), true, true, true),
    ("mimo-v2-pro-free", Some(1048576), true, true, true),
    ("mimo-v2.5-free", Some(200000), true, true, false),
    ("minimax-m2.1", Some(204800), true, false, true),
    ("minimax-m2.1-free", Some(204800), true, true, true),
    ("minimax-m2.5", Some(204800), true, false, false),
    ("minimax-m2.5-free", Some(204800), true, true, true),
    ("minimax-m2.7", Some(204800), true, false, false),
    ("minimax-m3", Some(512000), true, false, false),
    ("minimax-m3-free", Some(200000), true, true, true),
    ("muse-spark-1.2", Some(1048576), true, false, false),
    (
        "muse-spark-1.2-contributor-free",
        Some(1048576),
        true,
        true,
        false,
    ),
    ("muse-spark-1.3", Some(1048576), true, false, false),
    (
        "muse-spark-1.3-contributor-free",
        Some(1048576),
        true,
        true,
        false,
    ),
    ("nemotron-3-super-free", Some(204800), true, true, true),
    ("nemotron-3-ultra-free", Some(1000000), true, true, false),
    (
        "nemotron-3.5-lightning-free",
        Some(262144),
        true,
        true,
        false,
    ),
    ("north-mini-code-free", Some(256000), true, true, true),
    ("qwen3-coder", Some(262144), false, false, true),
    ("qwen3.5-plus", Some(262144), true, false, false),
    ("qwen3.6-plus", Some(262144), true, false, false),
    ("qwen3.6-plus-free", Some(262144), true, true, true),
    ("ring-2.6-1t-free", Some(262000), true, true, true),
    (
        "trinity-large-preview-free",
        Some(131072),
        false,
        true,
        true,
    ),
];

/// The `/zen/go/v1` catalog. Deliberately not merged with the table above:
/// several ids exist in both with different numbers (`glm-5.2` and
/// `minimax-m3` in particular carry a larger context window here), because
/// this is a distinct catalog behind a distinct endpoint, not a filtered view
/// of the full one.
const OPENCODE_GO_MODEL_METADATA: &[OpenCodeZenModelRow] = &[
    ("deepseek-v4-flash", Some(1000000), true, false, false),
    (
        "deepseek-v4-flash-vision-exp",
        Some(1000000),
        true,
        false,
        false,
    ),
    ("deepseek-v4-pro", Some(1000000), true, false, false),
    ("glm-5", Some(202752), true, false, true),
    ("glm-5.1", Some(202752), true, false, false),
    ("glm-5.2", Some(1000000), true, false, false),
    ("glm-5.3", Some(1000000), true, false, false),
    ("glm-5.3-flash", Some(1000000), true, false, false),
    ("gpt-5.6-luna", Some(1050000), true, false, false),
    ("grok-4.5", Some(500000), true, false, true),
    ("grok-4.6", Some(500000), true, false, false),
    ("hy3", Some(256000), true, false, false),
    ("hy4-preview", Some(1024000), true, false, false),
    ("kimi-k2.5", Some(262144), true, false, true),
    ("kimi-k2.6", Some(262144), true, false, false),
    ("kimi-k2.7-code", Some(262144), true, false, false),
    ("kimi-k3", Some(1048576), true, false, false),
    ("longcat-2.0", Some(1000000), true, false, false),
    ("mimo-v2-omni", Some(262144), true, false, true),
    ("mimo-v2-pro", Some(1048576), true, false, true),
    ("mimo-v2.5", Some(1000000), true, false, false),
    ("mimo-v2.5-pro", Some(1048576), true, false, false),
    ("minimax-m2.5", Some(204800), true, false, true),
    ("minimax-m2.7", Some(204800), true, false, false),
    ("minimax-m3", Some(1000000), true, false, false),
    (
        "muse-spark-1.2-contributor",
        Some(1048576),
        true,
        false,
        false,
    ),
    (
        "muse-spark-1.3-contributor",
        Some(1048576),
        true,
        false,
        false,
    ),
    ("omen-alpha", Some(500000), true, false, false),
    ("qwen3.5-plus", Some(262144), true, false, true),
    ("qwen3.6-plus", Some(1000000), true, false, false),
    ("qwen3.7-max", Some(1000000), true, false, false),
    ("qwen3.7-plus", Some(1000000), true, false, false),
    ("qwen3.8-flash", Some(1000000), true, false, false),
    ("qwen3.8-max", Some(1000000), true, false, false),
];

fn opencode_zen_metadata_tables(
    catalog: OpenCodeZenCatalog,
) -> (
    &'static [OpenCodeZenModelRow],
    &'static [OpenCodeZenModelRow],
) {
    match catalog {
        OpenCodeZenCatalog::Full => (OPENCODE_ZEN_MODEL_METADATA, OPENCODE_GO_MODEL_METADATA),
        OpenCodeZenCatalog::Go => (OPENCODE_GO_MODEL_METADATA, OPENCODE_ZEN_MODEL_METADATA),
    }
}

/// Looks up `model` in `catalog`'s own table first (exact id match — a `-free`
/// suffix is a different row with its own numbers, not an alias); falls back
/// to the other catalog's table only for ids this catalog does not carry at
/// all (for example `hy3` on `Full`), never overriding a present-but-different
/// entry with the other table's numbers.
fn opencode_zen_metadata_row(
    catalog: OpenCodeZenCatalog,
    model: &str,
) -> Option<&'static OpenCodeZenModelRow> {
    let (primary, secondary) = opencode_zen_metadata_tables(catalog);
    primary
        .iter()
        .find(|(id, ..)| id.eq_ignore_ascii_case(model))
        .or_else(|| {
            secondary
                .iter()
                .find(|(id, ..)| id.eq_ignore_ascii_case(model))
        })
}

fn opencode_zen_known_context_window(catalog: OpenCodeZenCatalog, model: &str) -> Option<u64> {
    opencode_zen_metadata_row(catalog, model).and_then(|(_, context_window, ..)| *context_window)
}

fn opencode_zen_known_reasoning(catalog: OpenCodeZenCatalog, model: &str) -> bool {
    opencode_zen_metadata_row(catalog, model).is_some_and(|(_, _, reasoning, ..)| *reasoning)
}

fn opencode_zen_known_deprecated(catalog: OpenCodeZenCatalog, model: &str) -> bool {
    opencode_zen_metadata_row(catalog, model).is_some_and(|(_, _, _, _, deprecated)| *deprecated)
}

pub fn opencode_zen_known_free(catalog: OpenCodeZenCatalog, model: &str) -> Option<bool> {
    if let Some((_, _, _, free, _)) = opencode_zen_metadata_row(catalog, model) {
        return Some(*free);
    }
    // Unknown ids are not guessed from a `-free` suffix.
    match catalog {
        OpenCodeZenCatalog::Full => {
            vellum_proxy_runtime::opencode::confirmed_free_zen_model(model).then_some(true)
        }
        OpenCodeZenCatalog::Go => Some(false),
    }
}

/// Whether a model is actually usable on the free tier *today*: costs
/// nothing **and** has not been retired by the provider.
///
/// The two are independent. models.dev still reports
/// `deepseek-v4-flash-free` at `cost: {input: 0, output: 0}` while marking it
/// `status: deprecated` -- the price of a withdrawn model does not change. A
/// `freeOnly` catalog that reads only the price keeps offering it.
pub fn opencode_zen_free_tier_model(catalog: OpenCodeZenCatalog, model: &str) -> bool {
    opencode_zen_known_free(catalog, model).unwrap_or(false)
        && !opencode_zen_known_deprecated(catalog, model)
}

fn opencode_zen_access_mode(catalog: OpenCodeZenCatalog, model: &str) -> crate::model::AccessMode {
    match catalog {
        OpenCodeZenCatalog::Full
            if vellum_proxy_runtime::opencode::confirmed_free_zen_model(model) =>
        {
            crate::model::AccessMode::AnonymousFree
        }
        _ => crate::model::AccessMode::Credentialed,
    }
}

/// Ordering used both to pick verification candidates and to sort the catalog
/// for display: free-and-current models first, then free-but-deprecated, then
/// paid-and-current, then paid-and-deprecated — each group in catalog order
/// (a stable sort). Models Vellum cannot route at all (`opencode_zen_wire_for_model`
/// returns `None`) always sort last, since no amount of preference helps if the
/// endpoint cannot be reached.
fn opencode_zen_display_rank(catalog: OpenCodeZenCatalog, model: &str) -> (bool, bool, bool) {
    let unsupported = opencode_zen_wire_for_model(model).is_none();
    let free = opencode_zen_known_free(catalog, model).unwrap_or(false);
    let deprecated = opencode_zen_known_deprecated(catalog, model);
    (unsupported, !free, deprecated)
}

/// Ordered candidates for the one paid-credit-free verification request
/// Vellum makes when a Zen account is connected. Free, non-deprecated models
/// come first, in catalog order, so connecting an account never unexpectedly
/// consumes paid credits and never leads with a model OpenCode itself has
/// retired; every other protocol-supported model follows so the connection
/// still succeeds if none of the free models pass the Codex typed-tool-call
/// probe (some free/coding models genuinely do not support function calling).
fn opencode_zen_probe_candidates(catalog: OpenCodeZenCatalog, models: &[String]) -> Vec<&str> {
    let mut candidates: Vec<&str> = models
        .iter()
        .filter(|model| opencode_zen_wire_for_model(model).is_some())
        .map(String::as_str)
        .collect();
    candidates.sort_by_key(|model| opencode_zen_display_rank(catalog, model));
    candidates
}

/// Upper bound on how many candidates `probe_opencode_zen_catalog` will
/// actually probe. Every wire-supported candidate is probed (not just one —
/// see the comment at its call site), so this exists only to protect against
/// an unexpectedly huge catalog; both known Zen catalogs (Full: 61, Go: 25 as
/// of this writing) fit comfortably under it.
const OPENCODE_ZEN_PROBE_CANDIDATE_LIMIT: usize = 96;

fn ollama_native_root(base_url: &str) -> Option<String> {
    let root = base_url.trim().trim_end_matches('/');
    let lower = root.to_ascii_lowercase();
    OLLAMA_NATIVE_SUFFIXES.into_iter().find_map(|suffix| {
        lower
            .ends_with(suffix)
            .then(|| root[..root.len() - suffix.len()].to_string())
    })
}

/// 內建的已知模型上下文視窗（doc/03 第 3 層）。
/// 第一個匹配的條目勝出；未命中回 None → needs_input。
pub fn known_context_window(model: &str) -> Option<u64> {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("grok-4") || lower == "grok-4.5" {
        Some(500_000)
    } else if lower.starts_with("grok-3") {
        Some(131_072)
    } else if lower.starts_with("gpt-5")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
        || lower.starts_with("claude")
    {
        Some(200_000)
    } else {
        None
    }
}

/// Conservative fallback for model families whose public Chat protocol is
/// known to expose a reasoning channel. The live response remains the primary
/// signal; this prevents a one-token probe from marking reasoning as
/// unsupported merely because the sampled response ended before emitting a
/// reasoning delta.
pub fn known_reasoning_capability(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.starts_with("glm-5")
        || lower.starts_with("deepseek-v")
        || lower.starts_with("deepseek-r")
        || lower.starts_with("nemotron-3")
        || lower.starts_with("grok-")
}

fn provider_has_known_reasoning_model(models: &[String]) -> bool {
    models.iter().any(|model| known_reasoning_capability(model))
}

/// 構造給 `/responses` 的極小試探請求 body（純函式，可單測）。
pub fn build_responses_body(prompt: &str, model: &str) -> Value {
    json!({
        "model": model,
        "input": prompt,
        "max_output_tokens": PROBE_MAX_TOKENS,
        "stream": false,
        "store": false,
    })
}

/// 構造給 `/chat/completions` 的極小試探請求 body（純函式，可單測）。
pub fn build_chat_body(prompt: &str, model: &str) -> Value {
    let prompt = if prompt.trim().is_empty() {
        PROBE_USER_MESSAGE
    } else {
        prompt
    };
    json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        "max_tokens": PROBE_MAX_TOKENS,
        "stream": false,
    })
}

pub fn build_responses_harness_body(model: &str) -> Value {
    json!({
        "model": model,
        "input": [
            {
                "type": "additional_tools",
                "tools": [{
                    "type": "function",
                    "name": "vellum_probe_tool",
                    "description": "Capability probe; do not call.",
                    "parameters": {
                        "type": "object",
                        "properties": {"value": {"type": "string"}},
                        "required": ["value"]
                    }
                }]
            },
            {
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "Reply with one character. Do not call tools."
                }]
            }
        ],
        "tools": [{
            "type": "function",
            "name": "vellum_top_level_probe",
            "description": "Capability probe; do not call.",
            "parameters": {"type": "object", "properties": {}}
        }],
        "max_output_tokens": PROBE_MAX_TOKENS,
        "stream": false,
        "store": false
    })
}

fn is_nvidia_nim_endpoint(v1: &str) -> bool {
    v1.to_ascii_lowercase().contains("integrate.api.nvidia.com")
}

fn is_nvidia_reasoning_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains("nemotron") || lower.contains("reasoning")
}

/// NVIDIA NIM exposes OpenAI-compatible Chat Completions, but reasoning models
/// require chat-template options before they emit the separate
/// `reasoning_content` channel. Keep those options scoped to NVIDIA reasoning
/// models so generic OpenAI-compatible providers never receive unknown fields.
pub(crate) fn apply_chat_probe_options(v1: &str, model: &str, body: &mut Value) {
    if !is_nvidia_nim_endpoint(v1) || !is_nvidia_reasoning_model(model) {
        return;
    }
    body["temperature"] = json!(1);
    body["top_p"] = json!(0.95);
    body["chat_template_kwargs"] = json!({ "enable_thinking": true });
    body["reasoning_budget"] = json!(PROBE_MAX_TOKENS);
}

/// 從狀態碼推斷 wire 格式。
/// - 2xx：該 wire 就是支援的
/// - 404 / 405：該路徑不存在 → 換另一種
/// - 400 / 422：路徑在但 body 不對 → 仍視為該 wire 存在（端點認得路徑）
pub fn infer_wire_from_status(status: u16, tried: WireFormat) -> Option<WireFormat> {
    if (200..300).contains(&status) {
        return Some(tried);
    }
    match status {
        404 | 405 => None,
        400 | 422 => Some(tried), // 路徑認得，只是 body 規格不同
        _ => None,
    }
}

/// 從回應 JSON 萃取 reasoning 能力（純函式）。
/// Responses wire：`output[].type == "reasoning"`。
/// Chat wire：`choices[].message.reasoning` 或 `reasoning_content`。
pub fn extract_reasoning(body: &Value) -> bool {
    if let Some(output) = body.get("output").and_then(Value::as_array) {
        return output
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"));
    }
    if let Some(choices) = body.get("choices").and_then(Value::as_array) {
        return choices.iter().any(|c| {
            let msg = c.get("message");
            msg.and_then(|m| m.get("reasoning")).is_some()
                || msg.and_then(|m| m.get("reasoning_content")).is_some()
        });
    }
    false
}

/// 從 `/models` 回應萃取模型清單（純函式）。
pub fn extract_models(body: &Value) -> Vec<String> {
    let mut models: Vec<String> = body
        .get("data")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    models.retain(|model| seen.insert(model.to_ascii_lowercase()));
    models
}

/// 從 `/models` 單一模型項目萃取 context_window（純函式）。
/// 不同端點用的欄位名不同，都試一遍。
pub fn extract_model_context_window(model_obj: &Value) -> Option<u64> {
    for key in [
        "context_window",
        "max_context_length",
        "context_length",
        "max_tokens",
    ] {
        if let Some(n) = model_obj
            .get(key)
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
        {
            return Some(n);
        }
    }
    for pointer in [
        "/meta/n_ctx",
        "/meta/context_length",
        "/details/context_length",
    ] {
        if let Some(n) = model_obj
            .pointer(pointer)
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
        {
            return Some(n);
        }
    }
    None
}

/// 依優先序合併 context window 來源（doc/03：供應商快取 > /models > 已知表 > None）。
pub fn merge_context_window(
    models_cache: Option<u64>,
    from_models: Option<u64>,
    known: Option<u64>,
) -> Option<u64> {
    models_cache
        .filter(|window| *window > 0)
        .or_else(|| from_models.filter(|window| *window > 0))
        .or_else(|| known.filter(|window| *window > 0))
}

/// 判定 content-type 是不是 SSE（純函式）。
pub fn is_sse_content_type(ct: Option<&str>) -> bool {
    ct.map(|s| s.contains("text/event-stream")).unwrap_or(false)
}

/// 探測結果。接線層把 reqwest 的回應轉成這個，再交給純函式判定。
struct ProbeOutcome {
    wire: Option<WireFormat>,
    /// `None` means the streaming probe timed out or never completed. That is
    /// not evidence the model lacks SSE.
    streaming: Option<bool>,
    reasoning: bool,
    store: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamingProbe {
    Supported,
    Unsupported,
    Timeout,
}

struct ModelsOutcome {
    models: Vec<String>,
    context_window: Option<u64>,
    context_windows: HashMap<String, u64>,
    effort_metadata: HashMap<String, EffortMetadata>,
}

#[derive(Debug, Clone, Default)]
struct EffortMetadata {
    levels: Vec<String>,
    default: Option<String>,
}

/// Canonical strength order for the Effort levels Vellum itself knows about.
/// Used only to order the *hardcoded candidate list* this probe tries when a
/// provider's `/models` response did not already declare its own level
/// vocabulary/order (`EffortMetadata::levels`) — a provider-declared order is
/// always kept as the provider's catalog listed it, never re-sorted against
/// this list.
const EFFORT_CANONICAL_ORDER: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh"];

/// A value no real provider is expected to accept as an Effort level. Sent
/// alongside the real candidates as a negative control: a provider that
/// validates the field rejects this and every genuinely unsupported real
/// candidate the same way, so a clean rejection here is what license the
/// probe to trust an "accepted" verdict on any real candidate. A provider
/// that instead returns 2xx for this proves it ignores the field outright —
/// accepting every real candidate too would otherwise be misread as
/// supporting all six levels.
const EFFORT_NEGATIVE_CONTROL: &str = "__vellum_probe_invalid_effort__";

/// Prompt for the Effort *behavioral* tiebreaker. Unlike the 1-token status
/// probes above, this needs enough headroom for the model's own reasoning
/// trace to actually diverge across Effort levels, so it carries real content
/// instead of a single `.`.
const EFFORT_BEHAVIOR_PROMPT: &str = "Say hi in one short sentence.";
/// Generous enough to capture a differentiated reasoning trace, bounded so a
/// slow local backend does not turn this into a multi-minute probe.
const EFFORT_BEHAVIOR_MAX_TOKENS: u32 = 128;
/// Low but nonzero — some providers reject `temperature: 0`. Paired with a
/// fixed `seed`, this keeps generation close enough to deterministic that two
/// requests differing only in the Effort value produce comparable output.
const EFFORT_BEHAVIOR_TEMPERATURE: f64 = 0.2;
const EFFORT_BEHAVIOR_SEED: u64 = 20260821;

/// Outcome of sending one Effort-level value to the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffortCandidateOutcome {
    /// A clean 2xx: the provider accepted this value.
    Accepted,
    /// A clean 4xx other than 401/403/429, or an explicit template validation
    /// error from a backend that incorrectly wraps it in HTTP 500: the
    /// provider validated and refused this value.
    Rejected,
    /// A network error, timeout, 401/403/429, or an ordinary 5xx. Proves
    /// nothing either way about this specific value.
    Inconclusive,
}

/// llama.cpp's Jinja exception is surfaced by some OpenAI-compatible proxies
/// as HTTP 500 even though the body is a deterministic field-validation
/// result. Provider 806 Qwen is one observed example:
/// `Unexpected reasoning effort high. Supported types are xhigh, medium, low`.
/// Only recognize narrow, self-describing validation messages; an unrelated
/// server failure that happens to mention Effort must remain inconclusive.
fn is_explicit_effort_validation_error(body: &Value) -> bool {
    let message = body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| body.get("message").and_then(Value::as_str))
        .unwrap_or_default()
        .to_ascii_lowercase();
    message.contains("unexpected reasoning effort")
        && (message.contains("supported types") || message.contains("supported values"))
        || message.contains("invalid reasoning_effort")
            && (message.contains("supported")
                || message.contains("allowed")
                || message.contains("one of"))
}

fn classify_effort_candidate(
    result: &AppResult<(u16, Value, Option<String>)>,
) -> EffortCandidateOutcome {
    match result {
        Ok((status, _, _)) if (200..300).contains(status) => EffortCandidateOutcome::Accepted,
        Ok((status, _, _)) if *status == 429 || *status == 401 || *status == 403 => {
            EffortCandidateOutcome::Inconclusive
        }
        Ok((status, body, _)) if *status >= 500 => {
            if is_explicit_effort_validation_error(body) {
                EffortCandidateOutcome::Rejected
            } else {
                EffortCandidateOutcome::Inconclusive
            }
        }
        Ok(_) => EffortCandidateOutcome::Rejected,
        Err(_) => EffortCandidateOutcome::Inconclusive,
    }
}

/// Result of one full Effort probe attempt for a model (all transports).
#[derive(Debug, Clone, PartialEq, Eq)]
enum EffortProbeOutcome {
    /// Reasoning/tool-calling was not verified, so Effort control is moot.
    NotApplicable,
    Supported {
        levels: Vec<String>,
        default: Option<String>,
        transport: ReasoningEffortTransport,
    },
    Unsupported,
    /// Ran but could not reach a reliable verdict. Carries a machine-readable
    /// reason for `ModelCapability::effort_probe_issue`.
    Indeterminate(&'static str),
}

fn effort_probe_diagnostic(issue: &str) -> &'static str {
    match issue {
        "provider_ignores_unknown_effort" => {
            "provider accepted the deliberately invalid Effort value, so supported Effort levels cannot be verified; repeating the same probe is unlikely to change this result"
        }
        "provider_quota_or_access" => {
            "Effort probe was blocked by provider quota or account access policy"
        }
        "provider_error" => {
            "one or more Effort requests failed, timed out, or returned inconclusive responses"
        }
        _ => "Effort support could not be verified",
    }
}

/// 用注入的 client 探測一個端點。
///
/// `base_url` 應為根（不含 `/v1`）；這裡會自己拼 `/v1/models`、`/v1/responses`、
/// `/v1/chat/completions`。`api_key` 可空（有些本地端點不需要）。
pub async fn probe_endpoint_with_client(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
) -> AppResult<ProbeResult> {
    let ollama_root = ollama_native_root(base_url);
    let v1 = normalize_provider_base_url(base_url);
    let catalog_token = opencode_catalog_token(&v1, api_key)?;

    let mut needs_input = Vec::new();

    // 1. GET /models —— 可連線 + 模型清單。
    let (reachable, models, models_context_window, models_context_windows, effort_metadata) =
        match if let Some(root) = ollama_root.as_deref() {
            get_ollama_models(client, root, api_key).await
        } else {
            get_models(client, &v1, catalog_token.or(api_key)).await
        } {
            Ok(outcome) => (
                true,
                outcome.models,
                outcome.context_window,
                outcome.context_windows,
                outcome.effort_metadata,
            ),
            Err(_) => (false, Vec::new(), None, HashMap::new(), HashMap::new()),
        };

    if !reachable {
        return Ok(ProbeResult {
            reachable: false,
            wire: None,
            models,
            context_window: None,
            streaming: false,
            reasoning: false,
            server_side_resume: false,
            model_capabilities: Vec::new(),
            stream_quality: None,
            needs_input: vec!["endpoint".to_string()],
        });
    }

    if let Some(catalog) = opencode_zen_catalog(&v1) {
        let api_key = match catalog {
            OpenCodeZenCatalog::Go => api_key
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| AppError::Message("OpenCode Go requires an API key".into()))?
                .to_string(),
            OpenCodeZenCatalog::Full => api_key
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(vellum_proxy_runtime::OPENCODE_PUBLIC_TOKEN)
                .to_string(),
        };
        return probe_opencode_zen_catalog(
            client,
            &v1,
            catalog,
            Some(&api_key),
            models,
            models_context_window,
            models_context_windows,
            effort_metadata,
        )
        .await;
    }

    // 2. 試 POST /responses，失敗退 /chat/completions。
    let representative = models
        .iter()
        .find(|model| known_reasoning_capability(model))
        .or_else(|| models.first())
        .map(String::as_str)
        .unwrap_or("probe");
    let outcome = try_responses_then_chat(client, &v1, api_key, representative).await;
    let wire = outcome.wire;
    // Unknown (timeout) must not persist as `false` — that disabled Codex
    // for models that only needed a longer cold-start TTFB.
    let streaming = outcome.streaming.unwrap_or(true);
    // The current route schema stores provider-level capabilities while
    // `/models` can list heterogeneous models. Do not let the provider's first
    // model (for example MiMo) hide reasoning support offered by a selected
    // GLM/DeepSeek/Nemotron model later in the same catalog.
    let reasoning = outcome.reasoning || provider_has_known_reasoning_model(&models);
    // store=false 或缺 → 要本地補歷史（doc/05）。
    let server_side_resume = outcome.store.unwrap_or(false);
    if wire.is_none() {
        needs_input.push("wire".to_string());
    }

    // 3. context window：/models 欄位 > 已知表。
    let first_model = models.first();
    // /models 的 context_window 欄位多半不在清單裡（供應商快取才有），
    // 先留空；真正的來源交給已知表（known_context_window）。
    let known = first_model.and_then(|m| known_context_window(m));
    let context_window = merge_context_window(None, models_context_window, known);
    if context_window.is_none() {
        needs_input.push("contextWindow".to_string());
    }

    // Probe each discovered model independently. Providers often expose a
    // heterogeneous catalog (for example MiMo + GLM + Nemotron) where the
    // first model's SSE/reasoning behaviour is not representative. Keep a
    // small concurrency limit to avoid a request burst against hosted APIs.
    let model_capabilities = if let Some(wire) = wire {
        let mut candidates = models.clone();
        // Probe known reasoning/coding families first so large marketplace
        // catalogs still verify the models users are most likely to expose.
        candidates.sort_by_key(|model| !known_reasoning_capability(model));
        candidates.truncate(MAX_MODEL_CAPABILITY_PROBES);
        let probed = stream::iter(candidates.into_iter().map(|model| {
            let context_window = models_context_windows.get(&model).copied();
            let effort_metadata = effort_metadata
                .get(&model.to_ascii_lowercase())
                .cloned()
                .unwrap_or_default();
            let endpoint = v1.clone();
            async move {
                probe_model_capability_with_wire_fallback(
                    client,
                    &endpoint,
                    api_key,
                    model,
                    wire,
                    context_window,
                    effort_metadata,
                    &RuntimeChatCapabilities::default(),
                )
                .await
            }
        }))
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;
        let probed = probed
            .into_iter()
            .map(|capability| (capability.model.to_ascii_lowercase(), capability))
            .collect::<HashMap<_, _>>();
        models
            .iter()
            .map(|model| {
                probed
                    .get(&model.to_ascii_lowercase())
                    .cloned()
                    .unwrap_or_else(|| ModelCapability {
                        model: model.clone(),
                        context_window: models_context_windows.get(model).copied(),
                        wire: None,
                        streaming: None,
                        reasoning: known_reasoning_capability(model).then_some(true),
                        probe_version: None,
                        ..Default::default()
                    })
            })
            .collect()
    } else {
        let mut candidates = models.clone();
        candidates.sort_by_key(|model| !known_reasoning_capability(model));
        candidates.truncate(MAX_MODEL_CAPABILITY_PROBES);
        let probed = stream::iter(candidates.into_iter().map(|model| {
            let context_window = models_context_windows.get(&model).copied();
            let endpoint = v1.clone();
            async move {
                probe_model_capability_with_wire_fallback(
                    client,
                    &endpoint,
                    api_key,
                    model,
                    WireFormat::Responses,
                    context_window,
                    EffortMetadata::default(),
                    &RuntimeChatCapabilities::default(),
                )
                .await
            }
        }))
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;
        let probed = probed
            .into_iter()
            .map(|capability| (capability.model.to_ascii_lowercase(), capability))
            .collect::<HashMap<_, _>>();
        models
            .iter()
            .map(|model| {
                probed
                    .get(&model.to_ascii_lowercase())
                    .cloned()
                    .unwrap_or_else(|| ModelCapability {
                        model: model.clone(),
                        context_window: models_context_windows.get(model).copied(),
                        wire: None,
                        streaming: None,
                        reasoning: known_reasoning_capability(model).then_some(true),
                        probe_version: None,
                        ..Default::default()
                    })
            })
            .collect()
    };

    Ok(ProbeResult {
        reachable,
        wire,
        models,
        context_window,
        streaming,
        reasoning,
        server_side_resume,
        model_capabilities,
        stream_quality: None,
        needs_input,
    })
}

/// Discover an endpoint and its catalog without running inference.
///
/// This is deliberately separate from capability verification. Listing the
/// catalog is normally sub-second, while probing every model can issue dozens
/// of paid inference requests and wait for cold local runtimes. The UI can now
/// present the catalog first and verify only models the user elects to import.
pub async fn discover_endpoint_with_client(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
) -> AppResult<ProbeResult> {
    let ollama_root = ollama_native_root(base_url);
    let v1 = normalize_provider_base_url(base_url);
    let outcome = if let Some(root) = ollama_root.as_deref() {
        get_ollama_models(client, root, api_key).await
    } else {
        get_models(client, &v1, api_key).await
    }?;

    let zen_catalog = opencode_zen_catalog(&v1);
    let model_capabilities = outcome
        .models
        .iter()
        .map(|model| {
            let catalog_wire = zen_catalog.and_then(|_| opencode_zen_wire_for_model(model));
            ModelCapability {
                model: model.clone(),
                context_window: outcome
                    .context_windows
                    .get(model)
                    .copied()
                    .or_else(|| {
                        zen_catalog
                            .and_then(|catalog| opencode_zen_known_context_window(catalog, model))
                    })
                    .or_else(|| known_context_window(model)),
                wire: catalog_wire,
                streaming: None,
                reasoning: (known_reasoning_capability(model)
                    || zen_catalog
                        .is_some_and(|catalog| opencode_zen_known_reasoning(catalog, model))
                    || outcome
                        .effort_metadata
                        .get(&model.to_ascii_lowercase())
                        .is_some_and(|metadata| !metadata.levels.is_empty()))
                .then_some(true),
                // OpenCode Zen publishes its own per-model wire map; a Zen
                // model absent from it genuinely does not support any wire
                // Vellum speaks, so it is pre-marked incompatible before the
                // user ever clicks it. A non-Zen custom provider has no such
                // catalog-level signal at discovery time — its wire is
                // determined later, per model, by the live verify probe
                // (`probe_model_capability_with_wire_fallback`). Marking
                // `tool_calling: false` here unconditionally (as opposed to
                // only when there is an actual Zen catalog to consult) used
                // to disable every model's checkbox on every ordinary
                // OpenAI-compatible endpoint before verification could ever
                // run, since `catalog_wire` is trivially always `None` when
                // `zen_catalog` is `None`.
                tool_calling: (zen_catalog.is_some() && catalog_wire.is_none()).then_some(false),
                probe_version: None,
                free: zen_catalog.and_then(|catalog| opencode_zen_known_free(catalog, model)),
                access_mode: zen_catalog.map(|catalog| opencode_zen_access_mode(catalog, model)),
                display_name: zen_catalog
                    .and_then(|_| {
                        vellum_proxy_runtime::opencode::confirmed_zen_model_display_name(model)
                    })
                    .map(str::to_string),
                ..Default::default()
            }
        })
        .collect();
    let context_window = merge_context_window(
        None,
        outcome.context_window,
        outcome
            .models
            .iter()
            .find_map(|model| outcome.context_windows.get(model).copied())
            .or_else(|| {
                outcome
                    .models
                    .iter()
                    .find_map(|model| known_context_window(model))
            }),
    );

    Ok(ProbeResult {
        reachable: true,
        wire: None,
        models: outcome.models,
        context_window,
        streaming: false,
        reasoning: false,
        server_side_resume: false,
        model_capabilities,
        stream_quality: None,
        needs_input: vec!["model".to_string()],
    })
}

/// Bounded so a route with many selected models still probes politely
/// against a hosted API, matching the concurrency the catalog-wide probe
/// already used per batch.
const REPROBE_SELECTED_MODELS_CONCURRENCY: usize = 4;

/// Refresh a route's `/models` catalog, then live-verify only `selected` —
/// the models a route actually exposes, not every model a marketplace
/// endpoint happens to list. A catalog can carry dozens of models (OpenCode
/// Zen: 60+) that a route never selects; scoping live verification (and
/// therefore inference cost) to the selection is what keeps a re-probe
/// affordable regardless of catalog size, and is the reusable core behind
/// the `reprobe_route_capabilities` command.
///
/// Returns the catalog discovery (for merging into a route, and so callers
/// can inspect the fresh model list/context window), the models that were
/// successfully live-verified this round, and a report a caller can surface
/// directly. Only a catalog-refresh failure (unreachable endpoint, bad
/// credential) is `Err`; a single targeted model's own probe failing is
/// reflected in the report, not propagated as an error.
///
/// `existing_capabilities` is the route's own `model_capabilities` going
/// into this round — each targeted model's prior `chat_capabilities` is
/// migrated forward the same way the single-model re-probe command already
/// does, so a Provider-level re-probe cannot regress a previously-verified
/// tail/reasoning-replay observation back to unknown just because it went
/// through the whole-route path instead.
pub async fn reprobe_selected_models_with_client(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    selected: &[String],
    existing_capabilities: &[ModelCapability],
    route_wire: WireFormat,
) -> AppResult<(ProbeResult, Vec<ModelCapability>, RouteReprobeReport)> {
    let discovery = discover_endpoint_with_client(client, base_url, api_key).await?;
    if !discovery.reachable {
        return Err(AppError::Message(
            "provider capability probe could not reach the endpoint".into(),
        ));
    }

    let discovered_lower: HashSet<String> = discovery
        .models
        .iter()
        .map(|model| model.to_ascii_lowercase())
        .collect();
    let targeted: Vec<String> = selected
        .iter()
        .filter(|model| discovered_lower.contains(&model.to_ascii_lowercase()))
        .cloned()
        .collect();
    let skipped = selected.len().saturating_sub(targeted.len());

    let probed = stream::iter(targeted.iter().cloned().map(|model| {
        let client = client.clone();
        let base_url = base_url.to_string();
        let existing = existing_capabilities
            .iter()
            .find(|capability| capability.model.eq_ignore_ascii_case(&model))
            .map(|capability| {
                capability.chat_capabilities.clone().migrate_legacy(
                    matches!(capability.wire, Some(WireFormat::Chat))
                        || route_wire == WireFormat::Chat,
                    capability.reasoning.unwrap_or(false),
                    capability.probe_version,
                )
            })
            .unwrap_or_default();
        async move {
            let result = probe_model_capability_with_client_and_existing(
                &client, &base_url, api_key, &model, &existing,
            )
            .await;
            (model, result)
        }
    }))
    .buffer_unordered(REPROBE_SELECTED_MODELS_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut verified = Vec::new();
    let mut errors = Vec::new();
    for (model, result) in probed {
        match result {
            Ok(capability) => {
                if capability.tool_calling == Some(true) {
                    verified.push(capability);
                } else {
                    let (stage, outcome, status, timeout, retry_after, msg) =
                        if let Some(last_failed) = capability
                            .probe_attempts
                            .iter()
                            .rev()
                            .find(|a| a.outcome != "success")
                        {
                            (
                                Some(last_failed.stage.clone()),
                                Some(last_failed.outcome.clone()),
                                last_failed.status,
                                last_failed.timeout,
                                last_failed.retry_after,
                                last_failed.message.clone().unwrap_or_else(|| {
                                    format!("{}: {}", last_failed.stage, last_failed.outcome)
                                }),
                            )
                        } else {
                            (
                                Some("typedTool".into()),
                                capability.probe_issue.clone(),
                                None,
                                false,
                                None,
                                capability.probe_issue.clone().unwrap_or_else(|| {
                                    "did not pass typed-tool capability probe".into()
                                }),
                            )
                        };
                    errors.push(ModelProbeError {
                        model: model.clone(),
                        message: msg,
                        stage,
                        outcome,
                        status,
                        timeout,
                        retry_after,
                    });
                    verified.push(capability);
                }
            }
            Err(error) => errors.push(ModelProbeError {
                model,
                message: error.to_string(),
                stage: Some("catalog".into()),
                outcome: Some("unsupported_protocol".into()),
                status: None,
                timeout: false,
                retry_after: None,
            }),
        }
    }
    let report = RouteReprobeReport {
        discovered: discovery.models.len() as u32,
        targeted: targeted.len() as u32,
        succeeded: verified
            .iter()
            .filter(|c| c.tool_calling == Some(true))
            .count() as u32,
        failed: errors.len() as u32,
        skipped: skipped as u32,
        errors,
    };
    Ok((discovery, verified, report))
}

#[allow(clippy::too_many_arguments)]
async fn probe_opencode_zen_catalog(
    client: &reqwest::Client,
    v1: &str,
    catalog: OpenCodeZenCatalog,
    api_key: Option<&str>,
    models: Vec<String>,
    models_context_window: Option<u64>,
    models_context_windows: HashMap<String, u64>,
    effort_metadata: HashMap<String, EffortMetadata>,
) -> AppResult<ProbeResult> {
    let _ = (
        client,
        v1,
        api_key,
        effort_metadata,
        OPENCODE_ZEN_PROBE_CANDIDATE_LIMIT,
    );
    // Connection reads the catalog only. Selected-model verification is a
    // separate, at-most-once streaming/tool-call probe.
    let representative = opencode_zen_probe_candidates(catalog, &models)
        .into_iter()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| {
            AppError::Message(
                "OpenCode catalog has no models Vellum can speak (Chat Completions or Responses)"
                    .into(),
            )
        })?;

    // Free-and-current models first (matching OpenCode's own model picker),
    // then free-but-deprecated, then paid, with protocol-unsupported models
    // last — see `opencode_zen_display_rank`. `models` and `model_capabilities`
    // are sorted together so every consumer (the detected-models list during
    // Add Provider, and the model catalog ledger once the route exists) shows
    // the same order without needing its own sort.
    let mut models = models;
    models.sort_by_key(|model| opencode_zen_display_rank(catalog, model));

    let model_capabilities = models
        .iter()
        .map(|model| {
            let wire = opencode_zen_wire_for_model(model);
            ModelCapability {
                model: model.clone(),
                context_window: models_context_windows
                    .get(model)
                    .copied()
                    .or_else(|| opencode_zen_known_context_window(catalog, model))
                    .or_else(|| known_context_window(model)),
                wire,
                streaming: wire.is_some().then_some(true),
                reasoning: (opencode_zen_known_reasoning(catalog, model)
                    || known_reasoning_capability(model))
                .then_some(true),
                tool_calling: wire.is_none().then_some(false),
                probe_version: None,
                free: opencode_zen_known_free(catalog, model),
                deprecated: Some(opencode_zen_known_deprecated(catalog, model)),
                access_mode: Some(opencode_zen_access_mode(catalog, model)),
                display_name: vellum_proxy_runtime::opencode::confirmed_zen_model_display_name(
                    model,
                )
                .map(str::to_string),
                ..Default::default()
            }
        })
        .collect::<Vec<_>>();

    let context_window = merge_context_window(
        None,
        models_context_window,
        models.iter().find_map(|model| {
            opencode_zen_known_context_window(catalog, model)
                .or_else(|| known_context_window(model))
        }),
    );
    let wire = opencode_zen_wire_for_model(&representative);
    let streaming = wire.is_some();
    let reasoning = models
        .iter()
        .any(|model| opencode_zen_known_reasoning(catalog, model))
        || provider_has_known_reasoning_model(&models);
    let mut needs_input = Vec::new();
    if wire.is_none() {
        needs_input.push("wire".to_string());
    }
    if context_window.is_none() {
        needs_input.push("contextWindow".to_string());
    }

    Ok(ProbeResult {
        reachable: true,
        wire,
        models,
        context_window,
        streaming,
        reasoning,
        server_side_resume: false,
        model_capabilities,
        stream_quality: None,
        needs_input,
    })
}

/// Probe selected OpenCode models at most once each. Free models run with
/// concurrency 1. The first 429 stops the rest.
pub async fn probe_opencode_selected_models(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    models: &[String],
) -> AppResult<Vec<ModelCapability>> {
    let v1 = normalize_provider_base_url(base_url);
    let catalog = opencode_zen_catalog(&v1).unwrap_or(OpenCodeZenCatalog::Full);
    let mut results = Vec::new();
    for model in models {
        let wire = opencode_zen_wire_for_model(model).ok_or_else(|| {
            AppError::Message(format!(
                "OpenCode model '{model}' uses a protocol Vellum does not support yet"
            ))
        })?;
        let token = opencode_model_probe_token(&v1, model, api_key)?;
        let capability = probe_model_capability(
            client,
            &v1,
            token,
            model.clone(),
            wire,
            opencode_zen_known_context_window(catalog, model)
                .or_else(|| known_context_window(model)),
            EffortMetadata::default(),
            &RuntimeChatCapabilities::default(),
        )
        .await;
        let quota = capability.probe_issue.as_deref() == Some(PROBE_ISSUE_PROVIDER_QUOTA);
        results.push(capability);
        if quota {
            break;
        }
        let _ = catalog;
    }
    Ok(results)
}

/// Strictly verify one model without re-running paid probes for an entire
/// marketplace catalog. OpenCode Zen uses this from the model picker; generic
/// OpenAI-compatible endpoints can use it as a focused diagnostic as well.
fn opencode_catalog_token<'a>(v1: &str, api_key: Option<&'a str>) -> AppResult<Option<&'a str>> {
    match opencode_zen_catalog(v1) {
        Some(OpenCodeZenCatalog::Go) => api_key
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| AppError::Message("OpenCode Go requires an API key".into()))
            .map(Some),
        Some(OpenCodeZenCatalog::Full) => Ok(Some(
            api_key
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(vellum_proxy_runtime::OPENCODE_PUBLIC_TOKEN),
        )),
        None => Ok(api_key.filter(|value| !value.trim().is_empty())),
    }
}

fn opencode_model_probe_token<'a>(
    v1: &str,
    model: &str,
    api_key: Option<&'a str>,
) -> AppResult<Option<&'a str>> {
    match opencode_zen_catalog(v1) {
        Some(OpenCodeZenCatalog::Go) => api_key
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| AppError::Message("OpenCode Go requires an API key".into()))
            .map(Some),
        Some(OpenCodeZenCatalog::Full)
            if vellum_proxy_runtime::opencode::confirmed_free_zen_model(model) =>
        {
            Ok(Some(vellum_proxy_runtime::OPENCODE_PUBLIC_TOKEN))
        }
        Some(OpenCodeZenCatalog::Full) => api_key
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                AppError::Message(format!("OpenCode Zen model '{model}' requires an API key"))
            })
            .map(Some),
        None => Ok(api_key.filter(|value| !value.trim().is_empty())),
    }
}

pub async fn probe_model_capability_with_client(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
) -> AppResult<ModelCapability> {
    probe_model_capability_with_client_and_existing(
        client,
        base_url,
        api_key,
        model,
        &RuntimeChatCapabilities::default(),
    )
    .await
}

pub async fn probe_model_capability_with_client_and_existing(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
    existing_chat: &RuntimeChatCapabilities,
) -> AppResult<ModelCapability> {
    let v1 = normalize_provider_base_url(base_url);
    let catalog = opencode_zen_catalog(&v1);
    let api_key = opencode_model_probe_token(&v1, model, api_key)?;
    let catalog_started = std::time::Instant::now();
    let models_result = get_models(client, &v1, api_key).await;
    let catalog_attempt = match &models_result {
        Ok(_) => crate::model::ProbeAttempt {
            stage: "catalog".into(),
            outcome: "success".into(),
            duration_ms: catalog_started.elapsed().as_millis() as u64,
            ..Default::default()
        },
        Err(error) => crate::model::ProbeAttempt {
            stage: "catalog".into(),
            outcome: "provider_unavailable".into(),
            duration_ms: catalog_started.elapsed().as_millis() as u64,
            message: Some(bounded_probe_message(&error.to_string())),
            ..Default::default()
        },
    };
    let models = models_result.ok();
    let context_window = models
        .as_ref()
        .and_then(|outcome| outcome.context_windows.get(model).copied())
        .or_else(|| catalog.and_then(|catalog| opencode_zen_known_context_window(catalog, model)))
        .or_else(|| known_context_window(model));
    let effort_metadata = models
        .as_ref()
        .and_then(|outcome| outcome.effort_metadata.get(&model.to_ascii_lowercase()))
        .cloned()
        .unwrap_or_default();
    if catalog.is_some() {
        let wire = opencode_zen_wire_for_model(model).ok_or_else(|| {
            AppError::Message(format!(
                "OpenCode Zen model '{model}' uses an Anthropic or Google-native protocol that Vellum does not support yet"
            ))
        })?;
        let mut capability = probe_model_capability(
            client,
            &v1,
            api_key,
            model.to_string(),
            wire,
            context_window,
            effort_metadata,
            existing_chat,
        )
        .await;
        capability.probe_attempts.insert(0, catalog_attempt);
        return Ok(capability);
    }

    // Probe the selected model directly. The old path first ran a complete
    // Responses/Chat harness probe merely to choose a wire and then repeated
    // the same harness, streaming and effort requests to build the capability.
    // Trying each wire exactly once cuts the normal request count roughly in
    // half while retaining the safe Responses -> Chat fallback.
    let mut capability = probe_model_capability_with_wire_fallback(
        client,
        &v1,
        api_key,
        model.to_string(),
        WireFormat::Responses,
        context_window,
        effort_metadata,
        existing_chat,
    )
    .await;
    capability.probe_attempts.insert(0, catalog_attempt);
    // A negative capability result is still a successful diagnostic operation.
    // Returning `Err` here used to discard the structured attempts collected by
    // the probe, so generic endpoints (including Provider 806) could only show
    // the old opaque "did not pass" message. Callers decide whether the
    // capability verified; transport/setup failures before a capability can be
    // assembled remain real `Err`s above.
    Ok(capability)
}

async fn get_models(
    client: &reqwest::Client,
    v1: &str,
    api_key: Option<&str>,
) -> AppResult<ModelsOutcome> {
    let mut req = client
        .get(format!("{v1}/models"))
        .timeout(models_catalog_timeout(v1));
    if let Some(k) = api_key {
        req = req.bearer_auth(k);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| AppError::Unreachable(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(AppError::Message(format!(
            "模型清單探測失敗：HTTP {}",
            resp.status()
        )));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| AppError::Message(e.to_string()))?;
    models_outcome_from_entries(
        body.get("data")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default(),
    )
}

fn models_catalog_timeout(v1: &str) -> std::time::Duration {
    if opencode_zen_catalog(v1).is_some() {
        std::time::Duration::from_secs(OPENCODE_CATALOG_TIMEOUT_SECS)
    } else {
        reachability_timeout()
    }
}

fn build_responses_tool_call_body(model: &str, max_output_tokens: u32) -> Value {
    json!({
        "model": model,
        "input": "Call vellum_probe_tool with value exactly ok. Do not answer in text.",
        "tools": [{
            "type": "function",
            "name": "vellum_probe_tool",
            "description": "Required Codex tool protocol probe.",
            "parameters": {
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"]
            }
        }],
        // `required` is the portable OpenAI-compatible form. vLLM accepts it
        // for both Responses and Chat even when named tool-choice objects are
        // not enabled by the deployment's parser configuration.
        "tool_choice": "required",
        "max_output_tokens": max_output_tokens,
        "stream": false,
        "store": false
    })
}

fn build_chat_tool_call_body(model: &str, max_tokens: u32, tool_choice: &str) -> Value {
    json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": "Call vellum_probe_tool with value exactly ok. Do not answer in text."
        }],
        "tools": [{
            "type": "function",
            "function": {
                "name": "vellum_probe_tool",
                "description": "Required Codex tool protocol probe.",
                "parameters": {
                    "type": "object",
                    "properties": {"value": {"type": "string"}},
                    "required": ["value"]
                }
            }
        }],
        "tool_choice": tool_choice,
        "max_tokens": max_tokens,
        "stream": false
    })
}

/// Some Chat Completions gateways run reasoning ("thinking") mode by default
/// and reject a forced tool choice outright — verified live against
/// deepseek-v4-pro on opencode.ai/zen/go/v1, which answers `tool_choice:
/// "required"` with HTTP 400 `{"error":{"type":"invalid_request_error",
/// "message":"...Thinking mode does not support this tool_choice"}}`, then
/// calls the tool correctly and gets a normal 200 once asked with
/// `tool_choice: "auto"` instead. Matched on the error mentioning
/// `tool_choice` rather than this one gateway's exact wording, since the same
/// policy plausibly exists elsewhere.
fn chat_tool_choice_rejected(status: u16, body: &Value) -> bool {
    (400..500).contains(&status)
        && body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.to_ascii_lowercase().contains("tool_choice"))
}

/// A provider access policy is not evidence that the model lacks structured
/// tools. OpenCode Go, for example, lists deepseek-v4-flash before the user has
/// opted into its China-hosted deployment, then returns 403 to every inference
/// request. Treating that as `tool_calling = false` made the UI claim a model
/// capability failure that never occurred.
const PROBE_ISSUE_PROVIDER_QUOTA: &str = "provider_quota";

#[allow(dead_code)]
fn capability_probe_issue(status: u16, body: &Value) -> Option<String> {
    if status == 429 {
        return Some(PROBE_ISSUE_PROVIDER_QUOTA.to_string());
    }
    if !matches!(status, 401 | 403) {
        return None;
    }
    let message = body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if message.contains("requires explicit opt in") {
        Some(PROBE_ISSUE_PROVIDER_OPT_IN_REQUIRED.to_string())
    } else {
        Some(PROBE_ISSUE_PROVIDER_ACCESS_RESTRICTED.to_string())
    }
}

/// The tool probe ran out of token budget mid-generation, so its lack of a typed
/// tool call says nothing about the endpoint's tool protocol.
fn chat_truncated_before_tool_call(body: &Value) -> bool {
    body.pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        == Some("length")
}

fn responses_truncated_before_tool_call(body: &Value) -> bool {
    body.pointer("/incomplete_details/reason")
        .and_then(Value::as_str)
        == Some("max_output_tokens")
        || body.get("status").and_then(Value::as_str) == Some("incomplete")
}

/// Whether a tool-call-less response leaves the question open. A response that
/// was cut off, or that shows the model reasoning about the request, is a model
/// that engaged with the tool and may well call it on the next attempt. A short
/// plain answer with neither is a server that ignores `tools` outright — the
/// text-only case this probe exists to reject, and retrying it only wastes time.
fn chat_probe_inconclusive(body: &Value) -> bool {
    chat_truncated_before_tool_call(body)
        || [
            "/choices/0/message/reasoning",
            "/choices/0/message/reasoning_content",
        ]
        .iter()
        .any(|pointer| {
            body.pointer(pointer)
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
        })
}

fn responses_probe_inconclusive(body: &Value) -> bool {
    responses_truncated_before_tool_call(body)
        || body
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
}

fn has_responses_typed_tool_call(body: &Value) -> bool {
    body.get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call")
                && item.get("name").and_then(Value::as_str) == Some("vellum_probe_tool")
                && parse_tool_arguments(item.get("arguments"))
                    .and_then(|arguments| arguments.get("value").cloned())
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .as_deref()
                    == Some("ok")
        })
}

fn has_chat_typed_tool_call(body: &Value) -> bool {
    body.pointer("/choices/0/message/tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|item| {
            item.pointer("/function/name").and_then(Value::as_str) == Some("vellum_probe_tool")
                && parse_tool_arguments(item.pointer("/function/arguments"))
                    .and_then(|arguments| arguments.get("value").cloned())
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .as_deref()
                    == Some("ok")
        })
}

fn parse_tool_arguments(value: Option<&Value>) -> Option<Value> {
    match value? {
        Value::String(arguments) => serde_json::from_str(arguments).ok(),
        Value::Object(_) => value.cloned(),
        _ => None,
    }
}

async fn get_ollama_models(
    client: &reqwest::Client,
    root: &str,
    api_key: Option<&str>,
) -> AppResult<ModelsOutcome> {
    let mut request = client
        .get(format!("{}/api/tags", root.trim_end_matches('/')))
        .timeout(reachability_timeout());
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .await
        .map_err(|error| AppError::Unreachable(error.to_string()))?;
    if !response.status().is_success() {
        return Err(AppError::Message(format!(
            "Ollama model discovery failed: HTTP {}",
            response.status()
        )));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|error| AppError::Message(error.to_string()))?;
    models_outcome_from_entries(
        body.get("models")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default(),
    )
}

fn models_outcome_from_entries(entries: &[Value]) -> AppResult<ModelsOutcome> {
    let mut models = Vec::new();
    let mut context_windows = HashMap::new();
    let mut effort_metadata = HashMap::new();
    for model in entries {
        let Some(id) = model
            .get("id")
            .or_else(|| model.get("name"))
            .or_else(|| model.get("model"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if !models
            .iter()
            .any(|known: &String| known.eq_ignore_ascii_case(id))
        {
            models.push(id.to_string());
        }
        if let Some(window) = extract_model_context_window(model) {
            context_windows.insert(id.to_string(), window);
        }
        effort_metadata.insert(id.to_ascii_lowercase(), extract_effort_metadata(model));
    }
    let context_window = models
        .iter()
        .find_map(|model| context_windows.get(model).copied());
    Ok(ModelsOutcome {
        models,
        context_window,
        context_windows,
        effort_metadata,
    })
}

fn extract_effort_metadata(model: &Value) -> EffortMetadata {
    let mut levels = Vec::new();
    let sources = [
        Some(model),
        model.get("info"),
        model.get("metadata"),
        model.get("capabilities"),
    ];
    for source in sources.into_iter().flatten() {
        for key in [
            "supported_reasoning_levels",
            "supported_reasoning_efforts",
            "reasoning_efforts",
        ] {
            if let Some(values) = source.get(key).and_then(Value::as_array) {
                for value in values {
                    let effort = value.as_str().or_else(|| {
                        value
                            .get("effort")
                            .or_else(|| value.get("value"))
                            .or_else(|| value.get("id"))
                            .and_then(Value::as_str)
                    });
                    if let Some(effort) = effort {
                        if !levels.iter().any(|known| known == effort) {
                            levels.push(effort.to_string());
                        }
                    }
                }
            }
        }
    }
    let default = ["default_reasoning_level", "default_reasoning_effort"]
        .into_iter()
        .find_map(|key| {
            sources
                .into_iter()
                .flatten()
                .find_map(|source| source.get(key).and_then(Value::as_str).map(str::to_owned))
        })
        .filter(|candidate| levels.iter().any(|level| level == candidate));
    EffortMetadata { levels, default }
}

async fn try_responses_then_chat(
    client: &reqwest::Client,
    v1: &str,
    api_key: Option<&str>,
    model: &str,
) -> ProbeOutcome {
    // 先試 Responses。
    if let Ok((status, body, _)) = post_json(
        client,
        &format!("{v1}/responses"),
        api_key,
        build_responses_body(".", model),
    )
    .await
    {
        if (200..300).contains(&status)
            && responses_harness_compatible(client, v1, api_key, model).await
        {
            return ProbeOutcome {
                wire: Some(WireFormat::Responses),
                streaming: probe_streaming(client, v1, api_key, model, WireFormat::Responses)
                    .await
                    .as_capability(),
                reasoning: extract_reasoning(&body) || known_reasoning_capability(model),
                store: body.get("store").and_then(Value::as_bool),
            };
        }
    }

    // 退 Chat Completions。
    let mut chat_body = build_chat_body(".", model);
    apply_chat_probe_options(v1, model, &mut chat_body);
    if let Ok((status, body, _)) = post_json(
        client,
        &format!("{v1}/chat/completions"),
        api_key,
        chat_body,
    )
    .await
    {
        if (200..300).contains(&status) && chat_harness_compatible(client, v1, api_key, model).await
        {
            return ProbeOutcome {
                wire: Some(WireFormat::Chat),
                streaming: probe_streaming(client, v1, api_key, model, WireFormat::Chat)
                    .await
                    .as_capability(),
                reasoning: extract_reasoning(&body) || known_reasoning_capability(model),
                store: None,
            };
        }
    }

    ProbeOutcome {
        wire: None,
        streaming: None,
        reasoning: false,
        store: None,
    }
}

#[derive(Debug, Clone)]
struct RawProbeResponse {
    status: Option<u16>,
    body: Value,
    #[allow(dead_code)]
    content_type: Option<String>,
    retry_after: Option<u64>,
    duration_ms: u64,
    timeout: bool,
    error: Option<String>,
}

async fn execute_probe_request(
    client: &reqwest::Client,
    url: &str,
    api_key: Option<&str>,
    body: Value,
    timeout: Option<std::time::Duration>,
) -> RawProbeResponse {
    let started = std::time::Instant::now();
    let mut req = client.post(url).json(&body);
    if let Some(k) = api_key {
        req = req.bearer_auth(k);
    }
    if let Some(t) = timeout {
        req = req.timeout(t);
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            let ct = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            match resp.bytes().await {
                Ok(bytes) => {
                    let duration_ms = started.elapsed().as_millis() as u64;
                    match serde_json::from_slice::<Value>(&bytes) {
                        Ok(body) => RawProbeResponse {
                            status: Some(status),
                            body,
                            content_type: ct,
                            retry_after,
                            duration_ms,
                            timeout: false,
                            error: None,
                        },
                        Err(error) => {
                            let response_text = String::from_utf8_lossy(&bytes);
                            let detail = if response_text.trim().is_empty() {
                                format!("response body was not valid JSON: {error}")
                            } else {
                                format!("non-JSON response body: {}", response_text.trim())
                            };
                            RawProbeResponse {
                                status: Some(status),
                                body: Value::Null,
                                content_type: ct,
                                retry_after,
                                duration_ms,
                                timeout: false,
                                error: Some(bounded_probe_message(&detail)),
                            }
                        }
                    }
                }
                Err(error) => RawProbeResponse {
                    status: Some(status),
                    body: Value::Null,
                    content_type: ct,
                    retry_after,
                    duration_ms: started.elapsed().as_millis() as u64,
                    timeout: error.is_timeout(),
                    error: Some(bounded_probe_message(&error.to_string())),
                },
            }
        }
        Err(e) => {
            let duration_ms = started.elapsed().as_millis() as u64;
            let is_timeout = e.is_timeout();
            RawProbeResponse {
                status: None,
                body: Value::Null,
                content_type: None,
                retry_after: None,
                duration_ms,
                timeout: is_timeout,
                error: Some(bounded_probe_message(&e.to_string())),
            }
        }
    }
}

fn bounded_probe_message(message: &str) -> String {
    const LIMIT: usize = 240;
    let trimmed = message.trim();
    let mut chars = trimmed.chars();
    let prefix = chars
        .by_ref()
        .take(LIMIT.saturating_sub(1))
        .collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn extract_probe_error_message(body: &Value, error_fallback: Option<&str>) -> Option<String> {
    let msg = body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| body.get("message").and_then(Value::as_str))
        .or_else(|| body.get("detail").and_then(Value::as_str))
        .or(error_fallback);
    msg.map(bounded_probe_message)
}

fn extract_full_response_text(body: &Value) -> String {
    let mut text = String::new();
    if let Some(choices) = body.get("choices").and_then(Value::as_array) {
        for choice in choices {
            if let Some(content) = choice.pointer("/message/content").and_then(Value::as_str) {
                text.push_str(content);
            }
        }
    }
    if let Some(output) = body.get("output").and_then(Value::as_array) {
        for item in output {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    if let Some(chunk) = part.get("text").and_then(Value::as_str) {
                        text.push_str(chunk);
                    }
                }
            }
        }
    }
    text
}

fn extract_reasoning_text(body: &Value) -> String {
    let mut text = String::new();
    if let Some(choices) = body.get("choices").and_then(Value::as_array) {
        for choice in choices {
            if let Some(content) = choice
                .pointer("/message/reasoning_content")
                .and_then(Value::as_str)
            {
                text.push_str(content);
            }
            if let Some(content) = choice.pointer("/message/reasoning").and_then(Value::as_str) {
                text.push_str(content);
            }
        }
    }
    if let Some(output) = body.get("output").and_then(Value::as_array) {
        for item in output {
            if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                if let Some(summary) = item.get("summary").and_then(Value::as_str) {
                    text.push_str(summary);
                }
            }
        }
    }
    text
}

fn analyze_missing_tool_call(body: &Value) -> (String, String) {
    if chat_truncated_before_tool_call(body) || responses_truncated_before_tool_call(body) {
        return (
            "truncated_token_limit".to_string(),
            "model output truncated before tool call".to_string(),
        );
    }
    let text = extract_full_response_text(body);
    if text.contains("<tool_call>")
        || text.contains("<function_call>")
        || text.contains("</tool_call>")
        || text.contains("<|tool_call|>")
        || text.contains(
            "```json
{\"name\":",
        )
    {
        return (
            "xml_tags".to_string(),
            "model output raw XML/JSON tags instead of typed tool call".to_string(),
        );
    }
    let reasoning = extract_reasoning_text(body);
    if !reasoning.trim().is_empty() && text.trim().is_empty() {
        return (
            "thought_only".to_string(),
            "model generated reasoning trace without tool call".to_string(),
        );
    }
    if text.trim().is_empty() {
        return (
            "empty".to_string(),
            "model returned empty response".to_string(),
        );
    }
    (
        "plain_text".to_string(),
        "model returned plain text instead of typed tool call".to_string(),
    )
}

fn classify_raw_probe(
    raw: &RawProbeResponse,
    stage: &str,
    wire: Option<WireFormat>,
) -> (crate::model::ProbeAttempt, Option<String>) {
    let mut issue = None;
    let (outcome, msg, model_response_kind) = if raw.timeout {
        issue = Some("timeout".to_string());
        (
            "timeout".to_string(),
            Some(format!("request timed out after {}ms", raw.duration_ms)),
            None,
        )
    } else if raw.error.is_some()
        && raw
            .status
            .is_some_and(|status| (200..300).contains(&status))
    {
        issue = Some("provider_protocol".to_string());
        ("provider_protocol".to_string(), raw.error.clone(), None)
    } else if let Some(status) = raw.status {
        let err_msg = extract_probe_error_message(&raw.body, raw.error.as_deref());
        if status == 429 {
            issue = Some("provider_quota".to_string());
            (
                "provider_quota".to_string(),
                err_msg.or(Some("rate limit / quota exceeded".into())),
                None,
            )
        } else if status == 401 || status == 403 {
            let is_opt_in = err_msg
                .as_deref()
                .is_some_and(|m| m.to_ascii_lowercase().contains("requires explicit opt in"));
            let issue_key = if is_opt_in {
                "provider_opt_in_required".to_string()
            } else {
                "provider_unauthorized".to_string()
            };
            issue = Some(issue_key);
            (
                "provider_unauthorized".to_string(),
                err_msg.or(Some("authentication failed".into())),
                None,
            )
        } else if (300..500).contains(&status) {
            issue = Some("provider_protocol".to_string());
            (
                "provider_protocol".to_string(),
                err_msg.or(Some(format!("HTTP {status} client error"))),
                None,
            )
        } else if status >= 500 {
            issue = Some("provider_unavailable".to_string());
            (
                "provider_unavailable".to_string(),
                err_msg.or(Some(format!("HTTP {status} server error"))),
                None,
            )
        } else {
            ("success".to_string(), None, None)
        }
    } else {
        issue = Some("provider_unavailable".to_string());
        (
            "provider_unavailable".to_string(),
            raw.error.clone().or(Some("network error".into())),
            None,
        )
    };

    (
        crate::model::ProbeAttempt {
            stage: stage.to_string(),
            wire,
            outcome,
            status: raw.status,
            duration_ms: raw.duration_ms,
            timeout: raw.timeout,
            retry_after: raw.retry_after,
            message: msg,
            model_response_kind,
        },
        issue,
    )
}

async fn responses_harness_compatible(
    client: &reqwest::Client,
    v1: &str,
    api_key: Option<&str>,
    model: &str,
) -> bool {
    let (ok, _, _) = probe_responses_typed_tool(client, v1, api_key, model).await;
    ok
}

async fn probe_responses_typed_tool(
    client: &reqwest::Client,
    v1: &str,
    api_key: Option<&str>,
    model: &str,
) -> (bool, Vec<crate::model::ProbeAttempt>, Option<String>) {
    let mut attempts = Vec::new();
    let mut last_issue = None;
    let mut body = build_responses_harness_body(model);
    let profile = crate::harness::resolve(
        crate::model::ProviderKind::OpenAiCompatible,
        WireFormat::Responses,
    );
    if crate::adapter::normalize_compatible_responses_request(&mut body, &profile, false).is_err() {
        attempts.push(crate::model::ProbeAttempt {
            stage: "responsesShape".into(),
            wire: Some(WireFormat::Responses),
            outcome: "unsupported_protocol".into(),
            status: None,
            duration_ms: 0,
            timeout: false,
            retry_after: None,
            message: Some("failed to normalize compatible responses request".into()),
            model_response_kind: None,
        });
        return (false, attempts, Some("unsupported_protocol".into()));
    }
    let raw_shape =
        execute_probe_request(client, &format!("{v1}/responses"), api_key, body, None).await;
    let (shape_attempt, shape_issue) =
        classify_raw_probe(&raw_shape, "responsesShape", Some(WireFormat::Responses));
    attempts.push(shape_attempt);
    if shape_issue.is_some() || !raw_shape.status.is_some_and(|s| (200..300).contains(&s)) {
        return (
            false,
            attempts,
            shape_issue.or(Some("unsupported_protocol".into())),
        );
    }

    let url = format!("{v1}/responses");
    let started = std::time::Instant::now();
    let mut attempt_idx = 0;
    while tool_probe_may_retry(v1, attempt_idx, started) {
        let probe = build_responses_tool_call_body(model, tool_probe_budget(attempt_idx));
        attempt_idx += 1;
        let raw =
            execute_probe_request(client, &url, api_key, probe, Some(tool_probe_timeout(v1))).await;
        let (mut att, issue) = classify_raw_probe(&raw, "typedTool", Some(WireFormat::Responses));
        if let Some(iss) = issue {
            last_issue = Some(iss);
            attempts.push(att);
            return (false, attempts, last_issue);
        }
        if raw.status.is_some_and(|s| (200..300).contains(&s)) {
            if has_responses_typed_tool_call(&raw.body) {
                att.outcome = "success".into();
                att.message = Some("typed tool call verified".into());
                attempts.push(att);
                return (true, attempts, None);
            }
            let (kind, desc) = analyze_missing_tool_call(&raw.body);
            att.outcome = "tool_call_missing".into();
            att.model_response_kind = Some(kind);
            att.message = Some(desc);
            last_issue = Some("tool_call_missing".into());
            attempts.push(att);
            if !responses_probe_inconclusive(&raw.body) {
                return (false, attempts, last_issue);
            }
        } else {
            attempts.push(att);
            return (
                false,
                attempts,
                last_issue.or(Some("provider_protocol".into())),
            );
        }
    }
    (false, attempts, last_issue.or(Some("timeout".into())))
}

/// The first attempt stays cheap for the common case: a non-reasoning model that
/// calls the tool immediately answers well inside 64 tokens. Once that attempt
/// has shown the model thinks or was cut off, the budget has to cover a whole
/// think block before the call.
fn tool_probe_budget(attempt: usize) -> u32 {
    if attempt == 0 {
        TOOL_PROBE_MAX_TOKENS
    } else {
        TOOL_PROBE_REASONING_MAX_TOKENS
    }
}

async fn chat_harness_compatible(
    client: &reqwest::Client,
    v1: &str,
    api_key: Option<&str>,
    model: &str,
) -> bool {
    let (ok, _, _) = probe_chat_typed_tool(client, v1, api_key, model).await;
    ok
}

async fn probe_chat_typed_tool(
    client: &reqwest::Client,
    v1: &str,
    api_key: Option<&str>,
    model: &str,
) -> (bool, Vec<crate::model::ProbeAttempt>, Option<String>) {
    let mut attempts = Vec::new();
    let mut last_issue = None;
    let url = format!("{v1}/chat/completions");
    let started = std::time::Instant::now();
    let mut attempt_idx = 0;
    let mut tool_choice = "required";
    while tool_probe_may_retry(v1, attempt_idx, started) {
        let mut probe =
            build_chat_tool_call_body(model, tool_probe_budget(attempt_idx), tool_choice);
        attempt_idx += 1;
        apply_chat_probe_options(v1, model, &mut probe);
        let raw =
            execute_probe_request(client, &url, api_key, probe, Some(tool_probe_timeout(v1))).await;
        let (mut att, issue) = classify_raw_probe(&raw, "typedTool", Some(WireFormat::Chat));
        if let Some(status) = raw.status {
            if (200..300).contains(&status) {
                if has_chat_typed_tool_call(&raw.body) {
                    att.outcome = "success".into();
                    att.message = Some("typed tool call verified".into());
                    attempts.push(att);
                    return (true, attempts, None);
                }
                let (kind, desc) = analyze_missing_tool_call(&raw.body);
                att.outcome = "tool_call_missing".into();
                att.model_response_kind = Some(kind);
                att.message = Some(desc);
                last_issue = Some("tool_call_missing".into());
                attempts.push(att);
                if !chat_probe_inconclusive(&raw.body) {
                    return (false, attempts, last_issue);
                }
                continue;
            }
            if tool_choice == "required" && chat_tool_choice_rejected(status, &raw.body) {
                tool_choice = "auto";
                attempt_idx = attempt_idx.saturating_sub(1);
                att.message = Some("tool_choice required rejected, retrying with auto".into());
                attempts.push(att);
                continue;
            }
        }
        last_issue = issue.or(Some("provider_protocol".into()));
        attempts.push(att);
        return (false, attempts, last_issue);
    }
    (false, attempts, last_issue.or(Some("timeout".into())))
}

/// Observe whether this Chat route rejects a tool-result tail. Only a live
/// rejection of that shape persists `neutralUserBridge`; names are never
/// guessed.
async fn probe_chat_continuation_tail(
    client: &reqwest::Client,
    v1: &str,
    api_key: Option<&str>,
    model: &str,
) -> ContinuationTailObservation {
    let url = format!("{v1}/chat/completions");
    let mut body = json!({
        "model": model,
        "messages": [
            {"role": "user", "content": "Call vellum_probe_tool then stop."},
            {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_probe_tail",
                    "type": "function",
                    "function": {
                        "name": "vellum_probe_tool",
                        "arguments": "{\"value\":\"ok\"}"
                    }
                }]
            },
            {
                "role": "tool",
                "tool_call_id": "call_probe_tail",
                "content": "ok"
            }
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "vellum_probe_tool",
                "description": "Required Codex tool protocol probe.",
                "parameters": {
                    "type": "object",
                    "properties": {"value": {"type": "string"}},
                    "required": ["value"]
                }
            }
        }],
        "max_tokens": PROBE_MAX_TOKENS,
        "stream": false
    });
    apply_chat_probe_options(v1, model, &mut body);
    let Ok((status, response, _)) =
        post_json_with_timeout(client, &url, api_key, body, Some(tool_probe_timeout(v1))).await
    else {
        return ContinuationTailObservation::Inconclusive;
    };
    if (200..300).contains(&status) {
        return ContinuationTailObservation::ConfirmedNative;
    }
    if status == 429 || status >= 500 {
        return ContinuationTailObservation::Inconclusive;
    }
    let message = response
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if message.contains("no user query") {
        ContinuationTailObservation::ConfirmedBridge
    } else {
        ContinuationTailObservation::Inconclusive
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationTailObservation {
    ConfirmedNative,
    ConfirmedBridge,
    Inconclusive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningReplayObservation {
    ConfirmedToolCallBound,
    ConfirmedNone,
    Inconclusive,
}

pub fn merge_chat_capability_observations(
    existing: &RuntimeChatCapabilities,
    tail: ContinuationTailObservation,
    reasoning: ReasoningReplayObservation,
) -> RuntimeChatCapabilities {
    RuntimeChatCapabilities {
        continuation_tail: match tail {
            ContinuationTailObservation::ConfirmedNative => ContinuationTail::NativeToolResult,
            ContinuationTailObservation::ConfirmedBridge => ContinuationTail::NeutralUserBridge,
            ContinuationTailObservation::Inconclusive => existing.continuation_tail,
        },
        reasoning_replay: match reasoning {
            ReasoningReplayObservation::ConfirmedToolCallBound => ReasoningReplay::ToolCallBound,
            ReasoningReplayObservation::ConfirmedNone => ReasoningReplay::None,
            ReasoningReplayObservation::Inconclusive => existing.reasoning_replay,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReasoningContinuationClass {
    Success,
    MissingReasoning,
    UnsupportedShape,
    Inconclusive,
}

fn classify_reasoning_continuation(status: u16, message: &str) -> ReasoningContinuationClass {
    if (200..300).contains(&status) {
        return ReasoningContinuationClass::Success;
    }
    if status == 429 || status >= 500 {
        return ReasoningContinuationClass::Inconclusive;
    }
    let lower = message.to_ascii_lowercase();
    if is_unsupported_reasoning_shape(&lower) {
        return ReasoningContinuationClass::UnsupportedShape;
    }
    if is_missing_reasoning_error(&lower) {
        return ReasoningContinuationClass::MissingReasoning;
    }
    ReasoningContinuationClass::Inconclusive
}

fn is_missing_reasoning_error(lower: &str) -> bool {
    let mentions_field = lower.contains("reasoning_content") || lower.contains("reasoning content");
    let required = lower.contains("must")
        || lower.contains("required")
        || lower.contains("missing")
        || lower.contains("pass")
        || lower.contains("need");
    (mentions_field && required) || lower.contains("missing reasoning")
}

fn is_unsupported_reasoning_shape(lower: &str) -> bool {
    let mentions_field = lower.contains("reasoning_content")
        || lower.contains("reasoning content")
        || lower.contains("reasoning");
    mentions_field
        && (lower.contains("unsupported")
            || lower.contains("not supported")
            || lower.contains("unknown field")
            || lower.contains("unrecognized")
            || lower.contains("unexpected")
            || lower.contains("extra field"))
}

fn chat_probe_tool_message(body: &Value) -> Option<(Value, Option<String>)> {
    let message = body.pointer("/choices/0/message")?.clone();
    let calls = message.get("tool_calls").and_then(Value::as_array)?;
    if calls.is_empty() {
        return None;
    }
    let reasoning = message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .or_else(|| message.get("reasoning").and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string);
    Some((message, reasoning))
}

fn error_message(body: &Value) -> String {
    body.pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn continuation_messages_without_reasoning(user: &Value, assistant: &Value) -> Vec<Value> {
    let mut assistant = assistant.clone();
    if let Some(object) = assistant.as_object_mut() {
        object.remove("reasoning_content");
        object.remove("reasoning");
        if object.get("content").is_none() {
            object.insert("content".into(), json!(""));
        }
    }
    let mut messages = vec![user.clone(), assistant];
    let call_ids: Vec<Value> = messages[1]
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|call| {
            call.get("id")
                .cloned()
                .unwrap_or(json!("call_probe_reason"))
        })
        .collect();
    for id in call_ids {
        messages.push(json!({
            "role": "tool",
            "tool_call_id": id,
            "content": "ok"
        }));
    }
    messages
}

fn with_provider_reasoning(messages: &[Value], reasoning: &str) -> Vec<Value> {
    let mut messages = messages.to_vec();
    if let Some(assistant) = messages.get_mut(1).and_then(Value::as_object_mut) {
        assistant.insert("reasoning_content".into(), json!(reasoning));
    }
    messages
}

async fn probe_chat_reasoning_replay(
    client: &reqwest::Client,
    v1: &str,
    api_key: Option<&str>,
    model: &str,
) -> ReasoningReplayObservation {
    let url = format!("{v1}/chat/completions");
    let mut first = build_chat_tool_call_body(model, tool_probe_budget(1), "required");
    apply_chat_probe_options(v1, model, &mut first);
    let Ok((status, response, _)) =
        post_json_with_timeout(client, &url, api_key, first, Some(tool_probe_timeout(v1))).await
    else {
        return ReasoningReplayObservation::Inconclusive;
    };
    let response = if (200..300).contains(&status) {
        response
    } else if status == 429 || status >= 500 {
        return ReasoningReplayObservation::Inconclusive;
    } else if chat_tool_choice_rejected(status, &response) {
        let mut retry = build_chat_tool_call_body(model, tool_probe_budget(1), "auto");
        apply_chat_probe_options(v1, model, &mut retry);
        let Ok((status, response, _)) =
            post_json_with_timeout(client, &url, api_key, retry, Some(tool_probe_timeout(v1)))
                .await
        else {
            return ReasoningReplayObservation::Inconclusive;
        };
        if status == 429 || status >= 500 || !(200..300).contains(&status) {
            return ReasoningReplayObservation::Inconclusive;
        }
        response
    } else {
        return ReasoningReplayObservation::Inconclusive;
    };
    let Some((assistant, reasoning)) = chat_probe_tool_message(&response) else {
        return ReasoningReplayObservation::Inconclusive;
    };
    let user = json!({
        "role": "user",
        "content": "Call vellum_probe_tool with value exactly ok. Do not answer in text."
    });
    let without_reasoning = continuation_messages_without_reasoning(&user, &assistant);
    let mut continuation = json!({
        "model": model,
        "messages": without_reasoning.clone(),
        "tools": [{
            "type": "function",
            "function": {
                "name": "vellum_probe_tool",
                "description": "Required Codex tool protocol probe.",
                "parameters": {
                    "type": "object",
                    "properties": {"value": {"type": "string"}},
                    "required": ["value"]
                }
            }
        }],
        "max_tokens": PROBE_MAX_TOKENS,
        "stream": false
    });
    apply_chat_probe_options(v1, model, &mut continuation);
    let Ok((status, response, _)) = post_json_with_timeout(
        client,
        &url,
        api_key,
        continuation.clone(),
        Some(tool_probe_timeout(v1)),
    )
    .await
    else {
        return ReasoningReplayObservation::Inconclusive;
    };
    match classify_reasoning_continuation(status, &error_message(&response)) {
        ReasoningContinuationClass::Success => ReasoningReplayObservation::ConfirmedNone,
        ReasoningContinuationClass::UnsupportedShape | ReasoningContinuationClass::Inconclusive => {
            ReasoningReplayObservation::Inconclusive
        }
        ReasoningContinuationClass::MissingReasoning => {
            let Some(reasoning) = reasoning else {
                return ReasoningReplayObservation::Inconclusive;
            };
            continuation["messages"] =
                Value::Array(with_provider_reasoning(&without_reasoning, &reasoning));
            let Ok((status, response, _)) = post_json_with_timeout(
                client,
                &url,
                api_key,
                continuation,
                Some(tool_probe_timeout(v1)),
            )
            .await
            else {
                return ReasoningReplayObservation::Inconclusive;
            };
            match classify_reasoning_continuation(status, &error_message(&response)) {
                ReasoningContinuationClass::Success => {
                    ReasoningReplayObservation::ConfirmedToolCallBound
                }
                _ => ReasoningReplayObservation::Inconclusive,
            }
        }
    }
}

fn tool_probe_timeout(v1: &str) -> std::time::Duration {
    std::time::Duration::from_secs(if opencode_zen_catalog(v1).is_some() {
        OPENCODE_TOOL_PROBE_TIMEOUT_SECS
    } else {
        TOOL_PROBE_TIMEOUT_SECS
    })
}

/// Budget for the list endpoints that establish the endpoint is alive. Kept
/// short on purpose: this is the timeout whose expiry genuinely means
/// "unreachable", and the user is waiting on it.
fn reachability_timeout() -> std::time::Duration {
    std::time::Duration::from_secs(PROBE_TIMEOUT_SECS)
}

/// Attempts remain worth making while both the count and the clock allow it.
/// `attempt` is zero-based, so attempt 0 always runs.
fn tool_probe_may_retry(v1: &str, attempt: usize, started: std::time::Instant) -> bool {
    let budget = if opencode_zen_catalog(v1).is_some() {
        OPENCODE_TOOL_PROBE_TOTAL_BUDGET_SECS
    } else {
        TOOL_PROBE_TOTAL_BUDGET_SECS
    };
    attempt < TOOL_PROBE_ATTEMPTS && started.elapsed() < std::time::Duration::from_secs(budget)
}

#[allow(clippy::too_many_arguments)]
async fn probe_model_capability(
    client: &reqwest::Client,
    v1: &str,
    api_key: Option<&str>,
    model: String,
    wire: WireFormat,
    context_window: Option<u64>,
    effort_metadata: EffortMetadata,
    existing_chat: &RuntimeChatCapabilities,
) -> ModelCapability {
    let mut probe_attempts = Vec::new();
    let metadata_declares_reasoning = !effort_metadata.levels.is_empty();
    let (endpoint, mut body) = match wire {
        WireFormat::Responses => (format!("{v1}/responses"), build_responses_body(".", &model)),
        WireFormat::Chat => (
            format!("{v1}/chat/completions"),
            build_chat_body(".", &model),
        ),
    };
    if wire == WireFormat::Chat {
        apply_chat_probe_options(v1, &model, &mut body);
    }
    let tested_raw = execute_probe_request(client, &endpoint, api_key, body, None).await;
    let (comp_attempt, comp_issue) = classify_raw_probe(&tested_raw, "completion", Some(wire));
    probe_attempts.push(comp_attempt.clone());

    let policy_issue = comp_issue.clone().filter(|i| {
        matches!(
            i.as_str(),
            PROBE_ISSUE_PROVIDER_OPT_IN_REQUIRED
                | PROBE_ISSUE_PROVIDER_ACCESS_RESTRICTED
                | PROBE_ISSUE_PROVIDER_QUOTA
        )
    });
    let request_accepted = tested_raw
        .status
        .is_some_and(|status| (200..300).contains(&status));

    let (tool_calling_ok, tool_attempts, tool_issue) = if policy_issue.is_some() {
        (false, Vec::new(), policy_issue.clone())
    } else if request_accepted {
        match wire {
            WireFormat::Responses => probe_responses_typed_tool(client, v1, api_key, &model).await,
            WireFormat::Chat => probe_chat_typed_tool(client, v1, api_key, &model).await,
        }
    } else {
        (false, Vec::new(), comp_issue.clone())
    };
    probe_attempts.extend(tool_attempts);
    let tool_calling = if policy_issue.is_some() {
        None
    } else {
        Some(tool_calling_ok)
    };

    let (verified_wire, reasoning, streaming, streaming_issue) = match tested_raw.status {
        Some(status) if (200..300).contains(&status) => {
            let streaming_start = std::time::Instant::now();
            let streaming = probe_streaming(client, v1, api_key, &model, wire).await;
            let streaming_ms = streaming_start.elapsed().as_millis() as u64;
            probe_attempts.push(crate::model::ProbeAttempt {
                stage: "streaming".into(),
                wire: Some(wire),
                outcome: if streaming == StreamingProbe::Supported {
                    "success".into()
                } else if streaming == StreamingProbe::Timeout {
                    "timeout".into()
                } else {
                    "provider_protocol".into()
                },
                status: None,
                duration_ms: streaming_ms,
                timeout: streaming == StreamingProbe::Timeout,
                retry_after: None,
                message: streaming.probe_issue().map(str::to_string),
                model_response_kind: None,
            });
            (
                Some(wire),
                Some(
                    extract_reasoning(&tested_raw.body)
                        || known_reasoning_capability(&model)
                        || metadata_declares_reasoning,
                ),
                streaming.as_capability(),
                streaming.probe_issue().map(str::to_string),
            )
        }
        _ if policy_issue.is_some() => (
            Some(wire),
            (known_reasoning_capability(&model) || metadata_declares_reasoning).then_some(true),
            None,
            None,
        ),
        _ => (
            None,
            (known_reasoning_capability(&model) || metadata_declares_reasoning).then_some(true),
            None,
            None,
        ),
    };
    let effort_probe_allowed = policy_issue.is_none();
    let reasoning_supported = reasoning.unwrap_or(false);
    let harness_verified = verified_wire.is_some() && tool_calling == Some(true);
    let effort_outcome = if !effort_probe_allowed {
        EffortProbeOutcome::Indeterminate("provider_quota_or_access")
    } else if !(reasoning_supported && harness_verified) {
        EffortProbeOutcome::NotApplicable
    } else {
        let effort_start = std::time::Instant::now();
        let outcome =
            probe_reasoning_efforts(client, v1, api_key, &model, wire, effort_metadata).await;
        let effort_ms = effort_start.elapsed().as_millis() as u64;
        let (eff_outcome, eff_msg) = match &outcome {
            EffortProbeOutcome::Supported { levels, .. } => (
                "success".to_string(),
                Some(format!("supported levels: {:?}", levels)),
            ),
            EffortProbeOutcome::Unsupported => (
                "unsupported_protocol".to_string(),
                Some("no effort levels accepted".into()),
            ),
            EffortProbeOutcome::Indeterminate(issue) => (
                issue.to_string(),
                Some(effort_probe_diagnostic(issue).to_string()),
            ),
            EffortProbeOutcome::NotApplicable => ("not_applicable".to_string(), None),
        };
        probe_attempts.push(crate::model::ProbeAttempt {
            stage: "effort".into(),
            wire: Some(wire),
            outcome: eff_outcome,
            status: None,
            duration_ms: effort_ms,
            timeout: false,
            retry_after: None,
            message: eff_msg,
            model_response_kind: None,
        });
        outcome
    };
    let (
        reasoning_efforts,
        default_reasoning_effort,
        reasoning_effort_transport,
        effort_probe_status,
        effort_probe_issue,
    ) = match effort_outcome {
        EffortProbeOutcome::NotApplicable => (
            Vec::new(),
            None,
            ReasoningEffortTransport::None,
            EffortProbeStatus::NotApplicable,
            None,
        ),
        EffortProbeOutcome::Unsupported => (
            Vec::new(),
            None,
            ReasoningEffortTransport::None,
            EffortProbeStatus::Unsupported,
            None,
        ),
        EffortProbeOutcome::Indeterminate(issue) => (
            Vec::new(),
            None,
            ReasoningEffortTransport::None,
            EffortProbeStatus::Indeterminate,
            Some(issue.to_string()),
        ),
        EffortProbeOutcome::Supported {
            levels,
            default,
            transport,
        } => (
            levels,
            default,
            transport,
            EffortProbeStatus::Supported,
            None,
        ),
    };
    let effort_probe_version =
        (effort_probe_status != EffortProbeStatus::Indeterminate).then_some(EFFORT_PROBE_VERSION);
    let tail_observation = if verified_wire == Some(WireFormat::Chat) && tool_calling == Some(true)
    {
        probe_chat_continuation_tail(client, v1, api_key, &model).await
    } else {
        ContinuationTailObservation::Inconclusive
    };
    let reasoning_observation = if verified_wire == Some(WireFormat::Chat)
        && tool_calling == Some(true)
        && reasoning_supported
    {
        probe_chat_reasoning_replay(client, v1, api_key, &model).await
    } else if verified_wire == Some(WireFormat::Chat) && !reasoning_supported {
        ReasoningReplayObservation::ConfirmedNone
    } else {
        ReasoningReplayObservation::Inconclusive
    };
    let chat_capabilities =
        merge_chat_capability_observations(existing_chat, tail_observation, reasoning_observation);

    let probe_issue = if tool_calling == Some(true) {
        policy_issue.or(streaming_issue)
    } else {
        tool_issue.or(policy_issue).or(streaming_issue)
    };
    let last_probe_failed = Some(tool_calling != Some(true));

    ModelCapability {
        model,
        context_window,
        wire: verified_wire,
        streaming,
        reasoning,
        tool_calling,
        probe_version: Some(HARNESS_PROBE_VERSION),
        probe_issue,
        reasoning_efforts,
        default_reasoning_effort,
        reasoning_effort_transport,
        effort_probe_version,
        effort_probed_at: effort_probe_version.map(|_| chrono::Utc::now().timestamp()),
        effort_probe_status,
        effort_probe_issue,
        chat_capabilities,
        probe_attempts,
        last_probe_failed,
        last_probed_at: Some(chrono::Utc::now().timestamp()),
        ..Default::default()
    }
}

/// Verify a model on its preferred wire, then try the other OpenAI-compatible
/// wire when that model rejects it. Provider catalogs can be heterogeneous:
/// one model may implement Responses while another on the same base URL only
/// implements Chat Completions.
#[allow(clippy::too_many_arguments)]
async fn probe_model_capability_with_wire_fallback(
    client: &reqwest::Client,
    v1: &str,
    api_key: Option<&str>,
    model: String,
    preferred_wire: WireFormat,
    context_window: Option<u64>,
    effort_metadata: EffortMetadata,
    existing_chat: &RuntimeChatCapabilities,
) -> ModelCapability {
    let primary = probe_model_capability(
        client,
        v1,
        api_key,
        model.clone(),
        preferred_wire,
        context_window,
        effort_metadata.clone(),
        existing_chat,
    )
    .await;
    if primary.tool_calling == Some(true)
        || matches!(
            primary.probe_issue.as_deref(),
            Some(PROBE_ISSUE_PROVIDER_OPT_IN_REQUIRED)
                | Some(PROBE_ISSUE_PROVIDER_ACCESS_RESTRICTED)
                | Some(PROBE_ISSUE_PROVIDER_QUOTA)
                | Some("provider_unauthorized")
        )
    {
        return primary;
    }
    let fallback_wire = match preferred_wire {
        WireFormat::Responses => WireFormat::Chat,
        WireFormat::Chat => WireFormat::Responses,
    };
    let mut fallback = probe_model_capability(
        client,
        v1,
        api_key,
        model,
        fallback_wire,
        context_window,
        effort_metadata,
        existing_chat,
    )
    .await;
    let mut combined_attempts = primary.probe_attempts;
    combined_attempts.extend(fallback.probe_attempts);
    fallback.probe_attempts = combined_attempts;
    fallback
}

/// Writes an Effort candidate into whichever request-body location `transport`
/// names. `ProviderSpecific` is `chat_template_kwargs.reasoning_effort` — the
/// convention llama.cpp/vLLM use to pass a value through to a custom Jinja
/// chat template's own variable of the same name, for deployments whose
/// template reads `reasoning_effort` but isn't reachable through either of
/// the two standard OpenAI-dialect encodings.
fn apply_effort_field(body: &mut Value, transport: ReasoningEffortTransport, effort: &str) {
    match transport {
        ReasoningEffortTransport::ResponsesObject | ReasoningEffortTransport::ChatObject => {
            body["reasoning"] = json!({"effort": effort});
        }
        ReasoningEffortTransport::ChatField => {
            body["reasoning_effort"] = json!(effort);
        }
        ReasoningEffortTransport::ProviderSpecific => {
            body["chat_template_kwargs"] = json!({"reasoning_effort": effort});
        }
        ReasoningEffortTransport::None => {}
    }
}

async fn probe_effort_value(
    client: &reqwest::Client,
    v1: &str,
    api_key: Option<&str>,
    model: &str,
    wire: WireFormat,
    transport: ReasoningEffortTransport,
    effort: &str,
) -> EffortCandidateOutcome {
    let (url, mut body) = match wire {
        WireFormat::Responses => (format!("{v1}/responses"), build_responses_body(".", model)),
        WireFormat::Chat => (
            format!("{v1}/chat/completions"),
            build_chat_body(".", model),
        ),
    };
    apply_effort_field(&mut body, transport, effort);
    // A cold local model can still spend a long time on the "thinking" trace
    // that precedes even a 1-token capped answer (real observed range:
    // 15–55s for a single candidate on a slow single-slot box) — the same
    // cold-start reasoning that motivates `STREAMING_PROBE_TIMEOUT_SECS`
    // elsewhere in this file.
    let timeout = streaming_probe_timeout(v1);
    let result = post_json_with_timeout(client, &url, api_key, body, Some(timeout)).await;
    classify_effort_candidate(&result)
}

/// Pulls together whatever generated text a response carries — reasoning
/// trace plus the final message — so the Effort behavioral tiebreaker can
/// diff two generations for content differences a status code can't reveal.
fn extract_effort_probe_text(body: &Value) -> String {
    let mut text = String::new();
    if let Some(output) = body.get("output").and_then(Value::as_array) {
        for item in output {
            let Some(parts) = item.get("content").and_then(Value::as_array) else {
                continue;
            };
            for part in parts {
                if let Some(chunk) = part.get("text").and_then(Value::as_str) {
                    text.push_str(chunk);
                }
            }
        }
        return text;
    }
    if let Some(choices) = body.get("choices").and_then(Value::as_array) {
        for choice in choices {
            let Some(message) = choice.get("message") else {
                continue;
            };
            for field in ["reasoning_content", "reasoning", "content"] {
                if let Some(chunk) = message.get(field).and_then(Value::as_str) {
                    text.push_str(chunk);
                }
            }
        }
    }
    text
}

/// Runs one behavioral-tiebreaker generation with `effort` written into
/// `transport`'s field, at a fixed low temperature/seed and a token budget
/// large enough for the model's own reasoning trace to show through. Returns
/// `None` unless the result is actually usable evidence: a clean 2xx, a
/// parseable body, and non-empty extracted text. A transport/network
/// failure, a non-2xx status, or an empty/unparseable result all collapse to
/// `None` — the caller must never treat a failed or empty comparison as
/// proof of anything either way.
async fn probe_effort_behavior_text(
    client: &reqwest::Client,
    v1: &str,
    api_key: Option<&str>,
    model: &str,
    wire: WireFormat,
    transport: ReasoningEffortTransport,
    effort: &str,
) -> Option<String> {
    let (url, mut body) = match wire {
        WireFormat::Responses => (
            format!("{v1}/responses"),
            build_responses_body(EFFORT_BEHAVIOR_PROMPT, model),
        ),
        WireFormat::Chat => (
            format!("{v1}/chat/completions"),
            build_chat_body(EFFORT_BEHAVIOR_PROMPT, model),
        ),
    };
    match wire {
        WireFormat::Responses => body["max_output_tokens"] = json!(EFFORT_BEHAVIOR_MAX_TOKENS),
        WireFormat::Chat => body["max_tokens"] = json!(EFFORT_BEHAVIOR_MAX_TOKENS),
    }
    body["temperature"] = json!(EFFORT_BEHAVIOR_TEMPERATURE);
    body["seed"] = json!(EFFORT_BEHAVIOR_SEED);
    apply_effort_field(&mut body, transport, effort);
    let timeout = streaming_probe_timeout(v1);
    let (status, response_body, _) =
        post_json_with_timeout(client, &url, api_key, body, Some(timeout))
            .await
            .ok()?;
    if !(200..300).contains(&status) || response_body.is_null() {
        return None;
    }
    let text = extract_effort_probe_text(&response_body);
    (!text.trim().is_empty()).then_some(text)
}

/// Whether `transport`'s Effort field is *confirmed*, by content, to change
/// generation — used only when the status-code negative control could not
/// tell (the provider returned 2xx for a deliberately invalid value too —
/// see `EFFORT_NEGATIVE_CONTROL`). Jinja-templated backends like llama.cpp
/// never reject an unrecognized value; they silently fall back to a default
/// level. So instead of a second status check, this runs an A/B/A sequence
/// at fixed low temperature/seed — negative control, `strongest_candidate`,
/// negative control again — and only trusts the A-vs-B diff once the two A
/// runs agree with each other: an unstable control (the provider is ignoring
/// `seed`, or is otherwise non-deterministic even at low temperature) makes
/// any single diff meaningless, so it never gets treated as a verdict.
///
/// Returns `true` only when the control is stable AND genuinely differs from
/// the real candidate — that is the *only* case this proves anything.
/// Everything else — an unstable control, a request that failed to produce
/// usable evidence, or identical output even from a stable control — returns
/// `false` and must read as `Indeterminate`, never `Unsupported`: a
/// behavioral non-difference is weak evidence (a trivial prompt, a model
/// that happens to converge regardless of effort, ...), nowhere near as
/// reliable as a real validated-and-rejected HTTP response, so it may never
/// carry the same "confirmed" weight `Unsupported` implies.
async fn effort_field_is_confirmed_effective(
    client: &reqwest::Client,
    v1: &str,
    api_key: Option<&str>,
    model: &str,
    wire: WireFormat,
    transport: ReasoningEffortTransport,
    strongest_candidate: &str,
) -> bool {
    let Some(control_first) = probe_effort_behavior_text(
        client,
        v1,
        api_key,
        model,
        wire,
        transport,
        EFFORT_NEGATIVE_CONTROL,
    )
    .await
    else {
        return false;
    };
    let Some(real) = probe_effort_behavior_text(
        client,
        v1,
        api_key,
        model,
        wire,
        transport,
        strongest_candidate,
    )
    .await
    else {
        return false;
    };
    let Some(control_second) = probe_effort_behavior_text(
        client,
        v1,
        api_key,
        model,
        wire,
        transport,
        EFFORT_NEGATIVE_CONTROL,
    )
    .await
    else {
        return false;
    };
    if control_first.trim() != control_second.trim() {
        // The control group itself is not stable/repeatable (seed ignored,
        // or the backend is inherently non-deterministic) -- no A-vs-B diff
        // under these conditions is trustworthy.
        return false;
    }
    control_first.trim() != real.trim()
}

/// Probe which Effort levels a model accepts, on whichever transport(s) its
/// wire supports. A deliberately-invalid negative-control value rides along
/// with every real candidate: a transport whose negative control comes back
/// `Inconclusive` (noise — 401/403/429/5xx/timeout) cannot be trusted, so
/// that transport's "accepted" real candidates are discarded rather than
/// reported as verified support. A transport whose negative control comes
/// back cleanly `Rejected` is trusted outright, at which point its accepted
/// real candidates are the model's genuine supported levels. A transport
/// whose negative control comes back `Accepted` gets one more chance before
/// being discarded: some backends (llama.cpp, vLLM — anything proxying a raw
/// Jinja chat template) never reject an unrecognized value, they silently
/// fall back to a default level, so a `2xx` here proves nothing about
/// support either way. For that case only, `effort_field_is_confirmed_effective`
/// runs an A/B/A sequence (negative control, strongest accepted real
/// candidate, negative control again) and only trusts the diff once the two
/// control runs agree with each other. Genuinely different output from a
/// stable control is trusted as real support; every other outcome —
/// identical output, an unstable control, or a request that produced no
/// usable evidence — stays `Indeterminate`, never `Unsupported`: a
/// behavioral non-difference is weaker evidence than a real validated HTTP
/// rejection, so it may never carry that same "confirmed" weight.
async fn probe_reasoning_efforts(
    client: &reqwest::Client,
    v1: &str,
    api_key: Option<&str>,
    model: &str,
    wire: WireFormat,
    metadata: EffortMetadata,
) -> EffortProbeOutcome {
    let provider_declared_order = !metadata.levels.is_empty();
    let candidates = if provider_declared_order {
        metadata.levels.clone()
    } else {
        EFFORT_CANONICAL_ORDER
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
    };
    // `ProviderSpecific` (`chat_template_kwargs.reasoning_effort`) is tried on
    // both wires: llama.cpp/vLLM-style Jinja-templated deployments read it
    // regardless of whether the surrounding request is Chat- or
    // Responses-shaped, since it rides in a top-level field neither wire
    // otherwise touches.
    let transports: &[ReasoningEffortTransport] = match wire {
        WireFormat::Responses => &[
            ReasoningEffortTransport::ResponsesObject,
            ReasoningEffortTransport::ProviderSpecific,
        ],
        WireFormat::Chat => &[
            ReasoningEffortTransport::ChatField,
            ReasoningEffortTransport::ChatObject,
            ReasoningEffortTransport::ProviderSpecific,
        ],
    };
    // The most informative outcome seen across every transport, kept in case
    // no transport reaches `Supported`: a clean `Unsupported` from one
    // transport outranks a merely `Indeterminate` verdict from another,
    // regardless of which transport was tried first.
    fn prefer(
        existing: Option<EffortProbeOutcome>,
        candidate: EffortProbeOutcome,
    ) -> Option<EffortProbeOutcome> {
        match &existing {
            Some(EffortProbeOutcome::Unsupported) => existing,
            _ => Some(candidate),
        }
    }
    let mut best_negative: Option<EffortProbeOutcome> = None;
    for transport in transports {
        let mut probe_values = candidates.clone();
        probe_values.push(EFFORT_NEGATIVE_CONTROL.to_string());
        // Sequential, not concurrent: a single-slot local server (llama.cpp
        // with one worker) does not queue overlapping inference requests, it
        // stalls them — verified live against a real single-slot deployment,
        // where three simultaneous candidates each timed out with no
        // response at all, while the same requests sent one at a time
        // succeeded every time. A remote multi-slot API pays a few hundred
        // extra milliseconds for this; a local single-slot one goes from
        // "can never finish this probe" to "works".
        let outcomes = stream::iter(probe_values.iter().cloned().map(|effort| {
            let transport = *transport;
            async move {
                let outcome =
                    probe_effort_value(client, v1, api_key, model, wire, transport, &effort).await;
                (effort, outcome)
            }
        }))
        .buffer_unordered(1)
        .collect::<HashMap<_, _>>()
        .await;
        let negative_control_outcome = outcomes
            .get(EFFORT_NEGATIVE_CONTROL)
            .copied()
            .unwrap_or(EffortCandidateOutcome::Inconclusive);
        // Preserve `candidates`' own order (canonical, or the provider's
        // declared catalog order) rather than the concurrent completion order
        // `buffer_unordered` produced.
        let build_supported = |accepted_set: &std::collections::HashSet<&str>| {
            let levels = candidates
                .iter()
                .filter(|candidate| accepted_set.contains(candidate.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            let default = metadata
                .default
                .clone()
                .filter(|value| levels.iter().any(|level| level == value));
            EffortProbeOutcome::Supported {
                levels,
                default,
                transport: *transport,
            }
        };
        if negative_control_outcome != EffortCandidateOutcome::Rejected {
            // A clean 4xx/5xx/timeout on the negative control itself proves
            // nothing either way — that is ordinary noise, not evidence the
            // field is ignored, so only `Accepted` is worth a behavioral
            // tiebreaker. Status-blind backends (llama.cpp/vLLM-style Jinja
            // templates) return 2xx for any value, valid or not, so the
            // status check alone cannot tell "ignored" from "genuinely
            // accepted" here — only a content diff can.
            if negative_control_outcome == EffortCandidateOutcome::Accepted {
                let accepted_set: std::collections::HashSet<&str> = candidates
                    .iter()
                    .filter(|candidate| {
                        outcomes.get(candidate.as_str()) == Some(&EffortCandidateOutcome::Accepted)
                    })
                    .map(String::as_str)
                    .collect();
                let strongest = candidates
                    .iter()
                    .rev()
                    .find(|candidate| accepted_set.contains(candidate.as_str()));
                if let Some(strongest) = strongest {
                    if effort_field_is_confirmed_effective(
                        client, v1, api_key, model, wire, *transport, strongest,
                    )
                    .await
                    {
                        return build_supported(&accepted_set);
                    }
                    // No confirmed difference (identical output, an unstable
                    // control, or a request that produced no usable
                    // evidence) — falls through to the ordinary
                    // Indeterminate handling below. Never `Unsupported`: see
                    // `effort_field_is_confirmed_effective`.
                }
            }
            let issue = if negative_control_outcome == EffortCandidateOutcome::Accepted {
                "provider_ignores_unknown_effort"
            } else {
                "provider_error"
            };
            best_negative = prefer(best_negative, EffortProbeOutcome::Indeterminate(issue));
            continue;
        }
        let accepted_set: std::collections::HashSet<&str> = candidates
            .iter()
            .filter(|candidate| outcomes.get(*candidate) == Some(&EffortCandidateOutcome::Accepted))
            .map(String::as_str)
            .collect();
        if !accepted_set.is_empty() {
            return build_supported(&accepted_set);
        }
        let all_real_candidates_settled = candidates.iter().all(|candidate| {
            matches!(
                outcomes.get(candidate),
                Some(EffortCandidateOutcome::Rejected)
            )
        });
        if all_real_candidates_settled {
            best_negative = prefer(best_negative, EffortProbeOutcome::Unsupported);
        } else {
            best_negative = prefer(
                best_negative,
                EffortProbeOutcome::Indeterminate("provider_error"),
            );
        }
    }
    best_negative.unwrap_or(EffortProbeOutcome::Indeterminate("provider_error"))
}

impl StreamingProbe {
    fn as_capability(self) -> Option<bool> {
        match self {
            Self::Supported => Some(true),
            Self::Unsupported => Some(false),
            Self::Timeout => None,
        }
    }

    fn probe_issue(self) -> Option<&'static str> {
        matches!(self, Self::Timeout).then_some(PROBE_ISSUE_TIMEOUT)
    }
}

fn streaming_probe_timeout(v1: &str) -> std::time::Duration {
    #[cfg(test)]
    {
        let millis = TEST_STREAMING_TIMEOUT_MS.load(std::sync::atomic::Ordering::SeqCst);
        if millis > 0 {
            return std::time::Duration::from_millis(millis);
        }
    }
    std::time::Duration::from_secs(if opencode_zen_catalog(v1).is_some() {
        OPENCODE_INFERENCE_TIMEOUT_SECS
    } else {
        STREAMING_PROBE_TIMEOUT_SECS
    })
}

#[cfg(test)]
static TEST_STREAMING_TIMEOUT_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn first_chunk_looks_like_sse(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("data:")
        || trimmed.starts_with("event:")
        || trimmed.contains("\ndata:")
        || trimmed.contains("chat.completion.chunk")
        || trimmed.contains("response.created")
        || trimmed.contains("response.output_text.delta")
}

async fn probe_streaming(
    client: &reqwest::Client,
    v1: &str,
    api_key: Option<&str>,
    model: &str,
    wire: WireFormat,
) -> StreamingProbe {
    probe_streaming_with_timeout(
        client,
        v1,
        api_key,
        model,
        wire,
        streaming_probe_timeout(v1),
    )
    .await
}

async fn probe_streaming_with_timeout(
    client: &reqwest::Client,
    v1: &str,
    api_key: Option<&str>,
    model: &str,
    wire: WireFormat,
    timeout: std::time::Duration,
) -> StreamingProbe {
    let (url, mut body) = match wire {
        WireFormat::Responses => (format!("{v1}/responses"), build_responses_body(".", model)),
        WireFormat::Chat => (
            format!("{v1}/chat/completions"),
            build_chat_body(".", model),
        ),
    };
    body["stream"] = Value::Bool(true);
    if wire == WireFormat::Chat {
        apply_chat_probe_options(v1, model, &mut body);
    }
    // Do not use `post_json` here. An SSE response is not a JSON document and
    // may remain open; attempting `resp.json()` waits for EOF and can hit the
    // global probe timeout even though the server already accepted streaming.
    let mut request = client
        .post(url)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .timeout(timeout)
        .json(&body);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response = match request.send().await {
        Ok(response) => response,
        // A missed deadline or dropped connect is not evidence the model
        // lacks SSE. Persist `streaming: unknown` + `probeIssue: timeout`.
        Err(_) => return StreamingProbe::Timeout,
    };
    if !response.status().is_success() {
        return StreamingProbe::Unsupported;
    }
    if is_sse_content_type(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
    ) {
        // Headers already prove the server accepted `stream: true`. Do not
        // wait for `[DONE]` — a thinking model can stream tokens for minutes.
        return StreamingProbe::Supported;
    }
    // Some gateways omit the SSE content-type but still emit legal chunks.
    let mut source = response.bytes_stream();
    let mut text = String::new();
    while let Some(chunk) = source.next().await {
        let Ok(chunk) = chunk else {
            return StreamingProbe::Unsupported;
        };
        text.push_str(&String::from_utf8_lossy(&chunk));
        if text.len() > 128 * 1024 {
            return StreamingProbe::Unsupported;
        }
        if first_chunk_looks_like_sse(&text) {
            return StreamingProbe::Supported;
        }
        if text.contains('{') && !first_chunk_looks_like_sse(&text) {
            return StreamingProbe::Unsupported;
        }
    }
    StreamingProbe::Unsupported
}

async fn post_json(
    client: &reqwest::Client,
    url: &str,
    api_key: Option<&str>,
    body: Value,
) -> AppResult<(u16, Value, Option<String>)> {
    post_json_with_timeout(client, url, api_key, body, None).await
}

/// `timeout` overrides the client-wide probe budget for a single request. Only
/// the reasoning tool-probe retry needs it; everything else stays on the short
/// default so an unresponsive endpoint still fails fast.
async fn post_json_with_timeout(
    client: &reqwest::Client,
    url: &str,
    api_key: Option<&str>,
    body: Value,
    timeout: Option<std::time::Duration>,
) -> AppResult<(u16, Value, Option<String>)> {
    let mut req = client.post(url).json(&body);
    if let Some(k) = api_key {
        req = req.bearer_auth(k);
    }
    if let Some(timeout) = timeout {
        req = req.timeout(timeout);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| AppError::Unreachable(e.to_string()))?;
    let status = resp.status().as_u16();
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // Always attempt to parse the body, not only on 2xx: callers that only
    // care about status keep working (a parse failure still falls back to
    // Null), and callers that need to distinguish *why* a request was
    // rejected — like chat_tool_choice_rejected — can now actually read the
    // error body instead of it being silently discarded.
    let body = resp.json::<Value>().await.unwrap_or(Value::Null);
    Ok((status, body, ct))
}

/// 預設 reqwest client：rustls、短 timeout、不跟隨重導。
pub fn default_client() -> AppResult<reqwest::Client> {
    reqwest::Client::builder()
        // The generous budget is the default because every POST this module
        // makes runs inference. The two list endpoints opt back down to
        // `reachability_timeout()`, and they are what decides "unreachable".
        .timeout(std::time::Duration::from_secs(
            COMPLETION_PROBE_TIMEOUT_SECS,
        ))
        .redirect(reqwest::redirect::Policy::none())
        // Several OpenAI-compatible gateways sit behind Cloudflare and reject
        // reqwest's empty/default fingerprint with Error 1010. The real Codex
        // traffic carries a codex_cli_rs UA, so keep the capability probe on
        // the same accepted client class instead of falsely marking the route
        // unreachable or non-streaming.
        .user_agent(concat!(
            "codex_cli_rs/",
            env!("CARGO_PKG_VERSION"),
            " (vellum-provider-probe)"
        ))
        .build()
        .map_err(|e| AppError::Message(format!("無法建立 HTTP client：{e}")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::assertions_on_constants)]

    use super::*;

    #[test]
    fn initial_completion_budget_covers_observed_qwen_cold_start() {
        assert!(COMPLETION_PROBE_TIMEOUT_SECS >= 60);
        assert!(COMPLETION_PROBE_TIMEOUT_SECS > TOOL_PROBE_TIMEOUT_SECS);
        assert_eq!(TOOL_PROBE_TOTAL_BUDGET_SECS, 2 * TOOL_PROBE_TIMEOUT_SECS);
    }

    #[test]
    fn chat_capability_merge_keeps_existing_on_inconclusive() {
        let existing = RuntimeChatCapabilities {
            continuation_tail: ContinuationTail::NeutralUserBridge,
            reasoning_replay: ReasoningReplay::ToolCallBound,
        };
        let merged = merge_chat_capability_observations(
            &existing,
            ContinuationTailObservation::Inconclusive,
            ReasoningReplayObservation::Inconclusive,
        );
        assert_eq!(merged, existing);
        let timeout_like = merge_chat_capability_observations(
            &existing,
            ContinuationTailObservation::Inconclusive,
            ReasoningReplayObservation::Inconclusive,
        );
        assert_eq!(
            timeout_like.continuation_tail,
            ContinuationTail::NeutralUserBridge
        );
        assert_eq!(
            timeout_like.reasoning_replay,
            ReasoningReplay::ToolCallBound
        );
    }

    #[test]
    fn chat_capability_merge_persists_confirmed_observations() {
        let existing = RuntimeChatCapabilities {
            continuation_tail: ContinuationTail::NeutralUserBridge,
            reasoning_replay: ReasoningReplay::ToolCallBound,
        };
        let merged = merge_chat_capability_observations(
            &existing,
            ContinuationTailObservation::ConfirmedNative,
            ReasoningReplayObservation::ConfirmedNone,
        );
        assert_eq!(merged.continuation_tail, ContinuationTail::NativeToolResult);
        assert_eq!(merged.reasoning_replay, ReasoningReplay::None);
    }

    #[test]
    fn reasoning_continuation_classifies_need_not_mere_mention() {
        assert_eq!(
            classify_reasoning_continuation(200, ""),
            ReasoningContinuationClass::Success
        );
        assert_eq!(
            classify_reasoning_continuation(
                400,
                "reasoning_content in assistant message that follows tool calls must be passed back"
            ),
            ReasoningContinuationClass::MissingReasoning
        );
        assert_eq!(
            classify_reasoning_continuation(
                400,
                "unknown field reasoning_content is not supported"
            ),
            ReasoningContinuationClass::UnsupportedShape
        );
        assert_eq!(
            classify_reasoning_continuation(400, "invalid reasoning_content"),
            ReasoningContinuationClass::Inconclusive
        );
        assert_eq!(
            classify_reasoning_continuation(429, "reasoning quota"),
            ReasoningContinuationClass::Inconclusive
        );
        assert_eq!(
            classify_reasoning_continuation(503, "reasoning unavailable"),
            ReasoningContinuationClass::Inconclusive
        );
    }

    #[test]
    fn chat_probe_tool_message_requires_a_real_tool_call() {
        assert!(
            chat_probe_tool_message(&json!({"choices":[{"message":{"content":"ok"}}]})).is_none()
        );
        let (message, reasoning) = chat_probe_tool_message(&json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "reasoning_content": "need the probe tool",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "vellum_probe_tool", "arguments": "{\"value\":\"ok\"}"}
                    }]
                }
            }]
        }))
        .unwrap();
        assert_eq!(reasoning.as_deref(), Some("need the probe tool"));
        let without = continuation_messages_without_reasoning(
            &json!({"role":"user","content":"go"}),
            &message,
        );
        assert!(without[1].get("reasoning_content").is_none());
        let with = with_provider_reasoning(&without, "need the probe tool");
        assert_eq!(with[1]["reasoning_content"], "need the probe tool");
    }

    #[test]
    fn known_windows_for_common_models() {
        assert_eq!(known_context_window("grok-4.5"), Some(500_000));
        assert_eq!(known_context_window("Grok-4.5-Mini"), Some(500_000));
        assert_eq!(known_context_window("grok-3"), Some(131_072));
        assert_eq!(known_context_window("gpt-5.6-sol"), Some(200_000));
        assert_eq!(known_context_window("claude-opus-4"), Some(200_000));
        assert_eq!(known_context_window("mystery-model"), None);
    }

    #[test]
    fn known_reasoning_fallback_is_conservative() {
        assert!(known_reasoning_capability("GLM-5.2"));
        assert!(known_reasoning_capability("DeepSeek-V4-Flash"));
        assert!(known_reasoning_capability("nemotron-3-ultra"));
        assert!(known_reasoning_capability("grok-4.5"));
        assert!(!known_reasoning_capability("MiMo-V2.5-Pro"));
        assert!(!known_reasoning_capability("unknown-chat-model"));
        assert!(provider_has_known_reasoning_model(&[
            "MiMo-V2.5-Pro".into(),
            "GLM-5.2".into()
        ]));
        assert!(!provider_has_known_reasoning_model(&[
            "MiMo-V2.5-Pro".into(),
            "unknown-chat-model".into()
        ]));
    }

    #[test]
    fn opencode_zen_is_recognized_only_at_its_official_api_root() {
        assert!(is_opencode_zen_endpoint(OPENCODE_ZEN_BASE_URL));
        assert!(is_opencode_zen_endpoint("https://opencode.ai/zen/v1/"));
        // Pasting the documented models endpoint is normalized back to the
        // provider root before preset detection.
        assert!(is_opencode_zen_endpoint(
            "https://opencode.ai/zen/v1/models"
        ));
        assert!(!is_opencode_zen_endpoint("https://example.test/zen/v1"));
    }

    #[test]
    fn opencode_zen_assigns_each_model_to_its_documented_protocol() {
        assert_eq!(
            opencode_zen_wire_for_model("gpt-5.6-sol"),
            Some(WireFormat::Responses)
        );
        assert_eq!(
            opencode_zen_wire_for_model("grok-4.5"),
            Some(WireFormat::Responses)
        );
        assert_eq!(
            opencode_zen_wire_for_model("glm-5.2"),
            Some(WireFormat::Chat)
        );
        assert_eq!(
            opencode_zen_wire_for_model("deepseek-v4-flash-free"),
            Some(WireFormat::Chat)
        );
        assert_eq!(opencode_zen_wire_for_model("claude-opus-5"), None);
        assert_eq!(opencode_zen_wire_for_model("gemini-3.6-flash"), None);
        assert_eq!(opencode_zen_wire_for_model("qwen3.6-plus"), None);
    }

    #[test]
    fn opencode_zen_known_free_uses_the_real_catalog_flag_not_a_naming_guess() {
        // Ground truth from OpenCode's own model registry, not a naming
        // convention: `-free` id suffixes happen to line up for these, but
        // the lookup is a real per-id `cost == 0` flag, and `gpt-5-nano` was
        // never free despite once being treated as a fallback free pick.
        assert_eq!(
            opencode_zen_known_free(OpenCodeZenCatalog::Full, "big-pickle"),
            Some(true)
        );
        assert_eq!(
            opencode_zen_known_free(OpenCodeZenCatalog::Full, "deepseek-v4-flash-free"),
            Some(true)
        );
        assert_eq!(
            opencode_zen_known_free(OpenCodeZenCatalog::Full, "Laguna-S-2.1-Free"),
            Some(true)
        );
        assert_eq!(
            opencode_zen_known_free(OpenCodeZenCatalog::Full, "gpt-5-nano"),
            Some(false)
        );
        assert_eq!(
            opencode_zen_known_free(OpenCodeZenCatalog::Full, "deepseek-v4-flash"),
            Some(false)
        );
        // OpenCode Go carries no free models at all — every one of its 24
        // models has a real per-token cost.
        assert_eq!(
            opencode_zen_known_free(OpenCodeZenCatalog::Go, "deepseek-v4-flash"),
            Some(false)
        );
        // An id absent from both tables is unknown, never guessed from a suffix.
        assert_eq!(
            opencode_zen_known_free(OpenCodeZenCatalog::Full, "some-new-model-free"),
            None
        );
    }

    /// The case this predicate exists for. OpenCode withdrew
    /// `deepseek-v4-flash-free`; models.dev still reports it at zero cost and
    /// additionally marks it `status: deprecated`. A free-tier catalog that
    /// reads only the price keeps offering a model the provider has retired.
    #[test]
    fn a_retired_zero_cost_model_is_not_on_the_free_tier() {
        let catalog = OpenCodeZenCatalog::Full;
        // Still free, by price.
        assert_eq!(
            opencode_zen_known_free(catalog, "deepseek-v4-flash-free"),
            Some(true)
        );
        // ...and still withdrawn, so not offered.
        assert!(!opencode_zen_free_tier_model(
            catalog,
            "deepseek-v4-flash-free"
        ));
        // A free model that is current stays on the tier.
        assert!(opencode_zen_free_tier_model(catalog, "big-pickle"));
        // Paid models were never on it.
        assert!(!opencode_zen_free_tier_model(catalog, "glm-5.2"));
    }

    #[test]
    fn opencode_zen_probe_candidates_prefer_free_current_models_before_paid_or_deprecated_ones() {
        // The annotations below are read from `OPENCODE_ZEN_MODEL_METADATA`,
        // not asserted from a naming convention -- `-free` in an id says
        // nothing, and a model's deprecation flag moves whenever
        // `scripts/refresh-opencode-catalog.mjs` picks up the upstream
        // registry again. When this expectation changes after a refresh,
        // check the table before touching the ordering: it is far more
        // likely that a model was deprecated upstream than that the
        // ordering rule broke.
        let models = vec![
            "claude-opus-5".to_string(),
            "gpt-5.6-sol".to_string(),
            "glm-5-free".to_string(),             // free but deprecated
            "deepseek-v4-flash-free".to_string(), // free but deprecated
            "big-pickle".to_string(),             // free and current
            "glm-5.2".to_string(),                // paid and current
        ];
        // claude-opus-5 is dropped entirely: it has no wire Vellum can speak.
        // Free + current models come first (catalog order preserved), then
        // free-but-deprecated (also in catalog order), then paid.
        assert_eq!(
            opencode_zen_probe_candidates(OpenCodeZenCatalog::Full, &models),
            vec![
                "big-pickle",
                "glm-5-free",
                "deepseek-v4-flash-free",
                "gpt-5.6-sol",
                "glm-5.2",
            ],
        );
    }

    #[test]
    fn opencode_zen_metadata_is_scoped_per_catalog_not_merged() {
        assert_eq!(
            opencode_zen_known_context_window(OpenCodeZenCatalog::Full, "glm-5.2"),
            Some(1_000_000)
        );
        assert_eq!(
            opencode_zen_known_context_window(OpenCodeZenCatalog::Full, "kimi-k3"),
            Some(1_048_576)
        );
        // A `-free` id is its own catalog row, not an alias of the paid
        // sibling — Zen genuinely serves it with a smaller context window.
        assert_eq!(
            opencode_zen_known_context_window(OpenCodeZenCatalog::Full, "deepseek-v4-flash-free"),
            Some(200_000)
        );
        assert_eq!(
            opencode_zen_known_context_window(OpenCodeZenCatalog::Full, "deepseek-v4-flash"),
            Some(1_000_000)
        );
        assert!(opencode_zen_known_reasoning(
            OpenCodeZenCatalog::Full,
            "mimo-v2.5-free"
        ));
        assert_eq!(
            opencode_zen_known_context_window(OpenCodeZenCatalog::Full, "unknown-zen-model"),
            None
        );
        // Full and Go disagree on this id's context window (1M vs 512K); each
        // catalog must return its own number, not the other one's.
        assert_eq!(
            opencode_zen_known_context_window(OpenCodeZenCatalog::Full, "minimax-m3"),
            Some(512_000)
        );
        assert_eq!(
            opencode_zen_known_context_window(OpenCodeZenCatalog::Go, "minimax-m3"),
            Some(1_000_000)
        );
        // hy3 exists only in Go's table; Full falls back to it rather than
        // reporting unknown, since it does not have its own conflicting row.
        assert_eq!(
            opencode_zen_known_context_window(OpenCodeZenCatalog::Full, "hy3"),
            Some(256_000)
        );
        assert_eq!(
            opencode_zen_known_context_window(OpenCodeZenCatalog::Full, "hy3-free"),
            Some(190_000)
        );
    }

    #[test]
    fn opencode_zen_catalog_distinguishes_the_full_and_go_endpoints() {
        assert_eq!(
            opencode_zen_catalog(OPENCODE_ZEN_BASE_URL),
            Some(OpenCodeZenCatalog::Full)
        );
        assert_eq!(
            opencode_zen_catalog(OPENCODE_GO_BASE_URL),
            Some(OpenCodeZenCatalog::Go)
        );
        assert_eq!(opencode_zen_catalog("https://example.test/zen/v1"), None);
    }

    #[tokio::test]
    async fn opencode_focused_probe_requires_a_key_for_paid_models() {
        let error = probe_model_capability_with_client(
            &default_client().unwrap(),
            OPENCODE_ZEN_BASE_URL,
            None,
            "gpt-5.4-mini",
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("requires an API key"));
    }

    #[test]
    fn opencode_free_models_do_not_require_a_private_key() {
        let token =
            opencode_model_probe_token(OPENCODE_ZEN_BASE_URL, "mimo-v2.5-free", None).unwrap();
        assert_eq!(token, Some(vellum_proxy_runtime::OPENCODE_PUBLIC_TOKEN));
        let token =
            opencode_model_probe_token(OPENCODE_ZEN_BASE_URL, "mimo-v2.5-free", Some("sk-private"))
                .unwrap();
        assert_eq!(token, Some(vellum_proxy_runtime::OPENCODE_PUBLIC_TOKEN));
        assert!(opencode_zen_known_free(OpenCodeZenCatalog::Full, "mystery-free").is_none());
        assert_eq!(
            opencode_zen_known_free(OpenCodeZenCatalog::Full, "mimo-v2.5-free"),
            Some(true)
        );
    }

    #[tokio::test]
    async fn opencode_zen_catalog_assigns_the_confirmed_display_name_and_leaves_the_routing_id_alone(
    ) {
        // The selected-model re-probe path (`probe_opencode_zen_catalog`)
        // must surface `x-preview-f-free`'s real catalog name ("Ox Alpha
        // Free") the same way initial discovery does -- and must never let
        // that friendly name leak into `model`, which routing and
        // persistence key off unconditionally.
        let result = probe_opencode_zen_catalog(
            &default_client().unwrap(),
            "http://127.0.0.1:1/v1",
            OpenCodeZenCatalog::Full,
            None,
            vec!["x-preview-f-free".into(), "big-pickle".into()],
            None,
            HashMap::new(),
            HashMap::new(),
        )
        .await
        .unwrap();

        let ox = result
            .model_capabilities
            .iter()
            .find(|capability| capability.model == "x-preview-f-free")
            .unwrap();
        assert_eq!(ox.display_name.as_deref(), Some("Ox Alpha Free"));
        assert_eq!(ox.model, "x-preview-f-free");

        // A model with no confirmed catalog name shows the upstream id
        // unchanged, exactly as before this feature.
        let pickle = result
            .model_capabilities
            .iter()
            .find(|capability| capability.model == "big-pickle")
            .unwrap();
        assert_eq!(pickle.display_name, None);
    }

    #[tokio::test]
    async fn opencode_catalog_probes_every_wire_supported_model_and_only_placeholders_the_rest() {
        use axum::extract::Json;
        use axum::http::{header, HeaderMap};
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;

        async fn chat(headers: HeaderMap, Json(body): Json<Value>) -> Response {
            if body["tool_choice"] == "required" {
                return Json(json!({
                    "choices": [{"message": {"content": null, "tool_calls": [{
                        "id": "call_probe",
                        "type": "function",
                        "function": {
                            "name": "vellum_probe_tool",
                            "arguments": "{\"value\":\"ok\"}"
                        }
                    }]}}]
                }))
                .into_response();
            }
            if body["stream"] == true
                && headers
                    .get(header::ACCEPT)
                    .and_then(|value| value.to_str().ok())
                    == Some("text/event-stream")
            {
                return (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
                )
                    .into_response();
            }
            Json(json!({"choices": [{"message": {"content": "ok"}}]})).into_response()
        }

        // gpt-5.4-mini resolves to the Responses wire (see
        // opencode_zen_wire_for_model), so it needs its own route now that
        // every wire-supported candidate is actually probed, not just the
        // free one.
        async fn responses(headers: HeaderMap, Json(body): Json<Value>) -> Response {
            if body["tool_choice"] == "required" {
                return Json(json!({"output": [{
                    "type": "function_call", "name": "vellum_probe_tool",
                    "arguments": "{\"value\":\"ok\"}"
                }]}))
                .into_response();
            }
            if body["stream"] == true
                && headers
                    .get(header::ACCEPT)
                    .and_then(|value| value.to_str().ok())
                    == Some("text/event-stream")
            {
                return (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n",
                )
                    .into_response();
            }
            Json(json!({
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "ok"}]}]
            }))
            .into_response()
        }

        let app = Router::new()
            .route("/v1/chat/completions", post(chat))
            .route("/v1/responses", post(responses));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let result = probe_opencode_zen_catalog(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            OpenCodeZenCatalog::Full,
            None,
            vec![
                "claude-opus-5".into(),
                "gpt-5.4-mini".into(),
                "big-pickle".into(),
            ],
            None,
            HashMap::new(),
            HashMap::new(),
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(result.wire, Some(WireFormat::Chat));
        let free = result
            .model_capabilities
            .iter()
            .find(|capability| capability.model == "big-pickle")
            .unwrap();
        assert_eq!(free.wire, Some(WireFormat::Chat));
        assert_eq!(free.tool_calling, None);
        assert_eq!(free.probe_version, None);
        assert_eq!(
            free.access_mode,
            Some(crate::model::AccessMode::AnonymousFree)
        );

        let gpt = result
            .model_capabilities
            .iter()
            .find(|capability| capability.model == "gpt-5.4-mini")
            .unwrap();
        assert_eq!(gpt.wire, Some(WireFormat::Responses));
        assert_eq!(gpt.tool_calling, None);
        assert_eq!(
            gpt.access_mode,
            Some(crate::model::AccessMode::Credentialed)
        );

        let claude = result
            .model_capabilities
            .iter()
            .find(|capability| capability.model == "claude-opus-5")
            .unwrap();
        assert_eq!(claude.wire, None);
        assert_eq!(claude.tool_calling, Some(false));
        assert_eq!(claude.probe_version, None);
    }

    /// Reproduces the bug: `big-pickle` accepts the plain request (so it used
    /// to satisfy the old "wire is some" check) but never returns a typed
    /// tool call. The catalog probe must not settle for it — it has to fall
    /// through to the next candidate that actually passes the Codex
    /// tool-calling probe, here `glm-5.2`, and must not leave `claude-opus-5`
    /// (unsupported protocol, never probed at all) selected as a side effect.
    #[tokio::test]
    async fn opencode_catalog_falls_through_when_the_free_model_cannot_call_tools() {
        use axum::extract::Json;
        use axum::http::{header, HeaderMap};
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;

        async fn chat(headers: HeaderMap, Json(body): Json<Value>) -> Response {
            let model = body["model"].as_str().unwrap_or_default().to_string();
            if body["tool_choice"] == "required" {
                if model == "big-pickle" {
                    // Accepts the request but answers in prose — a real
                    // free/coding model that does not support function calls.
                    return Json(json!({"choices": [{"message": {"content": "sure, done"}}]}))
                        .into_response();
                }
                return Json(json!({
                    "choices": [{"message": {"content": null, "tool_calls": [{
                        "id": "call_probe",
                        "type": "function",
                        "function": {
                            "name": "vellum_probe_tool",
                            "arguments": "{\"value\":\"ok\"}"
                        }
                    }]}}]
                }))
                .into_response();
            }
            if body["stream"] == true
                && headers
                    .get(header::ACCEPT)
                    .and_then(|value| value.to_str().ok())
                    == Some("text/event-stream")
            {
                return (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
                )
                    .into_response();
            }
            Json(json!({"choices": [{"message": {"content": "ok"}}]})).into_response()
        }

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let result = probe_opencode_zen_catalog(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            OpenCodeZenCatalog::Full,
            None,
            vec![
                "claude-opus-5".into(),
                "big-pickle".into(),
                "glm-5.2".into(),
            ],
            None,
            HashMap::new(),
            HashMap::new(),
        )
        .await
        .unwrap();
        server.abort();

        // glm-5.2, not big-pickle, is the one the catalog actually verified.
        assert_eq!(result.wire, Some(WireFormat::Chat));
        let glm = result
            .model_capabilities
            .iter()
            .find(|capability| capability.model == "glm-5.2")
            .unwrap();
        assert_eq!(glm.tool_calling, None);
        assert_eq!(glm.context_window, Some(1_000_000));

        let free = result
            .model_capabilities
            .iter()
            .find(|capability| capability.model == "big-pickle")
            .unwrap();
        assert_eq!(free.tool_calling, None);

        // claude-opus-5 was never a wire-eligible candidate, so it stays
        // unverified — it must never become the route's model.
        let claude = result
            .model_capabilities
            .iter()
            .find(|capability| capability.model == "claude-opus-5")
            .unwrap();
        assert_eq!(claude.wire, None);
        assert_eq!(claude.tool_calling, Some(false));
    }

    /// The point of probing every candidate instead of stopping at the first
    /// success: a user should never have to tick a checkbox and wait for an
    /// individual model probe after connecting. Every wire-supported model
    /// must have already made a real request by the time the catalog probe
    /// returns.
    #[tokio::test]
    async fn opencode_catalog_probes_every_candidate_not_only_the_first_to_succeed() {
        use axum::extract::Json;
        use axum::routing::post;
        use axum::Router;
        use std::sync::{Arc, Mutex};

        let seen_models = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let seen_for_handler = seen_models.clone();

        let chat = move |Json(body): Json<Value>| {
            let seen = seen_for_handler.clone();
            async move {
                if body["tool_choice"] == "required" {
                    if let Some(model) = body["model"].as_str() {
                        seen.lock().unwrap().insert(model.to_string());
                    }
                    return Json(json!({
                        "choices": [{"message": {"content": null, "tool_calls": [{
                            "id": "call_probe", "type": "function",
                            "function": {"name": "vellum_probe_tool", "arguments": "{\"value\":\"ok\"}"}
                        }]}}]
                    }));
                }
                Json(json!({"choices": [{"message": {"content": "ok"}}]}))
            }
        };

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let result = probe_opencode_zen_catalog(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            OpenCodeZenCatalog::Go,
            None,
            vec![
                "minimax-m3".into(),
                "minimax-m2.7".into(),
                "kimi-k3".into(),
                "glm-5.2".into(),
            ],
            None,
            HashMap::new(),
            HashMap::new(),
        )
        .await
        .unwrap();
        server.abort();

        let mut seen = seen_models
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        seen.sort();
        assert!(
            seen.is_empty(),
            "catalog connect must not issue completion probes, got {seen:?}"
        );
        assert_eq!(result.models.len(), 4);
        for model in ["minimax-m3", "minimax-m2.7", "kimi-k3", "glm-5.2"] {
            let capability = result
                .model_capabilities
                .iter()
                .find(|capability| capability.model == model)
                .unwrap();
            assert_eq!(capability.tool_calling, None);
            assert_eq!(capability.probe_version, None);
        }
    }

    /// When every candidate fails the tool-calling probe, the connection must
    /// fail loudly instead of silently falling back to the catalog's first
    /// (possibly protocol-unsupported) model — the exact failure mode this
    /// fix replaces.
    #[tokio::test]
    async fn opencode_catalog_errors_when_no_candidate_passes_tool_calling() {
        use axum::extract::Json;
        use axum::routing::post;
        use axum::Router;

        async fn chat(Json(_body): Json<Value>) -> Json<Value> {
            Json(json!({"choices": [{"message": {"content": "no tools here"}}]}))
        }

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let error = probe_opencode_zen_catalog(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            OpenCodeZenCatalog::Full,
            None,
            vec!["claude-opus-5".into()],
            None,
            HashMap::new(),
            HashMap::new(),
        )
        .await
        .unwrap_err();
        server.abort();

        assert!(error.to_string().contains("no models Vellum can speak"));
    }

    /// Free models must render first — this is the ordering OpenCode's own
    /// model picker uses, and the point of the fix: a paid model no longer
    /// buries the free ones a user would actually want to pick first.
    #[tokio::test]
    async fn opencode_catalog_sorts_free_models_before_paid_ones() {
        use axum::extract::Json;
        use axum::routing::post;
        use axum::Router;

        async fn chat(Json(body): Json<Value>) -> Json<Value> {
            if body["tool_choice"] == "required" {
                return Json(json!({
                    "choices": [{"message": {"content": null, "tool_calls": [{
                        "id": "call_probe", "type": "function",
                        "function": {"name": "vellum_probe_tool", "arguments": "{\"value\":\"ok\"}"}
                    }]}}]
                }));
            }
            Json(json!({"choices": [{"message": {"content": "ok"}}]}))
        }

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let result = probe_opencode_zen_catalog(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            OpenCodeZenCatalog::Full,
            None,
            // Catalog order deliberately puts the paid model first and mixes
            // in an unsupported-protocol one — the sort must still put the
            // free model first and the unsupported one last.
            vec![
                "glm-5.2".into(),
                "claude-opus-5".into(),
                "deepseek-v4-flash-free".into(),
            ],
            None,
            HashMap::new(),
            HashMap::new(),
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(
            result.models,
            vec!["deepseek-v4-flash-free", "glm-5.2", "claude-opus-5"],
        );
        assert_eq!(
            result
                .model_capabilities
                .iter()
                .map(|capability| capability.model.as_str())
                .collect::<Vec<_>>(),
            vec!["deepseek-v4-flash-free", "glm-5.2", "claude-opus-5"],
        );
        let free_flags = result
            .model_capabilities
            .iter()
            .map(|capability| (capability.model.as_str(), capability.free))
            .collect::<Vec<_>>();
        assert_eq!(
            free_flags,
            vec![
                ("deepseek-v4-flash-free", Some(true)),
                ("glm-5.2", Some(false)),
                ("claude-opus-5", Some(false)),
            ],
        );
    }

    /// The Go catalog is a distinct, smaller endpoint: none of Full's premium
    /// (Claude/Gemini/GPT) models are candidates for it at all, and its own
    /// metadata table — not Full's — supplies context window.
    #[tokio::test]
    async fn opencode_go_catalog_uses_its_own_model_list_and_metadata() {
        use axum::extract::Json;
        use axum::routing::post;
        use axum::Router;

        async fn chat(Json(body): Json<Value>) -> Json<Value> {
            if body["tool_choice"] == "required" {
                return Json(json!({
                    "choices": [{"message": {"content": null, "tool_calls": [{
                        "id": "call_probe", "type": "function",
                        "function": {"name": "vellum_probe_tool", "arguments": "{\"value\":\"ok\"}"}
                    }]}}]
                }));
            }
            Json(json!({"choices": [{"message": {"content": "ok"}}]}))
        }

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let result = probe_opencode_zen_catalog(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            OpenCodeZenCatalog::Go,
            None,
            vec!["minimax-m3".into(), "hy3".into()],
            None,
            HashMap::new(),
            HashMap::new(),
        )
        .await
        .unwrap();
        server.abort();

        let minimax = result
            .model_capabilities
            .iter()
            .find(|capability| capability.model == "minimax-m3")
            .unwrap();
        // Go's own number (1M), not Full's (512K) for the same model id.
        assert_eq!(minimax.context_window, Some(1_000_000));
    }

    #[tokio::test]
    async fn opencode_selected_probe_stops_after_the_first_429() {
        use axum::extract::Json;
        use axum::http::StatusCode;
        use axum::routing::post;
        use axum::Router;
        use std::sync::{Arc, Mutex};

        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_for_handler = seen.clone();
        let chat = move |Json(body): Json<Value>| {
            let seen = seen_for_handler.clone();
            async move {
                if let Some(model) = body["model"].as_str() {
                    seen.lock().unwrap().push(model.to_string());
                }
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [("retry-after", "30")],
                    Json(json!({"error": {"message": "rate limited"}})),
                )
            }
        };
        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let results = probe_opencode_selected_models(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            Some(vellum_proxy_runtime::OPENCODE_PUBLIC_TOKEN),
            &["mimo-v2.5-free".into(), "big-pickle".into()],
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].probe_issue.as_deref(), Some("provider_quota"));
        assert_eq!(*seen.lock().unwrap(), vec!["mimo-v2.5-free".to_string()]);
    }

    #[test]
    fn responses_body_is_minimal() {
        let b = build_responses_body("hi", "GLM-5.2");
        assert_eq!(b["max_output_tokens"], PROBE_MAX_TOKENS);
        assert_eq!(b["input"], "hi");
        assert_eq!(b["model"], "GLM-5.2");
        assert_eq!(b["stream"], false);
        assert_eq!(b["store"], false);
    }

    #[test]
    fn responses_harness_body_exercises_private_codex_items_and_tools() {
        let body = build_responses_harness_body("Qwen3-Coder-Next");
        assert_eq!(body["model"], "Qwen3-Coder-Next");
        assert!(body["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "additional_tools"));
        assert_eq!(body["tools"][0]["type"], "function");
    }

    #[test]
    fn chat_body_is_minimal() {
        let b = build_chat_body("hi", "grok-4.5");
        assert_eq!(b["max_tokens"], PROBE_MAX_TOKENS);
        assert_eq!(b["messages"][0]["content"], "hi");
        assert_eq!(b["model"], "grok-4.5");
    }

    #[test]
    fn chat_probe_never_sends_an_empty_user_turn() {
        let body = build_chat_body("", "x-preview-f-free");
        assert_eq!(
            body["messages"],
            json!([{ "role": "user", "content": "ping" }])
        );
    }

    #[test]
    fn nvidia_reasoning_probe_uses_nim_chat_template_options() {
        let mut body = build_chat_body("", "nvidia/nemotron-3-super-120b-a12b");
        apply_chat_probe_options(
            "https://integrate.api.nvidia.com/v1",
            "nvidia/nemotron-3-super-120b-a12b",
            &mut body,
        );
        assert_eq!(body["temperature"], json!(1));
        assert_eq!(body["top_p"], json!(0.95));
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(body["reasoning_budget"], PROBE_MAX_TOKENS);

        let mut generic = build_chat_body(".", "nemotron-3-ultra");
        apply_chat_probe_options(
            "https://llm.example.test/v1",
            "nemotron-3-ultra",
            &mut generic,
        );
        assert!(generic.get("chat_template_kwargs").is_none());
    }

    #[tokio::test]
    async fn probe_records_capabilities_per_model_instead_of_copying_first_model() {
        use axum::extract::Json;
        use axum::http::{header, HeaderMap, StatusCode};
        use axum::response::{IntoResponse, Response};
        use axum::routing::{get, post};
        use axum::Router;

        async fn models() -> Json<Value> {
            Json(json!({
                "data": [
                    {"id": "reasoning-model", "context_window": 200_000},
                    {"id": "plain-model", "context_window": 32_000}
                ]
            }))
        }

        async fn chat(headers: HeaderMap, Json(body): Json<Value>) -> Response {
            if body["tool_choice"] == "required" {
                return Json(json!({
                    "choices": [{"message": {"content": null, "tool_calls": [{
                        "id": "call_probe", "type": "function",
                        "function": {"name": "vellum_probe_tool", "arguments": "{\"value\":\"ok\"}"}
                    }]}}]
                }))
                .into_response();
            }
            let model = body["model"].as_str().unwrap_or_default();
            if body["stream"] == true {
                if model == "reasoning-model"
                    && headers
                        .get(header::ACCEPT)
                        .and_then(|value| value.to_str().ok())
                        == Some("text/event-stream")
                {
                    return (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        "data: {\"choices\":[]}\n\ndata: [DONE]\n\n",
                    )
                        .into_response();
                }
                return Json(json!({"choices": []})).into_response();
            }
            let message = if model == "reasoning-model" {
                json!({"content": "ok", "reasoning_content": "thinking"})
            } else {
                json!({"content": "ok"})
            };
            (
                StatusCode::OK,
                Json(json!({"choices": [{"message": message}]})),
            )
                .into_response()
        }

        let app = Router::new()
            .route("/v1/models", get(models))
            .route("/v1/responses", post(|| async { StatusCode::NOT_FOUND }))
            .route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let result = probe_endpoint_with_client(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(result.wire, Some(WireFormat::Chat));
        assert_eq!(result.model_capabilities.len(), 2);
        let reasoning = result
            .model_capabilities
            .iter()
            .find(|item| item.model == "reasoning-model")
            .unwrap();
        assert_eq!(reasoning.context_window, Some(200_000));
        assert_eq!(reasoning.streaming, Some(true));
        assert_eq!(reasoning.reasoning, Some(true));
        let plain = result
            .model_capabilities
            .iter()
            .find(|item| item.model == "plain-model")
            .unwrap();
        assert_eq!(plain.context_window, Some(32_000));
        assert_eq!(plain.streaming, Some(false));
        assert_eq!(plain.reasoning, Some(false));
    }

    #[tokio::test]
    async fn catalog_discovery_does_not_run_inference() {
        use axum::extract::{Json, State};
        use axum::http::StatusCode;
        use axum::routing::{get, post};
        use axum::Router;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        async fn models() -> Json<Value> {
            Json(json!({"data": [
                {"id": "first-model", "context_window": 32_000},
                {"id": "second-model", "context_window": 64_000}
            ]}))
        }
        async fn inference(State(count): State<Arc<AtomicUsize>>) -> StatusCode {
            count.fetch_add(1, Ordering::SeqCst);
            StatusCode::INTERNAL_SERVER_ERROR
        }

        let count = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/v1/models", get(models))
            .route("/v1/responses", post(inference))
            .route("/v1/chat/completions", post(inference))
            .with_state(count.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let result = discover_endpoint_with_client(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(result.models, ["first-model", "second-model"]);
        assert_eq!(result.context_window, Some(32_000));
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert!(result
            .model_capabilities
            .iter()
            .all(|capability| capability.probe_version.is_none()));
        // A regular OpenAI-compatible endpoint has no catalog-level wire map
        // the way OpenCode Zen does, so `tool_calling` must stay unverified
        // (`None`) here, not `Some(false)`. `Some(false)` before the user
        // ever clicked "verify" used to permanently disable every model's
        // checkbox on every non-Zen provider, since a plain custom endpoint's
        // wire is only ever discovered later, per model that the user
        // actually attempts to verify.
        assert!(
            result
                .model_capabilities
                .iter()
                .all(|capability| capability.tool_calling.is_none()),
            "discovery must leave tool_calling unset for a non-OpenCode-Zen \
             endpoint: {:?}",
            result.model_capabilities
        );
    }

    /// The confirmed default this whole re-probe path exists to enforce: a
    /// Provider-level re-probe against a large marketplace catalog (stood in
    /// here by 8 models -- the real OpenCode Zen catalog is 60+) must only
    /// run inference against the route's currently *selected* models, never
    /// every model the catalog happens to list.
    #[tokio::test]
    async fn reprobe_selected_models_only_sends_inference_for_the_selection_not_the_whole_catalog()
    {
        use axum::extract::{Json, State};
        use axum::routing::{get, post};
        use axum::Router;
        use std::sync::{Arc, Mutex};

        async fn models() -> Json<Value> {
            let data: Vec<Value> = (1..=8)
                .map(|index| json!({"id": format!("catalog-model-{index}")}))
                .collect();
            Json(json!({"data": data}))
        }
        async fn chat(
            State(calls): State<Arc<Mutex<Vec<String>>>>,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            calls
                .lock()
                .unwrap()
                .push(body["model"].as_str().unwrap_or_default().to_string());
            if body["tool_choice"] == "required" {
                return Json(json!({
                    "choices": [{"message": {"content": null, "tool_calls": [{
                        "id": "call_probe",
                        "type": "function",
                        "function": {"name": "vellum_probe_tool", "arguments": "{\"value\":\"ok\"}"}
                    }]}}]
                }));
            }
            Json(json!({"choices": [{"message": {"content": "ok"}}]}))
        }
        // Only the Chat wire is wired up: a model's Responses attempt gets
        // axum's default 404 and falls back to Chat, which still records
        // the call.
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/v1/models", get(models))
            .route("/v1/chat/completions", post(chat))
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let selected = vec!["catalog-model-3".to_string(), "catalog-model-7".to_string()];
        let (discovery, verified, report) = reprobe_selected_models_with_client(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            &selected,
            &[],
            WireFormat::Chat,
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(discovery.models.len(), 8);
        assert_eq!(report.discovered, 8);
        assert_eq!(report.targeted, 2);
        assert_eq!(report.succeeded, 2);
        assert_eq!(report.failed, 0);
        assert_eq!(report.skipped, 0);
        assert_eq!(verified.len(), 2);

        let called_models: std::collections::HashSet<String> =
            calls.lock().unwrap().iter().cloned().collect();
        // Only the two selected models ever reached the upstream: not the
        // other six catalog entries, and no duplicated over-probing either.
        assert!(called_models.iter().all(|model| selected.contains(model)));
        assert!(!called_models.is_empty());
    }

    /// A selected model no longer present in the freshly-discovered upstream
    /// catalog (deleted, renamed) is reported as `skipped`, not silently
    /// dropped and not counted as a probe `failed`.
    #[tokio::test]
    async fn reprobe_selected_models_reports_a_selection_missing_from_the_fresh_catalog_as_skipped()
    {
        use axum::extract::Json;
        use axum::routing::get;
        use axum::Router;

        async fn models() -> Json<Value> {
            Json(json!({"data": [{"id": "still-here"}]}))
        }

        let app = Router::new().route("/v1/models", get(models));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let selected = vec!["long-gone-model".to_string()];
        let (_, verified, report) = reprobe_selected_models_with_client(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            &selected,
            &[],
            WireFormat::Chat,
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(report.targeted, 0);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.succeeded, 0);
        assert_eq!(report.failed, 0);
        assert!(verified.is_empty());
    }

    #[tokio::test]
    async fn responses_probe_uses_the_same_public_item_normalization_as_the_proxy() {
        use axum::extract::Json;
        use axum::http::{header, HeaderMap, StatusCode};
        use axum::response::{IntoResponse, Response};
        use axum::routing::{get, post};
        use axum::Router;

        async fn models() -> Json<Value> {
            Json(json!({
                "data": [{
                    "id": "Qwen3-Coder-Next",
                    "meta": {"n_ctx": 204_800}
                }]
            }))
        }

        async fn responses(Json(body): Json<Value>) -> Response {
            if body
                .get("input")
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().any(|item| item["type"] == "additional_tools"))
            {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": {"message": "Cannot determine type of 'item'"}})),
                )
                    .into_response();
            }
            if body["tool_choice"] == "required" {
                return Json(json!({
                    "output": [{
                        "type": "function_call",
                        "id": "fc_probe",
                        "call_id": "call_probe",
                        "name": "vellum_probe_tool",
                        "arguments": "{\"value\":\"ok\"}"
                    }]
                }))
                .into_response();
            }
            Json(json!({
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "ok"}]}]
            }))
            .into_response()
        }

        async fn chat(headers: HeaderMap, Json(body): Json<Value>) -> Response {
            if body["tool_choice"] == "required" {
                return Json(json!({
                    "choices": [{"message": {"content": null, "tool_calls": [{
                        "id": "call_probe", "type": "function",
                        "function": {"name": "vellum_probe_tool", "arguments": "{\"value\":\"ok\"}"}
                    }]}}]
                }))
                .into_response();
            }
            if body["stream"] == true
                && headers
                    .get(header::ACCEPT)
                    .and_then(|value| value.to_str().ok())
                    == Some("text/event-stream")
            {
                return (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    "data: {\"choices\":[{\"delta\":{\"content\":\"o\"}}]}\n\ndata: [DONE]\n\n",
                )
                    .into_response();
            }
            Json(json!({"choices": [{"message": {"content": "ok"}}]})).into_response()
        }

        let app = Router::new()
            .route("/v1/models", get(models))
            .route("/v1/responses", post(responses))
            .route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let result = probe_endpoint_with_client(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(result.wire, Some(WireFormat::Responses));
        assert_eq!(result.context_window, Some(204_800));
        assert_eq!(
            result.model_capabilities[0].wire,
            Some(WireFormat::Responses)
        );
        assert_eq!(
            result.model_capabilities[0].probe_version,
            Some(HARNESS_PROBE_VERSION)
        );
        assert_eq!(result.model_capabilities[0].tool_calling, Some(true));
    }

    #[tokio::test]
    async fn vllm_textual_tool_markup_is_not_advertised_as_codex_tool_support() {
        use axum::extract::Json;
        use axum::http::StatusCode;
        use axum::routing::{get, post};
        use axum::Router;

        async fn models() -> Json<Value> {
            Json(json!({"data": [{"id": "text-only-vllm"}]}))
        }

        async fn responses(Json(body): Json<Value>) -> Json<Value> {
            let text = if body["tool_choice"] == "required" {
                "<tool_call>{\"name\":\"vellum_probe_tool\",\"arguments\":{\"value\":\"ok\"}}</tool_call>"
            } else {
                "ok"
            };
            Json(json!({
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": text}]
                }]
            }))
        }

        let app = Router::new()
            .route("/v1/models", get(models))
            .route("/v1/responses", post(responses))
            .route(
                "/v1/chat/completions",
                post(|| async { StatusCode::NOT_FOUND }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let result = probe_endpoint_with_client(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(result.wire, None);
        assert_eq!(result.model_capabilities[0].tool_calling, Some(false));
        assert_eq!(result.model_capabilities[0].wire, None);
    }

    /// Build a chat-wire server whose tool probe only emits a typed tool call
    /// once it is given room to think first, mirroring Qwen3-class models on
    /// Ollama. Returns the probe result and how many tool probes were served.
    /// `thinking_tokens: None` models a genuinely chat-only server that answers
    /// in text within budget and never emits a tool call.
    async fn probe_reasoning_chat_endpoint(
        thinking_tokens: Option<u32>,
    ) -> (ProbeResult, usize, Vec<u32>) {
        use axum::extract::{Json, State};
        use axum::http::StatusCode;
        use axum::routing::{get, post};
        use axum::Router;
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct Budgets {
            thinking_tokens: Option<u32>,
            seen: Arc<Mutex<Vec<u32>>>,
        }

        async fn models() -> Json<Value> {
            Json(json!({"data": [{"id": "qwen3-reasoner"}]}))
        }

        async fn chat(State(state): State<Budgets>, Json(body): Json<Value>) -> Json<Value> {
            if body.get("tools").is_none() {
                return Json(json!({
                    "choices": [{"message": {"role": "assistant", "content": "."},
                                 "finish_reason": "stop"}]
                }));
            }

            let budget = body["max_tokens"].as_u64().unwrap_or(0) as u32;
            let harness_first_turn = body
                .get("messages")
                .and_then(Value::as_array)
                .is_some_and(|messages| messages.len() == 1);
            if harness_first_turn {
                state.seen.lock().unwrap().push(budget);
            }

            // No reasoning field and no truncation: a server that ignores
            // `tools` entirely. Nothing here invites another attempt.
            let Some(thinking_tokens) = state.thinking_tokens else {
                return Json(json!({
                    "choices": [{"message": {"role": "assistant", "content": "Sure!"},
                                 "finish_reason": "stop"}]
                }));
            };

            // The model always thinks first. Only when the budget outlasts the
            // think block does the tool call make it into the response.
            if budget < thinking_tokens {
                return Json(json!({
                    "choices": [{
                        "message": {"role": "assistant", "content": "",
                                    "reasoning": "Thinking Process: 1. Identify"},
                        "finish_reason": "length"
                    }]
                }));
            }

            Json(json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "vellum_probe_tool",
                                "arguments": "{\"value\":\"ok\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }))
        }

        let seen = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/v1/models", get(models))
            .route("/v1/chat/completions", post(chat))
            .route("/v1/responses", post(|| async { StatusCode::NOT_FOUND }))
            .with_state(Budgets {
                thinking_tokens,
                seen: Arc::clone(&seen),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let result = probe_endpoint_with_client(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
        )
        .await
        .unwrap();
        server.abort();

        let budgets = seen.lock().unwrap().clone();
        (result, budgets.len(), budgets)
    }

    /// A harness check runs once for wire detection and once per model, so
    /// assert the shape of a single check — its budget escalation and attempt
    /// cap — rather than a total that tracks an unrelated call count.
    fn assert_attempts_per_check(budgets: &[u32], expected: &[u32]) {
        assert!(!budgets.is_empty(), "no tool probe was issued");
        assert_eq!(
            budgets.len() % expected.len(),
            0,
            "budgets {budgets:?} are not whole repeats of {expected:?}"
        );
        for check in budgets.chunks(expected.len()) {
            assert_eq!(check, expected, "within budgets {budgets:?}");
        }
    }

    #[tokio::test]
    async fn reasoning_model_truncated_by_the_tool_budget_is_retried_with_room_to_think() {
        // 64 tokens is not enough; the escalated budget is. Without the retry
        // this endpoint was classified chat-only and its models could not be
        // selected at all.
        let (result, _, budgets) = probe_reasoning_chat_endpoint(Some(200)).await;

        assert_eq!(result.model_capabilities[0].tool_calling, Some(true));
        assert_attempts_per_check(
            &budgets,
            &[TOOL_PROBE_MAX_TOKENS, TOOL_PROBE_REASONING_MAX_TOKENS],
        );
    }

    #[tokio::test]
    async fn text_only_server_is_rejected_on_the_first_attempt() {
        // Plain answer, no reasoning, no truncation: nothing suggests a model
        // that engaged with the tool, so retrying would only cost time.
        let (result, _, budgets) = probe_reasoning_chat_endpoint(None).await;

        assert_eq!(result.model_capabilities[0].tool_calling, Some(false));
        assert_attempts_per_check(&budgets, &[TOOL_PROBE_MAX_TOKENS]);
    }

    #[tokio::test]
    async fn tool_probe_gives_up_after_the_attempt_cap() {
        // A model that thinks past even the escalated budget every time stays
        // chat-only, and must not retry forever to get there.
        let (result, _, budgets) = probe_reasoning_chat_endpoint(Some(1_000_000)).await;

        let expected: Vec<u32> = (0..TOOL_PROBE_ATTEMPTS).map(tool_probe_budget).collect();
        assert_eq!(result.model_capabilities[0].tool_calling, Some(false));
        assert_attempts_per_check(&budgets, &expected);
    }

    #[test]
    fn a_thinking_model_that_declined_is_inconclusive_but_a_plain_answer_is_not() {
        let plain =
            json!({"choices": [{"message": {"content": "Sure!"}, "finish_reason": "stop"}]});
        assert!(!chat_probe_inconclusive(&plain));

        let thought = json!({"choices": [{
            "message": {"content": "Sure!", "reasoning": "I could just answer."},
            "finish_reason": "stop"
        }]});
        assert!(chat_probe_inconclusive(&thought));

        let thought_alias = json!({"choices": [{
            "message": {"content": "Sure!", "reasoning_content": "Hmm."},
            "finish_reason": "stop"
        }]});
        assert!(chat_probe_inconclusive(&thought_alias));

        // An empty reasoning field is not evidence the model engaged.
        let empty = json!({"choices": [{
            "message": {"content": "Sure!", "reasoning": "   "},
            "finish_reason": "stop"
        }]});
        assert!(!chat_probe_inconclusive(&empty));

        assert!(responses_probe_inconclusive(
            &json!({"output": [{"type": "reasoning"}], "status": "completed"})
        ));
        assert!(!responses_probe_inconclusive(
            &json!({"output": [{"type": "message"}], "status": "completed"})
        ));
    }

    #[test]
    fn chat_tool_choice_rejected_matches_the_error_generically() {
        // Real body from deepseek-v4-pro on opencode.ai/zen/go/v1.
        let deepseek = json!({"error": {"type": "invalid_request_error", "message":
            "Error from provider (Console Go): Upstream request failed: [invalid_request_error] Thinking mode does not support this tool_choice"}});
        assert!(chat_tool_choice_rejected(400, &deepseek));

        // Real body from qwen3.8-max on the same endpoint — different vendor
        // wording, same underlying policy. Matched because both mention
        // tool_choice, not because of shared phrasing.
        let qwen = json!({"error": {"type": "invalid_request_error", "message":
            "Error from provider (Console Go): Upstream request failed: [invalid_parameter_error] <400> InternalError.Algo.InvalidParameter: The tool_choice parameter does not support being set to required or object in thinking mode"}});
        assert!(chat_tool_choice_rejected(400, &qwen));

        // An unrelated 400 (bad schema, wrong model id, ...) must not trigger
        // the auto-retry — that would mask a genuinely broken endpoint as
        // "not tool-capable" via a needless extra request instead of failing
        // cleanly on the first one.
        let unrelated =
            json!({"error": {"type": "invalid_request_error", "message": "Unknown model"}});
        assert!(!chat_tool_choice_rejected(400, &unrelated));
        assert!(!chat_tool_choice_rejected(200, &deepseek));
    }

    #[test]
    fn provider_access_policy_is_not_classified_as_missing_tool_support() {
        let opt_in = json!({"error": {"message":
            "The latest version of this model is only available hosted in China and requires explicit opt in: https://opencode.ai/workspace/example/go"}});
        assert_eq!(
            capability_probe_issue(403, &opt_in).as_deref(),
            Some(PROBE_ISSUE_PROVIDER_OPT_IN_REQUIRED)
        );

        let denied = json!({"error": {"message": "This model is not enabled for the account"}});
        assert_eq!(
            capability_probe_issue(403, &denied).as_deref(),
            Some(PROBE_ISSUE_PROVIDER_ACCESS_RESTRICTED)
        );
        assert_eq!(capability_probe_issue(400, &opt_in), None);
    }

    #[tokio::test]
    async fn per_model_probe_falls_back_from_responses_to_chat() {
        use axum::extract::Json;
        use axum::http::StatusCode;
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;

        async fn responses() -> StatusCode {
            StatusCode::NOT_FOUND
        }

        async fn chat(Json(body): Json<Value>) -> Response {
            if body.get("tools").is_some() {
                return Json(json!({
                    "choices": [{"message": {"content": null, "tool_calls": [{
                        "id": "call_probe",
                        "type": "function",
                        "function": {
                            "name": "vellum_probe_tool",
                            "arguments": "{\"value\":\"ok\"}"
                        }
                    }]}}]
                }))
                .into_response();
            }
            Json(json!({"choices": [{"message": {"content": "ok"}}]})).into_response()
        }

        let app = Router::new()
            .route("/v1/responses", post(responses))
            .route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let capability = probe_model_capability_with_wire_fallback(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "muse-glimmer:latest".into(),
            WireFormat::Responses,
            None,
            EffortMetadata::default(),
            &RuntimeChatCapabilities::default(),
        )
        .await;
        server.abort();

        assert_eq!(capability.wire, Some(WireFormat::Chat));
        assert_eq!(capability.tool_calling, Some(true));
    }

    #[tokio::test]
    async fn account_opt_in_block_preserves_wire_and_leaves_tools_unverified() {
        use axum::extract::Json;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::Router;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let requests = Arc::new(AtomicUsize::new(0));
        let requests_for_handler = requests.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let requests = requests_for_handler.clone();
                async move {
                    requests.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::FORBIDDEN,
                        Json(json!({"error": {"message":
                            "The latest version of this model is only available hosted in China and requires explicit opt in: https://opencode.ai/workspace/example/go"}})),
                    )
                        .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let capability = probe_model_capability(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "deepseek-v4-flash".into(),
            WireFormat::Chat,
            Some(1_000_000),
            EffortMetadata::default(),
            &RuntimeChatCapabilities::default(),
        )
        .await;
        server.abort();

        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert_eq!(capability.wire, Some(WireFormat::Chat));
        assert_eq!(capability.tool_calling, None);
        assert_eq!(
            capability.probe_issue.as_deref(),
            Some(PROBE_ISSUE_PROVIDER_OPT_IN_REQUIRED)
        );
        assert_eq!(capability.effort_probe_version, None);
    }

    /// Reproduces the deepseek-v4-pro / qwen3.8-max bug this fixes: the
    /// gateway runs reasoning mode by default and rejects a forced tool
    /// choice with a 400, even though the model calls tools correctly once
    /// asked with `tool_choice: "auto"`. Before this fix, the harness gave up
    /// on the first 400 and reported these models as chat-only.
    #[tokio::test]
    async fn chat_harness_retries_with_auto_when_required_is_rejected_by_thinking_mode() {
        use axum::extract::Json;
        use axum::http::StatusCode;
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;

        async fn chat(Json(body): Json<Value>) -> Response {
            match body["tool_choice"].as_str() {
                Some("required") => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": {"type": "invalid_request_error",
                        "message": "Thinking mode does not support this tool_choice"}})),
                )
                    .into_response(),
                Some("auto") => Json(json!({
                    "choices": [{"message": {"content": null, "tool_calls": [{
                        "id": "call_probe", "type": "function",
                        "function": {"name": "vellum_probe_tool", "arguments": "{\"value\":\"ok\"}"}
                    }]}}]
                }))
                .into_response(),
                _ => StatusCode::BAD_REQUEST.into_response(),
            }
        }

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let compatible = chat_harness_compatible(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "deepseek-v4-pro",
        )
        .await;
        server.abort();

        assert!(compatible);
    }

    #[tokio::test]
    async fn reasoning_probe_retries_required_then_proves_missing_reasoning() {
        use axum::extract::Json;
        use axum::http::StatusCode;
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;
        use std::sync::{Arc, Mutex};

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_for_handler = seen.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let seen = seen_for_handler.clone();
                async move {
                    let messages = body["messages"].as_array().cloned().unwrap_or_default();
                    let tool_choice = body["tool_choice"].as_str().unwrap_or("missing");
                    let has_reasoning = messages.iter().any(|message| {
                        message
                            .get("reasoning_content")
                            .and_then(Value::as_str)
                            .is_some_and(|text| !text.is_empty())
                    });
                    let stage = if messages.len() <= 1 {
                        format!("first:{tool_choice}")
                    } else if has_reasoning {
                        "continuation:with-reasoning".to_string()
                    } else {
                        "continuation:missing-reasoning".to_string()
                    };
                    seen.lock().unwrap().push(stage.clone());
                    let response: Response = match stage.as_str() {
                        "first:required" => (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error": {"type": "invalid_request_error",
                                "message": "Thinking mode does not support this tool_choice"}})),
                        )
                            .into_response(),
                        "first:auto" => Json(json!({
                            "choices": [{"message": {
                                "role": "assistant",
                                "content": "",
                                "reasoning_content": "need the probe tool",
                                "tool_calls": [{
                                    "id": "call_probe",
                                    "type": "function",
                                    "function": {
                                        "name": "vellum_probe_tool",
                                        "arguments": "{\"value\":\"ok\"}"
                                    }
                                }]
                            }}]
                        }))
                        .into_response(),
                        "continuation:missing-reasoning" => (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error": {"message":
                                "reasoning_content in assistant message that follows tool calls must be passed back"}})),
                        )
                            .into_response(),
                        "continuation:with-reasoning" => Json(json!({
                            "choices": [{"message": {"content": "ok"}}]
                        }))
                        .into_response(),
                        _ => StatusCode::BAD_REQUEST.into_response(),
                    };
                    response
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let observation = probe_chat_reasoning_replay(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "deepseek-v4-pro",
        )
        .await;
        server.abort();

        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                "first:required".to_string(),
                "first:auto".to_string(),
                "continuation:missing-reasoning".to_string(),
                "continuation:with-reasoning".to_string(),
            ]
        );
        assert_eq!(
            observation,
            ReasoningReplayObservation::ConfirmedToolCallBound
        );
    }

    #[tokio::test]
    async fn reasoning_probe_keeps_quota_and_other_4xx_inconclusive() {
        use axum::extract::Json;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::Router;

        async fn quota(Json(_body): Json<Value>) -> impl IntoResponse {
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error": {"message": "rate limited"}})),
            )
        }

        let app = Router::new().route("/v1/chat/completions", post(quota));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let observation = probe_chat_reasoning_replay(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "deepseek-v4-pro",
        )
        .await;
        server.abort();
        assert_eq!(observation, ReasoningReplayObservation::Inconclusive);
    }

    #[test]
    fn truncation_is_read_from_the_finish_reason_not_the_missing_tool_call() {
        assert!(chat_truncated_before_tool_call(
            &json!({"choices": [{"finish_reason": "length"}]})
        ));
        assert!(!chat_truncated_before_tool_call(
            &json!({"choices": [{"finish_reason": "stop"}]})
        ));
        assert!(!chat_truncated_before_tool_call(&json!({"choices": [{}]})));

        assert!(responses_truncated_before_tool_call(
            &json!({"incomplete_details": {"reason": "max_output_tokens"}})
        ));
        assert!(responses_truncated_before_tool_call(
            &json!({"status": "incomplete"})
        ));
        assert!(!responses_truncated_before_tool_call(
            &json!({"status": "completed"})
        ));
    }

    #[test]
    fn ollama_native_urls_normalize_to_the_openai_compatible_base() {
        assert_eq!(
            normalize_provider_base_url("http://host:11434/api/generate"),
            "http://host:11434/v1"
        );
        assert_eq!(
            normalize_provider_base_url("http://host:11434/api/chat/"),
            "http://host:11434/v1"
        );
        assert_eq!(
            normalize_provider_base_url("http://host:11434/v1/responses"),
            "http://host:11434/v1"
        );
    }

    #[test]
    fn base_url_normalization_never_keeps_a_doubled_v1_prefix() {
        // A pasted endpoint that already contains `/v1` (or duplicates it)
        // must converge to a single version prefix: the proxy appends the wire
        // suffix itself, so `/v1/v1/chat/completions` would otherwise become
        // `/v1/v1/chat/completions` upstream.
        assert_eq!(
            normalize_provider_base_url("https://api.provider.example/v1"),
            "https://api.provider.example/v1"
        );
        assert_eq!(
            normalize_provider_base_url("https://api.provider.example/v1/v1"),
            "https://api.provider.example/v1"
        );
        assert_eq!(
            normalize_provider_base_url("https://api.provider.example/v1/v1/chat/completions"),
            "https://api.provider.example/v1"
        );
        assert_eq!(
            normalize_provider_base_url("https://api.provider.example/v1/v1/responses"),
            "https://api.provider.example/v1"
        );
        assert_eq!(
            normalize_provider_base_url("https://api.provider.example/"),
            "https://api.provider.example/v1"
        );
    }

    #[tokio::test]
    async fn ollama_generate_alias_discovers_native_tags_and_probes_responses() {
        use axum::extract::Json;
        use axum::http::{header, HeaderMap, StatusCode};
        use axum::response::{IntoResponse, Response};
        use axum::routing::{get, post};
        use axum::Router;

        async fn tags() -> Json<Value> {
            Json(json!({
                "models": [{
                    "name": "qwen-local",
                    "details": {"context_length": 262_144}
                }]
            }))
        }

        async fn responses(headers: HeaderMap, Json(body): Json<Value>) -> Response {
            if body
                .get("input")
                .and_then(Value::as_array)
                .is_some_and(|items| items.iter().any(|item| item["type"] == "additional_tools"))
            {
                return StatusCode::BAD_REQUEST.into_response();
            }
            if body["tool_choice"] == "required" {
                return Json(json!({"output": [{
                    "type": "function_call", "name": "vellum_probe_tool",
                    "arguments": "{\"value\":\"ok\"}"
                }]}))
                .into_response();
            }
            if body["stream"] == true
                && headers
                    .get(header::ACCEPT)
                    .and_then(|value| value.to_str().ok())
                    == Some("text/event-stream")
            {
                return (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n",
                )
                    .into_response();
            }
            if body["tool_choice"] == "required" {
                return Json(json!({"output": [{
                    "type": "function_call", "name": "vellum_probe_tool",
                    "arguments": "{\"value\":\"ok\"}"
                }]}))
                .into_response();
            }
            Json(json!({
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "ok"}]}]
            }))
            .into_response()
        }

        let app = Router::new()
            .route("/api/tags", get(tags))
            .route("/v1/responses", post(responses));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let result = probe_endpoint_with_client(
            &default_client().unwrap(),
            &format!("http://{address}/api/generate"),
            None,
        )
        .await
        .unwrap();
        server.abort();

        assert!(result.reachable);
        assert_eq!(result.models, vec!["qwen-local"]);
        assert_eq!(result.context_window, Some(262_144));
        assert_eq!(result.wire, Some(WireFormat::Responses));
        assert!(result.streaming);
    }

    #[tokio::test]
    async fn ollama_generate_alias_falls_back_to_chat_with_native_typed_tools() {
        use axum::extract::Json;
        use axum::http::{header, HeaderMap, StatusCode};
        use axum::response::{IntoResponse, Response};
        use axum::routing::{get, post};
        use axum::Router;

        async fn tags() -> Json<Value> {
            Json(json!({"models": [{"name": "qwen-chat"}]}))
        }

        async fn chat(headers: HeaderMap, Json(body): Json<Value>) -> Response {
            if body["stream"] == true
                && headers
                    .get(header::ACCEPT)
                    .and_then(|value| value.to_str().ok())
                    == Some("text/event-stream")
            {
                return (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    "data: {\"choices\":[{\"delta\":{\"content\":\"o\"}}]}\n\ndata: [DONE]\n\n",
                )
                    .into_response();
            }
            if body["tool_choice"] == "required" {
                return Json(json!({
                    "choices": [{"message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_probe",
                            "type": "function",
                            "function": {
                                "name": "vellum_probe_tool",
                                "arguments": {"value": "ok"}
                            }
                        }]
                    }}]
                }))
                .into_response();
            }
            Json(json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}))
                .into_response()
        }

        let app = Router::new()
            .route("/api/tags", get(tags))
            .route("/v1/responses", post(|| async { StatusCode::NOT_FOUND }))
            .route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let result = probe_endpoint_with_client(
            &default_client().unwrap(),
            &format!("http://{address}/api/generate"),
            None,
        )
        .await
        .unwrap();
        server.abort();

        assert!(result.reachable);
        assert_eq!(result.wire, Some(WireFormat::Chat));
        assert_eq!(result.model_capabilities[0].wire, Some(WireFormat::Chat));
        assert_eq!(result.model_capabilities[0].tool_calling, Some(true));
        assert!(result.streaming);
    }

    #[test]
    fn first_legal_sse_chunk_does_not_require_done() {
        assert!(first_chunk_looks_like_sse(
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\"}\n"
        ));
        assert!(first_chunk_looks_like_sse(
            "event: response.created\ndata: {\"type\":\"response.created\"}\n"
        ));
        assert!(!first_chunk_looks_like_sse("{\"choices\":[]}"));
    }

    #[tokio::test]
    async fn streaming_probe_accepts_sse_headers_without_waiting_for_done() {
        use axum::extract::Json;
        use axum::http::header;
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::Router;

        async fn stream(Json(body): Json<Value>) -> impl IntoResponse {
            assert_eq!(body["stream"], true);
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                "data: {\"choices\":[{\"delta\":{\"content\":\"o\"}}]}\n\n",
            )
        }

        let app = Router::new().route("/v1/chat/completions", post(stream));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let result = probe_streaming_with_timeout(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "qwen3.8-27b",
            WireFormat::Chat,
            std::time::Duration::from_secs(2),
        )
        .await;
        server.abort();
        assert_eq!(result, StreamingProbe::Supported);
        assert_eq!(result.as_capability(), Some(true));
        assert_eq!(result.probe_issue(), None);
    }

    #[tokio::test]
    async fn streaming_probe_timeout_is_unknown_not_false() {
        use axum::routing::post;
        use axum::Router;

        async fn hang() {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }

        let app = Router::new().route("/v1/chat/completions", post(hang));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let result = probe_streaming_with_timeout(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "qwen3.8-27b",
            WireFormat::Chat,
            std::time::Duration::from_millis(80),
        )
        .await;
        server.abort();
        assert_eq!(result, StreamingProbe::Timeout);
        assert_eq!(result.as_capability(), None);
        assert_eq!(result.probe_issue(), Some("timeout"));
    }

    #[tokio::test]
    async fn endpoint_probe_records_streaming_timeout_as_probe_issue() {
        use axum::extract::Json;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::{get, post};
        use axum::Router;

        async fn models() -> Json<Value> {
            Json(json!({"data": [{"id": "qwen3.8-27b"}]}))
        }
        async fn chat(Json(body): Json<Value>) -> impl IntoResponse {
            if body["stream"] == true {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                return StatusCode::REQUEST_TIMEOUT.into_response();
            }
            if body["tool_choice"] == "required" {
                return Json(json!({
                    "choices": [{"message": {"content": null, "tool_calls": [{
                        "id": "call_probe", "type": "function",
                        "function": {"name": "vellum_probe_tool", "arguments": "{\"value\":\"ok\"}"}
                    }]}}]
                }))
                .into_response();
            }
            Json(json!({"choices": [{"message": {"role": "assistant", "content": "ok"}}]}))
                .into_response()
        }

        let app = Router::new()
            .route("/v1/models", get(models))
            .route("/v1/responses", post(|| async { StatusCode::NOT_FOUND }))
            .route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        struct ResetTimeout;
        impl Drop for ResetTimeout {
            fn drop(&mut self) {
                TEST_STREAMING_TIMEOUT_MS.store(0, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _reset = ResetTimeout;
        TEST_STREAMING_TIMEOUT_MS.store(80, std::sync::atomic::Ordering::SeqCst);
        let result = probe_endpoint_with_client(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(result.model_capabilities[0].streaming, None);
        assert_eq!(
            result.model_capabilities[0].probe_issue.as_deref(),
            Some("timeout")
        );
        assert!(
            result.streaming,
            "route-level streaming must not persist a timeout as false"
        );
    }

    #[test]
    fn infer_wire_handles_status_codes() {
        // 2xx → 該 wire 存在
        assert_eq!(
            infer_wire_from_status(200, WireFormat::Responses),
            Some(WireFormat::Responses)
        );
        // 404/405 → 換 wire
        assert_eq!(infer_wire_from_status(404, WireFormat::Responses), None);
        assert_eq!(infer_wire_from_status(405, WireFormat::Chat), None);
        // 400/422 → 路徑認得，視為存在（body 規格不同）
        assert_eq!(
            infer_wire_from_status(400, WireFormat::Responses),
            Some(WireFormat::Responses)
        );
        assert_eq!(
            infer_wire_from_status(422, WireFormat::Chat),
            Some(WireFormat::Chat)
        );
    }

    #[test]
    fn extract_reasoning_from_responses_wire() {
        let body = json!({
            "output": [
                { "type": "reasoning", "summary": "..." },
                { "type": "message", "content": "hi" },
            ]
        });
        assert!(extract_reasoning(&body));
    }

    #[test]
    fn extract_reasoning_from_chat_wire() {
        let body = json!({
            "choices": [{
                "message": { "role": "assistant", "content": "hi", "reasoning": "thinking..." }
            }]
        });
        assert!(extract_reasoning(&body));

        let body2 = json!({
            "choices": [{
                "message": { "reasoning_content": "..." }
            }]
        });
        assert!(extract_reasoning(&body2));
    }

    #[test]
    fn extract_reasoning_absent() {
        let body = json!({
            "choices": [{ "message": { "content": "hi" } }]
        });
        assert!(!extract_reasoning(&body));
    }

    #[test]
    fn apply_effort_field_writes_provider_specific_into_chat_template_kwargs() {
        let mut body = json!({"model": "m"});
        apply_effort_field(
            &mut body,
            ReasoningEffortTransport::ProviderSpecific,
            "xhigh",
        );
        assert_eq!(body["chat_template_kwargs"]["reasoning_effort"], "xhigh");
        // Must not also write either standard-dialect field -- a provider
        // reading `reasoning.effort` off a `ProviderSpecific` probe would
        // otherwise see a value it happens to accept for the wrong reason.
        assert!(body.get("reasoning").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn apply_effort_field_none_transport_writes_nothing() {
        let mut body = json!({"model": "m"});
        apply_effort_field(&mut body, ReasoningEffortTransport::None, "high");
        assert_eq!(body, json!({"model": "m"}));
    }

    #[test]
    fn extract_effort_probe_text_combines_reasoning_and_message_on_chat_wire() {
        let body = json!({
            "choices": [{
                "message": { "reasoning_content": "thinking. ", "content": "hi" }
            }]
        });
        assert_eq!(extract_effort_probe_text(&body), "thinking. hi");
    }

    #[test]
    fn extract_effort_probe_text_reads_responses_wire_output_items() {
        let body = json!({
            "output": [
                {"type": "reasoning", "content": [{"type": "reasoning_text", "text": "thinking. "}]},
                {"type": "message", "content": [{"type": "output_text", "text": "hi"}]},
            ]
        });
        assert_eq!(extract_effort_probe_text(&body), "thinking. hi");
    }

    #[test]
    fn extract_models_from_data_array() {
        let body = json!({
            "data": [
                { "id": "grok-4.5" },
                { "id": "grok-3" },
            ]
        });
        assert_eq!(extract_models(&body), vec!["grok-4.5", "grok-3"]);
    }

    #[test]
    fn extract_models_missing_data_key() {
        let body = json!({ "object": "list" });
        assert!(extract_models(&body).is_empty());
    }

    #[test]
    fn extract_models_keeps_every_unique_model_in_provider_order() {
        let body = json!({
            "data": [
                { "id": "MiMo-V2.5-Pro" },
                { "id": "GLM-5.2" },
                { "id": "DeepSeek-V4-Flash" },
                { "id": "glm-5.2" }
            ]
        });
        assert_eq!(
            extract_models(&body),
            vec!["MiMo-V2.5-Pro", "GLM-5.2", "DeepSeek-V4-Flash"]
        );
    }

    #[test]
    fn extract_model_context_window_various_keys() {
        assert_eq!(
            extract_model_context_window(&json!({ "context_window": 200_000 })),
            Some(200_000)
        );
        assert_eq!(
            extract_model_context_window(&json!({ "max_context_length": 128_000 })),
            Some(128_000)
        );
        assert_eq!(
            extract_model_context_window(&json!({ "context_length": 8192 })),
            Some(8192)
        );
        assert_eq!(extract_model_context_window(&json!({ "name": "x" })), None);
        assert_eq!(
            extract_model_context_window(&json!({ "context_window": 0 })),
            None
        );
    }

    #[test]
    fn merge_context_window_priority() {
        // 供應商快取勝出
        assert_eq!(
            merge_context_window(Some(500_000), Some(200_000), Some(128_000)),
            Some(500_000)
        );
        // /models 次之
        assert_eq!(
            merge_context_window(None, Some(200_000), Some(128_000)),
            Some(200_000)
        );
        // 已知表再次
        assert_eq!(
            merge_context_window(None, None, Some(128_000)),
            Some(128_000)
        );
        // 全空 → None
        assert_eq!(merge_context_window(None, None, None), None);
        // Zero is an unknown/invalid provider value, not a real zero-token
        // model. Continue down the source chain instead of publishing 0/0 to
        // Codex Desktop.
        assert_eq!(
            merge_context_window(Some(0), Some(0), Some(128_000)),
            Some(128_000)
        );
    }

    #[test]
    fn opencode_catalog_reachability_has_a_shorter_budget() {
        assert_eq!(
            models_catalog_timeout(OPENCODE_ZEN_BASE_URL),
            std::time::Duration::from_secs(OPENCODE_CATALOG_TIMEOUT_SECS)
        );
        assert_eq!(
            models_catalog_timeout(OPENCODE_GO_BASE_URL),
            std::time::Duration::from_secs(OPENCODE_CATALOG_TIMEOUT_SECS)
        );
        assert_eq!(
            models_catalog_timeout("https://provider.example/v1"),
            reachability_timeout()
        );
        assert!(OPENCODE_CATALOG_TIMEOUT_SECS < PROBE_TIMEOUT_SECS);
    }

    #[test]
    fn is_sse_content_type_detection() {
        assert!(is_sse_content_type(Some("text/event-stream")));
        assert!(is_sse_content_type(Some(
            "text/event-stream; charset=utf-8"
        )));
        assert!(!is_sse_content_type(Some("application/json")));
        assert!(!is_sse_content_type(None));
    }

    #[test]
    fn default_client_builds_without_panic() {
        let c = default_client();
        assert!(c.is_ok());
    }

    #[test]
    fn effort_metadata_preserves_provider_level_names_and_default() {
        let metadata = extract_effort_metadata(&json!({
            "supported_reasoning_levels": [
                {"effort": "minimal"},
                {"effort": "xhigh"},
                {"effort": "minimal"}
            ],
            "default_reasoning_level": "xhigh"
        }));
        assert_eq!(metadata.levels, vec!["minimal", "xhigh"]);
        assert_eq!(metadata.default.as_deref(), Some("xhigh"));
    }

    #[tokio::test]
    async fn effort_probe_keeps_only_levels_accepted_by_the_recorded_transport() {
        use axum::extract::Json;
        use axum::http::StatusCode;
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;

        async fn chat(Json(body): Json<Value>) -> Response {
            let accepted = body
                .get("reasoning_effort")
                .and_then(Value::as_str)
                .is_some_and(|effort| matches!(effort, "minimal" | "xhigh"));
            if accepted {
                Json(json!({"choices": [{"message": {"content": "ok"}}]})).into_response()
            } else {
                StatusCode::BAD_REQUEST.into_response()
            }
        }

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let outcome = probe_reasoning_efforts(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "provider-model",
            WireFormat::Chat,
            EffortMetadata {
                levels: vec!["minimal".into(), "medium".into(), "xhigh".into()],
                default: Some("xhigh".into()),
            },
        )
        .await;
        server.abort();

        match outcome {
            EffortProbeOutcome::Supported {
                levels,
                default,
                transport,
            } => {
                assert_eq!(levels, vec!["minimal", "xhigh"]);
                assert_eq!(default.as_deref(), Some("xhigh"));
                assert_eq!(transport, ReasoningEffortTransport::ChatField);
            }
            other => panic!("expected Supported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn probe_effort_behavior_text_rejects_a_non_2xx_status() {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::Router;

        async fn chat() -> impl IntoResponse {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"message": "boom"}})),
            )
        }
        use axum::Json;

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let result = probe_effort_behavior_text(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "provider-model",
            WireFormat::Chat,
            ReasoningEffortTransport::ChatField,
            "high",
        )
        .await;
        server.abort();

        assert_eq!(result, None, "a 500 must never read as usable evidence");
    }

    #[tokio::test]
    async fn probe_effort_behavior_text_rejects_empty_output() {
        use axum::extract::Json;
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;

        async fn chat(Json(_body): Json<Value>) -> Response {
            Json(json!({"choices": [{"message": {"content": ""}}]})).into_response()
        }

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let result = probe_effort_behavior_text(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "provider-model",
            WireFormat::Chat,
            ReasoningEffortTransport::ChatField,
            "high",
        )
        .await;
        server.abort();

        assert_eq!(
            result, None,
            "empty extracted text must never read as usable evidence"
        );
    }

    #[tokio::test]
    async fn probe_effort_behavior_text_rejects_an_unparseable_body() {
        use axum::http::{header, StatusCode};
        use axum::response::IntoResponse;
        use axum::routing::post;
        use axum::Router;

        async fn chat() -> impl IntoResponse {
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/plain")],
                "not json at all",
            )
        }

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let result = probe_effort_behavior_text(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "provider-model",
            WireFormat::Chat,
            ReasoningEffortTransport::ChatField,
            "high",
        )
        .await;
        server.abort();

        assert_eq!(
            result, None,
            "a non-JSON 200 body must never read as usable evidence"
        );
    }

    /// A random third-party provider that neither validates the field by
    /// status (accepts the negative control) nor exhibits any behavioral
    /// difference is exactly the common case this whole mechanism must
    /// resolve to `Indeterminate` for, on the very first Effort probe run --
    /// not a hand-picked llama.cpp/vLLM shape, an arbitrary always-accepting
    /// mock with unrelated content.
    #[tokio::test]
    async fn effort_probe_stays_indeterminate_for_an_arbitrary_provider_with_no_behavioral_signal()
    {
        use axum::extract::Json;
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;

        async fn chat(Json(_body): Json<Value>) -> Response {
            Json(json!({
                "id": "chatcmpl-random",
                "choices": [{"message": {"content": "Sure, here's a fixed reply."}}]
            }))
            .into_response()
        }

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let outcome = probe_reasoning_efforts(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "some-random-provider-model",
            WireFormat::Chat,
            EffortMetadata::default(),
        )
        .await;
        server.abort();

        match outcome {
            EffortProbeOutcome::Indeterminate(_) => {}
            other => panic!("expected Indeterminate, got {other:?}"),
        }
    }

    /// A provider that returns 2xx for *any* `reasoning_effort` value,
    /// including the deliberately-invalid negative control, is not actually
    /// validating the field via status codes -- every real candidate looking
    /// "accepted" would otherwise be misread as full Effort support. That is
    /// the exact "misjudged as supporting all levels" failure mode the
    /// negative control's status check exists to catch. This mock also
    /// generates byte-identical content regardless of the value sent, so the
    /// behavioral tiebreaker (which runs precisely because the status check
    /// alone is inconclusive here) has a stable control but no A-vs-B diff.
    /// That must stay `Indeterminate`, never a confirmed `Unsupported`: an
    /// identical-output result is weaker evidence than a real validated HTTP
    /// rejection (a trivial prompt, or a model that happens to converge
    /// regardless of effort, can produce the same non-difference even on a
    /// provider that does read the field) -- see
    /// `effort_field_is_confirmed_effective`.
    #[tokio::test]
    async fn effort_probe_stays_indeterminate_when_behavioral_diff_finds_no_difference() {
        use axum::extract::Json;
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;

        async fn chat(Json(_body): Json<Value>) -> Response {
            Json(json!({"choices": [{"message": {"content": "ok"}}]})).into_response()
        }

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let outcome = probe_reasoning_efforts(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "provider-model",
            WireFormat::Chat,
            EffortMetadata::default(),
        )
        .await;
        server.abort();

        match outcome {
            EffortProbeOutcome::Indeterminate(issue) => {
                assert_eq!(issue, "provider_ignores_unknown_effort");
            }
            other => panic!("expected Indeterminate, got {other:?}"),
        }
    }

    /// A provider whose responses are not deterministic even at fixed low
    /// temperature/seed (ignores `seed` entirely, or is inherently noisy) --
    /// the A/B/A control check must catch this itself and refuse to trust
    /// any A-vs-B diff, landing on `Indeterminate` even though the two
    /// control runs' content happens to differ from the real candidate too.
    #[tokio::test]
    async fn effort_probe_stays_indeterminate_when_the_negative_control_itself_is_unstable() {
        use axum::extract::Json;
        use axum::routing::post;
        use axum::Router;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_handler = calls.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(_body): Json<Value>| {
                let calls = calls_for_handler.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    // A distinct reply every single call -- including the two
                    // negative-control calls -- so the control never agrees
                    // with itself, however the harness happens to sequence
                    // requests across transports/candidates.
                    Json(json!({"choices": [{"message": {"content": format!("reply-{n}")}}]}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let outcome = probe_reasoning_efforts(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "provider-model",
            WireFormat::Chat,
            EffortMetadata::default(),
        )
        .await;
        server.abort();

        match outcome {
            EffortProbeOutcome::Indeterminate(_) => {}
            other => panic!(
                "an unstable control must never be trusted as a confirmed verdict, got {other:?}"
            ),
        }
    }

    /// Mirrors the real llama.cpp/vLLM failure this fixes: a Jinja-templated
    /// backend that ignores the two standard OpenAI-dialect Effort fields
    /// (`reasoning_effort` flat, `reasoning.effort` nested) but genuinely
    /// reads `chat_template_kwargs.reasoning_effort` and changes its output
    /// accordingly. Status codes alone cannot distinguish any of this (the
    /// mock always returns 200), so only the behavioral tiebreaker on
    /// `ProviderSpecific` can find the real support here.
    #[tokio::test]
    async fn effort_probe_finds_provider_specific_support_via_behavioral_diff() {
        use axum::extract::Json;
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;

        async fn chat(Json(body): Json<Value>) -> Response {
            let effort = body
                .pointer("/chat_template_kwargs/reasoning_effort")
                .and_then(Value::as_str);
            let content = match effort {
                Some(value) if value != EFFORT_NEGATIVE_CONTROL => format!("effort:{value}"),
                _ => "baseline".to_string(),
            };
            Json(json!({"choices": [{"message": {"content": content}}]})).into_response()
        }

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let outcome = probe_reasoning_efforts(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "provider-model",
            WireFormat::Chat,
            EffortMetadata::default(),
        )
        .await;
        server.abort();

        match outcome {
            EffortProbeOutcome::Supported {
                levels, transport, ..
            } => {
                assert_eq!(transport, ReasoningEffortTransport::ProviderSpecific);
                assert!(!levels.is_empty());
            }
            other => panic!("expected Supported via ProviderSpecific, got {other:?}"),
        }
    }

    /// A provider that cleanly validates and rejects every level (including
    /// the negative control) is a genuine, trustworthy "does not support
    /// Effort control" verdict -- not a probe failure.
    #[tokio::test]
    async fn effort_probe_reports_unsupported_when_every_candidate_is_cleanly_rejected() {
        use axum::http::StatusCode;
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;

        async fn chat() -> Response {
            StatusCode::UNPROCESSABLE_ENTITY.into_response()
        }

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let outcome = probe_reasoning_efforts(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "provider-model",
            WireFormat::Chat,
            EffortMetadata::default(),
        )
        .await;
        server.abort();

        assert_eq!(outcome, EffortProbeOutcome::Unsupported);
    }

    /// A rate-limited/erroring endpoint must never read as a clean negative:
    /// the model might genuinely support Effort control, but this round
    /// could not tell. `effort_probe_version` must stay unset so a later
    /// re-probe is not silently skipped as already-determined (see
    /// `probe_model_capability`).
    #[tokio::test]
    async fn effort_probe_is_indeterminate_not_unsupported_when_every_candidate_is_rate_limited() {
        use axum::http::StatusCode;
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;

        async fn chat() -> Response {
            StatusCode::TOO_MANY_REQUESTS.into_response()
        }

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let outcome = probe_reasoning_efforts(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "provider-model",
            WireFormat::Chat,
            EffortMetadata::default(),
        )
        .await;
        server.abort();

        match outcome {
            EffortProbeOutcome::Indeterminate(_) => {}
            other => panic!("expected Indeterminate, got {other:?}"),
        }
    }

    #[test]
    fn effort_probe_status_only_versions_on_a_determinable_outcome() {
        // Mirrors the gate in `probe_model_capability`: Indeterminate must
        // never carry a version, or a later re-probe would look like a no-op
        // retry instead of actually trying again.
        for status in [
            EffortProbeStatus::NotApplicable,
            EffortProbeStatus::Supported,
            EffortProbeStatus::Unsupported,
        ] {
            assert_ne!(status, EffortProbeStatus::Indeterminate);
        }
    }

    /// Concurrent probing (`buffer_unordered`) completes candidates in
    /// whatever order their responses happen to arrive, not the order they
    /// were sent. When no provider-declared order exists, the accepted
    /// subset must still come back in the fixed none/minimal/low/medium/
    /// high/xhigh strength order, not completion order.
    #[tokio::test]
    async fn effort_probe_returns_the_fixed_canonical_order_regardless_of_completion_order() {
        use axum::extract::Json;
        use axum::http::StatusCode;
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;

        async fn chat(Json(body): Json<Value>) -> Response {
            let effort = body
                .get("reasoning_effort")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // Accept a scattered, non-adjacent subset (skip minimal/low/xhigh)
            // so canonical order cannot be mistaken for candidate-list order
            // by coincidence.
            if matches!(effort, "none" | "medium" | "high") {
                Json(json!({"choices": [{"message": {"content": "ok"}}]})).into_response()
            } else {
                StatusCode::BAD_REQUEST.into_response()
            }
        }

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let outcome = probe_reasoning_efforts(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "provider-model",
            WireFormat::Chat,
            EffortMetadata::default(),
        )
        .await;
        server.abort();

        match outcome {
            EffortProbeOutcome::Supported { levels, .. } => {
                assert_eq!(levels, vec!["none", "medium", "high"]);
            }
            other => panic!("expected Supported, got {other:?}"),
        }
    }

    /// A provider that declares its own Effort vocabulary/order in `/models`
    /// keeps that catalog order verbatim -- it is never re-sorted against
    /// Vellum's own none→minimal→low→medium→high→xhigh strength ordering.
    #[tokio::test]
    async fn effort_probe_preserves_provider_declared_order_not_canonical_order() {
        use axum::extract::Json;
        use axum::http::StatusCode;
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;

        async fn chat(Json(body): Json<Value>) -> Response {
            let effort = body
                .get("reasoning_effort")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if matches!(effort, "turbo" | "deep" | "max") {
                Json(json!({"choices": [{"message": {"content": "ok"}}]})).into_response()
            } else {
                StatusCode::BAD_REQUEST.into_response()
            }
        }

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // Provider's own catalog order is deliberately not alphabetical and
        // not Vellum's canonical strength order.
        let outcome = probe_reasoning_efforts(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "provider-model",
            WireFormat::Chat,
            EffortMetadata {
                levels: vec!["turbo".into(), "max".into(), "deep".into()],
                default: None,
            },
        )
        .await;
        server.abort();

        match outcome {
            EffortProbeOutcome::Supported { levels, .. } => {
                assert_eq!(levels, vec!["turbo", "max", "deep"]);
            }
            other => panic!("expected Supported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn probe_diagnostics_records_attempts_and_classifies_tool_call_missing_xml() {
        use axum::extract::Json;
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;

        async fn chat(Json(_body): Json<Value>) -> Response {
            // Return 200 with raw <tool_call> in plain text content, not typed tool call
            Json(json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "<tool_call>{\"name\": \"vellum_probe_tool\", \"arguments\": {\"value\": \"ok\"}}</tool_call>"
                    }
                }]
            }))
            .into_response()
        }

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let capability = probe_model_capability(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "qwen-mock".to_string(),
            WireFormat::Chat,
            Some(196_608),
            EffortMetadata::default(),
            &RuntimeChatCapabilities::default(),
        )
        .await;
        server.abort();

        assert_eq!(capability.tool_calling, Some(false));
        assert_eq!(capability.last_probe_failed, Some(true));
        assert_eq!(capability.context_window, Some(196_608));
        assert_eq!(capability.probe_issue.as_deref(), Some("tool_call_missing"));
        assert!(!capability.probe_attempts.is_empty());
        let tool_attempt = capability
            .probe_attempts
            .iter()
            .find(|a| a.stage == "typedTool")
            .expect("should record typedTool attempt");
        assert_eq!(tool_attempt.outcome, "tool_call_missing");
        assert_eq!(
            tool_attempt.model_response_kind.as_deref(),
            Some("xml_tags")
        );
    }

    #[tokio::test]
    async fn probe_diagnostics_records_quota_429_with_retry_after() {
        use axum::http::StatusCode;
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;

        async fn chat() -> Response {
            (
                StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", "45")],
                axum::Json(json!({"error": {"message": "Rate limit exceeded"}})),
            )
                .into_response()
        }

        let app = Router::new().route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let capability = probe_model_capability(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "mimo-mock".to_string(),
            WireFormat::Chat,
            Some(128_000),
            EffortMetadata::default(),
            &RuntimeChatCapabilities::default(),
        )
        .await;
        server.abort();

        assert_eq!(capability.probe_issue.as_deref(), Some("provider_quota"));
        assert_eq!(capability.last_probe_failed, Some(true));
        let attempt = capability
            .probe_attempts
            .iter()
            .find(|a| a.outcome == "provider_quota")
            .expect("should record provider_quota attempt");
        assert_eq!(attempt.status, Some(429));
        assert_eq!(attempt.retry_after, Some(45));
    }

    #[tokio::test]
    async fn qwen_effort_reprobe_accepts_explicit_template_validation_wrapped_in_500() {
        use axum::extract::Json;
        use axum::http::StatusCode;
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;

        async fn responses(Json(body): Json<Value>) -> Response {
            let effort = body
                .pointer("/reasoning/effort")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // Provider 806's Responses adapter accepts `none` (disable
            // reasoning) plus the Qwen template's low / medium / xhigh. It
            // wraps the Jinja validation exception for the other values in
            // HTTP 500 instead of 400.
            if matches!(effort, "none" | "low" | "medium" | "xhigh") {
                Json(json!({"output": [{"type": "message", "content": [{"type": "output_text", "text": "ok"}]}]})).into_response()
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": {"code": 500, "type": "server_error", "message": format!(
                        "Jinja Exception: Unexpected reasoning effort {effort}. Supported types are xhigh (default), medium, and low."
                    )}})),
                )
                    .into_response()
            }
        }

        let app = Router::new().route("/v1/responses", post(responses));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let outcome = probe_reasoning_efforts(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "qwen",
            WireFormat::Responses,
            EffortMetadata::default(),
        )
        .await;
        server.abort();

        match outcome {
            EffortProbeOutcome::Supported {
                levels, transport, ..
            } => {
                // Must reflect the live endpoint: `none` plus the three Qwen
                // levels, with minimal/high filtered by explicit validation.
                assert_eq!(levels, vec!["none", "low", "medium", "xhigh"]);
                assert_eq!(transport, ReasoningEffortTransport::ResponsesObject);
            }
            other => panic!("expected Supported, got {other:?}"),
        }
    }

    #[test]
    fn probe_diagnostic_bounding_is_utf8_safe_and_never_exceeds_the_limit() {
        let message = "錯誤".repeat(200);
        let bounded = bounded_probe_message(&message);
        assert_eq!(bounded.chars().count(), 240);
        assert!(bounded.ends_with('…'));
    }

    #[test]
    fn ignored_effort_negative_control_explains_that_retry_is_not_actionable() {
        let diagnostic = effort_probe_diagnostic("provider_ignores_unknown_effort");
        assert!(diagnostic.contains("invalid Effort value"));
        assert!(diagnostic.contains("unlikely to change"));
    }

    #[test]
    fn generic_500_is_not_mistaken_for_effort_validation() {
        let generic = Ok((
            500,
            json!({"error": {"message": "reasoning effort worker crashed"}}),
            None,
        ));
        assert_eq!(
            classify_effort_candidate(&generic),
            EffortCandidateOutcome::Inconclusive
        );
    }

    #[test]
    fn non_json_success_is_a_provider_protocol_failure() {
        let raw = RawProbeResponse {
            status: Some(200),
            body: Value::Null,
            content_type: Some("text/plain".into()),
            retry_after: None,
            duration_ms: 12,
            timeout: false,
            error: Some("non-JSON response body: upstream exploded".into()),
        };
        let (attempt, issue) = classify_raw_probe(&raw, "completion", Some(WireFormat::Responses));
        assert_eq!(attempt.outcome, "provider_protocol");
        assert_eq!(issue.as_deref(), Some("provider_protocol"));
    }

    #[tokio::test]
    async fn generic_failed_probe_returns_structured_attempts_instead_of_opaque_error() {
        use axum::extract::Json;
        use axum::routing::{get, post};
        use axum::Router;

        async fn models() -> Json<Value> {
            Json(json!({"data": [{"id": "qwen", "meta": {"n_ctx": 196608}}]}))
        }

        async fn responses() -> Json<Value> {
            Json(json!({
                "status": "completed",
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "plain"}]}]
            }))
        }

        async fn chat() -> Json<Value> {
            Json(json!({"choices": [{"message": {"content": "plain"}}]}))
        }

        let app = Router::new()
            .route("/v1/models", get(models))
            .route("/v1/responses", post(responses))
            .route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let capability = probe_model_capability_with_client(
            &default_client().unwrap(),
            &format!("http://{address}/v1"),
            None,
            "qwen",
        )
        .await
        .expect("a negative capability verdict is still a diagnostic success");
        server.abort();

        assert_eq!(capability.context_window, Some(196_608));
        assert_eq!(capability.last_probe_failed, Some(true));
        assert!(capability
            .probe_attempts
            .iter()
            .any(|attempt| attempt.stage == "catalog" && attempt.outcome == "success"));
        assert!(capability
            .probe_attempts
            .iter()
            .any(|attempt| attempt.stage == "typedTool"));
    }
}
