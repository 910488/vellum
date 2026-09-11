//! 自動審查（doc/06）。
//!
//! 組審查 prompt → 呼叫模型（要求結構化輸出）→ 解析成 Vec<Finding> →
//! 按嚴重度排序、去重。原則：沒發現問題就不輸出任何東西。
//!
//! 純函式（`build_review_prompt`、`parse_findings`、`sort_and_dedupe_findings`、
//! `severity_rank`）不碰網路，可單測。`run_review_with_client` 接收注入的
//! reqwest client，正式用預設、測試用 mock。

use crate::error::{AppError, AppResult};
use crate::model::{Finding, ModelRoute, ReviewPolicy, ReviewSettings, Severity, WireFormat};
use serde_json::{json, Value};

pub const AUTO_REVIEW_MODEL: &str = "codex-auto-review";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRoutePlan {
    pub primary_catalog_id: String,
    pub fallback_catalog_id: Option<String>,
}

pub fn effective_review_policy(settings: &ReviewSettings) -> ReviewPolicy {
    settings.policy.unwrap_or(ReviewPolicy::Always)
}

pub fn resolve_guardian_route_plan(
    settings: &ReviewSettings,
    models: &[ModelRoute],
) -> AppResult<ReviewRoutePlan> {
    let runtime_settings = vellum_proxy_runtime::ReviewSettings {
        on_edit: settings.on_edit,
        before_send: settings.before_send,
        before_compact: settings.before_compact,
        route_id: settings.route_id.clone(),
        model: settings.model.clone(),
        policy: settings.policy.map(|policy| match policy {
            ReviewPolicy::Always => vellum_proxy_runtime::ReviewPolicy::Always,
            ReviewPolicy::Failover => vellum_proxy_runtime::ReviewPolicy::Failover,
        }),
        fallback_catalog_id: settings.fallback_catalog_id.clone(),
        // Route resolution picks *which model* reviews, never who pays for
        // it. Left unset so this stays a pure projection of the fields the
        // planner reads.
        official_account_id: None,
    };
    let runtime_models = models
        .iter()
        .map(|model| vellum_proxy_runtime::ReviewModelRoute {
            route_id: model.route_id.clone(),
            catalog_id: model.catalog_id.clone(),
            upstream_model: model.upstream_model.clone(),
        })
        .collect::<Vec<_>>();
    let plan = vellum_proxy_runtime::resolve_guardian_route_plan(
        &runtime_settings,
        &runtime_models,
    )
    .map_err(|error| {
        AppError::Message(
            if error.contains("could not resolve a primary reviewer route") {
                "Auto Review 找不到可用的主要模型；請在 Vellum 設定中重新選擇 Provider 與模型"
                    .into()
            } else if error.contains("requires a fallback catalog id") {
                "Auto Review 已啟用備援，但尚未指定備援模型".into()
            } else if error.contains("fallback route is not configured") {
                "Auto Review 的備援模型目前不可用".into()
            } else if error.contains("must use different routes") {
                "Auto Review 的主要模型與備援模型必須使用不同 Provider".into()
            } else {
                error
            },
        )
    })?;
    Ok(ReviewRoutePlan {
        primary_catalog_id: plan.primary_catalog_id,
        fallback_catalog_id: plan.fallback_catalog_id,
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

pub fn prepare_guardian_request(body: &mut Value, catalog_id: &str) {
    body["model"] = Value::String(catalog_id.to_string());
    // Buffer the small approval response. This lets Vellum verify that the
    // reviewer actually returned an assessment before replying to Codex, and
    // safely try the configured fallback without partially forwarding a
    // broken stream.
    body["stream"] = Value::Bool(false);
    if let Some(object) = body.as_object_mut() {
        // A Guardian request already contains the transcript and the exact
        // proposed action that must be assessed.  The response from the
        // previous Guardian run is only an allow/deny verdict; resuming from
        // it makes otherwise independent reviews grow into one artificial
        // conversation and eventually exhausts the reviewer context window.
        // Keep reviews stateless instead.
        object.remove("previous_response_id");
        // The transcript and exact proposed action are already included.
        // Removing tools prevents substitute reviewers from spending the
        // approval window exploring instead of returning allow/deny JSON.
        object.remove("tools");
        object.remove("tool_choice");
        object.remove("parallel_tool_calls");
    }
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

/// 審查的上下文：要審的內容 + 用的模型 + 端點。
#[derive(Debug, Clone)]
pub struct ReviewContext {
    /// 要審查的內容（diff、即將送出的請求、或對話片段）。
    pub content: String,
    /// 審查用的模型（空字串 → 用同一條線路的模型，doc/06）。
    pub model: String,
    /// 端點根 URL（含 /v1 或不含皆可，與 probe 一致）。
    pub endpoint: String,
    /// API key（可空）。
    pub api_key: Option<String>,
}

/// 給模型看的 JSON schema：要求回傳 findings 陣列（純函式）。
pub fn findings_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "severity": {
                            "type": "string",
                            "enum": ["critical", "warning", "info"]
                        },
                        "title": { "type": "string" },
                        "location": { "type": "string" }
                    },
                    "required": ["severity", "title", "location"]
                }
            }
        },
        "required": ["findings"]
    })
}

/// 組審查 prompt（純函式）。要求只回結構化 JSON，沒問題就回空陣列。
pub fn build_review_prompt(content: &str) -> Value {
    json!({
        "model": "review",
        "messages": [
            {
                "role": "system",
                "content": "你是程式碼審查員。審查以下內容，找出潛在問題。\
                    只回 JSON：{\"findings\": [{\"severity\",\"title\",\"location\"}]}。\
                    severity 只能是 critical / warning / info。\
                    沒有問題就回 {\"findings\": []}。不要加任何說明文字。"
            },
            {
                "role": "user",
                "content": content
            }
        ],
        "temperature": 0,
        "max_tokens": 1024,
        "stream": false
    })
}

/// 嚴重度的排序鍵：critical=0, warning=1, info=2（純函式）。
pub fn severity_rank(s: Severity) -> u8 {
    match s {
        Severity::Critical => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
    }
}

/// 把字串轉成 Severity（純函式）。不認得的 → None。
pub fn parse_severity(s: &str) -> Option<Severity> {
    match s.to_ascii_lowercase().as_str() {
        "critical" => Some(Severity::Critical),
        "warning" => Some(Severity::Warning),
        "info" => Some(Severity::Info),
        _ => None,
    }
}

/// 從模型回應解析 findings（純函式）。
///
/// 支援兩種包法：
/// 1. 回應本身是 `{"findings": [...]}`
/// 2. 回應的 `choices[0].message.content` 是字串，裡面是 JSON
///    （模型沒完全遵守 schema 時的退路——盡力解 JSON）。
pub fn parse_findings(body: &Value) -> Vec<Finding> {
    // 先試直接的 findings 欄位。
    let raw = body
        .get("findings")
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| {
            // 退路：choices[0].message.content 字串裡的 JSON。
            let content = body
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|arr| arr.first())
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)?;
            serde_json::from_str::<Value>(content)
                .ok()
                .and_then(|v| v.get("findings").and_then(Value::as_array).cloned())
        })
        .or_else(|| {
            let content = body
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
                .collect::<String>();
            serde_json::from_str::<Value>(&content)
                .ok()
                .and_then(|value| value.get("findings").and_then(Value::as_array).cloned())
        });

    let Some(arr) = raw else {
        return Vec::new();
    };

    arr.iter()
        .filter_map(|item| {
            let severity = item
                .get("severity")
                .and_then(Value::as_str)
                .and_then(parse_severity)?;
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())?;
            let location = item
                .get("location")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some(Finding {
                // id 由排序後產生（f1, f2…），這裡先放空。
                id: String::new(),
                severity,
                title: title.to_string(),
                location,
            })
        })
        .collect()
}

/// 按嚴重度排序 + 去重（同一個 title 不報兩次）。回傳的 id 是 f1, f2…（純函式）。
pub fn sort_and_dedupe_findings(mut findings: Vec<Finding>) -> Vec<Finding> {
    // 先按嚴重度排（critical 在上）。
    findings.sort_by_key(|f| severity_rank(f.severity));
    // 去重：title 相同就只留第一個（已排序 → 留較嚴重的）。
    let mut seen = std::collections::HashSet::new();
    findings.retain(|f| seen.insert(f.title.clone()));
    // 重新編號 id。
    for (idx, f) in findings.iter_mut().enumerate() {
        f.id = format!("f{}", idx + 1);
    }
    findings
}

/// 用注入的 client 跑一次審查。端點正規化與 probe 一致。
pub async fn run_review_with_client(
    client: &reqwest::Client,
    ctx: &ReviewContext,
) -> AppResult<Vec<Finding>> {
    let v1 = normalize_v1(&ctx.endpoint);
    let mut body = build_review_prompt(&ctx.content);
    // 把 model 塞進去（空就用 "review" 佔位，端點會拒絕或用預設）。
    if !ctx.model.is_empty() {
        body["model"] = json!(ctx.model);
    }

    let mut req = client.post(format!("{v1}/chat/completions")).json(&body);
    if let Some(k) = &ctx.api_key {
        req = req.bearer_auth(k);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| AppError::Unreachable(e.to_string()))?;
    if !resp.status().is_success() {
        // 審查失敗不能擋主流程（doc/06）→ 回空。
        log::warn!("[Review] 審查請求失敗：HTTP {}", resp.status());
        return Ok(Vec::new());
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| AppError::Message(e.to_string()))?;
    Ok(sort_and_dedupe_findings(parse_findings(&body)))
}

pub async fn run_review_for_wire(
    client: &reqwest::Client,
    ctx: &ReviewContext,
    wire: WireFormat,
) -> AppResult<Vec<Finding>> {
    if wire == WireFormat::Chat {
        return run_review_with_client(client, ctx).await;
    }
    let v1 = normalize_v1(&ctx.endpoint);
    let prompt = build_review_prompt(&ctx.content);
    let mut request = client.post(format!("{v1}/responses")).json(&json!({
        "model": ctx.model,
        "input": prompt["messages"],
        "stream": false,
        "store": false,
        "max_output_tokens": 1024
    }));
    if let Some(key) = &ctx.api_key {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .await
        .map_err(|error| AppError::Unreachable(error.to_string()))?;
    if !response.status().is_success() {
        log::warn!(
            "[Review] Responses 審查請求失敗：HTTP {}",
            response.status()
        );
        return Ok(Vec::new());
    }
    let body: Value = response
        .json()
        .await
        .map_err(|error| AppError::Message(error.to_string()))?;
    Ok(sort_and_dedupe_findings(parse_findings(&body)))
}

/// Headers every first-party review call through the local proxy must carry.
/// The boundary key authenticates the caller; `x-vellum-internal-review`
/// is the review-only marker the runtime already expects.
pub fn build_review_via_proxy_request(
    client: &reqwest::Client,
    catalog_id: &str,
    input: &Value,
    boundary_key: &str,
) -> reqwest::RequestBuilder {
    client
        .post("http://127.0.0.1:15721/v1/responses")
        .header("x-vellum-internal-review", "1")
        .header(vellum_proxy_runtime::BOUNDARY_KEY_HEADER, boundary_key)
        .json(&json!({
            "model": catalog_id,
            "input": input,
            "stream": false,
            "store": false,
            "max_output_tokens": 1024
        }))
}

/// Send a review back through Vellum so the selected provider uses the same
/// OAuth/session/API-key handling as ordinary Codex traffic.
pub async fn run_review_via_proxy(
    client: &reqwest::Client,
    content: &str,
    catalog_id: &str,
    data_root: &std::path::Path,
) -> AppResult<Vec<Finding>> {
    let boundary_key = crate::proxy::ensure_boundary_key(data_root)?;
    let prompt = build_review_prompt(content);
    let response = build_review_via_proxy_request(
        client,
        catalog_id,
        &prompt["messages"],
        boundary_key.expose_for_storage(),
    )
    .send()
    .await
    .map_err(|error| AppError::Unreachable(error.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(200).collect();
        return Err(AppError::Message(if snippet.is_empty() {
            format!("Auto Review via Vellum failed: HTTP {status}")
        } else {
            format!("Auto Review via Vellum failed: HTTP {status}: {snippet}")
        }));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|error| AppError::Message(error.to_string()))?;
    Ok(sort_and_dedupe_findings(parse_findings(&body)))
}

/// 端點正規化：確保以 /v1 結尾（與 probe 對齊）。
fn normalize_v1(endpoint: &str) -> String {
    let root = endpoint.trim_end_matches('/');
    if root.ends_with("/v1") {
        root.to_string()
    } else {
        format!("{root}/v1")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(catalog_id: &str, route_id: &str, upstream_model: &str) -> ModelRoute {
        ModelRoute {
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            reasoning_effort_transport: Default::default(),
            catalog_id: catalog_id.into(),
            display_name: upstream_model.into(),
            route_id: route_id.into(),
            upstream_model: upstream_model.into(),
            context_window: Some(128_000),
            wire: WireFormat::Responses,
            reasoning: true,
            streaming: true,
            vision: false,
        }
    }

    #[test]
    fn review_route_plan_resolves_explicit_always_selection() {
        let settings = ReviewSettings {
            route_id: "openai".into(),
            model: "gpt-5.4-mini".into(),
            policy: Some(ReviewPolicy::Always),
            ..ReviewSettings::default()
        };
        let models = vec![
            model("vlm-glm", "weikuwu", "GLM-5.2"),
            model("gpt-mini", "openai", "gpt-5.4-mini"),
        ];
        let plan = resolve_guardian_route_plan(&settings, &models).unwrap();
        assert_eq!(plan.primary_catalog_id, "gpt-mini");
        assert_eq!(plan.fallback_catalog_id, None);
        assert!(resolve_guardian_route_plan(&ReviewSettings::default(), &models).is_err());
    }

    #[test]
    fn review_route_plan_uses_cross_provider_fallback() {
        let settings = ReviewSettings {
            route_id: "weikuwu".into(),
            model: "GLM-5.2".into(),
            policy: Some(ReviewPolicy::Failover),
            fallback_catalog_id: Some("gpt-mini".into()),
            ..ReviewSettings::default()
        };
        let models = vec![
            model("vlm-glm", "weikuwu", "GLM-5.2"),
            model("gpt-mini", "openai", "gpt-5.4-mini"),
        ];
        let plan = resolve_guardian_route_plan(&settings, &models).unwrap();
        assert_eq!(plan.primary_catalog_id, "vlm-glm");
        assert_eq!(plan.fallback_catalog_id.as_deref(), Some("gpt-mini"));
    }

    #[test]
    fn review_route_plan_rejects_same_provider_fallback() {
        let settings = ReviewSettings {
            route_id: "weikuwu".into(),
            model: "GLM-5.2".into(),
            policy: Some(ReviewPolicy::Failover),
            fallback_catalog_id: Some("vlm-glm-p".into()),
            ..ReviewSettings::default()
        };
        let models = vec![
            model("vlm-glm", "weikuwu", "GLM-5.2"),
            model("vlm-glm-p", "weikuwu", "GLM-5.2p"),
        ];
        assert!(resolve_guardian_route_plan(&settings, &models).is_err());
    }

    #[test]
    fn detects_codex_guardian_by_model_or_instruction_contract() {
        assert!(is_guardian_request(&json!({"model": AUTO_REVIEW_MODEL})));
        assert!(is_guardian_request_with_upstream_model(
            &json!({"model": "vlm-generated-catalog-id"}),
            Some(AUTO_REVIEW_MODEL)
        ));
        assert!(is_guardian_request(&json!({
            "model": "gpt-current",
            "instructions": "You are judging one planned coding-agent action.\n# User Authorization Scoring\n# Outcome Policy"
        })));
        assert!(!is_guardian_request(&json!({"model": "gpt-current"})));
    }

    #[test]
    fn guardian_routing_replaces_model_and_removes_tools() {
        let mut body = json!({
            "model": "gpt-current",
            "tools": [{"type": "shell"}],
            "tool_choice": "auto",
            "parallel_tool_calls": true
        });
        body["previous_response_id"] = json!("resp_previous_guardian");
        prepare_guardian_request(&mut body, "vlm-reviewer");
        assert_eq!(body["model"], "vlm-reviewer");
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("parallel_tool_calls").is_none());
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn guardian_assessment_requires_structured_payload() {
        assert!(has_guardian_assessment(&json!({
            "output": [{
                "content": [{"type": "output_text", "text": "{\"outcome\":\"allow\"}"}]
            }]
        })));
        assert!(has_guardian_assessment(&json!({
            "output": [{
                "content": [{"type": "output_text", "text": "```json\n{\"risk_level\":\"high\",\"user_authorization\":\"low\",\"outcome\":\"deny\",\"rationale\":\"not authorized\"}\n```"}]
            }]
        })));
        assert!(!has_guardian_assessment(&json!({"assessment": "allow"})));
        assert!(!has_guardian_assessment(&json!({"outcome": "maybe"})));
        assert!(!has_guardian_assessment(&json!({"outcome": "deny"})));
        assert!(!has_guardian_assessment(&json!({
            "output": [{
                "content": [{"type": "output_text", "text": "I approve this action."}]
            }]
        })));
    }

    #[test]
    fn guardian_chat_fallback_recovers_one_strict_decision_from_reasoning() {
        let body = json!({
            "choices": [{
                "message": {
                    "content": null,
                    "reasoning_content": concat!(
                        "The requested command is a read-only inspection.\n",
                        "{\"outcome\":\"allow\"}"
                    )
                }
            }]
        });
        assert_eq!(
            guardian_assessment_from_chat_body(&body),
            Some(json!({"outcome": "allow"}))
        );
    }

    #[test]
    fn guardian_chat_fallback_rejects_conflicting_reasoning_decisions() {
        let body = json!({
            "choices": [{
                "message": {
                    "content": "{\"outcome\":\"allow\"}",
                    "reasoning_content": concat!(
                        "{\"risk_level\":\"high\",",
                        "\"user_authorization\":\"unknown\",",
                        "\"outcome\":\"deny\",",
                        "\"rationale\":\"not authorized\"}"
                    )
                }
            }]
        });
        assert_eq!(guardian_assessment_from_chat_body(&body), None);
    }

    #[test]
    fn severity_rank_orders_critical_first() {
        assert_eq!(severity_rank(Severity::Critical), 0);
        assert_eq!(severity_rank(Severity::Warning), 1);
        assert_eq!(severity_rank(Severity::Info), 2);
    }

    #[test]
    fn parse_severity_case_insensitive() {
        assert_eq!(parse_severity("critical"), Some(Severity::Critical));
        assert_eq!(parse_severity("WARNING"), Some(Severity::Warning));
        assert_eq!(parse_severity("Info"), Some(Severity::Info));
        assert_eq!(parse_severity("bogus"), None);
    }

    #[test]
    fn parse_findings_from_direct_field() {
        let body = json!({
            "findings": [
                {"severity": "warning", "title": "未檢查 null", "location": "L42"},
                {"severity": "critical", "title": "SQL 注入", "location": "L10"}
            ]
        });
        let f = parse_findings(&body);
        assert_eq!(f.len(), 2);
    }

    #[test]
    fn parse_findings_from_content_string() {
        let body = json!({
            "choices": [{
                "message": {
                    "content": "{\"findings\":[{\"severity\":\"info\",\"title\":\"可加註解\",\"location\":\"\"}]}"
                }
            }]
        });
        let f = parse_findings(&body);
        assert_eq!(f.len(), 1);
        assert!(matches!(f[0].severity, Severity::Info));
    }

    #[test]
    fn parse_findings_empty_when_no_findings() {
        let body = json!({"findings": []});
        assert!(parse_findings(&body).is_empty());
    }

    #[test]
    fn parse_findings_skips_invalid_severity() {
        let body = json!({
            "findings": [
                {"severity": "bogus", "title": "x", "location": ""},
                {"severity": "warning", "title": "y", "location": "L1"}
            ]
        });
        let f = parse_findings(&body);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].title, "y");
    }

    #[test]
    fn parse_findings_skips_empty_title() {
        let body = json!({
            "findings": [
                {"severity": "warning", "title": "   ", "location": ""}
            ]
        });
        assert!(parse_findings(&body).is_empty());
    }

    #[test]
    fn sort_puts_critical_on_top() {
        let findings = vec![
            Finding {
                id: String::new(),
                severity: Severity::Info,
                title: "i".into(),
                location: String::new(),
            },
            Finding {
                id: String::new(),
                severity: Severity::Warning,
                title: "w".into(),
                location: String::new(),
            },
            Finding {
                id: String::new(),
                severity: Severity::Critical,
                title: "c".into(),
                location: String::new(),
            },
        ];
        let sorted = sort_and_dedupe_findings(findings);
        assert!(matches!(sorted[0].severity, Severity::Critical));
        assert!(matches!(sorted[1].severity, Severity::Warning));
        assert!(matches!(sorted[2].severity, Severity::Info));
    }

    #[test]
    fn sort_assigns_sequential_ids() {
        let findings = vec![
            Finding {
                id: String::new(),
                severity: Severity::Warning,
                title: "a".into(),
                location: String::new(),
            },
            Finding {
                id: String::new(),
                severity: Severity::Critical,
                title: "b".into(),
                location: String::new(),
            },
        ];
        let sorted = sort_and_dedupe_findings(findings);
        assert_eq!(sorted[0].id, "f1");
        assert_eq!(sorted[1].id, "f2");
    }

    #[test]
    fn dedupe_keeps_more_severe_for_same_title() {
        let findings = vec![
            Finding {
                id: String::new(),
                severity: Severity::Warning,
                title: "dup".into(),
                location: String::new(),
            },
            Finding {
                id: String::new(),
                severity: Severity::Critical,
                title: "dup".into(),
                location: String::new(),
            },
        ];
        let sorted = sort_and_dedupe_findings(findings);
        assert_eq!(sorted.len(), 1);
        assert!(matches!(sorted[0].severity, Severity::Critical));
    }

    #[test]
    fn build_prompt_has_system_and_user() {
        let p = build_review_prompt("diff --git ...");
        let msgs = p["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "diff --git ...");
        assert_eq!(p["temperature"], 0);
    }

    #[test]
    fn normalize_v1_handles_variants() {
        assert_eq!(normalize_v1("https://api.x.com"), "https://api.x.com/v1");
        assert_eq!(normalize_v1("https://api.x.com/v1"), "https://api.x.com/v1");
        assert_eq!(
            normalize_v1("https://api.x.com/v1/"),
            "https://api.x.com/v1"
        );
    }

    #[test]
    fn review_via_proxy_request_includes_the_boundary_key() {
        let key = "a1".repeat(32);
        let request =
            build_review_via_proxy_request(&reqwest::Client::new(), "vlm-glm", &json!([]), &key)
                .build()
                .expect("review request should build");
        assert_eq!(
            request
                .headers()
                .get(vellum_proxy_runtime::BOUNDARY_KEY_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(key.as_str())
        );
        assert_eq!(
            request
                .headers()
                .get("x-vellum-internal-review")
                .and_then(|value| value.to_str().ok()),
            Some("1")
        );
        assert_eq!(request.url().path(), "/v1/responses");
    }
}
