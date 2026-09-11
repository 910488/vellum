use std::fs;

use serde::Serialize;
use serde_json::json;
use vellum_enhanced_codex::{
    hash_identifier, AblationProfile, AdmitDecision, CompactDecision, ContentBlock,
    ContinuationDecision, EnhancedRuntimeLockFile, EnhancedTurnHooks, GatewayMutationGuard,
    GatewayOperation, HookDecision, LateResultDecision, ModelVisibleSurface, OverflowDecision,
    SurfaceItem, ToolCallLedger, ToolCallResolution, ToolResultPrunePolicy, TurnStopContext,
    UnfinishedSignal, MAX_AUTO_CONTINUATIONS, MAX_CONTEXT_OVERFLOW_RETRIES,
};

use crate::enhanced_runtime::{
    ExecutionPlane, RuntimeInstall, RuntimeRegistry, RuntimeRouter, TrustedProviderSet,
};
use crate::error::{AppError, AppResult};
use crate::eval::runner::EvalPaths;

pub async fn run(args: &[String]) -> AppResult<()> {
    let paths = EvalPaths::discover(None)?;
    let lock_path = paths.project_root.join("enhanced-runtime.lock.json");
    let lock =
        EnhancedRuntimeLockFile::parse(&fs::read(&lock_path).map_err(|error| {
            AppError::Message(format!("read {}: {error}", lock_path.display()))
        })?)
        .map_err(|error| AppError::Message(error.to_string()))?;

    let gates = HardGates::unproven();

    let mut report = FixtureReport {
        lock_profile: lock.build_profile.clone(),
        runtime_digest: lock
            .runtime_digest(
                vellum_enhanced_codex::EnhancedRuntimeFeatures::all_on(),
                env!("VELLUM_BUILD_TARGET"),
            )
            .ok(),
        identity_complete: lock.identity_complete_for_target(env!("VELLUM_BUILD_TARGET")),
        promotion_ready: false,
        promotion_blockers: promotion_blockers(&lock),
        cases: Vec::new(),
        hard_gates: gates,
    };

    report
        .cases
        .push(run_tool_reliability(&mut report.hard_gates));
    report
        .cases
        .push(run_context_recovery(&mut report.hard_gates));
    report.cases.push(run_continuation(&mut report.hard_gates));
    report.cases.push(run_routing(&mut report.hard_gates));
    report
        .cases
        .push(run_e0_matches_baseline(&mut report.hard_gates));
    report
        .cases
        .push(run_legacy_isolation(&mut report.hard_gates));

    report.promotion_ready = report.promotion_blockers.is_empty()
        && report.cases.iter().all(|case| case.passed)
        && report.hard_gates.duplicate_logical_tool_execution == 0
        && report.hard_gates.synthetic_false_success == 0
        && report.hard_gates.unbounded_overflow_retry == 0
        && report.hard_gates.automatic_continuation_over_max == 0
        && report.hard_gates.legacy_vellum_harness_mutation == 0
        && !report.hard_gates.official_runtime_behavior_changed;

    let failed = report.cases.iter().filter(|case| !case.passed).count();
    let output_dir = paths.output_root.join("enhanced-mvp-fixtures");
    fs::create_dir_all(&output_dir)
        .map_err(|error| AppError::Message(format!("create {}: {error}", output_dir.display())))?;
    let report_path = output_dir.join("report.json");
    fs::write(
        &report_path,
        serde_json::to_vec_pretty(&report).map_err(|error| {
            AppError::Message(format!("encode enhanced MVP fixture report: {error}"))
        })?,
    )
    .map_err(|error| AppError::Message(format!("write {}: {error}", report_path.display())))?;

    println!(
        "Enhanced MVP fixtures: {} passed, {} failed. Report: {}",
        report.cases.len() - failed,
        failed,
        report_path.display()
    );
    println!(
        "Hard gates: officialChanged={} migration={} dupExec={} falseSuccess={} overflowRetry={} contOverMax={} legacyMutations={}",
        report.hard_gates.official_runtime_behavior_changed,
        report.hard_gates.cross_runtime_thread_migration,
        report.hard_gates.duplicate_logical_tool_execution,
        report.hard_gates.synthetic_false_success,
        report.hard_gates.unbounded_overflow_retry,
        report.hard_gates.automatic_continuation_over_max,
        report.hard_gates.legacy_vellum_harness_mutation
    );
    if report.promotion_ready {
        println!("Promotion: GO");
    } else {
        println!("Promotion: NO-GO");
        for blocker in &report.promotion_blockers {
            println!("  - {blocker}");
        }
    }
    if failed > 0
        || report.hard_gates.duplicate_logical_tool_execution > 0
        || report.hard_gates.synthetic_false_success > 0
        || report.hard_gates.unbounded_overflow_retry > 0
        || report.hard_gates.automatic_continuation_over_max > 0
    {
        return Err(AppError::Message(
            "enhanced MVP fixture hard gates failed".into(),
        ));
    }
    let _ = args;
    Ok(())
}

fn promotion_blockers(lock: &EnhancedRuntimeLockFile) -> Vec<String> {
    let mut blockers = Vec::new();
    if !lock.identity_complete_for_target(env!("VELLUM_BUILD_TARGET")) {
        blockers.push(
            "enhancedCodexCommit and artifactSha256 are not pinned; no Enhanced Codex binary identity exists"
                .into(),
        );
        blockers.push(
            "ports are not yet landed in a 910488/codex vellum/enhanced-mvp agent-loop seam".into(),
        );
    }
    blockers
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FixtureReport {
    lock_profile: String,
    runtime_digest: Option<String>,
    identity_complete: bool,
    promotion_ready: bool,
    promotion_blockers: Vec<String>,
    cases: Vec<CaseResult>,
    hard_gates: HardGates,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HardGates {
    official_runtime_behavior_changed: bool,
    cross_runtime_thread_migration: u64,
    duplicate_logical_tool_execution: u64,
    synthetic_false_success: u64,
    unbounded_overflow_retry: u64,
    automatic_continuation_over_max: u64,
    legacy_vellum_harness_mutation: u64,
}

impl HardGates {
    fn unproven() -> Self {
        Self {
            official_runtime_behavior_changed: false,
            cross_runtime_thread_migration: 0,
            duplicate_logical_tool_execution: 0,
            synthetic_false_success: 0,
            unbounded_overflow_retry: 0,
            automatic_continuation_over_max: 0,
            legacy_vellum_harness_mutation: 0,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CaseResult {
    name: String,
    passed: bool,
    detail: String,
}

fn run_tool_reliability(gates: &mut HardGates) -> CaseResult {
    let mut hooks = EnhancedTurnHooks::new(AblationProfile::E1.features());
    let mut telemetry = vellum_enhanced_codex::MemoryTelemetry::default();
    let identity = ToolCallLedger::identity("call-1", "shell", &json!({"cmd": "rm"}));
    let first = hooks.admit_tool_call(identity.clone(), &mut telemetry);
    let second = hooks.admit_tool_call(identity, &mut telemetry);
    let executes = matches!(first, HookDecision::Handled(AdmitDecision::Execute))
        && matches!(
            second,
            HookDecision::Handled(AdmitDecision::SuppressDuplicate { .. })
        );
    if !executes {
        gates.duplicate_logical_tool_execution += 1;
    }
    let synthetic = ToolCallLedger::synthetic_duplicate_result("duplicate");
    if synthetic.success() {
        gates.synthetic_false_success += 1;
    }
    let late = matches!(
        hooks.ingest_late_tool_result("call-1"),
        HookDecision::Handled(LateResultDecision::Suppress)
    );
    hooks.mark_tool_resolution("call-1", ToolCallResolution::Executed);
    CaseResult {
        name: "tool-reliability".into(),
        passed: executes && late && !synthetic.success(),
        detail: format!(
            "first={first:?} second={second:?} late_suppressed={late} threadHash={}",
            hash_identifier("thread-eval")
        ),
    }
}

fn run_context_recovery(gates: &mut HardGates) -> CaseResult {
    let mut hooks = EnhancedTurnHooks::new(AblationProfile::E2.features());
    let mut telemetry = vellum_enhanced_codex::MemoryTelemetry::default();
    let surface = ModelVisibleSurface {
        item_token_estimates: Vec::new(),
        generation: 1,
        items: vec![
            SurfaceItem::ToolCall {
                call_id: "c1".into(),
                tool_name: "read".into(),
                tool_type: "function".into(),
                arguments: "{}".into(),
            },
            SurfaceItem::ToolResult {
                call_id: "c1".into(),
                tool_name: "read".into(),
                tool_type: "function".into(),
                blocks: vec![ContentBlock {
                    kind: "text".into(),
                    text: Some("n".repeat(8_000)),
                }],
            },
        ],
    };
    hooks.prune_policy = ToolResultPrunePolicy {
        trigger_ratio: 0.01,
        min_text_bytes: 20,
        keep_head_bytes: 8,
        keep_tail_bytes: 8,
    };
    let HookDecision::Handled(plan) = hooks.plan_pressure(&surface, 100, 2_000, &mut telemetry)
    else {
        return CaseResult {
            name: "context-recovery".into(),
            passed: false,
            detail: "E2 deferred to upstream".into(),
        };
    };
    let avoided = plan.compaction_avoided && plan.compact == CompactDecision::Skip;
    let mut after = plan.prune.surface.clone();
    after.generation = surface.generation + 1;
    let HookDecision::Handled(overflow) =
        hooks.plan_overflow(&surface, &after, false, &mut telemetry)
    else {
        return CaseResult {
            name: "context-recovery".into(),
            passed: false,
            detail: "overflow deferred".into(),
        };
    };
    let retried = matches!(
        overflow.decision,
        OverflowDecision::Retry { retry_index: 1 }
    );
    let HookDecision::Handled(second) = hooks.plan_overflow(&after, &after, false, &mut telemetry)
    else {
        return CaseResult {
            name: "context-recovery".into(),
            passed: false,
            detail: "second overflow deferred".into(),
        };
    };
    if !matches!(second.decision, OverflowDecision::PreserveOriginalError) {
        gates.unbounded_overflow_retry += 1;
    }
    hooks.on_new_user_input();
    let reset = hooks.overflow_retries_used == 0;
    CaseResult {
        name: "context-recovery".into(),
        passed: avoided
            && retried
            && matches!(second.decision, OverflowDecision::PreserveOriginalError)
            && plan.prune.surface.tool_pairs_intact()
            && reset
            && MAX_CONTEXT_OVERFLOW_RETRIES == 1,
        detail: format!(
            "avoided={avoided} retried={retried} second={:?} reset={reset}",
            second.decision
        ),
    }
}

fn run_continuation(gates: &mut HardGates) -> CaseResult {
    let mut hooks = EnhancedTurnHooks::new(AblationProfile::E3.features());
    let mut telemetry = vellum_enhanced_codex::MemoryTelemetry::default();
    let unfinished = TurnStopContext {
        unfinished: vec![UnfinishedSignal::StructuredPendingWork],
        ..TurnStopContext::default()
    };
    let first = hooks.on_turn_stop(&unfinished, &mut telemetry);
    if let HookDecision::Handled(ref plan) = first {
        if let Some(reserved) = plan.reservation {
            hooks.commit_continuation(reserved).unwrap();
        }
    }
    let failed_stream = hooks.on_turn_stop(&unfinished, &mut telemetry);
    if let HookDecision::Handled(ref plan) = failed_stream {
        let _ = plan.reservation;
        hooks.release_continuation();
    }
    let second = hooks.on_turn_stop(&unfinished, &mut telemetry);
    if let HookDecision::Handled(ref plan) = second {
        if let Some(reserved) = plan.reservation {
            hooks.commit_continuation(reserved).unwrap();
        }
    }
    let third = hooks.on_turn_stop(&unfinished, &mut telemetry);
    if !matches!(
        third,
        HookDecision::Handled(ref plan) if plan.decision == ContinuationDecision::StopBudgetExhausted
    ) {
        gates.automatic_continuation_over_max += 1;
    }
    let cancel = hooks.on_turn_stop(
        &TurnStopContext {
            cancelled: true,
            unfinished: vec![UnfinishedSignal::NativeSubagentWorkRemaining],
            ..TurnStopContext::default()
        },
        &mut telemetry,
    );
    CaseResult {
        name: "bounded-continuation".into(),
        passed: matches!(
            first,
            HookDecision::Handled(ref plan) if plan.decision == ContinuationDecision::Continue { index: 1 }
        ) && matches!(
            second,
            HookDecision::Handled(ref plan) if plan.decision == ContinuationDecision::Continue { index: 2 }
        ) && matches!(
            third,
            HookDecision::Handled(ref plan) if plan.decision == ContinuationDecision::StopBudgetExhausted
        ) && matches!(
            cancel,
            HookDecision::Handled(ref plan) if plan.decision == ContinuationDecision::StopCancelled
        ) && MAX_AUTO_CONTINUATIONS == 2,
        detail: format!("third={third:?} cancel={cancel:?}"),
    }
}

fn run_routing(gates: &mut HardGates) -> CaseResult {
    let router = RuntimeRouter::new(TrustedProviderSet::new(
        vec!["openai-official".into()],
        vec!["qwen".into(), "deepseek".into(), "grok".into()],
    ));
    let mut registry = RuntimeRegistry::new();
    registry
        .register_default(RuntimeInstall {
            plane: ExecutionPlane::OfficialCodex,
            executable: std::path::PathBuf::from("C:/codex/official-a.exe"),
            digest: "digest-a".into(),
            artifact_sha256: None,
            manifest: None,
        })
        .unwrap();
    registry
        .register(RuntimeInstall {
            plane: ExecutionPlane::OfficialCodex,
            executable: std::path::PathBuf::from("C:/codex/official-b.exe"),
            digest: "digest-b".into(),
            artifact_sha256: None,
            manifest: None,
        })
        .unwrap();
    let old = registry.require_digest(ExecutionPlane::OfficialCodex, "digest-a");
    let current = registry.require(ExecutionPlane::OfficialCodex);
    if old.is_err() || current.unwrap().digest != "digest-a" {
        gates.cross_runtime_thread_migration += 1;
    }
    let official = router.plane_for_provider("openai-official").ok();
    let third = router.plane_for_provider("qwen").ok();
    let model_name = router.plane_for_provider("gpt-5.4").is_err();
    CaseResult {
        name: "dual-runtime-routing".into(),
        passed: official == Some(ExecutionPlane::OfficialCodex)
            && third == Some(ExecutionPlane::EnhancedCodex)
            && model_name
            && gates.cross_runtime_thread_migration == 0,
        detail: format!("official={official:?} third={third:?} modelNameRejected={model_name}"),
    }
}

fn run_e0_matches_baseline(gates: &mut HardGates) -> CaseResult {
    let mut hooks = EnhancedTurnHooks::new(AblationProfile::E0.features());
    let mut telemetry = vellum_enhanced_codex::MemoryTelemetry::default();
    let identity = ToolCallLedger::identity("call-1", "shell", &json!({"cmd": "echo"}));
    let first = hooks.admit_tool_call(identity.clone(), &mut telemetry);
    let second = hooks.admit_tool_call(identity, &mut telemetry);
    let deferred =
        first == HookDecision::DeferToUpstream && second == HookDecision::DeferToUpstream;
    if !deferred || !hooks.ledger.is_empty() {
        gates.official_runtime_behavior_changed = true;
    }
    CaseResult {
        name: "e0-baseline".into(),
        passed: deferred && telemetry.events.is_empty() && hooks.ledger.is_empty(),
        detail: "E0 defers every hook to unmodified Codex".into(),
    }
}

fn run_legacy_isolation(gates: &mut HardGates) -> CaseResult {
    let mut guard = GatewayMutationGuard::default();
    guard.record_allowed(GatewayOperation::AuthInjection);
    guard.record_allowed(GatewayOperation::EndpointMapping);
    guard.record_allowed(GatewayOperation::ResponsesChatTranslation);
    guard.record_allowed(GatewayOperation::SseNormalization);
    guard.record_allowed(GatewayOperation::ProviderErrorMapping);
    guard.record_allowed(GatewayOperation::UsageAccounting);
    gates.legacy_vellum_harness_mutation = guard.legacy_harness_mutation_count();
    CaseResult {
        name: "legacy-gateway-isolation".into(),
        passed: guard.assert_isolated().is_ok(),
        detail: format!(
            "legacy_harness_mutation_count={}",
            gates.legacy_vellum_harness_mutation
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e0_does_not_claim_official_behavior_changed() {
        let mut gates = HardGates::unproven();
        assert!(run_e0_matches_baseline(&mut gates).passed);
        assert!(!gates.official_runtime_behavior_changed);
    }
}
