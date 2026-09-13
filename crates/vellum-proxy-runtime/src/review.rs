//! Auto Review / Guardian pipeline (plan M9).
//!
//! Ported from Desktop's `src-tauri/src/review.rs` so the shared runtime
//! detects guardian requests, resolves the review route plan (primary /
//! failover), prepares the guardian body, and parses guardian assessments and
//! review findings with the same semantics. The Desktop UI only configures
//! policy; execution lives here.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

/// The stable catalog id Codex uses for approval-review requests.
pub const AUTO_REVIEW_MODEL: &str = "codex-auto-review";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewPolicy {
    #[default]
    Always,
    Failover,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSettings {
    pub on_edit: bool,
    pub before_send: bool,
    pub before_compact: bool,
    #[serde(default)]
    pub route_id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub policy: Option<ReviewPolicy>,
    #[serde(default)]
    pub fallback_catalog_id: Option<String>,
    /// Which Vellum-managed ChatGPT account pays for a review that runs on
    /// the Official plane, independent of the account the user's own turns
    /// bill to. `None` keeps the default account, which is the behaviour
    /// every non-Official reviewer and every pre-existing config already
    /// has. Only an account *id* is stored here -- never a token -- so this
    /// value is safe to persist and to deploy to a remote host, and the
    /// resolved authorization is verified against it before the review is
    /// dispatched (see `ResolvedAuth::resolve`).
    #[serde(default)]
    pub official_account_id: Option<String>,
}

/// Where a running [`crate::exec::ProxyRuntime`] gets *this request's*
/// [`ReviewSettings`] from. The bug this seam fixes: a shared agent that
/// copies `ReviewSettings` once at startup and never looks again means a
/// user flipping Guardian policy in the UI has no effect on an
/// already-running proxy until it restarts. Every source implementation must
/// read whatever is authoritative *right now* — never memoize a value across
/// calls — so each new Guardian request gets the latest policy while a
/// request already in flight keeps whatever it captured at admission (see
/// [`crate::snapshot::RuntimeSnapshot`]).
pub trait ReviewSettingsSource: Send + Sync {
    fn current(&self) -> ReviewSettings;
}

/// A source that never changes after construction. Used by the headless
/// daemon, whose config is loaded once at startup, and as the default for any
/// [`crate::exec::ProxyRuntime`] that never configured Auto Review.
#[derive(Debug, Clone)]
pub struct StaticReviewSettingsSource(pub ReviewSettings);

impl ReviewSettingsSource for StaticReviewSettingsSource {
    fn current(&self) -> ReviewSettings {
        self.0.clone()
    }
}

/// Build the default review settings source (static, empty policy).
pub fn default_review_settings_source() -> Arc<dyn ReviewSettingsSource> {
    Arc::new(StaticReviewSettingsSource(ReviewSettings::default()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRoutePlan {
    pub primary_catalog_id: String,
    pub fallback_catalog_id: Option<String>,
}

pub fn effective_review_policy(settings: &ReviewSettings) -> ReviewPolicy {
    settings.policy.unwrap_or(ReviewPolicy::Always)
}

/// A minimal route view for plan resolution.
#[derive(Debug, Clone)]
pub struct ReviewModelRoute {
    pub route_id: String,
    pub catalog_id: String,
    pub upstream_model: String,
}

pub fn resolve_guardian_route_plan(
    settings: &ReviewSettings,
    models: &[ReviewModelRoute],
) -> Result<ReviewRoutePlan, String> {
    let policy = effective_review_policy(settings);
    let model_matches = |candidate: &&ReviewModelRoute| {
        !settings.model.trim().is_empty()
            && (candidate.catalog_id == settings.model
                || candidate
                    .upstream_model
                    .eq_ignore_ascii_case(&settings.model))
    };
    let configured_route_exists = models
        .iter()
        .any(|candidate| candidate.route_id == settings.route_id);
    let primary = models
        .iter()
        .find(|candidate| {
            candidate.route_id == settings.route_id
                && (settings.model.trim().is_empty() || model_matches(candidate))
        })
        .or_else(|| {
            // Route ids are local configuration identities and can change when
            // a provider is recreated. Recover only when the configured route
            // is *gone*, and only from an unambiguous model match.
            //
            // Both halves of that guard matter. A route that still exists but
            // does not carry the configured model is an ordinary
            // misconfiguration, and silently reviewing on whichever other
            // provider happens to offer that model name would move the trust
            // boundary the user chose without telling them -- the same hazard
            // as guessing among duplicates.
            if configured_route_exists {
                return None;
            }
            let mut matches = models.iter().filter(model_matches);
            let candidate = matches.next()?;
            matches.next().is_none().then_some(candidate)
        })
        .ok_or_else(|| "Auto Review could not resolve a primary reviewer route".to_string())?;

    let fallback_catalog_id = if policy == ReviewPolicy::Failover {
        let configured = settings
            .fallback_catalog_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "Auto Review requires a fallback catalog id".to_string())?;
        let fallback = models
            .iter()
            .find(|candidate| candidate.catalog_id == configured)
            .ok_or_else(|| "Auto Review fallback route is not configured".to_string())?;
        let primary_route_id = models
            .iter()
            .find(|candidate| candidate.catalog_id == primary.catalog_id)
            .map(|candidate| candidate.route_id.as_str())
            .unwrap_or(primary.route_id.as_str());
        if fallback.route_id == primary_route_id {
            return Err("Auto Review primary and fallback must use different routes".to_string());
        }
        Some(fallback.catalog_id.clone())
    } else {
        None
    };

    Ok(ReviewRoutePlan {
        primary_catalog_id: primary.catalog_id.clone(),
        fallback_catalog_id,
    })
}

/// Codex Desktop may keep the thread model id for Guardian requests, so model
/// name alone is not sufficient. These instruction markers are the stable
/// approval-review contract used by Codex.
pub fn is_guardian_request(body: &Value) -> bool {
    is_guardian_request_with_upstream_model(body, None)
}

pub fn is_guardian_request_with_upstream_model(body: &Value, upstream_model: Option<&str>) -> bool {
    if body.get("model").and_then(Value::as_str) == Some(AUTO_REVIEW_MODEL)
        || upstream_model.is_some_and(|model| model.eq_ignore_ascii_case(AUTO_REVIEW_MODEL))
    {
        return true;
    }
    let instructions = match body.get("instructions") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    instructions.contains("You are judging one planned coding-agent action.")
        && instructions.contains("# User Authorization Scoring")
        && instructions.contains("# Outcome Policy")
}

const GUARDIAN_OUTPUT_DIRECTIVE: &str = "Assess the planned action above. Return exactly one JSON object and no Markdown: {\"outcome\":\"allow\"}, or a deny object with outcome, risk_level, user_authorization, and rationale.";

/// Maximum estimated size of one reviewer request, independent of the
/// reviewer's advertised context window. Live 806/Qwen qualification with a
/// 1,024-token output allowance stayed valid through 64K input tokens, while
/// forwarding most of a 500K parent context only increased tail latency and
/// previously exposed stale/optimistic provider window declarations.
const GUARDIAN_MAX_INPUT_TOKENS: u64 = 65_536;

/// Prepare one reviewer leg for the route that will actually receive it.
/// Guardian prompts can contain the parent task's large context, while a
/// fallback reviewer may have a much smaller window. The reviewer projection
/// is therefore bounded per route rather than blindly replayed from primary to
/// fallback. The stable Guardian policy and planned action live in
/// `instructions`; when reduction is necessary we preserve its beginning and
/// end and replace inherited input history with one explicit assessment turn.
pub fn prepare_guardian_request(body: &mut Value, catalog_id: &str, context_window: Option<u64>) {
    body["model"] = Value::String(catalog_id.to_string());
    // Guardian is a bounded, single-assessment exchange: it must not inherit
    // continuation state or the parent agent's tool surface.
    //
    // `stream` is deliberately left as the client sent it. Failover already
    // sees a complete assessment before anything is replayed --
    // `drain_and_validate_guardian_sse` buffers every chunk of a streaming leg
    // and replays it byte-for-byte only after a terminal event validates, so
    // forcing `stream: false` here would buy no additional safety. It would
    // cost correctness twice over: the reviewer upstream is asked for a shape
    // the client did not request, and the winning leg then reaches Codex as a
    // JSON body on a request that asked for an event stream.
    if let Some(object) = body.as_object_mut() {
        for field in [
            "previous_response_id",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
        ] {
            object.remove(field);
        }
    }
    append_guardian_output_directive(body);
    if let Some(existing) = body.get("max_output_tokens").and_then(Value::as_u64) {
        body["max_output_tokens"] = Value::from(existing.min(1024));
    } else {
        body["max_output_tokens"] = Value::from(1024);
    }
    bound_guardian_request(body, context_window);
}

fn guardian_user_item(text: impl Into<String>) -> Value {
    json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text.into()}]
    })
}

fn append_guardian_output_directive(body: &mut Value) {
    let directive = guardian_user_item(GUARDIAN_OUTPUT_DIRECTIVE);
    match body.get_mut("input") {
        Some(Value::Array(items)) => items.push(directive),
        Some(Value::String(text)) => {
            let original = guardian_user_item(std::mem::take(text));
            body["input"] = Value::Array(vec![original, directive]);
        }
        _ => body["input"] = Value::Array(vec![directive]),
    }
}

fn bound_guardian_request(body: &mut Value, context_window: Option<u64>) {
    // Leave both a fixed output/adapter reserve and 20% tokenizer headroom
    // when the route publishes a window. Also enforce the live-qualified 64K
    // Guardian ceiling when the route has a much larger (or unknown) window:
    // Auto Review needs the authorization and planned-action boundaries, not
    // a verbatim replay of a 500K parent transcript.
    let route_target = context_window
        .filter(|window| *window > 0)
        .map(|window| {
            window
                .saturating_sub(8_192)
                .min(window.saturating_mul(4) / 5)
                .max(1_024)
        })
        .unwrap_or(GUARDIAN_MAX_INPUT_TOKENS);
    let target = route_target.min(GUARDIAN_MAX_INPUT_TOKENS);
    if crate::compaction::estimate_json_tokens(body) <= target {
        return;
    }

    // Inherited parent history is context for the action, not the action
    // authority. The Guardian contract and planned action are in
    // `instructions`; retain one explicit user turn so Chat adapters never
    // receive a system-only payload.
    body["input"] = Value::Array(vec![guardian_user_item(GUARDIAN_OUTPUT_DIRECTIVE)]);
    let instructions = body
        .get("instructions")
        .map(instruction_text)
        .unwrap_or_default();
    if instructions.is_empty() {
        return;
    }

    let mut allowed_bytes = target.saturating_mul(3).min(usize::MAX as u64) as usize;
    loop {
        body["instructions"] = Value::String(truncate_middle(&instructions, allowed_bytes));
        if crate::compaction::estimate_json_tokens(body) <= target || allowed_bytes <= 1_024 {
            break;
        }
        allowed_bytes = allowed_bytes.saturating_mul(3) / 4;
    }
}

fn instruction_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn truncate_middle(text: &str, max_bytes: usize) -> String {
    const MARKER: &str =
        "\n\n[... earlier Guardian context omitted to fit reviewer window ...]\n\n";
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let content_budget = max_bytes.saturating_sub(MARKER.len());
    let head_target = content_budget / 2;
    let tail_target = content_budget.saturating_sub(head_target);
    let mut head_end = head_target.min(text.len());
    while head_end > 0 && !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len().saturating_sub(tail_target);
    while tail_start < text.len() && !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!("{}{}{}", &text[..head_end], MARKER, &text[tail_start..])
}

pub fn has_guardian_assessment(response: &Value) -> bool {
    guardian_assessment(response).is_some()
}

pub fn guardian_assessment(response: &Value) -> Option<Value> {
    if value_has_guardian_decision(response) {
        return Some(response.clone());
    }
    response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|item| {
            item.get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .find_map(parse_guardian_assessment_text)
}

pub fn guardian_assessment_from_chat_body(body: &Value) -> Option<Value> {
    let message = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))?;
    let mut candidates = Vec::new();
    if let Some(content) = message.get("content") {
        collect_guardian_decisions_from_value(content, &mut candidates);
    }
    if let Some(reasoning) = message.get("reasoning_content") {
        collect_guardian_decisions_from_value(reasoning, &mut candidates);
    }
    candidates.dedup();
    (candidates.len() == 1).then(|| candidates.remove(0))
}

fn collect_guardian_decisions_from_value(value: &Value, candidates: &mut Vec<Value>) {
    match value {
        Value::String(text) => collect_guardian_decisions_from_text(text, candidates),
        Value::Array(values) => {
            for value in values {
                collect_guardian_decisions_from_value(value, candidates);
            }
        }
        Value::Object(object) => {
            if let Some(text) = object.get("text") {
                collect_guardian_decisions_from_value(text, candidates);
            } else if value_has_guardian_decision(value) {
                candidates.push(value.clone());
            }
        }
        _ => {}
    }
}

fn collect_guardian_decisions_from_text(text: &str, candidates: &mut Vec<Value>) {
    if let Some(value) = parse_guardian_assessment_text(text) {
        candidates.push(value);
        return;
    }
    for candidate in balanced_json_objects(text) {
        if let Ok(value) = serde_json::from_str::<Value>(candidate) {
            if value_has_guardian_decision(&value) {
                candidates.push(value);
            }
        }
    }
}

fn balanced_json_objects(text: &str) -> Vec<&str> {
    let mut objects = Vec::new();
    let mut start = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' if start.is_some() => in_string = true,
            '{' => {
                if depth == 0 {
                    start = Some(index);
                }
                depth += 1;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = start.take() {
                        objects.push(&text[start..index + character.len_utf8()]);
                    }
                }
            }
            _ => {}
        }
    }
    objects
}

fn parse_guardian_assessment_text(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    let candidate = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```JSON"))
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|value| value.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    serde_json::from_str::<Value>(candidate)
        .ok()
        .filter(value_has_guardian_decision)
}

fn value_has_guardian_decision(value: &Value) -> bool {
    match value.get("outcome").and_then(Value::as_str) {
        // Codex explicitly permits this compact shape for low-risk actions.
        Some("allow") => true,
        // Denials use the full Guardian schema so the caller receives the
        // actual risk and authorization rationale, not an ambiguous verdict.
        Some("deny") => {
            value
                .get("risk_level")
                .and_then(Value::as_str)
                .is_some_and(|risk| matches!(risk, "low" | "medium" | "high" | "critical"))
                && value
                    .get("user_authorization")
                    .and_then(Value::as_str)
                    .is_some_and(|authorization| {
                        matches!(authorization, "unknown" | "low" | "medium" | "high")
                    })
                && value
                    .get("rationale")
                    .and_then(Value::as_str)
                    .is_some_and(|rationale| !rationale.trim().is_empty())
        }
        _ => false,
    }
}

/// The findings schema Codex renders in the review UI.
pub fn findings_schema() -> Value {
    json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "severity": {"type": "string", "enum": ["critical", "warning", "info"]},
                "title": {"type": "string"},
                "location": {"type": "string"}
            },
            "required": ["id", "severity", "title", "location"]
        }
    })
}

pub fn build_review_prompt(content: &str) -> Value {
    json!([{
        "type": "text",
        "text": format!(
            "You are reviewing one planned coding-agent action before it is sent.\n\
             Return a findings array using this schema: {}\n\n\
             Action to review:\n{}",
            findings_schema(),
            content
        )
    }])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    Warning,
    Info,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub severity: Severity,
    pub title: String,
    pub location: String,
}

pub fn severity_rank(s: Severity) -> u8 {
    match s {
        Severity::Critical => 2,
        Severity::Warning => 1,
        Severity::Info => 0,
    }
}

pub fn parse_severity(s: &str) -> Option<Severity> {
    match s.to_ascii_lowercase().as_str() {
        "critical" => Some(Severity::Critical),
        "warning" | "warn" => Some(Severity::Warning),
        "info" | "informational" => Some(Severity::Info),
        _ => None,
    }
}

pub fn parse_findings(body: &Value) -> Vec<Finding> {
    let findings = body
        .get("findings")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    findings
        .iter()
        .filter_map(|finding| {
            let id = finding.get("id").and_then(Value::as_str)?.to_string();
            let severity = parse_severity(
                finding
                    .get("severity")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )?;
            let title = finding
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let location = finding
                .get("location")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(Finding {
                id,
                severity,
                title,
                location,
            })
        })
        .collect()
}

pub fn sort_and_dedupe_findings(mut findings: Vec<Finding>) -> Vec<Finding> {
    findings.sort_by_key(|finding| {
        (
            std::cmp::Reverse(severity_rank(finding.severity)),
            finding.id.clone(),
        )
    });
    findings.dedup_by(|a, b| a.id == b.id);
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guardian_request_is_detected_by_model_and_markers() {
        assert!(is_guardian_request(&json!({"model": AUTO_REVIEW_MODEL})));
        let markers = json!({
            "model": "thread-model",
            "instructions": [
                {"type": "text", "text": "You are judging one planned coding-agent action."},
                {"type": "text", "text": "# User Authorization Scoring"},
                {"type": "text", "text": "# Outcome Policy"}
            ]
        });
        assert!(is_guardian_request(&markers));
        assert!(!is_guardian_request(
            &json!({"model": "plain", "instructions": []})
        ));
    }

    #[test]
    fn guardian_projection_adds_a_semantic_user_turn_for_chat_reviewers() {
        let mut body = json!({
            "model": AUTO_REVIEW_MODEL,
            "instructions": "You are judging one planned coding-agent action.\n# User Authorization Scoring\n# Outcome Policy"
        });
        prepare_guardian_request(&mut body, "reviewer", Some(88_064));
        assert_eq!(body["model"], "reviewer");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.last().unwrap()["role"], "user");
        assert!(input
            .last()
            .unwrap()
            .to_string()
            .contains("exactly one JSON object"));
        assert_eq!(body["max_output_tokens"], 1024);
    }

    #[test]
    fn guardian_projection_fits_the_fallback_reviewers_window() {
        let mut body = json!({
            "model": AUTO_REVIEW_MODEL,
            "instructions": format!(
                "You are judging one planned coding-agent action.\n# User Authorization Scoring\n{}\n# Outcome Policy\nPLANNED_ACTION_AT_THE_END",
                "large context ".repeat(60_000)
            ),
            "input": [{"role":"user", "content":"old parent history".repeat(50_000)}]
        });
        prepare_guardian_request(&mut body, "qwen-reviewer", Some(88_064));
        assert!(crate::compaction::estimate_json_tokens(&body) <= 88_064 * 4 / 5);
        let projected = body["instructions"].as_str().unwrap();
        assert!(projected.starts_with("You are judging one planned coding-agent action."));
        assert!(projected.ends_with("PLANNED_ACTION_AT_THE_END"));
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn guardian_projection_reduces_a_500k_parent_context_to_the_qualified_cap() {
        let mut body = json!({
            "model": AUTO_REVIEW_MODEL,
            "instructions": format!(
                "You are judging one planned coding-agent action.\n# User Authorization Scoring\n{}\n# Outcome Policy\nPLANNED_ACTION_AT_THE_END",
                "four token inherited context ".repeat(500_000)
            ),
            "input": [{"role":"user", "content":"old parent history".repeat(500_000)}]
        });
        prepare_guardian_request(&mut body, "qwen-reviewer", Some(500_000));
        assert!(
            crate::compaction::estimate_json_tokens(&body) <= GUARDIAN_MAX_INPUT_TOKENS,
            "500K parent context was not reduced to the qualified Guardian cap"
        );
        let projected = body["instructions"].as_str().unwrap();
        assert!(projected.starts_with("You are judging one planned coding-agent action."));
        assert!(projected.ends_with("PLANNED_ACTION_AT_THE_END"));
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn plan_resolution_supports_failover() {
        let models = vec![
            ReviewModelRoute {
                route_id: "primary".into(),
                catalog_id: "primary-catalog".into(),
                upstream_model: "primary-reviewer".into(),
            },
            ReviewModelRoute {
                route_id: "backup".into(),
                catalog_id: "backup-catalog".into(),
                upstream_model: "backup-reviewer".into(),
            },
        ];
        let settings = ReviewSettings {
            route_id: "primary".into(),
            model: "primary-reviewer".into(),
            policy: Some(ReviewPolicy::Failover),
            fallback_catalog_id: Some("backup-catalog".into()),
            ..Default::default()
        };
        let plan = resolve_guardian_route_plan(&settings, &models).unwrap();
        assert_eq!(plan.primary_catalog_id, "primary-catalog");
        assert_eq!(plan.fallback_catalog_id.as_deref(), Some("backup-catalog"));
    }

    #[test]
    fn plan_resolution_recovers_stale_route_only_from_a_unique_model() {
        let unique = vec![ReviewModelRoute {
            route_id: "replacement-route".into(),
            catalog_id: "replacement-catalog".into(),
            upstream_model: "reviewer-model".into(),
        }];
        let settings = ReviewSettings {
            route_id: "deleted-route".into(),
            model: "reviewer-model".into(),
            ..Default::default()
        };
        assert_eq!(
            resolve_guardian_route_plan(&settings, &unique)
                .unwrap()
                .primary_catalog_id,
            "replacement-catalog"
        );

        let mut ambiguous = unique.clone();
        ambiguous.push(ReviewModelRoute {
            route_id: "another-route".into(),
            catalog_id: "another-catalog".into(),
            upstream_model: "reviewer-model".into(),
        });
        assert!(resolve_guardian_route_plan(&settings, &ambiguous).is_err());

        // The configured route still exists but does not carry the configured
        // model. That is a misconfiguration, not a renamed provider: recovering
        // onto `replacement-route` would review the action on a provider the
        // user never selected.
        let mut live_route = unique;
        live_route.push(ReviewModelRoute {
            route_id: "deleted-route".into(),
            catalog_id: "other-catalog".into(),
            upstream_model: "some-other-model".into(),
        });
        assert!(
            resolve_guardian_route_plan(&settings, &live_route).is_err(),
            "a live route with the wrong model must not silently switch provider"
        );
    }

    #[test]
    fn guardian_request_is_stateless_and_tool_free_without_changing_transport() {
        let mut body = json!({
            "model": "parent",
            "stream": true,
            "previous_response_id": "resp-parent",
            "tools": [{"type": "function", "name": "shell"}],
            "tool_choice": "auto",
            "parallel_tool_calls": true
        });
        prepare_guardian_request(&mut body, "reviewer-catalog", None);
        assert_eq!(body["model"], "reviewer-catalog");
        assert_eq!(
            body["stream"], true,
            "the reviewer leg keeps the transport the client asked for;              the dispatcher buffers it, the request shape does not"
        );
        for field in [
            "previous_response_id",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
        ] {
            assert!(body.get(field).is_none(), "Guardian leaked {field}");
        }
    }

    #[test]
    fn guardian_assessment_parses_from_response_text() {
        let response = json!({
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "{\"outcome\":\"allow\"}"}]
            }]
        });
        let assessment = guardian_assessment(&response).unwrap();
        assert_eq!(assessment["outcome"], "allow");
    }

    #[test]
    fn findings_parse_sort_and_dedupe() {
        let body = json!({
            "findings": [
                {"id": "f1", "severity": "warning", "title": "t1", "location": "l1"},
                {"id": "f2", "severity": "critical", "title": "t2", "location": "l2"},
                {"id": "f1", "severity": "warning", "title": "dup", "location": "l1"}
            ]
        });
        let findings = sort_and_dedupe_findings(parse_findings(&body));
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].id, "f2");
        assert_eq!(findings[0].severity, Severity::Critical);
    }
}
