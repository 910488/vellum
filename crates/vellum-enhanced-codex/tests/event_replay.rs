//! Original-event replay: parser / state / decision only.
//! Does not claim that a model would improve after a notice.

use serde::Deserialize;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use vellum_enhanced_codex::{
    fingerprint_original_result, plan_turn_stop, AblationProfile, AdmitDecision,
    AutoContinuationBudget, ContinuationDecision, EnhancedTurnHooks, HookDecision, InputEncoding,
    MemoryTelemetry, ObservationInput, ObservationResultStatus, ToolCallLedger, ToolKind,
    TurnStopContext, REPETITION_NOTICE,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureFile {
    label: String,
    baseline_core: String,
    cases: Vec<FixtureCase>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureCase {
    id: String,
    events: Vec<Value>,
    expect: FixtureExpect,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FixtureExpect {
    #[serde(default)]
    custom_input_present: Option<bool>,
    #[serde(default)]
    false_blocks: Option<u64>,
    #[serde(default)]
    notices: Option<u64>,
    #[serde(default)]
    diagnostics: Option<u64>,
    #[serde(default)]
    continuations: Option<u64>,
    #[serde(default)]
    format_failures_then_recovery: Option<u64>,
    #[serde(default)]
    min_distinct_ops: Option<u64>,
    #[serde(default)]
    error_classes_distinct: Option<Vec<String>>,
    #[serde(default)]
    native_poll_notices: Option<u64>,
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../evals/fixtures/enhanced-core-replay/cases.json")
}

fn identity_from_event(event: &Value) -> vellum_enhanced_codex::ProviderToolCallIdentity {
    let call_id = event["callId"].as_str().unwrap();
    let tool = event["tool"].as_str().unwrap();
    let kind = match event["kind"].as_str() {
        Some("custom") => ToolKind::Custom,
        Some("tool_search") => ToolKind::ToolSearch,
        _ => ToolKind::Function,
    };
    let namespace = event["namespace"].as_str().unwrap_or("");
    if kind == ToolKind::Custom || event.get("raw").is_some() {
        ToolCallLedger::identity_raw(
            call_id,
            tool,
            kind,
            namespace,
            event["raw"].as_str().unwrap_or(""),
        )
    } else {
        ToolCallLedger::identity_json(call_id, tool, kind, namespace, &event["args"])
    }
}

#[test]
fn original_event_replay_matches_named_cases() {
    let raw = fs::read_to_string(fixture_path()).unwrap();
    let fixtures: FixtureFile = serde_json::from_str(&raw).unwrap();
    assert_eq!(fixtures.label, "案例衍生重現");
    assert!(fixtures.baseline_core.starts_with("6576e4fb"));

    let mut report = serde_json::json!({
        "label": fixtures.label,
        "baselineCore": fixtures.baseline_core,
        "scriptedProviderIsNotLiveModelImprovement": true,
        "cases": []
    });

    for case in &fixtures.cases {
        let mut hooks = EnhancedTurnHooks::new(AblationProfile::E5.features());
        let mut telemetry = MemoryTelemetry::default();
        let mut budget = AutoContinuationBudget::default();
        let mut executed = 0u64;
        let mut notices = 0u64;
        let mut diagnostics = 0u64;
        let mut native_poll_notices = 0u64;
        let mut false_blocks = 0u64;
        let mut format_failures = 0u64;
        let mut custom_input_present = false;
        let mut error_classes = Vec::new();
        let mut last_stop = ContinuationDecision::AllowStop;

        for event in &case.events {
            match event["op"].as_str() {
                Some("user_turn") => {
                    hooks.on_new_user_input();
                    budget.reset_for_user_input();
                }
                Some("admit") => {
                    let identity = identity_from_event(event);
                    if identity.input_encoding == InputEncoding::RawCustom
                        && !identity.argument_fingerprint.is_empty()
                    {
                        custom_input_present = true;
                    }
                    let decision = hooks.admit_tool_call(identity, &mut telemetry);
                    let got = match decision {
                        HookDecision::Handled(AdmitDecision::Execute) => "execute",
                        HookDecision::Handled(AdmitDecision::SuppressDuplicate { .. }) => {
                            "suppress"
                        }
                        HookDecision::Handled(AdmitDecision::FailClosed { .. }) => "collision",
                        HookDecision::DeferToUpstream => "defer",
                    };
                    let expect = event["expect"].as_str().unwrap();
                    if got != expect {
                        false_blocks += 1;
                    }
                    if got == "execute" {
                        executed += 1;
                    }
                }
                Some("complete") => {
                    let call_id = event["callId"].as_str().unwrap();
                    let _ = hooks.complete_original_tool_result(call_id);
                    let status = match event["status"].as_str() {
                        Some("failed") => {
                            format_failures += 1;
                            ObservationResultStatus::Failed
                        }
                        Some("cancelled") => ObservationResultStatus::Cancelled,
                        _ => ObservationResultStatus::Success,
                    };
                    let result = event["result"].as_str().unwrap_or("");
                    let handled = hooks.ledger.get(call_id);
                    let input = ObservationInput {
                        tool_name: handled
                            .map(|h| h.identity.tool_name.clone())
                            .unwrap_or_else(|| event["tool"].as_str().unwrap_or("").into()),
                        tool_kind: handled
                            .map(|h| h.identity.tool_kind)
                            .unwrap_or(ToolKind::Function),
                        namespace: handled
                            .map(|h| h.identity.namespace.clone())
                            .unwrap_or_default(),
                        input_fingerprint: handled
                            .map(|h| h.identity.argument_fingerprint.clone())
                            .unwrap_or_default(),
                        original_result_fingerprint: fingerprint_original_result(
                            result,
                            Some(REPETITION_NOTICE),
                        ),
                        dispatch_index: handled.map(|h| h.dispatch_index).unwrap_or(0),
                        result_status: status,
                        native_wait_poll: event["nativeWaitPoll"].as_bool().unwrap_or(false),
                    };
                    let native_wait_poll = input.native_wait_poll;
                    let observed = hooks.observe_tool_result(call_id, input, &mut telemetry);
                    if observed.emit_diagnostic {
                        diagnostics += 1;
                    }
                    if observed.emit_model_notice {
                        notices += 1;
                        if native_wait_poll {
                            native_poll_notices += 1;
                        }
                    }
                }
                Some("error") => {
                    error_classes.push(event["category"].as_str().unwrap().to_string());
                    assert_ne!(event["http"].as_u64(), None);
                }
                Some("cancel") => {
                    let plan = plan_turn_stop(
                        &mut budget,
                        &TurnStopContext {
                            cancelled: true,
                            ..TurnStopContext::default()
                        },
                    );
                    last_stop = plan.decision;
                    hooks.observation.reset();
                }
                Some("steer") => {
                    let plan = plan_turn_stop(
                        &mut budget,
                        &TurnStopContext {
                            user_steer_pending: true,
                            ..TurnStopContext::default()
                        },
                    );
                    last_stop = plan.decision;
                    hooks.on_new_user_input();
                }
                Some("resume_ended") => {
                    let snapshot = hooks.ledger.to_durable_json();
                    hooks.restore_ledger(&snapshot).unwrap();
                }
                Some("parallel_ooo") => {
                    let mut local = vellum_enhanced_codex::ObservationState::new();
                    let mk = |index, file: &str| ObservationInput {
                        tool_name: "read".into(),
                        tool_kind: ToolKind::Function,
                        namespace: String::new(),
                        input_fingerprint: file.into(),
                        original_result_fingerprint: "x".into(),
                        dispatch_index: index,
                        result_status: ObservationResultStatus::Success,
                        native_wait_poll: false,
                    };
                    local.observe(mk(0, "a"), true);
                    local.observe(mk(2, "a"), true);
                    local.observe(mk(3, "a"), true);
                    let hole = local.observe(mk(1, "b"), true);
                    assert!(!hole.emit_diagnostic);
                }
                Some("stop") => {
                    let plan = plan_turn_stop(
                        &mut budget,
                        &TurnStopContext {
                            natural_stop: true,
                            ..TurnStopContext::default()
                        },
                    );
                    last_stop = plan.decision;
                    if event["expect"].as_str() == Some("allow_stop") {
                        assert_eq!(last_stop, ContinuationDecision::AllowStop);
                    }
                }
                other => panic!("unknown op {other:?}"),
            }
        }

        assert_eq!(false_blocks, 0, "{}", case.id);
        assert_eq!(budget.used, 0);
        assert_ne!(last_stop, ContinuationDecision::Continue { index: 1 });
        let actual = FixtureExpect {
            custom_input_present: Some(custom_input_present),
            false_blocks: Some(false_blocks),
            notices: Some(notices),
            diagnostics: Some(diagnostics),
            continuations: Some(u64::from(budget.used)),
            format_failures_then_recovery: Some(format_failures),
            min_distinct_ops: Some(executed),
            error_classes_distinct: Some(error_classes.clone()),
            native_poll_notices: Some(native_poll_notices),
        };
        assert_expect(&case.id, &case.expect, &actual);

        report["cases"].as_array_mut().unwrap().push(serde_json::json!({
            "id": case.id,
            "executed": executed,
            "falseBlocks": false_blocks,
            "notices": notices,
            "diagnostics": diagnostics,
            "continuations": u64::from(budget.used),
            "customInputPresent": custom_input_present,
            "formatFailures": format_failures,
            "errorClasses": error_classes,
            "passed": true
        }));
    }

    if let Ok(scratch) = std::env::var("GROK_GOAL_SCRATCH") {
        let path = PathBuf::from(scratch).join("event-replay.json");
        fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
}

fn assert_expect(id: &str, expect: &FixtureExpect, actual: &FixtureExpect) {
    if let Some(value) = expect.custom_input_present {
        assert_eq!(actual.custom_input_present, Some(value), "{id} customInputPresent");
    }
    if let Some(value) = expect.false_blocks {
        assert_eq!(actual.false_blocks, Some(value), "{id} falseBlocks");
    }
    if let Some(value) = expect.notices {
        assert_eq!(actual.notices, Some(value), "{id} notices");
    }
    if let Some(value) = expect.diagnostics {
        assert_eq!(actual.diagnostics, Some(value), "{id} diagnostics");
    }
    if let Some(value) = expect.continuations {
        assert_eq!(actual.continuations, Some(value), "{id} continuations");
    }
    if let Some(value) = expect.format_failures_then_recovery {
        assert_eq!(
            actual.format_failures_then_recovery,
            Some(value),
            "{id} formatFailuresThenRecovery"
        );
    }
    if let Some(value) = expect.min_distinct_ops {
        assert!(
            actual.min_distinct_ops.unwrap_or(0) >= value,
            "{id} minDistinctOps: got {:?} want >= {value}",
            actual.min_distinct_ops
        );
    }
    if let Some(value) = expect.error_classes_distinct.as_ref() {
        assert_eq!(
            actual.error_classes_distinct.as_ref(),
            Some(value),
            "{id} errorClassesDistinct"
        );
    }
    if let Some(value) = expect.native_poll_notices {
        assert_eq!(actual.native_poll_notices, Some(value), "{id} nativePollNotices");
    }
}

#[test]
fn diagnostic_and_model_notice_are_counted_separately() {
    let mut hooks = EnhancedTurnHooks::new(AblationProfile::E5.features());
    hooks.features.repetition_notice = true;
    let mut telemetry = MemoryTelemetry::default();
    let mut diagnostics = 0u64;
    let mut notices = 0u64;
    for index in 0..3 {
        let observed = hooks.observe_tool_result(
            &format!("c{index}"),
            ObservationInput {
                tool_name: "apply_patch".into(),
                tool_kind: ToolKind::Function,
                namespace: String::new(),
                input_fingerprint: "in-a".into(),
                original_result_fingerprint: "ok".into(),
                dispatch_index: index,
                result_status: ObservationResultStatus::Success,
                native_wait_poll: false,
            },
            &mut telemetry,
        );
        if observed.emit_diagnostic {
            diagnostics += 1;
        }
        if observed.emit_model_notice {
            notices += 1;
        }
        if observed.emit_diagnostic && observed.emit_model_notice {
            assert_ne!(
                diagnostics, notices + notices,
                "a dual-flag decision must not increment notices twice"
            );
        }
    }
    assert_eq!(diagnostics, 1);
    assert_eq!(notices, 1);
}
