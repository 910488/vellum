//! 前後端契約檢查。
//!
//! `src/types.ts` 與 `src-tauri/src/model.rs` 必須一一對應。
//! 這裡把每個 struct 序列化一次，確認欄位名真的是 camelCase ——
//! 少一個 `#[serde(rename_all)]`，前端就會靜默地拿到 undefined。

use vellum_lib::model::*;

fn keys(value: &serde_json::Value) -> Vec<String> {
    value
        .as_object()
        .expect("expected a JSON object")
        .keys()
        .cloned()
        .collect()
}

#[test]
fn route_serializes_as_camel_case() {
    let route = Route {
        id: "r".into(),
        name: "n".into(),
        base_url: "https://example.test".into(),
        model: "m".into(),
        wire: WireFormat::Responses,
        is_current: true,
        server_side_resume: false,
        streaming: true,
        reasoning: true,
        provider_kind: ProviderKind::Official,
        auth_kind: AuthKind::ChatGpt,
        enabled: true,
        models: vec!["m".into()],
        selected_models: None,
        context_window: Some(128_000),
        model_capabilities: Vec::new(),
        insecure_http_policy: Default::default(),
        catalog_scope: CatalogScope::All,
    };
    let json = serde_json::to_value(&route).unwrap();
    let k = keys(&json);
    assert!(k.contains(&"baseUrl".to_string()), "got {k:?}");
    assert!(k.contains(&"isCurrent".to_string()), "got {k:?}");
    assert!(k.contains(&"serverSideResume".to_string()), "got {k:?}");
    assert!(k.contains(&"catalogScope".to_string()), "got {k:?}");
    assert!(
        !k.iter().any(|key| key.contains('_')),
        "snake_case leaked: {k:?}"
    );
}

#[test]
fn catalog_scope_matches_typescript_union() {
    assert_eq!(serde_json::to_value(CatalogScope::All).unwrap(), "all");
    assert_eq!(
        serde_json::to_value(CatalogScope::FreeOnly).unwrap(),
        "freeOnly"
    );
}

#[test]
fn probe_attempt_and_error_serialize_as_camel_case() {
    let attempt = ProbeAttempt {
        stage: "typedTool".into(),
        wire: Some(WireFormat::Responses),
        outcome: "provider_quota".into(),
        status: Some(429),
        duration_ms: 1234,
        timeout: false,
        retry_after: Some(60),
        message: Some("rate limited".into()),
        model_response_kind: Some("plain_text".into()),
    };
    let json = serde_json::to_value(&attempt).unwrap();
    let k = keys(&json);
    assert!(k.contains(&"durationMs".to_string()), "got {k:?}");
    assert!(k.contains(&"retryAfter".to_string()), "got {k:?}");
    assert!(k.contains(&"modelResponseKind".to_string()), "got {k:?}");
    assert!(
        !k.iter().any(|key| key.contains('_')),
        "snake_case leaked: {k:?}"
    );

    let error = ModelProbeError {
        model: "qwen".into(),
        message: "rate limit".into(),
        stage: Some("typedTool".into()),
        outcome: Some("provider_quota".into()),
        status: Some(429),
        timeout: false,
        retry_after: Some(60),
    };
    let err_json = serde_json::to_value(&error).unwrap();
    let ek = keys(&err_json);
    assert!(ek.contains(&"retryAfter".to_string()), "got {ek:?}");
    assert!(
        !ek.iter().any(|key| key.contains('_')),
        "snake_case leaked: {ek:?}"
    );
}

#[test]
fn budget_source_matches_typescript_union() {
    // 對應 src/types.ts 的 BudgetSource
    for (variant, expected) in [
        (BudgetSource::Override, "override"),
        (BudgetSource::ModelCache, "modelCache"),
        (BudgetSource::Catalog, "catalog"),
        (BudgetSource::Fallback, "fallback"),
    ] {
        assert_eq!(serde_json::to_value(variant).unwrap(), expected);
    }
}

#[test]
fn wire_format_matches_typescript_union() {
    assert_eq!(
        serde_json::to_value(WireFormat::Responses).unwrap(),
        "responses"
    );
    assert_eq!(serde_json::to_value(WireFormat::Chat).unwrap(), "chat");
}

#[test]
fn severity_matches_typescript_union() {
    assert_eq!(
        serde_json::to_value(Severity::Critical).unwrap(),
        "critical"
    );
    assert_eq!(serde_json::to_value(Severity::Warning).unwrap(), "warning");
    assert_eq!(serde_json::to_value(Severity::Info).unwrap(), "info");
}

#[test]
fn context_budget_serializes_as_camel_case() {
    let budget = vellum_lib::budget::resolve(
        "r",
        "grok-4.5",
        vellum_lib::budget::BudgetInputs {
            model_cache: Some(500_000),
            effective_percent: Some(95),
            ..Default::default()
        },
    );
    let k = keys(&serde_json::to_value(&budget).unwrap());
    for expected in [
        "routeId",
        "contextWindow",
        "effectivePercent",
        "effectiveWindow",
        "overrideTokens",
        "compactThresholdPercent",
    ] {
        assert!(
            k.contains(&expected.to_string()),
            "missing {expected} in {k:?}"
        );
    }
}

#[test]
fn optional_fields_serialize_as_null_not_missing() {
    // 前端寫的是 `resetAt: string | null`，不是 `resetAt?: string`。
    // 欄位整個消失的話，TS 那邊的 null 檢查會過不了型別但執行期爆掉。
    let quota = QuotaSnapshot {
        route_id: "r".into(),
        used_percent: 0.0,
        period: QuotaPeriod::named(QuotaPeriodUnit::Week),
        reset_at: None,
        tier: None,
        stale: false,
    };
    let json = serde_json::to_value(&quota).unwrap();
    assert!(json.get("resetAt").is_some_and(serde_json::Value::is_null));
    assert!(json.get("tier").is_some_and(serde_json::Value::is_null));

    let proxy = ProxyStatus {
        running: false,
        base_url: "http://127.0.0.1:15721/v1".into(),
        catalog_path: None,
        codex_managed: false,
        last_error: None,
        notice: None,
        ..Default::default()
    };
    let json = serde_json::to_value(&proxy).unwrap();
    assert!(json
        .get("lastError")
        .is_some_and(serde_json::Value::is_null));
    assert!(json.get("notice").is_some_and(serde_json::Value::is_null));
}

#[test]
fn review_defaults_keep_before_compact_on() {
    // 歷史壓掉就回不來，這是最後能攔住錯誤的時間點。
    // 有人「順手」把預設關掉時要在這裡失敗。
    let defaults = ReviewSettings::default();
    assert!(defaults.before_compact, "壓縮前審查不能預設關閉");
    assert!(defaults.before_send);
    assert!(!defaults.on_edit, "每次編輯後審查太吵，預設應為關");
}
