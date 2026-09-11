//! Bridge-mode gate: the real bridge binary, App Server protocol children, a
//! scripted local provider, and no human in the loop. By default the children
//! are deterministic surrogates using the real portable Enhanced modules; that
//! profile is a component gate, not release-promotion evidence. Explicit
//! Official and Enhanced binaries are recorded separately when supplied.
//!
//! Everything here is over the wire. The gate speaks the App Server protocol to
//! the bridge's stdin/stdout exactly as Codex Desktop would, and reads its
//! conclusions back out of the durable binding database, the bridge attestation
//! and the qualification journal — the same three artifacts the product uses.
//! Nothing is asserted from in-process state that the product would not have.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use vellum_enhanced_codex::EnhancedRuntimeLockFile;

use crate::enhanced_runtime::launch_manifest::{
    sha256_file, LaunchManifestV1, RuntimeBinaryIdentity, LAUNCH_MANIFEST_ENV,
    LAUNCH_MANIFEST_SCHEMA_VERSION,
};
use crate::enhanced_runtime::qualification::{JournalEntry, QualificationJournal};
use crate::enhanced_runtime::{
    BridgeAttestationV1, ExecutionPlane, ModelProviderRoute, ThreadRuntimeBindingStore,
    TrustedModelProviderMap, FEATURE_DEFAULTS_MVP, MODEL_PROVIDER_MAP_SCHEMA_VERSION,
};
use crate::error::{AppError, AppResult};
use crate::model::{
    AuthKind, CatalogScope, ModelRoute, ProviderKind, ReasoningEffortTransport, Route, WireFormat,
};

use super::child::{gate_child_executable, workspace_side_effect_log};
use super::provider;
use super::scenarios::{
    default_catalog, DEEPSEEK_MODEL, DEEPSEEK_OVERFLOW_MODEL, DEEPSEEK_STUCK_MODEL,
    GATE_CATALOG_ENV, GATE_PROVIDER_URL_ENV, GATE_WORKSPACE_ENV, GROK_MODEL, NAME_TRAP_MODEL,
    OFFICIAL_MODEL, QWEN_CANCEL_MODEL, QWEN_CONTINUATION_MODEL, QWEN_MODEL, QWEN_STEER_MODEL,
    REAL_DEEPSEEK_SMOKE_MODEL, REAL_GROK_SMOKE_MODEL, REAL_QWEN_SMOKE_MODEL,
};
use super::{value_after, BinaryIdentity, CaseResult, GateMode, GateReport, ObservedBinding};

const OFFICIAL_PROVIDER: &str = "openai-official";
const QWEN_PROVIDER: &str = "qwen";
const DEEPSEEK_PROVIDER: &str = "deepseek";
const GROK_PROVIDER: &str = "grok";
/// The Vellum gateway all third-party traffic shares inside the child.
const CHILD_THIRD_PARTY_PROVIDER: &str = "vellum";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn run(run_id: &str, run_root: &Path, args: &[String]) -> AppResult<GateReport> {
    let started_at = chrono::Utc::now().timestamp();
    let mut report = GateReport::new(run_id.to_string(), GateMode::Bridge, started_at);

    let bridge = locate(args, "--bridge", "vellum-codex-app-server")?;
    let official = match value_after(args, "--official-binary") {
        Some(path) => PathBuf::from(path),
        None => gate_child_executable().ok_or_else(|| {
            AppError::Message(
                "cannot locate vellum-codex-gate-child; build it or pass --official-binary".into(),
            )
        })?,
    };
    let enhanced = match value_after(args, "--enhanced-binary") {
        Some(path) => PathBuf::from(path),
        None => official.clone(),
    };
    // Which suite to run and what the run may claim are two questions. Real
    // Codex binaries do not speak the gate child's protocol, so they get the
    // smoke path whether or not they turn out to be promotable; eligibility is
    // the stricter test below and never changes which cases execute.
    let real_binaries = !is_gate_child(&official) && !is_gate_child(&enhanced);
    // Reading the machine costs a few file opens, so only pay it once the
    // surrogates are out of the picture.
    let pinned_enhanced = if real_binaries {
        crate::enhanced_runtime::desktop_runtime_status(&crate::state::app_data_dir())
            .enhanced_codex_executable
    } else {
        None
    };
    let (profile, promotion_eligible) =
        execution_profile(&official, &enhanced, pinned_enhanced.as_deref());
    report.execution_profile = profile;
    report.promotion_eligible = promotion_eligible;
    report.bridge = Some(BinaryIdentity::of(&bridge));
    report.official = Some(BinaryIdentity::of(&official));
    report.enhanced = Some(BinaryIdentity::of(&enhanced));

    let workspace = run_root.join("workspace");
    let catalog_path = run_root.join("gate-catalog.json");
    default_catalog()
        .write(&catalog_path)
        .map_err(|error| AppError::Message(format!("write gate catalog: {error}")))?;
    let provider = provider::spawn(default_catalog())
        .await
        .map_err(|error| AppError::Message(format!("start scripted provider: {error}")))?;

    // The scripted provider serves on this runtime, so the protocol driving
    // has to leave the async worker alone.
    let inputs = DriveInputs {
        run_root: run_root.to_path_buf(),
        workspace,
        catalog_path,
        bridge: bridge.clone(),
        official: official.clone(),
        enhanced: enhanced.clone(),
        provider_url: provider.base_url.clone(),
    };
    let provider_state = Arc::clone(&provider.state);
    let driven = tokio::task::spawn_blocking(move || {
        let mut report = report;
        let outcome = drive(&inputs, provider_state, real_binaries, &mut report);
        (report, outcome.err())
    });
    let (mut report, error) = driven
        .await
        .map_err(|error| AppError::Message(format!("gate task panicked: {error}")))?;

    let observations = provider.state.observations();
    report.provider_requests_by_model = observations.requests_by_model.clone();
    report.hard_gates.remote_compaction_requests = observations.remote_compaction_requests;
    provider.stop().await;

    if let Some(error) = error {
        report.record(CaseResult::new("gate-execution", false, error.to_string()));
    }
    report.finish(chrono::Utc::now().timestamp());
    Ok(report)
}

pub(super) fn is_gate_child(path: &Path) -> bool {
    path.file_stem()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("vellum-codex-gate-child"))
}

/// What this run is entitled to claim, and — when it is not entitled to much —
/// the reason, because that string is what the footer prints after NO-GO.
///
/// The flags are not evidence. `--official-binary` and `--enhanced-binary`
/// take paths, and a path proves only that a file exists. Aimed at
/// yesterday's build, at a binary nobody pinned, or at one executable for
/// both planes, every case in this gate still goes green while proving
/// nothing about the runtime the user will actually run. Two cases would be
/// worse than useless: with a single binary behind both planes, "a dead
/// enhanced child leaves official serving" asserts that one process is both
/// dead and alive. So eligibility
/// is decided against the artifact this machine is pinned to, and anything that
/// cannot be tied back to it is named for why.
pub(super) fn execution_profile(
    official: &Path,
    enhanced: &Path,
    pinned_enhanced: Option<&Path>,
) -> (String, bool) {
    if is_gate_child(official) || is_gate_child(enhanced) {
        return ("surrogate-children".into(), false);
    }
    if super::preflight::same_path(official, enhanced) {
        return ("one-binary-serving-both-planes".into(), false);
    }
    match pinned_enhanced {
        None => (
            "enhanced-binary-is-not-pinned-on-this-machine".into(),
            false,
        ),
        Some(pinned) if !super::preflight::same_path(enhanced, pinned) => {
            ("enhanced-binary-is-not-the-pinned-artifact".into(), false)
        }
        Some(_) => ("explicit-runtime-binaries".into(), true),
    }
}

struct DriveInputs {
    run_root: PathBuf,
    workspace: PathBuf,
    catalog_path: PathBuf,
    bridge: PathBuf,
    official: PathBuf,
    enhanced: PathBuf,
    provider_url: String,
}

fn drive(
    inputs: &DriveInputs,
    provider: Arc<super::provider::ScriptedProvider>,
    real_binaries: bool,
    report: &mut GateReport,
) -> AppResult<()> {
    let DriveInputs {
        run_root,
        workspace,
        catalog_path,
        bridge,
        official,
        enhanced,
        provider_url,
    } = inputs;
    let run_root = run_root.as_path();
    let workspace = workspace.as_path();
    let binding_db = run_root.join("thread-bindings.sqlite3");
    let manifest = write_manifest(run_root, official, enhanced, "launch-bridge-1")?;
    stage_real_codex_homes(&manifest, provider_url)?;
    report.launch_id = Some(manifest.launch_id.clone());

    let mut client = BridgeClient::start(
        bridge,
        &LaunchManifestV1::path_in(run_root),
        catalog_path,
        provider_url,
        workspace,
    )?;
    client.initialize()?;

    if real_binaries {
        return drive_real_binary_smoke(client, &manifest, &binding_db, report);
    }

    // 1. Official isolation.
    let official_thread = client.start_thread(OFFICIAL_MODEL)?;
    let official_turn = client.turn(&official_thread, "official control task")?;
    let official_binding = read_binding(&binding_db, &official_thread)?;
    let official_isolated = official_binding.plane == ExecutionPlane::OfficialCodex
        && official_binding.provider_id == OFFICIAL_PROVIDER
        && official_turn["featureProfile"] == "E0"
        && official_turn["toolExecutions"] == 0
        && official_turn["continuations"] == 0
        && official_turn["prunedChars"] == 0;
    if !official_isolated {
        report.hard_gates.official_runtime_mutated += 1;
    }
    report.record(CaseResult::new(
        "official-model-binds-official-plane-only",
        official_isolated,
        format!(
            "plane={} provider={} profile={}",
            official_binding.plane.as_str(),
            official_binding.provider_id,
            official_turn["featureProfile"]
        ),
    ));

    // 2. Every third-party catalog id binds Enhanced, immutably.
    let mut third_party_threads = BTreeMap::new();
    let mut routing_ok = true;
    let mut routing_detail = Vec::new();
    for (model, expected_provider) in [
        (QWEN_MODEL, QWEN_PROVIDER),
        (DEEPSEEK_MODEL, DEEPSEEK_PROVIDER),
        (GROK_MODEL, GROK_PROVIDER),
        (NAME_TRAP_MODEL, QWEN_PROVIDER),
    ] {
        let thread = client.start_thread(model)?;
        let binding = read_binding(&binding_db, &thread)?;
        let correct = binding.plane == ExecutionPlane::EnhancedCodex
            && binding.provider_id == expected_provider
            && binding.runtime_digest == manifest.enhanced.runtime_digest;
        if !correct {
            routing_ok = false;
        }
        routing_detail.push(format!("{model}->{}", binding.provider_id));
        third_party_threads.insert(model, thread);
    }
    report.record(CaseResult::new(
        "third-party-catalog-ids-bind-enhanced",
        routing_ok,
        routing_detail.join(" "),
    ));

    // 3. Classification comes from the trusted map, never from a model name.
    let untrusted = client.request(
        "thread/start",
        json!({"modelProvider": CHILD_THIRD_PARTY_PROVIDER, "model": "not-in-the-map"}),
    )?;
    let trap_binding = read_binding(&binding_db, &third_party_threads[NAME_TRAP_MODEL])?;
    let classification_ok = untrusted.get("error").is_some()
        && trap_binding.plane == ExecutionPlane::EnhancedCodex
        && trap_binding.provider_id == QWEN_PROVIDER;
    report.record(CaseResult::new(
        "provider-classification-uses-the-trusted-map",
        classification_ok,
        format!(
            "unmapped model rejected={} gpt-shaped catalog id -> {}",
            untrusted.get("error").is_some(),
            trap_binding.provider_id
        ),
    ));

    // 4. Cancel, approval, and fork stay on the binding.
    let qwen_thread = third_party_threads[QWEN_MODEL].clone();
    let interrupted = client.request("turn/interrupt", json!({"threadId": qwen_thread}))?;
    let forked = client.request("thread/fork", json!({"threadId": qwen_thread}))?;
    let forked_thread = forked
        .pointer("/result/thread/id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let fork_binding = read_binding(&binding_db, &forked_thread)?;
    let stays_bound = interrupted.get("error").is_none()
        && fork_binding.plane == ExecutionPlane::EnhancedCodex
        && fork_binding.provider_id == QWEN_PROVIDER;
    report.record(CaseResult::new(
        "cancel-approval-and-fork-stay-on-the-binding",
        stays_bound,
        format!("fork plane={}", fork_binding.plane.as_str()),
    ));

    // 5. A provider switch on a bound thread is refused. The switch has to be
    //    expressed as a different catalog id, because the trusted map — not the
    //    `modelProvider` field, which every third party shares — is what
    //    decides who owns a model.
    let switched = client.request(
        "thread/resume",
        json!({"threadId": qwen_thread, "model": DEEPSEEK_MODEL}),
    )?;
    report.record(CaseResult::new(
        "provider-switch-on-a-bound-thread-is-refused",
        switched.get("error").is_some(),
        error_message(&switched),
    ));

    // 6. Qwen tool reliability: one side effect, one non-success duplicate.
    let tool_turn = client.turn(&qwen_thread, "run the tool twice")?;
    let side_effect_lines = std::fs::read_to_string(workspace_side_effect_log(workspace))
        .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0);
    if side_effect_lines > 1 {
        report.hard_gates.duplicate_logical_tool_execution += (side_effect_lines - 1) as u64;
    }
    let duplicate_suppressed = tool_turn["syntheticDuplicates"].as_u64().unwrap_or(0) == 1;
    let executed_once = tool_turn["toolExecutions"].as_u64().unwrap_or(0) == 1;
    report.record(CaseResult::new(
        "qwen-tool-reliability-executes-a-repeated-call-once",
        side_effect_lines == 1 && duplicate_suppressed && executed_once,
        format!(
            "sideEffectLines={side_effect_lines} toolExecutions={} syntheticDuplicates={}",
            tool_turn["toolExecutions"], tool_turn["syntheticDuplicates"]
        ),
    ));

    // 7. DeepSeek context recovery: prune, re-measure, then decide.
    let deepseek_thread = third_party_threads[DEEPSEEK_MODEL].clone();
    let context_turn = client.turn(&deepseek_thread, "read the large file")?;
    let pruned = context_turn["prunedChars"].as_u64().unwrap_or(0) > 0;
    let avoided = context_turn["compactionAvoided"] == json!(true);
    let pairs_intact = context_turn["toolPairsIntact"] == json!(true);
    report.record(CaseResult::new(
        "deepseek-prunes-and-remeasures-before-compacting",
        pruned && avoided && pairs_intact,
        format!(
            "prunedChars={} compactionAvoided={} localCompactions={} toolPairsIntact={}",
            context_turn["prunedChars"],
            context_turn["compactionAvoided"],
            context_turn["localCompactions"],
            context_turn["toolPairsIntact"]
        ),
    ));

    // 8. Overflow recovery: one retry when the surface really shrank, and the
    //    original provider error when it did not.
    let overflow_thread = client.start_thread(DEEPSEEK_OVERFLOW_MODEL)?;
    let overflow_turn = client.turn(&overflow_thread, "overflow once")?;
    let stuck_thread = client.start_thread(DEEPSEEK_STUCK_MODEL)?;
    let stuck_turn = client.turn(&stuck_thread, "overflow forever")?;
    let recovered = overflow_turn["status"] == "completed"
        && overflow_turn["overflowRetries"].as_u64().unwrap_or(0) == 1;
    let preserved = stuck_turn["status"] == "failed"
        && stuck_turn["overflowRetries"].as_u64().unwrap_or(0) == 1
        && stuck_turn["error"]
            .as_str()
            .is_some_and(|message| message.contains("context window"));
    for turn in [&overflow_turn, &stuck_turn] {
        if turn["overflowRetries"].as_u64().unwrap_or(0)
            > vellum_enhanced_codex::MAX_CONTEXT_OVERFLOW_RETRIES as u64
        {
            report.hard_gates.unbounded_overflow_retry += 1;
        }
    }
    report.record(CaseResult::new(
        "overflow-retries-once-then-preserves-the-provider-error",
        recovered && preserved,
        format!(
            "recovered={} retries={} stuckStatus={} stuckError={}",
            overflow_turn["status"],
            overflow_turn["overflowRetries"],
            stuck_turn["status"],
            stuck_turn["error"]
        ),
    ));

    // 9. Bounded continuation, then cancel, then user steer.
    let continuation_thread = client.start_thread(QWEN_CONTINUATION_MODEL)?;
    let continuation_turn = client.turn(&continuation_thread, "keep going")?;
    let continuations = continuation_turn["continuations"].as_u64().unwrap_or(0);
    if continuations > vellum_enhanced_codex::MAX_AUTO_CONTINUATIONS as u64 {
        report.hard_gates.automatic_continuation_over_max += 1;
    }
    let bounded = continuations == vellum_enhanced_codex::MAX_AUTO_CONTINUATIONS as u64
        && continuation_turn["continuationBudgetExhausted"] == json!(true);

    let cancel_thread = client.start_thread(QWEN_CANCEL_MODEL)?;
    let cancel_turn_id = client.begin_turn(&cancel_thread, "start slow work")?;
    std::thread::sleep(Duration::from_millis(250));
    client.request("turn/interrupt", json!({"threadId": cancel_thread}))?;
    let cancelled = client.await_turn(cancel_turn_id)?;

    let steer_thread = client.start_thread(QWEN_STEER_MODEL)?;
    let steer_turn_id = client.begin_turn(&steer_thread, "start slow work")?;
    std::thread::sleep(Duration::from_millis(250));
    client.request("turn/steer", json!({"threadId": steer_thread}))?;
    let steered = client.await_turn(steer_turn_id)?;

    report.record(CaseResult::new(
        "continuation-is-bounded-and-stops-on-cancel-or-steer",
        bounded && cancelled["status"] == "cancelled" && steered["status"] == "steered",
        format!(
            "continuations={continuations} cancel={} steer={}",
            cancelled["status"], steered["status"]
        ),
    ));

    // 10. The third party never asks the gateway for remote compaction.
    let observations = provider.observations();
    report.record(CaseResult::new(
        "third-party-turns-carry-no-remote-compaction-trigger",
        observations.remote_compaction_requests == 0,
        format!(
            "remoteCompactionRequests={}",
            observations.remote_compaction_requests
        ),
    ));

    // 11. Enhanced notifications are absorbed by the bridge.
    let leaked = client.notifications_matching("vellum/enhanced");
    report.hard_gates.enhanced_notification_leaked_to_desktop = leaked as u64;
    report.record(CaseResult::new(
        "enhanced-notifications-never-reach-the-client",
        leaked == 0,
        format!("leakedNotifications={leaked}"),
    ));

    // 12. Killing the Enhanced child fails third-party turns closed and leaves
    //     Official working.
    let attestation = BridgeAttestationV1::read(&manifest.attestation_path)
        .map_err(|error| AppError::Message(error.to_string()))?;
    report.attestation_state = Some(attestation.state.as_str().to_string());
    report.bridge_pid = Some(attestation.bridge_pid);
    report.official_child_pid = attestation.official.pid;
    report.enhanced_child_pid = attestation.enhanced.pid;
    report.enhanced_identity = attestation.enhanced_identity.clone();
    report.session_features_applied = attestation.session_features_applied;
    let attested = attestation.is_active_for(&manifest.launch_id);
    report.record(CaseResult::new(
        "bridge-attests-both-children-initialized",
        attested,
        format!(
            "state={} official={:?} enhanced={:?}",
            attestation.state.as_str(),
            attestation.official.pid,
            attestation.enhanced.pid
        ),
    ));

    if let Some(pid) = attestation.enhanced.pid {
        kill_pid(pid);
        wait_for_exit(pid);
        let refused = client.request("thread/start", json!({"model": QWEN_MODEL}))?;
        let official_still_works =
            client.request("thread/start", json!({"model": OFFICIAL_MODEL}))?;
        let fails_closed = refused.get("error").is_some();
        let official_alive = official_still_works.get("error").is_none();
        if !fails_closed {
            report.hard_gates.enhanced_fallback_to_official += 1;
        }
        report.record(CaseResult::new(
            "a-dead-enhanced-child-fails-closed-without-touching-official",
            fails_closed && official_alive,
            format!(
                "thirdPartyRefused={fails_closed} officialStillServing={official_alive} {}",
                error_message(&refused)
            ),
        ));
    }

    client.shutdown();

    // 13. Cross-process resume: a brand new bridge process, the same durable
    //     bindings, and the same execution plane.
    let resumed_manifest = write_manifest(run_root, official, enhanced, "launch-bridge-2")?;
    let mut resumed = BridgeClient::start(
        bridge,
        &LaunchManifestV1::path_in(run_root),
        catalog_path,
        provider_url,
        workspace,
    )?;
    resumed.initialize()?;
    let resume_response = resumed.request("thread/resume", json!({"threadId": deepseek_thread}))?;
    let resumed_binding = read_binding(&binding_db, &deepseek_thread)?;
    let resume_ok = resume_response.get("error").is_none()
        && resume_response
            .pointer("/result/thread/id")
            .and_then(Value::as_str)
            == Some(deepseek_thread.as_str())
        && resumed_binding.plane == ExecutionPlane::EnhancedCodex
        && resumed_binding.provider_id == DEEPSEEK_PROVIDER
        && resumed_binding.runtime_digest == resumed_manifest.enhanced.runtime_digest;
    report.record(CaseResult::new(
        "resume-across-a-bridge-restart-keeps-thread-digest-and-plane",
        resume_ok,
        format!(
            "thread={} plane={} digest={}",
            resume_response
                .pointer("/result/thread/id")
                .and_then(Value::as_str)
                .unwrap_or("<none>"),
            resumed_binding.plane.as_str(),
            resumed_binding.runtime_digest
        ),
    ));
    resumed.shutdown();

    // 14. Fail-closed configuration: a missing, tampered, or protocol-mismatched
    //     Enhanced binary must stop the bridge rather than degrade to Official.
    report.record(fail_closed_configuration_case(run_root, official, enhanced));

    // Evidence collected from the durable artifacts, not from memory.
    report.bindings = collect_bindings(
        &binding_db,
        [
            official_thread.as_str(),
            qwen_thread.as_str(),
            deepseek_thread.as_str(),
            overflow_thread.as_str(),
            continuation_thread.as_str(),
        ],
    );
    report.enhanced_event_counts = journal_counts(&manifest.qualification_journal_path);
    let expected_events = [
        "enhanced.tool.duplicate_detected",
        "enhanced.tool.duplicate_suppressed",
        "enhanced.context.prune_completed",
        "enhanced.context.compaction_avoided",
        "enhanced.context.overflow_retry",
        "enhanced.context.overflow_retry_refused",
        "enhanced.continuation.allowed",
        "enhanced.continuation.exhausted",
    ];
    let missing = expected_events
        .iter()
        .filter(|name| !report.enhanced_event_counts.contains_key(**name))
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();
    report.record(CaseResult::new(
        "every-port-produced-its-deterministic-events",
        missing.is_empty(),
        if missing.is_empty() {
            format!(
                "{} event kinds recorded",
                report.enhanced_event_counts.len()
            )
        } else {
            format!("missing {}", missing.join(", "))
        },
    ));

    Ok(())
}

fn fail_closed_configuration_case(run_root: &Path, official: &Path, enhanced: &Path) -> CaseResult {
    let mut findings = Vec::new();
    let mut passed = true;

    let mut missing = match build_manifest(run_root, official, enhanced, "launch-missing") {
        Ok(manifest) => manifest,
        Err(error) => {
            return CaseResult::new("fail-closed-configuration", false, error.to_string())
        }
    };
    missing.enhanced.executable = run_root.join("no-such-enhanced-binary");
    let path = run_root.join("manifest-missing.json");
    // `write` validates identity, not existence; loading is where it must fail.
    let _ = std::fs::write(
        &path,
        serde_json::to_vec_pretty(&missing).unwrap_or_default(),
    );
    let missing_result = crate::enhanced_runtime::app_server_bridge::BridgeConfig::load(
        &path,
        vec!["app-server".into()],
    );
    passed &= missing_result.is_err();
    findings.push(format!("missingEnhanced={}", missing_result.is_err()));

    let tampered_path = run_root.join("tampered-enhanced.bin");
    let mut tampered = match build_manifest(run_root, official, enhanced, "launch-tampered") {
        Ok(manifest) => manifest,
        Err(error) => {
            return CaseResult::new("fail-closed-configuration", false, error.to_string())
        }
    };
    let _ = std::fs::copy(enhanced, &tampered_path);
    let _ = std::fs::OpenOptions::new()
        .append(true)
        .open(&tampered_path)
        .and_then(|mut file| file.write_all(b"tamper"));
    tampered.enhanced.executable = tampered_path;
    let path = run_root.join("manifest-tampered.json");
    let _ = std::fs::write(
        &path,
        serde_json::to_vec_pretty(&tampered).unwrap_or_default(),
    );
    let tampered_result = crate::enhanced_runtime::app_server_bridge::BridgeConfig::load(
        &path,
        vec!["app-server".into()],
    );
    passed &= tampered_result.is_err();
    findings.push(format!("hashMismatch={}", tampered_result.is_err()));

    CaseResult::new("fail-closed-configuration", passed, findings.join(" "))
}

fn build_manifest(
    run_root: &Path,
    official: &Path,
    enhanced: &Path,
    launch_id: &str,
) -> AppResult<LaunchManifestV1> {
    let lock =
        EnhancedRuntimeLockFile::parse(include_bytes!("../../../../enhanced-runtime.lock.json"))
            .map_err(|error| AppError::Message(error.to_string()))?;
    let model_map_path = run_root.join("model-provider-map.json");
    gate_model_provider_map()
        .write(&model_map_path)
        .map_err(|error| AppError::Message(error.to_string()))?;
    Ok(LaunchManifestV1 {
        schema_version: LAUNCH_MANIFEST_SCHEMA_VERSION,
        launch_id: launch_id.to_string(),
        created_at: chrono::Utc::now().timestamp(),
        official: RuntimeBinaryIdentity {
            artifact_sha256: sha256_file(official).map_err(to_app_error)?,
            executable: official.to_path_buf(),
            runtime_digest: format!("sha256:gate-official-{}", short(official)),
            codex_home: run_root.join("codex-home-official"),
        },
        enhanced: RuntimeBinaryIdentity {
            artifact_sha256: sha256_file(enhanced).map_err(to_app_error)?,
            executable: enhanced.to_path_buf(),
            runtime_digest: format!("sha256:gate-enhanced-{}", short(enhanced)),
            codex_home: run_root.join("codex-home-enhanced"),
        },
        model_provider_map_sha256: sha256_file(&model_map_path).map_err(to_app_error)?,
        model_provider_map_path: model_map_path,
        binding_db: run_root.join("thread-bindings.sqlite3"),
        official_provider_ids: vec![OFFICIAL_PROVIDER.into()],
        third_party_provider_ids: vec![
            QWEN_PROVIDER.into(),
            DEEPSEEK_PROVIDER.into(),
            GROK_PROVIDER.into(),
        ],
        feature_profile: FEATURE_DEFAULTS_MVP.into(),
        enhanced_commit: lock.enhanced_codex_commit.clone().unwrap_or_default(),
        attestation_path: run_root.join("bridge-attestation.json"),
        qualification_journal_path: run_root.join("qualification-journal.jsonl"),
        relay: None,
    })
}

pub(super) fn write_manifest(
    run_root: &Path,
    official: &Path,
    enhanced: &Path,
    launch_id: &str,
) -> AppResult<LaunchManifestV1> {
    let manifest = build_manifest(run_root, official, enhanced, launch_id)?;
    manifest
        .write(&LaunchManifestV1::path_in(run_root))
        .map_err(to_app_error)?;
    Ok(manifest)
}

fn stage_real_codex_homes(manifest: &LaunchManifestV1, provider_url: &str) -> AppResult<()> {
    let catalog_path = manifest
        .model_provider_map_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("native-model-catalog.json");
    let route = Route {
        id: CHILD_THIRD_PARTY_PROVIDER.into(),
        name: "Vellum Gate".into(),
        base_url: provider_url.into(),
        model: OFFICIAL_MODEL.into(),
        wire: WireFormat::Responses,
        is_current: true,
        server_side_resume: false,
        streaming: true,
        reasoning: false,
        provider_kind: ProviderKind::OpenAiCompatible,
        auth_kind: AuthKind::None,
        enabled: true,
        models: default_catalog().models.keys().cloned().collect(),
        selected_models: None,
        context_window: Some(128_000),
        model_capabilities: Vec::new(),
        insecure_http_policy: Default::default(),
        catalog_scope: CatalogScope::All,
    };
    let models = default_catalog()
        .models
        .iter()
        .map(|(catalog_id, model)| ModelRoute {
            catalog_id: catalog_id.clone(),
            display_name: catalog_id.clone(),
            route_id: CHILD_THIRD_PARTY_PROVIDER.into(),
            upstream_model: catalog_id.clone(),
            context_window: Some(model.context_window),
            wire: WireFormat::Responses,
            reasoning: false,
            streaming: true,
            vision: false,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            reasoning_effort_transport: ReasoningEffortTransport::None,
        })
        .collect::<Vec<_>>();
    crate::catalog::write_catalog_with_model_routes(&catalog_path, &[route], &models, None)?;

    let catalog_json = serde_json::to_string(&catalog_path)
        .map_err(|error| AppError::Message(format!("encode gate catalog path: {error}")))?;
    let base_url_json = serde_json::to_string(provider_url)
        .map_err(|error| AppError::Message(format!("encode gate provider url: {error}")))?;
    let config = format!(
        concat!(
            "model_provider = \"{provider}\"\n",
            "model_catalog_json = {catalog_json}\n",
            "\n",
            "[model_providers.{provider}]\n",
            "name = \"Vellum Gate\"\n",
            "base_url = {base_url_json}\n",
            "wire_api = \"responses\"\n",
            "requires_openai_auth = false\n",
            "supports_websockets = false\n"
        ),
        provider = CHILD_THIRD_PARTY_PROVIDER,
        catalog_json = catalog_json,
        base_url_json = base_url_json
    );
    for home in [&manifest.official.codex_home, &manifest.enhanced.codex_home] {
        std::fs::create_dir_all(home)
            .map_err(|error| AppError::Message(format!("create CODEX_HOME: {error}")))?;
        std::fs::write(home.join("config.toml"), &config)
            .map_err(|error| AppError::Message(format!("write gate CODEX_HOME config: {error}")))?;
    }
    Ok(())
}

fn drive_real_binary_smoke(
    mut client: BridgeClient,
    manifest: &LaunchManifestV1,
    binding_db: &Path,
    report: &mut GateReport,
) -> AppResult<()> {
    let official_thread = client.start_thread(OFFICIAL_MODEL)?;
    let official_binding = read_binding(binding_db, &official_thread)?;
    let official_ok = official_binding.plane == ExecutionPlane::OfficialCodex
        && official_binding.provider_id == OFFICIAL_PROVIDER;
    if !official_ok {
        report.hard_gates.official_runtime_mutated += 1;
    }
    report.record(CaseResult::new(
        "real-official-model-binds-official-plane",
        official_ok,
        format!(
            "plane={} provider={}",
            official_binding.plane.as_str(),
            official_binding.provider_id
        ),
    ));

    let mut routed_threads = Vec::new();
    let mut routing_ok = true;
    let mut routing_detail = Vec::new();
    for (model, provider_id) in [
        (REAL_QWEN_SMOKE_MODEL, QWEN_PROVIDER),
        (REAL_DEEPSEEK_SMOKE_MODEL, DEEPSEEK_PROVIDER),
        (REAL_GROK_SMOKE_MODEL, GROK_PROVIDER),
    ] {
        let thread = client.start_thread(model)?;
        let binding = read_binding(binding_db, &thread)?;
        let correct = binding.plane == ExecutionPlane::EnhancedCodex
            && binding.provider_id == provider_id
            && binding.runtime_digest == manifest.enhanced.runtime_digest;
        routing_ok &= correct;
        routing_detail.push(format!(
            "{model}->{}:{}",
            binding.plane.as_str(),
            binding.provider_id
        ));
        routed_threads.push((model, thread));
    }
    report.record(CaseResult::new(
        "real-third-party-models-bind-enhanced-plane",
        routing_ok,
        routing_detail.join(" "),
    ));

    let mut turn_ok = true;
    let mut turn_detail = Vec::new();
    for (model, thread) in &routed_threads {
        let completed = client.native_turn(thread, "Reply with exactly PONG.")?;
        let status = completed
            .pointer("/params/turn/status")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        turn_ok &= status == "completed";
        turn_detail.push(format!("{model}={status}"));
    }
    report.record(CaseResult::new(
        "real-third-party-turns-complete-through-enhanced-runtime",
        turn_ok,
        turn_detail.join(" "),
    ));

    let attestation = BridgeAttestationV1::read(&manifest.attestation_path)
        .map_err(|error| AppError::Message(error.to_string()))?;
    report.attestation_state = Some(attestation.state.as_str().to_string());
    report.bridge_pid = Some(attestation.bridge_pid);
    report.official_child_pid = attestation.official.pid;
    report.enhanced_child_pid = attestation.enhanced.pid;
    report.enhanced_identity = attestation.enhanced_identity.clone();
    report.session_features_applied = attestation.session_features_applied;
    let attested = attestation.is_active_for(&manifest.launch_id);
    report.record(CaseResult::new(
        "real-bridge-attests-both-children-initialized",
        attested,
        format!(
            "state={} official={:?} enhanced={:?}",
            attestation.state.as_str(),
            attestation.official.pid,
            attestation.enhanced.pid
        ),
    ));

    report.bindings = collect_bindings(
        binding_db,
        std::iter::once(official_thread.as_str())
            .chain(routed_threads.iter().map(|(_, thread)| thread.as_str())),
    );
    client.shutdown();
    Ok(())
}

/// The trusted map the bridge routes by. Official catalog ids resolve to the
/// Official provider table; every third-party id shares the Vellum gateway.
fn gate_model_provider_map() -> TrustedModelProviderMap {
    let mut models = BTreeMap::new();
    models.insert(
        OFFICIAL_MODEL.to_string(),
        ModelProviderRoute {
            provider_id: OFFICIAL_PROVIDER.into(),
            // The gate's local provider serves the Official control turn too.
            // Plane selection comes from this trusted map's provider_id, not
            // from the child provider name.
            child_provider_id: CHILD_THIRD_PARTY_PROVIDER.into(),
        },
    );
    for model in [
        QWEN_MODEL,
        QWEN_CONTINUATION_MODEL,
        QWEN_CANCEL_MODEL,
        QWEN_STEER_MODEL,
        NAME_TRAP_MODEL,
    ] {
        models.insert(
            model.to_string(),
            ModelProviderRoute {
                provider_id: QWEN_PROVIDER.into(),
                child_provider_id: CHILD_THIRD_PARTY_PROVIDER.into(),
            },
        );
    }
    for model in [
        DEEPSEEK_MODEL,
        DEEPSEEK_OVERFLOW_MODEL,
        DEEPSEEK_STUCK_MODEL,
    ] {
        models.insert(
            model.to_string(),
            ModelProviderRoute {
                provider_id: DEEPSEEK_PROVIDER.into(),
                child_provider_id: CHILD_THIRD_PARTY_PROVIDER.into(),
            },
        );
    }
    models.insert(
        GROK_MODEL.to_string(),
        ModelProviderRoute {
            provider_id: GROK_PROVIDER.into(),
            child_provider_id: CHILD_THIRD_PARTY_PROVIDER.into(),
        },
    );
    for (model, provider_id) in [
        (REAL_QWEN_SMOKE_MODEL, QWEN_PROVIDER),
        (REAL_DEEPSEEK_SMOKE_MODEL, DEEPSEEK_PROVIDER),
        (REAL_GROK_SMOKE_MODEL, GROK_PROVIDER),
    ] {
        models.insert(
            model.to_string(),
            ModelProviderRoute {
                provider_id: provider_id.into(),
                child_provider_id: CHILD_THIRD_PARTY_PROVIDER.into(),
            },
        );
    }
    TrustedModelProviderMap {
        schema_version: MODEL_PROVIDER_MAP_SCHEMA_VERSION,
        models,
    }
}

pub(super) struct BridgeClient {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    buffered: Vec<Value>,
    next_id: i64,
}

impl BridgeClient {
    pub(super) fn start(
        bridge: &Path,
        manifest_path: &Path,
        catalog: &Path,
        provider_url: &str,
        workspace: &Path,
    ) -> AppResult<Self> {
        std::fs::create_dir_all(workspace)
            .map_err(|error| AppError::Message(format!("create gate workspace: {error}")))?;
        let mut child = Command::new(bridge)
            .arg("app-server")
            .env(LAUNCH_MANIFEST_ENV, manifest_path)
            .env(GATE_CATALOG_ENV, catalog)
            .env(GATE_PROVIDER_URL_ENV, provider_url)
            .env(GATE_WORKSPACE_ENV, workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                AppError::Message(format!("start bridge {}: {error}", bridge.display()))
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::Message("bridge did not expose stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::Message("bridge did not expose stdout".into()))?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    if tx.send(value).is_err() {
                        return;
                    }
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            rx,
            buffered: Vec::new(),
            next_id: 1,
        })
    }

    fn send(&mut self, value: &Value) -> AppResult<()> {
        serde_json::to_writer(&mut self.stdin, value)
            .map_err(|error| AppError::Message(format!("write to bridge: {error}")))?;
        self.stdin
            .write_all(b"\n")
            .and_then(|()| self.stdin.flush())
            .map_err(|error| AppError::Message(format!("flush bridge stdin: {error}")))
    }

    pub(super) fn initialize(&mut self) -> AppResult<()> {
        let response = self.request(
            "initialize",
            json!({"clientInfo": {
                "name": "vellum-gate",
                "title": "Vellum Enhanced Gate",
                "version": env!("CARGO_PKG_VERSION")
            }}),
        )?;
        if response.get("error").is_some() {
            return Err(AppError::Message(format!(
                "bridge initialize failed: {}",
                error_message(&response)
            )));
        }
        self.send(&json!({"method": "initialized"}))?;
        Ok(())
    }

    pub(super) fn request(&mut self, method: &str, params: Value) -> AppResult<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"id": id, "method": method, "params": params}))?;
        self.wait_for(id)
    }

    fn wait_for(&mut self, id: i64) -> AppResult<Value> {
        if let Some(index) = self
            .buffered
            .iter()
            .position(|value| value.get("id").and_then(Value::as_i64) == Some(id))
        {
            return Ok(self.buffered.remove(index));
        }
        loop {
            match self.rx.recv_timeout(REQUEST_TIMEOUT) {
                Ok(value) => {
                    if value.get("id").and_then(Value::as_i64) == Some(id) {
                        return Ok(value);
                    }
                    self.buffered.push(value);
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(AppError::Message(format!(
                        "bridge did not answer request {id} within {}s",
                        REQUEST_TIMEOUT.as_secs()
                    )))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(AppError::Message("bridge closed its stdout".into()))
                }
            }
        }
    }

    pub(super) fn drain(&mut self) {
        while let Ok(value) = self.rx.try_recv() {
            self.buffered.push(value);
        }
    }

    pub(super) fn notifications_matching(&mut self, prefix: &str) -> usize {
        self.drain();
        self.buffered
            .iter()
            .filter(|value| {
                value
                    .get("method")
                    .and_then(Value::as_str)
                    .is_some_and(|method| method.starts_with(prefix))
            })
            .count()
    }

    fn wait_for_notification(&mut self, method: &str, thread_id: Option<&str>) -> AppResult<Value> {
        if let Some(index) = self.buffered.iter().position(|value| {
            value.get("method").and_then(Value::as_str) == Some(method)
                && thread_id.is_none_or(|expected| {
                    value.pointer("/params/threadId").and_then(Value::as_str) == Some(expected)
                })
        }) {
            return Ok(self.buffered.remove(index));
        }
        loop {
            match self.rx.recv_timeout(REQUEST_TIMEOUT) {
                Ok(value) => {
                    if value.get("method").and_then(Value::as_str) == Some(method)
                        && thread_id.is_none_or(|expected| {
                            value.pointer("/params/threadId").and_then(Value::as_str)
                                == Some(expected)
                        })
                    {
                        return Ok(value);
                    }
                    self.buffered.push(value);
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(AppError::Message(format!(
                        "bridge did not emit {method} within {}s",
                        REQUEST_TIMEOUT.as_secs()
                    )))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(AppError::Message("bridge closed its stdout".into()))
                }
            }
        }
    }

    pub(super) fn start_thread(&mut self, model: &str) -> AppResult<String> {
        let response = self.request(
            "thread/start",
            json!({"modelProvider": CHILD_THIRD_PARTY_PROVIDER, "model": model}),
        )?;
        response
            .pointer("/result/thread/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                AppError::Message(format!(
                    "thread/start for {model} did not return a thread: {}",
                    error_message(&response)
                ))
            })
    }

    pub(super) fn begin_turn(&mut self, thread_id: &str, input: &str) -> AppResult<i64> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({
            "id": id,
            "method": "turn/create",
            "params": {"threadId": thread_id, "input": input}
        }))?;
        Ok(id)
    }

    pub(super) fn await_turn(&mut self, id: i64) -> AppResult<Value> {
        let response = self.wait_for(id)?;
        Ok(response
            .pointer("/result/turn")
            .cloned()
            .unwrap_or_else(|| json!({"status": "failed", "error": error_message(&response)})))
    }

    pub(super) fn turn(&mut self, thread_id: &str, input: &str) -> AppResult<Value> {
        let id = self.begin_turn(thread_id, input)?;
        self.await_turn(id)
    }

    pub(super) fn native_turn(&mut self, thread_id: &str, input: &str) -> AppResult<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({
            "id": id,
            "method": "turn/start",
            "params": {
                "threadId": thread_id,
                "clientUserMessageId": format!("gate-msg-{id}"),
                "input": [{
                    "type": "text",
                    "text": input,
                    "textElements": []
                }]
            }
        }))?;
        let response = self.wait_for(id)?;
        if response.get("error").is_some() {
            return Err(AppError::Message(format!(
                "turn/start for {thread_id} failed: {}",
                error_message(&response)
            )));
        }
        self.wait_for_notification("turn/completed", Some(thread_id))
    }

    pub(super) fn shutdown(mut self) {
        drop(self.stdin);
        for _ in 0..50 {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        kill_pid(self.child.id());
        let _ = self.child.wait();
    }
}

pub(super) fn read_binding(
    binding_db: &Path,
    thread_id: &str,
) -> AppResult<crate::enhanced_runtime::ThreadRuntimeBinding> {
    let store = ThreadRuntimeBindingStore::open(binding_db)
        .map_err(|error| AppError::Message(error.to_string()))?;
    store
        .get(thread_id)
        .map_err(|error| AppError::Message(error.to_string()))?
        .ok_or_else(|| AppError::Message(format!("thread {thread_id} has no durable binding")))
}

pub(super) fn collect_bindings<'a>(
    binding_db: &Path,
    thread_ids: impl IntoIterator<Item = &'a str>,
) -> Vec<ObservedBinding> {
    let Ok(store) = ThreadRuntimeBindingStore::open(binding_db) else {
        return Vec::new();
    };
    thread_ids
        .into_iter()
        .filter_map(|thread_id| store.get(thread_id).ok().flatten())
        .map(|binding| ObservedBinding::from_binding(&binding))
        .collect()
}

pub(super) fn journal_counts(path: &Path) -> BTreeMap<String, u64> {
    let mut counts = BTreeMap::new();
    for entry in QualificationJournal::read(path) {
        if let JournalEntry::Event { event, .. } = entry {
            *counts.entry(event.name).or_insert(0) += 1;
        }
    }
    counts
}

pub(super) fn error_message(value: &Value) -> String {
    value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub(super) fn locate(args: &[String], flag: &str, stem: &str) -> AppResult<PathBuf> {
    if let Some(path) = value_after(args, flag) {
        return Ok(PathBuf::from(path));
    }
    let current = std::env::current_exe().map_err(|error| {
        AppError::Message(format!("cannot locate the current executable: {error}"))
    })?;
    let name = format!("{stem}{}", std::env::consts::EXE_SUFFIX);
    let mut directory = current.parent().map(Path::to_path_buf);
    for _ in 0..3 {
        let Some(candidate_dir) = directory else {
            break;
        };
        let candidate = candidate_dir.join(&name);
        if candidate.is_file() {
            return Ok(candidate);
        }
        directory = candidate_dir.parent().map(Path::to_path_buf);
    }
    Err(AppError::Message(format!(
        "cannot locate {name}; build it or pass {flag}"
    )))
}

fn short(path: &Path) -> String {
    sha256_file(path)
        .map(|hash| {
            hash.trim_start_matches("sha256:")
                .chars()
                .take(16)
                .collect()
        })
        .unwrap_or_else(|_| "unknown".into())
}

fn to_app_error(error: impl std::fmt::Display) -> AppError {
    AppError::Message(error.to_string())
}

fn kill_pid(pid: u32) {
    #[cfg(target_os = "windows")]
    {
        let _ = crate::process::background_command("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = crate::process::background_command("kill")
            .args(["-KILL", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn wait_for_exit(pid: u32) {
    for _ in 0..50 {
        if !crate::enhanced_runtime::pid_is_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default run: green is a component result and says so.
    #[test]
    fn the_gate_children_are_never_promotion_material() {
        let child = Path::new("C:/vellum/target/debug/vellum-codex-gate-child.exe");
        let real = Path::new("C:/codex/codex.exe");
        assert_eq!(
            execution_profile(child, child, None),
            ("surrogate-children".into(), false)
        );
        // One real binary alongside a surrogate is still a surrogate run.
        assert_eq!(
            execution_profile(real, child, Some(real)),
            ("surrogate-children".into(), false)
        );
    }

    /// Two flags aimed at one file used to be enough to print GO. It cannot be:
    /// `a-dead-enhanced-child-fails-closed-without-touching-official` needs two
    /// processes to have anything to say.
    #[test]
    fn one_binary_behind_both_planes_proves_no_isolation() {
        let both = Path::new("C:/codex/codex.exe");
        let (profile, eligible) = execution_profile(both, both, Some(both));
        assert!(!eligible);
        assert_eq!(profile, "one-binary-serving-both-planes");
    }

    /// A path is not provenance. The enhanced binary under test has to be the
    /// artifact this machine is pinned to, or the run certifies a build the
    /// user will never launch.
    #[test]
    fn an_enhanced_binary_that_is_not_the_pinned_artifact_is_refused() {
        let official = Path::new("C:/codex/codex.exe");
        let stale = Path::new("C:/codex/yesterday/codex-enhanced.exe");
        let pinned = Path::new("C:/codex-enhanced/codex.exe");
        let (profile, eligible) = execution_profile(official, stale, Some(pinned));
        assert!(!eligible);
        assert_eq!(profile, "enhanced-binary-is-not-the-pinned-artifact");
    }

    /// CI has no Enhanced configuration to compare against, so it fails closed
    /// rather than taking the flags at their word.
    #[test]
    fn nothing_pinned_on_this_machine_is_not_promotion_eligible() {
        let (profile, eligible) = execution_profile(
            Path::new("C:/codex/codex.exe"),
            Path::new("C:/codex-enhanced/codex.exe"),
            None,
        );
        assert!(!eligible);
        assert_eq!(profile, "enhanced-binary-is-not-pinned-on-this-machine");
    }

    #[test]
    fn two_distinct_binaries_with_the_enhanced_one_pinned_may_be_promoted() {
        let pinned = Path::new("C:/codex-enhanced/codex.exe");
        assert_eq!(
            execution_profile(Path::new("C:/codex/codex.exe"), pinned, Some(pinned)),
            ("explicit-runtime-binaries".into(), true)
        );
    }

    #[test]
    fn the_trusted_map_routes_a_gpt_shaped_catalog_id_to_its_real_provider() {
        let map = gate_model_provider_map();
        assert_eq!(
            map.resolve(NAME_TRAP_MODEL).unwrap().provider_id,
            QWEN_PROVIDER
        );
        assert_eq!(
            map.resolve(OFFICIAL_MODEL).unwrap().provider_id,
            OFFICIAL_PROVIDER
        );
        assert!(map.resolve("not-in-the-map").is_none());
    }

    #[test]
    fn every_third_party_catalog_id_shares_the_vellum_gateway() {
        let map = gate_model_provider_map();
        for model in [QWEN_MODEL, DEEPSEEK_MODEL, GROK_MODEL, NAME_TRAP_MODEL] {
            assert_eq!(
                map.resolve(model).unwrap().child_provider_id,
                CHILD_THIRD_PARTY_PROVIDER
            );
        }
        assert_eq!(
            map.resolve(OFFICIAL_MODEL).unwrap().child_provider_id,
            CHILD_THIRD_PARTY_PROVIDER
        );
    }
}
