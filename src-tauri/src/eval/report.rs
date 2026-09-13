use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::runner::{RunRecord, TaskResult};
use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct EngineTaskStats {
    attempts: u64,
    total: u64,
    passed: u64,
    compaction_failures: u64,
    duplicate_commands: u64,
    repeated_file_changes: u64,
    input_tokens: u64,
    output_tokens: u64,
    durations: Vec<u64>,
    compression_ratios: Vec<f64>,
    engine_mismatches: u64,
    mixed_engine: u64,
    evaluator_void: u64,
    provider_invalid: u64,
}

impl EngineTaskStats {
    fn add(&mut self, result: &TaskResult) {
        self.attempts += 1;
        let engine_mismatch = result.compaction_engine_matched == Some(false);
        let mixed_engine = result.observed_compaction_engine.as_deref() == Some("mixed")
            || result
                .protocol_violations
                .iter()
                .any(|violation| violation == "mixed_engine");
        self.engine_mismatches += u64::from(engine_mismatch);
        self.mixed_engine += u64::from(mixed_engine);
        if engine_mismatch
            || mixed_engine
            || result.failure_origin
                == Some(crate::eval::runner::FailureOrigin::EvaluatorInfrastructure)
        {
            self.evaluator_void += 1;
            return;
        }
        if result.failure_origin == Some(crate::eval::runner::FailureOrigin::Provider) {
            self.provider_invalid += 1;
            return;
        }
        self.total += 1;
        self.passed += u64::from(result.passed);
        self.compaction_failures += u64::from(result.protocol_violations.iter().any(|violation| {
            matches!(
                violation.as_str(),
                "missing_compaction" | "compaction_not_armed"
            )
        }));
        self.duplicate_commands += result.metrics.duplicate_commands;
        self.repeated_file_changes += result.metrics.repeated_file_changes;
        self.input_tokens += result.metrics.input_tokens;
        self.output_tokens += result.metrics.output_tokens;
        self.durations.push(result.duration_ms);
        if let (Some(pre), Some(post)) = (
            result.pre_compaction_model_visible_tokens,
            result.post_compaction_model_visible_tokens,
        ) {
            if pre > 0 {
                self.compression_ratios.push(post as f64 / pre as f64);
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EngineComparisonRow {
    model: String,
    requested_engine: String,
    attempts: u64,
    evaluable: u64,
    task_pass: String,
    compaction_failure: u64,
    duplicate_commands: u64,
    repeated_file_changes: u64,
    p50_wall_ms: u64,
    p95_wall_ms: u64,
    input_tokens: u64,
    output_tokens: u64,
    compression_ratio: Option<f64>,
    engine_mismatches: u64,
    mixed_engine: u64,
    evaluator_void: u64,
    provider_invalid: u64,
}

fn engine_comparison(results: &[TaskResult]) -> Vec<EngineComparisonRow> {
    let mut groups = BTreeMap::<(String, String), EngineTaskStats>::new();
    for result in results {
        let requested = result
            .requested_compaction_engine
            .clone()
            .or_else(|| result.requested_canonical_engine.clone())
            .unwrap_or_else(|| "unknown".into());
        groups
            .entry((result.model.clone(), requested))
            .or_default()
            .add(result);
    }
    groups
        .into_iter()
        .map(|((model, requested_engine), stats)| EngineComparisonRow {
            model,
            requested_engine,
            attempts: stats.attempts,
            evaluable: stats.total,
            task_pass: format!("{}/{}", stats.passed, stats.total),
            compaction_failure: stats.compaction_failures,
            duplicate_commands: stats.duplicate_commands,
            repeated_file_changes: stats.repeated_file_changes,
            p50_wall_ms: percentile(&stats.durations, 0.50),
            p95_wall_ms: percentile(&stats.durations, 0.95),
            input_tokens: stats.input_tokens,
            output_tokens: stats.output_tokens,
            compression_ratio: if stats.compression_ratios.is_empty() {
                None
            } else {
                Some(
                    stats.compression_ratios.iter().sum::<f64>()
                        / stats.compression_ratios.len() as f64,
                )
            },
            engine_mismatches: stats.engine_mismatches,
            mixed_engine: stats.mixed_engine,
            evaluator_void: stats.evaluator_void,
            provider_invalid: stats.provider_invalid,
        })
        .collect()
}

fn engine_comparison_html(rows: &[EngineComparisonRow]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let body = rows
        .iter()
        .map(|row| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
                 <td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
                 <td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape(&row.model),
                escape(&row.requested_engine),
                row.attempts,
                row.evaluable,
                escape(&row.task_pass),
                row.compaction_failure,
                row.duplicate_commands,
                row.repeated_file_changes,
                format_duration(row.p50_wall_ms),
                format_duration(row.p95_wall_ms),
                row.input_tokens,
                row.output_tokens,
                row.compression_ratio
                    .map(|ratio| format!("{ratio:.3}"))
                    .unwrap_or_else(|| "—".into()),
                row.engine_mismatches,
                row.mixed_engine,
                row.evaluator_void,
                row.provider_invalid,
            )
        })
        .collect::<String>();
    format!(
        "<h2>Compaction engine comparison</h2>\
         <div class=\"meta\">Primary metric is task pass. Canonical schema scores are not used for A/B ranking. \
         VOID classes (wrong_engine, mixed_engine) are excluded from model capability.</div>\
         <table><thead><tr>\
         <th>Model</th><th>Engine</th><th>Attempts</th><th>Evaluable</th>\
         <th>Task pass</th><th>Compaction failure</th>\
         <th>Duplicate commands</th><th>Repeated file changes</th>\
         <th>p50 wall</th><th>p95 wall</th><th>Input tokens</th><th>Output tokens</th>\
         <th>Compression ratio</th><th>Engine mismatch</th><th>Mixed engine</th>\
         <th>Evaluator VOID</th><th>Provider invalid</th>\
         </tr></thead><tbody>{body}</tbody></table>"
    )
}

#[derive(Default)]
struct Aggregate {
    total: u64,
    passed: u64,
    provider_failures: u64,
    evaluator_failures: u64,
    capability_total: u64,
    capability_passed: u64,
    tokens: u64,
    tool_calls: u64,
    malformed_tools: u64,
    duplicate_operations: u64,
    reasoning_leaks: u64,
    missing_terminal_sse: u64,
    compaction_cases: u64,
    compaction_passed: u64,
    switch_cases: u64,
    switch_passed: u64,
    durations: Vec<u64>,
}

impl Aggregate {
    fn add(&mut self, result: &TaskResult) {
        self.total += 1;
        self.passed += u64::from(result.passed);
        if !result.passed {
            match result.failure_origin {
                Some(crate::eval::runner::FailureOrigin::Provider) => self.provider_failures += 1,
                Some(crate::eval::runner::FailureOrigin::EvaluatorInfrastructure) => {
                    self.evaluator_failures += 1
                }
                _ => {}
            }
        }
        if result.layers.transport_protocol
            && result.layers.tool_protocol
            && result.layers.compaction_triggered
            && (result.layers.compaction_applied || result.layers.canonical_materialized)
            && result.layers.session_resumed
            && result.layers.continuity_preserved
        {
            self.capability_total += 1;
            self.capability_passed += u64::from(result.layers.task_acceptance);
        }
        self.tokens += result.metrics.input_tokens + result.metrics.output_tokens;
        self.tool_calls += result.metrics.tool_calls;
        self.malformed_tools += result.metrics.malformed_tool_calls;
        self.duplicate_operations +=
            result.metrics.duplicate_commands + result.metrics.repeated_file_changes;
        self.reasoning_leaks += result.metrics.reasoning_leaks;
        self.missing_terminal_sse += result.metrics.terminal_sse_missing;
        if let Some(recovered) = result.compaction_recovered {
            self.compaction_cases += 1;
            self.compaction_passed += u64::from(recovered);
        }
        if result.model_switch_recovered.is_some() {
            self.switch_cases += 1;
            self.switch_passed += u64::from(result.model_switch_recovered == Some(true));
        }
        self.durations.push(result.duration_ms);
    }

    fn pass_rate(&self) -> f64 {
        percentage(self.passed, self.total)
    }

    fn engine_qualified_pass_rate(&self) -> f64 {
        let denom = self
            .total
            .saturating_sub(self.provider_failures + self.evaluator_failures);
        percentage(self.passed, denom)
    }

    fn tokens_per_success(&self) -> u64 {
        self.tokens / self.passed.max(1)
    }

    fn capability_pass_rate(&self) -> f64 {
        percentage(self.capability_passed, self.capability_total)
    }

    fn valid_tool_rate(&self) -> f64 {
        percentage(
            self.tool_calls.saturating_sub(self.malformed_tools),
            self.tool_calls,
        )
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BaselineComparison {
    pub baseline_run_id: String,
    pub pass_rate_delta_points: f64,
    pub token_delta_percent: f64,
    pub p95_duration_delta_percent: f64,
    pub newly_regressed_tasks: Vec<String>,
    pub pass_rate_gate: bool,
    pub stable_regression_gate: bool,
    pub performance_warning: bool,
}

pub fn write_html_report(run_root: &Path) -> AppResult<PathBuf> {
    write_html_report_with_baseline(run_root, None)
}

pub fn write_html_report_with_baseline(
    run_root: &Path,
    baseline_root: Option<&Path>,
) -> AppResult<PathBuf> {
    let run: RunRecord = read_json(&run_root.join("run.json"))?;
    let results = read_results(&run_root.join("results.jsonl"))?;
    let comparison = baseline_root
        .map(|root| compare_runs(&results, root))
        .transpose()?;
    let engine_rows = engine_comparison(&results);
    let comparison_json_path = run_root.join("compaction-engine-comparison.json");
    std::fs::write(
        &comparison_json_path,
        serde_json::to_vec_pretty(&engine_rows).map_err(|error| {
            AppError::Message(format!("cannot serialize engine comparison: {error}"))
        })?,
    )
    .map_err(|error| {
        AppError::Message(format!(
            "cannot write engine comparison {}: {error}",
            comparison_json_path.display()
        ))
    })?;

    let mut by_model = BTreeMap::<String, Aggregate>::new();
    let mut by_provider = BTreeMap::<String, Aggregate>::new();
    let mut by_category = BTreeMap::<String, Aggregate>::new();
    let mut by_transition = BTreeMap::<String, Aggregate>::new();
    let mut by_failure = BTreeMap::<String, Aggregate>::new();
    let mut by_task = BTreeMap::<String, Aggregate>::new();
    let mut total = Aggregate::default();
    for result in &results {
        by_model
            .entry(result.model.clone())
            .or_default()
            .add(result);
        by_provider
            .entry(result.provider.clone())
            .or_default()
            .add(result);
        by_category
            .entry(result.category.clone())
            .or_default()
            .add(result);
        by_transition
            .entry(result.transition.clone())
            .or_default()
            .add(result);
        by_task
            .entry(result.task_id.clone())
            .or_default()
            .add(result);
        if let Some(class) = &result.failure_class {
            by_failure.entry(class.clone()).or_default().add(result);
        }
        total.add(result);
    }

    let result_rows = results
        .iter()
        .map(|result| {
            let case_id = escape(&result.case_id);
            let expected_faults = result
                .http_faults
                .iter()
                .filter(|fault| fault.expected_injection)
                .count();
            let recovered_faults = result
                .http_faults
                .iter()
                .filter(|fault| fault.recovered)
                .count();
            let unrecovered_faults = result
                .http_faults
                .iter()
                .filter(|fault| fault.expected_injection && !fault.recovered)
                .count();
            let unexpected_faults = result
                .http_faults
                .iter()
                .filter(|fault| !fault.expected_injection)
                .count();
            let fault_summary = format!(
                "注入 {expected_faults} · 已恢復 {recovered_faults} · 未恢復 {unrecovered_faults} · 未預期 {unexpected_faults}"
            );
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
                 <td class=\"{}\">{}</td><td>{}</td><td>{}</td><td>{}</td>\
                 <td>{}</td><td>{}</td><td>{}/{}</td><td><a href=\"traces/{case_id}.gateway.jsonl\">gateway</a> · \
                 <a href=\"traces/{case_id}.phase-0.jsonl\">agent</a> · \
                 <a href=\"patches/{case_id}.patch\">patch</a> · \
                 <a href=\"test-output/{case_id}.txt\">tests</a></td></tr>",
                escape(&result.task_id),
                escape(&result.provider),
                escape(&result.model),
                escape(&result.category),
                escape(&result.transition),
                if result.passed { "pass" } else { "fail" },
                if result.passed { "PASS" } else { "FAIL" },
                escape(result.failure_class.as_deref().unwrap_or("—")),
                escape(&result.protocol_violations.join(", ")),
                fault_summary,
                format_duration(result.duration_ms),
                result.metrics.input_tokens + result.metrics.output_tokens,
                result.acceptance_passed,
                result.acceptance_total,
            )
        })
        .collect::<String>();

    let comparison_html = comparison
        .as_ref()
        .map(|comparison| {
            format!(
                "<h2>Baseline 比較</h2><div class=\"meta\">\
                 Baseline {} · 通過率 {:+.1} 個百分點 · Token {:+.1}% · P95 耗時 {:+.1}% · \
                 新增穩定退步 {} · 通過率門檻 {} · 穩定退步門檻 {}\
                 </div>",
                escape(&comparison.baseline_run_id),
                comparison.pass_rate_delta_points,
                comparison.token_delta_percent,
                comparison.p95_duration_delta_percent,
                comparison.newly_regressed_tasks.len(),
                gate_text(comparison.pass_rate_gate),
                gate_text(comparison.stable_regression_gate),
            )
        })
        .unwrap_or_default();
    let layer_rows = results
        .iter()
        .map(|result| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
                 <td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
                 <td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape(&result.task_id),
                escape(&result.model),
                gate_text(result.layers.task_acceptance),
                gate_text(result.layers.transport_protocol),
                gate_text(result.layers.tool_protocol),
                gate_text(result.layers.compaction_triggered),
                gate_text(result.layers.canonical_materialized),
                gate_text(result.layers.compaction_applied),
                gate_text(result.layers.session_resumed),
                gate_text(result.layers.continuity_preserved),
                gate_text(result.layers.resource_budget),
                gate_text(result.task_correctness_passed),
                gate_text(result.performance_qualified),
                gate_text(result.protocol_qualified),
                gate_text(result.layers.runtime_attribution),
                gate_text(result.layers.mechanism_exercised),
                escape(
                    result
                        .requested_compaction_engine
                        .as_deref()
                        .or(result.requested_canonical_engine.as_deref())
                        .unwrap_or("—"),
                ),
                escape(result.observed_compaction_engine.as_deref().unwrap_or("—")),
                gate_text(result.compaction_engine_matched.unwrap_or(false)),
            )
        })
        .collect::<String>();
    let diagnostic_rows = results
        .iter()
        .map(|result| {
            let attempt = result.compaction_attempts.last();
            let hash = attempt
                .map(|attempt| attempt.source_hash.as_str())
                .or(result.source_hash.as_deref())
                .or(result.canonical_hash.as_deref())
                .map(|value| value.chars().take(12).collect::<String>())
                .unwrap_or_else(|| "-".into());
            let phase_context = if result.phase_observations.is_empty() {
                "-".into()
            } else {
                result
                    .phase_observations
                    .iter()
                    .map(|phase| {
                        format!(
                            "p{}: {}/{} ({:?})",
                            phase.phase,
                            phase.estimated_context_tokens,
                            phase.auto_compact_token_limit,
                            phase.trigger_decision
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" · ")
            };
            let phase_execution = result
                .phase_execution_observations
                .iter()
                .map(|phase| {
                    format!(
                        "a{}p{} {} exit={:?} timeout={} {}ms tokens={}/{} compact+{}",
                        phase.attempt,
                        phase.phase,
                        phase.model,
                        phase.exit_code,
                        phase.timed_out,
                        phase.duration_ms,
                        phase.input_token_delta,
                        phase.output_token_delta,
                        phase.compaction_delta,
                    )
                })
                .collect::<Vec<_>>()
                .join(" · ");
            let phase_thresholds = format!("{phase_context} | {phase_execution}");
            let generation = attempt
                .map(|attempt| attempt.generation.to_string())
                .or_else(|| result.generation.map(|generation| generation.to_string()))
                .unwrap_or_else(|| "-".into());
            let tokens = attempt
                .map(|attempt| {
                    format!(
                        "{} → {} tokens / {} → {} items / {} ms / {} retreats",
                        attempt.tokens_before,
                        attempt.tokens_after,
                        attempt.items_before,
                        attempt.items_after,
                        attempt.elapsed_ms,
                        attempt.context_retreats,
                    )
                })
                .unwrap_or_else(|| "-".into());
            let quality = attempt
                .map(|attempt| attempt.engine_id.clone())
                .unwrap_or_else(|| "-".into());
            let fallback = attempt
                .and_then(|attempt| attempt.engine_provenance.clone())
                .or_else(|| result.fallback_reason.clone())
                .unwrap_or_else(|| "-".into());
            let failure = format!(
                "{:?} / {}",
                result.failure_origin,
                result.failure_class.as_deref().unwrap_or("-")
            );
            let semantic_validation = if result.semantic_validation_diagnostics.is_empty() {
                "-".into()
            } else {
                result
                    .semantic_validation_diagnostics
                    .iter()
                    .map(|diagnostic| {
                        let fields = diagnostic
                            .errors
                            .iter()
                            .map(|error| error.field.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!(
                            "gen {}: invalid refs {}, rejected {}, fields [{}]",
                            diagnostic.candidate_generation,
                            diagnostic.invalid_evidence_refs,
                            diagnostic.rejected_claim_count,
                            fields
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" · ")
            };
            let task_stall = result
                .task_stall_diagnostics
                .last()
                .map(|diagnostic| {
                    let injections = result
                        .task_stall_recoveries
                        .iter()
                        .map(|recovery| {
                            recovery
                                .request_index
                                .map(|index| format!("#{} {:?}", index, recovery.recovery_level))
                                .unwrap_or_else(|| format!("{:?}", recovery.recovery_level))
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        "since={} recovery={} post={} candidate={} terminal={} injections=[{}]",
                        diagnostic.tool_results_since_progress,
                        diagnostic.recovery_injected_count,
                        diagnostic.post_recovery_no_progress,
                        diagnostic.watchdog_candidate,
                        !result.task_stall_terminals.is_empty(),
                        injections
                    )
                })
                .unwrap_or_else(|| "-".into());
            let overlay_request_indexes = result
                .task_efficiency_diagnostics
                .iter()
                .filter(|diagnostic| {
                    matches!(
                        diagnostic.model_facing_recovery.as_deref(),
                        Some("researchSprawlL1" | "researchSprawlL2")
                    )
                })
                .filter_map(|diagnostic| diagnostic.request_index)
                .collect::<Vec<_>>();
            let recovery_activated_at = result
                .task_efficiency_diagnostics
                .iter()
                .find_map(|diagnostic| {
                    diagnostic
                        .active_action_recovery
                        .as_ref()
                        .and_then(|active| active.started_request_index)
                });
            let level_escalated_at = result
                .task_efficiency_diagnostics
                .iter()
                .find(|diagnostic| {
                    diagnostic.model_facing_recovery.as_deref() == Some("researchSprawlL2")
                })
                .and_then(|diagnostic| diagnostic.request_index);
            let compactions_while_active = result
                .task_efficiency_diagnostics
                .iter()
                .filter_map(|diagnostic| {
                    diagnostic
                        .active_action_recovery
                        .as_ref()
                        .map(|active| active.compactions_since_activation)
                })
                .max()
                .unwrap_or(0);
            let task_efficiency_detail = result
                .task_efficiency_diagnostics
                .last()
                .map(|diagnostic| {
                    let uncached = diagnostic
                        .cumulative_input_tokens
                        .saturating_sub(diagnostic.cumulative_cached_input_tokens);
                    let recoveries = result
                        .task_efficiency_recoveries
                        .iter()
                        .map(|recovery| {
                            recovery
                                .request_index
                                .map(|index| format!("#{index}"))
                                .unwrap_or_else(|| "#?".into())
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    let conversion_info = if !diagnostic.recovery_conversions.is_empty() {
                        let lines = diagnostic.recovery_conversions
                            .iter()
                            .map(|rec| {
                                let rec_req_str = rec.recovery_request_index
                                    .map(|idx| format!("#{idx}"))
                                    .unwrap_or_else(|| "#?".into());
                                let next_mut = match (
                                    rec.requests_to_workspace_mutation,
                                    rec.tokens_to_workspace_mutation,
                                ) {
                                    (Some(reqs), Some(tokens)) => {
                                        let target_req = rec.workspace_mutation_request_index
                                            .or_else(|| rec.recovery_request_index.map(|r| r + reqs))
                                            .map(|idx| format!("#{idx}"))
                                            .unwrap_or_else(|| "#?".into());
                                        format!("{target_req} (+{reqs} req / +{:.1}k input)", tokens as f64 / 1000.0)
                                    }
                                    (Some(reqs), None) => {
                                        let target_req = rec.workspace_mutation_request_index
                                            .or_else(|| rec.recovery_request_index.map(|r| r + reqs))
                                            .map(|idx| format!("#{idx}"))
                                            .unwrap_or_else(|| "#?".into());
                                        format!("{target_req} (+{reqs} req)")
                                    }
                                    _ => "-".into(),
                                };
                                format!(
                                    "rec_{}={rec_req_str} nextMutation={next_mut} postRecoveryOps=explore:{} validate:{} mutate:{}",
                                    rec.recovery_count,
                                    rec.exploration_ops,
                                    rec.validation_ops,
                                    rec.mutation_ops,
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("; ");
                        format!(" conversions=[{lines}]")
                    } else if let Some(rec_req) = diagnostic.last_recovery_request_index {
                        let next_mut = match (
                            diagnostic.requests_to_next_workspace_mutation,
                            diagnostic.tokens_to_next_workspace_mutation,
                        ) {
                            (Some(reqs), Some(tokens)) => {
                                format!("#{} (+{} req / +{:.1}k input)", rec_req + reqs, reqs, tokens as f64 / 1000.0)
                            }
                            (Some(reqs), None) => format!("#{} (+{} req)", rec_req + reqs, reqs),
                            _ => "-".into(),
                        };
                        format!(
                            " recovery=#{rec_req} nextMutation={next_mut} postRecoveryOps=explore:{} validate:{} mutate:{}",
                            diagnostic.exploration_ops_after_recovery,
                            diagnostic.validation_ops_after_recovery,
                            diagnostic.mutation_ops_after_recovery,
                        )
                    } else {
                        String::new()
                    };
                    let active_info = if recovery_activated_at.is_some()
                        || !overlay_request_indexes.is_empty()
                    {
                        let activated_at = recovery_activated_at
                            .map(|idx| format!("#{idx}"))
                            .unwrap_or_else(|| "#?".into());
                        let overlay_requests = overlay_request_indexes
                            .iter()
                            .map(|idx| format!("#{idx}"))
                            .collect::<Vec<_>>()
                            .join(",");
                        let escalated_at = level_escalated_at
                            .map(|idx| format!("#{idx}"))
                            .unwrap_or_else(|| "-".into());
                        let lifecycle = diagnostic
                            .active_action_recovery
                            .as_ref()
                            .map(|active| format!("{:?}", active.level))
                            .unwrap_or_else(|| "closed".into());
                        format!(
                            " activeRecovery={lifecycle} recoveryActivatedAt={activated_at} overlaysRendered={} overlayRequests=[{overlay_requests}] levelEscalatedAt={escalated_at} compactsWhileActive={compactions_while_active}",
                            overlay_request_indexes.len(),
                        )
                    } else {
                        String::new()
                    };
                    format!(
                        "gross={} cached={} uncached={} output={} tokens_since_wc={} ops_since_wc={} max_tokens_since_wc={} max_ops_since_wc={} first_wc_tokens={} first_mutation_tokens={} sprawl={} recoveries=[{}]{}{}",
                        diagnostic.cumulative_input_tokens,
                        diagnostic.cumulative_cached_input_tokens,
                        uncached,
                        diagnostic.cumulative_output_tokens,
                        diagnostic.input_tokens_since_world_change,
                        diagnostic.tool_results_since_world_change,
                        diagnostic.max_input_tokens_since_world_change,
                        diagnostic.max_tool_results_since_world_change,
                        diagnostic.tokens_to_first_world_change.map(|value| value.to_string()).unwrap_or_else(|| "-".into()),
                        diagnostic.tokens_to_first_workspace_mutation.map(|value| value.to_string()).unwrap_or_else(|| "-".into()),
                        diagnostic.research_sprawl_candidate,
                        recoveries,
                        conversion_info,
                        active_info,
                    )
                })
                .unwrap_or_else(|| "-".into());
            let coverage = &result.task_efficiency_usage_coverage;
            let task_efficiency = format!(
                "{} usageCoverage=settled:{} zero:{} available:{} reason:{}",
                task_efficiency_detail,
                coverage.settled_request_count,
                coverage.zero_usage_request_count,
                coverage.token_accounting_available,
                coverage.unavailable_reason.as_deref().unwrap_or("-")
            );
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
                 <td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape(&result.task_id),
                escape(&phase_thresholds),
                escape(&hash),
                escape(&generation),
                escape(&tokens),
                escape(&quality),
                escape(&fallback),
                escape(&failure),
                escape(&semantic_validation),
                escape(&task_stall),
                escape(&task_efficiency),
                escape(&result.supporting_evidence.join(" · ")),
            )
        })
        .collect::<String>();
    let comparison_html = format!(
        "{comparison_html}{}<h2>Layer gates</h2><table><thead><tr>\
         <th>Task</th><th>Model</th><th>Acceptance</th><th>Transport</th>\
         <th>Tools</th><th>Compact triggered</th><th>Canonical materialized</th>\
         <th>Compaction applied</th>\
         <th>Session resumed</th><th>Continuity</th><th>Budget</th>\
         <th>Task correctness</th><th>Performance</th><th>Protocol</th>\
         <th>Runtime attributed</th><th>Mechanism exercised</th>\
         <th>Requested engine</th><th>Observed engine</th><th>Engine matched</th>\
         </tr></thead><tbody>{layer_rows}</tbody></table>\
         <h2>Case diagnostics</h2><table><thead><tr>\
         <th>Task</th><th>Phase context / threshold</th><th>Source hash</th>\
         <th>Candidate generation</th><th>Visible → replacement / durable tokens</th>\
         <th>Quality</th><th>Fallback</th><th>Failure origin / class</th>\
         <th>Semantic validation</th><th>Task stall</th><th>Efficiency</th><th>Evidence</th>\
         </tr></thead><tbody>{diagnostic_rows}</tbody></table>",
        engine_comparison_html(&engine_rows)
    );

    let html = format!(
        r#"<!doctype html>
<html lang="zh-Hant">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Vellum Harness Eval · {run_id}</title>
<style>
:root {{ color-scheme: dark; font-family: Inter, "Segoe UI", sans-serif; }}
body {{ margin: 0; padding: 32px; background: #111318; color: #ececf1; }}
main {{ max-width: 1440px; margin: auto; }}
.meta, .cards, table {{ background: #1a1d24; border: 1px solid #30343f; border-radius: 14px; }}
.meta {{ padding: 16px 20px; color: #b7bdca; margin-bottom: 20px; }}
.cards {{ display: grid; grid-template-columns: repeat(9, minmax(120px, 1fr)); margin: 20px 0; }}
.card {{ padding: 18px; border-right: 1px solid #30343f; }}
.card:last-child {{ border-right: 0; }}
.value {{ display: block; font-size: 26px; color: white; margin-bottom: 6px; }}
table {{ width: 100%; border-collapse: collapse; overflow: hidden; margin-bottom: 28px; }}
th, td {{ padding: 11px 13px; text-align: left; border-bottom: 1px solid #30343f; }}
th {{ color: #aab1c0; font-size: 13px; }}
a {{ color: #8ab4ff; text-decoration: none; }}
.pass {{ color: #55d68b; }} .fail {{ color: #ff7b72; }}
@media (max-width: 900px) {{ .cards {{ grid-template-columns: 1fr 1fr; }} }}
</style>
</head>
<body><main>
<h1>Vellum 第三方模型 Harness 評測</h1>
<div class="meta">Run {run_id} · Suite {suite} {suite_version} · Vellum {vellum} · Codex {codex}</div>
<div class="cards">
  <div class="card"><span class="value">{passed}/{total}</span>通過任務</div>
  <div class="card"><span class="value">{pass_rate:.1}%</span>Raw 通過率</div>
  <div class="card"><span class="value">{qualified_pass_rate:.1}%</span>Engine-qualified</div>
  <div class="card"><span class="value">{provider_invalid}</span>Provider invalid</div>
  <div class="card"><span class="value">{evaluator_invalid}</span>Evaluator invalid</div>
  <div class="card"><span class="value">{tokens}</span>總 Token</div>
  <div class="card"><span class="value">{p50}</span>中位耗時</div>
  <div class="card"><span class="value">{p95}</span>P95 耗時</div>
  <div class="card"><span class="value">{compact_passed}/{compact_total}</span>壓縮恢復</div>
  <div class="card"><span class="value">{switch_passed}/{switch_total}</span>切換恢復</div>
</div>
{comparison_html}
<h2>模型統計</h2>{model_table}
<h2>Provider 統計</h2>{provider_table}
<h2>任務類別</h2>{category_table}
<h2>模型切換方向</h2>{transition_table}
<h2>失敗分類</h2>{failure_table}
<h2>三次執行穩定度</h2>{task_table}
<h2>案例明細</h2>
<table><thead><tr><th>任務</th><th>Provider</th><th>模型</th><th>類別</th><th>切換方向</th><th>結果</th>\
<th>失敗分類</th><th>協定違規</th><th>HTTP 故障</th><th>耗時</th><th>Token</th><th>通過驗證</th><th>檔案</th></tr></thead>\
<tbody>{result_rows}</tbody></table>
</main></body></html>"#,
        run_id = escape(&run.run_id),
        suite = escape(&run.suite),
        suite_version = escape(&run.suite_version),
        vellum = escape(&run.vellum_version),
        codex = escape(&run.codex_version),
        passed = total.passed,
        total = total.total,
        pass_rate = total.pass_rate(),
        qualified_pass_rate = total.engine_qualified_pass_rate(),
        provider_invalid = total.provider_failures,
        evaluator_invalid = total.evaluator_failures,
        tokens = total.tokens,
        p50 = format_duration(percentile(&total.durations, 0.50)),
        p95 = format_duration(percentile(&total.durations, 0.95)),
        compact_passed = total.compaction_passed,
        compact_total = total.compaction_cases,
        switch_passed = total.switch_passed,
        switch_total = total.switch_cases,
        model_table = aggregate_table("模型", &by_model),
        provider_table = aggregate_table("Provider", &by_provider),
        category_table = aggregate_table("類別", &by_category),
        transition_table = aggregate_table("切換方向", &by_transition),
        failure_table = aggregate_table("失敗分類", &by_failure),
        task_table = aggregate_table("任務", &by_task),
    );
    let path = run_root.join("report.html");
    std::fs::write(&path, html).map_err(|error| {
        AppError::Message(format!(
            "cannot write eval report {}: {error}",
            path.display()
        ))
    })?;
    Ok(path)
}

fn aggregate_table(label: &str, values: &BTreeMap<String, Aggregate>) -> String {
    let rows = values
        .iter()
        .map(|(name, aggregate)| aggregate_row(name, aggregate))
        .collect::<String>();
    format!(
        "<table><thead><tr><th>{}</th><th>通過</th><th>協定通過率</th><th>模型能力通過率</th><th>Token／成功</th>\
         <th>中位耗時</th><th>有效工具呼叫</th><th>重複操作</th><th>Reasoning 洩漏</th>\
         <th>缺少終止 SSE</th></tr></thead><tbody>{rows}</tbody></table>",
        escape(label)
    )
}

fn aggregate_row(name: &str, aggregate: &Aggregate) -> String {
    format!(
        "<tr><td>{}</td><td>{}/{}</td><td>{:.1}%</td><td>{:.1}%</td><td>{}</td><td>{}</td>\
         <td>{:.1}%</td><td>{}</td><td>{}</td><td>{}</td></tr>",
        escape(name),
        aggregate.passed,
        aggregate.total,
        aggregate.pass_rate(),
        aggregate.capability_pass_rate(),
        aggregate.tokens_per_success(),
        format_duration(percentile(&aggregate.durations, 0.50)),
        aggregate.valid_tool_rate(),
        aggregate.duplicate_operations,
        aggregate.reasoning_leaks,
        aggregate.missing_terminal_sse,
    )
}

fn compare_runs(results: &[TaskResult], baseline_root: &Path) -> AppResult<BaselineComparison> {
    let baseline_run: RunRecord = read_json(&baseline_root.join("run.json"))?;
    if baseline_run.status != "completed" {
        return Err(AppError::Message(format!(
            "baseline run {} is not completed (status={}); resume or rerun it before comparison",
            baseline_run.run_id, baseline_run.status
        )));
    }
    let baseline = read_results(&baseline_root.join("results.jsonl"))?;
    let current_case_ids = results
        .iter()
        .map(|result| result.case_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let baseline_case_ids = baseline
        .iter()
        .map(|result| result.case_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if current_case_ids != baseline_case_ids {
        return Err(AppError::Message(format!(
            "baseline run {} is not comparable: current has {} cases and baseline has {} matching IDs",
            baseline_run.run_id,
            current_case_ids.len(),
            baseline_case_ids.len()
        )));
    }
    let current_aggregate = aggregate(results);
    let baseline_aggregate = aggregate(&baseline);
    let current_tasks = task_pass_states(results);
    let baseline_tasks = task_pass_states(&baseline);
    let newly_regressed_tasks = baseline_tasks
        .iter()
        .filter(|(_, passed)| **passed)
        .filter(|(task, _)| current_tasks.get(*task).is_some_and(|passed| !passed))
        .map(|(task, _)| task.clone())
        .collect::<Vec<_>>();
    let pass_rate_delta_points = current_aggregate.pass_rate() - baseline_aggregate.pass_rate();
    let token_delta_percent = relative_delta(current_aggregate.tokens, baseline_aggregate.tokens);
    let p95_duration_delta_percent = relative_delta(
        percentile(&current_aggregate.durations, 0.95),
        percentile(&baseline_aggregate.durations, 0.95),
    );
    Ok(BaselineComparison {
        baseline_run_id: baseline_run.run_id,
        pass_rate_delta_points,
        token_delta_percent,
        p95_duration_delta_percent,
        pass_rate_gate: pass_rate_delta_points >= -5.0,
        stable_regression_gate: newly_regressed_tasks.len() <= 2,
        performance_warning: token_delta_percent > 30.0 || p95_duration_delta_percent > 30.0,
        newly_regressed_tasks,
    })
}

pub fn compare_run_roots(
    current_root: &Path,
    baseline_root: &Path,
) -> AppResult<BaselineComparison> {
    let results = read_results(&current_root.join("results.jsonl"))?;
    compare_runs(&results, baseline_root)
}

fn aggregate(results: &[TaskResult]) -> Aggregate {
    let mut aggregate = Aggregate::default();
    for result in results {
        aggregate.add(result);
    }
    aggregate
}

fn task_pass_states(results: &[TaskResult]) -> HashMap<String, bool> {
    let mut states = HashMap::new();
    for result in results {
        states
            .entry(format!("{}::{}", result.task_id, result.model))
            .and_modify(|passed| *passed &= result.passed)
            .or_insert(result.passed);
    }
    states
}

fn relative_delta(current: u64, baseline: u64) -> f64 {
    if baseline == 0 {
        if current == 0 {
            0.0
        } else {
            100.0
        }
    } else {
        (current as f64 - baseline as f64) * 100.0 / baseline as f64
    }
}

fn gate_text(passed: bool) -> &'static str {
    if passed {
        "PASS"
    } else {
        "FAIL"
    }
}

fn percentage(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        100.0
    } else {
        numerator as f64 * 100.0 / denominator as f64
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> AppResult<T> {
    let bytes = std::fs::read(path).map_err(|error| {
        AppError::Message(format!("cannot read eval data {}: {error}", path.display()))
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        AppError::Message(format!("invalid eval data {}: {error}", path.display()))
    })
}

pub fn read_results(path: &Path) -> AppResult<Vec<TaskResult>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(AppError::Message(format!(
                "cannot read eval results {}: {error}",
                path.display()
            )))
        }
    };
    let parsed = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .map_err(|error| AppError::Message(format!("invalid eval result: {error}")))
        })
        .collect::<AppResult<Vec<TaskResult>>>()?;
    let mut latest = Vec::<TaskResult>::new();
    let mut indexes = HashMap::<String, usize>::new();
    for result in parsed {
        if let Some(index) = indexes.get(&result.case_id).copied() {
            latest[index] = result;
        } else {
            indexes.insert(result.case_id.clone(), latest.len());
            latest.push(result);
        }
    }
    Ok(latest)
}

fn percentile(values: &[u64], percentile: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut values = values.to_vec();
    values.sort_unstable();
    let index = ((values.len() - 1) as f64 * percentile).ceil() as usize;
    values[index.min(values.len() - 1)]
}

fn format_duration(milliseconds: u64) -> String {
    if milliseconds >= 60_000 {
        format!(
            "{}m {:.1}s",
            milliseconds / 60_000,
            (milliseconds % 60_000) as f64 / 1000.0
        )
    } else {
        format!("{:.1}s", milliseconds as f64 / 1000.0)
    }
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub fn print_json_summary(run_root: &Path) -> AppResult<()> {
    print_json_summary_with_baseline(run_root, None)
}

pub fn print_json_summary_with_baseline(
    run_root: &Path,
    baseline_root: Option<&Path>,
) -> AppResult<()> {
    let run: RunRecord = read_json(&run_root.join("run.json"))?;
    let results = read_results(&run_root.join("results.jsonl"))?;
    let total = aggregate(&results);
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Summary<'a> {
        run_id: &'a str,
        suite: &'a str,
        passed: u64,
        total: u64,
        pass_rate: f64,
        raw_pass_rate: f64,
        engine_qualified_pass_rate: f64,
        provider_invalid_count: u64,
        evaluator_invalid_count: u64,
        capability_pass_rate: f64,
        total_tokens: u64,
        median_duration_ms: u64,
        p95_duration_ms: u64,
        protocol_violations: u64,
        eval_provider_display_name: &'a str,
        compaction_engine: &'a str,
        engine_comparison: Vec<EngineComparisonRow>,
        baseline: Option<BaselineComparison>,
    }
    let engine_rows = engine_comparison(&results);
    let summary = Summary {
        run_id: &run.run_id,
        suite: &run.suite,
        passed: total.passed,
        total: total.total,
        pass_rate: total.pass_rate(),
        raw_pass_rate: total.pass_rate(),
        engine_qualified_pass_rate: total.engine_qualified_pass_rate(),
        provider_invalid_count: total.provider_failures,
        evaluator_invalid_count: total.evaluator_failures,
        capability_pass_rate: total.capability_pass_rate(),
        total_tokens: total.tokens,
        median_duration_ms: percentile(&total.durations, 0.50),
        p95_duration_ms: percentile(&total.durations, 0.95),
        protocol_violations: results
            .iter()
            .map(|result| result.protocol_violations.len() as u64)
            .sum(),
        eval_provider_display_name: &run.eval_provider_display_name,
        compaction_engine: &run.compaction_engine,
        engine_comparison: engine_rows,
        baseline: baseline_root
            .map(|root| compare_runs(&results, root))
            .transpose()?,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&summary)
            .map_err(|error| AppError::Message(error.to_string()))?
    );
    Ok(())
}

fn is_pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        let success = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
        unsafe { CloseHandle(handle) };
        success != 0 && exit_code == 259
    }
    #[cfg(not(target_os = "windows"))]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
}

pub fn print_triage(run_root: &Path) -> AppResult<()> {
    if let Ok(run) = read_json::<RunRecord>(&run_root.join("run.json")) {
        if run.status == "running" {
            if let Some(pid) = run.pid {
                if !is_pid_alive(pid) {
                    println!("[STALE RUN] Run is marked 'running' (PID {pid}), but process is no longer active. Previous run was aborted.");
                } else {
                    println!("[ACTIVE RUN] Run is actively executing under PID {pid}.");
                }
            } else {
                println!("[WARNING] Run is in 'running' state; process may have terminated unexpectedly.");
            }
        }
    }
    let results = read_results(&run_root.join("results.jsonl"))?;
    let mut grouped = BTreeMap::<String, Vec<&TaskResult>>::new();
    for result in results.iter().filter(|result| !result.passed) {
        grouped
            .entry(
                result
                    .failure_class
                    .clone()
                    .unwrap_or_else(|| "indeterminate".into()),
            )
            .or_default()
            .push(result);
    }
    if grouped.is_empty() {
        println!("No failed cases.");
        return Ok(());
    }
    for (class, cases) in grouped {
        println!("\n[{class}] {} case(s)", cases.len());
        for result in cases {
            println!(
                "- {} · {} · {} · violations={} · trace=traces/{}.gateway.jsonl",
                result.task_id,
                result.provider,
                result.model,
                if result.protocol_violations.is_empty() {
                    "none".into()
                } else {
                    result.protocol_violations.join(",")
                },
                result.case_id
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]

    use super::*;
    use crate::eval::events::EventMetrics;
    use std::io::Write;

    fn run_record(id: &str) -> RunRecord {
        RunRecord {
            run_id: id.into(),
            status: "completed".into(),
            pid: None,
            suite: "smoke".into(),
            suite_version: "1".into(),
            suite_hash: "abc".into(),
            dataset_hashes: HashMap::new(),
            vellum_git_sha: "sha".into(),
            vellum_version: "0.1.0".into(),
            codex_version: "0.142.5".into(),
            executor: "windows".into(),
            catalog_mode: "production".into(),
            executor_platform: "windows".into(),
            shell_contract: "windows-powershell".into(),
            container_image_id: "sha256:image".into(),
            models: vec!["model-a".into()],
            oauth_account: None,
            grok_account: None,
            max_official_turns: None,
            profile: None,
            lane: None,
            transition: None,
            repeat: 1,
            seed: 0,
            max_tasks: None,
            max_wall_seconds: None,
            max_total_tokens: None,
            natural_context: false,
            task_filter: None,
            category_filter: None,
            provider_filter: None,
            tag_filter: None,
            control_model: None,
            baseline_run: None,
            compaction_engine: "canonical".into(),
            recovery_mode: "recover".into(),
            eval_provider_display_name: "OpenAI".into(),
            grok_compaction: "native".into(),
            canonical_contract_version: 2,
            schedule: "round-robin".into(),
            fail_fast: None,
            ablation_profile: None,
            runtime_digest: None,
            enhanced_codex_commit: None,
            enhanced_feature_flags: None,
            enhanced_artifact_sha256: None,
            started_at: "now".into(),
            completed_at: Some("later".into()),
            task_count: 1,
        }
    }

    fn result(passed: bool) -> TaskResult {
        TaskResult {
            run_id: "run-1".into(),
            case_id: "case-1".into(),
            task_id: "task-1".into(),
            category: "short-fix".into(),
            model: "model-a".into(),
            provider: "provider-a".into(),
            context_window: Some(16_000),
            compaction_mode: None,
            compact_request_count: 0,
            canonical_hash: None,
            canonical_schema_version: None,
            journal_schema_version: None,
            checkpoint_schema_version: None,
            canonical_strategy: None,
            continuity_kind: None,
            requested_canonical_engine: None,
            observed_canonical_engine: None,
            requested_compaction_engine: None,
            observed_compaction_engine: None,
            compaction_engine_matched: None,
            codex_compaction_events: 0,
            remote_compact_requests: 0,
            vellum_canonical_journal_count: 0,
            catalog_hash: None,
            eval_provider_display_name: None,
            pre_compaction_model_visible_tokens: None,
            post_compaction_model_visible_tokens: None,
            generation: None,
            source_tokens: None,
            checkpoint_tokens: None,
            semantic_claim_count: None,
            grounded_claim_count: None,
            rejected_claim_count: None,
            grounding_rate: None,
            extraction_attempts: None,
            fallback_used: None,
            repeated_exchanges_collapsed: None,
            canonical_kind: None,
            injected_faults: Vec::new(),
            http_faults: Vec::new(),
            switched_models: Vec::new(),
            transition: "model-a -> model-a".into(),
            repetition: 1,
            passed,
            task_correctness_passed: passed,
            performance_qualified: true,
            protocol_qualified: true,
            agent_completed: true,
            timed_out: false,
            limit_exceeded: false,
            max_input_tokens: None,
            max_output_tokens: None,
            duration_ms: 1000,
            first_byte_ms: Some(100),
            compaction_recovered: None,
            model_switch_recovered: None,
            acceptance_passed: u64::from(passed),
            acceptance_total: 1,
            verification_exit_code: Some(if passed { 0 } else { 1 }),
            verification_output: String::new(),
            changed_files: vec!["solution.py".into()],
            added_lines: 1,
            removed_lines: 1,
            metrics: EventMetrics {
                input_tokens: 100,
                output_tokens: 20,
                ..EventMetrics::default()
            },
            protocol_violations: Vec::new(),
            harness_expectation_misses: Vec::new(),
            failure_class: (!passed).then(|| "model_capability".into()),
            root_cause: (!passed).then(|| "model_capability".into()),
            source_model_visible_tokens: None,
            replacement_model_visible_tokens: None,
            replacement_durable_tokens: None,
            model_visible_compression_ratio: None,
            source_hash: None,
            fallback_reason: None,
            failure_origin: (!passed).then_some(crate::eval::runner::FailureOrigin::Model),
            phase_observations: Vec::new(),
            phase_execution_observations: Vec::new(),
            compaction_attempts: Vec::new(),
            semantic_validation_diagnostics: Vec::new(),
            task_stall_diagnostics: Vec::new(),
            task_stall_recoveries: Vec::new(),
            task_stall_terminals: Vec::new(),
            task_efficiency_diagnostics: Vec::new(),
            task_efficiency_recoveries: Vec::new(),
            task_efficiency_usage_coverage: Default::default(),
            supporting_evidence: vec!["acceptance=1/1".into()],
            error: None,
            layers: crate::eval::runner::LayerResults {
                task_acceptance: passed,
                transport_protocol: true,
                tool_protocol: true,
                expected_tool_activity: true,
                compaction_triggered: true,
                canonical_materialized: true,
                compaction_applied: true,
                session_resumed: true,
                continuity_preserved: true,
                resource_budget: true,
                runtime_attribution: true,
                mechanism_exercised: true,
                promotion_eligible: passed,
            },
        }
    }

    fn write_run(root: &Path, id: &str, value: &TaskResult) {
        std::fs::write(
            root.join("run.json"),
            serde_json::to_vec(&run_record(id)).unwrap(),
        )
        .unwrap();
        let mut file = std::fs::File::create(root.join("results.jsonl")).unwrap();
        serde_json::to_writer(&mut file, value).unwrap();
        writeln!(file).unwrap();
    }

    #[test]
    fn percentile_uses_nearest_rank_without_panicking() {
        assert_eq!(percentile(&[], 0.95), 0);
        assert_eq!(percentile(&[10, 20, 30, 40], 0.50), 30);
        assert_eq!(percentile(&[10, 20, 30, 40], 0.95), 40);
    }

    #[test]
    fn engine_comparison_excludes_void_and_provider_attempts_from_task_pass() {
        let mut pass = result(true);
        pass.requested_compaction_engine = Some("codex-local".into());
        pass.observed_compaction_engine = Some("codex_local".into());
        pass.compaction_engine_matched = Some(true);
        pass.pre_compaction_model_visible_tokens = Some(12_000);
        pass.post_compaction_model_visible_tokens = Some(3_000);

        let mut wrong_engine = result(false);
        wrong_engine.requested_compaction_engine = Some("codex-local".into());
        wrong_engine.observed_compaction_engine = Some("remote_v2".into());
        wrong_engine.compaction_engine_matched = Some(false);
        wrong_engine.failure_origin =
            Some(crate::eval::runner::FailureOrigin::EvaluatorInfrastructure);

        let mut provider = result(false);
        provider.requested_compaction_engine = Some("codex-local".into());
        provider.observed_compaction_engine = Some("codex_local".into());
        provider.compaction_engine_matched = Some(true);
        provider.failure_origin = Some(crate::eval::runner::FailureOrigin::Provider);

        let rows = engine_comparison(&[pass, wrong_engine, provider]);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.attempts, 3);
        assert_eq!(row.evaluable, 1);
        assert_eq!(row.task_pass, "1/1");
        assert_eq!(row.evaluator_void, 1);
        assert_eq!(row.provider_invalid, 1);
        assert_eq!(row.compression_ratio, Some(0.25));
    }

    #[test]
    fn writes_clean_utf8_report() {
        let temp = tempfile::tempdir().unwrap();
        write_run(temp.path(), "run-1", &result(true));
        let report = write_html_report(temp.path()).unwrap();
        let html = std::fs::read_to_string(report).unwrap();
        assert!(html.contains("Vellum 第三方模型 Harness 評測"));
        assert!(html.contains("model-a"));
        assert!(html.contains("100.0%"));
        assert!(html.contains("patches/case-1.patch"));
    }

    #[test]
    fn action_recovery_report_counts_rendered_overlays_from_request_diagnostics() {
        use vellum_proxy_runtime::task_efficiency::{
            ActionRecoveryLevel, ActiveExecutionRecovery, TaskEfficiencyDiagnostic,
            TaskEfficiencyPolicy, TaskEfficiencyState,
        };

        let temp = tempfile::tempdir().unwrap();
        let mut result = result(true);
        let mut state = TaskEfficiencyState::default();
        state.active_action_recovery = Some(ActiveExecutionRecovery {
            recovery_count: 1,
            level: ActionRecoveryLevel::ConvertEvidence,
            started_request_index: Some(11),
            started_input_tokens: 61_000,
            marker_source_index: Some(20),
            post_recovery_tool_results: 0,
            post_recovery_input_tokens: 0,
            exploration_ops: 0,
            validation_ops: 0,
            mutation_ops: 0,
            compactions_since_activation: 0,
        });
        let policy = TaskEfficiencyPolicy::recover();
        let mut l1 = TaskEfficiencyDiagnostic::from_state(
            &state,
            &policy,
            Some("sha256:req-11".into()),
            Some(11),
        );
        l1.model_facing_recovery = Some("researchSprawlL1".into());
        state.active_action_recovery.as_mut().unwrap().level = ActionRecoveryLevel::ActionRequired;
        state
            .active_action_recovery
            .as_mut()
            .unwrap()
            .compactions_since_activation = 2;
        let mut l2 = TaskEfficiencyDiagnostic::from_state(
            &state,
            &policy,
            Some("sha256:req-14".into()),
            Some(14),
        );
        l2.model_facing_recovery = Some("researchSprawlL2".into());
        state.active_action_recovery = None;
        let closed = TaskEfficiencyDiagnostic::from_state(
            &state,
            &policy,
            Some("sha256:req-15".into()),
            Some(15),
        );
        result.task_efficiency_diagnostics = vec![l1, l2, closed];

        write_run(temp.path(), "run-action-recovery", &result);
        let report = write_html_report(temp.path()).unwrap();
        let html = std::fs::read_to_string(report).unwrap();
        assert!(html.contains("activeRecovery=closed"));
        assert!(html.contains("recoveryActivatedAt=#11"));
        assert!(html.contains("overlaysRendered=2"));
        assert!(html.contains("overlayRequests=[#11,#14]"));
        assert!(html.contains("levelEscalatedAt=#14"));
        assert!(html.contains("compactsWhileActive=2"));
    }

    #[test]
    fn baseline_detects_new_stable_regression() {
        let current = tempfile::tempdir().unwrap();
        let baseline = tempfile::tempdir().unwrap();
        write_run(current.path(), "current", &result(false));
        write_run(baseline.path(), "baseline", &result(true));
        let comparison = compare_runs(
            &read_results(&current.path().join("results.jsonl")).unwrap(),
            baseline.path(),
        )
        .unwrap();
        assert_eq!(comparison.newly_regressed_tasks.len(), 1);
        assert!(!comparison.pass_rate_gate);
    }

    #[test]
    fn incomplete_or_case_mismatched_baseline_is_rejected() {
        let current = vec![result(false)];
        let baseline = tempfile::tempdir().unwrap();
        write_run(baseline.path(), "baseline", &result(true));

        let mut incomplete = run_record("baseline");
        incomplete.status = "running".into();
        std::fs::write(
            baseline.path().join("run.json"),
            serde_json::to_vec(&incomplete).unwrap(),
        )
        .unwrap();
        assert!(compare_runs(&current, baseline.path())
            .unwrap_err()
            .to_string()
            .contains("is not completed"));

        std::fs::write(
            baseline.path().join("run.json"),
            serde_json::to_vec(&run_record("baseline")).unwrap(),
        )
        .unwrap();
        let mut mismatched = result(true);
        mismatched.case_id = "different-case".into();
        let mut file = std::fs::File::create(baseline.path().join("results.jsonl")).unwrap();
        serde_json::to_writer(&mut file, &mismatched).unwrap();
        writeln!(file).unwrap();
        assert!(compare_runs(&current, baseline.path())
            .unwrap_err()
            .to_string()
            .contains("is not comparable"));
    }

    #[test]
    fn interrupted_run_without_results_is_reportable() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("run.json"),
            serde_json::to_vec(&run_record("interrupted")).unwrap(),
        )
        .unwrap();
        assert!(read_results(&temp.path().join("results.jsonl"))
            .unwrap()
            .is_empty());
        let report = write_html_report(temp.path()).unwrap();
        assert!(std::fs::read_to_string(report)
            .unwrap()
            .contains("interrupted"));
    }

    #[test]
    fn read_results_uses_latest_attempt_per_case() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("results.jsonl");
        let mut first = result(false);
        first.failure_origin = Some(crate::eval::runner::FailureOrigin::Provider);
        first.failure_class = Some("provider_quota".into());
        let latest = result(true);
        let mut file = std::fs::File::create(&path).unwrap();
        serde_json::to_writer(&mut file, &first).unwrap();
        writeln!(file).unwrap();
        serde_json::to_writer(&mut file, &latest).unwrap();
        writeln!(file).unwrap();

        let results = read_results(&path).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].passed);
    }
}
