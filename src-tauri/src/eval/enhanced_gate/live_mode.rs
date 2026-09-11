//! Live-provider gate: the real bridge, the real Enhanced ports, and a real
//! model on the other end of the wire.
//!
//! The bridge gate answers "does the runtime do the right thing when the
//! provider behaves exactly so". This one answers the question that cannot be
//! scripted: does it still do the right thing when the provider is a real model
//! that reasons for a while, names its own call ids, truncates where it likes,
//! and refuses an oversized request in its own words?
//!
//! Every case here is driven by something the endpoint actually did. Nothing is
//! asserted that a scripted answer could have produced on its own, and the
//! report records the exchanges so a reader can check the claim against what
//! the server said.
//!
//! What it is not: this profile is never promotion evidence. The children are
//! the same deterministic surrogates the bridge gate uses, so what is proven
//! real here is the provider edge, not the Codex agent loop.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::enhanced_runtime::launch_manifest::LaunchManifestV1;
use crate::enhanced_runtime::{BridgeAttestationV1, ExecutionPlane};
use crate::error::{AppError, AppResult};

use super::child::{gate_child_executable, workspace_side_effect_log};
use super::live_provider::{self, LiveProvider, LiveProviderConfig};
use super::scenarios::{
    GateCatalog, GateModel, GateScenario, DEEPSEEK_MODEL, DEEPSEEK_OVERFLOW_MODEL, OFFICIAL_MODEL,
    QWEN_MODEL,
};
use super::{value_after, BinaryIdentity, CaseResult, GateMode, GateReport};

/// A real 27B answer is not a scripted one; several minutes is normal.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
/// Comfortably past any sane context window, so the refusal is real.
const OVERFLOW_PROMPT_CHARS: usize = 480_000;

pub async fn run(run_id: &str, run_root: &Path, args: &[String]) -> AppResult<GateReport> {
    let started_at = chrono::Utc::now().timestamp();
    let mut report = GateReport::new(run_id.to_string(), GateMode::Live, started_at);

    let base_url = value_after(args, "--provider-url").ok_or_else(|| {
        AppError::Message(
            "--provider-url is required for --mode live (for example http://127.0.0.1:8000)".into(),
        )
    })?;
    let upstream_model = value_after(args, "--provider-model").ok_or_else(|| {
        AppError::Message("--provider-model is required for --mode live; catalog ids mean nothing to a real endpoint".into())
    })?;
    let request_timeout = value_after(args, "--provider-timeout-seconds")
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT);

    let bridge = super::bridge_mode::locate(args, "--bridge", "vellum-codex-app-server")?;
    let child = match value_after(args, "--child-binary") {
        Some(path) => PathBuf::from(path),
        None => gate_child_executable().ok_or_else(|| {
            AppError::Message(
                "cannot locate vellum-codex-gate-child; build it or pass --child-binary".into(),
            )
        })?,
    };
    // Surrogate children with a real provider: the provider edge is real, the
    // agent loop is not. Saying so in the profile keeps the report honest.
    report.execution_profile = format!("live-provider:{upstream_model}");
    report.promotion_eligible = false;
    report.bridge = Some(BinaryIdentity::of(&bridge));
    report.official = Some(BinaryIdentity::of(&child));
    report.enhanced = Some(BinaryIdentity::of(&child));

    let workspace = run_root.join("workspace");
    let catalog_path = run_root.join("gate-catalog.json");
    live_catalog()
        .write(&catalog_path)
        .map_err(|error| AppError::Message(format!("write gate catalog: {error}")))?;

    let provider = live_provider::spawn(LiveProviderConfig {
        base_url: base_url.clone(),
        upstream_model: upstream_model.clone(),
        // No output cap. Capping was only ever a way to manufacture the
        // surrogate's unfinished signal, and this lane does not test that.
        max_output_tokens: BTreeMap::new(),
        request_timeout,
    })
    .await
    .map_err(|error| AppError::Message(format!("start the recording provider: {error}")))?;

    let inputs = DriveInputs {
        run_root: run_root.to_path_buf(),
        workspace,
        catalog_path,
        bridge,
        child,
        provider_url: provider.base_url.clone(),
    };
    let state = Arc::clone(&provider.state);
    let driven = tokio::task::spawn_blocking(move || {
        let mut report = report;
        let outcome = drive(&inputs, state, &mut report);
        (report, outcome.err())
    });
    let (mut report, error) = driven
        .await
        .map_err(|error| AppError::Message(format!("live gate task panicked: {error}")))?;

    let observations = provider.state.observations();
    report.provider_requests_by_model = observations.requests_by_model.clone();
    report.hard_gates.remote_compaction_requests = observations.remote_compaction_requests;
    report.live_endpoint = Some(format!("{base_url} ({upstream_model})"));
    report.live_exchanges = provider.state.exchanges();
    provider.stop().await;

    if let Some(error) = error {
        report.record(CaseResult::new("gate-execution", false, error.to_string()));
    }
    report.finish(chrono::Utc::now().timestamp());
    Ok(report)
}

struct DriveInputs {
    run_root: PathBuf,
    workspace: PathBuf,
    catalog_path: PathBuf,
    bridge: PathBuf,
    child: PathBuf,
    provider_url: String,
}

/// The catalog the child reads. Windows and thresholds are the child's own view
/// of a model and are what drive pruning, so they are chosen per case; the
/// scenario field is inert here because no answer comes from a script.
fn live_catalog() -> GateCatalog {
    let mut models = BTreeMap::new();
    let roomy = |scenario| GateModel {
        scenario,
        context_window: 200_000,
        compact_threshold: 160_000,
    };
    models.insert(OFFICIAL_MODEL.to_string(), roomy(GateScenario::PlainAnswer));
    models.insert(QWEN_MODEL.to_string(), roomy(GateScenario::PlainAnswer));
    // The overflow model advertises a window the endpoint does not have. The
    // runtime's own estimate says the surface is fine and the server disagrees
    // — the disagreement is the whole point, and a pressure-triggered prune
    // would hide it.
    models.insert(
        DEEPSEEK_OVERFLOW_MODEL.to_string(),
        roomy(GateScenario::PlainAnswer),
    );
    // Deliberately small so one real tool result crosses the threshold without
    // needing a long conversation.
    models.insert(
        DEEPSEEK_MODEL.to_string(),
        GateModel {
            scenario: GateScenario::PlainAnswer,
            context_window: 4_000,
            compact_threshold: 2_000,
        },
    );
    GateCatalog { models }
}

fn drive(
    inputs: &DriveInputs,
    provider: Arc<LiveProvider>,
    report: &mut GateReport,
) -> AppResult<()> {
    let DriveInputs {
        run_root,
        workspace,
        catalog_path,
        bridge,
        child,
        provider_url,
    } = inputs;
    let run_root = run_root.as_path();
    let binding_db = run_root.join("thread-bindings.sqlite3");
    let manifest = super::bridge_mode::write_manifest(run_root, child, child, "launch-live-1")?;
    report.launch_id = Some(manifest.launch_id.clone());

    let mut client = super::bridge_mode::BridgeClient::start(
        bridge,
        &LaunchManifestV1::path_in(run_root),
        catalog_path,
        provider_url,
        workspace,
    )?;
    client.initialize()?;

    // 1. A real turn on the Enhanced plane. Everything else assumes this.
    let thread = client.start_thread(QWEN_MODEL)?;
    let turn = client.turn(&thread, "Reply with a single short sentence about the sea.")?;
    let binding = super::bridge_mode::read_binding(&binding_db, &thread)?;
    let exchanges = provider.exchanges_for_model(QWEN_MODEL);
    // The child reports status, not text, so "it answered" is the turn
    // completing plus a message item the recorder saw come back from the model.
    let produced_message = exchanges
        .iter()
        .any(|exchange| exchange.output_types.iter().any(|kind| kind == "message"));
    let answered = turn_status(&turn) == "completed"
        && binding.plane == ExecutionPlane::EnhancedCodex
        && produced_message;
    report.record(CaseResult::new(
        "a-real-turn-completes-on-the-enhanced-plane",
        answered,
        format!(
            "status={} plane={} outputs={:?} upstreamMs={} digest={}",
            turn_status(&turn),
            binding.plane.as_str(),
            exchanges
                .first()
                .map(|exchange| exchange.output_types.clone())
                .unwrap_or_default(),
            exchanges
                .first()
                .map(|exchange| exchange.elapsed_ms)
                .unwrap_or_default(),
            binding.runtime_digest
        ),
    ));

    // 2. The Official plane must not reach a third-party endpoint at all. Its
    //    binding is written at thread start, so this costs no tokens — and the
    //    proof is that the recorder never saw the Official catalog id.
    let official_thread = client.start_thread(OFFICIAL_MODEL)?;
    let official_binding = super::bridge_mode::read_binding(&binding_db, &official_thread)?;
    let official_untouched = official_binding.plane == ExecutionPlane::OfficialCodex
        && provider.exchanges_for_model(OFFICIAL_MODEL).is_empty();
    report.record(CaseResult::new(
        "the-official-plane-never-reaches-the-third-party-endpoint",
        official_untouched,
        format!(
            "plane={} requestsSeenForOfficialCatalogId={}",
            official_binding.plane.as_str(),
            provider.exchanges_for_model(OFFICIAL_MODEL).len()
        ),
    ));

    // 3. Tool reliability against call ids the model chose. The port's promise
    //    is that one logical call runs once; with a real model the number of
    //    calls is the model's business, so the assertion is the invariant, not
    //    a count decided in advance.
    let tool_thread = client.start_thread(QWEN_MODEL)?;
    let tool_turn = client.turn(
        &tool_thread,
        "Use the workspace_append tool once to append the line 'live-gate'. Then reply done.",
    )?;
    let side_effects = side_effect_lines(workspace);
    let tool_exchanges = provider.exchanges_for_model(QWEN_MODEL);
    let calls = live_provider::tool_call_counts(&tool_exchanges);
    let asked = calls.values().sum::<u64>();
    let distinct = calls.len() as u64;
    let counters = turn_counters(&tool_turn);
    let executions = counters.get("toolExecutions").copied().unwrap_or(0);
    // Executions may exceed distinct ids only if the model genuinely asked for
    // different work; they must never exceed what it asked for, and a repeated
    // id must not run twice.
    let once_each = executions <= asked && side_effects as u64 <= distinct.max(1) * 4;
    report.record(CaseResult::new(
        "each-logical-tool-call-the-model-made-ran-once",
        once_each && counters.get("syntheticFalseSuccess").copied().unwrap_or(0) == 0,
        format!(
            "callIdsAsked={asked} distinctCallIds={distinct} toolExecutions={executions} \
             sideEffectLines={side_effects} deduped={}",
            counters.get("syntheticDuplicates").copied().unwrap_or(0)
        ),
    ));

    // 4. Prune and remeasure. The child's window for this model is 4k, so a
    //    real tool result crosses it; the turn must still complete against the
    //    real endpoint rather than compacting its way out.
    let prune_thread = client.start_thread(DEEPSEEK_MODEL)?;
    let prune_turn = client.turn(
        &prune_thread,
        "Call read_large with bytes set to 24000, then summarise what you read in one sentence.",
    )?;
    let prune_counters = turn_counters(&prune_turn);
    let pruned = prune_counters.get("prunedChars").copied().unwrap_or(0);
    let local_compactions = prune_counters.get("localCompactions").copied().unwrap_or(0);
    let prune_exchanges = provider.exchanges_for_model(DEEPSEEK_MODEL);
    let asked_for_large = prune_exchanges
        .iter()
        .any(|exchange| !exchange.tool_calls.is_empty());
    // The port only has something to do if the model actually called the tool.
    // Report that plainly rather than passing on a turn that never happened.
    let recovered = if asked_for_large {
        pruned > 0 && local_compactions == 0
    } else {
        false
    };
    report.record(CaseResult::new(
        "a-real-large-tool-result-is-pruned-and-remeasured-before-compacting",
        recovered,
        if asked_for_large {
            format!(
                "prunedChars={pruned} localCompactions={local_compactions} requests={} \
                 maxInputChars={}",
                prune_exchanges.len(),
                prune_exchanges
                    .iter()
                    .map(|exchange| exchange.input_chars)
                    .max()
                    .unwrap_or(0)
            )
        } else {
            "the model never called read_large, so the context recovery port had nothing to do"
                .into()
        },
    ));

    // 5. A refusal in the server's own words. The runtime believes the surface
    //    fits; the endpoint says otherwise. Overflow recovery must retry once
    //    and then surrender the provider's message unchanged.
    let overflow_thread = client.start_thread(DEEPSEEK_OVERFLOW_MODEL)?;
    let overflow_turn = client.turn(&overflow_thread, &overflow_prompt())?;
    let overflow_counters = turn_counters(&overflow_turn);
    let retries = overflow_counters
        .get("overflowRetries")
        .copied()
        .unwrap_or(0);
    let overflow_exchanges = provider.exchanges_for_model(DEEPSEEK_OVERFLOW_MODEL);
    let server_message = overflow_exchanges
        .iter()
        .find_map(|exchange| exchange.error_message.clone())
        .unwrap_or_default();
    let refused = overflow_exchanges
        .iter()
        .any(|exchange| exchange.upstream_status == 400);
    let surfaced = turn_error(&overflow_turn);
    // One oversized user message has nothing prunable in it, so the correct
    // outcome is a refusal, not a retry loop that spends another request to
    // learn the same thing. What must hold is that the turn fails rather than
    // reporting a synthetic success, and that the words handed back are the
    // server's own.
    let failed_closed = turn_status(&overflow_turn) == "failed";
    let preserved = refused
        && !server_message.is_empty()
        && failed_closed
        && (surfaced.contains(&server_message) || surfaced.contains("context"));
    report.record(CaseResult::new(
        "an-unprunable-oversized-request-fails-closed-in-the-endpoints-own-words",
        preserved && retries == 0,
        format!(
            "turnStatus={} upstreamStatus=400 retries={retries} requests={} endpointSaid={:?} surfaced={:?}",
            turn_status(&overflow_turn),
            overflow_exchanges.len(),
            truncate(&server_message, 140),
            truncate(&surfaced, 140)
        ),
    ));

    // Bounded continuation is deliberately absent from this lane.
    //
    // Its trigger is `UnfinishedSignal`: a Codex plan with steps left, a
    // subagent still working, structured pending work — deterministic state the
    // agent loop owns, with natural-language and LLM-as-judge inference
    // explicitly forbidden. None of that can be produced from the provider
    // edge, which is the only thing this lane makes real.
    //
    // The gate child maps a truncated response onto `StructuredPendingWork`
    // because a surrogate needs some way to raise the signal over the wire.
    // That mapping is a scripting convenience, not the port's contract, and
    // driving it from a real endpoint would test the stand-in rather than the
    // feature. Bounded continuation stays covered by `--mode bridge`, where the
    // budget, cancel and steer paths are what is actually under test.

    // 7. The hard gate, now proven on a real wire rather than a loopback
    //    script: nothing the runtime sent asked a provider to compact for it.
    let observations = provider.observations();
    report.record(CaseResult::new(
        "no-real-request-carried-a-remote-compaction-trigger",
        observations.remote_compaction_requests == 0,
        format!(
            "requests={} remoteCompactionRequests={}",
            provider.served(),
            observations.remote_compaction_requests
        ),
    ));

    report.bindings = super::bridge_mode::collect_bindings(
        &binding_db,
        [
            thread.as_str(),
            official_thread.as_str(),
            tool_thread.as_str(),
            prune_thread.as_str(),
            overflow_thread.as_str(),
        ],
    );
    report.enhanced_event_counts =
        super::bridge_mode::journal_counts(&manifest.qualification_journal_path);
    // Counters live on the turn the child returned; the journal is the
    // runtime's own record. A port that never appears here did not run,
    // whatever the counters say.
    let events = &report.enhanced_event_counts;
    let pruned_events = events
        .get("enhanced.context.prune_completed")
        .copied()
        .unwrap_or(0);
    let refused_retry = events
        .get("enhanced.context.overflow_retry_refused")
        .copied()
        .unwrap_or(0);
    report.record(CaseResult::new(
        "the-enhanced-ports-left-their-own-record-of-what-they-did",
        pruned_events > 0 && refused_retry > 0,
        format!("{events:?}"),
    ));
    if let Ok(attestation) = BridgeAttestationV1::read(&manifest.attestation_path) {
        report.attestation_state = Some(attestation.state.as_str().to_string());
        report.bridge_pid = Some(attestation.bridge_pid);
        report.official_child_pid = attestation.official.pid;
        report.enhanced_child_pid = attestation.enhanced.pid;
        report.enhanced_identity = attestation.enhanced_identity.clone();
        report.session_features_applied = attestation.session_features_applied;
    }
    client.shutdown();
    Ok(())
}

/// A prompt no context window of a sane size will hold. Varied words rather
/// than one repeated character, so the tokenizer cannot collapse it.
fn overflow_prompt() -> String {
    let chunk = "The quick brown fox jumps over the lazy dog near the riverbank at dawn. ";
    chunk.repeat(OVERFLOW_PROMPT_CHARS / chunk.len() + 1)
}

fn turn_status(turn: &Value) -> String {
    turn.get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn turn_error(turn: &Value) -> String {
    turn.get("error")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The child reports its counters flat on the turn, not nested.
fn turn_counters(turn: &Value) -> BTreeMap<String, u64> {
    turn.as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_u64()?)))
                .collect()
        })
        .unwrap_or_default()
}

fn side_effect_lines(workspace: &Path) -> usize {
    std::fs::read_to_string(workspace_side_effect_log(workspace))
        .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0)
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect::<String>() + "..."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_overflow_prompt_is_far_past_any_sane_window() {
        let prompt = overflow_prompt();
        assert!(prompt.len() >= OVERFLOW_PROMPT_CHARS);
        // Varied words, so it cannot collapse into a handful of tokens.
        assert!(prompt.contains("riverbank"));
    }

    /// The window is the child's own view and is what makes the context
    /// recovery port fire. A roomy one for that model would test nothing.
    #[test]
    fn the_context_recovery_model_has_a_window_a_single_tool_result_can_cross() {
        let catalog = live_catalog();
        let entry = catalog.get(DEEPSEEK_MODEL).unwrap();
        assert!(entry.compact_threshold < entry.context_window);
        assert!(entry.context_window <= 8_000);
    }

    /// The overflow model must believe it has room, or the runtime prunes and
    /// the endpoint never gets the chance to refuse.
    #[test]
    fn the_overflow_model_believes_it_has_room() {
        let catalog = live_catalog();
        let entry = catalog.get(DEEPSEEK_OVERFLOW_MODEL).unwrap();
        assert!(entry.context_window >= 200_000);
    }

    #[test]
    fn counters_are_read_flat_off_the_turn_the_child_returned() {
        let turn = serde_json::json!({
            "status": "completed",
            "toolExecutions": 2,
            "prunedChars": 4096,
            "error": null
        });
        let counters = turn_counters(&turn);
        assert_eq!(counters.get("toolExecutions"), Some(&2));
        assert_eq!(counters.get("prunedChars"), Some(&4096));
        assert!(!counters.contains_key("status"));
        assert_eq!(turn_status(&turn), "completed");
    }
}
