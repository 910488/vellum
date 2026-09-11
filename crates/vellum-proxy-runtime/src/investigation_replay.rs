//! Offline investigation-replay harness.
//!
//! Replays portable tool events through the shared reducer without calling a model.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(test)]
use crate::evidence_operation::EvidenceOperationKind;
use crate::investigation_reducer::{
    InvestigationLedgerState, InvestigationPolicy, MaterialProgressState,
};

/// One portable tool event in a captured trace.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReplayRequestUsage {
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayEvent {
    pub command: String,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub usage: Option<ReplayRequestUsage>,
}

/// Portable material extracted from a Codex rollout JSONL artifact.
///
/// Keeping the provider-reported usage beside the response items lets offline
/// replay exercise the same request-boundary attribution and token-efficiency
/// reducer used by live traffic.
#[derive(Debug, Clone, Default)]
pub struct CapturedRolloutReplay {
    pub items: Vec<Value>,
    pub request_usage: Vec<crate::task_efficiency::CompletedRequestUsage>,
}

/// Parse a raw Codex rollout JSONL capture without contacting the provider.
/// Zero-usage token events are local compaction/control turns, not completed
/// model requests, so they must not consume an ordinal request boundary.
pub fn parse_codex_rollout_jsonl(text: &str) -> Result<CapturedRolloutReplay, String> {
    let mut captured = CapturedRolloutReplay::default();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(line)
            .map_err(|error| format!("parse rollout line {}: {error}", index + 1))?;
        match event.get("type").and_then(Value::as_str) {
            Some("response_item") => {
                if let Some(payload) = event.get("payload") {
                    captured.items.push(payload.clone());
                }
            }
            Some("event_msg")
                if event.pointer("/payload/type").and_then(Value::as_str)
                    == Some("token_count") =>
            {
                let Some(usage) = event.pointer("/payload/info/last_token_usage") else {
                    continue;
                };
                let input_tokens = usage
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let output_tokens = usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if input_tokens == 0 && output_tokens == 0 {
                    continue;
                }
                let sequence = captured.request_usage.len() as u64 + 1;
                captured
                    .request_usage
                    .push(crate::task_efficiency::CompletedRequestUsage {
                        usage_seq: sequence,
                        request_id: None,
                        response_id: None,
                        input_tokens,
                        cached_input_tokens: usage
                            .get("cached_input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        output_tokens,
                        request_index: Some(sequence as u32),
                        source_index: None,
                    });
            }
            _ => {}
        }
    }
    if captured.items.is_empty() {
        return Err("rollout contains no response_item payloads".into());
    }
    Ok(captured)
}

/// Structured replay report for evals and unit tests.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationReplayReport {
    pub operations: usize,
    #[serde(default)]
    pub operation_details: Vec<crate::investigation_diagnostics::InvestigationOperationDiagnostic>,
    #[serde(default)]
    pub link_details: Vec<crate::investigation_diagnostics::InvestigationLinkDiagnostic>,
    #[serde(default)]
    pub transition_details:
        Vec<crate::investigation_diagnostics::InvestigationTransitionDiagnostic>,
    #[serde(default)]
    pub recovery_details: Vec<crate::investigation_diagnostics::InvestigationRecoveryDiagnostic>,
    pub ledgers_created: usize,
    pub same_investigation_links: usize,
    pub frontier_expansions: usize,
    pub coverage_expansions: usize,
    pub redundant_revisits: usize,
    pub recoveries: usize,
    pub last_recovery: Option<String>,
    pub ledger_ids: Vec<String>,
    pub skipped: usize,
    #[serde(default)]
    pub tool_results_since_progress: u32,
    #[serde(default)]
    pub max_tool_results_since_progress: u32,
    #[serde(default)]
    pub first_stall_candidate_source_index: Option<usize>,
    #[serde(default)]
    pub stall_recovery_count: u32,
    #[serde(default)]
    pub stall_candidate: bool,
    #[serde(default)]
    pub watchdog_stop: bool,
    #[serde(default)]
    pub unattributed_operations: u64,
    #[serde(default)]
    pub tool_results_since_world_change: u32,
    #[serde(default)]
    pub input_tokens_since_world_change: u64,
    #[serde(default)]
    pub cumulative_input_tokens: u64,
    #[serde(default)]
    pub tokens_to_first_world_change: Option<u64>,
    #[serde(default)]
    pub first_world_change_source_index: Option<usize>,
    #[serde(default)]
    pub first_world_change_request_index: Option<u32>,
    #[serde(default)]
    pub tokens_to_first_workspace_mutation: Option<u64>,
    #[serde(default)]
    pub first_workspace_mutation_request_index: Option<u32>,
    #[serde(default)]
    pub first_research_sprawl_candidate_source_index: Option<usize>,
    #[serde(default)]
    pub research_sprawl_candidate: bool,
    #[serde(default)]
    pub research_sprawl_recovery_count: u32,
}

/// Reduce a sequence of command/output pairs against an optional prior ledger.
pub fn replay_investigation_events(
    user_prompt: &str,
    events: &[ReplayEvent],
    progress: MaterialProgressState,
    state: InvestigationLedgerState,
    policy: &InvestigationPolicy,
) -> (InvestigationLedgerState, InvestigationReplayReport) {
    let mut items = vec![serde_json::json!({
        "type": "message",
        "role": "user",
        "content": user_prompt
    })];
    let mut request_usages = Vec::new();
    for (i, event) in events.iter().enumerate() {
        let call_idx = items.len();
        items.push(serde_json::json!({
            "type": "function_call",
            "call_id": format!("replay_{i}"),
            "name": "exec_command",
            "arguments": serde_json::to_string(&serde_json::json!({ "command": event.command })).unwrap_or_default()
        }));
        items.push(serde_json::json!({
            "type": "function_call_output",
            "call_id": format!("replay_{i}"),
            "output": &event.output,
            "exit_code": event.exit_code
        }));
        if let Some(ref u) = event.usage {
            request_usages.push(crate::task_efficiency::CompletedRequestUsage {
                usage_seq: (i + 1) as u64,
                request_id: Some(format!("replay_{i}")),
                response_id: None,
                input_tokens: u.input_tokens,
                cached_input_tokens: u.cached_input_tokens,
                output_tokens: u.output_tokens,
                request_index: Some((i + 1) as u32),
                source_index: Some(call_idx),
            });
        }
    }
    replay_investigation_items_with_usage(&items, progress, state, policy, Some(&request_usages))
}

/// Replay a Responses-style item list through the same runtime input builder
/// used by live guard and Canonical compaction.
pub fn replay_investigation_items(
    items: &[Value],
    progress: MaterialProgressState,
    prior_ledger: InvestigationLedgerState,
    policy: &InvestigationPolicy,
) -> (InvestigationLedgerState, InvestigationReplayReport) {
    replay_investigation_items_with_usage(items, progress, prior_ledger, policy, None)
}

pub fn replay_investigation_items_with_usage(
    items: &[Value],
    progress: MaterialProgressState,
    prior_ledger: InvestigationLedgerState,
    policy: &InvestigationPolicy,
    request_usage: Option<&[crate::task_efficiency::CompletedRequestUsage]>,
) -> (InvestigationLedgerState, InvestigationReplayReport) {
    let prior_state = if prior_ledger.ledgers.is_empty() {
        None
    } else {
        let mut seeded = crate::investigation::InvestigationState::from_ledger(prior_ledger);
        seeded.progress = progress;
        Some(seeded)
    };
    let input = crate::investigation_runtime::build_investigation_runtime_input_seeded_with_usage(
        items,
        None,
        prior_state.as_ref(),
        None,
        request_usage,
    );
    let stall_policy = if policy.mode.injects_recovery() {
        crate::task_stall::TaskStallPolicy::recover()
    } else {
        crate::task_stall::TaskStallPolicy::shadow()
    };
    let efficiency_policy = if policy.mode.injects_recovery() {
        crate::task_efficiency::TaskEfficiencyPolicy::recover()
    } else {
        crate::task_efficiency::TaskEfficiencyPolicy::shadow()
    };
    let reduced =
        crate::investigation_runtime::reduce_investigation_input_traced_with_stall_and_efficiency(
            &input,
            policy,
            &stall_policy,
            &efficiency_policy,
        );
    let state = reduced.state;
    let last_recovery = reduced.ledger_recovery;
    let traces = reduced.traces;
    let stall_recovery = reduced.stall_recovery;
    let mut report = InvestigationReplayReport {
        operations: input.operations.len(),
        ..Default::default()
    };
    for trace in &traces {
        report
            .operation_details
            .push(crate::investigation_reducer::operation_diagnostic(
                &trace.operation,
            ));
        report
            .link_details
            .push(crate::investigation_reducer::link_diagnostic(
                (!trace.transition.ledger_id.is_empty())
                    .then_some(trace.transition.ledger_id.as_str()),
                trace.transition.link_method,
                trace.transition.link_confidence,
            ));
        report
            .transition_details
            .push(crate::investigation_reducer::transition_diagnostic(
                &trace.transition,
                trace.transition.recovery_state,
                trace.transition.redundant_revisit_count,
            ));
    }
    report.ledgers_created = traces
        .iter()
        .filter(|trace| trace.transition.created)
        .count();
    report.same_investigation_links = traces
        .iter()
        .filter(|trace| !trace.transition.created && !trace.transition.skipped)
        .count();
    report.redundant_revisits = traces
        .iter()
        .filter(|trace| trace.transition.redundant_revisit)
        .count();
    report.coverage_expansions = traces
        .iter()
        .filter(|trace| trace.transition.coverage_expanded)
        .count();
    report.frontier_expansions = traces
        .iter()
        .filter(|trace| trace.transition.frontier_expanded)
        .count();
    report.skipped = traces
        .iter()
        .filter(|trace| trace.transition.skipped)
        .count();
    report.ledger_ids = state
        .ledgers
        .iter()
        .map(|entry| entry.ledger_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if let Some(recovery) = last_recovery {
        let known_evidence_signatures = state
            .ledgers
            .iter()
            .find(|entry| entry.ledger_id == recovery.ledger_id)
            .map(|entry| {
                entry
                    .evidence_frontier
                    .iter()
                    .map(|signature| signature.digest.clone())
                    .take(8)
                    .collect()
            })
            .unwrap_or_default();
        report.recovery_details.push(
            crate::investigation_diagnostics::InvestigationRecoveryDiagnostic {
                ledger_id: recovery.ledger_id.clone(),
                why: "redundant_revisit_without_frontier_or_relevant_progress".into(),
                known_evidence_signatures,
                missing_state: "independent evidence still absent".into(),
                recovery_count: recovery.recovery_count,
                post_recovery_repeated: recovery.level
                    == crate::investigation_reducer::RecoveryState::Escalated,
            },
        );
        report.last_recovery = Some(recovery.ledger_id);
    }
    report.recoveries = if report.last_recovery.is_some() { 1 } else { 0 };
    report.tool_results_since_progress = state.task_stall.tool_results_since_progress;
    report.max_tool_results_since_progress = traces
        .iter()
        .map(|trace| trace.tool_results_since_progress)
        .max()
        .unwrap_or(0);
    report.first_stall_candidate_source_index = traces.iter().find_map(|trace| {
        trace
            .task_stall_transition
            .stall_candidate
            .then_some(trace.operation.source_index)
    });
    report.stall_recovery_count = stall_recovery
        .as_ref()
        .map(|action| action.recovery_count)
        .unwrap_or(state.task_stall.recovery_total);
    report.stall_candidate = report.first_stall_candidate_source_index.is_some();
    report.watchdog_stop =
        crate::task_stall::eval_watchdog_should_stop(&state.task_stall, &stall_policy);
    report.unattributed_operations = state.task_stall.unattributed_operations;
    if stall_recovery.is_some() {
        report.recoveries = report.recoveries.max(1);
    }
    report.tool_results_since_world_change =
        reduced.efficiency_state.tool_results_since_world_change;
    report.input_tokens_since_world_change =
        reduced.efficiency_state.input_tokens_since_world_change;
    report.cumulative_input_tokens = reduced.efficiency_state.cumulative_input_tokens;
    report.tokens_to_first_world_change = reduced.efficiency_state.tokens_to_first_world_change;
    report.first_world_change_request_index =
        reduced.efficiency_state.first_world_change_request_index;
    report.tokens_to_first_workspace_mutation =
        reduced.efficiency_state.tokens_to_first_workspace_mutation;
    report.first_workspace_mutation_request_index = reduced
        .efficiency_state
        .first_workspace_mutation_request_index;
    report.first_world_change_source_index = traces.iter().find_map(|t| {
        t.task_efficiency_transition
            .genuine_world_change
            .then_some(t.operation.source_index)
    });
    report.first_research_sprawl_candidate_source_index = traces.iter().find_map(|t| {
        t.task_efficiency_transition
            .research_sprawl_candidate
            .then_some(t.operation.source_index)
    });
    report.research_sprawl_candidate = crate::task_efficiency::research_sprawl_candidate(
        &reduced.efficiency_state,
        &efficiency_policy,
    ) || report
        .first_research_sprawl_candidate_source_index
        .is_some();
    report.research_sprawl_recovery_count = reduced
        .efficiency_recovery
        .as_ref()
        .map(|action| action.recovery_count)
        .unwrap_or(reduced.efficiency_state.research_sprawl_recovery_total);
    if reduced.efficiency_recovery.is_some() {
        report.recoveries = report.recoveries.max(1);
    }
    (state.as_ledger(), report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(command: &str, output: &str) -> ReplayEvent {
        ReplayEvent {
            command: command.into(),
            output: output.into(),
            exit_code: Some(0),
            usage: None,
        }
    }

    fn ev_with_usage(command: &str, output: &str, input_tokens: u64) -> ReplayEvent {
        ReplayEvent {
            command: command.into(),
            output: output.into(),
            exit_code: Some(0),
            usage: Some(ReplayRequestUsage {
                input_tokens,
                cached_input_tokens: 0,
                output_tokens: 100,
            }),
        }
    }

    #[test]
    fn adversarial_corpus_aggregates_cross_tool_same_investigation() {
        let prompt = "Locate missing prior task state for CIPHERTEXT_RECOVERY_EXACT in the workspace, git history, or session store.";
        let events = vec![
            ev(
                "Get-ChildItem -Recurse -Filter CIPHERTEXT_RECOVERY_EXACT",
                "",
            ),
            ev(
                "python parse.py workspace.json CIPHERTEXT_RECOVERY_EXACT",
                "[]",
            ),
            ev("git log --all -- CIPHERTEXT_RECOVERY_EXACT", ""),
            ev("Get-Content README.md", "# readme"),
            ev(
                r"python parse.py $env:CODEX_HOME\sessions\rollout-a.jsonl",
                "prompt:CIPHERTEXT_RECOVERY_EXACT",
            ),
            ev("Select-String -Pattern CIPHERTEXT_RECOVERY_EXACT", ""),
            ev("rg CIPHERTEXT_RECOVERY_EXACT", ""),
        ];
        let (_, report) = replay_investigation_events(
            prompt,
            &events,
            MaterialProgressState::default(),
            InvestigationLedgerState::default(),
            &InvestigationPolicy::recover(),
        );
        assert!(
            report.ledgers_created <= 3,
            "same hypothesis should aggregate, created={}",
            report.ledgers_created
        );
        assert!(
            report.same_investigation_links >= 3,
            "cross-tool ops must link to existing ledgers, links={}",
            report.same_investigation_links
        );
        assert!(
            report.redundant_revisits >= 1,
            "revisiting explored channels must accumulate, got {}",
            report.redundant_revisits
        );
    }

    #[test]
    fn first_channel_exploration_does_not_recover() {
        let events = vec![
            ev("Select-String -Pattern CIPHERTEXT_RECOVERY_EXACT", ""),
            ev("git grep CIPHERTEXT_RECOVERY_EXACT", ""),
            ev(r#"python -c "print('x')" sqlite.db"#, "no rows"),
        ];
        let (_, report) = replay_investigation_events(
            "Find CIPHERTEXT_RECOVERY_EXACT",
            &events,
            MaterialProgressState::default(),
            InvestigationLedgerState::default(),
            &InvestigationPolicy::recover(),
        );
        assert_eq!(report.recoveries, 0);
        assert!(report.coverage_expansions >= 2);
    }

    #[test]
    fn same_subject_different_predicate_does_not_false_merge() {
        let events = vec![
            ev("Test-Path DECISIONS.md", "False"),
            ev("Select-String -Path DECISIONS.md -Pattern CIPHER", ""),
        ];
        let (state, _) = replay_investigation_events(
            "Does DECISIONS.md exist, and does it contain CIPHER?",
            &events,
            MaterialProgressState::default(),
            InvestigationLedgerState::default(),
            &InvestigationPolicy::recover(),
        );
        assert!(
            state.ledgers.len() >= 2,
            "Exists vs Locate must not merge, got {}",
            state.ledgers.len()
        );
    }

    #[test]
    fn wrapper_empty_output_is_stable_no_result() {
        let events = vec![
            ev(
                "Select-String -Pattern CIPHERTEXT_RECOVERY_EXACT",
                "Chunk ID: 1\nProcess exited with code 0\nFinal output:\n",
            ),
            ev(
                "Select-String -Pattern CIPHERTEXT_RECOVERY_EXACT",
                "Chunk ID: 2\r\nProcess exited with code 0\r\nFinal output:\r\n",
            ),
            ev(
                "Select-String -Pattern CIPHERTEXT_RECOVERY_EXACT",
                "Chunk ID: 3\nWall time: 0.2 seconds\nFinal output:\n",
            ),
        ];
        let (_, report) = replay_investigation_events(
            "Find CIPHERTEXT_RECOVERY_EXACT",
            &events,
            MaterialProgressState::default(),
            InvestigationLedgerState::default(),
            &InvestigationPolicy::recover(),
        );
        assert_eq!(report.ledgers_created, 1);
        assert!(report.redundant_revisits >= 2);
        assert!(
            report.frontier_expansions <= 1,
            "envelope-only wrapper changes must not mint a new evidence identity, expansions={}",
            report.frontier_expansions
        );
    }

    #[test]
    fn transcript_self_growth_is_not_progress() {
        let events = vec![
            ev(
                r"Select-String -Path $env:CODEX_HOME\sessions\rollout-a.jsonl -Pattern CIPHERTEXT_RECOVERY_EXACT",
                "prompt:CIPHERTEXT_RECOVERY_EXACT",
            ),
            ev(
                r"Select-String -Path $env:CODEX_HOME\sessions\rollout-b.jsonl -Pattern CIPHERTEXT_RECOVERY_EXACT",
                "prompt:CIPHERTEXT_RECOVERY_EXACT extra",
            ),
        ];
        let (state, report) = replay_investigation_events(
            "Find CIPHERTEXT_RECOVERY_EXACT",
            &events,
            MaterialProgressState::default(),
            InvestigationLedgerState::default(),
            &InvestigationPolicy::recover(),
        );
        assert_eq!(report.frontier_expansions, 0);
        assert!(state.ledgers.iter().all(|l| l.evidence_frontier.is_empty()
            || l.evidence_frontier.iter().all(|s| s.provenance
                == crate::evidence_operation::FactProvenance::SelfGeneratedConversation)));
    }

    #[test]
    fn replay_preserves_an_early_stall_candidate_after_later_progress() {
        let mut events = Vec::new();
        for _ in 0..6 {
            events.push(ev("Get-Content README.md", "same content"));
        }
        events.push(ev("Get-Content NEW-EVIDENCE.md", "novel content"));

        let (_, report) = replay_investigation_events(
            "Inspect the workspace and resolve the task.",
            &events,
            MaterialProgressState::default(),
            InvestigationLedgerState::default(),
            &InvestigationPolicy::recover(),
        );

        assert_eq!(report.tool_results_since_progress, 0);
        assert_eq!(report.max_tool_results_since_progress, 5);
        assert!(report.first_stall_candidate_source_index.is_some());
        assert!(report.stall_candidate);
    }

    #[test]
    fn replay_items_seed_from_explicit_prior_ledger() {
        let prompt =
            "Implement normalize_records and preserve CIPHERTEXT_RECOVERY_EXACT in DECISIONS.md.";
        let (first, _) = replay_investigation_events(
            prompt,
            &[ev(r"Get-ChildItem $env:CODEX_HOME\sessions", "")],
            MaterialProgressState::default(),
            InvestigationLedgerState::default(),
            &InvestigationPolicy::shadow(),
        );
        assert_eq!(first.ledgers.len(), 1);
        let ledger_id = first.ledgers[0].ledger_id.clone();
        let revisits = first.ledgers[0].redundant_revisits;

        let second_items = vec![
            serde_json::json!({"type": "message", "role": "user", "content": prompt}),
            serde_json::json!({
                "type": "function_call",
                "call_id": "n1",
                "name": "exec_command",
                "arguments": serde_json::to_string(&serde_json::json!({
                    "command": r"Get-ChildItem $env:CODEX_HOME\sessions"
                })).unwrap()
            }),
            serde_json::json!({
                "type": "function_call_output",
                "call_id": "n1",
                "output": "",
                "exit_code": 0
            }),
        ];
        let (second, _) = replay_investigation_items(
            &second_items,
            first.progress.clone(),
            first.clone(),
            &InvestigationPolicy::shadow(),
        );
        assert_eq!(second.ledgers.len(), 1);
        assert_eq!(second.ledgers[0].ledger_id, ledger_id);
        assert_eq!(
            second.ledgers[0].redundant_revisits,
            revisits + 1,
            "explicit prior ledger must continue, not restart"
        );
    }

    #[test]
    fn test_replay_with_request_usage_tracks_metrics() {
        let prompt = "Fix bug in authentication module";
        let events = vec![
            ev_with_usage("Get-Content src/auth.rs", "let secret = 123;", 20_000),
            ev_with_usage("Get-Content src/token.rs", "let token = 456;", 20_000),
            ev_with_usage("Set-Content src/auth.rs 'fixed'", "ok", 15_000),
        ];
        let (_state, report) = replay_investigation_events(
            prompt,
            &events,
            MaterialProgressState::default(),
            InvestigationLedgerState::default(),
            &InvestigationPolicy::shadow(),
        );

        assert_eq!(report.cumulative_input_tokens, 55_000);
        assert_eq!(report.tokens_to_first_workspace_mutation, Some(55_000));
        assert_eq!(report.first_workspace_mutation_request_index, Some(3));
    }

    #[test]
    fn raw_codex_rollout_replays_provider_usage_and_skips_control_turns() {
        let rollout = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":"fix it"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","call_id":"call_1","name":"exec_command","arguments":"{\"cmd\":\"Get-Content src/lib.rs\"}"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":12000,"cached_input_tokens":4000,"output_tokens":200}}}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"source"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0}}}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","call_id":"call_2","name":"exec_command","arguments":"{\"cmd\":\"Set-Content src/lib.rs fixed\"}"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":15000,"cached_input_tokens":5000,"output_tokens":300}}}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_2","output":"written"}}"#,
        );

        let capture = parse_codex_rollout_jsonl(rollout).expect("valid rollout");
        assert_eq!(capture.items.len(), 5);
        assert_eq!(capture.request_usage.len(), 2);
        assert_eq!(capture.request_usage[1].usage_seq, 2);

        let (_, report) = replay_investigation_items_with_usage(
            &capture.items,
            MaterialProgressState::default(),
            InvestigationLedgerState::default(),
            &InvestigationPolicy::shadow(),
            Some(&capture.request_usage),
        );
        assert_eq!(report.cumulative_input_tokens, 27_000);
        assert_eq!(report.tokens_to_first_workspace_mutation, Some(27_000));
        assert_eq!(report.first_workspace_mutation_request_index, Some(2));
    }

    #[test]
    fn trace_425k_flags_sprawl_before_first_workspace_mutation() {
        let mut events = (0..58)
            .map(|index| {
                ev_with_usage(
                    &format!("Get-Content investigation-{index}.txt"),
                    "independent notes",
                    7_000,
                )
            })
            .collect::<Vec<_>>();
        events.push(ev_with_usage(
            "Set-Content src/fix.rs 'fixed'",
            "file written",
            19_351,
        ));

        let (_, report) = replay_investigation_events(
            "Investigate the bug and implement the repair.",
            &events,
            MaterialProgressState::default(),
            InvestigationLedgerState::default(),
            &InvestigationPolicy::shadow(),
        );

        assert_eq!(report.cumulative_input_tokens, 425_351);
        assert_eq!(report.tokens_to_first_workspace_mutation, Some(425_351));
        assert_eq!(report.first_workspace_mutation_request_index, Some(59));
        let mutation_source_index = report
            .operation_details
            .iter()
            .find(|operation| operation.operation_kind == EvidenceOperationKind::Mutate)
            .map(|operation| operation.source_index)
            .expect("workspace mutation");
        assert!(
            report
                .first_research_sprawl_candidate_source_index
                .expect("sprawl candidate")
                < mutation_source_index,
            "research sprawl must be observable before the late mutation: {report:?}"
        );
    }

    #[test]
    fn benign_short_frontier_and_validation_or_mutation_heavy_controls_do_not_sprawl() {
        let benign = (0..4)
            .map(|index| {
                ev_with_usage(
                    &format!("Get-Content prerequisite-{index}.rs"),
                    "new prerequisite",
                    20_000,
                )
            })
            .collect::<Vec<_>>();
        let (_, benign_report) = replay_investigation_events(
            "Understand four prerequisites before editing.",
            &benign,
            MaterialProgressState::default(),
            InvestigationLedgerState::default(),
            &InvestigationPolicy::shadow(),
        );
        assert!(!benign_report.research_sprawl_candidate);

        let validation = (0..12)
            .map(|index| {
                ev_with_usage(
                    &format!("cargo test focused_{index}"),
                    "test result: ok. 1 passed",
                    8_000,
                )
            })
            .collect::<Vec<_>>();
        let (_, validation_report) = replay_investigation_events(
            "Validate the compatibility matrix.",
            &validation,
            MaterialProgressState::default(),
            InvestigationLedgerState::default(),
            &InvestigationPolicy::shadow(),
        );
        assert!(!validation_report.research_sprawl_candidate);

        let mutation = (0..10)
            .map(|index| {
                ev_with_usage(
                    &format!("Set-Content src/part-{index}.rs 'implemented'"),
                    "file written",
                    10_000,
                )
            })
            .collect::<Vec<_>>();
        let (_, mutation_report) = replay_investigation_events(
            "Implement all ten generated parts.",
            &mutation,
            MaterialProgressState::default(),
            InvestigationLedgerState::default(),
            &InvestigationPolicy::shadow(),
        );
        assert!(!mutation_report.research_sprawl_candidate);
    }
}
